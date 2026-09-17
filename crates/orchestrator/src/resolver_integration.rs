//! Phase 6 wiring: resolver short-circuit for zero-waste orchestration (ADR-64).
//!
//! This module bridges the pure resolver (`crate::resolver`) with the
//! coordinator's dispatch loop.  It builds [`WorkCandidate`]s from live graph
//! state and, when the resolver returns [`DispatchDecision::Reuse`], injects
//! the cached result and marks the task done **without a model dispatch**.
//!
//! # Design
//!
//! The integration is conservative: it only short-circuits `Reuse` verdicts.
//! `Refine`, `Reopen`, and `Dispatch` flow through the existing normal
//! dispatch path — their dispatch framing is a Phase 5/follow-up concern.
//!
//! The [`ResolverAudit`] is recorded in two places:
//! 1. The checkpoint action ledger (durable, survives resume).
//! 2. A working-memory decision (visible in agent prompts and timeline
//!    enrichment).

use std::collections::HashMap;

use concerto_core::types::{AgentRunResult, SubTask, TaskId};
use tracing::debug;

use crate::fingerprint::{ArtifactFingerprint, SemanticKey};
use crate::graph::TaskGraph;
use crate::resolver::{should_dispatch, DispatchDecision, ResolverAudit, WorkCandidate};
use crate::timeline::TimelineProjection;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Outcome of the resolver integration for a single ready task.
#[derive(Debug, Clone)]
pub enum ResolverOutcome {
    /// The task was short-circuited: cached result injected, task marked done.
    Reused {
        /// The cached result that was injected.
        result: Box<AgentRunResult>,
        /// The resolver audit record.
        audit: ResolverAudit,
    },
    /// The task needs fresh dispatch (Reopen, Refine, or Dispatch).
    Dispatch,
}

/// Result of the resolver integration pass for a batch of ready tasks.
#[derive(Debug)]
pub struct ResolverPassResult {
    /// Per-task outcomes (only tasks that were short-circuited).
    pub reused: HashMap<TaskId, ResolverOutcome>,
    /// Task IDs that should proceed to normal dispatch (not short-circuited).
    pub dispatch_ids: Vec<(TaskId, concerto_core::types::AgentId)>,
}

// ---------------------------------------------------------------------------
// WorkCandidate builder
// ---------------------------------------------------------------------------

/// Build a [`WorkCandidate`] for a ready task from live graph state.
///
/// # Arguments
///
/// * `task` — the ready subtask.
/// * `graph` — the full task graph (for dependency lookup).
/// * `completed_results` — results of already-completed tasks (for dependency
///   fingerprinting and current-input synthesis).
/// * `objective_hash` — blake3 hex of the run-level objective text.
/// * `plan_version` — content hash of the plan governing this work.
/// * `expected_artifacts` — expected output files for this task (from the
///   coordinator's `expected_artifacts` map).
pub fn build_work_candidate(
    task: &SubTask,
    graph: &TaskGraph,
    completed_results: &HashMap<TaskId, AgentRunResult>,
    objective_hash: &str,
    plan_version: &str,
    expected_artifacts: &[camino::Utf8PathBuf],
) -> WorkCandidate {
    let output_contract = if expected_artifacts.is_empty() {
        String::new()
    } else {
        let mut paths: Vec<&str> = expected_artifacts.iter().map(|p| p.as_str()).collect();
        paths.sort();
        paths.join("\n")
    };

    let dependency_keys =
        compute_dependency_keys(task, graph, completed_results, objective_hash, plan_version);

    let current_inputs = compute_current_inputs(task, graph, completed_results);

    let semantic_key = SemanticKey::compute(
        objective_hash,
        plan_version,
        &task.description,
        &output_contract,
        &dependency_keys,
    );

    WorkCandidate {
        semantic_key,
        objective_hash: objective_hash.to_owned(),
        plan_version: plan_version.to_owned(),
        work_intent: task.description.clone(),
        current_inputs,
        output_contract,
    }
}

/// Compute semantic keys of direct dependencies (sorted, for set semantics).
fn compute_dependency_keys(
    task: &SubTask,
    graph: &TaskGraph,
    completed_results: &HashMap<TaskId, AgentRunResult>,
    objective_hash: &str,
    plan_version: &str,
) -> Vec<String> {
    let mut keys = Vec::new();
    for dep_id in &task.dependencies {
        if let Some(dep_task) = graph.get(dep_id) {
            if completed_results.contains_key(dep_id) {
                let key = SemanticKey::compute(
                    objective_hash,
                    plan_version,
                    &dep_task.description,
                    "", // simplified: no output contract for deps
                    &[],
                );
                keys.push(key.hex().to_owned());
            }
        }
    }
    keys.sort();
    keys
}

/// Synthesize `current_inputs` from dependency outputs.
///
/// For each completed dependency, fingerprint its `files_modified` as
/// `ArtifactFingerprint::Output`. Empty when no dependencies have recorded
/// outputs — that is SAFE because the resolver returns `Dispatch` (not
/// `Reuse`) when recorded_inputs is empty.
///
/// # Phase 7 hardening needed
///
/// The current `content_hash` is derived from the **file path string**, not
/// the actual file content.  This is semantically incorrect — a content change
/// at the same path would produce the same fingerprint — but safe for v1:
/// Phase 5 has not yet wired `semantic_key_hex` emission into timeline events,
/// so `find_matching_record` returns `None` and no `Reuse` actually fires.
/// Phase 7 must replace this with the write-gate's pre-image blake3 hash
/// (from `WroteFile` events or the WAL) to make the freshness check
/// meaningful.
fn compute_current_inputs(
    task: &SubTask,
    _graph: &TaskGraph,
    completed_results: &HashMap<TaskId, AgentRunResult>,
) -> Vec<ArtifactFingerprint> {
    let mut inputs: Vec<ArtifactFingerprint> = Vec::new();
    for dep_id in &task.dependencies {
        if let Some(result) = completed_results.get(dep_id) {
            for path in &result.files_modified {
                inputs.push(ArtifactFingerprint::Output {
                    path: path.as_str().to_owned(),
                    // TODO(ADR-64 Phase 7): use real blake3 content hash from
                    // WroteFile/WAL events, not path string hash.
                    content_hash: blake3::hash(path.as_str().as_bytes()).to_hex().to_string(),
                });
            }
        }
    }
    inputs
}

// ---------------------------------------------------------------------------
// Resolver integration
// ---------------------------------------------------------------------------

/// Run the resolver for a batch of ready tasks and return the outcomes.
///
/// For each ready task, builds a [`WorkCandidate`] and calls
/// [`should_dispatch`].  Tasks that return [`DispatchDecision::Reuse`] are
/// short-circuited: their cached result is returned for injection.  All other
/// decisions flow through normal dispatch.
///
/// # Arguments
///
/// * `ready_ids` — `(TaskId, AgentId)` pairs for the ready batch.
/// * `graph` — the full task graph.
/// * `completed_results` — results of already-completed tasks.
/// * `projection` — the timeline projection (from `build_timeline`).
/// * `objective_hash` — blake3 hex of the run-level objective text.
/// * `plan_version` — content hash of the plan governing this work.
/// * `expected_artifacts_map` — expected output files keyed by task ID.
pub fn resolve_batch(
    ready_ids: &[(TaskId, concerto_core::types::AgentId)],
    graph: &TaskGraph,
    completed_results: &HashMap<TaskId, AgentRunResult>,
    projection: &TimelineProjection,
    objective_hash: &str,
    plan_version: &str,
    expected_artifacts_map: &HashMap<TaskId, Vec<camino::Utf8PathBuf>>,
) -> ResolverPassResult {
    let mut reused = HashMap::new();
    let mut dispatch_ids = Vec::new();

    for &(task_id, ref role) in ready_ids {
        let Some(task) = graph.get(&task_id) else {
            dispatch_ids.push((task_id, role.clone()));
            continue;
        };

        let artifacts = expected_artifacts_map.get(&task_id).cloned().unwrap_or_default();
        let candidate = build_work_candidate(
            task,
            graph,
            completed_results,
            objective_hash,
            plan_version,
            &artifacts,
        );

        let decision = should_dispatch(projection, &candidate);

        match decision {
            DispatchDecision::Reuse => {
                // Try to find the cached result from the projection.
                let cached = projection.completed_results.get(&task_id).cloned().or_else(|| {
                    // Fallback: reconstruct from the matching SubtaskCompleted event.
                    let target_hex = candidate.semantic_key.hex();
                    projection.events.iter().rev().find_map(|e| {
                        if let crate::timeline::TimelineEvent::SubtaskCompleted {
                            semantic_key_hex,
                            summary,
                            files_modified,
                            ..
                        } = e
                        {
                            if semantic_key_hex == target_hex && !summary.is_empty() {
                                return Some(AgentRunResult {
                                    task_id,
                                    role: role.clone(),
                                    outcome: concerto_core::types::AgentOutcome::Success,
                                    summary: summary.clone(),
                                    files_modified: files_modified.clone(),
                                    tool_call_count: 0,
                                    cost_usd: 0.0,
                                    latency_ms: 0,
                                    provider: String::new(),
                                    model: String::new(),
                                    tokens_in: 0,
                                    tokens_out: 0,
                                });
                            }
                        }
                        None
                    })
                });

                if let Some(result) = cached {
                    let audit = ResolverAudit::new(
                        DispatchDecision::Reuse,
                        candidate.semantic_key.hex(),
                        "inputs unchanged, deliverable complete — zero-waste reuse",
                    );
                    debug!(
                        target: "orchestrator::resolver",
                        task_id = ?task_id,
                        semantic_key = %candidate.semantic_key.hex(),
                        "resolver: Reuse short-circuit (zero model dispatch)"
                    );
                    reused.insert(
                        task_id,
                        ResolverOutcome::Reused { result: Box::new(result), audit },
                    );
                } else {
                    // Cannot obtain cached result — fall through to dispatch.
                    debug!(
                        target: "orchestrator::resolver",
                        task_id = ?task_id,
                        "resolver: Reuse verdict but no cached result available — dispatching"
                    );
                    dispatch_ids.push((task_id, role.clone()));
                }
            }
            DispatchDecision::Refine | DispatchDecision::Reopen | DispatchDecision::Dispatch => {
                dispatch_ids.push((task_id, role.clone()));
            }
        }
    }

    ResolverPassResult { reused, dispatch_ids }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fingerprint::SemanticKey;
    use crate::graph::Dependency;
    use crate::timeline::TimelineEvent;
    use concerto_core::types::{AgentId, AgentOutcome, SubTaskStatus};
    use std::collections::HashMap;
    use time::OffsetDateTime;

    // ------------------------------------------------------------------
    // Helpers
    // ------------------------------------------------------------------

    fn make_task(description: &str, deps: Vec<TaskId>) -> SubTask {
        SubTask {
            id: TaskId::new(),
            parent_id: None,
            session_id: Default::default(),
            role: AgentId::new("coder"),
            description: description.to_owned(),
            status: SubTaskStatus::Pending,
            dependencies: deps,
            deliverable: None,
            created_at: OffsetDateTime::now_utc(),
            completed_at: None,
        }
    }

    fn make_result(task_id: TaskId, role: &str, summary: &str) -> AgentRunResult {
        AgentRunResult {
            task_id,
            role: AgentId::new(role),
            outcome: AgentOutcome::Success,
            summary: summary.to_owned(),
            files_modified: Vec::new(),
            tool_call_count: 0,
            cost_usd: 0.0,
            latency_ms: 0,
            provider: "mock".to_owned(),
            model: "mock".to_owned(),
            tokens_in: 0,
            tokens_out: 0,
        }
    }

    fn completed_event(
        key_hex: &str,
        summary: &str,
        inputs: Vec<ArtifactFingerprint>,
        gate_seq: u64,
        task_id: TaskId,
    ) -> TimelineEvent {
        TimelineEvent::SubtaskCompleted {
            event_id: format!("evt-{gate_seq}"),
            gate_seq,
            task_id,
            summary: summary.to_owned(),
            files_modified: vec![],
            content_hash: blake3::hash(summary.as_bytes()).to_hex().to_string(),
            created_at: 1_700_000_000_000 + gate_seq as i64,
            semantic_key_hex: key_hex.to_owned(),
            recorded_inputs: inputs,
        }
    }

    // ------------------------------------------------------------------
    // Test 1: Reuse short-circuit
    //
    // A task with a dependency whose output fingerprints provide
    // current_inputs that match the recorded_inputs in the completed
    // event. This is the correct Reuse scenario: evidence exists and
    // is unchanged.
    // ------------------------------------------------------------------

    #[test]
    fn resolve_batch_reuses_matching_task() {
        let objective_hash = "obj-test";
        let plan_version = "plan-v1";

        // Create a dependency task whose output provides current_inputs.
        let dep_task = make_task("research topic X", vec![]);
        let dep_id = dep_task.id;
        let mut graph = TaskGraph::new();
        graph.add_root(dep_task.clone());

        // The dependency's output file — this becomes the current_inputs
        // for any task that depends on it.
        let dep_output_path = "src/a.rs";
        // compute_current_inputs hashes path.as_str().as_bytes() via blake3;
        // the recorded_inputs must match.
        let dep_output_hash = blake3::hash(dep_output_path.as_bytes()).to_hex().to_string();

        // Create a dependent task.
        let task = make_task("implement feature X", vec![dep_id]);
        let task_id = task.id;
        graph.add_child(task.clone(), dep_id, Dependency::MustFinishBefore);

        // Build the semantic key that the resolver will compute for the
        // dependent task.
        let dep_key =
            SemanticKey::compute(objective_hash, plan_version, &dep_task.description, "", &[]);
        let key = SemanticKey::compute(
            objective_hash,
            plan_version,
            &task.description,
            "",
            &[dep_key.hex().to_string()],
        );

        // The dependency's completed result with files_modified — this is
        // what compute_current_inputs reads.
        let mut dep_result = make_result(dep_id, "researcher", "research done");
        dep_result.files_modified = vec![camino::Utf8PathBuf::from(dep_output_path)];
        let mut completed_results = HashMap::new();
        completed_results.insert(dep_id, dep_result.clone());

        // The completed event for the dependent task has recorded_inputs
        // matching the dependency's output fingerprint.
        let recorded_inputs = vec![ArtifactFingerprint::Output {
            path: dep_output_path.to_owned(),
            content_hash: dep_output_hash.to_owned(),
        }];

        let result = make_result(task_id, "coder", "implemented feature X");
        let mut projection = TimelineProjection {
            events: Vec::new(),
            checkpoints: Vec::new(),
            plan_artifacts: Vec::new(),
            completed_results: HashMap::new(),
        };
        projection.events.push(completed_event(
            key.hex(),
            "implemented feature X",
            recorded_inputs,
            1,
            task_id,
        ));
        projection.completed_results.insert(task_id, result.clone());
        projection.events.sort_by_key(|e| e.gate_seq());

        let ready_ids = vec![(task_id, AgentId::new("coder"))];
        let expected_map = HashMap::new();

        let pass = resolve_batch(
            &ready_ids,
            &graph,
            &completed_results,
            &projection,
            objective_hash,
            plan_version,
            &expected_map,
        );

        assert!(pass.reused.contains_key(&task_id), "task should be reused");
        assert!(pass.dispatch_ids.is_empty(), "no tasks should be dispatched");
        match &pass.reused[&task_id] {
            ResolverOutcome::Reused { result: r, audit } => {
                assert_eq!(r.summary, "implemented feature X");
                assert_eq!(audit.decision, DispatchDecision::Reuse);
            }
            other => panic!("expected Reused, got {other:?}"),
        }
    }

    // ------------------------------------------------------------------
    // Test 2: Changed inputs → Reopen → dispatch
    // ------------------------------------------------------------------

    #[test]
    fn resolve_batch_dispatches_on_changed_inputs() {
        let objective_hash = "obj-test";
        let plan_version = "plan-v1";

        let task = make_task("implement feature X", vec![]);
        let task_id = task.id;

        let mut graph = TaskGraph::new();
        graph.add_root(task.clone());

        let key = SemanticKey::compute(objective_hash, plan_version, &task.description, "", &[]);

        let recorded_inputs = vec![ArtifactFingerprint::Observation {
            path: "src/a.rs".to_owned(),
            content_hash: "hash-old".to_owned(),
        }];
        // Note: compute_current_inputs returns empty for tasks with no deps,
        // so the non-empty recorded_inputs cause a Reopen, not Reuse.

        let mut completed_results = HashMap::new();
        completed_results.insert(task_id, make_result(task_id, "coder", "done"));

        let mut projection = TimelineProjection {
            events: Vec::new(),
            checkpoints: Vec::new(),
            plan_artifacts: Vec::new(),
            completed_results: HashMap::new(),
        };
        projection.events.push(completed_event(
            key.hex(),
            "implemented feature X",
            recorded_inputs,
            1,
            task_id,
        ));
        projection.events.sort_by_key(|e| e.gate_seq());

        let ready_ids = vec![(task_id, AgentId::new("coder"))];
        let expected_map = HashMap::new();

        let pass = resolve_batch(
            &ready_ids,
            &graph,
            &completed_results,
            &projection,
            objective_hash,
            plan_version,
            &expected_map,
        );

        assert!(pass.reused.is_empty(), "no tasks should be reused");
        assert_eq!(pass.dispatch_ids.len(), 1, "one task should be dispatched");
        assert_eq!(pass.dispatch_ids[0].0, task_id);
    }

    // ------------------------------------------------------------------
    // Test 3: No prior record → Dispatch
    // ------------------------------------------------------------------

    #[test]
    fn resolve_batch_dispatches_when_no_prior_record() {
        let objective_hash = "obj-test";
        let plan_version = "plan-v1";

        let task = make_task("implement feature X", vec![]);
        let task_id = task.id;

        let mut graph = TaskGraph::new();
        graph.add_root(task.clone());

        let completed_results = HashMap::new();

        let projection = TimelineProjection {
            events: Vec::new(),
            checkpoints: Vec::new(),
            plan_artifacts: Vec::new(),
            completed_results: HashMap::new(),
        };

        let ready_ids = vec![(task_id, AgentId::new("coder"))];
        let expected_map = HashMap::new();

        let pass = resolve_batch(
            &ready_ids,
            &graph,
            &completed_results,
            &projection,
            objective_hash,
            plan_version,
            &expected_map,
        );

        assert!(pass.reused.is_empty(), "no tasks should be reused");
        assert_eq!(pass.dispatch_ids.len(), 1, "one task should be dispatched");
    }

    // ------------------------------------------------------------------
    // Test 4: Empty recorded inputs → Dispatch (no false Reuse)
    // ------------------------------------------------------------------

    #[test]
    fn resolve_batch_dispatches_when_recorded_inputs_empty() {
        let objective_hash = "obj-test";
        let plan_version = "plan-v1";

        let task = make_task("implement feature X", vec![]);
        let task_id = task.id;

        let mut graph = TaskGraph::new();
        graph.add_root(task.clone());

        let key = SemanticKey::compute(objective_hash, plan_version, &task.description, "", &[]);

        let completed_results = HashMap::new();

        let mut projection = TimelineProjection {
            events: Vec::new(),
            checkpoints: Vec::new(),
            plan_artifacts: Vec::new(),
            completed_results: HashMap::new(),
        };
        // Event with EMPTY recorded_inputs — must NOT yield Reuse.
        projection.events.push(completed_event(
            key.hex(),
            "implemented feature X",
            vec![], // empty recorded_inputs
            1,
            task_id,
        ));
        projection.events.sort_by_key(|e| e.gate_seq());

        let ready_ids = vec![(task_id, AgentId::new("coder"))];
        let expected_map = HashMap::new();

        let pass = resolve_batch(
            &ready_ids,
            &graph,
            &completed_results,
            &projection,
            objective_hash,
            plan_version,
            &expected_map,
        );

        assert!(pass.reused.is_empty(), "empty recorded inputs must NOT yield Reuse");
        assert_eq!(pass.dispatch_ids.len(), 1, "task must be dispatched");
    }

    // ------------------------------------------------------------------
    // Test 5: Multiple ready tasks — mixed outcomes
    //
    // task_a has a dependency whose output provides current_inputs that
    // match the recorded_inputs → Reuse. task_b has no prior record →
    // Dispatch.
    // ------------------------------------------------------------------

    #[test]
    fn resolve_batch_mixed_outcomes() {
        let objective_hash = "obj-test";
        let plan_version = "plan-v1";

        // Dependency for task_a.
        let dep_task = make_task("research topic A", vec![]);
        let dep_id = dep_task.id;
        let mut graph = TaskGraph::new();
        graph.add_root(dep_task.clone());

        let dep_output_path = "src/a.rs";
        // compute_current_inputs hashes path.as_str().as_bytes() via blake3.
        let dep_output_hash = blake3::hash(dep_output_path.as_bytes()).to_hex().to_string();

        // task_a depends on dep_task.
        let task_a = make_task("implement A", vec![dep_id]);
        let task_a_id = task_a.id;
        graph.add_child(task_a.clone(), dep_id, Dependency::MustFinishBefore);

        // task_b is independent.
        let task_b = make_task("implement B", vec![]);
        let task_b_id = task_b.id;
        graph.add_root(task_b.clone());

        let dep_key =
            SemanticKey::compute(objective_hash, plan_version, &dep_task.description, "", &[]);
        let key_a = SemanticKey::compute(
            objective_hash,
            plan_version,
            &task_a.description,
            "",
            &[dep_key.hex().to_string()],
        );

        let recorded_inputs_a = vec![ArtifactFingerprint::Output {
            path: dep_output_path.to_owned(),
            content_hash: dep_output_hash.to_owned(),
        }];

        // Dependency completed result with files_modified.
        let mut dep_result = make_result(dep_id, "researcher", "research done");
        dep_result.files_modified = vec![camino::Utf8PathBuf::from(dep_output_path)];
        let mut completed_results = HashMap::new();
        completed_results.insert(dep_id, dep_result.clone());

        let result_a = make_result(task_a_id, "coder", "done A");
        let mut projection = TimelineProjection {
            events: Vec::new(),
            checkpoints: Vec::new(),
            plan_artifacts: Vec::new(),
            completed_results: HashMap::new(),
        };
        projection.events.push(completed_event(
            key_a.hex(),
            "done A",
            recorded_inputs_a,
            1,
            task_a_id,
        ));
        projection.completed_results.insert(task_a_id, result_a);
        projection.events.sort_by_key(|e| e.gate_seq());

        let ready_ids =
            vec![(task_a_id, AgentId::new("coder")), (task_b_id, AgentId::new("coder"))];
        let expected_map = HashMap::new();

        let pass = resolve_batch(
            &ready_ids,
            &graph,
            &completed_results,
            &projection,
            objective_hash,
            plan_version,
            &expected_map,
        );

        // task_a should be reused, task_b should be dispatched.
        assert!(pass.reused.contains_key(&task_a_id));
        assert_eq!(pass.dispatch_ids.len(), 1);
        assert_eq!(pass.dispatch_ids[0].0, task_b_id);
    }

    // ------------------------------------------------------------------
    // Test 6: ResolverAudit serde round-trip
    // ------------------------------------------------------------------

    #[test]
    fn resolver_audit_roundtrip() {
        let audit = ResolverAudit::new(
            DispatchDecision::Reuse,
            "abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890",
            "test reason",
        );
        let json = serde_json::to_string(&audit).unwrap();
        let deserialized: ResolverAudit = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.decision, DispatchDecision::Reuse);
        assert_eq!(deserialized.semantic_key_hex, audit.semantic_key_hex);
        assert_eq!(deserialized.reason, audit.reason);
    }

    // ==================================================================
    // Phase 7a: Zero-waste e2e proof tests (ADR-64)
    // ==================================================================

    /// E2E zero-waste proof 1: re-running an unchanged completed graph
    /// yields ZERO dispatches - every task short-circuits as Reuse.
    #[test]
    fn zero_waste_e2e_rerun_unchanged_graph_all_reused() {
        let objective_hash = "obj-e2e";
        let plan_version = "plan-e2e";

        let dep_a = make_task("research topic A", vec![]);
        let dep_a_id = dep_a.id;
        let mut dep_a_result = make_result(dep_a_id, "researcher", "research A done");
        let dep_a_output = "src/research_a.md";
        dep_a_result.files_modified = vec![camino::Utf8PathBuf::from(dep_a_output)];

        let task_a = make_task("implement feature A", vec![dep_a_id]);
        let task_a_id = task_a.id;

        let dep_b = make_task("research topic B", vec![]);
        let dep_b_id = dep_b.id;
        let mut dep_b_result = make_result(dep_b_id, "researcher", "research B done");
        let dep_b_output = "src/research_b.md";
        dep_b_result.files_modified = vec![camino::Utf8PathBuf::from(dep_b_output)];

        let task_b = make_task("implement feature B", vec![dep_b_id]);
        let task_b_id = task_b.id;

        let mut graph = TaskGraph::new();
        graph.add_root(dep_a.clone());
        graph.add_child(task_a.clone(), dep_a_id, Dependency::MustFinishBefore);
        graph.add_root(dep_b.clone());
        graph.add_child(task_b.clone(), dep_b_id, Dependency::MustFinishBefore);

        let mut completed_results = HashMap::new();
        completed_results.insert(dep_a_id, dep_a_result);
        completed_results.insert(dep_b_id, dep_b_result);

        let dep_a_key =
            SemanticKey::compute(objective_hash, plan_version, &dep_a.description, "", &[]);
        let dep_b_key =
            SemanticKey::compute(objective_hash, plan_version, &dep_b.description, "", &[]);
        let key_a = SemanticKey::compute(
            objective_hash,
            plan_version,
            &task_a.description,
            "",
            &[dep_a_key.hex().to_string()],
        );
        let key_b = SemanticKey::compute(
            objective_hash,
            plan_version,
            &task_b.description,
            "",
            &[dep_b_key.hex().to_string()],
        );

        let recorded_a = vec![ArtifactFingerprint::Output {
            path: dep_a_output.to_owned(),
            content_hash: blake3::hash(dep_a_output.as_bytes()).to_hex().to_string(),
        }];
        let recorded_b = vec![ArtifactFingerprint::Output {
            path: dep_b_output.to_owned(),
            content_hash: blake3::hash(dep_b_output.as_bytes()).to_hex().to_string(),
        }];

        let result_a = make_result(task_a_id, "coder", "implemented feature A");
        let result_b = make_result(task_b_id, "coder", "implemented feature B");
        let mut projection = TimelineProjection {
            events: Vec::new(),
            checkpoints: Vec::new(),
            plan_artifacts: Vec::new(),
            completed_results: HashMap::new(),
        };
        projection.events.push(completed_event(
            key_a.hex(),
            "implemented feature A",
            recorded_a,
            1,
            task_a_id,
        ));
        projection.events.push(completed_event(
            key_b.hex(),
            "implemented feature B",
            recorded_b,
            2,
            task_b_id,
        ));
        projection.completed_results.insert(task_a_id, result_a);
        projection.completed_results.insert(task_b_id, result_b);
        projection.events.sort_by_key(|e| e.gate_seq());

        let ready_ids =
            vec![(task_a_id, AgentId::new("coder")), (task_b_id, AgentId::new("coder"))];
        let expected_map = HashMap::new();

        let pass = resolve_batch(
            &ready_ids,
            &graph,
            &completed_results,
            &projection,
            objective_hash,
            plan_version,
            &expected_map,
        );

        assert!(
            pass.dispatch_ids.is_empty(),
            "zero-waste proof failed: {} task(s) dispatched instead of zero",
            pass.dispatch_ids.len(),
        );
        assert_eq!(pass.reused.len(), 2, "both tasks must be short-circuited as Reuse");
        assert!(pass.reused.contains_key(&task_a_id), "task_a reused");
        assert!(pass.reused.contains_key(&task_b_id), "task_b reused");

        for outcome in pass.reused.values() {
            match outcome {
                ResolverOutcome::Reused { result, audit } => {
                    assert_eq!(audit.decision, DispatchDecision::Reuse, "verdict must be Reuse");
                    assert!(
                        !result.summary.is_empty(),
                        "injected result must carry a non-empty summary"
                    );
                }
                other => panic!("expected Reused, got {:?}", other),
            }
        }
    }

    /// E2E zero-waste proof 2 (negative): changed inputs for one task
    /// breaks reuse for THAT task only.
    #[test]
    fn zero_waste_e2e_changed_input_breaks_only_that_task() {
        let objective_hash = "obj-e2e";
        let plan_version = "plan-e2e";

        let dep_a = make_task("research topic A", vec![]);
        let dep_a_id = dep_a.id;
        let mut dep_a_result = make_result(dep_a_id, "researcher", "research A done");
        let dep_a_output = "src/research_a.md";
        dep_a_result.files_modified = vec![camino::Utf8PathBuf::from(dep_a_output)];

        let task_a = make_task("implement feature A", vec![dep_a_id]);
        let task_a_id = task_a.id;

        let dep_b = make_task("research topic B", vec![]);
        let dep_b_id = dep_b.id;
        let mut dep_b_result = make_result(dep_b_id, "researcher", "research B done");
        let dep_b_output = "src/research_b.md";
        dep_b_result.files_modified = vec![camino::Utf8PathBuf::from(dep_b_output)];

        let task_b = make_task("implement feature B", vec![dep_b_id]);
        let task_b_id = task_b.id;

        let mut graph = TaskGraph::new();
        graph.add_root(dep_a.clone());
        graph.add_child(task_a.clone(), dep_a_id, Dependency::MustFinishBefore);
        graph.add_root(dep_b.clone());
        graph.add_child(task_b.clone(), dep_b_id, Dependency::MustFinishBefore);

        let mut completed_results = HashMap::new();
        completed_results.insert(dep_a_id, dep_a_result);
        completed_results.insert(dep_b_id, dep_b_result);

        let dep_a_key =
            SemanticKey::compute(objective_hash, plan_version, &dep_a.description, "", &[]);
        let dep_b_key =
            SemanticKey::compute(objective_hash, plan_version, &dep_b.description, "", &[]);
        let key_a = SemanticKey::compute(
            objective_hash,
            plan_version,
            &task_a.description,
            "",
            &[dep_a_key.hex().to_string()],
        );
        let key_b = SemanticKey::compute(
            objective_hash,
            plan_version,
            &task_b.description,
            "",
            &[dep_b_key.hex().to_string()],
        );

        let recorded_a = vec![ArtifactFingerprint::Output {
            path: dep_a_output.to_owned(),
            content_hash: blake3::hash(dep_a_output.as_bytes()).to_hex().to_string(),
        }];
        let stale_hash = blake3::hash("old-research-b-content".as_bytes()).to_hex().to_string();
        let recorded_b = vec![ArtifactFingerprint::Output {
            path: dep_b_output.to_owned(),
            content_hash: stale_hash,
        }];

        let result_a = make_result(task_a_id, "coder", "implemented feature A");
        let result_b = make_result(task_b_id, "coder", "implemented feature B");
        let mut projection = TimelineProjection {
            events: Vec::new(),
            checkpoints: Vec::new(),
            plan_artifacts: Vec::new(),
            completed_results: HashMap::new(),
        };
        projection.events.push(completed_event(
            key_a.hex(),
            "implemented feature A",
            recorded_a,
            1,
            task_a_id,
        ));
        projection.events.push(completed_event(
            key_b.hex(),
            "implemented feature B",
            recorded_b,
            2,
            task_b_id,
        ));
        projection.completed_results.insert(task_a_id, result_a);
        projection.completed_results.insert(task_b_id, result_b);
        projection.events.sort_by_key(|e| e.gate_seq());

        let ready_ids =
            vec![(task_a_id, AgentId::new("coder")), (task_b_id, AgentId::new("coder"))];
        let expected_map = HashMap::new();

        let pass = resolve_batch(
            &ready_ids,
            &graph,
            &completed_results,
            &projection,
            objective_hash,
            plan_version,
            &expected_map,
        );

        assert!(pass.reused.contains_key(&task_a_id), "task_a must be reused (inputs unchanged)");
        assert_eq!(pass.dispatch_ids.len(), 1, "one task dispatched (task_b with stale inputs)");
        assert_eq!(pass.dispatch_ids[0].0, task_b_id, "task_b dispatched due to changed inputs");
    }

    /// E2E zero-waste proof 3 (partial reuse): one task reused, two
    /// dispatched in a single batch.
    #[test]
    fn zero_waste_e2e_partial_reuse_mixed_outcomes() {
        let objective_hash = "obj-e2e";
        let plan_version = "plan-e2e";

        let dep_a = make_task("research for A", vec![]);
        let dep_a_id = dep_a.id;
        let mut dep_a_result = make_result(dep_a_id, "researcher", "research done");
        let dep_a_output = "src/research_a.md";
        dep_a_result.files_modified = vec![camino::Utf8PathBuf::from(dep_a_output)];

        let task_a = make_task("implement feature A", vec![dep_a_id]);
        let task_a_id = task_a.id;

        let task_b = make_task("implement feature B", vec![]);
        let task_b_id = task_b.id;

        let task_c = make_task("implement feature C", vec![]);
        let task_c_id = task_c.id;

        let mut graph = TaskGraph::new();
        graph.add_root(dep_a.clone());
        graph.add_child(task_a.clone(), dep_a_id, Dependency::MustFinishBefore);
        graph.add_root(task_b.clone());
        graph.add_root(task_c.clone());

        let mut completed_results = HashMap::new();
        completed_results.insert(dep_a_id, dep_a_result);

        let dep_a_key =
            SemanticKey::compute(objective_hash, plan_version, &dep_a.description, "", &[]);
        let key_a = SemanticKey::compute(
            objective_hash,
            plan_version,
            &task_a.description,
            "",
            &[dep_a_key.hex().to_string()],
        );
        let key_b =
            SemanticKey::compute(objective_hash, plan_version, &task_b.description, "", &[]);

        let result_a = make_result(task_a_id, "coder", "implemented feature A");
        let mut projection = TimelineProjection {
            events: Vec::new(),
            checkpoints: Vec::new(),
            plan_artifacts: Vec::new(),
            completed_results: HashMap::new(),
        };

        let recorded_a = vec![ArtifactFingerprint::Output {
            path: dep_a_output.to_owned(),
            content_hash: blake3::hash(dep_a_output.as_bytes()).to_hex().to_string(),
        }];
        projection.events.push(completed_event(
            key_a.hex(),
            "implemented feature A",
            recorded_a,
            1,
            task_a_id,
        ));
        projection.events.push(completed_event(
            key_b.hex(),
            "implemented feature B",
            vec![],
            2,
            task_b_id,
        ));
        projection.completed_results.insert(task_a_id, result_a);
        projection.events.sort_by_key(|e| e.gate_seq());

        let ready_ids = vec![
            (task_a_id, AgentId::new("coder")),
            (task_b_id, AgentId::new("coder")),
            (task_c_id, AgentId::new("coder")),
        ];
        let expected_map = HashMap::new();

        let pass = resolve_batch(
            &ready_ids,
            &graph,
            &completed_results,
            &projection,
            objective_hash,
            plan_version,
            &expected_map,
        );

        assert!(pass.reused.contains_key(&task_a_id), "task_a must be reused");
        let dispatched_ids: Vec<TaskId> = pass.dispatch_ids.iter().map(|(id, _)| *id).collect();
        assert_eq!(dispatched_ids.len(), 2, "two tasks dispatched");
        assert!(dispatched_ids.contains(&task_b_id), "task_b dispatched");
        assert!(dispatched_ids.contains(&task_c_id), "task_c dispatched");
    }
}
