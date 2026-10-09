use iced::widget::{button, column, container, mouse_area, row, rule, stack, text, text_input};
use iced::{Element, Length};

use crate::root_consent;
use crate::shortcuts;
use crate::theme::AppTheme;
use crate::views;
use crate::widgets::agent_graph::NodeState;
use crate::widgets::capability_dialog;
use crate::widgets::circuit_background;
use crate::widgets::scanline_overlay;

use crate::services::session_handler::DesktopSessionHandler;
use crate::views::memory::MemoryStatus;
use crate::views::spend::CapUiState;
use crate::views::studio_runtime::{
    CheckpointStudioRuntimeReader, StudioRuntimeReader, StudioRuntimeSnapshot,
};
use camino::Utf8PathBuf;
use concerto_config::AppConfig;
use concerto_config::CredentialStore;
use concerto_core::event::EventBus;
// Only `mod tests` needs `ThinkingKind` directly now: the transcript mapping
// that used it moved to `views::chat::entries_from_transcript`.
#[cfg(test)]
use concerto_core::event::ThinkingKind;
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
use concerto_plugins::manager::SharedPluginManager;
use concerto_providers::factory::ProviderFactory;
use concerto_providers::provider_defs::{
    picker_model_options, provider_definition, provider_readiness,
};
use concerto_tools::diff::compute_diffs_from_virtual_fs;
use concerto_tools::virtual_fs::VirtualFs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::ui::feedback::{ToastLevel, ToastManager};

/// Passive event-stream wiring (keyboard shortcuts, event-bus relay,
/// terminal panel, animation ticks, config watch) lives in the
/// `subscription` submodule so the root `impl App` block in this file
/// stays focused on `update`/`view`.
mod subscription;

/// Approval-sink plumbing (`DesktopApprovalSink`, its session ctor, and the
/// run-summary formatter) lives in the `approval_sink` submodule so the
/// root `impl App` blocks in this file stay focused on `update`/`view`.
mod approval_sink;
use approval_sink::{desktop_approval_sink, format_run_summary};
// Only `mod tests` builds `DesktopApprovalSink` directly (field-level
// setup); every other reference goes through the ctor's trait object.
#[cfg(test)]
use approval_sink::DesktopApprovalSink;

/// Project-orchestration seed/persist cluster (first-open roster/blueprint
/// auto-seed, the Studio's single-arm Save with its guarded include write,
/// and the global-only enforcement banner + explicit import action) lives in
/// the `orchestration_persist` submodule so the root `impl App` block in this
/// file stays focused on `update`/`view`.
mod orchestration_persist;

/// Pure view sections extracted from `App::view()` (NORM S29): the 9-way
/// system-dialog stack + ambient circuit/scanline backgrounds in
/// `view_dialogs`, and the status/context/toast column + terminal bottom
/// panel in `view_panels`. Both are `&self`-only builders (no `Task`, no
/// locks, no mutation); `App::view()` keeps composition only.
mod view_dialogs;
mod view_panels;

/// Trivial arm groups extracted from the root `update` match (NORM S30):
/// run-cancel + animation ticks, the `AgentGraph`/`Terminal`
/// passthroughs, the prefs-theme/help flip, and the screenshot/toast/
/// quick/memory/terminal/git tail. Bodies moved verbatim; each group is
/// one `pub(super)` method taking the full [`Message`] so every arm keeps
/// its exact early-return `Task` semantics (same pattern as
/// `views::settings::update_mcp`). The parent `update` keeps one thin
/// delegating arm per group; the tests in `mod tests` stay put untouched.
mod simple_updates;

/// The project-switch cluster extracted from the root `update` match
/// (NORM S31): the dir-picker open/input/cancel/apply flow, the ADR-44
/// §4 root-consent gate, and the sidebar tree's expand / lazy-session /
/// session-click arms, plus the `switch_project_dir` hub helper they all
/// call. Bodies moved verbatim; the group is one `pub(super)` method
/// taking the full [`Message`] so every arm keeps its exact early-return
/// `Task` semantics. The parent `update` keeps one thin delegating arm;
/// `rebuild_project_tree` / `load_sessions_for_project` moved to the
/// `loaders` submodule in S46 (boot and post-run refresh also call them,
/// through `app.*` / `self.*`). The tests in `mod tests` stay put
/// untouched.
mod project_switch;

/// The thin-delegation arms extracted from the root `update` match (NORM
/// S37): the orchestration wrappers (banner import/dismiss, Studio dispatch
/// + guarded Save, runtime-snapshot load), the `Editor` dispatch arm, the
/// capability/ack/intent/plan dialog resolvers over the shared pending
/// queues, and the `ToolLog` child-view passthrough. Bodies moved verbatim;
/// each group is one `pub(super)` method taking the full [`Message`] so
/// every arm keeps its exact early-return `Task` semantics (same pattern as
/// `views::settings::update_mcp`). The parent `update` keeps one thin
/// delegating arm per group; the tests in `mod tests` stay put untouched.
mod delegated_updates;

/// The backend-event and model/config arms extracted from the root
/// `update` match (NORM S38): the `DesktopEvent` arm (App-level spend /
/// cap / run-stage chip updates, the `ErrorOccurred` toast, and the
/// `route_event` fan-out into the chat / tool-log / agent-graph / memory
/// view states) and the active provider/model selection + external
/// config-reload arms (`SetActiveProvider`, `SetActiveModel`,
/// `SetAgentModel`, `ConfigReloaded`). Bodies moved verbatim; each group
/// is one `pub(super)` method taking the full [`Message`] so every arm
/// keeps its exact early-return `Task` semantics (same pattern as
/// `views::settings::update_mcp`). The parent `update` keeps one thin
/// delegating arm per group; the tests in `mod tests` stay put untouched.
mod event_model_updates;

/// The view-state dispatch arms extracted from the root `update` match
/// (NORM S39): the `Chat` arm's pure-delegation tail (its clipboard /
/// navigation / New Session / focus / toggle pre-routing moved to the sibling
/// `update_routing` submodule in S42), the `Diff` dispatch arm (`diff.update`
/// plus the conditional VFS commit for accept/reject/undo decisions), and the
/// `Memory` dispatch arm (the child-view passthrough plus the
/// reindex/refresh/search/delete loader dispatches). Bodies moved verbatim;
/// each group is one `pub(super)` method taking the full [`Message`] so every
/// arm keeps its exact early-return `Task` semantics (same pattern as
/// `views::settings::update_mcp`). The parent `update` keeps one thin
/// delegating arm per group; the tests in `mod tests` stay put untouched.
/// The `Shortcut` handler (`handle_shortcut`) was extracted separately in
/// S42 into `update_routing`: it only re-dispatches and flips small routing
/// flags, not deep state.
mod view_dispatch;

/// The Settings dispatch arm extracted from the root `update` match (NORM
/// S40): the `Settings(msg)` inner match — `ShellSecurityFinished`,
/// `SaveSettings` (persist + reload/reconcile + theme + studio model sync +
/// discovery + plugin refresh), `ProviderModelsRefreshed` (staleness guards
/// + config-side cache write + chat/studio syncs), `ThemeSelected` /
/// `FontSizeChanged`, `ProviderModelsRefreshRequested`, and the
/// `settings.update` fallback. Bodies moved verbatim; the group is one
/// `pub(super)` method taking the full [`Message`] so every arm keeps its
/// exact early-return `Task` semantics (same pattern as
/// `views::settings::update_mcp`). The parent `update` keeps one thin
/// delegating arm; the tests in `mod tests` stay put untouched.
mod settings_updates;

/// The navigation and session-select arms extracted from the root `update`
/// match (NORM S41): the `Navigate` arm (page switch + streaming/focus
/// clear + Settings config syncs + one-time skills discovery + Studio
/// seed/load/sync/runtime load), the `SetSubView` / `OpenSpendLog` arms
/// (sub-view + page + overlay-fade target + Spend Log / Runtime modal
/// reloads), and the `SessionSelected` arm (session switch with sidecar
/// restore + durable-event replay fallback). Bodies moved verbatim; each
/// group is one `pub(super)` method taking the full [`Message`] so every
/// arm keeps its exact early-return `Task` semantics (same pattern as
/// `views::settings::update_mcp`). The parent `update` keeps one thin
/// delegating arm per group; the tests in `mod tests` stay put untouched.
mod navigation_updates;

/// The message-routing groups extracted from the root `update` match (NORM
/// S42): the `Chat` arm's pre-routing (clipboard write, the `Navigate*` /
/// `SetActiveModel` / `SelectSession` / `RefreshSpendLog` re-dispatches, the
/// `NewSession` reset, input-focus tracking, and the `SubmitInput` /
/// `ToggleMultiAgent` / `ToggleFastMode` / `ToggleMuteAgent` pre-work — its
/// un-intercepted fall-through still calls the S39 `update_chat_tail`) and the
/// `Shortcut` handler (`handle_shortcut`), which only re-dispatches into other
/// arms and flips a handful of small routing flags. Bodies moved verbatim;
/// each group is one `pub(super)` method taking the full [`Message`] so every
/// arm keeps its exact early-return `Task` semantics (same pattern as
/// `views::settings::update_mcp`). The parent `update` keeps one thin
/// delegating arm per group; the tests in `mod tests` stay put untouched.
mod update_routing;

/// The `AgentRunCompleted` arm extracted from the root `update` match (NORM
/// S43): the run-settled teardown (run-status/stage reset, deferred
/// memory-config flush, chat/agent-graph settle against the outcome, resume
/// checkpoint store/clear, transcript + active-agent-graph persist, VFS diff
/// reload, and the session-list + git-summary refresh batch). The body moved
/// verbatim; the group is one `pub(super)` method taking the full [`Message`]
/// so the arm keeps its exact `Task` semantics (same pattern as
/// `views::settings::update_mcp`). The parent `update` keeps one thin
/// delegating arm; the tests in `mod tests` stay put untouched.
mod run_completion;

/// The memory-result arms extracted from the root `update` match (NORM S44):
/// the `ReindexResult` outcome (memory status/loaded flags + the success-path
/// entry reload), the `MemoryEntriesLoaded` loader result, the
/// `PluginProvidersRefreshed` log-only placeholder, and `MemoryEntryDeleted`
/// (row removal / error state). Bodies moved verbatim; the group is one
/// `pub(super)` method taking the full [`Message`] so every arm keeps its
/// exact early-return `Task` semantics (same pattern as
/// `views::settings::update_mcp`). The parent `update` keeps one thin
/// delegating arm; the memory setters stay here, while the `trigger_reindex`
/// / `load_memory_entries` / `delete_memory_entry` Task factories moved to
/// the `loaders` submodule in S46. The tests in `mod tests` stay put
/// untouched.
mod memory_results;

/// The async loader Task-factory cluster (NORM S46): the Memory re-index /
/// graph / entries / delete loaders, the Git-summary refresh, the sidebar
/// session-list load with its synchronous project-tree rebuild, the session
/// resume, the Spend Log reload, and the read-only Studio runtime-snapshot
/// load. Bodies moved verbatim as `pub(super)` inherent methods, so the
/// existing `self.*` / `app.*` callers (root `update` arms, the sibling
/// submodule groups, and the `App::new` boot batch) resolve unchanged. The
/// synchronous spend/cap status-bar helpers and `load_diff_from_vfs` stay
/// here (they build no `Task`). The tests in `mod tests` stay put untouched.
mod loaders;

/// Shared text-focus state read by the keyboard-subscription fn pointer.
/// `keyboard::on_key_press` requires a bare fn (no captures), so we route
/// through a static rather than App's field.
static TEXT_FOCUSED: AtomicBool = AtomicBool::new(false);

/// Max reading width (logical px) for the centered chat column (`Page::Chat`).
const CHAT_MAX_WIDTH: f32 = 900.0;

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
    /// Advances the scanline overlay animation phase. Only dispatched while
    /// `scanline_overlay_enabled` is true and `page == Page::Chat`.
    ScanlineTick,
    /// One step (16 ms) of the shared overlay/terminal animation tick. Moves
    /// `overlay_fade` toward `overlay_fade_target` and `terminal_panel_anim`
    /// toward its open/closed target; the subscription stays active only
    /// while either animation is in flight.
    AnimTick,
    Chat(views::chat::Message),
    Diff(views::diff::Message),
    Memory(views::memory::Message),
    MemoryGraph(views::memory_graph::Message),
    ToolLog(views::tool_log::Message),
    Settings(views::settings::Message),
    AgentGraph(views::agent_graph::Message),
    Terminal(views::terminal::Message),
    OrchestrationStudio(views::orchestration_studio::StudioMessage),
    /// Read-only observability snapshot loaded from the session's persisted
    /// orchestration checkpoint when the Runtime modal (Ctrl+R) or the Studio
    /// opens. The `Option<Ulid>` is the session the load was for, so a result
    /// arriving after the active session changed is discarded.
    StudioRuntimeLoaded(Option<Ulid>, Box<StudioRuntimeSnapshot>),
    /// Explicit import action (global-only orchestration enforcement): copy
    /// the ignored project-layer orchestration keys
    /// (`[orchestration]`, `[multi_agent.custom_agents]`,
    /// `[multi_agent.model_pins]`) into the GLOBAL config via the
    /// merge-aware atomic seams, remove them from the project file, then
    /// reload. One user action, logged + toasted; never silent.
    ImportProjectOrchestration,
    /// Session-scoped dismissal of the project orchestration import banner:
    /// hidden until the next launch (not persisted).
    DismissOrchestrationBanner,
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
    /// Quick-swap one agent's model override from the right toolbar. Persists
    /// to the global config and re-derives runtime state (same seam the Studio
    /// uses), without opening Settings/Studio.
    SetAgentModel {
        agent_id: String,
        model: String,
    },
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
    /// Result of a post-SaveSettings plugin re-discovery pass against the
    /// retained plugin manager (log-only today; kept as a message so the task
    /// can later surface per-plugin outcomes without changing the wiring).
    PluginProvidersRefreshed,
    MemoryEntriesLoaded(Result<Vec<views::memory::MemoryRow>, String>),
    MemoryEntryDeleted {
        id: String,
        result: Result<(), String>,
    },
    /// ADR-69 slice 3 — the memory graph modal's load task finished.
    MemoryGraphLoaded(Result<concerto_memory::mermaid::MemoryGraph, String>),
    /// Open / close the read-only memory graph modal (from the Memory modal).
    OpenMemoryGraph,
    CloseMemoryGraph,
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
    /// User decision on the intent confirmation dialog (ADR-55 §2).
    IntentDialog(capability_dialog::IntentDialogMessage),
    /// User decision on the plan approval dialog (ADR-55 §4).
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
    pub memory_graph: views::memory_graph::State,
    pub tool_log: views::tool_log::State,
    pub settings: views::settings::State,
    pub agent_graph: views::agent_graph::State,
    pub terminal: views::terminal::State,
    pub orchestration_studio: views::orchestration_studio::State,
    pub editor: views::code_editor::State,

    // Capability approval dialog state
    pub cap_pending: capability_dialog::SharedPending,
    pub pending_ack: capability_dialog::SharedPendingAck,
    /// Pending intent confirmations (ADR-55 §2), FIFO queue.
    pub pending_intent: capability_dialog::SharedPendingIntent,
    /// Pending plan approvals (ADR-55 §4), FIFO queue.
    pub pending_plan: capability_dialog::SharedPendingPlan,

    pub bus: concerto_core::event::EventBus,
    pub config: Option<concerto_config::AppConfig>,
    /// Unmerged user-level settings. Global writes always use this layer as
    /// their base so project/env overrides cannot leak into `config.toml`.
    pub global_config: concerto_config::AppConfig,
    /// Project-scoped memory services (store, indexer, sync, cancel).
    /// Switched when the active project changes.
    pub memory_services: Arc<Mutex<Option<ActiveMemoryServices>>>,
    /// Process-lifetime WASM plugin-manager handle (plugin liveness). Passed
    /// to every run's `ServicesBuilder` and shared with the Settings state so
    /// the revoke and provider re-refresh paths act on the same plugin
    /// instances a running agent uses. The manager itself is materialised on
    /// the first agent run (its epoch ticker needs a tokio runtime context).
    pub plugin_manager: SharedPluginManager,
    pub cancel_token: concerto_core::CancellationToken,
    pub run_status: RunStatus,
    /// Current intent-router stage of the active run (ADR-55 §9),
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
    /// Scanline overlay: default-off feature flag. When true a faint
    /// horizontal scan-line pattern renders behind the chat column.
    /// Derived from `display.scanline_overlay_enabled` (see
    /// `reconcile_config_from_reload`); toggled in Settings → Display.
    pub scanline_overlay_enabled: bool,
    /// Reduced-motion override (`display.reduced_motion`, default false).
    /// Mirrored into the chat view (skips scan-line pulse, first-token
    /// emphasis, handoff hold, line wipe) and the scan-line overlay
    /// (static when true). Toggled in Settings → Display; the system
    /// a11y API hookup lands later.
    pub reduced_motion: bool,
    /// Phase accumulator (wraps in `[0.0, 1.0)`) driving the scanline
    /// overlay breathing animation. Advanced by `ScanlineTick` while the
    /// overlay is enabled and the Chat page is active.
    pub scanline_progress: f32,
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
    /// Whether the read-only memory graph modal (ADR-69 slice 3) is open.
    pub memory_graph_open: bool,
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
    /// Global-only orchestration enforcement (2026-09): the keys the project
    /// config declares that the load path now IGNORES (the `[orchestration]`
    /// table, `[multi_agent.custom_agents]`, `[multi_agent.model_pins]`).
    /// Recomputed from the raw project file on every config reconcile; drives
    /// the Studio import banner until the keys are relocated by the explicit
    /// import action or the banner is dismissed for this session.
    pub project_orchestration_keys: Vec<String>,
    /// Session-scoped dismissal of the import banner: a dismissal hides the
    /// banner until the next launch (new `App` instance) — never persisted,
    /// and re-arming only when the declared keys change shape is neither
    /// tracked nor needed (the keys list itself refreshes on reconcile).
    pub orchestration_banner_dismissed: bool,
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
/// entries for display.
///
/// Thin delegate: the mapping (and its skip rules) live in
/// [`views::chat::entries_from_messages`], next to `ChatEntry`.
fn messages_to_entries(history: Vec<concerto_core::types::Message>) -> Vec<views::chat::ChatEntry> {
    views::chat::entries_from(views::chat::ChatSource::Messages(history))
}

/// Map the durable typed transcript (ADR-36) onto chat entries for restore.
///
/// Thin delegate: the mapping lives in
/// [`views::chat::entries_from_transcript`], next to `ChatEntry`.
pub(crate) fn transcript_to_entries(entries: Vec<TranscriptEntry>) -> Vec<views::chat::ChatEntry> {
    views::chat::entries_from(views::chat::ChatSource::Transcript(entries))
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

/// Whether a provider is eligible for live model discovery: its type supports
/// it and any required credential is present.
///
/// Shared by startup auto-discovery and the save-triggered pass so the two
/// readiness gates can never drift. A provider type whose discovery is
/// unsupported, or which needs a credential that is not stored, is skipped.
fn provider_discovery_ready(
    provider: &concerto_config::ProviderConfig,
    credentials: &CredentialStore,
) -> bool {
    let definition = provider_definition(&provider.provider);
    definition.supports_discovery()
        && (!definition.requires_credential()
            || provider.api_key(credentials).map(|key| !key.is_empty()).unwrap_or(false))
}

/// Load the persisted desktop theme from the `UserPrefsStore`, falling back
/// to Midnight when the store cannot be opened.
///
/// Prefs are the single source of truth for every desktop surface that
/// renders the theme. `config.display.theme` only bridges the same palettes
/// to the CLI and is never read back into the UI — it is written alongside
/// prefs by [`App::apply_and_save_theme`] and asserted on every Settings save.
fn load_prefs_theme() -> AppTheme {
    let data_dir = dirs::data_dir()
        .map(|d| d.join("concerto"))
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let prefs_dir = data_dir.join("prefs");
    match concerto_memory::prefs::UserPrefsStore::open(&prefs_dir) {
        Ok(store) => crate::theme::prefs::load_theme(&store),
        Err(_) => AppTheme::by_name("Midnight"),
    }
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
        // Prefs — not `global_config.display.theme` — decide the startup
        // theme; both the live theme and the Settings picker are seeded from
        // this one value below.
        let theme = load_prefs_theme();
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
        // Process-lifetime plugin-manager handle: shared with the Settings
        // state (revoke) and passed to every run's ServicesBuilder (liveness).
        // The inner manager is materialised on the first agent run.
        let plugin_manager = concerto_plugins::manager::new_shared_plugin_manager();
        let initial_scanline_overlay = initial_config.display.scanline_overlay_enabled;
        let initial_reduced_motion = initial_config.display.reduced_motion;
        // Capture the persisted theme before it moves into the struct so the
        // Settings editor seeds from the applied theme (not the hardcoded
        // "Midnight" default) — the theme-persistence defect.
        let initial_theme_name = theme.name;
        let initial_font_size = theme.font_stack.base_size;
        let mut app = Self {
            page: Page::Chat,
            current_theme: theme,
            show_help: false,
            chat: views::chat::State::new(),
            diff: views::diff::State::new(),
            memory: views::memory::State::new(),
            memory_graph: views::memory_graph::State::new(),
            tool_log: views::tool_log::State::new(),
            settings: {
                let cfg = global_config.clone();
                let mut state = views::settings::State::from_config(&cfg);
                state.with_plugin_manager(plugin_manager.clone());
                state
            },
            agent_graph: views::agent_graph::State::new(),
            terminal: views::terminal::State::new(
                initial_project_dir.clone(),
                initial_config.clone(),
            ),
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
            plugin_manager,
            cancel_token: CancellationToken::new(),
            run_status: RunStatus::Idle,
            run_stage: None,
            circuit_progress: 0.0,
            scanline_overlay_enabled: initial_scanline_overlay,
            reduced_motion: initial_reduced_motion,
            scanline_progress: 0.0,
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
            memory_graph_open: false,
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
            project_orchestration_keys: Vec::new(),
            orchestration_banner_dismissed: false,
            toasts: ToastManager::new(),
        };
        // Plugin installer approval bridge (Settings → Plugins): install-time
        // capability prompts are answered through the SAME shared queue the
        // runtime capability dialog consumes, so a freshly installed plugin's
        // persistent grants land in the store that runs later honour.
        app.settings.seed_theme(initial_theme_name, initial_font_size);
        app.settings.with_plugin_approval(app.cap_pending.clone());
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
        app.refresh_project_orchestration_keys();
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
        // Mirror the configured motion override into the chat view so the
        // already-gated cues (scan-line pulse, first-token emphasis, handoff
        // hold, line wipe) follow Settings → Display without a restart.
        app.chat.set_reduced_motion(app.reduced_motion);
        app.seed_muted_agents();

        // Auto-discover models for every credentialed, discoverable provider at
        // startup so the unified picker (and per-provider lists) are populated
        // without any manual "refresh" action (Option-1: configure providers,
        // models flow in automatically).
        let discovery_credentials = CredentialStore::new();
        let ready_ids: Vec<String> = app
            .runtime_providers()
            .iter()
            .filter(|p| provider_discovery_ready(p, &discovery_credentials))
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

    /// Apply the theme currently selected in Settings and persist it to both
    /// stores that hold it — the `UserPrefsStore` (theme name + font size)
    /// and `config.display.theme` (the CLI bridge) — in one step, without
    /// waiting for a `SaveSettings` round-trip. Keeps `current_theme`, the
    /// terminal palette, the Settings picker, and both stores in lockstep so
    /// a restart restores the chosen theme (the theme-persistence defect:
    /// `ThemeSelected` only mutated the editor's local selection and was lost
    /// unless Settings was saved).
    fn apply_and_save_theme(&mut self) {
        let new_theme =
            AppTheme::by_name(self.settings.selected_theme).with_base_size(self.settings.font_size);
        let name_changed = new_theme.name != self.current_theme.name;
        self.current_theme = new_theme.clone();
        self.terminal.set_theme(&self.current_theme);
        // The picker is part of the applied state: re-seed it from what was
        // just applied so selection and live theme cannot drift apart.
        self.settings.seed_theme(new_theme.name, new_theme.font_stack.base_size);
        // Prefs are the single source the UI renders from; `display.theme` is
        // a read-through bridge for the CLI only, so it is asserted (memory +
        // disk, in one step) exactly when the applied theme's NAME changes —
        // a font-size tweak has no config key and must not rewrite the file.
        if name_changed {
            // Both in-memory copies are asserted together with the write, so
            // the merged config shown elsewhere in the app cannot keep the old
            // value. A project-layer `display.theme` override still wins on
            // the next reload (the file is truth, ADR-57 §6).
            let value = Some(new_theme.name.to_string());
            self.global_config.display.theme = value.clone();
            if let Some(config) = self.config.as_mut() {
                config.display.theme = value;
            }
            if let Some(path) = concerto_config::default_config_path() {
                if let Err(error) = concerto_config::save_config(&self.global_config, &path) {
                    tracing::error!(%error, "failed to persist the theme to the config");
                }
            }
        }
        let data_dir = dirs::data_dir()
            .map(|d| d.join("concerto"))
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        let prefs_dir = data_dir.join("prefs");
        if let Ok(store) = concerto_memory::prefs::UserPrefsStore::open(&prefs_dir) {
            crate::theme::prefs::save_theme(&store, &new_theme);
        }
    }

    /// Apply a theme loaded from the `UserPrefsStore` to every surface that
    /// renders it: the live theme, the terminal palette, and the Settings
    /// picker. Used by the external-prefs reload path (`Message::ThemeChanged`),
    /// where prefs — not config — are the source. Deliberately does not touch
    /// `config.display.theme`: an out-of-band prefs change must not rewrite
    /// the config file, and mutating only the in-memory copy would let the
    /// next config write persist a value that was never on disk.
    fn apply_prefs_theme(&mut self, theme: AppTheme) {
        self.current_theme = theme.clone();
        self.terminal.set_theme(&self.current_theme);
        self.settings.seed_theme(theme.name, theme.font_stack.base_size);
    }

    pub fn update(&mut self, message: Message) -> iced::Task<Message> {
        match message {
            // NORM S41 — the navigation arms (`Navigate`, `SetSubView`,
            // `OpenSpendLog`) moved verbatim to the sibling
            // `navigation_updates` submodule. The full `Message` is
            // forwarded so the helper preserves each arm's exact
            // early-return `Task` semantics.
            message @ (Message::Navigate(_) | Message::SetSubView(_) | Message::OpenSpendLog) => {
                self.update_navigation(message)
            }
            Message::Shortcut(shortcut) => self.handle_shortcut(shortcut),
            // NORM S43 — the `AgentRunCompleted` arm (the run-settled
            // teardown: run-status/stage reset, deferred memory-config flush,
            // chat/agent-graph settle against the outcome, resume checkpoint
            // store/clear, transcript + active-agent-graph persist, VFS diff
            // reload, and the session/git refresh batch) moved verbatim to the
            // sibling `run_completion` submodule. The full `Message` is
            // forwarded so the helper preserves the arm's exact `Task`
            // semantics.
            message @ Message::AgentRunCompleted(..) => self.update_run_completed(message),
            // NORM S30 — run cancel + the three shared animation ticks moved
            // verbatim to the sibling `simple_updates` submodule. The full
            // `Message` is forwarded so the helper preserves each arm's exact
            // early-return `Task` semantics.
            message @ (Message::CancelAgentRun
            | Message::CircuitTick
            | Message::ScanlineTick
            | Message::AnimTick) => self.update_cancel_and_ticks(message),
            // NORM S42 — the Chat arm's pre-routing (clipboard write,
            // navigation / session-select / spend-refresh re-dispatches,
            // the New Session reset, focus tracking, and the
            // submit/toggle pre-work) moved verbatim to the sibling
            // `update_routing` submodule. The full `Message` is forwarded
            // so the helper preserves each branch's exact early-return
            // `Task` semantics; its fall-through still calls the S39
            // `update_chat_tail`.
            message @ Message::Chat(_) => self.update_chat_pre_routing(message),
            // NORM S39 — the `Diff` dispatch arm (`diff.update` + the
            // conditional VFS commit for accept/reject/undo decisions)
            // moved verbatim to the sibling `view_dispatch` submodule. The
            // full `Message` is forwarded so the helper preserves the arm's
            // exact `Task` semantics.
            message @ Message::Diff(_) => self.update_diff(message),
            // NORM S39 — the `Memory` dispatch arm (child-view passthrough
            // + the reindex/refresh/search/delete loader dispatches) moved
            // verbatim to the sibling `view_dispatch` submodule. The full
            // `Message` is forwarded so the helper preserves each branch's
            // exact `Task` semantics.
            message @ Message::Memory(_) => self.update_memory(message),
            // NORM S44 — the four memory-result arms (`ReindexResult`,
            // `MemoryEntriesLoaded`, `PluginProvidersRefreshed`, and
            // `MemoryEntryDeleted`) moved verbatim to the sibling
            // `memory_results` submodule. The full `Message` is forwarded so
            // the helper preserves each arm's exact early-return `Task`
            // semantics.
            message @ (Message::ReindexResult(_)
            | Message::MemoryEntriesLoaded(_)
            | Message::PluginProvidersRefreshed
            | Message::MemoryEntryDeleted { .. }) => self.update_memory_results(message),
            // NORM S37 — the `ToolLog` child-view passthrough one-liner
            // lives in the sibling `delegated_updates` submodule.
            message @ Message::ToolLog(_) => self.update_tool_log(message),
            // NORM S41 — the `SessionSelected` arm (session switch with
            // sidecar restore + durable-event replay fallback) moved
            // verbatim to the sibling `navigation_updates` submodule. The
            // full `Message` is forwarded so the helper preserves the
            // arm's exact `Task` semantics.
            message @ Message::SessionSelected { .. } => self.update_session_selected(message),
            // NORM S40 — the `Settings` dispatch arm (its inner
            // `settings.update` forwarding plus the SaveSettings
            // persist/reconcile/theme/model-cache pipeline, the tracked
            // provider-model refresh request/refreshed pair with the
            // staleness guards and config-side cache write, the immediate
            // theme/font apply-persist pair, and the shell-security
            // revision merge) moved verbatim to the sibling
            // `settings_updates` submodule. The full `Message` is
            // forwarded so the helper preserves each arm's exact
            // early-return `Task` semantics.
            message @ Message::Settings(_) => self.update_settings(message),
            // NORM S30 — the `AgentGraph`/`Terminal` passthrough one-liners
            // live in the sibling `simple_updates` submodule.
            message @ (Message::AgentGraph(_) | Message::Terminal(_)) => {
                self.update_passthrough_views(message)
            }
            // NORM S37 — the thin-delegation orchestration arms (banner
            // import action, banner dismiss, Studio dispatch + guarded Save
            // persist, and the stale-checked runtime-snapshot load) moved
            // verbatim to the sibling `delegated_updates` submodule. The full
            // `Message` is forwarded so the helper preserves each arm's exact
            // early-return `Task` semantics.
            message @ (Message::ImportProjectOrchestration
            | Message::DismissOrchestrationBanner
            | Message::OrchestrationStudio(_)
            | Message::StudioRuntimeLoaded(..)) => self.update_orchestration(message),
            // NORM S37 — the `Editor` dispatch arm (screenshot re-dispatch,
            // `editor.update`, and the staged-diff reload) moved verbatim to
            // the sibling `delegated_updates` submodule.
            message @ Message::Editor(_) => self.update_editor(message),
            // NORM S30 — prefs-theme reload + help-overlay flip moved to the
            // sibling `simple_updates` submodule.
            message @ (Message::ThemeChanged | Message::HelpToggled) => {
                self.update_theme_and_help(message)
            }
            // NORM S31 — project-dir picker, root-consent gate, and
            // project-tree arms moved verbatim to the sibling
            // `project_switch` submodule. The full `Message` is
            // forwarded so the helper preserves each arm's exact
            // early-return `Task` semantics.
            message @ (Message::OpenProjectDirPicker
            | Message::ProjectDirInputChanged(_)
            | Message::ProjectDirCancel
            | Message::ProjectDirApply
            | Message::RootConsentAllow
            | Message::RootConsentDeny
            | Message::ToggleProjectExpanded(_)
            | Message::ProjectSessionsLoaded { .. }
            | Message::TreeSessionClicked { .. }) => self.update_project_switch(message),
            // NORM S37 — the capability / ack / intent / plan dialog resolver
            // arms (shared pending queues) moved verbatim to the sibling
            // `delegated_updates` submodule. The full `Message` is forwarded
            // so the helper preserves each arm's exact early-return `Task`
            // semantics.
            message @ (Message::CapabilityDlg(_)
            | Message::AckDialog(_)
            | Message::IntentDialog(_)
            | Message::PlanDialog(_)) => self.update_capability_dialogs(message),
            // NORM S38 — the `DesktopEvent` arm (App-level spend / cap /
            // run-stage chip updates, the `ErrorOccurred` toast, and the
            // `route_event` fan-out into the chat / tool-log / agent-graph /
            // memory view states) moved verbatim to the sibling
            // `event_model_updates` submodule. The full `Message` is
            // forwarded so the helper preserves the arm's exact semantics.
            message @ Message::DesktopEvent(_) => self.update_desktop_event(message),
            // NORM S38 — the active provider/model selection + external
            // config-reload arms (`SetActiveProvider`, `SetActiveModel`,
            // `SetAgentModel`, `ConfigReloaded`) moved verbatim to the
            // sibling `event_model_updates` submodule. The full `Message` is
            // forwarded so the helper preserves each arm's exact
            // early-return `Task` semantics.
            message @ (Message::SetActiveProvider(_)
            | Message::SetActiveModel(_)
            | Message::SetAgentModel { .. }
            | Message::ConfigReloaded) => self.update_model_config(message),

            // (sync_chat_model_options is invoked inside persist_active_model_selection)
            // NORM S30 — the trivial tail cluster (screenshot status, save
            // feedback, toasts, quick panel, memory modal/graph, terminal
            // panel + drag resize, git summary) moved verbatim to the
            // sibling `simple_updates` submodule. The full `Message` is
            // forwarded so the helper preserves each arm's exact early-return
            // `Task` semantics.
            message @ (Message::TakeScreenshot
            | Message::ScreenshotCompleted(_)
            | Message::ClearScreenshotStatus
            | Message::ClearSaveFeedback(_)
            | Message::ToastDismissed(_)
            | Message::ToastExpiryTick
            | Message::ToggleQuickPanel
            | Message::OpenMemoryModal
            | Message::CloseMemoryModal
            | Message::OpenMemoryGraph
            | Message::CloseMemoryGraph
            | Message::MemoryGraph(_)
            | Message::MemoryGraphLoaded(_)
            | Message::ToggleTerminalPanel
            | Message::TerminalPanelResizeStart
            | Message::TerminalPanelResizeMoved(_)
            | Message::TerminalPanelResizeEnd
            | Message::GitSummaryLoaded(_)) => self.update_trivial_ui_state(message),
        }
    }

    /// After an agent run completes, load diffs from the shared VirtualFs
    /// into the diff viewer state so the user can review proposed changes.
    fn load_diff_from_vfs(&mut self) {
        let Ok(vfs) = self.vfs.lock() else { return };
        let diff_results = compute_diffs_from_virtual_fs(&vfs);
        self.editor.set_staged_results(&vfs, diff_results.clone());

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

    /// Model names selectable for the active provider in the chat header,
    /// resolved through the shared picker resolver (selected / default / known
    /// / discovered / config-first `extra_models`) so every picker agrees.
    fn runtime_model_names(&self, provider_id: &str) -> Vec<String> {
        self.runtime_providers()
            .iter()
            .find(|provider| provider.id == provider_id)
            .map(picker_model_options)
            .unwrap_or_default()
    }

    fn dispatch_validation_error(&self) -> Option<String> {
        if self.runtime_providers().is_empty() {
            return Some("no providers are configured".to_string());
        }
        let creds = CredentialStore::new();
        // The intent gate is always on (ADR-55 §7): there is no mode
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
        // Drop the previous run's per-subagent progress cards for the same
        // reason: a stale card must never attach to the new prompt.
        self.chat.begin_run();
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
            let plugin_manager = self.plugin_manager.clone();
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
                            .with_plugins(plugin_manager)
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

    /// Re-run WASM plugin discovery against the retained plugin manager after
    /// a Settings save (interim "provider added" re-collect hook). A newly
    /// dropped-in `.wasm` provider plugin becomes active in the
    /// process-lifetime manager so the next run — and any future picker work —
    /// sees it, without displacing already-loaded plugins. Gated on plugins
    /// being enabled with `auto_load`, the same condition the runtime uses to
    /// auto-approve at run time. A full `.wasm` filesystem watcher is a
    /// documented follow-up; until then this save-time pass is the trigger.
    fn refresh_plugin_providers(&self) -> iced::Task<Message> {
        let Some(config) = self.config.clone() else {
            return iced::Task::none();
        };
        let Some(ref plugin_cfg) = config.plugins else {
            return iced::Task::none();
        };
        if !plugin_cfg.enabled || !plugin_cfg.auto_load {
            return iced::Task::none();
        }
        let manager = self.plugin_manager.clone();
        let search_paths = plugin_cfg.search_paths.clone();
        iced::Task::perform(
            async move {
                let mut guard = manager.lock().await;
                let Some(manager) = guard.as_mut().map(|(_, manager)| manager) else {
                    // No run has materialised the manager yet; the first run
                    // discovers any new plugin anyway.
                    tracing::debug!("plugin refresh: manager not materialised yet — skipped");
                    return;
                };
                let config = concerto_plugins::discovery::DiscoveryConfig {
                    search_paths: search_paths.into_iter().map(std::path::PathBuf::from).collect(),
                    bundled_path: None,
                };
                // Newly discovered plugins are initialised with an EMPTY grant
                // set on purpose: host functions stay fail-closed until the
                // next run re-initialises the plugin with its run-scoped,
                // auto-approved grants.
                match manager
                    .refresh_new_plugins(config, |_| {
                        concerto_plugins::capability::GrantedCapabilities::new()
                    })
                    .await
                {
                    Ok(count) if count > 0 => {
                        tracing::info!(count, "plugin refresh: loaded newly discovered plugins");
                    }
                    Ok(_) => tracing::debug!("plugin refresh: no new plugins"),
                    Err(error) => tracing::warn!(error = %error, "plugin refresh failed"),
                }
            },
            |_| Message::PluginProvidersRefreshed,
        )
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

    /// Persist one agent's model override to the global config (right-toolbar
    /// quick swap) and re-derive runtime state. Mirrors the Studio's
    /// `AssignModel` seam: an empty/"default" model removes the assignment so
    /// the agent falls back to the global default. Provider resolution prefers
    /// the agent's existing assignment, then the active provider, then the
    /// first configured provider.
    fn set_agent_model(&mut self, agent_id: String, model: String) {
        let provider_id = self
            .runtime_assignments()
            .iter()
            .find(|assignment| assignment.agent_role == agent_id)
            .map(|assignment| assignment.provider_config_id.clone())
            .filter(|id| !id.is_empty())
            .unwrap_or_else(|| {
                if !self.active_provider_id.is_empty() {
                    self.active_provider_id.clone()
                } else {
                    self.runtime_providers().first().map(|p| p.id.clone()).unwrap_or_default()
                }
            });
        let use_default = model.is_empty() || model == "default" || provider_id.is_empty();
        let mut config = self.global_config.clone();
        let settings = config.model_settings.get_or_insert_with(Default::default);
        if use_default {
            settings.agent_assignments.retain(|assignment| assignment.agent_role != agent_id);
        } else if let Some(assignment) =
            settings.agent_assignments.iter_mut().find(|a| a.agent_role == agent_id)
        {
            assignment.provider_config_id = provider_id;
            assignment.model_override = Some(model.clone());
        } else {
            settings.agent_assignments.push(concerto_config::AgentModelAssignment {
                agent_role: agent_id.clone(),
                provider_config_id: provider_id,
                model_override: Some(model.clone()),
            });
        }
        match concerto_config::default_config_path() {
            Some(path) => match concerto_config::save_config(&config, &path) {
                Ok(()) => self.reconcile_config_from_reload(),
                Err(error) => {
                    tracing::error!(%error, "failed to persist agent model override");
                    self.toasts.push(ToastLevel::Error, format!("Could not save model: {error}"));
                    return;
                }
            },
            None => {
                self.global_config = config.clone();
                self.config = Some(config);
            }
        }
        // Keep the rendered card in sync until the next config reload.
        let override_model = (!use_default).then_some(model);
        self.orchestration_studio.set_agent_model_override(&agent_id, override_model);
    }

    /// Rebuild the chat header model-option list from the active provider's
    /// shared resolver, so the `pick_list` can borrow a value that outlives the
    /// per-frame `view` borrow.
    fn sync_chat_model_options(&mut self) {
        self.chat_model_options = self.runtime_model_names(&self.active_provider_id);
    }

    /// Kick off live model discovery for ready providers that have never been
    /// fetched successfully, so a provider added in Settings populates its
    /// model lists on save — no manual refresh or restart required.
    ///
    /// Eligible providers pass the same readiness gate startup auto-discovery
    /// uses: the provider type must support discovery and any required
    /// credential must be present. Providers already in flight (startup or a
    /// manual refresh) are skipped, and so are providers with a cached catalog,
    /// so a save never re-hits an already-populated provider.
    fn discover_unfetched_models(&mut self) -> iced::Task<Message> {
        let credentials = CredentialStore::new();
        let ids: Vec<String> = self
            .runtime_providers()
            .iter()
            .filter(|p| {
                provider_discovery_ready(p, &credentials)
                    && p.cached_models.is_empty()
                    && p.cached_models_fetched_at == 0
                    && !self.pending_refresh.contains_key(&p.id)
            })
            .map(|p| p.id.clone())
            .collect();
        let mut tasks = Vec::with_capacity(ids.len());
        for id in ids {
            self.refresh_seq = self.refresh_seq.wrapping_add(1);
            let request_id = self.refresh_seq;
            self.pending_refresh.insert(id.clone(), request_id);
            self.settings.begin_provider_refresh(&id);
            tasks.push(self.fetch_models_for_provider(id, request_id));
        }
        iced::Task::batch(tasks)
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
                    api_key.expose(),
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

    /// Seed the chat thinking-bucket mute filter from the merged config.
    /// Called after every `self.chat` replacement (startup, new session,
    /// session restore, project switch) so blank and restored sessions
    /// honor `[display] muted_agents`. Hide-not-delete is preserved: the
    /// WAL and transcript keep every thought regardless of this filter.
    fn seed_muted_agents(&mut self) {
        let muted = self
            .config
            .as_ref()
            .map(|config| config.display.muted_agents.clone())
            .unwrap_or_default();
        self.chat.set_muted_agents(muted);
    }

    /// Persist the chat thinking-bucket mute set to `[display]
    /// muted_agents` in the global config file, mirroring the multi-agent
    /// toggle: save, then reload + re-derive through the shared helper so a
    /// project-layer override stays the truth (ADR-57 §6). Falls back to
    /// in-memory config when no global path exists. Never touches the WAL
    /// or the durable transcript (hide-not-delete).
    fn persist_muted_agents(&mut self) {
        let mut config = self.global_config.clone();
        config.display.muted_agents = self.chat.muted_agents_snapshot();
        match concerto_config::default_config_path() {
            Some(path) => {
                if let Err(error) = concerto_config::save_config(&config, &path) {
                    tracing::error!(%error, "failed to persist muted agents");
                } else {
                    self.reconcile_config_from_reload();
                }
            }
            None => {
                self.global_config = config.clone();
                self.config = Some(config);
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
        // Global-only orchestration enforcement: the ignored-key set is
        // derived from the raw PROJECT file (not the merged config) and must
        // refresh on every reconcile — including a project switch that yields
        // a byte-identical merged config (the short-circuit below would
        // otherwise leave the previous project's keys showing).
        self.refresh_project_orchestration_keys();
        self.apply_reloaded_config(reloaded_global, reloaded);
    }

    /// Recompute the ignored project-layer orchestration keys from the raw
    /// project file. Shared by [`App::new`],
    /// [`Self::reconcile_config_from_reload`], and the import/dismiss tests.
    fn refresh_project_orchestration_keys(&mut self) {
        let project_config =
            self.project_dir.join(concerto_config::legacy::NEW_PROJECT_CONFIG_FILE);
        self.project_orchestration_keys =
            concerto_config::declared_project_orchestration_keys(&project_config);
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
        // Re-derive the Display motion toggles — the file is truth. The chat
        // setter settles any in-flight wipe instantly when reduced-motion
        // turns on mid-animation.
        self.scanline_overlay_enabled = reloaded.display.scanline_overlay_enabled;
        self.reduced_motion = reloaded.display.reduced_motion;
        self.chat.set_reduced_motion(self.reduced_motion);
        // Re-derive the thinking-bucket mute filter — the file is truth
        // (covers the mute toggle's own save and external edits alike).
        self.chat.set_muted_agents(reloaded.display.muted_agents.clone());
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
        let _ = self.terminal.set_config(reloaded.clone(), &self.current_theme);
        // Provider rows first, then the derived caches: the row sync rebuilds
        // the caches from the refreshed rows (and is a no-op while the user
        // has in-flight edits, ADR-57 §3d); the cache-only refresh below then
        // re-derives the pickers/Studio caches from the config even when the
        // row sync was blocked. Neither the Settings form nor Studio drafts
        // are ever rebuilt against in-flight edits.
        self.settings.sync_providers_from_config(&reloaded);
        self.settings.refresh_provider_cache_from_config(&reloaded);
        self.settings.sync_display_from_config(&reloaded);
        self.settings.sync_project_context_from_config(&reloaded);
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

    pub fn view(&self) -> Element<'_, Message> {
        let sidebar = views::nav::sidebar_view(self);

        let content: Element<'_, Message> = match self.page {
            Page::Chat => {
                // Constrain the chat reading column: `Fill` width capped at
                // `CHAT_MAX_WIDTH` and centered — same pattern as the ~900px
                // modal card below (`container(..).width(Fill).max_width(900)`
                // inside a `.center_x(Fill)` backdrop). A raw `Fixed` width
                // here would overflow behind the fixed side rails on narrow
                // windows; `Fill` + `max_width` shrinks with the main area
                // instead.
                let chat = self
                    .chat
                    .view(
                        &self.current_theme,
                        !self.chat_model_options.is_empty(),
                        self.run_status == RunStatus::Running,
                        &self.agent_graph,
                    )
                    .map(Message::Chat);
                container(
                    container(chat)
                        .width(Length::Fill)
                        .max_width(CHAT_MAX_WIDTH)
                        .height(Length::Fill),
                )
                .width(Length::Fill)
                .height(Length::Fill)
                .center_x(Length::Fill)
                .into()
            }
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
            Page::OrchestrationStudio => {
                let studio = self.orchestration_studio.view(&self.current_theme);
                match (
                    self.project_orchestration_keys.as_slice(),
                    self.orchestration_banner_dismissed,
                ) {
                    ([], _) | (_, true) => studio,
                    _ => {
                        let banner: Element<'_, Message> = self.orchestration_import_banner_view();
                        column![banner, container(studio).height(Length::Fill).width(Length::Fill),]
                            .width(Length::Fill)
                            .height(Length::Fill)
                            .into()
                    }
                }
            }
            Page::Editor => self.editor.view(&self.current_theme).map(Message::Editor),
        };

        let sep = rule::vertical(1);
        let content_column = self.view_status_column(content);
        let main_area = self.view_terminal_panel(content_column);

        // Studio owns the full configuration workspace. The session rail is
        // restored on return without changing the user's panel preference.
        let studio_active = self.page == Page::OrchestrationStudio;
        let sep2 = (!studio_active).then(|| rule::vertical(1));
        let right_panel: Option<Element<'_, Message>> = if studio_active {
            None
        } else if self.quick_panel_open {
            Some(views::quick_panel::quick_panel_view(self))
        } else {
            Some(views::quick_panel::quick_panel_collapsed(self))
        };

        // Explicit `Fill` on the shell row: the sidebar and right panel keep
        // their natural/`Fixed` widths, the center column takes the remaining
        // space. Without this the row reports its natural width and the chat
        // column can slide under the fixed right rail on narrow windows.
        let shell = row![sidebar, sep, main_area, sep2, right_panel,]
            .width(Length::Fill)
            .height(Length::Fill);

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
                    views::chat::SubView::Runtime => views::studio_runtime::runtime_modal_view(
                        &self.orchestration_studio.runtime_snapshot,
                        &self.current_theme,
                    ),
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
                views::chat::SubView::Runtime => "Runtime",
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
                || self.chat.sub_view == views::chat::SubView::Runtime
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

        let composed = self.view_system_dialogs(after_subview);
        self.view_ambient(composed)
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

// NORM S45: the `app::tests` suite moved verbatim into `app/tests/mod.rs`
// (R02/S15 precedent); `mod tests;` on this file module resolves there.
#[cfg(test)]
mod tests;
