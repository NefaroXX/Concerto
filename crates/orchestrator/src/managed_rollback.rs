//! H09 — managed rollback facade: ONE preflight before execution, ONE
//! chokepoint for execution.
//!
//! An experiment may have touched files, database rows, and processes. A
//! rollback guarantee for it must answer "what can this rollback actually
//! reverse" across ALL of those kinds BEFORE anything executes — never
//! discover mid-restore that an effect is unrecoverable. This module is that
//! gate plus the execution path behind it:
//!
//! 1. **Preflight** ([`preflight_experiment_rollback`], pure, no I/O):
//!    assemble every adapter's coverage
//!    ([`assemble_rollback_offers`]) and run the core
//!    [`preflight_rollback_guarantee`](concerto_core::checkpoint::preflight_rollback_guarantee).
//!    `Guaranteed` may run; `Qualified` may run ONLY with the gaps recorded
//!    and explicitly acknowledged; `Refused` must not run.
//! 2. **Execution** ([`execute_memory_db_restore`],
//!    [`execute_managed_process`]): each enforces the preflight verdict
//!    FIRST (refused → [`RollbackError::Refused`]; qualified without
//!    acknowledgement → [`RollbackError::UnacknowledgedQualified`]) and then
//!    drives the mutation as a host-owned operation through the single
//!    [`ToolExecutor`](concerto_core::executor::ToolExecutor) chokepoint
//!    (`execute_host_operation` — policy-gated, approval-routed, audited,
//!    exactly like the file restore's gated writes). There is no second
//!    chokepoint: the adapter primitives never run outside this path in
//!    production, and the host-operation tools below are never registered in
//!    the agent-visible registry.
//!
//! Production policy posture is fail-closed: the policy engine denies unknown
//! tools by default (`default_deny`), so these host-only tools run in
//! production only under an operator-supplied allow rule or an explicit
//! approval — never silently.
//!
//! # Honest-limits index (the whole H09 claim surface)
//!
//! | Effect | Adapter | Claim |
//! |---|---|---|
//! | File content (within budget) | file checkpoint (`crate::file_checkpoint`, core `CheckpointRecord`) | Byte-exact pre-image replay through gated writes; hash-only above budget (`Manual`/`Partial`); stash inherits the untracked-file gap. |
//! | Working-memory DB rows (`decisions`, `task_nodes`, `memory_links` in `memory.db`) | `concerto-memory::db_checkpoint` | Scoped `BEGIN IMMEDIATE` snapshot (no file copy); single-transaction `DELETE+INSERT` restore; validated after commit (counts + hash + `quick_check` + FK check). Data-only. |
//! | DB schema/DDL/migrations | — | UNSUPPORTED: restore refuses (`SchemaChanged`) when live `CREATE` statements differ. |
//! | FTS / vector / global-memory / reindex state | — | UNSUPPORTED (out of scope; unclaimed at preflight). |
//! | Concerto-owned processes | `crate::managed_process` | Stop + recreate from recorded spec with readiness observation. Restart only. |
//! | Live process memory | — | UNSUPPORTED: no backend exists (`MEMORY_STATE_UNSUPPORTED`); reports always carry `memory_state_restored: false`. |
//! | Non-managed processes / grandchildren | — | UNSUPPORTED: the adapter cannot address what it never spawned; stop signals the direct child only. |
//! | Shell side-effects, network effects | — | UNSUPPORTED: never reversed; any requested guarantee covering them is refused. |

use std::collections::BTreeMap;
use std::sync::Arc;

use concerto_core::checkpoint::{
    file_checkpoint_coverage, preflight_rollback_guarantee, CheckpointRecord, EffectCoverage,
    EffectKind, EffectSupport, RollbackPreflight,
};
use concerto_core::error::ToolError;
use concerto_core::executor::ToolExecutor;
use concerto_core::traits::policy::PolicyEngine;
use concerto_core::traits::tool::Tool;
use concerto_core::types::{CapabilitySet, SessionContext, ToolOutput};
use concerto_core::CancellationToken;
use concerto_memory::db_checkpoint::{
    checkpoint_memory_db, memory_db_adapter_coverage, restore_memory_db, DbScopeOwnership,
    MemoryDbCheckpoint, MemoryDbCheckpointError, MEMORY_DB_CHECKPOINT_IO_TIMEOUT,
};
use sqlx::SqlitePool;
use thiserror::Error;

use crate::managed_process::{
    process_adapter_coverage, ManagedProcessError, ManagedProcessRegistry, ReadinessProbe,
};

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Errors from the managed rollback facade.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum RollbackError {
    /// The preflight refused the requested guarantee: the rollback must NOT
    /// run. Carries the report so the refusal is auditable.
    #[error("rollback refused: incomplete coverage: {0:?}")]
    Refused(RollbackPreflight),
    /// The preflight qualified the guarantee but the caller did not
    /// acknowledge the gaps: the rollback must NOT run silently.
    #[error("rollback qualified but not acknowledged: {0:?}")]
    UnacknowledgedQualified(RollbackPreflight),
    /// The policy-gated execution failed.
    #[error("rollback execution failed: {0}")]
    Tool(#[from] ToolError),
    /// The operation was cancelled.
    #[error("rollback cancelled")]
    Cancelled,
}

// ---------------------------------------------------------------------------
// Preflight
// ---------------------------------------------------------------------------

/// Assemble the `offered` coverage for a rollback: the file checkpoint's
/// share (or all-`Unsupported` when there is no file checkpoint — an absent
/// checkpoint claims nothing, never a default), plus the H09 database and
/// process adapter shares. Per kind the strongest claim wins
/// (`Supported` > `Qualified` > `Unsupported`); the result is deterministic
/// in [`EffectKind`] order.
pub fn assemble_rollback_offers(file_record: Option<&CheckpointRecord>) -> Vec<EffectCoverage> {
    assemble_rollback_offers_with_sandbox(file_record, &[])
}

/// Assemble rollback offers with an extra sandbox capability share (H10).
///
/// `sandbox_offers` is a [`SandboxCapabilityDescriptor`](concerto_core::sandbox::SandboxCapabilityDescriptor)'s
/// [`offers`](concerto_core::sandbox::SandboxCapabilityDescriptor::offers) —
/// all-`Unsupported` in this milestone, since no container/runtime
/// checkpoint-restore backend ships (ADR-83). The merge is strongest-wins
/// per kind, so an all-`Unsupported` share can never upgrade (or downgrade)
/// another adapter's claim: it composes structurally without changing any
/// verdict. Additive seam: existing callers keep calling
/// [`assemble_rollback_offers`].
pub fn assemble_rollback_offers_with_sandbox(
    file_record: Option<&CheckpointRecord>,
    sandbox_offers: &[EffectCoverage],
) -> Vec<EffectCoverage> {
    let mut sources: Vec<EffectCoverage> = Vec::new();
    match file_record {
        Some(record) => sources.extend(file_checkpoint_coverage(record)),
        None => {
            for kind in EffectKind::all() {
                sources.push(EffectCoverage {
                    kind: *kind,
                    support: EffectSupport::Unsupported {
                        reason: "no file checkpoint anchors this rollback".to_string(),
                    },
                });
            }
        }
    }
    sources.extend(memory_db_adapter_coverage());
    sources.extend(process_adapter_coverage());
    sources.extend(sandbox_offers.iter().cloned());
    fn rank(support: &EffectSupport) -> u8 {
        match support {
            EffectSupport::Supported { .. } => 2,
            EffectSupport::Qualified { .. } => 1,
            EffectSupport::Unsupported { .. } => 0,
        }
    }
    let mut merged: BTreeMap<EffectKind, EffectCoverage> = BTreeMap::new();
    // `EffectKind` is `Ord`, so iteration below is deterministic. (BTreeMap
    // needs `Ord`, not just `PartialEq` — the core enum provides it.)
    for offer in sources {
        match merged.get(&offer.kind) {
            Some(current) if rank(&current.support) >= rank(&offer.support) => {}
            _ => {
                merged.insert(offer.kind, offer);
            }
        }
    }
    // BTreeMap iteration is key-ordered; EffectKind order = declaration order.
    merged.into_values().collect()
}

/// Preflight a rollback guarantee for an experiment BEFORE executing
/// anything.
///
/// `observed` must be the FULL effect list (every kind the experiment may
/// have touched); `file_record` is the anchoring file checkpoint when one
/// exists. Pure: no I/O, no mutation — the before-execution gate the plan
/// requires. Callers execute only on `Guaranteed`, or on `Qualified` with
/// the gaps recorded and explicitly acknowledged
/// ([`check_preflight_for_execution`] enforces both).
pub fn preflight_experiment_rollback(
    observed: &[EffectKind],
    file_record: Option<&CheckpointRecord>,
) -> RollbackPreflight {
    preflight_rollback_guarantee(observed, &assemble_rollback_offers(file_record))
}

/// Enforce a preflight verdict before execution: `Refused` never runs;
/// `Qualified` runs only with `acknowledge_qualified` (the gaps are then the
/// caller's recorded qualification — never silent).
pub fn check_preflight_for_execution(
    preflight: &RollbackPreflight,
    acknowledge_qualified: bool,
) -> Result<(), RollbackError> {
    use concerto_core::checkpoint::PreflightVerdict;
    match preflight.verdict {
        PreflightVerdict::Guaranteed => Ok(()),
        PreflightVerdict::Qualified if acknowledge_qualified => Ok(()),
        PreflightVerdict::Qualified => {
            Err(RollbackError::UnacknowledgedQualified(preflight.clone()))
        }
        PreflightVerdict::Refused => Err(RollbackError::Refused(preflight.clone())),
    }
}

// ---------------------------------------------------------------------------
// Host-owned operations (the single chokepoint's operands)
// ---------------------------------------------------------------------------
//
// These `Tool` impls are NEVER registered in the agent-visible registry:
// the facade below invokes them via `ToolExecutor::execute_host_operation`
// (the host-constructs-the-operation path, per the plugin HTTP precedent),
// so every execution is policy-gated, approval-routed, and audited through
// the same `execute_resolved` path as any other write. Ownership and schema
// checks stay inside the operation (fail-closed before mutation), exactly
// like the file restore's oracle pre-checks.

/// Host-owned database restore: replays a [`MemoryDbCheckpoint`] over its
/// pool. The checkpoint is fixed at construction (host-supplied); the input
/// must echo its `checkpoint_id` or the operation fails loudly (a mismatched
/// input is a host bug, never silently executed).
pub struct MemoryDbRestoreHostOp {
    pool: SqlitePool,
    checkpoint: MemoryDbCheckpoint,
    ownership: Arc<dyn DbScopeOwnership>,
    by_agent: String,
}

impl MemoryDbRestoreHostOp {
    /// Construct the operation (host side — agents never construct this).
    pub fn new(
        pool: SqlitePool,
        checkpoint: MemoryDbCheckpoint,
        ownership: Arc<dyn DbScopeOwnership>,
        by_agent: String,
    ) -> Self {
        Self { pool, checkpoint, ownership, by_agent }
    }

    /// The checkpoint this operation replays.
    pub fn checkpoint_id(&self) -> &str {
        &self.checkpoint.checkpoint_id
    }
}

impl std::fmt::Debug for MemoryDbRestoreHostOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryDbRestoreHostOp")
            .field("checkpoint_id", &self.checkpoint.checkpoint_id)
            .field("by_agent", &self.by_agent)
            .finish_non_exhaustive()
    }
}

fn db_error_to_tool(error: MemoryDbCheckpointError) -> ToolError {
    match error {
        MemoryDbCheckpointError::Cancelled => ToolError::Cancelled,
        MemoryDbCheckpointError::Timeout => {
            ToolError::Timeout { timeout_secs: MEMORY_DB_CHECKPOINT_IO_TIMEOUT.as_secs() }
        }
        // Refusals (ownership / schema drift) are authorization-scope
        // refusals: `PolicyDenied` keeps them distinct from execution
        // failures, mirroring the file restore's denial mapping.
        MemoryDbCheckpointError::OwnershipConflict { scope, owner } => ToolError::PolicyDenied {
            rule: format!("ownership_conflict: {scope} owned by {owner}"),
        },
        MemoryDbCheckpointError::SchemaChanged { detail } => {
            ToolError::PolicyDenied { rule: format!("schema_changed: {detail}") }
        }
        other => ToolError::ExecutionFailed { message: other.to_string() },
    }
}

#[async_trait::async_trait]
impl Tool for MemoryDbRestoreHostOp {
    fn name(&self) -> &str {
        "memory_db_restore"
    }

    fn description(&self) -> &str {
        "Host-owned scoped restore of the working-memory database tables (never agent-registered)"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {"checkpoint_id": {"type": "string"}},
            "required": ["checkpoint_id"],
        })
    }

    fn capability_requirements(&self) -> CapabilitySet {
        CapabilitySet::default()
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        _policy: &dyn PolicyEngine,
        _session: &SessionContext,
        cancel: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let input_id = input.get("checkpoint_id").and_then(serde_json::Value::as_str).unwrap_or("");
        if input_id != self.checkpoint.checkpoint_id {
            return Err(ToolError::ExecutionFailed {
                message: format!(
                    "memory_db_restore input checkpoint_id mismatch: input {input_id:?} != operation {}",
                    self.checkpoint.checkpoint_id
                ),
            });
        }
        let report = restore_memory_db(
            &self.pool,
            &self.checkpoint,
            self.ownership.as_ref(),
            &self.by_agent,
            cancel,
        )
        .await
        .map_err(db_error_to_tool)?;
        Ok(ToolOutput {
            summary: format!(
                "memory db scope restored from checkpoint {}",
                self.checkpoint.checkpoint_id
            ),
            data: serde_json::to_value(&report).unwrap_or(serde_json::Value::Null),
        })
    }
}

/// One managed-process lifecycle mutation (host-owned).
#[derive(Debug, Clone)]
pub enum ProcessLifecycleAction {
    /// Stop the managed process `id`.
    Stop {
        /// The registry id.
        id: String,
    },
    /// Recreate the managed process `id` with readiness `probe`.
    Recreate {
        /// The registry id.
        id: String,
        /// The readiness probe for the new child.
        probe: ReadinessProbe,
    },
}

impl ProcessLifecycleAction {
    fn id(&self) -> &str {
        match self {
            Self::Stop { id } | Self::Recreate { id, .. } => id,
        }
    }

    fn operation(&self) -> &'static str {
        match self {
            Self::Stop { .. } => "stop",
            Self::Recreate { .. } => "recreate",
        }
    }
}

/// Host-owned process lifecycle operation over a [`ManagedProcessRegistry`].
/// Input must echo the action's `operation` + `id` or the operation fails
/// loudly.
pub struct ManagedProcessHostOp {
    registry: Arc<ManagedProcessRegistry>,
    action: ProcessLifecycleAction,
    by_agent: String,
}

impl ManagedProcessHostOp {
    /// Construct the operation (host side — agents never construct this).
    pub fn new(
        registry: Arc<ManagedProcessRegistry>,
        action: ProcessLifecycleAction,
        by_agent: String,
    ) -> Self {
        Self { registry, action, by_agent }
    }
}

impl std::fmt::Debug for ManagedProcessHostOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ManagedProcessHostOp")
            .field("action", &self.action)
            .field("by_agent", &self.by_agent)
            .finish_non_exhaustive()
    }
}

fn process_error_to_tool(error: ManagedProcessError) -> ToolError {
    match error {
        ManagedProcessError::Cancelled => ToolError::Cancelled,
        ManagedProcessError::OwnershipRefused { id, owner } => {
            ToolError::PolicyDenied { rule: format!("ownership_conflict: {id} owned by {owner}") }
        }
        other => ToolError::ExecutionFailed { message: other.to_string() },
    }
}

#[async_trait::async_trait]
impl Tool for ManagedProcessHostOp {
    fn name(&self) -> &str {
        "managed_process_lifecycle"
    }

    fn description(&self) -> &str {
        "Host-owned stop/recreate of Concerto-owned processes (never agent-registered)"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "operation": {"type": "string"},
                "id": {"type": "string"},
            },
            "required": ["operation", "id"],
        })
    }

    fn capability_requirements(&self) -> CapabilitySet {
        CapabilitySet::default()
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        _policy: &dyn PolicyEngine,
        _session: &SessionContext,
        cancel: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let input_op = input.get("operation").and_then(serde_json::Value::as_str).unwrap_or("");
        let input_id = input.get("id").and_then(serde_json::Value::as_str).unwrap_or("");
        if input_op != self.action.operation() || input_id != self.action.id() {
            return Err(ToolError::ExecutionFailed {
                message: format!(
                    "managed_process_lifecycle input mismatch: input ({input_op:?}, {input_id:?}) != operation ({}, {})",
                    self.action.operation(),
                    self.action.id()
                ),
            });
        }
        match &self.action {
            ProcessLifecycleAction::Stop { id } => {
                let report = self
                    .registry
                    .stop(id, &self.by_agent, &cancel)
                    .await
                    .map_err(process_error_to_tool)?;
                Ok(ToolOutput {
                    summary: format!("managed process {id} stopped"),
                    data: serde_json::to_value(&report).unwrap_or(serde_json::Value::Null),
                })
            }
            ProcessLifecycleAction::Recreate { id, probe } => {
                let report = self
                    .registry
                    .recreate(id, &self.by_agent, probe.clone(), &cancel)
                    .await
                    .map_err(process_error_to_tool)?;
                Ok(ToolOutput {
                    summary: format!(
                        "managed process {id} recreated (restart — memory state not restored)"
                    ),
                    data: serde_json::to_value(&report).unwrap_or(serde_json::Value::Null),
                })
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Facade execution (preflight verdict enforced first, always)
// ---------------------------------------------------------------------------

fn rollback_tool_error(error: ToolError) -> RollbackError {
    match error {
        ToolError::Cancelled => RollbackError::Cancelled,
        other => RollbackError::Tool(other),
    }
}

/// Restore the working-memory DB scope through the executor chokepoint.
///
/// Enforces the preflight FIRST: `Refused` and unacknowledged `Qualified`
/// never reach the executor (no mutation, no audit row for a run that must
/// not happen). On success the [`ToolOutput::data`] carries the
/// [`MemoryDbRestoreReport`](concerto_memory::db_checkpoint::MemoryDbRestoreReport).
pub async fn execute_memory_db_restore(
    executor: &ToolExecutor,
    op: &MemoryDbRestoreHostOp,
    session: &SessionContext,
    preflight: &RollbackPreflight,
    acknowledge_qualified: bool,
    cancel: CancellationToken,
) -> Result<ToolOutput, RollbackError> {
    check_preflight_for_execution(preflight, acknowledge_qualified)?;
    let checkpoint_id = op.checkpoint_id().to_owned();
    executor
        .execute_host_operation(
            op,
            serde_json::json!({"checkpoint_id": checkpoint_id}),
            session,
            cancel,
        )
        .await
        .map_err(rollback_tool_error)
}

/// Stop/recreate a managed process through the executor chokepoint.
///
/// Same preflight-first enforcement as [`execute_memory_db_restore`].
pub async fn execute_managed_process(
    executor: &ToolExecutor,
    op: &ManagedProcessHostOp,
    session: &SessionContext,
    preflight: &RollbackPreflight,
    acknowledge_qualified: bool,
    cancel: CancellationToken,
) -> Result<ToolOutput, RollbackError> {
    check_preflight_for_execution(preflight, acknowledge_qualified)?;
    let (operation, id) = (op.action.operation().to_string(), op.action.id().to_string());
    executor
        .execute_host_operation(
            op,
            serde_json::json!({"operation": operation, "id": id}),
            session,
            cancel,
        )
        .await
        .map_err(rollback_tool_error)
}

/// Capture a working-memory DB checkpoint (convenience re-export of the
/// adapter primitive for facade callers).
pub async fn capture_memory_db_checkpoint(
    pool: &SqlitePool,
    cancel: CancellationToken,
) -> Result<MemoryDbCheckpoint, MemoryDbCheckpointError> {
    checkpoint_memory_db(pool, cancel).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use concerto_core::checkpoint::{CheckpointScope, CheckpointStatus, RestoreMethod};
    use concerto_core::error::PolicyError;
    use concerto_core::ids::Ulid;
    use concerto_core::traits::policy::AuditEntry;
    use concerto_core::types::{PolicyAction, PolicyVerdict, ToolRegistry};
    use concerto_memory::db_checkpoint::DbScopeOwnership;
    use concerto_memory::storage::MemoryDb;

    use std::collections::BTreeMap;

    // ---- Test policy/audit doubles (mirror the core executor harness) ----

    struct AllowPolicy;
    #[async_trait::async_trait]
    impl PolicyEngine for AllowPolicy {
        async fn evaluate(
            &self,
            _action: &PolicyAction<'_>,
            _cancel: CancellationToken,
        ) -> Result<PolicyVerdict, PolicyError> {
            Ok(PolicyVerdict::Allow)
        }

        fn audit_log(&self) -> &dyn concerto_core::traits::policy::AuditLog {
            &IgnoreAudit
        }
    }

    struct IgnoreAudit;
    #[async_trait::async_trait]
    impl concerto_core::traits::policy::AuditLog for IgnoreAudit {
        async fn record(
            &self,
            _entry: AuditEntry,
            _cancel: CancellationToken,
        ) -> Result<(), PolicyError> {
            Ok(())
        }
    }

    #[derive(Debug, Default)]
    struct UnownedScopes;
    impl DbScopeOwnership for UnownedScopes {
        fn owner_of(&self, _scope: &str) -> Option<String> {
            None
        }
    }

    fn test_session() -> SessionContext {
        let dir = std::env::temp_dir();
        SessionContext::new(Ulid::new(), dir)
    }

    fn test_executor() -> ToolExecutor {
        ToolExecutor::new(Arc::new(ToolRegistry::default()), Arc::new(AllowPolicy))
    }

    fn active_file_record() -> CheckpointRecord {
        CheckpointRecord::new(
            Ulid::new(),
            Ulid::new(),
            CheckpointScope::FilesystemWrites { paths: vec!["a.txt".to_string()] },
            Vec::new(),
            Vec::new(),
            BTreeMap::new(),
            CheckpointStatus::Active,
            RestoreMethod::PreImageStore,
            Vec::new(),
            "agent-a".to_string(),
            1_700_000_000_000,
            None,
        )
        .expect("record builds")
    }

    async fn temp_memory_db() -> (tempfile::TempDir, MemoryDb) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path =
            camino::Utf8PathBuf::from_path_buf(dir.path().join("memory.db")).expect("utf8 path");
        let db = MemoryDb::connect(&path).await.expect("temp memory db");
        (dir, db)
    }

    #[test]
    fn offers_merge_strongest_claim_per_kind() {
        let record = active_file_record();
        let offers = assemble_rollback_offers(Some(&record));
        assert_eq!(offers.len(), EffectKind::all().len());
        let file = offers.iter().find(|entry| entry.kind == EffectKind::File).expect("file");
        assert!(file.support.is_full(), "active file record covers files: {file:?}");
        let database =
            offers.iter().find(|entry| entry.kind == EffectKind::Database).expect("database");
        assert!(
            matches!(database.support, EffectSupport::Qualified { .. }),
            "db adapter qualifies: {database:?}"
        );
        let process =
            offers.iter().find(|entry| entry.kind == EffectKind::Process).expect("process");
        assert!(
            matches!(process.support, EffectSupport::Qualified { .. }),
            "process adapter qualifies: {process:?}"
        );
        // No file checkpoint: nothing claims files either.
        let offers = assemble_rollback_offers(None);
        for offer in &offers {
            assert!(
                matches!(
                    offer.support,
                    EffectSupport::Qualified { .. } | EffectSupport::Unsupported { .. }
                ),
                "without a file checkpoint nothing is fully covered: {offer:?}"
            );
        }
    }

    #[test]
    fn shell_and_network_are_always_refused() {
        let record = active_file_record();
        let preflight = preflight_experiment_rollback(
            &[EffectKind::File, EffectKind::Shell, EffectKind::Network],
            Some(&record),
        );
        assert_eq!(preflight.verdict, concerto_core::checkpoint::PreflightVerdict::Refused);
        let refusal =
            check_preflight_for_execution(&preflight, true).expect_err("refused never runs");
        assert!(refusal.to_string().contains("refused"), "{refusal}");
    }

    #[test]
    fn sandbox_share_never_upgrades_another_adapters_claim() {
        use concerto_core::sandbox::{sandbox_capability_for, RuntimeAvailability};
        use concerto_core::types::SandboxProfile;
        // H10 feed: a sandbox descriptor's offers merge strongest-wins, so
        // the all-Unsupported sandbox share of this milestone changes no
        // verdict — it composes structurally without touching preflight.
        let sandbox_offers = sandbox_capability_for(
            SandboxProfile::Containerized,
            &RuntimeAvailability::Unavailable {
                reason: "no container runtime (docker or podman) found on PATH".to_string(),
            },
        )
        .offers();
        assert!(
            sandbox_offers
                .iter()
                .all(|offer| matches!(offer.support, EffectSupport::Unsupported { .. })),
            "this milestone's sandbox share restores nothing"
        );
        let record = active_file_record();
        let plain = assemble_rollback_offers(Some(&record));
        let merged = assemble_rollback_offers_with_sandbox(Some(&record), &sandbox_offers);
        assert_eq!(plain, merged, "an all-Unsupported share is verdict-neutral");
        let file = merged.iter().find(|entry| entry.kind == EffectKind::File).expect("file");
        assert!(file.support.is_full(), "the file claim survives the sandbox merge");
        // …and shell/network stay refused through the merged share.
        let preflight = preflight_rollback_guarantee(
            &[EffectKind::File, EffectKind::Shell, EffectKind::Network],
            &merged,
        );
        assert_eq!(preflight.verdict, concerto_core::checkpoint::PreflightVerdict::Refused);
        // Without a file checkpoint the sandbox share alone still refuses.
        let merged = assemble_rollback_offers_with_sandbox(None, &sandbox_offers);
        let preflight =
            preflight_rollback_guarantee(&[EffectKind::File, EffectKind::Shell], &merged);
        assert_eq!(preflight.verdict, concerto_core::checkpoint::PreflightVerdict::Refused);
    }

    #[test]
    fn qualified_requires_acknowledgement_before_execution() {
        let record = active_file_record();
        let preflight = preflight_experiment_rollback(
            &[EffectKind::File, EffectKind::Database, EffectKind::Process],
            Some(&record),
        );
        assert_eq!(preflight.verdict, concerto_core::checkpoint::PreflightVerdict::Qualified);
        assert!(matches!(
            check_preflight_for_execution(&preflight, false),
            Err(RollbackError::UnacknowledgedQualified(_))
        ));
        assert!(check_preflight_for_execution(&preflight, true).is_ok());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn refused_preflight_runs_nothing_db_and_process_untouched() {
        // The acceptance shape: a guarantee covering shell effects is refused
        // BEFORE any execution — prove it by showing the live DB rows and the
        // managed child are exactly as before the (never-run) rollback.
        let (_dir, db) = temp_memory_db().await;
        sqlx::query(
            "INSERT INTO decisions (id, session_id, task_id, what, why, outcome, category, confidence, superseded_by, created_at) VALUES ('d1', 's1', NULL, 'w', 'y', NULL, '\"other\"', 0.5, NULL, 1)",
        )
        .execute(db.pool())
        .await
        .expect("seed");
        let registry = Arc::new(ManagedProcessRegistry::new());
        let cancel = CancellationToken::new();
        let spec = crate::managed_process::ManagedProcessSpec::new(
            "p1".to_string(),
            "sleep".to_string(),
            vec!["30".to_string()],
            std::env::temp_dir(),
            BTreeMap::new(),
            "agent-a".to_string(),
            0,
        )
        .expect("spec");
        registry.register_and_spawn(spec, ReadinessProbe::Alive, &cancel).await.expect("spawn");

        let record = active_file_record();
        let preflight = preflight_experiment_rollback(
            &[EffectKind::File, EffectKind::Database, EffectKind::Process, EffectKind::Shell],
            Some(&record),
        );
        assert_eq!(preflight.verdict, concerto_core::checkpoint::PreflightVerdict::Refused);
        // The facade refuses without touching the executor at all.
        let executor = test_executor();
        let session = test_session();
        let checkpoint = capture_memory_db_checkpoint(db.pool(), CancellationToken::new())
            .await
            .expect("capture");
        let op = MemoryDbRestoreHostOp::new(
            db.pool().clone(),
            checkpoint,
            Arc::new(UnownedScopes),
            "agent-a".to_string(),
        );
        assert!(matches!(
            execute_memory_db_restore(
                &executor,
                &op,
                &session,
                &preflight,
                true,
                CancellationToken::new()
            )
            .await,
            Err(RollbackError::Refused(_))
        ));

        // Nothing ran: the seed row is intact, the child is still managed.
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM decisions")
            .fetch_one(db.pool())
            .await
            .expect("reread");
        assert_eq!(count, 1);
        assert!(registry.contains("p1").await);
        registry.stop("p1", "agent-a", &cancel).await.expect("cleanup");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn qualified_db_restore_runs_through_executor_when_acknowledged() {
        let (_dir, db) = temp_memory_db().await;
        sqlx::query(
            "INSERT INTO decisions (id, session_id, task_id, what, why, outcome, category, confidence, superseded_by, created_at) VALUES ('d1', 's1', NULL, 'w', 'y', NULL, '\"other\"', 0.5, NULL, 1)",
        )
        .execute(db.pool())
        .await
        .expect("seed");
        let checkpoint = capture_memory_db_checkpoint(db.pool(), CancellationToken::new())
            .await
            .expect("capture");
        sqlx::query("DELETE FROM decisions").execute(db.pool()).await.expect("mutate");

        let record = active_file_record();
        let preflight =
            preflight_experiment_rollback(&[EffectKind::File, EffectKind::Database], Some(&record));
        assert_eq!(preflight.verdict, concerto_core::checkpoint::PreflightVerdict::Qualified);

        let executor = test_executor();
        let session = test_session();
        let op = MemoryDbRestoreHostOp::new(
            db.pool().clone(),
            checkpoint,
            Arc::new(UnownedScopes),
            "agent-a".to_string(),
        );
        // Unacknowledged: no execution, rows stay deleted.
        assert!(matches!(
            execute_memory_db_restore(
                &executor,
                &op,
                &session,
                &preflight,
                false,
                CancellationToken::new()
            )
            .await,
            Err(RollbackError::UnacknowledgedQualified(_))
        ));
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM decisions")
            .fetch_one(db.pool())
            .await
            .expect("reread");
        assert_eq!(count, 0);
        // Acknowledged: the policy-gated restore runs and validates.
        let output = execute_memory_db_restore(
            &executor,
            &op,
            &session,
            &preflight,
            true,
            CancellationToken::new(),
        )
        .await
        .expect("acknowledged restore runs");
        assert!(output.summary.contains("restored"));
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM decisions")
            .fetch_one(db.pool())
            .await
            .expect("reread");
        assert_eq!(count, 1);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn process_recreate_runs_through_executor_with_readiness_and_no_memory_claim() {
        let registry = Arc::new(ManagedProcessRegistry::new());
        let cancel = CancellationToken::new();
        let spec = crate::managed_process::ManagedProcessSpec::new(
            "p1".to_string(),
            "sleep".to_string(),
            vec!["30".to_string()],
            std::env::temp_dir(),
            BTreeMap::new(),
            "agent-a".to_string(),
            0,
        )
        .expect("spec");
        registry.register_and_spawn(spec, ReadinessProbe::Alive, &cancel).await.expect("spawn");

        let record = active_file_record();
        let preflight =
            preflight_experiment_rollback(&[EffectKind::File, EffectKind::Process], Some(&record));
        assert_eq!(preflight.verdict, concerto_core::checkpoint::PreflightVerdict::Qualified);

        let executor = test_executor();
        let session = test_session();
        let op = ManagedProcessHostOp::new(
            registry.clone(),
            ProcessLifecycleAction::Recreate { id: "p1".to_string(), probe: ReadinessProbe::Alive },
            "agent-a".to_string(),
        );
        let output = execute_managed_process(
            &executor,
            &op,
            &session,
            &preflight,
            true,
            CancellationToken::new(),
        )
        .await
        .expect("acknowledged recreate runs");
        let report: crate::managed_process::RecreateReport =
            serde_json::from_value(output.data).expect("report");
        assert!(report.readiness.ready);
        assert!(!report.memory_state_restored, "restart is never memory recovery");
        assert_ne!(report.old_pid, report.new_pid);

        // A foreign agent's operation is denied by policy mapping — and the
        // child survives (refused before any signal).
        let op = ManagedProcessHostOp::new(
            registry.clone(),
            ProcessLifecycleAction::Stop { id: "p1".to_string() },
            "agent-b".to_string(),
        );
        match execute_managed_process(
            &executor,
            &op,
            &session,
            &preflight,
            true,
            CancellationToken::new(),
        )
        .await
        {
            Err(RollbackError::Tool(ToolError::PolicyDenied { rule })) => {
                assert!(rule.contains("agent-a"), "{rule}");
            }
            other => panic!("foreign stop must be policy-denied, got {other:?}"),
        }
        assert!(registry.contains("p1").await);
        registry.stop("p1", "agent-a", &cancel).await.expect("cleanup");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn host_op_input_mismatch_fails_loudly() {
        let registry = Arc::new(ManagedProcessRegistry::new());
        let executor = test_executor();
        let session = test_session();
        let op = ManagedProcessHostOp::new(
            registry,
            ProcessLifecycleAction::Stop { id: "p1".to_string() },
            "agent-a".to_string(),
        );
        // Bypass the facade to feed a mismatched input directly: the op
        // fails instead of acting on the wrong id.
        match executor
            .execute_host_operation(
                &op,
                serde_json::json!({"operation": "stop", "id": "other"}),
                &session,
                CancellationToken::new(),
            )
            .await
        {
            Err(ToolError::ExecutionFailed { message }) => {
                assert!(message.contains("mismatch"), "{message}");
            }
            other => panic!("mismatched input must fail loudly, got {other:?}"),
        }
    }
}
