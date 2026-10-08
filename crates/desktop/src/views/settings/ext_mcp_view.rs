//! MCP tab view builders (Settings → Extensions → MCP).
//!
//! Pure relocation (NORM S22-C): the MCP cluster of the Extensions section
//! moves verbatim from `views/settings/mod.rs` into this file as a second
//! inherent `impl State` block — the same pattern as [`super::shell`] and
//! [`super::state_sync`]. The cluster is the master–detail tab shell
//! (`ext_mcp_tab`), the inert-while-open "Add server" control, the
//! per-server row, the read-only detail pane with test/edit/delete controls,
//! and the edit and add draft forms. No behavior, signature, call-site, or
//! [`super::Message`] shape change: the parent `view` dispatch
//! (`ExtensionTab::Mcp => self.ext_mcp_tab(theme)`) is untouched.
//!
//! Every member is `pub(super)`: from this child module `super` is
//! `views::settings`, so `pub(super)` resolves to `views::settings` + its
//! descendants — exactly the effective scope a private item in `mod.rs`
//! had. The two MCP view smoke tests move with the file (inspector.rs
//! precedent); the state.rs MCP round-trip tests stay put and keep asserting
//! the same strings.

use iced::widget::{button, checkbox, column, row, text, text_input};
use iced::{Alignment, Element, Length};

use concerto_config::McpServerConfig;

use crate::theme::AppTheme;
use crate::ui::{form_field, SPACING_SM, SPACING_XS};

use super::{
    ext_master_detail, ext_meta_row, ExtensionTab, McpAddDraft, McpEditDraft, Message, State,
};

impl State {
    // ── MCP tab ──────────────────────────────────────────────────────────
    pub(super) fn ext_mcp_tab<'a>(&'a self, theme: &'a AppTheme) -> Element<'a, Message> {
        let palette = &theme.palette;

        let mut master: Vec<Element<'a, Message>> = vec![
            checkbox(self.mcp_enabled)
                .label("Enable MCP servers")
                .on_toggle(Message::McpEnabledToggled)
                .into(),
            text("Servers start with the next run. Use Test connection for a one-off check.")
                .size(12)
                .color(palette.text_muted)
                .into(),
            row![
                text("Configured servers").size(13).color(palette.text),
                self.ext_new_mcp_server_button(),
            ]
            .spacing(SPACING_SM)
            .align_y(Alignment::Center)
            .into(),
        ];

        if self.mcp_servers.is_empty() {
            master.push(
                text(
                    "No MCP servers configured yet. Press \"Add server\" to create one; it \
                     starts with the next run.",
                )
                .size(12)
                .color(palette.text_muted)
                .into(),
            );
        } else {
            let mut servers: Vec<Element<'a, Message>> = Vec::new();
            for server in &self.mcp_servers {
                servers.push(self.ext_mcp_row(theme, server));
            }
            master.push(column(servers).spacing(SPACING_XS).into());
        }

        ext_master_detail(
            theme,
            column(master).spacing(SPACING_SM).into(),
            if let Some(draft) = &self.mcp_add_draft {
                self.ext_mcp_add_form(theme, draft)
            } else {
                self.ext_mcp_detail(theme)
            },
        )
    }

    /// Master "Add server" action (ADR-43 add): opens the add form in the
    /// detail pane. Inert while the form is already open so the draft is never
    /// reset under the user.
    pub(super) fn ext_new_mcp_server_button<'a>(&'a self) -> Element<'a, Message> {
        let mut button = button(text("Add server").size(13))
            .style(crate::ui::button::secondary)
            .padding([6, 14]);
        if self.mcp_add_draft.is_none() {
            button = button.on_press(Message::McpAddPressed);
        }
        button.into()
    }

    pub(super) fn ext_mcp_row<'a>(
        &'a self,
        theme: &'a AppTheme,
        server: &'a McpServerConfig,
    ) -> Element<'a, Message> {
        let active = self.ext_selected_mcp.as_deref() == Some(server.id.as_str());
        let cmd_display = if server.args.is_empty() {
            server.command.clone()
        } else {
            format!("{} {}", server.command, server.args.join(" "))
        };
        let select_id = server.id.clone();
        let toggle_id = server.id.clone();

        row![
            crate::ui::list_item(
                theme,
                active,
                Message::ExtensionItemSelected(ExtensionTab::Mcp, select_id),
                column![
                    text(server.id.clone()).size(13).color(theme.palette.text),
                    text(cmd_display).size(11).color(theme.palette.text_muted),
                ]
                .spacing(2),
            ),
            checkbox(server.enabled)
                .label("")
                .on_toggle(move |on| Message::McpServerEnabledToggled(toggle_id.clone(), on)),
        ]
        .spacing(SPACING_XS)
        .align_y(Alignment::Center)
        .into()
    }

    pub(super) fn ext_mcp_detail<'a>(&'a self, theme: &'a AppTheme) -> Element<'a, Message> {
        let palette = &theme.palette;

        let Some(server) = self
            .mcp_servers
            .iter()
            .find(|server| self.ext_selected_mcp.as_deref() == Some(server.id.as_str()))
        else {
            return column![
                text("No server selected").size(14).color(palette.text),
                text(
                    "Select a server in the list to view command, environment, and timeout \
                     details."
                )
                .size(12)
                .color(palette.text_muted),
            ]
            .spacing(SPACING_SM)
            .into();
        };

        // Edit mode: replace the read-only metadata with the editable form.
        if self.mcp_editing_id.as_deref() == Some(server.id.as_str()) {
            if let Some(draft) = &self.mcp_edit_draft {
                return self.ext_mcp_edit_form(theme, server, draft);
            }
        }

        let args = if server.args.is_empty() { "none".to_string() } else { server.args.join(" ") };
        let env = match &server.env {
            Some(vars) if !vars.is_empty() => {
                let keys: Vec<&str> = vars.keys().map(String::as_str).collect();
                format!("{} (values redacted)", keys.join(", "))
            }
            _ => "none".to_string(),
        };
        let timeout = match server.timeout_secs {
            Some(secs) => format!("{secs}s"),
            None => "default (60s)".to_string(),
        };

        let probing = self.mcp_probing.contains(&server.id);
        let test_button: Element<'a, Message> = if probing {
            button(text("Testing…").size(13))
                .style(crate::ui::button::secondary)
                .padding([6, 14])
                .into()
        } else if !self.mcp_enabled || !server.enabled {
            button(text("Test connection").size(13))
                .style(crate::ui::button::secondary)
                .padding([6, 14])
                .into()
        } else {
            button(text("Test connection").size(13))
                .style(crate::ui::button::secondary)
                .padding([6, 14])
                .on_press(Message::McpProbePressed(server.id.clone()))
                .into()
        };

        let probe_result: Element<'a, Message> = match self.mcp_probe_results.get(&server.id) {
            Some(Ok(tools)) => {
                let names: Vec<&str> = tools.iter().map(|tool| tool.name.as_str()).collect();
                let n = tools.len();
                text(format!(
                    "Connected — {n} tool{s}: {list}",
                    n = n,
                    s = if n == 1 { "" } else { "s" },
                    list = names.join(", "),
                ))
                .size(11)
                .color(palette.success)
                .into()
            }
            Some(Err(error)) => {
                text(format!("Error: {error}")).size(11).color(palette.danger).into()
            }
            None => iced::widget::Space::new().height(0).into(),
        };

        // Deletion is destructive (removes the server from the pending
        // config), so the first press arms a confirm prompt instead of
        // deleting immediately — same explicit-confirm rule as providers.
        let delete_control: Element<'a, Message> =
            if self.mcp_delete_confirm.as_deref() == Some(server.id.as_str()) {
                row![
                    text("Delete this server?").size(12).color(palette.warning),
                    button(text("Confirm").size(13))
                        .style(crate::ui::button::secondary)
                        .padding([6, 12])
                        .on_press(Message::McpDeleteConfirmed(server.id.clone())),
                    button(text("Cancel").size(13))
                        .style(crate::ui::button::secondary)
                        .padding([6, 12])
                        .on_press(Message::McpDeleteCancelled(server.id.clone())),
                ]
                .spacing(SPACING_XS)
                .align_y(Alignment::Center)
                .into()
            } else {
                row![
                    button(text("Edit server").size(13))
                        .style(crate::ui::button::secondary)
                        .padding([6, 14])
                        .on_press(Message::McpEditPressed(server.id.clone())),
                    button(text("Delete server").size(13))
                        .style(crate::ui::button::danger_outline)
                        .padding([6, 14])
                        .on_press(Message::McpDeletePressed(server.id.clone())),
                ]
                .spacing(SPACING_SM)
                .into()
            };

        column![
            text(server.id.clone()).size(16).color(palette.text),
            ext_meta_row(theme, "Command", server.command.as_str()),
            ext_meta_row(theme, "Arguments", args),
            ext_meta_row(theme, "Environment", env),
            ext_meta_row(theme, "Per-call timeout", timeout),
            test_button,
            probe_result,
            delete_control,
        ]
        .spacing(SPACING_SM)
        .into()
    }

    /// Editable form for a selected MCP server (ADR-43 edit/delete). The
    /// stable `id` stays read-only; command, arguments, environment rows, and
    /// timeout are drafted against the server's config and applied by
    /// [`Message::McpEditSaved`] with inline validation on the way out.
    pub(super) fn ext_mcp_edit_form<'a>(
        &'a self,
        theme: &'a AppTheme,
        server: &'a McpServerConfig,
        draft: &'a McpEditDraft,
    ) -> Element<'a, Message> {
        let palette = &theme.palette;

        let mut env_rows: Vec<Element<'a, Message>> = Vec::new();
        let keys: Vec<String> = draft.env.keys().cloned().collect();
        for (i, key) in keys.iter().enumerate() {
            let value = draft.env.get(key).cloned().unwrap_or_default();
            env_rows.push(
                row![
                    text_input("VAR", key)
                        .on_input(move |s| Message::McpEditEnvKeyChanged(i, s))
                        .width(160),
                    text_input("value", &value)
                        .on_input(move |s| Message::McpEditEnvValueChanged(i, s))
                        .secure(!value.starts_with("keyring:"))
                        .width(Length::Fill),
                    button(text("Remove").size(12))
                        .style(crate::ui::button::danger_outline)
                        .padding([6, 10])
                        .on_press(Message::McpEditEnvRemove(i)),
                    button(text("Store secret").size(12))
                        .style(crate::ui::button::secondary)
                        .padding([6, 10])
                        .on_press(Message::McpCredentialStore(true, key.clone())),
                ]
                .spacing(SPACING_XS)
                .align_y(Alignment::Center)
                .into(),
            );
        }

        let mut env_field: Vec<Element<'a, Message>> = vec![
            text("Environment").size(13).color(palette.text).into(),
            text("Only launch variables are inherited. Store tokens in the keychain; config keeps keyring: references.")
                .size(11)
                .color(palette.text_muted)
                .into(),
        ];
        if env_rows.is_empty() {
            env_field.push(text("No variables set.").size(12).color(palette.text_muted).into());
        } else {
            env_field.extend(env_rows);
        }
        if let Some(error) = &draft.env_error {
            env_field.push(text(error.clone()).size(11).color(palette.danger).into());
        }
        env_field.push(
            button(text("+ Add variable").size(13))
                .style(crate::ui::button::secondary)
                .padding([6, 12])
                .on_press(Message::McpEditEnvAdd)
                .into(),
        );

        column![
            text(format!("{} — edit", server.id)).size(16).color(palette.text),
            text("Enabling or testing this server runs its executable with your user privileges. Trust the command and arguments.").size(12).color(palette.warning),
            form_field(
                theme,
                "Command",
                true,
                None::<&str>,
                draft.command_error.as_deref(),
                text_input("command", &draft.command)
                    .on_input(Message::McpEditCommandChanged)
                    .width(Length::Fill),
            ),
            form_field(
                theme,
                "Arguments",
                false,
                Some("JSON array, e.g. [\"-y\", \"path with spaces\"]"),
                draft.args_error.as_deref(),
                text_input("arguments", &draft.args)
                    .on_input(Message::McpEditArgsChanged)
                    .width(Length::Fill),
            ),
            column(env_field).spacing(SPACING_XS),
            form_field(
                theme,
                "Per-call timeout (seconds)",
                false,
                Some("Blank = crate default (60s); hard cap 300s"),
                draft.timeout_error.as_deref(),
                text_input("blank = 60", &draft.timeout)
                    .on_input(Message::McpEditTimeoutChanged)
                    .width(Length::Fill),
            ),
            row![
                button(text("Save").size(13))
                    .style(crate::ui::button::primary)
                    .padding([6, 14])
                    .on_press_maybe((self.mcp_credentials_pending == 0).then_some(Message::McpEditSaved)),
                button(text("Cancel").size(13))
                    .style(crate::ui::button::secondary)
                    .padding([6, 14])
                    .on_press(Message::McpEditCancelled),
            ]
            .spacing(SPACING_SM),
        ]
        .spacing(SPACING_SM)
        .into()
    }

    /// New-server form for the MCP tab (ADR-43 add), rendered in the detail
    /// pane in place of the read-only metadata. Mirrors the edit form with an
    /// editable `id` field; command, arguments, environment rows, and timeout
    /// are drafted and applied by [`Message::McpAddSaved`] with inline
    /// validation on the way out. Cancelling discards the draft.
    pub(super) fn ext_mcp_add_form<'a>(
        &'a self,
        theme: &'a AppTheme,
        draft: &'a McpAddDraft,
    ) -> Element<'a, Message> {
        let palette = &theme.palette;

        let mut env_rows: Vec<Element<'a, Message>> = Vec::new();
        let keys: Vec<String> = draft.env.keys().cloned().collect();
        for (i, key) in keys.iter().enumerate() {
            let value = draft.env.get(key).cloned().unwrap_or_default();
            env_rows.push(
                row![
                    text_input("VAR", key)
                        .on_input(move |s| Message::McpAddEnvKeyChanged(i, s))
                        .width(160),
                    text_input("value", &value)
                        .on_input(move |s| Message::McpAddEnvValueChanged(i, s))
                        .secure(!value.starts_with("keyring:"))
                        .width(Length::Fill),
                    button(text("Remove").size(12))
                        .style(crate::ui::button::danger_outline)
                        .padding([6, 10])
                        .on_press(Message::McpAddEnvRemove(i)),
                    button(text("Store secret").size(12))
                        .style(crate::ui::button::secondary)
                        .padding([6, 10])
                        .on_press(Message::McpCredentialStore(false, key.clone())),
                ]
                .spacing(SPACING_XS)
                .align_y(Alignment::Center)
                .into(),
            );
        }

        let mut env_field: Vec<Element<'a, Message>> = vec![
            text("Environment").size(13).color(palette.text).into(),
            text("Only launch variables are inherited. Store tokens in the keychain; config keeps keyring: references.")
                .size(11)
                .color(palette.text_muted)
                .into(),
        ];
        if env_rows.is_empty() {
            env_field.push(text("No variables set.").size(12).color(palette.text_muted).into());
        } else {
            env_field.extend(env_rows);
        }
        if let Some(error) = &draft.env_error {
            env_field.push(text(error.clone()).size(11).color(palette.danger).into());
        }
        env_field.push(
            button(text("+ Add variable").size(13))
                .style(crate::ui::button::secondary)
                .padding([6, 12])
                .on_press(Message::McpAddEnvAdd)
                .into(),
        );

        column![
            text("New MCP server").size(16).color(palette.text),
            text("Enabling or testing this server runs its executable with your user privileges. Trust the command and arguments.").size(12).color(palette.warning),
            text(
                "The server is added to the pending config and starts with the next run; \
                 edit and delete stay available from the detail pane."
            )
            .size(12)
            .color(palette.text_muted),
            form_field(
                theme,
                "Id",
                true,
                Some("Tool namespace key: mcp:<id>:<tool>; must be unique and free of ':'"),
                draft.id_error.as_deref(),
                text_input("server id", &draft.id)
                    .on_input(Message::McpAddIdChanged)
                    .width(Length::Fill),
            ),
            form_field(
                theme,
                "Command",
                true,
                None::<&str>,
                draft.command_error.as_deref(),
                text_input("command", &draft.command)
                    .on_input(Message::McpAddCommandChanged)
                    .width(Length::Fill),
            ),
            form_field(
                theme,
                "Arguments",
                false,
                Some("JSON array, e.g. [\"-y\", \"path with spaces\"]"),
                draft.args_error.as_deref(),
                text_input("arguments", &draft.args)
                    .on_input(Message::McpAddArgsChanged)
                    .width(Length::Fill),
            ),
            column(env_field).spacing(SPACING_XS),
            form_field(
                theme,
                "Per-call timeout (seconds)",
                false,
                Some("Blank = crate default (60s); hard cap 300s"),
                draft.timeout_error.as_deref(),
                text_input("blank = 60", &draft.timeout)
                    .on_input(Message::McpAddTimeoutChanged)
                    .width(Length::Fill),
            ),
            row![
                button(text("Add server").size(13))
                    .style(crate::ui::button::primary)
                    .padding([6, 14])
                    .on_press_maybe((self.mcp_credentials_pending == 0).then_some(Message::McpAddSaved)),
                button(text("Cancel").size(13))
                    .style(crate::ui::button::secondary)
                    .padding([6, 14])
                    .on_press(Message::McpAddCancelled),
            ]
            .spacing(SPACING_SM),
        ]
        .spacing(SPACING_SM)
        .into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test: the MCP detail pane renders in read-only, edit, and
    /// delete-confirm states without panicking (ADR-43 edit/delete).
    #[test]
    fn mcp_detail_renders_readonly_edit_and_delete_confirm_states() {
        let theme = AppTheme::by_name("Midnight");
        let base = concerto_config::AppConfig {
            mcp: Some(concerto_config::McpConfig {
                enabled: true,
                servers: vec![McpServerConfig {
                    id: "files".into(),
                    command: "npx".into(),
                    args: vec!["-y".into()],
                    env: Some([("TOKEN".into(), "secret".into())].into()),
                    enabled: true,
                    timeout_secs: Some(30),
                }],
            }),
            ..concerto_config::AppConfig::default()
        };
        let mut state = State::from_config(&base);
        state.ext_selected_mcp = Some("files".into());

        // Read-only detail.
        let _ = state.view(&theme, false);

        // Edit mode.
        let _ = state.update(Message::McpEditPressed("files".into()));
        let _ = state.view(&theme, false);

        // Edit mode with an inline validation error visible.
        let _ = state.update(Message::McpEditCommandChanged("".into()));
        let _ = state.update(Message::McpEditSaved);
        let _ = state.view(&theme, false);

        // Delete-confirm state.
        let _ = state.update(Message::McpDeletePressed("files".into()));
        let _ = state.view(&theme, false);
    }

    /// Smoke test: the MCP tab renders the add form in the detail pane over
    /// both the empty list and the populated list, with inline errors visible,
    /// and returns to the read-only detail after cancel, without panicking
    /// (ADR-43 add).
    #[test]
    fn mcp_tab_renders_add_form_and_empty_state() {
        let theme = AppTheme::by_name("Midnight");
        let mut state = State::from_config(&concerto_config::AppConfig::default());
        let _ = state.update(Message::ExtensionTabSelected(ExtensionTab::Mcp));

        // Empty list with the add form open.
        let _ = state.update(Message::McpAddPressed);
        let _ = state.view(&theme, false);

        // Add form open on top of existing servers.
        state.mcp_servers.push(McpServerConfig {
            id: "files".into(),
            command: "npx".into(),
            args: vec!["-y".into()],
            env: None,
            enabled: true,
            timeout_secs: None,
        });
        let _ = state.update(Message::McpAddPressed);
        let _ = state.view(&theme, false);

        // Add form with an inline validation error visible.
        let _ = state.update(Message::McpAddIdChanged("files".into()));
        let _ = state.update(Message::McpAddSaved);
        let _ = state.view(&theme, false);

        // Cancel returns to the read-only detail.
        let _ = state.update(Message::McpAddCancelled);
        let _ = state.view(&theme, false);
    }
}
