use crate::checkpoint::{
    versioned_fs_targets, CheckpointStore, MutationCaptureSink, OwnershipOracle, PreImageBytes,
    RestoreError, RestoreOutcome, StashRestorer,
};
use crate::error::{PolicyError, ToolError};
use crate::event::{EventBus, EventKind};
use crate::traits::approval::ApprovalDecision;
use crate::traits::policy::{AuditEntry, PolicyEngine};
use crate::traits::tool::Tool;
use crate::traits::ApprovalSink;
use crate::types::{
    CapabilitySet, CommandPolicyFacts, PathPolicyFacts, PolicyAction, PolicyVerdict,
    ReadResultFacts, SessionContext, ToolDefinition, ToolOutput, ToolRegistry,
};
use crate::CancellationToken;
use std::sync::Arc;
use std::time::Instant;
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
    /// ADR-82 slice 5 (b): pre-execution capture sink for mutating canonical
    /// filesystem ops on the direct (ungated) path. Banked bytes backfill the
    /// sidecar for active checkpoints covering the path (`INSERT OR IGNORE` —
    /// a checkpoint-creation snapshot always wins). `None` disables the seam.
    mutation_sink: Option<Arc<dyn MutationCaptureSink>>,
}

struct ExecutionAuditContext {
    correlation_id: crate::ids::Ulid,
    input_hash: String,
    facts: Option<CommandPolicyFacts>,
    path_facts: Option<PathPolicyFacts>,
    /// Canonical policy-view tool name (ADR-82 slice 1): the accounting
    /// identity of the call, recorded alongside the registered name.
    canonical_tool: Option<String>,
    /// Canonical operation from the policy-view input; `None` when the
    /// canonical input names no operation.
    canonical_operation: Option<String>,
}

/// The canonical **policy-view effect** of one tool call (ADR-82 slice 1).
///
/// The executor owns the registry, so it is the single producer of this
/// grammar: the identity is derived from the same [`Tool::policy_view`] the
/// policy engine evaluates, never from the registered name alone. Thin alias
/// tools (e.g. a `write` alias delegating to `filesystem`) therefore resolve
/// to their canonical effect — `("filesystem", "write")` — and every
/// accounting seam (write gate, fact writer, audit rows) keys on that pair
/// instead of the registered name, which closed the alias-write accounting
/// escapes.
///
/// **Canonicalization is accounting/attribution only — never a reroute.** The
/// implementation that executes stays the one registered under the requested
/// name; the canonical effect only determines how that execution is accounted,
/// versioned, and attributed. Callers that must also record the requested
/// identity keep both: requested name for attribution/forensics, canonical
/// effect for accounting.
///
/// Layering (ADR-82): this is a core vocabulary type — it names only
/// core-and-below types.
#[derive(Debug, Clone, PartialEq)]
pub struct CanonicalEffect {
    /// The canonical tool name policy evaluates (`policy_view`'s first
    /// element): `filesystem` for the `write` alias, the registered name for
    /// every non-alias tool.
    pub policy_name: String,
    /// The canonical policy-view **input** (`policy_view`'s second element) —
    /// for an alias write, the caller's raw input with `operation` forced to
    /// `"write"`. The write gate adopts this as the request's policy input so
    /// its versioned targets, pre-image capture and conflict check all key on
    /// the canonical operation; it is never the input the registered
    /// implementation executes with (execution keeps the registered name and
    /// input — see [`Self`]).
    pub policy_input: serde_json::Value,
    /// The canonical operation, read from the policy-view input's
    /// `operation` field (`"write"` for an alias write whose raw input named
    /// none). `None` when the canonical input carries no `operation` field.
    pub operation: Option<String>,
}

/// Derive the canonical policy-view effect of a call on `tool` (ADR-82
/// slice 1) — the shared derivation behind [`ToolExecutor::canonical_effect`]
/// and the audit contexts the executing paths already build: the identity is
/// always the one [`Tool::policy_view`] produces, so the service and the
/// audit rows can never disagree.
fn canonical_effect_of(tool: &dyn Tool, input: &serde_json::Value) -> CanonicalEffect {
    let (policy_name, policy_input) = tool.policy_view(input);
    let operation =
        policy_input.get("operation").and_then(serde_json::Value::as_str).map(str::to_owned);
    CanonicalEffect { policy_name, policy_input, operation }
}

/// Construct the policy action for a tool call, shared by the executing path
/// and the ADR-65 F1a serve gate so both evaluate the *exact* same action.
fn build_action<'a>(
    tool_name: &'a str,
    input: &'a serde_json::Value,
    tool: &dyn Tool,
    session: &SessionContext,
    orchestrator_authority: bool,
) -> PolicyAction<'a> {
    PolicyAction {
        tool_name,
        input,
        session_id: session.session_id,
        correlation_id: crate::ids::new_id(),
        capability_requirements: tool.capability_requirements(),
        sandbox_profile: tool.sandbox_profile(),
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
        command_facts: tool.command_facts(input, session),
        // Path-shaped counterpart: filesystem/git/LSP tools resolve the
        // operation and target path (attempted + confined) so the audit trail
        // can attribute the action to a concrete target.
        path_facts: tool.path_facts(input, session),
        // Orchestrator-authority marker: only the authority execute path sets
        // this; every ordinary (specialist / gate / bridge) path passes false.
        orchestrator_authority,
    }
}

/// Honest limitation for a path the checkpoint anchors but the sidecar cannot
/// return bytes for: prefer the record's own exclusion reason (it names the
/// real ceiling), else the generic hash-only note.
fn missing_bytes_limitation(record: &crate::checkpoint::CheckpointRecord, path: &str) -> String {
    if let Some(exclusion) = record.excluded.iter().find(|e| e.path.as_deref() == Some(path)) {
        return format!("{path}: {}", exclusion.reason);
    }
    format!("{path}: no restorable bytes banked; hash-only (drift-detectable, not reconstructible)")
}

/// One restore-path ownership check against the oracle (the gate's typed
/// read surface): `None` when the path is unowned or held by the restoring
/// agent (the write may proceed); `Some` names the foreign holder — surfaced
/// as [`RestoreError::OwnershipConflict`] without parsing any denial prose
/// (ADR-82). Shared by the Phase-1 pre-check and the Phase-2 per-write
/// re-check so both emit the identical structured conflict.
fn ownership_conflict(
    ownership: &dyn OwnershipOracle,
    path: &str,
    by_agent: &str,
) -> Option<crate::checkpoint::OwnershipConflictInfo> {
    let owner = ownership.owner_of(path)?;
    if owner == by_agent {
        return None;
    }
    Some(crate::checkpoint::OwnershipConflictInfo { path: path.to_owned(), owner })
}

/// Derive the audit-safe result summary of a read-only tool call from the
/// result the executor already holds.
///
/// `operation` is the path-facts operation recorded alongside the attempt:
/// `exists` / `list` / `read` are the filesystem read-only vocabulary, and no
/// other producer emits them (git records subcommands, LSP records method
/// names). Every value comes from `output.data`, produced by the execution
/// that just finished — the boolean is already computed, the entry count is
/// an array length, the size is a string length — so this derivation is
/// O(1) and performs **no** additional I/O: it never re-reads the file and
/// never adds a syscall to the hot path.
///
/// Returns `None` when the operation is not read-only, or when the expected
/// field is absent or of the wrong type (e.g. a future tool reusing one of
/// these operation names with a different result shape): an unknown result
/// is recorded as "nothing learned", never guessed.
fn read_result_facts(operation: &str, output: &ToolOutput) -> Option<ReadResultFacts> {
    match operation {
        "exists" => output
            .data
            .get("exists")
            .and_then(serde_json::Value::as_bool)
            .map(ReadResultFacts::Exists),
        "list" => output.data.get("entries").and_then(serde_json::Value::as_array).map(|entries| {
            ReadResultFacts::Entries(i64::try_from(entries.len()).unwrap_or(i64::MAX))
        }),
        "read" => output.data.get("content").and_then(serde_json::Value::as_str).map(|content| {
            ReadResultFacts::Bytes(i64::try_from(content.len()).unwrap_or(i64::MAX))
        }),
        _ => None,
    }
}

impl ToolExecutor {
    pub fn new(registry: Arc<ToolRegistry>, policy: Arc<dyn PolicyEngine>) -> Self {
        Self { registry, policy, approval_sink: None, event_bus: None, mutation_sink: None }
    }

    pub fn with_approval_sink(mut self, sink: Arc<dyn ApprovalSink>) -> Self {
        self.approval_sink = Some(sink);
        self
    }

    /// Attach an event bus so the executor can publish approval lifecycle
    /// events ([`EventKind::ApprovalRequested`] when a request starts waiting,
    /// [`EventKind::ApprovalResolved`] once it is answered) to subscribers.
    pub fn with_event_bus(mut self, bus: EventBus) -> Self {
        self.event_bus = Some(bus);
        self
    }

    /// Attach the ADR-82 slice 5 (b) pre-execution capture sink: after an
    /// `Allow` verdict (or an approval) and before the tool materializes a
    /// mutating canonical filesystem op, the executor banks the current bytes
    /// for active checkpoints covering the targets. Mirrors the filesystem
    /// tool's own capture-before-materialize discipline, lifted to the seam so
    /// the direct (ungated) path captures too. Fail-soft by contract (the sink
    /// swallows its own errors); a cancelled token skips the capture and the
    /// execution surfaces `Cancelled` itself.
    pub fn with_mutation_sink(mut self, sink: Arc<dyn MutationCaptureSink>) -> Self {
        self.mutation_sink = Some(sink);
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

    /// Resolve the canonical policy-view effect of a proposed call (ADR-82
    /// slice 1): the `(policy_name, operation)` identity — plus the canonical
    /// policy input — that policy will evaluate, derived from the registry
    /// this executor owns (the single grammar producer).
    ///
    /// `None` when `tool_name` is not registered: an unknown tool has no
    /// canonical identity, and callers keep their existing behavior (the
    /// ordinary execute path surfaces the "tool not found" refusal).
    ///
    /// This is an accounting/attribution view only — it never selects an
    /// implementation to run. The executing implementation stays the one
    /// registered under `tool_name`; see [`CanonicalEffect`].
    pub fn canonical_effect(
        &self,
        tool_name: &str,
        input: &serde_json::Value,
    ) -> Option<CanonicalEffect> {
        let tool = self.registry.get(tool_name)?;
        Some(canonical_effect_of(tool, input))
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
        let ExecutionAuditContext {
            correlation_id,
            input_hash,
            facts,
            path_facts,
            canonical_tool,
            canonical_operation,
        } = audit;
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
        // Read-only result facts: record what the operation RETURNED (the
        // `exists` boolean, the `list` entry count, the `read` byte size)
        // straight from the result already in hand — never the content, and
        // never a second read. Mutating operations, failed executions and
        // tools that name no operation stay `None`.
        let result_facts = match (&result, &path_facts) {
            (Ok(output), Some(facts)) => read_result_facts(&facts.operation, output),
            _ => None,
        };
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
            path_facts,
            result_facts,
            // ADR-82 slice 1: the completion row records BOTH identities —
            // `tool_name` above is the requested (registered) name for
            // attribution/forensics, the canonical pair below is what
            // accounting keys on (they diverge for alias calls).
            canonical_tool,
            canonical_operation,
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
        let (verdict, user_response) = match decision {
            ApprovalDecision::Approve => ("Approved", "user approved"),
            ApprovalDecision::ApproveAllForSession => {
                ("ApprovedAllForSession", "user approved all for session")
            }
            ApprovalDecision::Deny => ("Denied", "user denied"),
        };
        self.record_approval_outcome(action, verdict, user_response, cancel).await;
    }

    /// Shared builder for an approval-decision audit row. `verdict` is the
    /// decision outcome (`Approved` / `ApprovedAllForSession` / `Denied`);
    /// `user_response` is the human-readable detail that records who or what
    /// answered (the user, or the run's cancellation).
    async fn record_approval_outcome(
        &self,
        action: &PolicyAction<'_>,
        verdict: &str,
        user_response: &str,
        cancel: CancellationToken,
    ) {
        let entry = AuditEntry {
            tool_name: action.tool_name.to_string(),
            verdict: verdict.to_owned(),
            input_hash: crate::policy::compute_input_hash(action.input),
            session_id: action.session_id,
            correlation_id: action.correlation_id,
            timestamp: OffsetDateTime::now_utc(),
            user_response: Some(user_response.to_owned()),
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
            path_facts: action.path_facts.clone(),
            result_facts: None,
            canonical_tool: None,
            canonical_operation: None,
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
    /// continue, or abort); this is the backend-agnostic record seam for that
    /// path. The verdict vocabulary is extended with `RequestContinue` /
    /// `RequestAbort`, the warning text is preserved as `user_response`, and
    /// `rule_matched` is `"user_ack"`. Because an ack is not tied to any tool
    /// call, `tool_name` is the synthetic `"request_ack"`, `input_hash` is the
    /// empty string (no input exists), and the ADR-28 §6 execution fields stay
    /// `None`.
    ///
    /// Wired in production through the single-agent loop
    /// (`AgentLoop::setup_undo_stash` → `ToolExecutionBackend::record_ack_decision`),
    /// so every ack decision point — a continuing ack, an aborted ack, and a
    /// queue-full refusal (which surfaces to the sink as `false`) — records a
    /// row. The in-process backend delegates here; the supervised agent-process
    /// backend leaves the audit supervisor-side (ADR-60 D4/D5).
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
            path_facts: None,
            result_facts: None,
            canonical_tool: None,
            canonical_operation: None,
        };
        if let Err(error) = self.policy.audit_log().record(entry, cancel).await {
            tracing::error!(%error, "ack-decision audit write failed");
        }
    }

    /// Persist the coordinator's FINAL run-shape decision (advisor-mode intent
    /// routing) as a distinct audit entry.
    ///
    /// This row records the shape the coordinator actually chose AFTER
    /// consulting session context, plus the reason, so an override of the
    /// routing hint is observable and diagnosable rather than a silent
    /// divergence. `tool_name` is the synthetic
    /// `"coordinator_shape"`, `verdict` carries the final shape (`Plan` |
    /// `Execute`), `rule_matched` the reason code, and `user_response` a
    /// compact JSON envelope `{hint, final_shape, reason}`. The ADR-28 §6
    /// execution fields stay `None`: there is no command behind a shape
    /// decision.
    pub async fn record_coordinator_shape_decision(
        &self,
        session_id: crate::ids::Ulid,
        correlation_id: crate::ids::Ulid,
        hint: &str,
        final_shape: &str,
        reason: &str,
        cancel: CancellationToken,
    ) {
        let entry = AuditEntry {
            tool_name: "coordinator_shape".to_owned(),
            verdict: final_shape.to_owned(),
            input_hash: String::new(),
            session_id,
            correlation_id,
            timestamp: OffsetDateTime::now_utc(),
            user_response: Some(
                serde_json::json!({
                    "hint": hint,
                    "final_shape": final_shape,
                    "reason": reason,
                })
                .to_string(),
            ),
            rule_matched: Some(reason.to_owned()),
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
            // A shape decision binds no plan and names no source revision.
            plan_id: None,
            source_revision: None,
            path_facts: None,
            result_facts: None,
            canonical_tool: None,
            canonical_operation: None,
        };
        if let Err(error) = self.policy.audit_log().record(entry, cancel).await {
            tracing::error!(%error, "coordinator-shape audit write failed");
        } else {
            tracing::debug!(hint, final_shape, reason, "coordinator shape recorded");
        }
    }

    /// Persist a generic bypass/abort *decision* row as a distinct audit entry.
    ///
    /// The supremacy invariant ([`crate::executor`] is the run's single
    /// executor) is that a path which would otherwise abort silently — a
    /// cleared checkpoint, a loop cap, an operator-visible drop, a
    /// pre-coordinator setup failure — records WHY it happened as a
    /// Coordinator Decision row. This is the shared sink those sites use so
    /// the row shape stays identical everywhere.
    ///
    /// `decision` is the stable machine code (e.g. `checkpoint-cleared-scope`)
    /// carried in both `verdict` and `rule_matched`; `reason` is the
    /// human-readable detail. `tool_name` is the synthetic
    /// `"coordinator_decision"`, `input_hash` is empty (no tool input exists),
    /// and `user_response` is a compact JSON envelope `{decision, reason}`.
    /// The ADR-28 §6 execution fields stay `None`: there is no command behind a
    /// decision. Fail-soft: an audit failure is logged, never propagated.
    pub async fn record_coordinator_decision(
        &self,
        session_id: crate::ids::Ulid,
        correlation_id: crate::ids::Ulid,
        decision: &str,
        reason: &str,
        cancel: CancellationToken,
    ) {
        let entry = AuditEntry {
            tool_name: "coordinator_decision".to_owned(),
            verdict: decision.to_owned(),
            input_hash: String::new(),
            session_id,
            correlation_id,
            timestamp: OffsetDateTime::now_utc(),
            user_response: Some(
                serde_json::json!({ "decision": decision, "reason": reason }).to_string(),
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
            plan_id: None,
            source_revision: None,
            path_facts: None,
            result_facts: None,
            canonical_tool: None,
            canonical_operation: None,
        };
        if let Err(error) = self.policy.audit_log().record(entry, cancel).await {
            tracing::warn!(%error, decision, "coordinator-decision audit write failed (fail-soft)");
        } else {
            tracing::debug!(decision, reason, "coordinator decision recorded");
        }
    }

    /// Persist a plan-approval decision (ADR-55 §4) as a distinct audit
    /// entry.
    ///
    /// A plan decision is not tied to any tool call, so `tool_name` is the
    /// synthetic `"intent:plan"`. `rule_matched` and `verdict` both carry the
    /// user's decision label (`apply` | `replan` | `dismissed`), `input_hash`
    /// is the objective hash the binding is keyed on (so the audit row links
    /// back to the router's classification), and `user_response` is a compact
    /// JSON envelope with the bound `plan_id` and the source revision the plan
    /// was approved at. The same values are mirrored into the schema-derived
    /// `plan_id` / `source_revision` columns (ADR-55 §6) so the log is
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
            path_facts: None,
            result_facts: None,
            canonical_tool: None,
            canonical_operation: None,
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

    /// Persist an ADR-66 capability refusal as a distinct audit entry.
    ///
    /// A capability refusal is not tied to any tool call, so `tool_name` is
    /// the synthetic `"capability_gate"` and `input_hash` is empty. The row
    /// is observable and diagnosable (ADR-66 consequences: "every capability
    /// refusal writes audit rows — never silent"): `verdict` is `Refused`,
    /// `rule_matched` carries the refusing seam (`selection` before any
    /// spend, or `request_build` at the wire-path guard), and `user_response`
    /// holds the JSON envelope `{provider, model, capability, seam}`. The
    /// ADR-28 §6 execution fields stay `None`: there is no command behind a
    /// capability refusal.
    #[allow(clippy::too_many_arguments)]
    pub async fn record_capability_refusal(
        &self,
        session_id: crate::ids::Ulid,
        correlation_id: crate::ids::Ulid,
        provider: &str,
        model: &str,
        capability: &str,
        seam: &str,
        cancel: CancellationToken,
    ) {
        let entry = AuditEntry {
            tool_name: "capability_gate".to_owned(),
            verdict: "Refused".to_owned(),
            input_hash: String::new(),
            session_id,
            correlation_id,
            timestamp: OffsetDateTime::now_utc(),
            user_response: Some(
                serde_json::json!({
                    "provider": provider,
                    "model": model,
                    "capability": capability,
                    "seam": seam,
                })
                .to_string(),
            ),
            rule_matched: Some(seam.to_owned()),
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
            path_facts: None,
            result_facts: None,
            canonical_tool: None,
            canonical_operation: None,
        };
        if let Err(error) = self.policy.audit_log().record(entry, cancel).await {
            tracing::error!(%error, "capability-refusal audit write failed");
        } else {
            tracing::debug!(provider, model, capability, seam, "capability refusal recorded");
        }
    }

    /// Persist an ADR-66 §4 text-fallback driver event as a distinct audit
    /// entry.
    ///
    /// Fallback engagements must stay observable (ADR-66: "fallback turns
    /// are labeled in the transcript and audit — `tool_driver: fallback`").
    /// `tool_name` is the synthetic `"tool_driver"`, `verdict` is
    /// `"fallback"` (an engagement or a repair turn) or `"exhausted"` (the
    /// bounded-repair budget ran out — the loud terminal state),
    /// `rule_matched` carries the event (`engage` | `repair` |
    /// `exhausted`), and `user_response` holds the JSON envelope
    /// `{provider, model, event, detail}`. The ADR-28 §6 execution fields
    /// stay `None`: there is no command behind a driver event.
    #[allow(clippy::too_many_arguments)]
    pub async fn record_tool_driver_event(
        &self,
        session_id: crate::ids::Ulid,
        correlation_id: crate::ids::Ulid,
        provider: &str,
        model: &str,
        event: &str,
        verdict: &str,
        detail: &str,
        cancel: CancellationToken,
    ) {
        let entry = AuditEntry {
            tool_name: "tool_driver".to_owned(),
            verdict: verdict.to_owned(),
            input_hash: String::new(),
            session_id,
            correlation_id,
            timestamp: OffsetDateTime::now_utc(),
            user_response: Some(
                serde_json::json!({
                    "provider": provider,
                    "model": model,
                    "event": event,
                    "detail": detail,
                })
                .to_string(),
            ),
            rule_matched: Some(event.to_owned()),
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
            path_facts: None,
            result_facts: None,
            canonical_tool: None,
            canonical_operation: None,
        };
        if let Err(error) = self.policy.audit_log().record(entry, cancel).await {
            tracing::error!(%error, "tool-driver audit write failed");
        } else {
            tracing::debug!(provider, model, event, verdict, "tool driver event recorded");
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
        self.execute_inner(tool_name, input, session, cancel, false).await
    }

    /// Orchestrator-authority execute: identical to [`Self::execute`], but the
    /// policy action carries [`PolicyAction::orchestrator_authority`] = `true`.
    ///
    /// Called ONLY by the orchestrator's own top-level call sites (the
    /// Coordinator's self-executor tool calls and the single-agent `AgentLoop`
    /// executor calls). The engine then skips intent-derived restrictions
    /// (the grant-upgrade requirement, the read-only-intent pre-sink deny, and
    /// `un_granted` / `shell_requires_approval`) while keeping deny-class
    /// first, Consequential approval sinks, plan guards, and audit rows.
    /// Specialists, gate-proxy/supervisor children, and MCP/plugin bridges use
    /// [`Self::execute`] and stay fully intent-gated.
    pub async fn execute_with_authority(
        &self,
        tool_name: &str,
        input: serde_json::Value,
        session: &SessionContext,
        cancel: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        self.execute_inner(tool_name, input, session, cancel, true).await
    }

    async fn execute_inner(
        &self,
        tool_name: &str,
        input: serde_json::Value,
        session: &SessionContext,
        cancel: CancellationToken,
        orchestrator_authority: bool,
    ) -> Result<ToolOutput, ToolError> {
        let tool = self.registry.get(tool_name).ok_or_else(|| ToolError::ExecutionFailed {
            message: format!("tool not found: {tool_name}"),
        })?;

        self.execute_resolved(tool_name, tool, input, session, cancel, orchestrator_authority).await
    }

    /// Execute a host-owned operation through the same policy, approval, and audit gates.
    /// Extensions supply input only; the host constructs the operation and never grants authority.
    pub async fn execute_host_operation(
        &self,
        tool: &dyn Tool,
        input: serde_json::Value,
        session: &SessionContext,
        cancel: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        self.execute_resolved(tool.name(), tool, input, session, cancel, false).await
    }

    /// ADR-82 slice 5 (b): bank pre-mutation bytes for the canonical
    /// filesystem targets of an authorized call, when a capture sink is
    /// attached. Read-only operations, non-filesystem tools, and calls without
    /// versioned targets return immediately (no sink traffic); a cancelled
    /// token skips the capture (the execution surfaces `Cancelled` itself).
    /// Fail-soft by the sink's contract: nothing here can fail the write.
    async fn note_mutation_targets(
        &self,
        canonical: &CanonicalEffect,
        session: &SessionContext,
        cancel: CancellationToken,
    ) {
        let Some(sink) = self.mutation_sink.as_ref() else {
            return;
        };
        if cancel.is_cancelled() {
            return;
        }
        let targets = versioned_fs_targets(&canonical.policy_name, &canonical.policy_input);
        if targets.is_empty() {
            return;
        }
        sink.capture_before_mutation(targets, session, cancel).await;
    }

    #[allow(clippy::too_many_arguments)]
    async fn execute_resolved(
        &self,
        tool_name: &str,
        tool: &dyn Tool,
        input: serde_json::Value,
        session: &SessionContext,
        cancel: CancellationToken,
        orchestrator_authority: bool,
    ) -> Result<ToolOutput, ToolError> {
        if cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        let mut input = input;
        if let Err(error) = tool.prepare_input(&mut input, session) {
            // Preparation can refuse unsupported security requirements before
            // policy evaluation; the refusal is persisted without claiming a
            // resolved executable or an execution that never happened.
            self.record_preparation_refusal(tool_name, &input, session, &error, cancel).await;
            return Err(error);
        }

        // Policy evaluates the tool's canonical view (alias tools present the
        // canonical tool name and operation-bearing input); execution below
        // still uses the registered name and the caller's input. The same
        // canonical identity — derived by the one shared service — is recorded
        // on the completion row (ADR-82 slice 1): requested name for
        // attribution, canonical pair for accounting.
        let canonical = canonical_effect_of(tool, &input);
        let canonical_audit = (Some(canonical.policy_name.clone()), canonical.operation.clone());
        let action = build_action(
            &canonical.policy_name,
            &canonical.policy_input,
            tool,
            session,
            orchestrator_authority,
        );
        let correlation_id = action.correlation_id;
        let input_hash = crate::policy::compute_input_hash(&input);
        let command_facts = action.command_facts.clone();
        let path_facts = action.path_facts.clone();

        match self.policy.evaluate(&action, cancel.clone()).await {
            Ok(PolicyVerdict::Allow) => {
                // ADR-82 slice 5 (b): the write is authorized — bank current
                // bytes for active checkpoints BEFORE the tool materializes it.
                self.note_mutation_targets(&canonical, session, cancel.clone()).await;
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
                        path_facts,
                        canonical_tool: canonical_audit.0,
                        canonical_operation: canonical_audit.1,
                    },
                )
                .await
            }
            Ok(PolicyVerdict::Deny) => {
                Err(ToolError::PolicyDenied { rule: "policy_denied".into() })
            }
            // The verdict's `timeout` is deliberately ignored here: an
            // approval request has NO auto-expiry (owner requirement — the
            // session pauses completely until permission is given or
            // revoked). See `request_approval_decision`.
            Ok(
                PolicyVerdict::RequireApproval { .. }
                | PolicyVerdict::RequireApprovalWithTimeout { .. },
            ) => match &self.approval_sink {
                Some(sink) => {
                    match self
                        .request_approval_decision(sink.as_ref(), &action, cancel.clone())
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
                                    // ADR-82 slice 5 (b): same pre-execution
                                    // capture as the direct-Allow path above.
                                    self.note_mutation_targets(&canonical, session, cancel.clone())
                                        .await;
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
                                            path_facts,
                                            canonical_tool: canonical_audit.0,
                                            canonical_operation: canonical_audit.1,
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

    /// Persist a [`Tool::prepare_input`] refusal as an audit row.
    ///
    /// Shared by [`Self::execute_resolved`] and [`Self::execute_read_only`] so
    /// both paths write the identical `Deny` (for `PolicyDenied`) or
    /// `PreparationFailed` verdict, `rule_matched`, and input hash, without
    /// claiming a resolved executable or an execution that never happened.
    /// Fail-soft: an audit write failure is logged, never surfaced over the
    /// original refusal.
    async fn record_preparation_refusal(
        &self,
        tool_name: &str,
        input: &serde_json::Value,
        session: &SessionContext,
        error: &ToolError,
        cancel: CancellationToken,
    ) {
        let entry = AuditEntry {
            tool_name: tool_name.to_owned(),
            verdict: match error {
                ToolError::PolicyDenied { .. } => "Deny".into(),
                _ => "PreparationFailed".into(),
            },
            input_hash: crate::policy::compute_input_hash(input),
            session_id: session.session_id,
            correlation_id: crate::ids::new_id(),
            timestamp: OffsetDateTime::now_utc(),
            user_response: None,
            rule_matched: match error {
                ToolError::PolicyDenied { rule } => Some(rule.clone()),
                _ => None,
            },
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
            path_facts: None,
            result_facts: None,
            canonical_tool: None,
            canonical_operation: None,
        };
        if let Err(audit_error) = self.policy.audit_log().record(entry, cancel).await {
            tracing::error!(%audit_error, "preparation refusal audit write failed");
        }
    }

    /// Execute a tool **without** a policy decision, for gate-routed
    /// read-only operations.
    ///
    /// The write gate classifies `read`/`list`/`exists` filesystem operations
    /// as read-only and routes them here instead of through [`Self::execute`]'s
    /// policy gate: a read must never be denied by a write-oriented policy
    /// (nor persisted as a `write-rejected` whiteboard decision). Policy is
    /// still consulted *advisorily* by the gate — this method performs no
    /// ordinary policy evaluation. Input preparation runs first, exactly as on
    /// [`Self::execute_resolved`], so a tool that refuses its input is refused
    /// (and audited) here too; only then are the user security ceilings
    /// evaluated, and their refusals fail closed. It does keep the
    /// post-execution audit completion row, exactly like [`Self::execute`]'s
    /// allowed path.
    ///
    /// The caller is responsible for routing only genuinely read-only
    /// operations here; this method performs no read/write classification of
    /// its own.
    pub async fn execute_read_only(
        &self,
        tool_name: &str,
        input: serde_json::Value,
        session: &SessionContext,
        cancel: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let tool = self.registry.get(tool_name).ok_or_else(|| ToolError::ExecutionFailed {
            message: format!("tool not found: {tool_name}"),
        })?;
        // Preparation must run before the security ceiling: a tool that
        // refuses its input can never reach execution from this fast path
        // either, and the refusal is audited like the ordinary path's.
        let mut input = input;
        if let Err(error) = tool.prepare_input(&mut input, session) {
            self.record_preparation_refusal(tool_name, &input, session, &error, cancel.clone())
                .await;
            return Err(error);
        }
        // Build the audit context the same way `execute`'s allowed path does,
        // but without evaluating the action: the canonical policy view still
        // names the completion row's tool, and `command_facts` still enrich it.
        // ADR-82 slice 1: the canonical pair rides the completion row here
        // too, so alias calls are accounted identically on the fast path.
        let canonical = canonical_effect_of(tool, &input);
        let action =
            build_action(&canonical.policy_name, &canonical.policy_input, tool, session, false);
        if !matches!(
            self.policy.evaluate_security_ceiling(&action, cancel.clone()).await,
            Ok(PolicyVerdict::Allow)
        ) {
            return Err(ToolError::PolicyDenied {
                rule: "shell_security_read_only_ceiling".into(),
            });
        }
        let audit = ExecutionAuditContext {
            correlation_id: action.correlation_id,
            input_hash: crate::policy::compute_input_hash(&input),
            facts: action.command_facts.clone(),
            path_facts: action.path_facts.clone(),
            canonical_tool: Some(canonical.policy_name),
            canonical_operation: canonical.operation,
        };
        self.execute_allowed(tool, tool_name, input, session, cancel, audit).await
    }

    /// ADR-65 F1a: side-effect-free policy gate for the read-dedupe serve path.
    ///
    /// Re-evaluates the action through [`PolicyEngine::evaluate_advisory`] (no
    /// audit decision row, no quota consumption) and returns `true` only for an
    /// explicit [`PolicyVerdict::Allow`]. Unknown tools, `Deny`,
    /// `RequireApproval*`, or a policy error all return `false`, so the serve
    /// gate falls through to the normal executor path — a cached read is
    /// served only when the policy in force explicitly allows it.
    ///
    /// This must never be used for `provider` actions: the rate-limit check is
    /// intentionally bypassed in the advisory path, and the serve gate only
    /// ever consults it for file-read tools.
    pub async fn policy_verdict_is_allow(
        &self,
        tool_name: &str,
        input: &serde_json::Value,
        session: &SessionContext,
        cancel: CancellationToken,
    ) -> bool {
        let Some(tool) = self.registry.get(tool_name) else {
            return false;
        };
        let (policy_name, policy_input) = tool.policy_view(input);
        let action = build_action(&policy_name, &policy_input, tool, session, false);
        matches!(self.policy.evaluate_advisory(&action, cancel).await, Ok(PolicyVerdict::Allow))
    }

    /// ADR-65 F1b: persist a `ServedFromCache` audit row for a cached read.
    ///
    /// Called only when the serve gate served a cached read *without* executing
    /// the tool, so no policy decision row precedes this entry: it gets a
    /// fresh `correlation_id` and leaves the ADR-28 §6 execution fields `None`.
    /// The served path is recorded in a dedicated path-facts carrier (and, for
    /// backward compatibility with existing consumers, still in `argv`), and
    /// `rule_matched` is `"served_from_cache"`.
    ///
    /// `ServedFromCache` rows count toward read-count grounding metrics exactly
    /// like a real `ExecutionSucceeded` read row (ADR-65 §3.2). Fail-soft: a
    /// write error is logged and never fails the serve.
    #[allow(clippy::too_many_arguments)]
    pub async fn record_served_read_audit(
        &self,
        tool_name: &str,
        input: &serde_json::Value,
        path: &str,
        session: &SessionContext,
        cancel: CancellationToken,
    ) {
        // ADR-82 slice 1: the served row carries the same canonical accounting
        // identity as an executed read, resolved from the executor's own
        // registry (unknown tools keep None, as on every other row).
        let canonical = self.canonical_effect(tool_name, input);
        let entry = AuditEntry {
            tool_name: tool_name.to_owned(),
            verdict: "ServedFromCache".to_owned(),
            input_hash: crate::policy::compute_input_hash(input),
            session_id: session.session_id,
            correlation_id: crate::ids::new_id(),
            timestamp: OffsetDateTime::now_utc(),
            user_response: None,
            rule_matched: Some("served_from_cache".to_owned()),
            profile_id: None,
            resolved_executable: None,
            argv: Some(vec![path.to_owned()]),
            working_directory: None,
            network_requested: None,
            filesystem_scope: None,
            destructive_classification: None,
            exit_code: None,
            duration_ms: None,
            toolchain_version: None,
            plan_id: None,
            source_revision: None,
            path_facts: Some(PathPolicyFacts {
                operation: "read".to_owned(),
                attempted_path: Some(path.to_owned()),
                ..PathPolicyFacts::default()
            }),
            // No tool output at this seam: the serve path hands the caller the
            // cached payload directly, so there is no result to summarise
            // here without threading the payload through the API.
            result_facts: None,
            canonical_tool: canonical.as_ref().map(|effect| effect.policy_name.clone()),
            canonical_operation: canonical.as_ref().and_then(|effect| effect.operation.clone()),
        };
        if let Err(error) = self.policy.audit_log().record(entry, cancel).await {
            tracing::error!(%error, "served-read audit write failed");
        }
    }

    /// Restore a file checkpoint (ADR-82 §4, slice 5): an authorized,
    /// policy-gated, ownership-checked, idempotent mutation — never a bypass.
    ///
    /// Each byte-carrying resource restores as a normal gated `filesystem`
    /// write of its sidecar pre-image bytes through [`Self::execute`], so
    /// policy and `VirtualFs` apply exactly as for any other write; resources
    /// that were absent at checkpoint time restore as gated deletes (exact —
    /// absence needs no bytes). Two phases, fail-closed:
    ///
    /// 1. **Check everything**: ownership for every included path (a foreign
    ///    owner aborts the whole restore before any write — surfaced, never a
    ///    silent overwrite) and method/status gating (`Expired`/`Superseded`
    ///    refuse loudly — never advertise restorable without bytes;
    ///    `Manual` returns [`RestoreOutcome::RequiresOperator`] without
    ///    writing; `GitStash` delegates to the [`StashRestorer`]).
    /// 2. **Write everything**: policy-denied or failed writes abort
    ///    mid-restore with the checkpoint status left unchanged (retriable —
    ///    a retry rewrites the same bytes idempotently). Each write re-checks
    ///    the ownership oracle immediately before execution, so a sibling
    ///    that acquires a path after the Phase-1 pre-check aborts with the
    ///    same structured [`RestoreError::OwnershipConflict`] (the current
    ///    owner named) instead of overwriting silently.
    ///
    /// A second restore of the same id is a recorded no-op
    /// ([`RestoreOutcome::AlreadyRestored`]); the first restore's
    /// `Restored{at, by}` is the record. Restoring never acquires ownership:
    /// the gate's table is untouched (documented drift — the table only tracks
    /// gate-mediated acquires), and the pre-check is what keeps a restore
    /// from silently overwriting a foreign owner's artifact.
    #[allow(clippy::too_many_arguments)]
    pub async fn restore_checkpoint(
        &self,
        checkpoint_id: &str,
        store: &dyn CheckpointStore,
        ownership: &dyn OwnershipOracle,
        stash: &dyn StashRestorer,
        session: &SessionContext,
        by_agent: &str,
        cancel: CancellationToken,
    ) -> Result<RestoreOutcome, RestoreError> {
        use crate::checkpoint::{CheckpointResourceKind, CheckpointStatus, RestoreMethod};
        if cancel.is_cancelled() {
            return Err(RestoreError::Cancelled);
        }
        let record = store
            .load_record(checkpoint_id, cancel.clone())
            .await
            .map_err(RestoreError::Store)?
            .ok_or_else(|| RestoreError::NotFound(checkpoint_id.to_owned()))?;

        if let CheckpointStatus::Restored { at_ms, by } = &record.status {
            return Ok(RestoreOutcome::AlreadyRestored { at_ms: *at_ms, by: by.clone() });
        }
        match &record.status {
            CheckpointStatus::Expired { reason } => {
                return Err(RestoreError::NotRestorable {
                    id: checkpoint_id.to_owned(),
                    reason: format!("expired: {reason}"),
                });
            }
            CheckpointStatus::Superseded => {
                return Err(RestoreError::NotRestorable {
                    id: checkpoint_id.to_owned(),
                    reason: "superseded".to_string(),
                });
            }
            CheckpointStatus::Active | CheckpointStatus::Partial { .. } => {}
            CheckpointStatus::Restored { .. } => {}
        }

        match record.restore_method {
            RestoreMethod::Manual => {
                let mut limitations = record.limitations.clone();
                if limitations.is_empty() {
                    limitations.push(
                        "checkpoint restores as Manual: follow the record's excluded entries"
                            .to_string(),
                    );
                }
                return Ok(RestoreOutcome::RequiresOperator { limitations });
            }
            RestoreMethod::GitStash => {
                let detail = stash.restore_stash(&record).map_err(RestoreError::ExecutionFailed)?;
                if cancel.is_cancelled() {
                    return Err(RestoreError::Cancelled);
                }
                let _ = store
                    .mark_restored(checkpoint_id, crate::checkpoint::now_millis(), by_agent, cancel)
                    .await
                    .map_err(RestoreError::Store)?;
                return Ok(RestoreOutcome::StashRestored { detail });
            }
            RestoreMethod::PreImageStore => {}
        }

        // Phase 1: ownership pre-check over every included path, before any
        // write. Unowned paths pass (the gated writes below auto-attribute);
        // self-owned paths pass; foreign-owned paths abort the restore.
        let mut conflicts = Vec::new();
        for resource in &record.included {
            if let Some(conflict) =
                ownership_conflict(ownership, &resource.canonical_path, by_agent)
            {
                conflicts.push(conflict);
            }
        }
        if !conflicts.is_empty() {
            return Err(RestoreError::OwnershipConflict(conflicts));
        }

        // Phase 2: restore through gated writes.
        let mut restored = Vec::new();
        let mut recreated = Vec::new();
        let mut deleted = Vec::new();
        let mut missing = Vec::new();
        let mut limitations = Vec::new();
        let mut notes = Vec::new();
        for resource in &record.included {
            if cancel.is_cancelled() {
                return Err(RestoreError::Cancelled);
            }
            let path = resource.canonical_path.clone();
            if resource.kind == CheckpointResourceKind::FileDelete {
                // Absent at checkpoint time: ensure deletion (exact, no bytes).
                self.execute_filesystem_delete(&path, ownership, by_agent, session, cancel.clone())
                    .await?;
                deleted.push(path);
                continue;
            }
            // Byte-carrying kinds: the sidecar must hold verified bytes.
            match store.load_bytes(checkpoint_id, &path, cancel.clone()).await {
                Ok(PreImageBytes::Stored { bytes, hash, captured_at_ms: _, capture_kind }) => {
                    if blake3::hash(&bytes).to_hex().to_string() != hash {
                        missing.push(path.clone());
                        limitations.push(format!(
                            "{path}: sidecar bytes failed hash verification; refusing to write unverified bytes"
                        ));
                        continue;
                    }
                    // Byte-exact replay (binary included): the bytes ride
                    // `content_b64` through the normal gated `filesystem`
                    // write — same policy gate, same ownership checks, no
                    // text normalization — so what lands on disk is exactly
                    // what the checkpoint banked.
                    let existed = std::fs::metadata(session.project_dir.join(&path)).is_ok();
                    self.execute_filesystem_write_bytes(
                        &path,
                        &bytes,
                        ownership,
                        by_agent,
                        session,
                        cancel.clone(),
                    )
                    .await?;
                    if existed {
                        restored.push(path.clone());
                    } else {
                        recreated.push(path.clone());
                    }
                    if capture_kind == crate::checkpoint::CaptureKind::PreImage {
                        notes.push(format!(
                            "{path}: bytes backfilled by the first post-checkpoint mutation (pre-image provenance); exact when nothing changed between checkpoint creation and that mutation"
                        ));
                    }
                }
                Ok(PreImageBytes::HashOnly { .. }) | Ok(PreImageBytes::Absent) => {
                    missing.push(path.clone());
                    limitations.push(missing_bytes_limitation(&record, &path));
                }
                Err(error) => return Err(RestoreError::Store(error)),
            }
        }

        if missing.is_empty() {
            let transitioned = store
                .mark_restored(checkpoint_id, crate::checkpoint::now_millis(), by_agent, cancel)
                .await
                .map_err(RestoreError::Store)?;
            if transitioned {
                Ok(RestoreOutcome::Restored { restored, recreated, deleted, notes })
            } else {
                // Lost a race with a concurrent restore: report the stored
                // no-op rather than claiming this call restored.
                match store
                    .load_record(checkpoint_id, CancellationToken::new())
                    .await
                    .map_err(RestoreError::Store)?
                    .map(|record| record.status)
                {
                    Some(CheckpointStatus::Restored { at_ms, by }) => {
                        Ok(RestoreOutcome::AlreadyRestored { at_ms, by })
                    }
                    _ => Ok(RestoreOutcome::Restored { restored, recreated, deleted, notes }),
                }
            }
        } else {
            store
                .mark_partial(checkpoint_id, missing.clone(), cancel)
                .await
                .map_err(RestoreError::Store)?;
            Ok(RestoreOutcome::Partial { restored, missing, limitations })
        }
    }

    /// One gated byte-exact `filesystem` write for restore: policy +
    /// `VirtualFs` apply exactly as for any other write (never a bypass).
    ///
    /// The ownership oracle is re-consulted immediately before the write: a
    /// sibling that acquired the path after the Phase-1 pre-check aborts
    /// here with the structured [`RestoreError::OwnershipConflict`] (the
    /// current owner named, read from the gate's typed ownership surface —
    /// never string-matched denial prose, per ADR-82) and no write runs.
    /// Non-ownership denials keep their `PolicyDenied` mapping.
    ///
    /// The banked bytes ride `content_b64` (JSON strings are Unicode and
    /// cannot carry arbitrary bytes) and land verbatim — no text
    /// normalization, binary included. This is the only restore write path
    /// for byte-carrying resources.
    async fn execute_filesystem_write_bytes(
        &self,
        path: &str,
        bytes: &[u8],
        ownership: &dyn OwnershipOracle,
        by_agent: &str,
        session: &SessionContext,
        cancel: CancellationToken,
    ) -> Result<(), RestoreError> {
        use base64::Engine as _;
        if let Some(conflict) = ownership_conflict(ownership, path, by_agent) {
            return Err(RestoreError::OwnershipConflict(vec![conflict]));
        }
        let input = serde_json::json!({
            "operation": "write",
            "path": path,
            "content_b64": base64::engine::general_purpose::STANDARD.encode(bytes),
        });
        self.execute("filesystem", input, session, cancel).await.map(|_| ()).map_err(|error| {
            match error {
                ToolError::PolicyDenied { rule } => RestoreError::PolicyDenied(rule),
                ToolError::Cancelled => RestoreError::Cancelled,
                other => RestoreError::ExecutionFailed(other.to_string()),
            }
        })
    }

    /// One gated `filesystem` delete for restore (absent-at-checkpoint paths).
    /// Same Phase-2 ownership re-check as the byte-write path above.
    async fn execute_filesystem_delete(
        &self,
        path: &str,
        ownership: &dyn OwnershipOracle,
        by_agent: &str,
        session: &SessionContext,
        cancel: CancellationToken,
    ) -> Result<(), RestoreError> {
        if let Some(conflict) = ownership_conflict(ownership, path, by_agent) {
            return Err(RestoreError::OwnershipConflict(vec![conflict]));
        }
        let input = serde_json::json!({"operation": "delete", "path": path});
        self.execute("filesystem", input, session, cancel).await.map(|_| ()).map_err(|error| {
            match error {
                ToolError::PolicyDenied { rule } => RestoreError::PolicyDenied(rule),
                ToolError::Cancelled => RestoreError::Cancelled,
                other => RestoreError::ExecutionFailed(other.to_string()),
            }
        })
    }

    /// Park on the user's approval decision — **there is no auto-expiry**.
    ///
    /// Owner requirement: "There should be no timeout, the session is supposed
    /// to pause completely until permission is given or revoked." The timeout
    /// carried by `PolicyVerdict::RequireApproval{timeout}` (the knob behind
    /// `[policy] approval_timeout_secs`) is therefore NOT raced here; it is
    /// retained at the policy/config layer for compatibility and is inert
    /// (documented at its definition). This is the single enforcement point:
    /// once removed here, no approval anywhere in the run expires.
    ///
    /// * **Waiting primitive** — a park, never a poll: the sink awaits a
    ///   channel (`watch`/`oneshot`) held by the live UI, and the cancellation
    ///   arm awaits a notification. Nothing spins and nothing is logged while
    ///   waiting.
    /// * **Cancellation is the escape hatch** — user cancel / stop / session
    ///   teardown while the request is pending resolves it as DENY
    ///   (fail-closed): a `Denied` audit row records that the run's
    ///   cancellation answered, an `ApprovalResolved { approved: false }`
    ///   event is published, and `ToolError::Cancelled` lets the run unwind
    ///   cleanly. The tool never executes.
    /// * **Observability** — one `ApprovalRequested` event and one log line
    ///   mark the pending state (exactly once per request, so a waiting run is
    ///   distinguishable from a stuck one); resolution publishes one
    ///   `ApprovalResolved` event, and the caller records the decision audit
    ///   row naming who answered.
    /// * **Fail-closed** — a sink that returns without a decision (closed
    ///   channel, dismissed dialog) returns `Deny`; no sink at all denies at
    ///   the call site (`requires_approval_no_sink`). The only unbounded wait
    ///   is the legitimate one: a live UI that simply has not been clicked.
    async fn request_approval_decision(
        &self,
        sink: &dyn ApprovalSink,
        action: &PolicyAction<'_>,
        cancel: CancellationToken,
    ) -> Result<ApprovalDecision, ToolError> {
        // Pending state, emitted once before parking (never repeated).
        self.publish_approval_event(
            action,
            EventKind::ApprovalRequested {
                tool_name: action.tool_name.to_string(),
                // 0 = unbounded wait: no configured deadline is being armed.
                timeout_secs: 0,
            },
        );
        tracing::info!(
            tool_name = %action.tool_name,
            correlation_id = %action.correlation_id,
            "approval requested; pausing until the user answers (no timeout)"
        );

        tokio::select! {
            biased;
            // Cancellation first: on a cancel/answer race the run resolves
            // fail-closed (deny), never allow.
            () = cancel.cancelled() => {
                // A fresh (non-cancelled) token for the audit write: the
                // deny row must not be cancellable, or it would go unrecorded
                // on the exact path that needs it most.
                self.record_approval_outcome(
                    action,
                    "Denied",
                    "run cancelled while the approval was pending; denying (fail-closed)",
                    CancellationToken::new(),
                )
                .await;
                self.publish_approval_event(
                    action,
                    EventKind::ApprovalResolved {
                        tool_name: action.tool_name.to_string(),
                        approved: false,
                    },
                );
                Err(ToolError::Cancelled)
            }
            decision = sink.request_approval(action, cancel.clone()) => {
                self.publish_approval_event(
                    action,
                    EventKind::ApprovalResolved {
                        tool_name: action.tool_name.to_string(),
                        approved: !matches!(decision, ApprovalDecision::Deny),
                    },
                );
                Ok(decision)
            }
        }
    }

    /// Publish one approval lifecycle event for `action` when an event bus is
    /// attached. Fail-soft: a bus error is logged, never surfaced.
    fn publish_approval_event(&self, action: &PolicyAction<'_>, kind: EventKind) {
        if let Some(bus) = &self.event_bus {
            let _ = bus.publish_for_session(action.session_id, action.correlation_id, kind);
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
    use std::time::Duration;
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
                ..Default::default()
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

    /// Path-shaped tool (filesystem-like) that produces structured path facts,
    /// used to prove the carrier reaches both the policy and completion rows.
    struct PathTool;

    #[async_trait]
    impl Tool for PathTool {
        fn name(&self) -> &str {
            "filesystem"
        }
        fn description(&self) -> &str {
            "path-shaped test tool"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        fn capability_requirements(&self) -> CapabilitySet {
            CapabilitySet::default()
        }
        fn path_facts(
            &self,
            input: &serde_json::Value,
            _session: &SessionContext,
        ) -> Option<PathPolicyFacts> {
            Some(PathPolicyFacts {
                operation: input
                    .get("operation")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("read")
                    .to_owned(),
                attempted_path: input
                    .get("path")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned),
                resolved_path: Some("/proj/notes.txt".to_owned()),
                ..PathPolicyFacts::default()
            })
        }
        async fn execute(
            &self,
            _input: serde_json::Value,
            _policy: &dyn PolicyEngine,
            _session: &SessionContext,
            _cancel: CancellationToken,
        ) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput { summary: "ok".to_owned(), data: serde_json::json!({}) })
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

    struct RefusingPreparationTool;
    #[async_trait]
    impl Tool for RefusingPreparationTool {
        fn name(&self) -> &str {
            "refuse"
        }
        fn description(&self) -> &str {
            "test preparation refusal"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        fn capability_requirements(&self) -> CapabilitySet {
            CapabilitySet::default()
        }
        fn prepare_input(
            &self,
            _input: &mut serde_json::Value,
            _session: &SessionContext,
        ) -> Result<(), ToolError> {
            Err(ToolError::PolicyDenied { rule: "unsupported_boundary".into() })
        }
        async fn execute(
            &self,
            _input: serde_json::Value,
            _policy: &dyn PolicyEngine,
            _session: &SessionContext,
            _cancel: CancellationToken,
        ) -> Result<ToolOutput, ToolError> {
            panic!("refused preparation must never execute")
        }
    }

    // verifies: backend refusals are audited even when ordinary policy would allow the request.
    #[tokio::test]
    async fn preparation_refusal_is_audited_without_execution_facts() {
        let mut registry = ToolRegistry::default();
        registry.register(Box::new(RefusingPreparationTool));
        let audit = Arc::new(RecordingAudit::default());
        let policy = Arc::new(RecordingPolicy { audit: audit.clone() });
        let executor = ToolExecutor::new(Arc::new(registry), policy);
        let input = serde_json::json!({"command":"unresolved"});
        let result = executor
            .execute("refuse", input.clone(), &test_session(), CancellationToken::new())
            .await;
        assert!(matches!(result, Err(ToolError::PolicyDenied { .. })));
        let entries = audit.entries.lock().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].verdict, "Deny");
        assert_eq!(entries[0].rule_matched.as_deref(), Some("unsupported_boundary"));
        assert_eq!(entries[0].input_hash, crate::policy::compute_input_hash(&input));
        assert!(entries[0].resolved_executable.is_none());
        assert!(entries[0].exit_code.is_none());
    }

    // verifies: the read-only fast path runs prepare_input before the security
    // ceiling, so a refusing tool is refused and audited instead of executed.
    #[tokio::test]
    async fn read_only_preparation_refusal_is_refused_and_audited() {
        let mut registry = ToolRegistry::default();
        registry.register(Box::new(RefusingPreparationTool));
        let audit = Arc::new(RecordingAudit::default());
        // `RecordingPolicy` keeps the security ceiling at Allow, so any
        // refusal observed here can only come from `prepare_input`.
        let policy = Arc::new(RecordingPolicy { audit: audit.clone() });
        let executor = ToolExecutor::new(Arc::new(registry), policy);
        let input = serde_json::json!({"command":"unresolved"});
        let result = executor
            .execute_read_only("refuse", input.clone(), &test_session(), CancellationToken::new())
            .await;
        // The tool's `execute` panics, so reaching this line also proves it
        // never ran.
        assert!(matches!(result, Err(ToolError::PolicyDenied { .. })));
        let entries = audit.entries.lock().unwrap();
        assert_eq!(entries.len(), 1, "the refusal must leave exactly one audit row");
        assert_eq!(entries[0].tool_name, "refuse");
        assert_eq!(entries[0].verdict, "Deny");
        assert_eq!(entries[0].rule_matched.as_deref(), Some("unsupported_boundary"));
        assert_eq!(entries[0].input_hash, crate::policy::compute_input_hash(&input));
        assert!(entries[0].resolved_executable.is_none());
        assert!(entries[0].exit_code.is_none());
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

    /// Records the `orchestrator_authority` flag of the last action it saw, so
    /// the two executor entry points can be told apart.
    #[derive(Default)]
    struct FlagRecordingPolicy {
        saw_authority: std::sync::Mutex<Option<bool>>,
    }

    #[async_trait]
    impl PolicyEngine for FlagRecordingPolicy {
        async fn evaluate(
            &self,
            action: &PolicyAction<'_>,
            _cancel: CancellationToken,
        ) -> Result<PolicyVerdict, PolicyError> {
            *self.saw_authority.lock().unwrap_or_else(|e| e.into_inner()) =
                Some(action.orchestrator_authority);
            // Deny is sufficient: the test only observes the action shape.
            Ok(PolicyVerdict::Deny)
        }

        fn audit_log(&self) -> &dyn crate::traits::policy::AuditLog {
            &NullTestAuditLog
        }
    }

    struct NullTestAuditLog;

    #[async_trait]
    impl crate::traits::policy::AuditLog for NullTestAuditLog {
        async fn record(
            &self,
            _entry: AuditEntry,
            _cancel: CancellationToken,
        ) -> Result<(), PolicyError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn execute_marks_action_non_authority() {
        let policy = Arc::new(FlagRecordingPolicy::default());
        let executor = ToolExecutor::new(test_registry(), policy.clone());
        let _ = executor
            .execute("echo", serde_json::json!({}), &test_session(), CancellationToken::new())
            .await;
        assert_eq!(
            *policy.saw_authority.lock().unwrap_or_else(|e| e.into_inner()),
            Some(false),
            "the ordinary execute path never sets the authority marker"
        );
    }

    #[tokio::test]
    async fn execute_with_authority_marks_action_authority() {
        let policy = Arc::new(FlagRecordingPolicy::default());
        let executor = ToolExecutor::new(test_registry(), policy.clone());
        let _ = executor
            .execute_with_authority(
                "echo",
                serde_json::json!({}),
                &test_session(),
                CancellationToken::new(),
            )
            .await;
        assert_eq!(
            *policy.saw_authority.lock().unwrap_or_else(|e| e.into_inner()),
            Some(true),
            "the authority path sets the marker"
        );
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

    /// The coordinator's final run-shape decision records its own
    /// `coordinator_shape` row carrying the final shape + reason.
    #[tokio::test]
    async fn coordinator_shape_row_carries_the_final_shape_and_reason() {
        let audit = Arc::new(RecordingAudit::default());
        let policy = Arc::new(RecordingPolicy { audit: audit.clone() });
        let executor = ToolExecutor::new(test_registry(), policy);
        let session = Ulid::new();

        executor
            .record_coordinator_shape_decision(
                session,
                Ulid::new(),
                "Plan",
                "Execute",
                "approved_plan_overrides_plan_hint",
                CancellationToken::new(),
            )
            .await;

        let entries = audit.entries.lock().unwrap();
        let shape = entries
            .iter()
            .find(|entry| entry.tool_name == "coordinator_shape")
            .expect("the coordinator shape row is recorded");
        assert_eq!(shape.verdict, "Execute", "verdict carries the final shape");
        assert_eq!(
            shape.rule_matched.as_deref(),
            Some("approved_plan_overrides_plan_hint"),
            "rule_matched carries the override reason"
        );
        let envelope: serde_json::Value =
            serde_json::from_str(shape.user_response.as_deref().unwrap_or("{}"))
                .expect("the response is a JSON envelope");
        assert_eq!(envelope["hint"], "Plan");
        assert_eq!(envelope["final_shape"], "Execute");
    }

    /// A bypass/abort decision row is a distinct, machine-queryable audit
    /// entry: `tool_name = "coordinator_decision"`, `verdict`/`rule_matched`
    /// carry the decision code, and `user_response` is a `{decision, reason}`
    /// envelope.
    #[tokio::test]
    async fn coordinator_decision_row_carries_the_decision_shape() {
        let audit = Arc::new(RecordingAudit::default());
        let policy = Arc::new(RecordingPolicy { audit: audit.clone() });
        let executor = ToolExecutor::new(test_registry(), policy);
        let session = Ulid::new();

        executor
            .record_coordinator_decision(
                session,
                Ulid::new(),
                "checkpoint-cleared-malformed",
                "the checkpoint could not be parsed",
                CancellationToken::new(),
            )
            .await;

        let entries = audit.entries.lock().unwrap();
        assert_eq!(entries.len(), 1, "exactly one row");
        let row = &entries[0];
        assert_eq!(row.tool_name, "coordinator_decision");
        assert_eq!(row.verdict, "checkpoint-cleared-malformed");
        assert_eq!(row.rule_matched.as_deref(), Some("checkpoint-cleared-malformed"));
        assert_eq!(row.session_id, session);
        let envelope: serde_json::Value =
            serde_json::from_str(row.user_response.as_deref().unwrap_or("{}"))
                .expect("the response is a JSON envelope");
        assert_eq!(envelope["decision"], "checkpoint-cleared-malformed");
        assert_eq!(envelope["reason"], "the checkpoint could not be parsed");
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
        // A non-path tool records no path facts either.
        assert_eq!(entries[0].path_facts, None);
        assert_eq!(entries[1].path_facts, None);
        // Both rows share correlation_id and input_hash.
        assert_eq!(entries[0].correlation_id, entries[1].correlation_id);
        assert_eq!(entries[0].input_hash, entries[1].input_hash);
    }

    /// ADR-82 slice 1: a thin alias tool presenting the `filesystem` policy
    /// view (like the real `write` tool).
    struct AliasWriteTool;

    #[async_trait]
    impl Tool for AliasWriteTool {
        fn name(&self) -> &str {
            "write"
        }
        fn description(&self) -> &str {
            "test alias presenting the filesystem policy view"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        fn capability_requirements(&self) -> CapabilitySet {
            CapabilitySet::default()
        }
        fn policy_view(&self, input: &serde_json::Value) -> (String, serde_json::Value) {
            let mut canonical = input.clone();
            canonical["operation"] = serde_json::json!("write");
            ("filesystem".to_owned(), canonical)
        }
        async fn execute(
            &self,
            input: serde_json::Value,
            _policy: &dyn PolicyEngine,
            _session: &SessionContext,
            _cancel: CancellationToken,
        ) -> Result<ToolOutput, ToolError> {
            // Marker proving the REGISTERED implementation executed (no reroute):
            // the executor must run `write`, not `filesystem`.
            let mut data = input;
            data["executed_by"] = serde_json::json!("write-alias");
            Ok(ToolOutput { summary: serde_json::to_string(&data).unwrap_or_default(), data })
        }
    }

    /// ADR-82 slice 1: an alias call's verdict + completion rows carry BOTH
    /// identities — the requested name for attribution, the canonical pair
    /// for accounting — and the registered implementation is what executes.
    #[tokio::test]
    async fn alias_write_rows_carry_requested_and_canonical_identities() {
        let audit = Arc::new(RecordingAudit::default());
        let policy = Arc::new(SimplePolicyEngine::new(
            vec![PolicyRule::AutoApprove(Condition::Always)],
            audit.clone(),
        ));
        let mut registry = ToolRegistry::default();
        registry.register(Box::new(AliasWriteTool));
        let executor = ToolExecutor::new(Arc::new(registry), policy);

        let output = executor
            .execute(
                "write",
                serde_json::json!({"path": "w.md"}),
                &test_session(),
                CancellationToken::new(),
            )
            .await
            .expect("alias executes");
        assert_eq!(
            output.data.get("executed_by").and_then(|v| v.as_str()),
            Some("write-alias"),
            "the registered alias implementation executed, not the canonical tool"
        );

        let entries = audit.entries.lock().unwrap();
        assert_eq!(entries.len(), 2, "expected policy row + post-execution row");
        // Verdict row keys on the canonical effect policy evaluated.
        assert_eq!(entries[0].verdict, "Allow");
        assert_eq!(entries[0].tool_name, "filesystem");
        assert_eq!(entries[0].canonical_tool.as_deref(), Some("filesystem"));
        assert_eq!(entries[0].canonical_operation.as_deref(), Some("write"));
        // Completion row keeps the requested name and repeats the pair.
        assert_eq!(entries[1].verdict, "ExecutionSucceeded");
        assert_eq!(entries[1].tool_name, "write");
        assert_eq!(entries[1].canonical_tool.as_deref(), Some("filesystem"));
        assert_eq!(entries[1].canonical_operation.as_deref(), Some("write"));
        assert_eq!(entries[0].correlation_id, entries[1].correlation_id);
    }

    /// A path-shaped tool's structured facts reach both the policy decision row
    /// and the post-execution completion row.
    #[tokio::test]
    async fn path_tool_records_path_facts_on_policy_and_completion_rows() {
        let audit = Arc::new(RecordingAudit::default());
        let policy = Arc::new(SimplePolicyEngine::new(
            vec![PolicyRule::AutoApprove(Condition::Always)],
            audit.clone(),
        ));
        let mut registry = ToolRegistry::default();
        registry.register(Box::new(PathTool));
        let executor = ToolExecutor::new(Arc::new(registry), policy);

        executor
            .execute(
                "filesystem",
                serde_json::json!({"operation": "write", "path": "notes.txt"}),
                &test_session(),
                CancellationToken::new(),
            )
            .await
            .expect("path tool executes");

        let entries = audit.entries.lock().unwrap();
        assert_eq!(entries.len(), 2, "policy row + completion row");
        let decision_facts = entries[0].path_facts.as_ref().expect("policy row carries path facts");
        assert_eq!(decision_facts.operation, "write");
        assert_eq!(decision_facts.attempted_path.as_deref(), Some("notes.txt"));
        assert_eq!(decision_facts.resolved_path.as_deref(), Some("/proj/notes.txt"));
        assert_eq!(
            entries[1].path_facts.as_ref(),
            Some(decision_facts),
            "the completion row carries the same path facts"
        );
    }

    /// Read-only-shaped tool: reports the input operation in its path facts
    /// and returns the matching filesystem result shape, so the completion
    /// row's *result* facts can be observed end to end.
    struct ReadOnlyShapeTool;

    #[async_trait]
    impl Tool for ReadOnlyShapeTool {
        fn name(&self) -> &str {
            "filesystem"
        }
        fn description(&self) -> &str {
            "read-only shaped test tool"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        fn capability_requirements(&self) -> CapabilitySet {
            CapabilitySet::default()
        }
        fn path_facts(
            &self,
            input: &serde_json::Value,
            _session: &SessionContext,
        ) -> Option<PathPolicyFacts> {
            Some(PathPolicyFacts {
                operation: input
                    .get("operation")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("read")
                    .to_owned(),
                attempted_path: input
                    .get("path")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned),
                ..PathPolicyFacts::default()
            })
        }
        async fn execute(
            &self,
            input: serde_json::Value,
            _policy: &dyn PolicyEngine,
            _session: &SessionContext,
            _cancel: CancellationToken,
        ) -> Result<ToolOutput, ToolError> {
            let operation =
                input.get("operation").and_then(serde_json::Value::as_str).unwrap_or("read");
            let data = match operation {
                "exists" => serde_json::json!({
                    "path": "a.txt",
                    "exists": input.get("exists").and_then(serde_json::Value::as_bool).unwrap_or(false),
                }),
                "list" => serde_json::json!({
                    "path": ".",
                    "entries": input.get("entries").cloned().unwrap_or_else(|| serde_json::json!([])),
                }),
                "read" => serde_json::json!({
                    "path": "a.txt",
                    "content": input.get("content").and_then(serde_json::Value::as_str).unwrap_or(""),
                }),
                _ => serde_json::json!({ "path": "a.txt", "size": 12 }),
            };
            Ok(ToolOutput { summary: "ok".to_owned(), data })
        }
    }

    /// The incident regression: a read-only operation's completion row
    /// records what it RETURNED — the `exists` boolean, the `list` entry
    /// count (so `entries=0` proves emptiness was learned), the `read` byte
    /// size — while a mutating operation records nothing, and no content
    /// ever reaches the row.
    #[tokio::test]
    async fn read_only_operations_record_result_facts_on_the_completion_row() {
        let audit = Arc::new(RecordingAudit::default());
        let policy = Arc::new(SimplePolicyEngine::new(
            vec![PolicyRule::AutoApprove(Condition::Always)],
            audit.clone(),
        ));
        let mut registry = ToolRegistry::default();
        registry.register(Box::new(ReadOnlyShapeTool));
        let executor = ToolExecutor::new(Arc::new(registry), policy);

        let content = "Hello, World!";
        let cases: [(&str, serde_json::Value, Option<ReadResultFacts>); 6] = [
            (
                "exists present",
                serde_json::json!({"operation": "exists", "path": "a.txt", "exists": true}),
                Some(ReadResultFacts::Exists(true)),
            ),
            (
                "exists absent",
                serde_json::json!({"operation": "exists", "path": "b.txt", "exists": false}),
                Some(ReadResultFacts::Exists(false)),
            ),
            (
                "list one entry",
                serde_json::json!({
                    "operation": "list",
                    "path": ".",
                    "entries": [{"name": "file_test_pass.txt", "kind": "file"}],
                }),
                Some(ReadResultFacts::Entries(1)),
            ),
            (
                "list empty",
                serde_json::json!({"operation": "list", "path": ".", "entries": []}),
                Some(ReadResultFacts::Entries(0)),
            ),
            (
                "read size",
                serde_json::json!({"operation": "read", "path": "a.txt", "content": content}),
                Some(ReadResultFacts::Bytes(13)),
            ),
            (
                "mutating write",
                serde_json::json!({"operation": "write", "path": "a.txt", "content": content}),
                None,
            ),
        ];

        for (label, input, expected) in cases {
            audit.entries.lock().expect("audit lock").clear();
            executor
                .execute("filesystem", input, &test_session(), CancellationToken::new())
                .await
                .expect("tool executes");

            let entries = audit.entries.lock().expect("audit lock");
            assert_eq!(entries.len(), 2, "{label}: policy row + completion row");
            assert_eq!(
                entries[0].result_facts, None,
                "{label}: decision-time rows never record a result"
            );
            assert_eq!(entries[1].result_facts, expected, "{label}");

            let rendered = entries[1].result_facts.map(|facts| facts.to_string());
            assert!(!rendered.as_deref().unwrap_or_default().contains(content), "{label}");
            assert!(
                rendered.as_deref().unwrap_or_default().len() <= 24,
                "{label}: bounded rendering, got {rendered:?}"
            );
            if let Some(ReadResultFacts::Entries(count)) = entries[1].result_facts {
                assert_eq!(count == 0, label == "list empty", "{label}: emptiness");
            }
        }
    }

    /// `read_result_facts` records only safe scalars: unknown operations and
    /// missing/mistyped fields yield nothing, and neither content nor entry
    /// names nor paths can appear in the rendering.
    #[test]
    fn read_result_facts_never_carries_content_or_unknown_shapes() {
        let content = "SUPER-SECRET-FILE-CONTENT";
        let read = ToolOutput {
            summary: "read".to_owned(),
            data: serde_json::json!({ "path": "secrets.env", "content": content }),
        };
        let facts = read_result_facts("read", &read).expect("a read result is recorded");
        assert_eq!(facts, ReadResultFacts::Bytes(i64::try_from(content.len()).unwrap_or(i64::MAX)));
        let rendered = facts.to_string();
        assert!(!rendered.contains("SUPER-SECRET"), "{rendered}");
        assert!(!rendered.contains("secrets.env"), "{rendered}");
        assert_eq!(rendered, "bytes=25");

        let list = ToolOutput {
            summary: "list".to_owned(),
            data: serde_json::json!({ "path": ".", "entries": [{"name": "id_rsa"}] }),
        };
        assert_eq!(read_result_facts("list", &list), Some(ReadResultFacts::Entries(1)));

        // Mutating operation, missing field, wrong type: record nothing.
        let write = ToolOutput {
            summary: "wrote".to_owned(),
            data: serde_json::json!({ "path": "a.txt", "size": 12 }),
        };
        assert_eq!(read_result_facts("write", &write), None);
        let no_bool =
            ToolOutput { summary: "odd".to_owned(), data: serde_json::json!({ "exists": "yes" }) };
        assert_eq!(read_result_facts("exists", &no_bool), None);
        let no_entries =
            ToolOutput { summary: "odd".to_owned(), data: serde_json::json!({ "path": "." }) };
        assert_eq!(read_result_facts("list", &no_entries), None);
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
    // Approval requests: no auto-expiry, one pending event, fail-closed
    // on cancellation, and no sink at all.
    // ------------------------------------------------------------------

    /// Policy that returns `RequireApprovalWithTimeout` with a **configured**
    /// timeout. The value is deliberately present: tests must prove the
    /// executor ignores it entirely and parks regardless of the deadline.
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

    /// Await a decision on a watch receiver (see the CLI/desktop sinks): return
    /// the value immediately when already resolved, else wait for the change.
    async fn watch_decision(
        mut receiver: tokio::sync::watch::Receiver<Option<ApprovalDecision>>,
    ) -> ApprovalDecision {
        loop {
            let current = *receiver.borrow();
            if let Some(decision) = current {
                return decision;
            }
            if receiver.changed().await.is_err() {
                return ApprovalDecision::Deny;
            }
        }
    }

    /// Approval sink whose decision is delivered through a `watch` channel, so
    /// tests control exactly when — and whether — the "user" responds. The
    /// sink keeps its own receiver so a decision resolved AFTER the requester
    /// dropped its awaiting future (cancellation / teardown) is still
    /// delivered — the "no orphaned oneshot loss" property.
    struct ControlledApprovalSink {
        pending: Mutex<Option<tokio::sync::watch::Sender<Option<ApprovalDecision>>>>,
        preserved: Mutex<Option<tokio::sync::watch::Receiver<Option<ApprovalDecision>>>>,
        requests: AtomicUsize,
    }

    impl ControlledApprovalSink {
        fn new() -> Self {
            Self {
                pending: Mutex::new(None),
                preserved: Mutex::new(None),
                requests: AtomicUsize::new(0),
            }
        }

        fn resolve(&self, decision: ApprovalDecision) {
            if let Some(sender) = self.pending.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
                let _ = sender.send(Some(decision));
            }
        }

        /// The decision currently preserved on the sink (delivered even when
        /// no requester is attached).
        fn preserved_decision(&self) -> Option<ApprovalDecision> {
            self.preserved
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
                .and_then(|receiver| *receiver.borrow())
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
            let (sender, receiver) = tokio::sync::watch::channel(None);
            *self.pending.lock().unwrap_or_else(|e| e.into_inner()) = Some(sender);
            *self.preserved.lock().unwrap_or_else(|e| e.into_inner()) = Some(receiver.clone());
            watch_decision(receiver).await
        }

        async fn approve_all_for_session(&self, _session_id: Ulid, _cancel: CancellationToken) {}

        async fn request_ack(
            &self,
            _session_id: Ulid,
            _message: &str,
            _cancel: CancellationToken,
        ) -> bool {
            true
        }
    }

    /// Tool that records how many times it actually executed, so tests can
    /// prove an unanswered approval never reaches execution.
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
    /// guarantees the pending request exists before the test advances the
    /// clock or cancels the run.
    async fn wait_for_approval_request(sink: &ControlledApprovalSink) {
        for _ in 0..1_000 {
            if sink.requests.load(Ordering::SeqCst) > 0 {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("approval request never reached the sink");
    }

    /// Await the next bus event, bounded so a missing event fails the test
    /// instead of hanging it (paused time auto-advances to the deadline).
    async fn next_event(rx: &mut crate::event::EventReceiver) -> crate::event::Event {
        let event = tokio::time::timeout(Duration::from_secs(30), rx.recv())
            .await
            .unwrap_or_else(|_| panic!("approval lifecycle event never arrived"))
            .expect("event bus must stay alive");
        (*event).clone()
    }

    /// No auto-expiry: an unanswered approval request parks indefinitely.
    ///
    /// The policy still carries a 10-second timeout; the executor must ignore
    /// it. Far past that deadline the call is still pending — no pause
    /// variant error, no denial, no re-prompt — and the tool has not run. The
    /// pending state is emitted exactly once, not polled repeatedly, and only
    /// an explicit answer completes the call.
    #[tokio::test(start_paused = true)]
    async fn approval_request_parks_far_past_the_configured_timeout() {
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

        // The pending state is published once, before parking.
        let requested = next_event(&mut rx).await;
        match &requested.kind {
            EventKind::ApprovalRequested { tool_name, timeout_secs } => {
                assert_eq!(tool_name, "echo");
                assert_eq!(*timeout_secs, 0, "0 = unbounded wait, no deadline armed");
            }
            other => panic!("expected ApprovalRequested event, got {other:?}"),
        }

        // Far past the configured 10s deadline the request is still pending.
        tokio::time::advance(Duration::from_secs(3_600)).await;
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        assert!(!handle.is_finished(), "an approval must park until answered, never auto-expire");
        assert_eq!(calls.load(Ordering::SeqCst), 0, "an unanswered request never executes");
        assert_eq!(
            sink.requests.load(Ordering::SeqCst),
            1,
            "exactly one prompt: no retry, no re-prompt, no re-ask"
        );
        assert!(
            rx.try_recv().is_err(),
            "the waiting state must be emitted once, not polled repeatedly"
        );

        // The run completes only when the user answers.
        sink.resolve(ApprovalDecision::Approve);
        let output = handle
            .await
            .expect("executor task must not panic")
            .expect("an approved request must execute");
        assert_eq!(output.data, serde_json::json!({"key": "value"}));
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        let resolved = next_event(&mut rx).await;
        match &resolved.kind {
            EventKind::ApprovalResolved { tool_name, approved } => {
                assert_eq!(tool_name, "echo");
                assert!(*approved, "the answer was an approval");
            }
            other => panic!("expected ApprovalResolved event, got {other:?}"),
        }
        assert!(rx.try_recv().is_err(), "one request, one resolution, no repeats");
    }

    #[tokio::test(start_paused = true)]
    async fn approval_answered_executes_normally() {
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
            .expect("an answered approval must succeed");
        assert_eq!(output.data, serde_json::json!({"key": "value"}));
    }

    /// Cancellation while a request is pending resolves it as DENY
    /// (fail-closed): the tool never runs, the audit trail records a `Denied`
    /// row naming the run's cancellation as the answer, an
    /// `ApprovalResolved { approved: false }` event is published, and the
    /// caller unwinds as `Cancelled` instead of hanging.
    #[tokio::test(start_paused = true)]
    async fn cancellation_while_pending_denies_fail_closed_and_unwinds() {
        let audit = Arc::new(RecordingAudit::default());
        let policy = Arc::new(RecordingApprovalPolicy { audit: audit.clone() });
        let calls = Arc::new(AtomicUsize::new(0));
        let mut registry = ToolRegistry::default();
        registry.register(Box::new(CallCountingTool::new(calls.clone())));
        let bus = EventBus::default();
        let mut rx = bus.subscribe();
        let sink = Arc::new(ControlledApprovalSink::new());
        let executor = ToolExecutor::new(Arc::new(registry), policy)
            .with_approval_sink(sink.clone())
            .with_event_bus(bus.clone());
        let session = test_session();
        let cancel = CancellationToken::new();

        let handle = tokio::spawn({
            let cancel = cancel.clone();
            async move { executor.execute("echo", serde_json::json!({}), &session, cancel).await }
        });
        wait_for_approval_request(&sink).await;
        let requested = next_event(&mut rx).await;
        assert!(
            matches!(requested.kind, EventKind::ApprovalRequested { .. }),
            "the pending state must precede the cancellation"
        );

        cancel.cancel();
        let error = handle
            .await
            .expect("executor task must not panic")
            .expect_err("cancellation must abort the pending approval");
        assert!(matches!(error, ToolError::Cancelled), "expected cancellation, got {error:?}");
        assert_eq!(calls.load(Ordering::SeqCst), 0, "a cancelled request never executes");

        let resolved = next_event(&mut rx).await;
        match &resolved.kind {
            EventKind::ApprovalResolved { approved, .. } => {
                assert!(!*approved, "cancellation must resolve the request as denied");
            }
            other => panic!("expected ApprovalResolved event, got {other:?}"),
        }

        // The sink — not the requester — owns the pending entry, so an answer
        // that arrives after the requester dropped its future is not lost.
        sink.resolve(ApprovalDecision::Approve);
        assert_eq!(
            sink.preserved_decision(),
            Some(ApprovalDecision::Approve),
            "a late answer must survive the requester's cancellation"
        );

        // Audit: the policy's `RequireApproval` row plus the fail-closed
        // `Denied` row recording who answered.
        let entries = audit.entries.lock().unwrap();
        assert_eq!(
            entries.len(),
            2,
            "expected RequireApproval + Denied rows, got: {:?}",
            entries.iter().map(|entry| &entry.verdict).collect::<Vec<_>>()
        );
        assert_eq!(entries[0].verdict, "RequireApproval");
        assert_eq!(entries[1].verdict, "Denied");
        let response = entries[1].user_response.as_deref().unwrap_or_default();
        assert!(
            response.contains("cancelled"),
            "the deny row must record that the run's cancellation answered, got {response:?}"
        );
    }

    /// Fail-closed with no UI attached: `requires_approval_no_sink` denies
    /// immediately instead of parking forever for an answer that can never
    /// arrive.
    #[tokio::test]
    async fn missing_sink_denies_without_hanging() {
        let executor =
            ToolExecutor::new(test_registry(), Arc::new(ApprovalPolicy::with_timeout(30)));
        let session = test_session();

        let error = tokio::time::timeout(
            Duration::from_secs(30),
            executor.execute("echo", serde_json::json!({}), &session, CancellationToken::new()),
        )
        .await
        .expect("a request with no sink must deny, never park")
        .expect_err("approval with no sink must be denied");
        match error {
            ToolError::PolicyDenied { rule } => {
                assert_eq!(rule, "requires_approval_no_sink");
            }
            other => panic!("expected PolicyDenied(requires_approval_no_sink), got {other:?}"),
        }
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
                path_facts: None,
                result_facts: None,
                canonical_tool: None,
                canonical_operation: None,
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
    // same channel as approval decisions (audit H-04; ADR-55 §6).
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

    /// The ack seam is caller-correlated: the recorded row carries the session
    /// id the caller passed, even when it differs from the fixture session, so
    /// the audit trail associates the ack with the run it actually came from
    /// (ADR-68 H-04 "session correlation"; `audit_log.session_id` since
    /// migration 002).
    #[tokio::test]
    async fn ack_decision_records_caller_session_id() {
        let audit = Arc::new(RecordingAudit::default());
        let policy = Arc::new(RecordingPolicy { audit: audit.clone() });
        let executor = ToolExecutor::new(test_registry(), policy);
        let fixture_session = test_session();
        let caller_session_id = Ulid::new();
        assert_ne!(caller_session_id, fixture_session.session_id);
        let correlation_id = Ulid::new();

        executor
            .record_ack_decision(
                caller_session_id,
                correlation_id,
                "Session correlation probe",
                true,
                CancellationToken::new(),
            )
            .await;

        let entries = audit.entries.lock().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].session_id, caller_session_id);
        assert_eq!(entries[0].correlation_id, correlation_id);
        assert_eq!(entries[0].verdict, "RequestContinue");
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
        // ADR-55 §6: the values are mirrored into schema-derived
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

    // ------------------------------------------------------------------
    // ADR-65 F1a/F1b: serve-gate re-evaluation and ServedFromCache audit
    // ------------------------------------------------------------------

    /// The serve gate returns true only for an explicit Allow and evaluates
    /// through the advisory path (no decision row is written).
    #[tokio::test]
    async fn policy_verdict_is_allow_true_on_allow_without_audit_row() {
        let audit = Arc::new(RecordingAudit::default());
        let policy = Arc::new(SimplePolicyEngine::new(
            vec![PolicyRule::AutoApprove(Condition::Always)],
            audit.clone(),
        ));
        let executor = ToolExecutor::new(test_registry(), policy);
        let session = test_session();
        let input = serde_json::json!({"path": "src/a.rs"});

        assert!(
            executor
                .policy_verdict_is_allow("echo", &input, &session, CancellationToken::new())
                .await
        );
        assert!(
            audit.entries.lock().unwrap().is_empty(),
            "serve-gate re-evaluation must not write a decision row"
        );
    }

    /// A denying policy forces the serve gate closed so the read falls through
    /// to the normal executor path.
    #[tokio::test]
    async fn policy_verdict_is_allow_false_on_deny() {
        let audit = Arc::new(RecordingAudit::default());
        let policy = Arc::new(SimplePolicyEngine::new(
            vec![PolicyRule::AutoDeny(Condition::Always)],
            audit.clone(),
        ));
        let executor = ToolExecutor::new(test_registry(), policy);
        let session = test_session();

        assert!(
            !executor
                .policy_verdict_is_allow(
                    "echo",
                    &serde_json::json!({}),
                    &session,
                    CancellationToken::new(),
                )
                .await
        );
        assert!(audit.entries.lock().unwrap().is_empty());
    }

    /// An unknown tool can never pass the serve gate even under an allow-all
    /// policy (there is no action to re-evaluate).
    #[tokio::test]
    async fn policy_verdict_is_allow_false_on_unknown_tool() {
        let executor = ToolExecutor::new(test_registry(), Arc::new(AllowPolicy));
        let session = test_session();

        assert!(
            !executor
                .policy_verdict_is_allow(
                    "nonexistent",
                    &serde_json::json!({}),
                    &session,
                    CancellationToken::new(),
                )
                .await
        );
    }

    /// A served read records a single `ServedFromCache` row: fresh correlation
    /// id, the served path in `argv` (the schema has no dedicated path column),
    /// and no command-fact or execution-outcome fields.
    #[tokio::test]
    async fn record_served_read_audit_writes_served_from_cache_row() {
        let audit = Arc::new(RecordingAudit::default());
        let policy = Arc::new(RecordingPolicy { audit: audit.clone() });
        let executor = ToolExecutor::new(test_registry(), policy);
        let session = test_session();

        executor
            .record_served_read_audit(
                "read_file",
                &serde_json::json!({"path": "src/a.rs"}),
                "src/a.rs",
                &session,
                CancellationToken::new(),
            )
            .await;

        let entries = audit.entries.lock().unwrap();
        assert_eq!(entries.len(), 1, "exactly one served-read row");
        assert_eq!(entries[0].tool_name, "read_file");
        assert_eq!(entries[0].verdict, "ServedFromCache");
        assert_eq!(entries[0].rule_matched.as_deref(), Some("served_from_cache"));
        assert_eq!(entries[0].argv.as_deref(), Some(&["src/a.rs".to_owned()][..]));
        assert_eq!(entries[0].session_id, session.session_id);
        // No decision row precedes a serve, so the correlation id is fresh and
        // the input hash is derived from the served input.
        assert_ne!(entries[0].correlation_id, Ulid::default());
        assert!(!entries[0].input_hash.is_empty());
        // A served read never executed: no command facts or outcome fields.
        assert_eq!(entries[0].profile_id, None);
        assert_eq!(entries[0].working_directory, None);
        assert_eq!(entries[0].exit_code, None);
        assert_eq!(entries[0].duration_ms, None);
    }
}

#[cfg(test)]
mod restore_tests {
    //! ADR-82 slice 5: `restore_checkpoint` is an authorized, ownership-checked,
    //! idempotent mutation. Fakes stand in for the sessions store, the
    //! ownership table, the stash, and the filesystem tool so these tests pin
    //! the executor's own contract (gated writes, two-phase ordering,
    //! no-op/partial/manual outcomes) without a database.

    use super::*;
    use crate::checkpoint::{
        CaptureKind, CheckpointByteRef, CheckpointExclusion, CheckpointRecord, CheckpointResource,
        CheckpointResourceKind, CheckpointScope, CheckpointStatus, CheckpointStoreError,
        PreImageBytes, RestoreMethod,
    };
    use crate::ids::Ulid;
    use async_trait::async_trait;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    /// In-memory `filesystem` tool: `write`/`delete` mutate the map, every
    /// call is counted so tests prove what did (and did not) execute.
    /// Clonable around shared state so the registered box and the test
    /// observe the same files. Stores bytes (like the real tool's overlay)
    /// and accepts the restore path's `content_b64` as well as text
    /// `content`.
    #[derive(Clone)]
    struct FakeFsTool {
        files: Arc<Mutex<HashMap<String, Vec<u8>>>>,
        calls: Arc<AtomicUsize>,
    }

    impl FakeFsTool {
        fn new(files: HashMap<String, String>) -> Self {
            Self {
                files: Arc::new(Mutex::new(
                    files.into_iter().map(|(k, v)| (k, v.into_bytes())).collect(),
                )),
                calls: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn content(&self, path: &str) -> Option<String> {
            self.files
                .lock()
                .expect("fake fs lock")
                .get(path)
                .and_then(|bytes| String::from_utf8(bytes.clone()).ok())
        }

        fn bytes(&self, path: &str) -> Option<Vec<u8>> {
            self.files.lock().expect("fake fs lock").get(path).cloned()
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl Tool for FakeFsTool {
        fn name(&self) -> &str {
            "filesystem"
        }

        fn description(&self) -> &str {
            "in-memory filesystem for restore tests"
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
            let operation = input.get("operation").and_then(serde_json::Value::as_str);
            let path =
                input.get("path").and_then(serde_json::Value::as_str).unwrap_or("").to_owned();
            match operation {
                Some("write") => {
                    use base64::Engine as _;
                    let bytes = if let Some(b64) =
                        input.get("content_b64").and_then(serde_json::Value::as_str)
                    {
                        base64::engine::general_purpose::STANDARD
                            .decode(b64)
                            .expect("test content_b64 is valid base64")
                    } else {
                        input
                            .get("content")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("")
                            .as_bytes()
                            .to_vec()
                    };
                    self.files.lock().expect("fake fs lock").insert(path.clone(), bytes.clone());
                    Ok(ToolOutput {
                        summary: format!("Wrote {} bytes", bytes.len()),
                        data: serde_json::json!({"path": path}),
                    })
                }
                Some("delete") => {
                    self.files.lock().expect("fake fs lock").remove(&path);
                    Ok(ToolOutput {
                        summary: "Deleted".to_string(),
                        data: serde_json::json!({"path": path}),
                    })
                }
                Some("read") => {
                    let content = self
                        .files
                        .lock()
                        .expect("fake fs lock")
                        .get(&path)
                        .cloned()
                        .unwrap_or_default();
                    Ok(ToolOutput {
                        summary: "Read".to_string(),
                        data: serde_json::json!({"path": path, "content": content}),
                    })
                }
                _ => Err(ToolError::ExecutionFailed { message: "unknown op".to_string() }),
            }
        }
    }

    /// In-memory [`CheckpointStore`].
    type BankedBytes = HashMap<String, (Vec<u8>, String, CaptureKind)>;

    struct FakeStore {
        record: Mutex<CheckpointRecord>,
        bytes: Mutex<BankedBytes>,
        hash_only: Mutex<HashMap<String, Option<String>>>,
    }

    impl FakeStore {
        fn new(record: CheckpointRecord) -> Self {
            Self {
                record: Mutex::new(record),
                bytes: Mutex::new(HashMap::new()),
                hash_only: Mutex::new(HashMap::new()),
            }
        }

        fn with_bytes(self, path: &str, bytes: &[u8], kind: CaptureKind) -> Self {
            self.bytes.lock().expect("store lock").insert(
                path.to_owned(),
                (bytes.to_vec(), blake3::hash(bytes).to_hex().to_string(), kind),
            );
            self
        }

        fn status(&self) -> CheckpointStatus {
            self.record.lock().expect("store lock").status.clone()
        }
    }

    #[async_trait]
    impl CheckpointStore for FakeStore {
        async fn load_record(
            &self,
            _checkpoint_id: &str,
            _cancel: CancellationToken,
        ) -> Result<Option<CheckpointRecord>, CheckpointStoreError> {
            Ok(Some(self.record.lock().expect("store lock").clone()))
        }

        async fn load_bytes(
            &self,
            _checkpoint_id: &str,
            canonical_path: &str,
            _cancel: CancellationToken,
        ) -> Result<PreImageBytes, CheckpointStoreError> {
            if let Some((bytes, hash, capture_kind)) =
                self.bytes.lock().expect("store lock").get(canonical_path).cloned()
            {
                return Ok(PreImageBytes::Stored { bytes, hash, captured_at_ms: 0, capture_kind });
            }
            if let Some(hash) = self.hash_only.lock().expect("store lock").get(canonical_path) {
                return Ok(PreImageBytes::HashOnly { hash: hash.clone() });
            }
            Ok(PreImageBytes::Absent)
        }

        async fn mark_restored(
            &self,
            _checkpoint_id: &str,
            at_ms: i64,
            by: &str,
            _cancel: CancellationToken,
        ) -> Result<bool, CheckpointStoreError> {
            let mut record = self.record.lock().expect("store lock");
            if matches!(record.status, CheckpointStatus::Restored { .. }) {
                return Ok(false);
            }
            record.status = CheckpointStatus::Restored { at_ms, by: by.to_owned() };
            Ok(true)
        }

        async fn mark_partial(
            &self,
            _checkpoint_id: &str,
            missing: Vec<String>,
            _cancel: CancellationToken,
        ) -> Result<(), CheckpointStoreError> {
            self.record.lock().expect("store lock").status = CheckpointStatus::Partial { missing };
            Ok(())
        }
    }

    /// Ownership oracle backed by a static map.
    #[derive(Debug, Default)]
    struct FakeOracle {
        owners: HashMap<String, String>,
    }

    impl OwnershipOracle for FakeOracle {
        fn owner_of(&self, canonical_path: &str) -> Option<String> {
            self.owners.get(canonical_path).cloned()
        }
    }

    /// Stash restorer recording its calls.
    #[derive(Debug, Default)]
    struct FakeStash {
        calls: Mutex<usize>,
    }

    impl StashRestorer for FakeStash {
        fn restore_stash(&self, _checkpoint: &CheckpointRecord) -> Result<String, String> {
            *self.calls.lock().expect("stash lock") += 1;
            Ok("stash popped".to_string())
        }
    }

    /// Recording capture sink for the pre-execution hook tests.
    #[derive(Debug, Default)]
    struct RecordingSink {
        captured: Mutex<Vec<Vec<String>>>,
    }

    #[async_trait]
    impl MutationCaptureSink for RecordingSink {
        async fn capture_before_mutation(
            &self,
            canonical_paths: Vec<String>,
            _session: &SessionContext,
            _cancel: CancellationToken,
        ) {
            self.captured.lock().expect("sink lock").push(canonical_paths);
        }
    }

    fn session() -> SessionContext {
        SessionContext::new(Ulid::new(), std::path::PathBuf::from("/tmp/restore-test-no-disk"))
    }

    fn harness(
        files: HashMap<String, String>,
    ) -> (ToolExecutor, FakeFsTool, SessionContext, Arc<FakeStash>) {
        let tool = FakeFsTool::new(files);
        let mut registry = ToolRegistry::default();
        registry.register(Box::new(tool.clone()));
        let policy = Arc::new(AllowPolicyHarness);
        let executor = ToolExecutor::new(Arc::new(registry), policy);
        (executor, tool, session(), Arc::new(FakeStash::default()))
    }

    struct AllowPolicyHarness;

    #[async_trait]
    impl PolicyEngine for AllowPolicyHarness {
        async fn evaluate(
            &self,
            _action: &PolicyAction<'_>,
            _cancel: CancellationToken,
        ) -> Result<PolicyVerdict, PolicyError> {
            Ok(PolicyVerdict::Allow)
        }

        fn audit_log(&self) -> &dyn crate::traits::policy::AuditLog {
            &IgnoreAuditHarness
        }
    }

    struct IgnoreAuditHarness;

    #[async_trait]
    impl crate::traits::policy::AuditLog for IgnoreAuditHarness {
        async fn record(
            &self,
            _entry: AuditEntry,
            _cancel: CancellationToken,
        ) -> Result<(), PolicyError> {
            Ok(())
        }
    }

    fn active_record(
        included: Vec<CheckpointResource>,
        method: RestoreMethod,
    ) -> (CheckpointRecord, String) {
        let checkpoint_id = Ulid::new();
        let record = CheckpointRecord::new(
            checkpoint_id,
            Ulid::new(),
            CheckpointScope::FilesystemWrites {
                paths: included.iter().map(|r| r.canonical_path.clone()).collect(),
            },
            included,
            Vec::new(),
            std::collections::BTreeMap::new(),
            CheckpointStatus::Active,
            method,
            Vec::new(),
            "agent-a".to_string(),
            1_700_000_000_000,
            None,
        )
        .expect("test record builds");
        (record, checkpoint_id.to_string())
    }

    fn overwrite_resource(path: &str, pre: Option<&str>) -> CheckpointResource {
        CheckpointResource {
            kind: CheckpointResourceKind::FileOverwrite,
            canonical_path: path.to_owned(),
            pre_hash: pre.map(str::to_owned),
            post_hash: None,
            byte_ref: Some(CheckpointByteRef {
                checkpoint_id: "cid".to_string(),
                canonical_path: path.to_owned(),
            }),
            size_bytes: None,
        }
    }

    #[tokio::test]
    async fn restore_returns_byte_exact_content() {
        // A real tempdir backs the session so the existed-vs-recreated
        // heuristic observes the pre-restore file on disk.
        let tmp = tempfile::TempDir::new().expect("tmpdir");
        std::fs::write(tmp.path().join("a.txt"), "mutated").expect("seed file");
        let session = SessionContext::new(Ulid::new(), tmp.path().to_path_buf());
        let fs = FakeFsTool::new(HashMap::from([("a.txt".to_string(), "mutated".to_string())]));
        let mut registry = ToolRegistry::default();
        registry.register(Box::new(fs.clone()));
        let executor = ToolExecutor::new(Arc::new(registry), Arc::new(AllowPolicyHarness));
        let stash = Arc::new(FakeStash::default());
        let (record, id) =
            active_record(vec![overwrite_resource("a.txt", None)], RestoreMethod::PreImageStore);
        let store = FakeStore::new(record).with_bytes("a.txt", b"original", CaptureKind::Snapshot);

        let outcome = executor
            .restore_checkpoint(
                &id,
                &store,
                &FakeOracle::default(),
                &*stash,
                &session,
                "agent-a",
                CancellationToken::new(),
            )
            .await
            .expect("restore succeeds");
        match outcome {
            RestoreOutcome::Restored { restored, recreated, deleted, .. } => {
                assert_eq!(restored, vec!["a.txt"]);
                assert!(recreated.is_empty());
                assert!(deleted.is_empty());
            }
            other => panic!("expected Restored, got {other:?}"),
        }
        assert_eq!(fs.content("a.txt").as_deref(), Some("original"));
        assert!(matches!(store.status(), CheckpointStatus::Restored { .. }));
    }

    #[tokio::test]
    async fn binary_bytes_restore_byte_exact_through_gated_write() {
        // Non-UTF-8 banked bytes restore byte-exact through the same gated
        // write path — no UTF-8 gate, no Partial/Manual diversion. The
        // bytes (0xFF/0x00) could never ride the text `content` field, so
        // byte equality proves the `content_b64` channel carried them.
        let (executor, fs, session, stash) =
            harness(HashMap::from([("a.bin".to_string(), "mutated".to_string())]));
        let (record, id) =
            active_record(vec![overwrite_resource("a.bin", None)], RestoreMethod::PreImageStore);
        let banked: Vec<u8> = vec![0x00, 0xFF, 0xFE, 0x01, 0x02, b'h', b'i'];
        let store = FakeStore::new(record).with_bytes("a.bin", &banked, CaptureKind::Snapshot);

        match executor
            .restore_checkpoint(
                &id,
                &store,
                &FakeOracle::default(),
                &*stash,
                &session,
                "agent-a",
                CancellationToken::new(),
            )
            .await
            .expect("binary restore succeeds")
        {
            RestoreOutcome::Restored { restored, recreated, .. } => {
                // The harness seeds no disk file, so the write recreates.
                assert!(restored.is_empty());
                assert_eq!(recreated, vec!["a.bin"]);
            }
            other => panic!("expected Restored, got {other:?}"),
        }
        assert_eq!(fs.bytes("a.bin").as_deref(), Some(banked.as_slice()));
        assert!(matches!(store.status(), CheckpointStatus::Restored { .. }));
    }

    #[tokio::test]
    async fn restore_recreates_when_absent_at_restore_time() {
        // Session dir exists but the file does not: the write recreates it.
        let tmp = tempfile::TempDir::new().expect("tmpdir");
        let session = SessionContext::new(Ulid::new(), tmp.path().to_path_buf());
        let tool = FakeFsTool::new(HashMap::new());
        let mut registry = ToolRegistry::default();
        registry.register(Box::new(tool.clone()));
        let executor = ToolExecutor::new(Arc::new(registry), Arc::new(AllowPolicyHarness));
        let stash = Arc::new(FakeStash::default());
        let (record, id) =
            active_record(vec![overwrite_resource("a.txt", None)], RestoreMethod::PreImageStore);
        let store = FakeStore::new(record).with_bytes("a.txt", b"v1", CaptureKind::Snapshot);

        match executor
            .restore_checkpoint(
                &id,
                &store,
                &FakeOracle::default(),
                &*stash,
                &session,
                "agent-a",
                CancellationToken::new(),
            )
            .await
            .expect("restore succeeds")
        {
            RestoreOutcome::Restored { recreated, .. } => assert_eq!(recreated, vec!["a.txt"]),
            other => panic!("expected Restored, got {other:?}"),
        }
        assert_eq!(tool.content("a.txt").as_deref(), Some("v1"));
    }

    #[tokio::test]
    async fn double_restore_is_a_recorded_no_op() {
        let (executor, fs, session, stash) =
            harness(HashMap::from([("a.txt".to_string(), "mutated".to_string())]));
        let (record, id) =
            active_record(vec![overwrite_resource("a.txt", None)], RestoreMethod::PreImageStore);
        let store = FakeStore::new(record).with_bytes("a.txt", b"v1", CaptureKind::Snapshot);
        let oracle = FakeOracle::default();

        executor
            .restore_checkpoint(
                &id,
                &store,
                &oracle,
                &*stash,
                &session,
                "agent-a",
                CancellationToken::new(),
            )
            .await
            .expect("first restore");
        let calls_after_first = fs.calls();
        assert_eq!(fs.content("a.txt").as_deref(), Some("v1"));

        let outcome = executor
            .restore_checkpoint(
                &id,
                &store,
                &oracle,
                &*stash,
                &session,
                "agent-a",
                CancellationToken::new(),
            )
            .await
            .expect("second restore");
        match outcome {
            RestoreOutcome::AlreadyRestored { by, .. } => assert_eq!(by, "agent-a"),
            other => panic!("expected AlreadyRestored, got {other:?}"),
        }
        assert_eq!(fs.calls(), calls_after_first, "no write ran on re-restore");
        assert_eq!(fs.content("a.txt").as_deref(), Some("v1"));
    }

    #[tokio::test]
    async fn ownership_conflict_refuses_before_any_write() {
        let (executor, fs, session, stash) = harness(HashMap::from([
            ("a.txt".to_string(), "x".to_string()),
            ("b.txt".to_string(), "y".to_string()),
        ]));
        let (record, id) = active_record(
            vec![overwrite_resource("a.txt", None), overwrite_resource("b.txt", None)],
            RestoreMethod::PreImageStore,
        );
        let store = FakeStore::new(record)
            .with_bytes("a.txt", b"ra", CaptureKind::Snapshot)
            .with_bytes("b.txt", b"rb", CaptureKind::Snapshot);
        let oracle =
            FakeOracle { owners: HashMap::from([("b.txt".to_string(), "agent-b".to_string())]) };

        match executor
            .restore_checkpoint(
                &id,
                &store,
                &oracle,
                &*stash,
                &session,
                "agent-a",
                CancellationToken::new(),
            )
            .await
        {
            Err(RestoreError::OwnershipConflict(conflicts)) => {
                assert_eq!(conflicts.len(), 1);
                assert_eq!(conflicts[0].path, "b.txt");
                assert_eq!(conflicts[0].owner, "agent-b");
            }
            other => panic!("expected OwnershipConflict, got {other:?}"),
        }
        assert_eq!(fs.calls(), 0, "no write ran once a conflict surfaced");
        assert_eq!(fs.content("a.txt").as_deref(), Some("x"));
        assert!(matches!(store.status(), CheckpointStatus::Active), "status unchanged (retriable)");
    }

    /// Oracle modelling the TOCTOU race: the first lookup of the contested
    /// path (the Phase-1 pre-check) sees it unowned; every later lookup (the
    /// Phase-2 per-write re-check) sees the sibling's fresh acquisition.
    #[derive(Debug)]
    struct FlippingOracle {
        path: String,
        late_owner: String,
        lookups: Mutex<usize>,
    }

    impl OwnershipOracle for FlippingOracle {
        fn owner_of(&self, canonical_path: &str) -> Option<String> {
            if canonical_path != self.path {
                return None;
            }
            let mut seen = self.lookups.lock().expect("oracle lock");
            *seen += 1;
            if *seen == 1 {
                None
            } else {
                Some(self.late_owner.clone())
            }
        }
    }

    /// TOCTOU race: a sibling acquires ownership between the Phase-1
    /// pre-check and the Phase-2 gated write — the write's oracle re-check
    /// aborts with the STRUCTURED conflict naming the new owner (never a
    /// generic denial, never a silent overwrite), status left retriable.
    #[tokio::test]
    async fn phase2_ownership_recheck_surfaces_new_owner() {
        let (executor, fs, session, stash) =
            harness(HashMap::from([("a.txt".to_string(), "mutated".to_string())]));
        let (record, id) =
            active_record(vec![overwrite_resource("a.txt", None)], RestoreMethod::PreImageStore);
        let store = FakeStore::new(record).with_bytes("a.txt", b"original", CaptureKind::Snapshot);
        let oracle = FlippingOracle {
            path: "a.txt".to_string(),
            late_owner: "agent-b".to_string(),
            lookups: Mutex::new(0),
        };

        match executor
            .restore_checkpoint(
                &id,
                &store,
                &oracle,
                &*stash,
                &session,
                "agent-a",
                CancellationToken::new(),
            )
            .await
        {
            Err(RestoreError::OwnershipConflict(conflicts)) => {
                assert_eq!(conflicts.len(), 1);
                assert_eq!(conflicts[0].path, "a.txt");
                assert_eq!(
                    conflicts[0].owner, "agent-b",
                    "the conflict names the CURRENT owner, not a generic denial"
                );
            }
            other => panic!("expected structured OwnershipConflict, got {other:?}"),
        }
        assert_eq!(fs.calls(), 0, "the Phase-2 re-check aborts before any write");
        assert_eq!(fs.content("a.txt").as_deref(), Some("mutated"));
        assert!(
            matches!(store.status(), CheckpointStatus::Active),
            "mid-restore abort leaves the status unchanged (retriable)"
        );
    }

    #[tokio::test]
    async fn expired_and_superseded_refuse_loudly() {
        for status in [
            CheckpointStatus::Expired { reason: "retention prune".to_string() },
            CheckpointStatus::Superseded,
        ] {
            let (executor, fs, session, stash) =
                harness(HashMap::from([("a.txt".to_string(), "x".to_string())]));
            let checkpoint_id = Ulid::new();
            let record = CheckpointRecord::new(
                checkpoint_id,
                Ulid::new(),
                CheckpointScope::Run,
                vec![overwrite_resource("a.txt", None)],
                Vec::new(),
                std::collections::BTreeMap::new(),
                status,
                RestoreMethod::PreImageStore,
                Vec::new(),
                "agent-a".to_string(),
                0,
                None,
            )
            .expect("test record builds");
            let store = FakeStore::new(record).with_bytes("a.txt", b"ra", CaptureKind::Snapshot);
            match executor
                .restore_checkpoint(
                    &checkpoint_id.to_string(),
                    &store,
                    &FakeOracle::default(),
                    &*stash,
                    &session,
                    "agent-a",
                    CancellationToken::new(),
                )
                .await
            {
                Err(RestoreError::NotRestorable { .. }) => {}
                other => panic!("terminal status must refuse, got {other:?}"),
            }
            assert_eq!(fs.calls(), 0, "no write ran for a terminal checkpoint");
        }
    }

    #[tokio::test]
    async fn manual_checkpoints_require_the_operator() {
        let (executor, fs, session, stash) =
            harness(HashMap::from([("big.bin".to_string(), "x".to_string())]));
        let checkpoint_id = Ulid::new();
        let record = CheckpointRecord::new(
            checkpoint_id,
            Ulid::new(),
            CheckpointScope::Run,
            Vec::new(),
            vec![CheckpointExclusion {
                class: crate::checkpoint::CheckpointExclusionClass::OversizedFile,
                path: Some("big.bin".to_string()),
                reason: "too big".to_string(),
            }],
            std::collections::BTreeMap::new(),
            CheckpointStatus::Partial { missing: vec!["big.bin".to_string()] },
            RestoreMethod::Manual,
            vec!["binary deferred".to_string()],
            "agent-a".to_string(),
            0,
            None,
        )
        .expect("test record builds");
        let store = FakeStore::new(record);
        match executor
            .restore_checkpoint(
                &checkpoint_id.to_string(),
                &store,
                &FakeOracle::default(),
                &*stash,
                &session,
                "agent-a",
                CancellationToken::new(),
            )
            .await
            .expect("manual surfaces, never fails")
        {
            RestoreOutcome::RequiresOperator { limitations } => {
                assert!(!limitations.is_empty());
            }
            other => panic!("expected RequiresOperator, got {other:?}"),
        }
        assert_eq!(fs.calls(), 0, "Manual writes nothing");
    }

    #[tokio::test]
    async fn missing_bytes_restore_partial_with_limitations() {
        let (executor, fs, session, stash) =
            harness(HashMap::from([("a.txt".to_string(), "x".to_string())]));
        let (record, id) =
            active_record(vec![overwrite_resource("gone.txt", None)], RestoreMethod::PreImageStore);
        let store = FakeStore::new(record);
        match executor
            .restore_checkpoint(
                &id,
                &store,
                &FakeOracle::default(),
                &*stash,
                &session,
                "agent-a",
                CancellationToken::new(),
            )
            .await
            .expect("partial restore succeeds")
        {
            RestoreOutcome::Partial { restored, missing, limitations } => {
                assert!(restored.is_empty());
                assert_eq!(missing, vec!["gone.txt"]);
                assert!(!limitations.is_empty());
            }
            other => panic!("expected Partial, got {other:?}"),
        }
        assert_eq!(fs.calls(), 0);
        match store.status() {
            CheckpointStatus::Partial { missing } => assert_eq!(missing, vec!["gone.txt"]),
            other => panic!("status must be Partial, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn tampered_bytes_are_never_written() {
        let (executor, fs, session, stash) =
            harness(HashMap::from([("a.txt".to_string(), "x".to_string())]));
        let (record, id) =
            active_record(vec![overwrite_resource("a.txt", None)], RestoreMethod::PreImageStore);
        let store = FakeStore::new(record);
        // Bank bytes under a WRONG hash: the verifier must refuse them.
        store.bytes.lock().expect("store lock").insert(
            "a.txt".to_owned(),
            (b"evil".to_vec(), "deadbeef".to_string(), CaptureKind::Snapshot),
        );
        match executor
            .restore_checkpoint(
                &id,
                &store,
                &FakeOracle::default(),
                &*stash,
                &session,
                "agent-a",
                CancellationToken::new(),
            )
            .await
            .expect("verifier refusal restores partial")
        {
            RestoreOutcome::Partial { missing, limitations, .. } => {
                assert_eq!(missing, vec!["a.txt"]);
                assert!(limitations.iter().any(|l| l.contains("hash verification")));
            }
            other => panic!("expected Partial, got {other:?}"),
        }
        assert_eq!(fs.content("a.txt").as_deref(), Some("x"), "unverified bytes never land");
    }

    #[tokio::test]
    async fn delete_kind_restores_absence() {
        let (executor, fs, session, stash) =
            harness(HashMap::from([("new.txt".to_string(), "created after".to_string())]));
        let (record, id) = active_record(
            vec![CheckpointResource {
                kind: CheckpointResourceKind::FileDelete,
                canonical_path: "new.txt".to_owned(),
                pre_hash: None,
                post_hash: None,
                byte_ref: None,
                size_bytes: None,
            }],
            RestoreMethod::PreImageStore,
        );
        let store = FakeStore::new(record);
        match executor
            .restore_checkpoint(
                &id,
                &store,
                &FakeOracle::default(),
                &*stash,
                &session,
                "agent-a",
                CancellationToken::new(),
            )
            .await
            .expect("delete restore succeeds")
        {
            RestoreOutcome::Restored { deleted, .. } => assert_eq!(deleted, vec!["new.txt"]),
            other => panic!("expected Restored, got {other:?}"),
        }
        assert_eq!(fs.content("new.txt"), None);
    }

    #[tokio::test]
    async fn git_stash_delegates_to_the_stash_restorer() {
        let (executor, fs, session, stash) =
            harness(HashMap::from([("a.txt".to_string(), "x".to_string())]));
        let (mut record, id) = active_record(Vec::new(), RestoreMethod::GitStash);
        record.status = CheckpointStatus::Active;
        let store = FakeStore::new(record);
        match executor
            .restore_checkpoint(
                &id,
                &store,
                &FakeOracle::default(),
                &*stash,
                &session,
                "agent-a",
                CancellationToken::new(),
            )
            .await
            .expect("stash restore succeeds")
        {
            RestoreOutcome::StashRestored { detail } => assert_eq!(detail, "stash popped"),
            other => panic!("expected StashRestored, got {other:?}"),
        }
        assert_eq!(*stash.calls.lock().expect("stash lock"), 1);
        assert_eq!(fs.calls(), 0, "stash restores outside the byte path");
        assert!(matches!(store.status(), CheckpointStatus::Restored { .. }));
    }

    #[tokio::test]
    async fn pre_execution_sink_fires_only_for_mutating_fs_ops() {
        let tool = FakeFsTool::new(HashMap::new());
        let mut registry = ToolRegistry::default();
        registry.register(Box::new(tool));
        let sink = Arc::new(RecordingSink::default());
        let executor = ToolExecutor::new(Arc::new(registry), Arc::new(AllowPolicyHarness))
            .with_mutation_sink(sink.clone());
        let session = session();

        executor
            .execute(
                "filesystem",
                serde_json::json!({"operation": "write", "path": "a.txt", "content": "hi"}),
                &session,
                CancellationToken::new(),
            )
            .await
            .expect("write executes");
        executor
            .execute(
                "filesystem",
                serde_json::json!({"operation": "read", "path": "a.txt"}),
                &session,
                CancellationToken::new(),
            )
            .await
            .expect("read executes");
        assert!(
            executor
                .execute(
                    "shell",
                    serde_json::json!({"command": "ls"}),
                    &session,
                    CancellationToken::new(),
                )
                .await
                .is_err(),
            "unknown tool refuses"
        );

        let captured = sink.captured.lock().expect("sink lock").clone();
        assert_eq!(captured, vec![vec!["a.txt".to_string()]], "only the mutating op captured");
    }

    #[tokio::test]
    async fn cancelled_restore_writes_nothing() {
        let (executor, fs, session, stash) =
            harness(HashMap::from([("a.txt".to_string(), "x".to_string())]));
        let (record, id) =
            active_record(vec![overwrite_resource("a.txt", None)], RestoreMethod::PreImageStore);
        let store = FakeStore::new(record).with_bytes("a.txt", b"v", CaptureKind::Snapshot);
        let cancel = CancellationToken::new();
        cancel.cancel();
        match executor
            .restore_checkpoint(
                &id,
                &store,
                &FakeOracle::default(),
                &*stash,
                &session,
                "agent-a",
                cancel,
            )
            .await
        {
            Err(RestoreError::Cancelled) => {}
            other => panic!("expected Cancelled, got {other:?}"),
        }
        assert_eq!(fs.calls(), 0);
    }

    #[tokio::test]
    async fn unknown_checkpoint_id_is_not_found() {
        let (executor, _fs, session, stash) = harness(HashMap::new());
        struct EmptyStore;
        #[async_trait]
        impl CheckpointStore for EmptyStore {
            async fn load_record(
                &self,
                _id: &str,
                _cancel: CancellationToken,
            ) -> Result<Option<CheckpointRecord>, CheckpointStoreError> {
                Ok(None)
            }
            async fn load_bytes(
                &self,
                _id: &str,
                _path: &str,
                _cancel: CancellationToken,
            ) -> Result<PreImageBytes, CheckpointStoreError> {
                Ok(PreImageBytes::Absent)
            }
            async fn mark_restored(
                &self,
                _id: &str,
                _at: i64,
                _by: &str,
                _cancel: CancellationToken,
            ) -> Result<bool, CheckpointStoreError> {
                Ok(false)
            }
            async fn mark_partial(
                &self,
                _id: &str,
                _missing: Vec<String>,
                _cancel: CancellationToken,
            ) -> Result<(), CheckpointStoreError> {
                Ok(())
            }
        }
        match executor
            .restore_checkpoint(
                "nope",
                &EmptyStore,
                &FakeOracle::default(),
                &*stash,
                &session,
                "agent-a",
                CancellationToken::new(),
            )
            .await
        {
            Err(RestoreError::NotFound(id)) => assert_eq!(id, "nope"),
            other => panic!("expected NotFound, got {other:?}"),
        }
    }
}
