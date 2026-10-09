//! Settings dispatch `App::update()` arm for [`App`] — NORM slice S40.
//!
//! This module owns the root-update `Settings` arm, extracted from the root
//! `update` match in `app.rs`:
//!
//! * [`App::update_settings`] — the `Settings(msg)` arm and its inner match:
//!   `ShellSecurityFinished` (shell-security revision merge through
//!   `apply_reloaded_config`), `SaveSettings` (config persist +
//!   `reconcile_config_from_reload` + `apply_and_save_theme` + studio
//!   `sync_models` + unfetched-model discovery + plugin-provider refresh),
//!   `ProviderModelsRefreshed` (staleness / deleted-provider guards, the
//!   config-side discovery-cache write, and the chat/studio model-option
//!   syncs), `ThemeSelected` / `FontSizeChanged` (immediate apply + persist),
//!   `ProviderModelsRefreshRequested` (the tracked per-provider refresh
//!   kickoff through `fetch_models_for_provider`), and the passthrough
//!   fallback into `settings.update`.
//!
//! Bodies moved verbatim at the same 12-space arm indent, so each arm is
//! line-for-line with its origin; the only structural edit is the group
//! becoming one `pub(super)` method taking the full [`Message`] and
//! re-matching it (the same pattern as `views::settings::update_mcp` and
//! the sibling `simple_updates` / `project_switch` / `delegated_updates` /
//! `event_model_updates` / `view_dispatch` groups), so every arm keeps its
//! exact early-return `Task` semantics. The parent `update` keeps one thin
//! delegating arm; the fallback arm in the method is unreachable through
//! the parent dispatcher and is a documented no-op. No `Message` / `App`
//! shape change and no behavior change: the tests in `app.rs`'s
//! `mod tests` stay put and drive `update()` unchanged.
//!
//! No sub-arm was excluded for entanglement: every arm body touches only
//! settings / config / studio state plus the `pending_refresh` /
//! `refresh_seq` request bookkeeping, and reaches chat / graph / VFS /
//! sessions only through the shared `App` helpers that stay in `app.rs`.

use super::*;

impl App {
    /// The `Settings` dispatch arm (NORM S40): forwards into the child
    /// settings view state and runs the arm-specific App-level follow-up —
    /// the `SaveSettings` persist / reconcile / theme / model-cache
    /// pipeline, the tracked provider-model refresh request/refreshed pair
    /// (staleness guards + the config-side discovery-cache write + chat and
    /// studio model syncs), the immediate theme/font apply-persist pair,
    /// and the shell-security revision merge.
    ///
    /// The parent routes `Settings` here. Returns the arm's `Task`
    /// unchanged; a message the parent never routes here is a documented
    /// no-op.
    pub(super) fn update_settings(&mut self, message: Message) -> iced::Task<Message> {
        match message {
            Message::Settings(msg) => match &msg {
                views::settings::Message::ShellSecurityFinished { result, .. } => {
                    let security = result.as_ref().as_ref().ok().cloned();
                    let task = self.settings.update(msg).map(Message::Settings);
                    if let Some(security) = security.filter(|security| {
                        security.revision >= self.global_config.shell_security.revision
                    }) {
                        let mut global = self.global_config.clone();
                        global.shell_security = security.clone();
                        if let Some(mut config) = self.config.clone() {
                            config.shell_security = security;
                            self.apply_reloaded_config(global, config);
                        }
                    }
                    task
                }

                views::settings::Message::SaveSettings => {
                    let task = self.settings.update(msg).map(Message::Settings);
                    // Prefs decide the theme the UI renders, so assert it onto
                    // the config written here: without this, a save could
                    // persist whatever `display.theme` the file happened to
                    // carry (the UI never reads that key back).
                    let mut base = self.global_config.clone();
                    base.display.theme = Some(self.settings.selected_theme.to_string());
                    let new_config = self.settings.to_config(&base);
                    if let Some(path) = concerto_config::default_config_path() {
                        if let Err(e) = concerto_config::save_config(&new_config, &path) {
                            tracing::error!(error = %e, "failed to save config");
                        } else {
                            // Collapse the reload + full re-derivation onto the
                            // shared helper (ADR-57 §4), so the on-disk file is
                            // re-read and every config-derived field is derived
                            // in exactly one place.
                            self.reconcile_config_from_reload();
                        }
                    }
                    // Persist the picker's theme + font size through the one
                    // shared path (prefs AND config together), so a save can
                    // never write back a stale theme. Runs after the reload so
                    // it re-applies on top of the freshly derived config.
                    self.apply_and_save_theme();
                    // The studio's model cache must reflect any provider/model
                    // changes saved in Settings (add/delete/rename provider,
                    // or model-discovery results).
                    self.orchestration_studio
                        .sync_models(self.settings.cached_models_by_provider());
                    // Populate model lists for any provider added by this save
                    // that has never been fetched, so it is usable immediately
                    // (no manual refresh / restart). Computed before the
                    // immutable plugin-refresh call below.
                    let discovery_task = self.discover_unfetched_models();
                    // Plugin liveness: after a save (which may have added a
                    // provider or dropped a `.wasm` into the search path),
                    // re-discover plugins in the retained manager. Log-only.
                    iced::Task::batch(vec![task, self.refresh_plugin_providers(), discovery_task])
                }
                views::settings::Message::ProviderModelsRefreshed {
                    provider_id,
                    request_id,
                    result,
                } => {
                    // Staleness guard: drop results for superseded requests.
                    let current = self.pending_refresh.get(provider_id).copied();
                    if current != Some(*request_id) {
                        return iced::Task::none();
                    }
                    // Drop results for a provider that was deleted meanwhile.
                    if !self.runtime_providers().iter().any(|p| p.id == *provider_id) {
                        self.pending_refresh.remove(provider_id);
                        self.settings.end_provider_refresh(provider_id);
                        return iced::Task::none();
                    }
                    self.pending_refresh.remove(provider_id);
                    // Write the discovery into the config-side cache too. The
                    // outcome matters here exactly as it does in the Settings
                    // view: an empty refresh must not clobber a good catalog
                    // in the persisted config, and the user has to be told // which happened instead of the row reading as a fresh, empty
                    // discovery.
                    let mut empty_ignored = false;
                    if let (Some(model_settings), Ok(models)) = (
                        self.config.as_mut().and_then(|config| config.model_settings.as_mut()),
                        result,
                    ) {
                        if let Some(provider) =
                            model_settings.providers.iter_mut().find(|p| p.id == *provider_id)
                        {
                            empty_ignored = matches!(
                                provider.record_discovered_models(models.clone()),
                                concerto_config::DiscoveryOutcome::EmptyIgnored
                            );
                        }
                    }
                    if empty_ignored {
                        // Same inline channel the Settings row renders (the
                        // forwarded handler below writes it as well), so the // app layer itself guarantees the "previous list kept"
                        // outcome reaches the user.
                        self.settings.provider_refresh_errors.insert(
                            provider_id.clone(),
                            views::settings::EMPTY_DISCOVERY_KEPT.to_string(),
                        );
                    }
                    let task = self
                        .settings
                        .update(views::settings::Message::ProviderModelsRefreshed {
                            provider_id: provider_id.clone(),
                            request_id: *request_id,
                            result: result.clone(),
                        })
                        .map(Message::Settings);
                    self.sync_chat_model_options();
                    self.orchestration_studio
                        .sync_models(self.settings.cached_models_by_provider());
                    task
                }
                // Theme / font changes apply and persist immediately: the
                // selector must survive a restart without a SaveSettings
                // round-trip (and the picker must show the applied theme).
                views::settings::Message::ThemeSelected(_)
                | views::settings::Message::FontSizeChanged(_) => {
                    let task = self.settings.update(msg).map(Message::Settings);
                    self.apply_and_save_theme();
                    task
                }
                views::settings::Message::ProviderModelsRefreshRequested(provider_id) => {
                    // Manual per-provider model refresh. Only live rows whose
                    // provider type actually supports discovery get a tracked
                    // request; anything else (stale id, deleted mid-flight) is
                    // a silent no-op.
                    let supported = self.runtime_providers().iter().any(|p| {
                        p.id == *provider_id
                            && provider_definition(&p.provider).supports_discovery()
                    });
                    if !supported {
                        return iced::Task::none();
                    }
                    self.settings.begin_provider_refresh(provider_id);
                    let provider_id = provider_id.clone();
                    self.refresh_seq = self.refresh_seq.wrapping_add(1);
                    let req_id = self.refresh_seq;
                    self.pending_refresh.insert(provider_id.clone(), req_id);
                    self.fetch_models_for_provider(provider_id, req_id)
                }
                _ => self.settings.update(msg).map(Message::Settings),
            },
            // Never routed by the parent dispatcher; documented no-op.
            _ => iced::Task::none(),
        }
    }
}
