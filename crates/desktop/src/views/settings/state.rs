use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use concerto_api_types::extension::{McpToolDescriptor, SkillDescriptor};
use concerto_config::managed::ManagedRuntimeManager;
use concerto_config::shell::ShellProfileConfig;
use concerto_config::{
    AgentRelationshipConfig, AppConfig, ConditionDef, ManagedEnvConfig, McpConfig, McpServerConfig,
    PolicyConfig, PolicyRuleDef, ProviderConfig, ShellSettings, SkillsConfig,
};
use concerto_providers::provider_defs::{
    model_options_for, provider_definition, PROVIDER_TYPE_IDS,
};

use crate::theme::AppTheme;

use super::helpers::default_managed_source;
use super::message::SectionId;
use super::{
    readable_provider_label, Message, PolicyActionChoice, PolicyConditionChoice,
    CUSTOM_MODEL_SENTINEL, FILESYSTEM_OPERATIONS, POLICY_ACTIONS, POLICY_CONDITION_KINDS,
    POLICY_OPERATION_TOOLS, POLICY_TOOLS,
};

pub struct State {
    // Theme / display
    pub theme_names: Vec<&'static str>,
    pub selected_theme: &'static str,
    pub font_size: f32,

    // Multi-provider state
    pub providers: Vec<ProviderConfig>,
    /// Global default model used for single-agent mode and as fallback when an
    /// agent assignment's provider no longer exists. `None` = first provider's
    /// default model.
    pub global_default_model: Option<String>,
    pub relationship_rules: Vec<AgentRelationshipConfig>,
    pub new_relationship_from: &'static str,
    pub new_relationship_to: &'static str,
    pub new_relationship_type: &'static str,
    pub new_relationship_cycles: String,
    /// Inline validation feedback for the relationship builder (self-reference,
    /// duplicate, etc.). `None` when the current input is valid.
    pub relationship_warning: Option<String>,
    /// True once the user edits the relationship manager in Settings. The
    /// studio owns `multi_agent.relationships` and saves it independently; the
    /// relationship manager here is seeded from config at startup, so without
    /// this flag a plain Settings save (theme, retry, …) would silently
    /// overwrite newer studio-saved relationships with this snapshot.
    pub relationship_dirty: bool,

    // Cached provider display data (rebuilt when providers change)
    pub cached_provider_ids: Vec<String>,
    pub cached_provider_labels: Vec<String>,
    /// Per-provider model-option lists (shared resolver output), rebuilt in
    /// `rebuild_cache` so the chat header model picker can borrow them for `'a`.
    pub cached_provider_model_options: Vec<Vec<String>>,
    pub cached_model_names: Vec<String>,
    pub cached_models_by_provider: HashMap<String, Vec<String>>,

    // Add-provider form state
    pub show_form: bool,
    pub form_provider_type: String,
    pub form_name: String,
    pub form_api_base: String,
    pub form_api_key: String,

    /// Inline credential-edit state for an existing provider (plan §5.3).
    pub editing_key_for: Option<usize>,
    pub key_edit_text: String,
    /// When `Some(idx)`, the Clear action for that provider is awaiting confirm.
    pub confirm_clear_for: Option<usize>,
    /// When `Some(idx)`, deleting that provider is awaiting confirm. Provider
    /// deletion is destructive (removes the API key from the keyring), so the
    /// first delete press only arms this prompt.
    pub confirm_delete_for: Option<usize>,

    // Policy (kept)
    pub policy_rules: Vec<PolicyRuleDef>,
    pub new_policy_action: PolicyActionChoice,
    pub new_policy_condition_kind: PolicyConditionChoice,
    pub new_policy_tool: &'static str,
    pub new_policy_operation: &'static str,
    pub new_policy_condition_value: String,

    // Memory (kept)
    pub memory_enabled: bool,
    pub memory_ttl_days: f32,

    // Provider retry and recovery
    pub retry_enabled: bool,
    pub retry_initial_delay_ms: f32,
    pub retry_max_delay_ms: f32,
    pub retry_multiplier: f32,
    pub retry_fixed_delay_ms: String,
    /// Inline validation feedback for the fixed-delay override. `None` when the
    /// input is blank or a valid positive integer.
    pub retry_fixed_delay_error: Option<String>,
    pub retry_respect_after: bool,
    pub retry_jitter: bool,
    pub retry_max_elapsed_seconds: String,
    /// Inline validation feedback for the outage-time limit. `None` when the
    /// input is blank or a valid positive integer.
    pub retry_max_elapsed_error: Option<String>,

    // ADR-28 — Shell profiles and integrated toolchain
    pub shell_profiles: Vec<ShellProfileConfig>,
    /// Canonical shell selection. Agent execution is the primary consumer.
    pub shell_active_profile: String,
    pub selected_shell_profile: Option<usize>,
    /// Scratch inputs for adding a new env entry to the selected profile.
    pub shell_new_env_key: String,
    pub shell_new_env_value: String,
    /// Last `Test profile` result for the profile at index 0 of the tuple,
    /// shown transiently under the editor's Test button (ADR-28 Slice 1).
    pub shell_test_result: Option<(usize, String)>,

    // ADR-28 Slice 2 — Managed Bash runtime management UI state.
    /// Source bash path adopted by the Install action (defaults to a sensible
    /// system Bash; the user can point it at any local bash).
    pub shell_managed_source: String,
    /// Destination path for `Export manifest`.
    pub shell_managed_export_path: String,
    /// Source path for `Import manifest`.
    pub shell_managed_import_path: String,
    /// Transient result line for the last managed-runtime action.
    pub shell_managed_result: Option<String>,
    pub settings_saved_notice: bool,
    /// True when any settings state (policy/relationship/memory/retry/shell/
    /// providers) has changed since the last `SaveSettings`. Cleared on save.
    pub settings_dirty: bool,
    /// Tracks which sections the user has collapsed.
    #[allow(private_interfaces)]
    pub collapsed_sections: HashSet<SectionId>,

    // ADR-37 — Plugin grant lifecycle
    /// List of plugin IDs with capability grants.
    pub plugin_granted_ids: Vec<String>,
    /// Per-plugin capability summary strings (e.g. "FilesystemRead, ShellExecute").
    pub plugin_grants_summary: Vec<String>,
    /// Transient result after a plugin revoke action.
    pub plugin_revoke_result: Option<String>,

    // ADR-43 — Skills configuration
    /// Master skills toggle (`skills.enabled`).
    pub skills_enabled: bool,
    /// Search paths for skill packs (display + discovery).
    pub skills_search_paths: Vec<String>,
    /// Whether auto-load of discovered skills is enabled (display only in v1).
    pub skills_auto_load: bool,
    /// Discovered skill packs, populated lazily when the page opens.
    pub skills_discovered: Vec<SkillDescriptor>,
    /// True once a discovery run has completed (success or failure).
    pub skills_loaded: bool,
    /// True while a discovery run is in flight.
    pub skills_loading: bool,
    /// Human-readable discovery failure, when the last run failed.
    pub skills_error: Option<String>,
    /// True = all discovered skills are candidates (`enabled_ids: None`);
    /// false = `skills_enabled_ids` is the explicit allow-list.
    pub skills_allow_all: bool,
    /// Explicit allow-list of enabled skill ids (`skills.enabled_ids`).
    pub skills_enabled_ids: Vec<String>,
    /// Ids of discovered skills whose instruction preview is expanded.
    /// Transient view state; never persisted and never arms the dirty flag.
    pub skills_expanded: HashSet<String>,

    // ADR-43 — MCP configuration
    /// Master MCP toggle (`mcp.enabled`).
    pub mcp_enabled: bool,
    /// Configured MCP servers (editable in v1: per-server enabled flag).
    pub mcp_servers: Vec<McpServerConfig>,
    /// Probe results keyed by server id. `Ok` = tool list, `Err` = error text.
    pub mcp_probe_results: HashMap<String, Result<Vec<McpToolDescriptor>, String>>,
    /// Server ids currently being probed.
    pub mcp_probing: HashSet<String>,
}

impl State {
    fn load_form_provider_type_def() -> &'static str {
        match PROVIDER_TYPE_IDS.first() {
            Some(first) => first,
            None => "anthropic",
        }
    }
}

impl Default for State {
    fn default() -> Self {
        Self::new()
    }
}

impl State {
    pub fn new() -> Self {
        Self::from_config(&AppConfig::default())
    }

    pub fn from_config(config: &AppConfig) -> Self {
        let theme_names = AppTheme::all().iter().map(|t| t.name).collect();
        let policy_rules = config.policy.as_ref().map(|p| p.rules.clone()).unwrap_or_default();

        // Load provider state from model_settings, falling back to single-provider config
        let providers = if let Some(ms) = &config.model_settings {
            ms.providers.clone()
        } else if let Some(pc) = &config.primary_provider_config {
            vec![pc.clone()]
        } else {
            Vec::new()
        };
        let global_default_model =
            config.model_settings.as_ref().and_then(|ms| ms.global_default_model.clone());

        // ADR-43 — skills and MCP seed from the (optional) config sections.
        // Absent sections fall back to the crate defaults (skills off,
        // MCP off with no servers).
        let skills = config.skills.clone().unwrap_or_default();
        let mcp = config.mcp.clone().unwrap_or_default();

        let mut state = Self {
            theme_names,
            selected_theme: "Midnight",
            font_size: 14.0,
            providers,
            global_default_model,
            relationship_rules: config
                .multi_agent
                .as_ref()
                .map(|multi| multi.relationships.clone())
                .unwrap_or_default(),
            new_relationship_from: "reviewer",
            new_relationship_to: "coder",
            new_relationship_type: "supervises",
            new_relationship_cycles: "3".into(),
            relationship_warning: None,
            relationship_dirty: false,
            cached_provider_ids: Vec::new(),
            cached_provider_labels: Vec::new(),
            cached_provider_model_options: Vec::new(),
            cached_model_names: Vec::new(),
            cached_models_by_provider: HashMap::new(),
            show_form: false,
            form_provider_type: State::load_form_provider_type_def().to_string(),
            form_name: String::new(),
            form_api_base: String::new(),
            form_api_key: String::new(),
            editing_key_for: None,
            key_edit_text: String::new(),
            confirm_clear_for: None,
            confirm_delete_for: None,
            policy_rules,
            new_policy_action: POLICY_ACTIONS[0],
            new_policy_condition_kind: POLICY_CONDITION_KINDS[0],
            new_policy_tool: POLICY_TOOLS[0],
            new_policy_operation: FILESYSTEM_OPERATIONS[0],
            new_policy_condition_value: String::new(),
            memory_enabled: config.memory.enabled,
            memory_ttl_days: f32::from(config.memory.ttl_days),
            retry_enabled: config.retry.enabled,
            retry_initial_delay_ms: config.retry.initial_delay_ms as f32,
            retry_max_delay_ms: config.retry.max_delay_ms as f32,
            retry_multiplier: config.retry.multiplier as f32,
            retry_fixed_delay_ms: config
                .retry
                .fixed_delay_ms
                .map(|value| value.to_string())
                .unwrap_or_default(),
            retry_fixed_delay_error: None,
            retry_respect_after: config.retry.respect_retry_after,
            retry_jitter: config.retry.jitter,
            retry_max_elapsed_seconds: config
                .retry
                .max_elapsed_seconds
                .map(|value| value.to_string())
                .unwrap_or_default(),
            retry_max_elapsed_error: None,
            settings_saved_notice: false,
            settings_dirty: false,
            shell_profiles: Vec::new(),
            shell_active_profile: String::new(),
            selected_shell_profile: None,
            shell_new_env_key: String::new(),
            shell_new_env_value: String::new(),
            shell_test_result: None,
            shell_managed_source: default_managed_source(),
            shell_managed_export_path: String::new(),
            shell_managed_import_path: String::new(),
            shell_managed_result: None,
            collapsed_sections: {
                let mut s = HashSet::new();
                s.insert(SectionId::Policy);
                s.insert(SectionId::Retry);
                s.insert(SectionId::Memory);
                s.insert(SectionId::Relationships);
                s.insert(SectionId::Shell);
                s.insert(SectionId::Plugins);
                s.insert(SectionId::Skills);
                s.insert(SectionId::Mcp);
                s
            },
            plugin_granted_ids: Vec::new(),
            plugin_grants_summary: Vec::new(),
            plugin_revoke_result: None,
            skills_enabled: skills.enabled,
            skills_search_paths: skills.search_paths.clone(),
            skills_auto_load: skills.auto_load,
            skills_discovered: Vec::new(),
            skills_loaded: false,
            skills_loading: false,
            skills_error: None,
            skills_allow_all: skills.enabled_ids.is_none(),
            skills_enabled_ids: skills.enabled_ids.clone().unwrap_or_default(),
            skills_expanded: HashSet::new(),
            mcp_enabled: mcp.enabled,
            mcp_servers: mcp.servers.clone(),
            mcp_probe_results: HashMap::new(),
            mcp_probing: HashSet::new(),
        };
        let shell = config.resolved_shell_settings();
        state.shell_active_profile = shell.selected_profile_id().to_owned();
        state.shell_profiles = shell.profiles;
        state.selected_shell_profile = None;
        state.shell_new_env_key = String::new();
        state.shell_new_env_value = String::new();
        state.load_plugin_grants();
        state.normalize_model_settings();
        state
    }

    /// Refresh the relationship manager list from the live merged config.
    ///
    /// The Orchestration Studio owns `multi_agent.relationships` and saves it
    /// independently of this page; the list here is seeded from config at
    /// startup and would otherwise show stale rows after a studio save (and
    /// any subsequent relationship edit here would be seeded from that stale
    /// snapshot). Called when the Settings page is opened.
    ///
    /// In-flight edits made in this relationship manager are never
    /// overwritten: once the user adds or removes a relationship
    /// (`relationship_dirty`), this list takes ownership until the user saves
    /// or leaves the page without saving.
    pub fn sync_relationships_from_config(&mut self, config: &AppConfig) {
        if self.relationship_dirty {
            return;
        }
        self.relationship_rules = config
            .multi_agent
            .as_ref()
            .map(|multi| multi.relationships.clone())
            .unwrap_or_default();
        // The inline validation text refers to the previous list; recompute
        // (or clear) it against the refreshed rows rather than leave a stale
        // warning.
        self.relationship_warning = None;
    }

    /// Refresh the Settings provider list from the live merged config.
    ///
    /// The provider rows here are seeded from config at startup and mutated
    /// by the add/delete/credential form actions; a (re)loaded config
    /// carrying external provider edits would otherwise never reach the form
    /// until a restart, and the next Settings save would silently persist
    /// the stale snapshot over the external edit. Called when the Settings
    /// page is opened and on every config reload.
    ///
    /// In-flight edits made in this list are never overwritten [ADR-57 §3d]:
    /// once the user arms `settings_dirty` (provider add/delete/key actions)
    /// or starts typing a credential replacement (`editing_key_for`), this
    /// list takes ownership until the user saves or leaves the page without
    /// saving.
    pub fn sync_providers_from_config(&mut self, config: &AppConfig) {
        if self.settings_dirty || self.editing_key_for.is_some() {
            return;
        }
        // Same seeding logic as `from_config`: `model_settings.providers`
        // wins, falling back to the single-provider config, else empty.
        self.providers = if let Some(ms) = &config.model_settings {
            ms.providers.clone()
        } else if let Some(pc) = &config.primary_provider_config {
            vec![pc.clone()]
        } else {
            Vec::new()
        };
        // Rebuild the derived caches from the refreshed rows.
        self.rebuild_cache();
        // Per-row transient UI state may dangle after a row replacement;
        // clear it. The add-provider form is independent of the rows, so it
        // stays untouched.
        self.confirm_delete_for = None;
        self.confirm_clear_for = None;
        self.editing_key_for = None;
        self.key_edit_text.clear();
    }

    /// Build the `AppConfig` fragments this page owns, merging onto `base`.
    pub fn to_config(&self, base: &AppConfig) -> AppConfig {
        let mut cfg = base.clone();

        // Build model-first settings. Agent assignments are owned by the
        // Orchestration Studio and saved separately.
        let mut model_settings = base.model_settings.clone().unwrap_or_default();
        model_settings.providers = self.providers.clone();
        model_settings.global_default_model = self.global_default_model.clone();
        model_settings.global_default_id = None;
        cfg.model_settings = Some(model_settings);
        cfg.primary_provider = None;
        cfg.primary_provider_config = None;

        cfg.policy = Some(PolicyConfig { rules: self.policy_rules.clone(), time_window: None });
        // Only the studio may publish relationships unless the user explicitly
        // edited them here; otherwise this startup snapshot would silently
        // revert relationships the studio saved meanwhile.
        if self.relationship_dirty {
            cfg.multi_agent.get_or_insert_with(Default::default).relationships =
                self.relationship_rules.clone();
        }
        cfg.retry.enabled = self.retry_enabled;
        cfg.retry.initial_delay_ms = self.retry_initial_delay_ms.round() as u64;
        cfg.retry.max_delay_ms = self.retry_max_delay_ms.round() as u64;
        cfg.retry.multiplier = self.retry_multiplier as f64;
        cfg.retry.fixed_delay_ms =
            self.retry_fixed_delay_ms.trim().parse::<u64>().ok().filter(|value| *value > 0);
        cfg.retry.respect_retry_after = self.retry_respect_after;
        cfg.retry.jitter = self.retry_jitter;
        cfg.retry.max_elapsed_seconds =
            self.retry_max_elapsed_seconds.trim().parse::<u64>().ok().filter(|value| *value > 0);
        cfg.memory.enabled = self.memory_enabled;
        cfg.memory.ttl_days = self.memory_ttl_days.round().clamp(1.0, 365.0) as u16;

        // Persist the canonical shell profile. The managed environment
        // config is mirrored from the live runtime manager (source of truth) so
        // the saved config always reflects what is actually installed.
        let managed = ManagedRuntimeManager::auto_detect().map(|m| ManagedEnvConfig {
            install_dir: m.bash_executable.parent().map(PathBuf::from),
            version: Some(m.version.clone()),
            runtime_manifest: ManagedRuntimeManager::for_data_dir()
                .ok()
                .map(|mgr| mgr.manifest_path()),
            tool_manifest: None,
            offline: m.offline,
            integrity_enabled: m.integrity_enabled,
        });
        cfg.shell_settings = Some(ShellSettings::new(
            self.shell_profiles.clone(),
            self.shell_active_profile.clone(),
            managed,
        ));

        // ADR-43 — skills & MCP. Published on every save, seeded from config
        // at startup; the master toggles and the allow-list edits made here
        // are what differ from the base. `enabled_ids` stays `None` (all
        // discovered skills are candidates) until the user edits the
        // allow-list.
        cfg.skills = Some(SkillsConfig {
            enabled: self.skills_enabled,
            search_paths: self.skills_search_paths.clone(),
            auto_load: self.skills_auto_load,
            enabled_ids: if self.skills_allow_all {
                None
            } else {
                Some(self.skills_enabled_ids.clone())
            },
            max_chars: base.skills.as_ref().and_then(|skills| skills.max_chars),
        });
        cfg.mcp = Some(McpConfig { enabled: self.mcp_enabled, servers: self.mcp_servers.clone() });
        cfg
    }

    /// Validate an optional positive-integer field. Blank is valid (it means
    /// "use the default / retry indefinitely"); any other input must be a
    /// whole number greater than zero. Returns `Some(message)` when invalid.
    fn validate_optional_positive_int(s: &str) -> Option<String> {
        let trimmed = s.trim();
        if trimmed.is_empty() {
            return None; // blank is valid (means "use default/unlimited")
        }
        match trimmed.parse::<u64>() {
            Ok(v) if v > 0 => None,
            Ok(_) => Some("Must be a positive number".into()),
            Err(_) => Some("Must be a whole number".into()),
        }
    }

    pub(super) fn rule_display(rule: &PolicyRuleDef) -> String {
        let action = match rule.action.as_str() {
            "auto_approve" => "Allow automatically",
            "auto_deny" => "Deny",
            _ => "Ask for approval",
        };
        let cond = match &rule.condition {
            ConditionDef::ToolName { tool_name } => format!("when the tool is '{tool_name}'"),
            ConditionDef::ToolOperation { tool_name, operation } => {
                format!("when '{tool_name}' performs '{operation}'")
            }
            ConditionDef::PathGlob { path_glob } => {
                format!("when a project path matches '{path_glob}'")
            }
            ConditionDef::CommandPattern { command_pattern } => {
                format!("when a shell command matches /{command_pattern}/")
            }
            ConditionDef::GitOperation { git_operation } => {
                format!("when 'git' performs '{git_operation}'")
            }
            ConditionDef::Always { .. } => "for every operation".to_string(),
            _ => String::new(),
        };
        format!("{action} {cond}")
    }

    /// Human-readable sentence for an agent relationship rule, e.g.
    /// "reviewer supervises coder (max 3 cycles)".
    pub(super) fn relationship_display(rule: &AgentRelationshipConfig) -> String {
        match rule.max_cycles {
            Some(cycles) => {
                format!("{} {} {} (max {} cycles)", rule.from, rule.relationship, rule.to, cycles)
            }
            None => format!("{} {} {}", rule.from, rule.relationship, rule.to),
        }
    }

    /// Live, plain-language preview of the policy rule currently being built.
    pub(super) fn policy_preview(
        action: PolicyActionChoice,
        kind: PolicyConditionChoice,
        tool: &str,
        operation: &str,
        value: &str,
    ) -> String {
        let verb = match action {
            PolicyActionChoice::Allow => "Allow automatically",
            PolicyActionChoice::Ask => "Ask for approval",
            PolicyActionChoice::Deny => "Deny",
        };
        match kind {
            PolicyConditionChoice::Tool => format!("{verb} when the tool is '{tool}'"),
            PolicyConditionChoice::ToolOperation => {
                format!("{verb} when '{tool}' performs '{operation}'")
            }
            PolicyConditionChoice::ProjectPath if value.trim().is_empty() => {
                format!("{verb} when a project path matches the glob you enter")
            }
            PolicyConditionChoice::ProjectPath => {
                format!("{verb} when a project path matches '{}'", value.trim())
            }
            PolicyConditionChoice::ShellCommand if value.trim().is_empty() => {
                format!("{verb} when a shell command matches the regular expression you enter")
            }
            PolicyConditionChoice::ShellCommand => {
                format!("{verb} when a shell command matches /{}/", value.trim())
            }
            PolicyConditionChoice::Always => format!("{verb} for every operation"),
        }
    }

    pub(super) fn policy_condition_help(kind: PolicyConditionChoice) -> &'static str {
        match kind {
            PolicyConditionChoice::Tool => {
                "Applies to every call made through one available tool. 'filesystem' reads and changes files inside the selected project; 'shell' runs terminal commands in that project."
            }
            PolicyConditionChoice::ToolOperation => {
                "Applies to one exact filesystem operation: read, write, delete, or exists. Choose 'write' to control the code-writing action shown in the chat tool log."
            }
            PolicyConditionChoice::ProjectPath => {
                "Applies to filesystem calls whose project-relative path matches a glob. Examples: '**/*' for all files, 'src/**' for source files, or '**/*.rs' for Rust files."
            }
            PolicyConditionChoice::ShellCommand => {
                "Applies to shell commands matching a regular expression. Examples: '^cargo (check|test)$' or '^git status$'. Invalid regular expressions never match."
            }
            PolicyConditionChoice::Always => {
                "Applies to every tool and operation. Place a catch-all rule last because policy rules stop at the first match."
            }
        }
    }

    pub(super) fn policy_value_placeholder(kind: PolicyConditionChoice) -> &'static str {
        match kind {
            PolicyConditionChoice::ProjectPath => "e.g. src/** or **/*.rs",
            PolicyConditionChoice::ShellCommand => "e.g. ^cargo (check|test)$",
            _ => "",
        }
    }

    pub(super) fn operation_options(_tool: &str) -> &'static [&'static str] {
        FILESYSTEM_OPERATIONS
    }

    fn generate_provider_id(&self) -> String {
        format!("prov_{}", concerto_core::ids::Ulid::new())
    }

    /// Model options for a provider: static known models merged with any models
    /// discovered at runtime and persisted in `ProviderConfig::cached_models`.
    fn model_options_with_discovered(p: &ProviderConfig) -> Vec<String> {
        let def = provider_definition(&p.provider);
        let mut opts = model_options_for(p, &def, None);
        let mut seen: std::collections::HashSet<String> =
            opts.iter().map(|s| s.to_lowercase()).collect();
        for m in &p.cached_models {
            let t = m.trim().to_string();
            if !t.is_empty() && seen.insert(t.to_lowercase()) {
                opts.push(t);
            }
        }
        opts
    }

    fn rebuild_cache(&mut self) {
        let providers = self.providers.clone();
        self.rebuild_cache_with(&providers);
    }

    /// Recompute every derived provider cache from a provider list. Shared by
    /// `rebuild_cache` (form-backed) and `refresh_provider_cache_from_config`
    /// (config-backed).
    fn rebuild_cache_with(&mut self, providers: &[ProviderConfig]) {
        self.cached_provider_ids = providers.iter().map(|p| p.id.clone()).collect();
        self.cached_provider_labels = providers.iter().map(readable_provider_label).collect();

        // Precompute the shared model-option lists so the `pick_list` widgets
        // can borrow them for the view lifetime `'a`.
        self.cached_provider_model_options = providers
            .iter()
            .map(|p| {
                let mut opts = Self::model_options_with_discovered(p);
                opts.push(CUSTOM_MODEL_SENTINEL.to_string());
                opts
            })
            .collect();

        // Mainline cache: flat + per-provider model names used by the chat header
        // and model pickers. Sourced from each provider's model options (pinned /
        // known / discovered) regardless of credential readiness — the chat
        // picker should suggest models even before a key is stored.
        self.cached_models_by_provider.clear();
        self.cached_model_names.clear();
        for provider in providers {
            let models = Self::model_options_with_discovered(provider);
            self.cached_models_by_provider
                .entry(provider.id.clone())
                .or_default()
                .extend(models.iter().cloned());
            self.cached_model_names.extend(models);
        }
        self.cached_model_names.sort();
        self.cached_model_names.dedup();
        for models in self.cached_models_by_provider.values_mut() {
            models.sort();
            models.dedup();
        }
    }

    /// Refresh the derived provider caches from a (re)loaded config without
    /// touching form fields, the dirty flag, or the Shell editor state.
    ///
    /// External config edits flow into the label/id and model caches (used by
    /// the Studio model sync and provider pickers) while in-flight form edits
    /// are preserved and win on the next explicit save [ADR-57 §3d]. This is
    /// the cache-only half of a reload: the Settings form rows themselves are
    /// refreshed by [`Self::sync_providers_from_config`], which callers
    /// invoke before this when a reloaded config may have changed the
    /// provider list.
    pub fn refresh_provider_cache_from_config(&mut self, config: &AppConfig) {
        let providers: Vec<ProviderConfig> = match &config.model_settings {
            Some(ms) => ms.providers.clone(),
            None => config.primary_provider_config.clone().into_iter().collect(),
        };
        self.rebuild_cache_with(&providers);
    }

    fn normalize_model_settings(&mut self) {
        self.rebuild_cache();
    }

    /// Load plugin grants from the capability store and populate UI state.
    pub fn load_plugin_grants(&mut self) {
        let data_dir = dirs::data_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .join("concerto")
            .join("plugins");
        match concerto_plugins::capability::CapabilityManager::open(&data_dir) {
            Ok(cap_mgr) => {
                let plugins = cap_mgr.list_granted_plugins();
                self.plugin_granted_ids = plugins.clone();
                self.plugin_grants_summary = plugins
                    .iter()
                    .map(|id| {
                        let grants = cap_mgr.load_grants(id, None);
                        grants
                            .iter()
                            .map(|(d, _, _)| format!("{d:?}"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .collect();
            }
            Err(e) => {
                tracing::error!(error = %e, "failed to open capability store for plugin grants UI");
                self.plugin_granted_ids.clear();
                self.plugin_grants_summary.clear();
            }
        }
    }

    pub fn model_names_for_provider(&self, provider_id: &str) -> &[String] {
        self.cached_models_by_provider.get(provider_id).map(Vec::as_slice).unwrap_or(&[])
    }

    /// Return a snapshot of the per-provider model cache (provider_id → model
    /// names). Used by the Orchestration Studio to populate its unified model
    /// picker.
    pub fn cached_models_by_provider(&self) -> HashMap<String, Vec<String>> {
        self.cached_models_by_provider.clone()
    }

    /// Whether skill discovery has not yet run and no run is in flight. The
    /// App uses this on Settings open to trigger the one lazy discovery pass.
    pub fn skills_never_discovered(&self) -> bool {
        !self.skills_loaded && !self.skills_loading
    }

    /// Start a skill discovery pass. Sets the loading flag and returns the
    /// task whose completion is routed back as `SkillsDiscoveryResult`.
    /// Idempotent: a second call while a run is in flight is a no-op.
    pub fn start_skill_discovery(&mut self) -> iced::Task<Message> {
        if self.skills_loading {
            return iced::Task::none();
        }
        self.skills_loading = true;
        let search_paths = self.skills_search_paths.clone();
        iced::Task::perform(
            async move { super::helpers::discover_skills(search_paths) },
            Message::SkillsDiscoveryResult,
        )
    }

    /// Switch the skills allow-list from "all discovered" (`enabled_ids:
    /// None`) to an explicit list seeded from the last discovery run. Called
    /// the first time the user edits an individual skill checkbox; afterwards
    /// the explicit list is the single source of truth.
    fn ensure_skills_allow_list_materialized(&mut self) {
        if self.skills_allow_all {
            self.skills_allow_all = false;
            self.skills_enabled_ids =
                self.skills_discovered.iter().map(|skill| skill.id.clone()).collect();
        }
    }

    pub fn update(&mut self, message: Message) -> iced::Task<Message> {
        match message {
            Message::ThemeSelected(name) => self.selected_theme = name,
            Message::FontSizeChanged(size) => self.font_size = size.clamp(12.0, 20.0),

            // Legacy single-provider messages (no-op when using multi-provider)
            Message::ProviderSelected(_) => {}
            Message::ModelChanged(_) => {}
            Message::ApiBaseChanged(_) => {}
            Message::ProviderApiKeyChanged(_) => {}
            Message::SaveProviderKey => {}
            Message::ClearProviderKey => {}

            // Multi-provider management
            Message::ProviderAddPressed => {
                self.show_form = true;
                self.form_provider_type = State::load_form_provider_type_def().to_string();
                self.form_name.clear();
                self.form_api_base.clear();
                self.form_api_key.clear();
            }
            Message::ProviderDeletePressed(idx) => {
                // Toggle the confirm prompt; the destructive removal (provider
                // + keyring key) only happens on ProviderDeleteConfirmed
                // (plan §5.3 — explicit, confirmed delete).
                if self.confirm_delete_for == Some(idx) {
                    self.confirm_delete_for = None;
                } else {
                    self.confirm_delete_for = Some(idx);
                }
                self.settings_dirty = true;
            }
            Message::ProviderDeleteConfirmed(idx) => {
                if idx < self.providers.len() {
                    // Delete API key from keyring before removing provider (plan §5.3)
                    let key_to_delete = self.providers[idx].keyring_key.clone();
                    if !key_to_delete.is_empty() {
                        let creds = concerto_config::CredentialStore::new();
                        if let Err(e) = creds.delete(&key_to_delete) {
                            tracing::error!(error = %e, "failed to delete API key for provider {}", self.providers[idx].name);
                        }
                    }
                    self.providers.remove(idx);
                    self.confirm_delete_for = None;
                    self.normalize_model_settings();
                }
                self.settings_dirty = true;
            }
            Message::ProviderDeleteCancelled(_) => {
                self.confirm_delete_for = None;
            }
            Message::FormProviderTypeChanged(t) => {
                self.form_provider_type = t;
            }
            Message::FormNameChanged(n) => self.form_name = n,
            Message::FormApiBaseChanged(b) => self.form_api_base = b,
            Message::FormApiKeyChanged(k) => self.form_api_key = k,
            Message::FormSaveKey(idx) => {
                if idx < self.providers.len() {
                    let key = self.key_edit_text.trim().to_string();
                    if !key.is_empty() {
                        let creds = concerto_config::CredentialStore::new();
                        if let Err(e) = creds.set(&self.providers[idx].keyring_key, &key) {
                            tracing::error!(error = %e, "failed to save API key for provider {}", self.providers[idx].name);
                        }
                    }
                    self.key_edit_text.clear();
                    self.editing_key_for = None;
                    self.confirm_clear_for = None;
                    // Rebuild caches so the model picker sees models from this
                    // newly-credentialed provider.
                    self.rebuild_cache();
                }
                self.settings_dirty = true;
            }
            Message::FormClearKey(idx) => {
                // Toggle the confirm prompt; actual deletion happens on
                // FormClearKeyConfirmed (plan §5.3 — explicit, confirmed clear).
                if self.confirm_clear_for == Some(idx) {
                    self.confirm_clear_for = None;
                } else {
                    self.confirm_clear_for = Some(idx);
                }
                self.settings_dirty = true;
            }

            Message::FormEditKeyPressed(idx) => {
                self.editing_key_for = Some(idx);
                self.key_edit_text.clear();
                self.confirm_clear_for = None;
            }
            Message::FormKeyEditTextChanged(s) => self.key_edit_text = s,
            Message::FormClearKeyConfirmed(idx) => {
                if idx < self.providers.len() {
                    let key_to_delete = self.providers[idx].keyring_key.clone();
                    if !key_to_delete.is_empty() {
                        let creds = concerto_config::CredentialStore::new();
                        if let Err(e) = creds.delete(&key_to_delete) {
                            tracing::error!(error = %e, "failed to delete API key for provider {}", self.providers[idx].name);
                        }
                    }
                }
                self.editing_key_for = None;
                self.key_edit_text.clear();
                self.confirm_clear_for = None;
                // Rebuild caches so the model picker stops offering models
                // from this now-credentialess provider.
                self.rebuild_cache();
                self.settings_dirty = true;
            }
            Message::FormKeyEditCancel(idx) => {
                if self.editing_key_for == Some(idx) {
                    self.editing_key_for = None;
                }
                self.key_edit_text.clear();
                self.confirm_clear_for = None;
            }
            Message::FormConfirmAdd => {
                let id = self.generate_provider_id();
                let name = if self.form_name.is_empty() {
                    provider_definition(&self.form_provider_type).display_name.to_string()
                } else {
                    self.form_name.clone()
                };
                let keyring_key = format!("{}/api_key", &self.form_provider_type);

                let _def = provider_definition(&self.form_provider_type);

                // Save API key to keychain if provided.
                if !self.form_api_key.is_empty() {
                    let creds = concerto_config::CredentialStore::new();
                    let _ = creds.set(&keyring_key, &self.form_api_key);
                }

                // Providers are created with no model; the global default model
                // is selected via the unified picker below.

                self.providers.push(ProviderConfig {
                    id: id.clone(),
                    name,
                    provider: self.form_provider_type.clone(),
                    model: String::new(),
                    api_base: if self.form_api_base.trim().is_empty() {
                        None
                    } else {
                        Some(self.form_api_base.trim().to_string())
                    },
                    timeout_seconds: 30,
                    keyring_key: keyring_key.clone(),
                    cached_models: Vec::new(),
                    cached_models_fetched_at: 0,
                    ..ProviderConfig::default()
                });

                self.normalize_model_settings();
                self.show_form = false;
                self.form_api_key.clear();

                self.settings_dirty = true;
            }
            Message::FormCancel => {
                self.show_form = false;
            }

            // Phase 3 — model discovery (auto-triggered at startup; no manual button)
            Message::ProviderModelsRefreshed { provider_id, request_id: _, result } => {
                if let Some(p) = self.providers.iter_mut().find(|p| p.id == provider_id) {
                    if let Ok(models) = result {
                        p.record_discovered_models(models);
                    }
                }
                self.rebuild_cache();
                // Discovered models are provider settings: arm the dirty flag
                // so they persist together with other changes on Save Settings.
                // They are re-fetched at startup regardless, so an unsaved
                // discovery is never permanently lost.
                self.settings_dirty = true;
            }

            // Global default model — single unified picker.
            Message::GlobalDefaultModelChanged(model) => {
                self.global_default_model = model;
                self.rebuild_cache();
            }

            // Policy / relationships / memory are persisted on explicit Save Settings.
            Message::RelationshipFromChanged(role) => {
                self.settings_dirty = true;
                self.new_relationship_from = role;
                self.relationship_warning = None;
            }
            Message::RelationshipToChanged(role) => {
                self.settings_dirty = true;
                self.new_relationship_to = role;
                self.relationship_warning = None;
            }
            Message::RelationshipTypeChanged(relationship) => {
                self.settings_dirty = true;
                self.new_relationship_type = relationship;
                self.relationship_warning = None;
            }
            Message::RelationshipCyclesChanged(value) => {
                self.settings_dirty = true;
                self.new_relationship_cycles = value;
                self.relationship_warning = None;
            }
            Message::RelationshipAdded => {
                self.settings_dirty = true;
                self.relationship_dirty = true;
                // Inline validation: surface problems instead of silently
                // dropping or overwriting rules.
                if self.new_relationship_from == self.new_relationship_to {
                    self.relationship_warning =
                        Some("An agent cannot have a relationship with itself.".into());
                    return iced::Task::none();
                }
                let max_cycles = self
                    .new_relationship_cycles
                    .trim()
                    .parse::<u32>()
                    .ok()
                    .filter(|cycles| *cycles > 0);
                let rule = AgentRelationshipConfig {
                    from: self.new_relationship_from.into(),
                    to: self.new_relationship_to.into(),
                    relationship: self.new_relationship_type.into(),
                    max_cycles,
                };
                let duplicate = self
                    .relationship_rules
                    .iter()
                    .any(|existing| existing.from == rule.from && existing.to == rule.to);
                if duplicate {
                    self.relationship_warning = Some(format!(
                        "A relationship from '{}' to '{}' already exists; the new rule replaces it.",
                        rule.from, rule.to
                    ));
                } else {
                    self.relationship_warning = None;
                }
                if let Some(existing) = self
                    .relationship_rules
                    .iter_mut()
                    .find(|existing| existing.from == rule.from && existing.to == rule.to)
                {
                    *existing = rule;
                } else {
                    self.relationship_rules.push(rule);
                }
            }
            Message::RelationshipRemoved(index) => {
                self.settings_dirty = true;
                self.relationship_dirty = true;
                if index < self.relationship_rules.len() {
                    self.relationship_rules.remove(index);
                }
            }

            // Policy (kept)
            Message::NewPolicyActionSelected(a) => {
                self.settings_dirty = true;
                self.new_policy_action = a;
            }
            Message::NewPolicyConditionKindSelected(k) => {
                self.settings_dirty = true;
                self.new_policy_condition_kind = k;
                if k == PolicyConditionChoice::ToolOperation
                    && !POLICY_OPERATION_TOOLS.contains(&self.new_policy_tool)
                {
                    self.new_policy_tool = POLICY_OPERATION_TOOLS[0];
                    self.new_policy_operation = FILESYSTEM_OPERATIONS[0];
                }
            }
            Message::NewPolicyToolSelected(tool) => {
                self.settings_dirty = true;
                self.new_policy_tool = tool;
                self.new_policy_operation = Self::operation_options(tool)[0];
            }
            Message::NewPolicyOperationSelected(operation) => {
                self.settings_dirty = true;
                self.new_policy_operation = operation;
            }
            Message::NewPolicyConditionValueChanged(v) => {
                self.settings_dirty = true;
                self.new_policy_condition_value = v;
            }
            Message::PolicyRuleAdded => {
                self.settings_dirty = true;
                let condition = match self.new_policy_condition_kind {
                    PolicyConditionChoice::Tool => {
                        ConditionDef::ToolName { tool_name: self.new_policy_tool.to_string() }
                    }
                    PolicyConditionChoice::ToolOperation => ConditionDef::ToolOperation {
                        tool_name: self.new_policy_tool.to_string(),
                        operation: self.new_policy_operation.to_string(),
                    },
                    PolicyConditionChoice::ProjectPath => ConditionDef::PathGlob {
                        path_glob: self.new_policy_condition_value.clone(),
                    },
                    PolicyConditionChoice::ShellCommand => ConditionDef::CommandPattern {
                        command_pattern: self.new_policy_condition_value.clone(),
                    },
                    PolicyConditionChoice::Always => ConditionDef::Always { always: true },
                };
                if matches!(
                    self.new_policy_condition_kind,
                    PolicyConditionChoice::Tool
                        | PolicyConditionChoice::ToolOperation
                        | PolicyConditionChoice::Always
                ) || !self.new_policy_condition_value.trim().is_empty()
                {
                    self.policy_rules.push(PolicyRuleDef {
                        action: self.new_policy_action.config_value().to_string(),
                        condition,
                    });
                    self.new_policy_condition_value.clear();
                }
            }
            Message::PolicyRuleRemoved(idx) => {
                self.settings_dirty = true;
                if idx < self.policy_rules.len() {
                    self.policy_rules.remove(idx);
                }
            }
            Message::PolicyRuleMovedUp(idx) => {
                self.settings_dirty = true;
                if idx > 0 && idx < self.policy_rules.len() {
                    self.policy_rules.swap(idx, idx - 1);
                }
            }
            Message::PolicyRuleMovedDown(idx) => {
                self.settings_dirty = true;
                if idx + 1 < self.policy_rules.len() {
                    self.policy_rules.swap(idx, idx + 1);
                }
            }

            Message::MemoryEnabledToggled(v) => {
                self.settings_dirty = true;
                self.memory_enabled = v;
            }
            Message::MemoryTtlChanged(v) => {
                self.settings_dirty = true;
                self.memory_ttl_days = v.clamp(1.0, 365.0);
            }
            Message::RetryEnabledToggled(value) => {
                self.settings_dirty = true;
                self.retry_enabled = value;
            }
            Message::RetryInitialDelayChanged(value) => {
                self.settings_dirty = true;
                self.retry_initial_delay_ms = value;
            }
            Message::RetryMaxDelayChanged(value) => {
                self.settings_dirty = true;
                self.retry_max_delay_ms = value;
            }
            Message::RetryMultiplierChanged(value) => {
                self.settings_dirty = true;
                self.retry_multiplier = value;
            }
            Message::RetryFixedDelayChanged(value) => {
                self.settings_dirty = true;
                self.retry_fixed_delay_ms = value;
                self.retry_fixed_delay_error =
                    Self::validate_optional_positive_int(&self.retry_fixed_delay_ms);
            }
            Message::RetryRespectAfterToggled(value) => {
                self.settings_dirty = true;
                self.retry_respect_after = value;
            }
            Message::RetryJitterToggled(value) => {
                self.settings_dirty = true;
                self.retry_jitter = value;
            }
            Message::RetryMaxElapsedChanged(value) => {
                self.settings_dirty = true;
                self.retry_max_elapsed_seconds = value;
                self.retry_max_elapsed_error =
                    Self::validate_optional_positive_int(&self.retry_max_elapsed_seconds);
            }
            Message::SaveSettings => {
                self.settings_saved_notice = true;
                self.settings_dirty = false;
                self.relationship_dirty = false;
            }
            Message::ToggleSection(id) => {
                if !self.collapsed_sections.remove(&id) {
                    self.collapsed_sections.insert(id);
                }
            }

            // ADR-37 — Plugin grant lifecycle. Grants are persisted in the
            // capability store (not AppConfig), so revoke never touches
            // `settings_dirty`.
            Message::PluginRevokePressed(plugin_id) => {
                let data_dir = dirs::data_dir()
                    .unwrap_or_else(|| std::path::PathBuf::from("."))
                    .join("concerto")
                    .join("plugins");
                let result = (|| -> Result<(), String> {
                    let cap_mgr = concerto_plugins::capability::CapabilityManager::open(&data_dir)
                        .map_err(|e| format!("could not open capability store: {e}"))?;
                    cap_mgr.revoke_plugin(&plugin_id).map_err(|e| e.to_string())?;
                    Ok(())
                })();
                match &result {
                    Ok(()) => {
                        self.plugin_revoke_result =
                            Some(format!("Revoked grants for '{plugin_id}'"));
                        // Remove from cached lists.
                        if let Some(pos) =
                            self.plugin_granted_ids.iter().position(|id| *id == plugin_id)
                        {
                            self.plugin_granted_ids.remove(pos);
                            self.plugin_grants_summary.remove(pos);
                        }
                    }
                    Err(e) => {
                        self.plugin_revoke_result = Some(format!("Error: {e}"));
                    }
                }
            }

            // ADR-43 — Skills & MCP. Master toggles and allow-list edits arm
            // the dirty flag (they persist on Save Settings). Discovery and
            // probe results are transient view state and never arm it.
            Message::SkillsEnabledToggled(enabled) => {
                self.settings_dirty = true;
                self.skills_enabled = enabled;
            }
            Message::SkillTogglePressed(id, on) => {
                self.settings_dirty = true;
                // Checking a skill implies intent to use skills: arm the
                // master toggle as well.
                if on {
                    self.skills_enabled = true;
                    self.ensure_skills_allow_list_materialized();
                    if !self.skills_enabled_ids.contains(&id) {
                        self.skills_enabled_ids.push(id);
                    }
                } else {
                    // Unchecking is only meaningful against an explicit
                    // allow-list: materialize it from the discovered set first
                    // so the remaining rows keep reflecting reality. An empty
                    // allow-list intentionally keeps `skills.enabled = true`
                    // (the user can re-check individual skills).
                    self.ensure_skills_allow_list_materialized();
                    self.skills_enabled_ids.retain(|existing| *existing != id);
                }
            }
            Message::SkillExpandToggled(id) => {
                // Transient view state only: expanding a preview must not arm
                // the dirty flag (nothing here persists).
                if !self.skills_expanded.remove(&id) {
                    self.skills_expanded.insert(id);
                }
            }
            Message::SkillsDiscoveryRequested => {
                return self.start_skill_discovery();
            }
            Message::SkillsDiscoveryResult(result) => {
                self.skills_loading = false;
                self.skills_loaded = true;
                match result {
                    Ok(skills) => {
                        self.skills_discovered = skills;
                        self.skills_error = None;
                    }
                    Err(error) => self.skills_error = Some(error),
                }
            }
            Message::McpEnabledToggled(enabled) => {
                self.settings_dirty = true;
                self.mcp_enabled = enabled;
            }
            Message::McpServerEnabledToggled(id, enabled) => {
                self.settings_dirty = true;
                if let Some(server) = self.mcp_servers.iter_mut().find(|server| server.id == id) {
                    server.enabled = enabled;
                }
            }
            Message::McpProbePressed(id) => {
                let Some(server) = self.mcp_servers.iter().find(|server| server.id == id).cloned()
                else {
                    return iced::Task::none();
                };
                self.mcp_probing.insert(id.clone());
                return iced::Task::perform(
                    super::helpers::probe_mcp_server(server),
                    move |result| Message::McpProbeResult(id.clone(), result),
                );
            }
            Message::McpProbeResult(id, result) => {
                self.mcp_probing.remove(&id);
                self.mcp_probe_results.insert(id, result);
            }

            // Shell messages are delegated to handle_shell_message in shell.rs.
            // They modify settings state and only persist on Save Settings.
            other => {
                self.settings_dirty = true;
                return self.handle_shell_message(other);
            }
        }
        iced::Task::none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal skill descriptor for the toggle-semantics tests.
    fn skill(id: &str) -> SkillDescriptor {
        SkillDescriptor {
            id: id.to_string(),
            manifest: concerto_api_types::extension::SkillManifest {
                id: id.to_string(),
                name: id.to_string(),
                version: "1.0.0".to_string(),
                description: "test skill".to_string(),
                instructions_path: None,
                instructions: Some("do the thing".to_string()),
                tools: vec!["cargo test".to_string()],
                resources: Vec::new(),
            },
            instructions: "do the thing".to_string(),
            resource_paths: Vec::new(),
        }
    }

    fn server(id: &str) -> McpServerConfig {
        McpServerConfig {
            id: id.to_string(),
            command: "npx".to_string(),
            args: vec!["-y".to_string(), format!("@example/{id}")],
            env: None,
            enabled: true,
            timeout_secs: None,
        }
    }

    // ── ADR-43 — Skills toggle semantics ──────────────────────────────────
    //
    // `skills.enabled_ids` is `None` (allow-all: every discovered skill is a
    // candidate) until the user edits an individual checkbox. The first
    // explicit check materializes the allow-list from the discovered set,
    // flips `skills_allow_all = false`, and arms the master toggle. An empty
    // allow-list after edits stays explicit (nothing enabled), so the user
    // can re-check individual skills without silently reverting to
    // allow-all.

    #[test]
    fn fresh_state_starts_in_allow_all_mode() {
        let state = State::from_config(&AppConfig::default());
        assert!(!state.skills_enabled, "skills are off by default");
        assert!(state.skills_allow_all, "enabled_ids None means allow-all");
        assert!(state.skills_enabled_ids.is_empty());
        assert!(!state.skills_loaded, "discovery has not run yet");
    }

    #[test]
    fn explicit_check_flips_out_of_allow_all_and_arms_master() {
        let mut state = State::from_config(&AppConfig::default());
        let _ = state.update(Message::SkillTogglePressed("rust-testing".into(), true));

        assert!(state.skills_enabled, "checking a skill must arm the master toggle");
        assert!(!state.skills_allow_all, "first explicit check must leave allow-all");
        assert_eq!(state.skills_enabled_ids, vec!["rust-testing".to_string()]);
        assert!(state.settings_dirty, "allow-list edits must arm the dirty flag");
    }

    #[test]
    fn unchecked_skill_is_removed_from_allow_list() {
        let mut state = State::from_config(&AppConfig::default());
        state.skills_allow_all = false;
        state.skills_enabled_ids = vec!["a".into(), "b".into()];
        state.skills_enabled = true;

        let _ = state.update(Message::SkillTogglePressed("a".into(), false));

        assert_eq!(state.skills_enabled_ids, vec!["b".to_string()]);
        assert!(
            !state.skills_allow_all,
            "an empty allow-list must stay explicit, not revert to allow-all"
        );
        assert!(state.skills_enabled, "the master toggle is unaffected by unchecking");
    }

    #[test]
    fn unchecking_in_allow_all_mode_materializes_then_removes() {
        // Unchecking is only meaningful against an explicit allow-list: it
        // materializes the list from the discovered set first, then removes
        // the id, so the remaining rows keep reflecting reality.
        let mut state = State::from_config(&AppConfig::default());
        state.skills_discovered = vec![skill("a"), skill("b")];
        assert!(state.skills_allow_all);

        let _ = state.update(Message::SkillTogglePressed("a".into(), false));

        assert!(!state.skills_allow_all, "unchecking must materialize the allow-list");
        assert_eq!(state.skills_enabled_ids, vec!["b".to_string()]);
    }

    #[test]
    fn empty_allow_list_after_edits_stays_explicit() {
        let mut state = State::from_config(&AppConfig::default());
        state.skills_discovered = vec![skill("a")];
        let _ = state.update(Message::SkillTogglePressed("a".into(), true));
        assert!(!state.skills_allow_all);
        assert_eq!(state.skills_enabled_ids, vec!["a".to_string()]);

        let _ = state.update(Message::SkillTogglePressed("a".into(), false));

        assert!(!state.skills_allow_all, "empty list keeps allow_all=false (nothing enabled)");
        assert!(state.skills_enabled_ids.is_empty());
        assert!(state.skills_enabled, "master toggle stays on; the user can re-check");
    }

    #[test]
    fn skills_toggle_semantics_persist_through_config_round_trip() {
        let base = AppConfig::default();
        let mut state = State::from_config(&base);
        let _ = state.update(Message::SkillTogglePressed("rust-testing".into(), true));

        let saved = state.to_config(&base);
        let skills = saved.skills.expect("skills section must be published");
        assert!(skills.enabled);
        assert_eq!(skills.enabled_ids.as_deref(), Some(&["rust-testing".to_string()][..]));

        // Allow-all mode round-trips as None.
        let mut allow_all = State::from_config(&base);
        allow_all.skills_enabled = true;
        let saved = allow_all.to_config(&base);
        let skills = saved.skills.expect("skills section must be published");
        assert!(skills.enabled);
        assert_eq!(skills.enabled_ids, None, "untouched allow-list stays None");
    }

    #[test]
    fn skills_discovery_result_updates_state() {
        let mut state = State::from_config(&AppConfig::default());
        state.skills_loading = true;
        let discovered = vec![skill("rust-testing")];

        let _ = state.update(Message::SkillsDiscoveryResult(Ok(discovered.clone())));

        assert!(!state.skills_loading, "loading flag must clear after a result");
        assert!(state.skills_loaded);
        assert!(state.skills_error.is_none());
        assert_eq!(state.skills_discovered, discovered);

        // An error records the message without touching discovered skills.
        let _ = state.update(Message::SkillsDiscoveryResult(Err("boom".into())));
        assert!(state.skills_loaded);
        assert_eq!(state.skills_error.as_deref(), Some("boom"));
        assert!(
            !state.settings_dirty,
            "discovery results are transient and must not arm the dirty flag"
        );
    }

    #[test]
    fn discovery_request_schedules_one_task_and_is_idempotent() {
        let mut state = State::from_config(&AppConfig::default());
        assert!(state.skills_never_discovered());

        let task = state.update(Message::SkillsDiscoveryRequested);
        assert_eq!(task.units(), 1, "a discovery pass must be scheduled");
        assert!(state.skills_loading);
        assert!(!state.skills_never_discovered());

        // A second request while a run is in flight is a no-op.
        let task = state.update(Message::SkillsDiscoveryRequested);
        assert_eq!(task.units(), 0, "concurrent discovery must be a no-op");
        assert!(state.skills_loading);
    }

    #[test]
    fn skill_expand_toggle_is_transient_view_state() {
        let mut state = State::from_config(&AppConfig::default());
        let _ = state.update(Message::SkillExpandToggled("rust-testing".into()));
        assert!(state.skills_expanded.contains("rust-testing"));
        assert!(!state.settings_dirty, "expanding a preview must not arm the dirty flag");

        let _ = state.update(Message::SkillExpandToggled("rust-testing".into()));
        assert!(!state.skills_expanded.contains("rust-testing"));
    }

    // ── ADR-43 — MCP config and probe state ───────────────────────────────

    #[test]
    fn mcp_server_toggle_mutates_pending_config() {
        let base = AppConfig {
            mcp: Some(McpConfig { enabled: true, servers: vec![server("files")] }),
            ..AppConfig::default()
        };
        let mut state = State::from_config(&base);
        assert!(state.mcp_enabled);
        assert!(state.mcp_servers[0].enabled);

        let _ = state.update(Message::McpServerEnabledToggled("files".into(), false));

        assert!(!state.mcp_servers[0].enabled);
        assert!(state.settings_dirty, "a server toggle must arm the dirty flag");

        let saved = state.to_config(&base);
        let saved_mcp = saved.mcp.expect("mcp section must be published");
        assert!(saved_mcp.enabled);
        assert_eq!(saved_mcp.servers.len(), 1);
        assert!(!saved_mcp.servers[0].enabled, "pending config must reflect the toggle");
    }

    #[test]
    fn mcp_master_toggle_round_trips_through_config() {
        let base = AppConfig::default();
        let mut state = State::from_config(&base);
        assert!(!state.mcp_enabled);

        let _ = state.update(Message::McpEnabledToggled(true));

        let saved = state.to_config(&base);
        let saved_mcp = saved.mcp.expect("mcp section must be published");
        assert!(saved_mcp.enabled);
        assert!(saved_mcp.servers.is_empty());
    }

    #[test]
    fn mcp_probe_result_updates_probe_map_without_dirtying() {
        let mut state = State::from_config(&AppConfig::default());
        state.mcp_probing.insert("files".into());
        let tool = McpToolDescriptor {
            name: "read_file".into(),
            description: Some("Read a file".into()),
            input_schema: serde_json::Value::Null,
        };

        let _ = state.update(Message::McpProbeResult("files".into(), Ok(vec![tool.clone()])));

        assert!(!state.mcp_probing.contains("files"), "probing flag must clear");
        match state.mcp_probe_results.get("files") {
            Some(Ok(tools)) => assert_eq!(tools, &vec![tool]),
            other => panic!("expected Ok tool list, got {other:?}"),
        }
        assert!(
            !state.settings_dirty,
            "probe results are transient and must not arm the dirty flag"
        );

        let _ = state.update(Message::McpProbeResult("files".into(), Err("boom".into())));
        assert!(
            matches!(state.mcp_probe_results.get("files"), Some(Err(e)) if e == "boom"),
            "a later error result must replace the earlier success"
        );
    }

    #[test]
    fn mcp_probe_pressed_marks_probing_and_schedules_task() {
        let base = AppConfig {
            mcp: Some(McpConfig { enabled: true, servers: vec![server("files")] }),
            ..AppConfig::default()
        };
        let mut state = State::from_config(&base);

        let task = state.update(Message::McpProbePressed("files".into()));
        assert_eq!(task.units(), 1, "a probe must be scheduled");
        assert!(state.mcp_probing.contains("files"));

        // Unknown server ids are a no-op.
        let mut state = State::from_config(&base);
        let task = state.update(Message::McpProbePressed("nope".into()));
        assert_eq!(task.units(), 0);
        assert!(state.mcp_probing.is_empty());
    }

    // ── ADR-57 — cache-only refresh ──────────────────────────────────────────
    //
    // `refresh_provider_cache_from_config` is called on every config reload so
    // provider pickers and Studio model lists stay fresh. It must never rebuild
    // the Settings form rows or arm the dirty flag (unsaved edits win on the
    // next explicit save).

    #[test]
    fn refresh_provider_cache_from_config_updates_caches_but_not_the_form() {
        fn provider(id: &str, kind: &str, model: &str) -> ProviderConfig {
            ProviderConfig {
                id: id.to_string(),
                name: kind.to_string(),
                provider: kind.to_string(),
                model: model.to_string(),
                keyring_key: format!("{kind}/api_key"),
                ..ProviderConfig::default()
            }
        }
        let base = AppConfig {
            model_settings: Some(concerto_config::ModelSettings {
                providers: vec![
                    provider("router", "openrouter", "anthropic/claude-sonnet-4"),
                    provider("code", "opencode", "opencode/deepseek-v4-flash-free"),
                ],
                ..concerto_config::ModelSettings::default()
            }),
            ..AppConfig::default()
        };
        let mut state = State::from_config(&base);
        assert!(!state.providers.is_empty(), "from_config must seed form rows");

        // An external config edit adds a provider; in-flight form edits must
        // survive the cache refresh untouched.
        let edited = AppConfig {
            model_settings: Some(concerto_config::ModelSettings {
                providers: vec![
                    provider("router", "openrouter", "anthropic/claude-sonnet-4"),
                    provider("code", "opencode", "opencode/deepseek-v4-flash-free"),
                    provider("local", "ollama", "llama3.1:8b"),
                ],
                ..concerto_config::ModelSettings::default()
            }),
            ..AppConfig::default()
        };

        state.refresh_provider_cache_from_config(&edited);

        assert_eq!(
            state.cached_provider_ids,
            vec!["router".to_string(), "code".to_string(), "local".to_string()],
            "cache ids must pick up the new provider"
        );
        assert!(
            state.model_names_for_provider("local").contains(&"llama3.1:8b".to_string()),
            "cache model lists must pick up the new provider's model"
        );
        assert!(
            !state.providers.iter().any(|p| p.id == "local"),
            "form provider rows must never be rebuilt by a cache refresh"
        );
        assert!(!state.settings_dirty, "a cache refresh must not arm the dirty flag");
    }
}
