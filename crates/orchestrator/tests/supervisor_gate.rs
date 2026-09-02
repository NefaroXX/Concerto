//! Supervisor loop ↔ write-path services integration — ADR-60 S4c.
//!
//! Drives the *real* steady-state loop ([`Supervisor::run`]) against the
//! mock-agent fixture child, with the write gate, whiteboard pool and memory
//! spine attached ([`SupervisorServices`]). These tests prove the end-to-end
//! contracts of D3/D4/D6 over the wire:
//!
//! - `execute-tool` requests flow through the single gate: policy evaluation,
//!   whiteboard sequencing (`gate_seq` / per-agent `agent_seq`), and the
//!   persisted `WriteApplied` row;
//! - attribution is bound to the *registered* agent id at the process
//!   boundary — the mock deliberately spoofs `agent_id` on the wire
//!   (`spoofed-agent`) and every stored row must still say `agent-a`;
//! - denied writes persist nothing (fail-closed, and the agent is not
//!   treated as failed);
//! - `publish-event` requests append to the whiteboard with the same
//!   sequencing, and both paths share one global `gate_seq` order;
//! - `retrieve-memory` requests reach the memory spine exactly once per
//!   request;
//! - `store-memory` / `invalidate-memory` requests reach the memory spine
//!   exactly once per request, with entries bound to the supervisor's
//!   project scoping (ADR-60 D6).

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use concerto_core::error::{MemoryError, PolicyError, ToolError};
use concerto_core::executor::ToolExecutor;
use concerto_core::memory::{
    ChunkType, MemoryChunk, MemoryEntry, MemoryId, MemoryNamespace, MemoryQuery, ProjectId,
};
use concerto_core::policy::SimplePolicyEngine;
use concerto_core::traits::memory::MemoryStore;
use concerto_core::traits::policy::{AuditLog, PolicyEngine};
use concerto_core::traits::tool::Tool;
use concerto_core::types::{
    CapabilitySet, Condition, PolicyRule, SessionContext, ToolOutput, ToolRegistry,
};
use concerto_core::CancellationToken;
use concerto_orchestrator::gate::{FilePreImageReader, WriteGate};
use concerto_orchestrator::subscriptions::SubscriptionManager;
use concerto_orchestrator::supervisor::{
    AgentState, Supervisor, SupervisorConfig, SupervisorServices,
};
use concerto_sessions::whiteboard::{
    load_whiteboard_events, WhiteboardEvent, WhiteboardKind, WhiteboardLoadOpts,
};
use serde_json::json;
use sqlx::pool::PoolOptions;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqliteSynchronous};
use tempfile::TempDir;

/// The mock-agent fixture binary, speaking the ADR-60 D2 handshake.
fn mock_agent() -> Command {
    Command::new(env!("CARGO_BIN_EXE_orchestrator-mock-agent"))
}

/// The mock-agent fixture binary with one integer knob set.
fn mock_agent_with(knob: &str, value: u64) -> Command {
    let mut command = mock_agent();
    command.env(knob, value.to_string());
    command
}

/// No-op audit log for tests (mirrors `agent_loop`'s `TestAudit`).
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

/// No-op stub for a named tool — lets the gate's WAL+execute path succeed
/// without touching a real filesystem.
struct StubTool {
    name: &'static str,
}

#[async_trait]
impl Tool for StubTool {
    fn name(&self) -> &str {
        self.name
    }
    fn description(&self) -> &str {
        "stub tool"
    }
    fn input_schema(&self) -> serde_json::Value {
        json!({})
    }
    fn capability_requirements(&self) -> CapabilitySet {
        CapabilitySet::default()
    }
    async fn execute(
        &self,
        _input: serde_json::Value,
        _policy: &dyn PolicyEngine,
        _session: &SessionContext,
        _cancel: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput { summary: "ok".into(), data: json!({ "ok": true }) })
    }
}

/// Memory spine stub that counts retrievals/stores/invalidations and returns
/// one fixed chunk; it also records stored entries so e2e tests can assert
/// what actually reached the spine over the wire.
struct CountingMemoryStore {
    retrievals: AtomicUsize,
    stores: AtomicUsize,
    invalidations: AtomicUsize,
    stored: std::sync::Mutex<Vec<MemoryEntry>>,
}

impl CountingMemoryStore {
    fn new() -> Self {
        Self {
            retrievals: AtomicUsize::new(0),
            stores: AtomicUsize::new(0),
            invalidations: AtomicUsize::new(0),
            stored: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn retrieval_count(&self) -> usize {
        self.retrievals.load(Ordering::SeqCst)
    }

    fn store_count(&self) -> usize {
        self.stores.load(Ordering::SeqCst)
    }

    fn invalidation_count(&self) -> usize {
        self.invalidations.load(Ordering::SeqCst)
    }

    fn stored_entries(&self) -> Vec<MemoryEntry> {
        self.stored.lock().expect("stored lock").clone()
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
        Ok(vec![MemoryChunk {
            id: "chunk-1".to_owned(),
            project_id: ProjectId("proj-1".to_owned()),
            namespace: MemoryNamespace::Project(ProjectId("proj-1".to_owned())),
            content: "a remembered fact".to_owned(),
            file_path: None,
            start_line: None,
            end_line: None,
            chunk_type: ChunkType::Test,
            score: 0.9,
            model_id: "test-model".to_owned(),
            model_version: "0".to_owned(),
        }])
    }

    async fn store(
        &self,
        entry: MemoryEntry,
        _cancel: CancellationToken,
    ) -> Result<MemoryId, MemoryError> {
        self.stores.fetch_add(1, Ordering::SeqCst);
        self.stored.lock().expect("stored lock").push(entry);
        Ok(MemoryId(concerto_core::ids::Ulid::new()))
    }

    async fn invalidate(
        &self,
        _id: MemoryId,
        _cancel: CancellationToken,
    ) -> Result<(), MemoryError> {
        self.invalidations.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

/// A sqlite pool over the whiteboard schema in a temp dir.
async fn whiteboard_pool(max_connections: u32) -> (TempDir, sqlx::SqlitePool) {
    let dir = tempfile::tempdir().expect("tempdir created");
    let path = dir.path().join("supervisor_gate.db");
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

/// A policy engine that allows (or denies) everything.
fn engine(rules: Vec<PolicyRule>) -> Arc<SimplePolicyEngine> {
    Arc::new(SimplePolicyEngine::new(rules, Arc::new(TestAudit)))
}

/// A permissive gate over `gate_test` + `filesystem` stubs.
fn gate(policy: Arc<SimplePolicyEngine>, pool: sqlx::SqlitePool) -> Arc<WriteGate> {
    let mut registry = ToolRegistry::default();
    registry.register(Box::new(StubTool { name: "gate_test" }));
    registry.register(Box::new(StubTool { name: "filesystem" }));
    let executor = Arc::new(ToolExecutor::new(Arc::new(registry), policy.clone()));
    let root = PathBuf::from("/tmp");
    Arc::new(WriteGate::new(
        policy,
        executor,
        pool,
        Arc::new(FilePreImageReader::new(root.clone())),
        root,
        1,
    ))
}

/// Attach services (permissive gate by default) and run the loop for
/// `run_for`, returning the summary.
async fn run_supervisor(
    mut supervisor: Supervisor,
    services: SupervisorServices,
    run_for: Duration,
) -> concerto_orchestrator::supervisor::RunSummary {
    supervisor = supervisor.with_services(services);
    let shutdown = CancellationToken::new();
    let guard = shutdown.clone();
    let handle = tokio::spawn(async move { supervisor.run(shutdown).await });
    tokio::time::sleep(run_for).await;
    guard.cancel();
    handle.await.expect("loop task completes")
}

/// All whiteboard rows in gate_seq order.
async fn all_events(pool: &sqlx::SqlitePool) -> Vec<WhiteboardEvent> {
    load_whiteboard_events(pool, &WhiteboardLoadOpts::default()).await.expect("load events")
}

#[tokio::test]
async fn execute_tool_flows_through_gate_to_whiteboard_with_bound_attribution() {
    let (_dir, pool) = whiteboard_pool(2).await;
    let services = SupervisorServices {
        gate: gate(engine(vec![PolicyRule::AutoApprove(Condition::Always)]), pool.clone()),
        whiteboard_pool: pool.clone(),
        subscriptions: SubscriptionManager::new(pool.clone().clone()),
        consolidation: None,
        memory: Arc::new(CountingMemoryStore::new()),
        project_id: ProjectId("proj-1".to_owned()),
    };
    let mut supervisor = Supervisor::new(SupervisorConfig::default());
    supervisor
        .spawn_agent(&mut mock_agent_with("MOCK_AGENT_TOOL_REQUESTS", 2), "agent-a")
        .expect("spawn mock agent");

    let summary = run_supervisor(supervisor, services, Duration::from_secs(6)).await;
    assert!(summary.failed.is_empty(), "no agent may fail: {:?}", summary.failed);

    let events = all_events(&pool).await;
    assert_eq!(events.len(), 2, "two gated writes persist two WriteApplied rows");
    let mut event_ids: Vec<_> = events.iter().map(|event| event.event_id.as_str()).collect();
    event_ids.sort_unstable();
    assert_eq!(event_ids, ["mock-call-0", "mock-call-1"]);
    assert_eq!(events[0].gate_seq, 1);
    assert_eq!(events[1].gate_seq, 2);
    let mut agent_seqs: Vec<_> = events.iter().map(|event| event.agent_seq).collect();
    agent_seqs.sort_unstable();
    assert_eq!(agent_seqs, [1, 2]);
    for event in &events {
        assert_eq!(event.kind, WhiteboardKind::WriteApplied);
        // The mock spoofs `spoofed-agent` on the wire; the supervisor must
        // bind attribution to the registered process (ADR-60 D4).
        assert_eq!(event.agent_id, "agent-a", "wire agent_id must never be trusted");
    }
}

#[tokio::test]
async fn denied_execute_tool_persists_nothing_and_agent_stays_healthy() {
    let (_dir, pool) = whiteboard_pool(2).await;
    let services = SupervisorServices {
        gate: gate(engine(vec![PolicyRule::AutoDeny(Condition::Always)]), pool.clone()),
        whiteboard_pool: pool.clone(),
        subscriptions: SubscriptionManager::new(pool.clone().clone()),
        consolidation: None,
        memory: Arc::new(CountingMemoryStore::new()),
        project_id: ProjectId("proj-1".to_owned()),
    };
    let mut supervisor = Supervisor::new(SupervisorConfig::default());
    supervisor
        .spawn_agent(&mut mock_agent_with("MOCK_AGENT_TOOL_REQUESTS", 1), "agent-a")
        .expect("spawn mock agent");

    let summary = run_supervisor(supervisor, services, Duration::from_secs(6)).await;
    // A denied write is an ordinary reply, not an agent failure.
    assert!(summary.failed.is_empty(), "denial must not fail the agent: {:?}", summary.failed);

    // Denials are persisted as `WriteRejected` rows (auditability) — but the
    // tool never runs, so no `WriteApplied` row appears.
    let events = all_events(&pool).await;
    assert_eq!(events.len(), 1, "one denied write persists one WriteRejected row");
    assert_eq!(events[0].kind, WhiteboardKind::WriteRejected);
    assert_eq!(events[0].event_id, "mock-call-0");
    assert_eq!(events[0].gate_seq, 1);
    assert_eq!(events[0].agent_id, "agent-a", "wire agent_id must never be trusted");
    let reason = events[0].payload.get("reason").and_then(|v| v.as_str()).unwrap_or("");
    assert_eq!(
        reason, "Deny",
        "the policy verdict is recorded on the row: {:?}",
        events[0].payload
    );

    // The agent survived the denial and is still running at shutdown time.
    let agent = summary
        .agents
        .iter()
        .find(|meta| meta.agent_id == "agent-a")
        .expect("agent-a registered in the snapshot");
    assert_eq!(agent.state, AgentState::Running);
}

#[tokio::test]
async fn publish_event_assigns_sequencing_and_binds_agent() {
    let (_dir, pool) = whiteboard_pool(2).await;
    let services = SupervisorServices {
        gate: gate(engine(vec![PolicyRule::AutoApprove(Condition::Always)]), pool.clone()),
        whiteboard_pool: pool.clone(),
        subscriptions: SubscriptionManager::new(pool.clone().clone()),
        consolidation: None,
        memory: Arc::new(CountingMemoryStore::new()),
        project_id: ProjectId("proj-1".to_owned()),
    };
    let mut supervisor = Supervisor::new(SupervisorConfig::default());
    supervisor
        .spawn_agent(&mut mock_agent_with("MOCK_AGENT_PUBLISH", 2), "agent-a")
        .expect("spawn mock agent");

    let summary = run_supervisor(supervisor, services, Duration::from_secs(6)).await;
    assert!(summary.failed.is_empty(), "no agent may fail: {:?}", summary.failed);

    let events = all_events(&pool).await;
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].event_id, "mock-event-0");
    assert_eq!(events[1].event_id, "mock-event-1");
    assert_eq!(events[0].gate_seq, 1);
    assert_eq!(events[1].gate_seq, 2);
    assert_eq!(events[0].agent_seq, 1);
    assert_eq!(events[1].agent_seq, 2);
    for event in &events {
        assert_eq!(event.kind, WhiteboardKind::Finding);
        assert_eq!(event.agent_id, "agent-a", "wire agent_id must never be trusted");
        assert_eq!(event.scope, "mock");
    }
}

#[tokio::test]
async fn tool_and_publish_share_one_global_gate_seq_order() {
    let (_dir, pool) = whiteboard_pool(2).await;
    let services = SupervisorServices {
        gate: gate(engine(vec![PolicyRule::AutoApprove(Condition::Always)]), pool.clone()),
        whiteboard_pool: pool.clone(),
        subscriptions: SubscriptionManager::new(pool.clone().clone()),
        consolidation: None,
        memory: Arc::new(CountingMemoryStore::new()),
        project_id: ProjectId("proj-1".to_owned()),
    };
    let mut supervisor = Supervisor::new(SupervisorConfig::default());
    // The mock emits the tool request before the publish request. The
    // supervisor dispatches each request asynchronously, so completion order
    // is intentionally not coupled to pipe arrival order.
    supervisor
        .spawn_agent(
            mock_agent_with("MOCK_AGENT_TOOL_REQUESTS", 1).env("MOCK_AGENT_PUBLISH", "1"),
            "agent-a",
        )
        .expect("spawn mock agent");

    let summary = run_supervisor(supervisor, services, Duration::from_secs(6)).await;
    assert!(summary.failed.is_empty(), "no agent may fail: {:?}", summary.failed);

    let events = all_events(&pool).await;
    assert_eq!(events.len(), 2);
    let mut gate_seqs: Vec<_> = events.iter().map(|event| event.gate_seq).collect();
    gate_seqs.sort_unstable();
    assert_eq!(gate_seqs, [1, 2], "both writes share one global sequence");
    let mut agent_seqs: Vec<_> = events.iter().map(|event| event.agent_seq).collect();
    agent_seqs.sort_unstable();
    assert_eq!(agent_seqs, [1, 2], "agent sequence is independent of event kind");
    assert!(events.iter().any(|event| {
        event.kind == WhiteboardKind::WriteApplied && event.event_id == "mock-call-0"
    }));
    assert!(events.iter().any(|event| {
        event.kind == WhiteboardKind::Finding && event.event_id == "mock-event-0"
    }));
}

#[tokio::test]
async fn retrieve_memory_queries_the_spine_once_per_request() {
    let (_dir, pool) = whiteboard_pool(2).await;
    let memory = Arc::new(CountingMemoryStore::new());
    let services = SupervisorServices {
        gate: gate(engine(vec![PolicyRule::AutoApprove(Condition::Always)]), pool.clone()),
        whiteboard_pool: pool.clone(),
        subscriptions: SubscriptionManager::new(pool.clone().clone()),
        consolidation: None,
        memory: memory.clone(),
        project_id: ProjectId("proj-1".to_owned()),
    };
    let mut supervisor = Supervisor::new(SupervisorConfig::default());
    supervisor
        .spawn_agent(&mut mock_agent_with("MOCK_AGENT_RETRIEVE", 1), "agent-a")
        .expect("spawn mock agent");

    let summary = run_supervisor(supervisor, services, Duration::from_secs(6)).await;
    assert!(summary.failed.is_empty(), "no agent may fail: {:?}", summary.failed);
    assert_eq!(memory.retrieval_count(), 1, "one retrieve-memory request, one spine query");
    assert!(all_events(&pool).await.is_empty(), "retrieval is read-only: no whiteboard rows");
}

#[tokio::test]
async fn store_and_invalidate_memory_reach_the_spine() {
    let (_dir, pool) = whiteboard_pool(2).await;
    let memory = Arc::new(CountingMemoryStore::new());
    let services = SupervisorServices {
        gate: gate(engine(vec![PolicyRule::AutoApprove(Condition::Always)]), pool.clone()),
        whiteboard_pool: pool.clone(),
        subscriptions: SubscriptionManager::new(pool.clone().clone()),
        consolidation: None,
        memory: memory.clone(),
        project_id: ProjectId("proj-1".to_owned()),
    };
    let mut supervisor = Supervisor::new(SupervisorConfig::default());
    supervisor
        .spawn_agent(
            mock_agent_with("MOCK_AGENT_STORE", 1).env("MOCK_AGENT_INVALIDATE", "1"),
            "agent-a",
        )
        .expect("spawn mock agent");

    let summary = run_supervisor(supervisor, services, Duration::from_secs(6)).await;
    assert!(summary.failed.is_empty(), "no agent may fail: {:?}", summary.failed);
    assert_eq!(memory.store_count(), 1, "one store-memory request, one spine store");
    assert_eq!(
        memory.invalidation_count(),
        1,
        "one invalidate-memory request, one spine invalidate"
    );

    // The wire entry is a content-only projection: the spine records the
    // fixture's payload bound to the supervisor's project (ADR-60 D6).
    let stored = memory.stored_entries();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].content, "mock-store-0");
    assert_eq!(stored[0].metadata["seq"], 0);
    assert_eq!(stored[0].project_id, ProjectId("proj-1".to_owned()));
    assert!(all_events(&pool).await.is_empty(), "memory writes append no whiteboard rows");
}

#[tokio::test]
async fn clean_exit_marks_agent_completed_and_is_never_restarted() {
    let (_dir, pool) = whiteboard_pool(2).await;
    let services = SupervisorServices {
        gate: gate(engine(vec![PolicyRule::AutoApprove(Condition::Always)]), pool.clone()),
        whiteboard_pool: pool.clone(),
        subscriptions: SubscriptionManager::new(pool.clone()),
        consolidation: None,
        memory: Arc::new(CountingMemoryStore::new()),
        project_id: ProjectId("proj-1".to_owned()),
    };
    let mut supervisor = Supervisor::new(SupervisorConfig::default());
    // The mock exits 0 right after the handshake — the ADR-60 S5 terminal
    // "task completed" exit for a one-run-per-process agent.
    let mut command = mock_agent();
    command.env("MOCK_AGENT_EXIT_AFTER", "1");
    supervisor.spawn_agent(&mut command, "agent-a").expect("spawn mock agent");

    let summary = run_supervisor(supervisor, services, Duration::from_secs(6)).await;
    assert!(summary.failed.is_empty(), "a clean exit is not a failure: {:?}", summary.failed);
    let agent = summary
        .agents
        .iter()
        .find(|meta| meta.agent_id == "agent-a")
        .expect("agent-a registered in the snapshot");
    assert_eq!(agent.state, AgentState::Completed, "clean exit is terminal");
    assert_eq!(agent.restart_count, 0, "no restart after a clean exit");
}

#[tokio::test]
async fn nonzero_exit_is_a_crash_and_consumes_the_restart_budget() {
    let (_dir, pool) = whiteboard_pool(2).await;
    let services = SupervisorServices {
        gate: gate(engine(vec![PolicyRule::AutoApprove(Condition::Always)]), pool.clone()),
        whiteboard_pool: pool.clone(),
        subscriptions: SubscriptionManager::new(pool.clone()),
        consolidation: None,
        memory: Arc::new(CountingMemoryStore::new()),
        project_id: ProjectId("proj-1".to_owned()),
    };
    let mut supervisor =
        Supervisor::new(SupervisorConfig { max_restarts: 0, ..SupervisorConfig::default() });
    let mut command = mock_agent();
    command.env("MOCK_AGENT_EXIT_AFTER", "1").env("MOCK_AGENT_EXIT_STATUS", "1");
    supervisor.spawn_agent(&mut command, "agent-a").expect("spawn mock agent");

    let summary = run_supervisor(supervisor, services, Duration::from_secs(6)).await;
    assert_eq!(summary.failed, vec!["agent-a"], "a crashing agent exhausts its budget");
    let agent = summary
        .agents
        .iter()
        .find(|meta| meta.agent_id == "agent-a")
        .expect("agent-a registered in the snapshot");
    assert_eq!(agent.state, AgentState::Failed);
}
