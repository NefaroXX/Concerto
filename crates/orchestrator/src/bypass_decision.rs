//! Decision-shaped records for bypass/abort sites (supremacy invariant).
//!
//! Every path that would otherwise end by silently aborting a run — a cleared
//! checkpoint, a loop cap, an operator-visible drop, a pre-coordinator setup
//! failure — must end in a *Coordinator Decision* (or a documented terminal
//! class such as cancellation) rather than a bare `?`/return. This module owns
//! the write seam those sites share so the shape stays identical everywhere:
//!
//! - a `coordinator_decision` audit row via
//!   [`concerto_core::ToolExecutor::record_coordinator_decision`] (when an
//!   executor is available — the run's single executor is the audit writer);
//! - an ADR-65 whiteboard `Decision` event (when the session DB pool exists).
//!
//! Both writes are **fail-soft by contract**: a continuity/audit failure must
//! never fail the run or change the control-flow outcome the caller already
//! decided. This mirrors [`crate::coordinator`]'s
//! `record_run_shape_decision` shape — the difference is only that these
//! records originate outside the coordinator's own loop, so they are emitted
//! through a free function instead of a method on the coordinator.

use concerto_core::ids::Ulid;
use concerto_core::ToolExecutor;
use concerto_sessions::whiteboard::{append_whiteboard_event, NewWhiteboardEvent, WhiteboardKind};

/// Emit the shared decision-shaped records for one bypass/abort site.
///
/// `decision` is the stable machine code (e.g. `checkpoint-cleared-scope`),
/// `reason` is the human-readable detail. Both writes are fail-soft: an audit
/// failure is logged and otherwise ignored, and a missing executor or pool
/// simply skips that leg. Callers use this *before* the aborted action so the
/// record is durable even if the action fails.
pub(crate) async fn record_decision_row(
    executor: Option<&ToolExecutor>,
    pool: Option<&sqlx::SqlitePool>,
    session_id: Ulid,
    decision: &str,
    reason: &str,
) {
    if let Some(executor) = executor {
        executor
            .record_coordinator_decision(
                session_id,
                Ulid::new(),
                decision,
                reason,
                concerto_core::CancellationToken::new(),
            )
            .await;
    }
    if let Some(pool) = pool {
        let event = NewWhiteboardEvent {
            event_id: Ulid::new().to_string(),
            agent_id: "coordinator".to_owned(),
            kind: WhiteboardKind::Decision,
            scope: String::new(),
            session_id: Some(session_id.to_string()),
            plan_id: None,
            causation: None,
            payload: serde_json::json!({
                "selected_agent": "",
                "reason": reason,
                "required_output": decision,
                "supporting_evidence_ids": [],
            }),
            pre_image_hash: None,
            created_at: crate::tool_facts::unix_ms(),
        };
        if let Err(error) = append_whiteboard_event(pool, &event).await {
            tracing::warn!(%error, decision, "bypass decision whiteboard append failed (fail-soft)");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn missing_sinks_are_a_fail_soft_noop() {
        // No executor and no pool: the call must return without panicking.
        record_decision_row(None, None, Ulid::new(), "noop", "nothing to write").await;
    }

    #[tokio::test]
    async fn whiteboard_decision_is_appended_when_a_pool_exists() {
        use sqlx::sqlite::SqlitePoolOptions;
        use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqliteSynchronous};

        let dir = tempfile::tempdir().expect("tempdir created");
        let path = dir.path().join("bypass_decision.db");
        let options = SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .busy_timeout(std::time::Duration::from_secs(5))
            .foreign_keys(true)
            .synchronous(SqliteSynchronous::Normal);
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(options)
            .await
            .expect("test pool connects");
        sqlx::migrate!("../sessions/migrations").run(&pool).await.expect("migrations apply");

        let session = Ulid::new();
        record_decision_row(
            None,
            Some(&pool),
            session,
            "checkpoint-cleared-scope",
            "the resume checkpoint did not match this project",
        )
        .await;

        let events = concerto_sessions::whiteboard::load_whiteboard_events(
            &pool,
            &concerto_sessions::whiteboard::WhiteboardLoadOpts::default(),
        )
        .await
        .expect("load whiteboard");
        let decisions: Vec<_> =
            events.iter().filter(|event| event.kind == WhiteboardKind::Decision).collect();
        assert_eq!(decisions.len(), 1, "exactly one Decision event");
        assert_eq!(decisions[0].agent_id, "coordinator");
        assert_eq!(decisions[0].session_id.as_deref(), Some(session.to_string().as_str()));
        assert_eq!(
            decisions[0].payload.get("required_output").and_then(serde_json::Value::as_str),
            Some("checkpoint-cleared-scope"),
        );
    }
}
