//! Zhipu AI provider (GLM models).
//!
//! Zhipu serves an OpenAI-compatible Chat Completions API at
//! `https://open.bigmodel.cn/api/paas/v4/chat/completions` (the `/v4` path; the
//! connector appends `/chat/completions` and `/models`). This provider is a
//! thin wrapper around [`OpenAiProvider`] pointed at the Zhipu endpoint, in the
//! same style as the OpenRouter, NVIDIA NIM, DeepSeek, and Groq wrappers.
//!
//! Like every OpenAI-compatible wrapper it defaults to
//! [`ReasoningEcho::IfPresent`] (echo reasoning content only when the upstream
//! reported any); the per-config reasoning-echo dial is honored via
//! [`ZhipuProvider::with_reasoning_echo`].

use async_trait::async_trait;
use concerto_core::error::ProviderError;
use concerto_core::traits::{CompletionStream, LlmProvider};
use concerto_core::types::{CompletionRequest, ModelInfo, TokenBudget};
use concerto_core::CancellationToken;
use concerto_core::SecretString;

use crate::openai::{OpenAiProvider, ReasoningEcho};

/// Default Zhipu AI API base URL.
///
/// The `/v4` path is the OpenAI-SDK-compatible form for Zhipu's open platform
/// (`/chat/completions` and `/models` are appended by the inner connector).
pub(crate) const ZHIPU_API_BASE: &str = "https://open.bigmodel.cn/api/paas/v4";

/// Thin OpenAI-compatible wrapper for Zhipu AI.
///
/// Tool-call normalization, streaming, loose-schema adaptation (ADR-66 §4),
/// the flat proxy tool-call fallback, and reasoning capture all come from the
/// underlying [`OpenAiProvider`]; this struct only fixes the endpoint.
pub struct ZhipuProvider {
    inner: OpenAiProvider,
}

impl ZhipuProvider {
    /// Build a provider targeting the Zhipu endpoint.
    pub fn new(api_key: impl Into<SecretString>, model: String, timeout_secs: u64) -> Self {
        Self {
            inner: OpenAiProvider::new(api_key, model, timeout_secs)
                .with_api_base(ZHIPU_API_BASE.to_string()),
        }
    }

    openai_wrapper_forwarders!(inner);
}

openai_wrapper_forwarders!(llm ZhipuProvider, inner,
    name: "zhipu",
    capacity: forward,
    // Zhipu representative pricing: `glm-4.7` at $0.60 input / $2.20
    // output per MTok (the catalog's default entry).
    cost: per_mtok(0.60, 2.20),
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_name_is_zhipu() {
        let provider = ZhipuProvider::new("key".to_string(), "glm-4.7".to_string(), 15);
        assert_eq!(provider.provider_name(), "zhipu");
    }

    #[test]
    fn default_api_base_is_the_zhipu_endpoint() {
        assert_eq!(ZHIPU_API_BASE, "https://open.bigmodel.cn/api/paas/v4");
    }

    #[test]
    fn context_capacity_uses_zhipu_budget() {
        let provider = ZhipuProvider::new("key".to_string(), "glm-4.7".to_string(), 15);
        assert_eq!(provider.context_capacity("glm-4.7").capacity, 204_800);
    }

    #[test]
    fn approximate_cost_scales_with_tokens() {
        let provider = ZhipuProvider::new("key".to_string(), "glm-4.7".to_string(), 15);
        let small = provider.approximate_cost(1_000_000, 1_000_000);
        assert!(small > 0.0, "cost must be positive");
        assert!(
            provider.approximate_cost(2_000_000, 2_000_000) > small,
            "cost must scale with token volume"
        );
    }

    /// The canonical `function {name, arguments}` tool-call shape parses
    /// through the Zhipu provider (inherited from the inner `OpenAiProvider`).
    #[tokio::test]
    async fn stream_parses_function_shaped_tool_calls() {
        crate::testing::mock_server::assert_function_shaped_tool_calls(|base| {
            Box::new(
                ZhipuProvider::new("sk-test".to_string(), "glm-4.7".to_string(), 15)
                    .with_api_base(base),
            )
        })
        .await;
    }

    /// Fix 1 regression: the flat proxy fallback (`name` / `arguments`
    /// directly on the tool-call object, split across SSE deltas) also works
    /// through the Zhipu provider.
    #[tokio::test]
    async fn stream_parses_flat_shaped_tool_calls() {
        crate::testing::mock_server::assert_flat_shaped_tool_calls(|base| {
            Box::new(
                ZhipuProvider::new("sk-test".to_string(), "glm-4.7".to_string(), 15)
                    .with_api_base(base),
            )
        })
        .await;
    }
}
