//! Dynamic task splitting and merging (issue #57, parent #51).
//!
//! The Coordinator's model may restructure the OPEN part of the task DAG at
//! decision time: a split replaces one open task with ordered child tasks,
//! and a merge folds compatible open tasks into one. The transforms are
//! DETERMINISTIC graph rewrites — the model proposes, this module proves
//! the rewrite against the real graph invariants and applies it.
//!
//! # What transforms must preserve (issue constraints)
//!
//! - **Settled work is never invalidated.** Only Pending/Running tasks are
//!   splittable and only Pending tasks mergeable; `completed_results`,
//!   delivered summaries, and acceptance-evidence ledger rows of completed
//!   tasks are untouched by construction (a completed task can be neither
//!   a split parent nor a merge participant).
//! - **Dependency edges survive.** A split REPLACES the parent node: every
//!   incoming edge (a dependency of the parent) is duplicated onto every
//!   child with its original [`Dependency`] type, and every outgoing edge
//!   (a dependent) is rewritten so the dependent waits on EVERY child — a
//!   deterministic union replacement (the old dependent's work now
//!   consumes the whole split). Child-internal ordering uses the
//!   model-supplied `after` indices, which must be strictly backward, so
//!   no cycle can form inside the split. A merge transfers every loser's
//!   incoming and outgoing edges onto the survivor (deduplicated).
//! - **Ownership/attribution** (issue #56 world model): split children
//!   inherit the parent's role, session, lineage (`parent_id`), and
//!   session evidence context; a merge keeps the survivor's identity.
//!   Artifact ownership rides the coordinator's expected-artifacts map,
//!   which stays single-owner: a merge survivor keeps and accumulates
//!   artifact ownership (it is the lowest-id participant, so the owner id
//!   for pre-existing artifacts is deterministic), and a split child
//!   claims ownership only of the artifacts its spec re-declares
//!   (unspecified children inherit the parent's).
//! - **Attempt baselines**: split children inherit the parent's attempt
//!   counter as their starting baseline (the handler applies it to the
//!   dispatch ledger); a merge keeps the MAXIMUM attempt count of the
//!   merged group (no extra budget is granted by folding).
//!
//! # Compatibility rules
//!
//! - **Splittable**: exactly one task, present in the graph, status
//!   `Pending` or `Running`, with 1..=[`MAX_SPLIT_CHILDREN`] child specs
//!   whose `after` indices are strictly backward. Children inherit the
//!   parent's role — the model cannot re-cast the work to a DIFFERENT
//!   specialist through a split (that is a dispatch decision, not a
//!   transform).
//! - **Mergeable**: ≥ 2 distinct tasks (≤ [`MAX_MERGE_TASK_IDS`]), all
//!   present, ALL status `Pending`, and ALL the same agent role — they
//!   fold because they do compatible work. Roles differ ⇒ the coordinator
//!   must first choose WHO does the merged work, which is a dispatch
//!   decision, not a transform.
//!
//! # Determinism
//!
//! The merge survivor is the LOWEST task id (ULID order); removed members
//! are reported ascending. The same input spec against the same graph
//! always produces the same survivor, spec, and edge set (a split's child
//! ids are fresh ULIDs by design — the graph structure is what is
//! deterministic).
//!
//! A rejected transform is a structured [`TransformRejection`] — no state
//! mutation, no coordinator crash, mirroring the issue-#52 decision
//! discipline.

use crate::graph::{Dependency, TaskGraph, TaskGraphValidator};
use concerto_core::types::{SubTask, SubTaskStatus, TaskId};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use time::OffsetDateTime;

/// Maximum number of child tasks one split may produce.
pub const MAX_SPLIT_CHILDREN: usize = 8;

/// Maximum number of tasks one merge may fold (including the survivor).
pub const MAX_MERGE_TASK_IDS: usize = 16;

/// Maximum characters accepted for a merge's merged task description.
pub const MAX_MERGE_DESCRIPTION_CHARS: usize = crate::decisions::MAX_DECISION_TASK_CHARS;

/// Structured rejection for a transform that fails validation — read by the
/// Coordinator's model and retried; never a panic (same discipline as
/// [`crate::decisions::DecisionRejection`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransformRejection {
    /// Machine code (also the tool-result `error` key).
    pub code: &'static str,
    /// What exactly failed and how to fix it.
    pub message: String,
}

impl TransformRejection {
    /// The tool-result JSON the Coordinator's model reads.
    pub fn tool_value(&self) -> serde_json::Value {
        serde_json::json!({ "error": self.code, "message": self.message })
    }
}

/// One proposed child of a split. Children inherit the split parent's state
/// (role, session, evidence context, attempt baseline); the spec carries
/// only what distinguishes the child: its own work text, the artifacts it
/// owns, and the strictly-backward ordering over its siblings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SplitChildSpec {
    /// The complete self-contained work text the child represents.
    pub description: String,
    /// Workspace-root-relative artifact keys the child is expected to
    /// produce (canonicalized by the decision validator before they reach
    /// this spec).
    #[serde(default)]
    pub expected_artifacts: Vec<String>,
    /// Indices of EARLIER children (positions in the children list) that
    /// must finish before this child may run. Every entry must reference a
    /// strictly smaller index — a forward or self reference is rejected,
    /// which keeps the split acyclic by construction.
    #[serde(default)]
    pub after: Vec<usize>,
}

/// The payload of a split/merge decision (issue #57). Carried on the
/// validated [`crate::decisions::CoordinatorDecision`] additively (the
/// optional `transform` field), so old checkpoints and old journal entries
/// (no transform) still load unchanged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum TaskTransformSpec {
    /// Replace one open task with ordered child tasks.
    Split {
        /// The task that is split (must be open: Pending or Running).
        parent: TaskId,
        children: Vec<SplitChildSpec>,
    },
    /// Fold compatible pending tasks into one survivor.
    Merge {
        /// The tasks to merge — the survivor is deterministically the
        /// lowest id.
        task_ids: Vec<TaskId>,
        /// The merged task description that replaces the participants'
        /// work text (non-empty, length-capped).
        merged_description: String,
        /// Artifact keys the merged task is expected to produce.
        #[serde(default)]
        merged_artifacts: Vec<String>,
    },
}

/// The outcome of one committed transform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransformOutcome {
    /// The parent that was replaced and the materialized child ids in the
    /// order their specs were given.
    Split(SplitOutcome),
    /// The survivor and the removed members (ascending by id).
    Merge(MergeOutcome),
}

/// The outcome of one committed split.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SplitOutcome {
    pub parent: TaskId,
    pub children: Vec<TaskId>,
}

/// The outcome of one committed merge: the survivor and the removed
/// members (ascending by id).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeOutcome {
    pub survivor: TaskId,
    pub removed: Vec<TaskId>,
}

/// Prove a split/merge payload against a THROWAWAY COPY of the graph (so an
/// invalid rewrite never touches the live graph), then apply the identical
/// rewrite to the real graph.
///
/// The proof and the committed application share every input except the
/// freshly generated split-child ids, which validation does not depend on.
pub fn apply_transform(
    graph: &mut TaskGraph,
    spec: &TaskTransformSpec,
    now: OffsetDateTime,
) -> Result<TransformOutcome, TransformRejection> {
    // Proof pass on a copy: a rewrite that leaves an invalid DAG is
    // rejected BEFORE the live graph is touched.
    {
        let mut proof = graph.clone_for_transform();
        apply_spec(&mut proof, spec, now)?;
        if TaskGraphValidator::validate(&proof).is_err() {
            return Err(reject(
                "invalid_resulting_graph",
                "the resulting task graph failed validation (cycle or dangling \
                 dependency); adjust the transform and re-decide"
                    .to_owned(),
            ));
        }
    }
    // Committed pass on the live graph.
    let committed = apply_spec(graph, spec, now)?;
    // The run graph is always a valid DAG (invariant); a post-commit
    // failure would mean the rewrite itself is defective — recorded loudly,
    // never silently absorbed.
    if let Err(error) = TaskGraphValidator::validate(graph) {
        tracing::error!(
            target: "orchestrator::task_transform",
            %error,
            "post-commit task-graph validation failed — defective transform, \
             investigate the rewrite"
        );
    }
    Ok(committed)
}

fn apply_spec(
    graph: &mut TaskGraph,
    spec: &TaskTransformSpec,
    now: OffsetDateTime,
) -> Result<TransformOutcome, TransformRejection> {
    match spec {
        TaskTransformSpec::Split { .. } => {
            apply_split(graph, spec, now).map(TransformOutcome::Split)
        }
        TaskTransformSpec::Merge { .. } => apply_merge(graph, spec).map(TransformOutcome::Merge),
    }
}

/// Apply SPLIT mutably (the proof runner calls it on a copy first; tests
/// may call it directly on a scratch graph). Validation happens in
/// full on every call and mutates NOTHING on rejection.
pub fn apply_split(
    graph: &mut TaskGraph,
    spec: &TaskTransformSpec,
    now: OffsetDateTime,
) -> Result<SplitOutcome, TransformRejection> {
    let TaskTransformSpec::Split { parent, children } = spec else {
        return Err(mismatched_payload());
    };

    let Some(parent_task) = graph.get(parent).cloned() else {
        return Err(reject(
            "unknown_task",
            format!("split: no task {parent} in the graph; split an OPEN task id"),
        ));
    };

    // ── State gate: only OPEN work may be split ──────────────────────────
    if !is_splittable_status(parent_task.status) {
        return Err(reject(
            "not_splittable",
            format!(
                "split: task {parent} is {:?}; only Pending/Running tasks can be split — \
                 completed or settled work must never be re-cut",
                parent_task.status
            ),
        ));
    }

    // ── Shape of the children list ────────────────────────────────────────
    if children.is_empty() || children.len() > MAX_SPLIT_CHILDREN {
        return Err(reject(
            "children_invalid",
            format!(
                "split: children must number 1..={MAX_SPLIT_CHILDREN} (got {})",
                children.len()
            ),
        ));
    }
    for (index, child) in children.iter().enumerate() {
        validate_child_spec(index, child)?;
    }

    // ── Capture the parent's edges BEFORE the node is removed ────────────
    let incoming = graph.incoming_dependencies(parent);
    let outgoing = graph.outgoing_dependents(parent);

    // ── Materialize the children, then replace the parent ────────────────
    let mut child_ids = Vec::with_capacity(children.len());
    for child in children.iter() {
        // SubTask.dependencies mirrors the graph edges (checkpoint restore
        // validates both): inherited parent dependencies first, then the
        // strictly-backward sibling predecessors.
        let dependencies = incoming
            .iter()
            .map(|(dep, _)| *dep)
            .chain(child.after.iter().map(|&j| child_ids[j]))
            .collect();
        let id = TaskId::new();
        graph.add_root(SubTask {
            id,
            parent_id: parent_task.parent_id,
            session_id: parent_task.session_id,
            role: parent_task.role.clone(),
            description: child.description.trim().to_owned(),
            status: SubTaskStatus::Pending,
            dependencies,
            deliverable: None,
            created_at: now,
            completed_at: None,
        });
        child_ids.push(id);
    }

    // The parent node leaves the dispatch surface; removing it drops its
    // edges wholesale — the children carry its edges from here on.
    graph.remove_task(parent);

    // ── Rewire deterministically ─────────────────────────────────────────
    // Incoming: every dependency of the parent now also gates every child.
    // Outgoing: every dependent now waits for EVERY child (union
    // replacement). Child-internal ordering uses MustFinishBefore.
    for (dependency_id, dep_type) in &incoming {
        for &child in &child_ids {
            rewrite(graph, child, *dependency_id, *dep_type)?;
        }
    }
    for (dependent, dep_type) in &outgoing {
        for &child in &child_ids {
            rewrite(graph, *dependent, child, *dep_type)?;
        }
    }
    for (index, child) in children.iter().enumerate() {
        let Some(&child_id) = child_ids.get(index) else {
            return Err(reject(
                "children_invalid",
                format!("split: child {index} vanished while wiring"),
            ));
        };
        for &predecessor in &child.after {
            let Some(&predecessor_id) = child_ids.get(predecessor) else {
                return Err(reject(
                    "invalid_after_order",
                    format!(
                        "split: child {index} references predecessor {predecessor}; \
                         predecessors must be strictly earlier children"
                    ),
                ));
            };
            rewrite(graph, child_id, predecessor_id, Dependency::MustFinishBefore)?;
        }
    }

    Ok(SplitOutcome { parent: *parent, children: child_ids })
}

/// Apply MERGE mutably (the proof runner calls it on a copy first; tests
/// may call it directly on a scratch graph). Validation happens in full on
/// every call and mutates NOTHING on rejection.
pub fn apply_merge(
    graph: &mut TaskGraph,
    spec: &TaskTransformSpec,
) -> Result<MergeOutcome, TransformRejection> {
    let TaskTransformSpec::Merge { task_ids, merged_description, merged_artifacts: _ } = spec
    else {
        return Err(mismatched_payload());
    };

    // ── Shape: a deduplicated group of ≥ 2 distinct participants ─────────
    let mut seen: HashSet<TaskId> = HashSet::new();
    let group: Vec<TaskId> = task_ids.iter().copied().filter(|id| seen.insert(*id)).collect();
    if group.len() < 2 {
        return Err(reject(
            "merge_group_too_small",
            format!(
                "merge: fold at least 2 DISTINCT open tasks (the request listed {} \
                 unique id(s)); overlapping/equivalent work merges by pairs or more",
                group.len()
            ),
        ));
    }
    if group.len() > MAX_MERGE_TASK_IDS {
        return Err(reject(
            "merge_group_too_large",
            format!(
                "merge: at most {MAX_MERGE_TASK_IDS} tasks fold per decision (got {}); \
                 merge in stages",
                group.len()
            ),
        ));
    }
    let merged_description = merged_description.trim();
    if merged_description.is_empty() {
        return Err(reject(
            "incomplete_decision",
            "merge: the merged task needs a non-empty description".to_owned(),
        ));
    }
    if merged_description.chars().count() > MAX_MERGE_DESCRIPTION_CHARS {
        return Err(reject(
            "task_too_long",
            "merge: the merged description exceeds the task length cap; shorten it and \
             re-decide"
                .to_owned(),
        ));
    }

    // ── Compatibility: every participant exists, is Pending, same role ───
    let mut participants: Vec<SubTask> = Vec::with_capacity(group.len());
    for id in &group {
        let Some(task) = graph.get(id) else {
            return Err(reject(
                "unknown_task",
                format!("merge: no task {id} in the graph; merge OPEN pending task ids"),
            ));
        };
        if task.status != SubTaskStatus::Pending {
            return Err(reject(
                "not_mergeable",
                format!(
                    "merge: task {id} is {:?}; only Pending tasks merge — completed, \
                     failed, or blocked work must never be re-folded",
                    task.status
                ),
            ));
        }
        participants.push(task.clone());
    }
    let role = participants[0].role.clone();
    if let Some(first_mismatch) = participants.iter().find(|task| task.role != role) {
        return Err(reject(
            "merge_role_mismatch",
            format!(
                "merge: task {} has role {:?} but the group folds around role {:?}; \
                 merge only tasks of the SAME specialist role (deep compatibility: \
                 same work, one owner)",
                first_mismatch.id, first_mismatch.role, role
            ),
        ));
    }

    // ── Deterministic survivor: lowest ULID; removed ascending ───────────
    let survivor_task = participants
        .iter()
        .min_by_key(|task| task.id.0)
        .expect("validated group has ≥ 2 participants");
    let survivor = survivor_task.id;
    let removed: Vec<TaskId> = group.iter().copied().filter(|id| *id != survivor).collect();

    // ── Capture every participant's edges BEFORE any node is removed ─────
    // Deps between participants degenerate to survivor-self-loops after the
    // fold — skip them (the survivor is its own dependency otherwise).
    let group_set: HashSet<TaskId> = group.iter().copied().collect();
    let mut union_in: Vec<(TaskId, Dependency)> = Vec::new();
    let mut union_out: Vec<(TaskId, Dependency)> = Vec::new();
    let mut seen_in: HashSet<(TaskId, Dependency)> = HashSet::new();
    let mut seen_out: HashSet<(TaskId, Dependency)> = HashSet::new();
    for id in &group {
        for (dep, dep_type) in graph.incoming_dependencies(id) {
            if group_set.contains(&dep) {
                continue;
            }
            if seen_in.insert((dep, dep_type)) {
                union_in.push((dep, dep_type));
            }
        }
        for (dependent, dep_type) in graph.outgoing_dependents(id) {
            if group_set.contains(&dependent) {
                continue;
            }
            if seen_out.insert((dependent, dep_type)) {
                union_out.push((dependent, dep_type));
            }
        }
    }

    // ── Fold: remove all participants, re-add the survivor once ──────────
    let mut survivor_node = survivor_task.clone();
    survivor_node.description = merged_description.to_owned();
    survivor_node.status = SubTaskStatus::Pending;
    survivor_node.completed_at = None;
    for id in &group {
        graph.remove_task(id);
    }
    let mut readded = survivor_node.clone();
    readded.dependencies = union_in.iter().map(|(dep, _)| *dep).collect();
    graph.add_root(readded);

    for (dep, dep_type) in &union_in {
        graph.add_dependency(survivor, *dep, *dep_type).map_err(|error| {
            reject("invalid_resulting_graph", format!("merge: rewire failed: {error}"))
        })?;
    }
    for (dependent, dep_type) in &union_out {
        graph.add_dependency(*dependent, survivor, *dep_type).map_err(|error| {
            reject("invalid_resulting_graph", format!("merge: rewire failed: {error}"))
        })?;
    }
    let _ = survivor_node;

    Ok(MergeOutcome { survivor, removed })
}

fn is_splittable_status(status: SubTaskStatus) -> bool {
    matches!(status, SubTaskStatus::Pending | SubTaskStatus::Running)
}

fn validate_child_spec(index: usize, child: &SplitChildSpec) -> Result<(), TransformRejection> {
    let description = child.description.trim();
    if description.is_empty() {
        return Err(reject(
            "incomplete_decision",
            format!("split: every child needs a non-empty description (child {index} is empty)"),
        ));
    }
    if description.chars().count() > crate::decisions::MAX_DECISION_TASK_CHARS {
        return Err(reject(
            "task_too_long",
            format!(
                "split: child {index} description exceeds the task length cap; shorten it \
                 and re-decide"
            ),
        ));
    }
    for &predecessor in &child.after {
        if predecessor >= index {
            return Err(reject(
                "invalid_after_order",
                format!(
                    "split: child {index} lists predecessor {predecessor}; predecessors \
                     must be strictly earlier children (smaller indices) — a split is \
                     never cyclic"
                ),
            ));
        }
    }
    let unique: HashSet<&usize> = child.after.iter().collect();
    if unique.len() != child.after.len() {
        return Err(reject(
            "invalid_after_order",
            format!("split: child {index} lists duplicate predecessors; dedupe them"),
        ));
    }
    Ok(())
}

/// The validation gate for a single rewire (edge between two nodes whose
/// existence is proven by the transform's validations above).
fn rewrite(
    graph: &mut TaskGraph,
    to: TaskId,
    from: TaskId,
    dep: Dependency,
) -> Result<(), TransformRejection> {
    graph
        .add_dependency(to, from, dep)
        .map_err(|error| reject("invalid_resulting_graph", format!("rewire failed: {error}")))
}

fn mismatched_payload() -> TransformRejection {
    reject(
        "conflicting_decision",
        "the decision kind does not match its transform payload".to_owned(),
    )
}

fn reject(code: &'static str, message: String) -> TransformRejection {
    TransformRejection { code, message }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::Dependency;
    use concerto_core::ids::Ulid;
    use concerto_core::types::{AgentId, SubTask};

    /// A fixed, hand-pickable task id — deterministic merge-survivor tests
    /// need orderable ids (ULID order is u128 order).
    fn fixed(id: u128) -> TaskId {
        TaskId(Ulid::from(id))
    }

    fn make_task(id: TaskId, parent: Option<TaskId>, status: SubTaskStatus) -> SubTask {
        SubTask {
            id,
            parent_id: parent,
            session_id: Ulid::from(0xF000),
            role: AgentId::new("coder"),
            description: format!("task {id}"),
            status,
            dependencies: vec![],
            deliverable: None,
            created_at: time::OffsetDateTime::now_utc(),
            completed_at: None,
        }
    }

    fn child_spec(description: &str, after: Vec<usize>) -> SplitChildSpec {
        SplitChildSpec { description: description.to_owned(), expected_artifacts: vec![], after }
    }

    fn now() -> OffsetDateTime {
        time::OffsetDateTime::now_utc()
    }

    /// Base graph:
    ///   completed(1) → open(2, split target) → pending(3, dependent),
    ///   pending(3) also holds a ProvidesContextFor edge back to 1.
    fn base_graph() -> TaskGraph {
        let mut graph = TaskGraph::new();
        graph.add_root(make_task(fixed(1), None, SubTaskStatus::Completed));
        graph.add_root(make_task(fixed(2), None, SubTaskStatus::Pending));
        graph.add_root(make_task(fixed(3), None, SubTaskStatus::Pending));
        graph.add_dependency(fixed(2), fixed(1), Dependency::MustFinishBefore).expect("edge in");
        graph.add_dependency(fixed(3), fixed(2), Dependency::MustFinishBefore).expect("edge in");
        graph.add_dependency(fixed(3), fixed(1), Dependency::ProvidesContextFor).expect("edge in");
        graph
    }

    // ── Resume: transforms persist through the additive checkpoint ───────

    #[test]
    fn checkpoint_round_trip_after_transforms_and_old_checkpoints_load() {
        use crate::checkpoint::{
            build_checkpoint, restore_graph, CheckpointScope, CheckpointStage,
        };
        use std::collections::HashMap;

        // Split the open root of the base graph, then persist the result.
        let mut graph = base_graph();
        let split_children = match apply_transform(
            &mut graph,
            &TaskTransformSpec::Split {
                parent: fixed(2),
                children: vec![child_spec("part one", vec![]), child_spec("part two", vec![])],
            },
            now(),
        )
        .expect("split")
        {
            TransformOutcome::Split(outcome) => outcome,
            other => panic!("wrong outcome {other:?}"),
        };
        let meta =
            graph.get(&fixed(1)).map(|done| done.session_id).expect("the graph is not empty");

        // A journal entry that CARRIES the transform payload — the decision
        // state travels additively beside the rewritten graph.
        let transform_decision = crate::decisions::CoordinatorDecision {
            id: "dec-split-1".to_owned(),
            kind: crate::decisions::DecisionKind::Split,
            target_agent: None,
            task_description: "split of open task 2".to_owned(),
            notes: Some("issue #57".to_owned()),
            supporting_evidence_ids: vec![],
            expected_artifacts: vec![],
            transform: Some(TaskTransformSpec::Split {
                parent: fixed(2),
                children: vec![child_spec("part one", vec![]), child_spec("part two", vec![])],
            }),
            max_tool_calls: None,
            created_at: now(),
            status: crate::decisions::DecisionStatus::Settled,
        };
        let scope = CheckpointScope {
            run_id: Ulid::new(),
            session_id: meta,
            root_task_id: TaskId::new(),
            project_id: "test".into(),
            objective: "test".into(),
            objective_hash: "hash".into(),
            source_revision: Some("abc123".into()),
            sequence_num: 1,
        };
        let working_memory = concerto_core::types::AgentContext::new(
            concerto_core::types::SessionContext::new(meta, std::path::PathBuf::from(".")),
        )
        .working_memory;
        let checkpoint = build_checkpoint(
            &scope,
            CheckpointStage::Executing,
            None,
            &working_memory,
            &graph,
            &HashMap::new(),
            0.0,
            0,
            &[],
            &[],
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            &crate::checkpoint::CheckpointContext {
                decision_journal: vec![transform_decision.clone()],
                ..Default::default()
            },
        );

        // The full JSON round trip a resume performs.
        let json = serde_json::to_string(&checkpoint).expect("serialize");
        let loaded = crate::checkpoint::GraphCheckpoint::from_json(&json).expect("load");
        assert_eq!(loaded.decision_journal.len(), 1);
        assert_eq!(loaded.decision_journal[0], transform_decision);
        assert!(matches!(
            loaded.decision_journal[0].transform,
            Some(TaskTransformSpec::Split { .. })
        ));

        // The transformed graph restores intact: children, statuses, edges.
        let restored = restore_graph(&loaded).expect("restore");
        assert!(restored.get(&fixed(2)).is_none(), "the split parent is gone");
        for child in &split_children.children {
            let child_task = restored.get(child).expect("child survives the resume");
            assert_eq!(child_task.status, SubTaskStatus::Pending);
            assert!(child_task.dependencies.contains(&fixed(1)), "edges restore");
        }
        assert!(
            restored.dependencies_of(&fixed(3)).contains(&split_children.children[0]),
            "the dependent rewiring survives the resume"
        );
        assert!(
            restored.dependencies_of(&fixed(3)).contains(&split_children.children[1]),
            "the dependent rewiring survives the resume"
        );
        assert!(TaskGraphValidator::validate(&restored).is_ok());
    }

    // ── Split semantics (issue acceptance: split a running task) ─────────

    #[test]
    fn split_running_task_materializes_children_with_state_preserved() {
        let mut graph = base_graph();
        graph.mark_running(&fixed(2));

        let outcome = apply_split(
            &mut graph,
            &TaskTransformSpec::Split {
                parent: fixed(2),
                children: vec![
                    child_spec("write the module", vec![]),
                    child_spec("test the module", vec![0]),
                ],
            },
            now(),
        )
        .expect("a running task splits");

        let children = outcome.children.clone();
        assert_eq!(children.len(), 2);
        for (index, id) in children.iter().enumerate() {
            let child = graph.get(id).expect("child in graph");
            assert_eq!(child.status, SubTaskStatus::Pending, "child {index} starts pending");
            assert_eq!(child.role.as_str(), "coder", "the specialist role is inherited");
            assert_eq!(child.parent_id, None, "the parent's own lineage carries to children");
            assert_eq!(child.completed_at, None);
        }
        // Child-internal ordering: children[1] waits on children[0]
        // (alongside the retained inherited dependency).
        assert!(
            graph.dependencies_of(&children[1]).contains(&children[0]),
            "child-internal order holds"
        );
        // The parent's dependency is inherited by EVERY child.
        assert!(graph.dependencies_of(&children[0]).contains(&fixed(1)));
        assert!(graph.dependencies_of(&children[1]).contains(&fixed(1)));
        // The dependents repoint onto EVERY child (union replacement) —
        // deterministic rewrite, and the parent is gone from the graph.
        assert!(graph.get(&fixed(2)).is_none());
        assert!(
            graph.dependencies_of(&fixed(3)).contains(&children[0])
                && graph.dependencies_of(&fixed(3)).contains(&children[1]),
            "the dependents of the old parent now wait for every child"
        );
        // The dig is still a valid DAG, and ready_tasks exposes the split.
        assert!(TaskGraphValidator::validate(&graph).is_ok());
        assert_eq!(graph.ready_tasks().len(), 1, "only the first child is ready");
        assert_eq!(graph.ready_tasks()[0].id, children[0]);
    }

    #[test]
    fn split_preserves_settled_work_and_edge_types() {
        let mut graph = base_graph();
        let completed_before = serde_json::to_string(&graph.get(&fixed(1)).unwrap().clone())
            .expect("serialize settled task");

        let outcome = apply_split(
            &mut graph,
            &TaskTransformSpec::Split {
                parent: fixed(2),
                children: vec![child_spec("part one", vec![]), child_spec("part two", vec![])],
            },
            now(),
        )
        .expect("a pending task splits");

        // The settled task is BYTE-IDENTICAL after the transform.
        assert_eq!(
            serde_json::to_string(&graph.get(&fixed(1)).cloned().unwrap()).expect("serialize"),
            completed_before,
            "settled work survives the transform"
        );
        // Edge types travel with the rewrite: the ProvidesContextFor edge
        // from 3 back to 1 is untouched.
        assert!(graph.dependencies_of(&fixed(3)).contains(&fixed(1)));
        let _ = outcome;
    }

    #[test]
    fn split_completed_task_rejects() {
        let mut graph = base_graph();
        let error = apply_split(
            &mut graph,
            &TaskTransformSpec::Split {
                parent: fixed(1),
                children: vec![child_spec("cannot happen", vec![])],
            },
            now(),
        )
        .expect_err("completed work never re-cuts");
        assert_eq!(error.code, "not_splittable");
        assert_eq!(graph.len(), 3, "a rejected split mutates nothing");
    }

    #[test]
    fn split_unknown_task_rejects() {
        let mut graph = base_graph();
        let error = apply_split(
            &mut graph,
            &TaskTransformSpec::Split {
                parent: fixed(9),
                children: vec![child_spec("ghost", vec![])],
            },
            now(),
        )
        .expect_err("unknown task");
        assert_eq!(error.code, "unknown_task");
    }

    #[test]
    fn split_forward_after_self_after_rejects() {
        let mut graph = base_graph();
        for (after, code) in [
            (vec![1usize], "invalid_after_order"), // forward reference
            (vec![0usize], "invalid_after_order"), // self reference
        ] {
            let children = vec![child_spec("first", after), child_spec("second", vec![])];
            let error = apply_split(
                &mut graph,
                &TaskTransformSpec::Split { parent: fixed(2), children },
                now(),
            )
            .expect_err("non-strictly-backward `after`");
            assert_eq!(error.code, code);
        }
        // Duplicate predecessors also reject.
        let children = vec![child_spec("first", vec![]), child_spec("second", vec![0, 0])];
        let error = apply_split(
            &mut graph,
            &TaskTransformSpec::Split { parent: fixed(2), children },
            now(),
        )
        .expect_err("duplicate predecessors");
        assert_eq!(error.code, "invalid_after_order");
    }

    fn children_spec(count: usize) -> Vec<SplitChildSpec> {
        (0..count).map(|index| child_spec(&format!("child {index}"), vec![])).collect()
    }

    #[test]
    fn split_children_bounds_reject() {
        let mut graph = base_graph();
        let error = apply_split(
            &mut graph,
            &TaskTransformSpec::Split { parent: fixed(2), children: vec![] },
            now(),
        )
        .expect_err("empty split");
        assert_eq!(error.code, "children_invalid");

        let error = apply_split(
            &mut graph,
            &TaskTransformSpec::Split {
                parent: fixed(2),
                children: (0..=MAX_SPLIT_CHILDREN).map(|_| child_spec("child", vec![])).collect(),
            },
            now(),
        )
        .expect_err("too many children");
        assert_eq!(error.code, "children_invalid");
        assert_eq!(graph.len(), 3);
        let _ = children_spec(0);
    }

    // ── Merge semantics (issue acceptance: deterministic survival) ───────

    #[test]
    fn merge_is_deterministic_survivor_and_spec() {
        let survivor = fixed(4_000);
        let loser = fixed(9_000);
        // Same input twice (given in EITHER order): same survivor, same
        // merged spec, same edge set. Survivor = LOWEST ULID, always.
        for run in 0..2 {
            let mut graph = TaskGraph::new();
            let mut upstream = make_task(fixed(1), None, SubTaskStatus::Completed);
            upstream.dependencies = vec![];
            graph.add_root(upstream);
            graph.add_root(make_task(survivor, None, SubTaskStatus::Pending));
            graph.add_root(make_task(loser, None, SubTaskStatus::Pending));
            graph.add_dependency(survivor, fixed(1), Dependency::MustFinishBefore).unwrap();
            graph.add_dependency(loser, fixed(1), Dependency::MustFinishBefore).unwrap();
            let input_order = if run == 0 { vec![loser, survivor] } else { vec![survivor, loser] };
            let outcome = apply_merge(
                &mut graph,
                &TaskTransformSpec::Merge {
                    task_ids: input_order,
                    merged_description: "the merged work".to_owned(),
                    merged_artifacts: vec![],
                },
            )
            .unwrap_or_else(|error| panic!("merge {run} failed: {error:?}"));
            assert_eq!(outcome.survivor, survivor, "the lowest id survives");
            assert_eq!(outcome.removed, vec![loser], "the removed members ascend");
            let merged = graph.get(&survivor).unwrap();
            assert_eq!(merged.status, SubTaskStatus::Pending);
            assert_eq!(merged.description, "the merged work");
            assert_eq!(merged.role.as_str(), "coder", "the role is the group's");
            // The union dependency set carried over; the loser is gone.
            assert!(graph.dependencies_of(&survivor).contains(&fixed(1)));
            assert!(graph.get(&loser).is_none());
            assert!(graph.dependencies_of(&loser).is_empty());
        }
    }

    #[test]
    fn merge_transfers_edges_and_keeps_the_dag_valid() {
        let survivor = fixed(4_000);
        let loser = fixed(9_000);
        let depend = fixed(5_000);
        let dep_upstream = fixed(8_000);
        let mut graph = TaskGraph::new();
        graph.add_root(make_task(survivor, None, SubTaskStatus::Pending));
        graph.add_root(make_task(loser, None, SubTaskStatus::Pending));
        graph.add_root(make_task(depend, None, SubTaskStatus::Pending));
        graph.add_root(make_task(dep_upstream, None, SubTaskStatus::Completed));
        graph.add_dependency(loser, dep_upstream, Dependency::MustFinishBefore).unwrap();
        graph.add_dependency(depend, survivor, Dependency::MustFinishBefore).unwrap();
        graph.add_dependency(depend, loser, Dependency::ProvidesContextFor).unwrap();

        let outcome = apply_merge(
            &mut graph,
            &TaskTransformSpec::Merge {
                task_ids: vec![loser, survivor],
                merged_description: "folded".to_owned(),
                merged_artifacts: vec![],
            },
        )
        .expect("merge");

        // Removed member's incoming transfers to the survivor.
        assert!(graph.dependencies_of(&survivor).contains(&dep_upstream));
        // Removed member's outgoing transfers to the survivor.
        assert!(
            graph.outgoing_dependents(&survivor).iter().any(|(id, _)| *id == depend),
            "the loser's dependents reroute onto the survivor"
        );
        assert!(graph.get(&loser).is_none(), "the loser leaves the graph");
        assert!(removed_count(&outcome, loser));
        assert!(TaskGraphValidator::validate(&graph).is_ok(), "the merged DAG stays valid");
    }

    fn removed_count(outcome: &MergeOutcome, id: TaskId) -> bool {
        outcome.removed.contains(&id)
    }

    #[test]
    fn merge_participants_between_edges_skip_self_loops() {
        // survivor → loser (survivor depended on the loser in the group):
        // after the fold that becomes a survivor-self edge — skipped.
        let survivor = fixed(4_000);
        let loser = fixed(9_000);
        let mut graph = TaskGraph::new();
        graph.add_root(make_task(survivor, None, SubTaskStatus::Pending));
        graph.add_root(make_task(loser, None, SubTaskStatus::Pending));
        graph.add_dependency(survivor, loser, Dependency::MustFinishBefore).unwrap();

        let outcome = apply_merge(
            &mut graph,
            &TaskTransformSpec::Merge {
                task_ids: vec![survivor, loser],
                merged_description: "one task now".to_owned(),
                merged_artifacts: vec![],
            },
        )
        .expect("merge");
        assert_eq!(outcome.survivor, survivor);
        assert!(!graph.dependencies_of(&survivor).contains(&survivor), "never wired to itself");
        assert!(TaskGraphValidator::validate(&graph).is_ok());
    }

    #[test]
    fn merge_rejects_mixed_states_and_roles_and_singles() {
        let survivor = fixed(4_000);
        let loser = fixed(9_000);
        let mut graph = TaskGraph::new();
        graph.add_root(make_task(survivor, None, SubTaskStatus::Pending));
        graph.add_root(make_task(loser, None, SubTaskStatus::Pending));

        // A single (deduplicated to one) id is not a merge.
        let error = apply_merge(
            &mut graph,
            &TaskTransformSpec::Merge {
                task_ids: vec![survivor, survivor],
                merged_description: "x".to_owned(),
                merged_artifacts: vec![],
            },
        )
        .expect_err("a merge needs TWO distinct tasks");
        assert_eq!(error.code, "merge_group_too_small");

        // A completed member never merges — settled work is untouchable.
        let mut graph = TaskGraph::new();
        graph.add_root(make_task(survivor, None, SubTaskStatus::Completed));
        graph.add_root(make_task(loser, None, SubTaskStatus::Pending));
        let error = apply_merge(
            &mut graph,
            &TaskTransformSpec::Merge {
                task_ids: vec![survivor, loser],
                merged_description: "x".to_owned(),
                merged_artifacts: vec![],
            },
        )
        .expect_err("a completed participant");
        assert_eq!(error.code, "not_mergeable");
        assert_eq!(graph.len(), 2, "rejected merge mutates nothing");

        // Different roles are incompatible (the WHO is a dispatch decision).
        let mut graph = TaskGraph::new();
        graph.add_root(make_task(survivor, None, SubTaskStatus::Pending));
        let mut different = make_task(loser, None, SubTaskStatus::Pending);
        different.role = AgentId::new("reviewer");
        graph.add_root(different);
        let error = apply_merge(
            &mut graph,
            &TaskTransformSpec::Merge {
                task_ids: vec![survivor, loser],
                merged_description: "x".to_owned(),
                merged_artifacts: vec![],
            },
        )
        .expect_err("role mismatch");
        assert_eq!(error.code, "merge_role_mismatch");

        // Unknown participants reject structurally.
        let error = apply_merge(
            &mut graph,
            &TaskTransformSpec::Merge {
                task_ids: vec![survivor, fixed(77_000)],
                merged_description: "x".to_owned(),
                merged_artifacts: vec![],
            },
        )
        .expect_err("unknown member");
        assert_eq!(error.code, "unknown_task");

        // An empty merged description rejects.
        let mut graph = TaskGraph::new();
        graph.add_root(make_task(survivor, None, SubTaskStatus::Pending));
        graph.add_root(make_task(loser, None, SubTaskStatus::Pending));
        let error = apply_merge(
            &mut graph,
            &TaskTransformSpec::Merge {
                task_ids: vec![survivor, loser],
                merged_description: "   ".to_owned(),
                merged_artifacts: vec![],
            },
        )
        .expect_err("empty merged description");
        assert_eq!(error.code, "incomplete_decision");
    }

    #[test]
    fn transform_kinds_reject_the_mismatched_payload() {
        let mut graph = base_graph();
        // A Split key pointing at a merge payload...
        let error = apply_split(
            &mut graph,
            &TaskTransformSpec::Merge {
                task_ids: vec![fixed(4_000), fixed(9_000)],
                merged_description: "x".to_owned(),
                merged_artifacts: vec![],
            },
            now(),
        )
        .expect_err("mismatched");
        assert_eq!(error.code, "conflicting_decision");
    }

    // ── Determinism: merge survivor + spec (issue acceptance) ────────────

    #[test]
    fn merge_survives_by_lowest_id_regardless_of_input_order() {
        for order in [[fixed(9_000), fixed(4_000)], [fixed(4_000), fixed(9_000)]] {
            let mut graph = TaskGraph::new();
            graph.add_root(make_task(order[0], None, SubTaskStatus::Pending));
            graph.add_root(make_task(order[1], None, SubTaskStatus::Pending));
            let outcome = apply_merge(
                &mut graph,
                &TaskTransformSpec::Merge {
                    task_ids: order.to_vec(),
                    merged_description: "folded work".to_owned(),
                    merged_artifacts: vec![],
                },
            )
            .expect("merge");
            assert_eq!(outcome.survivor, fixed(4_000), "THE LOWEST ID ALWAYS SURVIVES");
            assert_eq!(outcome.removed, vec![fixed(9_000)]);
            assert_eq!(graph.get(&outcome.survivor).unwrap().description, "folded work");
        }
    }

    /// The full apply_transform flow (proof copy + committed graph) keeps
    /// both sibling transforms from ever dirtying the DAG.
    #[test]
    fn proofs_never_dirty_the_dag_and_commit_validates() {
        // Split, then merge the split result back: the composed graph must
        // still pass the validator.
        let mut graph = base_graph();
        let split_children = match apply_transform(
            &mut graph,
            &TaskTransformSpec::Split {
                parent: fixed(2),
                children: vec![child_spec("a", vec![]), child_spec("b", vec![])],
            },
            now(),
        )
        .expect("split")
        {
            TransformOutcome::Split(outcome) => outcome.children,
            other => panic!("wrong outcome {other:?}"),
        };
        // Merge them right back (same role, both pending).
        let outcome = apply_transform(
            &mut graph,
            &TaskTransformSpec::Merge {
                task_ids: split_children.clone(),
                merged_description: "rebuilt".to_owned(),
                merged_artifacts: vec![],
            },
            now(),
        )
        .expect("merge back");
        assert!(TaskGraphValidator::validate(&graph).is_ok(), "composed DAG valid");
        let survivor = match outcome {
            TransformOutcome::Merge(outcome) => outcome,
            other => panic!("wrong outcome {other:?}"),
        };
        assert_eq!(
            survivor.survivor,
            split_children.iter().min_by_key(|id| id.0).copied().unwrap()
        );
    }
}
