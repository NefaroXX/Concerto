//! Progress-aware stall detection for the Coordinator's decision loop
//! (issue #53, parent #51).
//!
//! The single-agent loop already carries a continuation-round fingerprint
//! ([`crate::agent_loop`]'s `ProgressFingerprint`) and a blunt round
//! ceiling; the coordinator historically had neither a progress signal nor
//! a recovery path between "the model loops forever" and the structural
//! iteration bound. This module adds the coordinator-level complement:
//! per cycle — one full observe→decide→dispatch turn of the decision
//! loop — a compact fingerprint is computed from OBSERVABLE execution
//! state, and a run of consecutive equivalent fingerprints triggers a
//! deterministic, BOUNDED reconsideration instead of a hard stop.
//!
//! # Placement
//!
//! Coordinator-level machinery like [`crate::decisions`] (issue #52), but a
//! different concern: cycle observability, not decision validation. It
//! stays in its own module so the decision boundary and the progress
//! boundary remain independently inspectable, and so
//! [`crate::checkpoint`] can persist the state alongside (but separate
//! from) the execution accumulators. It mirrors — and deliberately does
//! not change — the single-agent loop's fingerprint logic.
//!
//! # Fingerprint semantics
//!
//! A cycle's fingerprint is the blake3 digest of the sorted, deduplicated
//! set of observable components that cycle produced:
//!
//! - `dispatch:{agent}:{intent-hash}` — a dispatch decision
//!   (pending→dispatched), with the task text canonicalized by the SAME
//!   normalizer the plan hasher uses ([`work_intent_hash`]), so
//!   re-spelled equivalent tasks hash equal;
//! - `outcome:{agent}:{label}` — a settlement (dispatched→settled),
//!   including structured tool errors (`error:{code}`), so a coordinator
//!   that repeatedly issues rejected decisions is as visible as one that
//!   repeats failing dispatches;
//! - `file:{agent}:{path}` — files the specialist reported modifying;
//! - `artifact:{path}` — paths the coordinator's own executor tools touched;
//! - `snapshot:{generation}` — the workspace snapshot generation after the
//!   cycle (changes only when the workspace objectively changed);
//! - `journal:{kind}:{status}` — new decision-journal entries this cycle
//!   (issue #52's journal provides these transitions for free);
//! - `plan:draft` — an advisory plan was (re-)drafted.
//!
//! Two cycles that did equivalent observable work — same dispatch intents,
//! same outcomes, same files, unchanged workspace — therefore hash EQUAL,
//! which is exactly the stall signature. Genuine progress (a different
//! intent, a different outcome, new artifacts, a workspace change) always
//! changes at least one component. That is the anti-false-positive
//! contract, pinned by tests: a failed operation followed by a valid
//! recovery MUST advance the fingerprint (differing outcome, new facts,
//! changed snapshot), and slow-but-valid progress never flags.
//!
//! # Noise exclusion (explicit)
//!
//! Timestamps, spend deltas (≈ token cost), token counts, and provider
//! latencies change on EVERY cycle — including stalled ones — so hashing
//! them would make equivalent states unhashable. [`CycleObservation`]
//! carries them for reporting (the reconsideration prompt cites the spend
//! wasted across the stall window) but the fingerprint never includes
//! them; tests pin that a fingerprint-neutral change leaves the
//! fingerprint untouched.
//!
//! # Recovery (bounded, deterministic — never a hard stop)
//!
//! On a stall the tracker emits a bounded reconsideration the caller
//! injects into the EXISTING decision-loop conversation (a replan nudge:
//! change the decomposition, the specialist, or the task — or conclude in
//! prose). Recovery is budgeted ([`MAX_STALL_RECOVERIES`]); once exhausted
//! the tracker escalates and the caller stops the loop with a recoverable
//! note — surfacing `Partial` through the existing note machinery, never a
//! hard error. The hard iteration/round ceilings remain untouched
//! complements, exactly as today.

use serde::{Deserialize, Serialize};

use crate::decisions::{DecisionKind, DecisionStatus};
use crate::fingerprint::work_intent_hash;

/// How many consecutive cycles with an equivalent fingerprint constitute a
/// stall. Three (a deliberate mirror of the single-agent loop's
/// `MAX_STALE_ROUNDS`): one repeat is an anomaly guard, three consecutive
/// equivalent cycles — same dispatch intents, outcomes, artifacts, and an
/// unchanged workspace — is a stall worth a bounded recovery.
pub const MAX_STALL_ROUNDS: u32 = 3;

/// Repeats beyond the first identical cycle before a stall fires: a run of
/// `MAX_STALL_ROUNDS` identical cycles reaches this many repeats.
const REPEATS_TO_STALL: u32 = MAX_STALL_ROUNDS - 1;

/// Bounded reconsideration budget: how many recovery prompts the tracker
/// may emit before escalation. Recovery itself must not be able to loop
/// forever — after this budget the next stall escalates (the caller stops
/// the loop through the existing note machinery).
pub const MAX_STALL_RECOVERIES: u32 = 2;

/// Bounded fingerprint history kept for persistence/debugging (the stall
/// rule needs only the previous fingerprint; the extra entries make the
/// restored state inspectable across a resume).
pub const MAX_FINGERPRINT_HISTORY: usize = 4;

/// One coordinator cycle's observable outcome, built by the caller from
/// the real execution state at the point a full observe→decide→dispatch
/// cycle completes. Pure input to [`ProgressTracker::observe`]; the
/// noise-carrying fields are documented component-wise.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CycleObservation {
    /// Dispatch decisions made this cycle: `(agent id, raw task text)`.
    /// Each is a pending→dispatched transition.
    pub dispatches: Vec<(String, String)>,
    /// Settlements this cycle: `(agent, outcome label, files modified)`.
    /// Structured tool errors settle as `error:{code}` so repeated
    /// rejected decisions are observable too. Each is a
    /// dispatched→settled transition.
    pub settlements: Vec<(String, String, Vec<String>)>,
    /// Paths the coordinator's own executor tools touched this cycle
    /// (newly appended to the ledger's file list).
    pub artifacts: Vec<String>,
    /// Workspace snapshot generation after the cycle. `None` when no
    /// snapshot governs the run — the component is then simply absent and
    /// equivalence rests on the other signals.
    pub snapshot_generation: Option<String>,
    /// Decision-journal entries NEWLY recorded this cycle, as
    /// `(kind label, status label)` per entry (issue #52's journal gives
    /// these transitions for free).
    pub journal_transitions: Vec<(String, String)>,
    /// Whether the advisory `draft_plan` tool was called this cycle.
    pub draft_plan: bool,
    /// NOISE (`f64` USD spend delta this cycle): tracked for the
    /// reconsideration prompt's wasted-spend report, never hashed — it
    /// changes on every cycle, stalled or not.
    pub spend_delta_usd: f64,
    /// NOISE (cycle timestamp, unix ms): carried for logging, never
    /// hashed.
    pub timestamp_ms: i64,
}

impl CycleObservation {
    /// The cycle's observable components, sorted and deduplicated (set
    /// semantics — the multiset counts within one cycle are incidental;
    /// cross-cycle equality is what the stall rule keys on).
    fn components(&self) -> Vec<String> {
        let mut components: Vec<String> = Vec::new();
        for (agent, task) in &self.dispatches {
            components.push(format!("dispatch:{}:{}", agent, work_intent_hash(task)));
        }
        for (agent, outcome, files) in &self.settlements {
            components.push(format!("outcome:{}:{}", agent, outcome));
            for path in files {
                components.push(format!("file:{}:{}", agent, path));
            }
        }
        for path in &self.artifacts {
            components.push(format!("artifact:{}", path));
        }
        if let Some(generation) = &self.snapshot_generation {
            components.push(format!("snapshot:{}", generation));
        }
        for (kind, status) in &self.journal_transitions {
            components.push(format!("journal:{}:{}", kind, status));
        }
        if self.draft_plan {
            components.push("plan:draft".to_owned());
        }
        components.sort();
        components.dedup();
        components
    }

    /// The compact cycle fingerprint: blake3 over the length-prefixed
    /// component set (mirroring the canonical-encoding convention of
    /// [`crate::fingerprint`]). Deterministic: equivalent cycles hash
    /// equal, progress of any kind differs in at least one component.
    pub fn fingerprint(&self) -> String {
        let mut buf = Vec::new();
        for component in &self.components() {
            push_length_prefixed(&mut buf, component);
        }
        blake3::hash(&buf).to_hex().to_string()
    }
}

/// Persistable stall-detection state: the bounded fingerprint history, the
/// equivalence streak, the recovery budget consumed so far, and the spend
/// wasted across the stall window (reporting only). Carried through
/// checkpoints ADDITIVELY (serde default): pre-#53 checkpoints load with
/// the zero-value state and old readers ignore the key.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ProgressTrackerState {
    /// The most recent cycle fingerprints (oldest first, bounded).
    #[serde(default)]
    pub fingerprint_history: Vec<String>,
    /// Consecutive repeats of the previous fingerprint (0 = the latest
    /// fingerprint differed from its predecessor).
    #[serde(default)]
    pub repeated_rounds: u32,
    /// Reconsideration prompts already emitted for this run's decision
    /// loop. Persisted so the recovery budget is not reset by a resume.
    #[serde(default)]
    pub stall_recoveries: u32,
    /// Spend observed across the stall window (USD, reporting only —
    /// never hashed). Reset whenever a differing fingerprint arrives.
    #[serde(default)]
    pub wasted_spend_usd: f64,
}

/// What the tracker decided after one cycle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CycleVerdict {
    /// Observable progress — keep going.
    Progressing,
    /// A stall was detected and a recovery attempt remains: the payload is
    /// the bounded reconsideration prompt to inject into the EXISTING
    /// decision-loop conversation (deterministic recovery — never a hard
    /// stop, never a new recovery machine).
    Reconsider(String),
    /// The recovery budget is exhausted and the stall persists: the
    /// payload is the escalation note. The caller stops the loop with it
    /// (recoverable note → `Partial` exit through the existing note
    /// machinery); the hard iteration ceilings remain as the final net.
    Escalate(String),
}

/// The coordinator progress tracker. Owned by [`crate::coordinator::
/// CoordinatorAgent`], mutated once per decision-loop cycle, and
/// persisted/restored via [`ProgressTrackerState`] so detection survives a
/// resume.
#[derive(Debug, Clone, Default)]
pub struct ProgressTracker {
    state: ProgressTrackerState,
}

impl ProgressTracker {
    pub fn new() -> Self {
        Self { state: ProgressTrackerState::default() }
    }

    /// Restore from checkpointed state (additive `progress_tracker`
    /// checkpoint field; absent on pre-#53 records → default).
    pub fn from_state(state: ProgressTrackerState) -> Self {
        let mut state = state;
        state.fingerprint_history.truncate(MAX_FINGERPRINT_HISTORY);
        Self { state }
    }

    pub fn state(&self) -> &ProgressTrackerState {
        &self.state
    }

    /// Observe one completed coordinator cycle. Deterministic: same
    /// observation sequence → same verdict sequence.
    pub fn observe(&mut self, observation: &CycleObservation) -> CycleVerdict {
        let fingerprint = observation.fingerprint();
        let equivalent = self.state.fingerprint_history.last() == Some(&fingerprint);
        self.state.fingerprint_history.push(fingerprint);
        let excess = self.state.fingerprint_history.len().saturating_sub(MAX_FINGERPRINT_HISTORY);
        self.state.fingerprint_history.drain(0..excess);

        if !equivalent {
            // Observable progress of some kind: the equivalence window and
            // the wasted-spend report reset, but the recovery budget does
            // NOT (it is capped per decision loop, not per progress spell).
            self.state.repeated_rounds = 0;
            self.state.wasted_spend_usd = 0.0;
            return CycleVerdict::Progressing;
        }

        self.state.repeated_rounds = self.state.repeated_rounds.saturating_add(1);
        if self.state.repeated_rounds < REPEATS_TO_STALL {
            return CycleVerdict::Progressing;
        }

        self.state.wasted_spend_usd += observation.spend_delta_usd;
        if self.state.stall_recoveries < MAX_STALL_RECOVERIES {
            self.state.stall_recoveries = self.state.stall_recoveries.saturating_add(1);
            // The reconsideration gets a fresh REPEAT counter (the history
            // tail keeps the stalled fingerprint, so equivalent work still
            // counts): a recovery that merely repeats the same work
            // re-reaches the threshold two cycles later and consumes the
            // remaining budget, while genuine recovery progress resets the
            // streak entirely.
            self.state.repeated_rounds = 0;
            CycleVerdict::Reconsider(reconsider_text())
        } else {
            // Budget exhausted: escalate. `repeated_rounds` stays at the
            // threshold, so further equivalent cycles keep escalating.
            CycleVerdict::Escalate(escalation_text())
        }
    }
}

/// The bounded reconsideration prompt injected on a detected stall
/// (deterministic — same trigger, same text).
fn reconsider_text() -> String {
    format!(
        "Progress guard: the last {MAX_STALL_ROUNDS} coordinator cycles produced \
         equivalent observable work (same dispatch intents, outcomes, and artifacts, \
         with an unchanged workspace). Before dispatching again, CHANGE your approach: \
         a different decomposition, a different specialist, or a materially different \
         task — or conclude in prose stating what is blocked and why. Repeating \
         equivalent work will stop the run."
    )
}

/// The escalation note for an exhausted recovery budget (deterministic).
/// Mirrors the existing structural-bound note discipline: the recorded
/// dispatches are preserved, the run exits through the recoverable-note
/// machinery — never a hard crash.
fn escalation_text() -> String {
    format!(
        "Progress guard escalation: {MAX_STALL_RECOVERIES} reconsideration prompts \
         did not change the outcome — the coordinator kept issuing equivalent work \
         with no observable progress. The decision loop stopped to preserve the \
         budget; the recorded dispatches were preserved."
    )
}

/// Extract one `call_specialist` tool call's observable contribution into
/// the cycle summary (issue #53): the dispatch decision (agent + raw task)
/// and its settlement (outcome label + reported files). Structured errors
/// — including validation/policy rejections and unparsed arguments — are
/// recorded as `error:{code}` settlements so a coordinator that repeatedly
/// issues rejected or malformed decisions is as visible as one that
/// repeats failing dispatches. Pure; unit-tested.
pub fn observe_specialist_result(
    arguments: &serde_json::Value,
    result: &serde_json::Value,
    observation: &mut CycleObservation,
) {
    let (agent, task) = match (
        arguments.get("agent_id").and_then(serde_json::Value::as_str),
        arguments.get("task").and_then(serde_json::Value::as_str),
    ) {
        (Some(agent), Some(task)) => (agent.to_owned(), task.to_owned()),
        _ => ("(malformed)".to_owned(), "(malformed)".to_owned()),
    };
    observation.dispatches.push((agent.clone(), task));
    let files: Vec<String> = result
        .get("files_modified")
        .and_then(serde_json::Value::as_array)
        .map(|values| {
            values.iter().filter_map(serde_json::Value::as_str).map(str::to_owned).collect()
        })
        .unwrap_or_default();
    if let Some(outcome) = result.get("outcome").and_then(serde_json::Value::as_str) {
        observation.settlements.push((agent, outcome.to_owned(), files));
    } else if let Some(error) = result.get("error").and_then(serde_json::Value::as_str) {
        observation.settlements.push((agent, format!("error:{error}"), files));
    } else {
        observation.settlements.push((agent, "error:unknown_shape".to_owned(), files));
    }
}

/// Stable label for a decision kind (mirrors the kebab-case serde
/// representation in `decisions.rs` by an explicit match, not by its
/// `Debug` output which may drift).
pub(crate) fn decision_kind_label(kind: DecisionKind) -> &'static str {
    match kind {
        DecisionKind::DispatchSpecialist => "dispatch-specialist",
        DecisionKind::DraftPlan => "draft-plan",
        DecisionKind::SelfExecute => "self-execute",
        DecisionKind::Replan => "replan",
        DecisionKind::Retry => "retry",
        DecisionKind::FallbackTier => "fallback-tier",
        // Issue #57 task transforms.
        DecisionKind::Split => "split",
        DecisionKind::Merge => "merge",
    }
}

/// Stable label for a decision status (same discipline as
/// [`decision_kind_label`]).
pub(crate) fn decision_status_label(status: DecisionStatus) -> &'static str {
    match status {
        DecisionStatus::Pending => "pending",
        DecisionStatus::Validated => "validated",
        DecisionStatus::Dispatched => "dispatched",
        DecisionStatus::Settled => "settled",
        DecisionStatus::Rejected => "rejected",
    }
}

/// Write a length-prefixed string into `buf`
/// (`[4-byte BE length][UTF-8 bytes]`), matching the canonical-encoding
/// convention of [`crate::fingerprint`].
fn push_length_prefixed(buf: &mut Vec<u8>, s: &str) {
    let bytes = s.as_bytes();
    buf.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    buf.extend_from_slice(bytes);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A single-dispatch cycle observation.
    fn dispatch_cycle(agent: &str, task: &str, outcome: &str) -> CycleObservation {
        CycleObservation {
            dispatches: vec![(agent.to_owned(), task.to_owned())],
            settlements: vec![(agent.to_owned(), outcome.to_owned(), Vec::new())],
            journal_transitions: vec![("dispatch-specialist".to_owned(), "settled".to_owned())],
            ..CycleObservation::default()
        }
    }

    #[test]
    fn three_identical_cycles_trigger_reconsideration_within_budget() {
        let mut tracker = ProgressTracker::new();
        let cycle = dispatch_cycle("coder", "implement the thing", "success");
        let first = tracker.observe(&cycle);
        let second = tracker.observe(&cycle);
        let third = tracker.observe(&cycle);
        assert_eq!(first, CycleVerdict::Progressing, "first cycle: no predecessor");
        assert_eq!(second, CycleVerdict::Progressing, "one repeat is an anomaly guard");
        assert!(
            matches!(&third, CycleVerdict::Reconsider(text) if text.contains("Progress guard")),
            "third equivalent cycle is a stall: {third:?}"
        );
        // Detected BEFORE the recovery budget is exhausted (and far before
        // the decision loop's 64-turn structural bound): the guard fires at
        // MAX_STALL_ROUNDS cycles with budget remaining.
        assert_eq!(tracker.state().stall_recoveries, 1);
        assert!(tracker.state().stall_recoveries < MAX_STALL_RECOVERIES);
    }

    #[test]
    fn stall_detection_precedes_the_iteration_budget() {
        // The escalation path needs at most 2 * MAX_STALL_ROUNDS + 2 + 1
        // observed cycles (stall → fresh window → stall → escalation) — 10
        // here — while the decision loop's structural bound is 64 turns.
        // Pin the ordering invariant: the guard fires INSIDE the budget,
        // never after it.
        assert!((MAX_STALL_ROUNDS as usize) * 3 + 4 < 64);
        let mut tracker = ProgressTracker::new();
        let cycle = dispatch_cycle("coder", "t", "failed");
        let mut non_escalating = 0;
        let mut escalated = false;
        for _ in 0..64 {
            if matches!(tracker.observe(&cycle), CycleVerdict::Escalate(_)) {
                escalated = true;
                break;
            }
            non_escalating += 1;
        }
        assert!(escalated, "a persistent stall must escalate inside the bounded window");
        assert!(
            non_escalating < 64,
            "escalation happened after {non_escalating} cycles — inside the budget"
        );
        assert_eq!(tracker.state().stall_recoveries, MAX_STALL_RECOVERIES);
    }

    #[test]
    fn failed_operation_followed_by_recovery_is_not_a_stall() {
        let mut tracker = ProgressTracker::new();
        // A failed dispatch, THEN a valid recovery (same task re-run, but
        // it succeeds and produces a file + the workspace changes): the
        // fingerprint MUST advance — new outcome, new facts, changed
        // snapshot.
        let failed = dispatch_cycle("coder", "fix it", "failed");
        let mut recovered = dispatch_cycle("coder", "fix it", "success");
        recovered.settlements[0].2 = vec!["src/fix.rs".to_owned()];
        recovered.artifacts = vec!["src/fix.rs".to_owned()];
        recovered.snapshot_generation = Some("gen-2".to_owned());
        let first = tracker.observe(&failed);
        let second = tracker.observe(&recovered);
        let third = tracker.observe(&recovered);
        assert_eq!(first, CycleVerdict::Progressing);
        assert_eq!(second, CycleVerdict::Progressing, "the recovery advanced the fingerprint");
        assert_eq!(
            third,
            CycleVerdict::Progressing,
            "one repeat after the recovery is an anomaly guard, still not a stall"
        );
        assert_eq!(
            tracker.state().stall_recoveries,
            0,
            "no recovery prompt may be emitted on failed-then-recovered work"
        );
    }

    #[test]
    fn slow_but_valid_progress_never_flags() {
        let mut tracker = ProgressTracker::new();
        // Ten cycles, each advancing something small: a different task and
        // a different file touched each cycle.
        for index in 0..10 {
            let mut cycle = dispatch_cycle("coder", &format!("step {index}"), "success");
            cycle.settlements[0].2 = vec![format!("src/file-{index}.rs")];
            assert_eq!(
                tracker.observe(&cycle),
                CycleVerdict::Progressing,
                "cycle {index} advanced and must not flag"
            );
        }
        assert_eq!(
            tracker.state().stall_recoveries,
            0,
            "no recovery may be emitted on continuously progressing work"
        );
    }

    #[test]
    fn noise_fields_do_not_change_the_fingerprint() {
        let mut base = dispatch_cycle("coder", "the task", "success");
        base.spend_delta_usd = 0.0137;
        base.timestamp_ms = 1_700_000_000;
        let mut noisy = base.clone();
        noisy.spend_delta_usd = 9.999;
        noisy.timestamp_ms = 1_999_999_999;
        assert_eq!(
            base.fingerprint(),
            noisy.fingerprint(),
            "spend/timestamp noise is fingerprint-neutral by construction"
        );
        // And behaviorally: the tracker reads the noisy cycle as an
        // equivalent repeat, not as progress divergence.
        let mut tracker = ProgressTracker::new();
        let _ = tracker.observe(&base);
        let verdict = tracker.observe(&noisy);
        assert_eq!(verdict, CycleVerdict::Progressing);
        assert_eq!(tracker.state().repeated_rounds, 1, "the noisy cycle repeated the base");
    }

    #[test]
    fn snapshot_generation_change_is_progress() {
        let mut tracker = ProgressTracker::new();
        for generation in 0..12 {
            let mut cycle = dispatch_cycle("coder", "same task", "success");
            cycle.snapshot_generation = Some(format!("gen-{generation}"));
            assert_eq!(
                tracker.observe(&cycle),
                CycleVerdict::Progressing,
                "an objectively changed workspace advances the fingerprint"
            );
        }
    }

    #[test]
    fn within_cycle_order_does_not_matter() {
        let mut a = dispatch_cycle("coder", "same task", "success");
        a.artifacts = vec!["src/b.rs".to_owned(), "src/a.rs".to_owned()];
        let mut b = dispatch_cycle("coder", "same task", "success");
        b.artifacts = vec!["src/a.rs".to_owned(), "src/b.rs".to_owned()];
        assert_eq!(
            a.fingerprint(),
            b.fingerprint(),
            "fingerprint components are set-semantic (sorted + deduplicated)"
        );
    }

    #[test]
    fn equivalent_task_spellings_hash_equal() {
        let a = dispatch_cycle("coder", "Implement the Foo Bar", "success");
        let b = dispatch_cycle("coder", "implement  the   foo bar", "success");
        assert_eq!(
            a.fingerprint(),
            b.fingerprint(),
            "the dispatch component reuses the shared intent normalizer"
        );
    }

    #[test]
    fn recovery_resets_the_window_then_the_budget_exhausts_to_escalation() {
        let mut tracker = ProgressTracker::new();
        let stall_cycle = dispatch_cycle("coder", "t", "failed");
        let recovery = dispatch_cycle("coder", "t (revised approach)", "failed");

        // Stall #1 at the third identical cycle → recovery prompt #1, and
        // the equivalence window resets to give the recovery a fresh one.
        assert_eq!(tracker.observe(&stall_cycle), CycleVerdict::Progressing);
        assert_eq!(tracker.observe(&stall_cycle), CycleVerdict::Progressing);
        assert!(matches!(tracker.observe(&stall_cycle), CycleVerdict::Reconsider(_)));

        // The recovery changed the observable outcome (a different task) →
        // Progressing, window stays fresh.
        assert_eq!(tracker.observe(&recovery), CycleVerdict::Progressing);

        // Three identical cycles afterwards → stall #2, recovery prompt #2
        // (budget now exhausted).
        assert_eq!(
            tracker.observe(&stall_cycle),
            CycleVerdict::Progressing,
            "differs from the recovery cycle"
        );
        assert_eq!(tracker.observe(&stall_cycle), CycleVerdict::Progressing, "repeat 1");
        assert!(
            matches!(tracker.observe(&stall_cycle), CycleVerdict::Reconsider(_)),
            "repeat 2 stalls again"
        );
        assert_eq!(tracker.state().stall_recoveries, MAX_STALL_RECOVERIES);

        // The coordinator ignored both prompts: repeated equivalent work
        // runs over the fresh recovery window — two further identical
        // cycles (the 5th and 6th consecutive equivalents overall) reach
        // the escalation (the fingerprint history tail still ends on the
        // stalled cycle, so the post-recovery repeats count from there).
        assert_eq!(tracker.observe(&stall_cycle), CycleVerdict::Progressing);
        assert!(matches!(tracker.observe(&stall_cycle), CycleVerdict::Escalate(_)));
        // Sticky: further equivalent cycles keep escalating.
        assert!(matches!(tracker.observe(&stall_cycle), CycleVerdict::Escalate(_)));
        assert_eq!(tracker.state().stall_recoveries, MAX_STALL_RECOVERIES);
    }

    #[test]
    fn state_round_trips_through_serde_and_detection_survives_a_resume() {
        let mut tracker = ProgressTracker::new();
        let cycle = dispatch_cycle("coder", "t", "success");
        let _ = tracker.observe(&cycle);
        let _ = tracker.observe(&cycle);

        let serialized = serde_json::to_string(tracker.state()).expect("state serializes");
        let restored_state: ProgressTrackerState =
            serde_json::from_str(&serialized).expect("state deserializes");
        assert_eq!(restored_state, *tracker.state(), "state survives the round trip");

        // The restored tracker continues the streak exactly as if no
        // resume happened: the next equivalent cycle reaches the stall
        // threshold and emits the recovery prompt.
        let mut restored = ProgressTracker::from_state(restored_state);
        assert!(
            matches!(restored.observe(&cycle), CycleVerdict::Reconsider(_)),
            "detection survives a resume: history + streak restored"
        );
        assert_eq!(restored.state().stall_recoveries, 1);
    }

    #[test]
    fn specialist_result_extraction_records_dispatch_and_settlement() {
        let arguments = serde_json::json!({
            "agent_id": "coder",
            "task": "implement the thing"
        });
        let result = serde_json::json!({
            "outcome": "success",
            "agent_id": "coder",
            "files_modified": ["src/main.rs"],
            "tool_call_count": 3
        });
        let mut observation = CycleObservation::default();
        observe_specialist_result(&arguments, &result, &mut observation);
        assert_eq!(
            observation.dispatches,
            vec![("coder".to_owned(), "implement the thing".to_owned())]
        );
        assert_eq!(
            observation.settlements,
            vec![("coder".to_owned(), "success".to_owned(), vec!["src/main.rs".to_owned()])]
        );
    }

    #[test]
    fn specialist_result_extraction_treats_errors_as_observable_outcomes() {
        let arguments = serde_json::json!({ "agent_id": "ghost", "task": "rejected dispatch" });
        let result = serde_json::json!({
            "error": "unknown_agent",
            "message": "no specialist registered for id ghost"
        });
        let mut observation = CycleObservation::default();
        observe_specialist_result(&arguments, &result, &mut observation);
        assert_eq!(
            observation.settlements,
            vec![("ghost".to_owned(), "error:unknown_agent".to_owned(), Vec::<String>::new())],
            "a repeated rejected decision must be observable (stall-detectable)"
        );

        // Malformed arguments settle under a stable placeholder so a
        // repeated malformed decision is observable too.
        let mut malformed_observation = CycleObservation::default();
        observe_specialist_result(&serde_json::json!({}), &result, &mut malformed_observation);
        assert_eq!(malformed_observation.dispatches[0].0, "(malformed)");
        assert_eq!(malformed_observation.settlements[0].1, "error:unknown_agent");
    }
}
