//! Plugins tab view builders (Settings → Extensions → Plugins).
//!
//! Pure relocation (NORM S27-B): the Plugins cluster of the Extensions section
//! moves verbatim from `views/settings/mod.rs` into this file as an inherent
//! `impl State` block — the same pattern as [`super::ext_mcp_view`] and
//! [`super::ext_skills_view`]. The cluster is the master–detail tab shell
//! (`ext_plugins_tab`), the typed-path install card, the per-plugin row, and
//! the read-only detail pane with replace / revoke / two-step delete
//! controls. No behavior, signature, call-site, or [`super::Message`] shape
//! change: the parent `view` dispatch
//! (`ExtensionTab::Plugins => self.ext_plugins_tab(theme)`) is untouched.
//!
//! The shared module-level helpers (`ext_meta_row`, `ext_master_detail`) stay
//! in `mod.rs` and enter this file through `use super::…`, so the sibling
//! extension views keep rendering from a single definition. Every member is
//! `pub(super)`: from this child module `super` is `views::settings`, so
//! `pub(super)` resolves to `views::settings` plus its descendants — exactly
//! the effective scope a private item in `mod.rs` had. The Plugins view smoke
//! test lives with the file (`ext_mcp_view` / `ext_skills_view` precedent);
//! the footer and full-page render tests stay in `mod.rs`.

use iced::widget::{button, column, row, text, text_input};
use iced::{Alignment, Element, Length};

use crate::theme::AppTheme;
use crate::ui::{SPACING_SM, SPACING_XS};

use super::{ext_master_detail, ext_meta_row, ExtensionTab, InstalledPluginInfo, Message, State};

impl State {
    // ── Plugins tab (ADR-37) ────────────────────────────────────────────
    pub(super) fn ext_plugins_tab<'a>(&'a self, theme: &'a AppTheme) -> Element<'a, Message> {
        let palette = &theme.palette;

        let header = text(
            "Plugins are loaded from .wasm files in the plugins directory; their capability \
             grants live in the capability store, not in the config file. Install a validated \
             plugin here, replace an installed one, or remove it (grants included).",
        )
        .size(12)
        .color(palette.text_muted);

        let mut master: Vec<Element<'a, Message>> =
            vec![header.into(), self.ext_plugin_install_card()];

        if let Some(ref result) = self.plugin_install_result {
            master.push(
                text(result)
                    .size(11)
                    .color(if result.starts_with("Error") {
                        palette.danger
                    } else {
                        palette.success
                    })
                    .into(),
            );
        }
        if self.plugins_loading {
            master.push(text("Scanning plugins…").size(12).color(palette.text_muted).into());
        }
        if self.installed_plugins.is_empty() {
            if self.plugins_loaded && !self.plugins_loading {
                master.push(
                    text("No plugins installed. Choose a .wasm above to install one.")
                        .size(13)
                        .color(palette.text_muted)
                        .into(),
                );
            }
        } else {
            let rows: Vec<Element<'a, Message>> = self
                .installed_plugins
                .iter()
                .map(|plugin| self.ext_plugin_row(theme, plugin))
                .collect();
            master.push(column(rows).spacing(SPACING_XS).into());
        }

        ext_master_detail(
            theme,
            column(master).spacing(SPACING_SM).into(),
            self.ext_plugin_detail(theme),
        )
    }

    /// Install card: a typed `.wasm` path (or Browse…, which opens the native
    /// file picker) plus an Install button. Validation, capability approval
    /// and the atomic write all run inside a background task gated by
    /// `plugin_action_busy`.
    pub(super) fn ext_plugin_install_card<'a>(&'a self) -> Element<'a, Message> {
        let busy = self.plugin_action_busy || self.plugin_picker_busy;

        let input = text_input("Path to a .wasm plugin…", &self.plugin_install_path)
            .on_input(Message::PluginInstallPathChanged)
            .on_submit(Message::PluginInstallPressed)
            .width(Length::Fill);
        let browse = button(text("Browse…").size(13))
            .style(crate::ui::button::secondary)
            .padding([6, 14])
            .on_press(Message::PluginInstallBrowsePressed);
        let install =
            button(if busy { text("Working…").size(13) } else { text("Install").size(13) })
                .style(crate::ui::button::primary)
                .padding([6, 14])
                .on_press(Message::PluginInstallPressed);

        row![input, browse, install].spacing(SPACING_SM).into()
    }

    pub(super) fn ext_plugin_row<'a>(
        &'a self,
        theme: &'a AppTheme,
        plugin: &'a InstalledPluginInfo,
    ) -> Element<'a, Message> {
        let active = self.ext_selected_plugin.as_deref() == Some(plugin.id.as_str());
        let select_id = plugin.id.clone();

        crate::ui::list_item(
            theme,
            active,
            Message::ExtensionItemSelected(ExtensionTab::Plugins, select_id),
            column![
                row![
                    text(plugin.name.clone()).size(13).color(theme.palette.text),
                    text(format!(" v{}", plugin.version)).size(11).color(theme.palette.text_muted),
                ]
                .spacing(4)
                .align_y(Alignment::Center),
                text(if plugin.load_error.is_some() {
                    "unreadable — cannot load".to_string()
                } else if plugin.capability_summary.is_empty() {
                    format!("{} · no grants", plugin.provides)
                } else {
                    format!("{} · grants: {}", plugin.provides, plugin.capability_summary)
                })
                .size(11)
                .color(theme.palette.text_muted),
            ]
            .spacing(2),
        )
    }

    pub(super) fn ext_plugin_detail<'a>(&'a self, theme: &'a AppTheme) -> Element<'a, Message> {
        let palette = &theme.palette;

        let Some(plugin) = self
            .installed_plugins
            .iter()
            .find(|plugin| self.ext_selected_plugin.as_deref() == Some(plugin.id.as_str()))
        else {
            return column![
                text("No plugin selected").size(14).color(palette.text),
                text("Select an installed plugin to inspect or manage it.")
                    .size(12)
                    .color(palette.text_muted),
            ]
            .spacing(SPACING_SM)
            .into();
        };

        let grants = if plugin.capability_summary.is_empty() {
            "none".to_string()
        } else {
            plugin.capability_summary.clone()
        };

        let mut rows: Vec<Element<'a, Message>> = vec![
            text(plugin.name.clone()).size(16).color(palette.text).into(),
            ext_meta_row(theme, "ID", plugin.id.clone()),
            ext_meta_row(theme, "Version", plugin.version.clone()),
            ext_meta_row(theme, "Binary SHA-256", plugin.binary_hash.clone()),
            ext_meta_row(theme, "Source", plugin.wasm_path.to_string_lossy().into_owned()),
            ext_meta_row(theme, "Description", plugin.description.clone()),
            ext_meta_row(theme, "Provides", plugin.provides.clone()),
            ext_meta_row(theme, "Grants", grants),
        ];
        if let Some(ref load_error) = plugin.load_error {
            rows.push(
                text(format!("Unable to load: {load_error}")).size(11).color(palette.danger).into(),
            );
        }

        let busy = self.plugin_action_busy || self.plugin_picker_busy;
        let mut actions: Vec<Element<'a, Message>> = vec![
            button(if busy { text("Working…").size(13) } else { text("Replace…").size(13) })
                .style(crate::ui::button::secondary)
                .padding([6, 14])
                .on_press(Message::PluginReplacePressed)
                .into(),
            button(text("Revoke grants").size(13))
                .style(crate::ui::button::secondary)
                .padding([6, 14])
                .on_press(Message::PluginRevokePressed(plugin.id.clone()))
                .into(),
        ];
        // Two-step inline delete confirm, mirroring the skill/MCP flows.
        if self.plugin_delete_confirm.as_deref() == Some(plugin.id.as_str()) {
            actions.extend([row![
                text("Delete this plugin and its grants?").size(12).color(palette.danger),
                button(text("Cancel").size(12))
                    .style(crate::ui::button::secondary)
                    .padding([4, 10])
                    .on_press(Message::PluginDeleteCancelled(plugin.id.clone())),
                button(text("Delete").size(12))
                    .style(crate::ui::button::danger)
                    .padding([4, 10])
                    .on_press(Message::PluginDeleteConfirmed(plugin.id.clone())),
            ]
            .spacing(SPACING_SM)
            .into()]);
        } else {
            actions.push(
                button(if busy { text("Working…").size(13) } else { text("Delete").size(13) })
                    .style(crate::ui::button::danger)
                    .padding([6, 14])
                    .on_press(Message::PluginDeletePressed(plugin.id.clone()))
                    .into(),
            );
        }
        rows.push(row(actions).spacing(SPACING_SM).into());

        if let Some(ref result) = self.plugin_revoke_result {
            rows.push(
                text(result)
                    .size(11)
                    .color(if result.starts_with("Error") {
                        palette.danger
                    } else {
                        palette.success
                    })
                    .into(),
            );
        }

        column(rows).spacing(SPACING_SM).into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Smoke test: the Plugins tab renders in every state the builders handle
    /// — unloaded, loaded-but-empty, populated with a selected detail pane,
    /// the armed two-step delete confirm, and the busy/loading lines — without
    /// panicking (ADR-37).
    #[test]
    fn plugins_tab_renders_empty_selected_and_delete_confirm_states() {
        let theme = AppTheme::by_name("Midnight");
        let mut state = State::from_config(&concerto_config::AppConfig::default());
        state.active_extension_tab = ExtensionTab::Plugins;

        // Unloaded (scan not started) and loaded-but-empty states.
        let _ = state.view(&theme, false);
        state.plugins_loaded = true;
        let _ = state.view(&theme, false);

        // Populated with a selected plugin → read-only detail pane.
        state.installed_plugins = vec![InstalledPluginInfo {
            id: "demo".into(),
            name: "Demo".into(),
            version: "0.1.0".into(),
            description: "A demo plugin".into(),
            provides: "tool:demo".into(),
            capability_summary: "fs:read".into(),
            wasm_path: PathBuf::from("/plugins/demo.wasm"),
            binary_hash: "deadbeef".into(),
            load_error: None,
        }];
        state.ext_selected_plugin = Some("demo".into());
        let _ = state.view(&theme, false);

        // An unreadable plugin surfaces its load error in the detail pane.
        state.installed_plugins[0].load_error = Some("invalid wasm".into());
        let _ = state.view(&theme, false);
        state.installed_plugins[0].load_error = None;

        // Two-step delete confirm armed for the selected plugin.
        state.plugin_delete_confirm = Some("demo".into());
        let _ = state.view(&theme, false);

        // Busy install/replace controls plus the inline result + loading lines.
        state.plugin_action_busy = true;
        state.plugins_loading = true;
        state.plugin_install_result = Some("Installed demo.".into());
        let _ = state.view(&theme, false);

        // Revoke result line and an error-flavoured install result.
        state.plugin_revoke_result = Some("Error: no grants to revoke".into());
        state.plugin_install_result = Some("Error: bad wasm".into());
        let _ = state.view(&theme, false);
    }
}
