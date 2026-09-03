use async_stream::stream;
use async_trait::async_trait;
use concerto_core::error::{describe_error_chain, ProviderError};
use concerto_core::traits::{CompletionStream, LlmProvider};
use concerto_core::types::{
    CompletionChunk, CompletionRequest, CompletionUsage, ModelInfo, TokenBudget, ToolCall,
};
use concerto_core::CancellationToken;
use futures::stream::StreamExt;
use std::collections::{HashMap, VecDeque};

use crate::adapters::{Dialect, OpenAiChatDialect};
use crate::sse::BufferedSseParser;

/// Re-export of the reasoning-echo policy (ADR-46).
///
/// The canonical enum lives in `crate::adapters` — echo is a dialect concern,
/// and the policy is part of the [`Dialect::render_chat_body`] signature. This
/// re-export keeps the historical `crate::openai::ReasoningEcho` path working
/// for code that names it next to [`OpenAiProvider`] (e.g. `crate::opencode`).
pub use crate::adapters::ReasoningEcho;

pub struct OpenAiProvider {
    api_key: String,
    api_base: String,
    model: String,
    timeout_secs: u64,
    reasoning_echo: ReasoningEcho,
    /// Tool-schema presentation tier (adaptive tool schemas). Resolved per
    /// request against the actual model name; `Auto` (default) keeps every
    /// non-weak model on the verbatim strict schema.
    tool_schema_mode: concerto_config::ToolSchemaMode,
    dialect: OpenAiChatDialect,
}

impl OpenAiProvider {
    pub fn new(api_key: String, model: String, timeout_secs: u64) -> Self {
        Self {
            api_key,
            api_base: "https://api.openai.com/v1".to_string(),
            model,
            timeout_secs,
            reasoning_echo: ReasoningEcho::IfPresent,
            tool_schema_mode: concerto_config::ToolSchemaMode::default(),
            dialect: OpenAiChatDialect,
        }
    }

    pub fn with_api_base(mut self, api_base: String) -> Self {
        self.api_base = api_base;
        self
    }

    /// Set the reasoning-content echo policy (ADR-46).
    ///
    /// Defaults to [`ReasoningEcho::IfPresent`]. DeepSeek-backed endpoints such
    /// as OpenCode Zen should set [`ReasoningEcho::Always`] so assistant
    /// messages in a tool-call history never carry reasoning that the API
    /// rejects.
    pub fn with_reasoning_echo(mut self, echo: ReasoningEcho) -> Self {
        self.reasoning_echo = echo;
        self
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
}

// ---------------------------------------------------------------------------
// Request-body rendering now lives in `crate::adapters::openai_compat`
// (`OpenAiChatDialect`); stream parsing remains here in the connector.
// ---------------------------------------------------------------------------

#[derive(Default)]
struct PartialToolCall {
    id: String,
    name: String,
    arguments: String,
}

struct OpenAiStreamState {
    parser: BufferedSseParser,
    pending: VecDeque<Result<CompletionChunk, ProviderError>>,
    partial_tools: HashMap<usize, PartialToolCall>,
    /// Whether the request that produced this stream was rendered with
    /// loose (weak-model) tool schemas. When set, emitted tool-call
    /// arguments are re-nested from dot-notation back into the tools'
    /// original nested shape before the executor or the tool-call guard
    /// sees them (see `crate::adapters::schema_loose`).
    tool_adapted: bool,
    /// Accumulated `reasoning_content` deltas for the current turn (ADR-46).
    ///
    /// DeepSeek-style endpoints stream reasoning incrementally across many
    /// SSE events; the per-turn buffer is drained into a single final chunk so
    /// the collected assistant message can carry the full reasoning text.
    reasoning_buffer: String,
    /// Provider-reported usage observed before the stream ends (ADR-48 §4).
    ///
    /// OpenAI-compatible endpoints surface `usage` in a trailing chunk (either
    /// alongside `finish_reason` or in a separate chunk with empty `choices`
    /// when `stream_options.include_usage` is set). The first observed usage
    /// is attached to the terminal chunk only.
    usage: Option<CompletionUsage>,
}

impl OpenAiStreamState {
    fn new() -> Self {
        Self {
            parser: BufferedSseParser::new(),
            pending: VecDeque::new(),
            partial_tools: HashMap::new(),
            tool_adapted: false,
            reasoning_buffer: String::new(),
            usage: None,
        }
    }

    /// Capture a provider-reported `usage` object (top-level `usage` member of
    /// an SSE event, as OpenAI/DeepSeek emit it). The first observation wins;
    /// usage is only ever attached to the terminal chunk.
    fn capture_usage(&mut self, parsed: &serde_json::Value) {
        if self.usage.is_some() {
            return;
        }
        let Some(usage) = parsed.get("usage").and_then(|u| u.as_object()) else {
            return;
        };
        let usage = CompletionUsage {
            prompt_tokens: usage.get("prompt_tokens").and_then(|v| v.as_u64()),
            completion_tokens: usage.get("completion_tokens").and_then(|v| v.as_u64()),
        };
        // Only record usage that actually carries at least one token count.
        if usage.prompt_tokens.is_some() || usage.completion_tokens.is_some() {
            self.usage = Some(usage);
        }
    }

    /// Emit a final chunk carrying the accumulated reasoning (if any) before
    /// the stream's terminal chunk, so `is_final` stays on the last chunk.
    fn emit_reasoning_if_any(&mut self) {
        if !self.reasoning_buffer.is_empty() {
            self.pending.push_back(Ok(CompletionChunk {
                delta: String::new(),
                reasoning: Some(std::mem::take(&mut self.reasoning_buffer)),
                tool_call: None,
                is_final: false,
                usage: None,
            }));
        }
    }

    fn emit_tool_call(&mut self, index: usize) {
        if let Some(ptc) = self.partial_tools.remove(&index) {
            // Empty or garbage accumulated arguments must not serialize to
            // `"null"` / `"\"ls\""` on the wire (`HTTP 400: function.arguments
            // must be a JSON object`) — coerce to `{}` first.
            let mut args = crate::protocol::ensure_arguments_object(
                serde_json::from_str(&ptc.arguments).unwrap_or(serde_json::Value::Null),
            );
            // Adaptive tool schemas: when the request was rendered with loose
            // (weak-model) schemas, the model answers in the flattened
            // dot-notation shape — re-nest before the executor or the
            // tool-call guard validates against the original nested schema.
            if self.tool_adapted {
                crate::adapters::schema_loose::unflatten_tool_arguments(&mut args);
            }
            self.pending.push_back(Ok(CompletionChunk {
                delta: String::new(),
                reasoning: None,
                tool_call: Some(ToolCall { id: ptc.id, name: ptc.name, arguments: args }),
                is_final: false,
                usage: None,
            }));
        }
    }

    fn emit_remaining_tools(&mut self) {
        let mut indices: Vec<_> = self.partial_tools.keys().copied().collect();
        indices.sort();
        for idx in indices {
            self.emit_tool_call(idx);
        }
    }

    fn handle_event(&mut self, event: crate::sse::SseEvent) {
        if event.keepalive {
            // Liveness signal (SSE comment line): emit an empty chunk so the
            // stream stays active and the orchestrator idle timeout does not
            // fire during long keep-alive-only periods.
            self.pending.push_back(Ok(CompletionChunk {
                delta: String::new(),
                reasoning: None,
                tool_call: None,
                is_final: false,
                usage: None,
            }));
            return;
        }
        let data = match event.data {
            Some(d) => d,
            None => return,
        };

        if data == "[DONE]" {
            self.emit_remaining_tools();
            self.emit_reasoning_if_any();
            self.pending.push_back(Ok(CompletionChunk {
                delta: String::new(),
                reasoning: None,
                tool_call: None,
                is_final: true,
                usage: self.usage.take(),
            }));
            return;
        }

        let parsed: serde_json::Value = match serde_json::from_str(&data) {
            Ok(v) => v,
            Err(_) => return,
        };

        // OpenAI-compatible endpoints surface `usage` either alongside the
        // final choice or in a dedicated chunk with empty `choices` (when
        // `stream_options.include_usage` is set). Capture it now so the
        // terminal chunk can carry it (ADR-48 §4).
        self.capture_usage(&parsed);

        let Some(choices) = parsed["choices"].as_array() else {
            return;
        };
        let Some(choice) = choices.first() else {
            return;
        };

        if let Some(delta) = choice["delta"].as_object() {
            // Capture DeepSeek-style `reasoning_content`, which is streamed
            // incrementally across SSE deltas (ADR-46).
            if let Some(reasoning) = delta.get("reasoning_content").and_then(|c| c.as_str()) {
                self.reasoning_buffer.push_str(reasoning);
            }

            if let Some(content) = delta.get("content").and_then(|c| c.as_str()) {
                self.pending.push_back(Ok(CompletionChunk {
                    delta: content.to_string(),
                    reasoning: None,
                    tool_call: None,
                    is_final: false,
                    usage: None,
                }));
            }

            if let Some(tc_arr) = delta.get("tool_calls").and_then(|v| v.as_array()) {
                for tc in tc_arr {
                    let index = tc.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;

                    let partial = self.partial_tools.entry(index).or_default();

                    if let Some(id) = tc.get("id").and_then(|v| v.as_str()) {
                        partial.id = id.to_string();
                    }
                    if let Some(func) = tc.get("function") {
                        if let Some(name) = func.get("name").and_then(|v| v.as_str()) {
                            partial.name = name.to_string();
                        }
                        if let Some(args) = func.get("arguments").and_then(|v| v.as_str()) {
                            partial.arguments.push_str(args);
                        }
                    }
                }
            }
        }

        if let Some(finish_reason) = choice["finish_reason"].as_str() {
            if !finish_reason.is_empty() && finish_reason != "null" {
                self.emit_remaining_tools();
                self.emit_reasoning_if_any();
                self.pending.push_back(Ok(CompletionChunk {
                    delta: String::new(),
                    reasoning: None,
                    tool_call: None,
                    is_final: true,
                    usage: self.usage.take(),
                }));
            }
        }
    }
}

#[async_trait]
impl LlmProvider for OpenAiProvider {
    async fn test_connection(&self, _cancel: CancellationToken) -> Result<(), ProviderError> {
        let client = crate::new_client(self.timeout_secs);
        let url = format!("{}/models", self.api_base);
        let resp = client.get(&url).bearer_auth(&self.api_key).send().await.map_err(|e| {
            ProviderError::Other(format!("openai connection failed: {}", describe_error_chain(&e)))
        })?;
        if resp.status().is_success() {
            Ok(())
        } else if resp.status().as_u16() == 401 {
            Err(ProviderError::AuthFailure)
        } else {
            Err(ProviderError::Other(format!("openai returned {}", resp.status())))
        }
    }

    async fn list_models(
        &self,
        _cancel: CancellationToken,
    ) -> Result<Vec<ModelInfo>, ProviderError> {
        let client = crate::new_client(self.timeout_secs);
        let url = format!("{}/models", self.api_base);
        let resp = client.get(&url).bearer_auth(&self.api_key).send().await.map_err(|e| {
            ProviderError::Other(format!("openai list_models failed: {}", describe_error_chain(&e)))
        })?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(ProviderError::Other(format!(
                "openai list_models returned {status}: {text}"
            )));
        }

        let json: serde_json::Value = resp.json().await.map_err(|e| {
            ProviderError::Other(format!(
                "openai list_models parse failed: {}",
                describe_error_chain(&e)
            ))
        })?;

        let models = json["data"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| {
                        let id = v["id"].as_str()?.to_string();
                        let owned_by = v["owned_by"].as_str().map(String::from);
                        Some(ModelInfo { id: id.clone(), name: Some(id), owned_by })
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
        let span = tracing::info_span!("openai_stream_completion", model = %self.model);
        let _guard = span.enter();

        let client = crate::new_client(self.timeout_secs);
        let url = format!("{}/chat/completions", self.api_base);

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

        let body = self.dialect.render_chat_body(&request, &model, self.reasoning_echo);

        let response = tokio::select! {
            _ = cancel.cancelled() => Err(ProviderError::Cancelled),
            result = async {
                client
                    .post(&url)
                    .header("Authorization", format!("Bearer {}", self.api_key))
                    .header("Content-Type", "application/json")
                    .json(&body)
                    .send()
                    .await
                    .map_err(|e| ProviderError::Network(format!("request failed: {}", describe_error_chain(&e))))
            } => result,
        }?;

        if !response.status().is_success() {
            let status = response.status();
            let retry_after = crate::retry::parse_retry_after(response.headers());
            let text = response.text().await.unwrap_or_default();
            return Err(crate::retry::map_http_error(status, &text, retry_after));
        }

        let mut state = OpenAiStreamState::new();
        if tool_adapted {
            state.tool_adapted = true;
        }

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
                        // Drain ALL pending items from this chunk, not just one
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
            // Drain any remaining pending items at end of stream
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
        let input_cost = (tokens_in as f64 / 1_000_000.0) * 2.50;
        let output_cost = (tokens_out as f64 / 1_000_000.0) * 10.00;
        input_cost + output_cost
    }

    fn provider_name(&self) -> &'static str {
        "openai"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ADR-46: a streamed delta carrying `reasoning_content` is captured into a
    /// `CompletionChunk::reasoning`, accumulated across deltas, and emitted
    /// once on stream end.
    #[test]
    fn stream_reasoning_content_is_captured() {
        let mut state = OpenAiStreamState::new();

        // First reasoning delta.
        state.handle_event(crate::sse::SseEvent {
            event: None,
            data: Some(r#"{"choices":[{"delta":{"reasoning_content":"step one"}}]}"#.to_string()),
            id: None,
            keepalive: false,
        });
        // Second incremental reasoning delta.
        state.handle_event(crate::sse::SseEvent {
            event: None,
            data: Some(
                r#"{"choices":[{"delta":{"reasoning_content":" and step two"}}]}"#.to_string(),
            ),
            id: None,
            keepalive: false,
        });
        // No reasoning chunks should be emitted while streaming (still buffered).
        assert!(state.pending.is_empty());

        // End the stream: the accumulated reasoning is flushed before the final chunk.
        state.handle_event(crate::sse::SseEvent {
            event: None,
            data: Some("[DONE]".to_string()),
            id: None,
            keepalive: false,
        });

        assert_eq!(state.pending.len(), 2, "reasoning chunk + final chunk");
        let reasoning_chunk = state.pending.pop_front().unwrap().unwrap();
        assert_eq!(reasoning_chunk.reasoning.as_deref(), Some("step one and step two"));
        assert!(!reasoning_chunk.is_final, "reasoning chunk is not the terminal chunk");
        assert!(reasoning_chunk.delta.is_empty());

        let final_chunk = state.pending.pop_front().unwrap().unwrap();
        assert!(final_chunk.is_final);
        assert!(final_chunk.reasoning.is_none());
    }

    /// Parser parity: a scripted SSE stream carrying reasoning deltas, tool-call
    /// argument fragments and a trailing `[DONE]` reduces to exactly three
    /// chunks in order — the tool call, the reasoning accumulated into one
    /// chunk, then the final chunk. Guards the wire→canonical reducer against
    /// accidental drift that would break reason/tool round-trips.
    #[test]
    fn stream_parser_accumulates_reasoning_tools_and_done_in_order() {
        let mut state = OpenAiStreamState::new();

        let event = |data: &str| crate::sse::SseEvent {
            event: None,
            data: Some(data.to_string()),
            id: None,
            keepalive: false,
        };

        // Reasoning delta (DeepSeek-style, streamed separately from content).
        state.handle_event(event(
            r#"{"choices":[{"delta":{"reasoning_content":"think step one"}}]}"#,
        ));
        // Tool-call delta fragment: id, name and the argument prefix.
        state.handle_event(event(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"shell","arguments":""}}]}}]}"#,
        ));
        // Tool-call argument fragment (JSON string accumulated across deltas).
        state.handle_event(event(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"command\":\"ls\"}"}}]}}]}"#,
        ));
        // Deltas buffer; nothing emitted mid-stream.
        assert!(state.pending.is_empty(), "deltas buffer; nothing emitted mid-stream");

        state.handle_event(event("[DONE]"));

        let chunks: Vec<CompletionChunk> =
            state.pending.drain(..).map(|result| result.expect("chunk emitted")).collect();
        assert_eq!(chunks.len(), 3, "tool-call chunk + reasoning chunk + final chunk");

        // 0) Tool call with arguments parsed from the JSON argument fragments.
        let tool = chunks[0].tool_call.as_ref().expect("tool-call chunk emitted");
        assert_eq!(tool.id, "call_1");
        assert_eq!(tool.name, "shell");
        assert_eq!(tool.arguments, serde_json::json!({"command": "ls"}));
        assert!(!chunks[0].is_final);
        assert!(chunks[0].reasoning.is_none());

        // 1) Reasoning accumulated across deltas into ONE chunk.
        assert_eq!(chunks[1].reasoning.as_deref(), Some("think step one"));
        assert!(chunks[1].delta.is_empty());
        assert!(chunks[1].tool_call.is_none());

        // 2) Final chunk.
        assert!(chunks[2].is_final);
        assert!(chunks[2].reasoning.is_none());
        assert!(chunks[2].tool_call.is_none());
    }

    /// Tool-call arguments must always land on the canonical side as a JSON
    /// object. Empty accumulated arguments (which previously became
    /// `Value::Null` and later serialized to the string `"null"`) coerce to
    /// `{}`.
    #[test]
    fn stream_empty_arguments_coerce_to_empty_object() {
        let mut state = OpenAiStreamState::new();
        let event = |data: &str| crate::sse::SseEvent {
            event: None,
            data: Some(data.to_string()),
            id: None,
            keepalive: false,
        };

        state.handle_event(event(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"shell","arguments":""}}]}}]}"#,
        ));
        state.handle_event(event("[DONE]"));

        let chunks: Vec<CompletionChunk> =
            state.pending.drain(..).map(|result| result.expect("chunk emitted")).collect();
        let tool = chunks.iter().find_map(|c| c.tool_call.as_ref()).expect("tool-call chunk");
        assert!(tool.arguments.is_object(), "arguments must be a JSON object");
        assert_eq!(tool.arguments, serde_json::json!({}));
    }

    /// A raw string delivered as valid JSON (`"ls"`) is *not* a JSON object,
    /// which the upstream schema requires — coerce to `{}`, never pass it
    /// through as the string verbatim.
    #[test]
    fn stream_non_object_string_arguments_coerce_to_empty_object() {
        let mut state = OpenAiStreamState::new();
        let event = |data: &str| crate::sse::SseEvent {
            event: None,
            data: Some(data.to_string()),
            id: None,
            keepalive: false,
        };

        state.handle_event(event(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"shell","arguments":"\"ls\""}}]}}]}"#,
        ));
        state.handle_event(event("[DONE]"));

        let chunks: Vec<CompletionChunk> =
            state.pending.drain(..).map(|result| result.expect("chunk emitted")).collect();
        let tool = chunks.iter().find_map(|c| c.tool_call.as_ref()).expect("tool-call chunk");
        assert!(tool.arguments.is_object(), "arguments must be a JSON object");
        assert_eq!(tool.arguments, serde_json::json!({}));
    }

    /// A well-formed object argument passes through the normalizer unchanged.
    #[test]
    fn stream_object_arguments_are_preserved() {
        let mut state = OpenAiStreamState::new();
        let event = |data: &str| crate::sse::SseEvent {
            event: None,
            data: Some(data.to_string()),
            id: None,
            keepalive: false,
        };

        state.handle_event(event(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"shell","arguments":"{\"command\":\"ls\"}"}}]}}]}"#,
        ));
        state.handle_event(event("[DONE]"));

        let chunks: Vec<CompletionChunk> =
            state.pending.drain(..).map(|result| result.expect("chunk emitted")).collect();
        let tool = chunks.iter().find_map(|c| c.tool_call.as_ref()).expect("tool-call chunk");
        assert_eq!(tool.arguments, serde_json::json!({"command": "ls"}));
        assert!(tool.arguments.is_object(), "arguments must be a JSON object");
    }

    /// Adaptive tool schemas: when the request was rendered with loose
    /// (weak-model) schemas, dot-notation arguments emitted by the model are
    /// re-nested into the tool's original nested shape before leaving the
    /// connector. Without the flag the arguments pass through untouched, so
    /// strict models keep byte-identical behavior.
    #[test]
    fn stream_tool_adaptation_unflattens_dotted_arguments() {
        // Serialize through `json!` so argument strings are correctly escaped
        // inside the SSE data payload.
        let event = |data: String| crate::sse::SseEvent {
            event: None,
            data: Some(data),
            id: None,
            keepalive: false,
        };
        let start_event = || {
            event(
                serde_json::json!({
                    "choices": [{"delta": {"tool_calls": [
                        {"index": 0, "id": "call_1", "type": "function",
                         "function": {"name": "runner", "arguments": ""}}]}}]
                })
                .to_string(),
            )
        };
        let args_event = |arguments: &str| {
            event(
                serde_json::json!({
                    "choices": [{"delta": {"tool_calls": [
                        {"index": 0, "function": {"arguments": arguments}}]}}]
                })
                .to_string(),
            )
        };
        let dotted_args = r#"{"config.mode":"fast","config.retries":2}"#;

        // Adapted stream: dotted keys are re-nested.
        let mut adapted = OpenAiStreamState::new();
        adapted.tool_adapted = true;
        adapted.handle_event(start_event());
        adapted.handle_event(args_event(dotted_args));
        adapted.handle_event(event("[DONE]".to_string()));
        let chunks: Vec<CompletionChunk> =
            adapted.pending.drain(..).map(|result| result.expect("chunk emitted")).collect();
        let tool = chunks.iter().find_map(|c| c.tool_call.as_ref()).expect("tool-call chunk");
        assert_eq!(
            tool.arguments,
            serde_json::json!({"config": {"mode": "fast", "retries": 2}}),
            "dotted arguments must be re-nested on adapted streams"
        );

        // Non-adapted stream: identical wire input passes through unchanged.
        let mut strict = OpenAiStreamState::new();
        strict.handle_event(start_event());
        strict.handle_event(args_event(dotted_args));
        strict.handle_event(event("[DONE]".to_string()));
        let chunks: Vec<CompletionChunk> =
            strict.pending.drain(..).map(|result| result.expect("chunk emitted")).collect();
        let tool = chunks.iter().find_map(|c| c.tool_call.as_ref()).expect("tool-call chunk");
        assert_eq!(
            tool.arguments,
            serde_json::json!({"config.mode": "fast", "config.retries": 2}),
            "strict streams must pass arguments through untouched"
        );
    }

    /// ADR-48 §4: a trailing `usage` object (OpenAI `stream_options.include_usage`
    /// style, in a chunk with empty `choices`) is captured and attached to the
    /// terminal chunk only.
    #[test]
    fn stream_captures_usage_on_final_chunk() {
        let mut state = OpenAiStreamState::new();
        let event = |data: &str| crate::sse::SseEvent {
            event: None,
            data: Some(data.to_string()),
            id: None,
            keepalive: false,
        };

        // Content delta (no usage on intermediate chunks).
        state.handle_event(event(r#"{"choices":[{"delta":{"content":"hello"}}]}"#));

        // Dedicated usage chunk with empty choices (include_usage style).
        state.handle_event(event(
            r#"{"choices":[],"usage":{"prompt_tokens":42,"completion_tokens":7}}"#,
        ));
        // And again at the real end — the first observation wins.
        state.handle_event(event("[DONE]"));

        let chunks: Vec<CompletionChunk> =
            state.pending.drain(..).map(|result| result.expect("chunk emitted")).collect();
        assert_eq!(chunks.len(), 2, "content chunk + final chunk");
        assert_eq!(chunks[0].usage, None, "usage is only reported on the terminal chunk");
        assert!(chunks[1].is_final);
        assert_eq!(
            chunks[1].usage,
            Some(CompletionUsage { prompt_tokens: Some(42), completion_tokens: Some(7) })
        );
    }

    /// ADR-48 §4: the legacy wire shape embeds `usage` alongside the final
    /// choice (`finish_reason`) in the same chunk.
    #[test]
    fn stream_captures_usage_embedded_in_final_choice() {
        let mut state = OpenAiStreamState::new();
        let event = |data: &str| crate::sse::SseEvent {
            event: None,
            data: Some(data.to_string()),
            id: None,
            keepalive: false,
        };

        state.handle_event(event(
            r#"{"choices":[{"delta":{"content":"done"},"finish_reason":"stop"}],"usage":{"prompt_tokens":9,"completion_tokens":3}}"#,
        ));
        state.handle_event(event("[DONE]"));

        let chunks: Vec<CompletionChunk> =
            state.pending.drain(..).map(|result| result.expect("chunk emitted")).collect();
        let final_chunk = chunks.iter().find(|chunk| chunk.is_final).expect("final chunk");
        assert_eq!(
            final_chunk.usage,
            Some(CompletionUsage { prompt_tokens: Some(9), completion_tokens: Some(3) })
        );
    }

    /// ADR-48 §4: a `usage` object with no token counts is ignored — it is not
    /// a measurement and must not be surfaced as one.
    #[test]
    fn stream_ignores_usage_without_counts() {
        let mut state = OpenAiStreamState::new();
        let event = |data: &str| crate::sse::SseEvent {
            event: None,
            data: Some(data.to_string()),
            id: None,
            keepalive: false,
        };
        state.handle_event(event(r#"{"choices":[],"usage":{}}"#));
        state.handle_event(event("[DONE]"));
        let chunks: Vec<CompletionChunk> =
            state.pending.drain(..).map(|result| result.expect("chunk emitted")).collect();
        assert_eq!(chunks.last().unwrap().usage, None);
    }
}
