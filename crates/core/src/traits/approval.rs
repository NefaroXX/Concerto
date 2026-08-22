//! Approval interface for policy-gated tool calls.
//!
//! The CLI and Iced frontends each implement this trait so the orchestrator
//! can request user approval for dangerous operations without knowing which
//! UI is running.

use crate::ids::Ulid;
use crate::intent::{PlanDecision, RequestedOutcome};
use crate::types::PolicyAction;
use crate::CancellationToken;
use async_trait::async_trait;
use time::OffsetDateTime;

/// Decision returned by the approval sink.
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub enum ApprovalDecision {
    Approve,
    Deny,
    ApproveAllForSession,
}

/// Interface for requesting user approval of policy-gated actions.
///
/// The orchestrator holds an `Arc<dyn ApprovalSink>` and calls
/// `request_approval` before executing any operation whose policy verdict is
/// `RequireApproval` or `RequireApprovalWithTimeout`.
#[async_trait]
pub trait ApprovalSink: Send + Sync {
    /// Request approval for a specific policy action.
    /// Returns the user's decision.
    async fn request_approval(
        &self,
        action: &PolicyAction<'_>,
        cancel: CancellationToken,
    ) -> ApprovalDecision;

    /// Approve all remaining operations for the current session.
    async fn approve_all_for_session(&self, session_id: Ulid, cancel: CancellationToken);

    /// Ask the user to acknowledge a non-blocking warning before proceeding.
    /// Returns `true` if the user acknowledged and wants to continue,
    /// `false` if they want to abort the task.
    async fn request_ack(&self, message: &str, cancel: CancellationToken) -> bool;

    /// Ask the user to confirm a change of run intent (ADR-55 §1/§4).
    ///
    /// Called with the requested outcome names for the confirmation question.
    /// The default returns `None` (no confirmation surface available), which
    /// keeps every existing sink unchanged and is the conservative reading:
    /// a run loop with no response treats the intent as read-only so a
    /// mutation never slips through unconfirmed.
    async fn request_intent_confirmation(
        &self,
        _question: String,
        _options: &[RequestedOutcome],
        _cancel: CancellationToken,
    ) -> Option<RequestedOutcome> {
        None
    }

    /// Ask the user whether to apply a previously approved plan or replan
    /// first (ADR-55 Phase 1d).
    ///
    /// Called when an action-required Execute request matches a stored plan
    /// binding for the same objective. `Some(Apply)` authorizes running the
    /// stored plan (grants filesystem + git like a confirmed Execute);
    /// `Some(Replan)` keeps the run read-only and plans anew; the default
    /// `None` (no confirmation surface available, or the dialog was dismissed)
    /// keeps every existing sink unchanged and is the conservative reading —
    /// a run loop with no response treats the request as read-only so a
    /// mutation never slips through.
    ///
    /// `session_id` lets multi-session frontends route the prompt back to the
    /// window it belongs to; `plan_id` is the binding identifier reported in
    /// the audit row; `plan_text` is the stored (capped) plan body the dialog
    /// renders so the decision is made against the actual plan, not a summary;
    /// `created_at` is when the plan was recorded (UTC) so the dialog can show
    /// a relative age (e.g. "made 5m ago").
    async fn request_plan_approval(
        &self,
        _session_id: Ulid,
        _plan_id: &str,
        _question: String,
        _plan_text: &str,
        _created_at: OffsetDateTime,
        _cancel: CancellationToken,
    ) -> Option<PlanDecision> {
        None
    }
}
