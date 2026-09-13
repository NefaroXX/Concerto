//! Read-only Coordinator 2.0 runtime state for the Studio's observability
//! panels.
//!
//! The panels never talk to a live Coordinator: they render a bounded,
//! read-only [`StudioRuntimeSnapshot`] loaded from the session's persisted
//! orchestration checkpoint through the [`StudioRuntimeReader`] seam. The
//! trait keeps view code testable without a Coordinator process — production
//! uses [`CheckpointStudioRuntimeReader`] over the existing `SessionStore`
//! (no new IPC/HTTP/SSE), tests use [`MockStudioRuntimeReader`].
//!
//! Fail-soft by contract: a missing checkpoint, a parse failure, or a store
//! error yields an empty snapshot (plus an optional muted note), never a
//! crash and never a blocking wait.

use std::sync::Arc;

use concerto_core::ids::Ulid;
use concerto_core::CancellationToken;
use concerto_orchestrator::checkpoint::GraphCheckpoint;
use concerto_orchestrator::decisions::CoordinatorDecision;
use concerto_orchestrator::failure_diagnosis::FailureDiagnosis;
use concerto_orchestrator::suitability::{
    AgentSuitability, SuitabilityIndex, SuitabilityState, TaskClass,
};
use concerto_orchestrator::world_model::WorldModel;
use concerto_sessions::SessionStore;
use time::OffsetDateTime;

use iced::widget::text;
use iced::Element;

use crate::app::Message;
use crate::theme::AppTheme;

/// Hard cap on how many entries one panel renders (newest first). The
/// snapshot keeps the faithful, checkpoint-bounded history; the display is
/// bounded here so a long run cannot grow the pane without limit.
pub const MAX_PANEL_ROWS: usize = 50;

/// The four read-only observability panels registered in the Studio panes row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StudioRuntimePanel {
    DecisionJournal,
    WorldModel,
    FailureDiagnoses,
    Suitability,
}

impl StudioRuntimePanel {
    /// Every panel, in display order.
    pub const ALL: [StudioRuntimePanel; 4] = [
        StudioRuntimePanel::DecisionJournal,
        StudioRuntimePanel::WorldModel,
        StudioRuntimePanel::FailureDiagnoses,
        StudioRuntimePanel::Suitability,
    ];

    /// The panel's card title.
    pub fn title(self) -> &'static str {
        match self {
            StudioRuntimePanel::DecisionJournal => "Decision Journal",
            StudioRuntimePanel::WorldModel => "World Model",
            StudioRuntimePanel::FailureDiagnoses => "Failure Diagnoses",
            StudioRuntimePanel::Suitability => "Suitability",
        }
    }
}

/// One task class's deterministic suitability ranking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskClassSuitability {
    pub task_class: TaskClass,
    pub agents: Vec<AgentSuitability>,
}

/// The bounded, read-only Coordinator 2.0 runtime snapshot the panels render.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StudioRuntimeSnapshot {
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
        Self::from_parts(
            &checkpoint.decision_journal,
            &checkpoint.failure_diagnoses,
            &checkpoint.world_model,
            &checkpoint.suitability,
            OffsetDateTime::now_utc(),
        )
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
        let record = self
            .store
            .load_orchestration_checkpoint(session_id)
            .await
            .map_err(|error| StudioRuntimeError(format!("checkpoint read failed: {error}")))?;
        let Some(record) = record else {
            // No persisted run state for this session: empty panel, no error.
            return Ok(StudioRuntimeSnapshot::default());
        };
        let checkpoint = GraphCheckpoint::from_json(&record.state_json)
            .map_err(|error| StudioRuntimeError(format!("checkpoint parse failed: {error}")))?;
        Ok(StudioRuntimeSnapshot::from_checkpoint(&checkpoint))
    }
}

/// Test double: returns a fixed snapshot without touching a `SessionStore`, so
/// panel tests exercise the seam without a Coordinator process.
#[cfg(test)]
pub struct MockStudioRuntimeReader {
    snapshot: StudioRuntimeSnapshot,
}

#[cfg(test)]
impl MockStudioRuntimeReader {
    pub fn new(snapshot: StudioRuntimeSnapshot) -> Self {
        Self { snapshot }
    }
}

#[cfg(test)]
#[async_trait::async_trait]
impl StudioRuntimeReader for MockStudioRuntimeReader {
    async fn load(
        &self,
        _session_id: Ulid,
        _cancel: CancellationToken,
    ) -> Result<StudioRuntimeSnapshot, StudioRuntimeError> {
        Ok(self.snapshot.clone())
    }
}

/// The muted empty-state note every panel shares when it has no run data.
pub fn panel_note<'a>(theme: &'a AppTheme, message: &'a str) -> Element<'a, Message> {
    text(message).size(theme.type_scale.caption).color(theme.palette.text_muted).into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use concerto_core::types::AgentId;
    use concerto_orchestrator::decisions::{CoordinatorDecision, DecisionKind, DecisionStatus};
    use concerto_orchestrator::failure_diagnosis::{FailureDiagnosis, FailureKind};
    use concerto_orchestrator::suitability::{
        OutcomeKind, SuitabilityBucket, SuitabilityKey, SuitabilityKeyedBucket, SuitabilityOutcome,
    };
    use time::Duration;

    fn decision(id: &str, status: DecisionStatus) -> CoordinatorDecision {
        CoordinatorDecision {
            id: id.into(),
            kind: DecisionKind::DispatchSpecialist,
            target_agent: Some(AgentId::new("coder")),
            task_description: "implement the panel".into(),
            notes: None,
            supporting_evidence_ids: vec!["ev-1".into()],
            expected_artifacts: vec!["src/panel.rs".into()],
            transform: None,
            max_tool_calls: None,
            created_at: OffsetDateTime::now_utc(),
            status,
        }
    }

    fn diagnosis(code: &str) -> FailureDiagnosis {
        FailureDiagnosis {
            kind: FailureKind::Tool,
            code: code.into(),
            transient: true,
            retryable: true,
            same_agent_viable: true,
            alternate_agent_viable: false,
            replan_required: false,
            evidence: "tool exploded".into(),
        }
    }

    fn outcome(kind: OutcomeKind, age_minutes: i64) -> SuitabilityOutcome {
        SuitabilityOutcome {
            recorded_at: OffsetDateTime::now_utc() - Duration::minutes(age_minutes),
            kind,
            failure_dimension: String::new(),
            extra_turns: 0,
            is_retry: false,
            cost_usd_milli: 0,
        }
    }

    fn suitability_state() -> SuitabilityState {
        SuitabilityState {
            buckets: vec![
                SuitabilityKeyedBucket {
                    key: SuitabilityKey {
                        agent_id: "coder".into(),
                        task_class: TaskClass::CodeEdit,
                    },
                    bucket: SuitabilityBucket {
                        outcomes: vec![outcome(OutcomeKind::Success, 1)],
                        ..SuitabilityBucket::default()
                    },
                },
                SuitabilityKeyedBucket {
                    key: SuitabilityKey {
                        agent_id: "reviewer".into(),
                        task_class: TaskClass::CodeEdit,
                    },
                    bucket: SuitabilityBucket {
                        outcomes: vec![outcome(OutcomeKind::Failure, 1)],
                        ..SuitabilityBucket::default()
                    },
                },
                SuitabilityKeyedBucket {
                    key: SuitabilityKey {
                        agent_id: "researcher".into(),
                        task_class: TaskClass::Research,
                    },
                    bucket: SuitabilityBucket {
                        outcomes: vec![outcome(OutcomeKind::Success, 1)],
                        ..SuitabilityBucket::default()
                    },
                },
            ],
        }
    }

    #[test]
    fn empty_snapshot_is_empty_and_not_an_error() {
        let snapshot = StudioRuntimeSnapshot::default();
        assert!(snapshot.is_empty());
        assert!(snapshot.load_error.is_none());
    }

    #[test]
    fn from_parts_extracts_every_panel_source() {
        let world = WorldModel {
            objective: Some("ship the slice".into()),
            risks: vec![concerto_orchestrator::world_model::WorldRisk {
                ref_id: "r1".into(),
                label: "scope".into(),
            }],
            ..WorldModel::default()
        };
        let snapshot = StudioRuntimeSnapshot::from_parts(
            &[decision("d1", DecisionStatus::Settled)],
            &[diagnosis("tool-error")],
            &world,
            &suitability_state(),
            OffsetDateTime::now_utc(),
        );
        assert_eq!(snapshot.decisions.len(), 1);
        assert_eq!(snapshot.decisions[0].id, "d1");
        assert_eq!(snapshot.diagnoses.len(), 1);
        assert_eq!(snapshot.diagnoses[0].code, "tool-error");
        let world = snapshot.world_model.as_ref().expect("world model present");
        assert_eq!(world.objective.as_deref(), Some("ship the slice"));
        // One ranking per recorded task class (both present). `TaskClass`
        // ordering is its declaration order (Research precedes CodeEdit).
        assert_eq!(snapshot.suitability.len(), 2);
        assert!(snapshot.suitability.iter().any(|r| r.task_class == TaskClass::CodeEdit));
        assert!(snapshot.suitability.iter().any(|r| r.task_class == TaskClass::Research));
        assert!(!snapshot.is_empty());
    }

    #[test]
    fn default_world_model_does_not_render_as_a_pane() {
        let snapshot = StudioRuntimeSnapshot::from_parts(
            &[],
            &[],
            &WorldModel::default(),
            &SuitabilityState::default(),
            OffsetDateTime::now_utc(),
        );
        assert!(snapshot.world_model.is_none());
        assert!(snapshot.is_empty());
    }

    #[test]
    fn suitability_rankings_rank_success_above_failure() {
        let rankings = suitability_rankings(&suitability_state(), OffsetDateTime::now_utc());
        let code_edit = rankings
            .iter()
            .find(|ranking| ranking.task_class == TaskClass::CodeEdit)
            .expect("code-edit class present");
        assert_eq!(code_edit.agents.len(), 2);
        // Deterministic ordering: highest score first, then agent id.
        assert_eq!(code_edit.agents[0].agent_id, "coder");
        assert!(code_edit.agents[0].score_milli > code_edit.agents[1].score_milli);
        assert!(!code_edit.agents[0].reasons.is_empty());
    }

    #[test]
    fn unavailable_carries_the_note_and_stays_empty() {
        let snapshot = StudioRuntimeSnapshot::unavailable("checkpoint read failed: boom");
        assert!(snapshot.is_empty());
        assert_eq!(snapshot.load_error.as_deref(), Some("checkpoint read failed: boom"));
    }

    #[tokio::test]
    async fn mock_reader_returns_the_injected_snapshot() {
        let fixed = StudioRuntimeSnapshot::from_parts(
            &[decision("d1", DecisionStatus::Validated)],
            &[],
            &WorldModel::default(),
            &SuitabilityState::default(),
            OffsetDateTime::now_utc(),
        );
        let reader = MockStudioRuntimeReader::new(fixed.clone());
        let loaded = reader
            .load(Ulid::new(), CancellationToken::new())
            .await
            .expect("mock reader never fails");
        assert_eq!(loaded, fixed);
    }

    #[tokio::test]
    async fn checkpoint_reader_without_state_yields_an_empty_snapshot() {
        // The store-backed path end-to-end: a session with no checkpoint must
        // fail-soft to an empty snapshot, not an error.
        let store = Arc::new(
            concerto_sessions::SqliteSessionStore::connect_in_memory()
                .await
                .expect("in-memory store"),
        );
        let reader = CheckpointStudioRuntimeReader::new(store);
        let loaded = reader
            .load(Ulid::new(), CancellationToken::new())
            .await
            .expect("missing checkpoint is not an error");
        assert!(loaded.is_empty());
        assert!(loaded.load_error.is_none());
    }
}
