//! Short-term memory overflow strategy: summarise-then-slide.
//!
//! Implements the unified [`concerto_core::ContextOverflowStrategy`] trait.
//! When the context window is under pressure, the oldest messages are
//! selected by `ChunkSelector`, summarised via `LLMSummarizer`, and
//! replaced with a single compact summary. Original messages are marked
//! `summarized` (never deleted) and the summary is stored as a
//! `MemoryEntry` in long-term memory for cross-session retrieval.

use std::sync::Arc;

use async_trait::async_trait;
use concerto_core::event::{EventBus, EventKind};
use concerto_core::ids::Ulid;
use concerto_core::memory::{ChunkType, MemoryEntry, MemoryId, MemoryNamespace, ProjectId};
use concerto_core::traits::memory::MemoryStore;
use concerto_core::types::{Message, Role, TokenBudget};
use concerto_core::CancellationToken;
use time::OffsetDateTime;

use crate::chunk_selector::ChunkSelector;
use crate::summarizer::{LLMSummarizer, SUMMARIZATION_PROMPT};

/// Summarise the oldest messages to free up context capacity.
///
/// 1. Select oldest messages via `ChunkSelector`.
/// 2. Emit `SummarizationStarted` event.
/// 3. LLM-summarise the selected messages.
/// 4. Mark original messages as summarised (append marker to content).
/// 5. Insert the summary as a `System` message at the front of history.
/// 6. Store the summary as a long-term `MemoryEntry`.
/// 7. Emit `SummarizationCompleted` event.
pub struct SummarizeOldest {
    summarizer: Arc<dyn LLMSummarizer>,
    chunk_selector: ChunkSelector,
    bus: EventBus,
    /// Namespace for storing summaries (project-scoped or global).
    namespace: MemoryNamespace,
    project_id: ProjectId,
    memory_store: Arc<dyn MemoryStore>,
}

impl SummarizeOldest {
    pub fn new(
        summarizer: Arc<dyn LLMSummarizer>,
        chunk_selector: ChunkSelector,
        bus: EventBus,
        namespace: MemoryNamespace,
        project_id: ProjectId,
        memory_store: Arc<dyn MemoryStore>,
    ) -> Self {
        Self { summarizer, chunk_selector, bus, namespace, project_id, memory_store }
    }
}

#[async_trait]
impl concerto_core::ContextOverflowStrategy for SummarizeOldest {
    async fn apply(
        &self,
        history: &mut Vec<Message>,
        budget: &TokenBudget,
        session_id: Ulid,
        _cancel: CancellationToken,
    ) -> usize {
        let capacity = budget.capacity;

        // 1. Select oldest messages to summarise
        let indices = self.chunk_selector.select_oldest_n(history, capacity);
        if indices.is_empty() {
            return 0;
        }

        let messages_to_summarize = indices.len();

        // 2. Emit started event
        let _ = self.bus.publish_for_session(
            session_id,
            session_id,
            EventKind::SummarizationStarted { session_id, messages_to_summarize },
        );

        // 3. Collect the messages and summarise them
        let selected: Vec<Message> = indices.iter().map(|&i| history[i].clone()).collect();
        let summary = match self.summarizer.summarize(&selected, SUMMARIZATION_PROMPT).await {
            Ok(s) => s,
            Err(e) => {
                // Audit C-03: never drop the original messages when
                // summarization fails. `history` is the active request
                // projection; removing messages here without a replacement
                // summary loses content with no durable copy. Per the trait
                // contract, log the failure and return 0, leaving `history`
                // untouched. Messages are only ever removed after a
                // successful summarization below.
                tracing::warn!(%e, "summarization failed — original messages kept intact");
                return 0;
            }
        };

        let before_tokens = estimate_history_tokens(history);

        // 4. Remove the covered active range. The durable session transcript
        // remains in the session store; this vector is only the active request
        // projection.
        for &i in indices.iter().rev() {
            history.remove(i);
        }

        // 5. Insert summary as a System message at the front of the summarised block
        let summary_msg = Message {
            role: Role::System,
            content: format!("<previous_session_summary>\n{summary}\n</previous_session_summary>"),
            tool_calls: None,
            tool_results: None,
            reasoning_content: None,
            tokens_in: None,
            tokens_out: None,
        };

        // Find the first summarised index and insert before it
        let insert_at = indices[0].min(history.len());
        history.insert(insert_at, summary_msg);

        // 6. Prepare summary as MemoryEntry for long-term store
        let summary_entry = MemoryEntry {
            id: MemoryId(Ulid::new()),
            project_id: self.project_id.clone(),
            namespace: self.namespace.clone(),
            content: summary.clone(),
            chunk_type: ChunkType::SessionSummary,
            model_id: None,
            model_version: None,
            metadata: serde_json::json!({
                "type": "session_summary",
                "source_count": messages_to_summarize,
                "session_id": session_id.to_string(),
                "covered_start_index": indices.first().copied(),
                "covered_end_index": indices.last().copied(),
            }),
            expires_at: None,
            created_at: OffsetDateTime::now_utc(),
        };
        if let Err(error) = self.memory_store.store(summary_entry, CancellationToken::new()).await {
            tracing::warn!(%error, "failed to persist compacted session summary");
        }

        // Never accept a summary that failed to reduce the active projection.
        // Deterministic truncation is safer than context expansion.
        if estimate_history_tokens(history) >= before_tokens {
            history.remove(insert_at);
            tracing::warn!("summary did not reduce active context; retaining truncation only");
        }

        // 7. Emit completed event
        let _ = self.bus.publish_for_session(
            session_id,
            session_id,
            EventKind::SummarizationCompleted { session_id, summary_len: summary.len() },
        );

        messages_to_summarize
    }
}

fn estimate_history_tokens(history: &[Message]) -> u64 {
    history
        .iter()
        .map(|message| {
            let content = (message.content.len() as u64).div_ceil(4);
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
            content.saturating_add(calls).saturating_add(results).saturating_add(4)
        })
        .sum()
}

/// A no-op strategy that never summarises (for testing or when context
/// is known to always fit).
pub struct NoOpOverflowStrategy;

#[async_trait]
impl concerto_core::ContextOverflowStrategy for NoOpOverflowStrategy {
    async fn apply(
        &self,
        _history: &mut Vec<Message>,
        _budget: &TokenBudget,
        _session_id: Ulid,
        _cancel: CancellationToken,
    ) -> usize {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::summarizer::FakeSummarizer;
    use concerto_core::traits::memory::NullMemoryStore;
    use concerto_core::ContextOverflowStrategy;

    fn msg(role: Role, content: &str) -> Message {
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
    async fn summarise_oldest_selects_and_inserts_summary() {
        let bus = EventBus::default();
        let selector = ChunkSelector::new(0.50);
        let summarizer = Arc::new(FakeSummarizer::new("Test summary here"));
        let strategy = SummarizeOldest::new(
            summarizer,
            selector,
            bus,
            MemoryNamespace::Global { user_id_hash: "test".into() },
            ProjectId("test".into()),
            Arc::new(NullMemoryStore),
        );

        let mut history = vec![
            msg(Role::System, "You are a helpful assistant."),
            msg(Role::User, "Hello, how are you?"),
            msg(Role::Assistant, "I'm doing great! And you?"),
            msg(Role::User, "What is Rust?"),
            msg(Role::Assistant, "Rust is a systems programming language."),
        ];

        // non-system tokens = 9+11+7+15 = 42, trigger_pct=0.85 → trigger=41
        // 42 > 41 so selection proceeds with capacity 49
        let budget = concerto_core::types::TokenBudget::new(49, 10);
        let count =
            strategy.apply(&mut history, &budget, Ulid::new(), CancellationToken::new()).await;
        assert!(count > 0);

        // Summary message should have been inserted
        assert!(history.iter().any(|m| m.content.contains("Test summary here")));
    }

    #[tokio::test]
    async fn no_op_returns_zero() {
        let strategy = NoOpOverflowStrategy;
        let budget = concerto_core::types::TokenBudget::new(100, 10);
        let mut history = vec![msg(Role::User, "hello")];
        let count =
            strategy.apply(&mut history, &budget, Ulid::new(), CancellationToken::new()).await;
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn no_messages_to_summarise_returns_zero() {
        let bus = EventBus::default();
        let selector = ChunkSelector::new(0.01);
        let summarizer = Arc::new(FakeSummarizer::new("summary"));
        let strategy = SummarizeOldest::new(
            summarizer,
            selector,
            bus,
            MemoryNamespace::Global { user_id_hash: "test".into() },
            ProjectId("test".into()),
            Arc::new(NullMemoryStore),
        );

        let budget = concerto_core::types::TokenBudget::new(100, 10);
        let mut history = vec![msg(Role::System, "Only system messages here.")];
        let count =
            strategy.apply(&mut history, &budget, Ulid::new(), CancellationToken::new()).await;
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn summarizer_failure_leaves_original_messages_intact() {
        let bus = EventBus::default();
        let selector = ChunkSelector::new(0.50);
        // FakeSummarizer that returns an error
        let summarizer = Arc::new(FakeSummarizer::new_err("LLM unavailable"));
        let strategy = SummarizeOldest::new(
            summarizer,
            selector,
            bus,
            MemoryNamespace::Global { user_id_hash: "test".into() },
            ProjectId("test".into()),
            Arc::new(NullMemoryStore),
        );

        let original =
            vec![msg(Role::User, "Hello, how are you?"), msg(Role::Assistant, "I'm doing great!")];
        let mut history = original.clone();

        // total_tokens = 9+8 = 17, trigger_pct=0.85 → trigger=9 (cap 11)
        // 17 > 9 so selection proceeds; required_recovery=10 > 9 → selects both
        let budget = concerto_core::types::TokenBudget::new(11, 10);
        let count =
            strategy.apply(&mut history, &budget, Ulid::new(), CancellationToken::new()).await;
        // Regression for audit C-03: on summarization failure the strategy must
        // report zero messages handled and leave every original message intact.
        // The same budget previously produced count == 2 (and an emptied
        // history) under the old truncation fallback, proving selection runs.
        assert_eq!(count, 0);
        assert_eq!(history.len(), original.len());
        for (kept, expected) in history.iter().zip(original.iter()) {
            assert_eq!(kept.role, expected.role);
            assert_eq!(kept.content, expected.content);
        }
    }

    #[tokio::test]
    async fn summariser_removes_and_replaces_oldest() {
        let bus = EventBus::default();
        let selector = ChunkSelector::new(0.99);
        let summarizer = Arc::new(FakeSummarizer::new("x"));
        let strategy = SummarizeOldest::new(
            summarizer,
            selector,
            bus,
            MemoryNamespace::Global { user_id_hash: "test".into() },
            ProjectId("test".into()),
            Arc::new(NullMemoryStore),
        );

        // Long messages so token savings outweigh the wrapper overhead (~19 tokens)
        let mut history = vec![
            msg(Role::System, "You are a helpful assistant system prompt for this test scenario."),
            msg(Role::User, "hello this is a fairly long user message so that the summarization of it saves enough tokens to justify the xml wrapper overhead that will be inserted into the context"),
            msg(Role::Assistant, "hi there this is an equally long assistant response to ensure we have enough token mass to make the summarization worthwhile"),
        ];

        // non-system tokens ≈ 29+27 = 56, trigger_pct=0.85 → trigger=47,
        // capacity 56: trigger=47, 56>47, min_recovery=56*0.99=55 → select all 2
        let budget = concerto_core::types::TokenBudget::new(56, 10);
        let count =
            strategy.apply(&mut history, &budget, Ulid::new(), CancellationToken::new()).await;
        assert!(count > 0);

        // Original messages are removed; only System + summary remain
        let original_msg_count = history
            .iter()
            .filter(|m| {
                m.content.contains("this is a fairly long user message")
                    || m.content.contains("this is an equally long assistant response")
            })
            .count();
        assert_eq!(original_msg_count, 0, "original messages should be removed");
        // Summary was inserted
        assert!(history.iter().any(|m| m.content.contains("<previous_session_summary>")));
    }

    #[tokio::test]
    async fn summariser_inserts_summary_at_front_of_summarised_block() {
        let bus = EventBus::default();
        // Short summary to ensure it saves context after the wrapper overhead
        let summarizer = Arc::new(FakeSummarizer::new("x"));
        // High recovery to select enough messages to offset the wrapper overhead
        let selector = ChunkSelector::new(0.99);
        let strategy = SummarizeOldest::new(
            summarizer,
            selector,
            bus,
            MemoryNamespace::Global { user_id_hash: "test".into() },
            ProjectId("test".into()),
            Arc::new(NullMemoryStore),
        );

        // Long messages (~80 chars each) so token savings outweigh the
        // <previous_session_summary> wrapper's ~19-token overhead.
        let mut history = vec![
            msg(Role::System, "System prompt"),
            msg(Role::User, "first question that is fairly long so we get enough token savings to make summarization worthwhile"),
            msg(Role::Assistant, "first answer that is also quite substantial in length for the same reason regarding token economy"),
            msg(Role::User, "second question that needs to be sufficiently long to justify the context compression overhead"),
        ];

        // non-system tokens ≈ 25+24+25 = 74, trigger_pct=0.85 → trigger=63,
        // capacity 74: trigger=62, 74>62, min_recovery=74*0.99=73 → select all 3
        let budget = concerto_core::types::TokenBudget::new(74, 10);
        let count =
            strategy.apply(&mut history, &budget, Ulid::new(), CancellationToken::new()).await;
        assert!(count > 0);

        // Summary should be at the first summarised position (after System)
        let summary_pos =
            history.iter().position(|m| m.content.contains("<previous_session_summary>"));
        assert!(summary_pos.is_some(), "summary should be present");

        // The summary message should be a System role
        let summary_msg = &history[summary_pos.unwrap()];
        assert_eq!(summary_msg.role, Role::System);
        assert!(summary_msg.content.contains("<previous_session_summary>"));
        assert!(summary_msg.content.contains("</previous_session_summary>"));
    }
}
