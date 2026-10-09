//! The memory-result arms for [`App`] — NORM slice S44.
//!
//! This module owns the four async memory/plugin outcome arms of the root
//! `update` match: `ReindexResult` (the manual re-index outcome — Idle +
//! `loaded` + an entry reload on `Done`, Error on `Failed`, Idle on `Skipped`),
//! the `MemoryEntriesLoaded` loader result (rows or an error state), the
//! `PluginProvidersRefreshed` log-only placeholder, and `MemoryEntryDeleted`
//! (row removal or an error state).
//!
//! Bodies moved verbatim from `app.rs` — at the same 12-space arm indent, so
//! the copies are line-for-line with their origins — and the only structural
//! edit is the group becoming one `pub(super)` method taking the full
//! [`Message`] and re-matching it (the same pattern as the sibling
//! `simple_updates` / `project_switch` / `delegated_updates` /
//! `event_model_updates` / `view_dispatch` / `settings_updates` /
//! `navigation_updates` / `update_routing` / `run_completion` groups), so
//! every arm keeps its exact `Task` semantics. The parent `update` keeps one
//! thin delegating arm; the fallback arm here is unreachable through the
//! parent dispatcher and is a documented no-op. No `Message` / `App` shape
//! change and no behavior change: the helpers the arms and their sibling
//! `Memory` dispatch call (`trigger_reindex`, `load_memory_entries`,
//! `delete_memory_entry`) and the memory setters stay in `app.rs`, and the
//! tests in `app.rs`'s `mod tests` stay put and drive `update()` unchanged.

use super::*;

impl App {
    /// The memory-result group (NORM S44): `ReindexResult`,
    /// `MemoryEntriesLoaded`, `PluginProvidersRefreshed`, and
    /// `MemoryEntryDeleted`.
    ///
    /// The parent routes all four here. The re-index outcome drives the
    /// memory status/loaded flags and re-loads the entry list on success; the
    /// loader result stores the rows or surfaces the error; the plugin
    /// refresh completion is a log-only placeholder; and a delete result
    /// removes the row or surfaces the error. Returns each arm's `Task`
    /// unchanged; a message the parent never routes here is a documented
    /// no-op.
    pub(super) fn update_memory_results(&mut self, message: Message) -> iced::Task<Message> {
        match message {
            Message::ReindexResult(outcome) => {
                match outcome {
                    ReindexResult::Done(_) => {
                        self.memory.status = MemoryStatus::Idle;
                        self.memory.loaded = true;
                        return self.load_memory_entries();
                    }
                    ReindexResult::Failed(e) => {
                        self.memory.status = MemoryStatus::Error(e);
                    }
                    ReindexResult::Started => {}
                    ReindexResult::Skipped => {
                        self.memory.status = MemoryStatus::Idle;
                    }
                }
                iced::Task::none()
            }
            Message::MemoryEntriesLoaded(result) => {
                match result {
                    Ok(entries) => {
                        self.memory.set_entries(entries);
                        self.memory.status = MemoryStatus::Idle;
                    }
                    Err(error) => self.memory.status = MemoryStatus::Error(error),
                }
                iced::Task::none()
            }
            Message::PluginProvidersRefreshed => {
                // Log-only outcome (the refresh task logs its own results);
                // kept as a message so future UI feedback needs no wiring
                // change.
                iced::Task::none()
            }
            Message::MemoryEntryDeleted { id, result } => {
                match result {
                    Ok(()) => self.memory.remove_entry(&id),
                    Err(error) => self.memory.status = MemoryStatus::Error(error),
                }
                iced::Task::none()
            }
            // Never routed by the parent dispatcher; documented no-op.
            _ => iced::Task::none(),
        }
    }
}
