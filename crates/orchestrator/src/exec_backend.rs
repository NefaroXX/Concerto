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

    /// Execute one tool call carrying the orchestrator-authority marker.
    ///
    /// The in-process single-agent loop calls this so its own executor calls
    /// evaluate under [`concerto_core::types::PolicyAction::orchestrator_authority`]
    /// (intent-derived restrictions skipped; deny-class, Consequential sinks,
    /// plan guards and audit rows kept). The default delegates to
    /// [`Self::execute`]: the supervised gate-proxy backend runs in a child
    /// process whose calls are gated by the supervisor write gate and MUST NOT
    /// acquire authority, so it keeps the fully intent-gated path unchanged.
    async fn execute_with_authority(
        &self,
        tool_name: &str,
        input: serde_json::Value,
        call_id: &str,
        session: &SessionContext,
        cancel: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        self.execute(tool_name, input, call_id, session, cancel).await
    }

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

    /// ADR-66: persist a capability-refusal audit row (`capability_gate`).
    ///
    /// The default is a warn-log no-op mirroring `record_served_read_audit`:
    /// the supervised path's audit trail is written supervisor-side
    /// (ADR-60 D4/D5) and never records rows through this seam.
    #[allow(clippy::too_many_arguments)]
    async fn record_capability_refusal(
        &self,
        _session_id: Ulid,
        _correlation_id: Ulid,
        provider: &str,
        model: &str,
        capability: &str,
        seam: &str,
        _cancel: CancellationToken,
    ) {
        tracing::warn!(
            provider,
            model,
            capability,
            seam,
            "capability refusal (supervised backend: audit row written supervisor-side)"
        );
    }

    /// Supremacy invariant: persist a Coordinator Decision row for a
    /// bypass/abort exit the single-agent loop would otherwise take silently
    /// (a `Blocked` outcome, a provider error, an iteration/continuation cap).
    ///
    /// `decision` is the stable machine code; `reason` is the human-readable
    /// detail. The default is a warn-log no-op, mirroring
    /// `record_capability_refusal`: the supervised path's audit trail is
    /// written supervisor-side (ADR-60 D4/D5) and never records rows through
    /// this seam. The in-process path delegates to the concrete executor.
    async fn record_coordinator_decision(
        &self,
        _session_id: Ulid,
        _correlation_id: Ulid,
        decision: &str,
        reason: &str,
        _cancel: CancellationToken,
    ) {
        tracing::warn!(
            decision,
            reason,
            "coordinator decision (supervised backend: audit row written supervisor-side)"
        );
    }

    /// ADR-66 §4: persist a text-fallback driver audit row (`tool_driver`).
    ///
    /// Default warn-log no-op, mirroring `record_capability_refusal`.
    #[allow(clippy::too_many_arguments)]
    async fn record_tool_driver_event(
        &self,
        _session_id: Ulid,
        _correlation_id: Ulid,
        provider: &str,
        model: &str,
        event: &str,
        verdict: &str,
        detail: &str,
        _cancel: CancellationToken,
    ) {
        tracing::warn!(
            provider,
            model,
            event,
            verdict,
            detail,
            "tool driver event (supervised backend: audit row written supervisor-side)"
        );
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

    async fn execute_with_authority(
        &self,
        tool_name: &str,
        input: serde_json::Value,
        _call_id: &str,
        session: &SessionContext,
        cancel: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        ToolExecutor::execute_with_authority(self, tool_name, input, session, cancel).await
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

    async fn record_capability_refusal(
        &self,
        session_id: Ulid,
        correlation_id: Ulid,
        provider: &str,
        model: &str,
        capability: &str,
        seam: &str,
        cancel: CancellationToken,
    ) {
        ToolExecutor::record_capability_refusal(
            self,
            session_id,
            correlation_id,
            provider,
            model,
            capability,
            seam,
            cancel,
        )
        .await;
    }

    async fn record_tool_driver_event(
        &self,
        session_id: Ulid,
        correlation_id: Ulid,
        provider: &str,
        model: &str,
        event: &str,
        verdict: &str,
        detail: &str,
        cancel: CancellationToken,
    ) {
        ToolExecutor::record_tool_driver_event(
            self,
            session_id,
            correlation_id,
            provider,
            model,
            event,
            verdict,
            detail,
            cancel,
        )
        .await;
    }

    async fn record_coordinator_decision(
        &self,
        session_id: Ulid,
        correlation_id: Ulid,
        decision: &str,
        reason: &str,
        cancel: CancellationToken,
    ) {
        ToolExecutor::record_coordinator_decision(
            self,
            session_id,
            correlation_id,
            decision,
            reason,
            cancel,
        )
        .await;
    }
}

/// Convenience alias used by constructors that take the backend.
pub type SharedExecutionBackend = Arc<dyn ToolExecutionBackend>;
