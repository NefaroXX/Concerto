//! Agent inspector panel: the per-agent header (back navigation, protected
//! badge, duplicate/remove actions), the Prompt / Model / Tool-access tab
//! strip, the removal-confirmation card, and the dispatch to the pane
//! builders (`prompt_pane`, `model_pane`, `permissions_pane`).
//!
//! Extracted from `orchestration_studio.rs` (NORM Slice 18) as a behavior-
//! preserving move: the parent module owns `State`, `StudioMessage`, and the
//! pane builders; this file only renders the panel shell. The entry point
//! stays `pub(super)` so reachability is unchanged (parent `view` call site +
//! in-file tests).

use iced::widget::{button, column, row, text, Space};
use iced::{Alignment, Element};

use crate::app::Message;
use crate::theme::AppTheme;
use crate::ui::section_card::section_card;

use super::{badge, caps_summary, inspector_tab, InspectorSection, State, StudioMessage};

impl State {
    /// Render the inspector for `self.selected_agent_id`: header actions, the
    /// tab strip, the removal-confirmation card while one is pending, and the
    /// active pane. Falls back to the pipeline workspace when no agent is
    /// selected or the stored id no longer matches the roster.
    pub(super) fn inspector_view<'a>(&'a self, theme: &'a AppTheme) -> Element<'a, Message> {
        let ts = &theme.type_scale;
        let sp = &theme.spacing;
        if let Some(id) = &self.selected_agent_id {
            if let Some(agent) = self.agents.iter().find(|a| &a.id == id) {
                let tabs = row![
                    inspector_tab(
                        self.inspector_section == InspectorSection::Prompt,
                        "Instructions",
                        InspectorSection::Prompt,
                    ),
                    inspector_tab(
                        self.inspector_section == InspectorSection::Model,
                        "Model",
                        InspectorSection::Model,
                    ),
                    inspector_tab(
                        self.inspector_section == InspectorSection::Permissions,
                        "Tool access",
                        InspectorSection::Permissions,
                    ),
                ]
                .spacing(sp.sm);

                let body = match self.inspector_section {
                    InspectorSection::Prompt => self.prompt_pane(agent, theme),
                    InspectorSection::Model => self.model_pane(agent, theme),
                    InspectorSection::Permissions => self.permissions_pane(agent, theme),
                };
                let removal: Option<Element<'_, Message>> = if self.pending_agent_removal.as_ref()
                    == Some(id)
                {
                    Some(section_card(theme, format!("Remove {}?", agent.name), column![
                        text("This removes the agent and its blueprint staffing from the draft. Save applies the removal globally. Existing run history is retained.").size(ts.body),
                        row![
                            button("Remove agent").style(crate::ui::button::danger)
                                .on_press(Message::OrchestrationStudio(StudioMessage::ConfirmAgentRemoval)),
                            button("Keep agent").style(crate::ui::button::secondary)
                                .on_press(Message::OrchestrationStudio(StudioMessage::CancelAgentRemoval)),
                        ].spacing(sp.sm).wrap(),
                    ].spacing(sp.sm)))
                } else {
                    None
                };
                return column![
                    row![
                        button("← Agents")
                            .style(button::secondary)
                            .on_press(Message::OrchestrationStudio(StudioMessage::ShowPipeline)),
                        column![
                            row![
                                text(&agent.name).size(ts.display),
                                if agent.id == "coordinator" {
                                    badge(theme, "protected")
                                } else {
                                    Space::new().into()
                                },
                            ]
                            .spacing(sp.xs)
                            .align_y(Alignment::Center),
                            text(format!("{} · {}", agent.role, caps_summary(&agent.capabilities)))
                                .size(ts.body)
                                .color(theme.palette.text_muted),
                        ]
                        .spacing(sp.xs),
                    ]
                    .spacing(sp.md)
                    .align_y(Alignment::Center),
                    row![
                        button("Duplicate agent").style(crate::ui::button::secondary).on_press(
                            Message::OrchestrationStudio(StudioMessage::DuplicateAgent(id.clone()))
                        ),
                        button("Remove…").style(button::text).on_press(
                            Message::OrchestrationStudio(StudioMessage::RequestAgentRemoval(
                                id.clone()
                            ))
                        ),
                    ]
                    .spacing(sp.sm)
                    .wrap(),
                    removal,
                    tabs.wrap(),
                    iced::widget::rule::horizontal(1),
                    body,
                ]
                .spacing(sp.md)
                .padding([sp.xs, 0.0])
                .into();
            }
        }
        self.pipeline_view(theme, &self.validation())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The extracted entry point keeps both branch conditions the parent
    /// `view` relies on: a roster match renders the panel (each tab section),
    /// and a missing or dangling selection falls back to the pipeline
    /// workspace. iced 0.14 elements are opaque in headless tests (no text
    /// extraction), so this pins the dispatch and renders every combination
    /// without panicking.
    #[test]
    fn inspector_view_renders_every_tab_and_both_fallback_branches() {
        let theme = AppTheme::by_name("Midnight");
        let mut state = State::new();
        state.selected_agent_id = Some("architect".to_string());

        state.inspector_section = InspectorSection::Prompt;
        let _ = state.inspector_view(&theme);
        state.inspector_section = InspectorSection::Model;
        let _ = state.inspector_view(&theme);
        state.inspector_section = InspectorSection::Permissions;
        let _ = state.inspector_view(&theme);

        // Pending removal renders the confirmation card over the panel.
        state.pending_agent_removal = Some("architect".to_string());
        let _ = state.inspector_view(&theme);
        state.pending_agent_removal = None;

        // No selection ⇒ pipeline workspace fallback.
        state.selected_agent_id = None;
        let _ = state.inspector_view(&theme);

        // A dangling id never matches an agent ⇒ same fallback, no panic.
        state.selected_agent_id = Some("ghost".to_string());
        let _ = state.inspector_view(&theme);
    }
}
