use iced::widget::{
    button, checkbox, column, container, pick_list, row, scrollable, text, text_input, tooltip,
};
use iced::{Alignment, Background, Border, Element, Length};
use std::fmt;

use concerto_config::shell::WorkingDirBehavior;
use concerto_config::ProviderConfig;
use concerto_providers::provider_defs::{
    provider_definition, provider_readiness, CredentialRequirement, ProviderReadiness,
    PROVIDER_TYPE_IDS,
};

use crate::theme::AppTheme;
use crate::ui::{
    form_field, labeled_slider, padded, segmented, Segment, SPACING_MD, SPACING_SM, SPACING_XS,
};

mod ext_mcp_view;
mod ext_plugins_view;
mod ext_skills_view;
mod helpers;

pub mod message;
pub mod shell;
pub mod state;
mod state_mcp_validate;
mod state_sync;
mod update_mcp;

pub use message::ExtensionTab;
pub use message::Message;
pub use state::InstalledPluginInfo;
pub use state::McpAddDraft;
pub use state::McpEditDraft;
pub use state::SkillEditDraft;
pub use state::State;

// Use the single source of truth for provider types from provider_defs
const PROVIDER_TYPES: &[&str] = PROVIDER_TYPE_IDS;

/// Sentinel option appended to model pickers to reveal a custom-model text input.
const CUSTOM_MODEL_SENTINEL: &str = "Custom model ID…";

/// Widget id of the Settings main content `scrollable`, targeted by
/// [`Message::JumpToSection`] to scroll a section header into view.
pub(crate) const MAIN_SCROLL_ID: &str = "settings_main_scroll";

/// Inline refresh message when discovery produced nothing while a previous
/// catalog exists: the refresh is reported as a failure and the retained list
/// is named, so "0 models · updated just now" can never be shown as a valid
/// fresh discovery.
pub(crate) const EMPTY_DISCOVERY_KEPT: &str =
    "Discovery returned no models — keeping the previous list. Check credentials/network.";

/// Inline refresh message when discovery produced nothing and there was no
/// previous catalog to keep.
pub(crate) const EMPTY_DISCOVERY_NO_CACHE: &str =
    "Discovery returned no models — check credentials/network.";

/// A selectable parent-directory option in the skill-create wizard (ADR-43).
/// `raw` stays the configured search path (e.g. `~/.config/concerto/skills`);
/// `label` is the resolved absolute path plus an existence badge, so the user
/// picks an actual file path rather than the raw configuration string. The
/// picker's `Display` renders the label.
#[derive(Debug, Clone, PartialEq)]
pub struct CreateParentOption {
    pub(crate) raw: String,
    pub(crate) label: String,
}

impl fmt::Display for CreateParentOption {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.label)
    }
}

fn readable_provider_label(provider: &ProviderConfig) -> String {
    let definition = provider_definition(&provider.provider);
    let configured_name = provider.name.trim();
    if configured_name.is_empty()
        || configured_name.eq_ignore_ascii_case(&provider.provider)
        || configured_name.eq_ignore_ascii_case(&definition.display_name)
    {
        definition.display_name.to_string()
    } else {
        format!("{configured_name} ({})", definition.display_name)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyActionChoice {
    Allow,
    Ask,
    Deny,
}

impl PolicyActionChoice {
    fn config_value(self) -> &'static str {
        match self {
            Self::Allow => "auto_approve",
            Self::Ask => "require_approval",
            Self::Deny => "auto_deny",
        }
    }
}

impl fmt::Display for PolicyActionChoice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Allow => "Allow automatically",
            Self::Ask => "Ask for approval",
            Self::Deny => "Deny",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyConditionChoice {
    Tool,
    ToolOperation,
    ProjectPath,
    ShellCommand,
    Always,
}

impl fmt::Display for PolicyConditionChoice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Tool => "Tool is",
            Self::ToolOperation => "Tool operation is",
            Self::ProjectPath => "Project path matches",
            Self::ShellCommand => "Shell command matches",
            Self::Always => "Every operation",
        })
    }
}

const POLICY_ACTIONS: &[PolicyActionChoice] =
    &[PolicyActionChoice::Allow, PolicyActionChoice::Ask, PolicyActionChoice::Deny];
const POLICY_CONDITION_KINDS: &[PolicyConditionChoice] = &[
    PolicyConditionChoice::Tool,
    PolicyConditionChoice::ToolOperation,
    PolicyConditionChoice::ProjectPath,
    PolicyConditionChoice::ShellCommand,
    PolicyConditionChoice::Always,
];

/// UI dropdown choices for `WorkingDirBehavior` (ADR-28).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkingDirBehaviorChoice {
    ProjectRoot,
    Home,
    ShellDefault,
}

impl WorkingDirBehaviorChoice {
    fn from_behavior(b: &WorkingDirBehavior) -> Self {
        match b {
            WorkingDirBehavior::ProjectRoot => Self::ProjectRoot,
            WorkingDirBehavior::Home => Self::Home,
            WorkingDirBehavior::ShellDefault => Self::ShellDefault,
            _ => Self::ShellDefault,
        }
    }

    fn to_behavior(self) -> WorkingDirBehavior {
        match self {
            Self::ProjectRoot => WorkingDirBehavior::ProjectRoot,
            Self::Home => WorkingDirBehavior::Home,
            Self::ShellDefault => WorkingDirBehavior::ShellDefault,
        }
    }
}

impl fmt::Display for WorkingDirBehaviorChoice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::ProjectRoot => "Project root",
            Self::Home => "Home directory",
            Self::ShellDefault => "Shell default",
        })
    }
}

const WORKING_DIR_CHOICES: &[WorkingDirBehaviorChoice] = &[
    WorkingDirBehaviorChoice::ProjectRoot,
    WorkingDirBehaviorChoice::Home,
    WorkingDirBehaviorChoice::ShellDefault,
];
// Keep this list aligned with the tools registered by runtime_runner.
const POLICY_TOOLS: &[&str] = &["filesystem", "shell"];
const POLICY_OPERATION_TOOLS: &[&str] = &["filesystem"];
const FILESYSTEM_OPERATIONS: &[&str] = &["read", "write", "delete", "exists"];
const AGENT_ROLES: &[&str] =
    &["coordinator", "architect", "researcher", "coder", "reviewer", "validator"];
const RELATIONSHIP_TYPES: &[&str] =
    &["supervises", "provides_context_to", "reports_to", "owns_design"];

/// Cap for one discovery-diagnostic line in the Skills tab. A formatted
/// search path can be very long (e.g. `C:\Users\…\AppData\Roaming\concerto\…`)
/// and one pathological line must not balloon the settings pane.
const SKILL_DIAGNOSTIC_MAX_CHARS: usize = 140;

/// Truncate a display string to `max` chars, appending an ellipsis when cut.
/// Used to keep skill descriptions and snippets compact in the Extensions UI.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

/// Bottom-of-page footer: a one-line status message plus the Save button.
///
/// The Save button is ALWAYS rendered here. It previously lived only in the
/// `settings_dirty` branch of the inline `save_area` expression, so the moment
/// `SaveSettings` cleared that flag the button vanished from the widget tree
/// (regression: "Save button disappears after saving"). The status text fills
/// the row width on the left, keeping the button pinned to the right edge at a
/// stable one-line height in every state, so a save can never displace or
/// hide the button.
fn save_footer<'a>(state: &'a State, theme: &'a AppTheme) -> Element<'a, Message> {
    let palette = &theme.palette;

    let status: Element<'_, Message> = if state.settings_dirty {
        // Unsaved policy/relationship/memory/retry/shell changes exist.
        text("Unsaved changes").size(12).color(palette.text_muted).width(Length::Fill).into()
    } else if state.settings_saved_notice {
        text("Settings saved. Some changes (theme, font) apply immediately.")
            .size(12)
            .color(palette.success)
            .width(Length::Fill)
            .into()
    } else {
        text("All changes saved").size(12).color(palette.text_muted).width(Length::Fill).into()
    };

    let save_btn = button(text("Save Settings").size(13))
        .style(crate::ui::button::primary)
        .padding([6, 14])
        .on_press(Message::SaveSettings);

    row![status, save_btn].spacing(SPACING_SM).align_y(iced::Alignment::Center).into()
}

impl State {
    /// Render a collapsible section card. When collapsed only the clickable
    /// header row ([+]/[-] toggle + title) is shown. When expanded the full
    /// content is rendered inside the card.
    fn collapsible_section<'a>(
        &'a self,
        theme: &'a AppTheme,
        id: message::SectionId,
        title: &'a str,
        content: impl Into<Element<'a, Message>>,
    ) -> Element<'a, Message> {
        let content = content.into();
        let palette = &theme.palette;
        let collapsed = self.collapsed_sections.contains(&id);
        let icon = if collapsed { "[+]" } else { "[-]" };

        let header = button(
            row![
                text(icon).size(13).color(palette.text_muted),
                text(title).size(16).color(palette.text).width(Length::Fill),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
        )
        .style(button::text)
        .on_press(Message::ToggleSection(id));

        let body = if collapsed {
            column![header].spacing(12)
        } else {
            column![header, content].spacing(12)
        };

        container(body)
            .width(Length::Fill)
            .padding(16)
            .style(move |_theme: &iced::Theme| container::Style {
                background: Some(Background::Color(palette.surface_variant)),
                border: Border { color: palette.border, width: 1.0, radius: 12.0.into() },
                ..container::Style::default()
            })
            .into()
    }

    /// Render the full settings page.
    ///
    /// `hide_relationships` (Slice 4a, spec §7): when `[orchestration]` is
    /// present the blueprint's open relationship registry replaces the legacy
    /// rule manager, so both the "Agent Relationships" sidebar item and the
    /// relationship section in the main column are omitted.
    pub fn view<'a>(
        &'a self,
        theme: &'a AppTheme,
        hide_relationships: bool,
    ) -> Element<'a, Message> {
        let palette = &theme.palette;

        // ── Display section (Theme + Font) ────────────────────────────────
        let display_content = column![
            row![
                text("Theme:").size(13).color(palette.text),
                pick_list(&self.theme_names[..], Some(self.selected_theme), Message::ThemeSelected),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
            labeled_slider(
                theme,
                "Font Size",
                self.font_size,
                12.0..=20.0,
                Message::FontSizeChanged,
                |v| format!("{:.0}px", v),
            ),
            checkbox(self.reduced_motion)
                .label("Reduced motion (skip pulse, emphasis, handoff hold, line wipe)")
                .on_toggle(Message::ReducedMotionToggled),
            checkbox(self.scanline_overlay_enabled)
                .label("Scanline overlay behind chat (default off)")
                .on_toggle(Message::ScanlineOverlayToggled),
        ]
        .spacing(SPACING_SM);
        let display_section =
            self.collapsible_section(theme, message::SectionId::Theme, "Display", display_content);

        // ── Providers section ─────────────────────────────────────────────
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

        let provider_content =
            column![column(provider_items).spacing(SPACING_XS), add_btn,].spacing(SPACING_SM);

        let provider_section = self.collapsible_section(
            theme,
            message::SectionId::Providers,
            "Providers & Credentials",
            provider_content,
        );

        // ── Global Default Model section ───────────────────────────────────
        let palette = &theme.palette;

        // Unified model picker across all providers. Each option is rendered as
        // "model — provider" and selected stores just the model name.
        /// Display option for the global default model picker.
        #[derive(Debug, Clone, PartialEq, Eq)]
        struct GlobalModelOption {
            key: String,   // empty for "automatic", otherwise the model name
            label: String, // display text
        }

        impl std::fmt::Display for GlobalModelOption {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.label)
            }
        }

        let mut model_options: Vec<GlobalModelOption> = Vec::new();
        model_options.push(GlobalModelOption {
            key: String::new(),
            label: "Automatic (first available provider)".into(),
        });

        let mut pairs: Vec<(String, String)> = Vec::new();
        for (provider_id, models) in &self.cached_models_by_provider {
            for model in models {
                pairs.push((provider_id.clone(), model.clone()));
            }
        }
        pairs.sort_by(|a, b| a.1.cmp(&b.1).then(a.0.cmp(&b.0)));

        for (provider_id, model) in &pairs {
            let provider_label = self
                .providers
                .iter()
                .find(|provider| provider.id.as_str() == provider_id.as_str())
                .map(readable_provider_label)
                .unwrap_or_else(|| provider_id.clone());
            let label = format!("{model} — {provider_label}");
            model_options.push(GlobalModelOption { key: model.clone(), label });
        }

        let current_model = self.global_default_model.as_deref().unwrap_or("");
        let selected = if current_model.is_empty() {
            model_options.first().cloned()
        } else {
            model_options.iter().find(|o| o.key == current_model).cloned()
        };

        let default_pick: Element<'_, Message> =
            pick_list(model_options, selected, move |option: GlobalModelOption| {
                let model = if option.key.is_empty() { None } else { Some(option.key.clone()) };
                Message::GlobalDefaultModelChanged(model)
            })
            .width(Length::Fixed(400.0))
            .into();

        let default_content: Element<'_, Message> = container(
            column![
                text("Select the default model used for single-agent chat and as a fallback when an agent's assigned provider no longer exists.")
                    .size(12)
                    .color(palette.text_muted),
                row![
                    text("Default Model").size(14).width(Length::Fill),
                    default_pick,
                ]
                .spacing(SPACING_SM)
                .align_y(iced::Alignment::Center),
            ]
            .spacing(SPACING_XS),
        )
        .into();

        let default_section = self.collapsible_section(
            theme,
            message::SectionId::Assignments,
            "Default Model",
            default_content,
        );

        let relationship_builder = row![
            pick_list(
                AGENT_ROLES,
                Some(self.new_relationship_from),
                Message::RelationshipFromChanged,
            ),
            text("→"),
            pick_list(AGENT_ROLES, Some(self.new_relationship_to), Message::RelationshipToChanged,),
            pick_list(
                RELATIONSHIP_TYPES,
                Some(self.new_relationship_type),
                Message::RelationshipTypeChanged,
            ),
            text_input("cycles (optional)", &self.new_relationship_cycles)
                .on_input(Message::RelationshipCyclesChanged)
                .width(120),
            button(text("Add / replace").size(13))
                .style(crate::ui::button::secondary)
                .padding([6, 14])
                .on_press(Message::RelationshipAdded),
        ]
        .spacing(SPACING_XS)
        .align_y(iced::Alignment::Center);

        let relationship_rows: Vec<Element<'_, Message>> = self
            .relationship_rules
            .iter()
            .enumerate()
            .map(|(index, rule)| {
                row![
                    text(State::relationship_display(rule)).width(Length::Fill),
                    button(text("X").size(13))
                        .style(crate::ui::button::secondary)
                        .padding([6, 14])
                        .on_press(Message::RelationshipRemoved(index)),
                ]
                .spacing(SPACING_XS)
                .into()
            })
            .collect();
        let relationship_warning: Element<'_, Message> = match &self.relationship_warning {
            Some(msg) => container(text(msg).size(12).color(palette.warning)).padding(4).into(),
            None => container(text("")).height(0).into(),
        };
        let relationship_section = self.collapsible_section(
            theme,
            message::SectionId::Relationships,
            "Agent Relationship Manager",
            column![
                text("Directed rules control handoffs and review/validation cycle limits. An empty list uses Concerto's defaults.")
                    .size(12)
                    .color(palette.text_muted),
                relationship_builder,
                relationship_warning,
                column(relationship_rows).spacing(2),
            ]
            .spacing(SPACING_SM),
        );

        // ── Policy section ────────────────────────────────────────────────
        // ── Policy rule builder (vertical form) ──
        let condition_input: Element<'_, Message> = match self.new_policy_condition_kind {
            PolicyConditionChoice::Tool => form_field(
                theme,
                "Tool",
                false,
                None::<&str>,
                None::<&str>,
                pick_list(POLICY_TOOLS, Some(self.new_policy_tool), Message::NewPolicyToolSelected),
            ),
            PolicyConditionChoice::ToolOperation => row![
                form_field(
                    theme,
                    "Tool",
                    false,
                    None::<&str>,
                    None::<&str>,
                    pick_list(
                        POLICY_OPERATION_TOOLS,
                        Some(self.new_policy_tool),
                        Message::NewPolicyToolSelected,
                    )
                    .width(Length::Fill),
                ),
                form_field(
                    theme,
                    "Operation",
                    false,
                    None::<&str>,
                    None::<&str>,
                    pick_list(
                        Self::operation_options(self.new_policy_tool),
                        Some(self.new_policy_operation),
                        Message::NewPolicyOperationSelected,
                    )
                    .width(Length::Fill),
                ),
            ]
            .spacing(SPACING_SM)
            .into(),
            PolicyConditionChoice::ProjectPath | PolicyConditionChoice::ShellCommand => form_field(
                theme,
                if matches!(self.new_policy_condition_kind, PolicyConditionChoice::ProjectPath) {
                    "Path glob"
                } else {
                    "Command regex"
                },
                false,
                Some(Self::policy_value_placeholder(self.new_policy_condition_kind)),
                None::<&str>,
                text_input(
                    Self::policy_value_placeholder(self.new_policy_condition_kind),
                    &self.new_policy_condition_value,
                )
                .on_input(Message::NewPolicyConditionValueChanged)
                .width(Length::Fill),
            ),
            PolicyConditionChoice::Always => {
                container(text("Applies to every operation").size(12).color(palette.text_muted))
                    .padding(8)
                    .into()
            }
        };
        let policy_input_valid = matches!(
            self.new_policy_condition_kind,
            PolicyConditionChoice::Tool
                | PolicyConditionChoice::ToolOperation
                | PolicyConditionChoice::Always
        ) || !self.new_policy_condition_value.trim().is_empty();
        let add_policy_button = button(text("+ Add rule").size(13))
            .style(crate::ui::button::secondary)
            .padding([6, 14]);
        let add_policy_button: Element<'_, Message> = if policy_input_valid {
            add_policy_button.on_press(Message::PolicyRuleAdded).into()
        } else {
            add_policy_button.into()
        };
        let policy_builder = column![
            form_field(
                theme,
                "Action",
                false,
                None::<&str>,
                None::<&str>,
                pick_list(
                    POLICY_ACTIONS,
                    Some(self.new_policy_action),
                    Message::NewPolicyActionSelected,
                ),
            ),
            form_field(
                theme,
                "When",
                false,
                None::<&str>,
                None::<&str>,
                pick_list(
                    POLICY_CONDITION_KINDS,
                    Some(self.new_policy_condition_kind),
                    Message::NewPolicyConditionKindSelected,
                ),
            ),
            condition_input,
            add_policy_button,
        ]
        .spacing(SPACING_SM);

        let policy_preview_text = State::policy_preview(
            self.new_policy_action,
            self.new_policy_condition_kind,
            self.new_policy_tool,
            self.new_policy_operation,
            &self.new_policy_condition_value,
        );
        let policy_help = text(State::policy_condition_help(self.new_policy_condition_kind))
            .size(12)
            .color(palette.text_muted);

        let mut policy_rows: Vec<Element<'_, Message>> = Vec::new();
        for (i, rule) in self.policy_rules.iter().enumerate() {
            let rule_color = match rule.action.as_str() {
                "auto_approve" => palette.success,
                "auto_deny" => palette.danger,
                _ => palette.warning,
            };
            let up_button =
                button(text("Up").size(13)).style(crate::ui::button::secondary).padding([6, 14]);
            let up_button: Element<'_, Message> = if i > 0 {
                up_button.on_press(Message::PolicyRuleMovedUp(i)).into()
            } else {
                up_button.into()
            };
            let down_button =
                button(text("Down").size(13)).style(crate::ui::button::secondary).padding([6, 14]);
            let down_button: Element<'_, Message> = if i + 1 < self.policy_rules.len() {
                down_button.on_press(Message::PolicyRuleMovedDown(i)).into()
            } else {
                down_button.into()
            };
            policy_rows.push(
                row![
                    text(format!("{}. {}", i + 1, State::rule_display(rule)))
                        .width(Length::Fill)
                        .color(rule_color),
                    up_button,
                    down_button,
                    button(text("X").size(13))
                        .style(crate::ui::button::secondary)
                        .padding([6, 14])
                        .on_press(Message::PolicyRuleRemoved(i)),
                ]
                .spacing(SPACING_XS)
                .padding(2)
                .into(),
            );
        }
        let policy_rule_list: Element<'_, Message> = if policy_rows.is_empty() {
            text("No rules: all tool calls are currently allowed.")
                .size(12)
                .color(palette.warning)
                .into()
        } else {
            column(policy_rows).spacing(2).into()
        };

        let policy_section = self.collapsible_section(
            theme,
            message::SectionId::Policy,
            "Policy Rules",
            column![
                text("Rules are checked from top to bottom and the first match wins. Once a rule exists, unmatched tool calls are denied. Use Up and Down to set precedence.")
                    .size(12)
                    .color(palette.text_muted),
                policy_builder,
                policy_help,
                text(format!("Preview: {policy_preview_text}"))
                    .size(12)
                    .color(palette.text_muted),
                text("Configured rules (evaluation order)").size(13),
                policy_rule_list,
            ]
            .spacing(SPACING_SM),
        );

        // ── Provider retry & recovery section ───────────────────────────
        let retry_section = self.collapsible_section(
            theme,
            message::SectionId::Retry,
            "Provider Retry & Recovery",
            column![
                text("These settings apply to every configured provider and every agent role. Rate limits, temporary network failures, timeouts, and provider 5xx responses retry without ending the session. Authentication, invalid requests, policy denials, and user cancellation do not retry.")
                    .size(12)
                    .color(palette.text_muted),
                checkbox(self.retry_enabled)
                    .label("Retry transient provider failures automatically")
                    .on_toggle(Message::RetryEnabledToggled),
                // ── Timing sub-group ──
                text("Timing").size(13).color(palette.text),
                labeled_slider(
                    theme,
                    "Initial delay",
                    self.retry_initial_delay_ms,
                    100.0..=30000.0,
                    Message::RetryInitialDelayChanged,
                    |v| format!("{:.0} ms", v),
                ),
                labeled_slider(
                    theme,
                    "Maximum delay",
                    self.retry_max_delay_ms,
                    1000.0..=300000.0,
                    Message::RetryMaxDelayChanged,
                    |v| format!("{:.0} ms", v),
                ),
                labeled_slider(
                    theme,
                    "Backoff multiplier",
                    self.retry_multiplier,
                    1.0..=10.0,
                    Message::RetryMultiplierChanged,
                    |v| format!("{:.1}×", v),
                ),
                // ── Limits sub-group ──
                text("Limits").size(13).color(palette.text),
                form_field(
                    theme,
                    "Fixed delay override (ms)",
                    false,
                    Some("Leave blank for exponential backoff"),
                    self.retry_fixed_delay_error.as_deref(),
                    text_input("blank = exponential", &self.retry_fixed_delay_ms)
                        .on_input(Message::RetryFixedDelayChanged)
                        .width(Length::Fill),
                ),
                form_field(
                    theme,
                    "Outage time limit (seconds)",
                    false,
                    Some("Leave blank to retry indefinitely"),
                    self.retry_max_elapsed_error.as_deref(),
                    text_input("blank = keep retrying", &self.retry_max_elapsed_seconds)
                        .on_input(Message::RetryMaxElapsedChanged)
                        .width(Length::Fill),
                ),
                // ── Behavior toggles ──
                checkbox(self.retry_respect_after)
                    .label("Respect provider Retry-After instructions")
                    .on_toggle(Message::RetryRespectAfterToggled),
                checkbox(self.retry_jitter)
                    .label("Add jitter to prevent synchronized retry storms")
                    .on_toggle(Message::RetryJitterToggled),
            ]
            .spacing(SPACING_SM),
        );

        // ── Memory section ────────────────────────────────────────────────
        let memory_section = self.collapsible_section(
            theme,
            message::SectionId::Memory,
            "Memory Settings",
            column![
                checkbox(self.memory_enabled)
                    .label("Enabled")
                    .on_toggle(Message::MemoryEnabledToggled),
                labeled_slider(
                    theme,
                    "TTL",
                    self.memory_ttl_days,
                    1.0..=365.0,
                    Message::MemoryTtlChanged,
                    |v| format!("{:.0} days", v),
                ),
            ]
            .spacing(SPACING_SM),
        );

        // ── Save ──────────────────────────────────────────────────────────
        // The footer always renders the Save button; only the status text
        // changes with state (see `save_footer`).
        let save_area = save_footer(self, theme);

        let shell_content = self.shell_section(theme);
        let shell_section = self.collapsible_section(
            theme,
            message::SectionId::Shell,
            "Terminal & Shell",
            shell_content,
        );

        // ── Extensions section (ADR-37/43/70) ─────────────────────────────
        // Unified hub: Skills, MCP servers, Plugins, and project context share
        // one collapsible section and switch via sub-tabs.
        let extension_section = self.collapsible_section(
            theme,
            message::SectionId::Extensions,
            "Extensions",
            self.extensions_section(theme),
        );

        // ── Sidebar nav ──
        // Quick navigation: each entry jumps to its section (expand + scroll)
        // via `Message::JumpToSection`. Labels intentionally repeat the
        // collapsible_section titles (minor duplication keeps both readable).
        let sidebar_items = vec![
            (message::SectionId::Theme, "Display"),
            (message::SectionId::Providers, "Providers & Credentials"),
            (message::SectionId::Assignments, "Default Model"),
            (message::SectionId::Policy, "Safety & Policy"),
            (message::SectionId::Relationships, "Agent Relationships"),
            (message::SectionId::Retry, "Retry & Recovery"),
            (message::SectionId::Memory, "Memory"),
            (message::SectionId::Shell, "Terminal & Shell"),
            (message::SectionId::Extensions, "Extensions"),
        ];
        // Slice 4a (spec §7): with `[orchestration]` present the blueprint's
        // open relationship registry replaces the legacy rule manager, so the
        // sidebar entry is dropped (the section itself is omitted below).
        let sidebar_items: Vec<_> = if hide_relationships {
            sidebar_items
                .into_iter()
                .filter(|(id, _)| *id != message::SectionId::Relationships)
                .collect()
        } else {
            sidebar_items
        };

        let mut sidebar_buttons: Vec<Element<'_, Message>> = Vec::new();
        for (id, label) in sidebar_items {
            let is_expanded = !self.collapsed_sections.contains(&id);
            sidebar_buttons.push(crate::ui::list_item(
                theme,
                is_expanded,
                Message::JumpToSection(id),
                text(label)
                    .size(13)
                    .style(move |_| crate::theme::sidebar_item_style(palette, is_expanded)),
            ));
        }
        let sidebar = column(sidebar_buttons).spacing(4).width(180);

        // ── Main content ──
        // Sections are pushed conditionally: the relationship manager only
        // renders while the blueprint surface is inactive (Slice 4a, spec §7).
        let mut main_sections: Vec<Element<'_, Message>> =
            vec![display_section, provider_section, default_section, policy_section];
        if !hide_relationships {
            main_sections.push(relationship_section);
        }
        main_sections.extend([
            retry_section,
            memory_section,
            shell_section,
            extension_section,
            save_area,
        ]);
        let main_content = column(main_sections).spacing(SPACING_MD).padding(20);

        let main_scrollable = scrollable(container(main_content).width(Length::Fill))
            .id(iced::widget::Id::new(MAIN_SCROLL_ID));

        // ── Combined layout ──
        row![
            container(sidebar).width(180).padding(12).style(move |_theme: &iced::Theme| {
                container::Style {
                    background: Some(Background::Color(palette.surface_variant)),
                    border: Border { color: palette.border, width: 0.0, radius: 0.0.into() },
                    ..container::Style::default()
                }
            }),
            main_scrollable.width(Length::Fill),
        ]
        .spacing(0)
        .into()
    }

    // ── Extensions (ADR-37/43/70) — unified hub ─────────────────────────
    //
    // One collapsible section hosts Skills, MCP servers, Plugins, and project
    // context, switched with a segmented control. The Skills, MCP and Plugins
    // tabs share a master–detail pattern: the master pane carries an enable
    // toggle per row (plus per-item status), and the detail pane shows
    // read-only metadata and the per-item action (instructions preview /
    // probe / revoke). Everything is config-driven v1 with next-run
    // semantics: the desktop builds a fresh ServicesBuilder per agent run, so
    // these load on the next run.
    fn extensions_section<'a>(&'a self, theme: &'a AppTheme) -> Element<'a, Message> {
        let palette = &theme.palette;

        // A staging banner is shown whenever any settings edit is unsaved; it
        // drives home that extension changes apply to the next run only.
        let banner: Element<'a, Message> = if self.settings_dirty {
            container(
                row![
                    text("•").size(15).color(palette.warning),
                    text(
                        "Changes are staged. Press Save Settings below — skills, MCP \
                         servers, plugins, and project context load on the next run."
                    )
                    .size(12)
                    .color(palette.text)
                    .width(Length::Fill),
                ]
                .spacing(SPACING_SM)
                .align_y(Alignment::Center),
            )
            .width(Length::Fill)
            .padding([10, 12])
            .style(move |_theme: &iced::Theme| container::Style {
                background: Some(Background::Color(palette.surface_variant)),
                border: Border { color: palette.border, width: 1.0, radius: 8.0.into() },
                ..container::Style::default()
            })
            .into()
        } else {
            iced::widget::Space::new().height(0).into()
        };

        let tabs = [
            Segment {
                label: "Skills",
                active: self.active_extension_tab == ExtensionTab::Skills,
                on_press: Message::ExtensionTabSelected(ExtensionTab::Skills),
            },
            Segment {
                label: "MCP Servers",
                active: self.active_extension_tab == ExtensionTab::Mcp,
                on_press: Message::ExtensionTabSelected(ExtensionTab::Mcp),
            },
            Segment {
                label: "Plugins",
                active: self.active_extension_tab == ExtensionTab::Plugins,
                on_press: Message::ExtensionTabSelected(ExtensionTab::Plugins),
            },
            Segment {
                label: "Project Context",
                active: self.active_extension_tab == ExtensionTab::ProjectContext,
                on_press: Message::ExtensionTabSelected(ExtensionTab::ProjectContext),
            },
        ];

        let tab_content: Element<'a, Message> = match self.active_extension_tab {
            ExtensionTab::Skills => self.ext_skills_tab(theme),
            ExtensionTab::Mcp => self.ext_mcp_tab(theme),
            ExtensionTab::Plugins => self.ext_plugins_tab(theme),
            ExtensionTab::ProjectContext => self.ext_project_context_tab(theme),
        };

        column![banner, segmented(theme, &tabs), tab_content].spacing(SPACING_MD).into()
    }

    // ── Project context tab (ADR-70) ────────────────────────────────────
    fn ext_project_context_tab<'a>(&'a self, theme: &'a AppTheme) -> Element<'a, Message> {
        let palette = &theme.palette;

        // The nudge toggle is meaningful only while the feature is enabled;
        // keep it inert (and visually distinct) otherwise.
        let nudge = checkbox(self.project_context_auto_update_agents_md)
            .label("Remind the coordinator to refresh AGENTS.md (advisory nudge)");
        let nudge = if self.project_context_enabled {
            nudge.on_toggle(Message::ProjectContextNudgeToggled)
        } else {
            nudge
        };

        let global_path = self
            .project_context_global_path
            .as_deref()
            .map(|path| resolved_path_label(path, false))
            .unwrap_or_else(platform_default_agents_label);
        let budget = self
            .project_context_max_bytes
            .map(|bytes| format!("{bytes} chars per file"))
            .unwrap_or_else(|| "default (32 KiB) per file".to_string());
        let cadence = if self.project_context_update_frequency <= 1 {
            "every dispatch".to_string()
        } else {
            format!("every {} dispatches", self.project_context_update_frequency)
        };

        column![
            checkbox(self.project_context_enabled)
                .label("Enable project context (inject AGENTS.md into prompts)")
                .on_toggle(Message::ProjectContextEnabledToggled),
            nudge,
            ext_meta_row(theme, "Global AGENTS.md", global_path),
            ext_meta_row(theme, "Budget", budget),
            ext_meta_row(theme, "Nudge cadence", cadence),
            text(
                "The enable switch and nudge toggle are editable here (v1); the path, budget, \
                 and cadence are set in the config file. The orchestrator (re)reads AGENTS.md \
                 from the next run onward."
            )
            .size(11)
            .color(palette.text_muted),
        ]
        .spacing(SPACING_SM)
        .into()
    }
}

// ── Extensions helpers (module-level) ─────────────────────────────────
/// A muted label with a fill-width value, used for read-only metadata rows
/// inside the extension detail panes.
fn ext_meta_row<'a>(
    theme: &'a AppTheme,
    label: &'a str,
    value: impl Into<String>,
) -> Element<'a, Message> {
    let palette = &theme.palette;
    row![
        text(label).size(11).color(palette.text_muted).width(150),
        text(value.into()).size(12).color(palette.text).width(Length::Fill),
    ]
    .spacing(SPACING_XS)
    .align_y(Alignment::Center)
    .into()
}

/// Master–detail split for the extension tabs: a fixed-width master pane
/// (surface_tinted with a hairline border) beside a fill-width detail pane.
fn ext_master_detail<'a>(
    theme: &'a AppTheme,
    master: Element<'a, Message>,
    detail: Element<'a, Message>,
) -> Element<'a, Message> {
    let palette = &theme.palette;
    row![
        container(master).width(300).padding([12, 12]).style(move |_theme: &iced::Theme| {
            container::Style {
                background: Some(Background::Color(palette.surface_variant)),
                border: Border { color: palette.border, width: 1.0, radius: 8.0.into() },
                ..container::Style::default()
            }
        }),
        container(detail).width(Length::Fill).padding([12, 12]),
    ]
    .spacing(SPACING_MD)
    .align_y(Alignment::Start)
    .into()
}

/// Compact, single-line summary of a list of configured paths for metadata
/// rows. Each path is shown resolved to its absolute form (`~` / `%VAR%`
/// expanded) with a trailing `(exists)`/`(missing)` badge for the matching
/// existence check. Paths that cannot be expanded are shown verbatim.
/// `expect_dir` picks `is_dir()` (search paths) vs `is_file()` (AGENTS.md).
fn path_summary(paths: &[String], expect_dir: bool) -> String {
    if paths.is_empty() {
        "none configured".to_string()
    } else {
        paths.iter().map(|raw| resolved_path_label(raw, expect_dir)).collect::<Vec<_>>().join(", ")
    }
}

/// Resolve a configured path (`~` / `%VAR%` → absolute) for display and append
/// an existence badge. Falls back to the configured literal when the path
/// cannot be expanded. Performs no writes and only a `stat`-class check, so it
/// is safe to call from `view`.
fn resolved_path_label(raw: &str, expect_dir: bool) -> String {
    let display = concerto_skills::expanded_search_path(std::path::Path::new(raw))
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|| raw.to_string());
    let exists = if expect_dir {
        std::path::Path::new(&display).is_dir()
    } else {
        std::path::Path::new(&display).is_file()
    };
    format!("{display} ({})", if exists { "exists" } else { "missing" })
}

/// Human-readable label for the platform default global AGENTS.md path. Uses
/// `dirs::config_dir()` (Linux `~/.config`, Windows `%APPDATA%`) so the label
/// matches the orchestrator's actual lookup instead of hardcoding
/// `~/.config/concerto/AGENTS.md`.
fn platform_default_agents_label() -> String {
    match dirs::config_dir() {
        Some(config_dir) => {
            let path = config_dir.join("concerto").join("AGENTS.md");
            let badge = if path.is_file() { "exists" } else { "missing" };
            format!("platform default ({} · {badge})", path.display())
        }
        None => "platform default (config dir unavailable)".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The diff-tree tag of the Save button, fingerprinted from a button built
    /// exactly like the one inside `save_footer`. Button widgets carry an
    /// opaque private `State`, so we compare against a probe instead of naming
    /// the type.
    fn save_button_tag() -> iced_core::widget::tree::Tag {
        let probe: Element<'_, Message> = button(text("Save Settings"))
            .style(crate::ui::button::primary)
            .on_press(Message::SaveSettings)
            .into();
        probe.as_widget().tag()
    }

    /// Regression test for "Save button disappears after saving": the footer
    /// used to render the button only while `settings_dirty` was set, so the
    /// moment `SaveSettings` cleared the flag the button was removed from the
    /// widget tree. The footer must keep the button in every state the page
    /// can reach (dirty → just-saved → idle).
    #[test]
    fn save_button_is_present_in_every_footer_state() {
        let theme = AppTheme::by_name("Midnight");
        let button_tag = save_button_tag();

        for (dirty, notice) in [(true, false), (false, true), (false, false)] {
            let mut state = State::new();
            state.settings_dirty = dirty;
            state.settings_saved_notice = notice;

            let footer = save_footer(&state, &theme);
            let has_button =
                footer.as_widget().children().iter().any(|child| child.tag == button_tag);
            assert!(
                has_button,
                "Save button missing from footer with settings_dirty={dirty}, \
                 settings_saved_notice={notice}"
            );
        }
    }

    /// Smoke test: the full settings view renders in the exact post-save state
    /// (the state reached after `SaveSettings`) without panicking.
    #[test]
    fn settings_view_renders_in_post_save_state() {
        let mut state = State::new();
        let _ = state.update(Message::SaveSettings);
        assert!(!state.settings_dirty, "Save Settings must clear the dirty flag");
        assert!(state.settings_saved_notice, "Save Settings must show the success notice");

        let theme = AppTheme::by_name("Midnight");
        let _element = state.view(&theme, false);
    }

    /// Slice 4a (spec §7): the Relationships-hide flag (plumbed from
    /// `[orchestration]` presence by the App) must render the page in both
    /// states — true (blueprint registry replaces the legacy rule manager)
    /// and false (legacy path keeps it) — without panicking.
    #[test]
    fn relationships_hide_flag_renders_both_section_states() {
        let state = State::new();
        let theme = AppTheme::by_name("Midnight");
        let _ = state.view(&theme, true);
        let _ = state.view(&theme, false);
    }
}
