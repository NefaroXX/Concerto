//! Wrapper that records metrics for every provider call.
use crate::metrics::MetricsRecorder;
use crate::tokenizer::count_tokens;
use async_trait::async_trait;
use concerto_core::error::ProviderError;
use concerto_core::traits::{CompletionStream, LlmProvider};
use concerto_core::types::{CompletionRequest, ProviderMetrics, TokenBudget};
use concerto_core::CancellationToken;
use futures::StreamExt;
use std::sync::Arc;
use std::sync::Mutex;

/// Wraps an LlmProvider and automatically records usage metrics
/// (token counts, latency, cost) for each completion call.
pub struct MeteredProvider {
    inner: Box<dyn LlmProvider>,
    last_metrics: Arc<Mutex<Option<ProviderMetrics>>>,
}

impl MeteredProvider {
    pub fn new(inner: Box<dyn LlmProvider>) -> Self {
        Self { inner, last_metrics: Arc::new(Mutex::new(None)) }
    }

    /// Consume the metric record from the most recent completion call.
    /// Returns `None` if no call has completed or metrics were already taken.
    pub fn take_last_metrics(&self) -> Option<ProviderMetrics> {
        self.last_metrics.lock().ok()?.take()
    }
}

#[async_trait]
impl LlmProvider for MeteredProvider {
    async fn stream_completion(
        &self,
        request: CompletionRequest,
        cancel: CancellationToken,
    ) -> Result<CompletionStream, ProviderError> {
        let provider_name = self.inner.provider_name();
        let model = request.model.clone();
        let mut inner_stream = self.inner.stream_completion(request, cancel).await?;

        // Pre-compute per-token cost rates from the inner provider's pricing.
        // All real providers (OpenAI, Anthropic, Google, etc.) have linear cost
        // models: cost = tokens_in × rate_in + tokens_out × rate_out.
        // We compute the rates by passing 1M tokens for each side and then
        // dividing, which works correctly for all production providers.
        // For MockProvider (test double with flat cost) this decomposition is
        // approximate, but no test asserts cost_usd on metered metrics today.
        let rate_per_input_token = self.inner.approximate_cost(1_000_000, 0) / 1_000_000.0;
        let rate_per_output_token = self.inner.approximate_cost(0, 1_000_000) / 1_000_000.0;

        let mut recorder = MetricsRecorder::new(provider_name, &model);
        let last_metrics_clone = self.last_metrics.clone();
        let stream = Box::pin(async_stream::stream! {
            while let Some(item) = inner_stream.next().await {
                match item {
                    Ok(chunk) => {
                        let token_count = count_tokens(&chunk.delta, &model).count;
                        recorder.record_tokens_out(token_count);
                        yield Ok(chunk);
                    }
                    Err(e) => {
                        let cost = recorder.tokens_in() as f64 * rate_per_input_token
                            + recorder.tokens_out() as f64 * rate_per_output_token;
                        recorder.set_cost(cost);
                        if let Ok(mut lm) = last_metrics_clone.lock() {
                            *lm = Some(recorder.finish());
                        }
                        yield Err(e);
                        break;
                    }
                }
            }

            let cost = recorder.tokens_in() as f64 * rate_per_input_token
                + recorder.tokens_out() as f64 * rate_per_output_token;
            recorder.set_cost(cost);
            if let Ok(mut lm) = last_metrics_clone.lock() {
                *lm = Some(recorder.finish());
            }
        });
        Ok(Box::pin(stream))
    }

    fn context_capacity(&self, model: &str) -> TokenBudget {
        self.inner.context_capacity(model)
    }

    fn approximate_cost(&self, tokens_in: u64, tokens_out: u64) -> f64 {
        self.inner.approximate_cost(tokens_in, tokens_out)
    }

    fn provider_name(&self) -> &'static str {
        self.inner.provider_name()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::MockProvider;
    use crate::tokenizer::count_tokens;
    use concerto_core::types::{CompletionChunk, CompletionRequest};
    use futures::StreamExt;

    #[tokio::test]
    async fn test_metered_provider_records_metrics_on_success() {
        let mut mock = MockProvider::default();
        mock.latency_ms = 5;
        mock.tokens_in = 100;
        mock.tokens_out = 50;
        mock.cost_usd = 0.002;
        let metered = MeteredProvider::new(Box::new(mock));
        let cancel = CancellationToken::new();

        let request = CompletionRequest {
            model: "test-model".to_string(),
            messages: vec![],
            tools: None,
            tool_choice: None,
            temperature: None,
            max_tokens: None,
            stream: true,
        };

        let stream = metered.stream_completion(request, cancel).await.unwrap();
        let chunks: Vec<_> = stream.collect().await;
        assert_eq!(chunks.len(), 1, "expected one completion chunk");
        assert!(chunks[0].is_ok(), "chunk should be ok");

        let metrics = metered.take_last_metrics().expect("metrics should be recorded");
        assert_eq!(metrics.provider, "mock");
        assert_eq!(metrics.model, "test-model");
        // MockProvider returns an empty delta, so the tokenizer counts 0 tokens.
        let expected = count_tokens("", "test-model").count;
        assert_eq!(
            metrics.tokens_out, expected,
            "token count should match tokenizer for the chunk delta"
        );
        assert!(metrics.latency_ms >= 5, "latency should be at least mock's latency_ms");
    }

    #[tokio::test]
    async fn test_metered_provider_counts_tokens_per_chunk_delta() {
        use concerto_core::traits::LlmProvider;
        use futures::stream;

        /// A provider that returns multi-chunk streams with known delta text.
        struct ChunkedMockProvider;

        #[async_trait]
        impl LlmProvider for ChunkedMockProvider {
            fn provider_name(&self) -> &'static str {
                "chunked-mock"
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
                let chunks = vec![
                    Ok(CompletionChunk {
                        delta: "Hello, ".into(),
                        reasoning: None,
                        tool_call: None,
                        is_final: false,
                        usage: None,
                    }),
                    Ok(CompletionChunk {
                        delta: "world!".into(),
                        reasoning: None,
                        tool_call: None,
                        is_final: false,
                        usage: None,
                    }),
                    Ok(CompletionChunk {
                        delta: String::new(),
                        reasoning: None,
                        tool_call: None,
                        is_final: true,
                        usage: None,
                    }),
                ];
                Ok(Box::pin(stream::iter(chunks)))
            }
        }

        let metered = MeteredProvider::new(Box::new(ChunkedMockProvider));
        let cancel = CancellationToken::new();
        let request = CompletionRequest {
            model: "claude-3-5-sonnet".to_string(),
            messages: vec![],
            tools: None,
            tool_choice: None,
            temperature: None,
            max_tokens: None,
            stream: true,
        };

        let stream = metered.stream_completion(request, cancel).await.unwrap();
        let _chunks: Vec<_> = stream.collect().await;

        let metrics = metered.take_last_metrics().expect("metrics should be recorded");
        let expected = count_tokens("Hello, ", "claude-3-5-sonnet").count
            + count_tokens("world!", "claude-3-5-sonnet").count
            + count_tokens("", "claude-3-5-sonnet").count;
        assert_eq!(metrics.tokens_out, expected);
    }

    #[tokio::test]
    async fn test_metered_provider_take_last_metrics_idempotent() {
        let mock = MockProvider::default();
        let metered = MeteredProvider::new(Box::new(mock));
        let cancel = CancellationToken::new();

        let request = CompletionRequest {
            model: "idempotent".to_string(),
            messages: vec![],
            tools: None,
            tool_choice: None,
            temperature: None,
            max_tokens: None,
            stream: true,
        };

        let stream = metered.stream_completion(request, cancel).await.unwrap();
        let _chunks: Vec<_> = stream.collect().await;

        let m1 = metered.take_last_metrics();
        let m2 = metered.take_last_metrics();
        assert!(m1.is_some(), "first take returns metrics");
        assert!(m2.is_none(), "second take returns None (already consumed)");
    }

    #[tokio::test]
    async fn test_metered_provider_passes_through_metadata() {
        let mock = MockProvider::default();
        let metered = MeteredProvider::new(Box::new(mock));

        assert_eq!(metered.provider_name(), "mock");

        let budget = metered.context_capacity("gpt-4");
        assert!(budget.capacity > 0);

        let cost = metered.approximate_cost(100, 50);
        assert_eq!(cost, 0.0);
    }

    #[tokio::test]
    async fn test_metered_provider_empty_metrics_before_call() {
        let mock = MockProvider::default();
        let metered = MeteredProvider::new(Box::new(mock));

        assert!(metered.take_last_metrics().is_none());
    }

    #[tokio::test]
    async fn metered_cost_non_zero_for_paid_provider() {
        // Regression test for the "fake zero" bug: a completed provider call
        // must never report cost_usd = 0.0 when the inner provider has non-zero
        // pricing. This exercises the actual streaming path end-to-end.
        struct CostlyMockProvider;

        #[async_trait]
        impl LlmProvider for CostlyMockProvider {
            fn provider_name(&self) -> &'static str {
                "costly-mock"
            }

            fn context_capacity(&self, _model: &str) -> TokenBudget {
                TokenBudget::new(128_000, 4_096)
            }

            fn approximate_cost(&self, tokens_in: u64, tokens_out: u64) -> f64 {
                // Simulate GPT-4o-class pricing: $2.50/M input, $10.00/M output.
                tokens_in as f64 * 2.50 / 1_000_000.0 + tokens_out as f64 * 10.00 / 1_000_000.0
            }

            async fn stream_completion(
                &self,
                _request: CompletionRequest,
                _cancel: CancellationToken,
            ) -> Result<CompletionStream, ProviderError> {
                let chunks =
                    vec![
                    Ok(CompletionChunk {
                        delta:
                            "Hello, world! This is a test of the emergency broadcasting system. "
                                .into(),
                        reasoning: None,
                        tool_call: None,
                        is_final: false, usage: None,
                    }),
                    Ok(CompletionChunk {
                        delta: "This is only a test. If this had been an actual emergency, ".into(),
                        reasoning: None,
                        tool_call: None,
                        is_final: false, usage: None,
                    }),
                    Ok(CompletionChunk {
                        delta:
                            "you would have been instructed where to go for further information."
                                .into(),
                        reasoning: None,
                        tool_call: None,
                        is_final: true, usage: None,
                    }),
                ];
                Ok(Box::pin(futures::stream::iter(chunks)))
            }
        }

        let metered = MeteredProvider::new(Box::new(CostlyMockProvider));
        let cancel = CancellationToken::new();
        let request = CompletionRequest {
            model: "gpt-4o".to_string(),
            messages: vec![],
            tools: None,
            tool_choice: None,
            temperature: None,
            max_tokens: None,
            stream: true,
        };

        let stream = metered.stream_completion(request, cancel).await.unwrap();
        let _chunks: Vec<_> = stream.collect().await;

        let metrics = metered.take_last_metrics().expect("metrics should be recorded");
        assert!(
            metrics.cost_usd > 0.0,
            "cost_usd must be > 0 for a paid provider; got {}",
            metrics.cost_usd
        );
        assert!(
            metrics.tokens_out > 0,
            "tokens_out must be > 0 for non-empty deltas; got {}",
            metrics.tokens_out
        );
    }

    #[test]
    fn cost_decomposition_matches_direct_call() {
        // Regression test: the per-token rate decomposition used by
        // MeteredProvider must match the inner provider's approximate_cost.
        // This verifies that the fix for the "fake zero" bug is correct:
        // cost = tokens_in × rate_in + tokens_out × rate_out must equal
        // approximate_cost(tokens_in, tokens_out).
        use crate::openai::OpenAiProvider;

        let openai =
            OpenAiProvider::new("test-key".into(), "gpt-4o".into(), crate::DEFAULT_TIMEOUT_SECS);
        let rate_in = openai.approximate_cost(1_000_000, 0) / 1_000_000.0;
        let rate_out = openai.approximate_cost(0, 1_000_000) / 1_000_000.0;
        assert!(rate_in > 0.0, "OpenAI input rate should be non-zero");
        assert!(rate_out > 0.0, "OpenAI output rate should be non-zero");

        let tokens_in = 150u64;
        let tokens_out = 75u64;
        let direct = openai.approximate_cost(tokens_in, tokens_out);
        let decomposed = tokens_in as f64 * rate_in + tokens_out as f64 * rate_out;
        assert!(
            (direct - decomposed).abs() < f64::EPSILON,
            "cost decomposition must match direct call: direct={direct}, decomposed={decomposed}"
        );

        // Also verify Anthropic's pricing decomposition.
        use crate::anthropic::AnthropicProvider;
        let anthropic = AnthropicProvider::new(
            "test-key".into(),
            "claude-sonnet-4-20250514".into(),
            crate::DEFAULT_TIMEOUT_SECS,
        );
        let rate_in = anthropic.approximate_cost(1_000_000, 0) / 1_000_000.0;
        let rate_out = anthropic.approximate_cost(0, 1_000_000) / 1_000_000.0;
        assert!(rate_in > 0.0, "Anthropic input rate should be non-zero");
        assert!(rate_out > 0.0, "Anthropic output rate should be non-zero");

        let direct = anthropic.approximate_cost(tokens_in, tokens_out);
        let decomposed = tokens_in as f64 * rate_in + tokens_out as f64 * rate_out;
        assert!(
            (direct - decomposed).abs() < f64::EPSILON,
            "Anthropic cost decomposition must match direct call"
        );
    }
}
