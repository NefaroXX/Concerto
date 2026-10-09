//! View-state dispatch `App::update()` arms for [`App`] — NORM slice S39.
//!
//! This module owns the root-update arms whose body dispatches a message
//! into a child view state, extracted from the root `update` match in
//! `app.rs`:
//!
//! * [`App::update_chat_tail`] — the `Chat` arm's pure-delegation tail (the
//!   final `else` path every chat message the arm's pre-routing did not
//!   intercept falls through to: `chat.update` mapped back into
//!   `Message::Chat`). The pre-routing itself (clipboard, navigation
//!   re-dispatches, session select / spend refresh, the New Session
//!   intercept, focus tracking, and the submit/toggle intercepts) stays in
//!   the parent: those branches mutate App-level fields or re-dispatch
//!   rather than only forwarding to the child view state.
//! * [`App::update_diff`] — the `Diff` arm: `diff.update` plus the
//!   conditional VFS commit that applies an accept/reject/undo decision to
//!   the shared `VirtualFs`.
//! * [`App::update_memory`] — the `Memory` arm: the child-view passthrough
//!   plus the reindex / refresh / search / delete dispatches (each a loader
//!   task, or a passthrough batched with a loader task).
//!
//! The `Shortcut` arm is deliberately NOT extracted: its handler
//! (`App::handle_shortcut`) flips App-level fields (`show_help`, the memory
//! modals, page navigation) instead of only re-dispatching, so it is state
//! surgery rather than view-state dispatch.
//!
//! Bodies moved verbatim at the same 12-space arm indent, so each arm is
//! line-for-line with its origin; the only structural edit is each group
//! becoming one `pub(super)` method taking the full [`Message`] and
//! re-matching it (the same pattern as `views::settings::update_mcp` and
//! the sibling `simple_updates` / `project_switch` / `delegated_updates` /
//! `event_model_updates` groups), so every arm keeps its exact early-return
//! `Task` semantics. The parent `update` keeps one thin delegating arm per
//! group; the fallback arm in each method is unreachable through the parent
//! dispatcher and is a documented no-op. No `Message` / `App` shape
//! change and no behavior change: the tests in `mod tests` stay put and
//! drive `update()` unchanged.

use super::*;

impl App {
    /// The `Chat` arm's pure-delegation tail (NORM S39): every chat message
    /// the arm's pre-routing did not intercept falls through to the child
    /// view state's own `update`, and its `Task` is mapped back into the
    /// parent `Message` space (same shape as the `ToolLog` /
    /// `AgentGraph`/`Terminal` passthroughs).
    ///
    /// The parent routes the `Chat` arm's final `else` here. Returns the
    /// arm's `Task` unchanged; a message the parent never routes here is a
    /// documented no-op.
    pub(super) fn update_chat_tail(&mut self, message: Message) -> iced::Task<Message> {
        match message {
            Message::Chat(msg) => self.chat.update(msg).map(Message::Chat),
            // Never routed by the parent dispatcher; documented no-op.
            _ => iced::Task::none(),
        }
    }

    /// The `Diff` dispatch arm (NORM S39): the child view state's own
    /// `update`, followed by the conditional VFS commit that applies an
    /// accept/reject/undo decision to the shared `VirtualFs` (a commit
    /// failure is surfaced as an error toast, never swallowed).
    ///
    /// The parent routes `Diff` here. Returns the arm's `Task` unchanged;
    /// a message the parent never routes here is a documented no-op.
    pub(super) fn update_diff(&mut self, message: Message) -> iced::Task<Message> {
        match message {
            Message::Diff(msg) => {
                let needs_commit = matches!(
                    &msg,
                    views::diff::Message::AcceptHunk(_)
                        | views::diff::Message::RejectHunk(_)
                        | views::diff::Message::AcceptAll
                        | views::diff::Message::RejectAll
                        | views::diff::Message::Undo
                );
                let task = self.diff.update(msg).map(Message::Diff);
                if needs_commit {
                    if let Ok(mut vfs) = self.vfs.lock() {
                        if let Err(e) = self.diff.commit(&mut vfs) {
                            tracing::error!(error = %e, "failed to apply diff decision to VFS");
                            self.toasts.push(
                                ToastLevel::Error,
                                format!("Failed to apply diff decision: {e}"),
                            );
                        }
                    }
                }
                task
            }
            // Never routed by the parent dispatcher; documented no-op.
            _ => iced::Task::none(),
        }
    }

    /// The `Memory` dispatch arm (NORM S39): `Reindex` / `Refresh` /
    /// `DeleteConfirmed` dispatch a loader task (the delete case batches it
    /// with the child-view update), `SearchChanged` / `TypeFilterChanged`
    /// batch the child-view update with a re-query, and every other memory
    /// message is a pure passthrough into the child view state.
    ///
    /// The parent routes `Memory` here. Returns the arm's `Task` unchanged;
    /// a message the parent never routes here is a documented no-op.
    pub(super) fn update_memory(&mut self, message: Message) -> iced::Task<Message> {
        match message {
            Message::Memory(msg) => match msg {
                views::memory::Message::Reindex => self.trigger_reindex(),
                views::memory::Message::Refresh => self.load_memory_entries(),
                views::memory::Message::SearchChanged(_)
                | views::memory::Message::TypeFilterChanged(_) => {
                    let update = self.memory.update(msg).map(Message::Memory);
                    iced::Task::batch(vec![update, self.load_memory_entries()])
                }
                views::memory::Message::DeleteConfirmed => {
                    let id = self.memory.delete_target_id();
                    let update = self.memory.update(msg).map(Message::Memory);
                    if let Some(id) = id {
                        iced::Task::batch(vec![update, self.delete_memory_entry(id)])
                    } else {
                        update
                    }
                }
                other => self.memory.update(other).map(Message::Memory),
            },
            // Never routed by the parent dispatcher; documented no-op.
            _ => iced::Task::none(),
        }
    }
}
