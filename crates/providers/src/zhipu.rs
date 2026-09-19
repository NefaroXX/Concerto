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
    pub fn new(api_key: String, model: String, timeout_secs: u64) -> Self {
        Self {
            inner: OpenAiProvider::new(api_key, model, timeout_secs)
                .with_api_base(ZHIPU_API_BASE.to_string()),
        }
    }

    /// Override the API base URL (self-hosted gateways, proxies, or tests).
    pub fn with_api_base(mut self, api_base: String) -> Self {
        self.inner = self.inner.with_api_base(api_base);
        self
    }

    /// Set the reasoning-content echo policy (ADR-46), forwarded to the inner
    /// OpenAI-compatible connector. Defaults to [`ReasoningEcho::IfPresent`].
    pub fn with_reasoning_echo(mut self, echo: ReasoningEcho) -> Self {
        self.inner = self.inner.with_reasoning_echo(echo);
        self
    }

    /// Set the tool-schema presentation mode (adaptive tool schemas),
    /// forwarded to the inner OpenAI-compatible connector. Defaults to
    /// [`concerto_config::ToolSchemaMode::Auto`].
    pub fn with_tool_schema_mode(mut self, mode: concerto_config::ToolSchemaMode) -> Self {
        self.inner = self.inner.with_tool_schema_mode(mode);
        self
    }
}

#[async_trait]
impl LlmProvider for ZhipuProvider {
    async fn stream_completion(
        &self,
        request: CompletionRequest,
        cancel: CancellationToken,
    ) -> Result<CompletionStream, ProviderError> {
        self.inner.stream_completion(request, cancel).await
    }

    fn context_capacity(&self, model: &str) -> TokenBudget {
        self.inner.context_capacity(model)
    }

    fn approximate_cost(&self, tokens_in: u64, tokens_out: u64) -> f64 {
        // Zhipu representative pricing: `glm-4.7` at $0.60 input / $2.20
        // output per MTok (the catalog's default entry).
        let input_cost = (tokens_in as f64 / 1_000_000.0) * 0.60;
        let output_cost = (tokens_out as f64 / 1_000_000.0) * 2.20;
        input_cost + output_cost
    }

    fn provider_name(&self) -> &'static str {
        "zhipu"
    }

    async fn test_connection(&self, _cancel: CancellationToken) -> Result<(), ProviderError> {
        self.inner.test_connection(_cancel.clone()).await
    }

    async fn list_models(
        &self,
        _cancel: CancellationToken,
    ) -> Result<Vec<ModelInfo>, ProviderError> {
        self.inner.list_models(_cancel.clone()).await
    }
}

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
