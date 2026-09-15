//! Issue #63: explicit WAIT decisions and wake conditions.
//!
//! A Coordinator may pause the work on an external condition instead of
//! burning model turns or being misread as a stalled no-progress loop. This
//! module provides:
//!
//! - the *wake-condition* vocabulary ([`WakeCondition`]) and the journaled
//!   record of a single wait ([`WaitingRecord`]),
//! - a **pure**, total evaluation function ([`check_wake`]) that decides
//!   whether a wait should continue, wake, or expire given an *injected*
//!   snapshot of the world ([`WakeState`]) — no I/O, no wall-clock reads,
//!   fully deterministic for equal inputs, and therefore trivially testable,
//! - a bounded, cancellable executor ([`execute_wait`]) that re-evaluates
//!   after each of a series of short sleep slices until a condition trips, a
//!   deadline passes, the absolute wait cap is reached, or the run is
//!   cancelled.
//!
//! # Discipline
//!
//! - **Pure evaluation**: [`check_wake`] never reads time or state itself;
//!   both arrive via [`WakeState`]. Executors (which may legitimately read
//!   the wall clock) translate the world into a [`WakeView`] through a
//!   [`WakeRefiner`].
//! - **Bounded sleep**: every wait has an absolute upper bound
//!   ([`MAX_WAIT_MS`] unless a shorter `max_wait_ms` is configured) and every
//!   slice is clamped to [`MAX_WAIT_SLICE_MS`], keeping cancellation latency
//!   and stall-flagging latency bounded.
//! - **Cancellable**: cancellation is observed before the first slice and via
//!   `tokio::select!` during every slice, so a cancelled run aborts the wait
//!   immediately.
//! - **Deadline ≠ trigger**: a deadline is recorded separately from
//!   conditions; only a deadline can yield [`WakeOutcome::Expired`], while a
//!   condition (or replan/cancel) yields a [`WakeOutcome::Woken`]. Reaching
//!   the *internal cap* (no deadline involved) returns
//!   [`WakeOutcome::StillWaiting`] so the model can re-decide or re-wait
//!   rather than being told its deadline expired.

use std::collections::HashSet;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use concerto_core::types::{SubTaskStatus, TaskId};

/// Default sleep slice between wake re-evaluations.
pub const DEFAULT_WAIT_SLICE_MS: u64 = 4_000;
/// Upper bound for a single sleep slice ([`WaitConfig::slice_ms`] is clamped
/// to this) — keeps cancellation latency small even on a slow refiner.
pub const MAX_WAIT_SLICE_MS: u64 = 10_000;
/// Absolute lifetime cap for a wait that has no explicit deadline
/// ([`WaitConfig::max_wait_ms`] is clamped to this). After the cap the wait
/// returns [`WakeOutcome::StillWaiting`] and the model re-decides.
pub const MAX_WAIT_MS: u64 = 15 * 60 * 1000;
/// Maximum number of wake conditions a single wait may carry (structural
/// bound; the handler also rejects empty/degenerate conditions).
pub const MAX_WAIT_CONDITIONS: usize = 4;
/// Maximum number of affected task/resource ids a single wait may name.
pub const MAX_WAIT_AFFECTED_IDS: usize = 32;
/// Maximum number of event kinds a `NewEvidence` condition may name.
pub const MAX_WAIT_EVIDENCE_KINDS: usize = 8;

/// A declarative condition that wakes a wait (issue #63).
///
/// Tagged kebab-case on the wire so the journaled decision stays stable and
/// additive — a newer editor may append new condition shapes without breaking
/// older readers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum WakeCondition {
    /// Wake when every listed task has left the queue/run states — it settled
    /// in [`SubTaskStatus::Completed`], [`SubTaskStatus::Failed`],
    /// [`SubTaskStatus::Blocked`], or review.
    EventResolved { task_ids: Vec<TaskId> },
    /// Wake when the workspace-digest generation differs from the generation
    /// captured when the wait began.
    WorkspaceGenerationChanged { generation_at_wait: Option<String> },
    /// Wake when a whiteboard event of one of these kinds is appended after
    /// the wait began.
    NewEvidence { event_kinds: Vec<String> },
    /// Wake unconditionally at the next re-evaluation. Carries both meanings
    /// it can have: the plan was replanned (a newer `Replan` decision is on
    /// the journal), or the run was cancelled (the executor observes that).
    ReplanOrCancel,
}

/// A journaled WAIT decision (issue #63).
///
/// The deadline is a field of the record, **not** a [`WakeCondition`]: a
/// deadline is a hard bound that yields [`WakeOutcome::Expired`], while
/// conditions are satisfiable triggers that yield
/// [`WakeOutcome::Woken`]. The record is the persistence unit — an in-flight
/// wait is checkpointed whole and re-evaluated on resume.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WaitingRecord {
    /// The whiteboard decision id that created this wait.
    pub decision_id: String,
    /// Model-supplied reason (bounded by the caller).
    pub reason: String,
    /// Real whiteboard event ids cited by the wait decision.
    #[serde(default)]
    pub supporting_evidence_ids: Vec<String>,
    /// The conditions that must hold for the wait to end early.
    #[serde(default)]
    pub conditions: Vec<WakeCondition>,
    /// Tasks this wait is parked on (informational; also the resolution
    /// targets of `EventResolved`).
    #[serde(default)]
    pub affected_task_ids: Vec<TaskId>,
    /// Workspace-root-relative artifact paths this wait is parked on
    /// (informational — the wait itself performs no I/O).
    #[serde(default)]
    pub affected_resource_ids: Vec<String>,
    /// Wall-clock ms (UNIX epoch) when the wait began. Awaiter-side input —
    /// never read from inside pure evaluation.
    #[serde(default)]
    pub started_at_ms: i64,
    /// Optional hard deadline, absolute UNIX ms. Crossing it yields
    /// [`WakeOutcome::Expired`] even when no condition tripped.
    #[serde(default)]
    pub deadline_ms: Option<i64>,
    /// Whiteboard gate sequence captured at wait start, used to detect
    /// "new evidence" appended *after* we began waiting (resume-safe).
    #[serde(default)]
    pub start_gate_seq: Option<u64>,
}

impl WaitingRecord {
    /// The absolute moment (UNIX ms) past which this wait must not sleep —
    /// the earlier of the record's deadline and the started-at + cap bound.
    pub fn hard_bound_ms(&self, cap_ms: u64) -> i64 {
        let capped = self.started_at_ms.saturating_add(cap_ms as i64);
        self.deadline_ms.unwrap_or(i64::MAX).min(capped)
    }
}

/// A task counts as *resolved* — for `EventResolved` purposes — when it is no
/// longer queued or running; it settled in one of the terminal-ish states.
pub fn task_is_resolved(status: &SubTaskStatus) -> bool {
    !matches!(status, SubTaskStatus::Pending | SubTaskStatus::Running)
}

/// Result of a single wake evaluation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WakeOutcome {
    /// Neither a condition nor the deadline tripped; keep waiting.
    StillWaiting,
    /// A condition tripped, or the run was cancelled.
    Woken(WakeReason),
    /// The wait's deadline passed unsatisfied — the model must reconsider.
    Expired,
}

/// Why a wait ended early.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WakeReason {
    /// An `EventResolved` condition fired.
    EventResolved,
    /// The workspace generation changed from the value at wait start.
    WorkspaceGenerationChanged,
    /// A `NewEvidence` condition fired.
    NewEvidence,
    /// A replan superseded the wait (or cancel was signalled).
    ReplanOrCancel,
    /// The run's cancellation token fired.
    Cancelled,
}

/// An injected, immutable view of the world used for pure evaluation.
///
/// Borrows the caller's owned snapshot; construct via [`WakeState::from_view`]
/// or directly in tests.
#[derive(Debug)]
pub struct WakeState<'a> {
    /// Task ids currently resolved (see [`task_is_resolved`]).
    pub resolved_task_ids: &'a HashSet<TaskId>,
    /// Current workspace-digest generation, if one is tracked.
    pub workspace_generation: Option<&'a str>,
    /// Kinds (kebab-case strings) of whiteboard events appended since the
    /// wait's `start_gate_seq`.
    pub new_evidence_kinds: &'a [String],
    /// Whether a decision-journal `Replan` superseded this wait.
    pub replan_signaled: bool,
    /// Injected "now" in UNIX ms — the evaluator never reads the clock.
    pub now_ms: i64,
}

/// An owned snapshot produced by a [`WakeRefiner`] per evaluation slice.
#[derive(Debug, Clone, Default)]
pub struct WakeView {
    /// Task ids currently resolved.
    pub resolved_task_ids: HashSet<TaskId>,
    /// Current workspace-digest generation.
    pub workspace_generation: Option<String>,
    /// Kinds of whiteboard events appended since the wait began.
    pub new_evidence_kinds: Vec<String>,
    /// Whether a decision-journal `Replan` superseded this wait.
    pub replan_signaled: bool,
    /// Injected "now" in UNIX ms.
    pub now_ms: i64,
}

impl<'a> WakeState<'a> {
    /// Borrow a [`WakeView`] for pure evaluation.
    pub fn from_view(view: &'a WakeView) -> Self {
        Self {
            resolved_task_ids: &view.resolved_task_ids,
            workspace_generation: view.workspace_generation.as_deref(),
            new_evidence_kinds: &view.new_evidence_kinds,
            replan_signaled: view.replan_signaled,
            now_ms: view.now_ms,
        }
    }
}

/// Evaluates one wait against an injected snapshot. **Pure and total**: no
/// I/O, no wall-clock reads, no sleeps — deterministic for equal inputs.
///
/// Evaluation order:
/// 1. `ReplanOrCancel` (a superseding plan never waits behind the old one),
/// 2. `EventResolved`,
/// 3. `WorkspaceGenerationChanged`,
/// 4. `NewEvidence`,
/// 5. deadline expiry ([`WakeOutcome::Expired`]),
/// 6. otherwise [`WakeOutcome::StillWaiting`].
pub fn check_wake(record: &WaitingRecord, state: &WakeState<'_>) -> WakeOutcome {
    if state.replan_signaled {
        return WakeOutcome::Woken(WakeReason::ReplanOrCancel);
    }
    for condition in &record.conditions {
        match condition {
            WakeCondition::ReplanOrCancel => {
                return WakeOutcome::Woken(WakeReason::ReplanOrCancel);
            }
            WakeCondition::EventResolved { task_ids } => {
                if task_ids.iter().all(|id| state.resolved_task_ids.contains(id)) {
                    return WakeOutcome::Woken(WakeReason::EventResolved);
                }
            }
            WakeCondition::WorkspaceGenerationChanged { generation_at_wait } => {
                if generation_at_wait.as_deref() != state.workspace_generation {
                    return WakeOutcome::Woken(WakeReason::WorkspaceGenerationChanged);
                }
            }
            WakeCondition::NewEvidence { event_kinds } => {
                if event_kinds.iter().any(|kind| state.new_evidence_kinds.contains(kind)) {
                    return WakeOutcome::Woken(WakeReason::NewEvidence);
                }
            }
        }
    }
    if let Some(deadline) = record.deadline_ms {
        if state.now_ms >= deadline {
            return WakeOutcome::Expired;
        }
    }
    WakeOutcome::StillWaiting
}

/// Tuning knobs for the bounded sleep in [`execute_wait`].
#[derive(Debug, Clone, Copy)]
pub struct WaitConfig {
    /// Sleep slice between re-evaluations, clamped to `1..=MAX_WAIT_SLICE_MS`.
    pub slice_ms: u64,
    /// Absolute lifetime cap for a wait with no explicit deadline, clamped to
    /// `1..=MAX_WAIT_MS`. `None` means the default [`MAX_WAIT_MS`].
    pub max_wait_ms: Option<u64>,
}

impl Default for WaitConfig {
    fn default() -> Self {
        Self { slice_ms: DEFAULT_WAIT_SLICE_MS, max_wait_ms: Some(MAX_WAIT_MS) }
    }
}

/// Produces the owned snapshot evaluated after each sleep slice.
///
/// Kept a generic-bound trait — not a `dyn` object — because its method is
/// `async fn` (a `#[async_trait]`-free object-safe async-trait object is not
/// available on this edition) and because the refiner typically borrows the
/// coordinator across an await in the decision loop.
///
/// `async_fn_in_trait` is intentionally allowed: [`execute_wait`] awaits the
/// future inline in the same task (no `Send` bound needed) and the trait is
/// never used as a trait object, so the dyn-compatibility loss lint flags is
/// irrelevant here.
#[allow(async_fn_in_trait)]
pub trait WakeRefiner {
    /// Build the current snapshot of the world relevant to this wait.
    async fn refine(&self) -> WakeView;
}

/// Sleep in bounded slices, re-evaluating the wait after each slice, until a
/// condition trips, the deadline passes, the absolute cap is reached, or the
/// cancellation token fires.
///
/// Guarantees:
/// - **Bounded**: total sleeping is capped at
///   `min(deadline − started_at, max_wait_ms)`; each slice is clamped to
///   [`MAX_WAIT_SLICE_MS`];
/// - **Cancellable**: cancellation is honored before the first slice and via
///   `tokio::select!` during every slice (aborts sleep immediately);
/// - **Prompt on pre-satisfaction**: a condition that already holds is
///   honored with zero sleep (the first refine happens before any sleep);
/// - **Honest cap success**: reaching the *internal cap* — never the model's
///   deadline — yields [`WakeOutcome::StillWaiting`], so the model re-decides
///   or re-waits instead of being told its deadline expired.
pub async fn execute_wait<R: WakeRefiner>(
    record: &WaitingRecord,
    refiner: &R,
    config: &WaitConfig,
    cancel: &CancellationToken,
) -> WakeOutcome {
    if cancel.is_cancelled() {
        return WakeOutcome::Woken(WakeReason::Cancelled);
    }
    let cap_ms = config.max_wait_ms.unwrap_or(MAX_WAIT_MS).min(MAX_WAIT_MS);
    let hard_bound_ms = record.hard_bound_ms(cap_ms);
    let slice_ms = config.slice_ms.clamp(1, MAX_WAIT_SLICE_MS);

    loop {
        let view = refiner.refine().await;
        let outcome = check_wake(record, &WakeState::from_view(&view));
        if !matches!(outcome, WakeOutcome::StillWaiting) {
            return outcome;
        }
        let remaining_ms = hard_bound_ms.saturating_sub(view.now_ms);
        if remaining_ms <= 0 {
            // Internal cap (not a model deadline) reached unsatisfied: hand
            // control back so the model can re-decide or re-wait.
            return WakeOutcome::StillWaiting;
        }
        let sleep_ms = slice_ms.min(remaining_ms as u64);
        tokio::select! {
            _ = cancel.cancelled() => return WakeOutcome::Woken(WakeReason::Cancelled),
            _ = tokio::time::sleep(Duration::from_millis(sleep_ms)) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use concerto_core::ids::Ulid;

    use super::*;

    /// Deterministic task id from a short literal (tests read better with
    /// `"T1"` than with a 26-char ULID). The seed must be non-zero so ids of
    /// distinct names never collide in practice.
    fn task(name: &str) -> TaskId {
        let seed: u128 =
            name.bytes().fold(0u128, |acc, b| acc.wrapping_mul(131).wrapping_add(u128::from(b)));
        TaskId(Ulid::from(seed))
    }

    fn record(conditions: Vec<WakeCondition>) -> WaitingRecord {
        WaitingRecord {
            decision_id: "dec-1".to_string(),
            reason: "parked".to_string(),
            supporting_evidence_ids: Vec::new(),
            conditions,
            affected_task_ids: Vec::new(),
            affected_resource_ids: Vec::new(),
            started_at_ms: 1_000,
            deadline_ms: None,
            start_gate_seq: Some(42),
        }
    }

    fn view(
        resolved: &[&str],
        generation: Option<&str>,
        new_kinds: &[&str],
        replan: bool,
        now_ms: i64,
    ) -> WakeView {
        WakeView {
            resolved_task_ids: resolved.iter().map(|id| task(id)).collect(),
            workspace_generation: generation.map(String::from),
            new_evidence_kinds: new_kinds.iter().map(|s| s.to_string()).collect(),
            replan_signaled: replan,
            now_ms,
        }
    }

    /// Evaluate against an owned view, mirroring how the executor evaluates.
    fn check(record: &WaitingRecord, v: &WakeView) -> WakeOutcome {
        check_wake(record, &WakeState::from_view(v))
    }

    // ---- pure evaluation ------------------------------------------------

    #[test]
    fn event_resolved_wakes_when_all_tasks_resolved() {
        let w =
            record(vec![WakeCondition::EventResolved { task_ids: vec![task("T1"), task("T2")] }]);
        let s = view(&["T1", "T2"], None, &[], false, 5);
        assert_eq!(check(&w, &s), WakeOutcome::Woken(WakeReason::EventResolved));
    }

    #[test]
    fn event_resolved_still_waiting_while_any_task_runs() {
        let w =
            record(vec![WakeCondition::EventResolved { task_ids: vec![task("T1"), task("T2")] }]);
        let s = view(&["T1"], None, &[], false, 5);
        assert_eq!(check(&w, &s), WakeOutcome::StillWaiting);
    }

    #[test]
    fn workspace_generation_change_wakes() {
        let w = record(vec![WakeCondition::WorkspaceGenerationChanged {
            generation_at_wait: Some("gen-1".to_string()),
        }]);
        assert_eq!(
            check(&w, &view(&[], Some("gen-2"), &[], false, 5)),
            WakeOutcome::Woken(WakeReason::WorkspaceGenerationChanged)
        );
        // Same generation stays parked.
        assert_eq!(check(&w, &view(&[], Some("gen-1"), &[], false, 5)), WakeOutcome::StillWaiting);
    }

    #[test]
    fn new_evidence_kind_wakes() {
        let w = record(vec![WakeCondition::NewEvidence {
            event_kinds: vec!["review-state".to_string()],
        }]);
        assert_eq!(
            check(&w, &view(&[], None, &["review-state"], false, 5)),
            WakeOutcome::Woken(WakeReason::NewEvidence)
        );
        assert_eq!(check(&w, &view(&[], None, &["decision"], false, 5)), WakeOutcome::StillWaiting);
    }

    #[test]
    fn replan_signaled_wakes_before_any_condition() {
        let w = record(vec![WakeCondition::NewEvidence {
            event_kinds: vec!["review-state".to_string()],
        }]);
        // Not satisfied by evidence, but the plan was superseded.
        assert_eq!(
            check(&w, &view(&[], None, &[], true, 5)),
            WakeOutcome::Woken(WakeReason::ReplanOrCancel)
        );
    }

    #[test]
    fn explicit_replan_condition_wakes_unconditionally() {
        let w = record(vec![WakeCondition::ReplanOrCancel]);
        assert_eq!(
            check(&w, &view(&[], None, &[], false, 5)),
            WakeOutcome::Woken(WakeReason::ReplanOrCancel)
        );
    }

    #[test]
    fn deadline_expiry_returns_expired() {
        let w = record(vec![]);
        let w = WaitingRecord { deadline_ms: Some(10), ..w };
        assert_eq!(check(&w, &view(&[], None, &[], false, 10)), WakeOutcome::Expired);
        // Just before the deadline it is still a wait.
        assert_eq!(check(&w, &view(&[], None, &[], false, 9)), WakeOutcome::StillWaiting);
    }

    #[test]
    fn expiry_is_checked_even_when_conditions_are_unsatisfied() {
        let w = record(vec![WakeCondition::NewEvidence {
            event_kinds: vec!["review-state".to_string()],
        }]);
        let w = WaitingRecord { deadline_ms: Some(3), ..w };
        assert_eq!(check(&w, &view(&[], None, &[], false, 5)), WakeOutcome::Expired);
    }

    #[test]
    fn no_condition_no_deadline_still_waiting() {
        let w = record(vec![]);
        assert_eq!(check(&w, &view(&[], None, &[], false, 5)), WakeOutcome::StillWaiting);
    }

    #[test]
    fn task_is_resolved_matches_terminal_states() {
        use concerto_core::types::SubTaskStatus::*;
        assert!(task_is_resolved(&Completed));
        assert!(task_is_resolved(&Failed));
        assert!(task_is_resolved(&Blocked));
        assert!(task_is_resolved(&AwaitingReview));
        assert!(!task_is_resolved(&Pending));
        assert!(!task_is_resolved(&Running));
    }

    // ---- wire shape -----------------------------------------------------

    #[test]
    fn wake_condition_serializes_kebab_case_tagged() {
        let c = WakeCondition::NewEvidence { event_kinds: vec!["review-state".to_string()] };
        let json = serde_json::to_string(&c).expect("serializes");
        assert_eq!(json, r#"{"type":"new-evidence","event_kinds":["review-state"]}"#);
        let rt: WakeCondition = serde_json::from_str(&json).expect("round-trips");
        assert_eq!(rt, c);
    }

    #[test]
    fn waiting_record_round_trips_with_defaulted_new_fields() {
        let w = record(vec![WakeCondition::EventResolved { task_ids: vec![task("T1")] }]);
        let bytes = serde_json::to_vec(&w).expect("serializes");
        let back: WaitingRecord = serde_json::from_slice(&bytes).expect("round-trips");
        assert_eq!(back, w);
        // Old (pre-63) journals lack the new fields; the serde defaults must
        // load them as an empty-but-valid record.
        let legacy = r#"{"decision_id":"dec-1","reason":"parked"}"#;
        let loaded: WaitingRecord = serde_json::from_str(legacy).expect("legacy record loads");
        assert_eq!(loaded.conditions, Vec::new());
        assert_eq!(loaded.started_at_ms, 0);
    }

    // ---- executor ------------------------------------------------------

    /// A refiner whose snapshot can be mutated between slices, driven by call
    /// count and/or wall-clock progression.
    struct ScriptedRefiner {
        calls: Mutex<u32>,
        /// (calls to stay still, then flip to woken). `None` never wakes.
        flip_after: Option<u32>,
        resolved: HashSet<TaskId>,
        mutation: i64,
    }

    impl ScriptedRefiner {
        fn woken_after(calls: u32, resolved: HashSet<TaskId>) -> Self {
            Self { calls: Mutex::new(0), flip_after: Some(calls), resolved, mutation: 0 }
        }

        fn advancing_snapshot(per_call_ms: i64) -> Self {
            Self {
                calls: Mutex::new(0),
                flip_after: None,
                resolved: HashSet::new(),
                mutation: per_call_ms,
            }
        }

        fn next_view(&self) -> WakeView {
            let mut n = self.calls.lock().expect("refiner lock");
            *n += 1;
            let flip = self.flip_after.map(|f| *n >= f).unwrap_or(false);
            WakeView {
                resolved_task_ids: self.resolved.clone(),
                workspace_generation: None,
                new_evidence_kinds: (if flip {
                    vec!["review-state".to_string()]
                } else {
                    Vec::new()
                }),
                replan_signaled: false,
                now_ms: 1_000 + self.mutation.saturating_mul(i64::from(*n)),
            }
        }
    }

    impl WakeRefiner for ScriptedRefiner {
        async fn refine(&self) -> WakeView {
            self.next_view()
        }
    }

    #[tokio::test]
    async fn execute_wait_wakes_with_zero_sleep_when_pre_satisfied() {
        let w = record(vec![WakeCondition::EventResolved { task_ids: vec![task("T1")] }]);
        let refiner = ScriptedRefiner::woken_after(1, HashSet::from([task("T1")]));
        let started = std::time::Instant::now();
        let outcome = execute_wait(
            &w,
            &refiner,
            &WaitConfig { slice_ms: 10_000, max_wait_ms: Some(100_000) },
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(outcome, WakeOutcome::Woken(WakeReason::EventResolved));
        assert!(started.elapsed().as_millis() < 1_000, "pre-satisfied wait slept");
    }

    #[tokio::test]
    async fn execute_wait_wakes_on_subsequent_slice() {
        let w = record(vec![WakeCondition::NewEvidence {
            event_kinds: vec!["review-state".to_string()],
        }]);
        let refiner = ScriptedRefiner::woken_after(3, HashSet::new());
        let started = std::time::Instant::now();
        let outcome = execute_wait(
            &w,
            &refiner,
            &WaitConfig { slice_ms: 1, max_wait_ms: Some(30_000) },
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(outcome, WakeOutcome::Woken(WakeReason::NewEvidence));
        assert!(started.elapsed().as_millis() < 5_000, "slice wait overslept");
    }

    #[tokio::test]
    async fn execute_wait_expires_when_deadline_crosses() {
        let w = record(vec![]);
        let w = WaitingRecord { deadline_ms: Some(1_000 + 25), ..w };
        let refiner = ScriptedRefiner::advancing_snapshot(10);
        let started = std::time::Instant::now();
        let outcome = execute_wait(
            &w,
            &refiner,
            &WaitConfig { slice_ms: 1, max_wait_ms: Some(30_000) },
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(outcome, WakeOutcome::Expired);
        assert!(started.elapsed().as_millis() < 5_000, "deadline wait overslept");
    }

    #[tokio::test]
    async fn execute_wait_returns_still_waiting_after_internal_cap() {
        let w = record(vec![]);
        let refiner = ScriptedRefiner::advancing_snapshot(5);
        let started = std::time::Instant::now();
        let outcome = execute_wait(
            &w,
            &refiner,
            &WaitConfig { slice_ms: 1, max_wait_ms: Some(10) },
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(outcome, WakeOutcome::StillWaiting);
        assert!(started.elapsed().as_millis() < 5_000, "cap wait overslept");
    }

    #[tokio::test]
    async fn execute_wait_cancel_returns_cancelled() {
        let w = record(vec![]);
        let refiner = ScriptedRefiner::woken_after(u32::MAX, HashSet::new());
        let cancel = CancellationToken::new();
        let started = std::time::Instant::now();
        let outcome = execute_wait(
            &w,
            &refiner,
            &WaitConfig { slice_ms: 60_000, max_wait_ms: Some(120_000) },
            &cancel,
        );
        // Cancel while the (long) slice is sleeping.
        tokio::time::sleep(Duration::from_millis(20)).await;
        cancel.cancel();
        let outcome = outcome.await;
        assert_eq!(outcome, WakeOutcome::Woken(WakeReason::Cancelled));
        assert!(started.elapsed().as_millis() < 5_000, "cancel wait overslept");
    }

    #[tokio::test]
    async fn execute_wait_honours_pre_cancelled_token() {
        let w = record(vec![]);
        let refiner = ScriptedRefiner::woken_after(u32::MAX, HashSet::new());
        let cancel = CancellationToken::new();
        cancel.cancel();
        let started = std::time::Instant::now();
        let outcome = execute_wait(
            &w,
            &refiner,
            &WaitConfig { slice_ms: 60_000, max_wait_ms: Some(120_000) },
            &cancel,
        )
        .await;
        assert_eq!(outcome, WakeOutcome::Woken(WakeReason::Cancelled));
        assert!(started.elapsed().as_millis() < 1_000, "pre-cancelled wait slept");
    }
}
