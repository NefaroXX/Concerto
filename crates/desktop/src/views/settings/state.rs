use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;

use concerto_api_types::extension::{McpToolDescriptor, SkillDescriptor};
use concerto_config::managed::ManagedRuntimeManager;
use concerto_config::shell::ShellProfileConfig;
use concerto_config::{
    AgentRelationshipConfig, AppConfig, ConditionDef, ManagedEnvConfig, McpConfig, McpServerConfig,
    PolicyConfig, PolicyRuleDef, ProjectContextConfig, ProviderConfig, ShellSettings, SkillsConfig,
};
use concerto_providers::provider_defs::{
    picker_model_options, provider_definition, PROVIDER_TYPE_IDS,
};

use crate::theme::AppTheme;

use super::helpers::default_managed_source;
use super::message::SectionId;
use super::{
    readable_provider_label, CreateParentOption, ExtensionTab, Message, PolicyActionChoice,
    PolicyConditionChoice, CUSTOM_MODEL_SENTINEL, FILESYSTEM_OPERATIONS, MAIN_SCROLL_ID,
    POLICY_ACTIONS, POLICY_CONDITION_KINDS, POLICY_OPERATION_TOOLS, POLICY_TOOLS,
};

/// A plugin installed in the canonical plugins directory, as listed by the
/// Settings → Plugins tab. Transient view state: never persisted and never
/// arms the dirty flag.
///
/// `load_error` carries a warning when the file could not be loaded (e.g. a
/// hand-dropped malformed module) so even unreadable plugins stay visible in
/// the list and deletable; `id` then falls back to the file stem.
#[derive(Debug, Clone)]
pub struct InstalledPluginInfo {
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub provides: String,
    pub capability_summary: String,
    pub wasm_path: PathBuf,
    pub load_error: Option<String>,
}

pub struct State {
    // Theme / display
    pub theme_names: Vec<&'static str>,
    pub selected_theme: &'static str,
    pub font_size: f32,
    /// Reduced-motion override (`display.reduced_motion`).
    pub reduced_motion: bool,
    /// Scan-line overlay flag (`display.scanline_overlay_enabled`).
    pub scanline_overlay_enabled: bool,

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
    /// Providers with a model-list refresh currently in flight; their Refresh
    /// control renders inert ("Refreshing…") until the result arrives. View
    /// mirror of the App's `pending_refresh` request bookkeeping.
    pub refreshing_providers: HashSet<String>,
    /// Last model-discovery failure per provider id, shown inline next to the
    /// Refresh control until a later successful refresh clears it.
    pub provider_refresh_errors: HashMap<String, String>,

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
    /// Process-lifetime plugin-manager handle supplied by the desktop App
    /// (plugin liveness). `None` headless/tests: revoke falls back to the
    /// fresh best-effort manager, tolerating `NotActive`.
    pub plugin_manager: Option<concerto_plugins::manager::SharedPluginManager>,

    // ADR-37 — Plugin install / remove (Settings → Plugins)
    /// Installer approval bridge attached by the App at startup
    /// ([`Self::with_plugin_approval`]); drives the same capability dialog the
    /// runtime uses, so install-time prompts are answered before any file is
    /// written. `None` headless/tests.
    pub plugin_approval: Option<crate::services::plugin_approval::PluginApprovalService>,
    /// Plugins currently installed in the canonical plugins directory, folded
    /// with their capability-grant summaries. Transient view state.
    pub installed_plugins: Vec<InstalledPluginInfo>,
    /// True once `installed_plugins` has been seeded (success or failure);
    /// triggers the one lazy re-scan when the Plugins tab is first opened.
    pub plugins_loaded: bool,
    /// True while the directory re-scan is in flight (single-flight).
    pub plugins_loading: bool,
    /// Typed source path for the install card (also set by the file picker).
    pub plugin_install_path: String,
    /// Transient outcome line of the last install/replace/delete action.
    pub plugin_install_result: Option<String>,
    /// Single-flight gate for install/replace/delete tasks: the buttons render
    /// inert while a task is in flight so the user cannot queue competing
    /// plugin writes.
    pub plugin_action_busy: bool,
    /// True while the native file picker is open (install or replace).
    pub plugin_picker_busy: bool,
    /// Id whose delete-confirm prompt is armed, if any.
    pub plugin_delete_confirm: Option<String>,

    // ADR-43 — Skills configuration
    /// Master skills toggle (`skills.enabled`).
    pub skills_enabled: bool,
    /// Search paths for skill packs (display + discovery).
    pub skills_search_paths: Vec<String>,
    /// Whether auto-load of discovered skills is enabled (display only in v1).
    pub skills_auto_load: bool,
    /// Per-skill prompt budget (`skills.max_chars`); display-only in v1 and
    /// preserved verbatim on save.
    pub skills_max_chars: Option<usize>,
    /// Discovered skill packs, populated lazily when the page opens.
    pub skills_discovered: Vec<SkillDescriptor>,
    /// True once a discovery run has completed (success or failure).
    pub skills_loaded: bool,
    /// True while a discovery run is in flight.
    pub skills_loading: bool,
    /// Human-readable discovery failure, when the last run failed.
    pub skills_error: Option<String>,
    /// Per-path/per-pack diagnostics from the last successful discovery run
    /// (`resolved_paths`/`warnings`) — e.g. a configured search path that is
    /// missing, or a pack skipped for a malformed manifest. Rendered under the
    /// search-paths row so a "no skills found" result is explainable.
    pub skills_warnings: Vec<String>,
    /// Informational notes from the last successful discovery run (e.g. a pack
    /// whose manifest id was empty loaded under its directory name). These are
    /// not failures — the settings view renders them as muted info lines under
    /// the search-paths row. Deduplicated like [`Self::skills_warnings`].
    pub skills_notes: Vec<String>,
    /// True = all discovered skills are candidates (`enabled_ids: None`);
    /// false = `skills_enabled_ids` is the explicit allow-list.
    pub skills_allow_all: bool,
    /// Explicit allow-list of enabled skill ids (`skills.enabled_ids`).
    pub skills_enabled_ids: Vec<String>,
    /// Ids of discovered skills whose instruction preview is expanded.
    /// Transient view state; never persisted and never arms the dirty flag.
    pub skills_expanded: HashSet<String>,

    // ADR-43 — skill pack CRUD (transient wizard / confirm state)
    /// True while the skill-create wizard is open in the detail pane.
    pub skill_create_open: bool,
    /// New-pack id draft (`Id` field).
    pub skill_create_id: String,
    /// New-pack display-name draft.
    pub skill_create_name: String,
    /// New-pack version draft.
    pub skill_create_version: String,
    /// New-pack description draft.
    pub skill_create_description: String,
    /// New-pack inline instructions body.
    pub skill_create_instructions: iced::widget::text_editor::Content,
    /// New-pack parent directory (one of the configured search paths).
    pub skill_create_parent: Option<CreateParentOption>,
    /// Inline validation error for the id field.
    pub skill_create_id_error: Option<String>,
    /// Inline validation error for the parent field.
    pub skill_create_parent_error: Option<String>,
    /// True while a create/edit/delete task is in flight; gates the buttons so
    /// the user cannot queue competing pack writes.
    pub skill_crud_busy: bool,
    /// Outcome line of the last create/edit/delete ("Saved…" / "Error: …").
    pub skill_crud_result: Option<String>,
    /// Id of the skill currently in edit mode (`None` = read-only detail).
    pub skill_editing_id: Option<String>,
    /// In-progress edit draft for `skill_editing_id`.
    pub skill_edit_draft: Option<SkillEditDraft>,
    /// Id of the skill whose delete-confirm prompt is open.
    pub skill_delete_confirm: Option<String>,

    // ADR-43 — MCP configuration
    /// Master MCP toggle (`mcp.enabled`).
    pub mcp_enabled: bool,
    /// Configured MCP servers (editable in v1: per-server enabled flag,
    /// add, edit, and delete; all persisted on Save Settings).
    pub mcp_servers: Vec<McpServerConfig>,
    /// Probe results keyed by server id. `Ok` = tool list, `Err` = error text.
    pub mcp_probe_results: HashMap<String, Result<Vec<McpToolDescriptor>, String>>,
    /// Server ids currently being probed.
    pub mcp_probing: HashSet<String>,
    /// Id of the MCP server currently in edit mode; `None` = read-only
    /// detail pane. Transient: entering edit mode never arms the dirty flag.
    pub mcp_editing_id: Option<String>,
    /// Draft fields for the in-progress MCP server edit. Present iff
    /// [`Self::mcp_editing_id`] is set.
    pub mcp_edit_draft: Option<McpEditDraft>,
    /// Id of the MCP server whose deletion is awaiting confirmation.
    /// Transient view state; the actual removal only happens on
    /// [`Message::McpDeleteConfirmed`].
    pub mcp_delete_confirm: Option<String>,
    /// Draft fields for the in-progress MCP server add form. `Some` = the add
    /// form is open in the detail pane. Transient view state: entering the
    /// form never arms the dirty flag, and the new server only lands in
    /// [`Self::mcp_servers`] (and eventually the config) on
    /// [`Message::McpAddSaved`].
    pub mcp_add_draft: Option<McpAddDraft>,

    // Unified Extensions manager (ADR-37/43/70) — master-detail view state.
    /// Active sub-tab of the Extensions section. Transient; never persisted.
    pub active_extension_tab: ExtensionTab,
    /// Selected skill id in the Skills tab (transient view state).
    pub ext_selected_skill: Option<String>,
    /// Selected MCP server id in the MCP tab (transient view state).
    pub ext_selected_mcp: Option<String>,
    /// Selected plugin id in the Plugins tab (transient view state).
    pub ext_selected_plugin: Option<String>,

    // ADR-70 — project AGENTS.md context injection
    /// Master toggle (`project_context.enabled`); opt-in, defaults off.
    pub project_context_enabled: bool,
    /// Coordinator advisory nudge (`project_context.auto_update_agents_md`).
    pub project_context_auto_update_agents_md: bool,
    /// Nudge cadence in dispatches (`project_context.update_frequency`).
    /// Display-only in this release.
    pub project_context_update_frequency: u64,
    /// Per-source character budget (`project_context.max_bytes`).
    /// Display-only in this release.
    pub project_context_max_bytes: Option<usize>,
    /// Path to the user-global AGENTS.md (`project_context.global_path`).
    /// Display-only in this release.
    pub project_context_global_path: Option<String>,
    /// True once the user edits the project-context block. A plain Settings
    /// save/reload must neither publish the section from this snapshot nor
    /// clobber a newer externally-written section (mirrors `relationship_dirty`).
    pub project_context_dirty: bool,
}

/// Draft fields for an in-progress MCP server edit (ADR-43 edit/delete).
///
/// Seeded from the server's config on [`Message::McpEditPressed`] and applied
/// back on [`Message::McpEditSaved`]. The `id` is intentionally not
/// editable: it is the stable key namespacing tools as
/// `mcp:<server_id>:<tool_name>`, so the config loader's id validation
/// (non-empty, no `:`, unique — `McpConfig::validate`) is reused unchanged.
/// Per-field `*_error` hold inline validation messages; `None` = valid.
#[derive(Debug, Clone)]
pub struct McpEditDraft {
    /// Executable to spawn.
    pub command: String,
    /// Arguments joined with spaces (split back on save).
    pub args: String,
    /// Environment variables as key → value rows, rendered from the map's key
    /// order (same editor pattern as the shell profiles). An empty map saves
    /// as `env = none`.
    pub env: BTreeMap<String, String>,
    /// Per-call timeout in seconds; blank = crate default (60s).
    pub timeout: String,
    /// Inline error for the command field.
    pub command_error: Option<String>,
    /// Inline error for the environment rows (e.g. an empty key).
    pub env_error: Option<String>,
    /// Inline error for the timeout field.
    pub timeout_error: Option<String>,
}

/// Draft fields for an in-progress MCP server add (ADR-43 add).
///
/// Mirrors [`McpEditDraft`] with the `id` editable — a brand-new server needs
/// a key for its tool namespace `mcp:<server_id>:<tool_name>` — so validation
/// mirrors `McpConfig::validate` (non-empty, no `:`, unique) plus the edit
/// draft's command/env/timeout rules. Seeded blank on
/// [`Message::McpAddPressed`]; on [`Message::McpAddSaved`] a valid draft is
/// pushed into `mcp_servers` as a new [`McpServerConfig`], while an invalid
/// draft keeps the form open with inline errors. Per-field `*_error` hold
/// inline validation messages; `None` = valid.
#[derive(Debug, Clone)]
pub struct McpAddDraft {
    /// Unique id, used to namespace tools as `mcp:<server_id>:<tool_name>`.
    pub id: String,
    /// Executable to spawn.
    pub command: String,
    /// Arguments joined with spaces (split back on save).
    pub args: String,
    /// Environment variables as key → value rows, rendered from the map's key
    /// order (same editor pattern as the shell profiles). An empty map saves
    /// as `env = none`.
    pub env: BTreeMap<String, String>,
    /// Per-call timeout in seconds; blank = crate default (60s).
    pub timeout: String,
    /// Inline error for the id field.
    pub id_error: Option<String>,
    /// Inline error for the command field.
    pub command_error: Option<String>,
    /// Inline error for the environment rows (e.g. an empty key).
    pub env_error: Option<String>,
    /// Inline error for the timeout field.
    pub timeout_error: Option<String>,
}

/// In-progress edit of a discovered skill pack's `skill.toml` (ADR-43).
/// Seeded by [`Message::SkillEditPressed`] from the discovered descriptor and
/// written back by [`Message::SkillEditSaved`]. The pack `id` is not
/// editable — it is the pack directory name (dotfile registration adds
/// `skills.<id>` to `enabled_ids`) — so only `name`, `description`, and the
/// inline `instructions` body are drafted; `version`, `tools`, and `resources`
/// are preserved unchanged because nothing in the v1 editor manages them.
///
/// The instructions body is an iced `text_editor::Content`, which is not
/// `Clone`; the draft is owned by the state and never derived.
pub struct SkillEditDraft {
    /// Display name; blank renders as the id.
    pub name: String,
    /// Short description shown in the detail pane.
    pub description: String,
    /// Inline instruction body (replaces `instructions_path` on save).
    pub instructions: iced::widget::text_editor::Content,
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
        let project_context = config.project_context.clone().unwrap_or_default();

        let mut state = Self {
            theme_names,
            selected_theme: "Midnight",
            font_size: 14.0,
            reduced_motion: config.display.reduced_motion,
            scanline_overlay_enabled: config.display.scanline_overlay_enabled,
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
            refreshing_providers: HashSet::new(),
            provider_refresh_errors: HashMap::new(),
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
            // Every section starts folded (ADR-57 §3d UX): the sidebar index is
            // the navigation surface, and clicking an entry expands its section
            // in place via `Message::JumpToSection`.
            collapsed_sections: SectionId::ALL.iter().copied().collect(),
            plugin_granted_ids: Vec::new(),
            plugin_grants_summary: Vec::new(),
            plugin_revoke_result: None,
            plugin_manager: None,
            plugin_approval: None,
            installed_plugins: Vec::new(),
            plugins_loaded: false,
            plugins_loading: false,
            plugin_install_path: String::new(),
            plugin_install_result: None,
            plugin_action_busy: false,
            plugin_picker_busy: false,
            plugin_delete_confirm: None,
            skills_enabled: skills.enabled,
            skills_search_paths: skills.search_paths.clone(),
            skills_auto_load: skills.auto_load,
            skills_max_chars: skills.max_chars,
            skills_discovered: Vec::new(),
            skills_loaded: false,
            skills_loading: false,
            skills_error: None,
            skills_warnings: Vec::new(),
            skills_notes: Vec::new(),
            skills_allow_all: skills.enabled_ids.is_none(),
            skills_enabled_ids: skills.enabled_ids.clone().unwrap_or_default(),
            skills_expanded: HashSet::new(),
            // Transient skill-pack CRUD state (ADR-43). All of it is
            // wizard/confirm view state: nothing here persists directly and
            // nothing here arms `settings_dirty`.
            skill_create_open: false,
            skill_create_id: String::new(),
            skill_create_name: String::new(),
            skill_create_version: "0.1.0".to_string(),
            skill_create_description: String::new(),
            skill_create_instructions: iced::widget::text_editor::Content::new(),
            skill_create_parent: None,
            skill_create_id_error: None,
            skill_create_parent_error: None,
            skill_crud_busy: false,
            skill_crud_result: None,
            skill_editing_id: None,
            skill_edit_draft: None,
            skill_delete_confirm: None,
            mcp_enabled: mcp.enabled,
            mcp_servers: mcp.servers.clone(),
            mcp_probe_results: HashMap::new(),
            mcp_probing: HashSet::new(),
            mcp_editing_id: None,
            mcp_edit_draft: None,
            mcp_delete_confirm: None,
            mcp_add_draft: None,
            active_extension_tab: ExtensionTab::Skills,
            ext_selected_skill: None,
            ext_selected_mcp: mcp.servers.first().map(|server| server.id.clone()),
            ext_selected_plugin: None,
            project_context_enabled: project_context.enabled,
            project_context_auto_update_agents_md: project_context.auto_update_agents_md,
            project_context_update_frequency: project_context.update_frequency,
            project_context_max_bytes: project_context.max_bytes,
            project_context_global_path: project_context.global_path.clone(),
            project_context_dirty: false,
        };
        let shell = config.resolved_shell_settings();
        state.shell_active_profile = shell.selected_profile_id().to_owned();
        state.shell_profiles = shell.profiles;
        state.selected_shell_profile = None;
        state.shell_new_env_key = String::new();
        state.shell_new_env_value = String::new();
        state.load_plugin_grants();
        // Default the Plugins detail pane to the first granted plugin so the
        // pane never opens empty when grants exist.
        state.ext_selected_plugin = state.plugin_granted_ids.first().cloned();
        // Default the skill-create wizard's parent picker to the first
        // configured search path so a newly opened wizard is fully populated
        // (mirrors the plugin leading-selection seeding above).
        state.skill_create_parent = state.create_parent_options().into_iter().next();
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
        // Drop refresh markers/errors whose provider row no longer exists so
        // they can neither leak nor resurface on a recycled id.
        self.refreshing_providers.retain(|id| self.providers.iter().any(|p| &p.id == id));
        self.provider_refresh_errors.retain(|id, _| self.providers.iter().any(|p| p.id == *id));
    }

    /// Refresh the Display motion toggles from the live merged config.
    ///
    /// Same ownership rule as [`Self::sync_providers_from_config`] (ADR-57
    /// §3d): once the user edits anything (`settings_dirty`), the form owns
    /// the toggles until the next explicit save.
    pub fn sync_display_from_config(&mut self, config: &AppConfig) {
        if self.settings_dirty {
            return;
        }
        self.reduced_motion = config.display.reduced_motion;
        self.scanline_overlay_enabled = config.display.scanline_overlay_enabled;
    }

    /// Refresh the project-context block from the live merged config.
    ///
    /// Same ownership rule as [`Self::sync_providers_from_config`] (ADR-57
    /// §3d), scoped to the block the user actually edits: once the user toggles
    /// anything on this tab (`project_context_dirty`), the form owns the block
    /// until the next explicit save.
    pub fn sync_project_context_from_config(&mut self, config: &AppConfig) {
        if self.project_context_dirty {
            return;
        }
        let project_context = config.project_context.clone().unwrap_or_default();
        self.project_context_enabled = project_context.enabled;
        self.project_context_auto_update_agents_md = project_context.auto_update_agents_md;
        self.project_context_update_frequency = project_context.update_frequency;
        self.project_context_max_bytes = project_context.max_bytes;
        self.project_context_global_path = project_context.global_path.clone();
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
        cfg.display.reduced_motion = self.reduced_motion;
        cfg.display.scanline_overlay_enabled = self.scanline_overlay_enabled;

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
            max_chars: self.skills_max_chars,
        });
        cfg.mcp = Some(McpConfig { enabled: self.mcp_enabled, servers: self.mcp_servers.clone() });

        // ADR-70 — project context. Published only after an explicit edit here
        // (`project_context_dirty`); otherwise the section is left untouched so
        // an absent section stays absent and any external project-scoped edit
        // survives the save.
        if self.project_context_dirty {
            cfg.project_context = Some(ProjectContextConfig {
                enabled: self.project_context_enabled,
                global_path: self.project_context_global_path.clone(),
                max_bytes: self.project_context_max_bytes,
                auto_update_agents_md: self.project_context_auto_update_agents_md,
                update_frequency: self.project_context_update_frequency,
            });
        }
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

    /// Validate the timeout field of the MCP edit draft. Blank is valid (the
    /// crate default is 60s); otherwise the value must be a whole number in
    /// `1..=300` — the hard cap the MCP bridge enforces.
    fn validate_mcp_timeout(s: &str) -> Option<String> {
        let trimmed = s.trim();
        if trimmed.is_empty() {
            return None; // blank is valid (means "use the 60s default")
        }
        match trimmed.parse::<u64>() {
            Ok(0) => Some("Must be a positive number".into()),
            Ok(v) if v > 300 => Some("Hard cap is 300 seconds".into()),
            Ok(_) => None,
            Err(_) => Some("Must be a whole number".into()),
        }
    }

    /// Validate the id of an MCP add draft against the configured servers.
    /// Mirrors `McpConfig::validate` (ADR-43 §4): non-empty, no `:` — tools
    /// are namespaced `mcp:<server_id>:<tool_name>` — and unique.
    fn mcp_add_id_error(mcp_servers: &[McpServerConfig], id: &str) -> Option<String> {
        let trimmed = id.trim();
        if trimmed.is_empty() {
            return Some("Id is required".into());
        }
        if trimmed.contains(':') {
            return Some("Id must not contain ':'".into());
        }
        if mcp_servers.iter().any(|server| server.id == trimmed) {
            return Some("An MCP server with this id already exists".into());
        }
        None
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

    /// Model options for a provider: the shared picker resolver (selected /
    /// default / static known models, plus discovered `cached_models` and
    /// config-first `extra_models`) [ADR-57 §3d].
    fn model_options_with_discovered(p: &ProviderConfig) -> Vec<String> {
        picker_model_options(p)
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

    /// Attach the desktop's process-lifetime plugin-manager handle so the
    /// Settings revoke path can clear a LIVE plugin's in-memory grants
    /// (plugin liveness) instead of always hitting a fresh manager.
    pub fn with_plugin_manager(
        &mut self,
        plugin_manager: concerto_plugins::manager::SharedPluginManager,
    ) {
        self.plugin_manager = Some(plugin_manager);
    }

    /// Attach the capability-approval bridge so the plugin installer can
    /// prompt for capability grants before any file is written. The App passes
    /// the same shared queue its runtime capability dialog consumes, so the
    /// user answers install-time prompts in the familiar modal and
    /// `GrantedPersistent` decisions land in the same persisted store.
    pub fn with_plugin_approval(
        &mut self,
        pending: crate::widgets::capability_dialog::SharedPending,
    ) {
        self.plugin_approval =
            Some(crate::services::plugin_approval::PluginApprovalService::new(pending));
    }

    /// Whether an install/replace/delete task is in flight (gates the plugins
    /// tab's buttons).
    pub fn plugin_action_in_flight(&self) -> bool {
        self.plugin_action_busy
    }

    /// Re-scan the canonical plugins directory and fold capability-grant
    /// summaries into the installed list. Sets the loading flag and returns
    /// the task whose completion routes back as `PluginListRefreshResult`.
    /// Idempotent: a second call while a run is in flight is a no-op.
    pub fn start_plugin_list_refresh(&mut self) -> iced::Task<Message> {
        if self.plugins_loading {
            return iced::Task::none();
        }
        self.plugins_loading = true;
        iced::Task::perform(
            async move { super::helpers::list_installed_plugins().await },
            Message::PluginListRefreshResult,
        )
    }

    /// Start a plugin install/replace for `source` (typed path or picker
    /// result). Enforces the single-flight gate; returns the task to run, or
    /// `None` when already busy or the source is blank.
    fn start_plugin_install(&mut self, source: String) -> Option<iced::Task<Message>> {
        let trimmed = source.trim().to_string();
        if trimmed.is_empty() || self.plugin_action_busy {
            return None;
        }
        let approval = self.plugin_approval.clone();
        let manager = self.plugin_manager.clone();
        let store_dir = concerto_plugins::capability::CapabilityManager::data_dir();
        self.plugin_action_busy = true;
        Some(iced::Task::perform(
            async move {
                super::helpers::install_plugin(
                    std::path::PathBuf::from(trimmed),
                    store_dir,
                    manager,
                    approval,
                )
                .await
            },
            Message::PluginInstallResult,
        ))
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
        // Keep the Plugins-tab selection valid: if the selected plugin was
        // revoked (or the store changed underneath us), fall back to the
        // first granted id.
        if let Some(selected) = self.ext_selected_plugin.as_deref() {
            if !self.plugin_granted_ids.iter().any(|id| id == selected) {
                self.ext_selected_plugin = self.plugin_granted_ids.first().cloned();
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

    /// Mark a provider's model-list refresh as in flight. The row's Refresh
    /// control renders inert until the matching result message arrives.
    pub fn begin_provider_refresh(&mut self, provider_id: &str) {
        self.refreshing_providers.insert(provider_id.to_string());
    }

    /// Clear a provider's in-flight refresh marker. Idempotent.
    pub fn end_provider_refresh(&mut self, provider_id: &str) {
        self.refreshing_providers.remove(provider_id);
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

    /// Parent-directory options for the skill-create wizard, derived from the
    /// configured search paths. Each option shows the resolved absolute path
    /// with an existence badge (the same rendering used in the Search paths
    /// meta row), so the user picks an actual file path to write into.
    pub fn create_parent_options(&self) -> Vec<CreateParentOption> {
        self.skills_search_paths
            .iter()
            .map(|raw| CreateParentOption {
                raw: raw.clone(),
                label: super::resolved_path_label(raw, true),
            })
            .collect()
    }

    /// Validates a prospective new skill id against the same rules the skills
    /// crate enforces when a pack is created: a non-empty single path
    /// component after trimming (no `/`, `\`, `:`, or NUL; not `.`/`..`).
    /// Returns a human-readable inline error, or `None` when the id is
    /// acceptable. Mirrors `concerto-skills`' create/load id rules so a pack
    /// the wizard can create is never rejected by discovery later.
    fn skill_id_error(id: &str) -> Option<String> {
        let trimmed = id.trim();
        let invalid = trimmed.is_empty()
            || trimmed == "."
            || trimmed == ".."
            || trimmed.contains('/')
            || trimmed.contains('\\')
            || trimmed.contains(':')
            || trimmed.contains('\0');
        if invalid {
            Some(
                "Id must be a single path component (no slashes, backslashes, colons, or NUL)."
                    .to_string(),
            )
        } else {
            None
        }
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

    /// Scroll the Settings main column so `section`'s header lands at the top.
    ///
    /// Uses a fractional [`RelativeOffset`] derived from the section's position
    /// in the canonical [`SectionId::ALL`] order — a stable proxy for the
    /// rendered column, whose per-section heights vary. The `+ 1` denominator
    /// accounts for the trailing save footer and biases the jump slightly high,
    /// so the target section's expanded body (which grows downward) stays
    /// visible.
    fn scroll_to_section(section: SectionId) -> iced::Task<Message> {
        let Some(index) = SectionId::ALL.iter().position(|candidate| *candidate == section) else {
            return iced::Task::none();
        };
        let fraction = index as f32 / (SectionId::ALL.len() + 1) as f32;
        let offset = iced_core::widget::operation::scrollable::RelativeOffset {
            x: Some(0.0),
            y: Some(fraction),
        };
        iced::advanced::widget::operate(iced_core::widget::operation::scrollable::snap_to(
            iced::widget::Id::new(MAIN_SCROLL_ID),
            offset,
        ))
    }

    pub fn update(&mut self, message: Message) -> iced::Task<Message> {
        match message {
            Message::ThemeSelected(name) => self.selected_theme = name,
            Message::FontSizeChanged(size) => self.font_size = size.clamp(12.0, 20.0),
            Message::ReducedMotionToggled(reduced) => {
                self.settings_dirty = true;
                self.reduced_motion = reduced;
            }
            Message::ScanlineOverlayToggled(enabled) => {
                self.settings_dirty = true;
                self.scanline_overlay_enabled = enabled;
            }

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

            // Phase 3 — model discovery (startup auto-fetch + per-provider
            // Refresh button; the App layer spawns the fetch and forwards
            // the result here).
            Message::ProviderModelsRefreshRequested(provider_id) => {
                // Normally intercepted by the App layer before reaching this
                // update. Handled anyway so a misrouted message still flips
                // the row into its in-flight state instead of leaving a dead
                // button.
                self.begin_provider_refresh(&provider_id);
            }
            Message::ProviderModelsRefreshed { provider_id, request_id: _, result } => {
                self.end_provider_refresh(&provider_id);
                match result {
                    Ok(models) => {
                        if models.is_empty() {
                            // The providers crate collapses every discovery
                            // failure (network, auth, …) into an empty list.
                            // Keep the previous cache so one offline refresh
                            // cannot wipe the user's usable model list.
                            self.provider_refresh_errors.insert(
                                provider_id.clone(),
                                "Discovery returned no models — check credentials/network."
                                    .to_string(),
                            );
                        } else {
                            self.provider_refresh_errors.remove(&provider_id);
                            if let Some(p) = self.providers.iter_mut().find(|p| p.id == provider_id)
                            {
                                p.record_discovered_models(models);
                            }
                        }
                    }
                    Err(error) => {
                        self.provider_refresh_errors.insert(provider_id.clone(), error);
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
            // Sidebar navigation: always expand the target (never fold it) and
            // scroll the main column to its header.
            Message::JumpToSection(id) => {
                self.collapsed_sections.remove(&id);
                return Self::scroll_to_section(id);
            }

            // ADR-37 — Plugin grant lifecycle. Grants are persisted in the
            // capability store (not AppConfig), so revoke never touches
            // `settings_dirty`. The revoke runs off the UI thread inside
            // `Task::perform`; the callback refreshes the cached lists and
            // displays the outcome line.
            Message::PluginRevokePressed(plugin_id) => {
                return iced::Task::perform(
                    super::helpers::revoke_plugin_grants(plugin_id, self.plugin_manager.clone()),
                    Message::PluginRevokeResult,
                );
            }
            Message::PluginRevokeResult(result) => {
                self.plugin_revoke_result = Some(match &result {
                    Ok(message) => message.clone(),
                    Err(error) => format!("Error: {error}"),
                });
                if result.is_ok() {
                    // Persisted grants were removed; re-read the store so the
                    // cached lists reflect reality (including any other
                    // plugins affected by the same store write).
                    self.load_plugin_grants();
                }
            }

            // ADR-37 — Plugin install / remove. Transient view state and
            // capability-store writes only: nothing here arms `settings_dirty`.
            // The install/delete tasks run off the UI thread inside
            // `Task::perform`; the callbacks re-read grants and re-scan the
            // plugins directory.
            Message::PluginInstallPathChanged(value) => {
                self.plugin_install_path = value;
            }
            Message::PluginInstallBrowsePressed => {
                if self.plugin_picker_busy {
                    return iced::Task::none();
                }
                self.plugin_picker_busy = true;
                return iced::Task::perform(
                    super::helpers::pick_plugin_file(),
                    Message::PluginBrowsePicked,
                );
            }
            Message::PluginBrowsePicked(picked) => {
                self.plugin_picker_busy = false;
                if let Some(path) = picked {
                    self.plugin_install_path = path;
                }
                return iced::Task::none();
            }
            Message::PluginInstallPressed => {
                if let Some(task) = self.start_plugin_install(self.plugin_install_path.clone()) {
                    return task;
                }
                if self.plugin_install_path.trim().is_empty() {
                    self.plugin_install_result =
                        Some("Enter a .wasm path or use Browse….".to_string());
                }
                return iced::Task::none();
            }
            Message::PluginReplacePressed => {
                if self.plugin_picker_busy {
                    return iced::Task::none();
                }
                self.plugin_picker_busy = true;
                return iced::Task::perform(
                    super::helpers::pick_plugin_file(),
                    Message::PluginReplacePicked,
                );
            }
            Message::PluginReplacePicked(picked) => {
                self.plugin_picker_busy = false;
                return match picked {
                    // A picked replace source immediately starts the install
                    // pipeline (which detects the existing file and changes
                    // the grant flow to replace semantics).
                    Some(path) => {
                        self.plugin_install_path = path.clone();
                        self.start_plugin_install(path).unwrap_or_else(iced::Task::none)
                    }
                    None => iced::Task::none(),
                };
            }
            Message::PluginInstallResult(result) => {
                self.plugin_action_busy = false;
                self.plugin_install_result = Some(match &result {
                    Ok(message) => message.clone(),
                    Err(error) => format!("Error: {error}"),
                });
                if result.is_ok() {
                    // A successful install wrote a new file: re-read grants
                    // (hash pinning may have invalidated others) and re-scan
                    // so the list reflects reality.
                    self.load_plugin_grants();
                    self.plugin_install_path = String::new();
                    return self.start_plugin_list_refresh();
                }
                return iced::Task::none();
            }
            Message::PluginDeletePressed(id) => {
                self.plugin_delete_confirm = Some(id);
                return iced::Task::none();
            }
            Message::PluginDeleteCancelled(id) => {
                if self.plugin_delete_confirm.as_deref() == Some(id.as_str()) {
                    self.plugin_delete_confirm = None;
                }
                return iced::Task::none();
            }
            Message::PluginDeleteConfirmed(id) => {
                if self.plugin_action_busy {
                    return iced::Task::none();
                }
                self.plugin_delete_confirm = None;
                let wasm_path = self
                    .installed_plugins
                    .iter()
                    .find(|plugin| plugin.id == id)
                    .map(|plugin| plugin.wasm_path.clone());
                let manager = self.plugin_manager.clone();
                self.plugin_action_busy = true;
                return iced::Task::perform(
                    async move { super::helpers::delete_plugin(id, wasm_path, manager).await },
                    Message::PluginDeleteResult,
                );
            }
            Message::PluginDeleteResult(result) => {
                self.plugin_action_busy = false;
                self.plugin_install_result = Some(match &result {
                    Ok(message) => message.clone(),
                    Err(error) => format!("Error: {error}"),
                });
                if result.is_ok() {
                    self.load_plugin_grants();
                    return self.start_plugin_list_refresh();
                }
                return iced::Task::none();
            }
            Message::PluginListRefreshRequested => return self.start_plugin_list_refresh(),
            Message::PluginListRefreshResult(result) => {
                self.plugins_loaded = true;
                self.plugins_loading = false;
                match result {
                    Ok(infos) => {
                        // Keep the selection valid against the refreshed list:
                        // preserve it when still present, otherwise fall back
                        // to the first installed plugin.
                        let selected = self.ext_selected_plugin.clone();
                        self.installed_plugins = infos;
                        self.ext_selected_plugin = match selected {
                            Some(id)
                                if self.installed_plugins.iter().any(|plugin| plugin.id == id) =>
                            {
                                Some(id)
                            }
                            _ => self.installed_plugins.first().map(|plugin| plugin.id.clone()),
                        };
                    }
                    Err(error) => {
                        self.plugin_install_result =
                            Some(format!("Error: plugin directory scan failed: {error}"));
                    }
                }
                return iced::Task::none();
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
                    Ok(report) => {
                        self.skills_discovered = report.descriptors;
                        self.skills_warnings = dedupe_lines(report.warnings);
                        self.skills_notes = dedupe_lines(report.notes);
                        self.skills_error = None;
                        // Keep the master-detail selection valid: default to the
                        // first pack when nothing is selected or the selected
                        // id is no longer present in the discovered set.
                        if self.ext_selected_skill.is_none()
                            || !self
                                .skills_discovered
                                .iter()
                                .any(|skill| self.ext_selected_skill.as_deref() == Some(&skill.id))
                        {
                            self.ext_selected_skill =
                                self.skills_discovered.first().map(|skill| skill.id.clone());
                        }
                        // Discovery is the ground truth for what exists on disk:
                        // drop dangling edit/delete state whose pack is gone
                        // (removed on disk, or just deleted via the UI).
                        let discovered_ids: HashSet<&str> =
                            self.skills_discovered.iter().map(|skill| skill.id.as_str()).collect();
                        if let Some(editing) = &self.skill_editing_id {
                            if !discovered_ids.contains(editing.as_str()) {
                                self.skill_editing_id = None;
                                self.skill_edit_draft = None;
                            }
                        }
                        if let Some(confirming) = &self.skill_delete_confirm {
                            if !discovered_ids.contains(confirming.as_str()) {
                                self.skill_delete_confirm = None;
                            }
                        }
                    }
                    Err(error) => {
                        self.skills_error = Some(error);
                        self.skills_warnings = Vec::new();
                        self.skills_notes = Vec::new();
                    }
                }
            }

            // ADR-43 — skill pack CRUD (create wizard / edit / delete).
            // All of these are transient wizard/confirm/result messages: they
            // never arm `settings_dirty`, and pack files only land on disk via
            // the skills crate's create/update/delete operations inside the
            // spawned task.
            Message::SkillCreatePressed => {
                // Opening the wizard resets the draft and drops the previous
                // CRUD result (it described a different skill's operation).
                self.skill_crud_result = None;
                self.skill_create_id_error = None;
                self.skill_create_parent_error = None;
                self.skill_create_id = String::new();
                self.skill_create_name = String::new();
                self.skill_create_version = "0.1.0".to_string();
                self.skill_create_description = String::new();
                self.skill_create_instructions = iced::widget::text_editor::Content::new();
                self.skill_create_parent = self.create_parent_options().into_iter().next();
                self.skill_create_open = true;
            }
            Message::SkillCreateIdChanged(id) => {
                self.skill_create_id = id.clone();
                self.skill_create_id_error = Self::skill_id_error(&id);
                self.skill_crud_result = None;
            }
            Message::SkillCreateNameChanged(name) => self.skill_create_name = name,
            Message::SkillCreateVersionChanged(version) => self.skill_create_version = version,
            Message::SkillCreateDescriptionChanged(description) => {
                self.skill_create_description = description
            }
            Message::SkillCreateInstructionsChanged(action) => {
                self.skill_create_instructions.perform(action);
            }
            Message::SkillCreateParentChanged(parent) => {
                self.skill_create_parent = Some(parent);
                self.skill_create_parent_error = None;
                self.skill_crud_result = None;
            }
            Message::SkillCreateConfirmed => {
                // Validate id and parent before touching the filesystem.
                if let Some(error) = Self::skill_id_error(&self.skill_create_id) {
                    self.skill_create_id_error = Some(error);
                    return iced::Task::none();
                }
                let Some(parent_option) = self.skill_create_parent.clone() else {
                    self.skill_create_parent_error = Some("Pick a parent directory.".to_string());
                    return iced::Task::none();
                };
                self.skill_create_id_error = None;
                self.skill_create_parent_error = None;
                self.skill_crud_busy = true;
                let parent = parent_option.raw;
                let id = self.skill_create_id.trim().to_string();
                let name = {
                    let trimmed = self.skill_create_name.trim();
                    if trimmed.is_empty() {
                        id.clone()
                    } else {
                        trimmed.to_string()
                    }
                };
                let version = {
                    let trimmed = self.skill_create_version.trim();
                    if trimmed.is_empty() {
                        "0.1.0".to_string()
                    } else {
                        trimmed.to_string()
                    }
                };
                let description = self.skill_create_description.trim().to_string();
                let instructions = self.skill_create_instructions.text();
                return iced::Task::perform(
                    async move {
                        super::helpers::create_skill_pack(
                            parent,
                            id,
                            name,
                            version,
                            description,
                            instructions,
                        )
                    },
                    Message::SkillCreateResult,
                );
            }
            Message::SkillCreateCancelled => {
                self.skill_create_open = false;
                self.skill_crud_result = None;
                self.skill_create_id_error = None;
                self.skill_create_parent_error = None;
            }
            Message::SkillCreateResult(result) => {
                self.skill_crud_busy = false;
                match result {
                    Ok(outcome) => {
                        self.skill_crud_result = Some(outcome);
                        self.skill_create_open = false;
                        self.skill_create_id_error = None;
                        self.skill_create_parent_error = None;
                        // The pack landed on disk; refresh so it shows up in the
                        // discovered list (and becomes selectable).
                        return self.start_skill_discovery();
                    }
                    Err(error) => {
                        // Stay in the wizard so the user can correct the draft
                        // (e.g. an id that already exists on disk).
                        self.skill_crud_result = Some(format!("Error: {error}"));
                    }
                }
            }
            Message::SkillEditPressed(id) => {
                let Some(skill) = self.skills_discovered.iter().find(|skill| skill.id == id) else {
                    return iced::Task::none();
                };
                self.skill_crud_result = None;
                self.skill_editing_id = Some(id);
                self.skill_edit_draft = Some(SkillEditDraft {
                    name: skill.manifest.name.clone(),
                    description: skill.manifest.description.clone(),
                    instructions: iced::widget::text_editor::Content::with_text(
                        &skill.instructions,
                    ),
                });
            }
            Message::SkillEditNameChanged(name) => {
                if let Some(draft) = &mut self.skill_edit_draft {
                    draft.name = name;
                }
                self.skill_crud_result = None;
            }
            Message::SkillEditDescriptionChanged(description) => {
                if let Some(draft) = &mut self.skill_edit_draft {
                    draft.description = description;
                }
                self.skill_crud_result = None;
            }
            Message::SkillEditInstructionsChanged(action) => {
                if let Some(draft) = &mut self.skill_edit_draft {
                    draft.instructions.perform(action);
                }
                self.skill_crud_result = None;
            }
            Message::SkillEditSaved => {
                let Some(id) = self.skill_editing_id.clone() else {
                    return iced::Task::none();
                };
                let Some(skill) =
                    self.skills_discovered.iter().find(|skill| skill.id == id).cloned()
                else {
                    // The pack vanished between discovery and now; leave edit
                    // mode instead of writing to a gone directory.
                    self.skill_editing_id = None;
                    self.skill_edit_draft = None;
                    return iced::Task::none();
                };
                let Some(draft) = &self.skill_edit_draft else {
                    return iced::Task::none();
                };
                self.skill_crud_busy = true;
                let pack_dir = skill.pack_dir.to_string_lossy().into_owned();
                let name = {
                    let trimmed = draft.name.trim();
                    if trimmed.is_empty() {
                        skill.id.clone()
                    } else {
                        trimmed.to_string()
                    }
                };
                let manifest = concerto_api_types::extension::SkillManifest {
                    id: skill.id.clone(),
                    name,
                    version: skill.manifest.version.clone(),
                    description: draft.description.trim().to_string(),
                    instructions_path: None,
                    instructions: Some(draft.instructions.text()),
                    tools: skill.manifest.tools.clone(),
                    resources: skill.manifest.resources.clone(),
                };
                return iced::Task::perform(
                    async move { super::helpers::update_skill_pack(pack_dir, manifest) },
                    Message::SkillEditResult,
                );
            }
            Message::SkillEditCancelled => {
                self.skill_editing_id = None;
                self.skill_edit_draft = None;
                self.skill_crud_result = None;
            }
            Message::SkillEditResult(result) => {
                self.skill_crud_busy = false;
                match result {
                    Ok(outcome) => {
                        self.skill_crud_result = Some(outcome);
                        self.skill_editing_id = None;
                        self.skill_edit_draft = None;
                        return self.start_skill_discovery();
                    }
                    Err(error) => {
                        // Stay in edit mode so the user can fix the draft (e.g.
                        // a read-only pack directory).
                        self.skill_crud_result = Some(format!("Error: {error}"));
                    }
                }
            }
            Message::SkillDeletePressed(id) => {
                // First press only arms the confirm prompt (destructive action;
                // same explicit-confirm rule as providers and MCP servers).
                self.skill_delete_confirm = Some(id);
                self.skill_crud_result = None;
            }
            Message::SkillDeleteCancelled(id) => {
                if self.skill_delete_confirm.as_deref() == Some(id.as_str()) {
                    self.skill_delete_confirm = None;
                }
            }
            Message::SkillDeleteConfirmed(id) => {
                // Clear the prompt immediately: the task below is the one
                // destructive step, and the confirm row must not linger.
                self.skill_delete_confirm = None;
                let Some(pack_dir) = self
                    .skills_discovered
                    .iter()
                    .find(|skill| skill.id == id)
                    .map(|skill| skill.pack_dir.to_string_lossy().into_owned())
                else {
                    return iced::Task::none();
                };
                self.skill_crud_busy = true;
                return iced::Task::perform(
                    async move { super::helpers::delete_skill_pack(pack_dir) },
                    Message::SkillDeleteResult,
                );
            }
            Message::SkillDeleteResult(result) => {
                self.skill_crud_busy = false;
                match result {
                    Ok(outcome) => {
                        self.skill_crud_result = Some(outcome);
                        // The pack is gone from disk; refresh so the list no
                        // longer shows it. If it was selected, discovery
                        // re-defaults the selection to the first remaining pack.
                        return self.start_skill_discovery();
                    }
                    Err(error) => {
                        self.skill_crud_result = Some(format!("Error: {error}"));
                    }
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

            // ADR-43 — MCP server edit/delete. The draft is transient view
            // state: field edits mutate only the draft and never arm the
            // dirty flag. The pending config changes only when a draft is
            // committed (`McpEditSaved`) or a server is confirmed for
            // deletion (`McpDeleteConfirmed`), and is persisted by the
            // regular Save Settings flow.
            Message::McpEditPressed(id) => {
                let Some(server) = self.mcp_servers.iter().find(|server| server.id == id).cloned()
                else {
                    return iced::Task::none();
                };
                self.mcp_editing_id = Some(server.id.clone());
                self.mcp_edit_draft = Some(McpEditDraft {
                    command: server.command,
                    args: server.args.join(" "),
                    env: server.env.unwrap_or_default(),
                    timeout: server.timeout_secs.map(|secs| secs.to_string()).unwrap_or_default(),
                    command_error: None,
                    env_error: None,
                    timeout_error: None,
                });
            }
            Message::McpEditCancelled => {
                self.mcp_editing_id = None;
                self.mcp_edit_draft = None;
            }
            Message::McpEditCommandChanged(value) => {
                if let Some(draft) = &mut self.mcp_edit_draft {
                    draft.command = value;
                    draft.command_error = None;
                }
            }
            Message::McpEditArgsChanged(value) => {
                if let Some(draft) = &mut self.mcp_edit_draft {
                    draft.args = value;
                }
            }
            Message::McpEditEnvKeyChanged(index, value) => {
                if let Some(draft) = &mut self.mcp_edit_draft {
                    let keys: Vec<String> = draft.env.keys().cloned().collect();
                    if let Some(old) = keys.get(index) {
                        if let Some(env_value) = draft.env.remove(old) {
                            draft.env.insert(value, env_value);
                        }
                    }
                    draft.env_error = None;
                }
            }
            Message::McpEditEnvValueChanged(index, value) => {
                if let Some(draft) = &mut self.mcp_edit_draft {
                    let keys: Vec<String> = draft.env.keys().cloned().collect();
                    if let Some(key) = keys.get(index) {
                        draft.env.insert(key.clone(), value);
                    }
                    draft.env_error = None;
                }
            }
            Message::McpEditEnvAdd => {
                if let Some(draft) = &mut self.mcp_edit_draft {
                    // Pick the first unused VAR{n} name so empty/duplicate
                    // keys cannot be introduced by the Add button.
                    let mut n = 1;
                    while draft.env.contains_key(&format!("VAR{n}")) {
                        n += 1;
                    }
                    draft.env.insert(format!("VAR{n}"), String::new());
                    draft.env_error = None;
                }
            }
            Message::McpEditEnvRemove(index) => {
                if let Some(draft) = &mut self.mcp_edit_draft {
                    let keys: Vec<String> = draft.env.keys().cloned().collect();
                    if let Some(key) = keys.get(index) {
                        draft.env.remove(key);
                    }
                    draft.env_error = None;
                }
            }
            Message::McpEditTimeoutChanged(value) => {
                if let Some(draft) = &mut self.mcp_edit_draft {
                    draft.timeout = value;
                    draft.timeout_error = Self::validate_mcp_timeout(&draft.timeout);
                }
            }
            Message::McpEditSaved => {
                let Some(id) = self.mcp_editing_id.clone() else {
                    return iced::Task::none();
                };
                let Some(mut draft) = self.mcp_edit_draft.take() else {
                    return iced::Task::none();
                };
                // Inline validation: keep the user in edit mode with the
                // errors visible; only a valid draft is applied.
                draft.command_error = if draft.command.trim().is_empty() {
                    Some("Command is required".into())
                } else {
                    None
                };
                draft.env_error = if draft.env.keys().any(|key| key.trim().is_empty()) {
                    Some("Environment keys must not be empty".into())
                } else {
                    None
                };
                draft.timeout_error = Self::validate_mcp_timeout(&draft.timeout);
                if draft.command_error.is_some()
                    || draft.env_error.is_some()
                    || draft.timeout_error.is_some()
                {
                    self.mcp_edit_draft = Some(draft);
                    return iced::Task::none();
                }
                if let Some(server) = self.mcp_servers.iter_mut().find(|server| server.id == id) {
                    server.command = draft.command.trim().to_string();
                    server.args = draft.args.split_whitespace().map(String::from).collect();
                    server.env = Some(draft.env).filter(|vars| !vars.is_empty());
                    server.timeout_secs = draft.timeout.trim().parse::<u64>().ok();
                    self.settings_dirty = true;
                }
                self.mcp_editing_id = None;
                self.mcp_edit_draft = None;
                // A committed edit invalidates the last probe result for the
                // server (it described the previous command line).
                self.mcp_probe_results.remove(&id);
            }
            Message::McpDeletePressed(id) => {
                // Toggle the confirm prompt; the destructive removal only
                // happens on McpDeleteConfirmed (same explicit-confirm rule
                // as provider deletion, plan §5.3). Arming is transient and
                // never arms the dirty flag.
                if self.mcp_delete_confirm.as_deref() == Some(id.as_str()) {
                    self.mcp_delete_confirm = None;
                } else {
                    self.mcp_delete_confirm = Some(id);
                }
            }
            Message::McpDeleteCancelled(id) => {
                if self.mcp_delete_confirm.as_deref() == Some(id.as_str()) {
                    self.mcp_delete_confirm = None;
                }
            }
            Message::McpDeleteConfirmed(id) => {
                if self.mcp_delete_confirm.as_deref() != Some(id.as_str()) {
                    return iced::Task::none();
                }
                let Some(index) = self.mcp_servers.iter().position(|server| server.id == id) else {
                    self.mcp_delete_confirm = None;
                    return iced::Task::none();
                };
                self.mcp_servers.remove(index);
                self.mcp_delete_confirm = None;
                // Drop transient state that referenced the deleted server:
                // an open edit draft, probe results, and the selection.
                if self.mcp_editing_id.as_deref() == Some(id.as_str()) {
                    self.mcp_editing_id = None;
                    self.mcp_edit_draft = None;
                }
                if self.ext_selected_mcp.as_deref() == Some(id.as_str()) {
                    self.ext_selected_mcp =
                        self.mcp_servers.first().map(|server| server.id.clone());
                }
                self.mcp_probe_results.remove(&id);
                self.mcp_probing.remove(&id);
                self.settings_dirty = true;
            }

            // ADR-43 — MCP server add. The add draft is transient view state:
            // field edits mutate only the draft and never arm the dirty flag.
            // A committed add pushes a new server into `mcp_servers` and arms
            // the dirty flag, so it persists through the regular Save Settings
            // flow (next-run semantics).
            Message::McpAddPressed => {
                // Opening the add form leaves any in-progress edit/delete
                // state: the detail pane is given over to the new-server form.
                self.mcp_editing_id = None;
                self.mcp_edit_draft = None;
                self.mcp_delete_confirm = None;
                self.mcp_add_draft = Some(McpAddDraft {
                    id: String::new(),
                    command: String::new(),
                    args: String::new(),
                    env: BTreeMap::new(),
                    timeout: String::new(),
                    id_error: None,
                    command_error: None,
                    env_error: None,
                    timeout_error: None,
                });
            }
            Message::McpAddCancelled => {
                self.mcp_add_draft = None;
            }
            Message::McpAddIdChanged(value) => {
                if let Some(draft) = &mut self.mcp_add_draft {
                    draft.id = value;
                    draft.id_error = Self::mcp_add_id_error(&self.mcp_servers, draft.id.trim());
                }
            }
            Message::McpAddCommandChanged(value) => {
                if let Some(draft) = &mut self.mcp_add_draft {
                    draft.command = value;
                    draft.command_error = None;
                }
            }
            Message::McpAddArgsChanged(value) => {
                if let Some(draft) = &mut self.mcp_add_draft {
                    draft.args = value;
                }
            }
            Message::McpAddEnvKeyChanged(index, value) => {
                if let Some(draft) = &mut self.mcp_add_draft {
                    let keys: Vec<String> = draft.env.keys().cloned().collect();
                    if let Some(old) = keys.get(index) {
                        if let Some(env_value) = draft.env.remove(old) {
                            draft.env.insert(value, env_value);
                        }
                    }
                    draft.env_error = None;
                }
            }
            Message::McpAddEnvValueChanged(index, value) => {
                if let Some(draft) = &mut self.mcp_add_draft {
                    let keys: Vec<String> = draft.env.keys().cloned().collect();
                    if let Some(key) = keys.get(index) {
                        draft.env.insert(key.clone(), value);
                    }
                    draft.env_error = None;
                }
            }
            Message::McpAddEnvAdd => {
                if let Some(draft) = &mut self.mcp_add_draft {
                    // Pick the first unused VAR{n} name so empty/duplicate
                    // keys cannot be introduced by the Add button.
                    let mut n = 1;
                    while draft.env.contains_key(&format!("VAR{n}")) {
                        n += 1;
                    }
                    draft.env.insert(format!("VAR{n}"), String::new());
                    draft.env_error = None;
                }
            }
            Message::McpAddEnvRemove(index) => {
                if let Some(draft) = &mut self.mcp_add_draft {
                    let keys: Vec<String> = draft.env.keys().cloned().collect();
                    if let Some(key) = keys.get(index) {
                        draft.env.remove(key);
                    }
                    draft.env_error = None;
                }
            }
            Message::McpAddTimeoutChanged(value) => {
                if let Some(draft) = &mut self.mcp_add_draft {
                    draft.timeout = value;
                    draft.timeout_error = Self::validate_mcp_timeout(&draft.timeout);
                }
            }
            Message::McpAddSaved => {
                let Some(mut draft) = self.mcp_add_draft.take() else {
                    return iced::Task::none();
                };
                // Inline validation mirrors `McpConfig::validate` plus the
                // edit draft's command/env/timeout rules. Keep the form open
                // with the errors visible; only a valid draft is applied.
                draft.id_error = Self::mcp_add_id_error(&self.mcp_servers, draft.id.trim());
                draft.command_error = if draft.command.trim().is_empty() {
                    Some("Command is required".into())
                } else {
                    None
                };
                draft.env_error = if draft.env.keys().any(|key| key.trim().is_empty()) {
                    Some("Environment keys must not be empty".into())
                } else {
                    None
                };
                draft.timeout_error = Self::validate_mcp_timeout(&draft.timeout);
                if draft.id_error.is_some()
                    || draft.command_error.is_some()
                    || draft.env_error.is_some()
                    || draft.timeout_error.is_some()
                {
                    self.mcp_add_draft = Some(draft);
                    return iced::Task::none();
                }
                let id = draft.id.trim().to_string();
                self.mcp_servers.push(McpServerConfig {
                    id: id.clone(),
                    command: draft.command.trim().to_string(),
                    args: draft.args.split_whitespace().map(String::from).collect(),
                    env: Some(draft.env).filter(|vars| !vars.is_empty()),
                    enabled: true,
                    timeout_secs: draft.timeout.trim().parse::<u64>().ok(),
                });
                // Select the new server so its read-only detail renders
                // instead of the empty "No server selected" pane.
                self.ext_selected_mcp = Some(id);
                self.mcp_add_draft = None;
                self.settings_dirty = true;
            }

            // Unified Extensions manager (ADR-37/43/70). Tab switching and
            // master-list selection are transient view state: they never arm
            // the dirty flag and are never persisted.
            Message::ExtensionTabSelected(tab) => {
                self.active_extension_tab = tab;
                if tab == ExtensionTab::Plugins {
                    // The installed-plugin list is scanned once, lazily, the
                    // first time the tab is opened (mirrors skill discovery).
                    if !self.plugins_loaded && !self.plugins_loading {
                        return self.start_plugin_list_refresh();
                    }
                }
            }
            Message::ExtensionItemSelected(tab, id) => {
                match tab {
                    ExtensionTab::Skills => {
                        // A CRUD outcome belongs to the previously selected
                        // skill; drop it so the next selection never shows a
                        // stale result line.
                        self.skill_crud_result = None;
                        self.ext_selected_skill = Some(id);
                    }
                    ExtensionTab::Mcp => self.ext_selected_mcp = Some(id),
                    ExtensionTab::Plugins => {
                        // A revoke or delete outcome belongs to the previously
                        // selected plugin; drop it so the next selection never
                        // shows a stale result line.
                        self.plugin_revoke_result = None;
                        self.plugin_install_result = None;
                        self.plugin_delete_confirm = None;
                        self.ext_selected_plugin = Some(id);
                    }
                    // The project-context tab has no item list.
                    ExtensionTab::ProjectContext => {}
                }
            }

            // ADR-70 — project AGENTS.md context injection. Persisted on Save
            // Settings; each edit explicitly arms `project_context_dirty` so a
            // plain save never publishes the startup snapshot.
            Message::ProjectContextEnabledToggled(enabled) => {
                self.settings_dirty = true;
                self.project_context_dirty = true;
                self.project_context_enabled = enabled;
                // Disabling the feature also quiets the nudge: ADR-70 gates the
                // advisory on the whole feature being active, so leaving the
                // nudge on after a disable would be dead config.
                if !enabled {
                    self.project_context_auto_update_agents_md = false;
                }
            }
            Message::ProjectContextNudgeToggled(enabled) => {
                self.settings_dirty = true;
                self.project_context_dirty = true;
                self.project_context_auto_update_agents_md = enabled;
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

/// Collapse duplicates while preserving first-occurrence order.
///
/// Discovery diagnostics can repeat — e.g. two configured search paths that
/// resolve to the same tree, or one pack discovered through an overlapping
/// path scan. The settings list renders each line once.
fn dedupe_lines(lines: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    lines.into_iter().filter(|line| seen.insert(line.clone())).collect()
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
            pack_dir: PathBuf::new(),
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
        let report = concerto_skills::DiscoveryReport {
            resolved_paths: vec![PathBuf::from("/nonexistent/skills")],
            descriptors: vec![skill("rust-testing")],
            warnings: vec![
                "`/nonexistent/skills` is missing or not a directory; skipping".into(),
                // Duplicate diagnostics (e.g. two search paths resolving to the
                // same tree) must be collapsed by the state layer.
                "`/nonexistent/skills` is missing or not a directory; skipping".into(),
            ],
            notes: vec![
                "Skill 'rust-testing' loaded via directory name".into(),
                "Skill 'rust-testing' loaded via directory name".into(),
            ],
        };

        let _ = state.update(Message::SkillsDiscoveryResult(Ok(report)));

        assert!(!state.skills_loading, "loading flag must clear after a result");
        assert!(state.skills_loaded);
        assert!(state.skills_error.is_none());
        assert_eq!(state.skills_discovered, vec![skill("rust-testing")]);
        assert_eq!(
            state.skills_warnings,
            vec!["`/nonexistent/skills` is missing or not a directory; skipping".to_string()],
            "duplicate warnings must be collapsed"
        );
        assert_eq!(
            state.skills_notes,
            vec!["Skill 'rust-testing' loaded via directory name".to_string()],
            "discovery notes must be surfaced and duplicates collapsed"
        );

        // An error records the message without touching discovered skills;
        // warnings and notes from the previous run are cleared.
        let _ = state.update(Message::SkillsDiscoveryResult(Err("boom".into())));
        assert!(state.skills_loaded);
        assert_eq!(state.skills_error.as_deref(), Some("boom"));
        assert!(state.skills_warnings.is_empty());
        assert!(state.skills_notes.is_empty(), "notes must not leak across a failed run");
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

    #[test]
    fn skill_id_error_rejects_bad_ids_and_accepts_good_ones() {
        assert!(State::skill_id_error("rust-testing").is_none());
        assert!(
            State::skill_id_error("  rust-testing  ").is_none(),
            "leading/trailing whitespace is tolerated (trimmed before validation)"
        );
        for bad in ["", ".", "..", "a/b", "a\\b", "a\0b", "a:b"] {
            assert!(State::skill_id_error(bad).is_some(), "{bad:?} must be rejected");
        }
    }

    #[test]
    fn skill_create_wizard_validates_id_and_parent_before_writing() {
        let mut state = State::from_config(&AppConfig::default());
        let _ = state.update(Message::SkillCreatePressed);
        assert!(state.skill_create_open);
        assert!(
            state.skill_create_parent.is_some(),
            "opening the wizard seeds the parent picker from the configured search paths"
        );
        assert!(!state.settings_dirty, "opening the wizard must not arm the dirty flag");

        // Invalid id → nothing is spawned and the inline error surfaces.
        let _ = state.update(Message::SkillCreateIdChanged("a/b".into()));
        let task = state.update(Message::SkillCreateConfirmed);
        assert_eq!(task.units(), 0, "an invalid wizard must not spawn a write task");
        assert!(state.skill_create_open, "the wizard stays open for correction");
        assert!(state.skill_create_id_error.is_some());
        assert!(!state.settings_dirty);
        assert!(!state.skill_crud_busy, "a rejected confirm must not arm busy");

        // A missing parent (e.g. no search paths configured) is also rejected.
        state.skill_create_parent = None;
        let _ = state.update(Message::SkillCreateIdChanged("rust-testing".into()));
        let task = state.update(Message::SkillCreateConfirmed);
        assert_eq!(task.units(), 0);
        assert!(state.skill_create_parent_error.is_some());

        // A valid id + parent schedules a create task and clears both errors.
        let _ = state.update(Message::SkillCreateParentChanged(CreateParentOption {
            raw: "/nonexistent/parent/skills".into(),
            label: "/nonexistent/parent/skills (missing)".into(),
        }));
        let task = state.update(Message::SkillCreateConfirmed);
        assert_eq!(task.units(), 1, "a valid wizard must spawn a create task");
        assert!(state.skill_crud_busy);
        assert!(state.skill_create_id_error.is_none());
        assert!(state.skill_create_parent_error.is_none());
    }

    #[test]
    fn skill_crud_transients_never_arm_dirty_flag() {
        let mut state = State::from_config(&AppConfig::default());
        state.skills_discovered = vec![skill("rust-testing")];
        state.ext_selected_skill = Some("rust-testing".into());

        let _ = state.update(Message::SkillCreatePressed);
        let _ = state.update(Message::SkillCreateIdChanged("new-skill".into()));
        let _ = state.update(Message::SkillCreateNameChanged("New Skill".into()));
        let _ = state.update(Message::SkillCreateInstructionsChanged(
            iced::widget::text_editor::Action::Edit(iced::widget::text_editor::Edit::Enter),
        ));
        let _ = state.update(Message::SkillCreateCancelled);

        let _ = state.update(Message::SkillEditPressed("rust-testing".into()));
        let _ = state.update(Message::SkillEditNameChanged("Renamed".into()));
        assert!(state.skill_edit_draft.is_some());
        let _ = state.update(Message::SkillEditCancelled);
        assert!(state.skill_edit_draft.is_none());

        let _ = state.update(Message::SkillDeletePressed("rust-testing".into()));
        assert_eq!(state.skill_delete_confirm.as_deref(), Some("rust-testing"));
        let _ = state.update(Message::SkillDeleteCancelled("rust-testing".into()));
        assert!(state.skill_delete_confirm.is_none());

        assert!(
            !state.settings_dirty,
            "wizard/edit/delete transients must never arm the dirty flag"
        );
    }

    #[test]
    fn skill_delete_confirmed_spawns_delete_task() {
        let mut state = State::from_config(&AppConfig::default());
        state.skills_discovered = vec![skill("rust-testing")];

        let task = state.update(Message::SkillDeleteConfirmed("rust-testing".into()));
        assert_eq!(task.units(), 1, "a confirmed delete spawns the delete task");
        assert!(state.skill_crud_busy);
        assert!(
            state.skill_delete_confirm.is_none(),
            "the confirm prompt clears immediately when the task is spawned"
        );
    }

    #[test]
    fn skill_edit_pressed_seeds_draft_from_descriptor() {
        let mut state = State::from_config(&AppConfig::default());
        state.skills_discovered = vec![skill("rust-testing")];

        let _ = state.update(Message::SkillEditPressed("rust-testing".into()));
        let draft = state.skill_edit_draft.as_ref().expect("draft must be seeded");
        assert_eq!(draft.name, "rust-testing", "name seeds from the manifest name");
        assert_eq!(draft.description, "test skill");
        assert_eq!(draft.instructions.text(), "do the thing");

        let _ = state.update(Message::SkillEditSaved);
        assert!(state.skill_crud_busy, "saving the draft schedules the update task");
    }

    #[test]
    fn skill_discovery_result_drops_dangling_edit_and_delete_state() {
        let mut state = State::from_config(&AppConfig::default());
        state.skills_discovered = vec![skill("kept")];
        state.skill_editing_id = Some("gone".into());
        state.skill_edit_draft = Some(SkillEditDraft {
            name: "gone".into(),
            description: String::new(),
            instructions: iced::widget::text_editor::Content::new(),
        });
        state.skill_delete_confirm = Some("gone".into());

        let report = concerto_skills::DiscoveryReport {
            resolved_paths: vec![PathBuf::from("/nonexistent/skills")],
            descriptors: vec![skill("kept")],
            warnings: Vec::new(),
            notes: Vec::new(),
        };
        let _ = state.update(Message::SkillsDiscoveryResult(Ok(report)));

        assert!(state.skill_editing_id.is_none(), "edit state for a vanished pack must be dropped");
        assert!(state.skill_edit_draft.is_none());
        assert!(
            state.skill_delete_confirm.is_none(),
            "confirm state for a vanished pack must be dropped"
        );
        assert_eq!(state.skills_discovered, vec![skill("kept")]);
    }

    #[test]
    fn ext_selection_change_clears_stale_skill_crud_result() {
        let mut state = State::from_config(&AppConfig::default());
        state.skills_discovered = vec![skill("rust-testing"), skill("code-review")];
        state.ext_selected_skill = Some("rust-testing".into());
        state.skill_crud_result = Some("Saved '/tmp/pack'".into());

        let _ = state
            .update(Message::ExtensionItemSelected(ExtensionTab::Skills, "code-review".into()));

        assert_eq!(state.ext_selected_skill.as_deref(), Some("code-review"));
        assert!(
            state.skill_crud_result.is_none(),
            "a CRUD outcome belongs to the previously selected skill and must not leak"
        );
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

    // ── ADR-43 — MCP server edit/delete ───────────────────────────────────

    #[test]
    fn mcp_edit_pressed_seeds_draft_and_save_applies_changes() {
        let base = AppConfig {
            mcp: Some(McpConfig { enabled: true, servers: vec![server("files")] }),
            ..AppConfig::default()
        };
        let mut state = State::from_config(&base);
        // Seed an env and timeout so the draft seed + apply path covers them.
        state.mcp_servers[0].env = Some([("API_KEY".to_string(), "s3cret".to_string())].into());
        state.mcp_servers[0].timeout_secs = Some(120);

        let _ = state.update(Message::McpEditPressed("files".into()));
        let draft = state.mcp_edit_draft.as_ref().expect("edit pressed must seed a draft");
        assert_eq!(state.mcp_editing_id.as_deref(), Some("files"));
        assert_eq!(draft.command, "npx");
        assert_eq!(draft.args, "-y @example/files");
        assert_eq!(draft.env.get("API_KEY"), Some(&"s3cret".to_string()));
        assert_eq!(draft.timeout, "120");
        assert!(!state.settings_dirty, "entering edit mode must not arm the dirty flag by itself");

        // Mutate the draft and save.
        let _ = state.update(Message::McpEditCommandChanged("node".into()));
        let _ = state.update(Message::McpEditArgsChanged("server.js --port 3000".into()));
        let _ = state.update(Message::McpEditEnvValueChanged(0, "new-secret".into()));
        let _ = state.update(Message::McpEditTimeoutChanged("45".into()));
        let _ = state.update(Message::McpEditSaved);

        assert!(state.mcp_editing_id.is_none(), "a successful save must leave edit mode");
        assert!(state.mcp_edit_draft.is_none());
        assert!(state.settings_dirty, "a committed edit must arm the dirty flag");
        let server = &state.mcp_servers[0];
        assert_eq!(server.id, "files", "the id is the stable tool-namespace key");
        assert!(server.enabled, "the enabled flag must be preserved");
        assert_eq!(server.command, "node");
        assert_eq!(server.args, vec!["server.js", "--port", "3000"]);
        assert_eq!(
            server.env.as_ref().and_then(|env| env.get("API_KEY")),
            Some(&"new-secret".to_string())
        );
        assert_eq!(server.timeout_secs, Some(45));

        // The committed edit round-trips through to_config.
        let saved = state.to_config(&base);
        let mcp = saved.mcp.expect("mcp section must be published");
        assert_eq!(mcp.servers[0].command, "node");
        assert_eq!(
            mcp.servers[0].env.as_ref().and_then(|e| e.get("API_KEY")),
            Some(&"new-secret".to_string())
        );
    }

    #[test]
    fn mcp_edit_invalid_draft_stays_in_edit_mode_with_inline_errors() {
        let base = AppConfig {
            mcp: Some(McpConfig { enabled: true, servers: vec![server("files")] }),
            ..AppConfig::default()
        };
        let mut state = State::from_config(&base);
        let _ = state.update(Message::McpEditPressed("files".into()));
        // Blank command and an over-cap timeout are both invalid.
        let _ = state.update(Message::McpEditCommandChanged("   ".into()));
        let _ = state.update(Message::McpEditTimeoutChanged("400".into()));

        let _ = state.update(Message::McpEditSaved);

        assert!(state.mcp_editing_id.is_some(), "invalid input must keep edit mode open");
        let draft = state.mcp_edit_draft.as_ref().expect("the draft must be retained");
        assert_eq!(draft.command_error.as_deref(), Some("Command is required"));
        assert_eq!(draft.timeout_error.as_deref(), Some("Hard cap is 300 seconds"));
        assert_eq!(state.mcp_servers[0].command, "npx", "the server must be untouched");
        assert!(!state.settings_dirty, "a rejected draft must not arm the dirty flag");

        // Fixing the errors lets the edit land.
        let _ = state.update(Message::McpEditCommandChanged("npx".into()));
        let _ = state.update(Message::McpEditTimeoutChanged("300".into()));
        let _ = state.update(Message::McpEditSaved);
        assert!(state.mcp_editing_id.is_none());
        assert_eq!(state.mcp_servers[0].timeout_secs, Some(300));
    }

    #[test]
    fn mcp_edit_env_add_and_remove_manage_rows() {
        let base = AppConfig {
            mcp: Some(McpConfig { enabled: true, servers: vec![server("files")] }),
            ..AppConfig::default()
        };
        let mut state = State::from_config(&base);
        let _ = state.update(Message::McpEditPressed("files".into()));

        let _ = state.update(Message::McpEditEnvAdd);
        assert!(
            state.mcp_edit_draft.as_ref().unwrap().env.contains_key("VAR1"),
            "Add must create the first unused VAR1 row"
        );

        let _ = state.update(Message::McpEditEnvRemove(0));
        assert!(state.mcp_edit_draft.as_ref().unwrap().env.is_empty());
    }

    #[test]
    fn mcp_edit_cancel_discards_the_draft() {
        let base = AppConfig {
            mcp: Some(McpConfig { enabled: true, servers: vec![server("files")] }),
            ..AppConfig::default()
        };
        let mut state = State::from_config(&base);
        let _ = state.update(Message::McpEditPressed("files".into()));
        let _ = state.update(Message::McpEditCommandChanged("node".into()));

        let _ = state.update(Message::McpEditCancelled);

        assert!(state.mcp_editing_id.is_none());
        assert!(state.mcp_edit_draft.is_none());
        assert_eq!(state.mcp_servers[0].command, "npx", "cancelling must not change the server");
        assert!(!state.settings_dirty);
    }

    #[test]
    fn mcp_delete_requires_confirmation_and_removes_server() {
        let base = AppConfig {
            mcp: Some(McpConfig {
                enabled: true,
                servers: vec![server("files"), server("github")],
            }),
            ..AppConfig::default()
        };
        let mut state = State::from_config(&base);
        state.ext_selected_mcp = Some("files".into());

        // First press only arms the confirm prompt.
        let _ = state.update(Message::McpDeletePressed("files".into()));
        assert_eq!(state.mcp_delete_confirm.as_deref(), Some("files"));
        assert_eq!(state.mcp_servers.len(), 2, "arming must not remove anything");
        assert!(!state.settings_dirty, "arming a prompt must not arm the dirty flag");

        // Cancelling keeps the server.
        let _ = state.update(Message::McpDeleteCancelled("files".into()));
        assert!(state.mcp_delete_confirm.is_none());
        assert_eq!(state.mcp_servers.len(), 2);

        // Confirming removes the server and repoints the selection.
        let _ = state.update(Message::McpDeletePressed("files".into()));
        let _ = state.update(Message::McpDeleteConfirmed("files".into()));
        assert!(state.mcp_delete_confirm.is_none());
        assert_eq!(
            state.mcp_servers.iter().map(|server| server.id.as_str()).collect::<Vec<_>>(),
            vec!["github"]
        );
        assert_eq!(state.ext_selected_mcp.as_deref(), Some("github"));
        assert!(state.settings_dirty, "a confirmed delete must arm the dirty flag");

        // A confirm against a server that is not armed is a no-op.
        let _ = state.update(Message::McpDeleteConfirmed("github".into()));
        assert_eq!(state.mcp_servers.len(), 1);
    }

    #[test]
    fn mcp_delete_drops_edit_mode_and_probe_state_for_removed_server() {
        let base = AppConfig {
            mcp: Some(McpConfig {
                enabled: true,
                servers: vec![server("files"), server("github")],
            }),
            ..AppConfig::default()
        };
        let mut state = State::from_config(&base);
        // Edit the server that will be deleted, and keep a probe result for it.
        let _ = state.update(Message::McpEditPressed("files".into()));
        state.mcp_probing.insert("files".into());
        state.mcp_probe_results.insert("files".into(), Ok(Vec::new()));

        let _ = state.update(Message::McpDeletePressed("files".into()));
        let _ = state.update(Message::McpDeleteConfirmed("files".into()));

        assert!(state.mcp_editing_id.is_none(), "deleting a server must close its edit draft");
        assert!(state.mcp_edit_draft.is_none());
        assert!(!state.mcp_probing.contains("files"));
        assert!(!state.mcp_probe_results.contains_key("files"));
    }

    // ── ADR-43 — MCP server add ───────────────────────────────────────────
    //
    // The add draft is transient view state: entering/cancelling the add form
    // never arms the dirty flag, and the new server only lands in
    // `mcp_servers` on a successful `McpAddSaved` — which persists through the
    // regular Save Settings flow (next-run semantics).

    #[test]
    fn mcp_add_pressed_seeds_blank_draft_and_cancel_discards() {
        let base = AppConfig {
            mcp: Some(McpConfig { enabled: true, servers: vec![server("files")] }),
            ..AppConfig::default()
        };
        let mut state = State::from_config(&base);
        // Enter the add form; it must displace any in-progress edit state.
        let _ = state.update(Message::McpEditPressed("files".into()));
        assert!(state.mcp_editing_id.is_some(), "precondition: edit mode is open");
        let _ = state.update(Message::McpAddPressed);

        let draft = state.mcp_add_draft.as_ref().expect("Add pressed must open the form");
        assert!(state.mcp_editing_id.is_none(), "opening the add form must leave edit mode");
        assert!(state.mcp_edit_draft.is_none());
        assert!(draft.id.is_empty());
        assert!(draft.command.is_empty());
        assert!(draft.env.is_empty());
        assert!(draft.timeout.is_empty());
        assert!(draft.id_error.is_none());
        assert!(!state.settings_dirty, "opening the add form must not arm the dirty flag");

        let _ = state.update(Message::McpAddCancelled);
        assert!(state.mcp_add_draft.is_none(), "cancelling must discard the draft");
        assert!(!state.settings_dirty, "cancelling must not arm the dirty flag");
    }

    #[test]
    fn mcp_add_saved_appends_valid_server_and_round_trips() {
        let base = AppConfig {
            mcp: Some(McpConfig { enabled: true, servers: vec![server("files")] }),
            ..AppConfig::default()
        };
        let mut state = State::from_config(&base);
        let _ = state.update(Message::McpAddPressed);
        let _ = state.update(Message::McpAddIdChanged("github".into()));
        let _ = state.update(Message::McpAddCommandChanged("npx".into()));
        let _ = state.update(Message::McpAddArgsChanged("-y @example/github-server".into()));
        let _ = state.update(Message::McpAddEnvAdd);
        let _ = state.update(Message::McpAddEnvKeyChanged(0, "TOKEN".into()));
        let _ = state.update(Message::McpAddEnvValueChanged(0, "s3cret".into()));
        let _ = state.update(Message::McpAddTimeoutChanged("45".into()));

        let _ = state.update(Message::McpAddSaved);

        assert!(state.mcp_add_draft.is_none(), "a successful add must leave the form");
        assert!(state.settings_dirty, "a committed add must arm the dirty flag");
        assert_eq!(state.mcp_servers.len(), 2);
        let added = &state.mcp_servers[1];
        assert_eq!(added.id, "github");
        assert_eq!(added.command, "npx");
        assert_eq!(added.args, vec!["-y", "@example/github-server"]);
        assert_eq!(added.env.as_ref().and_then(|e| e.get("TOKEN")), Some(&"s3cret".to_string()));
        assert_eq!(added.timeout_secs, Some(45));
        assert!(added.enabled, "new servers are enabled by default");
        assert_eq!(
            state.ext_selected_mcp.as_deref(),
            Some("github"),
            "a committed add must select the new server"
        );

        // The added server survives to_config, so Save Settings persists it.
        let saved = state.to_config(&base);
        let mcp = saved.mcp.expect("the mcp section must be published");
        assert_eq!(mcp.servers.len(), 2);
        assert_eq!(mcp.servers[1].id, "github");
        assert_eq!(mcp.servers[1].command, "npx");
    }

    #[test]
    fn mcp_add_invalid_draft_stays_open_with_inline_errors() {
        let base = AppConfig {
            mcp: Some(McpConfig { enabled: true, servers: vec![server("files")] }),
            ..AppConfig::default()
        };
        let mut state = State::from_config(&base);
        let _ = state.update(Message::McpAddPressed);
        // Blank id + blank command + duplicate id + over-cap timeout are all
        // rejected on save and keep the form open with inline errors.
        let _ = state.update(Message::McpAddIdChanged("files".into()));
        let _ = state.update(Message::McpAddTimeoutChanged("400".into()));

        let _ = state.update(Message::McpAddSaved);

        let draft = state.mcp_add_draft.as_ref().expect("an invalid draft must keep the form open");
        assert_eq!(draft.id_error.as_deref(), Some("An MCP server with this id already exists"));
        assert_eq!(draft.command_error.as_deref(), Some("Command is required"));
        assert_eq!(draft.timeout_error.as_deref(), Some("Hard cap is 300 seconds"));
        assert_eq!(state.mcp_servers.len(), 1, "no server may be appended while invalid");
        assert!(!state.settings_dirty, "a rejected draft must not arm the dirty flag");

        // A colon in the id is rejected too.
        let _ = state.update(Message::McpAddIdChanged("a:b".into()));
        let _ = state.update(Message::McpAddSaved);
        let draft = state.mcp_add_draft.as_ref().expect("draft still open");
        assert_eq!(draft.id_error.as_deref(), Some("Id must not contain ':'"));

        // Fixing everything lets the add land.
        let _ = state.update(Message::McpAddIdChanged("github".into()));
        let _ = state.update(Message::McpAddCommandChanged("npx".into()));
        let _ = state.update(Message::McpAddTimeoutChanged("300".into()));
        let _ = state.update(Message::McpAddSaved);
        assert!(state.mcp_add_draft.is_none());
        assert_eq!(state.mcp_servers.len(), 2);
        assert_eq!(state.mcp_servers[1].timeout_secs, Some(300));
        assert!(state.settings_dirty);
    }

    #[test]
    fn mcp_add_blank_timeout_and_empty_env_save_as_defaults() {
        let mut state = State::from_config(&AppConfig::default());
        let _ = state.update(Message::McpAddPressed);
        let _ = state.update(Message::McpAddIdChanged("local".into()));
        let _ = state.update(Message::McpAddCommandChanged("uvx".into()));
        // Blank timeout (valid: uses the crate's 60s default) and no env rows.

        let _ = state.update(Message::McpAddSaved);

        assert!(state.mcp_add_draft.is_none());
        assert_eq!(state.mcp_servers.len(), 1);
        let added = &state.mcp_servers[0];
        assert_eq!(added.id, "local");
        assert_eq!(added.timeout_secs, None, "blank timeout saves as the crate default");
        assert_eq!(added.env, None, "an empty env map saves as None");
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

    // ---------------------------------------------------------------------
    // Plugins tab (ADR-37) — transient view state, never persisted.
    // ---------------------------------------------------------------------

    #[test]
    fn plugin_install_typed_path_updates_state() {
        let mut state = State::from_config(&AppConfig::default());
        let _ = state.update(Message::PluginInstallPathChanged("/opt/x.wasm".into()));
        assert_eq!(state.plugin_install_path, "/opt/x.wasm");
    }

    #[test]
    fn plugin_install_blank_path_reports_inline_error() {
        let mut state = State::from_config(&AppConfig::default());
        let _ = state.update(Message::PluginInstallPressed);
        assert_eq!(
            state.plugin_install_result.as_deref(),
            Some("Enter a .wasm path or use Browse….")
        );
    }

    #[test]
    fn plugin_install_pressed_is_gated_by_action_busy() {
        let mut state = State::from_config(&AppConfig::default());
        state.plugin_install_path = "/opt/x.wasm".into();
        state.plugin_action_busy = true;
        let _ = state.update(Message::PluginInstallPressed);
        assert!(state.plugin_action_busy, "the single-flight gate must hold");
        assert!(
            state.plugin_install_result.is_none(),
            "no second install may start while one is in flight"
        );
    }

    #[test]
    fn plugin_browse_cancel_clears_picker_busy() {
        let mut state = State::from_config(&AppConfig::default());
        state.plugin_picker_busy = true;
        let _ = state.update(Message::PluginBrowsePicked(None));
        assert!(!state.plugin_picker_busy);
        assert!(state.plugin_install_path.is_empty());
    }

    #[test]
    fn plugin_delete_confirm_arms_and_cancels() {
        let mut state = State::from_config(&AppConfig::default());
        let _ = state.update(Message::PluginDeletePressed("alpha".into()));
        assert_eq!(state.plugin_delete_confirm.as_deref(), Some("alpha"));

        // A cancel for a different plugin id leaves the pending confirm alone.
        let _ = state.update(Message::PluginDeleteCancelled("other".into()));
        assert_eq!(state.plugin_delete_confirm.as_deref(), Some("alpha"));

        let _ = state.update(Message::PluginDeleteCancelled("alpha".into()));
        assert_eq!(state.plugin_delete_confirm, None);
    }

    #[test]
    fn plugins_tab_select_launches_lazy_list_refresh_once() {
        let mut state = State::from_config(&AppConfig::default());
        assert!(!state.plugins_loaded, "the plugin list starts unread");
        assert!(!state.plugins_loading);

        let _ = state.update(Message::ExtensionTabSelected(ExtensionTab::Plugins));
        assert!(state.plugins_loading, "the first open must start a directory scan");
        assert_eq!(state.active_extension_tab, ExtensionTab::Plugins);

        // Re-selecting the tab while a scan is in flight is single-flighted.
        let _ = state.update(Message::ExtensionTabSelected(ExtensionTab::Plugins));
        assert!(state.plugins_loading);
    }

    #[test]
    fn plugin_selection_clears_stale_results() {
        let mut state = State::from_config(&AppConfig::default());
        state.plugin_install_result = Some("stale install note".into());
        state.plugin_delete_confirm = Some("stale id".into());
        let _ = state.update(Message::ExtensionItemSelected(ExtensionTab::Plugins, "beta".into()));
        assert_eq!(state.ext_selected_plugin.as_deref(), Some("beta"));
        assert!(
            state.plugin_install_result.is_none(),
            "a revoke/delete outcome belongs to the previously selected plugin"
        );
        assert!(state.plugin_delete_confirm.is_none());
    }
}
