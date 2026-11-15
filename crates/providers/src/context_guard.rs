//! Provider-boundary context validation and deterministic request materialization.
//!
//! Every production provider created by [`crate::factory::ProviderFactory`] is
//! wrapped by [`ContextGuardProvider`]. The wrapper validates the complete
//! outgoing request immediately before the network call, regardless of which
//! orchestrator path produced it.
//!
//! Reduction is performed only on the owned request passed into this wrapper.
//! It never mutates session storage or caller-owned message collections.

use std::sync::Arc;

use async_trait::async_trait;
use concerto_core::error::ProviderError;
use concerto_core::traits::provider::{CompletionStream, LlmProvider};
use concerto_core::types::{CompletionRequest, Message, ModelInfo, Role, TokenBudget};
use concerto_core::CancellationToken;

const DEFAULT_OUTPUT_RESERVE: u64 = 4_096;
const MIN_SAFETY_MARGIN: u64 = 512;
const NORMAL_SAFETY_PERCENT: u64 = 5;
const RETRY_SAFETY_PERCENT: u64 = 10;
const RECENT_TOOL_MESSAGES: usize = 2;
const RETRIEVED_MEMORY_START: &str = "<retrieved_project_memory>";
const RETRIEVED_MEMORY_END: &str = "</retrieved_project_memory>";
#[cfg(test)]
const WORKING_MEMORY_START: &str = "<working_memory>";
#[cfg(test)]
const WORKING_MEMORY_END: &str = "</working_memory>";
#[cfg(test)]
const RUN_MEMORY_START: &str = "<orchestration_run_state>";
#[cfg(test)]
const RUN_MEMORY_END: &str = "</orchestration_run_state>";
#[cfg(test)]
const PREVIOUS_RESULTS_START: &str = "<previous_agent_results>";
#[cfg(test)]
const PREVIOUS_RESULTS_END: &str = "</previous_agent_results>";
const CHANGED_FILE_CONTEXT_START: &str = "<changed_file_context>";
const CHANGED_FILE_CONTEXT_END: &str = "</changed_file_context>";
#[cfg(test)]
const CONVERSATION_HISTORY_START: &str = "<conversation_history>";
#[cfg(test)]
const CONVERSATION_HISTORY_END: &str = "</conversation_history>";
const COMPACTED_TOOL_CONTENT: &str =
    "[Older tool output compacted from this provider request. The source request remains unchanged.]";

/// Wraps an LLM provider and enforces a final request-size invariant.
pub struct ContextGuardProvider {
    inner: Arc<dyn LlmProvider>,
    default_model: String,
}

impl ContextGuardProvider {
    pub fn new(inner: Arc<dyn LlmProvider>, default_model: impl Into<String>) -> Self {
        Self { inner, default_model: default_model.into() }
    }

    fn prepare_request(
        &self,
        mut request: CompletionRequest,
        safety_percent: u64,
    ) -> Result<CompletionRequest, ProviderError> {
        if request.model.trim().is_empty() {
            request.model = self.default_model.clone();
        }

        let budget = self.inner.context_capacity(&request.model);
        let output_reserve = request
            .max_tokens
            .unwrap_or_else(|| budget.reserved_for_response.max(DEFAULT_OUTPUT_RESERVE))
            .max(budget.reserved_for_response);
        let safety_margin = budget
            .capacity
            .saturating_mul(safety_percent)
            .saturating_div(100)
            .max(MIN_SAFETY_MARGIN);
        let input_limit =
            budget.capacity.saturating_sub(output_reserve).saturating_sub(safety_margin);

        let original_tokens = estimate_request_tokens(&request);
        if original_tokens <= input_limit {
            return Ok(request);
        }

        compact_old_tool_outputs(&mut request.messages);

        let retrieved_memory_limit = input_limit / 4;
        let changed_file_context_limit = input_limit / 3;
        // Markers inside user/assistant text are not provenance. In particular,
        // earlier instructions and commitments cannot become optional merely
        // because their text resembles one of our generated context blocks.
        // Live run state, working memory and handoffs are mandatory even in
        // system messages; only retrieved/file evidence has a reduction budget.
        for message in request.messages.iter_mut().filter(|message| message.role == Role::System) {
            reduce_marked_block(
                &mut message.content,
                RETRIEVED_MEMORY_START,
                RETRIEVED_MEMORY_END,
                retrieved_memory_limit,
                "Retrieved project memory clipped to its request budget.",
            );
            reduce_marked_block(
                &mut message.content,
                CHANGED_FILE_CONTEXT_START,
                CHANGED_FILE_CONTEXT_END,
                changed_file_context_limit,
                "Changed file context clipped to its request budget.",
            );
        }

        // ADR-85: no dropping conversation groups or clipping their prose.
        // When required instructions cannot fit, fail before the inner call.
        let final_tokens = estimate_request_tokens(&request);
        if final_tokens > input_limit {
            tracing::warn!(
                provider = self.inner.provider_name(),
                model = %request.model,
                original_tokens,
                final_tokens,
                input_limit,
                capacity = budget.capacity,
                output_reserve,
                safety_margin,
                "request still exceeds context budget after deterministic reduction"
            );
            return Err(ProviderError::ContextOverflow {
                tokens_in: final_tokens.saturating_add(output_reserve),
                capacity: budget.capacity,
            });
        }

        tracing::warn!(
            provider = self.inner.provider_name(),
            model = %request.model,
            original_tokens,
            final_tokens,
            input_limit,
            "materialized a reduced request before provider call"
        );
        Ok(request)
    }
}

#[async_trait]
impl LlmProvider for ContextGuardProvider {
    async fn stream_completion(
        &self,
        request: CompletionRequest,
        cancel: CancellationToken,
    ) -> Result<CompletionStream, ProviderError> {
        let original = request.clone();
        let prepared = self.prepare_request(request, NORMAL_SAFETY_PERCENT)?;
        match self.inner.stream_completion(prepared, cancel.clone()).await {
            Err(ProviderError::ContextOverflow { .. }) => {
                tracing::warn!(
                    provider = self.inner.provider_name(),
                    "provider rejected estimated context; retrying once with a larger safety margin"
                );
                let prepared = self.prepare_request(original, RETRY_SAFETY_PERCENT)?;
                self.inner.stream_completion(prepared, cancel).await
            }
            result => result,
        }
    }

    fn context_capacity(&self, model: &str) -> TokenBudget {
        let model = if model.trim().is_empty() { &self.default_model } else { model };
        self.inner.context_capacity(model)
    }

    fn approximate_cost(&self, tokens_in: u64, tokens_out: u64) -> f64 {
        self.inner.approximate_cost(tokens_in, tokens_out)
    }

    fn provider_name(&self) -> &'static str {
        self.inner.provider_name()
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

fn estimate_request_tokens(request: &CompletionRequest) -> u64 {
    let messages = request.messages.iter().map(estimate_message_tokens).sum::<u64>();
    let tools = request
        .tools
        .as_ref()
        .and_then(|tools| serde_json::to_vec(tools).ok())
        .map_or(0, |bytes| bytes.len().div_ceil(4) as u64);
    messages.saturating_add(tools)
}

fn estimate_message_tokens(message: &Message) -> u64 {
    // Structured tool results are the canonical model-history
    // representation. Display text, when present for older sessions, is not
    // serialized by the provider adapters and must not be counted twice.
    let content = if message.role == Role::Tool && message.tool_results.is_some() {
        0
    } else {
        message.content.len().div_ceil(4) as u64
    };
    let calls = message
        .tool_calls
        .as_ref()
        .and_then(|calls| serde_json::to_vec(calls).ok())
        .map_or(0, |bytes| bytes.len().div_ceil(4) as u64);
    let results = message
        .tool_results
        .as_ref()
        .and_then(|results| serde_json::to_vec(results).ok())
        .map_or(0, |bytes| bytes.len().div_ceil(4) as u64);
    // OpenAI-compatible adapters replay this field. Omitting it lets a
    // short visible answer hide a request-sized reasoning payload.
    let reasoning =
        message.reasoning_content.as_ref().map_or(0, |text| text.len().div_ceil(4) as u64);
    content
        .saturating_add(reasoning)
        .saturating_add(calls)
        .saturating_add(results)
        .saturating_add(4)
}

fn compact_old_tool_outputs(messages: &mut [Message]) {
    let tool_indices = messages
        .iter()
        .enumerate()
        .filter_map(|(index, message)| (message.role == Role::Tool).then_some(index))
        .collect::<Vec<_>>();
    let compact_count = tool_indices.len().saturating_sub(RECENT_TOOL_MESSAGES);

    for index in tool_indices.into_iter().take(compact_count) {
        let message = &mut messages[index];
        message.content = COMPACTED_TOOL_CONTENT.to_string();
        if let Some(results) = &mut message.tool_results {
            for result in results {
                result.content = serde_json::json!({
                    "status": "compacted",
                    "note": "Older payload removed only from this materialized provider request."
                });
            }
        }
    }
}

fn reduce_marked_block(
    content: &mut String,
    start_marker: &str,
    end_marker: &str,
    token_limit: u64,
    clipped_note: &str,
) {
    let Some(start) = content.find(start_marker) else {
        return;
    };
    let body_start = start + start_marker.len();
    let Some(relative_end) = content[body_start..].find(end_marker) else {
        return;
    };
    let body_end = body_start + relative_end;
    let block_end = body_end + end_marker.len();
    if (block_end - start).div_ceil(4) as u64 <= token_limit {
        return;
    }

    let max_bytes = token_limit.saturating_mul(4) as usize;
    let suffix = format!("\n[{clipped_note}]");
    let fixed_bytes = start_marker.len() + end_marker.len() + suffix.len();
    let body_budget = max_bytes.saturating_sub(fixed_bytes);
    let body = clip_utf8(&content[body_start..body_end], body_budget);
    let replacement = format!("{start_marker}{body}{suffix}{end_marker}");
    content.replace_range(start..block_end, &replacement);
}

fn clip_utf8(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_string();
    }
    let mut end = max_bytes.min(value.len());
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_string()
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use concerto_core::types::{CompletionChunk, ToolResult};
    use futures::stream;

    use super::*;

    struct CaptureProvider {
        captured: Arc<Mutex<Option<CompletionRequest>>>,
        capacity: u64,
        reserve: u64,
    }

    #[async_trait]
    impl LlmProvider for CaptureProvider {
        async fn stream_completion(
            &self,
            request: CompletionRequest,
            _cancel: CancellationToken,
        ) -> Result<CompletionStream, ProviderError> {
            *self.captured.lock().unwrap_or_else(|error| error.into_inner()) = Some(request);
            Ok(Box::pin(stream::iter(vec![Ok(CompletionChunk {
                reasoning: None,
                delta: String::new(),
                tool_call: None,
                is_final: true,
                usage: None,
            })])))
        }

        fn context_capacity(&self, _model: &str) -> TokenBudget {
            TokenBudget::new(self.capacity, self.reserve)
        }

        fn approximate_cost(&self, _tokens_in: u64, _tokens_out: u64) -> f64 {
            0.0
        }

        fn provider_name(&self) -> &'static str {
            "capture"
        }
    }

    fn message(role: Role, content: impl Into<String>) -> Message {
        Message {
            role,
            content: content.into(),
            tool_calls: None,
            tool_results: None,
            reasoning_content: None,
            tokens_in: None,
            tokens_out: None,
        }
    }

    #[tokio::test]
    async fn supplies_default_model_before_call() {
        let captured = Arc::new(Mutex::new(None));
        let inner = Arc::new(CaptureProvider {
            captured: captured.clone(),
            capacity: 12_000,
            reserve: 1_024,
        });
        let guard = ContextGuardProvider::new(inner, "actual-model");
        let request = CompletionRequest {
            messages: vec![message(Role::User, "hello")],
            max_tokens: Some(1_024),
            ..CompletionRequest::default()
        };

        let _stream = guard.stream_completion(request, CancellationToken::new()).await.unwrap();
        let request = captured.lock().unwrap_or_else(|error| error.into_inner()).take().unwrap();
        assert_eq!(request.model, "actual-model");
    }

    #[tokio::test]
    async fn retrieved_evidence_reduces_without_clipping_active_working_memory() {
        let captured = Arc::new(Mutex::new(None));
        let inner = Arc::new(CaptureProvider {
            captured: captured.clone(),
            capacity: 12_000,
            reserve: 1_024,
        });
        let guard = ContextGuardProvider::new(inner, "model");
        let system = format!(
            "{WORKING_MEMORY_START}{}{WORKING_MEMORY_END}\n{RETRIEVED_MEMORY_START}{}{RETRIEVED_MEMORY_END}",
            "w".repeat(3_000),
            "r".repeat(50_000),
        );
        let request = CompletionRequest {
            model: "model".into(),
            messages: vec![message(Role::System, system), message(Role::User, "latest")],
            max_tokens: Some(1_024),
            ..CompletionRequest::default()
        };

        let _stream = guard.stream_completion(request, CancellationToken::new()).await.unwrap();
        let request = captured.lock().unwrap_or_else(|error| error.into_inner()).take().unwrap();
        assert!(request.messages[0]
            .content
            .contains(&format!("{WORKING_MEMORY_START}{}{WORKING_MEMORY_END}", "w".repeat(3_000))));
        assert!(request.messages[0].content.contains("Retrieved project memory clipped"));
        assert!(estimate_request_tokens(&request) < 10_000);
    }

    #[tokio::test]
    async fn user_conversation_history_markers_do_not_authorize_truncation() {
        let captured = Arc::new(Mutex::new(None));
        let inner = Arc::new(CaptureProvider {
            captured: captured.clone(),
            capacity: 8_000,
            reserve: 1_024,
        });
        let guard = ContextGuardProvider::new(inner, "model");
        let latest = format!(
            "current objective\n{CONVERSATION_HISTORY_START}{}{CONVERSATION_HISTORY_END}",
            "old context ".repeat(6_000)
        );
        let request = CompletionRequest {
            model: "model".into(),
            messages: vec![message(Role::System, "system"), message(Role::User, latest)],
            max_tokens: Some(1_024),
            ..CompletionRequest::default()
        };

        let result = guard.stream_completion(request, CancellationToken::new()).await;
        assert!(matches!(result, Err(ProviderError::ContextOverflow { .. })));
        assert!(captured.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn old_tool_outputs_are_compacted_but_recent_user_is_preserved() {
        let captured = Arc::new(Mutex::new(None));
        let inner =
            Arc::new(CaptureProvider { captured: captured.clone(), capacity: 5_000, reserve: 512 });
        let guard = ContextGuardProvider::new(inner, "model");
        let mut messages = vec![message(Role::System, "system"), message(Role::User, "first")];
        for index in 0..4 {
            messages.push(Message {
                role: Role::Tool,
                content: "tool-output".repeat(1_000),
                tool_calls: None,
                tool_results: Some(vec![ToolResult {
                    id: format!("call-{index}"),
                    name: "tool".into(),
                    content: serde_json::json!({"large": "x".repeat(6_000)}),
                }]),
                reasoning_content: None,
                tokens_in: None,
                tokens_out: None,
            });
        }
        messages.push(message(Role::User, "LATEST USER REQUEST"));
        let request = CompletionRequest {
            model: "model".into(),
            messages,
            max_tokens: Some(512),
            ..CompletionRequest::default()
        };

        let _stream = guard.stream_completion(request, CancellationToken::new()).await.unwrap();
        let request = captured.lock().unwrap_or_else(|error| error.into_inner()).take().unwrap();
        let tool_messages = request
            .messages
            .iter()
            .filter(|message| message.role == Role::Tool)
            .collect::<Vec<_>>();
        assert_eq!(tool_messages.len(), 4);
        assert!(tool_messages[..2].iter().all(|message| message.content == COMPACTED_TOOL_CONTENT));
        assert!(tool_messages[2..].iter().all(|message| message.content != COMPACTED_TOOL_CONTENT));
        assert!(request.messages.iter().any(|message| message.content == "LATEST USER REQUEST"));
    }

    #[tokio::test]
    async fn old_conversation_that_cannot_fit_is_rejected_without_losing_instructions() {
        let captured = Arc::new(Mutex::new(None));
        let inner =
            Arc::new(CaptureProvider { captured: captured.clone(), capacity: 4_000, reserve: 512 });
        let guard = ContextGuardProvider::new(inner, "model");
        let mut messages = vec![message(Role::System, "system")];
        for turn in 0..8 {
            messages.push(message(Role::User, format!("OLD USER {turn} {}", "u".repeat(1_200))));
            messages.push(message(
                Role::Assistant,
                format!("OLD ASSISTANT {turn} {}", "a".repeat(1_200)),
            ));
        }
        messages.push(message(Role::User, "LATEST USER REQUEST"));
        let request = CompletionRequest {
            model: "model".into(),
            messages,
            max_tokens: Some(512),
            ..CompletionRequest::default()
        };

        let result = guard.stream_completion(request, CancellationToken::new()).await;
        assert!(matches!(result, Err(ProviderError::ContextOverflow { .. })));
        assert!(captured.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn mandatory_system_content_that_cannot_fit_is_rejected() {
        let captured = Arc::new(Mutex::new(None));
        let inner = Arc::new(CaptureProvider { captured, capacity: 2_048, reserve: 512 });
        let guard = ContextGuardProvider::new(inner, "model");
        let request = CompletionRequest {
            model: "model".into(),
            messages: vec![
                message(Role::System, "x".repeat(20_000)),
                message(Role::User, "latest"),
            ],
            max_tokens: Some(512),
            ..CompletionRequest::default()
        };

        let result = guard.stream_completion(request, CancellationToken::new()).await;
        assert!(matches!(result, Err(ProviderError::ContextOverflow { .. })));
    }

    #[tokio::test]
    async fn replay_reasoning_that_exceeds_budget_is_rejected_before_provider_call() {
        let captured = Arc::new(Mutex::new(None));
        let inner =
            Arc::new(CaptureProvider { captured: captured.clone(), capacity: 2_048, reserve: 512 });
        let guard = ContextGuardProvider::new(inner, "model");
        let mut assistant = message(Role::Assistant, "brief answer");
        assistant.reasoning_content = Some("🦀".repeat(1_500));
        let request = CompletionRequest {
            messages: vec![
                message(Role::User, "first"),
                assistant,
                message(Role::User, "continue"),
            ],
            max_tokens: Some(512),
            ..CompletionRequest::default()
        };
        let result = guard.stream_completion(request, CancellationToken::new()).await;
        assert!(matches!(result, Err(ProviderError::ContextOverflow { .. })));
        assert!(
            captured.lock().unwrap().is_none(),
            "an oversized request must not reach the provider"
        );
    }

    #[tokio::test]
    async fn replay_reasoning_within_budget_is_preserved_despite_large_usage() {
        let captured = Arc::new(Mutex::new(None));
        let inner =
            Arc::new(CaptureProvider { captured: captured.clone(), capacity: 2_048, reserve: 512 });
        let guard = ContextGuardProvider::new(inner, "model");
        let mut assistant = message(Role::Assistant, "brief answer");
        assistant.reasoning_content = Some("🦀".repeat(100));
        assistant.tokens_in = Some(u64::MAX);
        assistant.tokens_out = Some(u64::MAX);
        let messages =
            vec![message(Role::User, "first"), assistant, message(Role::User, "continue")];
        let request = CompletionRequest {
            messages: messages.clone(),
            max_tokens: Some(512),
            ..CompletionRequest::default()
        };
        let _stream = guard.stream_completion(request, CancellationToken::new()).await.unwrap();
        let actual = captured.lock().unwrap().take().unwrap();
        assert_eq!(
            serde_json::to_value(actual.messages).unwrap(),
            serde_json::to_value(messages).unwrap()
        );
    }

    // Keep the regression fixture private to this test module.
    include!("context_guard_regressions.rs");
}
