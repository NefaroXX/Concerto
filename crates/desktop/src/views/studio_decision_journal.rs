//! S1 — read-only Decision Journal panel.
//!
//! Lists the Coordinator decision journal (status / kind / target / evidence
//! ids / timestamp) restored from the persisted checkpoint. Read-only: the
//! only interaction is the outer collapse toggle; there is no dispatch,
//! approve, or edit affordance.

use iced::widget::{column, text};
use iced::{Element, Length};

use concerto_orchestrator::decisions::{CoordinatorDecision, DecisionKind, DecisionStatus};

use crate::app::Message;
use crate::theme::AppTheme;
use crate::views::studio_runtime::{panel_note, StudioRuntimeSnapshot, MAX_PANEL_ROWS};

/// One display row projected from a journal entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecisionRow {
    pub id: String,
    pub status: &'static str,
    pub kind: &'static str,
    /// The named target agent, or `-` for structural kinds that never name one.
    pub target: String,
    pub evidence_count: usize,
    pub created_at: String,
}

/// Kebab-case label for a decision status (the persisted vocabulary, not
/// `Debug`).
fn status_label(status: DecisionStatus) -> &'static str {
    match status {
        DecisionStatus::Pending => "pending",
        DecisionStatus::Validated => "validated",
        DecisionStatus::Dispatched => "dispatched",
        DecisionStatus::Settled => "settled",
        DecisionStatus::Rejected => "rejected",
        DecisionStatus::Superseded => "superseded",
    }
}

/// Kebab-case label for a decision kind.
fn kind_label(kind: DecisionKind) -> &'static str {
    match kind {
        DecisionKind::DispatchSpecialist => "dispatch-specialist",
        DecisionKind::DraftPlan => "draft-plan",
        DecisionKind::SelfExecute => "self-execute",
        DecisionKind::Replan => "replan",
        DecisionKind::Retry => "retry",
        DecisionKind::FallbackTier => "fallback-tier",
        DecisionKind::Split => "split",
        DecisionKind::Merge => "merge",
        DecisionKind::Consult => "consult",
        DecisionKind::TransferOwnership => "transfer-ownership",
        DecisionKind::Investigate => "investigate",
        DecisionKind::Wait => "wait",
        DecisionKind::Reconsider => "reconsider",
    }
}

fn row(decision: &CoordinatorDecision) -> DecisionRow {
    DecisionRow {
        id: decision.id.clone(),
        status: status_label(decision.status),
        kind: kind_label(decision.kind),
        target: decision
            .target_agent
            .as_ref()
            .map(|agent| agent.to_string())
            .unwrap_or_else(|| "-".to_string()),
        evidence_count: decision.supporting_evidence_ids.len(),
        created_at: decision.created_at.to_string(),
    }
}

/// Project the journal into bounded, newest-first display rows.
pub fn decision_rows(snapshot: &StudioRuntimeSnapshot) -> Vec<DecisionRow> {
    snapshot.decisions.iter().rev().take(MAX_PANEL_ROWS).map(row).collect()
}

/// The panel body (no card chrome): the bounded journal list, or the muted
/// empty state when no run data exists.
pub fn body<'a>(snapshot: &'a StudioRuntimeSnapshot, theme: &'a AppTheme) -> Element<'a, Message> {
    let ts = &theme.type_scale;
    let rows = decision_rows(snapshot);
    if rows.is_empty() {
        return panel_note(theme, "No Coordinator decisions recorded for this session.");
    }
    let mut list = column![].spacing(6.0);
    for row in rows {
        list = list.push(
            column![
                text(format!("{} · {}", row.status, row.kind))
                    .size(ts.body)
                    .color(theme.palette.text),
                text(format!(
                    "target: {} · evidence: {} · {}",
                    row.target, row.evidence_count, row.created_at
                ))
                .size(ts.caption)
                .color(theme.palette.text_muted),
            ]
            .spacing(2.0)
            .width(Length::Fill),
        );
    }
    list.into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::views::studio_runtime::StudioRuntimeSnapshot;
    use concerto_core::types::AgentId;
    use concerto_orchestrator::decisions::CoordinatorDecision;
    use time::OffsetDateTime;

    fn decision(id: &str, status: DecisionStatus) -> CoordinatorDecision {
        CoordinatorDecision {
            id: id.into(),
            kind: DecisionKind::DispatchSpecialist,
            target_agent: Some(AgentId::new("coder")),
            task_description: "work".into(),
            notes: None,
            supporting_evidence_ids: vec!["e1".into(), "e2".into()],
            expected_artifacts: Vec::new(),
            transform: None,
            max_tool_calls: None,
            wait_record: None,
            created_at: OffsetDateTime::now_utc(),
            status,
        }
    }

    #[test]
    fn empty_snapshot_renders_the_empty_state_without_panicking() {
        let theme = AppTheme::by_name("Midnight");
        let snapshot = StudioRuntimeSnapshot::default();
        assert!(decision_rows(&snapshot).is_empty());
        let _ = body(&snapshot, &theme);
    }

    #[test]
    fn entries_project_status_kind_target_and_evidence() {
        let snapshot = StudioRuntimeSnapshot {
            decisions: vec![decision("d1", DecisionStatus::Settled)],
            ..StudioRuntimeSnapshot::default()
        };
        let rows = decision_rows(&snapshot);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, "settled");
        assert_eq!(rows[0].kind, "dispatch-specialist");
        assert_eq!(rows[0].target, "coder");
        assert_eq!(rows[0].evidence_count, 2);
        let theme = AppTheme::by_name("Midnight");
        let _ = body(&snapshot, &theme);
    }

    #[test]
    fn a_structural_kind_without_a_target_projects_a_dash() {
        let mut entry = decision("d2", DecisionStatus::Rejected);
        entry.kind = DecisionKind::Replan;
        entry.target_agent = None;
        let snapshot =
            StudioRuntimeSnapshot { decisions: vec![entry], ..StudioRuntimeSnapshot::default() };
        let rows = decision_rows(&snapshot);
        assert_eq!(rows[0].kind, "replan");
        assert_eq!(rows[0].target, "-");
    }

    #[test]
    fn rows_are_newest_first_and_bounded() {
        let decisions: Vec<CoordinatorDecision> = (0..MAX_PANEL_ROWS + 10)
            .map(|i| decision(&format!("d{i}"), DecisionStatus::Pending))
            .collect();
        let total = decisions.len();
        let snapshot = StudioRuntimeSnapshot { decisions, ..StudioRuntimeSnapshot::default() };
        let rows = decision_rows(&snapshot);
        assert_eq!(rows.len(), MAX_PANEL_ROWS, "the display is hard-bounded");
        // Newest-first: the last journaled decision leads.
        assert_eq!(rows[0].id, format!("d{}", total - 1));
    }
}
