//! S3 — read-only Failure Diagnoses panel.
//!
//! Lists the run's structured [`FailureDiagnosis`] history (kind / code /
//! recovery verdict / evidence) using the existing `FailureDiagnosis::brief()`
//! helper. Read-only.

use iced::widget::{column, text};
use iced::{Element, Length};

use concerto_orchestrator::failure_diagnosis::FailureDiagnosis;

use crate::app::Message;
use crate::theme::AppTheme;
use crate::views::studio_runtime::{panel_note, StudioRuntimeSnapshot, MAX_PANEL_ROWS};

/// One display row projected from a failure diagnosis.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiagnosisRow {
    pub kind: &'static str,
    pub code: String,
    /// The compact `brief()` line: verdict flags + bounded evidence excerpt.
    pub brief: String,
}

fn row(diagnosis: &FailureDiagnosis) -> DiagnosisRow {
    DiagnosisRow {
        kind: diagnosis.kind.as_str(),
        code: diagnosis.code.clone(),
        brief: diagnosis.brief(),
    }
}

/// Project the diagnosis history into bounded, newest-first display rows.
pub fn diagnosis_rows(snapshot: &StudioRuntimeSnapshot) -> Vec<DiagnosisRow> {
    snapshot.diagnoses.iter().rev().take(MAX_PANEL_ROWS).map(row).collect()
}

/// The panel body (no card chrome): the bounded diagnosis list, or the muted
/// empty state when no run data exists.
pub fn body<'a>(snapshot: &'a StudioRuntimeSnapshot, theme: &'a AppTheme) -> Element<'a, Message> {
    let ts = &theme.type_scale;
    let rows = diagnosis_rows(snapshot);
    if rows.is_empty() {
        return panel_note(theme, "No failure diagnoses recorded for this session.");
    }
    let mut list = column![].spacing(6.0);
    for row in rows {
        list = list.push(
            column![
                text(format!("[{}] {}", row.kind, row.code))
                    .size(ts.body)
                    .color(theme.palette.text),
                text(row.brief).size(ts.caption).color(theme.palette.text_muted),
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
    use concerto_orchestrator::failure_diagnosis::{FailureDiagnosis, FailureKind};

    fn diagnosis(code: &str) -> FailureDiagnosis {
        FailureDiagnosis {
            kind: FailureKind::Provider,
            code: code.into(),
            transient: true,
            retryable: true,
            same_agent_viable: false,
            alternate_agent_viable: true,
            replan_required: false,
            evidence: "connection reset".into(),
        }
    }

    #[test]
    fn empty_snapshot_renders_the_empty_state_without_panicking() {
        let theme = AppTheme::by_name("Midnight");
        let snapshot = StudioRuntimeSnapshot::default();
        assert!(diagnosis_rows(&snapshot).is_empty());
        let _ = body(&snapshot, &theme);
    }

    #[test]
    fn entries_reuse_the_brief_helper() {
        let diagnosis = diagnosis("rate-limit");
        let expected = diagnosis.brief();
        let snapshot = StudioRuntimeSnapshot {
            diagnoses: vec![diagnosis],
            ..StudioRuntimeSnapshot::default()
        };
        let rows = diagnosis_rows(&snapshot);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kind, "provider");
        assert_eq!(rows[0].code, "rate-limit");
        assert_eq!(rows[0].brief, expected);
        // The brief line carries the verdict + evidence excerpt.
        assert!(rows[0].brief.contains("transient"));
        assert!(rows[0].brief.contains("connection reset"));
        let theme = AppTheme::by_name("Midnight");
        let _ = body(&snapshot, &theme);
    }

    #[test]
    fn rows_are_newest_first_and_bounded() {
        let diagnoses: Vec<FailureDiagnosis> =
            (0..MAX_PANEL_ROWS + 5).map(|i| diagnosis(&format!("code-{i}"))).collect();
        let snapshot = StudioRuntimeSnapshot { diagnoses, ..StudioRuntimeSnapshot::default() };
        let rows = diagnosis_rows(&snapshot);
        assert_eq!(rows.len(), MAX_PANEL_ROWS, "the display is hard-bounded");
        assert_eq!(rows[0].code, format!("code-{}", MAX_PANEL_ROWS + 4));
    }
}
