//! Policy, retry, and memory section view builders (Settings page).
//!
//! Pure relocation (NORM S51): the Policy rule-builder cluster of the settings
//! page moves verbatim from `views/settings/mod.rs` into this file as an
//! inherent `impl State` block — the same pattern as [`super::shell`]
//! (`shell_section`), the `view_providers` builder (NORM S50), and the
//! `ext_*_view` tab builders. The cluster is the condition-input `match`, the
//! add-rule control
//! with its validity gate, the preview/help lines, and the ordered rule rows.
//!
//! The contiguous Provider Retry & Recovery and Memory blocks that follow it
//! in `view()` are folded in alongside (`retry_section`, `memory_section`) —
//! they are separated from the policy block by comment lines only, are fully
//! self-contained, and each is just a `collapsible_section` content column.
//! No behavior, signature, call-site, or [`super::Message`] shape change: the
//! parent `view` keeps wrapping every returned content element in its
//! `collapsible_section` card exactly as before.
//!
//! The shared items (`POLICY_*` tables, `PolicyConditionChoice`) stay in
//! `mod.rs` (they also feed `state.rs` and `update_policy.rs`) and enter this
//! file through `use super::…`. Every member is `pub(super)`: from this child
//! module `super` is `views::settings`, so `pub(super)` resolves to
//! `views::settings` plus its descendants — exactly the effective scope a
//! private item in `mod.rs` had. The full-page render tests and the policy /
//! retry / memory state tests stay put (`helpers.rs`, `mod.rs`): they exercise
//! `State::update` / `to_config` and the whole `view`, not these builders
//! alone.

use iced::widget::{button, checkbox, column, container, pick_list, row, text, text_input};
use iced::{Element, Length};

use crate::theme::AppTheme;
use crate::ui::{form_field, labeled_slider, SPACING_SM, SPACING_XS};

use super::{
    Message, PolicyConditionChoice, State, POLICY_ACTIONS, POLICY_CONDITION_KINDS,
    POLICY_OPERATION_TOOLS, POLICY_TOOLS,
};

impl State {
    /// Content of the Policy Rules section: the vertical rule builder, the
    /// inline preview/help lines, and the ordered rule list. The caller
    /// (`view`) wraps this in the `collapsible_section` card.
    pub(super) fn policy_section<'a>(&'a self, theme: &'a AppTheme) -> Element<'a, Message> {
        let palette = &theme.palette;

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
        .spacing(SPACING_SM)
        .into()
    }

    /// Content of the Provider Retry & Recovery section: the enable toggle,
    /// timing/limit controls, and the behavior toggles.
    pub(super) fn retry_section<'a>(&'a self, theme: &'a AppTheme) -> Element<'a, Message> {
        let palette = &theme.palette;

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
        .spacing(SPACING_SM)
        .into()
    }

    /// Content of the Memory Settings section: the enable toggle and TTL.
    pub(super) fn memory_section<'a>(&'a self, theme: &'a AppTheme) -> Element<'a, Message> {
        column![
            checkbox(self.memory_enabled).label("Enabled").on_toggle(Message::MemoryEnabledToggled),
            labeled_slider(
                theme,
                "TTL",
                self.memory_ttl_days,
                1.0..=365.0,
                Message::MemoryTtlChanged,
                |v| format!("{:.0} days", v),
            ),
        ]
        .spacing(SPACING_SM)
        .into()
    }
}
