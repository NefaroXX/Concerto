//! Pure state-transition and guard predicates for execution obligations.
//!
//! This module owns the *value-in / value-out* half of the obligation model:
//! mapping a graph [`SubTaskStatus`] onto the obligation lifecycle, validating
//! a single [`ObligationEvent`] transition without touching any state, and the
//! module-level dispatch-guard predicate. Nothing here mutates an
//! [`ObligationLedger`] — the parent module's `sync_from_graph` and `apply`
//! call these functions with state they already hold, and the coordinator's
//! combined guard derives its ledger first, then passes it in.
//!
//! Stability contract: guard outcomes, the zero-work / vacuous disarm, and
//! every reject `reason` string are observable behavior (the coordinator
//! surfaces transition errors as structured tool errors the model reads), so
//! the function bodies and their reject reasons move verbatim. The failure
//! tests below pin the already-outstanding, undispatched, completed, and
//! blocked edges.

use concerto_core::types::SubTaskStatus;

use super::{
    ObligationEvent, ObligationKind, ObligationLedger, ObligationState, ObligationTransitionError,
};

/// Map a graph subtask status onto the obligation lifecycle. `Completed` is
/// settled; `Failed` is terminal-but-retryable; `Declared` (obligation
/// declared via `declare_obligations`, not yet dispatched) is Outstanding —
/// enforceable before any dispatch, never auto-executed; every other status
/// is work the run has not settled yet. There is no graph-native "superseded"
/// — that transition lives on the ledger (reconsider/split/merge), never in
/// a status column.
pub fn obligation_state_for_status(status: SubTaskStatus) -> ObligationState {
    match status {
        SubTaskStatus::Completed => ObligationState::Completed,
        SubTaskStatus::Blocked => ObligationState::Blocked,
        SubTaskStatus::Failed => ObligationState::Failed,
        SubTaskStatus::Declared
        | SubTaskStatus::Pending
        | SubTaskStatus::Running
        | SubTaskStatus::AwaitingReview
        | SubTaskStatus::NeedsRevision => ObligationState::Outstanding,
        _ => ObligationState::Outstanding,
    }
}

/// Validate one transition without mutating anything. Pure: unit-testable
/// without a graph, a checkpoint, or a provider.
pub fn try_transition(
    current: ObligationState,
    event: &ObligationEvent,
    kind: ObligationKind,
    obligation_id: &str,
) -> Result<ObligationState, ObligationTransitionError> {
    let reject = |reason: &str| {
        Err(ObligationTransitionError {
            obligation_id: obligation_id.to_owned(),
            from: current,
            reason: reason.to_owned(),
        })
    };
    match (current, event) {
        (ObligationState::Completed, _) | (ObligationState::Superseded, _) => {
            reject("terminal obligations accept no further transitions")
        }
        (ObligationState::Outstanding, ObligationEvent::Complete { evidence }) => {
            if !kind.is_prose_satisfiable() && evidence.is_empty() {
                return reject(
                    "execution obligations require evidence; prose never marks them complete",
                );
            }
            Ok(ObligationState::Completed)
        }
        (ObligationState::Outstanding, ObligationEvent::Block { .. }) => {
            Ok(ObligationState::Blocked)
        }
        (ObligationState::Outstanding, ObligationEvent::Fail { .. }) => Ok(ObligationState::Failed),
        (ObligationState::Outstanding, ObligationEvent::Supersede { .. }) => {
            Ok(ObligationState::Superseded)
        }
        (ObligationState::Outstanding, ObligationEvent::Retry) => {
            reject("nothing to retry: the obligation is already outstanding")
        }
        (ObligationState::Blocked, ObligationEvent::Retry) => Ok(ObligationState::Outstanding),
        (ObligationState::Blocked, ObligationEvent::Fail { .. }) => Ok(ObligationState::Failed),
        (ObligationState::Blocked, ObligationEvent::Supersede { .. }) => {
            Ok(ObligationState::Superseded)
        }
        (ObligationState::Blocked, _) => {
            reject("blocked obligations must be retried before they can complete")
        }
        (ObligationState::Failed, ObligationEvent::Retry) => Ok(ObligationState::Outstanding),
        (ObligationState::Failed, ObligationEvent::Supersede { .. }) => {
            Ok(ObligationState::Superseded)
        }
        (ObligationState::Failed, _) => {
            reject("failed obligations must be retried before they can complete")
        }
    }
}

/// Whether the dispatch guards must arm for this run state: the legacy mode
/// predicate *or* an open execution obligation. Either source arms; neither
/// disarms the other's. Module-level form of the combined predicate — the
/// coordinator's `dispatch_guard_arms` arms on these two terms plus a
/// promised plan that produced no code artifact.
pub fn dispatch_guard_arms(mode_requires_dispatch: bool, ledger: &ObligationLedger) -> bool {
    mode_requires_dispatch || ledger.has_open_execution_work()
}

#[cfg(test)]
mod tests {
    use super::*;
    use concerto_core::ids::Ulid;
    use concerto_core::types::{AgentId, SubTask};

    use crate::graph::TaskGraph;

    /// Ledger derived the way production derives it: a graph holding one
    /// pending implement subtask yields one open execution obligation.
    fn ledger_with_open_implement() -> ObligationLedger {
        let mut graph = TaskGraph::new();
        let mut task = SubTask::new(Ulid::new(), AgentId::new("coder"), "fix the bug");
        task.status = SubTaskStatus::Pending;
        graph.add_root(task);
        let mut ledger = ObligationLedger::new();
        ledger.sync_from_graph(&graph, |_| Some(ObligationKind::Implement));
        ledger
    }

    /// Undispatched edge: a declared obligation is enforceable before any
    /// dispatch — Outstanding, never auto-executed and never dropped.
    #[test]
    fn undispatched_declared_status_derives_outstanding() {
        assert_eq!(
            obligation_state_for_status(SubTaskStatus::Declared),
            ObligationState::Outstanding,
            "declared (not-yet-dispatched) work stays enforceable"
        );
        assert_eq!(
            obligation_state_for_status(SubTaskStatus::Pending),
            ObligationState::Outstanding
        );
        assert_eq!(
            obligation_state_for_status(SubTaskStatus::Completed),
            ObligationState::Completed
        );
        assert_eq!(obligation_state_for_status(SubTaskStatus::Blocked), ObligationState::Blocked);
    }

    /// Already-outstanding edge: retry is a re-arm, not a no-op — an open
    /// obligation rejects it instead of silently re-cutting state.
    #[test]
    fn already_outstanding_obligation_rejects_retry() {
        let err = try_transition(
            ObligationState::Outstanding,
            &ObligationEvent::Retry,
            ObligationKind::Implement,
            "impl-1",
        )
        .expect_err("an outstanding obligation has nothing to retry");
        assert_eq!(err.reason, "nothing to retry: the obligation is already outstanding");
        assert_eq!(err.from, ObligationState::Outstanding);
        assert_eq!(err.obligation_id, "impl-1");
    }

    /// Completed edge: terminal states reject every event, with the same
    /// reject reason the coordinator surfaces as a structured tool error.
    #[test]
    fn completed_obligation_is_terminal_for_every_event() {
        let events = [
            ObligationEvent::Retry,
            ObligationEvent::Block { reason: "x".into() },
            ObligationEvent::Fail { reason: "y".into() },
            ObligationEvent::Complete { evidence: vec!["e".into()] },
            ObligationEvent::Supersede { by: "reconsidered".into() },
        ];
        for event in events {
            let err = try_transition(
                ObligationState::Completed,
                &event,
                ObligationKind::Implement,
                "impl-1",
            )
            .expect_err("completed work is terminal");
            assert_eq!(err.reason, "terminal obligations accept no further transitions");
        }
        // Evidenceless execution completion stays rejected on an open
        // obligation; prose settles only the prose-satisfiable kind.
        let rejected = try_transition(
            ObligationState::Outstanding,
            &ObligationEvent::Complete { evidence: Vec::new() },
            ObligationKind::Implement,
            "impl-1",
        )
        .expect_err("prose never marks execution complete");
        assert_eq!(
            rejected.reason,
            "execution obligations require evidence; prose never marks them complete"
        );
        assert_eq!(
            try_transition(
                ObligationState::Outstanding,
                &ObligationEvent::Complete { evidence: Vec::new() },
                ObligationKind::Explain,
                "explain-1",
            ),
            Ok(ObligationState::Completed),
            "an explanation settles in prose on its own"
        );
    }

    /// Blocked edge: blocked work must be retried first, and retry re-arms it
    /// as Outstanding while fail/supersede remain reachable.
    #[test]
    fn blocked_obligation_needs_retry_before_completion() {
        let err = try_transition(
            ObligationState::Blocked,
            &ObligationEvent::Complete { evidence: vec!["e".into()] },
            ObligationKind::Implement,
            "impl-1",
        )
        .expect_err("blocked work cannot complete without an explicit retry");
        assert_eq!(err.reason, "blocked obligations must be retried before they can complete");
        assert_eq!(
            try_transition(
                ObligationState::Blocked,
                &ObligationEvent::Retry,
                ObligationKind::Implement,
                "impl-1",
            ),
            Ok(ObligationState::Outstanding)
        );
        assert_eq!(
            try_transition(
                ObligationState::Blocked,
                &ObligationEvent::Fail { reason: "still red".into() },
                ObligationKind::Implement,
                "impl-1",
            ),
            Ok(ObligationState::Failed)
        );
    }

    /// Zero-work / vacuous semantics: with neither the mode term nor an open
    /// obligation the guard stays disarmed, and either source arms alone.
    #[test]
    fn guard_arms_on_either_term_and_disarms_on_zero_work() {
        assert!(
            !dispatch_guard_arms(false, &ObligationLedger::new()),
            "a vacuous turn owes no dispatch"
        );
        assert!(
            dispatch_guard_arms(true, &ObligationLedger::new()),
            "the legacy mode term arms on its own"
        );
        assert!(
            dispatch_guard_arms(false, &ledger_with_open_implement()),
            "open execution work arms without ActionRequired mode"
        );
    }
}
