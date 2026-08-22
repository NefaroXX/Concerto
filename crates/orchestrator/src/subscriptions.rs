//! ADR-60 D3 whiteboard subscription manager (supervisor side).
//!
//! Serves the protocol 0.2.0 `whiteboard-slice` / `ack-whiteboard` surface:
//! for each registered subscriber (an agent id) it tracks the subscribed
//! topic scopes and the subscriber's acknowledged consistent-cut cursor, and
//! produces bounded slices of whiteboard events for the supervisor loop to
//! push to the child process.
//!
//! # Delivery mechanism — DB replay, not a mailbox
//!
//! The addendum's bounded mailbox is implemented as DB replay: the whiteboard
//! log (`whiteboard_events`) *is* the queue, and each flush reads a bounded
//! window (`after_gate_seq = cursor`, [`WHITEBOARD_SLICE_WINDOW`] rows, ≤
//! [`WHITEBOARD_SLICE_MAX_BYTES`]). The persisted cursor
//! (`whiteboard_subscriptions.cursor_gate_seq`) advances **only** on the
//! subscriber's `AckWhiteboard` — never at enqueue/flush — so an overflow
//! (window full) merely stalls delivery; nothing is lost, and re-registration
//! (agent restart) resumes from the persisted cursor (at-least-once,
//! resume-from-cursor semantics). The agent dedups by a continuous
//! `gate_seq` high-water mark, making redelivery idempotent.
//!
//! The cursor is a consistent cut over the *whole* log, not just matching
//! events: a flush whose window contains only non-matching kinds still
//! delivers a (possibly empty) slice whose `end_gate_seq` advances the cut
//! past those events, so non-matching traffic never stalls a subscriber.
//!
//! This module is pure supervisor-side bookkeeping; transport (writing
//! notifications to child stdin) lives in the supervisor loop.

use std::collections::HashMap;

use concerto_sessions::whiteboard::{
    ack_whiteboard_subscription, load_whiteboard_events, load_whiteboard_subscription,
    upsert_whiteboard_subscription, WhiteboardEvent, WhiteboardLoadOpts, WhiteboardScope,
    WhiteboardSubscription,
};
use sqlx::SqlitePool;

/// Maximum whiteboard rows read per flush window (the per-slice event cap).
const WHITEBOARD_SLICE_WINDOW: usize = 64;
/// Per-slice payload cap (serialized JSON of the delivered events).
const WHITEBOARD_SLICE_MAX_BYTES: usize = 256 * 1024;

/// One subscriber's in-memory delivery state (cursor truth lives in the DB).
#[derive(Debug, Clone)]
pub(crate) struct SubscriberState {
    /// The subscribed topic scopes (a kind matches if it is in any scope).
    scopes: Vec<WhiteboardScope>,
    /// In-memory mirror of the DB cursor (the ack is the single writer).
    cursor_gate_seq: u64,
    /// Set when new matching traffic may be available; cleared on a drained
    /// flush. The supervisor drains dirty subscribers each loop iteration.
    dirty: bool,
    /// Last gate_seq the supervisor pushed in a slice (delivery watermark;
    /// the agent may or may not have acked it yet). A push ahead of the
    /// acked cursor suppresses re-pushes of the same span.
    last_flushed_gate_seq: u64,
}

/// A bounded, contiguous (in `gate_seq`) slice for one subscriber.
#[derive(Debug)]
pub(crate) struct SliceBatch {
    /// Cursor coordinate the subscriber should ack after applying `events`:
    /// the `gate_seq` of the last event *included* in the slice.
    pub end_gate_seq: u64,
    /// The subscribed-scope events in this slice (may be empty when the
    /// window contained only non-matching kinds — the cut still advances).
    pub events: Vec<WhiteboardEvent>,
    /// True when the window was full or bytes were truncated, i.e. the
    /// subscriber should be re-flushed after this slice is delivered.
    pub more: bool,
}

/// Registry of per-subscriber subscription state (supervisor side).
///
/// Public so the runtime and tests can attach a manager to
/// [`SupervisorServices`](crate::supervisor::SupervisorServices); its
/// methods are `pub(crate)` — the supervisor loop is the only caller.
#[derive(Debug)]
pub struct SubscriptionManager {
    pool: SqlitePool,
    inner: std::sync::Arc<tokio::sync::Mutex<HashMap<String, SubscriberState>>>,
}

impl Clone for SubscriptionManager {
    /// Task-local clone sharing the pool handle and the registry (callers —
    /// e.g. detached dispatch tasks — observe each other's registrations).
    fn clone(&self) -> Self {
        Self { pool: self.pool.clone(), inner: self.inner.clone() }
    }
}

impl SubscriptionManager {
    /// Create a manager bound to the whiteboard log pool.
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool, inner: std::sync::Arc::new(tokio::sync::Mutex::new(HashMap::new())) }
    }

    /// Register (or re-register) a subscriber and rehydrate its persisted
    /// cursor. Absent or unreadable cursor state degrades to `0` (replay from
    /// the beginning — safe under at-least-once, mirrored by the D5
    /// read-failure degradation convention).
    pub(crate) async fn register(&self, subscriber_id: String, scopes: Vec<WhiteboardScope>) {
        // Rehydrate the persisted cursor when a durable row exists (restart
        // path): register is idempotent and never lowers it.
        if let Ok(Some(subscription)) =
            load_whiteboard_subscription(&self.pool, &subscriber_id).await
        {
            let dirty = self.has_events_after(subscription.cursor_gate_seq).await;
            let mut inner = self.inner.lock().await;
            inner.insert(
                subscriber_id,
                SubscriberState {
                    scopes,
                    cursor_gate_seq: subscription.cursor_gate_seq,
                    dirty,
                    last_flushed_gate_seq: subscription.cursor_gate_seq,
                },
            );
            return;
        }
        // First registration: materialize the durable cursor row (cursor 0)
        // — the row is what makes an ack durable (`ack_whiteboard_subscription`
        // is a monotonic UPDATE, never an insert). The scopes are the
        // config-owned set (ADR-58/59); a crash-replay re-registration upserts
        // the same row. Absent/unreadable cursor state degrades to `0` (replay
        // from the log start — safe under at-least-once, mirrored by the D5
        // read-failure degradation convention).
        let cursor = 0;
        if let Err(error) = upsert_whiteboard_subscription(
            &self.pool,
            &WhiteboardSubscription {
                subscriber_id: subscriber_id.clone(),
                scopes: scopes.clone(),
                cursor_gate_seq: cursor,
            },
        )
        .await
        {
            tracing::warn!(
                subscriber = %subscriber_id,
                %error,
                "subscription row materialization failed; acks will not persist"
            );
        }
        let dirty = self.has_events_after(cursor).await;
        let mut inner = self.inner.lock().await;
        inner.insert(
            subscriber_id,
            SubscriberState {
                scopes,
                cursor_gate_seq: cursor,
                dirty,
                last_flushed_gate_seq: cursor,
            },
        );
    }
    /// Wake every subscriber whose cursor precedes the appended event's
    /// `gate_seq`. Call after any successful log append (`publish-event`,
    /// gated write, gate rejection row): dirty is merely "maybe pending" —
    /// [`Self::pending_slice`] does the real scope filtering and an empty
    /// slice advances the cut past non-matching events, so waking broadly is
    /// correct and cannot starve a subscriber.
    pub(crate) async fn mark_append(&self, gate_seq: u64) {
        let mut inner = self.inner.lock().await;
        for state in inner.values_mut() {
            if state.cursor_gate_seq < gate_seq {
                state.dirty = true;
            }
        }
    }
    /// Apply an acknowledged consistent-cut coordinate. Persists the
    /// monotonic max via the sessions helper (never lowers the cursor) and
    /// re-dirties the subscriber when the log has continuation beyond it.
    pub(crate) async fn ack(&self, subscriber_id: &str, end_gate_seq: u64) {
        if let Err(error) =
            ack_whiteboard_subscription(&self.pool, subscriber_id, end_gate_seq).await
        {
            // At-least-once: the next ack or re-registration retries the
            // same cut; delivery continues from the last persisted cursor.
            tracing::warn!(subscriber = %subscriber_id, %error, "cursor ack persistence failed");
        }
        let mut inner = self.inner.lock().await;
        if let Some(state) = inner.get_mut(subscriber_id) {
            if end_gate_seq > state.cursor_gate_seq {
                state.cursor_gate_seq = end_gate_seq;
            }
            state.dirty = self.has_events_after(state.cursor_gate_seq).await;
        }
    }

    /// The bounded next slice for a subscriber, or `None` when the log has
    /// nothing beyond its cursor (or the previous push of the current span
    /// is still unacked — an overflow stalls delivery rather than
    /// re-sending the same window every tick). Read-only: the supervisor
    /// calls [`Self::mark_flushed`] after delivering the slice.
    pub(crate) async fn pending_slice(&self, subscriber_id: &str) -> Option<SliceBatch> {
        let (cursor, topics) = {
            let inner = self.inner.lock().await;
            let state = inner.get(subscriber_id)?;
            // The span [cursor, ...] was already pushed and not yet acked:
            // nothing new to deliver (at-least-once — the agent dedups a
            // genuine retry by its own high-water mark, but the loop must
            // not recount the same span on its own cadence).
            if state.last_flushed_gate_seq > state.cursor_gate_seq {
                return None;
            }
            let topics: Vec<_> =
                state.scopes.iter().flat_map(|scope| scope.topics.iter().copied()).collect();
            (state.cursor_gate_seq, topics)
        };

        let opts = WhiteboardLoadOpts {
            after_gate_seq: cursor,
            session_id: None,
            scope: None,
            limit: WHITEBOARD_SLICE_WINDOW,
        };
        let raw = match load_whiteboard_events(&self.pool, &opts).await {
            Ok(events) => events,
            Err(error) => {
                tracing::warn!(subscriber = %subscriber_id, %error, "slice read failed");
                return None;
            }
        };
        if raw.is_empty() {
            return None;
        }

        // Keep matching events in order, bounded by the byte cap. The cut
        // end advances over non-matching events too (consistent cut), so an
        // empty slice is legitimate: the subscriber acks it and the log is
        // not re-read for the same window.
        let mut kept = Vec::new();
        let mut bytes: usize = 0;
        let mut truncated_by_bytes = false;
        let mut end_gate_seq = cursor;
        for event in raw.iter() {
            if topics.contains(&event.kind) {
                let size = serde_json::to_vec(event).map(|json| json.len()).unwrap_or(0);
                if bytes + size > WHITEBOARD_SLICE_MAX_BYTES {
                    truncated_by_bytes = true;
                    break;
                }
                bytes += size;
                kept.push(event.clone());
            }
            end_gate_seq = event.gate_seq;
        }

        let window_full = raw.len() == WHITEBOARD_SLICE_WINDOW;
        Some(SliceBatch { end_gate_seq, events: kept, more: window_full || truncated_by_bytes })
    }

    /// Subscriber ids currently marked dirty (drain candidates).
    pub(crate) async fn flush_candidates(&self) -> Vec<String> {
        let inner = self.inner.lock().await;
        inner.iter().filter(|(_, state)| state.dirty).map(|(id, _)| id.clone()).collect()
    }

    /// Record that `end_gate_seq` was pushed in a slice; keep the subscriber
    /// dirty only when the window indicated continuation (the pending span
    /// remains unacked until the agent acks it — [`Self::pending_slice`]
    /// suppresses re-pushes of the same span).
    pub(crate) async fn mark_flushed(&self, subscriber_id: &str, end_gate_seq: u64, more: bool) {
        let mut inner = self.inner.lock().await;
        if let Some(state) = inner.get_mut(subscriber_id) {
            state.last_flushed_gate_seq = end_gate_seq;
            state.dirty = more;
        }
    }

    /// Whether the log has any event strictly past `cursor` (cheap probe:
    /// limit 1).
    async fn has_events_after(&self, cursor: u64) -> bool {
        let opts =
            WhiteboardLoadOpts { after_gate_seq: cursor, session_id: None, scope: None, limit: 1 };
        match load_whiteboard_events(&self.pool, &opts).await {
            Ok(events) => !events.is_empty(),
            Err(error) => {
                tracing::warn!(%error, "continuation probe failed; assuming none");
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use concerto_sessions::whiteboard::{
        append_whiteboard_event, upsert_whiteboard_subscription, NewWhiteboardEvent,
        WhiteboardKind, WhiteboardSubscription,
    };
    use serde_json::json;
    use sqlx::pool::PoolOptions;
    use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqliteSynchronous};
    use std::time::Duration;
    use ulid::Ulid;

    async fn test_pool() -> (tempfile::TempDir, sqlx::SqlitePool) {
        let dir = tempfile::tempdir().expect("tempdir created");
        let path = dir.path().join("subscriptions.db");
        let options = SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .busy_timeout(Duration::from_secs(5))
            .foreign_keys(true)
            .synchronous(SqliteSynchronous::Normal);
        let pool = PoolOptions::new()
            .max_connections(6)
            .connect_with(options)
            .await
            .expect("test pool connects");
        sqlx::migrate!("../sessions/migrations").run(&pool).await.expect("migrations apply");
        (dir, pool)
    }

    fn event(agent: &str, kind: WhiteboardKind, payload: serde_json::Value) -> NewWhiteboardEvent {
        NewWhiteboardEvent {
            event_id: Ulid::new().to_string(),
            agent_id: agent.to_owned(),
            kind,
            scope: String::new(),
            session_id: None,
            plan_id: None,
            causation: None,
            payload,
            pre_image_hash: None,
            created_at: 1_700_000_000_000,
        }
    }

    fn decision_scope() -> Vec<WhiteboardScope> {
        vec![WhiteboardScope { topics: vec![WhiteboardKind::Decision] }]
    }

    async fn append(
        pool: &sqlx::SqlitePool,
        kind: WhiteboardKind,
        payload: serde_json::Value,
    ) -> WhiteboardEvent {
        append_whiteboard_event(pool, &event("agent-a", kind, payload))
            .await
            .expect("append succeeds")
    }

    #[tokio::test]
    async fn empty_log_yields_no_slice() {
        let (_dir, pool) = test_pool().await;
        let manager = SubscriptionManager::new(pool.clone());
        manager.register("sub".to_owned(), decision_scope()).await;
        assert!(manager.pending_slice("sub").await.is_none());
        assert!(manager.flush_candidates().await.is_empty());
    }

    #[tokio::test]
    async fn slice_matches_only_subscribed_kinds() {
        let (_dir, pool) = test_pool().await;
        append(&pool, WhiteboardKind::Decision, json!({"n": 1})).await;
        append(&pool, WhiteboardKind::Finding, json!({"n": 2})).await;
        append(&pool, WhiteboardKind::Decision, json!({"n": 3})).await;
        let manager = SubscriptionManager::new(pool.clone());
        manager.register("sub".to_owned(), decision_scope()).await;
        let batch = manager.pending_slice("sub").await.expect("slice present");
        assert_eq!(batch.events.len(), 2);
        assert!(batch.events.iter().all(|e| e.kind == WhiteboardKind::Decision));
        assert_eq!(batch.end_gate_seq, 3);
        assert!(!batch.more);
        manager.mark_flushed("sub", batch.end_gate_seq, batch.more).await;
        assert!(manager.flush_candidates().await.is_empty());
    }

    #[tokio::test]
    async fn empty_slice_advances_the_cut_past_non_matching_events() {
        let (_dir, pool) = test_pool().await;
        append(&pool, WhiteboardKind::Finding, json!({"n": 1})).await;
        append(&pool, WhiteboardKind::Finding, json!({"n": 2})).await;
        let manager = SubscriptionManager::new(pool.clone());
        manager.register("sub".to_owned(), decision_scope()).await;
        let batch = manager.pending_slice("sub").await.expect("window non-empty");
        assert!(batch.events.is_empty());
        assert_eq!(batch.end_gate_seq, 2, "cut advances over non-matching events");
    }

    #[tokio::test]
    async fn ack_advances_cursor_and_slice_continues_after_it() {
        let (_dir, pool) = test_pool().await;
        append(&pool, WhiteboardKind::Decision, json!({"n": 1})).await;
        append(&pool, WhiteboardKind::Decision, json!({"n": 2})).await;
        append(&pool, WhiteboardKind::Decision, json!({"n": 3})).await;
        let manager = SubscriptionManager::new(pool.clone());
        manager.register("sub".to_owned(), decision_scope()).await;
        manager.ack("sub", 1).await;
        let batch = manager.pending_slice("sub").await.expect("slice after ack");
        assert_eq!(batch.events.len(), 2);
        assert_eq!(batch.events[0].gate_seq, 2);
        assert_eq!(batch.end_gate_seq, 3);
    }

    #[tokio::test]
    async fn ack_never_lowers_the_cursor() {
        let (_dir, pool) = test_pool().await;
        append(&pool, WhiteboardKind::Decision, json!({"n": 1})).await;
        let manager = SubscriptionManager::new(pool.clone());
        manager.register("sub".to_owned(), decision_scope()).await;
        manager.ack("sub", 10).await;
        manager.ack("sub", 5).await;
        assert!(manager.pending_slice("sub").await.is_none(), "cursor stayed at 10");
    }

    #[tokio::test]
    async fn register_rehydrates_the_persisted_cursor() {
        let (_dir, pool) = test_pool().await;
        append(&pool, WhiteboardKind::Decision, json!({"n": 1})).await;
        append(&pool, WhiteboardKind::Decision, json!({"n": 2})).await;
        append(&pool, WhiteboardKind::Decision, json!({"n": 3})).await;
        upsert_whiteboard_subscription(
            &pool,
            &WhiteboardSubscription {
                subscriber_id: "sub".to_owned(),
                scopes: decision_scope(),
                cursor_gate_seq: 2,
            },
        )
        .await
        .expect("cursor upsert");
        let manager = SubscriptionManager::new(pool.clone());
        manager.register("sub".to_owned(), decision_scope()).await;
        let batch = manager.pending_slice("sub").await.expect("resume past cursor");
        assert_eq!(batch.events.len(), 1);
        assert_eq!(batch.events[0].gate_seq, 3);
    }

    #[tokio::test]
    async fn event_window_cap_keeps_continuation_dirty() {
        let (_dir, pool) = test_pool().await;
        for n in 0..70 {
            append(&pool, WhiteboardKind::Decision, json!({"n": n})).await;
        }
        let manager = SubscriptionManager::new(pool.clone());
        manager.register("sub".to_owned(), decision_scope()).await;
        let first = manager.pending_slice("sub").await.expect("first window");
        assert_eq!(first.events.len(), 64);
        assert_eq!(first.end_gate_seq, 64);
        assert!(first.more);
        manager.mark_flushed("sub", first.end_gate_seq, first.more).await;
        // The unacked span is not re-pushed: the overflow stalls delivery
        // until the subscriber acks the first window.
        assert!(manager.pending_slice("sub").await.is_none());
        manager.ack("sub", first.end_gate_seq).await;
        let second = manager.pending_slice("sub").await.expect("continuation");
        assert_eq!(second.events.len(), 6);
        assert_eq!(second.end_gate_seq, 70);
        assert!(!second.more);
    }

    #[tokio::test]
    async fn byte_cap_truncates_and_continues() {
        let (_dir, pool) = test_pool().await;
        for n in 0..12 {
            append(&pool, WhiteboardKind::Decision, json!({"n": n, "big": "x".repeat(30_000)}))
                .await;
        }
        let manager = SubscriptionManager::new(pool.clone());
        manager.register("sub".to_owned(), decision_scope()).await;
        let first = manager.pending_slice("sub").await.expect("byte-capped window");
        assert!(first.more);
        let first_bytes: usize =
            first.events.iter().map(|e| serde_json::to_vec(e).expect("serialize").len()).sum();
        assert!(first_bytes <= WHITEBOARD_SLICE_MAX_BYTES);
        assert!(first.events.len() < 12, "byte cap truncated the window");
        // Acknowledging advances the cut; the rest continues.
        manager.mark_flushed("sub", first.end_gate_seq, first.more).await;
        manager.ack("sub", first.end_gate_seq).await;
        let second = manager.pending_slice("sub").await.expect("continuation");
        assert_eq!(second.events.len() + first.events.len(), 12);
        assert_eq!(second.end_gate_seq, 12);
    }

    #[tokio::test]
    async fn unknown_subscriber_has_no_slice_or_candidates() {
        let (_dir, pool) = test_pool().await;
        let manager = SubscriptionManager::new(pool.clone());
        assert!(manager.pending_slice("nobody").await.is_none());
        manager.ack("nobody", 42).await;
        assert!(manager.flush_candidates().await.is_empty());
    }

    #[tokio::test]
    async fn mark_append_wakes_subscribers_ahead_of_their_cursor() {
        let (_dir, pool) = test_pool().await;
        let manager = SubscriptionManager::new(pool.clone());
        manager.register("sub-d".to_owned(), decision_scope()).await;
        manager
            .register(
                "sub-f".to_owned(),
                vec![WhiteboardScope { topics: vec![WhiteboardKind::Finding] }],
            )
            .await;
        // Wake is kind-agnostic by design (`pending_slice` filters; an empty
        // slice advances the cut past non-matching events).
        manager.mark_append(1).await;
        let candidates = manager.flush_candidates().await;
        assert!(candidates.contains(&"sub-d".to_owned()));
        assert!(candidates.contains(&"sub-f".to_owned()));
        // Draining one subscriber does not clear the other.
        manager.mark_flushed("sub-d", 1, false).await;
        let candidates = manager.flush_candidates().await;
        assert!(!candidates.contains(&"sub-d".to_owned()));
        assert!(candidates.contains(&"sub-f".to_owned()));
    }

    #[tokio::test]
    async fn mark_append_respects_the_cursor() {
        let (_dir, pool) = test_pool().await;
        append(&pool, WhiteboardKind::Decision, json!({"n": 1})).await;
        let manager = SubscriptionManager::new(pool.clone());
        manager.register("sub".to_owned(), decision_scope()).await;
        manager.ack("sub", 1).await;
        manager.mark_flushed("sub", 1, false).await;
        // A coarse wake at a gate_seq the cursor already reached does not
        // re-dirty the subscriber.
        manager.mark_append(1).await;
        assert!(manager.flush_candidates().await.is_empty());
        // A wake beyond the cursor does.
        manager.mark_append(2).await;
        assert!(manager.flush_candidates().await.contains(&"sub".to_owned()));
    }

    #[tokio::test]
    async fn mark_flushed_resets_dirty_only_when_drained() {
        let (_dir, pool) = test_pool().await;
        append(&pool, WhiteboardKind::Decision, json!({"n": 1})).await;
        let manager = SubscriptionManager::new(pool.clone());
        manager.register("sub".to_owned(), decision_scope()).await;
        assert!(manager.flush_candidates().await.contains(&"sub".to_owned()));
        manager.mark_flushed("sub", 1, false).await;
        assert!(manager.flush_candidates().await.is_empty());
        // A later append wakes it again.
        let ev = append(&pool, WhiteboardKind::Decision, json!({"n": 2})).await;
        manager.mark_append(ev.gate_seq).await;
        assert!(manager.flush_candidates().await.contains(&"sub".to_owned()));
    }
}
