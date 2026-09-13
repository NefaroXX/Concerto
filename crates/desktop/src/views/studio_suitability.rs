//! S4 — read-only Suitability panel.
//!
//! Shows per-task-class `AgentSuitability` rankings (score + bounded reasons)
//! restored from the checkpoint's suitability state. Read-only: nothing here
//! selects, dispatches, or re-scores.

use iced::widget::{column, text};
use iced::{Element, Length};

use crate::app::Message;
use crate::theme::AppTheme;
use crate::views::studio_runtime::{panel_note, StudioRuntimeSnapshot, TaskClassSuitability};

/// The deterministic score label: milli-points rendered as signed thousandths
/// (the score is the raw clamped sum, no hidden scaling).
pub fn score_label(score_milli: i64) -> String {
    format!("{:+.3}", score_milli as f64 / 1000.0)
}

/// The panel body (no card chrome): one block per task class, or the muted
/// empty state when no run data exists.
pub fn body<'a>(snapshot: &'a StudioRuntimeSnapshot, theme: &'a AppTheme) -> Element<'a, Message> {
    if snapshot.suitability.is_empty() {
        return panel_note(theme, "No suitability history recorded for this session.");
    }
    let mut list = column![].spacing(10.0);
    for class in &snapshot.suitability {
        list = list.push(class_block(class, theme));
    }
    list.into()
}

fn class_block<'a>(class: &'a TaskClassSuitability, theme: &'a AppTheme) -> Element<'a, Message> {
    let ts = &theme.type_scale;
    let mut block =
        column![text(class.task_class.as_str()).size(ts.label).color(theme.palette.text)]
            .spacing(4.0)
            .width(Length::Fill);
    if class.agents.is_empty() {
        block = block
            .push(text("no recorded candidates").size(ts.caption).color(theme.palette.text_muted));
        return block.into();
    }
    for agent in &class.agents {
        block = block.push(
            column![
                text(format!("{} · {}", agent.agent_id, score_label(agent.score_milli)))
                    .size(ts.body)
                    .color(theme.palette.text),
                text(agent.reasons.join("; ")).size(ts.caption).color(theme.palette.text_muted),
            ]
            .spacing(2.0)
            .width(Length::Fill),
        );
    }
    block.into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::views::studio_runtime::TaskClassSuitability;
    use concerto_orchestrator::suitability::{AgentSuitability, TaskClass};

    fn snapshot() -> StudioRuntimeSnapshot {
        StudioRuntimeSnapshot {
            suitability: vec![TaskClassSuitability {
                task_class: TaskClass::CodeEdit,
                agents: vec![
                    AgentSuitability {
                        agent_id: "coder".into(),
                        score_milli: 1500,
                        reasons: vec!["3 fresh successes".into()],
                    },
                    AgentSuitability {
                        agent_id: "reviewer".into(),
                        score_milli: -300,
                        reasons: vec!["1 failure (tool)".into()],
                    },
                ],
            }],
            ..StudioRuntimeSnapshot::default()
        }
    }

    #[test]
    fn empty_snapshot_renders_the_empty_state_without_panicking() {
        let theme = AppTheme::by_name("Midnight");
        let snapshot = StudioRuntimeSnapshot::default();
        assert!(snapshot.suitability.is_empty());
        let _ = body(&snapshot, &theme);
    }

    #[test]
    fn rankings_and_reasons_render_without_panicking() {
        let snapshot = snapshot();
        let theme = AppTheme::by_name("Midnight");
        let _ = body(&snapshot, &theme);
        assert_eq!(snapshot.suitability[0].agents.len(), 2);
        assert_eq!(snapshot.suitability[0].agents[0].agent_id, "coder");
        assert!(!snapshot.suitability[0].agents[0].reasons.is_empty());
    }

    #[test]
    fn score_label_formats_signed_thousandths() {
        assert_eq!(score_label(1500), "+1.500");
        assert_eq!(score_label(-300), "-0.300");
        assert_eq!(score_label(0), "+0.000");
    }

    #[test]
    fn a_class_with_no_ranked_candidates_renders_without_panicking() {
        let snapshot = StudioRuntimeSnapshot {
            suitability: vec![TaskClassSuitability {
                task_class: TaskClass::General,
                agents: Vec::new(),
            }],
            ..StudioRuntimeSnapshot::default()
        };
        let theme = AppTheme::by_name("Midnight");
        let _ = body(&snapshot, &theme);
    }
}
