//! S2 — read-only World Model panel.
//!
//! Renders the Coordinator's compact world-model projection using the existing
//! `WorldModel::render()` helper — never a reimplementation — plus a small
//! count summary over the artifacts / open questions / risks the render
//! already carries. Read-only.

use iced::widget::{column, text};
use iced::{Element, Length};

use crate::app::Message;
use crate::theme::AppTheme;
use crate::views::studio_runtime::{panel_note, StudioRuntimeSnapshot};

/// The bounded render text for the snapshot's world model, if one exists.
pub fn render_text(snapshot: &StudioRuntimeSnapshot) -> Option<String> {
    snapshot.world_model.as_ref().map(|model| model.render())
}

/// The panel body (no card chrome): the existing bounded render plus a count
/// summary, or the muted empty state.
pub fn body<'a>(snapshot: &'a StudioRuntimeSnapshot, theme: &'a AppTheme) -> Element<'a, Message> {
    let ts = &theme.type_scale;
    let Some(model) = snapshot.world_model.as_ref() else {
        return panel_note(theme, "No world model recorded for this session.");
    };
    let summary = format!(
        "artifacts: {} · open questions: {} · risks: {}",
        model.artifacts.len(),
        model.open_question_count(),
        model.risks.len(),
    );
    column![
        text(summary).size(ts.caption).color(theme.palette.text_muted).width(Length::Fill),
        text(model.render()).size(ts.caption).color(theme.palette.text),
    ]
    .spacing(6.0)
    .width(Length::Fill)
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::views::studio_runtime::StudioRuntimeSnapshot;
    use concerto_orchestrator::world_model::{
        ArtifactStatus, WorldArtifact, WorldModel, WorldRisk,
    };

    fn model() -> WorldModel {
        WorldModel {
            objective: Some("ship the bounded slice".into()),
            artifacts: vec![WorldArtifact {
                path: "src/panel.rs".into(),
                status: ArtifactStatus::Written,
                owner: Some("coder".into()),
                last_ref: None,
            }],
            risks: vec![WorldRisk { ref_id: "r1".into(), label: "scope".into() }],
            ..WorldModel::default()
        }
    }

    #[test]
    fn empty_snapshot_renders_the_empty_state_without_panicking() {
        let theme = AppTheme::by_name("Midnight");
        let snapshot = StudioRuntimeSnapshot::default();
        assert!(render_text(&snapshot).is_none());
        let _ = body(&snapshot, &theme);
    }

    #[test]
    fn render_reuses_the_world_model_helper_and_stays_bounded() {
        let snapshot = StudioRuntimeSnapshot {
            world_model: Some(model()),
            ..StudioRuntimeSnapshot::default()
        };
        let rendered = render_text(&snapshot).expect("world model present");
        // The render is the existing bounded helper, never reimplemented.
        assert_eq!(rendered, model().render());
        assert!(rendered.chars().count() <= concerto_orchestrator::world_model::MAX_RENDER_CHARS);
        assert!(rendered.contains("src/panel.rs"));
        assert!(rendered.contains("risks: scope"));
        let theme = AppTheme::by_name("Midnight");
        let _ = body(&snapshot, &theme);
    }
}
