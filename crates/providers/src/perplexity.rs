//! Perplexity provider.
//!
//! Perplexity serves an OpenAI-compatible Chat Completions API at
//! `https://api.perplexity.ai/chat/completions` (no `/v1` prefix — the inner
//! connector appends the path). This provider is a thin wrapper around
//! [`OpenAiProvider`] pointed at the Perplexity endpoint, in the same style
//! as the OpenRouter, NVIDIA NIM, DeepSeek, and Groq wrappers.
//!
//! Perplexity's `sonar` family is available on both the legacy Chat
//! Completions surface and the newer Responses-style API; this wrapper uses
//! the Chat Completions path like every other OpenAI-compatible connector.
//!
//! Like every OpenAI-compatible wrapper it defaults to
//! [`ReasoningEcho::IfPresent`] (echo reasoning content only when the upstream
//! reported any); the per-config reasoning-echo dial is honored via
//! [`PerplexityProvider::with_reasoning_echo`].

use async_trait::async_trait;
use concerto_core::error::ProviderError;
use concerto_core::traits::{CompletionStream, LlmProvider};
use concerto_core::types::{CompletionRequest, ModelInfo, TokenBudget};
use concerto_core::CancellationToken;
use concerto_core::SecretString;

use crate::openai::{OpenAiProvider, ReasoningEcho};

/// Default Perplexity API base URL.
///
/// Perplexity's OpenAI-compatible surface has no `/v1` segment; the connector
/// appends `/chat/completions` and `/models` to this base.
pub(crate) const PERPLEXITY_API_BASE: &str = "https://api.perplexity.ai";

/// Thin OpenAI-compatible wrapper for Perplexity.
///
/// Tool-call normalization, streaming, loose-schema adaptation (ADR-66 §4),
/// the flat proxy tool-call fallback, and reasoning capture all come from the
/// underlying [`OpenAiProvider`]; this struct only fixes the endpoint.
pub struct PerplexityProvider {
    inner: OpenAiProvider,
}

impl PerplexityProvider {
    /// Build a provider targeting the Perplexity endpoint.
    pub fn new(api_key: impl Into<SecretString>, model: String, timeout_secs: u64) -> Self {
        Self {
            inner: OpenAiProvider::new(api_key, model, timeout_secs)
                .with_api_base(PERPLEXITY_API_BASE.to_string()),
        }
    }

    openai_wrapper_forwarders!(inner, [
        with_api_base,
        with_reasoning_echo,
        with_tool_schema_mode,
        with_advertised_tool_support => [
            /// Forward the provider-advertised per-model tool-calling capability
            /// (ADR-66 §3 precedence level 2) to the inner OpenAI-compatible
            /// provider, so an advertised flag beats the last-resort name heuristic.
        ],
    ]);
}

openai_wrapper_forwarders!(llm PerplexityProvider, inner,
    name: "perplexity",
    capacity: forward,
    // Perplexity representative pricing: `sonar-pro` at $3.00 input /
    // $15.00 output per MTok (the default model; the higher-priced tier
    // of the sonar family).
    cost: per_mtok(3.00, 15.00),
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_name_is_perplexity() {
        let provider = PerplexityProvider::new("key".to_string(), "sonar-pro".to_string(), 15);
        assert_eq!(provider.provider_name(), "perplexity");
    }

    #[test]
    fn default_api_base_is_the_perplexity_endpoint() {
        assert_eq!(PERPLEXITY_API_BASE, "https://api.perplexity.ai");
    }

    #[test]
    fn context_capacity_uses_perplexity_budget() {
        let provider = PerplexityProvider::new("key".to_string(), "sonar-pro".to_string(), 15);
        assert_eq!(provider.context_capacity("sonar-pro").capacity, 200_000);
    }

    #[test]
    fn approximate_cost_scales_with_tokens() {
        let provider = PerplexityProvider::new("key".to_string(), "sonar-pro".to_string(), 15);
        let small = provider.approximate_cost(1_000_000, 1_000_000);
        assert!(small > 0.0, "cost must be positive");
        assert!(
            provider.approximate_cost(2_000_000, 2_000_000) > small,
            "cost must scale with token volume"
        );
    }

    /// The canonical `function {name, arguments}` tool-call shape parses
    /// through the Perplexity provider (inherited from the inner
    /// `OpenAiProvider`).
    #[tokio::test]
    async fn stream_parses_function_shaped_tool_calls() {
        crate::testing::mock_server::assert_function_shaped_tool_calls(|base| {
            Box::new(
                PerplexityProvider::new("sk-test".to_string(), "sonar-pro".to_string(), 15)
                    .with_api_base(base),
            )
        })
        .await;
    }

    /// Fix 1 regression: the flat proxy fallback (`name` / `arguments`
    /// directly on the tool-call object, split across SSE deltas) also works
    /// through the Perplexity provider.
    #[tokio::test]
    async fn stream_parses_flat_shaped_tool_calls() {
        crate::testing::mock_server::assert_flat_shaped_tool_calls(|base| {
            Box::new(
                PerplexityProvider::new("sk-test".to_string(), "sonar-pro".to_string(), 15)
                    .with_api_base(base),
            )
        })
        .await;
    }
}
