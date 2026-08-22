//! Shared single‑agent runner used by both CLI and Desktop.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::agent_runner::AgentRunner;
use crate::coordinator::{CoordinatorAgent, OrchestrationDepth};
use crate::intent_grants::{
    apply_intent_gate, outcome_name, router_route_name, IntentGrantStore, SessionIntentAuth,
};
use crate::plan_approval::{
    apply_plan_decision, plan_registry, rehydrate_durable_binding, verified_binding, PlanBinding,
};
use crate::registry::AgentRegistry;
use crate::session_manager::{ProjectSessionManager, SessionManagerConfig};
use crate::{AgentRelationship, CollaborationRule};

use async_trait::async_trait;
use concerto_config::AppConfig;
use concerto_config::BlueprintFacade;
use concerto_config::CredentialStore;
use concerto_config::RelationshipSemantics;
use concerto_config::ResolvedBlueprint;
use concerto_config::StageKind;
use concerto_core::error::{PolicyError, ProviderError};
use concerto_core::event::{Event, EventBus, EventKind};
use concerto_core::executor::ToolExecutor;
use concerto_core::ids::Ulid;
use concerto_core::intent::{PlanDecision, RequestedOutcome, RouterOutput, RouterRoute, RunStage};
use concerto_core::traits::approval::ApprovalSink;
use concerto_core::traits::memory::{MemoryStore, NullMemoryStore};
use concerto_core::traits::policy::{AuditEntry, AuditLog, PolicyEngine};
use concerto_core::traits::provider::LlmProvider;
use concerto_core::transcript::{
    transcript_entry_from_event_with_labels, GateLabels, TranscriptEntry, TranscriptToolStatus,
};
use concerto_core::types::ToolRegistry;
use concerto_core::types::{
    AgentCompletionStatus, AgentContext, AgentId, AgentOutput, AgentStage, AgentTask,
    CompletionRequest, Message, ProjectId, ProviderMetrics, Role, SessionContext, TaskId,
};
use concerto_core::types::{Condition, PolicyRule};
use concerto_core::{
    inject_intent_gate_rule, CancellationToken, IntentAuthorization, OrchestratorError,
    PolicyPresets, RpmLimiter, SimplePolicyEngine, SpendTracker, LOW_CONFIDENCE_THRESHOLD,
};
use concerto_sessions::audit::SqliteAuditLog;
use concerto_sessions::PlanBindingRecord;
use concerto_sessions::SessionStore;
use concerto_tools::git::GitTool;

use concerto_lsp::tools::*;

use concerto_providers::factory::ProviderFactory;
use concerto_providers::model_registry::ModelRegistry;
use concerto_providers::model_selector::ModelSelector;
use concerto_providers::retry::RetryPolicy;
use concerto_providers::routing::RoutingEngine;
use concerto_tools::filesystem::FilesystemTool;
use concerto_tools::shell::ShellTool;
use concerto_tools::undo::UndoManager;
use concerto_tools::virtual_fs::VirtualFs;

use crate::agent_loop::AgentLoop;
use crate::exec_backend::SharedExecutionBackend;
use crate::gate::{FilePreImageReader, WriteGate};
use crate::in_process_gate::InProcessGateBackend;
use crate::prompts::PromptBuilder;

/// Shared memory-enabled decision for both frontends.
///
/// CLI `-f/--fast` mode and the desktop "Fast mode" toggle both disable
/// project memory retrieval for a run while the configured flag stays
/// untouched. Centralising the boolean logic here — instead of inlining
/// `!fast && configured` at each call site — guarantees the two frontends can
/// never drift apart, and pins the contract in `crates/orchestrator/tests/
/// parity.rs` (`memory_enabled_contract`).
pub fn memory_enabled(fast: bool, configured: bool) -> bool {
    !fast && configured
}

/// Normalize a configured role string into an agent id.
///
/// Accepts any non-empty id (lowercased by [`AgentId::new`]). Built-in
/// specialist ids and user-defined custom ids both resolve; empty or
/// whitespace-only values are rejected.
fn configured_agent_id(role: &str) -> Option<AgentId> {
    let id = AgentId::new(role);
    (!id.as_str().is_empty()).then_some(id)
}

/// Resolve a configured relationship *kind string* into the engine's closed
/// `AgentRelationship` vocabulary (ADR-58 P2+P3, F9).
///
/// The closed legacy vocabulary (supervises / provides_context_to /
/// reports_to / owns_design) resolves byte-identically on the default
/// `standard` blueprint. Any other kind string is resolved through the
/// resolved blueprint's open relationship registry when a facade is attached
/// (kind row → closed semantics), and an unmatched kind is a hard error — a
/// typo'd legacy relationship must never be silently dropped (design doc §4
/// F7 review). Without a facade (tests, `[orchestration]`-less configs)
/// unknown kinds keep the legacy hard rejection.
fn configured_relationship(
    facade: Option<&BlueprintFacade>,
    relationship: &str,
) -> Result<AgentRelationship, OrchestratorError> {
    let lowered = relationship.to_ascii_lowercase();
    Ok(match lowered.as_str() {
        // The closed legacy vocabulary, byte-identical on `standard`.
        "supervises" => AgentRelationship::Supervises,
        "provides_context_to" => AgentRelationship::ProvidesContextTo,
        "reports_to" => AgentRelationship::ReportsTo,
        "owns_design" => AgentRelationship::OwnsDesign,
        // A blueprint-registered kind outside the closed vocabulary resolves
        // through the open registry rows over the closed semantics: the
        // `supervises`/`reports_to` family is Delegation, a gate kind such as
        // `approves` reads as Supervises (design doc §7 Q3).
        _ => {
            let Some(facade) = facade else {
                return Err(OrchestratorError::AgentLoopError(format!(
                    "unknown agent relationship: {relationship}"
                )));
            };
            let semantics = facade.relationship_semantics(&lowered).map_err(|error| {
                OrchestratorError::AgentLoopError(format!(
                    "unknown agent relationship: {relationship} ({error})"
                ))
            })?;
            match semantics {
                RelationshipSemantics::ApprovalGate => AgentRelationship::Supervises,
                RelationshipSemantics::ContextFlow => AgentRelationship::ProvidesContextTo,
                RelationshipSemantics::Delegation => AgentRelationship::OwnsDesign,
            }
        }
    })
}

/// ADR-58 P2+P3 (R6/F3): resolve the `RunStage` chip advance for one
/// coordinator event from the blueprint's per-stage feed bindings — the single
/// table live emission and sessions replay derive from.
///
/// - A `SubTaskCreated` for a role staffed in a feed-bound stage advances to
///   that stage's feed (`facade.feed_for(tag)`, blueprint §5.6). Without a
///   facade (tests, `[orchestration]`-less configs) the legacy implement-tag
///   classification keeps exactly today's behavior.
/// - Gate-cycle events advance to their gate stage's feed: a review cycle →
///   the review feed, a validation cycle → the validate feed — both `Verify`
///   on `standard`. The review-cycle → Verify advance is the deliberate Q4
///   pin (design doc §7 Q4; P1 binds `review → Verify`, blueprint.rs:668).
/// - `EventKind` stays closed (F3): a stage without a feed binding (custom /
///   `RunOnce` stages) emits no chip advance from the feed task.
/// - The coordinator self-implement sentinel keeps advancing to `Execute`
///   when the Execution stage is unstaffed (design doc §3 review F4, ADR-35
///   §8 trigger 1).
/// - Planning-only runs (M1) never report an implement transition.
fn stage_feed_advance(
    kind: &EventKind,
    registry: &AgentRegistry,
    facade: Option<&BlueprintFacade>,
    planning_only: bool,
    coordinator_self_implements: bool,
) -> Option<RunStage> {
    if planning_only {
        return None;
    }
    match kind {
        EventKind::SubTaskCreated { role, .. } => {
            let role_stage = registry.get(role).and_then(|agent| agent.stage());
            let feed = match facade {
                Some(facade) => {
                    role_stage.as_ref().and_then(|stage| facade.feed_for(stage.as_str()))
                }
                // No facade attached: the legacy implement-tag classification.
                None => role_stage
                    .as_ref()
                    .filter(|stage| stage.is_implement())
                    .map(|_| RunStage::Execute),
            };
            // Coordinator self-execution: the coordinator role exists only for
            // stage-absent implement subtasks (ADR-35 §8; review F4).
            feed.or_else(|| {
                (role.as_str() == "coordinator" && coordinator_self_implements)
                    .then_some(RunStage::Execute)
            })
        }
        // Gate-cycle events advance to their gate stage's feed binding. On
        // `standard` both gates publish Verify; the review-gate line is the
        // deliberate Q4 pin. The gate stage is resolved by kind, so a
        // renamed review/validate tag keeps its feed (issue #150).
        EventKind::ReviewCycleStarted { .. } => facade.and_then(|facade| {
            facade.first_stage_of_kind(StageKind::Review).and_then(|stage| stage.effective_feed)
        }),
        EventKind::ValidationCycleStarted { .. } => match facade {
            Some(facade) => facade
                .first_stage_of_kind(StageKind::Acceptance)
                .and_then(|stage| stage.effective_feed),
            None => Some(RunStage::Verify),
        },
        _ => None,
    }
}

/// Built-in specialist ids in seed order (ADR-35 phase 4).
///
/// Derived from `concerto_config::builtin_agent_seeds()` instead of a
/// literal role array, so renaming a seed in config keeps the runtime
/// topology and tool-calling classification in sync.
fn builtin_seed_ids() -> Vec<AgentId> {
    concerto_config::builtin_agent_seeds().into_iter().map(|seed| AgentId::new(&seed.id)).collect()
}

/// Build a per-agent config map from `MultiAgentConfig.custom_agents`.
///
/// Converts the `String`-based role field to `AgentId`; entries with
/// unrecognised roles are silently skipped. Returns an empty map when
/// `multi_agent` is `None`.
fn build_agent_config_map(
    multi_agent: &Option<concerto_config::MultiAgentConfig>,
) -> HashMap<AgentId, concerto_config::CustomAgentConfig> {
    let Some(multi_agent) = multi_agent else {
        return HashMap::new();
    };
    let mut map = HashMap::new();
    for agent in &multi_agent.custom_agents {
        if let Some(role) = configured_agent_id(&agent.role) {
            map.insert(role, agent.clone());
        }
    }
    map
}

/// Legacy `model_pins` plus per-agent model overrides folded in from
/// `custom_agents`.
///
/// Mirrors the pre-ADR-35 per-role assignment behavior: a model pinned on a
/// custom agent is honoured on its default provider when no explicit
/// `agent_assignments` entry exists. `agent_assignments` are still resolved
/// first and win over these pins.
fn legacy_pins_from_config(
    multi_agent: &Option<concerto_config::MultiAgentConfig>,
) -> HashMap<AgentId, String> {
    let mut pins = multi_agent.as_ref().map(|multi| multi.model_pins.clone()).unwrap_or_default();
    let Some(multi) = multi_agent else {
        return pins;
    };
    for agent in &multi.custom_agents {
        let Some(role) = configured_agent_id(&agent.role) else {
            continue;
        };
        if let Some(model) = non_empty(agent.model_override.as_deref()) {
            pins.insert(role, model.to_string());
        }
    }
    pins
}

/// ADR-35 phase 4: the roles needing provider/model resolution mirror the
/// runtime topology: the coordinator plus every registered specialist —
/// built-ins not disabled by config, then custom agents (disabled ones
/// excluded), in deterministic order.
fn topology_roles(multi_agent: &Option<concerto_config::MultiAgentConfig>) -> Vec<AgentId> {
    let mut roles = vec![AgentId::new("coordinator")];
    for id in builtin_seed_ids() {
        let disabled = multi_agent
            .as_ref()
            .and_then(|m| {
                m.custom_agents.iter().find(|a| configured_agent_id(&a.role).as_ref() == Some(&id))
            })
            .is_some_and(|a| a.disabled);
        if !disabled {
            roles.push(id);
        }
    }
    if let Some(multi_agent) = multi_agent {
        for agent in &multi_agent.custom_agents {
            if agent.disabled {
                continue;
            }
            if let Some(id) = configured_agent_id(&agent.role) {
                if !roles.contains(&id) {
                    roles.push(id);
                }
            }
        }
    }
    roles
}

const COORDINATOR_ONLY_ROLES: [&str; 1] = ["coordinator"];

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

async fn persist_provider_metrics(
    store: Option<&Arc<dyn concerto_sessions::SessionStore>>,
    session_id: Ulid,
    metrics: &[ProviderMetrics],
    cancel: CancellationToken,
) {
    let Some(store) = store else {
        return;
    };
    for metric in metrics {
        if metric.provider.trim().is_empty() {
            continue;
        }
        if let Err(error) = store.record_metrics(session_id, metric.clone(), cancel.clone()).await {
            tracing::warn!(%error, "failed to persist provider metrics");
        }
    }
}

/// Best-effort persist of one [`SpendRecord`] per settled provider call.
///
/// Mirrors [`persist_provider_metrics`] so the spend log stays aligned with the
/// metrics log: one record per `ProviderMetrics` entry, i.e. exactly once per
/// completed provider call, never per event. `metrics` carries the settled
/// actual cost (`cost_usd` is the same value used to settle the `SpendTracker`
/// in `AgentRunner` for multi-agent runs; for single-agent runs it is the
/// accumulated usage cost already persisted as metrics). `task_id` is the task
/// id available at the call site — the run's root task for multi-agent runs,
/// whose per-subtask spend is attributed to it because per-subtask ids are not
/// exposed beyond the coordinator. Failures are logged and swallowed so spend
/// persistence never breaks a run.
async fn persist_spend_records(
    store: Option<&Arc<dyn concerto_sessions::SessionStore>>,
    session_id: Ulid,
    task_id: Option<Ulid>,
    metrics: &[ProviderMetrics],
    cancel: CancellationToken,
) {
    let Some(store) = store else {
        return;
    };
    for metric in metrics {
        if metric.provider.trim().is_empty() {
            continue;
        }
        let record = concerto_sessions::spend::SpendRecord {
            id: Ulid::new(),
            session_id,
            task_id,
            provider: metric.provider.clone(),
            model: metric.model.clone(),
            tokens_in: metric.tokens_in,
            tokens_out: metric.tokens_out,
            cost_usd: metric.cost_usd,
            created_at: time::OffsetDateTime::now_utc(),
        };
        if let Err(error) = store.record_spend(record, cancel.clone()).await {
            tracing::warn!(%error, %session_id, "failed to persist spend record");
        }
    }
}

/// True when a store failure is the expected cancellation tail of a run
/// being cancelled: the token fired, or the store reported a cancelled
/// operation (e.g. `check_cancel`'s "operation cancelled"). Such failures
/// are not defects and are logged at debug level to keep cancellation
/// paths quiet.
fn is_expected_cancellation(
    error: &concerto_sessions::SessionError,
    cancel: &CancellationToken,
) -> bool {
    if cancel.is_cancelled() {
        return true;
    }
    matches!(
        error,
        concerto_sessions::SessionError::Database(msg) if msg.to_ascii_lowercase().contains("cancel")
    )
}

async fn maintain_context_after_run(
    store: Option<&Arc<dyn concerto_sessions::SessionStore>>,
    session_id: Ulid,
    context_config: Option<&concerto_config::ContextConfig>,
    cancel: CancellationToken,
    bus: Option<&EventBus>,
) {
    let Some(store) = store else {
        return;
    };
    let engine = crate::context_engine::ContextEngine::from_config(context_config);
    if let Err(error) = engine.maintain(store.clone(), session_id, cancel, bus).await {
        tracing::warn!(%error, %session_id, "failed to maintain durable context checkpoints");
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_text_only(
    task: &AgentTask,
    prompt: &'static str,
    mut history: Vec<Message>,
    provider: Arc<dyn LlmProvider>,
    model: String,
    retry_policy: &RetryPolicy,
    bus: &EventBus,
    skills_section: &str,
    cancel: CancellationToken,
) -> Result<AgentOutput, OrchestratorError> {
    // Text-only outcomes use the system prompt derived from the intent-gate
    // outcome (ADR-55 Phase 1e); append the enabled skills section (ADR-43) so
    // skill instructions apply in plain conversation too.
    let mut system = prompt.to_string();
    if !skills_section.is_empty() {
        system.push('\n');
        system.push_str(skills_section);
    }
    history.insert(
        0,
        Message {
            role: Role::System,
            content: system,
            tool_calls: None,
            tool_results: None,
            reasoning_content: None,
            tokens_in: None,
            tokens_out: None,
        },
    );
    history.push(Message {
        role: Role::User,
        content: task.description.clone(),
        tool_calls: None,
        tool_results: None,
        reasoning_content: None,
        tokens_in: None,
        tokens_out: None,
    });
    let estimated_tokens_in =
        history.iter().map(|message| message.content.len() as u64).sum::<u64>().div_ceil(4);
    let request = CompletionRequest {
        model: model.clone(),
        messages: history,
        tools: None,
        tool_choice: None,
        temperature: Some(0.3),
        max_tokens: Some(4096),
        stream: true,
    };
    let started = std::time::Instant::now();
    let (text, _, _, usage) = crate::prompts::complete_provider_request(
        &provider,
        &request,
        retry_policy,
        bus,
        task.session_id,
        task.id,
        &cancel,
    )
    .await?;
    // ADR-48 decision 4: provider-reported usage wins when present.
    let tokens_in = usage.as_ref().and_then(|u| u.prompt_tokens).unwrap_or(estimated_tokens_in);
    let tokens_out =
        usage.as_ref().and_then(|u| u.completion_tokens).unwrap_or((text.len() as u64).div_ceil(4));
    let metrics = ProviderMetrics {
        provider: provider.provider_name().to_string(),
        model,
        tokens_in,
        tokens_out,
        cost_usd: provider.approximate_cost(tokens_in, tokens_out),
        latency_ms: started.elapsed().as_millis() as u64,
    };
    Ok(AgentOutput {
        task_id: task.id,
        session_id: task.session_id,
        final_message: text,
        files_modified: Vec::new(),
        tool_call_count: 0,
        eval_result: None,
        tool_events: Vec::new(),
        verification: Vec::new(),
        project_root: None,
        completion_status: AgentCompletionStatus::Completed,
        provider_metrics: vec![metrics],
        checkpoint_json: None,
    })
}

/// Maximum plan→act→observe cycles for a single agent run. Five was far too
/// low for non-trivial build tasks (plan, write several files, verify, fix),
/// which caused the agent to stop mid-task when it hit the cap with no clear
/// signal — the "agent stops without reason" symptom. 25 leaves enough
/// headroom for multi-file changes while the cycle detector still bounds
/// runaway repetition.
const DEFAULT_MAX_ITERATIONS: u32 = 25;
use concerto_eval::EvalEngine;
use concerto_memory::embedder::{EmbeddingGenerator, ProviderEmbedder};
use concerto_memory::fts::SqliteFullTextStore;
use concerto_memory::indexer::{IndexConfig, ProjectIndexer};
use concerto_memory::storage::MemoryDb;
use concerto_memory::sync::ChunkSyncService;
use concerto_memory::vector_store::SqliteVectorStore;
use concerto_memory::watcher::{FileWatcher, ReindexQueueDrainer};
use concerto_memory::{
    decision_store::DecisionStore, fts::FullTextStore, task_tree::TaskTreeStore,
    vector_store::VectorStore,
};

/// Transition-only run-stage publisher (ADR-55 Phase 2a).
///
/// Tracks the current [`RunStage`] of the active run and publishes
/// [`EventKind::RunStageChanged`] to the bus *only when the stage actually
/// changes*: repeated signals of the same stage coalesce into a single event,
/// so no state is ever re-published. The dedupe lives here and only here —
/// callers just report the stage they are in.
///
/// Events are published through `publish_for_session` with the run's task id
/// as the correlation id, the same mechanism every other run-scoped event
/// uses, so replay/audit subscribers correlate them to the right session.
struct StageTracker {
    bus: EventBus,
    session_id: Ulid,
    task_id: TaskId,
    current: Option<RunStage>,
}

impl StageTracker {
    fn new(bus: EventBus, session_id: Ulid, task_id: TaskId) -> Self {
        Self { bus, session_id, task_id, current: None }
    }

    /// Advance to `stage`, publishing `RunStageChanged` only on transition.
    fn set(&mut self, stage: RunStage) {
        if self.current == Some(stage) {
            return;
        }
        self.current = Some(stage);
        let _ = self.bus.publish_for_session(
            self.session_id,
            self.task_id.0,
            EventKind::RunStageChanged { task_id: self.task_id, stage },
        );
    }
}

/// Request struct describing a single‑agent run.
pub struct AgentRunRequest {
    /// Full user input string.
    pub input: String,
    /// Provider ID chosen by the UI (if any).
    pub selected_provider_id: Option<String>,
    /// Model selected for this run. Overrides the selected provider's default.
    pub selected_model: Option<String>,
    /// Force the single-agent loop. When false, run the coordinator.
    pub force_single_agent: bool,
    /// Project root directory.
    pub project_dir: PathBuf,
    /// Session ID for persistent conversation history.
    pub session_id: Option<Ulid>,
    /// Previously persisted conversation messages for context.
    pub conversation_history: Vec<Message>,
    /// Whether project memory retrieval/indexing is enabled for this run.
    /// CLI fast mode disables it explicitly rather than silently ignoring the
    /// flag.
    pub memory_enabled: bool,
    /// Cancellation signal owned by the caller for this run.
    pub cancel_token: CancellationToken,
    /// Serialised orchestration checkpoint from a previous partial run.
    /// When present the coordinator skips `decompose_task` and resumes the
    /// graph directly from the checkpoint state.
    pub resume_checkpoint_json: Option<String>,
}

/// Groups all memory services for one active project, tracking the project id
/// so switching projects never reuses the previous project's memory system.
pub struct ActiveMemoryServices {
    pub project_id: ProjectId,
    pub store: Arc<dyn MemoryStore>,
    pub reindex: Arc<ProjectIndexer>,
    pub reindex_sync: Arc<ChunkSyncService>,
    pub cancel: CancellationToken,
}

/// Bundles services that are reused across calls.
pub struct SharedServices {
    pub bus: EventBus,
    pub config: AppConfig,
    /// Project-scoped memory services (store, indexer, sync, cancel).
    /// Switched when the active project changes.
    pub memory: Arc<Mutex<Option<ActiveMemoryServices>>>,
    /// Optional virtual filesystem (desktop only).
    pub vfs: Option<Arc<Mutex<VirtualFs>>>,
    /// Frontend-specific interactive approval presentation.
    pub approval_sink: Arc<dyn ApprovalSink>,
    /// Project‑scoped session manager for persistent conversations. When
    /// `None`, `run_shared_agent` lazily opens the default on‑disk store.
    pub session_manager: Option<Arc<ProjectSessionManager>>,
    /// Runtime-owned skills context (ADR-43, Task 4). Shared across prompt
    /// paths; a UI toggle (Task 7) calls `refresh` on this handle and the
    /// next prompt build picks up the new section.
    pub skills: Arc<crate::skills_context::SkillsContext>,
    /// Runtime-owned MCP server manager (ADR-43, Task 6). Constructed from
    /// config at build time; spawns nothing until `register_tools` is called
    /// (once per agent run). The UI (Task 7) reads live state via
    /// `server_state`/`servers`/`tools_for` and toggles servers via
    /// `start_server`/`stop_server`.
    pub mcp: Arc<concerto_mcp::McpManager>,
}

/// Simple no‑op audit logger used when no UI logging is required.
struct NoopAuditLog;

struct EventRecorderGuard {
    cancel: CancellationToken,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl EventRecorderGuard {
    async fn stop(mut self) {
        self.cancel.cancel();
        if let Some(task) = self.task.take() {
            if let Err(error) = task.await {
                tracing::warn!(%error, "session event recorder failed to stop cleanly");
            }
        }
    }
}

impl Drop for EventRecorderGuard {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

fn start_event_recorder(
    bus: &EventBus,
    store: Arc<dyn concerto_sessions::SessionStore>,
    session_id: Ulid,
) -> EventRecorderGuard {
    let mut receiver = bus.subscribe_durable();
    let cancel = CancellationToken::new();
    let task_cancel = cancel.clone();
    let task = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = task_cancel.cancelled() => {
                    while let Ok(event) = receiver.try_recv() {
                        if event.session_id != session_id {
                            tracing::debug!(
                                recorder_session = %session_id,
                                event_session = %event.session_id,
                                kind = ?event.kind,
                                "skipping event for different session",
                            );
                            continue;
                        }
                        if let Err(error) = store.record_event(session_id, &event, task_cancel.clone()).await {
                            tracing::warn!(%error, %session_id, "failed to flush session event");
                        }
                    }
                    break;
                },
                received = receiver.recv() => match received {
                    Some(event) => {
                        if event.session_id != session_id {
                            tracing::debug!(
                                recorder_session = %session_id,
                                event_session = %event.session_id,
                                kind = ?event.kind,
                                "skipping event for different session",
                            );
                        } else if let Err(error) = store.record_event(session_id, &event, task_cancel.clone()).await {
                            tracing::warn!(%error, %session_id, "failed to persist session event");
                        }
                    }
                    None => break,
                }
            }
        }
    });
    EventRecorderGuard { cancel, task: Some(task) }
}

/// Flush a batch once this many transcript-relevant events have accumulated in
/// the recorder's buffer. Entries are written in event order; anything still
/// buffered when the run ends is flushed by [`TranscriptRecorderGuard::stop`].
const TRANSCRIPT_FLUSH_THRESHOLD: usize = 32;

/// Correlation buffer shared between the transcript recorder task and its
/// guard methods. Entries are kept in event order until flushed to the store.
///
/// A `Running` tool call is deliberately kept buffered even across batches:
/// the store is append-only, so the terminal/approval event must merge into it
/// in place (ADR-36 §2) before anything behind it is written.
struct TranscriptRecorderState {
    pending: Vec<TranscriptEntry>,
}

impl TranscriptRecorderState {
    fn new() -> Self {
        Self { pending: Vec::new() }
    }

    /// Correlate one mapped event entry into the in-order buffer. A `Running`
    /// tool call stays buffered; a terminal tool event merges into the last
    /// `Running` entry with the same tool name (updating its status and
    /// appending the terminal detail), so exactly one entry per invocation is
    /// persisted.
    fn merge(&mut self, entry: TranscriptEntry) {
        match entry {
            TranscriptEntry::ToolCall { status: TranscriptToolStatus::Running, .. } => {
                self.pending.push(entry);
            }
            TranscriptEntry::ToolCall { tool_name, status, detail } => {
                let merge_index = self.pending.iter().rposition(|existing| {
                    matches!(existing,
                        TranscriptEntry::ToolCall { tool_name: n, status: TranscriptToolStatus::Running, .. } if *n == tool_name)
                });
                match merge_index {
                    Some(index) => {
                        if let TranscriptEntry::ToolCall {
                            status: existing_status,
                            detail: existing_detail,
                            ..
                        } = &mut self.pending[index]
                        {
                            *existing_status = status;
                            if !detail.is_empty() {
                                if existing_detail.is_empty() {
                                    *existing_detail = detail;
                                } else {
                                    existing_detail.push('\n');
                                    existing_detail.push_str(&detail);
                                }
                            }
                        }
                    }
                    // The start event was missed (e.g. it fired before this
                    // recorder subscribed or for another session): persist the
                    // terminal outcome as its own entry, defensively.
                    None => {
                        self.pending.push(TranscriptEntry::ToolCall { tool_name, status, detail })
                    }
                }
            }
            other => self.pending.push(other),
        }
    }

    /// Extract the longest in-order prefix of settled entries (everything
    /// before the first still-`Running` tool call), preserving event order in
    /// the store.
    fn drain_settled_prefix(&mut self) -> Vec<TranscriptEntry> {
        let flushable = self
            .pending
            .iter()
            .position(|entry| {
                matches!(
                    entry,
                    TranscriptEntry::ToolCall { status: TranscriptToolStatus::Running, .. }
                )
            })
            .unwrap_or(self.pending.len());
        self.pending.drain(..flushable).collect()
    }

    /// When enough entries have accumulated, flush the settled prefix. Returns
    /// an empty vec when the batch is not yet large enough or a `Running`
    /// entry still holds the front of the buffer.
    fn drain_batch(&mut self) -> Vec<TranscriptEntry> {
        if self.pending.len() < TRANSCRIPT_FLUSH_THRESHOLD {
            return Vec::new();
        }
        self.drain_settled_prefix()
    }

    /// ADR-36 settle-on-stop: any tool call still `Running` at run end is
    /// recorded as `Cancelled` (mirrors the desktop `settle_running_tool_calls`).
    fn settle_running(&mut self) {
        for entry in self.pending.iter_mut() {
            if let TranscriptEntry::ToolCall { status, .. } = entry {
                if *status == TranscriptToolStatus::Running {
                    *status = TranscriptToolStatus::Cancelled;
                }
            }
        }
    }
}

/// Persist a batch of transcript entries, best-effort: on failure log a
/// warning and drop the batch, keeping the recorder running (matches the event
/// recorder's `record_event` resilience).
async fn flush_entries(
    store: &Arc<dyn concerto_sessions::SessionStore>,
    session_id: Ulid,
    entries: Vec<TranscriptEntry>,
    cancel: &CancellationToken,
) {
    if entries.is_empty() {
        return;
    }
    if let Err(error) = store.append_transcript(session_id, &entries, cancel.clone()).await {
        tracing::warn!(%error, %session_id, "failed to flush transcript entries");
    }
}

/// Map one bus event to a transcript entry and merge it into the recorder's
/// in-order buffer, flushing a batch once the buffer is large enough.
///
/// ADR-58 P2+P3 (F8): gate labels come from the resolved blueprint (threaded
/// through the recorder), so a renamed review/validate stage renders its
/// configured label; the defaults keep the standard blueprint byte-identical.
async fn handle_transcript_event(
    event: &Event,
    state: &Arc<Mutex<TranscriptRecorderState>>,
    store: &Arc<dyn concerto_sessions::SessionStore>,
    session_id: Ulid,
    flush_cancel: &CancellationToken,
    gate_labels: &GateLabels,
) {
    let Some(entry) = transcript_entry_from_event_with_labels(&event.kind, gate_labels) else {
        return;
    };
    let batch = {
        let mut state = state.lock().unwrap_or_else(|error| error.into_inner());
        state.merge(entry);
        state.drain_batch()
    };
    flush_entries(store, session_id, batch, flush_cancel).await;
}

/// Filter events from other sessions and forward this session's events to the
/// transcript recorder (mirrors the event recorder's skip style).
async fn handle_bus_event(
    event: Arc<Event>,
    session_id: Ulid,
    state: &Arc<Mutex<TranscriptRecorderState>>,
    store: &Arc<dyn concerto_sessions::SessionStore>,
    flush_cancel: &CancellationToken,
    gate_labels: &GateLabels,
) {
    if event.session_id != session_id {
        tracing::debug!(
            recorder_session = %session_id,
            event_session = %event.session_id,
            kind = ?event.kind,
            "skipping transcript event for different session",
        );
        return;
    }
    handle_transcript_event(&event, state, store, session_id, flush_cancel, gate_labels).await;
}

struct TranscriptRecorderGuard {
    cancel: CancellationToken,
    /// Never cancelled while the guard lives; used for every `append_transcript`
    /// call so the final flush succeeds even when the run (or this recorder)
    /// was cancelled.
    flush_cancel: CancellationToken,
    task: Option<tokio::task::JoinHandle<()>>,
    store: Arc<dyn concerto_sessions::SessionStore>,
    session_id: Ulid,
    state: Arc<Mutex<TranscriptRecorderState>>,
}

impl TranscriptRecorderGuard {
    /// Record the user's prompt at the front of the run (ADR-36 §4). Flushes
    /// immediately so the prompt is durable even if the run is interrupted.
    async fn record_user_message(&self, content: String) {
        self.append_entries(&[TranscriptEntry::User { content }]).await;
    }

    /// Append entries through the recorder's in-order buffer and flush any
    /// settled prefix right away. Used for the run-start user prompt and the
    /// run-end assistant/completion entries so they are durably persisted
    /// before the recorder is stopped.
    async fn append_entries(&self, entries: &[TranscriptEntry]) {
        let batch = {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            state.pending.extend_from_slice(entries);
            state.drain_settled_prefix()
        };
        flush_entries(&self.store, self.session_id, batch, &self.flush_cancel).await;
    }

    /// Stop the recorder: cancel the background task, let it drain any events
    /// still in flight, then settle still-`Running` tool calls and flush
    /// everything remaining in order. Must run after the run-end entries are
    /// recorded so the final Assistant/Completion entries land in the DB.
    async fn stop(mut self) {
        self.cancel.cancel();
        if let Some(task) = self.task.take() {
            if let Err(error) = task.await {
                tracing::warn!(%error, "transcript recorder task failed to stop cleanly");
            }
        }
        let batch = {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            state.settle_running();
            std::mem::take(&mut state.pending)
        };
        flush_entries(&self.store, self.session_id, batch, &self.flush_cancel).await;
    }
}

impl Drop for TranscriptRecorderGuard {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

/// ADR-58 P2+P3 (F8): resolve the review/validate gate labels for transcript
/// activity entries from the resolved blueprint's stage definitions.
///
/// The canonical stage labels ("Review"/"Validate") on the default `standard`
/// blueprint produce the pre-blueprint transcript strings ("Reviewer" /
/// "Validator") via [`GateLabels::default`], keeping transcripts byte-
/// identical. A custom blueprint that renames a gate stage surfaces its
/// configured label in live and restored transcripts instead. Without a
/// resolved blueprint (tests, `[orchestration]`-less configs) the canonical
/// labels are used. Gates are resolved by kind, so renamed review/validate
/// tags still surface their labels (issue #150).
fn gate_labels_for_resolved(resolved: Option<&ResolvedBlueprint>) -> GateLabels {
    let Some(resolved) = resolved else { return GateLabels::default() };
    let facade = BlueprintFacade::new(resolved);
    let mut labels = GateLabels::default();
    if let Some(stage) = facade.first_stage_of_kind(StageKind::Review) {
        if stage.def.label != "Review" {
            labels.review = stage.def.label.clone();
        }
    }
    if let Some(stage) = facade.first_stage_of_kind(StageKind::Acceptance) {
        if stage.def.label != "Validate" {
            labels.validate = stage.def.label.clone();
        }
    }
    labels
}

fn start_transcript_recorder(
    bus: &EventBus,
    store: Arc<dyn concerto_sessions::SessionStore>,
    session_id: Ulid,
    gate_labels: GateLabels,
) -> TranscriptRecorderGuard {
    let mut receiver = bus.subscribe_durable();
    let cancel = CancellationToken::new();
    let flush_cancel = CancellationToken::new();
    let state = Arc::new(Mutex::new(TranscriptRecorderState::new()));
    let task_cancel = cancel.clone();
    let task_flush_cancel = flush_cancel.clone();
    let task_state = state.clone();
    let task_store = store.clone();
    let task_labels = gate_labels;
    let task = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = task_cancel.cancelled() => {
                    // Best-effort drain of events that were in flight when
                    // cancellation was requested; the final flush in stop()
                    // persists whatever remains buffered.
                    while let Ok(event) = receiver.try_recv() {
                        handle_bus_event(event, session_id, &task_state, &task_store, &task_flush_cancel, &task_labels).await;
                    }
                    break;
                }
                received = receiver.recv() => match received {
                    Some(event) => {
                        handle_bus_event(event, session_id, &task_state, &task_store, &task_flush_cancel, &task_labels).await;
                    }
                    None => break,
                }
            }
        }
    });
    TranscriptRecorderGuard { cancel, flush_cancel, task: Some(task), store, session_id, state }
}

#[async_trait]
impl AuditLog for NoopAuditLog {
    async fn record(
        &self,
        _entry: AuditEntry,
        _cancel: CancellationToken,
    ) -> Result<(), PolicyError> {
        Ok(())
    }
}

/// Open (or create) the SQLite pool for the append-only audit log.
/// Uses a separate connection from the session store so audit writes
/// never block session operations.
async fn create_audit_pool(
    data_dir: &std::path::Path,
) -> Result<sqlx::SqlitePool, OrchestratorError> {
    std::fs::create_dir_all(data_dir).map_err(|e| {
        OrchestratorError::AgentLoopError(format!("failed to create data directory: {e}"))
    })?;

    let db_path = data_dir.join("sessions.db");
    let options =
        sqlx::sqlite::SqliteConnectOptions::new().filename(&db_path).create_if_missing(true);
    let pool = sqlx::SqlitePool::connect_with(options).await.map_err(|e| {
        OrchestratorError::AgentLoopError(format!("failed to connect to audit DB: {e}"))
    })?;

    sqlx::query("PRAGMA journal_mode=WAL;")
        .execute(&pool)
        .await
        .map_err(|e| OrchestratorError::AgentLoopError(format!("audit PRAGMA error: {e}")))?;
    sqlx::query("PRAGMA foreign_keys=ON;")
        .execute(&pool)
        .await
        .map_err(|e| OrchestratorError::AgentLoopError(format!("audit PRAGMA error: {e}")))?;
    sqlx::migrate!("../sessions/migrations")
        .run(&pool)
        .await
        .map_err(|e| OrchestratorError::AgentLoopError(format!("audit migration error: {e}")))?;

    Ok(pool)
}

/// Resolve the provider based on the selected ID and configuration.
///
/// Model-first strategy:
/// 1. If a provider ID is explicitly selected, find that config and build it.
/// 2. If a model name is given, resolve the provider that offers it.
/// 3. Use `global_default_model` to select a provider.
/// 4. Fall back to the first configured provider.
/// 5. Legacy `primary_provider_config`.
/// 6. Environment-variable-based providers.
///
///    Never silently falls back to MockProvider — missing config is a hard error.
fn resolve_provider(
    config: &AppConfig,
    selected: Option<String>,
    selected_model: Option<&str>,
    plugin_providers: &std::collections::HashMap<String, Arc<dyn LlmProvider>>,
) -> Result<Arc<dyn LlmProvider>, OrchestratorError> {
    let creds = CredentialStore::new();
    let build = |provider: &concerto_config::ProviderConfig| {
        let mut provider = provider.clone();
        if let Some(model) = selected_model.filter(|model| !model.trim().is_empty()) {
            provider.model = model.to_string();
        }
        ProviderFactory::build(&provider, &creds)
    };

    // 1. Preferred ID from UI — look up the provider by id
    if let Some(id) = selected.as_deref() {
        if let Some(provider) = plugin_providers.get(id) {
            return Ok(provider.clone());
        }
        if let Some(ms) = &config.model_settings {
            if let Some(p) = ms.providers.iter().find(|p| p.id == id) {
                return build(p).map_err(OrchestratorError::Provider);
            }
        }
    }

    // 2. Model-first resolution: if a model name is explicitly given, find
    //    the provider that offers it.
    if let Some(model) = selected_model.filter(|m| !m.trim().is_empty()) {
        if let Some(ms) = &config.model_settings {
            if let Some(p) = ProviderFactory::config_for_model(ms, model, selected.as_deref()) {
                return build(p).map_err(OrchestratorError::Provider);
            }
        }
    }

    // 3. Global default model — use config_for_model with no preference
    if let Some(ms) = &config.model_settings {
        if let Some(default_model) = &ms.global_default_model {
            if let Some(p) = ProviderFactory::config_for_model(ms, default_model, None) {
                return build(p).map_err(OrchestratorError::Provider);
            }
        }

        // 4. First configured provider in model_settings
        if let Some(p) = ms.providers.first() {
            return build(p).map_err(OrchestratorError::Provider);
        }
    }

    // 5. Legacy single‑provider config
    if let Some(pc) = &config.primary_provider_config {
        return build(pc).map_err(OrchestratorError::Provider);
    }

    if config.primary_provider.as_deref() == Some("plugin") || config.primary_provider.is_none() {
        if let Some(provider) = plugin_providers.values().next() {
            return Ok(provider.clone());
        }
    }

    // 6. Env‑var fallbacks
    if let Ok(key) = std::env::var("ANTHROPIC_API_KEY") {
        tracing::info!("no provider config; using ANTHROPIC_API_KEY env fallback");
        return Ok(Arc::new(concerto_providers::anthropic::AnthropicProvider::new(
            key,
            "claude-sonnet-4-6".to_string(),
            15,
        )));
    }
    if let Ok(key) = std::env::var("OPENAI_API_KEY") {
        tracing::info!("no provider config; using OPENAI_API_KEY env fallback");
        return Ok(Arc::new(concerto_providers::openai::OpenAiProvider::new(
            key,
            "gpt-4o".to_string(),
            15,
        )));
    }

    // 7. No provider configured — hard error, no mock fallback
    Err(OrchestratorError::AgentLoopError(
        "no LLM provider configured. Add a provider entry in config or set ANTHROPIC_API_KEY/OPENAI_API_KEY".to_string(),
    ))
}

fn resolve_model_id(
    config: &AppConfig,
    selected_provider_id: Option<&str>,
    selected_model: Option<&str>,
) -> String {
    if let Some(model) = non_empty(selected_model) {
        return model.to_string();
    }
    if let Some(settings) = &config.model_settings {
        if let Some(id) = non_empty(selected_provider_id) {
            if let Some(provider) = settings
                .providers
                .iter()
                .find(|provider| ProviderFactory::config_id(provider) == id)
            {
                return provider.model.trim().to_string();
            }
        }
        if let Some(model) = non_empty(settings.global_default_model.as_deref()) {
            return model.to_string();
        }
        if let Some(provider) = settings.providers.first() {
            return provider.model.trim().to_string();
        }
    }
    config
        .primary_provider_config
        .as_ref()
        .map(|provider| provider.model.trim().to_string())
        .unwrap_or_else(|| "claude-sonnet-4-20250514".to_string())
}

/// Initialise or reuse the memory system scoped to a project root.
pub async fn init_memory_system(
    bus: EventBus,
    config: &AppConfig,
    project_dir: &std::path::Path,
    reindex: &Arc<Mutex<Option<Arc<ProjectIndexer>>>>,
    reindex_sync: &Arc<Mutex<Option<Arc<ChunkSyncService>>>>,
    memory_cancel: &Arc<Mutex<Option<CancellationToken>>>,
) -> Result<Arc<dyn MemoryStore>, OrchestratorError> {
    // Re‑use the same implementation as CLI/Desktop apps – copy/paste the
    // `init_memory_system` logic from those modules (project ID hashing, DB
    // path, vector & FTS stores, optional embedder, background indexing).
    let project_id = ProjectId(concerto_core::helpers::project_id_hash(project_dir));
    let lifecycle = CancellationToken::new();
    let previous =
        memory_cancel.lock().unwrap_or_else(|error| error.into_inner()).replace(lifecycle.clone());
    if let Some(previous) = previous {
        previous.cancel();
    }

    let data_dir = concerto_sessions::app_data_dir()
        .map_err(|e| OrchestratorError::AgentLoopError(format!("data directory error: {e}")))?
        .join("memory");
    let db_path = data_dir.join("memory.db");
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| OrchestratorError::AgentLoopError(format!("IO error: {}", e)))?;
    }
    let db = Arc::new(
        MemoryDb::connect(
            &camino::Utf8PathBuf::from_path_buf(db_path)
                .map_err(|e| OrchestratorError::AgentLoopError(format!("Path error: {:?}", e)))?,
        )
        .await
        .map_err(|e| OrchestratorError::AgentLoopError(format!("MemoryDb connect error: {}", e)))?,
    );
    let pool = db.pool().clone();
    let vector_store: Arc<dyn VectorStore> = Arc::new(
        SqliteVectorStore::new(pool.clone())
            .await
            .map_err(|e| OrchestratorError::AgentLoopError(format!("VectorStore error: {}", e)))?,
    );
    let fts_store: Arc<dyn FullTextStore> =
        Arc::new(SqliteFullTextStore::new(pool.clone()).await.map_err(|e| {
            OrchestratorError::AgentLoopError(format!("FullTextStore error: {}", e))
        })?);
    let ttl = concerto_memory::ttl::TtlManager::with_default_ttl_days(
        vector_store.clone(),
        fts_store.clone(),
        pool.clone(),
        config.memory.ttl_days,
    );
    if let Err(error) = ttl.purge_expired(&project_id, CancellationToken::new()).await {
        tracing::warn!(%error, "failed to purge expired project memory");
    }
    let decision_store = DecisionStore::load(db.clone()).await.map_err(|error| {
        OrchestratorError::AgentLoopError(format!("DecisionStore load error: {error}"))
    })?;
    let task_tree = TaskTreeStore::load(db.clone()).await.map_err(|error| {
        OrchestratorError::AgentLoopError(format!("TaskTreeStore load error: {error}"))
    })?;

    // Local fastembed embedder (BAAI/bge-small-en-v1.5). The model binary
    // downloads on first `embed` call; indexing is best‑effort and falls back
    // to FTS‑only when embedding is unavailable (e.g. offline).
    let embedder: Arc<dyn EmbeddingGenerator> =
        Arc::new(ProviderEmbedder::new("bge-small-en-v1.5"));

    // Chunk sync service is the single write path for the vector + FTS stores.
    let sync = Arc::new(ChunkSyncService::new(vector_store.clone(), fts_store.clone()));
    let indexer = Arc::new(ProjectIndexer::new(embedder.clone(), bus.clone(), project_id.clone()));

    // Expose the live indexer + sync so the UI can trigger re-indexes.
    *reindex.lock().unwrap_or_else(|e| e.into_inner()) = Some(indexer.clone());
    *reindex_sync.lock().unwrap_or_else(|e| e.into_inner()) = Some(sync.clone());

    // NOTE (ADR-54): global memory is part of the tiered memory system, which
    // is not implemented yet. It is intentionally STUBBED here: no global
    // database is opened or connected (a missing or unreadable
    // `global_memory.db` can no longer abort a run), and `None` is passed for
    // the global store — `MemorySystem` treats an absent global store
    // gracefully and never routes to it. The `GlobalMemoryStore` type,
    // `MemoryNamespace::Global`, and the `MemorySystem` wiring remain in
    // place; re-enable this block when the tiered system lands.
    let system = concerto_memory::system::MemorySystem::new(
        vector_store,
        fts_store,
        decision_store,
        task_tree,
        Some(embedder.clone()),
        project_id.clone(),
        None, // global store (stubbed, ADR-54)
    );
    let system = Arc::new(system);

    // Background project indexing — actually persists chunks (FTS + vectors).
    let mut index_config = IndexConfig {
        project_dir: camino::Utf8PathBuf::from_path_buf(project_dir.to_path_buf())
            .unwrap_or_else(|p| camino::Utf8PathBuf::from(p.to_string_lossy().as_ref())),
        ..IndexConfig::default()
    };
    index_config.exclude_patterns.extend(config.memory.exclude_patterns.clone());
    index_config.ignore_file = config.memory.ignore_file.clone();
    let indexer_bg = indexer.clone();
    let sync_bg = sync.clone();
    let pid_bg = project_id.clone();
    let index_cancel = lifecycle.child_token();

    // File watcher + reindex queue: enqueue changed files and drain by
    // re‑indexing them (rather than only marking them processed).
    let watcher = FileWatcher::new(bus.clone(), project_id.clone());
    let watch = match watcher.watch(project_dir, lifecycle.child_token()).await {
        Ok(watch) => Some(watch),
        Err(error) => {
            tracing::warn!(%error, "failed to start file watcher for project indexing");
            None
        }
    };
    let drainer = Arc::new(ReindexQueueDrainer::with_indexer_and_sync(
        Some(pool.clone()),
        indexer.clone(),
        sync.clone(),
        project_id.clone(),
        index_config.clone(),
    ));
    let drainer_cancel = lifecycle.child_token();
    tokio::spawn(async move {
        tracing::info!("starting background project indexing for {pid_bg}");
        match indexer_bg.index(&index_config, index_cancel.clone()).await {
            Ok(records) if !index_cancel.is_cancelled() => {
                match sync_bg.replace_project(&pid_bg, &records, index_cancel.clone()).await {
                    Ok(()) => {
                        tracing::info!(count = records.len(), "project indexing completed");
                    }
                    Err(error) => tracing::error!(%error, "failed to replace project index"),
                }
            }
            Ok(_) => tracing::debug!("project indexing cancelled before reconciliation"),
            Err(error) => tracing::error!(%error, "project indexing failed"),
        }

        if let Err(error) = drainer.drain(drainer_cancel.clone()).await {
            tracing::warn!(%error, "failed to drain queued memory re-index jobs");
        }
        let Some(mut watch) = watch else {
            return;
        };
        while let Some(mut paths) = watch.recv().await {
            while let Ok(mut queued_paths) = watch.try_recv() {
                paths.append(&mut queued_paths);
            }
            paths.sort();
            paths.dedup();
            for path in paths {
                if let Err(error) = drainer.enqueue(&pid_bg, Path::new(&path), "file_changed").await
                {
                    tracing::warn!(%error, %path, "failed to queue memory re-index");
                }
            }
            if let Err(error) = drainer.drain(drainer_cancel.clone()).await {
                tracing::warn!(%error, "failed to drain queued memory re-index jobs");
            }
        }
    });

    Ok(system as Arc<dyn MemoryStore>)
}

/// Load and initialise WASM plugins, registering their tools and collecting
/// provider instances. Errors are logged silently — a missing plugin host or
/// invalid WASM module never fails the agent run.
async fn load_and_configure_plugins(
    config: &AppConfig,
    project_dir: &std::path::Path,
    registry: &mut ToolRegistry,
) -> HashMap<String, Arc<dyn LlmProvider>> {
    let mut plugin_providers = HashMap::new();
    let Some(ref plugin_cfg) = config.plugins else {
        return plugin_providers;
    };
    if !plugin_cfg.enabled || !plugin_cfg.auto_load {
        if plugin_cfg.enabled {
            tracing::info!(
                "WASM plugins enabled but auto_load=false — no frontend loads them automatically"
            );
        }
        return plugin_providers;
    }

    use concerto_plugins::capability::{
        CapabilityDiscriminant, CapabilityManager, GrantedCapabilities,
    };
    use concerto_plugins::discovery::DiscoveryConfig as PluginDiscoveryCfg;
    use concerto_plugins::host::PluginHost;
    use concerto_plugins::manager::PluginManager;

    let Ok(host) = PluginHost::new() else {
        tracing::warn!("failed to create WASM plugin host — continuing without plugins");
        return plugin_providers;
    };
    let host = Arc::new(host);

    // Start the epoch ticker for WASM interruption (belt-and-suspenders with fuel).
    // This runs in the background and periodically increments the engine epoch,
    // allowing long-running plugins to be interrupted after EPOCH_DEADLINE ticks
    // (~EPOCH_BUDGET_SECS of wall-clock time at the configured interval).
    let _epoch_ticker = host.start_epoch_ticker(PluginHost::EPOCH_TICKER_INTERVAL_MS);

    let data_dir = concerto_sessions::app_data_dir()
        .unwrap_or_else(|_| std::path::PathBuf::from(".").join("concerto"))
        .join("plugins");
    let Ok(cap_mgr) = CapabilityManager::open(&data_dir) else {
        tracing::warn!("failed to open capability store — continuing without plugins");
        return plugin_providers;
    };
    let mut manager = PluginManager::new(host.clone(), cap_mgr, None, None);
    let search_paths: Vec<std::path::PathBuf> =
        plugin_cfg.search_paths.iter().map(std::path::PathBuf::from).collect();
    let disc_cfg = PluginDiscoveryCfg { search_paths, bundled_path: None };

    let Ok(candidates) = manager.discover(disc_cfg) else {
        tracing::warn!("plugin discovery failed — continuing without plugins");
        return plugin_providers;
    };

    for candidate in &candidates {
        let wasm_bytes = match std::fs::read(&candidate.wasm_path) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(
                    path = %candidate.wasm_path.display(),
                    error = %e,
                    "failed to read plugin WASM"
                );
                continue;
            }
        };
        let loader = concerto_plugins::loader::PluginLoader::new(host.clone());
        let loaded = match loader.load_from_bytes(&wasm_bytes, &candidate.wasm_path).await {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!(
                    path = %candidate.wasm_path.display(),
                    error = %e,
                    "failed to load plugin module"
                );
                continue;
            }
        };

        // Auto-approve all capabilities (trusted plugins).
        let mut granted = GrantedCapabilities::new();
        granted.set_root(project_dir.to_path_buf());
        for cap in &loaded.manifest.capabilities_required {
            let disc: CapabilityDiscriminant = cap.into();
            let scope: concerto_plugins::capability::CapabilityScope = cap.into();
            granted.grant_session(disc, scope);
        }

        let plugin_id = loaded.manifest.id.clone();
        match manager.initialise_plugin(&loaded, granted).await {
            Ok(()) => {
                if let Err(e) = manager.register_tools(&plugin_id, registry) {
                    tracing::warn!(
                        plugin_id = %plugin_id,
                        error = %e,
                        "failed to register plugin tools"
                    );
                } else {
                    tracing::info!(
                        plugin_id = %plugin_id,
                        "plugin loaded (auto-approved)"
                    );
                }
            }
            Err(e) => {
                tracing::warn!(
                    plugin_id = %plugin_id,
                    error = %e,
                    "failed to initialise plugin"
                );
            }
        }
    }
    match manager.collect_providers().await {
        Ok(providers) => plugin_providers = providers,
        Err(error) => tracing::warn!(
            %error,
            "failed to collect plugin-backed providers"
        ),
    }
    plugin_providers
}

/// Build the tool registry with filesystem, shell, git, and LSP tools.
///
/// The filesystem tool is anchored to the project directory (not the CWD).
/// The shell tool uses the canonical selected profile when available.
fn build_tool_registry(
    project_dir: &std::path::Path,
    vfs: &Option<Arc<Mutex<VirtualFs>>>,
    config: &AppConfig,
) -> ToolRegistry {
    let mut registry = ToolRegistry::default();
    let cwd_path: camino::Utf8PathBuf = if project_dir.as_os_str().is_empty() {
        std::env::current_dir()
            .map(|p| camino::Utf8PathBuf::from(p.to_string_lossy().as_ref()))
            .unwrap_or_default()
    } else {
        camino::Utf8PathBuf::from_path_buf(project_dir.to_path_buf()).unwrap_or_default()
    };
    if let Some(vfs) = vfs {
        registry.register(Box::new(FilesystemTool::new_shared(cwd_path.clone(), vfs.clone())));
    } else {
        registry.register(Box::new(FilesystemTool::new(cwd_path.clone())));
    }
    let shell_tool = {
        let settings = config.resolved_shell_settings();
        match settings.selected_profile() {
            Some(profile) => ShellTool::with_profile(profile.clone(), true),
            None => ShellTool::allow_all(),
        }
    };
    registry.register(Box::new(shell_tool));
    registry.register(Box::new(GitTool));

    // LSP tools — unconditional registration; each tool lazily starts the LSP
    // server on first use. If the server is not installed the tool returns a
    // recoverable error at call time.
    registry.register(Box::new(GetHover));
    registry.register(Box::new(FindReferences));
    registry.register(Box::new(RenameSymbol));
    registry.register(Box::new(GetDiagnostics));
    registry.register(Box::new(GetSemanticTokens));
    registry.register(Box::new(GetCodeActions));
    registry.register(Box::new(ExecuteCodeAction));
    registry.register(Box::new(GetInlayHints));

    registry
}

/// Resolve both the provider and the effective model ID from configuration.
///
/// Delegates to the existing [`resolve_model_id`] and [`resolve_provider`]
/// functions. Returns both values so callers need not duplicate the resolution
/// logic.
fn resolve_provider_and_model(
    config: &AppConfig,
    selected_provider_id: Option<String>,
    selected_model: Option<String>,
    plugin_providers: &HashMap<String, Arc<dyn LlmProvider>>,
) -> Result<(Arc<dyn LlmProvider>, String), OrchestratorError> {
    let model =
        resolve_model_id(config, selected_provider_id.as_deref(), selected_model.as_deref());
    let provider = resolve_provider(
        config,
        selected_provider_id,
        selected_model.as_deref(),
        plugin_providers,
    )?;
    Ok((provider, model))
}

/// Create the policy engine, audit log, spend tracker, and tool executor.
///
/// The spend cap is adjusted for multi-agent runs via the configured multiplier.
/// `intent_auth` (ADR-55) attaches the run's authorization state source to the
/// engine; `None` keeps behavior byte-identical to pre-ADR-55.
///
/// Also returns the shared policy engine and the session-DB pool (the same
/// `sessions.db` file the session store and audit log use). `run_shared_agent`
/// needs both to build the always-on in-process write gate (ADR-60 D4/D5): the
/// gate must evaluate with the exact policy the executor enforces and append
/// its whiteboard rows to the same durable DB.
#[allow(clippy::type_complexity)]
async fn setup_policy_and_audit(
    config: &AppConfig,
    approval_sink: Arc<dyn ApprovalSink>,
    registry: Arc<ToolRegistry>,
    force_single_agent: bool,
    bus: EventBus,
    intent_auth: Option<Arc<dyn IntentAuthorization>>,
) -> Result<
    (Arc<ToolExecutor>, Arc<SpendTracker>, Arc<dyn PolicyEngine>, Option<sqlx::SqlitePool>),
    OrchestratorError,
> {
    let mut policy_rules = config
        .policy
        .as_ref()
        .map(|p| p.to_rules())
        .filter(|rules| !rules.is_empty())
        .unwrap_or_else(PolicyPresets::default_rules);

    // MCP default posture (ADR-43 §6, AMEND-A3): unmatched mcp:* tools are
    // network-capable and must never be implicitly auto-approved. Append the
    // RequireApproval preset AFTER user rules so explicit user rules
    // (first-match-wins) keep precedence. Skipped when MCP is disabled or no
    // server is enabled.
    let mcp_has_enabled_server = config
        .mcp
        .as_ref()
        .filter(|mcp| mcp.enabled)
        .map(|mcp| mcp.servers.iter().any(|server| server.enabled))
        .unwrap_or(false);
    if mcp_has_enabled_server {
        policy_rules.push(PolicyRule::RequireApproval(Condition::ToolNamePrefix("mcp:".into())));
    }
    // ADR-55 §2 (B-3): custom user policy rules replace default_rules()
    // wholesale and may drop the bare `IntentAuthorized` gate rule, which
    // would leave the gate inert — still prompting and auditing but never
    // deciding. Whenever the gate's authorization provider is attached,
    // re-inject the gate rule after the leading deny-class rules so deny-first
    // ordering is preserved.
    if intent_auth.is_some() {
        policy_rules = inject_intent_gate_rule(policy_rules);
    }
    let (audit, gate_pool): (Arc<dyn AuditLog>, Option<sqlx::SqlitePool>) =
        match concerto_sessions::app_data_dir() {
            Ok(data_dir) => match create_audit_pool(&data_dir).await {
                Ok(pool) => (Arc::new(SqliteAuditLog::new(pool.clone())), Some(pool)),
                Err(e) => {
                    tracing::warn!(error = %e, "failed to open audit DB — audit logging disabled");
                    (Arc::new(NoopAuditLog), None)
                }
            },
            Err(e) => {
                tracing::warn!(error = %e, "no data directory available — audit logging disabled");
                (Arc::new(NoopAuditLog), None)
            }
        };
    let _rate_limiter = Arc::new(RpmLimiter::new(60));
    let spend_cap = if force_single_agent {
        config.session_spend_cap_usd
    } else {
        let multiplier = config
            .multi_agent
            .as_ref()
            .map(|multi| multi.spend_cap_multiplier)
            .filter(|multiplier| *multiplier > 0.0)
            .unwrap_or(3.0);
        config.session_spend_cap_usd.map(|cap| cap * multiplier)
    };
    let spend_tracker = Arc::new(SpendTracker::new(spend_cap, None, None));
    let mut policy_engine =
        SimplePolicyEngine::new(policy_rules, audit).with_spend_tracker(spend_tracker.clone());
    if let Some(auth) = intent_auth {
        policy_engine = policy_engine.with_intent_auth(auth);
    }
    policy_engine.validate().map_err(|error| OrchestratorError::InvalidPolicyConfiguration {
        reason: error.to_string(),
    })?;
    let policy = Arc::new(policy_engine);
    let executor = Arc::new(
        ToolExecutor::new(registry, policy.clone())
            .with_approval_sink(approval_sink)
            .with_event_bus(bus),
    );
    Ok((executor, spend_tracker, policy, gate_pool))
}

/// Create or resolve a session, start the event recorder, and refresh context.
///
/// Returns the resolved session ID, an optional session store reference, an
/// `EventRecorderGuard`, and a `TranscriptRecorderGuard` (ADR-36) that must be
/// stopped when the run completes.
#[allow(clippy::too_many_arguments)]
async fn create_session_and_recorder(
    services: &SharedServices,
    project_dir: &std::path::Path,
    provider_name: &str,
    model: &str,
    session_id: Option<Ulid>,
    bus: &EventBus,
    conversation_history: &mut Vec<Message>,
    cancel: CancellationToken,
) -> Result<
    (Ulid, Option<Arc<dyn SessionStore>>, EventRecorderGuard, TranscriptRecorderGuard),
    OrchestratorError,
> {
    let session_manager = match &services.session_manager {
        Some(manager) => manager.clone(),
        None => {
            let config = SessionManagerConfig {
                git_auto_init: services
                    .config
                    .tool_settings
                    .as_ref()
                    .map(|settings| settings.git_auto_init)
                    .unwrap_or(true),
            };
            Arc::new(ProjectSessionManager::connect_with_config(config).await.map_err(|error| {
                OrchestratorError::AgentLoopError(format!("session store unavailable: {error}"))
            })?)
        }
    };
    let session_store = Some(session_manager.store());
    let resolved_session_id = match session_id {
        Some(id) => {
            if session_manager
                .load_session(id, cancel.clone())
                .await
                .map_err(|error| {
                    OrchestratorError::AgentLoopError(format!("session lookup failed: {error}"))
                })?
                .is_none()
            {
                return Err(OrchestratorError::AgentLoopError(format!(
                    "session {id} does not exist"
                )));
            }
            id
        }
        None => {
            let project =
                camino::Utf8PathBuf::from_path_buf(project_dir.to_path_buf()).map_err(|path| {
                    OrchestratorError::AgentLoopError(format!(
                        "project path is not valid UTF-8: {}",
                        path.display()
                    ))
                })?;
            session_manager
                .get_or_create_active_session(&project, provider_name, model, cancel.clone())
                .await
                .map_err(|error| {
                    OrchestratorError::AgentLoopError(format!("session creation failed: {error}"))
                })?
                .session_id
        }
    };
    match crate::context_engine::ContextEngine::from_config(services.config.context.as_ref())
        .assemble(
            session_manager.store(),
            resolved_session_id,
            conversation_history,
            cancel.clone(),
            Some(bus),
        )
        .await
    {
        Ok(history) => *conversation_history = history,
        Err(error) => tracing::warn!(
            %error,
            %resolved_session_id,
            "failed to refresh durable context checkpoints"
        ),
    }
    let event_recorder = start_event_recorder(bus, session_manager.store(), resolved_session_id);
    // ADR-58 P2+P3 (F8): the review/validate gate labels for transcript
    // activity entries come from the resolved blueprint's stage definitions.
    // The default `standard` blueprint produces the canonical
    // "Reviewer"/"Validator" strings, keeping transcripts byte-identical.
    let gate_labels = gate_labels_for_resolved(services.config.resolved_blueprint.as_deref());
    let transcript_recorder =
        start_transcript_recorder(bus, session_manager.store(), resolved_session_id, gate_labels);
    Ok((resolved_session_id, session_store, event_recorder, transcript_recorder))
}

/// Build the overflow strategy, undo manager, eval engine, and AgentLoop, then
/// execute the single-agent task.
///
/// Run metrics and context maintenance are handled inline before returning.
///
/// `effective_outcome` is the intent gate's classified outcome for this run
/// (ADR-55 Phase 1e) and selects the system prompt.
#[allow(clippy::too_many_arguments)]
async fn execute_agent_loop(
    req: AgentRunRequest,
    services: &SharedServices,
    provider: Arc<dyn LlmProvider>,
    model: String,
    executor: crate::exec_backend::SharedExecutionBackend,
    memory: Arc<dyn MemoryStore>,
    session_store: Option<Arc<dyn SessionStore>>,
    session_id: Ulid,
    task: AgentTask,
    event_recorder: EventRecorderGuard,
    transcript_recorder: TranscriptRecorderGuard,
    effective_outcome: RequestedOutcome,
    // Gate-derived mutation grant (`effective_outcome == Execute` and the
    // run is not read-only). Threaded from `run_shared_agent` rather than
    // re-derived from `task.execution_mode` so the Execute stage can never
    // drift from the intent gate's decision if the task-shaping code changes.
    execute_granted: bool,
    stage_tracker: &Arc<Mutex<StageTracker>>,
) -> Result<AgentOutput, OrchestratorError> {
    // Audit C-03 (immediate fix): the LLM `SummarizeOldest` strategy is no
    // longer wired into the production runtime. Its failure path could delete
    // the original messages, and in-run LLM summarization is superseded by the
    // deterministic durable compaction in `context_compaction` —
    // `create_session_and_recorder` bounds the active history via the
    // context engine before the run; `maintain_context_after_run` checkpoints
    // after it. Overflow is handled there; passing this per-call strategy is
    // `None` keeps the mid-run message projection intact and is a safe no-op.
    // The `SummarizeOldest` type remains available for explicit opt-in use and
    // is exercised by `concerto-memory` tests.
    let overflow_strategy: Option<Arc<dyn concerto_core::ContextOverflowStrategy>> = {
        tracing::warn!(
            "in-run LLM overflow summarization disabled (audit C-03); context is bounded by \
             deterministic durable compaction"
        );
        None
    };

    let undo_manager = Arc::new(Mutex::new(UndoManager::new(&req.project_dir)));
    let eval = {
        let engine = EvalEngine::new(&req.project_dir);
        let settings = services.config.resolved_shell_settings();
        match settings.selected_profile() {
            Some(profile) => engine.with_shell_profile(profile.clone()),
            None => engine,
        }
    };
    let prompt_builder = PromptBuilder::with_skills(
        concerto_core::types::system_prompt_for(effective_outcome),
        Some(services.skills.clone()),
    );

    let retry_policy = RetryPolicy::new(services.config.retry.clone());
    let metrics_store = session_store.clone();
    let mut agent = AgentLoop::with_project_root(
        services.bus.clone(),
        services.approval_sink.clone(),
        provider,
        executor,
        memory,
        undo_manager,
        eval,
        prompt_builder,
        DEFAULT_MAX_ITERATIONS,
        false,
        req.project_dir.clone(),
        overflow_strategy,
        Some(concerto_memory::budget::ContextBudgetAllocator::default()),
    )
    .with_retry_policy(retry_policy)
    .with_usage_model(model)
    .with_initial_messages(req.conversation_history)
    .with_session_store(session_store);

    // `task` is moved into the loop below; Ulid is Copy so the task id is
    // captured up front for spend attribution (Phase 3, issue #93).
    let task_id = task.id.0;
    // Run-stage signals (ADR-55 Phase 2a): the loop is about to start, so the
    // run is Inspecting the workspace; a mutation-capable Execute run then
    // moves to Execute, a Plan run to Plan. Both are gate-derived decisions
    // available before the loop runs. Complete is reported only after `run`
    // returns Ok below — an Err or cancellation never advances the stage.
    //
    // `execute_granted` is the gate's own `effective_outcome == Execute &&
    // !gate_read_only` decision, computed in `run_shared_agent` and passed in:
    // `task.execution_mode` is built from that exact condition today
    // (`new_action_required` iff `task_action_required`), but deriving the
    // stage from the explicit grant keeps them decoupled.
    {
        let mut tracker = stage_tracker.lock().unwrap_or_else(|error| error.into_inner());
        tracker.set(RunStage::Inspect);
        if execute_granted {
            tracker.set(RunStage::Execute);
        } else if effective_outcome == RequestedOutcome::Plan {
            tracker.set(RunStage::Plan);
        }
    }
    let output = match agent.run(task, req.cancel_token.clone()).await {
        Ok(output) => {
            stage_tracker.lock().unwrap_or_else(|error| error.into_inner()).set(RunStage::Complete);
            output
        }
        Err(error) => {
            // Flush any partial transcript before surfacing the failure
            // (ADR-36): still-`Running` tool calls settle as Cancelled inside
            // stop().
            transcript_recorder.stop().await;
            event_recorder.stop().await;
            // Persist whatever the loop settled before failing (e.g. rate
            // limit): tokens consumed and cost accrued so far must not
            // vanish from the audit trail. Best-effort like the success tail.
            let settled = agent.provider_metrics();
            persist_provider_metrics(
                metrics_store.as_ref(),
                session_id,
                &settled,
                req.cancel_token.clone(),
            )
            .await;
            persist_spend_records(
                metrics_store.as_ref(),
                session_id,
                Some(task_id),
                &settled,
                req.cancel_token.clone(),
            )
            .await;
            return Err(error);
        }
    };
    // Final transcript entries (ADR-36 §4): assistant text + completion marker.
    // The orchestrator itself never publishes `AssistantMessage`, so this is
    // the only Assistant line for the single-agent run.
    transcript_recorder
        .append_entries(&[
            TranscriptEntry::Assistant { content: output.final_message.clone() },
            TranscriptEntry::Completion {
                multi_agent: false,
                completed: output.completion_status == AgentCompletionStatus::Completed,
                files: output.files_modified.iter().map(ToString::to_string).collect(),
                project_root: output.project_root.as_ref().map(ToString::to_string),
            },
        ])
        .await;
    persist_provider_metrics(
        metrics_store.as_ref(),
        session_id,
        &output.provider_metrics,
        req.cancel_token.clone(),
    )
    .await;
    // One spend record per settled provider call (the single-agent run
    // aggregates its usage into one metrics entry; best-effort).
    persist_spend_records(
        metrics_store.as_ref(),
        session_id,
        Some(task_id),
        &output.provider_metrics,
        req.cancel_token.clone(),
    )
    .await;
    maintain_context_after_run(
        metrics_store.as_ref(),
        session_id,
        services.config.context.as_ref(),
        req.cancel_token.clone(),
        Some(&services.bus),
    )
    .await;
    transcript_recorder.stop().await;
    event_recorder.stop().await;
    Ok(output)
}

/// Select the cached memory store when the previous run targeted the same
/// project, or `None` when a different (or no) project is active.
///
/// `None` means the caller must reset the previous project's lifecycle and
/// initialise fresh services — a project switch must never reuse the
/// previous project's store, indexer, or chunk-sync service (audit G1).
fn cached_store_for_project(
    memory: &Mutex<Option<ActiveMemoryServices>>,
    project_id: &ProjectId,
) -> Option<Arc<dyn MemoryStore>> {
    let lock = memory.lock().unwrap_or_else(|poison| poison.into_inner());
    lock.as_ref().and_then(|active| {
        if active.project_id == *project_id {
            Some(active.store.clone())
        } else {
            None
        }
    })
}

/// Cancel and drop the previous project's memory lifecycle. Called on a
/// project switch so the new project never inherits the previous one's
/// background indexer, chunk-sync service, or store.
fn reset_memory_services(memory: &Mutex<Option<ActiveMemoryServices>>) {
    if let Some(previous) = memory.lock().unwrap_or_else(|poison| poison.into_inner()).take() {
        previous.cancel.cancel();
    }
}

/// Select or initialise the project-scoped memory services for a run.
///
/// Same project as the previous run → reuse the cached store. Different
/// project → cancel and drop the previous project's lifecycle, then
/// initialise a fresh store for the new project. Returns `None` when memory
/// is disabled (the caller falls back to `NullMemoryStore`).
async fn select_or_init_memory_services(
    services: &SharedServices,
    project_dir: &Path,
    memory_enabled: bool,
) -> Result<Option<Arc<dyn MemoryStore>>, OrchestratorError> {
    if !memory_enabled {
        return Ok(None);
    }
    let project_id = ProjectId(concerto_core::helpers::project_id_hash(project_dir));
    if let Some(store) = cached_store_for_project(&services.memory, &project_id) {
        return Ok(Some(store));
    }
    // Different project (or no previous run): cancel + drop the old
    // lifecycle so no memory state leaks across project boundaries.
    reset_memory_services(&services.memory);

    // Create temporary wrappers for init_memory_system; it writes into them.
    let reindex_temp: Arc<Mutex<Option<Arc<ProjectIndexer>>>> = Arc::new(Mutex::new(None));
    let reindex_sync_temp: Arc<Mutex<Option<Arc<ChunkSyncService>>>> = Arc::new(Mutex::new(None));
    let cancel_temp: Arc<Mutex<Option<CancellationToken>>> = Arc::new(Mutex::new(None));
    let mem = init_memory_system(
        services.bus.clone(),
        &services.config,
        project_dir,
        &reindex_temp,
        &reindex_sync_temp,
        &cancel_temp,
    )
    .await
    .map_err(|e| OrchestratorError::AgentLoopError(format!("Memory init failed: {e}")))?;

    // Extract populated values from temp wrappers.
    let reindex =
        reindex_temp.lock().unwrap_or_else(|e| e.into_inner()).take().ok_or_else(|| {
            OrchestratorError::AgentLoopError(
                "init_memory_system did not populate project indexer".into(),
            )
        })?;
    let reindex_sync =
        reindex_sync_temp.lock().unwrap_or_else(|e| e.into_inner()).take().ok_or_else(|| {
            OrchestratorError::AgentLoopError(
                "init_memory_system did not populate chunk sync service".into(),
            )
        })?;
    let cancel = cancel_temp.lock().unwrap_or_else(|e| e.into_inner()).take().unwrap_or_default();

    let active =
        ActiveMemoryServices { project_id, store: mem.clone(), reindex, reindex_sync, cancel };
    *services.memory.lock().unwrap_or_else(|e| e.into_inner()) = Some(active);
    Ok(Some(mem))
}

/// ADR-55 Phase 2b: which prompts arm the real Apply/Replan dialog instead of
/// the generic intent gate (`None` fall-through).
///
/// Three cases arm it, newest-wins per match:
///
/// - **Exact-objective Execute replay** (Phase 1d, unchanged): a confident
///   Execute request whose input hash matches a stored binding.
/// - **Exact-objective replay under another outcome**: the user re-sends the
///   original plan-shaping prompt (the router re-classifies it — e.g. a
///   `plan1:` prompt lands in `Diagnose`, and re-sending it after the plan
///   renders must not silently re-run analysis). The stored binding is the
///   authoritative artifact for that objective, so the dialog offers
///   Apply/Replan. Excluded only for pure-text `Answer` replays.
/// - **Natural-language approval** ([`is_plan_approval_phrase`]): the input to
///   hash is an approval of the just-rendered plan, which routes as a fresh
///   `Plan` run because the router's `plan` keyword wins. The planning-only
///   run bound the rendered plan under its *own* input hash (M3), so the
///   session-wide newest binding is the plan being approved — surfacing the
///   dialog turns "i approve the plan" into an audited Apply instead of a
///   second plan presentation.
///
/// Every match is verified against the binding's artifact hash (ADR-55 §1
/// pending) before it arms the dialog: a binding whose plan text no longer
/// matches its creation-time hash — or an unverifiable legacy binding — falls
/// through to the generic gate.
pub fn bound_plan_for_approval(
    routing: &RouterOutput,
    session_id: Ulid,
    objective_hash: &str,
    input: &str,
) -> Option<PlanBinding> {
    let registry = plan_registry();
    if routing.outcome == RequestedOutcome::Execute
        && routing.confidence >= LOW_CONFIDENCE_THRESHOLD
    {
        if let Some(binding) = registry.pending(session_id, objective_hash) {
            return verified_binding(binding);
        }
    }
    if routing.outcome != RequestedOutcome::Answer {
        if let Some(binding) = registry.pending(session_id, objective_hash) {
            return verified_binding(binding);
        }
    }
    if is_plan_approval_phrase(input) {
        if let Some(binding) = registry.latest_for_session(session_id) {
            return verified_binding(binding);
        }
    }
    None
}

/// Short natural-language approvals of a rendered plan.
///
/// Every phrase names the plan or an approval of it; change-execution
/// phrasing like "apply the fix" (an Execute intent) never false-positives.
/// Bare approvals ("yes", "i approve") are intentionally included: the
/// lookup guard — a `(session_id)` binding must exist — keeps them inert
/// until a plan run actually rendered one, and the binding is consumed
/// (removed in-memory and in durable storage) when an Apply executes, so a
/// later bare "yes" in a fresh context cannot re-arm the dialog.
pub fn is_plan_approval_phrase(input: &str) -> bool {
    const PHRASES: &[&str] = &[
        "approve the plan",
        "approve plan",
        "approved the plan",
        "approved it",
        "approve it",
        "i approve",
        "i approve of",
        "i agree",
        "approved",
        "yes",
        "yes, approve",
        "yep",
        "sounds good",
        "looks good",
        "go ahead",
        "proceed",
        "do it",
        "apply the plan",
        "apply the stored plan",
        "apply plan",
        "apply it",
        "execute the plan",
        "execute plan",
        "run the plan",
        "run plan",
        "implement the plan",
        "proceed with the plan",
        "proceed with plan",
        "yes, execute the plan",
        "yes, apply the plan",
        "looks good, execute",
        "looks good, apply",
    ];
    /// Denials/hesitations that must never arm the dialog even though they
    /// contain an approval phrase ("don't approve the plan", "not yet").
    const NEGATIONS: &[&str] =
        &["don't", "do not", "not yet", "never", "wait", "hold on", "actually no"];
    let lower = input.trim().to_ascii_lowercase();
    if NEGATIONS.iter().any(|neg| lower.contains(neg)) {
        return false;
    }
    PHRASES.iter().any(|phrase| lower.contains(phrase))
}

/// ADR-55 §11: does `routing` ask for a confident Execute?
///
/// Mirrors the predicate [`bound_plan_for_approval`] and
/// [`crate::intent_grants::apply_intent_gate`] use (Execute with confidence at
/// or above [`LOW_CONFIDENCE_THRESHOLD`]), so the binding-driven arming path
/// engages exactly when the generic gate would already escalate to a user
/// confirmation — surfacing the stored plan text is strictly more informative
/// than a bare "may I execute?" ask.
fn is_confident_execute(routing: &RouterOutput) -> bool {
    routing.outcome == RequestedOutcome::Execute && routing.confidence >= LOW_CONFIDENCE_THRESHOLD
}

/// ADR-55 §11: arm the Apply/Replan dialog from the session's newest durable
/// plan binding when the router classified a confident Execute but neither
/// the approval-phrase nor the exact-objective-hash path armed one.
///
/// Round-4 live finding: a user typed "execute" after a `plan:` run; the
/// router classified a confident Execute, but because "execute" is neither an
/// approval phrase nor the plan objective hash, no binding armed — the
/// coordinator re-planned from "execute", the LLM planner returned empty
/// (retry + heuristic fallback), and no files were ever written, while the
/// session's newest durable plan sat unused. This path turns that "execute"
/// into the same audited Apply/Replan dialog the phrase/hash paths produce,
/// showing the STORED plan text.
///
/// Pure (no I/O) so the mapping is unit-testable: the caller loads
/// [`concerto_sessions::PlanBindingRecord`] and passes it in. `None` keeps
/// the run on the unchanged generic intent gate (fail-soft). The restored
/// binding is verified against its artifact hash (ADR-55 §1 pending): a
/// tampered or legacy unverifiable row falls through to the generic gate.
fn arm_binding_for_confident_execute(
    routing: &RouterOutput,
    session_record: Option<PlanBindingRecord>,
) -> Option<PlanBinding> {
    if !is_confident_execute(routing) {
        return None;
    }
    let record = session_record?;
    verified_binding(PlanBinding::restored(
        record.plan_id,
        record.objective_hash,
        record.source_revision,
        record.plan_text,
        record.artifact_hash,
        record.created_at,
    ))
}

/// Decide whether the run's task should be action-required (B-2).
///
/// `effective` carries the run's effective outcome and `gate_read_only`
/// reflects the gate's read-only state sampled right after `apply_intent_gate`:
///
/// - `Execute` + mutation-capable run → action-required.
/// - `Execute` + read-only run (a dismissed/absent confirmation keeps
///   effective Execute but the run is read-only) → answer-only, so the
///   `agent_loop` ever-retrying `ActionRequired` path never steers the model
///   toward a mutation the gate would deny (B-2).
/// - Any other effective outcome → answer-only; mutation limits are enforced
///   by the gate itself.
///
/// The intent gate is always on (ADR-55 Phase 1e) — there is no legacy mode
/// picker and `effective` is never `None`.
pub fn task_action_required(effective: RequestedOutcome, gate_read_only: bool) -> bool {
    matches!(effective, RequestedOutcome::Execute) && !gate_read_only
}

/// ADR-55 Phase 2b (M3, live-fix): an Apply run executes the APPROVED plan,
/// not the approval phrase ("i approve"). The stored, capped plan text is
/// what the user approved; the original ask rides in the transcript.
fn approved_plan_task_description(binding: &PlanBinding) -> String {
    format!(
        "Execute the approved plan (plan {}) for this objective:\n{}",
        binding.plan_id(),
        binding.plan_text()
    )
}

/// Build the run's task from the gate and plan-binding state (ADR-55 Phase
/// 2b, M3 live-fix).
///
/// An Apply run must describe the APPROVED plan rather than the approval
/// phrase that armed the dialog ("i approve"); without this the coordinator's
/// subtasks were literally built from the approval phrase and the Coder
/// produced nothing. The action-required/answer-only routing from B-2 is
/// otherwise untouched.
fn build_run_task(
    session_id: Ulid,
    action_required: bool,
    apply_plan: bool,
    applied_plan: Option<&PlanBinding>,
    input: &str,
) -> AgentTask {
    if apply_plan {
        if let Some(binding) = applied_plan {
            AgentTask::new_action_required(session_id, approved_plan_task_description(binding))
        } else {
            // Defensive: apply without a captured binding (should be
            // impossible — the Apply arm captures before consuming).
            AgentTask::new_action_required(session_id, input.to_owned())
        }
    } else if action_required {
        AgentTask::new_action_required(session_id, input.to_owned())
    } else {
        AgentTask::new(session_id, input.to_owned())
    }
}

/// Correlation id for the router's own decision row of one routing event
/// (ADR-55 Phase 2c §5/C4): the classifier's correlation id when a call
/// happened, otherwise a fresh per-event id. `Ulid::default()` is the all-zero
/// nil id — recording it would silently break the audit trail's correlation
/// chain for every non-classifier run, so it is never used here.
fn router_row_correlation_id(classifier_correlation_id: Option<Ulid>) -> Ulid {
    match classifier_correlation_id {
        Some(id) => id,
        None => Ulid::new(),
    }
}

/// Whether the LLM intent classifier should run for a deterministic route
/// (ADR-56 §1). The two fast paths — negation-override (a read-only safety
/// invariant, never model-overridable) and smalltalk (zero-cost chat) — run
/// BEFORE the classifier and win outright, so they never reach the provider.
/// Every other route (keyword/question hits and ask-user ambiguity alike) is
/// classifier-eligible.
///
/// The rule names are the `RULE_*` constants emitted by
/// `concerto_core::intent::route()`; they are crate-private there, so the
/// literals are matched here.
fn classifier_applies_to(route: &RouterRoute) -> bool {
    !matches!(
        route,
        RouterRoute::RuleHit { rule: "negation_override" }
            | RouterRoute::RuleHit { rule: "smalltalk" }
    )
}

/// Run a single‑agent task using shared components.
///
/// Orchestrates the full agent lifecycle by delegating to specialised helpers:
///
/// 1. `build_tool_registry`   — filesystem, shell, git, and LSP tools
/// 2. `load_and_configure_plugins` — WASM plugins + provider collection
/// 3. `register_tools` (MCP)  — namespaced `mcp:<server>:<tool>` bridge (ADR-43)
/// 4. `resolve_provider_and_model` — provider/model resolution from config
/// 5. `setup_policy_and_audit`     — policy engine, audit log, spend tracker
/// 6. `create_session_and_recorder` — session creation + event recording
/// 7. `execute_agent_loop`         — single-agent AgentLoop execution
///
/// For multi-agent runs the function delegates to `run_multi_agent` after the
/// shared setup phases are complete.
pub async fn run_shared_agent(
    mut req: AgentRunRequest,
    services: SharedServices,
) -> Result<AgentOutput, OrchestratorError> {
    // 1. Build tool registry (filesystem, shell, git, LSP tools)
    let mut registry = build_tool_registry(&req.project_dir, &services.vfs, &services.config);

    // 2. Load WASM plugins and collect plugin-backed providers
    let plugin_providers =
        load_and_configure_plugins(&services.config, &req.project_dir, &mut registry).await;

    // 3. MCP servers (ADR-43): bridge namespaced `mcp:<server>:<tool>` tools.
    // Runs after plugin tools so MCP can never clobber them; a failed or
    // duplicate server is marked `Failed` and never blocks startup. Tools of
    // servers connected on a previous run are re-bridged into this run's
    // fresh registry. Registration is fail-soft: per-server errors are logged
    // by the manager and only a config-level defect (e.g. duplicate server id
    // slipping past validation) surfaces here.
    if let Err(error) = services.mcp.register_tools(&mut registry).await {
        tracing::warn!(%error, "mcp registration failed; continuing without mcp tools");
    }
    let registry = Arc::new(registry);

    // 3. Resolve provider and final effective model
    let (provider, model) = resolve_provider_and_model(
        &services.config,
        req.selected_provider_id.clone(),
        req.selected_model.clone(),
        &plugin_providers,
    )?;

    // 4. Policy engine, audit log, spend tracker, tool executor.
    //
    // ADR-55 Phase 1e: the intent gate is the ONLY routing path and is always
    // on for every run (single- and multi-agent) — the `[intent]` config toggle
    // is removed with no replacement. Grants are per-run and non-durable
    // (ADR-55 §4): a fresh store per call means they can never cross sessions
    // and are re-confirmed on every resume.
    let store = Arc::new(IntentGrantStore::new());
    let auth = Arc::new(SessionIntentAuth::new(store.clone()));
    let intent_auth = Some(auth.clone() as Arc<dyn IntentAuthorization>);

    let (executor, spend_tracker, intent_policy, gate_log_pool) = setup_policy_and_audit(
        &services.config,
        services.approval_sink.clone(),
        registry.clone(),
        req.force_single_agent,
        services.bus.clone(),
        intent_auth,
    )
    .await?;

    // 5. Memory – reuse cached store or initialise a new one, scoped to
    // project. A project switch cancels and drops the previous project's
    // lifecycle so its store/indexer/sync services are never reused (audit
    // G1); `None` means memory is disabled and `NullMemoryStore` is used.
    let memory: Arc<dyn MemoryStore> =
        select_or_init_memory_services(&services, &req.project_dir, req.memory_enabled)
            .await?
            .unwrap_or_else(|| Arc::new(NullMemoryStore));

    // 6. Session creation and event recording
    let (session_id, session_store, event_recorder, transcript_recorder) =
        create_session_and_recorder(
            &services,
            &req.project_dir,
            provider.provider_name(),
            &model,
            req.session_id,
            &services.bus,
            &mut req.conversation_history,
            req.cancel_token.clone(),
        )
        .await?;

    // Record the user's prompt in the durable transcript up front (ADR-36 §4).
    // Text-only mode previously persisted no user message at all; the
    // transcript now carries the prompt for both modes.
    transcript_recorder.record_user_message(req.input.clone()).await;

    // 6b. Intent routing + user confirmation (ADR-55 §1/§2/§4/§6). Runs for
    // every run — the gate is always on (ADR-55 Phase 1e). The router
    // classifies; only a confirmed user decision grants — `None` from the sink
    // keeps the run read-only so a mutation never slips through unconfirmed.
    // Every routing decision is snapshotted to the audit through the
    // correlation-id chain (ADR-55 §5.2); a failed audit write is fail-soft.
    let mut routing = concerto_core::intent::route(&req.input, req.project_dir.clone());
    let plan_objective_hash = blake3::hash(req.input.as_bytes()).to_hex().to_string();

    // ADR-55 Phase 2c §5/C4 / ADR-56 §5: capture the router's own audit-row
    // route name BEFORE the intent classifier below can re-route `routing`.
    // The trail must record the deterministic outcome the classifier was asked
    // about — any route (ask-user ambiguity, a keyword hit, a question, ...),
    // never the classifier's own `llm_classifier` label. On fail-soft this
    // pre-replacement route is exactly what stands unchanged.
    let router_route = router_route_name(&routing.route);

    // Carry forward previous session spend so the cap looks at cumulative
    // cost. Recorded BEFORE the intent classifier so a session already at or
    // over its cap cannot fire a classifier call (ADR-55 Phase 2c §6
    // reserve-before-call ordering).
    let session_manager = services.session_manager.clone();
    if let Some(ref session_manager) = session_manager {
        if let Ok(Some(session)) =
            session_manager.load_session(session_id, req.cancel_token.clone()).await
        {
            spend_tracker.record(session.total_cost_usd);
        }
    }

    // ADR-56: model-first intent classification. When `[intent]
    // classifier_enabled` is true (the DEFAULT), the LLM classifier is the
    // intent authority for EVERY message except the two deterministic fast
    // paths, which run before the classifier and win outright:
    //   - negation-override (`rule = "negation_override"`): a read-only safety
    //     invariant — a permissive model must never override "don't touch".
    //   - smalltalk (`rule = "smalltalk"`): zero-cost chat — a ≤48-char
    //     greeting routes to a read-only Answer without spending a model call.
    // Every other route — keyword hits, question-detection results, and
    // AskUser-remaining ambiguity alike — is classified via one bounded,
    // spend-reserved provider call. A classification at or above the
    // configured threshold re-routes the deterministic result to the suggested
    // outcome; below-threshold, disabled, malformed, cancelled, spend-capped,
    // or failed calls fail soft and the deterministic result above stands
    // unchanged (ADR-56 §3/§4). The classifier never grants (§8): a re-routed
    // Execute still goes through the exact confirmation machinery below
    // (`bound_plan_for_approval` / `apply_intent_gate`). The audit chain is
    // untouched — `router_route` (captured above) and the classifier's shared
    // correlation id feed the same two-row chain as before (§5).
    let mut classifier_correlation_id: Option<Ulid> = None;
    let classifier_enabled =
        services.config.intent.as_ref().is_some_and(|intent| intent.classifier_enabled);
    if classifier_enabled && classifier_applies_to(&routing.route) {
        let ctx = crate::intent_classifier::ClassifierContext {
            config: &services.config,
            provider: &provider,
            run_model: &model,
            executor: &executor,
            spend_tracker: &spend_tracker,
            session_id,
            utterance: &req.input,
            cancel: req.cancel_token.clone(),
        };
        if let Some(call) = crate::intent_classifier::classify_ambiguity(ctx).await {
            classifier_correlation_id = Some(call.correlation_id);
            if crate::intent_classifier::apply_classifier_decision(&mut routing, &call) {
                tracing::info!(
                    %session_id,
                    correlation_id = %call.correlation_id,
                    outcome = ?routing.outcome,
                    confidence = routing.confidence,
                    "intent classifier re-routed the deterministic result"
                );
            }
        }
    }

    // ADR-55 Phase 1d: a confident Execute request checks the process-scoped
    // plan binding registry for the exact same objective. On a hit the generic
    // intent confirmation is replaced by a real, audited Apply/Replan dialog
    // (ADR-55 §3); on a miss the generic gate path below is byte-identical to
    // pre-Phase-1d. The mode picker no longer exists, so every Execute request
    // is a candidate.
    //
    // ADR-55 Phase 2b (live-fix): the same dialog must also arm when the user
    // approves a just-rendered plan in natural language — "i approve the
    // plan" hashes differently than the original objective and the router's
    // `plan` keyword re-classifies it as a brand-new Plan run, so without this
    // the approval silently re-plans instead of executing. See
    // [`bound_plan_for_approval`] / [`is_plan_approval_phrase`].
    let mut bound = bound_plan_for_approval(&routing, session_id, &plan_objective_hash, &req.input);

    // ADR-55 Phase 2b live-fix (restart-safe dialog): the in-memory registry
    // is process-scoped, so after an app restart between the planning run and
    // the user's approval the phrase arming above finds no binding. Rehydrate
    // the session's newest DURABLE binding (concerto-sessions `plan_bindings`,
    // same (session_id, objective_hash) key semantics, newest-wins) and
    // re-seed the in-process registry with it, then arm the dialog exactly as
    // an in-process hit would. Fail-soft: a storage error or a missing row
    // falls through to the generic intent gate unchanged.
    if bound.is_none() && is_plan_approval_phrase(&req.input) {
        if let Some(store) = &session_store {
            bound = rehydrate_durable_binding(store.as_ref(), session_id, req.cancel_token.clone())
                .await;
        }
    }

    // ADR-55 §11 (binding-driven arming, round-4 live-fix): a confident
    // Execute — "execute", "apply the fix", ... — that armed no binding via
    // the phrase or exact-objective-hash paths above must not silently
    // re-plan. Round-4 live evidence: typing "execute" after a `plan:` run
    // routed as confident Execute, no binding armed, the coordinator
    // re-planned from "execute", the planner came back empty, and no files
    // were written — while the session's newest durable plan sat unused.
    // Arm the same Apply/Replan dialog from that stored plan (its text is
    // what the dialog shows; the existing `Some(binding)` arm below captures
    // it before consumption and builds the run task from it). Fail-soft like
    // [`rehydrate_durable_binding`]: a storage error or a missing row falls
    // through to the generic intent gate unchanged.
    if bound.is_none() && is_confident_execute(&routing) {
        if let Some(store) = &session_store {
            match store.load_newest_plan_binding(session_id, req.cancel_token.clone()).await {
                Ok(record) => {
                    bound = arm_binding_for_confident_execute(&routing, record);
                    // Mirror the durable row into the process-scoped registry
                    // (same re-seed as [`rehydrate_durable_binding`]) so the
                    // dialog's Apply branch consumes it in both stores.
                    if let Some(binding) = &bound {
                        plan_registry().insert(session_id, binding.clone());
                        tracing::info!(
                            %session_id,
                            plan_id = %binding.plan_id(),
                            "armed Apply/Replan dialog from the session-newest durable plan binding"
                        );
                    }
                }
                Err(error) => {
                    tracing::warn!(
                        %error,
                        "durable plan binding lookup failed for a confident Execute; \
                         falling through to the generic intent gate"
                    );
                }
            }
        }
    }

    // Decide how the run may proceed: a stored plan binding for this
    // objective triggers an Apply/Replan dialog (0f/1c), otherwise the
    // intent gate decides. The dialog's Apply decision additionally feeds
    // the ADR-55 Phase 2b (M2) checkpoint suppression below.
    let mut plan_decision: Option<PlanDecision> = None;
    // ADR-55 Phase 2b (M3, live-fix): an Apply decision consumes the stored
    // binding below, so capture it BEFORE that consumption — the Execute run's
    // task must be built from the approved plan text, which is only available
    // while the binding still exists.
    let mut applied_plan: Option<PlanBinding> = None;
    let (effective, confirmation) = match bound {
        Some(binding) => {
            let objective_hash = binding.objective_hash();
            let current_revision = current_source_revision(&req.project_dir).await;
            let binding_revision = binding.source_revision().unwrap_or("unknown");
            // The wording names the plan id and avoids claiming the plan was
            // made "for this objective": phrase/hash arming loads the plan
            // bound to the current objective, but ADR-55 §11 arming loads the
            // session-newest durable plan, which may have been planned for an
            // earlier objective — the user must be able to tell what they are
            // approving.
            let question = format!(
                "A stored plan exists for this session (plan {plan_id}, planned at source \
                 revision {binding_revision}; current checkout {current_revision}). Apply it \
                 now (mutation-capable), or replan first (read-only)?",
                plan_id = binding.plan_id(),
                current_revision = current_revision.as_deref().unwrap_or("unknown"),
            );
            let decision = services
                .approval_sink
                .request_plan_approval(
                    session_id,
                    binding.plan_id(),
                    question,
                    binding.plan_text(),
                    binding.created_at(),
                    req.cancel_token.clone(),
                )
                .await;
            // The dialog is an audited approval-sink call: the decision is
            // snapshotted under the synthetic `intent:plan` identity with
            // plan_id + source revision in the user response, so the audit
            // trail ties the decision back to the binding that produced it.
            executor
                .record_plan_decision(
                    session_id,
                    Ulid::new(),
                    binding.plan_id(),
                    objective_hash,
                    current_revision.as_deref(),
                    decision.map_or("dismissed", PlanDecision::name),
                    req.cancel_token.clone(),
                )
                .await;
            // The decision rides along for ADR-55 Phase 2b (M2) checkpoint
            // suppression below.
            plan_decision = decision;
            // An Apply decision CONSUMES the stored plan: drop the session's
            // binding in the in-memory registry and in durable storage so a
            // later bare approval ("yes", "i approve") cannot re-arm the
            // dialog for an already-executed plan. A missing durable row is
            // a no-op (fail-soft).
            if matches!(plan_decision, Some(PlanDecision::Apply)) {
                // ADR-55 Phase 2b (M3, live-fix): capture the binding BEFORE
                // consuming it so the Execute run below can describe the
                // approved plan instead of the "i approve" phrase.
                applied_plan = Some(binding.clone());
                plan_registry().remove(session_id, objective_hash);
                if let Some(store) = &session_store {
                    if let Err(error) = store
                        .delete_plan_binding(session_id, objective_hash, req.cancel_token.clone())
                        .await
                    {
                        tracing::warn!(
                            %error,
                            "failed to clear durable plan binding after apply"
                        );
                    }
                }
            }
            apply_plan_decision(decision, &store, &routing)
        }
        None => {
            apply_intent_gate(
                &routing,
                services.approval_sink.as_ref(),
                &store,
                &auth,
                req.cancel_token.clone(),
            )
            .await
        }
    };

    // ADR-55 Phase 2b (M2): an Apply decision authorizes the STORED plan for
    // this objective — the run must execute that plan, never silently resume
    // a stale partial-graph checkpoint from an earlier Execute of the same
    // objective (its input hash would otherwise match the implicit-resume
    // check below).
    let apply_plan = matches!(plan_decision, Some(PlanDecision::Apply));

    // The plan-decision helper grants but never mutates the read-only flag
    // (it owns only the store), so normalize it here from the confirmation.
    // Idempotent with `apply_intent_gate`, which already sets the same value
    // on its own paths.
    auth.set_read_only(confirmation != "granted");
    let effective_outcome = effective;
    // The task-shape decision below must follow the gate's read-only state,
    // not just the effective outcome: a dismissed/absent confirmation keeps
    // effective Execute while the run is read-only (B-2). Sampled here,
    // immediately after the gate set it.
    let gate_read_only = auth.is_read_only();

    executor
        .record_routing_decision(
            session_id,
            // ADR-55 Phase 2c §5/C4: share the classifier's correlation id
            // when a call happened; otherwise mint a fresh per-event id —
            // `Ulid::default()` is the all-zero nil id and must never reach
            // the audit (see `router_row_correlation_id`).
            router_row_correlation_id(classifier_correlation_id),
            &req.input,
            router_route,
            outcome_name(effective),
            routing.confidence,
            confirmation,
            req.cancel_token.clone(),
        )
        .await;

    // The spend carry-forward moved up before the intent classifier (Phase 2c
    // §6 ordering); `session_manager` still gates the checkpoint/resume block.
    if let Some(ref session_manager) = session_manager {
        if !req.force_single_agent {
            let resume_requested = is_resume_request(&req.input);

            // ADR-55 Phase 2b (M2): an Apply executes the APPROVED plan for
            // this objective. A checkpoint left over from a previous partial
            // Execute of the same objective would match the input hash and
            // silently resume the old partial graph below — the approved
            // plan governs instead, so suppress resume entirely and clear
            // the stale checkpoint.
            if apply_plan {
                req.resume_checkpoint_json = None;
                suppress_stale_checkpoint_for_apply(session_manager, session_id).await;
            } else if req.resume_checkpoint_json.is_none() {
                // Load the stored checkpoint unconditionally (not just for
                // explicit "continue" requests).  This enables crash recovery:
                // after a restart the user sends the same objective, the hash
                // matches below, and the run resumes without needing a keyword.
                req.resume_checkpoint_json = session_manager
                    .store()
                    .load_orchestration_checkpoint(session_id)
                    .await
                    .map_err(|error| {
                        OrchestratorError::AgentLoopError(format!(
                            "failed to load orchestration checkpoint: {error}"
                        ))
                    })?
                    .map(|record| record.state_json);
            }

            if let Some(checkpoint_json) = req.resume_checkpoint_json.as_ref() {
                // Use the version-aware loader so legacy v2 checkpoints are
                // migrated in-memory and unknown future schema versions are
                // rejected cleanly (C-05).
                match crate::checkpoint::GraphCheckpoint::from_json(checkpoint_json) {
                    Ok(checkpoint) if resume_requested => {
                        // Explicit "continue" / "resume" — validate scope
                        // before trusting the checkpoint (Finding 2 / #65).
                        let project_id_str =
                            concerto_core::helpers::project_id_hash(&req.project_dir);
                        let source_revision = current_source_revision(&req.project_dir).await;
                        if let Err(reason) = checkpoint.validate_scope(
                            session_id,
                            &project_id_str,
                            source_revision.as_deref(),
                        ) {
                            tracing::warn!(
                                %reason,
                                "discarding checkpoint that failed scope validation on resume"
                            );
                            req.resume_checkpoint_json = None;
                            if let Err(error) = session_manager
                                .store()
                                .clear_orchestration_checkpoint(session_id)
                                .await
                            {
                                tracing::warn!(
                                    %error,
                                    "failed to clear checkpoint after scope validation failure",
                                );
                            }
                        }
                    }
                    Ok(checkpoint) => {
                        // Non-resume request.  Compare the input hash with
                        // the checkpoint's objective — if they match the
                        // user is re-sending their original task after a
                        // process restart, so we treat it as an implicit
                        // resume.  If they differ the user has a genuinely
                        // new task, so we clear the stale checkpoint.
                        let input_hash = blake3::hash(req.input.as_bytes()).to_hex().to_string();
                        if checkpoint.objective_hash == input_hash {
                            tracing::info!("input matches checkpoint objective — implicit resume after restart");
                        } else {
                            req.resume_checkpoint_json = None;
                            if let Err(error) = session_manager
                                .store()
                                .clear_orchestration_checkpoint(session_id)
                                .await
                            {
                                tracing::warn!(%error, "failed to clear checkpoint superseded by a new objective");
                            }
                        }
                    }
                    Err(error) => {
                        tracing::warn!(%error, "discarding malformed orchestration checkpoint");
                        req.resume_checkpoint_json = None;
                        if let Err(clear_error) =
                            session_manager.store().clear_orchestration_checkpoint(session_id).await
                        {
                            tracing::warn!(%clear_error, "failed to clear malformed orchestration checkpoint");
                        }
                    }
                }
            }
        }
    }

    // Task shape follows the gate state (ADR-55 Phase 1e): a confirmed,
    // mutation-capable Execute -> action-required; a dismissed Execute that the
    // gate forced read-only -> answer-only, preventing the agent_loop
    // ActionRequired retry loop that would steer the model toward a mutation
    // bypass, and every other gate outcome -> answer-only with mutation limits
    // enforced by the gate itself. The gate is always on — there is no legacy
    // mode picker.
    let action_required = task_action_required(effective_outcome, gate_read_only);
    // ADR-55 Phase 2b (M3, live-fix): an Apply run executes the APPROVED plan,
    // not the approval phrase. `req.input` is still recorded in the transcript
    // and audit; only the task the agents execute is replaced.
    let task =
        build_run_task(session_id, action_required, apply_plan, applied_plan.as_ref(), &req.input);

    // Run-stage tracking (ADR-55 Phase 2a). Created here, after routing, so
    // the stage chip always starts from Understand; threaded down into the
    // single- and multi-agent paths below, which report the later stages.
    let stage_tracker =
        Arc::new(Mutex::new(StageTracker::new(services.bus.clone(), session_id, task.id)));
    stage_tracker.lock().unwrap_or_else(|error| error.into_inner()).set(RunStage::Understand);

    // 7. Multi-agent dispatch. The gate is always on, so the effective
    // outcome and plan objective hash ride along: the text-only branch below
    // uses the outcome for its system prompt and stores the plan binding for
    // Plan runs (ADR-55 Phase 1e).
    if !req.force_single_agent {
        return run_multi_agent(
            &req,
            &services,
            session_id,
            session_store,
            executor,
            memory,
            spend_tracker,
            &task,
            event_recorder,
            transcript_recorder,
            action_required,
            effective_outcome,
            plan_objective_hash,
            &stage_tracker,
        )
        .await;
    }

    // 8. Single-agent execution. Capture the project dir and cancel token
    // before `req` is moved so the post-run binding insert can resolve the
    // current source revision and persist the durable binding.
    let plan_project_dir = req.project_dir.clone();
    let plan_cancel_token = req.cancel_token.clone();
    // ADR-60 D5: give the in-process single-agent loop the same always-on
    // write-gate protection the supervised agent-process path enforces. The
    // gate reuses the run's own policy/executor pair and the session-DB pool
    // (sessions.db), mirroring `SupervisorServices.gate`. It is constructed
    // only for single-agent runs; the coordinator path keeps the plain
    // executor (specialists are already policy-gated per-agent).
    let single_agent_executor: SharedExecutionBackend = match gate_log_pool {
        Some(log_pool) => {
            let gate = Arc::new(WriteGate::new(
                intent_policy,
                executor.clone(),
                log_pool,
                Arc::new(FilePreImageReader::new(&req.project_dir)),
                req.project_dir.clone(),
                1,
            ));
            Arc::new(InProcessGateBackend::new(gate, executor.clone(), "single-agent"))
        }
        None => {
            // No sessions DB available (audit log already fell back to
            // NoopAuditLog). The write gate cannot append WAL rows, so the
            // loop runs on the plain executor — a documented, load-bearing
            // degradation, never a silent one.
            tracing::warn!(
                "no session DB pool for the write gate — single-agent run proceeds with the plain tool executor"
            );
            executor.clone()
        }
    };
    let output = execute_agent_loop(
        req,
        &services,
        provider,
        model,
        single_agent_executor,
        memory,
        session_store.clone(),
        session_id,
        task,
        event_recorder,
        transcript_recorder,
        effective_outcome,
        action_required,
        &stage_tracker,
    )
    .await?;

    // 8b. ADR-55 Phase 1d post-run: after a Plan run completes, store the
    // produced plan as the binding for this objective (newest-wins, keyed by
    // session + objective hash) so a later Execute request of the same
    // objective can offer Apply. Error-path finals ("User input required"/
    // "Blocked") are partial recoveries, never plans, so only a `Completed`
    // run with a non-empty final message is recorded. Since the gate is now
    // always on (ADR-55 Phase 1e), this also fires for multi-agent Plan runs —
    // an intended consequence documented in the ADR addendum.
    if effective_outcome == RequestedOutcome::Plan
        && output.completion_status == AgentCompletionStatus::Completed
        && !output.final_message.trim().is_empty()
    {
        let source_revision = current_source_revision(&plan_project_dir).await;
        let binding = PlanBinding::new(
            Ulid::new().to_string(),
            plan_objective_hash.clone(),
            source_revision,
            output.final_message.clone(),
        );
        plan_registry().insert(session_id, binding.clone());
        // Live-fix: mirror the binding to durable storage so "i approve the
        // plan" offered after an app restart still arms the dialog.
        // Fail-soft: a persistence failure never fails the run — the
        // in-memory registry still arms it in-process.
        if let Some(store) = &session_store {
            if let Err(error) = store
                .save_plan_binding(
                    &PlanBindingRecord {
                        session_id,
                        objective_hash: binding.objective_hash().to_owned(),
                        plan_id: binding.plan_id().to_owned(),
                        plan_text: binding.plan_text().to_owned(),
                        source_revision: binding.source_revision().map(ToOwned::to_owned),
                        artifact_hash: binding.artifact_hash().map(ToOwned::to_owned),
                        created_at: binding.created_at(),
                    },
                    plan_cancel_token,
                )
                .await
            {
                tracing::warn!(%error, %session_id, "failed to persist durable plan binding");
            }
        }
        tracing::debug!(%session_id, %plan_objective_hash, "stored plan binding for objective");
    }

    Ok(output)
}

/// Run the multi-agent (coordinator) path: resolve role-specific providers,
/// optionally run text-only mode, or launch a full multi-agent `CoordinatorAgent`
/// with collaboration rules.
///
/// `action_required` / `effective_outcome` / `plan_objective_hash` come from
/// the always-on intent gate (ADR-55 Phase 1e): they shape the topology, the
/// text-only system prompt, and the post-run plan-binding insert.
#[allow(clippy::too_many_arguments)]
async fn run_multi_agent(
    req: &AgentRunRequest,
    services: &SharedServices,
    session_id: Ulid,
    session_store: Option<Arc<dyn SessionStore>>,
    executor: Arc<ToolExecutor>,
    memory: Arc<dyn MemoryStore>,
    spend_tracker: Arc<SpendTracker>,
    task: &AgentTask,
    event_recorder: EventRecorderGuard,
    transcript_recorder: TranscriptRecorderGuard,
    action_required: bool,
    effective_outcome: RequestedOutcome,
    plan_objective_hash: String,
    stage_tracker: &Arc<Mutex<StageTracker>>,
) -> Result<AgentOutput, OrchestratorError> {
    let project_dir = req.project_dir.clone();
    if let Some(store) = &session_store {
        let user_message = Message {
            role: concerto_core::types::Role::User,
            content: req.input.clone(),
            tool_calls: None,
            tool_results: None,
            reasoning_content: None,
            tokens_in: None,
            tokens_out: None,
        };
        if let Err(error) =
            store.append_messages(session_id, &[user_message], req.cancel_token.clone()).await
        {
            if is_expected_cancellation(&error, &req.cancel_token) {
                tracing::debug!(%error, "run cancelled; multi-agent user message not persisted");
            } else {
                tracing::warn!(%error, "failed to persist multi-agent user message");
            }
        }
    }
    let settings = services.config.model_settings.as_ref().ok_or_else(|| {
        OrchestratorError::AgentLoopError(
            "multi-agent mode requires at least one configured provider".into(),
        )
    })?;
    if settings.providers.is_empty() {
        event_recorder.stop().await;
        transcript_recorder.stop().await;
        return Err(OrchestratorError::AgentLoopError(
            "multi-agent mode requires at least one configured provider".into(),
        ));
    }

    let requested_provider_id = non_empty(req.selected_provider_id.as_deref());

    // Resolve the default provider using model-first strategy:
    //   1. Explicit provider ID → find that config.
    //   2. Explicit model name → find the provider that offers it.
    //   3. `global_default_model` → find the matching provider.
    //   4. First configured provider.
    let default_provider_config = if let Some(id) = requested_provider_id {
        settings
            .providers
            .iter()
            .find(|config| ProviderFactory::config_id(config) == id)
            .ok_or_else(|| {
                OrchestratorError::AgentLoopError(format!(
                    "selected provider configuration '{id}' no longer exists"
                ))
            })?
    } else if let Some(model) =
        non_empty(req.selected_model.as_deref()).filter(|m| !m.trim().is_empty())
    {
        ProviderFactory::config_for_model(settings, model, None).ok_or_else(|| {
            OrchestratorError::AgentLoopError(format!(
                "no configured provider offers model '{model}'"
            ))
        })?
    } else if let Some(model) =
        non_empty(settings.global_default_model.as_deref()).filter(|m| !m.trim().is_empty())
    {
        ProviderFactory::config_for_model(settings, model, None).ok_or_else(|| {
            OrchestratorError::AgentLoopError(format!(
                "no configured provider offers global default model '{model}'"
            ))
        })?
    } else {
        settings.providers.first().ok_or_else(|| {
            OrchestratorError::AgentLoopError("no providers configured in model_settings".into())
        })?
    };
    let default_model = non_empty(req.selected_model.as_deref())
        .or_else(|| non_empty(settings.global_default_model.as_deref()))
        .unwrap_or(default_provider_config.model.trim());
    if default_model.is_empty() {
        event_recorder.stop().await;
        transcript_recorder.stop().await;
        return Err(OrchestratorError::AgentLoopError(format!(
            "provider configuration '{}' has no selected model",
            ProviderFactory::config_id(default_provider_config)
        )));
    }

    let creds = CredentialStore::new();
    let mut default_config = default_provider_config.clone();
    default_config.model = default_model.to_string();
    let default_provider =
        ProviderFactory::build(&default_config, &creds).map_err(OrchestratorError::Provider)?;
    let mut role_providers = std::collections::HashMap::new();
    let legacy_pins = legacy_pins_from_config(&services.config.multi_agent);
    let base_profiles = ProviderFactory::build_profiles(settings);
    // ADR-45 tier 1b: the fallback ladder re-dispatches a failed role rebuilt
    // on the run's default provider — the pipe that serves the global default
    // model. Clone before the registry consumes `default_provider`; the
    // profile mirrors how role profiles are built below (base profile by
    // config id, model overridden to the default).
    let default_model_provider = default_provider.clone();
    let default_model_profile = base_profiles
        .iter()
        .find(|profile| {
            profile.provider_config_id == ProviderFactory::config_id(default_provider_config)
        })
        .cloned()
        .map(|mut profile| {
            profile.model = default_model.to_string();
            concerto_providers::model::ModelProfile {
                context_window: profile.context_window,
                supports_tool_calling: profile.supports_tool_calling,
                base_url: profile.base_url.clone(),
                description: profile.description.clone(),
                profile,
            }
        });
    let mut pins = std::collections::HashMap::new();
    let mut provider_pins = std::collections::HashMap::new();
    let mut profiles = Vec::new();

    // ADR-35 phase 4: the roles needing provider/model resolution mirror the
    // runtime topology (coordinator + built-ins not disabled + enabled custom
    // agents) instead of a hardcoded role list. The shape follows the intent
    // gate's effective outcome (ADR-55 Phase 1e): Execute runs use the full
    // topology, everything else resolves only the coordinator.
    let roles_to_resolve: Vec<AgentId> = if action_required {
        topology_roles(&services.config.multi_agent)
    } else {
        COORDINATOR_ONLY_ROLES.iter().map(|name| AgentId::new(*name)).collect()
    };
    // ADR-58 P2+P3 (Batch 1): the resolved blueprint attached at load is the
    // lifecycle-stage authority (design doc §1.2/§2). Derive the per-agent
    // stage and the tool-calling role set from it at the same construction
    // seam the registry consumes the agent configs. On the default `standard`
    // blueprint the derived stages equal the built-in seed stages and the
    // tool-calling set equals the legacy classification (Batch 1 pinned it;
    // R5/F1 deleted the standalone `tool_calling_roles_for` — the facade
    // method is the single implementation, preserving the full legacy
    // disjunction, design doc §4 Q5). Roles the blueprint does not staff
    // keep their config stage (Freeform/run_once semantics, ADR-58 D2).
    let facade = services.config.resolved_blueprint.as_deref().map(BlueprintFacade::new);

    // The tool-calling role set handed to the routing engine mirrors the same
    // topology. Derived before `roles_to_resolve` is consumed by the
    // resolution loop below.
    let mut agent_configs: HashMap<AgentId, concerto_config::CustomAgentConfig> =
        build_agent_config_map(&services.config.multi_agent);
    if let Some(facade) = facade.as_ref() {
        for (id, cfg) in agent_configs.iter_mut() {
            // Blueprint staffing fills in the stage of roles whose config
            // leaves it unset (the same gap the registry's seed merge covers
            // today): the resolved blueprint's `def.agents` is the
            // post-ADR-58 authority for which stage a role participates in.
            // Explicit config stages — including deliberate Freeform/run_once
            // retags of staffed built-ins — keep winning, so the default
            // path is byte-identical (every seed's declared stage already
            // equals the standard blueprint's staffing).
            if cfg.stage.is_none() {
                if let Some(stage) = facade.stage_for_agent(id) {
                    cfg.stage = Some(AgentStage::new(&stage.def.tag));
                }
            }
        }
    }
    let tool_calling_roles = match &facade {
        Some(facade) => facade.tool_calling_roles(&roles_to_resolve, &agent_configs),
        // ADR-58 P2+P3 (R5/F1): the legacy `tool_calling_roles_for` route is
        // deleted. `resolved_blueprint` is attached on every load path
        // (config/lib.rs `validate_config`), so this branch is unreachable
        // for runtime-built configs; an artificially facade-less config would
        // route with no tool-calling roles rather than regress to the deleted
        // classification.
        None => Default::default(),
    };
    for role in roles_to_resolve {
        // When an agent assignment references a provider that no longer
        // exists, silently fall back to the global default instead of
        // erroring — the user may have removed a provider without updating
        // every assignment in the Orchestration Studio.
        let assignment = settings
            .agent_assignments
            .iter()
            .find(|assignment| configured_agent_id(&assignment.agent_role).as_ref() == Some(&role))
            .filter(|assignment| {
                settings.providers.iter().any(|config| {
                    ProviderFactory::config_id(config) == assignment.provider_config_id
                })
            });
        let provider_config = if let Some(assignment) = assignment {
            settings
                .providers
                .iter()
                .find(|config| ProviderFactory::config_id(config) == assignment.provider_config_id)
                .unwrap_or(default_provider_config)
        } else {
            default_provider_config
        };
        let provider_id = ProviderFactory::config_id(provider_config);
        let model = assignment
            .and_then(|assignment| non_empty(assignment.model_override.as_deref()))
            .or_else(|| legacy_pins.get(&role).and_then(|model| non_empty(Some(model))))
            .unwrap_or_else(|| {
                if assignment.is_some() {
                    provider_config.model.trim()
                } else {
                    default_model
                }
            });
        if model.is_empty() {
            event_recorder.stop().await;
            transcript_recorder.stop().await;
            return Err(OrchestratorError::AgentLoopError(format!(
                "provider configuration '{provider_id}' assigned to {role} has no model"
            )));
        }

        let mut resolved_config = provider_config.clone();
        resolved_config.model = model.to_string();
        role_providers.insert(
            role.clone(),
            ProviderFactory::build(&resolved_config, &creds)
                .map_err(OrchestratorError::Provider)?,
        );
        pins.insert(role.clone(), model.to_string());
        provider_pins.insert(role.clone(), provider_id.clone());

        let mut profile = base_profiles
            .iter()
            .find(|profile| profile.provider_config_id == provider_id)
            .cloned()
            .ok_or_else(|| {
                OrchestratorError::AgentLoopError(format!(
                    "provider configuration '{provider_id}' has no routing profile"
                ))
            })?;
        profile.model = model.to_string();
        if !profiles.iter().any(|existing: &concerto_core::types::RoutingProfile| {
            existing.provider_config_id == profile.provider_config_id
                && existing.model == profile.model
        }) {
            profiles.push(profile);
        }
    }

    let coordinator_provider = role_providers
        .get(&AgentId::new("coordinator"))
        .cloned()
        .unwrap_or_else(|| default_provider.clone());
    let coordinator_model = pins
        .get(&AgentId::new("coordinator"))
        .cloned()
        .unwrap_or_else(|| default_model.to_string());
    // ADR-42 §4 tier 2: the routing profile of the coordinator's model on its
    // serving pipe. Built the same way the ADR-45 tier-1b profile is built
    // above (base profile by config id, model overridden), so self-execution
    // dispatches through the runner like any other role instead of a raw
    // single-shot request. `None` when the coordinator's pipe has no routing
    // profile — tier 2 then skips with a note (the profile is advisory for
    // the coordinator's own dispatch). The coordinator's pipe is the provider
    // config id it resolved to during the role-resolution loop above; roles
    // without a resolution (coordinator fallback) serve on the default pipe.
    let coordinator_pipe_id = provider_pins
        .get(&AgentId::new("coordinator"))
        .cloned()
        .unwrap_or_else(|| ProviderFactory::config_id(default_provider_config));
    let planning_profile = base_profiles
        .iter()
        .find(|profile| profile.provider_config_id == coordinator_pipe_id)
        .cloned()
        .map(|mut profile| {
            profile.model = coordinator_model.clone();
            concerto_providers::model::ModelProfile {
                context_window: profile.context_window,
                supports_tool_calling: profile.supports_tool_calling,
                base_url: profile.base_url.clone(),
                description: profile.description.clone(),
                profile,
            }
        });

    // Chat, Verify, Review, Diagnose and Answer are deliberately text-only
    // outcomes. Execute (action required) uses the full dependency graph;
    // text-only outcomes use the configured Coordinator provider directly
    // with persistent conversation history. Plan is NOT text-only (ADR-55
    // Phase 2b): it falls through to the full coordinator below, capped at
    // planning-only depth, so the produced plan is real and rendered. This
    // prevents a follow-up question from launching Coder/Validator or
    // mutating the workspace merely because the multi-agent toggle is on
    // (ADR-55 Phase 1e: the shape follows the intent gate's effective
    // outcome, not a mode picker).
    if !action_required && effective_outcome != RequestedOutcome::Plan {
        let retry_policy = RetryPolicy::new(services.config.retry.clone());
        let output = run_text_only(
            task,
            concerto_core::types::system_prompt_for(effective_outcome),
            req.conversation_history.clone(),
            coordinator_provider,
            coordinator_model,
            &retry_policy,
            &services.bus,
            &services.skills.section(),
            req.cancel_token.clone(),
        )
        .await?;
        // The single text-only provider call succeeded — the run is complete.
        stage_tracker.lock().unwrap_or_else(|error| error.into_inner()).set(RunStage::Complete);
        persist_provider_metrics(
            session_store.as_ref(),
            session_id,
            &output.provider_metrics,
            req.cancel_token.clone(),
        )
        .await;
        // One spend record for the single text-only provider call (best-effort).
        persist_spend_records(
            session_store.as_ref(),
            session_id,
            Some(task.id.0),
            &output.provider_metrics,
            req.cancel_token.clone(),
        )
        .await;
        if let Some(store) = &session_store {
            let assistant_message = Message {
                role: Role::Assistant,
                content: output.final_message.clone(),
                tool_calls: None,
                tool_results: None,
                reasoning_content: None,
                tokens_in: None,
                tokens_out: None,
            };
            if let Err(error) = store
                .append_messages(session_id, &[assistant_message], req.cancel_token.clone())
                .await
            {
                tracing::warn!(%error, "failed to persist text-only assistant message");
            }
        }
        // Final transcript entries (ADR-36 §4): assistant text + completion
        // marker. Text-only runs carry a single-agent completion marker.
        transcript_recorder
            .append_entries(&[
                TranscriptEntry::Assistant { content: output.final_message.clone() },
                TranscriptEntry::Completion {
                    multi_agent: false,
                    completed: output.completion_status == AgentCompletionStatus::Completed,
                    files: output.files_modified.iter().map(ToString::to_string).collect(),
                    project_root: output.project_root.as_ref().map(ToString::to_string),
                },
            ])
            .await;
        event_recorder.stop().await;
        transcript_recorder.stop().await;
        maintain_context_after_run(
            session_store.as_ref(),
            session_id,
            services.config.context.as_ref(),
            req.cancel_token.clone(),
            Some(&services.bus),
        )
        .await;
        // ADR-55 Phase 2b: Plan no longer reaches this text-only path — it is
        // produced by the full coordinator at planning-only depth below. Text-
        // only runs here are Chat/Verify/Review/Diagnose/Answer and store no
        // binding.
        return Ok(output);
    }
    // Share one RetryPolicy across the registry for per-agent
    // with_provider_retry (used inside each specialist agent).
    let retry_policy = RetryPolicy::new(services.config.retry.clone());
    // ADR-43 Task 4: the session's budgeted skills section, captured once per
    // run and shared by the planner and every registered specialist. A UI
    // toggle (Task 7) refreshes `services.skills`; the next run picks it up.
    let skills_section = services.skills.section();

    // `agent_configs` is built above (next to the tool-calling role
    // derivation) and reused here for the registry and the coordinator. The
    // resolved blueprint facade is handed to the registry so seeds are
    // registered from the resolved per-agent capabilities (ADR-58 P2+P3, R9).
    // `merge_seeds` mirrors `AppConfig::owns_agent_roster()`: once the config
    // declares a roster (custom agents or [orchestration]), the config IS
    // the roster and the seed set is NOT merged back in — deleted seeds stay
    // deleted at runtime (maintainer revision of ADR-58/59).
    let merge_seeds = !services.config.owns_agent_roster();
    let registry = Arc::new(AgentRegistry::build_with_roles_for_project_with_facade(
        role_providers,
        default_provider,
        executor.clone(),
        services.bus.clone(),
        retry_policy,
        &req.project_dir,
        &agent_configs,
        &skills_section,
        facade.as_ref(),
        merge_seeds,
    ));
    // The feed task below resolves implement-stage roles from the registry,
    // so keep a clone before `registry` moves into the coordinator.
    let stage_feed_registry = registry.clone();
    // The stage feed (R6) and the collaboration-rule resolution (F9) below
    // query the resolved blueprint, but the original facade moves into the
    // coordinator above — keep a clone for both sites.
    let run_facade = facade.clone();
    // Policy checks, routing, coordination, and actual-cost recording all
    // share one tracker so estimates are not charged as spend.
    let runner = AgentRunner::new(registry.clone(), services.bus.clone(), spend_tracker.clone());
    let routing = Arc::new(
        RoutingEngine::new(
            profiles.clone(),
            spend_tracker.clone(),
            concerto_config::ModelPinConfig {
                pins,
                // Tier-1 fallback target for the coordinator ladder:
                // `multi_agent.default_model` wins when set, otherwise the
                // run's `model_settings.global_default_model` fills in so a
                // user who only configured a global default still gets tier-1
                // fallback (see `ModelSettings::resolved_default_model`).
                default_model: settings
                    .resolved_default_model(services.config.multi_agent.as_ref()),
                // The model name switch never changes the serving provider
                // (ADR-42/45); only a `multi_agent` provider pin pairs with it.
                // `ModelSettings` has no global provider id of its own, so this
                // stays `None` unless multi_agent pins it.
                default_provider_config_id: services
                    .config
                    .multi_agent
                    .as_ref()
                    .and_then(|config| config.default_provider_config_id.clone()),
            },
            services.bus.clone(),
        )
        .with_provider_pins(provider_pins)
        .with_tool_calling_roles(tool_calling_roles),
    );
    let selector =
        Arc::new(ModelSelector::new(Arc::new(ModelRegistry::from_profiles(profiles)), routing));
    let mut coordinator = CoordinatorAgent::new(
        registry.clone(),
        runner,
        selector,
        spend_tracker,
        services.bus.clone(),
        coordinator_provider,
        memory.clone(),
    )
    .with_agent_configs(agent_configs)
    .with_skills_section(skills_section)
    // ADR-58 P2+P3 (Batch 1): the resolved blueprint facade backs the
    // sequencing guards in `Coordinator::stage_of` /
    // `Coordinator::first_agent_for_stage` (`debug_assert!` comparing the
    // registry answer against blueprint staffing). `None` when no resolved
    // blueprint is attached (e.g. coordinators built in tests directly);
    // guards then stay silent. Dispatch sites still consult the registry —
    // replacing them with facade lookups is the Batch 2+ table (R1–R11).
    .with_blueprint_facade(facade)
    .with_default_model_provider(Some(default_model_provider), default_model_profile)
    .with_planning_profile(planning_profile)
    // ADR-35 §8: the shared executor backs coordinator self-execution when a
    // lifecycle stage has no registered agent. Attached unconditionally; the
    // coordinator only uses it when it actually self-executes.
    .with_executor(executor.clone())
    // ADR-35 §5 Phase 5 C-06 amendment: the coordinator's eval engine backs
    // coordinator self-verification when no validation-stage agent is
    // registered. Built exactly like the single-agent path (detected runner
    // + optional shell profile); attached unconditionally — the coordinator
    // only uses it when a validate-stage agent is absent.
    .with_eval_engine(Arc::new({
        let engine = EvalEngine::new(&req.project_dir);
        let settings = services.config.resolved_shell_settings();
        match settings.selected_profile() {
            Some(profile) => engine.with_shell_profile(profile.clone()),
            None => engine,
        }
    }))
    // Model-first serving pipe: an unassigned role's effective serving pipe
    // is the run's default provider. Without this the fallback ladder could
    // never resolve a serving pipe for unassigned roles, so tier 1 would be
    // skipped (or, worse, dispatch across pipes).
    .with_default_provider_config_id(Some(ProviderFactory::config_id(default_provider_config)));
    // ADR-45 §4: user-configurable ladder knobs.
    if let Some(multi_agent) = &services.config.multi_agent {
        coordinator = coordinator.with_default_model_fallback(multi_agent.default_model_fallback);
        if let Some(max_attempts) = multi_agent.max_subtask_attempts {
            coordinator = coordinator.with_max_subtask_attempts(max_attempts);
        }
        // ADR-52: per-run model-dispatch cap (doom guard).
        coordinator = coordinator.with_max_total_iterations(multi_agent.max_total_iterations);
        // ADR-35 §5/§8: the Orchestration Studio's supplemental prompt,
        // appended to the coordinator self's built-in instructions.
        if let Some(prompt) = &multi_agent.coordinator_prompt {
            coordinator = coordinator.with_supplemental_prompt(prompt.clone());
        }
    }
    // ADR-52: durable plan artifacts. The plans manager lives in
    // concerto-sessions and shares the app data directory with the other
    // on-disk stores (memory, plugins, audit). A failure to open it only
    // disables plan persistence; runs proceed without it.
    if let Ok(plans) = concerto_sessions::plans::PlansManager::open() {
        coordinator = coordinator.with_plans(Some(plans));
    }
    if let Some(multi_agent) = &services.config.multi_agent {
        if !multi_agent.relationships.is_empty() {
            let rules = multi_agent
                .relationships
                .iter()
                .map(|configured| {
                    let from = configured_agent_id(&configured.from).ok_or_else(|| {
                        OrchestratorError::AgentLoopError(format!(
                            "unknown relationship source role: {}",
                            configured.from
                        ))
                    })?;
                    let to = configured_agent_id(&configured.to).ok_or_else(|| {
                        OrchestratorError::AgentLoopError(format!(
                            "unknown relationship target role: {}",
                            configured.to
                        ))
                    })?;
                    let relationship =
                        configured_relationship(run_facade.as_ref(), &configured.relationship)?;
                    Ok(CollaborationRule {
                        from,
                        to,
                        relationship,
                        max_cycles: configured.max_cycles,
                    })
                })
                .collect::<Result<Vec<_>, OrchestratorError>>()?;
            coordinator = coordinator.with_collaboration_rules(rules)?;
        }
    }
    // ADR-55 Phase 2b: a Plan outcome runs the FULL coordinator (memory +
    // design stage, planner, graph validation) capped at planning-only depth:
    // it renders and persists a real plan artifact and touches no tools. The
    // stage feed thus stays at Planning/Complete and reports no Execute/Verify
    // (M1); no checkpoint is written for the run; the rendered plan is bound
    // to the objective below (M3).
    let planning_only = effective_outcome == RequestedOutcome::Plan;
    if planning_only {
        coordinator = coordinator.with_orchestration_depth(OrchestrationDepth::PlanningOnly);
    }
    let multi_task = multi_agent_task_with_history(task.clone(), &req.conversation_history);
    let context = AgentContext::new(SessionContext::new(session_id, project_dir.clone()));
    // Run-stage tracking (ADR-55 Phase 2a): the coordinator publishes
    // `SubTaskCreated` and gate-cycle (`ReviewCycleStarted` /
    // `ValidationCycleStarted`) events on the bus as the graph progresses, so
    // the stage chip can follow the actual lifecycle instead of the wrapper's
    // static view. The feed task resolves each event to a `RunStage` through
    // the blueprint's per-stage feed bindings (ADR-58 P2+P3, R6/F3) and
    // forwards only real transitions into the shared tracker (which dedupes);
    // it is aborted once the coordinator run settles. Cancellation stops the
    // feed early, but the run's own cancellation already forbids the Complete
    // report on the error path below.
    //
    // The feed filters by session: the bus is process-global, so a concurrent
    // run in another session (second CLI, API server) publishes its own
    // subtask/gate events — without the filter they would advance this run's
    // chip.
    let stage_feed_bus = services.bus.clone();
    let stage_feed_tracker = stage_tracker.clone();
    let stage_feed_cancel = req.cancel_token.clone();
    // The feed task resolves each role's feed binding through the resolved
    // blueprint (R6); the facade clone moves in with the other captures.
    let stage_feed_facade = run_facade;
    // ADR-35 §8 trigger 1: the executor is always attached in this wiring, so
    // an empty implement-stage roster means the coordinator self executes the
    // implement subtasks. The stage feed below must then treat a
    // coordinator-role subtask exactly like an implement-stage one. The
    // implement roster keys the primary `Execution` stage's resolved tag, so
    // a renamed implement stage keeps trigger-1 semantics (issue #150).
    let implement_tag = stage_feed_facade
        .as_ref()
        .and_then(|facade| facade.primary_execution_stage())
        .map(|stage| stage.def.tag.clone())
        .unwrap_or_else(|| AgentStage::IMPLEMENT.to_string());
    let coordinator_self_implements =
        stage_feed_registry.ids_for_stage(&AgentStage::new(implement_tag)).is_empty();
    let stage_feed = tokio::spawn(async move {
        let mut receiver = stage_feed_bus.subscribe();
        loop {
            if stage_feed_cancel.is_cancelled() {
                break;
            }
            let event = match receiver.recv().await {
                Ok(event) => event,
                Err(_) => break,
            };
            if event.session_id != session_id {
                continue;
            }
            if let Some(stage) = stage_feed_advance(
                &event.kind,
                &stage_feed_registry,
                stage_feed_facade.as_ref(),
                planning_only,
                coordinator_self_implements,
            ) {
                stage_feed_tracker.lock().unwrap_or_else(|error| error.into_inner()).set(stage);
            }
        }
    });
    let mut output = match coordinator
        .run(multi_task, context, req.cancel_token.clone(), req.resume_checkpoint_json.clone())
        .await
    {
        Ok(output) => output,
        Err(error) => {
            stage_feed.abort();
            if let Some(store) = &session_store {
                let content = if matches!(
                    &error,
                    OrchestratorError::Cancelled
                        | OrchestratorError::Provider(ProviderError::Cancelled)
                ) {
                    "Task cancelled.".to_string()
                } else {
                    format!("Task failed: {error}")
                };
                let failure_message = Message {
                    role: concerto_core::types::Role::Assistant,
                    content,
                    tool_calls: None,
                    tool_results: None,
                    reasoning_content: None,
                    tokens_in: None,
                    tokens_out: None,
                };
                if let Err(store_error) = store
                    .append_messages(session_id, &[failure_message], req.cancel_token.clone())
                    .await
                {
                    if is_expected_cancellation(&store_error, &req.cancel_token) {
                        tracing::debug!(%store_error, "run cancelled; failure message not persisted");
                    } else {
                        tracing::warn!(%store_error, "failed to persist multi-agent failure");
                    }
                }
            }
            transcript_recorder.stop().await;
            event_recorder.stop().await;
            // Persist the coordinator's settled metrics before the error
            // propagates: on a rate-limit exhaustion / cancellation the run
            // consumed real tokens without any output — the audit trail must
            // still record them. Best-effort like the success tail.
            let settled = coordinator.settled_metrics();
            persist_provider_metrics(
                session_store.as_ref(),
                session_id,
                settled,
                req.cancel_token.clone(),
            )
            .await;
            persist_spend_records(
                session_store.as_ref(),
                session_id,
                Some(task.id.0),
                settled,
                req.cancel_token.clone(),
            )
            .await;
            return Err(error);
        }
    };
    stage_feed.abort();
    stage_tracker.lock().unwrap_or_else(|error| error.into_inner()).set(RunStage::Complete);
    // ADR-55 Phase 2b (M3): on a completed planning-only run, bind the
    // rendered plan (the coordinator's final message) to this objective,
    // keyed by the same plan_id the coordinator persisted as the durable
    // PlanArtifact (ADR-52), newest-wins.
    if planning_only && output.completion_status == AgentCompletionStatus::Completed {
        let plan_id = coordinator
            .last_plan_id()
            .map(ToString::to_string)
            .unwrap_or_else(|| Ulid::new().to_string());
        let source_revision = current_source_revision(&req.project_dir).await;
        let binding = PlanBinding::new(
            plan_id.clone(),
            plan_objective_hash.clone(),
            source_revision,
            output.final_message.clone(),
        );
        plan_registry().insert(session_id, binding.clone());
        // Live-fix: mirror the binding to durable storage so "i approve the
        // plan" offered after an app restart still arms the dialog.
        // Fail-soft: a persistence failure never fails the run — the
        // in-memory registry still arms it in-process.
        if let Some(store) = &session_store {
            if let Err(error) = store
                .save_plan_binding(
                    &PlanBindingRecord {
                        session_id,
                        objective_hash: binding.objective_hash().to_owned(),
                        plan_id: binding.plan_id().to_owned(),
                        plan_text: binding.plan_text().to_owned(),
                        source_revision: binding.source_revision().map(ToOwned::to_owned),
                        artifact_hash: binding.artifact_hash().map(ToOwned::to_owned),
                        created_at: binding.created_at(),
                    },
                    req.cancel_token.clone(),
                )
                .await
            {
                tracing::warn!(%error, %session_id, "failed to persist durable plan binding");
            }
        }
        tracing::debug!(%session_id, %plan_objective_hash, %plan_id, "stored plan binding for objective");
    }
    output.project_root =
        Some(camino::Utf8PathBuf::from_path_buf(project_dir.clone()).unwrap_or_default());
    persist_provider_metrics(
        session_store.as_ref(),
        session_id,
        &output.provider_metrics,
        req.cancel_token.clone(),
    )
    .await;
    // One spend record per completed subtask call (each entry in
    // `provider_metrics` maps to one settled `AgentRunner` run; the run's
    // root task id is attributed since per-subtask ids are not exposed here).
    // Best-effort: a spend-persistence failure never fails the run.
    persist_spend_records(
        session_store.as_ref(),
        session_id,
        Some(task.id.0),
        &output.provider_metrics,
        req.cancel_token.clone(),
    )
    .await;
    if let Some(store) = &session_store {
        let assistant_message = Message {
            role: concerto_core::types::Role::Assistant,
            content: output.final_message.clone(),
            tool_calls: None,
            tool_results: None,
            reasoning_content: None,
            tokens_in: None,
            tokens_out: None,
        };
        if let Err(error) =
            store.append_messages(session_id, &[assistant_message], req.cancel_token.clone()).await
        {
            if is_expected_cancellation(&error, &req.cancel_token) {
                tracing::debug!(%error, "run cancelled; assistant message not persisted");
            } else {
                tracing::warn!(%error, "failed to persist multi-agent assistant message");
            }
        }
    }
    // Final transcript entries (ADR-36 §4): assistant text + completion marker.
    transcript_recorder
        .append_entries(&[
            TranscriptEntry::Assistant { content: output.final_message.clone() },
            TranscriptEntry::Completion {
                multi_agent: true,
                completed: output.completion_status == AgentCompletionStatus::Completed,
                files: output.files_modified.iter().map(ToString::to_string).collect(),
                project_root: output.project_root.as_ref().map(ToString::to_string),
            },
        ])
        .await;
    maintain_context_after_run(
        session_store.as_ref(),
        session_id,
        services.config.context.as_ref(),
        req.cancel_token.clone(),
        Some(&services.bus),
    )
    .await;
    transcript_recorder.stop().await;
    event_recorder.stop().await;
    Ok(output)
}

/// Carry persisted conversational decisions into the multi-agent path.
///
/// The coordinator and every specialist derive their prompts from the parent
/// task, so enriching that task keeps planning and execution on the same
/// context without introducing a second, specialist-specific history channel.
fn multi_agent_task_with_history(mut task: AgentTask, history: &[Message]) -> AgentTask {
    if history.is_empty() {
        return task;
    }

    let mut context = String::from(
        "\n\n<conversation_history>\n\
         The following messages are prior context from this same session. \
         Preserve the user's earlier decisions and constraints:\n",
    );
    const MAX_HISTORY_CHARS: usize = 16_000;
    const MAX_MESSAGE_CHARS: usize = 4_000;
    let mut selected = Vec::new();
    let mut selected_chars = 0usize;
    for message in history.iter().rev() {
        if selected_chars >= MAX_HISTORY_CHARS {
            break;
        }
        let content = message.content.chars().take(MAX_MESSAGE_CHARS).collect::<String>();
        selected_chars = selected_chars.saturating_add(content.chars().count());
        selected.push((message.role.clone(), content));
    }
    selected.reverse();
    if selected.len() < history.len() {
        context.push_str(
            "\n[older session history omitted; retrieve durable events/messages if needed]\n",
        );
    }
    for (message_role, message_content) in selected {
        let role = match message_role {
            concerto_core::types::Role::System => "system",
            concerto_core::types::Role::User => "user",
            concerto_core::types::Role::Assistant => "assistant",
            concerto_core::types::Role::Tool => "tool",
            _ => "unknown",
        };
        context.push_str("\n[");
        context.push_str(role);
        context.push_str("]\n");
        context.push_str(&message_content);
    }
    context.push_str("\n</conversation_history>");
    task.description.push_str(&context);
    task
}

fn is_resume_request(input: &str) -> bool {
    matches!(
        input.trim().to_ascii_lowercase().as_str(),
        "continue" | "resume" | "keep going" | "continue the task" | "resume the task"
    )
}

/// ADR-55 Phase 2b (M2): discard the session's orchestration checkpoint
/// before a plan-driven (Apply) Execute so the run re-plans from the
/// approved plan instead of silently resuming an old partial graph — the
/// same objective hash would otherwise trip the implicit-resume check.
async fn suppress_stale_checkpoint_for_apply(
    session_manager: &ProjectSessionManager,
    session_id: Ulid,
) {
    if let Err(error) = session_manager.store().clear_orchestration_checkpoint(session_id).await {
        tracing::warn!(%error, "failed to clear checkpoint before plan-driven Execute");
    }
}

async fn current_source_revision(project_dir: &std::path::Path) -> Option<String> {
    let output = tokio::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(project_dir)
        .output()
        .await
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|revision| !revision.is_empty())
}

#[cfg(test)]
mod runtime_runner_tests {
    use super::*;
    use crate::plan_approval::plan_artifact_hash;
    use crate::services::ServicesBuilder;
    use concerto_core::error::ToolError;
    use concerto_core::event::EventKind;
    use concerto_core::event::EventReceiver;
    use concerto_core::traits::approval::ApprovalDecision;
    use concerto_core::traits::provider::CompletionStream;
    use concerto_core::traits::tool::Tool;
    use concerto_core::types::Role;
    use concerto_core::types::{CapabilitySet, CompletionChunk, TokenBudget, ToolCall, ToolOutput};
    use futures::stream;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn router_row_correlation_id_never_records_nil_id() {
        // ADR-55 Phase 2c §5/C4 (finding-2 guard): a run without a classifier
        // call — a fast-path route, or a disabled / unavailable / cancelled
        // classifier — must mint a fresh per-event id — never
        // `Ulid::default()`, which is the all-zero nil id that would silently
        // break the audit trail's correlation chain for such runs.
        let fresh = router_row_correlation_id(None);
        assert_ne!(fresh, Ulid::default(), "nil correlation id must never be recorded");
        // A classifier's correlation id is shared unchanged with the router row.
        let shared = Ulid::new();
        assert_eq!(router_row_correlation_id(Some(shared)), shared);
    }

    /// ADR-56 §1 fast paths: the negation-override and smalltalk routes are
    /// deterministic read-only outcomes that run BEFORE the LLM classifier —
    /// they must never invoke it (read-only safety invariant; zero-cost chat).
    #[test]
    fn classifier_skips_the_two_fast_paths() {
        assert!(
            !classifier_applies_to(&RouterRoute::RuleHit { rule: "negation_override" }),
            "negation-override must never reach the classifier"
        );
        assert!(
            !classifier_applies_to(&RouterRoute::RuleHit { rule: "smalltalk" }),
            "smalltalk must never reach the classifier"
        );
    }

    /// ADR-56 §1: every other route — keyword hits, question results, and
    /// ask-user ambiguity — is classifier-eligible when the classifier is
    /// enabled.
    #[test]
    fn classifier_applies_to_every_non_fast_path_route() {
        for rule in [
            "question",
            "verify_keyword",
            "plan_keyword",
            "review_keyword",
            "diagnose_keyword",
            "execute_keyword",
        ] {
            assert!(
                classifier_applies_to(&RouterRoute::RuleHit { rule }),
                "rule {rule} must be classifier-eligible"
            );
        }
        assert!(classifier_applies_to(&RouterRoute::AskUser), "ask-user ambiguity classifies");
        // Never produced by `route()`; included to pin the contract that the
        // classifier does not re-classify its own re-route.
        assert!(classifier_applies_to(&RouterRoute::LlmClassifier));
    }

    #[test]
    fn multi_agent_task_includes_persisted_conversation_history() {
        let session_id = Ulid::new();
        let task = AgentTask::new_action_required(session_id, "apply the fix");
        let history = vec![
            Message {
                role: Role::User,
                content: "do not change the public API".into(),
                tool_calls: None,
                tool_results: None,
                reasoning_content: None,
                tokens_in: None,
                tokens_out: None,
            },
            Message {
                role: Role::Assistant,
                content: "understood".into(),
                tool_calls: None,
                tool_results: None,
                reasoning_content: None,
                tokens_in: None,
                tokens_out: None,
            },
        ];

        let enriched = multi_agent_task_with_history(task, &history);

        assert!(enriched.description.starts_with("apply the fix"));
        assert!(enriched.description.contains("[user]\ndo not change the public API"));
        assert!(enriched.description.contains("[assistant]\nunderstood"));
    }

    #[test]
    fn multi_agent_task_is_unchanged_without_history() {
        let task = AgentTask::new_action_required(Ulid::new(), "apply the fix");
        let enriched = multi_agent_task_with_history(task.clone(), &[]);
        assert_eq!(enriched.description, task.description);
    }

    // ------------------------------------------------------------------
    // topology_roles (ADR-35 phase 4)
    // ------------------------------------------------------------------

    #[test]
    fn topology_roles_includes_coordinator_builtins_and_customs() {
        let multi_agent = concerto_config::MultiAgentConfig {
            custom_agents: vec![
                concerto_config::CustomAgentConfig {
                    id: "docs-writer".into(),
                    name: "Docs Writer".into(),
                    role: "docs-writer".into(),
                    ..Default::default()
                },
                concerto_config::CustomAgentConfig {
                    id: "copilot".into(),
                    name: "Copilot".into(),
                    role: "copilot".into(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };

        let roles = topology_roles(&Some(multi_agent));

        assert_eq!(
            roles,
            vec![
                AgentId::new("coordinator"),
                AgentId::new("architect"),
                AgentId::new("researcher"),
                AgentId::new("coder"),
                AgentId::new("reviewer"),
                AgentId::new("validator"),
                AgentId::new("docs-writer"),
                AgentId::new("copilot"),
            ],
            "coordinator first, builtins in fixed order, custom agents last"
        );
    }

    #[test]
    fn topology_roles_excludes_disabled() {
        let multi_agent = concerto_config::MultiAgentConfig {
            custom_agents: vec![
                // Disabled built-in (reviewer): omitted from the builtin pass
                // and the custom pass alike.
                concerto_config::CustomAgentConfig {
                    id: "reviewer".into(),
                    name: "Reviewer".into(),
                    role: "reviewer".into(),
                    disabled: true,
                    ..Default::default()
                },
                // Disabled custom agent: omitted.
                concerto_config::CustomAgentConfig {
                    id: "docs-writer".into(),
                    name: "Docs Writer".into(),
                    role: "docs-writer".into(),
                    disabled: true,
                    ..Default::default()
                },
                // Enabled custom agent: still present.
                concerto_config::CustomAgentConfig {
                    id: "copilot".into(),
                    name: "Copilot".into(),
                    role: "copilot".into(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };

        let roles = topology_roles(&Some(multi_agent));

        assert!(
            !roles.contains(&AgentId::new("reviewer")),
            "disabled built-in must be omitted from the topology"
        );
        assert!(
            !roles.contains(&AgentId::new("docs-writer")),
            "disabled custom agent must be omitted from the topology"
        );
        assert!(roles.contains(&AgentId::new("copilot")));
        assert_eq!(roles.first(), Some(&AgentId::new("coordinator")));
        // Deterministic order: coordinator, then builtins, then customs.
        let builtins = ["architect", "researcher", "coder", "validator"];
        let builtin_positions: Vec<usize> = builtins
            .iter()
            .map(|name| roles.iter().position(|r| r.as_str() == *name).unwrap())
            .collect();
        assert_eq!(
            builtin_positions,
            vec![1, 2, 3, 4],
            "builtins keep their fixed order after the coordinator"
        );
        assert_eq!(
            roles.last(),
            Some(&AgentId::new("copilot")),
            "custom agents come last, in config order"
        );
    }

    // ------------------------------------------------------------------
    // legacy_pins_from_config (per-agent model pins in the fallback chain)
    // ------------------------------------------------------------------

    #[test]
    fn custom_agent_model_override_feeds_legacy_pins_without_assignments() {
        // Config with NO `model_settings.agent_assignments` but a custom
        // agent pinning a model on the "coder" role: the resolved model must
        // come from the override (via the legacy-pin fallback), not the
        // provider default.
        let multi_agent = concerto_config::MultiAgentConfig {
            custom_agents: vec![concerto_config::CustomAgentConfig {
                id: "coder".into(),
                name: "Coder".into(),
                role: "coder".into(),
                model_override: Some("coder-model-x".into()),
                ..Default::default()
            }],
            ..Default::default()
        };

        let pins = legacy_pins_from_config(&Some(multi_agent));

        assert_eq!(
            pins.get(&AgentId::new("coder")),
            Some(&"coder-model-x".to_string()),
            "per-agent model pin must feed the legacy fallback used when no assignment exists"
        );
    }

    #[test]
    fn legacy_pins_preserve_model_pins_and_skip_unset_overrides() {
        // Explicit `model_pins` survive; custom agents without a non-empty
        // `model_override` add no pin.
        let multi_agent = concerto_config::MultiAgentConfig {
            model_pins: std::collections::HashMap::from([(
                AgentId::new("researcher"),
                "researcher-model".to_string(),
            )]),
            custom_agents: vec![
                concerto_config::CustomAgentConfig {
                    id: "coder".into(),
                    name: "Coder".into(),
                    role: "coder".into(),
                    model_override: Some("   ".into()),
                    ..Default::default()
                },
                concerto_config::CustomAgentConfig {
                    id: "docs".into(),
                    name: "Docs".into(),
                    role: "docs-writer".into(),
                    model_override: None,
                    ..Default::default()
                },
            ],
            ..Default::default()
        };

        let pins = legacy_pins_from_config(&Some(multi_agent));

        assert_eq!(
            pins.get(&AgentId::new("researcher")),
            Some(&"researcher-model".to_string()),
            "explicit model_pins are preserved"
        );
        assert!(
            !pins.contains_key(&AgentId::new("coder")),
            "whitespace-only model_override is skipped"
        );
        assert!(
            !pins.contains_key(&AgentId::new("docs-writer")),
            "custom agent without model_override adds no pin"
        );
    }

    // ------------------------------------------------------------------
    // Tool-calling role set (ADR-35 phase 4)
    // ------------------------------------------------------------------
    // ADR-58 P2+P3 (R5/F1): the standalone `tool_calling_roles_for` and its
    // unit tests were deleted — routing consumes the facade's
    // `tool_calling_roles` (the single implementation preserving the full
    // legacy disjunction, design doc §4 Q5). The disjunction is pinned by
    // `BlueprintFacade::tool_calling_roles_preserve_legacy_disjunction`
    // (crates/config/src/facade.rs); the runtime seam above (line ~2830)
    // hands that set to `RoutingEngine::with_tool_calling_roles`.

    // ------------------------------------------------------------------
    // Mock SessionStore for event-recorder testing
    // ------------------------------------------------------------------

    /// Records `record_event` and `append_transcript` calls; all other methods
    /// panic if invoked.
    struct EventRecorderStore {
        events: Arc<Mutex<Vec<concerto_core::event::Event>>>,
        transcript: Arc<Mutex<Vec<concerto_core::transcript::TranscriptEntry>>>,
    }

    impl EventRecorderStore {
        fn new() -> Self {
            Self {
                events: Arc::new(Mutex::new(Vec::new())),
                transcript: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn recorded_events(&self) -> Vec<concerto_core::event::Event> {
            self.events.lock().unwrap_or_else(|e| e.into_inner()).clone()
        }

        fn recorded_transcript(&self) -> Vec<concerto_core::transcript::TranscriptEntry> {
            self.transcript.lock().unwrap_or_else(|e| e.into_inner()).clone()
        }
    }

    #[async_trait::async_trait]
    impl SessionStore for EventRecorderStore {
        async fn record_event(
            &self,
            _session_id: Ulid,
            event: &concerto_core::event::Event,
            _cancel: CancellationToken,
        ) -> Result<(), concerto_sessions::SessionError> {
            self.events.lock().unwrap_or_else(|e| e.into_inner()).push(event.clone());
            Ok(())
        }

        async fn create_session(
            &self,
            _project_dir: &camino::Utf8Path,
            _provider: &str,
            _model: &str,
            _cancel: CancellationToken,
        ) -> Result<concerto_sessions::Session, concerto_sessions::SessionError> {
            unimplemented!("not expected in this test")
        }

        async fn load_session(
            &self,
            _id: Ulid,
            _cancel: CancellationToken,
        ) -> Result<Option<concerto_sessions::Session>, concerto_sessions::SessionError> {
            unimplemented!("not expected in this test")
        }

        async fn save_message(
            &self,
            _session_id: Ulid,
            _msg: &Message,
            _tokens_in: u64,
            _tokens_out: u64,
            _cancel: CancellationToken,
        ) -> Result<(), concerto_sessions::SessionError> {
            unimplemented!("not expected in this test")
        }

        async fn append_messages(
            &self,
            _session_id: Ulid,
            _messages: &[Message],
            _cancel: CancellationToken,
        ) -> Result<(), concerto_sessions::SessionError> {
            unimplemented!("not expected in this test")
        }

        async fn load_messages(
            &self,
            _session_id: Ulid,
            _cancel: CancellationToken,
        ) -> Result<Vec<Message>, concerto_sessions::SessionError> {
            unimplemented!("not expected in this test")
        }

        async fn list_recent_sessions(
            &self,
            _limit: usize,
            _cancel: CancellationToken,
        ) -> Result<Vec<concerto_sessions::SessionSummary>, concerto_sessions::SessionError>
        {
            unimplemented!("not expected in this test")
        }

        async fn list_sessions_older_than(
            &self,
            _before_unix: i64,
            _cancel: CancellationToken,
        ) -> Result<Vec<concerto_sessions::SessionSummary>, concerto_sessions::SessionError>
        {
            unimplemented!("not expected in this test")
        }

        async fn delete_session(
            &self,
            _id: Ulid,
            _cancel: CancellationToken,
        ) -> Result<bool, concerto_sessions::SessionError> {
            unimplemented!("not expected in this test")
        }

        async fn active_session_ids(
            &self,
            _cancel: CancellationToken,
        ) -> Result<Vec<Ulid>, concerto_sessions::SessionError> {
            unimplemented!("not expected in this test")
        }

        async fn list_sessions_for_project(
            &self,
            _project_dir: &camino::Utf8Path,
            _limit: usize,
            _cancel: CancellationToken,
        ) -> Result<Vec<concerto_sessions::SessionSummary>, concerto_sessions::SessionError>
        {
            unimplemented!("not expected in this test")
        }

        async fn get_active_session_for_project(
            &self,
            _project_dir: &camino::Utf8Path,
            _cancel: CancellationToken,
        ) -> Result<Option<Ulid>, concerto_sessions::SessionError> {
            unimplemented!("not expected in this test")
        }

        async fn set_active_session_for_project(
            &self,
            _project_dir: &camino::Utf8Path,
            _session_id: Ulid,
            _cancel: CancellationToken,
        ) -> Result<(), concerto_sessions::SessionError> {
            unimplemented!("not expected in this test")
        }

        async fn record_metrics(
            &self,
            _session_id: Ulid,
            _metrics: ProviderMetrics,
            _cancel: CancellationToken,
        ) -> Result<(), concerto_sessions::SessionError> {
            unimplemented!("not expected in this test")
        }

        async fn load_events(
            &self,
            _session_id: Ulid,
            _cancel: CancellationToken,
        ) -> Result<Vec<concerto_sessions::replay::StoredEvent>, concerto_sessions::SessionError>
        {
            unimplemented!("not expected in this test")
        }

        async fn load_events_until(
            &self,
            _session_id: Ulid,
            _max_seq: u64,
            _cancel: CancellationToken,
        ) -> Result<Vec<concerto_sessions::replay::StoredEvent>, concerto_sessions::SessionError>
        {
            unimplemented!("not expected in this test")
        }

        async fn record_spend(
            &self,
            _record: concerto_sessions::spend::SpendRecord,
            _cancel: CancellationToken,
        ) -> Result<(), concerto_sessions::SessionError> {
            unimplemented!("not expected in this test")
        }

        async fn list_spend_records(
            &self,
            _session_id: Ulid,
            _cancel: CancellationToken,
        ) -> Result<Vec<concerto_sessions::spend::SpendRecord>, concerto_sessions::SessionError>
        {
            unimplemented!("not expected in this test")
        }

        async fn spend_summary(
            &self,
            _session_id: Ulid,
            _cancel: CancellationToken,
        ) -> Result<concerto_sessions::spend::SpendSummary, concerto_sessions::SessionError>
        {
            unimplemented!("not expected in this test")
        }

        async fn create_task(
            &self,
            _task: &concerto_core::types::AgentTask,
            _cancel: CancellationToken,
        ) -> Result<(), concerto_sessions::SessionError> {
            unimplemented!("not expected in this test")
        }

        async fn update_task_status(
            &self,
            _task_id: concerto_core::types::TaskId,
            _status: &str,
            _cancel: CancellationToken,
        ) -> Result<(), concerto_sessions::SessionError> {
            unimplemented!("not expected in this test")
        }

        async fn get_task(
            &self,
            _task_id: concerto_core::types::TaskId,
            _cancel: CancellationToken,
        ) -> Result<Option<concerto_core::types::AgentTask>, concerto_sessions::SessionError>
        {
            unimplemented!("not expected in this test")
        }

        async fn list_tasks(
            &self,
            _session_id: Ulid,
            _cancel: CancellationToken,
        ) -> Result<Vec<concerto_core::types::AgentTask>, concerto_sessions::SessionError> {
            unimplemented!("not expected in this test")
        }

        async fn create_checkpoint(
            &self,
            _session_id: Ulid,
            _task_id: concerto_core::types::TaskId,
            _label: &str,
            _vfs_snapshot: &str,
            _sequence_num: u64,
            _cancel: CancellationToken,
        ) -> Result<Ulid, concerto_sessions::SessionError> {
            unimplemented!("not expected in this test")
        }

        async fn load_checkpoint(
            &self,
            _checkpoint_id: Ulid,
            _cancel: CancellationToken,
        ) -> Result<(String, u64), concerto_sessions::SessionError> {
            unimplemented!("not expected in this test")
        }

        async fn list_checkpoints(
            &self,
            _session_id: Ulid,
            _cancel: CancellationToken,
        ) -> Result<Vec<concerto_sessions::CheckpointSummary>, concerto_sessions::SessionError>
        {
            unimplemented!("not expected in this test")
        }

        async fn save_orchestration_checkpoint(
            &self,
            _record: &concerto_sessions::OrchestrationCheckpointRecord,
        ) -> Result<(), concerto_sessions::SessionError> {
            unimplemented!("not expected in this test")
        }

        async fn load_orchestration_checkpoint(
            &self,
            _session_id: Ulid,
        ) -> Result<
            Option<concerto_sessions::OrchestrationCheckpointRecord>,
            concerto_sessions::SessionError,
        > {
            unimplemented!("not expected in this test")
        }

        async fn clear_orchestration_checkpoint(
            &self,
            _session_id: Ulid,
        ) -> Result<(), concerto_sessions::SessionError> {
            unimplemented!("not expected in this test")
        }

        async fn append_transcript(
            &self,
            _session_id: Ulid,
            entries: &[concerto_core::transcript::TranscriptEntry],
            _cancel: CancellationToken,
        ) -> Result<(), concerto_sessions::SessionError> {
            self.transcript.lock().unwrap_or_else(|e| e.into_inner()).extend_from_slice(entries);
            Ok(())
        }

        async fn load_transcript(
            &self,
            _session_id: Ulid,
            _cancel: CancellationToken,
        ) -> Result<Vec<concerto_core::transcript::TranscriptEntry>, concerto_sessions::SessionError>
        {
            Ok(self.recorded_transcript())
        }
    }

    #[tokio::test]
    async fn event_recorder_filters_cross_session_events() {
        let bus = EventBus::default();
        let recorder_store = Arc::new(EventRecorderStore::new());
        let store: Arc<dyn SessionStore> = recorder_store.clone();

        let session_a = Ulid::new();
        let session_b = Ulid::new();

        let recorder = start_event_recorder(&bus, store, session_a);

        // Publish an event for session A — should be persisted.
        bus.publish(concerto_core::event::Event::new(
            Ulid::new(),
            session_a,
            concerto_core::event::EventKind::SessionSaved,
        ))
        .ok();

        // Publish an event for session B — should be filtered out.
        bus.publish(concerto_core::event::Event::new(
            Ulid::new(),
            session_b,
            concerto_core::event::EventKind::SessionSaved,
        ))
        .ok();

        // Stop the recorder — this flushes any buffered events and waits for
        // the background task to complete.
        recorder.stop().await;

        let recorded = recorder_store.recorded_events();
        assert_eq!(recorded.len(), 1, "only one event should be recorded");
        assert_eq!(recorded[0].session_id, session_a, "recorded event must belong to session A");
    }

    // ------------------------------------------------------------------
    // Transcript recorder (ADR-36, stage 2)
    // ------------------------------------------------------------------

    fn transcript_store() -> (Arc<EventRecorderStore>, Arc<dyn SessionStore>, Ulid, EventBus) {
        let recorder_store = Arc::new(EventRecorderStore::new());
        let store: Arc<dyn SessionStore> = recorder_store.clone();
        (recorder_store, store, Ulid::new(), EventBus::default())
    }

    /// Publish a tool-lifecycle event for `session_id` on `bus` (helper).
    fn publish(bus: &EventBus, session_id: Ulid, kind: EventKind) {
        bus.publish(Event::new(Ulid::new(), session_id, kind)).ok();
    }

    #[tokio::test]
    async fn transcript_recorder_correlates_tool_calls_into_single_entries() {
        let (recorder_store, store, session_id, bus) = transcript_store();
        let recorder = start_transcript_recorder(&bus, store, session_id, GateLabels::default());

        // Started + Finished(success) → one Completed entry.
        publish(
            &bus,
            session_id,
            EventKind::ToolExecutionStarted {
                tool_name: "read_file".into(),
                input_hash: "h1".into(),
                detail: Some("read src/main.rs".into()),
            },
        );
        publish(
            &bus,
            session_id,
            EventKind::ToolExecutionFinished {
                tool_name: "read_file".into(),
                duration_ms: 3,
                success: true,
                detail: Some("read 42 bytes".into()),
            },
        );

        // Started + ApprovalResolved(approved=true) → one Allowed entry.
        publish(
            &bus,
            session_id,
            EventKind::ToolExecutionStarted {
                tool_name: "write_file".into(),
                input_hash: "h2".into(),
                detail: Some("write notes.md".into()),
            },
        );
        publish(
            &bus,
            session_id,
            EventKind::ApprovalResolved { tool_name: "write_file".into(), approved: true },
        );

        // Started + ToolTimeout → one Failed entry.
        publish(
            &bus,
            session_id,
            EventKind::ToolExecutionStarted {
                tool_name: "shell".into(),
                input_hash: "h3".into(),
                detail: Some("cargo build".into()),
            },
        );
        publish(
            &bus,
            session_id,
            EventKind::ToolTimeout { tool_name: "shell".into(), timeout_secs: 30 },
        );

        // Started + ApprovalTimeout → one Cancelled entry.
        publish(
            &bus,
            session_id,
            EventKind::ToolExecutionStarted {
                tool_name: "git".into(),
                input_hash: "h4".into(),
                detail: Some("git status".into()),
            },
        );
        publish(
            &bus,
            session_id,
            EventKind::ApprovalTimeout { tool_name: "git".into(), timeout_secs: 60 },
        );

        recorder.stop().await;

        let transcript = recorder_store.recorded_transcript();
        assert_eq!(
            transcript,
            vec![
                TranscriptEntry::ToolCall {
                    tool_name: "read_file".into(),
                    detail: "read src/main.rs\nread 42 bytes".into(),
                    status: TranscriptToolStatus::Completed,
                },
                TranscriptEntry::ToolCall {
                    tool_name: "write_file".into(),
                    detail: "write notes.md".into(),
                    status: TranscriptToolStatus::Allowed,
                },
                TranscriptEntry::ToolCall {
                    tool_name: "shell".into(),
                    detail: "cargo build".into(),
                    status: TranscriptToolStatus::Failed,
                },
                TranscriptEntry::ToolCall {
                    tool_name: "git".into(),
                    detail: "git status".into(),
                    status: TranscriptToolStatus::Cancelled,
                },
            ],
            "one entry per invocation with the terminal status merged in"
        );
    }

    #[tokio::test]
    async fn transcript_recorder_preserves_event_order_across_merge() {
        // A Running tool call holds its position: a thinking line published
        // after the tool started must appear after the merged ToolCall entry.
        let (recorder_store, store, session_id, bus) = transcript_store();
        let recorder = start_transcript_recorder(&bus, store, session_id, GateLabels::default());

        publish(
            &bus,
            session_id,
            EventKind::AgentThought { agent_id: "coder".into(), content: "plan".into() },
        );
        publish(
            &bus,
            session_id,
            EventKind::ToolExecutionStarted {
                tool_name: "fs_write".into(),
                input_hash: "h1".into(),
                detail: Some("write main.rs".into()),
            },
        );
        publish(
            &bus,
            session_id,
            EventKind::AgentThought {
                agent_id: "coder".into(),
                content: "observing result".into(),
            },
        );
        publish(
            &bus,
            session_id,
            EventKind::ToolExecutionFinished {
                tool_name: "fs_write".into(),
                duration_ms: 5,
                success: true,
                detail: Some("wrote 42 bytes".into()),
            },
        );

        recorder.stop().await;

        let transcript = recorder_store.recorded_transcript();
        assert_eq!(
            transcript,
            vec![
                TranscriptEntry::Thinking { agent: "coder".into(), content: "plan".into() },
                TranscriptEntry::ToolCall {
                    tool_name: "fs_write".into(),
                    detail: "write main.rs\nwrote 42 bytes".into(),
                    status: TranscriptToolStatus::Completed,
                },
                TranscriptEntry::Thinking {
                    agent: "coder".into(),
                    content: "observing result".into(),
                },
            ],
            "terminal event merges in place; interleaved lines keep their position"
        );
    }

    #[tokio::test]
    async fn transcript_recorder_settles_running_tool_calls_on_stop() {
        let (recorder_store, store, session_id, bus) = transcript_store();
        let recorder = start_transcript_recorder(&bus, store, session_id, GateLabels::default());

        // Only the start event is published: at stop the Running entry must
        // settle to Cancelled (ADR-36 settle-on-stop).
        publish(
            &bus,
            session_id,
            EventKind::ToolExecutionStarted {
                tool_name: "fs_write".into(),
                input_hash: "h1".into(),
                detail: Some("write main.rs".into()),
            },
        );

        recorder.stop().await;

        let transcript = recorder_store.recorded_transcript();
        assert_eq!(
            transcript,
            vec![TranscriptEntry::ToolCall {
                tool_name: "fs_write".into(),
                detail: "write main.rs".into(),
                status: TranscriptToolStatus::Cancelled,
            }]
        );
    }

    #[tokio::test]
    async fn transcript_recorder_records_user_and_final_entries() {
        // Guard-API level: the run-start user prompt and the run-end assistant
        // + completion entries are recorded explicitly (ADR-36 §4).
        let (recorder_store, store, session_id, bus) = transcript_store();
        let recorder = start_transcript_recorder(&bus, store, session_id, GateLabels::default());

        recorder.record_user_message("build the widget".to_string()).await;
        recorder
            .append_entries(&[
                TranscriptEntry::Assistant { content: "done".into() },
                TranscriptEntry::Completion {
                    multi_agent: false,
                    completed: true,
                    files: vec!["main.rs".into()],
                    project_root: Some("/tmp/proj".into()),
                },
            ])
            .await;
        recorder.stop().await;

        let transcript = recorder_store.recorded_transcript();
        assert_eq!(
            transcript,
            vec![
                TranscriptEntry::User { content: "build the widget".into() },
                TranscriptEntry::Assistant { content: "done".into() },
                TranscriptEntry::Completion {
                    multi_agent: false,
                    completed: true,
                    files: vec!["main.rs".into()],
                    project_root: Some("/tmp/proj".into()),
                },
            ],
            "transcript shape is [User, Assistant, Completion]"
        );
    }

    #[tokio::test]
    async fn transcript_recorder_flushes_batches_in_order() {
        // More than the batch threshold of transcript-relevant events: the
        // recorder flushes an in-order mid-run batch and the stop flush must
        // not reorder or drop any entry.
        let (recorder_store, store, session_id, bus) = transcript_store();
        let recorder = start_transcript_recorder(&bus, store, session_id, GateLabels::default());

        for i in 0..40 {
            publish(
                &bus,
                session_id,
                EventKind::AgentThought { agent_id: "coder".into(), content: format!("step {i}") },
            );
        }
        recorder.stop().await;

        let transcript = recorder_store.recorded_transcript();
        assert_eq!(transcript.len(), 40, "no entry may be lost across batch flush");
        for (i, entry) in transcript.iter().enumerate() {
            assert_eq!(
                *entry,
                TranscriptEntry::Thinking { agent: "coder".into(), content: format!("step {i}") }
            );
        }
    }

    #[tokio::test]
    async fn transcript_recorder_filters_cross_session_events() {
        let (recorder_store, store, _session_id, bus) = transcript_store();
        let session_a = Ulid::new();
        let session_b = Ulid::new();

        let recorder = start_transcript_recorder(&bus, store, session_a, GateLabels::default());

        publish(
            &bus,
            session_a,
            EventKind::AgentThought { agent_id: "coder".into(), content: "step one".into() },
        );
        publish(
            &bus,
            session_b,
            EventKind::AgentThought { agent_id: "coder".into(), content: "other session".into() },
        );

        recorder.stop().await;

        let transcript = recorder_store.recorded_transcript();
        assert_eq!(transcript.len(), 1, "only this session's events should be recorded");
        assert_eq!(
            transcript[0],
            TranscriptEntry::Thinking { agent: "coder".into(), content: "step one".into() }
        );
    }

    // ------------------------------------------------------------------
    // Audit G1: project-switch memory isolation. The selection/reset core
    // of `select_or_init_memory_services` is tested directly with lightweight
    // in-memory stand-ins (no network, no SQLite): the same project reuses
    // its cached store, while a different project never reuses the previous
    // project's memory and drops its lifecycle entirely.
    // ------------------------------------------------------------------

    /// Minimal embedding generator — returns a fixed zero vector, never
    /// touches fastembed or the network.
    struct DummyEmbedder;

    #[async_trait]
    impl EmbeddingGenerator for DummyEmbedder {
        async fn embed(&self, _text: &str) -> Result<Vec<f32>, concerto_core::MemoryError> {
            Ok(vec![0.0; 4])
        }

        fn model_id(&self) -> &str {
            "dummy"
        }

        fn model_version(&self) -> &str {
            "test"
        }

        fn dims(&self) -> usize {
            4
        }
    }

    /// Minimal vector store — accepts writes, returns nothing on search.
    struct DummyVectorStore;

    #[async_trait]
    impl concerto_memory::vector_store::VectorStore for DummyVectorStore {
        async fn store(
            &self,
            _records: &[concerto_core::memory::EmbeddingRecord],
            _cancel: CancellationToken,
        ) -> Result<(), concerto_core::MemoryError> {
            Ok(())
        }

        async fn search(
            &self,
            _project_id: &ProjectId,
            _query: &[f32],
            _top_k: usize,
            _cancel: CancellationToken,
        ) -> Result<Vec<concerto_core::memory::VectorResult>, concerto_core::MemoryError> {
            Ok(Vec::new())
        }

        async fn tombstone(
            &self,
            _chunk_id: &str,
            _project_id: &ProjectId,
            _cancel: CancellationToken,
        ) -> Result<(), concerto_core::MemoryError> {
            Ok(())
        }

        async fn delete_tombstoned(
            &self,
            _project_id: &ProjectId,
            _cancel: CancellationToken,
        ) -> Result<(), concerto_core::MemoryError> {
            Ok(())
        }

        async fn mark_stale(
            &self,
            _project_id: &ProjectId,
            _chunk_id: &str,
            _cancel: CancellationToken,
        ) -> Result<(), concerto_core::MemoryError> {
            Ok(())
        }

        async fn delete_by_project(
            &self,
            _project_id: &ProjectId,
            _cancel: CancellationToken,
        ) -> Result<(), concerto_core::MemoryError> {
            Ok(())
        }

        async fn delete_by_file_path(
            &self,
            _project_id: &ProjectId,
            _file_path: &camino::Utf8PathBuf,
            _cancel: CancellationToken,
        ) -> Result<Vec<String>, concerto_core::MemoryError> {
            Ok(Vec::new())
        }
    }

    /// Minimal full-text store — accepts writes, returns nothing on search.
    struct DummyFullTextStore;

    #[async_trait]
    impl concerto_memory::fts::FullTextStore for DummyFullTextStore {
        async fn insert(
            &self,
            _chunk: &concerto_core::memory::MemoryChunk,
            _project_id: &ProjectId,
            _cancel: CancellationToken,
        ) -> Result<(), concerto_core::MemoryError> {
            Ok(())
        }

        async fn delete(
            &self,
            _chunk_id: &str,
            _project_id: &ProjectId,
            _cancel: CancellationToken,
        ) -> Result<(), concerto_core::MemoryError> {
            Ok(())
        }

        async fn search(
            &self,
            _query: &str,
            _project_id: &ProjectId,
            _top_k: usize,
            _cancel: CancellationToken,
        ) -> Result<Vec<concerto_core::memory::FtsResult>, concerto_core::MemoryError> {
            Ok(Vec::new())
        }

        async fn delete_by_project(
            &self,
            _project_id: &ProjectId,
            _cancel: CancellationToken,
        ) -> Result<(), concerto_core::MemoryError> {
            Ok(())
        }
    }

    /// A fully populated `ActiveMemoryServices` slot for `project_id` whose
    /// store identity is the passed `Arc` (so reuse can be proven with
    /// pointer equality).
    fn active_memory_for(
        project_id: ProjectId,
        store: Arc<dyn MemoryStore>,
    ) -> ActiveMemoryServices {
        ActiveMemoryServices {
            project_id,
            store,
            reindex: Arc::new(ProjectIndexer::new(
                Arc::new(DummyEmbedder),
                EventBus::new(16),
                ProjectId("unused".into()),
            )),
            reindex_sync: Arc::new(ChunkSyncService::new(
                Arc::new(DummyVectorStore),
                Arc::new(DummyFullTextStore),
            )),
            cancel: CancellationToken::new(),
        }
    }

    /// G1: a run for the same project reuses the cached store (same `Arc`
    /// identity — no re-initialisation, no project boundary crossed).
    #[test]
    fn same_project_reuses_cached_store() {
        let project_a = ProjectId("project-a".into());
        let store_a: Arc<dyn MemoryStore> = Arc::new(NullMemoryStore);
        let memory =
            Arc::new(Mutex::new(Some(active_memory_for(project_a.clone(), store_a.clone()))));

        let selected = cached_store_for_project(&memory, &project_a)
            .expect("same project must reuse the cached store");
        assert!(Arc::ptr_eq(&store_a, &selected), "reuse must hand back the same store");
    }

    /// G1: a run for a *different* project must not reuse the previous
    /// project's store — the selection returns `None`, forcing a reset +
    /// fresh initialisation for the new project.
    #[test]
    fn different_project_never_reuses_previous_store() {
        let project_a = ProjectId("project-a".into());
        let project_b = ProjectId("project-b".into());
        let store_a: Arc<dyn MemoryStore> = Arc::new(NullMemoryStore);
        let memory =
            Arc::new(Mutex::new(Some(active_memory_for(project_a.clone(), store_a.clone()))));

        assert!(
            cached_store_for_project(&memory, &project_b).is_none(),
            "a project switch must never hand back the previous project's store"
        );
    }

    /// G1: switching projects cancels and drops the previous project's
    /// lifecycle (indexer, sync service, store, cancellation token), so no
    /// memory state leaks across project boundaries.
    #[test]
    fn project_switch_drops_previous_lifecycle() {
        let project_a = ProjectId("project-a".into());
        let store_a: Arc<dyn MemoryStore> = Arc::new(NullMemoryStore);
        let memory =
            Arc::new(Mutex::new(Some(active_memory_for(project_a.clone(), store_a.clone()))));
        let cancel_token = memory
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .as_ref()
            .expect("services present")
            .cancel
            .clone();

        reset_memory_services(&memory);

        assert!(
            memory.lock().unwrap_or_else(|poison| poison.into_inner()).is_none(),
            "the previous project's services must be dropped on a project switch"
        );
        assert!(
            cancel_token.is_cancelled(),
            "the previous project's lifecycle token must be cancelled"
        );
    }

    /// G1 end-to-end shape: after switching to a different project, even a
    /// subsequent run for the *original* project must not find a stale cache
    /// — the previous lifecycle is gone and a fresh initialisation happens.
    #[test]
    fn switch_away_and_back_requires_fresh_initialisation() {
        let project_a = ProjectId("project-a".into());
        let project_b = ProjectId("project-b".into());
        let store_a: Arc<dyn MemoryStore> = Arc::new(NullMemoryStore);
        let memory =
            Arc::new(Mutex::new(Some(active_memory_for(project_a.clone(), store_a.clone()))));

        // Run for project B: no reuse, previous lifecycle dropped.
        assert!(cached_store_for_project(&memory, &project_b).is_none());
        reset_memory_services(&memory);

        // Run for project A again: the cache is gone, fresh init is required.
        assert!(
            cached_store_for_project(&memory, &project_a).is_none(),
            "no stale cache may survive a project switch"
        );
    }

    // ------------------------------------------------------------------
    // Spend-log persistence (Phase 3, issue #93)
    // ------------------------------------------------------------------

    /// One spend record is persisted per completed provider call with the
    /// settled actual cost, exactly the data `persist_spend_records` receives
    /// from the run output. Exercises the best-effort path against a real
    /// in-memory store so the row is actually written.
    #[tokio::test]
    async fn persist_spend_records_writes_one_record_per_metrics_entry() {
        let store = concerto_sessions::SqliteSessionStore::connect_in_memory().await.unwrap();
        let store: Arc<dyn SessionStore> = Arc::new(store);
        // `spend_records.session_id` REFERENCES sessions(id), so the session
        // must exist for the row to be written.
        let session_id = store
            .create_session(
                camino::Utf8Path::new("/tmp/spend-persist"),
                "openai",
                "gpt-4",
                CancellationToken::new(),
            )
            .await
            .unwrap()
            .id;
        let task_id = Ulid::new();

        // Two entries = two settled provider calls (e.g. two subtasks in a
        // multi-agent run). One carries an empty provider and must be skipped,
        // mirroring the `persist_provider_metrics` guard.
        let metrics = vec![
            ProviderMetrics {
                provider: "openai".into(),
                model: "gpt-4".into(),
                tokens_in: 120,
                tokens_out: 60,
                cost_usd: 0.02,
                latency_ms: 42,
            },
            ProviderMetrics {
                provider: "".into(),
                model: "gpt-4".into(),
                tokens_in: 999,
                tokens_out: 999,
                cost_usd: 1.0,
                latency_ms: 0,
            },
        ];

        persist_spend_records(
            Some(&store),
            session_id,
            Some(task_id),
            &metrics,
            CancellationToken::new(),
        )
        .await;

        let records = store.list_spend_records(session_id, CancellationToken::new()).await.unwrap();
        assert_eq!(
            records.len(),
            1,
            "exactly one record per settled call, empty providers skipped"
        );
        assert_eq!(records[0].session_id, session_id);
        assert_eq!(records[0].task_id, Some(task_id));
        assert_eq!(records[0].provider, "openai");
        assert_eq!(records[0].model, "gpt-4");
        assert_eq!(records[0].tokens_in, 120);
        assert_eq!(records[0].tokens_out, 60);
        assert!((records[0].cost_usd - 0.02).abs() < f64::EPSILON);
    }

    // ------------------------------------------------------------------
    // task_action_required (B-2): gate state shapes the task, not just the
    // effective outcome.
    // ------------------------------------------------------------------

    #[test]
    fn task_action_required_confirmed_execute_is_action_required() {
        // Gate enabled + effective Execute + mutation-capable run.
        assert!(task_action_required(RequestedOutcome::Execute, false));
    }

    #[test]
    fn task_action_required_dismissed_execute_is_answer_only() {
        // A dismissed/absent confirmation keeps effective Execute but the gate
        // forced the run read-only (B-2): shaping the task action-required
        // would start the agent_loop ActionRequired retry loop that steers the
        // model toward a mutation the gate hard-denies.
        assert!(!task_action_required(RequestedOutcome::Execute, true));
    }

    #[test]
    fn task_action_required_non_execute_outcomes_are_answer_only() {
        for outcome in [
            RequestedOutcome::Answer,
            RequestedOutcome::Diagnose,
            RequestedOutcome::Review,
            RequestedOutcome::Plan,
            RequestedOutcome::Verify,
        ] {
            assert!(
                !task_action_required(outcome, false),
                "{outcome:?} must shape an answer-only task"
            );
            assert!(
                !task_action_required(outcome, true),
                "{outcome:?} stays answer-only when read-only"
            );
        }
    }

    // ------------------------------------------------------------------
    // ADR-55 Phase 2b (M3, live-fix): an Apply run's task describes the
    // approved plan, never the approval phrase that armed the dialog.
    // ------------------------------------------------------------------

    #[test]
    fn approved_plan_task_description_embeds_plan_text_and_id() {
        let plan_text = "step 1: read the code\nstep 2: implement the change";
        let binding = PlanBinding::new(
            "plan-123".into(),
            "0123456789abcdef0123456789abcdef".into(),
            None,
            plan_text.into(),
        );

        let description = approved_plan_task_description(&binding);

        assert!(
            description.contains(binding.plan_id()),
            "the description names the plan id, got: {description}"
        );
        assert!(description.contains(plan_text), "the description carries the full plan text");
    }

    #[test]
    fn apply_run_uses_approved_plan_not_approval_phrase() {
        let session_id = Ulid::new();
        let plan_text = "step 1: read the code";
        let binding = PlanBinding::new(
            "plan-123".into(),
            "0123456789abcdef0123456789abcdef".into(),
            None,
            plan_text.into(),
        );
        let input = "i approve";

        // An Apply run with a captured binding executes the approved plan.
        let task = build_run_task(session_id, false, true, Some(&binding), input);
        assert!(
            task.description.contains(plan_text),
            "Apply task describes the approved plan, got: {}",
            task.description,
        );
        assert!(
            !task.description.contains(input),
            "Apply task must not carry the approval phrase, got: {}",
            task.description,
        );
        assert!(
            matches!(
                task.execution_mode,
                concerto_core::types::TaskExecutionMode::ActionRequired { .. }
            ),
            "the Apply task must stay action-required"
        );

        // Defensive fallback: apply without a captured binding (should be
        // impossible) reuses the input rather than panicking.
        let fallback = build_run_task(session_id, true, true, None, input);
        assert_eq!(fallback.description, input);

        // Non-apply routing is unchanged: action-required and answer-only
        // tasks both carry the user's input verbatim.
        let action = build_run_task(session_id, true, false, None, input);
        assert_eq!(action.description, input);
        assert!(
            matches!(
                action.execution_mode,
                concerto_core::types::TaskExecutionMode::ActionRequired { .. }
            ),
            "a confirmed Execute must stay action-required"
        );
        let answer = build_run_task(session_id, false, false, None, "explain X");
        assert_eq!(answer.description, "explain X");
        assert_eq!(answer.execution_mode, concerto_core::types::TaskExecutionMode::AnswerOnly);
    }

    // ------------------------------------------------------------------
    // Run-stage tracking (ADR-55 Phase 2a): StageTracker transition-only
    // emission and the single-agent wiring through execute_agent_loop.
    // ------------------------------------------------------------------

    // -----------------------------------------------------------------------
    // Local ApprovalTestHarness (can't import from concerto_core::testing
    // because that module is cfg(test) for the core crate, not re-exported
    // to downstream crate test builds).
    // -----------------------------------------------------------------------

    struct ApprovalTestHarness {
        decisions: VecDeque<ApprovalDecision>,
    }

    impl ApprovalTestHarness {
        fn always_approve() -> Self {
            Self { decisions: VecDeque::new() }
        }
    }

    #[async_trait]
    impl ApprovalSink for ApprovalTestHarness {
        async fn request_approval(
            &self,
            _action: &concerto_core::types::PolicyAction<'_>,
            _cancel: CancellationToken,
        ) -> ApprovalDecision {
            self.decisions.clone().into_iter().next().unwrap_or(ApprovalDecision::Approve)
        }
        async fn approve_all_for_session(&self, _session_id: Ulid, _cancel: CancellationToken) {}
        async fn request_ack(&self, _message: &str, _cancel: CancellationToken) -> bool {
            true // auto-acknowledge in tests
        }
    }

    struct TestAudit;
    #[async_trait]
    impl AuditLog for TestAudit {
        async fn record(
            &self,
            _entry: AuditEntry,
            _cancel: CancellationToken,
        ) -> Result<(), PolicyError> {
            Ok(())
        }
    }

    /// A tool that simulates writing a file (file-changing tool), so the
    /// agent_loop marks the run as having changed files.
    struct WriteFileTool;
    #[async_trait]
    impl Tool for WriteFileTool {
        fn name(&self) -> &str {
            "write_file"
        }
        fn description(&self) -> &str {
            "writes content to a file"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        fn capability_requirements(&self) -> CapabilitySet {
            CapabilitySet::default()
        }
        async fn execute(
            &self,
            _input: serde_json::Value,
            _policy: &dyn concerto_core::traits::policy::PolicyEngine,
            _session: &SessionContext,
            _cancel: CancellationToken,
        ) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput {
                summary: "file written".into(),
                data: serde_json::json!({"file_path": "/tmp/test.rs"}),
            })
        }
    }

    /// A mock LLM provider that returns a predefined sequence of tool-call
    /// batches. Each call to `stream_completion` advances to the next batch.
    struct ScriptedProvider {
        responses: Vec<Vec<ToolCall>>,
        call_count: AtomicUsize,
    }

    impl ScriptedProvider {
        fn new(responses: Vec<Vec<ToolCall>>) -> Self {
            Self { responses, call_count: AtomicUsize::new(0) }
        }
    }

    #[async_trait]
    impl LlmProvider for ScriptedProvider {
        fn provider_name(&self) -> &'static str {
            "scripted"
        }
        fn context_capacity(&self, _model: &str) -> TokenBudget {
            TokenBudget::new(128_000, 4_096)
        }
        fn approximate_cost(&self, _tokens_in: u64, _tokens_out: u64) -> f64 {
            0.0
        }
        async fn stream_completion(
            &self,
            _request: CompletionRequest,
            _cancel: CancellationToken,
        ) -> Result<CompletionStream, ProviderError> {
            let idx = self.call_count.fetch_add(1, Ordering::SeqCst);
            let tool_calls = self.responses.get(idx).cloned().unwrap_or_default();
            let chunks: Vec<_> = if tool_calls.is_empty() {
                vec![CompletionChunk {
                    reasoning: None,
                    delta: String::new(),
                    tool_call: None,
                    is_final: true,
                    usage: None,
                }]
            } else {
                tool_calls
                    .into_iter()
                    .map(|tc| CompletionChunk {
                        reasoning: None,
                        delta: String::new(),
                        tool_call: Some(tc),
                        is_final: false,
                        usage: None,
                    })
                    .collect()
            };
            Ok(Box::pin(stream::iter(chunks.into_iter().map(Ok))))
        }
    }

    /// A provider that always fails with a non-transient error, so the run
    /// fails fast (no retry sleep).
    struct FailingProvider;
    #[async_trait]
    impl LlmProvider for FailingProvider {
        fn provider_name(&self) -> &'static str {
            "failing"
        }
        fn context_capacity(&self, _model: &str) -> TokenBudget {
            TokenBudget::new(128_000, 4_096)
        }
        fn approximate_cost(&self, _tokens_in: u64, _tokens_out: u64) -> f64 {
            0.0
        }
        async fn stream_completion(
            &self,
            _request: CompletionRequest,
            _cancel: CancellationToken,
        ) -> Result<CompletionStream, ProviderError> {
            Err(ProviderError::NotConfigured)
        }
    }

    fn make_tool_call(name: &str, text: &str) -> ToolCall {
        ToolCall {
            id: "call_1".into(),
            name: name.into(),
            arguments: serde_json::json!({"text": text}),
        }
    }

    /// Shared services for a direct `execute_agent_loop` invocation, with an
    /// always-approve approval sink and default config.
    fn make_services(bus: EventBus) -> SharedServices {
        let approval = Arc::new(ApprovalTestHarness::always_approve());
        ServicesBuilder::new(bus, AppConfig::default(), approval).build()
    }

    /// An executor whose registry contains the file-changing `WriteFileTool`
    /// behind an allow-all policy, so tool calls execute without approvals.
    fn make_executor() -> Arc<ToolExecutor> {
        let mut registry = ToolRegistry::default();
        registry.register(Box::new(WriteFileTool));
        let allow_all = vec![PolicyRule::AutoApprove(Condition::Always)];
        let policy = Arc::new(SimplePolicyEngine::new(allow_all, Arc::new(TestAudit)));
        Arc::new(
            ToolExecutor::new(Arc::new(registry), policy)
                .with_approval_sink(Arc::new(ApprovalTestHarness::always_approve())),
        )
    }

    /// Drain all buffered `RunStageChanged` events for `session_id` in order.
    fn drain_stage_events(receiver: &mut EventReceiver, session_id: Ulid) -> Vec<RunStage> {
        let mut stages = Vec::new();
        while let Ok(event) = receiver.try_recv() {
            if event.session_id == session_id {
                if let EventKind::RunStageChanged { stage, .. } = &event.kind {
                    stages.push(*stage);
                }
            }
        }
        stages
    }

    #[test]
    fn stage_tracker_publishes_only_transitions() {
        let bus = EventBus::new(256);
        let mut receiver = bus.subscribe();
        let session_id = Ulid::new();
        let task_id = TaskId::new();
        let mut tracker = StageTracker::new(bus, session_id, task_id);

        tracker.set(RunStage::Understand);
        tracker.set(RunStage::Understand); // duplicate — must not re-publish
        tracker.set(RunStage::Inspect);
        tracker.set(RunStage::Execute);
        tracker.set(RunStage::Execute); // duplicate — must not re-publish
        tracker.set(RunStage::Complete);

        let mut seen = Vec::new();
        while let Ok(event) = receiver.try_recv() {
            if let EventKind::RunStageChanged { task_id: got, stage } = &event.kind {
                assert_eq!(*got, task_id, "RunStageChanged carries the run's task id");
                assert_eq!(event.correlation_id, task_id.0);
                assert_eq!(event.session_id, session_id);
                seen.push(*stage);
            }
        }
        assert_eq!(
            seen,
            vec![RunStage::Understand, RunStage::Inspect, RunStage::Execute, RunStage::Complete],
            "exactly one event per stage transition, duplicates deduped"
        );
    }

    fn routing(input: &str) -> RouterOutput {
        concerto_core::intent::route(input, std::path::PathBuf::from("/tmp"))
    }

    /// Live-fix regression: "i approve the plan" hashes differently than the
    /// original objective and routes as `Plan` (the `plan` keyword wins), so
    /// the approval must arm the Apply/Replan dialog through the session-wide
    /// newest binding — never silently re-trigger a planning run.
    #[test]
    fn approval_phrase_arms_dialog_via_session_binding() {
        let session = Ulid::new();
        let hash = "0123456789abcdef0123456789abcdef".to_owned();
        plan_registry().insert(
            session,
            PlanBinding::new("plan-1".into(), hash, None, "step 1: build verdict".into()),
        );

        let routing = routing("i approve the plan");
        assert_eq!(
            routing.outcome,
            RequestedOutcome::Plan,
            "regression premise: the approval phrase routes as a new Plan run"
        );
        let binding = bound_plan_for_approval(
            &routing,
            session,
            "fedcba9876543210fedcba9876543210",
            "i approve the plan",
        );
        assert_eq!(
            binding.map(|b| b.plan_id().to_owned()),
            Some("plan-1".to_owned()),
            "approving the rendered plan in natural language must surface the dialog"
        );
    }

    /// The dialog must NOT arm for plan-named change requests ("apply the
    /// fix" is an Execute intent about a change, not an approval of a stored
    /// plan) nor when the session holds no binding at all.
    ///
    /// Scope note: this pins `bound_plan_for_approval`'s phrase/hash behavior
    /// in isolation. At run level, ADR-55 §11 later supersedes the "apply the
    /// fix" premise: a confident Execute with a durable session-newest row
    /// arms the dialog via `arm_binding_for_confident_execute`.
    #[test]
    fn approval_phrase_requires_session_binding_and_plan_coupled_language() {
        let session = Ulid::new();
        let hash = "0123456789abcdef0123456789abcdef".to_owned();
        let routing_out = routing("i approve the plan");

        // Phrase but no binding: falls through to the generic gate.
        assert!(
            bound_plan_for_approval(&routing_out, session, &hash, "i approve the plan").is_none(),
            "no session binding, no dialog"
        );

        // Binding but change-execution language under a *different* objective:
        // "apply the fix" is an Execute intent about a change, not an approval
        // of the stored plan, and it does not hash to the binding's objective.
        plan_registry().insert(
            session,
            PlanBinding::new("plan-1".into(), hash.clone(), None, "step 1: build verdict".into()),
        );
        assert!(
            bound_plan_for_approval(
                &routing("apply the fix"),
                session,
                "ffffffffffffffffffffffffffffffff",
                "apply the fix"
            )
            .is_none(),
            "\"apply the fix\" names a change, not the stored plan"
        );
        plan_registry().remove(session, &hash);
    }

    /// Live-fix: bare real-world approvals ("I approve", "yes") are phrases,
    /// while denials/hesitations containing an approval word never are.
    #[test]
    fn approval_phrases_cover_bare_approvals_and_reject_denials() {
        for input in [
            "I approve",
            "i approve the plan",
            "yes",
            "yes, do it",
            "yep",
            "approved",
            "approved it",
            "go ahead",
            "looks good",
            "sounds good",
            "proceed with the plan",
            "run the plan",
            "run plan",
        ] {
            assert!(is_plan_approval_phrase(input), "expected an approval phrase: {input:?}");
        }
        for input in [
            "don't approve the plan",
            "don't apply it",
            "do not apply",
            "not yet",
            "never",
            "wait, i need to review this first",
            "hold on",
            "actually no",
            "i don't approve",
        ] {
            assert!(
                !is_plan_approval_phrase(input),
                "expected a denial, not an approval: {input:?}"
            );
        }
        // Change-execution language stays out (Execute intent, not approval).
        assert!(!is_plan_approval_phrase("apply the fix"));
    }

    /// Live-fix (restart-safe dialog): a durable binding in the session DB
    /// rehydrates into the once-empty in-process registry and then arms the
    /// dialog for the approval phrase exactly like an in-process hit.
    #[tokio::test]
    async fn durable_binding_rehydrates_and_arms_dialog() {
        use concerto_sessions::{PlanBindingRecord, SqliteSessionStore};

        let store = SqliteSessionStore::connect_in_memory().await.expect("in-memory store");
        let session = Ulid::new();
        let created_at =
            time::OffsetDateTime::from_unix_timestamp(1_700_000_000).expect("valid timestamp");
        store
            .save_plan_binding(
                &PlanBindingRecord {
                    session_id: session,
                    objective_hash: "obj-hash-1".to_owned(),
                    plan_id: "plan-1".to_owned(),
                    plan_text: "step 1: build verdict".to_owned(),
                    source_revision: Some("abc1234".to_owned()),
                    artifact_hash: Some(plan_artifact_hash("step 1: build verdict")),
                    created_at,
                },
                CancellationToken::new(),
            )
            .await
            .expect("durable save");

        // Simulate a restart: the process registry holds nothing for this
        // session; rehydration must restore the binding WITH its original age.
        let binding = rehydrate_durable_binding(&store, session, CancellationToken::new())
            .await
            .expect("rehydrated binding");
        assert_eq!(binding.plan_id(), "plan-1");
        assert_eq!(binding.created_at(), created_at, "original age preserved");

        // The re-seeded registry now arms the dialog for the approval phrase
        // under the phrase branch (a fresh input hash never matches).
        let out = bound_plan_for_approval(
            &routing("i approve the plan"),
            session,
            "fedcba9876543210fedcba9876543210",
            "i approve the plan",
        );
        assert_eq!(out.map(|b| b.plan_id().to_owned()), Some("plan-1".to_owned()));
    }

    /// ADR-55 §11 (round-4 live-fix): a confident Execute with a durable
    /// session-newest plan record arms the Apply/Replan dialog with the
    /// STORED plan — plan_id, original objective hash, plan text, source
    /// revision and original age all preserved — so "execute" executes the
    /// stored plan instead of silently re-planning.
    #[test]
    fn confident_execute_arms_dialog_from_durable_session_newest_binding() {
        let session = Ulid::new();
        let created_at =
            time::OffsetDateTime::from_unix_timestamp(1_700_000_000).expect("valid timestamp");
        let record = PlanBindingRecord {
            session_id: session,
            objective_hash: "obj-hash-original".to_owned(),
            plan_id: "plan-1".to_owned(),
            plan_text: "step 1: build verdict".to_owned(),
            source_revision: Some("abc1234".to_owned()),
            artifact_hash: Some(plan_artifact_hash("step 1: build verdict")),
            created_at,
        };

        let routed = routing("apply the fix");
        assert_eq!(routed.outcome, RequestedOutcome::Execute, "premise: execute keyword");
        assert!(
            routed.confidence >= LOW_CONFIDENCE_THRESHOLD,
            "premise: the routing is confident enough to arm"
        );

        let binding = arm_binding_for_confident_execute(&routed, Some(record))
            .expect("confident Execute with a durable row arms the dialog");
        assert_eq!(binding.plan_id(), "plan-1");
        assert_eq!(
            binding.objective_hash(),
            "obj-hash-original",
            "the binding keeps the ORIGINAL plan objective, not the execute input (ADR-55 §11)"
        );
        assert_eq!(binding.plan_text(), "step 1: build verdict");
        assert_eq!(binding.source_revision(), Some("abc1234"));
        assert_eq!(binding.created_at(), created_at, "original age preserved");
    }

    /// ADR-55 §11: a confident Execute with NO durable row falls through to
    /// the generic intent gate (fail-soft) — no dialog, no plan executed.
    #[test]
    fn confident_execute_without_durable_binding_stays_on_generic_gate() {
        let routed = routing("apply the fix");
        assert_eq!(routed.outcome, RequestedOutcome::Execute, "premise: execute keyword");
        assert!(
            arm_binding_for_confident_execute(&routed, None).is_none(),
            "no durable row means the unchanged generic gate decides"
        );
    }

    /// ADR-55 §12: the user's natural follow-up word "execute" (bare) routes
    /// as a confident Execute and arms the dialog from the durable binding —
    /// the exact gap observed in live rounds 4/5 where it fell to the
    /// generic AskUser list modal instead.
    #[test]
    fn bare_execute_arms_dialog_from_durable_binding() {
        let session = Ulid::new();
        let record = PlanBindingRecord {
            session_id: session,
            objective_hash: "obj-hash-original".to_owned(),
            plan_id: "plan-1".to_owned(),
            plan_text: "step 1: build verdict".to_owned(),
            source_revision: Some("abc1234".to_owned()),
            artifact_hash: Some(plan_artifact_hash("step 1: build verdict")),
            created_at: time::OffsetDateTime::from_unix_timestamp(1_700_000_000)
                .expect("valid timestamp"),
        };

        let routed = routing("execute");
        assert_eq!(
            routed.outcome,
            RequestedOutcome::Execute,
            "premise: bare execute routes Execute"
        );
        assert!(routed.confidence >= LOW_CONFIDENCE_THRESHOLD, "premise: confident enough to arm");

        let binding = arm_binding_for_confident_execute(&routed, Some(record))
            .expect("bare Execute with a durable row arms the dialog");
        assert_eq!(binding.plan_id(), "plan-1");
        assert_eq!(
            binding.objective_hash(),
            "obj-hash-original",
            "the binding keeps the ORIGINAL plan objective, not the execute input (ADR-55 §11)"
        );
        assert_eq!(binding.plan_text(), "step 1: build verdict");
    }

    /// ADR-55 §11: only a confident EXECUTE outcome arms from the durable
    /// session-newest binding. Non-Execute outcomes (e.g. "i approve the
    /// plan" routing as a fresh Plan run) keep their own phrase/hash paths
    /// untouched — this fallback never hijacks them.
    #[test]
    fn non_execute_outcome_never_arms_from_durable_binding() {
        let session = Ulid::new();
        let record = PlanBindingRecord {
            session_id: session,
            objective_hash: "obj-hash-original".to_owned(),
            plan_id: "plan-1".to_owned(),
            plan_text: "step 1: build verdict".to_owned(),
            source_revision: None,
            artifact_hash: Some(plan_artifact_hash("step 1: build verdict")),
            created_at: time::OffsetDateTime::from_unix_timestamp(1_700_000_000)
                .expect("valid timestamp"),
        };

        let routed = routing("i approve the plan");
        assert_eq!(routed.outcome, RequestedOutcome::Plan, "premise: plan keyword wins");
        assert!(
            arm_binding_for_confident_execute(&routed, Some(record)).is_none(),
            "a non-Execute outcome never arms from durable storage"
        );
    }

    // ------------------------------------------------------------------
    // ADR-55 §1 (pending): diff-vs-artifact — the dialog's plan text must
    // match the binding's creation-time artifact hash or it falls through to
    // the generic intent gate (fail-soft, no dialog).
    // ------------------------------------------------------------------

    /// A durable row whose plan_text was altered after creation (tampered or
    /// corrupted storage) must NOT arm the dialog: the text no longer matches
    /// the artifact hash captured at insert time.
    #[test]
    fn tampered_durable_plan_text_falls_through_to_generic_gate() {
        let session = Ulid::new();
        let record = PlanBindingRecord {
            session_id: session,
            objective_hash: "obj-hash-original".to_owned(),
            plan_id: "plan-1".to_owned(),
            plan_text: "step 1: build verdict AND delete everything".to_owned(),
            source_revision: Some("abc1234".to_owned()),
            artifact_hash: Some(plan_artifact_hash("step 1: build verdict")),
            created_at: time::OffsetDateTime::from_unix_timestamp(1_700_000_000)
                .expect("valid timestamp"),
        };

        let routed = routing("apply the fix");
        assert_eq!(routed.outcome, RequestedOutcome::Execute, "premise: execute keyword");
        assert!(
            arm_binding_for_confident_execute(&routed, Some(record)).is_none(),
            "plan text that does not match its artifact hash never arms the dialog"
        );
    }

    /// A legacy durable row (written before migration 025) carries no artifact
    /// hash. It is unverifiable, and per ADR-55 the verification is
    /// load-bearing: fall through to the generic intent gate.
    #[test]
    fn legacy_durable_row_without_artifact_hash_falls_through() {
        let session = Ulid::new();
        let record = PlanBindingRecord {
            session_id: session,
            objective_hash: "obj-hash-original".to_owned(),
            plan_id: "plan-1".to_owned(),
            plan_text: "step 1: build verdict".to_owned(),
            source_revision: Some("abc1234".to_owned()),
            artifact_hash: None,
            created_at: time::OffsetDateTime::from_unix_timestamp(1_700_000_000)
                .expect("valid timestamp"),
        };

        let routed = routing("apply the fix");
        assert_eq!(routed.outcome, RequestedOutcome::Execute, "premise: execute keyword");
        assert!(
            arm_binding_for_confident_execute(&routed, Some(record)).is_none(),
            "an unverifiable legacy row falls through to the generic gate"
        );
    }

    /// The registry arming path verifies too: an in-memory binding whose plan
    /// text was altered after insert (hash mismatch) must not arm the dialog.
    #[test]
    fn registry_binding_with_tampered_text_does_not_arm_dialog() {
        let session = Ulid::new();
        let hash = "0123456789abcdef0123456789abcdef".to_owned();
        plan_registry().insert(
            session,
            PlanBinding::restored(
                "plan-1".into(),
                hash.clone(),
                None,
                "step 1: build verdict AND delete everything".into(),
                Some(plan_artifact_hash("step 1: build verdict")),
                time::OffsetDateTime::now_utc(),
            ),
        );

        // "apply the fix" is a confident Execute replaying the objective the
        // binding is keyed on — the tampered binding must fall through.
        let routed = routing("apply the fix");
        assert_eq!(routed.outcome, RequestedOutcome::Execute, "premise: execute keyword");
        assert!(
            bound_plan_for_approval(&routed, session, &hash, "apply the fix").is_none(),
            "a registry binding with altered plan text never arms the dialog"
        );
        plan_registry().remove(session, &hash);
    }

    /// End-to-end through real SQLite: a binding tampered IN STORAGE after
    /// insert is rejected at rehydration — it must not be re-seeded into the
    /// registry and must not arm the dialog.
    #[tokio::test]
    async fn rehydration_rejects_tampered_durable_binding() {
        use concerto_sessions::SqliteSessionStore;

        let store = SqliteSessionStore::connect_in_memory().await.expect("in-memory store");
        let session = Ulid::new();
        let created_at =
            time::OffsetDateTime::from_unix_timestamp(1_700_000_000).expect("valid timestamp");
        store
            .save_plan_binding(
                &PlanBindingRecord {
                    session_id: session,
                    objective_hash: "obj-hash-1".to_owned(),
                    plan_id: "plan-1".to_owned(),
                    plan_text: "step 1: build verdict".to_owned(),
                    source_revision: Some("abc1234".to_owned()),
                    artifact_hash: Some(plan_artifact_hash("step 1: build verdict")),
                    created_at,
                },
                CancellationToken::new(),
            )
            .await
            .expect("durable save");

        // Tamper the stored text while keeping the ORIGINAL artifact hash —
        // exactly what a corrupted or hand-edited database looks like.
        store
            .save_plan_binding(
                &PlanBindingRecord {
                    session_id: session,
                    objective_hash: "obj-hash-1".to_owned(),
                    plan_id: "plan-1".to_owned(),
                    plan_text: "step 1: build verdict AND delete everything".to_owned(),
                    source_revision: Some("abc1234".to_owned()),
                    artifact_hash: Some(plan_artifact_hash("step 1: build verdict")),
                    created_at,
                },
                CancellationToken::new(),
            )
            .await
            .expect("tampered save");

        assert!(
            rehydrate_durable_binding(&store, session, CancellationToken::new()).await.is_none(),
            "a durable binding whose text no longer matches its artifact hash is not rehydrated"
        );
    }

    /// Deterministic replay of the exact original objective arms the dialog
    /// under the observed `Diagnose` routing too (the live `plan1:` prompt
    /// landed in Diagnose) and under Plan; pure-text Answer replays never do.
    #[test]
    fn exact_objective_replay_arms_dialog_under_planning_routes() {
        let session = Ulid::new();
        let hash = "0123456789abcdef0123456789abcdef".to_owned();
        plan_registry().insert(
            session,
            PlanBinding::new("plan-1".into(), hash.clone(), None, "step 1: build verdict".into()),
        );

        for (input, expected_route) in [
            ("draft a plan for the refactor", RequestedOutcome::Plan),
            ("diagnose this crash", RequestedOutcome::Diagnose),
            ("give me a code review of this change", RequestedOutcome::Review),
            ("please verify the release build", RequestedOutcome::Verify),
        ] {
            let routing = routing(input);
            assert_eq!(routing.outcome, expected_route);
            let binding = bound_plan_for_approval(&routing, session, &hash, input);
            assert!(
                binding.is_some(),
                "exact-objective replay under {expected_route:?} must arm the dialog"
            );
        }
        assert!(
            bound_plan_for_approval(
                &routing("What is the fastest way to sort a list?"),
                session,
                &hash,
                "What is the fastest way to sort a list?"
            )
            .is_none(),
            "a pure-text Answer replay never arms the dialog"
        );
    }

    #[tokio::test]
    async fn stage_tracker_emits_execute_sequence_for_action_required_run() {
        let bus = EventBus::new(256);
        let mut receiver = bus.subscribe();
        let session_id = Ulid::new();
        let store: Arc<dyn SessionStore> = Arc::new(EventRecorderStore::new());
        let event_recorder = start_event_recorder(&bus, store.clone(), session_id);
        let transcript_recorder =
            start_transcript_recorder(&bus, store.clone(), session_id, GateLabels::default());

        let services = make_services(bus.clone());
        let provider: Arc<dyn LlmProvider> = Arc::new(ScriptedProvider::new(vec![
            vec![make_tool_call("write_file", "edit")],
            vec![],
        ]));
        let executor = make_executor();

        let task = AgentTask::new_action_required(session_id, "apply the fix");
        // `run_shared_agent` creates the tracker and seeds Understand before
        // dispatching; mirror that here so the full sequence is asserted.
        let stage_tracker = Arc::new(Mutex::new(StageTracker::new(bus, session_id, task.id)));
        stage_tracker.lock().unwrap_or_else(|error| error.into_inner()).set(RunStage::Understand);

        let project_dir = tempfile::tempdir().expect("tempdir for run");
        let req = AgentRunRequest {
            input: "apply the fix".into(),
            selected_provider_id: Some("scripted".into()),
            selected_model: Some("test-model".into()),
            force_single_agent: true,
            project_dir: project_dir.path().to_path_buf(),
            session_id: Some(session_id),
            conversation_history: Vec::new(),
            memory_enabled: false,
            cancel_token: CancellationToken::new(),
            resume_checkpoint_json: None,
        };

        let output = execute_agent_loop(
            req,
            &services,
            provider,
            "test-model".into(),
            executor,
            Arc::new(NullMemoryStore),
            None,
            session_id,
            task,
            event_recorder,
            transcript_recorder,
            RequestedOutcome::Execute,
            true,
            &stage_tracker,
        )
        .await
        .expect("action-required run should complete");

        assert_eq!(output.completion_status, AgentCompletionStatus::Completed);
        assert_eq!(
            drain_stage_events(&mut receiver, session_id),
            vec![RunStage::Understand, RunStage::Inspect, RunStage::Execute, RunStage::Complete,],
            "action-required execute run reports Inspect -> Execute -> Complete"
        );
    }

    #[tokio::test]
    async fn stage_tracker_emits_plan_sequence_for_plan_run() {
        let bus = EventBus::new(256);
        let mut receiver = bus.subscribe();
        let session_id = Ulid::new();
        let store: Arc<dyn SessionStore> = Arc::new(EventRecorderStore::new());
        let event_recorder = start_event_recorder(&bus, store.clone(), session_id);
        let transcript_recorder =
            start_transcript_recorder(&bus, store.clone(), session_id, GateLabels::default());

        let services = make_services(bus.clone());
        let provider: Arc<dyn LlmProvider> = Arc::new(ScriptedProvider::new(vec![vec![]]));
        let executor = make_executor();

        let task = AgentTask::new(session_id, "propose a plan");
        let stage_tracker = Arc::new(Mutex::new(StageTracker::new(bus, session_id, task.id)));
        stage_tracker.lock().unwrap_or_else(|error| error.into_inner()).set(RunStage::Understand);

        let project_dir = tempfile::tempdir().expect("tempdir for run");
        let req = AgentRunRequest {
            input: "propose a plan".into(),
            selected_provider_id: Some("scripted".into()),
            selected_model: Some("test-model".into()),
            force_single_agent: true,
            project_dir: project_dir.path().to_path_buf(),
            session_id: Some(session_id),
            conversation_history: Vec::new(),
            memory_enabled: false,
            cancel_token: CancellationToken::new(),
            resume_checkpoint_json: None,
        };

        let output = execute_agent_loop(
            req,
            &services,
            provider,
            "test-model".into(),
            executor,
            Arc::new(NullMemoryStore),
            None,
            session_id,
            task,
            event_recorder,
            transcript_recorder,
            RequestedOutcome::Plan,
            false,
            &stage_tracker,
        )
        .await
        .expect("plan run should complete");

        assert_eq!(output.completion_status, AgentCompletionStatus::Completed);
        assert_eq!(
            drain_stage_events(&mut receiver, session_id),
            vec![RunStage::Understand, RunStage::Inspect, RunStage::Plan, RunStage::Complete],
            "plan run reports Inspect -> Plan -> Complete"
        );
    }

    #[tokio::test]
    async fn stage_tracker_omits_complete_when_run_fails() {
        let bus = EventBus::new(256);
        let mut receiver = bus.subscribe();
        let session_id = Ulid::new();
        let store: Arc<dyn SessionStore> = Arc::new(EventRecorderStore::new());
        let event_recorder = start_event_recorder(&bus, store.clone(), session_id);
        let transcript_recorder =
            start_transcript_recorder(&bus, store.clone(), session_id, GateLabels::default());

        let services = make_services(bus.clone());
        let provider: Arc<dyn LlmProvider> = Arc::new(FailingProvider);
        let executor = make_executor();

        let task = AgentTask::new_action_required(session_id, "apply the fix");
        let stage_tracker = Arc::new(Mutex::new(StageTracker::new(bus, session_id, task.id)));
        stage_tracker.lock().unwrap_or_else(|error| error.into_inner()).set(RunStage::Understand);

        let project_dir = tempfile::tempdir().expect("tempdir for run");
        let req = AgentRunRequest {
            input: "apply the fix".into(),
            selected_provider_id: Some("failing".into()),
            selected_model: Some("test-model".into()),
            force_single_agent: true,
            project_dir: project_dir.path().to_path_buf(),
            session_id: Some(session_id),
            conversation_history: Vec::new(),
            memory_enabled: false,
            cancel_token: CancellationToken::new(),
            resume_checkpoint_json: None,
        };

        let result = execute_agent_loop(
            req,
            &services,
            provider,
            "test-model".into(),
            executor,
            Arc::new(NullMemoryStore),
            None,
            session_id,
            task,
            event_recorder,
            transcript_recorder,
            RequestedOutcome::Execute,
            true,
            &stage_tracker,
        )
        .await;

        assert!(result.is_err(), "a non-transient provider error fails the run");
        assert_eq!(
            drain_stage_events(&mut receiver, session_id),
            vec![RunStage::Understand, RunStage::Inspect, RunStage::Execute],
            "an errored run never reports Complete"
        );
    }

    // ------------------------------------------------------------------
    // ADR-55 Phase 2b (M2): plan-driven Execute must not silently resume a
    // stale partial-graph checkpoint
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn apply_path_suppresses_stale_orchestration_checkpoint() {
        use concerto_sessions::{OrchestrationCheckpointRecord, SqliteSessionStore};

        let store =
            Arc::new(SqliteSessionStore::connect_in_memory().await.expect("in-memory store"));
        let store_dyn: Arc<dyn SessionStore> = store.clone();
        let manager = ProjectSessionManager::from_store(store_dyn.clone());

        // The checkpoint table FK-references the session row — create one
        // first (mirrors `get_or_create_active_session` in the runner) and
        // use its generated id.
        let project_dir = camino::Utf8PathBuf::from("/tmp/concerto-test-project");
        let session_id = store_dyn
            .create_session(&project_dir, "mock", "test-model", CancellationToken::new())
            .await
            .expect("create session")
            .id;

        // Seed a stale checkpoint from a previous partial Execute of the same
        // objective (fields mirror `persist_orchestration_checkpoint`).
        store_dyn
            .save_orchestration_checkpoint(&OrchestrationCheckpointRecord {
                session_id,
                run_id: Ulid::new(),
                root_task_id: TaskId::new(),
                project_id: "test-project".into(),
                objective_hash: "h".into(),
                schema_version: 3,
                source_revision: Some("abc123".into()),
                sequence_num: 1,
                state_json: r#"{"partial":true}"#.into(),
                completed: false,
                updated_at: time::OffsetDateTime::now_utc(),
            })
            .await
            .expect("seed checkpoint");

        assert!(
            store_dyn
                .load_orchestration_checkpoint(session_id)
                .await
                .expect("load seeded checkpoint")
                .is_some(),
            "the stale checkpoint must exist before the Apply path runs"
        );

        // The Apply path clears it (M2) so the run re-plans from the
        // approved plan instead of resuming the old partial graph.
        suppress_stale_checkpoint_for_apply(&manager, session_id).await;

        assert!(
            store_dyn
                .load_orchestration_checkpoint(session_id)
                .await
                .expect("load after suppression")
                .is_none(),
            "the Apply path must clear the stale checkpoint"
        );
    }

    // ------------------------------------------------------------------
    // ADR-58 P2+P3 (Batch 3b): per-stage feed bindings through the resolved
    // blueprint (R6/F3). The Q4 pin — a review-gate cycle now advances the
    // Verify chip — is deliberate (design doc §7 Q4; P1 binds `review →
    // Verify`, blueprint.rs:668) and is feed-only: replay binds the same
    // table `stage_feed_advance` resolves.
    // ------------------------------------------------------------------

    #[test]
    fn stage_feed_bindings_resolve_standard_and_legacy_advances() {
        use concerto_config::blueprint::OrchestrationConfig;
        use concerto_core::policy::SimplePolicyEngine;
        use concerto_core::types::ToolRegistry;
        use concerto_providers::mock::MockProvider;
        use concerto_providers::retry::RetryPolicy;

        // The resolved standard blueprint binds research → Understand, design
        // → Plan, implement → Execute, review → Verify, validate → Verify
        // (blueprint §5.6); feed-only emission and replay derive from the same
        // table.
        let resolved = OrchestrationConfig::default()
            .resolve(&[], None)
            .expect("the standard blueprint must validate and resolve");
        let facade = BlueprintFacade::new(&resolved);
        assert_eq!(
            facade.feed_for("review"),
            Some(RunStage::Verify),
            "the review-gate feed binding is the source of the Verify advance (Q4)"
        );

        let executor = Arc::new(ToolExecutor::new(
            Arc::new(ToolRegistry::default()),
            Arc::new(SimplePolicyEngine::new(Vec::new(), Arc::new(TestAudit))),
        ));
        let registry = AgentRegistry::build_with_roles_for_project(
            HashMap::new(),
            Arc::new(MockProvider::default()),
            executor,
            EventBus::new(128),
            RetryPolicy::default(),
            std::path::Path::new("."),
            &HashMap::new(),
            "",
            true,
        );

        let task_id = TaskId::new();
        // The review-gate cycle advances the chip to Verify via the review
        // stage's feed binding — today only the validation cycle did (Q4).
        assert_eq!(
            stage_feed_advance(
                &EventKind::ReviewCycleStarted { task_id, cycle_num: 1 },
                &registry,
                Some(&facade),
                false,
                false,
            ),
            Some(RunStage::Verify),
            "a review cycle must advance the Verify chip (Q4)"
        );
        // The validation cycle keeps advancing Verify through the same table.
        assert_eq!(
            stage_feed_advance(
                &EventKind::ValidationCycleStarted { task_id, cycle_num: 1 },
                &registry,
                Some(&facade),
                false,
                false,
            ),
            Some(RunStage::Verify),
            "a validation cycle keeps advancing the Verify chip"
        );
        // A staffed implement-stage subtask advances to Execute from its feed
        // binding.
        assert_eq!(
            stage_feed_advance(
                &EventKind::SubTaskCreated {
                    task_id,
                    role: AgentId::new("coder"),
                    description: "implement the change".into(),
                },
                &registry,
                Some(&facade),
                false,
                false,
            ),
            Some(RunStage::Execute),
            "a coder subtask advances to Execute from the implement feed"
        );
        // A research-stage subtask advances to Understand — the R6 per-stage
        // generalization of the old implement-only classification.
        assert_eq!(
            stage_feed_advance(
                &EventKind::SubTaskCreated {
                    task_id,
                    role: AgentId::new("researcher"),
                    description: "research".into(),
                },
                &registry,
                Some(&facade),
                false,
                false,
            ),
            Some(RunStage::Understand),
            "a researcher subtask advances to Understand from the research feed"
        );
        // The coordinator self-implement sentinel keeps advancing to Execute
        // when the Execution stage is unstaffed (review F4).
        assert_eq!(
            stage_feed_advance(
                &EventKind::SubTaskCreated {
                    task_id,
                    role: AgentId::new("coordinator"),
                    description: "self-execute".into(),
                },
                &registry,
                Some(&facade),
                false,
                true,
            ),
            Some(RunStage::Execute),
            "the coordinator self-implement sentinel advances to Execute"
        );
        // Planning-only runs (M1) never report an implement transition.
        assert_eq!(
            stage_feed_advance(
                &EventKind::ReviewCycleStarted { task_id, cycle_num: 1 },
                &registry,
                Some(&facade),
                true,
                false,
            ),
            None,
            "planning-only runs never advance past Planning"
        );
        // Without a facade the legacy feed classification keeps today's
        // behavior: a review cycle emits nothing, the validation cycle still
        // advances Verify, and only implement-stage subtasks advance Execute.
        assert_eq!(
            stage_feed_advance(
                &EventKind::ReviewCycleStarted { task_id, cycle_num: 1 },
                &registry,
                None,
                false,
                false,
            ),
            None,
            "without a facade a review cycle keeps the legacy no-advance behavior"
        );
        assert_eq!(
            stage_feed_advance(
                &EventKind::ValidationCycleStarted { task_id, cycle_num: 1 },
                &registry,
                None,
                false,
                false,
            ),
            Some(RunStage::Verify),
            "without a facade the validation cycle keeps advancing Verify"
        );
    }

    // ------------------------------------------------------------------
    // ADR-58 P2+P3 (Batch 4b): F8 — gate labels for transcript activity
    // entries resolve from the resolved blueprint's stage definitions.
    // ------------------------------------------------------------------

    #[test]
    fn gate_labels_resolve_from_blueprint_and_default_on_standard() {
        use concerto_config::blueprint::OrchestrationConfig;

        // No resolved blueprint (tests, `[orchestration]`-less configs):
        // the canonical labels are used.
        assert_eq!(
            gate_labels_for_resolved(None),
            GateLabels { review: "Reviewer".into(), validate: "Validator".into() },
        );

        // The default `standard` blueprint carries the canonical stage labels
        // ("Review"/"Validate"), so the canonical transcript strings survive
        // untouched — transcripts stay byte-identical on the default.
        let resolved = OrchestrationConfig::default()
            .resolve(&[], None)
            .expect("the standard blueprint must validate and resolve");
        assert_eq!(
            gate_labels_for_resolved(Some(&resolved)),
            GateLabels { review: "Reviewer".into(), validate: "Validator".into() },
            "standard blueprint keeps the canonical gate labels"
        );
    }

    #[test]
    fn gate_labels_route_custom_stage_labels() {
        use concerto_config::blueprint::{
            Blueprint, CapabilityMask, PipelineDef, StageCondition, StageDef, StageFlags, StageKind,
        };
        use concerto_config::ResolvedStage;
        use std::collections::HashMap;

        // A custom blueprint that renames the gate stages surfaces its
        // configured labels in transcript activity entries (F8).
        let review_def = StageDef {
            tag: "review".into(),
            label: "QA Reviewer".into(),
            kind: StageKind::Review.as_str().to_string(),
            version: 1,
            flags: StageFlags::default(),
            condition: StageCondition::Always,
            max_cycles: None,
            feed: None,
            primary: false,
            agents: vec!["reviewer".into()],
            fallback: None,
            files: None,
        };
        let validate_def = StageDef {
            tag: "validate".into(),
            label: "QA Verifier".into(),
            kind: StageKind::Acceptance.as_str().to_string(),
            version: 1,
            flags: StageFlags::default(),
            condition: StageCondition::Always,
            max_cycles: None,
            feed: None,
            primary: false,
            agents: vec!["validator".into()],
            fallback: None,
            files: None,
        };
        let resolved = ResolvedBlueprint {
            blueprint: Blueprint {
                schema_version: 1,
                name: "custom-gates".into(),
                description: None,
                pipeline: PipelineDef { stages: vec![review_def.clone(), validate_def.clone()] },
                relationships: Vec::new(),
            },
            stages: vec![
                ResolvedStage {
                    def: review_def,
                    effective_capabilities: CapabilityMask::default(),
                    effective_feed: None,
                },
                ResolvedStage {
                    def: validate_def,
                    effective_capabilities: CapabilityMask::default(),
                    effective_feed: None,
                },
            ],
            feed_map: HashMap::new(),
            relationship_defaults: Vec::new(),
        };
        assert_eq!(
            gate_labels_for_resolved(Some(&resolved)),
            GateLabels { review: "QA Reviewer".into(), validate: "QA Verifier".into() },
        );
    }

    /// Issue #150: gate labels are resolved by KIND, so renamed review/
    /// validate TAGS still surface their configured labels. This test's
    /// blueprint renames both tags (`quality`/`ship`, kinds preserved) — the
    /// canonical-tag lookup the old code used would miss them entirely and
    /// fall back to the default labels.
    #[test]
    fn gate_labels_follow_renamed_gate_tags() {
        use concerto_config::blueprint::{
            Blueprint, CapabilityMask, PipelineDef, StageCondition, StageDef, StageFlags,
        };
        use concerto_config::ResolvedStage;
        use std::collections::HashMap;

        let stage = |tag: &str, label: &str, kind: StageKind| StageDef {
            tag: tag.into(),
            label: label.into(),
            kind: kind.as_str().to_string(),
            version: 1,
            flags: StageFlags::default(),
            condition: StageCondition::Always,
            max_cycles: None,
            feed: None,
            primary: false,
            agents: Vec::new(),
            fallback: None,
            files: None,
        };
        let defs = vec![
            stage("quality", "QA Reviewer", StageKind::Review),
            stage("ship", "QA Verifier", StageKind::Acceptance),
        ];
        let resolved = ResolvedBlueprint {
            blueprint: Blueprint {
                schema_version: 1,
                name: "renamed-gate-labels".into(),
                description: None,
                pipeline: PipelineDef { stages: defs.clone() },
                relationships: Vec::new(),
            },
            stages: defs
                .iter()
                .map(|def| ResolvedStage {
                    def: def.clone(),
                    effective_capabilities: CapabilityMask::default(),
                    effective_feed: None,
                })
                .collect(),
            feed_map: HashMap::new(),
            relationship_defaults: Vec::new(),
        };
        assert_eq!(
            gate_labels_for_resolved(Some(&resolved)),
            GateLabels { review: "QA Reviewer".into(), validate: "QA Verifier".into() },
            "renamed gate tags keep their labels via kind-based resolution"
        );
    }
}
