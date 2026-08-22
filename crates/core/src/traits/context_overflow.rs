//! Unified context overflow strategy trait and built-in implementations.
//!
//! The single `ContextOverflowStrategy` trait used by both frontends,
//! `AgentLoop`, and `ProjectSessionManager`. Every overflow/trimming
//! decision flows through this trait so there is exactly one seam to
//! replace when changing behaviour.
//!
//! # Built-in strategies
//!
//! * [`TruncateOldest`] — sync, delete-only. Drops the oldest non-system
//!   messages when estimated tokens exceed a trigger ratio. Cheap and
//!   always succeeds.
//! * [`NoOpOverflowStrategy`] — a no-op strategy for testing or when
//!   overflow handling is intentionally disabled.

use crate::CancellationToken;
use async_trait::async_trait;

use crate::ids::Ulid;
use crate::types::{Message, TokenBudget};

/// Strategy for handling context window overflow.
///
/// When the conversation history exceeds the available token budget,
/// implementations decide which messages to drop, summarise, or compress.
///
/// The method is async so that LLM-based strategies (summarisation) can
/// make a provider call. Sync strategies like [`TruncateOldest`] simply
/// return immediately.
///
/// Errors are handled internally — the strategy logs the failure and
/// returns 0. The caller never needs to handle a strategy error.
#[async_trait]
pub trait ContextOverflowStrategy: Send + Sync {
    /// Apply the strategy to reduce `messages` to fit within `budget`.
    ///
    /// Returns the number of messages that were removed or summarised.
    /// If the strategy fails (e.g. LLM summarisation error) it should
    /// log the error and return 0, leaving `messages` unchanged.
    async fn apply(
        &self,
        messages: &mut Vec<Message>,
        budget: &TokenBudget,
        session_id: Ulid,
        cancel: CancellationToken,
    ) -> usize;
}

// ---------------------------------------------------------------------------
// TruncateOldest — sync, delete-only strategy
// ---------------------------------------------------------------------------

/// Strategy that trims the oldest non-System messages when the context
/// exceeds a configurable trigger ratio of the token budget capacity.
///
/// Uses a simple token estimation (bytes ÷ 4 + per-message overhead)
/// suitable for any model. Removes oldest `User`/`Assistant`/`Tool`
/// messages one at a time until the estimated total fits at or below
/// the target ratio. System messages are always preserved.
///
/// Default: trigger at 85% capacity, trim to 75% capacity.
///
/// This is the safe default — it always succeeds and never calls an LLM.
pub struct TruncateOldest {
    /// Fraction of `budget.capacity` that triggers trimming (e.g. 0.85).
    pub trigger_ratio: f64,
    /// Fraction of `budget.capacity` to trim down to (e.g. 0.75).
    pub target_ratio: f64,
}

impl Default for TruncateOldest {
    fn default() -> Self {
        Self { trigger_ratio: 0.85, target_ratio: 0.75 }
    }
}

impl TruncateOldest {
    /// Estimate the token count for a single message.
    /// 4 bytes ≈ 1 token on average, plus 4 tokens overhead for role markers.
    fn estimate_message_tokens(msg: &Message) -> u64 {
        let content_tokens = (msg.content.len() as u64).div_ceil(4);
        content_tokens + 4
    }
}

#[async_trait]
impl ContextOverflowStrategy for TruncateOldest {
    async fn apply(
        &self,
        messages: &mut Vec<Message>,
        budget: &TokenBudget,
        _session_id: Ulid,
        _cancel: CancellationToken,
    ) -> usize {
        let capacity = budget.capacity;
        let trigger = (capacity as f64 * self.trigger_ratio) as u64;
        let target = (capacity as f64 * self.target_ratio) as u64;

        let total_tokens: u64 = messages.iter().map(Self::estimate_message_tokens).sum();
        if total_tokens <= trigger || messages.len() <= 1 {
            return 0;
        }

        let mut to_remove: Vec<usize> = Vec::new();
        let mut running = total_tokens;

        for (i, msg) in messages.iter().enumerate() {
            if msg.role == crate::types::Role::System {
                continue;
            }
            if running <= target {
                break;
            }
            let tokens = Self::estimate_message_tokens(msg);
            running = running.saturating_sub(tokens);
            to_remove.push(i);
        }

        let count = to_remove.len();
        for &i in to_remove.iter().rev() {
            messages.remove(i);
        }
        count
    }
}

// ---------------------------------------------------------------------------
// NoOpOverflowStrategy
// ---------------------------------------------------------------------------

/// A no-op strategy that never trims (for testing or when overflow
/// handling is intentionally disabled).
pub struct NoOpOverflowStrategy;

#[async_trait]
impl ContextOverflowStrategy for NoOpOverflowStrategy {
    async fn apply(
        &self,
        _messages: &mut Vec<Message>,
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
    use crate::types::{Role, TokenBudget};

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
    async fn truncate_keeps_system_messages() {
        let strategy = TruncateOldest::default();
        let budget = TokenBudget::new(100, 10);
        let mut messages = vec![
            msg(Role::System, "You are a helpful assistant."),
            msg(Role::User, "Hello"),
            msg(Role::Assistant, "Hi there!"),
        ];
        strategy.apply(&mut messages, &budget, Ulid::new(), CancellationToken::new()).await;
        assert!(messages.iter().any(|m| m.role == Role::System));
    }

    #[tokio::test]
    async fn truncate_does_nothing_when_under_budget() {
        let strategy = TruncateOldest::default();
        let budget = TokenBudget::new(1_000_000, 10);
        let original = vec![msg(Role::User, "short")];
        let mut messages = original.clone();
        strategy.apply(&mut messages, &budget, Ulid::new(), CancellationToken::new()).await;
        assert_eq!(messages.len(), original.len());
    }

    #[tokio::test]
    async fn no_op_returns_zero() {
        let strategy = NoOpOverflowStrategy;
        let budget = TokenBudget::new(10, 0);
        let mut messages = vec![msg(Role::User, "test")];
        let count =
            strategy.apply(&mut messages, &budget, Ulid::new(), CancellationToken::new()).await;
        assert_eq!(count, 0);
        assert_eq!(messages.len(), 1);
    }
}
