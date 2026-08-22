use crate::error::{PolicyError, ToolError};
use crate::event::{EventBus, EventKind};
use crate::traits::approval::ApprovalDecision;
use crate::traits::policy::{AuditEntry, PolicyEngine};
use crate::traits::tool::Tool;
use crate::traits::ApprovalSink;
use crate::types::{
    CapabilitySet, CommandPolicyFacts, PolicyAction, PolicyVerdict, SessionContext, ToolDefinition,
    ToolOutput, ToolRegistry,
};
use crate::CancellationToken;
use std::sync::Arc;
use std::time::{Duration, Instant};
use time::OffsetDateTime;

/// Orchestrates policy-gated tool execution.
///
/// Caller creates one `ToolExecutor` with a registry and a policy engine,
/// then calls `execute()` to run any registered tool through the policy
/// gate. No tool can bypass policy — the executor enforces this at the
/// single call site.
pub struct ToolExecutor {
    registry: Arc<ToolRegistry>,
    policy: Arc<dyn PolicyEngine>,
    approval_sink: Option<Arc<dyn ApprovalSink>>,
    event_bus: Option<EventBus>,
}

struct ExecutionAuditContext {
    correlation_id: crate::ids::Ulid,
    input_hash: String,
    facts: Option<CommandPolicyFacts>,
}

impl ToolExecutor {
    pub fn new(registry: Arc<ToolRegistry>, policy: Arc<dyn PolicyEngine>) -> Self {
        Self { registry, policy, approval_sink: None, event_bus: None }
    }

    pub fn with_approval_sink(mut self, sink: Arc<dyn ApprovalSink>) -> Self {
        self.approval_sink = Some(sink);
        self
    }

    /// Attach an event bus so the executor can publish approval lifecycle
    /// events (e.g. [`EventKind::ApprovalTimeout`]) to subscribers.
    pub fn with_event_bus(mut self, bus: EventBus) -> Self {
        self.event_bus = Some(bus);
        self
    }

    /// Return tool definitions for all registered tools, for passing to an LLM.
    pub fn tool_definitions(&self) -> Vec<ToolDefinition> {
        self.registry.all_tool_definitions()
    }

    /// Return tool definitions for the tools whose capability requirements are
    /// satisfied by `caps` (used to scope which tools a specialist may see).
    ///
    /// Tools that require capabilities the caller does not have are omitted,
    /// so an agent without the `filesystem` capability never sees the
    /// filesystem tool (coarse flag vocabulary shared with the tool
    /// implementations; read vs write enforcement is the policy engine's job).
    pub fn tool_definitions_for(&self, caps: &CapabilitySet) -> Vec<ToolDefinition> {
        self.registry
            .capability_filter(caps)
            .into_iter()
            .map(|tool| ToolDefinition {
                name: tool.name().to_string(),
                description: tool.description().to_string(),
                parameters: tool.input_schema(),
            })
            .collect()
    }

    /// True when the registry contains at least one tool that *requires* a
    /// capability satisfied by `caps`.
    ///
    /// Tools with empty requirements (LSP, MCP bridge) are offered to every
    /// agent but must not flip a capability-free agent into an auto tool
    /// choice: only genuinely capability-gated tools unlock `Auto`.
    pub fn has_capability_gated_tools(&self, caps: &CapabilitySet) -> bool {
        self.registry.has_capability_gated_tools(caps)
    }

    async fn execute_allowed(
        &self,
        tool: &dyn Tool,
        tool_name: &str,
        input: serde_json::Value,
        session: &SessionContext,
        cancel: CancellationToken,
        audit: ExecutionAuditContext,
    ) -> Result<ToolOutput, ToolError> {
        let started = Instant::now();
        let result = tool.execute(input, self.policy.as_ref(), session, cancel.clone()).await;

        // Record a post-execution completion entry for *every* executed tool, not
        // just command-executing ones: the log must show whether the tool ran,
        // its exit code, and its duration. The ADR-28 §6 shell fields are derived
        // from `command_facts` when present and stay `None` otherwise, so
        // non-shell tools get a minimal, truthful completion row.
        let ExecutionAuditContext { correlation_id, input_hash, facts } = audit;
        let exit_code = result
            .as_ref()
            .ok()
            .and_then(|output| output.data.get("exit_code"))
            .and_then(serde_json::Value::as_i64)
            .and_then(|code| i32::try_from(code).ok());
        let verdict = match (&result, exit_code) {
            (Ok(_), Some(0) | None) => "ExecutionSucceeded".to_owned(),
            (Ok(_), Some(code)) => format!("ExecutionFailed({code})"),
            (Err(error), _) => format!("ExecutionError({error})"),
        };
        let duration_ms = i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);
        let entry = AuditEntry {
            tool_name: tool_name.to_owned(),
            verdict,
            input_hash,
            session_id: session.session_id,
            correlation_id,
            timestamp: OffsetDateTime::now_utc(),
            user_response: None,
            rule_matched: None,
            profile_id: facts.as_ref().and_then(|f| f.shell_profile_id.clone()),
            resolved_executable: facts
                .as_ref()
                .and_then(|f| f.resolved_executable.as_ref())
                .and_then(|path| path.to_str().map(str::to_owned)),
            argv: facts.as_ref().map(|f| f.argv.clone()),
            working_directory: facts
                .as_ref()
                .and_then(|f| f.working_directory.as_ref())
                .and_then(|path| path.to_str().map(str::to_owned)),
            network_requested: facts.as_ref().map(|f| f.network_requested),
            filesystem_scope: facts.as_ref().map(|f| format!("{:?}", f.filesystem_scope)),
            destructive_classification: facts
                .as_ref()
                .map(|f| format!("{:?}", f.destructive_classification)),
            exit_code,
            duration_ms: Some(duration_ms),
            toolchain_version: None,
            plan_id: None,
            source_revision: None,
        };
        if let Err(error) = self.policy.audit_log().record(entry, cancel.clone()).await {
            tracing::error!(%error, "post-execution audit write failed");
        }

        result
    }

    /// Persist the user's approval decision (`Approve` / `ApproveAllForSession`
    /// / `Deny`) as a distinct audit entry.
    ///
    /// Mirrors `SimplePolicyEngine::record_decision` from policy.rs: the entry
    /// shares the `correlation_id` and `input_hash` of the `RequireApproval`
    /// verdict row that preceded it, uses `rule_matched = "user_approval"`, and
    /// carries the same structured command facts when present. Execution
    /// outcomes are intentionally left `None` — `execute_allowed` writes a
    /// separate completion row only when the call actually runs.
    async fn record_approval_decision(
        &self,
        action: &PolicyAction<'_>,
        decision: ApprovalDecision,
        cancel: CancellationToken,
    ) {
        let entry = AuditEntry {
            tool_name: action.tool_name.to_string(),
            verdict: match decision {
                ApprovalDecision::Approve => "Approved".to_owned(),
                ApprovalDecision::ApproveAllForSession => "ApprovedAllForSession".to_owned(),
                ApprovalDecision::Deny => "Denied".to_owned(),
            },
            input_hash: crate::policy::compute_input_hash(action.input),
            session_id: action.session_id,
            correlation_id: action.correlation_id,
            timestamp: OffsetDateTime::now_utc(),
            user_response: Some(match decision {
                ApprovalDecision::Approve => "user approved".to_owned(),
                ApprovalDecision::ApproveAllForSession => {
                    "user approved all for session".to_owned()
                }
                ApprovalDecision::Deny => "user denied".to_owned(),
            }),
            rule_matched: Some("user_approval".to_owned()),
            // ---- ADR-28 §6/§7: carry structured facts forward to the log ----
            profile_id: action.command_facts.as_ref().and_then(|f| f.shell_profile_id.clone()),
            resolved_executable: action
                .command_facts
                .as_ref()
                .and_then(|f| f.resolved_executable.as_ref())
                .and_then(|p| p.to_str())
                .map(str::to_string),
            argv: action.command_facts.as_ref().map(|f| f.argv.clone()),
            working_directory: action
                .command_facts
                .as_ref()
                .and_then(|f| f.working_directory.as_ref())
                .and_then(|p| p.to_str())
                .map(str::to_string),
            network_requested: action.command_facts.as_ref().map(|f| f.network_requested),
            filesystem_scope: action
                .command_facts
                .as_ref()
                .map(|f| format!("{:?}", f.filesystem_scope)),
            destructive_classification: action
                .command_facts
                .as_ref()
                .map(|f| format!("{:?}", f.destructive_classification)),
            exit_code: None,
            duration_ms: None,
            toolchain_version: None,
            plan_id: None,
            source_revision: None,
        };
        if let Err(error) = self.policy.audit_log().record(entry, cancel).await {
            tracing::error!(%error, "approval-decision audit write failed");
        }
    }

    /// Persist the outcome of a non-blocking `ApprovalSink::request_ack`
    /// warning as a distinct audit entry through the same channel as
    /// [`Self::record_approval_decision`].
    ///
    /// `ApprovalSink::request_ack` returns a bare `bool` (acknowledged →
    /// continue, or abort) and today leaves no audit trace (audit H-04). This
    /// is the backend-agnostic record seam for that path: the verdict
    /// vocabulary is extended with `RequestContinue` / `RequestAbort`, the
    /// warning text is preserved as `user_response`, and `rule_matched` is
    /// `"user_ack"`. Because an ack is not tied to any tool call, `tool_name`
    /// is the synthetic `"request_ack"`, `input_hash` is the empty string (no
    /// input exists), and the ADR-28 §6 execution fields stay `None`.
    ///
    /// Phase 0 ships the channel only: the live `request_ack` call site lives
    /// in the orchestrator (`setup_undo_stash`), and wiring it here is a later,
    /// explicitly additive phase. Until then nothing calls this method in
    /// production, so this is zero-behavioral.
    pub async fn record_ack_decision(
        &self,
        session_id: crate::ids::Ulid,
        correlation_id: crate::ids::Ulid,
        message: &str,
        acknowledged: bool,
        cancel: CancellationToken,
    ) {
        let entry = AuditEntry {
            tool_name: "request_ack".to_owned(),
            verdict: if acknowledged { "RequestContinue" } else { "RequestAbort" }.to_owned(),
            input_hash: String::new(),
            session_id,
            correlation_id,
            timestamp: OffsetDateTime::now_utc(),
            user_response: Some(message.to_owned()),
            rule_matched: Some("user_ack".to_owned()),
            profile_id: None,
            resolved_executable: None,
            argv: None,
            working_directory: None,
            network_requested: None,
            filesystem_scope: None,
            destructive_classification: None,
            exit_code: None,
            duration_ms: None,
            toolchain_version: None,
            plan_id: None,
            source_revision: None,
        };
        if let Err(error) = self.policy.audit_log().record(entry, cancel).await {
            tracing::error!(%error, "ack-decision audit write failed");
        }
    }

    /// Persist a deterministic routing decision (ADR-55 §6) as a distinct
    /// audit entry through the same channel as
    /// [`Self::record_ack_decision`].
    ///
    /// A routing decision is not tied to any tool call, so `tool_name` is the
    /// synthetic `"intent_router"` and `input_hash` is the empty string (no
    /// input exists). The winning rule name — one of the rule constants from
    /// `crate::intent` (`execute_keyword`, `verify_keyword`, ...), or the
    /// `"llm_classifier"` / `"ask_user"` path names — becomes `rule_matched`;
    /// `user_response` carries the outcome label (`Execute`, `Verify`, ...);
    /// and `verdict` records the user's confirmation for the decision
    /// (`granted` | `declined` | `timed_out` | `canceled` | `n/a` — `n/a` when
    /// no confirmation was solicited, e.g. an unanswered `AskUser`). The
    /// ADR-28 §6 execution fields stay `None`: there is no command behind a
    /// routing decision.
    ///
    /// Phase 0 ships the channel only: the fixed [`AuditEntry`] schema has no
    /// dedicated columns for the originating `utterance` or the router's
    /// `confidence`, so those are surfaced at `debug` level here and durable
    /// coverage lands with ADR-55 Phase 2 ("durable audit coverage for the new
    /// records"). Wiring the run loop call site is a later, explicitly
    /// additive batch; until then nothing calls this method in production, so
    /// this is zero-behavioral.
    #[allow(clippy::too_many_arguments)]
    pub async fn record_routing_decision(
        &self,
        session_id: crate::ids::Ulid,
        correlation_id: crate::ids::Ulid,
        utterance: &str,
        route: &str,
        outcome: &str,
        confidence: f32,
        confirmation: &str,
        cancel: CancellationToken,
    ) {
        let entry = AuditEntry {
            tool_name: "intent_router".to_owned(),
            verdict: confirmation.to_owned(),
            input_hash: String::new(),
            session_id,
            correlation_id,
            timestamp: OffsetDateTime::now_utc(),
            user_response: Some(outcome.to_owned()),
            rule_matched: Some(route.to_owned()),
            profile_id: None,
            resolved_executable: None,
            argv: None,
            working_directory: None,
            network_requested: None,
            filesystem_scope: None,
            destructive_classification: None,
            exit_code: None,
            duration_ms: None,
            toolchain_version: None,
            // Routing decisions stay JSON-only (ADR-55 §6): no plan is bound.
            plan_id: None,
            source_revision: None,
        };
        if let Err(error) = self.policy.audit_log().record(entry, cancel).await {
            tracing::error!(%error, "routing-decision audit write failed");
        } else {
            tracing::debug!(
                route,
                outcome,
                confirmation,
                %confidence,
                utterance,
                "routing decision recorded"
            );
        }
    }

    /// Persist a plan-approval decision (ADR-55 Phase 1d) as a distinct audit
    /// entry through the same channel as [`Self::record_routing_decision`].
    ///
    /// A plan decision is not tied to any tool call, so `tool_name` is the
    /// synthetic `"intent:plan"`. `rule_matched` and `verdict` both carry the
    /// user's decision label (`apply` | `replan` | `dismissed`), `input_hash`
    /// is the objective hash the binding is keyed on (so the audit row links
    /// back to the router's classification), and `user_response` is a compact
    /// JSON envelope with the bound `plan_id` and the source revision the plan
    /// was approved at. The same values are mirrored into the schema-derived
    /// `plan_id` / `source_revision` columns (ADR-55 Phase 1d §4) so the log is
    /// queryable without JSON parsing; the envelope is retained for
    /// replay/backward compatibility. The ADR-28 §6 execution fields stay
    /// `None`: there is no command behind a plan decision.
    #[allow(clippy::too_many_arguments)]
    pub async fn record_plan_decision(
        &self,
        session_id: crate::ids::Ulid,
        correlation_id: crate::ids::Ulid,
        plan_id: &str,
        objective_hash: &str,
        source_revision: Option<&str>,
        decision: &str,
        cancel: CancellationToken,
    ) {
        let entry = AuditEntry {
            tool_name: "intent:plan".to_owned(),
            verdict: decision.to_owned(),
            input_hash: objective_hash.to_owned(),
            session_id,
            correlation_id,
            timestamp: OffsetDateTime::now_utc(),
            user_response: Some(
                serde_json::json!({
                    "plan_id": plan_id,
                    "source_revision": source_revision,
                })
                .to_string(),
            ),
            rule_matched: Some(decision.to_owned()),
            profile_id: None,
            resolved_executable: None,
            argv: None,
            working_directory: None,
            network_requested: None,
            filesystem_scope: None,
            destructive_classification: None,
            exit_code: None,
            duration_ms: None,
            toolchain_version: None,
            plan_id: Some(plan_id.to_owned()),
            source_revision: source_revision.map(str::to_owned),
        };
        if let Err(error) = self.policy.audit_log().record(entry, cancel).await {
            tracing::error!(%error, "plan-decision audit write failed");
        } else {
            tracing::debug!(
                plan_id,
                objective_hash,
                decision,
                source_revision,
                "plan decision recorded"
            );
        }
    }

    /// Look up `tool_name`, evaluate policy, and — if allowed — execute.
    pub async fn execute(
        &self,
        tool_name: &str,
        input: serde_json::Value,
        session: &SessionContext,
        cancel: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let tool = self.registry.get(tool_name).ok_or_else(|| ToolError::ExecutionFailed {
            message: format!("tool not found: {tool_name}"),
        })?;

        let action = PolicyAction {
            tool_name,
            input: &input,
            session_id: session.session_id,
            correlation_id: crate::ids::new_id(),
            capability_requirements: tool.capability_requirements(),
            sandbox_profile: None,
            // Estimate cost based on tool type — provider calls get a default estimate,
            // other tools have zero estimated cost.
            estimated_cost_usd: if tool_name == "provider" {
                input.get("estimated_cost_usd").and_then(|v| v.as_f64()).or(Some(0.001))
            } else {
                None
            },
            // ADR-28 §6: pull structured command facts from the tool (e.g. the
            // shell tool resolves executable/argv/cwd), so the policy engine
            // and audit log reason about what actually runs, not just a string.
            command_facts: tool.command_facts(&input, session),
        };
        let correlation_id = action.correlation_id;
        let input_hash = crate::policy::compute_input_hash(&input);
        let command_facts = action.command_facts.clone();

        match self.policy.evaluate(&action, cancel.clone()).await {
            Ok(PolicyVerdict::Allow) => {
                self.execute_allowed(
                    tool,
                    tool_name,
                    input,
                    session,
                    cancel,
                    ExecutionAuditContext { correlation_id, input_hash, facts: command_facts },
                )
                .await
            }
            Ok(PolicyVerdict::Deny) => {
                Err(ToolError::PolicyDenied { rule: "policy_denied".into() })
            }
            Ok(
                PolicyVerdict::RequireApproval { timeout }
                | PolicyVerdict::RequireApprovalWithTimeout { timeout },
            ) => match &self.approval_sink {
                Some(sink) => {
                    match self
                        .request_approval_decision(sink.as_ref(), &action, timeout, cancel.clone())
                        .await
                    {
                        Ok(decision) => {
                            // Persist the user's decision as a distinct audit
                            // entry, correlated with the `RequireApproval` row
                            // the policy engine already wrote. The execution
                            // outcome row (if any) is written separately by
                            // `execute_allowed`, so there is no duplication.
                            self.record_approval_decision(&action, decision, cancel.clone()).await;
                            match decision {
                                ApprovalDecision::Approve
                                | ApprovalDecision::ApproveAllForSession => {
                                    self.execute_allowed(
                                        tool,
                                        tool_name,
                                        input,
                                        session,
                                        cancel,
                                        ExecutionAuditContext {
                                            correlation_id,
                                            input_hash,
                                            facts: command_facts,
                                        },
                                    )
                                    .await
                                }
                                ApprovalDecision::Deny => {
                                    Err(ToolError::PolicyDenied { rule: "user_denied".into() })
                                }
                            }
                        }
                        Err(error) => Err(error),
                    }
                }
                None => Err(ToolError::PolicyDenied { rule: "requires_approval_no_sink".into() }),
            },
            Err(PolicyError::AuditLogWriteFailed(msg)) => {
                Err(ToolError::PolicyDenied { rule: format!("audit_failure: {msg}") })
            }
            Err(PolicyError::RuleViolation(msg)) => {
                Err(ToolError::PolicyDenied { rule: format!("rule_violation: {msg}") })
            }
            Err(PolicyError::ApprovalTimeout) => {
                Err(ToolError::ExecutionFailed { message: "approval timed out".into() })
            }
            Err(PolicyError::Cancelled) => Err(ToolError::Cancelled),
            Err(PolicyError::InvalidRule(msg)) => {
                Err(ToolError::PolicyDenied { rule: format!("invalid_policy_rule: {msg}") })
            }
        }
    }

    /// Race the approval decision, cancellation, and the configured timeout
    /// at the requester.
    ///
    /// H-02 remediation: the timeout is enforced here, not by wrapping the
    /// sink — a silent sink must not hang the caller, and an approval granted
    /// after the deadline must never let the action execute. A timeout is an
    /// explicit deny-by-default state: it fails the action deterministically
    /// and publishes an [`EventKind::ApprovalTimeout`] so subscribers see the
    /// same outcome.
    async fn request_approval_decision(
        &self,
        sink: &dyn ApprovalSink,
        action: &PolicyAction<'_>,
        timeout: Duration,
        cancel: CancellationToken,
    ) -> Result<ApprovalDecision, ToolError> {
        tokio::select! {
            biased;
            decision = sink.request_approval(action, cancel.clone()) => Ok(decision),
            _ = cancel.cancelled() => Err(ToolError::Cancelled),
            _ = tokio::time::sleep(timeout) => {
                if let Some(bus) = &self.event_bus {
                    let _ = bus.publish_for_session(
                        action.session_id,
                        action.correlation_id,
                        EventKind::ApprovalTimeout {
                            tool_name: action.tool_name.to_string(),
                            timeout_secs: timeout.as_secs(),
                        },
                    );
                }
                tracing::warn!(
                    tool_name = %action.tool_name,
                    timeout_secs = timeout.as_secs(),
                    "approval timed out; action denied by default"
                );
                Err(ToolError::PolicyDenied { rule: "approval_timeout".into() })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::Ulid;
    use crate::policy::SimplePolicyEngine;
    use crate::traits::policy::AuditEntry;
    use crate::types::{CapabilitySet, Condition, DestructiveClass, FilesystemScope, PolicyRule};
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;
    struct AllowPolicy;
    struct DenyPolicy;

    #[async_trait]
    impl PolicyEngine for AllowPolicy {
        async fn evaluate(
            &self,
            _action: &PolicyAction<'_>,
            _cancel: CancellationToken,
        ) -> Result<PolicyVerdict, PolicyError> {
            Ok(PolicyVerdict::Allow)
        }

        fn audit_log(&self) -> &dyn crate::traits::policy::AuditLog {
            &IgnoreAudit
        }
    }

    #[async_trait]
    impl PolicyEngine for DenyPolicy {
        async fn evaluate(
            &self,
            _action: &PolicyAction<'_>,
            _cancel: CancellationToken,
        ) -> Result<PolicyVerdict, PolicyError> {
            Ok(PolicyVerdict::Deny)
        }

        fn audit_log(&self) -> &dyn crate::traits::policy::AuditLog {
            &IgnoreAudit
        }
    }

    struct IgnoreAudit;
    #[async_trait]
    impl crate::traits::policy::AuditLog for IgnoreAudit {
        async fn record(
            &self,
            _entry: AuditEntry,
            _cancel: CancellationToken,
        ) -> Result<(), PolicyError> {
            Ok(())
        }
    }

    use crate::traits::tool::Tool;

    struct EchoTool;

    #[async_trait]
    impl Tool for EchoTool {
        fn name(&self) -> &str {
            "echo"
        }
        fn description(&self) -> &str {
            "echoes input back"
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
            _policy: &dyn PolicyEngine,
            _session: &SessionContext,
            _cancel: CancellationToken,
        ) -> Result<ToolOutput, ToolError> {
            let data = input;
            Ok(ToolOutput { summary: serde_json::to_string(&data).unwrap_or_default(), data })
        }
    }

    struct FactTool;

    #[async_trait]
    impl Tool for FactTool {
        fn name(&self) -> &str {
            "fact"
        }

        fn description(&self) -> &str {
            "returns a command-like result"
        }

        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({})
        }

        fn capability_requirements(&self) -> CapabilitySet {
            CapabilitySet::default()
        }

        fn command_facts(
            &self,
            _input: &serde_json::Value,
            session: &SessionContext,
        ) -> Option<CommandPolicyFacts> {
            Some(CommandPolicyFacts {
                shell_profile_id: Some("test-profile".to_owned()),
                resolved_executable: Some("/bin/test".into()),
                argv: vec!["/bin/test".to_owned()],
                working_directory: Some(session.project_dir.clone()),
                network_requested: false,
                filesystem_scope: FilesystemScope::ProjectOnly,
                destructive_classification: DestructiveClass::NonDestructive,
            })
        }

        async fn execute(
            &self,
            _input: serde_json::Value,
            _policy: &dyn PolicyEngine,
            _session: &SessionContext,
            _cancel: CancellationToken,
        ) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput {
                summary: "completed".to_owned(),
                data: serde_json::json!({ "exit_code": 7 }),
            })
        }
    }

    /// Non-shell tool (no `command_facts`) whose execution always errors, used
    /// to prove failed tools still produce an `ExecutionError` audit row.
    struct FailingTool;

    #[async_trait]
    impl Tool for FailingTool {
        fn name(&self) -> &str {
            "failing"
        }
        fn description(&self) -> &str {
            "always fails"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({})
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
            Err(ToolError::ExecutionFailed { message: "boom".to_owned() })
        }
    }

    #[derive(Default)]
    struct RecordingAudit {
        entries: Mutex<Vec<AuditEntry>>,
    }

    struct RecordingPolicy {
        audit: Arc<RecordingAudit>,
    }

    #[async_trait]
    impl PolicyEngine for RecordingPolicy {
        async fn evaluate(
            &self,
            _action: &PolicyAction<'_>,
            _cancel: CancellationToken,
        ) -> Result<PolicyVerdict, PolicyError> {
            Ok(PolicyVerdict::Allow)
        }

        fn audit_log(&self) -> &dyn crate::traits::policy::AuditLog {
            self.audit.as_ref()
        }
    }

    #[async_trait]
    impl crate::traits::policy::AuditLog for RecordingAudit {
        async fn record(
            &self,
            _entry: AuditEntry,
            _cancel: CancellationToken,
        ) -> Result<(), PolicyError> {
            self.entries.lock().unwrap().push(_entry);
            Ok(())
        }
    }

    fn test_session() -> SessionContext {
        SessionContext::new(Ulid::new(), std::path::PathBuf::from("/tmp"))
    }

    fn test_registry() -> Arc<ToolRegistry> {
        let mut reg = ToolRegistry::default();
        reg.register(Box::new(EchoTool));
        Arc::new(reg)
    }

    #[tokio::test]
    async fn allow_executes_tool() {
        let executor = ToolExecutor::new(test_registry(), Arc::new(AllowPolicy));
        let output = executor
            .execute(
                "echo",
                serde_json::json!({"key": "value"}),
                &test_session(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(output.data, serde_json::json!({"key": "value"}));
    }

    #[tokio::test]
    async fn deny_returns_error() {
        let executor = ToolExecutor::new(test_registry(), Arc::new(DenyPolicy));
        let err = executor
            .execute("echo", serde_json::json!({}), &test_session(), CancellationToken::new())
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::PolicyDenied { .. }));
    }

    #[tokio::test]
    async fn unknown_tool_returns_not_found() {
        let executor = ToolExecutor::new(test_registry(), Arc::new(AllowPolicy));
        let err = executor
            .execute(
                "nonexistent",
                serde_json::json!({}),
                &test_session(),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::ExecutionFailed { .. }));
    }

    /// Tool that requires the `filesystem(read=true)` capability.
    struct ReadOnlyFsTool;

    #[async_trait]
    impl Tool for ReadOnlyFsTool {
        fn name(&self) -> &str {
            "read_file"
        }
        fn description(&self) -> &str {
            "reads a file from the project"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        fn capability_requirements(&self) -> CapabilitySet {
            CapabilitySet::default().with_requirement("filesystem(read=true)")
        }
        async fn execute(
            &self,
            _input: serde_json::Value,
            _policy: &dyn PolicyEngine,
            _session: &SessionContext,
            _cancel: CancellationToken,
        ) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput {
                summary: "read".to_owned(),
                data: serde_json::json!({ "path": "src/a.rs", "content": "fn main() {}" }),
            })
        }
    }

    #[test]
    fn tool_definitions_for_filters_by_capability() {
        let mut registry = ToolRegistry::default();
        // `echo` requires no capabilities; `read_file` requires fs read.
        registry.register(Box::new(EchoTool));
        registry.register(Box::new(ReadOnlyFsTool));
        let executor = ToolExecutor::new(Arc::new(registry), Arc::new(AllowPolicy));

        // Empty capabilities only expose tools with no requirements.
        let empty = executor.tool_definitions_for(&CapabilitySet::default());
        assert_eq!(empty.len(), 1, "expected only the capability-free tool");
        assert_eq!(empty[0].name, "echo");

        // Matching read capabilities expose both tools.
        let read_caps = CapabilitySet::default().with_requirement("filesystem(read=true)");
        let filtered = executor.tool_definitions_for(&read_caps);
        let mut names: Vec<&str> =
            filtered.iter().map(|definition| definition.name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, vec!["echo", "read_file"]);

        // Shell capabilities expose neither (the fs tool requires read).
        let shell_caps = CapabilitySet::default().with_requirement("shell");
        assert_eq!(executor.tool_definitions_for(&shell_caps).len(), 1);
    }

    #[tokio::test]
    async fn command_execution_appends_correlated_completion_facts() {
        let audit = Arc::new(RecordingAudit::default());
        let policy = Arc::new(RecordingPolicy { audit: audit.clone() });
        let mut registry = ToolRegistry::default();
        registry.register(Box::new(FactTool));
        let executor = ToolExecutor::new(Arc::new(registry), policy);

        let output = executor
            .execute("fact", serde_json::json!({}), &test_session(), CancellationToken::new())
            .await
            .expect("fact tool executes");

        assert_eq!(output.data["exit_code"], 7);
        let entries = audit.entries.lock().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].verdict, "ExecutionFailed(7)");
        assert_eq!(entries[0].profile_id.as_deref(), Some("test-profile"));
        assert_eq!(entries[0].exit_code, Some(7));
        assert!(entries[0].duration_ms.is_some());
    }

    /// Non-shell tools (no `command_facts`) must still produce a post-execution
    /// row: policy row first, then an `ExecutionSucceeded` row carrying the
    /// exit code and duration, with the shell-specific fields left `None`.
    #[tokio::test]
    async fn non_shell_tool_records_policy_and_execution_rows() {
        let audit = Arc::new(RecordingAudit::default());
        let policy = Arc::new(SimplePolicyEngine::new(
            vec![PolicyRule::AutoApprove(Condition::Always)],
            audit.clone(),
        ));
        let mut registry = ToolRegistry::default();
        registry.register(Box::new(EchoTool));
        let executor = ToolExecutor::new(Arc::new(registry), policy);

        // EchoTool is a plain non-shell tool; its output data carries exit_code
        // only because the caller-supplied input is echoed back verbatim.
        let output = executor
            .execute(
                "echo",
                serde_json::json!({"exit_code": 0}),
                &test_session(),
                CancellationToken::new(),
            )
            .await
            .expect("echo executes");

        assert_eq!(output.data, serde_json::json!({"exit_code": 0}));
        let entries = audit.entries.lock().unwrap();
        assert_eq!(entries.len(), 2, "expected policy row + post-execution row");
        assert_eq!(entries[0].verdict, "Allow");
        assert_eq!(entries[0].rule_matched.as_deref(), Some("auto_approve"));
        assert_eq!(entries[0].exit_code, None);
        assert_eq!(entries[0].duration_ms, None);
        assert_eq!(entries[1].verdict, "ExecutionSucceeded");
        assert_eq!(entries[1].exit_code, Some(0));
        assert!(entries[1].duration_ms.is_some(), "duration must be populated");
        // Shell-derived fields stay None for a tool without command_facts.
        assert_eq!(entries[1].profile_id, None);
        assert_eq!(entries[1].argv, None);
        assert_eq!(entries[1].network_requested, None);
        assert_eq!(entries[1].resolved_executable, None);
        // Both rows share correlation_id and input_hash.
        assert_eq!(entries[0].correlation_id, entries[1].correlation_id);
        assert_eq!(entries[0].input_hash, entries[1].input_hash);
    }

    /// A tool whose execution returns an error must produce an
    /// `ExecutionError` completion row (duration populated, no exit code).
    #[tokio::test]
    async fn failing_tool_records_execution_error_row() {
        let audit = Arc::new(RecordingAudit::default());
        let policy = Arc::new(RecordingPolicy { audit: audit.clone() });
        let mut registry = ToolRegistry::default();
        registry.register(Box::new(FailingTool));
        let executor = ToolExecutor::new(Arc::new(registry), policy);

        let err = executor
            .execute("failing", serde_json::json!({}), &test_session(), CancellationToken::new())
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::ExecutionFailed { .. }));

        let entries = audit.entries.lock().unwrap();
        assert_eq!(entries.len(), 1, "only the completion row is recorded");
        assert!(
            entries[0].verdict.starts_with("ExecutionError("),
            "unexpected verdict: {}",
            entries[0].verdict
        );
        assert_eq!(entries[0].exit_code, None);
        assert!(entries[0].duration_ms.is_some(), "duration must be populated");
        assert_eq!(entries[0].profile_id, None, "no facts for a non-shell tool");
    }

    // ------------------------------------------------------------------
    // H-02: approval timeouts enforced by the requester
    // ------------------------------------------------------------------

    /// Policy that returns `RequireApprovalWithTimeout` for every action.
    struct ApprovalPolicy {
        timeout: Duration,
    }

    impl ApprovalPolicy {
        fn with_timeout(secs: u64) -> Self {
            Self { timeout: Duration::from_secs(secs) }
        }
    }

    #[async_trait]
    impl PolicyEngine for ApprovalPolicy {
        async fn evaluate(
            &self,
            _action: &PolicyAction<'_>,
            _cancel: CancellationToken,
        ) -> Result<PolicyVerdict, PolicyError> {
            Ok(PolicyVerdict::RequireApprovalWithTimeout { timeout: self.timeout })
        }

        fn audit_log(&self) -> &dyn crate::traits::policy::AuditLog {
            &IgnoreAudit
        }
    }

    /// Approval sink whose decision is delivered through a oneshot channel,
    /// so tests control exactly when — and whether — the "user" responds.
    struct ControlledApprovalSink {
        pending: Mutex<Option<tokio::sync::oneshot::Sender<ApprovalDecision>>>,
        requests: AtomicUsize,
    }

    impl ControlledApprovalSink {
        fn new() -> Self {
            Self { pending: Mutex::new(None), requests: AtomicUsize::new(0) }
        }

        fn resolve(&self, decision: ApprovalDecision) {
            if let Some(sender) = self.pending.lock().unwrap_or_else(|e| e.into_inner()).take() {
                let _ = sender.send(decision);
            }
        }
    }

    #[async_trait]
    impl ApprovalSink for ControlledApprovalSink {
        async fn request_approval(
            &self,
            _action: &PolicyAction<'_>,
            _cancel: CancellationToken,
        ) -> ApprovalDecision {
            self.requests.fetch_add(1, Ordering::SeqCst);
            let (sender, receiver) = tokio::sync::oneshot::channel();
            *self.pending.lock().unwrap_or_else(|e| e.into_inner()) = Some(sender);
            receiver.await.unwrap_or(ApprovalDecision::Deny)
        }

        async fn approve_all_for_session(&self, _session_id: Ulid, _cancel: CancellationToken) {}

        async fn request_ack(&self, _message: &str, _cancel: CancellationToken) -> bool {
            true
        }
    }

    /// Tool that records how many times it actually executed, so tests can
    /// prove a timed-out approval never reaches execution.
    struct CallCountingTool {
        calls: Arc<AtomicUsize>,
    }

    impl CallCountingTool {
        fn new(calls: Arc<AtomicUsize>) -> Self {
            Self { calls }
        }
    }

    #[async_trait]
    impl Tool for CallCountingTool {
        fn name(&self) -> &str {
            "echo"
        }
        fn description(&self) -> &str {
            "counts executions"
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
            _policy: &dyn PolicyEngine,
            _session: &SessionContext,
            _cancel: CancellationToken,
        ) -> Result<ToolOutput, ToolError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(ToolOutput { summary: "executed".to_owned(), data: input })
        }
    }

    /// Deterministically (under paused time) wait until the executor has
    /// polled the sink and registered the pending approval request. This
    /// guarantees both the sink branch and the timeout timer are armed before
    /// the test advances the clock.
    async fn wait_for_approval_request(sink: &ControlledApprovalSink) {
        for _ in 0..1_000 {
            if sink.requests.load(Ordering::SeqCst) > 0 {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("approval request never reached the sink");
    }

    #[tokio::test(start_paused = true)]
    async fn approval_timeout_denies_without_executing_and_emits_event() {
        let bus = EventBus::default();
        let mut rx = bus.subscribe();
        let calls = Arc::new(AtomicUsize::new(0));
        let mut registry = ToolRegistry::default();
        registry.register(Box::new(CallCountingTool::new(calls.clone())));
        let sink = Arc::new(ControlledApprovalSink::new());
        let executor =
            ToolExecutor::new(Arc::new(registry), Arc::new(ApprovalPolicy::with_timeout(10)))
                .with_approval_sink(sink.clone())
                .with_event_bus(bus.clone());
        let session = test_session();

        let handle = tokio::spawn(async move {
            executor
                .execute(
                    "echo",
                    serde_json::json!({"key": "value"}),
                    &session,
                    CancellationToken::new(),
                )
                .await
        });
        wait_for_approval_request(&sink).await;
        tokio::time::advance(Duration::from_secs(10)).await;

        let error = handle
            .await
            .expect("executor task must not panic")
            .expect_err("approval timeout must fail the action");
        assert!(
            matches!(&error, ToolError::PolicyDenied { rule } if rule == "approval_timeout"),
            "expected approval_timeout denial, got {error:?}"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0, "tool must never execute after timeout");

        // The timeout event is published before the executor returns.
        let event = rx.recv().await.expect("timeout event must be emitted");
        match &event.kind {
            EventKind::ApprovalTimeout { tool_name, timeout_secs } => {
                assert_eq!(tool_name, "echo");
                assert_eq!(*timeout_secs, 10);
            }
            other => panic!("expected ApprovalTimeout event, got {other:?}"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn approval_before_timeout_executes_normally() {
        let sink = Arc::new(ControlledApprovalSink::new());
        let executor =
            ToolExecutor::new(test_registry(), Arc::new(ApprovalPolicy::with_timeout(30)))
                .with_approval_sink(sink.clone());
        let session = test_session();

        let handle = tokio::spawn(async move {
            executor
                .execute(
                    "echo",
                    serde_json::json!({"key": "value"}),
                    &session,
                    CancellationToken::new(),
                )
                .await
        });
        wait_for_approval_request(&sink).await;
        sink.resolve(ApprovalDecision::Approve);

        let output = handle
            .await
            .expect("executor task must not panic")
            .expect("approval before the deadline must succeed");
        assert_eq!(output.data, serde_json::json!({"key": "value"}));
    }

    #[tokio::test(start_paused = true)]
    async fn cancellation_aborts_pending_approval_cleanly() {
        let sink = Arc::new(ControlledApprovalSink::new());
        let executor =
            ToolExecutor::new(test_registry(), Arc::new(ApprovalPolicy::with_timeout(60)))
                .with_approval_sink(sink.clone());
        let session = test_session();
        let cancel = CancellationToken::new();

        let handle = tokio::spawn({
            let cancel = cancel.clone();
            async move { executor.execute("echo", serde_json::json!({}), &session, cancel).await }
        });
        wait_for_approval_request(&sink).await;
        cancel.cancel();

        let error = handle
            .await
            .expect("executor task must not panic")
            .expect_err("cancellation must abort the pending approval");
        assert!(matches!(error, ToolError::Cancelled), "expected cancellation, got {error:?}");
    }

    // ------------------------------------------------------------------
    // Audit-log completeness: approval decisions are recorded, and every
    // approved call still gets its execution-outcome row.
    // ------------------------------------------------------------------

    /// Policy engine that requires approval for every action *and* records the
    /// `RequireApproval` verdict row, mirroring `SimplePolicyEngine` (the real
    /// policy engine writes the verdict row via `record_decision`).
    struct RecordingApprovalPolicy {
        audit: Arc<RecordingAudit>,
    }

    #[async_trait]
    impl PolicyEngine for RecordingApprovalPolicy {
        async fn evaluate(
            &self,
            action: &PolicyAction<'_>,
            _cancel: CancellationToken,
        ) -> Result<PolicyVerdict, PolicyError> {
            let entry = AuditEntry {
                tool_name: action.tool_name.to_string(),
                verdict: "RequireApproval".to_owned(),
                input_hash: crate::policy::compute_input_hash(action.input),
                session_id: action.session_id,
                correlation_id: action.correlation_id,
                timestamp: time::OffsetDateTime::now_utc(),
                user_response: None,
                rule_matched: Some("require_approval".to_owned()),
                profile_id: None,
                resolved_executable: None,
                argv: None,
                working_directory: None,
                network_requested: None,
                filesystem_scope: None,
                destructive_classification: None,
                exit_code: None,
                duration_ms: None,
                toolchain_version: None,
                plan_id: None,
                source_revision: None,
            };
            self.audit.entries.lock().unwrap().push(entry);
            Ok(PolicyVerdict::RequireApproval { timeout: Duration::from_secs(30) })
        }

        fn audit_log(&self) -> &dyn crate::traits::policy::AuditLog {
            self.audit.as_ref()
        }
    }

    #[tokio::test]
    async fn approval_approved_records_decision_and_execution_rows() {
        let audit = Arc::new(RecordingAudit::default());
        let policy = Arc::new(RecordingApprovalPolicy { audit: audit.clone() });
        let mut registry = ToolRegistry::default();
        registry.register(Box::new(EchoTool));
        let sink = Arc::new(ControlledApprovalSink::new());
        let executor =
            ToolExecutor::new(Arc::new(registry), policy).with_approval_sink(sink.clone());
        let session = test_session();

        let handle = tokio::spawn(async move {
            executor
                .execute(
                    "echo",
                    serde_json::json!({"exit_code": 0}),
                    &session,
                    CancellationToken::new(),
                )
                .await
        });
        wait_for_approval_request(&sink).await;
        sink.resolve(ApprovalDecision::Approve);

        let output =
            handle.await.expect("executor task must not panic").expect("approval must succeed");
        assert_eq!(output.data, serde_json::json!({"exit_code": 0}));

        let entries = audit.entries.lock().unwrap();
        assert_eq!(
            entries.len(),
            3,
            "expected RequireApproval + Approved + ExecutionSucceeded rows"
        );
        assert_eq!(entries[0].verdict, "RequireApproval");
        assert_eq!(entries[0].rule_matched.as_deref(), Some("require_approval"));
        // Decision row: verdict, rule, human-readable user response, no outcome.
        assert_eq!(entries[1].verdict, "Approved");
        assert_eq!(entries[1].rule_matched.as_deref(), Some("user_approval"));
        assert_eq!(entries[1].user_response.as_deref(), Some("user approved"));
        assert_eq!(entries[1].exit_code, None);
        assert_eq!(entries[1].duration_ms, None);
        // Execution row still comes from execute_allowed.
        assert_eq!(entries[2].verdict, "ExecutionSucceeded");
        assert_eq!(entries[2].exit_code, Some(0));
        assert!(entries[2].duration_ms.is_some());
        // All three rows share correlation_id and input_hash.
        let (correlation_id, input_hash) =
            (entries[0].correlation_id, entries[0].input_hash.clone());
        for entry in &entries[1..] {
            assert_eq!(entry.correlation_id, correlation_id);
            assert_eq!(entry.input_hash, input_hash);
        }
    }

    #[tokio::test]
    async fn approval_approved_all_records_decision_and_execution_rows() {
        let audit = Arc::new(RecordingAudit::default());
        let policy = Arc::new(RecordingApprovalPolicy { audit: audit.clone() });
        let mut registry = ToolRegistry::default();
        registry.register(Box::new(EchoTool));
        let sink = Arc::new(ControlledApprovalSink::new());
        let executor =
            ToolExecutor::new(Arc::new(registry), policy).with_approval_sink(sink.clone());
        let session = test_session();

        let handle = tokio::spawn(async move {
            executor
                .execute("echo", serde_json::json!({}), &session, CancellationToken::new())
                .await
        });
        wait_for_approval_request(&sink).await;
        sink.resolve(ApprovalDecision::ApproveAllForSession);

        handle.await.expect("executor task must not panic").expect("approve-all must succeed");

        let entries = audit.entries.lock().unwrap();
        assert_eq!(
            entries.len(),
            3,
            "expected RequireApproval + ApprovedAllForSession + ExecutionSucceeded rows"
        );
        assert_eq!(entries[0].verdict, "RequireApproval");
        assert_eq!(entries[1].verdict, "ApprovedAllForSession");
        assert_eq!(entries[1].rule_matched.as_deref(), Some("user_approval"));
        assert_eq!(entries[1].user_response.as_deref(), Some("user approved all for session"));
        assert_eq!(entries[2].verdict, "ExecutionSucceeded");
    }

    #[tokio::test]
    async fn approval_denied_records_decision_row_without_execution() {
        let audit = Arc::new(RecordingAudit::default());
        let policy = Arc::new(RecordingApprovalPolicy { audit: audit.clone() });
        let calls = Arc::new(AtomicUsize::new(0));
        let mut registry = ToolRegistry::default();
        registry.register(Box::new(CallCountingTool::new(calls.clone())));
        let sink = Arc::new(ControlledApprovalSink::new());
        let executor =
            ToolExecutor::new(Arc::new(registry), policy).with_approval_sink(sink.clone());
        let session = test_session();

        let handle = tokio::spawn(async move {
            executor
                .execute("echo", serde_json::json!({}), &session, CancellationToken::new())
                .await
        });
        wait_for_approval_request(&sink).await;
        sink.resolve(ApprovalDecision::Deny);

        let error = handle
            .await
            .expect("executor task must not panic")
            .expect_err("denied approval must fail the action");
        assert!(
            matches!(&error, ToolError::PolicyDenied { rule } if rule == "user_denied"),
            "expected user_denied denial, got {error:?}"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0, "tool must never execute after a deny");

        let entries = audit.entries.lock().unwrap();
        assert_eq!(entries.len(), 2, "expected RequireApproval + Denied rows, no execution row");
        assert_eq!(entries[0].verdict, "RequireApproval");
        assert_eq!(entries[1].verdict, "Denied");
        assert_eq!(entries[1].rule_matched.as_deref(), Some("user_approval"));
        assert_eq!(entries[1].user_response.as_deref(), Some("user denied"));
        assert_eq!(entries[1].exit_code, None);
        assert_eq!(entries[1].duration_ms, None);
        assert_eq!(entries[0].correlation_id, entries[1].correlation_id);
        assert_eq!(entries[0].input_hash, entries[1].input_hash);
    }

    // ------------------------------------------------------------------
    // Audit-log completeness: request_ack outcomes are recorded through the
    // same channel as approval decisions (audit H-04; ADR-55 Phase 0).
    // ------------------------------------------------------------------

    /// `request_ack` returning `true` (the user acknowledged and wants to
    /// continue) records a `RequestContinue` row carrying the warning text.
    #[tokio::test]
    async fn ack_continue_records_request_continue_row() {
        let audit = Arc::new(RecordingAudit::default());
        let policy = Arc::new(RecordingPolicy { audit: audit.clone() });
        let executor = ToolExecutor::new(test_registry(), policy);
        let session = test_session();
        let correlation_id = Ulid::new();

        executor
            .record_ack_decision(
                session.session_id,
                correlation_id,
                "This project is not a git repository — continue anyway?",
                true,
                CancellationToken::new(),
            )
            .await;

        let entries = audit.entries.lock().unwrap();
        assert_eq!(entries.len(), 1, "exactly one ack row is recorded");
        assert_eq!(entries[0].verdict, "RequestContinue");
        assert_eq!(entries[0].rule_matched.as_deref(), Some("user_ack"));
        assert_eq!(
            entries[0].user_response.as_deref(),
            Some("This project is not a git repository — continue anyway?")
        );
        assert_eq!(entries[0].tool_name, "request_ack");
        assert_eq!(entries[0].session_id, session.session_id);
        assert_eq!(entries[0].correlation_id, correlation_id);
        // An ack has no input, no command facts, no execution outcome.
        assert_eq!(entries[0].input_hash, "");
        assert_eq!(entries[0].profile_id, None);
        assert_eq!(entries[0].argv, None);
        assert_eq!(entries[0].network_requested, None);
        assert_eq!(entries[0].exit_code, None);
        assert_eq!(entries[0].duration_ms, None);
    }

    /// `request_ack` returning `false` (the user aborts the task) records a
    /// `RequestAbort` row so the abort leaves an audit trace.
    #[tokio::test]
    async fn ack_abort_records_request_abort_row() {
        let audit = Arc::new(RecordingAudit::default());
        let policy = Arc::new(RecordingPolicy { audit: audit.clone() });
        let executor = ToolExecutor::new(test_registry(), policy);
        let session = test_session();
        let correlation_id = Ulid::new();

        executor
            .record_ack_decision(
                session.session_id,
                correlation_id,
                "Proceed with destructive operation?",
                false,
                CancellationToken::new(),
            )
            .await;

        let entries = audit.entries.lock().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].verdict, "RequestAbort");
        assert_eq!(entries[0].rule_matched.as_deref(), Some("user_ack"));
        assert_eq!(
            entries[0].user_response.as_deref(),
            Some("Proceed with destructive operation?")
        );
        // Same synthetic tool identity as the continue path.
        assert_eq!(entries[0].tool_name, "request_ack");
        assert_eq!(entries[0].session_id, session.session_id);
        assert_eq!(entries[0].correlation_id, correlation_id);
    }

    /// A deterministic routing decision records a single row under the
    /// synthetic `intent_router` tool identity with the route rule in
    /// `rule_matched`, the outcome label in `user_response`, and the
    /// confirmation in `verdict` (`n/a` when no confirmation was solicited).
    #[tokio::test]
    async fn routing_decision_records_rule_row() {
        let audit = Arc::new(RecordingAudit::default());
        let policy = Arc::new(RecordingPolicy { audit: audit.clone() });
        let executor = ToolExecutor::new(test_registry(), policy);
        let session = test_session();
        let correlation_id = Ulid::new();

        executor
            .record_routing_decision(
                session.session_id,
                correlation_id,
                "add a retry to the uploader",
                "execute_keyword",
                "Execute",
                0.8,
                "n/a",
                CancellationToken::new(),
            )
            .await;

        let entries = audit.entries.lock().unwrap();
        assert_eq!(entries.len(), 1, "exactly one routing row is recorded");
        assert_eq!(entries[0].tool_name, "intent_router");
        assert_eq!(entries[0].verdict, "n/a");
        assert_eq!(entries[0].rule_matched.as_deref(), Some("execute_keyword"));
        assert_eq!(entries[0].user_response.as_deref(), Some("Execute"));
        assert_eq!(entries[0].session_id, session.session_id);
        assert_eq!(entries[0].correlation_id, correlation_id);
        // A routing decision has no input, no command facts, no execution.
        assert_eq!(entries[0].input_hash, "");
        assert_eq!(entries[0].profile_id, None);
        assert_eq!(entries[0].argv, None);
        assert_eq!(entries[0].network_requested, None);
        assert_eq!(entries[0].exit_code, None);
        assert_eq!(entries[0].duration_ms, None);
    }

    /// A classifier route that solicited a confirmation carries the user's
    /// answer in `verdict`, so a declined execution leaves a deny-side trace.
    #[tokio::test]
    async fn routing_decision_records_declined_confirmation() {
        let audit = Arc::new(RecordingAudit::default());
        let policy = Arc::new(RecordingPolicy { audit: audit.clone() });
        let executor = ToolExecutor::new(test_registry(), policy);
        let session = test_session();
        let correlation_id = Ulid::new();

        executor
            .record_routing_decision(
                session.session_id,
                correlation_id,
                "move the repo to a mono crate layout",
                "ask_user",
                "Execute",
                0.6,
                "declined",
                CancellationToken::new(),
            )
            .await;

        let entries = audit.entries.lock().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].tool_name, "intent_router");
        assert_eq!(entries[0].verdict, "declined");
        assert_eq!(entries[0].rule_matched.as_deref(), Some("ask_user"));
        assert_eq!(entries[0].user_response.as_deref(), Some("Execute"));
        assert_eq!(entries[0].session_id, session.session_id);
        assert_eq!(entries[0].correlation_id, correlation_id);
    }

    /// A plan-approval decision records a single row under the synthetic
    /// `intent:plan` tool identity. The user's answer is duplicated in both
    /// `rule_matched` and `verdict`, the objective hash lands in `input_hash`
    /// (linking the row back to the router's classification), and `user_response`
    /// carries the compact envelope with `plan_id` and `source_revision`. The
    /// ADR-28 §6 execution fields stay unset because there is no command behind
    /// a plan decision.
    #[tokio::test]
    async fn plan_decision_records_plan_row() {
        let audit = Arc::new(RecordingAudit::default());
        let policy = Arc::new(RecordingPolicy { audit: audit.clone() });
        let executor = ToolExecutor::new(test_registry(), policy);
        let session = test_session();
        let correlation_id = Ulid::new();

        executor
            .record_plan_decision(
                session.session_id,
                correlation_id,
                "01J4V6Q8X000000000000000001",
                "0123456789abcdef0123456789abcdef",
                Some("f00dcafe"),
                "apply",
                CancellationToken::new(),
            )
            .await;

        let entries = audit.entries.lock().unwrap();
        assert_eq!(entries.len(), 1, "exactly one plan row is recorded");
        assert_eq!(entries[0].tool_name, "intent:plan");
        assert_eq!(entries[0].verdict, "apply");
        assert_eq!(entries[0].rule_matched.as_deref(), Some("apply"));
        assert_eq!(
            entries[0].input_hash, "0123456789abcdef0123456789abcdef",
            "the objective hash is carried in input_hash for cross-linking"
        );
        let response: serde_json::Value =
            serde_json::from_str(entries[0].user_response.as_deref().unwrap()).unwrap();
        assert_eq!(
            response["plan_id"], "01J4V6Q8X000000000000000001",
            "user_response envelope carries the bound plan id"
        );
        assert_eq!(response["source_revision"], "f00dcafe");
        // ADR-55 Phase 1d §4: the values are mirrored into schema-derived
        // columns so the log is queryable without JSON parsing.
        assert_eq!(entries[0].plan_id.as_deref(), Some("01J4V6Q8X000000000000000001"));
        assert_eq!(entries[0].source_revision.as_deref(), Some("f00dcafe"));
        assert_eq!(entries[0].session_id, session.session_id);
        assert_eq!(entries[0].correlation_id, correlation_id);
        // No command facts, no execution behind a plan decision.
        assert_eq!(entries[0].profile_id, None);
        assert_eq!(entries[0].argv, None);
        assert_eq!(entries[0].network_requested, None);
        assert_eq!(entries[0].exit_code, None);
        assert_eq!(entries[0].duration_ms, None);
    }

    /// A dismissed plan dialog (no decision produced) records the `dismissed`
    /// label so the audit trail shows the run stayed read-only by default.
    #[tokio::test]
    async fn plan_decision_records_dismissed_row() {
        let audit = Arc::new(RecordingAudit::default());
        let policy = Arc::new(RecordingPolicy { audit: audit.clone() });
        let executor = ToolExecutor::new(test_registry(), policy);
        let session = test_session();
        let correlation_id = Ulid::new();

        executor
            .record_plan_decision(
                session.session_id,
                correlation_id,
                "01J4V6Q8X000000000000000002",
                "fedcba9876543210fedcba9876543210",
                None,
                "dismissed",
                CancellationToken::new(),
            )
            .await;

        let entries = audit.entries.lock().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].tool_name, "intent:plan");
        assert_eq!(entries[0].verdict, "dismissed");
        assert_eq!(entries[0].rule_matched.as_deref(), Some("dismissed"));
        let response: serde_json::Value =
            serde_json::from_str(entries[0].user_response.as_deref().unwrap()).unwrap();
        assert_eq!(response["plan_id"], "01J4V6Q8X000000000000000002");
        assert_eq!(response["source_revision"], serde_json::Value::Null);
        // The schema-derived columns mirror the envelope: plan_id is always
        // present, source_revision only when a revision was captured.
        assert_eq!(entries[0].plan_id.as_deref(), Some("01J4V6Q8X000000000000000002"));
        assert_eq!(entries[0].source_revision, None);
    }
}
