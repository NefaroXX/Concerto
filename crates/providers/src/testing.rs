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
