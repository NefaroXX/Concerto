//! Session-local documents and selected-file review. No writes occur on tab switches.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use camino::Utf8Path;
use concerto_core::CancellationToken;
use concerto_tools::diff::compute_diffs_from_virtual_fs;
use concerto_tools::virtual_fs::VirtualFs;
use iced::widget::text_editor;

use super::{ActiveFold, Diagnostic, EditKind, HistoryEntry, Message, State, TabMode};

pub(crate) struct Buffer {
    content: text_editor::Content,
    dirty: bool,
    diagnostics: Vec<Diagnostic>,
    lsp_version: i32,
    lsp_status: String,
    undo_stack: VecDeque<HistoryEntry>,
    redo_stack: Vec<HistoryEntry>,
    last_edit_kind: Option<EditKind>,
    folds: Vec<ActiveFold>,
    tab_mode: TabMode,
    line_ending: &'static str,
}

impl State {
    pub(crate) fn store_active_buffer(&mut self) {
        let Some(path) = self.active_file.clone() else { return };
        let Some(content) = self.content.take() else { return };
        self.buffers.insert(
            path,
            Buffer {
                content,
                dirty: self.dirty,
                diagnostics: std::mem::take(&mut self.diagnostics),
                lsp_version: self.lsp_version,
                lsp_status: self.lsp_status.clone(),
                undo_stack: std::mem::take(&mut self.undo_stack),
                redo_stack: std::mem::take(&mut self.redo_stack),
                last_edit_kind: self.last_edit_kind.take(),
                folds: std::mem::take(&mut self.folds),
                tab_mode: self.tab_mode,
                line_ending: self.line_ending,
            },
        );
    }

    pub(crate) fn restore_buffer(&mut self, path: &Utf8Path) -> bool {
        let Some(buffer) = self.buffers.remove(path) else { return false };
        self.active_file = Some(path.to_path_buf());
        self.content = Some(buffer.content);
        self.dirty = buffer.dirty;
        self.diagnostics = buffer.diagnostics;
        self.lsp_version = buffer.lsp_version;
        self.lsp_status = buffer.lsp_status;
        self.undo_stack = buffer.undo_stack;
        self.redo_stack = buffer.redo_stack;
        self.last_edit_kind = buffer.last_edit_kind;
        self.folds = buffer.folds;
        self.tab_mode = buffer.tab_mode;
        self.line_ending = buffer.line_ending;
        self.lang = crate::widgets::file_tree::lang_for_file(path);
        true
    }

    pub(crate) fn reset_document_widgets(&mut self) {
        self.completion_open = false;
        self.completion_items.clear();
        self.hover = None;
        self.review_open = false;
        self.find_current = None;
        self.region_cache = (-1, 0, Vec::new());
        self.find_matches.clear();
        self.find_overflow = false;
        if self.find_open {
            self.expand_intersecting(0, usize::MAX);
        }
        self.refresh_cursor_insights();
        if self.find_open {
            self.refresh_matches();
        }
    }

    pub(crate) fn tab_dirty(&self, path: &Utf8Path) -> bool {
        if self.active_file.as_deref() == Some(path) {
            self.dirty
        } else {
            self.buffers.get(path).is_some_and(|buffer| buffer.dirty)
        }
    }

    pub(crate) fn close_tab(
        &mut self,
        path: &Utf8Path,
        vfs: &Arc<Mutex<VirtualFs>>,
        project_dir: &Utf8Path,
        cancel: &CancellationToken,
    ) -> iced::Task<Message> {
        let Some(index) = self.tabs.iter().position(|p| p == path) else {
            return iced::Task::none();
        };
        self.tabs.remove(index);
        self.buffers.remove(path);
        let project = project_dir.to_path_buf();
        let file = path.to_path_buf();
        let cancellation = cancel.clone();
        let close_task = iced::Task::perform(
            async move {
                if let Err(error) = super::lsp_did_close(project, file, cancellation).await {
                    tracing::debug!(%error, "LSP document close failed");
                }
            },
            |()| Message::LspClosed,
        );
        if self.active_file.as_deref() != Some(path) {
            return close_task;
        }
        self.active_file = None;
        self.content = None;
        self.dirty = false;
        self.diagnostics.clear();
        self.undo_stack.clear();
        self.redo_stack.clear();
        self.folds.clear();
        self.document_revision = self.document_revision.wrapping_add(1);
        self.reset_document_widgets();
        let next = self.tabs.get(index.min(self.tabs.len().saturating_sub(1))).cloned();
        let open_task = match next {
            Some(path) => self.dispatch_file_open(&path, vfs, project_dir, cancel),
            None => {
                self.lang = "plain";
                self.lsp_status = "LSP idle".into();
                self.line_ending = "LF";
                iced::Task::none()
            }
        };
        close_task.chain(open_task)
    }

    /// Positions use editor byte columns; LSP conversion occurs at the request boundary.
    pub(crate) fn cursor_position(&self) -> Option<(usize, usize)> {
        self.content.as_ref().map(|content| {
            let position = content.cursor().position;
            (position.line, position.column)
        })
    }

    pub(crate) fn scope_task(
        &self,
        task: iced::Task<Message>,
        position_sensitive: bool,
    ) -> iced::Task<Message> {
        let Some(path) = self.active_file.clone() else { return task };
        let revision = self.document_revision;
        let cursor = if position_sensitive { self.cursor_position() } else { None };
        task.map(move |reply| Message::DocumentReply {
            path: path.clone(),
            revision,
            cursor,
            reply: Box::new(reply),
        })
    }

    /// Received diagnostics only: this list does not claim to index unopened files.
    pub(crate) fn document_diagnostics(&self) -> Vec<(&Utf8Path, &Diagnostic)> {
        let mut result = Vec::new();
        for path in &self.tabs {
            let diagnostics = if self.active_file.as_deref() == Some(path.as_path()) {
                self.diagnostics.as_slice()
            } else {
                self.buffers.get(path).map(|b| b.diagnostics.as_slice()).unwrap_or(&[])
            };
            result.extend(diagnostics.iter().map(|diagnostic| (path.as_path(), diagnostic)));
        }
        result
    }

    pub(crate) fn refresh_staged(&mut self, vfs: &Arc<Mutex<VirtualFs>>) {
        if let Ok(guard) = vfs.lock() {
            self.set_staged_results(&guard, compute_diffs_from_virtual_fs(&guard));
        }
    }

    pub(crate) fn set_staged_results(
        &mut self,
        vfs: &VirtualFs,
        mut results: Vec<concerto_api_types::diff::DiffResult>,
    ) {
        use concerto_tools::virtual_fs::VirtualFsEntry;
        self.staged_entries.clear();
        for path in vfs.changed_paths() {
            let Some(entry) = vfs.get(path) else { continue };
            let has_diff = results.iter().any(|result| result.path == path);
            let file_operation =
                matches!(entry, VirtualFsEntry::Created { .. } | VirtualFsEntry::Deleted { .. });
            if !has_diff && file_operation {
                results.push(concerto_api_types::diff::DiffResult {
                    path: path.to_path_buf(),
                    hunks: Vec::new(),
                });
            }
            if has_diff || file_operation {
                self.staged_entries.insert(path.to_path_buf(), entry.clone());
            }
        }
        results.sort_by(|a, b| a.path.cmp(&b.path));
        self.staged = results;
    }

    pub(crate) fn staged_file(&self) -> Option<&concerto_api_types::diff::DiffResult> {
        let path = self.active_file.as_ref()?;
        self.staged.iter().find(|result| &result.path == path)
    }

    pub(crate) fn staged_hunks(&self, path: &Utf8Path) -> usize {
        use concerto_api_types::diff::DiffLine;
        self.staged
            .iter()
            .find(|result| result.path == path)
            .map(|result| {
                result
                    .hunks
                    .iter()
                    .filter(|hunk| {
                        hunk.lines.iter().any(|line| !matches!(line, DiffLine::Context { .. }))
                    })
                    .count()
                    .max(1)
            })
            .unwrap_or(0)
    }

    /// Explicit user decision, scoped to a single entry; failures retain the overlay.
    pub(crate) fn decide_staged(
        &mut self,
        accept: bool,
        vfs: &Arc<Mutex<VirtualFs>>,
        project_dir: &Utf8Path,
        cancel: &CancellationToken,
    ) -> iced::Task<Message> {
        let Some(path) = self.active_file.clone() else { return iced::Task::none() };
        if self.dirty {
            return iced::Task::done(Message::LspError(
                "Save your edits before accepting or discarding staged changes.".into(),
            ));
        }
        let reviewed = self.staged_entries.get(&path).cloned();
        let result = (|| {
            let mut guard = vfs.lock().map_err(|_| "Staged changes are unavailable".to_string())?;
            if !guard.changed_paths().contains(&path.as_path()) {
                return Err("This file has no staged changes".to_string());
            }
            if reviewed.is_none() || guard.get(&path) != reviewed.as_ref() {
                return Err(
                    "Staged changes changed. Review the latest diff before deciding.".into()
                );
            }
            if accept {
                guard.materialize_paths(std::slice::from_ref(&path)).map_err(|e| e.to_string())?;
            }
            guard.unstage(&path);
            Ok(())
        })();
        if let Err(error) = result {
            return iced::Task::done(Message::LspError(error));
        }
        self.review_open = false;
        self.tree_dirty = true;
        self.refresh_staged(vfs);
        self.buffers.remove(&path);
        if path.as_std_path().exists() {
            let text = match std::fs::read_to_string(&path) {
                Ok(text) => text,
                Err(error) => return iced::Task::done(Message::LspError(error.to_string())),
            };
            let position = self.cursor_position().unwrap_or((0, 0));
            let mut content = text_editor::Content::with_text(&text);
            content.move_to(super::clamp_cursor(&content, position.0, position.1));
            self.content = Some(content);
            self.diagnostics.clear();
            self.folds.clear();
            self.undo_stack.clear();
            self.redo_stack.clear();
            self.last_edit_kind = None;
            self.document_revision = self.document_revision.wrapping_add(1);
            self.lsp_version += 1;
            self.reset_document_widgets();
            let project = project_dir.to_path_buf();
            let cancellation = cancel.clone();
            let version = self.lsp_version;
            let task = iced::Task::perform(
                async move { super::lsp_did_change(project, path, text, version, cancellation).await },
                |result| match result {
                    Ok(()) => Message::LspReady,
                    Err(error) => Message::LspError(error),
                },
            );
            self.scope_task(task, false)
        } else {
            self.close_tab(&path, vfs, project_dir, cancel)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::DiagnosticSeverity;
    use super::*;
    use camino::Utf8PathBuf;

    struct Fixture {
        _dir: tempfile::TempDir,
        root: Utf8PathBuf,
        first: Utf8PathBuf,
        second: Utf8PathBuf,
        vfs: Arc<Mutex<VirtualFs>>,
        cancel: CancellationToken,
    }

    impl Fixture {
        fn new() -> Result<Self, Box<dyn std::error::Error>> {
            let dir = tempfile::tempdir()?;
            let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf())
                .map_err(|p| std::io::Error::other(format!("Non-UTF8 test path: {p:?}")))?;
            let first = root.join("first.rs");
            let second = root.join("second.rs");
            std::fs::write(&first, "fn first() {\n    let n = 1;\n}\n")?;
            std::fs::write(&second, "fn second() {}\n")?;
            Ok(Self {
                _dir: dir,
                root,
                first,
                second,
                vfs: Arc::new(Mutex::new(VirtualFs::new())),
                cancel: CancellationToken::new(),
            })
        }

        fn update(&self, state: &mut State, message: Message) {
            let _ = state.update(message, &self.vfs, &self.root, &self.cancel);
        }
    }

    fn buffer_text(state: &State) -> String {
        state.content.as_ref().map(|content| content.text()).unwrap_or_default()
    }

    /// Keyboard tab navigation wraps without saving, and is safe with an empty workspace.
    #[test]
    fn keyboard_tab_navigation_wraps_and_handles_empty_workspaces(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let f = Fixture::new()?;
        let mut state = State::new(f.root.clone());
        f.update(&mut state, Message::NextTab);
        f.update(&mut state, Message::PreviousTab);
        f.update(&mut state, Message::CloseActiveTab);
        assert!(state.tabs.is_empty());
        state.open_file(&f.first, &f.vfs);
        state.open_file(&f.second, &f.vfs);
        f.update(&mut state, Message::NextTab);
        assert_eq!(state.active_file.as_ref(), Some(&f.first));
        f.update(&mut state, Message::PreviousTab);
        assert_eq!(state.active_file.as_ref(), Some(&f.second));
        Ok(())
    }

    /// Switching documents retains edits, cursor and undo; it does not save either file.
    #[test]
    fn switching_tabs_preserves_unsaved_history_and_cursor(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let f = Fixture::new()?;
        let original = std::fs::read_to_string(&f.first)?;
        let mut state = State::new(f.root.clone());
        state.open_file(&f.first, &f.vfs);
        f.update(
            &mut state,
            Message::Edit(text_editor::Action::Edit(text_editor::Edit::Insert('x'))),
        );
        let edited = buffer_text(&state);
        let cursor = state.cursor_position();
        state.open_file(&f.second, &f.vfs);
        f.update(
            &mut state,
            Message::Edit(text_editor::Action::Edit(text_editor::Edit::Insert('y'))),
        );
        assert!(state.tab_dirty(&f.first));
        state.open_file(&f.first, &f.vfs);
        assert_eq!(buffer_text(&state), edited);
        assert_eq!(state.cursor_position(), cursor);
        assert!(state.dirty);
        assert_eq!(state.tabs.len(), 2);
        assert_eq!(std::fs::read_to_string(&f.first)?, original);
        f.update(&mut state, Message::Undo);
        assert_eq!(buffer_text(&state), original);
        f.update(&mut state, Message::Redo);
        assert_eq!(buffer_text(&state), edited);
        Ok(())
    }

    /// Closing a dirty inactive tab is cancellable and preserves its text; unknown tabs are no-ops.
    #[test]
    fn dirty_tab_close_requires_explicit_discard() -> Result<(), Box<dyn std::error::Error>> {
        let f = Fixture::new()?;
        let mut state = State::new(f.root.clone());
        state.open_file(&f.first, &f.vfs);
        f.update(
            &mut state,
            Message::Edit(text_editor::Action::Edit(text_editor::Edit::Insert('x'))),
        );
        let edited = buffer_text(&state);
        state.open_file(&f.second, &f.vfs);
        f.update(&mut state, Message::CloseTab(f.first.clone()));
        assert_eq!(state.pending_close.as_ref(), Some(&f.first));
        assert_eq!(state.tabs.len(), 2);
        f.update(&mut state, Message::CloseTabCancelled);
        state.open_file(&f.first, &f.vfs);
        assert_eq!(buffer_text(&state), edited);
        f.update(&mut state, Message::CloseTab(f.root.join("unknown.rs")));
        assert_eq!(state.tabs.len(), 2);
        f.update(&mut state, Message::CloseTab(f.first.clone()));
        f.update(&mut state, Message::CloseTabConfirmed);
        assert_eq!(state.active_file.as_ref(), Some(&f.second));
        assert_eq!(state.tabs, vec![f.second.clone()]);
        assert!(!std::fs::read_to_string(&f.first)?.starts_with('x'));
        Ok(())
    }

    /// Diagnostics from another document/revision and positional results from another cursor are ignored.
    #[test]
    fn asynchronous_replies_cannot_cross_document_or_cursor(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let f = Fixture::new()?;
        let mut state = State::new(f.root.clone());
        state.open_file(&f.first, &f.vfs);
        let revision = state.document_revision;
        for (path, revision, cursor) in [
            (f.second.clone(), revision, None),
            (f.first.clone(), revision.wrapping_sub(1), None),
            (f.first.clone(), revision, Some((99, 99))),
        ] {
            f.update(
                &mut state,
                Message::DocumentReply {
                    path,
                    revision,
                    cursor,
                    reply: Box::new(Message::LspHover("stale".into())),
                },
            );
            assert!(state.hover.is_none());
        }
        let cursor = state.cursor_position();
        f.update(
            &mut state,
            Message::DocumentReply {
                path: f.first.clone(),
                revision,
                cursor,
                reply: Box::new(Message::LspHover("current".into())),
            },
        );
        assert_eq!(state.hover.as_deref(), Some("current"));
        Ok(())
    }

    /// Diagnostic navigation restores a dirty document and translates UTF-16 without corrupting Unicode.
    #[test]
    fn problems_navigation_preserves_text_and_converts_unicode(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let f = Fixture::new()?;
        std::fs::write(&f.first, "a😀b\n")?;
        let mut state = State::new(f.root.clone());
        state.open_file(&f.first, &f.vfs);
        state.diagnostics = vec![Diagnostic {
            line: 0,
            character: 3,
            message: "problem".into(),
            severity: DiagnosticSeverity::Warning,
        }];
        state.dirty = true;
        let original = buffer_text(&state);
        state.open_file(&f.second, &f.vfs);
        assert_eq!(state.document_diagnostics().len(), 1);
        f.update(&mut state, Message::DiagnosticSelected(f.first.clone(), 0, 3));
        assert_eq!(state.cursor_position(), Some((0, 5)));
        assert_eq!(buffer_text(&state), original);
        assert!(state.dirty);
        f.update(&mut state, Message::DiagnosticSelected(f.first.clone(), usize::MAX, usize::MAX));
        assert_eq!(buffer_text(&state), original);
        Ok(())
    }

    /// Accepting one staged file leaves unrelated overlays and disk files unchanged.
    #[test]
    fn review_acceptance_is_scoped_to_the_selected_file() -> Result<(), Box<dyn std::error::Error>>
    {
        let f = Fixture::new()?;
        let second_original = std::fs::read_to_string(&f.second)?;
        {
            let mut vfs = f.vfs.lock().map_err(|e| e.to_string())?;
            vfs.write(&f.first, "accepted\n".into())?;
            vfs.write(&f.second, "unrelated proposal\n".into())?;
        }
        let mut state = State::new(f.root.clone());
        state.open_file(&f.first, &f.vfs);
        f.update(&mut state, Message::AcceptStaged);
        assert_eq!(std::fs::read_to_string(&f.first)?, "accepted\n");
        assert_eq!(std::fs::read_to_string(&f.second)?, second_original);
        let vfs = f.vfs.lock().map_err(|e| e.to_string())?;
        assert!(vfs.get(&f.first).is_none());
        assert_eq!(vfs.read(&f.second)?, "unrelated proposal\n");
        Ok(())
    }

    /// Dirty review is blocked; discard removes only the overlay and never writes the disk baseline.
    #[test]
    fn review_blocks_dirty_buffers_and_discards_only_the_overlay(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let f = Fixture::new()?;
        let original = std::fs::read_to_string(&f.first)?;
        f.vfs.lock().map_err(|e| e.to_string())?.write(&f.first, "proposal\n".into())?;
        let mut state = State::new(f.root.clone());
        state.open_file(&f.first, &f.vfs);
        state.dirty = true;
        f.update(&mut state, Message::DiscardStaged);
        f.update(&mut state, Message::AcceptStaged);
        assert_eq!(buffer_text(&state), "proposal\n");
        assert!(f.vfs.lock().map_err(|e| e.to_string())?.get(&f.first).is_some());
        assert_eq!(std::fs::read_to_string(&f.first)?, original);
        state.dirty = false;
        f.update(&mut state, Message::DiscardStaged);
        assert_eq!(buffer_text(&state), original);
        assert_eq!(std::fs::read_to_string(&f.first)?, original);
        assert!(f.vfs.lock().map_err(|e| e.to_string())?.get(&f.first).is_none());
        Ok(())
    }

    /// A proposal changed after presentation cannot be accepted or discarded by an old review.
    #[test]
    fn changed_proposals_and_failed_materialization_retain_staging(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let f = Fixture::new()?;
        let original = std::fs::read_to_string(&f.first)?;
        f.vfs.lock().map_err(|e| e.to_string())?.write(&f.first, "first proposal\n".into())?;
        let mut state = State::new(f.root.clone());
        state.open_file(&f.first, &f.vfs);
        f.vfs.lock().map_err(|e| e.to_string())?.write(&f.first, "newer proposal\n".into())?;
        f.update(&mut state, Message::AcceptStaged);
        f.update(&mut state, Message::DiscardStaged);
        assert_eq!(std::fs::read_to_string(&f.first)?, original);
        assert_eq!(f.vfs.lock().map_err(|e| e.to_string())?.read(&f.first)?, "newer proposal\n");
        let blocked = f.root.join("blocked");
        std::fs::write(&blocked, "a file blocks this directory")?;
        let proposal = blocked.join("created.rs");
        f.vfs.lock().map_err(|e| e.to_string())?.write(&proposal, "created\n".into())?;
        state.open_file(&proposal, &f.vfs);
        f.update(&mut state, Message::AcceptStaged);
        assert_eq!(f.vfs.lock().map_err(|e| e.to_string())?.read(&proposal)?, "created\n");
        assert_eq!(buffer_text(&state), "created\n");
        assert_eq!(std::fs::read_to_string(&blocked)?, "a file blocks this directory");
        Ok(())
    }

    /// Discarding a created overlay closes its nonexistent tab and restores the neighboring document.
    #[test]
    fn discard_created_file_closes_only_its_tab() -> Result<(), Box<dyn std::error::Error>> {
        let f = Fixture::new()?;
        let created = f.root.join("created.rs");
        f.vfs.lock().map_err(|e| e.to_string())?.write(&created, "new\n".into())?;
        let mut state = State::new(f.root.clone());
        state.open_file(&f.first, &f.vfs);
        state.open_file(&created, &f.vfs);
        f.update(&mut state, Message::DiscardStaged);
        assert!(!created.as_std_path().exists());
        assert_eq!(state.active_file.as_ref(), Some(&f.first));
        assert_eq!(state.tabs, vec![f.first.clone()]);
        Ok(())
    }

    /// Empty file creation/deletion remains reviewable, and changing an entry's kind invalidates the old decision.
    #[test]
    fn review_tracks_file_operations_without_text_hunks() -> Result<(), Box<dyn std::error::Error>>
    {
        use concerto_tools::virtual_fs::VirtualFsEntry;
        let f = Fixture::new()?;
        let created = f.root.join("empty.rs");
        f.vfs.lock().map_err(|e| e.to_string())?.write(&created, String::new())?;
        let mut state = State::new(f.root.clone());
        state.open_file(&created, &f.vfs);
        assert_eq!(state.staged_hunks(&created), 1);
        f.update(&mut state, Message::AcceptStaged);
        assert!(created.as_std_path().exists());
        assert_eq!(std::fs::read_to_string(&created)?, "");
        f.vfs.lock().map_err(|e| e.to_string())?.stage_delete(&created)?;
        f.update(&mut state, Message::ReviewStaged);
        assert_eq!(state.staged_hunks(&created), 1);
        f.update(&mut state, Message::AcceptStaged);
        assert!(!created.as_std_path().exists());
        assert!(state.tabs.is_empty());

        f.vfs.lock().map_err(|e| e.to_string())?.write(&f.first, String::new())?;
        state.open_file(&f.first, &f.vfs);
        let original = std::fs::read_to_string(&f.first)?;
        f.vfs
            .lock()
            .map_err(|e| e.to_string())?
            .insert(VirtualFsEntry::Deleted { path: f.first.clone(), original: original.clone() });
        f.update(&mut state, Message::AcceptStaged);
        assert_eq!(std::fs::read_to_string(&f.first)?, original);
        Ok(())
    }

    /// Populated editor, review, search and completion all render in each supported theme.
    #[test]
    fn populated_workspace_renders_in_all_themes() -> Result<(), Box<dyn std::error::Error>> {
        let f = Fixture::new()?;
        f.vfs.lock().map_err(|e| e.to_string())?.write(&f.first, "staged\n".into())?;
        let mut state = State::new(f.root.clone());
        state.open_file(&f.first, &f.vfs);
        state.find_open = true;
        state.replace_open = true;
        state.completion_open = true;
        state.completion_items = (0..30)
            .map(|i| super::super::CompletionItem {
                label: format!("completion_{i}"),
                detail: Some("usize".into()),
                insert_text: "value".into(),
            })
            .collect();
        state.completion_selected = 29;
        for theme in crate::theme::AppTheme::all() {
            let _ = state.view(&theme);
            state.review_open = true;
            let _ = state.view(&theme);
            state.review_open = false;
        }
        Ok(())
    }

    /// Hiding Problems retains Explorer and the editor; showing it restores the bottom split.
    #[test]
    fn problems_toggle_preserves_the_explorer_split() -> Result<(), Box<dyn std::error::Error>> {
        let f = Fixture::new()?;
        let mut state = State::new(f.root.clone());
        let tree_pane = state.tree_pane;
        let editor_pane = state.editor_pane;
        f.update(&mut state, Message::ToggleDiagnostics);
        assert!(!state.show_diagnostics);
        assert_eq!(state.pane_state.len(), 2);
        assert_eq!(state.tree_pane, tree_pane);
        assert_eq!(state.editor_pane, editor_pane);
        f.update(&mut state, Message::ToggleDiagnostics);
        assert!(state.show_diagnostics);
        assert_eq!(state.pane_state.len(), 3);
        Ok(())
    }
}
