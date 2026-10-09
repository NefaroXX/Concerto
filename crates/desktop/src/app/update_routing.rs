//! Chat pre-routing and keyboard-shortcut `App::update()` arms for [`App`] —
//! NORM slice S42.
//!
//! This module owns the two root-update groups whose bodies decide *where* a
//! message goes (or flip small routing flags) rather than only forwarding to a
//! child view state, extracted from `app.rs`:
//!
//! * [`App::update_chat_pre_routing`] — the `Chat` arm's pre-routing: the
//!   clipboard write, the `Navigate*` / `SetActiveModel` / `SelectSession` /
//!   `RefreshSpendLog` re-dispatches, the `NewSession` reset, the input-focus
//!   tracking, and the `SubmitInput` / `ToggleMultiAgent` / `ToggleFastMode` /
//!   `ToggleMuteAgent` pre-work. Every branch the pre-routing does not
//!   intercept still falls through to the S39 pure-delegation tail
//!   ([`App::update_chat_tail`]) unchanged.
//! * [`App::handle_shortcut`] — the `Shortcut` arm's handler. It only
//!   re-dispatches into other `update` arms and flips a handful of small
//!   App-level routing flags (`page`, `show_help`, the two memory-modal
//!   booleans); it performs no deep state surgery (no backend call, no lock,
//!   no child-state rebuild), so it is in scope for this routing submodule.
//!
//! Bodies moved verbatim at the same 12-space arm indent, so each arm is
//! line-for-line with its origin; the only structural edit is each group
//! becoming one `pub(super)` method taking the full [`Message`] and
//! re-matching it (the same pattern as `views::settings::update_mcp` and the
//! sibling `simple_updates` / `project_switch` / `delegated_updates` /
//! `event_model_updates` / `view_dispatch` / `settings_updates` /
//! `navigation_updates` groups), so every arm keeps its exact early-return
//! `Task` semantics. The parent `update` keeps one thin delegating arm per
//! group; the fallback arm in each method is unreachable through the parent
//! dispatcher and is a documented no-op. No `Message` / `App` shape change and
//! no behavior change: the tests in `app.rs`'s `mod tests` stay put and drive
//! `update()` unchanged.

use super::*;

impl App {
    /// The `Chat` arm's pre-routing (NORM S42): the branches that must run at
    /// the App level before the chat view state sees the message. It writes
    /// to the clipboard, re-dispatches the navigation / active-model /
    /// session-select / spend-refresh messages into their own arms, resets the
    /// whole session on `NewSession`, tracks input focus for the keyboard
    /// subscription, and performs the submit / multi-agent / fast-mode /
    /// mute-agent pre-work. Every other chat message falls through to the S39
    /// pure-delegation tail ([`App::update_chat_tail`]).
    ///
    /// The parent routes the `Chat` arm here. Returns the arm's `Task`
    /// unchanged; a message the parent never routes here is a documented
    /// no-op.
    pub(super) fn update_chat_pre_routing(&mut self, message: Message) -> iced::Task<Message> {
        match message {
            Message::Chat(msg) => {
                if let views::chat::Message::CopyCode(code) = &msg {
                    return iced::clipboard::write(code.clone());
                }

                if let views::chat::Message::NavigateToToolLog(_) = &msg {
                    return self.update(Message::SetSubView(views::chat::SubView::ToolLog));
                }

                if matches!(&msg, views::chat::Message::NavigateToDiff) {
                    return self.update(Message::SetSubView(views::chat::SubView::Diff));
                }

                if matches!(&msg, views::chat::Message::NavigateToSettings) {
                    return self.update(Message::Navigate(Page::Settings));
                }

                if matches!(&msg, views::chat::Message::NavigateToStudio) {
                    return self.update(Message::Navigate(Page::OrchestrationStudio));
                }

                if let views::chat::Message::SetActiveModel(model) = &msg {
                    return self.update(Message::SetActiveModel(model.clone()));
                }

                if let views::chat::Message::SelectSession(id) = &msg {
                    return self.select_session(id.clone());
                }

                // Refresh the Spend Log modal's records via the session
                // handler (the chat state has no backend access).
                if matches!(&msg, views::chat::Message::RefreshSpendLog) {
                    return self.load_spend_log();
                }

                // Intercept New Session — flush the outgoing session, reset chat
                // + agent graph, and create a fresh session.
                if matches!(&msg, views::chat::Message::NewSession) {
                    // Persist the current session's agent graph before clearing.
                    self.persist_active_agent_graph();
                    self.chat = views::chat::State::new();
                    // A fresh chat starts from the factory defaults; re-apply
                    // the display prefs (reduced-motion + muted agents) so the
                    // new session honors the current settings.
                    self.chat.set_reduced_motion(self.reduced_motion);
                    self.seed_muted_agents();
                    self.agent_graph = views::agent_graph::State::new();
                    self.tool_log = views::tool_log::State::new();
                    self.active_session_id = None;
                    self.resume_checkpoint_json = None;
                    self.reset_spend_state();
                    return iced::Task::none();
                }

                match &msg {
                    views::chat::Message::InputChanged(_) => {
                        TEXT_FOCUSED.store(true, Ordering::Relaxed);
                        self.text_focused = true;
                    }
                    views::chat::Message::SubmitInput => {
                        TEXT_FOCUSED.store(false, Ordering::Relaxed);
                        self.text_focused = false;
                    }
                    _ => {}
                }

                if matches!(&msg, views::chat::Message::SubmitInput) {
                    if self.run_status != RunStatus::Idle {
                        return iced::Task::none();
                    }
                    let user_input = self.chat.input().to_string();
                    // `/thinking` is a local accordion toggle for the current
                    // movement (collapse-all / expand-all) — never dispatched.
                    if user_input.trim() == "/thinking" {
                        self.chat.toggle_thinking_all();
                        return self
                            .chat
                            .update(views::chat::Message::InputChanged(String::new()))
                            .map(Message::Chat);
                    }
                    let chat_task =
                        self.chat.update(views::chat::Message::SubmitInput).map(Message::Chat);
                    let agent_task = self.submit_to_agent(user_input);
                    iced::Task::batch(vec![chat_task, agent_task])
                } else if matches!(&msg, views::chat::Message::ToggleMultiAgent) {
                    self.multi_agent = !self.multi_agent;
                    let mut config = self.global_config.clone();
                    config.multi_agent.get_or_insert_with(Default::default).default_enabled =
                        self.multi_agent;
                    match concerto_config::default_config_path() {
                        Some(path) => {
                            if let Err(error) = concerto_config::save_config(&config, &path) {
                                tracing::error!(%error, "failed to persist multi-agent preference");
                            } else {
                                // Reload + re-derive through the shared helper:
                                // if a project file overrides `default_enabled`,
                                // the merged result is the truth (ADR-57 §6).
                                self.reconcile_config_from_reload();
                            }
                        }
                        None => {
                            self.global_config = config.clone();
                            self.config = Some(config);
                        }
                    }
                    iced::Task::none()
                } else if matches!(&msg, views::chat::Message::ToggleFastMode) {
                    // Runtime-only toggle, mirroring CLI `-f/--fast`: unlike the
                    // multi-agent toggle it is deliberately NOT persisted to
                    // config — fast mode is a per-session choice.
                    self.fast = !self.fast;
                    iced::Task::none()
                } else if matches!(&msg, views::chat::Message::ToggleMuteAgent(_)) {
                    // Thinking-bucket mute IS persisted (`[display]
                    // muted_agents`) — unlike fast mode, the filter is a
                    // durable preference. The WAL and transcript keep every
                    // thought regardless (hide-not-delete).
                    let chat_task = self.chat.update(msg).map(Message::Chat);
                    self.persist_muted_agents();
                    chat_task
                } else {
                    // NORM S39 — the pure-delegation tail (every chat
                    // message the pre-routing above did not intercept)
                    // moved to the sibling `view_dispatch` submodule; the
                    // full `Message` is re-wrapped so the helper keeps the
                    // full-Message re-match pattern.
                    self.update_chat_tail(Message::Chat(msg))
                }
            }
            // Never routed by the parent dispatcher; documented no-op.
            _ => iced::Task::none(),
        }
    }

    pub(super) fn handle_shortcut(&mut self, shortcut: shortcuts::Shortcut) -> iced::Task<Message> {
        use shortcuts::Shortcut;
        match shortcut {
            Shortcut::NewTask => self.update(Message::Chat(views::chat::Message::NewSession)),
            Shortcut::DiffViewer => {
                let new_sub = if self.page == Page::Chat
                    && self.chat.sub_view == views::chat::SubView::Diff
                {
                    views::chat::SubView::Main
                } else {
                    views::chat::SubView::Diff
                };
                self.update(Message::SetSubView(new_sub))
            }
            Shortcut::Memory => self.update(Message::OpenMemoryModal),
            Shortcut::ToolLog => {
                let new_sub = if self.page == Page::Chat
                    && self.chat.sub_view == views::chat::SubView::ToolLog
                {
                    views::chat::SubView::Main
                } else {
                    views::chat::SubView::ToolLog
                };
                self.update(Message::SetSubView(new_sub))
            }
            Shortcut::Terminal => self.update(Message::ToggleTerminalPanel),
            Shortcut::RuntimePanels => {
                let new_sub = if self.page == Page::Chat
                    && self.chat.sub_view == views::chat::SubView::Runtime
                {
                    views::chat::SubView::Main
                } else {
                    views::chat::SubView::Runtime
                };
                self.update(Message::SetSubView(new_sub))
            }
            Shortcut::UndoRun => {
                // On the Editor page with an open file, Ctrl+Z is text undo.
                if self.page == Page::Editor && self.editor.active_file().is_some() {
                    return self.update(Message::Editor(views::code_editor::Message::Undo));
                }
                iced::Task::none()
            }
            Shortcut::EditorRedo => {
                if self.page == Page::Editor && self.editor.active_file().is_some() {
                    return self.update(Message::Editor(views::code_editor::Message::Redo));
                }
                iced::Task::none()
            }
            Shortcut::EditorFind => {
                if self.page == Page::Editor && self.editor.active_file().is_some() {
                    return self.update(Message::Editor(views::code_editor::Message::OpenFind));
                }
                iced::Task::none()
            }
            Shortcut::EditorReplace => {
                if self.page == Page::Editor && self.editor.active_file().is_some() {
                    return self.update(Message::Editor(views::code_editor::Message::OpenReplace));
                }
                iced::Task::none()
            }
            Shortcut::EditorGoto => {
                if self.page == Page::Editor && self.editor.active_file().is_some() {
                    return self.update(Message::Editor(views::code_editor::Message::OpenGoto));
                }
                iced::Task::none()
            }
            Shortcut::EditorFindNext => {
                if self.page == Page::Editor && self.editor.active_file().is_some() {
                    return self.update(Message::Editor(views::code_editor::Message::FindNext));
                }
                iced::Task::none()
            }
            Shortcut::EditorFindPrev => {
                if self.page == Page::Editor && self.editor.active_file().is_some() {
                    return self.update(Message::Editor(views::code_editor::Message::FindPrev));
                }
                iced::Task::none()
            }
            Shortcut::SubmitInput => self.update(Message::Chat(views::chat::Message::SubmitInput)),
            Shortcut::CancelDialog => {
                self.show_help = false;
                // Esc also dismisses the Memory explorer modal.
                self.memory_view_open = false;
                // ...and the read-only memory graph modal (ADR-69 slice 3).
                self.memory_graph_open = false;
                // Esc dismisses the Runtime panels modal (memory-modal parity;
                // the Diff / Tool Log overlays stay close-button-only).
                if self.page == Page::Chat && self.chat.sub_view == views::chat::SubView::Runtime {
                    return self.update(Message::SetSubView(views::chat::SubView::Main));
                }
                // On the Editor page, Esc also dismisses the find/goto bars.
                if self.page == Page::Editor {
                    let close_find =
                        self.update(Message::Editor(views::code_editor::Message::CloseFind));
                    let close_goto =
                        self.update(Message::Editor(views::code_editor::Message::CloseGoto));
                    let cancel_close = self
                        .update(Message::Editor(views::code_editor::Message::CloseTabCancelled));
                    let close_completion =
                        self.update(Message::Editor(views::code_editor::Message::CompletionClose));
                    let close_review =
                        self.update(Message::Editor(views::code_editor::Message::CloseReview));
                    return iced::Task::batch([
                        close_find,
                        close_goto,
                        cancel_close,
                        close_completion,
                        close_review,
                    ]);
                }
                iced::Task::none()
            }
            Shortcut::HelpOverlay => {
                self.show_help = !self.show_help;
                iced::Task::none()
            }
            Shortcut::Screenshot => {
                // On the Editor page with an open file, Ctrl+S means Save.
                if self.page == Page::Editor && self.editor.active_file().is_some() {
                    return self.update(Message::Editor(views::code_editor::Message::Save));
                }
                self.update(Message::TakeScreenshot)
            }
            // Ctrl+Shift+S: the screenshot chord that works on every page,
            // the Editor included (where Ctrl+S is Save). Never saves.
            Shortcut::ScreenshotAlways => self.update(Message::TakeScreenshot),
            Shortcut::Editor => {
                self.page = Page::Editor;
                iced::Task::none()
            }
            Shortcut::EditorCloseTab | Shortcut::EditorNextTab | Shortcut::EditorPreviousTab => {
                if self.page != Page::Editor {
                    return iced::Task::none();
                }
                let message = match shortcut {
                    Shortcut::EditorCloseTab => views::code_editor::Message::CloseActiveTab,
                    Shortcut::EditorNextTab => views::code_editor::Message::NextTab,
                    _ => views::code_editor::Message::PreviousTab,
                };
                self.update(Message::Editor(message))
            }
        }
    }
}
