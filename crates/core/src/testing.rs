//! Test harness helpers for `concerto-core`.
//!
//! Provides `ApprovalTestHarness` and other test utilities used across
//! multiple crates.

use crate::ids::Ulid;
use crate::traits::approval::{ApprovalDecision, ApprovalSink};
use crate::types::PolicyAction;
use crate::CancellationToken;
use async_trait::async_trait;
use std::collections::VecDeque;

/// A test double for the `ApprovalSink` trait with configurable decisions.
///
/// Used by orchestrator tests and UI tests that need to simulate user
/// approval flows without a real UI.
pub struct ApprovalTestHarness {
    /// Queue of decisions to return. Each call to `request_approval` pops
    /// the front. Defaults to `Approve` if the queue is empty.
    pub decisions: VecDeque<ApprovalDecision>,
    /// Whether `approve_all_for_session` was called.
    pub approve_all_called: bool,
}

impl ApprovalTestHarness {
    /// Create a harness that always approves.
    pub fn always_approve() -> Self {
        Self { decisions: VecDeque::new(), approve_all_called: false }
    }

    /// Create a harness that always denies.
    pub fn always_deny() -> Self {
        let mut decisions = VecDeque::new();
        decisions.push_back(ApprovalDecision::Deny);
        Self { decisions, approve_all_called: false }
    }

    /// Create a harness that returns decisions in sequence.
    pub fn sequence(decisions: Vec<ApprovalDecision>) -> Self {
        Self { decisions: VecDeque::from(decisions), approve_all_called: false }
    }
}

#[async_trait]
impl ApprovalSink for ApprovalTestHarness {
    async fn request_approval(
        &self,
        _action: &PolicyAction<'_>,
        _cancel: CancellationToken,
    ) -> ApprovalDecision {
        self.decisions.clone().into_iter().next().unwrap_or(ApprovalDecision::Approve)
    }

    async fn approve_all_for_session(&self, _session_id: Ulid, _cancel: CancellationToken) {
        // No-op in tests
    }

    async fn request_ack(&self, _message: &str, _cancel: CancellationToken) -> bool {
        true // auto-acknowledge in tests by default
    }
}
