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
