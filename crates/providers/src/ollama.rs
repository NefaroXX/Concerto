use async_stream::stream;
use async_trait::async_trait;
use concerto_core::error::{describe_error_chain, ProviderError};
use concerto_core::traits::{CompletionStream, LlmProvider};
use concerto_core::types::{CompletionChunk, CompletionRequest, ModelInfo, TokenBudget};
use concerto_core::CancellationToken;
use futures::stream::StreamExt;

use crate::adapters::{Dialect, OllamaChatDialect, ReasoningEcho};

pub struct OllamaProvider {
    base_url: String,
    model: String,
    timeout_secs: u64,
    dialect: OllamaChatDialect,
    /// Tool-schema presentation tier (adaptive tool schemas). Resolved per
    /// request against the actual model name; `Auto` (default) keeps every
    /// non-weak model on the verbatim strict schema.
    tool_schema_mode: concerto_config::ToolSchemaMode,
}

impl OllamaProvider {
    pub fn new(model: String, timeout_secs: u64) -> Self {
        Self {
            base_url: "http://localhost:11434".to_string(),
            model,
            timeout_secs,
            dialect: OllamaChatDialect,
            tool_schema_mode: concerto_config::ToolSchemaMode::default(),
        }
    }

    pub fn with_base_url(mut self, base_url: String) -> Self {
        self.base_url = base_url;
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

#[async_trait]
impl LlmProvider for OllamaProvider {
    async fn test_connection(&self, _cancel: CancellationToken) -> Result<(), ProviderError> {
        let client = crate::new_client(self.timeout_secs);
        let url = format!("{}/api/tags", self.base_url);
        let resp = client.get(&url).send().await.map_err(|e| {
            ProviderError::Other(format!("ollama connection failed: {}", describe_error_chain(&e)))
        })?;
        if resp.status().is_success() {
            Ok(())
        } else {
            Err(ProviderError::Other(format!("ollama returned {}", resp.status())))
        }
    }

    async fn list_models(
        &self,
        _cancel: CancellationToken,
    ) -> Result<Vec<ModelInfo>, ProviderError> {
        let client = crate::new_client(self.timeout_secs);
        let url = format!("{}/api/tags", self.base_url);
        let resp = client.get(&url).send().await.map_err(|e| {
            ProviderError::Other(format!("ollama list_models failed: {}", describe_error_chain(&e)))
        })?;

        if !resp.status().is_success() {
            return Err(ProviderError::Other(format!(
                "ollama list_models returned {}",
                resp.status()
            )));
        }

        let json: serde_json::Value = resp.json().await.map_err(|e| {
            ProviderError::Other(format!(
                "ollama list_models parse failed: {}",
                describe_error_chain(&e)
            ))
        })?;

        let models = json["models"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| {
                        let name = v["name"].as_str()?;
                        Some(ModelInfo {
                            id: name.to_string(),
                            name: Some(name.to_string()),
                            owned_by: None,
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
        let span = tracing::info_span!(
            "provider.stream_completion",
            provider = "ollama",
            model = %request.model,
        );
        let _guard = span.enter();

        let client = crate::new_client(self.timeout_secs);
        let url = format!("{}/api/chat", self.base_url);
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

        // Request-body rendering now lives in `crate::adapters::ollama`
        // (`OllamaChatDialect`); stream parsing remains here in the connector.
        let body = self.dialect.render_chat_body(&request, &model, ReasoningEcho::IfPresent);

        let response = {
            let client = &client;
            let url = &url;
            let body = &body;
            tokio::select! {
                _ = cancel.cancelled() => return Err(ProviderError::Cancelled),
                result = async {
                    let r = client
                        .post(url)
                        .header("Content-Type", "application/json")
                        .json(body)
                        .send()
                        .await
                        .map_err(|e| ProviderError::Network(format!("request failed: {}", describe_error_chain(&e))))?;

                    if !r.status().is_success() {
                        let status = r.status();
                        let retry_after = crate::retry::parse_retry_after(r.headers());
                        let text = r.text().await.unwrap_or_default();
                        return Err(crate::retry::map_http_error(
                            status,
                            &text,
                            retry_after,
                        ));
                    }
                    Ok(r)
                } => { result? }
            }
        };

        let cancel = cancel.clone();
        let s = stream! {
            let mut byte_stream = response.bytes_stream();
            let mut buf: Vec<u8> = Vec::new();
            while let Some(chunk) = byte_stream.next().await {
                if cancel.is_cancelled() {
                    yield Err(ProviderError::Cancelled);
                    break;
                }
                let items = match chunk {
                    Ok(bytes) => {
                        buf.extend_from_slice(&bytes);
                        let mut items = Vec::new();
                        while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                            // Strip a trailing '\r' from CRLF line endings.
                            let mut line_bytes = buf[..pos].to_vec();
                            if matches!(line_bytes.last(), Some(b'\r')) {
                                line_bytes.pop();
                            }
                            buf.drain(..pos + 1);
                            // `from_utf8` failure can only mean the line itself
                            // is malformed: a multi-byte character split across
                            // chunks stays raw in the byte buffer until its
                            // line completes, so the lossy fallback is scoped to
                            // this line only and cannot corrupt adjacent lines.
                            let line = match String::from_utf8(line_bytes) {
                                Ok(s) => s,
                                Err(e) => String::from_utf8_lossy(e.as_bytes()).to_string(),
                            };
                            if line.is_empty() { continue; }
                            items.extend(ollama_line_to_chunks(&line, tool_adapted));
                        }
                        items
                    }
                    Err(e) => vec![Err(ProviderError::Other(format!("stream error: {}", describe_error_chain(&e))))],
                };
                for item in items {
                    yield item;
                }
            }
            // A non-streamed response body (`stream: false`, the weak-model
            // tier) is a single JSON object that may arrive without a
            // trailing newline: flush the residual buffer as the final line
            // so its full message and `done: true` are not dropped.
            if !cancel.is_cancelled() && !buf.is_empty() {
                let residual = String::from_utf8_lossy(&buf).trim().to_string();
                buf.clear();
                if !residual.is_empty() {
                    for item in ollama_line_to_chunks(&residual, tool_adapted) {
                        yield item;
                    }
                }
            }
        }
        .boxed();

        Ok(s)
    }

    fn context_capacity(&self, _model: &str) -> TokenBudget {
        TokenBudget::new(8_192, 1_024)
    }

    fn approximate_cost(&self, _tokens_in: u64, _tokens_out: u64) -> f64 {
        // Ollama is local/self-hosted; no provider-billed API cost is recorded.
        0.0
    }

    fn provider_name(&self) -> &'static str {
        "ollama"
    }
}

/// Reduce one Ollama response line into canonical completion chunks.
///
/// Both transports share this shape: streaming emits one NDJSON object per
/// line, and a non-streamed response (`stream: false`, the weak-model tier)
/// is a single JSON object carrying the full `message` and `done: true`.
/// Tool-call arguments arrive as complete JSON values in both cases, so
/// loose-schema dot-notation keys are re-nested here whenever the request
/// was rendered with adapted schemas.
fn ollama_line_to_chunks(
    line: &str,
    tool_adapted: bool,
) -> Vec<Result<CompletionChunk, ProviderError>> {
    let mut items = Vec::new();
    match serde_json::from_str::<serde_json::Value>(line) {
        Ok(json) => {
            if let Some(content) = json["message"]["content"].as_str() {
                if !content.is_empty() {
                    items.push(Ok(CompletionChunk {
                        reasoning: None,
                        delta: content.to_string(),
                        tool_call: None,
                        is_final: false,
                        usage: None,
                    }));
                }
            }
            // Parse tool calls from the message (Ollama follows
            // OpenAI's format: tool_calls[].function.{name, arguments}).
            if let Some(tcs) = json["message"]["tool_calls"].as_array() {
                for tc in tcs {
                    if let Some(func) = tc.get("function") {
                        let name = func.get("name").and_then(|v| v.as_str()).unwrap_or("unknown");
                        let mut args =
                            func.get("arguments").cloned().unwrap_or(serde_json::Value::Null);
                        // Adaptive tool schemas: re-nest dot-notation
                        // arguments from loose-schema responses before the
                        // executor or tool-call guard sees them.
                        if tool_adapted {
                            crate::adapters::schema_loose::unflatten_tool_arguments(&mut args);
                        }
                        // Generate a stable call ID for the tool result
                        // round-trip.
                        let id = format!("oc_{}", name);
                        items.push(Ok(CompletionChunk {
                            reasoning: None,
                            delta: String::new(),
                            tool_call: Some(concerto_core::types::ToolCall {
                                id,
                                name: name.to_string(),
                                arguments: args,
                            }),
                            is_final: false,
                            usage: None,
                        }));
                    }
                }
            }
            if json.get("done").and_then(|d| d.as_bool()).unwrap_or(false) {
                items.push(Ok(CompletionChunk {
                    reasoning: None,
                    delta: String::new(),
                    tool_call: None,
                    is_final: true,
                    usage: None,
                }));
            }
        }
        Err(_) => {
            items.push(Err(ProviderError::Other(format!(
                "failed to parse Ollama response: {line}"
            ))));
        }
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A non-streamed response body (`stream: false`, the weak-model tier) is
    /// one JSON object with the full message: it reduces to content, ONE
    /// whole tool-call arguments object, and the terminal chunk — exactly the
    /// shapes the streaming transport emits.
    #[test]
    fn non_stream_body_reduces_to_whole_tool_call_and_final() {
        let body = serde_json::json!({
            "model": "mimo-v2.5:latest",
            "message": {
                "role": "assistant",
                "content": "Reading the file.",
                "tool_calls": [{
                    "function": {
                        "name": "filesystem",
                        "arguments": {"operation": "read", "path": "src/main.rs"}
                    }
                }]
            },
            "done": true,
            "done_reason": "stop"
        });

        let chunks: Vec<CompletionChunk> = ollama_line_to_chunks(&body.to_string(), false)
            .into_iter()
            .map(|result| result.expect("chunk emitted"))
            .collect();

        assert_eq!(chunks.len(), 3, "content + tool call + final");
        assert_eq!(chunks[0].delta, "Reading the file.");
        let tool = chunks[1].tool_call.as_ref().expect("tool-call chunk");
        assert_eq!(tool.name, "filesystem");
        assert_eq!(
            tool.arguments,
            serde_json::json!({"operation": "read", "path": "src/main.rs"}),
            "arguments must arrive as ONE whole object, not truncated deltas"
        );
        assert!(chunks[2].is_final);
    }

    /// Dotted arguments emitted under loose schemas are re-nested before
    /// leaving the connector, on both transports.
    #[test]
    fn tool_adapted_line_unflattens_dotted_arguments() {
        let line = serde_json::json!({
            "message": {"tool_calls": [{
                "function": {"name": "runner", "arguments": {"config.mode": "fast"}}
            }]},
            "done": false
        })
        .to_string();

        let chunks: Vec<CompletionChunk> = ollama_line_to_chunks(&line, true)
            .into_iter()
            .map(|result| result.expect("chunk emitted"))
            .collect();
        let tool = chunks[0].tool_call.as_ref().expect("tool-call chunk");
        assert_eq!(tool.arguments, serde_json::json!({"config": {"mode": "fast"}}));
    }

    /// An unparseable line surfaces as a structured provider error, never a
    /// silent drop.
    #[test]
    fn unparseable_line_yields_error() {
        let items = ollama_line_to_chunks("not json", false);
        assert_eq!(items.len(), 1);
        assert!(items[0].is_err());
    }
}
