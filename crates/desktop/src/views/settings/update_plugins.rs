//! Plugin update arms (Settings → Extensions → Plugins).
//!
//! Pure relocation (NORM S34): the 15-arm Plugins group of
//! [`super::state::State::update`] moves verbatim from
//! `views/settings/state.rs` into this file as a second inherent `impl State`
//! block — the same pattern as [`super::update_mcp`], [`super::update_skills`],
//! [`super::update_providers`], [`super::shell`], and [`super::state_sync`].
//!
//! The group is the ADR-37 plugin grant lifecycle, the install / replace /
//! delete draft flows, and the directory re-scan result. No behavior,
//! signature, call-site, or [`super::Message`] shape change: the parent
//! `update` keeps a single thin delegating arm that forwards every
//! `Message::Plugin*` variant here.
//!
//! `update_plugins` takes the full [`super::Message`] (not a sub-enum) and
//! returns `iced::Task<Message>` so the relocated arms keep their exact
//! early-return `Task` semantics (the revoke/install/delete `Task::perform`s
//! and the many `Task::none()` bail-outs). As in the parent, the `match` is a
//! statement and the function tail is `iced::Task::none()`. The fallback arm
//! is unreachable through the parent dispatcher and is a documented no-op.

use super::{Message, State};

impl State {
    /// Handle the plugin `Message` group of Settings.
    ///
    /// The parent `State::update` routes every `Message::Plugin*` variant here.
    /// Returns the arm's `Task` unchanged; a non-plugin message (never routed
    /// by the parent) is a no-op.
    pub(super) fn update_plugins(&mut self, message: Message) -> iced::Task<Message> {
        match message {
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
            _ => {}
        }
        iced::Task::none()
    }
}
