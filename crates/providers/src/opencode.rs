//! OpenCode Zen provider.
//!
//! OpenCode Zen exposes an OpenAI-compatible `/v1` API, so this provider is a
//! thin wrapper around [`OpenAiProvider`] that points at the Zen base URL.
//! All request/response handling is delegated to the inner provider; only the
//! endpoint and the [`LlmProvider::provider_name`] differ.

use async_trait::async_trait;
use concerto_core::error::ProviderError;
use concerto_core::traits::{CompletionStream, LlmProvider};
use concerto_core::types::{CompletionRequest, ModelInfo, TokenBudget};
use concerto_core::CancellationToken;

use crate::openai::{OpenAiProvider, ReasoningEcho};

/// Default OpenCode Zen API base URL.
const OPENCODE_ZEN_API_BASE: &str = "https://opencode.ai/zen/v1";

/// OpenAI-compatible provider backed by the OpenCode Zen endpoint.
pub struct OpenCodeZenProvider {
    inner: OpenAiProvider,
}

impl OpenCodeZenProvider {
    /// Build a provider targeting the OpenCode Zen endpoint.
    pub fn new(api_key: String, model: String, timeout_secs: u64) -> Self {
        Self {
            inner: OpenAiProvider::new(api_key, model, timeout_secs)
                .with_api_base(OPENCODE_ZEN_API_BASE.to_string())
                .with_reasoning_echo(ReasoningEcho::Always),
        }
    }

    /// Build a provider with an explicit API base URL, overriding the Zen default.
    ///
    /// Useful for self-hosted gateways, proxies, or tests.
    pub fn with_api_base(
        api_key: String,
        model: String,
        timeout_secs: u64,
        api_base: String,
    ) -> Self {
        Self {
            inner: OpenAiProvider::new(api_key, model, timeout_secs)
                .with_api_base(api_base)
                .with_reasoning_echo(ReasoningEcho::Always),
        }
    }
}

#[async_trait]
impl LlmProvider for OpenCodeZenProvider {
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
        self.inner.approximate_cost(tokens_in, tokens_out)
    }

    fn provider_name(&self) -> &'static str {
        "opencode"
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
    fn provider_name_is_opencode() {
        let p = OpenCodeZenProvider::new("key".to_string(), "model".to_string(), 30);
        assert_eq!(p.provider_name(), "opencode");
    }
}
