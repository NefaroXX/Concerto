//! SambaNova provider.
//!
//! SambaNova serves an OpenAI-compatible Chat Completions API (`/v1`-style;
//! `/chat/completions` and `/models` are appended by the inner connector).
//! This provider is a thin wrapper around [`OpenAiProvider`] pointed at the
//! SambaNova Cloud endpoint, in the same style as the OpenRouter, NVIDIA NIM,
//! DeepSeek, and Groq wrappers.
//!
//! Like every OpenAI-compatible wrapper it defaults to
//! [`ReasoningEcho::IfPresent`] (echo reasoning content only when the upstream
//! reported any); the per-config reasoning-echo dial is honored via
//! [`SambaNovaProvider::with_reasoning_echo`].

use async_trait::async_trait;
use concerto_core::error::ProviderError;
use concerto_core::traits::{CompletionStream, LlmProvider};
use concerto_core::types::{CompletionRequest, ModelInfo, TokenBudget};
use concerto_core::CancellationToken;
use concerto_core::SecretString;

use crate::openai::{OpenAiProvider, ReasoningEcho};

/// Default SambaNova API base URL.
///
/// The `/v1`-style path is the OpenAI-SDK-compatible form (`/chat/completions`
/// and `/models` are appended by the inner connector, exactly like the
/// DeepSeek `/v1` wrapper).
pub(crate) const SAMBANOVA_API_BASE: &str = "https://api.sambanova.ai/v1";

/// Thin OpenAI-compatible wrapper for SambaNova.
///
/// Tool-call normalization, streaming, loose-schema adaptation (ADR-66 §4),
/// the flat proxy tool-call fallback, and reasoning capture all come from the
/// underlying [`OpenAiProvider`]; this struct only fixes the endpoint.
pub struct SambaNovaProvider {
    inner: OpenAiProvider,
}

impl SambaNovaProvider {
    /// Build a provider targeting the SambaNova endpoint.
    pub fn new(api_key: impl Into<SecretString>, model: String, timeout_secs: u64) -> Self {
        Self {
            inner: OpenAiProvider::new(api_key, model, timeout_secs)
                .with_api_base(SAMBANOVA_API_BASE.to_string()),
        }
    }

    openai_wrapper_forwarders!(inner);
}

openai_wrapper_forwarders!(llm SambaNovaProvider, inner,
    name: "sambanova",
    capacity: forward,
    // SambaNova representative pricing:
    // `Meta-Llama-3.3-70B-Instruct` at $0.60 input / $1.20 output per
    // MTok (the catalog's default entry).
    cost: per_mtok(0.60, 1.20),
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_name_is_sambanova() {
        let provider = SambaNovaProvider::new(
            "key".to_string(),
            "Meta-Llama-3.3-70B-Instruct".to_string(),
            15,
        );
        assert_eq!(provider.provider_name(), "sambanova");
    }

    #[test]
    fn default_api_base_is_the_sambanova_endpoint() {
        assert_eq!(SAMBANOVA_API_BASE, "https://api.sambanova.ai/v1");
    }

    #[test]
    fn context_capacity_uses_sambanova_budget() {
        let provider = SambaNovaProvider::new(
            "key".to_string(),
            "Meta-Llama-3.3-70B-Instruct".to_string(),
            15,
        );
        assert_eq!(provider.context_capacity("Meta-Llama-3.3-70B-Instruct").capacity, 131_072);
    }

    #[test]
    fn approximate_cost_scales_with_tokens() {
        let provider = SambaNovaProvider::new(
            "key".to_string(),
            "Meta-Llama-3.3-70B-Instruct".to_string(),
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
    /// through the SambaNova provider (inherited from the inner
    /// `OpenAiProvider`).
    #[tokio::test]
    async fn stream_parses_function_shaped_tool_calls() {
        crate::testing::mock_server::assert_function_shaped_tool_calls(|base| {
            Box::new(
                SambaNovaProvider::new(
                    "sk-test".to_string(),
                    "Meta-Llama-3.3-70B-Instruct".to_string(),
                    15,
                )
                .with_api_base(base),
            )
        })
        .await;
    }

    /// Fix 1 regression: the flat proxy fallback (`name` / `arguments`
    /// directly on the tool-call object, split across SSE deltas) also works
    /// through the SambaNova provider.
    #[tokio::test]
    async fn stream_parses_flat_shaped_tool_calls() {
        crate::testing::mock_server::assert_flat_shaped_tool_calls(|base| {
            Box::new(
                SambaNovaProvider::new(
                    "sk-test".to_string(),
                    "Meta-Llama-3.3-70B-Instruct".to_string(),
                    15,
                )
                .with_api_base(base),
            )
        })
        .await;
    }
}
