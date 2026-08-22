use iced::widget::{
    button, column, container, pick_list, row, scrollable, text, text_input, Column,
};
use iced::{Background, Element, Length};

use crate::theme::AppTheme;
use crate::widgets::confirm_modal::{ConfirmMessage, ConfirmModal};

#[derive(Debug, Clone)]
pub enum Message {
    SearchChanged(String),
    TypeFilterChanged(MemoryEntryType),
    EntrySelected(usize),
    DeleteRequested(usize),
    DeleteConfirmed,
    DeleteCancelled,
    Refresh,
    Reindex,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryEntryType {
    All,
    SlidingWindow,
    SessionSummary,
    ProjectSummary,
    Entity,
    Fact,
}

impl MemoryEntryType {
    fn all() -> &'static [Self] {
        &[
            Self::All,
            Self::SlidingWindow,
            Self::SessionSummary,
            Self::ProjectSummary,
            Self::Entity,
            Self::Fact,
        ]
    }
}

impl std::fmt::Display for MemoryEntryType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::All => write!(f, "All"),
            Self::SlidingWindow => write!(f, "Sliding Window"),
            Self::SessionSummary => write!(f, "Session Summary"),
            Self::ProjectSummary => write!(f, "Project Summary"),
            Self::Entity => write!(f, "Entity"),
            Self::Fact => write!(f, "Fact"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct MemoryRow {
    pub id: String,
    pub content_preview: String,
    pub source: String,
    pub age: String,
    pub score: f32,
    pub entry_type: MemoryEntryType,
}

pub enum MemoryStatus {
    Idle,
    Disabled,
    Indexing { processed: usize, total: usize },
    Error(String),
}

pub struct State {
    search_query: String,
    type_filter: MemoryEntryType,
    entries: Vec<MemoryRow>,
    selected_index: Option<usize>,
    /// Delete confirmation gate (same widget the code editor uses for its
    /// delete-file flow). Armed by `Message::DeleteRequested`, resolved by
    /// Confirm/Cancel. The actual removal happens at App level after the
    /// backend invalidate succeeds.
    pub(crate) pending_delete: Option<ConfirmModal>,
    delete_target: Option<usize>,
    pub status: MemoryStatus,
    /// Whether the memory store has been loaded at least once.
    pub loaded: bool,
}

impl Default for State {
    fn default() -> Self {
        Self::new()
    }
}

impl State {
    pub fn new() -> Self {
        State {
            search_query: String::new(),
            type_filter: MemoryEntryType::All,
            entries: Vec::new(),
            selected_index: None,
            pending_delete: None,
            delete_target: None,
            status: MemoryStatus::Idle,
            loaded: false,
        }
    }

    /// Called when indexing progress is reported.
    pub fn on_indexing_progress(&mut self, processed: usize, total: usize) {
        self.status = MemoryStatus::Indexing { processed, total };
    }

    /// Called when indexing completes.
    pub fn on_indexing_completed(&mut self, _chunk_count: usize) {
        self.status = MemoryStatus::Idle;
    }

    /// Load entries from a backend query result.
    pub fn set_entries(&mut self, entries: Vec<MemoryRow>) {
        self.entries = entries;
        self.loaded = true;
    }

    pub fn search_query(&self) -> &str {
        &self.search_query
    }

    pub fn type_filter(&self) -> MemoryEntryType {
        self.type_filter
    }

    pub fn delete_target_id(&self) -> Option<String> {
        self.delete_target.and_then(|index| self.entries.get(index)).map(|entry| entry.id.clone())
    }

    pub fn remove_entry(&mut self, id: &str) {
        self.entries.retain(|entry| entry.id != id);
        self.selected_index = None;
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        if enabled {
            if matches!(self.status, MemoryStatus::Disabled) {
                self.status = MemoryStatus::Idle;
                self.loaded = false;
            }
        } else {
            self.entries.clear();
            self.selected_index = None;
            self.status = MemoryStatus::Disabled;
            self.loaded = true;
        }
    }

    pub fn update(&mut self, message: Message) -> iced::Task<Message> {
        match message {
            Message::SearchChanged(query) => {
                self.search_query = query;
            }
            Message::TypeFilterChanged(filter) => {
                self.type_filter = filter;
            }
            Message::EntrySelected(idx) => {
                self.selected_index = Some(idx);
            }
            Message::DeleteRequested(idx) => {
                self.delete_target = Some(idx);
                let preview = self
                    .delete_target
                    .and_then(|index| self.entries.get(index))
                    .map(|entry| {
                        let preview: String = entry.content_preview.chars().take(60).collect();
                        if entry.content_preview.chars().count() > 60 {
                            format!("{preview}…")
                        } else {
                            preview
                        }
                    })
                    .unwrap_or_default();
                self.pending_delete = Some(ConfirmModal::delete(
                    "Delete Memory Entry",
                    format!("\"{preview}\" will be removed from memory. This cannot be undone."),
                ));
            }
            Message::DeleteConfirmed => {
                self.pending_delete = None;
                self.delete_target = None;
            }
            Message::DeleteCancelled => {
                self.pending_delete = None;
                self.delete_target = None;
            }
            Message::Refresh => {
                // Triggers a re-query from the memory store (handled at App level)
            }
            Message::Reindex => {
                // Triggers a re-index (handled at App level)
            }
        }
        iced::Task::none()
    }

    /// Render the Memory explorer as a centered modal dialog (issue #110).
    ///
    /// Unlike the retired 280px quick-panel section, the modal has room for a
    /// wide layout: the search input and type-filter dropdown sit side by
    /// side, and entry rows are taller and more readable. All functionality is
    /// preserved — search, type filter, status line, scrollable entry list and
    /// per-entry delete. Delete confirmation renders as a proper
    /// [`ConfirmModal`] (the same widget the code editor's delete-file flow
    /// uses) instead of the old inline confirm strip.
    pub fn modal_view<'a>(&'a self, theme: &'a AppTheme) -> Element<'a, Message> {
        let palette = &theme.palette;

        // Delete confirmation — a full ConfirmModal replaces the view content
        // while armed (same pattern as the code editor's delete-file flow).
        if let Some(modal) = &self.pending_delete {
            return modal.view().map(|msg| match msg {
                ConfirmMessage::Confirm => Message::DeleteConfirmed,
                ConfirmMessage::Cancel => Message::DeleteCancelled,
            });
        }

        let search = text_input("Search memory...", &self.search_query)
            .on_input(Message::SearchChanged)
            .width(Length::Fill);

        let filter =
            pick_list(MemoryEntryType::all(), Some(self.type_filter), Message::TypeFilterChanged)
                .width(Length::Fixed(190.0));

        let refresh_btn = button(text("⟳").size(13))
            .style(crate::ui::button::secondary)
            .on_press(Message::Refresh);

        let reindex_btn = button(text("⇪").size(13))
            .style(crate::ui::button::secondary)
            .on_press(Message::Reindex);

        let controls = row![search, filter, refresh_btn, reindex_btn].spacing(8);

        // Status indicator
        let status_text = match &self.status {
            MemoryStatus::Indexing { processed, total } => {
                format!("Indexing: {}/{} files...", processed, total)
            }
            MemoryStatus::Error(e) => format!("Error: {}", e),
            MemoryStatus::Disabled => "Memory is disabled in Settings".to_string(),
            MemoryStatus::Idle => String::new(),
        };

        let list: Element<'_, Message> = if matches!(self.status, MemoryStatus::Disabled) {
            crate::ui::empty_state_compact(
                theme,
                "◇",
                "Memory is disabled",
                "Enable Memory Settings to index and retrieve project context.",
            )
        } else if self.entries.is_empty() && !self.loaded {
            crate::ui::empty_state_compact(
                theme,
                "◇",
                "Memory is being indexed",
                "Project code is indexed in the background as you work; indexed content powers retrieval. Use Re-index to refresh now.",
            )
        } else if self.entries.is_empty() {
            crate::ui::empty_state_compact(
                theme,
                "◇",
                "No memory entries found",
                "Memory entries will appear here after the agent runs.",
            )
        } else {
            let rows: Vec<Element<'_, Message>> = self
                .entries
                .iter()
                .enumerate()
                .filter(|(_, entry)| {
                    self.type_filter == MemoryEntryType::All || entry.entry_type == self.type_filter
                })
                .map(|(i, entry)| {
                    let selected = self.selected_index == Some(i);
                    let row_content = row![
                        column![
                            text(&entry.content_preview).size(13),
                            text(&entry.source).size(11).color(palette.text_muted),
                        ]
                        .spacing(3)
                        .width(Length::Fill),
                        text(format!("{:.0}%", entry.score * 100.0)).size(11),
                        button(text("✕").size(11))
                            .style(crate::ui::button::secondary)
                            .on_press(Message::DeleteRequested(i)),
                    ]
                    .spacing(10)
                    .align_y(iced::Alignment::Center)
                    .padding(6);

                    if selected {
                        container(row_content)
                            .width(Length::Fill)
                            .style(move |_theme: &iced::Theme| container::Style {
                                background: Some(Background::Color(palette.primary)),
                                ..container::Style::default()
                            })
                            .into()
                    } else {
                        container(row_content).width(Length::Fill).into()
                    }
                })
                .collect();

            scrollable(Column::with_children(rows).spacing(4)).height(Length::Fill).into()
        };

        column![
            controls,
            if status_text.is_empty() {
                let empty: Element<'_, Message> = container(text("")).height(0).into();
                empty
            } else {
                container(text(status_text).size(12).color(palette.text_muted)).padding(4).into()
            },
            list,
        ]
        .spacing(8)
        .into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::AppTheme;

    fn theme() -> AppTheme {
        AppTheme::by_name("Midnight")
    }

    fn sample_state() -> State {
        let mut state = State::new();
        state.set_entries(vec![MemoryRow {
            id: "chunk-1".into(),
            content_preview: "a preview".into(),
            source: "src/main.rs".into(),
            age: String::new(),
            score: 0.9,
            entry_type: MemoryEntryType::Fact,
        }]);
        state
    }

    /// The modal renderer builds without panicking, with and without entries
    /// and while the ConfirmModal delete gate is armed.
    #[test]
    fn modal_view_renders_without_panic() {
        let _ = State::new().modal_view(&theme());
        let state = sample_state();
        let _ = state.modal_view(&theme());
        let mut state = sample_state();
        let _ = state.update(Message::DeleteRequested(0));
        let _ = state.modal_view(&theme());
    }

    /// DeleteRequested arms the ConfirmModal and records the target;
    /// DeleteCancelled clears it without mutating the entries.
    #[test]
    fn delete_flow_arms_and_cancels_confirm_modal() {
        let mut state = sample_state();
        let _ = state.update(Message::DeleteRequested(0));
        assert!(state.pending_delete.is_some());
        assert_eq!(state.delete_target, Some(0));
        let _ = state.modal_view(&theme());
        let _ = state.update(Message::DeleteCancelled);
        assert!(state.pending_delete.is_none());
        assert_eq!(state.delete_target, None);
        assert_eq!(state.entries.len(), 1);
    }

    /// DeleteConfirmed clears the gate (the backend invalidate is async and
    /// resolves at App level); `remove_entry` drops the row afterwards, which
    /// is what App applies once the delete succeeds.
    #[test]
    fn delete_confirm_clears_gate_and_remove_entry_drops_row() {
        let mut state = sample_state();
        let _ = state.update(Message::DeleteRequested(0));
        let _ = state.update(Message::DeleteConfirmed);
        assert!(state.pending_delete.is_none());
        assert_eq!(state.delete_target, None);
        assert_eq!(state.entries.len(), 1);
        state.remove_entry("chunk-1");
        assert!(state.entries.is_empty());
        assert_eq!(state.delete_target_id(), None);
    }
}
