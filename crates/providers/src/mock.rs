use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use concerto_core::error::ProviderError;
use concerto_core::traits::provider::{CompletionStream, LlmProvider};
use concerto_core::types::{CompletionChunk, CompletionRequest, ProviderMetrics, TokenBudget};
use concerto_core::CancellationToken;
use futures::stream;

/// A simple mock LLM provider used for evaluation without real API keys.
///
/// Default behavior returns a single empty final chunk and reports
/// configurable latency, token usage and cost. All responses are
/// deterministic.
///
/// [`MockProvider::scripted`] mode instead replays a per-turn script of
/// [`CompletionChunk`]s: turn `0` answers the first `stream_completion` call,
/// turn `1` the second, and so on; a request beyond the script's end gets an
/// empty final chunk. This lets tests drive a deterministic tool-call
/// conversation (e.g. the ADR-60 S5 agent-process entry) without a real
/// model. The script must be rebuilt per provider instance (it is not
/// shareable) — construct a fresh provider for each child.
pub struct MockProvider {
    /// Simulated latency in milliseconds for each request.
    pub latency_ms: u64,
    /// Tokens counted as input.
    pub tokens_in: u64,
    /// Tokens counted as output.
    pub tokens_out: u64,
    /// Simulated cost in USD.
    pub cost_usd: f64,
    /// Optional per-turn chunk script; `None` = default single-final-chunk
    /// behavior. Shared through the provider, so it is only set at build time.
    script: Option<Vec<Vec<CompletionChunk>>>,
    /// Index of the next scripted turn to serve.
    turn: AtomicUsize,
}

impl Default for MockProvider {
    fn default() -> Self {
        Self {
            latency_ms: 0,
            tokens_in: 0,
            tokens_out: 0,
            cost_usd: 0.0,
            script: None,
            turn: AtomicUsize::new(0),
        }
    }
}

impl MockProvider {
    /// A provider that replays `script` verbatim, one turn per
    /// `stream_completion` call. Chunks are cloned out of the scripted
    /// vectors (the provider is shared, the script is not).
    pub fn scripted(script: Vec<Vec<CompletionChunk>>) -> Self {
        Self {
            latency_ms: 0,
            tokens_in: 0,
            tokens_out: 0,
            cost_usd: 0.0,
            script: Some(script),
            turn: AtomicUsize::new(0),
        }
    }

    /// Collect metrics for the mock provider.
    pub fn collect_metrics(&self) -> ProviderMetrics {
        ProviderMetrics {
            provider: self.provider_name().to_string(),
            model: "mock-model".to_string(),
            tokens_in: self.tokens_in,
            tokens_out: self.tokens_out,
            cost_usd: self.cost_usd,
            latency_ms: self.latency_ms,
        }
    }
}

#[async_trait]
impl LlmProvider for MockProvider {
    fn provider_name(&self) -> &'static str {
        "mock"
    }

    fn context_capacity(&self, _model: &str) -> TokenBudget {
        TokenBudget::new(128_000, 4_096)
    }

    fn approximate_cost(&self, _tokens_in: u64, _tokens_out: u64) -> f64 {
        self.cost_usd
    }

    async fn stream_completion(
        &self,
        _request: CompletionRequest,
        _cancel: CancellationToken,
    ) -> Result<CompletionStream, ProviderError> {
        // Consume the next scripted turn, if any; requests beyond the script
        // fall back to a single empty final chunk.
        let turn_index = self.turn.fetch_add(1, Ordering::SeqCst);
        let chunks: Vec<Result<CompletionChunk, ProviderError>> = match &self.script {
            Some(script) => script
                .get(turn_index)
                .cloned()
                .unwrap_or_else(|| vec![empty_final_chunk()])
                .into_iter()
                .map(Ok)
                .collect(),
            None => vec![Ok(empty_final_chunk())],
        };
        // Simulate latency by sleeping inside the stream iterator.
        let latency = self.latency_ms;
        let iter = chunks.into_iter().inspect(move |_| {
            if latency > 0 {
                std::thread::sleep(std::time::Duration::from_millis(latency));
            }
        });
        Ok(Box::pin(stream::iter(iter)))
    }
}

/// The default terminal chunk: empty delta, no tool call.
fn empty_final_chunk() -> CompletionChunk {
    CompletionChunk {
        reasoning: None,
        delta: String::new(),
        tool_call: None,
        is_final: true,
        usage: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use concerto_core::types::{CompletionRequest, ToolCall};
    use serde_json::json;

    fn request() -> CompletionRequest {
        CompletionRequest {
            model: "mock-model".to_owned(),
            messages: vec![],
            tools: None,
            tool_choice: None,
            temperature: None,
            max_tokens: None,
            stream: true,
        }
    }

    async fn collect_once(provider: &MockProvider) -> Vec<CompletionChunk> {
        use futures::StreamExt;
        let mut stream = provider
            .stream_completion(request(), CancellationToken::new())
            .await
            .expect("mock stream starts");
        let mut chunks = Vec::new();
        while let Some(chunk) = stream.next().await {
            chunks.push(chunk.expect("mock chunk decodes"));
        }
        chunks
    }

    #[tokio::test]
    async fn default_returns_one_empty_final_chunk() {
        let provider = MockProvider::default();
        let chunks = collect_once(&provider).await;
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].is_final);
        assert!(chunks[0].tool_call.is_none());
    }

    #[tokio::test]
    async fn scripted_replays_turns_and_falls_back_after_script_end() {
        let tool_call_chunk = CompletionChunk {
            reasoning: None,
            delta: String::new(),
            tool_call: Some(ToolCall {
                id: "call-1".to_owned(),
                name: "write_file".to_owned(),
                arguments: json!({ "path": "note.md" }),
            }),
            is_final: false,
            usage: None,
        };
        let final_chunk = empty_final_chunk();
        let provider = MockProvider::scripted(vec![
            vec![tool_call_chunk.clone(), final_chunk.clone()],
            vec![final_chunk.clone()],
        ]);

        let turn_0 = collect_once(&provider).await;
        assert_eq!(turn_0.len(), 2);
        assert_eq!(turn_0[0].tool_call.as_ref().expect("tool call").id, "call-1");

        let turn_1 = collect_once(&provider).await;
        assert_eq!(turn_1.len(), 1);
        assert!(turn_1[0].is_final);

        // Beyond the script: deterministic fallback, never an error.
        let turn_2 = collect_once(&provider).await;
        assert_eq!(turn_2.len(), 1);
        assert!(turn_2[0].is_final);
    }
}
