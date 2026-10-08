//! Config↔form synchronization and derived provider caches (Settings → State).
//!
//! Verbatim relocation of the config-sync cluster from [`super::state`]:
//! seeding the form from a (re)loaded [`AppConfig`]-backed config
//! (`sync_*_from_config`), publishing the form back onto a config
//! (`to_config`), the derived provider/model caches (`rebuild_cache`,
//! `rebuild_cache_with`, `refresh_provider_cache_from_config`,
//! `model_options_with_discovered`, `generate_provider_id`,
//! `normalize_model_settings`), and the plugin-manager/approval handles
//! (`with_plugin_manager`, `with_plugin_approval`, `plugin_action_in_flight`).
//!
//! Every member stays an inherent `impl State` method, so call sites in
//! [`super::state`], the App, and the test modules are unchanged. The
//! previously-private members called from [`super::state`] are `pub(super)`
//! (this module's `super` is `views::settings`, the same scope their
//! `settings::state` definitions were visible in plus its siblings); members
//! used only here stay private. Intentionally NOT here: the `validate_*`
//! thin delegates, the policy/relationship display helpers, the MCP/skill
//! drafts, and `update()` — those are not config sync.

use std::path::PathBuf;

use concerto_config::managed::ManagedRuntimeManager;
use concerto_config::{
    AppConfig, ManagedEnvConfig, McpConfig, PolicyConfig, ProjectContextConfig, ProviderConfig,
    ShellSettings, SkillsConfig,
};
use concerto_providers::provider_defs::picker_model_options;

use super::{readable_provider_label, State, CUSTOM_MODEL_SENTINEL};

impl State {
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

        cfg.policy = Some(PolicyConfig {
            rules: self.policy_rules.clone(),
            time_window: None,
            approval_timeout_secs: None,
        });
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

    pub(super) fn generate_provider_id(&self) -> String {
        format!("prov_{}", concerto_core::ids::Ulid::new())
    }

    /// Model options for a provider: the shared picker resolver (selected /
    /// default / static known models, plus discovered `cached_models` and
    /// config-first `extra_models`) [ADR-57 §3d].
    fn model_options_with_discovered(p: &ProviderConfig) -> Vec<String> {
        picker_model_options(p)
    }

    pub(super) fn rebuild_cache(&mut self) {
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

    pub(super) fn normalize_model_settings(&mut self) {
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
}
