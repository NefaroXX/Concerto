//! Policy rule configuration and the tool-schema presentation tier.
//!
//! This module owns the `[[policy.rules]]` surface — [`PolicyConfig`],
//! [`PolicyTimeWindowConfig`], [`PolicyRuleDef`], [`ConditionDef`] and the
//! [`POLICY_ACTIONS`] action allowlist — plus the provider-side
//! [`ToolSchemaMode`] dial and its parser ([`parse_tool_schema_mode`]).
//! The bodies move verbatim from `schema.rs`; the parent module re-exports
//! them so every existing `crate::schema::{...}` path — including the
//! crate-root re-exports in `lib.rs` — keeps resolving unchanged. No tests
//! moved: the parent's single `mod tests` starts below the extracted span, so
//! it stays put and resolves these items through that re-export.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Phase 2 — policy rule configuration
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PolicyConfig {
    pub rules: Vec<PolicyRuleDef>,
    /// Optional time window configuration for auto-approval.
    pub time_window: Option<PolicyTimeWindowConfig>,
    /// Approval deadline (seconds) for approval-producing rules that do not
    /// carry an explicit timeout. Additive/optional: absent configs default to
    /// 30s, preserving pre-existing behavior.
    ///
    /// **Retained but not enforced.** The value still travels with the policy
    /// verdict (config and API compatibility), yet approvals never expire:
    /// the session pauses until permission is given or revoked, and only a
    /// user decision, an unanswerable sink, or run cancellation resolves the
    /// request.
    #[serde(default)]
    pub approval_timeout_secs: Option<u64>,
}

/// Time window configuration for auto-approval of low-cost operations
/// outside business hours.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PolicyTimeWindowConfig {
    /// Start hour (0-23, inclusive).
    pub start_hour: u8,
    /// End hour (0-23, inclusive).
    pub end_hour: u8,
    /// IANA timezone name, e.g. "America/New_York".
    pub timezone: String,
    /// Operations with estimated cost below this threshold are auto-approved
    /// when the current time is outside the configured window.
    pub auto_approve_below_usd: f64,
}

impl PolicyTimeWindowConfig {
    /// Validate the timezone string at config load time.
    pub fn validate(&self) -> Result<(), crate::ConfigError> {
        self.timezone.parse::<chrono_tz::Tz>().map(|_| ()).map_err(|e| {
            crate::ConfigError::InvalidValue(format!("invalid timezone '{}': {}", self.timezone, e))
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PolicyRuleDef {
    pub action: String,
    pub condition: ConditionDef,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
#[non_exhaustive]
/// Policy condition definitions (ADR-43 §6): `ToolNamePrefix` matches any tool
/// whose name starts with the prefix (e.g. `mcp:github:`), enabling
/// server-level MCP policy rules. `ResolvedPathGlob` matches a path-shaped
/// action's resolved target path (after workspace containment) against a glob,
/// unlike `PathGlob`, which matches the caller-supplied input path.
pub enum ConditionDef {
    ToolOperation { tool_name: String, operation: String },
    ToolName { tool_name: String },
    ToolNamePrefix { tool_name_prefix: String },
    PathGlob { path_glob: String },
    ResolvedPathGlob { resolved_path_glob: String },
    CommandPattern { command_pattern: String },
    GitOperation { git_operation: String },
    Always { always: bool },
}

/// Action strings accepted in a `[[policy.rules]]` entry.
///
/// Single source of truth for load-time validation ([`PolicyConfig::validate`])
/// and conversion ([`PolicyConfig::to_rules`]): an unrecognized action must
/// fail config load rather than silently degrade to a blanket deny.
///
/// Keep in sync with the `match` in [`PolicyConfig::to_rules`] — every entry
/// here must map to a real `PolicyRule` variant (the
/// `all_valid_policy_actions_map_as_before` test enforces that direction).
pub const POLICY_ACTIONS: [&str; 6] = [
    "auto_approve",
    "auto_deny",
    "require_approval",
    "require_managed_tool_approval",
    "require_toolchain_approval",
    "deny_network_egress",
];

impl PolicyConfig {
    /// Validate rule actions at config load time.
    ///
    /// Every `action` must be one of [`POLICY_ACTIONS`]. Without this check a
    /// typo (e.g. `"autoaprove"`) would silently become
    /// `PolicyRule::AutoDeny(Condition::Always)` in [`Self::to_rules`] — a
    /// blanket deny with no signal to the user.
    pub fn validate(&self) -> Result<(), crate::ConfigError> {
        for (index, rule) in self.rules.iter().enumerate() {
            if !POLICY_ACTIONS.contains(&rule.action.as_str()) {
                return Err(crate::ConfigError::InvalidValue(format!(
                    "policy.rules[{index}]: unrecognized action '{}' — allowed actions: {}",
                    rule.action,
                    POLICY_ACTIONS.join(", ")
                )));
            }
        }
        Ok(())
    }

    /// Convert config definitions to `concerto_core` `PolicyRule` enums.
    pub fn to_rules(&self) -> Vec<concerto_core::types::PolicyRule> {
        use concerto_core::types::{Condition, PolicyRule};

        self.rules
            .iter()
            .enumerate()
            .map(|(index, def)| {
                let condition = def.condition.to_condition();
                match def.action.as_str() {
                    "auto_approve" => PolicyRule::AutoApprove(condition),
                    "auto_deny" => PolicyRule::AutoDeny(condition),
                    "require_approval" => PolicyRule::RequireApproval(condition),
                    "require_managed_tool_approval" => {
                        PolicyRule::RequireManagedToolApproval(condition)
                    }
                    "require_toolchain_approval" => PolicyRule::RequireToolchainApproval(condition),
                    "deny_network_egress" => PolicyRule::DenyNetworkEgress(condition),
                    // Defensive only: `PolicyConfig::validate` rejects unknown
                    // actions at config load, so this arm is unreachable for
                    // loaded configs. Keep the historical blanket-deny outcome
                    // (fail closed) but make it observable.
                    _ => {
                        tracing::warn!(
                            rule_index = index,
                            action = %def.action,
                            allowed = %POLICY_ACTIONS.join(", "),
                            "unrecognized policy rule action; defaulting rule to \
                             AutoDeny(Always) — config load should have rejected it"
                        );
                        PolicyRule::AutoDeny(Condition::Always)
                    }
                }
            })
            .collect()
    }

    /// Convert time window config to `concerto_core` `TimeWindowCondition`.
    pub fn to_time_window(&self) -> Option<concerto_core::policy::TimeWindowCondition> {
        self.time_window.as_ref().map(|tw| concerto_core::policy::TimeWindowCondition {
            start_hour: tw.start_hour,
            end_hour: tw.end_hour,
            timezone: tw.timezone.clone(),
            auto_approve_below_usd: tw.auto_approve_below_usd,
        })
    }
}

impl ConditionDef {
    // `pub(super)`, not private: the parent module's tests (`schema::tests`)
    // live outside this submodule and call this directly — `pub(super)` keeps
    // exactly the pre-extraction visibility scope (the `schema` subtree).
    pub(super) fn to_condition(&self) -> concerto_core::types::Condition {
        use concerto_core::types::Condition;
        match self {
            ConditionDef::ToolName { tool_name } => Condition::ToolName(tool_name.clone()),
            ConditionDef::ToolNamePrefix { tool_name_prefix } => {
                Condition::ToolNamePrefix(tool_name_prefix.clone())
            }
            ConditionDef::ToolOperation { tool_name, operation } => Condition::All(vec![
                Condition::ToolName(tool_name.clone()),
                Condition::Operation(operation.clone()),
            ]),
            ConditionDef::PathGlob { path_glob } => Condition::PathGlob(path_glob.clone()),
            ConditionDef::ResolvedPathGlob { resolved_path_glob } => {
                Condition::ResolvedPathGlob(resolved_path_glob.clone())
            }
            ConditionDef::CommandPattern { command_pattern } => {
                Condition::CommandPattern(command_pattern.clone())
            }
            ConditionDef::GitOperation { git_operation } => Condition::All(vec![
                Condition::ToolName("git".into()),
                Condition::Operation(git_operation.clone()),
            ]),
            ConditionDef::Always { always: _ } => Condition::Always,
        }
    }
}

// ---- Tool-schema presentation tier (adaptive tool schemas) -------------------

/// How tool parameter schemas are presented to a model on the wire.
///
/// Weak tool-calling models (audit: the MiMo family) stall on nested
/// JSON-Schema tool parameters: they emit `null` arguments, hallucinate keys,
/// and miss required nested fields. "Loose" presentation flattens nested
/// object properties to dot-notation leaves, appends argument examples to
/// tool descriptions, and spells out enum members in property descriptions;
/// the provider connector re-nests dot-notation arguments on the way back so
/// the executor and the tool-call guard still see the original nested shape
/// (see `concerto_providers::adapters::schema_loose`).
///
/// The user-facing dial lives on each `[providers.*]` entry as the
/// `tool_schema_mode` string (`"auto"` | `"strict"` | `"loose"`); this enum is
/// the parsed, provider-side value. Default is [`ToolSchemaMode::Auto`]:
/// unknown model names keep today's verbatim ("strict") schema — only names
/// matching the last-resort weak-family heuristic are adapted. A `*-free`/
/// `:free` route token is a price tier, never a capability signal (ADR-75).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ToolSchemaMode {
    /// Decide per resolved model name: weak tool-callers (last-resort
    /// heuristic, see `concerto_providers::adapters::schema_loose`) get loose
    /// schemas, every other model keeps the verbatim strict schema.
    #[default]
    Auto,
    /// Always send the tool schema verbatim (strong tool-calling models).
    Strict,
    /// Always send the adapted loose schema, regardless of model name.
    Loose,
}

/// Parse the `[providers.*] tool_schema_mode` dial into a [`ToolSchemaMode`].
///
/// Lenient like the `reasoning_echo` dial: `None`, empty, and unrecognized
/// values resolve to [`ToolSchemaMode::Auto`] so configs stay
/// forward-compatible. Unknown-but-present values are logged by the factory,
/// which is the only caller that has logging context.
pub fn parse_tool_schema_mode(raw: Option<&str>) -> ToolSchemaMode {
    match raw.map(str::trim).filter(|value| !value.is_empty()) {
        Some(value) if value.eq_ignore_ascii_case("strict") => ToolSchemaMode::Strict,
        Some(value) if value.eq_ignore_ascii_case("loose") => ToolSchemaMode::Loose,
        _ => ToolSchemaMode::Auto,
    }
}
