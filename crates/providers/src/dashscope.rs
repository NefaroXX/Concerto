//! Alibaba DashScope provider (Qwen models).
//!
//! DashScope's OpenAI-compatible mode serves a Chat Completions API at the
//! `/compatible-mode/v1` prefix (`/chat/completions` and `/models` are appended
//! by the inner connector). This provider is a thin wrapper around
//! [`OpenAiProvider`] pointed at that endpoint, in the same style as the
//! OpenRouter, NVIDIA NIM, DeepSeek, and Groq wrappers.
//!
//! Like every OpenAI-compatible wrapper it defaults to
//! [`ReasoningEcho::IfPresent`] (echo reasoning content only when the upstream
//! reported any); the per-config reasoning-echo dial is honored via
//! [`DashScopeProvider::with_reasoning_echo`].

use async_trait::async_trait;
use concerto_core::error::ProviderError;
use concerto_core::traits::{CompletionStream, LlmProvider};
use concerto_core::types::{CompletionRequest, ModelInfo, TokenBudget};
use concerto_core::CancellationToken;

use crate::openai::{OpenAiProvider, ReasoningEcho};

/// Default DashScope (Alibaba Qwen) API base URL.
///
/// The `/compatible-mode/v1` path is the OpenAI-SDK-compatible form
/// (`/chat/completions` and `/models` are appended by the inner connector).
pub(crate) const DASHSCOPE_API_BASE: &str = "https://dashscope.aliyuncs.com/compatible-mode/v1";

/// Thin OpenAI-compatible wrapper for Alibaba DashScope.
///
/// Tool-call normalization, streaming, loose-schema adaptation (ADR-66 §4),
/// the flat proxy tool-call fallback, and reasoning capture all come from the
/// underlying [`OpenAiProvider`]; this struct only fixes the endpoint.
pub struct DashScopeProvider {
    inner: OpenAiProvider,
}

impl DashScopeProvider {
    /// Build a provider targeting the DashScope endpoint.
    pub fn new(api_key: String, model: String, timeout_secs: u64) -> Self {
        Self {
            inner: OpenAiProvider::new(api_key, model, timeout_secs)
                .with_api_base(DASHSCOPE_API_BASE.to_string()),
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
impl LlmProvider for DashScopeProvider {
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
        // Alibaba representative pricing: `qwen-plus` (the default model) at
        // $0.40 input / $1.20 output per MTok.
        let input_cost = (tokens_in as f64 / 1_000_000.0) * 0.40;
        let output_cost = (tokens_out as f64 / 1_000_000.0) * 1.20;
        input_cost + output_cost
    }

    fn provider_name(&self) -> &'static str {
        "dashscope"
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
    fn provider_name_is_dashscope() {
        let provider = DashScopeProvider::new("key".to_string(), "qwen-plus".to_string(), 15);
        assert_eq!(provider.provider_name(), "dashscope");
    }

    #[test]
    fn default_api_base_is_the_dashscope_endpoint() {
        assert_eq!(DASHSCOPE_API_BASE, "https://dashscope.aliyuncs.com/compatible-mode/v1");
    }

    #[test]
    fn context_capacity_uses_dashscope_budget() {
        let provider = DashScopeProvider::new("key".to_string(), "qwen-plus".to_string(), 15);
        assert_eq!(provider.context_capacity("qwen-plus").capacity, 1_000_000);
    }

    #[test]
    fn approximate_cost_scales_with_tokens() {
        let provider = DashScopeProvider::new("key".to_string(), "qwen-plus".to_string(), 15);
        let small = provider.approximate_cost(1_000_000, 1_000_000);
        assert!(small > 0.0, "cost must be positive");
        assert!(
            provider.approximate_cost(2_000_000, 2_000_000) > small,
            "cost must scale with token volume"
        );
    }

    /// The canonical `function {name, arguments}` tool-call shape parses
    /// through the DashScope provider (inherited from the inner
    /// `OpenAiProvider`).
    #[tokio::test]
    async fn stream_parses_function_shaped_tool_calls() {
        crate::testing::mock_server::assert_function_shaped_tool_calls(|base| {
            Box::new(
                DashScopeProvider::new("sk-test".to_string(), "qwen-plus".to_string(), 15)
                    .with_api_base(base),
            )
        })
        .await;
    }

    /// Fix 1 regression: the flat proxy fallback (`name` / `arguments`
    /// directly on the tool-call object, split across SSE deltas) also works
    /// through the DashScope provider.
    #[tokio::test]
    async fn stream_parses_flat_shaped_tool_calls() {
        crate::testing::mock_server::assert_flat_shaped_tool_calls(|base| {
            Box::new(
                DashScopeProvider::new("sk-test".to_string(), "qwen-plus".to_string(), 15)
                    .with_api_base(base),
            )
        })
        .await;
    }
}
