//! Pipeline workspace panel: the agent canvas, hand-off (relationship) list
//! and editor, the legacy run-limit drafts, and the validation card.
//!
//! Extracted from `orchestration_studio.rs` (NORM Slice 8) as a behavior-
//! preserving move: the parent module owns `State`, `StudioMessage`, and the
//! update logic; this file only renders the panel. The two entry points stay
//! `pub(super)` so reachability is unchanged (parent call site + in-file tests).

use iced::widget::{
    button, column, container, pick_list, row, scrollable, text, text_input, Space,
};
use iced::{Alignment, Element, Length};

use crate::app::Message;
use crate::theme::AppTheme;
use crate::ui::section_card::{section_card, section_card_with_subtitle};
use crate::widgets::agent_graph::{self, Message as GraphMessage};

use super::{
    legacy_rel_kind_options, legacy_relationship_semantics, semantics_glyph, semantics_label,
    AgentOption, State, StudioMessage, ValidationReport, STUDIO_HANDOFFS_LIST_ID,
};

impl State {
    /// Render the pipeline workspace: the clickable agent canvas, the
    /// hand-off (relationship) list with its add/edit form, the legacy
    /// run-limit drafts (hidden on the blueprint path — see `run_limits_card`),
    /// and the validation card for the supplied report.
    pub(super) fn pipeline_view<'a>(
        &'a self,
        theme: &'a AppTheme,
        report: &ValidationReport,
    ) -> Element<'a, Message> {
        let ts = &theme.type_scale;
        let sp = &theme.spacing;
        // The pipeline canvas: nodes are agents (click to inspect/scroll to
        // their hand-offs), edges are drawn but no longer interactive.
        let (graph_model, _edge_to_relationship) = self.pipeline_graph_model();
        let graph_colors = self.graph_colors(theme);
        let graph = agent_graph::view(graph_model.clone(), graph_colors, theme.palette.text).map(
            move |msg: GraphMessage| {
                let studio = match msg {
                    GraphMessage::NodeClicked(idx) => StudioMessage::GraphNodeClicked(idx),
                };
                Message::OrchestrationStudio(studio)
            },
        );
        // Size the canvas to the model's vertical extent so the whole chain is
        // visible on open (a fixed 340px clipped the standard 6-rank layout).
        let graph_height =
            graph_model.nodes.iter().map(|node| node.position.y + 60.0).fold(260.0, f32::max);
        // Clip the canvas to the card so panned/zoomed geometry never bleeds
        // over the Hand-offs / Run Limits panels below it (issue #136).
        let graph_canvas =
            container(graph).clip(true).width(Length::Fill).height(Length::Fixed(graph_height));

        let mut relationship_list = column![].spacing(sp.xs);
        for (i, rel) in self.relationships.iter().enumerate() {
            let cycles = rel
                .max_cycles
                .map(|value| format!("{value} cycle{}", if value == 1 { "" } else { "s" }))
                .unwrap_or_else(|| "unlimited cycles".into());
            let semantics = legacy_relationship_semantics(&rel.relationship);
            relationship_list = relationship_list.push(
                row![
                    column![
                        text(format!(
                            "{} → {}",
                            self.agent_label(&rel.from),
                            self.agent_label(&rel.to)
                        ))
                        .size(ts.body),
                        text(format!(
                            "{} {} · {cycles}",
                            semantics_glyph(semantics),
                            semantics_label(semantics)
                        ))
                        .size(ts.caption)
                        .color(theme.palette.text_muted),
                    ]
                    .spacing(sp.xs)
                    .width(Length::Fill),
                    button("Edit").style(button::secondary).on_press(Message::OrchestrationStudio(
                        StudioMessage::SelectRelationship(Some(i))
                    )),
                    button("Delete").style(button::danger).on_press(Message::OrchestrationStudio(
                        StudioMessage::DeleteRelationship(i)
                    )),
                ]
                .spacing(sp.sm)
                .align_y(Alignment::Center),
            );
        }
        if self.relationships.is_empty() {
            relationship_list = relationship_list.push(
                text("No relationships configured yet.")
                    .size(ts.body)
                    .color(theme.palette.text_muted),
            );
        }

        // The rows scroll inside a bounded region (240px max) so long lists
        // don't push the add/edit form off-screen; a short list (or the empty
        // state) shrinks to its natural height and wheel over a non-overflowing
        // list passes through to the page scrollable.
        let relationship_list_scrollable = container(
            scrollable(relationship_list)
                .id(iced::widget::Id::new(STUDIO_HANDOFFS_LIST_ID))
                .height(Length::Shrink),
        )
        .max_height(240.0);

        let agent_options: Vec<AgentOption> = self
            .agents
            .iter()
            .map(|a| AgentOption { id: a.id.clone(), label: a.name.clone() })
            .collect();
        let selected_from = agent_options.iter().find(|o| o.id == self.new_rel_from).cloned();
        let selected_to = agent_options.iter().find(|o| o.id == self.new_rel_to).cloned();
        let rel_types = legacy_rel_kind_options();
        let selected_rel_type =
            rel_types.iter().find(|option| option.kind == self.new_rel_type).cloned();
        // The add/edit form only renders while it is relevant: when an edge is
        // clicked (or "Add hand-off" is pressed). Browsing the pipeline and
        // editing a hand-off are now visually distinct states.
        let editor_visible = self.selected_relationship.is_some() || self.show_relationship_editor;
        // relationship_draft() clones the relationship list and runs a cycle
        // DFS — only pay for it while the editor is on screen.
        let draft_error = if editor_visible { self.relationship_draft().err() } else { None };
        let submit_label = if self.selected_relationship.is_some() {
            "Save relationship"
        } else {
            "Add relationship"
        };
        let submit = if draft_error.is_none() {
            button(submit_label)
                .style(button::primary)
                .on_press(Message::OrchestrationStudio(StudioMessage::CreateRelationship))
        } else {
            button(submit_label).style(button::primary)
        };
        let draft_note: Element<'_, Message> = match draft_error {
            Some(error) => text(error).size(ts.caption).color(theme.palette.text_muted).into(),
            None => text("This relationship keeps the pipeline acyclic.")
                .size(ts.caption)
                .color(theme.palette.success)
                .into(),
        };
        let relationship_editor = column![
            text(if self.selected_relationship.is_some() {
                "Edit relationship"
            } else {
                "Add relationship"
            })
            .size(ts.label),
            row![
                pick_list(agent_options.clone(), selected_from, |option| {
                    Message::OrchestrationStudio(StudioMessage::NewRelFrom(option.id))
                })
                .placeholder("From agent"),
                pick_list(agent_options, selected_to, |option| {
                    Message::OrchestrationStudio(StudioMessage::NewRelTo(option.id))
                })
                .placeholder("To agent"),
                pick_list(rel_types, selected_rel_type, |option| {
                    Message::OrchestrationStudio(StudioMessage::NewRelType(option.kind))
                })
                .placeholder("Relationship type"),
                text_input("Max cycles (optional)", &self.new_rel_max_cycles).on_input(|value| {
                    Message::OrchestrationStudio(StudioMessage::NewRelMaxCycles(value))
                }),
            ]
            .spacing(sp.sm),
            row![
                submit,
                if self.selected_relationship.is_some() {
                    button("Cancel").style(button::secondary).on_press(
                        Message::OrchestrationStudio(StudioMessage::SelectRelationship(None)),
                    )
                } else {
                    button("Clear").style(button::secondary).on_press(Message::OrchestrationStudio(
                        StudioMessage::SelectRelationship(None),
                    ))
                },
                draft_note,
            ]
            .spacing(sp.sm)
            .align_y(Alignment::Center),
        ]
        .spacing(sp.sm);

        let validation_details: Element<'_, Message> = if report.ok {
            text("Pipeline validation passed.").size(ts.body).color(theme.palette.success).into()
        } else {
            let mut issues = column![text("Fix these issues before saving:")
                .size(ts.body)
                .color(theme.palette.danger)]
            .spacing(sp.xs);
            for message in &report.messages {
                issues = issues.push(text(format!("• {message}")).size(ts.body));
            }
            issues.into()
        };
        let total_tokens: u64 =
            self.agents.iter().map(|agent| self.token_estimate_for(agent)).sum();
        // "Reset to standard" is intentionally absent: the toolbar's
        // "Load preset…" pick-list is the single entry point and already lists
        // "Standard Pipeline" first, so resetting stays one click away.
        let pipeline_card = section_card_with_subtitle(
            theme,
            "Pipeline",
            format!(
                "{} agents · {} relationships · ~{} prompt tokens",
                self.agents.len(),
                self.relationships.len(),
                total_tokens
            ),
            graph_canvas,
        );
        // The editor block renders only while relevant (see editor_visible
        // above); otherwise the space is reserved so the layout does not jump.
        let relationship_editor_block: Element<'_, Message> =
            if editor_visible { relationship_editor.into() } else { Space::new().into() };
        let handoffs_card = section_card(
            theme,
            "Relationships",
            column![
                row![
                    text(format!("{} configured", self.relationships.len()))
                        .size(ts.caption)
                        .color(theme.palette.text_muted),
                    Space::new().width(Length::Fill),
                    button("+ Add relationship").style(button::secondary).on_press(
                        Message::OrchestrationStudio(StudioMessage::ToggleRelationshipEditor(true))
                    ),
                ]
                .align_y(Alignment::Center),
                relationship_list_scrollable,
                relationship_editor_block,
            ]
            .spacing(sp.sm),
        );
        // Blueprint users never see the legacy run-limit drafts (oracle
        // finding, Slice 2): `[orchestration]` configs are governed by the
        // blueprint model (per-stage `max_cycles`, rule (f) bound) instead of
        // the legacy `multi_agent` tuning, and `validation()` skips the
        // drafts' checks on the blueprint path — rendering editable drafts
        // whose checks are skipped would be a dead/lying surface. The card
        // is the drafts' only renderer, so gating it here makes the drafts
        // unreachable on the blueprint path; the legacy path renders
        // unchanged.
        let run_limits_block: Element<'_, Message> =
            self.run_limits_card(theme).unwrap_or_else(|| Space::new().into());
        let validation_card = section_card(theme, "Validation", validation_details);
        column![
            pipeline_card,
            handoffs_card,
            row![run_limits_block, validation_card].spacing(sp.md),
        ]
        .spacing(sp.md)
        .padding([sp.xs, 0.0])
        .into()
    }

    /// The legacy Run Limits card (concurrency + spend-cap drafts), or `None`
    /// on the blueprint path — see the oracle-finding note at the call site.
    /// `None` keeps the legacy surface byte-identical: the card only exists
    /// for `multi_agent` configs without `[orchestration]`.
    pub(super) fn run_limits_card<'a>(
        &'a self,
        theme: &'a AppTheme,
    ) -> Option<Element<'a, Message>> {
        if self.orchestration.is_some() {
            return None;
        }
        let ts = &theme.type_scale;
        let sp = &theme.spacing;
        // Inline captions under each run-limit input mirror validation()'s
        // checks. Always present (a zero-height space when valid) so the row
        // heights stay stable whether or not the caption shows.
        let agents_caption: Element<'_, Message> = if self
            .run_agents_draft
            .parse::<usize>()
            .map_or(true, |v| v < 1)
        {
            text("must be a whole number ≥ 1").size(ts.caption).color(theme.palette.danger).into()
        } else {
            Space::new().into()
        };
        let provider_caption: Element<'_, Message> = if self
            .run_provider_draft
            .parse::<usize>()
            .map_or(true, |v| v < 1)
        {
            text("must be a whole number ≥ 1").size(ts.caption).color(theme.palette.danger).into()
        } else {
            Space::new().into()
        };
        let spend_caption: Element<'_, Message> = if self
            .run_spend_draft
            .parse::<f64>()
            .map_or(true, |v| v <= 0.0)
        {
            text("must be a positive number").size(ts.caption).color(theme.palette.danger).into()
        } else {
            Space::new().into()
        };
        let card = section_card_with_subtitle(
            theme,
            "Run Limits",
            "Concurrency and shared spend budget",
            column![
                row![
                    column![
                        text("Max concurrent agents").size(ts.caption),
                        text_input("3", &self.run_agents_draft).on_input(|value| {
                            Message::OrchestrationStudio(StudioMessage::RunAgentsChanged(value))
                        }),
                        agents_caption,
                    ]
                    .spacing(sp.xs),
                    column![
                        text("Max concurrent per provider").size(ts.caption),
                        text_input("2", &self.run_provider_draft).on_input(|value| {
                            Message::OrchestrationStudio(StudioMessage::RunProviderChanged(value))
                        }),
                        provider_caption,
                    ]
                    .spacing(sp.xs),
                    column![
                        text("Spend cap multiplier").size(ts.caption),
                        text_input("3.0", &self.run_spend_draft).on_input(|value| {
                            Message::OrchestrationStudio(StudioMessage::RunSpendChanged(value))
                        }),
                        spend_caption,
                    ]
                    .spacing(sp.xs),
                ]
                .spacing(sp.sm),
                text(
                    "Controls how many agents run at once and how tall the shared spend budget \
                     is. Multi-agent defaults: 3 agents, 2 per provider, budget ×3.",
                )
                .size(ts.caption)
                .color(theme.palette.text_muted),
            ]
            .spacing(sp.sm),
        );
        Some(card)
    }
}
