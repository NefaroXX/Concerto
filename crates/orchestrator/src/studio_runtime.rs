//! Shared read-only projection of persisted Coordinator checkpoints.
//!
//! Desktop and CLI use the same reader and suitability ranking. This is a
//! saved checkpoint view, not a live control API. Missing checkpoints are
//! empty; storage/schema failures remain distinguishable from empty state.

use std::sync::Arc;

use concerto_core::ids::Ulid;
use concerto_core::types::{AgentId, SubTaskStatus, TaskId};
use concerto_core::CancellationToken;
use concerto_sessions::SessionStore;
use time::OffsetDateTime;

use crate::checkpoint::GraphCheckpoint;
use crate::decisions::CoordinatorDecision;
use crate::failure_diagnosis::FailureDiagnosis;
use crate::graph::Dependency;
use crate::suitability::{AgentSuitability, SuitabilityIndex, SuitabilityState, TaskClass};
use crate::world_model::WorldModel;

/// One task class's deterministic suitability ranking.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct TaskClassSuitability {
    pub task_class: TaskClass,
    pub agents: Vec<AgentSuitability>,
}

/// Saved task state. This is a projection, never a live scheduler handle.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct RuntimeTask {
    pub id: TaskId,
    pub parent_id: Option<TaskId>,
    pub agent: AgentId,
    pub description: String,
    pub status: SubTaskStatus,
    pub dependencies: Vec<TaskId>,
    pub deliverable: Option<String>,
}

/// A directed edge from the prerequisite/context provider to its consumer.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct RuntimeDependency {
    pub from: TaskId,
    pub to: TaskId,
    pub kind: Dependency,
}

/// Keep the complete graph together so display limits cannot sever its edges.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct RuntimeTopology {
    pub tasks: Vec<RuntimeTask>,
    pub edges: Vec<RuntimeDependency>,
}

/// The bounded, read-only Coordinator 2.0 runtime snapshot the panels render.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct StudioRuntimeSnapshot {
    /// Task identities, saved statuses, and directed dependency edges.
    pub topology: RuntimeTopology,
    /// The typed decision journal (whiteboard Decision events reconciled in the
    /// persisted `decision_journal`).
    pub decisions: Vec<CoordinatorDecision>,
    /// The compact world-model projection, when the run produced one.
    pub world_model: Option<WorldModel>,
    /// The structured failure-diagnosis history.
    pub diagnoses: Vec<FailureDiagnosis>,
    /// Per-task-class suitability rankings restored from the checkpoint.
    pub suitability: Vec<TaskClassSuitability>,
    /// Set when the load failed. The panels still render their empty states;
    /// the surface shows this muted note (fail-soft).
    pub load_error: Option<String>,
}

impl StudioRuntimeSnapshot {
    /// Extract the snapshot from a persisted checkpoint. Pure; reads only the
    /// existing additive Coordinator fields — no new sources, no runtime APIs.
    pub fn from_checkpoint(checkpoint: &GraphCheckpoint) -> Self {
        let mut snapshot = Self::from_parts(
            &checkpoint.decision_journal,
            &checkpoint.failure_diagnoses,
            &checkpoint.world_model,
            &checkpoint.suitability,
            OffsetDateTime::now_utc(),
        );
        snapshot.topology = RuntimeTopology {
            tasks: checkpoint
                .subtasks
                .iter()
                .map(|task| RuntimeTask {
                    id: task.id,
                    parent_id: task.parent_id,
                    agent: task.role.clone(),
                    description: task.description.clone(),
                    status: task.status,
                    dependencies: task.dependencies.clone(),
                    deliverable: task.deliverable.clone(),
                })
                .collect(),
            edges: checkpoint
                .edges
                .iter()
                .map(|(from, to, kind)| RuntimeDependency { from: *from, to: *to, kind: *kind })
                .collect(),
        };
        snapshot
    }

    /// The testable extraction core: `now` is injected so suitability rankings
    /// stay deterministic in tests.
    pub fn from_parts(
        decision_journal: &[CoordinatorDecision],
        failure_diagnoses: &[FailureDiagnosis],
        world_model: &WorldModel,
        suitability: &SuitabilityState,
        now: OffsetDateTime,
    ) -> Self {
        Self {
            topology: RuntimeTopology::default(),
            decisions: decision_journal.to_vec(),
            world_model: world_model_present(world_model),
            diagnoses: failure_diagnoses.to_vec(),
            suitability: suitability_rankings(suitability, now),
            load_error: None,
        }
    }

    /// An empty snapshot carrying the reason the read failed. The panels show
    /// their empty states; the note is muted and non-blocking.
    pub fn unavailable(message: impl Into<String>) -> Self {
        Self { load_error: Some(message.into()), ..Self::default() }
    }

    /// Whether the snapshot holds any Coordinator 2.0 state to display.
    pub fn is_empty(&self) -> bool {
        self.decisions.is_empty()
            && self.topology.tasks.is_empty()
            && self.topology.edges.is_empty()
            && self.world_model.is_none()
            && self.diagnoses.is_empty()
            && self.suitability.is_empty()
    }
}

/// A world model counts as present unless it is the untouched zero value
/// (empty beyond the objective and with no objective). This mirrors the
/// checkpoint's serde-default field: old checkpoints deserialize an empty
/// model, which must not render as a bogus pane.
fn world_model_present(model: &WorldModel) -> Option<WorldModel> {
    if model.is_empty_beyond_objective() && model.objective.is_none() {
        None
    } else {
        Some(model.clone())
    }
}

/// Rank the recorded suitability state per task class. Deterministic given
/// `now`; the candidate set is exactly the agents with recorded outcomes for
/// the class — never a fabricated roster.
pub fn suitability_rankings(
    state: &SuitabilityState,
    now: OffsetDateTime,
) -> Vec<TaskClassSuitability> {
    let index = SuitabilityIndex::from_state(state.clone());
    let mut classes: Vec<TaskClass> =
        state.buckets.iter().map(|keyed| keyed.key.task_class).collect();
    classes.sort();
    classes.dedup();
    classes
        .into_iter()
        .map(|task_class| {
            let mut candidates: Vec<String> = state
                .buckets
                .iter()
                .filter(|keyed| keyed.key.task_class == task_class)
                .map(|keyed| keyed.key.agent_id.clone())
                .collect();
            candidates.sort();
            candidates.dedup();
            TaskClassSuitability { task_class, agents: index.rank(&candidates, task_class, now) }
        })
        .collect()
}

/// Why a runtime snapshot could not be read. The caller fail-softs to an empty
/// panel plus this note, never an error page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StudioRuntimeError(pub String);

impl std::fmt::Display for StudioRuntimeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for StudioRuntimeError {}

/// The read seam the observability panels are fed through: production reads
/// the session's persisted checkpoint; tests inject a fixed snapshot.
#[async_trait::async_trait]
pub trait StudioRuntimeReader: Send + Sync {
    async fn load(
        &self,
        session_id: Ulid,
        cancel: CancellationToken,
    ) -> Result<StudioRuntimeSnapshot, StudioRuntimeError>;
}

/// Production reader: the session's persisted orchestration checkpoint via the
/// existing `SessionStore` the desktop crate already depends on.
pub struct CheckpointStudioRuntimeReader {
    store: Arc<dyn SessionStore>,
}

impl CheckpointStudioRuntimeReader {
    pub fn new(store: Arc<dyn SessionStore>) -> Self {
        Self { store }
    }
}

#[async_trait::async_trait]
impl StudioRuntimeReader for CheckpointStudioRuntimeReader {
    async fn load(
        &self,
        session_id: Ulid,
        cancel: CancellationToken,
    ) -> Result<StudioRuntimeSnapshot, StudioRuntimeError> {
        // Honor cancellation at the seam before any I/O (CancellationToken is
        // threaded through every async op).
        if cancel.is_cancelled() {
            return Ok(StudioRuntimeSnapshot::default());
        }
        let record = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Ok(StudioRuntimeSnapshot::default()),
            result = self.store.load_orchestration_checkpoint(session_id) => {
                result.map_err(|error| StudioRuntimeError(format!("checkpoint read failed: {error}")))?
            }
        };
        let Some(record) = record else {
            // No persisted run state for this session: empty panel, no error.
            return Ok(StudioRuntimeSnapshot::default());
        };
        let checkpoint = GraphCheckpoint::from_json(&record.state_json)
            .map_err(|error| StudioRuntimeError(format!("checkpoint parse failed: {error}")))?;
        Ok(StudioRuntimeSnapshot::from_checkpoint(&checkpoint))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use concerto_core::types::TaskId;
    use concerto_sessions::{OrchestrationCheckpointRecord, SqliteSessionStore};

    #[test]
    fn topology_preserves_saved_statuses_and_dependency_direction() {
        let session_id = Ulid::new();
        let first = TaskId::new();
        let second = TaskId::new();
        let checkpoint = GraphCheckpoint::from_json(
            &serde_json::json!({
                "schema_version": 4, "session_id": session_id,
                "subtasks": [
                    {"id": first, "parent_id": null, "session_id": session_id,
                     "role": "coder", "description": "implement", "status": "Completed",
                     "dependencies": [], "deliverable": "src/lib.rs"},
                    {"id": second, "parent_id": first, "session_id": session_id,
                     "role": "reviewer", "description": "review", "status": "Blocked",
                     "dependencies": [first], "deliverable": null}
                ],
                "edges": [[first, second, "MustFinishBefore"]],
                "completed_results": {}, "total_cost": 0.0, "total_tool_calls": 0,
                "provider_metrics": [], "all_files": [], "subtask_attempts": {},
                "retry_feedback": {}
            })
            .to_string(),
        )
        .unwrap();
        let snapshot = StudioRuntimeSnapshot::from_checkpoint(&checkpoint);
        assert!(!snapshot.is_empty(), "a graph-only checkpoint has inspectable state");
        assert_eq!(snapshot.topology.tasks[0].status, SubTaskStatus::Completed);
        assert_eq!(snapshot.topology.tasks[0].deliverable.as_deref(), Some("src/lib.rs"));
        assert_eq!(snapshot.topology.tasks[1].status, SubTaskStatus::Blocked);
        assert_eq!(snapshot.topology.tasks[1].parent_id, Some(first));
        assert_eq!(snapshot.topology.tasks[1].dependencies, vec![first]);
        assert_eq!(
            snapshot.topology.edges,
            vec![RuntimeDependency { from: first, to: second, kind: Dependency::MustFinishBefore }]
        );
    }

    #[tokio::test]
    async fn reader_loads_checkpoint_without_changing_stored_state() {
        let store = Arc::new(SqliteSessionStore::connect_in_memory().await.unwrap());
        let session = store
            .create_session(
                &camino::Utf8PathBuf::from("/project"),
                "test",
                "test",
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let record = OrchestrationCheckpointRecord {
            session_id: session.id,
            run_id: Ulid::new(),
            root_task_id: TaskId::new(),
            project_id: "test".into(),
            objective_hash: "test".into(),
            schema_version: 4,
            source_revision: None,
            sequence_num: 1,
            state_json: serde_json::json!({
                "schema_version": 4,
                "session_id": session.id,
                "subtasks": [], "edges": [], "completed_results": {},
                "total_cost": 0.0, "total_tool_calls": 0, "provider_metrics": [],
                "all_files": [], "subtask_attempts": {}, "retry_feedback": {},
                "world_model": {"objective": "inspect this saved objective"}
            })
            .to_string(),
            completed: false,
            updated_at: OffsetDateTime::now_utc(),
        };
        store.save_orchestration_checkpoint(&record).await.unwrap();
        let reader = CheckpointStudioRuntimeReader::new(store.clone());
        let snapshot = reader.load(session.id, CancellationToken::new()).await.unwrap();
        assert_eq!(
            snapshot.world_model.unwrap().objective.as_deref(),
            Some("inspect this saved objective")
        );
        assert_eq!(
            store.load_orchestration_checkpoint(session.id).await.unwrap().unwrap().state_json,
            record.state_json
        );

        let malformed = OrchestrationCheckpointRecord { state_json: "broken".into(), ..record };
        store.save_orchestration_checkpoint(&malformed).await.unwrap();
        assert!(reader
            .load(session.id, CancellationToken::new())
            .await
            .unwrap_err()
            .to_string()
            .contains("parse failed"));
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(reader.load(session.id, cancel).await.unwrap().is_empty());
        assert!(reader.load(Ulid::new(), CancellationToken::new()).await.unwrap().is_empty());
    }
}
