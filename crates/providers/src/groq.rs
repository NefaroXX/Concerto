//! Groq provider.
//!
//! Groq serves an OpenAI-compatible Chat Completions API
//! (`/v1/chat/completions`) on LPU hardware with industry-leading latency.
//! This provider is a thin wrapper around [`OpenAiProvider`] pointed at the
//! Groq endpoint, in the same style as the OpenRouter, NVIDIA NIM, and
//! DeepSeek wrappers.
//!
//! Like every OpenAI-compatible wrapper it defaults to
//! [`ReasoningEcho::IfPresent`] (echo reasoning content only when the
//! upstream reported any); the per-config reasoning-echo dial is honored via
//! [`GroqProvider::with_reasoning_echo`].

use async_trait::async_trait;
use concerto_core::error::ProviderError;
use concerto_core::traits::{CompletionStream, LlmProvider};
use concerto_core::types::{CompletionRequest, ModelInfo, TokenBudget};
use concerto_core::CancellationToken;
use concerto_core::SecretString;

use crate::openai::{OpenAiProvider, ReasoningEcho};

/// Default Groq API base URL.
///
/// The `/v1`-style path is the OpenAI-SDK-compatible form (`/chat/completions`
/// and `/models` are appended by the inner connector, exactly like the
/// DeepSeek `/v1` wrapper).
pub(crate) const GROQ_API_BASE: &str = "https://api.groq.com/openai/v1";

/// Thin OpenAI-compatible wrapper for Groq.
///
/// Tool-call normalization, streaming, loose-schema adaptation (ADR-66 §4),
/// the flat proxy tool-call fallback, and reasoning capture all come from the
/// underlying [`OpenAiProvider`]; this struct only fixes the endpoint.
pub struct GroqProvider {
    inner: OpenAiProvider,
}

impl GroqProvider {
    /// Build a provider targeting the Groq endpoint.
    pub fn new(api_key: impl Into<SecretString>, model: String, timeout_secs: u64) -> Self {
        Self {
            inner: OpenAiProvider::new(api_key, model, timeout_secs)
                .with_api_base(GROQ_API_BASE.to_string()),
        }
    }

    openai_wrapper_forwarders!(inner);
}

openai_wrapper_forwarders!(llm GroqProvider, inner,
    name: "groq",
    capacity: forward,
    // Groq representative pricing: `llama-3.3-70b-versatile` at $0.59
    // input / $0.79 output per MTok (the catalog's mid-tier entry).
    cost: per_mtok(0.59, 0.79),
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_name_is_groq() {
        let provider =
            GroqProvider::new("key".to_string(), "llama-3.3-70b-versatile".to_string(), 15);
        assert_eq!(provider.provider_name(), "groq");
    }

    #[test]
    fn default_api_base_is_the_groq_endpoint() {
        assert_eq!(GROQ_API_BASE, "https://api.groq.com/openai/v1");
    }

    #[test]
    fn context_capacity_uses_groq_budget() {
        let provider =
            GroqProvider::new("key".to_string(), "llama-3.3-70b-versatile".to_string(), 15);
        assert_eq!(provider.context_capacity("llama-3.3-70b-versatile").capacity, 131_072);
    }

    #[test]
    fn approximate_cost_scales_with_tokens() {
        let provider =
            GroqProvider::new("key".to_string(), "llama-3.3-70b-versatile".to_string(), 15);
        let small = provider.approximate_cost(1_000_000, 1_000_000);
        assert!(small > 0.0, "cost must be positive");
        assert!(
            provider.approximate_cost(2_000_000, 2_000_000) > small,
            "cost must scale with token volume"
        );
    }

    /// The canonical `function {name, arguments}` tool-call shape parses
    /// through the Groq provider (inherited from the inner `OpenAiProvider`).
    #[tokio::test]
    async fn stream_parses_function_shaped_tool_calls() {
        crate::testing::mock_server::assert_function_shaped_tool_calls(|base| {
            Box::new(
                GroqProvider::new("sk-test".to_string(), "llama-3.3-70b-versatile".to_string(), 15)
                    .with_api_base(base),
            )
        })
        .await;
    }

    /// Fix 1 regression: the flat proxy fallback (`name` / `arguments`
    /// directly on the tool-call object, split across SSE deltas) also works
    /// through the Groq provider.
    #[tokio::test]
    async fn stream_parses_flat_shaped_tool_calls() {
        crate::testing::mock_server::assert_flat_shaped_tool_calls(|base| {
            Box::new(
                GroqProvider::new("sk-test".to_string(), "llama-3.3-70b-versatile".to_string(), 15)
                    .with_api_base(base),
            )
        })
        .await;
    }
}
