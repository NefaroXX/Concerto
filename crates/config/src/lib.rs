#![deny(clippy::all)]
#![deny(unused_imports, unused_variables, dead_code)]
#![allow(missing_docs)]

//! `concerto-config` — layered configuration (ADR-03, `figment`) and secure
//! credential storage (ADR-04, `keyring`).
//!
//! The crate owns the current application schema, migration, layered global
//! and project configuration, provider/model assignments, retry and policy
//! settings, shell profiles, and OS-keychain credential storage.

pub mod blueprint;
pub mod credentials;
pub mod facade;
pub mod legacy;
pub mod managed;
mod migration;
pub mod projects;
mod saving;
mod schema;
pub mod setup;
pub mod shell;

pub use blueprint::{
    coordinator_fallback, coordinator_self_implement_fallback, include_write_target,
    named_blueprint, parse_blueprint_file, resolve_blueprint, validate_blueprint, Blueprint,
    BlueprintError, BlueprintSelection, CapabilityMask, ExecutionFilesDef, FallbackPersonaDef,
    FeedLabel, OrchestrationConfig, PipelineDef, RelationshipDef, RelationshipSemantics,
    ResolvedBlueprint, ResolvedStage, StageCondition, StageDef, StageFlags, StageKind,
    BLUEPRINT_INCLUDE_FILE, NAMED_BLUEPRINTS, ORCHESTRATION_SCHEMA_VERSION,
    RESERVED_BLUEPRINT_NAMES,
};
pub use credentials::CredentialStore;
pub use facade::BlueprintFacade;
pub use managed::{
    IntegrityEntry, IntegrityInfo, IntegrityReport, IntegrityStatus, ManagedRuntimeError,
    ManagedRuntimeManager, RuntimeManifest, ToolEntry, ToolManifest,
};
pub use projects::ProjectRegistry;
pub use saving::{
    roster_materialized, save_agent_roster, save_blueprint, save_inline_blueprint,
    seed_agent_roster_only, seed_orchestration_roster,
};
pub use schema::{
    builtin_agent_seeds, AgentCapabilities, AgentModelAssignment, AgentRelationshipConfig,
    AppConfig, ConditionDef, ContextConfig, CustomAgentConfig, FewShotExample, IntentConfig,
    McpConfig, McpServerConfig, MemoryConfig, ModelPinConfig, ModelProfileOverride, ModelSettings,
    MultiAgentConfig, ObservabilityConfig, PipelinePreset, PlanBindingSource, PolicyConfig,
    PolicyRuleDef, PromptSections, ProviderConfig, RetryConfig, SkillsConfig, ToolSettings,
    UpdatesConfig, SCHEMA_VERSION,
};
pub use setup::{PendingConfig, SetupError, SetupWizard};
pub use shell::{
    default_shell_profiles, default_shell_settings, ManagedEnvConfig, ProfileAvailability,
    ShellBackendType, ShellProfileConfig, ShellSettings, WorkingDirBehavior,
};

use camino::Utf8PathBuf;
use concerto_core::error::ConfigError;
use figment::{
    providers::{Env, Format, Serialized, Toml},
    Figment,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Environment variable carrying the project-root allowlist (ADR-44).
///
/// Value is a path-separated list using the platform's
/// [`std::env::split_paths`] separator (e.g. `:` on Unix, `;` on Windows).
const PROJECT_ROOTS_ENV_VAR: &str = "CONCERTO_PROJECT_ROOTS";

/// Default config file location with legacy fallback.
///
/// Returns the new path (`~/.config/concerto/config.toml`) if it exists,
/// falling back to the old path (`~/.config/opencode-rs/config.toml`).
/// When neither exists, returns the new path (callers will create it on write).
/// Returns `None` if the OS has no resolvable config-dir (e.g. some minimal
/// containers); callers should fall back to defaults + env only in that case.
pub fn default_config_path() -> Option<PathBuf> {
    legacy::config_path()
}

/// Load config from the layered sources: defaults -> global file -> project file -> env.
/// Env vars are prefixed `CONCERTO_` (e.g. `CONCERTO_PROVIDER`), so env always wins.
///
/// `global_path` is the user-level config file. If `None`, falls back to
/// [`default_config_path()`] (which includes legacy fallback to `opencode-rs`).
/// `project_root` is the project directory; if a `.concerto.toml` exists there, it is merged
/// between the global file and env so per-project overrides (policy rules, model pins, spend cap)
/// can be committed to the project repo or gitignored as desired.
///
/// Environment variables: `CONCERTO_*` is the primary prefix. As a
/// convenience, `OPENCODE_RS_*` variables are also merged so existing
/// shell configs continue working.
///
/// If the loaded config has an older `schema_version`, automatic schema
/// migration is applied (see [`migration::migrate_config`]).
pub fn load_config(
    global_path: Option<&PathBuf>,
    project_root: Option<&Path>,
) -> Result<AppConfig, ConfigError> {
    load_config_layers(global_path, project_root, true)
}

/// Load only defaults plus the user-level file for settings editing.
///
/// Project files and environment variables are deliberately excluded so a
/// frontend can save global settings without promoting higher-precedence
/// runtime overrides into `config.toml`.
pub fn load_global_config(global_path: Option<&PathBuf>) -> Result<AppConfig, ConfigError> {
    load_config_layers(global_path, None, false)
}

fn load_config_layers(
    global_path: Option<&PathBuf>,
    project_root: Option<&Path>,
    include_environment: bool,
) -> Result<AppConfig, ConfigError> {
    let defaults = AppConfig::default();

    let mut figment = Figment::new().merge(Serialized::defaults(&defaults));

    // 1) Global user-level config (with legacy fallback)
    // If no explicit path, try default_config_path() which handles new→old fallback.
    let global_config = global_path.cloned().or_else(default_config_path);
    if let Some(ref p) = global_config {
        if p.exists() {
            figment = figment.merge(Toml::file(p));
        }
    }

    // 2) Project-scoped config (inserted between global config and env)
    if let Some(root) = project_root {
        let project_file = root.join(legacy::NEW_PROJECT_CONFIG_FILE);
        if project_file.exists() {
            figment = figment.merge(Toml::file(&project_file));
        } else if let Some(parent) = project_file.parent() {
            // Legacy project file fallback
            let legacy_project = parent.join(legacy::OLD_PROJECT_CONFIG_FILE);
            if legacy_project.exists() {
                figment = figment.merge(Toml::file(&legacy_project));
            }
        }
    }

    // 3) Env vars (highest priority over files). Both prefixes remain readable
    //    for migration; callers should not define the same key under both.
    //    `project_roots` is excluded from the env providers: its env source is
    //    a path-separated scalar (`CONCERTO_PROJECT_ROOTS`), which would fail
    //    `Vec` deserialization during figment extraction. It is parsed
    //    explicitly after extraction (ADR-44).
    if include_environment {
        figment =
            figment.merge(Env::prefixed(legacy::NEW_ENV_PREFIX).filter(|k| k != "project_roots"));
        figment =
            figment.merge(Env::prefixed(legacy::OLD_ENV_PREFIX).filter(|k| k != "project_roots"));
    }

    let config: AppConfig = figment.extract().map_err(|e| ConfigError::Load(e.to_string()))?;

    // Apply schema migration if needed.
    let mut config = migration::migrate_config(config)?;

    // ADR-44 §1: `CONCERTO_PROJECT_ROOTS` (path-separated) replaces the roots
    // from config files when set to a non-empty value — env wins per the
    // existing figment layering. Parsed here so every consumer of
    // `load_config` sees the same merged value. `load_global_config` (the
    // settings-editor path) deliberately excludes env overrides, so the
    // override only applies on the environment-inclusive load path.
    if include_environment {
        apply_project_roots_env(&mut config)?;
    }

    // Repair legacy provider ids (empty or duplicated) once on load. The
    // repaired ids are returned in-memory; they are persisted on the next
    // atomic config save (Settings save / active selection).
    if let Some(ms) = &mut config.model_settings {
        ms.repair_ids();
    }
    // Also repair a legacy single-provider config if present.
    if let Some(pc) = &mut config.primary_provider_config {
        pc.ensure_id();
    }

    // Validate retry settings (after migration so defaults are in place).
    config.retry.validate()?;
    config.memory.validate()?;

    // Validate the Phase-2c intent classifier threshold (ADR-55 Phase 2c §2):
    // a configured threshold below the deterministic gate constant would
    // create a band where a classifier Execute re-route misses the gate's
    // arm-1 confirmation dialog.
    if let Some(intent) = &config.intent {
        intent.validate()?;
    }

    // Validate MCP server entries (ADR-43 §4: non-empty id, no ':').
    if let Some(mcp) = &config.mcp {
        mcp.validate()?;
    }

    // ADR-58: resolve the blueprint on EVERY load path — the `[orchestration]`
    // selection when present, the default `standard` blueprint otherwise —
    // and enforce the load-time extension (B4: unknown agent stage tags are
    // rejected, but ONLY when `[orchestration]` is present; legacy configs
    // without the section keep their tags). Validation shifts to load time
    // (ADR-58 Consequences). The B3 write-capability widening hard error is
    // removed — capability flags are plain flags. Rule (f) is bound by the
    // ADR-52 run cap when `max_total_iterations` is configured, and is
    // vacuous otherwise.
    //
    // Config-dir order matters for include resolution (N1): the project root
    // comes FIRST so a project-scoped blueprint include overrides the global
    // one — consistent with the crate's project-overrides-global layering.
    let mut config_dirs = Vec::new();
    if let Some(root) = project_root {
        config_dirs.push(root.to_path_buf());
    }
    if let Some(ref path) = global_config {
        if let Some(parent) = path.parent() {
            config_dirs.push(parent.to_path_buf());
        }
    }
    let global_max = config.multi_agent.as_ref().and_then(|m| m.max_total_iterations);
    let resolved = match &config.orchestration {
        Some(orchestration) => orchestration.resolve(&config_dirs, global_max)?,
        None => OrchestrationConfig::default().resolve(&config_dirs, global_max)?,
    };
    let agents: &[CustomAgentConfig] =
        config.multi_agent.as_ref().map(|m| m.custom_agents.as_slice()).unwrap_or_default();
    validate_custom_agents(&resolved, agents, config.orchestration.is_some())?;
    // ADR-58 §4 (F9): the legacy closed-string relationship check applies
    // only while `[orchestration]` is ABSENT (pre-blueprint equivalence).
    // Under `[orchestration]`, relationship validity comes from the
    // blueprint's open relationship registry — the runtime resolves kind
    // strings through `BlueprintFacade::relationship_semantics`, which
    // hard-errors on unmatched kinds (design doc §4 F7 review).
    if config.orchestration.is_none() {
        validate_multi_agent_relationships(&config)?;
    }

    // ADR-58 P2+P3 (Batch 1, design doc §1.2): attach the resolved blueprint
    // to the config object as derived state. `resolved` was function-local —
    // the runtime facade (`concerto_config::BlueprintFacade`) now consumes it
    // once per run from `config.resolved_blueprint`. Serde-skipped, so it
    // never round-trips through a written config file.
    config.resolved_blueprint = Some(Arc::new(resolved));

    // ADR-35 §5: the Coordinator is constructed in code only. Config entries
    // targeting it are ignored by the registry; log a user-facing warning for
    // each one so the load path surfaces the silent drop.
    if let Some(multi_agent) = &config.multi_agent {
        for warning in multi_agent.pipeline_warnings() {
            tracing::warn!(%warning, "multi-agent config");
        }
    }

    Ok(config)
}

/// The sanctioned Freeform stage tag for user-defined agents (ADR-58 D2): the
/// RunOnce kind's registry tag. Today's unknown-tag Freeform semantics are
/// preserved under this tag; under `[orchestration]` a bare unknown stage
/// string is a load error (B4, rulebook (g) extended to agent stage tags).
const RUN_ONCE_AGENT_STAGE: &str = "run_once";

impl AppConfig {
    /// Whether the config file owns the working agent roster.
    ///
    /// Roster-authority rule: once a config declares a roster — ANY
    /// `[multi_agent.custom_agents]` entry **OR** an `[orchestration]`
    /// section — the roster is the config entries ONLY: the embedded
    /// `builtin_agent_seeds()` are not merged back in, and an id deleted from
    /// config stays deleted. Only when the config declares neither do the
    /// embedded seeds stand in (the legacy embedded default, unchanged).
    pub fn owns_agent_roster(&self) -> bool {
        self.orchestration.is_some()
            || self.multi_agent.as_ref().is_some_and(|m| !m.custom_agents.is_empty())
    }
}

/// ADR-58 D2 §5.3/§5.5 load-time enforcement for user-defined agents
/// (rulebook (g) extended):
///
/// - **Unknown stage tags** (B4): ONLY when `[orchestration]` is present — a
///   `stage: Some(tag)` that is neither a resolved blueprint stage tag nor
///   `run_once` is a hard load error with retag/register guidance. When
///   `[orchestration]` is absent the legacy tag strings are accepted
///   untouched (the embedded standard pipeline keeps today's tags). `None`
///   always passes untouched and the stored field is never mutated — the
///   registry keeps interpreting `None`/`run_once` as Freeform.
///
/// The explicit write-capability widening hard error (B3) is **removed**:
/// agent capability flags are plain flags (the config owns the roster once it
/// declares one).
///
/// `orchestration_present` is `config.orchestration.is_some()`; the
/// referential-integrity check couples agent tags to the resolved blueprint's
/// stage tags only when that blueprint actually comes from config.
fn validate_custom_agents(
    resolved: &ResolvedBlueprint,
    agents: &[CustomAgentConfig],
    orchestration_present: bool,
) -> Result<(), ConfigError> {
    let seeds = builtin_agent_seeds();
    let builtin_ids: Vec<&str> = seeds.iter().map(|s| s.id.as_str()).collect();
    for agent in agents {
        if builtin_ids.contains(&agent.id.as_str()) {
            continue;
        }
        let stage_tag = agent.stage.as_ref().map(|stage| stage.as_str());

        // B4: unknown stage tags are hard load errors only under
        // `[orchestration]` — a tag the config references must be registered
        // as a stage in the resolved blueprint (or be the sanctioned
        // `run_once` Freeform tag). Without `[orchestration]` the legacy tag
        // strings are preserved unchanged.
        if orchestration_present {
            if let Some(tag) = stage_tag {
                let known =
                    tag == RUN_ONCE_AGENT_STAGE || resolved.stages.iter().any(|s| s.def.tag == tag);
                if !known {
                    return Err(ConfigError::InvalidValue(format!(
                        "custom agent '{}' references unknown stage '{}': retag it as \
                         'run_once' (Freeform semantics, ADR-58 D2) or register a \
                         matching stage in the orchestration blueprint (ADR-58 §5.1)",
                        agent.id, tag
                    )));
                }
            }
        }
    }
    Ok(())
}

/// ADR-58 §4 load-time enforcement: every `multi_agent.relationships` kind
/// string must be a member of the closed relationship catalog (the strings
/// the runtime parses in `configured_relationship`). The runtime already
/// hard-fails on unmatched strings (`runtime_runner.rs:3198-3204`); this
/// relocates the failure strictly earlier, to config load.
fn validate_multi_agent_relationships(config: &AppConfig) -> Result<(), ConfigError> {
    const CLOSED_RELATIONSHIP_KINDS: &[&str] =
        &["supervises", "provides_context_to", "reports_to", "owns_design"];
    let Some(multi_agent) = &config.multi_agent else {
        return Ok(());
    };
    for rule in &multi_agent.relationships {
        let lowered = rule.relationship.to_ascii_lowercase();
        if !CLOSED_RELATIONSHIP_KINDS.contains(&lowered.as_str()) {
            return Err(ConfigError::InvalidValue(format!(
                "multi_agent.relationships: unknown relationship '{}' (from '{}' to '{}'); \
                 must be one of {} (ADR-58 §4)",
                rule.relationship,
                rule.from,
                rule.to,
                CLOSED_RELATIONSHIP_KINDS.join(", ")
            )));
        }
    }
    Ok(())
}

/// Apply the `CONCERTO_PROJECT_ROOTS` environment override to `config`.
///
/// Replaces `project_roots` with the path-separated environment list when the
/// variable is present and non-empty; otherwise leaves the extracted value
/// untouched. Returns a [`ConfigError`] if the value contains a non-UTF-8
/// path, matching how config-file roots deserialize.
fn apply_project_roots_env(config: &mut AppConfig) -> Result<(), ConfigError> {
    let Ok(raw) = std::env::var(PROJECT_ROOTS_ENV_VAR) else {
        return Ok(());
    };
    if raw.is_empty() {
        return Ok(());
    }
    let mut roots = Vec::new();
    for path in std::env::split_paths(&raw) {
        roots.push(Utf8PathBuf::from_path_buf(path).map_err(|p| {
            ConfigError::Load(format!("{PROJECT_ROOTS_ENV_VAR} contains a non-UTF-8 path: {p:?}"))
        })?);
    }
    config.project_roots = roots;
    Ok(())
}

/// Serialize `config` to TOML and write it to `path`, creating parent
/// directories as needed. Overwrites any existing file at `path`.
pub fn save_config(config: &AppConfig, path: &Path) -> Result<(), ConfigError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| ConfigError::Load(format!("failed to create config dir: {e}")))?;
    }
    let toml_str = toml::to_string_pretty(config)
        .map_err(|e| ConfigError::Load(format!("failed to serialize config: {e}")))?;
    std::fs::write(path, toml_str)
        .map_err(|e| ConfigError::Load(format!("failed to write config file: {e}")))?;
    Ok(())
}

/// ADR-59 Decision 2 init step 2: surgically write the `[orchestration]`
/// selection — `schema_version` plus exactly one of `name`/`include`/`inline`
/// per [`BlueprintSelection`] — into an existing `config.toml`.
///
/// Every other key, section, comment, and key order is preserved; the edit
/// delegates to `saving::merge_edit_toml` and is atomic (temp + rename) per
/// call. `save_config` is never invoked by the Studio after P4 — this
/// one-time selection write is the only `config.toml` write the init flow
/// performs.
pub fn save_blueprint_selection(
    config_path: &Path,
    selection: &BlueprintSelection,
) -> Result<(), ConfigError> {
    let selector: (&str, String) = match (&selection.name, &selection.include, &selection.inline) {
        (Some(name), None, None) => ("name", toml::Value::String(name.clone()).to_string()),
        (None, Some(include), None) => {
            ("include", toml::Value::String(include.clone()).to_string())
        }
        (None, None, Some(inline)) => {
            let value = toml::Value::try_from(inline).map_err(|e| {
                ConfigError::Load(format!("failed to serialize inline blueprint: {e}"))
            })?;
            ("inline", value.to_string())
        }
        _ => {
            return Err(ConfigError::Load(
                "invalid blueprint selection: exactly one of name/include/inline is required"
                    .to_string(),
            ));
        }
    };

    // Write the section schema version first, then the selector key: at every
    // intermediate point the config still loads (the section is complete, just
    // pointing at the previous selection).
    let schema_version = toml::Value::Integer(ORCHESTRATION_SCHEMA_VERSION as i64).to_string();
    crate::saving::merge_edit_toml(
        config_path,
        &["orchestration"],
        "schema_version",
        &schema_version,
    )?;
    crate::saving::merge_edit_toml(
        config_path,
        &["orchestration", "blueprint"],
        selector.0,
        &selector.1,
    )
}

impl From<BlueprintError> for ConfigError {
    /// Blueprint load/validation failures surface as config load errors so
    /// `load_config` fails fast on an invalid `[orchestration]` section.
    fn from(error: BlueprintError) -> Self {
        ConfigError::Load(format!("orchestration blueprint: {error}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::PolicyTimeWindowConfig;
    use std::sync::Mutex;

    /// Serializes the ADR-44 `project_roots` tests, which mutate
    /// `CONCERTO_PROJECT_ROOTS`, against each other (cargo runs tests in
    /// parallel threads).
    static PROJECT_ROOTS_ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn defaults_load_without_a_file() {
        let cfg = load_config(None, None).expect("defaults must always load");
        assert_eq!(cfg.schema_version, SCHEMA_VERSION);
    }

    #[test]
    fn invalid_timezone_rejected() {
        let config = PolicyTimeWindowConfig {
            start_hour: 9,
            end_hour: 17,
            timezone: "Not/AZone".into(),
            auto_approve_below_usd: 0.10,
        };
        assert!(config.validate().is_err());
        let err = config.validate().unwrap_err();
        assert!(format!("{}", err).contains("Not/AZone"));
    }

    #[test]
    fn valid_timezone_accepted() {
        let config = PolicyTimeWindowConfig {
            start_hour: 9,
            end_hour: 17,
            timezone: "America/New_York".into(),
            auto_approve_below_usd: 0.10,
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn retry_section_loaded_from_toml() {
        use std::io::Write;
        let dir = std::env::temp_dir().join(format!("concerto-retry-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "schema_version = 3").unwrap();
        writeln!(f, "[retry]").unwrap();
        writeln!(f, "fixed_delay_ms = 15000").unwrap();
        writeln!(f, "respect_retry_after = false").unwrap();

        let cfg = load_config(Some(&path), None).expect("config with [retry] must load");
        assert_eq!(cfg.retry.fixed_delay_ms, Some(15_000), "fixed override must be 15s");
        assert!(!cfg.retry.respect_retry_after);
        // Unspecified fields keep their defaults.
        assert!(cfg.retry.enabled);
        assert_eq!(cfg.retry.initial_delay_ms, 2_000);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn config_without_retry_section_still_loads() {
        use std::io::Write;
        let dir = std::env::temp_dir().join(format!("concerto-retry-test2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "schema_version = 3").unwrap();

        let cfg = load_config(Some(&path), None).expect("legacy config must still load");
        assert_eq!(cfg.retry, RetryConfig::default());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn global_editor_layer_excludes_project_overrides() {
        let dir = tempfile::tempdir().unwrap();
        let global_path = dir.path().join("config.toml");
        let project = dir.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(
            &global_path,
            format!("schema_version = {SCHEMA_VERSION}\nsession_spend_cap_usd = 1.0\n"),
        )
        .unwrap();
        std::fs::write(
            project.join(legacy::NEW_PROJECT_CONFIG_FILE),
            format!("schema_version = {SCHEMA_VERSION}\nsession_spend_cap_usd = 2.0\n"),
        )
        .unwrap();

        let effective = load_config(Some(&global_path), Some(&project)).unwrap();
        let global = load_global_config(Some(&global_path)).unwrap();
        assert_eq!(effective.session_spend_cap_usd, Some(2.0));
        assert_eq!(global.session_spend_cap_usd, Some(1.0));
    }

    #[test]
    fn memory_settings_validate_on_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            format!("schema_version = {SCHEMA_VERSION}\n[memory]\nttl_days = 0\n"),
        )
        .unwrap();
        assert!(load_config(Some(&path), None).is_err());
    }

    #[test]
    fn config_session_spend_cap_default_is_none() {
        let cfg = load_config(None, None).expect("defaults must load");
        assert!(cfg.session_spend_cap_usd.is_none());
    }

    #[test]
    fn config_primary_provider_defaults_to_none() {
        let cfg = load_config(None, None).expect("defaults must load");
        assert!(cfg.primary_provider.is_none());
    }

    #[test]
    fn config_model_settings_defaults() {
        let cfg = load_config(None, None).expect("defaults must load");
        assert!(cfg.model_settings.is_none());
    }

    #[test]
    fn project_roots_default_empty() {
        let _guard = PROJECT_ROOTS_ENV_LOCK.lock().unwrap();
        std::env::remove_var(PROJECT_ROOTS_ENV_VAR);
        let cfg = load_config(None, None).expect("defaults must load");
        assert!(cfg.project_roots.is_empty());
    }

    #[test]
    fn project_roots_parsed_from_toml() {
        let _guard = PROJECT_ROOTS_ENV_LOCK.lock().unwrap();
        std::env::remove_var(PROJECT_ROOTS_ENV_VAR);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            format!(
                "schema_version = {SCHEMA_VERSION}\n\
                 project_roots = [\"/home/user/project-a\", \"/srv/project-b\"]\n"
            ),
        )
        .unwrap();

        let cfg = load_config(Some(&path), None).expect("config with project_roots must load");
        assert_eq!(
            cfg.project_roots,
            vec![Utf8PathBuf::from("/home/user/project-a"), Utf8PathBuf::from("/srv/project-b"),]
        );
    }

    #[test]
    fn project_roots_env_overrides_toml() {
        let _guard = PROJECT_ROOTS_ENV_LOCK.lock().unwrap();
        std::env::set_var(PROJECT_ROOTS_ENV_VAR, "/opt/env-root");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            format!(
                "schema_version = {SCHEMA_VERSION}\n\
                 project_roots = [\"/home/user/project-a\"]\n"
            ),
        )
        .unwrap();

        let cfg = load_config(Some(&path), None).expect("config with env override must load");
        assert_eq!(cfg.project_roots, vec![Utf8PathBuf::from("/opt/env-root")]);

        std::env::remove_var(PROJECT_ROOTS_ENV_VAR);
    }

    #[test]
    fn project_roots_env_splits_multiple_paths() {
        let _guard = PROJECT_ROOTS_ENV_LOCK.lock().unwrap();
        let paths = vec!["/a/first", "/b/second", "/c/third"];
        let joined = std::env::join_paths(&paths).expect("join paths must succeed");
        std::env::set_var(PROJECT_ROOTS_ENV_VAR, &joined);

        let cfg = load_config(None, None).expect("defaults with env roots must load");
        let expected: Vec<Utf8PathBuf> = paths.iter().map(Utf8PathBuf::from).collect();
        assert_eq!(cfg.project_roots, expected);

        std::env::remove_var(PROJECT_ROOTS_ENV_VAR);
    }

    #[test]
    fn config_without_skills_or_mcp_sections_uses_defaults() {
        let cfg = load_config(None, None).expect("defaults must load");
        // Optional sections absent -> None; consumers fall back to the
        // documented Default impls.
        assert!(cfg.skills.is_none(), "skills section absent -> None");
        assert!(cfg.mcp.is_none(), "mcp section absent -> None");

        let skills = SkillsConfig::default();
        assert!(!skills.enabled, "skills default off per ADR-43 decision 5");
        assert_eq!(
            skills.search_paths,
            vec!["~/.local/share/concerto/skills".to_string(), "./.concerto/skills".to_string()]
        );
        assert!(skills.auto_load);
        assert_eq!(skills.enabled_ids, None);

        let mcp = McpConfig::default();
        assert!(!mcp.enabled, "mcp defaults to disabled until the user opts in (ADR-43 §6)");
        assert!(mcp.servers.is_empty());
    }

    #[test]
    fn mcp_server_section_parses_with_env_and_timeout() {
        use std::io::Write;
        let dir = std::env::temp_dir().join(format!("concerto-mcp-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "schema_version = 5").unwrap();
        writeln!(f, "[mcp]").unwrap();
        writeln!(f, "enabled = true").unwrap();
        writeln!(f, "[[mcp.servers]]").unwrap();
        writeln!(f, "id = \"filesystem\"").unwrap();
        writeln!(f, "command = \"npx\"").unwrap();
        writeln!(f, "args = [\"-y\", \"@modelcontextprotocol/server-filesystem\", \"/safe\"]")
            .unwrap();
        writeln!(f, "enabled = true").unwrap();
        writeln!(f, "timeout_secs = 60").unwrap();
        writeln!(f, "[mcp.servers.env]").unwrap();
        writeln!(f, "FOO = \"bar\"").unwrap();

        let cfg = load_config(Some(&path), None).expect("config with [[mcp.servers]] must load");
        let mcp = cfg.mcp.expect("mcp section must be present");
        assert!(mcp.enabled);
        assert_eq!(mcp.servers.len(), 1);
        let server = &mcp.servers[0];
        assert_eq!(server.id, "filesystem");
        assert_eq!(server.command, "npx");
        assert_eq!(server.args, vec!["-y", "@modelcontextprotocol/server-filesystem", "/safe"]);
        assert!(server.enabled);
        assert_eq!(server.timeout_secs, Some(60));
        let env = server.env.as_ref().expect("env map must be present");
        assert_eq!(env.get("FOO").map(String::as_str), Some("bar"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn v4_config_file_migrates_to_schema_v7() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "schema_version = 4\n").unwrap();
        let cfg = load_config(Some(&path), None).expect("v4 config must load");
        // v4 -> v5 (version bump) -> v6 (mode/intent removal) -> v7 ([intent]
        // classifier keys), landing on the latest schema.
        assert_eq!(cfg.schema_version, 7, "v4 config must migrate to the current schema");
        assert!(cfg.skills.is_none());
        assert!(cfg.mcp.is_none());
        // v6→v7 inserts [intent] with defaults: classifier on (ADR-56).
        let intent = cfg.intent.expect("migrated config must carry [intent]");
        assert!(intent.classifier_enabled, "classifier defaults to on (ADR-56)");
    }

    /// ADR-56 §2 (C1 superseded): an existing v6 config file loads at schema 7
    /// with `[intent]` defaulted (classifier ON).
    #[test]
    fn v6_config_file_loads_with_intent_defaulted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "schema_version = 6\nsession_spend_cap_usd = 1.0\n").unwrap();
        let cfg = load_config(Some(&path), None).expect("v6 config must load");
        assert_eq!(cfg.schema_version, 7);
        assert_eq!(cfg.session_spend_cap_usd, Some(1.0));
        let intent = cfg.intent.expect("v6→v7 migration must insert [intent]");
        assert!(intent.classifier_enabled, "classifier defaults to on (ADR-56)");
        assert_eq!(intent.classifier_model, None);
        assert_eq!(intent.classifier_confidence_threshold, concerto_core::LOW_CONFIDENCE_THRESHOLD);
    }

    /// ADR-55 Phase 2c §2 (C1): a configured threshold below
    /// `LOW_CONFIDENCE_THRESHOLD` is rejected at load.
    #[test]
    fn intent_threshold_below_low_confidence_rejected_at_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "schema_version = 7\n[intent]\nclassifier_enabled = true\nclassifier_confidence_threshold = 0.5\n",
        )
        .unwrap();
        let err = load_config(Some(&path), None).expect_err("sub-0.7 threshold must be rejected");
        assert!(
            format!("{err}").contains("classifier_confidence_threshold"),
            "expected threshold rejection, got: {err}"
        );
    }

    /// ADR-55 Phase 2c §2: a threshold exactly at `LOW_CONFIDENCE_THRESHOLD`
    /// (0.7) is accepted — the boundary value clears validation.
    #[test]
    fn intent_threshold_at_low_confidence_is_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "schema_version = 7\n[intent]\nclassifier_enabled = true\nclassifier_confidence_threshold = 0.7\n",
        )
        .unwrap();
        let cfg = load_config(Some(&path), None).expect("threshold 0.7 must load");
        let intent = cfg.intent.expect("[intent] section must be present");
        assert!(intent.classifier_enabled);
        assert_eq!(intent.classifier_confidence_threshold, 0.7);
    }

    /// ADR-55 Phase 2c §2: the `classifier_model` key parses.
    #[test]
    fn intent_classifier_model_key_parses() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "schema_version = 7\n[intent]\nclassifier_enabled = true\nclassifier_model = \"claude-sonnet-4\"\n",
        )
        .unwrap();
        let cfg = load_config(Some(&path), None).expect("config with classifier_model must load");
        let intent = cfg.intent.expect("[intent] section must be present");
        assert_eq!(intent.classifier_model.as_deref(), Some("claude-sonnet-4"));
    }

    #[test]
    fn mcp_server_id_with_colon_rejected_at_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "schema_version = 5\n[mcp]\nenabled = true\n[[mcp.servers]]\nid = \"bad:id\"\ncommand = \"npx\"\n",
        )
        .unwrap();
        let err = load_config(Some(&path), None).expect_err("bad server id must be rejected");
        assert!(
            format!("{err}").contains("must not contain ':'"),
            "expected colon rejection, got: {err}"
        );
    }

    #[test]
    fn mcp_server_empty_id_rejected_at_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "schema_version = 5\n[mcp]\nenabled = true\n[[mcp.servers]]\nid = \"\"\ncommand = \"npx\"\n",
        )
        .unwrap();
        let err = load_config(Some(&path), None).expect_err("empty server id must be rejected");
        assert!(
            format!("{err}").contains("must be non-empty"),
            "expected empty-id rejection, got: {err}"
        );
    }

    #[test]
    fn context_section_parses_and_missing_section_defaults_to_none() {
        // ADR-048 §6: `[context]` is additive; absent section -> None, and a
        // partial section leaves unset knobs as None (the engine resolves
        // None -> the embedded default at runtime).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "schema_version = 5\n[context]\ntrigger_tokens = 8000\n").unwrap();
        let cfg = load_config(Some(&path), None).expect("config with [context] must load");
        let context = cfg.context.expect("[context] section must be present");
        assert_eq!(context.trigger_tokens, Some(8_000));
        assert_eq!(context.retain_user_turns, None, "unset knobs stay None");
        assert_eq!(context.minimum_user_turns, None, "unset knobs stay None");

        let without = load_config(None, None).expect("defaults must load");
        assert!(without.context.is_none(), "no [context] section -> None");
    }

    #[test]
    fn context_section_parses_all_knobs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "schema_version = 5\n[context]\ntrigger_tokens = 12000\nretain_user_turns = 2\nminimum_user_turns = 4\n",
        )
        .unwrap();
        let cfg = load_config(Some(&path), None).expect("config with full [context] must load");
        let context = cfg.context.expect("[context] section must be present");
        assert_eq!(context.trigger_tokens, Some(12_000));
        assert_eq!(context.retain_user_turns, Some(2));
        assert_eq!(context.minimum_user_turns, Some(4));
    }

    // ---- ADR-58: [orchestration] loads and validates at load time ----

    /// A valid `[orchestration]` section (named "standard") loads; absent
    /// section keeps the legacy path (legacy equivalence).
    #[test]
    fn orchestration_section_named_loads_and_absent_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "schema_version = 7\n[orchestration]\nschema_version = 1\n[orchestration.blueprint]\nname = \"standard\"\n",
        )
        .unwrap();
        let cfg = load_config(Some(&path), None).expect("named blueprint must load");
        let orchestration = cfg.orchestration.expect("[orchestration] must be present");
        assert_eq!(orchestration.blueprint.name.as_deref(), Some("standard"));

        let without = load_config(None, None).expect("defaults must load");
        assert!(without.orchestration.is_none(), "no [orchestration] section -> None");
    }

    /// An inline `[orchestration]` blueprint with no primary `Execution` stage
    /// and no terminal kind now LOADS — the removed rulebook (a)/(b) gates no
    /// longer reject it (a single research-stage pipeline is valid).
    #[test]
    fn orchestration_blueprint_without_primary_or_terminal_loads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"schema_version = 7
[orchestration]
schema_version = 1
[orchestration.blueprint.inline]
schema_version = 1
name = "research-only-seed"
[orchestration.blueprint.inline.pipeline]
[[orchestration.blueprint.inline.pipeline.stages]]
tag = "research"
label = "Research"
kind = "research"
"#,
        )
        .unwrap();
        let cfg = load_config(Some(&path), None)
            .expect("a blueprint without a primary/terminal stage must load");
        let orchestration = cfg.orchestration.expect("[orchestration] must be present");
        let resolved =
            orchestration.resolve(&[], None).expect("the blueprint must validate and resolve");
        assert_eq!(resolved.stages.len(), 1);
        assert_eq!(resolved.stages[0].def.tag, "research");
    }

    /// The `[orchestration]` section uses `deny_unknown_fields`: a typo'd key
    /// is a hard load error (the asymmetry with `AppConfig`, ADR-58 §6).
    #[test]
    fn orchestration_section_denies_unknown_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "schema_version = 7\n[orchestration]\nschema_versoin = 1\n").unwrap();
        let err = load_config(Some(&path), None).expect_err("unknown orchestration key rejected");
        assert!(format!("{err}").to_lowercase().contains("orchestration"), "{err}");
    }

    /// An `[orchestration]` section pointing at a missing include file fails
    /// at load with the include-file error surfaced.
    #[test]
    fn orchestration_missing_include_file_rejected_at_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "schema_version = 7\n[orchestration]\nschema_version = 1\n[orchestration.blueprint]\ninclude = \"nope.toml\"\n",
        )
        .unwrap();
        let err = load_config(Some(&path), None)
            .expect_err("missing include file must be rejected at load");
        assert!(format!("{err}").contains("include file"), "{err}");
    }

    /// The blueprint include file itself is read, validated, and resolved at
    /// load (ADR-58 D4: `orchestration.blueprint.toml` merged at load).
    #[test]
    fn orchestration_include_file_loads_at_load_time() {
        let dir = tempfile::tempdir().unwrap();
        let include = dir.path().join(crate::blueprint::BLUEPRINT_INCLUDE_FILE);
        let blueprint = crate::blueprint::default_blueprint();
        std::fs::write(&include, toml::to_string_pretty(&blueprint).expect("blueprint serializes"))
            .unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "schema_version = 7\n[orchestration]\nschema_version = 1\n[orchestration.blueprint]\ninclude = \"orchestration.blueprint.toml\"\n",
        )
        .unwrap();
        let cfg =
            load_config(Some(&path), None).expect("include-file blueprint must load and validate");
        let orchestration = cfg.orchestration.expect("[orchestration] must be present");
        assert_eq!(
            orchestration.blueprint.include.as_deref(),
            Some("orchestration.blueprint.toml")
        );
    }

    // ---- ADR-58 load-time extensions: B4-under-orchestration (custom
    // agents, B3 removed), N4 (relationships) ----

    /// B3 removed: an explicit `fs_write = true` on a user agent staffed in a
    /// non-Execution stage (research) is a PLAIN FLAG, not a widening hard
    /// error — the config owns the roster and its capability flags. Loads.
    #[test]
    fn b3_removed_stage_flags_are_plain_flags_not_widening_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"schema_version = 7
[multi_agent]
[[multi_agent.custom_agents]]
id = "docs-writer"
name = "Docs Writer"
role = "docs-writer"
stage = "research"
[multi_agent.custom_agents.capabilities]
fs_write = true
"#,
        )
        .unwrap();
        let cfg = load_config(Some(&path), None)
            .expect("an explicit fs_write flag on a research-staffed agent must load");
        let agent = &cfg.multi_agent.expect("[multi_agent] present").custom_agents[0];
        assert_eq!(agent.id, "docs-writer");
        assert_eq!(agent.capabilities.fs_write, Some(true));
    }

    /// The same explicit `fs_write = true` passes on an Execution-kind stage
    /// tag (`implement`) — flags are orthogonal to the stage kind.
    #[test]
    fn b3_removed_execution_stage_flags_postive_control() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"schema_version = 7
[multi_agent]
[[multi_agent.custom_agents]]
id = "docs-writer"
name = "Docs Writer"
role = "docs-writer"
stage = "implement"
[multi_agent.custom_agents.capabilities]
fs_write = true
"#,
        )
        .unwrap();
        let cfg = load_config(Some(&path), None).expect("Execution-staffed fs_write must load");
        let agent = &cfg.multi_agent.expect("[multi_agent] present").custom_agents[0];
        assert_eq!(agent.id, "docs-writer");
    }

    /// B4 (rulebook (g)) is scoped to `[orchestration]`: absent the section,
    /// legacy stage tag strings are accepted untouched (`documentation`);
    /// present, an unknown tag is a hard load error with retag/register
    /// guidance; `run_once` and absent stage always pass.
    #[test]
    fn b4_unknown_agent_stage_tag_scoped_to_orchestration() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");

        // Without `[orchestration]`: the legacy tag string loads untouched.
        std::fs::write(
            &path,
            r#"schema_version = 7
[multi_agent]
[[multi_agent.custom_agents]]
id = "docs-writer"
name = "Docs Writer"
role = "docs-writer"
stage = "documentation"
"#,
        )
        .unwrap();
        let cfg = load_config(Some(&path), None)
            .expect("a legacy stage tag must load when [orchestration] is absent");
        let agent = &cfg.multi_agent.expect("[multi_agent] present").custom_agents[0];
        assert_eq!(agent.stage.as_ref().map(|s| s.as_str()), Some("documentation"));

        // Under `[orchestration]`: the same unknown tag is a hard load error.
        std::fs::write(
            &path,
            r#"schema_version = 7
[orchestration]
schema_version = 1
[multi_agent]
[[multi_agent.custom_agents]]
id = "docs-writer"
name = "Docs Writer"
role = "docs-writer"
stage = "documentation"
"#,
        )
        .unwrap();
        let err = load_config(Some(&path), None).expect_err("unknown stage tag must be rejected");
        let msg = format!("{err}");
        assert!(
            msg.contains("docs-writer")
                && msg.contains("documentation")
                && msg.contains("run_once"),
            "expected agent/stage naming + run_once guidance, got: {msg}"
        );

        // `run_once` is the sanctioned Freeform tag and loads under
        // `[orchestration]`.
        std::fs::write(
            &path,
            r#"schema_version = 7
[orchestration]
schema_version = 1
[multi_agent]
[[multi_agent.custom_agents]]
id = "docs-writer"
name = "Docs Writer"
role = "docs-writer"
stage = "run_once"
"#,
        )
        .unwrap();
        let cfg = load_config(Some(&path), None).expect("run_once stage must load");
        let agent = &cfg.multi_agent.expect("[multi_agent] present").custom_agents[0];
        assert_eq!(agent.stage.as_ref().map(|s| s.as_str()), Some("run_once"));

        // An absent stage (Freeform semantics) passes untouched.
        std::fs::write(
            &path,
            r#"schema_version = 7
[multi_agent]
[[multi_agent.custom_agents]]
id = "docs-writer"
name = "Docs Writer"
role = "docs-writer"
"#,
        )
        .unwrap();
        let cfg = load_config(Some(&path), None).expect("stage-less agent must load");
        let agent = &cfg.multi_agent.expect("[multi_agent] present").custom_agents[0];
        assert_eq!(agent.stage, None, "stored field is never mutated");
    }

    /// N4: a `multi_agent.relationships` kind string outside the closed
    /// catalog (`supervises`/`provides_context_to`/`reports_to`/`owns_design`)
    /// is a load error (the runtime already hard-fails; this moves the
    /// failure earlier).
    #[test]
    fn n4_unknown_relationship_kind_rejected_at_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"schema_version = 7
[multi_agent]
[[multi_agent.relationships]]
from = "reviewer"
to = "coder"
relationship = "watches"
"#,
        )
        .unwrap();
        let err = load_config(Some(&path), None)
            .expect_err("unknown relationship kind must be rejected at load");
        let msg = format!("{err}");
        assert!(
            msg.contains("watches") && msg.contains("reviewer") && msg.contains("coder"),
            "expected relationship naming, got: {msg}"
        );
    }
}
