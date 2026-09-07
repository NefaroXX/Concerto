//! Per-model tool-calling capability resolution (ADR-66 §3).
//!
//! Tool calling was historically hardcoded `true` per provider, so any
//! tool-requiring task dispatched to a model without tool support degraded
//! to silent text-only output. This module resolves
//! `supports_tool_calling` **per model** at selection time with a fixed
//! precedence:
//!
//! 1. **explicit config override** — `ModelProfileOverride::
//!    supports_tool_calling`, applied by the caller (config-level concern);
//! 2. **`list_models` capability flags** — where a provider advertises
//!    per-model capability metadata (e.g. Ollama's `capabilities` array),
//!    the advertised value wins over every heuristic below;
//! 3. **built-in family table** — known model families with known
//!    capability gaps (today: Zen-served genuine `muse-v*` models, whose
//!    Responses dialect carries no tool declarations);
//! 4. **provider default** — attempt native tool calling (ADR-66 §3:
//!    unknown models attempt native first, never silent text).
//!
//! The resolution feeds `RoutingProfile::supports_tool_calling` in
//! [`crate::factory::ProviderFactory::build_profiles`], which the routing
//! engine's capability filter turns into a fail-fast refusal for
//! tool-requiring roles — before any spend.
//!
//! # Not covered by the family table
//!
//! Plugin-backed providers (`plugin:<id>`) are hard-gated separately: their
//! wire protocol has no tool ops, so [`is_plugin_backed`] names them and
//! [`provider_default_supports_tools`] reports `false`. A tool-requiring
//! task resolved onto a plugin provider must refuse loudly (ADR-66
//! consequence: the factory gates them to AnswerOnly tasks).

use crate::opencode::{needs_responses_api, TOOL_CALLING_CAPABILITY};

/// Stable capability name used in every capability refusal.
///
/// Surfaced verbatim in [`concerto_core::error::ProviderError::
/// CapabilityRefused`] and in capability-gate audit rows so refusals are
/// grep-able and diagnosable.
pub fn tool_calling_capability() -> &'static str {
    TOOL_CALLING_CAPABILITY
}

/// Whether `provider` is a plugin-backed provider (`plugin:<id>` names).
pub fn is_plugin_backed(provider: &str) -> bool {
    provider.starts_with("plugin:")
}

/// Built-in family table: does this provider/model family support native
/// tool calling?
///
/// Returns `Some(supports)` when the family is known, `None` when the model
/// is not in the table (the caller falls through to the provider default).
/// ADR-66 §5: family membership is decided by whole family tokens — the
/// Muse rule reuses the dialect heuristic, which never matches
/// `muse-spark-*` near-misses.
pub fn family_table_supports_tools(provider: &str, model: &str) -> Option<bool> {
    match provider {
        // Zen-served genuine Muse models use the Responses API dialect,
        // which carries no tool declarations at all (ADR-66 context).
        "opencode" if needs_responses_api(model) => Some(false),
        _ => None,
    }
}

/// Provider-level default for native tool-calling support.
///
/// Every HTTP provider family defaults to *attempting* native tools (ADR-66
/// §3: unknown models attempt native first — never silent text). Plugin
/// providers have no tool ops in their protocol at all, so their default is
/// `false` and tool-requiring tasks on them refuse loudly (decision (a) of
/// the ADR-66 implementation: gated to AnswerOnly tasks with an explicit
/// error).
pub fn provider_default_supports_tools(provider: &str) -> bool {
    !is_plugin_backed(provider)
}

/// Resolve `supports_tool_calling` for one provider/model pair (ADR-66 §3).
///
/// Precedence: `config_override` > `advertised` (list_models capability
/// flags where the provider publishes them) > built-in family table >
/// provider default. `config_override` is threaded through for a single
/// source of truth; profile construction that applies overrides after the
/// fact passes `None` here and keeps its existing override application
/// (same precedence by construction).
pub fn resolve_tool_support(
    provider: &str,
    model: &str,
    config_override: Option<bool>,
    advertised: Option<bool>,
) -> bool {
    config_override
        .or(advertised)
        .or_else(|| family_table_supports_tools(provider, model))
        .unwrap_or_else(|| provider_default_supports_tools(provider))
}

/// Refuse a tool-requiring resolution onto a model without tool support.
///
/// The error names provider, model, and the missing capability (ADR-66
/// §2(a)) so the failure is actionable before any spend happens.
pub fn require_tool_support(
    provider: &str,
    model: &str,
    config_override: Option<bool>,
    advertised: Option<bool>,
) -> Result<(), concerto_core::error::ProviderError> {
    if resolve_tool_support(provider, model, config_override, advertised) {
        return Ok(());
    }
    Err(concerto_core::error::ProviderError::CapabilityRefused {
        provider: provider.to_string(),
        model: model.to_string(),
        capability: tool_calling_capability().to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_default_is_native_for_http_providers() {
        assert!(provider_default_supports_tools("openai"));
        assert!(provider_default_supports_tools("anthropic"));
        assert!(provider_default_supports_tools("google"));
        assert!(provider_default_supports_tools("ollama"));
        assert!(provider_default_supports_tools("opencode"));
        assert!(provider_default_supports_tools("openrouter"));
        assert!(provider_default_supports_tools("nim"));
        assert!(provider_default_supports_tools(""));
    }

    /// Plugin-backed providers are hard-gated: no tool ops in their wire
    /// protocol (ADR-66 consequence, decision (a)).
    #[test]
    fn plugin_backed_providers_default_to_no_tools() {
        assert!(is_plugin_backed("plugin:my-llm"));
        assert!(!is_plugin_backed("openai"));
        assert!(!is_plugin_backed("pluginish"));
        assert!(!provider_default_supports_tools("plugin:my-llm"));
    }

    /// Family table: genuine Zen-served Muse models have no tool support
    /// (Responses dialect carries no tool declarations); near-misses are
    /// not in the family and keep the provider default.
    #[test]
    fn family_table_covers_muse_and_not_its_near_misses() {
        assert_eq!(family_table_supports_tools("opencode", "muse-v2"), Some(false));
        assert_eq!(family_table_supports_tools("opencode", "muse-v3.1"), Some(false));
        // The live hazard from ADR-66 must NOT classify as Muse.
        assert_eq!(
            family_table_supports_tools("opencode", "muse-spark-1.3-contributor-free"),
            None
        );
        assert_eq!(family_table_supports_tools("opencode", "big-pickle"), None);
        assert_eq!(family_table_supports_tools("openai", "muse-v2"), None);
    }

    /// ADR-66 A3: the full precedence — override > advertised flags >
    /// family table > provider default.
    #[test]
    fn resolution_precedence_override_beats_all() {
        // Override wins even when everything below disagrees.
        assert!(resolve_tool_support("opencode", "muse-v2", Some(true), Some(false)));
        assert!(resolve_tool_support("plugin:x", "any", Some(true), None));
        assert!(!resolve_tool_support("openai", "gpt-4", Some(false), Some(true)));
    }

    #[test]
    fn resolution_precedence_advertised_beats_table_and_default() {
        // Advertised flags win over the family table.
        assert!(resolve_tool_support("opencode", "muse-v2", None, Some(true)));
        // ...and over the provider default.
        assert!(!resolve_tool_support("plugin:x", "any", None, Some(false)));
        assert!(resolve_tool_support("openai", "unknown-model", None, Some(true)));
    }

    #[test]
    fn resolution_precedence_table_beats_default() {
        // Family table wins over the provider default.
        assert!(!resolve_tool_support("opencode", "muse-v2", None, None));
        // No table entry → provider default.
        assert!(resolve_tool_support("opencode", "big-pickle", None, None));
        assert!(!resolve_tool_support("plugin:x", "any", None, None));
    }

    /// `require_tool_support` refuses with provider + model + capability,
    /// and passes for supported pairs and for explicit overrides.
    #[test]
    fn require_tool_support_refusal_names_everything() {
        let error = require_tool_support("opencode", "muse-v2", None, None)
            .expect_err("Muse models must refuse tool tasks");
        match error {
            concerto_core::error::ProviderError::CapabilityRefused {
                provider,
                model,
                capability,
            } => {
                assert_eq!(provider, "opencode");
                assert_eq!(model, "muse-v2");
                assert_eq!(capability, "tool_calling");
            }
            other => panic!("expected CapabilityRefused, got: {other:?}"),
        }

        assert!(require_tool_support("openai", "gpt-4", None, None).is_ok());
        // Explicit override rescues a table-miss.
        assert!(require_tool_support("opencode", "muse-v2", Some(true), None).is_ok());
    }
}
