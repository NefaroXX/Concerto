//! Together AI provider.
//!
//! Together serves an OpenAI-compatible Chat Completions API
//! (`/v1/chat/completions`) across hosted open-weight models. This provider
//! is a thin wrapper around [`OpenAiProvider`] pointed at the Together
//! endpoint, in the same style as the OpenRouter, NVIDIA NIM, and DeepSeek
//! wrappers.
//!
//! Like every OpenAI-compatible wrapper it defaults to
//! [`ReasoningEcho::IfPresent`] (echo reasoning content only when the
//! upstream reported any); the per-config reasoning-echo dial is honored via
//! [`TogetherProvider::with_reasoning_echo`].

use async_trait::async_trait;
use concerto_core::error::ProviderError;
use concerto_core::traits::{CompletionStream, LlmProvider};
use concerto_core::types::{CompletionRequest, ModelInfo, TokenBudget};
use concerto_core::CancellationToken;
use concerto_core::SecretString;

use crate::openai::{OpenAiProvider, ReasoningEcho};

/// Default Together API base URL.
///
/// The `/v1` prefix is the OpenAI-SDK-compatible form (`/chat/completions`
/// and `/models` are appended by the inner connector, exactly like the
/// DeepSeek `/v1` wrapper).
pub(crate) const TOGETHER_API_BASE: &str = "https://api.together.xyz/v1";

/// Thin OpenAI-compatible wrapper for Together AI.
///
/// Tool-call normalization, streaming, loose-schema adaptation (ADR-66 §4),
/// the flat proxy tool-call fallback, and reasoning capture all come from the
/// underlying [`OpenAiProvider`]; this struct only fixes the endpoint.
pub struct TogetherProvider {
    inner: OpenAiProvider,
}

impl TogetherProvider {
    /// Build a provider targeting the Together endpoint.
    pub fn new(api_key: impl Into<SecretString>, model: String, timeout_secs: u64) -> Self {
        Self {
            inner: OpenAiProvider::new(api_key, model, timeout_secs)
                .with_api_base(TOGETHER_API_BASE.to_string()),
        }
    }

    openai_wrapper_forwarders!(inner);
}

openai_wrapper_forwarders!(llm TogetherProvider, inner,
    name: "together",
    capacity: forward,
    // Together representative pricing: Llama 3.3 70B Instruct Turbo at
    // $1.04 input / $1.04 output per MTok (symmetrical per-token pricing).
    cost: per_mtok(1.04, 1.04),
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_name_is_together() {
        let provider = TogetherProvider::new(
            "key".to_string(),
            "meta-llama/Llama-3.3-70B-Instruct-Turbo".to_string(),
            15,
        );
        assert_eq!(provider.provider_name(), "together");
    }

    #[test]
    fn default_api_base_is_the_together_endpoint() {
        assert_eq!(TOGETHER_API_BASE, "https://api.together.xyz/v1");
    }

    #[test]
    fn context_capacity_uses_together_budget() {
        let provider = TogetherProvider::new(
            "key".to_string(),
            "meta-llama/Llama-3.3-70B-Instruct-Turbo".to_string(),
            15,
        );
        assert_eq!(
            provider.context_capacity("meta-llama/Llama-3.3-70B-Instruct-Turbo").capacity,
            131_072
        );
    }

    #[test]
    fn approximate_cost_scales_with_tokens() {
        let provider = TogetherProvider::new(
            "key".to_string(),
            "meta-llama/Llama-3.3-70B-Instruct-Turbo".to_string(),
            15,
        );
        let small = provider.approximate_cost(1_000_000, 1_000_000);
        assert!(small > 0.0, "cost must be positive");
        assert!(
            provider.approximate_cost(2_000_000, 2_000_000) > small,
            "cost must scale with token volume"
        );
    }

    /// The canonical `function {name, arguments}` tool-call shape parses
    /// through the Together provider (inherited from the inner
    /// `OpenAiProvider`).
    #[tokio::test]
    async fn stream_parses_function_shaped_tool_calls() {
        crate::testing::mock_server::assert_function_shaped_tool_calls(|base| {
            Box::new(
                TogetherProvider::new(
                    "sk-test".to_string(),
                    "meta-llama/Llama-3.3-70B-Instruct-Turbo".to_string(),
                    15,
                )
                .with_api_base(base),
            )
        })
        .await;
    }

    /// Fix 1 regression: the flat proxy fallback (`name` / `arguments`
    /// directly on the tool-call object, split across SSE deltas) also works
    /// through the Together provider.
    #[tokio::test]
    async fn stream_parses_flat_shaped_tool_calls() {
        crate::testing::mock_server::assert_flat_shaped_tool_calls(|base| {
            Box::new(
                TogetherProvider::new(
                    "sk-test".to_string(),
                    "meta-llama/Llama-3.3-70B-Instruct-Turbo".to_string(),
                    15,
                )
                .with_api_base(base),
            )
        })
        .await;
    }
}
