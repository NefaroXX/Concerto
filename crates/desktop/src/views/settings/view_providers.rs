//! Providers section view builders (Settings → Providers & Credentials).
//!
//! Pure relocation (NORM S50): the Providers cluster of the settings page
//! moves verbatim from `views/settings/mod.rs` into this file as an inherent
//! `impl State` block — the same pattern as [`super::shell`] (`shell_section`)
//! and the `ext_*_view` tab builders. The cluster is the empty-state
//! guidance line, the per-provider rows (readiness / credential / delete
//! confirm), the inline API-key edit row, the per-provider model-list refresh
//! control, and the add-provider form. No behavior, signature, call-site, or
//! [`super::Message`] shape change: the parent `view` keeps wrapping the
//! returned content in its `collapsible_section` card exactly as before.
//!
//! The shared items (`readable_provider_label`, `PROVIDER_TYPES`) stay in
//! `mod.rs` and enter this file through `use super::…`, so the Default Model
//! picker and the config-sync helpers keep rendering from a single definition.
//! Every member is `pub(super)`: from this child module `super` is
//! `views::settings`, so `pub(super)` resolves to `views::settings` plus its
//! descendants — exactly the effective scope a private item in `mod.rs` had.
//! The full-page render tests stay in `mod.rs` (they exercise the whole
//! `view`, not this builder alone).

use iced::widget::{button, column, container, pick_list, row, text, text_input, tooltip};
use iced::{Element, Length};

use concerto_providers::provider_defs::{
    provider_definition, provider_readiness, CredentialRequirement, ProviderReadiness,
};

use crate::theme::AppTheme;
use crate::ui::{form_field, padded, SPACING_SM, SPACING_XS};

use super::{readable_provider_label, Message, State, PROVIDER_TYPES};

impl State {
    /// Content of the Providers & Credentials section: the configured-provider
    /// cards, the inline add-provider form, and the "+ Add Provider" control.
    /// The caller (`view`) wraps this in the `collapsible_section` card.
    pub(super) fn provider_section<'a>(&'a self, theme: &'a AppTheme) -> Element<'a, Message> {
        let palette = &theme.palette;

        let mut provider_items: Vec<Element<'_, Message>> = Vec::new();

        // Empty state: guide the user when nothing is configured yet.
        if self.providers.is_empty() && !self.show_form {
            provider_items.push(
                text("No providers configured. Add one to connect to an LLM service like OpenAI, Anthropic, or a local model.")
                    .size(13)
                    .color(palette.text_muted)
                    .into(),
            );
        }

        for (i, prov) in self.providers.iter().enumerate() {
            let palette = &theme.palette;
            let def = provider_definition(&prov.provider);
            let creds = concerto_config::CredentialStore::new();
            let has_key = creds.exists(&prov.keyring_key);

            let provider_label = readable_provider_label(prov);

            // Model selection moved to the Assignments section (unified
            // "Model — Provider" picker). Provider rows no longer hold a model.

            // Readiness indicator — reflects credential/endpoint health. Providers
            // no longer carry a model (models are assigned per role), so we do not
            // surface a "MissingModel" state here.
            let readiness_text: Element<'_, Message> =
                if !has_key && def.credential_requirement == CredentialRequirement::Required {
                    text("Add an API key").size(12).color(palette.warning).into()
                } else if let ProviderReadiness::InvalidEndpoint(_) =
                    provider_readiness(prov, &def, has_key)
                {
                    text("Invalid API base URL").size(12).color(palette.danger).into()
                } else {
                    text("Ready").size(12).color(palette.text_muted).into()
                };

            // Credential indicator: distinguish required / optional / keyless.
            let key_text: Element<'_, Message> = match def.credential_requirement {
                CredentialRequirement::None => {
                    text("Local / no key").size(12).color(palette.text_muted).into()
                }
                CredentialRequirement::Required => {
                    if has_key {
                        text("Key: stored").size(12).color(palette.success).into()
                    } else {
                        text("Key: required").size(12).color(palette.warning).into()
                    }
                }
                CredentialRequirement::Optional => {
                    if has_key {
                        text("Key: stored").size(12).color(palette.success).into()
                    } else {
                        text("Key: optional").size(12).color(palette.text_muted).into()
                    }
                }
                _ => text("Unknown").size(12).color(palette.text_muted).into(),
            };

            // Deletion is destructive (removes the provider AND its keyring
            // key), so the first press arms a confirm prompt instead of
            // deleting immediately (plan §5.3 — explicit, confirmed delete).
            let delete_control: Element<'_, Message> = if self.confirm_delete_for == Some(i) {
                row![
                    text("Confirm delete?").size(12).color(palette.warning),
                    button(text("Confirm").size(13))
                        .style(crate::ui::button::secondary)
                        .padding([6, 14])
                        .on_press(Message::ProviderDeleteConfirmed(i)),
                    button(text("Cancel").size(13))
                        .style(crate::ui::button::secondary)
                        .padding([6, 14])
                        .on_press(Message::ProviderDeleteCancelled(i)),
                ]
                .spacing(SPACING_XS)
                .align_y(iced::Alignment::Center)
                .into()
            } else {
                button(text("X").size(13))
                    .style(crate::ui::button::secondary)
                    .padding([6, 14])
                    .on_press(Message::ProviderDeletePressed(i))
                    .into()
            };

            let row_content = row![
                text(provider_label).size(14).width(Length::FillPortion(2)),
                readiness_text,
                key_text,
                delete_control,
            ]
            .spacing(SPACING_SM)
            .align_y(iced::Alignment::Center);

            provider_items.push(row_content.into());

            // Plan §5.3 — inline credential edit for an existing provider.
            let key_edit_control: Element<'_, Message> = if self.editing_key_for == Some(i) {
                let input = text_input("New API key", &self.key_edit_text)
                    .on_input(Message::FormKeyEditTextChanged)
                    .secure(true)
                    .width(Length::Fill);
                if self.confirm_clear_for == Some(i) {
                    let confirm_row: Element<'_, Message> = row![
                        text("Clear key?").size(12).color(palette.warning),
                        button(text("Confirm").size(13))
                            .style(crate::ui::button::secondary)
                            .padding([6, 14])
                            .on_press(Message::FormClearKeyConfirmed(i)),
                        button(text("Cancel").size(13))
                            .style(crate::ui::button::secondary)
                            .padding([6, 14])
                            .on_press(Message::FormClearKey(i)),
                    ]
                    .spacing(SPACING_XS)
                    .align_y(iced::Alignment::Center)
                    .into();
                    row![
                        input,
                        button(text("Save").size(13))
                            .style(crate::ui::button::secondary)
                            .padding([6, 14])
                            .on_press(Message::FormSaveKey(i)),
                        confirm_row,
                        button(text("Close").size(13))
                            .style(crate::ui::button::secondary)
                            .padding([6, 14])
                            .on_press(Message::FormKeyEditCancel(i)),
                    ]
                    .spacing(SPACING_XS)
                    .align_y(iced::Alignment::Center)
                    .into()
                } else {
                    row![
                        input,
                        button(text("Save").size(13))
                            .style(crate::ui::button::secondary)
                            .padding([6, 14])
                            .on_press(Message::FormSaveKey(i)),
                        button(text("Clear").size(13))
                            .style(crate::ui::button::secondary)
                            .padding([6, 14])
                            .on_press(Message::FormClearKey(i)),
                        button(text("Close").size(13))
                            .style(crate::ui::button::secondary)
                            .padding([6, 14])
                            .on_press(Message::FormKeyEditCancel(i)),
                    ]
                    .spacing(SPACING_XS)
                    .align_y(iced::Alignment::Center)
                    .into()
                }
            } else {
                button(text("Edit Key").size(13))
                    .style(crate::ui::button::secondary)
                    .padding([6, 14])
                    .on_press(Message::FormEditKeyPressed(i))
                    .into()
            };
            let key_edit_row =
                row![text("API key:").size(12).color(palette.text_muted), key_edit_control,]
                    .spacing(SPACING_XS)
                    .align_y(iced::Alignment::Center);
            provider_items.push(key_edit_row.into());

            // Manual model-list refresh: re-runs discovery so newly released
            // models appear without editing config or restarting. Only shown
            // for providers that support discovery at all.
            if def.supports_discovery() {
                let refreshing = self.refreshing_providers.contains(&prov.id);
                let refresh_button = if refreshing {
                    // In flight: inert button, same disabled pattern as the
                    // skills section's "Discovering…".
                    button(text("Refreshing…").size(13))
                        .style(crate::ui::button::secondary)
                        .padding([6, 14])
                } else {
                    button(text("Refresh").size(13))
                        .style(crate::ui::button::secondary)
                        .padding([6, 14])
                        .on_press(Message::ProviderModelsRefreshRequested(prov.id.clone()))
                };
                let refresh_control: Element<'_, Message> = tooltip::Tooltip::new(
                    refresh_button,
                    container(text("Refresh model list from provider").size(12)).padding(8),
                    tooltip::Position::Top,
                )
                .gap(4)
                .into();
                let freshness = match prov.cached_models_age() {
                    Some(age) => format!("{} models · updated {age}", prov.cached_model_count()),
                    None => "model list not fetched yet".to_string(),
                };
                let mut model_row = row![
                    text("Model list:").size(12).color(palette.text_muted),
                    text(freshness).size(12).color(palette.text_muted),
                    refresh_control,
                ]
                .spacing(SPACING_XS)
                .align_y(iced::Alignment::Center);
                if let Some(error) = self.provider_refresh_errors.get(&prov.id) {
                    model_row = model_row.push(text(error.clone()).size(12).color(palette.danger));
                }
                provider_items.push(model_row.into());
            }
        }

        // Add provider form
        if self.show_form {
            let type_pick =
                pick_list(PROVIDER_TYPES, Some(self.form_provider_type.as_str()), |s| {
                    Message::FormProviderTypeChanged(s.to_string())
                });
            let type_row = form_field(theme, "Type", false, None::<&str>, None::<&str>, type_pick);

            let form_def = provider_definition(&self.form_provider_type);

            // Model selection happens per agent role (Assignments section), so the
            // Add Provider form intentionally has no model field.

            let mut form_col = column![
                type_row,
                form_field(
                    theme,
                    "Display Name",
                    true,
                    None::<&str>,
                    None::<&str>,
                    text_input("Display name", &self.form_name)
                        .on_input(Message::FormNameChanged)
                        .width(Length::Fill)
                ),
                form_field(
                    theme,
                    "API Base URL",
                    false,
                    Some("Optional custom endpoint"),
                    None::<&str>,
                    text_input("API base URL (optional)", &self.form_api_base)
                        .on_input(Message::FormApiBaseChanged)
                        .width(Length::Fill)
                ),
            ]
            .spacing(SPACING_XS);

            // API key only when the provider type requires a credential.
            if form_def.credential_requirement != CredentialRequirement::None {
                let key_row = form_field(
                    theme,
                    "API Key",
                    false,
                    Some("Stored securely in OS keychain"),
                    None::<&str>,
                    text_input("API key", &self.form_api_key)
                        .on_input(Message::FormApiKeyChanged)
                        .secure(true)
                        .width(Length::Fill),
                );
                form_col = form_col.push(key_row);
            }

            form_col = form_col.push(
                row![
                    button(text("Add Provider").size(13))
                        .style(crate::ui::button::secondary)
                        .padding([6, 14])
                        .on_press(Message::FormConfirmAdd),
                    button(text("Cancel").size(13))
                        .style(crate::ui::button::secondary)
                        .padding([6, 14])
                        .on_press(Message::FormCancel),
                ]
                .spacing(SPACING_SM),
            );

            provider_items.push(padded(8.0, form_col));
        }

        let add_btn: Element<'_, Message> = if !self.show_form {
            button(text("+ Add Provider").size(13))
                .style(crate::ui::button::secondary)
                .padding([6, 14])
                .on_press(Message::ProviderAddPressed)
                .into()
        } else {
            container(text("")).height(0).into()
        };

        column![column(provider_items).spacing(SPACING_XS), add_btn,].spacing(SPACING_SM).into()
    }
}
