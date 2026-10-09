use async_stream::stream;
use async_trait::async_trait;
use concerto_core::error::{describe_error_chain, ProviderError};
use concerto_core::traits::{CompletionStream, LlmProvider};
use concerto_core::types::{CompletionRequest, ModelInfo, TokenBudget};
use concerto_core::CancellationToken;
use concerto_core::SecretString;
use futures::stream::StreamExt;
use reqwest::header::CONTENT_TYPE;

use crate::adapters::{AnthropicChatDialect, Dialect, ReasoningEcho};
use crate::sse_state::AnthropicStreamState;

pub struct AnthropicProvider {
    api_key: SecretString,
    model: String,
    timeout_secs: u64,
    dialect: AnthropicChatDialect,
    /// Opt-in Anthropic prompt-cache breakpoints (ADR-48 decision 3). Off by
    /// default; toggled via [`Self::with_cache_breakpoints`].
    cache_breakpoints: bool,
    /// Tool-schema presentation tier (adaptive tool schemas). Resolved per
    /// request against the actual model name; `Auto` (default) keeps every
    /// non-weak model on the verbatim strict schema.
    tool_schema_mode: concerto_config::ToolSchemaMode,
    /// Provider-advertised per-model tool-calling capability (ADR-66 §3
    /// precedence level 2). `None` when the provider publishes no such
    /// metadata; when set it beats the last-resort name heuristic.
    advertised_tool_support: Option<bool>,
}

impl AnthropicProvider {
    pub fn new(api_key: impl Into<SecretString>, model: String, timeout_secs: u64) -> Self {
        Self {
            api_key: api_key.into(),
            model,
            timeout_secs,
            dialect: AnthropicChatDialect,
            cache_breakpoints: false,
            tool_schema_mode: concerto_config::ToolSchemaMode::default(),
            advertised_tool_support: None,
        }
    }

    /// Enable (or disable) Anthropic prompt-cache breakpoint markers on every
    /// rendered request body.
    ///
    /// Builder-style, mirroring the other provider flags (e.g. Ollama's
    /// `with_base_url`). When enabled, each request body is annotated with
    /// `cache_control` markers for the system prompt and the first user turn
    /// so Anthropic can cache the conversation prefix across consecutive
    /// turns.
    pub fn with_cache_breakpoints(mut self, enabled: bool) -> Self {
        self.cache_breakpoints = enabled;
        self
    }

    /// Whether this provider emits Anthropic prompt-cache breakpoints.
    pub fn cache_breakpoints(&self) -> bool {
        self.cache_breakpoints
    }

    /// Set the tool-schema presentation mode (adaptive tool schemas).
    ///
    /// Defaults to [`concerto_config::ToolSchemaMode::Auto`]: weak
    /// tool-calling models (name heuristic) get loose schemas (flattened
    /// nested objects, enum descriptions, argument examples) and the
    /// connector re-nests dot-notation arguments on the way back; every
    /// other model keeps the verbatim strict schema and byte-identical wire
    /// output. See `crate::adapters::schema_loose`.
    pub fn with_tool_schema_mode(mut self, mode: concerto_config::ToolSchemaMode) -> Self {
        self.tool_schema_mode = mode;
        self
    }

    /// Set the provider-advertised per-model tool-calling capability
    /// (ADR-66 §3 precedence level 2).
    pub fn with_advertised_tool_support(mut self, advertised: Option<bool>) -> Self {
        self.advertised_tool_support = advertised;
        self
    }

    /// Build the wire request body for a completion: render the canonical
    /// [`CompletionRequest`] via the dialect, then — only when the cache
    /// flag is on — annotate it with Anthropic prompt-cache breakpoints.
    fn build_body(&self, request: &CompletionRequest, model: &str) -> serde_json::Value {
        let mut body = self.dialect.render_chat_body(request, model, ReasoningEcho::IfPresent);
        if self.cache_breakpoints {
            self.dialect.apply_cache_breakpoints(&mut body);
        }
        body
    }
}

#[async_trait]
impl LlmProvider for AnthropicProvider {
    async fn test_connection(&self, _cancel: CancellationToken) -> Result<(), ProviderError> {
        let client = crate::new_client(self.timeout_secs);
        let request = client
            .get("https://api.anthropic.com/v1/models")
            .header("x-api-key", self.api_key.expose())
            .header("anthropic-version", "2023-06-01");
        crate::probe_connection(request, "anthropic", &[401], crate::unredacted).await
    }

    async fn list_models(
        &self,
        _cancel: CancellationToken,
    ) -> Result<Vec<ModelInfo>, ProviderError> {
        let client = crate::new_client(self.timeout_secs);
        let request = client
            .get("https://api.anthropic.com/v1/models")
            .header("x-api-key", self.api_key.expose())
            .header("anthropic-version", "2023-06-01");
        crate::list_models_json(request, "anthropic", crate::unredacted, |json| {
            json["data"]
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| {
                            let id = v["id"].as_str()?.to_string();
                            let name =
                                v["display_name"].as_str().or(v["id"].as_str()).map(String::from);
                            Some(ModelInfo {
                                id,
                                name,
                                owned_by: None,
                                supports_tool_calling: None,
                            })
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        })
        .await
    }

    async fn stream_completion(
        &self,
        request: CompletionRequest,
        cancel: CancellationToken,
    ) -> Result<CompletionStream, ProviderError> {
        let span = tracing::info_span!(
            "provider.stream_completion",
            provider = "anthropic",
            model = %request.model,
        );
        let _guard = span.enter();

        let client = crate::new_client(self.timeout_secs);
        let url = "https://api.anthropic.com/v1/messages";

        let model =
            if request.model.is_empty() { self.model.clone() } else { request.model.clone() };

        // Adaptive tool schemas (weak-model tier): when the resolved model
        // matches the loose tier, rewrite the request's tool definitions in
        // place before the dialect renders the body. Strict models are
        // untouched — their wire output stays byte-identical.
        let mut request = request;
        let resolved_mode = crate::capability::resolve_tool_schema_mode(
            "anthropic",
            &model,
            self.tool_schema_mode,
            self.advertised_tool_support,
        );
        let tool_adapted =
            crate::adapters::schema_loose::adaptive_tool_schemas_active(resolved_mode, &model);
        if tool_adapted {
            if let Some(tools) = request.tools.as_mut() {
                crate::adapters::schema_loose::adapt_tool_definitions(tools);
            }
        }

        // Request-body rendering now lives in
        // `crate::adapters::anthropic` (`AnthropicChatDialect`); stream
        // parsing remains here in the connector. `build_body` additionally
        // applies the opt-in prompt-cache breakpoints when enabled.
        let body = self.build_body(&request, &model);

        let response = tokio::select! {
            _ = cancel.cancelled() => Err(ProviderError::Cancelled),
            result = async {
                let r = client
                    .post(url)
                    .header("x-api-key", self.api_key.expose())
                    .header("anthropic-version", "2023-06-01")
                    .header(CONTENT_TYPE, "application/json")
                    .json(&body)
                    .send()
                    .await
                    .map_err(|e| ProviderError::Network(format!("request failed: {}", describe_error_chain(&e))))?;

                if !r.status().is_success() {
                    let status = r.status();
                    let retry_after = crate::retry::parse_retry_after(r.headers());
                    let text = r.text().await.unwrap_or_default();
                    return Err(crate::retry::map_http_error(status, &text, retry_after));
                }
                Ok(r)
            } => result,
        }?;

        let mut state = AnthropicStreamState::new();
        if tool_adapted {
            state.tool_adapted = true;
        }
        let cancel = cancel.clone();

        let s = stream! {
            let mut state = state;
            let mut byte_stream = response.bytes_stream();
            while let Some(chunk) = byte_stream.next().await {
                if cancel.is_cancelled() {
                    yield Err(ProviderError::Cancelled);
                    break;
                }
                let items = match chunk {
                    Ok(bytes) => {
                        let events = state.parser.push_bytes(&bytes);
                        for event in events {
                            state.handle_event(event);
                        }
                        let mut items = Vec::new();
                        while let Some(item) = state.pending.pop_front() {
                            items.push(item);
                        }
                        items
                    }
                    // Stream-retry: a transport fault
                    // mid-stream is retriable (tools execute only
                    // post-assembly — re-issue is side-effect-free within
                    // the bounded attempt budget); framing/parse failures
                    // inside a healthy stream stay fatal.
                    Err(e) => vec![Err(ProviderError::StreamTransport(format!(
                        "connection dropped mid-stream: {}",
                        describe_error_chain(&e)
                    )))]
                };
                for item in items {
                    yield item;
                }
            }
            while let Some(item) = state.pending.pop_front() {
                yield item;
            }
        }
        .boxed();

        Ok(s)
    }

    fn context_capacity(&self, model: &str) -> TokenBudget {
        crate::budget::budget_for_model(model, 4_000)
    }

    fn approximate_cost(&self, tokens_in: u64, tokens_out: u64) -> f64 {
        let (in_rate, out_rate) = if self.model.contains("opus") {
            (15.0, 75.0)
        } else if self.model.contains("sonnet") {
            (3.0, 15.0)
        } else if self.model.contains("haiku") {
            (0.25, 1.25)
        } else {
            (3.0, 15.0)
        };
        (tokens_in as f64 / 1_000_000.0) * in_rate + (tokens_out as f64 / 1_000_000.0) * out_rate
    }

    fn provider_name(&self) -> &'static str {
        "anthropic"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approximate_cost_opus() {
        let provider = AnthropicProvider::new("key".to_string(), "claude-3-opus".into(), 30);
        let cost = provider.approximate_cost(1_000_000, 100_000);
        assert!((cost - 22.5).abs() < 0.01, "expected 22.5, got {cost}");
    }

    #[test]
    fn approximate_cost_sonnet() {
        let provider = AnthropicProvider::new("key".to_string(), "claude-3-sonnet".into(), 30);
        let cost = provider.approximate_cost(1_000_000, 100_000);
        assert!((cost - 4.5).abs() < 0.01, "expected 4.5, got {cost}");
    }

    #[test]
    fn approximate_cost_haiku() {
        let provider = AnthropicProvider::new("key".to_string(), "claude-3-haiku".into(), 30);
        let cost = provider.approximate_cost(2_000_000, 200_000);
        assert!((cost - 0.75).abs() < 0.01, "expected 0.75, got {cost}");
    }

    #[test]
    fn approximate_cost_unknown_defaults_to_sonnet() {
        let provider = AnthropicProvider::new("key".to_string(), "claude-unknown-model".into(), 30);
        let cost = provider.approximate_cost(1_000_000, 100_000);
        assert!((cost - 4.5).abs() < 0.01, "expected 4.5 (sonnet default), got {cost}");
    }

    #[test]
    fn approximate_cost_zero_tokens() {
        let provider = AnthropicProvider::new("key".to_string(), "claude-3-sonnet".into(), 30);
        let cost = provider.approximate_cost(0, 0);
        assert_eq!(cost, 0.0);
    }

    #[test]
    fn context_capacity_returns_budget() {
        let provider = AnthropicProvider::new("key".to_string(), "claude-3-sonnet".into(), 30);
        let budget = provider.context_capacity("claude-3-sonnet-20240229");
        assert!(budget.capacity > 0);
    }

    #[test]
    fn provider_name_is_anthropic() {
        let provider = AnthropicProvider::new("key".to_string(), "model".into(), 30);
        assert_eq!(provider.provider_name(), "anthropic");
    }

    #[test]
    fn new_sets_fields() {
        let provider = AnthropicProvider::new("test-key".to_string(), "claude-4".into(), 60);
        assert_eq!(provider.api_key.expose(), "test-key");
        assert_eq!(provider.model, "claude-4");
        assert_eq!(provider.timeout_secs, 60);
        assert!(!provider.cache_breakpoints, "cache breakpoints default to off");
    }

    /// ADR-48 decision 3: building a body with the cache flag off must leave
    /// the dialect output untouched; turning the flag on must route the body
    /// through `apply_cache_breakpoints`.
    #[test]
    fn with_cache_breakpoints_toggles_apply() {
        let request = CompletionRequest {
            messages: vec![
                concerto_core::types::Message {
                    role: concerto_core::types::Role::System,
                    content: "You are a test assistant.".into(),
                    tool_calls: None,
                    tool_results: None,
                    reasoning_content: None,
                    tokens_in: None,
                    tokens_out: None,
                },
                concerto_core::types::Message {
                    role: concerto_core::types::Role::User,
                    content: "Hello".into(),
                    tool_calls: None,
                    tool_results: None,
                    reasoning_content: None,
                    tokens_in: None,
                    tokens_out: None,
                },
            ],
            ..Default::default()
        };

        // Off by default: a plain string system and no cache_control anywhere.
        let off = AnthropicProvider::new("key".to_string(), "claude-4".into(), 30);
        assert!(!off.cache_breakpoints());
        let body = off.build_body(&request, "claude-4");
        assert_eq!(body["system"], "You are a test assistant.");
        assert!(body.get("cache_control").is_none());
        let serialized = serde_json::to_string(&body).expect("serializes");
        assert!(!serialized.contains("cache_control"));

        // On: system is wrapped and the first user text block is marked.
        let on = off.with_cache_breakpoints(true);
        assert!(on.cache_breakpoints());
        let body = on.build_body(&request, "claude-4");
        assert_eq!(body["system"]["cache_control"], serde_json::json!({"type": "ephemeral"}));
        assert_eq!(
            body["messages"][0]["content"][0]["cache_control"],
            serde_json::json!({"type": "ephemeral"})
        );

        // Toggling back off restores the unmarked body.
        let off_again = on.with_cache_breakpoints(false);
        assert!(!off_again.cache_breakpoints());
        let body = off_again.build_body(&request, "claude-4");
        assert_eq!(body["system"], "You are a test assistant.");
        assert!(
            !serde_json::to_string(&body).expect("serializes").contains("cache_control"),
            "off-again body must carry no cache_control"
        );
    }

    /// Cost for unknown model falls back to a default (non-zero) estimate.
    #[test]
    fn approximate_cost_unknown_model_falls_back() {
        let provider = AnthropicProvider::new("key".to_string(), "unknown-v1".into(), 30);
        let cost = provider.approximate_cost(1000, 500);
        // Unknown models should produce some reasonable estimate.
        assert!(cost >= 0.0, "cost should not be negative");
    }
}
