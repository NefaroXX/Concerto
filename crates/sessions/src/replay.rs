use crate::{SessionError, SessionStore};
use concerto_core::event::{Event, EventKind};
use concerto_core::ids::Ulid;
use concerto_core::CancellationToken;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

/// A stored event in the session_events table.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredEvent {
    pub id: Ulid,
    pub session_id: Ulid,
    pub sequence_num: i64,
    pub correlation_id: Ulid,
    pub event_kind: String,
    pub payload: String,
    pub created_at: OffsetDateTime,
}

impl StoredEvent {
    pub fn to_event(&self) -> Result<Event, SessionError> {
        let kind: EventKind = serde_json::from_str(&self.payload).map_err(|e| {
            SessionError::Serialization(format!("failed to deserialize event payload: {e}"))
        })?;
        Ok(Event {
            id: self.id,
            correlation_id: self.correlation_id,
            session_id: self.session_id,
            timestamp: self.created_at,
            kind,
        })
    }
}

/// Reconstructs session state by replaying stored events.
pub struct SessionReplayer;

impl SessionReplayer {
    /// Replay all stored events for a session in order.
    pub async fn replay_all(
        store: &dyn SessionStore,
        session_id: Ulid,
    ) -> Result<Vec<Event>, SessionError> {
        let stored = store.load_events(session_id, CancellationToken::new()).await?;
        let mut events = Vec::with_capacity(stored.len());
        for s in stored {
            events.push(s.to_event()?);
        }
        Ok(events)
    }

    /// Replay events up to a maximum sequence number (for partial replay).
    pub async fn replay_until(
        store: &dyn SessionStore,
        session_id: Ulid,
        max_seq: u64,
    ) -> Result<Vec<Event>, SessionError> {
        let stored = store.load_events_until(session_id, max_seq, CancellationToken::new()).await?;
        let mut events = Vec::with_capacity(stored.len());
        for s in stored {
            events.push(s.to_event()?);
        }
        Ok(events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ConversationHistory;
    use concerto_core::traits::context_overflow::{NoOpOverflowStrategy, TruncateOldest};
    use concerto_core::types::{Message, Role, TokenBudget};

    #[test]
    fn stored_event_round_trip() {
        let kind = EventKind::ToolExecutionStarted {
            tool_name: "filesystem".into(),
            input_hash: "abc123".into(),
            detail: None,
        };
        let payload = serde_json::to_string(&kind).unwrap();

        let deserialized: EventKind = serde_json::from_str(&payload).unwrap();
        match deserialized {
            EventKind::ToolExecutionStarted { tool_name, input_hash, .. } => {
                assert_eq!(tool_name, "filesystem");
                assert_eq!(input_hash, "abc123");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // New tests added below (5 tests)
    // -----------------------------------------------------------------------

    #[test]
    /// Full `StoredEvent` serializes to JSON and deserializes back with all
    /// fields preserved.
    fn stored_event_full_serialization_round_trip() {
        let event = StoredEvent {
            id: Ulid::new(),
            session_id: Ulid::new(),
            sequence_num: 7,
            correlation_id: Ulid::new(),
            event_kind: "ToolExecutionStarted".into(),
            payload: r#"{"tool_name":"ls","input_hash":"abc"}"#.into(),
            created_at: OffsetDateTime::now_utc(),
        };
        let json = serde_json::to_string(&event).unwrap();
        let deserialized: StoredEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.id, event.id);
        assert_eq!(deserialized.session_id, event.session_id);
        assert_eq!(deserialized.sequence_num, 7);
        assert_eq!(deserialized.event_kind, "ToolExecutionStarted");
        assert_eq!(deserialized.payload, r#"{"tool_name":"ls","input_hash":"abc"}"#);
    }

    #[test]
    /// `StoredEvent` instances are ordered correctly by `sequence_num`.
    fn stored_event_ordering_by_sequence_num() {
        let mut events = [
            StoredEvent { sequence_num: 5, ..make_dummy_event() },
            StoredEvent { sequence_num: 1, ..make_dummy_event() },
            StoredEvent { sequence_num: 3, ..make_dummy_event() },
        ];
        events.sort_by_key(|e| e.sequence_num);
        assert_eq!(events[0].sequence_num, 1);
        assert_eq!(events[1].sequence_num, 3);
        assert_eq!(events[2].sequence_num, 5);
    }

    /// Helper: create a dummy `StoredEvent` with default-ish fields.
    fn make_dummy_event() -> StoredEvent {
        StoredEvent {
            id: Ulid::new(),
            session_id: Ulid::new(),
            sequence_num: 0,
            correlation_id: Ulid::new(),
            event_kind: "test".into(),
            payload: "{}".into(),
            created_at: OffsetDateTime::now_utc(),
        }
    }

    #[test]
    /// Multiple `EventKind` variants serialise and deserialise correctly
    /// through their payload strings.
    fn stored_event_different_event_kinds() {
        let kinds: Vec<EventKind> = vec![
            EventKind::SessionSaved,
            EventKind::AgentThought { agent_id: "a1".into(), content: "thinking".into() },
            EventKind::ToolExecutionFinished {
                tool_name: "fs".into(),
                duration_ms: 100,
                success: true,
                detail: Some("done".into()),
            },
        ];

        for kind in &kinds {
            let payload = serde_json::to_string(kind).unwrap();
            let deserialized: EventKind = serde_json::from_str(&payload).unwrap();
            // Verify it round-trips by re-serialising and comparing.
            let re_payload = serde_json::to_string(&deserialized).unwrap();
            assert_eq!(payload, re_payload, "round-trip mismatch for {kind:?}");
        }
    }

    #[tokio::test]
    /// `ConversationHistory::apply_overflow` with a `TruncateOldest` strategy
    /// removes oldest non-system messages when budget is exceeded.
    async fn conversation_history_apply_overflow_truncate() {
        let budget = TokenBudget::new(50, 5);
        let mut history = ConversationHistory::new(budget);
        // Add a system message (should be preserved).
        history.add(Message {
            role: Role::System,
            content: "You are a helpful assistant.".into(),
            tool_calls: None,
            tool_results: None,
            reasoning_content: None,
            tokens_in: None,
            tokens_out: None,
        });
        // Add several long user/assistant messages that exceed the budget.
        for i in 0..5 {
            history.add(Message {
                role: Role::User,
                content: format!(
                    "This is message number {i} with some extra padding to consume tokens."
                ),
                tool_calls: None,
                tool_results: None,
                reasoning_content: None,
                tokens_in: None,
                tokens_out: None,
            });
        }
        let strategy = TruncateOldest::default();
        let removed = history.apply_overflow(&strategy, CancellationToken::new()).await;
        // At least one message should have been removed.
        assert!(removed > 0, "expected trunkation to remove messages");
        // System message must still be present.
        assert!(history.messages().iter().any(|m| m.role == Role::System));
    }

    #[tokio::test]
    /// `ConversationHistory::apply_overflow` with a `NoOpOverflowStrategy`
    /// should never remove any messages.
    async fn conversation_history_apply_overflow_summarize() {
        let budget = TokenBudget::new(10, 0);
        let mut history = ConversationHistory::new(budget);
        history.add(Message {
            role: Role::User,
            content: "Hello".into(),
            tool_calls: None,
            tool_results: None,
            reasoning_content: None,
            tokens_in: None,
            tokens_out: None,
        });
        let len_before = history.messages().len();
        let strategy = NoOpOverflowStrategy;
        let removed = history.apply_overflow(&strategy, CancellationToken::new()).await;
        assert_eq!(removed, 0);
        assert_eq!(history.messages().len(), len_before);
    }
}
