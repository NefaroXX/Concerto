use async_stream::stream;
use async_trait::async_trait;
use concerto_core::error::{describe_error_chain, ProviderError};
use concerto_core::traits::{CompletionStream, LlmProvider};
use concerto_core::types::{CompletionChunk, CompletionRequest, ModelInfo, TokenBudget, ToolCall};
use concerto_core::CancellationToken;
use futures::stream::StreamExt;
use reqwest::header::CONTENT_TYPE;
use std::collections::{HashMap, VecDeque};

use crate::adapters::{AnthropicChatDialect, Dialect, ReasoningEcho};
use crate::sse::BufferedSseParser;

pub struct AnthropicProvider {
    api_key: String,
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
}

impl AnthropicProvider {
    pub fn new(api_key: String, model: String, timeout_secs: u64) -> Self {
        Self {
            api_key,
            model,
            timeout_secs,
            dialect: AnthropicChatDialect,
            cache_breakpoints: false,
            tool_schema_mode: concerto_config::ToolSchemaMode::default(),
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

#[derive(Default)]
struct AnthropicParseState {
    text_acc: HashMap<usize, String>,
    tool_acc: HashMap<usize, (String, String, String)>,
}

struct AnthropicStreamState {
    parser: BufferedSseParser,
    parse: AnthropicParseState,
    pending: VecDeque<Result<CompletionChunk, ProviderError>>,
    /// Whether the request that produced this stream was rendered with
    /// loose (weak-model) tool schemas. When set, emitted tool-call
    /// arguments are re-nested from dot-notation back into the tools'
    /// original nested shape (see `crate::adapters::schema_loose`).
    tool_adapted: bool,
}

impl AnthropicStreamState {
    fn new() -> Self {
        Self {
            parser: BufferedSseParser::new(),
            parse: AnthropicParseState::default(),
            pending: VecDeque::new(),
            tool_adapted: false,
        }
    }

    fn handle_event(&mut self, event: crate::sse::SseEvent) {
        if event.keepalive {
            // Liveness signal (SSE comment line): emit an empty chunk so the
            // stream stays active and the orchestrator idle timeout does not
            // fire during long keep-alive-only periods.
            self.pending.push_back(Ok(CompletionChunk {
                reasoning: None,
                delta: String::new(),
                tool_call: None,
                is_final: false,
                usage: None,
            }));
            return;
        }
        let data_str = match event.data {
            Some(d) => d,
            None => return,
        };

        let data: serde_json::Value = match serde_json::from_str(&data_str) {
            Ok(v) => v,
            Err(_) => return,
        };

        let event_type = event.event.as_deref().unwrap_or("");

        match event_type {
            "content_block_start" => {
                let index = data["index"].as_u64().unwrap_or(0) as usize;
                let ctype = data["content_block"]["type"].as_str().unwrap_or("");
                if ctype == "text" {
                    self.parse.text_acc.insert(index, String::new());
                } else if ctype == "tool_use" {
                    let id = data["content_block"]["id"].as_str().unwrap_or("").to_string();
                    let name = data["content_block"]["name"].as_str().unwrap_or("").to_string();
                    self.parse.tool_acc.insert(index, (id, name, String::new()));
                }
            }
            "content_block_delta" => {
                let index = data["index"].as_u64().unwrap_or(0) as usize;
                let delta = &data["delta"];
                if let Some(text) = delta.get("text").and_then(|v| v.as_str()) {
                    if let Some(acc) = self.parse.text_acc.get_mut(&index) {
                        acc.push_str(text);
                    }
                }
                if let Some(partial) = delta.get("partial_json").and_then(|v| v.as_str()) {
                    if let Some((_id, _name, args)) = self.parse.tool_acc.get_mut(&index) {
                        args.push_str(partial);
                    }
                }
            }
            "content_block_stop" => {
                let index = data["index"].as_u64().unwrap_or(0) as usize;
                if let Some(text) = self.parse.text_acc.remove(&index) {
                    self.pending.push_back(Ok(CompletionChunk {
                        reasoning: None,
                        delta: text,
                        tool_call: None,
                        is_final: false,
                        usage: None,
                    }));
                } else if let Some((id, name, args_str)) = self.parse.tool_acc.remove(&index) {
                    let mut args_json = if args_str.trim().is_empty() {
                        serde_json::Value::Null
                    } else {
                        serde_json::from_str(&args_str).unwrap_or(serde_json::Value::Null)
                    };
                    // Adaptive tool schemas: re-nest dot-notation arguments
                    // from loose-schema streams before the executor or the
                    // tool-call guard validates against the nested schema.
                    if self.tool_adapted {
                        crate::adapters::schema_loose::unflatten_tool_arguments(&mut args_json);
                    }
                    self.pending.push_back(Ok(CompletionChunk {
                        reasoning: None,
                        delta: String::new(),
                        tool_call: Some(ToolCall { id, name, arguments: args_json }),
                        is_final: false,
                        usage: None,
                    }));
                }
            }
            "message_stop" => {
                self.pending.push_back(Ok(CompletionChunk {
                    reasoning: None,
                    delta: String::new(),
                    tool_call: None,
                    is_final: true,
                    usage: None,
                }));
            }
            _ => {}
        }
    }
}

#[async_trait]
impl LlmProvider for AnthropicProvider {
    async fn test_connection(&self, _cancel: CancellationToken) -> Result<(), ProviderError> {
        let client = crate::new_client(self.timeout_secs);
        let resp = client
            .get("https://api.anthropic.com/v1/models")
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .send()
            .await
            .map_err(|e| {
                ProviderError::Other(format!(
                    "anthropic connection failed: {}",
                    describe_error_chain(&e)
                ))
            })?;
        if resp.status().is_success() {
            Ok(())
        } else if resp.status().as_u16() == 401 {
            Err(ProviderError::AuthFailure)
        } else {
            Err(ProviderError::Other(format!("anthropic returned {}", resp.status())))
        }
    }

    async fn list_models(
        &self,
        _cancel: CancellationToken,
    ) -> Result<Vec<ModelInfo>, ProviderError> {
        let client = crate::new_client(self.timeout_secs);
        let resp = client
            .get("https://api.anthropic.com/v1/models")
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .send()
            .await
            .map_err(|e| {
                ProviderError::Other(format!(
                    "anthropic list_models failed: {}",
                    describe_error_chain(&e)
                ))
            })?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(ProviderError::Other(format!(
                "anthropic list_models returned {status}: {text}"
            )));
        }

        let json: serde_json::Value = resp.json().await.map_err(|e| {
            ProviderError::Other(format!(
                "anthropic list_models parse failed: {}",
                describe_error_chain(&e)
            ))
        })?;

        let models = json["data"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| {
                        let id = v["id"].as_str()?.to_string();
                        let name =
                            v["display_name"].as_str().or(v["id"].as_str()).map(String::from);
                        Some(ModelInfo { id, name, owned_by: None })
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        Ok(models)
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
        let tool_adapted = crate::adapters::schema_loose::adaptive_tool_schemas_active(
            self.tool_schema_mode,
            &model,
        );
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
                    .header("x-api-key", &self.api_key)
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
                    Err(e) => vec![Err(ProviderError::Other(format!("stream error: {}", describe_error_chain(&e))))],
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
        let provider = AnthropicProvider::new("key".into(), "claude-3-opus".into(), 30);
        let cost = provider.approximate_cost(1_000_000, 100_000);
        assert!((cost - 22.5).abs() < 0.01, "expected 22.5, got {cost}");
    }

    #[test]
    fn approximate_cost_sonnet() {
        let provider = AnthropicProvider::new("key".into(), "claude-3-sonnet".into(), 30);
        let cost = provider.approximate_cost(1_000_000, 100_000);
        assert!((cost - 4.5).abs() < 0.01, "expected 4.5, got {cost}");
    }

    #[test]
    fn approximate_cost_haiku() {
        let provider = AnthropicProvider::new("key".into(), "claude-3-haiku".into(), 30);
        let cost = provider.approximate_cost(2_000_000, 200_000);
        assert!((cost - 0.75).abs() < 0.01, "expected 0.75, got {cost}");
    }

    #[test]
    fn approximate_cost_unknown_defaults_to_sonnet() {
        let provider = AnthropicProvider::new("key".into(), "claude-unknown-model".into(), 30);
        let cost = provider.approximate_cost(1_000_000, 100_000);
        assert!((cost - 4.5).abs() < 0.01, "expected 4.5 (sonnet default), got {cost}");
    }

    #[test]
    fn approximate_cost_zero_tokens() {
        let provider = AnthropicProvider::new("key".into(), "claude-3-sonnet".into(), 30);
        let cost = provider.approximate_cost(0, 0);
        assert_eq!(cost, 0.0);
    }

    #[test]
    fn context_capacity_returns_budget() {
        let provider = AnthropicProvider::new("key".into(), "claude-3-sonnet".into(), 30);
        let budget = provider.context_capacity("claude-3-sonnet-20240229");
        assert!(budget.capacity > 0);
    }

    #[test]
    fn provider_name_is_anthropic() {
        let provider = AnthropicProvider::new("key".into(), "model".into(), 30);
        assert_eq!(provider.provider_name(), "anthropic");
    }

    #[test]
    fn new_sets_fields() {
        let provider = AnthropicProvider::new("test-key".into(), "claude-4".into(), 60);
        assert_eq!(provider.api_key, "test-key");
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
        let off = AnthropicProvider::new("key".into(), "claude-4".into(), 30);
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

    #[test]
    fn parse_state_default_is_empty() {
        let state = AnthropicParseState::default();
        assert!(state.text_acc.is_empty());
        assert!(state.tool_acc.is_empty());
    }

    /// Cost for unknown model falls back to a default (non-zero) estimate.
    #[test]
    fn approximate_cost_unknown_model_falls_back() {
        let provider = AnthropicProvider::new("key".into(), "unknown-v1".into(), 30);
        let cost = provider.approximate_cost(1000, 500);
        // Unknown models should produce some reasonable estimate.
        assert!(cost >= 0.0, "cost should not be negative");
    }
}
