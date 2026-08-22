use camino::Utf8PathBuf;
use concerto_core::types::{AgentId, AgentStage, OutputMode};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use crate::blueprint::{OrchestrationConfig, ResolvedBlueprint};
use crate::credentials::CredentialStore;
use crate::shell::ShellSettings;
use crate::ConfigError;

/// Bump this whenever a breaking change is made to [`AppConfig`]'s shape.
/// See "Schema Migration Policy" in the roadmap — config migration is print
/// clear-error-and-stop for now; automatic migration is a later phase.
///
/// v3 -> v4: add `shell_settings` (ADR-28 shell profiles). Additive; old
/// configs migrate to host-detected profiles and one canonical selection.
///
/// v4 -> v5: add `[skills]` and `[mcp]` sections (ADR-43). Additive; old
/// configs keep working via serde defaults (skills enabled with standard
/// search paths, MCP disabled) and an insert-only migration step.
///
/// v5 -> v6: drop `mode` and `[intent]` (ADR-55 Phase 1e) — the intent gate is
/// now the only routing path and there is no user-selectable Build/Chat/Plan
/// mode. The keys simply cease to exist; stale TOML keys are ignored at load
/// because `AppConfig` has no `deny_unknown_fields`.
///
/// v6 -> v7: re-add `[intent]` (ADR-55 Phase 2c) with the three classifier
/// keys only. This does NOT resurrect the `mode`/`enabled` keys dropped at v6 —
/// the gate stays always-on; v7 adds only `classifier_enabled`,
/// `classifier_model`, and `classifier_confidence_threshold`. Additive;
/// `migrate_v6_to_v7` inserts the section with defaults when absent.
///
/// ADR-56 supersedes the Phase 2c `classifier_enabled` default pin (off → on):
/// the LLM classifier is the primary intent decider, so the omitted-key
/// default now enables it. Nothing else changes.
pub const SCHEMA_VERSION: u32 = 7;

// ---------------------------------------------------------------------------
// Provider retry configuration
// ---------------------------------------------------------------------------

fn default_retry_enabled() -> bool {
    true
}

fn default_retry_initial_delay_ms() -> u64 {
    2_000
}

fn default_retry_max_delay_ms() -> u64 {
    30_000
}

fn default_retry_multiplier() -> f64 {
    2.0
}

fn default_retry_jitter() -> bool {
    true
}

fn default_respect_retry_after() -> bool {
    true
}

fn default_retry_max_attempts() -> u32 {
    8
}

fn default_retry_max_elapsed_seconds() -> Option<u64> {
    Some(15 * 60)
}

fn default_time_to_first_byte_seconds() -> u64 {
    120
}

fn default_stream_idle_timeout_seconds() -> u64 {
    300
}

/// Centralized configuration for automatic retries of transient provider
/// failures (HTTP 429/5xx, connection resets, timeouts, etc.).
///
/// This controls *transport recovery* only. It is deliberately separate from
/// `ProviderConfig::timeout_seconds` (per-request timeout) and from the
/// agent's autonomy/continuation budget.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RetryConfig {
    /// Enable automatic retries for transient provider failures.
    #[serde(default = "default_retry_enabled")]
    pub enabled: bool,

    /// Initial exponential-backoff delay (milliseconds).
    #[serde(default = "default_retry_initial_delay_ms")]
    pub initial_delay_ms: u64,

    /// Maximum delay between retries (milliseconds).
    #[serde(default = "default_retry_max_delay_ms")]
    pub max_delay_ms: u64,

    /// Exponential multiplier, normally 2.0.
    #[serde(default = "default_retry_multiplier")]
    pub multiplier: f64,

    /// Add randomized jitter to avoid synchronized retries.
    #[serde(default = "default_retry_jitter")]
    pub jitter: bool,

    /// Fixed delay override (milliseconds).
    ///
    /// When `Some`, use this delay for every retry instead of calculating
    /// exponential backoff. Provider `Retry-After` metadata still takes
    /// precedence unless `respect_retry_after` is false.
    #[serde(default)]
    pub fixed_delay_ms: Option<u64>,

    /// Respect `Retry-After` and `retry-after-ms` response headers.
    #[serde(default = "default_respect_retry_after")]
    pub respect_retry_after: bool,

    /// Maximum attempts for one logical provider request, including the first.
    #[serde(default = "default_retry_max_attempts")]
    pub max_attempts: u32,

    /// Elapsed-time safety fuse for one uninterrupted provider outage.
    ///
    /// Explicit `None` remains supported for migrated configurations, but the
    /// production default is finite.
    #[serde(default = "default_retry_max_elapsed_seconds")]
    pub max_elapsed_seconds: Option<u64>,

    /// Deadline for receiving response headers / the provider response stream.
    #[serde(default = "default_time_to_first_byte_seconds")]
    pub time_to_first_byte_seconds: u64,

    /// Maximum quiet period between response chunks. This is reset whenever a
    /// chunk arrives, so a long but progressing generation remains valid.
    #[serde(default = "default_stream_idle_timeout_seconds")]
    pub stream_idle_timeout_seconds: u64,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            initial_delay_ms: 2_000,
            max_delay_ms: 30_000,
            multiplier: 2.0,
            jitter: true,
            fixed_delay_ms: None,
            respect_retry_after: true,
            max_attempts: default_retry_max_attempts(),
            max_elapsed_seconds: default_retry_max_elapsed_seconds(),
            time_to_first_byte_seconds: default_time_to_first_byte_seconds(),
            stream_idle_timeout_seconds: default_stream_idle_timeout_seconds(),
        }
    }
}

impl RetryConfig {
    /// Validate retry settings during config loading.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.initial_delay_ms == 0 && self.fixed_delay_ms.is_none() {
            return Err(ConfigError::InvalidValue(
                "retry.initial_delay_ms must be greater than zero".into(),
            ));
        }

        if self.max_delay_ms == 0 {
            return Err(ConfigError::InvalidValue(
                "retry.max_delay_ms must be greater than zero".into(),
            ));
        }

        if self.multiplier < 1.0 || !self.multiplier.is_finite() {
            return Err(ConfigError::InvalidValue(
                "retry.multiplier must be finite and at least 1.0".into(),
            ));
        }

        if let Some(delay) = self.fixed_delay_ms {
            if delay == 0 {
                return Err(ConfigError::InvalidValue(
                    "retry.fixed_delay_ms must be greater than zero".into(),
                ));
            }
        }

        if self.max_attempts == 0 {
            return Err(ConfigError::InvalidValue(
                "retry.max_attempts must be greater than zero".into(),
            ));
        }
        if self.max_elapsed_seconds == Some(0) {
            return Err(ConfigError::InvalidValue(
                "retry.max_elapsed_seconds must be greater than zero when set".into(),
            ));
        }
        if self.time_to_first_byte_seconds == 0 {
            return Err(ConfigError::InvalidValue(
                "retry.time_to_first_byte_seconds must be greater than zero".into(),
            ));
        }
        if self.stream_idle_timeout_seconds == 0 {
            return Err(ConfigError::InvalidValue(
                "retry.stream_idle_timeout_seconds must be greater than zero".into(),
            ));
        }

        Ok(())
    }
}

// ---- ADR-55: intent routing and intent-gated authorization ----------------

fn default_classifier_enabled() -> bool {
    // ADR-56 (model-first): when `[intent] classifier_enabled` is true the LLM
    // classifier is the PRIMARY intent decider for every non-fast-path
    // message. The deterministic router (concerto_core::intent::route) remains
    // the offline / fail-soft fallback and supplies the two fast-path
    // detections (negation-override, smalltalk). Default is ON — one bounded
    // model call per non-fast-path message is the intended primary path, not
    // an opt-in extra (ADR-56 §1/§2).
    true
}

fn default_classifier_confidence_threshold() -> f32 {
    // Bound to the gate's constant (not a literal) so no configured threshold
    // can create a [threshold, LOW_CONFIDENCE_THRESHOLD) band where a
    // classifier Execute re-route would miss the gate's arm-1 dialog
    // (ADR-55 Phase 2c §2).
    concerto_core::LOW_CONFIDENCE_THRESHOLD
}

/// LLM intent classifier configuration (ADR-55 Phase 2c §2; ADR-56).
///
/// `[intent]` is additive and default-on. When the classifier is enabled it is
/// the primary intent decider for every non-fast-path request (ADR-56 §1); a
/// missing `[intent]` section or a disabled classifier leaves the run
/// deterministic — the offline / fail-soft fallback (ADR-56 §3). The section
/// carries the three classifier keys only — the `mode`/`enabled` keys dropped
/// at v6 are NOT resurrected; the intent gate stays always-on.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct IntentConfig {
    /// Whether the LLM classifier is the primary intent decider. Default:
    /// true (ADR-56 §2) — the classifier runs for every non-fast-path message;
    /// only the negation-override and smalltalk fast paths bypass it (ADR-56
    /// §1).
    #[serde(default = "default_classifier_enabled")]
    pub classifier_enabled: bool,

    /// Model used for the classifier call. `None` = the run's effective chat
    /// model (ADR-55 Phase 2c §2, per §9 "same chat model").
    #[serde(default)]
    pub classifier_model: Option<String>,

    /// Minimum classifier confidence required to re-route the deterministic
    /// routing result to the suggested outcome. Default: 0.7 — validated at
    /// config load to be `>= concerto_core::LOW_CONFIDENCE_THRESHOLD` (the
    /// gate's constant), so a classifier Execute re-route always clears the
    /// intent gate's arm-1 confirmation dialog (ADR-55 Phase 2c §2; ADR-56 §4
    /// keeps the invariant).
    #[serde(default = "default_classifier_confidence_threshold")]
    pub classifier_confidence_threshold: f32,
}

impl Default for IntentConfig {
    fn default() -> Self {
        Self {
            classifier_enabled: default_classifier_enabled(),
            classifier_model: None,
            classifier_confidence_threshold: default_classifier_confidence_threshold(),
        }
    }
}

impl IntentConfig {
    /// Validate the classifier settings during config loading, mirroring
    /// [`RetryConfig::validate`].
    ///
    /// The threshold is bound to `concerto_core::LOW_CONFIDENCE_THRESHOLD`
    /// (not a literal): the intent gate's `is_confident_execute`/arm-1 dialog
    /// uses that constant, so a configured threshold below it could re-route a
    /// classifier Execute at a confidence the gate treats as ambiguous —
    /// landing it in the read-only wildcard instead of the confirmation
    /// dialog (ADR-55 Phase 2c §2, never-grant invariant).
    pub fn validate(&self) -> Result<(), ConfigError> {
        if !self.classifier_confidence_threshold.is_finite()
            || self.classifier_confidence_threshold < concerto_core::LOW_CONFIDENCE_THRESHOLD
        {
            return Err(ConfigError::InvalidValue(format!(
                "intent.classifier_confidence_threshold must be finite and >= {} \
                 (concerto_core::LOW_CONFIDENCE_THRESHOLD)",
                concerto_core::LOW_CONFIDENCE_THRESHOLD
            )));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub schema_version: u32,

    /// Legacy default-provider field retained for deserializing older configs.
    /// Current Settings saves clear it; execution is model-first.
    pub primary_provider: Option<String>,

    /// Legacy single-provider record retained for migration compatibility.
    pub primary_provider_config: Option<ProviderConfig>,

    /// Legacy fallback-provider field retained for migration compatibility.
    pub fallback_provider: Option<String>,

    /// Legacy fallback-provider record retained for migration compatibility.
    pub fallback_provider_config: Option<ProviderConfig>,

    pub ollama_base_url: Option<String>,

    /// Per-session spend cap in USD. `None` = no cap.
    pub session_spend_cap_usd: Option<f64>,

    /// Policy rules for tool execution gating.
    pub policy: Option<PolicyConfig>,

    /// Multi-agent configuration.
    pub multi_agent: Option<MultiAgentConfig>,

    /// Observability export configuration.
    /// `None` = all exporters disabled.
    pub observability: Option<ObservabilityConfig>,

    // ---- Phase 7.1: plugin configuration ------------------------------------------
    /// WASM plugin system configuration.
    #[serde(default)]
    pub plugins: Option<PluginConfig>,

    // ---- ADR-43: skills and MCP configuration -------------------------------------
    /// Skill pack configuration. `None` = defaults (enabled, standard search
    /// paths, auto-load).
    #[serde(default)]
    pub skills: Option<SkillsConfig>,

    /// MCP client configuration. `None` = disabled with no servers.
    #[serde(default)]
    pub mcp: Option<McpConfig>,

    /// Multi-provider model management settings.
    /// `None` = single-provider mode (backward compatible).
    #[serde(default)]
    pub model_settings: Option<ModelSettings>,

    /// Startup update check configuration.
    /// `None` = defaults to enabled (check on startup).
    #[serde(default)]
    pub updates: Option<UpdatesConfig>,

    /// Centralized provider retry configuration.
    /// Absent in older configs defaults to [`RetryConfig::default`].
    #[serde(default)]
    pub retry: RetryConfig,

    /// Shared project-memory behavior for every frontend.
    #[serde(default)]
    pub memory: MemoryConfig,

    /// Shell profile and toolchain configuration (ADR-28/ADR-30).
    /// `None` = detect installed host shells. Additive since v4.
    #[serde(default)]
    pub shell_settings: Option<ShellSettings>,

    /// Shared project-root allowlist (ADR-44).
    ///
    /// When non-empty, the desktop gates out-of-root project opens and the
    /// api-server refuses session roots outside this allowlist. Empty (the
    /// default) keeps local-first behavior permissive. The
    /// `CONCERTO_PROJECT_ROOTS` environment variable (path-separated) replaces
    /// this value on load; see `concerto_config::load_config`.
    #[serde(default)]
    pub project_roots: Vec<Utf8PathBuf>,

    /// Deterministic context budget for the single-agent loop (ADR-048).
    ///
    /// When non-None, the knobs override the engine's embedded defaults
    /// (`trigger_tokens` 16000, `retain_user_turns` 4, `minimum_user_turns` 6
    /// in `context_compaction.rs`). Additive serde-default only: old configs
    /// without a `[context]` section load unchanged and keep today's behavior.
    #[serde(default)]
    pub context: Option<ContextConfig>,

    /// LLM intent classifier (ADR-55 Phase 2c §2; ADR-56).
    ///
    /// `None` (a config without a `[intent]` section) disables the classifier
    /// — the deterministic router is the only routing path, serving as the
    /// offline / fail-soft fallback (ADR-56 §3). `migrate_v6_to_v7` inserts
    /// the section with defaults (classifier ON) when absent. Additive: no
    /// `deny_unknown_fields`, so stale v6 keys keep loading.
    #[serde(default)]
    pub intent: Option<IntentConfig>,

    /// Tool-level runtime settings applied at session start.
    ///
    /// `None` (a config without a `[tools]` section) keeps the embedded
    /// defaults — currently `git_auto_init` is ON, so brand-new project
    /// directories get a bare `git init` before the agent writes files. See
    /// `concerto_tools::git_init`. Additive: no `deny_unknown_fields`, so a
    /// config file carrying stale `[tools]` keys keeps loading.
    #[serde(default)]
    pub tool_settings: Option<ToolSettings>,

    /// ADR-58 `[orchestration]` — the Orchestration Blueprint pipeline.
    ///
    /// `None` (no `[orchestration]` table) keeps the engine's embedded
    /// five-stage pipeline: nothing here runs, so every pre-existing config
    /// loads unchanged (legacy equivalence). When present, the section opts
    /// into the blueprint data model: the selected blueprint (named variant,
    /// include file, or inline definition) is loaded and validated against
    /// the relaxed rulebook at config load (ADR-58 Consequences — validation
    /// shifts to load time). The engine consumes the resolved blueprint in
    /// the P2+P3 table-driven rewrite.
    #[serde(default)]
    pub orchestration: Option<OrchestrationConfig>,

    /// ADR-58 P2+P3: the validated, resolved blueprint captured at load.
    ///
    /// Derived state — never round-trips through config files
    /// (`#[serde(skip)]`); populated exactly once by the load seam
    /// (`crates/config/src/lib.rs`, `load_config_layers`) and consumed
    /// read-only by the runtime facade (`BlueprintFacade`).
    /// `None` means "not yet resolved" (constructors and direct struct
    /// literals); the runtime attaches the default `standard` blueprint when a
    /// config object was not built through the load seam.
    #[serde(skip)]
    pub resolved_blueprint: Option<Arc<ResolvedBlueprint>>,
}

impl PartialEq for AppConfig {
    /// Equality over the persisted surface. The derived
    /// `resolved_blueprint` (ADR-58 P2+P3) is deliberately excluded: it is
    /// filled at load in the validation seam, so two configs sharing the same
    /// persisted bytes compare equal whether or not either was resolved
    /// (design doc §1.2, review F8/Q1).
    fn eq(&self, other: &Self) -> bool {
        self.schema_version == other.schema_version
            && self.primary_provider == other.primary_provider
            && self.primary_provider_config == other.primary_provider_config
            && self.fallback_provider == other.fallback_provider
            && self.fallback_provider_config == other.fallback_provider_config
            && self.ollama_base_url == other.ollama_base_url
            && self.session_spend_cap_usd == other.session_spend_cap_usd
            && self.policy == other.policy
            && self.multi_agent == other.multi_agent
            && self.observability == other.observability
            && self.plugins == other.plugins
            && self.skills == other.skills
            && self.mcp == other.mcp
            && self.model_settings == other.model_settings
            && self.updates == other.updates
            && self.retry == other.retry
            && self.memory == other.memory
            && self.shell_settings == other.shell_settings
            && self.project_roots == other.project_roots
            && self.context == other.context
            && self.intent == other.intent
            && self.tool_settings == other.tool_settings
            && self.orchestration == other.orchestration
    }
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            primary_provider: None,
            primary_provider_config: None,
            fallback_provider: None,
            fallback_provider_config: None,
            ollama_base_url: Some("http://localhost:11434".into()),
            session_spend_cap_usd: None,
            policy: None,
            multi_agent: None,
            observability: None,
            plugins: None,
            skills: None,
            mcp: None,
            model_settings: None,
            updates: None,
            retry: RetryConfig::default(),
            memory: MemoryConfig::default(),
            shell_settings: None,
            project_roots: Vec::new(),
            context: None,
            intent: None,
            tool_settings: None,
            orchestration: None,
            resolved_blueprint: None,
        }
    }
}

/// `[context]` — deterministic context budget for the single-agent loop (ADR-048).
///
/// Additive serde-default only: every knob is optional, and a missing
/// `[context]` section (or missing knob) keeps the engine's embedded defaults
/// from `context_compaction.rs` (`trigger_tokens` 16000, `retain_user_turns` 4,
/// `minimum_user_turns` 6). No schema migration is required; this section is a
/// first-class v5 field but defaults to `None` for old configs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct ContextConfig {
    /// Token budget that triggers deterministic compaction. When unset the
    /// default trigger (`16000`) applies.
    #[serde(default)]
    pub trigger_tokens: Option<u64>,
    /// How many most-recent user turns are always retained verbatim after
    /// compaction. When unset the default (`4`) applies.
    #[serde(default)]
    pub retain_user_turns: Option<usize>,
    /// Minimum user turns before compaction may fire. When unset the default
    /// (`6`) applies.
    #[serde(default)]
    pub minimum_user_turns: Option<usize>,
}

/// Runtime memory controls shared by CLI and desktop.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MemoryConfig {
    /// Enable retrieval, background indexing, and memory persistence.
    #[serde(default = "default_memory_enabled")]
    pub enabled: bool,
    /// Default retention window for indexed chunks.
    #[serde(default = "default_memory_ttl_days")]
    pub ttl_days: u16,
    /// Additional project-relative patterns excluded from background indexing.
    #[serde(default)]
    pub exclude_patterns: Vec<String>,
    /// Optional project-relative or absolute ignore file loaded after
    /// `.gitignore` and `.concertoignore`.
    #[serde(default)]
    pub ignore_file: Option<Utf8PathBuf>,
}

fn default_memory_enabled() -> bool {
    true
}

fn default_memory_ttl_days() -> u16 {
    30
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            enabled: default_memory_enabled(),
            ttl_days: default_memory_ttl_days(),
            exclude_patterns: Vec::new(),
            ignore_file: None,
        }
    }
}

impl MemoryConfig {
    pub fn validate(&self) -> Result<(), ConfigError> {
        if !(1..=365).contains(&self.ttl_days) {
            return Err(ConfigError::InvalidValue(
                "memory.ttl_days must be between 1 and 365".into(),
            ));
        }
        Ok(())
    }
}

impl AppConfig {
    /// Resolve shell settings and refresh the host-detected profile list.
    pub fn resolved_shell_settings(&self) -> ShellSettings {
        match &self.shell_settings {
            Some(settings) => settings.clone().normalized_for_host(),
            None => ShellSettings::default(),
        }
    }
}

// ---- Tool settings: automatic `git init` -------------------------------------

/// Tool-level runtime settings applied at session start.
///
/// `[tools]` is additive and default-on: a missing `[tools]` section keeps
/// every knob at its embedded default. The only knob today is `git_auto_init`
/// — the session manager runs a bare `git init` for project directories that
/// are not yet inside a git repository (see `concerto_tools::git_init`). No
/// initial commit, `.gitignore`, identity, or remote is ever created; the
/// first commit stays the agent's job.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolSettings {
    /// Automatically run a bare `git init` for a project directory at session
    /// start when it is not already inside a git repository. Default: true.
    /// Set `[tools] git_auto_init = false` to disable the subprocess entirely
    /// (e.g. host environments that manage repos themselves).
    #[serde(default = "default_true")]
    pub git_auto_init: bool,
}

impl Default for ToolSettings {
    fn default() -> Self {
        Self { git_auto_init: true }
    }
}

// ---- Phase 8: observability export configuration ------------------------------

/// Observability export configuration. All fields are `Option` so that
/// leaving a section out of the config file defaults to disabled.
///
/// Three optional exporters: Prometheus, OTLP, Langfuse.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ObservabilityConfig {
    /// Prometheus metrics export port (e.g. 9091).
    /// `None` = Prometheus export disabled.
    pub prometheus_port: Option<u16>,

    /// Service name reported in traces and metrics.
    #[serde(default = "default_service_name")]
    pub service_name: String,

    /// OTLP HTTP endpoint, e.g. "http://localhost:4318". `None` = OTLP disabled.
    #[serde(default)]
    pub otlp_endpoint: Option<String>,

    /// Langfuse host, e.g. "https://cloud.langfuse.com". `None` = Langfuse disabled.
    #[serde(default)]
    pub langfuse_host: Option<String>,

    /// Langfuse public key. When starts with "keyring:", resolve via keyring.
    #[serde(default)]
    pub langfuse_public_key: Option<String>,

    /// Langfuse secret key. When starts with "keyring:", resolve via keyring.
    #[serde(default)]
    pub langfuse_secret_key: Option<String>,
}

fn default_service_name() -> String {
    "concerto".into()
}

impl Default for ObservabilityConfig {
    fn default() -> Self {
        Self {
            prometheus_port: None,
            service_name: default_service_name(),
            otlp_endpoint: None,
            langfuse_host: None,
            langfuse_public_key: None,
            langfuse_secret_key: None,
        }
    }
}

// ---- Startup update check configuration -----------------------------------

/// Configuration for the non-blocking startup update check.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UpdatesConfig {
    /// Whether to check for updates on startup. Default: true.
    #[serde(default = "default_updates_check_on_startup")]
    pub check_on_startup: bool,
    /// Custom update endpoint URL. `None` = use crates.io API. Source builds
    /// should disable the check while the workspace remains unpublished.
    #[serde(default)]
    pub update_endpoint: Option<String>,
}

fn default_updates_check_on_startup() -> bool {
    true
}

impl Default for UpdatesConfig {
    fn default() -> Self {
        Self { check_on_startup: true, update_endpoint: None }
    }
}

// ---- Phase 7.1: plugin configuration ------------------------------------------
/// WASM plugin system configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct PluginConfig {
    /// Whether the plugin system is enabled.
    /// Default: false (disabled until UI/CLI wiring is stable).
    #[serde(default)]
    pub enabled: bool,

    /// Directories to search for plugin .wasm files.
    /// If empty, the default XDG data directory is used.
    #[serde(default)]
    pub search_paths: Vec<String>,

    /// Whether to scan the bundled plugin directory.
    #[serde(default)]
    pub bundled_plugins_enabled: bool,

    /// Whether to auto-load discovered plugins on startup.
    #[serde(default)]
    pub auto_load: bool,
}

// ---------------------------------------------------------------------------
// ADR-43 — skills and MCP client configuration
// ---------------------------------------------------------------------------

fn default_skills_enabled() -> bool {
    // ADR-43 decision 5: skill activation is explicit and default-off for
    // safety; users opt in per project via the [skills] section.
    false
}

fn default_skills_search_paths() -> Vec<String> {
    vec!["~/.local/share/concerto/skills".into(), "./.concerto/skills".into()]
}

fn default_skills_auto_load() -> bool {
    true
}

/// Skill pack configuration (ADR-43). Skills are local filesystem packs whose
/// instructions are injected into prompts; they never execute code.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SkillsConfig {
    /// Whether skill discovery and instruction injection are enabled.
    /// Default: false (ADR-43 decision 5: skill activation is explicit and
    /// default-off for safety; users opt in per project via `[skills]`).
    #[serde(default = "default_skills_enabled")]
    pub enabled: bool,

    /// Directories to search for skill packs. Defaults to the per-user and
    /// per-project locations.
    #[serde(default = "default_skills_search_paths")]
    pub search_paths: Vec<String>,

    /// Whether to auto-load all discovered skills on startup.
    /// Default: true.
    #[serde(default = "default_skills_auto_load")]
    pub auto_load: bool,

    /// Explicit allow-list of skill ids to load. `None` = all discovered
    /// skills are candidates (subject to `auto_load`).
    #[serde(default)]
    pub enabled_ids: Option<Vec<String>>,

    /// Hard character cap on the injected skills section. `None` = use the
    /// orchestrator's default budget (4000 characters) — the section is
    /// truncated to this budget with a clear marker (ADR-43 decision 5).
    #[serde(default)]
    pub max_chars: Option<usize>,
}

impl Default for SkillsConfig {
    fn default() -> Self {
        Self {
            enabled: default_skills_enabled(),
            search_paths: default_skills_search_paths(),
            auto_load: default_skills_auto_load(),
            enabled_ids: None,
            max_chars: None,
        }
    }
}

fn default_mcp_enabled() -> bool {
    false
}

fn default_mcp_server_enabled() -> bool {
    true
}

/// MCP client configuration (ADR-43). Each configured server is a stdio child
/// process whose tools are bridged into the shared `ToolRegistry`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct McpConfig {
    /// Whether the MCP client is enabled. Default: false — MCP servers are
    /// network-capable child processes and require explicit opt-in.
    #[serde(default = "default_mcp_enabled")]
    pub enabled: bool,

    /// Configured MCP servers. Default: empty.
    #[serde(default)]
    pub servers: Vec<McpServerConfig>,
}

impl Default for McpConfig {
    fn default() -> Self {
        Self { enabled: default_mcp_enabled(), servers: Vec::new() }
    }
}

impl McpConfig {
    /// Validate server entries at config load time (ADR-43 §4): every server
    /// id must be non-empty, contain no `:` — tools are namespaced
    /// `mcp:<server_id>:<tool_name>` — and be unique. Duplicate ids are
    /// rejected here so `McpManager` registration can never silently collide;
    /// the manager additionally re-checks as defense in depth.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let mut seen = std::collections::HashSet::new();
        for server in &self.servers {
            if server.id.is_empty() {
                return Err(ConfigError::InvalidValue("mcp.servers[].id must be non-empty".into()));
            }
            if server.id.contains(':') {
                return Err(ConfigError::InvalidValue(format!(
                    "mcp.servers[].id '{}' must not contain ':'",
                    server.id
                )));
            }
            if !seen.insert(server.id.as_str()) {
                return Err(ConfigError::InvalidValue(format!(
                    "mcp.servers[].id '{}' is duplicated — each server id must be unique",
                    server.id
                )));
            }
        }
        Ok(())
    }
}

/// A single MCP stdio server definition (ADR-43).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct McpServerConfig {
    /// Unique server id, used to namespace tools as
    /// `mcp:<server_id>:<tool_name>`. Must be non-empty and contain no `:`.
    pub id: String,

    /// Executable to spawn for the server.
    pub command: String,

    /// Arguments passed to the executable.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,

    /// Extra environment variables for the child process. Secrets are never
    /// stored in TOML (keyring integration is deferred).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<BTreeMap<String, String>>,

    /// Whether this server is enabled. Default: true.
    #[serde(default = "default_mcp_server_enabled")]
    pub enabled: bool,

    /// Per-call timeout in seconds; `None` = crate default (60s), hard cap
    /// enforced in the bridge.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
}

// ---------------------------------------------------------------------------
// Phase 2 — policy rule configuration
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PolicyConfig {
    pub rules: Vec<PolicyRuleDef>,
    /// Optional time window configuration for auto-approval.
    pub time_window: Option<PolicyTimeWindowConfig>,
}

/// Time window configuration for auto-approval of low-cost operations
/// outside business hours.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PolicyTimeWindowConfig {
    /// Start hour (0-23, inclusive).
    pub start_hour: u8,
    /// End hour (0-23, inclusive).
    pub end_hour: u8,
    /// IANA timezone name, e.g. "America/New_York".
    pub timezone: String,
    /// Operations with estimated cost below this threshold are auto-approved
    /// when the current time is outside the configured window.
    pub auto_approve_below_usd: f64,
}

impl PolicyTimeWindowConfig {
    /// Validate the timezone string at config load time.
    pub fn validate(&self) -> Result<(), crate::ConfigError> {
        self.timezone.parse::<chrono_tz::Tz>().map(|_| ()).map_err(|e| {
            crate::ConfigError::InvalidValue(format!("invalid timezone '{}': {}", self.timezone, e))
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PolicyRuleDef {
    pub action: String,
    pub condition: ConditionDef,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
#[non_exhaustive]
/// Policy condition definitions (ADR-43 §6): `ToolNamePrefix` matches any tool
/// whose name starts with the prefix (e.g. `mcp:github:`), enabling
/// server-level MCP policy rules.
pub enum ConditionDef {
    ToolOperation { tool_name: String, operation: String },
    ToolName { tool_name: String },
    ToolNamePrefix { tool_name_prefix: String },
    PathGlob { path_glob: String },
    CommandPattern { command_pattern: String },
    GitOperation { git_operation: String },
    Always { always: bool },
}

impl PolicyConfig {
    /// Convert config definitions to `concerto_core` `PolicyRule` enums.
    pub fn to_rules(&self) -> Vec<concerto_core::types::PolicyRule> {
        use concerto_core::types::{Condition, PolicyRule};

        self.rules
            .iter()
            .map(|def| {
                let condition = def.condition.to_condition();
                match def.action.as_str() {
                    "auto_approve" => PolicyRule::AutoApprove(condition),
                    "auto_deny" => PolicyRule::AutoDeny(condition),
                    "require_approval" => PolicyRule::RequireApproval(condition),
                    "require_managed_tool_approval" => {
                        PolicyRule::RequireManagedToolApproval(condition)
                    }
                    "require_toolchain_approval" => PolicyRule::RequireToolchainApproval(condition),
                    "deny_network_egress" => PolicyRule::DenyNetworkEgress(condition),
                    _ => PolicyRule::AutoDeny(Condition::Always),
                }
            })
            .collect()
    }

    /// Convert time window config to `concerto_core` `TimeWindowCondition`.
    pub fn to_time_window(&self) -> Option<concerto_core::policy::TimeWindowCondition> {
        self.time_window.as_ref().map(|tw| concerto_core::policy::TimeWindowCondition {
            start_hour: tw.start_hour,
            end_hour: tw.end_hour,
            timezone: tw.timezone.clone(),
            auto_approve_below_usd: tw.auto_approve_below_usd,
        })
    }
}

impl ConditionDef {
    fn to_condition(&self) -> concerto_core::types::Condition {
        use concerto_core::types::Condition;
        match self {
            ConditionDef::ToolName { tool_name } => Condition::ToolName(tool_name.clone()),
            ConditionDef::ToolNamePrefix { tool_name_prefix } => {
                Condition::ToolNamePrefix(tool_name_prefix.clone())
            }
            ConditionDef::ToolOperation { tool_name, operation } => Condition::All(vec![
                Condition::ToolName(tool_name.clone()),
                Condition::Operation(operation.clone()),
            ]),
            ConditionDef::PathGlob { path_glob } => Condition::PathGlob(path_glob.clone()),
            ConditionDef::CommandPattern { command_pattern } => {
                Condition::CommandPattern(command_pattern.clone())
            }
            ConditionDef::GitOperation { git_operation } => Condition::All(vec![
                Condition::ToolName("git".into()),
                Condition::Operation(git_operation.clone()),
            ]),
            ConditionDef::Always { always: _ } => Condition::Always,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProviderConfig {
    /// Unique identifier for referencing in agent assignments.
    /// Auto-generated as UUID if empty.
    #[serde(default)]
    pub id: String,
    /// User-friendly display name.
    #[serde(default)]
    pub name: String,
    /// Provider type: "openai" / "anthropic" / "google" / "openrouter" / "nim" / "ollama" / "opencode"
    pub provider: String,
    /// Legacy provider-local model field and model-route discovery hint.
    /// User-facing execution choices live on chat and agent assignments.
    #[serde(default)]
    pub model: String,
    /// Optional custom API base URL.
    pub api_base: Option<String>,
    pub timeout_seconds: u64,
    /// Keyring account name for API key storage.
    pub keyring_key: String,
    /// Models discovered via live model discovery, cached separately from the
    /// user's selected model. Additive; merged with static known models by the
    /// shared resolver. Not a user-intent field.
    #[serde(default)]
    pub cached_models: Vec<String>,
    /// Unix timestamp (seconds) of the last successful discovery, or 0.
    #[serde(default)]
    pub cached_models_fetched_at: i64,
    /// Extra models this provider config advertises for model resolution.
    ///
    /// Each name is a model the provider may serve in addition to `model` and
    /// any `cached_models` discovered at runtime. The catalog resolver
    /// (`ProviderFactory::config_for_model`) treats every entry as an offering
    /// candidate; they never shadow the primary `model`.
    /// Mostly useful for config-first setups that point one `api_base` at an
    /// OpenAI-compatible gateway serving several models.
    #[serde(default)]
    pub extra_models: Vec<String>,
    /// Reasoning-content echo policy for OpenAI-compatible providers (ADR-46).
    ///
    /// One of `"always"` (emit `reasoning_content` on every assistant message,
    /// empty string when no reasoning was captured) or `"if-present"` (emit
    /// only when the assistant message carries captured reasoning).
    /// `None` leaves the provider's built-in policy untouched (current
    /// behavior). Unsupported values are tolerated: a warning is logged and the
    /// value is treated as unset, keeping configs forward-compatible.
    #[serde(default)]
    pub reasoning_echo: Option<String>,
    /// Emit Anthropic prompt-cache breakpoints; default off.
    ///
    /// When enabled, the Anthropic connector annotates each request body with
    /// `cache_control` markers on the system prompt and the first user turn so
    /// the provider can reuse the byte-identical conversation prefix across
    /// consecutive turns (ADR-48 decision 3 — opt-in extension point). Only
    /// meaningful for `provider == "anthropic"`; other families treat it as a
    /// no-op. Off preserves the current wire output exactly.
    #[serde(default)]
    pub cache_breakpoints: bool,
}

impl ProviderConfig {
    /// Retrieve the API key from the credential store (keyring) only.
    ///
    /// Runtime resolution additionally falls back to the `<PROVIDER>_API_KEY`
    /// env var; use [`Self::effective_api_key`] when the key the runtime would
    /// actually use must be known (e.g. `concerto health`).
    pub fn api_key(&self, store: &CredentialStore) -> Result<String, ConfigError> {
        store.get(&self.keyring_key)
    }

    /// Resolve the key the runtime would use: keyring first, then the
    /// `<PROVIDER>_API_KEY` env var (provider uppercased) — the exact
    /// resolution `ProviderFactory::build` uses.
    ///
    /// When both are missing, the original keyring error from
    /// [`Self::api_key`] is returned, preserving the credential-missing
    /// semantics callers rely on.
    pub fn effective_api_key(&self, store: &CredentialStore) -> Result<String, ConfigError> {
        match self.api_key(store) {
            Ok(key) => Ok(key),
            Err(error) => {
                let env_key = format!("{}_API_KEY", self.provider.to_uppercase());
                match std::env::var(env_key) {
                    Ok(key) => Ok(key),
                    Err(_) => Err(error),
                }
            }
        }
    }

    /// Generate a stable id if empty, using the project convention
    /// (`prov_<ulid>`). Returns `true` if an id was generated.
    ///
    /// Used to repair legacy config entries that were created before provider
    /// ids were required.
    pub fn ensure_id(&mut self) -> bool {
        if self.id.trim().is_empty() {
            self.id = format!("prov_{}", concerto_core::ids::Ulid::new());
            true
        } else {
            false
        }
    }

    /// Record a successful model discovery result. Normalizes (trims, dedupes,
    /// sorts) the discovered list and stamps the fetch time. Additive state,
    /// separate from the user's selected `model`.
    pub fn record_discovered_models(&mut self, models: Vec<String>) {
        self.cached_models = normalize_model_list(models);
        self.cached_models_fetched_at = now_unix_secs();
    }

    /// Count of cached discovered models (for compact UI summary).
    pub fn cached_model_count(&self) -> usize {
        self.cached_models.len()
    }

    /// Human-readable age of the cached discovery for UI display.
    /// Returns `None` when no cache exists.
    pub fn cached_models_age(&self) -> Option<String> {
        if self.cached_models_fetched_at <= 0 {
            return None;
        }
        let now = now_unix_secs();
        let delta = now - self.cached_models_fetched_at;
        if delta < 0 {
            return Some("unknown".to_string());
        }
        Some(humanize_duration(delta as u64))
    }
}

/// Trim, de-duplicate (case-insensitive), and sort a model id list.
fn normalize_model_list(models: Vec<String>) -> Vec<String> {
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut out: Vec<String> = Vec::with_capacity(models.len());
    for m in models {
        let t = m.trim().to_string();
        if t.is_empty() {
            continue;
        }
        let key = t.to_lowercase();
        if seen.insert(key) {
            out.push(t);
        }
    }
    out.sort();
    out
}

fn now_unix_secs() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

fn humanize_duration(secs: u64) -> String {
    const MIN: u64 = 60;
    const HOUR: u64 = 60 * MIN;
    const DAY: u64 = 24 * HOUR;
    if secs < 30 {
        "just now".to_string()
    } else if secs < HOUR {
        format!("{} min ago", secs / MIN)
    } else if secs < DAY {
        format!("{} hr ago", secs / HOUR)
    } else {
        format!("{} d ago", secs / DAY)
    }
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            provider: String::new(),
            model: String::new(),
            api_base: None,
            timeout_seconds: 30,
            keyring_key: String::new(),
            cached_models: Vec::new(),
            cached_models_fetched_at: 0,
            extra_models: Vec::new(),
            reasoning_echo: None,
            cache_breakpoints: false,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ModelPinConfig {
    pub pins: HashMap<AgentId, String>,
    /// Fallback model used when an agent's pinned/routed model is exhausted
    /// (LimitReached class in the coordinator fallback ladder). `None`
    /// disables this tier of the ladder (preserves current behavior).
    ///
    /// Honest semantics: the fallback re-dispatches the same agent, and an
    /// agent is bound to one provider at construction time. `default_model`
    /// must therefore be a model name offered by the role's OWN bound
    /// provider; only the model string (and the routing/concurrency profile)
    /// changes, never the serving provider.
    pub default_model: Option<String>,
    /// Optional provider config id to pair with `default_model`. `None`
    /// accepts any provider config that offers the model name.
    ///
    /// This does NOT switch the provider serving the fallback request (the
    /// role's bound provider still serves it). It only disambiguates which
    /// routing profile matches when the same model name exists across several
    /// providers, and selects the profile's diagnostics/concurrency bucket.
    pub default_provider_config_id: Option<String>,
}

// ---- Multi-Provider Model Management (Phase 9) ------------------------------

/// Container for multi-provider model management settings.
///
/// Replaces the single `primary_provider_config` with a list of provider
/// configurations and per-agent model assignments.
/// Per-provider override of model-level compatibility and cost metadata.
///
/// These overrides are applied on top of the hardcoded defaults in
/// `ProviderFactory::build_profiles()` so users can tune model-specific
/// cost, latency, context, and tool metadata without changing code.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ModelProfileOverride {
    /// Override estimated blended cost per 1,000 tokens in USD.
    #[serde(default)]
    pub cost_per_1k_tokens: Option<f64>,
    /// Override expected average provider latency in milliseconds.
    #[serde(default)]
    pub avg_latency_ms: Option<u64>,
    /// Override the context window size in tokens.
    #[serde(default)]
    pub context_window: Option<u32>,
    /// Override tool-calling support.
    #[serde(default)]
    pub supports_tool_calling: Option<bool>,
    /// Override the API base URL.
    #[serde(default)]
    pub base_url: Option<String>,
    /// Override the human-readable description.
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ModelSettings {
    /// All configured providers.
    #[serde(default)]
    pub providers: Vec<ProviderConfig>,
    /// Per-agent model assignments.
    #[serde(default)]
    pub agent_assignments: Vec<AgentModelAssignment>,
    /// Model selected when a new chat session starts. Its provider route is
    /// resolved from configured providers rather than stored as a default.
    #[serde(default)]
    pub global_default_model: Option<String>,
    /// Deprecated: ID of the default provider. Superseded by
    /// `global_default_model` with model-first routing. Kept for desktop
    /// UI backward compatibility; the runtime no longer uses this field.
    #[serde(default)]
    pub global_default_id: Option<String>,
    /// Per-provider overrides for model-level compatibility and cost metadata.
    /// Keyed by `ProviderConfig.id`.
    #[serde(default)]
    pub model_profile_overrides: HashMap<String, ModelProfileOverride>,
}

impl ModelSettings {
    /// Ensure every provider has a stable, unique id.
    ///
    /// Repairs empty ids (via [`ProviderConfig::ensure_id`]) and de-duplicates
    /// collisions deterministically by suffixing `-N`. Returns `true` if any
    /// change was made, so callers can persist the repaired config.
    ///
    /// Idempotent: a config that already has unique non-empty ids is left
    /// unchanged, so repeated loads do not churn ids.
    pub fn repair_ids(&mut self) -> bool {
        let mut changed = false;
        for p in &mut self.providers {
            changed |= p.ensure_id();
        }

        let mut counts: HashMap<String, usize> = HashMap::new();
        for p in &mut self.providers {
            let n = counts.get(&p.id).copied().unwrap_or(0);
            if n == 0 {
                counts.insert(p.id.clone(), 1);
            } else {
                // Find a unique suffixed id.
                let mut candidate = format!("{}-{}", p.id, n);
                let mut k = n + 1;
                while counts.contains_key(&candidate) {
                    candidate = format!("{}-{}", p.id, k);
                    k += 1;
                }
                p.id = candidate.clone();
                counts.insert(candidate, 1);
                changed = true;
            }
        }
        changed
    }

    /// Resolve the coordinator fallback-ladder tier-1 target model (ADR-42/45).
    ///
    /// Tier 0 is the model selected for a run (session selection, global
    /// default); tier 1 is the model the ladder re-dispatches a failed role on
    /// after a `LimitReached` failure. The run's explicit
    /// `multi_agent.default_model` wins when set; otherwise
    /// `self.global_default_model` fills the tier-1 target so users who only
    /// configured a global default still get ladder fallback. `None` when
    /// neither is set — the tier is then disabled (the routing engine's
    /// `NoAffordableModel` keeps the "ladder skips tier 1" behavior).
    ///
    /// Shared by the orchestrator (`runtime_runner`) and the `concerto health`
    /// CLI so both agree on the resolved tier-1 pin. Whitespace-only values are
    /// treated as unset in both positions.
    pub fn resolved_default_model(&self, multi_agent: Option<&MultiAgentConfig>) -> Option<String> {
        let explicit = multi_agent
            .and_then(|config| config.default_model.as_deref())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned);
        if explicit.is_some() {
            return explicit;
        }
        self.global_default_model
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
    }
}

/// Maps an agent role to a specific provider config and optional model override.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AgentModelAssignment {
    /// The agent role this assignment applies to (e.g. "coordinator", "coder").
    pub agent_role: String,
    /// References `ProviderConfig.id` in the parent `ModelSettings.providers`.
    pub provider_config_id: String,
    /// Optional model override. When `None`, the provider's default model is used.
    #[serde(default)]
    pub model_override: Option<String>,
}

// ---- Phase 5: multi-agent configuration ------------------------------------

/// Multi-agent orchestration configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MultiAgentConfig {
    /// Whether multi-agent mode is the default for new sessions.
    #[serde(default)]
    pub default_enabled: bool,
    /// Spend cap multiplier relative to single-agent mode (default 3.0).
    #[serde(default = "default_spend_cap_multiplier")]
    pub spend_cap_multiplier: f64,
    /// Per-agent model overrides.
    #[serde(default)]
    pub model_pins: HashMap<AgentId, String>,
    /// Directed relationships that govern handoffs and revision limits.
    /// Empty uses the orchestrator's validated default topology.
    #[serde(default)]
    pub relationships: Vec<AgentRelationshipConfig>,
    /// User-defined agents (prompts, capabilities, model overrides) managed in
    /// the Orchestration Studio. The working agent roster is config-owned:
    /// once ANY entry exists here (or an `[orchestration]` section is present),
    /// the roster is these entries ONLY — the embedded `builtin_agent_seeds()`
    /// are not merged back in, and an id deleted from config stays deleted.
    #[serde(default)]
    pub custom_agents: Vec<CustomAgentConfig>,
    /// Saved pipeline presets (topologies + bundled agents) for one-click load.
    #[serde(default)]
    pub presets: Vec<PipelinePreset>,
    /// Maximum specialist runs executing concurrently across all providers.
    #[serde(default = "default_max_concurrent_agents")]
    pub max_concurrent_agents: usize,
    /// Maximum specialist runs executing concurrently against one provider.
    #[serde(default = "default_max_concurrent_per_provider")]
    pub max_concurrent_per_provider: usize,
    /// Global default model for the coordinator fallback ladder tier 1.
    /// When unset, the ladder skips to reassignment/self-execution.
    ///
    /// Model-first semantics: tier 1 re-dispatches the SAME role on this
    /// model, but only when the role's effective serving pipe offers it — the
    /// per-agent provider assignment, or the run's default provider for
    /// unassigned roles. When the model is registered on a different pipe,
    /// tier 1 is skipped cleanly and tier 1b rebuilds the role on the pipe
    /// that actually serves the model.
    #[serde(default)]
    pub default_model: Option<String>,
    /// Optional provider config id paired with `default_model`.
    ///
    /// Model-first semantics: this pin selects which routing profile matches
    /// `default_model` when the model name is shared across providers (used by
    /// the routing engine — it does not force a provider switch). Tier 1 still
    /// re-dispatches the SAME role on its own serving pipe and only runs when
    /// that pipe offers `default_model`; when the model lives on a different
    /// pipe — or no serving pipe resolves — tier 1 is skipped and tier 1b
    /// rebuilds the role on the run's default provider (the pipe that serves
    /// the model).
    #[serde(default)]
    pub default_provider_config_id: Option<String>,
    /// Whether the coordinator's fallback ladder may dispatch the global
    /// default model on the run's default provider (the pipe that serves it)
    /// when the role's bound provider is the failure (latency, quota,
    /// outage). Defaults to enabled. Tier 1b of the ladder (ADR-45): same
    /// agent role rebuilt on the run's default provider, served with the
    /// default model.
    #[serde(default = "default_true")]
    pub default_model_fallback: bool,
    /// Maximum dispatch attempts per subtask before the fallback ladder
    /// walks in. `None` uses the runtime default (3).
    #[serde(default)]
    pub max_subtask_attempts: Option<u32>,
    /// Global ceiling on total model-dispatch cycles for one multi-agent
    /// run (a doom guard). Every subtask dispatch — including retries,
    /// escalation, replans, review follow-up tasks, and every fallback-ladder
    /// tier re-dispatch — counts toward this limit; when the next ready batch
    /// would push the run past the cap, the coordinator pauses with a
    /// `Partial` outcome instead of spending more tokens. `None` (the
    /// default) means unlimited.
    #[serde(default)]
    pub max_total_iterations: Option<usize>,
    /// Supplementary prompt appended to the coordinator self's built-in
    /// instructions when the coordinator carries an unstaffed implement stage
    /// (ADR-35 §8). Additive only — it never replaces or overrides the
    /// built-in system instructions (ADR-35 §5). `None` (the default) keeps
    /// the coordinator self on its stock instructions.
    ///
    /// Written by the Orchestration Studio; shipped only through the Studio's
    /// settings surface, so this field is intentionally not a runtime
    /// config-file knob.
    #[serde(default)]
    pub coordinator_prompt: Option<String>,
    /// ADR-60 Phase 1 (thin slice): when `true`, an Execute-classified
    /// multi-agent run dispatches through the process supervisor
    /// ([`Supervisor`] + real `orchestrator-agent-process` children) instead
    /// of the in-process `CoordinatorAgent` waves. Defaults to `false` — the
    /// coordinator remains the production path until supervised parity lands.
    /// When enabled but the supervisor cannot start (no session-DB pool,
    /// missing child binary, empty roster), the run degrades loudly (a warn)
    /// back to the coordinator.
    ///
    /// [`Supervisor`]: concerto_orchestrator::supervisor::Supervisor
    #[serde(default)]
    pub supervisor_enabled: bool,
}

fn default_true() -> bool {
    true
}

fn default_max_concurrent_agents() -> usize {
    3
}

fn default_max_concurrent_per_provider() -> usize {
    2
}

impl Default for MultiAgentConfig {
    fn default() -> Self {
        Self {
            default_enabled: false,
            spend_cap_multiplier: default_spend_cap_multiplier(),
            model_pins: HashMap::new(),
            relationships: Vec::new(),
            custom_agents: Vec::new(),
            presets: Vec::new(),
            max_concurrent_agents: default_max_concurrent_agents(),
            max_concurrent_per_provider: default_max_concurrent_per_provider(),
            default_model: None,
            default_provider_config_id: None,
            default_model_fallback: default_true(),
            max_subtask_attempts: None,
            max_total_iterations: None,
            coordinator_prompt: None,
            supervisor_enabled: false,
        }
    }
}

impl MultiAgentConfig {
    /// ADR-35 §5 Coordinator contract: the coordinator is constructed in
    /// code only. Config entries targeting it are ignored by the registry;
    /// return a user-facing warning for each one so the load path can log it.
    pub fn pipeline_warnings(&self) -> Vec<String> {
        let mut warnings = Vec::new();

        for agent in &self.custom_agents {
            // `id` is the studio key; `role` is the legacy identity. Check
            // both and deduplicate when both match the same entry.
            let id_match = agent.id == "coordinator";
            let role_match = agent.role.eq_ignore_ascii_case("coordinator");
            // The Orchestration Studio persists the built-in coordinator row
            // as an `is_custom: false` mirror (it carries model/provider
            // pins for the Coordinator). Product-written mirrors are expected
            // and harmless — warn only about user-authored entries.
            let pure_mirror = !agent.is_custom && id_match && role_match;
            if (id_match || role_match) && !pure_mirror {
                warnings.push(
                    "custom agent 'coordinator' is ignored: the Coordinator is constructed in \
                     code only and cannot be replaced (ADR-35 §5)"
                        .to_string(),
                );
            }
        }

        for preset in &self.presets {
            for agent in &preset.agents {
                let id_match = agent.id == "coordinator";
                let role_match = agent.role.eq_ignore_ascii_case("coordinator");
                if id_match || role_match {
                    warnings.push(format!(
                        "preset '{}' defines agent 'coordinator' which is ignored: the \
                         Coordinator cannot be replaced (ADR-35 §5)",
                        preset.name
                    ));
                }
            }
        }

        warnings
    }
}

/// Serializable relationship definition kept in the config crate to avoid a
/// dependency from configuration back into the orchestrator.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentRelationshipConfig {
    pub from: String,
    pub to: String,
    pub relationship: String,
    #[serde(default)]
    pub max_cycles: Option<u32>,
}

// ---- Orchestration Studio: custom agents & presets --------------------------

/// Structured prompt sections for a (custom) agent.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct PromptSections {
    /// Core system instructions.
    #[serde(default)]
    pub system_instructions: String,
    /// Guardrails / constraints the agent must respect.
    #[serde(default)]
    pub constraints: String,
    /// Expected output format / shape.
    #[serde(default)]
    pub output_format: String,
    /// Few-shot exemplars shown to the model.
    #[serde(default)]
    pub few_shot: Vec<FewShotExample>,
}

/// A single input/output exemplar for few-shot prompting.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct FewShotExample {
    #[serde(default)]
    pub input: String,
    #[serde(default)]
    pub output: String,
}

/// Capability flags gating which tools an agent may use.
///
/// Each flag is an [`Option<bool>`] under the ADR-58 D1 authority model:
/// `None` (the default for the tool flags) means the flag is **unset** —
/// the blueprint resolver derives the effective value from the stage-kind
/// mask, and every consumer of the raw config resolves `None` to disabled
/// (see [`AgentCapabilities::effective`]). An explicit `Some(value)` is a
/// **narrowing override** over the stage-derived mask; widening beyond the
/// stage mask is a load error unless the agent is also staffed in an
/// `Execution`-kind stage.
///
/// The `eval` flag is not governed by any stage mask (ADR-58 D1: `fs_read`/
/// `git`/`lsp`/`eval` stay per-agent config). It defaults to *enabled* —
/// both for missing config keys and for structs built with `Default` — so
/// configs written before ADR-35 phase 4, and minimal hand-written entries
/// without a capabilities table, keep validation on instead of silently
/// disabling it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AgentCapabilities {
    #[serde(default)]
    pub fs_read: Option<bool>,
    #[serde(default)]
    pub fs_write: Option<bool>,
    #[serde(default)]
    pub shell: Option<bool>,
    #[serde(default)]
    pub git: Option<bool>,
    #[serde(default)]
    pub lsp: Option<bool>,
    /// Whether the agent may run the built-in eval/test engine (validator
    /// stage). Gated at runtime: without it, the validator runs without an
    /// eval engine.
    ///
    /// Defaults to *enabled* when the key is absent so configs written before
    /// ADR-35 phase 4 (which never serialized this field) keep validation on
    /// after upgrade instead of silently disabling it.
    #[serde(default = "default_eval_enabled")]
    pub eval: Option<bool>,
}

/// `AgentCapabilities.eval` defaults to enabled for config files that
/// predate the field (see the field docs).
fn default_eval_enabled() -> Option<bool> {
    Some(true)
}

impl Default for AgentCapabilities {
    /// Every tool flag unset (resolved from the stage mask or to disabled);
    /// the eval/test-engine flag **explicitly on** so the shape equals a
    /// config entry with an omitted capabilities table — the registry's
    /// `== AgentCapabilities::default()` check then inherits the seed caps
    /// exactly as it did before the `Option` conversion.
    fn default() -> Self {
        Self {
            fs_read: None,
            fs_write: None,
            shell: None,
            git: None,
            lsp: None,
            eval: default_eval_enabled(),
        }
    }
}

/// A capability flag fully resolved to a concrete boolean (ADR-58 D1).
///
/// Blueprint resolution replaces each `None` tool flag either with the
/// stage-kind default mask or with disabled; consumers of the *raw* config
/// (the pre-resolution runtime path) resolve `None` via
/// [`AgentCapabilities::effective`] until the P2+P3 rewrite wires them
/// through the resolved blueprint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedCapabilities {
    pub fs_read: bool,
    pub fs_write: bool,
    pub shell: bool,
    pub git: bool,
    pub lsp: bool,
    pub eval: bool,
}

impl AgentCapabilities {
    /// Resolve the optional flags to concrete booleans for the runtime path.
    ///
    /// An unset tool flag (`None`) resolves to disabled — matching the
    /// pre-ADR-58 all-false default — and an unset `eval` resolves to
    /// *enabled* (its upgrade-safe default). This is behavior-identical to
    /// the previous all-bool shape.
    pub fn effective(&self) -> ResolvedCapabilities {
        ResolvedCapabilities {
            fs_read: self.fs_read.unwrap_or(false),
            fs_write: self.fs_write.unwrap_or(false),
            shell: self.shell.unwrap_or(false),
            git: self.git.unwrap_or(false),
            lsp: self.lsp.unwrap_or(false),
            eval: self.eval.unwrap_or(true),
        }
    }
}

/// A user-defined agent persisted from the Orchestration Studio.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct CustomAgentConfig {
    /// Stable id used as the relationship/assignment key.
    pub id: String,
    pub name: String,
    pub role: String,
    /// Pipeline stage tag (design/research/implement/review/validate).
    /// `None` means Freeform semantics: the agent runs once when ready in
    /// the DAG with full context and no lifecycle behavior.
    #[serde(default)]
    pub stage: Option<AgentStage>,
    #[serde(default)]
    pub prompt_sections: PromptSections,
    #[serde(default)]
    pub model_override: Option<String>,
    #[serde(default)]
    pub provider_id: Option<String>,
    #[serde(default)]
    pub capabilities: AgentCapabilities,
    #[serde(default)]
    pub is_custom: bool,
    /// When true the agent is not registered at runtime. Lets a user remove
    /// a built-in specialist (e.g. reviewer) from the pipeline via config.
    #[serde(default)]
    pub disabled: bool,
    /// Structured output mode for the agent. `Freeform` (the default) keeps
    /// the historical free-text result semantics; `DesignDoc` routes the
    /// agent through the typed `submit_design_doc` submission contract
    /// (audit H-01). Pre-existing configs omit the field and therefore keep
    /// Freeform behavior.
    #[serde(default)]
    pub output_mode: OutputMode,
}

/// The five built-in specialist seed entries (ADR-35 phase 4 + audit A-01).
///
/// All five are now config-driven seeds backed by `GenericSpecialistAgent`;
/// their `system_instructions` here are extracted verbatim from the
/// dedicated agent structs they replace, and their `output_mode` selects the
/// matching typed submission contract. The validator additionally carries a
/// `Pass/Fail` output format so the eval-runner summary is prefixed
/// accordingly.
///
/// The config owns the roster once it declares one: any
/// `[multi_agent.custom_agents]` entry (or an `[orchestration]` section)
/// makes the working roster the config entries ONLY, so these seeds stand
/// in as the /embedded default/ exactly when neither is present. Outside
/// that, a user entry for a built-in id replaces that seed, and
/// `disabled = true` removes it from the runtime topology.
pub fn builtin_agent_seeds() -> Vec<CustomAgentConfig> {
    vec![
        CustomAgentConfig {
            id: "architect".into(),
            name: "Architect".into(),
            role: "architect".into(),
            stage: Some(AgentStage::new(AgentStage::DESIGN)),
            prompt_sections: PromptSections {
                system_instructions: "You are the Architect agent. Your role is to produce a high-level design document for the following task. Use the submit_design_doc tool to submit your design.".into(),
                ..Default::default()
            },
            capabilities: AgentCapabilities::default(),
            output_mode: OutputMode::DesignDoc,
            ..Default::default()
        },
        CustomAgentConfig {
            id: "researcher".into(),
            name: "Researcher".into(),
            role: "researcher".into(),
            stage: Some(AgentStage::new(AgentStage::RESEARCH)),
            prompt_sections: PromptSections {
                system_instructions: "You are the Researcher agent. Your role is to research and gather information for the following task. Use the submit_research_report tool to submit your findings.".into(),
                ..Default::default()
            },
            capabilities: AgentCapabilities::default(),
            output_mode: OutputMode::ResearchReport,
            ..Default::default()
        },
        CustomAgentConfig {
            id: "coder".into(),
            name: "Coder".into(),
            role: "coder".into(),
            stage: Some(AgentStage::new(AgentStage::IMPLEMENT)),
            prompt_sections: PromptSections {
                system_instructions: "You are the Coder agent. Your role is to implement code for the following task:".into(),
                ..Default::default()
            },
            capabilities: AgentCapabilities::default(),
            output_mode: OutputMode::Freeform,
            ..Default::default()
        },
        CustomAgentConfig {
            id: "reviewer".into(),
            name: "Reviewer".into(),
            role: "reviewer".into(),
            stage: Some(AgentStage::new(AgentStage::REVIEW)),
            prompt_sections: PromptSections {
                system_instructions: "You are the Reviewer agent. Your role is to review code and produce a review report for the following task. Use the submit_review_report tool to submit your review.".into(),
                ..Default::default()
            },
            capabilities: AgentCapabilities::default(),
            output_mode: OutputMode::ReviewReport,
            ..Default::default()
        },
        CustomAgentConfig {
            id: "validator".into(),
            name: "Validator".into(),
            role: "validator".into(),
            stage: Some(AgentStage::new(AgentStage::VALIDATE)),
            prompt_sections: PromptSections {
                // Audit A-01: the eval-runner's summary is prefixed with
                // "Pass: "/"Fail: " only when output_format mentions
                // Pass/Fail (see GenericSpecialistAgent::format_summary).
                output_format: "Pass/Fail report".into(),
                ..Default::default()
            },
            capabilities: AgentCapabilities::default(),
            output_mode: OutputMode::Freeform,
            ..Default::default()
        },
    ]
}

/// A saved pipeline topology with optional bundled custom agents.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct PipelinePreset {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub agents: Vec<CustomAgentConfig>,
    #[serde(default)]
    pub relationships: Vec<AgentRelationshipConfig>,
    /// Built-in presets seeded by the app are never persisted back to config;
    /// the flag lets the UI filter them out of the save path.
    #[serde(default, skip_serializing_if = "is_false")]
    pub is_builtin: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn default_spend_cap_multiplier() -> f64 {
    3.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_config_defaults_are_sane() {
        let rc = RetryConfig::default();
        assert!(rc.enabled);
        assert_eq!(rc.initial_delay_ms, 2_000);
        assert_eq!(rc.max_delay_ms, 30_000);
        assert_eq!(rc.multiplier, 2.0);
        assert!(rc.jitter);
        assert_eq!(rc.fixed_delay_ms, None);
        assert!(rc.respect_retry_after);
        assert_eq!(rc.max_attempts, 8);
        assert_eq!(rc.max_elapsed_seconds, Some(15 * 60));
        assert_eq!(rc.time_to_first_byte_seconds, 120);
        assert_eq!(rc.stream_idle_timeout_seconds, 300);
    }

    /// ADR-56 §2: `classifier_enabled` defaults to true — the LLM classifier
    /// is the primary intent decider and a config without an explicit key (or
    /// migrated from v6) enables it. `classifier_model` and the threshold
    /// keep their unchanged defaults.
    #[test]
    fn intent_classifier_defaults_to_enabled() {
        let intent = IntentConfig::default();
        assert!(intent.classifier_enabled, "classifier must default ON (ADR-56)");
        assert_eq!(intent.classifier_model, None, "classifier_model still defaults to None");
        assert_eq!(
            intent.classifier_confidence_threshold,
            concerto_core::LOW_CONFIDENCE_THRESHOLD,
            "threshold still defaults to LOW_CONFIDENCE_THRESHOLD (0.7)"
        );
        // The serde default (what an omitted `classifier_enabled` key loads)
        // must agree with the `Default` impl — the flip applies to both paths.
        let from_toml =
            crate::schema::IntentConfig::deserialize(toml::Value::Table(toml::map::Map::new()))
                .expect("omitted keys fall back to their serde defaults");
        assert!(from_toml.classifier_enabled, "serde default must also be ON");
    }

    /// `[tools] git_auto_init` defaults to true on both construction paths:
    /// the `Default` impl (what the session manager uses) and the serde
    /// default (what an omitted `git_auto_init` key loads).
    #[test]
    fn tool_settings_git_auto_init_defaults_to_enabled() {
        let settings = ToolSettings::default();
        assert!(settings.git_auto_init, "git_auto_init must default to true");

        let from_toml =
            crate::schema::ToolSettings::deserialize(toml::Value::Table(toml::map::Map::new()))
                .expect("omitted keys fall back to their serde defaults");
        assert!(from_toml.git_auto_init, "serde default must also be true");
    }

    #[test]
    fn tool_operation_config_is_scoped_to_its_tool() {
        let definition = ConditionDef::ToolOperation {
            tool_name: "filesystem".into(),
            operation: "write".into(),
        };
        let condition = definition.to_condition();

        assert_eq!(
            condition,
            concerto_core::types::Condition::All(vec![
                concerto_core::types::Condition::ToolName("filesystem".into()),
                concerto_core::types::Condition::Operation("write".into()),
            ])
        );

        let encoded = toml::to_string(&definition).unwrap();
        let decoded: ConditionDef = toml::from_str(&encoded).unwrap();
        assert_eq!(decoded, definition);
    }

    #[test]
    fn retry_config_validate_accepts_defaults() {
        assert!(RetryConfig::default().validate().is_ok());
    }

    #[test]
    fn retry_config_validate_rejects_zero_initial_delay() {
        let rc =
            RetryConfig { initial_delay_ms: 0, fixed_delay_ms: None, ..RetryConfig::default() };
        assert!(rc.validate().is_err());
    }

    #[test]
    fn retry_config_validate_rejects_zero_fixed_delay() {
        let rc = RetryConfig { fixed_delay_ms: Some(0), ..RetryConfig::default() };
        assert!(rc.validate().is_err());
    }

    #[test]
    fn retry_config_validate_rejects_bad_multiplier() {
        let rc = RetryConfig { multiplier: 0.5, ..RetryConfig::default() };
        assert!(rc.validate().is_err());
    }

    #[test]
    fn custom_agent_config_stage_defaults_none_and_normalizes_case() {
        // Older configs without a stage tag must still load (None = Freeform).
        let legacy = toml::from_str::<CustomAgentConfig>(
            "id = \"docs-writer\"\nname = \"Docs Writer\"\nrole = \"docs-writer\"\n",
        )
        .unwrap();
        assert_eq!(legacy.stage, None);

        // Mixed-case stage tags are normalized to the canonical lowercase.
        let with_stage = toml::from_str::<CustomAgentConfig>(
            "id = \"docs-writer\"\nname = \"Docs Writer\"\nrole = \"docs-writer\"\nstage = \"Review\"\n",
        )
        .unwrap();
        assert_eq!(with_stage.stage, Some(AgentStage::new("review")));
        assert!(with_stage.stage.as_ref().unwrap().is_review());

        // Round-trips through serialization unchanged.
        let encoded = toml::to_string(&with_stage).unwrap();
        let decoded: CustomAgentConfig = toml::from_str(&encoded).unwrap();
        assert_eq!(decoded, with_stage);
    }

    #[test]
    fn capability_and_disabled_fields_round_trip_with_upgrade_safe_defaults() {
        // A bare config without the new flags must still load. `disabled`
        // defaults to false; `eval` defaults to *enabled* so configs written
        // before the field existed keep validation on after upgrade.
        let bare = toml::from_str::<CustomAgentConfig>(
            "id = \"bare\"\nname = \"Bare\"\nrole = \"bare\"\n",
        )
        .unwrap();
        assert!(!bare.disabled);
        assert!(
            bare.capabilities.eval.is_some_and(|eval| eval),
            "missing capabilities must keep eval enabled"
        );

        // The actual pre-phase-4 upgrade shape: a capabilities table without
        // the `eval` key (as every Orchestration Studio save wrote). The
        // missing key must deserialize as enabled.
        let legacy_validator = toml::from_str::<CustomAgentConfig>(
            "id = \"validator\"\nname = \"Validator\"\nrole = \"validator\"\n\
             capabilities = { fs_read = true, shell = true }\n",
        )
        .unwrap();
        assert!(
            legacy_validator.capabilities.eval.is_some_and(|eval| eval),
            "pre-phase-4 validator entries must keep validation enabled"
        );

        // Explicit values survive a TOML round trip unchanged.
        let agent = CustomAgentConfig {
            id: "specialist".into(),
            name: "Specialist".into(),
            role: "specialist".into(),
            disabled: true,
            capabilities: AgentCapabilities { eval: Some(false), ..Default::default() },
            ..Default::default()
        };
        let encoded = toml::to_string(&agent).unwrap();
        let decoded: CustomAgentConfig = toml::from_str(&encoded).unwrap();
        assert!(decoded.disabled);
        assert_eq!(decoded.capabilities.eval, Some(false));
        assert_eq!(decoded, agent);
    }

    #[test]
    fn output_mode_defaults_to_freeform_and_round_trips() {
        // A config written before the field existed must keep Freeform
        // semantics (identical behavior to today).
        let legacy = toml::from_str::<CustomAgentConfig>(
            "id = \"docs-writer\"\nname = \"Docs Writer\"\nrole = \"docs-writer\"\n",
        )
        .unwrap();
        assert_eq!(legacy.output_mode, OutputMode::Freeform);

        // Explicit DesignDoc mode survives a round trip unchanged.
        let agent = CustomAgentConfig {
            id: "architect-v2".into(),
            name: "Architect v2".into(),
            role: "architect-v2".into(),
            output_mode: OutputMode::DesignDoc,
            ..Default::default()
        };
        let encoded = toml::to_string(&agent).unwrap();
        assert!(encoded.contains("output_mode = \"design_doc\""), "unexpected encoding: {encoded}");
        let decoded: CustomAgentConfig = toml::from_str(&encoded).unwrap();
        assert_eq!(decoded, agent);
    }

    #[test]
    fn pipeline_warnings_flags_coordinator_entries() {
        let agent = |id: &str, role: &str| CustomAgentConfig {
            id: id.into(),
            name: id.into(),
            role: role.into(),
            ..Default::default()
        };

        let cfg = MultiAgentConfig {
            custom_agents: vec![
                agent("coordinator", "planner"), // matches by studio id
                agent("planner", "Coordinator"), // matches by legacy role (mixed case)
                agent("coder", "coder"),         // normal agent, no warning
                CustomAgentConfig {
                    id: "coordinator".into(),
                    name: "Coordinator".into(),
                    role: "coordinator".into(),
                    // Product-written mirror (studio persists the built-in
                    // coordinator row): expected, not warned about.
                    ..Default::default()
                },
            ],
            presets: vec![PipelinePreset {
                name: "strict".into(),
                agents: vec![agent("validator", "coordinator")],
                ..Default::default()
            }],
            ..Default::default()
        };

        let warnings = cfg.pipeline_warnings();
        assert_eq!(warnings.len(), 3, "two custom-agent + one preset warning");
        for w in &warnings {
            assert!(w.contains("coordinator"), "unexpected warning: {w}");
        }
        // Two entries matched in `custom_agents`.
        assert_eq!(warnings.iter().filter(|w| w.contains("custom agent 'coordinator'")).count(), 2);
        // The preset warning carries the preset name.
        assert_eq!(warnings.iter().filter(|w| w.contains("preset 'strict'")).count(), 1);
    }

    #[test]
    fn pipeline_warnings_empty_when_clean() {
        let agent = |id: &str| CustomAgentConfig {
            id: id.into(),
            name: id.into(),
            role: id.into(),
            ..Default::default()
        };

        let cfg = MultiAgentConfig {
            custom_agents: vec![agent("coder"), agent("reviewer")],
            presets: vec![PipelinePreset {
                name: "standard".into(),
                agents: vec![agent("docs-writer")],
                ..Default::default()
            }],
            ..Default::default()
        };

        assert!(cfg.pipeline_warnings().is_empty());
    }

    #[test]
    fn multi_agent_ladder_knobs_default_and_round_trip() {
        // ADR-45 §4: `default_model_fallback` defaults to ENABLED (gates the
        // default-model fallback ladder tier) and `max_subtask_attempts`
        // defaults to None (runtime default cap); older config files without
        // the keys still load.
        let cfg = MultiAgentConfig::default();
        assert!(cfg.default_model_fallback, "default-model fallback must default to enabled");
        assert_eq!(cfg.max_subtask_attempts, None, "attempt cap must default to None");

        let json = serde_json::to_string(&MultiAgentConfig {
            default_model_fallback: false,
            max_subtask_attempts: Some(7),
            ..Default::default()
        })
        .expect("serialize");
        let restored: MultiAgentConfig = serde_json::from_str(&json).expect("deserialize");
        assert!(!restored.default_model_fallback);
        assert_eq!(restored.max_subtask_attempts, Some(7));
    }

    #[test]
    fn multi_agent_global_cap_defaults_to_unlimited_and_round_trips() {
        // The run-wide dispatch cap is additive config: absent in older
        // files, defaults to None (unlimited), and survives a JSON round trip.
        let cfg = MultiAgentConfig::default();
        assert_eq!(
            cfg.max_total_iterations, None,
            "the global run cap must default to unlimited (None)"
        );

        let json = serde_json::to_string(&MultiAgentConfig {
            max_total_iterations: Some(2),
            ..Default::default()
        })
        .expect("serialize");
        let restored: MultiAgentConfig = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(restored.max_total_iterations, Some(2));

        // A config that never carried the key at all still parses.
        let legacy = r#"{"spend_cap_multiplier":3.0,"default_enabled":false}"#;
        let parsed: MultiAgentConfig = serde_json::from_str(legacy).expect("deserialize");
        assert_eq!(parsed.max_total_iterations, None);
    }

    #[test]
    fn multi_agent_coordinator_prompt_defaults_none_and_round_trips() {
        // ADR-35 §8: the Studio's supplemental coordinator prompt is additive
        // config — absent by default (stock coordinator self instructions),
        // preserved across a JSON round trip when set, and safely absent in
        // configs that never carried the key.
        let cfg = MultiAgentConfig::default();
        assert_eq!(cfg.coordinator_prompt, None, "the supplemental prompt must default to None");

        let json = serde_json::to_string(&MultiAgentConfig {
            coordinator_prompt: Some("Follow the studio playbook.".into()),
            ..Default::default()
        })
        .expect("serialize");
        let restored: MultiAgentConfig = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(
            restored.coordinator_prompt.as_deref(),
            Some("Follow the studio playbook."),
            "a set supplemental prompt must survive the round trip"
        );

        let legacy = r#"{"spend_cap_multiplier":3.0,"default_enabled":false}"#;
        let parsed: MultiAgentConfig = serde_json::from_str(legacy).expect("deserialize");
        assert_eq!(parsed.coordinator_prompt, None);
    }

    #[test]
    fn multi_agent_supervisor_enabled_defaults_off_and_round_trips() {
        // ADR-60 Phase 1: the supervised multi-agent path is opt-in. The flag
        // defaults to false (the coordinator stays the production path), and
        // configs that never carried the key keep loading as coordinator runs.
        let cfg = MultiAgentConfig::default();
        assert!(!cfg.supervisor_enabled, "the supervisor path must default to off");

        let json = serde_json::to_string(&MultiAgentConfig {
            supervisor_enabled: true,
            ..Default::default()
        })
        .expect("serialize");
        let restored: MultiAgentConfig = serde_json::from_str(&json).expect("deserialize");
        assert!(restored.supervisor_enabled, "an explicit opt-in must survive the round trip");

        let legacy = r#"{"spend_cap_multiplier":3.0,"default_enabled":false}"#;
        let parsed: MultiAgentConfig = serde_json::from_str(legacy).expect("deserialize");
        assert!(
            !parsed.supervisor_enabled,
            "legacy config without the key stays on the coordinator"
        );
    }

    #[test]
    fn app_config_carries_retry_field_with_defaults() {
        // Older config files without a [retry] section must still load, and the
        // field must fall back to RetryConfig::default().
        let cfg = AppConfig::default();
        assert_eq!(cfg.retry, RetryConfig::default());
    }

    #[test]
    fn ensure_id_repairs_empty_and_is_idempotent() {
        let mut pc = ProviderConfig {
            id: String::new(),
            name: "X".into(),
            provider: "openai".into(),
            model: "gpt-4o".into(),
            api_base: None,
            timeout_seconds: 30,
            keyring_key: "openai/api_key".into(),
            cached_models: Vec::new(),
            cached_models_fetched_at: 0,
            ..ProviderConfig::default()
        };
        assert!(pc.ensure_id());
        assert!(pc.id.starts_with("prov_"));
        // Second call is a no-op.
        assert!(!pc.ensure_id());
        assert!(pc.id.starts_with("prov_"));
    }

    #[test]
    fn repair_ids_fills_empty_and_deduplicates() {
        // Two providers with empty ids and one duplicate id.
        let mut ms = ModelSettings {
            providers: vec![
                ProviderConfig {
                    id: String::new(),
                    name: "A".into(),
                    provider: "openai".into(),
                    model: "gpt-4o".into(),
                    api_base: None,
                    timeout_seconds: 30,
                    keyring_key: "openai/api_key".into(),
                    cached_models: Vec::new(),
                    cached_models_fetched_at: 0,
                    ..ProviderConfig::default()
                },
                ProviderConfig {
                    id: String::new(),
                    name: "B".into(),
                    provider: "anthropic".into(),
                    model: "claude-3-5-sonnet-latest".into(),
                    api_base: None,
                    timeout_seconds: 30,
                    keyring_key: "anthropic/api_key".into(),
                    cached_models: Vec::new(),
                    cached_models_fetched_at: 0,
                    ..ProviderConfig::default()
                },
                ProviderConfig {
                    id: "dup".into(),
                    name: "C".into(),
                    provider: "ollama".into(),
                    model: "llama3".into(),
                    api_base: Some("http://localhost:11434".into()),
                    timeout_seconds: 30,
                    keyring_key: "ollama/api_key".into(),
                    cached_models: Vec::new(),
                    cached_models_fetched_at: 0,
                    ..ProviderConfig::default()
                },
                ProviderConfig {
                    id: "dup".into(),
                    name: "D".into(),
                    provider: "google".into(),
                    model: "gemini-1.5-pro".into(),
                    api_base: None,
                    timeout_seconds: 30,
                    keyring_key: "google/api_key".into(),
                    cached_models: Vec::new(),
                    cached_models_fetched_at: 0,
                    ..ProviderConfig::default()
                },
            ],
            ..Default::default()
        };

        assert!(ms.repair_ids());

        // All ids are now non-empty and unique.
        let ids: Vec<String> = ms.providers.iter().map(|p| p.id.clone()).collect();
        assert_eq!(ids.len(), 4);
        let unique: std::collections::HashSet<String> = ids.iter().cloned().collect();
        assert_eq!(unique.len(), 4, "ids must be unique after repair");
        assert!(ids.iter().all(|id| !id.is_empty()));

        // Idempotent: a second call makes no change.
        assert!(!ms.repair_ids());
    }

    #[test]
    fn repair_ids_keeps_existing_unique_ids() {
        let mut ms = ModelSettings {
            providers: vec![ProviderConfig {
                id: "keep-me".into(),
                name: "A".into(),
                provider: "openai".into(),
                model: "gpt-4o".into(),
                api_base: None,
                timeout_seconds: 30,
                keyring_key: "openai/api_key".into(),
                cached_models: Vec::new(),
                cached_models_fetched_at: 0,
                ..ProviderConfig::default()
            }],
            ..Default::default()
        };
        assert!(!ms.repair_ids());
        assert_eq!(ms.providers[0].id, "keep-me");
    }

    #[test]
    fn record_discovered_models_normalizes_and_stamps() {
        let mut pc = ProviderConfig {
            id: "prov_1".into(),
            name: "OpenAI".into(),
            provider: "openai".into(),
            model: "gpt-4o".into(),
            api_base: None,
            timeout_seconds: 30,
            keyring_key: "openai/api_key".into(),
            cached_models: Vec::new(),
            cached_models_fetched_at: 0,
            ..ProviderConfig::default()
        };
        // Duplicate + different casing + whitespace should be trimmed/deduped and sorted.
        pc.record_discovered_models(vec![
            " gpt-4o ".into(),
            "GPT-4O".into(),
            "gpt-4o-mini".into(),
            "".into(),
            "gpt-4o".into(),
        ]);
        assert_eq!(pc.cached_models, vec!["gpt-4o", "gpt-4o-mini"]);
        assert!(pc.cached_models_fetched_at > 0);
        assert_eq!(pc.cached_model_count(), 2);
        assert!(pc.cached_models_age().is_some());
    }

    #[test]
    fn provider_config_extra_models_and_reasoning_echo_round_trip() {
        // New fields are additive (`serde(default)`): a provider block written
        // before they existed must still load with the new fields at their
        // defaults (backward compatibility).
        let legacy = toml::from_str::<ProviderConfig>(
            "id = \"outdated\"\nprovider = \"openai\"\nmodel = \"gpt-4o\"\n\
             timeout_seconds = 30\nkeyring_key = \"openai/api_key\"\n",
        )
        .unwrap();
        assert!(legacy.extra_models.is_empty(), "legacy provider must default to no extra models");
        assert_eq!(legacy.reasoning_echo, None, "legacy provider must default to unset echo");

        // Explicit values survive a round trip unchanged. `reasoning_echo` is
        // stored as a raw string; the factory leniently parses it at build time.
        let pc = ProviderConfig {
            id: "gateway".into(),
            provider: "openai".into(),
            model: "primary".into(),
            extra_models: vec!["extra-a".into(), "extra-b".into()],
            reasoning_echo: Some("always".into()),
            ..ProviderConfig::default()
        };
        let encoded = toml::to_string(&pc).unwrap();
        assert!(encoded.contains("extra_models"), "unexpected encoding: {encoded}");
        assert!(encoded.contains("reasoning_echo"), "unexpected encoding: {encoded}");
        let decoded: ProviderConfig = toml::from_str(&encoded).unwrap();
        assert_eq!(decoded.extra_models, vec!["extra-a", "extra-b"]);
        assert_eq!(decoded.reasoning_echo.as_deref(), Some("always"));

        // An unknown `reasoning_echo` value is preserved as raw config data so
        // the factory can warn and fall back — never a hard parse failure.
        let lenient: ProviderConfig =
            toml::from_str(&encoded.replace("\"always\"", "\"sometimes\"")).unwrap();
        assert_eq!(lenient.reasoning_echo.as_deref(), Some("sometimes"));
    }

    #[test]
    fn provider_config_cache_breakpoints_round_trip() {
        // Legacy provider block without the field must load with breakpoints
        // off (additive `serde(default)`, backward compatible).
        let legacy = toml::from_str::<ProviderConfig>(
            "id = \"outdated\"\nprovider = \"anthropic\"\nmodel = \"claude-sonnet-4\"\n\
             timeout_seconds = 30\nkeyring_key = \"anthropic/api_key\"\n",
        )
        .unwrap();
        assert!(!legacy.cache_breakpoints, "legacy provider must default cache breakpoints off");

        // Explicit true and false survive a round trip unchanged.
        for (enabled, rendered) in
            [(true, "cache_breakpoints = true"), (false, "cache_breakpoints = false")]
        {
            let pc = ProviderConfig { cache_breakpoints: enabled, ..ProviderConfig::default() };
            let encoded = toml::to_string(&pc).unwrap();
            assert!(encoded.contains(rendered), "unexpected encoding: {encoded}");
            let decoded: ProviderConfig = toml::from_str(&encoded).unwrap();
            assert_eq!(decoded.cache_breakpoints, enabled);
        }

        // Parsing an explicitly-true provider block sets the flag.
        let explicit = toml::from_str::<ProviderConfig>(
            "id = \"anth\"\nprovider = \"anthropic\"\nmodel = \"claude-sonnet-4\"\n\
             timeout_seconds = 30\nkeyring_key = \"anthropic/api_key\"\n\
             cache_breakpoints = true\n",
        )
        .unwrap();
        assert!(explicit.cache_breakpoints);
    }

    // ------------------------------------------------------------------
    // humanize_duration
    // ------------------------------------------------------------------

    #[test]
    fn humanize_duration_seconds_returns_just_now() {
        assert_eq!(humanize_duration(0), "just now");
        assert_eq!(humanize_duration(29), "just now");
    }

    #[test]
    fn humanize_duration_minutes_returns_min_ago() {
        assert_eq!(humanize_duration(30), "0 min ago");
        assert_eq!(humanize_duration(60), "1 min ago");
        assert_eq!(humanize_duration(3599), "59 min ago");
    }

    #[test]
    fn humanize_duration_hours_returns_hr_ago() {
        assert_eq!(humanize_duration(3600), "1 hr ago");
        assert_eq!(humanize_duration(7200), "2 hr ago");
        assert_eq!(humanize_duration(86399), "23 hr ago");
    }

    #[test]
    fn humanize_duration_days_returns_d_ago() {
        assert_eq!(humanize_duration(86400), "1 d ago");
        assert_eq!(humanize_duration(172800), "2 d ago");
    }

    // ------------------------------------------------------------------
    // normalize_model_list
    // ------------------------------------------------------------------

    #[test]
    fn normalize_model_list_handles_empty_and_whitespace() {
        let result = normalize_model_list(vec![]);
        assert!(result.is_empty());

        let result = normalize_model_list(vec!["   ".into(), "".into()]);
        assert!(result.is_empty());
    }

    // ------------------------------------------------------------------
    // MemoryConfig validation
    // ------------------------------------------------------------------

    #[test]
    fn memory_config_validate_accepts_ttl_of_one_and_max() {
        let cfg = MemoryConfig { ttl_days: 1, ..Default::default() };
        assert!(cfg.validate().is_ok());

        let cfg = MemoryConfig { ttl_days: 365, ..Default::default() };
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn memory_config_validate_rejects_ttl_out_of_range() {
        let cfg = MemoryConfig { ttl_days: 0, ..Default::default() };
        assert!(cfg.validate().is_err());

        let cfg = MemoryConfig { ttl_days: 366, ..Default::default() };
        assert!(cfg.validate().is_err());
    }

    // ------------------------------------------------------------------
    // SkillsConfig / McpConfig defaults and validation
    // ------------------------------------------------------------------

    #[test]
    fn skills_config_defaults() {
        let skills = SkillsConfig::default();
        assert!(!skills.enabled, "skills default off per ADR-43 decision 5");
        assert_eq!(
            skills.search_paths,
            vec!["~/.local/share/concerto/skills".to_string(), "./.concerto/skills".to_string()]
        );
        assert!(skills.auto_load);
        assert_eq!(skills.enabled_ids, None);
        assert_eq!(skills.max_chars, None, "max_chars defaults to None (orchestrator budget)");
    }

    #[test]
    fn skills_config_serde_defaults_when_fields_missing() {
        let parsed: SkillsConfig =
            toml::from_str("search_paths = [\"./custom-skills\"]").expect("must parse");
        assert!(!parsed.enabled, "missing enabled must default to false (ADR-43 decision 5)");
        assert!(parsed.auto_load, "missing auto_load must default to true");
        assert_eq!(parsed.search_paths, vec!["./custom-skills".to_string()]);
        assert_eq!(parsed.enabled_ids, None);
        assert_eq!(parsed.max_chars, None, "absent max_chars must deserialize to None");
    }

    #[test]
    fn skills_config_parses_max_chars() {
        let parsed: SkillsConfig =
            toml::from_str("enabled = true\nmax_chars = 2048").expect("must parse");
        assert!(parsed.enabled);
        assert_eq!(parsed.max_chars, Some(2048));
    }

    #[test]
    fn mcp_config_defaults() {
        let mcp = McpConfig::default();
        assert!(!mcp.enabled, "mcp defaults to disabled (ADR-43 §6)");
        assert!(mcp.servers.is_empty());
    }

    #[test]
    fn mcp_config_validate_accepts_valid_server_ids() {
        let cfg = McpConfig {
            enabled: true,
            servers: vec![McpServerConfig {
                id: "filesystem".into(),
                command: "npx".into(),
                args: Vec::new(),
                env: None,
                enabled: true,
                timeout_secs: None,
            }],
        };
        assert!(cfg.validate().is_ok());
        assert!(McpConfig::default().validate().is_ok());
    }

    #[test]
    fn mcp_config_validate_rejects_colon_in_server_id() {
        let cfg = McpConfig {
            enabled: true,
            servers: vec![McpServerConfig {
                id: "bad:id".into(),
                command: "npx".into(),
                args: Vec::new(),
                env: None,
                enabled: true,
                timeout_secs: None,
            }],
        };
        let err = cfg.validate().unwrap_err();
        assert!(
            format!("{err}").contains("must not contain ':'"),
            "expected colon rejection, got: {err}"
        );
    }

    #[test]
    fn mcp_config_validate_rejects_empty_server_id() {
        let cfg = McpConfig {
            enabled: true,
            servers: vec![McpServerConfig {
                id: String::new(),
                command: "npx".into(),
                args: Vec::new(),
                env: None,
                enabled: true,
                timeout_secs: None,
            }],
        };
        let err = cfg.validate().unwrap_err();
        assert!(
            format!("{err}").contains("must be non-empty"),
            "expected empty-id rejection, got: {err}"
        );
    }

    #[test]
    fn mcp_config_validate_rejects_duplicate_server_ids() {
        let server = McpServerConfig {
            id: "github".into(),
            command: "npx".into(),
            args: Vec::new(),
            env: None,
            enabled: true,
            timeout_secs: None,
        };
        let cfg = McpConfig { enabled: true, servers: vec![server.clone(), server] };
        let err = cfg.validate().unwrap_err();
        let text = format!("{err}");
        assert!(
            text.contains("duplicated") && text.contains("github"),
            "expected duplicate-id rejection naming both ids, got: {text}"
        );
    }

    #[test]
    fn mcp_config_validate_accepts_distinct_server_ids() {
        let cfg = McpConfig {
            enabled: true,
            servers: vec![
                McpServerConfig {
                    id: "github".into(),
                    command: "npx".into(),
                    args: Vec::new(),
                    env: None,
                    enabled: true,
                    timeout_secs: None,
                },
                McpServerConfig {
                    id: "filesystem".into(),
                    command: "npx".into(),
                    args: Vec::new(),
                    env: None,
                    enabled: true,
                    timeout_secs: None,
                },
            ],
        };
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn tool_name_prefix_condition_parses_and_maps() {
        let definition = ConditionDef::ToolNamePrefix { tool_name_prefix: "mcp:github:".into() };
        assert_eq!(
            definition.to_condition(),
            concerto_core::types::Condition::ToolNamePrefix("mcp:github:".into())
        );

        // Round trip through TOML (the untagged mirror must stay parseable).
        let encoded = toml::to_string(&definition).unwrap();
        let decoded: ConditionDef = toml::from_str(&encoded).unwrap();
        assert_eq!(decoded, definition);

        // And a full policy rule parses from TOML text end to end.
        let rule_text = r#"
            schema_version = 5
            [[policy.rules]]
            action = "auto_deny"
            condition = { tool_name_prefix = "mcp:" }
        "#;
        let config: AppConfig = toml::from_str(rule_text).unwrap();
        let rules = config.policy.as_ref().expect("policy section parsed").to_rules();
        assert_eq!(
            rules,
            vec![concerto_core::types::PolicyRule::AutoDeny(
                concerto_core::types::Condition::ToolNamePrefix("mcp:".into())
            )]
        );
    }

    // ------------------------------------------------------------------
    // ModelSettings::resolved_default_model (ADR-42/45 tier-1 pin source)
    // ------------------------------------------------------------------

    fn multi_agent_with_default(model: Option<&str>) -> MultiAgentConfig {
        MultiAgentConfig { default_model: model.map(ToOwned::to_owned), ..Default::default() }
    }

    #[test]
    fn resolved_default_model_multi_agent_default_wins() {
        // An explicit multi-agent default wins over the global default.
        let settings = ModelSettings {
            global_default_model: Some("global-default".into()),
            ..Default::default()
        };
        let multi_agent = multi_agent_with_default(Some("agent-default"));
        assert_eq!(
            settings.resolved_default_model(Some(&multi_agent)).as_deref(),
            Some("agent-default"),
            "multi_agent.default_model must win over global_default_model",
        );
    }

    #[test]
    fn resolved_default_model_falls_back_to_global_when_no_multi_agent() {
        // No multi-agent default: the global default fills the tier-1 target so
        // users who only set `model_settings.global_default_model` still get
        // ladder fallback.
        let settings = ModelSettings {
            global_default_model: Some("global-default".into()),
            ..Default::default()
        };
        assert_eq!(
            settings.resolved_default_model(None).as_deref(),
            Some("global-default"),
            "global_default_model must fill the tier-1 pin when no multi_agent default exists",
        );
    }

    #[test]
    fn resolved_default_model_traps_whitespace_as_unset() {
        // Whitespace-only values are treated as unset in both positions:
        // an empty multi-agent default falls through to the global default,
        // and an empty global default leaves nothing resolved.
        let settings = ModelSettings {
            global_default_model: Some("global-default".into()),
            ..Default::default()
        };
        let multi_agent = multi_agent_with_default(Some("   "));
        assert_eq!(
            settings.resolved_default_model(Some(&multi_agent)).as_deref(),
            Some("global-default"),
            "whitespace-only multi_agent.default_model must fall through to the global default",
        );

        let no_global =
            ModelSettings { global_default_model: Some("   ".into()), ..Default::default() };
        assert_eq!(
            no_global.resolved_default_model(None),
            None,
            "whitespace-only global_default_model must resolve to None",
        );
        assert_eq!(
            no_global.resolved_default_model(Some(&multi_agent)),
            None,
            "whitespace-only values in both positions must resolve to None",
        );
    }

    #[test]
    fn resolved_default_model_none_when_neither_is_set() {
        let settings = ModelSettings::default();
        assert_eq!(settings.resolved_default_model(None), None);
        assert_eq!(
            settings.resolved_default_model(Some(&multi_agent_with_default(None))),
            None,
            "an explicitly-None multi_agent default must not resolve anything",
        );
    }

    // ------------------------------------------------------------------
    // ProviderConfig::effective_api_key (keyring -> <PROVIDER>_API_KEY env)
    // ------------------------------------------------------------------

    /// Drop guard deleting an env var even when the test panics, so it never
    /// leaks into tests running in parallel.
    struct EnvVarGuard(&'static str);
    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            std::env::remove_var(self.0);
        }
    }

    #[test]
    fn effective_api_key_falls_back_to_env_when_keyring_missing() {
        // A unique provider id keeps the env var from colliding with any real
        // provider or other tests.
        let provider = ProviderConfig { provider: "TESTPROVXYZ".into(), ..Default::default() };
        let store = CredentialStore::from_env();

        // Nothing configured anywhere: both resolutions refuse.
        assert!(provider.api_key(&store).is_err());
        assert!(provider.effective_api_key(&store).is_err());

        // Env-only key: the effective resolution must fill it while the raw
        // keyring-only `api_key` still errors (env is out of scope for it).
        let _guard = EnvVarGuard("TESTPROVXYZ_API_KEY");
        std::env::set_var("TESTPROVXYZ_API_KEY", "sk-test-xyz");
        assert!(provider.api_key(&store).is_err(), "keyring-only api_key must not read the env");
        assert_eq!(
            provider.effective_api_key(&store).unwrap(),
            "sk-test-xyz",
            "effective_api_key must fall back to the <PROVIDER>_API_KEY env var",
        );
    }
}
