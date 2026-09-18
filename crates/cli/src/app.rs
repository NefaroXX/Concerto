use std::collections::VecDeque;
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::text::{Line, Span};

use crate::approval::{
    ApprovalPrompt, CliApprovalSink, CliApprovalState, IntentPrompt, PlanPrompt,
};
use crate::theme::CliTheme;
use crate::ui::{self, chat_line_with_theme, ChatRole};
use concerto_config::{
    AgentModelAssignment, AppConfig, ConditionDef, ModelSettings, MultiAgentConfig, PolicyConfig,
    PolicyRuleDef,
};
use concerto_core::event::{EventBus, EventKind, ThinkingKind};
use concerto_core::ids::Ulid;
use concerto_core::intent::{PlanDecision, RequestedOutcome, RunStage};
use concerto_core::traits::approval::ApprovalDecision;
use concerto_core::transcript::{TranscriptEntry, TranscriptToolStatus};

use concerto_core::types::normalize_agent_id;
use concerto_core::types::AgentOutput;
use concerto_core::CancellationToken;
use concerto_orchestrator::runtime_runner::{
    memory_enabled, run_shared_agent, ActiveMemoryServices,
};
use concerto_orchestrator::services::{RequestBuilder, ServicesBuilder};
use concerto_orchestrator::session_manager::ProjectSessionManager;
use concerto_providers::factory::ProviderFactory;
use concerto_providers::provider_defs::{model_options_for, provider_definition};
use concerto_sessions::SessionSummary;

/// CLI flags supplied at startup and remembered for the life of the TUI
/// (ADR-57 D5). `Some` marks an explicit flag that must survive a config
/// reload; `None` means "follow the (reloaded) config default".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunFlags {
    /// Explicit `--multi-agent`/`-m`. Always `Some(true)` when the flag was
    /// passed (there is no `--no-multi-agent`); `None` when it was not.
    pub multi_agent: Option<bool>,
    /// Explicit `--fast`/`-f`, likewise. There is no config key for fast, so
    /// absent means `false`.
    pub fast: Option<bool>,
    /// Explicit `--reduced-motion`. `Some(true)` when the flag was passed
    /// (there is no `--no-reduced-motion`); `None` follows
    /// `CONCERTO_REDUCED_MOTION` env, then the config file — see
    /// `concerto_config::resolve_reduced_motion`.
    pub reduced_motion: Option<bool>,
    /// Explicit `--no-terminal-title`. `Some(true)` when the flag was passed
    /// (there is no `--terminal-title` opt-in); `None` follows
    /// `CONCERTO_NO_TITLE` env, then the config file — see
    /// `concerto_config::resolve_terminal_title_enabled`.
    pub no_terminal_title: Option<bool>,
    /// Explicit `--theme <name>`. `Some` carries the raw flag value
    /// (`Midnight` / `Slate` / `Chalk` / `Nebula`); `None` follows
    /// `CONCERTO_THEME` env, then `[display] theme` — see
    /// `crate::theme::resolve_cli_theme`.
    pub theme: Option<String>,
}

/// Per-run preferences resolved from the effective config plus the remembered
/// CLI flags (ADR-57 D5). Purely derived so reload behavior is unit-testable.
#[derive(Debug, Clone, PartialEq)]
struct ResolvedRunPrefs {
    multi_agent: bool,
    fast: bool,
    reduced_motion: bool,
    terminal_title_enabled: bool,
    cli_theme: CliTheme,
    selected_model: String,
    model_choices: Vec<String>,
    agent_assignments: Vec<AgentModelAssignment>,
}

/// Outcome of a per-run reload (ADR-57 D5). `Unchanged` triggers the equality
/// short-circuit — no re-derivation, so in-app overrides (multi-agent toggle,
/// fast, model cycler) survive an external write of the same config.
#[derive(Debug, Clone, PartialEq)]
enum ReloadOutcome {
    Unchanged,
    Changed(ResolvedRunPrefs),
}

/// A captured tool execution record for the tool log overlay.
#[derive(Debug, Clone)]
pub struct ToolLogEntry {
    pub tool_name: String,
    pub status: ToolStatus,
    pub detail: Option<String>,
    pub duration_ms: Option<u64>,
}

#[derive(Debug, Clone)]
pub enum ToolStatus {
    Running,
    Success,
    Failure,
    Timeout { timeout_secs: u64 },
}

impl ToolStatus {
    /// Provenance rail suffix for tool-log lines (Blueprint texture `‖ ok`).
    /// Read-only derivation from the stored status — it never consults the
    /// policy engine. The text is always appended (rail is state, not motion:
    /// `reduced_motion` never gates it); only the color is gated by `styling`.
    /// Mapping mirrors the desktop/transcript rails: executed tools passed
    /// the gate (`policy ok`), running tools await it (`approval needed`), and
    /// a timed-out tool surfaces as `denied` (the run refused to wait further).
    pub(crate) fn rail_text(&self) -> &'static str {
        match self {
            ToolStatus::Running => " ‖ approval needed",
            ToolStatus::Success | ToolStatus::Failure => " ‖ policy ok",
            ToolStatus::Timeout { .. } => " ‖ denied",
        }
    }

    /// Themed rail color: ok green, approval-needed yellow, denied red.
    /// Uses the CLI theme bridge so light/neon themes stay legible.
    pub(crate) fn rail_color(&self, theme: &CliTheme) -> ratatui::style::Color {
        match self {
            ToolStatus::Running => theme.warning,
            ToolStatus::Success | ToolStatus::Failure => theme.success,
            ToolStatus::Timeout { .. } => theme.danger,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Chat,
    Settings,
    Sessions,
    ToolLog,
    AgentAssignments,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsField {
    Model,
    Provider,
    PolicyPreset,
    MultiAgent,
    AgentAssignments,
    FastMode,
}

impl SettingsField {
    pub const ALL: &'static [SettingsField] = &[
        SettingsField::Model,
        SettingsField::Provider,
        SettingsField::PolicyPreset,
        SettingsField::MultiAgent,
        SettingsField::AgentAssignments,
        SettingsField::FastMode,
    ];

    pub fn label(self) -> &'static str {
        match self {
            SettingsField::Model => "Model",
            SettingsField::Provider => "Provider",
            SettingsField::PolicyPreset => "Policy preset",
            SettingsField::MultiAgent => "Multi-agent",
            SettingsField::AgentAssignments => "Agent assignments",
            SettingsField::FastMode => "Fast mode",
        }
    }

    pub fn display_value(self, app: &App) -> String {
        match self {
            SettingsField::Model => app.model_label().to_string(),
            SettingsField::Provider => app.provider_label().to_string(),
            SettingsField::PolicyPreset => app.policy_label().to_string(),
            SettingsField::MultiAgent => if app.multi_agent { "on" } else { "off" }.to_string(),
            SettingsField::AgentAssignments => {
                let count = app.agent_assignments.len();
                if count > 0 {
                    format!("{count} agents")
                } else {
                    "(default)".to_string()
                }
            }
            SettingsField::FastMode => if app.fast { "on" } else { "off" }.to_string(),
        }
    }
}

struct RunCompletion {
    result: Result<AgentOutput, String>,
}

/// An inbound line destined for the chat view, tagged by origin. Assistant
/// final messages arrive whole and are revealed character-by-character
/// (typewriter style, Issue #147 Part 2); every other line renders instantly.
#[derive(Debug, Clone)]
pub(crate) enum UiLine {
    /// A plain activity/status line rendered instantly.
    Text(String),
    /// The assistant's final message, animated into view by the reveal.
    Assistant(String),
    /// An agent thought, tiered for the score accordion (V2). `LowLevel`
    /// thoughts never become `UiLine`s (dropped at the bus adapter).
    Thought { agent: String, kind: ThinkingKind, content: String },
}

/// In-progress typewriter reveal of the assistant's final message. The last
/// `messages` line holds the revealed prefix; `shown` is how many characters
/// are visible so far. `ticks` counts elapsed reveal ticks (16 ms cadence)
/// for the first-token emphasis window; `hold` counts down the paragraph
/// handoff hold while the frontier stays frozen at a blank-line boundary.
struct RevealState {
    full: String,
    shown: usize,
    ticks: u8,
    hold: u8,
}

/// One tiered thought in the current movement's log (V2 score accordion).
/// `shown` records whether the line rendered at ingest time so `/thinking`
/// expand-all replays exactly the suppressed lines — never duplicates.
pub(crate) struct ThoughtRecord {
    agent: String,
    kind: ThinkingKind,
    content: String,
    shown: bool,
}

/// Characters the typewriter reveal adds per tick. Together with
/// `REVEAL_TICK_MS` this is the same ~500 chars/s pacing as the desktop
/// typewriter reveal (`REVEAL_CHARS_PER_TICK = 8` at a 16 ms tick) — the
/// CLI animates at exactly the same cadence for behavioral parity.
const REVEAL_CHARS_PER_TICK: usize = 8;
/// Milliseconds between reveal advances while a reveal is active.
const REVEAL_TICK_MS: u64 = 16;
/// First-token emphasis window (Score signature, prototype #6): 12 ticks at
/// the 16 ms cadence ≈ 200 ms, matching the desktop
/// `FIRST_TOKEN_EMPHASIS_TICKS`. The first word of each assistant turn
/// renders bold for this long, then settles to normal weight.
const FIRST_TOKEN_EMPHASIS_TICKS: u8 = 12;
/// Paragraph handoff hold (Baton cue, prototype #5): 3 ticks at the 16 ms
/// cadence ≈ 50 ms, matching the desktop `HANDOFF_HOLD_TICKS`. The reveal
/// frontier freezes at the blank-line boundary and the line dims while the
/// hold counts down, then the 8ch@16ms cadence resumes exactly where it
/// paused.
const HANDOFF_HOLD_TICKS: u8 = 3;
/// Wipe-analog rule drawn above each new assistant turn. The desktop canvas
/// line-wipe has no TUI equivalent, so a static horizontal rule marks the
/// same turn boundary. Skipped under reduced-motion (caller gate).
const WIPE_RULE: &str = "───";
/// ADR-60 D7 (interrupt-safe resume): how long a quit with a run still in
/// flight waits for the cancelled run to unwind and persist its interrupted
/// checkpoint before the process tears down. Bounded so the exit is never a
/// hang; a miss is the documented hard-kill loss (the later `continue`
/// resumes headless from the evidence chain). Generous enough for in-flight
/// provider streams to observe the cancellation and the coordinator's cancel
/// path to run its bounded SQLite write.
const GRACEFUL_SHUTDOWN_WAIT: Duration = Duration::from_secs(30);
/// Bound on the per-movement thought log (V3 quick win): the log is
/// session-scoped (cleared on new session) and drops oldest-first past this
/// cap so long movements cannot grow memory without bound.
const MAX_THOUGHT_LOG: usize = 500;

pub struct App {
    bus: EventBus,
    pub messages: Vec<Line<'static>>,
    /// Active typewriter reveal of the assistant's final message (Issue #147
    /// Part 2). The last `messages` line is the revealed prefix. `Some` only
    /// while a reveal is animating; it drives the 16 ms redraw cadence while
    /// active and is cleared once the full text is shown.
    reveal: Option<RevealState>,
    pub input: String,
    pub scroll: u16,
    /// Vertical scroll offset of the plan-approval modal's plan body (ADR-55
    /// Phase 1d). The stored plan can be up to 16 KiB, far larger than the
    /// modal, so j/k / arrow keys page through it.
    pub plan_scroll: u16,
    pub input_mode: bool,
    pub screen: Screen,
    pub settings_index: usize,
    pub config: Option<AppConfig>,
    global_config: Option<AppConfig>,
    /// Explicit CLI flags remembered at startup and re-applied after every
    /// config reload (ADR-57 D5) so an external edit cannot clobber `-m`/`-f`.
    pub run_flags: RunFlags,
    pub fast: bool,
    pub multi_agent: bool,
    pub project_dir: PathBuf,
    selected_model: String,
    model_choices: Vec<String>,
    memory: Arc<Mutex<Option<ActiveMemoryServices>>>,
    session_manager: Option<Arc<ProjectSessionManager>>,
    session_id: Option<Ulid>,
    pub sessions_list: Vec<SessionSummary>,
    pub sessions_index: usize,
    pub agent_assignments: Vec<AgentModelAssignment>,
    pub agent_assignment_index: usize,
    pub running: bool,
    /// Intent-router stage of the active run (ADR-55 Phase 2a), rendered in
    /// the status bar. `Some` only while a run is in flight; cleared at every
    /// run boundary: dispatch start, completion, and cancel.
    pub run_stage: Option<RunStage>,
    pub project_picker_mode: bool,
    cancel_token: CancellationToken,
    approval_state: CliApprovalState,
    approval_sink: Arc<CliApprovalSink>,
    completion_tx: std::sync::mpsc::Sender<RunCompletion>,
    completion_rx: std::sync::mpsc::Receiver<RunCompletion>,
    /// ADR-60 D7 (interrupt-safe resume): receiver for a REAL process SIGINT
    /// (`tokio::signal::ctrl_c()`), armed by `run`. Raw mode intercepts the
    /// Ctrl+C KEY for the TUI, so SIGINT arrives only from outside the TUI
    /// (`kill -INT`, raw mode off, or a relayed terminal signal) — the paths
    /// that previously terminated the process instantly and left the run
    /// with no orchestration checkpoint. Drained by the event loop like the
    /// other channels.
    interrupt_rx: Option<std::sync::mpsc::Receiver<()>>,
    pub tool_log: VecDeque<ToolLogEntry>,
    tool_event_rx: Option<std::sync::mpsc::Receiver<EventKind>>,
    stage_rx: Option<std::sync::mpsc::Receiver<RunStage>>,
    pub memory_chunks: usize,
    /// Tracks the most recently seen agent id for the thinking accordion
    /// header insertion. When the agent changes, a bold header line is
    /// injected before the first thought from the new agent.
    current_agent: Option<String>,
    /// Full thought log for the current movement (V2 score accordion).
    /// Every `Headline`/`Detail` thought is recorded with its visibility so
    /// `/thinking` expand-all can replay suppressed lines and collapse-all
    /// can print a per-agent digest. `LowLevel` thoughts never enter here.
    /// Bounded to `MAX_THOUGHT_LOG` entries (session-scoped; cleared on new
    /// session) so a long movement cannot grow memory without bound.
    pub(crate) thought_log: VecDeque<ThoughtRecord>,
    /// Whether `Detail` thoughts render inline (V2). `true` preserves the
    /// historical show-everything behavior; `/thinking` toggles it per
    /// movement. `Headline` thoughts always render.
    pub(crate) thinking_expanded: bool,
    /// Reduced-motion override (same gate as desktop): skips first-token
    /// bold, the handoff hold/dim, and the wipe-analog rule — reveals render
    /// plain and instant. Off by default; set via `set_reduced_motion`.
    pub(crate) reduced_motion: bool,
    /// Whether run stages are broadcast to the terminal title via OSC.
    /// Resolved per run (flag > env > config); the emit itself is gated to
    /// stage transitions only (never per-frame) — see `sync_terminal_title`.
    pub(crate) terminal_title_enabled: bool,
    /// Effective CLI theme (flag > `CONCERTO_THEME` env > `[display] theme` >
    /// `Midnight`). The single color source for gutters and status chrome;
    /// `NO_COLOR`/off-TTY still renders plain via the `styling` gate.
    pub cli_theme: CliTheme,
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

impl App {
    pub fn new() -> Self {
        let (completion_tx, completion_rx) = std::sync::mpsc::channel();
        let approval_state = CliApprovalState::default();
        let approval_sink = Arc::new(CliApprovalSink::with_state(approval_state.clone()));
        Self {
            bus: EventBus::default(),
            messages: {
                let mut initial = startup_signature_lines(styling_enabled());
                initial.push(Line::from("Concerto CLI — type a message and press Enter"));
                initial.push(Line::from(
                    "Esc: commands/settings  [l] sessions  [t] log  [n] new  [p] project  Ctrl+C: cancel or quit",
                ));
                initial
            },
            reveal: None,
            input: String::new(),
            scroll: 0,
            plan_scroll: 0,
            input_mode: true,
            screen: Screen::Chat,
            settings_index: 0,
            config: None,
            global_config: None,
            run_flags: RunFlags::default(),
            fast: false,
            multi_agent: false,
            project_dir: PathBuf::from("."),
            selected_model: String::new(),
            model_choices: Vec::new(),
            memory: Arc::new(Mutex::new(None)),
            session_manager: None,
            session_id: None,
            sessions_list: Vec::new(),
            sessions_index: 0,
            agent_assignments: Vec::new(),
            agent_assignment_index: 0,
            running: false,
            run_stage: None,
            project_picker_mode: false,
            cancel_token: CancellationToken::new(),
            approval_state,
            approval_sink,
            completion_tx,
            completion_rx,
            interrupt_rx: None,
            tool_log: VecDeque::new(),
            tool_event_rx: None,
            stage_rx: None,
            memory_chunks: 0,
            current_agent: None,
            thought_log: VecDeque::new(),
            thinking_expanded: true,
            reduced_motion: false,
            terminal_title_enabled: true,
            cli_theme: CliTheme::by_name("Midnight"),
        }
    }

    pub fn configure(
        &mut self,
        global_config: AppConfig,
        effective_config: AppConfig,
        project_dir: PathBuf,
        session_manager: Arc<ProjectSessionManager>,
    ) {
        self.project_dir = concerto_core::helpers::canonical_project_path(&project_dir);
        // Run-mode/model derivation honors the remembered CLI flags
        // (ADR-57 D5): an explicit -m/-f wins over the config default.
        self.apply_run_prefs(resolve_run_prefs(&effective_config, &self.run_flags));
        self.global_config = Some(global_config);
        self.config = Some(effective_config);
        self.session_manager = Some(session_manager);
        self.messages.push(Line::from(format!("Project: {}", self.project_dir.display())));
    }

    /// Reload `global_config` + `config` from disk at the top of a run so
    /// external config edits (global `config.toml`, project `.concerto.toml`)
    /// take effect on the next dispatch without a restart (ADR-57 D5).
    ///
    /// Semantics:
    /// - A broken file keeps the last-good config and the run proceeds on it
    ///   (runs never fail because of a bad edit window).
    /// - `load_global_config` is refreshed too so a later settings save cannot
    ///   silently overwrite an external edit.
    /// - When the reloaded effective config equals the current one, re-derivation
    ///   is skipped entirely (equality short-circuit, ADR-57 D3b): self-induced
    ///   writes and no-op edits are inert, and in-app overrides (multi-agent
    ///   toggle, fast, model cycler) survive to the next run.
    /// - On a real change, run prefs are re-derived with the remembered CLI
    ///   flags re-applied, so a config edit cannot clobber an explicit `-m`/`-f`.
    fn reload_config_for_run(&mut self) {
        let effective = match concerto_config::load_config(None, Some(&self.project_dir)) {
            Ok(effective) => effective,
            Err(error) => {
                tracing::warn!(%error, "config: reload failed, keeping last-good config");
                return;
            }
        };
        match concerto_config::load_global_config(None) {
            Ok(global) => self.global_config = Some(global),
            Err(error) => {
                tracing::warn!(
                    %error,
                    "config: global reload failed, keeping last-good global config"
                );
            }
        }
        match decide_reload(self.config.as_ref(), &effective, &self.run_flags) {
            ReloadOutcome::Unchanged => {}
            ReloadOutcome::Changed(prefs) => self.apply_run_prefs(prefs),
        }
        self.config = Some(effective);
    }

    /// Apply resolved per-run preferences to the App state (ADR-57 D5).
    fn apply_run_prefs(&mut self, prefs: ResolvedRunPrefs) {
        self.multi_agent = prefs.multi_agent;
        self.fast = prefs.fast;
        self.set_reduced_motion(prefs.reduced_motion);
        self.terminal_title_enabled = prefs.terminal_title_enabled;
        self.cli_theme = prefs.cli_theme;
        self.selected_model = prefs.selected_model;
        self.model_choices = prefs.model_choices;
        self.agent_assignments = prefs.agent_assignments;
    }

    /// Switch the active project directory, reloading config and resetting state.
    /// The session manager is shared (project-agnostic store), so it is reused.
    pub fn switch_project(&mut self, new_dir: PathBuf, _rt: &tokio::runtime::Handle) {
        let canonical = concerto_core::helpers::canonical_project_path(&new_dir);
        if !canonical.exists() {
            self.push_line(Line::from(format!(
                "Project directory does not exist: {}",
                canonical.display()
            )));
            return;
        }
        if canonical == self.project_dir {
            self.push_line(Line::from("Already in this project."));
            return;
        }

        // Cancel any running agent.
        if self.running {
            self.cancel_token.cancel();
            self.running = false;
            self.run_stage = None;
        }

        // Cancel memory indexer if active and clear it.
        if let Some(prev) = self.memory.lock().unwrap_or_else(|error| error.into_inner()).take() {
            prev.cancel.cancel();
        }

        // Reload configs for the new project.
        let config_path = concerto_config::default_config_path();
        let global_config =
            concerto_config::load_global_config(config_path.as_ref()).unwrap_or_default();
        let effective_config = concerto_config::load_config(config_path.as_ref(), Some(&canonical))
            .unwrap_or_else(|_| global_config.clone());

        // Update all project-dependent state.
        self.project_dir = canonical.clone();
        self.selected_model = default_model(&effective_config);
        self.model_choices = available_models(&effective_config);
        self.cli_theme =
            crate::theme::resolve_cli_theme(self.run_flags.theme.as_deref(), &effective_config);
        self.agent_assignments = effective_config
            .model_settings
            .as_ref()
            .map(|ms| ms.agent_assignments.clone())
            .unwrap_or_default();
        self.global_config = Some(global_config);
        self.config = Some(effective_config);

        // Reset session state.
        self.session_id = None;
        self.messages.clear();
        self.push_line(Line::from(format!("Switched to project: {}", canonical.display())));

        // Update project registry.
        if let Ok(mut registry) = concerto_config::ProjectRegistry::load() {
            let _ = registry.select(&canonical);
            registry.save().ok();
        }
    }

    pub fn restore_active_session(&mut self, rt: &tokio::runtime::Runtime) {
        let (Some(manager), Ok(project)) = (
            self.session_manager.clone(),
            camino::Utf8PathBuf::from_path_buf(self.project_dir.clone()),
        ) else {
            return;
        };
        let session = match rt.block_on(manager.get_or_create_active_session(
            &project,
            self.provider_label(),
            self.model_label(),
            CancellationToken::new(),
        )) {
            Ok(session) => session,
            Err(error) => {
                self.messages.push(Line::from(format!("Could not restore session: {error}")));
                return;
            }
        };
        // The durable typed transcript (ADR-36) is canonical for new sessions;
        // legacy sessions (empty transcript) fall back to the messages-only
        // view below.
        let transcript = match rt
            .block_on(manager.store().load_transcript(session.session_id, CancellationToken::new()))
        {
            Ok(transcript) => transcript,
            Err(error) => {
                self.messages.push(Line::from(format!("Could not load transcript: {error}")));
                Vec::new()
            }
        };
        if !transcript.is_empty() {
            self.session_id = Some(session.session_id);
            self.messages.push(Line::from(format!("Resumed session {}", session.session_id)));
            let theme = self.cli_theme;
            let styling = styling_enabled();
            self.messages.extend(transcript_lines_with_theme(&transcript, styling, &theme));
            return;
        }
        let history = match rt
            .block_on(manager.load_recent_messages(session.session_id, CancellationToken::new()))
        {
            Ok(history) => history,
            Err(error) => {
                self.messages.push(Line::from(format!("Could not restore history: {error}")));
                return;
            }
        };
        self.session_id = Some(session.session_id);
        if !history.is_empty() {
            self.messages.push(Line::from(format!("Resumed session {}", session.session_id)));
            let styling = styling_enabled();
            let theme = self.cli_theme;
            for message in history {
                match message.role {
                    concerto_core::types::Role::User => {
                        self.messages.push(chat_line_with_theme(
                            ChatRole::User,
                            &message.content,
                            styling,
                            &theme,
                        ));
                    }
                    concerto_core::types::Role::Assistant if !message.content.trim().is_empty() => {
                        self.messages.push(chat_line_with_theme(
                            ChatRole::Assistant,
                            &message.content,
                            styling,
                            &theme,
                        ));
                    }
                    _ => {}
                }
            }
        }
    }

    pub fn approval_prompt(&self) -> Option<ApprovalPrompt> {
        self.approval_state.prompt()
    }

    pub fn intent_prompt(&self) -> Option<IntentPrompt> {
        self.approval_state.intent_prompt()
    }

    pub fn plan_prompt(&self) -> Option<PlanPrompt> {
        self.approval_state.plan_prompt()
    }

    /// Reduced-motion override (same gate as desktop): when true the reveal
    /// renders plain — no first-token bold, no handoff hold/dim, no
    /// wipe-analog rule. Mirrors the desktop `set_reduced_motion` contract.
    pub fn set_reduced_motion(&mut self, reduced: bool) {
        self.reduced_motion = reduced;
    }

    /// Whether an approval (tool, intent, or plan) is currently awaiting the
    /// user. Overrides any run stage in the terminal title.
    fn is_awaiting_approval(&self) -> bool {
        self.approval_state.prompt().is_some()
            || self.approval_state.intent_prompt().is_some()
            || self.approval_state.plan_prompt().is_some()
    }

    /// Broadcast the current run stage to the terminal title (OSC `ESC ] 0`).
    ///
    /// Gated TTY-only via `should_emit_terminal_title` and called only on
    /// stage transitions (never per-frame), so the escape is written at most
    /// once per transition — no flicker, no pipe pollution, silent under CI.
    fn sync_terminal_title(&self) {
        use std::io::IsTerminal as _;
        let is_tty = std::io::stdout().is_terminal();
        let is_ci = std::env::var("CI").is_ok();
        if !should_emit_terminal_title(self.terminal_title_enabled, is_tty, is_ci) {
            return;
        }
        emit_terminal_title(&title_for_stage(self.run_stage, self.is_awaiting_approval()));
    }

    pub fn model_label(&self) -> &str {
        if self.selected_model.is_empty() {
            "(not configured)"
        } else {
            &self.selected_model
        }
    }

    pub fn provider_label(&self) -> &str {
        let Some(config) = &self.config else { return "none" };
        let Some(settings) = &config.model_settings else {
            return config.primary_provider.as_deref().unwrap_or("none");
        };
        ProviderFactory::config_for_model(settings, &self.selected_model, None)
            .map(|provider| provider.provider.as_str())
            .unwrap_or("auto")
    }

    pub fn policy_label(&self) -> &'static str {
        let Some(config) = &self.global_config else { return "safe" };
        match config.policy.as_ref() {
            None => "safe",
            Some(policy)
                if policy.rules.len() == 1
                    && policy.rules[0].action == "require_approval"
                    && matches!(policy.rules[0].condition, ConditionDef::Always { .. }) =>
            {
                "strict"
            }
            Some(policy)
                if policy.rules.last().is_some_and(|rule| {
                    rule.action == "auto_approve"
                        && matches!(rule.condition, ConditionDef::Always { .. })
                }) =>
            {
                "permissive"
            }
            Some(_) => "custom",
        }
    }

    pub fn run(
        &mut self,
        terminal: &mut ratatui::Terminal<ratatui::backend::CrosstermBackend<io::Stdout>>,
        rt: &tokio::runtime::Runtime,
    ) -> anyhow::Result<()> {
        crossterm::execute!(io::stdout(), crossterm::terminal::EnterAlternateScreen)?;
        crossterm::terminal::enable_raw_mode()?;

        let (event_tx, event_rx) = std::sync::mpsc::channel::<UiLine>();
        let (tool_event_tx, tool_event_rx) = std::sync::mpsc::channel::<EventKind>();
        self.tool_event_rx = Some(tool_event_rx);
        let (stage_tx, stage_rx) = std::sync::mpsc::channel::<RunStage>();
        self.stage_rx = Some(stage_rx);
        // ADR-60 D7 (interrupt-safe resume): arm a real SIGINT watcher. The
        // TUI intercepts the Ctrl+C key in raw mode (the `Action::Cancel`
        // path); this watcher covers every OTHER SIGINT source (`kill -INT`,
        // raw mode off, terminal relay) — the paths that previously killed
        // the process instantly, before the coordinator's cancel path could
        // persist the interrupted checkpoint.
        let (signal_tx, signal_rx) = std::sync::mpsc::channel::<()>();
        self.interrupt_rx = Some(signal_rx);
        rt.spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                let _ = signal_tx.send(());
            }
        });
        let bus = self.bus.clone();
        tokio::spawn(async move {
            let mut receiver = bus.subscribe();
            while let Ok(event) = receiver.recv().await {
                let is_tool = matches!(
                    event.kind,
                    EventKind::ToolExecutionStarted { .. }
                        | EventKind::ToolExecutionFinished { .. }
                        | EventKind::ToolTimeout { .. }
                        | EventKind::IndexingCompleted { .. }
                );
                // Tiered thoughts (score accordion V2): LowLevel never
                // reaches a chat surface; the App applies collapse filtering
                // and header injection at ingest time.
                if let EventKind::AgentThought { agent_id, content, kind } = &event.kind {
                    if *kind != ThinkingKind::LowLevel {
                        let _ = event_tx.send(UiLine::Thought {
                            agent: agent_id.clone(),
                            kind: *kind,
                            content: content.clone(),
                        });
                    }
                }
                if let Some(line) = event_line(&event.kind) {
                    let tagged = if matches!(&event.kind, EventKind::AssistantMessage { .. }) {
                        UiLine::Assistant(line)
                    } else {
                        UiLine::Text(line)
                    };
                    let _ = event_tx.send(tagged);
                }
                if is_tool {
                    let _ = tool_event_tx.send(event.kind.clone());
                }
                if let EventKind::RunStageChanged { stage, .. } = &event.kind {
                    let _ = stage_tx.send(*stage);
                }
            }
        });

        let result = self.event_loop(terminal, &event_rx, rt);
        // ADR-60 D7 (interrupt-safe resume): a run still in flight at quit
        // is cancelled and given a bounded window to unwind, so the
        // coordinator's cancel path persists the interrupted (completed=0)
        // checkpoint BEFORE the process tears down. Missing the window is
        // the documented hard-kill loss (the later `continue` then resumes
        // headless from the evidence chain) — never a hang.
        if self.running {
            self.cancel_token.cancel();
            self.await_run_settlement(GRACEFUL_SHUTDOWN_WAIT);
        }
        if let Some(prev) = self.memory.lock().unwrap_or_else(|error| error.into_inner()).take() {
            prev.cancel.cancel();
        }
        crossterm::terminal::disable_raw_mode()?;
        crossterm::execute!(io::stdout(), crossterm::terminal::LeaveAlternateScreen)?;
        result
    }

    /// ADR-60 D7 (interrupt-safe resume): the shared interrupt path for the
    /// in-TUI Ctrl+C key and a real SIGINT alike — cancel the run's token so
    /// the coordinator's cancel branch persists the interrupted (completed=0)
    /// checkpoint, and tell the user cancellation was requested. The run task
    /// keeps unwinding on the runtime; the UI stays responsive.
    fn handle_interrupt(&mut self) {
        self.cancel_token.cancel();
        self.push_line(Line::from("Cancellation requested…"));
    }

    /// ADR-60 D7 (interrupt-safe resume): wait, bounded, for the in-flight
    /// run to settle after cancellation — the run task signals completion
    /// only after `run_shared_agent` has returned, and the coordinator's
    /// cancel path persists the interrupted checkpoint BEFORE that return, so
    /// a settled completion implies the checkpoint (or the honest
    /// absence thereof) is durable. A deadline miss is the documented
    /// hard-kill loss (the later `continue` then resumes headless from the
    /// evidence chain) — never a hang.
    fn await_run_settlement(&mut self, grace: Duration) {
        let deadline = std::time::Instant::now() + grace;
        while self.running {
            let now = std::time::Instant::now();
            if now >= deadline {
                break;
            }
            match self.completion_rx.recv_timeout(deadline - now) {
                Ok(completion) => {
                    self.running = false;
                    // Mirror the event loop's completion handling minimally:
                    // the run task already published its final message; the
                    // session id stays authoritative for the next dispatch.
                    if let Ok(output) = &completion.result {
                        self.session_id = Some(output.session_id);
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => break,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
    }

    fn event_loop(
        &mut self,
        terminal: &mut ratatui::Terminal<ratatui::backend::CrosstermBackend<io::Stdout>>,
        event_rx: &std::sync::mpsc::Receiver<UiLine>,
        rt: &tokio::runtime::Runtime,
    ) -> anyhow::Result<()> {
        loop {
            while let Ok(ui_line) = event_rx.try_recv() {
                self.ingest_ui_line(ui_line);
            }
            while let Ok(completion) = self.completion_rx.try_recv() {
                self.running = false;
                // Run boundary: the stage chip must not survive the run.
                self.run_stage = None;
                match completion.result {
                    Ok(output) => self.session_id = Some(output.session_id),
                    Err(error) => self.push_line(Line::from(format!("Error: {error}"))),
                }
                self.sync_terminal_title();
            }

            // Drain raw events (tool + status) (collect first to avoid borrow conflict).
            let tool_events: Vec<EventKind> =
                self.tool_event_rx.as_ref().map(|rx| rx.try_iter().collect()).unwrap_or_default();
            for kind in tool_events {
                if let EventKind::IndexingCompleted { chunk_count, .. } = &kind {
                    self.memory_chunks = *chunk_count;
                } else {
                    self.push_tool_event(kind);
                }
            }

            // ADR-60 D7 (interrupt-safe resume): a REAL SIGINT takes the same
            // cooperative-cancel path as the in-TUI Ctrl+C key.
            if let Some(rx) = self.interrupt_rx.as_ref() {
                let mut interrupted = false;
                while rx.try_recv().is_ok() {
                    interrupted = true;
                }
                if interrupted {
                    self.handle_interrupt();
                }
            }

            // Run-stage transitions from the backend (ADR-55 Phase 2a); the
            // status bar shows only the latest stage. The terminal title
            // follows the same transition (once per change, never per-frame).
            let mut stage_changed = false;
            while let Ok(stage) = self
                .stage_rx
                .as_ref()
                .map(|rx| rx.try_recv())
                .unwrap_or(Err(std::sync::mpsc::TryRecvError::Empty))
            {
                if self.run_stage != Some(stage) {
                    self.run_stage = Some(stage);
                    stage_changed = true;
                }
            }
            if stage_changed {
                self.sync_terminal_title();
            }

            terminal.draw(|frame| ui::draw(frame, self))?;
            // While a reveal is animating, poll at the typewriter cadence so
            // each tick can redraw a longer prefix; idle stays at the original
            // 100 ms poll so the loop costs nothing at rest.
            let poll_ms = if self.reveal.is_some() { REVEAL_TICK_MS } else { 100 };
            if event::poll(Duration::from_millis(poll_ms))? {
                match self.handle_key(event::read()?) {
                    Action::Quit => break,
                    Action::Cancel => self.handle_interrupt(),
                    Action::Dispatch(message) => self.dispatch_message(message, rt),
                    Action::NewSession => self.start_new_session(rt),
                    Action::None => {}
                }
            }
            // Advance the typewriter reveal after the poll+key block, i.e.
            // once per loop iteration whether or not a key arrived, so an
            // idle iteration still moves a hanging reveal forward.
            if self.reveal.is_some() {
                self.advance_reveal();
            }
        }
        Ok(())
    }

    fn dispatch_message(&mut self, input: String, rt: &tokio::runtime::Runtime) {
        if self.running || input.trim().is_empty() {
            return;
        }
        // `/thinking` is a local accordion toggle for the current movement
        // (collapse-all / expand-all) — never dispatched as a run.
        if input.trim() == "/thinking" {
            self.toggle_thinking();
            return;
        }
        // External config edits take effect on this dispatch (ADR-57 D5).
        self.reload_config_for_run();
        let (Some(config), Some(session_manager)) =
            (self.config.clone(), self.session_manager.clone())
        else {
            self.push_line(Line::from("Configuration or session storage is unavailable."));
            return;
        };

        let project = match camino::Utf8PathBuf::from_path_buf(self.project_dir.clone()) {
            Ok(project) => project,
            Err(path) => {
                self.push_line(Line::from(format!(
                    "Project path is not valid UTF-8: {}",
                    path.display()
                )));
                return;
            }
        };
        let provider = self.provider_label().to_string();
        let model = self.selected_model.clone();
        let session_id = match self.session_id {
            Some(id) => id,
            None => match rt.block_on(session_manager.get_or_create_active_session(
                &project,
                &provider,
                &model,
                CancellationToken::new(),
            )) {
                Ok(session) => session.session_id,
                Err(error) => {
                    self.push_line(Line::from(format!("Could not open session: {error}")));
                    return;
                }
            },
        };
        self.session_id = Some(session_id);
        let history = match rt
            .block_on(session_manager.load_recent_messages(session_id, CancellationToken::new()))
        {
            Ok(history) => history,
            Err(error) => {
                self.push_line(Line::from(format!("Could not load session history: {error}")));
                return;
            }
        };

        self.push_line(chat_line_with_theme(
            ChatRole::User,
            &input,
            styling_enabled(),
            &self.cli_theme,
        ));
        self.running = true;
        // Fresh run boundary: no stale stage from a previous run may show in
        // the status bar (the chip re-appears once a stage event lands).
        self.run_stage = None;
        self.current_agent = None;
        // Fresh movement boundary: the thought log belongs to the previous
        // movement, and collapse mode resets to show-everything.
        self.thought_log.clear();
        self.thinking_expanded = true;
        self.cancel_token = CancellationToken::new();

        let request =
            RequestBuilder::new(input, self.project_dir.clone(), self.cancel_token.clone())
                .with_provider_model(None, (!model.is_empty()).then_some(model.clone()))
                .with_session(session_id, history)
                .with_single_agent(!self.multi_agent)
                .with_memory_enabled(memory_enabled(self.fast, config.memory.enabled))
                .build();

        let services = ServicesBuilder::new(self.bus.clone(), config, self.approval_sink.clone())
            .with_session_manager(session_manager)
            .with_memory(self.memory.clone())
            .build();
        let completion = self.completion_tx.clone();
        let bus = self.bus.clone();
        rt.spawn(async move {
            let result = run_shared_agent(request, services).await;
            if let Ok(output) = &result {
                let _ = bus.publish_for_session(
                    output.session_id,
                    output.task_id.0,
                    EventKind::AssistantMessage {
                        task_id: output.task_id,
                        content: output.final_message.clone(),
                    },
                );
            }
            let _ = completion
                .send(RunCompletion { result: result.map_err(|error| error.to_string()) });
        });
    }

    fn start_new_session(&mut self, rt: &tokio::runtime::Runtime) {
        if self.running {
            return;
        }
        let Some(manager) = self.session_manager.clone() else { return };
        let Ok(project) = camino::Utf8PathBuf::from_path_buf(self.project_dir.clone()) else {
            return;
        };
        match rt.block_on(manager.create_new_session(
            &project,
            self.provider_label(),
            self.model_label(),
            CancellationToken::new(),
        )) {
            Ok(session) => {
                self.session_id = Some(session.session_id);
                self.approval_sink.set_auto_approve(false);
                self.messages.clear();
                self.push_line(Line::from(format!(
                    "New session {} — {}",
                    session.session_id,
                    self.project_dir.display()
                )));
            }
            Err(error) => self.push_line(Line::from(format!("Could not create session: {error}"))),
        }
    }

    /// Route one tagged inbound line into the chat view (Issue #147 Part 2).
    /// Plain lines render instantly; assistant final messages start a
    /// typewriter reveal by occupying a fresh (empty) line slot that grows
    /// over subsequent ticks.
    fn ingest_ui_line(&mut self, line: UiLine) {
        match line {
            UiLine::Text(text) => self.push_line(Line::from(text)),
            UiLine::Thought { agent, kind, content } => {
                self.ingest_thought(agent, kind, content);
            }
            UiLine::Assistant(full) => {
                // Cancel any active reveal first (same semantics as
                // `push_line`) so its slot is materialized before we push the
                // replacement slot below.
                self.cancel_reveal();
                let styling = styling_enabled();
                let theme = self.cli_theme;
                // Reduced-motion: instant — the full markdown-lite + gutter
                // line lands at once, no reveal state, no bold/hold extension.
                if self.reduced_motion {
                    self.messages.push(chat_line_with_theme(
                        ChatRole::Assistant,
                        &full,
                        styling,
                        &theme,
                    ));
                    self.reveal = None;
                    return;
                }
                // Wipe analog (Score prototype #7): a horizontal rule above
                // each new assistant turn — the desktop canvas line-wipe has
                // no TUI equivalent. Skipped under reduced-motion (same gate
                // as desktop); dimmed only on a color TTY.
                self.messages.push(wipe_rule_line(styling));
                self.messages.push(chat_line_with_theme(ChatRole::Assistant, "", styling, &theme));
                self.reveal = Some(RevealState { full, shown: 0, ticks: 0, hold: 0 });
            }
        }
    }

    /// Route one tiered thought into the chat view (V2 score accordion).
    /// The full movement is always logged; rendering follows the collapse
    /// mode — `Headline` thoughts always show, `Detail` only when expanded.
    /// A bold per-agent header is injected on agent change.
    ///
    /// The agent id normalizes (trimmed + lowercased) for grouping so
    /// `" Coder "` and `"coder"` share one bucket; content keeps its
    /// original text.
    fn ingest_thought(&mut self, agent: String, kind: ThinkingKind, content: String) {
        let agent = normalize_agent_id(&agent);
        let shown = self.thinking_expanded || kind == ThinkingKind::Headline;
        self.thought_log.push_back(ThoughtRecord {
            agent: agent.clone(),
            kind,
            content: content.clone(),
            shown,
        });
        // Bounded log: drop oldest first so a long movement stays session-
        // scoped without unbounded growth.
        while self.thought_log.len() > MAX_THOUGHT_LOG {
            self.thought_log.pop_front();
        }
        if self.current_agent.as_deref() != Some(agent.as_str()) {
            let theme = self.cli_theme;
            self.push_line(thinking_header_with_theme(&agent, &theme));
            self.current_agent = Some(agent.clone());
        }
        if shown {
            self.push_line(Line::from(format!("· [{agent}] {content}")));
        }
    }

    /// `/thinking` toggle for the current movement: collapse-all prints a
    /// per-agent digest (latest headline each) and suppresses future
    /// `Detail` lines; expand-all replays exactly the suppressed lines.
    fn toggle_thinking(&mut self) {
        // Policy-gutter (`‖`, yellow on a color TTY) for the digest lines;
        // the text stays byte-identical to the V2 accordion contract — only
        // the gutter color is new, and plain (symbols only) off-TTY.
        let styling = styling_enabled();
        let theme = self.cli_theme;
        self.thinking_expanded = !self.thinking_expanded;
        if self.thinking_expanded {
            let replay: Vec<(String, String)> = self
                .thought_log
                .iter_mut()
                .filter(|record| !record.shown && record.kind == ThinkingKind::Detail)
                .map(|record| {
                    record.shown = true;
                    (record.agent.clone(), record.content.clone())
                })
                .collect();
            for (agent, content) in replay {
                self.push_line(Line::from(format!("· [{agent}] {content}")));
            }
            self.push_line(chat_line_with_theme(
                ChatRole::Policy,
                "thinking expanded",
                styling,
                &theme,
            ));
        } else {
            let mut order: Vec<String> = Vec::new();
            let mut latest: std::collections::HashMap<String, String> =
                std::collections::HashMap::new();
            for record in &self.thought_log {
                if record.kind == ThinkingKind::Headline {
                    if !latest.contains_key(&record.agent) {
                        order.push(record.agent.clone());
                    }
                    latest.insert(record.agent.clone(), record.content.clone());
                }
            }
            // Detail fallback (V3 quick win): agents with no `Headline`
            // (Detail-only movements; `LowLevel` never enters the log) fall
            // back to their latest `Detail` line instead of going invisible.
            // Headline digests are never overwritten by Detail lines. The
            // forward iteration leaves the latest Detail in the map.
            let headline_agents: std::collections::HashSet<&str> = self
                .thought_log
                .iter()
                .filter(|r| r.kind == ThinkingKind::Headline)
                .map(|r| r.agent.as_str())
                .collect();
            for record in &self.thought_log {
                if record.kind == ThinkingKind::Detail
                    && !headline_agents.contains(record.agent.as_str())
                {
                    if !latest.contains_key(&record.agent) {
                        order.push(record.agent.clone());
                    }
                    latest.insert(record.agent.clone(), record.content.clone());
                }
            }
            for agent in &order {
                if let Some(headline) = latest.get(agent) {
                    let first = headline.lines().next().unwrap_or("").trim();
                    self.push_line(chat_line_with_theme(
                        ChatRole::Policy,
                        &format!("[{agent}] {first}"),
                        styling,
                        &theme,
                    ));
                }
            }
            self.push_line(chat_line_with_theme(
                ChatRole::Policy,
                "thinking collapsed — /thinking to expand",
                styling,
                &theme,
            ));
        }
    }

    /// Advance the typewriter reveal one tick at the 8ch@16ms parity cadence.
    /// Each tick the emphasis clock advances; while a paragraph handoff hold
    /// is open the frontier stays frozen and the line renders dim instead of
    /// growing. Crossing a blank-line boundary opens a fresh
    /// `HANDOFF_HOLD_TICKS` hold (skipped under reduced-motion). The finished
    /// line always settles to normal weight, and the state clears once the
    /// full text is shown. No-op when no reveal is active.
    fn advance_reveal(&mut self) {
        let reduced = self.reduced_motion;
        let styling = styling_enabled();
        let theme = self.cli_theme;
        let Some(state) = self.reveal.as_mut() else { return };
        // The emphasis clock runs on every tick — including hold ticks, like
        // the desktop `advance_first_token_ticks` — so a handoff never
        // stretches the bold window.
        state.ticks = state.ticks.saturating_add(1);
        if state.hold > 0 {
            state.hold -= 1;
            let line =
                reveal_line_with_theme(&state.full, state.shown, false, true, styling, &theme);
            if let Some(last) = self.messages.last_mut() {
                *last = line;
            }
            return;
        }
        let total = state.full.chars().count();
        let from = state.shown;
        let next = from.saturating_add(REVEAL_CHARS_PER_TICK).min(total);
        // Paragraph handoff cue (Baton prototype #5): when the frontier
        // crosses a blank-line boundary mid-reveal, freeze it and hold the
        // dimmed line for `HANDOFF_HOLD_TICKS` before resuming. A finishing
        // tick never opens a hold; single-paragraph content never triggers.
        if next != total && !reduced && crosses_paragraph_boundary(&state.full, from, next) {
            state.shown = next;
            state.hold = HANDOFF_HOLD_TICKS;
            let line = reveal_line_with_theme(&state.full, next, false, true, styling, &theme);
            if let Some(last) = self.messages.last_mut() {
                *last = line;
            }
            return;
        }
        state.shown = next;
        let done = next >= total;
        // The completed line settles to normal weight even inside the
        // emphasis window — a one-tick reveal must not linger bold.
        let emphasis = !done && !reduced && state.ticks < FIRST_TOKEN_EMPHASIS_TICKS;
        let line = reveal_line_with_theme(&state.full, next, emphasis, false, styling, &theme);
        if let Some(last) = self.messages.last_mut() {
            *last = line;
        }
        if done {
            self.reveal = None;
        }
    }

    /// Push a line into the chat view, first cancelling any active reveal so
    /// a reveal can never keep mutating a line that a subsequent event
    /// replaced or refilled.
    fn push_line(&mut self, line: Line<'static>) {
        self.cancel_reveal();
        self.messages.push(line);
    }

    /// Cancel an active reveal in place: materialize its full text onto the
    /// line slot it was animating and clear the reveal state, so a competing
    /// line can safely take over the chat buffer. No-op when idle.
    fn cancel_reveal(&mut self) {
        if let Some(reveal) = self.reveal.take() {
            if let Some(last) = self.messages.last_mut() {
                // Settle to the final markdown-lite + gutter rendering, not
                // plain text, so the abandoned slot matches a completed line.
                let theme = self.cli_theme;
                *last = chat_line_with_theme(
                    ChatRole::Assistant,
                    &reveal.full,
                    styling_enabled(),
                    &theme,
                );
            }
        }
    }

    pub(crate) fn handle_key(&mut self, event: Event) -> Action {
        let Event::Key(key) = event else { return Action::None };
        if key.kind != KeyEventKind::Press {
            return Action::None;
        }

        if self.approval_state.prompt().is_some() {
            if key.code == KeyCode::Char('c')
                && key.modifiers.contains(crossterm::event::KeyModifiers::CONTROL)
            {
                self.approval_state.resolve(ApprovalDecision::Deny);
                return if self.running { Action::Cancel } else { Action::None };
            }
            let decision = match key.code {
                KeyCode::Char('y') | KeyCode::Enter => Some(ApprovalDecision::Approve),
                KeyCode::Char('a') => Some(ApprovalDecision::ApproveAllForSession),
                KeyCode::Char('n') | KeyCode::Esc => Some(ApprovalDecision::Deny),
                _ => None,
            };
            if let Some(decision) = decision {
                self.approval_state.resolve(decision);
            }
            return Action::None;
        }

        if self.approval_state.intent_prompt().is_some() {
            if key.code == KeyCode::Char('c')
                && key.modifiers.contains(crossterm::event::KeyModifiers::CONTROL)
            {
                self.approval_state.resolve_intent(None);
                return if self.running { Action::Cancel } else { Action::None };
            }
            if let Some(selected) = self.intent_key_choice(key.code) {
                self.approval_state.resolve_intent(selected);
            }
            return Action::None;
        }

        if let Some(plan) = self.approval_state.plan_prompt() {
            if key.code == KeyCode::Char('c')
                && key.modifiers.contains(crossterm::event::KeyModifiers::CONTROL)
            {
                self.approval_state.resolve_plan(plan.session_id, &plan.plan_id, None);
                self.plan_scroll = 0;
                return if self.running { Action::Cancel } else { Action::None };
            }
            match key.code {
                KeyCode::Enter | KeyCode::Char('a') => {
                    self.approval_state.resolve_plan(
                        plan.session_id,
                        &plan.plan_id,
                        Some(PlanDecision::Apply),
                    );
                    self.plan_scroll = 0;
                }
                KeyCode::Char('r') => {
                    self.approval_state.resolve_plan(
                        plan.session_id,
                        &plan.plan_id,
                        Some(PlanDecision::Replan),
                    );
                    self.plan_scroll = 0;
                }
                KeyCode::Char('q') | KeyCode::Char('n') | KeyCode::Esc => {
                    self.approval_state.resolve_plan(plan.session_id, &plan.plan_id, None);
                    self.plan_scroll = 0;
                }
                // The plan body is up to 16 KiB — page through it.
                KeyCode::Char('j') | KeyCode::Down => {
                    self.plan_scroll = self.plan_scroll.saturating_add(1);
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    self.plan_scroll = self.plan_scroll.saturating_sub(1);
                }
                _ => {}
            }
            return Action::None;
        }

        if key.code == KeyCode::Char('c')
            && key.modifiers.contains(crossterm::event::KeyModifiers::CONTROL)
        {
            return if self.running { Action::Cancel } else { Action::Quit };
        }

        match self.screen {
            Screen::Chat => self.handle_chat_key(key.code),
            Screen::Settings => self.handle_settings_key(key.code),
            Screen::Sessions => self.handle_sessions_key(key.code),
            Screen::ToolLog => self.handle_tool_log_key(key.code),
            Screen::AgentAssignments => self.handle_agent_assignments_key(key.code),
        }
    }

    /// Map an intent-prompt keypress to the outcome to resolve.
    ///
    /// Digits `1..=N` pick the Nth listed outcome (confirming it); Enter
    /// confirms the first (primary) option; `q`/`n`/Esc reject (`None`).
    /// Returns `None` when the key does not apply to the pending intent prompt.
    fn intent_key_choice(&self, key: KeyCode) -> Option<Option<RequestedOutcome>> {
        let prompt = self.approval_state.intent_prompt()?;
        match key {
            KeyCode::Enter => prompt.options.first().copied().map(Some),
            KeyCode::Char(digit) if digit.is_ascii_digit() => {
                let index = digit.to_digit(10)? as usize;
                prompt.options.get(index.saturating_sub(1)).copied().map(Some)
            }
            KeyCode::Char('q') | KeyCode::Char('n') | KeyCode::Esc => Some(None),
            _ => None,
        }
    }

    fn handle_chat_key(&mut self, key: KeyCode) -> Action {
        // Project picker mode takes over normal input.
        if self.project_picker_mode {
            match key {
                KeyCode::Esc => {
                    self.project_picker_mode = false;
                    self.input.clear();
                }
                KeyCode::Enter => {
                    let path = std::mem::take(&mut self.input);
                    self.project_picker_mode = false;
                    if !path.trim().is_empty() {
                        let new_dir = PathBuf::from(path.trim());
                        let rt = tokio::runtime::Handle::current();
                        self.switch_project(new_dir, &rt);
                    }
                }
                KeyCode::Char(c) => self.input.push(c),
                KeyCode::Backspace => {
                    self.input.pop();
                }
                _ => {}
            }
            return Action::None;
        }

        match key {
            KeyCode::Esc => self.input_mode = !self.input_mode,
            KeyCode::Enter if self.input_mode && !self.running => {
                let input = std::mem::take(&mut self.input);
                if !input.trim().is_empty() {
                    return Action::Dispatch(input);
                }
            }
            KeyCode::Char(character) if self.input_mode => self.input.push(character),
            KeyCode::Backspace if self.input_mode => {
                self.input.pop();
            }
            KeyCode::Char('s') if !self.input_mode => {
                self.screen = Screen::Settings;
                self.settings_index = 0;
            }
            KeyCode::Char('l') if !self.input_mode => {
                self.screen = Screen::Sessions;
                self.sessions_index = 0;
                self.load_sessions();
            }
            KeyCode::Char('t') if !self.input_mode => {
                self.screen = Screen::ToolLog;
            }
            KeyCode::Char('n') if !self.input_mode => return Action::NewSession,
            KeyCode::Char('p') if !self.input_mode => {
                self.project_picker_mode = true;
                self.input.clear();
            }
            KeyCode::Char('q') if !self.input_mode => return Action::Quit,
            KeyCode::Up if !self.input_mode => self.scroll = self.scroll.saturating_add(1),
            KeyCode::Down if !self.input_mode => self.scroll = self.scroll.saturating_sub(1),
            _ => {}
        }
        Action::None
    }

    /// Build the agent assignments list from config, or default to all known roles.
    fn prepare_agent_assignments(&mut self) {
        if !self.agent_assignments.is_empty() {
            return;
        }
        let default_model = self.selected_model.clone();
        let default_provider_id = self
            .global_config
            .as_ref()
            .and_then(|c| c.model_settings.as_ref())
            .and_then(|ms| ms.providers.first())
            .map(|p| p.id.clone())
            .unwrap_or_default();
        self.agent_assignments = vec![
            AgentModelAssignment {
                agent_role: "Architect".into(),
                provider_config_id: default_provider_id.clone(),
                model_override: Some(default_model.clone()),
            },
            AgentModelAssignment {
                agent_role: "Researcher".into(),
                provider_config_id: default_provider_id.clone(),
                model_override: Some(default_model.clone()),
            },
            AgentModelAssignment {
                agent_role: "Coder".into(),
                provider_config_id: default_provider_id.clone(),
                model_override: Some(default_model.clone()),
            },
            AgentModelAssignment {
                agent_role: "Reviewer".into(),
                provider_config_id: default_provider_id.clone(),
                model_override: Some(default_model.clone()),
            },
            AgentModelAssignment {
                agent_role: "Validator".into(),
                provider_config_id: default_provider_id,
                model_override: Some(default_model),
            },
        ];
    }

    fn handle_settings_key(&mut self, key: KeyCode) -> Action {
        match key {
            KeyCode::Esc | KeyCode::Char('q') => self.screen = Screen::Chat,
            KeyCode::Up | KeyCode::Char('k') => {
                self.settings_index = self.settings_index.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.settings_index =
                    (self.settings_index + 1).min(SettingsField::ALL.len().saturating_sub(1));
            }
            KeyCode::Enter | KeyCode::Right | KeyCode::Left => {
                let field = SettingsField::ALL[self.settings_index];
                if field == SettingsField::AgentAssignments && key == KeyCode::Enter {
                    // Navigate to agent assignments sub-screen.
                    self.prepare_agent_assignments();
                    self.screen = Screen::AgentAssignments;
                    self.agent_assignment_index = 0;
                } else {
                    let reverse = key == KeyCode::Left;
                    let persist = self.cycle_setting(field, reverse);
                    if persist {
                        self.save_global_config();
                    }
                }
            }
            _ => {}
        }
        Action::None
    }

    fn load_sessions(&mut self) {
        let Some(manager) = &self.session_manager else {
            self.sessions_list.clear();
            return;
        };
        let Ok(project) = camino::Utf8PathBuf::from_path_buf(self.project_dir.clone()) else {
            return;
        };
        let rt = tokio::runtime::Handle::current();
        self.sessions_list = rt
            .block_on(manager.list_project_sessions(&project, 50, CancellationToken::new()))
            .unwrap_or_default();
    }

    fn handle_sessions_key(&mut self, key: KeyCode) -> Action {
        match key {
            KeyCode::Esc | KeyCode::Char('q') => self.screen = Screen::Chat,
            KeyCode::Up | KeyCode::Char('k') => {
                self.sessions_index = self.sessions_index.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                let max = self.sessions_list.len().saturating_sub(1);
                self.sessions_index = (self.sessions_index + 1).min(max);
            }
            KeyCode::Enter if !self.sessions_list.is_empty() => {
                let session = &self.sessions_list[self.sessions_index];
                self.session_id = Some(session.id);
                self.screen = Screen::Chat;
                // Reload history into the chat display.
                self.resume_session_display(session.id);
            }
            _ => {}
        }
        Action::None
    }

    fn handle_tool_log_key(&mut self, key: KeyCode) -> Action {
        match key {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('t') => self.screen = Screen::Chat,
            KeyCode::Char('c') => self.clear_tool_log(),
            _ => {}
        }
        Action::None
    }

    fn handle_agent_assignments_key(&mut self, key: KeyCode) -> Action {
        match key {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.screen = Screen::Settings;
                // Persist agent assignments back to global config.
                if let Some(global) = &mut self.global_config {
                    let ms = global.model_settings.get_or_insert_with(ModelSettings::default);
                    ms.agent_assignments = self.agent_assignments.clone();
                }
                self.save_global_config();
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.agent_assignment_index = self.agent_assignment_index.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                let max = self.agent_assignments.len().saturating_sub(1);
                self.agent_assignment_index = (self.agent_assignment_index + 1).min(max);
            }
            KeyCode::Enter | KeyCode::Right | KeyCode::Left
                if !self.agent_assignments.is_empty() && !self.model_choices.is_empty() =>
            {
                // Cycle model for the selected agent role.
                let idx = self.agent_assignment_index;
                let current_model = self.agent_assignments[idx]
                    .model_override
                    .as_deref()
                    .unwrap_or(&self.selected_model);
                let current =
                    self.model_choices.iter().position(|m| m == current_model).unwrap_or(0);
                let next = if key == KeyCode::Left {
                    current.checked_sub(1).unwrap_or(self.model_choices.len() - 1)
                } else {
                    (current + 1) % self.model_choices.len()
                };
                self.agent_assignments[idx].model_override = Some(self.model_choices[next].clone());
                // Also update the provider_config_id to match the model's provider.
                if let Some(ms) =
                    self.global_config.as_ref().and_then(|c| c.model_settings.as_ref())
                {
                    if let Some(provider) =
                        concerto_providers::factory::ProviderFactory::config_for_model(
                            ms,
                            &self.model_choices[next],
                            None,
                        )
                    {
                        self.agent_assignments[idx].provider_config_id = provider.id.clone();
                    }
                }
            }
            _ => {}
        }
        Action::None
    }

    fn resume_session_display(&mut self, session_id: Ulid) {
        let Some(manager) = &self.session_manager else { return };
        let rt = tokio::runtime::Handle::current();
        let history =
            match rt.block_on(manager.load_recent_messages(session_id, CancellationToken::new())) {
                Ok(h) => h,
                Err(_) => return,
            };
        self.messages.clear();
        self.push_line(Line::from(format!("Resumed session {session_id}")));
        let styling = styling_enabled();
        let theme = self.cli_theme;
        for message in &history {
            match message.role {
                concerto_core::types::Role::User => {
                    self.push_line(chat_line_with_theme(
                        ChatRole::User,
                        &message.content,
                        styling,
                        &theme,
                    ));
                }
                concerto_core::types::Role::Assistant if !message.content.trim().is_empty() => {
                    self.push_line(chat_line_with_theme(
                        ChatRole::Assistant,
                        &message.content,
                        styling,
                        &theme,
                    ));
                }
                _ => {}
            }
        }
    }

    fn cycle_setting(&mut self, field: SettingsField, reverse: bool) -> bool {
        match field {
            SettingsField::Model => {
                if self.model_choices.is_empty() {
                    return false;
                }
                let current = self
                    .model_choices
                    .iter()
                    .position(|model| model == &self.selected_model)
                    .unwrap_or(0);
                let next = if reverse {
                    current.checked_sub(1).unwrap_or(self.model_choices.len() - 1)
                } else {
                    (current + 1) % self.model_choices.len()
                };
                self.selected_model = self.model_choices[next].clone();
                if let Some(global) = &mut self.global_config {
                    global
                        .model_settings
                        .get_or_insert_with(Default::default)
                        .global_default_model = Some(self.selected_model.clone());
                }
                true
            }
            SettingsField::Provider => {
                let settings = self
                    .global_config
                    .as_ref()
                    .and_then(|c| c.model_settings.as_ref())
                    .map(|s| s.providers.as_slice())
                    .unwrap_or_default();
                if settings.is_empty() {
                    return false;
                }
                // Find current provider index from the selected model.
                let current = settings
                    .iter()
                    .position(|provider| {
                        let definition = concerto_providers::provider_defs::provider_definition(
                            &provider.provider,
                        );
                        let model_options = concerto_providers::provider_defs::model_options_for(
                            provider,
                            &definition,
                            None,
                        );
                        model_options.contains(&self.selected_model)
                            || provider.cached_models.contains(&self.selected_model)
                            || provider.model == self.selected_model
                    })
                    .unwrap_or(0);
                let next = if reverse {
                    current.checked_sub(1).unwrap_or(settings.len() - 1)
                } else {
                    (current + 1) % settings.len()
                };
                let provider = &settings[next];
                // Update model to this provider's first option or its default.
                let definition =
                    concerto_providers::provider_defs::provider_definition(&provider.provider);
                let models = concerto_providers::provider_defs::model_options_for(
                    provider,
                    &definition,
                    None,
                );
                let new_model = models
                    .first()
                    .or_else(|| provider.cached_models.first())
                    .cloned()
                    .unwrap_or_default();
                if !new_model.is_empty() && new_model != self.selected_model {
                    self.selected_model = new_model;
                    // Rebuild model_choices scoped to this provider.
                    self.model_choices = models;
                }
                if let Some(global) = &mut self.global_config {
                    global
                        .model_settings
                        .get_or_insert_with(Default::default)
                        .global_default_model = Some(self.selected_model.clone());
                }
                true
            }
            SettingsField::PolicyPreset => {
                let current = self.policy_label();
                let Some(global) = &mut self.global_config else { return false };
                global.policy = match (current, reverse) {
                    ("safe", false) | ("permissive", true) => Some(strict_policy()),
                    ("strict", false) | ("safe", true) => Some(permissive_policy()),
                    _ => None,
                };
                true
            }
            SettingsField::MultiAgent => {
                self.multi_agent = !self.multi_agent;
                if let Some(global) = &mut self.global_config {
                    global
                        .multi_agent
                        .get_or_insert_with(MultiAgentConfig::default)
                        .default_enabled = self.multi_agent;
                }
                true
            }
            SettingsField::FastMode => {
                self.fast = !self.fast;
                false
            }
            SettingsField::AgentAssignments => {
                // Handled in handle_settings_key directly (navigation, not cycle).
                false
            }
        }
    }

    fn save_global_config(&mut self) {
        let (Some(global), Some(path)) =
            (self.global_config.as_ref(), concerto_config::default_config_path())
        else {
            return;
        };
        if let Err(error) = concerto_config::save_config(global, &path) {
            self.push_line(Line::from(format!("Could not save settings: {error}")));
            return;
        }
        match concerto_config::load_config(Some(&path), Some(&self.project_dir)) {
            Ok(effective) => {
                self.model_choices = available_models(&effective);
                self.config = Some(effective);
            }
            Err(error) => {
                self.push_line(Line::from(format!("Could not reload project config: {error}")));
            }
        }
    }
}

fn default_model(config: &AppConfig) -> String {
    config
        .model_settings
        .as_ref()
        .and_then(|settings| {
            settings
                .global_default_model
                .clone()
                .or_else(|| settings.providers.first().map(|provider| provider.model.clone()))
        })
        .or_else(|| config.primary_provider_config.as_ref().map(|provider| provider.model.clone()))
        .unwrap_or_default()
}

fn available_models(config: &AppConfig) -> Vec<String> {
    let mut models = Vec::new();
    if let Some(settings) = &config.model_settings {
        for provider in &settings.providers {
            let definition = provider_definition(&provider.provider);
            models.extend(model_options_for(provider, &definition, None));
            if !provider.model.trim().is_empty() {
                models.push(provider.model.clone());
            }
            models.extend(
                provider.cached_models.iter().filter(|model| !model.trim().is_empty()).cloned(),
            );
        }
        models.extend(
            settings
                .agent_assignments
                .iter()
                .filter_map(|assignment| assignment.model_override.clone())
                .filter(|model| !model.trim().is_empty()),
        );
        if let Some(default) =
            settings.global_default_model.as_ref().filter(|model| !model.trim().is_empty())
        {
            models.push(default.clone());
        }
    }
    if models.is_empty() {
        let fallback = default_model(config);
        if !fallback.is_empty() {
            models.push(fallback);
        }
    }
    models.sort();
    models.dedup();
    models
}

/// Resolve per-run preferences from the effective config and the remembered
/// CLI flags (ADR-57 D5). CLI flags win when present; otherwise the file is
/// truth. Pure so reload behavior is unit-testable.
fn resolve_run_prefs(effective: &AppConfig, flags: &RunFlags) -> ResolvedRunPrefs {
    let multi_agent = flags.multi_agent.unwrap_or_else(|| {
        effective.multi_agent.as_ref().map(|settings| settings.default_enabled).unwrap_or(false)
    });
    // `fast` has no config key: absent flag means off (matches the pre-reload
    // startup derivation).
    let fast = flags.fast.unwrap_or(false);
    // Reduced-motion precedence: explicit flag > CONCERTO_REDUCED_MOTION env
    // > config file > default (false). The CLI has no scan-line; this flag
    // gates the bold/hold/rule cues (all already respect it). NO_COLOR stays
    // independent (`styling_enabled` is untouched).
    let reduced_motion = concerto_config::resolve_reduced_motion(flags.reduced_motion, effective);
    // Terminal-title precedence: explicit flag > CONCERTO_NO_TITLE env >
    // config file > default (true). Resolved here so reloads re-apply the
    // remembered flag like the other run prefs.
    let terminal_title_enabled =
        concerto_config::resolve_terminal_title_enabled(flags.no_terminal_title, effective);
    // Theme precedence: explicit flag > CONCERTO_THEME env > config file >
    // default (Midnight). Resolved here so reloads re-apply the remembered
    // flag like the other run prefs.
    let cli_theme = crate::theme::resolve_cli_theme(flags.theme.as_deref(), effective);
    let selected_model = default_model(effective);
    let model_choices = available_models(effective);
    let agent_assignments = effective
        .model_settings
        .as_ref()
        .map(|settings| settings.agent_assignments.clone())
        .unwrap_or_default();
    ResolvedRunPrefs {
        multi_agent,
        fast,
        reduced_motion,
        terminal_title_enabled,
        cli_theme,
        selected_model,
        model_choices,
        agent_assignments,
    }
}

/// Decide the outcome of a per-run reload (ADR-57 D5). The equality
/// short-circuit makes self-induced writes (settings save, toggle, model
/// cycler) provably inert: an unchanged effective config must not re-derive
/// run prefs and clobber in-app overrides.
fn decide_reload(
    current: Option<&AppConfig>,
    effective: &AppConfig,
    flags: &RunFlags,
) -> ReloadOutcome {
    if current == Some(effective) {
        ReloadOutcome::Unchanged
    } else {
        ReloadOutcome::Changed(resolve_run_prefs(effective, flags))
    }
}

fn strict_policy() -> PolicyConfig {
    PolicyConfig {
        rules: vec![PolicyRuleDef {
            action: "require_approval".into(),
            condition: ConditionDef::Always { always: true },
        }],
        time_window: None,
    }
}

fn permissive_policy() -> PolicyConfig {
    let mut rules = [r"rm\s+(-rf\s+)?/", r"dd\s+if=", r"mkfs", r":\(\)\{\s*:\|:&\s*\};:"]
        .into_iter()
        .map(|pattern| PolicyRuleDef {
            action: "auto_deny".into(),
            condition: ConditionDef::CommandPattern { command_pattern: pattern.into() },
        })
        .collect::<Vec<_>>();
    rules.push(PolicyRuleDef {
        action: "auto_approve".into(),
        condition: ConditionDef::Always { always: true },
    });
    PolicyConfig { rules, time_window: None }
}

/// The first `shown` characters of `full`. Clamps naturally (a count past the
/// end yields the whole string) and is char-boundary safe because `chars()`
/// iterates Unicode scalar values, so multi-byte content is never sliced in
/// the middle of a code point.
fn prefix(full: &str, shown: usize) -> String {
    full.chars().take(shown).collect()
}

/// Startup signature staff: 2 static lines (≤3) printed as the first chat
/// lines in `App::new` — an ASCII staff plus the version/tagline. Static
/// text (reduced-motion irrelevant). Styled on a color TTY; plain ASCII
/// (no symbol beyond `-`) under `NO_COLOR`/off-TTY so nothing non-ASCII
/// leaks into pipes. `styling` is a parameter (like `reveal_line`) so the
/// plain side stays unit-testable off-TTY.
fn startup_signature_lines(styling: bool) -> Vec<Line<'static>> {
    use ratatui::style::{Modifier, Style};
    let version = env!("CARGO_PKG_VERSION");
    if styling {
        vec![
            Line::from(Span::styled(
                "♪ ─── ♪ ─── ♪",
                Style::default().add_modifier(Modifier::BOLD),
            )),
            Line::from(format!("CONCERTO v{version} — local-first • policy-governed")),
        ]
    } else {
        vec![
            Line::from("- - - CONCERTO - - -"),
            Line::from(format!("CONCERTO v{version} - local-first - policy-governed")),
        ]
    }
}
/// Terminal title for a run stage (OSC broadcaster, pure — no I/O).
///
/// Four titles cover the run lifecycle: early stages (Understand/Inspect/
/// Plan) are `Tuning`, doing stages (Execute/Verify) are `Performing`, an
/// idle run (no stage, or Complete) is `Done`, and a pending approval
/// overrides any stage with `Awaiting approval`.
pub fn title_for_stage(stage: Option<RunStage>, awaiting_approval: bool) -> String {
    if awaiting_approval {
        return "Concerto: Awaiting approval".to_string();
    }
    match stage {
        None | Some(RunStage::Complete) => "Concerto: Done".to_string(),
        Some(RunStage::Execute) | Some(RunStage::Verify) => "Concerto: Performing".to_string(),
        // `RunStage` is non-exhaustive: unknown future stages read Tuning
        // (grounding) rather than failing to compile at the next addition.
        Some(RunStage::Understand) | Some(RunStage::Inspect) | Some(RunStage::Plan) | Some(_) => {
            "Concerto: Tuning".to_string()
        }
    }
}

/// Pure emit gate for the OSC terminal-title broadcaster (unit-testable; no I/O).
///
/// Titles emit only when the resolved pref is enabled AND stdout is a TTY
/// AND the run is not under CI (`CI` env present). The anti-flicker half of
/// the contract is call-site discipline: `sync_terminal_title` runs only on
/// stage transitions (never per-frame), so the escape is written at most
/// once per transition.
pub(crate) fn should_emit_terminal_title(enabled: bool, is_tty: bool, is_ci: bool) -> bool {
    enabled && is_tty && !is_ci
}

/// Write one OSC `ESC ] 0 ; title BEL` sequence to stdout (no newline).
/// Caller-owned gating: only call after `should_emit_terminal_title`.
fn emit_terminal_title(title: &str) {
    use std::io::Write as _;
    let mut out = std::io::stdout();
    let _ = write!(out, "\x1b]0;{title}\x07");
    let _ = out.flush();
}
/// Styling gate for the CLI motion cues (bold first token, dim handoff,
/// dimmed wipe rule): cues render only on a color TTY. Under `NO_COLOR` or
/// off-TTY every line stays plain so no ANSI escapes leak into pipes — the
/// same contract as `thinking_header`. A parameter at the call boundary (not
/// read inside `reveal_line`) so the cues stay unit-testable off-TTY.
/// `pub(crate)` so the tool-log screen (`ui`) shares the identical gate for
/// its provenance rail colors (rail text itself is state and always present).
pub(crate) fn styling_enabled() -> bool {
    use std::io::IsTerminal as _;
    std::env::var("NO_COLOR").is_err() && std::io::stdout().is_terminal()
}

/// Styled reveal line for the assistant typewriter: the `full[..shown]`
/// prefix rendered as markdown-lite (`ui::markdown_spans`) behind the role
/// gutter (`♪`, green on a color TTY), with the Score motion cues layered on
/// top — a bold first word while `emphasis` (first-token emphasis, prototype
/// #6), a dim whole line while `holding` (paragraph handoff, prototype #5).
/// Plain (symbols only) when `styling` is false (`NO_COLOR`/off-TTY, see
/// `styling_enabled`).
#[allow(dead_code)]
fn reveal_line(
    full: &str,
    shown: usize,
    emphasis: bool,
    holding: bool,
    styling: bool,
) -> Line<'static> {
    reveal_line_with_theme(full, shown, emphasis, holding, styling, &CliTheme::by_name("Midnight"))
}

/// Theme-resolved reveal line; `reveal_line` keeps the `Midnight` contract.
fn reveal_line_with_theme(
    full: &str,
    shown: usize,
    emphasis: bool,
    holding: bool,
    styling: bool,
    theme: &CliTheme,
) -> Line<'static> {
    use ratatui::style::Modifier;
    let text = prefix(full, shown);
    let mut line = chat_line_with_theme(ChatRole::Assistant, &text, styling, theme);
    if !styling {
        return line;
    }
    if holding {
        for span in line.spans.iter_mut() {
            span.style = span.style.add_modifier(Modifier::DIM);
        }
        return line;
    }
    if emphasis {
        bold_first_token(&mut line);
    }
    line
}

/// Bold the first whitespace-delimited token of the body (every span past
/// the gutter at index 0), splitting the span that holds the boundary.
/// A prefix with no whitespace yet is one token — bold it all (mirrors the
/// desktop truncated-reveal path). Markdown-lite spans underneath keep
/// their own styles; this only adds `BOLD`.
fn bold_first_token(line: &mut Line<'static>) {
    use ratatui::style::Modifier;
    for index in 1..line.spans.len() {
        let content = line.spans[index].content.to_string();
        if let Some(pos) = content.find(char::is_whitespace) {
            let (head, tail) = content.split_at(pos);
            let base = line.spans[index].style;
            line.spans[index] =
                ratatui::text::Span::styled(head.to_string(), base.add_modifier(Modifier::BOLD));
            line.spans.insert(index + 1, ratatui::text::Span::styled(tail.to_string(), base));
            for span in line.spans.iter_mut().skip(1).take(index - 1) {
                span.style = span.style.add_modifier(Modifier::BOLD);
            }
            return;
        }
    }
    for span in line.spans.iter_mut().skip(1) {
        span.style = span.style.add_modifier(Modifier::BOLD);
    }
}

/// Wipe-analog rule above each new assistant turn (Score prototype #7).
/// Dimmed on a color TTY, plain under `NO_COLOR`/off-TTY. The reduced-motion
/// gate lives with the caller (`ingest_ui_line`), which skips the rule.
fn wipe_rule_line(styling: bool) -> Line<'static> {
    use ratatui::style::{Modifier, Style};
    if styling {
        Line::from(Span::styled(WIPE_RULE, Style::default().add_modifier(Modifier::DIM)))
    } else {
        Line::from(WIPE_RULE)
    }
}

/// Whether the reveal frontier crossed a paragraph boundary between `from`
/// (exclusive) and `to` (inclusive), measured in characters. Same contract
/// as the desktop Baton cue: a boundary is a blank line — an empty or
/// whitespace-only line terminated by `\n` with more content following it —
/// covering `\n\n`, `\r\n\r\n`, and whitespace-only separator lines.
/// Single-paragraph text (and a trailing newline run with no content after
/// it) yields `false`, so the handoff cue falls back to no cue.
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

/// ANSI counterpart of the desktop `agent_roles` palette map: one stable
/// color per specialist role so CLI headers match desktop buckets.
///
/// Kept as the `Midnight` contract; use [`agent_ansi_color_themed`] (or
/// [`CliTheme::agent_color`]) for the active theme.
#[allow(dead_code)]
fn agent_ansi_color(role: &str) -> ratatui::style::Color {
    agent_ansi_color_themed(&CliTheme::by_name("Midnight"), role)
}

/// Theme-resolved agent color (V2 score-accordion extension).
fn agent_ansi_color_themed(theme: &CliTheme, role: &str) -> ratatui::style::Color {
    theme.agent_color(role)
}

/// Per-agent thinking header (V2 score accordion). Colorized `[‖ agent]`
/// on a color TTY; plain `[+ agent]` under `NO_COLOR` or off-TTY so no ANSI
/// escapes leak into pipes.
#[allow(dead_code)]
pub(crate) fn thinking_header(agent: &str) -> Line<'static> {
    thinking_header_with_theme(agent, &CliTheme::by_name("Midnight"))
}

/// Theme-resolved thinking header; `thinking_header` keeps the `Midnight`
/// contract.
pub(crate) fn thinking_header_with_theme(agent: &str, theme: &CliTheme) -> Line<'static> {
    use ratatui::style::{Modifier, Style};
    use std::io::IsTerminal as _;
    let use_color = std::env::var("NO_COLOR").is_err() && std::io::stdout().is_terminal();
    if use_color {
        Line::from(Span::styled(
            format!("[‖ {agent}]"),
            Style::default().fg(agent_ansi_color_themed(theme, agent)).add_modifier(Modifier::BOLD),
        ))
    } else {
        Line::from(format!("[+ {agent}]"))
    }
}

fn event_line(kind: &EventKind) -> Option<String> {
    match kind {
        EventKind::AssistantMessage { content, .. } => Some(content.clone()),
        // -- tool lifecycle --
        // Live tool blocks carry the plain provenance rail suffix (no theme
        // available here; `UiLine::Text` renders unstyled, so pipes stay
        // clean). The restore path adds themed colors via
        // `transcript_lines_with_theme`.
        EventKind::ToolExecutionStarted { tool_name, detail, .. } => Some(format!(
            "· {tool_name}: {} ‖ approval needed",
            detail.as_deref().unwrap_or("running")
        )),
        EventKind::ToolExecutionFinished { tool_name, duration_ms, success, .. } => Some(format!(
            "· {tool_name} {} ({} ms) ‖ policy ok",
            if *success { "completed" } else { "failed" },
            duration_ms
        )),
        EventKind::ToolTimeout { tool_name, timeout_secs } => {
            Some(format!("· {tool_name} timed out after {timeout_secs}s ‖ denied"))
        }
        // -- agent activity (LowLevel thoughts never reach chat surfaces) --
        EventKind::AgentThought { agent_id, content, kind } => {
            if *kind == ThinkingKind::LowLevel {
                None
            } else {
                Some(format!("· [{}] {content}", normalize_agent_id(agent_id)))
            }
        }
        EventKind::SubTaskCreated { role, description, .. } => {
            Some(format!("· [{role:?}] starting: {description}"))
        }
        EventKind::SubTaskCompleted { role, outcome, .. } => {
            Some(format!("· [{role:?}] completed: {outcome}"))
        }
        EventKind::SubTaskNeedsRevision { role, reason, .. } => {
            Some(format!("· [{role:?}] needs revision: {reason}"))
        }
        EventKind::SubTaskBlocked { role, on, .. } => {
            Some(format!("· [{role:?}] blocked on {on:?}"))
        }
        EventKind::SubTaskCancelled { role, reason, .. } => {
            Some(format!("· [{role:?}] cancelled: {reason}"))
        }
        EventKind::SubTaskFailed { role, error, .. } => {
            Some(format!("· [{role:?}] failed: {error}"))
        }
        EventKind::TaskStarted { task_id, description } => {
            Some(format!("· Task {}: {description}", task_id.0))
        }
        EventKind::TaskCompleted { task_id, success } => Some(format!(
            "· Task {} {}",
            task_id.0,
            if *success { "completed" } else { "finished" }
        )),
        EventKind::TaskFailed { task_id, error } => {
            Some(format!("· Task {} failed: {error}", task_id.0))
        }
        // -- shell output --
        EventKind::ShellOutputChunk { chunk, is_stderr } => {
            let prefix = if *is_stderr { "! " } else { "  " };
            Some(format!("{prefix}{chunk}"))
        }
        // -- spend & context --
        EventKind::SpendUpdated { total_usd, .. } => Some(format!("· Cost: ${total_usd:.4}")),
        EventKind::ContextWindowApproaching { used_tokens, capacity_tokens, .. } => {
            Some(format!("· Context: {used_tokens}/{capacity_tokens} tokens"))
        }
        // -- summarization --
        EventKind::SummarizationStarted { messages_to_summarize, .. } => {
            Some(format!("· Summarizing {messages_to_summarize} messages..."))
        }
        EventKind::SummarizationCompleted { summary_len, .. } => {
            Some(format!("· Summary complete ({summary_len} chars)"))
        }
        // -- indexing --
        EventKind::IndexingProgress { files_processed, files_total, .. } => {
            Some(format!("· Indexing {files_processed}/{files_total}"))
        }
        EventKind::IndexingCompleted { chunk_count, duration_ms, .. } => {
            Some(format!("· Indexing done: {chunk_count} chunks in {duration_ms}ms"))
        }
        // -- embedder degradation (ADR-39) --
        EventKind::EmbedderDegraded { reason, .. } => Some(format!(
            "· Embedding unavailable ({reason}) — semantic search degraded to full-text"
        )),
        // -- session --
        EventKind::SessionSaved => Some("· Session saved".to_string()),
        // -- provider retry --
        EventKind::ProviderRetryScheduled { attempt, delay_ms, reason, .. } => {
            Some(format!("· Provider retry {attempt} in {}s: {reason}", delay_ms / 1000))
        }
        EventKind::ProviderRetryRecovered { attempts, .. } => {
            Some(format!("· Provider recovered after {attempts} attempts"))
        }
        EventKind::ProviderRetryExhausted { attempts, reason, .. } => {
            Some(format!("· Provider retries exhausted after {attempts}: {reason}"))
        }
        _ => None,
    }
}

/// Render a restored durable typed transcript (ADR-36) as chat lines.
///
/// Mirrors the live `event_line` formatting: dimmed `[agent]` activity lines,
/// `· tool` lifecycle lines, and the restore loop's `> user` / assistant style.
/// Tool-call statuses are color-coded (completed green, failed red, approvals
/// and cancellations dim). Pure so it can be unit-tested; the caller decides
/// whether the transcript is present (legacy sessions fall back to messages).
#[allow(dead_code)]
fn transcript_lines(entries: &[TranscriptEntry]) -> Vec<Line<'static>> {
    transcript_lines_with_theme(entries, styling_enabled(), &CliTheme::by_name("Midnight"))
}

/// Provenance rail suffix for tool lines (Blueprint texture `‖ ok`).
/// Read-only mapping from the stored policy/tool status — it never consults
/// the policy engine. The text is always appended; only the color is gated
/// by `styling` (plain symbols under `NO_COLOR`/off-TTY).
fn provenance_rail_text(status: &TranscriptToolStatus) -> &'static str {
    match status {
        TranscriptToolStatus::Denied => " ‖ denied",
        TranscriptToolStatus::Running | TranscriptToolStatus::Cancelled => " ‖ approval needed",
        TranscriptToolStatus::Completed
        | TranscriptToolStatus::Allowed
        | TranscriptToolStatus::Failed => " ‖ policy ok",
    }
}

/// Themed rail color: ok green, approval-needed yellow, denied red.
/// Uses the CLI theme bridge so light/neon themes stay legible.
fn provenance_rail_color(status: &TranscriptToolStatus, theme: &CliTheme) -> ratatui::style::Color {
    match status {
        TranscriptToolStatus::Denied => theme.danger,
        TranscriptToolStatus::Running | TranscriptToolStatus::Cancelled => theme.warning,
        TranscriptToolStatus::Completed
        | TranscriptToolStatus::Allowed
        | TranscriptToolStatus::Failed => theme.success,
    }
}

/// Theme-resolved transcript rendering; `transcript_lines` keeps the
/// `Midnight` contract.
fn transcript_lines_with_theme(
    entries: &[TranscriptEntry],
    styling: bool,
    theme: &CliTheme,
) -> Vec<Line<'static>> {
    use ratatui::style::Style;
    let status_label = |status: &TranscriptToolStatus| match status {
        TranscriptToolStatus::Running => "running",
        TranscriptToolStatus::Completed => "completed",
        TranscriptToolStatus::Failed => "failed",
        TranscriptToolStatus::Allowed => "allowed",
        TranscriptToolStatus::Denied => "denied",
        TranscriptToolStatus::Cancelled => "cancelled",
    };
    let mut lines = Vec::with_capacity(entries.len());
    // Restored lines carry the same role gutters + markdown-lite as live
    // chat (plain symbols off-TTY so tests and pipes stay stable).
    for entry in entries {
        let line = match entry {
            TranscriptEntry::User { content } => {
                chat_line_with_theme(ChatRole::User, content, styling, theme)
            }
            TranscriptEntry::Assistant { content } => {
                chat_line_with_theme(ChatRole::Assistant, content, styling, theme)
            }
            TranscriptEntry::Thinking { agent, content, .. } => {
                let agent = normalize_agent_id(agent);
                let text =
                    if agent.is_empty() { content.clone() } else { format!("[{agent}] {content}") };
                Line::from(text).style(Style::default().fg(theme.muted))
            }
            TranscriptEntry::Activity { agent, content } => {
                Line::from(format!("[{agent}] {content}")).style(Style::default().fg(theme.muted))
            }
            TranscriptEntry::Summary { content } => {
                Line::from(format!("[context] {content}")).style(Style::default().fg(theme.muted))
            }
            TranscriptEntry::ToolCall { tool_name, detail, status } => {
                let label = if detail.is_empty() {
                    format!("· {tool_name}: {}", status_label(status))
                } else {
                    format!("· {tool_name}: {detail} ({})", status_label(status))
                };
                let style = match status {
                    TranscriptToolStatus::Completed => Style::default().fg(theme.success),
                    TranscriptToolStatus::Failed => Style::default().fg(theme.danger),
                    // Running / Allowed / Denied / Cancelled settle to dim.
                    _ => Style::default().fg(theme.muted),
                };
                // Provenance rail: themed suffix on a color TTY, plain
                // symbols under NO_COLOR/off-TTY. The base line style is
                // untouched so status colors keep their contract — and under
                // plain the line stays fully unstyled (no Line-level fg leak).
                let mut line = Line::from(label);
                if styling {
                    line = line.style(style);
                }
                let rail = provenance_rail_text(status);
                if styling {
                    line.spans.push(Span::styled(
                        rail.to_string(),
                        Style::default().fg(provenance_rail_color(status, theme)),
                    ));
                } else {
                    line.spans.push(Span::raw(rail.to_string()));
                }
                line
            }
            TranscriptEntry::Error { content } => {
                Line::from(format!("error: {content}")).style(Style::default().fg(theme.danger))
            }
            TranscriptEntry::Completion { multi_agent, completed, files, .. } => {
                let mode = if *multi_agent { "multi-agent" } else { "single-agent" };
                let outcome = if *completed { "complete" } else { "incomplete" };
                let files_str = if files.is_empty() {
                    "no files changed".to_string()
                } else {
                    files.join(", ")
                };
                Line::from(format!("Run {mode} ({outcome}) — files: {files_str}"))
            }
        };
        lines.push(line);
    }
    lines
}

impl App {
    /// Push a tool event into the ring-buffer tool log.
    fn push_tool_event(&mut self, kind: EventKind) {
        const MAX_LOG: usize = 200;
        let entry = match kind {
            EventKind::ToolExecutionStarted { tool_name, detail, .. } => {
                ToolLogEntry { tool_name, status: ToolStatus::Running, detail, duration_ms: None }
            }
            EventKind::ToolExecutionFinished { tool_name, duration_ms, success, detail } => {
                ToolLogEntry {
                    tool_name,
                    status: if success { ToolStatus::Success } else { ToolStatus::Failure },
                    detail,
                    duration_ms: Some(duration_ms),
                }
            }
            EventKind::ToolTimeout { tool_name, timeout_secs } => ToolLogEntry {
                tool_name,
                status: ToolStatus::Timeout { timeout_secs },
                detail: None,
                duration_ms: None,
            },
            _ => return,
        };
        if self.tool_log.len() >= MAX_LOG {
            self.tool_log.pop_front();
        }
        self.tool_log.push_back(entry);
    }

    pub fn clear_tool_log(&mut self) {
        self.tool_log.clear();
    }
}

pub(crate) enum Action {
    Quit,
    Cancel,
    Dispatch(String),
    NewSession,
    None,
}

#[cfg(test)]
mod tests {
    use super::*;
    use concerto_core::traits::approval::ApprovalSink;
    use crossterm::event::{KeyEvent, KeyEventState, KeyModifiers};

    fn key_event(code: KeyCode, modifiers: KeyModifiers) -> Event {
        Event::Key(KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            state: KeyEventState::empty(),
        })
    }

    #[test]
    fn thought_ingest_logs_headline_and_detail_while_expanded() {
        let mut app = App::new();
        let base = app.messages.len();
        app.ingest_ui_line(UiLine::Thought {
            agent: "coder".into(),
            kind: ThinkingKind::Headline,
            content: "starting".into(),
        });
        app.ingest_ui_line(UiLine::Thought {
            agent: "coder".into(),
            kind: ThinkingKind::Detail,
            content: "tool reasoning".into(),
        });
        assert_eq!(app.thought_log.len(), 2);
        // Header + 2 thought lines.
        assert_eq!(app.messages.len(), base + 3);
        assert_eq!(app.current_agent.as_deref(), Some("coder"));
    }

    #[test]
    fn thinking_toggle_suppresses_detail_and_replays_on_expand() {
        let mut app = App::new();
        app.ingest_ui_line(UiLine::Thought {
            agent: "coder".into(),
            kind: ThinkingKind::Headline,
            content: "starting".into(),
        });
        // Collapse: digest only, future Detail suppressed.
        app.toggle_thinking();
        assert!(!app.thinking_expanded);
        let collapsed_len = app.messages.len();
        app.ingest_ui_line(UiLine::Thought {
            agent: "coder".into(),
            kind: ThinkingKind::Detail,
            content: "hidden detail".into(),
        });
        assert_eq!(app.messages.len(), collapsed_len, "collapsed Detail stays hidden");
        // Expand: exactly the suppressed line replays, no duplicates.
        app.toggle_thinking();
        assert!(app.thinking_expanded);
        assert!(
            app.messages
                .iter()
                .filter(|line| format!("{line:?}").contains("hidden detail"))
                .count()
                == 1
        );
    }

    #[test]
    fn collapse_digest_falls_back_to_detail_for_detail_only_agent() {
        let mut app = App::new();
        app.ingest_ui_line(UiLine::Thought {
            agent: "coder".into(),
            kind: ThinkingKind::Detail,
            content: "older detail".into(),
        });
        app.ingest_ui_line(UiLine::Thought {
            agent: "coder".into(),
            kind: ThinkingKind::Detail,
            content: "latest detail".into(),
        });
        // Collapse: Detail-only agent still gets a digest line (was invisible).
        // Only the post-toggle lines are digest lines (the inline Detail
        // lines rendered at ingest time while expanded).
        let before = app.messages.len();
        app.toggle_thinking();
        assert!(!app.thinking_expanded);
        assert!(app.messages[before..]
            .iter()
            .any(|line| format!("{line:?}").contains("latest detail")));
    }

    #[test]
    fn thought_log_is_bounded_oldest_first() {
        let mut app = App::new();
        for i in 0..(MAX_THOUGHT_LOG + 50) {
            app.ingest_ui_line(UiLine::Thought {
                agent: "coder".into(),
                kind: ThinkingKind::Detail,
                content: format!("detail {i}"),
            });
        }
        assert_eq!(app.thought_log.len(), MAX_THOUGHT_LOG);
        let front = app.thought_log.front().map(|r| r.content.clone()).unwrap_or_default();
        assert_eq!(front, format!("detail {}", 50));
    }

    #[test]
    fn thought_ingest_normalizes_agent_for_grouping() {
        let mut app = App::new();
        app.ingest_ui_line(UiLine::Thought {
            agent: " Coder ".into(),
            kind: ThinkingKind::Headline,
            content: "starting".into(),
        });
        app.ingest_ui_line(UiLine::Thought {
            agent: "CODER".into(),
            kind: ThinkingKind::Detail,
            content: "reasoning".into(),
        });
        // One header (single bucket), normalized grouping keys, original
        // content preserved.
        assert_eq!(app.current_agent.as_deref(), Some("coder"));
        assert!(app.thought_log.iter().all(|record| record.agent == "coder"));
        assert_eq!(app.thought_log.len(), 2);
        // Live lines render the normalized bucket prefix.
        assert_eq!(
            event_line(&EventKind::AgentThought {
                agent_id: " Coder ".into(),
                content: "hi".into(),
                kind: ThinkingKind::Headline,
            }),
            Some("· [coder] hi".to_string())
        );
    }

    #[test]
    fn low_level_thoughts_have_no_chat_line() {
        let kind = EventKind::AgentThought {
            agent_id: "coder".into(),
            content: "[system_instructions]".into(),
            kind: ThinkingKind::LowLevel,
        };
        assert_eq!(event_line(&kind), None);
        let detail = EventKind::AgentThought {
            agent_id: "coder".into(),
            content: "reasoning".into(),
            kind: ThinkingKind::Detail,
        };
        assert!(event_line(&detail).is_some());
    }

    #[test]
    fn thinking_header_is_plain_off_tty() {
        // Test harnesses run off-TTY, so headers must be the plain `[+]`
        // form with no ANSI styling.
        let header = thinking_header("coder");
        let rendered: String = header.spans.iter().map(|span| span.content.as_ref()).collect();
        assert_eq!(rendered, "[+ coder]");
    }

    #[test]
    fn ctrl_c_cancels_a_run_and_quits_when_idle() {
        let mut app = App::new();
        assert!(matches!(
            app.handle_key(key_event(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Action::Quit
        ));
        app.running = true;
        assert!(matches!(
            app.handle_key(key_event(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Action::Cancel
        ));
    }

    // ------------------------------------------------------------------
    // ADR-60 D7 (interrupt-safe resume): the graceful interrupt path.
    // ------------------------------------------------------------------

    fn test_agent_output(session_id: Ulid) -> AgentOutput {
        AgentOutput {
            task_id: concerto_core::types::TaskId(Ulid::new()),
            session_id,
            final_message: "done".into(),
            files_modified: vec![],
            tool_call_count: 0,
            eval_result: None,
            tool_events: vec![],
            verification: vec![],
            project_root: None,
            completion_status: concerto_core::types::AgentCompletionStatus::Completed,
            provider_metrics: vec![],
            checkpoint_json: None,
        }
    }

    /// The shared interrupt path cancels the run's token — the coordinator's
    /// cancel branch persists the interrupted checkpoint off that token.
    #[test]
    fn handle_interrupt_cancels_the_run_token() {
        let mut app = App::new();
        app.running = true;
        let token = app.cancel_token.clone();
        app.handle_interrupt();
        assert!(token.is_cancelled(), "the interrupt cancels the run token");
        assert!(
            app.messages.iter().any(|line| line.to_string().contains("Cancellation requested")),
            "the user is told cancellation was requested"
        );
        assert!(app.running, "the run keeps unwinding; the app stays responsive");
    }

    /// A drained SIGINT takes the same cooperative-cancel path as the Ctrl+C
    /// key (the process no longer dies on `kill -INT` before the run can
    /// checkpoint).
    #[test]
    fn a_drained_sigint_cancels_the_run() {
        let mut app = App::new();
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        app.interrupt_rx = Some(rx);
        app.running = true;
        let token = app.cancel_token.clone();
        tx.send(()).expect("signal sent");
        // Same drain the event loop runs, inline for a headless test.
        if let Some(rx) = app.interrupt_rx.as_ref() {
            let mut interrupted = false;
            while rx.try_recv().is_ok() {
                interrupted = true;
            }
            assert!(interrupted, "the signal was drained");
            if interrupted {
                app.handle_interrupt();
            }
        }
        assert!(token.is_cancelled());
    }

    /// The bounded quit wait returns as soon as the run settles — the
    /// coordinator's cancel path has already persisted its checkpoint by the
    /// time the completion is sent.
    #[test]
    fn await_run_settlement_returns_when_the_run_settles() {
        let mut app = App::new();
        app.running = true;
        let session_id = Ulid::new();
        let tx = app.completion_tx.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            let _ = tx.send(RunCompletion { result: Ok(test_agent_output(session_id)) });
        });
        app.await_run_settlement(Duration::from_secs(5));
        assert!(!app.running, "the settled completion ends the run");
        assert_eq!(app.session_id, Some(session_id), "the session id is retained");
    }

    /// The bounded quit wait NEVER hangs: without a completion it returns at
    /// the deadline with the run still marked in flight (the hard-kill loss
    /// the amendment documents, surfaced instead of a frozen terminal).
    #[test]
    fn await_run_settlement_bounds_the_wait() {
        let mut app = App::new();
        app.running = true;
        let started = std::time::Instant::now();
        app.await_run_settlement(Duration::from_millis(60));
        assert!(app.running, "no completion arrived; the run is still in flight");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the wait is bounded by the grace, got: {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn enter_does_not_overlap_running_dispatches() {
        let mut app = App::new();
        app.input = "second".into();
        app.running = true;
        assert!(matches!(
            app.handle_key(key_event(KeyCode::Enter, KeyModifiers::NONE)),
            Action::None
        ));
        assert_eq!(app.input, "second");
    }

    #[test]
    fn model_choices_are_model_first_and_deduplicated() {
        let config = AppConfig {
            model_settings: Some(concerto_config::ModelSettings {
                providers: vec![concerto_config::ProviderConfig {
                    model: "model-a".into(),
                    cached_models: vec!["model-a".into(), "model-b".into()],
                    ..Default::default()
                }],
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(available_models(&config), vec!["model-a", "model-b"]);
    }

    // ------------------------------------------------------------------
    // App label methods
    // ------------------------------------------------------------------

    #[test]
    fn model_label_not_configured_when_empty() {
        let app = App::new();
        assert_eq!(app.model_label(), "(not configured)");
    }

    #[test]
    fn model_label_returns_selected_model() {
        let mut app = App::new();
        app.selected_model = "gpt-4".into();
        assert_eq!(app.model_label(), "gpt-4");
    }

    #[test]
    fn policy_label_defaults_to_safe() {
        let app = App::new();
        assert_eq!(app.policy_label(), "safe");
    }

    #[test]
    fn approval_prompt_returns_none_when_empty() {
        let app = App::new();
        assert!(app.approval_prompt().is_none());
    }

    #[test]
    fn clear_tool_log_empties_queue() {
        let mut app = App::new();
        app.tool_log.push_back(ToolLogEntry {
            tool_name: "test".into(),
            status: ToolStatus::Success,
            detail: None,
            duration_ms: None,
        });
        assert_eq!(app.tool_log.len(), 1);
        app.clear_tool_log();
        assert!(app.tool_log.is_empty());
    }

    #[test]
    fn new_app_defaults_to_chat_screen() {
        let app = App::new();
        assert_eq!(app.screen, Screen::Chat);
        assert!(app.input_mode);
        assert!(!app.running);
        assert!(!app.multi_agent);
        assert!(!app.fast);
        assert_eq!(app.run_flags, RunFlags::default());
    }

    // ------------------------------------------------------------------
    // Per-run config reload (ADR-57 D5)
    // ------------------------------------------------------------------

    fn config_with_multi_agent(default_enabled: bool) -> AppConfig {
        AppConfig {
            multi_agent: Some(concerto_config::MultiAgentConfig {
                default_enabled,
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn config_with_model(model: &str, cached: &[&str]) -> AppConfig {
        AppConfig {
            model_settings: Some(concerto_config::ModelSettings {
                global_default_model: Some(model.to_string()),
                providers: vec![concerto_config::ProviderConfig {
                    model: model.to_string(),
                    cached_models: cached.iter().map(|m| m.to_string()).collect(),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn resolve_run_prefs_follows_config_when_no_flags() {
        let config = config_with_multi_agent(true);
        let prefs = resolve_run_prefs(&config, &RunFlags::default());
        assert!(prefs.multi_agent, "config default_enabled must be the truth with no -m");
        assert!(!prefs.fast, "fast has no config key and must be off with no -f");
    }

    #[test]
    fn resolve_run_prefs_config_change_applies() {
        // A config edit flipping multi_agent's default must flow into the next run.
        let current = config_with_multi_agent(false);
        let changed = config_with_multi_agent(true);
        let flags = RunFlags::default();
        match decide_reload(Some(&current), &changed, &flags) {
            ReloadOutcome::Changed(prefs) => assert!(prefs.multi_agent),
            ReloadOutcome::Unchanged => panic!("a differing config must re-derive run prefs"),
        }
    }

    #[test]
    fn resolve_run_prefs_flags_override_config() {
        // The file says multi-agent off, but an explicit -m -f must win.
        let config = config_with_multi_agent(false);
        let flags = RunFlags {
            multi_agent: Some(true),
            fast: Some(true),
            reduced_motion: Some(true),
            no_terminal_title: None,
            theme: None,
        };
        let prefs = resolve_run_prefs(&config, &flags);
        assert!(prefs.multi_agent, "an explicit -m must clobber a config default_enabled=false");
        assert!(prefs.fast, "an explicit -f must set fast");
        assert!(prefs.reduced_motion, "an explicit --reduced-motion must set reduced_motion");
    }

    #[test]
    fn resolve_run_prefs_reduced_motion_follows_config() {
        let mut config = config_with_multi_agent(false);
        assert!(
            !resolve_run_prefs(&config, &RunFlags::default()).reduced_motion,
            "default config must leave reduced_motion off"
        );
        config.display.reduced_motion = true;
        assert!(
            resolve_run_prefs(&config, &RunFlags::default()).reduced_motion,
            "config display.reduced_motion must flow into run prefs with no flag"
        );
    }

    // ------------------------------------------------------------------
    // OSC terminal-title broadcaster: pure string + gating (no real writes).
    // ------------------------------------------------------------------

    #[test]
    fn terminal_title_for_stage_covers_the_run_lifecycle() {
        // Idle (no stage yet, or finished) reads Done.
        assert_eq!(title_for_stage(None, false), "Concerto: Done");
        assert_eq!(title_for_stage(Some(RunStage::Complete), false), "Concerto: Done");
        // Grounding stages read Tuning; doing stages read Performing.
        for stage in [RunStage::Understand, RunStage::Inspect, RunStage::Plan] {
            assert_eq!(title_for_stage(Some(stage), false), "Concerto: Tuning", "{stage:?}");
        }
        for stage in [RunStage::Execute, RunStage::Verify] {
            assert_eq!(title_for_stage(Some(stage), false), "Concerto: Performing", "{stage:?}");
        }
        // A pending approval overrides any stage.
        assert_eq!(title_for_stage(Some(RunStage::Execute), true), "Concerto: Awaiting approval");
        assert_eq!(title_for_stage(None, true), "Concerto: Awaiting approval");
    }

    #[test]
    fn terminal_title_emit_gate_needs_tty_without_ci() {
        assert!(should_emit_terminal_title(true, true, false));
        assert!(
            !should_emit_terminal_title(false, true, false),
            "resolved pref off must stay silent"
        );
        assert!(
            !should_emit_terminal_title(true, false, false),
            "pipes must never see the OSC escape"
        );
        assert!(!should_emit_terminal_title(true, true, true), "CI logs must stay clean");
    }

    #[test]
    fn decide_reload_equality_short_circuits() {
        // A self-induced write (settings save) rewrites the same effective
        // config: the reload must be a no-op so in-app overrides survive.
        let config = config_with_multi_agent(true);
        assert!(matches!(
            decide_reload(Some(&config), &config, &RunFlags::default()),
            ReloadOutcome::Unchanged
        ));
    }

    #[test]
    fn decide_reload_first_load_always_derives() {
        // No current config (fresh App): a reload must always re-derive.
        let config = config_with_multi_agent(true);
        assert!(matches!(
            decide_reload(None, &config, &RunFlags::default()),
            ReloadOutcome::Changed(_)
        ));
    }

    #[test]
    fn resolve_run_prefs_model_and_choices_follow_config() {
        let config = config_with_model("model-a", &["model-a", "model-b"]);
        let prefs = resolve_run_prefs(&config, &RunFlags::default());
        assert_eq!(prefs.selected_model, "model-a");
        assert_eq!(prefs.model_choices, vec!["model-a", "model-b"]);
    }

    // ------------------------------------------------------------------
    // Chat input mode key handling
    // ------------------------------------------------------------------

    #[test]
    fn char_input_appends_to_input_buffer() {
        let mut app = App::new();
        assert!(app.input_mode);
        app.handle_key(key_event(KeyCode::Char('h'), KeyModifiers::empty()));
        app.handle_key(key_event(KeyCode::Char('i'), KeyModifiers::empty()));
        assert_eq!(app.input, "hi");
    }

    #[test]
    fn esc_toggles_input_mode_off_and_on() {
        let mut app = App::new();
        assert!(app.input_mode);
        // First Esc toggles off.
        app.handle_key(key_event(KeyCode::Esc, KeyModifiers::empty()));
        assert!(!app.input_mode);
        // Second Esc toggles back on.
        app.handle_key(key_event(KeyCode::Esc, KeyModifiers::empty()));
        assert!(app.input_mode);
    }

    #[test]
    fn enter_dispatches_input_when_not_running() {
        let mut app = App::new();
        app.input = "test message".into();
        let action = app.handle_key(key_event(KeyCode::Enter, KeyModifiers::empty()));
        assert!(matches!(action, Action::Dispatch(msg) if msg == "test message"));
        assert!(app.input.is_empty(), "input buffer should be cleared after dispatch");
    }

    #[test]
    fn enter_with_empty_input_does_not_dispatch() {
        let mut app = App::new();
        let action = app.handle_key(key_event(KeyCode::Enter, KeyModifiers::empty()));
        assert!(matches!(action, Action::None));
    }

    #[test]
    fn backspace_removes_last_char_in_input_mode() {
        let mut app = App::new();
        app.input = "hello".into();
        app.handle_key(key_event(KeyCode::Backspace, KeyModifiers::empty()));
        assert_eq!(app.input, "hell");
    }

    #[test]
    fn backspace_on_empty_input_does_nothing() {
        let mut app = App::new();
        assert!(app.input.is_empty());
        app.handle_key(key_event(KeyCode::Backspace, KeyModifiers::empty()));
        assert!(app.input.is_empty());
    }

    // ------------------------------------------------------------------
    // Command mode screen navigation
    // ------------------------------------------------------------------

    #[test]
    fn command_mode_s_opens_settings() {
        let mut app = App::new();
        // Enter command mode.
        app.handle_key(key_event(KeyCode::Esc, KeyModifiers::empty()));
        app.handle_key(key_event(KeyCode::Char('s'), KeyModifiers::empty()));
        assert_eq!(app.screen, Screen::Settings);
    }

    #[test]
    fn command_mode_l_opens_sessions_screen() {
        let mut app = App::new();
        app.handle_key(key_event(KeyCode::Esc, KeyModifiers::empty()));
        app.handle_key(key_event(KeyCode::Char('l'), KeyModifiers::empty()));
        assert_eq!(app.screen, Screen::Sessions);
    }

    #[test]
    fn command_mode_t_opens_tool_log_screen() {
        let mut app = App::new();
        app.handle_key(key_event(KeyCode::Esc, KeyModifiers::empty()));
        app.handle_key(key_event(KeyCode::Char('t'), KeyModifiers::empty()));
        assert_eq!(app.screen, Screen::ToolLog);
    }

    #[test]
    fn command_mode_n_returns_new_session_action() {
        let mut app = App::new();
        app.handle_key(key_event(KeyCode::Esc, KeyModifiers::empty()));
        let action = app.handle_key(key_event(KeyCode::Char('n'), KeyModifiers::empty()));
        assert!(matches!(action, Action::NewSession));
    }

    #[test]
    fn command_mode_p_activates_project_picker_mode() {
        let mut app = App::new();
        app.handle_key(key_event(KeyCode::Esc, KeyModifiers::empty()));
        assert!(!app.project_picker_mode);
        app.handle_key(key_event(KeyCode::Char('p'), KeyModifiers::empty()));
        assert!(app.project_picker_mode);
    }

    #[test]
    fn command_mode_up_increases_scroll() {
        let mut app = App::new();
        app.handle_key(key_event(KeyCode::Esc, KeyModifiers::empty()));
        assert_eq!(app.scroll, 0);
        app.handle_key(key_event(KeyCode::Up, KeyModifiers::empty()));
        assert_eq!(app.scroll, 1);
        app.handle_key(key_event(KeyCode::Up, KeyModifiers::empty()));
        assert_eq!(app.scroll, 2);
    }

    #[test]
    fn command_mode_down_decreases_scroll_from_floor() {
        let mut app = App::new();
        app.scroll = 3;
        app.handle_key(key_event(KeyCode::Esc, KeyModifiers::empty()));
        app.handle_key(key_event(KeyCode::Down, KeyModifiers::empty()));
        assert_eq!(app.scroll, 2);
        // Scrolling below 0 should saturate at 0.
        app.scroll = 0;
        app.handle_key(key_event(KeyCode::Down, KeyModifiers::empty()));
        assert_eq!(app.scroll, 0);
    }

    // ------------------------------------------------------------------
    // Agent assignments screen — regression: empty model_choices
    // ------------------------------------------------------------------
    // verifies: handle_agent_assignments_key does not panic when
    // model_choices is empty but agent_assignments is non-empty (the
    // model-cycling guard must prevent the checked_sub / % division
    // from operating on a zero-length vec).
    #[test]
    fn agent_assignments_key_empty_model_choices_does_not_panic() {
        let mut app = App::new();
        // model_choices is already empty in App::new()
        assert!(app.model_choices.is_empty());
        app.agent_assignments = vec![concerto_config::AgentModelAssignment {
            agent_role: "coordinator".into(),
            provider_config_id: "default".into(),
            model_override: None,
        }];
        // Call each cycling key — the guard (!model_choices.is_empty())
        // must prevent the arm from matching, returning Action::None.
        assert!(matches!(app.handle_agent_assignments_key(KeyCode::Enter), Action::None));
        assert!(matches!(app.handle_agent_assignments_key(KeyCode::Right), Action::None));
        assert!(matches!(app.handle_agent_assignments_key(KeyCode::Left), Action::None));
        // Also verify that agent_assignment_index is unchanged.
        assert_eq!(app.agent_assignment_index, 0);
    }

    // ------------------------------------------------------------------
    // Startup signature staff (static, ≤3 lines, NO_COLOR/off-TTY plain).
    // ------------------------------------------------------------------

    #[test]
    fn startup_signature_plain_is_ascii_and_versioned() {
        let lines = startup_signature_lines(false);
        assert!(lines.len() <= 3, "signature must stay within 3 lines");
        assert!(!lines.is_empty());
        let joined = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(joined.contains("CONCERTO"), "wordmark present: {joined}");
        assert!(joined.contains(env!("CARGO_PKG_VERSION")), "version present: {joined}");
        for line in &lines {
            let text = line_text(line);
            assert!(text.is_ascii(), "plain signature must be ASCII-only: {text:?}");
            assert!(
                text.chars().all(|c| c.is_alphanumeric() || c == '-' || c == ' ' || c == '.'),
                "plain signature uses no symbol beyond '-': {text:?}"
            );
        }
    }

    #[test]
    fn startup_signature_styled_carries_staff_and_new_app_leads_with_it() {
        let styled = startup_signature_lines(true);
        let joined = styled.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(joined.contains("CONCERTO"), "wordmark present: {joined}");
        assert!(joined.contains(env!("CARGO_PKG_VERSION")), "version present: {joined}");
        // Off-TTY tests get the plain side; the app must still lead with it.
        let app = App::new();
        let plain = startup_signature_lines(false);
        for (index, expected) in plain.iter().enumerate() {
            assert_eq!(line_text(&app.messages[index]), line_text(expected));
        }
    }

    // ------------------------------------------------------------------
    // Transcript restore (ADR-36, stage 3)
    // ------------------------------------------------------------------

    fn line_text(line: &Line) -> String {
        line.spans.iter().map(|span| span.content.to_string()).collect()
    }

    #[test]
    fn transcript_lines_renders_all_entry_variants() {
        use concerto_core::transcript::{TranscriptEntry, TranscriptToolStatus};
        use ratatui::style::Color;

        let entries = vec![
            TranscriptEntry::User { content: "build the widget".into() },
            TranscriptEntry::Thinking {
                agent: "coder".into(),
                content: "step one".into(),
                kind: ThinkingKind::Detail,
            },
            TranscriptEntry::ToolCall {
                tool_name: "fs_write".into(),
                detail: "write main.rs".into(),
                status: TranscriptToolStatus::Completed,
            },
            TranscriptEntry::ToolCall {
                tool_name: "shell".into(),
                detail: String::new(),
                status: TranscriptToolStatus::Allowed,
            },
            TranscriptEntry::ToolCall {
                tool_name: "net".into(),
                detail: String::new(),
                status: TranscriptToolStatus::Failed,
            },
            TranscriptEntry::Activity {
                agent: "Coordinator".into(),
                content: "Delegated subtask T1 to coder".into(),
            },
            TranscriptEntry::Assistant { content: "the fix is in".into() },
            TranscriptEntry::Error { content: "boom".into() },
            TranscriptEntry::Summary { content: "context compacted".into() },
            TranscriptEntry::Completion {
                multi_agent: true,
                completed: true,
                files: vec!["main.rs".into(), "lib.rs".into()],
                project_root: Some("/proj".into()),
            },
        ];
        let lines = transcript_lines_with_theme(&entries, true, &CliTheme::by_name("Midnight"));

        // Line text — mirrors the restore loop / live event_line formatting,
        // now behind the role gutters (`›` user, `♪` assistant).
        assert_eq!(line_text(&lines[0]), "› build the widget");
        assert_eq!(line_text(&lines[1]), "[coder] step one");
        assert_eq!(line_text(&lines[2]), "· fs_write: write main.rs (completed) ‖ policy ok");
        assert_eq!(line_text(&lines[3]), "· shell: allowed ‖ policy ok");
        assert_eq!(line_text(&lines[4]), "· net: failed ‖ policy ok");
        assert_eq!(line_text(&lines[5]), "[Coordinator] Delegated subtask T1 to coder");
        assert_eq!(line_text(&lines[6]), "♪ the fix is in");
        assert_eq!(line_text(&lines[7]), "error: boom");
        assert_eq!(line_text(&lines[8]), "[context] context compacted");
        assert_eq!(line_text(&lines[9]), "Run multi-agent (complete) — files: main.rs, lib.rs");

        // Styling — completed tools green, failures/errors red, activity,
        // thinking, approvals and summaries dimmed.
        assert_eq!(lines[2].style.fg, Some(Color::Green));
        assert_eq!(lines[4].style.fg, Some(Color::Red));
        assert_eq!(lines[7].style.fg, Some(Color::Red));
        assert_eq!(lines[3].style.fg, Some(Color::DarkGray));
        assert_eq!(lines[5].style.fg, Some(Color::DarkGray));
        assert_eq!(lines[1].style.fg, Some(Color::DarkGray));
        assert_eq!(lines[8].style.fg, Some(Color::DarkGray));
    }

    #[test]
    fn transcript_lines_empty_input_yields_no_lines() {
        assert!(transcript_lines(&[]).is_empty());
    }

    #[test]
    fn transcript_lines_incomplete_and_single_agent_completion() {
        use concerto_core::transcript::{TranscriptEntry, TranscriptToolStatus};
        use ratatui::style::Color;

        let lines = transcript_lines_with_theme(
            &[
                TranscriptEntry::ToolCall {
                    tool_name: "git".into(),
                    detail: String::new(),
                    status: TranscriptToolStatus::Denied,
                },
                TranscriptEntry::ToolCall {
                    tool_name: "live".into(),
                    detail: String::new(),
                    status: TranscriptToolStatus::Cancelled,
                },
                TranscriptEntry::ToolCall {
                    tool_name: "probe".into(),
                    detail: String::new(),
                    status: TranscriptToolStatus::Running,
                },
                TranscriptEntry::Completion {
                    multi_agent: false,
                    completed: false,
                    files: Vec::new(),
                    project_root: None,
                },
            ],
            true,
            &CliTheme::by_name("Midnight"),
        );
        assert_eq!(line_text(&lines[0]), "· git: denied ‖ denied");
        assert_eq!(line_text(&lines[1]), "· live: cancelled ‖ approval needed");
        assert_eq!(line_text(&lines[2]), "· probe: running ‖ approval needed");
        assert_eq!(line_text(&lines[3]), "Run single-agent (incomplete) — files: no files changed");
        // Denied / cancelled / running settle to the dim style.
        for line in &lines[..3] {
            assert_eq!(line.style.fg, Some(Color::DarkGray));
        }
    }

    #[test]
    fn provenance_rail_text_themed_and_plain() {
        use concerto_core::transcript::{TranscriptEntry, TranscriptToolStatus};
        use ratatui::style::{Color, Style};

        let tool = |status| TranscriptEntry::ToolCall {
            tool_name: "shell".into(),
            detail: String::new(),
            status,
        };
        let theme = CliTheme::by_name("Midnight");

        // Styled: rail text appended with themed colors (ok green,
        // approval yellow, denied red); base line style untouched.
        let lines = transcript_lines_with_theme(
            &[tool(TranscriptToolStatus::Completed), tool(TranscriptToolStatus::Denied)],
            true,
            &theme,
        );
        assert_eq!(line_text(&lines[0]), "· shell: completed ‖ policy ok");
        assert_eq!(line_text(&lines[1]), "· shell: denied ‖ denied");
        assert_eq!(lines[0].style.fg, Some(Color::Green));
        assert_eq!(lines[0].spans[1].style.fg, Some(Color::Green));
        assert_eq!(lines[1].spans[1].style.fg, Some(Color::Red));

        // Plain (NO_COLOR/off-TTY): rail symbols present, no ANSI styling.
        let plain =
            transcript_lines_with_theme(&[tool(TranscriptToolStatus::Running)], false, &theme);
        assert_eq!(line_text(&plain[0]), "· shell: running ‖ approval needed");
        assert!(plain[0].spans.iter().all(|span| span.style == Style::default()));
        assert_eq!(plain[0].style, Style::default());

        // Live event lines carry the plain rail too (unstyled by construction).
        assert_eq!(provenance_rail_text(&TranscriptToolStatus::Allowed), " ‖ policy ok");
        assert_eq!(provenance_rail_text(&TranscriptToolStatus::Cancelled), " ‖ approval needed");
    }

    // ------------------------------------------------------------------
    // Intent confirmation key handling (ADR-55 §1)
    // ------------------------------------------------------------------

    /// Wait until the spawned sink call has installed the pending intent so the
    /// keypress has a prompt to act on. Bounded so a broken wiring fails the
    /// test instead of hanging forever.
    async fn wait_for_intent(state: &CliApprovalState) {
        for _ in 0..500 {
            if state.intent_prompt().is_some() {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("intent confirmation never queued a prompt");
    }

    /// Install a pending intent prompt on `app` through a spawned sink call,
    /// returning the join handle the test asserts on after the keypress.
    fn spawn_intent(
        app: &mut App,
        options: &[RequestedOutcome],
    ) -> tokio::task::JoinHandle<Option<RequestedOutcome>> {
        let state = app.approval_state.clone();
        let sink = CliApprovalSink::with_state(state.clone());
        let options = options.to_vec();
        tokio::task::spawn(async move {
            sink.request_intent_confirmation(
                "proceed?".to_string(),
                &options,
                concerto_core::CancellationToken::new(),
            )
            .await
        })
    }

    #[tokio::test]
    async fn intent_prompt_digit_selects_listed_outcome() {
        let mut app = App::new();
        let options = vec![RequestedOutcome::Answer, RequestedOutcome::Diagnose];
        let handle = spawn_intent(&mut app, &options);
        wait_for_intent(&app.approval_state).await;

        // `[2]` picks the second option (index 1 → Diagnose).
        let action = app.handle_key(key_event(KeyCode::Char('2'), KeyModifiers::NONE));
        assert!(matches!(action, Action::None));
        assert_eq!(handle.await.expect("intent task panicked"), Some(RequestedOutcome::Diagnose));
        assert!(app.approval_state.intent_prompt().is_none());
    }

    #[tokio::test]
    async fn intent_prompt_enter_confirms_first_outcome() {
        let mut app = App::new();
        let options = vec![RequestedOutcome::Plan, RequestedOutcome::Execute];
        let handle = spawn_intent(&mut app, &options);
        wait_for_intent(&app.approval_state).await;

        let action = app.handle_key(key_event(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(action, Action::None));
        assert_eq!(handle.await.expect("intent task panicked"), Some(RequestedOutcome::Plan));
    }

    #[tokio::test]
    async fn intent_prompt_escape_rejects_with_none() {
        let mut app = App::new();
        let options = vec![RequestedOutcome::Execute];
        let handle = spawn_intent(&mut app, &options);
        wait_for_intent(&app.approval_state).await;

        let action = app.handle_key(key_event(KeyCode::Esc, KeyModifiers::NONE));
        assert!(matches!(action, Action::None));
        assert_eq!(handle.await.expect("intent task panicked"), None);
        assert!(app.approval_state.intent_prompt().is_none());
    }

    #[tokio::test]
    async fn intent_prompt_out_of_range_digit_is_ignored() {
        let mut app = App::new();
        let options = vec![RequestedOutcome::Answer];
        let handle = spawn_intent(&mut app, &options);
        wait_for_intent(&app.approval_state).await;

        // Digit 9 is beyond the single listed option — the prompt stays.
        let action = app.handle_key(key_event(KeyCode::Char('9'), KeyModifiers::NONE));
        assert!(matches!(action, Action::None));
        assert!(app.approval_state.intent_prompt().is_some());

        // An in-range digit then resolves it.
        let action = app.handle_key(key_event(KeyCode::Char('1'), KeyModifiers::NONE));
        assert!(matches!(action, Action::None));
        assert_eq!(handle.await.expect("intent task panicked"), Some(RequestedOutcome::Answer));
        assert!(app.approval_state.intent_prompt().is_none());
    }

    // ------------------------------------------------------------------
    // Plan approval key handling (ADR-55 Phase 1d)
    // ------------------------------------------------------------------

    /// Wait until the spawned sink call has installed the pending plan so the
    /// keypress has a prompt to act on. Bounded so broken wiring fails the test
    /// instead of hanging forever.
    async fn wait_for_plan(state: &CliApprovalState) {
        for _ in 0..500 {
            if state.plan_prompt().is_some() {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("plan approval never queued a prompt");
    }

    /// Install a pending plan prompt on `app` through a spawned sink call,
    /// returning the join handle the test asserts on after the keypress.
    fn spawn_plan(app: &mut App) -> tokio::task::JoinHandle<Option<PlanDecision>> {
        let state = app.approval_state.clone();
        let sink = CliApprovalSink::with_state(state.clone());
        let session_id = Ulid::new();
        let plan_id = "01JTESTPLAN0000000000001A".to_string();
        tokio::task::spawn(async move {
            sink.request_plan_approval(
                session_id,
                &plan_id,
                "Apply the stored plan?".to_string(),
                "step 1: rework module\nstep 2: verify",
                time::OffsetDateTime::now_utc(),
                concerto_core::CancellationToken::new(),
            )
            .await
        })
    }

    #[tokio::test]
    async fn plan_prompt_enter_applies() {
        let mut app = App::new();
        let handle = spawn_plan(&mut app);
        wait_for_plan(&app.approval_state).await;
        assert_eq!(app.plan_scroll, 0);

        // Enter confirms Apply (mutation-capable).
        let action = app.handle_key(key_event(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(action, Action::None));
        assert_eq!(handle.await.expect("plan task panicked"), Some(PlanDecision::Apply));
        assert!(app.approval_state.plan_prompt().is_none());
    }

    #[tokio::test]
    async fn plan_prompt_a_applies_too() {
        let mut app = App::new();
        let handle = spawn_plan(&mut app);
        wait_for_plan(&app.approval_state).await;

        let action = app.handle_key(key_event(KeyCode::Char('a'), KeyModifiers::NONE));
        assert!(matches!(action, Action::None));
        assert_eq!(handle.await.expect("plan task panicked"), Some(PlanDecision::Apply));
    }

    #[tokio::test]
    async fn plan_prompt_r_replans() {
        let mut app = App::new();
        let handle = spawn_plan(&mut app);
        wait_for_plan(&app.approval_state).await;

        // `r` discards the stored plan and replans (read-only).
        let action = app.handle_key(key_event(KeyCode::Char('r'), KeyModifiers::NONE));
        assert!(matches!(action, Action::None));
        assert_eq!(handle.await.expect("plan task panicked"), Some(PlanDecision::Replan));
        assert!(app.approval_state.plan_prompt().is_none());
    }

    #[tokio::test]
    async fn plan_prompt_escape_dismisses_with_none() {
        let mut app = App::new();
        let handle = spawn_plan(&mut app);
        wait_for_plan(&app.approval_state).await;

        // Esc dismisses (read-only `None`).
        let action = app.handle_key(key_event(KeyCode::Esc, KeyModifiers::NONE));
        assert!(matches!(action, Action::None));
        assert_eq!(handle.await.expect("plan task panicked"), None);
        assert!(app.approval_state.plan_prompt().is_none());
    }

    #[tokio::test]
    async fn plan_prompt_unrelated_key_is_ignored() {
        let mut app = App::new();
        let handle = spawn_plan(&mut app);
        wait_for_plan(&app.approval_state).await;

        // An unrelated key leaves the prompt pending.
        let action = app.handle_key(key_event(KeyCode::Char('x'), KeyModifiers::NONE));
        assert!(matches!(action, Action::None));
        assert!(app.approval_state.plan_prompt().is_some());
        assert!(!handle.is_finished());

        // A real key then resolves it.
        let action = app.handle_key(key_event(KeyCode::Char('r'), KeyModifiers::NONE));
        assert!(matches!(action, Action::None));
        assert_eq!(handle.await.expect("plan task panicked"), Some(PlanDecision::Replan));
    }

    #[tokio::test]
    async fn plan_prompt_scroll_keys_move_viewport_clamped() {
        let mut app = App::new();
        let handle = spawn_plan(&mut app);
        wait_for_plan(&app.approval_state).await;

        // j / Down scroll the plan body down; k / Up scroll back up, clamped
        // at the top.
        app.handle_key(key_event(KeyCode::Char('j'), KeyModifiers::NONE));
        assert_eq!(app.plan_scroll, 1);
        app.handle_key(key_event(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.plan_scroll, 2);
        app.handle_key(key_event(KeyCode::Char('k'), KeyModifiers::NONE));
        assert_eq!(app.plan_scroll, 1);
        app.handle_key(key_event(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(app.plan_scroll, 0);
        app.handle_key(key_event(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(app.plan_scroll, 0, "scroll must clamp at the top");

        // The prompt is unaffected by scrolling and still resolves.
        let action = app.handle_key(key_event(KeyCode::Esc, KeyModifiers::NONE));
        assert!(matches!(action, Action::None));
        assert_eq!(handle.await.expect("plan task panicked"), None);
    }

    #[tokio::test]
    async fn plan_prompt_resolves_prompt_of_spawned_session_only() {
        let mut app = App::new();
        let handle = spawn_plan(&mut app);
        wait_for_plan(&app.approval_state).await;
        let prompt = app.approval_state.plan_prompt().expect("plan prompt must be pending");

        // A resolve with the wrong session identity must not answer the prompt.
        assert!(
            !app.approval_state.resolve_plan(
                Ulid::new(),
                &prompt.plan_id,
                Some(PlanDecision::Apply)
            ),
            "a cross-session resolve must be rejected"
        );
        assert!(app.approval_state.plan_prompt().is_some(), "prompt must stay pending");
        assert!(!handle.is_finished(), "wrong-session resolve must not answer the task");

        // The canonical path (matching session + plan id) resolves it.
        assert!(app.approval_state.resolve_plan(
            prompt.session_id,
            &prompt.plan_id,
            Some(PlanDecision::Apply)
        ));
        assert_eq!(handle.await.expect("plan task panicked"), Some(PlanDecision::Apply));
    }

    // ------------------------------------------------------------------
    // Typewriter reveal of the assistant message (Issue #147 Part 2)
    // ------------------------------------------------------------------

    #[test]
    fn prefix_clamps_chars_shown() {
        assert_eq!(prefix("hello", 0), "");
        assert_eq!(prefix("hello", 3), "hel");
        // Shown past the end yields the whole string.
        assert_eq!(prefix("hello", 50), "hello");
        // Multi-byte UTF-8 never panics and always ends on a char boundary.
        let text = "héllo wörld — 你好";
        assert_eq!(prefix(text, 0), "");
        assert_eq!(prefix(text, text.chars().count()), text);
        let partial = prefix(text, 5);
        assert!(text.starts_with(&partial), "prefix must be a prefix of the source");
        assert!(partial.is_char_boundary(partial.len()), "prefix must end on a char boundary");
    }

    #[test]
    fn assistant_ingest_starts_reveal_with_empty_slot() {
        let mut app = App::new();
        let before = app.messages.len();
        app.ingest_ui_line(UiLine::Assistant("hello world".to_string()));
        // Wipe-analog rule + the empty reveal slot (assistant gutter only).
        assert_eq!(app.messages.len(), before + 2);
        assert_eq!(line_text(&app.messages[app.messages.len() - 2]), WIPE_RULE);
        assert_eq!(line_text(app.messages.last().unwrap()), "♪ ");
        let reveal = app.reveal.as_ref().unwrap();
        assert_eq!(reveal.full, "hello world");
        assert_eq!(reveal.shown, 0);
    }

    #[test]
    fn assistant_reveal_advances_eight_chars_per_tick_until_complete() {
        // 32 chars → exactly 4 ticks of 8.
        let full = "x".repeat(32);
        let mut ticks = 0;
        let mut app = App::new();
        app.ingest_ui_line(UiLine::Assistant(full.clone()));
        while app.reveal.is_some() {
            app.advance_reveal();
            ticks += 1;
            let shown = app.reveal.as_ref().map(|r| r.shown).unwrap_or(32);
            assert_eq!(shown, ticks * REVEAL_CHARS_PER_TICK, "each tick adds exactly 8 chars");
            assert_eq!(
                line_text(app.messages.last().unwrap()),
                format!("♪ {}", prefix(&full, shown))
            );
        }
        assert_eq!(ticks, 4);
        assert_eq!(line_text(app.messages.last().unwrap()), format!("♪ {full}"));
        assert!(app.reveal.is_none(), "reveal is cleared once the full text is shown");
    }

    #[test]
    fn assistant_reveal_clamps_short_message_to_full_in_one_tick() {
        let mut app = App::new();
        app.ingest_ui_line(UiLine::Assistant("short".to_string()));
        assert!(app.reveal.is_some());
        // 5 chars on an 8-char tick reveals everything at once.
        app.advance_reveal();
        assert_eq!(line_text(app.messages.last().unwrap()), "♪ short");
        assert!(app.reveal.is_none(), "reveal completes as soon as the full text is shown");
    }

    #[test]
    fn assistant_reveal_handles_multibyte_utf8() {
        let full = "héllo wörld — 你好!";
        let mut app = App::new();
        app.ingest_ui_line(UiLine::Assistant(full.to_string()));
        while app.reveal.is_some() {
            app.advance_reveal();
            let shown = app.reveal.as_ref().map(|r| r.shown).unwrap_or(full.chars().count());
            let expected = format!("♪ {}", prefix(full, shown));
            assert_eq!(line_text(app.messages.last().unwrap()), expected);
            assert!(prefix(full, shown).is_char_boundary(prefix(full, shown).len()));
        }
        assert_eq!(line_text(app.messages.last().unwrap()), format!("♪ {full}"));
    }

    #[test]
    fn plain_text_ingest_cancels_active_reveal() {
        let mut app = App::new();
        app.ingest_ui_line(UiLine::Assistant("long enough to still be revealing".to_string()));
        // One tick to get mid-reveal.
        app.advance_reveal();
        assert!(app.reveal.is_some());

        let before = app.messages.len();
        app.ingest_ui_line(UiLine::Text("· tool finished".to_string()));
        assert_eq!(app.messages.len(), before + 1);
        assert!(app.reveal.is_none(), "a plain line must cancel any active reveal");
        // The abandoned reveal slot materializes its full guttered text
        // before the new line is appended.
        assert_eq!(
            line_text(&app.messages[app.messages.len() - 2]),
            "♪ long enough to still be revealing"
        );
        assert_eq!(line_text(app.messages.last().unwrap()), "· tool finished");
    }

    #[test]
    fn push_line_cancels_active_reveal_like_a_text_ingest() {
        let mut app = App::new();
        app.ingest_ui_line(UiLine::Assistant("abcdefgh".to_string()));
        assert!(app.reveal.is_some());
        app.push_line(Line::from("> new question"));
        assert!(app.reveal.is_none(), "any direct push must cancel the reveal");
        // The reveal slot renders its full guttered text (it was cancelled, not lost).
        assert_eq!(line_text(&app.messages[app.messages.len() - 2]), "♪ abcdefgh");
        assert_eq!(line_text(app.messages.last().unwrap()), "> new question");
    }

    #[test]
    fn second_assistant_message_restarts_reveal_from_zero() {
        let mut app = App::new();
        app.ingest_ui_line(UiLine::Assistant("first message".to_string()));
        app.advance_reveal();
        assert!(app.reveal.is_some());

        app.ingest_ui_line(UiLine::Assistant("second".to_string()));
        assert_eq!(line_text(app.messages.last().unwrap()), "♪ ");
        let reveal = app.reveal.as_ref().unwrap();
        assert_eq!(reveal.full, "second");
        assert_eq!(reveal.shown, 0);
    }

    // ------------------------------------------------------------------
    // Signature text motion parity (Score first-token + Baton handoff +
    // wipe analog). `styling` is passed explicitly so the cues are
    // assertable off-TTY, where `styling_enabled()` is false.
    // ------------------------------------------------------------------

    #[test]
    fn reveal_line_bolds_first_token_only_while_emphasis_holds() {
        use ratatui::style::Modifier;
        let line = reveal_line("hello world here", 16, true, false, true);
        // Gutter + bold first token + plain remainder.
        assert_eq!(line.spans.len(), 3);
        assert_eq!(line.spans[0].content.as_ref(), "♪ ");
        assert_eq!(line.spans[1].content.as_ref(), "hello");
        assert!(line.spans[1].style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(line.spans[2].content.as_ref(), " world here");
        assert!(!line.spans[2].style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn reveal_line_bolds_whole_prefix_before_first_space() {
        use ratatui::style::Modifier;
        let line = reveal_line("hello", 5, true, false, true);
        assert_eq!(line.spans.len(), 2);
        assert!(line.spans[1].style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn reveal_line_dims_while_holding_and_settles_plain() {
        use ratatui::style::Modifier;
        let held = reveal_line("hello world", 11, false, true, true);
        assert!(held.spans.iter().all(|span| span.style.add_modifier.contains(Modifier::DIM)));
        let settled = reveal_line("hello world", 11, false, false, true);
        // Body settles plain (no emphasis BOLD); span 0 is the role gutter,
        // which keeps its `chat_line` role color + BOLD by contract.
        assert!(settled
            .spans
            .iter()
            .skip(1)
            .all(|span| !span.style.add_modifier.contains(Modifier::BOLD)));
        // NO_COLOR/off-TTY: gutter symbols only, no modifiers.
        let plain = reveal_line("hello world", 11, true, false, false);
        assert_eq!(line_text(&plain), "♪ hello world");
        assert_eq!(plain.spans.len(), 2);
    }

    #[test]
    fn handoff_hold_freezes_reveal_three_ticks_then_resumes() {
        let mut app = App::new();
        app.ingest_ui_line(UiLine::Assistant(
            "01234567\n\nrest of second paragraph here".to_string(),
        ));
        // First tick reveals 8 chars; the boundary end (char 10) is still
        // out of reach, so no cue yet.
        app.advance_reveal();
        assert_eq!(app.reveal.as_ref().unwrap().shown, 8);
        assert_eq!(app.reveal.as_ref().unwrap().hold, 0);

        // Second tick (8→16) crosses the blank-line boundary: the frontier
        // lands and a 3-tick hold opens.
        app.advance_reveal();
        assert_eq!(app.reveal.as_ref().unwrap().shown, 16);
        assert_eq!(app.reveal.as_ref().unwrap().hold, HANDOFF_HOLD_TICKS);

        // The frontier stays frozen while the hold counts down, one per tick.
        app.advance_reveal();
        assert_eq!(app.reveal.as_ref().unwrap().shown, 16);
        assert_eq!(app.reveal.as_ref().unwrap().hold, 2);
        app.advance_reveal();
        assert_eq!(app.reveal.as_ref().unwrap().shown, 16);
        assert_eq!(app.reveal.as_ref().unwrap().hold, 1);
        app.advance_reveal();
        assert_eq!(app.reveal.as_ref().unwrap().shown, 16);
        assert_eq!(app.reveal.as_ref().unwrap().hold, 0);

        // Next tick resumes the 8ch@16ms cadence exactly where it paused.
        app.advance_reveal();
        assert_eq!(app.reveal.as_ref().unwrap().shown, 24);
        assert_eq!(app.reveal.as_ref().unwrap().hold, 0);
    }

    #[test]
    fn single_paragraph_reveal_never_holds() {
        // 32 chars → 3 ticks of 8 stay mid-reveal, so `hold` is readable.
        let mut app = App::new();
        app.ingest_ui_line(UiLine::Assistant("0123456789abcdefghij0123456789AB".to_string()));
        for _ in 0..3 {
            app.advance_reveal();
            assert_eq!(app.reveal.as_ref().unwrap().hold, 0);
        }
        assert_eq!(app.reveal.as_ref().unwrap().shown, 24);
    }

    #[test]
    fn paragraph_boundary_detection_covers_separator_shapes() {
        assert!(!crosses_paragraph_boundary("just one paragraph", 0, 18));
        let two = "01234567\n\nrest";
        assert!(crosses_paragraph_boundary(two, 8, 16));
        assert!(!crosses_paragraph_boundary(two, 0, 8));
        assert!(!crosses_paragraph_boundary(two, 10, 16));
        assert!(crosses_paragraph_boundary("01234567\n   \nrest", 8, 16));
        assert!(crosses_paragraph_boundary("01234567\r\n\r\nrest", 8, 16));
        assert!(!crosses_paragraph_boundary("0123456789\n\n", 0, 12));
        assert!(!crosses_paragraph_boundary("0123456789\n", 0, 11));
        assert!(!crosses_paragraph_boundary("", 0, 0));
        assert!(!crosses_paragraph_boundary("a\n\nb", 4, 4));
    }

    #[test]
    fn reduced_motion_renders_assistant_instant_with_no_hold_or_rule() {
        let mut app = App::new();
        app.set_reduced_motion(true);
        let before = app.messages.len();
        let full = "01234567\n\nrest of second paragraph here".to_string();
        app.ingest_ui_line(UiLine::Assistant(full.clone()));
        // No wipe-analog rule and no reveal state: the full guttered line
        // lands at once.
        assert_eq!(app.messages.len(), before + 1);
        assert!(app.reveal.is_none(), "reduced-motion never opens a reveal");
        assert_eq!(line_text(app.messages.last().unwrap()), format!("♪ {full}"));
    }
}
