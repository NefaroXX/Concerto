use iced::widget::{
    button, checkbox, column, container, pick_list, row, scrollable, text, text_editor, text_input,
    tooltip,
};
use iced::{Alignment, Background, Border, Element, Length};
use std::fmt;

use concerto_api_types::extension::SkillDescriptor;
use concerto_config::shell::WorkingDirBehavior;
use concerto_config::{McpServerConfig, ProviderConfig};
use concerto_providers::provider_defs::{
    provider_definition, provider_readiness, CredentialRequirement, ProviderReadiness,
    PROVIDER_TYPE_IDS,
};

use crate::theme::AppTheme;
use crate::ui::{
    form_field, labeled_slider, padded, segmented, Segment, SPACING_MD, SPACING_SM, SPACING_XS,
};

mod helpers;

pub mod message;
pub mod shell;
pub mod state;

pub use message::ExtensionTab;
pub use message::Message;
pub use state::InstalledPluginInfo;
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

    /// Shared refresh control for the Skills tab (inert while a discovery run
    /// is in flight; the label doubles as the spinner).
    fn ext_refresh_button<'a>(&'a self) -> Element<'a, Message> {
        if self.skills_loading {
            button(text("Discovering…").size(13))
                .style(crate::ui::button::secondary)
                .padding([6, 14])
                .into()
        } else {
            button(text("Refresh").size(13))
                .style(crate::ui::button::secondary)
                .padding([6, 14])
                .on_press(Message::SkillsDiscoveryRequested)
                .into()
        }
    }

    // ── Skills tab ───────────────────────────────────────────────────────
    fn ext_skills_tab<'a>(&'a self, theme: &'a AppTheme) -> Element<'a, Message> {
        let palette = &theme.palette;

        let mut master: Vec<Element<'a, Message>> = vec![
            checkbox(self.skills_enabled)
                .label("Enable skills")
                .on_toggle(Message::SkillsEnabledToggled)
                .into(),
            ext_meta_row(theme, "Search paths", path_summary(&self.skills_search_paths, true)),
            ext_meta_row(theme, "Auto-load", if self.skills_auto_load { "on" } else { "off" }),
            ext_meta_row(
                theme,
                "Prompt budget",
                self.skills_max_chars
                    .map(|chars| chars.to_string())
                    .unwrap_or_else(|| "default (orchestrator budget)".to_string()),
            ),
            row![
                text("Discovered skills").size(13).color(palette.text),
                self.ext_new_skill_button(),
                self.ext_refresh_button(),
            ]
            .spacing(SPACING_SM)
            .align_y(Alignment::Center)
            .into(),
        ];

        if let Some(error) = &self.skills_error {
            master.push(text(error).size(12).color(palette.danger).into());
        } else if self.skills_discovered.is_empty() {
            master.push(
                text(if self.skills_loaded {
                    "No skills discovered under the configured search paths."
                } else if self.skills_loading {
                    "Discovering skills…"
                } else {
                    "Skills have not been discovered yet. Press Refresh."
                })
                .size(12)
                .color(palette.text_muted)
                .into(),
            );
        } else {
            for skill in &self.skills_discovered {
                master.push(self.ext_skill_row(theme, skill));
            }
        }

        // Per-path/per-pack diagnostics from the last discovery run (missing
        // search paths, packs skipped for malformed manifests) explain a
        // sparse result without hiding it behind a bare "no skills found" line.
        for warning in &self.skills_warnings {
            master.push(text(format!("• {warning}")).size(11).color(palette.warning).into());
        }

        ext_master_detail(
            theme,
            column(master).spacing(SPACING_SM).into(),
            if self.skill_create_open {
                self.ext_skill_create_form(theme)
            } else {
                self.ext_skill_detail(theme)
            },
        )
    }

    /// Master "New skill" action (ADR-43): opens the create wizard in the
    /// detail pane. Inert while the wizard is already open or a CRUD operation
    /// is in flight so the user cannot queue competing writes.
    fn ext_new_skill_button<'a>(&'a self) -> Element<'a, Message> {
        let enabled = !self.skill_create_open && !self.skill_crud_busy;
        let mut button =
            button(text("New skill").size(13)).style(crate::ui::button::secondary).padding([6, 14]);
        if enabled {
            button = button.on_press(Message::SkillCreatePressed);
        }
        button.into()
    }

    fn ext_skill_row<'a>(
        &'a self,
        theme: &'a AppTheme,
        skill: &'a SkillDescriptor,
    ) -> Element<'a, Message> {
        let active = self.ext_selected_skill.as_deref() == Some(skill.id.as_str());
        let checked = self.skills_allow_all || self.skills_enabled_ids.contains(&skill.id);
        let name = if skill.manifest.name.is_empty() {
            skill.id.clone()
        } else {
            skill.manifest.name.clone()
        };
        let version =
            if skill.manifest.version.is_empty() { "0.0.0" } else { &skill.manifest.version };
        let tool_count = skill.manifest.tools.len();
        let select_id = skill.id.clone();
        let toggle_id = skill.id.clone();

        row![
            crate::ui::list_item(
                theme,
                active,
                Message::ExtensionItemSelected(ExtensionTab::Skills, select_id),
                column![
                    text(name).size(13).color(theme.palette.text),
                    text(format!(
                        "v{version} · {n} tool{s}",
                        version = version,
                        n = tool_count,
                        s = if tool_count == 1 { "" } else { "s" },
                    ))
                    .size(11)
                    .color(theme.palette.text_muted),
                ]
                .spacing(2),
            ),
            checkbox(checked)
                .label("")
                .on_toggle(move |on| Message::SkillTogglePressed(toggle_id.clone(), on)),
        ]
        .spacing(SPACING_XS)
        .align_y(Alignment::Center)
        .into()
    }

    fn ext_skill_detail<'a>(&'a self, theme: &'a AppTheme) -> Element<'a, Message> {
        let palette = &theme.palette;
        let Some(skill) = self
            .skills_discovered
            .iter()
            .find(|skill| self.ext_selected_skill.as_deref() == Some(skill.id.as_str()))
        else {
            return column![
                text("No skill selected").size(14).color(palette.text),
                text("Select a skill in the list to view its metadata and instructions.")
                    .size(12)
                    .color(palette.text_muted),
            ]
            .spacing(SPACING_SM)
            .into();
        };

        // Edit mode: replace the read-only metadata with the editable form.
        if self.skill_editing_id.as_deref() == Some(skill.id.as_str()) {
            if let Some(draft) = &self.skill_edit_draft {
                return self.ext_skill_edit_form(theme, skill, draft);
            }
        }

        let name = if skill.manifest.name.is_empty() {
            skill.id.clone()
        } else {
            skill.manifest.name.clone()
        };
        let version =
            if skill.manifest.version.is_empty() { "0.0.0" } else { &skill.manifest.version };
        let tools = if skill.manifest.tools.is_empty() {
            "none".to_string()
        } else {
            skill.manifest.tools.join(", ")
        };
        let description = if skill.manifest.description.is_empty() {
            "(no description)".to_string()
        } else {
            truncate(&skill.manifest.description, 160)
        };
        let expanded = self.skills_expanded.contains(&skill.id);

        let mut rows: Vec<Element<'a, Message>> = vec![
            text(name).size(16).color(palette.text).into(),
            ext_meta_row(theme, "ID", skill.id.as_str()),
            ext_meta_row(theme, "Version", version),
            ext_meta_row(theme, "Tools", tools),
            ext_meta_row(theme, "Description", description),
            ext_meta_row(theme, "Pack", skill.pack_dir.display().to_string()),
            row![button(
                text(if expanded { "Hide instructions" } else { "Show instructions" }).size(13)
            )
            .style(crate::ui::button::secondary)
            .padding([6, 14])
            .on_press(Message::SkillExpandToggled(skill.id.clone())),]
            .into(),
        ];
        if expanded {
            rows.push(crate::widgets::code_block::view(
                &skill.instructions,
                None,
                Message::SkillExpandToggled(skill.id.clone()),
                palette.surface_variant,
            ));
        }
        rows.push(self.ext_skill_crud_controls(theme, skill));
        column(rows).spacing(SPACING_SM).into()
    }

    /// Edit / Delete controls for a selected skill, plus the inline
    /// delete-confirm prompt and the last CRUD outcome line (ADR-43 edge).
    /// Deletion is destructive, so the first Delete press only arms a confirm
    /// prompt (same explicit-confirm rule as providers and MCP servers);
    /// confirming moves the manifest files to hidden `.deleted-*` backups in
    /// place, so the operation stays reversible. Packs without a `skill.toml`
    /// (SKILL.md-only) cannot be edited in the v1 editor and only offer Delete.
    fn ext_skill_crud_controls<'a>(
        &'a self,
        theme: &'a AppTheme,
        skill: &'a SkillDescriptor,
    ) -> Element<'a, Message> {
        let palette = &theme.palette;
        let mut rows: Vec<Element<'a, Message>> = Vec::new();

        if let Some(result) = &self.skill_crud_result {
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

        if self.skill_delete_confirm.as_deref() == Some(skill.id.as_str()) {
            rows.push(
                row![
                    text("Delete this skill pack?").size(12).color(palette.warning),
                    button(text("Confirm").size(13))
                        .style(crate::ui::button::secondary)
                        .padding([6, 12])
                        .on_press(Message::SkillDeleteConfirmed(skill.id.clone())),
                    button(text("Cancel").size(13))
                        .style(crate::ui::button::secondary)
                        .padding([6, 12])
                        .on_press(Message::SkillDeleteCancelled(skill.id.clone())),
                ]
                .spacing(SPACING_XS)
                .align_y(Alignment::Center)
                .into(),
            );
            return column(rows).spacing(SPACING_XS).into();
        }

        let editable = skill.pack_dir.join("skill.toml").exists();
        let busy = self.skill_crud_busy;

        let mut edit_button = button(text("Edit skill").size(13))
            .style(crate::ui::button::secondary)
            .padding([6, 14]);
        if editable && !busy {
            edit_button = edit_button.on_press(Message::SkillEditPressed(skill.id.clone()));
        }

        let mut delete_button = button(text("Delete skill").size(13))
            .style(crate::ui::button::danger_outline)
            .padding([6, 14]);
        if !busy {
            delete_button = delete_button.on_press(Message::SkillDeletePressed(skill.id.clone()));
        }

        rows.push(row![edit_button, delete_button].spacing(SPACING_SM).into());
        if !editable {
            rows.push(
                text("This pack uses SKILL.md; edit it in the file directly.")
                    .size(11)
                    .color(palette.text_muted)
                    .into(),
            );
        }
        column(rows).spacing(SPACING_XS).into()
    }

    /// Full create wizard for a brand-new `skill.toml` pack (ADR-43), rendered
    /// in the detail pane in place of the read-only metadata. Id and parent are
    /// validated inline and again on Confirm; a success lands the pack on disk
    /// and triggers a discovery refresh so it appears immediately.
    fn ext_skill_create_form<'a>(&'a self, theme: &'a AppTheme) -> Element<'a, Message> {
        let palette = &theme.palette;
        let busy = self.skill_crud_busy;

        let create_button: Element<'a, Message> = if busy {
            button(text("Creating…").size(13))
                .style(crate::ui::button::primary)
                .padding([6, 14])
                .into()
        } else {
            button(text("Create skill").size(13))
                .style(crate::ui::button::primary)
                .padding([6, 14])
                .on_press(Message::SkillCreateConfirmed)
                .into()
        };

        let result_line: Element<'a, Message> = match &self.skill_crud_result {
            Some(result) if result.starts_with("Error") => {
                text(result).size(11).color(palette.danger).into()
            }
            Some(_) => iced::widget::Space::new().height(0).into(),
            None => iced::widget::Space::new().height(0).into(),
        };

        let instructions_label: Element<'a, Message> =
            text("Instructions").size(13).color(palette.text).into();
        let instructions_hint: Element<'a, Message> =
            text("The skill body, injected into prompts when the skill loads.")
                .size(11)
                .color(palette.text_muted)
                .into();

        column![
            text("New skill pack").size(16).color(palette.text),
            text(
                "Create a skill.toml in one of the configured search paths; the new pack \
                 appears in the discovered list after the refresh that follows a successful create."
            )
            .size(12)
            .color(palette.text_muted),
            form_field(
                theme,
                "Id",
                true,
                Some("Single path component, e.g. \"rust-testing\"; it becomes the pack directory name."),
                self.skill_create_id_error.as_deref(),
                text_input("skill id", &self.skill_create_id)
                    .on_input(Message::SkillCreateIdChanged)
                    .width(Length::Fill),
            ),
            form_field(
                theme,
                "Name",
                false,
                Some("Blank uses the id."),
                None::<&str>,
                text_input("display name", &self.skill_create_name)
                    .on_input(Message::SkillCreateNameChanged)
                    .width(Length::Fill),
            ),
            form_field(
                theme,
                "Version",
                false,
                Some("Blank uses 0.1.0."),
                None::<&str>,
                text_input("0.1.0", &self.skill_create_version)
                    .on_input(Message::SkillCreateVersionChanged)
                    .width(Length::Fill),
            ),
            form_field(
                theme,
                "Description",
                false,
                None::<&str>,
                None::<&str>,
                text_input("short description", &self.skill_create_description)
                    .on_input(Message::SkillCreateDescriptionChanged)
                    .width(Length::Fill),
            ),
            form_field(
                theme,
                "Parent directory",
                true,
                Some("A \"missing\" directory is created by the write."),
                self.skill_create_parent_error.as_deref(),
                pick_list(
                    self.create_parent_options(),
                    self.skill_create_parent.clone(),
                    Message::SkillCreateParentChanged,
                ),
            ),
            instructions_label,
            instructions_hint,
            text_editor(&self.skill_create_instructions)
                .placeholder("Skill instructions…")
                .on_action(Message::SkillCreateInstructionsChanged)
                .height(140)
                .font(theme.font_stack.mono)
                .size(theme.font_stack.base_size),
            row![
                create_button,
                button(text("Cancel").size(13))
                    .style(crate::ui::button::secondary)
                    .padding([6, 14])
                    .on_press(Message::SkillCreateCancelled),
            ]
            .spacing(SPACING_SM),
            result_line,
        ]
        .spacing(SPACING_SM)
        .into()
    }

    /// Editable form for a selected skill's `skill.toml` (ADR-43 edit). The
    /// stable `id` stays read-only; `name` and `description` edit the manifest
    /// fields directly, while the instructions body is always written inline
    /// (replacing any `instructions_path`). `version`, `tools`, and `resources`
    /// are preserved unchanged.
    fn ext_skill_edit_form<'a>(
        &'a self,
        theme: &'a AppTheme,
        skill: &'a SkillDescriptor,
        draft: &'a SkillEditDraft,
    ) -> Element<'a, Message> {
        let palette = &theme.palette;
        let busy = self.skill_crud_busy;
        let tools = if skill.manifest.tools.is_empty() {
            "none".to_string()
        } else {
            skill.manifest.tools.join(", ")
        };
        let version =
            if skill.manifest.version.is_empty() { "0.0.0" } else { &skill.manifest.version };

        let save_button: Element<'a, Message> = if busy {
            button(text("Saving…").size(13))
                .style(crate::ui::button::primary)
                .padding([6, 14])
                .into()
        } else {
            button(text("Save").size(13))
                .style(crate::ui::button::primary)
                .padding([6, 14])
                .on_press(Message::SkillEditSaved)
                .into()
        };

        let result_line: Element<'a, Message> = match &self.skill_crud_result {
            Some(result) if result.starts_with("Error") => {
                text(result).size(11).color(palette.danger).into()
            }
            Some(_) => iced::widget::Space::new().height(0).into(),
            None => iced::widget::Space::new().height(0).into(),
        };

        let instructions_row: Element<'a, Message> = row![
            text("Instructions").size(13).color(palette.text),
            text("written inline; replaces `instructions_path`").size(11).color(palette.text_muted),
        ]
        .spacing(SPACING_XS)
        .into();

        column![
            text(format!("{} — edit", skill.id)).size(16).color(palette.text),
            ext_meta_row(theme, "ID", skill.id.as_str()),
            ext_meta_row(theme, "Version", version),
            ext_meta_row(theme, "Tools", tools),
            form_field(
                theme,
                "Name",
                false,
                Some("Blank uses the id."),
                None::<&str>,
                text_input("display name", &draft.name)
                    .on_input(Message::SkillEditNameChanged)
                    .width(Length::Fill),
            ),
            form_field(
                theme,
                "Description",
                false,
                None::<&str>,
                None::<&str>,
                text_input("short description", &draft.description)
                    .on_input(Message::SkillEditDescriptionChanged)
                    .width(Length::Fill),
            ),
            instructions_row,
            text_editor(&draft.instructions)
                .placeholder("Skill instructions…")
                .on_action(Message::SkillEditInstructionsChanged)
                .height(140)
                .font(theme.font_stack.mono)
                .size(theme.font_stack.base_size),
            row![
                save_button,
                button(text("Cancel").size(13))
                    .style(crate::ui::button::secondary)
                    .padding([6, 14])
                    .on_press(Message::SkillEditCancelled),
            ]
            .spacing(SPACING_SM),
            result_line,
        ]
        .spacing(SPACING_SM)
        .into()
    }

    // ── MCP tab ──────────────────────────────────────────────────────────
    fn ext_mcp_tab<'a>(&'a self, theme: &'a AppTheme) -> Element<'a, Message> {
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
        ];

        if self.mcp_servers.is_empty() {
            master.push(
                column![
                    text(
                        "No MCP servers configured. Add `[mcp.servers]` entries to your \
                         config file."
                    )
                    .size(12)
                    .color(palette.text_muted),
                    text(
                        "Existing servers can be edited or removed from the detail pane. \
                         Adding a brand-new server still happens in the config file."
                    )
                    .size(11)
                    .color(palette.text_muted),
                ]
                .spacing(SPACING_XS)
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
            self.ext_mcp_detail(theme),
        )
    }

    fn ext_mcp_row<'a>(
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

    fn ext_mcp_detail<'a>(&'a self, theme: &'a AppTheme) -> Element<'a, Message> {
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
        } else if !self.mcp_enabled {
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
    fn ext_mcp_edit_form<'a>(
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
                        .width(Length::Fill),
                    button(text("Remove").size(13))
                        .style(crate::ui::button::danger_outline)
                        .padding([6, 10])
                        .on_press(Message::McpEditEnvRemove(i)),
                ]
                .spacing(SPACING_XS)
                .align_y(Alignment::Center)
                .into(),
            );
        }

        let mut env_field: Vec<Element<'a, Message>> = vec![
            text("Environment").size(13).color(palette.text).into(),
            text("Optional variable overrides for the server process; leave empty for none.")
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
                Some("Space-separated arguments, e.g. \"-y @example/server\""),
                None::<&str>,
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
                    .on_press(Message::McpEditSaved),
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

    // ── Plugins tab (ADR-37) ────────────────────────────────────────────
    fn ext_plugins_tab<'a>(&'a self, theme: &'a AppTheme) -> Element<'a, Message> {
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
    fn ext_plugin_install_card<'a>(&'a self) -> Element<'a, Message> {
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

    fn ext_plugin_row<'a>(
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

    fn ext_plugin_detail<'a>(&'a self, theme: &'a AppTheme) -> Element<'a, Message> {
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

    /// Smoke test: the Skills detail pane renders in read-only, edit, and
    /// delete-confirm states, and the create wizard renders, without panicking
    /// (ADR-43 create/edit/delete).
    #[test]
    fn skill_detail_renders_readonly_edit_and_delete_confirm_states() {
        let theme = AppTheme::by_name("Midnight");
        let mut state = State::from_config(&concerto_config::AppConfig::default());
        state.skills_discovered = vec![SkillDescriptor {
            id: "rust-testing".into(),
            manifest: concerto_api_types::extension::SkillManifest {
                id: "rust-testing".into(),
                name: "Rust Testing".into(),
                version: "1.0.0".into(),
                description: "Cargo verification guidance".into(),
                instructions_path: None,
                instructions: Some("Prefer cargo nextest.".into()),
                tools: vec!["cargo nextest run".into()],
                resources: Vec::new(),
            },
            instructions: "Prefer cargo nextest.".into(),
            pack_dir: std::path::PathBuf::new(),
            resource_paths: Vec::new(),
        }];
        state.ext_selected_skill = Some("rust-testing".into());

        // Read-only detail.
        let _ = state.view(&theme, false);

        // Edit mode (toggled by the "Edit" control).
        let _ = state.update(Message::SkillEditPressed("rust-testing".into()));
        let _ = state.view(&theme, false);

        // Delete-confirm state (armed, not yet confirmed).
        let _ = state.update(Message::SkillDeletePressed("rust-testing".into()));
        let _ = state.view(&theme, false);

        // Create wizard (replaces the detail).
        let _ = state.update(Message::SkillDeleteCancelled("rust-testing".into()));
        let _ = state.update(Message::SkillCreatePressed);
        let _ = state.view(&theme, false);
    }
}
