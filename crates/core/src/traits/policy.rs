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

    /// Record an infrastructure failure (MCP/plugin) that is not a policy
    /// decision.
    ///
    /// Unlike [`Self::record`], infra rows are not bound to an agent session
    /// (they happen at startup, in a background watcher, or for a plugin that
    /// outlives a session), so implementations persist them with a `NULL`
    /// `session_id` alongside the `error_kind` / `server_id` / `plugin_id`
    /// columns. The default implementation is a fail-soft no-op so minimal
    /// audit sinks (test stubs, in-memory presets) keep compiling; sinks that
    /// persist infra rows override it.
    async fn record_infra(
        &self,
        _entry: InfraAuditEntry,
        _cancel: CancellationToken,
    ) -> Result<(), PolicyError> {
        Ok(())
    }
}

/// Synthetic verdicts for infrastructure failures recorded on the audit log
/// (MCP server lifecycle, WASM plugin lifecycle, capability enforcement).
///
/// These are **not** policy verdicts: every infra row carries
/// [`RULE_INFRA_FAILURE`] as `rule_matched`, a `tool_name` of `mcp:<id>` or
/// `plugin:<id>`, and the human-readable failure detail in `user_response`.
/// They exist so the audit trail can distinguish "the user/policy denied this"
/// from "the subsystem broke / was disabled".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum InfraVerdict {
    /// An MCP server failed to spawn, handshake, or crashed mid-run.
    McpServerFailed,
    /// A WASM plugin failed to load or initialise.
    PluginLoadFailed,
    /// A capability check refused a plugin operation.
    CapabilityDenied,
    /// A plugin was administratively disabled (violation threshold or UI).
    PluginDisabled,
    /// The WASM runtime trapped (fuel/epoch exhaustion or a wasm trap).
    WasmTrap,
}

impl std::fmt::Display for InfraVerdict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let label = match self {
            InfraVerdict::McpServerFailed => "McpServerFailed",
            InfraVerdict::PluginLoadFailed => "PluginLoadFailed",
            InfraVerdict::CapabilityDenied => "CapabilityDenied",
            InfraVerdict::PluginDisabled => "PluginDisabled",
            InfraVerdict::WasmTrap => "WasmTrap",
        };
        f.write_str(label)
    }
}

/// Audit `rule_matched` value for every [`InfraAuditEntry`] row: infrastructure
/// failures are never policy decisions, so they share one sentinel rule name.
pub const RULE_INFRA_FAILURE: &str = "infra_failure";

/// An infrastructure-failure audit row (MCP/plugin), written with a `NULL`
/// session id. See [`InfraVerdict`] and [`AuditLog::record_infra`].
#[derive(Debug, Clone)]
pub struct InfraAuditEntry {
    /// Namespaced tool/subject name: `mcp:<server_id>` or `plugin:<plugin_id>`.
    pub tool_name: String,
    /// Synthetic infra verdict.
    pub verdict: InfraVerdict,
    /// Machine-readable failure category (e.g. `spawn_failed`,
    /// `duplicate_tool`, `wasm_trap`, `fuel_exhausted`). Stored in the
    /// `error_kind` column.
    pub error_kind: Option<String>,
    /// MCP server id, when the failure concerns an MCP server.
    pub server_id: Option<String>,
    /// Plugin id, when the failure concerns a WASM plugin.
    pub plugin_id: Option<String>,
    /// Human-readable failure detail (stored in `user_response`).
    pub detail: Option<String>,
    /// Correlation id, shared with any `session_events` row emitted for the
    /// same failure so the two can be joined.
    pub correlation_id: Ulid,
    /// Wall-clock time the failure was observed.
    pub timestamp: OffsetDateTime,
}

impl InfraAuditEntry {
    /// Build an infra entry for an arbitrary subject (used when the plugin id
    /// is not yet known, e.g. a module that failed to load so only its source
    /// path is available). `tool_name` is taken verbatim; callers normally pass
    /// `plugin:<id>` or `mcp:<id>`.
    pub fn plugin_subject(
        tool_name: impl Into<String>,
        plugin_id: Option<String>,
        verdict: InfraVerdict,
        error_kind: impl Into<String>,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            tool_name: tool_name.into(),
            verdict,
            error_kind: Some(error_kind.into()),
            server_id: None,
            plugin_id,
            detail: Some(detail.into()),
            correlation_id: Ulid::new(),
            timestamp: OffsetDateTime::now_utc(),
        }
    }

    /// Build an MCP-server infra entry (`tool_name = mcp:<server_id>`).
    pub fn mcp(
        server_id: impl Into<String>,
        verdict: InfraVerdict,
        error_kind: impl Into<String>,
        detail: impl Into<String>,
    ) -> Self {
        let server_id = server_id.into();
        Self {
            tool_name: format!("mcp:{server_id}"),
            verdict,
            error_kind: Some(error_kind.into()),
            server_id: Some(server_id),
            plugin_id: None,
            detail: Some(detail.into()),
            correlation_id: Ulid::new(),
            timestamp: OffsetDateTime::now_utc(),
        }
    }

    /// Build a WASM-plugin infra entry (`tool_name = plugin:<plugin_id>`).
    pub fn plugin(
        plugin_id: impl Into<String>,
        verdict: InfraVerdict,
        error_kind: impl Into<String>,
        detail: impl Into<String>,
    ) -> Self {
        let plugin_id = plugin_id.into();
        Self {
            tool_name: format!("plugin:{plugin_id}"),
            verdict,
            error_kind: Some(error_kind.into()),
            server_id: None,
            plugin_id: Some(plugin_id),
            detail: Some(detail.into()),
            correlation_id: Ulid::new(),
            timestamp: OffsetDateTime::now_utc(),
        }
    }
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
