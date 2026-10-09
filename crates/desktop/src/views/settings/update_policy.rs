//! Policy and memory/retry update arms (Settings → Policy / Memory / Retry).
//!
//! Pure relocation (NORM S35): two ADJACENT arm groups of
//! [`super::state::State::update`] move verbatim from
//! `views/settings/state.rs` into this file as two additional inherent
//! `impl State` methods — the same pattern as [`super::update_mcp`],
//! [`super::update_skills`], [`super::update_providers`],
//! [`super::update_plugins`], [`super::shell`], and [`super::state_sync`].
//!
//! The two groups were adjacent in the pre-refactor `update` body (the S28 map
//! recorded them as a single span, `state.rs` lines ~1277–1398 before any of
//! the arm-group slices), so they share this file:
//!
//! * [`State::update_policy`] — the 9-arm policy rule-builder (`NewPolicy*`
//!   draft edits plus the `PolicyRule*` list mutations).
//! * [`State::update_memory`] — the 10-arm memory / retry tuning knobs
//!   (`Memory*` and `Retry*`).
//!
//! A single method was rejected: naming the 10 memory/retry arms
//! `update_policy` would misdescribe them. The groups are distinct domains even
//! though they share a persistence tier (both mutate state that is written only
//! on an explicit Save Settings).
//!
//! No behavior, signature, call-site, or [`super::Message`] shape change: the
//! parent `update` keeps two thin delegating OR-pattern arms that forward the
//! policy and memory/retry variants here. Both helpers take the full
//! [`super::Message`] (not a sub-enum) and return `iced::Task<Message>`. The
//! relocated arms are pure state mutations, but keeping the shared signature
//! preserves the exact early-return semantics and the fallback tail. As in the
//! parent, each `match` is a statement and each function tail is
//! `iced::Task::none()`. The fallback arm is unreachable through the parent
//! dispatcher and is a documented no-op.
//!
//! Test coverage for these arms lives in the `views/settings/helpers.rs` and
//! `views/settings/state.rs` test modules and drives the public
//! [`super::state::State::update`] dispatch, so it stays put.

use concerto_config::{ConditionDef, PolicyRuleDef};

use super::state::State;
use super::{Message, PolicyConditionChoice, FILESYSTEM_OPERATIONS, POLICY_OPERATION_TOOLS};

impl State {
    /// Handle the policy rule-builder `Message` group of Settings.
    ///
    /// The parent `State::update` routes every `NewPolicy*` / `PolicyRule*`
    /// variant here. Returns the arm's `Task` unchanged; a non-policy message
    /// (never routed by the parent) is a no-op.
    pub(super) fn update_policy(&mut self, message: Message) -> iced::Task<Message> {
        match message {
            // Policy (kept)
            Message::NewPolicyActionSelected(a) => {
                self.settings_dirty = true;
                self.new_policy_action = a;
            }
            Message::NewPolicyConditionKindSelected(k) => {
                self.settings_dirty = true;
                self.new_policy_condition_kind = k;
                if k == PolicyConditionChoice::ToolOperation
                    && !POLICY_OPERATION_TOOLS.contains(&self.new_policy_tool)
                {
                    self.new_policy_tool = POLICY_OPERATION_TOOLS[0];
                    self.new_policy_operation = FILESYSTEM_OPERATIONS[0];
                }
            }
            Message::NewPolicyToolSelected(tool) => {
                self.settings_dirty = true;
                self.new_policy_tool = tool;
                self.new_policy_operation = Self::operation_options(tool)[0];
            }
            Message::NewPolicyOperationSelected(operation) => {
                self.settings_dirty = true;
                self.new_policy_operation = operation;
            }
            Message::NewPolicyConditionValueChanged(v) => {
                self.settings_dirty = true;
                self.new_policy_condition_value = v;
            }
            Message::PolicyRuleAdded => {
                self.settings_dirty = true;
                let condition = match self.new_policy_condition_kind {
                    PolicyConditionChoice::Tool => {
                        ConditionDef::ToolName { tool_name: self.new_policy_tool.to_string() }
                    }
                    PolicyConditionChoice::ToolOperation => ConditionDef::ToolOperation {
                        tool_name: self.new_policy_tool.to_string(),
                        operation: self.new_policy_operation.to_string(),
                    },
                    PolicyConditionChoice::ProjectPath => ConditionDef::PathGlob {
                        path_glob: self.new_policy_condition_value.clone(),
                    },
                    PolicyConditionChoice::ShellCommand => ConditionDef::CommandPattern {
                        command_pattern: self.new_policy_condition_value.clone(),
                    },
                    PolicyConditionChoice::Always => ConditionDef::Always { always: true },
                };
                if matches!(
                    self.new_policy_condition_kind,
                    PolicyConditionChoice::Tool
                        | PolicyConditionChoice::ToolOperation
                        | PolicyConditionChoice::Always
                ) || !self.new_policy_condition_value.trim().is_empty()
                {
                    self.policy_rules.push(PolicyRuleDef {
                        action: self.new_policy_action.config_value().to_string(),
                        condition,
                    });
                    self.new_policy_condition_value.clear();
                }
            }
            Message::PolicyRuleRemoved(idx) => {
                self.settings_dirty = true;
                if idx < self.policy_rules.len() {
                    self.policy_rules.remove(idx);
                }
            }
            Message::PolicyRuleMovedUp(idx) => {
                self.settings_dirty = true;
                if idx > 0 && idx < self.policy_rules.len() {
                    self.policy_rules.swap(idx, idx - 1);
                }
            }
            Message::PolicyRuleMovedDown(idx) => {
                self.settings_dirty = true;
                if idx + 1 < self.policy_rules.len() {
                    self.policy_rules.swap(idx, idx + 1);
                }
            }
            _ => {}
        }
        iced::Task::none()
    }

    /// Handle the memory / retry tuning `Message` group of Settings.
    ///
    /// The parent `State::update` routes every `Memory*` / `Retry*` variant
    /// here. Returns the arm's `Task` unchanged; a non-memory message (never
    /// routed by the parent) is a no-op.
    pub(super) fn update_memory(&mut self, message: Message) -> iced::Task<Message> {
        match message {
            Message::MemoryEnabledToggled(v) => {
                self.settings_dirty = true;
                self.memory_enabled = v;
            }
            Message::MemoryTtlChanged(v) => {
                self.settings_dirty = true;
                self.memory_ttl_days = v.clamp(1.0, 365.0);
            }
            Message::RetryEnabledToggled(value) => {
                self.settings_dirty = true;
                self.retry_enabled = value;
            }
            Message::RetryInitialDelayChanged(value) => {
                self.settings_dirty = true;
                self.retry_initial_delay_ms = value;
            }
            Message::RetryMaxDelayChanged(value) => {
                self.settings_dirty = true;
                self.retry_max_delay_ms = value;
            }
            Message::RetryMultiplierChanged(value) => {
                self.settings_dirty = true;
                self.retry_multiplier = value;
            }
            Message::RetryFixedDelayChanged(value) => {
                self.settings_dirty = true;
                self.retry_fixed_delay_ms = value;
                self.retry_fixed_delay_error =
                    Self::validate_optional_positive_int(&self.retry_fixed_delay_ms);
            }
            Message::RetryRespectAfterToggled(value) => {
                self.settings_dirty = true;
                self.retry_respect_after = value;
            }
            Message::RetryJitterToggled(value) => {
                self.settings_dirty = true;
                self.retry_jitter = value;
            }
            Message::RetryMaxElapsedChanged(value) => {
                self.settings_dirty = true;
                self.retry_max_elapsed_seconds = value;
                self.retry_max_elapsed_error =
                    Self::validate_optional_positive_int(&self.retry_max_elapsed_seconds);
            }
            _ => {}
        }
        iced::Task::none()
    }
}
