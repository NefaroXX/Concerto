//! `concerto health` — offline, deterministic "config-first catalog" report.
//!
//! Prints the *resolved* model/provider stack: the effective providers, the
//! routing profiles the runtime would build from them, the per-agent model
//! assignments, the tier-1 fallback target (ADR-42/45), and the `[context]`
//! budget. Everything is derived from the merged config — no network calls,
//! no model discovery, no keyring writes.
//!
//! The `--json` variant emits the same report through serde. `serde` and
//! `serde_json` are already direct `concerto-cli` dependencies (workspace
//! `serde` carries the `derive` feature), so no new dependencies were needed.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::Path;

use concerto_config::{
    builtin_agent_seeds, BlueprintFacade, ContextConfig, CredentialStore, CustomAgentConfig,
    ModelSettings, MultiAgentConfig, ProviderConfig,
};
use concerto_core::types::AgentId;
use concerto_providers::factory::ProviderFactory;
use serde::Serialize;

/// Embedded single-agent context budget defaults (ADR-048), mirrored from
/// `context_compaction.rs` so the report can show resolved (not raw) values.
const DEFAULT_TRIGGER_TOKENS: u64 = 16_000;
const DEFAULT_RETAIN_USER_TURNS: usize = 4;
const DEFAULT_MINIMUM_USER_TURNS: usize = 6;

/// Offline health snapshot of the resolved config-first catalog.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HealthReport {
    /// One entry per configured `model_settings.providers` entry.
    pub configured_providers: Vec<ProviderSummary>,
    /// Routing profiles built by [`ProviderFactory::build_profiles`], sorted
    /// by cost (then model) for deterministic output.
    pub routing_profiles: Vec<ProfileSummary>,
    /// Per-role model/provider assignment status, including the known default
    /// tool-calling roles.
    pub assignments: Vec<AssignmentSummary>,
    /// Resolved tier-1 fallback target for the coordinator ladder (ADR-42/45).
    pub tier1: Tier1Summary,
    /// Path of the config file that was actually loaded, or `None` when the
    /// report reflects pure defaults (no usable file found).
    pub config_path: Option<String>,
    /// Resolved `[context]` budget.
    pub context: ContextSummary,
    /// Offline store-status view of the local SQLite stores (ADR-54).
    pub stores: StoreSummary,
}

/// Offline status of one local SQLite store (ADR-54).
///
/// Derived from read-only file inspection: the SQLite magic header and any
/// `.corrupt-<ts>.bak` quarantines beside the store. No database is opened
/// and nothing is written — `concerto health` stays deterministic and safe.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StoreStatus {
    /// Absolute path of the store file.
    pub path: String,
    /// One of `ok`, `absent (created on first open)`, or
    /// `corrupt (rebuilt on next open)`.
    pub state: String,
    /// `true` when the file starts with the SQLite magic header.
    pub sqlite_header_valid: bool,
    /// Names of `.corrupt-<ts>.bak` backups found next to the store.
    pub quarantined_backups: Vec<String>,
}

/// Store view of the health report.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StoreSummary {
    /// The app data directory holding the stores.
    pub data_dir: String,
    /// The sessions database (`sessions.db`).
    pub sessions: StoreStatus,
    /// The project-memory database (`memory/memory.db`).
    pub memory: StoreStatus,
}

/// Per-provider view for the health report.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ProviderSummary {
    /// `ProviderConfig.id` (auto-repaired on load).
    pub id: String,
    /// User-friendly `ProviderConfig.name`.
    pub name: String,
    /// Provider type id ("openai", "anthropic", ...).
    pub provider: String,
    /// Primary model of this provider config.
    pub model: String,
    /// Additional model names this config advertises (`extra_models`).
    pub extra_models: Vec<String>,
    /// Custom API base URL, `None` for the provider's built-in endpoint.
    pub api_base: Option<String>,
    /// Resolved reasoning-echo policy: `"always"`, `"if-present"`, or
    /// `"default"`. The OpenCode Zen family reports `"always (built-in)"`
    /// because its built-in default already emits reasoning content and the
    /// config dial is a no-op there.
    pub echo: String,
    /// Whether the provider config opts into Anthropic cache breakpoints.
    pub cache_breakpoints: bool,
    /// `"present"` when a key resolves from env/keyring/legacy, else `"absent"`.
    pub key_status: String,
}

/// One routing profile the orchestrator would build from the config.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ProfileSummary {
    /// Provider type id.
    pub provider: String,
    /// Primary model of the provider config this profile was built from.
    pub model: String,
    /// Estimated blended cost per 1,000 tokens in USD.
    pub cost_per_1k: f64,
    /// Context window in tokens.
    pub context_window: u32,
    /// Whether the profile supports tool/function calling.
    pub tool_calling: bool,
    /// Custom API base URL, if any.
    pub base_url: Option<String>,
}

/// Model/provider resolution for one agent role.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AssignmentSummary {
    /// Agent role (e.g. "coder").
    pub role: String,
    /// Resolved model for the role, or `"(unset)"` when nothing is configured.
    pub model: String,
    /// Resolved provider config id, or `"(default)"` for an unassigned role.
    pub provider: String,
    /// `true` when the role has an explicit `[model_settings.agent_assignments]`
    /// entry; `false` for a known default tool-calling role without one.
    pub explicit: bool,
    /// `true` when a routing profile serves the resolved model.
    pub supported: bool,
}

/// Resolved coordinator fallback-ladder tier-1 target (ADR-42/45).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Tier1Summary {
    /// The resolved tier-1 model, `None` when the tier is disabled.
    pub model: Option<String>,
    /// Which config knob supplied the model:
    /// `"[multi_agent].default_model"` or `"global_default_model"`.
    pub source: Option<String>,
    /// Whether a routing profile serves the model.
    pub served: bool,
    /// Whether the serving profile supports tool calling (meaningful when
    /// `served` is true).
    pub tool_calling: bool,
}

/// Resolved `[context]` budget for the single-agent loop.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ContextSummary {
    /// Token budget that triggers deterministic compaction.
    pub trigger_tokens: u64,
    /// Most-recent user turns always retained verbatim after compaction.
    pub retain_user_turns: usize,
    /// Minimum user turns before compaction may fire.
    pub minimum_user_turns: usize,
    /// `true` when no `[context]` section was configured and the engine's
    /// embedded defaults (16000 / 4 / 6) apply.
    pub from_defaults: bool,
}

/// Collect the health report for the resolved config (defaults on error).
///
/// `config_path` (optional) overrides the default config location; otherwise
/// [`concerto_config::default_config_path`] is used. `project_root` feeds the
/// project-scoped config layer like the rest of the CLI. Unreadable or absent
/// files fall back to defaults, mirroring `config doctor`.
pub fn collect_health(config_path: Option<&Path>, project_root: Option<&Path>) -> HealthReport {
    let resolved_path =
        config_path.map(Path::to_path_buf).or_else(concerto_config::default_config_path);
    let config =
        concerto_config::load_config(resolved_path.as_ref(), project_root).unwrap_or_default();
    // ADR-58 P2+P3 (R12): the resolved blueprint attached at load is the
    // tool-calling-role authority (`BlueprintFacade::tool_calling_roles`);
    // a config object not built through the load seam carries no blueprint,
    // so the legacy literal set is used there instead.
    let facade = config.resolved_blueprint.as_deref().map(BlueprintFacade::new);

    let settings = config.model_settings;
    let creds = CredentialStore::new();
    let profiles = profile_summaries(settings.as_ref());
    HealthReport {
        configured_providers: provider_summaries(settings.as_ref(), &creds),
        routing_profiles: profiles.clone(),
        assignments: assignment_summaries(
            settings.as_ref(),
            &profiles,
            config.multi_agent.as_ref(),
            facade.as_ref(),
        ),
        tier1: tier1_summary(config.multi_agent.as_ref(), settings.as_ref(), &profiles),
        config_path: resolved_path
            .as_deref()
            .filter(|path| path.is_file())
            .map(|path| path.display().to_string()),
        context: context_summary(config.context.as_ref()),
        stores: collect_store_status(),
    }
}

/// Read-only probe of the local SQLite stores (ADR-54).
fn collect_store_status() -> StoreSummary {
    let data_dir = match concerto_sessions::app_data_dir() {
        Ok(dir) => dir,
        Err(error) => {
            let unavailable = StoreStatus {
                path: "(unavailable)".into(),
                state: "unavailable".into(),
                sqlite_header_valid: false,
                quarantined_backups: Vec::new(),
            };
            return StoreSummary {
                data_dir: format!("(unavailable: {error})"),
                sessions: unavailable.clone(),
                memory: unavailable,
            };
        }
    };
    StoreSummary {
        data_dir: data_dir.display().to_string(),
        sessions: probe_store(data_dir.join("sessions.db")),
        memory: probe_store(data_dir.join("memory").join("memory.db")),
    }
}

/// Inspect one store file: existence, SQLite magic header, and any
/// `.corrupt-<ts>.bak` quarantine backups next to it (ADR-54 self-heal view).
fn probe_store(path: std::path::PathBuf) -> StoreStatus {
    let mut quarantined_backups = Vec::new();
    let file_name =
        path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
    let prefix = format!("{file_name}.corrupt-");
    if let Some(parent) = path.parent() {
        if let Ok(entries) = std::fs::read_dir(parent) {
            let mut names: Vec<String> = entries
                .flatten()
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .filter(|name| name.starts_with(&prefix))
                .collect();
            names.sort();
            quarantined_backups = names;
        }
    }
    let exists = path.is_file();
    let sqlite_header_valid = exists && concerto_core::helpers::is_sqlite_file(&path);
    let state = match (exists, sqlite_header_valid) {
        (false, _) => "absent (created on first open)".into(),
        (true, true) => "ok".into(),
        (true, false) => "corrupt (rebuilt on next open)".into(),
    };
    StoreStatus {
        path: path.display().to_string(),
        state,
        sqlite_header_valid,
        quarantined_backups,
    }
}

/// One summary per configured provider, in config order.
fn provider_summaries(
    settings: Option<&ModelSettings>,
    creds: &CredentialStore,
) -> Vec<ProviderSummary> {
    let Some(settings) = settings else { return Vec::new() };
    settings
        .providers
        .iter()
        .map(|provider| ProviderSummary {
            id: provider.id.clone(),
            name: provider.name.clone(),
            provider: provider.provider.clone(),
            model: provider.model.clone(),
            extra_models: provider.extra_models.clone(),
            api_base: provider.api_base.clone(),
            echo: resolve_echo(provider),
            cache_breakpoints: provider.cache_breakpoints,
            key_status: if provider.effective_api_key(creds).is_ok() {
                "present"
            } else {
                "absent"
            }
            .to_string(),
        })
        .collect()
}

/// Resolve the effective reasoning-echo policy displayed for a provider.
fn resolve_echo(provider: &ProviderConfig) -> String {
    if provider.provider == "opencode" {
        // OpenCode Zen always emits reasoning content at construction and the
        // config dial is a no-op there (see `ProviderFactory::build`).
        return "always (built-in)".to_string();
    }
    match provider.reasoning_echo.as_deref().map(str::trim) {
        Some("always") => "always",
        Some("if-present") => "if-present",
        // `None` and unknown values resolve to the provider's built-in policy.
        _ => "default",
    }
    .to_string()
}

/// Routing profiles built from the config, sorted by cost then model name.
fn profile_summaries(settings: Option<&ModelSettings>) -> Vec<ProfileSummary> {
    let Some(settings) = settings else { return Vec::new() };
    let mut profiles: Vec<ProfileSummary> = ProviderFactory::build_profiles(settings)
        .into_iter()
        .map(|profile| ProfileSummary {
            provider: profile.provider,
            model: profile.model,
            cost_per_1k: profile.cost_per_1k_tokens,
            context_window: profile.context_window,
            tool_calling: profile.supports_tool_calling,
            base_url: profile.base_url,
        })
        .collect();
    profiles.sort_by(|a, b| {
        a.cost_per_1k
            .partial_cmp(&b.cost_per_1k)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.model.cmp(&b.model))
    });
    profiles
}

/// Accepts any non-empty role id (lowercased by [`AgentId::new`]), mirroring
/// the orchestrator's `configured_agent_id` so the catalog agrees with the
/// runtime's role naming (R12).
fn configured_id(role: &str) -> Option<AgentId> {
    let id = AgentId::new(role);
    (!id.as_str().is_empty()).then_some(id)
}

/// The roles the runtime would route model resolution for, in deterministic
/// order: the coordinator, the five builtin specialists (minus any disabled
/// by config), then enabled custom agents. Mirrors the orchestrator's
/// `topology_roles` (runtime_runner.rs) so the health catalog shows the same
/// roles the runtime would classify (R12).
fn role_topology(multi_agent: Option<&MultiAgentConfig>) -> Vec<AgentId> {
    let mut roles = vec![AgentId::new("coordinator")];
    for seed in builtin_agent_seeds() {
        let disabled = multi_agent
            .and_then(|multi| multi.custom_agents.iter().find(|agent| agent.role == seed.id))
            .is_some_and(|agent| agent.disabled);
        if !disabled {
            roles.push(AgentId::new(&seed.id));
        }
    }
    if let Some(multi_agent) = multi_agent {
        for agent in &multi_agent.custom_agents {
            if agent.disabled {
                continue;
            }
            if let Some(id) = configured_id(&agent.role) {
                if !roles.contains(&id) {
                    roles.push(id);
                }
            }
        }
    }
    roles
}

/// Per-agent config map keyed by role id, mirroring the orchestrator's
/// `build_agent_config_map` so the facade sees the same custom-agent shape
/// the runtime would (R12).
fn agent_config_map(multi_agent: Option<&MultiAgentConfig>) -> HashMap<AgentId, CustomAgentConfig> {
    let Some(multi_agent) = multi_agent else { return HashMap::new() };
    let mut map = HashMap::new();
    for agent in &multi_agent.custom_agents {
        if let Some(role) = configured_id(&agent.role) {
            map.insert(role, agent.clone());
        }
    }
    map
}

/// Explicit assignments followed by the default tool-calling roles.
///
/// The default roles come from the resolved blueprint facade
/// (`BlueprintFacade::tool_calling_roles`, ADR-58 R12/F2) so the catalog
/// matches what the runtime would route; each is shown with its assignment
/// status even when the user never wrote an `[model_settings
/// .agent_assignments]` entry for it. On the default `standard` blueprint
/// the derived set is exactly {researcher, coder, validator}.
fn assignment_summaries(
    settings: Option<&ModelSettings>,
    profiles: &[ProfileSummary],
    multi_agent: Option<&MultiAgentConfig>,
    facade: Option<&BlueprintFacade>,
) -> Vec<AssignmentSummary> {
    let Some(settings) = settings else { return Vec::new() };
    let topology = role_topology(multi_agent);
    let agent_configs = agent_config_map(multi_agent);
    // ADR-58 R12: derived from the facade (the resolved blueprint). The
    // legacy `DEFAULT_TOOL_CALLING_ROLES` literal is deleted; a facade-less
    // config (never produced by the load seam) keeps the same three default
    // tool-calling roles so the report cannot silently drop the known
    // defaults.
    let tool_calling_roles = facade
        .map(|facade| facade.tool_calling_roles(&topology, &agent_configs))
        .unwrap_or_else(|| {
            ["researcher", "coder", "validator"]
                .iter()
                .map(|id| AgentId::new(*id))
                .collect::<HashSet<AgentId>>()
        });
    let mut summaries =
        Vec::with_capacity(settings.agent_assignments.len() + tool_calling_roles.len());

    for assignment in &settings.agent_assignments {
        let provider = settings.providers.iter().find(|p| p.id == assignment.provider_config_id);
        let model = assignment
            .model_override
            .clone()
            .or_else(|| provider.map(|p| p.model.clone()))
            .unwrap_or_else(|| "(unset)".to_string());
        summaries.push(AssignmentSummary {
            role: assignment.agent_role.clone(),
            model: model.clone(),
            provider: assignment.provider_config_id.clone(),
            explicit: true,
            supported: profiles.iter().any(|profile| profile.model == model),
        });
    }

    // The default tool-calling roles get an (unassigned) entry unless the
    // user already wrote an explicit assignment for them. Rendered in the
    // deterministic topology order (builtin seeds first, then custom agents)
    // so the default output stays {researcher, coder, validator}.
    for role in topology.iter().filter(|id| tool_calling_roles.contains(*id)) {
        if settings
            .agent_assignments
            .iter()
            .any(|assignment| assignment.agent_role == role.as_str())
        {
            continue;
        }
        let fallback = default_role_resolution(settings, profiles);
        summaries.push(AssignmentSummary {
            role: role.as_str().to_string(),
            model: fallback.model,
            provider: fallback.provider,
            explicit: false,
            supported: fallback.supported,
        });
    }
    summaries
}

/// What an unassigned role would be served: like the runtime, an unassigned
/// role falls back to the run's default model on a provider that offers it.
struct ResolvedDefaultRole {
    model: String,
    provider: String,
    supported: bool,
}

fn default_role_resolution(
    settings: &ModelSettings,
    profiles: &[ProfileSummary],
) -> ResolvedDefaultRole {
    let Some(model) = settings.resolved_default_model(None) else {
        return ResolvedDefaultRole {
            model: "(unset)".to_string(),
            provider: "(default)".to_string(),
            supported: false,
        };
    };
    let provider = ProviderFactory::config_for_model(settings, &model, None)
        .map(|config| config.id.clone())
        .unwrap_or_else(|| "(default)".to_string());
    ResolvedDefaultRole {
        supported: profiles.iter().any(|profile| profile.model == model),
        model,
        provider,
    }
}

/// Tier-1 fallback target resolution (ADR-42/45), shared with the orchestrator
/// through `ModelSettings::resolved_default_model`.
fn tier1_summary(
    multi_agent: Option<&MultiAgentConfig>,
    settings: Option<&ModelSettings>,
    profiles: &[ProfileSummary],
) -> Tier1Summary {
    let Some(settings) = settings else {
        return Tier1Summary { model: None, source: None, served: false, tool_calling: false };
    };
    let model = settings.resolved_default_model(multi_agent);
    let source = if multi_agent
        .and_then(|config| config.default_model.as_deref())
        .map(str::trim)
        .is_some_and(|value| !value.is_empty())
    {
        Some("[multi_agent].default_model".to_string())
    } else if settings
        .global_default_model
        .as_deref()
        .map(str::trim)
        .is_some_and(|value| !value.is_empty())
    {
        Some("global_default_model".to_string())
    } else {
        None
    };
    let Some(ref resolved) = model else {
        return Tier1Summary { model: None, source, served: false, tool_calling: false };
    };
    let serving = profiles.iter().find(|profile| profile.model == *resolved);
    Tier1Summary {
        model: Some(resolved.clone()),
        source,
        served: serving.is_some(),
        tool_calling: serving.map(|profile| profile.tool_calling).unwrap_or(false),
    }
}

/// Resolved single-agent context budget (ADR-048).
fn context_summary(context: Option<&ContextConfig>) -> ContextSummary {
    match context {
        Some(config) => ContextSummary {
            trigger_tokens: config.trigger_tokens.unwrap_or(DEFAULT_TRIGGER_TOKENS),
            retain_user_turns: config.retain_user_turns.unwrap_or(DEFAULT_RETAIN_USER_TURNS),
            minimum_user_turns: config.minimum_user_turns.unwrap_or(DEFAULT_MINIMUM_USER_TURNS),
            from_defaults: false,
        },
        None => ContextSummary {
            trigger_tokens: DEFAULT_TRIGGER_TOKENS,
            retain_user_turns: DEFAULT_RETAIN_USER_TURNS,
            minimum_user_turns: DEFAULT_MINIMUM_USER_TURNS,
            from_defaults: true,
        },
    }
}

impl HealthReport {
    /// Warnings for the final summary line: anything that would surprise a
    /// user running the binary today.
    fn warnings(&self) -> Vec<String> {
        let mut warnings = Vec::new();
        if self.configured_providers.is_empty() {
            warnings.push("no providers configured".to_string());
        }
        for provider in &self.configured_providers {
            if provider.key_status == "absent" {
                warnings.push(format!("key missing: {}", provider.id));
            }
        }
        if let Some(model) = &self.tier1.model {
            if !self.tier1.served {
                warnings.push(format!("tier-1 default model '{model}' has no routing profile"));
            }
        }
        warnings
    }

    fn fmt_provider_stack(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "=== Provider Stack ===")?;
        match &self.config_path {
            Some(path) => writeln!(f, "Config file: {path}")?,
            None => writeln!(f, "Config file: (none — using defaults)")?,
        }
        writeln!(f)?;
        if self.configured_providers.is_empty() {
            writeln!(f, "No providers configured.")?;
            return Ok(());
        }
        for provider in &self.configured_providers {
            writeln!(f, "[{}] {} — model: {}", provider.id, provider.provider, provider.model)?;
            if !provider.name.is_empty() {
                writeln!(f, "    name: {}", provider.name)?;
            }
            if !provider.extra_models.is_empty() {
                writeln!(f, "    extra models: {}", provider.extra_models.join(", "))?;
            }
            let api_base = provider.api_base.as_deref().unwrap_or("(none)");
            let key = if provider.key_status == "present" { "✓ present" } else { "✗ missing" };
            writeln!(f, "    api base: {api_base}")?;
            writeln!(f, "    reasoning echo: {}", provider.echo)?;
            writeln!(f, "    cache breakpoints: {}", yes_no(provider.cache_breakpoints))?;
            writeln!(f, "    key: {key}")?;
        }
        Ok(())
    }

    fn fmt_routing_profiles(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "=== Routing Profiles ===")?;
        if self.routing_profiles.is_empty() {
            writeln!(f, "(none — no model_settings configured)")?;
            return Ok(());
        }
        for profile in &self.routing_profiles {
            let tool = if profile.tool_calling { "tool-calling" } else { "no tools" };
            let base = profile.base_url.as_deref().unwrap_or("(default)");
            writeln!(
                f,
                "  {:<14} {:<32} cost {:.6}/1k  ctx {:<7} {tool}  base {base}",
                profile.provider, profile.model, profile.cost_per_1k, profile.context_window,
            )?;
        }
        Ok(())
    }

    fn fmt_assignments(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "=== Assignments ===")?;
        if self.assignments.is_empty() {
            writeln!(f, "(none)")?;
            return Ok(());
        }
        for assignment in &self.assignments {
            let kind = if assignment.explicit { "explicit" } else { "default role" };
            let state = if assignment.supported { "served" } else { "not in profiles" };
            writeln!(
                f,
                "  {:<12} -> {:<28} via {:<14} ({kind}: {state})",
                assignment.role, assignment.model, assignment.provider,
            )?;
        }
        Ok(())
    }

    fn fmt_tier1(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "=== Tier-1 Default ===")?;
        let Some(model) = &self.tier1.model else {
            writeln!(f, "  not configured — tier-1 fallback disabled.")?;
            return Ok(());
        };
        let state = if self.tier1.served {
            if self.tier1.tool_calling {
                "served (tool-calling)".to_string()
            } else {
                "served (no tools)".to_string()
            }
        } else {
            "no routing profile".to_string()
        };
        writeln!(f, "  model: {model} ({state})")?;
        match &self.tier1.source {
            Some(source) => writeln!(f, "  source: {source}")?,
            None => writeln!(f, "  source: (n/a)")?,
        }
        Ok(())
    }

    fn fmt_context(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "=== Context Policy ===")?;
        let defaults = if self.context.from_defaults { " (defaults)" } else { "" };
        writeln!(f, "  trigger tokens: {}{}", self.context.trigger_tokens, defaults)?;
        writeln!(f, "  retain user turns: {}", self.context.retain_user_turns)?;
        writeln!(f, "  minimum user turns: {}", self.context.minimum_user_turns)?;
        Ok(())
    }

    fn fmt_stores(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "=== Store Status ===")?;
        writeln!(f, "  data dir: {}", self.stores.data_dir)?;
        fmt_store(f, "sessions", &self.stores.sessions)?;
        fmt_store(f, "memory", &self.stores.memory)?;
        Ok(())
    }

    fn fmt_summary(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f)?;
        writeln!(f, "=== Summary ===")?;
        let warnings = self.warnings();
        if warnings.is_empty() {
            writeln!(f, "OK — no problems detected.")?;
        } else {
            writeln!(f, "{} problem(s):", warnings.len())?;
            for warning in warnings {
                writeln!(f, "  - {warning}")?;
            }
        }
        Ok(())
    }
}

impl fmt::Display for HealthReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.fmt_provider_stack(f)?;
        writeln!(f)?;
        self.fmt_routing_profiles(f)?;
        writeln!(f)?;
        self.fmt_assignments(f)?;
        writeln!(f)?;
        self.fmt_tier1(f)?;
        writeln!(f)?;
        self.fmt_context(f)?;
        writeln!(f)?;
        self.fmt_stores(f)?;
        self.fmt_summary(f)
    }
}

fn yes_no(value: bool) -> &'static str {
    if value {
        "yes"
    } else {
        "no"
    }
}

/// One store line in the `=== Store Status ===` section.
fn fmt_store(f: &mut fmt::Formatter<'_>, label: &str, status: &StoreStatus) -> fmt::Result {
    writeln!(f, "  {label}: {}", status.state)?;
    writeln!(f, "          path: {}", status.path)?;
    if !status.quarantined_backups.is_empty() {
        writeln!(f, "          quarantined backups: {}", status.quarantined_backups.join(", "))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use concerto_config::{AgentModelAssignment, AppConfig};

    /// Build a deterministic fixture config exercising every report section.
    fn fixture_config() -> AppConfig {
        AppConfig {
            model_settings: Some(ModelSettings {
                providers: vec![
                    ProviderConfig {
                        id: "openai-1".into(),
                        name: "Primary".into(),
                        provider: "openai".into(),
                        model: "gpt-4o".into(),
                        api_base: None,
                        keyring_key: "concerto-health-test/primary".into(),
                        reasoning_echo: Some("if-present".into()),
                        extra_models: vec!["gpt-4o-mini".into()],
                        ..Default::default()
                    },
                    ProviderConfig {
                        id: "zen-1".into(),
                        name: "Zen".into(),
                        provider: "opencode".into(),
                        model: "deepseek-v3".into(),
                        api_base: None,
                        keyring_key: "concerto-health-test/zen".into(),
                        ..Default::default()
                    },
                ],
                global_default_model: Some("gpt-4o".into()),
                agent_assignments: vec![AgentModelAssignment {
                    agent_role: "coder".into(),
                    provider_config_id: "openai-1".into(),
                    model_override: None,
                }],
                ..Default::default()
            }),
            multi_agent: Some(MultiAgentConfig {
                default_model: Some("deepseek-v3".into()),
                ..Default::default()
            }),
            context: Some(ContextConfig {
                trigger_tokens: Some(20_000),
                retain_user_turns: None,
                minimum_user_turns: Some(2),
            }),
            ..Default::default()
        }
    }

    fn write_fixture(dir: &std::path::Path) -> std::path::PathBuf {
        let path = dir.join("config.toml");
        let toml = toml::to_string_pretty(&fixture_config()).expect("fixture serializes");
        std::fs::write(&path, toml).expect("fixture writes");
        path
    }

    #[test]
    fn health_with_no_config_reports_not_configured() {
        // A nonexistent config path forces pure defaults and must be treated as
        // "not configured" rather than an error (mirrors `config doctor`).
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("does-not-exist.toml");
        let report = collect_health(Some(&missing), None);

        assert!(report.configured_providers.is_empty());
        assert!(report.routing_profiles.is_empty());
        assert_eq!(report.tier1.model, None);
        assert!(report.config_path.is_none(), "no usable file -> no config path");
        assert!(report.context.from_defaults);

        let text = report.to_string();
        assert!(text.contains("No providers configured"));
        assert!(text.contains("no providers configured"), "summary warning missing");
        assert!(text.contains("tier-1 fallback disabled") || text.contains("Tier-1 Default"));
    }

    #[test]
    fn health_with_fixture_resolves_every_section() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_fixture(dir.path());
        let report = collect_health(Some(&path), None);

        // Providers: two configured, echo resolved from config, opencode note.
        assert_eq!(report.configured_providers.len(), 2);
        let openai = &report.configured_providers[0];
        assert_eq!(openai.id, "openai-1");
        assert_eq!(openai.echo, "if-present");
        assert_eq!(openai.extra_models, vec!["gpt-4o-mini"]);
        let zen = &report.configured_providers[1];
        assert_eq!(zen.echo, "always (built-in)", "opencode family always reports built-in echo");

        // Tier-1: multi_agent.default_model wins over the global default.
        assert_eq!(report.tier1.model.as_deref(), Some("deepseek-v3"));
        assert_eq!(report.tier1.source.as_deref(), Some("[multi_agent].default_model"));
        assert!(report.tier1.served, "the opencode profile serves the tier-1 model");
        assert!(report.tier1.tool_calling);

        // Assignments: explicit coder -> openai-1 with the provider's model.
        let coder =
            report.assignments.iter().find(|a| a.role == "coder").expect("coder present once");
        assert!(coder.explicit);
        assert_eq!(coder.model, "gpt-4o");
        assert_eq!(coder.provider, "openai-1");
        assert!(coder.supported);
        // Unassigned default roles are present and not explicit.
        let researcher =
            report.assignments.iter().find(|a| a.role == "researcher").expect("researcher listed");
        assert!(!researcher.explicit);
        assert_eq!(researcher.model, "gpt-4o", "unassigned roles use the global default");

        // Config path is reported when the file exists.
        assert_eq!(report.config_path.as_deref(), Some(path.to_str().expect("utf8 path")));

        // Context resolves from the section with defaults for unset knobs.
        assert_eq!(report.context.trigger_tokens, 20_000);
        assert_eq!(report.context.retain_user_turns, 4, "unset knob -> embedded default");
        assert_eq!(report.context.minimum_user_turns, 2);
        assert!(!report.context.from_defaults);

        // No warnings for a healthy fixture (keys may be missing on a fresh
        // machine though — only assert the "OK" line is absent when absent).
        let text = report.to_string();
        assert!(text.contains("=== Provider Stack ==="));
        assert!(text.contains("=== Routing Profiles ==="));
        assert!(text.contains("=== Assignments ==="));
        assert!(text.contains("=== Tier-1 Default ==="));
        assert!(text.contains("=== Context Policy ==="));
        assert!(text.contains("=== Summary ==="));
    }

    #[test]
    fn health_json_serializes_report_shape() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_fixture(dir.path());
        let report = collect_health(Some(&path), None);

        let json = serde_json::to_string(&report).expect("report serializes");
        let value: serde_json::Value = serde_json::from_str(&json).expect("json parses");

        assert_eq!(value["configured_providers"].as_array().map(Vec::len), Some(2));
        assert_eq!(value["routing_profiles"].as_array().map(Vec::len), Some(2));
        assert_eq!(value["tier1"]["model"], "deepseek-v3");
        assert_eq!(value["tier1"]["source"], "[multi_agent].default_model");
        assert_eq!(value["tier1"]["served"], true);
        assert_eq!(value["context"]["trigger_tokens"], 20_000);
        assert_eq!(value["assignments"][0]["role"], "coder");
        assert!(value["config_path"].is_string());
        assert!(value["stores"]["data_dir"].is_string(), "store section must be present");
        assert!(value["stores"]["sessions"].is_object());
        assert!(value["stores"]["memory"].is_object());
    }

    #[test]
    fn store_probe_detects_corruption_and_quarantine_backups() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("memory.db");

        // Garbage file -> corrupt, no valid header.
        std::fs::write(&db, b"this is not a sqlite database").unwrap();
        let status = probe_store(db.clone());
        assert_eq!(status.state, "corrupt (rebuilt on next open)");
        assert!(!status.sqlite_header_valid, "garbage file must fail the header check");

        // Valid SQLite header -> ok.
        std::fs::write(&db, *b"SQLite format 3\0with-padding").unwrap();
        let status = probe_store(db.clone());
        assert_eq!(status.state, "ok");
        assert!(status.sqlite_header_valid);

        // Quarantine backups are listed (ADR-54 self-heal evidence).
        let backup = "memory.db.corrupt-1723000000.bak";
        std::fs::write(dir.path().join(backup), b"quarantined").unwrap();
        let status = probe_store(db);
        assert_eq!(status.quarantined_backups, vec![backup.to_string()]);

        // Missing file -> absent, no panic.
        let status = probe_store(dir.path().join("nope.db"));
        assert_eq!(status.state, "absent (created on first open)");
        assert!(status.quarantined_backups.is_empty());
    }

    #[test]
    fn health_with_only_global_default_reports_global_source() {
        let mut config = fixture_config();
        config.multi_agent = None;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            toml::to_string_pretty(&config).expect("global-only fixture serializes"),
        )
        .expect("writes");
        let report = collect_health(Some(&path), None);

        assert_eq!(report.tier1.model.as_deref(), Some("gpt-4o"));
        assert_eq!(report.tier1.source.as_deref(), Some("global_default_model"));
        assert!(report.tier1.served);
    }

    /// Drop guard deleting an env var even when the test panics, so it never
    /// leaks into tests running in parallel.
    struct EnvVarGuard(&'static str);
    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            std::env::remove_var(self.0);
        }
    }

    #[test]
    fn health_reports_env_only_key_as_present() {
        // A provider whose key exists only as `<PROVIDER>_API_KEY` (no keyring
        // entry) must be reported present — the same resolution
        // `ProviderFactory::build` uses at runtime.
        let mut config = fixture_config();
        let provider = config
            .model_settings
            .as_mut()
            .expect("fixture has model settings")
            .providers
            .first_mut()
            .expect("fixture has a provider");
        provider.provider = "TESTPROVIDEN".into();

        let _guard = EnvVarGuard("TESTPROVIDEN_API_KEY");
        std::env::set_var("TESTPROVIDEN_API_KEY", "sk-health-env");

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, toml::to_string_pretty(&config).expect("fixture serializes"))
            .expect("writes");
        let report = collect_health(Some(&path), None);

        let summary = report
            .configured_providers
            .iter()
            .find(|summary| summary.id == "openai-1")
            .expect("provider listed");
        assert_eq!(summary.key_status, "present", "env-only key must be reported present");
    }
}
