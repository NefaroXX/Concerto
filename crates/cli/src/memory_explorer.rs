//! Cancellable project-memory screen; chat drafts and runtime ownership stay in App.
use std::cell::Cell;
use std::path::PathBuf;
use std::sync::{mpsc, Arc};

use concerto_config::AppConfig;
use concerto_core::event::EventBus;
use concerto_core::memory::{ChunkType, MemoryChunk};
use concerto_core::CancellationToken;
use concerto_orchestrator::memory_explorer::MemoryExplorer;
use crossterm::event::KeyCode;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::widgets::{Block, BorderType, Borders, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;

use crate::theme::CliTheme;

type Reply = Result<(Arc<MemoryExplorer>, Vec<MemoryChunk>, String), String>;

#[derive(Default)]
pub(crate) struct MemoryScreen {
    pub(crate) entries: Vec<MemoryChunk>,
    pub(crate) selected: usize,
    pub(crate) query: String,
    pub(crate) editing_query: bool,
    pub(crate) detail: bool,
    pub(crate) pending_delete: Option<String>,
    pub(crate) status: String,
    kind: usize,
    explorer: Option<Arc<MemoryExplorer>>,
    receiver: Option<mpsc::Receiver<Reply>>,
    cancel: Option<CancellationToken>,
    scroll: u16,
    max_scroll: Cell<u16>,
    page: Cell<u16>,
}

const KINDS: &[(&str, Option<ChunkType>)] = &[
    ("All", None),
    ("Code / window", Some(ChunkType::SlidingWindow)),
    ("Session summary", Some(ChunkType::SessionSummary)),
    ("Fact", Some(ChunkType::Fact)),
    ("Function", Some(ChunkType::Function)),
    ("Struct", Some(ChunkType::Struct)),
    ("Trait", Some(ChunkType::Trait)),
    ("Impl", Some(ChunkType::Impl)),
    ("Enum", Some(ChunkType::Enum)),
    ("Module", Some(ChunkType::Module)),
    ("Test", Some(ChunkType::Test)),
];

impl Drop for MemoryScreen {
    fn drop(&mut self) {
        self.close();
    }
}

impl MemoryScreen {
    pub(crate) fn close(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            cancel.cancel();
        }
        self.receiver = None;
        self.explorer = None;
        self.pending_delete = None;
        self.editing_query = false;
    }

    pub(crate) fn open(
        &mut self,
        project: PathBuf,
        config: AppConfig,
        bus: EventBus,
        existing: Result<Option<MemoryExplorer>, String>,
    ) {
        self.close();
        self.entries.clear();
        self.selected = 0;
        self.detail = false;
        self.scroll = 0;
        let existing = match existing {
            Ok(existing) => existing,
            Err(error) => {
                self.status = error;
                return;
            }
        };
        if !config.memory.enabled {
            self.status = "Memory is disabled. Enable memory.enabled in Settings.".into();
            return;
        }
        self.status = "Loading project memory…".into();
        let (sender, receiver) = mpsc::channel();
        self.receiver = Some(receiver);
        let cancel = CancellationToken::new();
        self.cancel = Some(cancel.clone());
        let query = self.query.clone();
        let kind = KINDS[self.kind].1;
        tokio::spawn(async move {
            let result = async {
                let explorer = match existing {
                    Some(explorer) => explorer,
                    None => MemoryExplorer::open(&project, &config, bus, cancel.clone()).await?,
                };
                let explorer = Arc::new(explorer);
                let entries = explorer.search(&query, kind, 200, cancel.clone()).await?;
                Ok((explorer, entries, "Up to 200 matching entries".into()))
            };
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {},
                result = result => { let _ = sender.send(result); }
            }
        });
    }

    pub(crate) fn poll(&mut self) {
        let Some(receiver) = &self.receiver else {
            return;
        };
        let result = match receiver.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => {
                Err("Memory operation stopped; reopen the explorer to retry.".into())
            }
        };
        self.receiver = None;
        self.cancel = None;
        match result {
            Ok((explorer, entries, status)) => {
                self.explorer = Some(explorer);
                self.entries = entries;
                self.selected = self.selected.min(self.entries.len().saturating_sub(1));
                self.scroll = 0;
                self.status = status;
            }
            Err(error) => self.status = error,
        }
    }

    fn reload(&mut self, delete: Option<String>, reindex: bool) {
        let Some(explorer) = self.explorer.clone() else {
            return;
        };
        let (sender, receiver) = mpsc::channel();
        self.receiver = Some(receiver);
        let cancel = CancellationToken::new();
        self.cancel = Some(cancel.clone());
        self.status = if delete.is_some() {
            "Deleting confirmed entry…"
        } else if reindex {
            "Re-indexing project…"
        } else {
            "Searching…"
        }
        .into();
        let query = self.query.clone();
        let kind = KINDS[self.kind].1;
        tokio::spawn(async move {
            let result = async {
                let status = if let Some(id) = delete {
                    explorer.delete(&id, cancel.clone()).await?;
                    format!("Deleted {}", serde_json::json!(id))
                } else if reindex {
                    format!("Indexed {} chunks", explorer.reindex(cancel.clone()).await?)
                } else {
                    "Up to 200 matching entries".into()
                };
                let entries = explorer.search(&query, kind, 200, cancel.clone()).await?;
                Ok((explorer, entries, status))
            };
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {},
                result = result => { let _ = sender.send(result); }
            }
        });
    }

    pub(crate) fn paste(&mut self, text: &str) {
        if self.editing_query {
            self.query.extend(text.chars().filter(|c| !c.is_control()));
        }
    }

    /// Returns true only when the screen should close. Confirmation is bound
    /// to the captured chunk ID; Enter never confirms destructive actions.
    pub(crate) fn key(&mut self, key: KeyCode) -> bool {
        if self.pending_delete.is_some() {
            match key {
                KeyCode::Char('y') => {
                    let id = self.pending_delete.take();
                    self.detail = false;
                    self.reload(id, false);
                }
                KeyCode::Esc | KeyCode::Char('n') => self.pending_delete = None,
                _ => {}
            }
            return false;
        }
        if self.editing_query {
            match key {
                KeyCode::Esc => self.editing_query = false,
                KeyCode::Enter => {
                    self.editing_query = false;
                    self.selected = 0;
                    self.reload(None, false);
                }
                KeyCode::Backspace => {
                    self.query.pop();
                }
                KeyCode::Char(c) if !c.is_control() => self.query.push(c),
                _ => {}
            }
            return false;
        }
        if self.receiver.is_some() {
            if key == KeyCode::Esc {
                self.close();
                return true;
            }
            return false;
        }
        match key {
            KeyCode::Esc if self.detail => {
                self.detail = false;
                self.scroll = 0;
            }
            KeyCode::Esc | KeyCode::Char('q') => {
                self.close();
                return true;
            }
            KeyCode::Char('/') => {
                self.editing_query = true;
                self.detail = false;
            }
            KeyCode::Tab => {
                self.kind = (self.kind + 1) % KINDS.len();
                self.selected = 0;
                self.detail = false;
                self.reload(None, false);
            }
            KeyCode::BackTab => {
                self.kind = (self.kind + KINDS.len() - 1) % KINDS.len();
                self.selected = 0;
                self.detail = false;
                self.reload(None, false);
            }
            KeyCode::Char('r') => self.reload(None, false),
            KeyCode::Char('R') => self.reload(None, true),
            KeyCode::Char('d') if self.explorer.is_some() => {
                self.pending_delete = self.entries.get(self.selected).map(|chunk| chunk.id.clone());
            }
            KeyCode::Enter if !self.entries.is_empty() => {
                self.detail = true;
                self.scroll = 0;
            }
            KeyCode::Down | KeyCode::Char('j') => self.move_by(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_by(-1),
            KeyCode::PageDown => self.move_by(i32::from(self.page.get().max(1))),
            KeyCode::PageUp => self.move_by(-i32::from(self.page.get().max(1))),
            KeyCode::Home => {
                self.scroll = 0;
                if !self.detail {
                    self.selected = 0;
                }
            }
            KeyCode::End => {
                self.scroll = self.max_scroll.get();
                if !self.detail {
                    self.selected = self.entries.len().saturating_sub(1);
                }
            }
            _ => {}
        }
        false
    }

    fn move_by(&mut self, delta: i32) {
        if self.detail {
            self.scroll =
                (i32::from(self.scroll) + delta).clamp(0, i32::from(self.max_scroll.get())) as u16;
        } else {
            self.selected = self
                .selected
                .saturating_add_signed(delta as isize)
                .min(self.entries.len().saturating_sub(1));
        }
    }

    pub(crate) fn draw(&self, frame: &mut Frame, theme: &CliTheme) {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .title(" Memory ")
            .border_style(Style::default().fg(theme.border));
        let inner = block.inner(frame.area());
        frame.render_widget(block, frame.area());
        let areas = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(2),
        ])
        .split(inner);
        let query = format!(
            "{} {} | {}",
            if self.editing_query { "Search>" } else { "/ search:" },
            serde_json::json!(self.query),
            KINDS[self.kind].0
        );
        frame.render_widget(Paragraph::new(query).style(Style::default().fg(theme.user)), areas[0]);
        frame.render_widget(
            Paragraph::new(self.status.as_str()).style(Style::default().fg(theme.muted)),
            areas[1],
        );
        self.page.set(areas[2].height.max(1));
        if let Some(id) = &self.pending_delete {
            let content = format!("Delete {} from project memory?\nThis removes it from retrieval. Re-indexing can restore file-backed entries.\nPress y to delete; n or Esc to cancel.", serde_json::json!(id));
            frame.render_widget(
                Paragraph::new(content)
                    .wrap(Wrap { trim: false })
                    .style(Style::default().fg(theme.warning)),
                areas[2],
            );
        } else if self.detail {
            if let Some(chunk) = self.entries.get(self.selected) {
                let text = serde_json::to_string_pretty(chunk).unwrap_or_else(|e| e.to_string());
                let body = Paragraph::new(text).wrap(Wrap { trim: false });
                let max = body.line_count(areas[2].width).saturating_sub(areas[2].height as usize);
                let max = u16::try_from(max).unwrap_or(u16::MAX);
                self.max_scroll.set(max);
                frame.render_widget(body.scroll((self.scroll.min(max), 0)), areas[2]);
            }
        } else if self.entries.is_empty() {
            frame.render_widget(
                Paragraph::new("No matching entries. r refresh | R re-index")
                    .wrap(Wrap { trim: false }),
                areas[2],
            );
        } else {
            let items: Vec<ListItem> = self
                .entries
                .iter()
                .map(|chunk| {
                    let preview: String = chunk.content.chars().take(100).collect();
                    ListItem::new(format!(
                        "{}{} {}",
                        if chunk.stale { "[stale] " } else { "" },
                        serde_json::json!(chunk.id),
                        serde_json::json!(preview)
                    ))
                })
                .collect();
            let list = List::new(items)
                .highlight_symbol("› ")
                .highlight_style(Style::default().fg(theme.user).add_modifier(Modifier::BOLD));
            let mut state = ListState::default().with_selected(Some(self.selected));
            frame.render_stateful_widget(list, areas[2], &mut state);
        }
        let help = if self.detail {
            "j/k PgUp/PgDn Home/End\nEsc list | d delete"
        } else if inner.width < 60 {
            "/ search | Tab type\nEnter view | d delete | Esc"
        } else {
            "/ search | Tab/Shift+Tab type | Enter full entry | d delete\nj/k select | r refresh | R re-index | Esc back"
        };
        frame.render_widget(Paragraph::new(help).style(Style::default().fg(theme.muted)), areas[3]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use concerto_core::memory::{MemoryNamespace, ProjectId};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn entries() -> Vec<MemoryChunk> {
        (0..20)
            .map(|index| MemoryChunk {
                id: format!("chunk-{index}"),
                project_id: ProjectId("project".into()),
                namespace: MemoryNamespace::Project(ProjectId("project".into())),
                content: "full content\n\u{1b}[2J".repeat(300),
                file_path: Some("src/lib.rs".into()),
                start_line: Some(1),
                end_line: Some(30),
                chunk_type: ChunkType::Fact,
                score: 0.5,
                model_id: "model".into(),
                model_version: "v1".into(),
                stale: false,
            })
            .collect()
    }

    #[test]
    fn query_navigation_confirmation_and_close_do_not_dispatch_chat() {
        let mut screen = MemoryScreen::default();
        screen.entries.extend(entries());
        assert!(!screen.key(KeyCode::Char('/')));
        screen.paste("世界\n\u{1b}query");
        assert_eq!(screen.query, "世界query");
        screen.key(KeyCode::Backspace);
        screen.key(KeyCode::Esc);
        assert!(!screen.editing_query);
        screen.key(KeyCode::End);
        assert_eq!(screen.selected, 19);
        screen.key(KeyCode::Enter);
        assert!(screen.detail);
        assert!(!screen.key(KeyCode::Esc));
        assert!(!screen.detail);
        screen.pending_delete.replace("chunk-19".into());
        screen.key(KeyCode::Enter);
        assert_eq!(screen.pending_delete.as_deref(), Some("chunk-19"));
        screen.key(KeyCode::Down);
        assert_eq!(screen.selected, 19);
        screen.key(KeyCode::Char('n'));
        assert!(screen.pending_delete.is_none());
        assert_eq!(screen.entries.len(), 20);
        assert!(screen.key(KeyCode::Esc));
    }

    #[test]
    fn close_cancels_pending_work_and_discards_late_results() {
        let mut screen = MemoryScreen::default();
        let (sender, receiver) = mpsc::channel();
        let cancel = CancellationToken::new();
        screen.cancel.replace(cancel.clone());
        screen.receiver.replace(receiver);
        screen.close();
        assert!(cancel.is_cancelled());
        assert!(sender.send(Err("late failure".into())).is_err());
        screen.poll();
        assert!(screen.status.is_empty());
    }

    #[test]
    fn selected_rows_and_full_detail_fit_small_terminals() {
        for (width, height) in [(24, 8), (40, 12), (80, 24), (120, 40)] {
            let mut screen = MemoryScreen::default();
            screen.entries.extend(entries());
            screen.key(KeyCode::End);
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            let theme = CliTheme::by_name("Midnight");
            terminal.draw(|frame| screen.draw(frame, &theme)).unwrap();
            let dump = |terminal: &Terminal<TestBackend>| {
                terminal
                    .backend()
                    .buffer()
                    .content
                    .iter()
                    .map(|cell| cell.symbol())
                    .collect::<String>()
            };
            assert!(dump(&terminal).contains("chunk-19"), "{width}x{height}");
            assert!(!dump(&terminal).contains('\u{1b}'));
            screen.key(KeyCode::Enter);
            terminal.draw(|frame| screen.draw(frame, &theme)).unwrap();
            assert!(screen.max_scroll.get() > 0);
            let first = dump(&terminal);
            screen.key(KeyCode::End);
            terminal.draw(|frame| screen.draw(frame, &theme)).unwrap();
            assert_ne!(first, dump(&terminal));
            screen.key(KeyCode::Home);
            assert_eq!(screen.scroll, 0);
        }
    }
}
