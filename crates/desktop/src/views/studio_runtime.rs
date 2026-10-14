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

pub use concerto_orchestrator::studio_runtime::{
    CheckpointStudioRuntimeReader, StudioRuntimeReader, StudioRuntimeSnapshot, TaskClassSuitability,
};

#[cfg(test)]
use concerto_core::{ids::Ulid, CancellationToken};
#[cfg(test)]
use concerto_orchestrator::studio_runtime::{suitability_rankings, StudioRuntimeError};
#[cfg(test)]
use concerto_orchestrator::suitability::{SuitabilityState, TaskClass};
#[cfg(test)]
use concerto_orchestrator::world_model::WorldModel;
#[cfg(test)]
use std::sync::Arc;
#[cfg(test)]
use time::OffsetDateTime;

use iced::widget::{column, container, scrollable, text};
use iced::{Element, Length};

use crate::app::Message;
use crate::theme::AppTheme;
use crate::views::{
    studio_decision_journal, studio_failure_diagnoses, studio_suitability, studio_world_model,
};

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

/// Height budget for the runtime modal's scrollable panel list (mirrors the
/// Spend Log modal's fixed list height so the card keeps a bounded, stable
/// size regardless of how much run state a session accumulated).
const RUNTIME_MODAL_BODY_HEIGHT: f32 = 560.0;

/// The per-session runtime modal body: the same four read-only observability
/// panels the Studio used to show in its right-hand rail, now rendered in the
/// chat canvas overlay bound to the active session's snapshot.
///
/// This is a pure renderer over a [`StudioRuntimeSnapshot`] already loaded
/// through the [`StudioRuntimeReader`] seam — no new transport, no runtime
/// calls. Fail-soft: a missing checkpoint yields an empty snapshot and every
/// panel renders its own muted empty state; a load error is shown as a muted,
/// non-blocking note above the panels.
pub fn runtime_modal_view<'a>(
    snapshot: &'a StudioRuntimeSnapshot,
    theme: &'a AppTheme,
) -> Element<'a, Message> {
    let ts = &theme.type_scale;
    let sp = &theme.spacing;
    let mut panels = column![].spacing(sp.md).width(Length::Fill);
    if let Some(error) = snapshot.load_error.as_deref() {
        // Fail-soft: the panels still render their empty states below; the
        // reason is surfaced here, muted and non-blocking.
        panels = panels
            .push(text(error).size(ts.caption).color(theme.palette.warning).width(Length::Fill));
    }
    for panel in StudioRuntimePanel::ALL {
        panels = panels.push(runtime_panel_card(panel, snapshot, theme));
    }
    scrollable(panels).height(Length::Fixed(RUNTIME_MODAL_BODY_HEIGHT)).into()
}

/// One read-only panel card (title + body). The card chrome is the same
/// palette-only surface the Studio rail used; the body is the untouched
/// panel projection from the corresponding `studio_*` module.
fn runtime_panel_card<'a>(
    panel: StudioRuntimePanel,
    snapshot: &'a StudioRuntimeSnapshot,
    theme: &'a AppTheme,
) -> Element<'a, Message> {
    let ts = &theme.type_scale;
    let sp = &theme.spacing;
    let body: Element<'_, Message> = match panel {
        StudioRuntimePanel::DecisionJournal => studio_decision_journal::body(snapshot, theme),
        StudioRuntimePanel::WorldModel => studio_world_model::body(snapshot, theme),
        StudioRuntimePanel::FailureDiagnoses => studio_failure_diagnoses::body(snapshot, theme),
        StudioRuntimePanel::Suitability => studio_suitability::body(snapshot, theme),
    };
    container(
        column![text(panel.title()).size(ts.label).color(theme.palette.text), body].spacing(sp.sm),
    )
    .width(Length::Fill)
    .padding(sp.sm)
    .style(move |_t: &iced::Theme| iced::widget::container::Style {
        background: Some(iced::Background::Color(theme.palette.surface_variant)),
        border: iced::Border { color: theme.palette.border, width: 1.0, radius: 12.0.into() },
        ..iced::widget::container::Style::default()
    })
    .into()
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
            wait_record: None,
            dismissed_question_id: None,
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
            decision_id: None,
            artifact_path: None,
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

    /// The per-session modal renders all four panels when the active session's
    /// snapshot carries data. Asserted through the mock reader (no store, no
    /// Coordinator) plus a non-panicking render of the populated modal body —
    /// iced 0.14 elements are opaque in headless tests, so the panel set is
    /// pinned via `StudioRuntimePanel::ALL`.
    #[tokio::test]
    async fn runtime_modal_renders_all_four_panels_for_a_session_with_data() {
        let fixed = StudioRuntimeSnapshot::from_parts(
            &[decision("d1", DecisionStatus::Settled)],
            &[diagnosis("tool-error")],
            &WorldModel { objective: Some("ship the slice".into()), ..WorldModel::default() },
            &suitability_state(),
            OffsetDateTime::now_utc(),
        );
        let reader = MockStudioRuntimeReader::new(fixed);
        let loaded = reader
            .load(Ulid::new(), CancellationToken::new())
            .await
            .expect("mock reader never fails");
        assert_eq!(StudioRuntimePanel::ALL.len(), 4, "the modal shows all four panels");
        assert!(!loaded.is_empty(), "the session snapshot carries all four sources");
        let theme = AppTheme::by_name("Midnight");
        let _ = runtime_modal_view(&loaded, &theme);
    }

    /// An empty session (or an unavailable checkpoint) renders the panels'
    /// empty states plus the muted fail-soft note, never a crash.
    #[test]
    fn runtime_modal_renders_empty_states_and_fail_soft_note_without_panicking() {
        let theme = AppTheme::by_name("Midnight");
        let _ = runtime_modal_view(&StudioRuntimeSnapshot::default(), &theme);
        let _ = runtime_modal_view(
            &StudioRuntimeSnapshot::unavailable("checkpoint read failed: boom"),
            &theme,
        );
    }

    /// Every panel the modal iterates has a distinct display title (the card
    /// header), so the four cards never collapse into duplicates.
    #[test]
    fn runtime_modal_panels_have_distinct_titles() {
        let mut titles: Vec<&str> = StudioRuntimePanel::ALL.iter().map(|p| p.title()).collect();
        titles.sort_unstable();
        titles.dedup();
        assert_eq!(titles.len(), 4);
    }
}
