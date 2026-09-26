use async_trait::async_trait;
use concerto_core::error::ProviderError;
use concerto_core::traits::{CompletionStream, LlmProvider};
use concerto_core::types::{CompletionRequest, ModelInfo, TokenBudget};
use concerto_core::CancellationToken;
use concerto_core::SecretString;

use crate::openai::{OpenAiProvider, ReasoningEcho, UsageRequest};

const OPENROUTER_API_BASE: &str = "https://openrouter.ai/api/v1";

pub struct OpenRouterProvider {
    inner: OpenAiProvider,
}

impl OpenRouterProvider {
    pub fn new(api_key: impl Into<SecretString>, model: String, timeout_secs: u64) -> Self {
        Self {
            inner: OpenAiProvider::new(api_key, model, timeout_secs)
                .with_api_base(OPENROUTER_API_BASE.to_string())
                // OpenRouter's documented usage switch is a top-level
                // `usage: {"include": true}` (ADR-48 §4), not OpenAI's
                // `stream_options`; opting in here is what makes OpenRouter
                // report usage at all on a streamed request.
                .with_usage_request(UsageRequest::OpenRouter),
        }
    }

    /// Set the reasoning-content echo policy (ADR-46), forwarded to the inner
    /// OpenAI-compatible connector. Defaults to [`ReasoningEcho::IfPresent`].
    pub fn with_reasoning_echo(mut self, echo: ReasoningEcho) -> Self {
        self.inner = self.inner.with_reasoning_echo(echo);
        self
    }

    /// Set the tool-schema presentation mode (adaptive tool schemas),
    /// forwarded to the inner OpenAI-compatible connector. Defaults to
    /// [`concerto_config::ToolSchemaMode::Auto`]. See
    /// `crate::adapters::schema_loose`.
    pub fn with_tool_schema_mode(mut self, mode: concerto_config::ToolSchemaMode) -> Self {
        self.inner = self.inner.with_tool_schema_mode(mode);
        self
    }
}

#[async_trait]
impl LlmProvider for OpenRouterProvider {
    async fn test_connection(&self, _cancel: CancellationToken) -> Result<(), ProviderError> {
        self.inner.test_connection(_cancel.clone()).await
    }

    async fn list_models(
        &self,
        _cancel: CancellationToken,
    ) -> Result<Vec<ModelInfo>, ProviderError> {
        self.inner.list_models(_cancel.clone()).await
    }

    async fn stream_completion(
        &self,
        request: CompletionRequest,
        cancel: CancellationToken,
    ) -> Result<CompletionStream, ProviderError> {
        self.inner.stream_completion(request, cancel).await
    }

    fn context_capacity(&self, model: &str) -> TokenBudget {
        crate::budget::budget_for_model(model, 4_000)
    }

    fn approximate_cost(&self, tokens_in: u64, tokens_out: u64) -> f64 {
        // OpenRouter cost varies by routed model. $0.003/1K is a conservative
        // estimate suitable for budget gating; actual charges are model-dependent.
        ((tokens_in + tokens_out) as f64 / 1_000.0) * 0.003
    }

    fn provider_name(&self) -> &'static str {
        "openrouter"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::openai_compat::map_usage;
    use crate::adapters::{Dialect, OpenAiChatDialect};
    use concerto_core::types::{CompletionUsage, Message, Role};

    fn provider() -> OpenRouterProvider {
        OpenRouterProvider::new("test-key".to_string(), "test-model".into(), 15)
    }

    fn streamed_request() -> CompletionRequest {
        CompletionRequest {
            messages: vec![Message {
                role: Role::User,
                content: "Hello".into(),
                tool_calls: None,
                tool_results: None,
                reasoning_content: None,
                tokens_in: None,
                tokens_out: None,
            }],
            stream: true,
            ..Default::default()
        }
    }

    /// The connector must opt OpenRouter in at construction — otherwise
    /// streamed requests never report usage and the columns stay 0.
    #[test]
    fn connector_requests_openrouter_usage_policy() {
        assert_eq!(provider().inner.usage_request(), UsageRequest::OpenRouter);
    }

    /// The policy is applied to the real dialect output: OpenRouter gets its
    /// documented `usage.include` switch and never OpenAI's `stream_options`.
    #[test]
    fn rendered_body_carries_usage_include_without_stream_options() {
        let request = streamed_request();
        let provider = provider();
        let mut body =
            OpenAiChatDialect.render_chat_body(&request, "test-model", ReasoningEcho::IfPresent);
        provider.inner.usage_request().apply(&mut body, request.stream);

        assert_eq!(body["usage"]["include"], true, "OpenRouter must ask for usage.include");
        assert!(
            body.get("stream_options").is_none(),
            "stream_options is not OpenRouter's contract"
        );
        assert_eq!(body["stream"], true, "the stream flag itself is untouched");
    }

    /// ADR-48 §4: OpenRouter's payload is the family shape plus extras
    /// (`cost`, `native_usage`, ...) which the shared mapper ignores.
    #[test]
    fn map_usage_reads_openrouter_payload() {
        let payload = serde_json::json!({
            "usage": {
                "prompt_tokens": 120,
                "completion_tokens": 34,
                "total_tokens": 154,
                "cost": 0.0021,
                "native_usage": {"prompt_tokens": 120},
            }
        });
        assert_eq!(
            map_usage(&payload),
            Some(CompletionUsage { prompt_tokens: Some(120), completion_tokens: Some(34) })
        );
    }

    /// Fail-soft: an endpoint that reports no usage keeps `None` — the
    /// adapter never errors and never fabricates a `0`.
    #[test]
    fn map_usage_without_usage_reports_none() {
        assert_eq!(map_usage(&serde_json::json!({"choices": []})), None);
        assert_eq!(map_usage(&serde_json::json!({"usage": {}})), None);
    }
}
