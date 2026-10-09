//! Config/model sync helpers for [`App`] — NORM slice S47.
//!
//! This module owns two adjacent helper clusters moved verbatim from
//! `app.rs`: the config-sync seam (memory enable sync, the muted-agent
//! seed/persist pair, and the reload/re-derive chain
//! `reconcile_config_from_reload` → `refresh_project_orchestration_keys` /
//! `apply_reloaded_config` → `refresh_effective_roots_from_config`) and the
//! model/provider cluster (`resolve_default_model`,
//! `persist_active_model_selection`, `set_agent_model`,
//! `sync_chat_model_options`, `discover_unfetched_models`,
//! `fetch_models_for_provider`). Every body is line-for-line with its origin
//! at the same 4-space `impl` indent; the only structural edit is each
//! method becoming `pub(super)`, which keeps the existing callers — the
//! `App::new` boot batch, the sibling submodule groups, and the tests in
//! `app.rs`'s `mod tests` — resolving through `self.*` / `app.*` unchanged
//! (the same visibility pattern as the sibling group modules). No `Message` /
//! `App` shape change and no behavior change.
//!
//! Left in `app.rs` deliberately: the spend/cap status-bar helpers
//! (`sync_session_cap_from_config`, `reset_spend_state`,
//! `reconcile_cap_state`) and `refresh_plugin_providers` — none of them is a
//! config/model sync seam.

use super::*;

impl App {
    /// Resolve the default chat model for the active provider.
    ///
    /// Under Option-1 models live on agent role assignments, not on providers,
    /// so we prefer the model assigned to a role that targets the active
    /// provider. Falls back to the first available model option.
    pub(super) fn resolve_default_model(&self) -> String {
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

    pub(super) fn persist_active_model_selection(&mut self) {
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
    pub(super) fn set_agent_model(&mut self, agent_id: String, model: String) {
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
    pub(super) fn sync_chat_model_options(&mut self) {
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
    pub(super) fn discover_unfetched_models(&mut self) -> iced::Task<Message> {
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

    pub(super) fn fetch_models_for_provider(
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

    pub(super) fn sync_memory_configuration(&mut self) {
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
    pub(super) fn seed_muted_agents(&mut self) {
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
    pub(super) fn persist_muted_agents(&mut self) {
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
    pub(super) fn reconcile_config_from_reload(&mut self) {
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
    pub(super) fn refresh_project_orchestration_keys(&mut self) {
        let project_config =
            self.project_dir.join(concerto_config::legacy::NEW_PROJECT_CONFIG_FILE);
        self.project_orchestration_keys =
            concerto_config::declared_project_orchestration_keys(&project_config);
    }

    /// Apply already-parsed configs: equality short-circuit plus the full
    /// re-derivation. Split out from [`Self::reconcile_config_from_reload`]
    /// so the derivation is testable without touching the disk.
    pub(super) fn apply_reloaded_config(
        &mut self,
        reloaded_global: AppConfig,
        reloaded: AppConfig,
    ) {
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
    pub(super) fn refresh_effective_roots_from_config(&mut self) {
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
}
