//! Thin-delegation `App::update()` arms for [`App`] — NORM slice S37.
//!
//! This module owns the root-update arms whose entire body is a delegation
//! to an existing submodule / view update or to a shared dialog resolver,
//! extracted from the root `update` match in `app.rs`:
//!
//! * [`App::update_orchestration`] — `ImportProjectOrchestration`,
//!   `DismissOrchestrationBanner`, `OrchestrationStudio` (Studio dispatch +
//!   guarded Save persist + toasts), and `StudioRuntimeLoaded`.
//! * [`App::update_editor`] — the `Editor` arm: `TakeScreenshot`
//!   re-dispatch, `editor.update`, and the staged-diff reload.
//! * [`App::update_capability_dialogs`] — `CapabilityDlg`, `AckDialog`,
//!   `IntentDialog`, and `PlanDialog` (the shared pending-queue resolvers).
//! * [`App::update_tool_log`] — the `ToolLog` child-view passthrough
//!   one-liner, the last U11-style passthrough left in the parent match.
//!
//! Bodies moved verbatim at the same 12-space arm indent, so each arm is
//! line-for-line with its origin; the only structural edit is each group
//! becoming one `pub(super)` method taking the full [`Message`] and
//! re-matching it (the same pattern as `views::settings::update_mcp` and
//! the sibling `simple_updates` / `project_switch` groups), so every arm
//! keeps its exact early-return `Task` semantics. The parent `update` keeps
//! one thin delegating OR-pattern arm per group; the fallback arm in each
//! method is unreachable through the parent dispatcher and is a documented
//! no-op. No `Message` / `App` shape change and no behavior change: the
//! tests in `app.rs`'s `mod tests` stay put and drive `update()` unchanged.

use super::*;

impl App {
    /// The thin-delegation orchestration group (NORM S37): the banner's
    /// import action, the banner dismiss, the Studio dispatch (including
    /// the single-arm Save's guarded persist + success/failure toasts), and
    /// the stale-checked runtime-snapshot load.
    ///
    /// The parent routes `ImportProjectOrchestration`,
    /// `DismissOrchestrationBanner`, `OrchestrationStudio`, and
    /// `StudioRuntimeLoaded` here. Returns the arm's `Task` unchanged; a
    /// message the parent never routes here is a documented no-op.
    pub(super) fn update_orchestration(&mut self, message: Message) -> iced::Task<Message> {
        match message {
            Message::ImportProjectOrchestration => {
                self.run_project_orchestration_import();
                iced::Task::none()
            }
            Message::DismissOrchestrationBanner => {
                self.orchestration_banner_dismissed = true;
                iced::Task::none()
            }
            Message::OrchestrationStudio(msg) => {
                // ADR-58/59 (rewritten) Slice 2 (single-arm Save), AMENDED
                // (global-only orchestration): `SaveOrchestration` persists
                // the Studio's editable blueprint via `persist_orchestration`,
                // which writes to the GLOBAL config (include → guarded include
                // write, name/inline → materialize inline into the global
                // config) + the roster to the global config, validates,
                // writes, and reloads — never navigating, never switching the
                // surface, and never creating a project `.concerto.toml`
                // (project-layer orchestration keys are now ignored at load;
                // a Save is refused while a project file still declares
                // `[orchestration]` — the banner import owns the relocation).
                // There is no init path anymore: the
                // roster auto-seeds globally on Studio open.
                let persist =
                    matches!(msg, views::orchestration_studio::StudioMessage::SaveOrchestration);
                let task = self.orchestration_studio.update(msg);
                if persist {
                    match self.persist_orchestration() {
                        Ok(()) => {
                            self.orchestration_studio.mark_saved();
                            self.toasts.push(ToastLevel::Success, "Orchestration saved".into());
                        }
                        Err(error) => {
                            // Every write is atomic and nothing is written on
                            // failure — surface the reason persistently so the
                            // draft is kept, not lost.
                            self.toasts.push(ToastLevel::Error, format!("Save failed: {error}"));
                            self.orchestration_studio.mark_save_failed(error);
                        }
                    }
                }
                task
            }
            Message::StudioRuntimeLoaded(session_id, snapshot) => {
                // Discard a stale load: only apply the snapshot whose session
                // still matches the active one.
                if session_id == self.active_session_id {
                    self.orchestration_studio.set_runtime_snapshot(*snapshot);
                }
                iced::Task::none()
            }
            // Never routed by the parent dispatcher; documented no-op.
            _ => iced::Task::none(),
        }
    }

    /// The `Editor` dispatch arm (NORM S37): the toolbar screenshot
    /// re-dispatch into the app-level capture, the delegated
    /// `editor.update`, and the staged-diff reload after a state-changing
    /// editor action.
    ///
    /// The parent routes `Editor` here. Returns the arm's `Task` unchanged;
    /// a message the parent never routes here is a documented no-op.
    pub(super) fn update_editor(&mut self, message: Message) -> iced::Task<Message> {
        match message {
            Message::Editor(msg) => match msg {
                // The Editor toolbar's screenshot button is an app-level
                // action (whole-window capture), so it never enters the
                // editor state — the Editor page is the one page whose
                // Ctrl+S means Save, and this is its screenshot access.
                views::code_editor::Message::TakeScreenshot => self.update(Message::TakeScreenshot),
                msg => {
                    let refresh_diff = matches!(
                        msg,
                        views::code_editor::Message::AcceptStaged
                            | views::code_editor::Message::DiscardStaged
                            | views::code_editor::Message::Save
                            | views::code_editor::Message::DeleteConfirmed
                    );
                    let task = self
                        .editor
                        .update(
                            msg,
                            &self.vfs,
                            &Utf8PathBuf::from_path_buf(self.project_dir.clone()).unwrap_or_else(
                                |p| Utf8PathBuf::from(p.to_string_lossy().as_ref()),
                            ),
                            &self.cancel_token,
                        )
                        .map(Message::Editor);
                    if refresh_diff {
                        self.load_diff_from_vfs();
                    }
                    task
                }
            },
            // Never routed by the parent dispatcher; documented no-op.
            _ => iced::Task::none(),
        }
    }

    /// The capability / ack / intent / plan dialog resolver group (NORM
    /// S37): each arm answers the dialog that is actually displayed through
    /// the widget's shared `resolve*` helpers, so a stale or cross-session
    /// queue entry can never answer a different run's prompt.
    ///
    /// The parent routes `CapabilityDlg`, `AckDialog`, `IntentDialog`, and
    /// `PlanDialog` here. Returns the arm's `Task` unchanged; a message the
    /// parent never routes here is a documented no-op.
    pub(super) fn update_capability_dialogs(&mut self, message: Message) -> iced::Task<Message> {
        match message {
            Message::CapabilityDlg(msg) => {
                capability_dialog::resolve(&self.cap_pending, &msg);
                iced::Task::none()
            }
            Message::AckDialog(msg) => {
                // Resolve only the ack that is actually displayed: capture the
                // pending ack's session identity so a stale or cross-session
                // entry can never answer a different run's prompt (ADR-68,
                // audit H-04).
                let session_id = {
                    let guard = self.pending_ack.lock().unwrap_or_else(|e| e.into_inner());
                    guard.front().map(|ack| ack.session_id)
                };
                if let Some(session_id) = session_id {
                    let acknowledged =
                        matches!(msg, capability_dialog::AckDialogMessage::Acknowledge);
                    capability_dialog::resolve_ack(&self.pending_ack, session_id, acknowledged);
                }
                iced::Task::none()
            }
            Message::IntentDialog(msg) => {
                capability_dialog::resolve_intent(&self.pending_intent, msg);
                iced::Task::none()
            }
            Message::PlanDialog(msg) => {
                // Resolve only the dialog that is actually displayed: capture
                // the front entry's identity so a stale or cross-session queue
                // entry can never answer a different prompt.
                let identity = {
                    let guard = self.pending_plan.lock().unwrap_or_else(|e| e.into_inner());
                    guard.front().map(|plan| (plan.session_id, plan.plan_id.clone()))
                };
                if let Some((session_id, plan_id)) = identity {
                    capability_dialog::resolve_plan(&self.pending_plan, session_id, &plan_id, msg);
                }
                iced::Task::none()
            }
            // Never routed by the parent dispatcher; documented no-op.
            _ => iced::Task::none(),
        }
    }

    /// The `ToolLog` child-view passthrough (NORM S37): a pure forward —
    /// the child state's own `update` runs and its `Task` is mapped back
    /// into the parent `Message` space (same shape as the
    /// `AgentGraph`/`Terminal` passthroughs in `simple_updates`).
    ///
    /// The parent routes `ToolLog` here. Returns the arm's `Task` unchanged;
    /// a message the parent never routes here is a documented no-op.
    pub(super) fn update_tool_log(&mut self, message: Message) -> iced::Task<Message> {
        match message {
            Message::ToolLog(msg) => self.tool_log.update(msg).map(Message::ToolLog),
            // Never routed by the parent dispatcher; documented no-op.
            _ => iced::Task::none(),
        }
    }
}
