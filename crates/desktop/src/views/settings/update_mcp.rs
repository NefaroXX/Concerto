//! MCP server update arms (Settings → Extensions → MCP).
//!
//! Pure relocation (NORM S28): the 30-arm MCP group of
//! [`super::state::State::update`] moves verbatim from
//! `views/settings/state.rs` into this file as a second inherent `impl State`
//! block — the same pattern as [`super::shell`] and [`super::state_sync`].
//! The group is the master MCP enable flag, per-server enable/probe, and the
//! ADR-43 edit/add/delete draft lifecycles. No behavior, signature, call-site,
//! or [`super::Message`] shape change: the parent `update` keeps a single thin
//! delegating arm that forwards every `Message::Mcp*` variant here.
//!
//! `update_mcp` takes the full [`super::Message`] (not a sub-enum) and returns
//! `iced::Task<Message>` so the relocated arms keep their exact early-return
//! `Task` semantics (the probe/credential-store `Task::perform`s and the many
//! `Task::none()` bail-outs). The fallback arm is unreachable through the
//! parent dispatcher and is a documented no-op.
//!
//! Every member is `pub(super)`: from this child module `super` is
//! `views::settings`, so `pub(super)` resolves to `views::settings` + its
//! descendants — exactly the effective scope the private items in `state.rs`
//! had. The state.rs MCP round-trip tests stay put (they drive the public
//! `State::update` dispatch) and keep asserting the same strings.

use std::collections::BTreeMap;

use concerto_config::McpServerConfig;

use super::state_mcp_validate::parse_mcp_args;
use super::{McpAddDraft, McpEditDraft, Message, State};

impl State {
    /// Handle the MCP-server `Message` group of Settings.
    ///
    /// The parent `State::update` routes every `Message::Mcp*` variant here.
    /// Returns the arm's `Task` unchanged; a non-MCP message (never routed by
    /// the parent) is a no-op.
    pub(super) fn update_mcp(&mut self, message: Message) -> iced::Task<Message> {
        match message {
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
                if !self.mcp_enabled || self.mcp_probing.contains(&id) {
                    return iced::Task::none();
                }
                let Some(server) = self.mcp_servers.iter().find(|server| server.id == id).cloned()
                else {
                    return iced::Task::none();
                };
                if !server.enabled {
                    return iced::Task::none();
                }
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
                    args: serde_json::to_string(&server.args).unwrap_or_else(|_| "[]".into()),
                    args_error: None,
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
                    draft.args_error = None;
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
            Message::McpCredentialStore(editing, key) => {
                let (id, env) = if editing {
                    let Some(id) = self.mcp_editing_id.clone() else {
                        return iced::Task::none();
                    };
                    let Some(draft) = self.mcp_edit_draft.as_mut() else {
                        return iced::Task::none();
                    };
                    (id, &mut draft.env)
                } else {
                    let Some(draft) = self.mcp_add_draft.as_mut() else {
                        return iced::Task::none();
                    };
                    if Self::validate_mcp_add_id(&self.mcp_servers, draft.id.trim()).is_some() {
                        draft.id_error = Some(
                            "Enter a valid unique server id before storing credentials".into(),
                        );
                        return iced::Task::none();
                    }
                    (draft.id.trim().to_owned(), &mut draft.env)
                };
                let Some(value) = env.get_mut(&key) else {
                    return iced::Task::none();
                };
                if key.trim().is_empty() || value.is_empty() || value.starts_with("keyring:") {
                    return iced::Task::none();
                }
                let account = format!("mcp/{id}/{key}");
                let secret = concerto_core::SecretString::from(std::mem::replace(
                    value,
                    format!("keyring:{account}"),
                ));
                self.mcp_credentials_pending += 1;
                return iced::Task::perform(
                    super::helpers::store_mcp_credential(account, secret),
                    move |result| Message::McpCredentialStored(editing, result),
                );
            }
            Message::McpCredentialStored(editing, result) => {
                self.mcp_credentials_pending = self.mcp_credentials_pending.saturating_sub(1);
                let error = result.err();
                if editing {
                    if let Some(draft) = self.mcp_edit_draft.as_mut() {
                        draft.env_error = error;
                    }
                } else if let Some(draft) = self.mcp_add_draft.as_mut() {
                    draft.env_error = error;
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
                    draft.args_error = parse_mcp_args(&draft.args).err();
                    draft.timeout_error = Self::validate_mcp_timeout(&draft.timeout);
                }
            }
            Message::McpEditSaved => {
                if self.mcp_credentials_pending > 0 {
                    return iced::Task::none();
                }
                let Some(id) = self.mcp_editing_id.clone() else {
                    return iced::Task::none();
                };
                let Some(mut draft) = self.mcp_edit_draft.take() else {
                    return iced::Task::none();
                };
                // Inline validation: keep the user in edit mode with the
                // errors visible; only a valid draft is applied.
                draft.command_error = Self::validate_mcp_command(&draft.command);
                draft.env_error =
                    McpServerConfig::validate_env(&draft.env).err().map(|e| e.to_string());
                draft.args_error = parse_mcp_args(&draft.args).err();
                draft.timeout_error = Self::validate_mcp_timeout(&draft.timeout);
                if draft.command_error.is_some()
                    || draft.args_error.is_some()
                    || draft.env_error.is_some()
                    || draft.timeout_error.is_some()
                {
                    self.mcp_edit_draft = Some(draft);
                    return iced::Task::none();
                }
                if let Some(server) = self.mcp_servers.iter_mut().find(|server| server.id == id) {
                    server.command = draft.command.trim().to_string();
                    server.args = parse_mcp_args(&draft.args).unwrap_or_default();
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
                    args: "[]".into(),
                    args_error: None,
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
                    draft.id_error = Self::validate_mcp_add_id(&self.mcp_servers, draft.id.trim());
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
                    draft.args_error = None;
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
                    draft.args_error = parse_mcp_args(&draft.args).err();
                    draft.timeout_error = Self::validate_mcp_timeout(&draft.timeout);
                }
            }
            Message::McpAddSaved => {
                if self.mcp_credentials_pending > 0 {
                    return iced::Task::none();
                }
                let Some(mut draft) = self.mcp_add_draft.take() else {
                    return iced::Task::none();
                };
                // Inline validation mirrors `McpConfig::validate` plus the
                // edit draft's command/env/timeout rules. Keep the form open
                // with the errors visible; only a valid draft is applied.
                draft.id_error = Self::validate_mcp_add_id(&self.mcp_servers, draft.id.trim());
                draft.command_error = Self::validate_mcp_command(&draft.command);
                draft.env_error =
                    McpServerConfig::validate_env(&draft.env).err().map(|e| e.to_string());
                draft.args_error = parse_mcp_args(&draft.args).err();
                draft.timeout_error = Self::validate_mcp_timeout(&draft.timeout);
                if draft.id_error.is_some()
                    || draft.command_error.is_some()
                    || draft.args_error.is_some()
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
                    args: parse_mcp_args(&draft.args).unwrap_or_default(),
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
            _ => {}
        }
        iced::Task::none()
    }
}
