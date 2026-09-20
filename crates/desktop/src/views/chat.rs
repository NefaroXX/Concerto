use iced::widget::{
    button, column, container, pick_list, row, rule, scrollable, text, text_input, toggler, tooltip,
};
use iced::{border::Radius, Alignment, Background, Border, Color, Element, Length};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::theme::AppTheme;
use crate::views::agent_graph;
use crate::views::spend::{
    cap_status_text, compact_created_at, spend_totals, CapUiState, SpendTotals,
};
use crate::widgets::agent_graph::NodeState;
use crate::widgets::markdown;
use concerto_core::event::ThinkingKind;
use concerto_core::types::normalize_agent_id;
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
    /// Per-session Runtime modal overlay: the read-only Coordinator 2.0
    /// observability panels (Decision Journal / World Model / Failure
    /// Diagnoses / Suitability) for the active session. Opened from the chat
    /// window (Ctrl+R); Esc or the close button dismisses it.
    Runtime,
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
    /// Mute/unmute one agent's thinking bucket (filter bar). View-only: the
    /// WAL and transcript still record everything.
    ToggleMuteAgent(String),
    /// Collapse every thinking bucket (`/thinking` toggle → collapsed side).
    CollapseAllThinking,
    /// Expand every thinking bucket (`/thinking` toggle → expanded side).
    ExpandAllThinking,
    /// `/thinking` accordion toggle for the current movement: expand all when
    /// nothing is open, collapse all otherwise. Drives `toggle_thinking_all`
    /// from the composer's real button — the `/thinking` text command still
    /// works too (handled in `app.rs`).
    ToggleThinkingAll,
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
        /// Agent that produced this thought — the V2 bucket key. Legacy
        /// entries (and `Message::AddThinking`) leave this empty; the render
        /// path falls back to parsing the `[agent]` content prefix.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        agent: String,
        content: String,
        /// Verbosity tier (V2). Legacy entries default to `Detail`.
        #[serde(default, skip_serializing_if = "ThinkingKind::is_detail")]
        kind: ThinkingKind,
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

/// Provenance rail caption for a tool call (Blueprint texture `‖ ok`).
/// Read-only derivation from the stored status + tool name — it never
/// touches the policy engine, executor, or `VirtualFs`. Rendered as a tiny
/// `text_muted` caption under the tool row; palette colors only.
fn provenance_rail(status: &ToolCallStatus, tool_name: &str) -> String {
    let policy = match status {
        ToolCallStatus::Denied => "policy denied",
        ToolCallStatus::Running | ToolCallStatus::Cancelled => "approval needed",
        ToolCallStatus::Completed | ToolCallStatus::Allowed | ToolCallStatus::Failed => "policy ok",
    };
    let reversibility = if is_mutating_tool(tool_name) { "fs reversible" } else { "read-only" };
    format!("‖ {policy} · {reversibility}")
}

/// Whether a tool stages mutations through the reversible `VirtualFs`
/// overlay (file writes / shell / git roll back via stash/branch).
/// Name-based heuristic, deliberately conservative: unknown tools read as
/// read-only rather than promising reversibility.
fn is_mutating_tool(tool_name: &str) -> bool {
    let name = tool_name.to_ascii_lowercase();
    ["write", "edit", "apply", "shell", "exec", "bash", "sh", "git", "mkdir", "patch", "commit"]
        .iter()
        .any(|marker| name.contains(marker))
}

pub struct State {
    entries: Vec<ChatEntry>,
    input: String,
    next_id: EntryId,
    /// Active sub-view overlay shown on top of the chat canvas.
    pub sub_view: SubView,
    /// The first id of the currently expanded thinking group, or `None` when
    /// all groups are collapsed. Drives the accordion toggle.
    expanded_thinking_id: Option<EntryId>,
    /// `/thinking` expand-all override: when true every bucket renders
    /// expanded regardless of `expanded_thinking_id`.
    thinking_expand_all: bool,
    /// Per-agent mute set for the thinking filter bar. Muted buckets are
    /// hidden from chat only — the WAL, transcript, and AgentGraph logs
    /// still record everything. Persisted to `[display] muted_agents`;
    /// seeded from config at startup via [`Self::set_muted_agents`].
    muted_agents: HashSet<String>,
    /// Whether the blinking cursor is currently shown on the live streaming
    /// assistant entry. Toggled by `Message::StreamingTick`; only meaningful
    /// (and only ever read) while at least one entry is still streaming.
    streaming_cursor_visible: bool,
    /// Typewriter-reveal frontier for a streaming assistant entry:
    /// `Some((id, n))` means the entry with id `id` has revealed its first
    /// `n` *characters* so far. Keyed by entry id rather than tail position:
    /// the run-boundary path (`AddAssistant` + `set_run_completion`) appends
    /// a `Completion` chip after the final reply, so the revealed entry is
    /// never the last one while its reveal plays. Transient view state only —
    /// deliberately NOT a `ChatEntry` field, never serialized, and dropped
    /// once the entry is finalized. `None` means nothing is animating (all
    /// content shown).
    revealed_chars: Option<(EntryId, usize)>,
    /// True while a reveal is being driven toward the end of a *final*
    /// assistant message (seeded by `Message::AddAssistant`). When the reveal
    /// reaches the content length the entry is auto-finalized: `streaming`
    /// flips to `false`, no dying cursor lingers, and the blink + typing
    /// subscriptions stop. Live streaming entries (seeded by
    /// `update_last_assistant`) keep their window at the content length
    /// instead, matching the pre-autofinish behavior.
    reveal_autofinish: bool,
    /// First-token emphasis ticks for assistant entries: `id -> elapsed
    /// ticks` since the turn started. While present, the first token (up to
    /// first whitespace) of the revealed text renders bold for ~384 ms
    /// (24 ticks × 16 ms), then settles. Transient view state — never
    /// serialized.
    first_token_ticks: HashMap<EntryId, u8>,
    /// Paragraph handoff hold (Baton seasoning, prototype #5): `Some((id,
    /// remaining))` while the typewriter reveal pauses at a blank-line
    /// paragraph boundary to signal "continuing". Counts down one per
    /// `TypingTick`; the reveal frontier stays frozen and the streaming
    /// cursor holds visible until it clears. Transient view state — never
    /// serialized.
    handoff_hold: Option<(EntryId, u8)>,
    /// Reduced-motion override: when true every tick-driven animation is
    /// skipped — typewriter reveal, first-token emphasis, line wipe, entrance
    /// fades, thinking reveals and the paragraph handoff all render instantly
    /// (the design-doc fallbacks). Iced exposes no system a11y API (see the
    /// scanline_overlay call site in `app.rs`), so this defaults to false until
    /// wired to a user setting.
    reduced_motion: bool,
    /// Per-entry cached parses of assistant markdown, so the 16 ms reveal tick
    /// re-renders an already-parsed event stream (`render_upto`) instead of
    /// re-running pulldown_cmark on the raw text every frame. Transient view
    /// state — never serialized; populated eagerly wherever assistant content
    /// is set and dropped by `trim_entries` when entries are evicted.
    md_docs: HashMap<EntryId, markdown::MarkdownDoc>,
    /// Entrance-fade progress for ToolCall / Error / Completion chips
    /// (and thinking previews): `id -> tick count since insertion`, capped
    /// at `ENTRANCE_TICKS` (then removed). Transient view state — never
    /// serialized.
    entrance_ticks: HashMap<EntryId, u8>,
    /// Line-wipe progress for new assistant entries (Score seasoning,
    /// prototype #7): `id -> tick count since insertion`, capped at
    /// `LINE_WIPE_TICKS` (then moved to [`line_wipe_settled`](Self::line_wipe_settled)).
    /// Transient view state — never serialized.
    line_wipe_ticks: HashMap<EntryId, u8>,
    /// Assistant entries whose line-wipe animation has settled to its muted
    /// top-rule marker (Score seasoning, prototype #7): while an id is present
    /// the entry's top rule stays rendered as a full-width, faint hairline —
    /// the visible "new turn" signature. It survives run finalize and mid-run
    /// navigation so a just-finished reply keeps its new-text marker instead
    /// of reverting to the old plain-text look. The marker is *static* (never
    /// animates), so it must not keep the 16 ms tick alive by itself.
    /// Reduced-motion never seeds it and clears it on enable. Transient view
    /// state — never serialized.
    line_wipe_settled: HashSet<EntryId>,
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
/// ~500 chars/s — a fast typing feel. Kept in parity with the CLI
/// (`crates/cli/src/app.rs`, `REVEAL_CHARS_PER_TICK = 8`): the reveal is a
/// *typing* texture, deliberately faster than typical provider streaming so
/// text is never visibly lagging behind the token arrival it follows.
const REVEAL_CHARS_PER_TICK: usize = 8;
/// Number of `TypingTick`s an entrance fade runs for (~128 ms at 16 ms/tick).
const ENTRANCE_TICKS: u8 = 8;
/// Number of `TypingTick`s the paragraph handoff cue holds (~128 ms at
/// 16 ms/tick). The reveal frontier freezes at the blank-line boundary and
/// the streaming cursor holds visible, then the reveal resumes — the Baton
/// seasoning (prototype #5). Deterministic: always exactly this many ticks.
/// Set long enough to read as an intentional "continuing" pause (the earlier
/// 3-tick / ~48 ms hold was sub-perceptual next to a 500 ms blink) without
/// upsetting the typing cadence.
const HANDOFF_HOLD_TICKS: u8 = 8;
/// Number of `TypingTick`s the first-token emphasis holds (~384 ms at 16 ms/tick).
/// The first word of each assistant turn renders slightly bolder/larger for this
/// duration, then settles to normal weight — the Score signature text (prototype #6).
/// Lengthened from 12 ticks (~200 ms): a 500 chars/s reveal outraces typical
/// provider streaming, so the freshly rendered post-prompt lines are the ones
/// a user reads first — a longer emphasis makes the turn start register as new.
const FIRST_TOKEN_EMPHASIS_TICKS: u8 = 24;
/// Number of `TypingTick`s a new assistant entry's top rule wipes 0→full
/// width over (~128 ms at 16 ms/tick) — the Score line-wipe entrance
/// (prototype #7). Deterministic: always exactly this many ticks.
const LINE_WIPE_TICKS: u8 = 8;
/// Period (in ticks) of the subtle thinking shimmer pulse: the color
/// interpolates from muted to text over `SHIMMER_PERIOD` ticks and back.
const SHIMMER_PERIOD: u32 = 8;

/// One agent's thinking bucket: every `Thinking` entry from that agent in
/// the current run, regardless of consecutiveness. The bucket's `expanded`
/// state is keyed to the first entry id so `ToggleEntry` toggles the whole
/// bucket (or `thinking_expand_all` opens all of them via `/thinking`).
#[derive(Debug, Clone)]
struct ThinkingGroup {
    agent_id: String,
    /// Indices into the parent `entries` slice.
    indices: Vec<usize>,
    /// The first entry id in the group — `ToggleEntry` target.
    first_id: EntryId,
    /// Whether this group is expanded (one at a time).
    expanded: bool,
    /// Latest headline: first line of the latest entry's content, heuristically
    /// shortened to a movement verb phrase.
    headline: String,
}

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
            expanded_thinking_id: None,
            thinking_expand_all: false,
            muted_agents: HashSet::new(),
            streaming_cursor_visible: false,
            revealed_chars: None,
            reveal_autofinish: false,
            first_token_ticks: HashMap::new(),
            reduced_motion: false,
            md_docs: HashMap::new(),
            entrance_ticks: HashMap::new(),
            line_wipe_ticks: HashMap::new(),
            line_wipe_settled: HashSet::new(),
            handoff_hold: None,
            shimmer_phase: 0,
            spend_log: Vec::new(),
            spend_log_loaded: false,
        }
    }

    /// Reconstruct a `State` from previously persisted entries (per-project
    /// chat transcript). The next entry id continues after the highest id
    /// already present so new entries don't collide with restored ones.
    ///
    /// Stored thinking agents normalize on read so old transcript rows map
    /// forward into the same buckets as live events.
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
            if let ChatEntry::Thinking { agent, .. } = entry {
                *agent = normalize_agent_id(agent);
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
            expanded_thinking_id: None,
            thinking_expand_all: false,
            muted_agents: HashSet::new(),
            streaming_cursor_visible: false,
            revealed_chars: None,
            reveal_autofinish: false,
            first_token_ticks: HashMap::new(),
            reduced_motion: false,
            md_docs,
            entrance_ticks: HashMap::new(),
            line_wipe_ticks: HashMap::new(),
            line_wipe_settled: HashSet::new(),
            handoff_hold: None,
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
            self.entrance_ticks.clear();
            self.first_token_ticks.clear();
            self.line_wipe_ticks.clear();
            self.line_wipe_settled.clear();
            self.handoff_hold = None;
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
        if !self.reduced_motion {
            self.entrance_ticks.insert(id, 0);
        }
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

    /// Add a thinking entry, bucketed per agent (V2). Consecutive open
    /// entries from the same agent at the same tier merge into one; anything
    /// else appends. Never touches the expand/mute view state, so new input
    /// never auto-expands a bucket.
    ///
    /// The stored agent id is normalized (trimmed + lowercased) so
    /// `" Coder "` and `"coder"` share one bucket; the content text keeps
    /// its original prefix untouched.
    pub fn add_thinking(&mut self, agent_id: &str, content: String, kind: ThinkingKind) {
        let agent_id = normalize_agent_id(agent_id);
        if let Some(ChatEntry::Thinking {
            agent,
            kind: existing_kind,
            content: ref mut existing,
            collapsed: false,
            finished_at: None,
            ..
        }) = self.entries.last_mut()
        {
            if *agent == agent_id && *existing_kind == kind {
                existing.push('\n');
                existing.push_str(&content);
                *existing = tail_chars(existing, MAX_THINKING_CHARS);
                // Merged content shares the entry's entrance fade — never
                // restarts it (a settled preview must not re-fade).
                return;
            }
        }
        let id = self.next_id;
        self.next_id += 1;
        // Collapse heuristic counts *characters*: a bytes() cutoff mis-counts
        // multi-byte content (CJK, emoji), so the same text could sometimes
        // collide into an open bucket and sometimes spawn a ragged one.
        let collapsed = content.chars().count() > 500;
        self.entries.push(ChatEntry::Thinking {
            id,
            agent: agent_id,
            content,
            kind,
            collapsed,
            created_at: Some(now_rfc3339()),
            finished_at: None,
        });
        // Thinking previews render their full content as it arrives with a
        // short entrance fade only — no typewriter reveal, so a cold start
        // never reads as a blank colored block that fills over seconds.
        // Reduced-motion shows the preview as full text instantly.
        if !self.reduced_motion {
            self.entrance_ticks.insert(id, 0);
        }
        self.trim_entries();
    }

    /// Replace the muted-agent filter set, seeding from `[display]
    /// muted_agents` at startup. Entries normalize exactly like bucket keys
    /// (trimmed + lowercased, empties dropped) so config and buckets agree.
    pub fn set_muted_agents(&mut self, agents: Vec<String>) {
        self.muted_agents = agents
            .iter()
            .map(|agent| normalize_agent_id(agent))
            .filter(|agent| !agent.is_empty())
            .collect();
    }

    /// Snapshot the muted-agent filter set for config persistence (sorted
    /// for stable file output). Hide-not-delete is preserved: the WAL and
    /// transcript keep every thought regardless of this filter.
    pub fn muted_agents_snapshot(&self) -> Vec<String> {
        let mut agents: Vec<String> = self.muted_agents.iter().cloned().collect();
        agents.sort();
        agents
    }

    /// Surface a blocking error to the user as a distinct chat entry (e.g. a
    /// dispatch validation failure before a run starts).
    pub fn add_error(&mut self, content: String) {
        self.finish_open_thinking();
        let id = self.next_id;
        self.next_id += 1;
        if !self.reduced_motion {
            self.entrance_ticks.insert(id, 0);
        }
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
        if !self.reduced_motion {
            self.entrance_ticks.insert(id, 0);
        }
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
        if !self.reduced_motion {
            self.entrance_ticks.insert(id, 0);
        }
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
        // Reduced-motion renders every entry instantly (design-doc fallbacks):
        // no tick-driven animation state is seeded, and any in-flight
        // animation settles in `set_reduced_motion`. Return false so the
        // 16 ms `TypingTick` subscription is never started.
        if self.reduced_motion {
            return false;
        }
        // Assistant typewriter-reveal window still animating. The window is
        // keyed by entry id, not by tail position: the run-boundary path
        // (`AddAssistant` + `set_run_completion`) appends a `Completion` chip
        // after the final reply, so the revealed entry is no longer the last
        // one — searching by id keeps the reveal alive until it finishes.
        let assistant_revealing = self.revealed_chars.is_some_and(|(id, revealed)| {
            self.entries.iter().any(|entry| {
                matches!(
                    entry,
                    ChatEntry::Assistant { id: entry_id, content, streaming: true, .. }
                        if *entry_id == id && revealed < content.chars().count()
                )
            })
        });
        if assistant_revealing {
            return true;
        }
        // Thinking-preview fade, entrance fade, first-token emphasis,
        // line wipe, or paragraph handoff hold still animating.
        if !self.entrance_ticks.is_empty()
            || !self.first_token_ticks.is_empty()
            || !self.line_wipe_ticks.is_empty()
            || self.handoff_hold.is_some()
        {
            return true;
        }
        // An open-but-settled thinking entry is static: its shimmer freezes
        // at the last rendered color, so it must NOT keep the 16 ms tick
        // alive indefinitely (the "thinking entry pulses forever" defect —
        // the fade above provides the transient motion). Nothing left to
        // animate.
        false
    }

    /// Advance the typewriter reveal one tick on the streaming assistant
    /// entry tracked by `revealed_chars`. The tracked entry is located by id
    /// anywhere in the transcript (not just the tail), so a `Completion` chip
    /// appended after the final reply cannot orphan the window. While a
    /// paragraph handoff hold is open the frontier freezes and the hold counts
    /// down instead of advancing. No-op when no reveal is in progress; resets
    /// the window to `None` when the tracked id is gone or no longer
    /// streaming, so a stale window can never drive a completed entry.
    fn advance_reveal(&mut self) {
        // Handoff hold (Baton prototype #5): freeze the frontier at the
        // paragraph boundary while the cue counts down, one tick at a time.
        // A stale hold (tracked entry gone or no longer streaming) clears.
        // The hold is keyed by id too, so a trailing `Completion` chip cannot
        // orphan it either.
        if let Some((hold_id, remaining)) = self.handoff_hold {
            let live = self.entries.iter().any(|entry| {
                matches!(
                    entry,
                    ChatEntry::Assistant { id: entry_id, streaming: true, .. }
                        if *entry_id == hold_id
                )
            });
            if live {
                if remaining <= 1 {
                    self.handoff_hold = None;
                } else {
                    self.handoff_hold = Some((hold_id, remaining - 1));
                }
                return;
            }
            self.handoff_hold = None;
        }
        let Some((id, revealed)) = self.revealed_chars else {
            return;
        };
        // The reveal window is keyed by entry id, not tail position: the
        // final reply seeded by `Message::AddAssistant` is immediately
        // followed by the run's `Completion` chip (`set_run_completion`), so
        // it is never the last entry while its reveal plays. Locate the
        // tracked entry anywhere in the transcript — still requiring it to be
        // `streaming: true`, so a stale window can never drive a finalized
        // entry. The length is snapshotted first because the entry borrow
        // cannot stay live across the writes below.
        let Some(len) = self.entries.iter().find_map(|entry| match entry {
            ChatEntry::Assistant { id: entry_id, content, streaming: true, .. }
                if *entry_id == id =>
            {
                Some(content.chars().count())
            }
            _ => None,
        }) else {
            self.revealed_chars = None;
            self.reveal_autofinish = false;
            self.handoff_hold = None;
            return;
        };
        let next = revealed.saturating_add(REVEAL_CHARS_PER_TICK).min(len);
        if next == len && self.reveal_autofinish {
            // A *final* message has fully revealed: the text is complete, so
            // mark the entry non-streaming (dropping the blinking cursor and
            // both subscriptions) and clear the window — the reveal is done,
            // not paused mid-content.
            self.finalize_tracked_reveal(id);
            self.revealed_chars = None;
            self.reveal_autofinish = false;
            self.handoff_hold = None;
        } else {
            // Paragraph handoff cue (Baton prototype #5): when the frontier
            // crosses a blank-line boundary mid-reveal, hold the cursor for
            // `HANDOFF_HOLD_TICKS` before resuming. Single-paragraph content
            // never triggers (fallback: no cue); reduced-motion skips the cue
            // entirely. This is a read-only pass over the entry (it borrows
            // `self.entries`) before the window write below.
            let open_hold =
                next != len && !self.reduced_motion && self.entries.iter().any(|entry| {
                    matches!(
                        entry,
                        ChatEntry::Assistant {
                            id: entry_id,
                            content,
                            streaming: true,
                            ..
                        } if *entry_id == id && crosses_paragraph_boundary(content, revealed, next)
                    )
                });
            if open_hold {
                self.handoff_hold = Some((id, HANDOFF_HOLD_TICKS));
            }
            self.revealed_chars = Some((id, next));
        }
    }

    /// Mark the assistant entry tracked by the autofinish reveal (seeded by
    /// `Message::AddAssistant`) non-streaming, if it still exists and is
    /// streaming. The tracked id — not the tail position — decides, so the
    /// entry finalizes even when the run's `Completion` chip (or any other
    /// entry) has been appended after it. This is the single place that
    /// settles a final reply mid-reveal, shared by the tick completer, run
    /// boundaries, and the reduced-motion toggle.
    fn finalize_tracked_reveal(&mut self, id: EntryId) {
        if let Some(ChatEntry::Assistant { streaming, .. }) =
            self.entries.iter_mut().find(|entry| {
                matches!(
                    entry,
                    ChatEntry::Assistant { id: entry_id, streaming: true, .. }
                        if *entry_id == id
                )
            })
        {
            *streaming = false;
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

    /// Advance first-token emphasis ticks: count each entry's elapsed ticks
    /// up, removing entries whose emphasis has expired. Each tick is ~16 ms,
    /// so `FIRST_TOKEN_EMPHASIS_TICKS` ticks ≈ 200 ms of emphasis.
    fn advance_first_token_ticks(&mut self) {
        self.first_token_ticks.retain(|_, ticks| {
            *ticks = ticks.saturating_add(1);
            *ticks < FIRST_TOKEN_EMPHASIS_TICKS
        });
    }

    /// Whether the assistant entry's first token still renders emphasized:
    /// its emphasis window is open and reduced-motion is off.
    fn first_token_emphasis(&self, id: EntryId) -> bool {
        !self.reduced_motion && self.first_token_ticks.contains_key(&id)
    }

    /// Advance every in-flight line wipe one tick, converging the wipe to its
    /// settled top-rule marker when it completes, and dropping any stale ids
    /// (entry evicted or no longer an assistant entry) so a dead wipe can
    /// never hold the tick alive.
    fn advance_line_wipe_ticks(&mut self) {
        // Ids are snapshotted first: the wipe map is mutated below, so the
        // entries borrow cannot stay live across the updates.
        let live: HashSet<EntryId> = self
            .entries
            .iter()
            .filter_map(|entry| match entry {
                ChatEntry::Assistant { id, .. } => Some(*id),
                _ => None,
            })
            .collect();
        // `settling` collects ids whose animation just finished so the retain
        // closure stays free of `&mut self.line_wipe_settled` borrows.
        let mut settling: Vec<EntryId> = Vec::new();
        self.line_wipe_ticks.retain(|id, ticks| {
            if !live.contains(id) {
                return false;
            }
            *ticks = ticks.saturating_add(1);
            if *ticks < LINE_WIPE_TICKS {
                true
            } else {
                settling.push(*id);
                false
            }
        });
        for id in settling {
            self.line_wipe_settled.insert(id);
        }
    }

    /// Settle every in-flight line wipe: the animated tick entry drops and
    /// the id moves to the settled set so the entry keeps its muted top-rule
    /// marker. Used at run/navigation boundaries where the wipe may never have
    /// had a tick to play (a fresh reply instantly finalized) — the "new
    /// turn" signature should still be visible post-run. Reduced-motion never
    /// seeds wipes, so the settled set stays clean under that override.
    fn settle_line_wipe(&mut self) {
        for (id, _) in self.line_wipe_ticks.drain() {
            self.line_wipe_settled.insert(id);
        }
    }

    /// Current line-wipe step for an assistant entry: `Some(elapsed)` while
    /// the wipe is animating, `None` once done or when reduced-motion skips
    /// the cue (the entry renders instantly with no rule).
    fn line_wipe_step(&self, id: EntryId) -> Option<u8> {
        if self.reduced_motion {
            return None;
        }
        self.line_wipe_ticks.get(&id).copied()
    }

    /// Set the reduced-motion override. When enabled, every in-flight
    /// tick-driven animation settles instantly — first-token emphasis, line
    /// wipe, entrance fades, thinking reveals, the paragraph handoff and the
    /// typewriter-reveal window all drop — so entries render at full content
    /// immediately. A *final* assistant message mid-reveal finalizes too, so
    /// no streaming cursor or blink subscription outlives the toggle.
    pub fn set_reduced_motion(&mut self, reduced: bool) {
        self.reduced_motion = reduced;
        if reduced {
            // A *final* message mid-reveal finalizes even when the run's
            // `Completion` chip has already been appended after it: the
            // tracked id decides, not the tail position.
            if self.reveal_autofinish {
                if let Some((id, _)) = self.revealed_chars {
                    self.finalize_tracked_reveal(id);
                }
            }
            self.revealed_chars = None;
            self.reveal_autofinish = false;
            self.handoff_hold = None;
            self.first_token_ticks.clear();
            self.entrance_ticks.clear();
            self.line_wipe_ticks.clear();
            self.line_wipe_settled.clear();
        }
    }

    /// Whether the reduced-motion override is active.
    pub fn reduced_motion(&self) -> bool {
        self.reduced_motion
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
            // Reduced-motion renders every arrival instantly: no reveal
            // window, no emphasis, no wipe (design-doc fallbacks).
            self.handoff_hold = None;
            if !self.reduced_motion {
                self.revealed_chars = Some((id, 0));
                // Seed first-token emphasis for the new turn (prototype #6):
                // elapsed-tick counter starts at 0 and expires after
                // `FIRST_TOKEN_EMPHASIS_TICKS` TypingTicks (~384 ms).
                self.first_token_ticks.insert(id, 0);
                // Seed the line-wipe entrance cue for the new turn
                // (prototype #7): the top rule wipes 0→full over
                // `LINE_WIPE_TICKS` TypingTicks.
                self.line_wipe_ticks.insert(id, 0);
            }
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
        // A final reply mid-autofinish-reveal is finalized by its tracked id —
        // the run's `Completion` chip may already trail it — so navigating
        // away mid-reveal cannot leave a `streaming` entry behind. Live
        // streaming entries (no autofinish) sit at the tail, as before.
        if self.reveal_autofinish {
            if let Some((id, _)) = self.revealed_chars {
                self.finalize_tracked_reveal(id);
            }
        } else if let Some(ChatEntry::Assistant { streaming, .. }) = self.entries.last_mut() {
            *streaming = false;
        }
        // The reveal window is transient; once the entry is finalized the
        // full text renders and there is nothing left to drive. The
        // first-token emphasis settles immediately at this boundary too.
        // In-flight line wipes *converge* to their settled top-rule marker
        // instead of dropping: a reply that was navigating-away mid-cue still
        // keeps its visible "new turn" hairline.
        self.revealed_chars = None;
        self.reveal_autofinish = false;
        self.first_token_ticks.clear();
        self.settle_line_wipe();
        self.handoff_hold = None;
    }

    /// Finalize a run: mark the last assistant entry as non-streaming (same
    /// as [`finalize_streaming`](Self::finalize_streaming)) AND stamp the end
    /// timestamp on every thinking entry still open. Safe only at the true
    /// run boundary (run completed or cancelled) because no further thinking
    /// content can arrive afterwards.
    ///
    /// A *final reply* that is still typewriter-revealing when the run ends is
    /// NOT snapped to full text: the app appends the `Completion` chip after
    /// this call, and the reveal keeps driving past that chip until it reaches
    /// the end (the tracked entry auto-finalizes itself, then the window
    /// drops). Killing the window here was the "instant dump" defect — the
    /// final reply jumped from its mid-reveal frontier to full text the moment
    /// the run landed.
    pub fn finalize_run(&mut self) {
        self.finish_all_open_thinking();
        // Only a live autofinish window is preserved: its tracked entry is
        // still `streaming` (found by id, not tail position, because the
        // Completion chip trails it).
        let autofinish_live = self.reveal_autofinish
            && self.revealed_chars.is_some_and(|(id, _)| {
                self.entries.iter().any(|entry| {
                    matches!(entry, ChatEntry::Assistant { id: entry_id, streaming: true, .. }
                    if *entry_id == id)
                })
            });
        if !autofinish_live {
            // No live autofinish window: either a live-streamed tail entry
            // (mark it non-streaming and drop the window — full text is final)
            // or a stale autofinish window whose tracked entry already
            // finalized (drop the orphaned window).
            if !self.reveal_autofinish {
                if let Some(ChatEntry::Assistant { streaming, .. }) = self.entries.last_mut() {
                    *streaming = false;
                }
            }
            self.revealed_chars = None;
            self.reveal_autofinish = false;
            self.handoff_hold = None;
        }
        // First-token emphasis settles at the boundary; in-flight line wipes
        // *converge* to their settled top-rule marker instead of dropping: a
        // just-finished reply keeps its visible "new turn" hairline.
        self.first_token_ticks.clear();
        self.settle_line_wipe();
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
                    self.handoff_hold = None;
                    if self.reduced_motion {
                        // Reduced-motion renders the final message instantly:
                        // no reveal window, no emphasis, no wipe.
                        self.reveal_autofinish = false;
                    } else {
                        // The final message enters with a typewriter reveal;
                        // once it reaches the end (`reveal_autofinish` + full
                        // window) the entry auto-finalizes itself.
                        self.revealed_chars = Some((id, 0));
                        self.reveal_autofinish = true;
                        // Seed first-token emphasis for the new turn (prototype #6):
                        // elapsed-tick counter starts at 0 (≈384 ms of emphasis).
                        self.first_token_ticks.insert(id, 0);
                        // Seed the line-wipe entrance cue (prototype #7), same
                        // cadence contract as the other entrance ticks.
                        self.line_wipe_ticks.insert(id, 0);
                    }
                }
                // An empty final message must never linger as a `streaming`
                // entry: without a reveal window it would blink a cursor and
                // keep the blink subscription alive at idle forever. Reduced
                // motion also renders instantly — a non-streaming entry shows
                // the full text with no cursor and no subscriptions.
                let streaming = !s.is_empty() && !self.reduced_motion;
                self.entries.push(ChatEntry::Assistant {
                    id,
                    content: s,
                    streaming,
                    created_at: Some(now_rfc3339()),
                });
                self.md_docs.insert(id, doc);
            }
            Message::AddThinking(s) => {
                // Legacy untyped path (tests, quick seeds): unattributed Detail.
                self.add_thinking("", s, ThinkingKind::Detail);
            }
            Message::AddToolCall(s) => {
                self.add_tool_call(s, String::new());
            }
            Message::StreamingTick => {
                self.streaming_cursor_visible = !self.streaming_cursor_visible;
            }
            Message::TypingTick => {
                self.advance_reveal();
                self.advance_entrance_ticks();
                self.advance_first_token_ticks();
                self.advance_line_wipe_ticks();
                self.shimmer_phase = self.shimmer_phase.wrapping_add(1);
            }
            Message::UsePrompt(prompt) => {
                self.input = prompt;
            }
            Message::SelectSession(_) => {
                // Handled by App because session loading requires shared services.
            }
            Message::ToggleEntry(id) => {
                // Toggle the thinking bucket accordion: the id is the first
                // entry's id of the bucket. Clicking the same bucket collapses
                // it; clicking a different bucket switches expansion. A manual
                // toggle always leaves expand-all mode first.
                if let Some(ChatEntry::Thinking { .. }) = self
                    .entries
                    .iter()
                    .find(|e| matches!(e, ChatEntry::Thinking { id: eid, .. } if *eid == id))
                {
                    self.thinking_expand_all = false;
                    self.expanded_thinking_id =
                        if self.expanded_thinking_id == Some(id) { None } else { Some(id) };
                }
            }
            Message::ToggleMuteAgent(agent) => {
                let agent = normalize_agent_id(&agent);
                if !self.muted_agents.remove(&agent) {
                    self.muted_agents.insert(agent);
                }
            }
            Message::CollapseAllThinking => {
                self.thinking_expand_all = false;
                self.expanded_thinking_id = None;
            }
            Message::ExpandAllThinking => {
                self.thinking_expand_all = true;
            }
            Message::ToggleThinkingAll => {
                self.toggle_thinking_all();
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
                // Reduced-motion has no reveal window: live arrivals render at
                // their full current content instantly.
                let reveal_window = match self.revealed_chars {
                    Some((rid, n))
                        if !self.reduced_motion && rid == *id && *streaming && n < char_count =>
                    {
                        Some(n)
                    }
                    _ => None,
                };
                // Render from the cached parse (always populated for assistant
                // entries); the budget maps the reveal frontier onto the
                // document's visible-text units so every tick re-drives an
                // already-parsed event stream instead of re-parsing markdown.
                // The raw `markdown::render` fallback is purely defensive.
                // First-token emphasis (Score prototype #6): while the entry's
                // ~384 ms window is open (and reduced-motion is off), its
                // first token renders bold, then settles to normal weight.
                // The uncached `markdown::render` fallback is purely defensive
                // (the cache is always populated for assistant entries) and
                // renders without emphasis.
                let emphasis = self.first_token_emphasis(*id);
                let md: Element<'_, Message> = match self.md_docs.get(id) {
                    Some(doc) => {
                        let budget = reveal_window.map(|n| n.min(doc.total_units));
                        doc.render_upto(
                            budget,
                            |code| Message::CopyCode(code.to_string()),
                            palette.surface_variant,
                            palette.text_muted,
                            palette.primary,
                            emphasis,
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
                // never sits after the complete message. During a paragraph
                // handoff hold (Baton prototype #5) the cursor holds visible
                // regardless of the 500 ms blink phase: the brief hold is the
                // "continuing" cue between paragraphs.
                let reveal_complete =
                    self.revealed_chars.is_some_and(|(rid, n)| rid == *id && n >= char_count);
                let in_handoff = self.handoff_hold.is_some_and(|(hold_id, _)| hold_id == *id);
                let body: Element<'a, Message> = if *streaming
                    && !self.reduced_motion
                    && (self.streaming_cursor_visible || in_handoff)
                    && !reveal_complete
                {
                    row![md, text("▌").size(14).color(palette.text_muted)].spacing(2).into()
                } else {
                    md
                };
                // Compact timestamp below the assistant message block. Shown
                // immediately for a streaming entry (timestamped at its first
                // chunk), not hidden while the reveal is running.
                let mut block = column![].spacing(4).width(Length::Fill);
                // Line-wipe entrance cue (Score prototype #7): a 2px rule at
                // the top of each new assistant entry wipes 0→full width over
                // `LINE_WIPE_TICKS` TypingTicks (~128 ms), then settles into a
                // muted full-width hairline (the "new turn" signature) that
                // persists across run finalize and navigation — reduced-motion
                // skips the cue entirely (no rule, instant render).
                if let Some(step) = self.line_wipe_step(*id) {
                    block = block.push(line_wipe_rule(step, palette));
                } else if self.line_wipe_settled.contains(id) {
                    block = block.push(settled_line_wipe_rule(palette));
                }
                block = block.push(body);
                if let Some(ts_line) = compact_timestamp_line(created_at, palette) {
                    block = block.push(ts_line);
                }
                block.into()
            }
            ChatEntry::Thinking { content, collapsed, id, created_at, finished_at, .. } => {
                let char_count = content.chars().count();
                // Thinking previews render their full content as it arrives:
                // a short entrance fade (see `add_thinking`) — no typewriter
                // reveal, so a cold start never reads as a blank colored
                // block that fills over seconds. Collapsed entries show the
                // stub line instead.
                let preview = if *collapsed {
                    format!("Iteration details hidden ({char_count} chars)")
                } else {
                    content.clone()
                };
                // Subtle shimmer while the thinking phase is still open: the
                // preview color pulses between palette tokens (never
                // hard-coded RGB). Driven by the `TypingTick` shimmer phase.
                // Reduced-motion freezes the cue — the preview renders at
                // the static muted weight. The entrance fade applies on top
                // either way, then the container fades in with the preview.
                let fade = entrance_alpha(self.entrance_ticks.get(id).copied());
                let preview_color = if finished_at.is_none() && !self.reduced_motion {
                    shimmer_color(palette, self.shimmer_phase)
                } else {
                    palette.text_muted
                };
                let preview_color = with_alpha(preview_color, fade);
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
                // Tool rows stay one line with a bounded first-line preview:
                // a full `detail` dump (multi-KB path lists, JSON blobs) made
                // the transcript sprawl and hid later entries. The Tool Log
                // modal holds the full record — the row routes there.
                let preview_limit = 48;
                let first_line = detail.lines().next().unwrap_or("");
                let snippet: String = first_line.chars().take(preview_limit).collect();
                let label = if first_line.is_empty() {
                    format!("[Tool] {}", tool_name)
                } else if snippet == first_line {
                    format!("[Tool] {} — {}", tool_name, snippet)
                } else {
                    format!("[Tool] {} — {}…", tool_name, snippet)
                };
                let tool_button: Element<'_, Message> = button(
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
                .into();
                // Provenance rail (Blueprint `‖ ok`): tiny policy +
                // reversibility caption under the tool row, palette only.
                let rail = provenance_rail(status, tool_name);
                column![tool_button, text(rail).size(11).color(muted),].spacing(2).into()
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

    /// `/thinking` toggle for the current movement: collapse everything when
    /// any bucket is open, otherwise expand all buckets.
    pub fn toggle_thinking_all(&mut self) {
        if self.thinking_expand_all || self.expanded_thinking_id.is_some() {
            self.thinking_expand_all = false;
            self.expanded_thinking_id = None;
        } else {
            self.thinking_expand_all = true;
        }
    }

    /// Per-agent mute filter bar (V2): one toggle chip per thinking bucket.
    /// Muted agents show dimmed with a `+` marker; hiding is chat-only.
    fn thinking_filter_bar<'a>(
        &'a self,
        palette: &'a crate::theme::Palette,
        order: &[String],
        buckets: &HashMap<String, Vec<usize>>,
    ) -> Element<'a, Message> {
        let mut bar = row![
            text("‖ thinking").size(11).color(palette.text_muted),
            text("/thinking toggles all").size(11).color(palette.text_muted),
        ]
        .spacing(8)
        .align_y(Alignment::Center);
        for agent in order {
            let count = buckets.get(agent).map(Vec::len).unwrap_or(0);
            let muted = self.muted_agents.contains(agent);
            let label = if muted {
                format!("[+ {agent}] muted ×{count}")
            } else {
                format!("[‖ {agent}] ×{count}")
            };
            let color = if muted {
                palette.text_muted
            } else {
                crate::theme::agent_color_from_id(agent, palette).unwrap_or(palette.text_muted)
            };
            bar = bar.push(
                button(text(label).size(11).color(color))
                    .style(crate::ui::button::secondary)
                    .on_press(Message::ToggleMuteAgent(agent.clone())),
            );
        }
        container(bar).padding([4, 8]).into()
    }

    /// Per-turn agent identity for an assistant block: a compact badge +
    /// role label above the reply. The assistant entry stores no agent id
    /// (single-agent transcript), so the label derives from the most recent
    /// thinking bucket's agent — view-only attribution, never persisted. The
    /// block's own line-wipe rule (or its settled marker) doubles as the
    /// hairline underneath.
    fn agent_turn_header<'a>(
        &'a self,
        agent: String,
        palette: &'a crate::theme::Palette,
    ) -> Element<'a, Message> {
        let color =
            crate::theme::agent_color_from_id(&agent, palette).unwrap_or(palette.text_muted);
        row![text("◆").size(11).color(color), text(agent).size(11).color(palette.text_muted)]
            .spacing(4)
            .align_y(Alignment::Center)
            .padding(iced::Padding::ZERO.top(2).right(8).bottom(0).left(8))
            .into()
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
                    // Startup signature wordmark: letterspaced via spaces (Iced
                    // has no letter-spacing), TypeScale display + palette.text;
                    // subline in text_muted. Static text — no entrance ticks,
                    // so reduced-motion renders identically (no fade to skip).
                    text("C O N C E R T O").size(theme.type_scale.display).color(palette.text),
                    text("local-first • policy-governed")
                        .size(theme.type_scale.caption)
                        .color(palette.text_muted),
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
    ) -> Element<'a, Message> {
        let mut phases = column![].spacing(0).width(Length::Fill);
        // Tool calls are NOT re-listed here: the chat column renders each one
        // as its own compact entry, so a timeline re-list would double every
        // tool row (the "tool quarantine" duplication — the timeline stays a
        // phase overview only).
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

        // Message list — render with the per-agent thinking accordion (V2).
        let mut col = column![].spacing(4).padding(8);
        let has_timeline =
            agent_graph.has_multi_agent_activity && !agent_graph.model.nodes.is_empty();
        let latest_user_index =
            self.entries.iter().rposition(|entry| matches!(entry, ChatEntry::User { .. }));
        if has_timeline && latest_user_index.is_none() {
            col = col.push(self.orchestration_timeline(agent_graph, palette));
        }
        // Full per-agent buckets over ALL thinking entries (not just
        // consecutive runs): one panel per agent in first-appearance order,
        // emitted at the agent's first entry. Muted agents are skipped here
        // (view-only; the WAL/transcript still hold everything). The filter
        // bar always renders once the chat has entries, so the composer's
        // position under it stays fixed — no layout jump when the first
        // bucket appears (the no-entries case returned above).
        let (bucket_order, buckets) = thinking_buckets(&self.entries);
        col = col.push(self.thinking_filter_bar(palette, &bucket_order, &buckets));
        let mut emitted_buckets = HashSet::new();
        // Per-turn agent attribution for assistant entries: the assistant
        // entry carries no agent id (single-agent transcript), so the header
        // derives the speaker from the most recent thinking bucket's agent,
        // falling back to the generic "agent" role. View-only — never stored.
        let mut last_turn_agent: Option<String> = None;
        let mut idx = 0;
        while idx < self.entries.len() {
            let bucket_key = match &self.entries[idx] {
                ChatEntry::Thinking { agent, content, .. } => {
                    Some(thinking_agent_key(agent, content))
                }
                _ => None,
            };
            if let Some(agent) = bucket_key {
                last_turn_agent = Some(agent.clone());
                if self.muted_agents.contains(&agent) {
                    idx += 1;
                    continue;
                }
                if emitted_buckets.insert(agent.clone()) {
                    let indices = buckets.get(&agent).cloned().unwrap_or_default();
                    let first_id = indices
                        .first()
                        .and_then(|&i| match &self.entries[i] {
                            ChatEntry::Thinking { id, .. } => Some(*id),
                            _ => None,
                        })
                        .unwrap_or(0);
                    let headline = thinking_headline_from_entries(&self.entries, &indices);
                    let group = ThinkingGroup {
                        agent_id: agent,
                        indices,
                        first_id,
                        expanded: self.thinking_expand_all
                            || self.expanded_thinking_id == Some(first_id),
                        headline,
                    };
                    col = col.push(render_thinking_group(
                        &group,
                        &self.entries,
                        palette,
                        &self.entrance_ticks,
                        self.reduced_motion,
                        self.shimmer_phase,
                    ));
                }
                idx += 1;
            } else {
                if let ChatEntry::Assistant { .. } = &self.entries[idx] {
                    col = col.push(self.agent_turn_header(
                        last_turn_agent.clone().unwrap_or_else(|| "agent".to_string()),
                        palette,
                    ));
                }
                col = col.push(self.render_entry(&self.entries[idx], palette));
                if has_timeline && latest_user_index == Some(idx) {
                    col = col.push(self.orchestration_timeline(agent_graph, palette));
                }
                idx += 1;
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

/// Bucket key for a thinking entry: the normalized stored agent id, falling
/// back to the normalized legacy `[agent]` content prefix for pre-V2
/// entries. Content text itself is never modified.
fn thinking_agent_key(agent: &str, content: &str) -> String {
    if !agent.is_empty() {
        return normalize_agent_id(agent);
    }
    let raw = content
        .split_once("] ")
        .map(|(p, _)| p.strip_prefix('[').unwrap_or(p).to_string())
        .unwrap_or_default();
    normalize_agent_id(&raw)
}

/// Full per-agent bucketing over every `Thinking` entry: returns agents in
/// first-appearance order plus `agent -> entry indices`.
fn thinking_buckets(entries: &[ChatEntry]) -> (Vec<String>, HashMap<String, Vec<usize>>) {
    let mut order = Vec::new();
    let mut buckets: HashMap<String, Vec<usize>> = HashMap::new();
    for (idx, entry) in entries.iter().enumerate() {
        if let ChatEntry::Thinking { agent, content, .. } = entry {
            let key = thinking_agent_key(agent, content);
            if !buckets.contains_key(&key) {
                order.push(key.clone());
            }
            buckets.entry(key).or_default().push(idx);
        }
    }
    (order, buckets)
}

/// Strip a legacy `[agent]` prefix from content for headline display.
fn strip_agent_prefix(content: &str) -> &str {
    content.split_once("] ").map(|(_, rest)| rest).unwrap_or(content)
}

/// Digest for a thinking bucket: the first line of the latest `Headline`
/// entry when one exists, else the latest `Detail` line. `LowLevel` entries
/// never feed the digest (they render only in the AgentGraph logs, and the
/// expanded accordion skips them too); a bucket with neither falls back to
/// the generic label.
fn thinking_headline_from_entries(entries: &[ChatEntry], indices: &[usize]) -> String {
    let headline_content = indices
        .iter()
        .rev()
        .filter_map(|&idx| match &entries[idx] {
            ChatEntry::Thinking { content, kind: ThinkingKind::Headline, .. } => {
                Some(content.as_str())
            }
            _ => None,
        })
        .next();
    let last_content = headline_content
        .or_else(|| {
            indices
                .iter()
                .rev()
                .filter_map(|&idx| match &entries[idx] {
                    ChatEntry::Thinking { content, kind: ThinkingKind::Detail, .. } => {
                        Some(content.as_str())
                    }
                    _ => None,
                })
                .next()
        })
        .unwrap_or("");
    let first_line = strip_agent_prefix(last_content).lines().next().unwrap_or("").trim();
    if first_line.is_empty() {
        return "Thinking…".to_string();
    }
    for verb in &["Tuning", "Scoring", "Rehearsing", "Coda", "Starting"] {
        if let Some(rest) = first_line.strip_prefix(verb) {
            let tail = rest.trim_start();
            if tail.is_empty() {
                return verb.to_string();
            }
            let token = tail.split_whitespace().next().unwrap_or("");
            return format!("{verb} {token}");
        }
    }
    let truncated: String = first_line.chars().take(48).collect();
    if truncated.len() < first_line.len() {
        format!("{truncated}…")
    } else {
        truncated
    }
}

/// Render a thinking bucket as a collapsible accordion panel. Collapsed
/// shows the agent badge + digest headline + count; expanded shows each
/// `Headline`/`Detail` entry (`LowLevel` never reaches chat — it is routed
/// to the AgentGraph logs in `runtime.rs`, and is skipped here defensively).
/// Entries render at full content with their own entrance fade and live
/// shimmer (fade-only — see `add_thinking`); reduced-motion renders them
/// static and instantly opaque.
fn render_thinking_group<'a>(
    group: &ThinkingGroup,
    entries: &'a [ChatEntry],
    palette: &'a crate::theme::Palette,
    entrance_ticks: &'a std::collections::HashMap<EntryId, u8>,
    reduced_motion: bool,
    shimmer_phase: u32,
) -> Element<'a, Message> {
    let count = group.indices.len();

    let agent_color =
        crate::theme::agent_color_from_id(&group.agent_id, palette).unwrap_or(palette.text_muted);

    if !group.expanded {
        // Collapsed: single header row with agent badge + headline + count.
        let label =
            format!("[{}] ‖ {headline} ×{count}", group.agent_id, headline = group.headline);
        let toggle_btn = button(text("▶").size(11))
            .style(crate::ui::button::secondary)
            .on_press(Message::ToggleEntry(group.first_id));
        container(row![toggle_btn, text(label).size(12).color(agent_color)].spacing(4))
            .padding([6, 8])
            .style(|_theme| container::Style {
                background: Some(Background::Color(palette.surface_variant)),
                border: Border { radius: Radius::from(8.0), ..Default::default() },
                ..container::Style::default()
            })
            .into()
    } else {
        // Expanded: each entry renders its full content with a per-entry
        // entrance fade and (while the phase is still open) a live shimmer.
        let mut col = column![].spacing(2);
        let header = row![
            button(text("▼").size(11))
                .style(crate::ui::button::secondary)
                .on_press(Message::ToggleEntry(group.first_id)),
            text(format!("[{}] ‖ {headline} ×{count}", group.agent_id, headline = group.headline))
                .size(12)
                .color(agent_color),
        ]
        .spacing(4);
        col = col.push(header);
        for &idx in &group.indices {
            if let ChatEntry::Thinking { id, content, kind, finished_at, .. } = &entries[idx] {
                if *kind == ThinkingKind::LowLevel {
                    continue;
                }
                let fade = entrance_alpha(entrance_ticks.get(id).copied());
                let color = if finished_at.is_none() && !reduced_motion {
                    with_alpha(shimmer_color(palette, shimmer_phase), fade)
                } else {
                    with_alpha(palette.text_muted, fade)
                };
                col = col
                    .push(container(text(content.clone()).size(13).color(color)).padding([4, 8]));
            }
        }
        container(col)
            .padding([6, 8])
            .width(Length::Fill)
            .style(|_theme| container::Style {
                background: Some(Background::Color(palette.surface_variant)),
                border: Border { radius: Radius::from(8.0), ..Default::default() },
                ..container::Style::default()
            })
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

/// Color for the live thinking shimmer at `shimmer_phase`: pulses between
/// the muted text token and the base text token over `SHIMMER_PERIOD` ticks
/// each way (triangle wave). Palette colors only — never hard-coded RGB.
fn shimmer_color(palette: &crate::theme::Palette, shimmer_phase: u32) -> Color {
    let phase = shimmer_phase % (2 * SHIMMER_PERIOD);
    let wave = if phase < SHIMMER_PERIOD {
        phase as f32 / SHIMMER_PERIOD as f32
    } else {
        (2 * SHIMMER_PERIOD - phase) as f32 / SHIMMER_PERIOD as f32
    };
    lerp_color(palette.text_muted, palette.text, 0.5 + 0.5 * wave)
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

/// Alpha for the line-wipe rule at `step` (0-based elapsed ticks): ramps in
/// equal per-tick increments from faint to near-opaque. The rule renders at
/// full width the whole time (see `line_wipe_rule`), so this opacity ramp is
/// the progress cue itself — width never animates. Palette colors only;
/// alpha modulation only, no transform.
fn line_wipe_alpha(step: u8) -> f32 {
    let done = f32::from(step.min(LINE_WIPE_TICKS.saturating_sub(1))) + 1.0;
    0.25 + 0.55 * (done / f32::from(LINE_WIPE_TICKS))
}

/// Top-rule element for the new-assistant line-wipe cue (prototype #7):
/// `step` is the elapsed tick count (0-based) since the entry was inserted.
/// A full-width 2px rule whose color lerps `palette.border` → `palette.accent`
/// while `line_wipe_alpha` ramps the opacity in — the width never animates,
/// so the chat column's right edge stays put (the diagnosed right-margin
/// pull when a partial-width bar collapsed mid-wipe). No transform; alpha
/// modulation only, same contract as `settled_line_wipe_rule`.
fn line_wipe_rule<'a>(step: u8, palette: &'a crate::theme::Palette) -> Element<'a, Message> {
    let total = u16::from(LINE_WIPE_TICKS);
    let done = u16::from(step.min(LINE_WIPE_TICKS.saturating_sub(1))) + 1;
    let progress = f32::from(done) / f32::from(total);
    let color =
        with_alpha(lerp_color(palette.border, palette.accent, progress), line_wipe_alpha(step));
    container(iced::widget::space::horizontal())
        .width(Length::Fill)
        .height(Length::Fixed(2.0))
        .style(move |_theme: &iced::Theme| container::Style {
            background: Some(Background::Color(color)),
            ..container::Style::default()
        })
        .into()
}

/// Settled top-rule marker for an assistant entry whose line-wipe animation
/// has finished (Score seasoning, prototype #7): the same 2px rule at full
/// width, muted to a quiet hairline so the "new turn" signature persists
/// without drawing attention. Static — never animates, so it must not keep
/// the 16 ms `TypingTick` alive by itself. Palette colors only; alpha
/// modulation only, no transform (same contract as `line_wipe_rule`).
fn settled_line_wipe_rule<'a>(palette: &'a crate::theme::Palette) -> Element<'a, Message> {
    let color = with_alpha(lerp_color(palette.border, palette.accent, 0.35), 0.45);
    container(iced::widget::space::horizontal())
        .width(Length::Fill)
        .height(Length::Fixed(2.0))
        .style(move |_theme: &iced::Theme| container::Style {
            background: Some(Background::Color(color)),
            ..container::Style::default()
        })
        .into()
}

/// Whether the reveal frontier crossed a paragraph boundary between `from`
/// (exclusive) and `to` (inclusive), measured in characters. A boundary is a
/// blank line — an empty or whitespace-only line terminated by `\n` with more
/// content following it — which covers `\n\n`, `\r\n\r\n`, and whitespace-only
/// separator lines. Single-paragraph text (and a trailing newline run with no
/// content after it) yields `false`, so the handoff cue falls back to no cue.
fn crosses_paragraph_boundary(content: &str, from: usize, to: usize) -> bool {
    if to <= from {
        return false;
    }
    let mut offset: usize = 0;
    let mut lines = content.split_inclusive('\n').peekable();
    while let Some(line) = lines.next() {
        let line_len = line.chars().count();
        let line_end = offset.saturating_add(line_len);
        // Only a newline-terminated blank line with content after it marks a
        // "continuing" handoff; a final newline run is just the message end.
        let is_separator = line.contains('\n') && line.trim().is_empty() && lines.peek().is_some();
        if is_separator && line_end > from && line_end <= to {
            return true;
        }
        offset = line_end;
    }
    false
}

/// Composer placeholder (V3 quick win): advertises the `/thinking`
/// accordion toggle with zero behavior change.
const COMPOSER_PLACEHOLDER: &str = "Type a message... (/thinking toggles thinking)";

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
    let txt = text_input(COMPOSER_PLACEHOLDER, input)
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

    // `/thinking` accordion toggle: the composer's real button (mirrors the
    // filter-bar caption; the `/thinking` text command still works too).
    let thinking_toggle: Element<'a, Message> = tooltip::Tooltip::new(
        button(text("‖ thinking").size(12))
            .style(crate::ui::button::secondary)
            .on_press(Message::ToggleThinkingAll),
        container(text("Expand/collapse every thinking trace (/thinking)").size(12)).padding(8),
        tooltip::Position::Top,
    )
    .gap(4)
    .into();

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

    let bar =
        row![new_session_btn, model_picker, txt, send, toggle_group, fast_group, thinking_toggle,]
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
        state.add_thinking("coder", "First".to_string(), ThinkingKind::Detail);
        state.add_thinking("coder", "Second".to_string(), ThinkingKind::Detail);
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
    fn line_wipe_seeds_and_completes_over_eight_ticks() {
        let mut state = State::new();
        let _ = state.update(Message::AddAssistant("hello".into()));
        let id = match state.entries().last() {
            Some(ChatEntry::Assistant { id, .. }) => *id,
            _ => panic!("expected an assistant entry"),
        };
        assert_eq!(state.line_wipe_step(id), Some(0));

        // Deterministic: exactly `LINE_WIPE_TICKS` ticks, then the animated
        // wipe is done and the entry keeps its muted settled marker (the
        // "new turn" signature) instead of the rule settling away entirely.
        for expected in 1..=LINE_WIPE_TICKS {
            let _ = state.update(Message::TypingTick);
            if expected < LINE_WIPE_TICKS {
                assert_eq!(state.line_wipe_step(id), Some(expected));
            } else {
                assert_eq!(state.line_wipe_step(id), None, "the animated wipe has finished");
                assert!(
                    state.line_wipe_settled.contains(&id),
                    "the completed wipe leaves its settled top-rule marker"
                );
            }
        }
    }

    #[test]
    fn settled_wipe_marker_is_static_and_never_drives_the_tick() {
        // The settled hairline is a static render, not an animation: once it
        // exists and every other cue is done, `is_revealing` must be false so
        // the 16 ms `TypingTick` subscription stops.
        let mut state = State::new();
        let _ = state.update(Message::AddAssistant("final reply".into()));
        let id = match state.entries().last() {
            Some(ChatEntry::Assistant { id, .. }) => *id,
            _ => panic!("expected an assistant entry"),
        };
        // AddAssistant auto-finalizes the reveal once it reaches the end; drive
        // every window out (emphasis is the longest).
        for _ in 0..FIRST_TOKEN_EMPHASIS_TICKS {
            let _ = state.update(Message::TypingTick);
        }
        assert!(state.line_wipe_settled.contains(&id), "the wipe settled into its marker");
        assert!(state.line_wipe_ticks.is_empty());
        assert!(state.first_token_ticks.is_empty());
        assert!(
            !state.is_revealing(),
            "a settled marker is static: no typing-tick work may remain"
        );
    }

    #[test]
    fn finalize_converges_in_flight_and_never_played_wipes_into_settled_markers() {
        // A run finishing mid-wipe (or before any tick played) must still leave
        // the "new turn" marker on the reply: finalize settles the wipe instead
        // of erasing all trace of the cue — the post-run signature.
        let mut via_run = State::new();
        let _ = via_run.update(Message::AddAssistant("final reply".into()));
        let id = match via_run.entries().last() {
            Some(ChatEntry::Assistant { id, .. }) => *id,
            _ => panic!("expected an assistant entry"),
        };
        assert!(
            !via_run.line_wipe_ticks.is_empty(),
            "a final reply seeds its wipe before any tick plays"
        );
        assert!(
            via_run.reveal_autofinish && via_run.is_revealing(),
            "a final reply seeds its autofinish reveal before any tick plays"
        );
        via_run.finalize_run();
        // Run finalize must NOT dump an in-flight autofinish reveal to full
        // text (the "instant dump" defect): the typewriter continues past the
        // run boundary (the Completion chip trails it), then the tracked
        // entry auto-finalizes.
        assert!(
            via_run.reveal_autofinish && via_run.is_revealing(),
            "an in-flight autofinish reveal survives the run boundary"
        );
        for _ in 0..FIRST_TOKEN_EMPHASIS_TICKS + 20 {
            let _ = via_run.update(Message::TypingTick);
        }
        assert!(!via_run.is_revealing(), "the preserved reveal completes fully and stops the tick");
        assert!(
            matches!(
                via_run.entries().iter().find(|e| {
                    matches!(e, ChatEntry::Assistant { id: eid, .. } if *eid == id)
                }),
                Some(ChatEntry::Assistant { streaming: false, content, .. })
                    if content == "final reply"
            ),
            "the tracked reply auto-finalizes with its full content after the reveal"
        );
        assert!(
            via_run.line_wipe_settled.contains(&id),
            "the reply keeps its settled marker after the run"
        );

        // Mid-run navigation boundary (`finalize_streaming`) behaves the same,
        // including a wipe that had a few ticks to play.
        let mut via_nav = State::new();
        via_nav.update_last_assistant("partial stream".to_string());
        let nav_id = match via_nav.entries().last() {
            Some(ChatEntry::Assistant { id, .. }) => *id,
            _ => panic!("expected an assistant entry"),
        };
        let _ = via_nav.update(Message::TypingTick);
        assert_eq!(via_nav.line_wipe_step(nav_id), Some(1));
        via_nav.finalize_streaming();
        assert!(
            via_nav.line_wipe_settled.contains(&nav_id),
            "navigating away mid-cue still settles the marker"
        );
    }

    #[test]
    fn reduced_motion_clears_settled_wipe_markers() {
        // A11y: reduced-motion must remove even the static settled markers, so
        // no cue — animated or settled — survives the toggle.
        let mut state = State::new();
        state.update_last_assistant("streamed text".to_string());
        let id = match state.entries().last() {
            Some(ChatEntry::Assistant { id, .. }) => *id,
            _ => panic!("expected an assistant entry"),
        };
        for _ in 0..LINE_WIPE_TICKS {
            let _ = state.update(Message::TypingTick);
        }
        assert!(state.line_wipe_settled.contains(&id), "the wipe settled into its marker");
        state.set_reduced_motion(true);
        assert!(
            state.line_wipe_settled.is_empty(),
            "reduced-motion removes the static settled markers"
        );
    }

    #[test]
    fn live_multi_paragraph_growth_keeps_reveal_and_handoff_alive() {
        // The live streaming path (runtime `AssistantMessage`) types, it must
        // not snap the frontier to the new length or drop the handoff cue:
        // multi-paragraph output keeps the reveal driving behind arriving text.
        let mut state = State::new();
        state.update_last_assistant("first paragraph\n\nsecond paragraph content".to_string());
        let id = match state.entries().last() {
            Some(ChatEntry::Assistant { id, .. }) => *id,
            _ => panic!("expected an assistant entry"),
        };
        assert_eq!(
            state.revealed_chars,
            Some((id, 0)),
            "a fresh live stream seeds a reveal window"
        );
        let _ = state.update(Message::TypingTick);
        let _ = state.update(Message::TypingTick);
        assert_eq!(state.revealed_chars, Some((id, 16)));
        // Third tick crosses the blank-line boundary: the handoff hold opens.
        let _ = state.update(Message::TypingTick);
        assert_eq!(
            state.handoff_hold.map(|(_, remaining)| remaining),
            Some(HANDOFF_HOLD_TICKS),
            "crossing a paragraph boundary opens the handoff hold"
        );
        let frozen = state.revealed_chars;

        // More content arrives mid-hold: the frontier must not jump to the new
        // length (live growth is typed, not instant) and the in-flight hold is
        // preserved.
        state.update_last_assistant(
            "first paragraph\n\nsecond paragraph content\n\nthird paragraph content here"
                .to_string(),
        );
        assert_eq!(
            state.revealed_chars, frozen,
            "arriving content must not snap the frontier to full"
        );
        assert!(state.handoff_hold.is_some(), "an open hold is preserved across growth");

        // Drain the hold: the frozen frontier resumes and keeps advancing a tick
        // at a time behind the arriving content.
        for _ in 0..HANDOFF_HOLD_TICKS {
            let _ = state.update(Message::TypingTick);
        }
        assert_eq!(state.handoff_hold, None, "the hold drains in exactly the constant");
        assert!(state.is_revealing(), "the reveal keeps driving after the hold clears");
        assert!(
            state.revealed_chars.is_some_and(|(rid, n)| rid == id && n > 16),
            "the resumed frontier advances beyond the frozen position"
        );
    }

    #[test]
    fn line_wipe_seeds_on_live_streaming_entry() {
        let mut state = State::new();
        state.update_last_assistant("streaming".to_string());
        let id = match state.entries().last() {
            Some(ChatEntry::Assistant { id, .. }) => *id,
            _ => panic!("expected an assistant entry"),
        };
        assert_eq!(state.line_wipe_step(id), Some(0));
    }

    #[test]
    fn line_wipe_reduced_motion_skips_instantly() {
        let mut state = State::new();
        state.set_reduced_motion(true);
        let _ = state.update(Message::AddAssistant("hello".into()));
        assert!(state.line_wipe_ticks.is_empty());
        // Enabling mid-wipe also settles instantly — and removes any settled
        // marker too, so reduced-motion renders every entry without a cue.
        let mut live = State::new();
        live.update_last_assistant("streaming".to_string());
        let id = match live.entries().last() {
            Some(ChatEntry::Assistant { id, .. }) => *id,
            _ => panic!("expected an assistant entry"),
        };
        assert!(!live.line_wipe_ticks.is_empty());
        for _ in 0..LINE_WIPE_TICKS {
            let _ = live.update(Message::TypingTick);
        }
        assert!(live.line_wipe_settled.contains(&id), "the wipe settled first");
        live.set_reduced_motion(true);
        assert!(live.line_wipe_ticks.is_empty());
        assert!(
            live.line_wipe_settled.is_empty(),
            "reduced-motion clears even the static settled markers"
        );
    }

    #[test]
    fn line_wipe_alpha_ramps_monotonically_within_bounds() {
        // Fallback fade-in contract: equal alpha steps, always low alpha.
        let mut prev = 0.0;
        for step in 0..LINE_WIPE_TICKS {
            let alpha = line_wipe_alpha(step);
            assert!(
                alpha > prev && (0.0..=1.0).contains(&alpha),
                "step {step} alpha {alpha} must increase within 0..=1"
            );
            prev = alpha;
        }
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
        state.add_thinking("coder", "thinking...".to_string(), ThinkingKind::Detail);
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
        state.add_thinking("coder", "planning...".to_string(), ThinkingKind::Detail);
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
        state.add_thinking("coder", "a".to_string(), ThinkingKind::Detail);
        state.add_thinking("coder", "b".to_string(), ThinkingKind::Detail);

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
        state.add_thinking("coder", "planning a".to_string(), ThinkingKind::Detail);
        // A second agent's thought lands in its own bucket entry; toggling
        // the accordion never splits entries, so both stay open.
        state.add_thinking("reviewer", "planning b".to_string(), ThinkingKind::Detail);
        let first_id = match &state.entries()[0] {
            ChatEntry::Thinking { id, .. } => *id,
            _ => panic!("Expected a Thinking entry"),
        };
        let _ = state.update(Message::ToggleEntry(first_id));
        assert_eq!(state.entries().len(), 2);
        // The assistant chunk closes only the *trailing* thinking entry; the
        // earlier bucket entry is out of reach of the trailing-only helper.
        state.update_last_assistant("partial answer".to_string());
        match &state.entries()[0] {
            ChatEntry::Thinking { finished_at, .. } => {
                assert!(
                    finished_at.is_none(),
                    "earlier bucket thinking must stay open until finalize_run"
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
        state.add_thinking("coder", "planning...".to_string(), ThinkingKind::Detail);
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
        // streaming, nothing is left to reveal. First-token emphasis keeps
        // the tick alive until its ~384 ms window expires.
        assert_eq!(state.revealed_chars, Some((id, 20)));
        assert!(state.first_token_emphasis(id), "a fresh turn emphasizes its first token");
        assert!(state.is_revealing());
        // Drive the emphasis window out: emphasis settles and, with the reveal
        // already complete, the tick stops.
        for _ in 3..FIRST_TOKEN_EMPHASIS_TICKS {
            let _ = state.update(Message::TypingTick);
        }
        assert!(!state.first_token_emphasis(id), "emphasis settles ~384 ms after the turn started");
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
    fn final_reply_reveal_survives_a_trailing_completion_entry() {
        // Normal desktop runs emit no mid-run `AssistantMessage`, so the final
        // reply enters through `AddAssistant` and is immediately followed by
        // the run's `Completion` chip (`set_run_completion`). The reveal must
        // track its entry by id — not by tail position — or the appended
        // Completion would orphan the window on the first tick and the reply
        // would render as an instant dump.
        let mut state = State::new();
        let _ = state.update(Message::AddAssistant("0123456789abcdefghij".to_string()));
        let id = match state.entries().last() {
            Some(ChatEntry::Assistant { id, .. }) => *id,
            _ => panic!("expected an assistant entry"),
        };
        state.set_run_completion(true, true, vec!["src/main.rs".into()], Some("project".into()));
        assert!(
            matches!(state.entries().last(), Some(ChatEntry::Completion { .. })),
            "the completion chip trails the final reply"
        );
        assert_eq!(state.revealed_chars, Some((id, 0)), "the final reply seeds a reveal");
        assert!(state.is_revealing(), "the reveal survives a trailing Completion entry");

        // The reveal must advance 8 chars/tick instead of snapping to full:
        // the trailing Completion must not drop the window on the first tick.
        let _ = state.update(Message::TypingTick);
        assert_eq!(state.revealed_chars, Some((id, 8)));
        let _ = state.update(Message::TypingTick);
        assert_eq!(state.revealed_chars, Some((id, 16)));
        assert!(state.is_revealing(), "mid-reveal keeps the typing tick alive");

        // Once the frontier reaches the content end the autofinish fires: the
        // entry stops streaming and the window clears, exactly as it would
        // without the trailing chip.
        let _ = state.update(Message::TypingTick);
        assert_eq!(state.revealed_chars, None, "the reveal finishes at the content end");
        assert!(
            matches!(
                state
                    .entries()
                    .iter()
                    .find(|e| { matches!(e, ChatEntry::Assistant { id: eid, .. } if *eid == id) }),
                Some(ChatEntry::Assistant { streaming: false, .. })
            ),
            "the final reply auto-finalizes once fully revealed"
        );
        assert!(
            matches!(
                state.entries().iter().find(|e| {
                    matches!(e, ChatEntry::Assistant { id: eid, .. } if *eid == id)
                }),
                Some(ChatEntry::Assistant { content, .. }) if content == "0123456789abcdefghij"
            ),
            "the full content stays intact after the reveal"
        );

        // The line wipe still plays and settles to its static marker (the
        // "new turn" signature), and every window drains so the 16 ms tick
        // subscription stops.
        for _ in 0..FIRST_TOKEN_EMPHASIS_TICKS {
            let _ = state.update(Message::TypingTick);
        }
        assert!(
            state.line_wipe_settled.contains(&id),
            "the wipe settles to its muted marker despite the trailing chip"
        );
        assert!(!state.is_revealing(), "no animation work remains after all windows settle");
    }

    #[test]
    fn final_reply_and_completion_render_instantly_under_reduced_motion() {
        // A11y: reduced-motion must never seed the reveal or its cues even
        // when the run's Completion chip trails the final reply.
        let mut state = State::new();
        state.set_reduced_motion(true);
        let _ = state.update(Message::AddAssistant("0123456789abcdefghij".to_string()));
        state.set_run_completion(true, true, vec![], None);

        assert_eq!(state.revealed_chars, None, "reduced-motion never seeds a reveal");
        assert!(state.line_wipe_ticks.is_empty(), "no wipe under reduced-motion");
        assert!(state.first_token_ticks.is_empty(), "no first-token emphasis");
        assert!(
            matches!(
                state.entries().iter().find(|e| matches!(e, ChatEntry::Assistant { .. })),
                Some(ChatEntry::Assistant { streaming: false, .. })
            ),
            "the final reply is never streaming under reduced-motion"
        );
        assert!(!state.is_revealing(), "the transcript renders instantly");
    }

    #[test]
    fn first_token_emphasis_survives_same_turn_chunks_without_reseed() {
        let mut state = State::new();
        state.update_last_assistant("hello world".to_string());
        let id = match state.entries().last() {
            Some(ChatEntry::Assistant { id, .. }) => *id,
            _ => panic!("Expected an assistant entry"),
        };
        for _ in 0..5 {
            let _ = state.update(Message::TypingTick);
        }
        // A streaming continuation on the same entry must not restart the
        // ~384 ms window: the clock runs from the turn start.
        state.update_last_assistant("hello world, more".to_string());
        assert!(state.first_token_emphasis(id));
        for _ in 0..FIRST_TOKEN_EMPHASIS_TICKS - 5 {
            let _ = state.update(Message::TypingTick);
        }
        assert!(
            !state.first_token_emphasis(id),
            "window settles exactly {FIRST_TOKEN_EMPHASIS_TICKS} ticks after the turn started, not after the last chunk"
        );
    }

    #[test]
    fn reduced_motion_skips_first_token_emphasis() {
        let mut state = State::new();
        state.set_reduced_motion(true);
        state.update_last_assistant("hello world".to_string());
        let id = match state.entries().last() {
            Some(ChatEntry::Assistant { id, .. }) => *id,
            _ => panic!("Expected an assistant entry"),
        };
        assert!(
            !state.first_token_emphasis(id),
            "reduced-motion renders every turn at normal weight immediately"
        );
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
    fn thinking_entrance_fade_completes_across_entrance_ticks() {
        // Thinking previews run a short entrance fade (no typewriter reveal):
        // full content renders immediately, the opacity ramps in over
        // `ENTRANCE_TICKS`, then the entry is static.
        let mut state = State::new();
        state.add_thinking("coder", "planning...".to_string(), ThinkingKind::Detail);
        let id = match state.entries().last() {
            Some(ChatEntry::Thinking { id, .. }) => *id,
            _ => panic!("Expected a thinking entry"),
        };
        assert_eq!(state.entrance_ticks.get(&id), Some(&0), "a new preview seeds its fade");
        assert!(state.is_revealing(), "a pending thinking fade keeps the tick alive");

        // Exactly `ENTRANCE_TICKS` ticks complete the fade and drop it.
        for _ in 0..ENTRANCE_TICKS {
            assert!(state.is_revealing(), "fade stays live until the cap");
            let _ = state.update(Message::TypingTick);
        }
        assert!(!state.entrance_ticks.contains_key(&id), "fade entry removed at cap");
        assert!(
            !state.is_revealing(),
            "a settled-but-open thinking entry is static: no shimmer tick drives it forever"
        );
    }

    #[test]
    fn thinking_merge_never_restarts_the_entrance_fade() {
        // Appended content merges into the same thinking entry and must NOT
        // resurrect a finished fade (the old reveal-restart defect).
        let mut state = State::new();
        state.add_thinking("coder", "short".to_string(), ThinkingKind::Detail);
        let id = match state.entries().last() {
            Some(ChatEntry::Thinking { id, .. }) => *id,
            _ => panic!("Expected a thinking entry"),
        };
        for _ in 0..ENTRANCE_TICKS {
            let _ = state.update(Message::TypingTick);
        }
        assert!(!state.entrance_ticks.contains_key(&id), "fade completed under normal motion");
        state.add_thinking("coder", " longer tail".to_string(), ThinkingKind::Detail);
        assert!(
            !state.entrance_ticks.contains_key(&id),
            "merged content must not restart the entrance fade"
        );
        assert!(
            matches!(
                state.entries().last(),
                Some(ChatEntry::Thinking { content, .. })
                    if content == "short\n longer tail"
            ),
            "the content still merged into the same entry"
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
    fn open_thinking_does_not_drive_is_revealing_after_its_fade() {
        let mut state = State::new();
        state.add_thinking("coder", "planning...".to_string(), ThinkingKind::Detail);
        let id = match state.entries().last() {
            Some(ChatEntry::Thinking { id, .. }) => *id,
            _ => panic!("Expected a thinking entry"),
        };
        // The entrance fade briefly drives the tick...
        assert!(state.is_revealing(), "a pending thinking fade keeps the tick alive");
        for _ in 0..ENTRANCE_TICKS {
            let _ = state.update(Message::TypingTick);
        }
        // ...but a settled open entry is static: its shimmer freezes at the
        // last color and must NOT hold the 16 ms tick forever (the "thinking
        // entry pulses forever" defect).
        assert!(!state.entrance_ticks.contains_key(&id), "the fade completed and dropped");
        assert!(
            state.entries().iter().any(|entry| {
                matches!(entry, ChatEntry::Thinking { id: eid, finished_at: None, .. } if *eid == id)
            }),
            "the thinking entry is still open (run not finalized)"
        );
        assert!(!state.is_revealing(), "an open but settled thinking entry drives nothing");

        state.finalize_run();
        assert!(!state.is_revealing(), "finalizing changes nothing static either");
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

    #[test]
    fn buckets_group_non_consecutive_same_agent_thoughts() {
        let mut state = State::new();
        state.add_thinking("coder", "first".to_string(), ThinkingKind::Detail);
        let _ = state.update(Message::AddAssistant("mid".to_string()));
        state.add_thinking("coder", "second".to_string(), ThinkingKind::Detail);
        state.add_thinking("reviewer", "review note".to_string(), ThinkingKind::Detail);
        let (order, buckets) = thinking_buckets(state.entries());
        assert_eq!(order, vec!["coder".to_string(), "reviewer".to_string()]);
        assert_eq!(buckets["coder"].len(), 2, "non-consecutive thoughts share one bucket");
        assert_eq!(buckets["reviewer"].len(), 1);
    }

    #[test]
    fn headline_digest_falls_back_to_detail_and_skips_low_level() {
        let entries = vec![
            ChatEntry::Thinking {
                id: 1,
                agent: "coder".into(),
                content: "older detail".into(),
                kind: ThinkingKind::Detail,
                collapsed: false,
                created_at: None,
                finished_at: None,
            },
            ChatEntry::Thinking {
                id: 2,
                agent: "coder".into(),
                content: "latest detail".into(),
                kind: ThinkingKind::Detail,
                collapsed: false,
                created_at: None,
                finished_at: None,
            },
            ChatEntry::Thinking {
                id: 3,
                agent: "coder".into(),
                content: "[system_instructions]".into(),
                kind: ThinkingKind::LowLevel,
                collapsed: false,
                created_at: None,
                finished_at: None,
            },
        ];
        assert_eq!(thinking_headline_from_entries(&entries, &[0, 1, 2]), "latest detail");
    }

    #[test]
    fn headline_digest_low_level_only_stays_generic() {
        let entries = vec![ChatEntry::Thinking {
            id: 1,
            agent: "coder".into(),
            content: "[system_instructions]".into(),
            kind: ThinkingKind::LowLevel,
            collapsed: false,
            created_at: None,
            finished_at: None,
        }];
        assert_eq!(thinking_headline_from_entries(&entries, &[0]), "Thinking…");
    }

    #[test]
    fn composer_placeholder_advertises_thinking_toggle() {
        assert!(COMPOSER_PLACEHOLDER.contains("/thinking"));
    }

    #[test]
    fn headline_digest_prefers_latest_headline_entry() {
        let entries = vec![
            ChatEntry::Thinking {
                id: 1,
                agent: "coder".into(),
                content: "old detail".into(),
                kind: ThinkingKind::Detail,
                collapsed: false,
                created_at: None,
                finished_at: None,
            },
            ChatEntry::Thinking {
                id: 2,
                agent: "coder".into(),
                content: "Starting work".into(),
                kind: ThinkingKind::Headline,
                collapsed: false,
                created_at: None,
                finished_at: None,
            },
            ChatEntry::Thinking {
                id: 3,
                agent: "coder".into(),
                content: "newer detail".into(),
                kind: ThinkingKind::Detail,
                collapsed: false,
                created_at: None,
                finished_at: None,
            },
        ];
        assert_eq!(thinking_headline_from_entries(&entries, &[0, 1, 2]), "Starting work");
    }

    #[test]
    fn toggle_thinking_all_collapses_then_expands() {
        let mut state = State::new();
        state.add_thinking("coder", "x".to_string(), ThinkingKind::Detail);
        // Nothing open: first toggle expands all.
        state.toggle_thinking_all();
        assert!(state.thinking_expand_all);
        // Anything open: next toggle collapses all.
        state.toggle_thinking_all();
        assert!(!state.thinking_expand_all);
        assert_eq!(state.expanded_thinking_id, None);
    }

    #[test]
    fn toggle_thinking_all_message_routes_to_toggle() {
        // The input-bar toggle must reach `toggle_thinking_all` through the
        // message bus (it is forwarded from App::update unhandled).
        let mut state = State::new();
        state.add_error("notes".to_string());
        assert!(!state.thinking_expand_all, "sanity: nothing open yet");
        let _ = state.update(Message::ToggleThinkingAll);
        assert!(state.thinking_expand_all, "first toggle opens all thinking");
        let _ = state.update(Message::ToggleThinkingAll);
        assert!(!state.thinking_expand_all, "second toggle collapses all");
    }

    #[test]
    fn mute_toggle_hides_bucket_without_dropping_entries() {
        let mut state = State::new();
        state.add_thinking("coder", "x".to_string(), ThinkingKind::Detail);
        let before = state.entries().len();
        let _ = state.update(Message::ToggleMuteAgent("coder".to_string()));
        assert!(state.muted_agents.contains("coder"));
        assert_eq!(state.entries().len(), before, "mute is view-only; entries stay");
        let _ = state.update(Message::ToggleMuteAgent("coder".to_string()));
        assert!(!state.muted_agents.contains("coder"));
    }

    #[test]
    fn paragraph_boundary_detection_covers_separator_shapes() {
        // Single paragraph: no boundary anywhere — the handoff falls back to
        // no cue.
        assert!(!crosses_paragraph_boundary("just one paragraph", 0, 18));
        // Plain blank-line separator: boundary end (char 10) is crossed.
        let two = "01234567\n\nrest";
        assert!(crosses_paragraph_boundary(two, 8, 16));
        assert!(!crosses_paragraph_boundary(two, 0, 8), "boundary end sits past the window");
        assert!(!crosses_paragraph_boundary(two, 10, 16), "window starts at the boundary end");
        // Whitespace-only separator line still counts as a blank line.
        assert!(crosses_paragraph_boundary("01234567\n   \nrest", 8, 16));
        // CRLF separators count too.
        assert!(crosses_paragraph_boundary("01234567\r\n\r\nrest", 8, 16));
        // Trailing newlines with no content after them are the message end,
        // not a "continuing" handoff.
        assert!(!crosses_paragraph_boundary("0123456789\n\n", 0, 12));
        assert!(!crosses_paragraph_boundary("0123456789\n", 0, 11));
        // Empty content and empty windows never trigger.
        assert!(!crosses_paragraph_boundary("", 0, 0));
        assert!(!crosses_paragraph_boundary("a\n\nb", 4, 4));
    }

    #[test]
    fn handoff_hold_freezes_reveal_then_resumes() {
        let mut state = State::new();
        state.update_last_assistant("01234567\n\nrest of second paragraph here".to_string());
        let id = match state.entries().last() {
            Some(ChatEntry::Assistant { id, .. }) => *id,
            _ => panic!("Expected an assistant entry"),
        };

        // First tick reveals 8 chars; the boundary end (char 10) is still out
        // of reach, so no cue yet.
        let _ = state.update(Message::TypingTick);
        assert_eq!(state.revealed_chars, Some((id, 8)));
        assert_eq!(state.handoff_hold, None);

        // Second tick (8→16) crosses the blank-line boundary: the frontier
        // lands and a `HANDOFF_HOLD_TICKS`-tick cursor hold opens.
        let _ = state.update(Message::TypingTick);
        assert_eq!(state.revealed_chars, Some((id, 16)));
        assert_eq!(state.handoff_hold, Some((id, HANDOFF_HOLD_TICKS)));
        assert!(state.is_revealing(), "an open hold keeps the 16 ms tick alive");

        // The frontier stays frozen while the hold counts down, one per tick.
        for remaining in (1..HANDOFF_HOLD_TICKS).rev() {
            let _ = state.update(Message::TypingTick);
            assert_eq!(state.revealed_chars, Some((id, 16)), "the frontier stays frozen");
            assert_eq!(
                state.handoff_hold,
                Some((id, remaining)),
                "the hold counts down exactly one per tick"
            );
        }

        // Clearing tick: the hold drops but the frontier has not moved yet.
        let _ = state.update(Message::TypingTick);
        assert_eq!(state.handoff_hold, None);
        assert_eq!(state.revealed_chars, Some((id, 16)));

        // Next tick resumes the 8ch@16ms cadence exactly where it paused.
        let _ = state.update(Message::TypingTick);
        assert_eq!(state.revealed_chars, Some((id, 24)));
        assert_eq!(state.handoff_hold, None);
    }

    #[test]
    fn single_paragraph_reveal_never_holds() {
        let mut state = State::new();
        state.update_last_assistant("0123456789abcdefghij".to_string());
        let id = match state.entries().last() {
            Some(ChatEntry::Assistant { id, .. }) => *id,
            _ => panic!("Expected an assistant entry"),
        };
        for tick in 1..=3 {
            let _ = state.update(Message::TypingTick);
            assert_eq!(state.handoff_hold, None, "no cue without a paragraph boundary");
            let expected = (REVEAL_CHARS_PER_TICK * tick).min(20);
            assert_eq!(state.revealed_chars, Some((id, expected)));
        }
    }

    #[test]
    fn reduced_motion_skips_handoff_hold() {
        let mut state = State::new();
        state.set_reduced_motion(true);
        state.update_last_assistant("01234567\n\nrest of second paragraph here".to_string());
        // Drive the whole reveal: the frontier must cross the boundary with
        // no hold ever opening.
        for _ in 0..10 {
            let _ = state.update(Message::TypingTick);
            assert_eq!(state.handoff_hold, None, "reduced-motion renders without the cue");
        }
    }

    #[test]
    fn reduced_motion_final_message_renders_instantly() {
        let mut state = State::new();
        state.set_reduced_motion(true);
        let _ = state.update(Message::AddAssistant("final reply".into()));
        match state.entries().last() {
            Some(ChatEntry::Assistant { streaming, content, .. }) => {
                assert!(!*streaming, "reduced-motion final messages render instantly");
                assert_eq!(content, "final reply");
            }
            _ => panic!("Expected an assistant entry"),
        }
        assert_eq!(state.revealed_chars, None, "no reveal window under reduced-motion");
        assert!(!state.is_revealing(), "reduced-motion never starts the 16 ms tick");
        assert!(!state.is_streaming(), "no blink subscription may linger at idle");
    }

    #[test]
    fn reduced_motion_live_stream_seeds_no_animation_state() {
        let mut state = State::new();
        state.set_reduced_motion(true);
        // The live-streaming path (runtime `AssistantMessage`) still marks the
        // entry streaming (the run IS streaming), but seeds no reveal window,
        // no emphasis and no line wipe — every arrival renders in full.
        state.update_last_assistant("streamed text".to_string());
        assert!(state.is_streaming());
        assert_eq!(state.revealed_chars, None);
        assert!(state.first_token_ticks.is_empty());
        assert!(state.line_wipe_ticks.is_empty());
        assert!(state.line_wipe_settled.is_empty(), "no settled markers either");
        assert!(!state.is_revealing(), "no typing-tick work under reduced-motion");
    }

    #[test]
    fn reduced_motion_seeds_no_entrance_or_thinking_animation() {
        let mut state = State::new();
        state.set_reduced_motion(true);
        state.add_thinking("coder", "planning...".to_string(), ThinkingKind::Detail);
        state.add_error("blocking".to_string());
        state.add_tool_call("read_file".into(), "src/main.rs".into());
        assert!(
            state.entrance_ticks.is_empty(),
            "reduced-motion chips and thinking previews render fully opaque instantly"
        );
    }

    #[test]
    fn set_reduced_motion_true_settles_in_flight_animations() {
        let mut state = State::new();
        // Seed every tick-driven animation under normal motion. The assistant
        // final message goes last so it is the tracked reveal entry.
        state.add_thinking("coder", "planning...".to_string(), ThinkingKind::Detail);
        state.add_error("blocking".to_string());
        let _ = state.update(Message::AddAssistant("hello world".into()));
        assert!(!state.entrance_ticks.is_empty());
        assert!(!state.line_wipe_ticks.is_empty());
        assert!(!state.first_token_ticks.is_empty());
        assert!(state.revealed_chars.is_some());
        assert!(state.is_revealing());

        state.set_reduced_motion(true);

        assert!(state.entrance_ticks.is_empty());
        assert!(state.line_wipe_ticks.is_empty());
        assert!(state.line_wipe_settled.is_empty());
        assert!(state.first_token_ticks.is_empty());
        assert_eq!(state.revealed_chars, None);
        assert!(!state.is_revealing(), "settling must stop the tick immediately");
        // Entry content is untouched by the settle; the final message
        // mid-reveal finalizes itself so no cursor or blink lingers.
        match state.entries().iter().find(|e| matches!(e, ChatEntry::Assistant { .. })) {
            Some(ChatEntry::Assistant { content, streaming, .. }) => {
                assert_eq!(content, "hello world");
                assert!(!*streaming, "a final message mid-reveal finalizes on settle");
            }
            _ => panic!("Expected the assistant entry"),
        }
    }

    #[test]
    fn finalize_run_clears_handoff_hold() {
        let mut state = State::new();
        state.update_last_assistant("01234567\n\nrest of second paragraph here".to_string());
        let id = match state.entries().last() {
            Some(ChatEntry::Assistant { id, .. }) => *id,
            _ => panic!("Expected an assistant entry"),
        };
        let _ = state.update(Message::TypingTick);
        let _ = state.update(Message::TypingTick);
        assert!(state.handoff_hold.is_some(), "hold must be open before finalizing");
        state.finalize_run();
        assert_eq!(state.handoff_hold, None);
        assert_eq!(state.revealed_chars, None);
        // The wipe (still animating at the boundary) converges to its settled
        // top-rule marker so the reply keeps its "new turn" signature.
        assert!(state.line_wipe_ticks.is_empty(), "the animated wipe drops at the run boundary");
        assert!(
            state.line_wipe_settled.contains(&id),
            "the reply keeps its settled marker after the run"
        );
    }

    #[test]
    fn provenance_rail_derives_policy_and_reversibility() {
        // Policy decision follows the stored status; mutating tools promise
        // fs reversibility, unknown tools stay read-only (conservative).
        assert_eq!(
            provenance_rail(&ToolCallStatus::Completed, "fs_write"),
            "‖ policy ok · fs reversible"
        );
        assert_eq!(
            provenance_rail(&ToolCallStatus::Denied, "shell_exec"),
            "‖ policy denied · fs reversible"
        );
        assert_eq!(
            provenance_rail(&ToolCallStatus::Running, "search"),
            "‖ approval needed · read-only"
        );
        assert_eq!(
            provenance_rail(&ToolCallStatus::Cancelled, "mcp:server:read"),
            "‖ approval needed · read-only"
        );
        // Failed execution still passed the policy gate.
        assert_eq!(
            provenance_rail(&ToolCallStatus::Failed, "git_commit"),
            "‖ policy ok · fs reversible"
        );
    }

    #[test]
    fn thinking_agent_ids_normalize_into_one_bucket() {
        let mut state = State::new();
        state.add_thinking(" Coder ", "a".to_string(), ThinkingKind::Detail);
        // Different case + padding merges into the same open entry.
        state.add_thinking("coder", "b".to_string(), ThinkingKind::Detail);
        state.add_thinking("CODER", "c".to_string(), ThinkingKind::Detail);
        let thinking: Vec<_> = state
            .entries()
            .iter()
            .filter(|entry| matches!(entry, ChatEntry::Thinking { .. }))
            .collect();
        assert_eq!(thinking.len(), 1, "mixed-case ids must share one bucket");
        assert!(matches!(
            thinking[0],
            ChatEntry::Thinking { agent, .. } if agent == "coder"
        ));
        // Legacy `[Agent]` content prefixes normalize for grouping while the
        // content text keeps its original prefix.
        assert_eq!(thinking_agent_key("", "[Coder] plan"), "coder");
        assert_eq!(thinking_agent_key(" Reviewer ", "x"), "reviewer");
        // A mixed-case mute toggle hits the same normalized bucket.
        let _ = state.update(Message::ToggleMuteAgent("CODER".to_string()));
        assert!(state.muted_agents.contains("coder"));
    }

    #[test]
    fn muted_agents_seed_and_snapshot_round_trip_normalized() {
        let mut state = State::new();
        state.set_muted_agents(vec![" Reviewer ".into(), "reviewer".into(), "  ".into()]);
        assert_eq!(state.muted_agents_snapshot(), vec!["reviewer".to_string()]);
        // Restore path normalizes stored agents the same way.
        let restored = State::from_entries(vec![ChatEntry::Thinking {
            id: 1,
            agent: " Coder ".into(),
            content: "x".into(),
            kind: ThinkingKind::Detail,
            collapsed: false,
            created_at: None,
            finished_at: None,
        }]);
        assert!(matches!(
            restored.entries().first(),
            Some(ChatEntry::Thinking { agent, .. }) if agent == "coder"
        ));
    }
}
