use camino::{Utf8Path, Utf8PathBuf};
use concerto_tools::virtual_fs::VirtualFs;
use concerto_tools::virtual_fs::VirtualFsSnapshot;
use iced::widget::{button, column, container, row, space, text};
use iced::{Element, Length};
use std::collections::HashMap;

use crate::theme::AppTheme;
use crate::widgets::diff_viewer::DiffLine;
use crate::widgets::{diff_viewer, file_tree};

/// Decisions a user can make for a hunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HunkDecision {
    Pending,
    Accepted,
    Rejected,
}

/// Messages emitted by the Diff view.
#[derive(Debug, Clone)]
pub enum Message {
    FileSelected(Utf8PathBuf),
    AcceptHunk(u32),
    RejectHunk(u32),
    AcceptAll,
    RejectAll,
    Undo,
    /// Reports the current vertical scroll offset (in points).
    Scrolled(f32),
}

/// Internal state for the diff view.
pub struct State {
    /// All files that have a diff.
    pub files: Vec<Utf8PathBuf>,
    /// Currently selected file.
    pub active_file: Option<Utf8PathBuf>,
    /// Decisions per (file, hunk_index).
    decisions: HashMap<(Utf8PathBuf, u32), HunkDecision>,
    /// Diff lines for the active file.
    pub diff_lines: Vec<DiffLine>,
    /// Whether we have a real diff provider available.
    pub has_real_diff: bool,
    /// Current vertical scroll offset for viewport-aware rendering.
    scroll_y: f32,
    review_snapshot: Option<VirtualFsSnapshot>,
    /// All diff results for all changed files (needed to reload when switching
    /// between files).
    diff_results: Vec<concerto_api_types::diff::DiffResult>,
}

impl Default for State {
    fn default() -> Self {
        Self::new()
    }
}

impl State {
    pub fn new() -> Self {
        Self {
            files: Vec::new(),
            active_file: None,
            decisions: HashMap::new(),
            diff_lines: Vec::new(),
            has_real_diff: false,
            scroll_y: 0.0,
            review_snapshot: None,
            diff_results: Vec::new(),
        }
    }

    /// Load real diff data from the git tool or undo manager.
    pub fn load_diff(
        &mut self,
        files: Vec<Utf8PathBuf>,
        file: Utf8PathBuf,
        lines: Vec<DiffLine>,
        snapshot: VirtualFsSnapshot,
        diff_results: Vec<concerto_api_types::diff::DiffResult>,
    ) {
        self.files = files;
        self.active_file = Some(file);
        self.diff_lines = lines;
        self.has_real_diff = true;
        self.decisions.clear();
        self.review_snapshot = Some(snapshot);
        self.diff_results = diff_results;
    }

    /// Reload diff_lines for a specific file from the stored diff results.
    fn reload_diff_lines(&mut self, path: &Utf8Path) {
        if let Some(result) = self.diff_results.iter().find(|r| r.path == path) {
            self.diff_lines = diff_result_to_lines(result);
        } else {
            self.diff_lines.clear();
        }
    }

    /// Apply Rejected decisions to the given VirtualFs.
    ///
    /// Accepted/Pending hunks require no action since VirtualFs already holds
    /// the accepted (post-change) content. Call this when the user confirms
    /// or directly from `App::update` when a hunk decision is made.
    pub fn commit(&mut self, vfs: &mut VirtualFs) -> Result<(), concerto_core::ToolError> {
        if let Some(snapshot) = &self.review_snapshot {
            vfs.restore(snapshot.clone());
        }
        let mut rejected: HashMap<Utf8PathBuf, Vec<usize>> = HashMap::new();
        for ((file, hunk_idx), decision) in &self.decisions {
            if *decision == HunkDecision::Rejected {
                rejected.entry(file.clone()).or_default().push(*hunk_idx as usize);
            }
        }
        for (file, mut hunks) in rejected {
            hunks.sort_unstable();
            vfs.reject_hunks(&file, &hunks)?;
        }
        vfs.materialize_paths(&self.files)
    }

    pub fn update(&mut self, message: Message) -> iced::Task<Message> {
        match message {
            Message::FileSelected(path) => {
                self.active_file = Some(path.clone());
                self.reload_diff_lines(&path);
                self.scroll_y = 0.0;
                iced::Task::none()
            }
            Message::AcceptHunk(idx) => {
                if let Some(file) = &self.active_file {
                    self.decisions.insert((file.clone(), idx), HunkDecision::Accepted);
                }
                iced::Task::none()
            }
            Message::RejectHunk(idx) => {
                if let Some(file) = &self.active_file {
                    self.decisions.insert((file.clone(), idx), HunkDecision::Rejected);
                }
                iced::Task::none()
            }
            Message::AcceptAll => {
                if let Some(file) = &self.active_file {
                    for idx in reviewable_hunks(&self.diff_lines) {
                        self.decisions.insert((file.clone(), idx), HunkDecision::Accepted);
                    }
                }
                iced::Task::none()
            }
            Message::RejectAll => {
                if let Some(file) = &self.active_file {
                    for idx in reviewable_hunks(&self.diff_lines) {
                        self.decisions.insert((file.clone(), idx), HunkDecision::Rejected);
                    }
                }
                iced::Task::none()
            }
            Message::Undo => {
                if let Some(file) = &self.active_file {
                    self.decisions.retain(|(f, _), _| f != file);
                }
                iced::Task::none()
            }
            Message::Scrolled(y) => {
                self.scroll_y = y;
                iced::Task::none()
            }
        }
    }

    pub fn view<'a>(&'a self, theme: &'a AppTheme) -> Element<'a, Message> {
        let palette = &theme.palette;

        if self.files.is_empty() && !self.has_real_diff {
            return crate::ui::empty_state(
                theme,
                "∆",
                "No diffs available",
                "Changes will appear here when files are modified.",
                None,
            );
        }

        // Header with file path and action buttons.
        let header = row![
            text(self.active_file.as_ref().map(|p| p.as_str()).unwrap_or("<no file>")).size(24),
            space::horizontal(),
            button("Accept All").on_press(Message::AcceptAll),
            button("Reject All").on_press(Message::RejectAll),
            button("Undo").on_press(Message::Undo),
        ]
        .spacing(10)
        .padding(5);

        // Sidebar file tree.
        let sidebar = file_tree::flat_view(&self.files, self.active_file.as_deref());

        // Compute visible range from scroll offset for row virtualization.
        const ESTIMATED_LINE_HEIGHT: f32 = 20.0;
        const VISIBLE_LINES_EST: usize = 30;
        const SCROLL_BUFFER: usize = 10;
        let visible_range = if !self.diff_lines.is_empty() {
            let len = self.diff_lines.len();
            let start = ((self.scroll_y / ESTIMATED_LINE_HEIGHT).floor() as usize)
                .min(len.saturating_sub(1));
            let count = (VISIBLE_LINES_EST + SCROLL_BUFFER).min(len.saturating_sub(start));
            Some(start..start + count)
        } else {
            None
        };

        // Main diff area - side by side (row, not column)
        let diff_area = if self.active_file.is_some() && !self.diff_lines.is_empty() {
            let file_decisions: HashMap<usize, HunkDecision> = self
                .decisions
                .iter()
                .filter(|((f, _), _)| Some(f.as_path()) == self.active_file.as_deref())
                .map(|((_, idx), d)| (*idx as usize, *d))
                .collect();
            diff_viewer::view(
                &self.diff_lines,
                palette.success,
                palette.danger,
                palette.secondary,
                visible_range,
                Some(Message::Scrolled),
                &file_decisions,
            )
        } else if self.active_file.is_some() {
            text("No diff content available for this file.").into()
        } else {
            text("Select a file to view diff").into()
        };

        // Layout: sidebar on the left, diff on the right (side by side).
        let content = row![
            container(sidebar).width(Length::FillPortion(1)).height(Length::Fill),
            container(diff_area).width(Length::FillPortion(3)).height(Length::Fill),
        ]
        .spacing(5);

        column![header, space::vertical(), content].spacing(5).into()
    }
}

/// Convert an API [`concerto_api_types::diff::DiffResult`] into the widget-level
/// [`DiffLine`] representation used by the diff viewer.
pub(crate) fn diff_result_to_lines(result: &concerto_api_types::diff::DiffResult) -> Vec<DiffLine> {
    use concerto_api_types::diff::DiffLine as ApiDiffLine;

    let mut all_lines = Vec::new();
    let mut change_hunk_index = 0u32;

    for hunk in &result.hunks {
        let is_change = hunk.lines.iter().any(|line| !matches!(line, ApiDiffLine::Context { .. }));
        if is_change {
            all_lines.push(DiffLine {
                before_number: Some(hunk.old_start as usize),
                after_number: Some(hunk.new_start as usize),
                before_content: format!(
                    "@@ -{},{} +{},{} @@",
                    hunk.old_start, hunk.old_len, hunk.new_start, hunk.new_len
                ),
                after_content: format!(
                    "@@ -{},{} +{},{} @@",
                    hunk.old_start, hunk.old_len, hunk.new_start, hunk.new_len
                ),
                kind: diff_viewer::DiffKind::HunkHeader,
                hunk_index: Some(change_hunk_index),
            });
            change_hunk_index += 1;
        }

        for line in &hunk.lines {
            match line {
                ApiDiffLine::Addition { content, line_num } => {
                    all_lines.push(DiffLine {
                        before_number: None,
                        after_number: Some(*line_num as usize),
                        before_content: String::new(),
                        after_content: content.clone(),
                        kind: diff_viewer::DiffKind::Addition,
                        hunk_index: None,
                    });
                }
                ApiDiffLine::Deletion { content, line_num } => {
                    all_lines.push(DiffLine {
                        before_number: Some(*line_num as usize),
                        after_number: None,
                        before_content: content.clone(),
                        after_content: String::new(),
                        kind: diff_viewer::DiffKind::Deletion,
                        hunk_index: None,
                    });
                }
                ApiDiffLine::Context { content, line_num } => {
                    all_lines.push(DiffLine {
                        before_number: Some(*line_num as usize),
                        after_number: Some(*line_num as usize),
                        before_content: content.clone(),
                        after_content: content.clone(),
                        kind: diff_viewer::DiffKind::Unchanged,
                        hunk_index: None,
                    });
                }
                _ => {}
            }
        }
    }

    all_lines
}

fn reviewable_hunks(lines: &[DiffLine]) -> impl Iterator<Item = u32> + '_ {
    lines.iter().filter_map(|line| line.hunk_index)
}
