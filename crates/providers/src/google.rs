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

/// Parse one Gemini `functionCall` part into a canonical [`ToolCall`].
///
/// Gemini provides no unique call IDs, so `counter` mints sequential `gc_<n>`
/// ids for the stream. When the loose tool-schema tier was active for the
/// stream, `unflatten` re-nests dot-notation arguments back into the tools'
/// original nested shape (see [`crate::adapters::schema_loose`]).
///
/// The opaque `thought_signature` the model attached to the part is preserved
/// verbatim: Google's API requires it to be replayed on the next request that
/// re-sends this function call, or the request 400s (see
/// [`crate::adapters::google`]).
fn parse_function_call_part(
    fc: &serde_json::Value,
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
    let thought_signature = fc.get("thought_signature").and_then(|v| v.as_str()).map(str::to_owned);
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
}

impl GoogleStreamState {
    fn new() -> Self {
        Self { parser: BufferedSseParser::new(), usage: None }
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
                                let tc = parse_function_call_part(fc, fc_counter, tool_adapted);
                                items.push(Ok(CompletionChunk {
                                    reasoning: None,
                                    delta: String::new(),
                                    tool_call: Some(tc),
                                    is_final: false,
                                    usage: None,
                                }));
                            }
                        }
                    }
                }
                if let Some(finish) = candidate["finishReason"].as_str() {
                    if !finish.is_empty() && finish != "STOP" {
                        items.push(Ok(CompletionChunk {
                            reasoning: None,
                            delta: String::new(),
                            tool_call: None,
                            is_final: true,
                            usage: self.usage.take(),
                        }));
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

    /// Gemini 3.x `functionCall` parts carry an opaque `thought_signature`
    /// that must be preserved for replay on the next request; the parsed
    /// canonical tool call keeps it (and leaves it `None` when absent).
    #[test]
    fn function_call_part_preserves_thought_signature() {
        let mut counter = 0u64;
        let fc = serde_json::json!({"name": "shell", "args": {"command": "ls"}, "thought_signature": "sig-9f2a"});
        let tc = parse_function_call_part(&fc, &mut counter, false);
        assert_eq!(tc.name, "shell");
        assert_eq!(tc.arguments, serde_json::json!({"command": "ls"}));
        assert_eq!(tc.thought_signature.as_deref(), Some("sig-9f2a"));
        assert_eq!(tc.id, "gc_1", "sequential stream id minted once");

        // A part without a signature parses to `None` — no replay key emitted.
        let plain = serde_json::json!({"name": "shell", "args": {"command": "ls"}});
        let tc = parse_function_call_part(&plain, &mut counter, false);
        assert_eq!(tc.thought_signature, None);
        assert_eq!(tc.id, "gc_2", "counter advances across parts");
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
