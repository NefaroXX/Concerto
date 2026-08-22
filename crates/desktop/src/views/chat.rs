use iced::widget::{
    button, column, container, pick_list, row, rule, scrollable, text, text_input, toggler, tooltip,
};
use iced::{border::Radius, Alignment, Background, Border, Color, Element, Length};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

use crate::theme::AppTheme;
use crate::views::agent_graph;
use crate::views::spend::{
    cap_status_text, compact_created_at, spend_totals, CapUiState, SpendTotals,
};
use crate::widgets::agent_graph::NodeState;
use crate::widgets::markdown;
use concerto_core::types::AgentId;
use concerto_sessions::spend::SpendRecord;

/// Unique entry identifier within a chat session.
pub type EntryId = usize;

/// Which sub-view overlay is active inside the chat canvas (if any).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SubView {
    /// Show the normal chat view (no overlay).
    #[default]
    Main,
    /// Diff overlay.
    Diff,
    /// Agent Graph overlay.
    AgentGraph,
    /// Tool Log modal overlay.
    ToolLog,
    /// Spend Log modal overlay (status-bar spend chip).
    SpendLog,
}

/// Messages that the chat view can handle.
#[derive(Debug, Clone)]
pub enum Message {
    InputChanged(String),
    SubmitInput,
    AddUser(String),
    AddAssistant(String),
    AddThinking(String),
    AddToolCall(String),
    /// Blink the streaming cursor: toggles `streaming_cursor_visible`.
    /// Driven by a 500 ms subscription in `app.rs` that stays alive only
    /// while an assistant entry is still streaming.
    StreamingTick,
    /// Advance the typewriter-reveal frontier on the live streaming
    /// assistant entry. Driven by a 16 ms subscription in `app.rs` that stays
    /// alive only while a reveal is in progress.
    TypingTick,
    /// Seed the composer from an empty-state quick action.
    UsePrompt(String),
    /// Resume a project session from the sidebar project tree.
    SelectSession(String),
    ToggleEntry(EntryId),
    CopyCode(String),
    ToggleMultiAgent,
    ToggleFastMode,
    NavigateToToolLog(String),
    /// Navigate to the Diff viewer to review post-run changes.
    NavigateToDiff,
    /// Navigate to the Settings page for provider setup.
    NavigateToSettings,
    /// Navigate to the Orchestration Studio for agent configuration.
    NavigateToStudio,
    /// User selected a model for the session.
    SetActiveModel(String),
    /// User clicked the New Session button — clear chat and start fresh.
    NewSession,
    /// Set the active sub-view overlay (Diff / AgentGraph / ToolLog / Main).
    SetSubView(SubView),
    /// Spend records for the active session loaded (Spend Log modal body).
    /// Driven from `App::load_spend_log`; best-effort (empty on failure).
    SpendLogsLoaded(Vec<SpendRecord>),
    /// Refresh the Spend Log modal's records. Handled by App (needs the
    /// session handler), like `SelectSession`.
    RefreshSpendLog,
}

/// One session row shown in the sidebar project tree. Moved here from the
/// removed Dashboard view so the tree can list per-project sessions.
#[derive(Debug, Clone)]
pub struct SessionRow {
    pub session_id: String,
    pub created_at: String,
    pub message_count: usize,
    pub cost: f64,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub duration: String,
    pub provider: String,
}

/// Internal representation of a chat entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ChatEntry {
    User {
        id: EntryId,
        content: String,
        /// RFC3339 UTC timestamp recorded when the entry was created. Legacy
        /// v1 transcripts omit the field; `None` keeps those entries loadable.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        created_at: Option<String>,
    },
    Assistant {
        id: EntryId,
        content: String,
        streaming: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        created_at: Option<String>,
    },
    Thinking {
        id: EntryId,
        content: String,
        collapsed: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        created_at: Option<String>,
        /// RFC3339 UTC timestamp marking the end of the thinking phase.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        finished_at: Option<String>,
    },
    ToolCall {
        id: EntryId,
        tool_name: String,
        detail: String,
        status: ToolCallStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        created_at: Option<String>,
    },
    Completion {
        id: EntryId,
        summary: RunCompletionSummary,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        created_at: Option<String>,
    },
    Error {
        id: EntryId,
        content: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        created_at: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ToolCallStatus {
    Running,
    Completed,
    Failed,
    Allowed,
    Denied,
    Cancelled,
}

/// Structured completion data rendered after a run. This deliberately stays
/// separate from the model-authored final message so file chips and completion
/// state cannot be fabricated by prose.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunCompletionSummary {
    pub multi_agent: bool,
    pub completed: bool,
    pub files: Vec<String>,
    pub project_root: Option<String>,
}

/// On-disk transcript wrapper. Version 2 added `created_at` timestamps.
const TRANSCRIPT_VERSION: u32 = 2;
#[derive(Debug, Clone, Serialize, Deserialize)]
struct TranscriptFile {
    version: u32,
    entries: Vec<ChatEntry>,
}

fn display_project_path(path: &str) -> String {
    // Windows canonical paths commonly carry the extended-length prefix. It
    // is useful to filesystem APIs but is implementation noise in the UI.
    path.strip_prefix(r"\\?\").unwrap_or(path).to_string()
}

/// Current UTC time as an RFC3339 string. Rfc3339 formatting of `now_utc`
/// cannot fail in practice; the empty-string fallback keeps this free of
/// panics.
fn now_rfc3339() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| String::new())
}

/// Whole elapsed seconds between two RFC3339 timestamps, clamped to a
/// non-negative value so clock adjustments can never render a negative
/// duration (e.g. "⏱ -5s"), or `None` when either timestamp fails to parse.
fn elapsed_seconds(start: &str, end: &str) -> Option<i64> {
    let start =
        time::OffsetDateTime::parse(start, &time::format_description::well_known::Rfc3339).ok()?;
    let end =
        time::OffsetDateTime::parse(end, &time::format_description::well_known::Rfc3339).ok()?;
    Some((end - start).whole_seconds().max(0))
}

/// Compact `MM-DD HH:MM` label for a chat entry's `created_at`. Returns `None`
/// for legacy entries that carry no timestamp and when the RFC3339 text is
/// unparseable. Chat entries store RFC3339 strings — unlike spend records,
/// which carry `OffsetDateTime` — so the text is parsed before being passed to
/// `views::spend::compact_created_at`.
fn compact_entry_created_at(created_at: &Option<String>) -> Option<String> {
    let text = created_at.as_deref()?;
    let dt =
        time::OffsetDateTime::parse(text, &time::format_description::well_known::Rfc3339).ok()?;
    Some(compact_created_at(dt))
}

/// Right-aligned, unobtrusive timestamp line placed below a user bubble or
/// assistant message block. `None` for legacy entries without `created_at`.
fn compact_timestamp_line<'a>(
    created_at: &'a Option<String>,
    palette: &'a crate::theme::Palette,
) -> Option<Element<'a, Message>> {
    let ts = compact_entry_created_at(created_at)?;
    Some(
        row![iced::widget::space::horizontal(), text(ts).size(11).color(palette.text_muted),]
            .into(),
    )
}

impl std::fmt::Display for ToolCallStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Running => write!(f, "running"),
            Self::Completed => write!(f, "completed"),
            Self::Failed => write!(f, "failed"),
            Self::Allowed => write!(f, "allowed"),
            Self::Denied => write!(f, "denied"),
            Self::Cancelled => write!(f, "cancelled"),
        }
    }
}

pub struct State {
    entries: Vec<ChatEntry>,
    input: String,
    next_id: EntryId,
    /// Active sub-view overlay shown on top of the chat canvas.
    pub sub_view: SubView,
    /// Whether the blinking cursor is currently shown on the live streaming
    /// assistant entry. Toggled by `Message::StreamingTick`; only meaningful
    /// (and only ever read) while at least one entry is still streaming.
    streaming_cursor_visible: bool,
    /// Typewriter-reveal frontier for the live streaming assistant entry:
    /// `Some((id, n))` means the entry with id `id` has revealed its first
    /// `n` *characters* so far. Transient view state only — deliberately NOT
    /// a `ChatEntry` field, never serialized, and dropped once the entry is
    /// finalized. `None` means nothing is animating (all content shown).
    revealed_chars: Option<(EntryId, usize)>,
    /// True while a reveal is being driven toward the end of a *final*
    /// assistant message (seeded by `Message::AddAssistant`). When the reveal
    /// reaches the content length the entry is auto-finalized: `streaming`
    /// flips to `false`, no dying cursor lingers, and the blink + typing
    /// subscriptions stop. Live streaming entries (seeded by
    /// `update_last_assistant`) keep their window at the content length
    /// instead, matching the pre-autofinish behavior.
    reveal_autofinish: bool,
    /// Per-entry cached parses of assistant markdown, so the 16 ms reveal tick
    /// re-renders an already-parsed event stream (`render_upto`) instead of
    /// re-running pulldown_cmark on the raw text every frame. Transient view
    /// state — never serialized; populated eagerly wherever assistant content
    /// is set and dropped by `trim_entries` when entries are evicted.
    md_docs: HashMap<EntryId, markdown::MarkdownDoc>,
    /// Typewriter-reveal frontier for thinking previews:
    /// `id -> chars revealed`. Removed once the reveal reaches the content
    /// length. Transient view state — never serialized.
    thinking_reveals: HashMap<EntryId, usize>,
    /// Entrance-fade progress for ToolCall / Error / Completion chips:
    /// `id -> tick count since insertion`, capped at `ENTRANCE_TICKS` (then
    /// removed). Transient view state — never serialized.
    entrance_ticks: HashMap<EntryId, u8>,
    /// Free-running 16 ms tick counter driving the subtle shimmer color pulse
    /// on open thinking entries. Transient view state — never serialized.
    shimmer_phase: u32,
    /// Per-call spend records for the active session, rendered in the Spend
    /// Log modal. Populated by `Message::SpendLogsLoaded` (driven from
    /// `App::load_spend_log`); cleared when the active session changes.
    spend_log: Vec<SpendRecord>,
    /// Whether `spend_log` has been loaded at least once for the current
    /// session (distinguishes "not loaded yet" from "loaded, empty").
    spend_log_loaded: bool,
}

const MAX_LIVE_ENTRIES: usize = 1_000;
const MAX_THINKING_CHARS: usize = 64_000;
const MAX_TOOL_DETAIL_CHARS: usize = 256_000;
/// Characters the typewriter reveal moves per `TypingTick`. The 16 ms
/// subscription cadence (`circuit_background::TICK_MS`) turns this into
/// ~500 chars/s — a fast typing feel.
const REVEAL_CHARS_PER_TICK: usize = 8;
/// Characters the thinking-preview reveal moves per `TypingTick`: with the
/// same 16 ms cadence this is ~4000 chars/s — a quick open/expand feel.
const THINKING_REVEAL_CHARS_PER_TICK: usize = 64;
/// Number of `TypingTick`s an entrance fade runs for (~128 ms at 16 ms/tick).
const ENTRANCE_TICKS: u8 = 8;
/// Period (in ticks) of the subtle thinking shimmer pulse: the color
/// interpolates from muted to text over `SHIMMER_PERIOD` ticks and back.
const SHIMMER_PERIOD: u32 = 8;

impl Default for State {
    fn default() -> Self {
        Self::new()
    }
}

impl State {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            input: String::new(),
            next_id: 1,
            sub_view: SubView::Main,
            streaming_cursor_visible: false,
            revealed_chars: None,
            reveal_autofinish: false,
            md_docs: HashMap::new(),
            thinking_reveals: HashMap::new(),
            entrance_ticks: HashMap::new(),
            shimmer_phase: 0,
            spend_log: Vec::new(),
            spend_log_loaded: false,
        }
    }

    /// Reconstruct a `State` from previously persisted entries (per-project
    /// chat transcript). The next entry id continues after the highest id
    /// already present so new entries don't collide with restored ones.
    pub fn from_entries(mut entries: Vec<ChatEntry>) -> Self {
        // A restored transcript cannot contain a genuinely live tool call.
        // Mark interrupted calls neutrally instead of displaying them forever
        // as running after the application is idle.
        for entry in &mut entries {
            if let ChatEntry::ToolCall { status, .. } = entry {
                if matches!(status, ToolCallStatus::Running) {
                    *status = ToolCallStatus::Cancelled;
                }
            }
        }
        if entries.len() > MAX_LIVE_ENTRIES {
            entries.drain(0..entries.len() - MAX_LIVE_ENTRIES);
        }
        // Restored assistant entries are fully rendered (no reveal window);
        // parse them eagerly so per-frame renders hit the cache.
        let mut md_docs = HashMap::new();
        for entry in &entries {
            if let ChatEntry::Assistant { id, content, .. } = entry {
                md_docs.insert(*id, markdown::MarkdownDoc::parse(content));
            }
        }
        let next_id = entries.iter().map(Self::entry_id).max().unwrap_or(0) + 1;
        Self {
            entries,
            input: String::new(),
            next_id,
            sub_view: SubView::Main,
            streaming_cursor_visible: false,
            revealed_chars: None,
            reveal_autofinish: false,
            md_docs,
            thinking_reveals: HashMap::new(),
            entrance_ticks: HashMap::new(),
            shimmer_phase: 0,
            spend_log: Vec::new(),
            spend_log_loaded: false,
        }
    }

    /// Borrow the current transcript (used for persistence).
    pub fn entries(&self) -> &[ChatEntry] {
        &self.entries
    }

    /// Serialize the transcript to `path` (creating parent dirs). Used to
    /// restore the on-screen conversation after an app restart. Writes the
    /// versioned `TranscriptFile` wrapper so the format can evolve without
    /// breaking older transcripts.
    pub fn save_to(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Transcripts must never persist a `streaming: true` assistant entry:
        // streaming is transient view state that drives the typewriter reveal
        // and blinking cursor, both of which are meaningless once the UI is
        // gone. Force every assistant entry to `streaming: false` before
        // writing so restored files are always clean, regardless of when they
        // were saved (restoration replays full content with no reveal anyway).
        let mut entries = self.entries.clone();
        for entry in &mut entries {
            if let ChatEntry::Assistant { streaming, .. } = entry {
                *streaming = false;
            }
        }
        let json =
            serde_json::to_string_pretty(&TranscriptFile { version: TRANSCRIPT_VERSION, entries })
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        std::fs::write(path, json)
    }

    /// Load previously persisted entries from `path`. Returns `None` when the
    /// file does not exist (fresh project / first launch) or cannot be parsed.
    ///
    /// Tries the versioned wrapper first; if that fails, falls back to the
    /// legacy v1 bare-array format (`[{...}]`). Entries missing the timestamp
    /// fields deserialize as `None` via `serde(default)`. The next `save_to`
    /// upgrades a legacy file to the current version.
    ///
    /// Transcripts written by a future version are refused — they are not
    /// silently rewritten or downgraded — and the mismatch is surfaced via a
    /// `tracing::warn!` log so the user has a chance to update the app before
    /// an older binary rewrites the transcript.
    pub fn load_entries(path: &Path) -> Option<Vec<ChatEntry>> {
        let text = std::fs::read_to_string(path).ok()?;
        if let Ok(file) = serde_json::from_str::<TranscriptFile>(&text) {
            if file.version != TRANSCRIPT_VERSION {
                tracing::warn!(
                    version = file.version,
                    supported_version = TRANSCRIPT_VERSION,
                    "chat transcript version unsupported; refusing to load"
                );
                return None;
            }
            return Some(file.entries);
        }
        serde_json::from_str(&text).ok()
    }

    /// Extract the stable id from a chat entry (used to continue id allocation
    /// after restoring a persisted transcript).
    fn entry_id(entry: &ChatEntry) -> EntryId {
        match entry {
            ChatEntry::User { id, .. }
            | ChatEntry::Assistant { id, .. }
            | ChatEntry::Thinking { id, .. }
            | ChatEntry::ToolCall { id, .. }
            | ChatEntry::Completion { id, .. }
            | ChatEntry::Error { id, .. } => *id,
        }
    }

    fn trim_entries(&mut self) {
        if self.entries.len() > MAX_LIVE_ENTRIES {
            self.entries.drain(0..self.entries.len() - MAX_LIVE_ENTRIES);
            // Evicted entries are gone; drop their transient view state too so
            // per-entry caches (markdown docs, reveal/animation maps) can never
            // grow stale or leak ids that no longer exist.
            self.md_docs.clear();
            self.thinking_reveals.clear();
            self.entrance_ticks.clear();
        }
    }

    /// Close the trailing open thinking entry by recording its end timestamp.
    /// No-op when the last entry is not an unfinished `Thinking`; consecutive
    /// thinking stays within one entry, so only the final one is closed here.
    /// Used by every push site: pushing a non-thinking entry ends the active
    /// trailing thinking phase.
    fn finish_open_thinking(&mut self) {
        if let Some(ChatEntry::Thinking { finished_at, .. }) = self.entries.last_mut() {
            if finished_at.is_none() {
                *finished_at = Some(now_rfc3339());
            }
        }
    }

    /// Close every thinking entry that is still open (`finished_at: None`),
    /// including earlier entries that were collapsed and are no longer
    /// trailing. Used only at the true run boundary, where no more thinking
    /// content can arrive.
    fn finish_all_open_thinking(&mut self) {
        for entry in &mut self.entries {
            if let ChatEntry::Thinking { finished_at, .. } = entry {
                if finished_at.is_none() {
                    *finished_at = Some(now_rfc3339());
                }
            }
        }
    }

    /// Get a reference to the current input text.
    pub fn input(&self) -> &str {
        &self.input
    }

    /// Spend records for the active session (Spend Log modal body).
    pub fn spend_log(&self) -> &[SpendRecord] {
        &self.spend_log
    }

    /// Whether the spend log has been loaded at least once for the active
    /// session.
    pub fn spend_log_loaded(&self) -> bool {
        self.spend_log_loaded
    }

    /// Clear the spend log when the active session changes so a resumed
    /// session never shows another session's records.
    pub fn clear_spend_log(&mut self) {
        self.spend_log.clear();
        self.spend_log_loaded = false;
    }

    /// Attach authoritative completion metadata from `AgentOutput`.
    pub fn set_run_completion(
        &mut self,
        multi_agent: bool,
        completed: bool,
        files: Vec<String>,
        project_root: Option<String>,
    ) {
        self.finish_open_thinking();
        let id = self.next_id;
        self.next_id += 1;
        self.entrance_ticks.insert(id, 0);
        self.entries.push(ChatEntry::Completion {
            id,
            summary: RunCompletionSummary {
                multi_agent,
                completed,
                files,
                project_root: project_root.map(|path| display_project_path(&path)),
            },
            created_at: Some(now_rfc3339()),
        });
        self.trim_entries();
    }

    /// Add a thinking entry (deduplicates consecutive thinking).
    pub fn add_thinking(&mut self, content: String) {
        if let Some(ChatEntry::Thinking {
            content: ref mut existing, collapsed: false, id, ..
        }) = self.entries.last_mut()
        {
            existing.push('\n');
            existing.push_str(&content);
            *existing = tail_chars(existing, MAX_THINKING_CHARS);
            // Content grew: keep any in-flight reveal, clamping it to the new
            // length (and finishing it if the tail was truncated into range).
            if let Some(revealed) = self.thinking_reveals.get_mut(id) {
                let len = existing.chars().count();
                *revealed = (*revealed).min(len);
                if *revealed >= len {
                    self.thinking_reveals.remove(id);
                }
            }
            return;
        }
        let id = self.next_id;
        self.next_id += 1;
        let collapsed = content.len() > 500;
        self.entries.push(ChatEntry::Thinking {
            id,
            content,
            collapsed,
            created_at: Some(now_rfc3339()),
            finished_at: None,
        });
        self.thinking_reveals.insert(id, 0);
        self.trim_entries();
    }

    /// Surface a blocking error to the user as a distinct chat entry (e.g. a
    /// dispatch validation failure before a run starts).
    pub fn add_error(&mut self, content: String) {
        self.finish_open_thinking();
        let id = self.next_id;
        self.next_id += 1;
        self.entrance_ticks.insert(id, 0);
        self.entries.push(ChatEntry::Error { id, content, created_at: Some(now_rfc3339()) });
        self.trim_entries();
    }

    /// Add a tool call annotation.
    pub fn add_tool_call(&mut self, tool_name: String, detail: String) {
        self.finish_open_thinking();
        if let Some(ChatEntry::ToolCall {
            tool_name: existing_name,
            detail: existing_detail,
            status: ToolCallStatus::Running,
            ..
        }) = self.entries.last_mut()
        {
            if *existing_name == tool_name {
                if existing_detail.is_empty() && !detail.is_empty() {
                    *existing_detail = detail;
                }
                return;
            }
        }
        let id = self.next_id;
        self.next_id += 1;
        self.entrance_ticks.insert(id, 0);
        self.entries.push(ChatEntry::ToolCall {
            id,
            tool_name,
            detail,
            status: ToolCallStatus::Running,
            created_at: Some(now_rfc3339()),
        });
        self.trim_entries();
    }

    /// Update the most recent matching *running* tool-call entry with its result,
    /// or record the result as a new entry if none is in flight.
    pub fn update_tool_call(&mut self, tool_name: &str, detail: String, success: bool) {
        self.finish_open_thinking();
        if let Some(ChatEntry::ToolCall { tool_name: _name, detail: existing, status, .. }) = self
            .entries
            .iter_mut()
            .rev()
            .find(|e| {
                matches!(e, ChatEntry::ToolCall { status: ToolCallStatus::Running, tool_name: n, .. } if n == tool_name)
            })
        {
            *status = if success { ToolCallStatus::Completed } else { ToolCallStatus::Failed };
            if !detail.is_empty() {
                existing.push_str(&format!("\n{}", detail));
                *existing = tail_chars(existing, MAX_TOOL_DETAIL_CHARS);
            }
            return;
        }
        let id = self.next_id;
        self.next_id += 1;
        self.entrance_ticks.insert(id, 0);
        self.entries.push(ChatEntry::ToolCall {
            id,
            tool_name: tool_name.to_string(),
            detail,
            status: if success { ToolCallStatus::Completed } else { ToolCallStatus::Failed },
            created_at: Some(now_rfc3339()),
        });
        self.trim_entries();
    }

    pub fn append_tool_output(&mut self, chunk: &str, is_stderr: bool) {
        if let Some(ChatEntry::ToolCall { detail, .. }) =
            self.entries.iter_mut().rev().find(|entry| {
                matches!(entry, ChatEntry::ToolCall { status: ToolCallStatus::Running, .. })
            })
        {
            if is_stderr {
                detail.push_str("[stderr] ");
            }
            detail.push_str(chunk);
            *detail = tail_chars(detail, MAX_TOOL_DETAIL_CHARS);
        }
    }

    /// Resolve tool calls that never received a terminal lifecycle event.
    /// This is invoked at the run boundary so the UI cannot remain "running"
    /// after the application has returned to idle.
    pub fn settle_running_tool_calls(&mut self, terminal_status: ToolCallStatus) {
        for entry in &mut self.entries {
            if let ChatEntry::ToolCall { status, .. } = entry {
                if matches!(status, ToolCallStatus::Running) {
                    *status = terminal_status.clone();
                }
            }
        }
    }

    /// Whether any assistant entry is still streaming. Used by `app.rs` to
    /// keep the blinking-cursor subscription alive only while a run is
    /// actually emitting text.
    pub fn is_streaming(&self) -> bool {
        self.entries
            .iter()
            .any(|entry| matches!(entry, ChatEntry::Assistant { streaming: true, .. }))
    }

    /// Whether the typewriter reveal is still in progress: the tracked entry
    /// exists, is still streaming, and has not yet revealed the full content.
    /// Used by `app.rs` to keep the 16 ms `TypingTick` subscription alive only
    /// while a reveal is animating.
    pub fn is_revealing(&self) -> bool {
        // Assistant typewriter-reveal window still animating.
        let assistant_revealing = self.revealed_chars.is_some_and(|(id, revealed)| {
            matches!(
                self.entries.last(),
                Some(ChatEntry::Assistant { id: entry_id, content, streaming: true, .. })
                    if *entry_id == id && revealed < content.chars().count()
            )
        });
        if assistant_revealing {
            return true;
        }
        // Thinking-preview reveal or an entrance fade still animating.
        if !self.thinking_reveals.is_empty() || !self.entrance_ticks.is_empty() {
            return true;
        }
        // An open thinking entry keeps the shimmer driver (and its 16 ms tick)
        // alive until the run boundary stamps it finished.
        self.entries
            .iter()
            .any(|entry| matches!(entry, ChatEntry::Thinking { finished_at: None, .. }))
    }

    /// Advance the typewriter reveal one tick on the live streaming assistant
    /// entry (the tracked entry id must still be the streaming tail). No-op
    /// when no reveal is in progress; resets the window to `None` when the
    /// tracked id no longer matches the live streaming entry, so a stale
    /// window can never drive a completed entry.
    fn advance_reveal(&mut self) {
        let Some((id, revealed)) = self.revealed_chars else {
            return;
        };
        match self.entries.last() {
            Some(ChatEntry::Assistant { id: entry_id, content, streaming: true, .. })
                if *entry_id == id =>
            {
                let len = content.chars().count();
                let next = revealed.saturating_add(REVEAL_CHARS_PER_TICK).min(len);
                if next == len && self.reveal_autofinish {
                    // A *final* message has fully revealed: the text is
                    // complete, so mark the entry non-streaming (dropping the
                    // blinking cursor and both subscriptions) and clear the
                    // window — the reveal is done, not paused mid-content.
                    if let Some(ChatEntry::Assistant { streaming, .. }) = self.entries.last_mut() {
                        *streaming = false;
                    }
                    self.revealed_chars = None;
                    self.reveal_autofinish = false;
                } else {
                    self.revealed_chars = Some((id, next));
                }
            }
            _ => {
                self.revealed_chars = None;
            }
        }
    }

    /// Advance every in-flight thinking reveal one tick (clamped to each
    /// entry's content length), removing entries whose reveal finished.
    fn advance_thinking_reveals(&mut self) {
        // Lengths are snapshotted first: the reveal map is mutated below, so
        // the entries borrow cannot stay live across the updates.
        let lengths: HashMap<EntryId, usize> = self
            .entries
            .iter()
            .filter_map(|entry| match entry {
                ChatEntry::Thinking { id, content, .. } => Some((*id, content.chars().count())),
                _ => None,
            })
            .collect();
        let mut done: Vec<EntryId> = Vec::new();
        for (id, revealed) in self.thinking_reveals.iter_mut() {
            match lengths.get(id).copied() {
                Some(len) => {
                    *revealed = revealed.saturating_add(THINKING_REVEAL_CHARS_PER_TICK).min(len);
                    if *revealed >= len {
                        done.push(*id);
                    }
                }
                None => done.push(*id),
            }
        }
        for id in done {
            self.thinking_reveals.remove(&id);
        }
    }

    /// Advance every pending entrance fade one tick, dropping entries whose
    /// fade has completed.
    fn advance_entrance_ticks(&mut self) {
        self.entrance_ticks.retain(|_, ticks| {
            *ticks = ticks.saturating_add(1);
            *ticks < ENTRANCE_TICKS
        });
    }

    /// Update the latest assistant entry with streaming content.
    pub fn update_last_assistant(&mut self, new_content: String) {
        self.finish_open_thinking();
        // Parse eagerly: every content change invalidates the previous cached
        // document, and rendering reads the cache on every reveal frame.
        let doc = markdown::MarkdownDoc::parse(&new_content);
        let new_len = new_content.chars().count();
        if let Some(ChatEntry::Assistant { id, content, streaming: true, .. }) =
            self.entries.last_mut()
        {
            // The typewriter reveal window belongs to this live entry; keep it
            // where it is because streaming content only grows. If content ever
            // shrank (it should not), clamp the window so it cannot point past
            // the end of the text.
            if let Some((wide_id, revealed)) = self.revealed_chars {
                self.revealed_chars = Some((wide_id, revealed.min(new_len)));
            }
            let entry_id = *id;
            *content = new_content;
            self.md_docs.insert(entry_id, doc);
        } else {
            let id = self.next_id;
            self.next_id += 1;
            self.entries.push(ChatEntry::Assistant {
                id,
                content: new_content,
                streaming: true,
                created_at: Some(now_rfc3339()),
            });
            // A brand-new streaming entry starts its reveal from scratch. Live
            // entries hold their window at the content length when done rather
            // than auto-finalizing (`reveal_autofinish` stays false).
            self.revealed_chars = Some((id, 0));
            self.reveal_autofinish = false;
            self.md_docs.insert(id, doc);
            self.trim_entries();
        }
    }

    /// Finalize streaming: mark the last assistant entry as non-streaming.
    /// This is used at mid-run boundaries (e.g. navigating away from the chat
    /// page) where the run may still be emitting `AgentThoughts`. Thinking
    /// entries are deliberately left open so their recorded duration spans
    /// the whole run; use [`finalize_run`](Self::finalize_run) at the true
    /// run boundary instead.
    pub fn finalize_streaming(&mut self) {
        if let Some(ChatEntry::Assistant { streaming, .. }) = self.entries.last_mut() {
            *streaming = false;
        }
        // The reveal window is transient; once the entry is finalized the
        // full text renders and there is nothing left to drive.
        self.revealed_chars = None;
        self.reveal_autofinish = false;
    }

    /// Finalize a run: mark the last assistant entry as non-streaming (same
    /// as [`finalize_streaming`](Self::finalize_streaming)) AND stamp the end
    /// timestamp on every thinking entry still open. Safe only at the true
    /// run boundary (run completed or cancelled) because no further thinking
    /// content can arrive afterwards.
    pub fn finalize_run(&mut self) {
        self.finish_all_open_thinking();
        if let Some(ChatEntry::Assistant { streaming, .. }) = self.entries.last_mut() {
            *streaming = false;
        }
        // Full text is final after a run boundary — stop any reveal window.
        self.revealed_chars = None;
        self.reveal_autofinish = false;
    }

    pub fn update(&mut self, message: Message) -> iced::Task<Message> {
        match message {
            Message::InputChanged(s) => {
                self.input = s;
            }
            Message::SubmitInput => {
                let trimmed = self.input.trim().to_string();
                if !trimmed.is_empty() {
                    self.finish_open_thinking();
                    let id = self.next_id;
                    self.next_id += 1;
                    self.entries.push(ChatEntry::User {
                        id,
                        content: trimmed,
                        created_at: Some(now_rfc3339()),
                    });
                    self.input.clear();
                }
            }
            Message::AddUser(s) => {
                self.finish_open_thinking();
                let id = self.next_id;
                self.next_id += 1;
                self.entries.push(ChatEntry::User {
                    id,
                    content: s,
                    created_at: Some(now_rfc3339()),
                });
            }
            Message::AddAssistant(s) => {
                self.finish_open_thinking();
                let id = self.next_id;
                self.next_id += 1;
                // Parse eagerly so the reveal re-renders a cached document on
                // every tick instead of re-running the markdown parser.
                let doc = markdown::MarkdownDoc::parse(&s);
                if !s.is_empty() {
                    // The final message enters with a typewriter reveal; once
                    // it reaches the end (`reveal_autofinish` + full window)
                    // the entry auto-finalizes itself.
                    self.revealed_chars = Some((id, 0));
                    self.reveal_autofinish = true;
                }
                // An empty final message must never linger as a `streaming`
                // entry: without a reveal window it would blink a cursor and
                // keep the blink subscription alive at idle forever.
                let streaming = !s.is_empty();
                self.entries.push(ChatEntry::Assistant {
                    id,
                    content: s,
                    streaming,
                    created_at: Some(now_rfc3339()),
                });
                self.md_docs.insert(id, doc);
            }
            Message::AddThinking(s) => {
                self.add_thinking(s);
            }
            Message::AddToolCall(s) => {
                self.add_tool_call(s, String::new());
            }
            Message::StreamingTick => {
                self.streaming_cursor_visible = !self.streaming_cursor_visible;
            }
            Message::TypingTick => {
                self.advance_reveal();
                self.advance_thinking_reveals();
                self.advance_entrance_ticks();
                self.shimmer_phase = self.shimmer_phase.wrapping_add(1);
            }
            Message::UsePrompt(prompt) => {
                self.input = prompt;
            }
            Message::SelectSession(_) => {
                // Handled by App because session loading requires shared services.
            }
            Message::ToggleEntry(id) => {
                if let Some(ChatEntry::Thinking { ref mut collapsed, .. }) =
                    self.entries.iter_mut().find(|e| match e {
                        ChatEntry::Thinking { id: eid, .. } => *eid == id,
                        _ => false,
                    })
                {
                    *collapsed = !*collapsed;
                }
            }
            Message::CopyCode(_) => {
                // Handled via clipboard integration in the code_block widget
            }
            Message::ToggleMultiAgent => {}
            Message::ToggleFastMode => {}
            Message::NavigateToDiff => {}
            Message::NavigateToSettings => {}
            Message::NavigateToStudio => {}
            Message::NavigateToToolLog(_) => {}
            Message::SetActiveModel(_) => {
                // Handled by the App level — just a passthrough
            }
            Message::NewSession => {
                // Handled by the App level — chat state is reset there
            }
            Message::SetSubView(_) => {
                // Handled by the App level — sets self.chat.sub_view
            }
            Message::SpendLogsLoaded(records) => {
                self.spend_log = records;
                self.spend_log_loaded = true;
            }
            Message::RefreshSpendLog => {
                // Handled by the App level — reloads via the session handler
            }
        }
        self.trim_entries();
        iced::Task::none()
    }

    fn render_entry<'a>(
        &'a self,
        entry: &'a ChatEntry,
        palette: &'a crate::theme::Palette,
    ) -> Element<'a, Message> {
        match entry {
            ChatEntry::User { content, created_at, .. } => {
                let bubble = container(text(content).size(14).color(palette.primary_text))
                    .padding(10)
                    .style(move |_theme: &iced::Theme| container::Style {
                        background: Some(Background::Color(palette.primary)),
                        border: Border { radius: Radius::from(12.0), ..Default::default() },
                        ..container::Style::default()
                    });
                // Compact timestamp below the bubble (absent for legacy
                // v1 entries that carry no `created_at`).
                let mut content_col = column![bubble].spacing(4);
                if let Some(ts_line) = compact_timestamp_line(created_at, palette) {
                    content_col = content_col.push(ts_line);
                }
                row![container(content_col).width(Length::Fill).padding(4)]
                    .spacing(4)
                    .padding(4)
                    .into()
            }
            ChatEntry::Assistant { id, content, streaming, created_at, .. } => {
                let char_count = content.chars().count();
                // Typewriter reveal frontier, in *characters*: while a fresh
                // streaming entry owns a reveal window, render the cached
                // markdown up to that budget so the message grows live. Any
                // other state renders the full content exactly as before.
                let reveal_window = match self.revealed_chars {
                    Some((rid, n)) if rid == *id && *streaming && n < char_count => Some(n),
                    _ => None,
                };
                // Render from the cached parse (always populated for assistant
                // entries); the budget maps the reveal frontier onto the
                // document's visible-text units so every tick re-drives an
                // already-parsed event stream instead of re-parsing markdown.
                // The raw `markdown::render` fallback is purely defensive.
                let md: Element<'_, Message> = match self.md_docs.get(id) {
                    Some(doc) => {
                        let budget = reveal_window.map(|n| n.min(doc.total_units));
                        doc.render_upto(
                            budget,
                            |code| Message::CopyCode(code.to_string()),
                            palette.surface_variant,
                            palette.text_muted,
                            palette.primary,
                        )
                    }
                    None => markdown::render(
                        content,
                        |code| Message::CopyCode(code.to_string()),
                        palette.surface_variant,
                        palette.text_muted,
                        palette.primary,
                    ),
                };
                // Blinking cursor on the live entry, as before — suppressed
                // once the reveal window has fully consumed the content, so it
                // never sits after the complete message.
                let reveal_complete =
                    self.revealed_chars.is_some_and(|(rid, n)| rid == *id && n >= char_count);
                let body: Element<'a, Message> =
                    if *streaming && self.streaming_cursor_visible && !reveal_complete {
                        row![md, text("▌").size(14).color(palette.text_muted)].spacing(2).into()
                    } else {
                        md
                    };
                // Compact timestamp below the assistant message block. Shown
                // immediately for a streaming entry (timestamped at its first
                // chunk), not hidden while the reveal is running.
                let mut block = column![body].spacing(4).width(Length::Fill);
                if let Some(ts_line) = compact_timestamp_line(created_at, palette) {
                    block = block.push(ts_line);
                }
                block.into()
            }
            ChatEntry::Thinking { content, collapsed, id, created_at, finished_at, .. } => {
                let char_count = content.chars().count();
                // Thinking preview typewriter-reveals while content arrives,
                // then shows the full text once the reveal catches up.
                // (Collapsed entries never reveal — they show the stub line.)
                let revealed = self.thinking_reveals.get(id).copied().unwrap_or(char_count);
                let preview = if *collapsed {
                    format!("Iteration details hidden ({char_count} chars)")
                } else {
                    content.chars().take(revealed).collect::<String>()
                };
                // Subtle shimmer while the thinking phase is still open: the
                // preview color pulses between palette tokens (never
                // hard-coded RGB). Driven by the `TypingTick` shimmer phase.
                let preview_color = if finished_at.is_none() {
                    let phase = self.shimmer_phase % (2 * SHIMMER_PERIOD);
                    let wave = if phase < SHIMMER_PERIOD {
                        phase as f32 / SHIMMER_PERIOD as f32
                    } else {
                        (2 * SHIMMER_PERIOD - phase) as f32 / SHIMMER_PERIOD as f32
                    };
                    lerp_color(palette.text_muted, palette.text, 0.5 + 0.5 * wave)
                } else {
                    palette.text_muted
                };
                let label = if *collapsed { "▶" } else { "▼" };
                let toggle_btn = button(text(label).size(11))
                    .style(crate::ui::button::secondary)
                    .on_press(Message::ToggleEntry(*id));
                // Show the elapsed thinking time only when both timestamps are
                // present and parse; otherwise the row stays unchanged.
                let duration_label: Option<Element<'_, Message>> =
                    match (created_at.as_deref(), finished_at.as_deref()) {
                        (Some(start), Some(end)) => elapsed_seconds(start, end).map(|secs| {
                            text(format!("⏱ {}s", secs)).size(11).color(palette.text_muted).into()
                        }),
                        _ => None,
                    };
                let mut preview_row =
                    row![toggle_btn, text(preview).size(13).color(preview_color),].spacing(4);
                if let Some(label) = duration_label {
                    preview_row = preview_row.push(label);
                }
                container(preview_row)
                    .padding(8)
                    .style(|_theme| container::Style {
                        background: Some(Background::Color(palette.surface_variant)),
                        border: Border { radius: Radius::from(8.0), ..Default::default() },
                        ..container::Style::default()
                    })
                    .into()
            }
            ChatEntry::ToolCall { id, tool_name, detail, status, .. } => {
                let fade = entrance_alpha(self.entrance_ticks.get(id).copied());
                let (icon, clr) = match status {
                    ToolCallStatus::Running => ("⟳", palette.warning),
                    ToolCallStatus::Completed | ToolCallStatus::Allowed => ("✓", palette.success),
                    ToolCallStatus::Failed | ToolCallStatus::Denied => ("✗", palette.danger),
                    ToolCallStatus::Cancelled => ("−", palette.text_muted),
                };
                let clr = with_alpha(clr, fade);
                let muted = with_alpha(palette.text_muted, fade);
                let label = if detail.is_empty() {
                    format!("[Tool] {}", tool_name)
                } else {
                    format!("[Tool] {} — {}", tool_name, detail)
                };
                button(
                    container(
                        row![
                            text(icon).size(13).color(clr),
                            text(label).size(13).color(muted),
                            text(status.to_string()).size(11).color(clr),
                        ]
                        .spacing(6),
                    )
                    .padding(8)
                    .style(move |_theme| container::Style {
                        background: Some(Background::Color(with_alpha(
                            palette.surface_variant,
                            fade,
                        ))),
                        border: Border { radius: Radius::from(4.0), ..Default::default() },
                        ..container::Style::default()
                    }),
                )
                .on_press(Message::NavigateToToolLog(tool_name.clone()))
                .style(|_theme, _status| button::Style {
                    background: None,
                    ..button::Style::default()
                })
                .into()
            }
            ChatEntry::Completion { id, summary, .. } => self.completion_card(
                summary,
                palette,
                entrance_alpha(self.entrance_ticks.get(id).copied()),
            ),
            ChatEntry::Error { id, content, .. } => {
                let fade = entrance_alpha(self.entrance_ticks.get(id).copied());
                container(text(content).size(13).color(with_alpha(palette.danger, fade)))
                    .padding(10)
                    .style(move |_theme| container::Style {
                        background: Some(Background::Color(with_alpha(
                            palette.surface_variant,
                            fade,
                        ))),
                        border: Border { radius: Radius::from(8.0), ..Default::default() },
                        ..container::Style::default()
                    })
                    .into()
            }
        }
    }

    fn empty_session_view<'a>(
        &'a self,
        theme: &'a AppTheme,
        has_providers: bool,
    ) -> Element<'a, Message> {
        let palette = &theme.palette;
        let hero: Element<'a, Message> = if has_providers {
            let quick_actions = column![
                row![
                    button(text("⚙  Fix a bug").size(13))
                        .style(button::secondary)
                        .padding([10, 16])
                        .width(Length::Fill)
                        .on_press(Message::UsePrompt("Fix this bug: ".into())),
                    button(text("＋  Add a feature").size(13))
                        .style(button::secondary)
                        .padding([10, 16])
                        .width(Length::Fill)
                        .on_press(Message::UsePrompt("Add this feature: ".into())),
                ]
                .spacing(8),
                row![
                    button(text("▤  Explain a file").size(13))
                        .style(button::secondary)
                        .padding([10, 16])
                        .width(Length::Fill)
                        .on_press(Message::UsePrompt("Explain this file: ".into())),
                    button(text("✣  From scratch").size(13))
                        .style(button::secondary)
                        .padding([10, 16])
                        .width(Length::Fill)
                        .on_press(Message::UsePrompt("Build this from scratch: ".into())),
                ]
                .spacing(8),
            ]
            .spacing(8)
            .width(Length::Fill);

            container(
                column![
                    text("✦").size(28).color(palette.text_muted),
                    text("Start building").size(18).color(palette.text),
                    text("Describe what you want built, or start from a quick action.")
                        .size(14)
                        .color(palette.text_muted),
                    quick_actions,
                ]
                .spacing(10)
                .align_x(Alignment::Center),
            )
            .padding([34, 24])
            .width(Length::Fill)
            .style(move |_| crate::theme::card_style(palette))
            .into()
        } else {
            container(
                column![
                    text("Concerto").size(42).color(theme.palette.accent),
                    text("Orchestrate Intelligence").size(18).color(palette.text_muted),
                    button(text("Open Settings → Configure Provider"))
                        .style(crate::ui::button::primary)
                        .padding(16)
                        .on_press(Message::NavigateToSettings),
                ]
                .align_x(Alignment::Center)
                .spacing(16),
            )
            .padding(64)
            .style(move |_| crate::theme::card_style(palette))
            .width(Length::Fill)
            .height(Length::Fill)
            .center_x(Length::Fill)
            .center_y(Length::Fill)
            .into()
        };

        container(hero).padding(12).width(Length::Fill).into()
    }

    fn orchestration_timeline<'a>(
        &'a self,
        graph: &'a agent_graph::State,
        palette: &'a crate::theme::Palette,
        run_entries: &'a [ChatEntry],
    ) -> Element<'a, Message> {
        let mut phases = column![].spacing(0).width(Length::Fill);
        // Tool events currently carry no role/task id. Render the shared trail
        // once under the newest Coder phase instead of duplicating it for every
        // repair or follow-up Coder subtask.
        let tool_owner = graph
            .model
            .nodes
            .iter()
            .rev()
            .find(|node| node.role == AgentId::new("coder"))
            .map(|node| node.id);
        for node in &graph.model.nodes {
            let (icon, color) = match node.state {
                NodeState::Completed => ("✓", palette.success),
                NodeState::Failed | NodeState::Cancelled | NodeState::Blocked => {
                    ("✕", palette.danger)
                }
                NodeState::NeedsRevision => ("⟳", palette.warning),
                NodeState::Active | NodeState::WaitingForApproval => ("●", palette.warning),
                NodeState::Idle | NodeState::Queued => ("○", palette.text_muted),
            };
            let role = format!("{:?}", node.role);
            let fallback =
                node.label.split_once(':').map(|(_, detail)| detail.trim()).unwrap_or("");
            let detail = if node.task_summary.is_empty() {
                fallback.to_string()
            } else {
                node.task_summary.clone()
            };
            let state = match node.state {
                NodeState::Completed => "Completed",
                NodeState::NeedsRevision => "Needs revision",
                NodeState::Blocked => "Blocked",
                NodeState::Failed => "Failed",
                NodeState::Cancelled => "Cancelled",
                NodeState::Active => "Running",
                NodeState::WaitingForApproval => "Approval needed",
                NodeState::Idle => "Idle",
                NodeState::Queued => "Queued",
            };
            let header = row![
                text(icon).size(15).color(color),
                column![
                    text(role).size(14).color(palette.text),
                    text(detail).size(12).color(palette.text_muted),
                ]
                .spacing(1)
                .width(Length::Fill),
                text(state).size(11).color(color),
            ]
            .spacing(10)
            .align_y(Alignment::Center);
            phases = phases.push(container(header).padding([10, 2]).width(Length::Fill));

            if tool_owner == Some(node.id) {
                for entry in run_entries {
                    if let ChatEntry::ToolCall { tool_name, detail, status, .. } = entry {
                        let (tool_icon, tool_color) = match status {
                            ToolCallStatus::Running => ("○", palette.warning),
                            ToolCallStatus::Completed | ToolCallStatus::Allowed => {
                                ("▣", palette.text_muted)
                            }
                            ToolCallStatus::Failed | ToolCallStatus::Denied => {
                                ("△", palette.danger)
                            }
                            ToolCallStatus::Cancelled => ("−", palette.text_muted),
                        };
                        let first_line = detail.lines().next().unwrap_or("");
                        let compact_detail: String = first_line.chars().take(100).collect();
                        let label = if compact_detail.is_empty() {
                            tool_name.clone()
                        } else {
                            format!("{} · {}", tool_name, compact_detail)
                        };
                        phases = phases.push(
                            row![
                                text(tool_icon).size(12).color(tool_color),
                                text(label).size(12).color(tool_color),
                            ]
                            .spacing(8)
                            .padding([3, 30]),
                        );
                    }
                }
            }
            phases = phases.push(iced::widget::rule::horizontal(1));
        }
        container(phases).padding([2, 8]).width(Length::Fill).into()
    }

    fn completion_card<'a>(
        &'a self,
        summary: &'a RunCompletionSummary,
        palette: &'a crate::theme::Palette,
        settle_alpha: f32,
    ) -> Element<'a, Message> {
        // Entrance fade ("settle"): modulate the heading color and card
        // background alpha; everything else derives from the palette.
        let color = with_alpha(
            if summary.completed { palette.success } else { palette.warning },
            settle_alpha,
        );
        let heading = if summary.completed { "Completed" } else { "Partial result preserved" };
        let icon = if summary.completed { "✓" } else { "!" };
        let mut file_chips: Vec<Element<'a, Message>> = if summary.files.is_empty() {
            vec![text("No files changed").size(12).color(palette.text_muted).into()]
        } else {
            summary
                .files
                .iter()
                .take(4)
                .map(|file| {
                    container(text(file).size(12))
                        .padding([3, 9])
                        .style(move |_theme| container::Style {
                            background: Some(Background::Color(palette.surface_variant)),
                            border: Border {
                                color: palette.border,
                                width: 1.0,
                                radius: Radius::from(6.0),
                            },
                            ..container::Style::default()
                        })
                        .into()
                })
                .collect()
        };
        if summary.files.len() > 4 {
            file_chips.push(
                text(format!("+{} more", summary.files.len() - 4))
                    .size(12)
                    .color(palette.text_muted)
                    .into(),
            );
        }
        let project = summary.project_root.as_deref().unwrap_or("Current project");
        let run_kind = if summary.multi_agent { "Multi-agent orchestration" } else { "Agent run" };

        // Show a "Review changes" button when files were modified
        let review_btn: Element<'a, Message> = if summary.files.is_empty() {
            container(text("").height(0)).into()
        } else {
            button(
                container(
                    row![
                        text("◇").size(13),
                        text("Review changes").size(13).color(palette.primary_text),
                    ]
                    .spacing(6)
                    .align_y(Alignment::Center),
                )
                .padding([6, 16]),
            )
            .style(button::primary)
            .on_press(Message::NavigateToDiff)
            .into()
        };

        container(
            column![
                row![text(icon).size(16).color(color), text(heading).size(15)].spacing(9),
                row(file_chips).spacing(8),
                text(format!("{} · {}", run_kind, project)).size(12).color(palette.text_muted),
                review_btn,
            ]
            .spacing(10),
        )
        .padding([16, 18])
        .width(Length::Fill)
        .style(move |_theme| container::Style {
            background: Some(Background::Color(with_alpha(palette.surface, settle_alpha))),
            border: Border { radius: Radius::from(12.0), ..Default::default() },
            ..container::Style::default()
        })
        .into()
    }

    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::too_many_arguments)]
    pub fn view<'a>(
        &'a self,
        theme: &'a AppTheme,
        multi_agent: bool,
        fast: bool,
        active_model: &'a str,
        model_names: &'a [String],
        model_source: &'a str,
        agent_graph: &'a agent_graph::State,
        has_agent_assignments: bool,
    ) -> Element<'a, Message> {
        let palette = &theme.palette;

        // Empty state
        if self.entries.is_empty() {
            let has_providers = !model_names.is_empty();
            return iced::widget::column![
                scrollable(self.empty_session_view(theme, has_providers)).height(Length::Fill),
                input_bar(
                    &self.input,
                    palette,
                    multi_agent,
                    fast,
                    active_model,
                    model_names,
                    model_source,
                    has_agent_assignments,
                    false,
                ),
            ]
            .into();
        }

        // Message list
        let mut col = column![].spacing(6).padding(8);
        let has_timeline =
            agent_graph.has_multi_agent_activity && !agent_graph.model.nodes.is_empty();
        let latest_user_index =
            self.entries.iter().rposition(|entry| matches!(entry, ChatEntry::User { .. }));
        if has_timeline && latest_user_index.is_none() {
            col = col.push(self.orchestration_timeline(agent_graph, palette, &self.entries));
        }
        for (index, entry) in self.entries.iter().enumerate() {
            col = col.push(self.render_entry(entry, palette));
            if has_timeline && latest_user_index == Some(index) {
                col = col.push(self.orchestration_timeline(
                    agent_graph,
                    palette,
                    &self.entries[index + 1..],
                ));
            }
        }
        let messages = scrollable(col).anchor_bottom().height(Length::Fill).width(Length::Fill);

        // Compose with input bar at the bottom
        column![
            messages,
            input_bar(
                &self.input,
                palette,
                multi_agent,
                fast,
                active_model,
                model_names,
                model_source,
                has_agent_assignments,
                true,
            )
        ]
        .into()
    }
}

/// Height (logical px) of the Spend Log modal's record list so the centered
/// max-width card stays bounded and the list actually scrolls.
const SPEND_LOG_LIST_HEIGHT: f32 = 360.0;

/// Render the Spend Log modal body: a session-totals header (with a refresh
/// button), the scrollable per-call record list, and the daily-total stub
/// row. The overlay chrome in `app.rs` supplies the card + title + close
/// button; this function fills the body, mirroring the Tool Log modal.
pub fn spend_log_view<'a>(
    spend_log: &'a [SpendRecord],
    daily_cost: Option<f64>,
    cap: Option<f64>,
    cap_state: &'a CapUiState,
    theme: &'a AppTheme,
) -> Element<'a, Message> {
    let palette = &theme.palette;
    let caption = theme.type_scale.caption;

    let totals = spend_totals(spend_log);
    let body: Element<'_, Message> = if spend_log.is_empty() {
        // Bounded box (same height as the record list) so the Fill-based
        // empty-state layout resolves inside the natural-height modal card.
        container(crate::ui::empty_state(
            theme,
            "◷",
            "No spend records yet",
            "Per-call spend appears here once provider calls settle.",
            None::<(String, Message)>,
        ))
        .width(Length::Fill)
        .height(Length::Fixed(SPEND_LOG_LIST_HEIGHT))
        .into()
    } else {
        let rows: Vec<Element<'_, Message>> =
            spend_log.iter().map(|record| spend_log_row(record, palette, caption)).collect();
        scrollable(iced::widget::Column::with_children(rows).spacing(2))
            .height(Length::Fixed(SPEND_LOG_LIST_HEIGHT))
            .into()
    };

    column![
        spend_log_header(totals, cap, cap_state, theme),
        body,
        rule::horizontal(1),
        spend_log_daily_row(daily_cost, palette, caption),
    ]
    .spacing(6)
    .padding(10)
    .into()
}

/// Header row of the Spend Log modal: session totals, cap status text and a
/// refresh button that re-runs `App::load_spend_log`.
fn spend_log_header<'a>(
    totals: SpendTotals,
    cap: Option<f64>,
    cap_state: &'a CapUiState,
    theme: &'a AppTheme,
) -> Element<'a, Message> {
    let palette = &theme.palette;
    let caption = theme.type_scale.caption;
    let status_color = match cap_state {
        CapUiState::Exceeded { .. } => palette.danger,
        CapUiState::Approaching { .. } => palette.warning,
        CapUiState::Normal => palette.text_muted,
    };
    let refresh_btn = button(text("⟳").size(14))
        .style(crate::ui::button::secondary)
        .on_press(Message::RefreshSpendLog);

    row![
        text(format!("Total: ${:.3}", totals.total_cost_usd)).size(14),
        text(format!("Tokens in: {}", totals.tokens_in)).size(caption).color(palette.text_muted),
        text(format!("Tokens out: {}", totals.tokens_out)).size(caption).color(palette.text_muted),
        text(format!("Records: {}", totals.record_count)).size(caption).color(palette.text_muted),
        text(cap_status_text(cap_state, cap)).size(caption).color(status_color),
        iced::widget::space::horizontal(),
        refresh_btn,
    ]
    .spacing(10)
    .align_y(Alignment::Center)
    .into()
}

/// One spend-record row: compact timestamp, provider/model, token counts and
/// cost.
fn spend_log_row<'a>(
    record: &'a SpendRecord,
    palette: &'a crate::theme::Palette,
    caption: f32,
) -> Element<'a, Message> {
    row![
        text(compact_created_at(record.created_at))
            .size(11)
            .color(palette.text_muted)
            .width(Length::Fixed(96.0)),
        text(format!("{} / {}", record.provider, record.model)).size(12).width(Length::Fill),
        text(format!("↑{} ↓{}", record.tokens_in, record.tokens_out))
            .size(caption)
            .color(palette.text_muted)
            .width(Length::Fixed(130.0)),
        text(format!("${:.3}", record.cost_usd)).size(12).width(Length::Fixed(80.0)),
    ]
    .spacing(8)
    .padding(4)
    .into()
}

/// Daily-total stub row. Daily spend tracking is not yet enabled (issue #93
/// Phase 4): `App.daily_cost` is always `None` today, so this renders the
/// placeholder "— (daily tracking not yet enabled)".
fn spend_log_daily_row<'a>(
    daily_cost: Option<f64>,
    palette: &'a crate::theme::Palette,
    caption: f32,
) -> Element<'a, Message> {
    let label = match daily_cost {
        Some(cost) => format!("Daily total: ${cost:.3}"),
        None => "Daily total: — (daily tracking not yet enabled)".to_string(),
    };
    text(label).size(caption).color(palette.text_muted).into()
}

fn tail_chars(value: &str, max_chars: usize) -> String {
    let count = value.chars().count();
    if count <= max_chars {
        return value.to_string();
    }
    let skipped = count - max_chars;
    format!(
        "[older live output omitted; full output remains in session events]\n{}",
        value.chars().skip(skipped).collect::<String>()
    )
}

/// Linear interpolation between two palette colors: `t = 0` yields `a`,
/// `t = 1` yields `b`. Used only to pulse between palette tokens for the
/// thinking shimmer — never to invent a hard-coded RGB value.
fn lerp_color(a: Color, b: Color, t: f32) -> Color {
    let t = t.clamp(0.0, 1.0);
    Color {
        r: a.r + (b.r - a.r) * t,
        g: a.g + (b.g - a.g) * t,
        b: a.b + (b.b - a.b) * t,
        a: a.a + (b.a - a.a) * t,
    }
}

/// Scale a palette color's alpha by `alpha` (clamped to 0..=1). Entrance
/// fades modulate transparency only — the color itself always comes from the
/// palette.
fn with_alpha(color: Color, alpha: f32) -> Color {
    let mut color = color;
    color.a *= alpha.clamp(0.0, 1.0);
    color
}

/// Fractional opacity for an entry's entrance fade: starts near `1/ENTRANCE_TICKS`
/// on insertion and reaches fully opaque once `ENTRANCE_TICKS` ticks elapse
/// (the entry is then dropped from `entrance_ticks`). Absent ticks mean the
/// fade finished — fully opaque.
fn entrance_alpha(ticks: Option<u8>) -> f32 {
    match ticks {
        Some(t) => (t as f32 + 1.0) / ENTRANCE_TICKS as f32,
        None => 1.0,
    }
}

#[allow(clippy::too_many_arguments)]
fn input_bar<'a>(
    input: &'a str,
    palette: &'a crate::theme::Palette,
    multi_agent: bool,
    fast: bool,
    active_model: &'a str,
    model_names: &'a [String],
    model_source: &'a str,
    has_agent_assignments: bool,
    has_entries: bool,
) -> Element<'a, Message> {
    let txt = text_input("Type a message...", input)
        .on_input(Message::InputChanged)
        .on_submit(Message::SubmitInput)
        .width(Length::Fill)
        .padding(10);

    let send = button(text("Send").size(14))
        .style(crate::ui::button::primary)
        .on_press(Message::SubmitInput);

    // New Session button — only shown when there are entries to clear
    let new_session_btn: Element<'_, Message> = if has_entries {
        button(text("New").size(13))
            .style(crate::ui::button::secondary)
            .on_press(Message::NewSession)
            .into()
    } else {
        container(text("").height(0)).into()
    };

    let selected_model = model_names.iter().find(|model| model.as_str() == active_model);
    let model_picker: Element<'_, Message> = if model_names.is_empty() {
        container(text("").height(0)).into()
    } else {
        let picker =
            pick_list(model_names, selected_model, |model| Message::SetActiveModel(model.clone()))
                .padding(2)
                .width(Length::Shrink);
        if !model_source.is_empty() {
            column![picker, text(model_source).size(9).color(palette.text_muted),]
                .spacing(1)
                .align_x(Alignment::Center)
                .into()
        } else {
            picker.into()
        }
    };

    // Multi-agent toggle with label and tooltip
    let toggle_group: Element<'a, Message> = {
        let label = text("Multi").size(12).color(palette.text_muted);
        let tgl = toggler(multi_agent).on_toggle(|_| Message::ToggleMultiAgent).spacing(4).size(20);
        let inner = row![label, tgl].spacing(4).align_y(iced::Alignment::Center);
        tooltip::Tooltip::new(
            inner,
            container(
                text("Route tasks to specialist agents. Configure agents in Studio.").size(12),
            )
            .padding(8),
            tooltip::Position::Top,
        )
        .gap(4)
        .into()
    };

    // Fast-mode toggle with label and tooltip (mirrors the multi-agent
    // toggler; mirrors CLI `-f/--fast` — skip project memory retrieval).
    let fast_group: Element<'a, Message> = {
        let label = text("Fast").size(12).color(palette.text_muted);
        let tgl = toggler(fast).on_toggle(|_| Message::ToggleFastMode).spacing(4).size(20);
        let inner = row![label, tgl].spacing(4).align_y(iced::Alignment::Center);
        tooltip::Tooltip::new(
            inner,
            container(text("Fast mode: skip project memory retrieval (like CLI --fast).").size(12))
                .padding(8),
            tooltip::Position::Top,
        )
        .gap(4)
        .into()
    };

    // Guidance hint when multi-agent is ON but no agents are configured
    let setup_hint: Element<'a, Message> = if multi_agent && !has_agent_assignments {
        container(
            button(
                row![
                    text("⚠").size(11),
                    text("Configure agents in Studio →").size(12).color(palette.primary),
                ]
                .spacing(4)
                .align_y(Alignment::Center),
            )
            .style(button::text)
            .on_press(Message::NavigateToStudio),
        )
        .padding([2, 8])
        .width(Length::Fill)
        .into()
    } else {
        container(text("").height(0)).into()
    };

    let bar = row![new_session_btn, model_picker, txt, send, toggle_group, fast_group,]
        .spacing(8)
        .padding(8)
        .align_y(iced::Alignment::Center);

    container(column![setup_hint, bar].spacing(2))
        .width(Length::Fill)
        .style(move |_theme: &iced::Theme| container::Style {
            background: Some(Background::Color(palette.surface)),
            border: Border { radius: Radius::from(0.0), ..Default::default() },
            ..container::Style::default()
        })
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_thinking_appends_to_existing_thinking() {
        let mut state = State::new();
        state.add_thinking("First".to_string());
        state.add_thinking("Second".to_string());
        assert_eq!(state.entries.len(), 1);
        match &state.entries[0] {
            ChatEntry::Thinking { content, .. } => {
                assert_eq!(content, "First\nSecond");
            }
            _ => panic!("Expected a Thinking entry"),
        }
    }

    #[test]
    fn transcript_round_trips_to_disk() {
        let mut state = State::new();
        let _ = state.update(Message::AddUser("what is 2+2?".to_string()));
        let _ = state.update(Message::AddAssistant("4".to_string()));
        state.add_tool_call("write_file".to_string(), "src/main.rs".to_string());
        let _ = state.update(Message::AddThinking("planning...".to_string()));
        state.set_run_completion(true, true, vec!["src/main.rs".into()], Some("project".into()));

        let path = std::env::temp_dir()
            .join(format!("concerto_chat_transcript_test_{}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);

        state.save_to(&path).expect("save transcript");
        let loaded = State::load_entries(&path).expect("load transcript");
        assert_eq!(loaded.len(), 5, "all entries should round-trip");

        let restored = State::from_entries(loaded);
        assert_eq!(restored.entries().len(), 5);
        assert!(matches!(restored.entries().last(), Some(ChatEntry::Completion { .. })));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn finalize_streaming_marks_the_latest_assistant_complete() {
        let mut state = State::new();
        state.update_last_assistant("partial".to_string());
        state.finalize_streaming();

        assert!(matches!(
            state.entries().last(),
            Some(ChatEntry::Assistant { streaming: false, .. })
        ));
    }

    #[test]
    fn streaming_cursor_toggles_and_reflects_streaming() {
        let mut state = State::new();
        assert!(!state.is_streaming());
        assert!(!state.streaming_cursor_visible);

        // `update_last_assistant` is the live-streaming path (it stamps the
        // entry with `streaming: true`); `AddAssistant` now drives the final
        // message through the typewriter reveal and therefore also marks its
        // entry streaming (it auto-finalizes once the reveal completes).
        state.update_last_assistant("hi".to_string());
        assert!(state.is_streaming());
        assert!(
            !state.streaming_cursor_visible,
            "cursor starts hidden; StreamingTick drives the blink"
        );

        // Each StreamingTick flips the cursor visibility (500 ms cadence
        // from the app.rs subscription while streaming is active).
        let _ = state.update(Message::StreamingTick);
        assert!(state.streaming_cursor_visible);
        let _ = state.update(Message::StreamingTick);
        assert!(!state.streaming_cursor_visible);
        let _ = state.update(Message::StreamingTick);
        assert!(state.streaming_cursor_visible);
    }

    #[test]
    fn finalize_streaming_stops_streaming() {
        let mut state = State::new();
        state.update_last_assistant("partial".to_string());
        assert!(state.is_streaming());

        state.finalize_streaming();

        assert!(!state.is_streaming());
    }

    #[test]
    fn quick_action_seeds_the_composer_without_starting_a_run() {
        let mut state = State::new();
        let _ = state.update(Message::UsePrompt("Fix this bug: ".into()));

        assert_eq!(state.input(), "Fix this bug: ");
        assert!(state.entries().is_empty());
    }

    #[test]
    fn completion_card_is_persisted_as_a_transcript_entry() {
        let mut state = State::new();
        state.set_run_completion(true, true, vec!["src/main.rs".into()], Some("project".into()));
        assert!(matches!(state.entries().last(), Some(ChatEntry::Completion { .. })));
    }

    #[test]
    fn restored_running_tool_call_becomes_cancelled() {
        let restored = State::from_entries(vec![ChatEntry::ToolCall {
            id: 1,
            tool_name: "filesystem".into(),
            detail: String::new(),
            status: ToolCallStatus::Running,
            created_at: None,
        }]);

        assert!(matches!(
            restored.entries().first(),
            Some(ChatEntry::ToolCall { status: ToolCallStatus::Cancelled, .. })
        ));
    }

    #[test]
    fn repeated_tool_start_updates_one_entry() {
        let mut state = State::new();
        state.add_tool_call("filesystem".into(), String::new());
        state.add_tool_call("filesystem".into(), "write src/main.rs".into());

        assert_eq!(state.entries().len(), 1);
        assert!(matches!(
            state.entries().first(),
            Some(ChatEntry::ToolCall { detail, .. }) if detail == "write src/main.rs"
        ));
    }

    #[test]
    fn windows_extended_path_prefix_is_hidden() {
        assert_eq!(
            display_project_path(r"\\?\C:\Users\User\oxide-serve"),
            r"C:\Users\User\oxide-serve"
        );
    }

    #[test]
    fn state_new_creates_empty_state() {
        let state = State::new();
        assert!(state.entries().is_empty());
        assert!(state.input().is_empty());
    }

    #[test]
    fn add_user_message_creates_entry() {
        let mut state = State::new();
        let _ = state.update(Message::AddUser("hello".into()));
        assert_eq!(state.entries().len(), 1);
        assert!(matches!(state.entries()[0], ChatEntry::User { .. }));
    }

    #[test]
    fn add_assistant_message_creates_entry() {
        let mut state = State::new();
        let _ = state.update(Message::AddAssistant("hello".into()));
        assert_eq!(state.entries().len(), 1);
        assert!(matches!(state.entries()[0], ChatEntry::Assistant { .. }));
    }

    #[test]
    fn add_thinking_creates_thinking_entry() {
        let mut state = State::new();
        state.add_thinking("thinking...".to_string());
        assert_eq!(state.entries().len(), 1);
        assert!(matches!(state.entries()[0], ChatEntry::Thinking { .. }));
    }

    #[test]
    fn add_tool_call_creates_tool_call_entry() {
        let mut state = State::new();
        state.add_tool_call("read_file".into(), "src/main.rs".into());
        assert_eq!(state.entries().len(), 1);
        assert!(matches!(state.entries()[0], ChatEntry::ToolCall { .. }));
    }

    #[test]
    fn use_prompt_seeds_composer_text() {
        let mut state = State::new();
        let _ = state.update(Message::UsePrompt("test text".to_string()));
        assert_eq!(state.input(), "test text");
    }

    #[test]
    fn display_project_path_returns_normal_path() {
        assert_eq!(display_project_path("/home/user/project"), "/home/user/project");
    }

    #[test]
    fn legacy_v1_bare_array_still_loads() {
        let path = std::env::temp_dir()
            .join(format!("concerto_chat_legacy_v1_test_{}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);
        // Exact v1 on-disk shape: a bare array of enum variants with no
        // timestamp fields.
        std::fs::write(&path, r#"[{"User":{"id":1,"content":"hi"}}]"#).expect("write legacy v1");

        let loaded = State::load_entries(&path).expect("load legacy v1 transcript");
        assert_eq!(loaded.len(), 1);
        match &loaded[0] {
            ChatEntry::User { id, content, created_at, .. } => {
                assert_eq!(*id, 1);
                assert_eq!(content, "hi");
                assert!(created_at.is_none(), "legacy entries carry no timestamp");
            }
            _ => panic!("Expected a User entry"),
        }
        let restored = State::from_entries(loaded);
        assert_eq!(restored.entries().len(), 1);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn thinking_finishes_when_assistant_follows() {
        let mut state = State::new();
        state.add_thinking("planning...".to_string());
        state.update_last_assistant("answer".to_string());

        match &state.entries()[0] {
            ChatEntry::Thinking { finished_at, .. } => {
                assert!(finished_at.is_some(), "thinking entry should be finished");
            }
            _ => panic!("Expected a Thinking entry"),
        }
        assert!(matches!(
            state.entries().last(),
            Some(ChatEntry::Assistant { created_at: Some(_), .. })
        ));
    }

    #[test]
    fn consecutive_thinking_shares_one_open_entry() {
        let mut state = State::new();
        state.add_thinking("a".to_string());
        state.add_thinking("b".to_string());

        assert_eq!(state.entries().len(), 1);
        match &state.entries()[0] {
            ChatEntry::Thinking { content, finished_at, .. } => {
                assert_eq!(content, "a\nb");
                assert!(finished_at.is_none(), "open thinking stays unfinished");
            }
            _ => panic!("Expected a Thinking entry"),
        }
    }

    #[test]
    fn finalize_run_closes_all_open_thinking() {
        let mut state = State::new();
        state.add_thinking("planning a".to_string());
        // Collapse the open thinking entry; the next thought then starts a
        // new entry instead of appending (add_thinking dedupes only when the
        // trailing entry is not collapsed).
        let first_id = match &state.entries()[0] {
            ChatEntry::Thinking { id, .. } => *id,
            _ => panic!("Expected a Thinking entry"),
        };
        let _ = state.update(Message::ToggleEntry(first_id));
        state.add_thinking("planning b".to_string());
        // The assistant chunk closes only the *trailing* thinking entry; the
        // earlier collapsed one is out of reach of the trailing-only helper.
        state.update_last_assistant("partial answer".to_string());
        match &state.entries()[0] {
            ChatEntry::Thinking { finished_at, .. } => {
                assert!(
                    finished_at.is_none(),
                    "earlier collapsed thinking must stay open until finalize_run"
                );
            }
            _ => panic!("Expected a Thinking entry"),
        }

        state.finalize_run();

        // Every thinking entry — including the non-trailing collapsed one —
        // now carries an end timestamp, and streaming is cleared.
        let thinking: Vec<bool> = state
            .entries()
            .iter()
            .filter_map(|e| match e {
                ChatEntry::Thinking { finished_at, .. } => Some(finished_at.is_some()),
                _ => None,
            })
            .collect();
        assert_eq!(thinking.len(), 2, "two thinking entries exist");
        assert!(
            thinking.iter().all(|finished| *finished),
            "finalize_run must close every open thinking entry"
        );
        assert!(matches!(
            state.entries().last(),
            Some(ChatEntry::Assistant { streaming: false, .. })
        ));
    }

    #[test]
    fn finalize_streaming_does_not_close_thinking() {
        let mut state = State::new();
        state.add_thinking("planning...".to_string());
        state.finalize_streaming();

        match &state.entries()[0] {
            ChatEntry::Thinking { finished_at, .. } => {
                assert!(
                    finished_at.is_none(),
                    "finalize_streaming must not stamp thinking entries \
                     (the run may still be emitting AgentThoughts)"
                );
            }
            _ => panic!("Expected a Thinking entry"),
        }
    }

    #[test]
    fn reveal_starts_at_zero_on_first_stream_chunk() {
        let mut state = State::new();
        state.update_last_assistant("a\nb\nc".to_string());
        let id = match state.entries().last() {
            Some(ChatEntry::Assistant { id, .. }) => *id,
            _ => panic!("Expected an assistant entry"),
        };
        assert_eq!(state.revealed_chars, Some((id, 0)));
        assert!(state.is_revealing());
    }

    #[test]
    fn reveal_advances_with_typing_tick_and_clamps() {
        let mut state = State::new();
        // 20-character streaming payload.
        state.update_last_assistant("0123456789abcdefghij".to_string());
        let id = match state.entries().last() {
            Some(ChatEntry::Assistant { id, .. }) => *id,
            _ => panic!("Expected an assistant entry"),
        };

        for tick in 1..=3 {
            let _ = state.update(Message::TypingTick);
            let expected = (REVEAL_CHARS_PER_TICK * tick).min(20);
            assert_eq!(state.revealed_chars, Some((id, expected)));
            assert!(expected <= 20, "reveal must never pass the content length");
        }

        // Clamped at the content length; although the entry is still
        // streaming, nothing is left to reveal.
        assert_eq!(state.revealed_chars, Some((id, 20)));
        assert!(!state.is_revealing());
    }

    #[test]
    fn reveal_resets_on_second_assistant_entry() {
        let mut state = State::new();
        state.update_last_assistant("first reply".to_string());
        state.finalize_streaming();
        state.update_last_assistant("second reply".to_string());

        let id = match state.entries().last() {
            Some(ChatEntry::Assistant { id, .. }) => *id,
            _ => panic!("Expected an assistant entry"),
        };
        // A new streaming entry restarts its own typewriter reveal from zero.
        assert_eq!(state.revealed_chars, Some((id, 0)));
        assert!(state.is_revealing());
    }

    #[test]
    fn finalize_stops_reveal() {
        let mut state = State::new();
        state.update_last_assistant("final text".to_string());
        assert!(state.is_revealing());
        let _ = state.update(Message::TypingTick);
        assert!(state.is_revealing());

        state.finalize_run();
        // The reveal window is transient view state: gone at the run boundary,
        // and the subscription driving it (`typing_sub`) no longer fires.
        assert_eq!(state.revealed_chars, None);
        assert!(!state.is_revealing());
    }

    #[test]
    fn finalize_keeps_full_content() {
        let mut state = State::new();
        state.update_last_assistant("complete final reply".to_string());
        let _ = state.update(Message::TypingTick);

        state.finalize_run();
        match state.entries().last() {
            Some(ChatEntry::Assistant { content, streaming: false, .. }) => {
                assert_eq!(content, "complete final reply");
            }
            _ => panic!("Expected a finalized assistant entry"),
        }
    }

    #[test]
    fn entry_timestamp_renders_compact_and_absent_for_legacy() {
        assert_eq!(
            compact_entry_created_at(&Some("2026-08-03T14:22:00Z".to_string())),
            Some("08-03 14:22".to_string())
        );
        assert_eq!(compact_entry_created_at(&None), None);

        // User entries created through the message path carry a timestamp that
        // yields a non-empty compact label.
        let mut state = State::new();
        let _ = state.update(Message::AddUser("hello".into()));
        match state.entries().last() {
            Some(ChatEntry::User { created_at: Some(ts), .. }) => {
                let label = compact_entry_created_at(&Some(ts.clone()))
                    .expect("fresh entry timestamp must format");
                assert!(!label.is_empty());
            }
            _ => panic!("Expected a timestamped user entry"),
        }
    }

    #[test]
    fn add_assistant_seeds_reveal_and_marks_streaming() {
        let mut state = State::new();
        let _ = state.update(Message::AddAssistant("final reply".to_string()));
        let id = match state.entries().last() {
            Some(ChatEntry::Assistant { id, streaming, .. }) => {
                assert!(*streaming, "AddAssistant must stream to drive the reveal");
                *id
            }
            _ => panic!("Expected an assistant entry"),
        };
        assert_eq!(state.revealed_chars, Some((id, 0)));
        assert!(state.reveal_autofinish, "final messages auto-finalize when the reveal ends");
        assert!(state.is_revealing());
    }

    #[test]
    fn add_assistant_reveal_auto_finalizes() {
        let mut state = State::new();
        let content = "complete final reply".to_string();
        let _ = state.update(Message::AddAssistant(content.clone()));
        let id = match state.entries().last() {
            Some(ChatEntry::Assistant { id, .. }) => *id,
            _ => panic!("Expected an assistant entry"),
        };
        assert!(state.is_revealing());

        // Drive far past the reveal length; the entries must auto-finalize
        // instead of leaving a dangling streaming cursor behind.
        for _ in 0..100 {
            let _ = state.update(Message::TypingTick);
        }

        match state.entries().last() {
            Some(ChatEntry::Assistant {
                id: entry_id, content: final_content, streaming, ..
            }) => {
                assert_eq!(*entry_id, id);
                assert!(!*streaming, "reveal completion auto-finalizes the entry");
                assert_eq!(final_content, &content, "auto-finalize must not touch content");
            }
            _ => panic!("Expected an assistant entry"),
        }
        assert_eq!(state.revealed_chars, None, "window clears once the reveal is done");
        assert!(!state.reveal_autofinish);
        assert!(!state.is_revealing());
    }

    #[test]
    fn add_assistant_empty_string_does_not_stream() {
        let mut state = State::new();
        let _ = state.update(Message::AddAssistant(String::new()));
        match state.entries().last() {
            Some(ChatEntry::Assistant { streaming, content, .. }) => {
                assert!(!*streaming, "an empty final message must not drive a reveal/cursor");
                assert!(content.is_empty());
            }
            _ => panic!("Expected an assistant entry"),
        }
        assert_eq!(state.revealed_chars, None, "no reveal window for empty content");
        assert!(!state.is_revealing());
        assert!(!state.is_streaming(), "no blink subscription may linger at idle");
    }

    #[test]
    fn save_to_never_writes_streaming_true_for_assistant() {
        let mut state = State::new();
        // Seed a live, revealing assistant entry (streaming in memory).
        let _ = state.update(Message::AddAssistant("streamed text".to_string()));
        assert!(matches!(
            state.entries().last(),
            Some(ChatEntry::Assistant { streaming: true, .. })
        ));

        let path = std::env::temp_dir()
            .join(format!("concerto_chat_stream_save_{}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);
        state.save_to(&path).expect("save transcript");

        let loaded = State::load_entries(&path).expect("load transcript");
        let all_quiet = loaded.iter().all(|entry| match entry {
            ChatEntry::Assistant { streaming, .. } => !streaming,
            _ => true,
        });
        assert!(all_quiet, "transcripts must never persist a streaming assistant entry");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn thinking_reveal_advances_per_tick_and_clamps() {
        let mut state = State::new();
        state.add_thinking("0123456789".to_string()); // 10 chars
        let id = match state.entries().last() {
            Some(ChatEntry::Thinking { id, .. }) => *id,
            _ => panic!("Expected a thinking entry"),
        };
        assert_eq!(state.thinking_reveals.get(&id), Some(&0));
        assert!(state.is_revealing());

        // First tick reveals 64 chars, clamped to the 10-char content; the
        // reveal then completes and is removed (content fully visible).
        let _ = state.update(Message::TypingTick);
        assert_eq!(state.thinking_reveals.get(&id), None, "reveal finishes at the content length");
        // Subsequent ticks must not re-insert the reveal.
        let _ = state.update(Message::TypingTick);
        assert!(!state.thinking_reveals.contains_key(&id));
    }

    #[test]
    fn thinking_reveal_clamps_when_content_grows() {
        let mut state = State::new();
        state.add_thinking("short".to_string());
        let id = match state.entries().last() {
            Some(ChatEntry::Thinking { id, .. }) => *id,
            _ => panic!("Expected a thinking entry"),
        };
        // Reveal finishes instantly (5 chars < 64/tick).
        let _ = state.update(Message::TypingTick);
        assert!(!state.thinking_reveals.contains_key(&id));
        // Content grows; the completed reveal must NOT restart for appended
        // content (per design: only an in-flight reveal is clamped).
        state.add_thinking(" longer tail".to_string());
        assert!(
            !state.thinking_reveals.contains_key(&id),
            "appended content must not resurrect a completed reveal"
        );
    }

    #[test]
    fn entrance_fade_completes_after_entrance_ticks() {
        let mut state = State::new();
        state.add_error("blocking".to_string());
        let id = match state.entries().last() {
            Some(ChatEntry::Error { id, .. }) => *id,
            _ => panic!("Expected an error entry"),
        };
        assert_eq!(state.entrance_ticks.get(&id), Some(&0));
        assert!(state.is_revealing(), "a pending entrance fade keeps the tick alive");

        // Exactly `ENTRANCE_TICKS` fades complete and drop the entry.
        for _ in 0..ENTRANCE_TICKS {
            assert!(state.is_revealing(), "fade stays live until the cap");
            let _ = state.update(Message::TypingTick);
        }
        assert!(!state.entrance_ticks.contains_key(&id), "fade entry removed at cap");
        assert!(!state.is_revealing(), "no animation work remains once the fade finished");
    }

    #[test]
    fn open_thinking_keeps_is_revealing_for_shimmer() {
        let mut state = State::new();
        state.add_thinking("planning...".to_string());
        let id = match state.entries().last() {
            Some(ChatEntry::Thinking { id, .. }) => *id,
            _ => panic!("Expected a thinking entry"),
        };
        // Reveal completes on the first tick...
        let _ = state.update(Message::TypingTick);
        assert!(!state.thinking_reveals.contains_key(&id));
        // ...but the open entry still drives the shimmer, so the tick stays on.
        assert!(state.is_revealing(), "open thinking keeps the shimmer tick alive");

        state.finalize_run();
        assert!(!state.is_revealing(), "stamping the end timestamp stops the shimmer");
    }

    #[test]
    fn cached_docs_populated_for_restored_assistants() {
        let mut entries = vec![ChatEntry::Assistant {
            id: 1,
            content: "## Hello\n\nWorld".to_string(),
            streaming: false,
            created_at: None,
        }];
        entries.push(ChatEntry::User { id: 2, content: "hi".to_string(), created_at: None });
        let restored = State::from_entries(entries);
        assert!(
            restored.md_docs.get(&1).is_some_and(|doc| doc.total_units > 0),
            "restored assistant content must be eagerly cached"
        );
        assert!(!restored.md_docs.contains_key(&2), "non-assistant entries never cache");
    }
}
