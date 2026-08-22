//! Code Editor — file tree sidebar, syntax-highlighted editor, and LSP integration.
//!
//! This view provides:
//! - A hierarchical file tree (expand/collapse directories, click to open files)
//! - A multi-line code editor with syntax highlighting via `iced_highlighter`
//! - File operations: open, save, new file, delete
//! - LSP integration: hover, diagnostics, go-to-definition (via `concerto-lsp`)

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;

use camino::{Utf8Path, Utf8PathBuf};
use iced::keyboard::{key::Named, Key};
use iced::widget::pane_grid;
use iced::widget::text_editor;
use iced::Color;

use crate::widgets::confirm_modal::ConfirmModal;
use crate::widgets::file_tree::TreeNode;
use concerto_core::helpers::ProjectIdHelper;
use concerto_core::CancellationToken;
use concerto_lsp::LspManager;

mod editor_core;
mod editor_view;
mod helpers;
mod message;
mod text_helpers;

use concerto_tools::virtual_fs::VirtualFs;
pub use message::Message;
pub use text_helpers::*;

/// A diagnostic from the LSP server.
#[derive(Debug, Clone)]
pub struct Diagnostic {
    pub line: usize,
    pub character: usize,
    pub message: String,
    pub severity: DiagnosticSeverity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticSeverity {
    Error,
    Warning,
    Information,
    Hint,
}

impl DiagnosticSeverity {
    fn color(&self, palette: &crate::theme::Palette) -> Color {
        match self {
            DiagnosticSeverity::Error => palette.danger,
            DiagnosticSeverity::Warning => palette.warning,
            DiagnosticSeverity::Information => palette.secondary,
            DiagnosticSeverity::Hint => palette.text_muted,
        }
    }

    fn label(&self) -> &'static str {
        match self {
            DiagnosticSeverity::Error => "Error",
            DiagnosticSeverity::Warning => "Warning",
            DiagnosticSeverity::Information => "Info",
            DiagnosticSeverity::Hint => "Hint",
        }
    }
}

/// Classification of text edits, used to group undo-history entries so a
/// typing burst collapses into a single undo unit while structural edits
/// (newline, paste, indent) always start a fresh entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditKind {
    InsertChar,
    Newline,
    Delete,
    Paste,
    Indent,
}

/// A point-in-time snapshot of the buffer for undo/redo. Snapshot-based (not
/// diff-based) because iced's `text_editor::Content` exposes no change API;
/// bounded to `MAX_HISTORY` entries so memory stays predictable.
#[derive(Debug, Clone)]
pub(crate) struct HistoryEntry {
    text: String,
    cursor_line: usize,
    cursor_index: usize,
}

/// Maximum number of undo snapshots retained.
const MAX_HISTORY: usize = 200;

/// Buffers larger than this skip match scanning so typing stays responsive.
pub(crate) const MAX_SEARCH_BYTES: usize = 1_000_000;

/// Bracket-pair scanning stops this many bytes from the cursor.
pub(crate) const MAX_BRACKET_SCAN: usize = 256 * 1024;

/// Match collection stops at this many hits (status shows a "+" overflow).
pub(crate) const MAX_MATCHES: usize = 10_000;

/// Widget ID of the find-bar text input (for focus tasks).
pub(crate) const FIND_INPUT_ID: &str = "editor-find-input";

/// Widget ID of the go-to-line text input (for focus tasks).
pub(crate) const GOTO_INPUT_ID: &str = "editor-goto-input";

/// Default share of the split width given to the file-tree pane.
pub(crate) const TREE_PANE_DEFAULT_RATIO: f32 = 0.22;

/// Clamp bounds for the tree/editor divider. `pane_grid` exposes no true
/// min-width API, so the floor is enforced on the divider ratio: the tree
/// never drops below 15% and the editor never below 35% of the split (the
/// `min_size` pixels floor on the widget is the secondary guard).
pub(crate) const TREE_PANE_MIN_RATIO: f32 = 0.15;
pub(crate) const TREE_PANE_MAX_RATIO: f32 = 0.65;

/// Default share of the editor|diag split given to the diagnostics pane
/// (#108). The split ratio itself is the *editor's* share of that width, so
/// the pane is initialized at `1 - DIAG_PANE_DEFAULT_SHARE`.
pub(crate) const DIAG_PANE_DEFAULT_SHARE: f32 = 0.20;

/// Clamp bounds for the editor|diag divider, expressed as the diagnostics
/// pane's share of the split width. These keep the column narrow and the
/// editor dominant regardless of how far the divider is dragged.
pub(crate) const DIAG_PANE_MIN_SHARE: f32 = 0.1;
pub(crate) const DIAG_PANE_MAX_SHARE: f32 = 0.35;

/// One occurrence of the find query in the buffer. The byte `offset` makes
/// replace operations O(n); `line`/`col` drive jumping and display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FindMatch {
    pub(crate) offset: usize,
    pub(crate) line: usize,
    pub(crate) col: usize,
}

/// Bracket-pair state at the cursor, recomputed on cursor movement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BracketStatus {
    /// Cursor is not adjacent to a bracket.
    None,
    /// The bracket pair closed at the given 0-based line.
    Matched { other_line: usize, other_col: usize },
    /// No partner found within the scan window.
    Unmatched,
}

/// A completion item from the LSP server.
#[derive(Debug, Clone)]
pub struct CompletionItem {
    pub label: String,
    pub detail: Option<String>,
    pub insert_text: String,
}

/// A folded region. `start` is the anchor's line number in the CURRENT
/// (folded) display buffer; the placeholder line sits at `start + 1`.
/// `hidden_text` (lines `start+1..` from the original buffer, joined by
/// newlines) lets expansion restore the exact original content.
///
/// iced's `text_editor` cannot hide lines, so folding substitutes buffer
/// text. Safety rules: any edit first expands intersecting folds (so undo
/// snapshots never contain placeholders), and Save always expands first (so
/// placeholders never reach disk).
#[derive(Debug, Clone)]
pub struct ActiveFold {
    pub(crate) start: usize,
    pub(crate) hidden_count: usize,
    pub(crate) hidden_text: String,
}

/// How the Tab key behaves in the editor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabMode {
    /// Insert a hard tab / indent the line.
    Tabs,
    /// Insert N spaces at the cursor.
    Spaces(usize),
}

impl TabMode {
    fn label(self) -> String {
        match self {
            TabMode::Tabs => "Tabs".to_string(),
            TabMode::Spaces(n) => format!("Spaces:{n}"),
        }
    }

    fn cycle(self) -> Self {
        match self {
            TabMode::Tabs => TabMode::Spaces(2),
            TabMode::Spaces(2) => TabMode::Spaces(4),
            TabMode::Spaces(4) => TabMode::Spaces(8),
            TabMode::Spaces(_) => TabMode::Tabs,
        }
    }

    pub(crate) fn width(self) -> usize {
        match self {
            TabMode::Tabs => 4,
            TabMode::Spaces(n) => n,
        }
    }
}

/// Internal editor state.
pub struct State {
    /// The file tree root node.
    pub(crate) tree: TreeNode,
    /// Whether the tree needs to be rebuilt from disk.
    pub(crate) tree_dirty: bool,
    /// The currently open file path.
    pub(crate) active_file: Option<Utf8PathBuf>,
    /// The text editor content.
    pub(crate) content: Option<text_editor::Content>,
    /// The language of the open file.
    pub(crate) lang: &'static str,
    /// Whether the file has unsaved changes.
    pub(crate) dirty: bool,
    /// LSP diagnostics for the open file.
    pub(crate) diagnostics: Vec<Diagnostic>,
    /// Hover text (if any).
    pub(crate) hover: Option<String>,
    /// Whether the diagnostics panel is visible.
    pub(crate) show_diagnostics: bool,
    /// LSP version counter for didChange notifications.
    pub(crate) lsp_version: i32,
    /// Undo snapshots (oldest at front).
    pub(crate) undo_stack: VecDeque<HistoryEntry>,
    /// Redo snapshots (most recent at back).
    pub(crate) redo_stack: Vec<HistoryEntry>,
    /// The kind of the last applied edit, for history grouping.
    pub(crate) last_edit_kind: Option<EditKind>,
    /// Tab key behavior.
    pub(crate) tab_mode: TabMode,
    /// Whether to strip trailing whitespace when saving.
    pub(crate) trim_trailing_on_save: bool,
    /// Whether the find bar is visible.
    pub(crate) find_open: bool,
    /// Whether the replace row is visible (implies `find_open`).
    pub(crate) replace_open: bool,
    /// Current find query.
    pub(crate) find_query: String,
    /// Case-sensitive matching toggle.
    pub(crate) find_case_sensitive: bool,
    /// Matches of `find_query` in the current buffer.
    pub(crate) find_matches: Vec<FindMatch>,
    /// Index of the current match (for next/prev and replace).
    pub(crate) find_current: Option<usize>,
    /// Whether `find_matches` was truncated at MAX_MATCHES.
    pub(crate) find_overflow: bool,
    /// Current replacement text.
    pub(crate) replace_query: String,
    /// Whether the go-to-line bar is visible.
    pub(crate) goto_open: bool,
    /// Current go-to-line input.
    pub(crate) goto_input: String,
    /// Bracket-pair state at the cursor.
    pub(crate) bracket_status: BracketStatus,
    /// Line indices containing the word under the cursor (for gutter marks).
    pub(crate) word_occurrences: Vec<usize>,
    /// The word under the cursor, if any (for the status bar).
    pub(crate) current_word: Option<String>,
    /// Active folded regions (display-line anchored, sorted ascending).
    pub(crate) folds: Vec<ActiveFold>,
    /// Foldable-region cache keyed by `(lsp_version, line_count)` so view()
    /// doesn't rescan the buffer every frame.
    pub(crate) region_cache: (i32, usize, Vec<(usize, usize)>),
    /// Whether the LSP completion popup is open.
    pub(crate) completion_open: bool,
    /// Available completion items.
    pub(crate) completion_items: Vec<CompletionItem>,
    /// Index of the selected completion item.
    pub(crate) completion_selected: usize,
    /// When `Some`, the destructive delete awaits confirmation (the modal is
    /// rendered from this; `DeleteConfirmed`/`DeleteCancelled` resolve it).
    pub(crate) pending_delete: Option<ConfirmModal>,
    /// pane_grid layout for the tree | editor | diagnostics split (#90, #108).
    pub(crate) pane_state: pane_grid::State<()>,
    /// The pane holding the file tree (leftmost pane).
    pub(crate) tree_pane: pane_grid::Pane,
    /// The pane holding the code editor (center pane).
    pub(crate) editor_pane: pane_grid::Pane,
    /// The pane holding the diagnostics / status column (rightmost pane).
    pub(crate) diag_pane: pane_grid::Pane,
    /// The divider between the tree and editor panes. `None` only in the
    /// degenerate single-pane fallback.
    pub(crate) tree_split: Option<pane_grid::Split>,
    /// The divider between the editor and diagnostics panes.
    pub(crate) diag_split: Option<pane_grid::Split>,
}

impl Default for State {
    fn default() -> Self {
        Self::new(Utf8PathBuf::from("."))
    }
}

impl State {
    /// Create a new editor state rooted at `project_dir`.
    pub fn new(project_dir: Utf8PathBuf) -> Self {
        let tree = TreeNode::from_disk(&project_dir);
        // The pane grid starts as a single tree pane, then splits off the
        // editor to its right and finally the diagnostics column to the
        // editor's right. A fresh single-pane grid always has a splittable
        // leaf; the fallbacks keep a valid (degenerate) layout.
        let (mut pane_state, tree_pane) = pane_grid::State::new(());
        let tree_split = pane_state.split(pane_grid::Axis::Vertical, tree_pane, ());
        let (editor_pane, tree_split) = match tree_split {
            Some((pane, split)) => {
                pane_state.resize(split, TREE_PANE_DEFAULT_RATIO);
                (pane, Some(split))
            }
            // Degenerate fallback: the tree pane doubles as the editor.
            None => (tree_pane, None),
        };
        let diag_split = pane_state.split(pane_grid::Axis::Vertical, editor_pane, ());
        let (diag_pane, diag_split) = match diag_split {
            Some((pane, split)) => {
                // The split ratio is the *editor's* share of the editor|diag
                // width; the diagnostics pane gets `DIAG_PANE_DEFAULT_SHARE`.
                pane_state.resize(split, 1.0 - DIAG_PANE_DEFAULT_SHARE);
                (pane, Some(split))
            }
            None => (editor_pane, None),
        };
        Self {
            tree,
            tree_dirty: false,
            active_file: None,
            content: None,
            lang: "plain",
            dirty: false,
            diagnostics: Vec::new(),
            hover: None,
            show_diagnostics: false,
            lsp_version: 0,
            undo_stack: VecDeque::new(),
            redo_stack: Vec::new(),
            last_edit_kind: None,
            tab_mode: TabMode::Spaces(4),
            trim_trailing_on_save: true,
            find_open: false,
            replace_open: false,
            find_query: String::new(),
            find_case_sensitive: false,
            find_matches: Vec::new(),
            find_current: None,
            find_overflow: false,
            replace_query: String::new(),
            goto_open: false,
            goto_input: String::new(),
            bracket_status: BracketStatus::None,
            word_occurrences: Vec::new(),
            current_word: None,
            folds: Vec::new(),
            region_cache: (0, 0, Vec::new()),
            completion_open: false,
            completion_items: Vec::new(),
            completion_selected: 0,
            pending_delete: None,
            pane_state,
            tree_pane,
            editor_pane,
            diag_pane,
            tree_split,
            diag_split,
        }
    }

    /// Get the currently open file path.
    pub fn active_file(&self) -> Option<&Utf8Path> {
        self.active_file.as_deref()
    }

    /// Open a file: read from VFS if staged, otherwise from disk.
    pub(crate) fn open_file(&mut self, path: &Utf8Path, vfs: &Arc<Mutex<VirtualFs>>) {
        // Try VFS first (for staged changes), then disk. A poisoned VFS lock
        // degrades gracefully to a plain disk read.
        let from_vfs = vfs.lock().ok().and_then(|guard| {
            if guard.exists(path) {
                Some(guard.read(path).unwrap_or_default())
            } else {
                None
            }
        });
        let content_text = match from_vfs {
            Some(text) => text,
            None => std::fs::read_to_string(path.as_std_path()).unwrap_or_default(),
        };

        self.active_file = Some(path.to_path_buf());
        self.content = Some(text_editor::Content::with_text(&content_text));
        self.lang = crate::widgets::file_tree::lang_for_file(path);
        self.dirty = false;
        self.diagnostics.clear();
        self.hover = None;
        // A new buffer starts with a clean history.
        self.undo_stack.clear();
        self.redo_stack.clear();
        self.last_edit_kind = None;
        self.folds.clear();
        self.completion_open = false;

        // Expand the tree to show the file.
        self.tree.expand_to(path);
        self.refresh_cursor_insights();
    }

    /// Open a file and run the full post-open LSP handshake: reset per-file
    /// state (dirty, version, diagnostics, hover) and send `textDocument/
    /// didOpen`. Shared by every file-open path so behavior cannot diverge.
    fn dispatch_file_open(
        &mut self,
        path: &Utf8Path,
        vfs: &Arc<Mutex<VirtualFs>>,
        project_dir: &Utf8Path,
        cancel: &CancellationToken,
    ) -> iced::Task<Message> {
        self.open_file(path, vfs);
        self.dirty = false;
        self.lsp_version = 0;
        self.diagnostics.clear();
        self.hover = None;

        let project_dir = project_dir.to_path_buf();
        let cancel = cancel.clone();
        let file_path = path.to_path_buf();
        let content = self.content.as_ref().map(|c| c.text()).unwrap_or_default();
        let lang = self.lang;
        iced::Task::perform(
            async move { lsp_did_open(project_dir, file_path, content, lang, cancel).await },
            |result| match result {
                Ok(()) => Message::LspReady,
                Err(e) => Message::LspError(e),
            },
        )
    }

    /// Capture the current buffer and cursor as an undo snapshot.
    fn snapshot(&self) -> Option<HistoryEntry> {
        let content = self.content.as_ref()?;
        let cursor = content.cursor();
        Some(HistoryEntry {
            text: content.text(),
            cursor_line: cursor.position.line,
            cursor_index: cursor.position.column,
        })
    }

    /// Push a pre-edit snapshot onto the undo stack, bounded to MAX_HISTORY.
    fn push_undo(&mut self) {
        if let Some(entry) = self.snapshot() {
            self.undo_stack.push_back(entry);
            while self.undo_stack.len() > MAX_HISTORY {
                self.undo_stack.pop_front();
            }
        }
    }

    /// Restore a snapshot: rebuild the buffer and clamp the cursor into range.
    /// Snapshots never contain placeholders, so fold anchors would be stale.
    fn restore(&mut self, entry: HistoryEntry) {
        let mut content = text_editor::Content::with_text(&entry.text);
        let cursor = clamp_cursor(&content, entry.cursor_line, entry.cursor_index);
        content.move_to(cursor);
        self.content = Some(content);
        self.folds.clear();
    }

    /// Apply an indent/unindent edit as a single undo entry.
    fn apply_indent(
        &mut self,
        edit: text_editor::Edit,
        project_dir: &Utf8Path,
        cancel: &CancellationToken,
    ) -> iced::Task<Message> {
        let (lo, hi) = self.content.as_ref().map(selection_line_range).unwrap_or((0, 0));
        self.expand_intersecting(lo, hi);
        self.push_undo();
        self.redo_stack.clear();
        self.last_edit_kind = Some(EditKind::Indent);
        if let Some(content) = &mut self.content {
            content.perform(text_editor::Action::Edit(edit));
        }
        self.after_text_change(project_dir, cancel)
    }

    /// Mark the buffer dirty and send `textDocument/didChange` to the LSP
    /// server. Returns a no-op task when no file is open.
    fn after_text_change(
        &mut self,
        project_dir: &Utf8Path,
        cancel: &CancellationToken,
    ) -> iced::Task<Message> {
        self.dirty = true;
        self.lsp_version += 1;
        self.refresh_cursor_insights();
        let Some(file_path) = self.active_file.clone() else {
            return iced::Task::none();
        };
        let Some(content_text) = self.content.as_ref().map(|c| c.text()) else {
            return iced::Task::none();
        };
        let version = self.lsp_version;
        let project_dir = project_dir.to_path_buf();
        let cancel = cancel.clone();
        iced::Task::perform(
            async move { lsp_did_change(project_dir, file_path, content_text, version, cancel).await },
            |result| match result {
                Ok(()) => Message::LspReady,
                Err(e) => Message::LspError(e),
            },
        )
    }

    /// Open the find bar (optionally with the replace row), pre-filling the
    /// query from the current single-line selection like Kate does.
    fn open_find_bar(&mut self, with_replace: bool) -> iced::Task<Message> {
        // Searching needs the real buffer: expand folds first.
        self.expand_intersecting(0, usize::MAX);
        self.find_open = true;
        self.replace_open = with_replace;
        self.goto_open = false;
        if let Some(content) = &self.content {
            if let Some(selection) = content.selection() {
                if !selection.contains('\n') && selection.len() <= 128 {
                    self.find_query = selection;
                }
            }
        }
        self.refresh_matches();
        self.find_current = None;
        iced::widget::operation::focus(iced::widget::Id::from(FIND_INPUT_ID))
    }

    /// Recompute `find_matches` from the current buffer and query.
    fn refresh_matches(&mut self) {
        let Some(content) = &self.content else {
            self.find_matches.clear();
            self.find_overflow = false;
            return;
        };
        let (matches, overflow) =
            find_matches_in(&content.text(), &self.find_query, self.find_case_sensitive);
        self.find_matches = matches;
        self.find_overflow = overflow;
        if let Some(i) = self.find_current {
            if i >= self.find_matches.len() {
                self.find_current = None;
            }
        }
    }

    /// Move the cursor to a match and select it, mimicking Kate's find jump.
    fn jump_to_match(&mut self, index: usize) {
        let Some(m) = self.find_matches.get(index).copied() else {
            return;
        };
        if let Some(content) = &mut self.content {
            content.move_to(text_editor::Cursor {
                position: text_editor::Position { line: m.line, column: m.col },
                selection: None,
            });
            // Select the match so Replace and plain typing act on it.
            let query_chars = self.find_query.chars().count();
            for _ in 0..query_chars {
                content.perform(text_editor::Action::Select(text_editor::Motion::Right));
            }
        }
        self.find_current = Some(index);
        self.refresh_cursor_insights();
    }

    /// Replace the current match with `replace_query` and advance.
    fn replace_current(
        &mut self,
        project_dir: &Utf8Path,
        cancel: &CancellationToken,
    ) -> iced::Task<Message> {
        let Some(index) = self.find_current else {
            return iced::Task::none();
        };
        let Some(m) = self.find_matches.get(index).copied() else {
            return iced::Task::none();
        };
        let Some(text) = self.content.as_ref().map(|c| c.text()) else {
            return iced::Task::none();
        };
        let end = m.offset + self.find_query.len();
        let Some(range) = text.get(m.offset..end) else {
            return iced::Task::none();
        };
        // Guard against a stale match list: only splice when the bytes at the
        // recorded offset still equal the query under the active case mode.
        let still_matches = if self.find_case_sensitive {
            range == self.find_query
        } else {
            range.eq_ignore_ascii_case(&self.find_query)
        };
        if !still_matches {
            self.refresh_matches();
            return iced::Task::none();
        }
        self.push_undo();
        self.redo_stack.clear();
        self.last_edit_kind = None;
        let mut new_text =
            String::with_capacity(text.len() - self.find_query.len() + self.replace_query.len());
        new_text.push_str(&text[..m.offset]);
        new_text.push_str(&self.replace_query);
        new_text.push_str(&text[end..]);
        if let Some(content) = &mut self.content {
            *content = text_editor::Content::with_text(&new_text);
            let (line, col) = offset_to_line_col(&new_text, m.offset + self.replace_query.len());
            content.move_to(text_editor::Cursor {
                position: text_editor::Position { line, column: col },
                selection: None,
            });
        }
        self.refresh_matches();
        if self.find_matches.is_empty() {
            self.find_current = None;
        } else {
            let next = index.min(self.find_matches.len() - 1);
            self.jump_to_match(next);
        }
        self.after_text_change(project_dir, cancel)
    }

    /// Replace every match in a single undoable edit.
    fn replace_all(
        &mut self,
        project_dir: &Utf8Path,
        cancel: &CancellationToken,
    ) -> iced::Task<Message> {
        if self.find_query.is_empty() || self.find_matches.is_empty() {
            return iced::Task::none();
        }
        let Some(text) = self.content.as_ref().map(|c| c.text()) else {
            return iced::Task::none();
        };
        self.push_undo();
        self.redo_stack.clear();
        self.last_edit_kind = None;
        let new_text = replace_all_from_matches(
            &text,
            &self.find_matches,
            self.find_query.len(),
            &self.replace_query,
        );
        if let Some(content) = &mut self.content {
            *content = text_editor::Content::with_text(&new_text);
        }
        self.refresh_matches();
        self.find_current = None;
        self.after_text_change(project_dir, cancel)
    }

    /// Expand the folds intersecting display lines `[lo, hi]` (inclusive).
    /// Splices each region's hidden text back over its placeholder line,
    /// shifts the anchors of surviving folds below, and remaps the cursor.
    /// `(0, usize::MAX)` expands everything.
    fn expand_intersecting(&mut self, lo: usize, hi: usize) {
        if self.folds.is_empty() {
            return;
        }
        // Full-expansion fast path (Save, Undo/Redo, OpenFind, UnfoldAll).
        if lo == 0 && hi == usize::MAX {
            let folds = std::mem::take(&mut self.folds);
            if let Some(content) = &mut self.content {
                let text = content.text();
                let cursor = content.cursor();
                let new_line = map_line_on_expand(cursor.position.line, &folds);
                *content = text_editor::Content::with_text(&expand_all_in_text(&text, &folds));
                let clamped = clamp_cursor(content, new_line, cursor.position.column);
                content.move_to(clamped);
            }
            return;
        }
        let (hits, rest): (Vec<ActiveFold>, Vec<ActiveFold>) = std::mem::take(&mut self.folds)
            .into_iter()
            .partition(|f| f.start + 1 >= lo && f.start <= hi);
        if hits.is_empty() {
            self.folds = rest;
            return;
        }
        let Some(content) = &mut self.content else {
            // No buffer to expand into: restore the fold list and bail.
            self.folds = rest.into_iter().chain(hits).collect();
            self.folds.sort_by_key(|f| f.start);
            return;
        };
        let text = content.text();
        let trailing_nl = text.ends_with('\n');
        let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
        // Expand bottom-up so anchors above stay valid during splicing.
        let mut hits_desc = hits.clone();
        hits_desc.sort_by_key(|f| std::cmp::Reverse(f.start));
        for f in &hits_desc {
            let pos = f.start + 1;
            if pos >= lines.len() {
                continue;
            }
            let hidden: Vec<String> = f.hidden_text.split('\n').map(str::to_string).collect();
            lines.splice(pos..=pos, hidden);
        }
        // Surviving folds below an expanded one shift down by its growth.
        let mut new_folds = rest;
        for f in new_folds.iter_mut() {
            for h in &hits {
                if f.start > h.start {
                    f.start += h.hidden_count - 1;
                }
            }
        }
        new_folds.sort_by_key(|f| f.start);
        let mut out = lines.join("\n");
        if trailing_nl {
            out.push('\n');
        }
        let cursor = content.cursor();
        let new_line = map_line_on_expand(cursor.position.line, &hits);
        *content = text_editor::Content::with_text(&out);
        let clamped = clamp_cursor(content, new_line, cursor.position.column);
        content.move_to(clamped);
        self.folds = new_folds;
    }

    /// Recompute cursor-derived insights: bracket-pair state, word
    /// occurrences, and clear LSP hover (which is position-specific).
    /// Called after any cursor movement or buffer change. Costs one O(n) pass
    /// over the buffer (capped); acceptable for editor sizes.
    fn refresh_cursor_insights(&mut self) {
        // Clear position-sensitive LSP hover; cursor moved.
        self.hover = None;
        let Some(content) = &self.content else {
            self.bracket_status = BracketStatus::None;
            self.word_occurrences.clear();
            self.current_word = None;
            return;
        };
        let text = content.text();
        let offset = cursor_byte_offset(content);
        self.bracket_status = find_bracket_match(&text, offset);
        match find_word_span(&text, offset) {
            Some((start, end)) => {
                let word = &text[start..end];
                self.word_occurrences = word_occurrence_lines(&text, word);
                self.current_word = Some(word.to_string());
            }
            None => {
                self.word_occurrences.clear();
                self.current_word = None;
            }
        }
        // Fold-region cache: recompute only when the buffer actually changed
        // (cursor moves alone reuse the cached regions).
        let line_count = content.line_count();
        if self.region_cache.0 != self.lsp_version || self.region_cache.1 != line_count {
            self.region_cache = (self.lsp_version, line_count, compute_fold_regions(&text));
        }
    }
}

/// Custom key bindings for the focused editor. Anything not handled here
/// falls through to iced's defaults (copy/cut/paste/select, motions, insert).
pub(crate) fn editor_key_binding(
    key_press: text_editor::KeyPress,
    tab_mode: TabMode,
    completion_open: bool,
) -> Option<text_editor::Binding<Message>> {
    let mods = key_press.modifiers;

    // When the completion popup is open, Up/Down/Enter/Tab/Escape control it.
    if completion_open {
        match key_press.modified_key.as_ref() {
            Key::Named(Named::ArrowDown) => {
                return Some(text_editor::Binding::Custom(Message::CompletionNext));
            }
            Key::Named(Named::ArrowUp) => {
                return Some(text_editor::Binding::Custom(Message::CompletionPrev));
            }
            Key::Named(Named::Enter) if !mods.control() && !mods.alt() => {
                return Some(text_editor::Binding::Custom(Message::CompletionAccept));
            }
            Key::Named(Named::Tab) if !mods.control() && !mods.alt() && !mods.shift() => {
                return Some(text_editor::Binding::Custom(Message::CompletionAccept));
            }
            Key::Named(Named::Escape) => {
                return Some(text_editor::Binding::Custom(Message::CompletionClose));
            }
            _ => {}
        }
    }

    // Ctrl+Z undo; Ctrl+Shift+Z / Ctrl+Y redo (layout-independent).
    if mods.command() {
        if let Some(ch) = key_press.key.to_latin(key_press.physical_key) {
            match (ch.to_ascii_lowercase(), mods.shift()) {
                ('z', false) => return Some(text_editor::Binding::Custom(Message::Undo)),
                ('z', true) | ('y', false) => {
                    return Some(text_editor::Binding::Custom(Message::Redo));
                }
                _ => {}
            }
        }
        // Kate's fold/unfold-all: Ctrl+Shift+- / Ctrl+Shift+=.
        if mods.shift() {
            if let Key::Character(c) = key_press.key.as_ref() {
                match c {
                    "-" => return Some(text_editor::Binding::Custom(Message::FoldAll)),
                    "=" => return Some(text_editor::Binding::Custom(Message::UnfoldAll)),
                    _ => {}
                }
            }
        }
        // Ctrl+Space → LSP completions.
        if matches!(key_press.key.as_ref(), Key::Named(Named::Space)) {
            return Some(text_editor::Binding::Custom(Message::CompletionRequest));
        }
        // Ctrl+I → LSP hover.
        if let Key::Character(c) = key_press.key.as_ref() {
            if c == "i" && !mods.shift() {
                return Some(text_editor::Binding::Custom(Message::HoverRequest));
            }
        }
    }

    // F12 → go-to-definition (unmodified).
    if !mods.control()
        && !mods.alt()
        && !mods.shift()
        && matches!(key_press.key.as_ref(), Key::Named(Named::F12))
    {
        return Some(text_editor::Binding::Custom(Message::DefinitionRequest));
    }

    // Plain Enter continues comments/indentation. Ctrl+Enter stays reserved
    // for the global submit shortcut.
    if !mods.control()
        && !mods.alt()
        && matches!(key_press.modified_key.as_ref(), Key::Named(Named::Enter))
    {
        return Some(text_editor::Binding::Custom(Message::SmartEnter));
    }

    // Tab/Shift+Tab: iced's default binding ignores Tab entirely, so both
    // modes are handled explicitly here.
    if matches!(key_press.modified_key.as_ref(), Key::Named(Named::Tab)) {
        if mods.shift() {
            return Some(text_editor::Binding::Custom(Message::UnindentSelection));
        }
        if !mods.control() && !mods.alt() {
            return Some(match tab_mode {
                TabMode::Tabs => text_editor::Binding::Custom(Message::IndentSelection),
                TabMode::Spaces(_) => text_editor::Binding::Custom(Message::InsertSpaces),
            });
        }
    }

    text_editor::Binding::from_key_press(key_press)
}

// ---------------------------------------------------------------------------
// LSP async helpers
// ---------------------------------------------------------------------------

/// Send `textDocument/didOpen` to the language server for the given file.
pub(crate) async fn lsp_did_open(
    project_dir: Utf8PathBuf,
    file_path: Utf8PathBuf,
    content: String,
    lang: &'static str,
    cancel: CancellationToken,
) -> Result<(), String> {
    let project_id = ProjectIdHelper::from_dir(&project_dir)
        .map_err(|e| format!("Failed to compute project ID: {e}"))?;
    let client =
        LspManager::get_or_start(project_id, project_dir.as_std_path().to_path_buf(), cancel).await;
    let mut client = client.lock().await;
    let uri = format!("file://{}", file_path);
    let params = serde_json::json!({
        "textDocument": {
            "uri": uri,
            "languageId": lang,
            "version": 1,
            "text": content,
        }
    });
    client
        .send_notification("textDocument/didOpen", params)
        .await
        .map_err(|e| format!("LSP didOpen failed: {e}"))?;
    Ok(())
}

/// Send `textDocument/didChange` to the language server.
pub(crate) async fn lsp_did_change(
    project_dir: Utf8PathBuf,
    file_path: Utf8PathBuf,
    content: String,
    version: i32,
    cancel: CancellationToken,
) -> Result<(), String> {
    let project_id = ProjectIdHelper::from_dir(&project_dir)
        .map_err(|e| format!("Failed to compute project ID: {e}"))?;
    let client =
        LspManager::get_or_start(project_id, project_dir.as_std_path().to_path_buf(), cancel).await;
    let mut client = client.lock().await;
    let uri = format!("file://{}", file_path);
    let params = serde_json::json!({
        "textDocument": {
            "uri": uri,
            "version": version,
        },
        "contentChanges": [
            { "text": content }
        ]
    });
    client
        .send_notification("textDocument/didChange", params)
        .await
        .map_err(|e| format!("LSP didChange failed: {e}"))?;
    Ok(())
}

/// Send `textDocument/didSave` to the language server. Some servers gate a
/// full/authoritative diagnostics pass on didSave, so a plain save must not
/// skip it.
pub(crate) async fn lsp_did_save(
    project_dir: Utf8PathBuf,
    file_path: Utf8PathBuf,
    content: String,
    cancel: CancellationToken,
) -> Result<(), String> {
    let project_id = ProjectIdHelper::from_dir(&project_dir)
        .map_err(|e| format!("Failed to compute project ID: {e}"))?;
    let client =
        LspManager::get_or_start(project_id, project_dir.as_std_path().to_path_buf(), cancel).await;
    let mut client = client.lock().await;
    let uri = format!("file://{}", file_path);
    let params = serde_json::json!({
        "textDocument": { "uri": uri },
        "text": content,
    });
    client
        .send_notification("textDocument/didSave", params)
        .await
        .map_err(|e| format!("LSP didSave failed: {e}"))?;
    Ok(())
}

/// Retrieve cached diagnostics for a file from the LSP client.
pub(crate) async fn lsp_get_diagnostics(
    project_dir: Utf8PathBuf,
    file_path: Utf8PathBuf,
    cancel: CancellationToken,
) -> Result<Vec<Diagnostic>, String> {
    let project_id = ProjectIdHelper::from_dir(&project_dir)
        .map_err(|e| format!("Failed to compute project ID: {e}"))?;
    let client =
        LspManager::get_or_start(project_id, project_dir.as_std_path().to_path_buf(), cancel).await;
    let client = client.lock().await;
    let raw_diags = client.get_diagnostics(file_path.as_str()).await;

    let mut diagnostics = Vec::new();
    for raw in raw_diags {
        let line = raw
            .get("range")
            .and_then(|r| r.get("start"))
            .and_then(|s| s.get("line"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize;
        let character = raw
            .get("range")
            .and_then(|r| r.get("start"))
            .and_then(|s| s.get("character"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize;
        let message = raw.get("message").and_then(|v| v.as_str()).unwrap_or("?").to_string();
        let severity = raw
            .get("severity")
            .and_then(|v| v.as_u64())
            .map(|s| match s {
                1 => DiagnosticSeverity::Error,
                2 => DiagnosticSeverity::Warning,
                3 => DiagnosticSeverity::Information,
                4 => DiagnosticSeverity::Hint,
                _ => DiagnosticSeverity::Error,
            })
            .unwrap_or(DiagnosticSeverity::Error);

        diagnostics.push(Diagnostic { line, character, message, severity });
    }

    Ok(diagnostics)
}

// ---------------------------------------------------------------------------
// LSP request helpers (completion, definition, hover)
// ---------------------------------------------------------------------------

/// Build `textDocument/completion` params and invoke it.
pub(crate) async fn lsp_request_completion(
    project_dir: Utf8PathBuf,
    file_path: Utf8PathBuf,
    line: usize,
    character: usize,
    cancel: CancellationToken,
) -> Vec<CompletionItem> {
    let Ok(project_id) = ProjectIdHelper::from_dir(&project_dir) else {
        return Vec::new();
    };
    let Ok(client) =
        LspManager::get_or_start(project_id, project_dir.as_std_path().to_path_buf(), cancel)
            .await
            .lock()
            .await
            .send_request(
                "textDocument/completion",
                serde_json::json!({
                    "textDocument": { "uri": format!("file://{}", file_path) },
                    "position": { "line": line, "character": character },
                }),
            )
            .await
    else {
        return Vec::new();
    };
    parse_completion_items(&client)
}

/// Build `textDocument/definition` params and invoke it.
pub(crate) async fn lsp_request_definition(
    project_dir: Utf8PathBuf,
    file_path: Utf8PathBuf,
    line: usize,
    character: usize,
    cancel: CancellationToken,
) -> Result<Option<(Utf8PathBuf, usize, usize)>, String> {
    let project_id =
        ProjectIdHelper::from_dir(&project_dir).map_err(|e| format!("Project ID: {e}"))?;
    let client =
        LspManager::get_or_start(project_id, project_dir.as_std_path().to_path_buf(), cancel).await;
    let mut client = client.lock().await;
    let result = client
        .send_request(
            "textDocument/definition",
            serde_json::json!({
                "textDocument": { "uri": format!("file://{}", file_path) },
                "position": { "line": line, "character": character },
            }),
        )
        .await
        .map_err(|e| format!("LSP definition failed: {e}"))?;
    Ok(parse_definition(&result))
}

/// Build `textDocument/hover` params and invoke it.
pub(crate) async fn lsp_request_hover(
    project_dir: Utf8PathBuf,
    file_path: Utf8PathBuf,
    line: usize,
    character: usize,
    cancel: CancellationToken,
) -> Result<String, String> {
    let project_id =
        ProjectIdHelper::from_dir(&project_dir).map_err(|e| format!("Project ID: {e}"))?;
    let client =
        LspManager::get_or_start(project_id, project_dir.as_std_path().to_path_buf(), cancel).await;
    let mut client = client.lock().await;
    let result = client
        .send_request(
            "textDocument/hover",
            serde_json::json!({
                "textDocument": { "uri": format!("file://{}", file_path) },
                "position": { "line": line, "character": character },
            }),
        )
        .await
        .map_err(|e| format!("LSP hover failed: {e}"))?;
    Ok(parse_hover_contents(&result).unwrap_or_default())
}

// ---------------------------------------------------------------------------
// LSP response parsers
// ---------------------------------------------------------------------------

/// Extract completion items from a `textDocument/completion` response.
/// Accepts both `CompletionItem[]` and `CompletionList` (`{items, isIncomplete}`).
fn parse_completion_items(value: &serde_json::Value) -> Vec<CompletionItem> {
    let arr = value
        .as_array()
        .cloned()
        .or_else(|| value.get("items").and_then(|i| i.as_array()).cloned())
        .unwrap_or_default();
    arr.iter()
        .filter_map(|item| {
            let label = item.get("label")?.as_str()?.to_string();
            let detail = item.get("detail").and_then(|d| d.as_str()).map(|s| s.to_string());
            let insert_text =
                item.get("insertText").and_then(|t| t.as_str()).unwrap_or(&label).to_string();
            Some(CompletionItem { label, detail, insert_text })
        })
        .take(100)
        .collect()
}

/// Parse a `textDocument/definition` response: `Location | Location[] |
/// LocationLink[] | null`.
fn parse_definition(value: &serde_json::Value) -> Option<(Utf8PathBuf, usize, usize)> {
    let first = if value.is_array() {
        value.as_array()?.first()?.clone()
    } else if value.is_null() {
        return None;
    } else {
        value.clone()
    };
    // LocationLink uses `targetUri`/`targetRange`; Location uses `uri`/`range`.
    let (uri, range) = if let Some(u) = first.get("targetUri") {
        (u.as_str()?, first.get("targetRange")?)
    } else {
        (first.get("uri")?.as_str()?, first.get("range")?)
    };
    let path_str = uri.strip_prefix("file://")?;
    let path = Utf8PathBuf::from(path_str);
    let line = range.get("start")?.get("line")?.as_u64()? as usize;
    let character = range.get("start")?.get("character")?.as_u64()? as usize;
    Some((path, line, character))
}

/// Parse a `textDocument/hover` response. Handles `MarkupContent`,
/// `MarkedString`, and `MarkedString[]`.
fn parse_hover_contents(value: &serde_json::Value) -> Option<String> {
    let contents = value.get("contents")?;
    // MarkupContent: { kind, value }
    if let Some(s) = contents.get("value").and_then(|v| v.as_str()) {
        if !s.is_empty() {
            return Some(s.to_string());
        }
    }
    // MarkedString[]: [{language, value}, ...] or ["str", ...]
    if let Some(arr) = contents.as_array() {
        let parts: Vec<String> = arr
            .iter()
            .filter_map(|item| {
                if let Some(s) = item.as_str() {
                    if s.is_empty() {
                        None
                    } else {
                        Some(s.to_string())
                    }
                } else {
                    item.get("value").and_then(|v| v.as_str()).map(|s| s.to_string())
                }
            })
            .collect();
        if !parts.is_empty() {
            return Some(parts.join("\n"));
        }
    }
    // Plain string.
    contents.as_str().map(|s| s.to_string())
}
