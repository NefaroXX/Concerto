//! `AgentRegistry` — maps agent IDs to their implementations.
//!
//! Provides lookup and lifecycle management for specialist agents.
//! `build_default` registers all five agents with their default setups.

use std::collections::HashMap;
use std::sync::Arc;

use concerto_config::{
    builtin_agent_seeds, AgentCapabilities, BlueprintFacade, CustomAgentConfig, PromptSections,
    StageKind,
};
use concerto_core::event::EventBus;
use concerto_core::executor::ToolExecutor;
use concerto_core::traits::agent::ExpertAgent;
use concerto_core::traits::provider::LlmProvider;
use concerto_core::types::{AgentId, AgentStage, OutputMode};
use concerto_providers::retry::RetryPolicy;

use crate::agents::GenericSpecialistAgent;

/// Merge the built-in specialist seeds with the user's `custom_agents`
/// config, user entries winning by id (ADR-35 phase 4) — or stand the
/// config alone.
///
/// All five built-in specialists (architect, researcher, coder, reviewer,
/// validator) are backed by [`GenericSpecialistAgent`] with the matching
/// [`OutputMode`]; the validator additionally carries the eval engine
/// attached at registration (audit A-01), so an unconfigured pipeline
/// behaves identically to the dedicated structs they replace. The reserved
/// `coordinator` id is never registered from config (ADR-35 §5 — the
/// Coordinator is constructed in code only).
///
/// `merge_seeds: true` keeps the legacy embedded-default behavior (seeds
/// overlaid with config entries; `AppConfig::owns_agent_roster() == false`).
///
/// `merge_seeds: false` is the config-ownership mode (maintainer revision of
/// ADR-58/59): once the config declares a roster (`custom_agents` non-empty
/// or `[orchestration]` present), the config IS the roster. Deleted seed ids
/// stay deleted — nothing is resurrected — exactly as the user instruction
/// requires.
fn merged_agent_configs(
    agent_configs: &HashMap<AgentId, CustomAgentConfig>,
    merge_seeds: bool,
) -> HashMap<AgentId, CustomAgentConfig> {
    if !merge_seeds {
        return agent_configs.clone();
    }
    let mut merged: HashMap<AgentId, CustomAgentConfig> =
        builtin_agent_seeds().into_iter().map(|cfg| (AgentId::new(&cfg.id), cfg)).collect();
    for (id, cfg) in agent_configs {
        let effective = merged
            .get(id)
            .map(|seed| merge_custom_over_seed(cfg, seed))
            .unwrap_or_else(|| cfg.clone());
        merged.insert(id.clone(), effective);
    }
    merged
}

/// Overlay a user's `custom_agents` entry on its built-in seed, keeping the
/// user's explicit fields but inheriting unset lifecycle defaults from the
/// seed.
///
/// The Orchestration Studio persists a `custom_agents` entry for *every*
/// studio agent (including the built-in specialists) on save, so entries
/// that only set a model/provider assignment still shadow their seed at
/// runtime. If such an entry omitted `stage` — e.g. it was written by an
/// older app version, or the user hand-edited the config — the seed's
/// lifecycle tag was lost wholesale, which removed the implement-stage
/// agent from the pipeline and made every Build fail instantly with
/// "no implementation-stage agent is registered; cannot plan implementation
/// work". The seed defaults are inherited field-by-field so a partial
/// override can never silently degrade the pipeline:
///
/// - `stage`: user's tag wins; `None` inherits the seed's.
/// - `output_mode`: the user's concrete value wins; the default (`Freeform`)
///   inherits the seed's (e.g. `DesignDoc` for the architect).
/// - `capabilities`: inherit the seed's when the user entry left the field
///   at its default (an entry that only sets `eval` — the default — is
///   indistinguishable from an unset one, so inheriting the seed's tool set
///   is the correct default behavior).
/// - `prompt_sections`: empty sections inherit the seed's prompts.
///
/// Everything else (name, role, model/provider assignment, `disabled`,
/// `is_custom`) always wins for the user entry.
fn merge_custom_over_seed(user: &CustomAgentConfig, seed: &CustomAgentConfig) -> CustomAgentConfig {
    let mut merged = user.clone();
    if merged.stage.is_none() {
        merged.stage = seed.stage.clone();
    }
    if merged.output_mode == OutputMode::default() {
        merged.output_mode = seed.output_mode;
    }
    if merged.capabilities == AgentCapabilities::default() {
        merged.capabilities = seed.capabilities.clone();
    }
    if merged.prompt_sections == PromptSections::default() {
        merged.prompt_sections = seed.prompt_sections.clone();
    }
    merged
}

/// Register every enabled agent from the merged seed/user config map, all
/// backed by [`GenericSpecialistAgent`] with the configured [`OutputMode`].
///
/// Provider resolution falls back to the default provider when the agent
/// has no role-specific assignment. The executor is shared (policy-gated),
/// matching how the built-in Coder used the same executor.
///
/// ADR-35 phase 4 + audit A-01: entries with `disabled = true` are absent
/// from the runtime topology — they are skipped entirely. The validate-stage
/// seed is registered here with the eval engine attached (gated on its
/// `stage` tag + `eval` capability, default on); the reserved `coordinator`
/// id is never registered from config.
///
/// ADR-58 P2+P3 (R9): when a resolved blueprint facade is present, each
/// agent is built from the **resolved per-agent capabilities**
/// (`facade.effective_capabilities_for` — seed `effective()` overlaid with
/// the staffing stage's write mask) instead of the raw config flags. On the
/// default `standard` blueprint this reproduces exactly the shape the parity
/// test pins (tests/parity.rs:204-221): coder → `{f,t,t,f,f,t}`, the other
/// four seeds → `{f,f,f,f,f,t}`. Facade-less builds (tests, manually
/// constructed registries) keep the pre-resolution raw-config path.
#[allow(clippy::too_many_arguments)]
fn register_seeded_agents(
    registry: &mut AgentRegistry,
    merged: &HashMap<AgentId, CustomAgentConfig>,
    get_provider: &dyn Fn(&AgentId) -> Arc<dyn LlmProvider>,
    executor: Arc<ToolExecutor>,
    bus: &EventBus,
    retry_policy: &RetryPolicy,
    eval_root: &std::path::Path,
    skills_section: &str,
    facade: Option<&BlueprintFacade>,
    // ADR-65 §3: the session-DB pool backing every registered specialist's
    // tool-evidence writer; `None` (tests, pools unavailable) disables it.
    fact_pool: Option<sqlx::SqlitePool>,
) {
    for (id, cfg) in merged {
        if cfg.disabled || id.as_str() == "coordinator" {
            continue;
        }
        // Build closure captures everything the agent needs except the
        // provider, so the registry can rebuild this agent on a different
        // provider when the fallback ladder switches (ADR-45 tier 1b). All
        // captures are owned so the closure is 'static (it outlives the
        // loop's borrowed configs).
        let id_owned = id.clone();
        let name = cfg.name.clone();
        let stage = cfg.stage.clone();
        let output_mode = cfg.output_mode;
        // R9: resolved per-agent capabilities from the blueprint facade
        // (seed `effective()` + staffing stage write mask); the raw config
        // flags when no facade is attached. Every flag is materialized to
        // `Some(..)` so `AgentCapabilities::effective()` is the identity —
        // the agent's capability set equals the resolved shape.
        let capabilities = facade
            .map(|facade| {
                let resolved = facade.effective_capabilities_for(cfg, id);
                AgentCapabilities {
                    fs_read: Some(resolved.fs_read),
                    fs_write: Some(resolved.fs_write),
                    shell: Some(resolved.shell),
                    git: Some(resolved.git),
                    lsp: Some(resolved.lsp),
                    eval: Some(resolved.eval),
                }
            })
            .unwrap_or_else(|| cfg.capabilities.clone());
        let prompt_sections = cfg.prompt_sections.clone();
        // ADR-58 F4 (Batch 4): the eval harness attaches to *verify
        // semantics*, not the literal `validate` tag — the agent's stage
        // kind must be the closed `Acceptance` kind (the default blueprint's
        // validate stage), so a renamed or custom Acceptance-kind stage agent
        // behaves the same. Without a facade (tests, manual registries) the
        // legacy tag classification is kept, byte-identical on the default
        // `standard` blueprint.
        let is_verify_stage = facade
            .and_then(|facade| {
                stage
                    .as_ref()
                    .and_then(|tag| facade.stage_kind(tag.as_str()))
                    .map(|kind| kind == StageKind::Acceptance)
            })
            .unwrap_or_else(|| stage.as_ref().is_some_and(AgentStage::is_validate));
        let skills = skills_section.to_string();
        let executor_owned = executor.clone();
        let bus_owned = bus.clone();
        let retry_policy_owned = retry_policy.clone();
        let eval_root_owned = eval_root.to_path_buf();
        let fact_pool_owned = fact_pool.clone();
        let factory_id = id_owned.clone();
        let build = move |provider: Arc<dyn LlmProvider>| -> Arc<dyn ExpertAgent> {
            // ADR-65 §3: stamp the shared pool onto this specific agent's
            // evidence writer (agent identity is never inferred).
            let fact_ctx = fact_pool_owned.clone().map(|pool| {
                crate::tool_facts::ToolFactContext::new(Some(pool), id_owned.to_string())
            });
            // Verify-stage (Acceptance-kind) agent: with the eval capability
            // on, the attached engine runs the test suite with no LLM call;
            // with it off, `with_eval(None)` still enables eval mode so the
            // run fails fast ("validation disabled") — C-06 says a build task
            // must not be accepted without real verification evidence. Keyed
            // on the stage kind (ADR-35 / ADR-58 F4) instead of the
            // `validator` role id: a renamed or custom Acceptance-stage agent
            // behaves the same. With the default five-seed config this is
            // behavior-identical (`AgentCapabilities::default()` enables eval
            // and only the validator seed carries the Acceptance-kind
            // validate stage).
            if is_verify_stage {
                let eval = capabilities
                    .effective()
                    .eval
                    .then(|| Arc::new(concerto_eval::EvalEngine::new(&eval_root_owned)));
                return Arc::new(
                    GenericSpecialistAgent::new(
                        id_owned.clone(),
                        name.clone(),
                        stage.clone(),
                        provider,
                        Some(executor_owned.clone()),
                        bus_owned.clone(),
                        retry_policy_owned.clone(),
                        prompt_sections.clone(),
                        capabilities.clone(),
                    )
                    .with_output_mode(output_mode)
                    .with_eval(eval)
                    .with_skills_section(&skills)
                    .with_tool_facts(fact_ctx),
                );
            }
            Arc::new(
                GenericSpecialistAgent::new(
                    id_owned.clone(),
                    name.clone(),
                    stage.clone(),
                    provider,
                    Some(executor_owned.clone()),
                    bus_owned.clone(),
                    retry_policy_owned.clone(),
                    prompt_sections.clone(),
                    capabilities.clone(),
                )
                .with_output_mode(output_mode)
                .with_skills_section(&skills)
                .with_tool_facts(fact_ctx),
            )
        };
        registry.register_with_factory(factory_id, build(get_provider(id)), Arc::new(build));
    }
}

/// Rebuilds an agent role bound to a different provider (ADR-45 tier 1b).
pub type AgentRebuildFactory =
    Arc<dyn Fn(Arc<dyn LlmProvider>) -> Arc<dyn ExpertAgent> + Send + Sync>;

/// Maps agent IDs to their registered implementations.
pub struct AgentRegistry {
    agents: HashMap<AgentId, Arc<dyn ExpertAgent>>,
    /// Per-role rebuild factories (ADR-45 tier 1b): reconstruct the same
    /// agent bound to a different provider, so the fallback ladder can
    /// escape a failing provider without re-registering anything.
    factories: HashMap<AgentId, AgentRebuildFactory>,
    /// Merged seed/user configs retained at build time (ADR-35 phase 4,
    /// roster enrichment): the planner reads per-agent capabilities and a
    /// human-readable description from here, so the roster reflects what each
    /// role can actually do. Mock-only registries (`from_mocks`, `register`)
    /// carry no configs and never populate this map.
    configs: HashMap<AgentId, CustomAgentConfig>,
}

impl AgentRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self { agents: HashMap::new(), factories: HashMap::new(), configs: HashMap::new() }
    }

    /// Register an agent.
    pub fn register(&mut self, agent: Arc<dyn ExpertAgent>) {
        let id = agent.id();
        self.agents.insert(id, agent);
    }

    /// Register an agent together with a factory that can rebuild it bound
    /// to a different provider. Roles registered without a factory fall back
    /// to the built agent when a provider switch is requested.
    pub fn register_with_factory(
        &mut self,
        id: AgentId,
        agent: Arc<dyn ExpertAgent>,
        factory: AgentRebuildFactory,
    ) {
        self.agents.insert(id.clone(), agent);
        self.factories.insert(id, factory);
    }

    /// Get an agent by its ID.
    pub fn get(&self, id: &AgentId) -> Option<Arc<dyn ExpertAgent>> {
        self.agents.get(id).cloned()
    }

    /// Get an agent rebuilt on the given provider (ADR-45 tier 1b). Roles
    /// without a registered factory return their built agent unchanged.
    pub fn get_with_provider(
        &self,
        id: &AgentId,
        provider: Arc<dyn LlmProvider>,
    ) -> Option<Arc<dyn ExpertAgent>> {
        self.factories
            .get(id)
            .map(|factory| factory(provider))
            .or_else(|| self.agents.get(id).cloned())
    }

    /// Whether the role has a rebuild factory. Roles without one (e.g. test
    /// mocks) keep their built agent on `get_with_provider`, so a
    /// provider-switch dispatch would silently repeat the original bound
    /// provider; the fallback ladder checks this before tier-1b/tier-2
    /// dispatches that depend on the pipe actually changing.
    pub fn has_rebuild_factory(&self, id: &AgentId) -> bool {
        self.factories.contains_key(id)
    }

    /// The merged seed/user config for a registered agent, returned only when
    /// the registry was built from agent configs (see [`Self::build_default`],
    /// [`Self::build_with_roles`], [`Self::build_with_roles_for_project`]).
    /// For an id the user overrode, this is the *merged* entry — explicit user
    /// fields win and unset lifecycle/capability defaults inherit from the
    /// seed (ADR-35 phase 4). Agent ids registered without a config (mock
    /// registries via `from_mocks`/`register`) return `None`.
    pub fn config(&self, id: &AgentId) -> Option<&CustomAgentConfig> {
        self.configs.get(id)
    }

    /// List all registered agent IDs.
    pub fn ids(&self) -> Vec<AgentId> {
        self.agents.keys().cloned().collect()
    }

    /// All registered agent IDs whose declared stage matches `stage`.
    ///
    /// ADR-35 §5: the coordinator resolves lifecycle-stage participants
    /// (design, research, implement, review, validate) from the registry
    /// instead of hardcoded role names, so a pipeline can override, add, or
    /// remove participants by configuration.
    pub fn ids_for_stage(&self, stage: &AgentStage) -> Vec<AgentId> {
        self.agents
            .iter()
            .filter(|(_, agent)| agent.stage().as_ref() == Some(stage))
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// Build the default set of specialist agents (no per-agent config).
    ///
    /// Note: a memory store was previously threaded through here for the
    /// five specialist `Agent::run(...)` calls, but the agents ingest
    /// memory chunks via `AgentContext.retrieved_chunks` populated
    /// upstream by `CoordinatorAgent::run` — no agent constructor takes
    /// a `MemoryStore` handle. See audit §3.2.
    ///
    /// `retry_policy` is shared across all agents for consistent provider
    /// retry behaviour (audit §4c).
    ///
    /// ADR-35 phase 4 + audit A-01: the five specialists all come from the
    /// built-in seed configs and are backed by [`GenericSpecialistAgent`];
    /// the validator seed's eval engine is attached here (the seed default).
    pub fn build_default(
        provider: Arc<dyn LlmProvider>,
        executor: Arc<ToolExecutor>,
        bus: EventBus,
        retry_policy: RetryPolicy,
        skills_section: &str,
    ) -> Self {
        let mut registry = Self::new();
        let empty_configs = HashMap::new();
        let merged = merged_agent_configs(&empty_configs, true);
        let get_provider = |_id: &AgentId| provider.clone();

        // No resolved blueprint facade on this construction path (its callers
        // are tests and manual registries); agents keep the raw-config seed
        // path, identical to pre-ADR-58 behavior. The facade-aware route is
        // [`AgentRegistry::build_with_roles_for_project_with_facade`].
        register_seeded_agents(
            &mut registry,
            &merged,
            &get_provider,
            executor,
            &bus,
            &retry_policy,
            std::path::Path::new("."),
            skills_section,
            None,
            None, // Adr-65 §3: no fact-writer pool on the default construction path
        );
        // Retain the merged configs so the planner roster can describe each
        // role (ADR-35 phase 4, roster enrichment).
        registry.configs = merged.clone();

        registry
    }

    /// Build specialist agents with per-role provider assignments.
    ///
    /// Each agent role gets its own provider from `role_providers`.
    /// Falls back to `default_provider` for roles not in the map.
    ///
    /// `agent_configs` provides per-agent `PromptSections` and
    /// `AgentCapabilities` from the config (or an empty map for defaults).
    ///
    /// `retry_policy` is shared across all agents for consistent provider
    /// retry behaviour (audit §4c).
    // `merge_seeds` deliberately threads config-ownership semantics (ADR-58/59
    // revision: config-owned rosters never merge back the seed set) through
    // this construction; one extra arg beats a builder for a bool we want
    // impossible to forget at every call site.
    #[allow(clippy::too_many_arguments)]
    pub fn build_with_roles(
        role_providers: HashMap<AgentId, Arc<dyn LlmProvider>>,
        default_provider: Arc<dyn LlmProvider>,
        executor: Arc<ToolExecutor>,
        bus: EventBus,
        retry_policy: RetryPolicy,
        agent_configs: &HashMap<AgentId, CustomAgentConfig>,
        skills_section: &str,
        merge_seeds: bool,
        fact_pool: Option<sqlx::SqlitePool>,
    ) -> Self {
        let merged = merged_agent_configs(agent_configs, merge_seeds);
        // Audit A-01: the eval engine is attached to the validator seed
        // inside the shared construction (gated on its `eval` capability);
        // when the validator is unconfigured it stays enabled so unmodified
        // configs behave identically.
        Self::build_registry_with(
            role_providers,
            default_provider,
            executor,
            bus,
            retry_policy,
            &merged,
            std::path::Path::new("."),
            skills_section,
            None,
            fact_pool,
        )
    }

    /// Shared specialist construction used by [`build_with_roles`] and
    /// [`build_with_roles_for_project`] (which differ only in the eval
    /// engine root). Registers every enabled agent from the merged seed/user
    /// config map; the validator seed carries the eval engine attached here
    /// so the engine is constructed exactly once.
    #[allow(clippy::too_many_arguments)]
    fn build_registry_with(
        role_providers: HashMap<AgentId, Arc<dyn LlmProvider>>,
        default_provider: Arc<dyn LlmProvider>,
        executor: Arc<ToolExecutor>,
        bus: EventBus,
        retry_policy: RetryPolicy,
        merged: &HashMap<AgentId, CustomAgentConfig>,
        eval_root: &std::path::Path,
        skills_section: &str,
        facade: Option<&BlueprintFacade>,
        fact_pool: Option<sqlx::SqlitePool>,
    ) -> Self {
        let get_provider = |id: &AgentId| -> Arc<dyn LlmProvider> {
            role_providers.get(id).cloned().unwrap_or_else(|| default_provider.clone())
        };

        let mut registry = Self::new();
        register_seeded_agents(
            &mut registry,
            merged,
            &get_provider,
            executor,
            &bus,
            &retry_policy,
            eval_root,
            skills_section,
            facade,
            fact_pool,
        );
        // Retain the merged configs so the planner roster can describe each
        // role (ADR-35 phase 4, roster enrichment). Covers
        // `build_with_roles` and `build_with_roles_for_project`, which both
        // funnel through here.
        registry.configs = merged.clone();

        registry
    }

    /// Build specialists for a concrete project root so validation targets
    /// the selected workspace instead of the process working directory.
    ///
    /// `agent_configs` provides per-agent `PromptSections` and
    /// `AgentCapabilities` from the config (or an empty map for defaults).
    #[allow(clippy::too_many_arguments)]
    pub fn build_with_roles_for_project(
        role_providers: HashMap<AgentId, Arc<dyn LlmProvider>>,
        default_provider: Arc<dyn LlmProvider>,
        executor: Arc<ToolExecutor>,
        bus: EventBus,
        retry_policy: RetryPolicy,
        project_root: &std::path::Path,
        agent_configs: &HashMap<AgentId, CustomAgentConfig>,
        skills_section: &str,
        merge_seeds: bool,
        fact_pool: Option<sqlx::SqlitePool>,
    ) -> Self {
        Self::build_with_roles_for_project_with_facade(
            role_providers,
            default_provider,
            executor,
            bus,
            retry_policy,
            project_root,
            agent_configs,
            skills_section,
            None,
            merge_seeds,
            fact_pool,
        )
    }

    /// Build specialists for a concrete project root, resolving each agent's
    /// capabilities through the ADR-58 blueprint facade (R9) when one is
    /// attached.
    ///
    /// This is the runtime construction seam: the multi-agent frontend holds
    /// the resolved blueprint (design doc §1.2) and hands it here so seeds are
    /// registered from the resolved per-agent capabilities (`facade
    /// .effective_capabilities_for`) instead of the raw config flags. Passing
    /// `None` (tests, manually constructed registries) keeps the exact
    /// pre-resolution raw-config path — byte-identical on the default
    /// `standard` blueprint.
    ///
    /// `fact_pool` (ADR-65 §3) backs every registered specialist's
    /// tool-evidence writer; `None` (tests, pools unavailable) disables it.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn build_with_roles_for_project_with_facade(
        role_providers: HashMap<AgentId, Arc<dyn LlmProvider>>,
        default_provider: Arc<dyn LlmProvider>,
        executor: Arc<ToolExecutor>,
        bus: EventBus,
        retry_policy: RetryPolicy,
        project_root: &std::path::Path,
        agent_configs: &HashMap<AgentId, CustomAgentConfig>,
        skills_section: &str,
        facade: Option<&BlueprintFacade>,
        merge_seeds: bool,
        fact_pool: Option<sqlx::SqlitePool>,
    ) -> Self {
        // Audit §3.2: `memory` was threaded in only to be forwarded to
        // `build_with_roles`, which itself ignored it (underscore-prefixed).
        // Removed from both signatures; if a future agent needs direct
        // memory-store access, that should be wired via AgentContext (where
        // `retrieved_chunks` already lives) rather than the constructor.
        let merged = merged_agent_configs(agent_configs, merge_seeds);
        // Same topology control and eval gating as `build_with_roles`, but
        // scoped to the concrete project root so validation targets the
        // selected workspace instead of the process working directory.
        Self::build_registry_with(
            role_providers,
            default_provider,
            executor,
            bus,
            retry_policy,
            &merged,
            project_root,
            skills_section,
            facade,
            fact_pool,
        )
    }
}

impl Default for AgentRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use concerto_config::{AgentCapabilities, PromptSections};
    use concerto_core::traits::agent::ExpertAgent;
    use concerto_core::traits::policy::AuditLog;
    use concerto_core::types::{AgentContext, AgentId, AgentOutcome, AgentRunResult, SubTask};
    use concerto_core::{CancellationToken, OrchestratorError};

    /// Minimal no-op audit log for building a policy engine in tests.
    struct TestAudit;

    #[async_trait::async_trait]
    impl AuditLog for TestAudit {
        async fn record(
            &self,
            _entry: concerto_core::traits::policy::AuditEntry,
            _cancel: CancellationToken,
        ) -> Result<(), concerto_core::error::PolicyError> {
            Ok(())
        }
    }

    /// A minimal agent used for registry tests.
    struct TestAgent {
        id: AgentId,
        stage: Option<AgentStage>,
    }

    #[async_trait::async_trait]
    impl ExpertAgent for TestAgent {
        fn id(&self) -> AgentId {
            self.id.clone()
        }

        fn stage(&self) -> Option<AgentStage> {
            self.stage.clone()
        }

        fn capabilities(&self) -> concerto_core::types::CapabilitySet {
            concerto_core::types::CapabilitySet::default()
        }

        async fn run(
            &self,
            _task: &SubTask,
            _context: AgentContext,
            _model: &str,
            _cancel: CancellationToken,
        ) -> Result<AgentRunResult, OrchestratorError> {
            Ok(AgentRunResult {
                task_id: concerto_core::types::TaskId::new(),
                role: self.id.clone(),
                outcome: AgentOutcome::Success,
                summary: "test".into(),
                files_modified: Vec::new(),
                tool_call_count: 0,
                cost_usd: 0.0,
                latency_ms: 0,
                provider: "test".into(),
                model: "test".into(),
                tokens_in: 0,
                tokens_out: 0,
            })
        }
    }

    #[test]
    fn new_creates_empty_registry() {
        let registry = AgentRegistry::new();
        assert!(registry.ids().is_empty());
    }

    #[test]
    fn register_and_get_round_trip() {
        let mut registry = AgentRegistry::new();
        let coder_id = AgentId::new("coder");
        let agent = Arc::new(TestAgent { id: coder_id.clone(), stage: None });
        registry.register(agent.clone());

        let retrieved = registry.get(&coder_id);
        assert!(retrieved.is_some(), "coder should be findable");
        assert_eq!(retrieved.unwrap().id(), coder_id);
    }

    #[test]
    fn ids_for_stage_filters_by_declared_stage() {
        let mut registry = AgentRegistry::new();
        registry.register(Arc::new(TestAgent {
            id: AgentId::new("coder"),
            stage: Some(AgentStage::new("implement")),
        }));
        registry.register(Arc::new(TestAgent {
            id: AgentId::new("copilot"),
            stage: Some(AgentStage::new("implement")),
        }));
        registry.register(Arc::new(TestAgent {
            id: AgentId::new("reviewer"),
            stage: Some(AgentStage::new("review")),
        }));
        registry.register(Arc::new(TestAgent { id: AgentId::new("docs-writer"), stage: None }));

        let mut implement = registry.ids_for_stage(&AgentStage::new("implement"));
        implement.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        assert_eq!(implement, vec![AgentId::new("coder"), AgentId::new("copilot")]);

        let review = registry.ids_for_stage(&AgentStage::new("review"));
        assert_eq!(review, vec![AgentId::new("reviewer")]);

        let design = registry.ids_for_stage(&AgentStage::new("design"));
        assert!(design.is_empty(), "no design-stage agent registered");

        // Stage-less (freeform) agents never match a lifecycle stage.
        let implement_again = registry.ids_for_stage(&AgentStage::new("implement"));
        assert!(!implement_again.contains(&AgentId::new("docs-writer")));
    }

    #[test]
    fn get_unregistered_id_returns_none() {
        let registry = AgentRegistry::new();
        assert!(registry.get(&AgentId::new("architect")).is_none());
    }

    #[test]
    fn ids_returns_all_registered() {
        let mut registry = AgentRegistry::new();
        registry.register(Arc::new(TestAgent { id: AgentId::new("coder"), stage: None }));
        registry.register(Arc::new(TestAgent { id: AgentId::new("reviewer"), stage: None }));

        let mut ids = registry.ids();
        ids.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&AgentId::new("coder")));
        assert!(ids.contains(&AgentId::new("reviewer")));
    }

    #[test]
    fn register_overwrites_existing_id() {
        let mut registry = AgentRegistry::new();
        let coder_id = AgentId::new("coder");
        registry.register(Arc::new(TestAgent { id: coder_id.clone(), stage: None }));
        registry.register(Arc::new(TestAgent { id: coder_id, stage: None })); // overwrite

        let ids = registry.ids();
        assert_eq!(ids.len(), 1, "only one coder remains after overwrite");
    }

    #[test]
    fn multiple_ids_dont_interfere() {
        let mut registry = AgentRegistry::new();
        registry.register(Arc::new(TestAgent { id: AgentId::new("architect"), stage: None }));
        registry.register(Arc::new(TestAgent { id: AgentId::new("researcher"), stage: None }));
        registry.register(Arc::new(TestAgent { id: AgentId::new("coder"), stage: None }));
        registry.register(Arc::new(TestAgent { id: AgentId::new("reviewer"), stage: None }));
        registry.register(Arc::new(TestAgent { id: AgentId::new("validator"), stage: None }));

        assert_eq!(registry.ids().len(), 5);
        for id in &[
            AgentId::new("architect"),
            AgentId::new("researcher"),
            AgentId::new("coder"),
            AgentId::new("reviewer"),
            AgentId::new("validator"),
        ] {
            assert!(registry.get(id).is_some(), "agent {id} should be registered");
        }
    }

    #[test]
    fn custom_config_entries_are_registered_as_generic_agents() {
        use concerto_core::types::AgentStage;

        let mut configs = HashMap::new();
        // Known id: overlaid on the built-in seed — explicit prompt sections
        // win, unset lifecycle fields (stage, output mode, capabilities)
        // inherit from the seed.
        configs.insert(
            AgentId::new("coder"),
            CustomAgentConfig {
                id: "coder".into(),
                name: "Coder".into(),
                role: "coder".into(),
                stage: None,
                prompt_sections: PromptSections {
                    system_instructions: "custom coder".into(),
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        // Unknown id: backed by GenericSpecialistAgent with its stage tag.
        configs.insert(
            AgentId::new("docs-writer"),
            CustomAgentConfig {
                id: "docs-writer".into(),
                name: "Docs Writer".into(),
                role: "docs-writer".into(),
                stage: Some(AgentStage::new("documentation")),
                prompt_sections: PromptSections::default(),
                model_override: None,
                provider_id: None,
                capabilities: AgentCapabilities::default(),
                is_custom: true,
                disabled: false,
                output_mode: concerto_core::types::OutputMode::default(),
            },
        );
        // Reserved coordinator id is never registered from config.
        configs.insert(
            AgentId::new("coordinator"),
            CustomAgentConfig {
                id: "coordinator".into(),
                name: "Evil Coordinator".into(),
                role: "coordinator".into(),
                stage: None,
                prompt_sections: PromptSections::default(),
                model_override: None,
                provider_id: None,
                capabilities: AgentCapabilities::default(),
                is_custom: true,
                disabled: false,
                output_mode: concerto_core::types::OutputMode::default(),
            },
        );

        let provider = Arc::new(concerto_providers::mock::MockProvider::default());
        let executor = Arc::new(ToolExecutor::new(
            Arc::new(concerto_core::types::ToolRegistry::default()),
            Arc::new(concerto_core::policy::SimplePolicyEngine::new(
                Vec::new(),
                Arc::new(TestAudit),
            )),
        ));
        let registry = AgentRegistry::build_with_roles(
            HashMap::new(),
            provider,
            executor,
            EventBus::new(128),
            RetryPolicy::default(),
            &configs,
            "",
            true,
            None, // no fact-writer pool in this test
        );

        let docs =
            registry.get(&AgentId::new("docs-writer")).expect("custom agent should be registered");
        assert_eq!(docs.id(), AgentId::new("docs-writer"));
        assert_eq!(docs.stage().map(|stage| stage.to_string()), Some("documentation".to_string()));
        // Built-in specialists are still present.
        assert!(registry.get(&AgentId::new("architect")).is_some());
        assert!(registry.get(&AgentId::new("coder")).is_some());
        // Reserved id must not create an agent.
        assert!(registry.get(&AgentId::new("coordinator")).is_none());
    }

    fn build_test_registry(configs: &HashMap<AgentId, CustomAgentConfig>) -> AgentRegistry {
        let provider = Arc::new(concerto_providers::mock::MockProvider::default());
        let executor = Arc::new(ToolExecutor::new(
            Arc::new(concerto_core::types::ToolRegistry::default()),
            Arc::new(concerto_core::policy::SimplePolicyEngine::new(
                Vec::new(),
                Arc::new(TestAudit),
            )),
        ));
        AgentRegistry::build_with_roles(
            HashMap::new(),
            provider,
            executor,
            EventBus::new(128),
            RetryPolicy::default(),
            configs,
            "",
            true,
            None, // no fact-writer pool in this test
        )
    }

    #[test]
    fn config_returns_merged_entry_for_config_backed_registry() {
        // ADR-35 phase 4, roster enrichment: the registry retains the MERGED
        // seed/user configs so the planner roster can describe what each role
        // can actually do. A user entry that only sets a model override is the
        // common Studio shape; the merged entry must keep the override while
        // inheriting the seed's lifecycle/capability defaults.
        let mut configs = HashMap::new();
        configs.insert(
            AgentId::new("coder"),
            CustomAgentConfig {
                id: "coder".into(),
                name: "Coder".into(),
                role: "coder".into(),
                model_override: Some("gpt-4o".into()),
                ..Default::default()
            },
        );

        let registry = build_test_registry(&configs);

        let coder =
            registry.config(&AgentId::new("coder")).expect("merged coder config must be retained");
        // Explicit user fields win.
        assert_eq!(coder.model_override.as_deref(), Some("gpt-4o"));
        // Unset lifecycle/capability defaults inherit from the seed.
        assert_eq!(coder.stage.as_ref().map(|s| s.as_str()), Some(AgentStage::IMPLEMENT));
        let seeds = builtin_agent_seeds();
        let seed = seeds.iter().find(|seed| seed.id == "coder").expect("coder seed");
        assert_eq!(
            coder.capabilities, seed.capabilities,
            "a model-only override must inherit the seed's capabilities"
        );
        assert_eq!(coder.capabilities.fs_write, seed.capabilities.fs_write);
        // Unknown ids and mock-only (config-free) registries return None.
        assert!(registry.config(&AgentId::new("ghost")).is_none());
        let mock_registry = AgentRegistry::new();
        assert!(mock_registry.config(&AgentId::new("coder")).is_none());
    }

    #[test]
    fn disabled_builtin_specialist_is_not_registered() {
        // ADR-35 phase 4: topology control — a disabled built-in specialist
        // (reviewer) is absent from the runtime registry.
        let mut configs = HashMap::new();
        configs.insert(
            AgentId::new("reviewer"),
            CustomAgentConfig {
                id: "reviewer".into(),
                name: "Reviewer".into(),
                role: "reviewer".into(),
                disabled: true,
                ..Default::default()
            },
        );

        let registry = build_test_registry(&configs);

        assert!(
            registry.get(&AgentId::new("reviewer")).is_none(),
            "disabled built-in specialist must not be registered"
        );
        assert!(
            registry.get(&AgentId::new("coder")).is_some(),
            "non-disabled built-in specialists remain registered"
        );
        assert!(
            registry.get(&AgentId::new("validator")).is_some(),
            "non-disabled built-in specialists remain registered"
        );
    }

    #[test]
    fn disabled_custom_agent_is_not_registered() {
        // ADR-35 phase 4: a disabled custom agent never appears in the
        // runtime registry.
        let mut configs = HashMap::new();
        configs.insert(
            AgentId::new("docs-writer"),
            CustomAgentConfig {
                id: "docs-writer".into(),
                name: "Docs Writer".into(),
                role: "docs-writer".into(),
                disabled: true,
                ..Default::default()
            },
        );

        let registry = build_test_registry(&configs);

        assert!(
            registry.get(&AgentId::new("docs-writer")).is_none(),
            "disabled custom agent must not be registered"
        );
        // Enabled custom agents are still registered.
        let mut enabled_configs = configs.clone();
        enabled_configs.insert(
            AgentId::new("copilot"),
            CustomAgentConfig {
                id: "copilot".into(),
                name: "Copilot".into(),
                role: "copilot".into(),
                ..Default::default()
            },
        );
        let registry = build_test_registry(&enabled_configs);
        assert!(registry.get(&AgentId::new("copilot")).is_some());
        assert!(registry.get(&AgentId::new("docs-writer")).is_none());
    }

    // ------------------------------------------------------------------
    // Audit A-01: all five seeds are generic-backed
    // ------------------------------------------------------------------

    #[test]
    fn default_registry_registers_all_five_seeds() {
        let provider = Arc::new(concerto_providers::mock::MockProvider::default());
        let executor = Arc::new(ToolExecutor::new(
            Arc::new(concerto_core::types::ToolRegistry::default()),
            Arc::new(concerto_core::policy::SimplePolicyEngine::new(
                Vec::new(),
                Arc::new(TestAudit),
            )),
        ));
        let registry = AgentRegistry::build_default(
            provider,
            executor,
            EventBus::new(128),
            RetryPolicy::default(),
            "",
        );

        for id in ["architect", "researcher", "coder", "reviewer", "validator"] {
            assert!(registry.get(&AgentId::new(id)).is_some(), "seed {id} must be registered");
        }
    }

    #[tokio::test]
    async fn coder_seed_is_generic_backed_freeform() {
        // The retired CoderAgent failed when no file was modified; the
        // generic Freeform backing reports Success for any terminal text.
        // This is the accepted interim until the C-06 acceptance work lands.
        let registry = build_test_registry(&HashMap::new());
        let coder = registry.get(&AgentId::new("coder")).expect("coder seed registered");
        assert_eq!(coder.stage().map(|s| s.to_string()), Some("implement".to_string()));

        let session = concerto_core::types::SessionContext::new(
            concerto_core::ids::Ulid::new(),
            std::env::temp_dir(),
        );
        let task = SubTask {
            id: concerto_core::types::TaskId::new(),
            parent_id: None,
            session_id: session.session_id,
            role: AgentId::new("coder"),
            description: "Implement the feature".into(),
            status: concerto_core::types::SubTaskStatus::Pending,
            dependencies: Vec::new(),
            deliverable: None,
            created_at: time::OffsetDateTime::now_utc(),
            completed_at: None,
        };

        // MockProvider returns an empty final chunk with no tool calls.
        let result = coder
            .run(&task, AgentContext::new(session), "mock-model", CancellationToken::new())
            .await
            .expect("run should succeed");

        assert!(
            matches!(result.outcome, AgentOutcome::Success),
            "generic Freeform coder reports Success for any terminal text (interim until C-06)"
        );
        assert!(result.files_modified.is_empty());
        assert_eq!(result.role, AgentId::new("coder"));
    }

    #[tokio::test]
    async fn validator_seed_attaches_eval_engine_and_runs_it() {
        // The registry attaches the eval engine to the validator seed; a
        // project-rooted engine targets a temp Makefile project so the run
        // is deterministic (no cargo invocation).
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Makefile"),
            "test:\n\t@echo \"all tests passed\"\n\t@exit 0\n",
        )
        .unwrap();

        let provider = Arc::new(concerto_providers::mock::MockProvider::default());
        let executor = Arc::new(ToolExecutor::new(
            Arc::new(concerto_core::types::ToolRegistry::default()),
            Arc::new(concerto_core::policy::SimplePolicyEngine::new(
                Vec::new(),
                Arc::new(TestAudit),
            )),
        ));
        let registry = AgentRegistry::build_with_roles_for_project(
            HashMap::new(),
            provider,
            executor,
            EventBus::new(128),
            RetryPolicy::default(),
            dir.path(),
            &HashMap::new(),
            "",
            true,
            None, // no fact-writer pool in this test
        );

        let validator =
            registry.get(&AgentId::new("validator")).expect("validator seed registered");
        assert_eq!(validator.stage().map(|s| s.to_string()), Some("validate".to_string()));

        let session = concerto_core::types::SessionContext::new(
            concerto_core::ids::Ulid::new(),
            dir.path().to_path_buf(),
        );
        let task = SubTask {
            id: concerto_core::types::TaskId::new(),
            parent_id: None,
            session_id: session.session_id,
            role: AgentId::new("validator"),
            description: "Run validation".into(),
            status: concerto_core::types::SubTaskStatus::Pending,
            dependencies: Vec::new(),
            deliverable: None,
            created_at: time::OffsetDateTime::now_utc(),
            completed_at: None,
        };

        let result = validator
            .run(&task, AgentContext::new(session), "", CancellationToken::new())
            .await
            .expect("run should succeed");

        assert!(matches!(result.outcome, AgentOutcome::Success));
        assert!(
            result.summary.starts_with("Pass:"),
            "validator seed output_format must enable the Pass/Fail prefix: {}",
            result.summary
        );
    }

    #[tokio::test]
    async fn facade_keyed_acceptance_stage_attaches_eval_to_validator() {
        // ADR-58 F4: with the resolved standard blueprint's facade attached,
        // the eval engine still attaches to the validator by *Acceptance-kind*
        // verify semantics (the blueprint's validate stage) — byte-identical
        // to the legacy `is_validate` tag keying.
        let resolved = concerto_config::OrchestrationConfig::default()
            .resolve(&[], None)
            .expect("the standard blueprint must validate and resolve");
        let facade = BlueprintFacade::new(&resolved);

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Makefile"),
            "test:\n\t@echo \"all tests passed\"\n\t@exit 0\n",
        )
        .unwrap();

        let provider = Arc::new(concerto_providers::mock::MockProvider::default());
        let executor = Arc::new(ToolExecutor::new(
            Arc::new(concerto_core::types::ToolRegistry::default()),
            Arc::new(concerto_core::policy::SimplePolicyEngine::new(
                Vec::new(),
                Arc::new(TestAudit),
            )),
        ));
        let registry = AgentRegistry::build_with_roles_for_project_with_facade(
            HashMap::new(),
            provider,
            executor,
            EventBus::new(128),
            RetryPolicy::default(),
            dir.path(),
            &HashMap::new(),
            "",
            Some(&facade),
            true,
            None, // no fact-writer pool in this test
        );

        let validator =
            registry.get(&AgentId::new("validator")).expect("validator seed registered");
        let session = concerto_core::types::SessionContext::new(
            concerto_core::ids::Ulid::new(),
            dir.path().to_path_buf(),
        );
        let task = SubTask {
            id: concerto_core::types::TaskId::new(),
            parent_id: None,
            session_id: session.session_id,
            role: AgentId::new("validator"),
            description: "Run validation".into(),
            status: concerto_core::types::SubTaskStatus::Pending,
            dependencies: Vec::new(),
            deliverable: None,
            created_at: time::OffsetDateTime::now_utc(),
            completed_at: None,
        };

        let result = validator
            .run(&task, AgentContext::new(session), "", CancellationToken::new())
            .await
            .expect("run should succeed");

        assert!(matches!(result.outcome, AgentOutcome::Success));
        assert!(
            result.summary.starts_with("Pass:"),
            "Acceptance-kind keying must attach the eval engine: {}",
            result.summary
        );
    }

    #[tokio::test]
    async fn eval_disabled_validate_stage_agent_fails_fast_through_registry() {
        // Stage-tag keyed (ADR-35): a *custom* validate-stage role whose id
        // is not "validator" still enters the eval path, and an eval-disabled
        // validate-stage agent fails fast at run time instead of silently
        // validating by LLM opinion — C-06: a build task must not be accepted
        // without real verification evidence.
        let mut configs = HashMap::new();
        configs.insert(
            AgentId::new("auditor"),
            CustomAgentConfig {
                id: "auditor".into(),
                name: "Auditor".into(),
                role: "auditor".into(),
                stage: Some(AgentStage::new("validate")),
                capabilities: AgentCapabilities { eval: Some(false), ..Default::default() },
                ..Default::default()
            },
        );

        let registry = build_test_registry(&configs);
        let auditor = registry.get(&AgentId::new("auditor")).expect("auditor registered");

        let session = concerto_core::types::SessionContext::new(
            concerto_core::ids::Ulid::new(),
            std::env::temp_dir(),
        );
        let task = SubTask {
            id: concerto_core::types::TaskId::new(),
            parent_id: None,
            session_id: session.session_id,
            role: AgentId::new("auditor"),
            description: "Run validation".into(),
            status: concerto_core::types::SubTaskStatus::Pending,
            dependencies: Vec::new(),
            deliverable: None,
            created_at: time::OffsetDateTime::now_utc(),
            completed_at: None,
        };

        let error = auditor
            .run(&task, AgentContext::new(session), "", CancellationToken::new())
            .await
            .expect_err("an eval-disabled validate-stage agent must fail fast");
        let message = error.to_string();
        assert!(
            message.contains("validation disabled"),
            "fail-fast error must flag disabled validation: {message}"
        );
    }

    // ------------------------------------------------------------------
    // Regression: partial user overrides must inherit seed lifecycle
    // defaults. The Studio persists a `custom_agents` entry for every
    // studio agent (built-ins included), so entries that only set a model
    // assignment shadow their seed at runtime. Without inheritance, an
    // entry lacking an explicit `stage` removed the implement-stage agent
    // and every Build failed instantly with "no implementation-stage agent
    // is registered; cannot plan implementation work".
    // ------------------------------------------------------------------

    #[test]
    fn stage_less_user_override_preserves_seed_stage() {
        // A Studio-written entry for the built-in coder carrying only the
        // per-agent model assignment (no stage tag).
        let mut configs = HashMap::new();
        configs.insert(
            AgentId::new("coder"),
            CustomAgentConfig {
                id: "coder".into(),
                name: "Coder".into(),
                role: "coder".into(),
                stage: None,
                model_override: Some("gpt-4o".into()),
                ..Default::default()
            },
        );

        let registry = build_test_registry(&configs);
        let implement = registry.ids_for_stage(&AgentStage::new(AgentStage::IMPLEMENT));
        assert!(
            implement.contains(&AgentId::new("coder")),
            "a stage-less user override must inherit the coder seed's implement stage: {implement:?}"
        );
        // Every lifecycle stage must still resolve so a session can start.
        for (id, stage) in [
            ("architect", AgentStage::DESIGN),
            ("researcher", AgentStage::RESEARCH),
            ("coder", AgentStage::IMPLEMENT),
            ("reviewer", AgentStage::REVIEW),
            ("validator", AgentStage::VALIDATE),
        ] {
            let agent = registry.get(&AgentId::new(id)).expect("seed registered");
            assert_eq!(
                agent.stage().as_ref().map(|s| s.as_str()),
                Some(stage),
                "{id} must keep its lifecycle stage under a stage-less override"
            );
        }
    }

    // Regression: the complete Studio-serialized config shape from a user
    // config written before the staged pipeline (predates the ladder
    // change). Every built-in has a `custom_agents` entry that replaces its
    // seed wholesale: no `stage` key at all, `output_mode = "freeform"` on
    // every entry, but concrete capabilities and per-agent model
    // assignments. The merged result must keep the full lifecycle, restore
    // the structured output modes, and keep the user's capabilities/model
    // pins intact.
    #[test]
    fn pre_ladder_full_studio_config_resolves_full_lifecycle() {
        let caps = |fs_read, fs_write, shell, git, lsp, eval| AgentCapabilities {
            fs_read: Some(fs_read),
            fs_write: Some(fs_write),
            shell: Some(shell),
            git: Some(git),
            lsp: Some(lsp),
            eval: Some(eval),
        };
        let entry = |id: &str, role: &str, capabilities, model: Option<&str>| CustomAgentConfig {
            id: id.into(),
            name: id.to_string(),
            role: role.into(),
            stage: None,                       // absent in pre-ladder configs
            output_mode: OutputMode::Freeform, // serialized as freeform everywhere
            capabilities,
            model_override: model.map(String::from),
            ..Default::default()
        };
        let mut configs = HashMap::new();
        // `coordinator` is reserved and never registered from config, but
        // the Studio persists an entry for it; the merge must tolerate it.
        configs.insert(
            AgentId::new("coordinator"),
            entry(
                "coordinator",
                "coordinator",
                caps(true, false, false, false, false, true),
                Some("coordinator-model"),
            ),
        );
        configs.insert(
            AgentId::new("architect"),
            entry(
                "architect",
                "architect",
                caps(true, false, false, false, true, false),
                Some("architect-model"),
            ),
        );
        configs.insert(
            AgentId::new("researcher"),
            entry(
                "researcher",
                "researcher",
                caps(true, false, false, false, false, false),
                Some("researcher-model"),
            ),
        );
        configs.insert(
            AgentId::new("coder"),
            entry("coder", "coder", caps(true, true, true, true, true, true), Some("coder-model")),
        );
        configs.insert(
            AgentId::new("reviewer"),
            entry(
                "reviewer",
                "reviewer",
                caps(true, false, false, true, true, false),
                Some("reviewer-model"),
            ),
        );
        configs.insert(
            AgentId::new("validator"),
            entry(
                "validator",
                "validator",
                caps(true, false, true, false, false, true),
                Some("validator-model"),
            ),
        );

        let merged = merged_agent_configs(&configs, true);
        let registry = build_test_registry(&configs);

        // Every lifecycle stage resolves; a session can start and reach
        // validation instead of dying at planning time.
        for (id, stage) in [
            ("architect", AgentStage::DESIGN),
            ("researcher", AgentStage::RESEARCH),
            ("coder", AgentStage::IMPLEMENT),
            ("reviewer", AgentStage::REVIEW),
            ("validator", AgentStage::VALIDATE),
        ] {
            let agent = registry.get(&AgentId::new(id)).expect("seed registered");
            assert_eq!(
                agent.stage().as_ref().map(|s| s.as_str()),
                Some(stage),
                "{id} must inherit its seed stage under a pre-ladder override"
            );
            let cfg = &merged[&AgentId::new(id)];
            assert_eq!(
                cfg.output_mode,
                builtin_agent_seeds().iter().find(|s| s.id == id).unwrap().output_mode,
                "{id} must inherit the seed's structured output mode"
            );
            // The coordinator entry exists in config but is reserved.
            assert!(
                registry.get(&AgentId::new("coordinator")).is_none(),
                "coordinator must never be registered from config"
            );
        }

        // Concrete user fields keep winning: capabilities and model pins.
        assert_eq!(
            merged[&AgentId::new("coder")].capabilities,
            caps(true, true, true, true, true, true)
        );
        assert_eq!(
            merged[&AgentId::new("validator")].model_override.as_deref(),
            Some("validator-model")
        );
        assert_eq!(
            merged[&AgentId::new("architect")].model_override.as_deref(),
            Some("architect-model")
        );
    }

    #[test]
    fn merge_inherits_unset_fields_from_seed_but_keeps_explicit_ones() {
        let seeds: HashMap<String, CustomAgentConfig> =
            builtin_agent_seeds().into_iter().map(|cfg| (cfg.id.clone(), cfg)).collect();

        // Partial override of the architect: only a model assignment is set.
        let partial = merge_custom_over_seed(
            &CustomAgentConfig {
                id: "architect".into(),
                name: "Architect".into(),
                role: "architect".into(),
                model_override: Some("claude-3.5".into()),
                ..Default::default()
            },
            seeds.get("architect").unwrap(),
        );
        assert_eq!(partial.stage.as_ref().map(|s| s.to_string()), Some("design".to_string()));
        assert_eq!(partial.output_mode, OutputMode::DesignDoc);
        assert_eq!(partial.capabilities, seeds["architect"].capabilities);
        assert_eq!(partial.prompt_sections, seeds["architect"].prompt_sections);
        assert_eq!(partial.model_override.as_deref(), Some("claude-3.5"));

        // Explicit user fields still win; unset fields still inherit.
        let explicit = merge_custom_over_seed(
            &CustomAgentConfig {
                id: "reviewer".into(),
                name: "Reviewer".into(),
                role: "reviewer".into(),
                stage: Some(AgentStage::new("quality")),
                disabled: true,
                output_mode: OutputMode::DesignDoc,
                ..Default::default()
            },
            seeds.get("reviewer").unwrap(),
        );
        assert_eq!(explicit.stage.as_ref().map(|s| s.to_string()), Some("quality".to_string()));
        assert!(explicit.disabled, "explicitly disabled must win");
        assert_eq!(explicit.output_mode, OutputMode::DesignDoc);
        assert_eq!(explicit.capabilities, seeds["reviewer"].capabilities);
        assert_eq!(explicit.prompt_sections, seeds["reviewer"].prompt_sections);

        // A left-at-default output mode inherits the seed's structured mode
        // (stale configs omit the field and must keep known-working defaults).
        let default_mode = merge_custom_over_seed(
            &CustomAgentConfig {
                id: "reviewer".into(),
                name: "Reviewer".into(),
                role: "reviewer".into(),
                ..Default::default()
            },
            seeds.get("reviewer").unwrap(),
        );
        assert_eq!(default_mode.output_mode, OutputMode::ReviewReport);

        // A genuinely custom (non-seed) id passes through untouched: no seed
        // inheritance, no injected defaults.
        let mut configs = HashMap::new();
        configs.insert(
            AgentId::new("copilot"),
            CustomAgentConfig {
                id: "copilot".into(),
                name: "Copilot".into(),
                role: "copilot".into(),
                stage: None,
                ..Default::default()
            },
        );
        let merged = merged_agent_configs(&configs, true);
        assert_eq!(merged[&AgentId::new("copilot")].stage, None);
        assert_eq!(merged[&AgentId::new("copilot")].prompt_sections, PromptSections::default());
        // Seeds untouched when not overridden.
        assert_eq!(
            merged[&AgentId::new("coder")].stage.as_ref().map(|s| s.to_string()),
            Some("implement".to_string())
        );
    }

    #[test]
    fn config_owned_roster_never_resurrects_seed_agents() {
        // Maintainer revision of ADR-58/59: once the config declares a
        // roster (custom_agents non-empty OR [orchestration] present),
        // the config IS the roster. A deleted seed id (e.g. "reviewer")
        // must NOT come back from the seed set at runtime.
        let mut configs = HashMap::new();
        configs.insert(
            AgentId::new("architect"),
            CustomAgentConfig {
                id: "architect".into(),
                name: "Architect v2".into(),
                role: "architect".into(),
                ..Default::default()
            },
        );
        // Ownership mode: no seed merge.
        let merged = merged_agent_configs(&configs, false);
        assert_eq!(merged.len(), 1, "roster is exactly the configured agents");
        assert_eq!(merged[&AgentId::new("architect")].name, "Architect v2");
        // Deleted seeds (reviewer/coder/researcher/validator) stay deleted.
        for deleted in ["reviewer", "coder", "researcher", "validator"] {
            assert!(
                !merged.contains_key(&AgentId::new(deleted)),
                "deleted seed '{deleted}' must not be resurrected in owned mode"
            );
        }
        // Empty owned roster (all agents deleted, orchestration present):
        // an empty map stays empty — no seed fallback.
        let empty: HashMap<AgentId, CustomAgentConfig> = HashMap::new();
        let merged_empty = merged_agent_configs(&empty, false);
        assert!(merged_empty.is_empty(), "owned empty roster must register nothing");
    }
}
