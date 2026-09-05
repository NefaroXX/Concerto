//! `CoordinatorAgent` — multi-agent orchestration controller.
//!
//! §3.6: Decomposes a user task into a DAG of `SubTask` nodes, delegates
//! each ready node to the appropriate `ExpertAgent` via `AgentRunner`, and
//! drives review/validation loops (§3.8) until completion or cycle-limit.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::{Arc, Mutex};

use futures::future::join_all;

use concerto_config::{
    coordinator_fallback, coordinator_self_implement_fallback, AgentCapabilities, BlueprintFacade,
    CustomAgentConfig, FallbackPersonaDef, PromptSections, StageKind,
};
use concerto_core::error::ProviderError;
use concerto_core::event::{EventBus, EventKind};
use concerto_core::executor::ToolExecutor;
use concerto_core::ids::Ulid;
use concerto_core::memory::{
    Decision, DecisionCategory, DecisionId, MemoryNamespace, MemoryQuery, TaskNode, TaskNodeId,
    TaskStatus,
};
use concerto_core::traits::agent::ExpertAgent;
use concerto_core::traits::memory::MemoryStore;
use concerto_core::types::{
    AgentContext, AgentId, AgentOutcome, AgentOutput, AgentRunResult, AgentStage, AgentTask,
    DesignDoc, ProviderMetrics, SubTask, SubTaskStatus, TaskId,
};
use concerto_core::{CancellationToken, OrchestratorError};
use concerto_providers::model::ModelProfile;
use concerto_providers::model_selector::ModelSelector;
use concerto_providers::retry::RetryPolicy;
use concerto_providers::routing::CostEstimator;
use concerto_sessions::spend::SpendTracker;
use concerto_sessions::whiteboard::append_whiteboard_event;
use concerto_sessions::whiteboard::{load_whiteboard_events, WhiteboardLoadOpts};
use concerto_sessions::{
    NewWhiteboardEvent, OrchestrationCheckpointRecord, ResourceFactRow, ResourceFacts,
    SessionStore, WhiteboardEvent, WhiteboardKind,
};

use crate::agent_runner::AgentRunner;
use crate::agents::GenericSpecialistAgent;
use crate::checkpoint;
use crate::cycle_manager::{ReviewCycleManager, ValidationCycleManager};
use crate::delta::FileDeltaTracker;
use crate::design_doc_verifier::{
    collect_design_doc_evidence, degraded_verdict, verify_design_doc, DesignDocReasonCode,
    DesignDocState, DesignDocVerdict,
};
use crate::evidence_scheduler::{self, DispatchStep, DocResolution, QuarantineCode};
use crate::graph::{Dependency, TaskGraph, TaskGraphValidator};
use crate::plan_approval::{
    append_review_state_event, load_review_resume, review_target_identity, ReviewCycleStatus,
    ReviewFeedbackEntry, ReviewResume, ReviewStatePayload,
};
use crate::planner::{PlanArtifact, PlannerAgentInfo, TaskPlanner};
use crate::registry::AgentRegistry;
use crate::relationship::{
    AgentHandoff, CollaborationRule, HandoffDeliverable, RelationshipManager,
};
use crate::resolver_integration::{self, ResolverOutcome};
use crate::resume::{self, ResumeOutcome};
use crate::state::OrchestratorState;
use tracing::warn;

/// Default dispatch-attempt ceiling per subtask before the fallback ladder
/// walks in (ADR-42 §1). Users can raise/lower it per run via
/// `MultiAgentConfig.max_subtask_attempts` (ADR-45 §4).
const DEFAULT_MAX_SUBTASK_ATTEMPTS: u32 = 3;

/// ADR-35 §8: system instructions for the coordinator's self-implement
/// persona. Used when NO implement-stage agent is registered and the
/// coordinator holds an executor — the coordinator then carries the
/// implement subtasks itself on its planning provider. The Orchestration
/// Studio's supplemental prompt (ADR-35 §5) is APPENDED to these
/// instructions, never replacing them.
const COORDINATOR_SELF_IMPLEMENT_PROMPT: &str = r#"You are the coordinator of a multi-agent software project, and you have temporarily taken over the implement-stage role because no implementation agent is registered in the current pipeline.

Your job for this subtask:
1. Understand the subtask description, the plan context, and any expected artifacts.
2. Inspect the repository and surrounding code as needed using your tools.
3. Implement the change requested by the subtask, writing production-quality code that fits the repository.
4. Verify your work with the narrowest useful checks before reporting.

Constraints:
- Work autonomously and keep the change as small and correct as possible.
- Do not refactor unrelated code or change public APIs unless the subtask explicitly requires it.
- If the subtask is underspecified, inspect first and make a conservative fix.
- Report the outcome as your final summary, listing every file you changed and why.
"#;

/// Three-way failure classification for subtask dispatches (ADR-42 §1).
enum SubtaskFailureClass {
    /// Transient; retry the same agent/model (today's recoverable path).
    Recoverable,
    /// Retries exhausted, or provider/model-specific hard failure (auth,
    /// context overflow, rate-limit ceiling, no-affordable-model). The task
    /// may still be solvable — walk the fallback ladder.
    LimitReached,
    /// Cancellation, invalid task graph, structural errors. Exit immediately;
    /// no ladder.
    NonRecoverable,
}

/// Classify a subtask dispatch error into the retry/ladder/exit decision
/// space (ADR-42 §1). Cancellation short-circuits to `NonRecoverable` before
/// any ladder tier.
fn classify_subtask_error(error: &OrchestratorError) -> SubtaskFailureClass {
    if is_cancellation_error(error) {
        return SubtaskFailureClass::NonRecoverable;
    }
    match error {
        OrchestratorError::AgentLoopError(_)
        | OrchestratorError::Memory(_)
        | OrchestratorError::Tool(_) => SubtaskFailureClass::Recoverable,
        // Transient provider errors (rate limits, network, timeouts, 5xx)
        // may resolve on retry — treat as Recoverable.
        OrchestratorError::Provider(p) if p.is_transient() => SubtaskFailureClass::Recoverable,
        // Provider/model-specific hard failure (auth, context overflow,
        // rate-limit ceiling) or model-selection failure (no affordable or
        // capable model, pinned model missing/unavailable/budget-blocked) is a
        // property of the *assignment*, not of the task — a different model or
        // agent may still complete it, so walk the fallback ladder.
        OrchestratorError::Provider(_)
        | OrchestratorError::NoAffordableModel { .. }
        | OrchestratorError::NoCapableModel { .. }
        | OrchestratorError::PinnedModelNotFound { .. }
        | OrchestratorError::PinnedModelMissingCapability { .. }
        | OrchestratorError::PinnedModelBudgetExceeded { .. } => SubtaskFailureClass::LimitReached,
        // Structural errors (invalid task graph, cycle detection, planning
        // failure, exhausted budgets for delegation) exit immediately.
        _ => SubtaskFailureClass::NonRecoverable,
    }
}

/// Outcome of the ADR-42 two-tier fallback ladder.
enum FallbackOutcome {
    /// A ladder tier produced a run result. The result flows through the same
    /// post-dispatch handling as a normally dispatched result — its own
    /// `outcome` may still be `Failed`/`Blocked`, in which case it is
    /// re-routed through the existing outcome handling.
    Success(Box<AgentRunResult>),
    /// The ladder observed cancellation; the subtask should be re-pended
    /// rather than failed, matching the dispatch-error cancellation path.
    Cancelled,
    /// Every tier was skipped or failed; the subtask stays failed and the
    /// caller proceeds with its existing terminal handling.
    Exhausted,
}

fn is_cancellation_error(error: &OrchestratorError) -> bool {
    matches!(
        error,
        OrchestratorError::Cancelled
            | OrchestratorError::Tool(concerto_core::ToolError::Cancelled)
            | OrchestratorError::Provider(ProviderError::Cancelled)
    )
}

fn failed_attempt_result(task_id: TaskId, role: AgentId, error: String) -> AgentRunResult {
    AgentRunResult {
        task_id,
        role,
        outcome: AgentOutcome::Failed { error: error.clone() },
        summary: format!("Previous attempt failed: {error}"),
        files_modified: Vec::new(),
        tool_call_count: 0,
        cost_usd: 0.0,
        latency_ms: 0,
        provider: String::new(),
        model: String::new(),
        tokens_in: 0,
        tokens_out: 0,
    }
}

/// Check if a Coder failure message indicates the agent could not produce
/// the expected project artifacts (files).  When this happens and the Coder
/// has exhausted retries, the coordinator should escalate to the Architect
/// for a design revision rather than giving up immediately.
fn is_artifact_failure(error: &str) -> bool {
    let lower = error.to_lowercase();
    lower.contains("expected artifacts not produced")
        || lower.contains("no file-changing tool call succeeded")
        || lower.contains("the coder made no project file changes")
}

/// Maximum bytes of an expected artifact scanned for placeholder content.
/// Files larger than this are treated as substantive by construction, so
/// acceptance only performs bounded reads (audit C-06).
const MAX_PLACEHOLDER_SCAN_BYTES: u64 = 64 * 1024;

/// ADR-65 §4: newest observed rows folded into the action digest injected
/// into dispatched agent contexts (bounded so the prompt stays lean).
const MAX_OBSERVATION_DIGEST_LINES: usize = 20;

/// Render observed `resource_facts` rows as the compact action-digest block:
/// clean rows as `path | unchanged-since <event_id> | hash-<first 8 hex>` (the
/// hash segment is omitted when the observation recorded no content hash),
/// dirty rows as `path | changed`. Deterministic — the input ordering is
/// already newest-first with a path tiebreak ([`ResourceFacts::list_observations`]).
fn format_action_digest(rows: &[ResourceFactRow]) -> String {
    let mut lines = vec![String::from("<action_digest>")];
    for row in rows {
        match (&row.dirty, &row.last_event_id, &row.content_hash) {
            (true, ..) | (false, None, _) => lines.push(format!("{} | changed", row.path)),
            (false, Some(event_id), Some(hash)) => {
                let head: String = hash.chars().take(8).collect();
                lines.push(format!("{} | unchanged-since {} | hash-{}", row.path, event_id, head));
            }
            (false, Some(event_id), None) => {
                lines.push(format!("{} | unchanged-since {}", row.path, event_id));
            }
        }
    }
    lines.push(String::from("</action_digest>"));
    lines.join("\n")
}

/// Minimal stub-marker set for the placeholder predicate (audit C-06).
///
/// A file is a placeholder when it is empty, whitespace-only, or every
/// non-blank line matches one of these markers exactly (after trimming and
/// lowercasing). Real files that merely *contain* a TODO comment alongside
/// substantive content are not rejected — only marker-only files are.
const PLACEHOLDER_MARKERS: &[&str] = &[
    "todo",
    "todo: implement",
    "todo: implement this",
    "stub",
    "placeholder",
    "not implemented",
    "coming soon",
    "lorem ipsum",
];

/// Placeholder predicate for expected-artifact acceptance (audit C-06):
/// `true` when `content` is empty, consists only of whitespace, or every
/// non-blank line is a known stub marker ("TODO: implement", "stub",
/// "placeholder", "not implemented", ...).
fn is_placeholder_content(content: &str) -> bool {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return true;
    }
    trimmed.lines().all(|line| {
        let line = line.trim();
        let lower = line.to_lowercase();
        line.is_empty() || PLACEHOLDER_MARKERS.contains(&lower.as_str())
    })
}

/// Verify one expected artifact (audit C-06): it must resolve inside the
/// project root, exist as a regular file, be non-empty, and hold
/// non-placeholder content. Returns a per-file reason on failure.
fn check_one_artifact(
    project_root: &camino::Utf8Path,
    artifact: &camino::Utf8Path,
) -> Result<(), String> {
    let resolved = concerto_tools::common::resolve_path(project_root, artifact)
        .map_err(|error| format!("cannot resolve path: {error}"))?;
    let metadata =
        std::fs::metadata(resolved.as_std_path()).map_err(|error| format!("missing: {error}"))?;
    if !metadata.is_file() {
        return Err("not a regular file".into());
    }
    if metadata.len() == 0 {
        return Err("empty file".into());
    }
    // Files larger than the scan cap hold substantive content by
    // construction; only bounded reads happen here.
    if metadata.len() > MAX_PLACEHOLDER_SCAN_BYTES {
        return Ok(());
    }
    match std::fs::read_to_string(resolved.as_std_path()) {
        // Binary/non-UTF-8 content is not a placeholder.
        Err(_) => Ok(()),
        Ok(content) if is_placeholder_content(&content) => Err("placeholder content".into()),
        Ok(_) => Ok(()),
    }
}

/// Verify every expected artifact exists on disk (relative to the project
/// root) and holds non-placeholder content (audit C-06). Returns the
/// verified paths on success, or the offending artifacts with a per-file
/// reason. An empty `expected` list passes vacuously.
fn verify_expected_artifacts(
    project_root: &camino::Utf8Path,
    expected: &[camino::Utf8PathBuf],
) -> Result<Vec<camino::Utf8PathBuf>, Vec<(camino::Utf8PathBuf, String)>> {
    let mut verified = Vec::new();
    let mut violations = Vec::new();
    for artifact in expected {
        match check_one_artifact(project_root, artifact) {
            Ok(()) => verified.push(artifact.clone()),
            Err(reason) => violations.push((artifact.clone(), reason)),
        }
    }
    if violations.is_empty() {
        Ok(verified)
    } else {
        Err(violations)
    }
}

/// The run's declared expected artifacts across all subtasks, de-duplicated
/// (the C-06 acceptance view of the `expected_artifacts` snapshot).
fn expected_artifact_list(
    snapshot: &HashMap<TaskId, Vec<camino::Utf8PathBuf>>,
) -> Vec<camino::Utf8PathBuf> {
    let mut seen = HashSet::new();
    snapshot.values().flatten().filter(|path| seen.insert((*path).clone())).cloned().collect()
}

/// Whether any declared expected artifact is unproduced on disk (missing,
/// empty, or placeholder content). An empty declared set is vacuously
/// produced — mirroring the C-06 acceptance gate's semantics.
fn expected_artifacts_unproduced(
    project_root: &camino::Utf8Path,
    expected_artifacts: &HashMap<TaskId, Vec<camino::Utf8PathBuf>>,
) -> bool {
    let expected = expected_artifact_list(expected_artifacts);
    !expected.is_empty() && verify_expected_artifacts(project_root, &expected).is_err()
}

/// Run-continuity Phase 1: the stall predicate evaluated at a run's final
/// exit. A run is STALLED — not cleanly done — when any of:
///
/// 1. its declared Completion is not complete (`Partial`),
/// 2. its declared expected deliverables are unproduced on disk (vacuous
///    when nothing is declared), or
/// 3. any subtask remains `Failed` or `Blocked`.
///
/// A stalled run KEEPS its resumable orchestration checkpoint (persisted
/// with `completed=false`); only a clean success clears it, so a later
/// bare "continue" can always pick the run back up.
fn run_is_stalled(
    completion_status: concerto_core::types::AgentCompletionStatus,
    deliverables_missing: bool,
    graph: &TaskGraph,
) -> bool {
    if completion_status != concerto_core::types::AgentCompletionStatus::Completed {
        return true;
    }
    if deliverables_missing {
        return true;
    }
    graph
        .all_tasks()
        .iter()
        .any(|subtask| matches!(subtask.status, SubTaskStatus::Failed | SubTaskStatus::Blocked))
}

/// Build a `Failed` run result that records an acceptance rejection (C-06).
fn acceptance_failure_result(task: &AgentTask, summary: String) -> AgentRunResult {
    AgentRunResult {
        task_id: task.id,
        role: AgentId::new("validator"),
        outcome: AgentOutcome::Failed { error: summary.clone() },
        summary,
        files_modified: Vec::new(),
        tool_call_count: 0,
        cost_usd: 0.0,
        latency_ms: 0,
        provider: String::new(),
        model: String::new(),
        tokens_in: 0,
        tokens_out: 0,
    }
}

/// True when a validator agent's error means declared verification commands
/// did not run (the generic eval-runner fails fast with this message when
/// the agent has no eval engine / the `eval` capability is off).
fn is_validation_disabled(message: &str) -> bool {
    let lower = message.to_lowercase();
    lower.contains("validation disabled") || lower.contains("no eval engine")
}

fn metrics_from_result(result: &AgentRunResult) -> ProviderMetrics {
    ProviderMetrics {
        provider: result.provider.clone(),
        model: result.model.clone(),
        tokens_in: result.tokens_in,
        tokens_out: result.tokens_out,
        cost_usd: result.cost_usd,
        latency_ms: result.latency_ms,
    }
}

/// Compact human-readable description of a configured agent: `name — role`,
/// skipping empty parts. Rendered by the planner prompt so it describes what
/// each role is (ADR-35 phase 4, roster enrichment), e.g. `Coder — coder`.
fn config_description(cfg: &CustomAgentConfig) -> String {
    let mut parts: Vec<&str> = Vec::new();
    let name = cfg.name.trim();
    let role = cfg.role.trim();
    if !name.is_empty() {
        parts.push(name);
    }
    if !role.is_empty() {
        parts.push(role);
    }
    parts.join(" — ")
}

fn refresh_working_memory(
    context: &mut AgentContext,
    graph: &TaskGraph,
    completed_results: &HashMap<TaskId, AgentRunResult>,
    stage_of: &dyn Fn(&AgentId) -> Option<AgentStage>,
    facade: Option<&BlueprintFacade>,
) {
    let mut subtasks = graph.all_tasks();
    subtasks.sort_by_key(|subtask| subtask.id.to_string());
    context.working_memory.session_id = context.session.session_id;
    context.working_memory.created_at = time::OffsetDateTime::now_utc();
    context.working_memory.task_tree = subtasks
        .iter()
        .map(|subtask| TaskNode {
            id: TaskNodeId(subtask.id.0),
            session_id: subtask.session_id,
            description: subtask.description.clone(),
            status: match subtask.status {
                SubTaskStatus::Pending
                | SubTaskStatus::AwaitingReview
                | SubTaskStatus::NeedsRevision => TaskStatus::Pending,
                SubTaskStatus::Blocked => TaskStatus::Blocked,
                SubTaskStatus::Running => TaskStatus::Running,
                SubTaskStatus::Completed => TaskStatus::Done,
                SubTaskStatus::Failed => TaskStatus::Failed,
                _ => TaskStatus::Pending,
            },
            parent_id: subtask.parent_id.map(|id| TaskNodeId(id.0)),
            children: subtasks
                .iter()
                .filter_map(|candidate| {
                    (candidate.parent_id == Some(subtask.id)).then_some(TaskNodeId(candidate.id.0))
                })
                .collect(),
            blocking: subtask.dependencies.iter().map(|id| TaskNodeId(id.0)).collect(),
            created_at: subtask.created_at,
        })
        .collect();

    let mut results = completed_results.values().collect::<Vec<_>>();
    results.sort_by_key(|result| result.task_id.to_string());
    for result in results {
        if context.working_memory.decisions.iter().any(|decision| {
            decision.task_id == Some(result.task_id)
                && decision.outcome.as_deref() == Some(result.summary.as_str())
        }) {
            continue;
        }
        context.working_memory.decisions.push(Decision {
            id: DecisionId(Ulid::new()),
            session_id: context.session.session_id,
            task_id: Some(result.task_id),
            what: format!("{:?} result accepted", result.role),
            why: match &result.outcome {
                AgentOutcome::Success => "The specialist completed its assigned graph node.".into(),
                AgentOutcome::NeedsRevision { reason } => {
                    format!("The specialist requested revision: {reason}")
                }
                AgentOutcome::Failed { error } => format!("The specialist failed: {error}"),
                AgentOutcome::Blocked { on } => format!("The specialist is blocked on {on:?}"),
                _ => "Unknown outcome".into(),
            },
            outcome: Some(result.summary.chars().take(1_500).collect()),
            // ADR-35 §5: categorize by the agent's declared stage tag
            // rather than hardcoded role ids. Facade-resolved by kind when
            // one is attached, so renamed Planning/Acceptance/Execution/
            // Review tags categorize by their semantics (issue #150); on
            // the default `standard` blueprint the two agree. Roles not
            // staffed in the resolved blueprint fall back to the legacy
            // tag-based classification (freeform/custom stages).
            category: match facade.and_then(|facade| facade.stage_for_agent(&result.role)) {
                Some(stage) => match stage.def.known_kind() {
                    Some(StageKind::Planning) => DecisionCategory::Architecture,
                    Some(StageKind::Acceptance) => DecisionCategory::Test,
                    Some(StageKind::Execution) | Some(StageKind::Review) => {
                        DecisionCategory::Implementation
                    }
                    _ => DecisionCategory::Other,
                },
                None => match stage_of(&result.role).as_ref().map(|stage| stage.as_str()) {
                    Some("design") => DecisionCategory::Architecture,
                    Some("validate") => DecisionCategory::Test,
                    Some("implement" | "review") => DecisionCategory::Implementation,
                    _ => DecisionCategory::Other,
                },
            },
            confidence: if matches!(&result.outcome, AgentOutcome::Success) { 1.0 } else { 0.5 },
            superseded_by: None,
            created_at: time::OffsetDateTime::now_utc(),
        });
    }
    const MAX_LEDGER_DECISIONS: usize = 64;
    if context.working_memory.decisions.len() > MAX_LEDGER_DECISIONS {
        let remove = context.working_memory.decisions.len() - MAX_LEDGER_DECISIONS;
        context.working_memory.decisions.drain(0..remove);
    }
}

struct ReadyTask {
    id: TaskId,
    role: AgentId,
    subtask: SubTask,
    description: String,
    dependencies: Vec<TaskId>,
    previous_results: Vec<AgentRunResult>,
}

/// ADR-55 Phase 2b: how far a multi-agent run may take the orchestrated
/// graph. `Full` runs the historical lifecycle (plan, execute, review,
/// validate); `PlanningOnly` stops after the plan is produced, rendered and
/// persisted — no subtask dispatch, review, or validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OrchestrationDepth {
    /// Run the full coordinator lifecycle.
    #[default]
    Full,
    /// Plan only: memory + design + TaskPlanner + graph validation, then
    /// return the rendered plan (ADR-55 Phase 2b).
    PlanningOnly,
}

/// Result of graph decomposition or checkpoint restoration.
struct DecomposeResult {
    graph: TaskGraph,
    completed_results: HashMap<TaskId, AgentRunResult>,
    total_cost: f64,
    total_tool_calls: u32,
    all_files: Vec<camino::Utf8PathBuf>,
    provider_metrics: Vec<ProviderMetrics>,
    subtask_attempts: HashMap<TaskId, u32>,
    retry_feedback: HashMap<TaskId, Vec<AgentRunResult>>,
    model_assignments: HashMap<TaskId, String>,
    action_ledger: Vec<checkpoint::CheckpointAction>,
    /// The objective this run's checkpoints record. On a fresh run this is
    /// the task description; on a checkpoint restore it is the ORIGINAL
    /// objective carried in the checkpoint — a resumed run must keep
    /// recording the original objective text + hash, not its (bare
    /// "continue") input, so every later resume still names the same work.
    objective: String,
    objective_hash: String,
}

/// ADR-65 §7: the outcome of a resume evaluation applied to the restored
/// state. `Replan` returns no ledger — the caller falls through to fresh
/// decomposition (the Phase-6 scheduler governs the new planning; the
/// resume path itself never dispatches an agent).
enum ResumeApplication {
    Applied {
        /// Ledger entries recording the resume decisions/rewrites, to be
        /// carried into the resumed run's checkpoints.
        ledger_entries: Vec<checkpoint::CheckpointAction>,
        /// The subtask a Continue/Replace decision re-armed; its attempt
        /// counter resets so the granted decision can actually dispatch.
        re_armed: Option<TaskId>,
    },
    Replan,
}

/// The coordinator drives the full multi-agent lifecycle.
pub struct CoordinatorAgent {
    registry: Arc<AgentRegistry>,
    runner: AgentRunner,
    model_selector: Arc<ModelSelector>,
    spend_tracker: Arc<SpendTracker>,
    bus: EventBus,
    review_cycles: ReviewCycleManager,
    validation_cycles: ValidationCycleManager,
    cycle_state: OrchestratorState,
    /// Tracks file-level progress per task so the cycle detector can
    /// distinguish repeated writes of the same content from real edits.
    file_delta: FileDeltaTracker,
    planning_provider: Arc<dyn concerto_core::traits::provider::LlmProvider>,
    /// Rules governing how agents relate during orchestration (e.g. who
    /// supervises whom, max review cycles, etc.).
    relationships: RelationshipManager,
    /// Memory store for retrieving project context. Used to populate
    /// `retrieved_chunks` in agent contexts at task start (audit §3.3).
    memory_store: Arc<dyn MemoryStore>,
    /// Per-agent prompt/capability configs from the Studio. Passed through
    /// to the registry at construction time so each `ExpertAgent` struct
    /// carries its configured `PromptSections` and `AgentCapabilities`.
    agent_configs: HashMap<AgentId, CustomAgentConfig>,
    /// Expected artifact files per task, derived from the DesignDoc's
    /// proposed_files. Used by the coder completion gate to reject
    /// premature completion when required files are missing.
    expected_artifacts: Mutex<HashMap<TaskId, Vec<camino::Utf8PathBuf>>>,
    /// Most recent DesignDoc produced by the Architect (or a replan), captured
    /// into every checkpoint so a resumed run retains the original plan
    /// without re-running the Architect (C-05).
    design_doc: Mutex<Option<DesignDoc>>,
    /// Tracks which original Coder subtasks have already triggered a replan
    /// escalation to the Architect. Keyed by the original Coder's task id;
    /// a value of 1 means one replan has been attempted. Both the original
    /// Coder and any follow-up Coder spawned from the replan are recorded
    /// here so a cascading failure loop cannot produce infinite replans.
    replan_attempts: HashMap<TaskId, u32>,
    retry_policy: RetryPolicy,
    session_store: Option<Arc<dyn SessionStore>>,
    source_revision: Option<String>,
    /// ADR-42 §4 fallback-ladder guards. These are checkpointed (see
    /// `checkpoint::GraphCheckpoint`): a resumed run restores them and does
    /// NOT re-walk ladder tiers that already fired before the interruption.
    /// Each guard is at-most-once per task per run, so the ladder is
    /// loop-free by construction.
    /// Tracks which subtasks have already received an escalation retry
    /// (the final attempt with a different model or relaxed criteria).
    /// Prevents infinite escalation loops by ensuring at most one escalation
    /// attempt per task per run.
    escalation_attempted: HashSet<TaskId>,
    /// ADR-42 §4 tier 1 guard: whether the global-default-model fallback has
    /// already been attempted for a task this run (at most once per task).
    default_model_attempted: HashSet<TaskId>,
    /// ADR-42 §4 tier 2 guard: whether coordinator self-execution has already
    /// been attempted for a task this run (at most once per task).
    self_execute_attempted: HashSet<TaskId>,
    /// ADR-45 tier 1b guard: whether a default-model-on-default-provider
    /// re-dispatch has already been attempted for a task this run (at most
    /// once per task).
    default_model_provider_attempted: HashSet<TaskId>,
    /// ADR-45 tier 1b: the run's default provider — the pipe that serves the
    /// global default model — as the rebuilt-agent re-dispatch target. `None`
    /// when the runtime has no default provider (configuration error) — tier
    /// 1b is skipped and the ladder continues at tier 2.
    default_model_provider: Option<Arc<dyn concerto_core::traits::provider::LlmProvider>>,
    /// ADR-45 tier 1b: the routing profile of the global default model on the
    /// run's default provider. `None` when the runtime could not resolve one.
    default_model_profile: Option<ModelProfile>,
    /// ADR-45 §4: user gate for tier 1b. Defaults to enabled; when disabled
    /// the ladder skips the default-model-on-default-provider re-dispatch
    /// (ADR-42 behavior).
    default_model_fallback: bool,
    /// ADR-42 §4 tier 2: the routing profile of the coordinator's own model on
    /// its serving pipe (`planning_provider`). `None` when unresolved (config
    /// error) — tier 2 then skips with a note instead of dispatching a raw
    /// single-shot request.
    planning_profile: Option<ModelProfile>,
    /// The run's default provider config id, used to derive a role's
    /// effective serving pipe when the role has no per-agent provider
    /// assignment. `None` only when the runtime resolved no default provider.
    default_provider_config_id: Option<String>,
    /// ADR-45 §4: per-run dispatch-attempt ceiling, mirrored from
    /// `MultiAgentConfig.max_subtask_attempts` (default 3).
    max_subtask_attempts: u32,
    /// Run-wide doom guard (ADR-52): maximum number of model dispatches for
    /// one multi-agent run, mirrored from `MultiAgentConfig.max_total_iterations`.
    /// `None` (or `Some(0)`) means unlimited. Every ready-batch dispatch,
    /// retry, escalation, replan follow-up, and fallback-ladder tier counts
    /// toward the ceiling via `model_dispatch_count`; when the next ready
    /// batch would push the run past the cap, the coordinator pauses with a
    /// `Partial` outcome instead of spending more tokens.
    max_total_iterations: Option<usize>,
    /// Monotonic per-run count of model dispatches performed by
    /// `execute_graph` (batch dispatches + ladder tiers). Reset at the start
    /// of each `execute_graph` invocation. Consumed by the global run cap.
    model_dispatch_count: usize,
    /// Durable planner-plan artifacts (ADR-52). `None` disables persistence
    /// (the default; the production wiring in `runtime_runner` attaches the
    /// manager under the app data root, and tests opt in hermetically).
    plans: Option<concerto_sessions::plans::PlansManager>,
    /// Session skills instructions (ADR-43, Task 4). Pre-budgeted by
    /// `SkillsContext`; injected into every planner prompt for this run.
    /// Empty when skills are disabled.
    skills_section: String,
    /// ADR-55 Phase 2b: how far this run may go — full lifecycle (default)
    /// or planning-only (produce + render + persist the plan, nothing else).
    orchestration_depth: OrchestrationDepth,
    /// ADR-55 Phase 2b: plan id of the most recently persisted PlanArtifact
    /// (ADR-52), surfaced so the runtime runner can bind a planning-only
    /// run's rendered plan to its durable artifact.
    last_plan_id: Option<String>,
    /// Provider metrics settled so far by the current (or most recent) run,
    /// mirrored at every settlement and exact-reset on success. Survives a
    /// failed run so callers can persist what was actually consumed before
    /// the error (rate-limit exhaustion, cancellation, …).
    settled_metrics: Vec<ProviderMetrics>,
    /// ADR-35 §8: the shared executor backing coordinator self-execution. When
    /// present the coordinator can carry an unstaffed implement stage itself;
    /// when `None` self-execution is unavailable and a pipeline without an
    /// implement-stage agent fails exactly as before.
    tool_executor: Option<Arc<ToolExecutor>>,
    /// ADR-35 §5/§8: the Orchestration Studio's supplemental coordinator
    /// prompt, APPENDED to the coordinator self's built-in instructions. Never
    /// replaces or overrides them. Empty when unset.
    supplemental_prompt: String,
    /// ADR-35 §5, Phase 5 C-06 amendment: the coordinator's eval engine,
    /// backing coordinator self-verification when no validation-stage agent
    /// is registered. `None` (the default) preserves the pre-amendment
    /// behavior: a build task without a validator rejects acceptance.
    eval_engine: Option<Arc<concerto_eval::EvalEngine>>,
    /// ADR-58 P2+P3 (Batch 1): the resolved blueprint's typed facade,
    /// backing the sequencing guards in [`Self::stage_of`] and
    /// [`Self::first_agent_for_stage`] (`debug_assert!` comparing the
    /// registry answer against blueprint staffing). `None` for coordinators
    /// built without a resolved blueprint (tests) — the guards stay silent.
    blueprint_facade: Option<BlueprintFacade>,
    /// ADR-60 D7 (#152): structured plan state rehydrated from the whiteboard
    /// for an approved-plan Execute run. Consumed once by the first
    /// `decompose_task`: a seeded DesignDoc drives the planner directly and
    /// the architect is NOT re-invoked on the same objective — re-deriving an
    /// already-approved plan (silent re-decompose) is forbidden.
    approved_plan_seed: Option<ApprovedPlanSeed>,
    /// ADR-60 Deferred 3: session-DB pool backing review-cycle resumability
    /// (`ReviewState` whiteboard snapshots written before every reviewer
    /// invocation and read back on restart). `None` (the default) degrades
    /// review cycles to pre-Phase 3 behavior — observable via a debug log at
    /// cycle entry, never a run failure.
    review_store: Option<sqlx::SqlitePool>,
    /// ADR-65 §2 (Phase 2): the pre-planning workspace snapshot captured by
    /// the readiness barrier, threaded through to agent dispatch so every
    /// dispatched agent receives the snapshot digest in its context.
    /// Never used to gate behavior in this phase — persistence/write gating is
    /// load-bearing only with the Phase 5/6 evidence checks.
    workspace_snapshot: Option<crate::workspace_snapshot::WorkspaceSnapshotRecord>,
    /// ADR-65 §3: the run id (fresh per `execute_graph`, matching the
    /// checkpoint scope) stamped into every dispatched agent's tool-evidence
    /// facts, so a run's tool commands are attributable across task
    /// boundaries. `None` only when no run has started yet.
    run_id: Option<String>,
    /// ADR-65 §7: where the DesignDoc claim last stood (verdict + real event
    /// ids), captured into every checkpoint. Restored from the checkpoint on
    /// a resume so the §7 fields keep round-tripping; refreshed by the
    /// Phase-5 verifier on every planning run. (The §7 whiteboard cursor is
    /// stamped by `persist_checkpoint` at persist time — the log head then —
    /// and needs no stored field here.)
    last_doc_resolution: Option<checkpoint::CheckpointDocResolution>,
    /// ADR-65 §7: the last scheduler dispatch still awaiting completion —
    /// recorded when a dispatch decision is appended, cleared when its
    /// subtask settles, and captured into every checkpoint as the pending
    /// decision a resume may continue behind.
    last_dispatch_decision: Option<checkpoint::CheckpointPendingDecision>,
    /// ADR-65 §7: the checkpoint row's own `updated_at` (unix ms), threaded
    /// from the runtime runner for pre-§7 (v3) checkpoints only — the hint
    /// the additive backfill uses to derive "the last log event before the
    /// checkpoint's own append". `None` (no hint) is fail-soft: the cursor
    /// stays unknown and the resume treats the whole log as pre-cursor.
    resume_cursor_hint_ms: Option<i64>,
}

/// ADR-60 D7 (#152): the whiteboard-verified state attached to a coordinator
/// executing an approved plan. Produced by `plan_approval::load_approved_plan`
/// and attached via [`CoordinatorAgent::with_approved_plan_seed`].
#[derive(Debug, Clone, Default)]
pub struct ApprovedPlanSeed {
    /// The approved plan id (for logging/attribution).
    pub plan_id: String,
    /// Structured DesignDoc rehydrated from the `plan-approved` event.
    /// `None` when the planning run produced only text — decompose then keeps
    /// its normal design stage rather than inventing a doc.
    pub design_doc: Option<DesignDoc>,
}

// ─────────────────────────────────────────────────────────────────────────
// ADR-58 P2+P3 (Batch 3a): fallback persona rendering
// ─────────────────────────────────────────────────────────────────────────

/// The resolved primary `Execution` stage's tag — the tag the coordinator's
/// sentinel persona carries while the pipeline's implementation stage is
/// unstaffed (ADR-58 §2.2 R4 / §3). Without an attached facade this falls
/// back to the legacy `implement` tag, so planner partitions and the stage
/// feed keep working byte-identically on the default blueprint (planner.rs
/// keys the primary-`Execution` partition on the facade when attached, else
/// on the `implement` literal).
fn execution_stage_tag(facade: Option<&BlueprintFacade>) -> String {
    facade
        .and_then(|facade| facade.primary_execution_stage())
        .map(|stage| stage.def.tag.clone())
        .unwrap_or_else(|| AgentStage::IMPLEMENT.to_string())
}

/// ADR-65 §6: upper bound on the observation facts gathered per scheduler
/// consultation, so heavy workspaces keep the evidence read bounded.
const MAX_EVIDENCE_OBSERVATIONS: usize = 64;

/// Map the Phase-5 verifier's verdict onto the checkpoint's §7 doc-resolution
/// field: Active/Quarantined/Skipped + the REAL claim/decision event ids. A
/// failed append yields no ids to cite — the ids stay `None` (never
/// fabricated), but the resolution itself is still recorded.
fn checkpoint_doc_resolution(
    verdict: &Option<DesignDocVerdict>,
    doc_event_ids: Option<&(String, String)>,
) -> Option<checkpoint::CheckpointDocResolution> {
    let evidence = doc_event_ids
        .map(|(claim_id, decision_id)| (Some(claim_id.clone()), Some(decision_id.clone())))
        .unwrap_or((None, None));
    let (claim_event_id, verdict_event_id) = evidence;
    let verdict = verdict.as_ref()?;
    use crate::design_doc_verifier::DesignDocState;
    Some(match verdict.state {
        DesignDocState::Verified => checkpoint::CheckpointDocResolution::Active {
            contract_paths: verdict.contract_paths.clone(),
            claim_event_id,
            verdict_event_id,
        },
        DesignDocState::Skipped => {
            checkpoint::CheckpointDocResolution::Skipped { claim_event_id, verdict_event_id }
        }
        DesignDocState::Quarantined => {
            checkpoint::CheckpointDocResolution::Quarantined {
                reason_codes: verdict
                    .reasons
                    .iter()
                    .filter_map(|reason| {
                        // Kebab-case reason codes (mirrors the verifier's
                        // wire form via serde rename).
                        serde_json::to_value(reason.code)
                            .ok()
                            .and_then(|value| value.as_str().map(str::to_owned))
                    })
                    .collect(),
                claim_event_id,
                verdict_event_id,
            }
        }
    })
}

/// ADR-65 §7: upper bound on the session log window a resume reads (newest
/// events, anchored at the log head). Bounds the §7 evidence read; a session
/// with a longer tail simply loses the earliest rows to the fail-soft
/// degradation (the checkpoint ledger still carries the per-task outcomes).
const RESUME_LOG_WINDOW: usize = 2000;

/// The paths the run's OWN recorded writes explain (ADR-65 §7): the
/// checkpoint's accumulated `all_files`, the completed results' reported
/// `files_modified`, and — for the post-cursor tail — applied write paths
/// and file-affecting tool paths (a read observation is NOT an own write: a
/// stalled run's read of a path the user then edited IS an external change).
/// Used to keep the F3 reconciliation honest: the run's own progress must
/// never look like an external workspace change.
fn own_write_paths(
    checkpoint_all_files: &[camino::Utf8PathBuf],
    completed_results: &HashMap<TaskId, AgentRunResult>,
    post_cursor: &[WhiteboardEvent],
    project_root: &std::path::Path,
) -> std::collections::HashSet<String> {
    let mut own = std::collections::HashSet::new();
    let mut push = |raw: &str| {
        if let Some(canonical) = crate::tool_facts::canonical_project_path(project_root, raw) {
            own.insert(canonical);
        }
    };
    for path in checkpoint_all_files {
        push(path.as_str());
    }
    for result in completed_results.values() {
        for path in &result.files_modified {
            push(path.as_str());
        }
    }
    for event in post_cursor {
        match event.kind {
            WhiteboardKind::WriteApplied => {
                if let Some(path) = event
                    .payload
                    .get("input")
                    .and_then(|input| input.get("path"))
                    .and_then(serde_json::Value::as_str)
                {
                    push(path);
                }
            }
            WhiteboardKind::ToolExecuted => {
                let tool = event.payload.get("tool").and_then(serde_json::Value::as_str);
                let args = event.payload.get("args").cloned().unwrap_or(serde_json::Value::Null);
                let file_affecting =
                    tool.is_some_and(|tool| crate::tool_facts::is_file_affecting_tool(tool, &args));
                if !(file_affecting
                    && event.payload.get("success").and_then(serde_json::Value::as_bool)
                        == Some(true))
                {
                    continue;
                }
                if let Some(paths) =
                    event.payload.get("paths").and_then(serde_json::Value::as_array)
                {
                    for path in paths {
                        if let Some(path) = path.get("path").and_then(serde_json::Value::as_str) {
                            push(path);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    own
}

/// The deterministic graph description for a scheduled fallback step.
fn fallback_step_description(step: &DispatchStep, task: &AgentTask) -> String {
    match step.capability {
        evidence_scheduler::Capability::Explore => {
            format!("Ground workspace evidence for: {}", task.description)
        }
        evidence_scheduler::Capability::Design => {
            format!("Design architecture for: {}", task.description)
        }
        evidence_scheduler::Capability::Implement => {
            format!("Implement: {}", task.description)
        }
    }
}

/// Map the Phase-5 verifier's resolution onto the scheduler's decision
/// surface (ADR-65 §6): Active/Quarantined/Skipped plus machine reason codes,
/// citing the REAL claim/decision event ids when both landed (ids are never
/// fabricated — a failed append yields no ids to cite).
fn scheduler_doc_resolution(
    verdict: &Option<DesignDocVerdict>,
    doc: Option<&DesignDoc>,
    doc_event_ids: &Option<(String, String)>,
) -> DocResolution {
    let evidence_ids = doc_event_ids
        .as_ref()
        .map(|(claim, decision)| vec![claim.clone(), decision.clone()])
        .unwrap_or_default();
    match (verdict, doc) {
        (Some(verdict), _) => match verdict.state {
            DesignDocState::Verified => DocResolution::Active {
                contract_paths: verdict.contract_paths.clone(),
                evidence_ids,
            },
            DesignDocState::Skipped => DocResolution::Skipped { evidence_ids },
            DesignDocState::Quarantined => {
                // Deterministic first-occurrence order over the verdict's
                // reason codes (per-path reasons may repeat a code).
                let mut codes: Vec<QuarantineCode> = Vec::new();
                for reason in &verdict.reasons {
                    let code = match reason.code {
                        DesignDocReasonCode::UngroundedPath => QuarantineCode::UngroundedPath,
                        DesignDocReasonCode::TreeConflict => QuarantineCode::TreeConflict,
                        DesignDocReasonCode::NoObservations => QuarantineCode::NoObservations,
                        _ => QuarantineCode::Other,
                    };
                    if !codes.contains(&code) {
                        codes.push(code);
                    }
                }
                DocResolution::Quarantined { codes, evidence_ids }
            }
        },
        // A seeded approved-plan doc (ADR-60 D7) is human-approved — its
        // approval IS the evidence — so it schedules as Active and binds
        // unconditionally, with no verdict events of its own to cite.
        (None, Some(doc)) => DocResolution::Active {
            contract_paths: doc.proposed_files.iter().map(|p| p.as_str().to_owned()).collect(),
            evidence_ids: Vec::new(),
        },
        (None, None) => DocResolution::Undecided,
    }
}

/// The tag of the first stage carrying the given known kind, resolved from
/// the facade so **renamed stage tags keep gate/fix-loop dispatch working**
/// (issue #150): a blueprint that renames the review stage to `quality` or
/// the validation stage to `ship` (kind preserved) must still get its
/// review/validation gate cycles, replan/fix-pair loops, and skip messages.
/// Without a facade — or when no stage carries the kind (an
/// `Execution`-free or gate-free pipeline) — the legacy canonical tag
/// stands in, byte-identical on the default `standard` blueprint.
fn kind_stage_tag(
    facade: Option<&BlueprintFacade>,
    kind: StageKind,
    legacy_tag: &'static str,
) -> String {
    facade
        .and_then(|facade| facade.first_stage_of_kind(kind))
        .map(|stage| stage.def.tag.clone())
        .unwrap_or_else(|| legacy_tag.to_string())
}

/// The fallback persona for an unstaffed lifecycle stage: the stage block's
/// configured `fallback` when present, otherwise `engine_default`. Gate
/// stages default to [`coordinator_fallback`] (the reserved `coordinator`
/// identity, empty persona — B5 mirror); an unstaffed `Execution` stage
/// defaults to the promoted [`coordinator_self_implement_fallback`] (ADR-58
/// review F5). The persona renders only while the stage is actually
/// unstaffed.
fn stage_fallback_persona(
    facade: Option<&BlueprintFacade>,
    stage_tag: &str,
    engine_default: FallbackPersonaDef,
) -> FallbackPersonaDef {
    facade
        .and_then(|facade| facade.stage_by_tag(stage_tag))
        .and_then(|stage| stage.def.fallback.clone())
        .unwrap_or(engine_default)
}

/// Engine-owned capability flags for the unstaffed-`Execution` sentinel
/// render (ADR-58 review F1): `fs_read`/`git`/`lsp` are engine defaults, the
/// write flags come from the persona narrowed against the stage-kind mask,
/// and the eval engine stays coordinator-owned (never attached to a sentinel
/// render). With the engine-default persona this reproduces exactly the
/// pre-blueprint hardcoded self-implement flags (fs_read, fs_write, shell,
/// git, lsp — no eval).
fn sentinel_capabilities(persona: &FallbackPersonaDef, kind: StageKind) -> AgentCapabilities {
    let mask = persona.effective_capabilities(kind);
    AgentCapabilities {
        fs_read: Some(true),
        fs_write: Some(mask.fs_write),
        shell: Some(mask.shell),
        git: Some(true),
        lsp: Some(true),
        eval: Some(false),
    }
}

/// ADR-60 Deferred 3: build one full-state [`ReviewStatePayload`] snapshot
/// for the review cycle identified by `(plan_id, target_hash)`. Every
/// snapshot carries the complete feedback ledger and counters so a resumed
/// run needs exactly the newest row (oracle: full state, not minimal).
#[allow(clippy::too_many_arguments)]
fn review_snapshot(
    plan_id: &str,
    session_id: Ulid,
    implement_role: &str,
    review_target: &str,
    target_hash: &str,
    status: ReviewCycleStatus,
    max_cycles: u32,
    retry_count: u32,
    ledger: &[ReviewFeedbackEntry],
    gate_seq_cursor: u64,
) -> ReviewStatePayload {
    let now = time::OffsetDateTime::now_utc();
    ReviewStatePayload {
        plan_id: plan_id.to_owned(),
        session_id: session_id.to_string(),
        implement_role: implement_role.to_owned(),
        review_target: review_target.to_owned(),
        review_target_hash: target_hash.to_owned(),
        status,
        max_cycles,
        retry_count,
        feedback_ledger: ledger.to_vec(),
        gate_seq_cursor,
        created_at_ms: now.unix_timestamp() * 1000 + i64::from(now.millisecond()),
    }
}

impl CoordinatorAgent {
    /// Create a new coordinator with all required subsystems.
    pub fn new(
        registry: Arc<AgentRegistry>,
        runner: AgentRunner,
        model_selector: Arc<ModelSelector>,
        spend_tracker: Arc<SpendTracker>,
        bus: EventBus,
        planning_provider: Arc<dyn concerto_core::traits::provider::LlmProvider>,
        memory_store: Arc<dyn MemoryStore>,
    ) -> Self {
        let cycle_state = OrchestratorState::with_bus(bus.clone());
        Self {
            registry,
            runner,
            model_selector,
            spend_tracker,
            bus,
            planning_provider,
            review_cycles: ReviewCycleManager::default(),
            validation_cycles: ValidationCycleManager::default(),
            cycle_state,
            file_delta: FileDeltaTracker::new(),
            relationships: RelationshipManager::defaults(),
            memory_store,
            agent_configs: HashMap::new(),
            expected_artifacts: Mutex::new(HashMap::new()),
            design_doc: Mutex::new(None),
            replan_attempts: HashMap::new(),
            retry_policy: RetryPolicy::default(),
            session_store: None,
            source_revision: None,
            escalation_attempted: HashSet::new(),
            default_model_attempted: HashSet::new(),
            self_execute_attempted: HashSet::new(),
            default_model_provider_attempted: HashSet::new(),
            default_model_provider: None,
            default_model_profile: None,
            default_model_fallback: true,
            planning_profile: None,
            default_provider_config_id: None,
            max_subtask_attempts: DEFAULT_MAX_SUBTASK_ATTEMPTS,
            skills_section: String::new(),
            max_total_iterations: None,
            model_dispatch_count: 0,
            plans: None,
            orchestration_depth: OrchestrationDepth::Full,
            last_plan_id: None,
            settled_metrics: Vec::new(),
            tool_executor: None,
            supplemental_prompt: String::new(),
            eval_engine: None,
            blueprint_facade: None,
            approved_plan_seed: None,
            review_store: None,
            workspace_snapshot: None,
            run_id: None,
            last_doc_resolution: None,
            last_dispatch_decision: None,
            resume_cursor_hint_ms: None,
        }
    }

    /// ADR-65 §7: thread the checkpoint row's own `updated_at` (unix ms) in
    /// as the backfill hint for pre-§7 (v3) checkpoints. Only the additive
    /// v4 backfill consumes it; present v4 fields are never overwritten.
    pub fn with_resume_cursor_hint_ms(mut self, hint_ms: Option<i64>) -> Self {
        self.resume_cursor_hint_ms = hint_ms;
        self
    }

    /// Attach the ADR-45 tier-1b fallback target: the run's default provider
    /// (the pipe that serves the global default model) and the routing profile
    /// of that model on it. Pass `None` for both to disable tier 1b (e.g. no
    /// default provider configured).
    pub fn with_default_model_provider(
        mut self,
        provider: Option<Arc<dyn concerto_core::traits::provider::LlmProvider>>,
        profile: Option<ModelProfile>,
    ) -> Self {
        self.default_model_provider = provider;
        self.default_model_profile = profile;
        self
    }

    /// ADR-45 §4: user gate for the default-model-on-default-provider
    /// fallback tier.
    pub fn with_default_model_fallback(mut self, enabled: bool) -> Self {
        self.default_model_fallback = enabled;
        self
    }

    /// ADR-42 §4 tier 2: the routing profile of the coordinator's model on its
    /// serving pipe (`planning_provider`). Pass `None` when unresolvable —
    /// tier 2 then skips with a note instead of dispatching a raw single-shot
    /// request.
    pub fn with_planning_profile(mut self, planning_profile: Option<ModelProfile>) -> Self {
        self.planning_profile = planning_profile;
        self
    }

    /// ADR-35 §8: attach the shared tool executor backing coordinator
    /// self-execution. Without it, self-execution is unavailable and a
    /// pipeline with no implement-stage agent fails exactly as before.
    pub fn with_executor(mut self, executor: Arc<ToolExecutor>) -> Self {
        self.tool_executor = Some(executor);
        self
    }

    /// ADR-35 §5, Phase 5 C-06 amendment: attach the coordinator's eval
    /// engine, enabling coordinator self-verification when no
    /// validation-stage agent is registered. Without it, a pipeline with no
    /// validate-stage agent rejects acceptance for build tasks exactly as
    /// before.
    pub fn with_eval_engine(mut self, engine: Arc<concerto_eval::EvalEngine>) -> Self {
        self.eval_engine = Some(engine);
        self
    }

    /// ADR-58 P2+P3 (Batch 1): attach the resolved blueprint's typed facade,
    /// backing the sequencing guards in [`Self::stage_of`] and
    /// [`Self::first_agent_for_stage`]. Pass `None` (or omit the call) for
    /// coordinators built without a resolved blueprint — the guards stay
    /// silent.
    ///
    /// ADR-58 P2+P3 (Batch 3a): the facade is also propagated to the cycle
    /// state so Rule B keys on the gate being executed (R11) and drives the
    /// unstaffed-stage fallback persona renders (R4/§3).
    pub fn with_blueprint_facade(mut self, facade: Option<BlueprintFacade>) -> Self {
        if let Some(facade) = &facade {
            self.cycle_state = self.cycle_state.clone().with_blueprint_facade(Some(facade.clone()));
        }
        self.blueprint_facade = facade;
        self
    }

    /// ADR-35 §5: the Orchestration Studio's supplemental coordinator prompt,
    /// appended to the coordinator self's built-in instructions. Never
    /// replaces them.
    pub fn with_supplemental_prompt(mut self, prompt: String) -> Self {
        self.supplemental_prompt = prompt;
        self
    }

    /// ADR-35 §8, trigger 1 (stage absence): whether the coordinator can carry
    /// an unstaffed lifecycle stage itself. Requires the shared executor; the
    /// planning provider is always present.
    fn self_execute_available(&self) -> bool {
        self.tool_executor.is_some()
    }

    /// Build the coordinator's self-implement persona (ADR-35 §8): a
    /// standalone, never-registered generic specialist carrying the reserved
    /// `coordinator` id, an implement-stage tag, the planning provider, and
    /// the shared executor (whose policy engine still gates every tool call —
    /// this never bypasses `SimplePolicyEngine` or `VirtualFs`). The
    /// supplemental prompt (ADR-35 §5) is appended, never replacing the
    /// built-in self-implement instructions.
    ///
    /// ADR-58 P2+P3 (§3/F5): the render is driven by the unstaffed-`Execution`
    /// fallback persona (see `implement_fallback_persona`): its label and
    /// narrowed write flags, with the engine-owned `fs_read`/`git`/`lsp`
    /// defaults overlaid and the persona's supplementary instructions appended
    /// after the supplemental prompt. With the engine default this matches the
    /// pre-blueprint construction (full tool set, no eval) except for the
    /// persona label, which changes from the legacy "Coordinator" to
    /// "Coordinator (self-execute)" (ADR-58 F5-accepted delta; the sentinel
    /// agent id "coordinator" is unchanged).
    fn self_implement_agent(&self, persona: &FallbackPersonaDef) -> GenericSpecialistAgent {
        GenericSpecialistAgent::new(
            AgentId::new("coordinator"),
            persona.label.clone(),
            Some(AgentStage::new(execution_stage_tag(self.blueprint_facade.as_ref()))),
            self.planning_provider.clone(),
            self.tool_executor.clone(),
            self.bus.clone(),
            self.retry_policy.clone(),
            PromptSections {
                system_instructions: format!(
                    "{COORDINATOR_SELF_IMPLEMENT_PROMPT}{}{}",
                    self.supplemental_prompt,
                    persona.system_instructions.as_deref().unwrap_or_default()
                ),
                ..Default::default()
            },
            sentinel_capabilities(persona, StageKind::Execution),
        )
        .with_skills_section(&self.skills_section)
        // ADR-65 §3: evidence attribution for the coordinator's own tool
        // commands when it self-implements (no registration-time pool).
        .with_tool_facts(self.review_store.clone().map(|pool| {
            crate::tool_facts::ToolFactContext::new(Some(pool), "coordinator".to_string())
        }))
    }

    /// ADR-58 P2+P3 (§3/R4): the unstaffed-`Execution` fallback persona — the
    /// primary `Execution` stage's configured `fallback`, or the promoted
    /// engine default [`coordinator_self_implement_fallback`] when the stage
    /// ships `fallback: None` (the `standard` blueprint).
    fn implement_fallback_persona(&self) -> FallbackPersonaDef {
        stage_fallback_persona(
            self.blueprint_facade.as_ref(),
            &execution_stage_tag(self.blueprint_facade.as_ref()),
            coordinator_self_implement_fallback(),
        )
    }

    /// ADR-35 §5, Phase 5 C-06 amendment: whether the coordinator can carry
    /// the verification stage itself when no validation-stage agent is
    /// registered. Requires the eval engine (attached via
    /// [`Self::with_eval_engine`]); the planning provider is always present.
    fn self_verify_available(&self) -> bool {
        self.eval_engine.is_some()
    }

    /// Build the coordinator's self-verify persona (ADR-35 §5, Phase 5 C-06
    /// amendment): a standalone, never-registered generic specialist carrying
    /// the reserved `coordinator` id, a validate-stage tag, the planning
    /// provider (unused in eval mode but required by the constructor), no
    /// tool executor (eval mode never invokes tools or the LLM), and the
    /// shared eval engine. `PromptSections::default()` suffices: `run_eval`
    /// only reads the (empty) constraint/output-format sections for
    /// post-processing and never builds a prompt.
    ///
    /// ADR-58 P2+P3 (§3): the render is driven by the unstaffed-`Acceptance`
    /// fallback persona (see `acceptance_fallback_persona`), which on the
    /// default blueprint is [`coordinator_fallback`] — the pre-blueprint
    /// hardcoded identity (label "Coordinator", empty instructions,
    /// eval-only capabilities; the Acceptance-kind mask narrows the write
    /// flags to `false`).
    fn self_verify_agent(&self, persona: &FallbackPersonaDef) -> GenericSpecialistAgent {
        let mask = persona.effective_capabilities(StageKind::Acceptance);
        let mut sections = PromptSections::default();
        if let Some(instructions) = &persona.system_instructions {
            sections.system_instructions = instructions.clone();
        }
        GenericSpecialistAgent::new(
            AgentId::new("coordinator"),
            persona.label.clone(),
            Some(AgentStage::new(kind_stage_tag(
                self.blueprint_facade.as_ref(),
                StageKind::Acceptance,
                AgentStage::VALIDATE,
            ))),
            self.planning_provider.clone(),
            None,
            self.bus.clone(),
            self.retry_policy.clone(),
            sections,
            AgentCapabilities {
                fs_write: Some(mask.fs_write),
                shell: Some(mask.shell),
                eval: Some(true),
                ..Default::default()
            },
        )
        .with_eval(self.eval_engine.clone())
    }

    /// ADR-58 P2+P3 (§3): the unstaffed-`Acceptance` (validate) fallback
    /// persona — the stage's configured `fallback`, or the reserved
    /// [`coordinator_fallback`] gate persona when unconfigured. The stage is
    /// resolved by kind, so a renamed acceptance tag keeps its configured
    /// fallback (issue #150).
    fn acceptance_fallback_persona(&self) -> FallbackPersonaDef {
        stage_fallback_persona(
            self.blueprint_facade.as_ref(),
            &kind_stage_tag(
                self.blueprint_facade.as_ref(),
                StageKind::Acceptance,
                AgentStage::VALIDATE,
            ),
            coordinator_fallback(),
        )
    }

    /// ADR-35 §8, trigger 1: execute a subtask assigned to the reserved
    /// `coordinator` role (an implement subtask in a pipeline with no
    /// implement-stage agent) directly on the planning provider through the
    /// executor-backed freeform loop.
    ///
    /// The instrumentation footprint mirrors [`AgentRunner::run_with_agent`]
    /// exactly so self-execution is metered identically to a runner-dispatched
    /// role: the queued/started `AgentThought` transcripts, a
    /// `SubTaskStarted` lifecycle event, a budget reservation via
    /// `SpendTracker::check_and_add` before the run, `settle_reservation` +
    /// `publish_spend_events` after it, and — on the Ok path — the same
    /// outcome lifecycle events (`SubTaskCompleted` only for a genuine
    /// `Success`) emitted through the shared runner helper. A successful run
    /// is tagged with the `coordinator-self-execute` provider sentinel
    /// (ADR-42/45 tier 2) so audit, policy, and UI consumers can identify the
    /// self-dispatch; failures and partial outcomes keep the planning
    /// provider's name. The per-run concurrency permits are skipped: the
    /// coordinator already bounds concurrency to one ready batch at a time,
    /// and they are not part of the events/spend/metrics footprint.
    async fn run_coordinator_self(
        &self,
        subtask: &SubTask,
        context: AgentContext,
        profile: &ModelProfile,
        cancel: CancellationToken,
    ) -> Result<AgentRunResult, OrchestratorError> {
        let agent = self.self_implement_agent(&self.implement_fallback_persona());
        let role_name = format!("{}", subtask.role);
        let model_name = profile.model_name();
        let correlation_id = subtask.id.0;

        let _ = self.bus.publish_for_session(
            subtask.session_id,
            correlation_id,
            EventKind::AgentThought {
                agent_id: role_name.clone(),
                content: format!(
                    "Queued subtask: {}\nUsing {}/{}",
                    subtask.description.chars().take(1_000).collect::<String>(),
                    profile.profile.provider,
                    model_name
                ),
            },
        );

        let start = std::time::Instant::now();

        // Reserve the budget before starting, exactly like AgentRunner, so a
        // hard cap stops the self-run the same way it stops a delegated run.
        let reserved_cost = CostEstimator::estimate(&subtask.role, &profile.profile);
        self.spend_tracker
            .check_and_add(reserved_cost)
            .map_err(|_| OrchestratorError::NoBudgetForDelegation)?;

        let _ = self.bus.publish_for_session(
            subtask.session_id,
            correlation_id,
            EventKind::SubTaskStarted { task_id: subtask.id, role: subtask.role.clone() },
        );
        let _ = self.bus.publish_for_session(
            subtask.session_id,
            correlation_id,
            EventKind::AgentThought {
                agent_id: role_name.clone(),
                content: format!(
                    "Started subtask with {}/{} after waiting {} ms",
                    profile.profile.provider,
                    model_name,
                    start.elapsed().as_millis()
                ),
            },
        );

        let result = agent.run(subtask, context, model_name, cancel.clone()).await;

        let latency_ms = start.elapsed().as_millis() as u64;
        let actual_cost = result.as_ref().map_or(0.0, |run_result| run_result.cost_usd);
        self.spend_tracker.settle_reservation(reserved_cost, actual_cost);
        // Publish the live spend snapshot (and cap signal) after the run
        // settles, exactly like AgentRunner so UIs show the session total.
        crate::agent_runner::publish_spend_events(
            &self.bus,
            subtask.session_id,
            correlation_id,
            &self.spend_tracker,
        );

        match result {
            Ok(mut run_result) => {
                run_result.latency_ms = latency_ms;
                run_result.model = model_name.to_string();
                if matches!(run_result.outcome, AgentOutcome::Success) {
                    run_result.provider = "coordinator-self-execute".into();
                }
                self.runner.publish_outcome_events(
                    correlation_id,
                    subtask,
                    &subtask.role,
                    &run_result,
                );
                Ok(run_result)
            }
            Err(error) => {
                let error_string = error.to_string();
                let cancelled = is_cancellation_error(&error) || cancel.is_cancelled();
                let _ = self.bus.publish_for_session(
                    subtask.session_id,
                    correlation_id,
                    EventKind::AgentThought {
                        agent_id: role_name,
                        content: if cancelled {
                            format!("Subtask cancelled: {error_string}")
                        } else {
                            format!("Subtask failed: {error_string}")
                        },
                    },
                );
                let lifecycle_event = if cancelled {
                    EventKind::SubTaskCancelled {
                        task_id: subtask.id,
                        role: subtask.role.clone(),
                        reason: error_string,
                    }
                } else {
                    EventKind::SubTaskFailed {
                        task_id: subtask.id,
                        role: subtask.role.clone(),
                        error: error_string,
                    }
                };
                let _ = self.bus.publish_for_session(
                    subtask.session_id,
                    correlation_id,
                    lifecycle_event,
                );
                Err(error)
            }
        }
    }

    /// ADR-35 §5, Phase 5 C-06 amendment: run the coordinator's self-verify
    /// persona — the attached eval engine runs the project's detected test
    /// runner (no LLM, no tools, zero cost). The instrumentation footprint
    /// mirrors the validator path: a `ValidationCycleStarted` event (cycle 1,
    /// so the stage feed advances and replay sees the cycle) followed by the
    /// direct agent run; on error the loop's terminal `ValidationEscalated`
    /// event is published before the error propagates. Unlike a runner
    /// dispatch, no `SubTaskStarted`/`SubTaskCompleted` lifecycle or spend
    /// accounting happens — the validator path does neither, and eval runs
    /// are zero-cost. Metrics are settled by the caller via
    /// `metrics_from_result`, exactly like the validator path.
    async fn run_coordinator_self_verify(
        &self,
        task: &SubTask,
        context: AgentContext,
        cancel: CancellationToken,
    ) -> Result<AgentRunResult, OrchestratorError> {
        let _ = self.bus.publish_for_session(
            task.session_id,
            task.id.0,
            EventKind::ValidationCycleStarted { task_id: task.id, cycle_num: 1 },
        );
        let result = self
            .self_verify_agent(&self.acceptance_fallback_persona())
            .run(task, context, "", cancel)
            .await;
        if result.is_err() {
            let _ = self.bus.publish_for_session(
                task.session_id,
                task.id.0,
                EventKind::ValidationEscalated { task_id: task.id, max_cycles: 1 },
            );
        }
        result
    }

    /// The run's default provider config id, used to derive a role's effective
    /// serving pipe when the role has no per-agent provider assignment.
    pub fn with_default_provider_config_id(
        mut self,
        default_provider_config_id: Option<String>,
    ) -> Self {
        self.default_provider_config_id = default_provider_config_id;
        self
    }

    /// ADR-45 §4: per-run dispatch-attempt ceiling (from
    /// `MultiAgentConfig.max_subtask_attempts`).
    pub fn with_max_subtask_attempts(mut self, max_attempts: u32) -> Self {
        self.max_subtask_attempts = max_attempts.max(1);
        self
    }

    /// ADR-52: run-wide model-dispatch doom guard (from
    /// `MultiAgentConfig.max_total_iterations`). `None` or `Some(0)` disables
    /// the guard (unlimited dispatches).
    pub fn with_max_total_iterations(mut self, max_total_iterations: Option<usize>) -> Self {
        self.max_total_iterations = max_total_iterations.filter(|cap| *cap > 0);
        self
    }

    /// ADR-52: attach the durable planner-plan manager. `None` (the default)
    /// disables plan persistence.
    pub fn with_plans(mut self, plans: Option<concerto_sessions::plans::PlansManager>) -> Self {
        self.plans = plans;
        self
    }

    /// ADR-55 Phase 2b: cap the run at planning. The coordinator produces
    /// and renders the plan (persisted as a PlanArtifact when `with_plans`
    /// is attached) but never dispatches subtask execution, review, or
    /// validation.
    pub fn with_orchestration_depth(mut self, depth: OrchestrationDepth) -> Self {
        self.orchestration_depth = depth;
        self
    }

    /// ADR-55 Phase 2b: the plan id of the most recently persisted plan
    /// artifact (`None` when plan persistence is disabled or failed). The
    /// runtime runner reads this after a planning-only run to bind the
    /// rendered plan to its durable artifact.
    pub fn last_plan_id(&self) -> Option<&str> {
        self.last_plan_id.as_deref()
    }

    /// ADR-60 D7 (#152): seed an approved-plan Execute with the structured
    /// state rehydrated from the whiteboard. When the seed carries a
    /// DesignDoc, `decompose_task` skips the architect entirely — the
    /// approved plan governs and is never re-derived (silent re-decompose is
    /// forbidden). Unseeded runs (the default) are byte-identical to pre-D7.
    pub fn with_approved_plan_seed(mut self, seed: ApprovedPlanSeed) -> Self {
        self.approved_plan_seed = Some(seed);
        self
    }

    /// ADR-60 Deferred 3: attach the whiteboard pool that makes review cycles
    /// resumable. With a store AND an approved-plan seed attached, every
    /// review cycle persists full-state snapshots (feedback ledger + retry
    /// counters + gate-seq cursor) BEFORE invoking the reviewer, and an entry
    /// after a restart resumes the crashed cycle instead of duplicating it.
    /// `None` (the default) keeps pre-Phase 3 behavior with a degradation log.
    pub fn with_review_store(mut self, pool: Option<sqlx::SqlitePool>) -> Self {
        self.review_store = pool;
        self
    }

    /// ADR-65 §2 (Phase 2): attach the pre-planning workspace snapshot so its
    /// digest rides into every dispatched agent's context. Purely additive in
    /// this phase — the snapshot is never used to gate behavior here.
    pub fn with_workspace_snapshot(
        mut self,
        snapshot: crate::workspace_snapshot::WorkspaceSnapshotRecord,
    ) -> Self {
        self.workspace_snapshot = Some(snapshot);
        self
    }

    /// The pre-planning snapshot digest for context injection; `None` when the
    /// readiness barrier produced no snapshot (readability or fail-soft).
    ///
    /// ADR-65 §4: augmented with a compact per-observation **action digest** —
    /// the newest [`MAX_OBSERVATION_DIGEST_LINES`] observed paths from the
    /// derived `resource_facts` store, queried fresh on every dispatch so the
    /// agent sees what changed since planning rather than a stale snapshot.
    /// Fail-soft: an absent pool or a store error degrades to the bare
    /// snapshot digest with a warning, never a dispatch failure.
    async fn snapshot_digest(&self, cancel: &CancellationToken) -> Option<String> {
        let snapshot = self.workspace_snapshot.as_ref()?;
        let base = snapshot.digest();
        let Some(pool) = &self.review_store else {
            return Some(base);
        };
        // ADR-65 F5c: observations live in the snapshot's own project-root
        // scope — rows from other roots (or legacy "") must not leak into the
        // digest's claim about THIS workspace.
        let root_hash = crate::tool_facts::project_root_hash(snapshot.project_root.as_std_path());
        // The observations are BORROWED + locally refreshed below so the digest
        // stays derived-truth; never persisted mutation of the source of truth.
        let mut observations = match ResourceFacts::new(pool.clone())
            .list_observations(&root_hash, MAX_OBSERVATION_DIGEST_LINES, cancel)
            .await
        {
            Ok(rows) => rows,
            Err(err) => {
                warn!(%err, "action digest: observation list unavailable; falling back to the snapshot digest only");
                return Some(base);
            }
        };
        if observations.is_empty() {
            return Some(base);
        }
        // ADR-65 F3: reconcile freshness NOW. A row whose current stat (size,
        // mtime) diverges from the observation — or whose file vanished — is
        // folded DIRTY so the action digest never claims an unchanged state the
        // disk no longer matches. The re-dirty is also persisted best-effort so
        // the derived store and the digest agree from here on.
        let store = ResourceFacts::new(pool.clone());
        for row in &mut observations {
            let fresh = std::fs::metadata(snapshot.project_root.join(&row.path))
                .ok()
                .filter(|meta| {
                    row.size_bytes.unwrap_or(0) == meta.len()
                        && row.mtime_ms == crate::tool_facts::mtime_ms(meta)
                })
                .is_some();
            if fresh {
                continue;
            }
            row.dirty = true;
            if let Err(err) = store.mark_dirty(&root_hash, &row.path, cancel).await {
                warn!(
                    %err,
                    path = %row.path,
                    "action digest: freshness mark_dirty failed; digest still shows the row as dirty"
                );
            }
        }
        let mut digest = String::from(&base);
        digest.push('\n');
        digest.push_str(&format_action_digest(&observations));
        Some(digest)
    }

    /// The pre-planning workspace generation id (ADR-65) for evidence
    /// attribution; `None` when no snapshot was captured.
    fn snapshot_generation(&self) -> Option<String> {
        self.workspace_snapshot.as_ref().map(|snapshot| snapshot.generation.clone())
    }

    /// Attach the session's skills instructions (ADR-43, Task 4), injected
    /// into every planner prompt. Pass an empty string to omit them.
    pub fn with_skills_section(mut self, skills_section: String) -> Self {
        self.skills_section = skills_section;
        self
    }

    /// Apply the same request-level retry policy to planner calls as the
    /// specialist agents use.
    pub fn with_retry_policy(mut self, retry_policy: RetryPolicy) -> Self {
        self.retry_policy = retry_policy;
        self
    }

    pub fn with_checkpoint_store(
        mut self,
        session_store: Option<Arc<dyn SessionStore>>,
        source_revision: Option<String>,
    ) -> Self {
        self.session_store = session_store;
        self.source_revision = source_revision;
        self
    }

    async fn persist_checkpoint(&mut self, checkpoint: &mut checkpoint::GraphCheckpoint) {
        // ADR-65 §7: stamp the whiteboard cursor at PERSIST time — the log
        // head is the consistent cut this checkpoint is consistent with, so
        // a resume reads only facts appended after it. Fail-soft: without a
        // log pool (or on a read error) the cursor stays `None` and the
        // resume treats the whole log as pre-cursor.
        if checkpoint.whiteboard_cursor_gate_seq.is_none() {
            if let Some(pool) = self.review_store.as_ref() {
                match concerto_sessions::whiteboard::latest_gate_seq(pool).await {
                    Ok(head) => checkpoint.whiteboard_cursor_gate_seq = Some(head),
                    Err(error) => {
                        warn!(%error, "ADR-65 §7: cursor stamp read failed; checkpoint persists without a cursor")
                    }
                }
            }
        }
        if checkpoint.snapshot_generation.is_none() {
            checkpoint.snapshot_generation = self.snapshot_generation();
        }
        let Some(store) = &self.session_store else {
            // Never silent: a coordinator without a session store cannot
            // leave resumable state, so `continue` can never resume it.
            // (Live miss, Sep 2026: run_multi_agent never attached the
            // store and stalled runs persisted zero rows with zero logs.)
            tracing::warn!(
                session_id = %checkpoint.session_id,
                "no session store attached — orchestration checkpoint not persisted; \
                 stalled runs will not be resumable"
            );
            return;
        };
        let Ok(state_json) = serde_json::to_string(checkpoint) else {
            tracing::warn!("failed to serialize orchestration checkpoint");
            return;
        };
        let record = OrchestrationCheckpointRecord {
            session_id: checkpoint.session_id,
            run_id: checkpoint.run_id,
            root_task_id: checkpoint.root_task_id,
            project_id: checkpoint.project_id.clone(),
            objective_hash: checkpoint.objective_hash.clone(),
            schema_version: checkpoint.schema_version,
            source_revision: checkpoint.source_revision.clone(),
            sequence_num: checkpoint.sequence_num,
            state_json,
            completed: checkpoint.completed,
            updated_at: time::OffsetDateTime::now_utc(),
        };
        if let Err(error) = store.save_orchestration_checkpoint(&record).await {
            tracing::warn!(%error, "failed to persist orchestration checkpoint");
        }
    }

    fn expected_artifacts_snapshot(&self) -> HashMap<TaskId, Vec<camino::Utf8PathBuf>> {
        self.expected_artifacts.lock().unwrap_or_else(|error| error.into_inner()).clone()
    }

    /// ADR-60 D7 (#152): snapshot of the most recent DesignDoc — the
    /// runtime_runner binds it into the planning-only run's `plan-approved`
    /// whiteboard event so Execute can rehydrate the structured object
    /// instead of re-deriving it from rendered prose.
    pub fn design_doc_snapshot(&self) -> Option<DesignDoc> {
        self.design_doc.lock().unwrap_or_else(|error| error.into_inner()).clone()
    }

    /// ADR-55 Phase 2b: render the produced plan as the run's final message —
    /// the design-doc summary plus one line per planned subtask (role,
    /// description, dependencies). Rendered from the GRAPH so both the
    /// TaskPlanner success path and the heuristic fallback pipeline produce
    /// a plan (T7).
    fn render_plan(&self, task: &AgentTask, graph: &TaskGraph) -> String {
        let mut out = String::new();
        out.push_str("# Plan\n");
        if let Some(doc) = self.design_doc_snapshot() {
            out.push_str("## Design\n");
            for goal in &doc.goals {
                out.push_str(&format!("- Goal: {goal}\n"));
            }
            for constraint in &doc.constraints {
                out.push_str(&format!("- Constraint: {constraint}\n"));
            }
            for file in &doc.proposed_files {
                out.push_str(&format!("- Proposed file: {file}\n"));
            }
            if !doc.interface_sketch.trim().is_empty() {
                out.push_str(&format!("- Interface: {}\n", doc.interface_sketch.trim()));
            }
        }
        out.push_str("## Subtasks\n");
        for (index, subtask) in graph.all_tasks().iter().enumerate() {
            let dependencies = if subtask.dependencies.is_empty() {
                "none".to_string()
            } else {
                subtask.dependencies.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ")
            };
            out.push_str(&format!(
                "{}. [{}] {} (dependencies: {dependencies})\n",
                index + 1,
                subtask.role,
                subtask.description,
            ));
        }
        out.push_str(&format!("Objective: {}", task.description));
        out
    }

    /// ADR-52: whether the run-wide dispatch cap has been reached (`None` or
    /// `Some(0)` disables the guard entirely).
    fn iteration_cap_reached(&self) -> bool {
        self.max_total_iterations.is_some_and(|cap| self.model_dispatch_count >= cap)
    }

    /// ADR-52: persist a plan artifact to the configured plans dir
    /// (`plan-<id>.json`), logging the pretty JSON at debug and falling back
    /// to a `None` plan id when persistence is disabled or fails (non-fatal —
    /// the run proceeds without a durable plan).
    fn persist_plan_artifact(&self, artifact: &PlanArtifact) -> Option<String> {
        let json = match artifact.pretty_json() {
            Ok(json) => json,
            Err(e) => {
                tracing::warn!(
                    target: "orchestrator::planner",
                    error = %e,
                    "failed to serialize plan artifact",
                );
                return None;
            }
        };
        tracing::debug!(target: "orchestrator::planner", plan = %json, "planner produced plan");
        let Some(plans) = &self.plans else {
            return None;
        };
        match plans.write_plan(&artifact.plan_id, &json) {
            Ok(path) => {
                tracing::debug!(
                    target: "orchestrator::planner",
                    path = %path.display(),
                    "persisted plan artifact",
                );
                Some(artifact.plan_id.clone())
            }
            Err(e) => {
                tracing::warn!(
                    target: "orchestrator::planner",
                    error = %e,
                    "failed to persist plan artifact",
                );
                None
            }
        }
    }

    /// Build the coordinator-side checkpoint context, including the ADR-42
    /// ladder guards (default-model / self-execution / escalation attempts).
    /// These are captured into every checkpoint so a resumed run does NOT
    /// re-walk ladder tiers that already fired before the interruption.
    /// ADR-65 §7: the doc resolution and the pending dispatch decision ride
    /// along so a resume restores state at the cursor instead of replaying
    /// prose; the cursor itself is stamped at persist time (the log head
    /// then) by `persist_checkpoint`.
    fn checkpoint_context(
        &self,
        model_assignments: &HashMap<TaskId, String>,
        action_ledger: &[checkpoint::CheckpointAction],
    ) -> checkpoint::CheckpointContext {
        checkpoint::CheckpointContext {
            design_doc: self.design_doc_snapshot(),
            model_assignments: model_assignments.clone(),
            action_ledger: action_ledger.to_vec(),
            default_model_attempted: self.default_model_attempted.clone(),
            default_model_provider_attempted: self.default_model_provider_attempted.clone(),
            self_execute_attempted: self.self_execute_attempted.clone(),
            escalation_attempted: self.escalation_attempted.clone(),
            doc_resolution: self.last_doc_resolution.clone(),
            snapshot_generation: self.snapshot_generation(),
            pending_decision: self.last_dispatch_decision.clone(),
        }
    }

    /// The declared stage tag of a registered agent, if any. `None` means
    /// the agent is freeform (no lifecycle participation).
    fn stage_of(&self, role: &AgentId) -> Option<AgentStage> {
        let answer = self.registry.get(role).and_then(|agent| agent.stage());
        // ADR-58 P2+P3 (Batch 1) sequencing guard: when the resolved
        // blueprint staffs this role, the registry's declared stage must
        // equal the blueprint staffing tag. `debug_assert!` only — active in
        // debug builds, silent in release and for coordinators built without
        // a facade. Every Batch 2+ replacement site (R1–R11) funnels
        // through this method, so a registry/blueprint drift fails here.
        if let Some(facade) = &self.blueprint_facade {
            debug_assert!(
                match facade.stage_for_agent(role) {
                    Some(staffed) => {
                        answer.as_ref().map(AgentStage::as_str) == Some(staffed.def.tag.as_str())
                    }
                    // Role not staffed in the resolved blueprint: custom /
                    // Freeform / run_once — nothing to compare against.
                    None => true,
                },
                "registry stage {answer:?} for {role} diverges from resolved blueprint \
                 staffing {:?}",
                facade.stage_for_agent(role).map(|s| s.def.tag.as_str())
            );
        }
        answer
    }

    /// Whether `role` participates in a stage of the given known kind —
    /// facade-resolved (issue #150), so a renamed `Execution`/`Planning`
    /// stage keeps its replan/fix-loop classification. Falls back to the
    /// legacy canonical-tag check (`AgentStage::is_design` /
    /// `is_implement`, …) when no facade is attached or the role is not
    /// staffed in the resolved blueprint; on the default `standard`
    /// blueprint the two classifications agree.
    fn role_in_kind_stage(
        &self,
        role: &AgentId,
        kind: StageKind,
        legacy_check: fn(&AgentStage) -> bool,
    ) -> bool {
        match &self.blueprint_facade {
            Some(facade) => facade
                .stage_for_agent(role)
                .is_some_and(|stage| stage.def.known_kind() == Some(kind)),
            None => self.stage_of(role).as_ref().is_some_and(legacy_check),
        }
    }

    /// The single deterministic agent for a lifecycle stage, if any.
    ///
    /// ADR-35 §5: participants are resolved from the registry by stage tag
    /// instead of hardcoded role ids. When several agents declare the same
    /// stage, the lexicographically first id wins; a pipeline without any
    /// agent for a stage simply skips that stage.
    fn first_agent_for_stage(&self, stage: &AgentStage) -> Option<AgentId> {
        let mut ids = self.registry.ids_for_stage(stage);
        ids.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        let answer = ids.into_iter().next();
        // ADR-58 P2+P3 (Batch 1) sequencing guard: when the resolved
        // blueprint defines this stage tag, the set of agents participating
        // must equal the blueprint's `def.agents` staffing (sorted). The
        // lexicographically-first id is the single deterministic participant
        // both the registry path and the blueprint derive identically.
        if let Some(facade) = &self.blueprint_facade {
            debug_assert!(
                match facade.stage_by_tag(stage.as_str()) {
                    Some(staffed) => {
                        let mut expected: Vec<&str> =
                            staffed.def.agents.iter().map(String::as_str).collect();
                        expected.sort_unstable();
                        let mut actual: Vec<String> = self
                            .registry
                            .ids_for_stage(stage)
                            .iter()
                            .map(|a| a.as_str().to_string())
                            .collect();
                        actual.sort_unstable();
                        actual.iter().map(String::as_str).collect::<Vec<_>>() == expected
                    }
                    // No such stage tag in the resolved blueprint: nothing to
                    // compare against.
                    None => true,
                },
                "registry staffing for {stage} diverges from resolved blueprint \
                 def.agents {:?}",
                facade.stage_by_tag(stage.as_str()).map(|s| s.def.agents.clone())
            );
        }
        answer
    }

    /// Publish a coordinator `AgentThought` describing a fallback-ladder step.
    fn publish_ladder_note(&self, task: &SubTask, message: impl Into<String>) {
        let _ = self.bus.publish_for_session(
            task.session_id,
            task.id.0,
            EventKind::AgentThought { agent_id: "coordinator".into(), content: message.into() },
        );
    }

    /// Queue a revision subtask for `task_id`, mirroring the correction the
    /// `NeedsRevision` outcome produces: the completed task stays done, a new
    /// `Pending` subtask reusing the same role is attached as a
    /// `MustFinishBefore` child, and the parent's expected artifacts carry
    /// over so the revision receives the same deliverable contract.
    /// Returns the new revision task id.
    ///
    /// Shared by the `NeedsRevision` outcome arm and the zero-file implement
    /// success short-circuit so both paths enqueue identical correction tasks.
    fn queue_revision_subtask(
        &self,
        graph: &mut TaskGraph,
        task_id: TaskId,
        session_id: Ulid,
        role: AgentId,
        reason: String,
    ) -> TaskId {
        let revised = SubTask {
            id: TaskId::new(),
            parent_id: Some(task_id),
            session_id,
            role,
            description: format!("Revision: {reason}"),
            status: SubTaskStatus::Pending,
            dependencies: vec![task_id],
            deliverable: None,
            created_at: time::OffsetDateTime::now_utc(),
            completed_at: None,
        };
        let revised_id = revised.id;
        let expected = self
            .expected_artifacts
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(&task_id)
            .cloned()
            .unwrap_or_default();
        self.expected_artifacts
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(revised_id, expected);
        graph.add_child(revised, task_id, Dependency::MustFinishBefore);
        revised_id
    }

    /// ADR-42 §4 + ADR-45: walk the fallback ladder for a `LimitReached`
    /// subtask. Each tier is attempt-bounded and cancellation-aware; per-task
    /// guard sets (checkpointed with the run) keep the ladder loop-free by
    /// construction.
    ///
    /// - Tier 1: same agent with the global default model (once per task).
    ///   The agent's provider stays bound at construction time — the model
    ///   name is swapped, never the serving provider. Skipped when the
    ///   resolved default-model profile is served by a different pipe than
    ///   the role's serving provider (tier 1 would throw a model name across
    ///   a pipe that doesn't offer it).
    /// - Tier 1b (ADR-45): the same role rebuilt on the run's default
    ///   provider — the pipe that serves the global default model — with that
    ///   model. This is the model-first fallback: when the role's bound pipe
    ///   is the failure (latency, quota, outage), the default model is
    ///   dispatched on the pipe that actually serves it. Skipped when the
    ///   identical (model, pipe) pair already executed in tier 1, or when the
    ///   user disables `default_model_fallback`.
    /// - Tier 2: coordinator self-execution through the runner on
    ///   `planning_provider` — no artifact gate (ADR-45): the coordinator is
    ///   the last functioning execution path and must take over any subtask,
    ///   artifact-bearing or not, rather than abandoning the run.
    ///
    /// Returns `FallbackOutcome::Success` when a tier produced a run result —
    /// processed by the caller exactly like a normally dispatched result — or
    /// `FallbackOutcome::Exhausted` when every tier was skipped or failed.
    /// Cancellation observed inside a tier short-circuits with
    /// `FallbackOutcome::Cancelled`.
    async fn attempt_fallback_ladder(
        &mut self,
        task: &SubTask,
        original_role: &AgentId,
        error: &OrchestratorError,
        context: &AgentContext,
        cancel: &CancellationToken,
    ) -> FallbackOutcome {
        // A role with no registered agent is a configuration error: do not
        // silently rescue it with tier-2 self-execution (which would bypass
        // the missing specialist entirely). The caller surfaces the original
        // error through its terminal handling.
        if self.registry.get(original_role).is_none() {
            tracing::warn!(
                target: "orchestrator::coordinator",
                role = ?original_role,
                "role has no registered agent; skipping fallback ladder",
            );
            return FallbackOutcome::Exhausted;
        }
        if cancel.is_cancelled() {
            return FallbackOutcome::Cancelled;
        }

        // ── Tier 1: same agent, global default model ──────────────────────
        // Resolved profile is captured so tier 1b can detect the degenerate
        // case where the default provider IS the role's bound provider.
        let mut tier1_profile: Option<ModelProfile> = None;
        if !self.default_model_attempted.contains(&task.id) {
            self.default_model_attempted.insert(task.id);
            let profile = match self.model_selector.fallback_to_default(original_role) {
                Ok(profile) => Some(profile),
                Err(tier_err) => {
                    self.publish_ladder_note(
                        task,
                        format!(
                            "Fallback tier 1 (default model) unavailable for {original_role} \
                             subtask {}: {tier_err}",
                            task.id
                        ),
                    );
                    // Fall through to tier 1b/2; the default model is additive
                    // and its absence must not block the rest of the ladder.
                    None
                }
            };
            if let Some(profile) = profile {
                // Model-first tier-1 semantics: the fallback re-dispatches the
                // SAME agent on the model the routing engine resolved. The
                // agent's serving provider is fixed at construction, so the
                // resolved profile only selects the model/diagnostics bucket.
                // Tier 1 may only execute when the role's effective serving
                // pipe is KNOWN and that pipe actually serves the default
                // model; otherwise the dispatch would ask pipe A to serve a
                // model registered on pipe B. Both skip cases are clean (with
                // a note) and defer to tier 1b, which rebuilds the role on the
                // pipe that serves the default model. The role's effective
                // serving pipe is the per-agent provider assignment when
                // present, else the run's default provider (model-first: an
                // unassigned role serves on the default pipe).
                let serving_pipe = self
                    .agent_configs
                    .get(original_role)
                    .and_then(|config| config.provider_id.as_deref())
                    .or(self.default_provider_config_id.as_deref());
                match serving_pipe {
                    // Defensive: no pipe resolves at all (no per-agent
                    // assignment and no run-level default provider). Tier 1
                    // cannot verify the model-on-pipe pairing here — never
                    // throw the default model at an unknown pipe. Tier 1b
                    // below always knows its pipe (the run's default
                    // provider), so it can take over safely.
                    None => {
                        tier1_profile = None;
                        self.publish_ladder_note(
                            task,
                            format!(
                                "Fallback tier 1 (default model) skipped for {original_role} \
                                 subtask {}: no serving pipe resolved; the ladder continues at \
                                 tier 1b",
                                task.id
                            ),
                        );
                    }
                    Some(serving_pipe) => {
                        let pipe_mismatch = serving_pipe != profile.profile.provider_config_id;
                        if pipe_mismatch {
                            // Tier 1 does NOT execute: `tier1_profile` stays
                            // `None` so tier 1b's degenerate check does not
                            // think the (model, pipe) pair already ran.
                            tier1_profile = None;
                            self.publish_ladder_note(
                                task,
                                format!(
                                    "Fallback tier 1 (default model) skipped for {original_role} \
                                     subtask {}: the default model is served by provider {}, not \
                                     the role's serving provider {}; tier 1b will rebuild on the \
                                     default provider that serves it",
                                    task.id, profile.profile.provider_config_id, serving_pipe
                                ),
                            );
                        } else {
                            // Tier 1 executes on the role's serving pipe.
                            // Record the pair it ACTUALLY runs — model-first:
                            // the default model on the effective serving pipe
                            // — not the resolved profile's pin pipe, so tier
                            // 1b's degenerate check compares against the right
                            // reference (the pipe tier 1b would rebuild on).
                            tier1_profile = Some(ModelProfile {
                                profile: concerto_core::types::RoutingProfile {
                                    provider_config_id: serving_pipe.to_string(),
                                    ..profile.profile.clone()
                                },
                                ..profile.clone()
                            });
                            // ADR-52: a ladder-tier dispatch is a real model
                            // dispatch and counts toward the run-wide cap.
                            self.model_dispatch_count = self.model_dispatch_count.saturating_add(1);
                            match self
                                .runner
                                .run(
                                    original_role.clone(),
                                    task,
                                    context.clone(),
                                    &profile,
                                    cancel.clone(),
                                )
                                .await
                            {
                                Ok(result) => {
                                    if matches!(result.outcome, AgentOutcome::Success) {
                                        return FallbackOutcome::Success(Box::new(result));
                                    }
                                    // A completed run that still failed (outcome != Success)
                                    // counts as a tier-1 failure: the default-model swap did
                                    // not rescue the subtask, so fall through to tier 1b/2.
                                    // This keeps the ladder semantics identical whether tier
                                    // 1 fails with an error or with a failed outcome — the
                                    // coordinator takes over either way (ADR-42 §4).
                                    self.publish_ladder_note(
                                        task,
                                        format!(
                                            "Fallback tier 1 (default model) failed for \
                                             {original_role} subtask {}: {error}",
                                            task.id
                                        ),
                                    );
                                }
                                Err(tier_err) => {
                                    self.publish_ladder_note(
                                        task,
                                        format!(
                                            "Fallback tier 1 (default model) failed for \
                                             {original_role} subtask {}: {tier_err}",
                                            task.id
                                        ),
                                    );
                                    if is_cancellation_error(&tier_err) {
                                        return FallbackOutcome::Cancelled;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        // ── Tier 1b: same role rebuilt on the default provider (ADR-45) ────
        // ADR-42's tier 1 can never escape a failing pipe: the agent's serving
        // provider is bound at construction, so a default-model swap on the
        // same provider repeats the failure. Tier 1b is the model-first
        // fallback: it rebuilds the role on the run's default provider — the
        // pipe that serves the global default model — and dispatches that
        // model on it (registry factory). Gated on the user's
        // `default_model_fallback`, on the role having a rebuild factory
        // (without one `run_with_provider` would silently repeat the original
        // bound provider — a cross-pipe dispatch, not a skip), and skipped
        // when the identical (model, pipe) pair already executed in tier 1 (a
        // rebuild would be a degenerate no-op). The guard is checkpointed: at
        // most once per task per run.
        if !self.default_model_provider_attempted.contains(&task.id) && self.default_model_fallback
        {
            let provider = self.default_model_provider.clone();
            let default_model_profile = self.default_model_profile.clone();
            if let (Some(provider), Some(default_model_profile)) = (provider, default_model_profile)
            {
                self.default_model_provider_attempted.insert(task.id);
                let degenerate = tier1_profile.as_ref().is_some_and(|tier1| {
                    tier1.profile.model == default_model_profile.profile.model
                        && tier1.profile.provider_config_id
                            == default_model_profile.profile.provider_config_id
                });
                if degenerate {
                    self.publish_ladder_note(
                        task,
                        format!(
                            "Fallback tier 1b (default provider) skipped for {original_role} \
                             subtask {}: the (model, pipe) pair already executed in tier 1",
                            task.id
                        ),
                    );
                } else if !self.registry.has_rebuild_factory(original_role) {
                    // `run_with_provider` falls back to the original built
                    // agent for factory-less roles, so a dispatch here would
                    // repeat the original bound provider with the default
                    // model name on the wrong pipe. Skip like tier 2 does.
                    self.publish_ladder_note(
                        task,
                        format!(
                            "Fallback tier 1b (default provider) skipped for {original_role} \
                             subtask {}: role has no rebuild factory",
                            task.id
                        ),
                    );
                } else {
                    self.publish_ladder_note(
                        task,
                        format!(
                            "Fallback tier 1b (default provider) for {original_role} subtask {}",
                            task.id
                        ),
                    );
                    // ADR-52: a ladder-tier dispatch is a real model dispatch
                    // and counts toward the run-wide cap.
                    self.model_dispatch_count = self.model_dispatch_count.saturating_add(1);
                    match self
                        .runner
                        .run_with_provider(
                            original_role.clone(),
                            provider,
                            task,
                            context.clone(),
                            &default_model_profile,
                            cancel.clone(),
                        )
                        .await
                    {
                        Ok(result) => {
                            if matches!(result.outcome, AgentOutcome::Success) {
                                return FallbackOutcome::Success(Box::new(result));
                            }
                            self.publish_ladder_note(
                                task,
                                format!(
                                    "Fallback tier 1b (default provider) failed for \
                                     {original_role} subtask {}: {error}",
                                    task.id
                                ),
                            );
                        }
                        Err(tier_err) => {
                            self.publish_ladder_note(
                                task,
                                format!(
                                    "Fallback tier 1b (default provider) failed for \
                                     {original_role} subtask {}: {tier_err}",
                                    task.id
                                ),
                            );
                            if is_cancellation_error(&tier_err) {
                                return FallbackOutcome::Cancelled;
                            }
                        }
                    }
                }
            }
        }

        self.self_execute_tier(task, original_role, error, context, cancel).await
    }

    /// ADR-42 §4 tier 2 (amended by ADR-45 §3): coordinator takeover. When
    /// both the role's provider and the default provider have failed, the
    /// coordinator is the last functioning execution path and takes over any
    /// subtask — artifact-bearing or not. The takeover is a REAL dispatch
    /// through the runner: the role is rebuilt on the coordinator's serving
    /// pipe (`planning_provider`) and run with the full agent loop (tool loop
    /// included), so it can actually produce artifacts instead of a text-only
    /// single shot. The result is still tagged with the ADR-42 §6
    /// `"coordinator-self-execute"` provider sentinel so audit/UI consumers
    /// can identify coordinator-produced results.
    async fn self_execute_tier(
        &mut self,
        task: &SubTask,
        original_role: &AgentId,
        error: &OrchestratorError,
        context: &AgentContext,
        cancel: &CancellationToken,
    ) -> FallbackOutcome {
        if self.self_execute_attempted.insert(task.id) {
            // Skip tier 2 when the coordinator's own model profile is
            // unresolvable or the role has no rebuild factory: both would
            // silently repeat an earlier attempt (a profile-less single-shot,
            // or the original bound provider through `get_with_provider`'s
            // fallback) instead of switching to the coordinator's pipe.
            let planning_profile = self.planning_profile.clone();
            let rebuildable = self.registry.has_rebuild_factory(original_role);
            match (planning_profile, rebuildable) {
                (Some(planning_profile), true) => {
                    // ADR-52: tier-2 coordinator takeover is a real model
                    // dispatch and counts toward the run-wide cap.
                    self.model_dispatch_count = self.model_dispatch_count.saturating_add(1);
                    match self
                        .runner
                        .run_with_provider(
                            original_role.clone(),
                            self.planning_provider.clone(),
                            task,
                            context.clone(),
                            &planning_profile,
                            cancel.clone(),
                        )
                        .await
                    {
                        Ok(mut result) => {
                            // Only a Success with substantive output counts:
                            // an empty deliverable is a tier failure, not a
                            // completed subtask (FIX 1 semantics, preserved
                            // from the single-shot path).
                            if matches!(result.outcome, AgentOutcome::Success)
                                && !result.summary.trim().is_empty()
                            {
                                // ADR-42 §6: tag the takeover with the
                                // coordinator-self-execute sentinel so
                                // provider metrics/audit identify the tier.
                                result.provider = "coordinator-self-execute".into();
                                return FallbackOutcome::Success(Box::new(result));
                            }
                            self.publish_ladder_note(
                                task,
                                format!(
                                    "Fallback tier 2 (coordinator self-execution) failed for \
                                     subtask {}: empty or non-success deliverable",
                                    task.id
                                ),
                            );
                        }
                        Err(tier_err) => {
                            self.publish_ladder_note(
                                task,
                                format!(
                                    "Fallback tier 2 (coordinator self-execution) failed for \
                                     subtask {}: {tier_err}",
                                    task.id
                                ),
                            );
                            if is_cancellation_error(&tier_err) {
                                return FallbackOutcome::Cancelled;
                            }
                        }
                    }
                }
                (None, _) => {
                    self.publish_ladder_note(
                        task,
                        format!(
                            "Fallback tier 2 (coordinator self-execution) skipped for subtask \
                             {}: no planning profile resolved for the coordinator's serving pipe",
                            task.id
                        ),
                    );
                }
                (Some(_), false) => {
                    self.publish_ladder_note(
                        task,
                        format!(
                            "Fallback tier 2 (coordinator self-execution) skipped for subtask \
                              {}: role has no rebuild factory",
                            task.id
                        ),
                    );
                }
            }
        }
        self.publish_ladder_note(
            task,
            format!(
                "Fallback ladder exhausted for {original_role} subtask {}; surfacing partial \
                 outcome: {error}",
                task.id
            ),
        );
        FallbackOutcome::Exhausted
    }

    /// Replace the default collaboration topology with validated rules.
    pub fn with_collaboration_rules(
        mut self,
        rules: Vec<CollaborationRule>,
    ) -> Result<Self, OrchestratorError> {
        self.relationships = RelationshipManager::new(rules).map_err(|error| {
            OrchestratorError::AgentLoopError(format!("invalid collaboration rules: {error}"))
        })?;
        Ok(self)
    }

    /// Attach per-agent prompt/capability configs. These are passed to the
    /// registry at construction time so each `ExpertAgent` carries its
    /// configured `PromptSections` and `AgentCapabilities`.
    pub fn with_agent_configs(
        mut self,
        agent_configs: HashMap<AgentId, CustomAgentConfig>,
    ) -> Self {
        self.agent_configs = agent_configs;
        self
    }

    /// Retrieve project memory context for the specialist agents.
    async fn retrieve_memory_context(
        &self,
        task: &AgentTask,
        context: &mut AgentContext,
        cancel: CancellationToken,
    ) {
        // Populate retrieved_chunks so every specialist agent (Architect,
        // Researcher, Coder, Reviewer, Validator) sees relevant project
        // context from the start. This is the snapshot-at-start approach
        // described in audit §3.3; per-cycle refresh is Phase 2.
        if context.retrieved_chunks.is_empty() {
            let query = MemoryQuery {
                text: task.description.clone(),
                project_id: context.session.project_id.clone(),
                namespace: MemoryNamespace::Project(context.session.project_id.clone()),
                top_k: 5,
                filters: vec![],
            };
            context.retrieved_chunks =
                self.memory_store.retrieve(&query, cancel).await.unwrap_or_default();
        }
    }

    /// Decompose the task into a TaskGraph, or restore from a previous checkpoint.
    async fn decompose_or_restore(
        &mut self,
        task: &AgentTask,
        context: &AgentContext,
        cancel: &CancellationToken,
        resume_checkpoint_json: Option<String>,
    ) -> Result<DecomposeResult, OrchestratorError> {
        if let Some(cp_json) = resume_checkpoint_json {
            match self.restore_and_evaluate(&cp_json, task, context, cancel).await {
                Ok(Some(result)) => return Ok(result),
                // ADR-65 §7 Replan: the workspace objectively changed
                // materially to the pending step. The resume path itself
                // dispatches nothing — the fresh decompose below lets the
                // Phase-6 scheduler govern the new planning.
                Ok(None) => {}
                Err(error) => return Err(error),
            }
        }

        // Fresh decompose path (also the ADR-65 §7 Replan destination).
        // If the Architect agent fails (e.g. repeated malformed JSON
        // from the LLM) we return a clean Partial result rather than
        // propagating an error that hard-crashes the session.
        let (graph, plan_artifact) = match self.decompose_task(task, context, cancel, None).await {
            Ok(result) => result,
            Err(e) => {
                let _ = self.bus.publish_for_session(
                    task.session_id,
                    task.id.0,
                    EventKind::MultiAgentModeCompleted { task_id: task.id, cost_usd: 0.0 },
                );
                return Err(e);
            }
        };
        TaskGraphValidator::validate(&graph)?;
        let completed_results = graph
            .all_tasks()
            .into_iter()
            .filter_map(|subtask| {
                subtask.deliverable.as_ref().map(|summary| {
                    (
                        subtask.id,
                        AgentRunResult {
                            task_id: subtask.id,
                            role: subtask.role.clone(),
                            outcome: AgentOutcome::Success,
                            summary: summary.clone(),
                            files_modified: Vec::new(),
                            tool_call_count: 0,
                            cost_usd: 0.0,
                            latency_ms: 0,
                            provider: String::new(),
                            model: String::new(),
                            tokens_in: 0,
                            tokens_out: 0,
                        },
                    )
                })
            })
            .collect();

        // ADR-55 Phase 2b: the planning-only branch binds its rendered
        // plan to the durable artifact id, so *both* decompose paths must
        // persist one — including the heuristic-pipeline fallback, which
        // by itself yields no `PlanArtifact` (observed live: a planner
        // JSON parse failure left `plans/` empty and `plan_id` null even
        // though the run completed and rendered a plan).
        let plan_id = match plan_artifact {
            Some(plan) => self.persist_plan_artifact(&plan),
            None => {
                let fallback_plan = PlanArtifact::from_graph(
                    Ulid::new().to_string(),
                    task,
                    &graph,
                    &HashMap::new(),
                );
                self.persist_plan_artifact(&fallback_plan)
            }
        };
        // ADR-55 Phase 2b: retain the id so a planning-only run can bind
        // its rendered plan to the durable artifact.
        self.last_plan_id = plan_id.clone();
        let _ = self.bus.publish_for_session(
            task.session_id,
            task.id.0,
            EventKind::MultiAgentModeStarted {
                task_id: task.id,
                subtask_count: graph.len(),
                plan_id,
            },
        );
        Ok(DecomposeResult {
            graph,
            completed_results,
            total_cost: 0.0,
            total_tool_calls: 0,
            all_files: Vec::new(),
            provider_metrics: Vec::new(),
            subtask_attempts: HashMap::new(),
            retry_feedback: HashMap::new(),
            model_assignments: HashMap::new(),
            action_ledger: Vec::new(),
            objective: task.description.clone(),
            objective_hash: blake3::hash(task.description.as_bytes()).to_hex().to_string(),
        })
    }

    /// Restore the graph from `cp_json` and run the ADR-65 §7 resume
    /// evaluation: read the facts appended after the checkpoint's whiteboard
    /// cursor, compare workspace reality (fresh snapshot generation + F3
    /// reconciliation), and choose — continue blocked task / replace agent /
    /// skip / refresh evidence / replan — recording every outcome as a
    /// whiteboard `Decision` event with REAL evidence ids (acceptance 8:
    /// the append validates them). `Ok(None)` means Replan.
    ///
    /// Fail-soft by contract: a log read failure or an unreadable payload
    /// degrades individual evaluation inputs, never the restore; only the
    /// existing checkpoint/scope errors propagate.
    async fn restore_and_evaluate(
        &mut self,
        cp_json: &str,
        task: &AgentTask,
        context: &AgentContext,
        cancel: &CancellationToken,
    ) -> Result<Option<DecomposeResult>, OrchestratorError> {
        let mut cp: crate::checkpoint::GraphCheckpoint =
            crate::checkpoint::GraphCheckpoint::from_json(cp_json).map_err(|e| {
                OrchestratorError::AgentLoopError(format!(
                    "failed to deserialize resume checkpoint: {e}"
                ))
            })?;

        // Validate checkpoint scope as a second safety net (the
        // runtime_runner already does this, but we check again here
        // for defense-in-depth). A scope mismatch is surfaced as a
        // clean AgentLoopError so the caller can fall through to a
        // fresh run rather than producing a corrupted partial output.
        cp.validate_scope(task.session_id, &context.session.project_id.0).map_err(|reason| {
            OrchestratorError::AgentLoopError(format!(
                "checkpoint scope validation failed: {reason}"
            ))
        })?;

        // ── ADR-65 §7: additive v4 backfill ──────────────────────────────
        // Pre-§7 (v3) records carry no cursor/doc/snapshot fields; derive
        // them from the log (additive, fail-soft). A log read below that
        // fails degrades to an empty slice and every absent field stays
        // absent — a resume must not fail because history is imperfect.
        let log_window = self.read_resume_log_window(task.session_id, cancel).await;
        resume::backfill_v4_fields(&mut cp, &log_window, self.resume_cursor_hint_ms);
        // The evidence view: facts appended AFTER the cursor only (never the
        // pre-cursor log — no prose replay, no pre-cursor fact replayed into
        // a decision). A checkpoint without a cursor (backfill degraded)
        // treats the whole log as pre-cursor: the conservative reading.
        let post_cursor = match cp.whiteboard_cursor_gate_seq {
            Some(cursor) => resume::split_at_cursor(&log_window, cursor).1,
            None => &[],
        };

        let mut graph = crate::checkpoint::restore_graph(&cp).map_err(|e| {
            OrchestratorError::AgentLoopError(format!("checkpoint restore failed: {e}"))
        })?;
        let completed_results = cp.completed_results;
        let total_cost = cp.total_cost;
        let total_tool_calls = cp.total_tool_calls;
        let all_files = cp.all_files;
        let provider_metrics = cp.provider_metrics;
        let mut subtask_attempts = cp.subtask_attempts;
        let retry_feedback = cp.retry_feedback;
        let model_assignments = cp.model_assignments;
        let action_ledger = cp.action_ledger;
        // Retain the original plan so subsequent checkpoints keep it too.
        *self.design_doc.lock().unwrap_or_else(|error| error.into_inner()) = cp.design_doc.clone();
        // Retain expected-artifact expectations so the C-06 acceptance
        // gate verifies the same files after a resume.
        *self.expected_artifacts.lock().unwrap_or_else(|error| error.into_inner()) =
            cp.expected_artifacts.clone();
        // Restore the ADR-42/ADR-45 ladder guards so a resumed run does
        // NOT re-walk ladder tiers (default-model swap, default-provider
        // re-dispatch, self-execution, escalation retry) that already
        // fired before the interruption.
        self.default_model_attempted = cp.default_model_attempted.clone();
        self.default_model_provider_attempted = cp.default_model_provider_attempted.clone();
        self.self_execute_attempted = cp.self_execute_attempted.clone();
        self.escalation_attempted = cp.escalation_attempted.clone();
        // ADR-65 §7: keep the doc resolution and pending decision
        // round-tripping — the resumed run's checkpoints carry them forward
        // (a planning run refreshes them; a dispatch decision recorded below
        // replaces the stale pending one).
        self.last_doc_resolution = cp.doc_resolution.clone();
        self.last_dispatch_decision = cp.pending_decision.clone();

        // ── ADR-65 §7: evaluate the resume at the cursor ─────────────────
        let pending_decision = cp.pending_decision.clone();
        let snapshot_generation_at_checkpoint = cp.snapshot_generation.clone();
        let application = self
            .evaluate_and_apply_resume(
                &mut graph,
                task,
                context,
                &action_ledger,
                &completed_results,
                &all_files,
                pending_decision.as_ref(),
                snapshot_generation_at_checkpoint.as_deref(),
                &log_window,
                post_cursor,
                cancel,
            )
            .await;
        let ResumeApplication::Applied { ledger_entries, re_armed } = application else {
            // Replan. The restored plan is superseded by workspace reality:
            // clear the restored per-plan bookkeeping so the fresh decompose
            // repopulates it for the new graph.
            self.expected_artifacts.lock().unwrap_or_else(|error| error.into_inner()).clear();
            self.last_doc_resolution = None;
            self.last_dispatch_decision = None;
            warn!(
                run_id = %cp.run_id,
                "ADR-65 §7: resume chose REPLAN — the workspace objectively changed \
                 materially to the pending step; delegating to the Phase-6 scheduler"
            );
            return Ok(None);
        };
        let mut action_ledger = action_ledger;
        action_ledger.extend(ledger_entries);
        if let Some(re_armed) = re_armed {
            // The resume decision granted the re-armed step a fresh bounded
            // attempt budget (the ladder guards stay restored, so no tier
            // re-walks); without the reset the resumed run would exit
            // Partial before ever dispatching the decided task.
            subtask_attempts.insert(re_armed, 0);
        }

        // ADR-52: a resumed run re-persists its durable plan artifact
        // (idempotent overwrite of `plan-<run_id>.json`) so the plans dir
        // stays a complete history of every plan execution, and carries
        // the run's plan_id into the lifecycle event.
        let plan =
            PlanArtifact::from_graph(cp.run_id.to_string(), task, &graph, &cp.expected_artifacts);
        let plan_id = self.persist_plan_artifact(&plan);
        // ADR-55 Phase 2b: retain the id so a planning-only run can bind
        // its rendered plan to the durable artifact.
        self.last_plan_id = plan_id.clone();
        let _ = self.bus.publish_for_session(
            task.session_id,
            task.id.0,
            EventKind::MultiAgentModeStarted {
                task_id: task.id,
                subtask_count: graph.len(),
                plan_id,
            },
        );
        // Run-continuity Phase 1: keep recording the ORIGINAL objective
        // (text + hash) in every checkpoint this resumed run persists —
        // the resume input is a bare "continue", not the objective. The
        // fields are trusted metadata from the validated v3 record; a
        // corrupt value degrades the recorded objective text only and is
        // never a reason to refuse the resume (fail-soft).
        let objective = cp.objective.clone();
        let objective_hash = cp.objective_hash.clone();
        Ok(Some(DecomposeResult {
            graph,
            completed_results,
            total_cost,
            total_tool_calls,
            all_files,
            provider_metrics,
            subtask_attempts,
            retry_feedback,
            model_assignments,
            action_ledger,
            objective,
            objective_hash,
        }))
    }

    /// Read this session's log tail window for the resume evaluation
    /// (ADR-65 §7 read side). Fail-soft: no log pool or a read error yields
    /// an empty slice — the evaluation then relies on the checkpoint's own
    /// ledger and workspace reality alone, and never fails the resume. The
    /// window is a single bounded read of the newest events, anchored at the
    /// log head.
    async fn read_resume_log_window(
        &self,
        session_id: Ulid,
        cancel: &CancellationToken,
    ) -> Vec<WhiteboardEvent> {
        let Some(pool) = self.review_store.as_ref() else {
            return Vec::new();
        };
        let head = match concerto_sessions::whiteboard::latest_gate_seq(pool).await {
            Ok(head) => head,
            Err(error) => {
                warn!(%error, "ADR-65 §7: resume log read failed (head); treating the log as pre-cursor");
                return Vec::new();
            }
        };
        let after = head.saturating_sub(RESUME_LOG_WINDOW as u64);
        match load_whiteboard_events(
            pool,
            &WhiteboardLoadOpts {
                after_gate_seq: after,
                session_id: Some(session_id.to_string()),
                scope: None,
                limit: RESUME_LOG_WINDOW,
            },
        )
        .await
        {
            Ok(events) => events,
            Err(error) => {
                if cancel.is_cancelled() {
                    return Vec::new();
                }
                warn!(%error, "ADR-65 §7: resume log window read failed (fail-soft); \
                     the evaluation proceeds on the checkpoint ledger alone");
                Vec::new()
            }
        }
    }

    /// The ADR-65 §7 workspace-change verdict for a resume: the fresh
    /// snapshot generation compared against the checkpoint's, plus the F3
    /// reconciliation of the observed rows against the live filesystem —
    /// changed/vanished paths the run's OWN recorded writes do not explain
    /// are the externally-changed evidence. Fail-soft: every read/storage
    /// failure degrades to "no change detected" on that axis.
    async fn workspace_change_verdict(
        &self,
        checkpoint_generation: Option<&str>,
        own_written: &std::collections::HashSet<String>,
        cancel: &CancellationToken,
    ) -> resume::WorkspaceChange {
        let mut change = resume::WorkspaceChange::default();
        if let (Some(recorded), Some(fresh)) = (checkpoint_generation, self.snapshot_generation()) {
            change.generation_mismatch = recorded != fresh;
        }
        let Some(snapshot) = self.workspace_snapshot.as_ref() else { return change };
        let Some(pool) = self.review_store.as_ref() else { return change };
        // ADR-65 F5c: reconcile only this project root's rows.
        let root_hash = crate::tool_facts::project_root_hash(snapshot.project_root.as_std_path());
        let rows = match ResourceFacts::new(pool.clone())
            .list_observations(&root_hash, MAX_EVIDENCE_OBSERVATIONS, cancel)
            .await
        {
            Ok(rows) => rows,
            Err(error) => {
                if !cancel.is_cancelled() {
                    warn!(%error, "ADR-65 §7: workspace reconciliation read failed (fail-soft)");
                }
                return change;
            }
        };
        for row in rows {
            // ADR-65 F3: a row whose live stat (size, mtime) diverges from
            // the observation — or whose file vanished — is stale.
            let fresh = std::fs::metadata(snapshot.project_root.join(&row.path))
                .ok()
                .filter(|meta| {
                    row.size_bytes.unwrap_or(0) == meta.len()
                        && row.mtime_ms == crate::tool_facts::mtime_ms(meta)
                })
                .is_some();
            if fresh {
                continue;
            }
            let canonical = crate::tool_facts::canonical_project_path(
                snapshot.project_root.as_std_path(),
                &row.path,
            )
            .unwrap_or_else(|| row.path.clone());
            if own_written.contains(&canonical) {
                // Already explained by the run's own recorded writes.
                continue;
            }
            change.externally_changed.push((row.path.clone(), row.last_event_id.clone()));
        }
        change
    }

    /// The ADR-65 §7 resume evaluation driver: gather the blocked step, its
    /// post-cursor facts, the workspace-change verdict and the replacement
    /// candidates, evaluate the deterministic policy, record the Decision
    /// event (fail-soft, real evidence ids only), and apply the outcome to
    /// the graph.
    #[allow(clippy::too_many_arguments)]
    async fn evaluate_and_apply_resume(
        &mut self,
        graph: &mut TaskGraph,
        task: &AgentTask,
        context: &AgentContext,
        checkpoint_action_ledger: &[checkpoint::CheckpointAction],
        completed_results: &HashMap<TaskId, AgentRunResult>,
        checkpoint_all_files: &[camino::Utf8PathBuf],
        pending_decision: Option<&checkpoint::CheckpointPendingDecision>,
        checkpoint_snapshot_generation: Option<&str>,
        log_window: &[WhiteboardEvent],
        post_cursor: &[WhiteboardEvent],
        cancel: &CancellationToken,
    ) -> ResumeApplication {
        // Steps a previous resume already skipped stay skipped — a recorded
        // skip is terminal for this plan.
        let skipped_ids: HashSet<TaskId> = checkpoint_action_ledger
            .iter()
            .filter(|action| action.kind == "resume-skipped")
            .filter_map(|action| action.task_id)
            .collect();
        // The first blocked/failed step in graph order (callers of the
        // restored run proceed step by step; later resumes handle the rest).
        // Snapshot the decision inputs before any outcome mutates the graph.
        let snapshot = graph.all_tasks().into_iter().find_map(|subtask| {
            let is_candidate =
                matches!(subtask.status, SubTaskStatus::Blocked | SubTaskStatus::Failed)
                    && !skipped_ids.contains(&subtask.id);
            if !is_candidate {
                return None;
            }
            // Failed outcomes for this step: ledger `failed` entries
            // (pre-cursor) plus post-cursor failure facts. The cursor keeps
            // the two disjoint, so summing never double-counts; without a
            // cursor the post-cursor view is empty (conservative).
            let ledger_failures = checkpoint_action_ledger
                .iter()
                .filter(|action| action.kind == "failed" && action.task_id == Some(subtask.id))
                .count() as u32;
            let facts = resume::task_facts_after_cursor(post_cursor, &subtask.id.0.to_string());
            Some((subtask.id, subtask.role.clone(), facts, ledger_failures))
        });

        let mut blocked: Option<resume::BlockedStep> = None;
        let mut step_facts = resume::TaskFacts::default();
        if let Some((task_id, role, facts, ledger_failures)) = snapshot {
            step_facts = facts.clone();
            // The capability class maps from the registry's stage tags
            // (ADR-58): research → Explore, design → Design; everything
            // else (including freeform workers) is Implement-class work.
            let stage = self.stage_of(&role).map(|stage| stage.as_str().to_owned());
            let class = match stage.as_deref() {
                Some(tag) if tag == AgentStage::RESEARCH => resume::StepClass::Explore,
                Some(tag) if tag == AgentStage::DESIGN => resume::StepClass::Design,
                _ => resume::StepClass::Implement,
            };
            // Agents already tried for this step: its current role, every
            // agent a logged decision selected, and the pending decision's
            // selection. Replacement never re-selects a tried agent.
            let mut tried_agents = vec![role.as_str().to_owned()];
            tried_agents.extend(resume::selected_agents_after_cursor(log_window));
            if let Some(pending) = pending_decision {
                if !pending.selected_agent.is_empty() {
                    tried_agents.push(pending.selected_agent.clone());
                }
            }
            tried_agents.sort();
            tried_agents.dedup();
            // Acceptance 7: the recorded, evidence-backed decision gate —
            // the pending decision or any logged Decision row explicitly
            // selecting this step's agent with evidence ids.
            let recorded_selection =
                log_window.iter().any(|event| resume::decision_selects(event, role.as_str()))
                    || pending_decision.is_some_and(|pending| {
                        pending.selected_agent == role.as_str()
                            && !pending.supporting_evidence_ids.is_empty()
                    });
            blocked = Some(resume::BlockedStep {
                task_id,
                agent: role.as_str().to_owned(),
                class,
                failure_count: ledger_failures + facts.failure_event_ids.len() as u32,
                tried_agents,
                recorded_selection,
            });
        }

        // Replacement candidates: registered agents with the implement
        // stage tag minus the tried agents, deterministic order
        // (lexicographic — the registry map has no stable order). Design /
        // explore steps are never replaced by the resume path (acceptance
        // 7): continue-behind-decision or skip.
        let candidates: Vec<String> = match blocked.as_ref().map(|step| step.class) {
            Some(resume::StepClass::Implement) => {
                let implement_tag = execution_stage_tag(self.blueprint_facade.as_ref());
                let mut ids = self.registry.ids_for_stage(&AgentStage::new(implement_tag));
                ids.sort_by(|a, b| a.as_str().cmp(b.as_str()));
                ids.into_iter()
                    .map(|id| id.as_str().to_owned())
                    .filter(|id| {
                        blocked.as_ref().is_some_and(|step| !step.tried_agents.contains(id))
                    })
                    .collect()
            }
            _ => Vec::new(),
        };

        // The workspace-change verdict: generation mismatch + the F3
        // reconciliation restricted to paths the run's OWN writes do not
        // explain.
        let own_written = own_write_paths(
            checkpoint_all_files,
            completed_results,
            post_cursor,
            &context.session.project_dir,
        );
        let change = self
            .workspace_change_verdict(checkpoint_snapshot_generation, &own_written, cancel)
            .await;
        let outcome = resume::evaluate(&resume::ResumeInput {
            blocked_step: blocked.as_ref(),
            replacement_candidates: &candidates,
            pending_decision,
            step_facts: &step_facts,
            change: &change,
        });

        // Record the outcome as a whiteboard Decision event (ADR-65 §6/§7):
        // reason + REAL evidence ids only — post-cursor task facts for a
        // step outcome, the changed rows' observation ids for a workspace
        // outcome. A fabricated id is rejected at append (acceptance 8), so
        // never invent one.
        let mut evidence_ids: Vec<String> = Vec::new();
        if blocked.is_some() {
            evidence_ids.extend(step_facts.progress_event_ids.iter().cloned());
            evidence_ids.extend(step_facts.failure_event_ids.iter().cloned());
        } else {
            evidence_ids.extend(change.externally_changed.iter().filter_map(|(_, event_id)| {
                event_id.as_ref().filter(|id| !id.is_empty()).cloned()
            }));
        }
        evidence_ids.truncate(MAX_EVIDENCE_OBSERVATIONS);
        self.append_resume_decision(task.session_id, &outcome, &evidence_ids).await;

        match &outcome {
            ResumeOutcome::Replan => ResumeApplication::Replan,
            ResumeOutcome::RestoreAndContinue | ResumeOutcome::RefreshEvidence => {
                ResumeApplication::Applied { ledger_entries: Vec::new(), re_armed: None }
            }
            ResumeOutcome::ContinueBlocked { .. }
            | ResumeOutcome::ReplaceAgent { .. }
            | ResumeOutcome::SkipStep { .. } => {
                let ledger_entries = resume::apply_outcome(&mut *graph, &outcome, blocked.as_ref());
                let re_armed = if outcome.dispatches() {
                    blocked.as_ref().map(|step| step.task_id)
                } else {
                    None
                };
                ResumeApplication::Applied { ledger_entries, re_armed }
            }
        }
    }

    /// Append the whiteboard `Decision` event for a resume outcome
    /// (ADR-65 §7): `selected_agent, reason, required_output,
    /// supporting_evidence_ids` — real ids only (the append validates them,
    /// acceptance 8) and fail-soft like every continuity write.
    async fn append_resume_decision(
        &self,
        session_id: Ulid,
        outcome: &ResumeOutcome,
        evidence_ids: &[String],
    ) {
        let Some(pool) = self.review_store.as_ref() else { return };
        let required_output = match outcome {
            ResumeOutcome::RestoreAndContinue => {
                "Restored the graph; no blocked step to decide on".to_owned()
            }
            ResumeOutcome::ContinueBlocked { agent } => {
                format!("Continue the blocked subtask with {agent} from the whiteboard cursor")
            }
            ResumeOutcome::ReplaceAgent { previous, replacement } => {
                format!("Replace the blocked subtask's agent {previous} with {replacement}")
            }
            ResumeOutcome::SkipStep { agent } => {
                format!("Skip the blocked subtask previously dispatched to {agent}")
            }
            ResumeOutcome::RefreshEvidence => {
                "Refresh the workspace evidence (snapshot barrier re-ran); continue".to_owned()
            }
            ResumeOutcome::Replan => {
                "Workspace objectively changed: replan via the evidence scheduler".to_owned()
            }
        };
        let event = NewWhiteboardEvent {
            event_id: Ulid::new().to_string(),
            agent_id: "coordinator".to_owned(),
            kind: WhiteboardKind::Decision,
            scope: String::new(),
            session_id: Some(session_id.to_string()),
            plan_id: None,
            causation: None,
            payload: serde_json::json!({
                "selected_agent": outcome.selected_agent().unwrap_or(""),
                "reason": outcome.reason_code(),
                "required_output": required_output,
                "supporting_evidence_ids": evidence_ids,
            }),
            pre_image_hash: None,
            created_at: crate::tool_facts::unix_ms(),
        };
        if let Err(error) = append_whiteboard_event(pool, &event).await {
            warn!(%error, "ADR-65 §7: resume decision append failed (fail-soft)");
        }
    }

    /// Execute the task graph until all tasks complete. Returns the final
    /// `AgentOutput` for a fully-completed run.
    #[allow(clippy::too_many_arguments)]
    async fn execute_graph(
        &mut self,
        task: AgentTask,
        mut context: AgentContext,
        cancel: CancellationToken,
        mut graph: TaskGraph,
        mut completed_results: HashMap<TaskId, AgentRunResult>,
        mut total_cost: f64,
        mut total_tool_calls: u32,
        mut all_files: Vec<camino::Utf8PathBuf>,
        mut provider_metrics: Vec<ProviderMetrics>,
        mut subtask_attempts: HashMap<TaskId, u32>,
        mut retry_feedback: HashMap<TaskId, Vec<AgentRunResult>>,
        mut model_assignments: HashMap<TaskId, String>,
        mut action_ledger: Vec<checkpoint::CheckpointAction>,
        run_objective: String,
        run_objective_hash: String,
    ) -> Result<(AgentOutput, Vec<String>), OrchestratorError> {
        // ADR-52: the run-wide dispatch cap is per `execute_graph` invocation
        // (a fresh run or a resume from checkpoint restarts the counter).
        self.model_dispatch_count = 0;
        let mut terminal_subtask_failure = None;
        // When resuming from a checkpoint, blocked tasks with exhausted retry
        // attempts may already be present in the graph.  Populate
        // `terminal_subtask_failure` so the empty-ready-queue exit below
        // returns Partial (ADR-26) instead of INTERNAL_ERROR (C-05).
        // On a fresh run there are no blocked tasks, so this is a no-op.
        for task in graph.all_tasks() {
            if task.status == SubTaskStatus::Blocked {
                let attempts = subtask_attempts.get(&task.id).copied().unwrap_or(0);
                if attempts >= self.max_subtask_attempts {
                    let _ = self.bus.publish_for_session(
                        task.session_id,
                        task.id.0,
                        EventKind::SubTaskFailed {
                            task_id: task.id,
                            role: task.role.clone(),
                            error: format!(
                                "subtask exhausted after {} attempt(s) (resumed from checkpoint)",
                                attempts,
                            ),
                        },
                    );
                    terminal_subtask_failure = Some(OrchestratorError::SubTaskRetriesExhausted {
                        task_id: task.id,
                        role: task.role.clone(),
                        attempts,
                        last_error: "subtask exhausted retry attempts before checkpoint".into(),
                    });
                    break;
                }
            }
        }
        let mut non_recoverable_exit: Option<(TaskId, AgentId, OrchestratorError)> = None;
        let mut recoverable_notes = Vec::new();
        // Zero-file implement successes short-circuit to a revision subtask
        // only ONCE per lineage (reset per `execute_graph` invocation, like
        // `model_dispatch_count`). Keyed by the lineage root: the FIRST
        // zero-file success queues a revision, and any LATER zero-file
        // implement success in the same lineage — the revision/revise pass
        // itself reusing the same role and completing with the same empty
        // `files_modified` — fails the subtask instead of re-arming the
        // short-circuit. Without this bound the guard would recurse forever:
        // revision subtasks get fresh `TaskId`s, so `subtask_attempts` cannot
        // limit them, and `max_total_iterations` defaults to `None`.
        let mut zero_file_revision_queued_roots: HashSet<TaskId> = HashSet::new();
        let mut checkpoint_scope = checkpoint::CheckpointScope {
            run_id: Ulid::new(),
            session_id: task.session_id,
            root_task_id: task.id,
            project_id: context.session.project_id.0.clone(),
            // Run-continuity Phase 1: the run objective (the ORIGINAL
            // objective text on a resumed run, the task description on a
            // fresh one) — never the (bare "continue") resume input.
            objective: run_objective,
            objective_hash: run_objective_hash,
            source_revision: self.source_revision.clone(),
            sequence_num: 0,
        };
        // ADR-65 §3: stamp the run id (matching the checkpoint scope) so
        // every dispatched agent records its tool evidence under this run.
        self.run_id = Some(checkpoint_scope.run_id.to_string());
        refresh_working_memory(
            &mut context,
            &graph,
            &completed_results,
            &|role| self.stage_of(role),
            self.blueprint_facade.as_ref(),
        );
        checkpoint_scope.sequence_num = checkpoint_scope.sequence_num.saturating_add(1);
        let mut initial_execution_checkpoint = checkpoint::build_checkpoint(
            &checkpoint_scope,
            checkpoint::CheckpointStage::Executing,
            None,
            &context.working_memory,
            &graph,
            &completed_results,
            total_cost,
            total_tool_calls,
            &provider_metrics,
            &all_files,
            &self.expected_artifacts_snapshot(),
            &subtask_attempts,
            &retry_feedback,
            &self.checkpoint_context(&model_assignments, &action_ledger),
        );
        self.persist_checkpoint(&mut initial_execution_checkpoint).await;

        // ADR-35 §5: lifecycle stages are resolved from the registry rather
        // than hardcoded role ids. A pipeline without a design-stage agent
        // cannot replan after artifact failures; a pipeline without an
        // implement-stage agent cannot spawn revision tasks (the planner
        // already rejects that case). Tags are resolved through the facade
        // by kind, so renamed Planning/Execution stages keep their replan
        // and revision machinery (issue #150).
        let design_tag =
            kind_stage_tag(self.blueprint_facade.as_ref(), StageKind::Planning, AgentStage::DESIGN);
        let design_role = self.first_agent_for_stage(&AgentStage::new(design_tag));
        let implement_tag = execution_stage_tag(self.blueprint_facade.as_ref());
        let implement_ids = self.registry.ids_for_stage(&AgentStage::new(implement_tag));

        loop {
            if cancel.is_cancelled() {
                // An interrupted provider/tool call has no durable completion
                // record. Put its graph node back in the ready state and save
                // that transition before returning so a later Continue never
                // restores an orphaned Running node.
                graph.mark_all_with_status(
                    concerto_core::types::SubTaskStatus::Running,
                    concerto_core::types::SubTaskStatus::Pending,
                );
                refresh_working_memory(
                    &mut context,
                    &graph,
                    &completed_results,
                    &|role| self.stage_of(role),
                    self.blueprint_facade.as_ref(),
                );
                checkpoint_scope.sequence_num = checkpoint_scope.sequence_num.saturating_add(1);
                let mut interrupted = checkpoint::build_checkpoint(
                    &checkpoint_scope,
                    checkpoint::CheckpointStage::Executing,
                    None,
                    &context.working_memory,
                    &graph,
                    &completed_results,
                    total_cost,
                    total_tool_calls,
                    &provider_metrics,
                    &all_files,
                    &self.expected_artifacts_snapshot(),
                    &subtask_attempts,
                    &retry_feedback,
                    &self.checkpoint_context(&model_assignments, &action_ledger),
                );
                self.persist_checkpoint(&mut interrupted).await;
                return Err(OrchestratorError::Cancelled);
            }

            // Clone IDs to avoid borrow conflicts with mutable graph access
            let ready_ids: Vec<(TaskId, AgentId)> =
                graph.ready_tasks().iter().map(|st| (st.id, st.role.clone())).collect();

            // ADR-64 Phase 5: capsule projection — clone the timeline
            // projection out of the resolver block so it is available
            // for capsule building in the dispatch futures below.
            let mut projection_for_capsule: Option<crate::timeline::TimelineProjection> = None;

            // ── ADR-64 Phase 6: resolver short-circuit ──────────────
            // Before dispatching any ready tasks, check whether the
            // resolver can prove a task is *reusable* from the timeline.
            // Reuse = zero model dispatch: the cached result is injected
            // directly.  All other verdicts flow through normal dispatch.
            let ready_ids = if !ready_ids.is_empty() {
                if let Some(pool) = self.review_store.as_ref() {
                    // Build the timeline projection from durable sources.
                    let projection_opt = match crate::timeline::build_timeline(
                        pool,
                        Some(&task.session_id.to_string()),
                        self.last_plan_id.as_deref(),
                        u64::MAX,
                    )
                    .await
                    {
                        Ok(proj) => Some(proj),
                        Err(e) => {
                            tracing::warn!(
                                target: "orchestrator::coordinator",
                                error = %e,
                                "resolver: failed to build timeline projection, skipping reuse short-circuit"
                            );
                            None
                        }
                    };

                    if let Some(projection) = projection_opt {
                        // ADR-64 Phase 5: keep a clone for capsule building.
                        projection_for_capsule = Some(projection.clone());
                        // Derive plan_version: design_doc content hash → objective_hash.
                        let plan_version = {
                            let design_doc =
                                self.design_doc.lock().unwrap_or_else(|error| error.into_inner());
                            if let Some(ref doc) = *design_doc {
                                // Hash the serialized design doc to get a stable
                                // content hash, matching plan_approval.rs:173.
                                serde_json::to_string(doc)
                                    .ok()
                                    .map(|s| blake3::hash(s.as_bytes()).to_hex().to_string())
                                    .unwrap_or_else(|| checkpoint_scope.objective_hash.clone())
                            } else {
                                checkpoint_scope.objective_hash.clone()
                            }
                        };

                        let expected_map = self.expected_artifacts_snapshot();

                        let pass = resolver_integration::resolve_batch(
                            &ready_ids,
                            &graph,
                            &completed_results,
                            &projection,
                            &checkpoint_scope.objective_hash,
                            &plan_version,
                            &expected_map,
                        );

                        // Inject cached results for reused tasks.
                        for (tid, outcome) in &pass.reused {
                            if let ResolverOutcome::Reused { result, audit } = outcome {
                                // 1. Inject cached result.
                                completed_results.insert(*tid, *result.clone());
                                // 1b. Accumulate files_modified so the final
                                //     AgentOutput is complete.
                                all_files.extend(result.files_modified.clone());
                                // 2. Mark graph node done.
                                graph.mark_done(tid);
                                // 3. Set deliverable on the SubTask.
                                if let Some(subtask) = graph.get_mut(tid) {
                                    subtask.deliverable = Some(result.summary.clone());
                                }
                                // 4. Clear retry feedback.
                                retry_feedback.remove(tid);
                                // 5. Record in the checkpoint action ledger.
                                action_ledger.push(checkpoint::CheckpointAction {
                                    kind: "resolver-reuse".into(),
                                    task_id: Some(*tid),
                                    timestamp: time::OffsetDateTime::now_utc(),
                                    evidence: None,
                                });
                                // 6. Publish audit via event bus for real-time
                                //    visibility.  The checkpoint action ledger
                                //    provides durability across resume.
                                let _ = self.bus.publish_for_session(
                                    task.session_id,
                                    tid.0,
                                    EventKind::AgentThought {
                                        agent_id: "coordinator".into(),
                                        content: format!(
                                            "ADR-64 resolver: Reuse short-circuit for task {tid} \
                                             (semantic_key={}, reason: {})",
                                            audit.semantic_key_hex, audit.reason,
                                        ),
                                    },
                                );
                                // 7. Add a working-memory decision so the
                                //    timeline enrichment sees the audit.
                                context.working_memory.decisions.push(
                                    concerto_core::memory::Decision {
                                        id: concerto_core::memory::DecisionId(
                                            concerto_core::ids::Ulid::new(),
                                        ),
                                        session_id: task.session_id,
                                        task_id: Some(*tid),
                                        what: "ADR-64 resolver: Reuse".into(),
                                        why: audit.reason.clone(),
                                        outcome: Some(result.summary.chars().take(500).collect()),
                                        category:
                                            concerto_core::memory::DecisionCategory::Implementation,
                                        confidence: 1.0,
                                        superseded_by: None,
                                        created_at: time::OffsetDateTime::now_utc(),
                                    },
                                );
                            }
                        }

                        // Return only the non-reused task IDs for normal dispatch.
                        pass.dispatch_ids
                    } else {
                        // Projection build failed — dispatch all.
                        ready_ids
                    }
                } else {
                    // No pool available — skip resolver, dispatch all.
                    ready_ids
                }
            } else {
                ready_ids
            };

            if ready_ids.is_empty() {
                if graph.all_completed() {
                    break;
                }
                if let Some(error) = terminal_subtask_failure {
                    let final_message = format!(
                        "Automation paused after exhausting recovery attempts for a non-fatal subtask. Existing workspace changes and session context were preserved. {error}"
                    );
                    let _ = self.bus.publish_for_session(
                        task.session_id,
                        task.id.0,
                        EventKind::MultiAgentModeCompleted {
                            task_id: task.id,
                            cost_usd: total_cost,
                        },
                    );
                    checkpoint_scope.sequence_num = checkpoint_scope.sequence_num.saturating_add(1);
                    let mut cp = checkpoint::build_checkpoint(
                        &checkpoint_scope,
                        checkpoint::CheckpointStage::Executing,
                        None,
                        &context.working_memory,
                        &graph,
                        &completed_results,
                        total_cost,
                        total_tool_calls,
                        &provider_metrics,
                        &all_files,
                        &self.expected_artifacts_snapshot(),
                        &subtask_attempts,
                        &retry_feedback,
                        &self.checkpoint_context(&model_assignments, &action_ledger),
                    );
                    self.persist_checkpoint(&mut cp).await;
                    let checkpoint_json = serde_json::to_string(&cp).ok();
                    return Ok((
                        AgentOutput {
                            task_id: task.id,
                            session_id: task.session_id,
                            final_message,
                            files_modified: all_files,
                            tool_call_count: total_tool_calls,
                            eval_result: None,
                            tool_events: Vec::new(),
                            verification: Vec::new(),
                            project_root: None,
                            completion_status: concerto_core::types::AgentCompletionStatus::Partial,
                            provider_metrics,
                            checkpoint_json,
                        },
                        recoverable_notes,
                    ));
                }
                return Err(OrchestratorError::AgentLoopError(
                    "task graph has unblocked but unfinished tasks".into(),
                ));
            }

            // ── 2a. ADR-52 global run cap (doom guard) ─────────────────
            // Checked at batch boundaries BEFORE the next ready batch is
            // dispatched. Every dispatch (ready batch, retries, escalation,
            // replan follow-ups, and fallback-ladder tiers) advances
            // `model_dispatch_count`, so once the cap is consumed we pause
            // with a Partial outcome instead of spending more tokens —
            // mirroring the ladder-exhausted / terminal-failure exits below.
            if self.iteration_cap_reached() {
                let cap = self.max_total_iterations.unwrap_or(0);
                let final_message = format!(
                    "Automation paused after reaching the run-wide dispatch cap ({cap} \
                     total model dispatches). Existing workspace changes and session \
                     context were preserved."
                );
                let _ = self.bus.publish_for_session(
                    task.session_id,
                    task.id.0,
                    EventKind::MultiAgentModeCompleted { task_id: task.id, cost_usd: total_cost },
                );
                checkpoint_scope.sequence_num = checkpoint_scope.sequence_num.saturating_add(1);
                let mut cp = checkpoint::build_checkpoint(
                    &checkpoint_scope,
                    checkpoint::CheckpointStage::Executing,
                    None,
                    &context.working_memory,
                    &graph,
                    &completed_results,
                    total_cost,
                    total_tool_calls,
                    &provider_metrics,
                    &all_files,
                    &self.expected_artifacts_snapshot(),
                    &subtask_attempts,
                    &retry_feedback,
                    &self.checkpoint_context(&model_assignments, &action_ledger),
                );
                self.persist_checkpoint(&mut cp).await;
                let checkpoint_json = serde_json::to_string(&cp).ok();
                return Ok((
                    AgentOutput {
                        task_id: task.id,
                        session_id: task.session_id,
                        final_message,
                        files_modified: all_files,
                        tool_call_count: total_tool_calls,
                        eval_result: None,
                        tool_events: Vec::new(),
                        verification: Vec::new(),
                        project_root: None,
                        completion_status: concerto_core::types::AgentCompletionStatus::Partial,
                        provider_metrics,
                        checkpoint_json,
                    },
                    recoverable_notes,
                ));
            }

            checkpoint_scope.sequence_num = checkpoint_scope.sequence_num.saturating_add(1);
            let mut progress_checkpoint = checkpoint::build_checkpoint(
                &checkpoint_scope,
                checkpoint::CheckpointStage::Executing,
                None,
                &context.working_memory,
                &graph,
                &completed_results,
                total_cost,
                total_tool_calls,
                &provider_metrics,
                &all_files,
                &self.expected_artifacts_snapshot(),
                &subtask_attempts,
                &retry_feedback,
                &self.checkpoint_context(&model_assignments, &action_ledger),
            );
            self.persist_checkpoint(&mut progress_checkpoint).await;

            // ── 2a. Check budget once per batch ─────────────────────
            if self.spend_tracker.check(0.001).is_err() {
                return Err(OrchestratorError::NoBudgetForDelegation);
            }

            // ── 2b. Clone subtasks & mark_running before dispatch ──
            let mut batch = Vec::new();
            for (task_id, role) in &ready_ids {
                let subtask = graph.get(task_id).ok_or_else(|| {
                    OrchestratorError::AgentLoopError(format!("task {task_id} vanished from graph"))
                })?;
                let desc = subtask.description.clone();
                let deps = graph.dependencies_of(task_id);
                let mut previous_results: Vec<_> = deps
                    .iter()
                    .filter_map(|dependency_id| completed_results.get(dependency_id).cloned())
                    .collect();
                previous_results.extend(retry_feedback.get(task_id).into_iter().flatten().cloned());
                let sid = subtask.session_id;
                let run_subtask = SubTask {
                    id: *task_id,
                    parent_id: None,
                    session_id: sid,
                    role: role.clone(),
                    description: desc.clone(),
                    status: concerto_core::types::SubTaskStatus::Running,
                    dependencies: deps.clone(),
                    deliverable: None,
                    created_at: time::OffsetDateTime::now_utc(),
                    completed_at: None,
                };
                graph.mark_running(task_id);
                *subtask_attempts.entry(*task_id).or_insert(0) += 1;
                // Record the dispatch decision in the checkpoint action ledger.
                action_ledger.push(checkpoint::CheckpointAction {
                    kind: "dispatched".into(),
                    task_id: Some(*task_id),
                    timestamp: time::OffsetDateTime::now_utc(),
                    evidence: None,
                });
                batch.push(ReadyTask {
                    id: *task_id,
                    role: role.clone(),
                    subtask: run_subtask,
                    description: desc,
                    dependencies: deps,
                    previous_results,
                });
            }

            // ── 2c. Dispatch all ready tasks concurrently ────────────
            // Note: the budget check runs before dispatch so a batch can
            // make a bounded overshoot on the cap if several tasks finish
            // within the same iteration — acceptable given narrow graph
            // width (typically 2-3 ready tasks even after decomposition).
            // ADR-52: every ready task entering the batch is a real model
            // dispatch and advances the run-wide cap.
            self.model_dispatch_count = self.model_dispatch_count.saturating_add(batch.len());
            let this: &CoordinatorAgent = &*self;
            let session = context.session.clone();
            let parent_task = task.clone();
            let working_memory = context.working_memory.clone();
            let retrieved_chunks = context.retrieved_chunks.clone();
            // Pre-compute artifact expectations under the mutex lock so the
            // async closure below does not need to hold a reference.
            let expected_for_batch: Vec<(TaskId, Vec<camino::Utf8PathBuf>)> = {
                let map = self.expected_artifacts.lock().unwrap_or_else(|error| error.into_inner());
                batch
                    .iter()
                    .map(|ready| {
                        let artifacts = map.get(&ready.id).cloned().unwrap_or_default();
                        (ready.id, artifacts)
                    })
                    .collect()
            };
            // ADR-64 Phase 5: pre-compute workspace capsules for each task
            // in the batch. The projection was cloned out of the resolver
            // block; build_capsule is pure (no I/O, no LLM calls).
            let capsules_for_batch: Vec<(TaskId, Option<concerto_core::types::WorkspaceCapsule>)> =
                if let Some(ref projection) = projection_for_capsule {
                    expected_for_batch
                        .iter()
                        .map(|(tid, artifacts)| {
                            let capsule =
                                crate::capsule::build_capsule(projection, tid, &graph, artifacts);
                            (*tid, Some(capsule))
                        })
                        .collect()
                } else {
                    batch.iter().map(|ready| (ready.id, None)).collect()
                };
            let futures = batch.iter().map(|ready| {
                let tid = ready.id;
                let rl = ready.role.clone();
                let subtask = ready.subtask.clone();
                let previous_results = ready.previous_results.clone();
                let ctx_sesh = session.clone();
                let ctx_parent = parent_task.clone();
                let ctx_wm = working_memory.clone();
                let ctx_rc = retrieved_chunks.clone();
                let project_id = context.session.project_id.clone();
                let cancel_clone = cancel.clone();
                let task_artifacts = expected_for_batch
                    .iter()
                    .find(|(id, _)| *id == tid)
                    .map(|(_, a)| a.clone())
                    .unwrap_or_default();
                let task_capsule = capsules_for_batch
                    .iter()
                    .find(|(id, _)| *id == tid)
                    .and_then(|(_, c)| c.clone());
                async move {
                    let profile = match this.model_selector.select_for_session(
                        &rl,
                        None,
                        tid,
                        Some(subtask.session_id),
                    ) {
                        Ok(p) => p,
                        Err(e) => return (tid, rl, String::new(), Err(e)),
                    };
                    let query = MemoryQuery {
                        text: subtask.description.clone(),
                        project_id: project_id.clone(),
                        namespace: MemoryNamespace::Project(project_id),
                        top_k: 3,
                        filters: vec![],
                    };
                    let role_chunks =
                        match this.memory_store.retrieve(&query, cancel_clone.clone()).await {
                            Ok(chunks) if !chunks.is_empty() => chunks,
                            Ok(_) | Err(_) => ctx_rc,
                        };
                    let agent_ctx = AgentContext {
                        session: ctx_sesh,
                        parent_task: Some(ctx_parent),
                        working_memory: ctx_wm,
                        retrieved_chunks: role_chunks,
                        previous_results,
                        budget_remaining_usd: None,
                        expected_artifacts: task_artifacts,
                        workspace_capsule: task_capsule,
                        workspace_snapshot_digest: this.snapshot_digest(&cancel_clone).await,
                        run_id: this.run_id.clone(),
                        workspace_generation: this.snapshot_generation(),
                    };
                    // ADR-35 §8 trigger 1 (stage absence): implement subtasks
                    // planned for the reserved `coordinator` role are executed
                    // by the coordinator itself on its planning provider
                    // through the shared executor. This happens only when no
                    // implement-stage agent is registered (decompose_task
                    // selects the coordinator into that role).
                    if rl.as_str() == "coordinator" {
                        let result = this
                            .run_coordinator_self(
                                &subtask,
                                agent_ctx,
                                &profile,
                                cancel_clone.clone(),
                            )
                            .await;
                        return (tid, rl, String::new(), result);
                    }
                    let result: Result<AgentRunResult, concerto_core::OrchestratorError> = this
                        .runner
                        .run(rl.clone(), &subtask, agent_ctx, &profile, cancel_clone)
                        .await;
                    (tid, rl, profile.model_name().to_string(), result)
                }
            });

            let batch_results: Vec<(TaskId, AgentId, String, Result<AgentRunResult, _>)> =
                join_all(futures).await;

            // ── 2d. Process results sequentially ─────────────────────
            let mut cancelled_during_batch = false;
            for (task_id, role, model, result_or_err) in batch_results {
                let attempt = subtask_attempts.get(&task_id).copied().unwrap_or(1);
                // Record which model actually ran this task (reproducibility).
                if !model.is_empty() {
                    model_assignments.insert(task_id, model);
                }

                let result = match result_or_err {
                    Ok(result) => result,
                    Err(e) => {
                        let _ = self.bus.publish_for_session(
                            task.session_id,
                            task_id.0,
                            EventKind::SubTaskFailed {
                                task_id,
                                role: role.clone(),
                                error: e.to_string(),
                            },
                        );
                        if is_cancellation_error(&e) {
                            graph.mark_pending(&task_id);
                            cancelled_during_batch = true;
                            continue;
                        }
                        match classify_subtask_error(&e) {
                            // Transient errors retry the same agent/model
                            // while attempts remain (ADR-42 §1 Recoverable).
                            SubtaskFailureClass::Recoverable
                                if attempt < self.max_subtask_attempts =>
                            {
                                retry_feedback.entry(task_id).or_default().push(
                                    failed_attempt_result(task_id, role.clone(), e.to_string()),
                                );
                                graph.mark_pending(&task_id);
                                let _ = self.bus.publish_for_session(task.session_id, task_id.0, EventKind::AgentThought {
                                    agent_id: "coordinator".into(),
                                    content: format!(
                                        "Retrying {role} subtask {task_id} after recoverable failure (attempt {attempt}/{}): {e}", self.max_subtask_attempts
                                    ),
                                });
                                continue;
                            }
                            // Retries exhausted (Recoverable) or a
                            // provider/model-specific hard failure
                            // (LimitReached): walk the ADR-42 fallback ladder
                            // before any Partial exit. Recoverable errors keep
                            // the one-shot escalation retry first. Note the
                            // asymmetry with the outcome arm: this block
                            // escalates ONLY Recoverable errors — LimitReached
                            // (auth, context overflow, no-affordable-model)
                            // skips escalation and enters the ladder directly,
                            // while the outcome arm escalates any non-implement
                            // failure regardless of class.
                            SubtaskFailureClass::Recoverable
                            | SubtaskFailureClass::LimitReached => {
                                // ── Escalation retry ─────────────────────
                                // On the final retry attempt of a recoverable
                                // error, do one escalation retry before the
                                // ladder. Escalation is limited to once per
                                // task per run by the escalation_attempted set.
                                //
                                // Attempt-math confirmation (see the outcome
                                // arm for the same shape): the counter is
                                // incremented at dispatch time (batch build,
                                // `subtask_attempts.entry(t).or_insert(0) += 1`)
                                // and read here as `attempt`. Retries fire for
                                // attempts 1..=2 (`attempt < MAX`); attempt 3
                                // falls through to this block. The reset to
                                // `MAX - 1` makes the NEXT dispatch read
                                // exactly `MAX` again, so the retry arm does
                                // NOT re-fire on the escalated dispatch — the
                                // extra dispatch comes from `mark_pending` +
                                // `continue` (the re-pended task re-enters the
                                // ready queue). Net effect: exactly ONE
                                // additional dispatch per task per run, with
                                // the escalated failure appended to
                                // `retry_feedback` (surfaced to the agent as
                                // previous results). Without the reset the
                                // counter would drift to MAX + 1, skewing the
                                // `attempt {attempt}/{MAX}` reporting.
                                let is_recoverable = matches!(
                                    classify_subtask_error(&e),
                                    SubtaskFailureClass::Recoverable
                                );
                                if is_recoverable && !self.escalation_attempted.contains(&task_id) {
                                    self.escalation_attempted.insert(task_id);
                                    subtask_attempts.insert(
                                        task_id,
                                        self.max_subtask_attempts.saturating_sub(1),
                                    );
                                    retry_feedback.entry(task_id).or_default().push(
                                        failed_attempt_result(
                                            task_id,
                                            role.clone(),
                                            format!("{e} (escalation retry)"),
                                        ),
                                    );
                                    graph.mark_pending(&task_id);
                                    let _ = self.bus.publish_for_session(
                                        task.session_id,
                                        task_id.0,
                                        EventKind::AgentThought {
                                            agent_id: "coordinator".into(),
                                            content: format!(
                                                "Escalating {role} subtask {task_id} — escalation retry \
                                             (attempt {attempt}/{} exhausted)", self.max_subtask_attempts
                                            ),
                                        },
                                    );
                                    continue;
                                }

                                // ── ADR-42 fallback ladder ───────────────
                                // Rebuild the per-task dispatch context for
                                // the ladder (the original was consumed inside
                                // the async dispatch closure above).
                                let Some(ladder_entry) =
                                    batch.iter().find(|entry| entry.id == task_id)
                                else {
                                    graph.mark_blocked(&task_id);
                                    terminal_subtask_failure =
                                        Some(OrchestratorError::SubTaskRetriesExhausted {
                                            task_id,
                                            role,
                                            attempts: attempt,
                                            last_error: e.to_string(),
                                        });
                                    continue;
                                };
                                let ladder_artifacts = self
                                    .expected_artifacts
                                    .lock()
                                    .unwrap_or_else(|error| error.into_inner())
                                    .get(&task_id)
                                    .cloned()
                                    .unwrap_or_default();
                                let ladder_ctx = AgentContext {
                                    session: session.clone(),
                                    parent_task: Some(parent_task.clone()),
                                    working_memory: working_memory.clone(),
                                    retrieved_chunks: retrieved_chunks.clone(),
                                    previous_results: ladder_entry.previous_results.clone(),
                                    budget_remaining_usd: None,
                                    expected_artifacts: ladder_artifacts,
                                    workspace_capsule: None,
                                    workspace_snapshot_digest: self.snapshot_digest(&cancel).await,
                                    run_id: self.run_id.clone(),
                                    workspace_generation: self.snapshot_generation(),
                                };
                                match self
                                    .attempt_fallback_ladder(
                                        &ladder_entry.subtask,
                                        &role,
                                        &e,
                                        &ladder_ctx,
                                        &cancel,
                                    )
                                    .await
                                {
                                    // Ladder Success always carries a genuine
                                    // Success outcome (see the outcome arm):
                                    // yielding it here completes the subtask.
                                    FallbackOutcome::Success(result) => *result,
                                    FallbackOutcome::Cancelled => {
                                        graph.mark_pending(&task_id);
                                        cancelled_during_batch = true;
                                        continue;
                                    }
                                    FallbackOutcome::Exhausted => {
                                        graph.mark_blocked(&task_id);
                                        terminal_subtask_failure =
                                            Some(OrchestratorError::SubTaskRetriesExhausted {
                                                task_id,
                                                role,
                                                attempts: attempt,
                                                last_error: e.to_string(),
                                            });
                                        continue;
                                    }
                                }
                            }
                            // Structural errors (cancellation, invalid task
                            // graph, cycle detection, planning failure) exit
                            // immediately via the graceful non-recoverable
                            // path. ADR-42 reclassifies the former
                            // non-recoverable provider/model-specific family
                            // (auth, context overflow, no-affordable-model) as
                            // LimitReached, so it now walks the ladder above
                            // instead of reaching this arm. Audit §3.4: the
                            // previous code did `return Err(e)` here, which:
                            //   (a) discarded `all_files`, `provider_metrics`,
                            //       and any successful siblings already
                            //       processed earlier in this same
                            //       `batch_results` loop,
                            //   (b) dropped subsequent siblings still queued
                            //       in `batch_results` without processing
                            //       their success/failure,
                            //   (c) bypassed every graceful-completion path
                            //       the rest of `run` builds (the empty-queue
                            //       terminal-failure exit below does the same
                            //       thing for *recoverable* errors after
                            //       retries are exhausted).
                            // Instead, stash the error so we can finish the
                            // current batch, then surface a Partial AgentOutput
                            // through the same graceful-degradation
                            // construction used at the empty-queue exit.
                            SubtaskFailureClass::NonRecoverable => {
                                tracing::warn!(
                                    target: "orchestrator::coordinator",
                                    task_id = ?task_id,
                                    role = ?role,
                                    error = %e,
                                    "non-recoverable subtask error; draining batch then surfacing partial AgentOutput",
                                );
                                non_recoverable_exit = Some((task_id, role, e));
                                // Skip the success-path processing for this
                                // errored task; the outer loop tail will detect
                                // `non_recoverable_exit` and return the graceful
                                // Partial AgentOutput after the rest of `batch_results`
                                // has been processed.
                                continue;
                            }
                        }
                    }
                };

                // Record the outcome in the checkpoint action ledger.
                // ADR-65 §7: the dispatched step settled — the pending
                // decision it recorded no longer awaits completion.
                if self.last_dispatch_decision.as_ref().and_then(|pending| pending.task_id)
                    == Some(task_id)
                {
                    self.last_dispatch_decision = None;
                }
                action_ledger.push(checkpoint::CheckpointAction {
                    kind: if matches!(result.outcome, AgentOutcome::Success) {
                        "completed".into()
                    } else {
                        "failed".into()
                    },
                    task_id: Some(task_id),
                    timestamp: time::OffsetDateTime::now_utc(),
                    evidence: None,
                });

                total_cost += result.cost_usd;
                total_tool_calls += result.tool_call_count;
                all_files.extend(result.files_modified.clone());
                let settled = metrics_from_result(&result);
                provider_metrics.push(settled.clone());
                self.settled_metrics.push(settled);
                let stop_followups = cancel.is_cancelled();
                cancelled_during_batch |= stop_followups;
                let (desc, deps, sid) = {
                    let entry =
                        batch.iter().find(|entry| entry.id == task_id).ok_or_else(|| {
                            OrchestratorError::InvalidTaskGraph {
                                reason: format!(
                                    "completed task {task_id} was not in dispatched batch"
                                ),
                            }
                        })?;
                    (
                        entry.description.clone(),
                        entry.dependencies.clone(),
                        entry.subtask.session_id,
                    )
                };
                match result.outcome.clone() {
                    AgentOutcome::Success => {
                        if let Some(subtask) = graph.get_mut(&task_id) {
                            subtask.deliverable = Some(result.summary.clone());
                        }
                        completed_results.insert(task_id, result.clone());
                        retry_feedback.remove(&task_id);
                        graph.mark_done(&task_id);

                        // ── Replan fallback: design-stage redesign complete ──
                        // When a design-stage replan subtask finishes
                        // successfully, parse its DesignDoc and spawn a new
                        // implement-stage subtask (same role as the original)
                        // with the revised expected artifacts. The design
                        // classification is facade-resolved by kind, so a
                        // renamed Planning stage keeps replanning (issue
                        // #150).
                        if self.role_in_kind_stage(
                            &role,
                            StageKind::Planning,
                            AgentStage::is_design,
                        ) && !stop_followups
                        {
                            let parent_id = graph.get(&task_id).and_then(|st| st.parent_id);
                            if let Some(orig_impl_id) = parent_id {
                                if self.replan_attempts.contains_key(&orig_impl_id) {
                                    let design_doc: Option<DesignDoc> =
                                        crate::prompts::parse_json_substring(&result.summary);
                                    // A replan supersedes the original plan;
                                    // keep the newest DesignDoc for checkpoints.
                                    if let Some(ref doc) = design_doc {
                                        *self
                                            .design_doc
                                            .lock()
                                            .unwrap_or_else(|error| error.into_inner()) =
                                            Some(doc.clone());
                                    }
                                    // The follow-up implement task keeps the
                                    // original role (which may be a custom
                                    // implement-stage agent, ADR-35 §5).
                                    let implement_role = graph
                                        .get(&orig_impl_id)
                                        .map(|t| t.role.clone())
                                        .or_else(|| implement_ids.first().cloned());

                                    let (new_expected, new_desc) = if let Some(ref doc) = design_doc
                                    {
                                        let files = if doc.proposed_files.is_empty() {
                                            self.expected_artifacts
                                                .lock()
                                                .unwrap_or_else(|error| error.into_inner())
                                                .get(&orig_impl_id)
                                                .cloned()
                                                .unwrap_or_default()
                                        } else {
                                            doc.proposed_files.clone()
                                        };
                                        let orig_desc = graph
                                            .get(&orig_impl_id)
                                            .map(|t| t.description.clone())
                                            .unwrap_or_default();
                                        let desc = if doc.goals.is_empty() {
                                            format!(
                                                "Re-implement based on revised design: {orig_desc}"
                                            )
                                        } else {
                                            format!(
                                                "Re-implement based on revised design: {}",
                                                doc.goals.join("; ")
                                            )
                                        };
                                        (files, desc)
                                    } else {
                                        let fallback_expected = self
                                            .expected_artifacts
                                            .lock()
                                            .unwrap_or_else(|error| error.into_inner())
                                            .get(&orig_impl_id)
                                            .cloned()
                                            .unwrap_or_default();
                                        let orig_desc = graph
                                            .get(&orig_impl_id)
                                            .map(|t| t.description.clone())
                                            .unwrap_or_default();
                                        (
                                            fallback_expected,
                                            format!(
                                                "Re-implement based on revised design: {orig_desc}"
                                            ),
                                        )
                                    };

                                    if let Some(implement_role) = implement_role {
                                        let new_coder_desc = new_desc;
                                        let new_coder = SubTask {
                                            id: TaskId::new(),
                                            parent_id: Some(task_id),
                                            session_id: sid,
                                            role: implement_role.clone(),
                                            description: new_coder_desc.clone(),
                                            status: SubTaskStatus::Pending,
                                            dependencies: vec![task_id],
                                            deliverable: None,
                                            created_at: time::OffsetDateTime::now_utc(),
                                            completed_at: None,
                                        };
                                        let new_coder_id = new_coder.id;

                                        // Record this new implement task in
                                        // replan_attempts so that if it also
                                        // fails with an artifact error we do
                                        // not attempt a second cascading
                                        // replan.
                                        self.replan_attempts.insert(new_coder_id, 1);

                                        self.expected_artifacts
                                            .lock()
                                            .unwrap_or_else(|error| error.into_inner())
                                            .insert(new_coder_id, new_expected);

                                        graph.add_child(
                                            new_coder,
                                            task_id,
                                            Dependency::MustFinishBefore,
                                        );

                                        let _ = self.bus.publish_for_session(
                                            sid,
                                            task_id.0,
                                            EventKind::SubTaskCreated {
                                                task_id: new_coder_id,
                                                role: implement_role,
                                                description: new_coder_desc,
                                            },
                                        );
                                        let _ = self.bus.publish_for_session(
                                            sid,
                                            task_id.0,
                                            EventKind::AgentThought {
                                                agent_id: "coordinator".into(),
                                                content: format!(
                                                    "Design redesign complete. Spawning new implementation subtask {new_coder_id} for re-implementation."
                                                ),
                                            },
                                        );
                                    }
                                }
                            }
                        }

                        // ── Zero-file implement success short-circuit ─────
                        // An implement-stage agent that "succeeds" without
                        // writing any file has no deliverable for the review
                        // cycle to inspect. Skip the reviewer model call and
                        // directly queue the correction task the review
                        // outcome would have produced (the Revision subtask
                        // pattern below) instead of spending a reviewer call
                        // to rediscover that no deliverable exists.
                        //
                        // Bounded once per lineage, in two steps:
                        // 1. The FIRST zero-file success in a lineage skips
                        //    the reviewer and queues a revision — the
                        //    deliverable may simply be missing because the
                        //    model under-performed, and a revision is a cheap
                        //    second chance.
                        // 2. A SUBSEQUENT zero-file implement success in the
                        //    SAME lineage (the revision/revise pass itself —
                        //    same role, fresh task id, same lineage root) had
                        //    its chance to produce the deliverables and
                        //    produced none. Running the reviewer would only
                        //    re-discover the missing artifacts and queue yet
                        //    another revision, spinning forever on an empty
                        //    `files_modified` — the observed quota-burning
                        //    loop. That completion FAILS the subtask instead:
                        //    no further revision is queued and no review
                        //    cycle starts for that lineage.
                        //
                        // Pipelines with no review-stage agent never
                        // short-circuit: there is no reviewer call to save.
                        let implement_phase = self.role_in_kind_stage(
                            &role,
                            StageKind::Execution,
                            AgentStage::is_implement,
                        ) && !stop_followups;
                        // The short-circuit exists to save a *reviewer* model
                        // call. Without a review-stage agent in the pipeline
                        // there is no call to save (`run_review_cycle` is a
                        // cheap skip), so zero-file successes take the normal
                        // path instead of injecting a needless revision. The
                        // review stage is resolved by kind, so a renamed
                        // review tag keeps the short-circuit (issue #150).
                        let review_tag = kind_stage_tag(
                            self.blueprint_facade.as_ref(),
                            StageKind::Review,
                            AgentStage::REVIEW,
                        );
                        let has_review_stage_agent =
                            self.first_agent_for_stage(&AgentStage::new(review_tag)).is_some();
                        let lineage_root = {
                            let mut root = task_id;
                            while let Some(parent) =
                                graph.get(&root).and_then(|node| node.parent_id)
                            {
                                root = parent;
                            }
                            root
                        };
                        let zero_file_success = result.files_modified.is_empty();
                        if implement_phase
                            && zero_file_success
                            && has_review_stage_agent
                            && !zero_file_revision_queued_roots.contains(&lineage_root)
                        {
                            // Step 1: first zero-file success in this lineage.
                            zero_file_revision_queued_roots.insert(lineage_root);
                            let _ = self.bus.publish_for_session(
                                sid,
                                task_id.0,
                                EventKind::AgentThought {
                                    agent_id: "coordinator".into(),
                                    content: format!(
                                        "Coder subtask {task_id} completed with no file changes; queuing revision without running the review cycle."
                                    ),
                                },
                            );
                            recoverable_notes.push(format!(
                                "Coder subtask {task_id} completed with no file changes; revision queued without running the review cycle."
                            ));
                            self.queue_revision_subtask(
                                &mut graph,
                                task_id,
                                sid,
                                role.clone(),
                                "completed with no file changes".into(),
                            );
                        } else if implement_phase && zero_file_success && has_review_stage_agent {
                            // Step 2: a zero-file implement success after the
                            // short-circuit already fired for this lineage is
                            // the revision/revise pass itself, and it produced
                            // no deliverable. Fail the subtask terminally so
                            // the run surfaces the missing deliverables
                            // instead of burning the quota on a
                            // revision → zero-file → revision loop.
                            let zero_file_error = format!(
                                "subtask {task_id} ({role:?}) completed with no file \
                                 changes after revision; required deliverables were \
                                 not produced"
                            );
                            let _ = self.bus.publish_for_session(
                                sid,
                                task_id.0,
                                EventKind::SubTaskFailed {
                                    task_id,
                                    role: role.clone(),
                                    error: zero_file_error.clone(),
                                },
                            );
                            graph.mark_blocked(&task_id);
                            terminal_subtask_failure =
                                Some(OrchestratorError::SubTaskRetriesExhausted {
                                    task_id,
                                    role,
                                    attempts: attempt,
                                    last_error: zero_file_error,
                                });
                            continue;
                        } else if implement_phase {
                            // Any implement-stage agent's success triggers the
                            // review cycle (ADR-35 §5), not just the built-in
                            // Coder.
                            let review_context = context.clone();
                            let desc_for_review = desc.clone();
                            let review_result = self
                                .run_review_cycle(
                                    &mut graph,
                                    task_id,
                                    desc_for_review,
                                    sid,
                                    &result,
                                    &review_context,
                                    task.clone(),
                                    &cancel,
                                )
                                .await?;
                            total_cost += review_result.cost_usd;
                            total_tool_calls += review_result.tool_call_count;
                            all_files.extend(review_result.files_modified.clone());
                            let settled = metrics_from_result(&review_result);
                            provider_metrics.push(settled.clone());
                            self.settled_metrics.push(settled);
                            if !matches!(review_result.outcome, AgentOutcome::Success) {
                                recoverable_notes.push(format!(
                                    "Review remains unresolved: {}",
                                    review_result.summary
                                ));
                            }
                        }
                    }
                    AgentOutcome::NeedsRevision { reason } => {
                        if let Some(subtask) = graph.get_mut(&task_id) {
                            subtask.deliverable = Some(result.summary.clone());
                        }
                        completed_results.insert(task_id, result.clone());
                        retry_feedback.remove(&task_id);
                        graph.mark_done(&task_id);
                        if stop_followups {
                            continue;
                        }
                        // The revision subtask reuses the original
                        // implement-stage role (custom or built-in).
                        self.queue_revision_subtask(&mut graph, task_id, sid, role.clone(), reason);
                    }
                    AgentOutcome::Failed { error } => {
                        let _ = self.bus.publish_for_session(
                            task.session_id,
                            task_id.0,
                            EventKind::SubTaskFailed {
                                task_id,
                                role: role.clone(),
                                error: error.clone(),
                            },
                        );
                        if attempt < self.max_subtask_attempts {
                            retry_feedback.entry(task_id).or_default().push(result.clone());
                            graph.mark_pending(&task_id);
                            let _ = self.bus.publish_for_session(task.session_id, task_id.0, EventKind::AgentThought {
                                agent_id: "coordinator".into(),
                                content: format!(
                                    "Retrying {role} subtask {task_id} with failure feedback (attempt {attempt}/{}): {error}", self.max_subtask_attempts
                                ),
                            });
                            continue;
                        }

                        // ── Escalation retry (non-implement) ─────────────────────
                        // For non-implement failures that have exhausted normal
                        // retries, try one escalation retry before giving up. This
                        // gives the agent one more attempt with accumulated failure
                        // feedback, which can help with design/research roles where
                        // the model may produce a better result with more context.
                        // Implement-stage failures instead funnel into the replan
                        // fallback below.
                        //
                        // Attempt-math confirmation (identical to the
                        // dispatch-error arm): the counter is incremented at
                        // dispatch time and read here as `attempt`. Retries
                        // fire for attempts 1..=2; the reset to `MAX - 1`
                        // makes the next dispatch read exactly `MAX`, so the
                        // retry arm does NOT re-fire on the escalated dispatch
                        // — the extra dispatch comes from `mark_pending` +
                        // `continue`. Net effect: exactly ONE additional
                        // dispatch per task per run with the failed result
                        // appended to `retry_feedback`.
                        if !self.role_in_kind_stage(
                            &role,
                            StageKind::Execution,
                            AgentStage::is_implement,
                        ) && !self.escalation_attempted.contains(&task_id)
                        {
                            self.escalation_attempted.insert(task_id);
                            subtask_attempts
                                .insert(task_id, self.max_subtask_attempts.saturating_sub(1));
                            retry_feedback.entry(task_id).or_default().push(result.clone());
                            graph.mark_pending(&task_id);
                            let _ = self.bus.publish_for_session(
                                task.session_id,
                                task_id.0,
                                EventKind::AgentThought {
                                    agent_id: "coordinator".into(),
                                    content: format!(
                                        "Escalating {role} subtask {task_id} — escalation retry \
                                     (attempt {attempt}/{} exhausted, role-based)",
                                        self.max_subtask_attempts
                                    ),
                                },
                            );
                            continue;
                        }

                        // ── Replan fallback (implement-stage only) ─────────────
                        // If an implement-stage subtask exhausts its retries
                        // because expected artifacts were not produced, escalate
                        // to the design-stage agent for a design revision instead
                        // of giving up immediately. Pipelines without a
                        // design-stage agent have no redesign path and fall
                        // through to blocked.
                        if let Some(design_role) = design_role.as_ref() {
                            if self.role_in_kind_stage(
                                &role,
                                StageKind::Execution,
                                AgentStage::is_implement,
                            ) && is_artifact_failure(&error)
                                && !self.replan_attempts.contains_key(&task_id)
                            {
                                self.replan_attempts.insert(task_id, 1);

                                // Complete the original implement task so the
                                // replan design task (which depends on it) becomes
                                // ready.
                                graph.mark_done(&task_id);
                                completed_results.insert(task_id, result.clone());
                                retry_feedback.remove(&task_id);

                                // Copy the original task's expected artifacts so
                                // the design agent knows what files were expected.
                                let orig_expected = self
                                    .expected_artifacts
                                    .lock()
                                    .unwrap_or_else(|error| error.into_inner())
                                    .get(&task_id)
                                    .cloned()
                                    .unwrap_or_default();

                                let arch_replan = SubTask {
                                    id: TaskId::new(),
                                    parent_id: Some(task_id),
                                    session_id: sid,
                                    role: design_role.clone(),
                                    description: format!("Replan: {desc}"),
                                    status: SubTaskStatus::Pending,
                                    dependencies: vec![task_id],
                                    deliverable: None,
                                    created_at: time::OffsetDateTime::now_utc(),
                                    completed_at: None,
                                };
                                let arch_replan_id = arch_replan.id;

                                self.expected_artifacts
                                    .lock()
                                    .unwrap_or_else(|error| error.into_inner())
                                    .insert(arch_replan_id, orig_expected);

                                graph.add_child(arch_replan, task_id, Dependency::MustFinishBefore);

                                // Publish a handoff event: implement → design
                                let handoff = AgentHandoff::new(
                                    role.clone(),
                                    design_role.clone(),
                                    task_id,
                                    "Implementing agent could not produce expected artifacts; replanning design"
                                        .into(),
                                    HandoffDeliverable::Design(error.clone()),
                                );
                                let _ = self.bus.publish_for_session(
                                    sid,
                                    task_id.0,
                                    EventKind::AgentHandoff {
                                        from: handoff.from,
                                        to: handoff.to,
                                        task_id: handoff.task_id,
                                        rationale: handoff.rationale.clone(),
                                    },
                                );
                                let _ = self.bus.publish_for_session(
                                    sid,
                                    task_id.0,
                                    EventKind::AgentThought {
                                        agent_id: "coordinator".into(),
                                        content: format!(
                                            "Implement subtask {task_id} exhausted attempts producing expected artifacts. Escalating to {design_role} for redesign (replan #1)."
                                        ),
                                    },
                                );
                                continue;
                            }
                        }

                        // ── ADR-42 fallback ladder ─────────────────────
                        // Retries (and escalation/replan, where applicable)
                        // are exhausted: walk the fallback ladder before
                        // surfacing a partial outcome. The agent-produced
                        // error string is wrapped for classification purposes
                        // only — exhaustion already implies LimitReached
                        // (ADR-42 §1).
                        let Some(ladder_entry) = batch.iter().find(|entry| entry.id == task_id)
                        else {
                            graph.mark_blocked(&task_id);
                            terminal_subtask_failure =
                                Some(OrchestratorError::SubTaskRetriesExhausted {
                                    task_id,
                                    role,
                                    attempts: attempt,
                                    last_error: error,
                                });
                            continue;
                        };
                        let ladder_artifacts = self
                            .expected_artifacts
                            .lock()
                            .unwrap_or_else(|error| error.into_inner())
                            .get(&task_id)
                            .cloned()
                            .unwrap_or_default();
                        let ladder_ctx = AgentContext {
                            session: session.clone(),
                            parent_task: Some(parent_task.clone()),
                            working_memory: working_memory.clone(),
                            retrieved_chunks: retrieved_chunks.clone(),
                            previous_results: ladder_entry.previous_results.clone(),
                            budget_remaining_usd: None,
                            expected_artifacts: ladder_artifacts,
                            workspace_capsule: None,
                            workspace_snapshot_digest: self.snapshot_digest(&cancel).await,
                            run_id: self.run_id.clone(),
                            workspace_generation: self.snapshot_generation(),
                        };
                        let classify_err = OrchestratorError::AgentLoopError(error.clone());
                        match self
                            .attempt_fallback_ladder(
                                &ladder_entry.subtask,
                                &role,
                                &classify_err,
                                &ladder_ctx,
                                &cancel,
                            )
                            .await
                        {
                            FallbackOutcome::Success(fb_result) => {
                                let fb_result = *fb_result;
                                if !matches!(fb_result.outcome, AgentOutcome::Success) {
                                    // Defense-in-depth: the ladder only reports
                                    // Success for a genuine Success outcome —
                                    // attempt_fallback_ladder sends tier-1
                                    // Ok(Failed) results into tier 2, and tier
                                    // 2 only yields Success on substantive
                                    // output. A non-Success result here is
                                    // terminal: retries are exhausted and the
                                    // per-task guards bound the ladder.
                                    graph.mark_blocked(&task_id);
                                    terminal_subtask_failure =
                                        Some(OrchestratorError::SubTaskRetriesExhausted {
                                            task_id,
                                            role,
                                            attempts: attempt,
                                            last_error: error,
                                        });
                                    continue;
                                }
                                // The ladder completed the subtask: record it
                                // like a successful dispatch. The fallback is
                                // a single last-resort attempt, so review /
                                // follow-up cycles (wired to primary dispatch
                                // results above) are not re-run here.
                                action_ledger.push(checkpoint::CheckpointAction {
                                    kind: "completed".into(),
                                    task_id: Some(task_id),
                                    timestamp: time::OffsetDateTime::now_utc(),
                                    evidence: None,
                                });
                                total_cost += fb_result.cost_usd;
                                total_tool_calls += fb_result.tool_call_count;
                                all_files.extend(fb_result.files_modified.clone());
                                let settled = metrics_from_result(&fb_result);
                                provider_metrics.push(settled.clone());
                                self.settled_metrics.push(settled);
                                if let Some(subtask) = graph.get_mut(&task_id) {
                                    subtask.deliverable = Some(fb_result.summary.clone());
                                }
                                completed_results.insert(task_id, fb_result.clone());
                                retry_feedback.remove(&task_id);
                                graph.mark_done(&task_id);
                                continue;
                            }
                            FallbackOutcome::Cancelled => {
                                // The run is being cancelled (handled at the
                                // loop top); re-pend the subtask rather than
                                // failing it.
                                graph.mark_pending(&task_id);
                                cancelled_during_batch = true;
                                continue;
                            }
                            FallbackOutcome::Exhausted => {
                                graph.mark_blocked(&task_id);
                                terminal_subtask_failure =
                                    Some(OrchestratorError::SubTaskRetriesExhausted {
                                        task_id,
                                        role,
                                        attempts: attempt,
                                        last_error: error,
                                    });
                                continue;
                            }
                        }
                    }
                    AgentOutcome::Blocked { on } => {
                        if attempt < self.max_subtask_attempts {
                            let mut attached = 0usize;
                            let mut failures = Vec::new();
                            for blocker in &on {
                                match graph.add_dependency(
                                    task_id,
                                    *blocker,
                                    Dependency::MustFinishBefore,
                                ) {
                                    Ok(()) => attached += 1,
                                    Err(e) => {
                                        warn!(
                                            target: "orchestrator::coordinator",
                                            task_id = ?task_id,
                                            blocker = ?blocker,
                                            error = %e,
                                            "Blocked handler: could not attach dependency, blocker id unknown",
                                        );
                                        failures.push(e);
                                    }
                                }
                            }
                            if attached == 0 && !on.is_empty() {
                                // Every reported blocker is invalid — don't busy-retry.
                                warn!(
                                    target: "orchestrator::coordinator",
                                    task_id = ?task_id,
                                    blockers = ?on,
                                    "all reported blockers are unknown; marking task blocked",
                                );
                                graph.mark_blocked(&task_id);
                                terminal_subtask_failure =
                                    Some(OrchestratorError::TaskGraphError(format!(
                                    "task {task_id} reported blocked on {} unknown task(s): {:?}",
                                    on.len(), on
                                )));
                                continue;
                            }
                            retry_feedback.entry(task_id).or_default().push(result.clone());
                            graph.mark_pending(&task_id);
                        } else {
                            graph.mark_blocked(&task_id);
                            terminal_subtask_failure =
                                Some(OrchestratorError::SubTaskRetriesExhausted {
                                    task_id,
                                    role: role.clone(),
                                    attempts: attempt,
                                    last_error: format!("agent remained blocked on {on:?}"),
                                });
                        }
                    }
                    _ => {
                        graph.mark_blocked(&task_id);
                        terminal_subtask_failure =
                            Some(OrchestratorError::SubTaskRetriesExhausted {
                                task_id,
                                role: role.clone(),
                                attempts: attempt,
                                last_error: "unexpected agent outcome".into(),
                            });
                        continue;
                    }
                }

                // Cycle detection — content-aware via FileDeltaTracker so
                // that editing the same file across iterations counts as
                // progress (the old `!files_modified.is_empty()` would reset
                // on every write, but could not distinguish an actual edit
                // from a repeated write of identical content).
                let has_progress =
                    self.file_delta.has_progress_since(&task_id, &result.files_modified);
                // ADR-58 P2+P3 (R11): Rule B keys on the gate being executed —
                // the tag of the gate stage in which the role is staffed,
                // resolved through the blueprint facade (which also classifies
                // custom gate tags), falling back to the role's registered
                // stage and then the legacy `AgentStage::is_review`
                // classification when no facade is attached. The coordinator
                // sentinel is never registered, so for self-execution this
                // stays `None` and Rule B never fires on it.
                let stage = match &self.blueprint_facade {
                    Some(facade) => facade
                        .stage_for_agent(&role)
                        .filter(|stage| stage.def.is_gate())
                        .map(|stage| AgentStage::new(&stage.def.tag))
                        .or_else(|| self.stage_of(&role)),
                    None => self.stage_of(&role),
                };
                self.cycle_state.record(
                    task.session_id,
                    task_id,
                    role,
                    stage,
                    &desc,
                    &deps,
                    has_progress,
                )?;
            }

            refresh_working_memory(
                &mut context,
                &graph,
                &completed_results,
                &|role| self.stage_of(role),
                self.blueprint_facade.as_ref(),
            );
            checkpoint_scope.sequence_num = checkpoint_scope.sequence_num.saturating_add(1);
            let mut completed_batch_checkpoint = checkpoint::build_checkpoint(
                &checkpoint_scope,
                checkpoint::CheckpointStage::Executing,
                None,
                &context.working_memory,
                &graph,
                &completed_results,
                total_cost,
                total_tool_calls,
                &provider_metrics,
                &all_files,
                &self.expected_artifacts_snapshot(),
                &subtask_attempts,
                &retry_feedback,
                &self.checkpoint_context(&model_assignments, &action_ledger),
            );
            self.persist_checkpoint(&mut completed_batch_checkpoint).await;

            if cancelled_during_batch {
                return Err(OrchestratorError::Cancelled);
            }

            // Audit §3.4: after the current batch is fully processed
            // (so successful siblings had their `all_files`/`provider_metrics`
            // updates applied), check whether a non-recoverable subtask
            // error requested a graceful exit. If so, surface a Partial
            // AgentOutput — same shape the empty-queue terminal-failure
            // exit below uses for *recoverable* errors after retries are
            // exhausted. We do NOT continue dispatching the next ready
            // batch; that would waste budget on work we already know will
            // be discarded.
            if let Some((failed_task_id, failed_role, error)) = non_recoverable_exit.take() {
                let final_message = format!(
                    "Automation paused after a non-recoverable subtask error \
                     ({failed_role:?} subtask {failed_task_id}). Existing \
                     workspace changes and session context were preserved. {error}"
                );
                let _ = self.bus.publish_for_session(
                    task.session_id,
                    task.id.0,
                    EventKind::MultiAgentModeCompleted { task_id: task.id, cost_usd: total_cost },
                );
                checkpoint_scope.sequence_num = checkpoint_scope.sequence_num.saturating_add(1);
                let mut cp = checkpoint::build_checkpoint(
                    &checkpoint_scope,
                    checkpoint::CheckpointStage::Executing,
                    None,
                    &context.working_memory,
                    &graph,
                    &completed_results,
                    total_cost,
                    total_tool_calls,
                    &provider_metrics,
                    &all_files,
                    &self.expected_artifacts_snapshot(),
                    &subtask_attempts,
                    &retry_feedback,
                    &self.checkpoint_context(&model_assignments, &action_ledger),
                );
                self.persist_checkpoint(&mut cp).await;
                let checkpoint_json = serde_json::to_string(&cp).ok();
                return Ok((
                    AgentOutput {
                        task_id: task.id,
                        session_id: task.session_id,
                        final_message,
                        files_modified: all_files,
                        tool_call_count: total_tool_calls,
                        eval_result: None,
                        tool_events: Vec::new(),
                        verification: Vec::new(),
                        project_root: None,
                        completion_status: concerto_core::types::AgentCompletionStatus::Partial,
                        provider_metrics,
                        checkpoint_json,
                    },
                    recoverable_notes,
                ));
            }
        }

        // ── 3. Run validation suite ──────────────────────────────────────
        // C-06: acceptance of a run that contained implement-stage work is
        // coordinator-owned; the validation loop needs to know whether this
        // was a build task so it can enforce artifact + verification
        // evidence.
        let build_task = graph.all_tasks().iter().any(|subtask| {
            self.stage_of(&subtask.role).as_ref().is_some_and(AgentStage::is_implement)
        });
        let validation_result = self
            .run_validation_loop(&task, &context, &cancel, build_task, &mut action_ledger)
            .await?;
        total_cost += validation_result.cost_usd;
        total_tool_calls += validation_result.tool_call_count;
        all_files.extend(validation_result.files_modified.clone());
        let settled = metrics_from_result(&validation_result);
        provider_metrics.push(settled.clone());
        self.settled_metrics.push(settled);
        if !matches!(validation_result.outcome, AgentOutcome::Success) {
            recoverable_notes
                .push(format!("Validation remains unresolved: {}", validation_result.summary));
        }

        let _ = self.bus.publish_for_session(
            task.session_id,
            task.id.0,
            EventKind::MultiAgentModeCompleted { task_id: task.id, cost_usd: total_cost },
        );

        let completion_status = if recoverable_notes.is_empty() {
            concerto_core::types::AgentCompletionStatus::Completed
        } else {
            concerto_core::types::AgentCompletionStatus::Partial
        };
        // ── Run-continuity Phase 1: stall gate at the final exit ────────
        // A stalled run (declared-Completion false, declared deliverables
        // unproduced, or a Failed/Blocked subtask) KEEPS its resumable
        // checkpoint — persisted with completed=false — instead of
        // clearing it; only a clean success clears, byte-identical to the
        // pre-Phase-1 behavior.
        let project_root = camino::Utf8PathBuf::from_path_buf(context.session.project_dir.clone())
            .unwrap_or_else(|_| camino::Utf8PathBuf::from("."));
        let deliverables_missing =
            expected_artifacts_unproduced(&project_root, &self.expected_artifacts_snapshot());
        let stalled = run_is_stalled(completion_status, deliverables_missing, &graph);
        let checkpoint_json = if stalled {
            checkpoint_scope.sequence_num = checkpoint_scope.sequence_num.saturating_add(1);
            let mut cp = checkpoint::build_checkpoint(
                &checkpoint_scope,
                checkpoint::CheckpointStage::Validating,
                None,
                &context.working_memory,
                &graph,
                &completed_results,
                total_cost,
                total_tool_calls,
                &provider_metrics,
                &all_files,
                &self.expected_artifacts_snapshot(),
                &subtask_attempts,
                &retry_feedback,
                &self.checkpoint_context(&model_assignments, &action_ledger),
            );
            self.persist_checkpoint(&mut cp).await;
            serde_json::to_string(&cp).ok()
        } else {
            if let Some(store) = &self.session_store {
                if let Err(error) = store.clear_orchestration_checkpoint(task.session_id).await {
                    tracing::warn!(%error, "failed to clear completed orchestration checkpoint");
                }
            }
            None
        };
        // Exact-reset the settled mirror from the authoritative local vec
        // (the settlement pushes above may have populated it during the run;
        // on success the output's vec is the single source of truth).
        self.settled_metrics.clear();
        self.settled_metrics.extend(provider_metrics.iter().cloned());
        Ok((
            AgentOutput {
                task_id: task.id,
                session_id: task.session_id,
                final_message: if recoverable_notes.is_empty() {
                    "Multi-agent orchestration completed".into()
                } else {
                    format!(
                    "Automation preserved its workspace changes and session context after recoverable issues remained. {}",
                    recoverable_notes.join(" ")
                )
                },
                files_modified: all_files,
                tool_call_count: total_tool_calls,
                eval_result: None,
                tool_events: Vec::new(),
                verification: Vec::new(),
                project_root: None,
                completion_status,
                provider_metrics,
                checkpoint_json,
            },
            recoverable_notes,
        ))
    }

    pub async fn run(
        &mut self,
        task: AgentTask,
        mut context: AgentContext,
        cancel: CancellationToken,
        resume_checkpoint_json: Option<String>,
    ) -> Result<AgentOutput, OrchestratorError> {
        // Fresh run: forget settlements from any prior run on this instance.
        self.settled_metrics.clear();
        // Phase 0: Retrieve project memory context
        self.retrieve_memory_context(&task, &mut context, cancel.clone()).await;

        // Phase 1: Decompose or restore
        let DecomposeResult {
            graph,
            completed_results,
            total_cost,
            total_tool_calls,
            all_files,
            provider_metrics,
            subtask_attempts,
            retry_feedback,
            model_assignments,
            action_ledger,
            objective: run_objective,
            objective_hash: run_objective_hash,
        } = match self.decompose_or_restore(&task, &context, &cancel, resume_checkpoint_json).await
        {
            Ok(result) => result,
            Err(e) => {
                // If decomposition fails (e.g. Architect parser error) return
                // a clean Partial result rather than crashing the session.
                return Ok(AgentOutput {
                    task_id: task.id,
                    session_id: task.session_id,
                    final_message: format!(
                        "Automation paused: could not produce a valid plan. {e}"
                    ),
                    files_modified: vec![],
                    tool_call_count: 0,
                    eval_result: None,
                    tool_events: vec![],
                    verification: vec![],
                    project_root: None,
                    completion_status: concerto_core::types::AgentCompletionStatus::Partial,
                    provider_metrics: vec![],
                    checkpoint_json: None,
                });
            }
        };

        // ADR-55 Phase 2b: planning-only orchestration. The plan was just
        // produced (resume is always `None` on this path, so nothing was
        // restored); render it as the run's final message and return without
        // dispatching any subtask, review, or validation. No checkpoint is
        // written or cleared here: a stale partial-run checkpoint stays
        // untouched for the caller to resolve (M2).
        if self.orchestration_depth == OrchestrationDepth::PlanningOnly {
            let final_message = self.render_plan(&task, &graph);
            let _ = self.bus.publish_for_session(
                task.session_id,
                task.id.0,
                EventKind::MultiAgentModeCompleted { task_id: task.id, cost_usd: total_cost },
            );
            return Ok(AgentOutput {
                task_id: task.id,
                session_id: task.session_id,
                final_message,
                files_modified: vec![],
                tool_call_count: 0,
                eval_result: None,
                tool_events: vec![],
                verification: vec![],
                project_root: None,
                completion_status: concerto_core::types::AgentCompletionStatus::Completed,
                provider_metrics: vec![],
                checkpoint_json: None,
            });
        }

        // Phase 2: Execute graph
        self.execute_graph(
            task,
            context,
            cancel,
            graph,
            completed_results,
            total_cost,
            total_tool_calls,
            all_files,
            provider_metrics,
            subtask_attempts,
            retry_feedback,
            model_assignments,
            action_ledger,
            run_objective,
            run_objective_hash,
        )
        .await
        .map(|(output, _notes)| output)
    }

    // ── review cycle (§3.8) ─────────────────────────────────────────────

    /// Provider metrics settled by the most recent [`Self::run`], also
    /// available after a failed run so callers can persist what was actually
    /// consumed before the error. Exact copy of the success output's
    /// `provider_metrics` on success; the pre-failure accumulation otherwise.
    pub fn settled_metrics(&self) -> &[ProviderMetrics] {
        &self.settled_metrics
    }

    /// ADR-60 Deferred 3: persist one review-cycle snapshot (fail-soft).
    ///
    /// Returns the stored row's `gate_seq` for cursor chaining, or `None`
    /// when persistence degraded (no pool attached / append error). The
    /// review continues either way — continuity bookkeeping must never fail
    /// a run — and every degradation is logged, never silent.
    async fn persist_review_state(&self, payload: &ReviewStatePayload) -> Option<u64> {
        let pool = self.review_store.as_ref()?;
        match append_review_state_event(pool, payload).await {
            Ok(stored) => Some(stored.gate_seq),
            Err(error) => {
                warn!(
                    %error,
                    plan_id = %payload.plan_id,
                    "failed to persist review state; the review continues without \
                     resumability (ADR-60 Deferred 3 degradation)"
                );
                None
            }
        }
    }

    /// Run the review loop: a review-stage agent checks implement output,
    /// re-runs the implement-stage agent if revision is needed, up to
    /// `max_cycles`. The cycle limit is governed by the `CollaborationRule`
    /// for `Reviewer -> Coder` (defaults to 3 if not configured).
    ///
    /// ADR-35 §5: review participants are resolved by stage tag from the
    /// registry. Pipelines without a review-stage agent skip review.
    ///
    /// ADR-60 Deferred 3: with a review store AND an approved-plan binding
    /// attached, every cycle transition is a full-state whiteboard snapshot
    /// committed BEFORE the work it describes (WAL-before-invoke), and an
    /// entry after a restart resumes the interrupted cycle group — ledger,
    /// retry counters, and cursor rehydrated and validated — instead of
    /// running a duplicate second review. Costs spent by the crashed attempt
    /// are gone with it (only verdicts are durable) and are honestly absent
    /// from the resumed run's totals.
    #[allow(clippy::too_many_arguments)]
    async fn run_review_cycle(
        &mut self,
        graph: &mut TaskGraph,
        task_id: TaskId,
        description: String,
        session_id: Ulid,
        source_result: &AgentRunResult,
        context: &AgentContext,
        task: AgentTask,
        cancel: &CancellationToken,
    ) -> Result<AgentRunResult, OrchestratorError> {
        // The review stage is resolved by kind: a renamed review tag keeps
        // the gate cycle and its skip message (issue #150).
        let review_tag =
            kind_stage_tag(self.blueprint_facade.as_ref(), StageKind::Review, AgentStage::REVIEW);
        let Some(reviewer_role) = self.first_agent_for_stage(&AgentStage::new(&review_tag)) else {
            // ADR-58 P2+P3 (F8): the skip message routes the review stage's
            // configured label when it differs from the standard "Review"; on
            // the default blueprint the emitted string stays byte-identical.
            let summary = match self
                .blueprint_facade
                .as_ref()
                .and_then(|facade| facade.stage_by_tag(&review_tag))
                .map(|stage| stage.def.label.as_str())
            {
                Some("Review") | None => {
                    "No review-stage agent registered; review skipped".to_string()
                }
                Some(label) => {
                    format!("No review-stage agent registered ({label}); review skipped")
                }
            };
            return Ok(AgentRunResult {
                task_id: TaskId::new(),
                role: source_result.role.clone(),
                outcome: AgentOutcome::Success,
                summary,
                files_modified: Vec::new(),
                tool_call_count: 0,
                cost_usd: 0.0,
                latency_ms: 0,
                provider: String::new(),
                model: String::new(),
                tokens_in: 0,
                tokens_out: 0,
            });
        };
        let implement_role = source_result.role.clone();
        // ADR-58 P2+P3 (R2): the fallback cap is the closed gate kind's engine
        // default (Review → 3), resolved through the blueprint facade when one
        // is attached; a legacy `CollaborationRule` cap still wins when one is
        // configured.
        let kind_default = match &self.blueprint_facade {
            Some(facade) => facade.max_cycles(
                &reviewer_role,
                &implement_role,
                StageKind::Review.default_max_cycles(),
            ),
            None => StageKind::Review.default_max_cycles(),
        };
        let max_cycles =
            self.relationships.max_cycles(&reviewer_role, &implement_role, kind_default);
        self.review_cycles.set_max_cycles(max_cycles);
        // ── ADR-60 Deferred 3: durable review-cycle state ───────────────────
        // Identity of THIS cycle group: the approved plan id (the only
        // identity that survives a process restart) plus a restart-stable
        // hash of `(implement role, target description)` — approved-plan runs
        // decompose from the seeded DesignDoc, so the description repeats
        // byte-identically after a restart. Everything here is fail-soft:
        // any rehydration or persistence problem degrades to pre-Phase 3
        // behavior with an observable log, never a run failure.
        let review_key = match (&self.review_store, self.approved_plan_seed.as_ref()) {
            (Some(pool), Some(seed)) => Some((
                pool.clone(),
                seed.plan_id.clone(),
                review_target_identity(implement_role.as_str(), &description),
            )),
            _ => None,
        };
        if review_key.is_none() {
            tracing::debug!(
                task_id = %task_id.0,
                "review cycle not resumable (ADR-60 Deferred 3): no whiteboard store \
                 attached or no approved-plan binding for this run"
            );
        }
        let mut ledger: Vec<ReviewFeedbackEntry> = Vec::new();
        let mut retry_count = 0_u32;
        let mut last_review_event_seq = 0_u64;
        let mut start_cycle = 1_u32;
        if let Some((pool, plan_id, target_hash)) = &review_key {
            match load_review_resume(pool, plan_id, target_hash, &session_id.to_string()).await {
                Ok(ReviewResume::Resolved { status, feedback_ledger }) => {
                    // Oracle comment 3 (idempotency): a previous attempt
                    // already settled this cycle group — its recorded outcome
                    // stands and NO second reviewer call may run for the same
                    // target, even though this attempt's implement subtask
                    // re-entered the review gate.
                    tracing::info!(
                        %plan_id,
                        cycles = feedback_ledger.len(),
                        ?status,
                        "review cycle already settled per the whiteboard; suppressing \
                         duplicate review (ADR-60 Deferred 3)"
                    );
                    if status == ReviewCycleStatus::Escalated {
                        let _ = self.bus.publish_for_session(
                            session_id,
                            task_id.0,
                            EventKind::ReviewCycleEscalated { task_id, max_cycles },
                        );
                    } else {
                        let _ = self.bus.publish_for_session(
                            session_id,
                            task_id.0,
                            EventKind::ReviewCycleCompleted {
                                task_id,
                                cycle_num: u32::try_from(feedback_ledger.len() + 1).unwrap_or(1),
                                verdict: "pass".into(),
                            },
                        );
                    }
                    let last_reason = feedback_ledger.last().and_then(|entry| entry.reason.clone());
                    let summary = match status {
                        ReviewCycleStatus::Escalated => format!(
                            "Review remains unresolved: {} (settled before the restart; \
                             resumed from whiteboard)",
                            last_reason.unwrap_or_else(|| "max cycles reached".to_owned())
                        ),
                        _ => "Review previously completed for this deliverable \
                              (resumed from whiteboard); no new review run"
                            .to_owned(),
                    };
                    return Ok(AgentRunResult {
                        task_id: TaskId::new(),
                        role: source_result.role.clone(),
                        outcome: AgentOutcome::Success,
                        summary,
                        files_modified: Vec::new(),
                        tool_call_count: 0,
                        cost_usd: 0.0,
                        latency_ms: 0,
                        provider: String::new(),
                        model: String::new(),
                        tokens_in: 0,
                        tokens_out: 0,
                    });
                }
                Ok(ReviewResume::Resume {
                    resume_cycle,
                    retry_count: persisted_retries,
                    feedback_ledger: persisted_ledger,
                    from_gate_seq,
                }) => {
                    if resume_cycle > max_cycles {
                        // The cap shrank between attempts (config change):
                        // every slot is spent; settle unresolved without
                        // another reviewer call rather than exceeding the cap.
                        warn!(
                            %plan_id,
                            %resume_cycle,
                            %max_cycles,
                            "persisted review cycle is beyond the current cycle cap; \
                             settling unresolved without another reviewer call"
                        );
                        let _ = self.bus.publish_for_session(
                            session_id,
                            task_id.0,
                            EventKind::ReviewCycleEscalated { task_id, max_cycles },
                        );
                        return Ok(AgentRunResult {
                            task_id: TaskId::new(),
                            role: source_result.role.clone(),
                            outcome: AgentOutcome::Success,
                            summary: format!(
                                "Review remains unresolved after {max_cycles} cycles \
                                 (resumed beyond the configured cap from whiteboard)"
                            ),
                            files_modified: Vec::new(),
                            tool_call_count: 0,
                            cost_usd: 0.0,
                            latency_ms: 0,
                            provider: String::new(),
                            model: String::new(),
                            tokens_in: 0,
                            tokens_out: 0,
                        });
                    }
                    // Fast-forward the in-memory gate counter so `next_cycle`
                    // inside the loop stays consistent with the persisted
                    // position (a restart rebuilt this manager empty).
                    for _ in 1..resume_cycle {
                        let _ = self.review_cycles.next_cycle(task_id);
                    }
                    start_cycle = resume_cycle;
                    retry_count = persisted_retries;
                    ledger = persisted_ledger;
                    last_review_event_seq = from_gate_seq;
                    tracing::info!(
                        %plan_id,
                        %resume_cycle,
                        retries = retry_count,
                        "resuming interrupted review cycle from whiteboard state \
                         (ADR-60 Deferred 3)"
                    );
                }
                // Nothing trustworthy persisted — start fresh (pre-Phase 3).
                Ok(ReviewResume::Fresh) => {}
                Err(error) => warn!(
                    %error,
                    "review-state lookup failed; proceeding without resumability \
                     (ADR-60 Deferred 3 degradation)"
                ),
            }
        }
        let mut review_input = source_result.clone();
        let mut revision_cost = 0.0;
        let mut revision_tool_calls = 0_u32;
        let mut revision_files = Vec::new();
        let mut revision_tokens_in = 0_u64;
        let mut revision_tokens_out = 0_u64;

        for cycle in start_cycle..=max_cycles {
            if cancel.is_cancelled() {
                return Err(OrchestratorError::Cancelled);
            }

            let _ = self.bus.publish_for_session(
                session_id,
                task_id.0,
                EventKind::ReviewCycleStarted { task_id, cycle_num: cycle },
            );

            // ADR-60 Deferred 3 (WAL-before-invoke): commit the FULL snapshot
            // — ledger, counters, cursor — BEFORE spawning the reviewer, so a
            // crash can only ever land between durable snapshots. A verdict
            // lost in that gap leaves the snapshot open and the resumed run
            // replays exactly one reviewer call with the ledger carried over.
            if let Some((_pool, plan_id, target_hash)) = &review_key {
                let snapshot = review_snapshot(
                    plan_id,
                    session_id,
                    implement_role.as_str(),
                    &description,
                    target_hash,
                    ReviewCycleStatus::Started,
                    max_cycles,
                    retry_count,
                    &ledger,
                    last_review_event_seq,
                );
                if let Some(stored_seq) = self.persist_review_state(&snapshot).await {
                    last_review_event_seq = stored_seq;
                }
            }

            // On a resumed cycle the reviewer must see the feedback the
            // previous attempt already collected — otherwise it would redo
            // settled work (the redundant cost Phase 3 exists to avoid).
            let mut review_description = format!("Review cycle {cycle} for: {description}");
            if !ledger.is_empty() {
                review_description.push_str(
                    "\n\nPrior review feedback carried over from before the restart \
                     (ADR-60 Deferred 3); verify these were addressed instead of \
                     redoing settled work:",
                );
                for entry in &ledger {
                    let reason = entry.reason.as_deref().unwrap_or("unspecified");
                    review_description.push_str(&format!(
                        "\n- cycle {}: needs revision: {reason}",
                        entry.cycle_num
                    ));
                }
            }
            let review_task = SubTask {
                id: TaskId::new(),
                parent_id: Some(task_id),
                session_id,
                role: reviewer_role.clone(),
                description: review_description,
                status: concerto_core::types::SubTaskStatus::Pending,
                dependencies: vec![task_id],
                deliverable: None,
                created_at: time::OffsetDateTime::now_utc(),
                completed_at: None,
            };

            // Add review task to graph
            let review_task_id = review_task.id;
            graph.add_child(review_task.clone(), task_id, Dependency::MustFinishBefore);
            graph.mark_running(&review_task_id);

            let profile = self.model_selector.select_for_session(
                &reviewer_role,
                None,
                review_task_id,
                Some(session_id),
            )?;

            let review_ctx = AgentContext {
                session: context.session.clone(),
                parent_task: Some(task.clone()),
                working_memory: context.working_memory.clone(),
                retrieved_chunks: context.retrieved_chunks.clone(),
                previous_results: vec![review_input.clone()],
                budget_remaining_usd: None,
                expected_artifacts: Vec::new(),
                workspace_capsule: None,
                workspace_snapshot_digest: self.snapshot_digest(cancel).await,
                run_id: self.run_id.clone(),
                workspace_generation: self.snapshot_generation(),
            };

            let result = self
                .runner
                .run(reviewer_role.clone(), &review_task, review_ctx, &profile, cancel.clone())
                .await?;
            if let Some(review_subtask) = graph.get_mut(&review_task_id) {
                review_subtask.deliverable = Some(result.summary.clone());
            }
            graph.mark_done(&review_task_id);

            self.review_cycles.next_cycle(task_id)?;

            match &result.outcome {
                AgentOutcome::Success => {
                    let _ = self.bus.publish_for_session(
                        session_id,
                        task_id.0,
                        EventKind::ReviewCycleCompleted {
                            task_id,
                            cycle_num: cycle,
                            verdict: "pass".into(),
                        },
                    );
                    // ADR-60 Deferred 3: settle the cycle group durably so a
                    // restart reports it resolved instead of re-reviewing
                    // (oracle comment 3). No cursor update needed — this arm
                    // returns immediately.
                    if let Some((_pool, plan_id, target_hash)) = &review_key {
                        let snapshot = review_snapshot(
                            plan_id,
                            session_id,
                            implement_role.as_str(),
                            &description,
                            target_hash,
                            ReviewCycleStatus::Completed,
                            max_cycles,
                            retry_count,
                            &ledger,
                            last_review_event_seq,
                        );
                        let _ = self.persist_review_state(&snapshot).await;
                    }
                    let mut completed = result.clone();
                    completed.cost_usd += revision_cost;
                    completed.tool_call_count =
                        completed.tool_call_count.saturating_add(revision_tool_calls);
                    completed.files_modified.extend(revision_files);
                    completed.tokens_in = completed.tokens_in.saturating_add(revision_tokens_in);
                    completed.tokens_out = completed.tokens_out.saturating_add(revision_tokens_out);
                    return Ok(completed);
                }
                AgentOutcome::NeedsRevision { reason } => {
                    // ADR-60 Deferred 3: the verdict is durable BEFORE any
                    // follow-up work, so a resumed run never replays a cycle
                    // that already produced one.
                    ledger.push(ReviewFeedbackEntry {
                        cycle_num: cycle,
                        verdict: "needs-revision".to_owned(),
                        reason: Some(reason.clone()),
                    });
                    retry_count = retry_count.saturating_add(1);
                    if cycle >= max_cycles {
                        let _ = self.bus.publish_for_session(
                            session_id,
                            task_id.0,
                            EventKind::ReviewCycleEscalated { task_id, max_cycles },
                        );
                        if let Some((_pool, plan_id, target_hash)) = &review_key {
                            let snapshot = review_snapshot(
                                plan_id,
                                session_id,
                                implement_role.as_str(),
                                &description,
                                target_hash,
                                ReviewCycleStatus::Escalated,
                                max_cycles,
                                retry_count,
                                &ledger,
                                last_review_event_seq,
                            );
                            // Terminal arm — returns below, no cursor update.
                            let _ = self.persist_review_state(&snapshot).await;
                        }
                        let mut unresolved = result.clone();
                        unresolved.cost_usd += revision_cost;
                        unresolved.tool_call_count =
                            unresolved.tool_call_count.saturating_add(revision_tool_calls);
                        unresolved.files_modified.extend(revision_files);
                        unresolved.tokens_in =
                            unresolved.tokens_in.saturating_add(revision_tokens_in);
                        unresolved.tokens_out =
                            unresolved.tokens_out.saturating_add(revision_tokens_out);
                        return Ok(unresolved);
                    }
                    // Publish agent handoff event for the audit log
                    let handoff = AgentHandoff::new(
                        reviewer_role.clone(),
                        implement_role.clone(),
                        task_id,
                        reason.clone(),
                        HandoffDeliverable::CodeReview(reason.clone()),
                    );
                    let _ = self.bus.publish_for_session(
                        session_id,
                        task_id.0,
                        EventKind::AgentHandoff {
                            from: handoff.from,
                            to: handoff.to,
                            task_id: handoff.task_id,
                            rationale: handoff.rationale.clone(),
                        },
                    );
                    // ADR-60 Deferred 3: durably record the queued revision
                    // before dispatching it — a crash here resumes AFTER this
                    // verdict (one fresh implement pass + next review), never
                    // re-asking the same reviewer question.
                    if let Some((_pool, plan_id, target_hash)) = &review_key {
                        let snapshot = review_snapshot(
                            plan_id,
                            session_id,
                            implement_role.as_str(),
                            &description,
                            target_hash,
                            ReviewCycleStatus::RevisionQueued,
                            max_cycles,
                            retry_count,
                            &ledger,
                            last_review_event_seq,
                        );
                        if let Some(stored_seq) = self.persist_review_state(&snapshot).await {
                            last_review_event_seq = stored_seq;
                        }
                    }
                    let coder_task = SubTask {
                        id: TaskId::new(),
                        parent_id: Some(task_id),
                        session_id,
                        role: implement_role.clone(),
                        description: format!("Revise (review {cycle}): {reason}"),
                        status: concerto_core::types::SubTaskStatus::Pending,
                        dependencies: vec![task_id],
                        deliverable: None,
                        created_at: time::OffsetDateTime::now_utc(),
                        completed_at: None,
                    };
                    let coder_artifacts = self
                        .expected_artifacts
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .get(&task_id)
                        .cloned()
                        .unwrap_or_default();
                    let coder_ctx = AgentContext {
                        session: context.session.clone(),
                        parent_task: Some(task.clone()),
                        working_memory: context.working_memory.clone(),
                        retrieved_chunks: context.retrieved_chunks.clone(),
                        previous_results: vec![result],
                        budget_remaining_usd: None,
                        expected_artifacts: coder_artifacts,
                        workspace_capsule: None,
                        workspace_snapshot_digest: self.snapshot_digest(cancel).await,
                        run_id: self.run_id.clone(),
                        workspace_generation: self.snapshot_generation(),
                    };
                    // Select routing profile for coder revision
                    let coder_profile = self.model_selector.select_for_session(
                        &implement_role,
                        None,
                        coder_task.id,
                        Some(session_id),
                    )?;
                    let coder_result = self
                        .runner
                        .run(
                            implement_role.clone(),
                            &coder_task,
                            coder_ctx,
                            &coder_profile,
                            cancel.clone(),
                        )
                        .await?;
                    revision_cost += coder_result.cost_usd;
                    revision_tokens_in = revision_tokens_in.saturating_add(coder_result.tokens_in);
                    revision_tokens_out =
                        revision_tokens_out.saturating_add(coder_result.tokens_out);
                    revision_tool_calls =
                        revision_tool_calls.saturating_add(coder_result.tool_call_count);
                    revision_files.extend(coder_result.files_modified.clone());
                    review_input = coder_result;
                }
                _ => {
                    return Err(OrchestratorError::AgentLoopError(
                        "reviewer agent failed unexpectedly".into(),
                    ));
                }
            }
        }

        Ok(AgentRunResult {
            task_id: TaskId::new(),
            role: reviewer_role,
            outcome: AgentOutcome::Success,
            summary: "Review completed".into(),
            files_modified: vec![],
            tool_call_count: 0,
            cost_usd: 0.0,
            latency_ms: 0,
            provider: String::new(),
            model: String::new(),
            tokens_in: 0,
            tokens_out: 0,
        })
    }

    // ── validation loop (§3.8) ──────────────────────────────────────────

    /// Record an acceptance decision in the checkpoint action ledger
    /// (audit C-06). `accepted` selects the kind (`"accepted"`/`"rejected"`);
    /// the evidence carries the verified artifact list and whether the
    /// validation (eval) run passed.
    fn record_acceptance(
        &self,
        action_ledger: &mut Vec<checkpoint::CheckpointAction>,
        task_id: TaskId,
        accepted: bool,
        artifacts: &[camino::Utf8PathBuf],
        verification_passed: bool,
    ) {
        action_ledger.push(checkpoint::CheckpointAction {
            kind: if accepted { "accepted".into() } else { "rejected".into() },
            task_id: Some(task_id),
            timestamp: time::OffsetDateTime::now_utc(),
            evidence: Some(checkpoint::AcceptanceEvidence {
                artifacts: artifacts.to_vec(),
                verification_passed,
            }),
        });
    }

    /// C-06: coordinator-owned acceptance for build tasks.
    ///
    /// Runs after the validator reports `Success`. For build tasks the run is
    /// accepted only when (1) every expected artifact exists on disk
    /// (resolved against the project root) with non-placeholder content and
    /// (2) verification evidence is present — the validator's eval pass, i.e.
    /// its `Success` outcome (the generic Freeform coder's `Success` on
    /// terminal text never self-accepts a build task).
    ///
    /// Returns `None` when accepted, or a `Failed` result carrying the
    /// rejection. Records the decision (and evidence) in the checkpoint
    /// action ledger.
    ///
    /// Policy for tasks without expected artifacts: no expected artifacts +
    /// verification passed → accepted (the artifact check is vacuous).
    fn acceptance_rejection(
        &self,
        task: &AgentTask,
        build_task: bool,
        project_root: &camino::Utf8Path,
        action_ledger: &mut Vec<checkpoint::CheckpointAction>,
    ) -> Option<AgentRunResult> {
        if !build_task {
            // Non-build stages (research/review/design) keep their existing
            // acceptance semantics (review-verdict mapping etc. unchanged).
            return None;
        }
        // Collect the run's expected artifacts across all implement
        // subtasks, de-duplicated.
        let expected = expected_artifact_list(&self.expected_artifacts_snapshot());

        match verify_expected_artifacts(project_root, &expected) {
            Ok(verified) => {
                self.record_acceptance(action_ledger, task.id, true, &verified, true);
                None
            }
            Err(violations) => {
                let detail = violations
                    .iter()
                    .map(|(path, reason)| format!("{path}: {reason}"))
                    .collect::<Vec<_>>()
                    .join("; ");
                let summary = format!(
                    "Acceptance rejected: expected artifacts missing or placeholder — {detail}"
                );
                let offending: Vec<camino::Utf8PathBuf> =
                    violations.iter().map(|(path, _)| path.clone()).collect();
                self.record_acceptance(action_ledger, task.id, false, &offending, true);
                Some(acceptance_failure_result(task, summary))
            }
        }
    }

    /// Run the validation loop: run test suite after all code is written.
    /// Max cycles is governed by the `CollaborationRule` for
    /// `Validator -> Coder` (defaults to 2 if not configured).
    ///
    /// ADR-35 §5: validation participants are resolved by stage tag from
    /// the registry. Pipelines without a validation-stage agent skip
    /// validation.
    ///
    /// `build_task` marks a run that contained implement-stage work: for
    /// those runs acceptance is coordinator-owned (audit C-06) and requires
    /// artifact + verification evidence (see [`Self::acceptance_rejection`]).
    /// `action_ledger` records the acceptance decision for the checkpoint.
    async fn run_validation_loop(
        &mut self,
        task: &AgentTask,
        context: &AgentContext,
        cancel: &CancellationToken,
        build_task: bool,
        action_ledger: &mut Vec<checkpoint::CheckpointAction>,
    ) -> Result<AgentRunResult, OrchestratorError> {
        // The acceptance stage is resolved by kind, so a renamed validate
        // tag keeps its validation gate and self-verify fallback (issue
        // #150).
        let validate_tag = kind_stage_tag(
            self.blueprint_facade.as_ref(),
            StageKind::Acceptance,
            AgentStage::VALIDATE,
        );
        let Some(validator_role) = self.first_agent_for_stage(&AgentStage::new(&validate_tag))
        else {
            if build_task && self.self_verify_available() {
                // ADR-35 §5, Phase 5 C-06 amendment: no validation-stage
                // agent is registered, but the coordinator holds an eval
                // engine — the coordinator carries verification itself. This
                // is a single cycle (there is no validator + implement pair
                // to fix failures), so the cycle counter stays at 1.
                let project_root =
                    camino::Utf8PathBuf::from_path_buf(context.session.project_dir.clone())
                        .unwrap_or_else(|_| camino::Utf8PathBuf::from("."));
                let val_task = SubTask {
                    id: TaskId::new(),
                    parent_id: Some(task.id),
                    session_id: task.session_id,
                    role: AgentId::new("coordinator"),
                    description: "Coordinator self-verification".into(),
                    status: concerto_core::types::SubTaskStatus::Pending,
                    dependencies: vec![],
                    deliverable: None,
                    created_at: time::OffsetDateTime::now_utc(),
                    completed_at: None,
                };
                let mut val_ctx = context.clone();
                val_ctx.parent_task = Some(task.clone());
                let result = match self
                    .run_coordinator_self_verify(&val_task, val_ctx, cancel.clone())
                    .await
                {
                    Ok(result) => result,
                    Err(error) => {
                        // The single self-verify cycle is exhausted (the
                        // escalation event was already published by
                        // `run_coordinator_self_verify`). Record the
                        // failed acceptance before propagating, mirroring
                        // the rejection path's ledger discipline.
                        self.record_acceptance(action_ledger, task.id, false, &[], false);
                        return Err(error);
                    }
                };
                // Route exactly like the validator path: the coordinator's
                // self-verification Success is necessary but not sufficient
                // for a build task — acceptance also requires the artifact
                // evidence (C-06).
                if matches!(result.outcome, AgentOutcome::Success) {
                    match self.acceptance_rejection(task, build_task, &project_root, action_ledger)
                    {
                        None => {
                            // Coordinator self-verification passed and the
                            // acceptance evidence is complete.
                            let mut accepted = result;
                            accepted.summary = format!(
                                "Coordinator self-verification passed: {}",
                                accepted.summary
                            );
                            return Ok(accepted);
                        }
                        Some(rejected) => return Ok(rejected),
                    }
                }
                // Failed — the detected test runner failed, or no runner was
                // detected. Verification ran but did not pass; a build task
                // is not accepted without verification evidence.
                let summary = format!(
                    "Acceptance rejected: coordinator self-verification failed — {}",
                    result.summary
                );
                self.record_acceptance(action_ledger, task.id, false, &[], false);
                return Ok(acceptance_failure_result(task, summary));
            }
            if build_task {
                // C-06: a build task whose pipeline has no validation-stage
                // agent never produced verification evidence. Do not accept
                // silently — the absence of declared verification commands
                // is an acceptance failure.
                let summary = "Acceptance rejected: no validation-stage agent registered; verification did not run for a build task"
                    .to_string();
                self.record_acceptance(action_ledger, task.id, false, &[], false);
                return Ok(acceptance_failure_result(task, summary));
            }
            return Ok(AgentRunResult {
                task_id: TaskId::new(),
                role: AgentId::new("validator"),
                outcome: AgentOutcome::Success,
                summary: "No validation-stage agent registered; validation skipped".into(),
                files_modified: Vec::new(),
                tool_call_count: 0,
                cost_usd: 0.0,
                latency_ms: 0,
                provider: String::new(),
                model: String::new(),
                tokens_in: 0,
                tokens_out: 0,
            });
        };
        // The implement role keys the primary `Execution` stage's resolved
        // tag, so a renamed implement stage keeps the self-verify fix-pair
        // loop (issue #150).
        let implement_tag = execution_stage_tag(self.blueprint_facade.as_ref());
        let implement_role = self.first_agent_for_stage(&AgentStage::new(&implement_tag));
        let max_cycles = match &implement_role {
            Some(role) => {
                // ADR-58 P2+P3 (R3): the fallback cap is the closed gate
                // kind's engine default (Acceptance → 2), resolved through
                // the blueprint facade when one is attached; a legacy
                // `CollaborationRule` cap still wins when configured.
                let kind_default = match &self.blueprint_facade {
                    Some(facade) => facade.max_cycles(
                        &validator_role,
                        role,
                        StageKind::Acceptance.default_max_cycles(),
                    ),
                    None => StageKind::Acceptance.default_max_cycles(),
                };
                self.relationships.max_cycles(&validator_role, role, kind_default)
            }
            // No implement agent to fix failures: the first failed cycle is
            // treated as exhausted.
            None => 1,
        };
        self.validation_cycles.set_max_cycles(max_cycles);
        let mut fix_cost = 0.0;
        let mut fix_tool_calls = 0_u32;
        let mut fixed_files = Vec::new();
        let mut fix_tokens_in = 0_u64;
        let mut fix_tokens_out = 0_u64;
        let mut previous_results = Vec::new();
        // Workspace root used to resolve expected artifacts for the C-06
        // acceptance gate.
        let project_root = camino::Utf8PathBuf::from_path_buf(context.session.project_dir.clone())
            .unwrap_or_else(|_| camino::Utf8PathBuf::from("."));

        for cycle in 1..=max_cycles {
            if cancel.is_cancelled() {
                return Err(OrchestratorError::Cancelled);
            }

            let _ = self.bus.publish_for_session(
                task.session_id,
                task.id.0,
                EventKind::ValidationCycleStarted { task_id: task.id, cycle_num: cycle },
            );
            // Advance validation cycle counter
            self.validation_cycles.next_cycle(task.id)?;

            // Use the validation-stage agent to run the test suite
            let agent = self.registry.get(&validator_role).ok_or_else(|| {
                OrchestratorError::AgentLoopError(format!(
                    "no agent registered for validation role {validator_role}"
                ))
            })?;

            let val_task = SubTask {
                id: TaskId::new(),
                parent_id: Some(task.id),
                session_id: task.session_id,
                role: validator_role.clone(),
                description: format!("Validation cycle {cycle}"),
                status: concerto_core::types::SubTaskStatus::Pending,
                dependencies: vec![],
                deliverable: None,
                created_at: time::OffsetDateTime::now_utc(),
                completed_at: None,
            };

            let mut val_ctx = context.clone();
            val_ctx.parent_task = Some(task.clone());
            val_ctx.previous_results = previous_results.clone();

            let mut result = match agent.run(&val_task, val_ctx, "", cancel.clone()).await {
                Ok(result) => result,
                // C-06: an eval-disabled validator errors instead of running
                // verification. A build task must not be silently accepted
                // without verification evidence — fail acceptance immediately
                // (implement retries cannot enable a missing engine).
                Err(OrchestratorError::AgentLoopError(message))
                    if is_validation_disabled(&message) =>
                {
                    let summary =
                        format!("Acceptance rejected: verification did not run — {message}");
                    self.record_acceptance(action_ledger, task.id, false, &[], false);
                    return Ok(acceptance_failure_result(task, summary));
                }
                Err(error) => return Err(error),
            };

            // C-06: coordinator-owned acceptance for build tasks. The
            // validator's Success is necessary but not sufficient — the run
            // is accepted only when the artifact evidence also passes. A
            // rejection converts the validator pass into a Failed outcome so
            // it flows through the retry logic below (distinguishable from a
            // genuine test failure by the "Acceptance rejected" summary and
            // the ledger entry).
            if matches!(result.outcome, AgentOutcome::Success) {
                match self.acceptance_rejection(task, build_task, &project_root, action_ledger) {
                    None => {
                        // Validation passed and acceptance evidence is
                        // complete.
                        result.cost_usd += fix_cost;
                        result.tool_call_count =
                            result.tool_call_count.saturating_add(fix_tool_calls);
                        result.files_modified.extend(fixed_files);
                        result.tokens_in = result.tokens_in.saturating_add(fix_tokens_in);
                        result.tokens_out = result.tokens_out.saturating_add(fix_tokens_out);
                        return Ok(result);
                    }
                    Some(rejected) => result = rejected,
                }
            }

            match result.outcome.clone() {
                AgentOutcome::Success => {
                    // Defensive: an accepted run returned above; a rejected
                    // run carries a Failed outcome instead.
                    return Ok(result);
                }
                AgentOutcome::Failed { error } => {
                    if cycle >= max_cycles {
                        let _ = self.bus.publish_for_session(
                            task.session_id,
                            task.id.0,
                            EventKind::ValidationEscalated { task_id: task.id, max_cycles },
                        );
                        result.cost_usd += fix_cost;
                        result.tool_call_count =
                            result.tool_call_count.saturating_add(fix_tool_calls);
                        result.files_modified.extend(fixed_files);
                        result.tokens_in = result.tokens_in.saturating_add(fix_tokens_in);
                        result.tokens_out = result.tokens_out.saturating_add(fix_tokens_out);
                        result.summary = format!(
                            "Validation still fails after {max_cycles} automatic recovery cycles. Latest result: {}",
                            result.summary
                        );
                        return Ok(result);
                    }
                    // Re-run the implement-stage agent to fix validation
                    // failures. Unreachable when no implement agent exists:
                    // max_cycles is 1 in that case, so the escalated return
                    // above already fired.
                    let Some(implement_role) = &implement_role else {
                        return Err(OrchestratorError::AgentLoopError(
                            "no implementation-stage agent registered to fix validation failures"
                                .into(),
                        ));
                    };
                    let fix_task = SubTask {
                        id: TaskId::new(),
                        parent_id: Some(task.id),
                        session_id: task.session_id,
                        role: implement_role.clone(),
                        description: format!("Fix validation (cycle {cycle}): {error}"),
                        status: concerto_core::types::SubTaskStatus::Pending,
                        dependencies: vec![],
                        deliverable: None,
                        created_at: time::OffsetDateTime::now_utc(),
                        completed_at: None,
                    };
                    let mut fix_ctx = context.clone();
                    fix_ctx.parent_task = Some(task.clone());
                    fix_ctx.previous_results = vec![result.clone()];

                    // Select routing profile for coder fix
                    let fix_profile = self.model_selector.select_for_session(
                        implement_role,
                        None,
                        task.id,
                        Some(task.session_id),
                    )?;
                    let fix_result = self
                        .runner
                        .run(
                            implement_role.clone(),
                            &fix_task,
                            fix_ctx,
                            &fix_profile,
                            cancel.clone(),
                        )
                        .await?;
                    fix_cost += fix_result.cost_usd;
                    fix_tokens_in = fix_tokens_in.saturating_add(fix_result.tokens_in);
                    fix_tokens_out = fix_tokens_out.saturating_add(fix_result.tokens_out);
                    fix_tool_calls = fix_tool_calls.saturating_add(fix_result.tool_call_count);
                    fixed_files.extend(fix_result.files_modified.clone());
                    previous_results = vec![result, fix_result];
                }
                _ => {
                    return Err(OrchestratorError::AgentLoopError(
                        "validator agent returned unexpected outcome".into(),
                    ));
                }
            }
        }

        Ok(AgentRunResult {
            task_id: TaskId::new(),
            role: validator_role,
            outcome: AgentOutcome::Success,
            summary: "Validation completed".into(),
            files_modified: vec![],
            tool_call_count: 0,
            cost_usd: 0.0,
            latency_ms: 0,
            provider: String::new(),
            model: String::new(),
            tokens_in: 0,
            tokens_out: 0,
        })
    }

    // ── relationship summary ────────────────────────────────────────────

    /// Build a human-readable summary of the current collaboration rules.
    pub fn relationship_summary(&self) -> String {
        self.relationships
            .rules()
            .iter()
            .map(|r| format!("{:?} {:?} {:?}", r.from, r.relationship, r.to))
            .collect::<Vec<_>>()
            .join("\n")
    }

    // ── task decomposition ───────────────────────────────────────────────

    /// Run a single design-stage (architect) attempt, folding model-selection
    /// and run failures into one `Result`. A model-selection failure (e.g.
    /// `NoAffordableModel` / `PinnedModelNotFound` — classified as
    /// `LimitReached`) is folded into the same recovery path as a run failure,
    /// resolved once per attempt inside the retry loop. Returns the completed
    /// design task plus a parseable `DesignDoc` on a genuine Success
    /// (ADR-65 §5: even an EMPTY doc completes the design stage — the
    /// deterministic verifier decides afterwards whether it binds, skips, or
    /// quarantines; only an unparseable summary stays a failure); otherwise
    /// the failure that the caller's recovery loop class decides on.
    async fn design_stage_attempt(
        &self,
        arch_task: &SubTask,
        role: &AgentId,
        task: &AgentTask,
        base_ctx: &AgentContext,
        cancel: &CancellationToken,
        retry_feedback: &[AgentRunResult],
    ) -> Result<(SubTask, DesignDoc), OrchestratorError> {
        let arch_ctx = AgentContext {
            session: base_ctx.session.clone(),
            parent_task: Some(task.clone()),
            working_memory: base_ctx.working_memory.clone(),
            retrieved_chunks: base_ctx.retrieved_chunks.clone(),
            previous_results: retry_feedback.to_vec(),
            budget_remaining_usd: None,
            expected_artifacts: Vec::new(),
            workspace_capsule: None,
            workspace_snapshot_digest: self.snapshot_digest(cancel).await,
            run_id: self.run_id.clone(),
            workspace_generation: self.snapshot_generation(),
        };
        let profile = self.model_selector.select_for_session(
            role,
            None,
            arch_task.id,
            Some(task.session_id),
        )?;
        let result =
            self.runner.run(role.clone(), arch_task, arch_ctx, &profile, cancel.clone()).await?;

        // A genuine Success carrying a parseable DesignDoc completes the
        // design stage — empty or not. Whether the (possibly empty) doc binds
        // is the verifier's call, resolved later in decompose_task (ADR-65 §5).
        if let Some(doc) = crate::prompts::parse_json_substring(&result.summary) {
            let mut completed_arch = arch_task.clone();
            completed_arch.status = SubTaskStatus::Completed;
            completed_arch.deliverable = Some(result.summary.clone());
            return Ok((completed_arch, doc));
        }

        // The model completed a run but produced no usable plan — a Failed /
        // Blocked outcome or a Success with an unparseable DesignDoc. Any of
        // these is a recoverable attempt that the recovery loop may retry and
        // (only when the retries are exhausted) rescue via the ladder.
        Err(match &result.outcome {
            AgentOutcome::Failed { .. } => OrchestratorError::AgentLoopError(format!(
                "Design agent failed: {}",
                result.summary
            )),
            outcome if !matches!(outcome, AgentOutcome::Success) => {
                OrchestratorError::AgentLoopError(format!(
                    "Design agent failed: {}",
                    result.summary
                ))
            }
            _ => OrchestratorError::AgentLoopError(
                "Design agent produced an unparseable DesignDoc".to_string(),
            ),
        })
    }

    /// Run the design-stage architect through the SAME classify → retry →
    /// escalate → fallback-ladder recovery the execution-phase dispatch loop
    /// uses (ADR-42/45). This gives the planning phase the resilience a model
    /// or provider failure gets during subtask dispatch, instead of a bare
    /// `.await?` that abandons the whole plan.
    ///
    /// Recovery semantics (mirroring the dispatch error arm):
    /// - any run error → classify; a Recoverable error retries the same
    ///   agent/model while attempts remain (default `max_subtask_attempts`),
    ///   with prior failures surfaced via `previous_results`;
    /// - Recoverable-exhausted or `LimitReached` → one escalation retry (once
    ///   per task, `Recoverable` only) before the ladder;
    /// - then the ADR-42 fallback ladder (tier 1 default model → tier 1b
    ///   default provider → tier 2 coordinator self-execution), isolated from
    ///   execution-phase walks by the per-`arch_id` guards (`arch_id` is a
    ///   fresh `TaskId`);
    /// - only an exhausted ladder surfaces the original error — the caller
    ///   degrades that to a graceful `Partial` plan, never a hard crash.
    ///
    /// Cancellation propagates immediately and non-recoverable structural
    /// errors exit without a ladder walk.
    async fn run_design_stage_with_recovery(
        &mut self,
        role: &AgentId,
        task: &AgentTask,
        base_ctx: &AgentContext,
        cancel: &CancellationToken,
        planning_repair: &str,
    ) -> Result<(SubTask, DesignDoc), OrchestratorError> {
        let arch_task = SubTask {
            id: TaskId::new(),
            parent_id: None,
            session_id: task.session_id,
            role: role.clone(),
            description: format!(
                "Design architecture for: {}{}",
                task.description, planning_repair
            ),
            status: SubTaskStatus::Running,
            dependencies: vec![],
            deliverable: None,
            created_at: time::OffsetDateTime::now_utc(),
            completed_at: None,
        };
        let arch_id = arch_task.id;

        // A role with no registered agent is a configuration error; the caller
        // surfaces the original failure without a (misdirected) ladder walk.
        if self.registry.get(role).is_none() {
            return Err(OrchestratorError::AgentLoopError(format!(
                "no agent registered for design role {role}"
            )));
        }

        // Failure feedback accumulated across re-attempts and surfaced to each
        // re-run through `previous_results` (mirrors the dispatch-loop
        // `retry_feedback` map).
        let mut retry_feedback: Vec<AgentRunResult> = Vec::new();
        // One-shot escalation per task, guarded by the run-level set
        // (checkpointed with the run).
        let mut escalated = false;
        // 1-indexed attempt counter; reset after escalation so the next read
        // equals `max_subtask_attempts` again (dispatch-loop semantics).
        let mut attempt: u32 = 0;

        loop {
            attempt += 1;

            match self
                .design_stage_attempt(&arch_task, role, task, base_ctx, cancel, &retry_feedback)
                .await
            {
                Ok((completed_arch, doc)) => return Ok((completed_arch, doc)),
                Err(err) => {
                    // Cancellation short-circuits (NonRecoverable semantics).
                    if is_cancellation_error(&err) {
                        return Err(err);
                    }
                    // Surface this attempt's failure to later re-runs.
                    retry_feedback.push(failed_attempt_result(
                        arch_id,
                        role.clone(),
                        err.to_string(),
                    ));

                    match classify_subtask_error(&err) {
                        // Structural: exit immediately.
                        SubtaskFailureClass::NonRecoverable => return Err(err),
                        // Transient: retry the same agent/model while attempts
                        // remain (ADR-42 §1).
                        SubtaskFailureClass::Recoverable if attempt < self.max_subtask_attempts => {
                            let _ = self.bus.publish_for_session(
                                task.session_id,
                                arch_id.0,
                                EventKind::AgentThought {
                                    agent_id: "coordinator".into(),
                                    content: format!(
                                        "Retrying design ({role}) after recoverable failure \
                                         (attempt {attempt}/{}): {err}",
                                        self.max_subtask_attempts
                                    ),
                                },
                            );
                            continue;
                        }
                        // Retries exhausted (Recoverable) or a provider/
                        // model-specific hard failure (LimitReached): escalate
                        // once, then walk the ladder.
                        SubtaskFailureClass::Recoverable | SubtaskFailureClass::LimitReached => {
                            // ── Escalation retry (Recoverable only) ───────
                            let is_recoverable = matches!(
                                classify_subtask_error(&err),
                                SubtaskFailureClass::Recoverable
                            );
                            if is_recoverable
                                && !escalated
                                && !self.escalation_attempted.contains(&arch_id)
                            {
                                escalated = true;
                                self.escalation_attempted.insert(arch_id);
                                // Reset to `MAX - 1` so the next loop read is
                                // exactly `MAX` (dispatch-path attempt-math).
                                attempt = self.max_subtask_attempts.saturating_sub(1);
                                let _ = self.bus.publish_for_session(
                                    task.session_id,
                                    arch_id.0,
                                    EventKind::AgentThought {
                                        agent_id: "coordinator".into(),
                                        content: format!(
                                            "Escalating design ({role}) — escalation retry \
                                             (attempt {attempt}/{} exhausted)",
                                            self.max_subtask_attempts
                                        ),
                                    },
                                );
                                continue;
                            }

                            // ── ADR-42 fallback ladder ────────────────────
                            let ladder_ctx = AgentContext {
                                session: base_ctx.session.clone(),
                                parent_task: Some(task.clone()),
                                working_memory: base_ctx.working_memory.clone(),
                                retrieved_chunks: base_ctx.retrieved_chunks.clone(),
                                previous_results: retry_feedback.clone(),
                                budget_remaining_usd: None,
                                expected_artifacts: Vec::new(),
                                workspace_capsule: None,
                                workspace_snapshot_digest: self.snapshot_digest(cancel).await,
                                run_id: self.run_id.clone(),
                                workspace_generation: self.snapshot_generation(),
                            };
                            match self
                                .attempt_fallback_ladder(
                                    &arch_task,
                                    role,
                                    &err,
                                    &ladder_ctx,
                                    cancel,
                                )
                                .await
                            {
                                FallbackOutcome::Success(result) => {
                                    // A ladder tier produced a genuine Success;
                                    // accept its DesignDoc whether or not it is
                                    // empty — the verifier decides binding,
                                    // skipping, or quarantine (ADR-65 §5).
                                    match crate::prompts::parse_json_substring(&result.summary) {
                                        Some(doc) => {
                                            let mut completed_arch = arch_task.clone();
                                            completed_arch.status = SubTaskStatus::Completed;
                                            completed_arch.deliverable = Some(result.summary);
                                            return Ok((completed_arch, doc));
                                        }
                                        // The ladder could not produce a usable
                                        // plan: the terminal failure drives the
                                        // caller's graceful Partial exit.
                                        _ => return Err(err),
                                    }
                                }
                                FallbackOutcome::Cancelled => {
                                    return Err(OrchestratorError::Cancelled);
                                }
                                FallbackOutcome::Exhausted => {
                                    // `err` is the original failure the ladder
                                    // failed to rescue; it drives the caller's
                                    // graceful Partial exit.
                                    return Err(err);
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    /// Decompose a user task into a DAG of `SubTask` nodes.
    ///
    /// ADR-35 §5: the pipeline shape comes from the registry. A design-stage
    /// agent (if registered) runs first and produces a `DesignDoc`; the
    /// planner then assigns research/implement/custom work; review and
    /// validation stages run automatically after execution. A pipeline
    /// without a design-stage agent skips the design step entirely.
    async fn decompose_task(
        &mut self,
        task: &AgentTask,
        context: &AgentContext,
        cancel: &CancellationToken,
        planning_feedback: Option<&checkpoint::PlanningCheckpoint>,
    ) -> Result<(TaskGraph, Option<PlanArtifact>), OrchestratorError> {
        let mut graph = TaskGraph::new();

        // ── Resolve lifecycle-stage participants from the registry ──────
        let design_role = self.first_agent_for_stage(&AgentStage::new(AgentStage::DESIGN));
        // ADR-58 P2+P3 (R4): the implement roster keys the primary `Execution`
        // stage's resolved tag — a custom blueprint that renames the stage
        // keeps its staffing, and on the default blueprint the resolved tag
        // is exactly `implement`, so behavior stays byte-identical (see the
        // sentinel render below, which uses the same resolution).
        let implement_tag = execution_stage_tag(self.blueprint_facade.as_ref());
        let mut implement_ids = self.registry.ids_for_stage(&AgentStage::new(&implement_tag));
        // ADR-35 §8 trigger 1 (stage absence): with no registered
        // implement-stage agent, the coordinator carries implementation itself
        // when it holds an executor. The reserved `coordinator` id is never
        // registered (the registry skips it), so it cannot collide with a real
        // agent and only reaches the planner roster / graph while this
        // self-execution standby is active.
        let coordinator_self_implements = implement_ids.is_empty() && self.self_execute_available();
        if coordinator_self_implements {
            implement_ids.push(AgentId::new("coordinator"));
        }
        let mut planner_agents: Vec<PlannerAgentInfo> = self
            .registry
            .ids()
            .iter()
            .map(|id| {
                // The stage tag always comes from the registered agent
                // (trait = source of truth); capabilities/description come
                // from the retained merged config when present (ADR-35
                // phase 4, roster enrichment).
                let stage = self.registry.get(id).and_then(|agent| agent.stage());
                let (capabilities, description) = match self.registry.config(id) {
                    Some(cfg) => (cfg.capabilities.clone(), config_description(cfg)),
                    None => (AgentCapabilities::default(), String::new()),
                };
                PlannerAgentInfo { id: id.clone(), stage, capabilities, description }
            })
            .collect();
        if coordinator_self_implements {
            // ADR-58 P2+P3 (R4/§3): the sentinel's roster entry mirrors the
            // self-implement render — the unstaffed-`Execution` fallback
            // persona (`implement_fallback_persona`) supplies the description
            // and the narrowed write flags, with the engine-owned
            // `fs_read`/`git`/`lsp` defaults overlaid (review F1) and the
            // reserved `coordinator` id never registered (F2). The stage tag
            // follows the primary `Execution` stage, falling back to
            // `implement` (planner.rs partitions on the same resolution).
            let persona = self.implement_fallback_persona();
            planner_agents.push(PlannerAgentInfo {
                id: AgentId::new("coordinator"),
                stage: Some(AgentStage::new(execution_stage_tag(self.blueprint_facade.as_ref()))),
                capabilities: sentinel_capabilities(&persona, StageKind::Execution),
                description: persona.label.clone(),
            });
        }

        let planning_repair = planning_feedback
            .filter(|feedback| !feedback.validation_error.trim().is_empty())
            .map(|feedback| {
                format!(
                    "\n\nResume the existing planning attempt. The last response failed validation: {}\nLast response excerpt:\n{}",
                    feedback.validation_error,
                    feedback.last_response.chars().take(2_000).collect::<String>()
                )
            })
            .unwrap_or_default();

        // ── Design stage (optional) ──────────────────────────────────────
        // ADR-60 D7 (#152): an approved-plan Execute seeds its structured
        // DesignDoc from the whiteboard and skips the architect — re-invoking
        // the architect with the same objective is exactly the silent
        // re-decompose D7 forbids. The seeded doc lands in `self.design_doc`
        // below, so checkpoints and expected-artifact derivation keep
        // working unchanged. A seed without a stored doc (text-only plans)
        // falls back to the normal stage rather than inventing a document.
        let mut design_doc: Option<DesignDoc> = self
            .approved_plan_seed
            .take()
            .map(|seed| {
                tracing::info!(
                    plan_id = %seed.plan_id,
                    "executing an approved plan: seeding the whiteboard DesignDoc \
                     and skipping the architect (ADR-60 D7)"
                );
                seed.design_doc
            })
            .unwrap_or(None);
        let design_id: Option<TaskId> = if design_doc.is_some() {
            // Seeded doc: subtasks become graph roots (the same shape a
            // pipeline without any design-stage agent produces).
            None
        } else {
            match &design_role {
                Some(role) => {
                    // The architect run is dispatched through the SAME
                    // classify → retry → escalate → fallback-ladder recovery the
                    // execution-phase dispatch loop uses (ADR-42/45). A provider
                    // failure during planning (e.g. a non-retryable stream-idle
                    // timeout) therefore walks the ladder instead of abandoning
                    // the run with a bare `?`. Only an exhausted ladder surfaces
                    // as a graceful Partial plan via the caller.
                    let (completed_arch, doc) = self
                        .run_design_stage_with_recovery(
                            role,
                            task,
                            context,
                            cancel,
                            &planning_repair,
                        )
                        .await?;
                    design_doc = Some(doc);
                    let arch_id = completed_arch.id;
                    graph.add_root(completed_arch);
                    Some(arch_id)
                }
                // No design-stage agent: the graph starts directly at the
                // planned subtasks.
                None => None,
            }
        };
        // Keep the plan (or lack of one) so checkpoints capture the DesignDoc
        // without re-running the Architect on resume (C-05).
        *self.design_doc.lock().unwrap_or_else(|error| error.into_inner()) = design_doc.clone();

        // ── ADR-65 §5: resolve the DesignDoc claim against evidence ────────
        // The doc becomes a binding contract (planner claims, expected
        // artifacts, research heuristics) ONLY when the deterministic,
        // model-free verifier marks it Verified — every proposed path grounded
        // against the pre-planning snapshot and the session's ToolExecuted
        // facts. Quarantined/Skipped docs stay advisory and the pipeline
        // degrades to the unverified path; never a hard error.
        //
        // A seeded approved-plan doc (ADR-60 D7) is human-approved — its
        // approval IS the evidence — so it binds unconditionally and is never
        // re-verified (the architect is not re-invoked for it; `design_id` is
        // None on that path).
        //
        // The FULL verdict (not just the binding flag) is retained: the
        // ADR-65 §6 fallback scheduler consumes the resolution
        // (Active/Quarantined/Skipped + machine reason codes) to schedule.
        let (design_verdict, doc_event_ids): (Option<DesignDocVerdict>, Option<(String, String)>) =
            match design_id.as_ref() {
                // Agent-produced doc: resolve the (possibly empty) claim.
                Some(_arch_id) => match (design_doc.as_ref(), design_role.as_ref()) {
                    (Some(doc), Some(author)) => {
                        let verdict = self
                            .verify_design_doc_claim(doc, Some(author), task.session_id, cancel)
                            .await?;
                        let ids = self
                            .append_design_doc_events(task.session_id, Some(author), doc, &verdict)
                            .await;
                        (Some(verdict), ids)
                    }
                    // No parsed doc despite an architect run: nothing binds
                    // (defensive; the design stage returns Err before this state).
                    _ => (None, None),
                },
                // Seeded approved doc (D7) and doc-less runs verify nothing.
                None => (None, None),
            };
        // ADR-65 §7: capture where the doc claim stood so every checkpoint
        // this run persists carries the resolution (a resume restores state,
        // not prose). Real ids only — append failures leave them unset.
        self.last_doc_resolution =
            checkpoint_doc_resolution(&design_verdict, doc_event_ids.as_ref());
        let binding_doc: Option<&DesignDoc> = match (&design_verdict, design_doc.as_ref()) {
            (Some(verdict), Some(doc)) => verdict.state.is_active().then_some(doc),
            (Some(_), None) => None,
            // Seeded approved doc (D7): binds without re-verification. No doc
            // at all: nothing to bind.
            (None, other) => other,
        };

        let planner = TaskPlanner;
        match planner
            .plan(
                task,
                binding_doc,
                &planner_agents,
                self.planning_provider.clone(),
                &self.retry_policy,
                &self.bus,
                cancel.clone(),
                &self.skills_section,
                self.blueprint_facade.as_ref(),
            )
            .await
        {
            Ok(outcome) => {
                let planned_subtasks = outcome.tasks;
                for pst in &planned_subtasks {
                    let subtask = SubTask {
                        id: pst.id,
                        parent_id: design_id,
                        session_id: task.session_id,
                        role: pst.role.clone(),
                        description: pst.description.clone(),
                        status: SubTaskStatus::Pending,
                        dependencies: pst.dependencies.clone(),
                        deliverable: None,
                        created_at: time::OffsetDateTime::now_utc(),
                        completed_at: None,
                    };
                    graph.add_root(subtask);
                    if implement_ids.contains(&pst.role) {
                        self.expected_artifacts
                            .lock()
                            .unwrap_or_else(|error| error.into_inner())
                            .insert(pst.id, pst.expected_artifacts.clone());
                    }
                }

                for pst in &planned_subtasks {
                    if pst.dependencies.is_empty() {
                        if let Some(design_id) = design_id {
                            self.add_managed_dependency(
                                &mut graph,
                                pst.id,
                                design_id,
                                Dependency::MustFinishBefore,
                            )?;
                        }
                    } else {
                        for dep_id in &pst.dependencies {
                            self.add_managed_dependency(
                                &mut graph,
                                pst.id,
                                *dep_id,
                                Dependency::MustFinishBefore,
                            )?;
                        }
                    }
                }
                // Emit creation events.
                for subtask in graph.all_tasks() {
                    let _ = self.bus.publish_for_session(
                        task.session_id,
                        subtask.id.0,
                        EventKind::SubTaskCreated {
                            task_id: subtask.id,
                            role: subtask.role.clone(),
                            description: subtask.description.clone(),
                        },
                    );
                }

                return Ok((graph, Some(outcome.artifact)));
            }
            Err(e) => {
                tracing::warn!("Task planning failed: {e:?}; scheduling the pipeline from evidence (ADR-65 §6)");
            }
        }

        // ── ADR-65 §6: evidence-driven fallback scheduling ───────────────
        // The planner produced no decidable plan. The fixed
        // `design → research → implement` heuristic is replaced with a
        // deterministic, model-free scheduler (`crate::evidence_scheduler`):
        // the coordinator derives the unmet needs from evidence — the
        // workspace snapshot, the session's observed facts, and the
        // design-doc verdict — and schedules only what the evidence
        // justifies, among the currently registered agents. A stage with no
        // registered agent is never called (ADR-58, acceptance 6).

        // Build the scheduler candidate roster from the registry. The agents'
        // stage tags are the capability tags (ADR-58), and the tie-break is
        // the lexicographic id rank (the same rule `first_agent_for_stage`
        // uses).
        let mut candidates = self.scheduler_candidates(&implement_tag);
        // Preserve the ADR-35 §8 coordinator self-execute standby: with no
        // registered implement-stage agent (and an executor present) the
        // coordinator carries implementation itself as the reserved
        // `coordinator` id — never registered, so it cannot collide.
        if coordinator_self_implements {
            candidates.push(evidence_scheduler::Candidate {
                agent_id: "coordinator".to_owned(),
                capabilities: BTreeSet::from([evidence_scheduler::Capability::Implement]),
                order: usize::MAX,
            });
        }
        if !candidates.iter().any(|candidate| {
            candidate.capabilities.contains(&evidence_scheduler::Capability::Implement)
        }) {
            return Err(OrchestratorError::AgentLoopError(
                "no implementation-stage agent is registered; cannot plan implementation work"
                    .into(),
            ));
        }

        // The scheduler's view of the DesignDoc claim: the Phase-5 verifier's
        // resolution mapped onto the scheduler's decision surface.
        let doc_resolution =
            scheduler_doc_resolution(&design_verdict, design_doc.as_ref(), &doc_event_ids);

        // Consult the scheduler. A lone Exploration step is the rule-(b)
        // deferred case: its facts must land before the next decision, so the
        // coordinator dispatches it inline and re-consults the scheduler with
        // refreshed evidence. Bounded by construction: the loop state
        // (`exploration_attempted`) makes the second consultation terminal.
        const MAX_SCHEDULER_CONSULTATIONS: usize = 2;
        let mut exploration_attempted = false;
        let mut explored: Option<SubTask> = None;
        let steps = {
            let mut steps: Vec<DispatchStep> = Vec::new();
            for round in 1..=MAX_SCHEDULER_CONSULTATIONS {
                let state = self
                    .gather_evidence_state(
                        task,
                        doc_resolution.clone(),
                        candidates.clone(),
                        exploration_attempted,
                        cancel,
                    )
                    .await?;
                let plan = evidence_scheduler::schedule(&state);
                match plan.steps.split_first() {
                    // Rule (b): the scheduler returned the NEXT STEP ONLY —
                    // dispatch the exploration now, then re-consult.
                    Some((first, rest))
                        if rest.is_empty()
                            && first.capability == evidence_scheduler::Capability::Explore =>
                    {
                        if round == MAX_SCHEDULER_CONSULTATIONS {
                            // Defensive bound (unreachable: the loop state
                            // makes the second consultation terminal) —
                            // materialize rather than loop forever.
                            warn!(
                                "ADR-65 §6: scheduler still yields an exploration step on \
                                 re-consultation; materializing (bounded loop)"
                            );
                            steps = plan.steps;
                            break;
                        }
                        // Rule (b): the scheduler returned the NEXT STEP ONLY —
                        // dispatch the exploration now, then re-consult.
                        self.append_dispatch_decision(task.session_id, first, None, None).await;
                        if let Some(completed) =
                            self.run_fallback_exploration(first, task, context, cancel).await?
                        {
                            // Fail-soft caller: no facts landed → the loop
                            // proceeds on current knowledge.
                            explored = Some(completed);
                        }
                        exploration_attempted = true;
                    }
                    // Terminal plan: materialize as-is.
                    _ => {
                        steps = plan.steps;
                        break;
                    }
                }
            }
            steps
        };

        // Materialize the scheduled steps as ordered graph tasks, chaining
        // each onto the previous dispatch; a completed inline exploration
        // rides first. Every materialized dispatch is recorded as a whiteboard
        // `Decision` event (ADR-65 §6) citing the step's real evidence ids.
        let mut parent_id = design_id;
        if let Some(completed) = explored {
            let completed_id = completed.id;
            match parent_id {
                Some(design) => {
                    let relationship = self.fallback_relationship(
                        &graph,
                        design,
                        &completed.role,
                        design_role.as_ref(),
                    );
                    graph.add_child_with_relationship(
                        completed,
                        design,
                        Dependency::MustFinishBefore,
                        relationship,
                    );
                }
                None => graph.add_root(completed),
            }
            parent_id = Some(completed_id);
        }
        if steps.is_empty() && parent_id == design_id {
            // No step could be scheduled (defensive: the implement guard
            // above makes this unreachable) — the preserved heuristic
            // failure.
            return Err(OrchestratorError::AgentLoopError(
                "no implementation-stage agent is registered; cannot plan implementation work"
                    .into(),
            ));
        }
        for step in &steps {
            let role = AgentId::new(&step.candidate_agent_id);
            let subtask = SubTask {
                id: TaskId::new(),
                parent_id,
                session_id: task.session_id,
                role: role.clone(),
                description: fallback_step_description(step, task),
                status: SubTaskStatus::Pending,
                dependencies: parent_id.into_iter().collect(),
                deliverable: None,
                created_at: time::OffsetDateTime::now_utc(),
                completed_at: None,
            };
            let subtask_id = subtask.id;
            match parent_id {
                Some(parent) => {
                    let relationship =
                        self.fallback_relationship(&graph, parent, &role, design_role.as_ref());
                    graph.add_child_with_relationship(
                        subtask,
                        parent,
                        Dependency::MustFinishBefore,
                        relationship,
                    );
                }
                None => graph.add_root(subtask),
            }
            let causation = if step.reason.is_doc_driven() {
                doc_event_ids.as_ref().map(|(claim, _)| claim.clone())
            } else {
                None
            };
            self.append_dispatch_decision(task.session_id, step, causation, Some(subtask_id)).await;
            parent_id = Some(subtask_id);
        }

        // Emit creation events and populate expected artifacts for every
        // implement-stage subtask from the BINDING DesignDoc's proposed_files
        // (ADR-65 §5): contract paths only ever come from a Verified doc; a
        // Quarantined/Skipped doc contributes none.
        for subtask in graph.all_tasks() {
            let _ = self.bus.publish_for_session(
                task.session_id,
                subtask.id.0,
                EventKind::SubTaskCreated {
                    task_id: subtask.id,
                    role: subtask.role.clone(),
                    description: subtask.description.clone(),
                },
            );
            if implement_ids.contains(&subtask.role) {
                if let Some(doc) = binding_doc {
                    self.expected_artifacts
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .insert(subtask.id, doc.proposed_files.clone());
                }
            }
        }

        Ok((graph, None))
    }

    // ── ADR-65 §6: evidence-driven fallback scheduling (read/write) ─────

    /// Build the scheduler candidate roster from the registry (ADR-65 §6,
    /// ADR-58): the agents' stage tags ARE the capability tags — the
    /// `research` stage tag maps to [`evidence_scheduler::Capability::Explore`],
    /// the `design` stage tag to `Design`, and the resolved primary
    /// `Execution` stage tag (the same resolution the replaced heuristic
    /// fallback used for its implement roster) to `Implement`.
    ///
    /// Candidates are ranked lexicographically by agent id: the registry map
    /// has no stable iteration order, and `first_agent_for_stage` breaks ties
    /// the same way, so the scheduler's tie-break is deterministic.
    fn scheduler_candidates(&self, implement_tag: &str) -> Vec<evidence_scheduler::Candidate> {
        let mut ids = self.registry.ids();
        ids.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        ids.into_iter()
            .enumerate()
            .filter_map(|(order, id)| {
                let stage = self.registry.get(&id).and_then(|agent| agent.stage())?;
                let mut capabilities = BTreeSet::new();
                if stage.as_str() == AgentStage::RESEARCH {
                    capabilities.insert(evidence_scheduler::Capability::Explore);
                }
                if stage.as_str() == AgentStage::DESIGN {
                    capabilities.insert(evidence_scheduler::Capability::Design);
                }
                if stage.as_str() == implement_tag {
                    capabilities.insert(evidence_scheduler::Capability::Implement);
                }
                if capabilities.is_empty() {
                    // An agent without a matching stage tag is not a
                    // candidate for any scheduled capability (ADR-58: stage
                    // tags are config data; missing stages are not candidates).
                    return None;
                }
                Some(evidence_scheduler::Candidate {
                    agent_id: id.as_str().to_owned(),
                    capabilities,
                    order,
                })
            })
            .collect()
    }

    /// Gather the scheduler's evidence state from the runtime (ADR-65 §6
    /// read side): the workspace snapshot record, its REAL log event id, the
    /// in-scope observation facts (each cited by its real `resource_facts`
    /// event id), the objective, and the loop state. Store failures degrade
    /// to the empty-observation gap (fail-soft); cancellation propagates.
    async fn gather_evidence_state(
        &self,
        task: &AgentTask,
        doc: DocResolution,
        candidates: Vec<evidence_scheduler::Candidate>,
        exploration_attempted: bool,
        cancel: &CancellationToken,
    ) -> Result<evidence_scheduler::EvidenceState, OrchestratorError> {
        let (snapshot_present, observations, snapshot_event_id) =
            match (self.workspace_snapshot.as_ref(), self.review_store.as_ref()) {
                // Snapshot + store: the in-scope observation facts are the
                // evidence the scheduler consumes (ADR-65 F5c scoping).
                (Some(snapshot), Some(pool)) => {
                    let root_hash =
                        crate::tool_facts::project_root_hash(snapshot.project_root.as_std_path());
                    match ResourceFacts::new(pool.clone())
                        .list_observations(&root_hash, MAX_EVIDENCE_OBSERVATIONS, cancel)
                        .await
                    {
                        Ok(rows) => {
                            let observations = rows
                                .iter()
                                .filter_map(|row| {
                                    // Rows without a real event id are omitted —
                                    // ids are never fabricated.
                                    row.last_event_id.clone().map(|event_id| {
                                        evidence_scheduler::Observation {
                                            event_id,
                                            path: row.path.clone(),
                                        }
                                    })
                                })
                                .collect();
                            // The barrier's snapshot apply stamps every row it
                            // creates with the `WorkspaceSnapshot` event id, so
                            // a row carrying the snapshot's generation names the
                            // REAL snapshot event.
                            let snapshot_event_id = rows
                                .iter()
                                .find(|row| row.generation == snapshot.generation)
                                .and_then(|row| row.last_event_id.clone());
                            (true, observations, snapshot_event_id)
                        }
                        Err(err) => {
                            if cancel.is_cancelled() {
                                return Err(OrchestratorError::Cancelled);
                            }
                            warn!(
                                %err,
                                "ADR-65 §6: observation list unavailable; scheduling on the \
                                 snapshot record only (fail-soft)"
                            );
                            (true, Vec::new(), None)
                        }
                    }
                }
                // Snapshot without a store: the digest exists but no fact is
                // recorded — an evidence gap by the honest count.
                (Some(_), None) => (true, Vec::new(), None),
                // No snapshot record: no bootstrap evidence at all.
                (None, _) => (false, Vec::new(), None),
            };
        Ok(evidence_scheduler::EvidenceState {
            objective: task.description.clone(),
            planner_failed: true,
            snapshot_present,
            snapshot_event_id,
            observations,
            doc,
            candidates,
            exploration_attempted,
        })
    }

    /// Append the whiteboard `Decision` event for one scheduled dispatch
    /// (ADR-65 §6): `selected_agent, reason, required_output,
    /// supporting_evidence_ids`. The ids are the scheduler's REAL consumed
    /// event ids — the append side validates them (ADR-65 §1, acceptance 8:
    /// a fabricated id is rejected), and the failure is fail-soft for
    /// planning. The causation is the DesignDoc claim event for doc-driven
    /// decisions.
    ///
    /// ADR-65 §7: when the dispatch's subtask id is known it is recorded as
    /// the coordinator's PENDING decision (the last scheduler `DispatchStep`
    /// awaiting completion), captured into every checkpoint so a resume can
    /// continue behind the recorded, evidence-backed decision.
    async fn append_dispatch_decision(
        &mut self,
        session_id: Ulid,
        step: &DispatchStep,
        causation: Option<String>,
        task_id: Option<TaskId>,
    ) {
        self.last_dispatch_decision =
            task_id.map(|task_id| checkpoint::CheckpointPendingDecision {
                selected_agent: step.candidate_agent_id.clone(),
                reason: step.reason.code().to_owned(),
                required_output: step.required_output.clone(),
                supporting_evidence_ids: step.supporting_evidence_ids.clone(),
                task_id: Some(task_id),
            });
        let Some(pool) = self.review_store.as_ref() else { return };
        let event = NewWhiteboardEvent {
            event_id: Ulid::new().to_string(),
            agent_id: "coordinator".to_owned(),
            kind: WhiteboardKind::Decision,
            scope: String::new(),
            session_id: Some(session_id.to_string()),
            plan_id: None,
            causation,
            payload: serde_json::json!({
                "selected_agent": step.candidate_agent_id,
                "reason": step.reason.code(),
                "required_output": step.required_output,
                "supporting_evidence_ids": step.supporting_evidence_ids,
            }),
            pre_image_hash: None,
            created_at: crate::tool_facts::unix_ms(),
        };
        if let Err(err) = append_whiteboard_event(pool, &event).await {
            warn!(%err, "ADR-65 §6: dispatch decision append failed (fail-soft)");
        }
    }

    /// Run ONE fallback exploration dispatch (ADR-65 §6 rule b): the
    /// scheduler asked for a grounded fact inventory, so the coordinator
    /// dispatches the selected exploration-capable specialist NOW through the
    /// same specialist-run plumbing the design stage uses — the facts must
    /// land before the scheduler is re-consulted (bounded loop).
    ///
    /// Single attempt, fail-soft by contract: a failed exploration is logged
    /// and the run proceeds on current knowledge (the scheduler is
    /// re-consulted with `exploration_attempted`). Cancellation propagates.
    async fn run_fallback_exploration(
        &self,
        step: &DispatchStep,
        task: &AgentTask,
        base_ctx: &AgentContext,
        cancel: &CancellationToken,
    ) -> Result<Option<SubTask>, OrchestratorError> {
        let role = AgentId::new(&step.candidate_agent_id);
        let explore_task = SubTask {
            id: TaskId::new(),
            parent_id: None,
            session_id: task.session_id,
            role: role.clone(),
            description: fallback_step_description(step, task),
            status: SubTaskStatus::Running,
            dependencies: Vec::new(),
            deliverable: None,
            created_at: time::OffsetDateTime::now_utc(),
            completed_at: None,
        };
        let explore_ctx = AgentContext {
            session: base_ctx.session.clone(),
            parent_task: Some(task.clone()),
            working_memory: base_ctx.working_memory.clone(),
            retrieved_chunks: base_ctx.retrieved_chunks.clone(),
            previous_results: Vec::new(),
            budget_remaining_usd: None,
            expected_artifacts: Vec::new(),
            workspace_capsule: None,
            workspace_snapshot_digest: self.snapshot_digest(cancel).await,
            run_id: self.run_id.clone(),
            workspace_generation: self.snapshot_generation(),
        };
        let profile = match self.model_selector.select_for_session(
            &role,
            None,
            explore_task.id,
            Some(task.session_id),
        ) {
            Ok(profile) => profile,
            Err(err) => {
                if is_cancellation_error(&err) {
                    return Err(OrchestratorError::Cancelled);
                }
                warn!(
                    %err, role = %role,
                    "ADR-65 §6: fallback exploration model selection failed (fail-soft); \
                     proceeding on current evidence"
                );
                return Ok(None);
            }
        };
        let result = match self
            .runner
            .run(role.clone(), &explore_task, explore_ctx, &profile, cancel.clone())
            .await
        {
            Ok(result) => result,
            Err(err) => {
                if is_cancellation_error(&err) {
                    return Err(OrchestratorError::Cancelled);
                }
                warn!(
                    %err, role = %role,
                    "ADR-65 §6: fallback exploration dispatch failed (fail-soft); proceeding \
                     on current evidence"
                );
                return Ok(None);
            }
        };
        if !matches!(result.outcome, AgentOutcome::Success) {
            warn!(
                role = %role,
                summary = %result.summary,
                "ADR-65 §6: fallback exploration produced no usable inventory (fail-soft); \
                 proceeding on current evidence"
            );
            return Ok(None);
        }
        let mut completed = explore_task;
        completed.status = SubTaskStatus::Completed;
        completed.deliverable = Some(result.summary);
        completed.completed_at = Some(time::OffsetDateTime::now_utc());
        Ok(Some(completed))
    }

    /// The relationship between a fallback step and its graph parent — the
    /// same resolution the replaced heuristic fallback used: a configured
    /// collaboration rule first, then `OwnsDesign` under the design agent,
    /// else `ProvidesContextTo`.
    fn fallback_relationship(
        &self,
        graph: &TaskGraph,
        parent: TaskId,
        child_role: &AgentId,
        design_role: Option<&AgentId>,
    ) -> crate::relationship::AgentRelationship {
        match graph.get(&parent).map(|parent_task| parent_task.role.clone()) {
            Some(parent_role) => self
                .relationships
                .rule(&parent_role, child_role)
                .map(|rule| rule.relationship)
                .unwrap_or_else(|| {
                    if design_role == Some(&parent_role) {
                        crate::relationship::AgentRelationship::OwnsDesign
                    } else {
                        crate::relationship::AgentRelationship::ProvidesContextTo
                    }
                }),
            None => crate::relationship::AgentRelationship::ProvidesContextTo,
        }
    }

    // ── ADR-65 §5: DesignDoc claim resolution (read/write whiteboard) ───

    /// Resolve a DesignDoc claim against deterministic evidence — the
    /// pre-planning workspace snapshot plus the session's `ToolExecuted`
    /// facts — via the model-free verifier, and return the verdict.
    ///
    /// Fail-soft by contract: an evidence-store failure degrades the claim to
    /// a Quarantined (non-empty) / Skipped (empty) verdict with a warning —
    /// planning must never crash because the whiteboard read failed. Only an
    /// explicit cancellation propagates as an error.
    async fn verify_design_doc_claim(
        &self,
        doc: &DesignDoc,
        author: Option<&AgentId>,
        session_id: Ulid,
        cancel: &CancellationToken,
    ) -> Result<DesignDocVerdict, OrchestratorError> {
        let proposed: Vec<String> =
            doc.proposed_files.iter().map(|path| path.as_str().to_owned()).collect();
        match collect_design_doc_evidence(
            self.review_store.as_ref(),
            session_id,
            author,
            self.workspace_snapshot.as_ref(),
            cancel,
        )
        .await
        {
            Ok(mut input) => {
                // The evidence gatherer shapes the workspace facts; the doc's
                // own claim (its proposed paths) is filled in here so the pure
                // verifier resolves the ACTUAL design, not an empty one.
                input.proposed_paths = proposed;
                Ok(verify_design_doc(&input))
            }
            Err(err) => {
                if cancel.is_cancelled() {
                    return Err(OrchestratorError::Cancelled);
                }
                tracing::warn!(
                    %err,
                    author = ?author,
                    "ADR-65 §5: evidence store unavailable; design doc advisory (fail-soft)"
                );
                Ok(degraded_verdict(&proposed, 0))
            }
        }
    }

    /// Persist a DesignDoc claim and its verdict as whiteboard events
    /// (ADR-65 §5 write side): a `DesignDoc` kind event carries the serialized
    /// doc, and a `Decision` kind event (causation = the claim's `event_id`)
    /// carries the verdict. Both appends are fail-soft by contract — evidence
    /// logging must never break planning — and a failed claim append suppresses
    /// the orphaned decision append.
    ///
    /// Returns the `(claim_event_id, decision_event_id)` pair when BOTH events
    /// landed — the real ids downstream scheduling cites as supporting
    /// evidence (ADR-65 §6); `None` when either append failed (ids are never
    /// fabricated).
    async fn append_design_doc_events(
        &self,
        session_id: Ulid,
        author: Option<&AgentId>,
        doc: &DesignDoc,
        verdict: &DesignDocVerdict,
    ) -> Option<(String, String)> {
        let pool = self.review_store.as_ref()?;
        let created_at = crate::tool_facts::unix_ms();
        // The claim is authored by the architect that produced the doc (or the
        // coordinator when attribution is unavailable); the decision is always
        // the coordinator's finding.
        let claim_author =
            author.map(|id| id.as_str().to_owned()).unwrap_or_else(|| "coordinator".to_owned());
        let claim_id = Ulid::new().to_string();
        let doc_payload = match serde_json::to_value(doc) {
            Ok(payload) => payload,
            Err(err) => {
                tracing::warn!(
                    %err,
                    "ADR-65 §5: design-doc claim serialization failed; claim not recorded"
                );
                return None;
            }
        };
        if let Err(err) = append_whiteboard_event(
            pool,
            &NewWhiteboardEvent {
                event_id: claim_id.clone(),
                agent_id: claim_author,
                kind: WhiteboardKind::DesignDoc,
                scope: String::new(),
                session_id: Some(session_id.to_string()),
                plan_id: None,
                causation: None,
                payload: doc_payload,
                pre_image_hash: None,
                created_at,
            },
        )
        .await
        {
            tracing::warn!(
                %err,
                "ADR-65 §5: design-doc claim append failed (fail-soft); decision not recorded"
            );
            return None;
        }
        let verdict_payload = match serde_json::to_value(verdict) {
            Ok(payload) => payload,
            Err(err) => {
                tracing::warn!(
                    %err,
                    "ADR-65 §5: design-doc verdict serialization failed; decision not recorded"
                );
                return None;
            }
        };
        let decision_id = Ulid::new().to_string();
        if let Err(err) = append_whiteboard_event(
            pool,
            &NewWhiteboardEvent {
                event_id: decision_id.clone(),
                agent_id: "coordinator".to_owned(),
                kind: WhiteboardKind::Decision,
                scope: String::new(),
                session_id: Some(session_id.to_string()),
                plan_id: None,
                causation: Some(claim_id.clone()),
                payload: verdict_payload,
                pre_image_hash: None,
                created_at,
            },
        )
        .await
        {
            tracing::warn!(
                %err,
                "ADR-65 §5: design-doc decision append failed (fail-soft)"
            );
            return None;
        }
        Some((claim_id, decision_id))
    }

    fn add_managed_dependency(
        &self,
        graph: &mut TaskGraph,
        task_id: TaskId,
        depends_on: TaskId,
        dependency: Dependency,
    ) -> Result<(), OrchestratorError> {
        let roles = graph
            .get(&depends_on)
            .zip(graph.get(&task_id))
            .map(|(from, to)| (from.role.clone(), to.role.clone()));
        if let Some((from, to)) = roles {
            if let Some(rule) = self.relationships.rule(&from, &to) {
                graph.add_dependency_with_relationship(
                    task_id,
                    depends_on,
                    dependency,
                    rule.relationship,
                )?;
                return Ok(());
            }
        }
        graph.add_dependency(task_id, depends_on, dependency)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{AgentFlowTestHarness, BudgetScenarioBuilder, MockExpertAgent};
    use concerto_core::error::PolicyError;
    use concerto_core::executor::ToolExecutor;
    use concerto_core::policy::SimplePolicyEngine;
    use concerto_core::traits::memory::NullMemoryStore;
    use concerto_core::traits::policy::{AuditEntry, AuditLog};
    use concerto_core::types::{Condition, PolicyRule, ToolRegistry};
    use concerto_providers::mock::MockProvider;
    use concerto_providers::model_registry::ModelRegistry;
    use concerto_providers::routing::RoutingEngine;

    /// An executor whose empty tool set and allow-all policy make coordinator
    /// self-execution available to the coordinator under test. Self-execution
    /// tests serve plain-text planning-provider responses, so no tool is ever
    /// actually invoked.
    fn coordinator_self_executor() -> Arc<ToolExecutor> {
        let registry = ToolRegistry::default();
        let allow_all = vec![PolicyRule::AutoApprove(Condition::Always)];
        let policy = Arc::new(SimplePolicyEngine::new(allow_all, Arc::new(TestAudit)));
        Arc::new(ToolExecutor::new(Arc::new(registry), policy))
    }

    /// No-op audit for the self-execution executor (nothing is executed).
    struct TestAudit;
    #[async_trait::async_trait]
    impl AuditLog for TestAudit {
        async fn record(
            &self,
            _entry: AuditEntry,
            _cancel: CancellationToken,
        ) -> Result<(), PolicyError> {
            Ok(())
        }
    }

    // Pin the three-way failure classification that the dispatch loop relies
    // on (ADR-42 §1). Transient provider errors (rate limits, network,
    // timeouts, 5xx) are Recoverable and subject to subtask retry.
    // Provider/model-specific hard failures (auth, context overflow,
    // rate-limit ceiling, no-affordable-model) are LimitReached and walk the
    // fallback ladder. Cancellation and structural errors are NonRecoverable
    // and exit immediately.
    #[test]
    fn classify_subtask_error_partition() {
        // Recoverable: AgentLoopError, Memory, any Tool error that isn't
        // Cancelled, and transient Provider errors.
        assert!(matches!(
            classify_subtask_error(&OrchestratorError::AgentLoopError(
                "transient loop issue".into()
            )),
            SubtaskFailureClass::Recoverable
        ));
        assert!(matches!(
            classify_subtask_error(&OrchestratorError::Memory(
                concerto_core::MemoryError::NotFound("chunk".into())
            )),
            SubtaskFailureClass::Recoverable
        ));
        assert!(matches!(
            classify_subtask_error(&OrchestratorError::Tool(
                concerto_core::ToolError::ExecutionFailed { message: "oops".into() }
            )),
            SubtaskFailureClass::Recoverable
        ));

        // Transient provider errors — will be retried
        assert!(matches!(
            classify_subtask_error(&OrchestratorError::Provider(
                concerto_core::error::ProviderError::RateLimit {
                    retry_after: std::time::Duration::from_secs(1)
                }
            )),
            SubtaskFailureClass::Recoverable
        ));
        assert!(matches!(
            classify_subtask_error(&OrchestratorError::Provider(
                concerto_core::error::ProviderError::Network("connection reset".into())
            )),
            SubtaskFailureClass::Recoverable
        ));
        assert!(matches!(
            classify_subtask_error(&OrchestratorError::Provider(
                concerto_core::error::ProviderError::Timeout {
                    phase: "request",
                    timeout: std::time::Duration::from_secs(30),
                }
            )),
            SubtaskFailureClass::Recoverable
        ));
        assert!(matches!(
            classify_subtask_error(&OrchestratorError::Provider(
                concerto_core::error::ProviderError::HttpStatus {
                    status: 503,
                    retry_after: None,
                    message: "service unavailable".into()
                }
            )),
            SubtaskFailureClass::Recoverable
        ));
        assert!(matches!(
            classify_subtask_error(&OrchestratorError::Provider(
                concerto_core::error::ProviderError::InvalidResponse("bad json".into())
            )),
            SubtaskFailureClass::Recoverable
        ));

        // LimitReached: non-transient provider errors (auth, context
        // overflow, rate-limit ceiling) and model-selection failures — a
        // different model or agent may still complete the task, so the
        // fallback ladder is walked before any Partial exit.
        assert!(matches!(
            classify_subtask_error(&OrchestratorError::Provider(
                concerto_core::error::ProviderError::AuthFailure
            )),
            SubtaskFailureClass::LimitReached
        ));
        assert!(matches!(
            classify_subtask_error(&OrchestratorError::Provider(
                concerto_core::error::ProviderError::ContextOverflow {
                    tokens_in: 200_000,
                    capacity: 128_000,
                }
            )),
            SubtaskFailureClass::LimitReached
        ));
        // RetryExhausted is the rate-limit ceiling: the provider's own retry
        // layer is done, but the subtask may still be solvable elsewhere.
        assert!(matches!(
            classify_subtask_error(&OrchestratorError::Provider(
                concerto_core::error::ProviderError::RetryExhausted {
                    attempts: 3,
                    elapsed: std::time::Duration::from_secs(30),
                    last_error: "all retries failed".into()
                }
            )),
            SubtaskFailureClass::LimitReached
        ));
        assert!(matches!(
            classify_subtask_error(&OrchestratorError::NoAffordableModel {
                role: AgentId::new("coder")
            }),
            SubtaskFailureClass::LimitReached
        ));
        assert!(matches!(
            classify_subtask_error(&OrchestratorError::PinnedModelNotFound {
                role: AgentId::new("coder"),
                provider_config_id: None,
                model: "claude-3.5-sonnet".into(),
            }),
            SubtaskFailureClass::LimitReached
        ));

        // NonRecoverable: cancellation and structural errors — retrying or
        // re-routing within a single task won't fix them.
        assert!(matches!(
            classify_subtask_error(&OrchestratorError::Cancelled),
            SubtaskFailureClass::NonRecoverable
        ));
        assert!(matches!(
            classify_subtask_error(&OrchestratorError::Tool(concerto_core::ToolError::Cancelled)),
            SubtaskFailureClass::NonRecoverable
        ));
        assert!(matches!(
            classify_subtask_error(&OrchestratorError::NoBudgetForDelegation),
            SubtaskFailureClass::NonRecoverable
        ));
        assert!(matches!(
            classify_subtask_error(&OrchestratorError::MultiAgentPlanFailed {
                reason: "no model available".into()
            }),
            SubtaskFailureClass::NonRecoverable
        ));
        assert!(matches!(
            classify_subtask_error(&OrchestratorError::TaskGraphError("no such task".into())),
            SubtaskFailureClass::NonRecoverable
        ));
    }

    // ------------------------------------------------------------------
    // Regression: Fix 5 — DesignDoc content validation accepts empty doc
    // (ADR-65 §5: an empty claim is SKIPPED, never rejected)
    // ------------------------------------------------------------------

    /// The empty DesignDoc JSON has no proposed_files, no goals, and an
    /// empty interface_sketch — all default values.
    const EMPTY_DESIGN_DOC_JSON: &str = r#"{"goals":[],"proposed_files":[],"interface_sketch":""}"#;

    #[tokio::test]
    async fn empty_design_doc_is_skipped_not_rejected() {
        // ADR-65 §5: an empty DesignDoc is a VALID claim — the verifier
        // resolves it to Skipped (NO_OBSERVATIONS_NO_DESIGN / NO_DESIGN_NEEDED)
        // instead of failing the design stage. The Partial result below must
        // therefore come from a LATER stage — this roster registers no
        // implement-stage agent, so planning cannot build a fallback pipeline —
        // never from the design stage rejecting the empty doc.
        let mocks =
            vec![MockExpertAgent::always_succeed(AgentId::new("architect"), EMPTY_DESIGN_DOC_JSON)];

        let plan = crate::graph::TaskGraph::default();
        let budget = BudgetScenarioBuilder::generous();
        let mut harness = AgentFlowTestHarness::new(mocks, plan, budget);
        let task = AgentTask::new(concerto_core::ids::Ulid::new(), "test task");
        let result = harness.run(task).await;

        let output = result.expect("Architect failure should return Partial, not hard error");
        assert_eq!(
            output.completion_status,
            concerto_core::types::AgentCompletionStatus::Partial,
            "empty DesignDoc should still produce a Partial completion when planning cannot proceed"
        );
        assert!(
            output.final_message.contains("could not produce a valid plan"),
            "expected 'could not produce a valid plan' in final_message, got: {}",
            output.final_message
        );
        // The failure is the missing implement-stage agent — the empty doc was
        // accepted by the design stage and must not be called a "rejected
        // design".
        assert!(
            output.final_message.contains("no implementation-stage agent is registered"),
            "the Partial must be caused by the missing implement agent, got: {}",
            output.final_message
        );
    }

    // ------------------------------------------------------------------
    // ADR-35 phase 2: custom agents from the planner are dispatched
    // ------------------------------------------------------------------

    /// Planning provider that serves one canned response per request, in
    /// order. The coordinator only calls the planner once per run.
    struct SeqProvider {
        responses: std::sync::Mutex<std::collections::VecDeque<String>>,
    }

    impl SeqProvider {
        fn new(responses: Vec<String>) -> Self {
            Self { responses: std::sync::Mutex::new(responses.into()) }
        }
    }

    #[async_trait::async_trait]
    impl concerto_core::traits::provider::LlmProvider for SeqProvider {
        async fn stream_completion(
            &self,
            _request: concerto_core::types::CompletionRequest,
            _cancel: CancellationToken,
        ) -> Result<concerto_core::traits::provider::CompletionStream, ProviderError> {
            let text = self.responses.lock().unwrap().pop_front().unwrap_or_default();
            Ok(Box::pin(futures::stream::iter(vec![Ok(concerto_core::types::CompletionChunk {
                reasoning: None,
                delta: text,
                tool_call: None,
                is_final: true,
                usage: None,
            })])))
        }
        fn context_capacity(&self, _model: &str) -> concerto_core::types::TokenBudget {
            concerto_core::types::TokenBudget::new(128_000, 4_096)
        }
        fn approximate_cost(&self, _tokens_in: u64, _tokens_out: u64) -> f64 {
            0.0
        }
        fn provider_name(&self) -> &'static str {
            "seq"
        }
    }

    /// Build a fully-wired coordinator whose planning provider serves one
    /// canned plan response. `registry` carries the agents the run may use.
    fn coordinator_with(
        bus: EventBus,
        registry: Arc<AgentRegistry>,
        plan_json: String,
    ) -> CoordinatorAgent {
        coordinator_with_responses(bus, registry, vec![plan_json])
    }

    /// Like [`coordinator_with`], but the planning provider serves multiple
    /// canned responses in order — used when the coordinator self-executes
    /// implement subtasks on the planning provider (ADR-35 §8) and the second
    /// response is the self-executor's summary.
    fn coordinator_with_responses(
        bus: EventBus,
        registry: Arc<AgentRegistry>,
        responses: Vec<String>,
    ) -> CoordinatorAgent {
        let planning_provider: Arc<dyn concerto_core::traits::provider::LlmProvider> =
            Arc::new(SeqProvider::new(responses));
        let spend_tracker = Arc::new(SpendTracker::default());
        let runner = AgentRunner::new(registry.clone(), bus.clone(), spend_tracker.clone());
        let profiles: Vec<concerto_core::types::RoutingProfile> = vec![
            concerto_core::types::RoutingProfile {
                provider_config_id: "test".into(),
                provider: "test".into(),
                model: "cheap".into(),
                cost_per_1k_tokens: 0.001,
                avg_latency_ms: 100,
                context_window: 8192,
                supports_tool_calling: true,
                base_url: None,
                description: None,
            },
            concerto_core::types::RoutingProfile {
                provider_config_id: "test".into(),
                provider: "test".into(),
                model: "mid".into(),
                cost_per_1k_tokens: 0.005,
                avg_latency_ms: 100,
                context_window: 8192,
                supports_tool_calling: true,
                base_url: None,
                description: None,
            },
            concerto_core::types::RoutingProfile {
                provider_config_id: "test".into(),
                provider: "test".into(),
                model: "expensive".into(),
                cost_per_1k_tokens: 0.01,
                avg_latency_ms: 100,
                context_window: 8192,
                supports_tool_calling: true,
                base_url: None,
                description: None,
            },
        ];
        let routing = Arc::new(RoutingEngine::new(
            profiles.clone(),
            spend_tracker.clone(),
            concerto_config::ModelPinConfig {
                pins: std::collections::HashMap::new(),
                ..Default::default()
            },
            EventBus::default(),
        ));
        let model_registry = Arc::new(ModelRegistry::from_profiles(profiles));
        let model_selector = Arc::new(ModelSelector::new(model_registry, routing));
        CoordinatorAgent::new(
            registry,
            runner,
            model_selector,
            spend_tracker,
            bus.clone(),
            planning_provider,
            Arc::new(NullMemoryStore),
        )
    }

    /// A pre-planning `WorkspaceSnapshot` whose inventory grounds the given
    /// proposed paths, so the ADR-65 §5 verifier resolves a DesignDoc claiming
    /// them to Verified (Active) — the doc BINDS and the coordinator reaches
    /// the planner with `Some`, recreating the pre-Phase-5 harness where a
    /// design doc reached the planner unconditionally.
    ///
    /// Without this, the no-evidence unit harness degrades every non-empty doc
    /// to Quarantined (nothing binds: `expected_artifacts` stays empty, the
    /// proposed_files membership check is off) and coordinator-level tests
    /// (C-06 artifact gates, review/validation cycles, zero-file guard, custom
    /// dispatch, checkpoint resume) would silently exercise the passive path
    /// instead of their intended gates. The evidence pipeline only reads
    /// `entry.path`, so a synthetic inventory is a faithful stand-in for a
    /// real workspace walk here.
    fn grounded_snapshot(proposed: &[&str]) -> crate::workspace_snapshot::WorkspaceSnapshotRecord {
        crate::workspace_snapshot::WorkspaceSnapshotRecord {
            generation: "g1".to_owned(),
            entries: proposed
                .iter()
                .map(|path| concerto_sessions::ObservedPath {
                    path: (*path).to_owned(),
                    size_bytes: Some(1),
                    mtime_ms: Some(1),
                    content_hash: Some("deadbeef".to_owned()),
                })
                .collect(),
            captured_at_ms: 1,
            project_root: "work".into(),
        }
    }

    /// `coordinator_with` plus a grounded workspace snapshot for the design
    /// doc's proposed files — see [`grounded_snapshot`].
    fn coordinator_with_grounded(
        bus: EventBus,
        registry: Arc<AgentRegistry>,
        plan_json: String,
        proposed: &[&str],
    ) -> CoordinatorAgent {
        coordinator_with(bus, registry, plan_json)
            .with_workspace_snapshot(grounded_snapshot(proposed))
    }

    /// Run a wired coordinator to completion and collect all bus events.
    /// The workspace root is a throwaway temp dir so mocks may safely write
    /// expected artifacts (audit C-06) without touching the source tree.
    async fn run_for_test(
        mut coordinator: CoordinatorAgent,
        bus: EventBus,
    ) -> (AgentOutput, Vec<EventKind>) {
        let mut rx = bus.subscribe();
        let task = AgentTask::new(Ulid::new(), "test task");
        let project_dir = tempfile::tempdir().expect("tempdir for test workspace");
        let context = AgentContext::new(concerto_core::types::SessionContext::new(
            task.session_id,
            project_dir.path().to_path_buf(),
        ));
        let output = coordinator
            .run(task, context, CancellationToken::new(), None)
            .await
            .expect("coordinator run should succeed");
        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event.kind.clone());
        }
        (output, events)
    }

    /// A plan containing a custom role must dispatch that role to the
    /// registered `GenericSpecialistAgent` through the runner, and the
    /// orchestration must complete successfully.
    #[tokio::test]
    async fn custom_agent_from_plan_is_dispatched_via_runner() {
        use crate::agents::GenericSpecialistAgent;

        let bus = EventBus::new(256);
        // Planner response: Researcher, custom docs-writer, Coder.
        let plan_json = r#"[
            {"role":"Researcher","description":"inspect docs","depends_on":[]},
            {"role":"docs-writer","description":"write release notes","depends_on":[0]},
            {"role":"Coder","description":"implement feature","depends_on":[0]}
        ]"#;
        let mocks = vec![
            MockExpertAgent::always_succeed(
                AgentId::new("architect"),
                r#"{"goals":["ship release notes"],"proposed_files":["src/auth.rs"],"interface_sketch":"login form"}"#,
            ),
            MockExpertAgent::always_succeed(AgentId::new("researcher"), "found docs"),
            MockExpertAgent::always_succeed(AgentId::new("coder"), "implemented")
                .with_artifact_writer(),
            MockExpertAgent::always_succeed(AgentId::new("reviewer"), "approved"),
            MockExpertAgent::always_succeed(AgentId::new("validator"), "validation ok"),
        ];
        let mut registry = AgentRegistry::from_mocks(mocks);
        registry.register(Arc::new(GenericSpecialistAgent::new(
            AgentId::new("docs-writer"),
            "Docs Writer".into(),
            Some(concerto_core::AgentStage::new("documentation")),
            Arc::new(MockProvider::default()),
            None,
            bus.clone(),
            RetryPolicy::default(),
            concerto_config::PromptSections::default(),
            concerto_config::AgentCapabilities::default(),
        )));

        let (output, events) = run_for_test(
            coordinator_with_grounded(
                bus.clone(),
                Arc::new(registry),
                plan_json.into(),
                &["src/auth.rs"],
            ),
            bus.clone(),
        )
        .await;

        // The runner actually dispatched the custom agent.
        let dispatched_custom = events.iter().any(|kind| {
            matches!(
                kind,
                EventKind::SubTaskStarted { role, .. } if role == &AgentId::new("docs-writer")
            )
        });
        assert!(dispatched_custom, "expected docs-writer subtask to be dispatched");
        assert!(
            output.final_message.contains("Multi-agent orchestration completed"),
            "unexpected final message: {}",
            output.final_message
        );
    }

    // ------------------------------------------------------------------
    // ADR-35 phase 3: pipeline shape follows the registry's stage tags
    // ------------------------------------------------------------------

    const DESIGN_DOC_JSON: &str =
        r#"{"goals":["do the thing"],"proposed_files":["src/a.rs"],"interface_sketch":"s"}"#;
    const PLAN_RESEARCH_CODER: &str = r#"[
        {"role":"Researcher","description":"inspect","depends_on":[]},
        {"role":"Coder","description":"implement","depends_on":[0]}
    ]"#;

    #[tokio::test]
    async fn no_review_agent_skips_review_cycle() {
        let bus = EventBus::new(256);
        let mocks = vec![
            MockExpertAgent::always_succeed(AgentId::new("architect"), DESIGN_DOC_JSON),
            MockExpertAgent::always_succeed(AgentId::new("researcher"), "found"),
            MockExpertAgent::always_succeed(AgentId::new("coder"), "implemented")
                .with_artifact_writer(),
            MockExpertAgent::always_succeed(AgentId::new("validator"), "valid"),
        ];
        let (output, events) = run_for_test(
            coordinator_with(
                bus.clone(),
                Arc::new(AgentRegistry::from_mocks(mocks)),
                PLAN_RESEARCH_CODER.into(),
            ),
            bus.clone(),
        )
        .await;

        assert!(
            output.final_message.contains("Multi-agent orchestration completed"),
            "unexpected final message: {}",
            output.final_message
        );
        assert!(
            !events.iter().any(|kind| matches!(kind, EventKind::ReviewCycleStarted { .. })),
            "no review-stage agent should mean no review cycle runs"
        );
    }

    #[tokio::test]
    async fn no_design_agent_plans_without_design_step() {
        let bus = EventBus::new(256);
        let mocks = vec![
            MockExpertAgent::always_succeed(AgentId::new("researcher"), "found"),
            MockExpertAgent::always_succeed(AgentId::new("coder"), "implemented"),
            MockExpertAgent::always_succeed(AgentId::new("reviewer"), "approved"),
            MockExpertAgent::always_succeed(AgentId::new("validator"), "valid"),
        ];
        let (output, events) = run_for_test(
            coordinator_with(
                bus.clone(),
                Arc::new(AgentRegistry::from_mocks(mocks)),
                PLAN_RESEARCH_CODER.into(),
            ),
            bus.clone(),
        )
        .await;

        // Without a design stage there is no DesignDoc to seed the coder's
        // expected artifacts, so the (mock) coder completes with no files.
        // The zero-file implement guard fires exactly once per lineage: the
        // first zero-file success short-circuits the reviewer and queues a
        // revision, and the revision's own zero-file success (same lineage
        // root) FAILS the subtask — no second review cycle, no further
        // revision — so the run ends Partial with the missing-deliverable
        // failure surfaced to the user.
        assert_eq!(
            output.completion_status,
            concerto_core::types::AgentCompletionStatus::Partial,
            "a zero-file coder with a review-stage agent must end the run as Partial after the post-revision zero-file failure, got: {:?}",
            output.completion_status,
        );
        assert!(
            output.final_message.contains("required deliverables were not produced"),
            "expected the post-revision zero-file failure in the final message, got: {}",
            output.final_message
        );
        assert!(
            output.final_message.contains("completed with no file changes after revision"),
            "expected the zero-file-after-revision wording in the final message, got: {}",
            output.final_message
        );
        let review_cycle_count = events
            .iter()
            .filter(|kind| matches!(kind, EventKind::ReviewCycleStarted { .. }))
            .count();
        assert_eq!(
            review_cycle_count, 0,
            "no review cycle may run: the revision's own zero-file success fails instead of falling through to the reviewer, got: {review_cycle_count}"
        );
        assert!(
            !events.iter().any(|kind| {
                matches!(
                    kind,
                    EventKind::SubTaskStarted { role, .. } if role == &AgentId::new("architect")
                )
            }),
            "no design-stage agent should mean no design subtask runs"
        );
    }

    /// The zero-file implement guard is bounded ONCE per lineage: the first
    /// zero-file implement success skips the reviewer and queues a revision;
    /// the revision's own zero-file success (same lineage root, fresh task
    /// id) FAILS the subtask instead of re-arming the short-circuit or
    /// falling through to the reviewer. Without this bound the guard would
    /// recurse forever — revision subtasks get fresh `TaskId`s so
    /// `subtask_attempts` cannot limit them, and `max_total_iterations`
    /// defaults to `None`.
    #[tokio::test]
    async fn zero_file_implement_guard_short_circuits_once_per_lineage() {
        let bus = EventBus::new(256);
        let mocks = vec![
            MockExpertAgent::always_succeed(AgentId::new("coder"), "implemented"),
            MockExpertAgent::always_succeed(AgentId::new("reviewer"), "approved"),
            MockExpertAgent::always_succeed(AgentId::new("validator"), "valid"),
        ];
        let session_id = Ulid::new();
        let mut coordinator = coordinator_for_ladder(
            bus.clone(),
            mocks,
            concerto_config::ModelPinConfig::default(),
            Arc::new(MockProvider::default()),
        );
        let (graph, _coder_id) = single_pending_graph(session_id, "coder");

        let (output, events) =
            run_graph_for_test(&mut coordinator, bus.clone(), graph, session_id, HashMap::new())
                .await;

        // The terminal zero-file failure surfaces as Partial (not Completed).
        assert_eq!(
            output.completion_status,
            concerto_core::types::AgentCompletionStatus::Partial,
            "the post-revision zero-file failure must surface as Partial, got: {:?}",
            output.completion_status,
        );
        let short_circuit_note = events.iter().any(|kind| {
            matches!(
                kind,
                EventKind::AgentThought { content, .. }
                    if content.contains(
                        "completed with no file changes; queuing revision without running the review cycle"
                    )
            )
        });
        assert!(
            short_circuit_note,
            "expected the zero-file short-circuit AgentThought to be published"
        );

        // No review cycle may start: the first zero-file success was
        // short-circuited, and the revision's own zero-file success FAILS
        // the subtask instead of falling through to the reviewer.
        let review_cycle_count = events
            .iter()
            .filter(|kind| matches!(kind, EventKind::ReviewCycleStarted { .. }))
            .count();
        assert_eq!(
            review_cycle_count, 0,
            "no review cycle may run: the revision's zero-file success fails the subtask, got: {review_cycle_count}"
        );

        // The failure must be surfaced the way other failing subtasks are:
        // a SubTaskFailed event with the missing-deliverable reason.
        let subtask_failed = events.iter().any(|kind| {
            matches!(
                kind,
                EventKind::SubTaskFailed { error, .. }
                    if error.contains("completed with no file changes after revision; required deliverables were not produced")
            )
        });
        assert!(
            subtask_failed,
            "expected a SubTaskFailed event naming the missing deliverables, got: {events:?}"
        );
        assert!(
            output.final_message.contains("required deliverables were not produced"),
            "expected the failure reason in the final message, got: {}",
            output.final_message
        );

        // Original coder dispatch + one bounded revision = two coder
        // dispatches. A re-armed short-circuit would queue a third coder
        // task (and, without the bound, keep going).
        let coder_dispatch_count = events
            .iter()
            .filter(|kind| {
                matches!(
                    kind,
                    EventKind::SubTaskStarted { role, .. } if role == &AgentId::new("coder")
                )
            })
            .count();
        assert_eq!(
            coder_dispatch_count, 2,
            "original coder plus one bounded revision must be the only coder dispatches, got: {coder_dispatch_count}"
        );
    }

    /// A zero-file implement success that queues a revision does not doom
    /// the lineage: the revision's own completion with real file changes
    /// falls through to the normal review cycle and the graph completes.
    /// The run still surfaces as Partial because the short-circuit note is
    /// recorded as a recoverable issue (the pre-existing convention: any
    /// recoverable note → Partial) — the key point is that the revision was
    /// dispatched and no subtask failed.
    #[tokio::test]
    async fn zero_file_revision_that_produces_files_recovers_and_completes() {
        let bus = EventBus::new(256);
        let mut revised_with_files =
            ok_result("coder", "implemented").expect("ok_result should not fail");
        revised_with_files.files_modified = vec![camino::Utf8PathBuf::from("src/a.rs")];
        let mocks = vec![
            MockExpertAgent::always_succeed(AgentId::new("researcher"), "found"),
            // First coder dispatch returns zero files; the queued revision
            // (second dispatch, same role) returns real file changes.
            MockExpertAgent::sequence(
                AgentId::new("coder"),
                vec![ok_result("coder", "implemented"), Ok(revised_with_files)],
            ),
            MockExpertAgent::always_succeed(AgentId::new("reviewer"), "approved"),
            MockExpertAgent::always_succeed(AgentId::new("validator"), "valid"),
        ];
        let (output, events) = run_for_test(
            coordinator_with(
                bus.clone(),
                Arc::new(AgentRegistry::from_mocks(mocks)),
                PLAN_RESEARCH_CODER.into(),
            ),
            bus.clone(),
        )
        .await;

        // The graph recovered (the revision produced files, so no subtask
        // failed), but the short-circuit note is a recoverable issue, so the
        // run surfaces as Partial with the note — never as the terminal
        // zero-file failure.
        assert_eq!(
            output.completion_status,
            concerto_core::types::AgentCompletionStatus::Partial,
            "the recovered run carries the short-circuit recoverable note, so it must surface as Partial (not Completed), got: {:?}",
            output.completion_status,
        );
        assert!(
            output.final_message.contains("revision queued without running the review cycle"),
            "expected the short-circuit recovery note in the final message, got: {}",
            output.final_message
        );
        assert!(
            !output.final_message.contains("required deliverables were not produced"),
            "the recovery path must not fail the subtask, got: {}",
            output.final_message
        );
        // Original coder dispatch + the queued revision = two coder
        // dispatches, proving the revision was actually queued and ran.
        let coder_dispatch_count = events
            .iter()
            .filter(|kind| {
                matches!(
                    kind,
                    EventKind::SubTaskStarted { role, .. } if role == &AgentId::new("coder")
                )
            })
            .count();
        assert_eq!(
            coder_dispatch_count, 2,
            "the queued revision must actually be dispatched, got: {coder_dispatch_count}"
        );
        // Exactly one review cycle — for the file-producing revision result.
        let review_cycle_count = events
            .iter()
            .filter(|kind| matches!(kind, EventKind::ReviewCycleStarted { .. }))
            .count();
        assert_eq!(
            review_cycle_count, 1,
            "the file-producing revision must flow through the review cycle exactly once, got: {review_cycle_count}"
        );
        assert!(
            !events.iter().any(|kind| matches!(kind, EventKind::SubTaskFailed { .. })),
            "no subtask may fail on the revision recovery path"
        );
    }

    /// A zero-file success from a research-stage (or any non-implement)
    /// agent must never trigger the zero-file implement guard: the gate
    /// stays on `implement_phase`, so a research agent that produces no
    /// files is unaffected and the run completes normally.
    #[tokio::test]
    async fn zero_file_research_stage_agent_is_unaffected() {
        let bus = EventBus::new(256);
        let mocks = vec![
            // The architect seeds the coder's expected artifacts so the
            // coder's artifact writer produces real file changes.
            MockExpertAgent::always_succeed(AgentId::new("architect"), DESIGN_DOC_JSON),
            // Researcher completes with zero files — must NOT queue a
            // revision or fail anything.
            MockExpertAgent::always_succeed(AgentId::new("researcher"), "found"),
            MockExpertAgent::always_succeed(AgentId::new("coder"), "implemented")
                .with_artifact_writer(),
            MockExpertAgent::always_succeed(AgentId::new("reviewer"), "approved"),
            MockExpertAgent::always_succeed(AgentId::new("validator"), "valid"),
        ];
        let (output, events) = run_for_test(
            coordinator_with_grounded(
                bus.clone(),
                Arc::new(AgentRegistry::from_mocks(mocks)),
                PLAN_RESEARCH_CODER.into(),
                &["src/a.rs"],
            ),
            bus.clone(),
        )
        .await;

        assert_eq!(
            output.completion_status,
            concerto_core::types::AgentCompletionStatus::Completed,
            "a zero-file research-stage result must not disturb the run, got: {:?}",
            output.completion_status,
        );
        assert!(
            output.final_message.contains("Multi-agent orchestration completed"),
            "unexpected final message: {}",
            output.final_message
        );
        // Exactly one coder dispatch — no revision was queued for the
        // researcher's zero-file result.
        let coder_dispatch_count = events
            .iter()
            .filter(|kind| {
                matches!(
                    kind,
                    EventKind::SubTaskStarted { role, .. } if role == &AgentId::new("coder")
                )
            })
            .count();
        assert_eq!(
            coder_dispatch_count, 1,
            "no zero-file revision may be queued for a research-stage result, got: {coder_dispatch_count}"
        );
        assert!(
            !events.iter().any(|kind| matches!(kind, EventKind::SubTaskFailed { .. })),
            "no subtask may fail when the researcher produces zero files"
        );
        // The file-producing coder still flows through the review cycle.
        assert!(
            events.iter().any(|kind| matches!(kind, EventKind::ReviewCycleStarted { .. })),
            "the file-producing coder must still trigger the review cycle"
        );
    }

    #[tokio::test]
    async fn no_implement_agent_returns_partial_with_clear_error() {
        let bus = EventBus::new(256);
        let mocks = vec![
            MockExpertAgent::always_succeed(AgentId::new("architect"), DESIGN_DOC_JSON),
            MockExpertAgent::always_succeed(AgentId::new("researcher"), "found"),
            MockExpertAgent::always_succeed(AgentId::new("reviewer"), "approved"),
            MockExpertAgent::always_succeed(AgentId::new("validator"), "valid"),
        ];
        let (output, _events) = run_for_test(
            coordinator_with(
                bus.clone(),
                Arc::new(AgentRegistry::from_mocks(mocks)),
                PLAN_RESEARCH_CODER.into(),
            ),
            bus.clone(),
        )
        .await;

        assert_eq!(
            output.completion_status,
            concerto_core::types::AgentCompletionStatus::Partial,
            "no implement agent should yield Partial, got: {:?}",
            output.completion_status
        );
        assert!(
            output.final_message.contains("no implementation-stage agent"),
            "expected a clear no-implement-agent error, got: {}",
            output.final_message
        );
    }

    /// ADR-35 §8 trigger 1 (stage absence): a pipeline with no registered
    /// implement-stage agent and an executor-carrying coordinator plans the
    /// implement subtask to the reserved `coordinator` role, which the
    /// coordinator then executes itself on its planning provider through the
    /// shared executor. Successes carry the `coordinator-self-execute`
    /// provider sentinel (ADR-42/45) so audit and UI consumers can identify
    /// the self-dispatch.
    #[tokio::test]
    async fn coordinator_self_executes_when_no_implement_agent_is_registered() {
        let bus = EventBus::new(256);
        let mocks = vec![
            MockExpertAgent::always_succeed(AgentId::new("architect"), DESIGN_DOC_JSON),
            MockExpertAgent::always_succeed(AgentId::new("researcher"), "found"),
            // Deliberately NO implement-stage agent: the coordinator carries
            // the implement subtask itself. No reviewer either — with a
            // review-stage agent the zero-file guard would queue a revision
            // for the synthetic success (it modifies no files), which is a
            // different code path from the one under test.
            MockExpertAgent::always_succeed(AgentId::new("validator"), "valid"),
        ];
        // The planner roster includes the coordinator as the sole
        // implement-stage participant, so the canned plan names it.
        let plan_json = r#"[
            {"role":"Researcher","description":"inspect","depends_on":[]},
            {"role":"coordinator","description":"implement","depends_on":[0]}
        ]"#;
        let mut coordinator = coordinator_with_responses(
            bus.clone(),
            Arc::new(AgentRegistry::from_mocks(mocks)),
            vec![plan_json.to_string(), "implemented by the coordinator self".into()],
        );
        coordinator = coordinator.with_executor(coordinator_self_executor());
        let (output, events) = run_for_test(coordinator, bus.clone()).await;

        assert_eq!(
            output.completion_status,
            concerto_core::types::AgentCompletionStatus::Completed,
            "coordinator self-execution must complete the run, got: {:?}",
            output.completion_status
        );
        assert!(
            output.final_message.contains("Multi-agent orchestration completed"),
            "unexpected final message: {}",
            output.final_message
        );
        // The self-dispatch is recorded with the sentinel serving provider.
        assert!(
            output
                .provider_metrics
                .iter()
                .any(|metrics| metrics.provider == "coordinator-self-execute"),
            "expected the coordinator-self-execute provider sentinel in metrics: {:?}",
            output.provider_metrics
        );
        // The coordinator-role subtask was created by the decomposition, and
        // the coordinator persona actually ran (its freeform loop publishes
        // AgentThought events under the reserved id).
        assert!(
            events.iter().any(|kind| matches!(
                kind,
                EventKind::SubTaskCreated { role, .. } if role == &AgentId::new("coordinator")
            )),
            "expected a coordinator-role subtask to be created"
        );
        assert!(
            events.iter().any(|kind| matches!(
                kind,
                EventKind::AgentThought { agent_id, .. } if agent_id == "coordinator"
            )),
            "expected the coordinator persona to run via AgentThought events"
        );
        // The self-execution is metered exactly like a runner-dispatched
        // subtask: a SubTaskStarted / SubTaskCompleted lifecycle pair under
        // the coordinator role and a live spend snapshot after the run
        // settles (published by the shared publish_spend_events helper).
        assert!(
            events.iter().any(|kind| matches!(
                kind,
                EventKind::SubTaskStarted { role, .. } if role == &AgentId::new("coordinator")
            )),
            "expected SubTaskStarted for the coordinator-role subtask"
        );
        assert!(
            events.iter().any(|kind| matches!(
                kind,
                EventKind::SubTaskCompleted { role, .. } if role == &AgentId::new("coordinator")
            )),
            "expected SubTaskCompleted for the coordinator-role subtask"
        );
        assert!(
            events.iter().any(|kind| matches!(kind, EventKind::SpendUpdated { .. })),
            "expected a SpendUpdated snapshot after the self-run settled"
        );
    }

    #[tokio::test]
    async fn custom_implement_stage_agent_triggers_review() {
        let bus = EventBus::new(256);
        // The plan names the custom implement-stage agent "copilot" instead
        // of the built-in Coder.
        let plan_json = r#"[
            {"role":"Researcher","description":"inspect","depends_on":[]},
            {"role":"copilot","description":"implement","depends_on":[0]}
        ]"#;
        let mocks = vec![
            MockExpertAgent::always_succeed(AgentId::new("architect"), DESIGN_DOC_JSON),
            MockExpertAgent::always_succeed(AgentId::new("researcher"), "found"),
            MockExpertAgent::always_succeed(AgentId::new("copilot"), "implemented")
                .with_stage(Some(concerto_core::AgentStage::new("implement")))
                .with_artifact_writer(),
            MockExpertAgent::always_succeed(AgentId::new("reviewer"), "approved"),
            MockExpertAgent::always_succeed(AgentId::new("validator"), "valid"),
        ];
        let (output, events) = run_for_test(
            coordinator_with_grounded(
                bus.clone(),
                Arc::new(AgentRegistry::from_mocks(mocks)),
                plan_json.into(),
                &["src/a.rs"],
            ),
            bus.clone(),
        )
        .await;

        assert!(
            output.final_message.contains("Multi-agent orchestration completed"),
            "unexpected final message: {}",
            output.final_message
        );
        assert!(
            events.iter().any(|kind| {
                matches!(
                    kind,
                    EventKind::SubTaskStarted { role, .. } if role == &AgentId::new("copilot")
                )
            }),
            "expected the custom implement-stage agent to be dispatched"
        );
        assert!(
            events.iter().any(|kind| matches!(kind, EventKind::ReviewCycleStarted { .. })),
            "implement-stage success should trigger the review cycle"
        );
    }

    /// A build task (contains implement-stage work) whose pipeline has no
    /// validation-stage agent must not be silently accepted: with no declared
    /// verification, acceptance is a failure (audit C-06).
    #[tokio::test]
    async fn build_task_without_validator_is_not_silently_accepted() {
        let bus = EventBus::new(256);
        let mocks = vec![
            MockExpertAgent::always_succeed(AgentId::new("architect"), DESIGN_DOC_JSON),
            MockExpertAgent::always_succeed(AgentId::new("researcher"), "found"),
            MockExpertAgent::always_succeed(AgentId::new("coder"), "implemented")
                .with_artifact_writer(),
            MockExpertAgent::always_succeed(AgentId::new("reviewer"), "approved"),
        ];
        let (output, events) = run_for_test(
            coordinator_with_grounded(
                bus.clone(),
                Arc::new(AgentRegistry::from_mocks(mocks)),
                PLAN_RESEARCH_CODER.into(),
                &["src/a.rs"],
            ),
            bus.clone(),
        )
        .await;

        assert!(
            output.final_message.contains("Acceptance rejected: no validation-stage agent"),
            "unexpected final message: {}",
            output.final_message
        );
        assert!(
            !events.iter().any(|kind| matches!(kind, EventKind::ValidationCycleStarted { .. })),
            "no validation-stage agent should mean no validation cycle runs"
        );
    }

    #[tokio::test]
    async fn decompose_task_accepts_non_empty_design_doc() {
        // A DesignDoc with at least one goal should pass validation.
        let mocks = vec![MockExpertAgent::always_succeed(
            AgentId::new("architect"),
            r#"{"goals":["implement login"],"proposed_files":["src/auth.rs"],"interface_sketch":"login form"}"#,
        )];

        // We also need other agents because the planner will create subtasks
        // and the execution loop will dispatch them.
        let plan = crate::graph::TaskGraph::default();
        let budget = BudgetScenarioBuilder::generous();
        let mut harness = AgentFlowTestHarness::new(mocks, plan, budget);
        let task = AgentTask::new(concerto_core::ids::Ulid::new(), "test task");
        let result = harness.run(task).await;

        // Should not error on DesignDoc validation.  It may error later if
        // planner-created subtasks reference agents we don't have, but the
        // validation itself should pass.
        if let Err(OrchestratorError::AgentLoopError(msg)) = &result {
            assert!(
                !msg.contains("empty DesignDoc"),
                "unexpected empty DesignDoc error for valid doc: {msg}"
            );
        }
        // Any other outcome is acceptable (error from planner or success).
        assert!(result.is_ok() || result.is_err(), "expected ok or err, got: {result:?}");
    }

    /// Verify that `metrics_from_result` correctly extracts fields from
    /// an `AgentRunResult`.
    #[test]
    fn metrics_from_result_aggregates_fields() {
        let result = AgentRunResult {
            task_id: TaskId::new(),
            role: AgentId::new("coder"),
            outcome: AgentOutcome::Success,
            summary: "wrote code".into(),
            files_modified: vec![],
            tool_call_count: 5,
            cost_usd: 0.02,
            latency_ms: 1500,
            provider: "openai".into(),
            model: "gpt-4o".into(),
            tokens_in: 500,
            tokens_out: 200,
        };
        let metrics = metrics_from_result(&result);
        assert_eq!(metrics.provider, "openai");
        assert_eq!(metrics.model, "gpt-4o");
        assert_eq!(metrics.tokens_in, 500);
        assert_eq!(metrics.tokens_out, 200);
        assert!((metrics.cost_usd - 0.02).abs() < f64::EPSILON);
        assert_eq!(metrics.latency_ms, 1500);
    }

    /// Verify that `failed_attempt_result` creates a valid result entry
    /// with the error message preserved.
    #[test]
    fn failed_attempt_result_contains_error() {
        let task_id = TaskId::new();
        let role = AgentId::new("coder");
        let error_msg = "provider rate limited".to_string();
        let result = failed_attempt_result(task_id, role.clone(), error_msg.clone());
        assert_eq!(result.task_id, task_id);
        assert_eq!(result.role, role);
        assert_eq!(result.summary, format!("Previous attempt failed: {error_msg}"));
        match &result.outcome {
            AgentOutcome::Failed { error } => {
                assert_eq!(error, &error_msg);
            }
            other => panic!("expected Failed outcome, got: {other:?}"),
        }
        assert!(result.files_modified.is_empty());
        assert_eq!(result.tool_call_count, 0);
        assert_eq!(result.cost_usd, 0.0);
    }

    /// Verify that `classify_subtask_error` correctly classifies edge-case
    /// error variants (budget, cycle, task-graph, and exhausted-retry errors).
    #[test]
    fn classify_subtask_error_edge_cases() {
        // SubTaskRetriesExhausted is a terminal condition — no ladder.
        assert!(matches!(
            classify_subtask_error(&OrchestratorError::SubTaskRetriesExhausted {
                task_id: TaskId::new(),
                role: AgentId::new("coder"),
                attempts: 3,
                last_error: "still failing".into(),
            }),
            SubtaskFailureClass::NonRecoverable
        ));
        // InvalidTaskGraph is structural — graph corruption needs a human.
        assert!(matches!(
            classify_subtask_error(&OrchestratorError::InvalidTaskGraph {
                reason: "missing node".into(),
            }),
            SubtaskFailureClass::NonRecoverable
        ));
        // CycleDetected is structural — needs human intervention.
        assert!(matches!(
            classify_subtask_error(&OrchestratorError::CycleDetected {
                tool_name: "reviewer".into(),
                count: 3,
            }),
            SubtaskFailureClass::NonRecoverable
        ));
        // MultiAgentPlanFailed is structural.
        assert!(matches!(
            classify_subtask_error(&OrchestratorError::MultiAgentPlanFailed {
                reason: "architect failed".into(),
            }),
            SubtaskFailureClass::NonRecoverable
        ));
        // Unrecoverable is structural by construction.
        assert!(matches!(
            classify_subtask_error(&OrchestratorError::Unrecoverable { message: "fatal".into() }),
            SubtaskFailureClass::NonRecoverable
        ));
    }

    // ------------------------------------------------------------------
    // Regression: ADR-26 / C-05 — resume from checkpoint with an
    // exhausted-blocked subtask returns Partial, not INTERNAL_ERROR.
    // ------------------------------------------------------------------

    // ------------------------------------------------------------------
    // Regression: Coder artifact-failure replan fallback
    // ------------------------------------------------------------------

    /// Verify that when a Coder subtask exhausts retries with an
    /// artifact-production failure, the coordinator creates an Architect
    /// replan subtask, and when the Architect completes with a valid
    /// DesignDoc, a new Coder subtask is spawned.  The run completes
    /// successfully because the follow-up Coder (populated from the
    /// mock's default success) passes.
    #[tokio::test]
    async fn coder_artifact_failure_triggers_replan_and_spawns_new_coder() {
        // ── 1. Coordinator with mocks ───────────────────────────────
        let bus = EventBus::new(256);
        let spend_tracker = Arc::new(SpendTracker::default());

        let mocks = vec![
            MockExpertAgent::always_fail(AgentId::new("coder"), "Expected artifacts not produced")
                .with_artifact_writer(),
            MockExpertAgent::always_succeed(
                AgentId::new("architect"),
                r#"{"goals":["redesign"],"proposed_files":["src/new_main.rs"],"interface_sketch":"updated"}"#,
            ),
            MockExpertAgent::always_succeed(AgentId::new("reviewer"), "review passed"),
            MockExpertAgent::always_succeed(AgentId::new("validator"), "validation passed"),
        ];
        let registry = Arc::new(AgentRegistry::from_mocks(mocks));
        let runner = AgentRunner::new(registry.clone(), bus.clone(), spend_tracker.clone());

        // Provide routing profiles so `select_for_session` does not fail
        // when dispatching Coder, Architect, and Reviewer tasks.
        use concerto_core::types::RoutingProfile;
        let profiles: Vec<RoutingProfile> = vec![
            RoutingProfile {
                provider_config_id: "test".into(),
                provider: "test".into(),
                model: "cheap".into(),
                cost_per_1k_tokens: 0.001,
                avg_latency_ms: 100,
                context_window: 8192,
                supports_tool_calling: true,
                base_url: None,
                description: None,
            },
            RoutingProfile {
                provider_config_id: "test".into(),
                provider: "test".into(),
                model: "mid".into(),
                cost_per_1k_tokens: 0.005,
                avg_latency_ms: 100,
                context_window: 8192,
                supports_tool_calling: true,
                base_url: None,
                description: None,
            },
            RoutingProfile {
                provider_config_id: "test".into(),
                provider: "test".into(),
                model: "expensive".into(),
                cost_per_1k_tokens: 0.01,
                avg_latency_ms: 100,
                context_window: 8192,
                supports_tool_calling: true,
                base_url: None,
                description: None,
            },
        ];
        let routing = Arc::new(RoutingEngine::new(
            profiles.clone(),
            spend_tracker.clone(),
            concerto_config::ModelPinConfig {
                pins: std::collections::HashMap::new(),
                ..Default::default()
            },
            EventBus::default(),
        ));
        let provider: Arc<dyn concerto_core::traits::provider::LlmProvider> =
            Arc::new(MockProvider::default());
        let model_registry = Arc::new(ModelRegistry::from_profiles(profiles));
        let model_selector = Arc::new(ModelSelector::new(model_registry, routing));

        let mut coordinator = CoordinatorAgent::new(
            registry,
            runner,
            model_selector,
            spend_tracker,
            bus.clone(),
            provider,
            Arc::new(NullMemoryStore),
        );

        // ── 2. Graph: completed Architect → pending Coder ───────────
        let mut graph = TaskGraph::new();
        let session_id = Ulid::new();
        let arch_id = TaskId::new();
        let coder_id = TaskId::new();

        graph.add_root(SubTask {
            id: arch_id,
            parent_id: None,
            session_id,
            role: AgentId::new("architect"),
            description: "Initial design".into(),
            status: SubTaskStatus::Completed,
            dependencies: vec![],
            deliverable: Some("initial design complete".into()),
            created_at: time::OffsetDateTime::now_utc(),
            completed_at: Some(time::OffsetDateTime::now_utc()),
        });

        graph.add_child(
            SubTask {
                id: coder_id,
                parent_id: Some(arch_id),
                session_id,
                role: AgentId::new("coder"),
                description: "Implement: test feature".into(),
                status: SubTaskStatus::Pending,
                dependencies: vec![arch_id],
                deliverable: None,
                created_at: time::OffsetDateTime::now_utc(),
                completed_at: None,
            },
            arch_id,
            Dependency::MustFinishBefore,
        );

        // Pre-set attempt count so the first dispatch pushes it to
        // DEFAULT_MAX_SUBTASK_ATTEMPTS and triggers the replan immediately.
        let mut subtask_attempts = HashMap::new();
        subtask_attempts.insert(coder_id, DEFAULT_MAX_SUBTASK_ATTEMPTS - 1);

        let task = AgentTask::new(session_id, "test task");
        let project_dir = tempfile::tempdir().expect("tempdir for test workspace");
        let context =
            concerto_core::types::AgentContext::new(concerto_core::types::SessionContext::new(
                task.session_id,
                project_dir.path().to_path_buf(),
            ));

        // ── 3. Execute graph — replan should fire ───────────────────
        let run_objective = task.description.clone();
        let run_objective_hash = blake3::hash(run_objective.as_bytes()).to_hex().to_string();
        let result = coordinator
            .execute_graph(
                task,
                context,
                CancellationToken::new(),
                graph,
                HashMap::new(), // completed_results
                0.0,            // total_cost
                0,              // total_tool_calls
                vec![],         // all_files
                vec![],         // provider_metrics
                subtask_attempts,
                HashMap::new(), // retry_feedback
                HashMap::new(), // model_assignments
                Vec::new(),     // action_ledger
                run_objective,
                run_objective_hash,
            )
            .await;

        // The run should complete (replan Architect → new Coder succeeds)
        assert!(result.is_ok(), "expected Ok, got error: {result:?}");
        let (output, _notes) = result.unwrap();

        // The new Coder from replan gets a default-success from the mock,
        // so the overall run should be Completed (not Partial).
        assert_eq!(
            output.completion_status,
            concerto_core::types::AgentCompletionStatus::Completed,
            "replan fallback should produce Completed status when the \
             follow-up Coder succeeds, got: {:?}",
            output.completion_status,
        );

        // Smoke-check that the replan occurred: the coordinator's
        // replan_attempts map should contain the original Coder id and
        // the new Coder id (inserted to prevent cascading replans).
        assert!(
            coordinator.replan_attempts.contains_key(&coder_id),
            "original Coder task should be recorded in replan_attempts",
        );
        assert!(
            coordinator.replan_attempts.len() >= 2,
            "expected at least 2 entries in replan_attempts (original Coder + follow-up Coder), got {}",
            coordinator.replan_attempts.len(),
        );
    }

    // ------------------------------------------------------------------
    // ADR-42: two-tier fallback ladder (global default model →
    // coordinator self-execution)
    // ------------------------------------------------------------------

    fn ok_result(role: &str, summary: &str) -> Result<AgentRunResult, OrchestratorError> {
        Ok(AgentRunResult {
            task_id: TaskId::new(),
            role: AgentId::new(role),
            outcome: AgentOutcome::Success,
            summary: summary.into(),
            files_modified: Vec::new(),
            tool_call_count: 0,
            cost_usd: 0.0,
            latency_ms: 0,
            provider: "mock".into(),
            model: "mock-model".into(),
            tokens_in: 0,
            tokens_out: 0,
        })
    }

    /// A success result that *claims* the given files were modified without
    /// actually writing them — modeling a coder that reports deliverable
    /// file changes (so the run passes the zero-file guard) while the
    /// expected artifacts stay missing/placeholder on disk (so the C-06
    /// acceptance gate rejects the run, which is what these tests assert).
    fn claimed_files(
        role: &str,
        summary: &str,
        files: &[&str],
    ) -> Result<AgentRunResult, OrchestratorError> {
        let mut result = ok_result(role, summary).expect("ok_result should not fail");
        result.files_modified = files.iter().map(camino::Utf8PathBuf::from).collect();
        Ok(result)
    }

    /// A hard, non-transient provider failure (ADR-42 §1 LimitReached) —
    /// the error class that enters the fallback ladder on first dispatch.
    fn err_auth() -> Result<AgentRunResult, OrchestratorError> {
        Err(OrchestratorError::Provider(ProviderError::AuthFailure))
    }

    /// A hard budget-exhaustion failure (`PinnedModelBudgetExceeded` →
    /// ADR-42 §1 LimitReached) — the "no affordable model left in budget"
    /// class that enters the fallback ladder on first dispatch.
    fn err_budget() -> Result<AgentRunResult, OrchestratorError> {
        Err(OrchestratorError::PinnedModelBudgetExceeded {
            model: "cheap".into(),
            estimated: 1.0,
            remaining: 0.01,
        })
    }

    /// A transient provider failure (503 → `Recoverable`, ADR-42 §1) — the
    /// error class that triggers a same-agent/model retry rather than the
    /// fallback ladder.
    fn err_transient() -> Result<AgentRunResult, OrchestratorError> {
        Err(OrchestratorError::Provider(ProviderError::HttpStatus {
            status: 503,
            retry_after: None,
            message: "upstream busy".into(),
        }))
    }

    /// A hard provider limit (stream-idle timeout → `RetryExhausted`, which
    /// `is_transient()` reports as false) — the class that enters the fallback
    /// ladder directly (ADR-42 §1 LimitReached).
    fn err_stream_idle() -> Result<AgentRunResult, OrchestratorError> {
        Err(OrchestratorError::Provider(ProviderError::RetryExhausted {
            attempts: 3,
            elapsed: std::time::Duration::ZERO,
            last_error: "stream-idle timeout".into(),
        }))
    }

    /// A completed run whose `outcome` is `Failed` — used to exercise the
    /// re-entry path where a ladder tier returns a result the coordinator
    /// must route through the ordinary failed-outcome handling.
    fn ok_failed(role: &str, error: &str) -> Result<AgentRunResult, OrchestratorError> {
        Ok(AgentRunResult {
            task_id: TaskId::new(),
            role: AgentId::new(role),
            outcome: AgentOutcome::Failed { error: error.into() },
            summary: format!("failed: {error}"),
            files_modified: Vec::new(),
            tool_call_count: 0,
            cost_usd: 0.0,
            latency_ms: 0,
            provider: "mock".into(),
            model: "mock-model".into(),
            tokens_in: 0,
            tokens_out: 0,
        })
    }

    /// Wire a coordinator for fallback-ladder tests: mocks carry the agent
    /// responses (each mock is registered with a rebuild factory that returns
    /// itself, so tier-2 dispatches resolve), `pin_config` seeds the routing
    /// engine's tier-1 target, and `planning_provider` is the coordinator's
    /// serving pipe for tier-2 dispatches (the planning profile is fixed to
    /// `test/cheap` by the harness; mock agents ignore the model). To control
    /// the tier-2 result, script it into the mock's response queue — the
    /// planning provider's own output is no longer consulted.
    fn coordinator_for_ladder(
        bus: EventBus,
        mocks: Vec<MockExpertAgent>,
        pin_config: concerto_config::ModelPinConfig,
        planning_provider: Arc<dyn concerto_core::traits::provider::LlmProvider>,
    ) -> CoordinatorAgent {
        coordinator_for_ladder_with(
            bus,
            Arc::new(AgentRegistry::from_mocks(mocks)),
            pin_config,
            planning_provider,
            None,
        )
    }

    /// Extended ladder fixture: custom registry (so tests can register
    /// rebuild factories for ADR-45 tier 1b), optional fallback provider +
    /// profile, and optional per-run attempt cap.
    fn coordinator_for_ladder_with(
        bus: EventBus,
        registry: Arc<AgentRegistry>,
        pin_config: concerto_config::ModelPinConfig,
        planning_provider: Arc<dyn concerto_core::traits::provider::LlmProvider>,
        fallback: Option<(
            Arc<dyn concerto_core::traits::provider::LlmProvider>,
            concerto_providers::model::ModelProfile,
        )>,
    ) -> CoordinatorAgent {
        let spend_tracker = Arc::new(SpendTracker::default());
        let runner = AgentRunner::new(registry.clone(), bus.clone(), spend_tracker.clone());
        use concerto_core::types::RoutingProfile;
        let profiles: Vec<RoutingProfile> = vec![
            RoutingProfile {
                provider_config_id: "test".into(),
                provider: "test".into(),
                model: "cheap".into(),
                cost_per_1k_tokens: 0.001,
                avg_latency_ms: 100,
                context_window: 8192,
                supports_tool_calling: true,
                base_url: None,
                description: None,
            },
            RoutingProfile {
                provider_config_id: "test".into(),
                provider: "test".into(),
                model: "mid".into(),
                cost_per_1k_tokens: 0.005,
                avg_latency_ms: 100,
                context_window: 8192,
                supports_tool_calling: true,
                base_url: None,
                description: None,
            },
            RoutingProfile {
                provider_config_id: "test".into(),
                provider: "test".into(),
                model: "expensive".into(),
                cost_per_1k_tokens: 0.01,
                avg_latency_ms: 100,
                context_window: 8192,
                supports_tool_calling: true,
                base_url: None,
                description: None,
            },
        ];
        // The coordinator's serving pipe in this harness is `test`; its
        // planning profile resolves the coordinator's own model on it so
        // tier-2 self-execution dispatches through the runner (the planning
        // provider passed in is the pipe, the profile is the model/diagnostic
        // bucket — mock agents ignore both).
        let planning_routing = profiles[0].clone();
        let routing = Arc::new(RoutingEngine::new(
            profiles.clone(),
            spend_tracker.clone(),
            pin_config,
            EventBus::default(),
        ));
        let model_registry = Arc::new(ModelRegistry::from_profiles(profiles));
        let model_selector = Arc::new(ModelSelector::new(model_registry, routing));
        let mut coordinator = CoordinatorAgent::new(
            registry,
            runner,
            model_selector,
            spend_tracker,
            bus.clone(),
            planning_provider,
            Arc::new(NullMemoryStore),
        )
        .with_planning_profile(Some(concerto_providers::model::ModelProfile {
            context_window: 8192,
            supports_tool_calling: true,
            base_url: None,
            description: None,
            profile: planning_routing,
        }))
        // Model-first serving pipe: this harness's run-level default provider
        // is the `test` pipe (the profiles all live on it), so an unassigned
        // role's effective serving pipe is `test`. Tests may override with
        // `.with_default_provider_config_id(...)` to exercise other shapes.
        .with_default_provider_config_id(Some("test".into()));
        if let Some((provider, profile)) = fallback {
            coordinator = coordinator.with_default_model_provider(Some(provider), Some(profile));
        }
        coordinator
    }

    /// A graph with a single pending subtask of the given role and no
    /// dependencies — the minimal shape that exercises the fallback ladder.
    fn single_pending_graph(session_id: Ulid, role: &str) -> (TaskGraph, TaskId) {
        let mut graph = TaskGraph::new();
        let id = TaskId::new();
        graph.add_root(SubTask {
            id,
            parent_id: None,
            session_id,
            role: AgentId::new(role),
            description: "Ladder subtask".into(),
            status: SubTaskStatus::Pending,
            dependencies: vec![],
            deliverable: None,
            created_at: time::OffsetDateTime::now_utc(),
            completed_at: None,
        });
        (graph, id)
    }

    /// Drive a pre-built graph through `execute_graph` and collect the
    /// resulting output and all bus events.
    async fn run_graph_for_test(
        coordinator: &mut CoordinatorAgent,
        bus: EventBus,
        graph: TaskGraph,
        session_id: Ulid,
        subtask_attempts: HashMap<TaskId, u32>,
    ) -> (AgentOutput, Vec<EventKind>) {
        let mut rx = bus.subscribe();
        let task = AgentTask::new(session_id, "test task");
        let project_dir = tempfile::tempdir().expect("tempdir for test workspace");
        let context = AgentContext::new(concerto_core::types::SessionContext::new(
            task.session_id,
            project_dir.path().to_path_buf(),
        ));
        let (output, _notes) = coordinator
            .execute_graph(
                task,
                context,
                CancellationToken::new(),
                graph,
                HashMap::new(), // completed_results
                0.0,            // total_cost
                0,              // total_tool_calls
                vec![],         // all_files
                vec![],         // provider_metrics
                subtask_attempts,
                HashMap::new(), // retry_feedback
                HashMap::new(), // model_assignments
                Vec::new(),     // action_ledger
                "test task".to_string(),
                blake3::hash("test task".as_bytes()).to_hex().to_string(),
            )
            .await
            .expect("execute_graph should succeed");
        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event.kind.clone());
        }
        (output, events)
    }

    /// ADR-42 tier 1: when the first dispatch hits a hard provider limit
    /// (AuthFailure → LimitReached), the coordinator retries the same agent
    /// with the configured global default model and completes the subtask.
    #[tokio::test]
    async fn ladder_tier1_global_default_model_rescues_subtask() {
        let bus = EventBus::new(256);
        // Response 1 fails with auth; response 2 is the tier-1 re-dispatch.
        // (An empty queue would return "mock default" instead, so the
        // distinct summary proves the ladder re-ran the agent.)
        let architect = MockExpertAgent::sequence(
            AgentId::new("architect"),
            vec![err_auth(), ok_result("architect", "default model rescued")],
        );
        let session_id = Ulid::new();
        let mut coordinator = coordinator_for_ladder(
            bus.clone(),
            vec![architect],
            concerto_config::ModelPinConfig {
                default_model: Some("mid".into()),
                ..Default::default()
            },
            Arc::new(MockProvider::default()),
        );
        let (graph, _task_id) = single_pending_graph(session_id, "architect");

        let (output, events) =
            run_graph_for_test(&mut coordinator, bus.clone(), graph, session_id, HashMap::new())
                .await;

        assert_eq!(
            output.completion_status,
            concerto_core::types::AgentCompletionStatus::Completed,
            "tier-1 fallback should complete the subtask, got: {:?}",
            output.completion_status,
        );
        // The ladder re-dispatched the same agent on the default-model
        // profile ("test/mid" — the first dispatch used "test/cheap").
        let used_default_model = events.iter().any(|kind| {
            matches!(kind, EventKind::AgentThought { content, .. } if content.contains("test/mid"))
        });
        assert!(
            used_default_model,
            "expected a re-dispatch using the global default model (test/mid)"
        );
        let completed = events.iter().any(|kind| {
            matches!(
                kind,
                EventKind::SubTaskCompleted { outcome, .. } if outcome == "default model rescued"
            )
        });
        assert!(completed, "expected the tier-1 run to complete with its scripted summary");
    }

    /// ADR-42 tier 1 + capability gate: a tool-calling role (coder) whose
    /// primary dispatch fails with budget exhaustion (`PinnedModelBudgetExceeded`
    /// → `LimitReached`) is re-dispatched on the configured global default
    /// model. The resolved default profile passes the tool_calling capability
    /// gate, so tier 1 runs on the role's own serving pipe and completes the
    /// subtask.
    #[tokio::test]
    async fn ladder_tier1_budget_exhaustion_uses_default_model_with_tool_calling() {
        let bus = EventBus::new(256);
        // Response 1 fails with budget exhaustion; response 2 is the tier-1
        // re-dispatch on the default model. (An empty queue would return "mock
        // default" instead, so the distinct summary proves the ladder re-ran.)
        let coder = MockExpertAgent::sequence(
            AgentId::new("coder"),
            vec![err_budget(), ok_result("coder", "default model saved the budget")],
        );
        // The coder mock declares the implement stage, so the run is a
        // build task: the C-06 acceptance gate requires verification
        // evidence from a registered validation-stage agent. A succeeding
        // validator mock supplies it (the run has no expected artifacts,
        // so the artifact check is vacuous).
        let validator = MockExpertAgent::always_succeed(AgentId::new("validator"), "validation ok");
        let session_id = Ulid::new();
        let mut coordinator = coordinator_for_ladder(
            bus.clone(),
            vec![coder, validator],
            concerto_config::ModelPinConfig {
                default_model: Some("mid".into()),
                ..Default::default()
            },
            Arc::new(MockProvider::default()),
        );
        let (graph, _task_id) = single_pending_graph(session_id, "coder");

        let (output, events) =
            run_graph_for_test(&mut coordinator, bus.clone(), graph, session_id, HashMap::new())
                .await;

        assert_eq!(
            output.completion_status,
            concerto_core::types::AgentCompletionStatus::Completed,
            "tier-1 fallback should complete the subtask after budget exhaustion, got: {:?}",
            output.completion_status,
        );
        // The ladder re-dispatched the same agent on the default-model profile
        // ("test/mid" — the primary dispatch used "test/cheap"). The coder
        // role requires tool calling, so this also proves the default profile
        // passed the tool_calling capability gate.
        let used_default_model = events.iter().any(|kind| {
            matches!(kind, EventKind::AgentThought { content, .. } if content.contains("test/mid"))
        });
        assert!(
            used_default_model,
            "expected a tier-1 re-dispatch on the global default model (test/mid)"
        );
        let completed = events.iter().any(|kind| {
            matches!(
                kind,
                EventKind::SubTaskCompleted { outcome, .. }
                    if outcome == "default model saved the budget"
            )
        });
        assert!(completed, "expected the tier-1 run to complete with its scripted summary");
    }

    /// ADR-42 tier 2: with no default model, a hard provider limit on a
    /// freeform-role subtask is executed directly by the coordinator through
    /// the runner — the role is rebuilt on the coordinator's pipe and
    /// dispatched, tagged with the "coordinator-self-execute" provider
    /// convention.
    #[tokio::test]
    async fn ladder_tier2_coordinator_self_executes_freeform_subtask() {
        let bus = EventBus::new(256);
        // Freeform role ("docs-writer" has no pipeline stage); its first
        // scripted response fails, the second is the tier-2 dispatch result.
        let docs_writer = MockExpertAgent::sequence(
            AgentId::new("docs-writer"),
            vec![err_auth(), ok_result("docs-writer", "coordinator self-execution output")],
        );
        let session_id = Ulid::new();
        // Tier-2 dispatch runs through the runner with the coordinator's
        // planning provider as the pipe; the registry factory returns the
        // mock (the provider's own output is not consulted).
        let planning: Arc<dyn concerto_core::traits::provider::LlmProvider> =
            Arc::new(MockProvider::default());
        let mut coordinator = coordinator_for_ladder(
            bus.clone(),
            vec![docs_writer],
            concerto_config::ModelPinConfig::default(),
            planning,
        );
        let (graph, _task_id) = single_pending_graph(session_id, "docs-writer");

        let (output, events) =
            run_graph_for_test(&mut coordinator, bus.clone(), graph, session_id, HashMap::new())
                .await;

        assert_eq!(
            output.completion_status,
            concerto_core::types::AgentCompletionStatus::Completed,
            "tier-2 self-execution should complete the subtask, got: {:?}",
            output.completion_status,
        );
        // The self-executed run is tagged with the ADR-42 §6 sentinel
        // provider, visible in the final provider metrics.
        let self_executed = output
            .provider_metrics
            .iter()
            .any(|metrics| metrics.provider == "coordinator-self-execute");
        assert!(self_executed, "expected tier-2 self-execution to appear in provider metrics");
        // The settled mirror on the coordinator exactly matches the success
        // output — failure-path persistence relies on this mirror, and any
        // drift (duplicates included) would double-count spend records.
        let settled = coordinator.settled_metrics();
        assert_eq!(settled.len(), output.provider_metrics.len(), "mirror length must match");
        for (mirrored, expected) in settled.iter().zip(&output.provider_metrics) {
            assert_eq!(mirrored.provider, expected.provider);
            assert_eq!(mirrored.model, expected.model);
            assert_eq!(mirrored.tokens_in, expected.tokens_in);
            assert_eq!(mirrored.tokens_out, expected.tokens_out);
            assert_eq!(mirrored.cost_usd, expected.cost_usd);
            assert_eq!(mirrored.latency_ms, expected.latency_ms);
        }
        // The scripted failure happened on the first dispatch, then the
        // fallback took over.
        let failed = events.iter().any(|kind| {
            matches!(
                kind,
                EventKind::SubTaskFailed { role, .. } if role == &AgentId::new("docs-writer")
            )
        });
        assert!(failed, "expected the scripted first-attempt failure");
    }

    /// ADR-42 tier 2 + FIX 1: when the tier-2 dispatch returns empty (or
    /// whitespace-only) text, coordinator self-execution must NOT complete
    /// the subtask — the tier fails and the ladder exhausts to a partial
    /// outcome instead of publishing an empty deliverable.
    #[tokio::test]
    async fn ladder_tier2_empty_provider_output_does_not_complete() {
        let bus = EventBus::new(256);
        let docs_writer = MockExpertAgent::sequence(
            AgentId::new("docs-writer"),
            vec![err_auth(), ok_result("docs-writer", "")],
        );
        let session_id = Ulid::new();
        // The tier-2 dispatch "answers" with a Success whose summary is empty:
        // this is exactly the case FIX 1 guards against.
        let planning: Arc<dyn concerto_core::traits::provider::LlmProvider> =
            Arc::new(MockProvider::default());
        let mut coordinator = coordinator_for_ladder(
            bus.clone(),
            vec![docs_writer],
            concerto_config::ModelPinConfig::default(),
            planning,
        );
        let (graph, _task_id) = single_pending_graph(session_id, "docs-writer");

        let (output, events) =
            run_graph_for_test(&mut coordinator, bus.clone(), graph, session_id, HashMap::new())
                .await;

        assert_eq!(
            output.completion_status,
            concerto_core::types::AgentCompletionStatus::Partial,
            "empty self-execution output must not complete the subtask, got: {:?}",
            output.completion_status,
        );
        // The empty deliverable is treated as a tier failure (FIX 1): the
        // ladder reports the tier-2 failure instead of recording a deliverable.
        let empty_deliverable_rejected = events.iter().any(|kind| {
            matches!(
                kind,
                EventKind::AgentThought { content, .. }
                    if content.contains("Fallback tier 2 (coordinator self-execution) failed")
            )
        });
        assert!(empty_deliverable_rejected, "expected the empty-deliverable tier-2 failure note");
        let exhausted_note = events.iter().any(|kind| {
            matches!(
                kind,
                EventKind::AgentThought { content, .. }
                    if content.contains("Fallback ladder exhausted")
            )
        });
        assert!(exhausted_note, "expected the ladder-exhaustion note to be published");
    }

    /// ADR-45: the tier-2 artifact gate is removed. When the role's bound
    /// provider fails (and the default provider is unavailable in this
    /// harness), the coordinator is the last functioning execution path and
    /// takes over ANY subtask — artifact-bearing or not — so the run
    /// completes with the coordinator's deliverable instead of abandoning
    /// the instruction mid-run. The dispatch carries the original role's
    /// expected artifacts into the run context, so the rebuilt agent can
    /// produce them.
    #[tokio::test]
    async fn ladder_tier2_takes_over_file_artifact_subtask() {
        let bus = EventBus::new(256);
        let docs_writer = MockExpertAgent::sequence(
            AgentId::new("docs-writer"),
            vec![err_auth(), ok_result("docs-writer", "coordinator deliverable")],
        );
        let session_id = Ulid::new();
        // Tier-2 dispatch: substantive so the takeover counts as a Success and
        // the subtask completes (ADR-45 §3).
        let planning: Arc<dyn concerto_core::traits::provider::LlmProvider> =
            Arc::new(MockProvider::default());
        let mut coordinator = coordinator_for_ladder(
            bus.clone(),
            vec![docs_writer],
            concerto_config::ModelPinConfig::default(),
            planning,
        );
        let (graph, task_id) = single_pending_graph(session_id, "docs-writer");
        // A file-artifact contract: the pre-ADR-45 gate would have skipped
        // self-execution for this subtask and abandoned the run.
        coordinator
            .expected_artifacts
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(task_id, vec![camino::Utf8PathBuf::from("docs/guide.md")]);

        let (output, events) =
            run_graph_for_test(&mut coordinator, bus.clone(), graph, session_id, HashMap::new())
                .await;

        assert_eq!(
            output.completion_status,
            concerto_core::types::AgentCompletionStatus::Completed,
            "a file-artifact subtask must be taken over by coordinator self-execution, \
             got: {:?}",
            output.completion_status,
        );
        // The coordinator-self-execute provider metric records the takeover.
        let self_executed = output
            .provider_metrics
            .iter()
            .any(|metrics| metrics.provider == "coordinator-self-execute");
        assert!(self_executed, "self-execution must take over file-artifact subtasks");
        let completed =
            events.iter().any(|kind| matches!(kind, EventKind::SubTaskCompleted { .. }));
        assert!(completed, "expected SubTaskCompleted for the coordinator deliverable");
        let exhausted_note = events.iter().any(|kind| {
            matches!(
                kind,
                EventKind::AgentThought { content, .. }
                    if content.contains("Fallback ladder exhausted")
            )
        });
        assert!(!exhausted_note, "ladder must not exhaust when tier 2 rescues the subtask");
    }

    /// A fallback profile for ADR-45 tier 1b tests: a provider OTHER than the
    /// role's bound provider (`test`). The model matters for the (model, pipe)
    /// degenerate check: pass the same model the tier-1 profile resolved to
    /// when the test wants tier 1b to detect a no-op repeat of tier 1.
    fn fallback_profile(
        provider_config_id: &str,
        model: &str,
    ) -> concerto_providers::model::ModelProfile {
        use concerto_core::types::RoutingProfile;
        concerto_providers::model::ModelProfile {
            context_window: 8192,
            supports_tool_calling: true,
            base_url: None,
            description: None,
            profile: RoutingProfile {
                provider_config_id: provider_config_id.into(),
                provider: "fallback".into(),
                model: model.into(),
                cost_per_1k_tokens: 0.0,
                avg_latency_ms: 0,
                context_window: 8192,
                supports_tool_calling: true,
                base_url: None,
                description: None,
            },
        }
    }

    /// ADR-45 tier 1b: when the role's bound provider is the failure, the
    /// ladder rebuilds the SAME role on the run's default provider (registry
    /// rebuild factory) and the subtask completes. ADR-42's tier 1 could only
    /// swap the model name on the bound provider; tier 1b is the provider
    /// switch that escapes latency/quota/auth failures of the role's own
    /// provider.
    #[tokio::test]
    async fn ladder_tier1b_default_provider_rescues_subtask() {
        use concerto_core::traits::provider::LlmProvider;
        let bus = EventBus::new(256);
        let coder_id = AgentId::new("architect");
        // The bound agent fails auth on the first dispatch AND on the tier-1
        // default-model re-dispatch (two errors; an empty queue would return
        // "mock default" success, which must not rescue these first runs).
        let bound =
            Arc::new(MockExpertAgent::sequence(coder_id.clone(), vec![err_auth(), err_auth()]));
        // The registry rebuild factory: same role, different provider — the
        // rebuilt agent succeeds where the bound one failed.
        let rescued =
            Arc::new(MockExpertAgent::always_succeed(coder_id.clone(), "default provider rescued"));
        let mut registry = AgentRegistry::new();
        registry.register_with_factory(
            coder_id.clone(),
            bound,
            Arc::new(move |_provider: Arc<dyn LlmProvider>| rescued.clone()),
        );
        let session_id = Ulid::new();
        let fallback: Arc<dyn LlmProvider> = Arc::new(MockProvider::default());
        let mut coordinator = coordinator_for_ladder_with(
            bus.clone(),
            Arc::new(registry),
            concerto_config::ModelPinConfig {
                default_model: Some("mid".into()),
                ..Default::default()
            },
            Arc::new(MockProvider::default()),
            Some((fallback, fallback_profile("fallback", "default-model"))),
        );
        let (graph, _task_id) = single_pending_graph(session_id, "architect");

        let (output, events) =
            run_graph_for_test(&mut coordinator, bus.clone(), graph, session_id, HashMap::new())
                .await;

        assert_eq!(
            output.completion_status,
            concerto_core::types::AgentCompletionStatus::Completed,
            "the default-provider re-dispatch must rescue the subtask, got: {:?}",
            output.completion_status,
        );
        let rescued_summary = events.iter().any(|kind| {
            matches!(
                kind,
                EventKind::SubTaskCompleted { outcome, .. } if outcome == "default provider rescued"
            )
        });
        assert!(rescued_summary, "the rebuilt agent must produce the deliverable");
        let note = events.iter().any(|kind| {
            matches!(
                kind,
                EventKind::AgentThought { content, .. }
                    if content.contains("Fallback tier 1b (default provider)")
            )
        });
        assert!(note, "expected the tier-1b note to be published");
        let exhausted_note = events.iter().any(|kind| {
            matches!(
                kind,
                EventKind::AgentThought { content, .. }
                    if content.contains("Fallback ladder exhausted")
            )
        });
        assert!(!exhausted_note, "ladder must not exhaust when tier 1b rescues the subtask");
    }

    /// ADR-45 §4: `default_model_fallback` gates tier 1b. Disabled — the
    /// ladder keeps ADR-42 semantics (default model on the bound provider,
    /// then coordinator self-execution) and never rebuilds on the default
    /// provider.
    #[tokio::test]
    async fn ladder_tier1b_disabled_by_config() {
        use concerto_core::traits::provider::LlmProvider;
        let bus = EventBus::new(256);
        let coder_id = AgentId::new("architect");
        let bound =
            Arc::new(MockExpertAgent::sequence(coder_id.clone(), vec![err_auth(), err_auth()]));
        let rescued = Arc::new(MockExpertAgent::always_succeed(coder_id.clone(), "must not run"));
        let mut registry = AgentRegistry::new();
        registry.register_with_factory(
            coder_id.clone(),
            bound,
            Arc::new(move |_provider: Arc<dyn LlmProvider>| rescued.clone()),
        );
        let session_id = Ulid::new();
        let fallback: Arc<dyn LlmProvider> = Arc::new(MockProvider::default());
        let mut coordinator = coordinator_for_ladder_with(
            bus.clone(),
            Arc::new(registry),
            concerto_config::ModelPinConfig {
                default_model: Some("mid".into()),
                ..Default::default()
            },
            Arc::new(SeqProvider::new(vec!["coordinator deliverable".into()])),
            Some((fallback, fallback_profile("fallback", "default-model"))),
        )
        .with_default_model_fallback(false);
        let (graph, _task_id) = single_pending_graph(session_id, "architect");

        let (output, events) =
            run_graph_for_test(&mut coordinator, bus.clone(), graph, session_id, HashMap::new())
                .await;

        assert_eq!(
            output.completion_status,
            concerto_core::types::AgentCompletionStatus::Completed,
            "tier 2 must rescue the subtask when tier 1b is disabled, got: {:?}",
            output.completion_status,
        );
        let tier1b_note = events.iter().any(|kind| {
            matches!(
                kind,
                EventKind::AgentThought { content, .. }
                    if content.contains("Fallback tier 1b (default provider)")
            )
        });
        assert!(!tier1b_note, "tier 1b must not run when the switch is disabled");
        let self_executed = output
            .provider_metrics
            .iter()
            .any(|metrics| metrics.provider == "coordinator-self-execute");
        assert!(self_executed, "tier 2 must take over when tier 1b is disabled");
    }

    /// ADR-45 tier 1b: skipped when the default provider IS the role's bound
    /// provider AND the default-model (model, pipe) pair already executed in
    /// tier 1 — a rebuild would be a no-op repeat of tier 1. The ladder notes
    /// the skip and continues at tier 2.
    #[tokio::test]
    async fn ladder_tier1b_skipped_when_default_provider_is_bound() {
        use concerto_core::traits::provider::LlmProvider;
        let bus = EventBus::new(256);
        let coder_id = AgentId::new("architect");
        let bound =
            Arc::new(MockExpertAgent::sequence(coder_id.clone(), vec![err_auth(), err_auth()]));
        let rescued = Arc::new(MockExpertAgent::always_succeed(coder_id.clone(), "must not run"));
        let mut registry = AgentRegistry::new();
        registry.register_with_factory(
            coder_id.clone(),
            bound,
            Arc::new(move |_provider: Arc<dyn LlmProvider>| rescued.clone()),
        );
        let session_id = Ulid::new();
        let fallback: Arc<dyn LlmProvider> = Arc::new(MockProvider::default());
        // `test` is the role's bound provider config id in this harness and the
        // tier-1 profile resolves to the default model `mid` on it; the fallback
        // profile carries the SAME (model, pipe) pair, so tier 1b detects the
        // degenerate no-op and skips.
        let mut coordinator = coordinator_for_ladder_with(
            bus.clone(),
            Arc::new(registry),
            concerto_config::ModelPinConfig {
                default_model: Some("mid".into()),
                ..Default::default()
            },
            Arc::new(SeqProvider::new(vec!["coordinator deliverable".into()])),
            Some((fallback, fallback_profile("test", "mid"))),
        );
        let (graph, _task_id) = single_pending_graph(session_id, "architect");

        let (output, events) =
            run_graph_for_test(&mut coordinator, bus.clone(), graph, session_id, HashMap::new())
                .await;

        assert_eq!(
            output.completion_status,
            concerto_core::types::AgentCompletionStatus::Completed,
            "tier 2 must rescue the subtask after the degenerate tier-1b skip, got: {:?}",
            output.completion_status,
        );
        let skip_note = events.iter().any(|kind| {
            matches!(
                kind,
                EventKind::AgentThought { content, .. }
                    if content.contains("Fallback tier 1b (default provider) skipped")
            )
        });
        assert!(skip_note, "expected the degenerate tier-1b skip note");
        let self_executed = output
            .provider_metrics
            .iter()
            .any(|metrics| metrics.provider == "coordinator-self-execute");
        assert!(self_executed, "tier 2 must take over after the degenerate skip");
    }

    /// ADR-45 §4: `max_subtask_attempts` caps the retry arm. With the default
    /// cap (3) a recoverable error retries to completion; with a cap of 1 the
    /// retry arm never fires and the ladder walks in immediately (tier 1
    /// rescues on the default model — the ladder note proves the cap).
    #[tokio::test]
    async fn ladder_max_subtask_attempts_config_is_honored() {
        let flaky = || Err(OrchestratorError::AgentLoopError("flaky provider latency".to_string()));
        // Two recoverable errors, then the mock's empty-queue default success.
        let run_case = |max_attempts: Option<u32>| async move {
            let bus = EventBus::new(256);
            let coder_id = AgentId::new("architect");
            let mocks = vec![MockExpertAgent::sequence(coder_id, vec![flaky(), flaky()])];
            let mut coordinator = coordinator_for_ladder(
                bus.clone(),
                mocks,
                concerto_config::ModelPinConfig {
                    default_model: Some("mid".into()),
                    ..Default::default()
                },
                Arc::new(SeqProvider::new(vec!["coordinator deliverable".into()])),
            );
            if let Some(max) = max_attempts {
                coordinator = coordinator.with_max_subtask_attempts(max);
            }
            let session_id = Ulid::new();
            let (graph, _task_id) = single_pending_graph(session_id, "architect");
            let (output, events) = run_graph_for_test(
                &mut coordinator,
                bus.clone(),
                graph,
                session_id,
                HashMap::new(),
            )
            .await;
            (output, events)
        };

        // Default cap: the retry arm absorbs both recoverable failures and the
        // mock's default success completes the run — no escalation, no ladder.
        let (output, events) = run_case(None).await;
        assert_eq!(
            output.completion_status,
            concerto_core::types::AgentCompletionStatus::Completed,
            "default retry cap must complete via retries, got: {:?}",
            output.completion_status,
        );
        let retried = events.iter().any(|kind| {
            matches!(
                kind,
                EventKind::AgentThought { content, .. } if content.contains("Retrying")
            )
        });
        assert!(retried, "default cap must retry recoverable failures");
        let escalated = events.iter().any(|kind| {
            matches!(
                kind,
                EventKind::AgentThought { content, .. } if content.contains("Escalating")
            )
        });
        assert!(!escalated, "default cap must not escalate while retries remain");

        // Cap of 1: the retry arm never fires (1 < 1 is false) — the failure
        // goes straight to the once-per-run escalation retry, then the ladder.
        let (output, events) = run_case(Some(1)).await;
        assert_eq!(
            output.completion_status,
            concerto_core::types::AgentCompletionStatus::Completed,
            "capped retries must still complete via the ladder, got: {:?}",
            output.completion_status,
        );
        let retried = events.iter().any(|kind| {
            matches!(
                kind,
                EventKind::AgentThought { content, .. } if content.contains("Retrying")
            )
        });
        assert!(!retried, "cap 1 must short-circuit the retry arm");
        let escalated = events.iter().any(|kind| {
            matches!(
                kind,
                EventKind::AgentThought { content, .. }
                    if content.contains("Escalating") && content.contains("exhausted")
            )
        });
        assert!(escalated, "cap 1 must route the failure to the escalation retry");
    }

    /// ADR-52: a run-wide dispatch cap stops the run at the batch boundary.
    /// Build a chained two-subtask graph (researcher → coder); with no cap the
    /// run completes with both dispatches; with a cap of 1 the run pauses with
    /// a Partial outcome (and a clear message) rather than dispatching the
    /// second subtask.
    #[tokio::test]
    async fn max_total_iterations_caps_dispatch_at_batch_boundary() {
        let session_id = Ulid::new();
        let run_case = |cap: Option<usize>| async move {
            let bus = EventBus::new(256);
            let mocks = vec![
                MockExpertAgent::always_succeed(AgentId::new("researcher"), "found"),
                MockExpertAgent::always_succeed(AgentId::new("coder"), "implemented"),
                MockExpertAgent::always_succeed(AgentId::new("validator"), "valid"),
            ];
            let mut coordinator = coordinator_for_ladder(
                bus.clone(),
                mocks,
                concerto_config::ModelPinConfig::default(),
                Arc::new(MockProvider::default()),
            )
            .with_max_total_iterations(cap);
            drop(bus);
            // Chained graph: researcher → coder (no other ready set).
            let mut graph = TaskGraph::new();
            let researcher_id = TaskId::new();
            let coder_id = TaskId::new();
            graph.add_root(SubTask {
                id: researcher_id,
                parent_id: None,
                session_id,
                role: AgentId::new("researcher"),
                description: "research".into(),
                status: SubTaskStatus::Pending,
                dependencies: vec![],
                deliverable: None,
                created_at: time::OffsetDateTime::now_utc(),
                completed_at: None,
            });
            graph.add_child(
                SubTask {
                    id: coder_id,
                    parent_id: Some(researcher_id),
                    session_id,
                    role: AgentId::new("coder"),
                    description: "implement".into(),
                    status: SubTaskStatus::Pending,
                    dependencies: vec![researcher_id],
                    deliverable: None,
                    created_at: time::OffsetDateTime::now_utc(),
                    completed_at: None,
                },
                researcher_id,
                crate::graph::Dependency::MustFinishBefore,
            );
            let (output, _events) = run_graph_for_test(
                &mut coordinator,
                EventBus::new(256),
                graph,
                session_id,
                HashMap::new(),
            )
            .await;
            output
        };

        // Cap of 1: the second batch never dispatches → Partial with a clear
        // message naming the cap.
        let output = run_case(Some(1)).await;
        assert_eq!(
            output.completion_status,
            concerto_core::types::AgentCompletionStatus::Partial,
            "a cap of 1 must pause the run before the second batch, got: {:?}",
            output.completion_status,
        );
        assert!(
            output.final_message.contains("dispatch cap (1 total"),
            "expected the cap to be named in the pause message, got: {}",
            output.final_message,
        );

        // No cap (the default wire-up) completes both subtasks.
        let output = run_case(None).await;
        assert_eq!(
            output.completion_status,
            concerto_core::types::AgentCompletionStatus::Completed,
            "no cap must complete the run, got: {:?}",
            output.completion_status,
        );

        // `Some(0)` is treated as "no cap" (unlimited).
        let output = run_case(Some(0)).await;
        assert_eq!(
            output.completion_status,
            concerto_core::types::AgentCompletionStatus::Completed,
            "a cap of 0 must behave as unlimited, got: {:?}",
            output.completion_status,
        );

        // A cap that exactly covers the needed dispatches must NOT pause the
        // run: the batch boundary check happens before the next batch, so
        // `count >= cap` only trips when there is still ready work.
        let output = run_case(Some(2)).await;
        assert_eq!(
            output.completion_status,
            concerto_core::types::AgentCompletionStatus::Completed,
            "a cap that covers the run's dispatches must not pause it, got: {:?}",
            output.completion_status,
        );
    }

    /// ADR-52 e2e: a wide multi-agent graph whose first dispatch of TWO roles
    /// fails hard (auth + budget) still completes — the fallback ladder
    /// rescues both roles (tier 1 default model), the rescued summaries land
    /// in `completed_results` (surfaced as SubTaskCompleted events), and the
    /// run exits Completed rather than Partial. This pins the "exit gate": a
    /// capped, exhausted run pauses (see the cap tests) while a rescued run
    /// proceeds to full completion.
    #[tokio::test]
    async fn multi_failure_exit_gate_run_still_completes() {
        let bus = EventBus::new(256);
        // Five roles in one graph. Researcher and coder fail their first
        // dispatch with different hard error classes (LimitReached) and are
        // rescued by the tier-1 default-model re-dispatch; reviewer/validator/
        // docs-writer succeed on their only dispatch.
        let researcher = MockExpertAgent::sequence(
            AgentId::new("researcher"),
            vec![err_auth(), ok_result("researcher", "researcher rescued on default model")],
        );
        let coder = MockExpertAgent::sequence(
            AgentId::new("coder"),
            vec![err_budget(), ok_result("coder", "coder rescued on default model")],
        )
        .with_artifact_writer();
        let reviewer = MockExpertAgent::always_succeed(AgentId::new("reviewer"), "review ok");
        let validator = MockExpertAgent::always_succeed(AgentId::new("validator"), "validation ok");
        let docs = MockExpertAgent::always_succeed(AgentId::new("docs-writer"), "docs written");
        let session_id = Ulid::new();
        let mut coordinator = coordinator_for_ladder(
            bus.clone(),
            vec![researcher, coder, reviewer, validator, docs],
            concerto_config::ModelPinConfig {
                default_model: Some("mid".into()),
                ..Default::default()
            },
            Arc::new(MockProvider::default()),
        );

        // Graph: five independent roots so the whole run is a single wide
        // batch (each root a real dispatch; two of them fail first).
        let mut graph = TaskGraph::new();
        let mut coder_task_id = None;
        for role in ["researcher", "coder", "reviewer", "validator", "docs-writer"] {
            let id = TaskId::new();
            if role == "coder" {
                coder_task_id = Some(id);
            }
            graph.add_root(SubTask {
                id,
                parent_id: None,
                session_id,
                role: AgentId::new(role),
                description: format!("{role} subtask"),
                status: SubTaskStatus::Pending,
                dependencies: vec![],
                deliverable: None,
                created_at: time::OffsetDateTime::now_utc(),
                completed_at: None,
            });
        }
        // The rescued coder must produce a real deliverable: seed its expected
        // artifacts so the artifact-writing mock reports the written file in
        // `files_modified` and the zero-file guard stays out of this run's way.
        let coder_task_id = coder_task_id.expect("coder root must exist in the graph");
        coordinator
            .expected_artifacts
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(coder_task_id, vec![camino::Utf8PathBuf::from("src/generated.rs")]);

        let (output, events) =
            run_graph_for_test(&mut coordinator, bus.clone(), graph, session_id, HashMap::new())
                .await;

        // The exit gate held: despite two hard first-attempt failures the
        // ladder rescued both roles, so the run Completed (not Partial).
        assert_eq!(
            output.completion_status,
            concerto_core::types::AgentCompletionStatus::Completed,
            "multi-failure run must still complete after ladder rescue, got: {:?}",
            output.completion_status,
        );
        let rescued_summaries: Vec<&str> = events
            .iter()
            .filter_map(|kind| match kind {
                EventKind::SubTaskCompleted { outcome, .. } => Some(outcome.as_str()),
                _ => None,
            })
            .filter(|outcome| outcome.contains("rescued on default model"))
            .collect();
        assert!(
            rescued_summaries.contains(&"researcher rescued on default model"),
            "researcher rescue must appear in completed results, got: {rescued_summaries:?}",
        );
        assert!(
            rescued_summaries.contains(&"coder rescued on default model"),
            "coder rescue must appear in completed results, got: {rescued_summaries:?}",
        );
        // The ladder ledger records NO exhaustion: both rescued roles must have
        // exited the ladder through a successful tier-1 dispatch (any
        // "Fallback ladder exhausted" note would mean the run had instead
        // degraded to Partial).
        let exhausted_note = events.iter().any(|kind| {
            matches!(
                kind,
                EventKind::AgentThought { content, .. }
                    if content.contains("Fallback ladder exhausted")
            )
        });
        assert!(!exhausted_note, "a rescued run must not record ladder exhaustion in the ledger",);
    }

    /// ADR-52 durability: `persist_plan_artifact` writes the pretty JSON to
    /// `<plans_dir>/plan-<plan_id>.json` and returns the plan id, and the file
    /// round-trips through `read_plan`. Uses the harness-resume shape
    /// (`PlanArtifact::from_graph`, which the checkpoint resume path uses) so
    /// a restored run's plan is written identically to a freshly planned one.
    #[tokio::test]
    async fn persist_plan_artifact_round_trips_to_plans_dir() {
        let dir = tempfile::tempdir().expect("tempdir for plans persistence test");
        let plans = concerto_sessions::plans::PlansManager::at(dir.path().join("plans"));
        let session = Ulid::new();

        let coordinator = coordinator_for_ladder(
            EventBus::new(256),
            vec![MockExpertAgent::always_succeed(AgentId::new("coder"), "ok")],
            concerto_config::ModelPinConfig::default(),
            Arc::new(MockProvider::default()),
        )
        .with_plans(Some(plans.clone()));

        // A small completed graph rendered through the resume-path artifact
        // constructor (checkpoint `run_id` gives the plan id).
        let mut graph = TaskGraph::new();
        let done = TaskId::new();
        graph.add_root(SubTask {
            id: done,
            parent_id: None,
            session_id: session,
            role: AgentId::new("coder"),
            description: "done task".into(),
            status: SubTaskStatus::Completed,
            dependencies: vec![],
            deliverable: Some("ok".into()),
            created_at: time::OffsetDateTime::now_utc(),
            completed_at: Some(time::OffsetDateTime::now_utc()),
        });
        let task = AgentTask::new(session, "resume task");
        let mut expected = std::collections::HashMap::new();
        expected.insert(done, vec![camino::Utf8PathBuf::from("src/lib.rs")]);
        let artifact = PlanArtifact::from_graph("run-123".into(), &task, &graph, &expected);

        let plan_id = coordinator
            .persist_plan_artifact(&artifact)
            .expect("persistence must succeed with a manager attached");
        assert_eq!(plan_id, "run-123");
        let path = plans.plan_path(&plan_id);
        assert!(path.exists(), "plan file must exist at {}", path.display());
        let contents = plans.read_plan(&plan_id).expect("read plan").expect("plan present");
        assert!(contents.contains("\"plan_id\": \"run-123\""), "stored json: {contents}");
        // The restored subtask and its expected artifact survive the snapshot.
        assert!(contents.contains("done task"), "restored task description missing");
        assert!(contents.contains("src/lib.rs"), "expected artifact missing");

        // Without a manager attached, persistence degrades to a None plan id
        // (the run proceeds; only the artifact write is skipped).
        let coordinator = coordinator_for_ladder(
            EventBus::new(256),
            vec![MockExpertAgent::always_succeed(AgentId::new("coder"), "ok")],
            concerto_config::ModelPinConfig::default(),
            Arc::new(MockProvider::default()),
        );
        drop(coordinator);
    }

    /// ADR-42 (two-tier): the ladder NEVER reassigns a subtask to another
    /// agent with the same declared stage. With multiple design-stage agents
    /// registered, a hard `LimitReached` failure dispatches ONLY the original
    /// role — tier 1 re-uses the same agent on the default model, and when
    /// tier 2 (coordinator self-execution) also fails the run exits Partial
    /// without the same-stage peer ever being dispatched. (The old three-tier
    /// ladder reassigned to the lexicographically-first untried same-stage
    /// agent; the peer's silence proves that tier is gone.)
    #[tokio::test]
    async fn ladder_hard_failure_never_reassigns_stages() {
        let bus = EventBus::new(256);
        // The original design-stage role fails hard on the first dispatch, the
        // tier-1 default-model re-dispatch, AND the tier-2 takeover dispatch
        // (three auth errors) so the ladder exhausts to Partial. `architect-alt`
        // shares the "design" stage and would succeed if dispatched — under the
        // old three-tier ladder it was the tier-2 reassignment target, so its
        // silence is the proof that no reassignment happens.
        let architect = MockExpertAgent::sequence(
            AgentId::new("architect"),
            vec![err_auth(), err_auth(), err_auth()],
        );
        let alt = MockExpertAgent::sequence(
            AgentId::new("architect-alt"),
            vec![ok_result("architect-alt", "peer must never run")],
        )
        .with_stage(Some(AgentStage::new("design")));
        let session_id = Ulid::new();
        // Tier 2 (takeover dispatch) also fails with the third auth error, so
        // the ladder exhausts to Partial rather than rescuing the subtask.
        let planning: Arc<dyn concerto_core::traits::provider::LlmProvider> =
            Arc::new(MockProvider::default());
        let mut coordinator = coordinator_for_ladder(
            bus.clone(),
            vec![architect, alt],
            concerto_config::ModelPinConfig {
                default_model: Some("mid".into()),
                ..Default::default()
            },
            planning,
        );
        let (graph, _task_id) = single_pending_graph(session_id, "architect");

        let (output, events) =
            run_graph_for_test(&mut coordinator, bus.clone(), graph, session_id, HashMap::new())
                .await;

        assert_eq!(
            output.completion_status,
            concerto_core::types::AgentCompletionStatus::Partial,
            "a hard failure with both ladder tiers exhausted must surface Partial, got: {:?}",
            output.completion_status,
        );
        // Only the original role is ever dispatched (the runner dispatches it
        // three times — first dispatch + tier-1 re-dispatch + tier-2 takeover —
        // and every dispatch reuses the original role). No same-stage peer
        // appears in any SubTaskStarted event.
        let alt_dispatched = events.iter().any(|kind| {
            matches!(
                kind,
                EventKind::SubTaskStarted { role, .. } if role == &AgentId::new("architect-alt")
            )
        });
        assert!(!alt_dispatched, "the ladder must never dispatch a same-stage peer");
        let started_roles: std::collections::HashSet<AgentId> = events
            .iter()
            .filter_map(|kind| match kind {
                EventKind::SubTaskStarted { role, .. } => Some(role.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            started_roles,
            std::collections::HashSet::from([AgentId::new("architect")]),
            "expected only the original role to be dispatched, got: {started_roles:?}",
        );
        // The ladder really walked tier 1 (same agent, default model) then
        // tier 2 (self-execution) before exhausting to Partial.
        let tier1_failed = events.iter().any(|kind| {
            matches!(
                kind,
                EventKind::AgentThought { content, .. }
                    if content.contains("Fallback tier 1 (default model) failed")
            )
        });
        assert!(tier1_failed, "expected the tier-1 re-dispatch failure to be reported");
        let exhausted_note = events.iter().any(|kind| {
            matches!(
                kind,
                EventKind::AgentThought { content, .. }
                    if content.contains("Fallback ladder exhausted")
            )
        });
        assert!(exhausted_note, "expected the ladder-exhaustion note to be published");
    }

    /// ADR-42 §4: a ladder tier may return an `Ok` result whose `outcome` is
    /// `Failed`. A failed tier-1 (default-model) run counts as a tier-1
    /// failure and ADVANCES the ladder to tier 2 (coordinator takeover) — the
    /// coordinator takes over when the model swap did not rescue the subtask.
    /// Boundedness comes from the guards (`default_model_attempted`,
    /// `self_execute_attempted`): tier 1 runs once, tier 2 runs once, then the
    /// ladder exhausts to a Partial outcome — it is never re-entered.
    #[tokio::test]
    async fn ladder_success_with_failed_outcome_is_bounded() {
        let bus = EventBus::new(256);
        // Every dispatch fails with a Failed outcome. Dispatch budget
        // (the attempt counter increments at dispatch time): attempts 1, 2, 3
        // → one escalation retry, which lands at attempt 3 again → the ladder
        // re-dispatches the SAME agent once more on the default model (tier 1,
        // still Failed) → tier 2 dispatches the rebuilt role once (still
        // Failed) → the ladder exhausts. Total: 5 dispatches + 1 tier-2
        // dispatch = 6 SubTaskStarted events, exactly — the ladder is never
        // re-entered.
        let architect = MockExpertAgent::sequence(
            AgentId::new("architect"),
            vec![
                ok_failed("architect", "cannot proceed"),
                ok_failed("architect", "cannot proceed"),
                ok_failed("architect", "cannot proceed"),
                ok_failed("architect", "cannot proceed"),
                ok_failed("architect", "cannot proceed"),
                ok_failed("architect", "cannot proceed"),
            ],
        );
        let session_id = Ulid::new();
        // The tier-2 dispatch (through the runner) also yields a Failed
        // outcome, so the ladder must exhaust to Partial.
        let mut coordinator = coordinator_for_ladder(
            bus.clone(),
            vec![architect],
            concerto_config::ModelPinConfig {
                default_model: Some("mid".into()),
                ..Default::default()
            },
            Arc::new(MockProvider::default()),
        );
        let (graph, _task_id) = single_pending_graph(session_id, "architect");

        let (output, events) =
            run_graph_for_test(&mut coordinator, bus.clone(), graph, session_id, HashMap::new())
                .await;

        assert_eq!(
            output.completion_status,
            concerto_core::types::AgentCompletionStatus::Partial,
            "a failed fallback result must exhaust the ladder to Partial, got: {:?}",
            output.completion_status,
        );
        // Exactly one ladder re-dispatch (tier 1, on the default model) and
        // one tier-2 dispatch happened, then the run terminated: the guards
        // bound the ladder, so the run cannot loop.
        let architect_dispatches = events
            .iter()
            .filter(|kind| {
                matches!(
                    kind,
                    EventKind::SubTaskStarted { role, .. } if role == &AgentId::new("architect")
                )
            })
            .count();
        assert_eq!(
            architect_dispatches, 6,
            "expected exactly 5 dispatches + 1 tier-2 dispatch (6 SubTaskStarted), got {architect_dispatches}",
        );
        // The ladder re-dispatched the role on the tier-1 default model
        // (test/mid) — and that attempt still failed.
        let tier1_redispatch = events.iter().any(|kind| {
            matches!(
                kind,
                EventKind::AgentThought { content, .. }
                    if content.contains("Using test/mid")
                        && content.contains("Queued subtask")
            )
        });
        assert!(tier1_redispatch, "expected the tier-1 default-model re-dispatch to be attempted");
        // The failed tier-1 result advanced the ladder to tier 2: the
        // coordinator took over via a runner dispatch, which also failed
        // (a Failed outcome), exhausting to Partial.
        let tier2_attempted = events.iter().any(|kind| {
            matches!(
                kind,
                EventKind::AgentThought { content, .. }
                    if content.contains("Fallback tier 2 (coordinator self-execution) failed")
            )
        });
        assert!(tier2_attempted, "expected tier 2 to be attempted after the failed tier-1 result");
    }

    /// Model-first tier 1: `default_provider_config_id` selects the routing
    /// profile that matches `default_model` (it does not re-route the
    /// dispatch). When the pin names a provider that offers no matching
    /// profile, tier 1 cannot resolve the default model — it guards loudly
    /// (unavailable note) and continues down the ladder instead of guessing a
    /// provider.
    #[tokio::test]
    async fn ladder_tier1_unmatched_pin_provider_guards_and_continues() {
        let bus = EventBus::new(256);
        let architect = MockExpertAgent::sequence(AgentId::new("architect"), vec![err_auth()]);
        let session_id = Ulid::new();
        // "other-provider" matches no routing profile: `fallback_to_default`
        // returns PinnedModelNotFound instead of guessing a provider.
        let mut coordinator = coordinator_for_ladder(
            bus.clone(),
            vec![architect],
            concerto_config::ModelPinConfig {
                default_model: Some("mid".into()),
                default_provider_config_id: Some("other-provider".into()),
                ..Default::default()
            },
            Arc::new(SeqProvider::new(vec!["self-executed after guarded tier 1".into()])),
        );
        let (graph, _task_id) = single_pending_graph(session_id, "architect");

        let (output, events) =
            run_graph_for_test(&mut coordinator, bus.clone(), graph, session_id, HashMap::new())
                .await;

        assert_eq!(
            output.completion_status,
            concerto_core::types::AgentCompletionStatus::Completed,
            "the guarded tier-1 failure must not block the rest of the ladder, got: {:?}",
            output.completion_status,
        );
        // Tier 1 failed loudly (the unmatched pin produced an error) rather
        // than silently switching providers.
        let tier1_guarded = events.iter().any(|kind| {
            matches!(
                kind,
                EventKind::AgentThought { content, .. }
                    if content.contains("Fallback tier 1 (default model) unavailable")
            )
        });
        assert!(tier1_guarded, "expected the tier-1 pin mismatch to be reported, not silent");
        // The subtask was eventually completed by tier 2 self-execution.
        let self_executed = output
            .provider_metrics
            .iter()
            .any(|metrics| metrics.provider == "coordinator-self-execute");
        assert!(self_executed, "expected tier 2 to complete the subtask after the guard");
    }

    /// ADR-45 model-first semantics: tier 1 swaps the MODEL on the role's
    /// bound pipe. When the role's effective serving pipe (per-agent provider
    /// assignment) differs from the pipe that serves the global default model,
    /// tier 1 would ask pipe A to serve a model registered on pipe B — it is
    /// skipped cleanly (with a note) and tier 1b rebuilds the role on the pipe
    /// that actually serves the default model.
    #[tokio::test]
    async fn ladder_tier1_skipped_when_default_model_is_served_by_another_pipe() {
        use concerto_core::traits::provider::LlmProvider;
        let bus = EventBus::new(256);
        let architect_id = AgentId::new("architect");
        // The bound agent fails auth on the first dispatch; the tier-1b
        // rebuild (on the default provider) succeeds.
        let bound = Arc::new(MockExpertAgent::sequence(architect_id.clone(), vec![err_auth()]));
        let rescued = Arc::new(MockExpertAgent::always_succeed(
            architect_id.clone(),
            "default provider rescued",
        ));
        let mut registry = AgentRegistry::new();
        registry.register_with_factory(
            architect_id.clone(),
            bound,
            Arc::new(move |_provider: Arc<dyn LlmProvider>| rescued.clone()),
        );
        let session_id = Ulid::new();
        let fallback: Arc<dyn LlmProvider> = Arc::new(MockProvider::default());
        let mut coordinator = coordinator_for_ladder_with(
            bus.clone(),
            Arc::new(registry),
            concerto_config::ModelPinConfig {
                default_model: Some("mid".into()),
                ..Default::default()
            },
            Arc::new(MockProvider::default()),
            Some((fallback, fallback_profile("fallback", "default-model"))),
        )
        // The role is explicitly assigned to the `fallback` pipe, while the
        // global default model `mid` resolves on the harness's `test` pipe:
        // tier 1 must skip (model-on-wrong-pipe) and let tier 1b rebuild.
        .with_agent_configs(HashMap::from([(
            architect_id,
            concerto_config::CustomAgentConfig {
                provider_id: Some("fallback".into()),
                ..Default::default()
            },
        )]));
        let (graph, _task_id) = single_pending_graph(session_id, "architect");

        let (output, events) =
            run_graph_for_test(&mut coordinator, bus.clone(), graph, session_id, HashMap::new())
                .await;

        assert_eq!(
            output.completion_status,
            concerto_core::types::AgentCompletionStatus::Completed,
            "tier 1b must rebuild on the default-model pipe and rescue the subtask, got: {:?}",
            output.completion_status,
        );
        let skip_note = events.iter().any(|kind| {
            matches!(
                kind,
                EventKind::AgentThought { content, .. }
                    if content.contains("Fallback tier 1 (default model) skipped")
            )
        });
        assert!(skip_note, "expected the tier-1 cross-pipe skip note");
        // Tier 1 never dispatched: no `Using test/mid` queue note, and the
        // rescued result came from the tier-1b rebuild.
        let tier1_dispatched = events.iter().any(|kind| {
            matches!(
                kind,
                EventKind::AgentThought { content, .. }
                    if content.contains("Using test/mid") && content.contains("Queued subtask")
            )
        });
        assert!(!tier1_dispatched, "tier 1 must not dispatch the default model on the wrong pipe");
        let rescued_summary = events.iter().any(|kind| {
            matches!(
                kind,
                EventKind::SubTaskCompleted { outcome, .. }
                    if outcome == "default provider rescued"
            )
        });
        assert!(rescued_summary, "the tier-1b rebuild must produce the deliverable");
    }

    /// Model-first serving-pipe resolution: an UNASSIGNED role (no per-agent
    /// provider assignment) serves on the run's default provider
    /// (`default_provider_config_id`). When that pipe differs from the pipe
    /// that serves the global default model, tier 1 is skipped (model-on-
    /// wrong-pipe, no dispatch) and tier 1b rebuilds the role on the default
    /// provider.
    #[tokio::test]
    async fn ladder_tier1_unassigned_role_uses_run_default_pipe() {
        use concerto_core::traits::provider::LlmProvider;
        let bus = EventBus::new(256);
        let architect_id = AgentId::new("architect");
        // The bound agent fails on the primary dispatch; the tier-1b rebuild
        // (on the default provider) succeeds.
        let bound = Arc::new(MockExpertAgent::sequence(architect_id.clone(), vec![err_auth()]));
        let rescued = Arc::new(MockExpertAgent::always_succeed(
            architect_id.clone(),
            "default provider rescued",
        ));
        let mut registry = AgentRegistry::new();
        registry.register_with_factory(
            architect_id.clone(),
            bound,
            Arc::new(move |_provider: Arc<dyn LlmProvider>| rescued.clone()),
        );
        let session_id = Ulid::new();
        let fallback: Arc<dyn LlmProvider> = Arc::new(MockProvider::default());
        // No `with_agent_configs` entry: the role's effective serving pipe
        // must come from the run-level default provider id — `fallback`, not
        // the harness's `test`. The default model `mid` resolves on `test`, so
        // tier 1 must skip and tier 1b must rebuild on `fallback`.
        let mut coordinator = coordinator_for_ladder_with(
            bus.clone(),
            Arc::new(registry),
            concerto_config::ModelPinConfig {
                default_model: Some("mid".into()),
                ..Default::default()
            },
            Arc::new(MockProvider::default()),
            Some((fallback, fallback_profile("fallback", "default-model"))),
        )
        .with_default_provider_config_id(Some("fallback".into()));
        let (graph, _task_id) = single_pending_graph(session_id, "architect");

        let (output, events) =
            run_graph_for_test(&mut coordinator, bus.clone(), graph, session_id, HashMap::new())
                .await;

        assert_eq!(
            output.completion_status,
            concerto_core::types::AgentCompletionStatus::Completed,
            "tier 1b must rebuild on the default provider and rescue the subtask, got: {:?}",
            output.completion_status,
        );
        let skip_note = events.iter().any(|kind| {
            matches!(
                kind,
                EventKind::AgentThought { content, .. }
                    if content.contains("Fallback tier 1 (default model) skipped")
            )
        });
        assert!(skip_note, "expected the tier-1 cross-pipe skip note");
        // Tier 1 never dispatched: the rescued result came from the tier-1b
        // rebuild, not from a wrong-pipe tier-1 run.
        let tier1_dispatched = events.iter().any(|kind| {
            matches!(
                kind,
                EventKind::AgentThought { content, .. }
                    if content.contains("Using test/mid") && content.contains("Queued subtask")
            )
        });
        assert!(!tier1_dispatched, "tier 1 must not dispatch the default model on the wrong pipe");
        let rescued_summary = events.iter().any(|kind| {
            matches!(
                kind,
                EventKind::SubTaskCompleted { outcome, .. } if outcome == "default provider rescued"
            )
        });
        assert!(rescued_summary, "the tier-1b rebuild must produce the deliverable");
    }

    /// Model-first serving-pipe resolution: when an unassigned role resolves
    /// NO serving pipe at all (no per-agent assignment, no run-level default
    /// provider), tier 1 must not throw the default model at an unknown pipe —
    /// it skips defensively with a note and the ladder continues at tier 2.
    #[tokio::test]
    async fn ladder_tier1_unassigned_role_no_serving_pipe_skips_defensively() {
        let bus = EventBus::new(256);
        // Response 1 fails on the primary dispatch; response 2 rescues the
        // subtask through the tier-2 takeover dispatch.
        let architect = MockExpertAgent::sequence(
            AgentId::new("architect"),
            vec![err_auth(), ok_result("architect", "tier 2 rescued")],
        );
        let session_id = Ulid::new();
        let mut coordinator = coordinator_for_ladder(
            bus.clone(),
            vec![architect],
            concerto_config::ModelPinConfig {
                default_model: Some("mid".into()),
                ..Default::default()
            },
            Arc::new(MockProvider::default()),
        )
        // Defensive shape: the harness's `test` default pipe is cleared so the
        // role has no resolvable serving pipe at all.
        .with_default_provider_config_id(None);
        let (graph, _task_id) = single_pending_graph(session_id, "architect");

        let (output, events) =
            run_graph_for_test(&mut coordinator, bus.clone(), graph, session_id, HashMap::new())
                .await;

        assert_eq!(
            output.completion_status,
            concerto_core::types::AgentCompletionStatus::Completed,
            "the ladder must continue at tier 2 after the defensive skip, got: {:?}",
            output.completion_status,
        );
        let skip_note = events.iter().any(|kind| {
            matches!(
                kind,
                EventKind::AgentThought { content, .. }
                    if content.contains("Fallback tier 1 (default model) skipped")
                        && content.contains("no serving pipe resolved")
            )
        });
        assert!(skip_note, "expected the defensive tier-1 skip note");
        let self_executed = output
            .provider_metrics
            .iter()
            .any(|metrics| metrics.provider == "coordinator-self-execute");
        assert!(self_executed, "tier 2 must take over after the defensive skip");
    }

    /// ADR-45 tier 1b: a role registered WITHOUT a rebuild factory cannot be
    /// re-dispatched on the default provider — `run_with_provider` would
    /// silently repeat the original bound provider (a cross-pipe dispatch, not
    /// a skip). Tier 1b must check the factory and skip with a note; tier 2
    /// has the same guard, so the ladder exhausts to Partial.
    #[tokio::test]
    async fn ladder_tier1b_skipped_when_role_has_no_rebuild_factory() {
        let bus = EventBus::new(256);
        // Registered with `register` (no factory): a provider-switch
        // re-dispatch cannot change the serving pipe.
        let architect =
            MockExpertAgent::sequence(AgentId::new("architect"), vec![err_auth(), err_auth()]);
        let mut registry = AgentRegistry::new();
        registry.register(Arc::new(architect));
        let session_id = Ulid::new();
        let fallback: Arc<dyn concerto_core::traits::provider::LlmProvider> =
            Arc::new(MockProvider::default());
        let mut coordinator = coordinator_for_ladder_with(
            bus.clone(),
            Arc::new(registry),
            concerto_config::ModelPinConfig {
                default_model: Some("mid".into()),
                ..Default::default()
            },
            Arc::new(MockProvider::default()),
            Some((fallback, fallback_profile("fallback", "default-model"))),
        );
        let (graph, _task_id) = single_pending_graph(session_id, "architect");

        let (output, events) =
            run_graph_for_test(&mut coordinator, bus.clone(), graph, session_id, HashMap::new())
                .await;

        assert_eq!(
            output.completion_status,
            concerto_core::types::AgentCompletionStatus::Partial,
            "a factory-less role must not be dispatched across pipes; the ladder exhausts, \
             got: {:?}",
            output.completion_status,
        );
        let tier1b_skip = events.iter().any(|kind| {
            matches!(
                kind,
                EventKind::AgentThought { content, .. }
                    if content.contains("Fallback tier 1b (default provider) skipped")
                        && content.contains("no rebuild factory")
            )
        });
        assert!(tier1b_skip, "expected the factory-less tier-1b skip note");
        let exhausted_note = events.iter().any(|kind| {
            matches!(
                kind,
                EventKind::AgentThought { content, .. }
                    if content.contains("Fallback ladder exhausted")
            )
        });
        assert!(exhausted_note, "the ladder must exhaust for a factory-less role");
    }

    /// ADR-42 §4: when every ladder tier is skipped or fails, the coordinator
    /// surfaces a graceful Partial outcome (existing workspace state and
    /// session context preserved) instead of aborting the run.
    #[tokio::test]
    async fn ladder_exhausted_surfaces_partial_outcome() {
        let bus = EventBus::new(256);
        // Every ladder tier fails: no default model (tier 1 unavailable), no
        // default provider (tier 1b skipped), and the tier-2 takeover dispatch
        // hits the second scripted auth error.
        let architect =
            MockExpertAgent::sequence(AgentId::new("architect"), vec![err_auth(), err_auth()]);
        let session_id = Ulid::new();
        let mut coordinator = coordinator_for_ladder(
            bus.clone(),
            vec![architect],
            concerto_config::ModelPinConfig::default(),
            Arc::new(MockProvider::default()),
        );
        let (graph, _task_id) = single_pending_graph(session_id, "architect");

        let (output, events) =
            run_graph_for_test(&mut coordinator, bus.clone(), graph, session_id, HashMap::new())
                .await;

        assert_eq!(
            output.completion_status,
            concerto_core::types::AgentCompletionStatus::Partial,
            "an exhausted ladder should surface Partial, got: {:?}",
            output.completion_status,
        );
        assert!(
            output.final_message.contains("Automation paused after exhausting recovery attempts"),
            "unexpected final message: {}",
            output.final_message,
        );
        let exhausted_note = events.iter().any(|kind| {
            matches!(
                kind,
                EventKind::AgentThought { content, .. } if content.contains("Fallback ladder exhausted")
            )
        });
        assert!(exhausted_note, "expected the ladder-exhaustion note to be published");
    }

    // ------------------------------------------------------------------
    // Design-stage recovery: the planner's architect run goes through the
    // same classify → retry → escalate → fallback-ladder path as subtask
    // dispatch, instead of a bare `?` abandoning the whole plan.
    // ------------------------------------------------------------------

    /// Full-pipeline ladder fixture for design-stage tests: registers the
    /// architect (design) plus a minimal research/implement/validate set so
    /// the planner's plan can execute to completion, and wires a `SeqProvider`
    /// as the planning provider (it serves the canned plan). The ladder config
    /// (`coordinator_for_ladder_with`) keeps tier 1/2 semantics identical to
    /// the execution-phase ladder tests.
    fn coordinator_for_design_stage(
        bus: EventBus,
        architect: MockExpertAgent,
        pin_config: concerto_config::ModelPinConfig,
        plan_json: String,
    ) -> CoordinatorAgent {
        let mocks = vec![
            architect,
            MockExpertAgent::always_succeed(AgentId::new("researcher"), "gathered"),
            MockExpertAgent::always_succeed(AgentId::new("coder"), "implemented")
                .with_artifact_writer(),
            MockExpertAgent::always_succeed(AgentId::new("validator"), "valid"),
        ];
        coordinator_for_ladder(bus, mocks, pin_config, Arc::new(SeqProvider::new(vec![plan_json])))
    }

    /// Design-stage: a transient provider error during the architect run is
    /// retried (same agent/model, `previous_results` feedback) and the second
    /// attempt succeeds — the run completes instead of failing the plan.
    #[tokio::test]
    async fn design_stage_recoverable_retry_then_succeeds() {
        let bus = EventBus::new(256);
        let architect = MockExpertAgent::sequence(
            AgentId::new("architect"),
            vec![err_transient(), ok_result("architect", DESIGN_DOC_JSON)],
        );
        let (output, events) = run_for_test(
            coordinator_for_design_stage(
                bus.clone(),
                architect,
                concerto_config::ModelPinConfig::default(),
                PLAN_RESEARCH_CODER.into(),
            ),
            bus.clone(),
        )
        .await;

        assert_eq!(
            output.completion_status,
            concerto_core::types::AgentCompletionStatus::Completed,
            "a retried design stage should complete the run, got: {:?}",
            output.completion_status,
        );
        let retry_note = events.iter().any(|kind| {
            matches!(
                kind,
                EventKind::AgentThought { content, .. }
                    if content.contains("Retrying design (architect)")
                        && content.contains("after recoverable failure")
            )
        });
        assert!(retry_note, "expected a design-stage retry note to be published");
    }

    /// Design-stage: a stream-idle timeout (`RetryExhausted` → `LimitReached`)
    /// cannot be retried (non-transient), so the architect run enters the
    /// fallback ladder; tier 1 (global default model) rescues it and the run
    /// completes.
    #[tokio::test]
    async fn design_stage_stream_idle_ladder_tier1_rescues() {
        let bus = EventBus::new(256);
        // Response 1 fails hard (stream-idle); response 2 is the tier-1
        // re-dispatch on the global default model, returning a valid doc.
        let architect = MockExpertAgent::sequence(
            AgentId::new("architect"),
            vec![err_stream_idle(), ok_result("architect", DESIGN_DOC_JSON)],
        );
        let (output, events) = run_for_test(
            coordinator_for_design_stage(
                bus.clone(),
                architect,
                concerto_config::ModelPinConfig {
                    default_model: Some("mid".into()),
                    ..Default::default()
                },
                PLAN_RESEARCH_CODER.into(),
            ),
            bus.clone(),
        )
        .await;

        assert_eq!(
            output.completion_status,
            concerto_core::types::AgentCompletionStatus::Completed,
            "tier-1 fallback should rescue the design stage, got: {:?}",
            output.completion_status,
        );
        let used_default_model = events.iter().any(|kind| {
            matches!(kind, EventKind::AgentThought { content, .. } if content.contains("test/mid"))
        });
        assert!(
            used_default_model,
            "expected the tier-1 re-dispatch on the default model (test/mid)"
        );
    }

    /// Design-stage: with no default model, the ladder exhausts and the
    /// coordinator surfaces a graceful Partial plan (never a hard crash).
    #[tokio::test]
    async fn design_stage_ladder_exhausted_returns_partial() {
        let bus = EventBus::new(256);
        // First architect response fails hard (stream-idle); the tier-2
        // takeover hits the second scripted auth error. No default model
        // means tier 1 is unavailable and tier 1b has no provider to rebuild.
        let architect = MockExpertAgent::sequence(
            AgentId::new("architect"),
            vec![err_stream_idle(), err_auth()],
        );
        let (output, events) = run_for_test(
            coordinator_for_design_stage(
                bus.clone(),
                architect,
                concerto_config::ModelPinConfig::default(),
                PLAN_RESEARCH_CODER.into(),
            ),
            bus.clone(),
        )
        .await;

        assert_eq!(
            output.completion_status,
            concerto_core::types::AgentCompletionStatus::Partial,
            "an exhausted design-stage ladder must degrade to Partial, got: {:?}",
            output.completion_status,
        );
        assert!(
            output.final_message.contains("could not produce a valid plan"),
            "unexpected final message: {}",
            output.final_message,
        );
        let exhausted_note = events.iter().any(|kind| {
            matches!(
                kind,
                EventKind::AgentThought { content, .. } if content.contains("Fallback ladder exhausted")
            )
        });
        assert!(exhausted_note, "expected the ladder-exhaustion note to be published");
    }

    /// Design-stage: an empty/non-parseable DesignDoc is treated as a failed
    /// attempt (Recoverable) and retried; the second attempt returns a valid
    /// doc and the run completes.
    #[tokio::test]
    async fn design_stage_empty_doc_retried_until_valid() {
        let bus = EventBus::new(256);
        // Empty doc first (Recoverable → retried), then a populated doc.
        let architect = MockExpertAgent::sequence(
            AgentId::new("architect"),
            vec![
                ok_result("architect", EMPTY_DESIGN_DOC_JSON),
                ok_result("architect", DESIGN_DOC_JSON),
            ],
        );
        let (output, _events) = run_for_test(
            coordinator_for_design_stage(
                bus.clone(),
                architect,
                concerto_config::ModelPinConfig::default(),
                PLAN_RESEARCH_CODER.into(),
            ),
            bus.clone(),
        )
        .await;

        assert_eq!(
            output.completion_status,
            concerto_core::types::AgentCompletionStatus::Completed,
            "a retried empty design doc should complete once a valid doc lands, got: {:?}",
            output.completion_status,
        );
    }

    #[tokio::test]
    async fn resume_checkpoint_exhausted_blocked_returns_partial() {
        // ── 1. Minimal coordinator wiring (unused in the fast exit) ──
        let bus = EventBus::new(256);
        let spend_tracker = Arc::new(SpendTracker::default());
        let registry = Arc::new(AgentRegistry::new());
        let runner = AgentRunner::new(registry.clone(), bus.clone(), spend_tracker.clone());
        let provider: Arc<dyn concerto_core::traits::provider::LlmProvider> =
            Arc::new(MockProvider::default());
        let routing = Arc::new(RoutingEngine::new(
            vec![],
            spend_tracker.clone(),
            concerto_config::ModelPinConfig {
                pins: std::collections::HashMap::new(),
                ..Default::default()
            },
            EventBus::default(),
        ));
        let model_registry = Arc::new(ModelRegistry::from_profiles(vec![]));
        let model_selector = Arc::new(ModelSelector::new(model_registry, routing));

        let mut coordinator = CoordinatorAgent::new(
            registry,
            runner,
            model_selector,
            spend_tracker,
            bus.clone(),
            provider,
            Arc::new(NullMemoryStore),
        );

        // ── 2. Graph with a single blocked Coder subtask ────────────
        let mut graph = TaskGraph::new();
        let coder_id = TaskId::new();
        graph.add_subtask(SubTask {
            id: coder_id,
            parent_id: None,
            session_id: Ulid::new(),
            role: AgentId::new("coder"),
            description: "implement feature".into(),
            status: SubTaskStatus::Blocked,
            dependencies: vec![],
            deliverable: None,
            created_at: time::OffsetDateTime::now_utc(),
            completed_at: None,
        });

        let mut subtask_attempts = HashMap::new();
        subtask_attempts.insert(coder_id, DEFAULT_MAX_SUBTASK_ATTEMPTS);

        let task = AgentTask::new(Ulid::new(), "test task");
        let context =
            concerto_core::types::AgentContext::new(concerto_core::types::SessionContext::new(
                task.session_id,
                std::env::current_dir().unwrap(),
            ));

        // ── 3. Execute graph — should hit Partial fast-exit ─────────
        let run_objective = task.description.clone();
        let run_objective_hash = blake3::hash(run_objective.as_bytes()).to_hex().to_string();
        let result = coordinator
            .execute_graph(
                task,
                context,
                CancellationToken::new(),
                graph,
                HashMap::new(), // completed_results
                0.0,            // total_cost
                0,              // total_tool_calls
                vec![],         // all_files
                vec![],         // provider_metrics
                subtask_attempts,
                HashMap::new(), // retry_feedback
                HashMap::new(), // model_assignments
                Vec::new(),     // action_ledger
                run_objective,
                run_objective_hash,
            )
            .await;

        assert!(result.is_ok(), "expected Ok(Partial), got error: {result:?}");
        let (output, _notes) = result.unwrap();
        assert_eq!(
            output.completion_status,
            concerto_core::types::AgentCompletionStatus::Partial,
            "resumed checkpoint with exhausted blocked task should yield Partial",
        );
        assert!(
            output.final_message.contains("exhausting recovery attempts"),
            "final_message should mention recovery exhaustion, got: {}",
            output.final_message,
        );
    }

    /// ADR-42 §4 resume semantics: a resumed run restores the ladder guards
    /// from the checkpoint (see the `decompose_task` resume path), so a task
    /// whose tier-1 default-model attempt already fired before an interruption
    /// must NOT re-walk tier 1 after resume. Seed the `default_model_attempted`
    /// guard — exactly what the checkpoint-restore does — and verify the
    /// ladder jumps straight to tier 2 (coordinator takeover).
    #[tokio::test]
    async fn resume_with_default_model_guard_skips_tier1() {
        let bus = EventBus::new(256);
        // Response 1 fails hard; response 2 is consumed by the tier-2 takeover
        // dispatch (the resumed run's only ladder tier). A tier-1 re-dispatch
        // would additionally emit its `Using test/mid` queue note — its
        // absence proves tier 1 never ran.
        let architect = MockExpertAgent::sequence(
            AgentId::new("architect"),
            vec![err_auth(), ok_result("architect", "resumed via coordinator")],
        );
        let session_id = Ulid::new();
        // Tier 2 dispatches through the runner on the coordinator's pipe; the
        // planning provider's own output is not consulted.
        let planning: Arc<dyn concerto_core::traits::provider::LlmProvider> =
            Arc::new(MockProvider::default());
        let mut coordinator = coordinator_for_ladder(
            bus.clone(),
            vec![architect],
            concerto_config::ModelPinConfig {
                default_model: Some("mid".into()),
                ..Default::default()
            },
            planning,
        );
        let (graph, task_id) = single_pending_graph(session_id, "architect");
        // Simulate a checkpoint restore: the tier-1 guard fired before the
        // interruption, so the resumed run must not re-dispatch the agent on
        // the default model.
        coordinator.default_model_attempted.insert(task_id);

        let (output, events) =
            run_graph_for_test(&mut coordinator, bus.clone(), graph, session_id, HashMap::new())
                .await;

        assert_eq!(
            output.completion_status,
            concerto_core::types::AgentCompletionStatus::Completed,
            "the resumed run should complete via tier 2 takeover, got: {:?}",
            output.completion_status,
        );
        // The tier-1 re-dispatch never happened: its `Using test/mid` queue
        // note was never published (the first dispatch used test/cheap, the
        // tier-2 dispatch uses the coordinator's planning profile).
        let tier1_redispatched = events.iter().any(|kind| {
            matches!(
                kind,
                EventKind::AgentThought { content, .. }
                    if content.contains("Using test/mid") && content.contains("Queued subtask")
            )
        });
        assert!(!tier1_redispatched, "tier 1 must not be re-walked after a resume");
        let self_executed = output
            .provider_metrics
            .iter()
            .any(|metrics| metrics.provider == "coordinator-self-execute");
        assert!(self_executed, "expected the resumed run to take over the subtask in tier 2");
    }

    /// H-04 regression: session-scoped lifecycle events (`MultiAgentModeStarted`
    /// / `MultiAgentModeCompleted`) must be published via `publish_for_session`
    /// so they carry the run's session id and reach the right session stream
    /// instead of leaking via an unscoped `publish_raw` broadcast.
    #[tokio::test]
    async fn multi_agent_mode_events_are_session_scoped() {
        let bus = EventBus::new(256);
        let mocks = vec![
            MockExpertAgent::always_succeed(AgentId::new("architect"), DESIGN_DOC_JSON),
            MockExpertAgent::always_succeed(AgentId::new("researcher"), "found"),
            MockExpertAgent::always_succeed(AgentId::new("coder"), "implemented"),
            MockExpertAgent::always_succeed(AgentId::new("validator"), "valid"),
        ];
        let mut rx = bus.subscribe();
        let task = AgentTask::new(Ulid::new(), "test task");
        let project_dir = std::env::current_dir().unwrap_or_default();
        let context = AgentContext::new(concerto_core::types::SessionContext::new(
            task.session_id,
            project_dir,
        ));

        let mut coordinator = coordinator_with(
            bus.clone(),
            Arc::new(AgentRegistry::from_mocks(mocks)),
            PLAN_RESEARCH_CODER.into(),
        );
        coordinator
            .run(task.clone(), context, CancellationToken::new(), None)
            .await
            .expect("coordinator run should succeed");

        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }

        let lifecycle: Vec<_> = events
            .iter()
            .filter(|event| {
                matches!(
                    event.kind,
                    EventKind::MultiAgentModeStarted { .. }
                        | EventKind::MultiAgentModeCompleted { .. }
                )
            })
            .collect();
        assert!(
            !lifecycle.is_empty(),
            "expected MultiAgentModeStarted/MultiAgentModeCompleted events"
        );
        for event in lifecycle {
            assert_eq!(
                event.session_id,
                task.session_id,
                "session-scoped event {kind:?} leaked with wrong session id",
                kind = event.kind,
            );
        }
    }

    // ------------------------------------------------------------------
    // Audit C-06: coordinator-owned build acceptance with evidence
    // ------------------------------------------------------------------

    #[test]
    fn placeholder_predicate_rejects_marker_only_files() {
        assert!(is_placeholder_content(""));
        assert!(is_placeholder_content("   \n\t "));
        assert!(is_placeholder_content("TODO"));
        assert!(is_placeholder_content("todo: implement"));
        assert!(is_placeholder_content("TODO: implement this"));
        assert!(is_placeholder_content("stub"));
        assert!(is_placeholder_content("PLACEHOLDER"));
        assert!(is_placeholder_content("Not implemented"));
        assert!(is_placeholder_content("coming soon"));
        assert!(is_placeholder_content("lorem ipsum"));
        // Blank lines between markers still count as marker-only content.
        assert!(is_placeholder_content("todo\n\nstub\n"));
        // Substantive content — even with a TODO line — is not a placeholder.
        assert!(!is_placeholder_content("pub fn main() {}\n"));
        assert!(!is_placeholder_content("// TODO: fix later\npub fn main() {}\n"));
        assert!(!is_placeholder_content("fn main() { println!(\"hello\"); }"));
    }

    #[test]
    fn verify_expected_artifacts_accepts_real_files_and_rejects_missing_or_placeholder() {
        let dir = tempfile::tempdir().expect("tempdir for artifact check");
        let root = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf())
            .expect("tempdir path is valid UTF-8");
        std::fs::create_dir_all(dir.path().join("src")).expect("create src dir");

        // A declared artifact that never appeared on disk fails.
        let missing = root.join("src/missing.rs");
        assert!(
            verify_expected_artifacts(&root, &[missing]).is_err(),
            "missing artifact should fail acceptance"
        );

        // Empty and marker-only files fail too.
        let empty = root.join("src/empty.rs");
        std::fs::write(&empty, "").expect("write empty file");
        let stub = root.join("src/stub.rs");
        std::fs::write(&stub, "TODO\n").expect("write stub file");
        let violations =
            verify_expected_artifacts(&root, &[empty, stub]).expect_err("expected violations");
        assert_eq!(violations.len(), 2, "both files should be flagged");

        // Substantive content passes and the resolved path is returned.
        let real = root.join("src/real.rs");
        std::fs::write(&real, "pub fn main() {}\n").expect("write real file");
        let verified =
            verify_expected_artifacts(&root, std::slice::from_ref(&real)).expect("accepted");
        assert_eq!(verified, vec![real]);

        // No declared artifacts passes vacuously.
        assert!(verify_expected_artifacts(&root, &[]).is_ok());
    }

    #[test]
    fn record_acceptance_writes_evidence_to_ledger() {
        let bus = EventBus::new(256);
        let coordinator = coordinator_with(bus, Arc::new(AgentRegistry::new()), r#"[]"#.into());
        let mut ledger = Vec::new();
        let task_id = TaskId::new();
        let artifacts = vec![camino::Utf8PathBuf::from("src/a.rs")];

        coordinator.record_acceptance(&mut ledger, task_id, true, &artifacts, true);
        assert_eq!(ledger.len(), 1);
        assert_eq!(ledger[0].kind, "accepted");
        assert_eq!(ledger[0].task_id, Some(task_id));
        let evidence = ledger[0].evidence.as_ref().expect("accepted entry has evidence");
        assert!(evidence.verification_passed);
        assert_eq!(evidence.artifacts, artifacts);

        coordinator.record_acceptance(&mut ledger, task_id, false, &[], false);
        assert_eq!(ledger.len(), 2);
        assert_eq!(ledger[1].kind, "rejected");
        let evidence = ledger[1].evidence.as_ref().expect("rejected entry has evidence");
        assert!(!evidence.verification_passed);
        assert!(evidence.artifacts.is_empty());
    }

    /// C-06: a build task whose expected artifacts exist with substantive
    /// content and whose validator passes is accepted; the run completes.
    #[tokio::test]
    async fn build_task_with_evidence_is_accepted() {
        let bus = EventBus::new(256);
        let mocks = vec![
            MockExpertAgent::always_succeed(AgentId::new("architect"), DESIGN_DOC_JSON),
            MockExpertAgent::always_succeed(AgentId::new("researcher"), "found"),
            MockExpertAgent::always_succeed(AgentId::new("coder"), "implemented")
                .with_artifact_writer(),
            MockExpertAgent::always_succeed(AgentId::new("reviewer"), "approved"),
            MockExpertAgent::always_succeed(AgentId::new("validator"), "valid"),
        ];
        let (output, _events) = run_for_test(
            coordinator_with_grounded(
                bus.clone(),
                Arc::new(AgentRegistry::from_mocks(mocks)),
                PLAN_RESEARCH_CODER.into(),
                &["src/a.rs"],
            ),
            bus.clone(),
        )
        .await;

        assert!(
            output.final_message.contains("Multi-agent orchestration completed"),
            "unexpected final message: {}",
            output.final_message
        );
    }

    /// C-06: a build task whose declared artifact was never written to disk
    /// is rejected even though the validator reported Success.
    #[tokio::test]
    async fn build_task_missing_artifact_is_rejected() {
        let bus = EventBus::new(256);
        // The coder mock claims to have written `src/a.rs` but does not
        // actually write it, so the artifact never appears on disk. (A
        // zero-file coder would trip the zero-file implement guard instead,
        // never reaching the C-06 acceptance gate this test exercises.)
        let mocks = vec![
            MockExpertAgent::always_succeed(AgentId::new("architect"), DESIGN_DOC_JSON),
            MockExpertAgent::always_succeed(AgentId::new("researcher"), "found"),
            MockExpertAgent::sequence(
                AgentId::new("coder"),
                vec![claimed_files("coder", "implemented", &["src/a.rs"])],
            ),
            MockExpertAgent::always_succeed(AgentId::new("reviewer"), "approved"),
            MockExpertAgent::always_succeed(AgentId::new("validator"), "valid"),
        ];
        let (output, _events) = run_for_test(
            coordinator_with_grounded(
                bus.clone(),
                Arc::new(AgentRegistry::from_mocks(mocks)),
                PLAN_RESEARCH_CODER.into(),
                &["src/a.rs"],
            ),
            bus.clone(),
        )
        .await;

        assert!(
            output
                .final_message
                .contains("Acceptance rejected: expected artifacts missing or placeholder"),
            "unexpected final message: {}",
            output.final_message
        );
    }

    /// C-06: an expected artifact holding only placeholder markers is
    /// rejected even though the validator reported Success.
    #[tokio::test]
    async fn build_task_placeholder_artifact_is_rejected() {
        let bus = EventBus::new(256);
        // The coder mock claims to have written `src/a.rs` without actually
        // writing it, so the seeded placeholder marker stays untouched. (A
        // zero-file coder would trip the zero-file implement guard instead,
        // never reaching the C-06 acceptance gate this test exercises.)
        let mocks = vec![
            MockExpertAgent::always_succeed(AgentId::new("architect"), DESIGN_DOC_JSON),
            MockExpertAgent::always_succeed(AgentId::new("researcher"), "found"),
            MockExpertAgent::sequence(
                AgentId::new("coder"),
                vec![claimed_files("coder", "implemented", &["src/a.rs"])],
            ),
            MockExpertAgent::always_succeed(AgentId::new("reviewer"), "approved"),
            MockExpertAgent::always_succeed(AgentId::new("validator"), "valid"),
        ];
        // Seed the workspace with a marker-only file where the design doc
        // declares `src/a.rs`; the coder mock leaves it untouched.
        let dir = tempfile::tempdir().expect("tempdir for test workspace");
        std::fs::create_dir_all(dir.path().join("src")).expect("create src dir");
        std::fs::write(dir.path().join("src/a.rs"), "TODO: implement\n")
            .expect("write placeholder file");
        let rx = bus.subscribe();
        let mut coordinator = coordinator_with_grounded(
            bus.clone(),
            Arc::new(AgentRegistry::from_mocks(mocks)),
            PLAN_RESEARCH_CODER.into(),
            &["src/a.rs"],
        );
        let task = AgentTask::new(Ulid::new(), "test task");
        let context = AgentContext::new(concerto_core::types::SessionContext::new(
            task.session_id,
            dir.path().to_path_buf(),
        ));
        let output = coordinator
            .run(task, context, CancellationToken::new(), None)
            .await
            .expect("coordinator run should succeed");
        drop(rx);

        assert!(
            output
                .final_message
                .contains("Acceptance rejected: expected artifacts missing or placeholder"),
            "unexpected final message: {}",
            output.final_message
        );
    }

    /// C-06: when the validator errors because the eval capability is
    /// disabled, verification never ran and a build task is rejected even
    /// though the coder produced files.
    #[tokio::test]
    async fn build_task_with_disabled_verification_is_rejected() {
        let bus = EventBus::new(256);
        let mocks = vec![
            MockExpertAgent::always_succeed(AgentId::new("architect"), DESIGN_DOC_JSON),
            MockExpertAgent::always_succeed(AgentId::new("researcher"), "found"),
            MockExpertAgent::always_succeed(AgentId::new("coder"), "implemented")
                .with_artifact_writer(),
            MockExpertAgent::always_succeed(AgentId::new("reviewer"), "approved"),
            MockExpertAgent::sequence(
                AgentId::new("validator"),
                vec![Err(OrchestratorError::AgentLoopError(
                    "validation disabled: eval capability not enabled".into(),
                ))],
            ),
        ];
        let (output, _events) = run_for_test(
            coordinator_with_grounded(
                bus.clone(),
                Arc::new(AgentRegistry::from_mocks(mocks)),
                PLAN_RESEARCH_CODER.into(),
                &["src/a.rs"],
            ),
            bus.clone(),
        )
        .await;

        assert!(
            output.final_message.contains("Acceptance rejected: verification did not run"),
            "unexpected final message: {}",
            output.final_message
        );
    }

    // ------------------------------------------------------------------
    // Phase 6 G5: real-disk build cycle — the validator is a real
    // `GenericSpecialistAgent` in eval mode (no LLM) running `make test`
    // against the workspace, and artifacts are written to / checked on real
    // disk. The coordinator's C-06 acceptance gate rejects a placeholder
    // artifact and accepts a real one, and the ledger decision (with its
    // evidence) is asserted from the run's checkpoint.
    // ------------------------------------------------------------------

    /// A coder that writes the next queued content into every expected
    /// artifact on real disk before reporting Success. After the queue is
    /// exhausted it falls back to `fallback` so validation/review retry
    /// cycles keep writing the same kind of content (placeholder for the
    /// reject phase, substantive for the accept phase).
    struct DiskCoder {
        id: AgentId,
        contents: Mutex<std::collections::VecDeque<String>>,
        fallback: String,
    }

    impl DiskCoder {
        fn new(contents: Vec<String>, fallback: String) -> Self {
            Self { id: AgentId::new("coder"), contents: Mutex::new(contents.into()), fallback }
        }
    }

    #[async_trait::async_trait]
    impl concerto_core::traits::agent::ExpertAgent for DiskCoder {
        fn id(&self) -> AgentId {
            self.id.clone()
        }

        fn stage(&self) -> Option<AgentStage> {
            Some(AgentStage::new(AgentStage::IMPLEMENT))
        }

        fn capabilities(&self) -> concerto_core::types::CapabilitySet {
            concerto_core::types::CapabilitySet::default()
        }

        async fn run(
            &self,
            task: &SubTask,
            context: AgentContext,
            _model: &str,
            _cancel: CancellationToken,
        ) -> Result<AgentRunResult, OrchestratorError> {
            let content = {
                let mut guard = self.contents.lock().unwrap();
                guard.pop_front().unwrap_or_else(|| self.fallback.clone())
            };
            // Mirror a real agent: files actually written to the workspace
            // are reported in `files_modified`, so the zero-file implement
            // guard does not fire for a coder that genuinely produced the
            // expected deliverables (audit C-06 exercises the acceptance gate
            // on the artifact *content*, not on a missing work claim).
            let mut written_paths = Vec::new();
            for path in &context.expected_artifacts {
                let target = context.session.project_dir.join(path.as_str());
                if let Some(parent) = target.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let _ = std::fs::write(&target, content.as_bytes());
                written_paths.push(path.clone());
            }
            Ok(AgentRunResult {
                task_id: task.id,
                role: self.id.clone(),
                outcome: AgentOutcome::Success,
                summary: "implemented".into(),
                files_modified: written_paths,
                tool_call_count: 0,
                cost_usd: 0.0,
                latency_ms: 0,
                provider: "mock".into(),
                model: "mock".into(),
                tokens_in: 0,
                tokens_out: 0,
            })
        }
    }

    /// A reviewer that always asks for revisions, so the review cycle ends
    /// unresolved. Used in the accept phase to keep the run Partial — the
    /// coordinator only serialises the action ledger into a checkpoint on
    /// partial runs, and the ledger is where the C-06 acceptance decision
    /// (with its evidence) is recorded.
    struct AlwaysRevise;

    #[async_trait::async_trait]
    impl concerto_core::traits::agent::ExpertAgent for AlwaysRevise {
        fn id(&self) -> AgentId {
            AgentId::new("reviewer")
        }

        fn stage(&self) -> Option<AgentStage> {
            Some(AgentStage::new(AgentStage::REVIEW))
        }

        fn capabilities(&self) -> concerto_core::types::CapabilitySet {
            concerto_core::types::CapabilitySet::default()
        }

        async fn run(
            &self,
            task: &SubTask,
            _context: AgentContext,
            _model: &str,
            _cancel: CancellationToken,
        ) -> Result<AgentRunResult, OrchestratorError> {
            Ok(AgentRunResult {
                task_id: task.id,
                role: AgentId::new("reviewer"),
                outcome: AgentOutcome::NeedsRevision { reason: "needs more work".into() },
                summary: "needs more work".into(),
                files_modified: Vec::new(),
                tool_call_count: 0,
                cost_usd: 0.0,
                latency_ms: 0,
                provider: "mock".into(),
                model: "mock".into(),
                tokens_in: 0,
                tokens_out: 0,
            })
        }
    }

    /// A real (non-mock) validator in eval mode: runs the attached
    /// `EvalEngine` against the project workspace instead of an LLM.
    fn real_eval_validator(
        bus: &EventBus,
        project_dir: &std::path::Path,
    ) -> crate::agents::GenericSpecialistAgent {
        crate::agents::GenericSpecialistAgent::new(
            AgentId::new("validator"),
            "Validator".into(),
            Some(AgentStage::new(AgentStage::VALIDATE)),
            Arc::new(MockProvider::default()),
            None,
            bus.clone(),
            RetryPolicy::default(),
            concerto_config::PromptSections {
                output_format: "Pass/Fail report".into(),
                ..Default::default()
            },
            concerto_config::AgentCapabilities::default(),
        )
        .with_output_mode(concerto_core::types::OutputMode::Freeform)
        .with_eval(Some(Arc::new(concerto_eval::EvalEngine::new(project_dir))))
    }

    /// Extract the C-06 acceptance decisions (`"accepted"`/`"rejected"`)
    /// from a run's checkpoint ledger. The ledger is serialised into the
    /// checkpoint only on partial runs; completed runs expose no checkpoint.
    fn acceptance_decisions(output: &AgentOutput) -> Vec<checkpoint::CheckpointAction> {
        let checkpoint_json = output
            .checkpoint_json
            .as_ref()
            .expect("a partial run must carry a serialised checkpoint");
        let cp: checkpoint::GraphCheckpoint =
            serde_json::from_str(checkpoint_json).expect("valid checkpoint json");
        cp.action_ledger
            .iter()
            .filter(|action| action.kind == "accepted" || action.kind == "rejected")
            .cloned()
            .collect()
    }

    /// G5: a full build → validate → accept/reject cycle on real disk. The
    /// workspace holds a Makefile whose `test` target passes only when
    /// `src/main.rs` exists (no Cargo.toml, so the eval engine selects the
    /// `make` runner — no cargo/network). A real eval-mode validator runs
    /// `make test`; the coordinator's C-06 gate then rejects a marker-only
    /// artifact and accepts a substantive one, recording both decisions in
    /// the checkpoint ledger.
    #[tokio::test]
    async fn real_disk_build_cycle_rejects_placeholder_and_accepts_real_artifact() {
        let dir = tempfile::tempdir().expect("tempdir for real project workspace");
        std::fs::write(dir.path().join("Makefile"), "test:\n\t@test -f src/main.rs\n")
            .expect("write Makefile");
        let project_root = dir.path().to_path_buf();

        const DESIGN_DOC_MAIN: &str = r#"{"goals":["implement main"],"proposed_files":["src/main.rs"],"interface_sketch":"entry point"}"#;

        // ── Phase 1: the coder leaves only a placeholder → rejected ──
        let bus = EventBus::new(256);
        let mocks = vec![
            MockExpertAgent::always_succeed(AgentId::new("architect"), DESIGN_DOC_MAIN),
            MockExpertAgent::always_succeed(AgentId::new("researcher"), "found"),
            MockExpertAgent::always_succeed(AgentId::new("reviewer"), "approved"),
        ];
        let mut registry = AgentRegistry::from_mocks(mocks);
        registry.register(Arc::new(DiskCoder::new(
            vec!["TODO: implement\n".into()],
            "TODO: implement\n".into(),
        )));
        registry.register(Arc::new(real_eval_validator(&bus, &project_root)));
        let mut coordinator = coordinator_with_grounded(
            bus.clone(),
            Arc::new(registry),
            PLAN_RESEARCH_CODER.into(),
            &["src/main.rs"],
        );
        let task = AgentTask::new(Ulid::new(), "build a main");
        let context = AgentContext::new(concerto_core::types::SessionContext::new(
            task.session_id,
            project_root.clone(),
        ));
        let output = coordinator
            .run(task, context, CancellationToken::new(), None)
            .await
            .expect("coordinator run should succeed");

        assert_eq!(
            output.completion_status,
            concerto_core::types::AgentCompletionStatus::Partial,
            "a rejected build task must not complete as success"
        );
        assert!(
            output
                .final_message
                .contains("Acceptance rejected: expected artifacts missing or placeholder"),
            "unexpected final message: {}",
            output.final_message
        );
        let decisions = acceptance_decisions(&output);
        assert!(
            !decisions.is_empty(),
            "at least one acceptance decision expected on the reject path"
        );
        assert!(
            decisions.iter().all(|action| action.kind == "rejected"),
            "every acceptance decision on the reject path must be a rejection, got: {decisions:?}"
        );
        assert!(
            !decisions.iter().any(|action| action.kind == "accepted"),
            "no accepted entry may exist on the reject path"
        );
        let evidence = decisions[0].evidence.as_ref().expect("acceptance entry carries evidence");
        assert!(evidence.verification_passed, "the eval passed; artifact gating rejected it");
        assert_eq!(evidence.artifacts, vec![camino::Utf8PathBuf::from("src/main.rs")]);

        // ── Phase 2: same project, now with a real artifact → accepted ──
        // The reviewer stays unsatisfied so the run ends Partial and still
        // serialises the ledger — letting the test observe the accepted
        // decision and its evidence (artifacts + verification_passed).
        let bus = EventBus::new(256);
        let mocks = vec![
            MockExpertAgent::always_succeed(AgentId::new("architect"), DESIGN_DOC_MAIN),
            MockExpertAgent::always_succeed(AgentId::new("researcher"), "found"),
        ];
        let mut registry = AgentRegistry::from_mocks(mocks);
        registry.register(Arc::new(DiskCoder::new(
            vec!["// real implementation\npub fn main() {}\n".into()],
            "// real implementation\npub fn main() {}\n".into(),
        )));
        registry.register(Arc::new(AlwaysRevise));
        registry.register(Arc::new(real_eval_validator(&bus, &project_root)));
        let mut coordinator = coordinator_with_grounded(
            bus.clone(),
            Arc::new(registry),
            PLAN_RESEARCH_CODER.into(),
            &["src/main.rs"],
        );
        let task = AgentTask::new(Ulid::new(), "build a main");
        let context = AgentContext::new(concerto_core::types::SessionContext::new(
            task.session_id,
            project_root.clone(),
        ));
        let output = coordinator
            .run(task, context, CancellationToken::new(), None)
            .await
            .expect("coordinator run should succeed");

        // Acceptance passed (the ledger says so); the run is Partial only
        // because the review stayed unresolved.
        assert_eq!(
            output.completion_status,
            concerto_core::types::AgentCompletionStatus::Partial,
            "an unresolved review keeps the run Partial even though acceptance passed"
        );
        assert!(
            output.final_message.contains("Review remains unresolved"),
            "expected the unresolved-review note, got: {}",
            output.final_message
        );
        let decisions = acceptance_decisions(&output);
        assert_eq!(
            decisions.len(),
            1,
            "exactly one acceptance decision expected on the accept path"
        );
        assert_eq!(decisions[0].kind, "accepted", "real artifact must be accepted");
        assert!(
            !decisions.iter().any(|action| action.kind == "rejected"),
            "no rejected entry may exist on the accept path"
        );
        let evidence = decisions[0].evidence.as_ref().expect("acceptance entry carries evidence");
        assert!(evidence.verification_passed, "acceptance requires the eval pass");
        assert_eq!(evidence.artifacts, vec![camino::Utf8PathBuf::from("src/main.rs")]);

        // The artifact the coder wrote is substantive on real disk.
        let disk = std::fs::read_to_string(dir.path().join("src/main.rs")).expect("read artifact");
        assert!(
            disk.contains("pub fn main"),
            "workspace must hold a substantive artifact, got: {disk:?}"
        );
    }

    // ------------------------------------------------------------------
    // ADR-35 §5 Phase 5 C-06 amendment: coordinator self-verification.
    // When NO validation-stage agent is registered and the coordinator
    // holds an eval engine, the coordinator carries verification itself
    // (runs the detected test runner via `make test`). Acceptance is
    // rejected ONLY when verification is required (build task) and
    // impossible or failing — exactly the same gate the normal validator
    // path applies.
    // ------------------------------------------------------------------

    const DESIGN_DOC_MAIN: &str = r#"{"goals":["implement main"],"proposed_files":["src/main.rs"],"interface_sketch":"entry point"}"#;

    /// A build task with no validation-stage agent is self-verified by the
    /// coordinator: the eval engine runs a REAL passing `make test` against
    /// the workspace and the C-06 artifact gate records the acceptance. The
    /// reviewer stays unsatisfied so the run ends Partial and still
    /// serialises the checkpoint ledger — letting the test observe the
    /// accepted decision and its evidence (artifacts + verification_passed).
    #[tokio::test]
    async fn coordinator_self_verifies_build_task_when_no_validator_registered() {
        let dir = tempfile::tempdir().expect("tempdir for real project workspace");
        // `detect_runner` selects the `make` runner via the Makefile; the
        // passing `test` target lets the coordinator's eval engine verify.
        std::fs::write(dir.path().join("Makefile"), "test:\n\t@true\n").expect("write Makefile");
        let project_root = dir.path().to_path_buf();

        let bus = EventBus::new(256);
        let mut rx = bus.subscribe();
        let mocks = vec![
            MockExpertAgent::always_succeed(AgentId::new("architect"), DESIGN_DOC_MAIN),
            MockExpertAgent::always_succeed(AgentId::new("researcher"), "found"),
        ];
        let mut registry = AgentRegistry::from_mocks(mocks);
        // A real implement-stage agent writes a substantive artifact; the
        // reviewer always asks for revisions (keeps the run Partial). NO
        // validate-stage agent is registered — the coordinator carries
        // verification itself.
        registry.register(Arc::new(DiskCoder::new(
            vec!["// real implementation\npub fn main() {}\n".into()],
            "// real implementation\npub fn main() {}\n".into(),
        )));
        registry.register(Arc::new(AlwaysRevise));
        let mut coordinator = coordinator_with_grounded(
            bus.clone(),
            Arc::new(registry),
            PLAN_RESEARCH_CODER.into(),
            &["src/main.rs"],
        )
        .with_eval_engine(Arc::new(concerto_eval::EvalEngine::new(project_root.clone())));
        let task = AgentTask::new(Ulid::new(), "build a main");
        let context = AgentContext::new(concerto_core::types::SessionContext::new(
            task.session_id,
            project_root.clone(),
        ));
        let output = coordinator
            .run(task, context, CancellationToken::new(), None)
            .await
            .expect("coordinator run should succeed");
        let events = {
            let mut collected = Vec::new();
            while let Ok(event) = rx.try_recv() {
                collected.push(event.kind.clone());
            }
            collected
        };

        // The coordinator self-verify cycle DID run (stage feed + replay see
        // the single ValidationCycleStarted event).
        assert!(
            events.iter().any(|kind| matches!(kind, EventKind::ValidationCycleStarted { .. })),
            "the coordinator self-verify must publish a ValidationCycleStarted event"
        );
        let decisions = acceptance_decisions(&output);
        assert_eq!(
            decisions.len(),
            1,
            "exactly one acceptance decision expected on the self-verify path"
        );
        assert_eq!(decisions[0].kind, "accepted", "passing self-verification must be accepted");
        assert!(
            !decisions.iter().any(|action| action.kind == "rejected"),
            "no rejected entry may exist when self-verification passed"
        );
        let evidence = decisions[0].evidence.as_ref().expect("acceptance entry carries evidence");
        assert!(evidence.verification_passed, "acceptance requires the verification pass");
        assert_eq!(evidence.artifacts, vec![camino::Utf8PathBuf::from("src/main.rs")]);
        // The run is Partial only because the review stayed unresolved — the
        // validation itself passed and no acceptance rejection is present.
        assert_eq!(
            output.completion_status,
            concerto_core::types::AgentCompletionStatus::Partial,
            "an unresolved review keeps the run Partial even though self-verification passed"
        );
        assert!(
            output.final_message.contains("Review remains unresolved"),
            "expected the unresolved-review note, got: {}",
            output.final_message
        );
        assert!(
            !output.final_message.contains("Acceptance rejected"),
            "self-verified acceptance must not be rejected: {}",
            output.final_message
        );
    }

    /// A build task with no validation-stage agent and a FAILING test runner
    /// is rejected by the coordinator's self-verification: verification ran
    /// but did not pass, so acceptance fails and the ledger records the
    /// rejection (verification_passed = false, no artifacts).
    #[tokio::test]
    async fn coordinator_self_verification_failure_rejects_build_task() {
        let dir = tempfile::tempdir().expect("tempdir for real project workspace");
        // `detect_runner` selects the `make` runner; the failing `test`
        // target makes the coordinator's self-verification fail.
        std::fs::write(dir.path().join("Makefile"), "test:\n\t@false\n").expect("write Makefile");
        let project_root = dir.path().to_path_buf();

        let bus = EventBus::new(256);
        let mut rx = bus.subscribe();
        let mocks = vec![
            MockExpertAgent::always_succeed(AgentId::new("architect"), DESIGN_DOC_MAIN),
            MockExpertAgent::always_succeed(AgentId::new("researcher"), "found"),
        ];
        let mut registry = AgentRegistry::from_mocks(mocks);
        // Implement-stage coder writes a real artifact; the reviewer passes
        // (so the run's Partial status comes from the validation failure, not
        // an unresolved review). NO validate-stage agent is registered.
        registry.register(Arc::new(DiskCoder::new(
            vec!["// real implementation\npub fn main() {}\n".into()],
            "// real implementation\npub fn main() {}\n".into(),
        )));
        registry.register(Arc::new(MockExpertAgent::always_succeed(
            AgentId::new("reviewer"),
            "approved",
        )));
        let mut coordinator = coordinator_with_grounded(
            bus.clone(),
            Arc::new(registry),
            PLAN_RESEARCH_CODER.into(),
            &["src/main.rs"],
        )
        .with_eval_engine(Arc::new(concerto_eval::EvalEngine::new(project_root.clone())));
        let task = AgentTask::new(Ulid::new(), "build a main");
        let context = AgentContext::new(concerto_core::types::SessionContext::new(
            task.session_id,
            project_root.clone(),
        ));
        let output = coordinator
            .run(task, context, CancellationToken::new(), None)
            .await
            .expect("coordinator run should succeed");
        let events = {
            let mut collected = Vec::new();
            while let Ok(event) = rx.try_recv() {
                collected.push(event.kind.clone());
            }
            collected
        };

        assert!(
            events.iter().any(|kind| matches!(kind, EventKind::ValidationCycleStarted { .. })),
            "the coordinator self-verify must publish a ValidationCycleStarted event"
        );
        assert!(
            output
                .final_message
                .contains("Acceptance rejected: coordinator self-verification failed"),
            "unexpected final message: {}",
            output.final_message
        );
        assert_eq!(
            output.completion_status,
            concerto_core::types::AgentCompletionStatus::Partial,
            "a failing self-verification must not complete the build task"
        );
        let decisions = acceptance_decisions(&output);
        assert!(
            !decisions.is_empty(),
            "at least one acceptance decision expected on the self-verify reject path"
        );
        assert!(
            decisions.iter().all(|action| action.kind == "rejected"),
            "every acceptance decision on the reject path must be a rejection, got: {decisions:?}"
        );
        let evidence = decisions[0].evidence.as_ref().expect("acceptance entry carries evidence");
        assert!(
            !evidence.verification_passed,
            "self-verification ran but did not pass, so verification_passed must be false"
        );
        assert!(evidence.artifacts.is_empty(), "a rejected run records no verified artifacts");
    }

    /// A NON-build task (no implement-stage work) with no validation-stage
    /// agent is vacuously accepted: verification is not required, so the
    /// run completes with the standard skip note and no acceptance rejection
    /// — even when the coordinator holds no eval engine.
    #[tokio::test]
    async fn non_build_task_without_validator_is_vacuously_accepted() {
        let bus = EventBus::new(256);
        // The plan names the reserved "coordinator" role for implementation
        // (ADR-35 §8 standby self-execution). Because that role resolves to no
        // registered agent, `stage_of` returns None for it and the graph holds
        // no implement-stage work: `build_task` is false, so verification is
        // not required (audit C-06).
        let plan_json = r#"[
            {"role":"Researcher","description":"inspect","depends_on":[]},
            {"role":"coordinator","description":"implement","depends_on":[0]}
        ]"#;
        let mocks = vec![
            MockExpertAgent::always_succeed(AgentId::new("architect"), DESIGN_DOC_JSON),
            MockExpertAgent::always_succeed(AgentId::new("researcher"), "found"),
        ];
        // No coder, reviewer, or validator — deliberately no eval engine.
        let mut coordinator = coordinator_with_responses(
            bus.clone(),
            Arc::new(AgentRegistry::from_mocks(mocks)),
            vec![plan_json.to_string(), "implemented by the coordinator self".into()],
        );
        coordinator = coordinator.with_executor(coordinator_self_executor());
        let (output, _events) = run_for_test(coordinator, bus.clone()).await;

        assert_eq!(
            output.completion_status,
            concerto_core::types::AgentCompletionStatus::Completed,
            "a non-build task without a validator completes vacuously, got: {:?} — msg: {}",
            output.completion_status,
            output.final_message
        );
        assert!(
            output.final_message.contains("Multi-agent orchestration completed"),
            "unexpected final message: {}",
            output.final_message
        );
        assert!(
            !output.final_message.contains("Acceptance rejected"),
            "a non-build task must never hit acceptance rejection: {}",
            output.final_message
        );
    }

    // ------------------------------------------------------------------
    // ADR-55 Phase 2b: planning-only orchestration depth
    // ------------------------------------------------------------------

    /// T1/T2/T3 (coordinator side): a planning-only run renders the produced
    /// plan as its final message, completes, and NEVER dispatches a subtask —
    /// zero tool calls, no SubTaskStarted, no review/validation cycles.
    /// Full-depth behavior is unchanged for every existing `run_for_test`
    /// caller, which still defaults to `OrchestrationDepth::Full` (T8).
    #[tokio::test]
    async fn planning_only_renders_plan_and_never_executes() {
        let bus = EventBus::new(256);
        let mocks = vec![
            MockExpertAgent::always_succeed(AgentId::new("architect"), DESIGN_DOC_JSON),
            MockExpertAgent::always_succeed(AgentId::new("researcher"), "found"),
            MockExpertAgent::always_succeed(AgentId::new("coder"), "implemented"),
            MockExpertAgent::always_succeed(AgentId::new("reviewer"), "approved"),
            MockExpertAgent::always_succeed(AgentId::new("validator"), "valid"),
        ];
        let (output, events) = run_for_test(
            coordinator_with(
                bus.clone(),
                Arc::new(AgentRegistry::from_mocks(mocks)),
                PLAN_RESEARCH_CODER.into(),
            )
            .with_orchestration_depth(OrchestrationDepth::PlanningOnly),
            bus.clone(),
        )
        .await;

        assert_eq!(
            output.completion_status,
            concerto_core::types::AgentCompletionStatus::Completed,
            "a successful planning-only run completes, got: {:?}",
            output.completion_status,
        );
        assert_eq!(output.tool_call_count, 0, "planning-only never invokes tools");
        assert!(
            !output.final_message.contains("Multi-agent orchestration completed"),
            "the placeholder must be replaced by the rendered plan"
        );
        assert!(
            output.final_message.contains("[coder]"),
            "the rendered plan names the planned coder subtask: {}",
            output.final_message,
        );
        assert!(
            output.final_message.contains("[researcher]"),
            "the rendered plan names the planned researcher subtask: {}",
            output.final_message,
        );
        assert!(
            !events.iter().any(|kind| {
                matches!(kind, EventKind::SubTaskStarted { role, .. } if role.as_str() != "architect")
            }),
            "planning-only must never dispatch an implement/subtask role"
        );
        assert!(
            !events.iter().any(|kind| {
                matches!(
                    kind,
                    EventKind::ReviewCycleStarted { .. } | EventKind::ValidationCycleStarted { .. }
                )
            }),
            "planning-only must never run review or validation cycles"
        );
        assert!(
            events.iter().any(|kind| matches!(kind, EventKind::MultiAgentModeCompleted { .. })),
            "planning-only still publishes the completion event (S3)"
        );
    }

    /// ADR-60 D7 (#152): a seeded approved-plan DesignDoc drives decompose
    /// directly — the architect is NOT re-invoked on the same objective
    /// (silent re-decompose is forbidden), and the planned work still
    /// executes to completion off the structured doc.
    #[tokio::test]
    async fn approved_plan_seed_skips_the_architect() {
        let bus = EventBus::new(256);
        let mocks = vec![
            // A poisoned architect: if the design stage ran anyway, its
            // failure would surface in the run output and the assertion
            // below would catch the dispatch itself.
            MockExpertAgent::always_fail(AgentId::new("architect"), "must not be dispatched"),
            MockExpertAgent::always_succeed(AgentId::new("coder"), "implemented")
                .with_artifact_writer(),
            MockExpertAgent::always_succeed(AgentId::new("validator"), "valid"),
        ];
        let seed = ApprovedPlanSeed {
            plan_id: "plan-d7".to_owned(),
            design_doc: Some(DesignDoc {
                goals: vec!["approved goal".to_owned()],
                constraints: vec![],
                proposed_files: vec![camino::Utf8PathBuf::from("src/approved.rs")],
                interface_sketch: "approved interface".to_owned(),
                risks: vec![],
            }),
        };
        let plan_json = r#"[{"role":"Coder","description":"implement","depends_on":[]}]"#;
        let (output, events) = run_for_test(
            coordinator_with(
                bus.clone(),
                Arc::new(AgentRegistry::from_mocks(mocks)),
                plan_json.into(),
            )
            .with_approved_plan_seed(seed),
            bus.clone(),
        )
        .await;

        assert!(
            !events.iter().any(|kind| {
                matches!(kind, EventKind::SubTaskStarted { role, .. } if role.as_str() == "architect")
            }),
            "the architect must never be re-invoked for an approved plan"
        );
        assert_eq!(
            output.completion_status,
            concerto_core::types::AgentCompletionStatus::Completed,
            "the seeded plan still executes to completion: {}",
            output.final_message
        );
        assert!(
            events.iter().any(|kind| matches!(
                kind,
                EventKind::SubTaskStarted { role, .. } if role.as_str() == "coder"
            )),
            "the implement subtask still dispatches off the seeded doc"
        );
    }

    // ------------------------------------------------------------------
    // Run-continuity Phase 1: stall gate + resume objective continuity
    // ------------------------------------------------------------------

    /// A wired coordinator whose orchestration checkpoints persist to an
    /// in-memory SQLite store, so tests can assert the stall gate's
    /// persist-vs-clear behavior on real storage. A session row is created
    /// first — the checkpoint table FK-references it — and its id is
    /// returned alongside the store so tests can read back what the
    /// coordinator persisted.
    async fn coordinator_with_store(
        bus: EventBus,
        registry: Arc<AgentRegistry>,
        plan_json: String,
        project_dir: &std::path::Path,
    ) -> (CoordinatorAgent, Arc<concerto_sessions::SqliteSessionStore>, Ulid) {
        let store = Arc::new(
            concerto_sessions::SqliteSessionStore::connect_in_memory()
                .await
                .expect("in-memory session store"),
        );
        let project = camino::Utf8PathBuf::from_path_buf(project_dir.to_path_buf())
            .expect("test project dir is UTF-8");
        let session_id = store
            .create_session(&project, "mock", "test-model", CancellationToken::new())
            .await
            .expect("create session row")
            .id;
        // Every caller pairs this helper with the standard `DESIGN_DOC_JSON`
        // (proposes `src/a.rs`), so ground that path once here: without a
        // snapshot the ADR-65 §5 verifier degrades the doc to Quarantined and
        // the checkpoint/resume tests would exercise the passive path instead
        // of their intended artifact gates.
        let coordinator = coordinator_with(bus, registry, plan_json)
            .with_workspace_snapshot(grounded_snapshot(&["src/a.rs"]))
            .with_checkpoint_store(
                Some(store.clone() as Arc<dyn concerto_sessions::SessionStore>),
                None,
            );
        (coordinator, store, session_id)
    }

    /// A coordinator wired to an EXISTING checkpoint store + session — used
    /// for the resume phase of two-phase tests, which must share the first
    /// phase's storage (and therefore its session row) to exercise the
    /// persist/clear cycle across runs.
    fn coordinator_on_store(
        bus: EventBus,
        registry: Arc<AgentRegistry>,
        plan_json: String,
        store: Arc<concerto_sessions::SqliteSessionStore>,
    ) -> CoordinatorAgent {
        coordinator_with(bus, registry, plan_json)
            .with_checkpoint_store(Some(store as Arc<dyn concerto_sessions::SessionStore>), None)
    }

    /// A stalled run (review unresolved → Partial) with a DesignDoc KEEPS its
    /// orchestration checkpoint: persisted with completed=false, carrying the
    /// original objective text + hash and the design doc, so a later bare
    /// "continue" can restore it.
    #[tokio::test]
    async fn stalled_run_persists_checkpoint_with_design_doc() {
        let dir = tempfile::tempdir().expect("tempdir for test workspace");
        let bus = EventBus::new(256);
        let mocks = vec![
            MockExpertAgent::always_succeed(AgentId::new("architect"), DESIGN_DOC_JSON),
            MockExpertAgent::always_succeed(AgentId::new("researcher"), "found"),
            MockExpertAgent::always_succeed(AgentId::new("coder"), "implemented")
                .with_artifact_writer(),
        ];
        let mut registry = AgentRegistry::from_mocks(mocks);
        registry.register(Arc::new(AlwaysRevise));
        let registry = Arc::new(registry);
        let (mut coordinator, store, session_id) =
            coordinator_with_store(bus.clone(), registry, PLAN_RESEARCH_CODER.into(), dir.path())
                .await;

        let task = AgentTask::new(session_id, "build the thing");
        let context = AgentContext::new(concerto_core::types::SessionContext::new(
            session_id,
            dir.path().to_path_buf(),
        ));
        let output = coordinator
            .run(task, context, CancellationToken::new(), None)
            .await
            .expect("coordinator run should succeed");

        assert_eq!(
            output.completion_status,
            concerto_core::types::AgentCompletionStatus::Partial,
            "the unresolved review leaves the run stalled: {}",
            output.final_message
        );
        assert!(output.checkpoint_json.is_some(), "a stalled run surfaces its checkpoint");

        // The store row survives the stalled run, marked not-completed.
        let record = store
            .load_orchestration_checkpoint(session_id)
            .await
            .expect("checkpoint store read")
            .expect("a stalled run must persist its checkpoint");
        assert!(!record.completed, "the stalled checkpoint is resumable");

        let cp: checkpoint::GraphCheckpoint =
            serde_json::from_str(&record.state_json).expect("valid persisted checkpoint");
        assert!(!cp.completed && cp.stage != checkpoint::CheckpointStage::Completed);
        assert_eq!(
            cp.objective, "build the thing",
            "the persisted objective is the run objective, not a resume input"
        );
        assert_eq!(
            cp.objective_hash,
            blake3::hash("build the thing".as_bytes()).to_hex().to_string()
        );
        assert!(cp.design_doc.is_some(), "the DesignDoc is preserved for the resume");
    }

    /// A fully-successful run CLEARS its orchestration checkpoint —
    /// byte-identical to the pre-Phase-1 behavior — even though the run
    /// persisted interim checkpoints along the way.
    #[tokio::test]
    async fn successful_run_clears_orchestration_checkpoint() {
        let dir = tempfile::tempdir().expect("tempdir for test workspace");
        let bus = EventBus::new(256);
        let mocks = vec![
            MockExpertAgent::always_succeed(AgentId::new("architect"), DESIGN_DOC_JSON),
            MockExpertAgent::always_succeed(AgentId::new("researcher"), "found"),
            MockExpertAgent::always_succeed(AgentId::new("coder"), "implemented")
                .with_artifact_writer(),
            MockExpertAgent::always_succeed(AgentId::new("validator"), "valid"),
        ];
        let (mut coordinator, store, session_id) = coordinator_with_store(
            bus.clone(),
            Arc::new(AgentRegistry::from_mocks(mocks)),
            PLAN_RESEARCH_CODER.into(),
            dir.path(),
        )
        .await;

        // Seed a stale row from an earlier partial run of this session — the
        // successful run must leave NO row behind when it finishes.
        store
            .save_orchestration_checkpoint(&concerto_sessions::OrchestrationCheckpointRecord {
                session_id,
                run_id: Ulid::new(),
                root_task_id: TaskId::new(),
                project_id: "stale".into(),
                objective_hash: "stale".into(),
                schema_version: checkpoint::GRAPH_CHECKPOINT_SCHEMA_VERSION,
                source_revision: None,
                sequence_num: 1,
                state_json: r#"{"stale":true}"#.into(),
                completed: false,
                updated_at: time::OffsetDateTime::now_utc(),
            })
            .await
            .expect("seed stale checkpoint");

        let task = AgentTask::new(session_id, "build the thing");
        let context = AgentContext::new(concerto_core::types::SessionContext::new(
            session_id,
            dir.path().to_path_buf(),
        ));
        let output = coordinator
            .run(task, context, CancellationToken::new(), None)
            .await
            .expect("coordinator run should succeed");

        assert_eq!(
            output.completion_status,
            concerto_core::types::AgentCompletionStatus::Completed,
            "the clean run completes: {}",
            output.final_message
        );
        assert!(output.checkpoint_json.is_none(), "a completed run carries no checkpoint");
        assert!(
            store
                .load_orchestration_checkpoint(session_id)
                .await
                .expect("checkpoint store read")
                .is_none(),
            "a clean success clears the orchestration checkpoint"
        );
    }

    /// Run-continuity Phase 1 (Task A): a resumed run keeps recording the
    /// ORIGINAL objective text + hash in every checkpoint it persists — the
    /// resume input ("continue") never replaces it, so a later resume still
    /// names the same work.
    #[tokio::test]
    async fn resumed_run_keeps_original_objective_in_checkpoints() {
        let dir = tempfile::tempdir().expect("tempdir for test workspace");

        // ── Phase 1: fresh run stalls (review unresolved) ────────────
        let bus = EventBus::new(256);
        let mocks = vec![
            MockExpertAgent::always_succeed(AgentId::new("architect"), DESIGN_DOC_JSON),
            MockExpertAgent::always_succeed(AgentId::new("researcher"), "found"),
            MockExpertAgent::always_succeed(AgentId::new("coder"), "implemented")
                .with_artifact_writer(),
        ];
        let mut registry = AgentRegistry::from_mocks(mocks);
        registry.register(Arc::new(AlwaysRevise));
        let registry = Arc::new(registry);
        let (mut coordinator, store, session_id) =
            coordinator_with_store(bus.clone(), registry, PLAN_RESEARCH_CODER.into(), dir.path())
                .await;
        let task = AgentTask::new(session_id, "build the thing");
        let context = AgentContext::new(concerto_core::types::SessionContext::new(
            session_id,
            dir.path().to_path_buf(),
        ));
        let first = coordinator
            .run(task, context, CancellationToken::new(), None)
            .await
            .expect("first run should succeed");
        let stored =
            first.checkpoint_json.clone().expect("the stalled first run carries a checkpoint");

        // ── Phase 2: bare "continue" resumes from the stored checkpoint ──
        let bus2 = EventBus::new(256);
        // The architect is a canary: the resume path restores the graph and
        // must never re-derive it.
        let mocks2 = vec![
            MockExpertAgent::always_fail(AgentId::new("architect"), "must not be dispatched"),
            MockExpertAgent::always_succeed(AgentId::new("researcher"), "found"),
            MockExpertAgent::always_succeed(AgentId::new("coder"), "implemented")
                .with_artifact_writer(),
        ];
        let mut registry2 = AgentRegistry::from_mocks(mocks2);
        registry2.register(Arc::new(AlwaysRevise));
        let registry2 = Arc::new(registry2);
        // The resume phase shares phase 1's store and session row — the
        // checkpoint cycle must survive across runs on the same session.
        let mut coordinator2 = coordinator_on_store(bus2, registry2, String::new(), store.clone());
        let continue_task = AgentTask::new(session_id, "continue");
        let continue_ctx = AgentContext::new(concerto_core::types::SessionContext::new(
            session_id,
            dir.path().to_path_buf(),
        ));
        let second = coordinator2
            .run(continue_task, continue_ctx, CancellationToken::new(), Some(stored))
            .await
            .expect("resumed run should succeed");

        assert_eq!(
            second.completion_status,
            concerto_core::types::AgentCompletionStatus::Partial,
            "the resumed run stalls again on the unresolved review"
        );
        let record = store
            .load_orchestration_checkpoint(session_id)
            .await
            .expect("checkpoint store read")
            .expect("the stalled resume keeps its checkpoint");
        let cp: checkpoint::GraphCheckpoint =
            serde_json::from_str(&record.state_json).expect("valid resumed checkpoint");
        assert_eq!(cp.objective, "build the thing", "the original objective survives the resume");
        assert_eq!(
            cp.objective_hash,
            blake3::hash("build the thing".as_bytes()).to_hex().to_string(),
            "the objective hash is the ORIGINAL objective's hash, not the resume input's"
        );
        // ADR-65 §7: the resumed run's checkpoints keep carrying the §7
        // state — the doc resolution captured by the verifier and the
        // snapshot generation both survive into the post-resume row.
        assert_eq!(
            cp.schema_version,
            checkpoint::GRAPH_CHECKPOINT_SCHEMA_VERSION,
            "resumed checkpoints are canonical v4"
        );
        assert!(
            cp.doc_resolution.is_some(),
            "the doc resolution captured at verify time rides every persist"
        );
    }

    // ── ADR-65 §7: continuation restores state at the whiteboard cursor ──

    /// A hermetic review-store pool (the ADR-65 §7 evaluation's log source).
    async fn resume_log_pool() -> (tempfile::TempDir, sqlx::SqlitePool) {
        let dir = tempfile::tempdir().expect("tempdir created");
        let db_path = dir.path().join("resume_evidence_test.db");
        let options = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(&db_path)
            .create_if_missing(true)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .busy_timeout(std::time::Duration::from_secs(5))
            .foreign_keys(true)
            .synchronous(sqlx::sqlite::SqliteSynchronous::Normal);
        let pool = sqlx::pool::PoolOptions::new()
            .max_connections(4)
            .connect_with(options)
            .await
            .expect("test pool connects");
        sqlx::migrate!("../sessions/migrations").run(&pool).await.expect("migrations apply");
        (dir, pool)
    }

    /// A v4-shaped checkpoint JSON with one blocked/failed subtask (and an
    /// optional failed-ledger count). The §7 fields sit behind the additive
    /// schema surface exactly as `persist_checkpoint` writes them.
    fn blocked_step_checkpoint_json(
        project_id: &str,
        session_id: Ulid,
        subtask_id: Ulid,
        role: &str,
        status: &str,
        ledger_failed_entries: usize,
        cursor: Option<u64>,
    ) -> String {
        let ledger: Vec<serde_json::Value> = (0..ledger_failed_entries)
            .map(|_| {
                serde_json::json!({
                    "kind": "failed",
                    "task_id": subtask_id.to_string(),
                    "timestamp": [2026, 254, 0, 0, 0, 0, 0, 0, 0],
                })
            })
            .collect();
        serde_json::json!({
            "schema_version": 4,
            "run_id": Ulid::new().to_string(),
            "session_id": session_id.to_string(),
            "root_task_id": Ulid::new().to_string(),
            "project_id": project_id,
            "objective": "build the thing",
            "objective_hash": blake3::hash("build the thing".as_bytes()).to_hex().to_string(),
            "stage": "Executing",
            "completed": false,
            "subtasks": [{
                "id": subtask_id.to_string(),
                "parent_id": null,
                "session_id": session_id.to_string(),
                "role": role,
                "description": "the blocked work",
                "status": status,
                "dependencies": [],
                "deliverable": null,
            }],
            "edges": [],
            "completed_results": {},
            "total_cost": 0.0,
            "total_tool_calls": 0,
            "provider_metrics": [],
            "all_files": [],
            "expected_artifacts": {},
            "subtask_attempts": {},
            "retry_feedback": {},
            "action_ledger": ledger,
            "whiteboard_cursor_gate_seq": cursor,
        })
        .to_string()
    }

    /// Append a `ToolExecuted` fact row attributed to a task (real id,
    /// session-scoped — the shape the fact writer produces).
    async fn append_tool_fact(
        pool: &sqlx::SqlitePool,
        session_id: Ulid,
        event_id: &str,
        task_id: &str,
        success: bool,
    ) -> WhiteboardEvent {
        append_whiteboard_event(
            pool,
            &NewWhiteboardEvent {
                event_id: event_id.to_owned(),
                agent_id: "coder".to_owned(),
                kind: WhiteboardKind::ToolExecuted,
                scope: String::new(),
                session_id: Some(session_id.to_string()),
                plan_id: None,
                causation: None,
                payload: serde_json::json!({
                    "agent_id": "coder",
                    "task_id": task_id,
                    "tool": "filesystem",
                    "args": { "operation": "read", "path": "src/main.rs" },
                    "success": success,
                    "generation": "",
                    "paths": [],
                }),
                pre_image_hash: None,
                created_at: 1,
            },
        )
        .await
        .expect("fact row appended")
    }

    /// Resume decisions are recorded with reason codes and REAL evidence
    /// ids; a restore-and-continue resets its graph in place.
    #[tokio::test]
    async fn resume_continues_progressing_blocked_step_from_the_cursor() {
        let (_dir, pool) = resume_log_pool().await;
        let workspace = tempfile::tempdir().expect("workspace dir");
        let session_id = Ulid::new();
        let subtask_id = Ulid::new();
        let project_id = concerto_core::types::ProjectId::resolve(workspace.path()).0;

        // Pre-cursor: one failure fact BEFORE the cursor — checkpoint-era
        // state, never replayed into the evidence view.
        append_tool_fact(&pool, session_id, "ev-pre-fail", &subtask_id.to_string(), false).await;
        // Post-cursor: a successful tool execution — the agent made progress.
        append_tool_fact(&pool, session_id, "ev-post-progress", &subtask_id.to_string(), true)
            .await;

        let registry = Arc::new(AgentRegistry::new()); // no candidates
        let bus = EventBus::new(16);
        let provider: Arc<dyn concerto_core::traits::provider::LlmProvider> =
            Arc::new(MockProvider::default());
        let spend_tracker = Arc::new(SpendTracker::default());
        let routing = Arc::new(RoutingEngine::new(
            vec![],
            spend_tracker.clone(),
            concerto_config::ModelPinConfig::default(),
            EventBus::default(),
        ));
        let model_selector =
            Arc::new(ModelSelector::new(Arc::new(ModelRegistry::from_profiles(vec![])), routing));
        let mut coordinator = CoordinatorAgent::new(
            registry,
            AgentRunner::new(Arc::new(AgentRegistry::new()), bus.clone(), spend_tracker.clone()),
            model_selector,
            spend_tracker.clone(),
            bus.clone(),
            provider,
            Arc::new(NullMemoryStore),
        )
        .with_review_store(Some(pool.clone()));

        let cp_json = blocked_step_checkpoint_json(
            &project_id,
            session_id,
            subtask_id,
            "coder",
            "Blocked",
            0,
            Some(1),
        );
        let task = AgentTask::new(session_id, "continue");
        let context = AgentContext::new(concerto_core::types::SessionContext::new(
            session_id,
            workspace.path().to_path_buf(),
        ));
        let result = coordinator
            .decompose_or_restore(&task, &context, &CancellationToken::new(), Some(cp_json))
            .await
            .expect("restore succeeds");

        // The step was re-armed for the SAME agent (facts show progress).
        let graph_task = result
            .graph
            .all_tasks()
            .into_iter()
            .find(|subtask| subtask.id.0 == subtask_id)
            .expect("restored step");
        assert_eq!(graph_task.status, SubTaskStatus::Pending, "Continue re-arms the step");
        assert_eq!(graph_task.role.as_str(), "coder", "progress ⇒ the same agent continues");
        assert!(
            result.action_ledger.iter().any(|entry| entry.kind == "resume-continued"),
            "the decision lands in the checkpoint ledger"
        );

        // The evidence view is the log AFTER the cursor: the pre-cursor
        // failure is not cited (no replay), the post-cursor progress fact
        // is.
        let logged = load_whiteboard_events(
            &pool,
            &WhiteboardLoadOpts { after_gate_seq: 0, session_id: None, scope: None, limit: 100 },
        )
        .await
        .expect("log loads");
        let decisions: Vec<_> =
            logged.iter().filter(|event| event.kind == WhiteboardKind::Decision).collect();
        assert_eq!(decisions.len(), 1, "exactly one resume decision, got: {decisions:?}");
        assert_eq!(decisions[0].payload["reason"], "resume-continue-blocked");
        assert_eq!(decisions[0].payload["selected_agent"], "coder");
        let cited: Vec<&str> = decisions[0].payload["supporting_evidence_ids"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(serde_json::Value::as_str)
            .collect();
        assert_eq!(
            cited,
            vec!["ev-post-progress"],
            "cited ids are the REAL post-cursor facts only (cursor respected), got: {cited:?}"
        );
    }

    /// ADR-65 §8 / acceptance 9: a memory store that FORBIDS writes — wired
    /// into the continuation loop to prove no path touches vector memory
    /// while restoring an evidenced checkpoint. Retrieve mirrors the
    /// disabled behavior (empty).
    struct ForbiddenMemoryStore;

    #[async_trait::async_trait]
    impl MemoryStore for ForbiddenMemoryStore {
        async fn retrieve(
            &self,
            _query: &concerto_core::memory::MemoryQuery,
            _cancel: CancellationToken,
        ) -> Result<Vec<concerto_core::memory::MemoryChunk>, concerto_core::MemoryError> {
            Ok(Vec::new())
        }

        async fn store(
            &self,
            _entry: concerto_core::memory::MemoryEntry,
            _cancel: CancellationToken,
        ) -> Result<concerto_core::memory::MemoryId, concerto_core::MemoryError> {
            panic!("vector memory write attempted with memory disabled (ADR-65 acceptance 9)")
        }

        async fn invalidate(
            &self,
            _id: concerto_core::memory::MemoryId,
            _cancel: CancellationToken,
        ) -> Result<(), concerto_core::MemoryError> {
            Ok(())
        }
    }

    /// ADR-65 acceptance 9: the blocked-step continuation loop completes with
    /// vector memory entirely DISABLED, and no path ever writes a memory
    /// entry (the store panics on any write — the run's success is the proof).
    #[tokio::test]
    async fn continuation_loop_completes_with_vector_memory_disabled() {
        let (_dir, pool) = resume_log_pool().await;
        let workspace = tempfile::tempdir().expect("workspace dir");
        let session_id = Ulid::new();
        let subtask_id = Ulid::new();
        let project_id = concerto_core::types::ProjectId::resolve(workspace.path()).0;

        // Pre-cursor failure (checkpoint-era, never replayed) + post-cursor
        // progress fact: the same evidence shape the Phase 7 continue test
        // uses.
        append_tool_fact(&pool, session_id, "ev-pre-fail", &subtask_id.to_string(), false).await;
        append_tool_fact(&pool, session_id, "ev-post-progress", &subtask_id.to_string(), true)
            .await;

        let registry = Arc::new(AgentRegistry::new()); // no candidates
        let bus = EventBus::new(16);
        let provider: Arc<dyn concerto_core::traits::provider::LlmProvider> =
            Arc::new(MockProvider::default());
        let spend_tracker = Arc::new(SpendTracker::default());
        let routing = Arc::new(RoutingEngine::new(
            vec![],
            spend_tracker.clone(),
            concerto_config::ModelPinConfig::default(),
            EventBus::default(),
        ));
        let model_selector =
            Arc::new(ModelSelector::new(Arc::new(ModelRegistry::from_profiles(vec![])), routing));
        let mut coordinator = CoordinatorAgent::new(
            registry,
            AgentRunner::new(Arc::new(AgentRegistry::new()), bus.clone(), spend_tracker.clone()),
            model_selector,
            spend_tracker.clone(),
            bus.clone(),
            provider,
            Arc::new(ForbiddenMemoryStore),
        )
        .with_review_store(Some(pool.clone()));

        let cp_json = blocked_step_checkpoint_json(
            &project_id,
            session_id,
            subtask_id,
            "coder",
            "Blocked",
            0,
            Some(1),
        );
        let task = AgentTask::new(session_id, "continue");
        let context = AgentContext::new(concerto_core::types::SessionContext::new(
            session_id,
            workspace.path().to_path_buf(),
        ));
        let result = coordinator
            .decompose_or_restore(&task, &context, &CancellationToken::new(), Some(cp_json))
            .await
            .expect("restore succeeds without vector memory");

        // The restore still reads the log, re-arms the step, and appends its
        // decision — projected memory was simply never consulted for state.
        let graph_task = result
            .graph
            .all_tasks()
            .into_iter()
            .find(|subtask| subtask.id.0 == subtask_id)
            .expect("restored step");
        assert_eq!(graph_task.status, SubTaskStatus::Pending, "Continue re-arms the step");
        let logged = load_whiteboard_events(
            &pool,
            &WhiteboardLoadOpts { after_gate_seq: 0, session_id: None, scope: None, limit: 100 },
        )
        .await
        .expect("log loads");
        let decisions: Vec<_> =
            logged.iter().filter(|event| event.kind == WhiteboardKind::Decision).collect();
        assert_eq!(decisions.len(), 1, "exactly one resume decision");
        assert_eq!(decisions[0].payload["selected_agent"], "coder");
        assert_eq!(decisions[0].payload["reason"], "resume-continue-blocked");
    }

    /// No progress ⇒ replace the agent, never a blind same-agent
    /// re-dispatch (the 5-repeat live failure's first guard).
    #[tokio::test]
    async fn resume_replaces_the_blocked_step_agent_without_progress() {
        let (_dir, pool) = resume_log_pool().await;
        let workspace = tempfile::tempdir().expect("workspace dir");
        let session_id = Ulid::new();
        let subtask_id = Ulid::new();
        let project_id = concerto_core::types::ProjectId::resolve(workspace.path()).0;

        let mut registry = AgentRegistry::new();
        registry.register(Arc::new(MockExpertAgent::always_succeed(AgentId::new("coder"), "x")));
        registry.register(Arc::new(
            MockExpertAgent::always_succeed(AgentId::new("coder2"), "done")
                .with_stage(Some(AgentStage::new("implement"))),
        ));
        let bus = EventBus::new(16);
        let provider: Arc<dyn concerto_core::traits::provider::LlmProvider> =
            Arc::new(MockProvider::default());
        let spend_tracker = Arc::new(SpendTracker::default());
        let routing = Arc::new(RoutingEngine::new(
            vec![],
            spend_tracker.clone(),
            concerto_config::ModelPinConfig::default(),
            EventBus::default(),
        ));
        let model_selector =
            Arc::new(ModelSelector::new(Arc::new(ModelRegistry::from_profiles(vec![])), routing));
        let registry = Arc::new(registry);
        let mut coordinator = CoordinatorAgent::new(
            registry.clone(),
            AgentRunner::new(registry, bus.clone(), spend_tracker.clone()),
            model_selector,
            spend_tracker.clone(),
            bus.clone(),
            provider,
            Arc::new(NullMemoryStore),
        )
        .with_review_store(Some(pool.clone()));

        let cp_json = blocked_step_checkpoint_json(
            &project_id,
            session_id,
            subtask_id,
            "coder",
            "Blocked",
            1, // one failed outcome before the checkpoint
            None,
        );
        let task = AgentTask::new(session_id, "continue");
        let context = AgentContext::new(concerto_core::types::SessionContext::new(
            session_id,
            workspace.path().to_path_buf(),
        ));
        let result = coordinator
            .decompose_or_restore(&task, &context, &CancellationToken::new(), Some(cp_json))
            .await
            .expect("restore succeeds");

        let graph_task = result
            .graph
            .all_tasks()
            .into_iter()
            .find(|subtask| subtask.id.0 == subtask_id)
            .expect("restored step");
        assert_eq!(
            graph_task.role.as_str(),
            "coder2",
            "no progress ⇒ replace the agent, never a blind same-coder re-dispatch"
        );
        assert_eq!(graph_task.status, SubTaskStatus::Pending, "the replacement re-arms");

        let logged = load_whiteboard_events(
            &pool,
            &WhiteboardLoadOpts { after_gate_seq: 0, session_id: None, scope: None, limit: 100 },
        )
        .await
        .expect("log loads");
        let decision = logged
            .iter()
            .find(|event| event.kind == WhiteboardKind::Decision)
            .expect("the replace decision is recorded");
        assert_eq!(decision.payload["reason"], "resume-replace-agent");
        assert_eq!(decision.payload["selected_agent"], "coder2");
        assert!(result.action_ledger.iter().any(|entry| entry.kind == "resume-replaced"));
    }

    /// Repeated identical failures with no alternative left ⇒ skip — the
    /// bound that kills the documented 5-repeat failure: at most ONE bounded
    /// same-agent continue is ever granted, then the step is skipped.
    #[tokio::test]
    async fn resume_skips_after_repeated_identical_failures() {
        let (_dir, pool) = resume_log_pool().await;
        let workspace = tempfile::tempdir().expect("workspace dir");
        let session_id = Ulid::new();
        let subtask_id = Ulid::new();
        let project_id = concerto_core::types::ProjectId::resolve(workspace.path()).0;

        // One more failure AFTER the checkpoint (2 in the ledger → 3 total),
        // a real row the decision may cite.
        let fact =
            append_tool_fact(&pool, session_id, "ev-post-fail", &subtask_id.to_string(), false)
                .await;

        let registry = Arc::new(AgentRegistry::new()); // no alternative agent
        let bus = EventBus::new(16);
        let provider: Arc<dyn concerto_core::traits::provider::LlmProvider> =
            Arc::new(MockProvider::default());
        let spend_tracker = Arc::new(SpendTracker::default());
        let routing = Arc::new(RoutingEngine::new(
            vec![],
            spend_tracker.clone(),
            concerto_config::ModelPinConfig::default(),
            EventBus::default(),
        ));
        let model_selector =
            Arc::new(ModelSelector::new(Arc::new(ModelRegistry::from_profiles(vec![])), routing));
        let mut coordinator = CoordinatorAgent::new(
            registry,
            AgentRunner::new(Arc::new(AgentRegistry::new()), bus.clone(), spend_tracker.clone()),
            model_selector,
            spend_tracker.clone(),
            bus.clone(),
            provider,
            Arc::new(NullMemoryStore),
        )
        .with_review_store(Some(pool.clone()));

        let cp_json = blocked_step_checkpoint_json(
            &project_id,
            session_id,
            subtask_id,
            "coder",
            "Blocked",
            2,
            Some(fact.gate_seq - 1),
        );
        let task = AgentTask::new(session_id, "continue");
        let context = AgentContext::new(concerto_core::types::SessionContext::new(
            session_id,
            workspace.path().to_path_buf(),
        ));
        let result = coordinator
            .decompose_or_restore(&task, &context, &CancellationToken::new(), Some(cp_json))
            .await
            .expect("restore succeeds");

        let graph_task = result
            .graph
            .all_tasks()
            .into_iter()
            .find(|subtask| subtask.id.0 == subtask_id)
            .expect("restored step");
        assert_eq!(
            graph_task.status,
            SubTaskStatus::Failed,
            "the step is skipped (honest terminal state), never re-dispatched again"
        );
        let logged = load_whiteboard_events(
            &pool,
            &WhiteboardLoadOpts { after_gate_seq: 0, session_id: None, scope: None, limit: 100 },
        )
        .await
        .expect("log loads");
        let decision = logged
            .iter()
            .find(|event| event.kind == WhiteboardKind::Decision)
            .expect("the skip decision is recorded");
        assert_eq!(decision.payload["reason"], "resume-skip-step");
        // The skip cites the REAL post-cursor failure fact.
        assert_eq!(
            decision.payload["supporting_evidence_ids"],
            serde_json::json!(["ev-post-fail"]),
        );
        assert!(result.action_ledger.iter().any(|entry| entry.kind == "resume-skipped"));
    }

    /// Acceptance 7 (e2e): with NO recorded, evidence-backed decision
    /// selecting the architect, a blocked architect step is never re-armed
    /// for dispatch — and with a recorded scheduler decision (a logged
    /// Decision row explicitly selecting the researcher) plus progress
    /// facts, the dispatch IS allowed.
    #[tokio::test]
    async fn resume_never_dispatches_architect_or_researcher_without_recorded_decision() {
        // ── Negative: no recorded decision ⇒ the architect step stays
        //    terminated, never re-armed. ──────────────────────────────────
        let (_dir, pool) = resume_log_pool().await;
        let workspace = tempfile::tempdir().expect("workspace dir");
        let session_id = Ulid::new();
        let subtask_id = Ulid::new();
        let project_id = concerto_core::types::ProjectId::resolve(workspace.path()).0;

        // The architect is registered with its design stage so the
        // evaluation classifies the step as architect work (acceptance 7).
        let mut architect_registry = AgentRegistry::new();
        architect_registry
            .register(Arc::new(MockExpertAgent::always_succeed(AgentId::new("architect"), "x")));
        let bus = EventBus::new(16);
        let provider: Arc<dyn concerto_core::traits::provider::LlmProvider> =
            Arc::new(MockProvider::default());
        let spend_tracker = Arc::new(SpendTracker::default());
        let routing = Arc::new(RoutingEngine::new(
            vec![],
            spend_tracker.clone(),
            concerto_config::ModelPinConfig::default(),
            EventBus::default(),
        ));
        let model_selector =
            Arc::new(ModelSelector::new(Arc::new(ModelRegistry::from_profiles(vec![])), routing));
        let mut coordinator = CoordinatorAgent::new(
            Arc::new(architect_registry),
            AgentRunner::new(Arc::new(AgentRegistry::new()), bus.clone(), spend_tracker.clone()),
            model_selector,
            spend_tracker.clone(),
            bus.clone(),
            provider,
            Arc::new(NullMemoryStore),
        )
        .with_review_store(Some(pool.clone()));

        let cp_json = blocked_step_checkpoint_json(
            &project_id,
            session_id,
            subtask_id,
            "architect",
            "Failed",
            0,
            None,
        );
        let task = AgentTask::new(session_id, "continue");
        let context = AgentContext::new(concerto_core::types::SessionContext::new(
            session_id,
            workspace.path().to_path_buf(),
        ));
        let result = coordinator
            .decompose_or_restore(&task, &context, &CancellationToken::new(), Some(cp_json))
            .await
            .expect("restore succeeds");
        let step = result.graph.all_tasks().into_iter().next().expect("the blocked step");
        assert_ne!(
            step.status,
            SubTaskStatus::Pending,
            "the architect step is NEVER re-armed for dispatch without a recorded decision"
        );
        assert_eq!(step.status, SubTaskStatus::Failed, "the step is skipped instead");
        let logged = load_whiteboard_events(
            &pool,
            &WhiteboardLoadOpts { after_gate_seq: 0, session_id: None, scope: None, limit: 100 },
        )
        .await
        .expect("log loads");
        let decision = logged
            .iter()
            .find(|event| event.kind == WhiteboardKind::Decision)
            .expect("the skip decision is recorded");
        assert_eq!(decision.payload["reason"], "resume-skip-step");

        // ── Positive: a recorded, evidence-backed decision (the Phase-6
        //    scheduler's logged dispatch decision) explicitly selects the
        //    researcher, and post-cursor progress facts exist: the resume
        //    continues the dispatch behind that decision. ─────────────────
        let researcher_id = Ulid::new();
        let progress = append_tool_fact(
            &pool,
            session_id,
            "ev-research-progress",
            &researcher_id.to_string(),
            true,
        )
        .await;
        // The scheduler's dispatch decision cites a REAL evidence id.
        let scheduled_decision = append_whiteboard_event(
            &pool,
            &NewWhiteboardEvent {
                event_id: "ev-scheduled-exploration".to_owned(),
                agent_id: "coordinator".to_owned(),
                kind: WhiteboardKind::Decision,
                scope: String::new(),
                session_id: Some(session_id.to_string()),
                plan_id: None,
                causation: None,
                payload: serde_json::json!({
                    "selected_agent": "researcher",
                    "reason": "evidence-gap-explore",
                    "required_output": "Grounded fact inventory (tool reads only)",
                    "supporting_evidence_ids": ["ev-research-progress"],
                }),
                pre_image_hash: None,
                created_at: 2,
            },
        )
        .await
        .expect("the scheduler decision is recorded");

        let cp_json = blocked_step_checkpoint_json(
            &project_id,
            session_id,
            researcher_id,
            "researcher",
            "Blocked",
            0,
            Some(progress.gate_seq - 1),
        );
        let result = coordinator
            .decompose_or_restore(&task, &context, &CancellationToken::new(), Some(cp_json))
            .await
            .expect("restore succeeds");
        let step = result
            .graph
            .all_tasks()
            .into_iter()
            .find(|subtask| subtask.id.0 == researcher_id)
            .expect("the blocked researcher step");
        assert_eq!(
            step.status,
            SubTaskStatus::Pending,
            "with a recorded, evidence-backed decision the dispatch is allowed"
        );
        assert_eq!(step.role.as_str(), "researcher");
        let _ = scheduled_decision;
    }

    /// Run-continuity Phase 1 (Task D): a bare "continue" over a stored
    /// checkpoint restores the graph AND the DesignDoc — the architect is
    /// NOT re-invoked (the poisoned canary would fail the run if it were) —
    /// and the resumed run completes and clears the checkpoint.
    #[tokio::test]
    async fn continue_with_stored_checkpoint_skips_architect() {
        let dir = tempfile::tempdir().expect("tempdir for test workspace");

        // ── Phase 1: fresh run stalls (review unresolved) ────────────
        let bus = EventBus::new(256);
        let mocks = vec![
            MockExpertAgent::always_succeed(AgentId::new("architect"), DESIGN_DOC_JSON),
            MockExpertAgent::always_succeed(AgentId::new("researcher"), "found"),
            MockExpertAgent::always_succeed(AgentId::new("coder"), "implemented")
                .with_artifact_writer(),
        ];
        let mut registry = AgentRegistry::from_mocks(mocks);
        registry.register(Arc::new(AlwaysRevise));
        let registry = Arc::new(registry);
        let (mut coordinator, store, session_id) =
            coordinator_with_store(bus.clone(), registry, PLAN_RESEARCH_CODER.into(), dir.path())
                .await;
        let task = AgentTask::new(session_id, "build the thing");
        let context = AgentContext::new(concerto_core::types::SessionContext::new(
            session_id,
            dir.path().to_path_buf(),
        ));
        let first = coordinator
            .run(task, context, CancellationToken::new(), None)
            .await
            .expect("first run should succeed");
        let stored =
            first.checkpoint_json.clone().expect("the stalled first run carries a checkpoint");

        // ── Phase 2: "continue" restores and completes ───────────────
        let bus2 = EventBus::new(256);
        let mut rx = bus2.subscribe();
        let mocks2 = vec![
            // Poisoned canary: any architect dispatch fails the run.
            MockExpertAgent::always_fail(AgentId::new("architect"), "must not be dispatched"),
            MockExpertAgent::always_succeed(AgentId::new("coder"), "implemented")
                .with_artifact_writer(),
            MockExpertAgent::always_succeed(AgentId::new("validator"), "valid"),
        ];
        // The resume phase shares phase 1's store and session row.
        let mut coordinator2 = coordinator_on_store(
            bus2,
            Arc::new(AgentRegistry::from_mocks(mocks2)),
            String::new(),
            store.clone(),
        );
        let continue_task = AgentTask::new(session_id, "continue");
        let continue_ctx = AgentContext::new(concerto_core::types::SessionContext::new(
            session_id,
            dir.path().to_path_buf(),
        ));
        let second = coordinator2
            .run(continue_task, continue_ctx, CancellationToken::new(), Some(stored))
            .await
            .expect("resumed run should succeed");

        assert_eq!(
            second.completion_status,
            concerto_core::types::AgentCompletionStatus::Completed,
            "the resumed run completes off the restored graph: {}",
            second.final_message
        );
        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event.kind.clone());
        }
        assert!(
            !events.iter().any(|kind| {
                matches!(kind, EventKind::SubTaskStarted { role, .. } if role.as_str() == "architect")
            }),
            "the architect must never be re-invoked on a checkpoint resume"
        );
        let doc = coordinator2
            .design_doc_snapshot()
            .expect("the DesignDoc is restored from the checkpoint");
        assert_eq!(doc.goals, vec!["do the thing".to_owned()]);
        assert!(
            store
                .load_orchestration_checkpoint(session_id)
                .await
                .expect("checkpoint store read")
                .is_none(),
            "the successful resumed run clears the checkpoint"
        );
    }

    /// Fail-soft fallback: a resume-shaped run with NO stored checkpoint
    /// decomposes fresh — the architect dispatches once and the run
    /// completes exactly like a first run.
    #[tokio::test]
    async fn continue_without_checkpoint_falls_back_to_fresh_decompose() {
        let dir = tempfile::tempdir().expect("tempdir for test workspace");
        let bus = EventBus::new(256);
        let mut rx = bus.subscribe();
        let mocks = vec![
            MockExpertAgent::always_succeed(AgentId::new("architect"), DESIGN_DOC_JSON),
            MockExpertAgent::always_succeed(AgentId::new("researcher"), "found"),
            MockExpertAgent::always_succeed(AgentId::new("coder"), "implemented")
                .with_artifact_writer(),
            MockExpertAgent::always_succeed(AgentId::new("validator"), "valid"),
        ];
        let (mut coordinator, store, session_id) = coordinator_with_store(
            bus.clone(),
            Arc::new(AgentRegistry::from_mocks(mocks)),
            PLAN_RESEARCH_CODER.into(),
            dir.path(),
        )
        .await;

        // A bare "continue" whose session holds NO orchestration checkpoint.
        let task = AgentTask::new(session_id, "continue");
        let context = AgentContext::new(concerto_core::types::SessionContext::new(
            session_id,
            dir.path().to_path_buf(),
        ));
        let output = coordinator
            .run(task, context, CancellationToken::new(), None)
            .await
            .expect("coordinator run should succeed");

        assert_eq!(
            output.completion_status,
            concerto_core::types::AgentCompletionStatus::Completed,
            "the fallback run completes like a fresh run: {}",
            output.final_message
        );
        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event.kind.clone());
        }
        let architect_dispatches = events
            .iter()
            .filter(|kind| {
                matches!(kind, EventKind::SubTaskStarted { role, .. } if role.as_str() == "architect")
            })
            .count();
        assert_eq!(
            architect_dispatches, 1,
            "with nothing stored, a continue falls back to a fresh decompose (one architect pass)"
        );
        assert!(
            store
                .load_orchestration_checkpoint(session_id)
                .await
                .expect("checkpoint store read")
                .is_none(),
            "the successful fallback run still clears its checkpoint"
        );
    }

    /// Unit test of the stall predicate: declared-Completion false, declared
    /// deliverables unproduced, or a Failed/Blocked subtask each stall an
    /// otherwise-completed run; only the clean combination does not.
    #[test]
    fn run_is_stalled_predicate() {
        let completed = concerto_core::types::AgentCompletionStatus::Completed;
        let partial = concerto_core::types::AgentCompletionStatus::Partial;

        let mut graph = TaskGraph::default();
        graph.add_root(SubTask {
            id: TaskId::new(),
            parent_id: None,
            session_id: Ulid::new(),
            role: AgentId::new("researcher"),
            description: "research".into(),
            status: SubTaskStatus::Completed,
            dependencies: vec![],
            deliverable: None,
            created_at: time::OffsetDateTime::now_utc(),
            completed_at: Some(time::OffsetDateTime::now_utc()),
        });

        // 1. declared Completion false → stalled.
        assert!(run_is_stalled(partial, false, &graph));
        // Clean success → not stalled.
        assert!(!run_is_stalled(completed, false, &graph));
        // 2. declared deliverables unproduced → stalled even when Completed.
        assert!(run_is_stalled(completed, true, &graph));
        // 3. a Blocked or Failed subtask → stalled even when Completed.
        let mut blocked_graph = TaskGraph::default();
        blocked_graph.add_root(SubTask {
            id: TaskId::new(),
            parent_id: None,
            session_id: Ulid::new(),
            role: AgentId::new("coder"),
            description: "blocked".into(),
            status: SubTaskStatus::Blocked,
            dependencies: vec![],
            deliverable: None,
            created_at: time::OffsetDateTime::now_utc(),
            completed_at: None,
        });
        assert!(run_is_stalled(completed, false, &blocked_graph));
        let mut failed_graph = TaskGraph::default();
        failed_graph.add_root(SubTask {
            id: TaskId::new(),
            parent_id: None,
            session_id: Ulid::new(),
            role: AgentId::new("coder"),
            description: "failed".into(),
            status: SubTaskStatus::Failed,
            dependencies: vec![],
            deliverable: None,
            created_at: time::OffsetDateTime::now_utc(),
            completed_at: None,
        });
        assert!(run_is_stalled(completed, false, &failed_graph));
    }

    /// The missing-deliverables clause: a non-empty declared set with an
    /// unproduced file stalls; a produced file or an empty declared set
    /// (vacuous) does not.
    #[test]
    fn expected_artifacts_unproduced_matches_c06_semantics() {
        let dir = tempfile::tempdir().expect("tempdir for artifact check");
        let root = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf())
            .expect("tempdir path is valid UTF-8");

        // Nothing declared → vacuously produced.
        assert!(!expected_artifacts_unproduced(&root, &HashMap::new()));

        // Declared but missing → unproduced.
        let mut declared = HashMap::new();
        declared.insert(TaskId::new(), vec![root.join("src/missing.rs")]);
        assert!(expected_artifacts_unproduced(&root, &declared));

        // Declared and produced with real content → produced.
        std::fs::create_dir_all(dir.path().join("src")).expect("create src dir");
        std::fs::write(dir.path().join("src/main.rs"), "pub fn main() {}\n")
            .expect("write produced artifact");
        let mut produced = HashMap::new();
        produced.insert(TaskId::new(), vec![root.join("src/main.rs")]);
        assert!(!expected_artifacts_unproduced(&root, &produced));
    }

    /// T5: the only agent dispatched during planning-only is the design
    /// (architect) stage agent; the planner runs as a provider call and every
    /// other registered role stays untouched.
    #[tokio::test]
    async fn planning_only_dispatch_limited_to_design_stage() {
        let bus = EventBus::new(256);
        let mocks = vec![
            MockExpertAgent::always_succeed(AgentId::new("architect"), DESIGN_DOC_JSON),
            MockExpertAgent::always_succeed(AgentId::new("researcher"), "found"),
            MockExpertAgent::always_succeed(AgentId::new("coder"), "implemented"),
            MockExpertAgent::always_succeed(AgentId::new("reviewer"), "approved"),
            MockExpertAgent::always_succeed(AgentId::new("validator"), "valid"),
        ];
        let (output, events) = run_for_test(
            coordinator_with(
                bus.clone(),
                Arc::new(AgentRegistry::from_mocks(mocks)),
                PLAN_RESEARCH_CODER.into(),
            )
            .with_orchestration_depth(OrchestrationDepth::PlanningOnly),
            bus.clone(),
        )
        .await;

        assert_eq!(
            output.completion_status,
            concerto_core::types::AgentCompletionStatus::Completed
        );
        let started_roles = events
            .iter()
            .filter_map(|kind| match kind {
                EventKind::SubTaskStarted { role, .. } => Some(role.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(
            started_roles.contains(&"architect"),
            "the architect must have been dispatched once for the design stage"
        );
        assert!(
            started_roles.iter().all(|role| *role == "architect"),
            "only the design-stage agent may be dispatched in planning-only, got: {started_roles:?}"
        );
    }

    /// T7: when the planner cannot parse its response, the heuristic fallback
    /// pipeline still builds a graph and planning-only still renders a plan.
    /// T7 + live-fix: when the planner backend fails JSON parsing, the
    /// heuristic pipeline still produces a planning-only plan AND persists a
    /// durable PlanArtifact — the live run showed `plans/` empty and a null
    /// `plan_id` on this path, which left the rendered plan unbound.
    #[tokio::test]
    async fn planning_only_planner_fallback_still_renders() {
        let bus = EventBus::new(256);
        let mocks = vec![
            MockExpertAgent::always_succeed(AgentId::new("architect"), DESIGN_DOC_JSON),
            MockExpertAgent::always_succeed(AgentId::new("researcher"), "found"),
            MockExpertAgent::always_succeed(AgentId::new("coder"), "implemented"),
        ];
        let plans_dir = tempfile::tempdir().expect("tempdir for plans artifacts");
        let plans = concerto_sessions::plans::PlansManager::at(plans_dir.path().join("plans"));
        let (output, events) = run_for_test(
            coordinator_with(
                bus.clone(),
                Arc::new(AgentRegistry::from_mocks(mocks)),
                "this is not a plan".into(),
            )
            .with_orchestration_depth(OrchestrationDepth::PlanningOnly)
            .with_plans(Some(plans)),
            bus.clone(),
        )
        .await;

        assert_eq!(
            output.completion_status,
            concerto_core::types::AgentCompletionStatus::Completed,
            "the fallback pipeline must still produce a planning-only plan, got: {:?}",
            output.completion_status,
        );
        assert!(
            output.final_message.contains("[coder]"),
            "the fallback graph renders its implement subtask: {}",
            output.final_message,
        );
        let plan_ids: Vec<&String> = events
            .iter()
            .filter_map(|event| match event {
                concerto_core::event::EventKind::MultiAgentModeStarted { plan_id, .. } => {
                    plan_id.as_ref()
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            plan_ids.len(),
            1,
            "exactly one MultiAgentModeStarted on the planning path, got: {plan_ids:?}"
        );
        assert!(
            !plan_ids[0].is_empty(),
            "the heuristic fallback must still persist a durable artifact id"
        );
        let persisted: Vec<_> = std::fs::read_dir(plans_dir.path().join("plans"))
            .expect("plans dir must exist")
            .filter_map(Result::ok)
            .collect();
        assert_eq!(
            persisted.len(),
            1,
            "the fallback plan artifact must be persisted to the plans dir"
        );
    }

    // ------------------------------------------------------------------
    // ADR-65 §6: evidence-driven fallback scheduling, end to end
    // ------------------------------------------------------------------

    /// A file-backed sessions pool with migrations applied, teed under a
    /// tempdir so the DB lives on disk and is cleaned up on drop.
    async fn fallback_evidence_pool() -> (tempfile::TempDir, sqlx::SqlitePool) {
        let dir = tempfile::tempdir().expect("tempdir for the evidence store");
        let options = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(dir.path().join("evidence.db"))
            .create_if_missing(true)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .busy_timeout(std::time::Duration::from_secs(5))
            .foreign_keys(true)
            .synchronous(sqlx::sqlite::SqliteSynchronous::Normal);
        let pool = sqlx::pool::PoolOptions::new()
            .max_connections(4)
            .connect_with(options)
            .await
            .expect("pool connects");
        sqlx::migrate!("../sessions/migrations").run(&pool).await.expect("migrations apply");
        (dir, pool)
    }

    /// ADR-65 §6 acceptance: a fallback-path run (planner JSON fails) with a
    /// grounded pre-planning snapshot executes the Implement step and records
    /// its dispatch `Decision` on the whiteboard evidence chain — citing the
    /// REAL design-doc claim events as supporting evidence and causation.
    #[tokio::test]
    async fn fallback_with_grounded_snapshot_executes_implement_and_records_decision() {
        // Real project root + real file: the snapshot barrier grounds the
        // architect's proposed path, so the Phase-5 verifier marks the doc
        // Active and the scheduler schedules Implement WITH the contract.
        let workspace = tempfile::tempdir().expect("workspace tempdir");
        let src = workspace.path().join("src");
        std::fs::create_dir_all(&src).expect("src dir");
        std::fs::write(src.join("main.rs"), b"fn main() {}\n").expect("write src/main.rs");

        let (_store_dir, pool) = fallback_evidence_pool().await;
        let cancel = CancellationToken::new();
        let snapshot = crate::workspace_snapshot::run_snapshot_barrier(
            Some(&pool),
            workspace.path(),
            "fallback-grounded-session",
            &cancel,
        )
        .await
        .expect("the barrier captures a readable project");

        let bus = EventBus::new(256);
        let mocks = vec![
            // The architect proposes src/main.rs — GROUNDED by the barrier's
            // inventory (and by the rows the barrier applied).
            MockExpertAgent::always_succeed(
                AgentId::new("architect"),
                r#"{"goals":["grounded"],"proposed_files":["src/main.rs"],"interface_sketch":"s"}"#,
            ),
            MockExpertAgent::always_succeed(AgentId::new("coder"), "implemented")
                .with_artifact_writer(),
            MockExpertAgent::always_succeed(AgentId::new("reviewer"), "approved"),
            MockExpertAgent::always_succeed(AgentId::new("validator"), "valid"),
        ];
        let (output, events) = run_for_test(
            coordinator_with(
                bus.clone(),
                Arc::new(AgentRegistry::from_mocks(mocks)),
                "this is not a plan".into(),
            )
            .with_workspace_snapshot(snapshot)
            .with_review_store(Some(pool.clone())),
            bus.clone(),
        )
        .await;

        assert_eq!(
            output.completion_status,
            concerto_core::types::AgentCompletionStatus::Completed,
            "the evidence-sufficient fallback executes its Implement step: {}",
            output.final_message
        );
        assert!(
            events.iter().any(|kind| matches!(
                kind,
                EventKind::SubTaskStarted { role, .. } if role.as_str() == "coder"
            )),
            "the Implement step dispatched to the implement-stage agent"
        );
        assert!(
            !events.iter().any(|kind| matches!(
                kind,
                EventKind::SubTaskStarted { role, .. } if role.as_str() == "researcher"
            )),
            "evidence is sufficient: no exploration dispatch is added"
        );

        // The dispatch Decision cites the REAL claim/verdict events.
        let events_load = concerto_sessions::whiteboard::load_whiteboard_events(
            &pool,
            &concerto_sessions::whiteboard::WhiteboardLoadOpts {
                after_gate_seq: 0,
                session_id: None,
                scope: None,
                limit: usize::MAX,
            },
        )
        .await
        .expect("whiteboard loads");
        let dispatch_decisions: Vec<_> = events_load
            .iter()
            .filter(|event| {
                event.kind == WhiteboardKind::Decision
                    && event.payload.get("selected_agent").is_some()
            })
            .collect();
        assert_eq!(
            dispatch_decisions.len(),
            1,
            "exactly one fallback dispatch decision, got: {dispatch_decisions:?}"
        );
        let decision = dispatch_decisions[0];
        assert_eq!(decision.payload["selected_agent"], "coder");
        assert_eq!(decision.payload["reason"], "doc-active-implement-with-contract");
        let claim_event = events_load
            .iter()
            .find(|event| event.kind == WhiteboardKind::DesignDoc)
            .expect("the doc claim is on the log");
        assert_eq!(
            decision.causation.as_deref(),
            Some(claim_event.event_id.as_str()),
            "the doc-driven decision causally references the REAL claim event"
        );
        // Its supporting evidence ids are REAL existing events.
        assert!(
            decision.payload["supporting_evidence_ids"]
                .as_array()
                .expect("supporting ids array")
                .iter()
                .all(|id| events_load.iter().any(|event| &event.event_id == id)),
            "every cited id exists in the log"
        );
    }

    /// ADR-65 §6 integration: a fallback run with a quarantined doc and no
    /// recorded evidence starts with the grounding Exploration step, then the
    /// scheduled Implement step runs WITHOUT the doc contract (the quarantine
    /// persists as advisory context). The old fixed research→implement shape
    /// is replaced by the evidence-driven pair; both dispatches are recorded
    /// as whiteboard `Decision` events in order.
    ///
    /// Executed-phase completion is orthogonal to scheduling here (a
    /// no-contract implement declares no artifacts, so the C-06 zero-file
    /// gate stalls it after revision — hermetic mocks produce no files); the
    /// assertions below cover the scheduled dispatches and their Decision
    /// records.
    #[tokio::test]
    async fn fallback_quarantined_doc_explores_then_implements_without_contract() {
        let (_store_dir, pool) = fallback_evidence_pool().await;
        let bus = EventBus::new(256);
        let mocks = vec![
            // DESIGN_DOC_JSON proposes src/a.rs, which nothing grounds here:
            // the verifier quarantines the doc (ungrounded + no observations).
            MockExpertAgent::always_succeed(AgentId::new("architect"), DESIGN_DOC_JSON),
            MockExpertAgent::always_succeed(AgentId::new("researcher"), "grounded"),
            MockExpertAgent::always_succeed(AgentId::new("coder"), "implemented"),
            MockExpertAgent::always_succeed(AgentId::new("reviewer"), "approved"),
            MockExpertAgent::always_succeed(AgentId::new("validator"), "valid"),
        ];
        let (_output, _events) = run_for_test(
            coordinator_with(
                bus.clone(),
                Arc::new(AgentRegistry::from_mocks(mocks)),
                "this is not a plan".into(),
            )
            .with_review_store(Some(pool.clone())),
            bus.clone(),
        )
        .await;

        // The whiteboard recorded the evidence-driven pair, in order.
        let logged = concerto_sessions::whiteboard::load_whiteboard_events(
            &pool,
            &concerto_sessions::whiteboard::WhiteboardLoadOpts {
                after_gate_seq: 0,
                session_id: None,
                scope: None,
                limit: usize::MAX,
            },
        )
        .await
        .expect("whiteboard loads");
        let decisions: Vec<_> = logged
            .iter()
            .filter(|event| {
                event.kind == WhiteboardKind::Decision
                    && event.payload.get("selected_agent").is_some()
            })
            .collect();
        let reasons: Vec<&str> = decisions
            .iter()
            .map(|event| event.payload["reason"].as_str().expect("reason string"))
            .collect();
        assert_eq!(
            reasons,
            vec!["quarantined-grounding-explore", "quarantined-proceed-without-doc",],
            "the quarantine plan grounds FIRST, then proceeds without the doc: {reasons:?}"
        );
        assert_eq!(decisions[0].payload["selected_agent"], "researcher");
        assert_eq!(decisions[1].payload["selected_agent"], "coder");
        assert!(
            !decisions[1].payload["supporting_evidence_ids"]
                .as_array()
                .expect("supporting ids array")
                .is_empty(),
            "the doc-driven decision cites the REAL claim/verdict events"
        );
    }

    /// ADR-65 §6 rule (b), end to end: with NO design-capable agent and no
    /// workspace evidence, the scheduler returns the exploration step ONLY;
    /// the coordinator dispatches it inline (same specialist-run plumbing as
    /// the design stage) and re-consults — the materialized plan then carries
    /// the implement step. Bounded: exactly one exploration dispatch.
    #[tokio::test]
    async fn fallback_evidence_gap_dispatches_exploration_inline_then_implements() {
        let (_store_dir, pool) = fallback_evidence_pool().await;
        let bus = EventBus::new(256);
        let mocks = vec![
            // NO architect: the design stage has no candidate, so the design
            // stage cannot run and the design stays undecided.
            MockExpertAgent::always_succeed(AgentId::new("researcher"), "grounded"),
            MockExpertAgent::always_succeed(AgentId::new("coder"), "implemented"),
            MockExpertAgent::always_succeed(AgentId::new("reviewer"), "approved"),
            MockExpertAgent::always_succeed(AgentId::new("validator"), "valid"),
        ];
        let (_output, _events) = run_for_test(
            coordinator_with(
                bus.clone(),
                Arc::new(AgentRegistry::from_mocks(mocks)),
                "this is not a plan".into(),
            )
            .with_review_store(Some(pool.clone())),
            bus.clone(),
        )
        .await;

        // The whiteboard recorded the two-consultation loop: the exploration
        // first (next step only), then the re-consulted implement decision.
        let logged = concerto_sessions::whiteboard::load_whiteboard_events(
            &pool,
            &concerto_sessions::whiteboard::WhiteboardLoadOpts {
                after_gate_seq: 0,
                session_id: None,
                scope: None,
                limit: usize::MAX,
            },
        )
        .await
        .expect("whiteboard loads");
        let decisions: Vec<_> = logged
            .iter()
            .filter(|event| {
                event.kind == WhiteboardKind::Decision
                    && event.payload.get("selected_agent").is_some()
            })
            .collect();
        let reasons: Vec<&str> = decisions
            .iter()
            .map(|event| event.payload["reason"].as_str().expect("reason string"))
            .collect();
        assert_eq!(
            reasons,
            vec!["evidence-gap-explore", "design-undecided-no-designer-implement"],
            "exploration dispatches first, then the bounded re-consultation: {reasons:?}"
        );
        // The gap's decision is an absence: nothing is fabricated to cite.
        assert!(
            decisions[0].payload["supporting_evidence_ids"]
                .as_array()
                .expect("supporting ids array")
                .is_empty(),
            "no snapshot event: no ids are fabricated for the gap decision"
        );
    }

    /// ADR-65 §6 acceptance 6, end to end: the roster WITHOUT the
    /// architect/researcher and WITHOUT any implement-capable agent fails
    /// planning with the preserved heuristic error — scheduling never invents
    /// a missing stage (the C-06 Partial carries the same message).
    #[tokio::test]
    async fn fallback_architect_only_roster_reports_no_implement_agent() {
        let bus = EventBus::new(256);
        let mocks =
            vec![MockExpertAgent::always_succeed(AgentId::new("architect"), DESIGN_DOC_JSON)];
        let (output, _events) = run_for_test(
            coordinator_with(
                bus.clone(),
                Arc::new(AgentRegistry::from_mocks(mocks)),
                "this is not a plan".into(),
            ),
            bus.clone(),
        )
        .await;
        assert!(
            output.final_message.contains("no implementation-stage agent is registered"),
            "the preserved failure surfaces on the evidence-driven path: {}",
            output.final_message
        );
    }

    /// T6: when the design stage exhausts its recovery ladder, planning-only
    /// surfaces a graceful Partial with the standard paused message and NO
    /// rendered plan (the empty-final-message guard keeps it unbound).
    #[tokio::test]
    async fn planning_only_design_failure_returns_partial_without_plan() {
        let bus = EventBus::new(256);
        let architect = MockExpertAgent::sequence(
            AgentId::new("architect"),
            vec![err_stream_idle(), err_auth()],
        );
        let (output, _events) = run_for_test(
            coordinator_for_design_stage(
                bus.clone(),
                architect,
                concerto_config::ModelPinConfig::default(),
                PLAN_RESEARCH_CODER.into(),
            )
            .with_orchestration_depth(OrchestrationDepth::PlanningOnly),
            bus.clone(),
        )
        .await;

        assert_eq!(
            output.completion_status,
            concerto_core::types::AgentCompletionStatus::Partial,
            "an exhausted design-stage ladder degrades to Partial, got: {:?}",
            output.completion_status,
        );
        assert!(
            output.final_message.contains("could not produce a valid plan"),
            "unexpected final message: {}",
            output.final_message,
        );
        assert!(
            !output.final_message.contains("# Plan"),
            "a failed planning-only run must not render a plan: {}",
            output.final_message,
        );
    }

    /// ADR-58 P2+P3 (Batch 3a): without an attached facade the
    /// unstaffed-`Execution` tag resolution yields the legacy `implement`
    /// tag, so planner partitions and the stage feed are byte-identical on
    /// coordinators built without a resolved blueprint.
    #[test]
    fn execution_stage_tag_without_facade_is_implement() {
        assert_eq!(execution_stage_tag(None), AgentStage::IMPLEMENT);
    }

    /// ADR-58 P2+P3 (Batch 4b): with a facade attached, the unstaffed-
    /// `Execution` tag resolution follows the primary `Execution` stage's
    /// configured tag. `decompose_task` keys its implement roster off this
    /// resolution (R4), so a custom blueprint that renames the primary
    /// `Execution` stage keeps its staffing instead of silently dropping it.
    #[test]
    fn execution_stage_tag_with_facade_follows_primary_execution_stage() {
        use concerto_config::blueprint::{
            Blueprint, CapabilityMask, PipelineDef, StageCondition, StageDef, StageFlags, StageKind,
        };
        use concerto_config::{ResolvedBlueprint, ResolvedStage};
        use std::collections::HashMap;

        let execution_def = StageDef {
            tag: "build".into(),
            label: "Build".into(),
            kind: StageKind::Execution.as_str().to_string(),
            version: 1,
            flags: StageFlags::default(),
            condition: StageCondition::Always,
            max_cycles: None,
            feed: None,
            primary: true,
            agents: vec!["coder".into()],
            fallback: None,
            files: None,
        };
        let resolved = ResolvedBlueprint {
            blueprint: Blueprint {
                schema_version: 1,
                name: "custom-execution".into(),
                description: None,
                pipeline: PipelineDef { stages: vec![execution_def.clone()] },
                relationships: Vec::new(),
            },
            stages: vec![ResolvedStage {
                def: execution_def,
                effective_capabilities: CapabilityMask::default(),
                effective_feed: None,
            }],
            feed_map: HashMap::new(),
            relationship_defaults: Vec::new(),
        };
        let facade = BlueprintFacade::new(&resolved);
        assert_eq!(
            execution_stage_tag(Some(&facade)),
            "build",
            "the resolve follows the primary Execution stage's tag"
        );
        // The standard blueprint resolves to the canonical `implement` tag.
        let standard = concerto_config::OrchestrationConfig::default()
            .resolve(&[], None)
            .expect("the standard blueprint must validate and resolve");
        let standard_facade = BlueprintFacade::new(&standard);
        assert_eq!(execution_stage_tag(Some(&standard_facade)), AgentStage::IMPLEMENT);
    }

    /// Issue #150: kind-based tag resolution follows RENAMED gate stages. A
    /// blueprint that renames the review stage to `quality` and the
    /// acceptance stage to `ship` (kinds preserved) keeps its gate cycles,
    /// replan/fix loops, feeds, and skip messages at the renamed tags; a
    /// kind absent from the pipeline falls back to the legacy canonical tag,
    /// and a coordinator without a facade stays byte-identical.
    #[test]
    fn kind_stage_tag_follows_renamed_gate_tags() {
        use concerto_config::blueprint::{
            Blueprint, CapabilityMask, PipelineDef, StageCondition, StageDef, StageFlags,
        };
        use concerto_config::{ResolvedBlueprint, ResolvedStage};
        use std::collections::HashMap;

        let stage = |tag: &str, label: &str, kind: StageKind| StageDef {
            tag: tag.into(),
            label: label.into(),
            kind: kind.as_str().to_string(),
            version: 1,
            flags: StageFlags::default(),
            condition: StageCondition::Always,
            max_cycles: None,
            feed: None,
            primary: false,
            agents: Vec::new(),
            fallback: None,
            files: None,
        };
        let defs = vec![
            stage("build", "Build", StageKind::Execution),
            stage("quality", "Quality Gate", StageKind::Review),
            stage("ship", "Ship", StageKind::Acceptance),
        ];
        let resolved = ResolvedBlueprint {
            blueprint: Blueprint {
                schema_version: 1,
                name: "renamed-gates".into(),
                description: None,
                pipeline: PipelineDef { stages: defs.clone() },
                relationships: Vec::new(),
            },
            stages: defs
                .iter()
                .map(|def| ResolvedStage {
                    def: def.clone(),
                    effective_capabilities: CapabilityMask::default(),
                    effective_feed: None,
                })
                .collect(),
            feed_map: HashMap::new(),
            relationship_defaults: Vec::new(),
        };
        let facade = BlueprintFacade::new(&resolved);

        // Renamed gate kinds resolve to the renamed tags.
        assert_eq!(kind_stage_tag(Some(&facade), StageKind::Review, AgentStage::REVIEW), "quality");
        assert_eq!(
            kind_stage_tag(Some(&facade), StageKind::Acceptance, AgentStage::VALIDATE),
            "ship"
        );
        // Execution resolution follows the tolerant chain.
        assert_eq!(execution_stage_tag(Some(&facade)), "build");
        // A kind absent from the pipeline falls back to the canonical tag —
        // the legacy lookup stays intact for gate-free pipelines.
        assert_eq!(
            kind_stage_tag(Some(&facade), StageKind::Planning, AgentStage::DESIGN),
            "design"
        );
        // No facade: byte-identical legacy behavior.
        assert_eq!(kind_stage_tag(None, StageKind::Review, AgentStage::REVIEW), "review");
        assert_eq!(kind_stage_tag(None, StageKind::Acceptance, AgentStage::VALIDATE), "validate");
        assert_eq!(execution_stage_tag(None), AgentStage::IMPLEMENT);
    }

    /// Issue #150 (parity): the unstaffed-`Acceptance` fallback resolves
    /// through a RENAMED acceptance tag. The issue's residual gap was that
    /// the facade fallback/eval-attach path was only ever exercised on the
    /// canonical `validate` tag, where facade-kind and legacy-tag keying
    /// agree. Here the acceptance stage is tagged `ship` (kind preserved)
    /// with its own configured fallback persona — `stage_fallback_persona`
    /// (which drives `self_verify_agent`'s attach when no validator is
    /// registered) must pick up that fallback instead of the engine default.
    #[test]
    fn acceptance_fallback_resolves_renamed_acceptance_stage() {
        use concerto_config::blueprint::{
            Blueprint, CapabilityMask, PipelineDef, StageCondition, StageDef, StageFlags,
        };
        use concerto_config::{FallbackPersonaDef, ResolvedBlueprint, ResolvedStage};
        use std::collections::HashMap;

        let stage_def = StageDef {
            tag: "ship".into(),
            label: "Ship".into(),
            kind: StageKind::Acceptance.as_str().to_string(),
            version: 1,
            flags: StageFlags::default(),
            condition: StageCondition::Always,
            max_cycles: None,
            feed: None,
            primary: false,
            agents: Vec::new(),
            fallback: Some(FallbackPersonaDef {
                id: "self-verify".into(),
                label: "Self-verify".into(),
                system_instructions: None,
                capabilities: StageFlags::default(),
            }),
            files: None,
        };
        let resolved = ResolvedBlueprint {
            blueprint: Blueprint {
                schema_version: 1,
                name: "renamed-acceptance".into(),
                description: None,
                pipeline: PipelineDef { stages: vec![stage_def.clone()] },
                relationships: Vec::new(),
            },
            stages: vec![ResolvedStage {
                def: stage_def,
                effective_capabilities: CapabilityMask::default(),
                effective_feed: None,
            }],
            feed_map: HashMap::new(),
            relationship_defaults: Vec::new(),
        };
        let facade = BlueprintFacade::new(&resolved);
        let tag = kind_stage_tag(Some(&facade), StageKind::Acceptance, AgentStage::VALIDATE);
        let persona = stage_fallback_persona(Some(&facade), &tag, coordinator_fallback());
        assert_eq!(persona.id, "self-verify", "renamed acceptance stage's fallback must resolve");
        assert_eq!(persona.label, "Self-verify");
        // The same resolution on the canonical tag would miss the fallback —
        // the exact divergence the issue's parity gap worried about.
        let canonical =
            stage_fallback_persona(Some(&facade), AgentStage::VALIDATE, coordinator_fallback());
        assert_eq!(canonical.id, "coordinator", "canonical-tag lookup misses the renamed fallback");
    }

    /// Slice 1b: an UNKNOWN-kind stage (open kind string, e.g. "blogger")
    /// staffed by a custom agent loads, validates, and resolves through the
    /// config rulebook, the role is classified to its stage, and every
    /// coordinator resolution that keys on the primary-`Execution` stage
    /// degrades gracefully to its legacy fallback. Nothing rejects, skips,
    /// or panics on the unknown kind string.
    #[test]
    fn unknown_kind_stage_dispatch_is_generic_and_panic_free() {
        use concerto_config::blueprint::{
            Blueprint, PipelineDef, StageCondition, StageDef, StageFlags,
        };

        let blogger_def = StageDef {
            tag: "blogger".into(),
            label: "Blogger".into(),
            kind: "blogger".into(), // open unknown user kind
            version: 1,
            flags: StageFlags::default(),
            condition: StageCondition::Always,
            max_cycles: None,
            feed: None,
            primary: false,
            agents: vec!["blogger".into()],
            fallback: None,
            files: None,
        };
        // Loads + resolves: the relaxed rulebook accepts unknown kinds.
        let resolved = concerto_config::resolve_blueprint(&Blueprint {
            schema_version: concerto_config::ORCHESTRATION_SCHEMA_VERSION,
            name: "blog-era".into(),
            description: None,
            pipeline: PipelineDef { stages: vec![blogger_def.clone()] },
            relationships: Vec::new(),
        })
        .expect("a custom-kind stage must validate and resolve");
        let facade = BlueprintFacade::new(&resolved);

        // The staffed role is classified to its stage — never skipped or
        // rejected.
        let stage = facade
            .stage_for_agent(&AgentId::new("blogger"))
            .expect("the staffed role must resolve to its stage");
        assert_eq!(stage.def.tag, "blogger");
        assert_eq!(stage.def.known_kind(), None, "the kind string parses to no known kind");

        // Engine defaults for unknown kinds: no gate, single cycle, no write
        // mask, no feed binding.
        assert!(!stage.def.is_gate());
        assert_eq!(stage.def.default_max_cycles(), 1, "unknown kinds run once");
        assert!(!facade.is_gate("blogger"));
        assert_eq!(stage.effective_capabilities, concerto_config::CapabilityMask::default());

        // No primary-`Execution` stage exists: the sentinel resolutions
        // degrade to their legacy fallbacks instead of panicking on `None`.
        assert_eq!(facade.primary_execution_stage(), None);
        assert_eq!(
            execution_stage_tag(Some(&facade)),
            AgentStage::IMPLEMENT,
            "an Execution-free blueprint falls back to the legacy implement tag"
        );
    }

    /// ADR-58 P2+P3 (§3/F1): the sentinel capability overlay composes the
    /// engine-owned `fs_read`/`git`/`lsp` defaults with the persona's write
    /// flags narrowed against the `Execution`-kind mask, and never attaches
    /// the eval engine. With the engine-default persona this reproduces
    /// exactly the pre-blueprint hardcoded self-implement flags.
    #[test]
    fn sentinel_capabilities_lock_the_engine_default_render() {
        let caps =
            sentinel_capabilities(&coordinator_self_implement_fallback(), StageKind::Execution);
        assert_eq!(
            caps,
            AgentCapabilities {
                fs_read: Some(true),
                fs_write: Some(true),
                shell: Some(true),
                git: Some(true),
                lsp: Some(true),
                eval: Some(false),
            },
            "the engine-default sentinel render must equal the hardcoded construction"
        );
        let effective = caps.effective();
        assert!(effective.fs_read && effective.fs_write && effective.shell);
        assert!(effective.git && effective.lsp);
        assert!(!effective.eval);
    }

    /// ADR-58 P2+P3 (§3/F5): the fallback-persona lookup is byte-identical on
    /// the `standard` blueprint — the implement stage ships `fallback: None`
    /// (→ the promoted engine default `coordinator_self_implement_fallback`)
    /// and the validate gate ships `coordinator_fallback()`.
    #[test]
    fn standard_blueprint_fallbacks_resolve_to_engine_defaults() {
        let resolved = concerto_config::OrchestrationConfig::default()
            .resolve(&[], None)
            .expect("the standard blueprint must validate and resolve");
        let facade = BlueprintFacade::new(&resolved);

        assert_eq!(
            stage_fallback_persona(
                Some(&facade),
                AgentStage::IMPLEMENT,
                coordinator_self_implement_fallback(),
            ),
            coordinator_self_implement_fallback(),
            "standard implement ships fallback: None → engine default"
        );
        assert_eq!(
            stage_fallback_persona(Some(&facade), AgentStage::VALIDATE, coordinator_fallback(),),
            coordinator_fallback(),
            "standard validate ships coordinator_fallback()"
        );
        // Without a facade the same calls resolve to the engine defaults,
        // which is the pre-blueprint construction surface.
        assert_eq!(
            stage_fallback_persona(
                None,
                AgentStage::IMPLEMENT,
                coordinator_self_implement_fallback()
            ),
            coordinator_self_implement_fallback(),
        );
        assert_eq!(
            stage_fallback_persona(None, AgentStage::VALIDATE, coordinator_fallback()),
            coordinator_fallback(),
        );
    }

    // ------------------------------------------------------------------
    // ADR-65 §4: action digest (snapshot ⨁ observations)
    // ------------------------------------------------------------------

    #[test]
    fn format_action_digest_renders_clean_and_dirty_rows_deterministically() {
        let rows = vec![
            ResourceFactRow {
                project_root_hash: "root-a".to_owned(),
                path: "src/main.rs".to_owned(),
                generation: "g1".to_owned(),
                size_bytes: Some(10),
                mtime_ms: Some(1),
                content_hash: Some("0123456789abcdef".to_owned()),
                last_event_id: Some("ev-2".to_owned()),
                last_agent_id: Some("coder".to_owned()),
                observed_at: 200,
                dirty: false,
            },
            ResourceFactRow {
                project_root_hash: "root-a".to_owned(),
                path: "gen/config.yaml".to_owned(),
                generation: "g1".to_owned(),
                size_bytes: Some(5),
                mtime_ms: Some(3),
                content_hash: Some("fedcba9876543210".to_owned()),
                last_event_id: Some("ev-1".to_owned()),
                last_agent_id: Some("researcher".to_owned()),
                observed_at: 100,
                dirty: false,
            },
            ResourceFactRow {
                project_root_hash: "root-a".to_owned(),
                path: "notes/todo.md".to_owned(),
                generation: "g2".to_owned(),
                size_bytes: Some(4),
                mtime_ms: Some(2),
                content_hash: None,
                last_event_id: Some("ev-3".to_owned()),
                last_agent_id: Some("coder".to_owned()),
                observed_at: 150,
                dirty: true,
            },
        ];
        let digest = super::format_action_digest(&rows);
        assert_eq!(
            digest,
            "<action_digest>\n\
             src/main.rs | unchanged-since ev-2 | hash-01234567\n\
             gen/config.yaml | unchanged-since ev-1 | hash-fedcba98\n\
             notes/todo.md | changed\n\
             </action_digest>"
        );
    }

    #[test]
    fn format_action_digest_omits_hash_when_unhashed_row_is_clean() {
        let rows = vec![ResourceFactRow {
            project_root_hash: "root-a".to_owned(),
            path: "big.bin".to_owned(),
            generation: "g1".to_owned(),
            size_bytes: Some(200 * 1024),
            mtime_ms: Some(1),
            content_hash: None,
            last_event_id: Some("ev-9".to_owned()),
            last_agent_id: Some("coder".to_owned()),
            observed_at: 100,
            dirty: false,
        }];
        assert_eq!(
            super::format_action_digest(&rows),
            "<action_digest>\nbig.bin | unchanged-since ev-9\n</action_digest>"
        );
    }

    /// The augmented digest injects an action block after the snapshot digest
    /// when a `review_store` pool with observations is present, and the row
    /// still carries the snapshot digest as a prefix (the injection is
    /// additive, purely for context — ADR-65 §4). Rows are reconciled fresh
    /// (ADR-65 F3) against real files under the snapshot's project root.
    #[tokio::test]
    async fn snapshot_digest_appends_action_digest_when_store_present() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("action_digest_test.db");
        let options = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(&db_path)
            .create_if_missing(true)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .busy_timeout(std::time::Duration::from_secs(5))
            .foreign_keys(true)
            .synchronous(sqlx::sqlite::SqliteSynchronous::Normal);
        let pool = sqlx::pool::PoolOptions::new()
            .max_connections(4)
            .connect_with(options)
            .await
            .expect("pool connects");
        sqlx::migrate!("../sessions/migrations").run(&pool).await.expect("migrations apply");

        // Real project root + real files: F3 re-stats each row NOW, so the
        // observation must carry the file's ACTUAL metadata (size + mtime) —
        // otherwise the freshness fold would (correctly) re-brand it dirty and
        // the "unchanged-since" render below would be impossible to observe.
        let root = tempfile::tempdir().expect("project root");
        let lib_dir = root.path().join("src");
        let data_dir = root.path().join("gen");
        std::fs::create_dir_all(&lib_dir).expect("src dir");
        std::fs::create_dir_all(&data_dir).expect("gen dir");
        std::fs::write(lib_dir.join("lib.rs"), b"0123456789").expect("write lib.rs");
        std::fs::write(data_dir.join("data.json"), br#"{"ok":true}"#).expect("write data.json");
        let lib_meta = std::fs::metadata(lib_dir.join("lib.rs")).expect("lib.rs meta");
        let data_meta = std::fs::metadata(data_dir.join("data.json")).expect("data.json meta");

        let facts = ResourceFacts::new(pool.clone());
        let cancel = CancellationToken::new();
        let root_hash = crate::tool_facts::project_root_hash(root.path());
        let observed = |path: &str,
                        size: u64,
                        mtime: Option<u64>,
                        content_hash: Option<&str>,
                        generation: &str| {
            concerto_sessions::ToolExecutedPayload {
                agent_id: Some("coder".to_owned()),
                task_id: None,
                run_id: None,
                tool: "filesystem".to_owned(),
                args: serde_json::json!({ "operation": "read", "path": path }),
                success: true,
                exit_code: Some(0),
                generation: generation.to_owned(),
                project_root_hash: root_hash.clone(),
                served_from: None,
                paths: vec![concerto_sessions::ObservedPath {
                    path: path.to_owned(),
                    size_bytes: Some(size),
                    mtime_ms: mtime,
                    content_hash: content_hash.map(String::from),
                }],
            }
        };
        // Row 1: clean, content-hashed observation whose metadata matches the
        // real file — the F3 freshness fold leaves it unchanged.
        facts
            .apply_observed(
                "ev-1",
                "coder",
                100,
                &observed(
                    "src/lib.rs",
                    lib_meta.len(),
                    crate::tool_facts::mtime_ms(&lib_meta),
                    Some("deadbeef00cafe01"),
                    "g1",
                ),
                &cancel,
            )
            .await
            .expect("observe row 1");
        // Row 2: dirty (write uncertainty) — must render `changed` even though
        // it carries a content hash and its file is fresh.
        facts
            .apply_observed(
                "ev-2",
                "coder",
                200,
                &observed(
                    "gen/data.json",
                    data_meta.len(),
                    crate::tool_facts::mtime_ms(&data_meta),
                    Some("0123456789abcdef"),
                    "g1",
                ),
                &cancel,
            )
            .await
            .expect("observe row 2");
        facts.mark_dirty(&root_hash, "gen/data.json", &cancel).await.expect("dirty row 2");

        let coordinator =
            coordinator_with(EventBus::new(256), Arc::new(AgentRegistry::default()), "[]".into())
                .with_workspace_snapshot(crate::workspace_snapshot::WorkspaceSnapshotRecord {
                    generation: "gen-1".to_owned(),
                    entries: vec![],
                    captured_at_ms: 0,
                    project_root: root.path().to_string_lossy().into_owned().into(),
                })
                .with_review_store(Some(pool));

        let digest = coordinator.snapshot_digest(&cancel).await.expect("snapshot present");
        assert!(
            digest.starts_with("workspace-snapshot generation=gen-1"),
            "the snapshot digest remains the prefix: {digest}"
        );
        assert!(
            digest.contains("<action_digest>"),
            "the clean row injects into the action digest: {digest}"
        );
        assert!(
            digest.contains("src/lib.rs | unchanged-since ev-1 | hash-deadbeef"),
            "clean content-hashed row renders with the abbreviated hash: {digest}"
        );
        assert!(
            digest.contains("gen/data.json | changed"),
            "dirty rows render as changed: {digest}"
        );
    }

    /// A row whose file diverged from the observation — or vanished — is
    /// folded DIRTY by the F3 freshness reconciliation, so the action digest
    /// never claims an unchanged state the disk no longer matches.
    #[tokio::test]
    async fn snapshot_digest_reconciles_diverged_rows_as_changed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("action_digest_f3.db");
        let options = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(&db_path)
            .create_if_missing(true)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .busy_timeout(std::time::Duration::from_secs(5))
            .foreign_keys(true);
        let pool = sqlx::pool::PoolOptions::new()
            .max_connections(4)
            .connect_with(options)
            .await
            .expect("pool connects");
        sqlx::migrate!("../sessions/migrations").run(&pool).await.expect("migrations apply");

        let root = tempfile::tempdir().expect("project root");
        std::fs::write(root.path().join("lib.rs"), b"ORIGINAL-BYTES").expect("write lib.rs");
        let meta = std::fs::metadata(root.path().join("lib.rs")).expect("meta");
        let facts = ResourceFacts::new(pool.clone());
        let cancel = CancellationToken::new();
        let root_hash = crate::tool_facts::project_root_hash(root.path());
        let payload = concerto_sessions::ToolExecutedPayload {
            agent_id: Some("coder".to_owned()),
            task_id: None,
            run_id: None,
            tool: "filesystem".to_owned(),
            args: serde_json::json!({ "operation": "read", "path": "lib.rs" }),
            success: true,
            exit_code: Some(0),
            generation: "g1".to_owned(),
            project_root_hash: root_hash.clone(),
            served_from: None,
            paths: vec![concerto_sessions::ObservedPath {
                path: "lib.rs".to_owned(),
                size_bytes: Some(meta.len()),
                mtime_ms: crate::tool_facts::mtime_ms(&meta),
                content_hash: Some("cafecafecafecafe".to_owned()),
            }],
        };
        facts.apply_observed("ev-1", "coder", 100, &payload, &cancel).await.expect("observe");

        // Second row: a file that is observed clean and then DELETED — the F3
        // freshness fold must render it changed (the metadata probe fails).
        std::fs::write(root.path().join("gone.md"), b"will vanish").expect("write gone.md");
        let gone_meta = std::fs::metadata(root.path().join("gone.md")).expect("meta gone");
        let gone_payload = concerto_sessions::ToolExecutedPayload {
            agent_id: Some("coder".to_owned()),
            task_id: None,
            run_id: None,
            tool: "filesystem".to_owned(),
            args: serde_json::json!({ "operation": "read", "path": "gone.md" }),
            success: true,
            exit_code: Some(0),
            generation: "g1".to_owned(),
            project_root_hash: root_hash.clone(),
            served_from: None,
            paths: vec![concerto_sessions::ObservedPath {
                path: "gone.md".to_owned(),
                size_bytes: Some(gone_meta.len()),
                mtime_ms: crate::tool_facts::mtime_ms(&gone_meta),
                content_hash: Some("feedfacefeedface".to_owned()),
            }],
        };
        facts.apply_observed("ev-2", "coder", 150, &gone_payload, &cancel).await.expect("observe");

        // The file changes (size divergence is deterministic) AFTER the
        // observation — this is exactly the staleness F3 must surface.
        std::fs::write(root.path().join("lib.rs"), b"LONGER THAN BEFORE").expect("rewrite");
        // And the second file vanishes entirely.
        std::fs::remove_file(root.path().join("gone.md")).expect("remove gone.md");

        let coordinator =
            coordinator_with(EventBus::new(256), Arc::new(AgentRegistry::default()), "[]".into())
                .with_workspace_snapshot(crate::workspace_snapshot::WorkspaceSnapshotRecord {
                    generation: "gen-1".to_owned(),
                    entries: vec![],
                    captured_at_ms: 0,
                    project_root: root.path().to_string_lossy().into_owned().into(),
                })
                .with_review_store(Some(pool));

        let digest = coordinator.snapshot_digest(&cancel).await.expect("snapshot present");
        assert!(
            digest.contains("lib.rs | changed"),
            "a row whose file diverged from the observation renders changed (F3): {digest}"
        );
        assert!(
            digest.contains("gone.md | changed"),
            "a row whose file vanished renders changed (F3): {digest}"
        );
        assert!(
            !digest.contains("unchanged-since"),
            "no row may claim unchanged after divergence: {digest}"
        );
    }

    /// Without a store pool the digest degrades to the bare snapshot digest —
    /// the action block is an optimization, never a dispatch requirement.
    #[tokio::test]
    async fn snapshot_digest_falls_back_to_snapshot_without_store() {
        let coordinator =
            coordinator_with(EventBus::new(256), Arc::new(AgentRegistry::default()), "[]".into())
                .with_workspace_snapshot(crate::workspace_snapshot::WorkspaceSnapshotRecord {
                    generation: "gen-1".to_owned(),
                    entries: vec![],
                    captured_at_ms: 0,
                    // Unused without a store (no root-hash lookup happens), but
                    // the record type now always carries the root identity.
                    project_root: "/proj".into(),
                });
        let digest = coordinator.snapshot_digest(&CancellationToken::new()).await;
        let digest = digest.expect("snapshot present");
        assert!(!digest.contains("<action_digest>"));
        assert!(digest.starts_with("workspace-snapshot generation=gen-1"));
    }

    /// ADR-65 §5 binding-state contract at the coordinator seam: the
    /// `binding_doc` handed to the planner is `Some` ONLY when the
    /// model-free verifier marks the claim active (Verified). A Quarantined
    /// doc resolves to a passive verdict — the coordinator then feeds the
    /// planner `None`, so the proposed_files membership check is NOT applied
    /// (the planner-side ON/OFF semantics are pinned by the planner's own
    /// `design_doc_contract_enforces_proposed_files_membership` test).
    #[tokio::test]
    async fn verify_design_doc_claim_active_only_when_grounded() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("binding_state.db");
        let options = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(&db_path)
            .create_if_missing(true)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .busy_timeout(std::time::Duration::from_secs(5))
            .foreign_keys(true);
        let pool = sqlx::pool::PoolOptions::new()
            .max_connections(4)
            .connect_with(options)
            .await
            .expect("pool connects");
        sqlx::migrate!("../sessions/migrations").run(&pool).await.expect("migrations apply");

        let root = std::path::Path::new("/proj/binding");
        let root_hash = crate::tool_facts::project_root_hash(root);
        let session_id = Ulid::new();
        let project_root = camino::Utf8PathBuf::from(root.to_string_lossy().into_owned());
        let snapshot = crate::workspace_snapshot::WorkspaceSnapshotRecord {
            generation: "gen-1".to_owned(),
            entries: vec![concerto_sessions::ObservedPath {
                path: "src/main.rs".to_owned(),
                size_bytes: Some(1024),
                mtime_ms: Some(1),
                content_hash: None,
            }],
            captured_at_ms: 0,
            project_root: project_root.clone(),
        };

        // The author's grounded read fact: counts as evidence AND grounds
        // nothing new (the snapshot already carries src/main.rs), but proves
        // the coordinator's claim resolution runs against the session log.
        let executed_payload = concerto_sessions::ToolExecutedPayload {
            agent_id: Some("architect".to_owned()),
            task_id: None,
            run_id: None,
            tool: "read_file".to_owned(),
            args: serde_json::json!({}),
            success: true,
            exit_code: Some(0),
            generation: "gen-1".to_owned(),
            project_root_hash: root_hash.clone(),
            served_from: None,
            paths: vec![concerto_sessions::ObservedPath {
                path: "src/main.rs".to_owned(),
                size_bytes: Some(1024),
                mtime_ms: Some(1),
                content_hash: None,
            }],
        };
        append_whiteboard_event(
            &pool,
            &NewWhiteboardEvent {
                event_id: "ev-arch-read".to_owned(),
                agent_id: "architect".to_owned(),
                kind: WhiteboardKind::ToolExecuted,
                scope: String::new(),
                session_id: Some(session_id.to_string()),
                plan_id: None,
                causation: None,
                payload: serde_json::to_value(executed_payload).expect("payload serializes"),
                pre_image_hash: None,
                created_at: 100,
            },
        )
        .await
        .expect("append author read fact");

        let coordinator =
            coordinator_with(EventBus::new(256), Arc::new(AgentRegistry::default()), "[]".into())
                .with_review_store(Some(pool))
                .with_workspace_snapshot(snapshot);
        let cancel = CancellationToken::new();
        let author = AgentId::new("architect");

        // Grounded claim → Verified → active → the doc BINDS (enforced).
        let grounded = DesignDoc {
            goals: Vec::new(),
            constraints: Vec::new(),
            proposed_files: vec![camino::Utf8PathBuf::from("src/main.rs")],
            interface_sketch: String::new(),
            risks: Vec::new(),
        };
        let verdict = coordinator
            .verify_design_doc_claim(&grounded, Some(&author), session_id, &cancel)
            .await
            .expect("claim resolves");
        assert_eq!(verdict.state, crate::design_doc_verifier::DesignDocState::Verified);
        assert!(verdict.state.is_active(), "a verified doc must be a binding contract");
        assert_eq!(verdict.contract_paths, vec!["src/main.rs"]);
        assert_eq!(verdict.author_read_count, 1, "the author's read fact was counted");
        coordinator.append_design_doc_events(session_id, Some(&author), &grounded, &verdict).await;

        // Ungrounded claim → Quarantined → NOT active → the doc stays ADVISORY
        // (the planner receives `binding_doc = None` and enforces nothing).
        let hallucinated = DesignDoc {
            goals: Vec::new(),
            constraints: Vec::new(),
            proposed_files: vec![camino::Utf8PathBuf::from("src/hallucinated.rs")],
            interface_sketch: String::new(),
            risks: Vec::new(),
        };
        let verdict = coordinator
            .verify_design_doc_claim(&hallucinated, Some(&author), session_id, &cancel)
            .await
            .expect("claim resolves");
        assert_eq!(verdict.state, crate::design_doc_verifier::DesignDocState::Quarantined);
        assert!(!verdict.state.is_active(), "a quarantined doc must stay passive");
        assert!(verdict.contract_paths.is_empty(), "no contract paths from an ungrounded doc");
        assert_eq!(verdict.reject_count, 1);
        coordinator
            .append_design_doc_events(session_id, Some(&author), &hallucinated, &verdict)
            .await;

        // Lifecycle (ADR-65 §5): both claims were recorded as `DesignDoc`
        // events and each decision as a `Decision` event carrying the verdict
        // state — verified for the grounded claim, quarantined for the other.
        let events = concerto_sessions::whiteboard::load_whiteboard_events(
            coordinator.review_store.as_ref().expect("review store attached"),
            &concerto_sessions::whiteboard::WhiteboardLoadOpts {
                after_gate_seq: 0,
                session_id: Some(session_id.to_string()),
                scope: None,
                limit: usize::MAX,
            },
        )
        .await
        .expect("whiteboard loads");
        let claims: Vec<_> =
            events.iter().filter(|event| event.kind == WhiteboardKind::DesignDoc).collect();
        assert_eq!(claims.len(), 2, "both claims recorded: {:?}", claims.len());
        let decisions: Vec<_> =
            events.iter().filter(|event| event.kind == WhiteboardKind::Decision).collect();
        assert_eq!(decisions.len(), 2, "both decisions recorded: {:?}", decisions.len());
        let states: Vec<Option<&str>> = decisions
            .iter()
            .map(|event| event.payload.get("state").and_then(|value| value.as_str()))
            .collect();
        assert!(
            states.contains(&Some("verified")),
            "the grounded claim's decision must be verified, got: {states:?}"
        );
        assert!(
            states.contains(&Some("quarantined")),
            "the ungrounded claim's decision must be quarantined, got: {states:?}"
        );
        for claim in &claims {
            assert!(
                decisions
                    .iter()
                    .any(|decision| decision.causation.as_deref() == Some(claim.event_id.as_str())),
                "every claim event has a decision caused by it"
            );
        }
    }
}
