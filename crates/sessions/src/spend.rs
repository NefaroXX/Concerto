//! Session-level spend types and re-export of the single SpendTracker.
//!
//! The authoritative multi-level (session / task / daily) SpendTracker lives in
//! `concerto_core::policy`. This module keeps only the persistence / reporting
//! types that the session store needs and re-exports the core tracker so that
//! there is exactly one implementation.

use concerto_core::event::{EventBus, EventKind};
use concerto_core::ids::Ulid;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

// Re-export the single source of truth.
pub use concerto_core::SpendTracker;

/// Raw spend record per provider call (persisted by the session store).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpendRecord {
    pub id: Ulid,
    pub session_id: Ulid,
    pub task_id: Option<Ulid>,
    pub provider: String,
    pub model: String,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub cost_usd: f64,
    pub created_at: OffsetDateTime,
}

/// Aggregated spend summary for a session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpendSummary {
    pub session_id: Ulid,
    pub total_cost_usd: f64,
    pub total_tokens_in: u64,
    pub total_tokens_out: u64,
    pub record_count: u64,
}

/// Spend cap check result (used by session-level guards / UI).
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum CapStatus {
    Ok,
    Approaching { current_usd: f64, cap_usd: f64, pct: f64 },
    Exceeded { current_usd: f64, cap_usd: f64 },
}

/// Kind of spend cap that was exceeded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum CapKind {
    Session,
    Task,
    Daily,
}

/// Result of a cap check (richer form used by session code).
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum CapResult {
    Ok,
    Approaching { pct: f64, cap_usd: f64 },
    Exceeded { cap_kind: CapKind, spent: f64, cap: f64 },
}

/// Accumulates spend records and checks against caps, emitting events.
pub struct SpendAccumulator {
    session_id: Ulid,
    bus: EventBus,
}

impl SpendAccumulator {
    pub fn new(session_id: Ulid, bus: EventBus) -> Self {
        Self { session_id, bus }
    }

    /// Check running total against cap, emitting events as thresholds are
    /// crossed.
    pub async fn check_cap(&self, current_total: f64, cap_usd: Option<f64>) -> CapStatus {
        let Some(cap) = cap_usd else {
            return CapStatus::Ok;
        };
        if cap <= 0.0 {
            return CapStatus::Ok;
        }

        let pct = (current_total / cap) * 100.0;

        if pct >= 100.0 {
            let _ = self.bus.publish_for_session(
                self.session_id,
                self.session_id,
                EventKind::SpendCapExceeded {
                    session_id: self.session_id,
                    current_usd: current_total,
                    cap_usd: cap,
                },
            );
            CapStatus::Exceeded { current_usd: current_total, cap_usd: cap }
        } else if pct >= 80.0 {
            let _ = self.bus.publish_for_session(
                self.session_id,
                self.session_id,
                EventKind::SpendCapApproaching { current_usd: current_total, cap_usd: cap, pct },
            );
            CapStatus::Approaching { current_usd: current_total, cap_usd: cap, pct }
        } else {
            CapStatus::Ok
        }
    }
}

/// Spend guard wraps `SpendAccumulator` to check before each call.
pub struct SpendGuard {
    accumulator: SpendAccumulator,
    cap_usd: Option<f64>,
}

impl SpendGuard {
    pub fn new(session_id: Ulid, bus: EventBus, cap_usd: Option<f64>) -> Self {
        Self { accumulator: SpendAccumulator::new(session_id, bus), cap_usd }
    }

    /// Check whether spend cap allows another call.
    /// Returns `CapStatus::Exceeded` if cap is already reached or exceeded.
    pub async fn check_before_call(&self, current_total: f64) -> CapStatus {
        self.accumulator.check_cap(current_total, self.cap_usd).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spend_round_trip() {
        let record = SpendRecord {
            id: Ulid::new(),
            session_id: Ulid::new(),
            task_id: None,
            provider: "openai".into(),
            model: "gpt-4".into(),
            tokens_in: 100,
            tokens_out: 50,
            cost_usd: 0.002,
            created_at: OffsetDateTime::now_utc(),
        };
        let json = serde_json::to_string(&record).unwrap();
        let deserialized: SpendRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.provider, "openai");
        assert_eq!(deserialized.cost_usd, 0.002);
        assert!(deserialized.task_id.is_none());
    }

    #[tokio::test]
    async fn cap_not_exceeded_when_no_cap() {
        let bus = EventBus::default();
        let acc = SpendAccumulator::new(Ulid::new(), bus);
        let status = acc.check_cap(0.5, None).await;
        assert_eq!(status, CapStatus::Ok);
    }

    #[tokio::test]
    async fn cap_exceeded_when_over_limit() {
        let bus = EventBus::default();
        let mut events = bus.subscribe_durable();
        let session_id = Ulid::new();
        let acc = SpendAccumulator::new(session_id, bus);
        let status = acc.check_cap(1.0, Some(0.50)).await;
        assert_eq!(status, CapStatus::Exceeded { current_usd: 1.0, cap_usd: 0.50 });
        let event = events.recv().await.unwrap();
        assert_eq!(event.session_id, session_id);
    }

    #[tokio::test]
    async fn cap_approaching_at_80_percent() {
        let bus = EventBus::default();
        let acc = SpendAccumulator::new(Ulid::new(), bus);
        let status = acc.check_cap(0.80, Some(1.0)).await;
        match status {
            CapStatus::Approaching { pct, .. } => assert!((pct - 80.0).abs() < 1.0),
            other => panic!("expected Approaching, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // New tests added below (8 tests)
    // -----------------------------------------------------------------------

    #[test]
    /// SpendSummary serializes to JSON and deserializes back without data loss.
    fn spend_summary_serialization_round_trip() {
        let summary = SpendSummary {
            session_id: Ulid::new(),
            total_cost_usd: 1.23,
            total_tokens_in: 500,
            total_tokens_out: 300,
            record_count: 10,
        };
        let json = serde_json::to_string(&summary).unwrap();
        let deserialized: SpendSummary = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.session_id, summary.session_id);
        assert!((deserialized.total_cost_usd - 1.23).abs() < f64::EPSILON);
        assert_eq!(deserialized.total_tokens_in, 500);
        assert_eq!(deserialized.total_tokens_out, 300);
        assert_eq!(deserialized.record_count, 10);
    }

    #[tokio::test]
    /// A cap of exactly 0.0 should be treated as "no cap" and always return
    /// `CapStatus::Ok`.
    async fn spend_accumulator_zero_cap() {
        let bus = EventBus::default();
        let acc = SpendAccumulator::new(Ulid::new(), bus);
        // Any spend should be Ok when cap is 0 (disabled).
        let status = acc.check_cap(100.0, Some(0.0)).await;
        assert_eq!(status, CapStatus::Ok);
    }

    #[tokio::test]
    /// A negative cap should be treated as "no cap" and always return
    /// `CapStatus::Ok`.
    async fn spend_accumulator_negative_cap() {
        let bus = EventBus::default();
        let acc = SpendAccumulator::new(Ulid::new(), bus);
        let status = acc.check_cap(50.0, Some(-1.0)).await;
        assert_eq!(status, CapStatus::Ok);
    }

    #[tokio::test]
    /// Exactly 80 % of cap should be classified as `Approaching`.
    async fn spend_accumulator_boundary_exactly_80_percent() {
        let bus = EventBus::default();
        let acc = SpendAccumulator::new(Ulid::new(), bus);
        // current_total = 80, cap = 100  => 80.0% exactly.
        let status = acc.check_cap(80.0, Some(100.0)).await;
        match status {
            CapStatus::Approaching { pct, current_usd, cap_usd } => {
                assert!((pct - 80.0).abs() < f64::EPSILON);
                assert!((current_usd - 80.0).abs() < f64::EPSILON);
                assert!((cap_usd - 100.0).abs() < f64::EPSILON);
            }
            other => panic!("expected Approaching, got {other:?}"),
        }
    }

    #[tokio::test]
    /// Exactly 100 % of cap should be classified as `Exceeded`.
    async fn spend_accumulator_boundary_exactly_100_percent() {
        let bus = EventBus::default();
        let acc = SpendAccumulator::new(Ulid::new(), bus);
        let status = acc.check_cap(100.0, Some(100.0)).await;
        assert_eq!(status, CapStatus::Exceeded { current_usd: 100.0, cap_usd: 100.0 });
    }

    #[tokio::test]
    /// Dropping a `SpendGuard` and creating a new one should work,
    /// demonstrating that no locked state is leaked.
    async fn spend_guard_drop_releases_lock() {
        let bus = EventBus::default();
        let guard = SpendGuard::new(Ulid::new(), bus, Some(10.0));
        let status = guard.check_before_call(5.0).await;
        assert_eq!(status, CapStatus::Ok);
        // Drop the guard.
        drop(guard);
        // Create and use a new guard — no poisoned state.
        let bus2 = EventBus::default();
        let guard2 = SpendGuard::new(Ulid::new(), bus2, Some(10.0));
        let status2 = guard2.check_before_call(12.0).await;
        assert_eq!(status2, CapStatus::Exceeded { current_usd: 12.0, cap_usd: 10.0 });
    }

    #[tokio::test]
    /// `SpendTracker` handles concurrent `check_and_add` calls correctly.
    async fn spend_tracker_concurrent_records() {
        use std::sync::Arc;
        let tracker = Arc::new(SpendTracker::new(Some(100.0), Some(50.0), None));
        let mut handles = Vec::new();
        for _ in 0..10 {
            let t = Arc::clone(&tracker);
            handles.push(tokio::spawn(async move { t.check_and_add(1.0) }));
        }
        for h in handles {
            let result = h.await.unwrap();
            assert!(result.is_ok(), "concurrent record should succeed");
        }
    }

    #[test]
    /// SpendRecord serialization round-trip with a task_id set.
    fn spend_record_with_task_id() {
        let record = SpendRecord {
            id: Ulid::new(),
            session_id: Ulid::new(),
            task_id: Some(Ulid::new()),
            provider: "anthropic".into(),
            model: "claude-3".into(),
            tokens_in: 200,
            tokens_out: 100,
            cost_usd: 0.015,
            created_at: OffsetDateTime::now_utc(),
        };
        let json = serde_json::to_string(&record).unwrap();
        let deserialized: SpendRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.provider, "anthropic");
        assert_eq!(deserialized.model, "claude-3");
        assert_eq!(deserialized.tokens_in, 200);
        assert_eq!(deserialized.tokens_out, 100);
        assert!((deserialized.cost_usd - 0.015).abs() < f64::EPSILON);
        assert!(deserialized.task_id.is_some());
    }
}
