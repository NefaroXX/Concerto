//! System-dialog + ambient-background view sections for [`App`] — one
//! cluster extracted from `app.rs` (NORM slice S29).
//!
//! This module owns the system-level dialog stack (V6: the 9-way modal-card
//! chain — root consent, dir picker, capability/ack/intent/plan dialogs,
//! memory graph + memory explorer, and the shortcuts help — composed above
//! the sub-view overlay) and the ambient backgrounds (V7: the circuit-trace
//! pulse + chat scan-line overlay stacked under everything). The bodies
//! moved verbatim from [`App::view`]; the only edits are the `pub(super)`
//! annotations, the parameter plumbing (each fn takes the Element it stacks
//! instead of closing over a `view()` local), and the `let composed:`
//! binding becoming the tail expression. Both fns are pure `&self`
//! builders: read-only state + the theme only, no `Task`, no locks, no
//! mutation — the dialog helpers they call (`capability_dialog::view` and
//! friends) lock only inside their own unchanged bodies, exactly as when
//! they were inlined in `view()`. `App::view()` keeps composition only; the
//! modal tests in `app.rs`'s `mod tests` stay put untouched.

use super::*;

// Explicit import outranks both the `use super::*;` glob and the std
// prelude's `column!` (Rust 1.96), which otherwise collide as ambiguous.
use iced::widget::column;

use crate::widgets::confirm_modal::ConfirmMessage;

impl App {
    /// V6 — system-level dialogs (shown above subviews): exactly one modal
    /// card (or none) stacked over `after_subview`, in priority order.
    pub(super) fn view_system_dialogs<'a>(
        &'a self,
        after_subview: Element<'a, Message>,
    ) -> Element<'a, Message> {
        // ── System-level dialogs (shown above subviews) ──
        // Topmost: the unsaved-changes guard armed while closing a dirty
        // Settings modal. It is a small centered confirm card over a dimmed
        // backdrop; confirming discards the edits and closes Settings.
        if let Some(confirm) = &self.settings_discard_confirm {
            let backdrop = container(confirm.view().map(|message| match message {
                ConfirmMessage::Confirm => Message::SettingsDiscardConfirmed,
                ConfirmMessage::Cancel => Message::SettingsDiscardCancelled,
            }))
            .width(Length::Fill)
            .height(Length::Fill)
            .style(|_theme: &iced::Theme| container::Style {
                background: Some(iced::Background::Color(iced::Color {
                    a: 0.55,
                    ..self.current_theme.palette.background
                })),
                ..container::Style::default()
            });
            stack![after_subview, backdrop].into()
        } else if let Some(pending) = &self.pending_root_consent {
            // ADR-44 §4 consent gate: top of the system-dialog stack so it
            // blocks all interaction until the user decides. Composed via
            // the same pattern as the Memory modal (PR #120): a centered
            // modal card over a semi-transparent palette backdrop.
            let modal = container(root_consent::consent_card(
                pending,
                &self.current_theme.iced,
                Message::RootConsentAllow,
                Message::RootConsentDeny,
            ))
            .width(Length::FillPortion(2))
            .height(Length::FillPortion(2))
            .style(crate::ui::container::modal);
            let backdrop = container(modal)
                .width(Length::Fill)
                .height(Length::Fill)
                .center_x(Length::Fill)
                .center_y(Length::Fill)
                .style(|_theme: &iced::Theme| container::Style {
                    background: Some(iced::Background::Color(iced::Color {
                        a: 0.55,
                        ..self.current_theme.palette.background
                    })),
                    ..container::Style::default()
                });
            stack![after_subview, backdrop].into()
        } else if self.show_dir_picker {
            let input = text_input("Project folder path", &self.project_dir_input)
                .on_input(Message::ProjectDirInputChanged)
                .width(420);
            let open_btn = button(text("Open"))
                .style(crate::ui::button::primary)
                .on_press(Message::ProjectDirApply);
            let cancel_btn = button(text("Cancel"))
                .style(crate::ui::button::secondary)
                .on_press(Message::ProjectDirCancel);
            let modal = container(
                column![
                    text("Project Folder").size(18),
                    text("Files the agent writes are saved here.")
                        .size(13)
                        .color(self.current_theme.palette.text_muted),
                    input,
                    row![cancel_btn, open_btn].spacing(10).padding(10),
                ]
                .spacing(10)
                .padding(24)
                .width(480),
            )
            .style(crate::ui::container::modal);
            let overlay = container(modal)
                .width(Length::Fill)
                .height(Length::Fill)
                .center_x(Length::Fill)
                .center_y(Length::Fill);
            stack![after_subview, overlay].into()
        } else if let Some(cap_view) = capability_dialog::view(&self.cap_pending) {
            let dlg = cap_view.map(Message::CapabilityDlg);
            // Dimmed, interaction-blocking backdrop so the dialog reads as a
            // modal: clicks on the backdrop are captured and do not reach the
            // chat underneath, and the centered card stays clearly visible.
            let backdrop = container(dlg)
                .width(Length::Fill)
                .height(Length::Fill)
                .center_x(Length::Fill)
                .center_y(Length::Fill)
                .style(|_theme: &iced::Theme| container::Style {
                    background: Some(iced::Background::Color(iced::Color {
                        a: 0.55,
                        ..self.current_theme.palette.background
                    })),
                    ..container::Style::default()
                });
            stack![after_subview, backdrop].into()
        } else if let Some(ack_dlg) = capability_dialog::ack_view(&self.pending_ack) {
            let backdrop = container(ack_dlg.map(Message::AckDialog))
                .width(Length::Fill)
                .height(Length::Fill)
                .center_x(Length::Fill)
                .center_y(Length::Fill)
                .style(|_theme: &iced::Theme| container::Style {
                    background: Some(iced::Background::Color(iced::Color {
                        a: 0.55,
                        ..self.current_theme.palette.background
                    })),
                    ..container::Style::default()
                });
            stack![after_subview, backdrop].into()
        } else if let Some(intent_dlg) = capability_dialog::intent_view(&self.pending_intent) {
            // Intent confirmation modal (ADR-55 §2), mirroring the capability
            // and ack dialogs: a centered card over a dimmed palette backdrop.
            let backdrop = container(intent_dlg.map(Message::IntentDialog))
                .width(Length::Fill)
                .height(Length::Fill)
                .center_x(Length::Fill)
                .center_y(Length::Fill)
                .style(|_theme: &iced::Theme| container::Style {
                    background: Some(iced::Background::Color(iced::Color {
                        a: 0.55,
                        ..self.current_theme.palette.background
                    })),
                    ..container::Style::default()
                });
            stack![after_subview, backdrop].into()
        } else if let Some(plan_dlg) =
            capability_dialog::plan_view(&self.pending_plan, &self.current_theme)
        {
            // Plan approval modal (ADR-55 §4), mirroring the intent
            // dialog: a centered card over a dimmed palette backdrop.
            let backdrop = container(plan_dlg.map(Message::PlanDialog))
                .width(Length::Fill)
                .height(Length::Fill)
                .center_x(Length::Fill)
                .center_y(Length::Fill)
                .style(|_theme: &iced::Theme| container::Style {
                    background: Some(iced::Background::Color(iced::Color {
                        a: 0.55,
                        ..self.current_theme.palette.background
                    })),
                    ..container::Style::default()
                });
            stack![after_subview, backdrop].into()
        } else if self.memory_graph_open {
            // Read-only memory graph modal (ADR-69 slice 3). Rendered before
            // the Memory explorer branch so it sits on top when opened from
            // the Explorer's header button.
            let graph_content =
                self.memory_graph.modal_view(&self.current_theme).map(Message::MemoryGraph);
            let modal = container(
                column![
                    row![
                        text("Memory Graph").size(18).width(Length::Fill),
                        button(text("↻").size(14))
                            .style(crate::ui::button::secondary)
                            .on_press(Message::MemoryGraph(views::memory_graph::Message::Refresh)),
                        button(text("✕").size(14))
                            .style(crate::ui::button::secondary)
                            .on_press(Message::CloseMemoryGraph),
                    ]
                    .align_y(iced::Alignment::Center),
                    graph_content,
                ]
                .spacing(10)
                .padding(20)
                .width(Length::Fill),
            )
            .width(Length::FillPortion(2))
            .height(Length::FillPortion(2))
            .style(crate::ui::container::modal);
            let backdrop = container(modal)
                .width(Length::Fill)
                .height(Length::Fill)
                .center_x(Length::Fill)
                .center_y(Length::Fill)
                .style(|_theme: &iced::Theme| container::Style {
                    background: Some(iced::Background::Color(iced::Color {
                        a: 0.55,
                        ..self.current_theme.palette.background
                    })),
                    ..container::Style::default()
                });
            stack![after_subview, backdrop].into()
        } else if self.memory_view_open {
            // Memory explorer modal (issue #110). Composed via the same
            // system-dialog stack mechanism as the dir picker / capability /
            // ack dialogs: a centered modal card over a semi-transparent
            // palette backdrop, sitting above sub-view overlays.
            let memory_content = self.memory.modal_view(&self.current_theme).map(Message::Memory);
            let modal = container(
                column![
                    row![
                        text("Memory").size(18).width(Length::Fill),
                        button(text("⇄ Graph").size(13))
                            .style(crate::ui::button::secondary)
                            .on_press(Message::OpenMemoryGraph),
                        button(text("✕").size(14))
                            .style(crate::ui::button::secondary)
                            .on_press(Message::CloseMemoryModal),
                    ]
                    .align_y(iced::Alignment::Center),
                    memory_content,
                ]
                .spacing(10)
                .padding(20)
                .width(Length::Fill),
            )
            .width(Length::FillPortion(2))
            .height(Length::FillPortion(2))
            .style(crate::ui::container::modal);
            let backdrop = container(modal)
                .width(Length::Fill)
                .height(Length::Fill)
                .center_x(Length::Fill)
                .center_y(Length::Fill)
                .style(|_theme: &iced::Theme| container::Style {
                    background: Some(iced::Background::Color(iced::Color {
                        a: 0.55,
                        ..self.current_theme.palette.background
                    })),
                    ..container::Style::default()
                });
            stack![after_subview, backdrop].into()
        } else if self.harness_view_open {
            // H13 harness modal: the desktop controls over the shared harness
            // service. Same modal-card + palette backdrop mechanism as the
            // memory explorer above; every result is the service payload.
            let harness_content = self.harness.view(&self.current_theme).map(Message::Harness);
            let modal = container(
                column![
                    row![
                        text("Harness").size(18).width(Length::Fill),
                        button(text("✕").size(14))
                            .style(crate::ui::button::secondary)
                            .on_press(Message::CloseHarnessModal),
                    ]
                    .align_y(iced::Alignment::Center),
                    harness_content,
                ]
                .spacing(10)
                .padding(20)
                .width(Length::Fill),
            )
            .width(Length::FillPortion(2))
            .height(Length::FillPortion(2))
            .style(crate::ui::container::modal);
            let backdrop = container(modal)
                .width(Length::Fill)
                .height(Length::Fill)
                .center_x(Length::Fill)
                .center_y(Length::Fill)
                .style(|_theme: &iced::Theme| container::Style {
                    background: Some(iced::Background::Color(iced::Color {
                        a: 0.55,
                        ..self.current_theme.palette.background
                    })),
                    ..container::Style::default()
                });
            stack![after_subview, backdrop].into()
        } else if self.show_settings_modal {
            // Settings modal (ADR-57 §3d): the same `settings::State::view`
            // page body, hosted as a centered overlay card instead of a page
            // switch, so opening it never abandons in-progress chat/studio
            // work. A dirty form shows an "Unsaved changes" badge; closing via
            // ✕ / Esc / backdrop arms the discard guard above.
            let hide_relationships =
                self.config.as_ref().is_some_and(orchestration_hides_relationships);
            let dirty = self.settings.settings_dirty;
            let header = row![
                text("Settings").size(18).width(Length::Fill),
                if dirty {
                    text("Unsaved changes").size(12).color(self.current_theme.palette.warning)
                } else {
                    text("").size(12)
                },
                button(text("✕").size(14))
                    .style(crate::ui::button::secondary)
                    .on_press(Message::CloseSettingsModal),
            ]
            .spacing(12)
            .align_y(iced::Alignment::Center);
            let body = container(
                self.settings.view(&self.current_theme, hide_relationships).map(Message::Settings),
            )
            .width(Length::Fill)
            .height(Length::Fill);
            let modal =
                container(column![header, body].spacing(12).padding(16).width(Length::Fill))
                    .width(Length::Fill)
                    .max_width(860.0)
                    .height(Length::FillPortion(2))
                    .style(crate::ui::container::modal);
            // The dim layer is a `mouse_area` stacked *under* the card, so a
            // click on empty backdrop space closes the modal while clicks on
            // the card itself do not fall through.
            let dim = mouse_area(
                container(iced::widget::space::Space::new())
                    .width(Length::Fill)
                    .height(Length::Fill),
            )
            .on_press(Message::CloseSettingsModal);
            let centered_card = container(modal)
                .width(Length::Fill)
                .height(Length::Fill)
                .center_x(Length::Fill)
                .center_y(Length::Fill);
            let backdrop = container(stack![dim, centered_card])
                .width(Length::Fill)
                .height(Length::Fill)
                .style(|_theme: &iced::Theme| container::Style {
                    background: Some(iced::Background::Color(iced::Color {
                        a: 0.55,
                        ..self.current_theme.palette.background
                    })),
                    ..container::Style::default()
                });
            stack![after_subview, backdrop].into()
        } else if self.show_help {
            // Shortcuts reference modal (`?` or the right-toolbar keyboard
            // button). Lists every binding from `shortcuts::ALL` so the panel
            // cannot drift from the resolver.
            let rows = shortcuts::ALL.iter().fold(column![].spacing(6), |col, info| {
                col.push(
                    row![
                        text(info.keys).size(13).width(Length::Fixed(200.0)),
                        text(info.label).size(13).color(self.current_theme.palette.text_muted),
                    ]
                    .spacing(12),
                )
            });
            let modal = container(
                column![
                    row![
                        text("Keyboard Shortcuts").size(18).width(Length::Fill),
                        button(text("✕").size(14))
                            .style(crate::ui::button::secondary)
                            .on_press(Message::HelpToggled),
                    ]
                    .align_y(iced::Alignment::Center),
                    rows,
                ]
                .spacing(12)
                .padding(20)
                .width(Length::Fill),
            )
            .width(Length::FillPortion(2))
            .height(Length::FillPortion(2))
            .style(crate::ui::container::modal);
            let backdrop = container(modal)
                .width(Length::Fill)
                .height(Length::Fill)
                .center_x(Length::Fill)
                .center_y(Length::Fill)
                .style(|_theme: &iced::Theme| container::Style {
                    background: Some(iced::Background::Color(iced::Color {
                        a: 0.55,
                        ..self.current_theme.palette.background
                    })),
                    ..container::Style::default()
                });
            stack![after_subview, backdrop].into()
        } else {
            after_subview
        }
    }

    /// V7 — ambient backgrounds: the circuit-trace pulse (runs only) and the
    /// chat scan-line overlay (opt-in) stacked under `composed`.
    ///
    /// Ambient circuit-trace pulse, active only while an agent run is in
    /// progress. Bottom-most layer, so it reads through any gaps in
    /// `composed` rather than covering it.
    pub(super) fn view_ambient<'a>(
        &'a self,
        composed: Element<'a, Message>,
    ) -> Element<'a, Message> {
        let circuit_bg = (self.run_status == RunStatus::Running).then(|| {
            circuit_background::view(self.circuit_progress, self.current_theme.palette.accent)
        });
        // Faint scan-line overlay behind the chat column, when the default-off
        // flag is explicitly enabled. Default-off is the design intent
        // (`text-presentation-animation.md`: the Blueprint texture is an
        // *optional* accent, and the widget visibly pulses while streaming) —
        // shipping it on by default would add constant background motion to a
        // productivity tool and contradict the subtle-texture intent. Users
        // who want it enable the Settings toggle (works end-to-end); it is the
        // one cue not seeded by the message machinery, so reduced-motion and
        // the settled "new turn" markers (see `chat.rs::line_wipe_settled`) are
        // the always-visible presentation changes, not a hidden default.
        // Sits above the circuit background (per the widget's contract) so it
        // stays visible during runs — the only state in which a streaming
        // entry can double its pulse rate. It pulses at idle rate otherwise.
        let scanline_bg = (self.scanline_overlay_enabled && self.page == Page::Chat).then(|| {
            scanline_overlay::view(
                self.scanline_progress,
                self.chat.is_streaming(),
                self.reduced_motion,
                false, // show_grid — off for chat
                self.current_theme.palette.surface,
                self.current_theme.palette.border,
            )
        });
        match (circuit_bg, scanline_bg) {
            (Some(circuit), Some(scanline)) => stack![circuit, scanline, composed].into(),
            (Some(circuit), None) => stack![circuit, composed].into(),
            (None, Some(scanline)) => stack![scanline, composed].into(),
            (None, None) => composed,
        }
    }
}
