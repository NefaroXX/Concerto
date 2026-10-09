//! Shared single‑agent runner used by both CLI and Desktop.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::agent_runner::AgentRunner;
use crate::coordinator::{ApprovedPlanSeed, CoordinatorAgent, HeadlessResumeSeed};
use crate::intent_grants::{IntentGrantStore, RunEnvelope, SessionIntentAuth};
use crate::plan_approval::{
    append_plan_approved_event, apply_auto_plan_decision, load_approved_plan, plan_registry,
    rehydrate_durable_binding, ApprovedPlanContext, PlanBinding, PlanLedger,
};
use crate::registry::AgentRegistry;
use crate::session_manager::{message_row_usage, ProjectSessionManager, SessionManagerConfig};
use crate::{AgentRelationship, CollaborationRule};

use concerto_config::AppConfig;
use concerto_config::BlueprintFacade;
use concerto_config::CredentialStore;
use concerto_config::RelationshipSemantics;
use concerto_config::StageKind;
use concerto_core::error::ProviderError;
use concerto_core::event::{EventBus, EventKind};
use concerto_core::executor::ToolExecutor;
use concerto_core::ids::Ulid;
use concerto_core::intent::{PlanDecision, RunStage};
use concerto_core::lock::DataDirLock;
use concerto_core::traits::approval::ApprovalSink;
use concerto_core::traits::memory::{MemoryStore, NullMemoryStore};
use concerto_core::traits::policy::{AuditLog, PolicyEngine};
use concerto_core::traits::provider::LlmProvider;
use concerto_core::transcript::TranscriptEntry;
use concerto_core::types::ToolRegistry;
use concerto_core::types::{
    AgentCompletionStatus, AgentContext, AgentId, AgentOutput, AgentStage, AgentTask, DesignDoc,
    Message, ProjectId, ProviderMetrics, Role, SessionContext, TaskExecutionMode, TaskId,
};
use concerto_core::types::{Condition, PolicyRule};
use concerto_core::{
    inject_intent_gate_rule, CancellationToken, IntentAuthorization, OrchestratorError,
    PolicyPresets, RpmLimiter, SimplePolicyEngine, SpendTracker,
};
use concerto_sessions::audit::SqliteAuditLog;
use concerto_sessions::PlanBindingRecord;
use concerto_sessions::SessionStore;

use concerto_providers::factory::ProviderFactory;
use concerto_providers::model_registry::ModelRegistry;
use concerto_providers::model_selector::ModelSelector;
use concerto_providers::retry::RetryPolicy;
use concerto_providers::routing::RoutingEngine;

use concerto_tools::undo::UndoManager;
use concerto_tools::virtual_fs::VirtualFs;

use crate::agent_loop::AgentLoop;
use crate::exec_backend::SharedExecutionBackend;
use crate::gate::{FilePreImageReader, WriteGate};
use crate::in_process_gate::InProcessGateBackend;
use crate::prompts::PromptBuilder;
use crate::subscriptions::SubscriptionManager;
use crate::supervisor::{AgentState, Supervisor, SupervisorConfig, SupervisorServices};

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

/// The live provider a custom agent pins for `role`, when it serves `model`.
///
/// ADR-31 precedence: an explicit, valid `settings.agent_assignments` entry is
/// resolved by the caller *before* this helper is consulted and always wins.
/// When no assignment exists, a custom agent's `provider_id` (persisted from
/// the Orchestration Studio) is honoured only if it names a live
/// `settings.providers` config — matched by [`ProviderFactory::config_id`] —
/// **and** that config advertises the role's resolved model
/// ([`ProviderFactory::config_offers_model`]: primary/extra/cached/catalog).
///
/// Otherwise the helper returns `None` and the caller keeps the run's default
/// provider. A set-but-stale `provider_id` (provider removed) or one that does
/// not offer the resolved model is logged and silently ignored — the existing
/// fallback behavior for removed providers.
fn custom_agent_provider<'a>(
    settings: &'a concerto_config::ModelSettings,
    agent_configs: &HashMap<AgentId, concerto_config::CustomAgentConfig>,
    role: &AgentId,
    model: &str,
) -> Option<&'a concerto_config::ProviderConfig> {
    let provider_id =
        non_empty(agent_configs.get(role).and_then(|agent| agent.provider_id.as_deref()))?;
    let Some(config) =
        settings.providers.iter().find(|config| ProviderFactory::config_id(config) == provider_id)
    else {
        tracing::warn!(
            role = %role,
            provider_id,
            "custom agent provider is no longer configured; falling back to the default provider"
        );
        return None;
    };
    if !ProviderFactory::config_offers_model(config, model) {
        tracing::warn!(
            role = %role,
            provider_id,
            model,
            "custom agent provider does not offer the resolved model; falling back to the default provider"
        );
        return None;
    }
    Some(config)
}

/// The provider configuration that serves `role`.
///
/// ADR-31 precedence: an explicit, valid `agent_assignments` entry
/// (`assignment_provider_config`, already validated live by the caller) always
/// wins; otherwise a custom agent's live `provider_id` that offers the resolved
/// `model` ([`custom_agent_provider`]); otherwise the run's default provider.
///
/// Model resolution is independent and handled by the caller — this function
/// only picks the serving pipe, so the legacy `assignment override → legacy pin
/// → default` model order is untouched.
fn resolve_role_provider_config<'a>(
    settings: &'a concerto_config::ModelSettings,
    agent_configs: &HashMap<AgentId, concerto_config::CustomAgentConfig>,
    role: &AgentId,
    assignment_provider_config: Option<&'a concerto_config::ProviderConfig>,
    model: &str,
    default_provider_config: &'a concerto_config::ProviderConfig,
) -> &'a concerto_config::ProviderConfig {
    if let Some(provider_config) = assignment_provider_config {
        return provider_config;
    }
    custom_agent_provider(settings, agent_configs, role, model).unwrap_or(default_provider_config)
}

/// Resolve the run's default provider configuration and model using the
/// model-first strategy:
///
/// 1. Explicit provider ID → find that config.
/// 2. Explicit model name → find the provider that offers it.
/// 3. `global_default_model` → find the matching provider.
/// 4. First configured provider.
///
/// Pure config lookup (no IO, no awaits). The caller owns the empty-model
/// recorder-stop/early-return, so the resolved model is returned verbatim
/// even when empty.
fn resolve_default_provider<'a>(
    req: &'a AgentRunRequest,
    settings: &'a concerto_config::ModelSettings,
) -> Result<(&'a concerto_config::ProviderConfig, &'a str), OrchestratorError> {
    let requested_provider_id = non_empty(req.selected_provider_id.as_deref());
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
    Ok((default_provider_config, default_model))
}

/// The coordinator's model.
///
/// The coordinator is hardcoded (maintainer decision 2026-09) and always
/// follows the run's global default model. `_configured_pins` is accepted so
/// the call site documents the pin source the coordinator used to consult, but
/// it is deliberately never read: `model_pins` / `agent_assignments` entries
/// naming `coordinator` are inert (their user data is left on disk untouched).
fn resolve_coordinator_model(
    _configured_pins: &HashMap<AgentId, String>,
    default_model: &str,
) -> String {
    default_model.to_string()
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

/// ADR-35 phase 4 / ADR-58 P2+P3 (Batch 1): assemble the run's role topology,
/// per-agent config map, and resolved blueprint facade.
///
/// Pure data assembly — no IO, no awaits. Returns the roles needing
/// provider/model resolution, the optional blueprint facade, the
/// (facade-staffed) agent-config map, and the tool-calling role set derived
/// from it.
fn build_role_topology(
    multi_agent: &Option<concerto_config::MultiAgentConfig>,
    resolved_blueprint: Option<&concerto_config::ResolvedBlueprint>,
    action_capable: bool,
) -> (
    Vec<AgentId>,
    Option<BlueprintFacade>,
    HashMap<AgentId, concerto_config::CustomAgentConfig>,
    std::collections::HashSet<AgentId>,
) {
    // ADR-35 phase 4: the roles needing provider/model resolution mirror the
    // runtime topology (coordinator + built-ins not disabled + enabled custom
    // agents) instead of a hardcoded role list. The shape follows the intent
    // gate's effective outcome (ADR-55 §7): Execute runs use the full
    // topology, everything else resolves only the coordinator.
    let roles_to_resolve: Vec<AgentId> = if action_capable {
        topology_roles(multi_agent)
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
    let facade = resolved_blueprint.map(BlueprintFacade::new);

    // The tool-calling role set handed to the routing engine mirrors the same
    // topology. Derived before `roles_to_resolve` is consumed by the
    // resolution loop below.
    let mut agent_configs: HashMap<AgentId, concerto_config::CustomAgentConfig> =
        build_agent_config_map(multi_agent);
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
    (roles_to_resolve, facade, agent_configs, tool_calling_roles)
}

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

/// Maximum plan→act→observe cycles for a single agent run. Five was far too
/// low for non-trivial build tasks (plan, write several files, verify, fix),
/// which caused the agent to stop mid-task when it hit the cap with no clear
/// signal — the "agent stops without reason" symptom. 25 leaves enough
/// headroom for multi-file changes while the cycle detector still bounds
/// runaway repetition.
const DEFAULT_MAX_ITERATIONS: u32 = 25;
use concerto_eval::EvalEngine;
use concerto_memory::embedder::{EmbeddingGenerator, ProviderEmbedder};
use concerto_memory::indexer::ProjectIndexer;
use concerto_memory::sync::ChunkSyncService;
use concerto_memory::{decision_store::DecisionStore, task_tree::TaskTreeStore};

/// Transition-only run-stage publisher (ADR-55 §9).
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
    /// Held `.concerto.lock` for the Concerto data root (ADR-11), acquired
    /// when the memory system initialises and kept for the subsystem's
    /// lifetime.
    pub data_dir_lock: Option<Arc<DataDirLock>>,
    /// Phase 6 M3a/M3b: the SAME decisions/task-tree `Arc`s the memory system
    /// wraps, handed to the coordinator for outcome write-back and
    /// run-scoped retrieval. `None` only when the memory system was built by
    /// a legacy path that did not expose them.
    pub decision_store: Option<Arc<DecisionStore>>,
    pub task_tree: Option<Arc<TaskTreeStore>>,
}

/// Phase 6 M3a/M3b: a freshly initialised memory system plus the shared store
/// handles a run needs for outcome write-back (M3b) and run-scoped retrieval
/// (M3a). The handles are the SAME `Arc`s the memory system wraps, so a
/// coordinator write is immediately visible to the system's own reads.
pub struct MemorySystemHandles {
    pub store: Arc<dyn MemoryStore>,
    pub decision_store: Arc<DecisionStore>,
    pub task_tree: Arc<TaskTreeStore>,
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
    /// Process-lifetime WASM plugin manager (desktop-only, plugin liveness).
    /// When `Some`, `load_and_configure_plugins` materialises it on the first
    /// run and reuses it across runs so the Settings UI's revoke and
    /// re-discovery paths act on the same live instances a running agent uses.
    /// When `None` (CLI, tests, headless), a per-run manager is constructed
    /// and dropped after each run as before.
    pub plugins: Option<concerto_plugins::manager::SharedPluginManager>,
}

// NORM S21a: the event/transcript recorder cluster and the no-op audit sink
// live in `runtime_runner/recorders.rs`. This file is loaded via `#[path]` as
// `runtime_runner_impl`, so the submodule needs an explicit `#[path]` too.
#[path = "runtime_runner/recorders.rs"]
mod recorders;
pub(crate) use recorders::*;

/// Open (or create) the SQLite pool for the append-only audit log.
/// Uses a separate connection from the session store so audit writes
/// never block session operations — via `SqliteSessionStore::open_pool`
/// so the same at-rest policy as the store applies: an encrypted
/// `sessions.db` must not silently disable auditing (row 44).
async fn create_audit_pool(
    data_dir: &std::path::Path,
) -> Result<sqlx::SqlitePool, OrchestratorError> {
    std::fs::create_dir_all(data_dir).map_err(|e| {
        OrchestratorError::AgentLoopError(format!("failed to create data directory: {e}"))
    })?;

    let db_path = data_dir.join("sessions.db");
    concerto_sessions::SqliteSessionStore::open_pool(&db_path).await.map_err(|e| {
        OrchestratorError::AgentLoopError(format!("failed to connect to audit DB: {e}"))
    })
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
) -> Result<(Arc<dyn LlmProvider>, Option<String>), OrchestratorError> {
    let creds = CredentialStore::new();
    let build = |provider: &concerto_config::ProviderConfig| {
        let mut provider = provider.clone();
        if let Some(model) = selected_model.filter(|model| !model.trim().is_empty()) {
            provider.model = model.to_string();
        }
        // Carry the resolved config id so callers can consult the matching
        // `[model_profiles.<id>]` overrides (ADR-66 §3 capability override).
        let config_id = ProviderFactory::config_id(&provider);
        ProviderFactory::build(&provider, &creds).map(|built| (built, Some(config_id)))
    };

    // 1. Preferred ID from UI — look up the provider by id
    if let Some(id) = selected.as_deref() {
        if let Some(provider) = plugin_providers.get(id) {
            return Ok((provider.clone(), None));
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
            return Ok((provider.clone(), None));
        }
    }

    // 6. Env‑var fallbacks
    if let Ok(key) = std::env::var("ANTHROPIC_API_KEY") {
        tracing::info!("no provider config; using ANTHROPIC_API_KEY env fallback");
        return Ok((
            Arc::new(concerto_providers::anthropic::AnthropicProvider::new(
                key,
                "claude-sonnet-4-6".to_string(),
                15,
            )),
            None,
        ));
    }
    if let Ok(key) = std::env::var("OPENAI_API_KEY") {
        tracing::info!("no provider config; using OPENAI_API_KEY env fallback");
        return Ok((
            Arc::new(
                concerto_providers::openai::OpenAiProvider::new(key, "gpt-4o".to_string(), 15)
                    // ADR-48 §4: the same usage opt-in as the factory's
                    // `openai` arm — a key-only fallback must report
                    // provider usage too, or the columns stay 0 here.
                    .with_usage_request(
                        concerto_providers::openai::UsageRequest::IncludeStreamUsage,
                    ),
            ),
            None,
        ));
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

// NORM S21b: the memory-bootstrap cluster (`init_memory_system`, the ADR-46
// L1 summarizer, and the ADR-69 link store) lives in
// `runtime_runner/memory_bootstrap.rs`. This file is loaded via `#[path]` as
// `runtime_runner_impl`, so the submodule needs an explicit `#[path]` too.
// `init_memory_system` keeps a `pub` re-export: `runtime_runner_persistent`
// republishes it as `runtime_runner::init_memory_system`, which the desktop
// front end calls; the rest of the cluster is re-exported `pub(crate)` only.
#[path = "runtime_runner/memory_bootstrap.rs"]
mod memory_bootstrap;
pub use memory_bootstrap::init_memory_system;
pub(crate) use memory_bootstrap::*;

// NORM S21c: the plugin/tool-setup cluster (`build_plugin_manager`, the
// plugin discovery/load wrappers, `build_tool_registry`,
// `resolve_provider_and_model`, and the ADR-66 tool-support predicates) lives
// in `runtime_runner/plugin_setup.rs`. This file is loaded via `#[path]` as
// `runtime_runner_impl`, so the submodule needs an explicit `#[path]` too.
// The whole cluster is re-exported `pub(crate)` only: call sites — including
// `memory_bootstrap` and the `runtime_runner_tests` precedence test — stay
// byte-identical through this glob.
#[path = "runtime_runner/plugin_setup.rs"]
mod plugin_setup;
pub(crate) use plugin_setup::*;

// NORM S21d: the ADR-60 D7 run-continuity cluster (`RunContinuity`, the
// windowed whiteboard loaders, the trigger predicate, and the fail-soft
// seeder) lives in `runtime_runner/continuity.rs`. This file is loaded via
// `#[path]` as `runtime_runner_impl`, so the submodule needs an explicit
// `#[path]` too. The whole cluster is re-exported `pub(crate)` only, so the
// run-loop dispatch site stays byte-identical through this glob.
#[path = "runtime_runner/continuity.rs"]
mod continuity;
pub(crate) use continuity::*;

/// Build the run's audit sink and its backing session-DB pool.
///
/// Fail-soft: a missing data directory or unopenable/pathological DB degrades
/// to [`NoopAuditLog`] with a warning rather than failing the run. Shared by
/// the infra-audit sites (MCP/plugin startup) and the policy engine so every
/// writer in one run targets the same `sessions.db`.
async fn build_audit_sink() -> (Arc<dyn AuditLog>, Option<sqlx::SqlitePool>) {
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
    }
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
#[allow(clippy::too_many_arguments)]
async fn setup_policy_and_audit(
    config: &AppConfig,
    project_dir: &Path,
    approval_sink: Arc<dyn ApprovalSink>,
    registry: Arc<ToolRegistry>,
    force_single_agent: bool,
    bus: EventBus,
    intent_auth: Option<Arc<dyn IntentAuthorization>>,
    audit: Arc<dyn AuditLog>,
    gate_pool: Option<sqlx::SqlitePool>,
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
    // ADR-55 §3 (B-3): custom user policy rules replace default_rules()
    // wholesale and may drop the bare `IntentAuthorized` gate rule, which
    // would leave the gate inert — still prompting and auditing but never
    // deciding. Whenever the gate's authorization provider is attached,
    // re-inject the gate rule after the leading deny-class rules so deny-first
    // ordering is preserved.
    if intent_auth.is_some() {
        policy_rules = inject_intent_gate_rule(policy_rules);
    }
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
    // Approval deadline from `[policy] approval_timeout_secs` (default 30s).
    // The deadline rides the policy verdict for config/API compatibility, but
    // the executor never enforces it: approvals park until answered (see
    // `ToolExecutor::request_approval_decision`).
    let approval_timeout = std::time::Duration::from_secs(
        config.policy.as_ref().and_then(|policy| policy.approval_timeout_secs).unwrap_or(30),
    );
    let mut policy_engine = SimplePolicyEngine::new(policy_rules, audit)
        .with_spend_tracker(spend_tracker.clone())
        .with_approval_timeout(approval_timeout)
        .with_shell_security(config.shell_security.clone(), project_dir.to_path_buf())
        .with_protected_config(concerto_config::default_config_path());
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
    // The default `standard` blueprint produces the generic
    // "Review"/"Validate" gate labels (never role ids).
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
/// `envelope` drives the system prompt: Acting runs get the tool-capable Build
/// prompt, ReadOnly runs the Chat prompt. Under full local agency the envelope
/// is always Acting; there is no keyword-selected tool-less path.
#[allow(clippy::too_many_arguments)]
async fn execute_agent_loop(
    req: AgentRunRequest,
    services: &SharedServices,
    provider: Arc<dyn LlmProvider>,
    model: String,
    // Provider-advertised tool-calling capability for `model` (ADR-66 §3
    // level 2 / ADR-75). `Some(false)` engages the §4 fallback driver.
    advertised_tool_support: Option<bool>,
    executor: crate::exec_backend::SharedExecutionBackend,
    memory: Arc<dyn MemoryStore>,
    session_store: Option<Arc<dyn SessionStore>>,
    session_id: Ulid,
    task: AgentTask,
    event_recorder: EventRecorderGuard,
    transcript_recorder: TranscriptRecorderGuard,
    // ADR-55 §2: the run's permission envelope. Under full local
    // agency this is always `Acting`; the parameter is retained so the loop's
    // prompt selection stays explicit rather than implicit.
    envelope: RunEnvelope,
    // Full-local-agency runs are action-capable (see `run_shared_agent`).
    execute_granted: bool,
    stage_tracker: &Arc<Mutex<StageTracker>>,
    // ADR-65 §3: the session-DB pool backing the single-agent loop's
    // tool-evidence writer (`ToolFactContext::new(pool, "single-agent")`).
    // `None` when no sessions DB is available (the writer is a fail-soft
    // no-op). Cloned from the write-gate pool in `run_shared_agent` before
    // the gates consume it.
    fact_pool: Option<sqlx::SqlitePool>,
    // The run-scoped project AGENTS.md context (ADR-70), injected into the
    // single-agent system prompt between the skills section and the
    // environment card via `PromptBuilder::with_project_context`. Refreshed
    // once at run start by `run_shared_agent` (fail-soft).
    project_context: Arc<crate::project_context::ProjectContext>,
) -> Result<AgentOutput, OrchestratorError> {
    // ADR-67 M-01 (audit C-03 gate): the in-run overflow-strategy slot is
    // removed from `AgentLoop`. Context overflow is bounded deterministically
    // by the context engine — `create_session_and_recorder` bounds the active
    // history before the run and `maintain_context_after_run` checkpoints
    // after it. Re-introducing an in-run overflow strategy anywhere in
    // production requires a superseding ADR.

    let undo_manager = Arc::new(Mutex::new(UndoManager::new(&req.project_dir)));
    let eval = EvalEngine::new(&req.project_dir).with_process_executor(Arc::new(
        crate::exec_backend::NativeEvalExecutor {
            backend: executor.clone(),
            session: SessionContext::new(session_id, req.project_dir.clone()),
            orchestrator_authority: true,
        },
    ));
    // Full local agency: the unified loop always gets the tool-capable Build
    // prompt (writes stay policy-gated and approval-sinked). No keyword can
    // select a tool-less path; the model decides tool use from its own output.
    let base_prompt = if envelope.is_acting() {
        concerto_core::types::SYSTEM_PROMPT_BUILD
    } else {
        concerto_core::types::SYSTEM_PROMPT_CHAT
    };
    let prompt_text = base_prompt.to_string();
    let prompt_builder = PromptBuilder::with_skills(prompt_text, Some(services.skills.clone()))
        // Native execution uses explicit argv, independently of compatibility profiles.
        .with_native_shell()
        // Run-scoped project AGENTS.md context (ADR-70): injected between the
        // skills section and the environment card.
        .with_project_context(Some(project_context))
        // ADR-048: `[context].cache_stable_prefix` pins a byte-stable system
        // head and appends the volatile working memory after it. Resolved
        // through the engine's budget policy so the knob's default lives in
        // exactly one place (`ContextBudgetPolicy::from_config`); unset or
        // `false` keeps today's byte-identical assembly.
        .with_cache_stable_prefix(
            crate::context_engine::ContextBudgetPolicy::from_config(
                services.config.context.as_ref(),
            )
            .cache_stable_prefix,
        );

    // ADR-43: one audit record + `info` log per run proving which enabled
    // skill packs (and how many characters) were injected into this loop's
    // system prompt. Content-free (ids and sizes only) and silent when no
    // section is injected.
    services.skills.report_injection(&services.bus, session_id);

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
        Some(concerto_memory::budget::ContextBudgetAllocator::default()),
    )
    .with_retry_policy(retry_policy)
    .with_usage_model(model)
    .with_advertised_tool_support(advertised_tool_support)
    .with_initial_messages(req.conversation_history)
    .with_session_store(session_store)
    .with_tool_facts(fact_pool.map(|pool| {
        crate::tool_facts::ToolFactContext::new(Some(pool), crate::tool_facts::SINGLE_AGENT_FACT_ID)
    }));

    // `task` is moved into the loop below; Ulid is Copy so the task id is
    // captured up front for spend attribution (Phase 3, issue #93).
    let task_id = task.id.0;
    // Run-stage signals (ADR-55 §9): the loop is about to start, so the
    // run is Inspecting the workspace; a mutation-capable run then moves to
    // Execute. Complete is reported only after `run` returns Ok below — an Err
    // or cancellation never advances the stage. Under full local agency the
    // single-agent (forced) loop is action-capable, so `execute_granted` is
    // true; the explicit flag keeps the stage independent of task shaping.
    {
        let mut tracker = stage_tracker.lock().unwrap_or_else(|error| error.into_inner());
        tracker.set(RunStage::Inspect);
        if execute_granted {
            tracker.set(RunStage::Execute);
        }
    }
    let output = match agent.run(task, req.cancel_token.clone()).await {
        Ok(output) => {
            // Full-local-agency runs are action-capable: an empty final message
            // is left as the model produced it (no honesty synthesis, which is
            // reserved for read-only runs that no longer exist as a mode).
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

/// Phase 6 M3a/M3b: the shared decision/task-store handles of the cached
/// memory services — `(None, None)` when memory is disabled or not yet
/// initialised. The coordinator's outcome write-back and run-scoped retrieval
/// are no-ops without them, so an absent handle is never an error.
fn memory_writeback_handles(
    memory: &Mutex<Option<ActiveMemoryServices>>,
) -> (Option<Arc<DecisionStore>>, Option<Arc<TaskTreeStore>>) {
    let lock = memory.lock().unwrap_or_else(|poison| poison.into_inner());
    match lock.as_ref() {
        Some(active) => (active.decision_store.clone(), active.task_tree.clone()),
        None => (None, None),
    }
}

/// Cancel and drop the previous project's memory lifecycle. Called on a
/// project switch so the new project never inherits the previous one's
/// background indexer, chunk-sync service, or store.
fn reset_memory_services(memory: &Mutex<Option<ActiveMemoryServices>>) {
    if let Some(previous) = memory.lock().unwrap_or_else(|poison| poison.into_inner()).take() {
        previous.cancel.cancel();
    }
}

/// Turn the memory-services selection into the store a run uses: a healthy
/// store, or `NullMemoryStore` when memory was disabled/absent or when init
/// FAILED. A run's correctness (plan, scheduling, execute, resume — ADR-65
/// acceptance 9) never depends on vector memory, so an init failure must log
/// and degrade, never abort the run.
fn memory_store_or_disabled(
    selection: Result<Option<Arc<dyn MemoryStore>>, OrchestratorError>,
) -> Arc<dyn MemoryStore> {
    match selection {
        Ok(Some(store)) => store,
        Ok(None) => Arc::new(NullMemoryStore),
        Err(error) => {
            tracing::warn!(
                %error,
                "memory init failed — the run proceeds without memory (vector store absent)"
            );
            Arc::new(NullMemoryStore)
        }
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
    let lock_temp: Arc<Mutex<Option<Arc<DataDirLock>>>> = Arc::new(Mutex::new(None));
    let mem = init_memory_system_with_handles(
        services.bus.clone(),
        &services.config,
        project_dir,
        &reindex_temp,
        &reindex_sync_temp,
        &cancel_temp,
        &lock_temp,
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
    let data_dir_lock = lock_temp.lock().unwrap_or_else(|e| e.into_inner()).take();

    let active = ActiveMemoryServices {
        project_id,
        store: mem.store.clone(),
        reindex,
        reindex_sync,
        cancel,
        data_dir_lock,
        // Phase 6 M3a/M3b: retain the shared handles for the coordinator.
        decision_store: Some(mem.decision_store.clone()),
        task_tree: Some(mem.task_tree.clone()),
    };
    *services.memory.lock().unwrap_or_else(|e| e.into_inner()) = Some(active);
    Ok(Some(mem.store))
}

/// ADR-55 §4: resolve the plan binding a confident Execute
/// auto-Applies — the coordinator decides, no clicks.
///
/// Two sources, in order:
///
/// - **Exact-objective binding** (1d §2 registry, keyed
///   `(session_id, objective_hash)`): a confident Execute whose input hash
///   matches a stored binding auto-Applies it. The binding's plan text is
///   verified against its creation-time artifact hash; a binding that no
///   longer matches (tampered or corrupted storage) is a **loud failure**
///   (`Err`) — never a silent fall-through into a fresh re-decompose of the
///   same objective (2d §3: "artifact_hash verified, loud-fail on drift").
/// - **Session-newest durable binding** (§11/§12 lineage): with no
///   exact-objective hit, the session's newest durable `plan_bindings` row
///   (which may have been planned for an earlier objective) is rehydrated
///   and auto-Applied. This leg stays fail-soft (§11 posture): a missing row,
///   a storage error, or an unverifiable/tampered row returns `Ok(None)` and
///   the run falls through to the generic intent gate — a fresh objective is
///   not a re-decompose of the stored plan.
///
/// `Ok(None)` = no binding to auto-Apply (the unified loop decides what to do).
/// Intent routing is no longer consulted: the interception is keyed purely on
/// a stored binding for **this exact objective** (the in-process registry, or
/// the session-newest durable row when its objective hash matches). A binding
/// for a different objective never intercepts, so safety does not depend on
/// keyword classification.
async fn resolve_auto_apply_binding(
    session_id: Ulid,
    objective_hash: &str,
    session_store: Option<&Arc<dyn SessionStore>>,
    cancel: CancellationToken,
) -> Result<Option<PlanBinding>, OrchestratorError> {
    if let Some(raw) = plan_registry().pending(session_id, objective_hash) {
        if raw.artifact_verifies() {
            return Ok(Some(raw));
        }
        // ADR-55 §4: the approved plan for THIS objective drifted
        // from its artifact hash. Executing anything else would be a silent
        // re-decompose — fail the run loudly instead.
        return Err(OrchestratorError::Unrecoverable {
            message: format!(
                "approved plan binding for objective {objective_hash} drifted from its artifact \
                 hash (plan {}); refusing to silently re-decompose — re-plan explicitly to \
                 replace it (ADR-55 §4)",
                raw.plan_id(),
            ),
        });
    }
    if let Some(store) = session_store {
        // Restart-safe exact-objective leg: the session-newest durable row is
        // rehydrated, but it intercepts ONLY when it is the same objective.
        // A different objective never auto-Applies, so a keyword-free run can
        // never execute an unrelated stored plan. Fail-soft: a missing/
        // unverifiable row falls through to the unified loop.
        if let Some(binding) = rehydrate_durable_binding(store.as_ref(), session_id, cancel).await {
            if binding.objective_hash() == objective_hash {
                tracing::info!(
                    %session_id,
                    plan_id = %binding.plan_id(),
                    "auto-Apply armed from the durable plan binding for this objective"
                );
                return Ok(Some(binding));
            }
        }
    }
    Ok(None)
}

/// W3b (run_shared_agent P6c): perform the ADR-55 §4 auto-Apply intercept for
/// a hash-verified stored plan binding in ONE ordered unit. The revision
/// snapshot (`current_source_revision`) precedes the `record_plan_decision`
/// audit write, which precedes the two-store consume (in-memory registry
/// removal, then the durable `delete_plan_binding`), and the run-scoped grant
/// decision lands last — the same order the interception always ran in.
/// `plan_decision` / `applied_plan` / `approval_time_revision` are mutated in
/// place so the caller's later reads are unchanged. The durable delete is
/// fail-soft (a missing row is a warn, never an abort).
#[allow(clippy::too_many_arguments)]
async fn apply_bound_plan(
    binding: PlanBinding,
    executor: &ToolExecutor,
    session_id: Ulid,
    session_store: Option<&Arc<dyn SessionStore>>,
    project_dir: &std::path::Path,
    cancel: &CancellationToken,
    intent_store: &Arc<IntentGrantStore>,
    plan_decision: &mut Option<PlanDecision>,
    applied_plan: &mut Option<PlanBinding>,
    approval_time_revision: &mut Option<String>,
) {
    // ADR-55 §4: auto-Apply — no dialog. The binding was
    // hash-verified at interception ([`resolve_auto_apply_binding`]);
    // consume it in both stores so a later run cannot re-apply an
    // already-executed plan.
    let objective_hash = binding.objective_hash();
    let current_revision = current_source_revision(project_dir).await;
    *approval_time_revision = current_revision.clone();
    let binding_revision = binding.source_revision().unwrap_or("unknown");
    // The auto decision is audited under the synthetic `intent:plan`
    // identity with plan_id + source revision in the user response
    // (`auto_apply`, ADR-55 §6).
    executor
        .record_plan_decision(
            session_id,
            Ulid::new(),
            binding.plan_id(),
            objective_hash,
            current_revision.as_deref(),
            // ADR-55 §6: the plan-decision seam gains the
            // `auto_apply` variant.
            "auto_apply",
            cancel.clone(),
        )
        .await;
    tracing::info!(
        %session_id,
        plan_id = %binding.plan_id(),
        plan_revision = %binding_revision,
        current_revision = %current_revision.as_deref().unwrap_or("unknown"),
        "auto-Applying the hash-verified stored plan (ADR-55 §4, no dialog)"
    );
    // The decision rides along for ADR-55 §4 (M2) checkpoint
    // suppression below.
    *plan_decision = Some(PlanDecision::Apply);
    // The auto-Apply CONSUMES the stored plan: drop the session's
    // binding in the in-memory registry and in durable storage so a
    // later run cannot re-apply an already-executed plan. A missing
    // durable row is a no-op (fail-soft).
    // ADR-55 §4 (M3, live-fix): capture the binding BEFORE
    // consuming it so the Execute run below can describe the
    // approved plan.
    *applied_plan = Some(binding.clone());
    plan_registry().remove(session_id, objective_hash);
    if let Some(store) = session_store {
        if let Err(error) =
            store.delete_plan_binding(session_id, objective_hash, cancel.clone()).await
        {
            tracing::warn!(%error, "failed to clear durable plan binding after apply");
        }
    }
    let _ = apply_auto_plan_decision(intent_store);
}

/// The authoritative acting-run vehicle.
///
/// Routing is no longer consulted: with full local agency every non-forced run
/// is coordinator-owned, and the coordinator's decision loop engages
/// specialists on need (task requires hands), never by pre-scanning words.
/// `force_single_agent` is the only remaining explicit mode switch.
///
/// Pure so tests can pin the mode without a full run.
pub fn dispatches_to_coordinator(force_single_agent: bool) -> bool {
    !force_single_agent
}

/// ADR-55 §4 (M3, live-fix): an Apply run executes the APPROVED plan,
/// not the approval phrase ("i approve"). The stored, capped plan text is
/// what the user approved; the original ask rides in the transcript.
fn approved_plan_task_description(binding: &PlanBinding) -> String {
    format!(
        "Execute the approved plan (plan {}) for this objective:\n{}",
        binding.plan_id(),
        binding.plan_text()
    )
}

/// ADR-60 D7 (#152): the task description for an Execute run governed by a
/// whiteboard-verified approved plan. The structured artifact — DesignDoc
/// when the planning run produced one, otherwise the hash-verified plan text
/// read back from the log — replaces the rendered-plan prose, and the
/// carry-forward ledger states what already happened under the plan so
/// completed subtasks are not redone and failed commands are not re-run
/// unchanged (#152 acceptance).
fn approved_plan_structured_description(context: &ApprovedPlanContext) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "Execute the approved plan {} (objective {}). The approved structured artifact below \
         governs this run — do not re-derive or re-plan it.\n",
        context.binding.plan_id(),
        context.binding.objective_hash(),
    ));
    match &context.design_doc {
        Some(doc) => {
            out.push_str("\n<approved-design-doc>\n");
            for goal in &doc.goals {
                out.push_str(&format!("Goal: {goal}\n"));
            }
            for constraint in &doc.constraints {
                out.push_str(&format!("Constraint: {constraint}\n"));
            }
            for file in &doc.proposed_files {
                out.push_str(&format!("Proposed file: {file}\n"));
            }
            if !doc.interface_sketch.trim().is_empty() {
                out.push_str(&format!("Interface: {}\n", doc.interface_sketch.trim()));
            }
            for risk in &doc.risks {
                out.push_str(&format!("Risk: {risk}\n"));
            }
            out.push_str("</approved-design-doc>\n");
        }
        // No structured doc exists (single-agent plans): the verified plan
        // text IS the persisted artifact — loaded from the log and checked
        // against its creation-time hash, never replayed from the transcript.
        None => {
            out.push_str("\n<approved-plan-artifact>\n");
            out.push_str(context.binding.plan_text());
            out.push_str("\n</approved-plan-artifact>\n");
        }
    }
    out.push_str(&render_plan_ledger_section(&context.ledger));
    out
}

/// The carry-forward ledger block shared by the approved-plan Execute
/// description and the run-continuity section below: completed subtasks are
/// not redone, files touched are known, and failed commands carry their
/// failure reasons so they are never re-run unchanged (#152 acceptance).
fn render_plan_ledger_section(ledger: &PlanLedger) -> String {
    let mut out = String::new();
    out.push_str("\n<approved-plan-ledger>\n");
    if ledger.completed_subtasks.is_empty()
        && ledger.files_touched.is_empty()
        && ledger.failed_commands.is_empty()
    {
        out.push_str("No prior execution under this plan.\n");
    } else {
        if !ledger.completed_subtasks.is_empty() {
            out.push_str("Completed subtasks (do not redo):\n");
            for entry in &ledger.completed_subtasks {
                out.push_str(&format!("- {entry}\n"));
            }
        }
        if !ledger.files_touched.is_empty() {
            out.push_str("Files already touched under this plan:\n");
            for entry in &ledger.files_touched {
                out.push_str(&format!("- {entry}\n"));
            }
        }
        if !ledger.failed_commands.is_empty() {
            out.push_str(
                "Failed commands (address the recorded failure reason; never re-run unchanged):\n",
            );
            for entry in &ledger.failed_commands {
                out.push_str(&format!("- {entry}\n"));
            }
        }
    }
    out.push_str("</approved-plan-ledger>");
    out
}

/// Build the run's task from the gate and plan-binding state (ADR-55 §4,
/// M3 live-fix).
///
/// An Apply run must describe the APPROVED plan rather than the approval
/// phrase that armed the dialog ("i approve"); without this the coordinator's
/// subtasks were literally built from the approval phrase and the Coder
/// produced nothing. ADR-60 D7: when the whiteboard state was verified
/// (`approved`), the structured artifact + ledger description replaces the
/// legacy rendered-prose description entirely.
///
/// Issue #145: the caller passes the run's [`TaskExecutionMode`]. An Apply is
/// real work by construction, so the `apply_plan` arm always builds an
/// [`TaskExecutionMode::ActionRequired`] task regardless of the passed mode;
/// every other shape carries the caller's mode verbatim (CoordinatorDecides
/// for ordinary coordinator turns, AnswerOnly/ActionRequired for the
/// single-agent path).
#[allow(clippy::too_many_arguments)]
fn build_run_task(
    session_id: Ulid,
    execution_mode: TaskExecutionMode,
    apply_plan: bool,
    applied_plan: Option<&PlanBinding>,
    approved: Option<&ApprovedPlanContext>,
    input: &str,
) -> AgentTask {
    if apply_plan {
        if let Some(context) = approved {
            AgentTask::new_action_required(
                session_id,
                approved_plan_structured_description(context),
            )
        } else if let Some(binding) = applied_plan {
            AgentTask::new_action_required(session_id, approved_plan_task_description(binding))
        } else {
            // Defensive: apply without a captured binding (should be
            // impossible — the Apply arm captures before consuming).
            AgentTask::new_action_required(session_id, input.to_owned())
        }
    } else {
        AgentTask::new_with_execution_mode(session_id, input.to_owned(), execution_mode)
    }
}

/// ADR-60 D7 gating (issue #19 decoupling): the whiteboard plan-binding path
/// is gated solely by the `plan_binding_source` rollout switch (default
/// `whiteboard`) — it no longer rides `[orchestration] supervisor_enabled`,
/// which now governs only the supervised concurrency runtime. `legacy` keeps
/// the exact pre-D7 prose behavior. A config without any multi-agent section
/// (`None`) defaults to `Whiteboard` (continuity on) — fresh installs get the
/// fix, `legacy` must be opted into explicitly.
fn d7_whiteboard_enabled(multi_agent: Option<&concerto_config::MultiAgentConfig>) -> bool {
    multi_agent
        .map(|config| config.plan_binding_source == concerto_config::PlanBindingSource::Whiteboard)
        .unwrap_or(true)
}

/// ADR-60 Deferred 3 (issue #19 decoupling): review-cycle resumability stays a
/// supervised-runtime feature — gated by `[orchestration] supervisor_enabled`
/// alone and deliberately independent of the D7 `plan_binding_source` switch,
/// which now governs only Plan→Execute state sourcing.
fn supervisor_review_enabled(multi_agent: Option<&concerto_config::MultiAgentConfig>) -> bool {
    multi_agent.is_some_and(|config| config.supervisor_enabled)
}

/// ADR-60 D7 (#152): append the content-addressed `plan-approved` whiteboard
/// event for a freshly bound plan. Ordering is deliberate: the LOG commits
/// first (source of truth) and the `plan_bindings` durable mirror follows —
/// a crash in between leaves the log ahead of the projection, which an
/// Execute read resolves by degrading to the legacy prose path with a warn,
/// never by reading an unattested artifact.
///
/// Fail-soft like every persistence step on this path: a missing pool or an
/// append error warns and returns; the just-completed Plan run is never
/// failed by continuity bookkeeping.
async fn append_plan_binding_event(
    pool: Option<&sqlx::SqlitePool>,
    session_id: Ulid,
    binding: &PlanBinding,
    design_doc: Option<&DesignDoc>,
    multi_agent: Option<&concerto_config::MultiAgentConfig>,
) {
    if !d7_whiteboard_enabled(multi_agent) {
        return;
    }
    let Some(pool) = pool else {
        // Same documented degradation as the write gate without a session DB:
        // observable, never silent.
        tracing::warn!(
            plan_id = %binding.plan_id(),
            "no session DB pool — plan-approved whiteboard event not recorded; \
             Execute will use the legacy prose path"
        );
        return;
    };
    match append_plan_approved_event(pool, session_id, binding, design_doc).await {
        Ok(stored) => tracing::info!(
            plan_id = %binding.plan_id(),
            gate_seq = stored.gate_seq,
            "recorded plan-approved whiteboard event (ADR-60 D7)"
        ),
        Err(error) => tracing::warn!(
            %error,
            plan_id = %binding.plan_id(),
            "failed to append the plan-approved whiteboard event; Execute falls \
             back to the legacy prose path"
        ),
    }
}

/// W2d: the startup tooling/provider stack for a shared run (P1-P3b of
/// [`run_shared_agent`]): the tool registry plus audit sink, plugin and MCP
/// tool loading, and provider/model resolution. Each field feeds the
/// policy/audit executor the caller builds next (P4) or a later phase. Every
/// step is a linear await, so building the whole stack in one call preserves
/// their original order.
struct SharedTooling {
    registry: Arc<ToolRegistry>,
    audit: Arc<dyn AuditLog>,
    gate_pool: Option<sqlx::SqlitePool>,
    plugin_context: Option<concerto_plugins::host::PluginHostContext>,
    provider: Arc<dyn LlmProvider>,
    model: String,
    provider_config_id: Option<String>,
}

/// W2d (P1-P3b): build the tool registry, audit sink, plugin/MCP tools, and
/// the resolved provider/model in their original order. Returns the same
/// values the inlined phases produced, so the caller's subsequent
/// `setup_policy_and_audit` (P4) and session creation are unchanged. The
/// plugin-backed provider map is consumed by resolution here and not returned.
async fn setup_shared_tooling(
    req: &AgentRunRequest,
    services: &SharedServices,
) -> Result<SharedTooling, OrchestratorError> {
    // 1. Build tool registry (filesystem, shell, git, LSP tools)
    let mut registry = build_tool_registry(&req.project_dir, &services.vfs, &services.config);

    // 1a. Audit sink built up front so MCP + plugin infra failures observed
    // during startup (spawn/handshake, load/init, capability denial) are
    // audited through the same sink the policy engine uses. Fail-soft: a
    // missing data directory or DB just disables audit logging.
    let (audit, gate_pool): (Arc<dyn AuditLog>, Option<sqlx::SqlitePool>) =
        build_audit_sink().await;

    // 2. Load WASM plugins and collect plugin-backed providers. The desktop
    // passes a retained manager handle (plugin liveness); CLI/tests pass none
    // and get the per-run behaviour.
    let (plugin_providers, plugin_context) = load_and_configure_plugins(
        &services.config,
        &req.project_dir,
        &mut registry,
        services.plugins.as_ref(),
        &services.bus,
        Some(audit.clone()),
    )
    .await;

    // 3. MCP servers (ADR-43): bridge namespaced `mcp:<server>:<tool>` tools.
    // Runs after plugin tools so MCP can never clobber them; a failed or
    // duplicate server is marked `Failed` and never blocks startup. Tools of
    // servers connected on a previous run are re-bridged into this run's
    // fresh registry. Registration is fail-soft: per-server errors are logged
    // by the manager and only a config-level defect (e.g. duplicate server id
    // slipping past validation) surfaces here. The infra audit sink is
    // attached first so startup failures are recorded.
    services.mcp.set_audit_log(audit.clone());
    if let Err(error) = services.mcp.register_tools(&mut registry).await {
        tracing::warn!(%error, "mcp registration failed; continuing without mcp tools");
    }
    let registry = Arc::new(registry);

    // 3. Resolve provider and final effective model
    // Sessionless setup abort (item: setup aborts): provider resolution, the
    // policy/audit executor construction below, and session creation all run
    // BEFORE a session id exists, so there is no audit sink to record to — the
    // abort is a coordinator-error/intervention class with nothing to write.
    // Every post-session abort below records a Decision row.
    let (provider, model, provider_config_id) = resolve_provider_and_model(
        &services.config,
        req.selected_provider_id.clone(),
        req.selected_model.clone(),
        &plugin_providers,
    )?;

    Ok(SharedTooling {
        registry,
        audit,
        gate_pool,
        plugin_context,
        provider,
        model,
        provider_config_id,
    })
}

/// W2d (P5): resolve the run's memory store — the cached project store, a
/// freshly initialised one, or the null store when memory is disabled or
/// initialisation fails (ADR-65 acceptance 9: memory is optional for a run's
/// correctness). Single await, preserved verbatim.
async fn resolve_run_memory(
    services: &SharedServices,
    project_dir: &Path,
    memory_enabled: bool,
) -> Arc<dyn MemoryStore> {
    memory_store_or_disabled(
        select_or_init_memory_services(services, project_dir, memory_enabled).await,
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
    // W2d: P1-P3b (registry + audit sink, plugin/MCP tool loading, and
    // provider/model resolution) in one ordered call. The returned stack feeds
    // the policy/audit executor built next.
    let SharedTooling {
        registry,
        audit,
        gate_pool,
        plugin_context,
        provider,
        model,
        provider_config_id,
    } = setup_shared_tooling(&req, &services).await?;

    // 4. Policy engine, audit log, spend tracker, tool executor.
    //
    // Full local agency: the run-scoped grant store is created fresh per call
    // and never derived from routing. Grants are per-run and non-durable
    // (ADR-55 §5): a fresh store per call means they can never cross sessions.
    // Deny-class rules and the approval sink remain the boundaries.
    let store = Arc::new(IntentGrantStore::new());
    let auth = Arc::new(SessionIntentAuth::new(store.clone()));
    let intent_auth = Some(auth.clone() as Arc<dyn IntentAuthorization>);

    let (executor, spend_tracker, intent_policy, gate_log_pool) = setup_policy_and_audit(
        &services.config,
        &req.project_dir,
        services.approval_sink.clone(),
        registry.clone(),
        req.force_single_agent,
        services.bus.clone(),
        intent_auth,
        audit,
        gate_pool,
    )
    .await?;

    // 5. Memory – reuse cached store or initialise a new one, scoped to
    // project. A project switch cancels and drops the previous project's
    // lifecycle so its store/indexer/sync services are never reused (audit
    // G1); `None` means memory is disabled and `NullMemoryStore` is used.
    // ADR-65 acceptance 9: memory is OPTIONAL for a run's correctness — an
    // init failure (missing/unopenable DB, failed migration) degrades to the
    // null store with a warn instead of aborting the run.
    let memory: Arc<dyn MemoryStore> =
        resolve_run_memory(&services, &req.project_dir, req.memory_enabled).await;

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

    if let Some(context) = plugin_context {
        if let Ok(mut context) = context.write() {
            *context = Some(concerto_plugins::host::PluginExecutionContext {
                executor: Arc::downgrade(&executor),
                session: SessionContext::new(session_id, req.project_dir.clone()),
            });
        }
    }

    // Record the user's prompt in the durable transcript up front (ADR-36 §4).
    // Text-only mode previously persisted no user message at all; the
    // transcript now carries the prompt for both modes.
    transcript_recorder.record_user_message(req.input.clone()).await;

    // 6b. Full local agency (unified loop). Intent routing is no longer
    // control flow: every message enters ONE unified loop that may act, with
    // the orchestrator authority (landed) bypassing intent restrictions while
    // deny-class rules still run first and Consequential actions still reach
    // the approval sink. Grants remain per-run and non-durable (ADR-55 §5).
    let plan_objective_hash = blake3::hash(req.input.as_bytes()).to_hex().to_string();

    // Carry forward previous session spend so the cap looks at cumulative
    // cost.
    let session_manager = services.session_manager.clone();
    if let Some(ref session_manager) = session_manager {
        if let Ok(Some(session)) =
            session_manager.load_session(session_id, req.cancel_token.clone()).await
        {
            spend_tracker.record(session.total_cost_usd);
        }
    }

    // ADR-55 §4 (plan→Execute auto-Apply): a stored plan binding for
    // THIS exact objective executes the persisted plan outright — hash-verified,
    // loud-fail on drift, no `approve the plan` click and no keyword gate. A
    // binding for a different objective never intercepts. See
    // [`resolve_auto_apply_binding`].
    let bound = resolve_auto_apply_binding(
        session_id,
        &plan_objective_hash,
        session_store.as_ref(),
        req.cancel_token.clone(),
    )
    .await?;

    // Decide how the run may proceed under full local agency. A stored plan
    // binding for THIS exact objective auto-Applies (no dialog, no keyword);
    // otherwise the unified loop runs with full local agency — no route-derived
    // grant and no read-only-from-routing. The orchestrator authority (landed)
    // bypasses intent restrictions while deny-class rules run first and
    // Consequential actions still reach the approval sink. The auto-Apply
    // additionally feeds the ADR-55 §4 (M2) checkpoint suppression below.
    let mut plan_decision: Option<PlanDecision> = None;
    // ADR-55 §4 (M3, live-fix): the auto-Apply consumes the stored
    // binding below, so capture it BEFORE that consumption — the Execute run's
    // task must be built from the approved plan text, which is only available
    // while the binding still exists.
    let mut applied_plan: Option<PlanBinding> = None;
    // ADR-60 D7: the source revision snapshot the auto-Apply decision was
    // made at. The divergence guard below fails loudly when the tree moves
    // AFTER this snapshot — a change between decision and dispatch is silent
    // divergence, never something a user saw.
    let mut approval_time_revision: Option<String> = None;
    if let Some(binding) = bound {
        apply_bound_plan(
            binding,
            executor.as_ref(),
            session_id,
            session_store.as_ref(),
            &req.project_dir,
            &req.cancel_token,
            &store,
            &mut plan_decision,
            &mut applied_plan,
            &mut approval_time_revision,
        )
        .await;
    }

    // ADR-55 §4 (M2): an Apply decision authorizes the STORED plan for
    // this objective — the run must execute that plan, never silently resume
    // a stale partial-graph checkpoint from an earlier Execute of the same
    // objective (its input hash would otherwise match the implicit-resume
    // check below).
    let apply_plan = matches!(plan_decision, Some(PlanDecision::Apply));

    // ─── ADR-60 D7 (#152): whiteboard rehydration for the approved-plan
    // Execute. Gated solely by the `plan_binding_source` rollout switch
    // (default `whiteboard`); it no longer rides the supervisor opt-in
    // (issue #19 decoupling). The verified structured
    // artifact + carry-forward ledger replace the rendered-plan prose; any
    // divergence from what the user approved is a LOUD failure — silent
    // re-decompose is forbidden.
    let mut approved_context: Option<ApprovedPlanContext> = None;
    if apply_plan && d7_whiteboard_enabled(services.config.multi_agent.as_ref()) {
        match (applied_plan.as_ref(), gate_log_pool.as_ref()) {
            (Some(binding), Some(pool)) => match load_approved_plan(pool, binding).await {
                Ok(Some(context)) => {
                    // Divergence guard, revision axis: the tree may not move
                    // between the auto-Apply decision snapshot and dispatch.
                    // Unverifiable revisions (git absent) degrade with a warn
                    // — the artifact hash above still guards content
                    // integrity.
                    let now_revision = current_source_revision(&req.project_dir).await;
                    match (&now_revision, &approval_time_revision) {
                        (Some(now), Some(approved_at)) if now != approved_at => {
                            // Pre-coordinator abort (item: setup aborts): the
                            // run cannot proceed, so it is a coordinator-error
                            // class — but a session exists, so the abort is
                            // recorded as a Decision row before it returns.
                            crate::bypass_decision::record_decision_row(
                                Some(executor.as_ref()),
                                gate_log_pool.as_ref(),
                                session_id,
                                "plan-diverged-after-approval",
                                &format!(
                                    "approved plan {} diverged: source revision moved from \
                                     {approved_at} to {now}",
                                    binding.plan_id(),
                                ),
                            )
                            .await;
                            return Err(OrchestratorError::Unrecoverable {
                                message: format!(
                                    "approved plan {} diverged from its approval: source \
                                     revision moved from {approved_at} to {now} after the \
                                     Apply decision — an explicit re-plan is required \
                                     (ADR-60 D7 forbids silent divergence)",
                                    binding.plan_id(),
                                ),
                            });
                        }
                        (None, _) | (_, None) => tracing::warn!(
                            plan_id = %binding.plan_id(),
                            "source revision could not be resolved on both sides of the \
                             approval; proceeding with artifact-hash verification only"
                        ),
                        _ => {}
                    }
                    tracing::info!(
                        plan_id = %binding.plan_id(),
                        design_doc = context.design_doc.is_some(),
                        completed_subtasks = context.ledger.completed_subtasks.len(),
                        files_touched = context.ledger.files_touched.len(),
                        failed_commands = context.ledger.failed_commands.len(),
                        "rehydrated approved plan from the whiteboard (ADR-60 D7)"
                    );
                    approved_context = Some(context);
                }
                Ok(None) => tracing::warn!(
                    plan_id = %binding.plan_id(),
                    "no plan-approved whiteboard events for this binding (pre-D7 plan?) — \
                     Execute falls back to the legacy prose path"
                ),
                Err(divergence) => {
                    crate::bypass_decision::record_decision_row(
                        Some(executor.as_ref()),
                        gate_log_pool.as_ref(),
                        session_id,
                        "plan-rehydration-failed",
                        &divergence,
                    )
                    .await;
                    return Err(OrchestratorError::Unrecoverable { message: divergence });
                }
            },
            (Some(_), None) => tracing::warn!(
                "no session DB pool for the approved-plan lookup — Execute falls back to the \
                 legacy prose path"
            ),
            // Defensive: an Apply without a captured binding is handled by
            // `build_run_task`'s fallback below.
            (None, _) => {}
        }
    }

    // Full local agency: the unified loop may act. No route-derived
    // read-only flip — deny-class policy rules and the approval sink are the
    // only gates, and orchestrator authority (landed) bypasses the intent
    // restrictions that remain.
    auth.set_read_only(false);
    // Full local agency: every run is action-capable. Deny-class rules and the
    // approval sink enforce the boundaries, never a keyword.
    let envelope = RunEnvelope::Acting;

    // Intent routing no longer writes audit rows: the `intent_router` grant
    // rows were the routed-authority decision, and that decision is gone.
    // Action rows (`record_plan_decision`, executor action rows) and the
    // coordinator's own decision rows remain the audit trail.

    // The spend carry-forward is recorded before the gate above;
    // `session_manager` still gates the checkpoint/resume block.
    // ADR-65 §7: the checkpoint row's own `updated_at` (backfill hint for
    // pre-§7 v3 checkpoints) and the checkpoint's whiteboard cursor (anchors
    // the run-continuity read at the cursor — the resumed run's evidence view
    // is the log AFTER it, never a replay of pre-cursor prose). Declared
    // here so both the checkpoint block and the continuity seed see them.
    let mut resume_updated_at_ms: Option<i64> = None;
    let mut resume_cursor: Option<u64> = None;
    if let Some(ref session_manager) = session_manager {
        if !req.force_single_agent {
            let resume_requested = is_resume_request(&req.input);

            // ADR-55 §4 (M2): an Apply executes the APPROVED plan for
            // this objective. A checkpoint left over from a previous partial
            // Execute of the same objective would match the input hash and
            // silently resume the old partial graph below — the approved
            // plan governs instead, so suppress resume entirely and clear
            // the stale checkpoint.
            if apply_plan {
                req.resume_checkpoint_json = None;
                suppress_stale_checkpoint_for_apply(
                    session_manager,
                    session_id,
                    Some(executor.as_ref()),
                    gate_log_pool.as_ref(),
                )
                .await;
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
                    .map(|record| {
                        // ADR-65 §7: the row's own updated_at backs the
                        // additive v4 backfill for pre-§7 (v3) checkpoints.
                        resume_updated_at_ms =
                            i64::try_from(record.updated_at.unix_timestamp_nanos() / 1_000_000)
                                .ok();
                        let cursor =
                            crate::checkpoint::GraphCheckpoint::from_json(&record.state_json)
                                .ok()
                                .and_then(|checkpoint| checkpoint.whiteboard_cursor_gate_seq);
                        if cursor.is_some() {
                            resume_cursor = cursor;
                        }
                        record.state_json
                    });
            }

            if let Some(checkpoint_json) = req.resume_checkpoint_json.as_ref() {
                // Use the version-aware loader so legacy v2 checkpoints are
                // migrated in-memory and unknown future schema versions are
                // rejected cleanly (C-05).
                match crate::checkpoint::GraphCheckpoint::from_json(checkpoint_json) {
                    Ok(checkpoint) if resume_requested => {
                        // Explicit "continue" / "resume" — validate scope
                        // before trusting the checkpoint (Finding 2 / #65).
                        // ADR-60 D7 (interrupt-safe resume): the scope is the
                        // checkpoint's OWN project-id definition — the row was
                        // written from `SessionContext::project_id`
                        // (`ProjectId::resolve`: git-remote hash or
                        // canonicalized-path hash). Validating against the
                        // unrelated 16-char path hash rejected every real row
                        // with "belongs to a different project" and then
                        // cleared it — the only resumable state a graceful
                        // interrupt had produced never survived a resume.
                        let project_id_str = resume_scope_project_id(&req.project_dir);
                        if let Err(reason) = checkpoint.validate_scope(session_id, &project_id_str)
                        {
                            tracing::warn!(
                                %reason,
                                "discarding checkpoint that failed scope validation on resume"
                            );
                            // Supremacy invariant: a cleared checkpoint is a
                            // Coordinator Decision, not a silent drop. Record
                            // BEFORE the clear so the trail survives even if
                            // the clear fails.
                            crate::bypass_decision::record_decision_row(
                                Some(executor.as_ref()),
                                gate_log_pool.as_ref(),
                                session_id,
                                "checkpoint-cleared-scope",
                                &format!("resume checkpoint failed scope validation: {reason}"),
                            )
                            .await;
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
                        } else {
                            // The checkpoint governs this run: its cursor
                            // anchors the §7 evidence view.
                            resume_cursor = checkpoint.whiteboard_cursor_gate_seq;
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
                            crate::bypass_decision::record_decision_row(
                                Some(executor.as_ref()),
                                gate_log_pool.as_ref(),
                                session_id,
                                "checkpoint-cleared-new-objective",
                                "stale checkpoint superseded by a new objective (input hash mismatch)",
                            )
                            .await;
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
                        crate::bypass_decision::record_decision_row(
                            Some(executor.as_ref()),
                            gate_log_pool.as_ref(),
                            session_id,
                            "checkpoint-cleared-malformed",
                            &format!("malformed orchestration checkpoint discarded: {error}"),
                        )
                        .await;
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

    // Task shape (issue #145): separate the *capability* to act from the
    // *requirement* to act. Every non-AnswerOnly run stays action-capable (the
    // capability gate, full topology resolution, and the supervised path below
    // all key off `action_capable`), but the task's execution mode decides
    // whether a specialist dispatch is mandatory:
    // - an approved-plan Apply is real work → ActionRequired;
    // - a checkpoint-governed resume continues prior (possibly action-required)
    //   work → ActionRequired, so a stalled action-required run cannot resume
    //   into a prose-exempt mode;
    // - a forced single-agent run keeps the action-capable ActionRequired loop
    //   (unchanged);
    // - every other coordinator-owned turn → CoordinatorDecides: the
    //   coordinator may answer directly or delegate. This is the default for
    //   ordinary conversation and informational requests; no word router
    //   classifies them (ADR-71: the coordinator is the sole decision-maker).
    let execution_mode =
        if apply_plan || req.resume_checkpoint_json.is_some() || req.force_single_agent {
            TaskExecutionMode::ACTION_REQUIRED
        } else {
            TaskExecutionMode::CoordinatorDecides
        };
    // Capability (not requirement): true for ActionRequired and
    // CoordinatorDecides, false only for AnswerOnly. Mutation boundaries remain
    // enforced by deny-class policy rules and the approval sink.
    let action_capable = execution_mode.permits_delegation();

    // ADR-66 §2(a) selection gate with the §4 fallback carve-out (2026-09-08
    // correction): a tool-requiring run is refused BEFORE any agent
    // spend only when no tool path exists at all. A model whose ONLY gap is
    // native tool declarations proceeds — the automatic ADR-66 §4
    // text-fallback driver covers it with labeled turns (events +
    // `tool_driver` audit rows), and the driver's requests carry no wire
    // tool declarations so the Responses-path request-build seam never
    // fires on a fallback-driven turn. Refusal is reserved for providers the
    // fallback cannot cover: plugin-backed providers (decision (a) — no tool
    // ops in the protocol, excluded from the fallback), audited
    // (`capability_gate` / Refused) and naming provider, model, and the
    // missing capability. The resolution follows the ADR-66 §3 precedence:
    // explicit config override first, then the built-in family table, then
    // the provider default.
    if action_capable {
        let override_flag = tool_support_override(&services.config, provider_config_id.as_deref());
        // Provider-advertised capability (level 2) is threaded here so a model
        // that advertises its absence is not forced into native tools when a
        // fallback exists (ADR-75).
        let advertised_flag =
            advertised_tool_support(&services.config, provider_config_id.as_deref(), &model);
        if let Err(refusal) = concerto_providers::capability::require_tool_support_with_fallback(
            provider.provider_name(),
            &model,
            override_flag,
            advertised_flag,
        ) {
            let ProviderError::CapabilityRefused {
                provider: refused_provider,
                model: refused_model,
                capability,
            } = &refusal
            else {
                unreachable!("require_tool_support only fails with CapabilityRefused");
            };
            tracing::error!(
                provider = %refused_provider,
                model = %refused_model,
                capability = %capability,
                "tool-requiring task resolved onto a model without tool support (ADR-66)"
            );
            executor
                .record_capability_refusal(
                    session_id,
                    Ulid::new(),
                    refused_provider,
                    refused_model,
                    capability,
                    "selection",
                    req.cancel_token.clone(),
                )
                .await;
            // Pre-coordinator abort (item: setup aborts): a session exists, so
            // the refusal also records a Decision-shaped row (the capability
            // refusal row above stays as the ADR-66 record).
            crate::bypass_decision::record_decision_row(
                Some(executor.as_ref()),
                gate_log_pool.as_ref(),
                session_id,
                "provider-capability-refused",
                &format!(
                    "model {refused_model} on {refused_provider} lacks capability {capability}"
                ),
            )
            .await;
            return Err(OrchestratorError::Provider(refusal));
        }
    }
    // ADR-55 §4 (M3, live-fix): an Apply run executes the APPROVED plan,
    // not the approval phrase. `req.input` is still recorded in the transcript
    // and audit; only the task the agents execute is replaced. ADR-60 D7: a
    // whiteboard-verified approved plan swaps in the structured artifact +
    // ledger description instead of the rendered prose.
    let mut task = build_run_task(
        session_id,
        execution_mode,
        apply_plan,
        applied_plan.as_ref(),
        approved_context.as_ref(),
        &req.input,
    );

    // Run-history audit: a checkpoint row governs this run, so record the
    // resume intent BEFORE any restore or dispatch work — the history shows a
    // deliberate resume (explicit `continue`/`resume` input OR an implicit
    // objective match after a restart), not a fresh run, even when the resume
    // later fails, replans, or is cleared. The checkpoint block above has
    // already finalized the row (cleared/malformed ⇒ `None`), so `is_some()`
    // is the decision. Fail-soft like every other publish.
    if req.resume_checkpoint_json.is_some() {
        let run_id = crate::coordinator::CoordinatorAgent::checkpoint_run_id_hint(
            req.resume_checkpoint_json.as_deref(),
        );
        let _ = services.bus.publish_for_session(
            session_id,
            task.id.0,
            EventKind::ResumeRequested { run_id, session_id, message: req.input.clone() },
        );
    }

    // ─── ADR-60 D7 run-continuity: a `continue`/resume run with no approved
    // plan binding still rehydrates the session's whiteboard ledger — files
    // touched, failed commands, and the session's last approved artifact —
    // so resuming after a failed Execute (or in a reopened project) starts
    // from what the ledger already knows instead of restarting blank and
    // wastefully re-deriving it. The approved-plan path above keeps its own
    // verified rehydration; this seeds only when that path did not fire.
    // Fail-soft: no pool, an empty log, or a read error leaves the task
    // untouched — continuity bookkeeping never fails the run.
    //
    // ADR-60 D7 (interrupt-safe resume, 2026-09-05): when the run is HEADLESS
    // (no checkpoint row survived the checkpoint block above), the same
    // verified payload also seeds the DISPATCH cursor — the resumed run
    // schedules from the evidence chain (verified design + research done ⇒
    // the coder) instead of re-entering design.
    let mut headless_resume: Option<HeadlessResumeSeed> = None;
    if run_continuity_applies(apply_plan, &req.input, services.config.multi_agent.as_ref()) {
        if let Some(pool) = gate_log_pool.as_ref() {
            headless_resume = seed_run_continuity(
                &mut task,
                pool,
                session_id,
                resume_cursor,
                req.resume_checkpoint_json.is_none(),
            )
            .await;
        }
    }

    // Run-stage tracking (ADR-55 §9). Created here, after routing, so
    // the stage chip always starts from Understand; threaded down into the
    // single- and multi-agent paths below, which report the later stages.
    let stage_tracker =
        Arc::new(Mutex::new(StageTracker::new(services.bus.clone(), session_id, task.id)));
    stage_tracker.lock().unwrap_or_else(|error| error.into_inner()).set(RunStage::Understand);

    // 6c. Project AGENTS.md context (ADR-70). Run-scoped like the session: it
    // needs THIS run's project dir, so one instance is built per run and
    // refreshed once at startup. The same handle then feeds both prompt
    // paths — the single-agent PromptBuilder and the coordinator dispatch
    // assembly. Fail-soft: a refresh error logs at warn and the run proceeds
    // without the section — an unreadable AGENTS.md can never fail an agent
    // loop (ADR-70).
    let project_context = Arc::new(crate::project_context::ProjectContext::from_config(
        services.config.project_context.as_ref(),
        &req.project_dir,
    ));
    if let Err(error) = project_context.refresh() {
        tracing::warn!(
            %error,
            "project AGENTS.md context refresh failed at run start; prompts proceed \
             without the project-context section (ADR-70)"
        );
    }

    // 7. Multi-agent dispatch. With full local agency the coordinator owns
    // every non-forced run; it engages specialists on need inside its decision
    // loop — never by pre-scanning the user's words. `force_single_agent` is
    // the only explicit mode switch.
    if dispatches_to_coordinator(req.force_single_agent) {
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
            action_capable,
            plan_objective_hash,
            approved_context.as_ref(),
            &stage_tracker,
            intent_policy.clone(),
            gate_log_pool.clone(),
            resume_updated_at_ms,
            headless_resume,
            project_context.clone(),
        )
        .await;
    }

    // 8. Single-agent execution (explicit `force_single_agent` mode).
    // ADR-65 §3: this pool clone backs the single-agent loop's tool-evidence
    // writer (recorded via `.with_tool_facts` in `execute_agent_loop`).
    let d7_event_pool = gate_log_pool.clone();
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
    let single_agent_advertised =
        advertised_tool_support(&services.config, provider_config_id.as_deref(), &model);
    let output = execute_agent_loop(
        req,
        &services,
        provider,
        model,
        single_agent_advertised,
        single_agent_executor,
        memory,
        session_store.clone(),
        session_id,
        task,
        event_recorder,
        transcript_recorder,
        envelope,
        action_capable,
        &stage_tracker,
        d7_event_pool.clone(),
        project_context,
    )
    .await?;

    Ok(output)
}

/// Run the multi-agent (coordinator) path: resolve role-specific providers,
/// or launch a full multi-agent `CoordinatorAgent` with collaboration rules.
///
/// ADR-55 §1: this path receives every non-forced run. Under full
/// local agency the coordinator's decision loop engages specialists on need,
/// never by pre-scanning the user's words; the run is action-capable
/// (`action_capable`), while whether a dispatch is *mandatory* is carried by
/// the task's own `execution_mode` (issue #145).
///
/// `plan_objective_hash` keys the approved-plan binding resolution.
///
/// ADR-60 D7 (#152): `approved_plan` carries the whiteboard-verified state of
/// an approved-plan Execute — it suppresses the conversation-history prose
/// injection (the transcript contains the rendered plan markdown; the
/// verified artifact governs instead) and seeds the coordinator so decompose
/// skips the architect rather than re-deriving an approved plan.
///
/// `intent_policy` + `gate_log_pool` are the run's own policy engine and
/// session-DB pool; they back the ADR-60 Phase 1 supervised path's write gate
/// when `[orchestration] supervisor_enabled` opts in, and (under the same
/// opt-in only) the review-cycle resumability store. They mirror exactly what
/// the single-agent in-process gate is built from, so both gated paths enforce
/// the same policy over the same durable log.
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
    action_capable: bool,
    plan_objective_hash: String,
    approved_plan: Option<&ApprovedPlanContext>,
    stage_tracker: &Arc<Mutex<StageTracker>>,
    intent_policy: Arc<dyn PolicyEngine>,
    gate_log_pool: Option<sqlx::SqlitePool>,
    // ADR-65 §7: the checkpoint row's own `updated_at` — the v3 backfill
    // hint for the coordinator's additive §7 backfill. `None` for a fresh
    // run (the backfill then treats the whole log as pre-cursor, fail-soft).
    resume_cursor_hint_ms: Option<i64>,
    // ADR-60 D7 (interrupt-safe resume, 2026-09-05): the logged-evidence
    // dispatch seed for a checkpointless `continue` run — `None` for every
    // other run shape (fresh, checkpoint-resumed, approved-plan Execute).
    headless_resume: Option<HeadlessResumeSeed>,
    // The run-scoped project AGENTS.md context (ADR-70), injected into the
    // coordinator dispatch assembly between the skills section and the
    // environment card; its refresh cadence also gates the coordinator's
    // maintenance nudge. Refreshed once at run start by `run_shared_agent`.
    project_context: Arc<crate::project_context::ProjectContext>,
) -> Result<AgentOutput, OrchestratorError> {
    let project_dir = req.project_dir.clone();
    if let Some(store) = &session_store {
        // Run-start row: no provider call has happened yet, so there is no
        // usage to record — tokens stay `None` by design.
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

    // Resolve the default provider using model-first strategy (W3f).
    let (default_provider_config, default_model) = resolve_default_provider(req, settings)?;
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

    // Role topology, per-agent config map, and blueprint facade (W3g).
    let (roles_to_resolve, facade, agent_configs, tool_calling_roles) = build_role_topology(
        &services.config.multi_agent,
        services.config.resolved_blueprint.as_deref(),
        action_capable,
    );
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
        let assignment_provider_config = assignment.and_then(|assignment| {
            settings
                .providers
                .iter()
                .find(|config| ProviderFactory::config_id(config) == assignment.provider_config_id)
        });
        let model = assignment
            .and_then(|assignment| non_empty(assignment.model_override.as_deref()))
            .or_else(|| legacy_pins.get(&role).and_then(|model| non_empty(Some(model))))
            .unwrap_or_else(|| {
                if let Some(provider_config) = assignment_provider_config {
                    provider_config.model.trim()
                } else {
                    default_model
                }
            });
        // ADR-31 precedence for the serving provider: an explicit, valid
        // `agent_assignments` entry wins; otherwise a custom agent's live
        // `provider_id` that offers the resolved model; otherwise the run's
        // default provider. Model resolution above is independent and
        // unchanged (assignment override → legacy pin → default).
        let provider_config = resolve_role_provider_config(
            settings,
            &agent_configs,
            &role,
            assignment_provider_config,
            model,
            default_provider_config,
        );
        let provider_id = ProviderFactory::config_id(provider_config);
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

    // Hardcoded coordinator (maintainer decision 2026-09): the coordinator is
    // not a user-configurable roster agent. It is constructed in code and
    // always runs on the run's global default provider/model — any
    // `model_pins` or `model_settings.agent_assignments` entry naming
    // `coordinator` is inert (none are read here). Existing pin data is left
    // untouched on disk; it is simply never consulted for the coordinator.
    let coordinator_provider = default_provider.clone();
    let coordinator_model = resolve_coordinator_model(&pins, default_model);
    // ADR-42 §4 tier 2: the routing profile of the coordinator's model on its
    // serving pipe. With the coordinator pinned to the global default, this is
    // always the run's default pipe (the profile the ADR-45 tier-1b ladder
    // rebuilds onto), built the same way as every role profile above. The
    // coordinator's self-execution dispatches therefore route through the
    // runner like any other role instead of a raw single-shot request.
    let coordinator_pipe_id = ProviderFactory::config_id(default_provider_config);
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

    // ADR-45 tier-1b amendment: the alternate fallback pipes for the ladder.
    // On default configs the default-model pipe IS the coordinator's planning
    // pipe, so a "fallback to the default" would be a degenerate no-op; the
    // ladder instead prefers the first OTHER configured tool-calling pipe whose
    // credentials resolve. `base_profiles` is the run's configured routing
    // surface; the default pipe is excluded here (it is the primary preference
    // inside `resolve_fallback_pipe`). Credential resolution failures skip the
    // pipe rather than failing the run — a fallback is best-effort.
    let fallback_pipes: Vec<(Arc<dyn LlmProvider>, concerto_providers::model::ModelProfile)> =
        base_profiles
            .iter()
            .filter(|profile| profile.provider_config_id != coordinator_pipe_id)
            .filter(|profile| profile.supports_tool_calling)
            .filter_map(|profile| {
                let config = settings.providers.iter().find(|config| {
                    ProviderFactory::config_id(config) == profile.provider_config_id
                })?;
                let mut resolved_config = config.clone();
                resolved_config.model = profile.model.clone();
                let provider = ProviderFactory::build(&resolved_config, &creds).ok()?;
                Some((
                    provider,
                    concerto_providers::model::ModelProfile {
                        context_window: profile.context_window,
                        supports_tool_calling: profile.supports_tool_calling,
                        base_url: profile.base_url.clone(),
                        description: profile.description.clone(),
                        profile: profile.clone(),
                    },
                ))
            })
            .collect();

    // ADR-60 Phase 1 thin slice: an opt-in supervised run dispatches through
    // the process supervisor (real `orchestrator-agent-process` children under
    // one write gate) instead of the in-process coordinator waves. Only
    // Execute-classified runs take this path — the text-only fork above was
    // deleted with the unified agent loop (ADR-55 §1) and Plan still
    // runs on the coordinator at planning-only depth. Any preparation gap
    // (no session-DB pool, missing child binary, empty roster) degrades loudly
    // to the coordinator below rather than failing the run.
    if action_capable
        && services.config.multi_agent.as_ref().is_some_and(|multi| multi.supervisor_enabled)
    {
        // ADR-60 D7 ledger enrichment (Phase 4): a plan-driven run (its
        // whiteboard-verified context rehydrated by the caller) hands its
        // approved plan id to the children so their gated writes and terminal
        // events key into the plan's ledger.
        let supervised_plan_id = approved_plan.map(|context| context.binding.plan_id().to_owned());
        let consolidation =
            build_supervised_consolidation(&req.project_dir, req.memory_enabled).await;
        match prepare_supervised_run(
            services.config.multi_agent.as_ref(),
            &req.project_dir,
            session_id,
            &task.description,
            intent_policy.clone(),
            executor.clone(),
            gate_log_pool.clone(),
            memory.clone(),
            consolidation,
            supervised_plan_id,
            // ADR-60 S5 (DEFERRED #49): hand the children the effective config
            // (real provider rebuild), the parent-rendered skills section, the
            // run's bus (approval audit events) and the SAME frontend approval
            // sink the in-process paths use.
            &services.config,
            &services.skills,
            &services.bus,
            &services.approval_sink,
        ) {
            Some(supervised) => {
                return drive_supervised_run(
                    supervised,
                    services,
                    stage_tracker,
                    session_store.as_ref(),
                    task.id,
                    transcript_recorder,
                    event_recorder,
                    req.cancel_token.clone(),
                )
                .await;
            }
            None => {
                tracing::warn!(
                    "supervised multi-agent run could not start — falling back to the \
                     in-process coordinator"
                );
            }
        }
    }
    // Share one RetryPolicy across the registry for per-agent
    // with_provider_retry (used inside each specialist agent).
    let retry_policy = RetryPolicy::new(services.config.retry.clone());
    // ADR-43 Task 4: the session's budgeted skills section, captured once per
    // run and shared by the planner and every registered specialist. A UI
    // toggle (Task 7) refreshes `services.skills`; the next run picks it up.
    let skills_section = services.skills.section();
    // ADR-43: one audit record + `info` log per run proving which enabled
    // skill packs (and how many characters) were injected into the
    // coordinator's dispatch system prompt. Content-free (ids and sizes only)
    // and silent when no section is injected. Emitted here — after the
    // supervised-path early return above — so the record only claims what this
    // in-process coordinator actually injects.
    services.skills.report_injection(&services.bus, session_id);

    // `agent_configs` is built above (next to the tool-calling role
    // derivation) and reused here for the registry and the coordinator. The
    // resolved blueprint facade is handed to the registry so seeds are
    // registered from the resolved per-agent capabilities (ADR-58 P2+P3, R9).
    // `merge_seeds` mirrors `AppConfig::owns_agent_roster()`: once the config
    // declares a roster (custom agents or [orchestration]), the config IS
    // the roster and the seed set is NOT merged back in — deleted seeds stay
    // deleted at runtime (maintainer revision of ADR-58/59).
    let merge_seeds = !services.config.owns_agent_roster();
    // All specialists receive the native argv contract and actual host OS.
    let environment_card = crate::prompts::native_environment_card();
    let registry = Arc::new(AgentRegistry::build_with_roles_for_project_with_facade(
        role_providers,
        default_provider,
        executor.clone(),
        services.bus.clone(),
        retry_policy,
        &req.project_dir,
        &agent_configs,
        &skills_section,
        &environment_card,
        facade.as_ref(),
        merge_seeds,
        // ADR-65 §3: the session-DB pool backs every registered specialist's
        // tool-evidence writer. The snapshot barrier runs below, so the
        // writer's per-call `generation` comes from `AgentContext` instead of
        // being baked here.
        gate_log_pool.clone(),
        // Native validators attach the governed executor using their actual session.
        None,
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
    let runner = AgentRunner::new(registry.clone(), services.bus.clone(), spend_tracker.clone())
        .with_blueprint_facade(facade.clone());
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
        .with_tool_calling_roles(tool_calling_roles)
        .with_blueprint_facade(facade.clone()),
    );
    let selector =
        Arc::new(ModelSelector::new(Arc::new(ModelRegistry::from_profiles(profiles)), routing));
    // Phase 6 M3a/M3b: hand the coordinator the SAME decision/task-store
    // `Arc`s the memory system wraps, so a settled subtask writes back its
    // outcome and the run's Phase-0 retrieval reads this run's decisions.
    // Both are no-ops when memory is disabled or was initialised by a legacy
    // path that exposed no handles.
    let (memory_decision_store, memory_task_tree) = memory_writeback_handles(&services.memory);
    let mut coordinator = CoordinatorAgent::new(
        registry.clone(),
        runner,
        selector,
        spend_tracker,
        services.bus.clone(),
        coordinator_provider,
        memory.clone(),
    )
    .with_memory_writeback(memory_decision_store, memory_task_tree)
    .with_agent_configs(agent_configs)
    .with_skills_section(skills_section)
    // Run-scoped project AGENTS.md context (ADR-70): injected into the
    // dispatch system prompt between the skills section and the environment
    // card; its refresh cadence also gates the coordinator maintenance nudge.
    .with_project_context(Some(project_context))
    // Native execution facts for specialist and coordinator prompts.
    .with_environment_card(environment_card)
    // Dispatch uses the same native command contract as the single-agent loop.
    .with_native_shell()
    // ADR-048: `[context].cache_stable_prefix` pins a byte-stable dispatch
    // prompt head and appends the volatile working memory after it, matching
    // the single-agent loop. Resolved through the engine's budget policy so
    // the knob's default lives in exactly one place.
    .with_cache_stable_prefix(
        crate::context_engine::ContextBudgetPolicy::from_config(services.config.context.as_ref())
            .cache_stable_prefix,
    )
    // ADR-58 P2+P3 (Batch 1): the resolved blueprint facade backs the
    // stage-kind resolutions (`role_in_kind_stage`, `execution_stage_tag`,
    // `kind_stage_tag`). ADR-58 amendment (2026-09-05): the facade never
    // enforces blueprint staffing — the registry built from `custom_agents`
    // is the roster. `None` when no resolved blueprint is attached (e.g.
    // coordinators built in tests directly). Dispatch sites consult the
    // registry; dispatch authority belongs to the Coordinator (ADR-35
    // amendment 2026-09-05).
    .with_blueprint_facade(facade)
    .with_default_model_provider(Some(default_model_provider), default_model_profile)
    .with_planning_profile(planning_profile)
    // ADR-45 tier-1b amendment: alternate (non-default) tool-calling pipes for
    // the fallback ladder's degenerate escape (see `resolve_fallback_pipe`).
    .with_fallback_pipes(fallback_pipes)
    // ADR-35 §8: the shared executor backs coordinator self-execution when a
    // lifecycle stage has no registered agent. Attached unconditionally; the
    // coordinator only uses it when it actually self-executes.
    .with_executor(executor.clone())
    // ADR-35 §5 Phase 5 C-06 amendment: the coordinator's eval engine backs
    // coordinator self-verification when no validation-stage agent is
    // registered. Uses the same governed native executor as the single-agent
    // path; attached unconditionally — the coordinator
    // only uses it when a validate-stage agent is absent.
    .with_eval_engine(Arc::new(EvalEngine::new(&req.project_dir).with_process_executor(Arc::new(
        crate::exec_backend::NativeEvalExecutor {
            backend: executor.clone(),
            session: SessionContext::new(session_id, req.project_dir.clone()),
            orchestrator_authority: true,
        },
    ))))
    // Model-first serving pipe: an unassigned role's effective serving pipe
    // is the run's default provider. Without this the fallback ladder could
    // never resolve a serving pipe for unassigned roles, so tier 1 would be
    // skipped (or, worse, dispatch across pipes).
    .with_default_provider_config_id(Some(ProviderFactory::config_id(default_provider_config)))
    // ADR-35 amendment (2026-09-05): the run's policy engine gates every
    // `call_specialist` decision exactly like any tool call — the same
    // engine the shared executor enforces, never bypassed.
    .with_policy_engine(intent_policy.clone());
    // Run-continuity Phase 1: the coordinator persists resumable checkpoints
    // (and reads them back on `continue`) through the session store. Without
    // this the store stays `None`, `persist_checkpoint` silently no-ops, and
    // stalled runs leave zero resumable state.
    coordinator = coordinator.with_checkpoint_store(
        session_store.clone(),
        current_source_revision(&req.project_dir).await,
    );
    // ADR-65 §7: the checkpoint row's own updated_at backs the additive v4
    // backfill for pre-§7 (v3) checkpoints (fail-soft when absent).
    coordinator = coordinator.with_resume_cursor_hint_ms(resume_cursor_hint_ms);
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
    // Full local agency: the coordinator's decision loop owns the run shape at
    // runtime. No router hint is attached and no shape is predicted up front,
    // so a pure-text turn records no run-shape whiteboard/audit decision on
    // message arrival (lazy machinery). The coordinator runs its full
    // lifecycle and engages specialists only when the task needs hands.
    let planning_only = false;
    // ADR-60 D7 (#152): an approved-plan run seeds the whiteboard-verified
    // structured state so decompose skips the architect — re-deriving an
    // approved plan (silent re-decompose) is forbidden. The supervised path
    // above needs no seeding: children receive the structured task
    // description directly.
    if let Some(context) = approved_plan {
        coordinator = coordinator.with_approved_plan_seed(ApprovedPlanSeed {
            plan_id: context.binding.plan_id().to_owned(),
            design_doc: context.design_doc.clone(),
        });
    }
    // ADR-60 D7 (interrupt-safe resume, 2026-09-05): a checkpointless
    // `continue` run schedules from the logged evidence chain. The seed's
    // verified plan re-anchors the design; the evidence scheduler then
    // dispatches the coder (not the architect) and every dispatch is recorded
    // as an evidence-backed `Decision` event (ADR-65 §7). The pool is
    // attached here too when a seed rides along: the scheduler's evidence
    // read (workspace observations) and the Decision appends need it, and
    // this resume is a D7 whiteboard read by construction.
    if let Some(seed) = headless_resume {
        coordinator = coordinator.with_headless_resume_seed(seed);
        if gate_log_pool.is_none() {
            tracing::warn!(
                "headless resume armed without a session DB pool — the evidence \
                 scheduler will see no observations (rule-b exploration applies)"
            );
        }
        coordinator = coordinator.with_review_store(gate_log_pool.clone());
    }
    // ADR-60 Deferred 3 (issue #19 decoupling): review-cycle resumability
    // stays gated behind `[orchestration] supervisor_enabled` — the supervised
    // runtime's opt-in — and is deliberately independent of the D7
    // `plan_binding_source` switch, which now governs only Plan→Execute state
    // sourcing. The pool is the run's own session DB,
    // the same durable log the write gate and the plan events use. Without a
    // pool (or without the flag) review cycles degrade to pre-Phase 3
    // behavior; the missing-pool case is warned here because only this site
    // knows both facts.
    if supervisor_review_enabled(services.config.multi_agent.as_ref()) {
        if gate_log_pool.is_none() {
            tracing::warn!(
                "no session DB pool — review cycles run non-resumable \
                 (ADR-60 Deferred 3 degradation)"
            );
        }
        coordinator = coordinator.with_review_store(gate_log_pool.clone());
    }
    // Decision-event persistence: the session-DB pool backs every ADR-65
    // `Decision` whiteboard append this in-process coordinator performs
    // (run-shape, planning/prose recovery, resume, dispatch failover). The
    // conditional arms above are retained for their explicit degradation
    // logging, but the pool is attached UNCONDITIONALLY so a fresh
    // (non-resume, non-supervisor) run still persists its decisions instead
    // of silently dropping them. Fail-soft by contract: a `None` pool simply
    // skips the appends.
    coordinator = coordinator.with_review_store(gate_log_pool.clone());
    // ADR-60 D7: an approved-plan run does NOT inject the conversation
    // history as prose — that transcript carries the rendered plan markdown,
    // and the whiteboard-verified structured artifact governs instead.
    // Prior decisions that matter ride the ledger in the task description;
    // non-approved runs keep the exact pre-D7 history injection.
    let multi_task = match approved_plan {
        Some(_) => task.clone(),
        None => multi_agent_task_with_history(task.clone(), &req.conversation_history),
    };
    let mut context = AgentContext::new(SessionContext::new(session_id, project_dir.clone()));
    // Run-stage tracking (ADR-55 §9): the coordinator publishes
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
    // ---- ADR-65 §2 (Phase 2): workspace snapshot readiness barrier ----
    //
    // A read-only, deterministic, language-agnostic project-tree inventory
    // (relative paths + size + mtime + cheap content hash) is captured BEFORE
    // planning begins. Planning waits on THIS barrier — never on the
    // asynchronously spawned vector indexing. The digest rides into every
    // dispatched agent's context (via the snapshot attached to the
    // coordinator), and a `WorkspaceSnapshot` whiteboard event + a
    // `resource_facts` application are appended best-effort to the shared
    // run database. Fail-soft by contract: an unreadable project dir (None) or
    // an unpersistable snapshot degrades to a warning — the run proceeds.
    let workspace_snapshot = crate::workspace_snapshot::run_snapshot_barrier(
        gate_log_pool.as_ref(),
        &project_dir,
        &session_id.to_string(),
        &req.cancel_token,
    )
    .await;
    if let Some(snapshot) = workspace_snapshot {
        context.workspace_snapshot_digest = Some(snapshot.digest());
        coordinator = coordinator.with_workspace_snapshot(snapshot);
    }
    let mut output = match coordinator
        .run(multi_task, context, req.cancel_token.clone(), req.resume_checkpoint_json.clone())
        .await
    {
        Ok(output) => output,
        Err(error) => {
            stage_feed.abort();
            // Settle the coordinator's usage first: it feeds both the failure
            // row below and the metrics/spend tail, so one rate-limit
            // exhaustion or cancellation records the same tokens everywhere
            // (ADR-48 §4 — real usage when available, on the message row too).
            let settled = coordinator.settled_metrics();
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
                let (tokens_in, tokens_out) = message_row_usage(settled);
                let failure_message = Message {
                    role: concerto_core::types::Role::Assistant,
                    content,
                    tool_calls: None,
                    tool_results: None,
                    reasoning_content: None,
                    tokens_in,
                    tokens_out,
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
            // Persist the coordinator's settled metrics (bound above, before
            // the failure row) before the error propagates: on a rate-limit
            // exhaustion / cancellation the run consumed real tokens without
            // any output — the audit trail must still record them. Best-effort
            // like the success tail.
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
    // ADR-55 §4 (M3): on a completed planning-only run, bind the
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
        // ADR-60 D7 (#152): the whiteboard event commits FIRST (the log is
        // the source of truth), then the durable `plan_bindings` mirror — a
        // crash in between leaves the log ahead of the projection. A
        // planning-only run carries the structured DesignDoc the architect
        // produced; Execute rehydrates it instead of re-deriving the plan.
        append_plan_binding_event(
            gate_log_pool.as_ref(),
            session_id,
            &binding,
            coordinator.design_doc_snapshot().as_ref(),
            services.config.multi_agent.as_ref(),
        )
        .await;
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
        // ADR-48 §4: the assistant row records the same settled usage the
        // `provider_metrics` and spend writes above record for this run. The
        // run-start user row predates any provider call and stays `None`.
        let (tokens_in, tokens_out) = message_row_usage(&output.provider_metrics);
        let assistant_message = Message {
            role: concerto_core::types::Role::Assistant,
            content: output.final_message.clone(),
            tool_calls: None,
            tool_results: None,
            reasoning_content: None,
            tokens_in,
            tokens_out,
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

// ===========================================================================
// ADR-60 Phase 1 thin slice: the supervised multi-agent path.
//
// Opt-in via `[orchestration] supervisor_enabled` (default off — the
// in-process coordinator below remains the production path). The supervised
// run shares ONE write gate over the run's own policy/executor pair and
// session-DB pool (mirroring the single-agent `InProcessGateBackend`
// construction), and every child reaches tools exclusively through the
// supervisor's `execute-tool` dispatch, which is `WriteGate::submit`: the
// WAL append commits before any VirtualFs apply, and a base_version collision
// surfaces as `GateError::Conflict` → IPC `-32005` — a retriable tool error
// for the child, never process termination. There is no direct executor write
// anywhere on this path.
//
// ADR-60 Deferred 3: review-state resumability lives in `plan_approval.rs`
// (shared `ReviewState` whiteboard kind + payload, one serialization for
// every path). ADR-35 amendment (2026-09-16 §1) removed the automatic
// `run_review_cycle` / `run_validation_loop` gates, so review/validation are
// now Coordinator `call_specialist` decisions with no compiled cycle driver.
// The coordinator path still attaches the store in this file behind the D7
// opt-in. Supervised CHILDREN are single-agent loops and run no multi-agent
// review cycles today; when they gain review participation (Phase 4), they
// must reuse the same plan_approval helpers — not a second serialization.
// ===========================================================================

/// Wall-clock budget for one supervised multi-agent run before it is torn
/// down and reported partial. Generous on purpose: children are full agent
/// loops; this guards only against a wedged child or a lost completion.
const SUPERVISED_RUN_BUDGET: std::time::Duration = std::time::Duration::from_secs(600);

/// Everything a prepared supervised multi-agent run needs.
struct SupervisedRun {
    /// Write-path services shared by every child (ADR-60 D3/D4): one write
    /// gate over the run's policy/executor pair + session DB pool, the
    /// whiteboard log pool, the memory spine, and the subscription registry.
    services: SupervisorServices,
    /// Supervisor tunables, including each agent's whiteboard subscription
    /// (`Decision` topics), registered by the loop on first sight of the
    /// child generation.
    config: SupervisorConfig,
    /// Resolved path of the `orchestrator-agent-process` child binary.
    binary: PathBuf,
    /// Project root handed to each child (its tool scope).
    project_root: PathBuf,
    /// Owning session id, stamped on appended messages for this run.
    session_id: Ulid,
    /// `(agent id, task description)` pairs to spawn, in topology order.
    tasks: Vec<(String, String)>,
    /// ADR-60 D7 ledger enrichment (Phase 4): the approved plan this run
    /// executes, when the run is plan-driven under the whiteboard binding.
    /// Handed to each child via `CONCERTO_PLAN_ID` so the child stamps
    /// `GateRequest.plan_id` and its terminal events — write-applied rows and
    /// subtask completions then key into the plan's ledger (`fold_ledger`).
    plan_id: Option<String>,
    /// ADR-60 S5 (DEFERRED #49): extra environment the parent stamps on every
    /// child so it can rebuild the real provider and inject the same skills
    /// section. Resolved once per run: the effective `AppConfig` as JSON
    /// ([`crate::agent_process_config::CONFIG_ENV`], no secrets) and the
    /// parent-rendered skills section (`CONCERTO_AGENT_SKILLS_SECTION`).
    spawn_env: Vec<(String, String)>,
}

/// Prepare everything a supervised multi-agent run needs, or `None` (with a
/// logged reason) when the supervised path cannot start and the caller must
/// fall back to the in-process coordinator — degradation, never silent.
///
/// The gate mirrors the single-agent construction exactly: same policy/
/// executor pair, same pool, max-in-flight 1 per agent.
#[allow(clippy::too_many_arguments)]
fn prepare_supervised_run(
    multi_agent: Option<&concerto_config::MultiAgentConfig>,
    project_dir: &Path,
    session_id: Ulid,
    objective: &str,
    policy: Arc<dyn PolicyEngine>,
    executor: Arc<ToolExecutor>,
    gate_log_pool: Option<sqlx::SqlitePool>,
    memory: Arc<dyn MemoryStore>,
    consolidation: Option<Arc<crate::consolidation::Consolidator>>,
    plan_id: Option<String>,
    config: &AppConfig,
    skills: &crate::skills_context::SkillsContext,
    bus: &EventBus,
    approval_sink: &Arc<dyn ApprovalSink>,
) -> Option<SupervisedRun> {
    let tasks = supervised_agent_tasks(multi_agent, objective);
    if tasks.is_empty() {
        tracing::warn!(
            "no enabled specialist agents in the configured topology — supervised path unavailable"
        );
        return None;
    }
    let Some(log_pool) = gate_log_pool else {
        // Same documented degradation as the single-agent gate: without the
        // session DB the write gate cannot append its WAL rows, so the
        // supervised invariant (WAL-before-execute) could not hold.
        tracing::warn!(
            "no session DB pool for the supervised write gate — falling back to the coordinator"
        );
        return None;
    };
    let Some(binary) = agent_process_binary() else {
        tracing::warn!(
            "orchestrator-agent-process binary not found (set CONCERTO_AGENT_PROCESS_BIN or \
             place it next to the running executable) — falling back to the coordinator"
        );
        return None;
    };

    let gate = Arc::new(WriteGate::new(
        policy,
        executor,
        log_pool.clone(),
        Arc::new(FilePreImageReader::new(project_dir)),
        project_dir.to_path_buf(),
        1,
    ));
    let services = SupervisorServices {
        gate,
        whiteboard_pool: log_pool.clone(),
        memory,
        project_id: ProjectId(concerto_core::helpers::project_id_hash(project_dir)),
        subscriptions: SubscriptionManager::new(log_pool),
        consolidation,
        // ADR-60 S5 approval bridge: children route approvals to the SAME
        // frontend sink the in-process paths use. `None` keeps the fail-closed
        // deny default.
        approval_sink: Some(approval_sink.clone()),
        bus: Some(bus.clone()),
    };
    let spawn_env = supervised_spawn_env(config, skills);
    // ADR-60 D3: every supervised worker subscribes to `Decision` topics so
    // sibling decisions stream to it as `whiteboard-slice` pushes (protocol
    // 0.2.0); the supervisor loop registers these on first sight of the child.
    let mut supervisor_config = SupervisorConfig::default();
    for (agent_id, _) in &tasks {
        supervisor_config = supervisor_config.with_whiteboard_subscription(
            agent_id.clone(),
            vec![concerto_sessions::whiteboard::WhiteboardKind::Decision],
        );
    }
    Some(SupervisedRun {
        services,
        config: supervisor_config,
        binary,
        project_root: project_dir.to_path_buf(),
        session_id,
        tasks,
        plan_id,
        spawn_env,
    })
}

/// Build the per-run child environment carrying the effective config and the
/// parent-rendered skills section (ADR-60 S5, DEFERRED #49).
///
/// Fail-soft on serialization: a config that cannot be serialized is logged
/// and omitted — the child then fails closed with a named missing-config error
/// rather than silently falling back to a mock. The values carry no secrets
/// (provider metadata + skill *instructions*, which are local instruction
/// packs by design).
fn supervised_spawn_env(
    config: &AppConfig,
    skills: &crate::skills_context::SkillsContext,
) -> Vec<(String, String)> {
    use crate::agent_process_config::CONFIG_ENV;
    let mut env = Vec::new();
    match serde_json::to_string(config) {
        Ok(json) => env.push((CONFIG_ENV.to_owned(), json)),
        Err(error) => {
            tracing::warn!(
                %error,
                "supervised run: could not serialize the effective config for children; \
                 children will fail closed on a missing provider"
            );
        }
    }
    let section = skills.section();
    if !section.is_empty() {
        env.push((SUPERVISED_SKILLS_ENV.to_owned(), section));
    }
    env
}

/// Environment variable carrying the parent-rendered skills section to each
/// supervised child (`agent_process.rs` reads the same name).
const SUPERVISED_SKILLS_ENV: &str = "CONCERTO_AGENT_SKILLS_SECTION";

/// ADR-60 D6 (Phase 4): construct the supervised consolidation projection
/// task over the project's memory DB (`<app data>/memory/memory.db`), the
/// same store the memory subsystem indexes into.
///
/// Fail-soft like every optional spine on this path: memory disabled, an
/// unopenable DB, or a failed store migration yields `None` with a warn — the
/// supervised run proceeds without consolidation, and the whiteboard log (the
/// source of truth) is unaffected. Gated behind `memory_enabled` so a user
/// who disabled the memory subsystem never gets a hidden projection writer.
async fn build_supervised_consolidation(
    project_dir: &Path,
    memory_enabled: bool,
) -> Option<Arc<crate::consolidation::Consolidator>> {
    if !memory_enabled {
        tracing::debug!("memory disabled — supervised D6 consolidation not constructed");
        return None;
    }
    let project_id = ProjectId(concerto_core::helpers::project_id_hash(project_dir));
    let db_path = match concerto_sessions::app_data_dir() {
        Ok(dir) => dir.join("memory").join("memory.db"),
        Err(error) => {
            tracing::warn!(%error, "data directory unavailable — no D6 consolidation projection");
            return None;
        }
    };
    let db_utf8 = match camino::Utf8PathBuf::from_path_buf(db_path) {
        Ok(path) => path,
        Err(path) => {
            tracing::warn!(
                path = %path.display(),
                "memory DB path is not valid UTF-8 — no D6 consolidation projection"
            );
            return None;
        }
    };
    let db = match concerto_memory::storage::MemoryDb::connect(&db_utf8).await {
        Ok(db) => db,
        Err(error) => {
            tracing::warn!(%error, "memory DB unavailable — no D6 consolidation projection");
            return None;
        }
    };
    let pool = db.pool().clone();
    let store = match concerto_memory::vector_store::SqliteVectorStore::new(pool.clone()).await {
        Ok(store) => Arc::new(store),
        Err(error) => {
            tracing::warn!(%error, "vector store unavailable — no D6 consolidation projection");
            return None;
        }
    };
    // Use the same on-device embedder the project indexer uses. The model
    // downloads on first embed; a failed embed fails soft to the projection's
    // deterministic hash vector, so a pass never fails on a missing model.
    let embedder: Arc<dyn EmbeddingGenerator> =
        Arc::new(ProviderEmbedder::new("bge-small-en-v1.5"));
    Some(Arc::new(crate::consolidation::Consolidator::new(pool, store, project_id, Some(embedder))))
}

/// Locate the ADR-60 agent-process child binary: an explicit
/// `CONCERTO_AGENT_PROCESS_BIN` wins; otherwise the sibling of the running
/// executable (`orchestrator-agent-process[.exe]`). `None` when neither names
/// a file — the caller degrades loudly to the coordinator.
fn agent_process_binary() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("CONCERTO_AGENT_PROCESS_BIN") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Some(path);
        }
        tracing::warn!(
            path = %path.display(),
            "CONCERTO_AGENT_PROCESS_BIN does not name a file; probing the executable directory"
        );
    }
    let exe = std::env::current_exe().ok()?;
    let sibling =
        exe.with_file_name(format!("orchestrator-agent-process{}", std::env::consts::EXE_SUFFIX));
    sibling.is_file().then_some(sibling)
}

/// The supervised roster: every enabled specialist role in the configured
/// topology (built-ins not disabled by config, then custom agents), excluding
/// the coordinator — the coordinator role stays an in-process planning concern
/// (ADR-35 §5) with no agent-process form.
///
/// Phase 1 thin slice: each worker receives the whole objective — there is no
/// graph decomposition yet (that remains the coordinator fallback's job until
/// the supervised planner lands), which is exactly what makes the shared
/// write gate's conflict surface the coordination mechanism.
fn supervised_agent_tasks(
    multi_agent: Option<&concerto_config::MultiAgentConfig>,
    objective: &str,
) -> Vec<(String, String)> {
    // Reuse the canonical topology ordering (ADR-35 phase 4); the
    // once-per-run config clone keeps `topology_roles`' signature untouched.
    let owned = multi_agent.cloned();
    topology_roles(&owned)
        .into_iter()
        .filter(|role| role.as_str() != "coordinator")
        .map(|role| (role.as_str().to_owned(), objective.to_owned()))
        .collect()
}

/// W2e: build the child commands for a supervised run — one per task, in
/// topology order, each carrying the agent identity, project root, task
/// description, the run's serialized config/skills environment, and (for a
/// plan-driven run) the approved plan id. Pure: constructs the commands and
/// performs no spawning or I/O; the caller owns `Supervisor::spawn_agent`.
/// Takes the disjoint fields so it can be called after `run.config` has moved
/// into the supervisor.
fn build_supervised_commands(
    binary: &Path,
    project_root: &Path,
    tasks: &[(String, String)],
    spawn_env: &[(String, String)],
    plan_id: Option<&str>,
) -> Vec<(String, std::process::Command)> {
    tasks
        .iter()
        .map(|(agent_id, description)| {
            let mut command = std::process::Command::new(binary);
            command
                .env("CONCERTO_AGENT_ID", agent_id)
                .env("CONCERTO_PROJECT_ROOT", project_root)
                .env("CONCERTO_TASK_DESCRIPTION", description);
            // ADR-60 S5 (DEFERRED #49): the effective config (no secrets) and
            // the parent-rendered skills section are stamped once per run so
            // each child rebuilds the real provider and injects the same
            // skills. `mock` is an explicit opt-in only — the child never
            // falls back to it implicitly.
            for (name, value) in spawn_env {
                command.env(name, value);
            }
            // ADR-60 D7 ledger enrichment: a plan-driven run stamps every
            // child write with the approved plan id (the child mirrors it onto
            // `GateRequest.plan_id` and its terminal whiteboard events).
            if let Some(plan_id) = plan_id {
                command.env("CONCERTO_PLAN_ID", plan_id);
            }
            (agent_id.clone(), command)
        })
        .collect()
}

/// Drive one prepared supervised multi-agent run to completion and synthesize
/// the run output from the supervisor's summary.
///
/// Cancellation propagates as [`OrchestratorError::Cancelled`] (matching the
/// coordinator path); a budget overrun tears the children down gracefully and
/// reports [`AgentCompletionStatus::Partial`].
#[allow(clippy::too_many_arguments)]
async fn drive_supervised_run(
    run: SupervisedRun,
    services: &SharedServices,
    stage_tracker: &Arc<Mutex<StageTracker>>,
    session_store: Option<&Arc<dyn SessionStore>>,
    task_id: TaskId,
    transcript_recorder: TranscriptRecorderGuard,
    event_recorder: EventRecorderGuard,
    cancel: CancellationToken,
) -> Result<AgentOutput, OrchestratorError> {
    // Oracle comment 4 (ADR-60 Phase 1 review): in-flight review state is not
    // persisted yet (Deferred 3). A supervised run that dies mid-review loses
    // the cycle; a restart re-enters it from scratch. Loud until Phase 3
    // lands the plan_id/review_target stash.
    tracing::warn!(
        "supervised multi-agent run starting without review-cycle resumability \
         (ADR-60 Deferred 3): in-flight review state is lost on restart"
    );
    let mut supervisor = Supervisor::new(run.config);
    // Keep a pool handle for the Decision rows emitted after `run.services` is
    // moved into the supervisor (`with_services`).
    let decision_pool = run.services.whiteboard_pool.clone();
    // Supremacy invariant (item: supervisor child spawn): a child that cannot
    // be spawned is routed into the run's failure accounting and a Coordinator
    // Decision row — it is NOT a terminal error to the caller. The remaining
    // children still run under the shared write gate, and the run ends Partial
    // with the failed agent named. (Retry/ladder is the process-supervision
    // driver's existing `max_restarts` bound, which governs runtime crashes;
    // a start failure has no in-flight process to restart, so it is decided
    // directly as a failed subtask.)
    let mut failed_to_start: Vec<String> = Vec::new();
    for (agent_id, mut command) in build_supervised_commands(
        &run.binary,
        &run.project_root,
        &run.tasks,
        &run.spawn_env,
        run.plan_id.as_deref(),
    ) {
        if let Err(error) = supervisor.spawn_agent(&mut command, &agent_id) {
            tracing::warn!(%agent_id, %error, "supervised agent failed to start; recording a decision");
            crate::bypass_decision::record_decision_row(
                None,
                Some(&decision_pool),
                run.session_id,
                "supervisor-agent-start-failed",
                &format!("agent-process '{agent_id}' failed to start: {error}"),
            )
            .await;
            failed_to_start.push(agent_id);
        }
    }

    let expected: std::collections::HashSet<String> =
        run.tasks.iter().map(|(id, _)| id.clone()).collect();
    let mut supervisor = supervisor.with_services(run.services);
    // Budget guard: a separate token so an overrun teardown can never be
    // misreported as user cancellation.
    let budget_cancel = CancellationToken::new();
    // ADR-60 D7 (interrupt-safe resume): the user's cancel token was never
    // wired into `run_until` — a cancelled supervised run kept driving its
    // children to completion, and only THEN reported cancellation. A stop
    // now reaches the supervisor directly: `run_until`'s teardown stops the
    // children and persists the gate-boundary shutdown checkpoint
    // (`Supervisor::checkpoint_at_shutdown` semantics) before the summary
    // returns. The combined stop token is the budget token's child, so a
    // budget expiry still cancels the run; a watcher forwards user
    // cancellation into it.
    let run_stop = budget_cancel.child_token();
    let stop_watcher = {
        let run_stop = run_stop.clone();
        let user_cancel = cancel.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = user_cancel.cancelled() => run_stop.cancel(),
                // The run ended (or the budget fired through the child):
                // nothing to forward.
                _ = run_stop.cancelled() => {}
            }
        })
    };
    let driven = tokio::time::timeout(
        SUPERVISED_RUN_BUDGET,
        supervisor.run_until(run_stop.clone(), |supervisor| {
            supervisor.agents().iter().all(|meta| {
                expected.contains(&meta.agent_id)
                    && matches!(meta.state, AgentState::Completed | AgentState::Failed)
            })
        }),
    )
    .await;
    stop_watcher.abort();
    let summary = match driven {
        Ok(summary) => summary,
        Err(_elapsed) => {
            tracing::warn!("supervised multi-agent run exceeded its time budget; tearing down");
            budget_cancel.cancel();
            supervisor.run_until(CancellationToken::new(), |_| false).await
        }
    };

    if cancel.is_cancelled() {
        event_recorder.stop().await;
        transcript_recorder.stop().await;
        return Err(OrchestratorError::Cancelled);
    }

    // ADR-60 D5: a supervised run that ended without a durable shutdown
    // checkpoint loses its cheap resume point (restart restore must fall
    // back to a full log replay). Loud until restore is wired end-to-end.
    if let Some(error) = &summary.checkpoint_error {
        tracing::warn!(
            %error,
            "supervised multi-agent run ended without a persisted shutdown checkpoint (ADR-60 D5)"
        );
    }

    let total = expected.len();
    let completed = summary
        .agents
        .iter()
        .filter(|meta| meta.state == AgentState::Completed && expected.contains(&meta.agent_id))
        .count();
    let all_completed = total > 0 && completed == total && failed_to_start.is_empty();
    for agent_id in &summary.failed {
        tracing::warn!(%agent_id, "supervised multi-agent run finished with a failed agent");
        // Supremacy invariant: a runtime child crash / restart-budget
        // exhaustion is recorded as a Coordinator Decision row (and accounted
        // as a failed subtask below), never a silent terminal.
        let reason = summary
            .agents
            .iter()
            .find(|meta| &meta.agent_id == agent_id)
            .map(|meta| {
                format!(
                    "agent-process '{agent_id}' failed (state={:?}, restarts={})",
                    meta.state, meta.restart_count
                )
            })
            .unwrap_or_else(|| format!("agent-process '{agent_id}' failed"));
        crate::bypass_decision::record_decision_row(
            None,
            Some(&decision_pool),
            run.session_id,
            "supervisor-agent-failed",
            &reason,
        )
        .await;
    }
    let final_message = if all_completed {
        format!(
            "Supervised run completed: {total} agent process(es) finished under the shared \
             write gate."
        )
    } else {
        let mut failed_all: Vec<String> = summary.failed.clone();
        for agent_id in &failed_to_start {
            if !failed_all.contains(agent_id) {
                failed_all.push(agent_id.clone());
            }
        }
        let failed_list =
            if failed_all.is_empty() { "none".to_owned() } else { failed_all.join(", ") };
        format!(
            "Supervised run partially completed: {completed}/{total} agent process(es) finished; \
             failed agents: {failed_list}."
        )
    };
    // Item (warn-only drops): ONE degraded note for the run summary rather than
    // a Decision row per continuity drop (the supervisor's `degraded` flag).
    let final_message = if summary.degraded {
        format!(
            "{final_message} Degraded: a continuity write (shutdown checkpoint / lease release) \
             could not be persisted; the individual warnings remain in the logs. Restart restore \
             may fall back to a full log replay."
        )
    } else {
        final_message
    };
    let project_root =
        camino::Utf8PathBuf::from_path_buf(run.project_root.clone()).unwrap_or_default();

    stage_tracker.lock().unwrap_or_else(|error| error.into_inner()).set(RunStage::Complete);
    // Per-child provider metrics/spend are not surfaced through the parent in
    // this slice (the mock-provider children consume none), so there is
    // nothing to persist here yet — the coordinator tail's metric writes are
    // deliberately omitted rather than called with empty data, and the
    // assistant row's token fields stay `None` for the same reason.
    if let Some(store) = session_store {
        let assistant_message = Message {
            role: Role::Assistant,
            content: final_message.clone(),
            tool_calls: None,
            tool_results: None,
            reasoning_content: None,
            tokens_in: None,
            tokens_out: None,
        };
        if let Err(error) =
            store.append_messages(run.session_id, &[assistant_message], cancel.clone()).await
        {
            if is_expected_cancellation(&error, &cancel) {
                tracing::debug!(%error, "run cancelled; supervised assistant message not persisted");
            } else {
                tracing::warn!(%error, "failed to persist supervised assistant message");
            }
        }
    }
    transcript_recorder
        .append_entries(&[
            TranscriptEntry::Assistant { content: final_message.clone() },
            TranscriptEntry::Completion {
                multi_agent: true,
                completed: all_completed,
                files: Vec::new(),
                project_root: Some(project_root.to_string()),
            },
        ])
        .await;
    maintain_context_after_run(
        session_store,
        run.session_id,
        services.config.context.as_ref(),
        cancel.clone(),
        Some(&services.bus),
    )
    .await;
    transcript_recorder.stop().await;
    event_recorder.stop().await;

    Ok(AgentOutput {
        task_id,
        session_id: run.session_id,
        final_message,
        files_modified: Vec::new(),
        tool_call_count: 0,
        eval_result: None,
        tool_events: Vec::new(),
        verification: Vec::new(),
        project_root: Some(project_root),
        completion_status: if all_completed {
            AgentCompletionStatus::Completed
        } else {
            AgentCompletionStatus::Partial
        },
        provider_metrics: Vec::new(),
        checkpoint_json: None,
    })
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

/// The project scope a resume validates an orchestration checkpoint against.
///
/// This is the checkpoint's OWN project-id definition — the row's
/// `project_id` is written by the coordinator from
/// `SessionContext::project_id` ([`concerto_core::types::ProjectId::resolve`]:
/// blake3 of the git default remote URL, or of the canonicalized absolute
/// path for non-git projects). Scope validation must compare like with
/// like; the previous use of the unrelated
/// [`concerto_core::helpers::project_id_hash`] definition (SipHash of the
/// path, 16 hex chars) could never match, so every real checkpoint row was
/// discarded as "belongs to a different project" and then cleared on
/// resume (ADR-60 D7 interrupt-safe resume, 2026-09-05).
fn resume_scope_project_id(project_dir: &Path) -> String {
    ProjectId::resolve(project_dir).0
}

/// ADR-55 §4 (M2): discard the session's orchestration checkpoint
/// before a plan-driven (Apply) Execute so the run re-plans from the
/// approved plan instead of silently resuming an old partial graph — the
/// same objective hash would otherwise trip the implicit-resume check.
async fn suppress_stale_checkpoint_for_apply(
    session_manager: &ProjectSessionManager,
    session_id: Ulid,
    executor: Option<&concerto_core::ToolExecutor>,
    pool: Option<&sqlx::SqlitePool>,
) {
    crate::bypass_decision::record_decision_row(
        executor,
        pool,
        session_id,
        "checkpoint-cleared-pre-execute",
        "plan-driven Execute suppresses any stale checkpoint so the approved plan governs",
    )
    .await;
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

// NORM S15: the `EventRecorderStore` stub wall + its recorder tests live
// in `runtime_runner/tests/mod.rs`. This file is loaded via `#[path]` as
// `runtime_runner_impl`, so the submodule needs an explicit `#[path]` too.
#[cfg(test)]
#[path = "runtime_runner/tests/mod.rs"]
mod tests;

#[cfg(test)]
mod runtime_runner_tests {
    // NORM S24D: the shared D7 fixtures (`d7_pool`, `d7_design_doc`,
    // `d7_binding`) moved to `tests::approved_plan_d7` and are re-exported here
    // at their old path, so the continuity tests' back-import
    // (`super::super::runtime_runner_tests::{...}`) stays unchanged.
    pub(super) use super::tests::approved_plan_d7::{d7_binding, d7_design_doc, d7_pool};

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
    // Run-stage tracking (ADR-55 §9): StageTracker transition-only
    // emission and the single-agent wiring through execute_agent_loop.
    // ------------------------------------------------------------------
}
