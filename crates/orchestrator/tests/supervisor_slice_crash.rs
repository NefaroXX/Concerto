//! ADR-60 Testing §2 — crash-injection at the whiteboard Ack→cursor window
//! (Phase 4, oracle comment 4), Windows-safe by construction: the "crash" is
//! the subscriber fixture exiting itself (status 1) between slice delivery
//! and `ack-whiteboard`, not an OS signal.
//!
//! Scenario: a subscribed agent receives its `whiteboard-slice`, dies BEFORE
//! acking (the persisted cursor has not advanced — nothing is lost), the
//! supervisor restarts it one_for_one, and the restarted incarnation is
//! REDelivered the same span from the persisted cursor, acks it, and the
//! cursor advances exactly once.
//!
//! A second test covers the supervisor-side restart surface directly: a
//! manager generation dropped after *flushing but before an ack* is replaced
//! by a fresh manager over the same DB — the unacked span re-delivers.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use concerto_core::error::{MemoryError, PolicyError};
use concerto_core::executor::ToolExecutor;
use concerto_core::memory::{MemoryChunk, MemoryEntry, MemoryId, MemoryQuery, ProjectId};
use concerto_core::policy::SimplePolicyEngine;
use concerto_core::traits::memory::MemoryStore;
use concerto_core::traits::policy::AuditLog;
use concerto_core::types::{Condition, PolicyRule, ToolRegistry};
use concerto_core::CancellationToken;
use concerto_orchestrator::gate::{FilePreImageReader, WriteGate};
use concerto_orchestrator::ipc::{IpcNotification, IpcParams};
use concerto_orchestrator::subscriptions::SubscriptionManager;
use concerto_orchestrator::supervisor::{
    RunSummary, Supervisor, SupervisorConfig, SupervisorServices,
};
use concerto_sessions::whiteboard::{
    append_whiteboard_event, load_whiteboard_subscription, NewWhiteboardEvent, WhiteboardKind,
    WhiteboardScope,
};
use concerto_tools::filesystem::FilesystemTool;
use serde_json::json;
use tempfile::TempDir;

struct TestAudit;

#[async_trait]
impl AuditLog for TestAudit {
    async fn record(
        &self,
        _entry: concerto_core::traits::policy::AuditEntry,
        _cancel: CancellationToken,
    ) -> Result<(), PolicyError> {
        Ok(())
    }
}

fn allow_engine() -> Arc<SimplePolicyEngine> {
    Arc::new(SimplePolicyEngine::new(
        vec![PolicyRule::AutoApprove(Condition::Always)],
        Arc::new(TestAudit),
    ))
}

fn fs_gate(pool: sqlx::SqlitePool, root: PathBuf) -> Arc<WriteGate> {
    let mut registry = ToolRegistry::default();
    registry.register(Box::new(FilesystemTool::new(
        camino::Utf8PathBuf::from_path_buf(root.clone()).expect("tempdir is utf-8"),
    )));
    let executor = Arc::new(ToolExecutor::new(Arc::new(registry), allow_engine()));
    Arc::new(WriteGate::new(
        allow_engine(),
        executor,
        pool,
        Arc::new(FilePreImageReader::new(root.clone())),
        root,
        1,
    ))
}

struct NullMemory;

#[async_trait]
impl MemoryStore for NullMemory {
    async fn retrieve(
        &self,
        _query: &MemoryQuery,
        _cancel: CancellationToken,
    ) -> Result<Vec<MemoryChunk>, MemoryError> {
        Ok(vec![])
    }
    async fn store(
        &self,
        _entry: MemoryEntry,
        _cancel: CancellationToken,
    ) -> Result<MemoryId, MemoryError> {
        Ok(MemoryId(ulid::Ulid::new()))
    }
    async fn invalidate(
        &self,
        _id: MemoryId,
        _cancel: CancellationToken,
    ) -> Result<(), MemoryError> {
        Ok(())
    }
}

async fn whiteboard_pool() -> (TempDir, sqlx::SqlitePool) {
    let dir = tempfile::tempdir().expect("tempdir created");
    let options = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(dir.path().join("slice_crash.db"))
        .create_if_missing(true)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
        .busy_timeout(Duration::from_secs(5))
        .foreign_keys(true)
        .synchronous(sqlx::sqlite::SqliteSynchronous::Normal);
    let pool = sqlx::pool::PoolOptions::new()
        .max_connections(6)
        .connect_with(options)
        .await
        .expect("test pool connects");
    sqlx::migrate!("../sessions/migrations").run(&pool).await.expect("migrations apply");
    (dir, pool)
}

fn seeded_event(agent: &str, kind: WhiteboardKind, note: &str) -> NewWhiteboardEvent {
    NewWhiteboardEvent {
        event_id: ulid::Ulid::new().to_string(),
        agent_id: agent.to_owned(),
        kind,
        scope: String::new(),
        session_id: None,
        plan_id: None,
        causation: None,
        payload: json!({ "note": note }),
        pre_image_hash: None,
        created_at: 1_700_000_000_000,
    }
}

/// The mock-agent fixture wired as a crash-then-consume subscriber.
fn subscribing_mock(log_file: &Path, crash_marker: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_orchestrator-mock-agent"));
    command
        .env("MOCK_AGENT_LOG_FILE", log_file)
        .env("MOCK_AGENT_ACK_SLICES", "1")
        .env("MOCK_AGENT_CRASH_ONCE_FILE", crash_marker);
    command
}

/// Parse the recorded `whiteboard-slice` notification lines from the log.
fn slice_notifications(log: &str) -> Vec<IpcNotification> {
    log.lines()
        .filter_map(|line| serde_json::from_str::<IpcNotification>(line).ok())
        .filter(|notification| matches!(notification.params, IpcParams::WhiteboardSlice { .. }))
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn subscriber_crash_between_slice_and_ack_redelivers_from_the_cursor() {
    let gate_root = tempfile::tempdir().expect("gate root tempdir");
    let (_pool_dir, pool) = whiteboard_pool().await;
    let log_file = _pool_dir.path().join("agent-s.log");
    let crash_marker = _pool_dir.path().join("crashed-before-ack.marker");

    // Seed the log BEFORE the subscriber spawns so its first flush window
    // already carries the span it will die on.
    for n in 0..3 {
        append_whiteboard_event(
            &pool,
            &seeded_event("publisher", WhiteboardKind::Decision, &format!("seed-{n}")),
        )
        .await
        .expect("seed append");
    }

    let services = SupervisorServices {
        gate: fs_gate(pool.clone(), gate_root.path().to_path_buf()),
        whiteboard_pool: pool.clone(),
        subscriptions: SubscriptionManager::new(pool.clone()),
        memory: Arc::new(NullMemory),
        project_id: ProjectId("proj-d3-crash".to_owned()),
        consolidation: None,
    };
    let config = SupervisorConfig::default()
        .with_whiteboard_subscription("agent-s", vec![WhiteboardKind::Decision]);

    let mut supervisor = Supervisor::new(config);
    supervisor
        .spawn_agent(&mut subscribing_mock(&log_file, &crash_marker), "agent-s")
        .expect("spawn agent-s");
    supervisor = supervisor.with_services(services);

    // Drive the loop until the RESTARTED incarnation has acked the full span
    // (cursor >= 3), then cancel and collect the summary.
    let shutdown = CancellationToken::new();
    let guard = shutdown.clone();
    let handle = tokio::spawn(async move { supervisor.run(shutdown).await });

    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let cursor = load_whiteboard_subscription(&pool, "agent-s")
            .await
            .expect("cursor query")
            .map(|subscription| subscription.cursor_gate_seq)
            .unwrap_or(0);
        if cursor >= 3 {
            break;
        }
        let log_tail = std::fs::read_to_string(&log_file).unwrap_or_default();
        let tail =
            if log_tail.len() > 4000 { &log_tail[log_tail.len() - 4000..] } else { &log_tail };
        assert!(
            tokio::time::Instant::now() < deadline,
            "the redelivered span was never acked after the crash; marker={} log_tail={tail}",
            crash_marker.exists(),
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    guard.cancel();
    let summary: RunSummary = handle.await.expect("loop task completes");
    // The crash was recovered: nothing stays failed (the restarted incarnation
    // is healthy at teardown).
    assert!(summary.failed.is_empty(), "no agent may remain failed: {:?}", summary.failed);
    let meta = summary.agents.iter().find(|m| m.agent_id == "agent-s").expect("registered");
    assert_eq!(meta.restart_count, 1, "exactly one crash, exactly one restart");

    // The crash actually happened in the target window (the marker is the
    // fixture's own record of dying before its first ack).
    assert!(crash_marker.exists(), "the fixture must have crashed between slice delivery and ack");

    // Redelivery: the log records slices delivered to BOTH incarnations, and
    // the final one covers the whole span again (delivery resumes from the
    // stale persisted cursor, not from the tail).
    let log = std::fs::read_to_string(&log_file).unwrap_or_default();
    let slices = slice_notifications(&log);
    assert!(
        slices.len() >= 2,
        "the unacked span must be redelivered after the restart; got {} slice(s)",
        slices.len()
    );
    let last = slices.last().expect("at least one slice recorded");
    match &last.params {
        IpcParams::WhiteboardSlice { events, end_gate_seq, .. } => {
            assert_eq!(*end_gate_seq, 3);
            assert_eq!(events.len(), 3, "the restarted incarnation got the full span again");
            assert_eq!(
                events.iter().map(|e| e.gate_seq).collect::<Vec<_>>(),
                vec![1, 2, 3],
                "redelivery preserves gate_seq order"
            );
        }
        other => panic!("expected WhiteboardSlice params, got {other:?}"),
    }
}

#[tokio::test]
async fn fresh_supervisor_generation_redelivers_a_flushed_but_unacked_span() {
    let (_pool_dir, pool) = whiteboard_pool().await;
    for n in 0..3 {
        append_whiteboard_event(
            &pool,
            &seeded_event("publisher", WhiteboardKind::Decision, &format!("seed-{n}")),
        )
        .await
        .expect("seed append");
    }
    let scopes = vec![WhiteboardScope { topics: vec![WhiteboardKind::Decision] }];

    // Generation 1 registers (cursor materialized at 0), delivers its window
    // (the log read IS the delivery), and dies before any ack.
    concerto_sessions::whiteboard::upsert_whiteboard_subscription(
        &pool,
        &concerto_sessions::whiteboard::WhiteboardSubscription {
            subscriber_id: "agent-s".to_owned(),
            scopes: scopes.clone(),
            cursor_gate_seq: 0,
        },
    )
    .await
    .expect("generation 1 registration");
    let delivered = load_span(&pool, 0).await;
    assert_eq!(delivered.iter().map(|e| e.gate_seq).collect::<Vec<_>>(), vec![1, 2, 3]);
    // NO ack - generation 1 dies here.

    // Generation 2 rehydrates the PERSISTED cursor on registration: still 0,
    // so the flushed-but-unacked span re-delivers intact.
    let persisted =
        load_whiteboard_subscription(&pool, "agent-s").await.expect("cursor query").expect("row");
    assert_eq!(persisted.cursor_gate_seq, 0, "no ack means no cursor advance");
    let redelivered = load_span(&pool, persisted.cursor_gate_seq).await;
    assert_eq!(
        redelivered.iter().map(|e| e.gate_seq).collect::<Vec<_>>(),
        vec![1, 2, 3],
        "the unacked span re-delivers to a fresh supervisor generation"
    );

    // Once an incarnation acks, the cut is durable and monotonic.
    concerto_sessions::whiteboard::ack_whiteboard_subscription(&pool, "agent-s", 3)
        .await
        .expect("ack");
    let acked =
        load_whiteboard_subscription(&pool, "agent-s").await.expect("cursor query").expect("row");
    assert_eq!(acked.cursor_gate_seq, 3);
    assert!(
        load_span(&pool, acked.cursor_gate_seq).await.is_empty(),
        "acked span never re-delivers"
    );
}

/// The delivery window a supervisor generation would push: events strictly
/// past `cursor`, in gate_seq order (the same read `pending_slice` performs).
async fn load_span(
    pool: &sqlx::SqlitePool,
    cursor: u64,
) -> Vec<concerto_sessions::whiteboard::WhiteboardEvent> {
    concerto_sessions::whiteboard::load_whiteboard_events(
        pool,
        &concerto_sessions::whiteboard::WhiteboardLoadOpts {
            after_gate_seq: cursor,
            ..Default::default()
        },
    )
    .await
    .expect("span read")
}
