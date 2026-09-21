use async_stream::stream;
use async_trait::async_trait;
use concerto_core::error::{describe_error_chain, ProviderError};
use concerto_core::traits::{CompletionStream, LlmProvider};
use concerto_core::types::{
    CompletionChunk, CompletionRequest, CompletionUsage, ModelInfo, TokenBudget, ToolCall,
};
use concerto_core::CancellationToken;
use futures::stream::StreamExt;

use crate::adapters::{Dialect, GeminiChatDialect, ReasoningEcho};
use crate::sse::BufferedSseParser;

pub struct GoogleProvider {
    api_key: String,
    model: String,
    timeout_secs: u64,
    dialect: GeminiChatDialect,
    /// Tool-schema presentation tier (adaptive tool schemas, ADR-66 §4
    /// family). Weak Gemini models stall on strict nested JSON-Schema
    /// parameters, so the loose tier flattens nested properties to
    /// dot-notation leaves, enriches enums, and appends argument examples;
    /// emitted `functionCall.args` are re-nested on the way back. `Auto`
    /// (the default) keeps every non-weak model on the verbatim strict
    /// schema. See `crate::adapters::schema_loose`.
    tool_schema_mode: concerto_config::ToolSchemaMode,
}

impl GoogleProvider {
    pub fn new(api_key: String, model: String, timeout_secs: u64) -> Self {
        Self {
            api_key,
            model,
            timeout_secs,
            dialect: GeminiChatDialect,
            tool_schema_mode: concerto_config::ToolSchemaMode::default(),
        }
    }

    /// Set the tool-schema presentation mode (adaptive tool schemas).
    ///
    /// Applies to the Gemini wire path: when the resolved model matches the
    /// loose tier, tool definitions are rewritten in place before the
    /// dialect renders the body and emitted function-call arguments are
    /// re-nested from dot-notation back into the tools' original nested
    /// shape. Strict models are untouched. See `crate::adapters::schema_loose`.
    pub fn with_tool_schema_mode(mut self, mode: concerto_config::ToolSchemaMode) -> Self {
        self.tool_schema_mode = mode;
        self
    }

    /// Rewrite the request's tool definitions in place when the loose tier
    /// is active for `model`. Returns whether adaptation happened so the
    /// stream parser can re-nest emitted arguments.
    fn adapt_tools_for(&self, request: &mut CompletionRequest, model: &str) -> bool {
        let tool_adapted = crate::adapters::schema_loose::adaptive_tool_schemas_active(
            self.tool_schema_mode,
            model,
        );
        if tool_adapted {
            if let Some(tools) = request.tools.as_mut() {
                crate::adapters::schema_loose::adapt_tool_definitions(tools);
            }
        }
        tool_adapted
    }
}

/// Extract the `args` value of a Gemini `functionCall` part into a canonical
/// tool-call arguments value.
///
/// Gemini accepts arbitrary JSON in `functionCall.args`, but canonical
/// `ToolCall.arguments` feeds OpenAI-compatible serializers downstream that
/// require a JSON object; absent or non-object `args` (e.g. a raw string) are
/// coerced to `{}` so the wire never carries `"null"` / `"\"ls\""`
/// (`HTTP 400: function.arguments must be a JSON object`).
fn function_call_args(fc: &serde_json::Value) -> serde_json::Value {
    crate::protocol::ensure_arguments_object(
        fc.get("args").cloned().unwrap_or(serde_json::Value::Null),
    )
}

/// Read the opaque `thought_signature` from the *part-level sibling* keys of
/// a Gemini `functionCall` part — the confirmed native `generateContent`
/// placement, next to `functionCall` rather than inside it.
///
/// The wire spelling has varied: the API's own error text and early field
/// dumps use snake_case `thought_signature`, while the canonical IR names the
/// same field `thoughtSignature` (ARCHITECTURE-V2 §2.1). Both spellings are
/// accepted so a capture never silently drops a signature.
fn part_thought_signature(part: &serde_json::Value) -> Option<String> {
    ["thought_signature", "thoughtSignature"]
        .into_iter()
        .filter_map(|key| part.get(key))
        .find_map(|value| value.as_str().map(str::to_owned))
}

/// Read the opaque `thought_signature` from *inside* a Gemini `functionCall`
/// object (both spellings).
///
/// Legacy fallback only: native `generateContent` attaches the signature as a
/// part-level sibling (see [`part_thought_signature`]) and rejects the nested
/// spelling on replay, but a capture must never silently drop a signature a
/// model emitted in either shape.
fn fc_thought_signature(fc: &serde_json::Value) -> Option<String> {
    ["thought_signature", "thoughtSignature"]
        .into_iter()
        .filter_map(|key| fc.get(key))
        .find_map(|value| value.as_str().map(str::to_owned))
}

/// Resolve the opaque `thought_signature` for one `functionCall` part.
///
/// Primary location: the part-level sibling keys (both spellings) — the
/// confirmed native `generateContent` placement. The fc-nested keys (both
/// spellings) remain a *fallback* only, so a capture never silently drops a
/// signature emitted in a legacy or third-party shape.
fn function_call_thought_signature(
    fc: &serde_json::Value,
    part: &serde_json::Value,
) -> Option<String> {
    part_thought_signature(part).or_else(|| fc_thought_signature(fc))
}

/// Parse one Gemini `functionCall` part into a canonical [`ToolCall`].
///
/// Gemini provides no unique call IDs, so `counter` mints sequential `gc_<n>`
/// ids for the stream. When the loose tool-schema tier was active for the
/// stream, `unflatten` re-nests dot-notation arguments back into the tools'
/// original nested shape (see [`crate::adapters::schema_loose`]).
///
/// The opaque `thought_signature` the model attached to the part is preserved
/// verbatim (part-level sibling preferred, the fc-nested spelling kept as a
/// legacy fallback, either wire spelling accepted; see
/// [`function_call_thought_signature`]): Google's API requires it to be
/// replayed on the next request that re-sends this function call, or the
/// request 400s (see [`crate::adapters::google`]). Per-part presence is
/// preserved exactly — no signature is backfilled onto sibling parts of the
/// same turn.
fn parse_function_call_part(
    fc: &serde_json::Value,
    part: &serde_json::Value,
    counter: &mut u64,
    unflatten: bool,
) -> ToolCall {
    let name = fc
        .get("name")
        .and_then(|v| v.as_str())
        .map(str::to_owned)
        .unwrap_or_else(|| "unknown".to_owned());
    let mut args = function_call_args(fc);
    if unflatten {
        crate::adapters::schema_loose::unflatten_tool_arguments(&mut args);
    }
    *counter += 1;
    let id = format!("gc_{counter}");
    let thought_signature = function_call_thought_signature(fc, part);
    ToolCall { id, name, arguments: args, thought_signature }
}

/// Streaming state for one Gemini `:streamGenerateContent` response.
///
/// Holds the SSE parser and the accumulated provider-reported usage so the
/// stream reducer is testable without network I/O (mirrors the state structs
/// of the OpenAI/Anthropic connectors).
struct GoogleStreamState {
    parser: BufferedSseParser,
    /// Provider-reported usage captured from the final chunk's
    /// `usageMetadata` (ADR-48 §4); attached to the terminal chunk only.
    usage: Option<CompletionUsage>,
    /// Tool calls parsed from the current assistant turn, held out of the
    /// stream until the turn completes so a multi-call turn is always emitted
    /// as one group before the terminal chunk (and a cancelled or truncated
    /// stream never drops an already-parsed call).
    ///
    /// Gemini attaches the opaque `thought_signature` to exactly the parts the
    /// model signed (usually only the first of a parallel-call turn) and
    /// Google requires *that exact per-part presence* on replay — sibling
    /// parts that arrived bare must stay bare, because forcing a signature
    /// onto them is itself rejectable. Nothing is backfilled here.
    pending_tool_calls: Vec<CompletionChunk>,
}

impl GoogleStreamState {
    fn new() -> Self {
        Self { parser: BufferedSseParser::new(), usage: None, pending_tool_calls: Vec::new() }
    }

    /// Drain the turn's parsed tool calls unchanged.
    ///
    /// Every call keeps its exact captured `thought_signature` presence: the
    /// parts the model signed replay their signature, and sibling parts that
    /// arrived bare stay `None` (fail-open). No within-turn propagation
    /// happens — forcing a signature onto a sibling part is itself rejectable
    /// by Google.
    fn flush_pending_tool_calls(&mut self) -> Vec<Result<CompletionChunk, ProviderError>> {
        self.pending_tool_calls.drain(..).map(Ok).collect()
    }

    /// Capture Gemini's `usageMetadata` (ADR-48 §4).
    ///
    /// Gemini reports input/output tokens in the `usageMetadata` member of
    /// the final streamed chunk (`promptTokenCount` / `candidatesTokenCount`).
    /// Only counts actually present on the wire are recorded — `None` and
    /// `0` are both legitimate reports, so no coalescing happens here. A
    /// metadata object with no token counts is not a measurement and stays
    /// `None`.
    fn capture_usage(&mut self, parsed: &serde_json::Value) {
        let Some(metadata) = parsed.get("usageMetadata") else { return };
        let prompt_tokens = metadata["promptTokenCount"].as_u64();
        let completion_tokens = metadata["candidatesTokenCount"].as_u64();
        if prompt_tokens.is_none() && completion_tokens.is_none() {
            return;
        }
        self.usage = Some(CompletionUsage { prompt_tokens, completion_tokens });
    }

    /// Reduce one SSE event into canonical chunks to yield.
    fn handle_event(
        &mut self,
        event: crate::sse::SseEvent,
        fc_counter: &mut u64,
        tool_adapted: bool,
    ) -> Vec<Result<CompletionChunk, ProviderError>> {
        let mut items = Vec::new();
        if event.keepalive {
            // Liveness signal (SSE comment line): emit an empty chunk so the
            // stream stays active and the orchestrator idle timeout does not
            // fire during long keep-alive-only periods.
            items.push(Ok(CompletionChunk {
                reasoning: None,
                delta: String::new(),
                tool_call: None,
                is_final: false,
                usage: None,
            }));
            return items;
        }
        let Some(data) = event.data else { return items };
        if data == "[DONE]" {
            // End of the turn: drain any parsed tool calls (see
            // [`Self::pending_tool_calls`]) before the terminal chunk so a
            // stream that ended without an explicit finishReason still emits
            // them.
            items.extend(self.flush_pending_tool_calls());
            items.push(Ok(CompletionChunk {
                reasoning: None,
                delta: String::new(),
                tool_call: None,
                is_final: true,
                usage: self.usage.take(),
            }));
            return items;
        }
        let parsed: serde_json::Value = match serde_json::from_str(&data) {
            Ok(v) => v,
            Err(_) => return items,
        };
        self.capture_usage(&parsed);
        if let Some(candidates) = parsed["candidates"].as_array() {
            if let Some(candidate) = candidates.first() {
                if let Some(content) = candidate["content"].as_object() {
                    // `content` is a `serde_json::Map`, whose `Index` impl
                    // panics on a missing key (unlike `Value` indexing, which
                    // yields Null). Gemini may omit "parts" (e.g.
                    // finishReason-only candidates).
                    if let Some(parts) = content.get("parts").and_then(serde_json::Value::as_array)
                    {
                        for part in parts {
                            if let Some(text) = part["text"].as_str() {
                                items.push(Ok(CompletionChunk {
                                    reasoning: None,
                                    delta: text.to_string(),
                                    tool_call: None,
                                    is_final: false,
                                    usage: None,
                                }));
                            }
                            if let Some(fc) = part.get("functionCall") {
                                // Gemini emits function calls inline in
                                // parts. Parse the name, args, and the opaque
                                // `thought_signature` (replayed verbatim by
                                // the dialect) into a ToolCall chunk.
                                // Per-part signature presence is preserved
                                // exactly — no within-turn backfill onto
                                // siblings. The chunk is held until the turn
                                // completes so a multi-call turn is emitted
                                // as one group before the terminal chunk.
                                let tc =
                                    parse_function_call_part(fc, part, fc_counter, tool_adapted);
                                self.pending_tool_calls.push(CompletionChunk {
                                    reasoning: None,
                                    delta: String::new(),
                                    tool_call: Some(tc),
                                    is_final: false,
                                    usage: None,
                                });
                            }
                        }
                    }
                }
                if let Some(finish) = candidate["finishReason"].as_str() {
                    if !finish.is_empty() && finish != "STOP" {
                        // A non-STOP terminal (MAX_TOKENS, SAFETY, ...) closes
                        // the turn and the stream: flush sibling calls, then
                        // the terminal chunk with usage.
                        items.extend(self.flush_pending_tool_calls());
                        items.push(Ok(CompletionChunk {
                            reasoning: None,
                            delta: String::new(),
                            tool_call: None,
                            is_final: true,
                            usage: self.usage.take(),
                        }));
                    } else if !finish.is_empty() {
                        // Plain STOP closes the assistant turn; the [DONE]
                        // sentinel still delivers the terminal usage chunk.
                        // Flush the parsed calls now so a clean STOP (or a
                        // truncated stream before [DONE]) never drops them.
                        items.extend(self.flush_pending_tool_calls());
                    }
                }
            }
        }
        items
    }
}

#[async_trait]
impl LlmProvider for GoogleProvider {
    async fn test_connection(&self, _cancel: CancellationToken) -> Result<(), ProviderError> {
        let client = crate::new_client(self.timeout_secs);
        let url =
            format!("https://generativelanguage.googleapis.com/v1beta/models?key={}", self.api_key);
        let resp = client.get(&url).send().await.map_err(|e| {
            ProviderError::Other(format!("google connection failed: {}", describe_error_chain(&e)))
        })?;
        if resp.status().is_success() {
            Ok(())
        } else if resp.status().as_u16() == 401 || resp.status().as_u16() == 403 {
            Err(ProviderError::AuthFailure)
        } else {
            Err(ProviderError::Other(format!("google returned {}", resp.status())))
        }
    }

    async fn list_models(
        &self,
        _cancel: CancellationToken,
    ) -> Result<Vec<ModelInfo>, ProviderError> {
        let client = crate::new_client(self.timeout_secs);
        let url =
            format!("https://generativelanguage.googleapis.com/v1beta/models?key={}", self.api_key);
        let resp = client.get(&url).send().await.map_err(|e| {
            ProviderError::Other(format!("google list_models failed: {}", describe_error_chain(&e)))
        })?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(ProviderError::Other(format!(
                "google list_models returned {status}: {text}"
            )));
        }

        let json: serde_json::Value = resp.json().await.map_err(|e| {
            ProviderError::Other(format!(
                "google list_models parse failed: {}",
                describe_error_chain(&e)
            ))
        })?;

        let models = json["models"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| {
                        let full_name = v["name"].as_str()?;
                        // Strip "models/" prefix for the model ID
                        let id = full_name.strip_prefix("models/").unwrap_or(full_name).to_string();
                        let name = v["displayName"].as_str().map(String::from);
                        let owned_by = Some("google".to_string());
                        Some(ModelInfo { id, name, owned_by, supports_tool_calling: None })
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
            provider = "google",
            model = %request.model,
        );
        let _guard = span.enter();

        let client = crate::new_client(self.timeout_secs);
        let model =
            if request.model.is_empty() { self.model.clone() } else { request.model.clone() };
        let url = format!(
            "https://generativelanguage.googleapis.com/v1beta/models/{}:streamGenerateContent?alt=sse&key={}",
            model, self.api_key
        );

        // Adaptive tool schemas (weak-model tier, ADR-66 §4 family): when
        // the resolved model matches the loose tier, rewrite the request's
        // tool definitions in place before the dialect renders the body.
        // Strict models are untouched — their wire output stays
        // byte-identical.
        let mut request = request;
        let tool_adapted = self.adapt_tools_for(&mut request, &model);

        // Request-body rendering now lives in `crate::adapters::google`
        // (`GeminiChatDialect`); stream parsing remains here in the connector.
        let body = self.dialect.render_chat_body(&request, &model, ReasoningEcho::IfPresent);

        // Clone cancel token for use inside the stream later
        let cancel = cancel.clone();
        let response = tokio::select! {
            _ = cancel.cancelled() => return Err(ProviderError::Cancelled),
            result = async {
                let r = client
                    .post(&url)
                    .header("Content-Type", "application/json")
                    .json(&body)
                    .send()
                    .await
                    .map_err(|e| ProviderError::Network(format!("request failed: {}", describe_error_chain(&e))))?;

                if !r.status().is_success() {
                    let status = r.status();
                    // Extract optional retry-after / retry-after-ms header
                    let retry_after = crate::retry::parse_retry_after(r.headers());
                    let text = r.text().await.unwrap_or_default();
                    return Err(crate::retry::map_http_error(status, &text, retry_after));
                }
                Ok(r)
            } => {
                result?
            }
        };

        // Gemini does not provide unique call IDs for function calls, so we
        // generate sequential IDs within a stream.
        let fc_counter: u64 = 0;

        let s = stream! {
            let mut state = GoogleStreamState::new();
            let mut fc_counter = fc_counter;
            let mut byte_stream = response.bytes_stream();
            while let Some(chunk) = byte_stream.next().await {
                // Check for cancellation before processing the chunk
                if cancel.is_cancelled() {
                    // A mid-turn cancel must not drop already-parsed calls:
                    // flush the pending turn (per-part signatures preserved,
                    // possibly all `None` — fail-open).
                    for item in state.flush_pending_tool_calls() {
                        yield item;
                    }
                    break;
                }
                let items = match chunk {
                    Ok(bytes) => {
                        let events = state.parser.push_bytes(&bytes);
                        let mut items = Vec::new();
                        for event in events {
                            items.extend(state.handle_event(event, &mut fc_counter, tool_adapted));
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
            // The byte stream ended without an explicit terminal/[DONE]:
            // flush any pending tool calls so a truncated stream never drops
            // a parsed turn (already flushed by a finishReason — a no-op).
            for item in state.flush_pending_tool_calls() {
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
        // Gemini pricing per 1M tokens (input / output, USD).
        // https://ai.google.dev/pricing
        let (in_rate, out_rate) = if self.model.contains("2.0-flash") {
            (0.10, 0.40)
        } else if self.model.contains("2.0") {
            (1.00, 2.00)
        } else if self.model.contains("1.5-pro") {
            (3.50, 10.50)
        } else if self.model.contains("1.5-flash") {
            (0.35, 1.05)
        } else if self.model.contains("1.0-pro") {
            (0.50, 1.50)
        } else {
            // Conservative default for unknown Gemini models.
            (1.00, 2.00)
        };
        (tokens_in as f64 / 1_000_000.0) * in_rate + (tokens_out as f64 / 1_000_000.0) * out_rate
    }

    fn provider_name(&self) -> &'static str {
        "google"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use concerto_core::types::ToolDefinition;

    #[test]
    fn google_provider_new_sets_fields() {
        let p = GoogleProvider::new("test-key".into(), "gemini-2.0-flash".into(), 30);
        assert_eq!(p.api_key, "test-key");
        assert_eq!(p.model, "gemini-2.0-flash");
        assert_eq!(p.timeout_secs, 30);
    }

    #[test]
    fn google_provider_name() {
        let p = GoogleProvider::new("k".into(), "m".into(), 30);
        assert_eq!(p.provider_name(), "google");
    }

    #[test]
    fn google_context_capacity_returns_budget() {
        let p = GoogleProvider::new("k".into(), "gemini-1.5-pro".into(), 30);
        let budget = p.context_capacity("gemini-1.5-pro");
        assert!(budget.capacity > 0);
    }

    #[test]
    fn google_approximate_cost_flash() {
        let p = GoogleProvider::new("k".into(), "gemini-2.0-flash".into(), 30);
        // 1M input, 1M output tokens
        let cost = p.approximate_cost(1_000_000, 1_000_000);
        // 0.10 + 0.40 = 0.50
        assert!((cost - 0.50).abs() < 0.001);
    }

    #[test]
    fn google_approximate_cost_1_5_pro() {
        let p = GoogleProvider::new("k".into(), "gemini-1.5-pro".into(), 30);
        let cost = p.approximate_cost(1_000_000, 1_000_000);
        // 3.50 + 10.50 = 14.00
        assert!((cost - 14.00).abs() < 0.001);
    }

    #[test]
    fn google_approximate_cost_1_5_flash() {
        let p = GoogleProvider::new("k".into(), "gemini-1.5-flash".into(), 30);
        let cost = p.approximate_cost(1_000_000, 1_000_000);
        // 0.35 + 1.05 = 1.40
        assert!((cost - 1.40).abs() < 0.001);
    }

    #[test]
    fn google_approximate_cost_1_0_pro() {
        let p = GoogleProvider::new("k".into(), "gemini-1.0-pro".into(), 30);
        let cost = p.approximate_cost(1_000_000, 1_000_000);
        // 0.50 + 1.50 = 2.00
        assert!((cost - 2.00).abs() < 0.001);
    }

    #[test]
    fn google_approximate_cost_unknown_model_uses_default() {
        let p = GoogleProvider::new("k".into(), "gemini-unknown-model".into(), 30);
        let cost = p.approximate_cost(1_000_000, 1_000_000);
        // default: 1.00 + 2.00 = 3.00
        assert!((cost - 3.00).abs() < 0.001);
    }

    #[test]
    fn google_approximate_cost_zero_tokens() {
        let p = GoogleProvider::new("k".into(), "gemini-2.0-flash".into(), 30);
        let cost = p.approximate_cost(0, 0);
        assert!((cost - 0.0).abs() < 0.0001);
    }

    #[test]
    fn google_context_capacity_uses_model_name() {
        let p = GoogleProvider::new("k".into(), "gemini-1.5-pro".into(), 30);
        let budget = p.context_capacity("unknown-model");
        // Should fall back to default budget of 4000
        assert!(budget.capacity > 0);
    }

    /// Gemini `functionCall.args` reduces to a canonical JSON object: a
    /// non-object (raw string) or absent `args` coerces to `{}` so it never
    /// reaches an OpenAI-compatible wire as the string `"null"` / `"\"ls\""`;
    /// a well-formed object passes through unchanged.
    #[test]
    fn function_call_args_enforce_object_on_canonical_side() {
        // Raw string arg (`"not-an-object"` upstream) -> `{}`.
        let fc = serde_json::json!({"name": "shell", "args": "not-an-object"});
        assert!(function_call_args(&fc).is_object(), "non-object args must coerce to an object");
        assert_eq!(function_call_args(&fc), serde_json::json!({}));

        // Absent `args` -> `{}` (never `Value::Null`).
        let fc = serde_json::json!({"name": "shell"});
        assert_eq!(function_call_args(&fc), serde_json::json!({}));
        assert!(function_call_args(&fc).is_object(), "absent args must coerce to an object");

        // Well-formed object args -> unchanged.
        let fc = serde_json::json!({"name": "shell", "args": {"command": "ls"}});
        assert_eq!(function_call_args(&fc), serde_json::json!({"command": "ls"}));
    }

    /// Gemini 3.x `functionCall` parts carry an opaque `thoughtSignature` — the
    /// confirmed camel wire spelling, placed as a *part-level sibling* of
    /// `functionCall`; the snake `thought_signature` variant from early dumps
    /// and the fc-nested legacy shapes must keep parsing too. Whatever shape
    /// the model emitted is preserved for replay on the next request; a part
    /// with no signature anywhere parses to `None` (fail-open, the legacy
    /// single-call wire shape).
    #[test]
    fn function_call_part_preserves_thought_signature() {
        let mut counter = 0u64;
        // Part-level camel sibling — the confirmed native placement.
        let part = serde_json::json!({
            "functionCall": {"name": "shell", "args": {"command": "ls"}},
            "thoughtSignature": "sig-9f2a"
        });
        let tc = parse_function_call_part(&part["functionCall"], &part, &mut counter, false);
        assert_eq!(tc.name, "shell");
        assert_eq!(tc.arguments, serde_json::json!({"command": "ls"}));
        assert_eq!(tc.thought_signature.as_deref(), Some("sig-9f2a"));
        assert_eq!(tc.id, "gc_1", "sequential stream id minted once");

        // The legacy snake spelling inside `functionCall` must still parse
        // verbatim (the API error text and early field dumps use it) — a
        // capture never silently drops a signature just because its key
        // spelling and location differed.
        let legacy = serde_json::json!({"name": "shell", "args": {"command": "ls"}, "thought_signature": "sig-legacy"});
        let part = serde_json::json!({"functionCall": legacy});
        let tc = parse_function_call_part(&part["functionCall"], &part, &mut counter, false);
        assert_eq!(
            tc.thought_signature.as_deref(),
            Some("sig-legacy"),
            "fc-nested snake fallback still parses"
        );

        // A part without a signature parses to `None` — no replay key emitted.
        let part =
            serde_json::json!({"functionCall": {"name": "shell", "args": {"command": "ls"}}});
        let tc = parse_function_call_part(&part["functionCall"], &part, &mut counter, false);
        assert_eq!(tc.thought_signature, None);
        assert_eq!(tc.id, "gc_3", "counter advances across parts");
    }

    /// The capture 2x2 matrix must not depend on the field's wire spelling or
    /// location. Native `generateContent` places the signature as a
    /// *part-level sibling* of `functionCall` (primary, both spellings — the
    /// confirmed camel `thoughtSignature` and the snake `thought_signature`
    /// from early dumps); the fc-nested location (both spellings) remains a
    /// legacy fallback. A part-level sibling wins over a conflicting
    /// fc-level value; absent everywhere resolves to `None` (fail-open).
    #[test]
    fn thought_signature_is_read_in_either_spelling_and_location() {
        let mut counter = 0u64;

        // Part-level sibling, camelCase — the confirmed native placement.
        let part = serde_json::json!({
            "functionCall": {"name": "shell", "args": {"command": "ls"}},
            "thoughtSignature": "sib-camel"
        });
        let tc = parse_function_call_part(&part["functionCall"], &part, &mut counter, false);
        assert_eq!(tc.thought_signature.as_deref(), Some("sib-camel"));

        // Part-level sibling, snake_case (early dumps).
        let part = serde_json::json!({
            "functionCall": {"name": "shell", "args": {"command": "ls"}},
            "thought_signature": "sib-snake"
        });
        let tc = parse_function_call_part(&part["functionCall"], &part, &mut counter, false);
        assert_eq!(tc.thought_signature.as_deref(), Some("sib-snake"));

        // fc-nested camel — legacy fallback.
        let fc = serde_json::json!({"name": "shell", "args": {"command": "ls"}, "thoughtSignature": "fc-camel"});
        let part = serde_json::json!({"functionCall": fc});
        let tc = parse_function_call_part(&part["functionCall"], &part, &mut counter, false);
        assert_eq!(tc.thought_signature.as_deref(), Some("fc-camel"));

        // fc-nested snake — legacy fallback.
        let fc = serde_json::json!({"name": "shell", "args": {"command": "ls"}, "thought_signature": "fc-snake"});
        let part = serde_json::json!({"functionCall": fc});
        let tc = parse_function_call_part(&part["functionCall"], &part, &mut counter, false);
        assert_eq!(tc.thought_signature.as_deref(), Some("fc-snake"));

        // The part-level sibling value wins over a conflicting fc-level value.
        let part = serde_json::json!({
            "functionCall": {"name": "shell", "args": {}, "thought_signature": "fc-snake"},
            "thoughtSignature": "sib-camel"
        });
        assert_eq!(
            function_call_thought_signature(&part["functionCall"], &part).as_deref(),
            Some("sib-camel")
        );

        // Absent everywhere -> `None` (fail-open, the legacy single-call shape).
        let part = serde_json::json!({"functionCall": {"name": "shell", "args": {}}});
        assert_eq!(function_call_thought_signature(&part["functionCall"], &part), None);
    }

    /// A parallel-call turn usually carries the opaque `thoughtSignature` on the
    /// first `functionCall` part only; the parts that arrived bare must stay
    /// bare. Native `generateContent` rejects a *forged* signature on a
    /// sibling as readily as it rejects a missing one, so the stream preserves
    /// per-part presence exactly — no within-turn propagation.
    #[test]
    fn stream_preserves_per_part_signature_presence() {
        let mut state = GoogleStreamState::new();
        let mut fc_counter = 0u64;
        let event = |data: &str| crate::sse::SseEvent {
            event: None,
            data: Some(data.to_string()),
            id: None,
            keepalive: false,
        };

        let payload = serde_json::json!({
            "candidates": [{
                "content": {
                    "parts": [
                        {"functionCall": {"name": "shell", "args": {"command": "ls"}}, "thoughtSignature": "sig-9f2a"},
                        {"functionCall": {"name": "read_file", "args": {"path": "Cargo.toml"}}},
                        {"functionCall": {"name": "grep", "args": {"pattern": "fn"}}}
                    ]
                },
                "finishReason": "STOP"
            }]
        });
        let chunks: Vec<CompletionChunk> = state
            .handle_event(event(&payload.to_string()), &mut fc_counter, false)
            .into_iter()
            .map(|r| r.expect("chunk emitted"))
            .collect();

        let calls: Vec<&ToolCall> = chunks.iter().filter_map(|c| c.tool_call.as_ref()).collect();
        assert_eq!(calls.len(), 3, "all three parts parse to tool calls");
        assert_eq!(calls[0].id, "gc_1", "sequential per-part ids minted in order");
        assert_eq!(calls[2].id, "gc_3");
        assert_eq!(
            calls[0].thought_signature.as_deref(),
            Some("sig-9f2a"),
            "the part the model signed keeps its signature: {calls:?}"
        );
        assert_eq!(
            calls[1].thought_signature, None,
            "a sibling that arrived bare must stay None, never backfilled: {calls:?}"
        );
        assert_eq!(
            calls[2].thought_signature, None,
            "a sibling that arrived bare must stay None, never backfilled: {calls:?}"
        );
    }

    /// The known signature from an earlier part of the turn is *not* carried
    /// forward to a sibling part streamed in a later event: per-part presence
    /// is preserved verbatim, so the bare sibling stays `None` on replay.
    /// (Delayed emission until the turn closes is retained only so a
    /// multi-call turn is emitted as one group before the terminal chunk.)
    #[test]
    fn stream_does_not_propagate_signature_across_events_within_turn() {
        let mut state = GoogleStreamState::new();
        let mut fc_counter = 0u64;
        let event = |data: &str| crate::sse::SseEvent {
            event: None,
            data: Some(data.to_string()),
            id: None,
            keepalive: false,
        };

        let first = serde_json::json!({
            "candidates": [{
                "content": {
                    "parts": [
                        {"functionCall": {"name": "shell", "args": {"command": "ls"}}, "thoughtSignature": "sig-9f2a"}
                    ]
                }
            }]
        });
        let mut chunks: Vec<CompletionChunk> = state
            .handle_event(event(&first.to_string()), &mut fc_counter, false)
            .into_iter()
            .map(|r| r.expect("chunk emitted"))
            .collect();

        let second = serde_json::json!({
            "candidates": [{
                "content": {
                    "parts": [
                        {"functionCall": {"name": "grep", "args": {"pattern": "fn"}}}
                    ]
                },
                "finishReason": "STOP"
            }]
        });
        chunks.extend(
            state
                .handle_event(event(&second.to_string()), &mut fc_counter, false)
                .into_iter()
                .map(|r| r.expect("chunk emitted")),
        );

        let calls: Vec<&ToolCall> = chunks.iter().filter_map(|c| c.tool_call.as_ref()).collect();
        assert_eq!(calls.len(), 2, "one chunk per part, emitted at turn close");
        assert_eq!(calls[0].id, "gc_1");
        assert_eq!(calls[1].id, "gc_2");
        assert_eq!(
            calls[0].thought_signature.as_deref(),
            Some("sig-9f2a"),
            "the signed part keeps its signature: {calls:?}"
        );
        assert_eq!(
            calls[1].thought_signature, None,
            "a sibling emitted later in the same turn must not inherit the signature: {calls:?}"
        );
    }

    /// A turn whose parts carry no signature anywhere emits its calls with
    /// `None` unchanged (fail-open, the legacy single-call wire shape) — no
    /// signature is ever fabricated just because a turn happened to carry
    /// multiple calls.
    #[test]
    fn stream_without_signature_emits_none_sibling_calls() {
        let mut state = GoogleStreamState::new();
        let mut fc_counter = 0u64;
        let event = |data: &str| crate::sse::SseEvent {
            event: None,
            data: Some(data.to_string()),
            id: None,
            keepalive: false,
        };

        let payload = serde_json::json!({
            "candidates": [{
                "content": {
                    "parts": [
                        {"functionCall": {"name": "shell", "args": {"command": "ls"}}},
                        {"functionCall": {"name": "grep", "args": {"pattern": "fn"}}}
                    ]
                },
                "finishReason": "STOP"
            }]
        });
        let chunks: Vec<CompletionChunk> = state
            .handle_event(event(&payload.to_string()), &mut fc_counter, false)
            .into_iter()
            .map(|r| r.expect("chunk emitted"))
            .collect();

        let calls: Vec<&ToolCall> = chunks.iter().filter_map(|c| c.tool_call.as_ref()).collect();
        assert_eq!(calls.len(), 2);
        for call in &calls {
            assert_eq!(
                call.thought_signature, None,
                "a turn with no signature anywhere must stay None, never fabricated: {call:?}"
            );
        }
    }

    /// A signature carried as a part-level sibling key (next to
    /// `functionCall`, not inside it) is captured off the stream and carried
    /// on the emitted tool call. Both wire spellings are accepted — snake_case
    /// (early dumps) and the confirmed camel `thoughtSignature`.
    #[test]
    fn stream_captures_part_level_sibling_signature() {
        let mut state = GoogleStreamState::new();
        let mut fc_counter = 0u64;
        let event = |data: &str| crate::sse::SseEvent {
            event: None,
            data: Some(data.to_string()),
            id: None,
            keepalive: false,
        };

        // snake_case sibling.
        let payload = serde_json::json!({
            "candidates": [{
                "content": {
                    "parts": [
                        {"functionCall": {"name": "shell", "args": {}}, "thought_signature": "sib-snake"}
                    ]
                },
                "finishReason": "STOP"
            }]
        });
        let chunks: Vec<CompletionChunk> = state
            .handle_event(event(&payload.to_string()), &mut fc_counter, false)
            .into_iter()
            .map(|r| r.expect("chunk emitted"))
            .collect();
        let calls: Vec<&ToolCall> = chunks.iter().filter_map(|c| c.tool_call.as_ref()).collect();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].thought_signature.as_deref(), Some("sib-snake"));

        // camelCase sibling — the confirmed wire spelling.
        let payload = serde_json::json!({
            "candidates": [{
                "content": {
                    "parts": [
                        {"functionCall": {"name": "read_file", "args": {}}, "thoughtSignature": "sib-camel"}
                    ]
                },
                "finishReason": "STOP"
            }]
        });
        let chunks: Vec<CompletionChunk> = state
            .handle_event(event(&payload.to_string()), &mut fc_counter, false)
            .into_iter()
            .map(|r| r.expect("chunk emitted"))
            .collect();
        let calls: Vec<&ToolCall> = chunks.iter().filter_map(|c| c.tool_call.as_ref()).collect();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].thought_signature.as_deref(), Some("sib-camel"));
    }

    /// ADR-66 §4 family: the loose tier flattens nested properties for the
    /// active tier and the adapted arguments re-nest into the tools'
    /// original nested shape. (Note: every `gemini-*` name contains the
    /// weak-tier "mini" hint — "ge**mini**" — so under `Auto` all Gemini
    /// models adapt; the explicit dials pin the tier for this test.)
    #[test]
    fn google_loose_schema_tier_resolves_and_round_trips() {
        let strict_provider = GoogleProvider::new("k".into(), "gemini-2.0-flash".into(), 30)
            .with_tool_schema_mode(concerto_config::ToolSchemaMode::Strict);
        let loose_provider = GoogleProvider::new("k".into(), "gemini-2.0-flash".into(), 30)
            .with_tool_schema_mode(concerto_config::ToolSchemaMode::Loose);

        let mut request = CompletionRequest {
            tools: Some(vec![ToolDefinition {
                name: "runner".into(),
                description: String::new(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "config": {
                            "type": "object",
                            "properties": {"mode": {"type": "string"}},
                            "required": ["mode"]
                        }
                    },
                    "required": ["config"]
                }),
            }]),
            ..Default::default()
        };

        // Strict dial: request untouched.
        assert!(!strict_provider.adapt_tools_for(&mut request, "gemini-2.0-flash"));
        assert!(
            request
                .tools
                .as_ref()
                .and_then(|t| t.first())
                .is_some_and(|tool| tool.parameters["properties"].get("config").is_some()),
            "strict model keeps the nested schema"
        );

        // Loose tier: flattens the nested property.
        assert!(loose_provider.adapt_tools_for(&mut request, "gemini-2.0-flash"));
        let tool = request.tools.as_ref().and_then(|t| t.first()).expect("tool present");
        assert!(
            tool.parameters["properties"].get("config.mode").is_some(),
            "loose tier flattens nested properties: {}",
            tool.parameters
        );

        // The wire round trip: dotted arguments emitted by the weak model
        // re-nest into the original nested shape.
        let fc = serde_json::json!({"name": "runner", "args": {"config.mode": "fast"}});
        let mut args = function_call_args(&fc);
        crate::adapters::schema_loose::unflatten_tool_arguments(&mut args);
        assert_eq!(args, serde_json::json!({"config": {"mode": "fast"}}));
    }

    /// Explicit `Strict` dials win over the weak-model name heuristic.
    #[test]
    fn google_strict_dial_disables_adaptation() {
        let strict = GoogleProvider::new("k".into(), "gemini-2.0-flash".into(), 30)
            .with_tool_schema_mode(concerto_config::ToolSchemaMode::Strict);
        let loose = GoogleProvider::new("k".into(), "gemini-2.0-flash".into(), 30)
            .with_tool_schema_mode(concerto_config::ToolSchemaMode::Loose);
        let mut request = CompletionRequest {
            tools: Some(vec![ToolDefinition {
                name: "t".into(),
                description: String::new(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "opts": {"type": "object", "properties": {"x": {"type": "string"}}}
                    }
                }),
            }]),
            ..Default::default()
        };
        assert!(!strict.adapt_tools_for(&mut request, "mimo-v2.5-free"));
        assert!(loose.adapt_tools_for(&mut request, "gemini-2.0-flash"));
    }

    /// ADR-48 §4: Gemini's `usageMetadata` (`promptTokenCount` /
    /// `candidatesTokenCount`, reported on the final streamed chunk) is
    /// captured and attached to the terminal chunk only; intermediate chunks
    /// carry no usage.
    #[test]
    fn stream_captures_usage_on_final_chunk() {
        let mut state = GoogleStreamState::new();
        let mut fc_counter = 0u64;
        let event = |data: &str| crate::sse::SseEvent {
            event: None,
            data: Some(data.to_string()),
            id: None,
            keepalive: false,
        };

        let mut chunks: Vec<CompletionChunk> = Vec::new();
        // Content delta (no usage on intermediate chunks).
        chunks.extend(
            state
                .handle_event(
                    event(r#"{"candidates":[{"content":{"parts":[{"text":"hello"}]}}]}"#),
                    &mut fc_counter,
                    false,
                )
                .into_iter()
                .map(|r| r.expect("chunk emitted")),
        );
        // The final data chunk carries usageMetadata; Gemini ends the stream
        // with finishReason "STOP" and then the [DONE] sentinel.
        chunks.extend(
            state
                .handle_event(
                    event(r#"{"candidates":[{"content":{"parts":[{"text":" world"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":11,"candidatesTokenCount":5}}"#),
                    &mut fc_counter,
                    false,
                )
                .into_iter()
                .map(|r| r.expect("chunk emitted")),
        );
        chunks.extend(
            state
                .handle_event(event("[DONE]"), &mut fc_counter, false)
                .into_iter()
                .map(|r| r.expect("chunk emitted")),
        );

        assert!(
            chunks[..chunks.len() - 1].iter().all(|chunk| !chunk.is_final),
            "only the final chunk is terminal"
        );
        assert_eq!(chunks[0].usage, None, "content deltas carry no usage");
        let terminal = chunks.last().expect("terminal chunk");
        assert!(terminal.is_final);
        assert_eq!(
            terminal.usage,
            Some(CompletionUsage { prompt_tokens: Some(11), completion_tokens: Some(5) })
        );
        crate::testing::assert_terminal_usage_contract(&chunks);
    }

    /// ADR-48 §4: `finishReason` terminals other than `STOP` (e.g.
    /// `MAX_TOKENS` / `SAFETY`) carry the usage captured from the same
    /// chunk's `usageMetadata`.
    #[test]
    fn stream_captures_usage_on_non_stop_finish() {
        let mut state = GoogleStreamState::new();
        let mut fc_counter = 0u64;
        let event = |data: &str| crate::sse::SseEvent {
            event: None,
            data: Some(data.to_string()),
            id: None,
            keepalive: false,
        };

        let chunks: Vec<CompletionChunk> = state
            .handle_event(
                event(r#"{"candidates":[{"finishReason":"MAX_TOKENS"}],"usageMetadata":{"promptTokenCount":9,"candidatesTokenCount":3}}"#),
                &mut fc_counter,
                false,
            )
            .into_iter()
            .map(|r| r.expect("chunk emitted"))
            .collect();
        assert_eq!(chunks.len(), 1);
        let terminal = chunks.last().expect("terminal chunk");
        assert!(terminal.is_final);
        assert_eq!(
            terminal.usage,
            Some(CompletionUsage { prompt_tokens: Some(9), completion_tokens: Some(3) })
        );
        crate::testing::assert_terminal_usage_contract(&chunks);
    }

    /// ADR-48 §4: a `usageMetadata` object with no token counts is not a
    /// measurement and must not be surfaced as one (mirrors the OpenAI /
    /// Anthropic capture rule).
    #[test]
    fn stream_ignores_usage_without_counts() {
        let mut state = GoogleStreamState::new();
        let mut fc_counter = 0u64;
        let event = |data: &str| crate::sse::SseEvent {
            event: None,
            data: Some(data.to_string()),
            id: None,
            keepalive: false,
        };

        let mut chunks: Vec<CompletionChunk> = Vec::new();
        chunks.extend(
            state
                .handle_event(
                    event(r#"{"candidates":[],"usageMetadata":{}}"#),
                    &mut fc_counter,
                    false,
                )
                .into_iter()
                .map(|r| r.expect("chunk emitted")),
        );
        chunks.extend(
            state
                .handle_event(event("[DONE]"), &mut fc_counter, false)
                .into_iter()
                .map(|r| r.expect("chunk emitted")),
        );
        let terminal = chunks.last().expect("terminal chunk");
        assert!(terminal.is_final);
        assert_eq!(terminal.usage, None, "counts-less usageMetadata must stay None");
        crate::testing::assert_terminal_usage_contract(&chunks);
    }
}
