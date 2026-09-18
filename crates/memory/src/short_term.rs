//! Short-term memory overflow strategy gate (ADR-67 M-01).
//!
//! The only conversation-pool overflow strategy retained here is
//! [`NoOpOverflowStrategy`], which documents the intentional decision to
//! truncate rather than summarise in run. The LLM `SummarizeOldest`
//! summarise-then-slide strategy (audit C-03 gate) is deleted: no code path
//! ever selected it, and context overflow is handled deterministically by the
//! orchestrator's durable compaction (`ContextEngine`) with the provider
//! boundary clip (`ContextGuardProvider`) as the final safety net. LLM-based
//! summarization may return as a configurable option behind a feature flag in
//! a superseding ADR.

use async_trait::async_trait;
use concerto_core::ids::Ulid;
use concerto_core::types::{Message, TokenBudget};
use concerto_core::CancellationToken;

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
    use concerto_core::types::Role;
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
    async fn no_op_returns_zero() {
        let strategy = NoOpOverflowStrategy;
        let budget = concerto_core::types::TokenBudget::new(100, 10);
        let mut history = vec![msg(Role::User, "hello")];
        let count =
            strategy.apply(&mut history, &budget, Ulid::new(), CancellationToken::new()).await;
        assert_eq!(count, 0);
        assert_eq!(history.len(), 1, "NoOp must leave the active history untouched");
    }
}
