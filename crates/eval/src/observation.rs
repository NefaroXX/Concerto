//! Read-only per-run observation export (H15).
//!
//! Surfaces the ordered tool-call sequence and the typed verification stamps
//! that the harness plan's three sequence-dependent metrics need, projected
//! from the [`AgentOutput`] the eval factory already holds — no audit-log
//! re-read, no second runner, no live model required.
//!
//! The export is deliberately **shaped and bounded**: it carries canonical
//! identity (tool + canonical operation + an input hash), success, and
//! timestamps — never raw tool arguments, tool summaries, file paths, or
//! verification output. Long runs retain only a prefix window; the total and
//! a truncation flag make the dropped tail visible so a truncated window
//! under-counts honestly instead of returning a wrong-looking exact number.
//!
//! ## The three metrics
//!
//! 1. [`probing_calls_before_first_write`] — environment-probing calls before
//!    the first successful file-affecting action.
//! 2. [`identical_failed_retries`] — maximal runs of consecutive identical
//!    failed calls before any success.
//! 3. [`time_to_first_passing_verification`] — the first passing verification
//!    record's typed `checked_at` minus the run start.

use std::time::Duration;

use concerto_core::types::AgentOutput;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

/// Maximum number of tool-call observations retained per run.
///
/// Bounds per-run memory for long runs (plan constraint 7: long operations
/// bounded). The retained window is the run's **prefix** — the calls that
/// metrics 1 and 2 read — and the dropped tail is reported via
/// [`AgentObservation::tool_calls_truncated`] and
/// [`AgentObservation::tool_calls_total`], never silently.
pub const MAX_OBSERVED_TOOL_CALLS: usize = 512;

/// Maximum number of verification observations retained per run.
pub const MAX_OBSERVED_VERIFICATIONS: usize = 128;

/// The read-only observation export for one agent run.
///
/// Every field is additive and serde-defaulted so a caller that predates the
/// export still compiles/decodes. `Default` is the honest "not observed"
/// value.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AgentObservation {
    /// Wall-clock time the run started (UTC). `None` when the producer did not
    /// capture it; metric 3 is then unmeasurable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_started_at: Option<OffsetDateTime>,
    /// Ordered tool-call sequence, retained prefix only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ObservedToolCall>,
    /// Total tool calls the run made, before the retention cap.
    #[serde(default)]
    pub tool_calls_total: u32,
    /// True when [`Self::tool_calls`] is a prefix of a longer sequence: the
    /// tail was dropped, so any metric read from the window is a lower bound
    /// when its event was not observed.
    #[serde(default)]
    pub tool_calls_truncated: bool,
    /// Verification records, retained prefix only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub verifications: Vec<ObservedVerification>,
    /// Total verification records, before the retention cap.
    #[serde(default)]
    pub verifications_total: u32,
    /// True when [`Self::verifications`] is a prefix of a longer sequence.
    #[serde(default)]
    pub verifications_truncated: bool,
}

impl AgentObservation {
    /// True when the observation carries no data at all (skipped in reports).
    pub fn is_empty(&self) -> bool {
        self.run_started_at.is_none()
            && self.tool_calls.is_empty()
            && self.tool_calls_total == 0
            && self.verifications.is_empty()
            && self.verifications_total == 0
    }
}

/// One tool call in the read-only export.
///
/// Canonical identity for retry detection is `(tool, operation,
/// input_fingerprint)`. `tool`/`operation` are the ADR-82 canonical policy-view
/// pair; `input_fingerprint` is a hash (never raw arguments), so an eval
/// artifact carries no secrets and an identical retry keeps the same key.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObservedToolCall {
    /// Zero-based position in the run's call sequence (ordering authority).
    pub sequence: u32,
    /// Canonical policy-view tool name (`filesystem` for a `write` alias).
    pub tool: String,
    /// Canonical operation, when the policy-view input named one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation: Option<String>,
    /// Whether the call succeeded.
    pub success: bool,
    /// Wall-clock time the call finished, when the producer stamped one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<OffsetDateTime>,
    /// Canonical input hash (the audit's `input_hash` grammar), never raw
    /// arguments. `None` when the call never reached argument normalization.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_fingerprint: Option<String>,
    /// Typed failure code for a failed call (e.g. `exec.io_error`), bounded and
    /// sanitized at the source. `None` for successes and unclassified failures.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_code: Option<String>,
}

/// One verification record in the read-only export: the typed stamp and the
/// freshness verdict, with no checked-file output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObservedVerification {
    /// Short command label, e.g. `cargo check`, `py_compile`, `npm test`.
    pub command: String,
    /// The resolved outcome label (`passed`, `failed`, `skipped`, ...).
    pub outcome: String,
    /// The freshness verdict: whether this record satisfies the strict
    /// `is_passing_evidence` predicate (outcome `passed` **and** the full R2
    /// freshness basis — verifier, scope, generation, content fingerprint,
    /// typed `checked_at`, evidence ids). False for the eval path's records,
    /// which set verifier/scope but not generation, content fingerprint, or
    /// evidence ids — itself the honest signal metric 3's operator needs.
    pub is_passing_evidence: bool,
    /// Typed check timestamp (ADR-82 R2). `None` on legacy records.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked_at: Option<OffsetDateTime>,
}

/// A window-derived count that distinguishes an exact value from an honest
/// lower bound.
///
/// A truncated window drops the tail; when the event a metric looks for is not
/// in the retained prefix, the count is only a lower bound (the true value is
/// `>=` it) and must never be reported as an exact number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObservedCount {
    /// The retained window held the whole sequence, so the count is exact.
    Exact(u32),
    /// The window was truncated and the event was not observed; the true count
    /// is at least this.
    LowerBound(u32),
    /// The observation was never captured (e.g. the no-agent fallback), so no
    /// honest count exists — never report this as an exact zero.
    Unknown,
}

impl ObservedCount {
    /// The **exact** value, or `None` when the count is a lower bound or
    /// unknown.
    ///
    /// Deliberately returns nothing for [`Self::LowerBound`]: a truncated
    /// window's count must be acknowledged as a lower bound through
    /// [`Self::lower_bound_value`] and never read as a bare number. No single
    /// accessor returns a value for both `Exact` and `LowerBound`, so a caller
    /// cannot silently treat a lower bound as exact.
    pub fn value(self) -> Option<u32> {
        match self {
            Self::Exact(value) => Some(value),
            Self::LowerBound(_) | Self::Unknown => None,
        }
    }

    /// The **lower-bound** value, or `None` when the count is exact or unknown.
    pub fn lower_bound_value(self) -> Option<u32> {
        match self {
            Self::LowerBound(value) => Some(value),
            Self::Exact(_) | Self::Unknown => None,
        }
    }

    /// Whether the count is exact (not a lower bound).
    pub fn is_exact(self) -> bool {
        matches!(self, Self::Exact(_))
    }

    /// Whether the count is an honest lower bound.
    pub fn is_lower_bound(self) -> bool {
        matches!(self, Self::LowerBound(_))
    }
}

/// Project an [`AgentOutput`] into the bounded, secret-free export.
///
/// Retains the first [`MAX_OBSERVED_TOOL_CALLS`] calls and
/// [`MAX_OBSERVED_VERIFICATIONS`] verifications, records the totals, and
/// flags truncation. Drops tool summaries, paths, and verification output —
/// only canonical identity, success, failure codes, and timestamps ride.
pub fn observe_agent_output(
    output: &AgentOutput,
    run_started_at: OffsetDateTime,
) -> AgentObservation {
    let total_calls = output.tool_events.len();
    let tool_calls: Vec<ObservedToolCall> = output
        .tool_events
        .iter()
        .take(MAX_OBSERVED_TOOL_CALLS)
        .enumerate()
        .map(|(index, event)| ObservedToolCall {
            sequence: u32::try_from(index).unwrap_or(u32::MAX),
            tool: event.canonical_tool.clone().unwrap_or_else(|| event.tool_name.clone()),
            operation: event.canonical_operation.clone().or_else(|| event.operation.clone()),
            success: event.success,
            at: event.at,
            input_fingerprint: event.input_fingerprint.clone(),
            failure_code: event.failure.as_ref().map(|failure| failure.code.clone()),
        })
        .collect();

    let total_verifications = output.verification.len();
    let verifications: Vec<ObservedVerification> = output
        .verification
        .iter()
        .take(MAX_OBSERVED_VERIFICATIONS)
        .map(|record| ObservedVerification {
            command: record.command.clone(),
            outcome: record.outcome_label().to_string(),
            is_passing_evidence: record.is_passing_evidence(),
            checked_at: record.checked_at,
        })
        .collect();

    AgentObservation {
        run_started_at: Some(run_started_at),
        tool_calls_total: u32::try_from(total_calls).unwrap_or(u32::MAX),
        tool_calls_truncated: total_calls > MAX_OBSERVED_TOOL_CALLS,
        tool_calls,
        verifications_total: u32::try_from(total_verifications).unwrap_or(u32::MAX),
        verifications_truncated: total_verifications > MAX_OBSERVED_VERIFICATIONS,
        verifications,
    }
}

/// Operations that mutate the workspace, per the measurement procedure
/// ("write/delete/edit/filesystem mutation"). Read-only discovery
/// (`read`/`list`/`search`/`exists`) counts as probing.
const MUTATING_OPERATIONS: &[&str] = &[
    "write", "delete", "edit", "move", "copy", "create", "append", "rename", "mkdir", "touch",
    "remove", "truncate",
];

/// Whether a call is a file-affecting mutation (metric 1's "useful action").
fn is_file_affecting(call: &ObservedToolCall) -> bool {
    if call.operation.as_deref().is_some_and(|operation| MUTATING_OPERATIONS.contains(&operation)) {
        return true;
    }
    // Registered write-alias tool names (no canonical operation recorded).
    matches!(call.tool.as_str(), "write" | "edit" | "delete")
}

/// Metric 1: environment-probing calls before the first useful action.
///
/// Counts tool-call rows from run start until the first **successful**
/// file-affecting write; read-only discovery counts as probing. The write
/// itself is the first useful action, so it is not counted as probing.
///
/// Returns [`ObservedCount::Exact`] when the sequence was fully retained (or
/// no write occurred and the sequence was complete),
/// [`ObservedCount::LowerBound`] when the window was truncated and no write
/// was observed — the true count is at least the retained length — and
/// [`ObservedCount::Unknown`] when the observation was never captured.
pub fn probing_calls_before_first_write(observation: &AgentObservation) -> ObservedCount {
    if observation.run_started_at.is_none() {
        // The observation was never populated (e.g. the no-agent fallback);
        // an exact zero here would be a wrong-looking number.
        return ObservedCount::Unknown;
    }
    let calls = &observation.tool_calls;
    match calls.iter().position(|call| call.success && is_file_affecting(call)) {
        Some(index) => ObservedCount::Exact(u32::try_from(index).unwrap_or(u32::MAX)),
        None if observation.tool_calls_truncated => {
            ObservedCount::LowerBound(u32::try_from(calls.len()).unwrap_or(u32::MAX))
        }
        None => ObservedCount::Exact(u32::try_from(calls.len()).unwrap_or(u32::MAX)),
    }
}

/// Whether two calls share the retry identity `(tool, operation, input)`.
///
/// Falls back to `(tool, operation)` **only when neither side carries an input
/// fingerprint** — a coarser but honest match, since an absent fingerprint on
/// both sides means neither input was normalized. A fingerprinted call and an
/// unfingerprinted one are deliberately NOT the same identity: the missing
/// fingerprint means that call's input is unknown, so folding it into a retry
/// would inflate the count with a plausible-wrong number.
fn same_retry_identity(a: &ObservedToolCall, b: &ObservedToolCall) -> bool {
    if a.tool != b.tool || a.operation != b.operation {
        return false;
    }
    match (&a.input_fingerprint, &b.input_fingerprint) {
        (Some(left), Some(right)) => left == right,
        (None, None) => true,
        // One side fingerprinted, the other not: not provably identical.
        _ => false,
    }
}

/// Metric 2: identical failed-call retries before any success.
///
/// Considers the failed prefix up to (but excluding) the first successful call.
/// Each maximal run of `L >= 2` consecutive failed calls sharing the retry
/// identity contributes `L - 1` retries (the first call is not a retry).
///
/// Returns [`ObservedCount::Exact`] when the first success (or the end of the
/// sequence) was retained, [`ObservedCount::LowerBound`] when the window was
/// truncated and no success was observed — the dropped tail may hold more
/// identical retries — and [`ObservedCount::Unknown`] when the observation was
/// never captured.
pub fn identical_failed_retries(observation: &AgentObservation) -> ObservedCount {
    if observation.run_started_at.is_none() {
        return ObservedCount::Unknown;
    }
    let calls = &observation.tool_calls;
    let first_success = calls.iter().position(|call| call.success);
    let end = first_success.unwrap_or(calls.len());
    let prefix = &calls[..end];

    let mut retries: u32 = 0;
    let mut index = 0usize;
    while index < prefix.len() {
        let mut run_end = index + 1;
        while run_end < prefix.len() && same_retry_identity(&prefix[index], &prefix[run_end]) {
            run_end += 1;
        }
        // A maximal run of `run_end - index` identical consecutive failures
        // contributes one fewer retry than its length.
        retries = retries.saturating_add(u32::try_from(run_end - index - 1).unwrap_or(0));
        index = run_end;
    }

    if observation.tool_calls_truncated && first_success.is_none() {
        ObservedCount::LowerBound(retries)
    } else {
        ObservedCount::Exact(retries)
    }
}

/// Metric 3: elapsed time from run start to the first fresh passing evidence.
///
/// Selects the first record whose [`ObservedVerification::is_passing_evidence`]
/// verdict is true — the strict H04 freshness predicate — and uses its typed
/// [`ObservedVerification::checked_at`]. The raw `outcome == "passed"` label is
/// deliberately NOT sufficient: a record can read `passed` while failing the
/// freshness basis (a legacy record missing verifier/scope/generation/content
/// fingerprint/evidence ids), and that is display-grade, not fresh evidence.
///
/// Returns `None` when the run start, a fresh passing record, or its typed
/// `checked_at` is not captured (the metric is then unmeasured, not estimated).
pub fn time_to_first_passing_verification(observation: &AgentObservation) -> Option<Duration> {
    let start = observation.run_started_at?;
    let first = observation.verifications.iter().find(|record| record.is_passing_evidence)?;
    let checked_at = first.checked_at?;
    let elapsed = checked_at - start;
    if elapsed.is_negative() {
        return None;
    }
    let millis = u64::try_from(elapsed.whole_milliseconds()).ok()?;
    Some(Duration::from_millis(millis))
}

#[cfg(test)]
mod tests {
    use super::*;
    use concerto_core::ids::Ulid;
    use concerto_core::types::{
        AgentCompletionStatus, AgentOutput, TaskId, ToolExecutionSummary, VerificationSummary,
    };

    fn call(
        sequence: u32,
        tool: &str,
        operation: Option<&str>,
        success: bool,
        fingerprint: Option<&str>,
    ) -> ObservedToolCall {
        ObservedToolCall {
            sequence,
            tool: tool.to_string(),
            operation: operation.map(str::to_string),
            success,
            at: None,
            input_fingerprint: fingerprint.map(str::to_string),
            failure_code: None,
        }
    }

    fn observation_with(calls: Vec<ObservedToolCall>) -> AgentObservation {
        AgentObservation {
            run_started_at: Some(OffsetDateTime::UNIX_EPOCH),
            tool_calls_total: u32::try_from(calls.len()).unwrap_or(u32::MAX),
            tool_calls: calls,
            ..AgentObservation::default()
        }
    }

    // ---- Metric 1: probing calls before the first useful action ----------

    #[test]
    fn probing_counts_read_only_discovery_before_the_first_write() {
        let observation = observation_with(vec![
            call(0, "filesystem", Some("read"), true, Some("h1")),
            call(1, "filesystem", Some("list"), true, Some("h2")),
            call(2, "filesystem", Some("write"), true, Some("h3")),
            call(3, "filesystem", Some("read"), true, Some("h4")),
        ]);
        assert_eq!(probing_calls_before_first_write(&observation), ObservedCount::Exact(2));
    }

    #[test]
    fn probing_ignores_failed_writes_and_counts_a_later_successful_write() {
        let observation = observation_with(vec![
            call(0, "filesystem", Some("read"), true, Some("h1")),
            call(1, "filesystem", Some("write"), false, Some("h2")),
            call(2, "filesystem", Some("read"), true, Some("h3")),
            call(3, "filesystem", Some("write"), true, Some("h4")),
        ]);
        // The failed write is probing; the first SUCCESSFUL write is the
        // useful action, at index 3.
        assert_eq!(probing_calls_before_first_write(&observation), ObservedCount::Exact(3));
    }

    #[test]
    fn probing_is_exact_when_no_write_occurred_and_the_window_is_complete() {
        let observation = observation_with(vec![
            call(0, "filesystem", Some("read"), true, Some("h1")),
            call(1, "filesystem", Some("read"), true, Some("h2")),
        ]);
        assert_eq!(probing_calls_before_first_write(&observation), ObservedCount::Exact(2));
    }

    #[test]
    fn probing_is_a_lower_bound_when_the_window_is_truncated_without_a_write() {
        let mut observation = observation_with(vec![
            call(0, "filesystem", Some("read"), true, Some("h1")),
            call(1, "filesystem", Some("read"), true, Some("h2")),
        ]);
        observation.tool_calls_truncated = true;
        observation.tool_calls_total = 900;
        let count = probing_calls_before_first_write(&observation);
        assert_eq!(count, ObservedCount::LowerBound(2));
        assert!(count.is_lower_bound(), "a truncated window under-counts, never exact");
        // The bare `value()` must NOT hand back a lower bound as if exact.
        assert_eq!(count.value(), None);
        assert_eq!(count.lower_bound_value(), Some(2));
    }

    // ---- Metric 2: identical failed-call retries -------------------------

    #[test]
    fn identical_retries_counts_repeats_of_the_same_call_before_success() {
        let observation = observation_with(vec![
            call(0, "filesystem", Some("write"), false, Some("same")),
            call(1, "filesystem", Some("write"), false, Some("same")),
            call(2, "filesystem", Some("write"), false, Some("same")),
            call(3, "filesystem", Some("write"), true, Some("same")),
        ]);
        // A run of three identical failures is two retries.
        assert_eq!(identical_failed_retries(&observation), ObservedCount::Exact(2));
    }

    #[test]
    fn identical_retries_distinguishes_different_inputs() {
        let observation = observation_with(vec![
            call(0, "filesystem", Some("write"), false, Some("input-a")),
            call(1, "filesystem", Some("write"), false, Some("input-b")),
            call(2, "filesystem", Some("write"), false, Some("input-b")),
            call(3, "filesystem", Some("write"), true, Some("input-c")),
        ]);
        // input-a then input-b twice: one retry (the second input-b).
        assert_eq!(identical_failed_retries(&observation), ObservedCount::Exact(1));
    }

    #[test]
    fn identical_retries_ignores_failures_after_a_success() {
        let observation = observation_with(vec![
            call(0, "shell", Some("exec"), false, Some("same")),
            call(1, "shell", Some("exec"), false, Some("same")),
            call(2, "shell", Some("exec"), true, Some("ok")),
            call(3, "shell", Some("exec"), false, Some("same")),
            call(4, "shell", Some("exec"), false, Some("same")),
        ]);
        // Only the pre-success prefix counts: one retry.
        assert_eq!(identical_failed_retries(&observation), ObservedCount::Exact(1));
    }

    #[test]
    fn identical_retries_falls_back_to_tool_and_operation_without_fingerprints() {
        let observation = observation_with(vec![
            call(0, "shell", Some("exec"), false, None),
            call(1, "shell", Some("exec"), false, None),
            call(2, "filesystem", Some("write"), false, None),
        ]);
        assert_eq!(identical_failed_retries(&observation), ObservedCount::Exact(1));
    }

    #[test]
    fn identical_retries_does_not_fold_a_fingerprinted_call_into_an_unfingerprinted_one() {
        // Regression (blocker 1): a fingerprinted call followed by an
        // unfingerprinted call with the same tool/operation has UNKNOWN input
        // on the second call — it must not be counted as an identical retry.
        let observation = observation_with(vec![
            call(0, "filesystem", Some("write"), false, Some("known-input")),
            call(1, "filesystem", Some("write"), false, None),
        ]);
        assert_eq!(
            identical_failed_retries(&observation),
            ObservedCount::Exact(0),
            "a mixed fingerprint pair is not an identical retry"
        );

        // The reverse order is equally not a retry.
        let reversed = observation_with(vec![
            call(0, "filesystem", Some("write"), false, None),
            call(1, "filesystem", Some("write"), false, Some("known-input")),
        ]);
        assert_eq!(identical_failed_retries(&reversed), ObservedCount::Exact(0));
    }

    #[test]
    fn identical_retries_is_a_lower_bound_when_truncated_without_success() {
        let mut observation = observation_with(vec![
            call(0, "shell", Some("exec"), false, Some("same")),
            call(1, "shell", Some("exec"), false, Some("same")),
        ]);
        observation.tool_calls_truncated = true;
        observation.tool_calls_total = 800;
        let count = identical_failed_retries(&observation);
        assert_eq!(count, ObservedCount::LowerBound(1));
        assert!(count.is_lower_bound(), "a truncated window under-counts, never exact");
    }

    #[test]
    fn counts_are_unknown_when_the_observation_was_never_captured() {
        // The no-agent fallback leaves a `Default` observation; an exact zero
        // here would be a wrong-looking number.
        let uncaptured = AgentObservation::default();
        assert_eq!(probing_calls_before_first_write(&uncaptured), ObservedCount::Unknown);
        assert_eq!(identical_failed_retries(&uncaptured), ObservedCount::Unknown);
        assert_eq!(probing_calls_before_first_write(&uncaptured).value(), None);
        assert_eq!(probing_calls_before_first_write(&uncaptured).lower_bound_value(), None);
    }

    #[test]
    fn observed_count_never_exposes_a_lower_bound_as_a_bare_value() {
        // Regression (blocker 3): no single accessor returns a value for both
        // `Exact` and `LowerBound`; the caller must acknowledge which it got.
        assert_eq!(ObservedCount::Exact(4).value(), Some(4));
        assert_eq!(ObservedCount::Exact(4).lower_bound_value(), None);
        assert_eq!(ObservedCount::LowerBound(4).value(), None);
        assert_eq!(ObservedCount::LowerBound(4).lower_bound_value(), Some(4));
        assert_eq!(ObservedCount::Unknown.value(), None);
        assert_eq!(ObservedCount::Unknown.lower_bound_value(), None);
    }

    // ---- Metric 3: time to first passing verification --------------------

    #[test]
    fn time_to_first_passing_verification_uses_the_typed_stamp() {
        let start = OffsetDateTime::UNIX_EPOCH;
        let observation = AgentObservation {
            run_started_at: Some(start),
            verifications: vec![
                ObservedVerification {
                    command: "cargo check".to_string(),
                    outcome: "failed".to_string(),
                    is_passing_evidence: false,
                    checked_at: Some(start + time::Duration::seconds(1)),
                },
                ObservedVerification {
                    command: "cargo test".to_string(),
                    outcome: "passed".to_string(),
                    is_passing_evidence: true,
                    checked_at: Some(start + time::Duration::seconds(5)),
                },
                ObservedVerification {
                    command: "cargo test".to_string(),
                    outcome: "passed".to_string(),
                    is_passing_evidence: true,
                    checked_at: Some(start + time::Duration::seconds(9)),
                },
            ],
            ..AgentObservation::default()
        };
        assert_eq!(
            time_to_first_passing_verification(&observation),
            Some(Duration::from_secs(5)),
            "the first fresh passing record's typed checked_at wins"
        );
    }

    #[test]
    fn time_to_first_passing_verification_requires_the_freshness_verdict_not_just_passed() {
        let start = OffsetDateTime::UNIX_EPOCH;
        // Regression (blocker 2): a `passed`-labelled record that fails the
        // freshness basis is display-grade, not fresh evidence, so it must NOT
        // satisfy the metric.
        let stale_pass = AgentObservation {
            run_started_at: Some(start),
            verifications: vec![ObservedVerification {
                command: "cargo test".to_string(),
                outcome: "passed".to_string(),
                is_passing_evidence: false,
                checked_at: Some(start + time::Duration::seconds(3)),
            }],
            ..AgentObservation::default()
        };
        assert_eq!(
            time_to_first_passing_verification(&stale_pass),
            None,
            "outcome == passed without the freshness verdict is not fresh evidence"
        );

        // The first record whose VERDICT is true wins, even when an earlier
        // record carries a `passed` label without the freshness basis.
        let verdict_wins = AgentObservation {
            run_started_at: Some(start),
            verifications: vec![
                ObservedVerification {
                    command: "cargo test".to_string(),
                    outcome: "passed".to_string(),
                    is_passing_evidence: false,
                    checked_at: Some(start + time::Duration::seconds(3)),
                },
                ObservedVerification {
                    command: "cargo test".to_string(),
                    outcome: "passed".to_string(),
                    is_passing_evidence: true,
                    checked_at: Some(start + time::Duration::seconds(7)),
                },
            ],
            ..AgentObservation::default()
        };
        assert_eq!(
            time_to_first_passing_verification(&verdict_wins),
            Some(Duration::from_secs(7)),
            "the freshness verdict, not the raw outcome label, selects the record"
        );
    }

    #[test]
    fn time_to_first_passing_verification_is_unmeasured_without_a_pass_or_stamp() {
        let start = OffsetDateTime::UNIX_EPOCH;
        // No passing record.
        let no_pass = AgentObservation {
            run_started_at: Some(start),
            verifications: vec![ObservedVerification {
                command: "cargo test".to_string(),
                outcome: "failed".to_string(),
                is_passing_evidence: false,
                checked_at: Some(start + time::Duration::seconds(2)),
            }],
            ..AgentObservation::default()
        };
        assert_eq!(time_to_first_passing_verification(&no_pass), None);

        // Passing outcome but no typed stamp: unmeasured, never a proxy.
        let no_stamp = AgentObservation {
            run_started_at: Some(start),
            verifications: vec![ObservedVerification {
                command: "cargo test".to_string(),
                outcome: "passed".to_string(),
                is_passing_evidence: true,
                checked_at: None,
            }],
            ..AgentObservation::default()
        };
        assert_eq!(time_to_first_passing_verification(&no_stamp), None);

        // No run start.
        let no_start = AgentObservation {
            run_started_at: None,
            verifications: vec![ObservedVerification {
                command: "cargo test".to_string(),
                outcome: "passed".to_string(),
                is_passing_evidence: true,
                checked_at: Some(start),
            }],
            ..AgentObservation::default()
        };
        assert_eq!(time_to_first_passing_verification(&no_start), None);
    }

    // ---- Projection: cap, truncation visibility, no secrets --------------

    fn tool_summary(index: usize) -> ToolExecutionSummary {
        ToolExecutionSummary {
            tool_name: "filesystem".to_string(),
            operation: Some("write".to_string()),
            path: None,
            success: true,
            summary: format!("Wrote file {index} with api_key=SECRET-{index}"),
            canonical_tool: Some("filesystem".to_string()),
            canonical_operation: Some("write".to_string()),
            failure: None,
            at: Some(OffsetDateTime::UNIX_EPOCH),
            input_fingerprint: Some(format!("fingerprint-{index}")),
        }
    }

    fn agent_output_with_events(events: Vec<ToolExecutionSummary>) -> AgentOutput {
        AgentOutput {
            task_id: TaskId::new(),
            session_id: Ulid::new(),
            final_message: String::new(),
            files_modified: Vec::new(),
            tool_call_count: u32::try_from(events.len()).unwrap_or(u32::MAX),
            eval_result: None,
            tool_events: events,
            verification: Vec::new(),
            project_root: None,
            completion_status: AgentCompletionStatus::Completed,
            provider_metrics: Vec::new(),
            checkpoint_json: None,
        }
    }

    #[test]
    fn projection_caps_the_window_and_reports_truncation_honestly() {
        let total = MAX_OBSERVED_TOOL_CALLS + 5;
        let output = agent_output_with_events((0..total).map(tool_summary).collect::<Vec<_>>());
        let observation = observe_agent_output(&output, OffsetDateTime::UNIX_EPOCH);
        assert_eq!(observation.tool_calls.len(), MAX_OBSERVED_TOOL_CALLS, "prefix retained");
        assert_eq!(observation.tool_calls_total, u32::try_from(total).unwrap());
        assert!(observation.tool_calls_truncated, "truncation is visible");
        // The retained window is the prefix, so sequence numbers are contiguous.
        assert_eq!(observation.tool_calls.first().map(|call| call.sequence), Some(0));
        assert_eq!(
            observation.tool_calls.last().map(|call| call.sequence),
            Some(u32::try_from(MAX_OBSERVED_TOOL_CALLS - 1).unwrap())
        );
    }

    #[test]
    fn projection_drops_summaries_and_paths_so_no_secrets_ride() {
        let output = agent_output_with_events(vec![tool_summary(7)]);
        let observation = observe_agent_output(&output, OffsetDateTime::UNIX_EPOCH);
        let json = serde_json::to_string(&observation).expect("observation serializes");
        assert!(!json.contains("SECRET"), "tool summary text must not ride: {json}");
        assert!(!json.contains("Wrote file"), "tool summary text must not ride: {json}");
        // The canonical identity and the input hash do ride.
        assert!(json.contains("fingerprint-7"));
        assert_eq!(observation.tool_calls[0].tool, "filesystem");
        assert_eq!(observation.tool_calls[0].operation.as_deref(), Some("write"));
    }

    #[test]
    fn projection_caps_verifications_and_marks_truncation() {
        let output = AgentOutput {
            task_id: TaskId::new(),
            session_id: Ulid::new(),
            final_message: String::new(),
            files_modified: Vec::new(),
            tool_call_count: 0,
            eval_result: None,
            tool_events: Vec::new(),
            verification: (0..MAX_OBSERVED_VERIFICATIONS + 3)
                .map(|index| {
                    VerificationSummary::passed(
                        format!("src/f{index}.rs"),
                        "cargo check",
                        format!("ok {index}"),
                    )
                })
                .collect(),
            project_root: None,
            completion_status: AgentCompletionStatus::Completed,
            provider_metrics: Vec::new(),
            checkpoint_json: None,
        };
        let observation = observe_agent_output(&output, OffsetDateTime::UNIX_EPOCH);
        assert_eq!(observation.verifications.len(), MAX_OBSERVED_VERIFICATIONS);
        assert!(observation.verifications_truncated);
        assert_eq!(
            observation.verifications_total,
            u32::try_from(MAX_OBSERVED_VERIFICATIONS + 3).unwrap()
        );
        // A constructed record carries a typed checked_at and is not strict
        // passing evidence (no verifier/scope/evidence ids on the eval path).
        assert!(observation.verifications[0].checked_at.is_some());
        assert!(!observation.verifications[0].is_passing_evidence);
        assert_eq!(observation.verifications[0].outcome, "passed");
    }
}
