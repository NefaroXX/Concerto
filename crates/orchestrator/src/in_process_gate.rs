//! In-process write-gate backend — ADR-60 D5 parity for the single-agent
//! loop.
//!
//! The supervised child runs every tool call through the supervisor's single
//! [`WriteGate`] via [`crate::gate_proxy::GateProxyBackend`]: WAL-before-
//! execute, whiteboard pre-image attribution, and always-on `base_version`
//! conflict detection (a declared claim on a target whose current hash
//! differs is refused loudly, never silently lost to last-writer-wins).
//!
//! In single-process mode (CLI/Desktop) the loop ran tools through a plain
//! [`ToolExecutor`] with none of that — a concurrent loop writing the same
//! file simply clobbered it. [`InProcessGateBackend`] closes that gap
//! in-process: it wraps an [`Arc<WriteGate>`] and dispatches through
//! [`WriteGate::submit`] with the same always-on stamp
//! ([`stamp_base_versions`]) the supervisor applies, so the two paths share
//! one error surface and one whiteboard.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use concerto_core::error::ToolError;
use concerto_core::executor::ToolExecutor;
use concerto_core::ids::Ulid;
use concerto_core::types::{SessionContext, ToolDefinition, ToolOutput};
use concerto_core::CancellationToken;

use crate::exec_backend::ToolExecutionBackend;
use crate::gate::{stamp_base_versions, GateError, GateRequest, WriteGate};
use crate::gate_proxy::gate_rejection_message;
use crate::ipc::IpcError;

/// The in-process twin of [`crate::gate_proxy::GateProxyBackend`]: every loop
/// tool call is a gated write through the shared [`WriteGate`], with identical
/// dispatch semantics (always-on `base_versions` stamp, WAL-before-execute,
/// per-agent attribution).
pub struct InProcessGateBackend {
    /// The shared write gate; owns the policy/executor pair it executes
    /// under (one enforcement point, same as the supervisor).
    gate: Arc<WriteGate>,
    /// The wrapped executor, retained for the `record_ack_decision` audit
    /// passthrough (the gate itself has no ack channel).
    executor: Arc<ToolExecutor>,
    /// Whiteboard attribution + per-agent limiter key for this loop. Matches
    /// the loop's established `"single-agent"` identity (`EventKind::
    /// AgentThought`).
    agent_id: String,
    /// Write scope the loop's tool calls are gated under; `"fs"` today,
    /// identical to the supervised backend.
    scope: String,
}

impl InProcessGateBackend {
    /// The write scope this agent's tool calls are gated under; `"fs"` today.
    const SCOPE: &'static str = "fs";

    /// Wrap a gate (built over the *shared* policy/executor pair) as the
    /// loop's execution backend.
    ///
    /// `agent_id` is the whiteboard attribution and per-agent limiter key; the
    /// in-process loop uses its established `"single-agent"` id.
    pub fn new(
        gate: Arc<WriteGate>,
        executor: Arc<ToolExecutor>,
        agent_id: impl Into<String>,
    ) -> Self {
        Self { gate, executor, agent_id: agent_id.into(), scope: Self::SCOPE.to_owned() }
    }
}

#[async_trait]
impl ToolExecutionBackend for InProcessGateBackend {
    fn tool_definitions(&self) -> Vec<ToolDefinition> {
        self.gate.tool_definitions()
    }

    async fn execute(
        &self,
        tool_name: &str,
        input: serde_json::Value,
        call_id: &str,
        session: &SessionContext,
        cancel: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let mut input = input;
        // ADR-60 D5: `base_versions` is a gate-level concurrency claim map,
        // not tool input — lift it out of the payload and forward the rest
        // (identical to the supervised backend's lift).
        let mut base_versions = BTreeMap::new();
        if let Some(map) = input.as_object_mut() {
            if let Some(serde_json::Value::Object(claims)) = map.remove("base_versions") {
                for (target, claim) in claims {
                    if let serde_json::Value::String(hash) = claim {
                        base_versions.insert(target, hash);
                    }
                }
            }
        }

        let mut request = GateRequest {
            call_id: call_id.to_owned(),
            agent_id: self.agent_id.clone(),
            tool: tool_name.to_owned(),
            input,
            session_id: Some(session.session_id.to_string()),
            scope: self.scope.clone(),
            plan_id: None,
            causation: None,
            base_versions,
        };
        // ADR-60 D5 always-on: stamp each mutated target's current pre-image
        // hash before submission — the same injection the supervisor applies
        // in `handle_execute_tool` — so the in-process loop's base_version
        // claims are attested from the same reader the gate conflict-checks
        // against. The gate refuses a sibling-altared target loudly.
        stamp_base_versions(&self.gate, &mut request).await;
        let outcome = match self.gate.submit(request, cancel.clone()).await {
            Ok(outcome) => outcome,
            Err(error) => {
                // ADR-60 D5: a base_version collision is the loud signal that
                // agents raced on shared files — record it at warn so the
                // operator has a manual-resolution trail (mirrors the
                // supervisor's `handle_execute_tool`).
                if let GateError::Conflict { event_id, reason } = &error {
                    tracing::warn!(
                        agent_id = %self.agent_id,
                        %event_id,
                        %reason,
                        "in-process: optimistic write conflict"
                    );
                }
                return Err(gate_error_to_tool_error(error));
            }
        };
        // Mirror the supervised child's post-round-trip cancellation check:
        // a write that already applied while the run was cancelled surfaces
        // as a cancellation, not a success.
        if cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        serde_json::from_value::<ToolOutput>(outcome.result).map_err(|error| {
            ToolError::ExecutionFailed {
                message: format!("gate outcome did not carry a ToolOutput: {error}"),
            }
        })
    }

    async fn record_ack_decision(
        &self,
        session_id: Ulid,
        correlation_id: Ulid,
        message: &str,
        acknowledged: bool,
        cancel: CancellationToken,
    ) {
        // In-process the loop keeps the plain-executor ack behavior (audit
        // write through the shared policy engine); the supervised path skips
        // it because the supervisor owns the audit (ADR-60 D4/D5).
        self.executor
            .record_ack_decision(session_id, correlation_id, message, acknowledged, cancel)
            .await;
    }
}

/// Map a gate failure onto the loop's tool error taxonomy, byte-identical to
/// what the supervised child receives over the wire: the supervisor renders
/// the error via [`IpcError::from_gate`], and the child surfaces the flattened
/// `(code, message)` through [`gate_rejection_message`].
fn gate_error_to_tool_error(error: GateError) -> ToolError {
    match error {
        // The gate returns `Cancelled` only under the shared token, which the
        // backend's post-submit check would also observe; surface it as a
        // cancellation (the loop must break, not retry).
        GateError::Cancelled => ToolError::Cancelled,
        other => {
            let ipc = IpcError::from_gate(&other);
            ToolError::ExecutionFailed { message: gate_rejection_message(ipc.code, &ipc.message) }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use concerto_core::error::PolicyError;
    use concerto_core::policy::SimplePolicyEngine;
    use concerto_core::traits::policy::AuditLog;
    use concerto_core::types::{Condition, PolicyRule, ToolRegistry};
    use concerto_tools::filesystem::FilesystemTool;
    use serde_json::json;
    use sqlx::pool::PoolOptions;
    use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqliteSynchronous};
    use std::time::Duration;
    use tempfile::TempDir;

    /// No-op audit log for tests.
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

    /// File-backed pool with the same PRAGMAs as production (WAL,
    /// busy_timeout, synchronous=NORMAL) and all sessions migrations applied.
    async fn test_pool() -> (TempDir, sqlx::SqlitePool) {
        let dir = tempfile::tempdir().expect("tempdir created");
        let path = dir.path().join("in_process_gate.db");
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

    /// An allow-all gate + its executor over the REAL `FilesystemTool` rooted
    /// at `root` — the shape the in-process runtime builds in production. The
    /// executor handle is returned too, so the backend's `record_ack_decision`
    /// passthrough (and `Arc<WriteGate>` construction) use the same instance.
    fn fs_gate(
        root: camino::Utf8PathBuf,
        pool: sqlx::SqlitePool,
    ) -> (Arc<WriteGate>, Arc<ToolExecutor>) {
        let mut registry = ToolRegistry::default();
        registry.register(Box::new(FilesystemTool::new(root.clone())));
        let allow_all = vec![PolicyRule::AutoApprove(Condition::Always)];
        let policy: Arc<dyn concerto_core::traits::policy::PolicyEngine> =
            Arc::new(SimplePolicyEngine::new(allow_all, Arc::new(TestAudit)));
        let executor = Arc::new(ToolExecutor::new(Arc::new(registry), policy.clone()));
        let gate = Arc::new(WriteGate::new(
            policy,
            executor.clone(),
            pool,
            Arc::new(crate::gate::FilePreImageReader::new(root.as_std_path().to_path_buf())),
            root.as_std_path().to_path_buf(),
            1,
        ));
        (gate, executor)
    }

    /// The gate error surface an in-process agent sees must equal what the
    /// supervised child builds after the wire round-trip.
    #[test]
    fn gate_error_maps_identically_to_the_supervised_child_surface() {
        let cases = [
            GateError::Denied { event_id: "call-1".into(), reason: "denial".into() },
            GateError::Conflict {
                event_id: "call-1".into(),
                reason: "base_version mismatch on a.txt".into(),
            },
            GateError::Policy("eval failed".into()),
            GateError::Whiteboard("db full".into()),
            GateError::Execution("boom".into()),
            GateError::PreImage("read failed".into()),
            GateError::InvalidRequest("bad call_id".into()),
        ];
        for error in cases {
            let supervised = {
                let ipc = IpcError::from_gate(&error);
                crate::gate_proxy::gate_proxy_to_tool_error(
                    crate::gate_proxy::GateProxyError::Supervisor {
                        code: ipc.code,
                        message: ipc.message,
                    },
                )
            };
            assert_eq!(
                gate_error_to_tool_error(error).to_string(),
                supervised.to_string(),
                "in-process and supervised surfaces must not drift"
            );
        }
        assert_eq!(
            gate_error_to_tool_error(GateError::Cancelled).to_string(),
            ToolError::Cancelled.to_string(),
            "a gate cancellation must surface as a tool cancellation"
        );
    }

    /// The backend dispatches through the gate: the WAL row lands BEFORE the
    /// tool runs, the pre-image is attributed, and the outcome round-trips as
    /// a full `ToolOutput`.
    #[tokio::test]
    async fn execute_goes_through_the_gate_and_round_trips_the_output() {
        let root = tempfile::tempdir().expect("tempdir");
        let utf8_root =
            camino::Utf8PathBuf::from_path_buf(root.path().to_path_buf()).expect("utf-8 tempdir");
        let (_dir, pool) = test_pool().await;
        let (gate, executor) = fs_gate(utf8_root.clone(), pool.clone());
        let backend = InProcessGateBackend::new(gate, executor, "single-agent");

        // Seed the target so the gated write is a MODIFICATION with a real
        // prior version for the pre-image to attribute.
        std::fs::write(root.path().join("hello.txt"), b"seed").expect("seed the target");
        let session = SessionContext::new(Ulid::new(), root.path().to_path_buf());
        let output = backend
            .execute(
                "filesystem",
                json!({ "operation": "write", "path": "hello.txt", "content": "hi" }),
                "call-1",
                &session,
                CancellationToken::new(),
            )
            .await
            .expect("gated write applies");

        assert_eq!(
            std::fs::read_to_string(root.path().join("hello.txt")).expect("file on disk"),
            "hi",
            "the gate executed the tool"
        );
        assert!(!output.summary.is_empty(), "summary flows back to the loop");

        let events = concerto_sessions::whiteboard::load_whiteboard_events(
            &pool,
            &concerto_sessions::whiteboard::WhiteboardLoadOpts::default(),
        )
        .await
        .expect("load events");
        let applied =
            events.iter().find(|event| event.event_id == "call-1").expect("write-applied row");
        assert_eq!(
            applied.kind,
            concerto_sessions::whiteboard::WhiteboardKind::WriteApplied,
            "WAL-before-execute persisted the applied event"
        );
        assert_eq!(applied.agent_id, "single-agent", "attribution uses the loop's agent id");
        assert_eq!(
            applied.payload["pre_images"]["hello.txt"],
            json!(blake3_hex(b"seed")),
            "the applied row attributes the seeded content as the pre-image"
        );
    }

    fn blake3_hex(bytes: &[u8]) -> String {
        blake3::hash(bytes).to_hex().to_string()
    }
}
