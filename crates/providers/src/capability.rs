//! Per-model tool-calling capability resolution (ADR-66 §3, extended by
//! ADR-75).
//!
//! Tool calling was historically hardcoded `true` per provider, so any
//! tool-requiring task dispatched to a model without tool support degraded
//! to silent text-only output. This module resolves
//! `supports_tool_calling` **per model** at selection time from evidence,
//! never from a model-name guess.
//!
//! # Two chains, one rule
//!
//! 1. **Dispatch capability** — may this model carry tools at all?
//!    ([`resolve_tool_support`] / [`require_tool_support`] /
//!    [`require_tool_support_with_fallback`]). Precedence:
//!
//!    1. **explicit user declaration** — `ModelProfileOverride::
//!       supports_tool_calling` (config-level intent) always wins;
//!    2. **provider-advertised metadata** — [`concerto_core::types::ModelInfo::
//!       supports_tool_calling`], captured from model discovery, wins over any
//!       heuristic;
//!    3. **optimistic default** — an unknown model with no metadata attempts
//!       native tool calling (ADR-66 §3: never silent text). Plugin-backed
//!       providers (`plugin:<id>`) are the one exception: their wire protocol
//!       has no tool ops at all, so their default is `false`.
//!
//!    **No model name participates in this chain.** A model is never declared
//!    incapable because of what it is called.
//!
//! 2. **Wire presentation tier** — how should tool schemas be presented?
//!    ([`resolve_tool_schema_mode`]). Precedence:
//!
//!    1. **explicit config dial** — `tool_schema_mode` `Strict`/`Loose`;
//!    2. **provider-advertised capability** — advertised absence selects the
//!       loose tier, advertised support selects strict;
//!    3. **last-resort name heuristic** — only when nothing above is known,
//!       the conservative weak-family markers in
//!       [`crate::adapters::schema_loose::last_resort_weak_tool_calling_model`]
//!       apply (clearly labelled last resort, never a capability fact);
//!    4. **optimistic default** — `Strict` (verbatim schema, streamed
//!       transport), matching ADR-66 §3's "unknown models attempt native
//!       first".
//!
//! The name heuristic can only influence *presentation* (level 3), never the
//! dispatch capability, and it is never consulted when advertised metadata or
//! an explicit user declaration exists.
//!
//! # Degrade, never refuse
//!
//! A model whose only gap is native tool declarations is covered by the
//! ADR-66 §4 prompt-text fallback driver ([`tool_fallback_available`]) — the
//! task still completes instead of being refused. A hard refusal
//! ([`require_tool_support_with_fallback`]) is reserved for the one dialect
//! that genuinely cannot carry tools: plugin-backed providers, whose wire
//! protocol has no tool ops (decision (a)).
//!
//! # Plugin-backed providers
//!
//! [`is_plugin_backed`] names `plugin:<id>` providers;
//! [`provider_default_supports_tools`] reports `false` for them. A
//! tool-requiring task resolved onto a plugin provider must refuse loudly
//! (ADR-66 consequence: the factory gates them to AnswerOnly tasks).

use crate::opencode::TOOL_CALLING_CAPABILITY;

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
/// Precedence: `config_override` (explicit user declaration) > `advertised`
/// (provider-advertised `list_models` metadata) > optimistic provider default.
///
/// The `model` argument is retained for API stability and call-site
/// readability only: **no model name is consulted here**. A model is never
/// declared incapable because of what it is called; the only way to obtain
/// `false` is an explicit declaration, advertised absence, or the
/// plugin-backed provider default.
pub fn resolve_tool_support(
    provider: &str,
    _model: &str,
    config_override: Option<bool>,
    advertised: Option<bool>,
) -> bool {
    config_override.or(advertised).unwrap_or_else(|| provider_default_supports_tools(provider))
}

/// Resolve the tool-schema presentation tier for one provider/model pair
/// (ADR-75), consulting the capability precedence chain.
///
/// Precedence:
///
/// 1. **explicit config dial** — an explicit [`ToolSchemaMode::Strict`] or
///    [`ToolSchemaMode::Loose`] is level-1 user intent and always wins;
/// 2. **advertised capability** — when the provider publishes per-model
///    capability metadata ([`concerto_core::types::ModelInfo::
///    supports_tool_calling`]), `Some(false)` selects the loose tier and
///    `Some(true)` selects strict, regardless of the model name;
/// 3. **last-resort name heuristic** — only when nothing above is known, the
///    conservative weak-family markers in
///    [`crate::adapters::schema_loose::last_resort_weak_tool_calling_model`]
///    apply (presentation only — they never make a model incapable);
/// 4. **optimistic default** — strict (verbatim schema, streamed transport),
///    matching ADR-66 §3's "unknown models attempt native first".
///
/// The critical property is the **default**: an unknown model with no
/// advertised metadata resolves to [`ToolSchemaMode::Strict`] (the reliable,
/// streaming tier). Defaulting unknown models to weak was the root cause of
/// the `space-bunny-free` misclassification (ADR-75).
pub fn resolve_tool_schema_mode(
    _provider: &str,
    model: &str,
    configured: concerto_config::ToolSchemaMode,
    advertised: Option<bool>,
) -> concerto_config::ToolSchemaMode {
    use concerto_config::ToolSchemaMode;
    match configured {
        // Explicit user dials are precedence level 1 and always win.
        ToolSchemaMode::Strict | ToolSchemaMode::Loose => configured,
        ToolSchemaMode::Auto => {
            if let Some(supports) = advertised {
                return if supports { ToolSchemaMode::Strict } else { ToolSchemaMode::Loose };
            }
            if crate::adapters::schema_loose::last_resort_weak_tool_calling_model(model) {
                ToolSchemaMode::Loose
            } else {
                ToolSchemaMode::Strict
            }
        }
    }
}

/// Refuse a tool-requiring resolution onto a model without tool support.
///
/// The error names provider, model, and the missing capability (ADR-66
/// §2(a)) so the failure is actionable before any spend happens.
///
/// This is the **pure** §2(a) primitive: it refuses when
/// [`resolve_tool_support`] is `false` even though the ADR-66 §4 fallback
/// could cover the gap. Selection should prefer
/// [`require_tool_support_with_fallback`], which refuses only when no tool
/// path exists at all.
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

/// Whether the ADR-66 §4 text-fallback driver can cover a missing native
/// tool support on `provider`.
///
/// The driver is prompt-text-based, so it works over any text completion —
/// including the Responses SSE path — and its requests carry no wire tool
/// declarations. The only uncoverable gap is a wire protocol that cannot
/// carry a text completion's tool instructions at all: plugin-backed
/// providers (`plugin:<id>`) are excluded from the fallback (ADR-66
/// decision (a)) and stay hard-gated to AnswerOnly tasks.
pub fn tool_fallback_available(provider: &str) -> bool {
    !is_plugin_backed(provider)
}

/// Refuse a tool-requiring resolution only when no tool path exists at all.
///
/// ADR-66 §2(a) with the §4 fallback carve-out (2026-09-08): a model whose
/// ONLY gap is native tool declarations proceeds — the automatic
/// text-fallback driver covers it and labels its turns — so a refusal at
/// selection is reserved for providers the fallback cannot cover
/// (plugin-backed, decision (a)). Everything else behaves exactly like
/// [`require_tool_support`]: explicit override and advertised flags
/// (precedence levels 1–2) still win, and a supported pair passes.
pub fn require_tool_support_with_fallback(
    provider: &str,
    model: &str,
    config_override: Option<bool>,
    advertised: Option<bool>,
) -> Result<(), concerto_core::error::ProviderError> {
    if resolve_tool_support(provider, model, config_override, advertised) {
        return Ok(());
    }
    if tool_fallback_available(provider) {
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

    /// Rule (governing principle 1): **no model name decides capability.**
    ///
    /// Inverts the old `family_table_covers_muse_and_not_its_near_misses`
    /// test, which asserted that the Responses-dialect names `muse-v2` /
    /// `muse-spark-*` resolve to `Some(false)` (no native tool support). Now
    /// that the Responses converter carries native tools, those names resolve
    /// optimistically like any unknown model: a name never decides capability.
    #[test]
    fn no_model_name_decides_tool_capability() {
        for model in [
            "muse-v2",
            "muse-v3.1",
            "muse-spark-1.3-contributor-free",
            "mimo-v2.5",
            "gpt-4o-mini",
            "space-bunny-free",
            "big-pickle",
            "some-muse-model",
            "amuse-v2",
        ] {
            assert!(
                resolve_tool_support("opencode", model, None, None),
                "{model} must resolve optimistically: names never decide capability"
            );
        }
    }

    /// ADR-66 A3: the full precedence — explicit declaration > advertised
    /// flags > optimistic provider default.
    #[test]
    fn resolution_precedence_override_beats_all() {
        // Override wins even when everything below disagrees.
        assert!(resolve_tool_support("opencode", "muse-v2", Some(true), Some(false)));
        assert!(resolve_tool_support("plugin:x", "any", Some(true), None));
        assert!(!resolve_tool_support("openai", "gpt-4", Some(false), Some(true)));
    }

    #[test]
    fn resolution_precedence_advertised_beats_default() {
        // Advertised flags win over the provider default in both directions.
        assert!(resolve_tool_support("opencode", "muse-v2", None, Some(true)));
        assert!(!resolve_tool_support("plugin:x", "any", None, Some(false)));
        assert!(resolve_tool_support("openai", "unknown-model", None, Some(true)));
        // Advertised absence is believed over the optimistic default.
        assert!(!resolve_tool_support("openai", "gpt-4", None, Some(false)));
    }

    #[test]
    fn resolution_precedence_default_is_optimistic() {
        // No metadata → provider default (native for HTTP providers), even
        // for the names the removed family table used to blacklist.
        assert!(resolve_tool_support("opencode", "muse-v2", None, None));
        assert!(resolve_tool_support("opencode", "big-pickle", None, None));
        assert!(!resolve_tool_support("plugin:x", "any", None, None));
    }

    /// `require_tool_support` refuses with provider + model + capability, and
    /// passes for supported pairs and for explicit overrides.
    ///
    /// Inverted from the old assertion that `opencode`/`muse-v2` must refuse:
    /// the Responses converter now supports tools natively, so only the
    /// plugin-backed dialect (no tool ops at all) can refuse.
    #[test]
    fn require_tool_support_refusal_names_everything() {
        // The Zen Responses-dialect names now pass — no name decides capability.
        assert!(require_tool_support("opencode", "muse-v2", None, None).is_ok());
        assert!(require_tool_support("openai", "gpt-4", None, None).is_ok());

        // A plugin-backed provider with no fallback refuses, naming everything.
        let error = require_tool_support("plugin:my-llm", "any", None, None)
            .expect_err("plugin providers have no tool ops at all");
        match error {
            concerto_core::error::ProviderError::CapabilityRefused {
                provider,
                model,
                capability,
            } => {
                assert_eq!(provider, "plugin:my-llm");
                assert_eq!(model, "any");
                assert_eq!(capability, "tool_calling");
            }
            other => panic!("expected CapabilityRefused, got: {other:?}"),
        }

        // Explicit override rescues a genuinely unsupported pair.
        assert!(require_tool_support("openai", "gpt-4", Some(true), Some(false)).is_ok());
    }

    /// The §4 fallback availability: every provider except plugin-backed
    /// ones is coverable (the driver is prompt-text-based and works over
    /// any text completion, including the Responses SSE path).
    #[test]
    fn fallback_availability_excludes_only_plugins() {
        assert!(tool_fallback_available("opencode"));
        assert!(tool_fallback_available("openai"));
        assert!(tool_fallback_available("ollama"));
        assert!(tool_fallback_available(""));
        assert!(!tool_fallback_available("plugin:my-llm"));
    }

    /// ADR-75: the tool-schema tier consults the same precedence chain as the
    /// dispatch gate. Explicit dials win; advertised metadata beats the name
    /// heuristic; an unknown model defaults to the reliable (strict) tier.
    #[test]
    fn tool_schema_tier_follows_capability_precedence() {
        use concerto_config::ToolSchemaMode;

        // Level 1 — explicit dials always win, even against advertised flags.
        assert_eq!(
            resolve_tool_schema_mode("openai", "gpt-4o", ToolSchemaMode::Loose, Some(true)),
            ToolSchemaMode::Loose
        );
        assert_eq!(
            resolve_tool_schema_mode("openai", "mimo-x", ToolSchemaMode::Strict, Some(false)),
            ToolSchemaMode::Strict
        );

        // Level 2 — advertised metadata beats the name heuristic.
        // A `*-free` model advertised as tool-capable stays strict...
        assert_eq!(
            resolve_tool_schema_mode(
                "openrouter",
                "space-bunny-free",
                ToolSchemaMode::Auto,
                Some(true)
            ),
            ToolSchemaMode::Strict
        );
        // ...and a weak-looking name advertised as tool-less goes loose.
        assert_eq!(
            resolve_tool_schema_mode("openai", "gpt-4o", ToolSchemaMode::Auto, Some(false)),
            ToolSchemaMode::Loose
        );
        // ...and an advertised-capable Responses-dialect name is strict.
        assert_eq!(
            resolve_tool_schema_mode("opencode", "muse-v2", ToolSchemaMode::Auto, Some(true)),
            ToolSchemaMode::Strict
        );

        // Level 3 — the last-resort name heuristic (presentation only; it can
        // never make a model incapable).
        assert_eq!(
            resolve_tool_schema_mode("openai", "mimo-v2.5", ToolSchemaMode::Auto, None),
            ToolSchemaMode::Loose
        );
        assert_eq!(
            resolve_tool_schema_mode("openai", "space-bunny-free", ToolSchemaMode::Auto, None),
            ToolSchemaMode::Strict
        );
        // A Responses-dialect name with no metadata keeps the optimistic
        // strict tier: the Responses converter now carries native tools, so
        // the removed family table no longer forces it loose.
        assert_eq!(
            resolve_tool_schema_mode("opencode", "muse-v2", ToolSchemaMode::Auto, None),
            ToolSchemaMode::Strict
        );

        // Default: an unknown model with no advertised metadata is OPTIMISTIC
        // (strict/streaming), never weak.
        assert_eq!(
            resolve_tool_schema_mode("openai", "some-new-model", ToolSchemaMode::Auto, None),
            ToolSchemaMode::Strict
        );
        assert_eq!(
            resolve_tool_schema_mode("", "", ToolSchemaMode::Auto, None),
            ToolSchemaMode::Strict
        );
    }

    /// The tier resolver agrees with the dispatch gate: a model the gate
    /// considers tool-capable is kept on the reliable strict/streaming tier
    /// unless a dial or advertised flag says otherwise.
    #[test]
    fn tool_schema_tier_is_optimistic_for_unknown_models() {
        use concerto_config::ToolSchemaMode;
        for model in ["gpt-4o", "claude-sonnet-4", "qwen-max", "llama-3.1-70b", ""] {
            let supports = resolve_tool_support("openai", model, None, None);
            assert!(supports, "gate is optimistic for {model:?}");
            assert_eq!(
                resolve_tool_schema_mode("openai", model, ToolSchemaMode::Auto, None),
                ToolSchemaMode::Strict,
                "tier is optimistic (strict) for {model:?}"
            );
        }
    }

    /// ADR-66 §2(a) + §4 carve-out: a tool-requiring run proceeds whenever a
    /// tool path exists — native (the Responses dialect now carries native
    /// tools) or via the labeled fallback driver — while a plugin-backed gap
    /// refuses loudly (decision (a)) with the full
    /// provider/model/capability naming. Precedence levels 1–2 are
    /// untouched.
    ///
    /// Re-scoped from the old assertion that Responses-dialect models only
    /// proceed *via the fallback driver*: they now pass on native support.
    #[test]
    fn gate_with_fallback_refuses_only_uncoverable_gaps() {
        // Responses-dialect models on Zen now pass on native tool support.
        assert!(require_tool_support_with_fallback("opencode", "muse-v2", None, None).is_ok());
        assert!(resolve_tool_support("opencode", "muse-v2", None, None));
        assert!(require_tool_support_with_fallback(
            "opencode",
            "muse-spark-1.3-contributor-free",
            None,
            None
        )
        .is_ok());
        // Capable pairs pass as before.
        assert!(require_tool_support_with_fallback("openai", "gpt-4", None, None).is_ok());
        // Plugin-backed gap: refused, naming everything.
        let error = require_tool_support_with_fallback("plugin:my-llm", "any", None, None)
            .expect_err("plugin providers stay hard-gated (ADR-66 decision (a))");
        match error {
            concerto_core::error::ProviderError::CapabilityRefused {
                provider,
                model,
                capability,
            } => {
                assert_eq!(provider, "plugin:my-llm");
                assert_eq!(model, "any");
                assert_eq!(capability, "tool_calling");
            }
            other => panic!("expected CapabilityRefused, got: {other:?}"),
        }
        // Precedence levels 1–2 unchanged: override and advertised flags win.
        assert!(require_tool_support_with_fallback("opencode", "muse-v2", Some(true), None).is_ok());
        assert!(require_tool_support_with_fallback("opencode", "muse-v2", None, Some(true)).is_ok());
        assert!(
            require_tool_support_with_fallback("plugin:my-llm", "any", Some(true), None).is_ok()
        );
    }

    /// A provider that advertises its absence is believed even for a
    /// well-known model name, and a user declaration overrides the advertised
    /// flag — the full precedence chain, asserted directly.
    #[test]
    fn advertised_metadata_and_user_declaration_beat_names() {
        // Advertised absence beats an optimistic name.
        assert!(!resolve_tool_support("openai", "gpt-4o", None, Some(false)));
        // Advertised support beats a weak-looking name.
        assert!(resolve_tool_support("openai", "mimo-v2.5", None, Some(true)));
        // User declaration beats advertised metadata.
        assert!(resolve_tool_support("openai", "gpt-4o", Some(true), Some(false)));
        assert!(!resolve_tool_support("openai", "mimo-v2.5", Some(false), Some(true)));
    }
}
