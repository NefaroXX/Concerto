//! DeepInfra provider.
//!
//! DeepInfra serves an OpenAI-compatible Chat Completions API at the
//! `…/v1/openai` prefix (`/chat/completions` and `/models` are appended by the
//! inner connector). This provider is a thin wrapper around [`OpenAiProvider`]
//! pointed at the DeepInfra endpoint, in the same style as the OpenRouter,
//! NVIDIA NIM, DeepSeek, and Groq wrappers.
//!
//! Like every OpenAI-compatible wrapper it defaults to
//! [`ReasoningEcho::IfPresent`] (echo reasoning content only when the upstream
//! reported any); the per-config reasoning-echo dial is honored via
//! [`DeepInfraProvider::with_reasoning_echo`].

use async_trait::async_trait;
use concerto_core::error::ProviderError;
use concerto_core::traits::{CompletionStream, LlmProvider};
use concerto_core::types::{CompletionRequest, ModelInfo, TokenBudget};
use concerto_core::CancellationToken;
use concerto_core::SecretString;

use crate::openai::{OpenAiProvider, ReasoningEcho};

/// Default DeepInfra API base URL.
///
/// The `/v1/openai` path is the OpenAI-SDK-compatible form; the connector
/// appends `/chat/completions` and `/models` to this base, exactly like the
/// Groq (`/openai/v1`) wrapper. DeepInfra's docs use the same base for the
/// Bearer-auth completions surface.
pub(crate) const DEEPINFRA_API_BASE: &str = "https://api.deepinfra.com/v1/openai";

/// Thin OpenAI-compatible wrapper for DeepInfra.
///
/// Tool-call normalization, streaming, loose-schema adaptation (ADR-66 §4),
/// the flat proxy tool-call fallback, and reasoning capture all come from the
/// underlying [`OpenAiProvider`]; this struct only fixes the endpoint.
pub struct DeepInfraProvider {
    inner: OpenAiProvider,
}

impl DeepInfraProvider {
    /// Build a provider targeting the DeepInfra endpoint.
    pub fn new(api_key: impl Into<SecretString>, model: String, timeout_secs: u64) -> Self {
        Self {
            inner: OpenAiProvider::new(api_key, model, timeout_secs)
                .with_api_base(DEEPINFRA_API_BASE.to_string()),
        }
    }

    openai_wrapper_forwarders!(inner);
}

openai_wrapper_forwarders!(llm DeepInfraProvider, inner,
    name: "deepinfra",
    capacity: forward,
    // DeepInfra representative pricing:
    // `meta-llama/Meta-Llama-3.1-70B-Instruct-Turbo` at $0.28 input /
    // $0.10 output per MTok (the catalog's default entry).
    cost: per_mtok(0.28, 0.10),
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_name_is_deepinfra() {
        let provider = DeepInfraProvider::new(
            "key".to_string(),
            "meta-llama/Meta-Llama-3.1-70B-Instruct-Turbo".to_string(),
            15,
        );
        assert_eq!(provider.provider_name(), "deepinfra");
    }

    #[test]
    fn default_api_base_is_the_deepinfra_endpoint() {
        assert_eq!(DEEPINFRA_API_BASE, "https://api.deepinfra.com/v1/openai");
    }

    #[test]
    fn context_capacity_uses_deepinfra_budget() {
        let provider = DeepInfraProvider::new(
            "key".to_string(),
            "meta-llama/Meta-Llama-3.1-70B-Instruct-Turbo".to_string(),
            15,
        );
        assert_eq!(
            provider.context_capacity("meta-llama/Meta-Llama-3.1-70B-Instruct-Turbo").capacity,
            131_072
        );
    }

    #[test]
    fn approximate_cost_scales_with_tokens() {
        let provider = DeepInfraProvider::new(
            "key".to_string(),
            "meta-llama/Meta-Llama-3.1-70B-Instruct-Turbo".to_string(),
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
    /// through the DeepInfra provider (inherited from the inner
    /// `OpenAiProvider`).
    #[tokio::test]
    async fn stream_parses_function_shaped_tool_calls() {
        crate::testing::mock_server::assert_function_shaped_tool_calls(|base| {
            Box::new(
                DeepInfraProvider::new(
                    "sk-test".to_string(),
                    "meta-llama/Meta-Llama-3.1-70B-Instruct-Turbo".to_string(),
                    15,
                )
                .with_api_base(base),
            )
        })
        .await;
    }

    /// Fix 1 regression: the flat proxy fallback (`name` / `arguments`
    /// directly on the tool-call object, split across SSE deltas) also works
    /// through the DeepInfra provider.
    #[tokio::test]
    async fn stream_parses_flat_shaped_tool_calls() {
        crate::testing::mock_server::assert_flat_shaped_tool_calls(|base| {
            Box::new(
                DeepInfraProvider::new(
                    "sk-test".to_string(),
                    "meta-llama/Meta-Llama-3.1-70B-Instruct-Turbo".to_string(),
                    15,
                )
                .with_api_base(base),
            )
        })
        .await;
    }
}
