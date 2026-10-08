//! Provider update arms (Settings → Providers).
//!
//! Pure relocation (NORM S33): the 25-arm Providers group of
//! [`super::state::State::update`] moves verbatim from
//! `views/settings/state.rs` into this file as a second inherent `impl State`
//! block — the same pattern as [`super::update_mcp`], [`super::update_skills`],
//! [`super::shell`], and [`super::state_sync`].
//!
//! The group is the legacy single-provider no-ops, the multi-provider add /
//! delete / credential-edit arms, the per-provider model-discovery refresh
//! arms, and the global default model picker. No behavior, signature,
//! call-site, or [`super::Message`] shape change: the parent `update` keeps a
//! single thin delegating arm that forwards every provider / form /
//! global-default variant here.
//!
//! `update_providers` takes the full [`super::Message`] (not a sub-enum) and
//! returns `iced::Task<Message>` so the relocated arms keep their exact
//! early-return `Task` semantics (the keyring credential writes and the
//! `rebuild_cache` paths). As in the parent, the `match` is a statement and the
//! function tail is `iced::Task::none()`. The fallback arm is unreachable
//! through the parent dispatcher and is a documented no-op.

use concerto_config::{DiscoveryOutcome, ProviderConfig};
use concerto_providers::provider_defs::provider_definition;

use super::state::State;
use super::{Message, EMPTY_DISCOVERY_KEPT, EMPTY_DISCOVERY_NO_CACHE};

impl State {
    /// Handle the provider `Message` group of Settings.
    ///
    /// The parent `State::update` routes every legacy provider, `Provider*`,
    /// `Form*`, and `GlobalDefaultModelChanged` variant here. Returns the arm's
    /// `Task` unchanged; a non-provider message (never routed by the parent) is
    /// a no-op.
    pub(super) fn update_providers(&mut self, message: Message) -> iced::Task<Message> {
        match message {
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
                        // `record_discovered_models` owns the no-clobber
                        // contract: an empty (or blank-only) refresh never
                        // overwrites an existing catalog and never advances
                        // the fetch time. Judge emptiness on the same rule it
                        // uses — after trimming — so a blank-only result is
                        // not mistaken for a real discovery.
                        let produced_nothing = models.iter().all(|m| m.id.trim().is_empty());
                        let outcome = self
                            .providers
                            .iter_mut()
                            .find(|p| p.id == provider_id)
                            .map(|p| p.record_discovered_models(models));
                        if produced_nothing {
                            // Tell the user what actually happened instead of
                            // letting the row read as a fresh, empty discovery:
                            // the previous list is kept when there was one,
                            // and the failure is reported either way.
                            let message = if matches!(outcome, Some(DiscoveryOutcome::EmptyIgnored))
                            {
                                EMPTY_DISCOVERY_KEPT
                            } else {
                                EMPTY_DISCOVERY_NO_CACHE
                            };
                            self.provider_refresh_errors
                                .insert(provider_id.clone(), message.to_string());
                        } else {
                            self.provider_refresh_errors.remove(&provider_id);
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
            _ => {}
        }
        iced::Task::none()
    }
}
