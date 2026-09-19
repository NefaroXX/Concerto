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

/// Synthetic tool-call slot used by the content-embedded envelope fallback
/// (proxy Fix 2 STRICT). Real proxy indexes are small wire integers from the
/// `tool_calls` array; this sentinel, combined with the empty-`partial_tools`
/// gate, guarantees the synthesized call can never collide with a real one.
const ENVELOPE_CALL_INDEX: usize = usize::MAX;

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
    /// Accumulated `content` deltas for the current turn (proxy Fix 2 STRICT —
    /// content-embedded tool-call envelope).
    ///
    /// Content is buffered instead of streamed eagerly so the fallback can
    /// judge the COMPLETE turn text with a strict full-text parse (no substring
    /// extraction). Non-envelope content is drained as plain text at the
    /// terminal point — identical `delta` output, just aggregated at turn end
    /// instead of per-delta (mirrors how reasoning and tool args already
    /// buffer to the terminal point).
    content_buffer: String,
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
            content_buffer: String::new(),
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

    /// Strict, full-text-only parse of the turn's accumulated `content` as a
    /// proxy tool-call envelope (Fix 2 STRICT).
    ///
    /// Accepted — the ENTIRE content must be exactly one JSON object of one of
    /// these shapes (full-text parse only, no substring extraction):
    ///
    ///   * `{"name": "<string>", "arguments": { … }}`       — object arguments
    ///   * `{"name": "<string>", "arguments": "<json …>"}`  — string arguments
    ///   * `{"name": "<string>", "input": { … }}`           — `input` alias
    ///     (object only, consistent with the Fix 1 flat path)
    ///
    /// An optional string `id` member is carried through when present. Anything
    /// else — trailing prose (the parse must consume the whole payload), JSON
    /// arrays, missing `name`/arguments, wrong member types, `input` as a
    /// non-object — returns `None` and the content stays plain text with zero
    /// behavioral change. No heuristic text mining here: loose recovery from
    /// free text is the tool guard's / driver's job.
    fn parse_content_envelope(content: &str) -> Option<PartialToolCall> {
        let parsed: serde_json::Value = serde_json::from_str(content).ok()?;
        let object = parsed.as_object()?;
        let name = object.get("name")?.as_str()?.to_string();
        let arguments = match object.get("arguments") {
            Some(serde_json::Value::Object(inner)) => {
                serde_json::Value::Object(inner.clone()).to_string()
            }
            Some(serde_json::Value::String(inner)) => inner.clone(),
            // `arguments` present but not Object|String → reject the envelope.
            Some(_) => return None,
            // `arguments` absent → `input` alias (object only, Fix 1-consistent).
            None => {
                let input = object.get("input")?.as_object()?;
                serde_json::Value::Object(input.clone()).to_string()
            }
        };
        let id = object.get("id").and_then(|v| v.as_str()).unwrap_or_default().to_string();
        Some(PartialToolCall { id, name, arguments })
    }

    /// Terminal resolution of the buffered `content` (proxy Fix 2 STRICT).
    ///
    /// Invoked from both terminal paths (`[DONE]` and `finish_reason`) before
    /// tools/reasoning/final are emitted:
    ///
    ///   * real tool deltas observed (`partial_tools` non-empty) → the
    ///     buffered content is drained as plain text; the fallback never
    ///     competes with real deltas (no double-emit);
    ///   * otherwise a strict envelope match synthesizes a [`PartialToolCall`]
    ///     at [`ENVELOPE_CALL_INDEX`], emitted by the EXISTING
    ///     [`Self::emit_tool_call`] pipeline (strict parse with the Fix 3 retry
    ///     chain + `Null` fallback, plus loose-schema un-flattening);
    ///   * otherwise the content is drained as plain text — same `delta`
    ///     payload as before, aggregated at turn end instead of per-delta.
    ///
    /// OpenRouter/NIM inherit this fallback through their `OpenAiProvider`
    /// inner — no wrapper edits.
    fn flush_content(&mut self) {
        if self.content_buffer.is_empty() {
            return;
        }
        if !self.partial_tools.is_empty() {
            self.emit_buffered_content();
            return;
        }
        let content = std::mem::take(&mut self.content_buffer);
        if let Some(envelope) = Self::parse_content_envelope(&content) {
            self.partial_tools.insert(ENVELOPE_CALL_INDEX, envelope);
        } else {
            self.pending.push_back(Ok(CompletionChunk {
                delta: content,
                reasoning: None,
                tool_call: None,
                is_final: false,
                usage: None,
            }));
        }
    }

    /// Drain the turn's buffered `content` as one plain-text chunk (real tool
    /// deltas won, or the content is not a strict envelope).
    fn emit_buffered_content(&mut self) {
        let content = std::mem::take(&mut self.content_buffer);
        self.pending.push_back(Ok(CompletionChunk {
            delta: content,
            reasoning: None,
            tool_call: None,
            is_final: false,
            usage: None,
        }));
    }

    /// Emit the accumulated arguments for a finished tool call (proxy Fix 3).
    ///
    /// Empty accumulated arguments become `Null` silently. Non-empty arguments
    /// parse strictly; if that parse fails, a short retry chain rescues the two
    /// common proxy corruption classes — surrounding whitespace (re-parse after
    /// trimming, the `double-wrap`/pad case) and single-quoted JSON (re-parse
    /// after replacing `'` with `"`). The single-quote fixup is safe by
    /// construction: it only runs after the strict parse already failed and
    /// its result is still validated by `serde_json`, so legitimate apostrophes
    /// inside string values that strict JSON accepts are never touched. If
    /// every parse fails, the tool call still emits with `Null` arguments and a
    /// `tracing::warn!` carries the tool name, the raw payload length (never
    /// the payload itself — avoids log injection) and the original parse error.
    fn emit_tool_call(&mut self, index: usize) {
        if let Some(ptc) = self.partial_tools.remove(&index) {
            let mut args = if ptc.arguments.trim().is_empty() {
                serde_json::Value::Null
            } else {
                match serde_json::from_str(&ptc.arguments) {
                    Ok(parsed) => parsed,
                    Err(parse_err) => {
                        // Proxy sent malformed JSON arguments. Try a few common
                        // fixes before giving up.
                        let cleaned = ptc.arguments.trim();
                        // Some proxies double-wrap: "{"key": "val"}" → try as-is
                        let result = serde_json::from_str(cleaned).or_else(|_| {
                            // Some proxies send single-quoted keys
                            let fixed = cleaned.replace('\'', "\"");
                            serde_json::from_str(&fixed)
                        });
                        match result {
                            Ok(parsed) => parsed,
                            Err(_) => {
                                tracing::warn!(
                                    tool_name = %ptc.name,
                                    raw_len = ptc.arguments.len(),
                                    parse_error = %parse_err,
                                    "emit_tool_call: failed to parse arguments, emitting null."
                                );
                                serde_json::Value::Null
                            }
                        }
                    }
                }
            };
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
                tool_call: Some(ToolCall {
                    id: ptc.id,
                    name: ptc.name,
                    arguments: args,
                    ..Default::default()
                }),
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

    /// Feed a non-streamed completion body through the stream reducer.
    ///
    /// A `stream: false` response carries the full assistant turn in
    /// `choices[].message` (complete `tool_calls` entries, no `index`)
    /// instead of the streamed `choices[].delta` fragments. Rewriting the
    /// body into the delta shape lets the existing reducer handle both
    /// transports: reasoning capture (ADR-46), argument-string coercion to
    /// objects, loose-schema un-flattening, and `usage` attached to the
    /// terminal chunk all apply unchanged.
    fn handle_non_stream_body(&mut self, mut parsed: serde_json::Value) {
        if let Some(choices) = parsed.get_mut("choices").and_then(serde_json::Value::as_array_mut) {
            for choice in choices.iter_mut() {
                let Some(choice_object) = choice.as_object_mut() else { continue };
                let Some(message) = choice_object.remove("message") else { continue };
                let mut delta = message;
                if let Some(tool_calls) =
                    delta.get_mut("tool_calls").and_then(serde_json::Value::as_array_mut)
                {
                    for (index, call) in tool_calls.iter_mut().enumerate() {
                        if let Some(call_object) = call.as_object_mut() {
                            call_object.insert("index".to_owned(), serde_json::Value::from(index));
                        }
                    }
                }
                choice_object.insert("delta".to_owned(), delta);
            }
        }
        self.handle_event(crate::sse::SseEvent {
            event: None,
            data: Some(parsed.to_string()),
            id: None,
            keepalive: false,
        });
        self.handle_event(crate::sse::SseEvent {
            event: None,
            data: Some("[DONE]".to_string()),
            id: None,
            keepalive: false,
        });
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
            self.flush_content();
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
                // Buffer the turn's content instead of streaming it eagerly:
                // the Fix 2 STRICT fallback needs the COMPLETE text to judge
                // whether it is a tool-call envelope (full-text parse only).
                // Non-envelope content is drained as plain text at the
                // terminal point — identical output.
                self.content_buffer.push_str(content);
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

                    // ── Proxy fallback: flat format (name / arguments directly
                    // on the tool-call object, no `function` wrapper) ──
                    //
                    // Some proxy gateways translate a model's native tool-call
                    // shape into `{id, name, arguments}` directly on the tool
                    // call object. Both fallbacks are gated conservatively on
                    // the same tool call carrying BOTH a string `name` AND
                    // `arguments` (string or object) so unrelated payloads
                    // cannot be misread as tool calls; nothing is extracted
                    // from `content` or free text.
                    //
                    // (a) Flat string arguments: accumulate across deltas the
                    // same way `function.arguments` fragments do.
                    if let Some(name) = tc.get("name").and_then(|v| v.as_str()) {
                        if let Some(args) = tc.get("arguments").and_then(|v| v.as_str()) {
                            partial.name = name.to_string();
                            partial.arguments.push_str(args);
                        }
                    }
                    // (b) Flat object arguments (or an `input` alias): one-shot,
                    // the complete object serializes directly and needs no chunk
                    // accumulation. `emit_tool_call` parses it through the same
                    // strict-parse / Fix 3 retry chain + loose-schema
                    // un-flattening pipeline as every other shape.
                    if let Some(name) = tc.get("name").and_then(|v| v.as_str()) {
                        let object_args = match tc.get("arguments") {
                            Some(v) => v.as_object(),
                            None => tc.get("input").and_then(|v| v.as_object()),
                        };
                        if let Some(object_args) = object_args {
                            partial.name = name.to_string();
                            partial.arguments =
                                serde_json::Value::Object(object_args.clone()).to_string();
                        }
                    }
                }
            }
        }

        if let Some(finish_reason) = choice["finish_reason"].as_str() {
            if !finish_reason.is_empty() && finish_reason != "null" {
                self.flush_content();
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
                        Some(ModelInfo {
                            id: id.clone(),
                            name: Some(id),
                            owned_by,
                            supports_tool_calling: None,
                        })
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

        // Adaptive tool schemas + non-streamed transport (weak-model tier):
        // when the resolved model matches the loose tier, rewrite the
        // request's tool definitions in place before the dialect renders the
        // body AND request the completion non-streamed so tool-call
        // arguments arrive whole. Strict models are untouched — their wire
        // output stays byte-identical (streamed).
        let mut request = request;
        let tool_adapted = crate::adapters::schema_loose::non_streaming_transport_active(
            self.tool_schema_mode,
            &model,
        );
        if tool_adapted {
            if let Some(tools) = request.tools.as_mut() {
                crate::adapters::schema_loose::adapt_tool_definitions(tools);
                tracing::debug!(
                    model = %model,
                    tools = tools.len(),
                    "weak-model loose tool schemas applied (flattened + examples)"
                );
            }
            request.stream = false;
            tracing::info!(
                model = %model,
                "weak tool-calling model: requesting non-streamed completion (stream=false)"
            );
        }
        let non_streamed = !request.stream;

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

        // Non-streamed responses (weak-model tier, or any request rendered
        // with `stream: false`) carry a single JSON completion object instead
        // of an SSE event stream: read the whole body and reduce it through
        // the same stream state so downstream chunk shapes stay identical.
        if non_streamed {
            let body_text = tokio::select! {
                _ = cancel.cancelled() => return Err(ProviderError::Cancelled),
                result = response.text() => result.map_err(|e| {
                    ProviderError::Network(format!(
                        "failed to read response body: {}",
                        describe_error_chain(&e)
                    ))
                })?,
            };
            let parsed: serde_json::Value = serde_json::from_str(&body_text).map_err(|e| {
                ProviderError::Other(format!("invalid JSON in non-streamed completion body: {e}"))
            })?;
            state.handle_non_stream_body(parsed);
            let s = stream! {
                let mut state = state;
                while let Some(item) = state.pending.pop_front() {
                    yield item;
                }
            }
            .boxed();
            return Ok(s);
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
                    // ADR-55 Phase 2e stream-retry: a transport fault
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

    fn sse(data: &str) -> crate::sse::SseEvent {
        crate::sse::SseEvent {
            event: None,
            data: Some(data.to_string()),
            id: None,
            keepalive: false,
        }
    }

    /// Build an SSE `delta.content` event carrying `content` verbatim as a
    /// JSON *string* (escaping handled by serde), so hand-rolled `\"` /
    /// `\\` escapes are never needed in fixtures.
    fn content_event(content: &str) -> crate::sse::SseEvent {
        sse(&serde_json::json!({"choices": [{"delta": {"content": content}}]}).to_string())
    }

    fn drain(state: &mut OpenAiStreamState) -> Vec<CompletionChunk> {
        state.pending.drain(..).map(|result| result.expect("chunk emitted")).collect()
    }

    /// Assert that an event sequence left the content as verbatim plain text
    /// with ONE data chunk, no synthesized tool call, and a terminal chunk.
    fn assert_plain_text(state: &mut OpenAiStreamState, expected: &str) {
        let chunks = drain(state);
        assert!(chunks.iter().all(|c| c.tool_call.is_none()), "no tool call synthesized");
        let text: Vec<&str> =
            chunks.iter().filter(|c| !c.delta.is_empty()).map(|c| c.delta.as_str()).collect();
        assert_eq!(text, vec![expected], "content delivered verbatim as plain text");
        assert!(chunks.last().unwrap().is_final, "terminal chunk present");
    }

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

    /// Empty accumulated arguments (proxy Fix 3) become `Null` — they never
    /// take the retry chain and never fire the diagnostic `warn!`.
    #[test]
    fn stream_empty_arguments_emit_null_without_warn() {
        let mut state = OpenAiStreamState::new();
        let subscriber = WarnSink::default();
        let sink = subscriber.clone();
        let event = |data: &str| crate::sse::SseEvent {
            event: None,
            data: Some(data.to_string()),
            id: None,
            keepalive: false,
        };

        tracing::subscriber::with_default(subscriber, || {
            state.handle_event(event(
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"shell","arguments":""}}]}}]}"#,
            ));
            state.handle_event(event("[DONE]"));
        });

        let chunks: Vec<CompletionChunk> =
            state.pending.drain(..).map(|result| result.expect("chunk emitted")).collect();
        let tool = chunks.iter().find_map(|c| c.tool_call.as_ref()).expect("tool-call chunk");
        assert_eq!(tool.arguments, serde_json::Value::Null, "empty args emit Null");
        assert_eq!(tool.name, "shell");
        assert_eq!(sink.warns().len(), 0, "empty args must not emit a parse-failure warning");
    }

    /// A raw string delivered as valid JSON (`"ls"`) parses successfully, so
    /// Fix 3's success path returns the parsed value directly (no `{ }`
    /// coercion) and emits no diagnostic.
    #[test]
    fn stream_non_object_string_arguments_pass_through() {
        let mut state = OpenAiStreamState::new();
        let subscriber = WarnSink::default();
        let sink = subscriber.clone();
        let event = |data: &str| crate::sse::SseEvent {
            event: None,
            data: Some(data.to_string()),
            id: None,
            keepalive: false,
        };

        tracing::subscriber::with_default(subscriber, || {
            state.handle_event(event(
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"shell","arguments":"\"ls\""}}]}}]}"#,
            ));
            state.handle_event(event("[DONE]"));
        });

        let chunks: Vec<CompletionChunk> =
            state.pending.drain(..).map(|result| result.expect("chunk emitted")).collect();
        let tool = chunks.iter().find_map(|c| c.tool_call.as_ref()).expect("tool-call chunk");
        assert_eq!(tool.arguments, serde_json::json!("ls"), "valid JSON passes through verbatim");
        assert_eq!(sink.warns().len(), 0, "no warning on a parseable payload");
    }

    /// A well-formed object argument parses directly and passes through unchanged.
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

    /// Proxy flat format (proxy-tool-call-fix doc failure mode #1): the proxy
    /// emits `{id, name, arguments}` directly on the tool-call object with no
    /// `function` wrapper, and the string arguments arrive in incremental
    /// fragments. The flat string fallback accumulates them exactly like the
    /// `function.arguments` path, and `emit_tool_call` still normalizes to a
    /// JSON object.
    #[test]
    fn stream_flat_tool_call_with_string_arguments() {
        let mut state = OpenAiStreamState::new();
        let event = |data: &str| crate::sse::SseEvent {
            event: None,
            data: Some(data.to_string()),
            id: None,
            keepalive: false,
        };

        // First flat fragment: id, name and the argument prefix.
        state.handle_event(event(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","name":"shell","arguments":"{\"command\":"}]}}]}"#,
        ));
        // Second flat fragment: name re-sent, arguments continue.
        state.handle_event(event(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"name":"shell","arguments":"\"ls\"}"}]}}]}"#,
        ));
        state.handle_event(event("[DONE]"));

        let chunks: Vec<CompletionChunk> =
            state.pending.drain(..).map(|result| result.expect("chunk emitted")).collect();
        let tool = chunks.iter().find_map(|c| c.tool_call.as_ref()).expect("tool-call chunk");
        assert_eq!(tool.name, "shell");
        assert_eq!(tool.arguments, serde_json::json!({"command": "ls"}));
        assert!(tool.arguments.is_object(), "arguments must be a JSON object");
    }

    /// Proxy flat format with the complete `arguments` delivered as a JSON
    /// object (one-shot). The object serializes directly and is parsed back to
    /// the same object by `emit_tool_call` — no chunk accumulation involved.
    #[test]
    fn stream_flat_tool_call_with_object_arguments() {
        let mut state = OpenAiStreamState::new();
        let event = |data: &str| crate::sse::SseEvent {
            event: None,
            data: Some(data.to_string()),
            id: None,
            keepalive: false,
        };

        state.handle_event(event(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","name":"shell","arguments":{"command":"ls"}}]}}]}"#,
        ));
        state.handle_event(event("[DONE]"));

        let chunks: Vec<CompletionChunk> =
            state.pending.drain(..).map(|result| result.expect("chunk emitted")).collect();
        let tool = chunks.iter().find_map(|c| c.tool_call.as_ref()).expect("tool-call chunk");
        assert_eq!(tool.name, "shell");
        assert_eq!(tool.arguments, serde_json::json!({"command": "ls"}));
        assert!(tool.arguments.is_object(), "arguments must be a JSON object");
    }

    /// Proxy flat format using the `input` alias instead of `arguments` for the
    /// complete JSON object (one-shot). Recognized by the same flat fallback.
    #[test]
    fn stream_flat_tool_call_with_input_alias() {
        let mut state = OpenAiStreamState::new();
        let event = |data: &str| crate::sse::SseEvent {
            event: None,
            data: Some(data.to_string()),
            id: None,
            keepalive: false,
        };

        state.handle_event(event(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","name":"shell","input":{"command":"ls"}}]}}]}"#,
        ));
        state.handle_event(event("[DONE]"));

        let chunks: Vec<CompletionChunk> =
            state.pending.drain(..).map(|result| result.expect("chunk emitted")).collect();
        let tool = chunks.iter().find_map(|c| c.tool_call.as_ref()).expect("tool-call chunk");
        assert_eq!(tool.name, "shell");
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

    /// A non-streamed completion body (`stream: false`, the weak-model
    /// transport) reduces to the same canonical chunk shapes as a streamed
    /// response: content, ONE whole tool-call arguments object, the
    /// accumulated reasoning, and a terminal chunk carrying `usage`.
    #[test]
    fn non_stream_body_reduces_to_whole_tool_call_and_final() {
        let mut state = OpenAiStreamState::new();
        let body: serde_json::Value = serde_json::from_str(
            r#"{
                "choices": [{
                    "message": {
                        "role": "assistant",
                        "content": "Running the tests.",
                        "reasoning_content": "thinking it through",
                        "tool_calls": [{
                            "id": "call_1",
                            "type": "function",
                            "function": {
                                "name": "shell",
                                "arguments": "{\"command\": \"cargo test\"}"
                            }
                        }]
                    },
                    "finish_reason": "tool_calls"
                }],
                "usage": {"prompt_tokens": 11, "completion_tokens": 5}
            }"#,
        )
        .expect("fixture parses");

        state.handle_non_stream_body(body);

        let chunks: Vec<CompletionChunk> =
            state.pending.drain(..).map(|result| result.expect("chunk emitted")).collect();

        let content = chunks.iter().find(|c| !c.delta.is_empty()).expect("content chunk");
        assert_eq!(content.delta, "Running the tests.");

        let tool = chunks.iter().find_map(|c| c.tool_call.as_ref()).expect("tool-call chunk");
        assert_eq!(tool.id, "call_1");
        assert_eq!(tool.name, "shell");
        assert_eq!(
            tool.arguments,
            serde_json::json!({"command": "cargo test"}),
            "arguments must arrive as ONE whole object, not truncated deltas"
        );

        let reasoning = chunks.iter().find_map(|c| c.reasoning.clone()).expect("reasoning chunk");
        assert_eq!(reasoning, "thinking it through");

        let final_chunk = chunks.iter().find(|c| c.is_final).expect("terminal chunk");
        assert_eq!(
            final_chunk.usage,
            Some(CompletionUsage { prompt_tokens: Some(11), completion_tokens: Some(5) })
        );
    }

    /// On the weak-model tier (`tool_adapted`), whole arguments emitted by a
    /// non-streamed completion still get dot-notation keys re-nested before
    /// leaving the connector — same as on the streamed transport.
    #[test]
    fn non_stream_body_unflattens_dotted_arguments_when_adapted() {
        let mut state = OpenAiStreamState::new();
        state.tool_adapted = true;
        let body: serde_json::Value = serde_json::from_str(
            r#"{
                "choices": [{
                    "message": {
                        "role": "assistant",
                        "tool_calls": [{
                            "id": "call_1",
                            "type": "function",
                            "function": {
                                "name": "runner",
                                "arguments": "{\"config.mode\": \"fast\"}"
                            }
                        }]
                    },
                    "finish_reason": "tool_calls"
                }]
            }"#,
        )
        .expect("fixture parses");

        state.handle_non_stream_body(body);

        let chunks: Vec<CompletionChunk> =
            state.pending.drain(..).map(|result| result.expect("chunk emitted")).collect();
        let tool = chunks.iter().find_map(|c| c.tool_call.as_ref()).expect("tool-call chunk");
        assert_eq!(
            tool.arguments,
            serde_json::json!({"config": {"mode": "fast"}}),
            "dotted arguments must be re-nested on the non-streamed transport too"
        );
    }

    /// A non-streamed body without a `message` (degenerate response) must not
    /// panic or emit phantom content: only the `[DONE]` terminator is
    /// produced.
    #[test]
    fn non_stream_body_without_message_yields_final_only() {
        let mut state = OpenAiStreamState::new();
        state.handle_non_stream_body(serde_json::json!({"choices": []}));
        let chunks: Vec<CompletionChunk> =
            state.pending.drain(..).map(|result| result.expect("chunk emitted")).collect();
        assert_eq!(chunks.len(), 1, "only the final chunk");
        assert!(chunks[0].is_final);
    }

    // ─────────────────────────────────────────────────────────────────────
    // Proxy Fix 2 STRICT — content-embedded tool-call envelope
    // ─────────────────────────────────────────────────────────────────────

    /// Accepted shape (a): the proxy embeds a complete tool call as a JSON
    /// object in the content text. Because the ENTIRE content is exactly the
    /// envelope, it becomes a real tool call and the raw text is NOT echoed as
    /// a separate content chunk (no text+call double representation).
    #[test]
    fn content_envelope_object_arguments_becomes_tool_call() {
        let mut state = OpenAiStreamState::new();
        state.handle_event(content_event(r#"{"name":"shell","arguments":{"command":"ls"}}"#));
        state.handle_event(sse("[DONE]"));

        let chunks = drain(&mut state);
        let tool = chunks.iter().find_map(|c| c.tool_call.as_ref()).expect("tool-call chunk");
        assert_eq!(tool.name, "shell");
        assert_eq!(tool.arguments, serde_json::json!({"command": "ls"}));
        assert!(tool.arguments.is_object());
        assert!(chunks.iter().all(|c| c.delta.is_empty()), "envelope must not be echoed as text");
        assert!(chunks.last().unwrap().is_final);
    }

    /// Accepted shape (b): `arguments` delivered as a JSON *string* flows
    /// through the same `emit_tool_call` parse (Fix 3) as every other
    /// shape.
    #[test]
    fn content_envelope_string_arguments_becomes_tool_call() {
        let mut state = OpenAiStreamState::new();
        state.handle_event(content_event(r#"{"name":"shell","arguments":"{\"command\":\"ls\"}"}"#));
        state.handle_event(sse("[DONE]"));

        let chunks = drain(&mut state);
        let tool = chunks.iter().find_map(|c| c.tool_call.as_ref()).expect("tool-call chunk");
        assert_eq!(tool.name, "shell");
        assert_eq!(tool.arguments, serde_json::json!({"command": "ls"}));
        assert!(tool.arguments.is_object(), "arguments must land as a JSON object");
    }

    /// Accepted shape (c): the `input` alias for arguments (object only),
    /// consistent with the Fix 1 flat path.
    #[test]
    fn content_envelope_input_alias_becomes_tool_call() {
        let mut state = OpenAiStreamState::new();
        state.handle_event(content_event(r#"{"name":"shell","input":{"command":"ls"}}"#));
        state.handle_event(sse("[DONE]"));

        let chunks = drain(&mut state);
        let tool = chunks.iter().find_map(|c| c.tool_call.as_ref()).expect("tool-call chunk");
        assert_eq!(tool.name, "shell");
        assert_eq!(tool.arguments, serde_json::json!({"command": "ls"}));
        assert!(tool.arguments.is_object());
    }

    /// A synthesized envelope still runs through the EXISTING emit pipeline,
    /// so loose-schema un-flattening applies on adapted streams exactly as it
    /// does to real deltas.
    #[test]
    fn content_envelope_unflattens_when_adapted() {
        let mut state = OpenAiStreamState::new();
        state.tool_adapted = true;
        state.handle_event(content_event(
            r#"{"name":"runner","arguments":{"config.mode":"fast","config.retries":2}}"#,
        ));
        state.handle_event(sse("[DONE]"));

        let chunks = drain(&mut state);
        let tool = chunks.iter().find_map(|c| c.tool_call.as_ref()).expect("tool-call chunk");
        assert_eq!(tool.name, "runner");
        assert_eq!(
            tool.arguments,
            serde_json::json!({"config": {"mode": "fast", "retries": 2}}),
            "dotted arguments must be re-nested on adapted streams"
        );
    }

    /// NEG: prose that merely contains a JSON-ish example must stay plain text
    /// — the full-text parse cannot consume the whole payload (no substring
    /// mining).
    #[test]
    fn content_prose_with_json_example_stays_text() {
        let mut state = OpenAiStreamState::new();
        let prose =
            r#"For example, call {"name": "shell", "arguments": {"command": "ls"}} to list files."#;
        state.handle_event(content_event(prose));
        state.handle_event(sse("[DONE]"));
        assert_plain_text(&mut state, prose);
    }

    /// NEG: an envelope followed by trailing prose — the full-text parse
    /// fails, so the whole content stays text.
    #[test]
    fn content_envelope_with_trailing_prose_stays_text() {
        let mut state = OpenAiStreamState::new();
        let content = r#"{"name": "shell", "arguments": {"command": "ls"}} and that's it"#;
        state.handle_event(content_event(content));
        state.handle_event(sse("[DONE]"));
        assert_plain_text(&mut state, content);
    }

    /// NEG: a mixed turn — content text whose entire payload LOOKS like an
    /// envelope PLUS real structured tool deltas. The real deltas win: the
    /// content stays plain text and exactly one tool call (the real one) is
    /// emitted. No envelope synthesis, no double-emit.
    #[test]
    fn content_text_with_real_tool_deltas_prefers_real_deltas() {
        let mut state = OpenAiStreamState::new();
        let envelope_text = r#"{"name":"shell","arguments":{"command":"ls"}}"#;
        state.handle_event(sse(&serde_json::json!({
            "choices": [{"delta": {
                "content": envelope_text,
                "tool_calls": [{"index": 0, "id": "call_1", "type": "function",
                                "function": {"name": "runner", "arguments": ""}}]
            }}]
        })
        .to_string()));
        state.handle_event(sse(&serde_json::json!({
            "choices": [{"delta": {"tool_calls": [
                {"index": 0, "function": {"arguments": r#"{"suite":"demo"}"#}}]}}]
        })
        .to_string()));
        state.handle_event(sse("[DONE]"));

        let chunks = drain(&mut state);
        let text: Vec<&str> =
            chunks.iter().filter(|c| !c.delta.is_empty()).map(|c| c.delta.as_str()).collect();
        assert_eq!(text, vec![envelope_text], "content stays plain text with real tool deltas");
        let calls: Vec<&ToolCall> = chunks.iter().filter_map(|c| c.tool_call.as_ref()).collect();
        assert_eq!(
            calls.len(),
            1,
            "exactly one tool call — the real delta, no synthesized envelope"
        );
        assert_eq!(calls[0].id, "call_1");
        assert_eq!(calls[0].name, "runner");
        assert_eq!(calls[0].arguments, serde_json::json!({"suite": "demo"}));
        assert!(chunks.last().unwrap().is_final);
    }

    /// NEG: a top-level JSON array is not an envelope.
    #[test]
    fn content_array_stays_text() {
        let mut state = OpenAiStreamState::new();
        let content =
            serde_json::json!([{"name": "shell", "arguments": {"command": "ls"}}]).to_string();
        state.handle_event(content_event(&content));
        state.handle_event(sse("[DONE]"));
        assert_plain_text(&mut state, &content);
    }

    /// NEG: an object without a string `name` is not an envelope.
    #[test]
    fn content_object_missing_name_stays_text() {
        let mut state = OpenAiStreamState::new();
        state.handle_event(content_event(r#"{"arguments":{"command":"ls"}}"#));
        state.handle_event(sse("[DONE]"));
        assert_plain_text(&mut state, r#"{"arguments":{"command":"ls"}}"#);
    }

    /// NEG: an object without any arguments/`input` member is not an envelope.
    #[test]
    fn content_object_missing_arguments_stays_text() {
        let mut state = OpenAiStreamState::new();
        state.handle_event(content_event(r#"{"name":"shell"}"#));
        state.handle_event(sse("[DONE]"));
        assert_plain_text(&mut state, r#"{"name":"shell"}"#);
    }

    /// NEG: `arguments` must be Object|String — a number is rejected.
    #[test]
    fn content_wrong_argument_type_stays_text() {
        let mut state = OpenAiStreamState::new();
        state.handle_event(content_event(r#"{"name":"shell","arguments":42}"#));
        state.handle_event(sse("[DONE]"));
        assert_plain_text(&mut state, r#"{"name":"shell","arguments":42}"#);
    }

    /// NEG: `input` mirrors Fix 1 — object only, so a string `input` is
    /// rejected.
    #[test]
    fn content_input_alias_string_stays_text() {
        let mut state = OpenAiStreamState::new();
        state.handle_event(content_event(r#"{"name":"shell","input":"{\"command\":\"ls\"}"}"#));
        state.handle_event(sse("[DONE]"));
        assert_plain_text(&mut state, r#"{"name":"shell","input":"{\"command\":\"ls\"}"}"#);
    }

    // ─────────────────────────────────────────────────────────────────────
    // Proxy Fix 3 — diagnostic logging + retry in `emit_tool_call`
    // ─────────────────────────────────────────────────────────────────────

    /// The Fix 3 diagnostic fields extracted from a captured WARN event:
    /// `tool_name`, `raw_len` (the payload length, never the raw payload) and
    /// `parse_error` (the first strict-parse failure).
    #[derive(Clone, Debug, Default)]
    struct CapturedWarn {
        tool_name: Option<String>,
        raw_len: Option<u64>,
        parse_error: Option<String>,
    }

    #[derive(Default)]
    struct FieldCapture {
        tool_name: Option<String>,
        raw_len: Option<u64>,
        parse_error: Option<String>,
    }

    impl From<FieldCapture> for CapturedWarn {
        fn from(c: FieldCapture) -> Self {
            Self { tool_name: c.tool_name, raw_len: c.raw_len, parse_error: c.parse_error }
        }
    }

    impl tracing::field::Visit for FieldCapture {
        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            match field.name() {
                "tool_name" => self.tool_name = Some(value.to_string()),
                "parse_error" => self.parse_error = Some(value.to_string()),
                _ => {}
            }
        }

        fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
            if field.name() == "raw_len" {
                self.raw_len = Some(value);
            }
        }

        fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
            if field.name() == "raw_len" {
                self.raw_len = Some(value as u64);
            }
        }

        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            // `%field` (Display) values reach `record_debug` Display-wrapped,
            // so their `{:?}` representation is the formatted text.
            let formatted = format!("{value:?}");
            match field.name() {
                "tool_name" => self.tool_name = Some(formatted),
                "parse_error" => self.parse_error = Some(formatted),
                _ => {}
            }
        }
    }

    /// Minimal `tracing::Subscriber` that collects WARN events while a closure
    /// runs under `tracing::subscriber::with_default`, so Fix 3's diagnostic
    /// — and its absence on the empty/valid/recovered paths — can be asserted
    /// without pulling `tracing-subscriber` into dev-dependencies.
    #[derive(Clone, Default)]
    struct WarnSink {
        warns: std::sync::Arc<std::sync::Mutex<Vec<CapturedWarn>>>,
    }

    impl WarnSink {
        fn warns(&self) -> Vec<CapturedWarn> {
            self.warns.lock().expect("warn capture mutex poisoned").clone()
        }
    }

    impl tracing::Subscriber for WarnSink {
        fn enabled(&self, _metadata: &tracing::Metadata<'_>) -> bool {
            true
        }

        fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::Id {
            static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
            tracing::Id::from_u64(NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
        }

        fn record(&self, _span: &tracing::Id, _values: &tracing::span::Record<'_>) {}

        fn record_follows_from(&self, _span: &tracing::Id, _follows: &tracing::Id) {}

        fn event(&self, event: &tracing::Event<'_>) {
            if event.metadata().level() == &tracing::Level::WARN {
                let mut capture = FieldCapture::default();
                event.record(&mut capture);
                self.warns.lock().expect("warn capture mutex poisoned").push(capture.into());
            }
        }

        fn enter(&self, _span: &tracing::Id) {}

        fn exit(&self, _span: &tracing::Id) {}
    }

    /// Reduce one whole tool-call turn — a single `arguments` fragment for
    /// `index 0` / `call_1` / `shell`, then `[DONE]` — under a [`WarnSink`]
    /// subscriber, returning the emitted chunks and the captured warnings.
    fn tool_call_turn(arguments: &str) -> (Vec<CompletionChunk>, WarnSink) {
        let mut state = OpenAiStreamState::new();
        let subscriber = WarnSink::default();
        let sink = subscriber.clone();
        let start = serde_json::json!({
            "choices": [{"delta": {"tool_calls": [
                {"index": 0, "id": "call_1", "type": "function",
                 "function": {"name": "shell", "arguments": arguments}}]}}]
        })
        .to_string();
        tracing::subscriber::with_default(subscriber, || {
            state.handle_event(sse(&start));
            state.handle_event(sse("[DONE]"));
        });
        (drain(&mut state), sink)
    }

    /// Fix 3 — malformed arguments: every parse in the retry chain fails, so
    /// the tool call still emits with `Null` arguments (no panic) and the
    /// `tracing::warn!` fires with `tool_name`, `raw_len` and `parse_error`.
    #[test]
    fn stream_malformed_args_emit_null_and_warn() {
        let payload = "this is not json";
        let (chunks, sink) = tool_call_turn(payload);

        let tool = chunks.iter().find_map(|c| c.tool_call.as_ref()).expect("tool-call chunk");
        assert_eq!(tool.id, "call_1");
        assert_eq!(tool.name, "shell");
        assert_eq!(tool.arguments, serde_json::Value::Null, "unrecoverable args emit Null");
        assert!(chunks.len() >= 2, "tool-call chunk + terminal chunk");
        assert!(chunks.last().unwrap().is_final, "terminal chunk still emitted");

        let warns = sink.warns();
        assert_eq!(warns.len(), 1, "exactly one diagnostic warning");
        assert_eq!(warns[0].tool_name.as_deref(), Some("shell"));
        assert_eq!(warns[0].raw_len, Some(payload.len() as u64), "raw_len, not raw payload");
        assert!(warns[0].parse_error.is_some(), "first strict-parse error attached");
    }

    /// Fix 3 — single-quote fixup recovery: the strict parse fails on
    /// single-quoted JSON, the `'` → `"` fixup rescues it into a real object,
    /// and no warning fires.
    #[test]
    fn stream_single_quote_args_are_recovered() {
        let (chunks, sink) = tool_call_turn("{'command': 'ls'}");

        let tool = chunks.iter().find_map(|c| c.tool_call.as_ref()).expect("tool-call chunk");
        assert_eq!(tool.arguments, serde_json::json!({"command": "ls"}));
        assert!(tool.arguments.is_object());
        assert_eq!(sink.warns().len(), 0, "recovery succeeds silently");
    }

    /// Fix 3 — double-wrap/trim recovery: the payload carries surrounding
    /// whitespace (exercising the trim re-parse) AND single quotes (the fixup
    /// re-parse); the chained retries on the trimmed text recover the object.
    #[test]
    fn stream_double_wrapped_trim_args_are_recovered() {
        let (chunks, sink) = tool_call_turn("  {'command': 'ls'}  ");

        let tool = chunks.iter().find_map(|c| c.tool_call.as_ref()).expect("tool-call chunk");
        assert_eq!(tool.arguments, serde_json::json!({"command": "ls"}));
        assert!(tool.arguments.is_object());
        assert_eq!(sink.warns().len(), 0, "recovery succeeds silently");
    }

    /// NEG — a valid JSON payload passes through byte-for-byte with no warning:
    /// Fix 3 must not perturb the success path.
    #[test]
    fn stream_valid_json_arguments_unchanged() {
        let (chunks, sink) = tool_call_turn(r#"{"command":"ls"}"#);

        let tool = chunks.iter().find_map(|c| c.tool_call.as_ref()).expect("tool-call chunk");
        assert_eq!(tool.arguments, serde_json::json!({"command": "ls"}));
        assert!(tool.arguments.is_object());
        assert_eq!(sink.warns().len(), 0, "no warning on a valid payload");
    }

    /// NEG — legitimate apostrophes inside double-quoted string values are
    /// valid JSON, so the strict parse succeeds and the single-quote fixup
    /// never runs (the apostrophe-corruption risk identified in the spec is
    /// mitigated by construction). The payload is preserved verbatim.
    #[test]
    fn stream_string_values_with_apostrophes_are_preserved() {
        let (chunks, sink) = tool_call_turn(r#"{"message": "it's fine"}"#);

        let tool = chunks.iter().find_map(|c| c.tool_call.as_ref()).expect("tool-call chunk");
        assert_eq!(tool.arguments, serde_json::json!({"message": "it's fine"}));
        assert!(tool.arguments.is_object());
        assert_eq!(sink.warns().len(), 0, "valid JSON with apostrophes is untouched");
    }
}
