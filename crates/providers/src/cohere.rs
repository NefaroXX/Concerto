//! Cohere provider.
//!
//! Cohere's Compatibility API exposes an OpenAI-compatible Chat Completions
//! surface (`/compatibility/v1/chat/completions`) for the Command family and
//! hosted open-weight models. This provider is a thin wrapper around
//! [`OpenAiProvider`] pointed at the Cohere endpoint, in the same style as
//! the OpenRouter, NVIDIA NIM, and DeepSeek wrappers.
//!
//! Like every OpenAI-compatible wrapper it defaults to
//! [`ReasoningEcho::IfPresent`] (echo reasoning content only when the
//! upstream reported any); the per-config reasoning-echo dial is honored via
//! [`CohereProvider::with_reasoning_echo`].

use async_trait::async_trait;
use concerto_core::error::ProviderError;
use concerto_core::traits::{CompletionStream, LlmProvider};
use concerto_core::types::{CompletionRequest, ModelInfo, TokenBudget};
use concerto_core::CancellationToken;
use concerto_core::SecretString;

use crate::openai::{OpenAiProvider, ReasoningEcho};

/// Default Cohere API base URL.
///
/// The `/compatibility/v1` prefix is the OpenAI-SDK-compatible form
/// (`/chat/completions` and `/models` are appended by the inner connector).
///
/// Cohere's compatibility quickstart currently shows the `api.cohere.ai`
/// host; `api.cohere.com` is the provider's primary API domain and serves
/// the same compatibility surface. We pin the `.com` host per the Tier-1
/// integration spec (docs/missing-providers.md is stale on this point).
pub(crate) const COHERE_API_BASE: &str = "https://api.cohere.com/compatibility/v1";

/// Thin OpenAI-compatible wrapper for Cohere.
///
/// Tool-call normalization, streaming, loose-schema adaptation (ADR-66 §4),
/// the flat proxy tool-call fallback, and reasoning capture all come from the
/// underlying [`OpenAiProvider`]; this struct only fixes the endpoint.
pub struct CohereProvider {
    inner: OpenAiProvider,
}

impl CohereProvider {
    /// Build a provider targeting the Cohere endpoint.
    pub fn new(api_key: impl Into<SecretString>, model: String, timeout_secs: u64) -> Self {
        Self {
            inner: OpenAiProvider::new(api_key, model, timeout_secs)
                .with_api_base(COHERE_API_BASE.to_string()),
        }
    }

    openai_wrapper_forwarders!(inner);
}

openai_wrapper_forwarders!(llm CohereProvider, inner,
    name: "cohere",
    capacity: forward,
    // Cohere representative pricing: `command-a` at $2.50 input / $10.00
    // output per MTok (enterprise Command tier).
    cost: per_mtok(2.50, 10.00),
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_name_is_cohere() {
        let provider =
            CohereProvider::new("key".to_string(), "command-a-plus-05-2026".to_string(), 15);
        assert_eq!(provider.provider_name(), "cohere");
    }

    #[test]
    fn default_api_base_is_the_cohere_compatibility_endpoint() {
        assert_eq!(COHERE_API_BASE, "https://api.cohere.com/compatibility/v1");
    }

    #[test]
    fn context_capacity_uses_cohere_budget() {
        let provider =
            CohereProvider::new("key".to_string(), "command-a-plus-05-2026".to_string(), 15);
        assert_eq!(provider.context_capacity("command-a-plus-05-2026").capacity, 128_000);
    }

    #[test]
    fn approximate_cost_scales_with_tokens() {
        let provider =
            CohereProvider::new("key".to_string(), "command-a-plus-05-2026".to_string(), 15);
        let small = provider.approximate_cost(1_000_000, 1_000_000);
        assert!(small > 0.0, "cost must be positive");
        assert!(
            provider.approximate_cost(2_000_000, 2_000_000) > small,
            "cost must scale with token volume"
        );
    }

    /// The canonical `function {name, arguments}` tool-call shape parses
    /// through the Cohere provider (inherited from the inner `OpenAiProvider`).
    #[tokio::test]
    async fn stream_parses_function_shaped_tool_calls() {
        crate::testing::mock_server::assert_function_shaped_tool_calls(|base| {
            Box::new(
                CohereProvider::new(
                    "sk-test".to_string(),
                    "command-a-plus-05-2026".to_string(),
                    15,
                )
                .with_api_base(base),
            )
        })
        .await;
    }

    /// Fix 1 regression: the flat proxy fallback (`name` / `arguments`
    /// directly on the tool-call object, split across SSE deltas) also works
    /// through the Cohere provider.
    #[tokio::test]
    async fn stream_parses_flat_shaped_tool_calls() {
        crate::testing::mock_server::assert_flat_shaped_tool_calls(|base| {
            Box::new(
                CohereProvider::new(
                    "sk-test".to_string(),
                    "command-a-plus-05-2026".to_string(),
                    15,
                )
                .with_api_base(base),
            )
        })
        .await;
    }
}
