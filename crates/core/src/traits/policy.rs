use crate::error::PolicyError;
use crate::ids::Ulid;
use crate::types::{PolicyAction, PolicyVerdict};
use crate::CancellationToken;
use async_trait::async_trait;
use time::OffsetDateTime;

#[async_trait]
pub trait PolicyEngine: Send + Sync {
    async fn evaluate(
        &self,
        action: &PolicyAction<'_>,
        cancel: CancellationToken,
    ) -> Result<PolicyVerdict, PolicyError>;

    /// ADR-65 F1a: advisory policy evaluation.
    ///
    /// Same decision logic as [`Self::evaluate`], but **side-effect-free**:
    /// it never persists an audit decision row and never consumes integration
    /// quotas (rate-limiter tokens, spend reservations). The read-dedupe serve
    /// path (ADR-65 §3.2) re-evaluates a cached read through this method before
    /// serving it, so a denial still falls through to the normal executor path
    /// without polluting the audit log with a duplicate decision row.
    ///
    /// The default implementation delegates to [`Self::evaluate`] so minimal
    /// engines (test stubs, in-memory presets) keep compiling with their
    /// pre-ADR-65 behavior; engines that can provide the side-effect-free
    /// contract override it.
    async fn evaluate_advisory(
        &self,
        action: &PolicyAction<'_>,
        cancel: CancellationToken,
    ) -> Result<PolicyVerdict, PolicyError> {
        self.evaluate(action, cancel).await
    }

    fn audit_log(&self) -> &dyn AuditLog;
}

/// Append-only. No deletes, no updates.
#[async_trait]
pub trait AuditLog: Send + Sync {
    async fn record(&self, entry: AuditEntry, cancel: CancellationToken)
        -> Result<(), PolicyError>;
}

#[derive(Debug, Clone)]
pub struct AuditEntry {
    pub tool_name: String,
    pub verdict: String,
    pub input_hash: String,
    pub session_id: Ulid,
    pub correlation_id: Ulid,
    pub timestamp: OffsetDateTime,
    pub user_response: Option<String>,
    pub rule_matched: Option<String>,
    // ---- ADR-28 §6/§7: structured command facts + execution results ----
    /// Shell/profile id that produced the command, if any.
    pub profile_id: Option<String>,
    /// Resolved executable path, if known.
    pub resolved_executable: Option<String>,
    /// Full argv, if known.
    pub argv: Option<Vec<String>>,
    /// Working directory, if known.
    pub working_directory: Option<String>,
    /// Whether network egress was requested.
    pub network_requested: Option<bool>,
    /// Filesystem scope classification (Debug string), if known.
    pub filesystem_scope: Option<String>,
    /// Destructive classification (Debug string), if known.
    pub destructive_classification: Option<String>,
    /// Exit code of the executed command, if known (filled post-execution).
    pub exit_code: Option<i32>,
    /// Duration of execution in milliseconds, if known.
    pub duration_ms: Option<i64>,
    /// Toolchain/runtime version that ran the command, if known.
    pub toolchain_version: Option<String>,
    // ---- ADR-55 Phase 1d §4: schema-derived intent-decision columns ----
    /// Bound plan id of a plan-approval decision (`intent:plan`), if any.
    pub plan_id: Option<String>,
    /// Source revision the plan was approved at, if known.
    pub source_revision: Option<String>,
}
