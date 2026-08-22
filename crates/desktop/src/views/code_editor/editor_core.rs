use std::sync::{Arc, Mutex};

use camino::{Utf8Component, Utf8Path, Utf8PathBuf};
use iced::widget::operation;
use iced::widget::text_editor;

use concerto_core::CancellationToken;
use concerto_tools::virtual_fs::VirtualFs;

use super::helpers::{lsp_position, utf16_col_to_byte};
use super::{
    clamp_cursor, classify_edit, compute_fold_regions, continuation_prefix, cursor_byte_offset,
    find_word_span, fold_regions_in_text, lsp_did_save, lsp_get_diagnostics,
    lsp_request_completion, lsp_request_definition, lsp_request_hover, map_line_on_fold,
    offset_to_line_col, outermost_regions, selection_line_range, should_snapshot,
    trim_trailing_whitespace, EditKind, Message, State, DIAG_PANE_MAX_SHARE, DIAG_PANE_MIN_SHARE,
    GOTO_INPUT_ID, TREE_PANE_MAX_RATIO, TREE_PANE_MIN_RATIO,
};
use crate::widgets::confirm_modal::ConfirmModal;
use crate::widgets::file_tree;

impl State {
    /// Update the editor state.
    ///
    /// `vfs` and `project_dir` are needed for file operations.
    /// `cancel` is needed for LSP operations.
    pub fn update(
        &mut self,
        message: Message,
        vfs: &Arc<Mutex<VirtualFs>>,
        project_dir: &Utf8Path,
        cancel: &CancellationToken,
    ) -> iced::Task<Message> {
        match message {
            Message::FileSelected(path) => self.dispatch_file_open(&path, vfs, project_dir, cancel),
            Message::DirToggled(path) => {
                self.tree.toggle_path(&path);
                iced::Task::none()
            }
            Message::PaneResized(event) => {
                // pane_grid exposes no true min-width API, so the editor
                // floor is enforced on the divider ratio (the widget's
                // min_size pixels floor is the secondary guard). Each divider
                // has its own clamp regime:
                //   - tree|editor: the tree's share is clamped directly.
                //   - editor|diag: the split ratio is the editor's share of
                //     the editor|diag width, so the diagnostics pane's share
                //     (1 - ratio) is what gets clamped (#108).
                let ratio = if self.tree_split == Some(event.split) {
                    event.ratio.clamp(TREE_PANE_MIN_RATIO, TREE_PANE_MAX_RATIO)
                } else if self.diag_split == Some(event.split) {
                    let diag_share =
                        (1.0 - event.ratio).clamp(DIAG_PANE_MIN_SHARE, DIAG_PANE_MAX_SHARE);
                    1.0 - diag_share
                } else {
                    // Unknown/legacy split: fall back to the tree clamp.
                    event.ratio.clamp(TREE_PANE_MIN_RATIO, TREE_PANE_MAX_RATIO)
                };
                self.pane_state.resize(event.split, ratio);
                iced::Task::none()
            }
            Message::Edit(action) => {
                // Cursor movements and clicks are not edits: they neither dirty
                // the buffer nor belong in the undo history or LSP didChange.
                if let text_editor::Action::Edit(ref edit) = action {
                    // Any text edit closes the completion popup.
                    self.completion_open = false;
                    // Expand any fold intersecting the edit range BEFORE the
                    // snapshot, so undo history never contains placeholders.
                    let (lo, hi) =
                        self.content.as_ref().map(selection_line_range).unwrap_or((0, 0));
                    self.expand_intersecting(lo, hi);
                    let edit_line =
                        self.content.as_ref().map(|c| c.cursor().position.line).unwrap_or(0);
                    let old_lines = self.content.as_ref().map(|c| c.line_count()).unwrap_or(0);
                    let kind = classify_edit(edit);
                    if should_snapshot(self.last_edit_kind, kind) {
                        self.push_undo();
                        self.redo_stack.clear();
                    }
                    self.last_edit_kind = Some(kind);
                    if let Some(content) = &mut self.content {
                        content.perform(action);
                    }
                    // Surviving folds below the edit shift by the line delta.
                    if !self.folds.is_empty() {
                        let new_lines = self.content.as_ref().map(|c| c.line_count()).unwrap_or(0);
                        let delta = new_lines as isize - old_lines as isize;
                        if delta != 0 {
                            for f in self.folds.iter_mut() {
                                if f.start > edit_line {
                                    f.start = f.start.saturating_add_signed(delta);
                                }
                            }
                        }
                    }
                    self.after_text_change(project_dir, cancel)
                } else {
                    // A cursor move breaks a typing burst so the next edit
                    // starts a fresh undo entry.
                    self.last_edit_kind = None;
                    // Any non-edit action (click, drag, move, select) moves the
                    // cursor: close the completion popup so it can't linger,
                    // positionally stale, after the user clicked away.
                    self.completion_open = false;
                    if let Some(content) = &mut self.content {
                        content.perform(action);
                    }
                    // Pure cursor movement: still refresh bracket/word state.
                    self.refresh_cursor_insights();
                    iced::Task::none()
                }
            }
            Message::Save => {
                // Expand folds first so placeholders never reach disk.
                self.expand_intersecting(0, usize::MAX);
                let Some(path) = self.active_file.clone() else {
                    return iced::Task::none();
                };
                let Some(content) = &mut self.content else {
                    return iced::Task::none();
                };
                let mut text = content.text();
                if self.trim_trailing_on_save {
                    let trimmed = trim_trailing_whitespace(&text);
                    if trimmed != text {
                        // Rebuild content with the trimmed text, keeping
                        // the cursor at a clamped equivalent position.
                        let cursor = content.cursor();
                        *content = text_editor::Content::with_text(&trimmed);
                        let clamped =
                            clamp_cursor(content, cursor.position.line, cursor.position.column);
                        content.move_to(clamped);
                        text = trimmed;
                    }
                }
                if let Err(e) = std::fs::write(path.as_std_path(), &text) {
                    return iced::Task::done(Message::LspError(format!("Failed to save: {e}")));
                }
                self.dirty = false;
                // Disk is now authoritative: drop any staged VFS entry so the
                // next open_file() reads the just-saved content instead of
                // shadowing it with stale staged text.
                if let Ok(mut guard) = vfs.lock() {
                    guard.unstage(&path);
                }
                // Some language servers gate a full diagnostics pass on
                // didSave; didChange alone can leave diagnostics stale.
                let project_dir = project_dir.to_path_buf();
                let cancel = cancel.clone();
                let file_path = path.clone();
                iced::Task::perform(
                    async move { lsp_did_save(project_dir, file_path, text, cancel).await },
                    |result| match result {
                        Ok(()) => Message::LspReady,
                        Err(e) => Message::LspError(e),
                    },
                )
            }
            Message::NewFile => {
                // In a full implementation, this would open a dialog.
                // For now, create a file named "new_file.rs" in the project root.
                let new_path = project_dir.join("new_file.rs");
                if !new_path.as_std_path().exists() {
                    if let Err(e) = std::fs::write(new_path.as_std_path(), "// new file\n") {
                        return iced::Task::done(Message::LspError(format!(
                            "Failed to create file: {e}"
                        )));
                    }
                }
                self.tree_dirty = true;
                iced::Task::done(Message::FileSelected(new_path))
            }
            Message::NewFileName(name) => {
                // The name must resolve to a path inside the project root:
                // reject empty names, absolute paths, and `..` traversal
                // before anything reaches the filesystem.
                let Some(new_path) = sanitize_new_file_name(project_dir, &name) else {
                    return iced::Task::done(Message::LspError(format!(
                        "Invalid file name: {name:?}"
                    )));
                };
                if let Err(e) = std::fs::write(new_path.as_std_path(), "") {
                    return iced::Task::done(Message::LspError(format!(
                        "Failed to create file: {e}"
                    )));
                }
                self.tree_dirty = true;
                iced::Task::done(Message::FileSelected(new_path))
            }
            Message::DeleteFile => {
                // Destructive action: arm the confirmation gate; the actual
                // delete happens only on Message::DeleteConfirmed.
                if let Some(path) = &self.active_file {
                    let file_name = path.file_name().unwrap_or("current file").to_string();
                    self.pending_delete = Some(ConfirmModal::delete(
                        "Delete File",
                        format!("Delete {file_name}? This cannot be undone."),
                    ));
                }
                iced::Task::none()
            }
            Message::DeleteConfirmed => {
                self.pending_delete = None;
                let Some(path) = self.active_file.clone() else {
                    return iced::Task::none();
                };
                if path.as_std_path().exists() {
                    if let Err(e) = std::fs::remove_file(path.as_std_path()) {
                        return iced::Task::done(Message::LspError(format!(
                            "Failed to delete: {e}"
                        )));
                    }
                }
                // Drop any staged VFS entry so open_file() cannot resurrect
                // the deleted content from the staging area.
                if let Ok(mut guard) = vfs.lock() {
                    guard.unstage(&path);
                }
                self.active_file = None;
                self.content = None;
                self.tree_dirty = true;
                iced::Task::none()
            }
            Message::DeleteCancelled => {
                self.pending_delete = None;
                iced::Task::none()
            }
            Message::RefreshTree => {
                self.tree = crate::widgets::file_tree::TreeNode::from_disk(project_dir);
                self.tree_dirty = false;
                iced::Task::none()
            }
            Message::LspHover(text) => {
                self.hover = Some(text);
                iced::Task::none()
            }
            Message::LspDiagnostics(diags) => {
                self.diagnostics = diags;
                iced::Task::none()
            }
            Message::ToggleDiagnostics => {
                self.show_diagnostics = !self.show_diagnostics;
                iced::Task::none()
            }
            Message::ClearHover => {
                self.hover = None;
                iced::Task::none()
            }
            Message::LspReady => {
                // LSP operation completed successfully.
                // Request diagnostics for the current file.
                if let Some(path) = &self.active_file {
                    let project_dir = project_dir.to_path_buf();
                    let cancel = cancel.clone();
                    let file_path = path.clone();
                    return iced::Task::perform(
                        async move { lsp_get_diagnostics(project_dir, file_path, cancel).await },
                        |result| match result {
                            Ok(diags) => Message::LspDiagnostics(diags),
                            Err(e) => Message::LspError(e),
                        },
                    );
                }
                iced::Task::none()
            }
            Message::LspError(_) => {
                // Error already logged; clear hover.
                self.hover = None;
                iced::Task::none()
            }
            Message::Undo => {
                // Snapshots never contain placeholders, but folds may be
                // active: expand first so positions line up.
                self.expand_intersecting(0, usize::MAX);
                let Some(entry) = self.undo_stack.pop_back() else {
                    return iced::Task::none();
                };
                if let Some(current) = self.snapshot() {
                    self.redo_stack.push(current);
                }
                self.restore(entry);
                self.last_edit_kind = None;
                self.after_text_change(project_dir, cancel)
            }
            Message::Redo => {
                self.expand_intersecting(0, usize::MAX);
                let Some(entry) = self.redo_stack.pop() else {
                    return iced::Task::none();
                };
                if let Some(current) = self.snapshot() {
                    self.undo_stack.push_back(current);
                }
                self.restore(entry);
                self.last_edit_kind = None;
                self.after_text_change(project_dir, cancel)
            }
            Message::SmartEnter => {
                // Compute the continuation prefix from the text before the
                // cursor, then perform Enter + the prefix inserts as one undo
                // entry of kind `Newline`.
                let (lo, hi) = self.content.as_ref().map(selection_line_range).unwrap_or((0, 0));
                self.expand_intersecting(lo, hi);
                let prefix = {
                    let Some(content) = &self.content else {
                        return iced::Task::none();
                    };
                    let cursor = content.cursor();
                    content
                        .line(cursor.position.line)
                        .and_then(|l| l.text.get(..cursor.position.column).map(|s| s.to_string()))
                        .and_then(|before| continuation_prefix(&before))
                };
                self.push_undo();
                self.redo_stack.clear();
                self.last_edit_kind = Some(EditKind::Newline);
                if let Some(content) = &mut self.content {
                    content.perform(text_editor::Action::Edit(text_editor::Edit::Enter));
                    if let Some(prefix) = prefix {
                        for ch in prefix.chars() {
                            content
                                .perform(text_editor::Action::Edit(text_editor::Edit::Insert(ch)));
                        }
                    }
                }
                self.after_text_change(project_dir, cancel)
            }
            Message::InsertSpaces => {
                let count = self.tab_mode.width();
                let (lo, hi) = self.content.as_ref().map(selection_line_range).unwrap_or((0, 0));
                self.expand_intersecting(lo, hi);
                self.push_undo();
                self.redo_stack.clear();
                self.last_edit_kind = Some(EditKind::InsertChar);
                if let Some(content) = &mut self.content {
                    for _ in 0..count {
                        content.perform(text_editor::Action::Edit(text_editor::Edit::Insert(' ')));
                    }
                }
                self.after_text_change(project_dir, cancel)
            }
            Message::IndentSelection => {
                self.apply_indent(text_editor::Edit::Indent, project_dir, cancel)
            }
            Message::UnindentSelection => {
                self.apply_indent(text_editor::Edit::Unindent, project_dir, cancel)
            }
            Message::CycleTabMode => {
                self.tab_mode = self.tab_mode.cycle();
                iced::Task::none()
            }
            Message::ToggleTrimTrailing => {
                self.trim_trailing_on_save = !self.trim_trailing_on_save;
                iced::Task::none()
            }
            Message::OpenFind => self.open_find_bar(false),
            Message::OpenReplace => self.open_find_bar(true),
            Message::CloseFind => {
                self.find_open = false;
                self.replace_open = false;
                self.find_matches.clear();
                self.find_current = None;
                iced::Task::none()
            }
            Message::FindQueryChanged(query) => {
                self.find_query = query;
                self.refresh_matches();
                // Keep the current index valid; select the first match so the
                // user sees feedback as they type (incremental search).
                if !self.find_matches.is_empty() {
                    self.jump_to_match(0);
                } else {
                    self.find_current = None;
                }
                iced::Task::none()
            }
            Message::FindNext => {
                if self.find_matches.is_empty() {
                    self.refresh_matches();
                }
                if !self.find_matches.is_empty() {
                    let next = match self.find_current {
                        Some(i) => (i + 1) % self.find_matches.len(),
                        None => 0,
                    };
                    self.jump_to_match(next);
                }
                iced::Task::none()
            }
            Message::FindPrev => {
                if !self.find_matches.is_empty() {
                    let prev = match self.find_current {
                        Some(0) | None => self.find_matches.len() - 1,
                        Some(i) => i - 1,
                    };
                    self.jump_to_match(prev);
                }
                iced::Task::none()
            }
            Message::ToggleFindCase => {
                self.find_case_sensitive = !self.find_case_sensitive;
                self.refresh_matches();
                self.find_current = None;
                iced::Task::none()
            }
            Message::ReplaceQueryChanged(query) => {
                self.replace_query = query;
                iced::Task::none()
            }
            Message::ReplaceCurrent => self.replace_current(project_dir, cancel),
            Message::ReplaceAll => self.replace_all(project_dir, cancel),
            Message::OpenGoto => {
                self.completion_open = false;
                self.goto_open = true;
                self.find_open = false;
                self.replace_open = false;
                operation::focus(iced::widget::Id::from(GOTO_INPUT_ID))
            }
            Message::GotoInputChanged(input) => {
                self.goto_input = input;
                iced::Task::none()
            }
            Message::GotoSubmit => {
                if let Ok(line_1based) = self.goto_input.trim().parse::<usize>() {
                    if let Some(content) = &mut self.content {
                        let line = line_1based
                            .saturating_sub(1)
                            .min(content.line_count().saturating_sub(1));
                        content.move_to(text_editor::Cursor {
                            position: text_editor::Position { line, column: 0 },
                            selection: None,
                        });
                    }
                }
                self.goto_open = false;
                self.goto_input.clear();
                self.refresh_cursor_insights();
                iced::Task::none()
            }
            Message::CloseGoto => {
                self.goto_open = false;
                self.goto_input.clear();
                iced::Task::none()
            }
            Message::FoldAll => {
                // Normalize first so regions are computed on the real buffer.
                self.expand_intersecting(0, usize::MAX);
                let Some(content) = &self.content else {
                    return iced::Task::none();
                };
                let text = content.text();
                let regions = outermost_regions(&compute_fold_regions(&text));
                if regions.is_empty() {
                    return iced::Task::none();
                }
                let (new_text, folds) = fold_regions_in_text(&text, &regions);
                let cursor = content.cursor();
                let mapped_line = map_line_on_fold(cursor.position.line, &regions, &folds);
                if let Some(content) = &mut self.content {
                    *content = text_editor::Content::with_text(&new_text);
                    let clamped = clamp_cursor(content, mapped_line, cursor.position.column);
                    content.move_to(clamped);
                }
                self.folds = folds;
                self.refresh_cursor_insights();
                iced::Task::none()
            }
            Message::UnfoldAll => {
                self.expand_intersecting(0, usize::MAX);
                self.refresh_cursor_insights();
                iced::Task::none()
            }
            Message::ToggleFold(line) => {
                // Clicking an active anchor unfolds that region.
                if self.folds.iter().any(|f| f.start == line) {
                    self.expand_intersecting(line, line);
                    self.refresh_cursor_insights();
                    return iced::Task::none();
                }
                // No nested folds: new folds only from a flat buffer.
                if !self.folds.is_empty() {
                    return iced::Task::none();
                }
                let Some(content) = &self.content else {
                    return iced::Task::none();
                };
                let text = content.text();
                let regions = compute_fold_regions(&text);
                let Some(&(start, end)) = regions.iter().find(|&&(s, _)| s == line) else {
                    return iced::Task::none();
                };
                let (new_text, folds) = fold_regions_in_text(&text, &[(start, end)]);
                let cursor = content.cursor();
                let mapped_line = map_line_on_fold(cursor.position.line, &[(start, end)], &folds);
                if let Some(content) = &mut self.content {
                    *content = text_editor::Content::with_text(&new_text);
                    let clamped = clamp_cursor(content, mapped_line, cursor.position.column);
                    content.move_to(clamped);
                }
                self.folds = folds;
                self.refresh_cursor_insights();
                iced::Task::none()
            }
            Message::CompletionRequest => {
                // Expand folds so LSP positions refer to the real buffer.
                self.expand_intersecting(0, usize::MAX);
                let Some(content) = &self.content else { return iced::Task::none() };
                let Some(file_path) = self.active_file.clone() else {
                    return iced::Task::none();
                };
                let (line, character) = lsp_position(content);
                let project_dir = project_dir.to_path_buf();
                let cancel = cancel.clone();
                iced::Task::perform(
                    async move {
                        lsp_request_completion(project_dir, file_path, line, character, cancel)
                            .await
                    },
                    Message::CompletionReceived,
                )
            }
            Message::CompletionReceived(items) => {
                if items.is_empty() {
                    self.completion_open = false;
                } else {
                    self.completion_open = true;
                    self.completion_items = items;
                    self.completion_selected = 0;
                }
                iced::Task::none()
            }
            Message::CompletionNext => {
                if !self.completion_items.is_empty() {
                    self.completion_selected =
                        (self.completion_selected + 1) % self.completion_items.len();
                }
                iced::Task::none()
            }
            Message::CompletionPrev => {
                if !self.completion_items.is_empty() {
                    self.completion_selected = self
                        .completion_selected
                        .checked_sub(1)
                        .unwrap_or(self.completion_items.len() - 1);
                }
                iced::Task::none()
            }
            Message::CompletionAccept => {
                self.completion_open = false;
                let Some(item) = self.completion_items.get(self.completion_selected).cloned()
                else {
                    return iced::Task::none();
                };
                let Some(content) = &self.content else { return iced::Task::none() };
                let text = content.text();
                let offset = cursor_byte_offset(content);
                let prefix_start = find_word_span(&text, offset).map(|(s, _)| s).unwrap_or(offset);
                let new_text =
                    format!("{}{}{}", &text[..prefix_start], item.insert_text, &text[offset..]);
                let insert_end = prefix_start + item.insert_text.len();
                let (line, col) = offset_to_line_col(&new_text, insert_end);
                self.push_undo();
                self.redo_stack.clear();
                self.last_edit_kind = Some(EditKind::Paste);
                if let Some(content) = &mut self.content {
                    *content = text_editor::Content::with_text(&new_text);
                    let clamped = clamp_cursor(content, line, col);
                    content.move_to(clamped);
                }
                self.after_text_change(project_dir, cancel)
            }
            Message::CompletionClose => {
                self.completion_open = false;
                iced::Task::none()
            }
            Message::CompletionPick(i) => {
                // Delegate to accept with a manually chosen index.
                self.completion_selected = i.min(self.completion_items.len().saturating_sub(1));
                let msg = Message::CompletionAccept;
                // Re-dispatch to trigger the full accept path.
                self.update(msg, vfs, project_dir, cancel)
            }
            Message::DefinitionRequest => {
                self.expand_intersecting(0, usize::MAX);
                let Some(content) = &self.content else { return iced::Task::none() };
                let Some(file_path) = self.active_file.clone() else {
                    return iced::Task::none();
                };
                let (line, character) = lsp_position(content);
                let project_dir = project_dir.to_path_buf();
                let cancel = cancel.clone();
                iced::Task::perform(
                    async move {
                        lsp_request_definition(project_dir, file_path, line, character, cancel)
                            .await
                    },
                    |result| match result {
                        Ok(def) => Message::DefinitionReceived(def),
                        Err(e) => Message::LspError(e),
                    },
                )
            }
            Message::DefinitionReceived(def) => {
                let Some((path, line, utf16_col)) = def else {
                    self.hover = Some("No definition found".into());
                    return iced::Task::none();
                };
                let need_open = self.active_file.as_ref() != Some(&path);
                // If we opened a new file, send didOpen to the LSP server.
                let open_task = if need_open {
                    self.dispatch_file_open(&path, vfs, project_dir, cancel)
                } else {
                    iced::Task::none()
                };
                // Jump cursor to definition — content is already loaded.
                if let Some(content) = &mut self.content {
                    let line = line.min(content.line_count().saturating_sub(1));
                    let byte_col = content
                        .line(line)
                        .map(|l| utf16_col_to_byte(&l.text, utf16_col))
                        .unwrap_or(0);
                    let clamped = clamp_cursor(content, line, byte_col);
                    content.move_to(clamped);
                }
                self.refresh_cursor_insights();
                open_task
            }
            Message::HoverRequest => {
                self.expand_intersecting(0, usize::MAX);
                let Some(content) = &self.content else { return iced::Task::none() };
                let Some(file_path) = self.active_file.clone() else {
                    return iced::Task::none();
                };
                let (line, character) = lsp_position(content);
                let project_dir = project_dir.to_path_buf();
                let cancel = cancel.clone();
                iced::Task::perform(
                    async move {
                        lsp_request_hover(project_dir, file_path, line, character, cancel).await
                    },
                    |result| match result {
                        Ok(text) => Message::LspHover(text),
                        Err(e) => Message::LspError(e),
                    },
                )
            }
            Message::FileTree(tree_msg) => match tree_msg {
                file_tree::TreeMessage::FileSelected(path) => {
                    self.dispatch_file_open(&path, vfs, project_dir, cancel)
                }
                file_tree::TreeMessage::DirToggled(path) => {
                    self.tree.toggle_path(&path);
                    iced::Task::none()
                }
            },
        }
    }
}

/// Validate a new-file name and resolve it inside `project_dir`.
///
/// Rejects empty names, absolute paths, and any name containing `..` or
/// root components, then — as a symlink-aware backstop — verifies that the
/// deepest existing ancestor of the target resolves inside the canonical
/// project directory. Returns `None` for invalid names.
fn sanitize_new_file_name(project_dir: &Utf8Path, name: &str) -> Option<Utf8PathBuf> {
    if name.is_empty() {
        return None;
    }
    let candidate = Utf8Path::new(name);
    if candidate.is_absolute()
        || candidate
            .components()
            .any(|c| matches!(c, Utf8Component::ParentDir | Utf8Component::RootDir))
    {
        return None;
    }
    let joined = project_dir.join(candidate);
    // Belt and braces: a symlinked project dir (or symlinked subdir) must not
    // let the resolved path escape via canonicalization. Walk up from the
    // target until an existing ancestor is found and require it to be inside
    // the canonical project root.
    let canon_project = std::fs::canonicalize(project_dir).ok()?;
    let mut probe = joined.as_std_path();
    let mut canon_probe = std::fs::canonicalize(probe).ok();
    while canon_probe.is_none() {
        probe = probe.parent()?;
        canon_probe = std::fs::canonicalize(probe).ok();
    }
    if !canon_probe?.starts_with(&canon_project) {
        return None;
    }
    Some(joined)
}
