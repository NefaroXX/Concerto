//! Novita AI provider.
//!
//! Novita serves an OpenAI-compatible Chat Completions API (`/chat/completions`
//! and `/models` are appended by the inner connector). This provider is a thin
//! wrapper around [`OpenAiProvider`] pointed at the Novita endpoint, in the
//! same style as the OpenRouter, NVIDIA NIM, DeepSeek, and Groq wrappers.
//!
//! Novita is shipped as a *discovery-driven* provider: no static model catalog
//! or default model is hard-coded because the Novita OpenAI-compatible surface
//! does not publish a stable hand-verifiable model ID list, and the base URL
//! varies by endpoint. Users configure the model and optionally a custom
//! `api_base`; model discovery fills the picker.
//!
//! Like every OpenAI-compatible wrapper it defaults to
//! [`ReasoningEcho::IfPresent`] (echo reasoning content only when the upstream
//! reported any); the per-config reasoning-echo dial is honored via
//! [`NovitaProvider::with_reasoning_echo`].

use async_trait::async_trait;
use concerto_core::error::ProviderError;
use concerto_core::traits::{CompletionStream, LlmProvider};
use concerto_core::types::{CompletionRequest, ModelInfo, TokenBudget};
use concerto_core::CancellationToken;

use crate::openai::{OpenAiProvider, ReasoningEcho};

/// Default Novita API base URL.
///
/// Novita documents an OpenAI-compatible surface at
/// `https://api.novita.ai/openai` (`/chat/completions` and `/models` are
/// appended by the inner connector). The Novita catalog is split across
/// per-endpoint bases, so this base is the documented default — users pointing
/// at a dedicated endpoint set a custom `api_base`.
pub(crate) const NOVITA_API_BASE: &str = "https://api.novita.ai/openai";

/// Thin OpenAI-compatible wrapper for Novita AI.
///
/// Tool-call normalization, streaming, loose-schema adaptation (ADR-66 §4),
/// the flat proxy tool-call fallback, and reasoning capture all come from the
/// underlying [`OpenAiProvider`]; this struct only fixes the endpoint.
pub struct NovitaProvider {
    inner: OpenAiProvider,
}

impl NovitaProvider {
    /// Build a provider targeting the Novita endpoint.
    pub fn new(api_key: String, model: String, timeout_secs: u64) -> Self {
        Self {
            inner: OpenAiProvider::new(api_key, model, timeout_secs)
                .with_api_base(NOVITA_API_BASE.to_string()),
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
impl LlmProvider for NovitaProvider {
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
        // Novita representative pricing: a mid-range serverless model at
        // roughly $0.15 input / $0.60 output per MTok. Novita's catalog is
        // broad and pricing is per-model; these figures are deliberately
        // representative, consistent with the "approximate" contract of this
        // method.
        let input_cost = (tokens_in as f64 / 1_000_000.0) * 0.15;
        let output_cost = (tokens_out as f64 / 1_000_000.0) * 0.60;
        input_cost + output_cost
    }

    fn provider_name(&self) -> &'static str {
        "novita"
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
    fn provider_name_is_novita() {
        let provider = NovitaProvider::new("key".to_string(), "custom-model".to_string(), 15);
        assert_eq!(provider.provider_name(), "novita");
    }

    #[test]
    fn default_api_base_is_the_novita_endpoint() {
        assert_eq!(NOVITA_API_BASE, "https://api.novita.ai/openai");
    }

    /// Discovery-driven catalog: unlisted models (including the user-chosen
    /// live model) fall back to the default context budget.
    #[test]
    fn context_capacity_falls_back_for_unlisted_models() {
        let provider = NovitaProvider::new("key".to_string(), "custom-model".to_string(), 15);
        // No static catalog ships for Novita; the budget's default capacity
        // applies until discovery or the user's config pins a known model.
        assert_eq!(provider.context_capacity("custom-model").capacity, 128_000);
    }

    #[test]
    fn approximate_cost_scales_with_tokens() {
        let provider = NovitaProvider::new("key".to_string(), "custom-model".to_string(), 15);
        let small = provider.approximate_cost(1_000_000, 1_000_000);
        assert!(small > 0.0, "cost must be positive");
        assert!(
            provider.approximate_cost(2_000_000, 2_000_000) > small,
            "cost must scale with token volume"
        );
    }

    /// The canonical `function {name, arguments}` tool-call shape parses
    /// through the Novita provider (inherited from the inner `OpenAiProvider`).
    #[tokio::test]
    async fn stream_parses_function_shaped_tool_calls() {
        crate::testing::mock_server::assert_function_shaped_tool_calls(|base| {
            Box::new(
                NovitaProvider::new("sk-test".to_string(), "custom-model".to_string(), 15)
                    .with_api_base(base),
            )
        })
        .await;
    }

    /// Fix 1 regression: the flat proxy fallback (`name` / `arguments`
    /// directly on the tool-call object, split across SSE deltas) also works
    /// through the Novita provider.
    #[tokio::test]
    async fn stream_parses_flat_shaped_tool_calls() {
        crate::testing::mock_server::assert_flat_shaped_tool_calls(|base| {
            Box::new(
                NovitaProvider::new("sk-test".to_string(), "custom-model".to_string(), 15)
                    .with_api_base(base),
            )
        })
        .await;
    }
}
