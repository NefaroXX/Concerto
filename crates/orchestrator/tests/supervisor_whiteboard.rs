//! ADR-60 D3 whiteboard subscription push — supervisor side e2e.
//!
//! These drive a real `orchestrator-mock-agent` OS process through the
//! supervisor's steady-state loop and assert on the *wire*: exactly what the
//! supervisor wrote to the child's stdin (`MOCK_AGENT_LOG_FILE` records every
//! received line). The mock replies to the handshake but never acks slices —
//! so the ack-advancing the cursor, redelivery-after-crash, and overflow
//! continuation scenarios (which need a consuming agent) land in the
//! child-side gate-proxy slice; this file pins the supervisor half: correct
//! slice framing, ordering, and the at-least-once rule that an unacked span
//! is NOT re-sent on the loop's own cadence.

use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

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
use concerto_sessions::whiteboard::{append_whiteboard_event, NewWhiteboardEvent, WhiteboardKind};
use concerto_tools::filesystem::FilesystemTool;
use serde_json::json;
use sqlx::pool::PoolOptions;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqliteSynchronous};
use tempfile::TempDir;

/// No-op audit log (mirrors the other supervisor e2e suites).
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

/// A policy engine that allows everything.
fn allow_engine() -> Arc<SimplePolicyEngine> {
    Arc::new(SimplePolicyEngine::new(
        vec![PolicyRule::AutoApprove(Condition::Always)],
        Arc::new(TestAudit),
    ))
}

/// A gate whose executor is the real `FilesystemTool` rooted at `root`
/// (unused by these tests — the field is required by `SupervisorServices`).
fn fs_gate(pool: sqlx::SqlitePool, root: &Path) -> Arc<WriteGate> {
    let mut registry = ToolRegistry::default();
    registry.register(Box::new(FilesystemTool::new(
        camino::Utf8PathBuf::from_path_buf(root.to_path_buf()).expect("tempdir is utf-8"),
    )));
    let executor = Arc::new(ToolExecutor::new(Arc::new(registry), allow_engine()));
    Arc::new(WriteGate::new(
        allow_engine(),
        executor,
        pool,
        Arc::new(FilePreImageReader::new(root.to_path_buf())),
        root.to_path_buf(),
        1,
    ))
}

/// Memory spine stub (mirrors the other supervisor e2e suites).
struct CountingMemoryStore {
    retrievals: AtomicUsize,
}

impl CountingMemoryStore {
    fn new() -> Self {
        Self { retrievals: AtomicUsize::new(0) }
    }
}

#[async_trait]
impl MemoryStore for CountingMemoryStore {
    async fn retrieve(
        &self,
        _query: &MemoryQuery,
        _cancel: CancellationToken,
    ) -> Result<Vec<MemoryChunk>, MemoryError> {
        self.retrievals.fetch_add(1, Ordering::SeqCst);
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

/// A sqlite pool over the whiteboard schema in a temp dir.
async fn whiteboard_pool(max_connections: u32) -> (TempDir, sqlx::SqlitePool) {
    let dir = tempfile::tempdir().expect("tempdir created");
    let path = dir.path().join("supervisor_whiteboard.db");
    let options = SqliteConnectOptions::new()
        .filename(&path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .busy_timeout(Duration::from_secs(5))
        .foreign_keys(true)
        .synchronous(SqliteSynchronous::Normal);
    let pool = PoolOptions::new()
        .max_connections(max_connections)
        .connect_with(options)
        .await
        .expect("test pool connects");
    sqlx::migrate!("../sessions/migrations").run(&pool).await.expect("migrations apply");
    (dir, pool)
}

/// The mock agent with the wire-recording knob, registered as `agent_id`.
fn recording_mock(log_file: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_orchestrator-mock-agent"));
    command.env("MOCK_AGENT_LOG_FILE", log_file);
    command
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

/// Parse the recorded `whiteboard-slice` notification lines from the log.
fn slice_notifications(log: &str) -> Vec<IpcNotification> {
    log.lines()
        .filter_map(|line| serde_json::from_str::<IpcNotification>(line).ok())
        .filter(|n| matches!(n.params, IpcParams::WhiteboardSlice { .. }))
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn supervisor_pushes_one_bounded_slice_and_never_resends_an_unacked_span() {
    let gate_root = tempfile::tempdir().expect("gate root tempdir");
    let (_pool_dir, pool) = whiteboard_pool(6).await;
    // Seed the log BEFORE the subscriber spawns: the registration marks the
    // subscriber dirty (cursor 0), so the loop's first tick pushes a slice.
    for n in 0..3 {
        append_whiteboard_event(
            &pool,
            &seeded_event("writer-a", WhiteboardKind::Decision, &format!("seed-{n}")),
        )
        .await
        .expect("seed append");
    }

    let log_file = _pool_dir.path().join("agent-a.log");
    let services = SupervisorServices {
        gate: fs_gate(pool.clone(), gate_root.path()),
        whiteboard_pool: pool.clone(),
        subscriptions: SubscriptionManager::new(pool.clone()),
        memory: Arc::new(CountingMemoryStore::new()),
        project_id: ProjectId("proj-d3-flush".to_owned()),
    };
    let config = SupervisorConfig::default()
        .with_whiteboard_subscription("agent-a", vec![WhiteboardKind::Decision]);

    let mut command = recording_mock(&log_file);
    let mut supervisor = Supervisor::new(config);
    supervisor.spawn_agent(&mut command, "agent-a").expect("spawn agent-a");
    supervisor = supervisor.with_services(services);

    let shutdown = CancellationToken::new();
    // Run until the slice is recorded AND a few ticks have passed with no
    // re-send — proving the unacked span is not re-pushed on the loop cadence.
    let start = Instant::now();
    let seen = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let seen_clone = seen.clone();
    let log_clone = log_file.clone();
    let summary: RunSummary = supervisor
        .run_until(shutdown.clone(), move |_| {
            let content = std::fs::read_to_string(&log_clone).unwrap_or_default();
            if !content.contains("whiteboard-slice") {
                return false;
            }
            seen_clone.store(true, Ordering::SeqCst);
            start.elapsed() >= Duration::from_millis(500)
        })
        .await;

    shutdown.cancel();
    assert!(seen.load(Ordering::SeqCst), "a whiteboard-slice line must reach the child");
    assert!(summary.failed.is_empty(), "agents may not fail: {:?}", summary.failed);

    let log = std::fs::read_to_string(&log_file).expect("log readable");
    let slices = slice_notifications(&log);
    assert_eq!(slices.len(), 1, "exactly one slice — the unacked span must not be resent: {log}");
    match &slices[0].params {
        IpcParams::WhiteboardSlice { subscription_id, events, end_gate_seq } => {
            assert_eq!(subscription_id, "agent-a");
            assert_eq!(*end_gate_seq, 3, "cut covers all three seeded events");
            assert_eq!(events.len(), 3, "all subscribed-kind events delivered");
            assert!(events.iter().all(|e| e.kind == WhiteboardKind::Decision));
            assert_eq!(events[0].payload["note"], json!("seed-0"));
            assert_eq!(events[2].payload["note"], json!("seed-2"), "ordering = gate_seq order");
        }
        other => panic!("expected WhiteboardSlice params, got {other:?}"),
    }
}
