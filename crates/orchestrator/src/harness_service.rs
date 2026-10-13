//! H13 — the ONE harness service seam: `environment.describe`,
//! `checkpoint.create` / `checkpoint.inspect` / `checkpoint.restore`,
//! `verify.run`, `memory.recall` / `memory.inspect`.
//!
//! # Declared release coverage (H13 §0)
//!
//! The plan permits implementing only a declared adapter coverage as long as
//! the declaration is explicit and anything outside it reads as out-of-scope.
//! This release declares:
//!
//! | Adapter path | Status | Where proven |
//! | --- | --- | --- |
//! | In-process single-agent | covered | `harness_service::tests`, `vertical_slice` |
//! | In-process coordinator | covered | `harness_service::tests` |
//! | Supervised agents | covered (memory recall) | `ipc` wire + `supervisor` status fields |
//! | CLI JSON | covered | `concerto-cli` `harness` subcommand tests |
//! | Desktop | covered | `concerto-desktop` harness view tests |
//! | External API / MCP facade | **out of scope** | see [`EXTERNAL_ACCESS_NON_COVERAGE`] |
//!
//! Every covered adapter calls **this service** (not the tools, not the
//! stores). The service executes every operation through
//! [`ToolExecutor`][concerto_core::executor::ToolExecutor] (or, for restore,
//! through the executor's dedicated `restore_checkpoint` pipeline), so the
//! normal policy + audit trail is identical on every path.
//!
//! # External access — deliberate non-coverage
//!
//! "External agents should receive narrow, run/project-scoped API access."
//! The honest outcome for this release is that **no external path meets the
//! bar**, and shipping a thin unauthenticated endpoint to claim coverage was
//! rejected. See [`EXTERNAL_ACCESS_NON_COVERAGE`] for the full reasoning. The
//! plan's MCP facade is a separate ADR-43 deferral and is **not built here**.

use std::sync::{Arc, Mutex};

use concerto_core::executor::ToolExecutor;
use concerto_core::memory::ProjectId;
use concerto_core::traits::approval::{ApprovalDecision, ApprovalSink};
use concerto_core::traits::memory::MemoryStore;
use concerto_core::types::{SessionContext, ToolOutput, ToolRegistry};
use concerto_core::{CancellationToken, ToolError};
use concerto_sessions::file_checkpoints::FileCheckpointStore;
use concerto_tools::undo::UndoManager;
use serde::{Deserialize, Serialize};

use crate::environment_describe::ENVIRONMENT_DESCRIBE_TOOL_NAME;
use crate::gate::WriteGate;
use crate::harness_checkpoint::{
    CheckpointCreateTool, CheckpointInspectTool, CHECKPOINT_CREATE_TOOL_NAME,
    CHECKPOINT_INSPECT_TOOL_NAME, CHECKPOINT_RESTORE_TOOL_NAME,
};
use crate::harness_memory::{
    MemoryInspectTool, MemoryRecallTool, MemoryStoreResolver, MEMORY_INSPECT_TOOL_NAME,
    MEMORY_RECALL_TOOL_NAME,
};
use crate::verify::VERIFY_RUN_TOOL_NAME;

/// Why no external adapter is covered in this release.
pub const EXTERNAL_ACCESS_NON_COVERAGE: &str =
    "No external path is covered in this release. The api-server's `AppState` is session/task/\
     spend-oriented: it holds no `ToolExecutor` and no run-scoped executor, so routing a harness \
     operation through it would either (a) build a second, ungoverned executor (a direct violation \
     of the binding constraint that every operation keeps its normal policy/audit path), or (b) \
     accept a project root from the caller with no run binding, which is exactly the wide, \
     unscoped access the plan forbids. The server's existing API-key auth authenticates a client, \
     not a run, so it cannot scope the operation to the caller's own run/project. Building a \
     correct external surface needs its own run-scoped transport/authentication/lifecycle decision \
     (the same class of decision the plan defers for the MCP facade under ADR-43) and is therefore \
     declared out of scope here rather than shipped thin.";

/// The service's identity for one operation — the single source of the wire
/// payload every adapter consumes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessInvocation {
    /// The operation name (e.g. `environment.describe`).
    pub operation: String,
    /// The tool's bounded human-facing summary.
    pub summary: String,
    /// The operation payload — the tool's own `data` object, unmodified. This
    /// is the service shape; the CLI/desktop serialize exactly this value and
    /// never re-serialize the underlying tool output.
    pub data: serde_json::Value,
}

impl HarnessInvocation {
    /// Build an invocation from a completed [`ToolOutput`].
    pub fn from_output(operation: &str, output: ToolOutput) -> Self {
        Self { operation: operation.to_string(), summary: output.summary, data: output.data }
    }

    /// Whether the operation reports a degraded capability (recall ran
    /// vector-disabled, etc.) — read straight from the tool payload so no
    /// adapter invents its own reading.
    pub fn is_degraded(&self) -> bool {
        self.data.get("degraded").and_then(serde_json::Value::as_bool).unwrap_or(false)
    }

    /// Whether the operation reports partial success (`checkpoint.inspect` of
    /// a `Partial` record, a partial restore).
    pub fn is_partial(&self) -> bool {
        self.data.get("status").and_then(serde_json::Value::as_str) == Some("partial")
            || self
                .data
                .get("record")
                .and_then(|record| record.get("status"))
                .and_then(|status| status.get("partial"))
                .is_some()
    }
}

/// Why the harness service could not produce an invocation.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum HarnessServiceError {
    /// The operation named no known harness tool.
    #[error("unknown harness operation: {0}")]
    UnknownOperation(String),
    /// The checkpoint state (store/gate/stash) was not wired into this
    /// service, so `checkpoint.restore` cannot run.
    #[error(
        "checkpoint restore is unavailable: no checkpoint store/gate is wired to this service"
    )]
    CheckpointUnavailable,
    /// The executor refused or failed the operation (policy denial, missing
    /// tool, execution failure).
    #[error("harness operation failed: {0}")]
    Execution(#[from] ToolError),
}

/// The printer/audit identity used for a restore performed through the service.
const DEFAULT_RESTORE_AGENT: &str = "harness";

/// Checkpoint state a service needs to run `checkpoint.restore`.
pub struct CheckpointAccess {
    store: Arc<FileCheckpointStore>,
    gate: Arc<WriteGate>,
    stash: Option<Arc<Mutex<UndoManager>>>,
}

impl CheckpointAccess {
    /// Wire a checkpoint store + gate (+ optional git-stash undo manager).
    pub fn new(
        store: Arc<FileCheckpointStore>,
        gate: Arc<WriteGate>,
        stash: Option<Arc<Mutex<UndoManager>>>,
    ) -> Self {
        Self { store, gate, stash }
    }
}

/// The harness service: the single seam every adapter calls.
///
/// Construct once per run/frontend invocation from the run's
/// [`ToolExecutor`] and [`SessionContext`]; the registry behind the executor
/// carries the harness tools (see [`register_harness_tools`]). All requests are
/// bounded and cancellation-aware.
pub struct HarnessService {
    executor: Arc<ToolExecutor>,
    session: SessionContext,
    checkpoint: Option<CheckpointAccess>,
}

impl HarnessService {
    /// Build a service over a run's executor + session.
    pub fn new(executor: Arc<ToolExecutor>, session: SessionContext) -> Self {
        Self { executor, session, checkpoint: None }
    }

    /// Attach checkpoint state so `checkpoint.restore` can run.
    #[must_use]
    pub fn with_checkpoint(mut self, access: CheckpointAccess) -> Self {
        self.checkpoint = Some(access);
        self
    }

    /// The session this service is scoped to.
    pub fn session(&self) -> &SessionContext {
        &self.session
    }

    /// `environment.describe`.
    pub async fn environment_describe(
        &self,
        input: serde_json::Value,
        cancel: CancellationToken,
    ) -> Result<HarnessInvocation, HarnessServiceError> {
        self.invoke_executor(ENVIRONMENT_DESCRIBE_TOOL_NAME, input, cancel).await
    }

    /// `checkpoint.create`.
    pub async fn checkpoint_create(
        &self,
        input: serde_json::Value,
        cancel: CancellationToken,
    ) -> Result<HarnessInvocation, HarnessServiceError> {
        self.invoke_executor(CHECKPOINT_CREATE_TOOL_NAME, input, cancel).await
    }

    /// `checkpoint.inspect`.
    pub async fn checkpoint_inspect(
        &self,
        input: serde_json::Value,
        cancel: CancellationToken,
    ) -> Result<HarnessInvocation, HarnessServiceError> {
        self.invoke_executor(CHECKPOINT_INSPECT_TOOL_NAME, input, cancel).await
    }

    /// `verify.run`.
    pub async fn verify_run(
        &self,
        input: serde_json::Value,
        cancel: CancellationToken,
    ) -> Result<HarnessInvocation, HarnessServiceError> {
        self.invoke_executor(VERIFY_RUN_TOOL_NAME, input, cancel).await
    }

    /// `memory.recall`.
    pub async fn memory_recall(
        &self,
        input: serde_json::Value,
        cancel: CancellationToken,
    ) -> Result<HarnessInvocation, HarnessServiceError> {
        self.invoke_executor(MEMORY_RECALL_TOOL_NAME, input, cancel).await
    }

    /// `memory.inspect`.
    pub async fn memory_inspect(
        &self,
        input: serde_json::Value,
        cancel: CancellationToken,
    ) -> Result<HarnessInvocation, HarnessServiceError> {
        self.invoke_executor(MEMORY_INSPECT_TOOL_NAME, input, cancel).await
    }

    /// `checkpoint.restore`.
    ///
    /// Restore is service-level (not a nested registered tool) because it must
    /// itself issue gated `filesystem` writes; it runs through
    /// [`ToolExecutor::restore_checkpoint`], the executor's dedicated restore
    /// pipeline (per-resource policy + audit). See
    /// [`crate::harness_checkpoint`] for the rationale.
    pub async fn checkpoint_restore(
        &self,
        input: serde_json::Value,
        cancel: CancellationToken,
    ) -> Result<HarnessInvocation, HarnessServiceError> {
        let Some(access) = self.checkpoint.as_ref() else {
            return Err(HarnessServiceError::CheckpointUnavailable);
        };
        let parsed: crate::harness_checkpoint::CheckpointRestoreInput =
            serde_json::from_value(input).map_err(|error| {
                HarnessServiceError::Execution(ToolError::ExecutionFailed {
                    message: format!("invalid checkpoint.restore input: {error}"),
                })
            })?;
        let by_agent = parsed.by_agent.as_deref().unwrap_or(DEFAULT_RESTORE_AGENT);
        let outcome = crate::file_checkpoint::restore_file_checkpoint(
            &self.executor,
            &access.store,
            &access.gate,
            access.stash.as_ref(),
            &self.session,
            &parsed.checkpoint_id,
            by_agent,
            &cancel,
        )
        .await
        .map_err(|error| {
            HarnessServiceError::Execution(match error {
                concerto_core::checkpoint::RestoreError::Cancelled => ToolError::Cancelled,
                other => ToolError::ExecutionFailed { message: other.to_string() },
            })
        })?;
        let projected: crate::harness_checkpoint::HarnessRestoreOutcome = outcome.into();
        let summary = format!(
            "checkpoint.restore id={} status={}",
            parsed.checkpoint_id,
            projected.summary()
        );
        let data = serde_json::to_value(&projected).unwrap_or_else(|error| {
            tracing::error!(%error, "failed to serialize restore outcome");
            serde_json::json!({ "status": "serialization_error" })
        });
        Ok(HarnessInvocation { operation: CHECKPOINT_RESTORE_TOOL_NAME.to_string(), summary, data })
    }

    /// Invoke any operation by its service name.
    ///
    /// This is the uniform dispatch every adapter uses; the operation names
    /// are the constants in this module and its sibling operation modules.
    pub async fn invoke(
        &self,
        operation: &str,
        input: serde_json::Value,
        cancel: CancellationToken,
    ) -> Result<HarnessInvocation, HarnessServiceError> {
        match operation {
            ENVIRONMENT_DESCRIBE_TOOL_NAME => self.environment_describe(input, cancel).await,
            CHECKPOINT_CREATE_TOOL_NAME => self.checkpoint_create(input, cancel).await,
            CHECKPOINT_INSPECT_TOOL_NAME => self.checkpoint_inspect(input, cancel).await,
            CHECKPOINT_RESTORE_TOOL_NAME => self.checkpoint_restore(input, cancel).await,
            VERIFY_RUN_TOOL_NAME => self.verify_run(input, cancel).await,
            MEMORY_RECALL_TOOL_NAME => self.memory_recall(input, cancel).await,
            MEMORY_INSPECT_TOOL_NAME => self.memory_inspect(input, cancel).await,
            other => Err(HarnessServiceError::UnknownOperation(other.to_string())),
        }
    }

    async fn invoke_executor(
        &self,
        tool_name: &str,
        input: serde_json::Value,
        cancel: CancellationToken,
    ) -> Result<HarnessInvocation, HarnessServiceError> {
        let output = self.executor.execute(tool_name, input, &self.session, cancel).await?;
        Ok(HarnessInvocation::from_output(tool_name, output))
    }
}

/// Register the H13 operation tools the runtime/frontend can supply into the
/// run's shared registry.
///
/// `environment.describe` and `verify.run` are registered by their own
/// helpers (`register_describe_tool` / `register_verify_tool`) because they
/// carry run-specific caches; this helper registers the checkpoint and memory
/// operations. `checkpoint.restore` is service-level and is deliberately not
/// registered (see the module docs).
pub struct HarnessToolRegistration<'a> {
    /// Project root the checkpoint store is anchored to.
    pub project_root: &'a std::path::Path,
    /// Checkpoint sidecar, when the run has a sessions database pool.
    pub checkpoint_store: Option<Arc<FileCheckpointStore>>,
    /// Project-scoped memory store for `memory.recall`.
    pub memory_store: Option<Arc<dyn MemoryStore>>,
    /// Lazily-resolved memory store for `memory.recall` (runtime registration,
    /// where the store is initialised after the registry is frozen).
    pub memory_resolver: Option<Arc<dyn MemoryStoreResolver>>,
    /// Memory database path for `memory.inspect`.
    pub memory_db_path: Option<std::path::PathBuf>,
    /// Project id for `memory.recall` / `memory.inspect`.
    pub project_id: ProjectId,
}

/// Register checkpoint + memory operations into `registry`.
///
/// Call after plugin/MCP registration and before `environment.describe`
/// registration (so the describe tool-names snapshot covers them). Missing
/// dependencies simply skip their operations (a run without a sessions DB has
/// no checkpoint store; memory disabled has no store), never a broken tool.
pub fn register_harness_tools(registry: &mut ToolRegistry, config: HarnessToolRegistration<'_>) {
    if let Some(store) = config.checkpoint_store {
        registry.register(Box::new(CheckpointCreateTool::new(
            store.clone(),
            config.project_root.to_path_buf(),
        )));
        registry.register(Box::new(CheckpointInspectTool::new(store)));
    }
    if let Some(resolver) = config.memory_resolver {
        registry.register(Box::new(MemoryRecallTool::lazy(resolver, config.project_id.clone())));
    } else if let Some(store) = config.memory_store {
        registry.register(Box::new(MemoryRecallTool::new(store, config.project_id.clone())));
    }
    if let Some(db_path) = config.memory_db_path {
        registry.register(Box::new(MemoryInspectTool::new(db_path, config.project_id)));
    }
}

/// A fail-closed approval sink for non-interactive frontends (CLI one-shot
/// subcommands, tests). Any operation whose policy verdict requires approval
/// is denied rather than silently auto-approved.
pub struct DenyApprovalSink;

#[async_trait::async_trait]
impl ApprovalSink for DenyApprovalSink {
    async fn request_approval(
        &self,
        _action: &concerto_core::types::PolicyAction<'_>,
        _cancel: CancellationToken,
    ) -> ApprovalDecision {
        ApprovalDecision::Deny
    }

    async fn approve_all_for_session(
        &self,
        _session_id: concerto_core::ids::Ulid,
        _cancel: CancellationToken,
    ) {
    }

    async fn request_ack(
        &self,
        _session_id: concerto_core::ids::Ulid,
        _message: &str,
        _cancel: CancellationToken,
    ) -> bool {
        false
    }
}

/// An approval sink for a one-shot operator invocation (a typed
/// `concerto harness ...` command, or a desktop control press).
///
/// The operator issuing the request *is* the approval for that single
/// operation; this sink approves within the process that was explicitly
/// started for it. It never widens a running agent's authority: the
/// in-process/run path continues to use the run's configured sink.
pub struct ApproveInvocationSink;

#[async_trait::async_trait]
impl ApprovalSink for ApproveInvocationSink {
    async fn request_approval(
        &self,
        _action: &concerto_core::types::PolicyAction<'_>,
        _cancel: CancellationToken,
    ) -> ApprovalDecision {
        ApprovalDecision::Approve
    }

    async fn approve_all_for_session(
        &self,
        _session_id: concerto_core::ids::Ulid,
        _cancel: CancellationToken,
    ) {
    }

    async fn request_ack(
        &self,
        _session_id: concerto_core::ids::Ulid,
        _message: &str,
        _cancel: CancellationToken,
    ) -> bool {
        true
    }
}

/// A standalone harness service plus the state that must outlive it.
///
/// Frontends that are not inside an agent run (the CLI's `harness` subcommand,
/// the desktop's harness panel) build this once, invoke operations on
/// [`Self::service`], and drop it. The held handles keep the write gate and any
/// memory background tasks alive for the service's lifetime.
pub struct StandaloneHarness {
    /// The service every adapter calls.
    pub service: HarnessService,
    checkpoint: Option<CheckpointAccess>,
    memory_cancel: Option<CancellationToken>,
    gate: Option<Arc<WriteGate>>,
}

impl StandaloneHarness {
    /// The configured checkpoint access, for callers that build a second
    /// service over the same checkpoint state.
    pub fn checkpoint_access(&self) -> Option<&CheckpointAccess> {
        self.checkpoint.as_ref()
    }

    /// The write gate, when one was built.
    pub fn gate(&self) -> Option<&Arc<WriteGate>> {
        self.gate.as_ref()
    }
}

impl Drop for StandaloneHarness {
    fn drop(&mut self) {
        // Stop the memory watcher/indexer background tasks deterministically.
        if let Some(cancel) = self.memory_cancel.take() {
            cancel.cancel();
        }
    }
}

/// Build a standalone harness service for a frontend outside a full agent run.
///
/// Reuses the runtime's own tool-registry, audit-sink, and policy/executor
/// construction (`build_tool_registry` / `build_audit_sink` /
/// `setup_policy_and_audit`) so a CLI/desktop request travels the identical
/// policy engine and audit log an in-process agent would use — the "same
/// effective contract" the plan requires. Memory is initialised through the
/// same `init_memory_system` funnel when enabled.
pub async fn build_standalone_harness(
    project_root: &std::path::Path,
    config: &concerto_config::AppConfig,
    memory_enabled: bool,
    approval_sink: Arc<dyn ApprovalSink>,
) -> Result<StandaloneHarness, concerto_core::OrchestratorError> {
    use std::sync::Mutex as StdMutex;

    use concerto_core::event::EventBus;
    use concerto_core::lock::DataDirLock;
    use concerto_memory::indexer::ProjectIndexer;
    use concerto_memory::sync::ChunkSyncService;

    let bus = EventBus::new(256);
    let mut registry = crate::runtime_runner_impl::build_tool_registry(project_root, &None, config);
    let (audit, gate_pool) = crate::runtime_runner_impl::build_audit_sink().await;

    // verify.run before environment.describe (describe snapshots tool names).
    let _verify = crate::verify::register_verify_tool(
        &mut registry,
        project_root,
        concerto_eval::EvalEngine::new(project_root),
        None,
    );

    // Memory: same funnel as a run; failed init degrades to the null store.
    let project_id = ProjectId(concerto_core::helpers::project_id_hash(project_root));
    let reindex: Arc<StdMutex<Option<Arc<ProjectIndexer>>>> = Arc::new(StdMutex::new(None));
    let reindex_sync: Arc<StdMutex<Option<Arc<ChunkSyncService>>>> = Arc::new(StdMutex::new(None));
    let memory_cancel: Arc<StdMutex<Option<CancellationToken>>> = Arc::new(StdMutex::new(None));
    let data_dir_lock: Arc<StdMutex<Option<Arc<DataDirLock>>>> = Arc::new(StdMutex::new(None));
    let memory_store: Arc<dyn MemoryStore> = if memory_enabled {
        match crate::runtime_runner::init_memory_system(
            bus.clone(),
            config,
            project_root,
            &reindex,
            &reindex_sync,
            &memory_cancel,
            &data_dir_lock,
        )
        .await
        {
            Ok(store) => store,
            Err(error) => {
                tracing::warn!(%error, "harness memory init failed; memory operations return empty");
                Arc::new(concerto_core::traits::memory::NullMemoryStore)
            }
        }
    } else {
        Arc::new(concerto_core::traits::memory::NullMemoryStore)
    };
    let memory_cancel_token = memory_cancel.lock().unwrap_or_else(|e| e.into_inner()).clone();

    // Checkpoint store rides the sessions pool (the same DB the audit log uses).
    let checkpoint_store = gate_pool.clone().map(|pool| Arc::new(FileCheckpointStore::new(pool)));

    let memory_db_path =
        concerto_sessions::app_data_dir().ok().map(|dir| dir.join("memory").join("memory.db"));

    register_harness_tools(
        &mut registry,
        HarnessToolRegistration {
            project_root,
            checkpoint_store: checkpoint_store.clone(),
            memory_store: Some(memory_store),
            memory_resolver: None,
            memory_db_path,
            project_id,
        },
    );

    // environment.describe LAST so its snapshot covers the tools above.
    let _describe = crate::environment_describe::register_describe_tool(
        &mut registry,
        project_root,
        config,
        gate_pool.is_some(),
        memory_enabled,
        gate_pool.clone(),
    );

    let registry = Arc::new(registry);
    let (executor, _spend, policy, gate_pool) = crate::runtime_runner_impl::setup_policy_and_audit(
        config,
        project_root,
        approval_sink,
        registry,
        true,
        bus.clone(),
        None,
        audit,
        gate_pool,
    )
    .await?;

    // The gate is only meaningful with a durable log pool; without one the
    // restore path reports a loud refusal rather than pretending to gate.
    let checkpoint_parts = match (checkpoint_store, gate_pool) {
        (Some(store), Some(pool)) => {
            let gate = Arc::new(
                WriteGate::new(
                    policy,
                    executor.clone(),
                    pool,
                    Arc::new(crate::gate::FilePreImageReader::new(project_root)),
                    project_root.to_path_buf(),
                    8,
                )
                .with_checkpoint_store((*store).clone()),
            );
            Some((store, gate))
        }
        _ => None,
    };

    let session = SessionContext::new(concerto_core::ids::Ulid::new(), project_root.to_path_buf());
    let mut service = HarnessService::new(executor, session);
    let (access, gate_handle) = match checkpoint_parts {
        Some((store, gate)) => {
            service =
                service.with_checkpoint(CheckpointAccess::new(store.clone(), gate.clone(), None));
            (Some(CheckpointAccess::new(store, gate.clone(), None)), Some(gate))
        }
        None => (None, None),
    };

    Ok(StandaloneHarness {
        service,
        checkpoint: access,
        memory_cancel: memory_cancel_token,
        gate: gate_handle,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use async_trait::async_trait;
    use concerto_core::policy::SimplePolicyEngine;
    use concerto_core::traits::policy::{AuditEntry, AuditLog};
    use concerto_core::traits::tool::Tool;
    use concerto_core::types::{CapabilitySet, Condition, PolicyRule, ToolRegistry};

    struct TestAudit;

    #[async_trait]
    impl AuditLog for TestAudit {
        async fn record(
            &self,
            _entry: AuditEntry,
            _cancel: CancellationToken,
        ) -> Result<(), concerto_core::error::PolicyError> {
            Ok(())
        }
    }

    /// A tool that echoes its input as the `data` payload.
    struct EchoTool {
        name: String,
    }

    #[async_trait]
    impl Tool for EchoTool {
        fn name(&self) -> &str {
            &self.name
        }
        fn description(&self) -> &str {
            "echo"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        fn capability_requirements(&self) -> CapabilitySet {
            CapabilitySet::default()
        }
        async fn execute(
            &self,
            input: serde_json::Value,
            _policy: &dyn concerto_core::traits::PolicyEngine,
            _session: &SessionContext,
            _cancel: CancellationToken,
        ) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput { summary: "echoed".into(), data: input })
        }
    }

    fn service_with(names: &[&str]) -> HarnessService {
        let mut registry = ToolRegistry::default();
        for name in names {
            registry.register(Box::new(EchoTool { name: (*name).to_string() }));
        }
        let policy = Arc::new(SimplePolicyEngine::new(
            vec![PolicyRule::AutoApprove(Condition::Always)],
            Arc::new(TestAudit),
        ));
        let executor = Arc::new(ToolExecutor::new(Arc::new(registry), policy));
        let session = SessionContext::new(concerto_core::ids::Ulid::new(), std::env::temp_dir());
        HarnessService::new(executor, session)
    }

    #[tokio::test]
    async fn invoke_dispatches_each_operation_name() {
        let service = service_with(&[
            ENVIRONMENT_DESCRIBE_TOOL_NAME,
            CHECKPOINT_CREATE_TOOL_NAME,
            CHECKPOINT_INSPECT_TOOL_NAME,
            VERIFY_RUN_TOOL_NAME,
            MEMORY_RECALL_TOOL_NAME,
            MEMORY_INSPECT_TOOL_NAME,
        ]);
        for operation in [
            ENVIRONMENT_DESCRIBE_TOOL_NAME,
            CHECKPOINT_CREATE_TOOL_NAME,
            CHECKPOINT_INSPECT_TOOL_NAME,
            VERIFY_RUN_TOOL_NAME,
            MEMORY_RECALL_TOOL_NAME,
            MEMORY_INSPECT_TOOL_NAME,
        ] {
            let invocation = service
                .invoke(
                    operation,
                    serde_json::json!({"probe": operation}),
                    CancellationToken::new(),
                )
                .await
                .expect("operation runs through the executor");
            assert_eq!(invocation.operation, operation);
            assert_eq!(invocation.data["probe"], operation);
        }
    }

    #[tokio::test]
    async fn unknown_operation_is_refused() {
        let service = service_with(&[]);
        let error = service
            .invoke("nope.nope", serde_json::json!({}), CancellationToken::new())
            .await
            .expect_err("unknown operation refused");
        assert!(matches!(error, HarnessServiceError::UnknownOperation(_)));
    }

    #[tokio::test]
    async fn restore_without_checkpoint_access_is_refused() {
        let service = service_with(&[]);
        let error = service
            .checkpoint_restore(serde_json::json!({"checkpoint_id": "x"}), CancellationToken::new())
            .await
            .expect_err("restore refused without wiring");
        assert!(matches!(error, HarnessServiceError::CheckpointUnavailable));
    }

    #[tokio::test]
    async fn cancelled_invocation_stops_before_execution() {
        let service = service_with(&[ENVIRONMENT_DESCRIBE_TOOL_NAME]);
        let cancel = CancellationToken::new();
        cancel.cancel();
        let error = service
            .environment_describe(serde_json::json!({}), cancel)
            .await
            .expect_err("cancelled token refused");
        assert!(matches!(error, HarnessServiceError::Execution(ToolError::Cancelled)));
    }

    #[test]
    fn invocation_reads_degraded_and_partial_from_payload() {
        let degraded = HarnessInvocation {
            operation: MEMORY_RECALL_TOOL_NAME.into(),
            summary: "s".into(),
            data: serde_json::json!({ "degraded": true, "interrupted": false }),
        };
        assert!(degraded.is_degraded());
        let partial = HarnessInvocation {
            operation: CHECKPOINT_RESTORE_TOOL_NAME.into(),
            summary: "s".into(),
            data: serde_json::json!({ "status": "partial" }),
        };
        assert!(partial.is_partial());
        let complete = HarnessInvocation {
            operation: CHECKPOINT_RESTORE_TOOL_NAME.into(),
            summary: "s".into(),
            data: serde_json::json!({ "status": "restored" }),
        };
        assert!(!complete.is_partial());
    }
}
