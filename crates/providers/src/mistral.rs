//! Mistral provider.
//!
//! Mistral serves an OpenAI-compatible Chat Completions API
//! (`/v1/chat/completions`). This provider is a thin wrapper around
//! [`OpenAiProvider`] pointed at the Mistral endpoint, in the same style as
//! the OpenRouter, NVIDIA NIM, and DeepSeek wrappers.
//!
//! Like every OpenAI-compatible wrapper it defaults to
//! [`ReasoningEcho::IfPresent`] (echo reasoning content only when the
//! upstream reported any); the per-config reasoning-echo dial is honored via
//! [`MistralProvider::with_reasoning_echo`].

use async_trait::async_trait;
use concerto_core::error::ProviderError;
use concerto_core::traits::{CompletionStream, LlmProvider};
use concerto_core::types::{CompletionRequest, ModelInfo, TokenBudget};
use concerto_core::CancellationToken;
use concerto_core::SecretString;

use crate::openai::{OpenAiProvider, ReasoningEcho};

/// Default Mistral API base URL.
///
/// The `/v1` prefix is the OpenAI-SDK-compatible form (`/chat/completions`
/// and `/models` are appended by the inner connector, exactly like the
/// DeepSeek `/v1` wrapper).
pub(crate) const MISTRAL_API_BASE: &str = "https://api.mistral.ai/v1";

/// Thin OpenAI-compatible wrapper for Mistral.
///
/// Tool-call normalization, streaming, loose-schema adaptation (ADR-66 §4),
/// the flat proxy tool-call fallback, and reasoning capture all come from the
/// underlying [`OpenAiProvider`]; this struct only fixes the endpoint.
pub struct MistralProvider {
    inner: OpenAiProvider,
}

impl MistralProvider {
    /// Build a provider targeting the Mistral endpoint.
    pub fn new(api_key: impl Into<SecretString>, model: String, timeout_secs: u64) -> Self {
        Self {
            inner: OpenAiProvider::new(api_key, model, timeout_secs)
                .with_api_base(MISTRAL_API_BASE.to_string()),
        }
    }

    openai_wrapper_forwarders!(inner);
}

openai_wrapper_forwarders!(llm MistralProvider, inner,
    name: "mistral",
    capacity: forward,
    // Mistral representative pricing: `mistral-large-latest` at $0.50
    // input / $1.50 output per MTok (the flagship Large tier).
    cost: per_mtok(0.50, 1.50),
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_name_is_mistral() {
        let provider =
            MistralProvider::new("key".to_string(), "mistral-large-latest".to_string(), 15);
        assert_eq!(provider.provider_name(), "mistral");
    }

    #[test]
    fn default_api_base_is_the_mistral_endpoint() {
        assert_eq!(MISTRAL_API_BASE, "https://api.mistral.ai/v1");
    }

    #[test]
    fn context_capacity_uses_mistral_budget() {
        let provider =
            MistralProvider::new("key".to_string(), "mistral-large-latest".to_string(), 15);
        assert_eq!(provider.context_capacity("mistral-large-latest").capacity, 131_072);
    }

    #[test]
    fn approximate_cost_scales_with_tokens() {
        let provider =
            MistralProvider::new("key".to_string(), "mistral-large-latest".to_string(), 15);
        let small = provider.approximate_cost(1_000_000, 1_000_000);
        assert!(small > 0.0, "cost must be positive");
        assert!(
            provider.approximate_cost(2_000_000, 2_000_000) > small,
            "cost must scale with token volume"
        );
    }

    /// The canonical `function {name, arguments}` tool-call shape parses
    /// through the Mistral provider (inherited from the inner
    /// `OpenAiProvider`).
    #[tokio::test]
    async fn stream_parses_function_shaped_tool_calls() {
        crate::testing::mock_server::assert_function_shaped_tool_calls(|base| {
            Box::new(
                MistralProvider::new("sk-test".to_string(), "mistral-large-latest".to_string(), 15)
                    .with_api_base(base),
            )
        })
        .await;
    }

    /// Fix 1 regression: the flat proxy fallback (`name` / `arguments`
    /// directly on the tool-call object, split across SSE deltas) also works
    /// through the Mistral provider.
    #[tokio::test]
    async fn stream_parses_flat_shaped_tool_calls() {
        crate::testing::mock_server::assert_flat_shaped_tool_calls(|base| {
            Box::new(
                MistralProvider::new("sk-test".to_string(), "mistral-large-latest".to_string(), 15)
                    .with_api_base(base),
            )
        })
        .await;
    }
}
