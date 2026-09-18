//! DeepSeek provider.
//!
//! DeepSeek serves an OpenAI-compatible Chat Completions API
//! (`deepseek-chat`, `deepseek-reasoner`). This provider is a thin wrapper
//! around [`OpenAiProvider`] pointed at the DeepSeek endpoint, in the same
//! style as the OpenRouter and NVIDIA NIM wrappers.
//!
//! DeepSeek is a reasoning endpoint (ADR-46): once tool calls are in history,
//! every assistant message must carry `reasoning_content` (an empty string
//! when the model produced none) or the API rejects the request. The
//! connector therefore defaults to [`ReasoningEcho::Always`] at construction
//! — the same contract as the OpenCode Zen gateway — while still allowing an
//! explicit override via [`DeepSeekProvider::with_reasoning_echo`].

use async_trait::async_trait;
use concerto_core::error::ProviderError;
use concerto_core::traits::{CompletionStream, LlmProvider};
use concerto_core::types::{CompletionRequest, ModelInfo, TokenBudget};
use concerto_core::CancellationToken;

use crate::openai::{OpenAiProvider, ReasoningEcho};

/// Default DeepSeek API base URL.
///
/// The `/v1` prefix is the OpenAI-SDK-compatible form; the connector appends
/// `/chat/completions` and `/models` to this base, exactly like the OpenRouter
/// (`/api/v1`) and NVIDIA NIM (`/v1`) wrappers. DeepSeek's docs accept both
/// `https://api.deepseek.com` and `https://api.deepseek.com/v1`.
pub(crate) const DEEPSEEK_API_BASE: &str = "https://api.deepseek.com/v1";

/// Thin OpenAI-compatible wrapper for DeepSeek.
///
/// Tool-call normalization, streaming, loose-schema adaptation (ADR-66 §4),
/// the flat proxy tool-call fallback, and reasoning capture all come from the
/// underlying [`OpenAiProvider`]; this struct only fixes the endpoint and the
/// ADR-46 echo policy.
pub struct DeepSeekProvider {
    inner: OpenAiProvider,
}

impl DeepSeekProvider {
    /// Build a provider targeting the DeepSeek endpoint.
    pub fn new(api_key: String, model: String, timeout_secs: u64) -> Self {
        Self::with_api_base(api_key, model, timeout_secs, DEEPSEEK_API_BASE.to_string())
    }

    /// Build a provider with an explicit API base URL, overriding the DeepSeek
    /// default.
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

    /// Override the reasoning-content echo policy (ADR-46).
    ///
    /// The connector defaults to [`ReasoningEcho::Always`] for the DeepSeek
    /// reasoning contract; `with_reasoning_echo` lets a caller or config dial
    /// opt back in to [`ReasoningEcho::IfPresent`].
    pub fn with_reasoning_echo(mut self, echo: ReasoningEcho) -> Self {
        self.inner = self.inner.with_reasoning_echo(echo);
        self
    }

    /// Set the tool-schema presentation mode (adaptive tool schemas).
    ///
    /// Defaults to [`concerto_config::ToolSchemaMode::Auto`] — see
    /// [`OpenAiProvider::with_tool_schema_mode`] for the loose-schema
    /// semantics.
    pub fn with_tool_schema_mode(mut self, mode: concerto_config::ToolSchemaMode) -> Self {
        self.inner = self.inner.with_tool_schema_mode(mode);
        self
    }
}

#[async_trait]
impl LlmProvider for DeepSeekProvider {
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
        // DeepSeek V4 Flash pricing (docs/missing-providers.md): $0.14 input /
        // $0.28 output per MTok — the cheapest frontier tier.
        let input_cost = (tokens_in as f64 / 1_000_000.0) * 0.14;
        let output_cost = (tokens_out as f64 / 1_000_000.0) * 0.28;
        input_cost + output_cost
    }

    fn provider_name(&self) -> &'static str {
        "deepseek"
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

// ---------------------------------------------------------------------------
// Tests
//
// The tool-call parser (the canonical `function` shape and the Fix 1 flat
// proxy fallback) and the ADR-46 reasoning-echo render live inside the inner
// `OpenAiProvider`. To prove them *through* `DeepSeekProvider` without adding
// an HTTP-mock dependency (none exists in this workspace), the tests run a
// tiny one-shot HTTP server on `std::net::TcpListener` in a background
// thread: it records the request body it receives and answers with a canned
// SSE stream. Every wait is bounded (`recv_timeout`, `tokio::time::timeout`).
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use concerto_core::types::{
        CompletionChunk, Message, Role, ToolCall, ToolDefinition, ToolResult,
    };
    use futures::TryStreamExt;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::mpsc;
    use std::time::Duration;

    /// Serve `sse_body` for the next single request and return
    /// `(base_url, captured_request_receiver)`.
    fn spawn_sse_server(sse_body: String) -> (String, mpsc::Receiver<Vec<u8>>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind local mock server");
        let port = listener.local_addr().expect("local address").port();
        let base = format!("http://127.0.0.1:{port}");
        let (req_tx, req_rx) = mpsc::channel();
        let _ = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept one request");
            let request = read_request(&mut stream);
            let _ = req_tx.send(request.clone());
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                sse_body.len(),
                sse_body,
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        });
        (base, req_rx)
    }

    /// Read a full HTTP request: headers plus a `Content-Length`-framed body.
    fn read_request(stream: &mut TcpStream) -> Vec<u8> {
        let mut raw = Vec::new();
        let mut buf = [0u8; 2048];
        loop {
            let n = stream.read(&mut buf).expect("read request bytes");
            if n == 0 {
                break;
            }
            raw.extend_from_slice(&buf[..n]);
            if let Some(header_end) = find_subsequence(&raw, b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&raw[..header_end]);
                let declared = content_length(&headers);
                let body_start = header_end + 4;
                let complete = declared.is_none_or(|len| raw.len() >= body_start + len);
                if complete {
                    break;
                }
            }
        }
        raw
    }

    fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack.windows(needle.len()).position(|window| window == needle)
    }

    fn content_length(headers: &str) -> Option<usize> {
        headers.lines().find_map(|line| {
            let (name, value) = line.split_once(':')?;
            if name.trim().eq_ignore_ascii_case("content-length") {
                value.trim().parse::<usize>().ok()
            } else {
                None
            }
        })
    }

    /// Parse the captured request's JSON body out of its raw HTTP bytes.
    fn request_body(raw: Vec<u8>) -> serde_json::Value {
        let header_end = find_subsequence(&raw, b"\r\n\r\n").expect("captured request has headers");
        serde_json::from_slice(&raw[header_end + 4..]).expect("captured request body is JSON")
    }

    fn user_message(content: &str) -> Message {
        Message {
            role: Role::User,
            content: content.to_string(),
            tool_calls: None,
            tool_results: None,
            reasoning_content: None,
            tokens_in: None,
            tokens_out: None,
        }
    }

    /// Drain a completion stream, bounding the wait.
    async fn collect_stream(
        stream: CompletionStream,
    ) -> Result<Vec<CompletionChunk>, ProviderError> {
        tokio::time::timeout(Duration::from_secs(10), stream.try_collect())
            .await
            .map_err(|_| ProviderError::Other("deepseek test stream timed out".to_string()))?
    }

    #[test]
    fn provider_name_is_deepseek() {
        let provider = DeepSeekProvider::new("key".to_string(), "deepseek-chat".to_string(), 15);
        assert_eq!(provider.provider_name(), "deepseek");
    }

    #[test]
    fn context_capacity_uses_deepseek_budget() {
        let provider = DeepSeekProvider::new("key".to_string(), "deepseek-chat".to_string(), 15);
        assert_eq!(provider.context_capacity("deepseek-chat").capacity, 1_000_000);
        assert_eq!(provider.context_capacity("deepseek-reasoner").capacity, 1_000_000);
    }

    /// The canonical `function {name, arguments}` tool-call shape parses
    /// through the DeepSeek provider (inherited from the inner
    /// `OpenAiProvider`).
    #[tokio::test]
    async fn stream_parses_function_shaped_tool_calls() {
        let sse = concat!(
            "data: ",
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_fn","type":"function","function":{"name":"shell","arguments":"{\"command\":\"ls\"}"}}]}}]}"#,
            "\n\n",
            "data: [DONE]\n\n",
        );
        let (base, _req_rx) = spawn_sse_server(sse.to_string());
        let provider = DeepSeekProvider::with_api_base(
            "sk-test".to_string(),
            "deepseek-chat".to_string(),
            15,
            base,
        );
        let request = CompletionRequest {
            stream: true,
            messages: vec![user_message("list files")],
            tools: Some(vec![ToolDefinition {
                name: "shell".to_string(),
                description: "Run a command.".to_string(),
                parameters: serde_json::json!({"type": "object", "properties": {}}),
            }]),
            ..Default::default()
        };
        let stream =
            provider.stream_completion(request, CancellationToken::new()).await.expect("stream");
        let chunks = collect_stream(stream).await.expect("collect");

        let calls: Vec<&ToolCall> = chunks.iter().filter_map(|c| c.tool_call.as_ref()).collect();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell");
        assert_eq!(calls[0].arguments, serde_json::json!({"command": "ls"}));
        assert!(chunks.last().expect("final chunk").is_final, "stream terminates");
    }

    /// Fix 1 regression: the flat proxy fallback (`name` / `arguments`
    /// directly on the tool-call object, split across SSE deltas) also works
    /// through the DeepSeek provider.
    #[tokio::test]
    async fn stream_parses_flat_shaped_tool_calls() {
        let sse = concat!(
            "data: ",
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_flat","name":"read_file","arguments":"{\"path\":"}]}}]}"#,
            "\n\n",
            "data: ",
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"name":"read_file","arguments":"\"Cargo.toml\"}"}]}}]}"#,
            "\n\n",
            "data: [DONE]\n\n",
        );
        let (base, _req_rx) = spawn_sse_server(sse.to_string());
        let provider = DeepSeekProvider::with_api_base(
            "sk-test".to_string(),
            "deepseek-chat".to_string(),
            15,
            base,
        );
        let request = CompletionRequest {
            stream: true,
            messages: vec![user_message("read the manifest")],
            tools: Some(vec![ToolDefinition {
                name: "read_file".to_string(),
                description: "Read a file.".to_string(),
                parameters: serde_json::json!({"type": "object", "properties": {}}),
            }]),
            ..Default::default()
        };
        let stream =
            provider.stream_completion(request, CancellationToken::new()).await.expect("stream");
        let chunks = collect_stream(stream).await.expect("collect");

        let calls: Vec<&ToolCall> = chunks.iter().filter_map(|c| c.tool_call.as_ref()).collect();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_flat");
        assert_eq!(calls[0].name, "read_file");
        assert_eq!(calls[0].arguments, serde_json::json!({"path": "Cargo.toml"}));
        assert!(chunks.last().expect("final chunk").is_final, "stream terminates");
    }

    /// ADR-46 wire contract for DeepSeek: the request rendered by the
    /// provider carries `reasoning_content` on *every* assistant message —
    /// verbatim when captured, `""` when the model produced none.
    #[tokio::test]
    async fn request_carries_reasoning_echo_always() {
        let sse = concat!(
            "data: ",
            r#"{"choices":[{"delta":{"content":"ok"}}]}"#,
            "\n\n",
            "data: [DONE]\n\n",
        );
        let (base, req_rx) = spawn_sse_server(sse.to_string());
        let provider = DeepSeekProvider::with_api_base(
            "sk-test".to_string(),
            "deepseek-chat".to_string(),
            15,
            base,
        );
        let request = CompletionRequest {
            stream: true,
            messages: vec![
                // The previous assistant turn issued a tool call and carried
                // captured reasoning: both must survive on the wire.
                Message {
                    role: Role::Assistant,
                    content: String::new(),
                    tool_calls: Some(vec![ToolCall {
                        id: "call_1".to_string(),
                        name: "shell".to_string(),
                        arguments: serde_json::json!({"command": "ls"}),
                        ..Default::default()
                    }]),
                    tool_results: None,
                    reasoning_content: Some("checked the contract".to_string()),
                    tokens_in: None,
                    tokens_out: None,
                },
                // The matching tool result.
                Message {
                    role: Role::Tool,
                    content: "ok".to_string(),
                    tool_calls: None,
                    tool_results: Some(vec![ToolResult {
                        id: "call_1".to_string(),
                        name: "shell".to_string(),
                        content: serde_json::json!("ok"),
                    }]),
                    reasoning_content: None,
                    tokens_in: None,
                    tokens_out: None,
                },
                // A plain assistant turn with no captured reasoning.
                Message {
                    role: Role::Assistant,
                    content: "next".to_string(),
                    tool_calls: None,
                    tool_results: None,
                    reasoning_content: None,
                    tokens_in: None,
                    tokens_out: None,
                },
                user_message("continue"),
            ],
            ..Default::default()
        };
        let stream =
            provider.stream_completion(request, CancellationToken::new()).await.expect("stream");
        let chunks = collect_stream(stream).await.expect("collect");
        assert!(chunks.last().expect("final chunk").is_final, "stream terminates");

        let raw = req_rx.recv_timeout(Duration::from_secs(10)).expect("server captured request");
        let body = request_body(raw);
        let messages = body["messages"].as_array().expect("chat messages");

        let assistant_with_call = messages[0].as_object().expect("assistant message");
        assert_eq!(assistant_with_call["role"], "assistant");
        assert_eq!(
            assistant_with_call["reasoning_content"].as_str(),
            Some("checked the contract"),
            "captured reasoning is echoed verbatim"
        );
        assert!(assistant_with_call["tool_calls"].is_array());

        let assistant_plain = messages[2].as_object().expect("assistant message");
        assert_eq!(assistant_plain["role"], "assistant");
        assert_eq!(
            assistant_plain["reasoning_content"].as_str(),
            Some(""),
            "reasoning-less assistant message still carries the empty field"
        );
    }
}
