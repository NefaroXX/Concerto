//! xAI provider.
//!
//! xAI serves an OpenAI-compatible Chat Completions API (`/v1/chat/completions`)
//! for the Grok model family. This provider is a thin wrapper around
//! [`OpenAiProvider`] pointed at the xAI endpoint, in the same style as the
//! OpenRouter, NVIDIA NIM, and DeepSeek wrappers.
//!
//! Like every OpenAI-compatible wrapper it defaults to
//! [`ReasoningEcho::IfPresent`] (echo reasoning content only when the
//! upstream reported any); the per-config reasoning-echo dial is honored via
//! [`XaiProvider::with_reasoning_echo`].

use async_trait::async_trait;
use concerto_core::error::ProviderError;
use concerto_core::traits::{CompletionStream, LlmProvider};
use concerto_core::types::{CompletionRequest, ModelInfo, TokenBudget};
use concerto_core::CancellationToken;
use concerto_core::SecretString;

use crate::openai::{OpenAiProvider, ReasoningEcho};

/// Default xAI API base URL.
///
/// The `/v1` prefix is the OpenAI-SDK-compatible form (`/chat/completions`
/// and `/models` are appended by the inner connector, exactly like the
/// DeepSeek `/v1` wrapper).
pub(crate) const XAI_API_BASE: &str = "https://api.x.ai/v1";

/// Thin OpenAI-compatible wrapper for xAI.
///
/// Tool-call normalization, streaming, loose-schema adaptation (ADR-66 §4),
/// the flat proxy tool-call fallback, and reasoning capture all come from the
/// underlying [`OpenAiProvider`]; this struct only fixes the endpoint.
pub struct XaiProvider {
    inner: OpenAiProvider,
}

impl XaiProvider {
    /// Build a provider targeting the xAI endpoint.
    pub fn new(api_key: impl Into<SecretString>, model: String, timeout_secs: u64) -> Self {
        Self {
            inner: OpenAiProvider::new(api_key, model, timeout_secs)
                .with_api_base(XAI_API_BASE.to_string()),
        }
    }

    openai_wrapper_forwarders!(inner);
}

openai_wrapper_forwarders!(llm XaiProvider, inner,
    name: "xai",
    capacity: forward,
    // xAI representative pricing: the `grok-4.6` flagship at $2.00 input /
    // $6.00 output per MTok (frontier-tier pricing).
    cost: per_mtok(2.00, 6.00),
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_name_is_xai() {
        let provider = XaiProvider::new("key".to_string(), "grok-4".to_string(), 15);
        assert_eq!(provider.provider_name(), "xai");
    }

    #[test]
    fn default_api_base_is_the_xai_endpoint() {
        assert_eq!(XAI_API_BASE, "https://api.x.ai/v1");
    }

    #[test]
    fn context_capacity_uses_xai_budget() {
        let provider = XaiProvider::new("key".to_string(), "grok-4".to_string(), 15);
        assert_eq!(provider.context_capacity("grok-4").capacity, 256_000);
    }

    #[test]
    fn approximate_cost_scales_with_tokens() {
        let provider = XaiProvider::new("key".to_string(), "grok-4".to_string(), 15);
        let small = provider.approximate_cost(1_000_000, 1_000_000);
        assert!(small > 0.0, "cost must be positive");
        assert!(
            provider.approximate_cost(2_000_000, 2_000_000) > small,
            "cost must scale with token volume"
        );
    }

    /// The canonical `function {name, arguments}` tool-call shape parses
    /// through the xAI provider (inherited from the inner `OpenAiProvider`).
    #[tokio::test]
    async fn stream_parses_function_shaped_tool_calls() {
        crate::testing::mock_server::assert_function_shaped_tool_calls(|base| {
            Box::new(
                XaiProvider::new("sk-test".to_string(), "grok-4".to_string(), 15)
                    .with_api_base(base),
            )
        })
        .await;
    }

    /// Fix 1 regression: the flat proxy fallback (`name` / `arguments`
    /// directly on the tool-call object, split across SSE deltas) also works
    /// through the xAI provider.
    #[tokio::test]
    async fn stream_parses_flat_shaped_tool_calls() {
        crate::testing::mock_server::assert_flat_shaped_tool_calls(|base| {
            Box::new(
                XaiProvider::new("sk-test".to_string(), "grok-4".to_string(), 15)
                    .with_api_base(base),
            )
        })
        .await;
    }
}
