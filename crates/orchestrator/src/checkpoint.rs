//! Orchestration checkpoint for partial-result resume.
//!
//! When `CoordinatorAgent` exits with `completion_status: Partial` the graph
//! is serialised into a `GraphCheckpoint` and stored as JSON.  On "Continue"
//! the checkpoint is deserialised and passed to `run_with_checkpoint`, which
//! skips the expensive `decompose_task` (Architect) phase and resumes the
//! execution loop with the remaining subtasks.

use std::collections::{BTreeMap, HashMap, HashSet};

use concerto_core::types::{
    AgentId, AgentRunResult, DesignDoc, ProviderMetrics, SubTask, SubTaskStatus, TaskId,
};
use concerto_core::OrchestratorError;
use concerto_sessions::whiteboard::{WhiteboardCheckpoint, WhiteboardEvent, WhiteboardKind};
use concerto_sessions::SessionError;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::graph::{Dependency, TaskGraph};

pub const GRAPH_CHECKPOINT_SCHEMA_VERSION: u32 = 3;

/// Last schema version before the current one.  Records written at this
/// version load under the current policy (new fields filled with serde
/// defaults) and are migrated in-memory to the current version on load.
pub const LEGACY_GRAPH_CHECKPOINT_SCHEMA_VERSION: u32 = 2;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum CheckpointStage {
    Planning,
    #[default]
    Executing,
    Validating,
    Completed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanningCheckpoint {
    pub attempt: u32,
    pub last_response: String,
    pub validation_error: String,
}

#[derive(Debug, Clone)]
pub struct CheckpointScope {
    pub run_id: concerto_core::ids::Ulid,
    pub session_id: concerto_core::ids::Ulid,
    pub root_task_id: TaskId,
    pub project_id: String,
    pub objective: String,
    pub objective_hash: String,
    pub source_revision: Option<String>,
    pub sequence_num: u64,
}

/// Evidence recorded when the coordinator accepts (or rejects) a run after
/// the validation loop (audit C-06).
///
/// The generic Freeform coder reports `Success` on any terminal text, so the
/// coordinator alone decides acceptance of a build task: every expected
/// artifact must exist on disk with non-placeholder content and the declared
/// verification (eval) run must have passed. The artifact list and the eval
/// pass are kept here so a resume or audit can see on what evidence a run
/// was accepted.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AcceptanceEvidence {
    /// Artifact paths (relative to the project root) that were verified to
    /// exist with non-placeholder content. Empty when the build task declared
    /// no expected artifacts (vacuous pass).
    pub artifacts: Vec<camino::Utf8PathBuf>,
    /// True when the validation (eval) run passed; false when verification
    /// did not run (no validation-stage agent, or an eval-disabled engine).
    pub verification_passed: bool,
}

/// A single recorded orchestrator decision/action, kept in the checkpoint so a
/// resume can see what was attempted before interruption.
///
/// `kind` is a free-form string so the ledger can grow without a schema bump;
/// current kinds are `"dispatched"`, `"completed"`, `"failed"`, and — for the
/// run-level acceptance decision (C-06) — `"accepted"` and `"rejected"`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CheckpointAction {
    pub kind: String,
    pub task_id: Option<TaskId>,
    pub timestamp: OffsetDateTime,
    /// Acceptance evidence attached to `"accepted"`/`"rejected"` ledger
    /// entries (C-06). Absent on dispatch and per-subtask outcome entries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<AcceptanceEvidence>,
}

/// Coordinator-side state captured into every checkpoint snapshot.
#[derive(Debug, Clone, Default)]
pub struct CheckpointContext {
    pub design_doc: Option<DesignDoc>,
    pub model_assignments: HashMap<TaskId, String>,
    pub action_ledger: Vec<CheckpointAction>,
    /// ADR-42 §4 / ADR-45 ladder guards captured at save time so a resumed
    /// run does not re-walk ladder tiers that already fired before
    /// interruption.
    pub default_model_attempted: HashSet<TaskId>,
    /// ADR-45 tier 1b guard: default-model-on-default-provider re-dispatch
    /// already attempted for the task this run.
    pub default_model_provider_attempted: HashSet<TaskId>,
    pub self_execute_attempted: HashSet<TaskId>,
    pub escalation_attempted: HashSet<TaskId>,
}

// ---------------------------------------------------------------------------
// Serializable graph representation
// ---------------------------------------------------------------------------

/// Lightweight, serializable representation of a single subtask.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointSubTask {
    pub id: TaskId,
    pub parent_id: Option<TaskId>,
    pub session_id: concerto_core::ids::Ulid,
    pub role: AgentId,
    pub description: String,
    pub status: SubTaskStatus,
    pub dependencies: Vec<TaskId>,
    pub deliverable: Option<String>,
    /// Original creation time of the task.  `None` on v2 records (which did
    /// not capture timestamps); restore falls back to "now" in that case.
    #[serde(default)]
    pub created_at: Option<OffsetDateTime>,
    /// Original completion time, if the task completed before the checkpoint.
    #[serde(default)]
    pub completed_at: Option<OffsetDateTime>,
}

/// Complete orchestration checkpoint — everything needed to resume a partial
/// coordinator run without re-running the Architect.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphCheckpoint {
    #[serde(default = "current_schema_version")]
    pub schema_version: u32,
    #[serde(default = "concerto_core::ids::Ulid::new")]
    pub run_id: concerto_core::ids::Ulid,
    #[serde(default = "concerto_core::ids::Ulid::new")]
    pub session_id: concerto_core::ids::Ulid,
    #[serde(default = "TaskId::new")]
    pub root_task_id: TaskId,
    #[serde(default)]
    pub project_id: String,
    #[serde(default)]
    pub objective: String,
    #[serde(default)]
    pub objective_hash: String,
    #[serde(default)]
    pub source_revision: Option<String>,
    #[serde(default)]
    pub sequence_num: u64,
    #[serde(default)]
    pub stage: CheckpointStage,
    #[serde(default)]
    pub completed: bool,
    #[serde(default)]
    pub planning: Option<PlanningCheckpoint>,
    #[serde(default)]
    pub working_memory: Option<concerto_core::WorkingMemorySnapshot>,
    pub subtasks: Vec<CheckpointSubTask>,
    pub edges: Vec<(TaskId, TaskId, Dependency)>,
    pub completed_results: HashMap<TaskId, AgentRunResult>,
    pub total_cost: f64,
    pub total_tool_calls: u32,
    pub provider_metrics: Vec<ProviderMetrics>,
    pub all_files: Vec<camino::Utf8PathBuf>,
    #[serde(default)]
    pub expected_artifacts: HashMap<TaskId, Vec<camino::Utf8PathBuf>>,
    pub subtask_attempts: HashMap<TaskId, u32>,
    pub retry_feedback: HashMap<TaskId, Vec<AgentRunResult>>,
    /// The DesignDoc produced by the Architect (or the latest replan), so a
    /// resume retains the original plan without re-running the Architect.
    #[serde(default)]
    pub design_doc: Option<DesignDoc>,
    /// Task -> model mapping used for the run, for reproducibility.
    #[serde(default)]
    pub model_assignments: HashMap<TaskId, String>,
    /// Ordered record of orchestrator decisions/actions since the run began.
    #[serde(default)]
    pub action_ledger: Vec<CheckpointAction>,
    /// ADR-42 §4 / ADR-45 ladder guards. Captured so a resumed run does NOT
    /// re-walk the ladder: a task that already consumed its tier-1
    /// default-model attempt, its ADR-45 tier-1b default-provider re-dispatch,
    /// its self-execution attempt, or its escalation retry before the
    /// interruption must not get them again after resume. Old checkpoints
    /// without these fields deserialize as empty sets (serde default) and
    /// re-walk the ladder exactly once, which is bounded and safe.
    #[serde(default)]
    pub default_model_attempted: HashSet<TaskId>,
    /// ADR-45 tier 1b guard. Serialized under the historical
    /// `fallback_provider_attempted` key so checkpoints written before the
    /// model-first rename still deserialize (and vice versa: new checkpoints
    /// remain readable by older binaries for the same field).
    #[serde(default, rename = "fallback_provider_attempted")]
    pub default_model_provider_attempted: HashSet<TaskId>,
    #[serde(default)]
    pub self_execute_attempted: HashSet<TaskId>,
    #[serde(default)]
    pub escalation_attempted: HashSet<TaskId>,
}

const fn current_schema_version() -> u32 {
    GRAPH_CHECKPOINT_SCHEMA_VERSION
}

impl GraphCheckpoint {
    /// Deserialize a checkpoint JSON string, applying the schema-version
    /// policy:
    /// - current version (v3) loads as-is;
    /// - legacy v2 records are migrated in-memory to v3 (serde fills the
    ///   new fields with defaults);
    /// - unknown future versions are rejected with a clear error.
    pub fn from_json(json: &str) -> Result<Self, String> {
        let mut checkpoint: GraphCheckpoint = serde_json::from_str(json)
            .map_err(|error| format!("failed to deserialize checkpoint: {error}"))?;
        checkpoint.migrate_schema()?;
        Ok(checkpoint)
    }

    /// Apply the schema-version compatibility policy in place.
    fn migrate_schema(&mut self) -> Result<(), String> {
        match self.schema_version {
            GRAPH_CHECKPOINT_SCHEMA_VERSION => Ok(()),
            LEGACY_GRAPH_CHECKPOINT_SCHEMA_VERSION => {
                // v2 -> v3: serde defaults already filled the new fields
                // (design_doc=None, model_assignments={}, action_ledger=[],
                // per-task timestamps None, ladder guard sets empty). Bump the
                // recorded version so a resaved checkpoint is canonical v3.
                self.schema_version = GRAPH_CHECKPOINT_SCHEMA_VERSION;
                Ok(())
            }
            other => Err(format!(
                "unsupported checkpoint schema version {other}: this runtime supports v{GRAPH_CHECKPOINT_SCHEMA_VERSION} and migrates v{LEGACY_GRAPH_CHECKPOINT_SCHEMA_VERSION}"
            )),
        }
    }

    pub fn validate_scope(
        &self,
        session_id: concerto_core::ids::Ulid,
        project_id: &str,
        source_revision: Option<&str>,
    ) -> Result<(), String> {
        // The current and the last legacy schema version are both resumable;
        // anything else (a future version) must be rejected cleanly.
        if self.schema_version != GRAPH_CHECKPOINT_SCHEMA_VERSION
            && self.schema_version != LEGACY_GRAPH_CHECKPOINT_SCHEMA_VERSION
        {
            return Err(format!(
                "checkpoint schema {} is incompatible with runtime schema {}",
                self.schema_version, GRAPH_CHECKPOINT_SCHEMA_VERSION
            ));
        }
        if self.session_id != session_id {
            return Err("checkpoint belongs to a different session".into());
        }
        if self.project_id != project_id {
            return Err("checkpoint belongs to a different project".into());
        }
        if self.source_revision.as_deref() != source_revision {
            return Err(format!(
                "checkpoint source revision {:?} differs from current revision {:?}",
                self.source_revision.as_deref(),
                source_revision
            ));
        }
        if self.completed || self.stage == CheckpointStage::Completed {
            return Err("checkpoint is already completed".into());
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Conversion helpers
// ---------------------------------------------------------------------------

impl From<&SubTask> for CheckpointSubTask {
    fn from(st: &SubTask) -> Self {
        Self {
            id: st.id,
            parent_id: st.parent_id,
            session_id: st.session_id,
            role: st.role.clone(),
            description: st.description.clone(),
            status: st.status,
            dependencies: st.dependencies.clone(),
            deliverable: st.deliverable.clone(),
            created_at: Some(st.created_at),
            completed_at: st.completed_at,
        }
    }
}

impl CheckpointSubTask {
    /// Convert back into a `SubTask`, preserving the original timestamps so a
    /// resume does not reset progress history.  v2 records (which did not
    /// capture timestamps) fall back to `now` for `created_at` and keep
    /// `completed_at` as `None` since we are resuming.
    pub fn into_subtask(self) -> SubTask {
        SubTask {
            id: self.id,
            parent_id: self.parent_id,
            session_id: self.session_id,
            role: self.role,
            description: self.description,
            status: self.status,
            dependencies: self.dependencies,
            deliverable: self.deliverable,
            created_at: self.created_at.unwrap_or_else(OffsetDateTime::now_utc),
            completed_at: self.completed_at,
        }
    }
}

// ---------------------------------------------------------------------------
// Build a checkpoint from a live TaskGraph + coordinator local state
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
pub fn build_checkpoint(
    scope: &CheckpointScope,
    stage: CheckpointStage,
    planning: Option<PlanningCheckpoint>,
    working_memory: &concerto_core::WorkingMemorySnapshot,
    graph: &TaskGraph,
    completed_results: &HashMap<TaskId, AgentRunResult>,
    total_cost: f64,
    total_tool_calls: u32,
    provider_metrics: &[ProviderMetrics],
    all_files: &[camino::Utf8PathBuf],
    expected_artifacts: &HashMap<TaskId, Vec<camino::Utf8PathBuf>>,
    subtask_attempts: &HashMap<TaskId, u32>,
    retry_feedback: &HashMap<TaskId, Vec<AgentRunResult>>,
    context: &CheckpointContext,
) -> GraphCheckpoint {
    let subtasks: Vec<CheckpointSubTask> =
        graph.all_tasks().into_iter().map(CheckpointSubTask::from).collect();

    let edges = graph.all_edges();

    GraphCheckpoint {
        schema_version: GRAPH_CHECKPOINT_SCHEMA_VERSION,
        run_id: scope.run_id,
        session_id: scope.session_id,
        root_task_id: scope.root_task_id,
        project_id: scope.project_id.clone(),
        objective: scope.objective.clone(),
        objective_hash: scope.objective_hash.clone(),
        source_revision: scope.source_revision.clone(),
        sequence_num: scope.sequence_num,
        stage,
        completed: stage == CheckpointStage::Completed,
        planning,
        working_memory: Some(working_memory.clone()),
        subtasks,
        edges,
        completed_results: completed_results.clone(),
        total_cost,
        total_tool_calls,
        provider_metrics: provider_metrics.to_vec(),
        all_files: all_files.to_vec(),
        expected_artifacts: expected_artifacts.clone(),
        subtask_attempts: subtask_attempts.clone(),
        retry_feedback: retry_feedback.clone(),
        design_doc: context.design_doc.clone(),
        model_assignments: context.model_assignments.clone(),
        action_ledger: context.action_ledger.clone(),
        default_model_attempted: context.default_model_attempted.clone(),
        default_model_provider_attempted: context.default_model_provider_attempted.clone(),
        self_execute_attempted: context.self_execute_attempted.clone(),
        escalation_attempted: context.escalation_attempted.clone(),
    }
}

pub fn build_planning_checkpoint(
    scope: &CheckpointScope,
    working_memory: &concerto_core::WorkingMemorySnapshot,
    planning: PlanningCheckpoint,
) -> GraphCheckpoint {
    build_checkpoint(
        scope,
        CheckpointStage::Planning,
        Some(planning),
        working_memory,
        &TaskGraph::new(),
        &HashMap::new(),
        0.0,
        0,
        &[],
        &[],
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        &CheckpointContext::default(),
    )
}

// ---------------------------------------------------------------------------
// Rebuild a TaskGraph from a checkpoint
// ---------------------------------------------------------------------------

/// Restore a `TaskGraph` from a checkpoint, preserving dependency edges,
/// subtask statuses, and original timestamps.  The returned graph is ready to
/// resume execution.
///
/// Every edge endpoint and every dependency reference must resolve to a task
/// present in the checkpoint; a dangling reference fails the restore with a
/// clear error instead of being silently dropped.
pub fn restore_graph(cp: &GraphCheckpoint) -> Result<TaskGraph, OrchestratorError> {
    use std::collections::HashSet;

    let mut graph = TaskGraph::new();
    let ids: HashSet<TaskId> = cp.subtasks.iter().map(|cst| cst.id).collect();

    // First pass: add every subtask with its current status.  A subtask that
    // lists a dependency on a task absent from the checkpoint is corrupt —
    // reject it rather than resuming with a silently truncated dependency set.
    for cst in &cp.subtasks {
        let mut st = cst.clone().into_subtask();
        if st.status == SubTaskStatus::Running {
            st.status = SubTaskStatus::Pending;
        }
        for dep in &st.dependencies {
            if !ids.contains(dep) {
                return Err(OrchestratorError::TaskGraphError(format!(
                    "checkpoint restore: subtask {} references dependency {} which is missing from the checkpoint",
                    st.id, dep
                )));
            }
        }
        graph.add_subtask(st);
    }

    // Second pass: connect dependency edges.  Both endpoints must already
    // exist; a dangling edge is a hard error, never a silent drop.
    // add_dependency expects (task, depends_on, dep), so from depends on to.
    for (from, to, dep) in &cp.edges {
        restore_edge(&mut graph, &ids, *from, *to, *dep)?;
    }

    Ok(graph)
}

/// Validate and add a single checkpoint edge to the graph being restored.
/// Both endpoints must exist in the checkpoint (dangling references are
/// rejected with a clear error), and any `add_dependency` failure is surfaced
/// with restore context instead of being silently dropped.
fn restore_edge(
    graph: &mut TaskGraph,
    ids: &std::collections::HashSet<TaskId>,
    from: TaskId,
    to: TaskId,
    dep: Dependency,
) -> Result<(), OrchestratorError> {
    if !ids.contains(&from) || !ids.contains(&to) {
        return Err(OrchestratorError::TaskGraphError(format!(
            "checkpoint restore: edge {from} -> {to} references a task missing from the checkpoint"
        )));
    }
    graph.add_dependency(to, from, dep).map_err(|error| {
        OrchestratorError::TaskGraphError(format!(
            "checkpoint restore: failed to add dependency {from} -> {to}: {error}"
        ))
    })
}

// ---------------------------------------------------------------------------
// ADR-60 D5: gate-boundary checkpoints & per-agent revert
// ---------------------------------------------------------------------------

/// A gate-boundary checkpoint (ADR-60 D5 (i)): the projected file state of a
/// run pinned to a consistent cut of the whiteboard log.
///
/// `gate_seq` is the consistent-cut coordinate — "everything ≤ seq S" — and
/// `files` is that cut replayed through the log's total order: for every
/// path, the content of the last applied write at or before S. Restoring a
/// later state means *restore = snapshot + replay tail*
/// ([`GateBoundaryCheckpoint::replay_tail_excluding`]); per-agent revert is
/// restore + tail replay skipping one agent's `event_id`s (D5 (ii)) — never a
/// last-action undo, always an attribution-aware log replay.
///
/// The checkpoint is a **projection, not a second store**: every folded event
/// was committed WAL-first by the write gate before its tool executed (the D4
/// ordering invariant), so the cut survives any crash without separate
/// storage and the raw log is never truncated or rewritten. Materialize one
/// from the durable log via [`crate::gate::WriteGate::create_checkpoint_at`],
/// or from an in-memory event slice via [`GateBoundaryCheckpoint::at_cut`].
///
/// Replay scope: applied filesystem **write** operations only — the op set
/// the per-agent-revert e2e fixture (`supervisor_parallel_e2e.rs`) exercises,
/// from which this API was promoted. Move/copy/delete revert remains future
/// work; non-write rows are skipped, not misapplied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateBoundaryCheckpoint {
    /// The inclusive consistent-cut coordinate: every folded event has
    /// `gate_seq <= gate_seq`; every event after it belongs to the replay
    /// tail.
    pub gate_seq: u64,
    /// Session filter the cut was taken under (`None` = whole log). Purely
    /// informational here — filtering happens where events are loaded.
    pub session_id: Option<String>,
    /// Projected file state at the cut: relative path → content.
    pub files: BTreeMap<String, String>,
}

impl GateBoundaryCheckpoint {
    /// Fold an ordered event slice into the snapshot at cut `gate_seq`.
    ///
    /// `events` must be ordered ascending by `gate_seq` — the order every
    /// whiteboard reader returns, and the total order the replay semantics
    /// depend on. Events with `gate_seq > gate_seq` are ignored (they are the
    /// tail, not part of this cut).
    pub fn at_cut(events: &[WhiteboardEvent], gate_seq: u64, session_id: Option<String>) -> Self {
        let mut files = BTreeMap::new();
        for event in events.iter().filter(|event| event.gate_seq <= gate_seq) {
            apply_write_event(&mut files, event);
        }
        Self { gate_seq, session_id, files }
    }

    /// D5 (ii): restore to this snapshot, then replay the log tail — every
    /// event with `gate_seq > self.gate_seq` **excluding** `exclude_agent`'s
    /// rows — over it, returning the resulting file state.
    ///
    /// `exclude_agent = None` replays the whole tail (plain restore-forward).
    /// The result is the same map shape as [`Self::files`] so callers can
    /// diff, verify, or materialize it.
    pub fn replay_tail_excluding(
        &self,
        events: &[WhiteboardEvent],
        exclude_agent: Option<&str>,
    ) -> BTreeMap<String, String> {
        let mut files = self.files.clone();
        for event in events.iter().filter(|event| event.gate_seq > self.gate_seq) {
            if exclude_agent.is_some_and(|agent| event.agent_id == agent) {
                continue;
            }
            apply_write_event(&mut files, event);
        }
        files
    }
}

/// Per-agent revert (ADR-60 D5 (ii)) in one call over a log slice: restore to
/// the consistent cut at `checkpoint_seq`, then replay the tail excluding
/// `exclude_agent`'s `event_ids`, returning the final file state.
///
/// The exclusion filters **the tail only** — a checkpoint is, by definition,
/// state that predates what is being reverted, so to exclude an agent across
/// the whole log pass `checkpoint_seq = 0` (empty restore point; this is what
/// the promoted `supervisor_parallel_e2e` fixture uses). The degenerate
/// `checkpoint_seq = u64::MAX` with `exclude_agent = None` is the full-log
/// replay whose last writer per path is the log's verdict. For a stored
/// snapshot object use [`GateBoundaryCheckpoint::replay_tail_excluding`]
/// instead of re-folding the prefix.
pub fn revert_excluding_agent(
    events: &[WhiteboardEvent],
    exclude_agent: Option<&str>,
    checkpoint_seq: u64,
) -> BTreeMap<String, String> {
    GateBoundaryCheckpoint::at_cut(events, checkpoint_seq, None)
        .replay_tail_excluding(events, exclude_agent)
}

// ---------------------------------------------------------------------------
// Durable store round-trip (ADR-60 D5 (i)): persist / load
// ---------------------------------------------------------------------------

/// Why a gate-boundary checkpoint could not be persisted or reloaded.
#[derive(Debug)]
pub enum CheckpointStoreError {
    /// SQLite persistence failure surfaced by the sessions crate.
    Storage(SessionError),
    /// The snapshot could not be serialized to its stored JSON form.
    Serialize(serde_json::Error),
    /// A stored snapshot row exists but its payload no longer deserializes
    /// into a [`GateBoundaryCheckpoint`] (schema drift or corruption).
    Deserialize { gate_seq: u64, reason: String },
    /// The write gate refused to materialize the cut.
    Gate(String),
}

impl std::fmt::Display for CheckpointStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Storage(error) => write!(f, "checkpoint store error: {error}"),
            Self::Serialize(error) => {
                write!(f, "failed to serialize checkpoint snapshot: {error}")
            }
            Self::Deserialize { gate_seq, reason } => {
                write!(f, "stored checkpoint at gate_seq {gate_seq} is unreadable: {reason}")
            }
            Self::Gate(reason) => write!(f, "gate refused to materialize the checkpoint: {reason}"),
        }
    }
}

impl std::error::Error for CheckpointStoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Storage(error) => Some(error),
            Self::Serialize(error) => Some(error),
            Self::Deserialize { .. } | Self::Gate(_) => None,
        }
    }
}

impl From<SessionError> for CheckpointStoreError {
    fn from(error: SessionError) -> Self {
        Self::Storage(error)
    }
}

impl From<serde_json::Error> for CheckpointStoreError {
    fn from(error: serde_json::Error) -> Self {
        Self::Serialize(error)
    }
}

/// Persist a [`GateBoundaryCheckpoint`] into the sessions whiteboard
/// checkpoint store (`whiteboard_checkpoints`, migration 028), pinned to the
/// checkpoint's own `gate_seq` (the consistent-cut coordinate).
///
/// The store is a projection, never a second source of truth: every folded
/// event already lives WAL-first in the log, so a failed persist loses
/// nothing — restore can always be rebuilt from the raw log.
pub async fn persist_gate_boundary_checkpoint(
    pool: &sqlx::SqlitePool,
    checkpoint: &GateBoundaryCheckpoint,
) -> Result<WhiteboardCheckpoint, CheckpointStoreError> {
    let snapshot = serde_json::to_string(checkpoint)?;
    Ok(concerto_sessions::whiteboard::create_whiteboard_checkpoint(
        pool,
        checkpoint.gate_seq,
        &snapshot,
    )
    .await?)
}

/// Load the latest persisted checkpoint at or before `gate_seq`, deserialized
/// back into a typed [`GateBoundaryCheckpoint`]. The read-side of restart
/// restore: **read-only** — this returns the projected file state, it never
/// materializes anything to disk (see `Supervisor::checkpoint_at_shutdown`
/// for the write side and the documented restore gap).
///
/// `None` means no stored checkpoint exists at or before `gate_seq`.
pub async fn load_gate_boundary_checkpoint(
    pool: &sqlx::SqlitePool,
    gate_seq: u64,
) -> Result<Option<(WhiteboardCheckpoint, GateBoundaryCheckpoint)>, CheckpointStoreError> {
    let Some(record) =
        concerto_sessions::whiteboard::load_whiteboard_checkpoint_by_gate_seq(pool, gate_seq)
            .await?
    else {
        return Ok(None);
    };
    let checkpoint = serde_json::from_str(&record.snapshot).map_err(|reason| {
        CheckpointStoreError::Deserialize { gate_seq: record.gate_seq, reason: reason.to_string() }
    })?;
    Ok(Some((record, checkpoint)))
}

/// Fold one whiteboard row into the projected file state if it is an applied
/// filesystem write carrying a usable path/content pair; anything else is
/// skipped. A malformed payload on an applied-write row degrades to a
/// `debug!`-logged skip (observable), never a hard failure — the projection
/// must stay reconstructible from any historical log.
fn apply_write_event(files: &mut BTreeMap<String, String>, event: &WhiteboardEvent) {
    if event.kind != WhiteboardKind::WriteApplied {
        return;
    }
    let Some(input) = event.payload.get("input") else {
        tracing::debug!(
            target: "concerto_orchestrator::checkpoint",
            event_id = %event.event_id,
            "checkpoint replay: applied write without input payload; row skipped"
        );
        return;
    };
    // Slice scope (see [`GateBoundaryCheckpoint`]): writes only, so
    // move/copy/delete rows are left for their dedicated revert support.
    if input.get("operation").and_then(serde_json::Value::as_str) != Some("write") {
        return;
    }
    let path = input.get("path").and_then(serde_json::Value::as_str);
    let content = input.get("content").and_then(serde_json::Value::as_str);
    match (path, content) {
        (Some(path), Some(content)) => {
            files.insert(path.to_owned(), content.to_owned());
        }
        (path, content) => {
            tracing::debug!(
                target: "concerto_orchestrator::checkpoint",
                event_id = %event.event_id,
                ?path,
                ?content,
                "checkpoint replay: applied write missing path or string content; row skipped"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::Dependency;
    use concerto_core::ids::Ulid;
    use concerto_core::types::{AgentId, AgentOutcome, AgentRunResult, ProviderMetrics};

    /// Build a minimal checkpoint for round-trip tests.
    fn build_minimal_checkpoint(graph: &TaskGraph) -> GraphCheckpoint {
        build_checkpoint(
            &CheckpointScope {
                run_id: Ulid::new(),
                session_id: Ulid::new(),
                root_task_id: TaskId::new(),
                project_id: "test".into(),
                objective: "test objective".into(),
                objective_hash: "hash".into(),
                source_revision: None,
                sequence_num: 0,
            },
            CheckpointStage::Executing,
            None,
            &concerto_core::memory::WorkingMemorySnapshot {
                id: Ulid::new(),
                session_id: Ulid::new(),
                decisions: vec![],
                task_tree: vec![],
                created_at: time::OffsetDateTime::now_utc(),
            },
            graph,
            &HashMap::new(),
            0.0,
            0,
            &[],
            &[],
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            &CheckpointContext::default(),
        )
    }

    fn make_subtask(label: &str, role: AgentId, deps: Vec<TaskId>) -> SubTask {
        SubTask {
            id: TaskId::new(),
            parent_id: None,
            session_id: Ulid::new(),
            role,
            description: label.into(),
            status: SubTaskStatus::Pending,
            dependencies: deps,
            deliverable: None,
            created_at: time::OffsetDateTime::now_utc(),
            completed_at: None,
        }
    }

    // ------------------------------------------------------------------
    // Empty graph
    // ------------------------------------------------------------------

    #[test]
    fn empty_graph_round_trip() {
        let graph = TaskGraph::new();
        let cp = build_minimal_checkpoint(&graph);
        let restored = restore_graph(&cp).expect("restore graph");
        assert!(restored.all_tasks().is_empty(), "empty graph restored as empty");
    }

    // ------------------------------------------------------------------
    // Single task
    // ------------------------------------------------------------------

    #[test]
    fn single_task_round_trip() {
        let mut graph = TaskGraph::new();
        let task = make_subtask("analyze", AgentId::new("researcher"), vec![]);
        graph.add_root(task.clone());

        let cp = build_minimal_checkpoint(&graph);
        let restored = restore_graph(&cp).expect("restore graph");

        let restored_tasks = restored.all_tasks();
        assert_eq!(restored_tasks.len(), 1);
        assert_eq!(restored_tasks[0].id, task.id);
        assert_eq!(restored_tasks[0].role, AgentId::new("researcher"));
        assert_eq!(restored_tasks[0].description, "analyze");
    }

    // ------------------------------------------------------------------
    // Dependency edges
    // ------------------------------------------------------------------

    #[test]
    fn dependency_edges_survive_round_trip() {
        let mut graph = TaskGraph::new();
        let root = make_subtask("design", AgentId::new("architect"), vec![]);
        let child = make_subtask("implement", AgentId::new("coder"), vec![root.id]);

        graph.add_root(root.clone());
        graph.add_child(child.clone(), root.id, Dependency::MustFinishBefore);

        let cp = build_minimal_checkpoint(&graph);
        let restored = restore_graph(&cp).expect("restore graph");

        let restored_tasks = restored.all_tasks();
        assert_eq!(restored_tasks.len(), 2);

        // Both tasks should be present by ID.
        assert!(restored_tasks.iter().any(|t| t.id == root.id));
        assert!(restored_tasks.iter().any(|t| t.id == child.id));
    }

    #[test]
    fn chained_dependencies_restored_in_order() {
        let mut graph = TaskGraph::new();
        let a = make_subtask("A", AgentId::new("architect"), vec![]);
        let b = make_subtask("B", AgentId::new("coder"), vec![a.id]);
        let c = make_subtask("C", AgentId::new("reviewer"), vec![b.id]);

        graph.add_root(a.clone());
        graph.add_child(b.clone(), a.id, Dependency::MustFinishBefore);
        graph.add_child(c.clone(), b.id, Dependency::MustFinishBefore);

        let cp = build_minimal_checkpoint(&graph);
        let restored = restore_graph(&cp).expect("restore graph");

        // All three tasks present.
        let ids: Vec<TaskId> = restored.all_tasks().into_iter().map(|t| t.id).collect();
        assert_eq!(ids.len(), 3);
        assert!(ids.contains(&a.id));
        assert!(ids.contains(&b.id));
        assert!(ids.contains(&c.id));

        // Dependencies are preserved: B depends on A, C depends on B.
        let b_task = restored.all_tasks().into_iter().find(|t| t.id == b.id).unwrap();
        let c_task = restored.all_tasks().into_iter().find(|t| t.id == c.id).unwrap();
        assert!(b_task.dependencies.contains(&a.id), "B should depend on A");
        assert!(c_task.dependencies.contains(&b.id), "C should depend on B");
    }

    // ------------------------------------------------------------------
    // Subtask status preservation
    // ------------------------------------------------------------------

    #[test]
    fn subtask_statuses_preserved() {
        let mut graph = TaskGraph::new();
        let mut completed = make_subtask("done", AgentId::new("coder"), vec![]);
        completed.status = SubTaskStatus::Completed;
        let mut failed = make_subtask("broken", AgentId::new("reviewer"), vec![completed.id]);
        failed.status = SubTaskStatus::Failed;
        graph.add_root(completed.clone());
        graph.add_child(failed.clone(), completed.id, Dependency::MustFinishBefore);

        let cp = build_minimal_checkpoint(&graph);
        let restored = restore_graph(&cp).expect("restore graph");
        let tasks = restored.all_tasks();

        let restored_completed = tasks.iter().find(|t| t.description == "done").unwrap();
        let restored_failed = tasks.iter().find(|t| t.description == "broken").unwrap();
        assert_eq!(restored_completed.status, SubTaskStatus::Completed);
        assert_eq!(restored_failed.status, SubTaskStatus::Failed);
    }

    // ------------------------------------------------------------------
    // Metadata fields
    // ------------------------------------------------------------------

    #[test]
    fn checkpoint_metadata_preserved() {
        let graph = TaskGraph::new();
        let cp = build_checkpoint(
            &CheckpointScope {
                run_id: Ulid::new(),
                session_id: Ulid::new(),
                root_task_id: TaskId::new(),
                project_id: "test".into(),
                objective: "test objective".into(),
                objective_hash: "hash".into(),
                source_revision: None,
                sequence_num: 0,
            },
            CheckpointStage::Executing,
            None,
            &concerto_core::memory::WorkingMemorySnapshot {
                id: Ulid::new(),
                session_id: Ulid::new(),
                decisions: vec![],
                task_tree: vec![],
                created_at: time::OffsetDateTime::now_utc(),
            },
            &graph,
            &HashMap::new(),
            42.5,
            17,
            &[ProviderMetrics {
                provider: "test".into(),
                model: "gpt-4".into(),
                tokens_in: 1000,
                tokens_out: 200,
                cost_usd: 0.05,
                latency_ms: 1500,
            }],
            &[camino::Utf8PathBuf::from("/tmp/a.rs"), camino::Utf8PathBuf::from("/tmp/b.rs")],
            &HashMap::new(),
            &HashMap::from([(TaskId::new(), 2)]),
            &HashMap::new(),
            &CheckpointContext::default(),
        );

        assert!((cp.total_cost - 42.5).abs() < f64::EPSILON, "total_cost preserved");
        assert_eq!(cp.total_tool_calls, 17, "total_tool_calls preserved");
        assert_eq!(cp.provider_metrics.len(), 1, "provider_metrics preserved");
        assert_eq!(cp.all_files.len(), 2, "all_files preserved");
        assert_eq!(*cp.subtask_attempts.values().next().unwrap(), 2, "subtask_attempts preserved");
    }

    // ------------------------------------------------------------------
    // Completed results round-trip
    // ------------------------------------------------------------------

    #[test]
    fn completed_results_survive_round_trip() {
        let graph = TaskGraph::new();
        let task_id = TaskId::new();
        let result = AgentRunResult {
            task_id,
            role: AgentId::new("coder"),
            outcome: AgentOutcome::Success,
            summary: "implemented feature".into(),
            files_modified: vec![camino::Utf8PathBuf::from("src/main.rs")],
            tool_call_count: 5,
            cost_usd: 0.10,
            latency_ms: 2000,
            provider: "openai".into(),
            model: "gpt-4".into(),
            tokens_in: 500,
            tokens_out: 300,
        };
        let mut results = HashMap::new();
        results.insert(task_id, result.clone());

        let cp = build_checkpoint(
            &CheckpointScope {
                run_id: Ulid::new(),
                session_id: Ulid::new(),
                root_task_id: TaskId::new(),
                project_id: "test".into(),
                objective: "test objective".into(),
                objective_hash: "hash".into(),
                source_revision: None,
                sequence_num: 0,
            },
            CheckpointStage::Executing,
            None,
            &concerto_core::memory::WorkingMemorySnapshot {
                id: Ulid::new(),
                session_id: Ulid::new(),
                decisions: vec![],
                task_tree: vec![],
                created_at: time::OffsetDateTime::now_utc(),
            },
            &graph,
            &results,
            1.0,
            10,
            &[],
            &[],
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            &CheckpointContext::default(),
        );
        let restored_result = cp.completed_results.get(&task_id).unwrap();

        assert_eq!(restored_result.outcome, AgentOutcome::Success);
        assert_eq!(restored_result.summary, "implemented feature");
        assert_eq!(restored_result.tool_call_count, 5);
    }

    // ------------------------------------------------------------------
    // JSON round-trip (serde)
    // ------------------------------------------------------------------

    #[test]
    fn checkpoint_json_round_trip() {
        let graph = TaskGraph::new();
        let mut attempts = HashMap::new();
        attempts.insert(TaskId::new(), 3);
        let cp = build_checkpoint(
            &CheckpointScope {
                run_id: Ulid::new(),
                session_id: Ulid::new(),
                root_task_id: TaskId::new(),
                project_id: "test".into(),
                objective: "test objective".into(),
                objective_hash: "hash".into(),
                source_revision: None,
                sequence_num: 0,
            },
            CheckpointStage::Executing,
            None,
            &concerto_core::memory::WorkingMemorySnapshot {
                id: Ulid::new(),
                session_id: Ulid::new(),
                decisions: vec![],
                task_tree: vec![],
                created_at: time::OffsetDateTime::now_utc(),
            },
            &graph,
            &HashMap::new(),
            99.9,
            42,
            &[],
            &[],
            &HashMap::new(),
            &attempts,
            &HashMap::new(),
            &CheckpointContext::default(),
        );

        let json = serde_json::to_string(&cp).expect("serialize checkpoint");
        let deserialized: GraphCheckpoint =
            serde_json::from_str(&json).expect("deserialize checkpoint");

        assert!((deserialized.total_cost - 99.9).abs() < f64::EPSILON);
        assert_eq!(deserialized.total_tool_calls, 42);
    }

    // ------------------------------------------------------------------
    // SubTask conversion
    // ------------------------------------------------------------------

    #[test]
    fn checkpoint_subtask_conversion_round_trip() {
        let original = SubTask {
            id: TaskId::new(),
            parent_id: None,
            session_id: Ulid::new(),
            role: AgentId::new("coder"),
            description: "implement login".into(),
            status: SubTaskStatus::Running,
            dependencies: vec![TaskId::new()],
            deliverable: Some("auth module".into()),
            created_at: time::OffsetDateTime::now_utc(),
            completed_at: Some(time::OffsetDateTime::now_utc()),
        };

        let cst = CheckpointSubTask::from(&original);
        assert_eq!(cst.id, original.id);
        assert_eq!(cst.description, "implement login");

        let restored = cst.into_subtask();
        assert_eq!(restored.id, original.id);
        assert_eq!(restored.description, original.description);
        assert_eq!(restored.status, original.status);
        assert_eq!(restored.deliverable, original.deliverable);
        // Timestamps are preserved, not reset, so a resume keeps the
        // original progress history (C-05).
        assert_eq!(restored.created_at, original.created_at);
        assert_eq!(restored.completed_at, original.completed_at);
    }

    #[test]
    fn v2_subtask_without_timestamps_falls_back_to_now() {
        let cst = CheckpointSubTask {
            id: TaskId::new(),
            parent_id: None,
            session_id: Ulid::new(),
            role: AgentId::new("coder"),
            description: "legacy task".into(),
            status: SubTaskStatus::Pending,
            dependencies: vec![],
            deliverable: None,
            // v2 records did not capture timestamps.
            created_at: None,
            completed_at: None,
        };
        let restored = cst.into_subtask();
        assert!(restored.created_at.unix_timestamp() > 0, "v2 created_at falls back to now");
        assert!(restored.completed_at.is_none(), "v2 completed_at stays None");
    }

    // ------------------------------------------------------------------
    // C-05: v3 fields round-trip
    // ------------------------------------------------------------------

    #[test]
    fn v3_fields_survive_json_round_trip() {
        let mut graph = TaskGraph::new();
        let mut task = make_subtask("analyze", AgentId::new("researcher"), vec![]);
        task.created_at = time::OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
        task.completed_at = Some(time::OffsetDateTime::from_unix_timestamp(1_700_000_100).unwrap());
        graph.add_root(task.clone());

        let task_id = task.id;
        let design_doc = DesignDoc {
            goals: vec!["build x".into()],
            constraints: vec!["no deps".into()],
            proposed_files: vec![camino::Utf8PathBuf::from("src/x.rs")],
            interface_sketch: "fn x()".into(),
            risks: vec!["scale".into()],
        };
        let mut model_assignments = HashMap::new();
        model_assignments.insert(task_id, "gpt-4o".to_string());
        let action_ledger = vec![
            CheckpointAction {
                kind: "dispatched".into(),
                task_id: Some(task_id),
                timestamp: time::OffsetDateTime::from_unix_timestamp(1_700_000_010).unwrap(),
                evidence: None,
            },
            CheckpointAction {
                kind: "completed".into(),
                task_id: Some(task_id),
                timestamp: time::OffsetDateTime::from_unix_timestamp(1_700_000_100).unwrap(),
                evidence: None,
            },
        ];

        let cp = build_checkpoint(
            &CheckpointScope {
                run_id: Ulid::new(),
                session_id: Ulid::new(),
                root_task_id: TaskId::new(),
                project_id: "test".into(),
                objective: "test objective".into(),
                objective_hash: "hash".into(),
                source_revision: None,
                sequence_num: 0,
            },
            CheckpointStage::Executing,
            None,
            &concerto_core::memory::WorkingMemorySnapshot {
                id: Ulid::new(),
                session_id: Ulid::new(),
                decisions: vec![],
                task_tree: vec![],
                created_at: time::OffsetDateTime::now_utc(),
            },
            &graph,
            &HashMap::new(),
            0.0,
            0,
            &[],
            &[],
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            &CheckpointContext {
                design_doc: Some(design_doc.clone()),
                model_assignments: model_assignments.clone(),
                action_ledger: action_ledger.clone(),
                default_model_provider_attempted: HashSet::new(),
                default_model_attempted: HashSet::from([task_id]),
                self_execute_attempted: HashSet::new(),
                escalation_attempted: HashSet::new(),
            },
        );
        assert_eq!(cp.schema_version, GRAPH_CHECKPOINT_SCHEMA_VERSION);

        let json = serde_json::to_string(&cp).unwrap();
        let loaded = GraphCheckpoint::from_json(&json).unwrap();

        // DesignDoc, model assignments, and the action ledger survive.
        assert_eq!(
            serde_json::to_value(&loaded.design_doc).unwrap(),
            serde_json::to_value(Some(design_doc)).unwrap(),
            "design_doc preserved"
        );
        assert_eq!(
            loaded.model_assignments.get(&task_id).map(String::as_str),
            Some("gpt-4o"),
            "model assignments preserved"
        );
        assert_eq!(loaded.action_ledger, action_ledger, "action ledger preserved");
        // ADR-42 §4 ladder guards round-trip: a resumed run must not re-walk
        // tiers that already fired before the interruption.
        assert_eq!(
            loaded.default_model_attempted,
            HashSet::from([task_id]),
            "default-model guard preserved"
        );
        assert!(
            loaded.self_execute_attempted.is_empty() && loaded.escalation_attempted.is_empty(),
            "empty guard sets preserved"
        );

        // Original timestamps survive the restore.
        let restored = restore_graph(&loaded).unwrap();
        let restored_task = restored.get(&task_id).unwrap();
        assert_eq!(restored_task.created_at, task.created_at, "created_at preserved");
        assert_eq!(restored_task.completed_at, task.completed_at, "completed_at preserved");
    }

    #[test]
    fn legacy_fallback_provider_attempted_key_maps_to_default_model_guard() {
        // ADR-45: the tier-1b guard serializes under its historical key
        // `fallback_provider_attempted`. A checkpoint written before the
        // model-first rename must deserialize into the renamed
        // `default_model_provider_attempted` field.
        let graph = TaskGraph::new();
        let task_id = TaskId::new();

        let mut checkpoint = build_minimal_checkpoint(&graph);
        checkpoint.default_model_provider_attempted = HashSet::from([task_id]);
        let json = serde_json::to_string(&checkpoint).unwrap();
        assert!(
            json.contains("\"fallback_provider_attempted\""),
            "the guard must serialize under the legacy key: {json}"
        );
        let loaded = GraphCheckpoint::from_json(&json).unwrap();
        assert_eq!(
            loaded.default_model_provider_attempted,
            HashSet::from([task_id]),
            "the legacy-key value must land in default_model_provider_attempted"
        );

        // Absent key: serde default fills an empty set, so a resumed run
        // re-walks the tier-1b guard exactly once (bounded and safe).
        let json_without_key = serde_json::to_string(&build_minimal_checkpoint(&graph)).unwrap();
        assert!(
            json_without_key.contains("\"fallback_provider_attempted\":[]"),
            "an empty guard still serializes under the legacy key: {json_without_key}"
        );
        let json_without_key = json_without_key.replace("\"fallback_provider_attempted\":[],", "");
        assert!(
            !json_without_key.contains("fallback_provider_attempted"),
            "key must be stripped to exercise the serde default path"
        );
        let loaded_default = GraphCheckpoint::from_json(&json_without_key).unwrap();
        assert!(
            loaded_default.default_model_provider_attempted.is_empty(),
            "a missing key must deserialize as an empty guard set"
        );
    }

    #[test]
    fn restore_yields_semantically_identical_state() {
        // Scenario 7 acceptance bar: "Continue" restores the identical graph,
        // artifact requirements, timestamps, assignments, and completed
        // results.
        //
        // Known divergence (documented in docs/audits/AUDIT_FINDINGS_CURRENT.md,
        // C-05): `model_assignments` captured in a checkpoint lags one batch —
        // the coordinator inserts each task's assignment only after its
        // batch's results are processed, so a checkpoint taken mid-run
        // captures assignments up to the previous batch. That lag is NOT
        // fixed here; we assert that whatever assignments ARE captured
        // round-trip exactly. (Resume re-selects models, so the lag is
        // informational.)
        let mut graph = TaskGraph::new();

        // Fixed timestamps so the round-trip is exact — never "now".
        let t_created = time::OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
        let t_completed = time::OffsetDateTime::from_unix_timestamp(1_700_000_300).unwrap();
        let t_dispatch = time::OffsetDateTime::from_unix_timestamp(1_700_000_010).unwrap();
        let t_accepted = time::OffsetDateTime::from_unix_timestamp(1_700_000_400).unwrap();
        let t_rejected = time::OffsetDateTime::from_unix_timestamp(1_700_000_500).unwrap();

        // t1: completed, with outcome + exact completion timestamp.
        let mut done = make_subtask("implement", AgentId::new("coder"), vec![]);
        done.created_at = t_created;
        done.completed_at = Some(t_completed);
        done.status = SubTaskStatus::Completed;
        let done_id = done.id;
        graph.add_root(done.clone());

        // t2: still pending, depends on t1.
        let pending = make_subtask("review", AgentId::new("reviewer"), vec![done_id]);
        let pending_id = pending.id;
        graph.add_child(pending.clone(), done_id, Dependency::MustFinishBefore);

        // t3: failed (terminal, non-recoverable).
        let mut failed = make_subtask("research", AgentId::new("researcher"), vec![]);
        failed.status = SubTaskStatus::Failed;
        let failed_id = failed.id;
        graph.add_root(failed.clone());

        // t4: blocked on the failed task.
        let mut blocked = make_subtask("docs", AgentId::new("writer"), vec![failed_id]);
        blocked.status = SubTaskStatus::Blocked;
        let blocked_id = blocked.id;
        graph.add_child(blocked.clone(), failed_id, Dependency::MustFinishBefore);

        // Completed result for the completed task.
        let result = AgentRunResult {
            task_id: done_id,
            role: AgentId::new("coder"),
            outcome: AgentOutcome::Success,
            summary: "implemented feature".into(),
            files_modified: vec![camino::Utf8PathBuf::from("src/main.rs")],
            tool_call_count: 5,
            cost_usd: 0.10,
            latency_ms: 2000,
            provider: "openai".into(),
            model: "gpt-4o".into(),
            tokens_in: 500,
            tokens_out: 300,
        };
        let mut completed_results = HashMap::new();
        completed_results.insert(done_id, result.clone());

        // Artifact requirements: design doc proposed_files + the per-task
        // expected-artifact map.
        let design_doc = DesignDoc {
            goals: vec!["build x".into()],
            constraints: vec!["no deps".into()],
            proposed_files: vec![
                camino::Utf8PathBuf::from("src/main.rs"),
                camino::Utf8PathBuf::from("README.md"),
            ],
            interface_sketch: "fn main()".into(),
            risks: vec!["scale".into()],
        };
        let expected_artifacts = HashMap::from([
            (done_id, vec![camino::Utf8PathBuf::from("src/main.rs")]),
            (pending_id, vec![camino::Utf8PathBuf::from("src/lib.rs")]),
        ]);

        // Model assignments captured so far (one-batch lag caveat above).
        let model_assignments = HashMap::from([
            (done_id, "gpt-4o".to_string()),
            (failed_id, "claude-3-5-sonnet".to_string()),
        ]);

        // Action ledger: dispatched, completed, accepted (with evidence),
        // rejected (with evidence).
        let action_ledger = vec![
            CheckpointAction {
                kind: "dispatched".into(),
                task_id: Some(done_id),
                timestamp: t_dispatch,
                evidence: None,
            },
            CheckpointAction {
                kind: "completed".into(),
                task_id: Some(done_id),
                timestamp: t_completed,
                evidence: None,
            },
            CheckpointAction {
                kind: "accepted".into(),
                task_id: Some(done_id),
                timestamp: t_accepted,
                evidence: Some(AcceptanceEvidence {
                    artifacts: vec![camino::Utf8PathBuf::from("src/main.rs")],
                    verification_passed: true,
                }),
            },
            CheckpointAction {
                kind: "rejected".into(),
                task_id: Some(pending_id),
                timestamp: t_rejected,
                evidence: Some(AcceptanceEvidence {
                    artifacts: vec![],
                    verification_passed: false,
                }),
            },
        ];

        let scope = CheckpointScope {
            run_id: Ulid::new(),
            session_id: Ulid::new(),
            root_task_id: done_id,
            project_id: "test".into(),
            objective: "test objective".into(),
            objective_hash: "hash".into(),
            source_revision: None,
            sequence_num: 0,
        };
        let working_memory = concerto_core::memory::WorkingMemorySnapshot {
            id: Ulid::new(),
            session_id: Ulid::new(),
            decisions: vec![],
            task_tree: vec![],
            created_at: time::OffsetDateTime::now_utc(),
        };
        let cp = build_checkpoint(
            &scope,
            CheckpointStage::Executing,
            None,
            &working_memory,
            &graph,
            &completed_results,
            1.0,
            10,
            &[],
            &[camino::Utf8PathBuf::from("src/main.rs")],
            &expected_artifacts,
            &HashMap::new(),
            &HashMap::new(),
            &CheckpointContext {
                design_doc: Some(design_doc.clone()),
                model_assignments: model_assignments.clone(),
                action_ledger: action_ledger.clone(),
                default_model_provider_attempted: HashSet::new(),
                default_model_attempted: HashSet::new(),
                self_execute_attempted: HashSet::new(),
                escalation_attempted: HashSet::new(),
            },
        );

        // Serialize -> fallible load (the production restore path) -> graph.
        let json = serde_json::to_string(&cp).unwrap();
        let loaded = GraphCheckpoint::from_json(&json).unwrap();
        let restored = restore_graph(&loaded).unwrap();

        // Task statuses identical (Completed/Failed/Blocked stay terminal,
        // Pending stays pending; only Running would degrade to Pending).
        assert_eq!(restored.get(&done_id).unwrap().status, SubTaskStatus::Completed);
        assert_eq!(restored.get(&pending_id).unwrap().status, SubTaskStatus::Pending);
        assert_eq!(restored.get(&failed_id).unwrap().status, SubTaskStatus::Failed);
        assert_eq!(restored.get(&blocked_id).unwrap().status, SubTaskStatus::Blocked);

        // Completion timestamps exact — v3 round-trips, never fallback-now.
        // (v2 records carry no timestamps and fall back to `now` on restore;
        // that documented fallback is covered by
        // `v2_subtask_without_timestamps_falls_back_to_now` and
        // `v2_record_loads_under_v3_policy_with_defaults`.)
        assert_eq!(restored.get(&done_id).unwrap().created_at, t_created);
        assert_eq!(restored.get(&done_id).unwrap().completed_at, Some(t_completed));
        assert_eq!(restored.get(&pending_id).unwrap().created_at, pending.created_at);
        assert_eq!(restored.get(&pending_id).unwrap().completed_at, None);

        // Artifact requirements: design doc proposed_files + per-task map.
        assert_eq!(
            serde_json::to_value(&loaded.design_doc).unwrap(),
            serde_json::to_value(Some(design_doc)).unwrap(),
            "design_doc (incl. proposed_files) preserved"
        );
        assert_eq!(loaded.expected_artifacts, expected_artifacts, "per-task artifacts preserved");

        // Model assignments captured so far round-trip exactly.
        assert_eq!(loaded.model_assignments, model_assignments, "model assignments preserved");

        // Ledger entries (action + evidence) round-trip exactly.
        assert_eq!(loaded.action_ledger, action_ledger, "action ledger preserved");

        // Completed results round-trip exactly. (`AgentRunResult` has no
        // `PartialEq`; compare via canonical JSON.)
        assert_eq!(
            serde_json::to_value(&loaded.completed_results).unwrap(),
            serde_json::to_value(&completed_results).unwrap(),
            "completed results preserved"
        );
    }

    // ------------------------------------------------------------------
    // ADR-60 D5: gate-boundary checkpoint + per-agent revert
    // ------------------------------------------------------------------

    /// A stored `WriteApplied` filesystem-write row shaped exactly like the
    /// write gate produces (payload `{ tool, input: { operation, path,
    /// content } }`).
    fn applied_write(
        gate_seq: u64,
        event_id: &str,
        agent_id: &str,
        path: &str,
        content: &str,
    ) -> WhiteboardEvent {
        WhiteboardEvent {
            event_id: event_id.to_owned(),
            gate_seq,
            agent_id: agent_id.to_owned(),
            agent_seq: 1,
            kind: WhiteboardKind::WriteApplied,
            scope: String::new(),
            session_id: None,
            plan_id: None,
            causation: None,
            payload: serde_json::json!({
                "tool": "filesystem",
                "input": { "operation": "write", "path": path, "content": content }
            }),
            content_hash: String::new(),
            pre_image_hash: None,
            created_at: 1_700_000_000_000,
        }
    }

    #[test]
    fn d5_checkpoint_at_cut_then_revert_excluding_agent_replays_the_log() {
        let events = vec![
            applied_write(1, "e1", "agent-a", "shared.txt", "base"),
            applied_write(2, "e2", "agent-b", "notes.md", "b1"),
            // Non-write rows must never be folded into the projection.
            WhiteboardEvent {
                kind: WhiteboardKind::Decision,
                payload: serde_json::json!({ "note": "not a write" }),
                ..applied_write(3, "e3", "agent-a", "poison.txt", "never")
            },
            applied_write(4, "e4", "agent-a", "shared.txt", "cut-value"),
            // A rejected write carries no file effect even for a kept agent.
            WhiteboardEvent {
                kind: WhiteboardKind::WriteRejected,
                ..applied_write(5, "e5", "agent-b", "rejected.txt", "nope")
            },
            applied_write(6, "e6", "agent-b", "shared.txt", "tail-b"),
            applied_write(7, "e7", "agent-a", "notes.md", "a2"),
        ];

        // Checkpoint at S = 4 ("everything ≤ seq S"): the cut holds each
        // path's last applied write at or before the boundary.
        let cut = GateBoundaryCheckpoint::at_cut(&events, 4, None);
        assert_eq!(cut.gate_seq, 4);
        assert_eq!(cut.files.get("shared.txt").map(String::as_str), Some("cut-value"));
        assert_eq!(cut.files.get("notes.md").map(String::as_str), Some("b1"));
        assert!(!cut.files.contains_key("poison.txt"), "non-write rows are not applied");
        assert!(!cut.files.contains_key("rejected.txt"), "rejected writes are not applied");
        assert!(!cut.files.contains_key("extra.txt"));

        // Per-agent revert (D5 ii): restore the cut, replay the tail minus
        // agent-b's rows. shared.txt stays at its cut value (b's tail write
        // is skipped); notes.md advances via agent-a's tail write.
        let reverted = revert_excluding_agent(&events, Some("agent-b"), 4);
        assert_eq!(reverted.get("shared.txt").map(String::as_str), Some("cut-value"));
        assert_eq!(reverted.get("notes.md").map(String::as_str), Some("a2"));
        assert!(!reverted.contains_key("extra.txt"), "excluded agent's creates are gone");

        // Whole-log exclusion (cut 0 — the promoted fixture's semantics):
        // agent-b's rows are skipped everywhere, so shared.txt keeps only
        // agent-a's writes and notes.md advances straight to a2.
        let no_b = revert_excluding_agent(&events, Some("agent-b"), 0);
        assert_eq!(no_b.get("shared.txt").map(String::as_str), Some("cut-value"));
        assert_eq!(no_b.get("notes.md").map(String::as_str), Some("a2"));

        // Full-log replay (empty restore point, no exclusion): the log's last
        // writer per path wins. Only the two genuinely applied write paths
        // appear; poison/rejected rows contribute nothing.
        let full = revert_excluding_agent(&events, None, u64::MAX);
        assert_eq!(full.len(), 2);
        assert_eq!(full.get("shared.txt").map(String::as_str), Some("tail-b"));
        assert_eq!(full.get("notes.md").map(String::as_str), Some("a2"));
        assert!(!full.contains_key("poison.txt") && !full.contains_key("rejected.txt"));

        // Restore-forward through the same snapshot object agrees with the
        // one-shot helper.
        let forward = cut.replay_tail_excluding(&events, Some("agent-b"));
        assert_eq!(forward, reverted);
    }

    // ------------------------------------------------------------------
    // C-05: schema version policy
    // ------------------------------------------------------------------

    #[test]
    fn v2_record_loads_under_v3_policy_with_defaults() {
        // v2-shaped fixture: no design_doc, model_assignments, action_ledger,
        // and no per-subtask timestamps.
        let json = r#"{
            "schema_version": 2,
            "run_id": "01HZ0X0X0X0X0X0X0X0X0X0X0X",
            "session_id": "01HZ0X0X0X0X0X0X0X0X0X0X0X",
            "root_task_id": "01HZ0X0X0X0X0X0X0X0X0X0X0X",
            "project_id": "test",
            "objective": "test objective",
            "objective_hash": "hash",
            "source_revision": null,
            "sequence_num": 1,
            "stage": "Executing",
            "completed": false,
            "planning": null,
            "working_memory": null,
            "subtasks": [
                {
                    "id": "01HZ0X0X0X0X0X0X0X0X0X0X0X",
                    "parent_id": null,
                    "session_id": "01HZ0X0X0X0X0X0X0X0X0X0X0X",
                    "role": "researcher",
                    "description": "analyze",
                    "status": "Completed",
                    "dependencies": [],
                    "deliverable": "done"
                }
            ],
            "edges": [],
            "completed_results": {},
            "total_cost": 0.0,
            "total_tool_calls": 0,
            "provider_metrics": [],
            "all_files": [],
            "expected_artifacts": {},
            "subtask_attempts": {},
            "retry_feedback": {}
        }"#;

        let loaded = GraphCheckpoint::from_json(json).unwrap();
        assert_eq!(
            loaded.schema_version, GRAPH_CHECKPOINT_SCHEMA_VERSION,
            "v2 record migrated to v3 on load"
        );
        assert!(loaded.design_doc.is_none(), "design_doc defaults to None");
        assert!(loaded.model_assignments.is_empty(), "model_assignments defaults empty");
        assert!(loaded.action_ledger.is_empty(), "action_ledger defaults empty");
        assert_eq!(loaded.subtasks.len(), 1);
        assert!(loaded.subtasks[0].created_at.is_none(), "v2 timestamps default to None");

        // A migrated v2 record restores to a usable graph.
        let restored = restore_graph(&loaded).unwrap();
        assert_eq!(restored.len(), 1);
        let task = restored.all_tasks()[0];
        assert_eq!(task.status, SubTaskStatus::Completed);
        assert!(task.created_at.unix_timestamp() > 0, "restore falls back to now for v2");

        // The migration is also honored by scope validation (defense in depth).
        assert!(loaded.validate_scope(loaded.session_id, "test", None).is_ok());
    }

    #[test]
    fn unknown_future_schema_version_rejected() {
        let cp = build_minimal_checkpoint(&TaskGraph::new());
        let mut value = serde_json::to_value(&cp).unwrap();
        value["schema_version"] = serde_json::json!(99);

        let error = GraphCheckpoint::from_json(&value.to_string()).unwrap_err();
        assert!(
            error.contains("unsupported checkpoint schema version 99"),
            "expected clear version error, got: {error}"
        );

        // validate_scope also rejects unknown future versions cleanly.
        let future: GraphCheckpoint = serde_json::from_value(value).unwrap();
        let scope_error = future.validate_scope(future.session_id, "test", None).unwrap_err();
        assert!(
            scope_error.contains("incompatible with runtime schema"),
            "expected scope rejection, got: {scope_error}"
        );
    }

    // ------------------------------------------------------------------
    // C-05: dangling references fail restore loudly
    // ------------------------------------------------------------------

    #[test]
    fn restore_rejects_dangling_edge() {
        let mut cp = build_minimal_checkpoint(&TaskGraph::new());
        let phantom_from = TaskId::new();
        let phantom_to = TaskId::new();
        cp.edges.push((phantom_from, phantom_to, Dependency::MustFinishBefore));

        let Err(error) = restore_graph(&cp) else {
            panic!("expected dangling-edge error, got a successful restore");
        };
        assert!(
            error.to_string().contains("missing from the checkpoint"),
            "expected dangling-edge error, got: {error}"
        );
    }

    #[test]
    fn restore_rejects_dangling_subtask_dependency() {
        let graph = TaskGraph::new();
        let mut cp = build_minimal_checkpoint(&graph);
        let present = TaskId::new();
        let missing = TaskId::new();
        cp.subtasks.push(CheckpointSubTask {
            id: present,
            parent_id: None,
            session_id: Ulid::new(),
            role: AgentId::new("coder"),
            description: "depends on a phantom task".into(),
            status: SubTaskStatus::Pending,
            dependencies: vec![missing],
            deliverable: None,
            created_at: None,
            completed_at: None,
        });

        let Err(error) = restore_graph(&cp) else {
            panic!("expected dangling-dependency error, got a successful restore");
        };
        assert!(
            error.to_string().contains("missing from the checkpoint"),
            "expected dangling-dependency error, got: {error}"
        );
    }

    #[test]
    fn restore_propagates_add_dependency_failure_with_context() {
        // Both endpoints pass the checkpoint-level pre-validation (they are
        // listed as subtasks), but the graph being restored cannot resolve
        // one of them — exactly the defense-in-depth scenario where
        // `add_dependency` itself fails. The failure must propagate with
        // restore context instead of being silently discarded.
        use std::collections::HashSet;

        let from = TaskId::new();
        let to = TaskId::new();
        let ids: HashSet<TaskId> = [from, to].into_iter().collect();
        let mut graph = TaskGraph::new();
        // Neither endpoint is actually present in the graph's node map, so
        // add_dependency fails at the graph layer even though both pass the
        // checkpoint-level pre-validation.
        graph.add_root(make_subtask("present", AgentId::new("architect"), vec![]));

        let error =
            restore_edge(&mut graph, &ids, from, to, Dependency::MustFinishBefore).unwrap_err();
        assert!(
            error.to_string().contains("failed to add dependency"),
            "add_dependency failure must be wrapped with restore context, got: {error}"
        );
        assert!(
            error.to_string().contains("checkpoint restore"),
            "error must be recognizable as a restore failure, got: {error}"
        );
    }
}
