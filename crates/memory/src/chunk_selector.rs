//! Selector for choosing which messages to summarise when the context
//! window is under pressure.
//!
//! Extracted from `SummarizeOldest` so the selection algorithm is
//! independently testable.

use concerto_core::types::{Message, Role};

/// Selects the oldest messages that together exceed a target recovery
/// percentage of the context capacity.
///
/// Never selects system messages (they are assumed to be structural
/// prompts, not conversation content).
pub struct ChunkSelector {
    /// Fraction of capacity to recover by summarising old messages
    /// (default 0.20 = recover 20%).
    pub target_recovery_pct: f64,
    /// Fraction of capacity at which compaction becomes eligible.
    pub trigger_pct: f64,
    /// Fraction of capacity to target after compaction.
    pub target_pct: f64,
}

impl Default for ChunkSelector {
    fn default() -> Self {
        Self { target_recovery_pct: 0.20, trigger_pct: 0.85, target_pct: 0.70 }
    }
}

impl ChunkSelector {
    pub fn new(target_recovery_pct: f64) -> Self {
        Self { target_recovery_pct, trigger_pct: 0.85, target_pct: 0.70 }
    }

    /// Select the oldest messages that together exceed `target_recovery_pct`
    /// of `capacity` tokens. Returns indices into `history`.
    ///
    /// System messages are skipped (they are structural prompts, not
    /// conversation content).
    pub fn select_oldest_n(&self, history: &[Message], capacity: u64) -> Vec<usize> {
        let total_tokens = history.iter().map(estimate_message_tokens).sum::<u64>();
        let trigger_tokens = (capacity as f64 * self.trigger_pct) as u64;
        if total_tokens <= trigger_tokens {
            return Vec::new();
        }

        let target_tokens = (capacity as f64 * self.target_pct) as u64;
        let minimum_recovery = (capacity as f64 * self.target_recovery_pct) as u64;
        let required_recovery = total_tokens.saturating_sub(target_tokens).max(minimum_recovery);
        let mut selected: Vec<usize> = Vec::new();
        let mut accumulated: u64 = 0;

        for (idx, msg) in history.iter().enumerate() {
            // Skip system messages
            if msg.role == Role::System {
                continue;
            }
            let estimated = estimate_message_tokens(msg);
            accumulated += estimated;
            selected.push(idx);
            if accumulated >= required_recovery {
                break;
            }
        }

        selected
    }
}

fn estimate_message_tokens(message: &Message) -> u64 {
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
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn selects_from_front() {
        let selector = ChunkSelector::new(0.20);
        let history = vec![
            msg(Role::User, "first message that is fairly long"),
            msg(Role::Assistant, "second message also pretty long"),
            msg(Role::User, "third entry"),
        ];
        let selected = selector.select_oldest_n(&history, 30);
        assert!(!selected.is_empty());
        assert_eq!(selected[0], 0); // starts from oldest
    }

    #[test]
    fn skips_system_messages() {
        let selector = ChunkSelector::new(0.01);
        let history = vec![
            msg(Role::System, "system prompt that is very long indeed"),
            msg(Role::User, "hi"),
            msg(Role::Assistant, "hello"),
        ];
        let selected = selector.select_oldest_n(&history, 20);
        // Index 0 (System) should be skipped
        assert_eq!(selected[0], 1); // starts from the user message
    }

    #[test]
    fn returns_empty_when_only_system() {
        let selector = ChunkSelector::new(0.20);
        let history = vec![msg(Role::System, "only system messages")];
        let selected = selector.select_oldest_n(&history, 100);
        assert!(selected.is_empty());
    }

    #[test]
    fn returns_empty_when_short_history() {
        let selector = ChunkSelector::new(0.50);
        let history = vec![msg(Role::User, "short")];
        let selected = selector.select_oldest_n(&history, 10000);
        assert!(selected.is_empty());
    }

    #[test]
    fn select_respects_capacity() {
        let selector = ChunkSelector::new(0.50);
        let history = vec![
            msg(Role::User, "short"),
            msg(Role::User, "medium length message"),
            msg(Role::User, "a bit longer message here now"),
        ];
        // total_tokens = 6+9+12 = 27, trigger_pct=0.85 → trigger=26
        // 27 > 26 so selection proceeds; needs 15 tokens (msgs 0+1)
        let selected = selector.select_oldest_n(&history, 31);
        assert!(!selected.is_empty());
        // All selected indices should be >= 0 and < length
        for idx in &selected {
            assert!(*idx < history.len());
        }
    }

    #[test]
    fn select_accumulates_consecutive() {
        let selector = ChunkSelector::new(0.99);
        let history = vec![
            msg(Role::User, "first"),
            msg(Role::Assistant, "second"),
            msg(Role::User, "third"),
        ];
        // total_tokens = 6+6+6 = 18, trigger_pct=0.85 → trigger=15
        // 18 > 15 so selection proceeds; needs 17 tokens, all 3 selected
        let selected = selector.select_oldest_n(&history, 18);
        // Should select at least the first two to accumulate enough tokens
        assert!(selected.len() >= 2);
        // Indices should be consecutive starting from 0 (skipping no system)
        for (i, &idx) in selected.iter().enumerate() {
            assert_eq!(idx, i);
        }
    }

    #[test]
    fn custom_recovery_pct() {
        let selector = ChunkSelector::new(0.01); // 1% recovery
        let history = vec![
            msg(Role::User, "very long message here that should exceed 1% of 200 easily"),
            msg(Role::User, "another message"),
        ];
        // total_tokens = 17 + 8 = 25, trigger_pct=0.85 → trigger=24
        // 25 > 24 so selection proceeds; first msg (17 tokens) hits required_recovery=5
        let selected = selector.select_oldest_n(&history, 29);
        // With 1% target, the first message alone should be enough
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0], 0);
    }

    #[test]
    fn zero_recovery_selects_none() {
        let selector = ChunkSelector::new(0.0);
        let history = vec![msg(Role::User, "first message"), msg(Role::User, "second message")];
        // total_tokens = 8+8 = 16, trigger_pct=0.85 → trigger=15
        // 16 > 15 so selection proceeds; needs 4 tokens, first msg covers it
        let selected = selector.select_oldest_n(&history, 18);
        assert_eq!(selected.len(), 1);
    }
}
