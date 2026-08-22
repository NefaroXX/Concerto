//! Context budget allocator for RAG and working memory.
//!
//! Replaces ad-hoc percentage calculations with a tested, configurable
//! struct that enforces allocation limits.

use concerto_core::error::MemoryError;
use concerto_core::memory::MemoryChunk;
use concerto_core::types::Message;

const CLIPPED_SUFFIX: &str = "\n[Content clipped to fit the context budget.]";

/// Allocates context window capacity across RAG, working memory, and
/// conversation history.
///
/// Default split:
/// - 25% RAG chunks
/// - 10% working memory
/// - 65% conversation history
pub struct ContextBudgetAllocator {
    /// Fraction of total capacity reserved for RAG chunks (default 0.25).
    pub rag_pct: f64,
    /// Fraction of total capacity reserved for working memory (default 0.10).
    pub working_mem_pct: f64,
}

impl Default for ContextBudgetAllocator {
    fn default() -> Self {
        Self { rag_pct: 0.25, working_mem_pct: 0.10 }
    }
}

impl ContextBudgetAllocator {
    pub fn new(rag_pct: f64, working_mem_pct: f64) -> Result<Self, MemoryError> {
        if !rag_pct.is_finite()
            || !working_mem_pct.is_finite()
            || rag_pct < 0.0
            || working_mem_pct < 0.0
            || rag_pct + working_mem_pct >= 1.0
        {
            return Err(MemoryError::Persistence(
                "RAG and working-memory percentages must be finite, non-negative, and leave room for history".into(),
            ));
        }
        Ok(Self { rag_pct, working_mem_pct })
    }

    /// Token limit for RAG chunks given total capacity.
    pub fn rag_limit(&self, capacity: u64) -> u64 {
        (capacity as f64 * self.rag_pct) as u64
    }

    /// Token limit for working memory.
    pub fn working_mem_limit(&self, capacity: u64) -> u64 {
        (capacity as f64 * self.working_mem_pct) as u64
    }

    /// Select and, when necessary, clip chunks to the aggregate RAG budget.
    /// Highest-scored chunks are retained first; one oversized chunk can no
    /// longer consume the complete provider context.
    pub fn truncate_to_rag_limit(
        &self,
        mut chunks: Vec<MemoryChunk>,
        capacity: u64,
    ) -> Vec<MemoryChunk> {
        let limit = self.rag_limit(capacity);
        if chunks.is_empty() || limit == 0 {
            return Vec::new();
        }

        chunks.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
        let mut selected = Vec::new();
        let mut used = 0u64;

        for mut chunk in chunks {
            let remaining = limit.saturating_sub(used);
            if remaining == 0 {
                break;
            }

            let estimated = estimate_text_tokens(&chunk.content);
            if estimated > remaining {
                chunk.content = clip_text_to_tokens(&chunk.content, remaining);
            }

            let retained = estimate_text_tokens(&chunk.content);
            if retained == 0 || retained > remaining {
                break;
            }
            used = used.saturating_add(retained);
            selected.push(chunk);
        }

        selected
    }

    /// Assemble final ordered message list respecting all budgets.
    pub fn build_context(
        &self,
        rag_chunks: Vec<MemoryChunk>,
        mut working_mem_block: Message,
        history: &[Message],
        capacity: u64,
    ) -> Vec<Message> {
        let mut messages = Vec::new();

        // 1. RAG context as system messages.
        for chunk in self.truncate_to_rag_limit(rag_chunks, capacity) {
            messages.push(Message {
                role: concerto_core::types::Role::System,
                content: format!("[Context]\n{}", chunk.content),
                tool_calls: None,
                tool_results: None,
                reasoning_content: None,
                tokens_in: None,
                tokens_out: None,
            });
        }

        // 2. Working memory is independently bounded before insertion.
        let working_limit = self.working_mem_limit(capacity);
        if !working_mem_block.content.is_empty() && working_limit > 0 {
            working_mem_block.content =
                clip_text_to_tokens(&working_mem_block.content, working_limit);
            messages.push(working_mem_block);
        }

        // 3. Use the actual consumed tokens and keep the newest history that
        // fits. The old implementation reserved 10% and then inserted an
        // unlimited block, while also subtracting the reservation twice.
        let used = messages.iter().map(estimate_message_tokens).sum::<u64>();
        let remaining = capacity.saturating_sub(used);
        let mut retained_history = Vec::new();
        let mut history_tokens = 0u64;
        for message in history.iter().rev() {
            let estimated = estimate_message_tokens(message);
            if history_tokens.saturating_add(estimated) > remaining {
                break;
            }
            history_tokens = history_tokens.saturating_add(estimated);
            retained_history.push(message.clone());
        }
        retained_history.reverse();
        messages.extend(retained_history);

        messages
    }
}

fn estimate_text_tokens(value: &str) -> u64 {
    value.len().div_ceil(4) as u64
}

fn estimate_message_tokens(message: &Message) -> u64 {
    let content = estimate_text_tokens(&message.content);
    let calls = message
        .tool_calls
        .as_ref()
        .and_then(|value| serde_json::to_vec(value).ok())
        .map_or(0, |value| value.len().div_ceil(4) as u64);
    let results = message
        .tool_results
        .as_ref()
        .and_then(|value| serde_json::to_vec(value).ok())
        .map_or(0, |value| value.len().div_ceil(4) as u64);
    content.saturating_add(calls).saturating_add(results).saturating_add(4)
}

fn clip_text_to_tokens(value: &str, token_limit: u64) -> String {
    let max_bytes = token_limit.saturating_mul(4) as usize;
    if value.len() <= max_bytes {
        return value.to_string();
    }
    if max_bytes == 0 {
        return String::new();
    }

    let suffix_bytes = CLIPPED_SUFFIX.len().min(max_bytes);
    let target = max_bytes.saturating_sub(suffix_bytes);
    let mut end = target.min(value.len());
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }

    let mut clipped = value[..end].to_string();
    let suffix_start = CLIPPED_SUFFIX.len().saturating_sub(suffix_bytes);
    clipped.push_str(&CLIPPED_SUFFIX[suffix_start..]);
    clipped
}

#[cfg(test)]
mod tests {
    use super::*;
    use concerto_core::memory::{ChunkType, MemoryNamespace, ProjectId};
    use concerto_core::types::Role;

    fn make_chunk(content: &str, score: f64) -> MemoryChunk {
        MemoryChunk {
            id: "test".into(),
            project_id: ProjectId("test".into()),
            namespace: MemoryNamespace::Project(ProjectId("test".into())),
            content: content.into(),
            file_path: None,
            start_line: None,
            end_line: None,
            chunk_type: ChunkType::SlidingWindow,
            score,
            model_id: "test".into(),
            model_version: "1.0".into(),
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

    fn message_with_tool_calls(role: Role, content: impl Into<String>) -> Message {
        use concerto_core::types::{ToolCall, ToolResult};
        Message {
            role,
            content: content.into(),
            tool_calls: Some(vec![ToolCall {
                id: "call_1".into(),
                name: "test".into(),
                arguments: serde_json::json!({}),
            }]),
            tool_results: Some(vec![ToolResult {
                id: "call_1".into(),
                name: "test".into(),
                content: serde_json::json!({"success": true}),
            }]),
            reasoning_content: None,
            tokens_in: None,
            tokens_out: None,
        }
    }

    #[test]
    fn default_allocation() {
        let alloc = ContextBudgetAllocator::default();
        assert_eq!(alloc.rag_limit(10_000), 2_500);
        assert_eq!(alloc.working_mem_limit(10_000), 1_000);
    }

    #[test]
    fn new_rejects_invalid_allocations() {
        assert!(ContextBudgetAllocator::new(0.6, 0.5).is_err());
        assert!(ContextBudgetAllocator::new(-0.1, 0.1).is_err());
        assert!(ContextBudgetAllocator::new(f64::NAN, 0.1).is_err());
    }

    #[test]
    fn new_total_exactly_1_0_errors() {
        // Sum exactly 1.0 (rag + working >= 1.0 triggers error)
        assert!(ContextBudgetAllocator::new(0.5, 0.5).is_err());
        assert!(ContextBudgetAllocator::new(0.99, 0.01).is_err());
    }

    #[test]
    fn new_accepts_valid_zero_percentages() {
        let alloc = ContextBudgetAllocator::new(0.0, 0.0).unwrap();
        assert_eq!(alloc.rag_limit(10_000), 0);
        assert_eq!(alloc.working_mem_limit(10_000), 0);
    }

    #[test]
    fn new_accepts_valid_boundary_below_one() {
        let alloc = ContextBudgetAllocator::new(0.5, 0.49).unwrap();
        assert_eq!(alloc.rag_limit(10_000), 5_000);
        assert_eq!(alloc.working_mem_limit(10_000), 4_900);
    }

    #[test]
    fn truncate_keeps_highest_scores() {
        let alloc = ContextBudgetAllocator::default();
        let chunks = vec![
            make_chunk(&"a".repeat(80), 0.9),
            make_chunk(&"b".repeat(80), 0.1),
            make_chunk(&"c".repeat(80), 0.5),
        ];
        let selected = alloc.truncate_to_rag_limit(chunks, 200);
        assert!(!selected.is_empty());
        assert_eq!(selected[0].score, 0.9);
        assert!(
            selected.iter().map(|chunk| estimate_text_tokens(&chunk.content)).sum::<u64>() <= 50
        );
    }

    #[test]
    fn truncate_empty_chunks_returns_empty() {
        let alloc = ContextBudgetAllocator::default();
        let selected = alloc.truncate_to_rag_limit(vec![], 1_000);
        assert!(selected.is_empty());
    }

    #[test]
    fn truncate_all_fit_within_large_budget() {
        let alloc = ContextBudgetAllocator::default();
        let chunks = vec![
            make_chunk(&"a".repeat(40), 0.9),
            make_chunk(&"b".repeat(40), 0.8),
            make_chunk(&"c".repeat(40), 0.7),
        ];
        let selected = alloc.truncate_to_rag_limit(chunks, 10_000);
        assert_eq!(selected.len(), 3);
    }

    #[test]
    fn truncate_stops_when_budget_exhausted() {
        let alloc = ContextBudgetAllocator::default();
        let chunks = vec![
            make_chunk(&"a".repeat(800), 0.9), // ~200 tokens each
            make_chunk(&"b".repeat(800), 0.8),
            make_chunk(&"c".repeat(800), 0.7),
        ];
        // rag_limit(3200) = 800 tokens — but each chunk is ~200 tokens,
        // so exactly 4 fit. With 3 chunks of 200 tokens each, all 3 fit.
        let selected = alloc.truncate_to_rag_limit(chunks, 100); // rag_limit=25
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].score, 0.9);
    }

    #[test]
    fn truncate_respects_score_order_when_tied() {
        let alloc = ContextBudgetAllocator::default();
        let chunks = vec![
            make_chunk(&"T".repeat(800), 0.5),
            make_chunk(&"U".repeat(800), 0.5),
            make_chunk(&"V".repeat(800), 0.5),
        ];
        // rag_limit(100) = 25 tokens, each chunk ~200 tokens → only 1 fits after clip
        let selected = alloc.truncate_to_rag_limit(chunks, 100);
        assert_eq!(selected.len(), 1);
    }

    #[test]
    fn oversized_single_chunk_is_clipped_to_rag_limit() {
        let alloc = ContextBudgetAllocator::default();
        let selected =
            alloc.truncate_to_rag_limit(vec![make_chunk(&"x".repeat(20_000), 1.0)], 1_000);
        assert_eq!(selected.len(), 1);
        assert!(estimate_text_tokens(&selected[0].content) <= alloc.rag_limit(1_000));
        assert!(selected[0].content.contains("clipped"));
    }

    #[test]
    fn zero_capacity_returns_empty() {
        let alloc = ContextBudgetAllocator::default();
        let chunks = vec![make_chunk("test", 1.0)];
        assert!(alloc.truncate_to_rag_limit(chunks, 0).is_empty());
    }

    #[test]
    fn rag_limit_rounds_down() {
        let alloc = ContextBudgetAllocator::new(0.25, 0.10).unwrap();
        // 7 * 0.25 = 1.75 → truncates to 1
        assert_eq!(alloc.rag_limit(7), 1);
        assert_eq!(alloc.working_mem_limit(7), 0); // 7 * 0.10 = 0.7 → truncates to 0
    }

    #[test]
    fn build_context_orders_correctly() {
        let alloc = ContextBudgetAllocator::default();
        let chunks = vec![make_chunk("rag data", 1.0)];
        let working = message(Role::System, "<working_memory>active</working_memory>");
        let history = vec![message(Role::User, "hello")];
        let result = alloc.build_context(chunks, working, &history, 1_000);
        assert!(result.len() >= 2);
        assert!(result[0].content.contains("rag data"));
    }

    #[test]
    fn build_context_no_rag_chunks() {
        let alloc = ContextBudgetAllocator::default();
        let working = message(Role::System, "<wm>data</wm>");
        let history = vec![message(Role::User, "hello")];
        let result = alloc.build_context(vec![], working, &history, 1_000);
        assert!(result.iter().any(|m| m.content.contains("<wm>")));
        assert!(result.iter().any(|m| m.content == "hello"));
    }

    #[test]
    fn build_context_no_working_mem() {
        let alloc = ContextBudgetAllocator::default();
        let chunks = vec![make_chunk("rag data", 1.0)];
        let result = alloc.build_context(chunks, message(Role::System, ""), &[], 1_000);
        assert!(result.iter().any(|m| m.content.contains("rag data")));
    }

    #[test]
    fn build_context_empty_history() {
        let alloc = ContextBudgetAllocator::default();
        let chunks = vec![make_chunk("rag data", 1.0)];
        let result = alloc.build_context(chunks, message(Role::System, ""), &[], 1_000);
        assert_eq!(result.len(), 1);
        assert!(result[0].content.contains("rag data"));
    }

    #[test]
    fn working_memory_cannot_exceed_its_allocation() {
        let alloc = ContextBudgetAllocator::default();
        let working = message(Role::System, "w".repeat(100_000));
        let result = alloc.build_context(Vec::new(), working, &[], 10_000);
        assert_eq!(result.len(), 1);
        assert!(estimate_message_tokens(&result[0]) <= alloc.working_mem_limit(10_000) + 4);
        assert!(result[0].content.contains("clipped"));
    }

    #[test]
    fn newest_history_is_retained_when_history_does_not_fit() {
        let alloc = ContextBudgetAllocator::new(0.0, 0.0).unwrap();
        let history = vec![
            message(Role::User, "old".repeat(2_000)),
            message(Role::Assistant, "middle".repeat(2_000)),
            message(Role::User, "latest"),
        ];
        let result = alloc.build_context(Vec::new(), message(Role::System, ""), &history, 100);
        assert!(result.iter().any(|item| item.content == "latest"));
        assert!(!result.iter().any(|item| item.content.starts_with("old")));
    }

    #[test]
    fn clip_text_to_tokens_exact_fit() {
        let text = "Hello, world!";
        // 13 bytes / 4 = 3.25 → ceil = 4 tokens
        let clipped = clip_text_to_tokens(text, 4);
        assert_eq!(clipped, text);
    }

    #[test]
    fn clip_text_to_tokens_zero_limit() {
        let clipped = clip_text_to_tokens("some text", 0);
        assert!(clipped.is_empty());
    }

    #[test]
    fn clip_text_to_tokens_small_limit_only_suffix() {
        let text = "This is a very long text that should be clipped";
        // limit of 1 token = 4 bytes, suffix alone may be longer than that
        let clipped = clip_text_to_tokens(text, 1);
        assert!(clipped.len() < text.len() || clipped == text);
    }

    #[test]
    fn build_context_with_tool_calls_in_history() {
        let alloc = ContextBudgetAllocator::new(0.0, 0.0).unwrap();
        let history = vec![message_with_tool_calls(Role::Assistant, "calling tool")];
        let result = alloc.build_context(Vec::new(), message(Role::System, ""), &history, 10_000);
        assert_eq!(result.len(), 1);
        assert!(result[0].content == "calling tool");
    }

    #[test]
    fn estimate_message_tokens_counts_tool_calls_and_results() {
        let msg = message_with_tool_calls(Role::Assistant, "hello");
        let estimated = estimate_message_tokens(&msg);
        // Content "hello" = 5 / 4 = 1.25 → 1 token
        // Tool calls JSON approx 32 bytes / 4 = 8 tokens
        // Tool results JSON approx 16 bytes / 4 = 4 tokens
        // + 4 overhead
        assert!(estimated > 5, "should count tool calls and results, got {estimated}");
    }
}
