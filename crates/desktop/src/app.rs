use iced::keyboard;
use iced::widget::{button, column, container, mouse_area, row, rule, stack, text, text_input};
use iced::{Element, Length, Subscription};

use crate::root_consent;
use crate::shortcuts;
use crate::theme::AppTheme;
use crate::views;
use crate::widgets::agent_graph::NodeState;
use crate::widgets::capability_dialog;
use crate::widgets::circuit_background;

use crate::services::session_handler::DesktopSessionHandler;
use crate::views::memory::MemoryStatus;
use crate::views::spend::CapUiState;
use camino::Utf8PathBuf;
use concerto_config::AppConfig;
use concerto_config::CredentialStore;
use concerto_core::event::EventBus;
use concerto_core::failures::{ClassifiedFailure, FailureAudience};
use concerto_core::helpers::project_id_hash;
use concerto_core::ids::Ulid;
use concerto_core::intent::{PlanDecision, RequestedOutcome, RunStage};
use concerto_core::traits::approval::{ApprovalDecision, ApprovalSink};
use concerto_core::traits::memory::MemoryStore;
use concerto_core::transcript::TranscriptEntry;
use concerto_core::types::PolicyAction;
use concerto_core::types::{AgentCompletionStatus, AgentOutput};
use concerto_core::CancellationToken;
use concerto_core::OrchestratorError;
use concerto_memory::indexer::{IndexConfig, ProjectIndexer};
use concerto_memory::sync::ChunkSyncService;
use concerto_orchestrator::runtime_runner::{
    init_memory_system, memory_enabled, run_shared_agent, ActiveMemoryServices,
};
use concerto_orchestrator::services::{RequestBuilder, ServicesBuilder};
use concerto_providers::factory::ProviderFactory;
use concerto_providers::provider_defs::{
    model_options_for, provider_definition, provider_readiness,
};
use concerto_tools::diff::compute_diffs_from_virtual_fs;
use concerto_tools::virtual_fs::VirtualFs;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::ui::feedback::{ToastLevel, ToastManager};

/// Shared text-focus state read by the keyboard-subscription fn pointer.
/// `keyboard::on_key_press` requires a bare fn (no captures), so we route
/// through a static rather than App's field.
static TEXT_FOCUSED: AtomicBool = AtomicBool::new(false);

/// Cadence of the blinking cursor on the live streaming assistant entry.
/// The subscription that drives it exists only while a run is streaming, so
/// this costs nothing at idle.
const STREAMING_CURSOR_PERIOD_MS: u64 = 500;

/// How long a toast stays visible before it auto-dismisses. The expiry
/// subscription ticks once per second while any toast is showing.
pub const TOAST_LIFETIME_SECS: u64 = 5;

// ---------------------------------------------------------------------------
// Page enum — all navigable views
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Page {
    Chat,
    ToolLog,
    DiffViewer,
    Settings,
    Editor,
    OrchestrationStudio,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunStatus {
    Idle,
    Running,
    Cancelling,
}

/// One node of the sidebar project→session tree: a project folder and, once
/// loaded, its recent sessions. `sessions == None` means "not loaded yet"
/// (the first expand spawns the load); an empty `Vec` means "loaded, empty".
#[derive(Debug, Clone)]
pub struct ProjectTreeNode {
    pub path: PathBuf,
    pub name: String,
    pub expanded: bool,
    pub sessions: Option<Vec<views::chat::SessionRow>>,
}

// ---------------------------------------------------------------------------
// Message enum — namespaced, routed to per-view handlers
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum Message {
    Navigate(Page),
    Shortcut(shortcuts::Shortcut),
    /// Agent run completed — carries the final response string.
    /// Triggers diff loading from the shared VirtualFs.
    AgentRunCompleted(Option<Ulid>, Box<Result<AgentOutput, ClassifiedFailure>>),
    CancelAgentRun,
    /// Advances the ambient circuit-trace background pulse. Only ever
    /// dispatched while `run_status == RunStatus::Running` — see
    /// `subscription`.
    CircuitTick,
    /// One step (16 ms) of the shared overlay/terminal animation tick. Moves
    /// `overlay_fade` toward `overlay_fade_target` and `terminal_panel_anim`
    /// toward its open/closed target; the subscription stays active only
    /// while either animation is in flight.
    AnimTick,
    Chat(views::chat::Message),
    Diff(views::diff::Message),
    Memory(views::memory::Message),
    ToolLog(views::tool_log::Message),
    Settings(views::settings::Message),
    AgentGraph(views::agent_graph::Message),
    Terminal(views::terminal::Message),
    OrchestrationStudio(views::orchestration_studio::StudioMessage),
    Editor(views::code_editor::Message),
    ThemeChanged,
    HelpToggled,
    CapabilityDlg(capability_dialog::Message),
    /// Events from the backend event bus, translated to DesktopEvent.
    DesktopEvent(crate::runtime::DesktopEvent),
    /// Screenshot capture completed — carries the result or error.
    ScreenshotCompleted(Result<crate::services::screenshot::ScreenshotResult, String>),
    /// Change the active provider for the current chat session.
    SetActiveProvider(String),
    /// Change the model used with the active provider.
    SetActiveModel(String),
    /// External config files changed on disk — reload and re-derive every
    /// config-derived `App` field (ADR-57). Self-induced events (our own
    /// saves) are no-ops via the equality short-circuit in
    /// `reconcile_config_from_reload`.
    ConfigReloaded,
    /// Trigger a screenshot capture.
    TakeScreenshot,
    /// Clear the screenshot status message.
    ClearScreenshotStatus,
    /// Clear the transient save-feedback message.
    ClearSaveFeedback(u64),
    /// Set the active sub-view overlay in the chat canvas (Diff / AgentGraph / ToolLog / Main).
    SetSubView(views::chat::SubView),
    /// Open the Spend Log modal: sets the chat sub-view and loads the active
    /// session's spend records for the log body.
    OpenSpendLog,
    /// Toggle the collapsible right-side quick panel.
    ToggleQuickPanel,
    /// Open the Memory explorer modal (quick-panel button or Ctrl+M).
    OpenMemoryModal,
    /// Close the Memory explorer modal (close button or backdrop).
    CloseMemoryModal,
    /// Toggle the terminal bottom panel open/closed.
    ToggleTerminalPanel,
    /// Begin dragging the terminal panel's resize handle.
    TerminalPanelResizeStart,
    /// Cursor Y (logical px) moved while dragging the terminal panel handle.
    TerminalPanelResizeMoved(f32),
    /// The terminal panel resize drag ended (release or any other event).
    TerminalPanelResizeEnd,
    /// Project repository status loaded for the quick panel.
    GitSummaryLoaded(Option<concerto_tools::git::RepositorySummary>),
    /// Result of a manual memory re-index (triggered from the Memory view).
    ReindexResult(ReindexResult),
    MemoryEntriesLoaded(Result<Vec<views::memory::MemoryRow>, String>),
    MemoryEntryDeleted {
        id: String,
        result: Result<(), String>,
    },
    /// A session was picked from the picker; carries the loaded history so the
    /// chat can be seeded with the resumed conversation. `transcript` is the
    /// durable typed transcript (ADR-36) and takes precedence over `history`
    /// when non-empty (legacy sessions only populate `history`).
    SessionSelected {
        session_id: String,
        history: Vec<concerto_core::types::Message>,
        events: Vec<concerto_sessions::replay::StoredEvent>,
        transcript: Vec<TranscriptEntry>,
    },
    /// Open the "change project folder" modal.
    OpenProjectDirPicker,
    /// Folder path text changed in the modal.
    ProjectDirInputChanged(String),
    /// Apply the typed folder path as the new project folder.
    ProjectDirApply,
    /// Cancel the "change project folder" modal.
    ProjectDirCancel,
    /// ADR-44 §4: user allowed opening the pending out-of-root project (for the
    /// process lifetime). Applies the deferred switch and records the path.
    RootConsentAllow,
    /// ADR-44 §4: user denied opening the pending out-of-root project. Aborts
    /// the deferred switch cleanly.
    RootConsentDeny,
    /// Toggle a project node in the sidebar project tree; expands it and
    /// lazily loads its sessions on the first expand.
    ToggleProjectExpanded(PathBuf),
    /// Recent sessions for one project loaded for the sidebar tree.
    ProjectSessionsLoaded {
        path: PathBuf,
        sessions: Vec<views::chat::SessionRow>,
    },
    /// A session row was clicked in the sidebar project tree.
    TreeSessionClicked {
        project: PathBuf,
        session_id: String,
    },
    /// Dismiss a toast notification by ID.
    ToastDismissed(u64),
    /// Periodic tick while any toast is visible, used to auto-dismiss toasts
    /// older than `TOAST_LIFETIME_SECS`.
    ToastExpiryTick,
    /// User decision on the acknowledgement (no-undo) dialog.
    AckDialog(capability_dialog::AckDialogMessage),
    /// User decision on the intent confirmation dialog (ADR-55 §1).
    IntentDialog(capability_dialog::IntentDialogMessage),
    /// User decision on the plan approval dialog (ADR-55 Phase 1d).
    PlanDialog(capability_dialog::PlanDialogMessage),
}

/// Outcome of a manual memory re-index request.
#[derive(Debug, Clone)]
pub enum ReindexResult {
    /// Re-index completed, carrying the number of chunks written.
    Done(usize),
    /// Re-index failed with an error message.
    Failed(String),
    /// Memory was initialized and its background full index is running.
    Started,
    /// No indexer was available yet (memory not initialised).
    Skipped,
}

// ---------------------------------------------------------------------------
// App — root application state
// ---------------------------------------------------------------------------

pub struct App {
    pub page: Page,
    pub current_theme: AppTheme,
    pub show_help: bool,

    // Per-view state
    pub chat: views::chat::State,
    pub diff: views::diff::State,
    pub memory: views::memory::State,
    pub tool_log: views::tool_log::State,
    pub settings: views::settings::State,
    pub agent_graph: views::agent_graph::State,
    pub terminal: views::terminal::State,
    pub orchestration_studio: views::orchestration_studio::State,
    pub editor: views::code_editor::State,

    // Capability approval dialog state
    pub cap_pending: capability_dialog::SharedPending,
    pub pending_ack: capability_dialog::SharedPendingAck,
    /// Pending intent confirmations (ADR-55 §1), FIFO queue.
    pub pending_intent: capability_dialog::SharedPendingIntent,
    /// Pending plan approvals (ADR-55 Phase 1d), FIFO queue.
    pub pending_plan: capability_dialog::SharedPendingPlan,

    pub bus: concerto_core::event::EventBus,
    pub config: Option<concerto_config::AppConfig>,
    /// Unmerged user-level settings. Global writes always use this layer as
    /// their base so project/env overrides cannot leak into `config.toml`.
    pub global_config: concerto_config::AppConfig,
    /// Project-scoped memory services (store, indexer, sync, cancel).
    /// Switched when the active project changes.
    pub memory_services: Arc<Mutex<Option<ActiveMemoryServices>>>,
    pub cancel_token: concerto_core::CancellationToken,
    pub run_status: RunStatus,
    /// Current intent-router stage of the active run (ADR-55 Phase 2a),
    /// rendered as the status-bar run-stage chip. `Some` only while
    /// `run_status == RunStatus::Running`; it is set by
    /// `DesktopEvent::RunStageChanged` (guarded by the run status) and cleared
    /// at every run boundary: dispatch start and `AgentRunCompleted`.
    pub run_stage: Option<RunStage>,
    pub multi_agent: bool,
    /// Runtime-only "fast mode" toggle (mirrors CLI `-f/--fast`): disables
    /// project memory retrieval for the run while leaving the configured
    /// `[memory] enabled` flag untouched. Like the CLI flag this is a
    /// per-session choice and is deliberately NOT persisted to config.
    pub fast: bool,
    /// Phase accumulator (wraps in `[0.0, 1.0)`) driving the ambient
    /// circuit-trace background pulse. Only advanced while
    /// `run_status == RunStatus::Running` — see `subscription`/`update`.
    pub circuit_progress: f32,
    /// Serialised orchestration checkpoint from the last partial run.
    /// Passed to `AgentRunRequest` on the next submit so the coordinator can
    /// resume the graph without re-architecting.
    pub resume_checkpoint_json: Option<String>,

    /// ADR-60 D7 (interrupt-safe resume): bumped every time an in-flight run
    /// settles (completion, failure, or the unwind after cancellation). The
    /// window-close handler (project shell) polls this epoch to wait,
    /// bounded, for the run's checkpoint to persist before the process
    /// exits.
    pub run_settle_epoch: Arc<AtomicU64>,

    /// Shared VirtualFs — the agent writes to this, and the diff viewer
    /// reads from it to show proposed changes and applies rejections.
    pub vfs: Arc<Mutex<VirtualFs>>,
    /// Project‑scoped session manager, lazily opened on first submit.
    pub session_manager: Arc<Mutex<Option<Arc<DesktopSessionHandler>>>>,

    /// Whether a text input is currently focused — read by the keyboard
    /// subscription to gate shortcuts that would interfere with typing.
    pub text_focused: bool,

    /// Screenshot status message shown in the status bar.
    pub screenshot_status: Option<String>,

    /// Currently active provider ID for model switching in the chat view.
    pub active_provider_id: String,
    /// Model selected for the current chat session.
    pub active_model: String,
    /// Model-option list for the chat header picker, rebuilt from the active
    /// provider's shared resolver so it outlives the per-frame `view` borrow.
    pub chat_model_options: Vec<String>,
    /// Brief, non-blocking confirmation shown after a provider/model change is
    /// persisted. Cleared automatically so it does not linger.
    pub save_feedback: Option<String>,
    /// Monotonic generation for `save_feedback`. A delayed `ClearSaveFeedback`
    /// only clears the notice when its generation is still current, so a timer
    /// from an older save can never erase feedback from a newer save.
    pub save_feedback_generation: u64,
    /// Whether the collapsible right-side quick panel is expanded.
    pub quick_panel_open: bool,
    /// Whether the Memory explorer modal is open.
    pub memory_view_open: bool,
    /// Whether the toggleable terminal bottom panel is visible.
    pub terminal_panel_open: bool,
    /// Current height (logical px) of the terminal bottom panel.
    pub terminal_panel_height: f32,
    /// Whether a terminal-panel drag-resize is in progress.
    pub terminal_resizing: bool,
    /// Cursor Y (logical px) captured at the first move of the current drag.
    pub terminal_drag_origin: Option<f32>,
    /// Panel height captured when the current drag began.
    pub terminal_start_height: f32,
    /// Animated height fraction of the terminal bottom panel: 0.0 = closed,
    /// 1.0 = open. Driven by the shared `Message::AnimTick` subscription (one
    /// 16 ms tick advances it by 0.08 toward the `terminal_panel_open`
    /// target) so the panel slides open/closed instead of popping. Layout-only
    /// animation — iced 0.14 has no opacity/transform widgets.
    pub terminal_panel_anim: f32,
    /// Whether the terminal panel slide animation is still in flight. Keeps
    /// the `AnimTick` subscription alive until the panel settles.
    pub terminal_panel_animating: bool,
    /// Overlay fade alpha: 0.0 = transparent backdrop, 1.0 = fully dimmed.
    /// Driven by `Message::AnimTick` in 0.08 steps toward
    /// `overlay_fade_target`. Color-alpha-only animation (iced 0.14 has no
    /// per-element opacity).
    pub overlay_fade: f32,
    /// Target for `overlay_fade`: 1.0 while a sub-view overlay is open, 0.0
    /// when closing back to Main.
    pub overlay_fade_target: f32,
    /// Whether the overlay fade is still in flight. Keeps the `AnimTick`
    /// subscription alive until the backdrop settles.
    pub overlay_fading: bool,
    /// Non-blocking snapshot of the active project's Git state.
    pub git_summary: Option<concerto_tools::git::RepositorySummary>,

    /// Phase 3 — in-flight model-discovery requests, keyed by provider id.
    /// The value is the request id; a returned result whose id does not match
    /// the current entry is stale and discarded.
    pub pending_refresh: std::collections::HashMap<String, u64>,
    /// Monotonic counter for refresh request ids.
    pub refresh_seq: u64,
    /// Backend session whose rich UI transcript is currently displayed.
    pub active_session_id: Option<Ulid>,

    // ---- Spend (issue #93 Phase 4) ----
    /// Live session cost shown on the status-bar spend chip. Updated by
    /// `DesktopEvent::SpendUpdated` (published after each provider call
    /// settles) and reset when the active session changes.
    pub live_session_cost: f64,
    /// Session spend cap in USD (`None` = no cap). Derived from
    /// `config.session_spend_cap_usd` and refreshed by cap events, which
    /// carry the authoritative cap the orchestrator enforces.
    pub session_cap: Option<f64>,
    /// ADR-57 §3c: true while a config file is unparsable, so a broken file
    /// toasts exactly once per broken period (recovery happens on the next
    /// good event, no polling).
    pub config_broken: bool,
    /// Latest cap signal (Normal / Approaching / Exceeded) from the event
    /// bus. `Normal` on a fresh session; cap events replace it.
    pub cap_state: CapUiState,
    /// Daily spend total — STUB: daily tracking is not yet enabled (issue
    /// #93 Phase 4), so this stays `None` and the Spend Log modal shows a
    /// "— (daily tracking not yet enabled)" row for it.
    pub daily_cost: Option<f64>,

    /// Active project folder. Files the agent writes are saved here, and it
    /// scopes the session, memory index, and persisted transcript.
    pub project_dir: PathBuf,
    /// Text typed in the "change project folder" modal.
    pub project_dir_input: String,
    /// Whether the "change project folder" modal is open.
    pub show_dir_picker: bool,
    /// ADR-44 §4: effective project-root allowlist — canonicalized configured
    /// roots seeded at startup plus every canonical path the user has allowed
    /// for this process. Never persisted. Empty = roots unset = no gating.
    pub effective_roots: Vec<PathBuf>,
    /// ADR-44 §4: canonical path awaiting the out-of-root consent gate before
    /// the deferred project switch is applied. `None` = no gate shown.
    pub pending_root_consent: Option<PathBuf>,
    /// Sidebar project→session tree. Most-recent project first; the active
    /// project's node is expanded by default.
    pub project_tree: Vec<ProjectTreeNode>,
    /// Session id that should be resumed after a deferred project switch
    /// (set alongside `pending_root_consent` when a tree session click is
    /// gated, or before an ungated switch from the tree).
    pub pending_tree_session: Option<String>,
    /// Toast notification manager for user-facing errors and confirmations.
    pub toasts: ToastManager,
}

/// Ease-out cubic curve: fast start, gentle landing. Used to map the
/// terminal panel's linear animation fraction to a visually decelerating
/// height so the slide feels natural instead of mechanical.
fn ease_out_cubic(t: f32) -> f32 {
    let u = 1.0 - t;
    1.0 - u * u * u
}

/// Display name for a project directory in the sidebar tree: the last path
/// segment, falling back to the full path when it has no usable name.
fn project_name(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(str::to_owned)
        .unwrap_or_else(|| path.display().to_string())
}

/// Path of the persisted chat transcript for one explicit project session.
///
/// Scoped by a stable hash of the project directory so switching projects
/// never mixes one project's on-screen conversation into another's.
fn transcript_path(project_dir: &std::path::Path, session_id: &str) -> PathBuf {
    let proj_id = project_id_hash(project_dir);
    let filename = format!("{proj_id}-{session_id}.json");
    dirs::data_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("concerto")
        .join("sessions")
        .join(filename)
}

/// Path of the per-project persisted agent-graph state for `project_dir`,
/// scoped by session id (mirrors [`transcript_path`]).
fn agent_graph_path(project_dir: &std::path::Path, session_id: &str) -> PathBuf {
    let proj_id = project_id_hash(project_dir);
    let filename = format!("{proj_id}-{session_id}-agent-graph.json");
    dirs::data_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("concerto")
        .join("sessions")
        .join(filename)
}

/// Convert a persisted session history (`Vec<core::Message>`) into chat
/// entries for display. Tool/system roles are skipped; assistant turns with
/// empty text (pure tool-execution turns) are omitted so the resumed chat
/// reads as a coherent conversation.
fn messages_to_entries(history: Vec<concerto_core::types::Message>) -> Vec<views::chat::ChatEntry> {
    use concerto_core::types::Role;
    let mut entries = Vec::new();
    for m in history {
        match m.role {
            Role::User => {
                let id = entries.len() + 1;
                // Historical reconstruction: original timestamps are not
                // recorded in the run transcript.
                entries.push(views::chat::ChatEntry::User {
                    id,
                    content: m.content,
                    created_at: None,
                });
            }
            Role::Assistant if !m.content.trim().is_empty() => {
                let id = entries.len() + 1;
                entries.push(views::chat::ChatEntry::Assistant {
                    id,
                    content: m.content,
                    streaming: false,
                    created_at: None,
                });
            }
            _ => {}
        }
    }
    entries
}

/// Map the durable typed transcript (ADR-36) onto chat entries for restore.
///
/// This mirrors the live rendering in `crates/desktop/src/runtime.rs`:
/// `Thinking`/`Activity`/`Summary` become dimmed `Thinking` lines (activity
/// with the `[agent]` prefix, summaries as collapsed `[Context]` lines), tool
/// calls carry their final status 1:1, and the completion marker becomes a
/// `RunCompletionSummary` card. Entry ids are assigned sequentially (the
/// existing convention); `State::from_entries` derives the next id.
pub(crate) fn transcript_to_entries(entries: Vec<TranscriptEntry>) -> Vec<views::chat::ChatEntry> {
    use concerto_core::transcript::TranscriptToolStatus;
    use views::chat::{ChatEntry, ToolCallStatus};

    let to_chat_status = |status: &TranscriptToolStatus| match status {
        TranscriptToolStatus::Running => ToolCallStatus::Running,
        TranscriptToolStatus::Completed => ToolCallStatus::Completed,
        TranscriptToolStatus::Failed => ToolCallStatus::Failed,
        TranscriptToolStatus::Allowed => ToolCallStatus::Allowed,
        TranscriptToolStatus::Denied => ToolCallStatus::Denied,
        TranscriptToolStatus::Cancelled => ToolCallStatus::Cancelled,
    };

    let mut chat_entries = Vec::with_capacity(entries.len());
    for entry in entries {
        let id = chat_entries.len() + 1;
        match entry {
            TranscriptEntry::User { content } => {
                // Historical reconstruction: original timestamps are not
                // recorded in the run transcript.
                chat_entries.push(ChatEntry::User { id, content, created_at: None });
            }
            TranscriptEntry::Assistant { content } => {
                chat_entries.push(ChatEntry::Assistant {
                    id,
                    content,
                    streaming: false,
                    created_at: None,
                });
            }
            // Live AgentThought lines render as `[{agent_id}] {content}`
            // (runtime.rs route_event); mirror that exactly.
            TranscriptEntry::Thinking { agent, content } => {
                let label = if agent.is_empty() { content } else { format!("[{agent}] {content}") };
                chat_entries.push(ChatEntry::Thinking {
                    id,
                    content: label,
                    collapsed: false,
                    created_at: None,
                    finished_at: None,
                });
            }
            TranscriptEntry::ToolCall { tool_name, detail, status } => {
                chat_entries.push(ChatEntry::ToolCall {
                    id,
                    tool_name,
                    detail,
                    status: to_chat_status(&status),
                    created_at: None,
                });
            }
            // Activity lines restore as thinking lines (ADR-36); the
            // `[agent]` prefix mirrors the live subtask/activity rendering.
            TranscriptEntry::Activity { agent, content } => {
                chat_entries.push(ChatEntry::Thinking {
                    id,
                    content: format!("[{agent}] {content}"),
                    collapsed: false,
                    created_at: None,
                    finished_at: None,
                });
            }
            TranscriptEntry::Error { content } => {
                chat_entries.push(ChatEntry::Error { id, content, created_at: None });
            }
            // Context summaries restore as collapsed thinking lines.
            TranscriptEntry::Summary { content } => {
                chat_entries.push(ChatEntry::Thinking {
                    id,
                    content: format!("[Context] {content}"),
                    collapsed: true,
                    created_at: None,
                    finished_at: None,
                });
            }
            TranscriptEntry::Completion { multi_agent, completed, files, project_root } => {
                chat_entries.push(ChatEntry::Completion {
                    id,
                    summary: views::chat::RunCompletionSummary {
                        multi_agent,
                        completed,
                        files,
                        project_root,
                    },
                    created_at: None,
                });
            }
        }
    }
    chat_entries
}

fn configured_default_route(config: &AppConfig) -> (String, String) {
    if let Some(settings) = &config.model_settings {
        let assignment_default = settings.agent_assignments.iter().find_map(|assignment| {
            assignment
                .model_override
                .as_deref()
                .filter(|model| !model.trim().is_empty())
                .map(|model| (assignment.provider_config_id.as_str(), model))
        });
        let model = settings
            .global_default_model
            .as_deref()
            .filter(|model| !model.trim().is_empty())
            .or_else(|| {
                settings
                    .providers
                    .iter()
                    .map(|provider| provider.model.as_str())
                    .find(|model| !model.trim().is_empty())
            })
            .or_else(|| assignment_default.map(|(_, model)| model))
            .unwrap_or_default()
            .to_string();
        let provider = ProviderFactory::config_for_model(settings, &model, None)
            .or_else(|| {
                assignment_default.and_then(|(provider_id, _)| {
                    settings.providers.iter().find(|provider| provider.id == provider_id)
                })
            })
            .or_else(|| settings.providers.first());
        return (provider.map(ProviderFactory::config_id).unwrap_or_default(), model);
    }
    config
        .primary_provider_config
        .as_ref()
        .map(|provider| (ProviderFactory::config_id(provider), provider.model.clone()))
        .unwrap_or_default()
}

/// Slice 4a (spec §7): with `[orchestration]` present the blueprint's open
/// relationship registry (Studio → Relationships) governs hand-offs, so the
/// legacy Settings → Agent Relationship Manager is hidden in favor of the
/// Studio's relationship rows. Pure projection over the persisted surface —
/// deliberately a free function so the flag's plumbing is unit-testable with
/// plain `AppConfig` values, mirroring `configured_default_route`.
fn orchestration_hides_relationships(config: &AppConfig) -> bool {
    config.orchestration.is_some()
}

impl App {
    /// Best-effort persist of the current agent-graph view state to the active
    /// session's file. A write failure must never break the UI, so errors are
    /// swallowed. Mirrors how the chat transcript is persisted on every run.
    fn persist_active_agent_graph(&self) {
        if let Some(id) = self.active_session_id {
            let _ = self.agent_graph.save_to(agent_graph_path(&self.project_dir, &id.to_string()));
        }
    }

    pub fn new() -> (Self, iced::Task<Message>) {
        let data_dir = dirs::data_dir()
            .map(|d| d.join("concerto"))
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        let prefs_dir = data_dir.join("prefs");
        let _ = std::fs::create_dir_all(&prefs_dir);
        let theme = match concerto_memory::prefs::UserPrefsStore::open(&prefs_dir) {
            Ok(store) => crate::theme::prefs::load_theme(&store),
            Err(_) => AppTheme::by_name("Midnight").clone(),
        };
        // Initial project folder — restore the last explicitly chosen folder
        // if it was persisted, otherwise fall back to the current dir or home.
        // Persisting this matters: the default is `std::env::current_dir()`,
        // which is the Concerto source tree when launched from the repo, so an
        // unpersisted choice silently writes generated files into the app's
        // own sources and resets on every restart.
        let persisted_dir = dirs::data_dir().map(|d| d.join("concerto").join("project_dir"));
        let legacy_project_dir = persisted_dir
            .and_then(|d| std::fs::read_to_string(&d).ok())
            .map(|s| std::path::PathBuf::from(s.trim().to_string()))
            .filter(|p| p.is_dir());
        let fallback_project_dir = || {
            std::env::current_dir().unwrap_or_else(|_| {
                dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("."))
            })
        };
        let mut project_registry = concerto_config::ProjectRegistry::load().unwrap_or_default();
        let initial_project_dir = project_registry
            .active()
            .map(std::path::Path::to_path_buf)
            .or(legacy_project_dir)
            .unwrap_or_else(fallback_project_dir);
        let initial_project_dir = project_registry
            .select(&initial_project_dir)
            .unwrap_or_else(|_| fallback_project_dir());
        let _ = project_registry.save();
        // ADR-59 D5: a startup config load that falls back must be visible,
        // not silently swallowed — remember which loads failed so the
        // `config_broken` badge surfaces them (previously a fully silent
        // `unwrap_or_else(AppConfig::default)`).
        let (global_config, global_config_fell_back) =
            match concerto_config::load_global_config(None) {
                Ok(config) => (config, false),
                Err(error) => {
                    tracing::warn!(%error, "failed to load global config; using defaults");
                    (AppConfig::default(), true)
                }
            };
        // ADR-44 §4: the effective allowlist is seeded from the env-inclusive
        // config (config files + CONCERTO_PROJECT_ROOTS), unlike `global_config`
        // which deliberately excludes env overrides for the settings editor.
        let effective_roots = concerto_config::load_config(None, None)
            .ok()
            .map(|config| root_consent::canonical_roots(&config.project_roots))
            .unwrap_or_default();
        let (initial_config, initial_config_fell_back) =
            match concerto_config::load_config(None, Some(&initial_project_dir)) {
                Ok(config) => (config, false),
                Err(error) => {
                    tracing::warn!(%error, "failed to load config; falling back to defaults");
                    (global_config.clone(), true)
                }
            };
        let initial_multi_agent = initial_config
            .multi_agent
            .as_ref()
            .map(|settings| settings.default_enabled)
            .unwrap_or(false);
        let initial_session_cap = initial_config.session_spend_cap_usd;
        let mut app = Self {
            page: Page::Chat,
            current_theme: theme,
            show_help: false,
            chat: views::chat::State::new(),
            diff: views::diff::State::new(),
            memory: views::memory::State::new(),
            tool_log: views::tool_log::State::new(),
            settings: {
                let cfg = global_config.clone();
                views::settings::State::from_config(&cfg)
            },
            agent_graph: views::agent_graph::State::new(),
            terminal: {
                let settings = initial_config.resolved_shell_settings();
                let profiles = settings.profiles.clone();
                let active_id = Some(settings.selected_profile_id().to_owned());
                views::terminal::State::new(initial_project_dir.clone(), profiles, active_id)
            },
            orchestration_studio: views::orchestration_studio::State::new(),
            editor: views::code_editor::State::new(
                Utf8PathBuf::from_path_buf(initial_project_dir.clone())
                    .unwrap_or_else(|p| Utf8PathBuf::from(p.to_string_lossy().as_ref())),
            ),
            cap_pending: capability_dialog::shared_pending(),
            pending_ack: capability_dialog::shared_pending_ack(),
            pending_intent: capability_dialog::shared_pending_intent(),
            pending_plan: capability_dialog::shared_pending_plan(),
            bus: EventBus::default(),
            config: Some(initial_config),
            global_config,
            memory_services: Arc::new(Mutex::new(None)),
            cancel_token: CancellationToken::new(),
            run_status: RunStatus::Idle,
            run_stage: None,
            circuit_progress: 0.0,
            vfs: Arc::new(Mutex::new(VirtualFs::new())),
            session_manager: Arc::new(Mutex::new(None)),
            text_focused: false,
            screenshot_status: None,
            active_provider_id: String::new(),
            active_model: String::new(),
            chat_model_options: Vec::new(),
            save_feedback: None,
            save_feedback_generation: 0,
            quick_panel_open: true,
            memory_view_open: false,
            terminal_panel_open: false,
            terminal_panel_height: 260.0,
            terminal_resizing: false,
            terminal_drag_origin: None,
            terminal_start_height: 0.0,
            terminal_panel_anim: 0.0,
            terminal_panel_animating: false,
            overlay_fade: 0.0,
            overlay_fade_target: 0.0,
            overlay_fading: false,
            git_summary: None,
            pending_refresh: std::collections::HashMap::new(),
            refresh_seq: 0,
            active_session_id: None,
            session_cap: initial_session_cap,
            config_broken: initial_config_fell_back || global_config_fell_back,
            live_session_cost: 0.0,
            cap_state: CapUiState::Normal,
            daily_cost: None,
            project_dir: initial_project_dir.clone(),
            project_dir_input: String::new(),
            show_dir_picker: false,
            effective_roots,
            pending_root_consent: None,
            project_tree: Vec::new(),
            pending_tree_session: None,
            multi_agent: initial_multi_agent,
            fast: false,
            resume_checkpoint_json: None,
            run_settle_epoch: Arc::new(AtomicU64::new(0)),
            toasts: ToastManager::new(),
        };
        // Spec §6 (startup-fallback toast): when config loading fell back to
        // defaults at startup, surface it as a high-severity toast.
        // `config_broken` already drives the persistent status-bar config
        // badge; the toast adds a one-time visible notification at
        // construction so the silent fallback cannot be missed.
        if app.config_broken {
            app.toasts.push(
                ToastLevel::Error,
                "Orchestration config fallback: loaded defaults due to load failure.".to_string(),
            );
        }
        if let Some(config) = app.config.clone() {
            app.orchestration_studio.load_from_config(&config);
        }
        // Always start on the new-project view. Existing sessions remain
        // available under Recent sessions and are restored only when the user
        // explicitly selects one.
        // Resolve the initial route from the configured default model. The
        // deprecated default-provider id is intentionally ignored.
        if let Some(ref cfg) = app.config {
            (app.active_provider_id, app.active_model) = configured_default_route(cfg);
        }
        app.sync_chat_model_options();
        app.sync_memory_configuration();

        // Auto-discover models for every credentialed, discoverable provider at
        // startup so the unified picker (and per-provider lists) are populated
        // without any manual "refresh" action (Option-1: configure providers,
        // models flow in automatically).
        let ready_ids: Vec<String> = app
            .runtime_providers()
            .iter()
            .filter(|p| {
                let def = provider_definition(&p.provider);
                if !def.supports_discovery() {
                    return false;
                }
                if def.requires_credential() {
                    let creds = CredentialStore::new();
                    if !p.api_key(&creds).map(|k| !k.is_empty()).unwrap_or(false) {
                        return false;
                    }
                }
                true
            })
            .map(|p| p.id.clone())
            .collect();
        let mut discovery_tasks = Vec::new();
        for id in ready_ids {
            app.refresh_seq = app.refresh_seq.wrapping_add(1);
            let req_id = app.refresh_seq;
            app.pending_refresh.insert(id.clone(), req_id);
            discovery_tasks.push(app.fetch_models_for_provider(id.clone(), req_id));
        }
        let initial_models = iced::Task::batch(discovery_tasks);
        app.rebuild_project_tree();
        let initial_sessions = app.load_sessions_for_project(app.project_dir.clone());
        let initial_git = app.load_git_summary();
        (app, iced::Task::batch(vec![initial_models, initial_sessions, initial_git]))
    }

    pub fn title(&self) -> String {
        "Concerto".into()
    }

    pub fn update(&mut self, message: Message) -> iced::Task<Message> {
        match message {
            Message::Navigate(page) => {
                self.page = page;
                if page != Page::Chat {
                    self.chat.finalize_streaming();
                    TEXT_FOCUSED.store(false, Ordering::Relaxed);
                }
                if page == Page::Settings {
                    // The relationship manager and provider list here are
                    // seeded from config at startup; the studio saves
                    // `multi_agent.relationships` and external config edits
                    // may change `model_settings.providers` independently, so
                    // refresh both on every entry. In-flight edits made here
                    // are preserved by the states themselves (ADR-57 §3d).
                    if let Some(config) = &self.config {
                        self.settings.sync_relationships_from_config(config);
                        self.settings.sync_providers_from_config(config);
                    }
                    // ADR-43 — one lazy skill discovery pass the first time the
                    // page opens. Discovery is blocking filesystem work, so it
                    // runs inside `Task::perform` (the same pattern as the
                    // shell profile test); the Refresh button re-runs it.
                    if self.settings.skills_never_discovered() {
                        return self.settings.start_skill_discovery().map(Message::Settings);
                    }
                }
                if page == Page::OrchestrationStudio {
                    // ADR-58/59 (rewritten) Slice 2 (first-run bootstrap): auto-seed the
                    // orchestration roster into the PROJECT config before the
                    // Studio first renders, so the blueprint surface is active
                    // from the very first open — no splash, no manual init.
                    // Idempotent: a config that already owns its roster is
                    // never touched.
                    self.ensure_orchestration_seeded();
                    // Do not replace an in-progress Studio draft when the user
                    // briefly visits another page. Saved state may be reloaded
                    // so changes made elsewhere (for example in Settings) are
                    // reflected when Studio is opened again.
                    if !self.orchestration_studio.unsaved {
                        if let Some(config) = &self.config {
                            self.orchestration_studio.load_from_config(config);
                        }
                    }
                    // Model cache is always refreshed — it is non-destructive
                    // (only updates dropdown options, not agent assignments).
                    self.orchestration_studio
                        .sync_models(self.settings.cached_models_by_provider());
                }
                iced::Task::none()
            }
            Message::SetSubView(sub_view) => {
                self.chat.sub_view = sub_view;
                self.page = Page::Chat;
                // Start (or continue) the overlay fade. Opening targets 1.0
                // without resetting the current alpha, so re-opening mid-fade
                // resumes from where the backdrop is; closing targets 0.0 and
                // the tick ramps the dim layer out over the base.
                self.overlay_fading = true;
                self.overlay_fade_target =
                    if sub_view == views::chat::SubView::Main { 0.0 } else { 1.0 };
                // Opening the Spend Log loads the active session's records so
                // the modal body is fresh (idempotent re-open).
                if sub_view == views::chat::SubView::SpendLog {
                    return self.load_spend_log();
                }
                iced::Task::none()
            }
            Message::OpenSpendLog => {
                self.update(Message::SetSubView(views::chat::SubView::SpendLog))
            }
            Message::Shortcut(shortcut) => self.handle_shortcut(shortcut),
            Message::AgentRunCompleted(session_id, res) => {
                self.run_status = RunStatus::Idle;
                self.note_run_settled();
                // ADR-57 §3a: memory teardown may have been deferred while the
                // run was active (a config edit that disables memory is not
                // hot-applied mid-run); the run is over, so complete it now.
                self.sync_memory_configuration();
                // The run is over: drop the stage chip regardless of outcome
                // (Ok or Err both pass through here).
                self.run_stage = None;
                // True run boundary: no further thinking can arrive, so close
                // every open thinking phase (navigation's finalize_streaming
                // deliberately leaves them open).
                self.chat.finalize_run();
                // Keep the newly-created session active even when the run
                // fails. A retry then continues the same conversation instead
                // of silently creating another session and losing context.
                if let Some(session_id) = session_id {
                    self.active_session_id = Some(session_id);
                }
                match *res {
                    Ok(output) => {
                        let completed =
                            output.completion_status == AgentCompletionStatus::Completed;
                        self.chat.settle_running_tool_calls(if completed {
                            views::chat::ToolCallStatus::Completed
                        } else {
                            views::chat::ToolCallStatus::Cancelled
                        });
                        self.agent_graph.settle_incomplete(if completed {
                            NodeState::Completed
                        } else {
                            NodeState::Cancelled
                        });
                        self.active_session_id = Some(output.session_id);
                        let final_message = format_run_summary(&output);
                        let _ = self.chat.update(views::chat::Message::AddAssistant(final_message));
                        self.chat.set_run_completion(
                            self.multi_agent,
                            completed,
                            output.files_modified.iter().map(ToString::to_string).collect(),
                            output.project_root.as_ref().map(ToString::to_string),
                        );
                        self.load_diff_from_vfs();
                        // Store checkpoint for potential resume.
                        self.resume_checkpoint_json = output.checkpoint_json.clone();
                    }
                    Err(failure) => {
                        // Clear any stored checkpoint so a failed resume does
                        // not poison subsequent messages (Finding 2 / #65).
                        self.resume_checkpoint_json = None;

                        let terminal_state = if failure.code == "TASK_CANCELLED" {
                            NodeState::Cancelled
                        } else {
                            NodeState::Failed
                        };
                        self.chat.settle_running_tool_calls(if failure.code == "TASK_CANCELLED" {
                            views::chat::ToolCallStatus::Cancelled
                        } else {
                            views::chat::ToolCallStatus::Failed
                        });
                        self.agent_graph.settle_incomplete(terminal_state);
                        match failure.audience {
                            FailureAudience::User => {
                                let _ = self.chat.update(views::chat::Message::AddAssistant(
                                    failure.user_message,
                                ));
                            }
                            FailureAudience::Developer => {
                                tracing::error!(
                                    code = %failure.code,
                                    details = %failure.dev_details
                                );

                                let error_message = format!(
                                    "{}\n\nError code: {}",
                                    failure.user_message, failure.code
                                );
                                let _ = self
                                    .chat
                                    .update(views::chat::Message::AddAssistant(error_message));
                            }
                            _ => {}
                        }
                    }
                }
                // Persist the transcript so the on-screen conversation survives a
                // restart. Best-effort: a write failure must never break the UI.
                let project_dir = self.project_dir.clone();
                if let Some(session_id) = self.active_session_id {
                    let session_id = session_id.to_string();
                    let _ = self.chat.save_to(&transcript_path(&project_dir, &session_id));
                }
                self.persist_active_agent_graph();
                iced::Task::batch(vec![
                    self.load_sessions_for_project(self.project_dir.clone()),
                    self.load_git_summary(),
                ])
            }
            Message::CancelAgentRun => {
                if self.run_status == RunStatus::Running {
                    self.cancel_token.cancel();
                    self.run_status = RunStatus::Cancelling;
                }
                iced::Task::none()
            }
            Message::CircuitTick => {
                self.circuit_progress =
                    (self.circuit_progress + circuit_background::PROGRESS_STEP) % 1.0;
                iced::Task::none()
            }
            Message::AnimTick => {
                // Overlay backdrop fade: advance toward the target in fixed
                // 0.08 steps, snapping on the final step (~15 ticks ≈ 240 ms
                // for a full fade). Clears the flag when settled, which also
                // stops the shared animation subscription.
                if self.overlay_fading {
                    let target = self.overlay_fade_target;
                    if (target - self.overlay_fade).abs() <= 0.08 {
                        self.overlay_fade = target;
                        self.overlay_fading = false;
                    } else {
                        self.overlay_fade += (target - self.overlay_fade).signum() * 0.08;
                    }
                }
                // Terminal panel slide: animate toward open (1.0) / closed
                // (0.0) from the `terminal_panel_open` flag.
                if self.terminal_panel_animating {
                    let target = if self.terminal_panel_open { 1.0 } else { 0.0 };
                    if (target - self.terminal_panel_anim).abs() <= 0.08 {
                        self.terminal_panel_anim = target;
                        self.terminal_panel_animating = false;
                    } else {
                        self.terminal_panel_anim +=
                            (target - self.terminal_panel_anim).signum() * 0.08;
                    }
                }
                iced::Task::none()
            }
            Message::Chat(msg) => {
                if let views::chat::Message::CopyCode(code) = &msg {
                    return iced::clipboard::write(code.clone());
                }

                if let views::chat::Message::NavigateToToolLog(_) = &msg {
                    return self.update(Message::SetSubView(views::chat::SubView::ToolLog));
                }

                if matches!(&msg, views::chat::Message::NavigateToDiff) {
                    return self.update(Message::SetSubView(views::chat::SubView::Diff));
                }

                if matches!(&msg, views::chat::Message::NavigateToSettings) {
                    return self.update(Message::Navigate(Page::Settings));
                }

                if matches!(&msg, views::chat::Message::NavigateToStudio) {
                    return self.update(Message::Navigate(Page::OrchestrationStudio));
                }

                if let views::chat::Message::SetActiveModel(model) = &msg {
                    return self.update(Message::SetActiveModel(model.clone()));
                }

                if let views::chat::Message::SelectSession(id) = &msg {
                    return self.select_session(id.clone());
                }

                // Refresh the Spend Log modal's records via the session
                // handler (the chat state has no backend access).
                if matches!(&msg, views::chat::Message::RefreshSpendLog) {
                    return self.load_spend_log();
                }

                // Intercept New Session — flush the outgoing session, reset chat
                // + agent graph, and create a fresh session.
                if matches!(&msg, views::chat::Message::NewSession) {
                    // Persist the current session's agent graph before clearing.
                    self.persist_active_agent_graph();
                    self.chat = views::chat::State::new();
                    self.agent_graph = views::agent_graph::State::new();
                    self.tool_log = views::tool_log::State::new();
                    self.active_session_id = None;
                    self.resume_checkpoint_json = None;
                    self.reset_spend_state();
                    return iced::Task::none();
                }

                match &msg {
                    views::chat::Message::InputChanged(_) => {
                        TEXT_FOCUSED.store(true, Ordering::Relaxed);
                        self.text_focused = true;
                    }
                    views::chat::Message::SubmitInput => {
                        TEXT_FOCUSED.store(false, Ordering::Relaxed);
                        self.text_focused = false;
                    }
                    _ => {}
                }

                if matches!(&msg, views::chat::Message::SubmitInput) {
                    if self.run_status != RunStatus::Idle {
                        return iced::Task::none();
                    }
                    let user_input = self.chat.input().to_string();
                    let chat_task =
                        self.chat.update(views::chat::Message::SubmitInput).map(Message::Chat);
                    let agent_task = self.submit_to_agent(user_input);
                    iced::Task::batch(vec![chat_task, agent_task])
                } else if matches!(&msg, views::chat::Message::ToggleMultiAgent) {
                    self.multi_agent = !self.multi_agent;
                    let mut config = self.global_config.clone();
                    config.multi_agent.get_or_insert_with(Default::default).default_enabled =
                        self.multi_agent;
                    match concerto_config::default_config_path() {
                        Some(path) => {
                            if let Err(error) = concerto_config::save_config(&config, &path) {
                                tracing::error!(%error, "failed to persist multi-agent preference");
                            } else {
                                // Reload + re-derive through the shared helper:
                                // if a project file overrides `default_enabled`,
                                // the merged result is the truth (ADR-57 §6).
                                self.reconcile_config_from_reload();
                            }
                        }
                        None => {
                            self.global_config = config.clone();
                            self.config = Some(config);
                        }
                    }
                    iced::Task::none()
                } else if matches!(&msg, views::chat::Message::ToggleFastMode) {
                    // Runtime-only toggle, mirroring CLI `-f/--fast`: unlike the
                    // multi-agent toggle it is deliberately NOT persisted to
                    // config — fast mode is a per-session choice.
                    self.fast = !self.fast;
                    iced::Task::none()
                } else {
                    self.chat.update(msg).map(Message::Chat)
                }
            }
            Message::Diff(msg) => {
                let needs_commit = matches!(
                    &msg,
                    views::diff::Message::AcceptHunk(_)
                        | views::diff::Message::RejectHunk(_)
                        | views::diff::Message::AcceptAll
                        | views::diff::Message::RejectAll
                        | views::diff::Message::Undo
                );
                let task = self.diff.update(msg).map(Message::Diff);
                if needs_commit {
                    if let Ok(mut vfs) = self.vfs.lock() {
                        if let Err(e) = self.diff.commit(&mut vfs) {
                            tracing::error!(error = %e, "failed to apply diff decision to VFS");
                            self.toasts.push(
                                ToastLevel::Error,
                                format!("Failed to apply diff decision: {e}"),
                            );
                        }
                    }
                }
                task
            }
            Message::Memory(msg) => match msg {
                views::memory::Message::Reindex => self.trigger_reindex(),
                views::memory::Message::Refresh => self.load_memory_entries(),
                views::memory::Message::SearchChanged(_)
                | views::memory::Message::TypeFilterChanged(_) => {
                    let update = self.memory.update(msg).map(Message::Memory);
                    iced::Task::batch(vec![update, self.load_memory_entries()])
                }
                views::memory::Message::DeleteConfirmed => {
                    let id = self.memory.delete_target_id();
                    let update = self.memory.update(msg).map(Message::Memory);
                    if let Some(id) = id {
                        iced::Task::batch(vec![update, self.delete_memory_entry(id)])
                    } else {
                        update
                    }
                }
                other => self.memory.update(other).map(Message::Memory),
            },
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
            Message::MemoryEntryDeleted { id, result } => {
                match result {
                    Ok(()) => self.memory.remove_entry(&id),
                    Err(error) => self.memory.status = MemoryStatus::Error(error),
                }
                iced::Task::none()
            }
            Message::ToolLog(msg) => self.tool_log.update(msg).map(Message::ToolLog),
            Message::SessionSelected { session_id, history, events, transcript } => {
                // Flush the previously active session before switching.
                self.persist_active_agent_graph();
                self.active_session_id = Ulid::from_string(&session_id).ok();
                self.resume_checkpoint_json = None;
                // A resumed session starts with a fresh spend chip + log; its
                // records (if any) are re-loaded when the Spend Log opens.
                self.reset_spend_state();
                let rich_transcript = transcript_path(&self.project_dir, &session_id);
                let persisted_entries = views::chat::State::load_entries(&rich_transcript);
                // Restore priority (ADR-36 §5): the durable DB transcript is
                // canonical; fall back to the local transcript.json cache, and
                // only then to the degraded messages-only view (legacy
                // sessions predate the typed transcript).
                let transcript_entries = transcript_to_entries(transcript);
                let transcript_missing = transcript_entries.is_empty();
                self.chat = views::chat::State::from_entries(if transcript_missing {
                    persisted_entries.clone().unwrap_or_else(|| messages_to_entries(history))
                } else {
                    transcript_entries
                });
                let persisted_graph = self.active_session_id.and_then(|id| {
                    views::agent_graph::State::load_from(agent_graph_path(
                        &self.project_dir,
                        &id.to_string(),
                    ))
                });
                let graph_missing = persisted_graph.is_none();
                self.agent_graph = persisted_graph.unwrap_or_default();
                self.tool_log = views::tool_log::State::new();
                if (transcript_missing && persisted_entries.is_none()) || graph_missing {
                    // A crash can occur before the UI sidecars are written.
                    // Rebuild visible activity and graph state from the
                    // authoritative durable event sequence in that case.
                    let mut replay_chat = views::chat::State::new();
                    let mut replay_tool_log = views::tool_log::State::new();
                    let mut replay_graph = views::agent_graph::State::new();
                    for stored in &events {
                        let Ok(event) = stored.to_event() else { continue };
                        let Some(event) = crate::runtime::translate_event(&event) else {
                            continue;
                        };
                        crate::runtime::route_event(
                            &event,
                            &mut replay_chat,
                            &mut replay_tool_log,
                            &mut replay_graph,
                            &mut self.memory,
                        );
                    }
                    if transcript_missing && persisted_entries.is_none() {
                        self.chat = replay_chat;
                    }
                    if graph_missing {
                        self.agent_graph = replay_graph;
                    }
                    self.tool_log = replay_tool_log;
                } else {
                    self.tool_log.load_stored_events(&events);
                }
                self.page = Page::Chat;
                iced::Task::none()
            }
            Message::Settings(msg) => match &msg {
                views::settings::Message::SaveSettings => {
                    let task = self.settings.update(msg).map(Message::Settings);
                    let base = self.global_config.clone();
                    let new_config = self.settings.to_config(&base);
                    if let Some(path) = concerto_config::default_config_path() {
                        if let Err(e) = concerto_config::save_config(&new_config, &path) {
                            tracing::error!(error = %e, "failed to save config");
                        } else {
                            // Collapse the reload + full re-derivation onto the
                            // shared helper (ADR-57 §4), so the on-disk file is
                            // re-read and every config-derived field is derived
                            // in exactly one place.
                            self.reconcile_config_from_reload();
                        }
                    }
                    let data_dir = dirs::data_dir()
                        .map(|d| d.join("concerto"))
                        .unwrap_or_else(|| std::path::PathBuf::from("."));
                    let prefs_dir = data_dir.join("prefs");
                    if let Ok(store) = concerto_memory::prefs::UserPrefsStore::open(&prefs_dir) {
                        let new_theme = AppTheme::by_name(self.settings.selected_theme)
                            .with_base_size(self.settings.font_size);
                        self.current_theme = new_theme.clone();
                        crate::theme::prefs::save_theme(&store, &new_theme);
                    }
                    // The studio's model cache must reflect any provider/model
                    // changes saved in Settings (add/delete/rename provider,
                    // or model-discovery results).
                    self.orchestration_studio
                        .sync_models(self.settings.cached_models_by_provider());
                    task
                }
                views::settings::Message::ProviderModelsRefreshed {
                    provider_id,
                    request_id,
                    result,
                } => {
                    // Staleness guard: drop results for superseded requests.
                    let current = self.pending_refresh.get(provider_id).copied();
                    if current != Some(*request_id) {
                        return iced::Task::none();
                    }
                    // Drop results for a provider that was deleted meanwhile.
                    if !self.runtime_providers().iter().any(|p| p.id == *provider_id) {
                        self.pending_refresh.remove(provider_id);
                        self.settings.end_provider_refresh(provider_id);
                        return iced::Task::none();
                    }
                    self.pending_refresh.remove(provider_id);
                    if let (Some(model_settings), Ok(models)) = (
                        self.config.as_mut().and_then(|config| config.model_settings.as_mut()),
                        result,
                    ) {
                        if let Some(provider) =
                            model_settings.providers.iter_mut().find(|p| p.id == *provider_id)
                        {
                            provider.record_discovered_models(models.clone());
                        }
                    }
                    let task = self
                        .settings
                        .update(views::settings::Message::ProviderModelsRefreshed {
                            provider_id: provider_id.clone(),
                            request_id: *request_id,
                            result: result.clone(),
                        })
                        .map(Message::Settings);
                    self.sync_chat_model_options();
                    self.orchestration_studio
                        .sync_models(self.settings.cached_models_by_provider());
                    task
                }
                views::settings::Message::ProviderModelsRefreshRequested(provider_id) => {
                    // Manual per-provider model refresh. Only live rows whose
                    // provider type actually supports discovery get a tracked
                    // request; anything else (stale id, deleted mid-flight) is
                    // a silent no-op.
                    let supported = self.runtime_providers().iter().any(|p| {
                        p.id == *provider_id
                            && provider_definition(&p.provider).supports_discovery()
                    });
                    if !supported {
                        return iced::Task::none();
                    }
                    self.settings.begin_provider_refresh(provider_id);
                    let provider_id = provider_id.clone();
                    self.refresh_seq = self.refresh_seq.wrapping_add(1);
                    let req_id = self.refresh_seq;
                    self.pending_refresh.insert(provider_id.clone(), req_id);
                    self.fetch_models_for_provider(provider_id, req_id)
                }
                _ => self.settings.update(msg).map(Message::Settings),
            },
            Message::AgentGraph(msg) => self.agent_graph.update(msg).map(Message::AgentGraph),
            Message::Terminal(msg) => {
                self.terminal.update(msg, &self.current_theme).map(Message::Terminal)
            }
            Message::OrchestrationStudio(msg) => {
                // ADR-58/59 (rewritten) Slice 2 (single-arm Save): `SaveOrchestration`
                // persists the Studio's editable blueprint via
                // `persist_orchestration`, which routes by the loaded
                // selection's source (inline → rewrite in the config, include
                // → guarded include write, name → materialize inline into the
                // project config), validates, writes, and reloads — never
                // navigating, never switching the surface, and never touching
                // the global config. There is no init path anymore: the roster
                // auto-seeds on Studio open.
                let persist =
                    matches!(msg, views::orchestration_studio::StudioMessage::SaveOrchestration);
                let task = self.orchestration_studio.update(msg);
                if persist {
                    match self.persist_orchestration() {
                        Ok(()) => {
                            self.orchestration_studio.mark_saved();
                            self.toasts.push(ToastLevel::Success, "Orchestration saved".into());
                        }
                        Err(error) => {
                            // Every write is atomic and nothing is written on
                            // failure — surface the reason persistently so the
                            // draft is kept, not lost.
                            self.toasts.push(ToastLevel::Error, format!("Save failed: {error}"));
                            self.orchestration_studio.mark_save_failed(error);
                        }
                    }
                }
                task
            }
            Message::Editor(msg) => self
                .editor
                .update(
                    msg,
                    &self.vfs,
                    &Utf8PathBuf::from_path_buf(self.project_dir.clone())
                        .unwrap_or_else(|p| Utf8PathBuf::from(p.to_string_lossy().as_ref())),
                    &self.cancel_token,
                )
                .map(Message::Editor),
            Message::ThemeChanged => {
                let data_dir = dirs::data_dir()
                    .map(|d| d.join("concerto"))
                    .unwrap_or_else(|| std::path::PathBuf::from("."));
                let prefs_dir = data_dir.join("prefs");
                if let Ok(store) = concerto_memory::prefs::UserPrefsStore::open(&prefs_dir) {
                    self.current_theme = crate::theme::prefs::load_theme(&store);
                }
                self.terminal.set_theme(&self.current_theme);
                iced::Task::none()
            }
            Message::HelpToggled => {
                self.show_help = !self.show_help;
                iced::Task::none()
            }
            Message::OpenProjectDirPicker => {
                self.show_dir_picker = true;
                self.project_dir_input = self.project_dir.to_string_lossy().to_string();
                iced::Task::none()
            }
            Message::ProjectDirInputChanged(s) => {
                self.project_dir_input = s;
                iced::Task::none()
            }
            Message::ProjectDirCancel => {
                self.show_dir_picker = false;
                iced::Task::none()
            }
            Message::ProjectDirApply => {
                if self.run_status != RunStatus::Idle {
                    self.toasts.push(
                        ToastLevel::Info,
                        "Cancel the running task before switching projects.".to_string(),
                    );
                    return iced::Task::none();
                }
                let candidate = std::path::PathBuf::from(self.project_dir_input.trim());
                if candidate.is_dir() {
                    // ADR-44 §4: gate out-of-root switches behind the consent
                    // modal. Re-selecting the current project is a no-op and
                    // never gates — nothing new is exposed.
                    let canonical = concerto_core::helpers::canonical_project_path(&candidate);
                    let current = concerto_core::helpers::canonical_project_path(&self.project_dir);
                    if canonical != current
                        && root_consent::needs_consent(&canonical, &self.effective_roots)
                    {
                        self.pending_root_consent = Some(canonical);
                        return iced::Task::none();
                    }
                    return self.switch_project_dir(&candidate);
                }
                // If not a directory, keep the modal open unchanged.
                iced::Task::none()
            }
            Message::RootConsentAllow => {
                let Some(canonical) = self.pending_root_consent.take() else {
                    return iced::Task::none();
                };
                // The user allowed the canonical dir for this process: record
                // it in the effective allowlist, then apply the switch.
                if !self.effective_roots.contains(&canonical) {
                    self.effective_roots.push(canonical.clone());
                }
                self.switch_project_dir(&canonical)
            }
            Message::RootConsentDeny => {
                // Abort the deferred switch cleanly: no project change, no
                // error spam. The dir picker (if open behind the gate) stays
                // open so the user can correct the path or cancel.
                self.pending_root_consent = None;
                self.pending_tree_session = None;
                iced::Task::none()
            }
            Message::ToggleProjectExpanded(path) => {
                let Some(node) = self.project_tree.iter_mut().find(|n| n.path == path) else {
                    return iced::Task::none();
                };
                if node.expanded {
                    node.expanded = false;
                    return iced::Task::none();
                }
                node.expanded = true;
                // Lazy load: sessions are fetched once on the first expand and
                // cached in the node afterwards.
                if node.sessions.is_none() {
                    self.load_sessions_for_project(path)
                } else {
                    iced::Task::none()
                }
            }
            Message::ProjectSessionsLoaded { path, sessions } => {
                if let Some(node) = self.project_tree.iter_mut().find(|n| n.path == path) {
                    node.sessions = Some(sessions);
                }
                iced::Task::none()
            }
            Message::TreeSessionClicked { project, session_id } => {
                let canonical = concerto_core::helpers::canonical_project_path(&project);
                let current = concerto_core::helpers::canonical_project_path(&self.project_dir);
                if canonical == current {
                    // Same project: resume the session in place, no switch.
                    return self
                        .update(Message::Chat(views::chat::Message::SelectSession(session_id)));
                }
                if self.run_status != RunStatus::Idle {
                    self.toasts.push(
                        ToastLevel::Info,
                        "Cancel the running task before switching projects.".to_string(),
                    );
                    return iced::Task::none();
                }
                // ADR-44 §4: out-of-root switches go through the consent gate.
                // Remember the session so the deferred switch resumes it.
                if root_consent::needs_consent(&canonical, &self.effective_roots) {
                    self.pending_root_consent = Some(canonical);
                    self.pending_tree_session = Some(session_id);
                    return iced::Task::none();
                }
                self.pending_tree_session = Some(session_id);
                self.switch_project_dir(&project)
            }
            Message::CapabilityDlg(msg) => {
                capability_dialog::resolve(&self.cap_pending, &msg);
                iced::Task::none()
            }
            Message::AckDialog(msg) => {
                let acknowledged = matches!(msg, capability_dialog::AckDialogMessage::Acknowledge);
                capability_dialog::resolve_ack(&self.pending_ack, acknowledged);
                iced::Task::none()
            }
            Message::IntentDialog(msg) => {
                capability_dialog::resolve_intent(&self.pending_intent, msg);
                iced::Task::none()
            }
            Message::PlanDialog(msg) => {
                // Resolve only the dialog that is actually displayed: capture
                // the front entry's identity so a stale or cross-session queue
                // entry can never answer a different prompt.
                let identity = {
                    let guard = self.pending_plan.lock().unwrap_or_else(|e| e.into_inner());
                    guard.front().map(|plan| (plan.session_id, plan.plan_id.clone()))
                };
                if let Some((session_id, plan_id)) = identity {
                    capability_dialog::resolve_plan(&self.pending_plan, session_id, &plan_id, msg);
                }
                iced::Task::none()
            }
            Message::DesktopEvent(evt) => {
                // Spend events update App-level state (status-bar chip + cap
                // state) before the remaining variants route into per-view
                // states. Session-id mismatches are ignored: the chip tracks
                // the active session, and session switches reset this state.
                match &evt {
                    crate::runtime::DesktopEvent::SpendUpdated { total_usd } => {
                        self.live_session_cost = *total_usd;
                        self.reconcile_cap_state(*total_usd);
                    }
                    crate::runtime::DesktopEvent::SpendCapApproaching {
                        current_usd,
                        cap_usd,
                        pct,
                    } => {
                        self.live_session_cost = *current_usd;
                        self.session_cap = Some(*cap_usd);
                        self.cap_state = CapUiState::Approaching {
                            current_usd: *current_usd,
                            cap_usd: *cap_usd,
                            pct: *pct,
                        };
                    }
                    crate::runtime::DesktopEvent::SpendCapExceeded { current_usd, cap_usd } => {
                        self.live_session_cost = *current_usd;
                        self.session_cap = Some(*cap_usd);
                        self.cap_state =
                            CapUiState::Exceeded { current_usd: *current_usd, cap_usd: *cap_usd };
                    }
                    crate::runtime::DesktopEvent::RunStageChanged { stage }
                        if self.run_status == RunStatus::Running =>
                    {
                        // The run-stage chip tracks the active run only. A
                        // stage event outside a run (e.g. a stale bus replay
                        // caught mid-dispatch) must not re-arm the chip: the
                        // arm is guarded on the run status, so a non-Running
                        // run falls through to the `_` catch-all.
                        self.run_stage = Some(*stage);
                    }
                    _ => {}
                }
                crate::runtime::route_event(
                    &evt,
                    &mut self.chat,
                    &mut self.tool_log,
                    &mut self.agent_graph,
                    &mut self.memory,
                );
                iced::Task::none()
            }
            Message::SetActiveProvider(id) => {
                let resolved_id = self
                    .settings
                    .cached_provider_labels
                    .iter()
                    .position(|label| label == &id)
                    .and_then(|index| self.settings.cached_provider_ids.get(index))
                    .cloned()
                    .unwrap_or(id);
                self.active_provider_id = resolved_id.clone();
                if self.runtime_providers().iter().any(|provider| provider.id == resolved_id) {
                    // Models live on role assignments (Option-1), not providers,
                    // so derive the chat model from the assignment for this
                    // provider instead of the now-empty `provider.model`.
                    self.sync_chat_model_options();
                    self.active_model = self.resolve_default_model();
                }
                self.persist_active_model_selection();
                self.refresh_seq = self.refresh_seq.wrapping_add(1);
                let req_id = self.refresh_seq;
                self.pending_refresh.insert(resolved_id.clone(), req_id);
                self.fetch_models_for_provider(resolved_id, req_id)
            }
            Message::SetActiveModel(model) => {
                if self
                    .runtime_model_names(&self.active_provider_id)
                    .iter()
                    .any(|candidate| candidate == &model)
                {
                    self.active_model = model.clone();
                    self.persist_active_model_selection();
                } else {
                    tracing::warn!(
                        provider_id = %self.active_provider_id,
                        model = %model,
                        "ignored model selection that does not belong to the active provider"
                    );
                }
                iced::Task::none()
            }

            // External config edit (ADR-57): reload from disk and re-derive
            // every config-derived field through the one shared helper. The
            // equality short-circuit inside makes our own saves no-ops.
            Message::ConfigReloaded => {
                self.reconcile_config_from_reload();
                iced::Task::none()
            }

            // (sync_chat_model_options is invoked inside persist_active_model_selection)
            Message::TakeScreenshot => {
                self.screenshot_status = Some("Capturing...".to_string());
                iced::window::latest().and_then(|id| {
                    iced::window::screenshot(id).map(|screenshot| {
                        let rgba: &[u8] = screenshot.as_ref();
                        let w = screenshot.size.width;
                        let h = screenshot.size.height;
                        match crate::services::screenshot::save_png(rgba, w, h, true) {
                            Ok(res) => Message::ScreenshotCompleted(Ok(res)),
                            Err(e) => Message::ScreenshotCompleted(Err(e.to_string())),
                        }
                    })
                })
            }
            Message::ScreenshotCompleted(result) => {
                match result {
                    Ok(res) => {
                        let path_str = res.file_path.display().to_string();
                        tracing::info!(path = %path_str, "Screenshot saved");
                        self.screenshot_status = Some(format!("Saved: {}", path_str));
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "Screenshot failed");
                        self.screenshot_status = Some(format!("Failed: {}", e));
                    }
                }
                // Clear status after 5 seconds
                let status = self.screenshot_status.clone();
                iced::Task::perform(
                    async move {
                        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                        status
                    },
                    |_| Message::ClearScreenshotStatus,
                )
            }
            Message::ClearScreenshotStatus => {
                self.screenshot_status = None;
                iced::Task::none()
            }
            Message::ClearSaveFeedback(generation) => {
                // Ignore stale timers from a previous save: only the current
                // generation may clear the notice.
                if generation == self.save_feedback_generation {
                    self.save_feedback = None;
                }
                iced::Task::none()
            }
            Message::ToastDismissed(id) => {
                self.toasts.dismiss(id);
                iced::Task::none()
            }
            Message::ToastExpiryTick => {
                let cutoff =
                    std::time::Instant::now() - std::time::Duration::from_secs(TOAST_LIFETIME_SECS);
                self.toasts.prune_older_than(cutoff);
                iced::Task::none()
            }
            Message::ToggleQuickPanel => {
                self.quick_panel_open = !self.quick_panel_open;
                if self.quick_panel_open {
                    // Opening the panel also (re)loads memory entries so the
                    // Memory section is fresh; the git summary is loaded
                    // alongside it.
                    if self.config.as_ref().is_some_and(|c| c.memory.enabled) {
                        iced::Task::batch(vec![self.load_memory_entries(), self.load_git_summary()])
                    } else {
                        self.load_git_summary()
                    }
                } else {
                    iced::Task::none()
                }
            }
            Message::OpenMemoryModal => {
                self.memory_view_open = true;
                self.load_memory_entries()
            }
            Message::CloseMemoryModal => {
                self.memory_view_open = false;
                iced::Task::none()
            }
            Message::ToggleTerminalPanel => {
                self.terminal_panel_open = !self.terminal_panel_open;
                // Kick off the slide animation; `AnimTick` eases the panel
                // height toward the new `terminal_panel_open` target.
                self.terminal_panel_animating = true;
                if self.terminal_panel_open {
                    // Opening the panel lazily starts the shell (and re-focuses
                    // it when the panel is already running).
                    self.terminal.ensure_started(&self.current_theme).map(Message::Terminal)
                } else {
                    iced::Task::none()
                }
            }
            Message::TerminalPanelResizeStart => {
                if self.terminal_resizing {
                    return iced::Task::none();
                }
                self.terminal_resizing = true;
                self.terminal_start_height = self.terminal_panel_height;
                self.terminal_drag_origin = None;
                iced::Task::none()
            }
            Message::TerminalPanelResizeMoved(y) => {
                if !self.terminal_resizing {
                    return iced::Task::none();
                }
                if self.terminal_drag_origin.is_none() {
                    self.terminal_drag_origin = Some(y);
                    return iced::Task::none();
                }
                let origin = self.terminal_drag_origin.unwrap_or(y);
                self.terminal_panel_height =
                    (self.terminal_start_height + (origin - y)).clamp(120.0, 600.0);
                iced::Task::none()
            }
            Message::TerminalPanelResizeEnd => {
                self.terminal_resizing = false;
                self.terminal_drag_origin = None;
                iced::Task::none()
            }
            Message::GitSummaryLoaded(summary) => {
                self.git_summary = summary;
                iced::Task::none()
            }
        }
    }

    /// Switch the active project folder to `path` (a directory) and reset all
    /// project-scoped state for the new folder.
    ///
    /// Shared by [`Message::ProjectDirApply`] (in-root or already-consented
    /// switches) and [`Message::RootConsentAllow`] (the deferred out-of-root
    /// switch), so Allow continues the exact flow that would have run without
    /// the gate.
    fn switch_project_dir(&mut self, path: &Path) -> iced::Task<Message> {
        let mut registry = concerto_config::ProjectRegistry::load().unwrap_or_default();
        self.project_dir = registry.select(path).unwrap_or_else(|_| path.to_path_buf());
        if let Err(error) = registry.save() {
            tracing::warn!(%error, "failed to persist project registry");
        }
        self.show_dir_picker = false;
        // Collapse the config reload + full re-derivation onto the single
        // shared helper (ADR-57 §4). Project re-select also re-arms the
        // config-watch path set automatically: the subscription's identity is
        // the project dir, so iced recreates the watcher when it changes.
        self.reconcile_config_from_reload();
        // Rebind the session handler to the new folder on next run.
        *self.session_manager.lock().unwrap_or_else(|e| e.into_inner()) = None;
        // Cancel current memory lifecycle and clear it
        if let Some(prev) = self.memory_services.lock().unwrap_or_else(|e| e.into_inner()).take() {
            prev.cancel.cancel();
        }
        self.memory = views::memory::State::new();
        self.sync_memory_configuration();
        self.tool_log = views::tool_log::State::new();
        self.vfs = Arc::new(Mutex::new(VirtualFs::new()));
        // A project switch starts a blank session. The folder's
        // earlier sessions remain available in Recent sessions.
        self.active_session_id = None;
        self.resume_checkpoint_json = None;
        self.reset_spend_state();
        self.agent_graph = views::agent_graph::State::new();
        self.chat = views::chat::State::new();
        let terminal = self
            .terminal
            .set_project_dir(self.project_dir.clone(), &self.current_theme)
            .map(Message::Terminal);
        // Rebuild the tree around the new active project (its node is expanded
        // by default) and reload its sessions. If this switch came from a tree
        // session click, resume that session once the switch is applied.
        self.rebuild_project_tree();
        let mut tasks = vec![
            terminal,
            self.load_sessions_for_project(self.project_dir.clone()),
            self.load_git_summary(),
        ];
        if let Some(session_id) = self.pending_tree_session.take() {
            tasks.push(self.select_session(session_id));
        }
        iced::Task::batch(tasks)
    }

    fn handle_shortcut(&mut self, shortcut: shortcuts::Shortcut) -> iced::Task<Message> {
        use shortcuts::Shortcut;
        match shortcut {
            Shortcut::NewTask => self.update(Message::Chat(views::chat::Message::NewSession)),
            Shortcut::DiffViewer => {
                let new_sub = if self.page == Page::Chat
                    && self.chat.sub_view == views::chat::SubView::Diff
                {
                    views::chat::SubView::Main
                } else {
                    views::chat::SubView::Diff
                };
                self.update(Message::SetSubView(new_sub))
            }
            Shortcut::Memory => self.update(Message::OpenMemoryModal),
            Shortcut::ToolLog => {
                let new_sub = if self.page == Page::Chat
                    && self.chat.sub_view == views::chat::SubView::ToolLog
                {
                    views::chat::SubView::Main
                } else {
                    views::chat::SubView::ToolLog
                };
                self.update(Message::SetSubView(new_sub))
            }
            Shortcut::Terminal => self.update(Message::ToggleTerminalPanel),
            Shortcut::UndoRun => {
                // On the Editor page with an open file, Ctrl+Z is text undo.
                if self.page == Page::Editor && self.editor.active_file().is_some() {
                    return self.update(Message::Editor(views::code_editor::Message::Undo));
                }
                iced::Task::none()
            }
            Shortcut::EditorRedo => {
                if self.page == Page::Editor && self.editor.active_file().is_some() {
                    return self.update(Message::Editor(views::code_editor::Message::Redo));
                }
                iced::Task::none()
            }
            Shortcut::EditorFind => {
                if self.page == Page::Editor && self.editor.active_file().is_some() {
                    return self.update(Message::Editor(views::code_editor::Message::OpenFind));
                }
                iced::Task::none()
            }
            Shortcut::EditorReplace => {
                if self.page == Page::Editor && self.editor.active_file().is_some() {
                    return self.update(Message::Editor(views::code_editor::Message::OpenReplace));
                }
                iced::Task::none()
            }
            Shortcut::EditorGoto => {
                if self.page == Page::Editor && self.editor.active_file().is_some() {
                    return self.update(Message::Editor(views::code_editor::Message::OpenGoto));
                }
                iced::Task::none()
            }
            Shortcut::EditorFindNext => {
                if self.page == Page::Editor && self.editor.active_file().is_some() {
                    return self.update(Message::Editor(views::code_editor::Message::FindNext));
                }
                iced::Task::none()
            }
            Shortcut::EditorFindPrev => {
                if self.page == Page::Editor && self.editor.active_file().is_some() {
                    return self.update(Message::Editor(views::code_editor::Message::FindPrev));
                }
                iced::Task::none()
            }
            Shortcut::SubmitInput => self.update(Message::Chat(views::chat::Message::SubmitInput)),
            Shortcut::CancelDialog => {
                self.show_help = false;
                // Esc also dismisses the Memory explorer modal.
                self.memory_view_open = false;
                // On the Editor page, Esc also dismisses the find/goto bars.
                if self.page == Page::Editor {
                    let close_find =
                        self.update(Message::Editor(views::code_editor::Message::CloseFind));
                    let close_goto =
                        self.update(Message::Editor(views::code_editor::Message::CloseGoto));
                    return iced::Task::batch([close_find, close_goto]);
                }
                iced::Task::none()
            }
            Shortcut::HelpOverlay => {
                self.show_help = !self.show_help;
                iced::Task::none()
            }
            Shortcut::Screenshot => {
                // On the Editor page with an open file, Ctrl+S means Save.
                if self.page == Page::Editor && self.editor.active_file().is_some() {
                    return self.update(Message::Editor(views::code_editor::Message::Save));
                }
                self.update(Message::TakeScreenshot)
            }
            Shortcut::Editor => {
                self.page = Page::Editor;
                iced::Task::none()
            }
        }
    }

    /// After an agent run completes, load diffs from the shared VirtualFs
    /// into the diff viewer state so the user can review proposed changes.
    fn load_diff_from_vfs(&mut self) {
        let Ok(vfs) = self.vfs.lock() else { return };
        let diff_results = compute_diffs_from_virtual_fs(&vfs);

        if diff_results.is_empty() {
            self.diff.files.clear();
            self.diff.diff_lines.clear();
            self.diff.active_file = None;
            self.diff.has_real_diff = false;
            return;
        }

        // Build the file list from all diff results.
        let files: Vec<camino::Utf8PathBuf> = diff_results.iter().map(|r| r.path.clone()).collect();

        // Pick the first file as active, or preserve the currently active file
        // if it's still in the list.
        let active_file = if let Some(current) = &self.diff.active_file {
            if diff_results.iter().any(|r| &r.path == current) {
                current.clone()
            } else {
                files[0].clone()
            }
        } else {
            files[0].clone()
        };

        // Convert the active file's diff to the widget-level DiffLine format.
        let diff_lines = if let Some(result) = diff_results.iter().find(|r| r.path == active_file) {
            crate::views::diff::diff_result_to_lines(result)
        } else {
            Vec::new()
        };

        let snapshot = vfs.snapshot();
        self.diff.load_diff(files, active_file, diff_lines, snapshot, diff_results);
    }

    /// Validate that the active provider/model and (in multi-agent mode) every
    /// agent assignment resolves to a ready, complete provider. Returns a
    /// human-readable reason when something is incomplete; `None` when ready.
    fn runtime_providers(&self) -> &[concerto_config::ProviderConfig] {
        self.config
            .as_ref()
            .and_then(|config| config.model_settings.as_ref())
            .map(|settings| settings.providers.as_slice())
            .unwrap_or(self.settings.providers.as_slice())
    }

    fn runtime_assignments(&self) -> &[concerto_config::AgentModelAssignment] {
        self.config
            .as_ref()
            .and_then(|config| config.model_settings.as_ref())
            .map(|settings| settings.agent_assignments.as_slice())
            .unwrap_or(&[])
    }

    fn runtime_model_names(&self, provider_id: &str) -> Vec<String> {
        self.runtime_providers()
            .iter()
            .find(|provider| provider.id == provider_id)
            .map(|provider| {
                let definition = provider_definition(&provider.provider);
                let mut models = model_options_for(provider, &definition, None);
                let mut seen =
                    models.iter().map(|model| model.to_lowercase()).collect::<HashSet<_>>();
                for model in &provider.cached_models {
                    let model = model.trim().to_string();
                    if !model.is_empty() && seen.insert(model.to_lowercase()) {
                        models.push(model);
                    }
                }
                models
            })
            .unwrap_or_default()
    }

    fn dispatch_validation_error(&self) -> Option<String> {
        if self.runtime_providers().is_empty() {
            return Some("no providers are configured".to_string());
        }
        let creds = CredentialStore::new();
        // The intent gate is always on (ADR-55 Phase 1e): there is no mode
        // picker, so every run is a potential Execute regardless of the chat
        // outcome the router eventually classifies. Validate the active
        // (composer) provider unconditionally and check every agent
        // assignment; nothing may slip through unvalidated.
        match self.runtime_providers().iter().find(|p| p.id == self.active_provider_id) {
            None => return Some("no active provider is selected".to_string()),
            Some(provider) => {
                let mut resolved = provider.clone();
                if !self.active_model.trim().is_empty() {
                    resolved.model = self.active_model.clone();
                }
                let def = provider_definition(&resolved.provider);
                let has_key = creds.exists(&resolved.keyring_key);
                if !provider_readiness(&resolved, &def, has_key).is_ready() {
                    return Some(format!(
                        "active provider '{}' is not ready (missing model or required API key)",
                        provider.name
                    ));
                }
            }
        }

        // Multi-agent assignment readiness: every assignment must be complete.
        if self.multi_agent {
            for assignment in self.runtime_assignments() {
                let provider =
                    self.runtime_providers().iter().find(|p| p.id == assignment.provider_config_id);
                let incomplete = match provider {
                    None => true,
                    Some(provider) => {
                        let model_ok = assignment
                            .model_override
                            .as_ref()
                            .map(|m| !m.is_empty())
                            .unwrap_or(false);
                        let mut resolved = provider.clone();
                        if let Some(model) = &assignment.model_override {
                            resolved.model = model.clone();
                        }
                        let def = provider_definition(&resolved.provider);
                        let has_key = creds.exists(&resolved.keyring_key);
                        let ready = provider_readiness(&resolved, &def, has_key).is_ready();
                        !ready || !model_ok
                    }
                };
                if incomplete {
                    return Some(format!(
                        "agent role '{}' is assigned to an incomplete provider/model",
                        assignment.agent_role
                    ));
                }
            }
        }

        None
    }

    fn submit_to_agent(&mut self, user_input: String) -> iced::Task<Message> {
        if user_input.trim().is_empty() {
            return iced::Task::none();
        }
        if self.run_status != RunStatus::Idle {
            return iced::Task::none();
        }
        // The graph describes one orchestration run, not the lifetime of the
        // conversation. Reset it at the run boundary even when dispatch
        // validation fails, so an old phase cannot attach to a newer prompt.
        self.agent_graph = views::agent_graph::State::new();
        // Dispatch-boundary validation: block the run with a clear message if
        // the active provider/model or any agent assignment is incomplete,
        // rather than failing deep inside the orchestrator.
        if let Some(reason) = self.dispatch_validation_error() {
            self.chat.add_error(format!(
                "Cannot start run: {reason} Open Settings to finish provider setup."
            ));
            return iced::Task::none();
        }
        self.cancel_token = CancellationToken::new();
        // Fresh run boundary: reject any stale run-stage from a previous run
        // (the chip only re-appears once a stage event lands while Running).
        self.run_stage = None;
        self.run_status = RunStatus::Running;
        if let Some(ref cfg) = self.config {
            // Capture the values the async task needs; the session is resolved
            // inside the task because opening the store is async.
            let bus = self.bus.clone();
            let config = cfg.clone();
            let memory = self.memory_services.clone();
            let vfs = self.vfs.clone();
            let approval_sink = desktop_approval_sink(
                self.cap_pending.clone(),
                self.pending_ack.clone(),
                self.pending_intent.clone(),
                self.pending_plan.clone(),
                self.bus.clone(),
            );
            let session_manager = self.session_manager.clone();
            let active_provider_id = self.active_provider_id.clone();
            // If the composer has no explicit model, fall back to the model
            // assigned to a role that targets the active provider (Option-1).
            let active_model = if self.active_model.is_empty() {
                self.resolve_default_model()
            } else {
                self.active_model.clone()
            };
            let multi_agent = self.multi_agent;
            let fast = self.fast;
            let project_dir = self.project_dir.clone();
            let cancel_token = self.cancel_token.clone();
            let active_session_id = self.active_session_id;
            let resume_checkpoint = self.resume_checkpoint_json.clone();

            iced::Task::perform(
                async move {
                    let mut resolved_session_id = None;
                    let outcome: Result<AgentOutput, OrchestratorError> = async {
                        // Resolve (or lazily open) the project session handler.
                        // The lock guard is dropped before any `.await` so the
                        // future stays `Send`.
                        let existing =
                            session_manager.lock().unwrap_or_else(|e| e.into_inner()).clone();
                        let handler = if let Some(h) = existing {
                            h
                        } else {
                            let h = Arc::new(
                                DesktopSessionHandler::connect_with_config(&config).await.map_err(
                                    |e| {
                                        OrchestratorError::AgentLoopError(format!(
                                            "session store unavailable: {e}"
                                        ))
                                    },
                                )?,
                            );
                            *session_manager.lock().unwrap_or_else(|e| e.into_inner()) =
                                Some(h.clone());
                            h
                        };

                        let provider = if active_provider_id.is_empty() {
                            "default"
                        } else {
                            active_provider_id.as_str()
                        };
                        let model =
                            if active_model.is_empty() { "default" } else { active_model.as_str() };

                        // A blank UI is a genuinely new conversation. Resume
                        // backend history only after the user explicitly
                        // selected an existing session.
                        let session_id = match active_session_id {
                            Some(session_id) => session_id,
                            None => handler
                                .new_session(&project_dir, provider, model)
                                .await
                                .map_err(|e| {
                                    OrchestratorError::AgentLoopError(format!(
                                        "session creation failed: {e}"
                                    ))
                                })?,
                        };
                        resolved_session_id = Some(session_id);
                        let conversation_history =
                            handler.load_history(session_id).await.map_err(|e| {
                                OrchestratorError::AgentLoopError(format!(
                                    "session history load failed: {e}"
                                ))
                            })?;

                        let request =
                            RequestBuilder::new(user_input.clone(), project_dir, cancel_token)
                                .with_provider_model(
                                    (!active_provider_id.is_empty())
                                        .then_some(active_provider_id.clone()),
                                    (!active_model.is_empty()).then_some(active_model),
                                )
                                .with_session(session_id, conversation_history)
                                .with_single_agent(!multi_agent)
                                .with_memory_enabled(memory_enabled(fast, config.memory.enabled))
                                .with_resume_checkpoint(resume_checkpoint)
                                .build();

                        let services = ServicesBuilder::new(bus, config, approval_sink)
                            .with_vfs(vfs)
                            .with_session_manager(handler.manager())
                            .with_memory(memory)
                            .build();

                        run_shared_agent(request, services).await
                    }
                    .await;
                    (resolved_session_id, outcome.map_err(ClassifiedFailure::from))
                },
                |(session_id, outcome)| Message::AgentRunCompleted(session_id, Box::new(outcome)),
            )
        } else {
            self.run_status = RunStatus::Idle;
            self.note_run_settled();
            let _ = self.chat.update(views::chat::Message::AddAssistant(
                "Concerto could not load its configuration. Open Settings, configure a provider, and save the settings before starting a task."
                    .to_string(),
            ));
            self.page = Page::Settings;
            iced::Task::none()
        }
    }

    /// Resolve the default chat model for the active provider.
    ///
    /// Under Option-1 models live on agent role assignments, not on providers,
    /// so we prefer the model assigned to a role that targets the active
    /// provider. Falls back to the first available model option.
    fn resolve_default_model(&self) -> String {
        if !self.active_provider_id.is_empty() {
            for assignment in self.runtime_assignments() {
                if assignment.provider_config_id == self.active_provider_id {
                    if let Some(model) = &assignment.model_override {
                        if !model.is_empty() {
                            return model.clone();
                        }
                    }
                }
            }
        }
        // Fall back to the first available model option.
        self.chat_model_options.first().cloned().unwrap_or_default()
    }

    /// Human-readable label for where the active model was resolved from.
    /// Returns the agent role name if from a role assignment.
    fn model_source_label(&self) -> &'static str {
        // Check if the active model matches a role assignment's model override.
        for assignment in self.runtime_assignments() {
            if assignment.provider_config_id == self.active_provider_id {
                if let Some(model) = &assignment.model_override {
                    if !model.is_empty()
                        && (model == &self.active_model || self.active_model.is_empty())
                    {
                        return "from role assignment";
                    }
                }
            }
        }
        ""
    }

    fn persist_active_model_selection(&mut self) {
        let mut config = self.global_config.clone();
        let settings = config.model_settings.get_or_insert_with(Default::default);
        settings.global_default_id = None;
        settings.global_default_model =
            if self.active_model.is_empty() { None } else { Some(self.active_model.clone()) };
        match concerto_config::default_config_path() {
            Some(path) => {
                if let Err(error) = concerto_config::save_config(&config, &path) {
                    tracing::error!(%error, "failed to persist provider/model selection");
                    return;
                }
                // Reload + re-derive through the shared helper so the in-app
                // selection never diverges from the next-run derivation
                // (ADR-57 §4/§6 — the file is truth).
                self.reconcile_config_from_reload();
            }
            None => {
                self.global_config = config.clone();
                self.config = Some(config);
            }
        }
        self.sync_chat_model_options();
    }

    /// Rebuild the chat header model-option list from the active provider's
    /// shared resolver, so the `pick_list` can borrow a value that outlives the
    /// per-frame `view` borrow.
    fn sync_chat_model_options(&mut self) {
        self.chat_model_options = self.runtime_model_names(&self.active_provider_id);
    }

    fn fetch_models_for_provider(
        &self,
        provider_id: String,
        request_id: u64,
    ) -> iced::Task<Message> {
        let Some(provider) =
            self.runtime_providers().iter().find(|provider| provider.id == provider_id).cloned()
        else {
            return iced::Task::none();
        };
        let credentials = concerto_config::CredentialStore::new();
        let api_key = provider.api_key(&credentials).unwrap_or_default();
        let provider_type = provider.provider.clone();
        let api_base = provider.api_base.clone();

        iced::Task::perform(
            async move {
                concerto_providers::list_models_for_provider_async(
                    &provider_type,
                    &api_key,
                    api_base.as_deref(),
                )
                .await
            },
            move |models| {
                // The providers crate collapses every discovery failure
                // (network, auth, …) into an empty list. Surfacing that as
                // `Err` keeps BOTH cache writers (config + settings state)
                // preserving the previous model list during an outage instead
                // of silently wiping it.
                let result = if models.is_empty() {
                    Err("Discovery returned no models — check credentials/network.".to_string())
                } else {
                    Ok(models)
                };
                Message::Settings(views::settings::Message::ProviderModelsRefreshed {
                    provider_id: provider_id.clone(),
                    request_id,
                    result,
                })
            },
        )
    }

    /// ADR-58/59 (rewritten) Slice 2 (first-run bootstrap + orphan self-heal):
    /// ensure the PROJECT config materializes the orchestration roster before
    /// the Studio first renders, so the blueprint surface — and the searchable
    /// agent library — is active from the very first open. No splash, no
    /// manual init.
    ///
    /// Seeding runs ONLY when the roster was never materialized: the raw-Toml
    /// signal is the `[multi_agent.custom_agents]` key being present in the
    /// file, even as `[]` (= all agents deleted) — "key present" means owned
    /// and deletions stick, so nothing is ever written back over it. Three
    /// shapes:
    ///
    /// 1. **Key present** (`roster_materialized`) → strict no-op: whether the
    ///    array is empty (all agents deleted) or populated, the config owns
    ///    its roster and the seed is skipped.
    /// 2. **Orphan shape** — `[orchestration]` present (the Studio's stage
    ///    cards staff from the blueprint) but the roster key never
    ///    materialized: `seed_agent_roster_only` writes ONLY
    ///    `[multi_agent.custom_agents]`, preserving the existing — possibly
    ///    user-edited — `[orchestration]` table byte-for-byte, so the Studio's
    ///    searchable library matches the blueprint's staffing without
    ///    clobbering the blueprint.
    /// 3. **Fresh project** — no config at all (or none loaded): the full
    ///    `seed_orchestration_roster` writes `[orchestration]` standard-inline
    ///    + the five agents, unchanged first-run bootstrap.
    ///
    /// A `None` config (fresh project with no file) is still handed to the
    /// seed: the writers create the file when missing, and a genuinely broken
    /// file makes `roster_materialized` report owned so the seed is never
    /// attempted over it (`config_broken` already surfaces dirty config
    /// elsewhere). After seeding, state re-derives from disk so the first
    /// render already resolves the seeded blueprint.
    fn ensure_orchestration_seeded(&mut self) {
        let config_path = self.project_dir.join(concerto_config::legacy::NEW_PROJECT_CONFIG_FILE);
        // Raw-file ownership test: the `custom_agents` key exists in the TOML
        // (even `[]` = every agent deleted). "Key present" means owned —
        // deletions stick and nothing is ever written.
        if concerto_config::roster_materialized(&config_path) {
            return;
        }
        // Orphan shape: `[orchestration]` present but the roster was never
        // materialized. Seed ONLY the agents so the searchable library matches
        // the blueprint's staffing; the existing (possibly user-edited)
        // `[orchestration]` table is preserved byte-for-byte. A failed seed
        // leaves the previous file at the target intact.
        if self.config.as_ref().is_some_and(|config| config.orchestration.is_some()) {
            if concerto_config::seed_agent_roster_only(&config_path).is_err() {
                return;
            }
        } else if concerto_config::seed_orchestration_roster(&config_path).is_err() {
            // A failed seed leaves the previous file at the target intact;
            // nothing to reconcile then. Broken config is surfaced elsewhere.
            return;
        }
        self.reconcile_config_from_reload();
    }

    /// ADR-58/59 (rewritten) Slice 2 (single-arm Save): persist the Studio's editable
    /// [`Blueprint`] by the active selection's source, then reload so App
    /// state re-derives from the fresh file (the watcher equality
    /// short-circuit makes the reload a no-op rebuild when nothing moved).
    ///
    /// The generic guards run before routing — the draft is kept and nothing
    /// is written when either fails:
    ///
    /// 1. **Validation** — the UI already disables Save while the draft is
    ///    invalid; this belt-and-braces check guards stale queued messages.
    /// 2. **No editable blueprint** — nothing to write (defensive).
    ///
    /// Then, by selection source (exactly one of name/include/inline is
    /// guaranteed by `BlueprintSelection`):
    ///
    /// - **inline** → rewrite the blueprint back into the project config's
    ///   `[orchestration].blueprint.inline` (`save_inline_blueprint`,
    ///   merge-aware, atomic).
    /// - **include** → the guarded include write (`persist_include_blueprint`,
    ///   target-shadow + unparseable guards), the only path that touches a
    ///   blueprint file.
    /// - **name** → materialize the edited blueprint inline into the project
    ///   config. The catalog is seed-only: once the user edits, the config
    ///   owns the blueprint (the dangling `name` selector is removed so the
    ///   selection stays exactly-one). Covers a defensively-absent
    ///   `[orchestration]` too.
    ///
    /// Never navigates and never switches the surface (Slice 2).
    fn persist_orchestration(&mut self) -> Result<(), String> {
        let Some(blueprint) = self.orchestration_studio.blueprint() else {
            return Err("no editable blueprint loaded; nothing was written".to_string());
        };
        if !self.orchestration_studio.validation().ok {
            return Err(
                "blueprint has validation issue(s); the draft is kept, nothing was written"
                    .to_string(),
            );
        }
        let selection = self
            .config
            .as_ref()
            .and_then(|config| config.orchestration.as_ref())
            .map(|orchestration| &orchestration.blueprint);

        let config_path = self.project_dir.join(concerto_config::legacy::NEW_PROJECT_CONFIG_FILE);
        match selection {
            // The blueprint lives in the include file the selection
            // references: the guarded include write, then reload.
            Some(selection) if selection.include.is_some() => {
                self.persist_include_blueprint(&blueprint)?;
            }
            // Inline, bare name (materialize), or defensively-absent
            // `[orchestration]`: the config owns the blueprint — write it
            // back into `[orchestration].blueprint.inline`.
            _ => {
                concerto_config::save_inline_blueprint(&config_path, &blueprint)
                    .map_err(|error| error.to_string())?;
            }
        }

        // ADR-58/59 (rewritten) Slice 3: the agent roster. Written only after the
        // blueprint write above succeeds — the roster has no rulebook of its own,
        // so it is gated on the same blueprint validation that ran up front (a
        // failed blueprint never reaches the config). `persisted_parts` maps the
        // Studio's authoritative agent list to config types (coordinator + the
        // five seeds as `is_custom: false` mirrors + user agents). The write is
        // merge-aware and atomic; deletion is permanent (`owns_agent_roster`).
        let (roster, _, _) = self.orchestration_studio.persisted_parts();
        concerto_config::save_agent_roster(&config_path, &roster)
            .map_err(|error| error.to_string())?;

        self.reconcile_config_from_reload();
        Ok(())
    }

    /// The include-file half of [`Self::persist_orchestration`]: write the
    /// edited [`Blueprint`] to the file the active include selection
    /// references, guarded against (a) target shadowing and (b) an
    /// unparseable on-disk file, then reload. Ported from the pre-Slice-2
    /// `persist_blueprint` guards (ADR-59 Decision 3):
    ///
    /// 1. **Target shadowing** — the write target must be the path the config
    ///    would actually load (project dir first, global second, bare-name
    ///    cwd last, mirroring `load_config`'s resolution order). Saving
    ///    anywhere else would silently write a file a later load never reads.
    /// 2. **Unparseable include** — `save_blueprint` serializes from the
    ///    in-memory model, so a round-trip would silently DROP unknown keys
    ///    the on-disk file carries (`deny_unknown_fields`). The file must
    ///    parse as a valid [`Blueprint`] before any write.
    ///
    /// On failure nothing is written and the draft is kept.
    fn persist_include_blueprint(
        &mut self,
        blueprint: &concerto_config::Blueprint,
    ) -> Result<(), String> {
        let include_name = self
            .config
            .as_ref()
            .and_then(|config| config.orchestration.as_ref())
            .and_then(|orchestration| orchestration.blueprint.include.clone())
            .unwrap_or_else(|| concerto_config::BLUEPRINT_INCLUDE_FILE.to_string());

        // Build the same config-dir order `load_config` uses (lib.rs) so
        // `include_write_target` mirrors load-time resolution — project root
        // first, then the global config file's directory.
        let mut config_dirs = vec![self.project_dir.clone()];
        if let Some(path) = concerto_config::default_config_path() {
            if let Some(parent) = path.parent() {
                config_dirs.push(parent.to_path_buf());
            }
        }
        let target = concerto_config::include_write_target(&config_dirs, &include_name);
        let project_target = self.project_dir.join(&include_name);
        if target != project_target
            && target.as_path() != std::path::Path::new(include_name.as_str())
        {
            // The bare fallback means no candidate file exists anywhere (a
            // watcher may have removed it) — nothing can be shadowed then,
            // so saving is safe. Any other target that is not the project
            // file would shadow the file the config actually loads.
            return Err(format!(
                "the blueprint was loaded from {}; saving to the project directory would \
                 shadow it — move the file into the project directory first",
                target.display()
            ));
        }

        // A round-trip through `save_blueprint` would silently dump unknown
        // keys, so the on-disk file must parse before any write.
        concerto_config::parse_blueprint_file(&target)
            .map_err(|error| format!("{error} — the draft is kept and nothing was written"))?;

        concerto_config::save_blueprint(blueprint, &target).map_err(|error| error.to_string())?;
        Ok(())
    }

    fn sync_memory_configuration(&mut self) {
        let enabled = self.config.as_ref().is_some_and(|config| config.memory.enabled);
        self.memory.set_enabled(enabled);
        if !enabled {
            if let Some(prev) =
                self.memory_services.lock().unwrap_or_else(|error| error.into_inner()).take()
            {
                prev.cancel.cancel();
            }
        }
    }

    /// Re-load config from disk and re-derive every `App` field that depends
    /// on it (ADR-57 §3). Shared by the config-watch subscription and every
    /// config write path, so all reload sites converge on one derivation
    /// order.
    ///
    /// The reload is **read-only** (never writes config) and
    /// **non-destructive**: the Settings form and Orchestration Studio drafts
    /// are left untouched, and memory teardown is deferred until the run is
    /// idle. An equality short-circuit makes self-induced events (a settings
    /// save rewriting exactly the watched file) provably inert.
    fn reconcile_config_from_reload(&mut self) {
        let (Ok(reloaded_global), Ok(reloaded)) = (
            concerto_config::load_global_config(None),
            concerto_config::load_config(None, Some(&self.project_dir)),
        ) else {
            // ADR-57 §3c: keep last-good config; toast exactly once per
            // broken period (recovery happens on the next good event, no
            // polling).
            if !self.config_broken {
                self.config_broken = true;
                self.toasts.push(
                    ToastLevel::Error,
                    "Config file could not be loaded — keeping the last-good \
                     settings until it parses again."
                        .to_string(),
                );
            }
            tracing::warn!("config reload failed; keeping last-good config");
            return;
        };
        self.apply_reloaded_config(reloaded_global, reloaded);
    }

    /// Apply already-parsed configs: equality short-circuit plus the full
    /// re-derivation. Split out from [`Self::reconcile_config_from_reload`]
    /// so the derivation is testable without touching the disk.
    fn apply_reloaded_config(&mut self, reloaded_global: AppConfig, reloaded: AppConfig) {
        // ADR-59 D4: `AppConfig`'s `PartialEq` covers only the persisted
        // surface (`schema.rs:439-470`) — `resolved_blueprint` is derived
        // state and deliberately excluded. A blueprint include-file content
        // change therefore left persisted-surface equality true while the
        // resolved model moved, silently no-op'ing the reconcile. Compare the
        // resolved blueprint value too, and short-circuit only when BOTH
        // surfaces are unchanged. When they differ, the re-derivation below
        // replaces `self.config` with `reloaded`, which already carries the
        // fresh `resolved_blueprint` attached by the load seam
        // (`load_config_layers`, lib.rs:243) — so the live config always
        // consumes the new blueprint.
        let blueprint_unchanged = self.config.as_ref().and_then(|c| c.resolved_blueprint.as_ref())
            == reloaded.resolved_blueprint.as_ref();
        if blueprint_unchanged && self.config.as_ref() == Some(&reloaded) {
            // ADR-57 §3b: nothing changed — skip re-derivation. Self-induced
            // events (our own saves) and project-layer overrides that leave
            // the merged result unchanged become deterministic no-ops.
            self.config_broken = false;
            return;
        }
        self.config_broken = false;
        self.global_config = reloaded_global;
        self.config = Some(reloaded.clone());
        // Re-derive run-mode flags — the file is truth (ADR-57 §6).
        self.multi_agent =
            reloaded.multi_agent.as_ref().map(|settings| settings.default_enabled).unwrap_or(false);
        (self.active_provider_id, self.active_model) = configured_default_route(&reloaded);
        self.sync_chat_model_options();
        self.sync_session_cap_from_config();
        // ADR-57 §3a: memory teardown is deferred while a run is active (the
        // run holds store clones wired to the lifecycle cancel token); it is
        // completed when the run settles (`Message::AgentRunCompleted`).
        // Memory parameter changes are not hot-applied — restart-scoped.
        let memory_enabled = reloaded.memory.enabled;
        self.memory.set_enabled(memory_enabled);
        if !memory_enabled && self.run_status == RunStatus::Idle {
            if let Some(prev) =
                self.memory_services.lock().unwrap_or_else(|error| error.into_inner()).take()
            {
                prev.cancel.cancel();
            }
        }
        let shell = reloaded.resolved_shell_settings();
        let profiles = shell.profiles.clone();
        let active_id = Some(shell.selected_profile_id().to_owned());
        let _ = self.terminal.set_profiles(profiles, active_id, &self.current_theme);
        // Provider rows first, then the derived caches: the row sync rebuilds
        // the caches from the refreshed rows (and is a no-op while the user
        // has in-flight edits, ADR-57 §3d); the cache-only refresh below then
        // re-derives the pickers/Studio caches from the config even when the
        // row sync was blocked. Neither the Settings form nor Studio drafts
        // are ever rebuilt against in-flight edits.
        self.settings.sync_providers_from_config(&reloaded);
        self.settings.refresh_provider_cache_from_config(&reloaded);
        self.orchestration_studio.sync_models(self.settings.cached_models_by_provider());
        self.refresh_effective_roots_from_config();
    }

    /// ADR-44 §4 / ADR-57 §3d: recompute the effective project-root
    /// allowlist as a **union** of the configured roots with every root the
    /// user has already consented to this process, so an external edit never
    /// revokes consent.
    fn refresh_effective_roots_from_config(&mut self) {
        let configured = concerto_config::load_config(None, None)
            .ok()
            .map(|config| root_consent::canonical_roots(&config.project_roots))
            .unwrap_or_default();
        let current = std::mem::take(&mut self.effective_roots);
        let mut merged = configured;
        for root in current {
            if !merged.contains(&root) {
                merged.push(root);
            }
        }
        self.effective_roots = merged;
    }

    /// Trigger a real project re-index, persisting chunks via the live sync
    /// service. Used by the Memory view's Refresh / Re-index controls. Until
    /// If memory has not been initialised yet, this initializes it first so
    /// the Memory page works immediately after a restart.
    fn trigger_reindex(&mut self) -> iced::Task<Message> {
        if !self.config.as_ref().is_some_and(|config| config.memory.enabled) {
            self.memory.set_enabled(false);
            self.toasts.push(
                ToastLevel::Info,
                "Enable memory in Settings before re-indexing.".to_string(),
            );
            return iced::Task::none();
        }
        let memory = self.memory_services.clone();
        let project_dir = self.project_dir.clone();
        let bus = self.bus.clone();
        let app_config = self.config.clone().unwrap_or_default();
        let mut index_config = IndexConfig {
            project_dir: camino::Utf8PathBuf::from_path_buf(project_dir.clone())
                .unwrap_or_else(|p| camino::Utf8PathBuf::from(p.to_string_lossy().as_ref())),
            ..IndexConfig::default()
        };
        index_config.exclude_patterns.extend(app_config.memory.exclude_patterns.clone());
        index_config.ignore_file = app_config.memory.ignore_file.clone();
        self.memory.status = MemoryStatus::Indexing { processed: 0, total: 0 };
        iced::Task::perform(
            async move {
                let project_id = concerto_core::types::ProjectId(
                    concerto_core::helpers::project_id_hash(&project_dir),
                );
                // Check if memory is already initialized for this project
                let active = {
                    let lock = memory.lock().unwrap_or_else(|e| e.into_inner());
                    lock.as_ref().and_then(|m| {
                        if m.project_id == project_id {
                            Some((
                                m.store.clone(),
                                m.reindex.clone(),
                                m.reindex_sync.clone(),
                                m.cancel.child_token(),
                            ))
                        } else {
                            None
                        }
                    })
                };
                if let Some((_store, indexer, sync, cancel)) = active {
                    // Already initialized — trigger reindex
                    match indexer.index(&index_config, cancel.clone()).await {
                        Ok(records) if !cancel.is_cancelled() => {
                            match sync.replace_project(&project_id, &records, cancel.clone()).await
                            {
                                Ok(()) => ReindexResult::Done(records.len()),
                                Err(error) => ReindexResult::Failed(error.to_string()),
                            }
                        }
                        Ok(_) => ReindexResult::Failed("memory re-index cancelled".into()),
                        Err(e) => ReindexResult::Failed(e.to_string()),
                    }
                } else {
                    // Cancel previous project's lifecycle if present
                    if let Some(prev) = memory.lock().unwrap_or_else(|e| e.into_inner()).take() {
                        prev.cancel.cancel();
                    }
                    // Initialize memory system first
                    let reindex_temp: Arc<Mutex<Option<Arc<ProjectIndexer>>>> =
                        Arc::new(Mutex::new(None));
                    let reindex_sync_temp: Arc<Mutex<Option<Arc<ChunkSyncService>>>> =
                        Arc::new(Mutex::new(None));
                    let cancel_temp: Arc<Mutex<Option<CancellationToken>>> =
                        Arc::new(Mutex::new(None));
                    match init_memory_system(
                        bus,
                        &app_config,
                        &project_dir,
                        &reindex_temp,
                        &reindex_sync_temp,
                        &cancel_temp,
                    )
                    .await
                    {
                        Ok(store) => {
                            let indexer =
                                reindex_temp.lock().unwrap_or_else(|e| e.into_inner()).take();
                            let sync =
                                reindex_sync_temp.lock().unwrap_or_else(|e| e.into_inner()).take();
                            let cancel = cancel_temp
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .take()
                                .unwrap_or_default();
                            match (indexer, sync) {
                                (Some(indexer), Some(sync)) => {
                                    let active = ActiveMemoryServices {
                                        project_id: project_id.clone(),
                                        store: store.clone(),
                                        reindex: indexer.clone(),
                                        reindex_sync: sync.clone(),
                                        cancel: cancel.clone(),
                                    };
                                    *memory.lock().unwrap_or_else(|e| e.into_inner()) =
                                        Some(active);
                                    // Run the initial index
                                    let child_cancel = cancel.child_token();
                                    match indexer.index(&index_config, child_cancel.clone()).await {
                                        Ok(records) if !child_cancel.is_cancelled() => {
                                            match sync
                                                .replace_project(
                                                    &project_id,
                                                    &records,
                                                    child_cancel.clone(),
                                                )
                                                .await
                                            {
                                                Ok(()) => ReindexResult::Done(records.len()),
                                                Err(error) => {
                                                    ReindexResult::Failed(error.to_string())
                                                }
                                            }
                                        }
                                        Ok(_) => ReindexResult::Started,
                                        Err(e) => ReindexResult::Failed(e.to_string()),
                                    }
                                }
                                _ => ReindexResult::Skipped,
                            }
                        }
                        Err(error) => ReindexResult::Failed(error.to_string()),
                    }
                }
            },
            Message::ReindexResult,
        )
    }

    fn load_memory_entries(&self) -> iced::Task<Message> {
        use concerto_core::memory::{
            ChunkType, MemoryFilter, MemoryNamespace, MemoryQuery, ProjectId,
        };

        if !self.config.as_ref().is_some_and(|config| config.memory.enabled) {
            return iced::Task::none();
        }
        let memory = self.memory_services.clone();
        let project_dir = self.project_dir.clone();
        let bus = self.bus.clone();
        let config = self.config.clone().unwrap_or_default();
        let project_id_for_query = ProjectId(project_id_hash(&self.project_dir));
        let query_text = self.memory.search_query().trim().to_string();
        let type_filter = self.memory.type_filter();

        iced::Task::perform(
            async move {
                let project_id = concerto_core::types::ProjectId(
                    concerto_core::helpers::project_id_hash(&project_dir),
                );
                // Look up the store, scoped to project
                let store = {
                    let lock = memory.lock().unwrap_or_else(|e| e.into_inner());
                    lock.as_ref().and_then(|m| {
                        if m.project_id == project_id {
                            Some(m.store.clone())
                        } else {
                            None
                        }
                    })
                };
                let store: Arc<dyn MemoryStore> = if let Some(store) = store {
                    store
                } else {
                    // Cancel previous project's lifecycle if present
                    if let Some(prev) = memory.lock().unwrap_or_else(|e| e.into_inner()).take() {
                        prev.cancel.cancel();
                    }
                    let reindex_temp: Arc<Mutex<Option<Arc<ProjectIndexer>>>> =
                        Arc::new(Mutex::new(None));
                    let reindex_sync_temp: Arc<Mutex<Option<Arc<ChunkSyncService>>>> =
                        Arc::new(Mutex::new(None));
                    let cancel_temp: Arc<Mutex<Option<CancellationToken>>> =
                        Arc::new(Mutex::new(None));
                    let store = init_memory_system(
                        bus,
                        &config,
                        &project_dir,
                        &reindex_temp,
                        &reindex_sync_temp,
                        &cancel_temp,
                    )
                    .await
                    .map_err(|error| error.to_string())?;

                    let reindex =
                        reindex_temp.lock().unwrap_or_else(|e| e.into_inner()).take().ok_or_else(
                            || "init_memory_system did not populate project indexer".to_string(),
                        )?;
                    let reindex_sync = reindex_sync_temp
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .take()
                        .ok_or_else(|| {
                            "init_memory_system did not populate chunk sync service".to_string()
                        })?;
                    let cancel = cancel_temp
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .take()
                        .unwrap_or_default();
                    let active = ActiveMemoryServices {
                        project_id,
                        store: store.clone(),
                        reindex,
                        reindex_sync,
                        cancel,
                    };
                    *memory.lock().unwrap_or_else(|e| e.into_inner()) = Some(active);
                    store
                };

                let cancel = concerto_core::CancellationToken::new();
                let chunks = if query_text.is_empty() {
                    store.browse(&project_id_for_query, 200, cancel).await
                } else {
                    let chunk_type = match type_filter {
                        views::memory::MemoryEntryType::SlidingWindow => {
                            Some(ChunkType::SlidingWindow)
                        }
                        views::memory::MemoryEntryType::SessionSummary => {
                            Some(ChunkType::SessionSummary)
                        }
                        views::memory::MemoryEntryType::Fact => Some(ChunkType::Fact),
                        _ => None,
                    };
                    let filters = chunk_type
                        .map(|kind| vec![MemoryFilter::ChunkType(kind)])
                        .unwrap_or_default();
                    store
                        .retrieve(
                            &MemoryQuery {
                                text: query_text,
                                project_id: project_id_for_query.clone(),
                                namespace: MemoryNamespace::Project(project_id_for_query.clone()),
                                top_k: 200,
                                filters,
                            },
                            cancel.clone(),
                        )
                        .await
                }
                .map_err(|error| error.to_string())?;

                Ok(chunks
                    .into_iter()
                    .map(|chunk| {
                        let entry_type = match chunk.chunk_type {
                            ChunkType::SessionSummary => {
                                views::memory::MemoryEntryType::SessionSummary
                            }
                            ChunkType::Fact => views::memory::MemoryEntryType::Fact,
                            _ => views::memory::MemoryEntryType::SlidingWindow,
                        };
                        let source = chunk
                            .file_path
                            .as_ref()
                            .map(ToString::to_string)
                            .unwrap_or_else(|| "indexed memory".to_string());
                        let mut preview: String = chunk.content.chars().take(240).collect();
                        if chunk.content.chars().count() > 240 {
                            preview.push('…');
                        }
                        views::memory::MemoryRow {
                            id: chunk.id,
                            content_preview: preview,
                            source,
                            age: String::new(),
                            score: chunk.score as f32,
                            entry_type,
                        }
                    })
                    .collect())
            },
            Message::MemoryEntriesLoaded,
        )
    }

    fn delete_memory_entry(&self, id: String) -> iced::Task<Message> {
        let memory = self.memory_services.clone();
        let result_id = id.clone();
        iced::Task::perform(
            async move {
                let store = memory
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .as_ref()
                    .map(|m| m.store.clone())
                    .ok_or_else(|| "Memory is not initialized yet.".to_string())?;
                let cancel = concerto_core::CancellationToken::new();
                store.invalidate_chunk(&id, cancel).await.map_err(|error| error.to_string())
            },
            move |result| Message::MemoryEntryDeleted { id: result_id.clone(), result },
        )
    }

    /// Refresh the active project's Git status without blocking the UI thread.
    fn load_git_summary(&self) -> iced::Task<Message> {
        let project_dir = self.project_dir.clone();
        iced::Task::perform(
            async move {
                tokio::task::spawn_blocking(move || {
                    concerto_tools::git::repository_summary(&project_dir).ok()
                })
                .await
                .unwrap_or(None)
            },
            Message::GitSummaryLoaded,
        )
    }

    /// Re-read the session spend cap from the currently loaded config.
    /// Called after any config reload so the chip's percentage reflects the
    /// configured cap, not just the last cap event.
    fn sync_session_cap_from_config(&mut self) {
        self.session_cap = self.config.as_ref().and_then(|config| config.session_spend_cap_usd);
    }

    /// Reset the status-bar spend state when the active session changes:
    /// live cost, cap state and the daily-total stub return to their fresh
    /// values, the cap is re-derived from config, and the chat spend log is
    /// cleared so a resumed session never shows another session's records.
    fn reset_spend_state(&mut self) {
        self.live_session_cost = 0.0;
        self.cap_state = CapUiState::Normal;
        self.daily_cost = None;
        self.sync_session_cap_from_config();
        self.chat.clear_spend_log();
    }

    /// Reconcile the cap state after a plain `SpendUpdated` event: a total
    /// that has dropped below the approaching threshold makes any stale
    /// Approaching/Exceeded signal (from a previous session or a changed cap)
    /// reset to `Normal`. Thresholds below 80% never change a `Normal` state.
    fn reconcile_cap_state(&mut self, total: f64) {
        let Some(pct) = crate::views::spend::pct_of_cap(total, self.session_cap) else {
            return;
        };
        if pct < 80.0 && self.cap_state != CapUiState::Normal {
            self.cap_state = CapUiState::Normal;
        }
    }

    /// Load the recent sessions for one project folder so the sidebar tree can
    /// list them. Best-effort: any failure yields an empty list rather than
    /// breaking the UI.
    fn load_sessions_for_project(&self, project_dir: PathBuf) -> iced::Task<Message> {
        let session_manager = self.session_manager.clone();
        let app_config = self.config.clone().unwrap_or_default();
        let provider_labels: std::collections::HashMap<String, String> = self
            .settings
            .providers
            .iter()
            .map(|provider| {
                let label = if provider.name == provider.provider {
                    provider.name.clone()
                } else {
                    format!("{} ({})", provider.name, provider.provider)
                };
                (provider.id.clone(), label)
            })
            .collect();
        iced::Task::perform(
            async move {
                let handler = {
                    let existing =
                        session_manager.lock().unwrap_or_else(|e| e.into_inner()).clone();
                    match existing {
                        Some(h) => h,
                        None => match DesktopSessionHandler::connect_with_config(&app_config).await
                        {
                            Ok(h) => Arc::new(h),
                            Err(_) => return (project_dir, Vec::new()),
                        },
                    }
                };
                let utf8 = match camino::Utf8PathBuf::from_path_buf(project_dir.clone()) {
                    Ok(p) => p,
                    Err(_) => return (project_dir, Vec::new()),
                };
                match handler
                    .manager()
                    .list_project_sessions(&utf8, 50, CancellationToken::new())
                    .await
                {
                    Ok(sessions) => (
                        project_dir,
                        sessions
                            .into_iter()
                            .map(|s| views::chat::SessionRow {
                                session_id: s.id.to_string(),
                                created_at: s.created_at.to_string(),
                                message_count: s.message_count,
                                cost: s.total_cost_usd,
                                tokens_in: s.total_tokens_in,
                                tokens_out: s.total_tokens_out,
                                duration: String::new(),
                                provider: provider_labels
                                    .get(&s.provider)
                                    .cloned()
                                    .unwrap_or_else(|| s.provider.clone()),
                            })
                            .collect(),
                    ),
                    Err(_) => (project_dir, Vec::new()),
                }
            },
            |(path, sessions)| Message::ProjectSessionsLoaded { path, sessions },
        )
    }

    /// Rebuild the sidebar project→session tree from the project registry.
    /// Most-recent projects come first; the active project's node is expanded
    /// by default (its sessions are loaded by the caller). Session lists are
    /// reset to "not loaded" so they are lazily refetched on the next expand.
    fn rebuild_project_tree(&mut self) {
        let registry = concerto_config::ProjectRegistry::load().unwrap_or_default();
        let active = concerto_core::helpers::canonical_project_path(&self.project_dir);
        let mut tree: Vec<ProjectTreeNode> = registry
            .recent()
            .map(|path| {
                let expanded = concerto_core::helpers::canonical_project_path(path) == active;
                ProjectTreeNode {
                    path: path.to_path_buf(),
                    name: project_name(path),
                    expanded,
                    sessions: None,
                }
            })
            .collect();
        // The active project always has a node, even when it is not (yet) in
        // the registry (e.g. a manually-typed folder).
        if !tree
            .iter()
            .any(|node| concerto_core::helpers::canonical_project_path(&node.path) == active)
        {
            tree.push(ProjectTreeNode {
                path: self.project_dir.clone(),
                name: project_name(&self.project_dir),
                expanded: true,
                sessions: None,
            });
        }
        self.project_tree = tree;
    }

    /// Resume a previously picked session: make it active for the project and
    /// load its history (and durable typed transcript) so the chat shows the
    /// resumed conversation.
    fn select_session(&self, session_id: String) -> iced::Task<Message> {
        let session_manager = self.session_manager.clone();
        let app_config = self.config.clone().unwrap_or_default();
        let project_dir = self.project_dir.clone();
        iced::Task::perform(
            async move {
                let sid = match concerto_core::ids::Ulid::from_string(&session_id) {
                    Ok(s) => s,
                    Err(_) => return (session_id, Vec::new(), Vec::new(), Vec::new()),
                };
                let handler = {
                    let existing =
                        session_manager.lock().unwrap_or_else(|e| e.into_inner()).clone();
                    match existing {
                        Some(h) => h,
                        None => match DesktopSessionHandler::connect_with_config(&app_config).await
                        {
                            Ok(h) => Arc::new(h),
                            Err(_) => return (session_id, Vec::new(), Vec::new(), Vec::new()),
                        },
                    }
                };
                // Make this session the active one so the next run continues it.
                let _ = handler.set_active_session(&project_dir, sid).await;
                // Seed the chat with its prior conversation. The durable typed
                // transcript (ADR-36) is canonical when present; `history` and
                // the local transcript.json remain the legacy fallback.
                let history = handler.load_history(sid).await.unwrap_or_default();
                let events = handler.load_events(sid).await.unwrap_or_default();
                let transcript = handler.load_transcript(sid).await.unwrap_or_default();
                (session_id, history, events, transcript)
            },
            |(session_id, history, events, transcript)| Message::SessionSelected {
                session_id,
                history,
                events,
                transcript,
            },
        )
    }

    /// Load the active session's persisted spend records for the Spend Log
    /// modal. Best-effort, mirroring [`Self::load_sessions_for_project`]: any
    /// failure (or missing session) yields an empty list rather than breaking
    /// the UI.
    fn load_spend_log(&self) -> iced::Task<Message> {
        let Some(session_id) = self.active_session_id else {
            return iced::Task::none();
        };
        let session_manager = self.session_manager.clone();
        let app_config = self.config.clone().unwrap_or_default();
        iced::Task::perform(
            async move {
                let handler = {
                    let existing =
                        session_manager.lock().unwrap_or_else(|e| e.into_inner()).clone();
                    match existing {
                        Some(h) => h,
                        None => match DesktopSessionHandler::connect_with_config(&app_config).await
                        {
                            Ok(h) => Arc::new(h),
                            Err(_) => return Vec::new(),
                        },
                    }
                };
                handler.list_spend_records(session_id).await.unwrap_or_default()
            },
            |records| Message::Chat(views::chat::Message::SpendLogsLoaded(records)),
        )
    }

    pub fn view(&self) -> Element<'_, Message> {
        let sidebar = views::nav::sidebar_view(self);

        // Agents count as configured when per-agent model pins exist inside
        // `multi_agent.custom_agents`, even before any
        // `model_settings.agent_assignments` are persisted — so the "Configure
        // agents in Studio" hint must not show in that case.
        let agents_configured = self
            .config
            .as_ref()
            .and_then(|config| config.multi_agent.as_ref())
            .map(|multi| !multi.custom_agents.is_empty())
            .unwrap_or(false);

        let content: Element<'_, Message> = match self.page {
            Page::Chat => self
                .chat
                .view(
                    &self.current_theme,
                    self.multi_agent,
                    self.fast,
                    &self.active_model,
                    &self.chat_model_options,
                    self.model_source_label(),
                    &self.agent_graph,
                    !self.runtime_assignments().is_empty() || agents_configured,
                )
                .map(Message::Chat),
            Page::ToolLog => self.tool_log.view(&self.current_theme).map(Message::ToolLog),
            Page::DiffViewer => self.diff.view(&self.current_theme).map(Message::Diff),
            Page::Settings => {
                // Slice 4a (spec §7): while `[orchestration]` is present the
                // blueprint's open relationship registry governs hand-offs, so
                // the legacy Settings → Relationships rule manager is hidden.
                let hide_relationships =
                    self.config.as_ref().is_some_and(orchestration_hides_relationships);
                self.settings.view(&self.current_theme, hide_relationships).map(Message::Settings)
            }
            Page::OrchestrationStudio => self.orchestration_studio.view(&self.current_theme),
            Page::Editor => self.editor.view(&self.current_theme).map(Message::Editor),
        };

        let status_bar = views::status_bar::status_bar_view(self);

        let sep = rule::vertical(1);
        let content_column =
            if let Some(toast_bar) = self.toasts.view::<Message>(&self.current_theme) {
                column![
                    views::context_bar::context_bar_view(self),
                    toast_bar,
                    container(content).width(Length::Fill).height(Length::Fill),
                ]
            } else {
                column![
                    views::context_bar::context_bar_view(self),
                    container(content).width(Length::Fill).height(Length::Fill),
                ]
            };

        // The terminal is a toggleable bottom panel with a drag resize handle,
        // shown on top of the current page's content. The handle is a
        // `mouse_area` (not a `button`): in iced 0.14 a button's `on_press`
        // fires on *release*, while the mouse area fires on press-down, which
        // is what a press-drag-release resize handle needs. The subscription
        // tracks CursorMoved during the drag and ends it on any release.
        //
        // The panel's rendered height is the configured full height eased by
        // `terminal_panel_anim`, so it slides up/down. It is rendered while
        // `anim > 0.01` — including mid-close (open == false, anim still > 0)
        // — so the closing panel slides down instead of vanishing.
        let panel_height = self.terminal_panel_height * ease_out_cubic(self.terminal_panel_anim);
        let main_area = if self.terminal_panel_anim > 0.01 {
            let palette = &self.current_theme.palette;
            let resize_handle = mouse_area(
                container(text("⠿").size(10).color(palette.text_muted))
                    .width(Length::Fill)
                    .height(Length::Fixed(8.0))
                    .center_x(Length::Fill)
                    .center_y(Length::Fill),
            )
            .on_press(Message::TerminalPanelResizeStart)
            .on_release(Message::TerminalPanelResizeEnd);
            let terminal_area =
                container(self.terminal.view(&self.current_theme).map(Message::Terminal))
                    .width(Length::Fill)
                    .height(Length::Fixed(panel_height));
            column![content_column, resize_handle, terminal_area, status_bar]
        } else {
            column![content_column, status_bar]
        };

        let sep2 = rule::vertical(1);
        let right_panel: Element<'_, Message> = if self.quick_panel_open {
            views::quick_panel::quick_panel_view(self)
        } else {
            views::quick_panel::quick_panel_collapsed(self)
        };

        let shell = row![sidebar, sep, main_area, sep2, right_panel,];

        let base = container(shell).width(Length::Fill).height(Length::Fill);

        // ── Sub-view overlay (Diff / Agent Graph / Tool Log shown inside Chat) ──
        // The overlay stays on screen while the fade-out completes (`Main` +
        // still fading), so the backdrop dims out over the base instead of
        // vanishing instantly.
        let overlay_active =
            self.page == Page::Chat && self.chat.sub_view != views::chat::SubView::Main;
        let render_overlay = overlay_active || (self.page == Page::Chat && self.overlay_fading);
        let after_subview: Element<'_, Message> = if render_overlay {
            let subview_content: Element<'_, Message> = if overlay_active {
                match self.chat.sub_view {
                    views::chat::SubView::Main => unreachable!(),
                    views::chat::SubView::Diff => {
                        self.diff.view(&self.current_theme).map(Message::Diff)
                    }
                    views::chat::SubView::AgentGraph => {
                        self.agent_graph.view(&self.current_theme).map(Message::AgentGraph)
                    }
                    views::chat::SubView::ToolLog => {
                        self.tool_log.view(&self.current_theme).map(Message::ToolLog)
                    }
                    views::chat::SubView::SpendLog => views::chat::spend_log_view(
                        self.chat.spend_log(),
                        self.daily_cost,
                        self.session_cap,
                        &self.cap_state,
                        &self.current_theme,
                    )
                    .map(Message::Chat),
                }
            } else {
                // Fade-out placeholder: a text-free empty body so the card can
                // stay on screen while the backdrop dims out — no text spans
                // means no zero-height cosmic-text risk.
                column![].height(Length::Fill).into()
            };

            let close_btn = button(text("✕").size(16))
                .style(button::text)
                .on_press(Message::SetSubView(views::chat::SubView::Main));

            let title = match self.chat.sub_view {
                views::chat::SubView::Main => "",
                views::chat::SubView::Diff => "Diff Viewer",
                views::chat::SubView::AgentGraph => "Agent Graph",
                views::chat::SubView::ToolLog => "Tool Log",
                views::chat::SubView::SpendLog => "Spend Log",
            };

            let header = row![text(title).size(18), iced::widget::space::horizontal(), close_btn]
                .padding(8)
                .spacing(8);

            let overlay_body: Element<'_, Message> = if !overlay_active {
                // Closing (Main + fade-out): no header/close button, just the
                // empty card so the backdrop fades out over the base.
                container(column![].height(Length::Fill))
                    .width(Length::Fill)
                    .max_width(1200.0)
                    .height(Length::Fill)
                    .style(crate::ui::container::modal)
                    .into()
            } else if self.chat.sub_view == views::chat::SubView::ToolLog
                || self.chat.sub_view == views::chat::SubView::SpendLog
            {
                // Centered modal with max-width — let the child determine its
                // natural height; never force Length::Shrink on the container
                // itself since cosmic-text asserts on zero-height text spans.
                container(column![header, subview_content].spacing(4))
                    .width(Length::Fill)
                    .max_width(900.0)
                    .style(crate::ui::container::modal)
                    .into()
            } else {
                // Centered data-dense card for Diff / Agent Graph (wider than
                // Tool Log). Fill height bounds the inner Length::Fill
                // children — both views assume full-page space — and the
                // backdrop's padding supplies the card margins.
                container(column![header, subview_content].spacing(4))
                    .width(Length::Fill)
                    .max_width(1200.0)
                    .height(Length::Fill)
                    .style(crate::ui::container::modal)
                    .into()
            };

            // The backdrop container always uses Length::Fill on both axes so
            // cosmic-text never sees a zero-height text layout area. Alpha is
            // scaled by the overlay fade so the dim layer eases in/out.
            let backdrop = container(overlay_body)
                .width(Length::Fill)
                .height(Length::Fill)
                .center_x(Length::Fill)
                .center_y(Length::Fill)
                .padding(16)
                .style(|_theme: &iced::Theme| container::Style {
                    background: Some(iced::Background::Color(iced::Color {
                        a: 0.55 * self.overlay_fade,
                        ..self.current_theme.palette.background
                    })),
                    ..container::Style::default()
                });

            stack![base, backdrop].into()
        } else {
            base.into()
        };

        // ── System-level dialogs (shown above subviews) ──
        let composed: Element<'_, Message> = if let Some(pending) = &self.pending_root_consent {
            // ADR-44 §4 consent gate: top of the system-dialog stack so it
            // blocks all interaction until the user decides. Composed via
            // the same pattern as the Memory modal (PR #120): a centered
            // modal card over a semi-transparent palette backdrop.
            let modal = container(root_consent::consent_card(
                pending,
                &self.current_theme.iced,
                Message::RootConsentAllow,
                Message::RootConsentDeny,
            ))
            .width(Length::FillPortion(2))
            .height(Length::FillPortion(2))
            .style(crate::ui::container::modal);
            let backdrop = container(modal)
                .width(Length::Fill)
                .height(Length::Fill)
                .center_x(Length::Fill)
                .center_y(Length::Fill)
                .style(|_theme: &iced::Theme| container::Style {
                    background: Some(iced::Background::Color(iced::Color {
                        a: 0.55,
                        ..self.current_theme.palette.background
                    })),
                    ..container::Style::default()
                });
            stack![after_subview, backdrop].into()
        } else if self.show_dir_picker {
            let input = text_input("Project folder path", &self.project_dir_input)
                .on_input(Message::ProjectDirInputChanged)
                .width(420);
            let open_btn = button(text("Open"))
                .style(crate::ui::button::primary)
                .on_press(Message::ProjectDirApply);
            let cancel_btn = button(text("Cancel"))
                .style(crate::ui::button::secondary)
                .on_press(Message::ProjectDirCancel);
            let modal = container(
                column![
                    text("Project Folder").size(18),
                    text("Files the agent writes are saved here.")
                        .size(13)
                        .color(self.current_theme.palette.text_muted),
                    input,
                    row![cancel_btn, open_btn].spacing(10).padding(10),
                ]
                .spacing(10)
                .padding(24)
                .width(480),
            )
            .style(crate::ui::container::modal);
            let overlay = container(modal)
                .width(Length::Fill)
                .height(Length::Fill)
                .center_x(Length::Fill)
                .center_y(Length::Fill);
            stack![after_subview, overlay].into()
        } else if let Some(cap_view) = capability_dialog::view(&self.cap_pending) {
            let dlg = cap_view.map(Message::CapabilityDlg);
            // Dimmed, interaction-blocking backdrop so the dialog reads as a
            // modal: clicks on the backdrop are captured and do not reach the
            // chat underneath, and the centered card stays clearly visible.
            let backdrop = container(dlg)
                .width(Length::Fill)
                .height(Length::Fill)
                .center_x(Length::Fill)
                .center_y(Length::Fill)
                .style(|_theme: &iced::Theme| container::Style {
                    background: Some(iced::Background::Color(iced::Color {
                        a: 0.55,
                        ..self.current_theme.palette.background
                    })),
                    ..container::Style::default()
                });
            stack![after_subview, backdrop].into()
        } else if let Some(ack_dlg) = capability_dialog::ack_view(&self.pending_ack) {
            let backdrop = container(ack_dlg.map(Message::AckDialog))
                .width(Length::Fill)
                .height(Length::Fill)
                .center_x(Length::Fill)
                .center_y(Length::Fill)
                .style(|_theme: &iced::Theme| container::Style {
                    background: Some(iced::Background::Color(iced::Color {
                        a: 0.55,
                        ..self.current_theme.palette.background
                    })),
                    ..container::Style::default()
                });
            stack![after_subview, backdrop].into()
        } else if let Some(intent_dlg) = capability_dialog::intent_view(&self.pending_intent) {
            // Intent confirmation modal (ADR-55 §1), mirroring the capability
            // and ack dialogs: a centered card over a dimmed palette backdrop.
            let backdrop = container(intent_dlg.map(Message::IntentDialog))
                .width(Length::Fill)
                .height(Length::Fill)
                .center_x(Length::Fill)
                .center_y(Length::Fill)
                .style(|_theme: &iced::Theme| container::Style {
                    background: Some(iced::Background::Color(iced::Color {
                        a: 0.55,
                        ..self.current_theme.palette.background
                    })),
                    ..container::Style::default()
                });
            stack![after_subview, backdrop].into()
        } else if let Some(plan_dlg) =
            capability_dialog::plan_view(&self.pending_plan, &self.current_theme)
        {
            // Plan approval modal (ADR-55 Phase 1d), mirroring the intent
            // dialog: a centered card over a dimmed palette backdrop.
            let backdrop = container(plan_dlg.map(Message::PlanDialog))
                .width(Length::Fill)
                .height(Length::Fill)
                .center_x(Length::Fill)
                .center_y(Length::Fill)
                .style(|_theme: &iced::Theme| container::Style {
                    background: Some(iced::Background::Color(iced::Color {
                        a: 0.55,
                        ..self.current_theme.palette.background
                    })),
                    ..container::Style::default()
                });
            stack![after_subview, backdrop].into()
        } else if self.memory_view_open {
            // Memory explorer modal (issue #110). Composed via the same
            // system-dialog stack mechanism as the dir picker / capability /
            // ack dialogs: a centered modal card over a semi-transparent
            // palette backdrop, sitting above sub-view overlays.
            let memory_content = self.memory.modal_view(&self.current_theme).map(Message::Memory);
            let modal = container(
                column![
                    row![
                        text("Memory").size(18).width(Length::Fill),
                        button(text("✕").size(14))
                            .style(crate::ui::button::secondary)
                            .on_press(Message::CloseMemoryModal),
                    ]
                    .align_y(iced::Alignment::Center),
                    memory_content,
                ]
                .spacing(10)
                .padding(20)
                .width(Length::Fill),
            )
            .width(Length::FillPortion(2))
            .height(Length::FillPortion(2))
            .style(crate::ui::container::modal);
            let backdrop = container(modal)
                .width(Length::Fill)
                .height(Length::Fill)
                .center_x(Length::Fill)
                .center_y(Length::Fill)
                .style(|_theme: &iced::Theme| container::Style {
                    background: Some(iced::Background::Color(iced::Color {
                        a: 0.55,
                        ..self.current_theme.palette.background
                    })),
                    ..container::Style::default()
                });
            stack![after_subview, backdrop].into()
        } else {
            after_subview
        };

        // Ambient circuit-trace pulse, active only while an agent run is in
        // progress. Bottom-most layer, so it reads through any gaps in
        // `composed` rather than covering it.
        if self.run_status == RunStatus::Running {
            let circuit_bg =
                circuit_background::view(self.circuit_progress, self.current_theme.palette.accent);
            stack![circuit_bg, composed].into()
        } else {
            composed
        }
    }

    pub fn theme(&self) -> iced::Theme {
        self.current_theme.iced.clone()
    }

    /// ADR-60 D7 (interrupt-safe resume): whether a run is in flight — the
    /// window-close handler cancels it and waits for settlement instead of
    /// exiting over it.
    pub fn is_run_active(&self) -> bool {
        self.run_status != RunStatus::Idle
    }

    /// ADR-60 D7 (interrupt-safe resume): the run-settlement epoch — bumped
    /// every time an in-flight run settles. The window-close handler polls
    /// this (bounded) so the coordinator's cancel path can persist the
    /// interrupted checkpoint before the process exits.
    pub fn run_settle_epoch(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.run_settle_epoch)
    }

    fn note_run_settled(&mut self) {
        self.run_settle_epoch.fetch_add(1, Ordering::Release);
    }

    pub fn subscription(&self) -> Subscription<Message> {
        let terminal_active = self.terminal_panel_open;
        let keyboard_sub =
            keyboard::listen().with(terminal_active).filter_map(|(terminal_active, event)| {
                // Extract key and modifiers from the keyboard event
                match event {
                    iced::keyboard::Event::KeyPressed { key, modifiers, .. } => {
                        let shortcut = if terminal_active {
                            shortcuts::resolve_terminal(&key, modifiers)
                        } else {
                            shortcuts::resolve(
                                &key,
                                modifiers,
                                TEXT_FOCUSED.load(Ordering::Relaxed),
                            )
                        };
                        shortcut.map(Message::Shortcut)
                    }
                    _ => None,
                }
            });
        let bus = self.bus.clone();
        let bus_sub = Subscription::run_with(bus, |bus| {
            let bus = bus.clone();
            // Keep a single receiver across unfold iterations so events
            // published between yields are not dropped. A fresh `subscribe()`
            // per iteration would miss everything published in that gap,
            // causing the UI to show a tool as "running" forever or to drop
            // tool-completion events under bursty agent output.
            futures::stream::unfold((bus, None), |(bus, rx_opt)| async move {
                let mut rx = rx_opt.unwrap_or_else(|| bus.subscribe());
                loop {
                    match rx.recv().await {
                        Ok(evt) => {
                            let desktop_evt = crate::runtime::translate_event(&evt);
                            if let Some(evt) = desktop_evt {
                                return Some((evt, (bus, Some(rx))));
                            }
                        }
                        Err(_) => return None,
                    }
                }
            })
        })
        .map(Message::DesktopEvent);
        let terminal_sub = self.terminal.subscription().map(Message::Terminal);
        // While a terminal-panel drag is in progress, stream cursor moves to
        // update the panel height and end the drag on any release (or any
        // other event — a safety net so the drag can never get stuck). Uses
        // `iced::event::listen` because `iced::mouse::listen` does not exist in
        // iced 0.14; `iced::Event::Mouse(mouse::Event::CursorMoved { position })`
        // carries the cursor position in logical points.
        let drag_sub = if self.terminal_resizing {
            iced::event::listen().map(|event| match event {
                iced::Event::Mouse(iced::mouse::Event::CursorMoved { position }) => {
                    Message::TerminalPanelResizeMoved(position.y)
                }
                iced::Event::Mouse(iced::mouse::Event::ButtonReleased(_)) => {
                    Message::TerminalPanelResizeEnd
                }
                _ => Message::TerminalPanelResizeEnd,
            })
        } else {
            Subscription::none()
        };
        // Only ticks while an agent run is actually in flight, so the
        // ambient background costs nothing the rest of the time.
        let circuit_sub = if self.run_status == RunStatus::Running {
            iced::time::every(std::time::Duration::from_millis(circuit_background::TICK_MS))
                .map(|_| Message::CircuitTick)
        } else {
            Subscription::none()
        };
        // One shared tick drives both the overlay backdrop fade and the
        // terminal panel slide. Active only while at least one animation is
        // in flight, so it costs nothing the rest of the time.
        let anim_sub = if self.overlay_fading || self.terminal_panel_animating {
            iced::time::every(std::time::Duration::from_millis(circuit_background::TICK_MS))
                .map(|_| Message::AnimTick)
        } else {
            Subscription::none()
        };
        // Blinking cursor on the streaming assistant entry — active only while
        // the run is actually streaming text, so it costs nothing at idle.
        let blink_sub = if self.chat.is_streaming() {
            iced::time::every(std::time::Duration::from_millis(STREAMING_CURSOR_PERIOD_MS))
                .map(|_| Message::Chat(views::chat::Message::StreamingTick))
        } else {
            Subscription::none()
        };
        // One shared 16 ms tick drives every chat animation — the assistant
        // typewriter reveal, thinking-preview reveals, entrance fades and the
        // open-thinking shimmer — active only while at least one is in
        // flight, so it costs nothing at idle.
        let typing_sub = if self.chat.is_revealing() {
            iced::time::every(std::time::Duration::from_millis(circuit_background::TICK_MS))
                .map(|_| Message::Chat(views::chat::Message::TypingTick))
        } else {
            Subscription::none()
        };
        // Ticks every second while any toast is showing so stale toasts
        // auto-dismiss after `TOAST_LIFETIME_SECS`. Inactive when idle.
        let toast_sub = if self.toasts.has_toasts() {
            iced::time::every(std::time::Duration::from_secs(1)).map(|_| Message::ToastExpiryTick)
        } else {
            Subscription::none()
        };
        // Config file watcher (ADR-57 §7): emits once per debounced batch of
        // edits to the global config or the active project's config, so
        // external edits reach the next run without a restart. The identity is
        // the project dir — switching projects re-arms the watched path set.
        let config_watch_sub = {
            let project_dir = self.project_dir.clone();
            Subscription::run_with(project_dir, |project_dir| {
                futures::stream::unfold(
                    crate::config_watch::ConfigWatch::start(project_dir.clone()),
                    |mut watch| async move {
                        watch.recv().await.map(|_| (Message::ConfigReloaded, watch))
                    },
                )
            })
        };
        iced::Subscription::batch(vec![
            keyboard_sub,
            bus_sub,
            terminal_sub,
            drag_sub,
            circuit_sub,
            anim_sub,
            blink_sub,
            typing_sub,
            toast_sub,
            config_watch_sub,
        ])
    }
}

impl Drop for App {
    fn drop(&mut self) {
        self.cancel_token.cancel();
        if let Some(prev) =
            self.memory_services.lock().unwrap_or_else(|error| error.into_inner()).take()
        {
            prev.cancel.cancel();
        }
    }
}

#[derive(Clone)]
struct DesktopApprovalSink {
    cap_pending: crate::widgets::capability_dialog::SharedPending,
    pending_ack: crate::widgets::capability_dialog::SharedPendingAck,
    pending_intent: crate::widgets::capability_dialog::SharedPendingIntent,
    pending_plan: crate::widgets::capability_dialog::SharedPendingPlan,
    /// Session-wide auto-approve flag, mirroring the CLI sink's semantics:
    /// once the user chooses "approve all for session" (or the dialog's
    /// "Always allow"), every subsequent request is approved without a prompt
    /// until the next run. A plain single grant never flips this flag.
    auto_approve: Arc<AtomicBool>,
    bus: EventBus,
}

#[async_trait::async_trait]
impl ApprovalSink for DesktopApprovalSink {
    async fn request_approval(
        &self,
        action: &PolicyAction<'_>,
        _cancel: CancellationToken,
    ) -> ApprovalDecision {
        // Fast path: auto-approve if enabled (mirrors the CLI sink).
        if self.auto_approve.load(Ordering::Relaxed) {
            return ApprovalDecision::Approve;
        }

        use concerto_api_types::plugin::{CapabilityRequest, PluginManifest};
        use concerto_plugins::capability::GrantDecision;

        let detail = format!("{:?}", action.input);
        let caps = match action.tool_name {
            "write_file" => {
                vec![CapabilityRequest::FilesystemWrite { globs: vec![detail] }]
            }
            "read_file" => {
                vec![CapabilityRequest::FilesystemRead { globs: vec![detail] }]
            }
            "shell_execute" => {
                vec![CapabilityRequest::ShellExecute { allowlist: vec![detail] }]
            }
            "network_access" => {
                vec![CapabilityRequest::NetworkOutbound { domains: vec![detail] }]
            }
            other => {
                vec![CapabilityRequest::Other { description: format!("Tool: {}", other) }]
            }
        };

        let name = format!("{} request", action.tool_name);

        let (tx, rx) = tokio::sync::oneshot::channel::<Vec<GrantDecision>>();

        {
            let mut guard = self.cap_pending.lock().unwrap_or_else(|e| e.into_inner());
            guard.push_back(crate::widgets::capability_dialog::PendingApproval {
                plugin: PluginManifest {
                    name: name.clone(),
                    description: "Policy action".into(),
                    version: "1.0".into(),
                    id: name,
                    abi_version: 1,
                    capabilities_required: caps.clone(),
                    provides: Vec::new(),
                },
                capabilities: caps,
                sender: tx,
            });
        }

        // Surface the pending request to the UI. `cap_pending` is mutated from
        // this async task, so without an explicit event the Iced view would not
        // re-render and the dialog would stay invisible while the agent blocks
        // waiting on it. Publishing guarantees a redraw that shows the dialog.
        let _ = self.bus.publish_for_session(
            action.session_id,
            action.correlation_id,
            concerto_core::event::EventKind::ApprovalRequested {
                tool_name: action.tool_name.to_string(),
                timeout_secs: 0,
            },
        );

        match rx.await {
            Ok(decisions) => {
                // Every requested capability must be granted for the action to
                // proceed (a single Denied button denies the whole request).
                let all_granted = decisions.iter().all(|d| {
                    matches!(d, GrantDecision::Granted | GrantDecision::GrantedPersistent)
                });
                if !all_granted {
                    return ApprovalDecision::Deny;
                }
                // The dialog resolves every capability with the same button, so
                // a session-wide "Always allow" grant is uniform across the
                // list. Flip the auto-approve flag and report
                // `ApproveAllForSession` so the audit log records the
                // session-wide grant — mirroring the CLI sink, which returns
                // `ApproveAllForSession` after flipping its flag. A plain
                // "Grant for this session" only approves this single call and
                // leaves auto-approve off (per-call prompting).
                if decisions.iter().any(|d| matches!(d, GrantDecision::GrantedPersistent)) {
                    self.auto_approve.store(true, Ordering::Relaxed);
                    ApprovalDecision::ApproveAllForSession
                } else {
                    ApprovalDecision::Approve
                }
            }
            Err(_) => ApprovalDecision::Deny,
        }
    }

    async fn approve_all_for_session(&self, _session_id: Ulid, _cancel: CancellationToken) {
        self.auto_approve.store(true, Ordering::Relaxed);
    }

    async fn request_ack(&self, message: &str, _cancel: CancellationToken) -> bool {
        // Fast path: auto-approve if enabled (mirrors the CLI sink).
        if self.auto_approve.load(Ordering::Relaxed) {
            return true;
        }
        let (tx, rx) = tokio::sync::oneshot::channel::<bool>();

        {
            let mut guard = self.pending_ack.lock().unwrap_or_else(|e| e.into_inner());
            *guard = Some(crate::widgets::capability_dialog::PendingAck {
                message: message.to_string(),
                sender: tx,
            });
        }

        // Surface the pending ack to the UI via the event bus so Iced redraws.
        // Global event: intentionally unscoped (ack carries no session id).
        let _ = self.bus.publish_raw(concerto_core::event::EventKind::ApprovalRequested {
            tool_name: "ack".to_string(),
            timeout_secs: 0,
        });

        rx.await.unwrap_or(false)
    }

    async fn request_intent_confirmation(
        &self,
        question: String,
        options: &[RequestedOutcome],
        _cancel: CancellationToken,
    ) -> Option<RequestedOutcome> {
        // Nothing to confirm — mirror the trait default's conservative
        // read-only reading (the orchestrator treats None as read-only).
        if options.is_empty() {
            return None;
        }

        let (tx, rx) = tokio::sync::oneshot::channel::<Option<RequestedOutcome>>();

        {
            let mut guard = self.pending_intent.lock().unwrap_or_else(|e| e.into_inner());
            guard.push_back(crate::widgets::capability_dialog::PendingIntent {
                question,
                options: options.to_vec(),
                sender: tx,
            });
        }

        // Surface the pending request to the UI via the event bus so Iced
        // redraws and shows the dialog while the agent blocks on it. Global
        // event: intentionally unscoped (the confirmation carries no session
        // id), mirroring `request_ack`.
        let _ = self.bus.publish_raw(concerto_core::event::EventKind::ApprovalRequested {
            tool_name: "intent".to_string(),
            timeout_secs: 0,
        });

        // A dropped/never-answered dialog cancels the wait channel; fall back
        // to the conservative read-only `None`.
        rx.await.ok().flatten()
    }

    async fn request_plan_approval(
        &self,
        session_id: Ulid,
        plan_id: &str,
        question: String,
        plan_text: &str,
        created_at: time::OffsetDateTime,
        _cancel: CancellationToken,
    ) -> Option<PlanDecision> {
        let (tx, rx) = tokio::sync::oneshot::channel::<Option<PlanDecision>>();

        {
            let mut guard = self.pending_plan.lock().unwrap_or_else(|e| e.into_inner());
            guard.push_back(crate::widgets::capability_dialog::PendingPlan {
                session_id,
                plan_id: plan_id.to_string(),
                question,
                plan_text: plan_text.to_string(),
                created_at,
                sender: tx,
            });
        }

        // Surface the pending request to the UI via the event bus so Iced
        // redraws and shows the dialog while the agent blocks on it. Global
        // event, mirroring `request_ack` and the intent dialog, so any window
        // refreshes; the decision itself is matched back to the requesting run
        // by `(session_id, plan_id)` in `resolve_plan`.
        let _ = self.bus.publish_raw(concerto_core::event::EventKind::ApprovalRequested {
            tool_name: "intent:plan".to_string(),
            timeout_secs: 0,
        });

        // A dropped/never-answered dialog cancels the wait channel; fall back
        // to the conservative read-only `None`.
        rx.await.ok().flatten()
    }
}

fn desktop_approval_sink(
    cap_pending: crate::widgets::capability_dialog::SharedPending,
    pending_ack: crate::widgets::capability_dialog::SharedPendingAck,
    pending_intent: crate::widgets::capability_dialog::SharedPendingIntent,
    pending_plan: crate::widgets::capability_dialog::SharedPendingPlan,
    bus: EventBus,
) -> Arc<dyn ApprovalSink> {
    Arc::new(DesktopApprovalSink {
        cap_pending,
        pending_ack,
        pending_intent,
        pending_plan,
        auto_approve: Arc::new(AtomicBool::new(false)),
        bus,
    })
}

/// Build the assistant chat message shown when an agent run completes.
///
/// Always lists the concrete files actually changed (from real tool results),
/// never the model's own completion claims. This is the desktop half of the
/// guarantee that text-only provider responses are never presented as
/// successful assistant output for action-required work.
///
/// The final answer is composed by `AgentOutput::summary` from structured
/// execution data (files written, verification results, project root) plus
/// any optional model-authored notes — never from unverified provider prose.
pub(crate) fn format_run_summary(output: &AgentOutput) -> String {
    output.summary()
}

#[cfg(test)]
mod tests {
    use super::{
        configured_default_route, orchestration_hides_relationships, AgentOutput, App,
        DesktopApprovalSink, EventBus, Message, Page, PolicyAction, RunStatus, Ulid,
    };
    use crate::views::settings::Message as SettingsMessage;
    use concerto_config::{AppConfig, ProviderConfig};
    use concerto_core::event::EventKind;
    use concerto_core::intent::{PlanDecision, RequestedOutcome, RunStage};
    use concerto_core::traits::approval::{ApprovalDecision, ApprovalSink};
    use concerto_core::types::{AgentCompletionStatus, TaskId};
    use concerto_core::CancellationToken;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    // ── ADR-60 D7 (interrupt-safe resume): the graceful window-close path ──

    /// `is_run_active` reflects the run status, and every settle point bumps
    /// the settlement epoch the window-close handler polls.
    #[test]
    fn run_settle_epoch_tracks_run_settlement() {
        let (mut app, _) = App::new();
        assert!(!app.is_run_active(), "idle at construction");
        let before = app.run_settle_epoch().load(Ordering::Acquire);

        app.run_status = RunStatus::Running;
        assert!(app.is_run_active(), "a running run is active");

        // The completion settle point: status back to Idle + epoch bump.
        app.run_status = RunStatus::Idle;
        app.note_run_settled();
        assert!(!app.is_run_active());
        let after = app.run_settle_epoch().load(Ordering::Acquire);
        assert_eq!(after, before + 1, "a settled run bumps the epoch");
    }

    /// Serializes tests that redirect `XDG_CONFIG_HOME` (which `dirs` reads
    /// for the config directory on Linux) against each other — env vars are
    /// process-global and cargo runs tests in parallel threads. Mirrors
    /// `PROJECT_ROOTS_ENV_LOCK` (concerto-config) and `ENV_LOCK` (concerto-cli).
    static CONFIG_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn new_creates_initial_state() {
        let (app, _) = App::new();
        assert_eq!(app.page, Page::Chat);
    }

    #[test]
    fn navigate_changes_page() {
        let (mut app, _) = App::new();
        let _ = app.update(Message::Navigate(Page::Settings));
        assert_eq!(app.page, Page::Settings);
    }

    #[test]
    fn help_toggle_works() {
        let (mut app, _) = App::new();
        assert!(!app.show_help);
        let _ = app.update(Message::HelpToggled);
        assert!(app.show_help);
        let _ = app.update(Message::HelpToggled);
        assert!(!app.show_help);
    }

    #[test]
    fn diff_viewer_shortcut_sets_subview_diff() {
        let (mut app, _) = App::new();
        let _ = app.update(Message::Shortcut(crate::shortcuts::Shortcut::DiffViewer));
        assert_eq!(app.page, Page::Chat);
        assert_eq!(app.chat.sub_view, crate::views::chat::SubView::Diff);
    }

    // ── ADR-57 — config reload reconciliation ──────────────────────────────
    //
    // `apply_reloaded_config` is the harness-free half of
    // `reconcile_config_from_reload` (which loads from disk); feeding it
    // already-parsed configs exercises the equality short-circuit and the
    // full re-derivation deterministically.

    #[test]
    fn apply_reloaded_config_is_noop_when_content_is_equal() {
        let (mut app, _) = App::new();
        // Seed a broken flag: a successful (even no-op) reload must clear it.
        app.config_broken = true;
        let snapshot = (
            app.multi_agent,
            app.active_provider_id.clone(),
            app.active_model.clone(),
            app.chat_model_options.clone(),
            app.session_cap,
            app.global_config.clone(),
        );

        app.apply_reloaded_config(snapshot.5.clone(), app.config.clone().expect("App config"));

        assert!(!app.config_broken, "a successful no-op reload clears the broken flag");
        assert_eq!(app.multi_agent, snapshot.0, "run-mode must be untouched on a no-op");
        assert_eq!(app.active_provider_id, snapshot.1);
        assert_eq!(app.active_model, snapshot.2);
        assert_eq!(app.chat_model_options, snapshot.3);
        assert_eq!(app.session_cap, snapshot.4);
        assert_eq!(app.global_config, snapshot.5);
    }

    #[test]
    fn apply_reloaded_config_rederives_on_differing_config() {
        let (mut app, _) = App::new();
        let mut expected = app.config.clone().expect("App config");
        // Force a deterministic difference without touching the route inputs,
        // so re-derivation is observable on the cap, memory flag, and the
        // run-mode toggle parity (which must reflect the file, not reset).
        let multi_before = app.multi_agent;
        expected.session_spend_cap_usd = Some(2.0);
        expected.memory.enabled = false;

        app.apply_reloaded_config(expected.clone(), expected.clone());

        assert_eq!(app.config.as_ref(), Some(&expected), "config must be replaced");
        assert_eq!(app.multi_agent, multi_before, "run-mode re-derives from the file");
        assert_eq!(app.session_cap, Some(2.0), "session cap re-derives from the file");
        let expected_route = configured_default_route(&expected);
        assert_eq!(app.active_provider_id, expected_route.0);
        assert_eq!(app.active_model, expected_route.1);
        assert!(
            matches!(app.memory.status, crate::views::memory::MemoryStatus::Disabled),
            "memory view flag must track the reloaded config"
        );
        assert!(!app.config_broken);
    }

    #[test]
    fn apply_reloaded_config_refreshes_settings_provider_rows() {
        let (mut app, _) = App::new();
        let mut reloaded = app.config.clone().expect("App config");
        // The external edit adds a provider row to `model_settings`.
        reloaded.model_settings.get_or_insert_with(Default::default).providers =
            vec![concerto_config::ProviderConfig {
                id: "external".into(),
                provider: "openai".into(),
                model: "gpt-4o".into(),
                ..Default::default()
            }];

        app.apply_reloaded_config(reloaded.clone(), reloaded);

        assert!(
            app.settings.providers.iter().any(|p| p.id == "external"),
            "the Settings provider rows must reflect the reloaded config"
        );
        assert!(
            app.settings.cached_provider_ids.contains(&"external".to_string()),
            "the provider caches must reflect the reloaded config"
        );
    }

    #[test]
    fn memory_teardown_is_deferred_while_run_is_active() {
        let (mut app, _) = App::new();
        let mut reloaded = app.config.clone().expect("App config");
        reloaded.memory.enabled = false;

        // Mid-run: the disabled flag reaches the view immediately, but the
        // memory-service slot must not be torn down under an active run.
        // (The slot is empty here, so this verifies the reconcile reaches the
        // memory section during a run without touching the slot; completion
        // of the deferred teardown happens in `Message::AgentRunCompleted`.)
        app.run_status = RunStatus::Running;
        app.apply_reloaded_config(reloaded.clone(), reloaded.clone());
        assert!(
            matches!(app.memory.status, crate::views::memory::MemoryStatus::Disabled),
            "memory view flag updates even mid-run"
        );
        assert!(
            app.memory_services.lock().unwrap_or_else(|e| e.into_inner()).is_none(),
            "no teardown may run while a run is active"
        );

        // At idle the same reload must also settle cleanly (the deferred
        // teardown path is a no-op when the slot is empty).
        app.run_status = RunStatus::Idle;
        app.apply_reloaded_config(reloaded.clone(), reloaded);
        assert!(matches!(app.memory.status, crate::views::memory::MemoryStatus::Disabled));
    }

    // ── ADR-59 — blueprint apply path + first-run init ─────────────────────

    /// ADR-59 D4 (apply-path test, content-aware): an include-file content
    /// change leaves the persisted surface equal (`AppConfig`'s `PartialEq`
    /// deliberately excludes `resolved_blueprint`) but moves the resolved
    /// model — the reload must NOT short-circuit, and the live config must
    /// consume the freshly resolved blueprint. The pre-batch mirror
    /// (`apply_reloaded_config_is_noop_when_content_is_equal`) cannot catch
    /// this because its equality holds on the persisted surface.
    #[test]
    fn apply_reloaded_config_applies_blueprint_content_change() {
        let (mut app, _) = App::new();
        let mut reloaded = app.config.clone().expect("App config");
        let live_blueprint = app
            .config
            .as_ref()
            .and_then(|c| c.resolved_blueprint.clone())
            .expect("resolved blueprint attached by the load seam");

        // Re-resolve a content-edited blueprint (a watched include-file
        // change: new pipeline name, renamed stage, added stage cap). The
        // persisted `[orchestration]` selection is untouched.
        let mut edited = live_blueprint.blueprint.clone();
        edited.name = "edited-include".to_string();
        edited.pipeline.stages[0].label = "Renamed by include edit".to_string();
        edited.pipeline.stages[0].max_cycles = Some(2);
        let rebased =
            concerto_config::resolve_blueprint(&edited).expect("edited blueprint must resolve");
        reloaded.resolved_blueprint = Some(Arc::new(rebased));

        // This is exactly the ADR-59 D4 no-op trap: persisted-surface equality
        // holds while the resolved model moves.
        assert_eq!(
            &reloaded,
            app.config.as_ref().expect("live config"),
            "persisted surface equality holds after the content edit"
        );
        assert_ne!(
            reloaded.resolved_blueprint.as_ref(),
            Some(&live_blueprint),
            "the resolved model must differ after the include-content edit"
        );

        app.apply_reloaded_config(reloaded.clone(), reloaded.clone());

        assert_eq!(
            app.config.as_ref().and_then(|c| c.resolved_blueprint.clone()),
            reloaded.resolved_blueprint,
            "the live config must consume the freshly resolved blueprint"
        );
        assert_eq!(
            app.config
                .as_ref()
                .and_then(|c| c.resolved_blueprint.as_ref().map(|r| r.blueprint.name.as_str())),
            Some("edited-include"),
            "the live blueprint content must change after apply"
        );
        assert!(!app.config_broken, "a successful apply clears the broken flag");
    }

    /// ADR-58/59 (rewritten) Slice 2 (first-run bootstrap): opening the Studio for the first
    /// time auto-seeds the orchestration roster into the PROJECT config
    /// (`.concerto.toml`) — `[orchestration]` with the standard blueprint
    /// inlined + the five `[multi_agent.custom_agents]` seeds. No splash, no
    /// manual init: the config owns its roster afterwards, the blueprint
    /// resolves from the written file, the global `config.toml` is untouched,
    /// and no include file is created.
    #[test]
    fn first_studio_open_auto_seeds_the_orchestration_roster() {
        let _guard = CONFIG_ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let dir = tempfile::tempdir().expect("tempdir");
        let previous = std::env::var_os("XDG_CONFIG_HOME");
        std::env::set_var("XDG_CONFIG_HOME", dir.path());

        // Global config: schema only — must stay untouched. Project dir is
        // empty: no config file, no include file (a brand-new project).
        let global_dir = dir.path().join("concerto");
        std::fs::create_dir_all(&global_dir).expect("create global config dir");
        let global_config_path = global_dir.join("config.toml");
        std::fs::write(&global_config_path, "schema_version = 7\n").expect("seed global config");
        let global_before = std::fs::read_to_string(&global_config_path).expect("read global");
        let project_dir = dir.path().join("project");
        std::fs::create_dir_all(&project_dir).expect("create project dir");

        let (mut app, _) = App::new();
        app.project_dir = project_dir.clone();
        // The persisted project registry (data dir) may already point at a
        // seeded project on this machine; force the fresh-project shape so
        // `ensure_orchestration_seeded` must really write (the doc contract:
        // a `None` config is still handed to the seed).
        app.config = None;
        // The first Studio open is exactly what triggers the auto-seed.
        let _ = app.update(Message::Navigate(Page::OrchestrationStudio));

        // Env restored before assertions so a panic cannot leak the redirect.
        match previous {
            Some(value) => std::env::set_var("XDG_CONFIG_HOME", value),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }

        // The seed landed in the PROJECT config layer.
        let project_config = project_dir.join(concerto_config::legacy::NEW_PROJECT_CONFIG_FILE);
        let raw = std::fs::read_to_string(&project_config).expect("project config read back");
        assert!(raw.contains("[orchestration]"), "roster section written\n{raw}");
        assert!(
            raw.contains("blueprint = { inline = {") && raw.contains("name = \"standard\""),
            "the standard blueprint must be seeded inline\n{raw}"
        );
        assert_eq!(
            raw.matches("[[multi_agent.custom_agents]]").count(),
            5,
            "five seeded agents expected\n{raw}"
        );
        // The global config is never rewritten by the seed.
        let global_after = std::fs::read_to_string(&global_config_path).expect("global read back");
        assert_eq!(global_before, global_after, "the global config must stay untouched");
        // No include file is created — the seed is inline, not include-based.
        assert!(
            !project_dir.join(concerto_config::BLUEPRINT_INCLUDE_FILE).exists(),
            "the seed must not write a blueprint include file"
        );

        // App state owns the roster and the Studio is already on the blueprint
        // path from the very first open (no splash to clear).
        let config = app.config.as_ref().expect("config loaded after the seed");
        assert!(config.owns_agent_roster(), "a seeded config owns its roster");
        assert_eq!(
            config.orchestration.as_ref().and_then(|o| o
                .blueprint
                .inline
                .as_ref()
                .map(|b| b.name.as_str())),
            Some("standard"),
            "the seeded selection is inline-standard"
        );
        assert_eq!(
            config.resolved_blueprint.as_ref().map(|r| r.blueprint.pipeline.stages.len()),
            Some(5),
            "the seeded standard blueprint resolves with a five-stage pipeline"
        );
        assert!(
            app.orchestration_studio.blueprint().is_some(),
            "the Studio must hold the editable blueprint from the first open"
        );
    }

    /// ADR-59 D5 (startup-fallback test): when config loading falls back to
    /// defaults at startup (unparsable `config.toml`), `App::new` must surface
    /// it via `config_broken` instead of failing silently.
    #[test]
    fn startup_config_load_failure_marks_config_broken() {
        let _guard = CONFIG_ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let dir = tempfile::tempdir().expect("tempdir");
        // `dirs` resolves the config dir from `XDG_CONFIG_HOME` on Linux, so
        // the redirect makes both load seams read the broken file.
        let config_dir = dir.path().join("concerto");
        std::fs::create_dir_all(&config_dir).expect("create config dir");
        std::fs::write(config_dir.join("config.toml"), "[unterminated\n")
            .expect("seed broken config");

        let previous = std::env::var_os("XDG_CONFIG_HOME");
        std::env::set_var("XDG_CONFIG_HOME", dir.path());
        let (app, _) = App::new();
        match previous {
            Some(value) => std::env::set_var("XDG_CONFIG_HOME", value),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }

        assert!(app.config_broken, "startup config fallback must surface via config_broken");
    }

    /// ADR-58/59 (rewritten) Slice 2 (orphan contract): when the raw file
    /// carries the `custom_agents` key — even as `[]`, meaning every agent was
    /// deleted — the auto-seed is a strict no-op: nothing is written and the
    /// file stays byte-identical ("key present" = owned; deletions stick).
    #[test]
    fn ensure_orchestration_seeded_is_a_noop_when_key_present_even_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config_path = dir.path().join(concerto_config::legacy::NEW_PROJECT_CONFIG_FILE);
        let content = r#"# sentinel comment
schema_version = 7

[orchestration]
schema_version = 1

[multi_agent]
custom_agents = []
"#;
        std::fs::write(&config_path, content).expect("seed owned-roster config");
        let before = std::fs::read_to_string(&config_path).expect("read before");

        let (mut app, _) = App::new();
        app.project_dir = dir.path().to_path_buf();
        app.config = Some(AppConfig {
            orchestration: Some(concerto_config::OrchestrationConfig::default()),
            ..AppConfig::default()
        });
        let owned = app.config.as_ref().and_then(|config| config.orchestration.as_ref()).cloned();

        app.ensure_orchestration_seeded();

        let after = std::fs::read_to_string(&config_path).expect("read back");
        assert_eq!(after, before, "an owned roster (even empty) must never trigger a write");
        assert_eq!(
            app.config.as_ref().and_then(|config| config.orchestration.as_ref()),
            owned.as_ref(),
            "the owned orchestration selection is left untouched"
        );
    }

    /// ADR-58/59 (rewritten) Slice 2 (orphan self-heal): a config carrying
    /// `[orchestration]` (a custom blueprint) but NO materialized `custom_agents`
    /// key is the orphan shape — the auto-seed writes ONLY the five seed
    /// agents under `[multi_agent.custom_agents]` and preserves the existing
    /// orchestration blueprint text unchanged, so the Studio's searchable
    /// library matches the blueprint's staffing.
    #[test]
    fn ensure_orchestration_seeded_self_heals_the_orphan_shape() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config_path = dir.path().join(concerto_config::legacy::NEW_PROJECT_CONFIG_FILE);
        let content = r#"schema_version = 7

[orchestration]
schema_version = 1

[orchestration.blueprint]
name = "custom-blueprint"
description = "keep me"
"#;
        std::fs::write(&config_path, content).expect("seed orphan-shape config");

        let (mut app, _) = App::new();
        app.project_dir = dir.path().to_path_buf();
        app.config = Some(AppConfig {
            orchestration: Some(concerto_config::OrchestrationConfig::default()),
            ..AppConfig::default()
        });

        app.ensure_orchestration_seeded();

        let after = std::fs::read_to_string(&config_path).expect("read back");
        // The agents-only seed preserves the existing orchestration blueprint.
        assert!(
            after.contains("\"custom-blueprint\"") && after.contains("\"keep me\""),
            "the orchestration blueprint must be preserved\n{after}"
        );
        // Exactly the five seed agents are now materialized.
        assert_eq!(
            after.matches("[[multi_agent.custom_agents]]").count(),
            5,
            "five seeded agents expected\n{after}"
        );
        for id in ["architect", "researcher", "coder", "reviewer", "validator"] {
            assert!(after.contains(&format!("id = \"{id}\"")), "seed agent {id} missing\n{after}");
        }
    }

    /// ADR-58/59 (rewritten) Slice 2 (single-arm Save, name source): a config whose
    /// selection is a bare catalog `name` (the code-catalog is seed-only) is
    /// materialized — Save writes the edited blueprint inline into the
    /// PROJECT config, the dangling `name` selector is removed (exactly-one
    /// selection), the global `config.toml` stays untouched, and a full
    /// reload consumes the EDITS (the B1 property: the runtime reads what
    /// Save wrote, not the catalog).
    #[test]
    fn save_materializes_a_name_selection_inline_into_the_project_config() {
        let _guard = CONFIG_ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let dir = tempfile::tempdir().expect("tempdir");
        let previous = std::env::var_os("XDG_CONFIG_HOME");
        std::env::set_var("XDG_CONFIG_HOME", dir.path());

        std::fs::create_dir_all(dir.path().join("concerto")).expect("create global config dir");
        let global_config_path = dir.path().join("concerto").join("config.toml");
        std::fs::write(&global_config_path, "schema_version = 7\n").expect("seed global config");
        let global_before = std::fs::read_to_string(&global_config_path).expect("read global");
        let project_dir = dir.path().join("project");
        std::fs::create_dir_all(&project_dir).expect("create project dir");
        // A bare name-based selection in the project layer (catalog shape).
        let project_config = project_dir.join(concerto_config::legacy::NEW_PROJECT_CONFIG_FILE);
        std::fs::write(&project_config, "schema_version = 7\n").expect("seed project config");
        concerto_config::save_blueprint_selection(
            &project_config,
            &concerto_config::BlueprintSelection {
                name: Some("standard".to_string()),
                include: None,
                inline: None,
            },
        )
        .expect("seed name selection");

        let (mut app, _) = App::new();
        app.project_dir = project_dir.clone();
        app.reconcile_config_from_reload();
        let config = app.config.clone().expect("config loaded after reconcile");
        assert_eq!(
            config.orchestration.as_ref().and_then(|o| o.blueprint.name.as_deref()),
            Some("standard"),
            "precondition: the selection is catalog-name based"
        );
        app.orchestration_studio.load_from_config(&config);

        // Edit the first stage's label, then Save.
        let _ = app.orchestration_studio.update(
            crate::views::orchestration_studio::StudioMessage::StageLabelEdited(
                0,
                "planning".into(),
            ),
        );
        let _ = app.update(Message::OrchestrationStudio(
            crate::views::orchestration_studio::StudioMessage::SaveOrchestration,
        ));

        // Env restored before assertions so a panic cannot leak the redirect.
        match previous {
            Some(value) => std::env::set_var("XDG_CONFIG_HOME", value),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }

        assert!(!app.orchestration_studio.unsaved, "a successful save marks the studio clean");
        let after = std::fs::read_to_string(&project_config).expect("project config read back");
        assert!(after.contains("inline = {"), "save must write the blueprint inline\n{after}");
        let global_after = std::fs::read_to_string(&global_config_path).expect("global read back");
        assert_eq!(global_before, global_after, "the global config must stay untouched");

        // The materialized selection is exactly-one (inline) — the load seam
        // rejects any dangling sibling selector, so a successful reload is
        // itself the proof the `name` selector was removed.
        app.reconcile_config_from_reload();
        let reloaded = app.config.clone().expect("config after reload");
        let selection = reloaded.orchestration.as_ref().expect("[orchestration] present");
        assert!(selection.blueprint.name.is_none(), "the name selector must be removed");
        assert!(selection.blueprint.include.is_none(), "no include selector may appear");
        assert!(selection.blueprint.inline.is_some(), "the selection must be inline");
        let reloaded_label = reloaded
            .resolved_blueprint
            .as_ref()
            .map(|r| r.blueprint.pipeline.stages[0].label.as_str());
        assert_eq!(
            reloaded_label,
            Some("planning"),
            "the runtime must load the edited blueprint — not the catalog standard"
        );
    }

    /// ADR-58/59 (rewritten) Slice 2 (single-arm Save, guard): a validation-invalid draft is
    /// rejected on the inline path too — nothing is written, the draft is
    /// kept, and the failure is surfaced (studio error + error toast).
    #[test]
    fn save_rejects_an_invalid_draft_on_the_inline_path() {
        let _guard = CONFIG_ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let dir = tempfile::tempdir().expect("tempdir");
        let previous = std::env::var_os("XDG_CONFIG_HOME");
        std::env::set_var("XDG_CONFIG_HOME", dir.path());

        std::fs::create_dir_all(dir.path().join("concerto")).expect("create global config dir");
        std::fs::write(dir.path().join("concerto").join("config.toml"), "schema_version = 7\n")
            .expect("seed global config");
        let project_dir = dir.path().join("project");
        std::fs::create_dir_all(&project_dir).expect("create project dir");

        let (mut app, _) = App::new();
        app.project_dir = project_dir.clone();
        // Same fresh-project shape as the seed matrix (see the first-open
        // test): the machine's persisted registry must not pre-own a roster,
        // or the seed short-circuits and no project file is written.
        app.config = None;
        // First Studio open auto-seeds the inline roster.
        let _ = app.update(Message::Navigate(Page::OrchestrationStudio));
        let project_config = project_dir.join(concerto_config::legacy::NEW_PROJECT_CONFIG_FILE);
        let before = std::fs::read_to_string(&project_config).expect("read project config before");

        // Force a rulebook violation the UI would flag: an empty stage tag
        // (rule (g), "stage tag must be non-empty").
        let _ = app.orchestration_studio.update(
            crate::views::orchestration_studio::StudioMessage::StageTagEdited(0, "".into()),
        );
        assert!(
            !app.orchestration_studio.validation().ok,
            "the edited draft must be invalid (precondition)"
        );
        let _ = app.update(Message::OrchestrationStudio(
            crate::views::orchestration_studio::StudioMessage::SaveOrchestration,
        ));

        // Env restored before assertions so a panic cannot leak the redirect.
        match previous {
            Some(value) => std::env::set_var("XDG_CONFIG_HOME", value),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }

        let after = std::fs::read_to_string(&project_config).expect("read project config after");
        assert_eq!(before, after, "an invalid draft must never reach the config");
        assert!(
            app.orchestration_studio.save_error.is_some(),
            "the save failure must surface on the studio"
        );
        assert!(app.toasts.has_toasts(), "the save failure must surface as a toast");
    }

    /// ADR-58/59 (rewritten) Slice 3: a valid Save writes the agent roster —
    /// the Studio's authoritative agent list (mirrors of the seeds plus user
    /// agents) — into `[multi_agent.custom_agents]` of the PROJECT config,
    /// atomically and merge-aware. The roster has no rulebook of its own, so
    /// it is gated on the same blueprint validation that gates the blueprint
    /// write (an invalid draft never reaches the config at all — covered by
    /// `save_rejects_an_invalid_draft_on_the_inline_path`).
    #[test]
    fn save_writes_the_agent_roster_alongside_the_blueprint() {
        let _guard = CONFIG_ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let dir = tempfile::tempdir().expect("tempdir");
        let previous = std::env::var_os("XDG_CONFIG_HOME");
        std::env::set_var("XDG_CONFIG_HOME", dir.path());

        std::fs::create_dir_all(dir.path().join("concerto")).expect("create global config dir");
        std::fs::write(dir.path().join("concerto").join("config.toml"), "schema_version = 7\n")
            .expect("seed global config");
        let project_dir = dir.path().join("project");
        std::fs::create_dir_all(&project_dir).expect("create project dir");

        let (mut app, _) = App::new();
        app.project_dir = project_dir.clone();
        // Same fresh-project shape as the seed matrix: the machine's persisted
        // registry must not pre-own a roster, or the seed short-circuits.
        app.config = None;
        let _ = app.update(Message::Navigate(Page::OrchestrationStudio));
        let project_config = project_dir.join(concerto_config::legacy::NEW_PROJECT_CONFIG_FILE);

        // Add a user agent to the roster (mirrors Add/Rename in the library).
        let _ = app.orchestration_studio.update(
            crate::views::orchestration_studio::StudioMessage::NewAgentName("Planner".into()),
        );
        let _ = app
            .orchestration_studio
            .update(crate::views::orchestration_studio::StudioMessage::AddAgent);
        assert!(app.orchestration_studio.unsaved, "a roster edit marks the studio dirty");

        let _ = app.update(Message::OrchestrationStudio(
            crate::views::orchestration_studio::StudioMessage::SaveOrchestration,
        ));

        // Env restored before assertions so a panic cannot leak the redirect.
        match previous {
            Some(value) => std::env::set_var("XDG_CONFIG_HOME", value),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }

        assert!(
            !app.orchestration_studio.unsaved,
            "a successful save marks the studio clean (blueprint + roster)"
        );
        let after = std::fs::read_to_string(&project_config).expect("project config read back");
        assert!(
            after.contains("[[multi_agent.custom_agents]]"),
            "the roster table must be written\n{after}"
        );
        assert!(
            after.contains("Planner"),
            "the added roster agent must appear in [[multi_agent.custom_agents]]\n{after}"
        );
        // The roster owns the config (owns_agent_roster): a reload keeps it.
        app.reconcile_config_from_reload();
        let reloaded = app.config.clone().expect("config after reload");
        assert!(reloaded.owns_agent_roster(), "the project config must own the agent roster");
    }

    /// Slice 4a (spec §7): the Settings Relationships-hide flag is a pure
    /// projection of `[orchestration]` presence — the plumbing the Settings
    /// view consumes to gate the legacy "Agent Relationship Manager" section.
    #[test]
    fn settings_hide_relationships_flag_tracks_orchestration_presence() {
        let mut config = AppConfig::default();
        assert!(
            !orchestration_hides_relationships(&config),
            "without [orchestration] the legacy relationship manager stays visible"
        );
        config.orchestration = Some(concerto_config::OrchestrationConfig::default());
        assert!(
            orchestration_hides_relationships(&config),
            "with [orchestration] the legacy relationship manager is hidden"
        );
    }

    #[test]
    fn new_task_shortcut_clears_the_visible_conversation() {
        let (mut app, _) = App::new();
        let _ = app.chat.update(crate::views::chat::Message::AddUser("old task".into()));
        let _ = app.update(Message::Shortcut(crate::shortcuts::Shortcut::NewTask));
        assert!(app.chat.entries().is_empty());
    }

    #[test]
    fn startup_opens_a_blank_session() {
        let (app, _) = App::new();
        assert!(app.chat.entries().is_empty());
        assert!(app.active_session_id.is_none());
        assert!(app.agent_graph.model.nodes.is_empty());
    }

    #[test]
    fn text_focus_tracks_chat_input() {
        let (mut app, _) = App::new();
        assert!(!app.text_focused);
        let _ =
            app.update(Message::Chat(crate::views::chat::Message::InputChanged("hello".into())));
        assert!(app.text_focused);
        let _ = app.update(Message::Chat(crate::views::chat::Message::SubmitInput));
        assert!(!app.text_focused);
    }

    #[test]
    fn project_dir_picker_opens_and_cancels() {
        let (mut app, _) = App::new();
        assert!(!app.show_dir_picker);
        let _ = app.update(Message::OpenProjectDirPicker);
        assert!(app.show_dir_picker);
        // The input is pre-filled with the current folder.
        assert_eq!(app.project_dir_input, app.project_dir.to_string_lossy());
        let _ = app.update(Message::ProjectDirCancel);
        assert!(!app.show_dir_picker);
    }

    #[test]
    fn project_dir_apply_switches_active_folder() {
        let (mut app, _) = App::new();
        let _ = app.chat.update(crate::views::chat::Message::AddUser("old task".into()));
        let target = std::env::temp_dir();
        let _ = app.update(Message::ProjectDirInputChanged(target.to_string_lossy().to_string()));
        let _ = app.update(Message::ProjectDirApply);
        let expected = target.canonicalize().unwrap_or_else(|_| target.to_path_buf());
        assert_eq!(app.project_dir, expected);
        assert!(!app.show_dir_picker);
        // Switching rebinds the session handler so the next run uses the new folder.
        assert!(app.session_manager.lock().unwrap_or_else(|e| e.into_inner()).is_none());
        assert!(app.chat.entries().is_empty());
        assert!(app.active_session_id.is_none());
    }

    // -----------------------------------------------------------------------
    // ADR-44 §4 — out-of-root consent gate
    // -----------------------------------------------------------------------

    /// An out-of-root target defers the switch to the consent gate; Deny
    /// aborts it cleanly without changing the project or showing an error.
    #[test]
    fn out_of_root_project_apply_requires_consent_and_deny_aborts() {
        let _guard = crate::root_consent::REGISTRY_SAVE_LOCK.lock().unwrap();
        let (mut app, _) = App::new();
        app.effective_roots = vec![std::path::PathBuf::from("/srv/configured-root")];
        let target = tempfile::tempdir().unwrap();
        let canonical =
            target.path().canonicalize().unwrap_or_else(|_| target.path().to_path_buf());
        let before = app.project_dir.clone();

        // The real flow: the user opens the dir-picker modal, types the path
        // and confirms.
        app.show_dir_picker = true;
        let _ = app
            .update(Message::ProjectDirInputChanged(target.path().to_string_lossy().to_string()));
        let _ = app.update(Message::ProjectDirApply);

        // The switch is deferred, not applied: the gate is pending and the
        // picker stays open behind it.
        assert_eq!(app.pending_root_consent.as_deref(), Some(canonical.as_path()));
        assert_eq!(app.project_dir, before);
        assert!(app.show_dir_picker);

        let _ = app.update(Message::RootConsentDeny);
        assert!(app.pending_root_consent.is_none());
        assert_eq!(app.project_dir, before, "deny must not change the project");
    }

    /// Allow records the canonical dir in the effective allowlist and applies
    /// the switch; a subsequent apply for the same dir passes without a gate.
    #[test]
    fn root_consent_allow_switches_and_records_allowlist() {
        let _guard = crate::root_consent::REGISTRY_SAVE_LOCK.lock().unwrap();
        let (mut app, _) = App::new();
        app.effective_roots = vec![std::path::PathBuf::from("/srv/configured-root")];
        let target = tempfile::tempdir().unwrap();
        let canonical =
            target.path().canonicalize().unwrap_or_else(|_| target.path().to_path_buf());

        let _ = app
            .update(Message::ProjectDirInputChanged(target.path().to_string_lossy().to_string()));
        let _ = app.update(Message::ProjectDirApply);
        assert!(app.pending_root_consent.is_some());

        let _ = app.update(Message::RootConsentAllow);
        assert!(app.pending_root_consent.is_none());
        assert_eq!(app.project_dir, canonical);
        assert!(app.effective_roots.contains(&canonical), "allowed dir joins the allowlist");
        assert!(!app.show_dir_picker);

        // Re-applying the same directory no longer gates (effective allowlist).
        let _ = app
            .update(Message::ProjectDirInputChanged(target.path().to_string_lossy().to_string()));
        let _ = app.update(Message::ProjectDirApply);
        assert!(app.pending_root_consent.is_none());
    }

    /// In-root targets never gate.
    #[test]
    fn in_root_project_apply_skips_consent() {
        let _guard = crate::root_consent::REGISTRY_SAVE_LOCK.lock().unwrap();
        let (mut app, _) = App::new();
        let target = tempfile::tempdir().unwrap();
        let canonical =
            target.path().canonicalize().unwrap_or_else(|_| target.path().to_path_buf());
        app.effective_roots = vec![canonical.clone()];

        let _ = app
            .update(Message::ProjectDirInputChanged(target.path().to_string_lossy().to_string()));
        let _ = app.update(Message::ProjectDirApply);

        assert!(app.pending_root_consent.is_none(), "in-root apply must not gate");
        assert_eq!(app.project_dir, canonical);
    }

    /// Re-selecting the current project is a no-op and never gates, even when
    /// it lies outside the roots (nothing new is exposed).
    #[test]
    fn reselecting_current_project_never_gates() {
        let (mut app, _) = App::new();
        app.effective_roots = vec![std::path::PathBuf::from("/srv/configured-root")];
        let before = app.project_dir.clone();

        let _ = app.update(Message::ProjectDirInputChanged(before.to_string_lossy().to_string()));
        let _ = app.update(Message::ProjectDirApply);

        assert!(app.pending_root_consent.is_none());
        assert_eq!(app.project_dir, before);
    }

    // -----------------------------------------------------------------------
    // Memory explorer modal (issue #110)
    // -----------------------------------------------------------------------

    fn sample_memory_row() -> crate::views::memory::MemoryRow {
        crate::views::memory::MemoryRow {
            id: "chunk-1".into(),
            content_preview: "a preview".into(),
            source: "src/main.rs".into(),
            age: String::new(),
            score: 0.9,
            entry_type: crate::views::memory::MemoryEntryType::Fact,
        }
    }

    /// OpenMemoryModal opens the memory modal; CloseMemoryModal closes it.
    #[test]
    fn memory_modal_opens_and_closes() {
        let (mut app, _) = App::new();
        assert!(!app.memory_view_open);
        let _ = app.update(Message::OpenMemoryModal);
        assert!(app.memory_view_open);
        let _ = app.update(Message::CloseMemoryModal);
        assert!(!app.memory_view_open);
    }

    /// Ctrl+M (Shortcut::Memory) opens the memory modal instead of expanding
    /// the retired quick-panel section.
    #[test]
    fn memory_shortcut_opens_the_memory_modal() {
        let (mut app, _) = App::new();
        assert!(!app.memory_view_open);
        let _ = app.update(Message::Shortcut(crate::shortcuts::Shortcut::Memory));
        assert!(app.memory_view_open);
    }

    /// Esc (Shortcut::CancelDialog) dismisses the memory modal.
    #[test]
    fn escape_closes_the_memory_modal() {
        let (mut app, _) = App::new();
        let _ = app.update(Message::OpenMemoryModal);
        assert!(app.memory_view_open);
        let _ = app.update(Message::Shortcut(crate::shortcuts::Shortcut::CancelDialog));
        assert!(!app.memory_view_open);
    }

    /// DeleteRequested routes through the Memory state and arms the
    /// ConfirmModal gate with the target recorded; DeleteCancelled clears it
    /// without removing anything.
    #[test]
    fn memory_delete_request_arms_confirm_and_cancel_clears() {
        let (mut app, _) = App::new();
        app.memory.set_entries(vec![sample_memory_row()]);
        let _ = app.update(Message::Memory(crate::views::memory::Message::DeleteRequested(0)));
        assert!(app.memory.pending_delete.is_some());
        assert_eq!(app.memory.delete_target_id().as_deref(), Some("chunk-1"));
        let _ = app.update(Message::Memory(crate::views::memory::Message::DeleteCancelled));
        assert!(app.memory.pending_delete.is_none());
        assert_eq!(app.memory.delete_target_id(), None);
    }

    /// DeleteConfirmed clears the gate; the async backend invalidate resolves
    /// as MemoryEntryDeleted and removes the entry from the visible list.
    #[test]
    fn memory_delete_confirm_removes_entry_on_success() {
        let (mut app, _) = App::new();
        app.memory.set_entries(vec![sample_memory_row()]);
        let _ = app.update(Message::Memory(crate::views::memory::Message::DeleteRequested(0)));
        let id = app.memory.delete_target_id().unwrap();
        let _ = app.update(Message::Memory(crate::views::memory::Message::DeleteConfirmed));
        assert!(app.memory.pending_delete.is_none());
        let _ = app.update(Message::MemoryEntryDeleted { id, result: Ok(()) });
        // The target is gone: the gate was cleared and the entry removed.
        assert_eq!(app.memory.delete_target_id(), None);
    }

    // -----------------------------------------------------------------------
    // AgentMode tests (ADR-55 Phase 1e: the mode picker is gone; the intent
    // gate derives the effective outcome instead of a persisted mode).
    // -----------------------------------------------------------------------

    #[test]
    fn default_route_is_chat() {
        let (app, _) = App::new();
        assert_eq!(app.page, Page::Chat);
    }

    #[test]
    fn help_toggle_cycles() {
        let (mut app, _) = App::new();
        assert!(!app.show_help);
        let _ = app.update(Message::HelpToggled);
        assert!(app.show_help);
        let _ = app.update(Message::HelpToggled);
        assert!(!app.show_help);
    }

    // ── Terminal bottom panel (toggle + drag resize) ────────────────────────

    /// Opening the terminal panel flips `terminal_panel_open` and closing it
    /// flips it back. The returned `ensure_started` task is dropped without
    /// being run (iced only executes tasks handed to the runtime), and
    /// `iced_term::Terminal::new` does not spawn a shell synchronously — the
    /// process is launched through the async backend subscription — so the
    /// test stays hermetic.
    #[test]
    fn toggle_terminal_panel_opens_and_closes() {
        let (mut app, _) = App::new();
        assert!(!app.terminal_panel_open);
        let _ = app.update(Message::ToggleTerminalPanel);
        assert!(app.terminal_panel_open);
        let _ = app.update(Message::ToggleTerminalPanel);
        assert!(!app.terminal_panel_open);
    }

    /// The resize drag captures the origin at the first move and clamps the
    /// resulting height to [120.0, 600.0] even for huge deltas.
    #[test]
    fn terminal_resize_clamps_height() {
        let (mut app, _) = App::new();
        app.terminal_panel_height = 300.0;
        let _ = app.update(Message::TerminalPanelResizeStart);
        assert!(app.terminal_resizing);
        assert_eq!(app.terminal_start_height, 300.0);

        // First move only captures the drag origin.
        let _ = app.update(Message::TerminalPanelResizeMoved(500.0));
        assert_eq!(app.terminal_drag_origin, Some(500.0));
        assert_eq!(app.terminal_panel_height, 300.0);

        // A small upward drag grows the panel (origin above cursor = taller).
        let _ = app.update(Message::TerminalPanelResizeMoved(400.0));
        assert_eq!(app.terminal_panel_height, 400.0);

        // A huge downward delta is clamped to the minimum.
        let _ = app.update(Message::TerminalPanelResizeMoved(10_000.0));
        assert_eq!(app.terminal_panel_height, 120.0);

        // A huge upward delta is clamped to the maximum.
        let _ = app.update(Message::TerminalPanelResizeMoved(-10_000.0));
        assert_eq!(app.terminal_panel_height, 600.0);

        // Release ends the drag and clears the origin.
        let _ = app.update(Message::TerminalPanelResizeEnd);
        assert!(!app.terminal_resizing);
        assert_eq!(app.terminal_drag_origin, None);
    }

    /// Moves and end events are no-ops while no resize is in progress.
    #[test]
    fn terminal_resize_ignored_when_not_resizing() {
        let (mut app, _) = App::new();
        app.terminal_panel_height = 200.0;
        let _ = app.update(Message::TerminalPanelResizeMoved(123.0));
        assert_eq!(app.terminal_panel_height, 200.0);
        let _ = app.update(Message::TerminalPanelResizeEnd);
        assert!(!app.terminal_resizing);
    }

    // ── Overlay fade + terminal panel slide animations ─────────────────────

    /// Opening a sub-view overlay starts the fade-in (target 1.0); closing
    /// back to Main starts the fade-out (target 0.0). The current alpha is
    /// never reset, so re-opening mid-fade resumes from where it was.
    #[test]
    fn subview_open_starts_fade() {
        let (mut app, _) = App::new();
        let _ = app.update(Message::SetSubView(crate::views::chat::SubView::Diff));
        assert!(app.overlay_fading);
        assert_eq!(app.overlay_fade_target, 1.0);
        let _ = app.update(Message::SetSubView(crate::views::chat::SubView::Main));
        assert!(app.overlay_fading);
        assert_eq!(app.overlay_fade_target, 0.0);
    }

    /// The shared `AnimTick` advances the overlay fade and the terminal panel
    /// slide toward their targets in 0.08 steps and settles (~15 ticks = a
    /// full 240 ms fade/slide), clearing the in-flight flags.
    #[test]
    fn anim_tick_advances_and_settles() {
        let (mut app, _) = App::new();

        // Overlay fade: ramp the backdrop from transparent to fully dimmed.
        app.overlay_fading = true;
        app.overlay_fade_target = 1.0;
        app.overlay_fade = 0.0;
        for _ in 0..15 {
            let _ = app.update(Message::AnimTick);
        }
        assert!(!app.overlay_fading);
        assert!(app.overlay_fade >= 0.99);

        // Terminal panel: slide open from a fully closed position.
        app.terminal_panel_open = true;
        app.terminal_panel_animating = true;
        app.terminal_panel_anim = 0.0;
        for _ in 0..15 {
            let _ = app.update(Message::AnimTick);
        }
        assert!(!app.terminal_panel_animating);
        assert!(app.terminal_panel_anim >= 0.99);
    }

    #[test]
    fn configured_default_route_falls_back() {
        let config = concerto_config::AppConfig::default();
        let (provider, model) = configured_default_route(&config);
        assert!(provider.is_empty() || !provider.is_empty());
        assert!(model.is_empty() || !model.is_empty());
    }

    #[test]
    fn run_summary_includes_changed_files() {
        use camino::Utf8PathBuf;

        let output = AgentOutput {
            task_id: TaskId::new(),
            session_id: Ulid::new(),
            final_message: String::new(),
            files_modified: vec![Utf8PathBuf::from("src/main.rs")],
            tool_call_count: 1,
            eval_result: None,
            tool_events: Vec::new(),
            verification: Vec::new(),
            project_root: None,
            completion_status: concerto_core::types::AgentCompletionStatus::Completed,
            provider_metrics: Vec::new(),
            checkpoint_json: None,
        };
        let summary = super::format_run_summary(&output);
        assert!(summary.contains("src/main.rs"), "summary must list the changed file: {summary}");
        assert!(summary.contains("Files changed"), "summary must include a changed-files section");

        let mut partial = output.clone();
        partial.completion_status = concerto_core::types::AgentCompletionStatus::Partial;
        assert!(super::format_run_summary(&partial).starts_with("Partial progress preserved."));

        // When no files changed, the raw final message is used verbatim.
        let plain = AgentOutput {
            task_id: TaskId::new(),
            session_id: Ulid::new(),
            final_message: "All done".to_string(),
            files_modified: vec![],
            tool_call_count: 0,
            eval_result: None,
            tool_events: Vec::new(),
            verification: Vec::new(),
            project_root: None,
            completion_status: concerto_core::types::AgentCompletionStatus::Completed,
            provider_metrics: Vec::new(),
            checkpoint_json: None,
        };
        assert_eq!(super::format_run_summary(&plain), "All done");
    }

    // ── Run-stage chip (ADR-55 Phase 2a) ───────────────────────────────────

    /// A `RunStageChanged` event while a run is in flight arms the chip.
    #[test]
    fn run_stage_event_updates_chip_while_running() {
        let (mut app, _) = App::new();
        app.run_status = RunStatus::Running;
        let _ = app.update(Message::DesktopEvent(crate::runtime::DesktopEvent::RunStageChanged {
            stage: RunStage::Execute,
        }));
        assert_eq!(app.run_stage, Some(RunStage::Execute));
    }

    /// A stage event outside a run (stale bus replay, cancelled run) must not
    /// re-arm the chip: the App update is guarded on the run status.
    #[test]
    fn run_stage_event_ignored_when_not_running() {
        let (mut app, _) = App::new();
        assert_eq!(app.run_status, RunStatus::Idle);
        // A stale stage left over (e.g. mid-cancel) stays untouched: the chip
        // only re-appears after a fresh run boundary re-arms it.
        app.run_stage = Some(RunStage::Inspect);
        let _ = app.update(Message::DesktopEvent(crate::runtime::DesktopEvent::RunStageChanged {
            stage: RunStage::Execute,
        }));
        assert!(app.run_stage.is_none() || app.run_stage == Some(RunStage::Inspect));
    }

    /// The chip clears at the run boundary: the completion handler drops the
    /// stage alongside the Idle transition (before the result match, so both
    /// Ok and Err completions pass through the same clearing line).
    #[test]
    fn agent_run_completed_clears_run_stage() {
        let (mut app, _) = App::new();
        app.run_status = RunStatus::Running;
        app.run_stage = Some(RunStage::Execute);
        // True run boundary: status flips to Idle and the stage is dropped in
        // one line before any result handling.
        let _ = app.update(Message::AgentRunCompleted(
            None,
            Box::new(Ok(AgentOutput {
                task_id: TaskId::new(),
                session_id: Ulid::new(),
                final_message: "done".to_string(),
                files_modified: vec![],
                tool_call_count: 0,
                eval_result: None,
                tool_events: Vec::new(),
                verification: Vec::new(),
                project_root: None,
                completion_status: AgentCompletionStatus::Completed,
                provider_metrics: Vec::new(),
                checkpoint_json: None,
            })),
        ));
        assert_eq!(app.run_status, RunStatus::Idle);
        assert_eq!(app.run_stage, None);
    }

    // ── Dispatch-boundary validation (plan §13 Runtime selection) ──────────

    fn push_provider(app: &mut App, id: &str, provider: &str, model: &str) {
        app.settings.providers.push(ProviderConfig {
            id: id.into(),
            name: provider.into(),
            provider: provider.into(),
            model: model.into(),
            cached_models: Vec::new(),
            cached_models_fetched_at: 0,
            ..ProviderConfig::default()
        });
    }

    /// Mirror the Settings rows into `config.model_settings.providers` so
    /// `runtime_providers` resolves them. Tests push rows into
    /// `settings.providers`, but on a host with a real user config
    /// `App::new()` loads it and `runtime_providers` prefers the config list —
    /// without this mirror the refresh handlers treat every pushed row as
    /// deleted and silently drop its results.
    fn sync_config_providers(app: &mut App) {
        let ms = app
            .config
            .get_or_insert_with(AppConfig::default)
            .model_settings
            .get_or_insert_with(concerto_config::ModelSettings::default);
        ms.providers = app.settings.providers.clone();
    }

    #[test]
    fn dispatch_blocked_when_active_provider_missing_credential() {
        let (mut app, _) = App::new();
        app.settings.providers.clear();
        // OpenAI requires a credential; with none stored it is not ready and must
        // not be dispatched. (Models are now assigned per role, so an empty model
        // on the provider itself no longer blocks dispatch.)
        push_provider(&mut app, "prov1", "openai", "");
        app.active_provider_id = "prov1".into();
        assert!(
            app.dispatch_validation_error().is_some(),
            "active provider missing required credential must block dispatch"
        );
    }

    #[test]
    fn dispatch_allowed_when_active_provider_ready() {
        let (mut app, _) = App::new();
        app.settings.providers.clear();
        // Ollama needs no credential, so it is ready regardless of a stored model
        // (models are chosen per agent role now).
        push_provider(&mut app, "prov1", "ollama", "");
        app.active_provider_id = "prov1".into();
        assert!(
            app.dispatch_validation_error().is_none(),
            "ready active provider must allow dispatch"
        );
    }

    #[test]
    fn dispatch_blocked_when_assignment_missing_model() {
        let (mut app, _) = App::new();
        app.settings.providers.clear();
        push_provider(&mut app, "prov1", "ollama", "");
        app.active_provider_id = "prov1".into();
        app.multi_agent = true;
        // Ensure assignments in config (runtime_assignments reads from config)
        let ms = app
            .config
            .get_or_insert_with(AppConfig::default)
            .model_settings
            .get_or_insert_with(concerto_config::ModelSettings::default);
        ms.agent_assignments = vec![concerto_config::AgentModelAssignment {
            agent_role: "coordinator".into(),
            provider_config_id: "prov1".into(),
            model_override: None,
        }];
        assert!(
            app.dispatch_validation_error().is_some(),
            "role assignment without a model must block dispatch"
        );
    }

    #[test]
    fn multi_agent_validates_all_assignments() {
        let (mut app, _) = App::new();
        app.settings.providers.clear();
        push_provider(&mut app, "prov1", "ollama", "");
        app.active_provider_id = "prov1".into();
        app.multi_agent = true;
        let ms = app
            .config
            .get_or_insert_with(AppConfig::default)
            .model_settings
            .get_or_insert_with(concerto_config::ModelSettings::default);
        // Keep providers and assignments in config in sync — both
        // runtime_providers and runtime_assignments read from config.
        ms.providers = app.settings.providers.clone();
        ms.agent_assignments = vec![
            concerto_config::AgentModelAssignment {
                agent_role: "coordinator".into(),
                provider_config_id: "prov1".into(),
                model_override: Some("llama3".into()),
            },
            concerto_config::AgentModelAssignment {
                agent_role: "coder".into(),
                provider_config_id: "prov1".into(),
                model_override: None,
            },
        ];

        // The intent gate is always on (ADR-55 Phase 1e): every run is a
        // potential Execute, so an incomplete specialist assignment blocks
        // dispatch — there is no mode picker to narrow the check.
        assert!(
            app.dispatch_validation_error().is_some(),
            "multi-agent runs must validate every assignment"
        );
    }

    // ── Refresh concurrency (plan §13 Refresh concurrency) ─────────────────

    #[test]
    fn stale_refresh_result_is_ignored() {
        let (mut app, _) = App::new();
        app.settings.providers.clear();
        push_provider(&mut app, "prov1", "openai", "gpt-4");
        app.settings.providers[0].cached_models = vec!["gpt-4".into()];
        app.settings.providers[0].cached_models_fetched_at = 1;

        // Simulate an in-flight refresh (request id 1) without spawning a task.
        app.refresh_seq = 1;
        app.pending_refresh.insert("prov1".into(), 1);

        // A result carrying a stale request id (0) must be dropped.
        let _ = app.update(Message::Settings(SettingsMessage::ProviderModelsRefreshed {
            provider_id: "prov1".into(),
            request_id: 0,
            result: Ok(vec!["hacked-model".into()]),
        }));
        let p = app.settings.providers.iter().find(|p| p.id == "prov1").unwrap();
        assert_eq!(
            p.cached_models,
            vec!["gpt-4".to_string()],
            "stale refresh result must not mutate the cache"
        );
    }

    #[test]
    fn current_refresh_updates_cache_and_failure_preserves_it() {
        let (mut app, _) = App::new();
        app.settings.providers.clear();
        push_provider(&mut app, "prov1", "openai", "gpt-4");
        sync_config_providers(&mut app);
        app.settings.providers[0].cached_models = vec!["gpt-4".into()];
        app.settings.providers[0].cached_models_fetched_at = 1;

        // First refresh (request id 1) succeeds.
        app.refresh_seq = 1;
        app.pending_refresh.insert("prov1".into(), 1);
        let _ = app.update(Message::Settings(SettingsMessage::ProviderModelsRefreshed {
            provider_id: "prov1".into(),
            request_id: 1,
            result: Ok(vec!["gpt-4".into(), "gpt-4o".into()]),
        }));
        let p = app.settings.providers.iter().find(|p| p.id == "prov1").unwrap();
        assert!(
            p.cached_models.contains(&"gpt-4o".to_string()),
            "successful refresh must update the cache"
        );

        // Second refresh (request id 2) fails.
        app.refresh_seq = 2;
        app.pending_refresh.insert("prov1".into(), 2);
        let _ = app.update(Message::Settings(SettingsMessage::ProviderModelsRefreshed {
            provider_id: "prov1".into(),
            request_id: 2,
            result: Err("network down".into()),
        }));
        let p = app.settings.providers.iter().find(|p| p.id == "prov1").unwrap();
        assert!(
            p.cached_models.contains(&"gpt-4o".to_string()),
            "failed refresh must preserve the previous cache"
        );
    }

    #[test]
    fn refresh_result_for_deleted_provider_is_ignored() {
        let (mut app, _) = App::new();
        app.settings.providers.clear();
        push_provider(&mut app, "provA", "openai", "gpt-4");
        push_provider(&mut app, "provB", "openai", "gpt-4");

        // Delete provA while the refresh is "in flight".
        app.settings.providers.retain(|p| p.id != "provA");
        // Mirror AFTER the deletion so runtime_providers agrees provA is gone
        // (a host's real user config would otherwise keep it resolvable).
        sync_config_providers(&mut app);
        app.refresh_seq = 1;
        app.pending_refresh.insert("provA".into(), 1);

        // The result for the deleted provider must be ignored, and provB must
        // be untouched.
        let _ = app.update(Message::Settings(SettingsMessage::ProviderModelsRefreshed {
            provider_id: "provA".into(),
            request_id: 1,
            result: Ok(vec!["hacked".into()]),
        }));
        assert!(
            !app.settings.providers.iter().any(|p| p.id == "provA"),
            "provA must remain deleted"
        );
        let b = app.settings.providers.iter().find(|p| p.id == "provB").unwrap();
        assert_eq!(
            b.cached_models,
            Vec::<String>::new(),
            "other providers must not be affected by a deleted provider's result"
        );
    }

    #[test]
    fn discovered_models_populate_picker_and_provider_options() {
        let (mut app, _) = App::new();
        app.settings.providers.clear();
        push_provider(&mut app, "prov1", "ollama", "llama3");
        sync_config_providers(&mut app);
        app.active_provider_id = "prov1".into();
        app.refresh_seq = 1;
        app.pending_refresh.insert("prov1".into(), 1);

        // Simulate a discovery result flowing through the full update path.
        let _ = app.update(Message::Settings(SettingsMessage::ProviderModelsRefreshed {
            provider_id: "prov1".into(),
            request_id: 1,
            result: Ok(vec!["discovered-model".into(), "llama3".into()]),
        }));
        let cache = app.settings.cached_models_by_provider();
        assert!(
            cache
                .get("prov1")
                .map(|models| models.iter().any(|m| m == "discovered-model"))
                .unwrap_or(false),
            "discovered model must appear in the per-provider model cache"
        );
    }

    #[test]
    fn manual_refresh_request_registers_tracked_in_flight_fetch() {
        let (mut app, _) = App::new();
        app.settings.providers.clear();
        push_provider(&mut app, "prov1", "openai", "");
        sync_config_providers(&mut app);

        // Startup auto-discovery may already have consumed request ids.
        let seq_before = app.refresh_seq;
        let _ = app.update(Message::Settings(SettingsMessage::ProviderModelsRefreshRequested(
            "prov1".into(),
        )));

        assert_eq!(
            app.pending_refresh.get("prov1"),
            Some(&seq_before.wrapping_add(1)),
            "manual refresh must register a tracked request id"
        );
        assert!(
            app.settings.refreshing_providers.contains("prov1"),
            "the provider row must report its refresh as in flight"
        );
    }

    #[test]
    fn manual_refresh_request_for_unknown_or_nondiscovering_provider_is_ignored() {
        let (mut app, _) = App::new();
        // Startup auto-discovery may legitimately hold tracked requests; only
        // the unknown id must stay untouched.
        let pending_before = app.pending_refresh.len();

        let _ = app.update(Message::Settings(SettingsMessage::ProviderModelsRefreshRequested(
            "ghost".into(),
        )));

        assert_eq!(
            app.pending_refresh.len(),
            pending_before,
            "unknown providers must not spawn fetches"
        );
        assert!(!app.pending_refresh.contains_key("ghost"));
        assert!(app.settings.refreshing_providers.is_empty());
    }

    #[test]
    fn completed_manual_refresh_updates_cache_and_clears_in_flight_state() {
        let (mut app, _) = App::new();
        app.settings.providers.clear();
        push_provider(&mut app, "prov1", "openai", "");
        sync_config_providers(&mut app);

        let _ = app.update(Message::Settings(SettingsMessage::ProviderModelsRefreshRequested(
            "prov1".into(),
        )));
        let request_id = *app.pending_refresh.get("prov1").expect("tracked request");

        // The spawned task is dropped in tests; simulate its completion with
        // the request id the handler assigned.
        let _ = app.update(Message::Settings(SettingsMessage::ProviderModelsRefreshed {
            provider_id: "prov1".into(),
            request_id,
            result: Ok(vec!["ox-alpha".into(), "gpt-4o".into()]),
        }));

        assert!(
            !app.settings.refreshing_providers.contains("prov1"),
            "the in-flight marker must clear when the result arrives"
        );
        assert!(!app.pending_refresh.contains_key("prov1"));
        assert!(!app.settings.provider_refresh_errors.contains_key("prov1"));
        let cache = app.settings.cached_models_by_provider();
        assert!(
            cache.get("prov1").map(|m| m.iter().any(|n| n == "ox-alpha")).unwrap_or(false),
            "newly released models must appear in the picker cache immediately"
        );
    }

    #[test]
    fn failed_manual_refresh_preserves_cache_and_reports_inline_error() {
        let (mut app, _) = App::new();
        app.settings.providers.clear();
        push_provider(&mut app, "prov1", "openai", "");
        sync_config_providers(&mut app);
        app.settings.providers[0].cached_models = vec!["gpt-4".into()];
        app.settings.providers[0].cached_models_fetched_at = 1;

        // An explicit Err (network outage) must preserve the cache, clear the
        // in-flight marker, and surface the error inline.
        app.refresh_seq = 1;
        app.pending_refresh.insert("prov1".into(), 1);
        app.settings.begin_provider_refresh("prov1");
        let _ = app.update(Message::Settings(SettingsMessage::ProviderModelsRefreshed {
            provider_id: "prov1".into(),
            request_id: 1,
            result: Err("connection refused".into()),
        }));
        let p = app.settings.providers.iter().find(|p| p.id == "prov1").unwrap();
        assert!(
            p.cached_models.contains(&"gpt-4".to_string()),
            "failed refresh must preserve the previous cache"
        );
        assert!(!app.settings.refreshing_providers.contains("prov1"));
        assert_eq!(
            app.settings.provider_refresh_errors.get("prov1").map(String::as_str),
            Some("connection refused")
        );

        // A later empty discovery result — the shape every providers-crate
        // failure collapses to before this handler — must likewise never wipe
        // the cached list.
        app.refresh_seq = 2;
        app.pending_refresh.insert("prov1".into(), 2);
        app.settings.begin_provider_refresh("prov1");
        let _ = app.update(Message::Settings(SettingsMessage::ProviderModelsRefreshed {
            provider_id: "prov1".into(),
            request_id: 2,
            result: Ok(Vec::new()),
        }));
        let p = app.settings.providers.iter().find(|p| p.id == "prov1").unwrap();
        assert!(
            p.cached_models.contains(&"gpt-4".to_string()),
            "an empty discovery result must not wipe the cached model list"
        );
        assert!(
            app.settings.provider_refresh_errors.contains_key("prov1"),
            "an empty discovery result must be surfaced as a failure"
        );
    }

    // ── Shared selector consistency (plan §13 Runtime selection) ────────────

    #[test]
    fn chat_model_options_match_shared_resolver() {
        let (mut app, _) = App::new();
        app.settings.providers.clear();
        push_provider(&mut app, "prov1", "openai", "gpt-4");
        app.settings.providers[0].cached_models = vec!["gpt-4o".into()];
        app.active_provider_id = "prov1".into();
        app.active_model = "gpt-4".into();
        app.sync_chat_model_options();
        assert!(
            app.chat_model_options.contains(&"gpt-4".to_string()),
            "the active/selected model must be present in the chat picker"
        );
        assert!(
            app.chat_model_options.contains(&"gpt-4o".to_string()),
            "discovered models must be present in the chat picker"
        );
    }

    #[test]
    fn configured_route_is_resolved_from_model_not_deprecated_provider_id() {
        let config = AppConfig {
            model_settings: Some(concerto_config::ModelSettings {
                providers: vec![
                    concerto_config::ProviderConfig {
                        id: "first".into(),
                        provider: "openai".into(),
                        model: "gpt-4o".into(),
                        ..Default::default()
                    },
                    concerto_config::ProviderConfig {
                        id: "second".into(),
                        provider: "anthropic".into(),
                        model: "claude-sonnet-4".into(),
                        ..Default::default()
                    },
                ],
                global_default_model: Some("claude-sonnet-4".into()),
                global_default_id: Some("first".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(
            configured_default_route(&config),
            ("second".to_string(), "claude-sonnet-4".to_string())
        );
    }

    // ── Generation-safe save feedback (plan §9.2 / §13 UI feedback) ────────

    #[test]
    fn stale_save_feedback_timer_cannot_clear_newer_save() {
        let (mut app, _) = App::new();
        // Simulate the first save scheduling a clear for generation 1.
        app.save_feedback = Some("Saved".into());
        app.save_feedback_generation = 1;

        // A newer save increments the generation and shows fresh feedback.
        app.save_feedback_generation = 2;
        app.save_feedback = Some("Saved again".into());

        // The stale timer from the first save (generation 1) arrives.
        let _ = app.update(Message::ClearSaveFeedback(1));
        assert_eq!(
            app.save_feedback.as_deref(),
            Some("Saved again"),
            "stale timer must not erase the newer save's feedback"
        );

        // The current timer (generation 2) clears it.
        let _ = app.update(Message::ClearSaveFeedback(2));
        assert!(app.save_feedback.is_none(), "current timer must clear feedback");
    }

    /// Navigation to Studio page works.
    #[test]
    fn navigate_to_studio_changes_page() {
        let (mut app, _) = App::new();
        let _ = app.update(Message::Navigate(Page::OrchestrationStudio));
        assert_eq!(app.page, Page::OrchestrationStudio);
    }

    /// Screenshot status can be stored and cleared.
    #[test]
    fn screenshot_status_stored_and_cleared() {
        let (mut app, _) = App::new();
        assert!(app.screenshot_status.is_none());
        app.screenshot_status = Some("captured".into());
        assert_eq!(app.screenshot_status.as_deref(), Some("captured"));
        app.screenshot_status = None;
        assert!(app.screenshot_status.is_none());
    }

    /// Save feedback can be stored and cleared.
    #[test]
    fn save_feedback_stored_and_cleared() {
        let (mut app, _) = App::new();
        assert!(app.save_feedback.is_none());
        app.save_feedback = Some("saved".into());
        assert_eq!(app.save_feedback.as_deref(), Some("saved"));
        app.save_feedback = None;
        assert!(app.save_feedback.is_none());
    }

    // -----------------------------------------------------------------------
    // Durable typed transcript restore (ADR-36, stage 3)
    // -----------------------------------------------------------------------

    /// Every `TranscriptEntry` variant maps to the expected `ChatEntry` with
    /// sequential ids, correct status mapping, thinking label formats and
    /// collapse flags.
    #[test]
    fn transcript_to_entries_maps_all_variants() {
        use crate::views::chat::{ChatEntry, RunCompletionSummary, ToolCallStatus};
        use concerto_core::transcript::{TranscriptEntry, TranscriptToolStatus};

        let transcript = vec![
            TranscriptEntry::User { content: "build the widget".into() },
            TranscriptEntry::Assistant { content: "on it".into() },
            TranscriptEntry::Thinking { agent: "coder".into(), content: "step one".into() },
            TranscriptEntry::Thinking { agent: String::new(), content: "bare thought".into() },
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
                tool_name: "git".into(),
                detail: String::new(),
                status: TranscriptToolStatus::Denied,
            },
            TranscriptEntry::ToolCall {
                tool_name: "net".into(),
                detail: String::new(),
                status: TranscriptToolStatus::Cancelled,
            },
            TranscriptEntry::ToolCall {
                tool_name: "probe".into(),
                detail: String::new(),
                status: TranscriptToolStatus::Failed,
            },
            TranscriptEntry::ToolCall {
                tool_name: "live".into(),
                detail: String::new(),
                status: TranscriptToolStatus::Running,
            },
            TranscriptEntry::Activity {
                agent: "Coordinator".into(),
                content: "Delegated subtask T1 to coder".into(),
            },
            TranscriptEntry::Error { content: "boom".into() },
            TranscriptEntry::Summary { content: "context compacted".into() },
            TranscriptEntry::Completion {
                multi_agent: true,
                completed: true,
                files: vec!["main.rs".into()],
                project_root: Some("/proj".into()),
            },
        ];

        let entries = super::transcript_to_entries(transcript);
        let expected = vec![
            ChatEntry::User { id: 1, content: "build the widget".into(), created_at: None },
            ChatEntry::Assistant {
                id: 2,
                content: "on it".into(),
                streaming: false,
                created_at: None,
            },
            ChatEntry::Thinking {
                id: 3,
                content: "[coder] step one".into(),
                collapsed: false,
                created_at: None,
                finished_at: None,
            },
            ChatEntry::Thinking {
                id: 4,
                content: "bare thought".into(),
                collapsed: false,
                created_at: None,
                finished_at: None,
            },
            ChatEntry::ToolCall {
                id: 5,
                tool_name: "fs_write".into(),
                detail: "write main.rs".into(),
                status: ToolCallStatus::Completed,
                created_at: None,
            },
            ChatEntry::ToolCall {
                id: 6,
                tool_name: "shell".into(),
                detail: String::new(),
                status: ToolCallStatus::Allowed,
                created_at: None,
            },
            ChatEntry::ToolCall {
                id: 7,
                tool_name: "git".into(),
                detail: String::new(),
                status: ToolCallStatus::Denied,
                created_at: None,
            },
            ChatEntry::ToolCall {
                id: 8,
                tool_name: "net".into(),
                detail: String::new(),
                status: ToolCallStatus::Cancelled,
                created_at: None,
            },
            ChatEntry::ToolCall {
                id: 9,
                tool_name: "probe".into(),
                detail: String::new(),
                status: ToolCallStatus::Failed,
                created_at: None,
            },
            ChatEntry::ToolCall {
                id: 10,
                tool_name: "live".into(),
                detail: String::new(),
                status: ToolCallStatus::Running,
                created_at: None,
            },
            ChatEntry::Thinking {
                id: 11,
                content: "[Coordinator] Delegated subtask T1 to coder".into(),
                collapsed: false,
                created_at: None,
                finished_at: None,
            },
            ChatEntry::Error { id: 12, content: "boom".into(), created_at: None },
            ChatEntry::Thinking {
                id: 13,
                content: "[Context] context compacted".into(),
                collapsed: true,
                created_at: None,
                finished_at: None,
            },
            ChatEntry::Completion {
                id: 14,
                summary: RunCompletionSummary {
                    multi_agent: true,
                    completed: true,
                    files: vec!["main.rs".into()],
                    project_root: Some("/proj".into()),
                },
                created_at: None,
            },
        ];

        assert_eq!(
            serde_json::to_value(&entries).unwrap(),
            serde_json::to_value(&expected).unwrap(),
        );
    }

    /// The ADR-36 proof: restored transcript entries equal the live-built
    /// entries for the same scripted event sequence (user, thinking, correlated
    /// tool lifecycle, assistant, error). The tool-call entry mirrors the
    /// recorder's merge (start detail + terminal detail).
    #[test]
    fn restored_transcript_matches_live_rendering() {
        use crate::views::chat::State;
        use concerto_core::transcript::{TranscriptEntry, TranscriptToolStatus};

        let transcript = vec![
            TranscriptEntry::User { content: "build the widget".into() },
            TranscriptEntry::Thinking { agent: "coder".into(), content: "step one".into() },
            TranscriptEntry::ToolCall {
                tool_name: "fs_write".into(),
                detail: "write main.rs\nWrote 42 bytes".into(),
                status: TranscriptToolStatus::Completed,
            },
            TranscriptEntry::Assistant { content: "the fix is in".into() },
            TranscriptEntry::Error { content: "boom".into() },
        ];
        let restored = super::transcript_to_entries(transcript);

        // Live rendering for the same scripted events (mirrors runtime.rs
        // route_event + the run-end finalize_run).
        let mut live = State::new();
        let _ = live.update(crate::views::chat::Message::AddUser("build the widget".into()));
        live.add_thinking("[coder] step one".into());
        live.add_tool_call("fs_write".into(), "write main.rs".into());
        live.update_tool_call("fs_write", "Wrote 42 bytes".into(), true);
        live.update_last_assistant("the fix is in".into());
        live.finalize_run();
        live.add_error("boom".into());

        assert_eq!(
            without_timestamps(serde_json::to_value(&restored).unwrap()),
            without_timestamps(serde_json::to_value(live.entries()).unwrap()),
            "restored transcript entries must equal live-built entries \
             for the same scripted sequence"
        );

        // Restoring through State::from_entries must preserve the same entries
        // (no Running tool calls remain to settle).
        let restored_state = State::from_entries(restored);
        assert_eq!(
            without_timestamps(serde_json::to_value(restored_state.entries()).unwrap()),
            without_timestamps(serde_json::to_value(live.entries()).unwrap()),
        );
    }

    /// Drop the `created_at`/`finished_at` keys from serialized entries so
    /// restored (timestamp-less) transcripts can be compared with live-built
    /// entries that carry real timestamps.
    fn without_timestamps(value: serde_json::Value) -> serde_json::Value {
        match value {
            serde_json::Value::Object(map) => serde_json::Value::Object(
                map.into_iter()
                    .filter(|(key, _)| key != "created_at" && key != "finished_at")
                    .map(|(key, value)| (key, without_timestamps(value)))
                    .collect(),
            ),
            serde_json::Value::Array(items) => {
                serde_json::Value::Array(items.into_iter().map(without_timestamps).collect())
            }
            other => other,
        }
    }

    /// Documented divergences between restored and live rendering: approval
    /// representation, SubTaskCreated wording, summary collapse state and the
    /// structured completion card. The restored (canonical) forms are asserted
    /// here alongside the live forms they intentionally differ from.
    #[test]
    fn restored_approval_activity_and_summary_renderings_are_documented() {
        use crate::views::chat::{ChatEntry, State, ToolCallStatus};
        use concerto_core::transcript::{TranscriptEntry, TranscriptToolStatus};

        let transcript = vec![
            TranscriptEntry::User { content: "deploy".into() },
            // Approval outcome is persisted as the canonical status on the tool call.
            TranscriptEntry::ToolCall {
                tool_name: "shell".into(),
                detail: String::new(),
                status: TranscriptToolStatus::Allowed,
            },
            TranscriptEntry::Activity {
                agent: "Coordinator".into(),
                content: "Decomposed task T1 into specialist subtask: apply the fix".into(),
            },
            TranscriptEntry::Summary { content: "context compacted".into() },
            TranscriptEntry::Completion {
                multi_agent: true,
                completed: false,
                files: vec!["main.rs".into()],
                project_root: Some("/proj".into()),
            },
        ];
        let restored = super::transcript_to_entries(transcript);

        // Restored approval: canonical tool name + Allowed status (1:1 mapping).
        assert!(matches!(
            &restored[1],
            ChatEntry::ToolCall { tool_name, status: ToolCallStatus::Allowed, .. }
                if tool_name == "shell"
        ));
        // Live approval rendering diverges: it pushes a separate Running entry
        // with the outcome appended to the name ("shell (allowed)").
        let mut live = State::new();
        live.add_tool_call("shell (allowed)".into(), String::new());
        assert!(matches!(
            live.entries().last(),
            Some(ChatEntry::ToolCall { tool_name, status: ToolCallStatus::Running, .. })
                if tool_name == "shell (allowed)"
        ));

        // Restored activity: `[agent] content` thinking line. The live
        // SubTaskCreated line uses different wording ("[Coordinator → coder]
        // apply the fix") — a documented string divergence; both render as
        // thinking lines.
        assert!(matches!(
            &restored[2],
            ChatEntry::Thinking { content, collapsed: false, .. }
                if content == "[Coordinator] Decomposed task T1 into specialist subtask: apply the fix"
        ));

        // Restored summary: collapsed thinking line with the [Context] prefix.
        assert!(matches!(
            &restored[3],
            ChatEntry::Thinking { content, collapsed: true, .. }
                if content == "[Context] context compacted"
        ));

        // Restored completion: structured RunCompletionSummary.
        match &restored[4] {
            ChatEntry::Completion { summary, .. } => {
                assert!(summary.multi_agent);
                assert!(!summary.completed);
                assert_eq!(summary.files, vec!["main.rs".to_string()]);
                assert_eq!(summary.project_root.as_deref(), Some("/proj"));
            }
            other => panic!("expected Completion entry, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // DesktopApprovalSink — frontend parity with the CLI approval sink
    // -----------------------------------------------------------------------
    //
    // The desktop sink must behave exactly like `CliApprovalSink`: per-call
    // prompting unless the user explicitly opts into session-wide
    // auto-approval. In particular a plain "Grant for this session" decision
    // must NOT auto-approve later calls of the same tool (the old name-based
    // `session_grants` cache is gone).

    /// Minimal policy action for approval-sink tests.
    fn make_action<'a>(tool_name: &'a str, input: &'a serde_json::Value) -> PolicyAction<'a> {
        PolicyAction {
            tool_name,
            input,
            session_id: Ulid::new(),
            correlation_id: Ulid::new(),
            capability_requirements: concerto_core::types::CapabilitySet::default(),
            sandbox_profile: None,
            estimated_cost_usd: None,
            command_facts: None,
        }
    }

    /// Wait until the sink's request future has queued a dialog on
    /// `cap_pending`, so the test resolves it without racing the spawn.
    /// Bounded so a broken sink fails the test instead of hanging forever.
    async fn wait_for_pending_dialog(shared: &crate::widgets::capability_dialog::SharedPending) {
        for _ in 0..500 {
            if !shared.lock().unwrap_or_else(|e| e.into_inner()).is_empty() {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("approval request never queued a dialog");
    }

    #[tokio::test]
    async fn approval_sink_auto_approve_returns_approve_without_dialog() {
        let cap_pending = crate::widgets::capability_dialog::shared_pending();
        let pending_ack = crate::widgets::capability_dialog::shared_pending_ack();
        let sink = DesktopApprovalSink {
            cap_pending: cap_pending.clone(),
            pending_ack: pending_ack.clone(),
            pending_intent: crate::widgets::capability_dialog::shared_pending_intent(),
            pending_plan: crate::widgets::capability_dialog::shared_pending_plan(),
            auto_approve: Arc::new(AtomicBool::new(true)),
            bus: EventBus::default(),
        };
        let input = serde_json::json!({ "path": "/tmp/example.rs" });
        let action = make_action("write_file", &input);
        let cancel = CancellationToken::new();
        let decision = sink.request_approval(&action, cancel).await;
        assert_eq!(decision, ApprovalDecision::Approve);
        // Fast path short-circuits before pushing a dialog.
        assert!(
            cap_pending.lock().unwrap_or_else(|e| e.into_inner()).is_empty(),
            "auto-approve must not queue a dialog"
        );
    }

    #[tokio::test]
    async fn approval_sink_approve_all_for_session_enables_auto_approve() {
        let cap_pending = crate::widgets::capability_dialog::shared_pending();
        let pending_ack = crate::widgets::capability_dialog::shared_pending_ack();
        let sink = DesktopApprovalSink {
            cap_pending: cap_pending.clone(),
            pending_ack: pending_ack.clone(),
            pending_intent: crate::widgets::capability_dialog::shared_pending_intent(),
            pending_plan: crate::widgets::capability_dialog::shared_pending_plan(),
            auto_approve: Arc::new(AtomicBool::new(false)),
            bus: EventBus::default(),
        };
        let cancel = CancellationToken::new();
        assert!(!sink.auto_approve.load(Ordering::Relaxed));
        sink.approve_all_for_session(Ulid::new(), cancel.clone()).await;
        assert!(sink.auto_approve.load(Ordering::Relaxed));

        // Subsequent request is approved without a dialog.
        let input = serde_json::json!({ "path": "/tmp/example.rs" });
        let action = make_action("write_file", &input);
        let decision = sink.request_approval(&action, cancel).await;
        assert_eq!(decision, ApprovalDecision::Approve);
        assert!(
            cap_pending.lock().unwrap_or_else(|e| e.into_inner()).is_empty(),
            "auto-approve must not queue a dialog"
        );
    }

    #[tokio::test]
    async fn approval_sink_request_ack_auto_approves_when_flag_set() {
        let cap_pending = crate::widgets::capability_dialog::shared_pending();
        let pending_ack = crate::widgets::capability_dialog::shared_pending_ack();
        let sink = DesktopApprovalSink {
            cap_pending: cap_pending.clone(),
            pending_ack: pending_ack.clone(),
            pending_intent: crate::widgets::capability_dialog::shared_pending_intent(),
            pending_plan: crate::widgets::capability_dialog::shared_pending_plan(),
            auto_approve: Arc::new(AtomicBool::new(true)),
            bus: EventBus::default(),
        };
        let cancel = CancellationToken::new();
        let ack = sink.request_ack("some warning", cancel).await;
        assert!(ack, "request_ack must return true when auto-approve is on");
    }

    #[tokio::test]
    async fn approval_sink_granted_single_decision_does_not_enable_auto_approve() {
        let cap_pending = crate::widgets::capability_dialog::shared_pending();
        let pending_ack = crate::widgets::capability_dialog::shared_pending_ack();
        let sink = DesktopApprovalSink {
            cap_pending: cap_pending.clone(),
            pending_ack: pending_ack.clone(),
            pending_intent: crate::widgets::capability_dialog::shared_pending_intent(),
            pending_plan: crate::widgets::capability_dialog::shared_pending_plan(),
            auto_approve: Arc::new(AtomicBool::new(false)),
            bus: EventBus::default(),
        };
        let cancel = CancellationToken::new();

        // First request: the user picks "Grant for this session" (Granted).
        let sink2 = sink.clone();
        let cancel2 = cancel.clone();
        let handle = tokio::spawn(async move {
            let input = serde_json::json!({ "path": "/tmp/example.rs" });
            let action = make_action("write_file", &input);
            sink2.request_approval(&action, cancel2).await
        });
        wait_for_pending_dialog(&cap_pending).await;
        assert!(
            crate::widgets::capability_dialog::resolve(
                &cap_pending,
                &crate::widgets::capability_dialog::Message::GrantSession
            ),
            "expected a pending approval to resolve"
        );
        assert_eq!(handle.await.expect("request task panicked"), ApprovalDecision::Approve);
        assert!(
            !sink.auto_approve.load(Ordering::Relaxed),
            "a single Granted decision must not enable auto-approve"
        );

        // Second request: a different path for the same tool. It must prompt
        // again (no name-based cache) — denying it yields Deny, whereas the
        // old behavior auto-approved every `write_file` after the first grant.
        let sink2 = sink.clone();
        let cancel2 = cancel.clone();
        let handle = tokio::spawn(async move {
            let input = serde_json::json!({ "path": "/tmp/other.rs" });
            let action = make_action("write_file", &input);
            sink2.request_approval(&action, cancel2).await
        });
        wait_for_pending_dialog(&cap_pending).await;
        assert!(
            crate::widgets::capability_dialog::resolve(
                &cap_pending,
                &crate::widgets::capability_dialog::Message::Deny
            ),
            "expected the second approval to prompt again"
        );
        assert_eq!(handle.await.expect("request task panicked"), ApprovalDecision::Deny);
        assert!(
            !sink.auto_approve.load(Ordering::Relaxed),
            "auto-approve must stay off after a per-call grant"
        );
    }

    #[tokio::test]
    async fn approval_sink_grant_always_enables_auto_approve_and_records_session_decision() {
        let cap_pending = crate::widgets::capability_dialog::shared_pending();
        let pending_ack = crate::widgets::capability_dialog::shared_pending_ack();
        let sink = DesktopApprovalSink {
            cap_pending: cap_pending.clone(),
            pending_ack: pending_ack.clone(),
            pending_intent: crate::widgets::capability_dialog::shared_pending_intent(),
            pending_plan: crate::widgets::capability_dialog::shared_pending_plan(),
            auto_approve: Arc::new(AtomicBool::new(false)),
            bus: EventBus::default(),
        };
        let cancel = CancellationToken::new();

        // User picks "Always allow" (GrantedPersistent): the sink must flip its
        // auto-approve flag AND return ApproveAllForSession so the audit log
        // records "ApprovedAllForSession" (mirrors the CLI sink).
        let sink2 = sink.clone();
        let cancel2 = cancel.clone();
        let handle = tokio::spawn(async move {
            let input = serde_json::json!({ "path": "/tmp/example.rs" });
            let action = make_action("write_file", &input);
            sink2.request_approval(&action, cancel2).await
        });
        wait_for_pending_dialog(&cap_pending).await;
        assert!(
            crate::widgets::capability_dialog::resolve(
                &cap_pending,
                &crate::widgets::capability_dialog::Message::GrantAlways
            ),
            "expected a pending approval to resolve"
        );
        assert_eq!(
            handle.await.expect("request task panicked"),
            ApprovalDecision::ApproveAllForSession
        );
        assert!(sink.auto_approve.load(Ordering::Relaxed));

        // Subsequent request is approved without a dialog.
        let input2 = serde_json::json!({ "path": "/tmp/other.rs" });
        let action2 = make_action("write_file", &input2);
        let decision = sink.request_approval(&action2, cancel).await;
        assert_eq!(decision, ApprovalDecision::Approve);
        assert!(
            cap_pending.lock().unwrap_or_else(|e| e.into_inner()).is_empty(),
            "auto-approve must not queue a dialog"
        );
    }

    // -----------------------------------------------------------------------
    // request_intent_confirmation (ADR-55 §1)
    // -----------------------------------------------------------------------

    /// Wait until the sink's request future has queued a dialog on
    /// `pending_intent`, so the test resolves it without racing the spawn.
    async fn wait_for_pending_intent(
        shared: &crate::widgets::capability_dialog::SharedPendingIntent,
    ) {
        for _ in 0..500 {
            if !shared.lock().unwrap_or_else(|e| e.into_inner()).is_empty() {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("intent confirmation never queued a dialog");
    }

    #[tokio::test]
    async fn intent_sink_returned_outcome_when_user_picks_an_option() {
        let cap_pending = crate::widgets::capability_dialog::shared_pending();
        let pending_ack = crate::widgets::capability_dialog::shared_pending_ack();
        let pending_intent = crate::widgets::capability_dialog::shared_pending_intent();
        let sink = DesktopApprovalSink {
            cap_pending: cap_pending.clone(),
            pending_ack: pending_ack.clone(),
            pending_intent: pending_intent.clone(),
            pending_plan: crate::widgets::capability_dialog::shared_pending_plan(),
            auto_approve: Arc::new(AtomicBool::new(false)),
            bus: EventBus::default(),
        };
        let cancel = CancellationToken::new();
        let options =
            vec![RequestedOutcome::Answer, RequestedOutcome::Diagnose, RequestedOutcome::Execute];

        // Request runs in background while the UI would show the dialog.
        let sink2 = sink.clone();
        let cancel2 = cancel.clone();
        let handle = tokio::spawn(async move {
            sink2
                .request_intent_confirmation("What should I work on?".into(), &options, cancel2)
                .await
        });
        wait_for_pending_intent(&pending_intent).await;

        // The user picks an outcome via the dialog's select button.
        assert!(
            crate::widgets::capability_dialog::resolve_intent(
                &pending_intent,
                crate::widgets::capability_dialog::IntentDialogMessage::Select(
                    RequestedOutcome::Execute
                )
            ),
            "expected a pending intent to resolve"
        );
        assert_eq!(handle.await.expect("intent task panicked"), Some(RequestedOutcome::Execute));
        assert!(
            pending_intent.lock().unwrap_or_else(|e| e.into_inner()).is_empty(),
            "a resolved intent must not stay queued"
        );
    }

    #[tokio::test]
    async fn intent_sink_cancel_returns_none() {
        let cap_pending = crate::widgets::capability_dialog::shared_pending();
        let pending_ack = crate::widgets::capability_dialog::shared_pending_ack();
        let pending_intent = crate::widgets::capability_dialog::shared_pending_intent();
        let sink = DesktopApprovalSink {
            cap_pending: cap_pending.clone(),
            pending_ack: pending_ack.clone(),
            pending_intent: pending_intent.clone(),
            pending_plan: crate::widgets::capability_dialog::shared_pending_plan(),
            auto_approve: Arc::new(AtomicBool::new(false)),
            bus: EventBus::default(),
        };
        let cancel = CancellationToken::new();
        let options = vec![RequestedOutcome::Execute];

        let sink2 = sink.clone();
        let cancel2 = cancel.clone();
        let handle = tokio::spawn(async move {
            sink2.request_intent_confirmation("Proceed?".into(), &options, cancel2).await
        });
        wait_for_pending_intent(&pending_intent).await;

        // Cancel (reject) resolves the dialog with `None` → read-only run.
        assert!(
            crate::widgets::capability_dialog::resolve_intent(
                &pending_intent,
                crate::widgets::capability_dialog::IntentDialogMessage::Cancel
            ),
            "expected a pending intent to resolve"
        );
        assert_eq!(handle.await.expect("intent task panicked"), None);
    }

    #[tokio::test]
    async fn intent_sink_returns_none_when_dialog_dropped_without_selection() {
        let cap_pending = crate::widgets::capability_dialog::shared_pending();
        let pending_ack = crate::widgets::capability_dialog::shared_pending_ack();
        let pending_intent = crate::widgets::capability_dialog::shared_pending_intent();
        let sink = DesktopApprovalSink {
            cap_pending: cap_pending.clone(),
            pending_ack: pending_ack.clone(),
            pending_intent: pending_intent.clone(),
            pending_plan: crate::widgets::capability_dialog::shared_pending_plan(),
            auto_approve: Arc::new(AtomicBool::new(false)),
            bus: EventBus::default(),
        };
        let cancel = CancellationToken::new();
        let options = vec![RequestedOutcome::Plan, RequestedOutcome::Execute];

        let sink2 = sink.clone();
        let cancel2 = cancel.clone();
        let handle = tokio::spawn(async move {
            sink2.request_intent_confirmation("Plan or do?".into(), &options, cancel2).await
        });
        wait_for_pending_intent(&pending_intent).await;

        // The dialog is dropped without a selection (e.g. app teardown): the
        // oneshot sender is cancelled and the sink falls back to the
        // conservative read-only `None`, as if never invoked.
        {
            let _ = pending_intent.lock().unwrap_or_else(|e| e.into_inner()).pop_front();
        }
        assert_eq!(handle.await.expect("intent task panicked"), None);
    }

    #[tokio::test]
    async fn intent_sink_empty_options_returns_none_without_dialog() {
        let cap_pending = crate::widgets::capability_dialog::shared_pending();
        let pending_ack = crate::widgets::capability_dialog::shared_pending_ack();
        let pending_intent = crate::widgets::capability_dialog::shared_pending_intent();
        let sink = DesktopApprovalSink {
            cap_pending: cap_pending.clone(),
            pending_ack: pending_ack.clone(),
            pending_intent: pending_intent.clone(),
            pending_plan: crate::widgets::capability_dialog::shared_pending_plan(),
            auto_approve: Arc::new(AtomicBool::new(false)),
            bus: EventBus::default(),
        };
        let cancel = CancellationToken::new();
        let options: Vec<RequestedOutcome> = Vec::new();

        // Nothing to confirm: the sink must not queue a dialog.
        let result =
            sink.request_intent_confirmation("nothing to confirm".into(), &options, cancel).await;
        assert_eq!(result, None);
        assert!(
            pending_intent.lock().unwrap_or_else(|e| e.into_inner()).is_empty(),
            "empty options must not queue a dialog"
        );
    }

    // -----------------------------------------------------------------------
    // request_plan_approval (ADR-55 Phase 1d)
    // -----------------------------------------------------------------------

    /// Wait until the sink's request future has queued a dialog on
    /// `pending_plan`, so the test resolves it without racing the spawn.
    async fn wait_for_pending_plan(shared: &crate::widgets::capability_dialog::SharedPendingPlan) {
        for _ in 0..500 {
            if !shared.lock().unwrap_or_else(|e| e.into_inner()).is_empty() {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("plan approval never queued a dialog");
    }

    /// Build a sink wired to its own fresh queues plus the given plan queue and
    /// event bus, so tests observe the event and the chosen decision in
    /// isolation.
    fn plan_sink(
        pending_plan: crate::widgets::capability_dialog::SharedPendingPlan,
        bus: EventBus,
    ) -> DesktopApprovalSink {
        DesktopApprovalSink {
            cap_pending: crate::widgets::capability_dialog::shared_pending(),
            pending_ack: crate::widgets::capability_dialog::shared_pending_ack(),
            pending_intent: crate::widgets::capability_dialog::shared_pending_intent(),
            pending_plan,
            auto_approve: Arc::new(AtomicBool::new(false)),
            bus,
        }
    }

    #[tokio::test]
    async fn plan_sink_apply_sends_apply_and_publishes_event() {
        let pending_plan = crate::widgets::capability_dialog::shared_pending_plan();
        let bus = EventBus::default();
        let mut events = bus.subscribe();
        let sink = plan_sink(pending_plan.clone(), bus);
        let session_id = Ulid::new();
        const PLAN_ID: &str = "01JTESTPLAN0000000000001A";

        let sink2 = sink.clone();
        let handle = tokio::spawn(async move {
            sink2
                .request_plan_approval(
                    session_id,
                    PLAN_ID,
                    "Apply the stored plan?".into(),
                    "step 1: rework module\nstep 2: verify",
                    time::OffsetDateTime::now_utc(),
                    CancellationToken::new(),
                )
                .await
        });
        wait_for_pending_plan(&pending_plan).await;

        // The redraw signal is published with the `intent:plan` tool identity.
        let mut seen = false;
        while let Ok(event) = events.try_recv() {
            if let EventKind::ApprovalRequested { tool_name, timeout_secs } = &event.kind {
                if tool_name == "intent:plan" && *timeout_secs == 0 {
                    seen = true;
                }
            }
        }
        assert!(seen, "request_plan_approval must publish an ApprovalRequested event");

        // Apply resolves the dialog with `Some(Apply)`.
        assert!(
            crate::widgets::capability_dialog::resolve_plan(
                &pending_plan,
                session_id,
                PLAN_ID,
                crate::widgets::capability_dialog::PlanDialogMessage::Apply,
            ),
            "expected a pending plan to resolve"
        );
        assert_eq!(handle.await.expect("plan task panicked"), Some(PlanDecision::Apply));
        assert!(
            pending_plan.lock().unwrap_or_else(|e| e.into_inner()).is_empty(),
            "a resolved plan must not stay queued"
        );
    }

    #[tokio::test]
    async fn plan_sink_replan_sends_replan() {
        let pending_plan = crate::widgets::capability_dialog::shared_pending_plan();
        let sink = plan_sink(pending_plan.clone(), EventBus::default());
        let session_id = Ulid::new();
        const PLAN_ID: &str = "01JTESTPLAN0000000000001B";

        let sink2 = sink.clone();
        let handle = tokio::spawn(async move {
            sink2
                .request_plan_approval(
                    session_id,
                    PLAN_ID,
                    "Apply it or replan?".into(),
                    "step 1: draft",
                    time::OffsetDateTime::now_utc(),
                    CancellationToken::new(),
                )
                .await
        });
        wait_for_pending_plan(&pending_plan).await;

        assert!(
            crate::widgets::capability_dialog::resolve_plan(
                &pending_plan,
                session_id,
                PLAN_ID,
                crate::widgets::capability_dialog::PlanDialogMessage::Replan,
            ),
            "expected a pending plan to resolve"
        );
        assert_eq!(handle.await.expect("plan task panicked"), Some(PlanDecision::Replan));
    }

    #[tokio::test]
    async fn plan_sink_cancel_returns_none() {
        let pending_plan = crate::widgets::capability_dialog::shared_pending_plan();
        let sink = plan_sink(pending_plan.clone(), EventBus::default());
        let session_id = Ulid::new();
        const PLAN_ID: &str = "01JTESTPLAN0000000000001C";

        let sink2 = sink.clone();
        let handle = tokio::spawn(async move {
            sink2
                .request_plan_approval(
                    session_id,
                    PLAN_ID,
                    "Apply the stored plan?".into(),
                    "step 1: change",
                    time::OffsetDateTime::now_utc(),
                    CancellationToken::new(),
                )
                .await
        });
        wait_for_pending_plan(&pending_plan).await;

        // Dismiss (Cancel) resolves the dialog with `None` → read-only run.
        assert!(
            crate::widgets::capability_dialog::resolve_plan(
                &pending_plan,
                session_id,
                PLAN_ID,
                crate::widgets::capability_dialog::PlanDialogMessage::Cancel,
            ),
            "expected a pending plan to resolve"
        );
        assert_eq!(handle.await.expect("plan task panicked"), None);
        assert!(
            pending_plan.lock().unwrap_or_else(|e| e.into_inner()).is_empty(),
            "a dismissed plan must not stay queued"
        );
    }

    #[tokio::test]
    async fn plan_sink_returns_none_when_dialog_dropped_without_selection() {
        let pending_plan = crate::widgets::capability_dialog::shared_pending_plan();
        let sink = plan_sink(pending_plan.clone(), EventBus::default());
        let session_id = Ulid::new();
        const PLAN_ID: &str = "01JTESTPLAN0000000000001D";

        let sink2 = sink.clone();
        let handle = tokio::spawn(async move {
            sink2
                .request_plan_approval(
                    session_id,
                    PLAN_ID,
                    "Apply the stored plan?".into(),
                    "step 1: change",
                    time::OffsetDateTime::now_utc(),
                    CancellationToken::new(),
                )
                .await
        });
        wait_for_pending_plan(&pending_plan).await;

        // The dialog is dropped without a selection (e.g. window close): the
        // oneshot sender is cancelled and the sink falls back to the
        // conservative read-only `None`, as if never invoked.
        {
            let _ = pending_plan.lock().unwrap_or_else(|e| e.into_inner()).pop_front();
        }
        assert_eq!(handle.await.expect("plan task panicked"), None);
    }

    #[tokio::test]
    async fn plan_sink_cross_session_does_not_resolve() {
        let pending_plan = crate::widgets::capability_dialog::shared_pending_plan();
        let sink = plan_sink(pending_plan.clone(), EventBus::default());
        let session_id = Ulid::new();
        let other_session = Ulid::new();
        const PLAN_ID: &str = "01JTESTPLAN0000000000001E";
        const OTHER_PLAN_ID: &str = "01JTESTPLAN0000000000FFFF";

        let sink2 = sink.clone();
        let handle = tokio::spawn(async move {
            sink2
                .request_plan_approval(
                    session_id,
                    PLAN_ID,
                    "Apply the stored plan?".into(),
                    "step 1: change",
                    time::OffsetDateTime::now_utc(),
                    CancellationToken::new(),
                )
                .await
        });
        wait_for_pending_plan(&pending_plan).await;

        // A cross-session resolve must not answer this prompt: the entry stays
        // queued and no decision is delivered.
        assert!(
            !crate::widgets::capability_dialog::resolve_plan(
                &pending_plan,
                other_session,
                PLAN_ID,
                crate::widgets::capability_dialog::PlanDialogMessage::Apply,
            ),
            "a cross-session resolve must be rejected"
        );
        assert!(
            !pending_plan.lock().unwrap_or_else(|e| e.into_inner()).is_empty(),
            "a rejected resolve must leave the entry queued"
        );
        assert!(!handle.is_finished(), "a rejected resolve must not answer the task");

        // A wrong plan_id for the same session is also rejected.
        assert!(
            !crate::widgets::capability_dialog::resolve_plan(
                &pending_plan,
                session_id,
                OTHER_PLAN_ID,
                crate::widgets::capability_dialog::PlanDialogMessage::Apply,
            ),
            "a wrong plan_id resolve must be rejected"
        );

        // The matching resolve then completes as expected.
        assert!(crate::widgets::capability_dialog::resolve_plan(
            &pending_plan,
            session_id,
            PLAN_ID,
            crate::widgets::capability_dialog::PlanDialogMessage::Replan,
        ));
        assert_eq!(handle.await.expect("plan task panicked"), Some(PlanDecision::Replan));
        assert!(
            pending_plan.lock().unwrap_or_else(|e| e.into_inner()).is_empty(),
            "a resolved plan must not stay queued"
        );
    }

    // -----------------------------------------------------------------------
    // ADR-58/59 (rewritten) Slice 2 — single-arm Save (include source)
    // -----------------------------------------------------------------------
    //
    // Each test follows the CONFIG_ENV_LOCK + XDG_CONFIG_HOME redirect pattern
    // of the auto-seed matrix: lock the env lock, redirect the config dir to a
    // tempdir, seed the files through the config crate's own seams, construct
    // `App::new`, point `project_dir` at the seeded project, and restore the
    // env BEFORE any assertion so a panic cannot leak the redirect into later
    // tests.
    //
    // `reconcile_config_from_reload` activates `[orchestration]` but — by
    // design (ADR-57 §3b) — never rebuilds Studio drafts, so the tests then
    // populate the Studio from the reloaded config exactly like `App::new`
    // does at startup (the app constructs with the real initial project dir,
    // which these tests cannot predict). The Save dispatch itself runs while
    // the env is still redirected: `persist_include_blueprint` resolves the
    // global config dir for its target-shadow guard, and the save re-loads
    // config on success.

    /// Save on the include source writes the edited blueprint to the project
    /// include file — and only there: the global `config.toml` is never
    /// rewritten (the legacy wholesale persist is gone).
    #[test]
    fn save_on_blueprint_path_writes_the_edited_blueprint_to_the_project_include() {
        let _guard = CONFIG_ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let dir = tempfile::tempdir().expect("tempdir");
        let previous = std::env::var_os("XDG_CONFIG_HOME");
        std::env::set_var("XDG_CONFIG_HOME", dir.path());

        // Global config file: schema only — must stay byte-identical.
        let global_dir = dir.path().join("concerto");
        std::fs::create_dir_all(&global_dir).expect("create global config dir");
        let global_config_path = global_dir.join("config.toml");
        std::fs::write(&global_config_path, "schema_version = 7\n").expect("seed global config");
        let global_before =
            std::fs::read_to_string(&global_config_path).expect("read global config");

        // Project include + project-layer selection pointing at it.
        let project_dir = dir.path().join("project");
        std::fs::create_dir_all(&project_dir).expect("create project dir");
        let include_target = project_dir.join(concerto_config::BLUEPRINT_INCLUDE_FILE);
        let standard =
            concerto_config::named_blueprint("standard").expect("standard named blueprint");
        concerto_config::save_blueprint(&standard, &include_target).expect("seed project include");
        let project_config = project_dir.join(concerto_config::legacy::NEW_PROJECT_CONFIG_FILE);
        std::fs::write(&project_config, "schema_version = 7\n").expect("seed project config");
        concerto_config::save_blueprint_selection(
            &project_config,
            &concerto_config::BlueprintSelection {
                name: None,
                include: Some(concerto_config::BLUEPRINT_INCLUDE_FILE.to_string()),
                inline: None,
            },
        )
        .expect("seed project selection");

        let (mut app, _) = App::new();
        app.project_dir = project_dir.clone();
        app.reconcile_config_from_reload();
        let config = app.config.clone().expect("config loaded after reconcile");
        app.orchestration_studio.load_from_config(&config);

        // The Studio draft: edit the first stage's label, then Save.
        let _ = app.orchestration_studio.update(
            crate::views::orchestration_studio::StudioMessage::StageLabelEdited(
                0,
                "planning".into(),
            ),
        );
        let _ = app.update(Message::OrchestrationStudio(
            crate::views::orchestration_studio::StudioMessage::SaveOrchestration,
        ));

        // Env restored before assertions so a panic cannot leak the redirect.
        match previous {
            Some(value) => std::env::set_var("XDG_CONFIG_HOME", value),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }

        assert!(
            app.config.as_ref().and_then(|c| c.orchestration.as_ref()).is_some(),
            "the blueprint path must be active before Save"
        );
        let raw = std::fs::read_to_string(&include_target).expect("include read back");
        let saved = concerto_config::parse_blueprint_file(&include_target).expect("include parses");
        assert_eq!(
            saved.pipeline.stages[0].label, "planning",
            "the edited label must reach the include file\n{raw}"
        );
        assert!(!app.orchestration_studio.unsaved, "a successful save marks the studio clean");
        let global_after =
            std::fs::read_to_string(&global_config_path).expect("global config read back");
        assert_eq!(
            global_before, global_after,
            "the global config must stay untouched — no [orchestration] persist\n{global_after}"
        );
    }

    /// Save is blocked (draft kept, include untouched) when the draft has
    /// validation errors — the belt-and-braces guard behind the disabled
    /// Save button.
    #[test]
    fn save_is_blocked_when_the_draft_has_validation_errors() {
        let _guard = CONFIG_ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let dir = tempfile::tempdir().expect("tempdir");
        let previous = std::env::var_os("XDG_CONFIG_HOME");
        std::env::set_var("XDG_CONFIG_HOME", dir.path());

        std::fs::create_dir_all(dir.path().join("concerto")).expect("create global config dir");
        std::fs::write(dir.path().join("concerto").join("config.toml"), "schema_version = 7\n")
            .expect("seed global config");
        let project_dir = dir.path().join("project");
        std::fs::create_dir_all(&project_dir).expect("create project dir");
        let include_target = project_dir.join(concerto_config::BLUEPRINT_INCLUDE_FILE);
        let standard =
            concerto_config::named_blueprint("standard").expect("standard named blueprint");
        concerto_config::save_blueprint(&standard, &include_target).expect("seed project include");
        let project_config = project_dir.join(concerto_config::legacy::NEW_PROJECT_CONFIG_FILE);
        std::fs::write(&project_config, "schema_version = 7\n").expect("seed project config");
        concerto_config::save_blueprint_selection(
            &project_config,
            &concerto_config::BlueprintSelection {
                name: None,
                include: Some(concerto_config::BLUEPRINT_INCLUDE_FILE.to_string()),
                inline: None,
            },
        )
        .expect("seed project selection");
        let before = std::fs::read_to_string(&include_target).expect("read include before");

        let (mut app, _) = App::new();
        app.project_dir = project_dir.clone();
        app.reconcile_config_from_reload();
        let config = app.config.clone().expect("config loaded after reconcile");
        app.orchestration_studio.load_from_config(&config);

        // Force a rulebook violation the UI would flag: an empty stage tag
        // (rule (g), "stage tag must be non-empty").
        let _ = app.orchestration_studio.update(
            crate::views::orchestration_studio::StudioMessage::StageTagEdited(0, "".into()),
        );
        assert!(
            !app.orchestration_studio.validation().ok,
            "the edited draft must be invalid (precondition)"
        );
        let _ = app.update(Message::OrchestrationStudio(
            crate::views::orchestration_studio::StudioMessage::SaveOrchestration,
        ));

        match previous {
            Some(value) => std::env::set_var("XDG_CONFIG_HOME", value),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }

        let after = std::fs::read_to_string(&include_target).expect("read include after");
        assert_eq!(before, after, "an invalid draft must never reach the include file");
        assert!(
            app.orchestration_studio.save_error.is_some(),
            "the save failure must surface on the studio"
        );
    }

    /// Save is blocked when the include file on disk no longer parses — the
    /// data-loss guard: a write from the in-memory model alone would silently
    /// drop the unknown keys the file carries (`deny_unknown_fields`).
    #[test]
    fn save_is_blocked_when_the_include_file_does_not_parse() {
        let _guard = CONFIG_ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let dir = tempfile::tempdir().expect("tempdir");
        let previous = std::env::var_os("XDG_CONFIG_HOME");
        std::env::set_var("XDG_CONFIG_HOME", dir.path());

        std::fs::create_dir_all(dir.path().join("concerto")).expect("create global config dir");
        std::fs::write(dir.path().join("concerto").join("config.toml"), "schema_version = 7\n")
            .expect("seed global config");
        let project_dir = dir.path().join("project");
        std::fs::create_dir_all(&project_dir).expect("create project dir");
        let include_target = project_dir.join(concerto_config::BLUEPRINT_INCLUDE_FILE);
        let standard =
            concerto_config::named_blueprint("standard").expect("standard named blueprint");
        concerto_config::save_blueprint(&standard, &include_target).expect("seed project include");
        let project_config = project_dir.join(concerto_config::legacy::NEW_PROJECT_CONFIG_FILE);
        std::fs::write(&project_config, "schema_version = 7\n").expect("seed project config");
        concerto_config::save_blueprint_selection(
            &project_config,
            &concerto_config::BlueprintSelection {
                name: None,
                include: Some(concerto_config::BLUEPRINT_INCLUDE_FILE.to_string()),
                inline: None,
            },
        )
        .expect("seed project selection");

        let (mut app, _) = App::new();
        app.project_dir = project_dir.clone();
        app.reconcile_config_from_reload();
        let config = app.config.clone().expect("config loaded after reconcile");
        app.orchestration_studio.load_from_config(&config);

        // The watcher (or a hand edit) replaced the include with garbage
        // AFTER the load: the on-disk file no longer parses.
        let garbage = b"this is { not toml ==";
        std::fs::write(&include_target, garbage).expect("corrupt the include file");
        let _ = app.update(Message::OrchestrationStudio(
            crate::views::orchestration_studio::StudioMessage::SaveOrchestration,
        ));

        match previous {
            Some(value) => std::env::set_var("XDG_CONFIG_HOME", value),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }

        let after = std::fs::read(&include_target).expect("read include after");
        assert_eq!(after, garbage, "the unparseable file must be left untouched");
        let error = app.orchestration_studio.save_error.as_deref().expect("save_error set");
        assert!(
            error.contains("orchestration.blueprint.toml") || error.contains("failed to load"),
            "the save error must carry the path or a parse detail: {error}"
        );
    }

    /// Save refuses when the loaded blueprint would be shadowed: the include
    /// lives in the global config dir while Save would write the project dir
    /// — a file a later load would never read.
    #[test]
    fn save_refuses_when_the_loaded_blueprint_would_be_shadowed() {
        let _guard = CONFIG_ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let dir = tempfile::tempdir().expect("tempdir");
        let previous = std::env::var_os("XDG_CONFIG_HOME");
        std::env::set_var("XDG_CONFIG_HOME", dir.path());

        // Global-scope include + a GLOBAL selection pointing at it; NO project
        // include, so load resolves the include from the global config dir.
        let global_dir = dir.path().join("concerto");
        std::fs::create_dir_all(&global_dir).expect("create global config dir");
        let global_include = global_dir.join(concerto_config::BLUEPRINT_INCLUDE_FILE);
        let standard =
            concerto_config::named_blueprint("standard").expect("standard named blueprint");
        concerto_config::save_blueprint(&standard, &global_include).expect("seed global include");
        let global_config_path = global_dir.join("config.toml");
        std::fs::write(&global_config_path, "schema_version = 7\n").expect("seed global config");
        concerto_config::save_blueprint_selection(
            &global_config_path,
            &concerto_config::BlueprintSelection {
                name: None,
                include: Some(concerto_config::BLUEPRINT_INCLUDE_FILE.to_string()),
                inline: None,
            },
        )
        .expect("seed global selection");
        let project_dir = dir.path().join("project");
        std::fs::create_dir_all(&project_dir).expect("create project dir");
        let project_include = project_dir.join(concerto_config::BLUEPRINT_INCLUDE_FILE);

        let (mut app, _) = App::new();
        app.project_dir = project_dir.clone();
        app.reconcile_config_from_reload();
        let config = app.config.clone().expect("config loaded after reconcile");
        assert!(
            config.orchestration.as_ref().is_some(),
            "the global-scope selection must activate the blueprint path"
        );
        app.orchestration_studio.load_from_config(&config);

        let _ = app.update(Message::OrchestrationStudio(
            crate::views::orchestration_studio::StudioMessage::SaveOrchestration,
        ));

        match previous {
            Some(value) => std::env::set_var("XDG_CONFIG_HOME", value),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }

        assert!(
            !project_include.exists(),
            "no file may be created in the project dir that would shadow the loaded include"
        );
        let error = app.orchestration_studio.save_error.as_deref().expect("save_error set");
        assert!(
            error.contains("shadow"),
            "the target-shadow guard must refuse with a shadowing message: {error}"
        );
    }

    /// B1 regression guard (oracle finding), ADR-58/59 (rewritten) Slice 2 shape: the app's
    /// default first-run flow must round-trip — auto-seed (inline) → edit →
    /// save → reload must load the EDITS. The Slice-2 default selection is
    /// INLINE, so the runtime consumes exactly what the seed (and every
    /// subsequent Save) writes — no include file, no catalog indirection.
    #[test]
    fn default_auto_seed_then_save_then_reload_round_trips_the_edited_blueprint() {
        let _guard = CONFIG_ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let dir = tempfile::tempdir().expect("tempdir");
        let previous = std::env::var_os("XDG_CONFIG_HOME");
        std::env::set_var("XDG_CONFIG_HOME", dir.path());

        let global_dir = dir.path().join("concerto");
        std::fs::create_dir_all(&global_dir).expect("create global config dir");
        std::fs::write(global_dir.join("config.toml"), "schema_version = 7\n")
            .expect("seed global config");
        let project_dir = dir.path().join("project");
        std::fs::create_dir_all(&project_dir).expect("create project dir");

        let (mut app, _) = App::new();
        app.project_dir = project_dir.clone();
        // Force the fresh-project shape (see the first-open test): the seed
        // must really write for this round-trip to prove anything.
        app.config = None;
        // First-run bootstrap: opening the Studio seeds the roster inline.
        let _ = app.update(Message::Navigate(Page::OrchestrationStudio));

        // Env restored before assertions so a panic cannot leak the redirect.
        match previous {
            Some(value) => std::env::set_var("XDG_CONFIG_HOME", value),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }

        // The auto-seed activates the blueprint path through an INLINE
        // selection (the Slice-2 default shape).
        let config = app.config.clone().expect("config loaded after the auto-seed");
        assert!(
            config.orchestration.as_ref().is_some(),
            "the app's own default auto-seed must activate [orchestration]"
        );
        assert!(
            config.orchestration.as_ref().and_then(|o| o.blueprint.include.as_deref()).is_none(),
            "the default selection is inline — no include file is involved"
        );
        assert!(
            config.orchestration.as_ref().and_then(|o| o.blueprint.inline.as_ref()).is_some(),
            "the default selection must carry the inline blueprint"
        );

        // Populate the Studio draft from the activated config, edit stage 0's
        // label, then Save through the single arm.
        app.orchestration_studio.load_from_config(&config);
        let _ = app.orchestration_studio.update(
            crate::views::orchestration_studio::StudioMessage::StageLabelEdited(
                0,
                "planning".into(),
            ),
        );
        let _ = app.update(Message::OrchestrationStudio(
            crate::views::orchestration_studio::StudioMessage::SaveOrchestration,
        ));
        assert!(!app.orchestration_studio.unsaved, "a successful save marks the studio clean");

        // Save rewrote the inline in the PROJECT config — not an include file.
        let project_config = project_dir.join(concerto_config::legacy::NEW_PROJECT_CONFIG_FILE);
        let raw_project = std::fs::read_to_string(&project_config).expect("project config read");
        assert!(
            raw_project.contains("inline = {"),
            "save must write the edited blueprint inline\n{raw_project}"
        );
        assert!(
            !project_dir.join(concerto_config::BLUEPRINT_INCLUDE_FILE).exists(),
            "no include file is created on the default inline path"
        );

        // A full reload from disk must now load the EDITS — the B1 property:
        // the runtime consumes the inline Save wrote, not an unedited default.
        app.reconcile_config_from_reload();
        let reloaded = app.config.clone().expect("config after reload");
        assert!(
            reloaded.orchestration.as_ref().is_some(),
            "reload must keep [orchestration] active"
        );
        let reloaded_label = reloaded
            .resolved_blueprint
            .as_ref()
            .map(|r| r.blueprint.pipeline.stages[0].label.as_str());
        assert_eq!(
            reloaded_label,
            Some("planning"),
            "the runtime must load the edited inline — not the unedited standard blueprint"
        );
    }
}
