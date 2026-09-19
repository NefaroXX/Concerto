//! Test harness: `ScriptedProvider` — a mock LLM provider that returns
//! pre-configured responses.
//!
//! Used by orchestrator tests to validate the agent loop without a real
//! LLM API call.

use async_trait::async_trait;
use concerto_core::error::ProviderError;
use concerto_core::traits::provider::{CompletionStream, LlmProvider};
use concerto_core::types::{
    CompletionChunk, CompletionRequest, ProviderMetrics, TokenBudget, ToolCall,
};
use concerto_core::CancellationToken;
use futures::stream;
use std::collections::VecDeque;

/// A pre-configured response from the scripted provider.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum ScriptedResponse {
    /// Emit a text delta.
    Text(String),
    /// Emit a tool call.
    ToolCall(ToolCall),
    /// Signal completion.
    Done,
}

/// A mock LLM provider that returns pre-configured responses in sequence.
///
/// Usage:
/// ```ignore
/// let provider = ScriptedProvider::new(vec![
///     ScriptedResponse::ToolCall(ToolCall { id: "call_1".into(), name: "shell".into(), arguments: json!({"command": "echo hi"}) }),
///     ScriptedResponse::Text("The output was: hi".into()),
///     ScriptedResponse::Done,
/// ]);
/// ```
pub struct ScriptedProvider {
    responses: VecDeque<ScriptedResponse>,
}

impl ScriptedProvider {
    /// Create a new provider with the given sequence of responses.
    pub fn new(responses: Vec<ScriptedResponse>) -> Self {
        Self { responses: VecDeque::from(responses) }
    }

    /// Convenience: create a single text response then done.
    pub fn text(content: &str) -> Self {
        Self::new(vec![ScriptedResponse::Text(content.to_string()), ScriptedResponse::Done])
    }

    /// Convenience: create a single tool call response then done.
    pub fn tool_call(name: &str, arguments: serde_json::Value) -> Self {
        Self::new(vec![
            ScriptedResponse::ToolCall(ToolCall {
                id: "call_scripted".to_string(),
                name: name.to_string(),
                arguments,

                ..Default::default()
            }),
            ScriptedResponse::Done,
        ])
    }

    /// Convenience: done immediately with a final message.
    pub fn done(message: &str) -> Self {
        Self::new(vec![ScriptedResponse::Text(message.to_string()), ScriptedResponse::Done])
    }
}

#[async_trait]
impl LlmProvider for ScriptedProvider {
    fn provider_name(&self) -> &'static str {
        "scripted"
    }

    fn context_capacity(&self, _model: &str) -> TokenBudget {
        TokenBudget::new(128_000, 4_096)
    }

    fn approximate_cost(&self, _tokens_in: u64, _tokens_out: u64) -> f64 {
        0.0
    }

    async fn stream_completion(
        &self,
        _request: CompletionRequest,
        _cancel: CancellationToken,
    ) -> Result<CompletionStream, ProviderError> {
        let cloned = self.responses.clone();
        let iter = cloned.into_iter().map(|response| {
            Ok(match response {
                ScriptedResponse::Text(delta) => CompletionChunk {
                    delta,
                    reasoning: None,
                    tool_call: None,
                    is_final: false,
                    usage: None,
                },
                ScriptedResponse::ToolCall(tc) => CompletionChunk {
                    delta: String::new(),
                    reasoning: None,
                    tool_call: Some(tc),
                    is_final: false,
                    usage: None,
                },
                ScriptedResponse::Done => CompletionChunk {
                    delta: String::new(),
                    reasoning: None,
                    tool_call: None,
                    is_final: true,
                    usage: None,
                },
            })
        });

        Ok(Box::pin(stream::iter(iter)))
    }
}

impl Default for ScriptedProvider {
    fn default() -> Self {
        Self::done("ok")
    }
}

/// Assert the ADR-48 §4 usage contract across a finished provider stream.
///
/// Every provider connector must surface wire usage on the terminal chunk
/// when the provider reported it, and never on intermediate chunks; when the
/// wire carries no usable counts, usage stays `None`. `None` and `0` are both
/// legitimate provider reports, so this helper only checks *placement*, never
/// coalescing: it fails when a non-terminal chunk carries usage, or when more
/// than one chunk in the stream carries it. New connectors mirror the
/// per-provider `stream_*_usage*` tests using this helper so the rule stays
/// enforced as provider coverage grows.
pub fn assert_terminal_usage_contract(chunks: &[CompletionChunk]) {
    let carrying = chunks.iter().filter(|chunk| chunk.usage.is_some()).count();
    assert!(
        carrying <= 1,
        "usage must surface on at most one (terminal) chunk; found {carrying} carrying chunks"
    );
    for (index, chunk) in chunks.iter().enumerate() {
        if chunk.usage.is_some() {
            assert!(chunk.is_final, "chunk #{index} carries usage but is not the terminal chunk");
        }
    }
}

impl ScriptedProvider {
    pub fn collect_metrics(&self) -> ProviderMetrics {
        ProviderMetrics {
            provider: self.provider_name().to_string(),
            model: "test-model".to_string(),
            tokens_in: 0,
            tokens_out: 0,
            cost_usd: 0.0,
            latency_ms: 0,
        }
    }
}

/// One-shot HTTP mock server and shared stream assertions for the thin
/// OpenAI-compatible provider wrappers.
///
/// Each wrapper test runs a tiny `std::net::TcpListener` server on a
/// background thread that records the request body and answers with a canned
/// SSE stream (no HTTP-mock crate exists in this workspace). Every wait is
/// bounded (`recv_timeout`, `tokio::time::timeout`). The shared
/// [`assert_function_shaped_tool_calls`] / [`assert_flat_shaped_tool_calls`]
/// helpers prove the canonical and Fix-1 flat proxy tool-call shapes through
/// any wrapper via its `with_api_base` builder, so new connectors only carry
/// a two-line test each instead of a full HTTP-stub suite.
#[cfg(test)]
pub mod mock_server {
    use concerto_core::error::ProviderError;
    use concerto_core::traits::{CompletionStream, LlmProvider};
    use concerto_core::types::{
        CompletionChunk, CompletionRequest, Message, Role, ToolCall, ToolDefinition,
    };
    use concerto_core::CancellationToken;
    use futures::TryStreamExt;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::mpsc;
    use std::time::Duration;

    /// Serve `sse_body` for the next single request and return
    /// `(base_url, captured_request_receiver)`.
    pub fn spawn(sse_body: String) -> (String, mpsc::Receiver<Vec<u8>>) {
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

    /// Parse the captured request's JSON body out of its raw HTTP bytes.
    pub fn request_body(raw: Vec<u8>) -> serde_json::Value {
        let header_end = find_subsequence(&raw, b"\r\n\r\n").expect("captured request has headers");
        serde_json::from_slice(&raw[header_end + 4..]).expect("captured request body is JSON")
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

    /// A chat request asking for a command to run, with the `shell` tool.
    fn shell_request() -> CompletionRequest {
        CompletionRequest {
            stream: true,
            messages: vec![user_message("list files")],
            tools: Some(vec![ToolDefinition {
                name: "shell".to_string(),
                description: "Run a command.".to_string(),
                parameters: serde_json::json!({"type": "object", "properties": {}}),
            }]),
            ..Default::default()
        }
    }

    /// A chat request asking for a manifest read, with the `read_file` tool.
    fn read_file_request() -> CompletionRequest {
        CompletionRequest {
            stream: true,
            messages: vec![user_message("read the manifest")],
            tools: Some(vec![ToolDefinition {
                name: "read_file".to_string(),
                description: "Read a file.".to_string(),
                parameters: serde_json::json!({"type": "object", "properties": {}}),
            }]),
            ..Default::default()
        }
    }

    /// Drain a completion stream, bounding the wait.
    async fn collect(stream: CompletionStream) -> Result<Vec<CompletionChunk>, ProviderError> {
        tokio::time::timeout(Duration::from_secs(10), stream.try_collect())
            .await
            .map_err(|_| ProviderError::Other("mock-server stream timed out".to_string()))?
    }

    /// Assert the canonical `function {name, arguments}` tool-call shape
    /// parses through `make`, a provider wrapper built from a mock base URL.
    pub async fn assert_function_shaped_tool_calls(
        make: impl FnOnce(String) -> Box<dyn LlmProvider>,
    ) {
        let sse = concat!(
            "data: ",
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_fn","type":"function","function":{"name":"shell","arguments":"{\"command\":\"ls\"}"}}]}}]}"#,
            "\n\n",
            "data: [DONE]\n\n",
        );
        let (base, _req_rx) = spawn(sse.to_string());
        let provider = make(base);
        let request = shell_request();
        let stream =
            provider.stream_completion(request, CancellationToken::new()).await.expect("stream");
        let chunks = collect(stream).await.expect("collect");

        let calls: Vec<&ToolCall> = chunks.iter().filter_map(|c| c.tool_call.as_ref()).collect();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell");
        assert_eq!(calls[0].arguments, serde_json::json!({"command": "ls"}));
        assert!(chunks.last().expect("final chunk").is_final, "stream terminates");
    }

    /// Assert the Fix 1 flat proxy fallback (`name` / `arguments` directly on
    /// the tool-call object, split across SSE deltas) parses through `make`.
    pub async fn assert_flat_shaped_tool_calls(make: impl FnOnce(String) -> Box<dyn LlmProvider>) {
        let sse = concat!(
            "data: ",
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_flat","name":"read_file","arguments":"{\"path\":"}]}}]}"#,
            "\n\n",
            "data: ",
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"name":"read_file","arguments":"\"Cargo.toml\"}"}]}}]}"#,
            "\n\n",
            "data: [DONE]\n\n",
        );
        let (base, _req_rx) = spawn(sse.to_string());
        let provider = make(base);
        let request = read_file_request();
        let stream =
            provider.stream_completion(request, CancellationToken::new()).await.expect("stream");
        let chunks = collect(stream).await.expect("collect");

        let calls: Vec<&ToolCall> = chunks.iter().filter_map(|c| c.tool_call.as_ref()).collect();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_flat");
        assert_eq!(calls[0].name, "read_file");
        assert_eq!(calls[0].arguments, serde_json::json!({"path": "Cargo.toml"}));
        assert!(chunks.last().expect("final chunk").is_final, "stream terminates");
    }
}
