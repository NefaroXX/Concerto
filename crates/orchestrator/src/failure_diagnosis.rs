//! Structured failure diagnosis for the Coordinator (issue #54, parent #51).
//!
//! Every failure that surfaces in the coordinator — a dispatch error, a
//! settled specialist failure, a tool fault, a provider outage, an agent
//! crash — is normalized ONCE into a [`FailureDiagnosis`] at the point it
//! surfaces, and recovery is deterministic from that diagnosis. The
//! diagnosis is data, not behavior: the actions it selects are executed
//! through the EXISTING recovery machinery (the ADR-42 subtask retry budget,
//! the ADR-42/ADR-45 fallback ladder, the #53 reconsideration tracker, and
//! the graceful Partial exit) — this module adds no executor of its own.
//!
//! # The eight dimensions
//!
//! [`FailureKind`] covers the issue's minimum classification space:
//! Provider, Tool, Agent, Task, Environment, Dependency, Contract, Unknown.
//! Each normalized diagnosis additionally records:
//!
//! - `transient` — may resolve on its own;
//! - `retryable` — a re-dispatch is meaningful at all;
//! - `same_agent_viable` — the same agent/model can plausibly succeed;
//! - `alternate_agent_viable` — a different agent/model may succeed (the
//!   fallback ladder's precondition);
//! - `replan_required` — the task itself must be reshaped before retrying;
//! - `evidence` — a bounded, human-readable excerpt of the source error.
//!
//! # Deterministic recovery table
//!
//! [`recovery_action`] maps a diagnosis onto one of four verdicts, in the
//! issue's precedence order, using only the diagnosis flags and the
//! run's EXISTING attempt budget:
//!
//! 1. `RetrySame` — same-agent viable and attempts remain (the ADR-42
//!    Recoverable path; the budget is the caller's attempt counter, so
//!    retrying is bounded by construction);
//! 2. `RetryAlternate` — a different agent/model may succeed (the ADR-42
//!    fallback ladder's precondition);
//! 3. `Reconsider` — the task must be replanned/reconsidered (#53's
//!    reconsideration surface or the design-stage replan fallback);
//! 4. `Escalate` — nothing else is viable: the graceful Partial exit.
//!
//! Permanent failures therefore escalate or replan INSTEAD of looping: a
//! diagnosis with neither same-agent nor alternate viability never yields
//! `RetrySame`/`RetryAlternate`, and the budgets guarding the retry paths
//! are the run's own attempt counters, not new state.
//!
//! # Normalization boundaries (adapt, never balloon)
//!
//! The typed error enums (`ProviderError`, `ToolError`, `OrchestratorError`)
//! are NOT modified — they are matched here, at the coordinator boundary.
//! Where an existing classifier already encodes the right verdict it is
//! reused: [`ProviderError::is_transient`] remains the authority for
//! coordinator-level provider retryability (the provider-internal retry
//! layer has already run by the time an error surfaces here — a surfaced
//! `RetryExhausted` IS its terminal verdict), and
//! `concerto_providers::retry::classify_provider_error` supplies the
//! fine-grained class codes (rate-limit, gateway, stream-transport, …).
//! Agent-shaped failures that never produce a typed variant (malformed
//! output, missed artifact contracts, dependency failures reported as
//! outcome text) are diagnosed at the outcome boundary by
//! [`diagnose_outcome_failure`] / [`diagnose_blocked`].

use serde::{Deserialize, Serialize};

/// Upper bound on the evidence excerpt carried by one diagnosis. Errors are
/// untrusted model/tool output; the diagnosis stays bounded so checkpoints
/// and whiteboard rows cannot balloon.
pub const MAX_DIAGNOSIS_EVIDENCE_CHARS: usize = 512;

/// Bounded size of the coordinator's persisted diagnosis history (additive
/// checkpoint field; oldest entries drop first).
pub const MAX_DIAGNOSIS_HISTORY: usize = 32;

/// The failure dimensions (issue #54 minimum classification space).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FailureKind {
    /// The LLM provider/transport layer failed (network, rate limit,
    /// timeout, auth, context window, provider disappearance).
    Provider,
    /// A tool execution failed or was denied.
    Tool,
    /// The agent process itself failed (crash, loop, stall, iteration cap).
    Agent,
    /// The task/graph structure is at fault (invalid graph, planning
    /// failure, exhausted retry envelope).
    Task,
    /// The environment/budget the run depends on is at fault (memory
    /// store, spend cap, model availability, missing repository).
    Environment,
    /// A dependency of the work failed or is not ready.
    Dependency,
    /// The produced output missed its contract (expected artifacts not
    /// produced, malformed output, quality cycles exhausted).
    Contract,
    /// Nothing more specific applies.
    Unknown,
}

/// One normalized failure: the dimension, a stable machine code, the
/// recovery-relevant flags, and bounded evidence. Serialized additively
/// into checkpoints (`failure_diagnoses`) — old records default the field
/// empty and old readers ignore the key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailureDiagnosis {
    pub kind: FailureKind,
    /// Stable machine code for the specific failure signature (e.g.
    /// `network-loss`, `rate-limit`, `context-exhaustion`); stable strings
    /// the whiteboard payloads and tests key on.
    pub code: String,
    /// The failure may resolve on its own (transient vs permanent).
    pub transient: bool,
    /// A re-dispatch is meaningful at all.
    pub retryable: bool,
    /// The same agent/model can plausibly succeed on retry.
    pub same_agent_viable: bool,
    /// A different agent/model may succeed (fallback-ladder viable).
    pub alternate_agent_viable: bool,
    /// The task must be reshaped/replanned before it can succeed.
    pub replan_required: bool,
    /// Bounded excerpt of the source error (evidence for the diagnosis).
    pub evidence: String,
}

impl FailureKind {
    /// The kebab-case label used in whiteboard payloads and notes (the
    /// serde representation, by an explicit match — not `Debug`).
    pub fn as_str(self) -> &'static str {
        match self {
            FailureKind::Provider => "provider",
            FailureKind::Tool => "tool",
            FailureKind::Agent => "agent",
            FailureKind::Task => "task",
            FailureKind::Environment => "environment",
            FailureKind::Dependency => "dependency",
            FailureKind::Contract => "contract",
            FailureKind::Unknown => "unknown",
        }
    }
}

impl FailureDiagnosis {
    /// Build a diagnosis with a bounded evidence excerpt. The flag set is
    /// exactly the issue's recovery axes, so the builder is deliberately
    /// positional and total — every call site states all of them.
    #[allow(clippy::too_many_arguments)]
    fn new(
        kind: FailureKind,
        code: &str,
        transient: bool,
        retryable: bool,
        same_agent_viable: bool,
        alternate_agent_viable: bool,
        replan_required: bool,
        evidence: &str,
    ) -> Self {
        Self {
            kind,
            code: code.to_owned(),
            transient,
            retryable,
            same_agent_viable,
            alternate_agent_viable,
            replan_required,
            evidence: bounded_evidence(evidence),
        }
    }

    /// Whether this diagnosis describes a Cancellation (not a failure —
    /// callers short-circuit before diagnosing; this is the defensive
    /// shape).
    pub fn is_cancellation(&self) -> bool {
        self.code == "cancelled"
    }

    /// The compact human-readable line for bus events and run notes.
    pub fn brief(&self) -> String {
        let mut traits = Vec::new();
        traits.push(if self.transient { "transient" } else { "permanent" });
        if self.same_agent_viable {
            traits.push("retry-same viable");
        }
        if self.alternate_agent_viable {
            traits.push("alternate viable");
        }
        if self.replan_required {
            traits.push("replan required");
        }
        format!(
            "failure diagnosis: {} [{}] ({}) — {}",
            self.kind.as_str(),
            self.code,
            traits.join(", "),
            self.evidence
        )
    }

    /// The diagnosis as a tool-result JSON object the Coordinator's model
    /// reads (issue #54: the decision loop decides from the diagnosis).
    pub fn tool_summary(&self) -> serde_json::Value {
        serde_json::json!({
            "kind": self.kind.as_str(),
            "code": self.code,
            "transient": self.transient,
            "retryable": self.retryable,
            "same_agent_viable": self.same_agent_viable,
            "alternate_agent_viable": self.alternate_agent_viable,
            "replan_required": self.replan_required,
        })
    }
}

/// Bound the evidence excerpt (chars, not bytes — model output may be
/// multibyte).
fn bounded_evidence(evidence: &str) -> String {
    if evidence.chars().count() <= MAX_DIAGNOSIS_EVIDENCE_CHARS {
        return evidence.to_owned();
    }
    let mut out: String = evidence.chars().take(MAX_DIAGNOSIS_EVIDENCE_CHARS).collect();
    out.push('…');
    out
}

/// The deterministic recovery verdict for a diagnosis (issue #54): mapped
/// onto the EXISTING recovery machinery by the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryAction {
    /// Re-dispatch the same agent/model while the run's attempt budget
    /// holds (the ADR-42 Recoverable path).
    RetrySame,
    /// Fail over to a different agent/model (the ADR-42/ADR-45 fallback
    /// ladder).
    RetryAlternate,
    /// Replan/reconsider the task (#53 reconsideration or the design-stage
    /// replan fallback).
    Reconsider,
    /// Nothing is viable: the graceful escalate/Partial exit.
    Escalate,
}

/// The recovery table (deterministic): diagnosis + the run's attempt
/// budget → the verdict, in the issue's precedence order retry-same →
/// retry-alternate → replan-reconsider → escalate. The attempt budget is
/// the caller's EXISTING counter, so every retry path stays bounded by
/// construction — a diagnosis can never manufacture attempts.
pub fn recovery_action(
    diagnosis: &FailureDiagnosis,
    attempts_used: u32,
    max_attempts: u32,
) -> RecoveryAction {
    if diagnosis.same_agent_viable && diagnosis.retryable && attempts_used < max_attempts {
        return RecoveryAction::RetrySame;
    }
    if diagnosis.alternate_agent_viable {
        return RecoveryAction::RetryAlternate;
    }
    if diagnosis.replan_required {
        return RecoveryAction::Reconsider;
    }
    RecoveryAction::Escalate
}

/// Normalize ANY orchestration error into one diagnosis (issue #54).
/// Cancellation is diagnosed defensively (`is_cancellation`); callers
/// short-circuit on cancellation before this point.
pub fn diagnose(error: &concerto_core::OrchestratorError) -> FailureDiagnosis {
    // Cancellation is not a failure; diagnosed defensively for the shape.
    if matches!(
        error,
        concerto_core::OrchestratorError::Cancelled
            | concerto_core::OrchestratorError::Tool(concerto_core::ToolError::Cancelled)
            | concerto_core::OrchestratorError::Provider(
                concerto_core::error::ProviderError::Cancelled
            )
    ) {
        return cancelled_diagnosis(&error.to_string());
    }
    match error {
        concerto_core::OrchestratorError::Provider(provider) => diagnose_provider(provider),
        concerto_core::OrchestratorError::Tool(tool) => diagnose_tool(tool),
        concerto_core::OrchestratorError::AgentLoopError(message) => {
            diagnose_agent_message(message)
        }
        // An agent that hit its iteration cap or repeated identical tool
        // calls stalled — the same agent cannot do better, but a different
        // model/agent may (bounded by the ladder's at-most-once guards).
        concerto_core::OrchestratorError::MaxIterationsReached { .. }
        | concerto_core::OrchestratorError::CycleDetected { .. } => FailureDiagnosis::new(
            FailureKind::Agent,
            "agent-stalled",
            false,
            false,
            false,
            true,
            false,
            &error.to_string(),
        ),
        concerto_core::OrchestratorError::Memory(_) => FailureDiagnosis::new(
            FailureKind::Environment,
            "memory-store",
            true,
            true,
            true,
            true,
            false,
            &error.to_string(),
        ),
        // Model-selection failures are a property of the ASSIGNMENT, not of
        // the task — a different model may still complete it (the ladder).
        concerto_core::OrchestratorError::NoAffordableModel { .. }
        | concerto_core::OrchestratorError::NoCapableModel { .. }
        | concerto_core::OrchestratorError::PinnedModelNotFound { .. }
        | concerto_core::OrchestratorError::PinnedModelMissingCapability { .. }
        | concerto_core::OrchestratorError::PinnedModelBudgetExceeded { .. } => {
            FailureDiagnosis::new(
                FailureKind::Environment,
                "model-unavailable",
                false,
                false,
                false,
                true,
                false,
                &error.to_string(),
            )
        }
        // The spend cap is absolute: no recovery may spend past it.
        concerto_core::OrchestratorError::NoBudgetForDelegation => FailureDiagnosis::new(
            FailureKind::Environment,
            "budget-exhausted",
            false,
            false,
            false,
            false,
            false,
            &error.to_string(),
        ),
        // Structural task failures: re-dispatching cannot fix the graph.
        concerto_core::OrchestratorError::TaskGraphError(_)
        | concerto_core::OrchestratorError::InvalidTaskGraph { .. }
        | concerto_core::OrchestratorError::MultiAgentPlanFailed { .. }
        | concerto_core::OrchestratorError::SubTaskRetriesExhausted { .. } => {
            FailureDiagnosis::new(
                FailureKind::Task,
                "task-structure",
                false,
                false,
                false,
                false,
                true,
                &error.to_string(),
            )
        }
        // Quality cycles exhausted without converging: the contract was
        // never met; replanning (not another cycle) is the recovery.
        concerto_core::OrchestratorError::MaxReviewCyclesExceeded { .. }
        | concerto_core::OrchestratorError::MaxValidationCyclesExceeded { .. } => {
            FailureDiagnosis::new(
                FailureKind::Contract,
                "quality-cycle-exhausted",
                false,
                false,
                false,
                false,
                true,
                &error.to_string(),
            )
        }
        concerto_core::OrchestratorError::Unrecoverable { .. } => FailureDiagnosis::new(
            FailureKind::Unknown,
            "unrecoverable",
            false,
            false,
            false,
            false,
            true,
            &error.to_string(),
        ),
        _ => FailureDiagnosis::new(
            FailureKind::Unknown,
            "unknown",
            false,
            false,
            false,
            false,
            false,
            &error.to_string(),
        ),
    }
}

/// Cancellation, normalized defensively (callers short-circuit before
/// diagnosing real cancellations).
fn cancelled_diagnosis(evidence: &str) -> FailureDiagnosis {
    FailureDiagnosis::new(
        FailureKind::Unknown,
        "cancelled",
        false,
        false,
        false,
        false,
        false,
        evidence,
    )
}

/// Normalize a provider error (issue #54). Reuses BOTH existing
/// classifiers: [`ProviderError::is_transient`] stays the authority for
/// coordinator-level retryability (it is what the subtask-retry machinery
/// has always keyed on), and the providers retry classifier supplies the
/// fine-grained class code. The provider-internal retry layer has already
/// run by the time an error surfaces here — a surfaced `RetryExhausted` IS
/// that layer's terminal verdict, diagnosed as permanent with
/// alternate-agent viability (fail over to a different model/provider).
pub fn diagnose_provider(error: &concerto_core::error::ProviderError) -> FailureDiagnosis {
    use concerto_core::error::ProviderError;
    if matches!(error, ProviderError::Cancelled) {
        return cancelled_diagnosis(&error.to_string());
    }
    let transient = error.is_transient();
    // (kind, code): the specific signature first, the retry classifier's
    // class for the generic transient family.
    let (kind, code) = match error {
        // Provider disappearance: the configured provider/credential is
        // gone — fail over to another provider (ladder tier 1b/2).
        ProviderError::NotConfigured
        | ProviderError::UnsupportedProvider { .. }
        | ProviderError::CredentialMissing { .. } => {
            (FailureKind::Environment, "provider-disappeared".to_owned())
        }
        ProviderError::AuthFailure => (FailureKind::Provider, "provider-auth".to_owned()),
        ProviderError::ContextOverflow { .. } => {
            (FailureKind::Provider, "context-exhaustion".to_owned())
        }
        ProviderError::RetryExhausted { .. } => {
            (FailureKind::Provider, "provider-retries-exhausted".to_owned())
        }
        ProviderError::CapabilityRefused { .. } => {
            (FailureKind::Provider, "capability-refused".to_owned())
        }
        // A broken wire format will not heal on retry.
        ProviderError::Serialization(_) | ProviderError::InvalidResponse(_) => {
            (FailureKind::Provider, "malformed-provider-response".to_owned())
        }
        _ => {
            let class = concerto_providers::retry::classify_provider_error(error);
            let code = match class.class {
                Some(retry_class) => match retry_class {
                    concerto_providers::retry::RetryClass::RateLimited => "rate-limit".to_owned(),
                    concerto_providers::retry::RetryClass::Overloaded => {
                        "provider-overloaded".to_owned()
                    }
                    concerto_providers::retry::RetryClass::ServiceUnavailable => {
                        "provider-unavailable".to_owned()
                    }
                    concerto_providers::retry::RetryClass::GatewayFailure => {
                        "provider-gateway".to_owned()
                    }
                    concerto_providers::retry::RetryClass::Network
                    | concerto_providers::retry::RetryClass::ConnectionReset => {
                        "network-loss".to_owned()
                    }
                    concerto_providers::retry::RetryClass::StreamTransport => {
                        "stream-transport-fault".to_owned()
                    }
                    concerto_providers::retry::RetryClass::RequestTimeout
                    | concerto_providers::retry::RetryClass::StreamIdleTimeout => {
                        "provider-timeout".to_owned()
                    }
                    // The classifier is non-exhaustive by design; an
                    // unknown future class keeps the generic code.
                    _ => "provider-error".to_owned(),
                },
                None => "provider-error".to_owned(),
            };
            (FailureKind::Provider, code)
        }
    };
    let replan_required = code == "context-exhaustion";
    FailureDiagnosis::new(
        kind,
        &code,
        transient,
        transient,
        transient,
        true,
        replan_required,
        &error.to_string(),
    )
}

/// Normalize an agent-loop error message (the Agent dimension): a missed
/// artifact contract, malformed output, or a plain agent crash. The
/// recovery flags line up with the coordinator's existing dispatch-error
/// handling (corrective-feedback retries, then the fallback ladder).
pub fn diagnose_agent_message(message: &str) -> FailureDiagnosis {
    let lower = message.to_lowercase();
    if ARTIFACT_CONTRACT_MARKERS.iter().any(|marker| lower.contains(marker)) {
        return FailureDiagnosis::new(
            FailureKind::Contract,
            "artifact-contract-missed",
            false,
            true,
            true,
            true,
            true,
            message,
        );
    }
    if MALFORMED_OUTPUT_MARKERS.iter().any(|marker| lower.contains(marker)) {
        return FailureDiagnosis::new(
            FailureKind::Contract,
            "malformed-output",
            true,
            true,
            true,
            true,
            false,
            message,
        );
    }
    FailureDiagnosis::new(FailureKind::Agent, "agent-crash", true, true, true, true, false, message)
}

/// Normalize a tool error (issue #54). A policy denial is permanent and
/// absolute — the same dispatch would be denied again — so it escalates
/// instead of burning the attempt budget.
pub fn diagnose_tool(error: &concerto_core::ToolError) -> FailureDiagnosis {
    use concerto_core::ToolError;
    if matches!(error, ToolError::Cancelled) {
        return cancelled_diagnosis(&error.to_string());
    }
    match error {
        ToolError::PolicyDenied { .. } => FailureDiagnosis::new(
            FailureKind::Tool,
            "policy-denied",
            false,
            false,
            false,
            false,
            true,
            &error.to_string(),
        ),
        // An environment fact, not a transient fault.
        ToolError::NotARepository { .. } => FailureDiagnosis::new(
            FailureKind::Environment,
            "environment-not-a-repo",
            false,
            false,
            false,
            false,
            true,
            &error.to_string(),
        ),
        ToolError::RollbackNotSupported => FailureDiagnosis::new(
            FailureKind::Tool,
            "rollback-unsupported",
            false,
            false,
            false,
            false,
            true,
            &error.to_string(),
        ),
        // Execution faults, timeouts, vfs conflicts, LSP and I/O errors may
        // resolve on a re-run of the same agent.
        ToolError::ExecutionFailed { .. }
        | ToolError::Timeout { .. }
        | ToolError::VirtualFsConflict { .. }
        | ToolError::LspError { .. }
        | ToolError::Io(_) => FailureDiagnosis::new(
            FailureKind::Tool,
            "tool-failed",
            true,
            true,
            true,
            true,
            false,
            &error.to_string(),
        ),
        _ => FailureDiagnosis::new(
            FailureKind::Tool,
            "tool-failed",
            false,
            false,
            false,
            true,
            false,
            &error.to_string(),
        ),
    }
}

/// Artifact-contract failure markers (the same strings the coordinator's
/// replan fallback keys on — one grammar, one place).
const ARTIFACT_CONTRACT_MARKERS: &[&str] = &[
    "expected artifacts not produced",
    "no file-changing tool call succeeded",
    "the coder made no project file changes",
];

/// Markers of output the agent produced but that failed to parse.
const MALFORMED_OUTPUT_MARKERS: &[&str] = &[
    "malformed",
    "invalid json",
    "could not parse",
    "failed to parse",
    "unparseable",
    "did not produce valid",
];

/// Diagnose a settled `Failed { error }` outcome (the agent ran and
/// reported failure — agent crash, malformed output, or a missed artifact
/// contract). The recovery flags line up with the coordinator's existing
/// outcome handling: corrective-feedback retries, then the replan/ladder
/// fallbacks.
pub fn diagnose_outcome_failure(error: &str) -> FailureDiagnosis {
    let lower = error.to_lowercase();
    if ARTIFACT_CONTRACT_MARKERS.iter().any(|marker| lower.contains(marker)) {
        // The contract (expected artifacts) was missed: corrective retries
        // are viable, and a replan is required if they fail.
        return FailureDiagnosis::new(
            FailureKind::Contract,
            "artifact-contract-missed",
            false,
            true,
            true,
            true,
            true,
            error,
        );
    }
    if MALFORMED_OUTPUT_MARKERS.iter().any(|marker| lower.contains(marker)) {
        // Malformed output: a retry with the failure as feedback can fix it.
        return FailureDiagnosis::new(
            FailureKind::Contract,
            "malformed-output",
            true,
            true,
            true,
            true,
            false,
            error,
        );
    }
    // Generic agent failure: the agent ran to completion and failed.
    FailureDiagnosis::new(FailureKind::Agent, "agent-failed", true, true, true, true, false, error)
}

/// Diagnose a settled `Blocked { on }` outcome: the work is blocked on
/// dependencies that are not ready (issue #54's Dependency dimension).
pub fn diagnose_blocked(blockers: &[concerto_core::types::TaskId]) -> FailureDiagnosis {
    let evidence = format!("blocked on {} dependency task(s): {blockers:?}", blockers.len());
    FailureDiagnosis::new(
        FailureKind::Dependency,
        "dependency-failed",
        true,
        true,
        true,
        true,
        false,
        &evidence,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use concerto_core::error::ProviderError;
    use concerto_core::{OrchestratorError, ToolError};
    use std::time::Duration;

    // ── The issue's eight coverage scenarios ────────────────────────────
    // network loss, 429/rate-limit, provider disappearance, agent crash,
    // malformed output, tool failure, dependency failure, context
    // exhaustion. Each maps to a diagnosis with an observable recovery.

    #[test]
    fn network_loss_diagnoses_transient_and_retryable() {
        let diagnosis = diagnose(&OrchestratorError::Provider(ProviderError::Network(
            "connection reset by peer".into(),
        )));
        assert_eq!(diagnosis.kind, FailureKind::Provider);
        assert_eq!(diagnosis.code, "network-loss");
        assert!(diagnosis.transient);
        assert!(diagnosis.retryable);
        assert!(diagnosis.same_agent_viable);
        assert!(diagnosis.alternate_agent_viable);
        assert!(!diagnosis.replan_required);
        assert_eq!(
            recovery_action(&diagnosis, 0, 3),
            RecoveryAction::RetrySame,
            "network loss retries the same agent while the budget holds"
        );
    }

    #[test]
    fn mid_stream_transport_fault_is_a_transient_network_family_fault() {
        let diagnosis = diagnose(&OrchestratorError::Provider(ProviderError::StreamTransport(
            "connection dropped mid-stream".into(),
        )));
        assert_eq!(diagnosis.code, "stream-transport-fault");
        assert!(diagnosis.retryable && diagnosis.same_agent_viable);
    }

    #[test]
    fn rate_limit_429_diagnoses_transient_retryable() {
        for error in [
            ProviderError::RateLimit { retry_after: Duration::from_secs(2) },
            ProviderError::HttpStatus {
                status: 429,
                retry_after: Some(Duration::from_secs(2)),
                message: "too many requests".into(),
            },
        ] {
            let diagnosis = diagnose(&OrchestratorError::Provider(error));
            assert_eq!(diagnosis.code, "rate-limit");
            assert!(diagnosis.transient && diagnosis.retryable && diagnosis.same_agent_viable);
            assert_eq!(recovery_action(&diagnosis, 0, 3), RecoveryAction::RetrySame);
        }
    }

    #[test]
    fn provider_disappearance_diagnoses_permanent_failover_viable() {
        for error in [
            ProviderError::NotConfigured,
            ProviderError::UnsupportedProvider { provider: "ghost".into() },
            ProviderError::CredentialMissing { provider: "openai".into() },
        ] {
            let diagnosis = diagnose(&OrchestratorError::Provider(error));
            assert_eq!(diagnosis.kind, FailureKind::Environment, "disappearance is environmental");
            assert_eq!(diagnosis.code, "provider-disappeared");
            assert!(!diagnosis.transient && !diagnosis.retryable);
            assert!(!diagnosis.same_agent_viable);
            assert!(
                diagnosis.alternate_agent_viable,
                "the recovery path for a disappeared provider is fail-over"
            );
            assert_eq!(recovery_action(&diagnosis, 3, 3), RecoveryAction::RetryAlternate);
        }
    }

    #[test]
    fn agent_crash_diagnoses_transient_retryable() {
        let diagnosis = diagnose(&OrchestratorError::AgentLoopError("agent panicked".into()));
        assert_eq!(diagnosis.kind, FailureKind::Agent);
        assert_eq!(diagnosis.code, "agent-crash");
        assert!(diagnosis.transient && diagnosis.retryable && diagnosis.same_agent_viable);
        assert_eq!(recovery_action(&diagnosis, 0, 3), RecoveryAction::RetrySame);
    }

    #[test]
    fn malformed_output_diagnoses_contract_retryable() {
        let diagnosis = diagnose_outcome_failure("the model produced malformed JSON output");
        assert_eq!(diagnosis.kind, FailureKind::Contract);
        assert_eq!(diagnosis.code, "malformed-output");
        assert!(diagnosis.retryable && diagnosis.same_agent_viable);
        assert!(!diagnosis.replan_required, "a retry with feedback can fix malformed output");
    }

    #[test]
    fn tool_failure_diagnoses_retryable() {
        let diagnosis = diagnose(&OrchestratorError::Tool(ToolError::ExecutionFailed {
            message: "cargo test exited 101".into(),
        }));
        assert_eq!(diagnosis.kind, FailureKind::Tool);
        assert_eq!(diagnosis.code, "tool-failed");
        assert!(diagnosis.transient && diagnosis.retryable && diagnosis.same_agent_viable);
        assert_eq!(recovery_action(&diagnosis, 0, 3), RecoveryAction::RetrySame);

        // A timed-out tool is the same family.
        let timeout = diagnose(&OrchestratorError::Tool(ToolError::Timeout { timeout_secs: 30 }));
        assert_eq!(timeout.code, "tool-failed");
        assert!(timeout.retryable);
    }

    #[test]
    fn dependency_failure_diagnoses_retryable_and_observable() {
        let blocker = concerto_core::types::TaskId::new();
        let diagnosis = diagnose_blocked(&[blocker]);
        assert_eq!(diagnosis.kind, FailureKind::Dependency);
        assert_eq!(diagnosis.code, "dependency-failed");
        assert!(diagnosis.transient && diagnosis.retryable);
        assert!(
            diagnosis.evidence.contains("blocked on 1 dependency task"),
            "the evidence names the blockers: {}",
            diagnosis.evidence
        );
        assert_eq!(recovery_action(&diagnosis, 0, 3), RecoveryAction::RetrySame);
    }

    #[test]
    fn context_exhaustion_diagnoses_permanent_alternate_viable_replan() {
        let diagnosis = diagnose(&OrchestratorError::Provider(ProviderError::ContextOverflow {
            tokens_in: 200_000,
            capacity: 128_000,
        }));
        assert_eq!(diagnosis.kind, FailureKind::Provider);
        assert_eq!(diagnosis.code, "context-exhaustion");
        assert!(!diagnosis.transient && !diagnosis.retryable);
        assert!(!diagnosis.same_agent_viable, "the same context window cannot fit the same input");
        assert!(diagnosis.alternate_agent_viable, "a larger-context model is the recovery path");
        assert!(diagnosis.replan_required);
        // Precedence: alternate fail-over BEFORE replan, matching the
        // ladder's bounded semantics.
        assert_eq!(recovery_action(&diagnosis, 0, 3), RecoveryAction::RetryAlternate);
    }

    // ── The rest of the dimension mapping ──────────────────────────────

    #[test]
    fn provider_family_maps_to_expected_codes_and_viability() {
        // Transient family: retryable, same-agent viable.
        for (error, code) in [
            (
                ProviderError::Timeout { phase: "request", timeout: Duration::from_secs(5) },
                "provider-timeout",
            ),
            (
                ProviderError::HttpStatus {
                    status: 503,
                    retry_after: None,
                    message: "unavailable".into(),
                },
                "provider-unavailable",
            ),
            (
                ProviderError::HttpStatus {
                    status: 502,
                    retry_after: None,
                    message: "bad gateway".into(),
                },
                "provider-gateway",
            ),
            (ProviderError::InvalidResponse("bad shape".into()), "malformed-provider-response"),
        ] {
            let diagnosis = diagnose(&OrchestratorError::Provider(error));
            assert_eq!(diagnosis.code, code);
            assert!(diagnosis.same_agent_viable, "{code} retries the same agent");
        }
        // 4xx (non-429) is a permanent provider-side rejection.
        let http_400 = diagnose(&OrchestratorError::Provider(ProviderError::HttpStatus {
            status: 400,
            retry_after: None,
            message: "bad request".into(),
        }));
        assert!(!http_400.transient && !http_400.same_agent_viable);
        assert!(http_400.alternate_agent_viable);

        // RetryExhausted IS the providers retry layer's terminal verdict.
        let exhausted = diagnose(&OrchestratorError::Provider(ProviderError::RetryExhausted {
            attempts: 4,
            elapsed: Duration::from_secs(30),
            last_error: "all retries failed".into(),
        }));
        assert_eq!(exhausted.code, "provider-retries-exhausted");
        assert!(!exhausted.transient && !exhausted.retryable && !exhausted.same_agent_viable);
        assert!(exhausted.alternate_agent_viable);
        assert_eq!(recovery_action(&exhausted, 3, 3), RecoveryAction::RetryAlternate);

        // Auth is permanent for the same agent; fail-over is viable.
        let auth = diagnose(&OrchestratorError::Provider(ProviderError::AuthFailure));
        assert_eq!(auth.code, "provider-auth");
        assert!(!auth.same_agent_viable && auth.alternate_agent_viable);

        // A capability refusal is a permanent wire-path fact.
        let capability = diagnose(&OrchestratorError::Provider(ProviderError::CapabilityRefused {
            provider: "opencode".into(),
            model: "responses-only".into(),
            capability: "tool_calling".into(),
        }));
        assert_eq!(capability.code, "capability-refused");
        assert!(!capability.same_agent_viable && capability.alternate_agent_viable);
    }

    #[test]
    fn model_selection_failures_are_environmental_failover_viable() {
        let diagnosis = diagnose(&OrchestratorError::NoAffordableModel {
            role: concerto_core::types::AgentId::new("coder"),
        });
        assert_eq!(diagnosis.kind, FailureKind::Environment);
        assert_eq!(diagnosis.code, "model-unavailable");
        assert!(!diagnosis.same_agent_viable && diagnosis.alternate_agent_viable);
    }

    #[test]
    fn budget_exhaustion_escalates_and_never_retries() {
        let diagnosis = diagnose(&OrchestratorError::NoBudgetForDelegation);
        assert_eq!(diagnosis.kind, FailureKind::Environment);
        assert_eq!(diagnosis.code, "budget-exhausted");
        assert!(!diagnosis.same_agent_viable && !diagnosis.alternate_agent_viable);
        assert_eq!(recovery_action(&diagnosis, 0, 3), RecoveryAction::Escalate);
    }

    #[test]
    fn agent_stall_is_alternate_viable_not_same_agent() {
        for error in [
            OrchestratorError::MaxIterationsReached { max: 25 },
            OrchestratorError::CycleDetected { tool_name: "shell".into(), count: 4 },
        ] {
            let diagnosis = diagnose(&error);
            assert_eq!(diagnosis.kind, FailureKind::Agent);
            assert_eq!(diagnosis.code, "agent-stalled");
            assert!(!diagnosis.same_agent_viable, "the same agent would stall again");
            assert!(diagnosis.alternate_agent_viable);
        }
    }

    #[test]
    fn structural_task_failures_replan_and_escalate() {
        let diagnosis = diagnose(&OrchestratorError::TaskGraphError("missing node".into()));
        assert_eq!(diagnosis.kind, FailureKind::Task);
        assert_eq!(diagnosis.code, "task-structure");
        assert!(!diagnosis.same_agent_viable && !diagnosis.alternate_agent_viable);
        assert!(diagnosis.replan_required);
        assert_eq!(recovery_action(&diagnosis, 0, 3), RecoveryAction::Reconsider);
    }

    #[test]
    fn quality_cycle_exhaustion_is_a_contract_failure_requiring_replan() {
        let task_id = concerto_core::types::TaskId::new();
        let diagnosis =
            diagnose(&OrchestratorError::MaxValidationCyclesExceeded { task_id, cycles: 2 });
        assert_eq!(diagnosis.kind, FailureKind::Contract);
        assert_eq!(diagnosis.code, "quality-cycle-exhausted");
        assert!(!diagnosis.retryable && diagnosis.replan_required);
        assert_eq!(recovery_action(&diagnosis, 0, 3), RecoveryAction::Reconsider);
    }

    #[test]
    fn policy_denial_is_permanent_and_escalates() {
        let diagnosis = diagnose(&OrchestratorError::Tool(ToolError::PolicyDenied {
            rule: "deny-shell-rm".into(),
        }));
        assert_eq!(diagnosis.kind, FailureKind::Tool);
        assert_eq!(diagnosis.code, "policy-denied");
        assert!(!diagnosis.same_agent_viable && !diagnosis.alternate_agent_viable);
        assert!(diagnosis.replan_required);
        assert_eq!(recovery_action(&diagnosis, 0, 3), RecoveryAction::Reconsider);
    }

    #[test]
    fn artifact_contract_miss_diagnoses_replan_required() {
        let diagnosis = diagnose_outcome_failure(
            "Coder failed: expected artifacts not produced (src/lib.rs missing)",
        );
        assert_eq!(diagnosis.kind, FailureKind::Contract);
        assert_eq!(diagnosis.code, "artifact-contract-missed");
        assert!(diagnosis.retryable && diagnosis.same_agent_viable && diagnosis.replan_required);
        // While the budget holds, corrective retries come first; the replan
        // flag drives the EXISTING design-stage replan on exhaustion.
        assert_eq!(recovery_action(&diagnosis, 0, 3), RecoveryAction::RetrySame);
        assert_eq!(recovery_action(&diagnosis, 3, 3), RecoveryAction::RetryAlternate);
    }

    #[test]
    fn generic_agent_failure_and_cancellation_diagnose_defensively() {
        let failed = diagnose_outcome_failure("agent gave up");
        assert_eq!(failed.kind, FailureKind::Agent);
        assert_eq!(failed.code, "agent-failed");
        assert!(failed.retryable && failed.same_agent_viable);

        let cancelled = diagnose(&OrchestratorError::Cancelled);
        assert!(cancelled.is_cancellation());
        assert_eq!(recovery_action(&cancelled, 0, 3), RecoveryAction::Escalate);

        let tool_cancelled = diagnose(&OrchestratorError::Tool(ToolError::Cancelled));
        assert!(tool_cancelled.is_cancellation());

        let provider_cancelled = diagnose(&OrchestratorError::Provider(ProviderError::Cancelled));
        assert!(provider_cancelled.is_cancellation());

        let unrecoverable =
            diagnose(&OrchestratorError::Unrecoverable { message: "state corrupted".into() });
        assert_eq!(unrecoverable.kind, FailureKind::Unknown);
        assert_eq!(recovery_action(&unrecoverable, 0, 3), RecoveryAction::Reconsider);
    }

    // ── Recovery-table determinism + boundedness ────────────────────────

    #[test]
    fn recovery_table_is_deterministic_and_bounded_by_the_attempt_budget() {
        let diagnosis =
            diagnose(&OrchestratorError::Provider(ProviderError::Network("flaky".into())));
        // The SAME inputs always produce the SAME verdict.
        for attempt in 0..3 {
            assert_eq!(
                recovery_action(&diagnosis, attempt, 3),
                recovery_action(&diagnosis, attempt, 3)
            );
        }
        // RetrySame only while attempts remain.
        assert_eq!(recovery_action(&diagnosis, 0, 3), RecoveryAction::RetrySame);
        assert_eq!(recovery_action(&diagnosis, 2, 3), RecoveryAction::RetrySame);
        assert_eq!(
            recovery_action(&diagnosis, 3, 3),
            RecoveryAction::RetryAlternate,
            "budget exhausted → fail over, never a fourth retry"
        );
        // A zero budget goes straight to fail-over.
        assert_eq!(recovery_action(&diagnosis, 0, 0), RecoveryAction::RetryAlternate);
    }

    #[test]
    fn permanent_failure_never_yields_a_retry_verdict() {
        // For every PERMANENT, non-viable diagnosis the table must land on
        // Reconsider or Escalate — the anti-infinite-retry contract.
        let permanent: Vec<FailureDiagnosis> = vec![
            diagnose(&OrchestratorError::NoBudgetForDelegation),
            diagnose(&OrchestratorError::Tool(ToolError::PolicyDenied { rule: "r".into() })),
            diagnose(&OrchestratorError::Unrecoverable { message: "x".into() }),
            diagnose_blocked(&[]),
            diagnose(&OrchestratorError::Cancelled),
        ]
        .into_iter()
        .filter(|diagnosis| !diagnosis.transient && !diagnosis.same_agent_viable)
        .collect();
        assert!(!permanent.is_empty(), "the fixture must contain permanent diagnoses");
        for diagnosis in permanent {
            let action = recovery_action(&diagnosis, 0, 0);
            assert!(
                matches!(action, RecoveryAction::Reconsider | RecoveryAction::Escalate),
                "{} must not retry: got {action:?}",
                diagnosis.code
            );
        }
    }

    // ── Shape contracts ─────────────────────────────────────────────────

    #[test]
    fn diagnosis_evidence_is_bounded() {
        let huge = "x".repeat(MAX_DIAGNOSIS_EVIDENCE_CHARS * 4);
        let diagnosis = diagnose(&OrchestratorError::AgentLoopError(huge));
        assert!(diagnosis.evidence.chars().count() <= MAX_DIAGNOSIS_EVIDENCE_CHARS + 1);
        let small = diagnose(&OrchestratorError::AgentLoopError("short".into()));
        assert_eq!(small.evidence, "short");
    }

    #[test]
    fn diagnosis_round_trips_through_serde() {
        let diagnosis =
            diagnose(&OrchestratorError::Provider(ProviderError::Network("reset".into())));
        let json = serde_json::to_string(&diagnosis).expect("diagnosis serializes");
        let back: FailureDiagnosis = serde_json::from_str(&json).expect("diagnosis deserializes");
        assert_eq!(back, diagnosis);
        assert!(json.contains("\"provider\""), "kebab-case kind: {json}");
        assert!(json.contains("\"network-loss\""), "stable code: {json}");
    }

    #[test]
    fn tool_summary_and_brief_render_the_diagnosis() {
        let diagnosis = diagnose(&OrchestratorError::Provider(ProviderError::RateLimit {
            retry_after: Duration::from_secs(1),
        }));
        let summary = diagnosis.tool_summary();
        assert_eq!(summary["kind"], "provider");
        assert_eq!(summary["code"], "rate-limit");
        assert_eq!(summary["retryable"], true);
        let brief = diagnosis.brief();
        assert!(brief.contains("provider [rate-limit]"), "{brief}");
        assert!(brief.contains("transient"), "{brief}");
    }
}
