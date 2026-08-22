//! Production [`LLMSummarizer`] that delegates summarization to the configured
//! LLM provider.
//!
//! Wraps an [`Arc<dyn LlmProvider>`] and issues a non-streaming completion
//! using the provider's default model.  Used by [`SummarizeOldest`] to
//! compress overflowing conversation history into a compact summary.
//!
//! Stream creation and collection are bounded by the same timeouts the
//! orchestrator applies to agent requests (60s time-to-first-byte and 120s
//! stream-idle, from [`RetryConfig`] defaults), so a provider that accepts the
//! request but never responds cannot stall a session indefinitely.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use concerto_config::RetryConfig;
use concerto_core::error::{MemoryError, ProviderError};
use concerto_core::traits::provider::LlmProvider;
use concerto_core::types::{CompletionRequest, Message, Role};
use concerto_core::CancellationToken;

/// A summarizer that delegates to an LLM provider.
///
/// Constructed once per agent run and reused for each overflow event.
/// Uses the provider's built-in completion via `collect_stream_with_timeouts`.
pub struct ProviderSummarizer {
    provider: Arc<dyn LlmProvider>,
    model: String,
    cancel: CancellationToken,
}

impl ProviderSummarizer {
    /// Create a new provider-backed summarizer.
    ///
    /// `model` should be the model ID to use for summarization (e.g.,
    /// `"gpt-4o-mini"` or the agent's active model).  Passing a short,
    /// cheap model is recommended since summaries are informational
    /// and do not drive agent behaviour.
    ///
    /// `cancel` is the cancellation token from the agent run so that
    /// summarization is cancelled when the user cancels the run.
    pub fn new(provider: Arc<dyn LlmProvider>, model: String, cancel: CancellationToken) -> Self {
        Self { provider, model, cancel }
    }
}

#[async_trait]
impl concerto_memory::summarizer::LLMSummarizer for ProviderSummarizer {
    async fn summarize(&self, messages: &[Message], prompt: &str) -> Result<String, MemoryError> {
        let system_msg = Message {
            role: Role::System,
            content: prompt.to_string(),
            tool_calls: None,
            tool_results: None,
            reasoning_content: None,
            tokens_in: None,
            tokens_out: None,
        };
        let mut request_messages = vec![system_msg];
        request_messages.extend_from_slice(messages);

        let request = CompletionRequest {
            model: self.model.clone(),
            messages: request_messages,
            tools: None,
            tool_choice: None,
            temperature: Some(0.3),
            max_tokens: Some(1024),
            stream: true,
        };

        // Bound stream creation and collection with the same timeouts the
        // orchestrator uses for agent requests (from RetryConfig defaults), so
        // a hung provider cannot stall summarization indefinitely.
        let retry_config = RetryConfig::default();
        let ttfb = Duration::from_secs(retry_config.time_to_first_byte_seconds);
        let idle = Duration::from_secs(retry_config.stream_idle_timeout_seconds);

        let stream = tokio::time::timeout(
            ttfb,
            self.provider.stream_completion(request, self.cancel.clone()),
        )
        .await
        .map_err(|_| ProviderError::Timeout { phase: "time-to-first-byte", timeout: ttfb })
        .map_err(|e| MemoryError::RetrievalFailed(format!("summarization completion failed: {e}")))?
        .map_err(|e| {
            MemoryError::RetrievalFailed(format!("summarization completion failed: {e}"))
        })?;

        // Collect the stream into text. We discard tool calls, reasoning, and
        // usage (a summarization is not charged to the session transcript).
        let (text, _, _tool_calls, _usage) =
            crate::prompts::collect_stream_with_timeouts(stream, &self.cancel, ttfb, idle)
                .await
                .map_err(|e| {
                    MemoryError::RetrievalFailed(format!("summarization stream failed: {e}"))
                })?;

        Ok(text)
    }
}
