//! Tool-execution backend seam — ADR-60 S5 (agent-process entry).
//!
//! The single-agent loop calls tools through one narrow interface,
//! [`ToolExecutionBackend`], instead of the concrete [`ToolExecutor`]. The
//! in-process path implements it by delegating to the local executor; the
//! supervised path implements it with a gate-proxy client that forwards
//! every call to the supervisor's write gate over stdio (ADR-60 D4). This is
//! the executor call-site swap the ADR's slicing note calls for: the loop
//! does not know which backend it runs under.
//!
//! `tool_definitions` is deliberately synchronous: the loop builds provider
//! requests in a non-async helper, and a backend may cache the registry it
//! fetched during connect (the gate-proxy does exactly that).

use std::sync::Arc;

use async_trait::async_trait;
use concerto_core::error::ToolError;
use concerto_core::executor::ToolExecutor;
use concerto_core::ids::Ulid;
use concerto_core::types::{SessionContext, ToolDefinition, ToolOutput};
use concerto_core::CancellationToken;

/// The execution backend seam behind the loop's single tool call site.
#[async_trait]
pub trait ToolExecutionBackend: Send + Sync {
    /// Tool definitions to present to the model (may be cached/fetched).
    fn tool_definitions(&self) -> Vec<ToolDefinition>;

    /// Execute one tool call.
    ///
    /// `call_id` is the idempotency key the supervised path forwards to the
    /// gate (`GateRequest.call_id`); the local path has no dedup layer and
    /// ignores it.
    async fn execute(
        &self,
        tool_name: &str,
        input: serde_json::Value,
        call_id: &str,
        session: &SessionContext,
        cancel: CancellationToken,
    ) -> Result<ToolOutput, ToolError>;

    /// Persist an acknowledgment decision through the audit channel (ADR-55
    /// §5 / audit H-04). The supervised path logs a warning instead: in the
    /// ADR-60 model the audit trail is written supervisor-side (D4/D5).
    async fn record_ack_decision(
        &self,
        session_id: Ulid,
        correlation_id: Ulid,
        message: &str,
        acknowledged: bool,
        cancel: CancellationToken,
    );

    /// ADR-65 F1a: re-evaluate a proposed tool call without recording a
    /// decision row; returns `true` only for an explicit `Allow`.
    ///
    /// The in-process path delegates to the concrete executor's advisory gate
    /// (see [`ToolExecutor::policy_verdict_is_allow`]). The default is `false`
    /// because the supervised path never serves cached reads — the gate-proxy
    /// child has no resource-facts store — so disable-by-default is the
    /// truthful contract for every backend that does not override it.
    async fn policy_verdict_is_allow(
        &self,
        _tool_name: &str,
        _input: &serde_json::Value,
        _session: &SessionContext,
        _cancel: CancellationToken,
    ) -> bool {
        false
    }

    /// ADR-65 F1b: persist a `ServedFromCache` audit row for a cached read.
    ///
    /// Called only when the serve gate served a read *without* executing the
    /// tool (see [`ToolExecutor::record_served_read_audit`]). The supervised
    /// path uses the default no-op: its audit trail is written supervisor-side
    /// (ADR-60 D4/D5) and it never serves, so there is nothing to record.
    async fn record_served_read_audit(
        &self,
        _tool_name: &str,
        _input: &serde_json::Value,
        _path: &str,
        _session: &SessionContext,
        _cancel: CancellationToken,
    ) {
    }
}

/// The local (single-process) backend: plain delegation to the concrete
/// [`ToolExecutor`]. The loop and every existing construction site keep
/// working unchanged — `Arc<ToolExecutor>` coerces to `Arc<dyn
/// ToolExecutionBackend>`.
#[async_trait]
impl ToolExecutionBackend for ToolExecutor {
    fn tool_definitions(&self) -> Vec<ToolDefinition> {
        ToolExecutor::tool_definitions(self)
    }

    async fn execute(
        &self,
        tool_name: &str,
        input: serde_json::Value,
        _call_id: &str,
        session: &SessionContext,
        cancel: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        ToolExecutor::execute(self, tool_name, input, session, cancel).await
    }

    async fn record_ack_decision(
        &self,
        session_id: Ulid,
        correlation_id: Ulid,
        message: &str,
        acknowledged: bool,
        cancel: CancellationToken,
    ) {
        ToolExecutor::record_ack_decision(
            self,
            session_id,
            correlation_id,
            message,
            acknowledged,
            cancel,
        )
        .await;
    }

    async fn policy_verdict_is_allow(
        &self,
        tool_name: &str,
        input: &serde_json::Value,
        session: &SessionContext,
        cancel: CancellationToken,
    ) -> bool {
        ToolExecutor::policy_verdict_is_allow(self, tool_name, input, session, cancel).await
    }

    async fn record_served_read_audit(
        &self,
        tool_name: &str,
        input: &serde_json::Value,
        path: &str,
        session: &SessionContext,
        cancel: CancellationToken,
    ) {
        ToolExecutor::record_served_read_audit(self, tool_name, input, path, session, cancel).await;
    }
}

/// Convenience alias used by constructors that take the backend.
pub type SharedExecutionBackend = Arc<dyn ToolExecutionBackend>;
