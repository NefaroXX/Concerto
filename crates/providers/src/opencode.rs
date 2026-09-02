//! OpenCode Zen provider.
//!
//! OpenCode Zen serves multiple model families through a single gateway:
//!
//! - **OpenAI-compatible models** (e.g. `big-pickle`, DeepSeek): routed via
//!   [`OpenAiProvider`] to `POST {base}/chat/completions`.
//! - **Anthropic models** (`claude-*`): routed via the Anthropic Messages API
//!   to `POST {base}/messages` with `x-api-key` + `anthropic-version` headers.
//! - **Muse models** (e.g. `muse-spark-1.2-contributor-free`): routed via the
//!   OpenAI Responses API to `POST {base}/responses`. Muse moved to the
//!   Responses API upstream and now 500s on both `/chat/completions` and
//!   `/messages`.
//!
//! The dialect is chosen automatically based on the model name: models
//! containing `"muse"` (case-insensitive) use the Responses API, models
//! containing `"claude"` use the Anthropic dialect, and everything else uses
//! OpenAI-compatible chat completions.

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
use crate::openai::OpenAiProvider;
use crate::sse::BufferedSseParser;

/// Default OpenCode Zen API base URL.
const OPENCODE_ZEN_API_BASE: &str = "https://opencode.ai/zen/v1";

/// Detect whether a model name requires the Anthropic Messages API dialect.
///
/// Claude models served by the Zen gateway expect the Anthropic wire format
/// (`POST /messages`, `x-api-key` header, Anthropic SSE events). All other
/// non-Muse models use the OpenAI-compatible dialect.
fn needs_anthropic_dialect(model: &str) -> bool {
    model.to_lowercase().contains("claude")
}

/// Detect whether a model name requires the OpenAI Responses API dialect.
///
/// Muse models served by the Zen gateway only work via `POST /responses`
/// (Responses SSE events); they 500 on both `/chat/completions` and
/// `/messages`.
fn needs_responses_api(model: &str) -> bool {
    model.to_lowercase().contains("muse")
}

/// OpenCode Zen provider that automatically selects the correct wire dialect
/// per model family.
pub struct OpenCodeZenProvider {
    api_key: String,
    model: String,
    timeout_secs: u64,
    api_base: String,
    /// Tool-schema presentation tier (adaptive tool schemas) for the
    /// provider's own Anthropic-dialect path. The OpenAI-compatible path
    /// delegates to `openai_inner`, which carries its own copy. `Auto`
    /// (default) keeps every non-weak model on the verbatim strict schema.
    tool_schema_mode: concerto_config::ToolSchemaMode,
    /// Pre-built inner OpenAI provider for OpenAI-compatible models.
    openai_inner: OpenAiProvider,
}

impl OpenCodeZenProvider {
    /// Build a provider targeting the OpenCode Zen endpoint.
    pub fn new(api_key: String, model: String, timeout_secs: u64) -> Self {
        Self::with_api_base(api_key, model, timeout_secs, OPENCODE_ZEN_API_BASE.to_string())
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
        let openai_inner = OpenAiProvider::new(api_key.clone(), model.clone(), timeout_secs)
            .with_api_base(api_base.clone())
            .with_reasoning_echo(ReasoningEcho::Always);
        Self {
            api_key,
            model,
            timeout_secs,
            api_base,
            tool_schema_mode: concerto_config::ToolSchemaMode::default(),
            openai_inner,
        }
    }

    /// Set the tool-schema presentation mode (adaptive tool schemas).
    ///
    /// Applies to both wire paths: the Anthropic Messages path handled here
    /// and the OpenAI-compatible path delegated to the inner provider. The
    /// Responses API path (Muse models) carries no tool declarations at all,
    /// so there is nothing to adapt there.
    ///
    /// Defaults to [`concerto_config::ToolSchemaMode::Auto`]: weak
    /// tool-calling models (name heuristic) get loose schemas and the
    /// connector re-nests dot-notation arguments on the way back; every
    /// other model keeps the verbatim strict schema and byte-identical wire
    /// output. See `crate::adapters::schema_loose`.
    pub fn with_tool_schema_mode(mut self, mode: concerto_config::ToolSchemaMode) -> Self {
        self.tool_schema_mode = mode;
        self.openai_inner = self.openai_inner.with_tool_schema_mode(mode);
        self
    }

    /// Resolve the effective model name for a request.
    fn resolve_model(&self, request: &CompletionRequest) -> String {
        if request.model.is_empty() {
            self.model.clone()
        } else {
            request.model.clone()
        }
    }

    /// Build the Anthropic Messages API request body for the Zen endpoint.
    fn build_anthropic_body(&self, request: &CompletionRequest, model: &str) -> serde_json::Value {
        let dialect = AnthropicChatDialect;
        dialect.render_chat_body(request, model, ReasoningEcho::IfPresent)
    }

    /// Build the Responses API request body for Muse models.
    ///
    /// Uses the easy input format: an array of `{role, content}` items, with
    /// system messages carried as instructions.
    fn build_responses_body(request: &CompletionRequest, model: &str) -> serde_json::Value {
        let mut instructions = String::new();
        let mut input: Vec<serde_json::Value> = Vec::new();
        for msg in &request.messages {
            match msg.role {
                concerto_core::types::Role::System => {
                    if !instructions.is_empty() {
                        instructions.push_str("\n\n");
                    }
                    instructions.push_str(&msg.content);
                }
                concerto_core::types::Role::User => {
                    input.push(serde_json::json!({"role": "user", "content": msg.content}));
                }
                concerto_core::types::Role::Assistant => {
                    input.push(serde_json::json!({"role": "assistant", "content": msg.content}));
                }
                concerto_core::types::Role::Tool => {
                    input.push(serde_json::json!({
                        "role": "user",
                        "content": format!("[tool result]\n{}", msg.content),
                    }));
                }
                // Future `#[non_exhaustive]` variants: drop rather than fail.
                _ => {}
            }
        }
        let mut body = serde_json::json!({
            "model": model,
            "input": input,
            "stream": true,
        });
        if !instructions.is_empty() {
            body["instructions"] = serde_json::Value::String(instructions);
        }
        if let Some(max_tokens) = request.max_tokens {
            body["max_output_tokens"] = serde_json::json!(max_tokens);
        }
        body
    }

    /// Stream a completion using the OpenAI Responses API dialect.
    ///
    /// This path handles Muse models, which the Zen gateway serves only via
    /// `POST /responses` with Responses SSE events (`response.output_text.delta`,
    /// `response.completed`, etc.).
    async fn stream_completion_responses(
        &self,
        request: CompletionRequest,
        cancel: CancellationToken,
    ) -> Result<CompletionStream, ProviderError> {
        let model = self.resolve_model(&request);
        let span = tracing::info_span!(
            "provider.stream_completion",
            provider = "opencode",
            dialect = "responses",
            model = %model,
        );
        let _guard = span.enter();

        let client = crate::new_client(self.timeout_secs);
        let url = format!("{}/responses", self.api_base);

        let body = Self::build_responses_body(&request, &model);

        let response = tokio::select! {
            _ = cancel.cancelled() => Err(ProviderError::Cancelled),
            result = async {
                let r = client
                    .post(&url)
                    .bearer_auth(&self.api_key)
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

        let state = ResponsesStreamState::new();
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

    /// Stream a completion using the Anthropic Messages API dialect.
    ///
    /// This path handles Claude models that the Zen gateway serves via the
    /// Anthropic wire format (`POST /messages`, Anthropic SSE events).
    async fn stream_completion_anthropic(
        &self,
        request: CompletionRequest,
        cancel: CancellationToken,
    ) -> Result<CompletionStream, ProviderError> {
        let model = self.resolve_model(&request);
        let span = tracing::info_span!(
            "provider.stream_completion",
            provider = "opencode",
            dialect = "anthropic",
            model = %model,
        );
        let _guard = span.enter();

        let client = crate::new_client(self.timeout_secs);
        let url = format!("{}/messages", self.api_base);

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

        let body = self.build_anthropic_body(&request, &model);

        let response = tokio::select! {
            _ = cancel.cancelled() => Err(ProviderError::Cancelled),
            result = async {
                let r = client
                    .post(&url)
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
}

#[async_trait]
impl LlmProvider for OpenCodeZenProvider {
    async fn stream_completion(
        &self,
        request: CompletionRequest,
        cancel: CancellationToken,
    ) -> Result<CompletionStream, ProviderError> {
        let model = self.resolve_model(&request);
        if needs_anthropic_dialect(&model) {
            self.stream_completion_anthropic(request, cancel).await
        } else if needs_responses_api(&model) {
            self.stream_completion_responses(request, cancel).await
        } else {
            self.openai_inner.stream_completion(request, cancel).await
        }
    }

    fn context_capacity(&self, model: &str) -> TokenBudget {
        self.openai_inner.context_capacity(model)
    }

    fn approximate_cost(&self, tokens_in: u64, tokens_out: u64) -> f64 {
        self.openai_inner.approximate_cost(tokens_in, tokens_out)
    }

    fn provider_name(&self) -> &'static str {
        "opencode"
    }

    async fn test_connection(&self, _cancel: CancellationToken) -> Result<(), ProviderError> {
        self.openai_inner.test_connection(_cancel.clone()).await
    }

    async fn list_models(
        &self,
        _cancel: CancellationToken,
    ) -> Result<Vec<ModelInfo>, ProviderError> {
        self.openai_inner.list_models(_cancel.clone()).await
    }
}

// ---------------------------------------------------------------------------
// Anthropic SSE stream parser (Muse/Claude path).
//
// Mirrors the event handling from `crate::anthropic::AnthropicStreamState` but
// is kept local to this module to avoid widening the public API surface of the
// anthropic connector. Handles the four Anthropic SSE event types:
// `content_block_start`, `content_block_delta`, `content_block_stop`,
// `message_stop`.
// ---------------------------------------------------------------------------

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

struct ResponsesStreamState {
    parser: BufferedSseParser,
    pending: VecDeque<Result<CompletionChunk, ProviderError>>,
}

impl ResponsesStreamState {
    fn new() -> Self {
        Self { parser: BufferedSseParser::new(), pending: VecDeque::new() }
    }

    fn handle_event(&mut self, event: crate::sse::SseEvent) {
        if event.keepalive {
            self.pending.push_back(Ok(CompletionChunk {
                reasoning: None,
                delta: String::new(),
                tool_call: None,
                is_final: false,
                usage: None,
            }));
            return;
        }

        let Some(data) = event.data else { return };
        let Ok(data) = serde_json::from_str::<serde_json::Value>(&data) else { return };
        match event.event.as_deref().unwrap_or("") {
            "response.output_text.delta" => {
                if let Some(delta) = data.get("delta").and_then(serde_json::Value::as_str) {
                    self.pending.push_back(Ok(CompletionChunk {
                        reasoning: None,
                        delta: delta.to_owned(),
                        tool_call: None,
                        is_final: false,
                        usage: None,
                    }));
                }
            }
            "response.completed" | "response.done" => {
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
                    let args_json = if args_str.trim().is_empty() {
                        serde_json::Value::Null
                    } else {
                        serde_json::from_str(&args_str).unwrap_or(serde_json::Value::Null)
                    };
                    let mut args = crate::protocol::ensure_arguments_object(args_json);
                    // Adaptive tool schemas: re-nest dot-notation arguments
                    // from loose-schema streams before the executor or the
                    // tool-call guard validates against the nested schema.
                    if self.tool_adapted {
                        crate::adapters::schema_loose::unflatten_tool_arguments(&mut args);
                    }
                    self.pending.push_back(Ok(CompletionChunk {
                        reasoning: None,
                        delta: String::new(),
                        tool_call: Some(ToolCall { id, name, arguments: args }),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_name_is_opencode() {
        let p = OpenCodeZenProvider::new("key".to_string(), "model".to_string(), 30);
        assert_eq!(p.provider_name(), "opencode");
    }

    // -----------------------------------------------------------------------
    // Dialect detection tests
    // -----------------------------------------------------------------------

    #[test]
    fn muse_models_use_the_responses_dialect() {
        assert!(!needs_anthropic_dialect("muse-spark-1.2-contributor-free"));
        assert!(!needs_anthropic_dialect("Muse-Spark-1.2"));
        assert!(!needs_anthropic_dialect("some-muse-model"));
        assert!(needs_responses_api("MUSE-v2"));
    }

    #[test]
    fn claude_models_need_anthropic_dialect() {
        assert!(needs_anthropic_dialect("claude-3-5-sonnet"));
        assert!(needs_anthropic_dialect("Claude-3-opus"));
        assert!(needs_anthropic_dialect("claude-4"));
    }

    #[test]
    fn openai_models_do_not_need_anthropic_dialect() {
        assert!(!needs_anthropic_dialect("big-pickle"));
        assert!(!needs_anthropic_dialect("deepseek-v4-flash-free"));
        assert!(!needs_anthropic_dialect("gpt-4o"));
        assert!(!needs_anthropic_dialect("MiMo-7B"));
    }

    #[test]
    fn empty_model_defaults_to_openai() {
        assert!(!needs_anthropic_dialect(""));
    }

    // -----------------------------------------------------------------------
    // Anthropic SSE parser tests
    // -----------------------------------------------------------------------

    #[test]
    fn anthropic_stream_text_only() {
        let mut state = AnthropicStreamState::new();
        let event = |etype: &str, data: &str| crate::sse::SseEvent {
            event: Some(etype.to_string()),
            data: Some(data.to_string()),
            id: None,
            keepalive: false,
        };

        state.handle_event(event(
            "content_block_start",
            r#"{"index":0,"content_block":{"type":"text"}}"#,
        ));
        state.handle_event(event(
            "content_block_delta",
            r#"{"index":0,"delta":{"type":"text_delta","text":"Hello"}}"#,
        ));
        state.handle_event(event(
            "content_block_delta",
            r#"{"index":0,"delta":{"type":"text_delta","text":" world"}}"#,
        ));
        state.handle_event(event("content_block_stop", r#"{"index":0}"#));
        state.handle_event(event("message_stop", "{}"));

        let chunks: Vec<CompletionChunk> = state.pending.drain(..).map(|r| r.unwrap()).collect();
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].delta, "Hello world");
        assert!(!chunks[0].is_final);
        assert!(chunks[1].is_final);
    }

    #[test]
    fn anthropic_stream_tool_use() {
        let mut state = AnthropicStreamState::new();
        let event = |etype: &str, data: &str| crate::sse::SseEvent {
            event: Some(etype.to_string()),
            data: Some(data.to_string()),
            id: None,
            keepalive: false,
        };

        state.handle_event(event(
            "content_block_start",
            r#"{"index":0,"content_block":{"type":"tool_use","id":"call_1","name":"shell"}}"#,
        ));
        state.handle_event(event(
            "content_block_delta",
            r#"{"index":0,"delta":{"type":"input_json_delta","partial_json":"{\"command\":"}}"#,
        ));
        state.handle_event(event(
            "content_block_delta",
            r#"{"index":0,"delta":{"type":"input_json_delta","partial_json":"\"ls\"}"}}"#,
        ));
        state.handle_event(event("content_block_stop", r#"{"index":0}"#));
        state.handle_event(event("message_stop", "{}"));

        let chunks: Vec<CompletionChunk> = state.pending.drain(..).map(|r| r.unwrap()).collect();
        assert_eq!(chunks.len(), 2);
        let tc = chunks[0].tool_call.as_ref().unwrap();
        assert_eq!(tc.id, "call_1");
        assert_eq!(tc.name, "shell");
        assert_eq!(tc.arguments, serde_json::json!({"command": "ls"}));
        assert!(chunks[1].is_final);
    }

    #[test]
    fn anthropic_stream_keepalive_emits_empty_chunk() {
        let mut state = AnthropicStreamState::new();
        state.handle_event(crate::sse::SseEvent {
            event: None,
            data: None,
            id: None,
            keepalive: true,
        });
        assert_eq!(state.pending.len(), 1);
        let chunk = state.pending.pop_front().unwrap().unwrap();
        assert!(chunk.delta.is_empty());
        assert!(!chunk.is_final);
    }

    #[test]
    fn anthropic_stream_tool_empty_args_coerce_to_object() {
        let mut state = AnthropicStreamState::new();
        let event = |etype: &str, data: &str| crate::sse::SseEvent {
            event: Some(etype.to_string()),
            data: Some(data.to_string()),
            id: None,
            keepalive: false,
        };

        state.handle_event(event(
            "content_block_start",
            r#"{"index":0,"content_block":{"type":"tool_use","id":"call_2","name":"noop"}}"#,
        ));
        // No argument deltas — empty tool call.
        state.handle_event(event("content_block_stop", r#"{"index":0}"#));
        state.handle_event(event("message_stop", "{}"));

        let chunks: Vec<CompletionChunk> = state.pending.drain(..).map(|r| r.unwrap()).collect();
        let tc = chunks[0].tool_call.as_ref().unwrap();
        assert!(tc.arguments.is_object(), "empty args must coerce to object");
        assert_eq!(tc.arguments, serde_json::json!({}));
    }

    // -----------------------------------------------------------------------
    // Anthropic body rendering tests (dialect integration)
    // -----------------------------------------------------------------------

    #[test]
    fn muse_model_renders_anthropic_body_via_dialect() {
        let p =
            OpenCodeZenProvider::new("key".into(), "muse-spark-1.2-contributor-free".into(), 30);
        let request = CompletionRequest {
            messages: vec![concerto_core::types::Message {
                role: concerto_core::types::Role::User,
                content: "Hello".into(),
                tool_calls: None,
                tool_results: None,
                reasoning_content: None,
                tokens_in: None,
                tokens_out: None,
            }],
            ..Default::default()
        };
        let body = p.build_anthropic_body(&request, "muse-spark-1.2-contributor-free");
        // Anthropic wire format: stream is always true, max_tokens defaults to 4096
        assert_eq!(body["stream"], true);
        assert_eq!(body["max_tokens"], 4096);
        assert_eq!(body["model"], "muse-spark-1.2-contributor-free");
        // Messages use Anthropic content-array format
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[0]["content"][0]["type"], "text");
        assert_eq!(msgs[0]["content"][0]["text"], "Hello");
    }

    #[test]
    fn openai_model_body_not_affected_by_anthropic_path() {
        // big-pickle should not trigger the Anthropic path
        assert!(!needs_anthropic_dialect("big-pickle"));
    }
}
