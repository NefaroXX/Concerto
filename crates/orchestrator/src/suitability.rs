//! Issue #60: agent-suitability measurement — a DERIVED, BOUNDED record of
//! dispatch outcomes per (specialist agent, task class), plus a
//! deterministic ranking query the Coordinator consumes as ADVISORY
//! context (the model still decides who to call; nothing is selected by
//! code, and no cost/spend/model-quality signal is ever a score input).
//!
//! Sources of truth (never a parallel ledger): every observation recorded
//! here is one the existing dispatch machinery already produced — the
//! settled [`AgentOutcome`] of a `call_specialist` dispatch, its #54
//! diagnosis (failure kind), its retry/recovery turn count, and the
//! dispatch-time policy denials. Spend is RECORDED as an observed metric
//! only and is deliberately absent from the score.
//!
//! Invariants (pinned in the tests below):
//! - the store is bounded (ring per key, bounded key count);
//! - outcomes decay with a fixed time half-life so stale results fade and
//!   eventually drop out;
//! - the score is floored above exclusion: a single failure (or any
//!   failure count) never removes a candidate from the ranking, and fresh
//!   successes restore standing;
//! - the ranking is a pure function of (recorded outcomes, query, now):
//!   the same history and the same query produce byte-identical rankings.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

// --------------------------------------------------------------------------
// Task class: deterministic derivation from the task text + artifacts
// --------------------------------------------------------------------------

/// The bounded task-class taxonomy dispatches are bucketed into. Derivation
/// is fully deterministic (artifact extensions + keyword scan) — never a
/// model call; the same input always maps to the same class.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TaskClass {
    /// Design the architecture / produce a plan or specification.
    Design,
    /// Gather, compare, or investigate information.
    Research,
    /// Verify, test, review, or audit existing work.
    Validation,
    /// Write or edit implementation code.
    CodeEdit,
    /// Write or edit documentation.
    Docs,
    /// Build, run, or configure tooling/CI/scripts/dependencies.
    Ops,
    /// Nothing more specific applies.
    General,
}

impl TaskClass {
    /// The kebab-case label used in reason strings.
    pub fn as_str(self) -> &'static str {
        match self {
            TaskClass::Design => "design",
            TaskClass::Research => "research",
            TaskClass::Validation => "validation",
            TaskClass::CodeEdit => "code-edit",
            TaskClass::Docs => "docs",
            TaskClass::Ops => "ops",
            TaskClass::General => "general",
        }
    }

    /// Deterministically bucket a task: the first recognized artifact
    /// extension (scan order = argument order) settles the class; otherwise
    /// the first keyword family (scan order below) containing ANY matching
    /// keyword does; otherwise `General`.
    pub fn derive(task_text: &str, expected_artifacts: &[String]) -> Self {
        for artifact in expected_artifacts {
            if let Some(class) = Self::from_extension(artifact) {
                return class;
            }
        }
        let folded = task_text.to_lowercase();
        for (keywords, class) in KEYWORD_FAMILIES {
            if keywords.iter().copied().any(|keyword| folded.contains(keyword)) {
                return class;
            }
        }
        TaskClass::General
    }

    /// The class implied by one expected-artifact path's extension.
    fn from_extension(path: &str) -> Option<Self> {
        let base = path.rsplit(['/', '\\']).next().unwrap_or(path);
        let extension = base.rsplit('.').next()?;
        if extension.is_empty() || !base.contains('.') {
            return None;
        }
        let class = match extension {
            "rs" | "ts" | "tsx" | "js" | "py" | "go" | "java" | "c" | "cpp" | "h" | "cs" | "rb"
            | "kt" | "swift" | "vue" => TaskClass::CodeEdit,
            "md" | "txt" | "rst" | "adoc" => TaskClass::Docs,
            "sh" | "toml" | "yaml" | "yml" | "json" | "lock" | "conf" => TaskClass::Ops,
            _ => return None,
        };
        Some(class)
    }
}

/// The keyword families in their declared scan order (order = the class
/// sites' documented derivation order).
const KEYWORD_FAMILIES: [(&[&str], TaskClass); 6] = [
    (&["design", "blueprint", "plan ", "spec ", "specif"], TaskClass::Design),
    (&["research", "investigat", "survey", "compare", "find out", "locat"], TaskClass::Research),
    (&["test", "verify", "review", "audit", "check ", "inspect", "validat"], TaskClass::Validation),
    (
        &[
            "implement",
            "write code",
            "fix ",
            "refactor",
            "add support",
            "engine ",
            "module",
            "function",
            "component",
            "api ",
        ],
        TaskClass::CodeEdit,
    ),
    (&["document", "readme", "changelog", "adr", "release notes"], TaskClass::Docs),
    (&["configure", "install", "migrate", "pipeline", "set up"], TaskClass::Ops),
];

// --------------------------------------------------------------------------
// Outcome record: bounded, decayed, per (agent, task class)
// --------------------------------------------------------------------------

/// The bounded outcome observations stored per (agent, task class). Kept
/// small on purpose: the full detail already lives in the journal (#52),
/// the diagnosis trail (#54), the ledger, and the spend records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OutcomeKind {
    /// The dispatch settled with [`AgentOutcome::Success`].
    Success,
    /// The specialist asked for revision (need-correction cycle).
    NeedsRevision,
    /// The dispatch failed or settled blocked; the label is the #54
    /// [`crate::failure_diagnosis::FailureKind`] dimension.
    Failure,
    /// The dispatch was denied by policy before any agent ran.
    Denied,
}

/// One recorded dispatch observation for a (agent, task class) key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SuitabilityOutcome {
    pub recorded_at: OffsetDateTime,
    pub kind: OutcomeKind,
    /// The #54 failure-dimension label (`failure` outcomes only, empty
    /// otherwise) — e.g. "provider", "tool".
    #[serde(default)]
    pub failure_dimension: String,
    /// Retry/recovery cost in extra decision-loop turns: the number of
    /// extra dispatches of the SAME agent for the SAME task class since
    /// its last settled success, charged to this outcome. Zero on the
    /// first attempt after success; the success completing a failure
    /// streak charges that streak.
    #[serde(default)]
    pub extra_turns: u32,
    /// Whether this outcome was itself a re-dispatch after the same
    /// agent's own pending unsuccessful outcome for the class.
    #[serde(default)]
    pub is_retry: bool,
    /// RECORDED-OBSERVATION ONLY. The dispatch's USD cost in
    /// thousandths, captured for run-metric parity. NEVER read by the
    /// score — pinned by `spend_does_not_change_ranking`.
    #[serde(default)]
    pub cost_usd_milli: u64,
}

/// Ring history for one (agent, task class): bounded newest-capped list
/// plus the descent counter for retry/recovery accounting.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SuitabilityBucket {
    pub outcomes: Vec<SuitabilityOutcome>,
    /// Consecutive unsuccessful dispatches for this key since the last
    /// success — not decayed (bookkeeping, not evidence), reset by any
    /// success record.
    #[serde(default)]
    pub pending_unsuccessful: u32,
    /// Monotonic sequence at the bucket's last record — bucket eviction
    /// and restore capping avoid re-deriving recency from wall clocks.
    #[serde(default)]
    pub last_seq: u64,
}

// --------------------------------------------------------------------------
// Thresholds — pinned public constants (tests assert them) — and decay
// --------------------------------------------------------------------------

/// Newest-capped outcomes per (agent, task class). Bounded so a history
/// can neither grow unbounded nor be dominated by one long tail.
pub const MAX_OUTCOMES_PER_KEY: usize = 16;

/// Bounded tracked (agent, task class) keys; the least-recently-recorded
/// key is evicted first when the cap is exceeded.
pub const MAX_BUCKETS: usize = 64;

/// Decay half-life in whole days: every `SUITABILITY_HALF_LIFE_DAYS` of
/// recorded age an outcome's weight halves. A fixed constant — the only
/// clock in the math is the query's `now`.
pub const SUITABILITY_HALF_LIFE_DAYS: f64 = 14.0;

/// Ages (days) after which an outcome is dropped outright (its decayed
/// weight rounds to zero): stale outcomes fade and eventually vanish.
pub const MAX_OUTCOME_AGE_DAYS: i64 = 90;

/// Score floor in milli-points. Above "excluded": candidates never drop
/// out of the ranking, and fresh successes (unbounded above) always
/// restore standing.
pub const MIN_SCORE_MILLI: i64 = -2_000;

/// The (agent, task class) key. `String` agent id — the registry id the
/// roster lists, never a model name.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SuitabilityKey {
    pub agent_id: String,
    pub task_class: TaskClass,
}

// --------------------------------------------------------------------------
// Base signal values (milli-points per outcome at full decay weight)
// --------------------------------------------------------------------------

/// Mill-point base for one fully-weighted observation: success credit.
const BASE_SUCCESS_MILLI: i64 = 1_000;
/// Mill-point base for a needs-revision outcome (recoverable, still cost).
const BASE_NEEDS_REVISION_MILLI: i64 = -400;
/// Mill-point base for a failed/blocked dispatch.
const BASE_FAILURE_MILLI: i64 = -1_000;
/// Mill-point base for a policy-denied dispatch (the agent cannot even
/// run — a strong compatibility miss).
const BASE_DENIED_MILLI: i64 = -800;
/// Additional mill-point charge per retry/recovery turn (each decision
/// turn the Coordinator spent re-dispatching the same agent for the class).
const BASE_EXTRA_TURN_MILLI: i64 = -300;
/// Additional mill-point charge when the outcome itself was a re-dispatch
/// after the same agent's own pending unsuccessful outcome.
const BASE_RETRY_MILLI: i64 = -200;

/// Reason bounding: at most this many reasons per candidate, each at most
/// [`MAX_REASON_CHARS`] characters, ranked by contribution.
const MAX_REASONS: usize = 4;
/// One reason string's hard character bound.
const MAX_REASON_CHARS: usize = 80;

// --------------------------------------------------------------------------
// Persistence snapshot (additive checkpoint field)
// --------------------------------------------------------------------------

/// The serde snapshot carried inside checkpoints so suitability survives a
/// resume. Bounded on restore (older records may exceed the caps).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SuitabilityState {
    #[serde(default)]
    pub buckets: Vec<SuitabilityKeyedBucket>,
}

/// A bounded bucket paired with its identity for the additive snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SuitabilityKeyedBucket {
    pub key: SuitabilityKey,
    pub bucket: SuitabilityBucket,
}

impl SuitabilityState {
    /// Enforce the caps on restore: older records may exceed the ring
    /// bounds or carry more keys than the current cap; most-recently-
    /// recorded keys and newest outcomes survive.
    fn enforce_caps(&mut self) {
        self.buckets.sort_by_key(|keyed| std::cmp::Reverse(keyed.bucket.last_seq));
        self.buckets.truncate(MAX_BUCKETS);
        self.buckets.sort_by(|a, b| a.key.cmp(&b.key));
        for keyed in &mut self.buckets {
            let outcomes_len = keyed.bucket.outcomes.len();
            if outcomes_len > MAX_OUTCOMES_PER_KEY {
                keyed.bucket.outcomes.drain(0..outcomes_len - MAX_OUTCOMES_PER_KEY);
            }
        }
    }
}

// --------------------------------------------------------------------------
// The index
// --------------------------------------------------------------------------

/// The suitability record: bounded dispatch-outcome history per (agent,
/// task class) plus the retry/recovery descent bookkeeping. Owned by the
/// Coordinator; persisted additively through checkpoints.
#[derive(Debug, Clone, Default)]
pub struct SuitabilityIndex {
    buckets: BTreeMap<SuitabilityKey, SuitabilityBucket>,
    /// Monotonic record counter — bucket eviction keeps the least-
    /// recently-recorded key out.
    seq: u64,
    /// The most recently recorded dispatch (key + whether it settled
    /// successfully) for retry/recovery accounting across buckets.
    last_dispatch: Option<(SuitabilityKey, bool)>,
}

/// One agent's deterministic suitability result for a query.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSuitability {
    pub agent_id: String,
    /// Clamped milli-point score (see `MIN_SCORE_MILLI`).
    pub score_milli: i64,
    /// Bounded, contributing-first reason strings.
    pub reasons: Vec<String>,
}

/// One reason bin: a bounded aggregate of one outcome kind (plus its
/// failure dimension) or the extra-turns bucket, so the reason list
/// never exceeds `MAX_REASONS` lines.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum ReasonBin {
    ExtraTurns,
    Denied,
    NeedsRevision,
    Failure(String),
    Success,
}

/// The bin an outcome's base signal aggregates into.
fn outcome_bin(outcome: &SuitabilityOutcome) -> ReasonBin {
    match outcome.kind {
        OutcomeKind::Success => ReasonBin::Success,
        OutcomeKind::NeedsRevision => ReasonBin::NeedsRevision,
        OutcomeKind::Denied => ReasonBin::Denied,
        OutcomeKind::Failure => ReasonBin::Failure(outcome.failure_dimension.clone()),
    }
}

/// Bin accumulator: outcome count, summed contribution (milli), summed
/// decay weight (thousandths), summed extra turns.
type BinAcc = (u32, i64, u64, u32);

fn add_bin(
    bins: &mut BTreeMap<ReasonBin, BinAcc>,
    label: ReasonBin,
    contribution: i64,
    weight_milli: u64,
    turns: u32,
) {
    let entry = bins.entry(label).or_insert((0, 0, 0, 0));
    entry.0 += 1;
    entry.1 += contribution;
    entry.2 += weight_milli;
    entry.3 += turns;
}

impl SuitabilityIndex {
    /// Record ONE dispatch observation. `failure_dimension` is the #54
    /// `FailureKind` label for Failure outcomes (empty otherwise).
    /// `cost_usd_milli` is RECORDED-OBSERVATION ONLY — it never enters the
    /// score (pinned by `spend_does_not_change_ranking`).
    pub fn record(
        &mut self,
        agent_id: &str,
        task_class: TaskClass,
        kind: OutcomeKind,
        failure_dimension: Option<&str>,
        now: OffsetDateTime,
        cost_usd_milli: u64,
    ) {
        let key = SuitabilityKey { agent_id: agent_id.to_owned(), task_class };
        let is_retry = matches!(&self.last_dispatch, Some((last, false)) if *last == key);
        let bucket = self.buckets.entry(key.clone()).or_default();
        let (extra_turns, pending_unsuccessful) = if kind == OutcomeKind::Success {
            (bucket.pending_unsuccessful, 0)
        } else {
            (0, bucket.pending_unsuccessful.saturating_add(1))
        };
        self.seq += 1;
        bucket.last_seq = self.seq;
        bucket.pending_unsuccessful = pending_unsuccessful;
        bucket.outcomes.push(SuitabilityOutcome {
            recorded_at: now,
            kind,
            failure_dimension: failure_dimension.unwrap_or_default().to_owned(),
            extra_turns,
            is_retry,
            cost_usd_milli,
        });
        if bucket.outcomes.len() > MAX_OUTCOMES_PER_KEY {
            bucket.outcomes.remove(0);
        }
        self.last_dispatch = Some((key, kind == OutcomeKind::Success));
        self.evict_overflowing_buckets();
    }

    /// Keep at most `MAX_BUCKETS` keys: the least-recently-recorded
    /// bucket (lowest `last_seq`) drops first.
    fn evict_overflowing_buckets(&mut self) {
        while self.buckets.len() > MAX_BUCKETS {
            let evict = self
                .buckets
                .iter()
                .min_by_key(|(_, bucket)| bucket.last_seq)
                .map(|(key, _)| key.clone());
            let Some(key) = evict else { break };
            self.buckets.remove(&key);
        }
    }

    /// The bounded persistence snapshot carried in checkpoints.
    pub fn state(&self) -> SuitabilityState {
        let mut keyed: Vec<SuitabilityKeyedBucket> = self
            .buckets
            .iter()
            .map(|(key, bucket)| SuitabilityKeyedBucket {
                key: key.clone(),
                bucket: bucket.clone(),
            })
            .collect();
        keyed.sort_by(|a, b| a.key.cmp(&b.key));
        SuitabilityState { buckets: keyed }
    }

    /// Restore from the additive checkpoint snapshot (bounded; older
    /// records may exceed the caps — the caps are enforced here).
    pub fn from_state(state: SuitabilityState) -> Self {
        let mut bounded = state;
        bounded.enforce_caps();
        let mut buckets = BTreeMap::new();
        for keyed in bounded.buckets {
            // Duplicate keys (none by construction) degrade to last-wins.
            buckets.insert(keyed.key, keyed.bucket);
        }
        SuitabilityIndex { buckets, ..SuitabilityIndex::default() }
    }

    /// True when nothing is recorded at all (an empty history is the
    /// neutral default, not an error).
    pub fn is_empty(&self) -> bool {
        self.buckets.is_empty()
    }

    /// Rank candidate agents for a task class, deterministically, with
    /// bounded reasons. Includes EVERY candidate (never excludes — the
    /// floor guarantees standing above exclusion), sorted by score
    /// descending then agent id ascending. A pure function of (recorded
    /// outcomes, candidates, `now`): no cost, spend, latency, workload,
    /// or model-quality input exists on this path.
    pub fn rank(
        &self,
        candidates: &[String],
        task_class: TaskClass,
        now: OffsetDateTime,
    ) -> Vec<AgentSuitability> {
        let mut seen = std::collections::BTreeSet::new();
        let mut ranked = Vec::new();
        for agent_id in candidates {
            if seen.insert(agent_id.clone()) {
                ranked.push(self.score(agent_id, task_class, now));
            }
        }
        ranked.sort_by(|a, b| {
            b.score_milli.cmp(&a.score_milli).then_with(|| a.agent_id.cmp(&b.agent_id))
        });
        ranked
    }

    /// Score ONE candidate: the clamped decay-weighted signal sum with
    /// bounded, contributing-first reasons.
    fn score(
        &self,
        agent_id: &str,
        task_class: TaskClass,
        now: OffsetDateTime,
    ) -> AgentSuitability {
        let key = SuitabilityKey { agent_id: agent_id.to_owned(), task_class };
        let Some(bucket) = self.buckets.get(&key) else {
            return AgentSuitability {
                agent_id: agent_id.to_owned(),
                score_milli: 0,
                reasons: vec!["no recorded outcomes for this task class".to_owned()],
            };
        };

        let mut raw_milli: i64 = 0;
        let mut bins: BTreeMap<ReasonBin, BinAcc> = BTreeMap::new();
        for outcome in &bucket.outcomes {
            let weight_milli = decay_weight_milli(&outcome.recorded_at, now);
            if weight_milli == 0 {
                continue;
            }
            let factor = i64::from(weight_milli);
            let base = match outcome.kind {
                OutcomeKind::Success => BASE_SUCCESS_MILLI,
                OutcomeKind::NeedsRevision => BASE_NEEDS_REVISION_MILLI,
                OutcomeKind::Failure => BASE_FAILURE_MILLI,
                OutcomeKind::Denied => BASE_DENIED_MILLI,
            };
            let outcome_contribution = (base * factor) / 1_000;
            raw_milli += outcome_contribution;
            add_bin(
                &mut bins,
                outcome_bin(outcome),
                outcome_contribution,
                u64::from(weight_milli),
                outcome.extra_turns,
            );
            if outcome.extra_turns > 0 {
                let turns = i64::from(outcome.extra_turns);
                let turns_contribution = (BASE_EXTRA_TURN_MILLI * factor) / 1_000 * turns;
                raw_milli += turns_contribution;
                add_bin(
                    &mut bins,
                    ReasonBin::ExtraTurns,
                    turns_contribution,
                    u64::from(weight_milli),
                    outcome.extra_turns,
                );
            }
            if outcome.is_retry {
                // The retry charge rides the outcome's own bin (a retry
                // IS a dispatch observation); it changes the score, not
                // the reason-line count. The retry contribution is added
                // directly here and folded into the outcome bin's total
                // for the reason text below.
                let retry_contribution = (BASE_RETRY_MILLI * factor) / 1_000;
                raw_milli += retry_contribution;
                if let Some(entry) = bins.get_mut(&outcome_bin(outcome)) {
                    entry.1 += retry_contribution;
                }
            }
        }
        if bins.is_empty() {
            return AgentSuitability {
                agent_id: agent_id.to_owned(),
                score_milli: 0,
                reasons: vec![bound_text(
                    "no fresh outcomes for this task class (history decayed)",
                    MAX_REASON_CHARS,
                )],
            };
        }
        let score_milli = raw_milli.max(MIN_SCORE_MILLI);
        AgentSuitability {
            agent_id: agent_id.to_owned(),
            score_milli,
            reasons: reason_strings(bins),
        }
    }
}

/// Build the bounded reason list: bins sorted by summed contribution
/// (most contributing first, ties by bin label), capped to
/// `MAX_REASONS`, each line bounded to `MAX_REASON_CHARS`.
fn reason_strings(bins: BTreeMap<ReasonBin, BinAcc>) -> Vec<String> {
    let mut rows: Vec<(ReasonBin, BinAcc)> = bins.into_iter().collect();
    // Ascending contribution = most-negative/most-contributing first;
    // the sort is total (label tie-break) so determinism holds.
    rows.sort_by(|a, b| a.1 .1.cmp(&b.1 .1).then_with(|| a.0.cmp(&b.0)));
    rows.into_iter()
        .take(MAX_REASONS)
        .map(|(bin, acc)| bound_text(&reason_line(bin, &acc), MAX_REASON_CHARS))
        .collect()
}

/// Format ONE reason line (factoring counts, decayed weight, and the
/// extra-turn charges into bounded text).
fn reason_line(bin: ReasonBin, acc: &BinAcc) -> String {
    match bin {
        ReasonBin::ExtraTurns => {
            let mut line = format!("extra retry/recovery turns: {}", acc.0);
            if acc.3 > 0 {
                line.push_str(&format!(" ({} turns)", acc.3));
            }
            line
        }
        ReasonBin::Success => format!(
            "{} success (decayed weight {:.1})",
            acc.0,
            f64::from(u16::try_from(acc.2).unwrap_or(9_999)) / 1_000.0
        ),
        ReasonBin::Denied => format!(
            "{} denied (decayed weight {:.1})",
            acc.0,
            f64::from(u16::try_from(acc.2).unwrap_or(9_999)) / 1_000.0
        ),
        ReasonBin::NeedsRevision => format!(
            "{} needs-revision (decayed weight {:.1})",
            acc.0,
            f64::from(u16::try_from(acc.2).unwrap_or(9_999)) / 1_000.0
        ),
        ReasonBin::Failure(dimension) if dimension.is_empty() => format!(
            "{} failure (decayed weight {:.1})",
            acc.0,
            f64::from(u16::try_from(acc.2).unwrap_or(9_999)) / 1_000.0
        ),
        ReasonBin::Failure(dimension) => format!(
            "{} failure:{dimension} (decayed weight {:.1})",
            acc.0,
            f64::from(u16::try_from(acc.2).unwrap_or(9_999)) / 1_000.0
        ),
    }
}

/// Hard truncate a bounded string at a char boundary (≤ `max` chars).
pub fn bound_reasons(reasons: &[String]) -> String {
    reasons.iter().map(String::as_str).collect::<Vec<_>>().join("; ")
}

fn bound_text(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

/// The bounded decay weight of one outcome (thousandths of full credit
/// remaining at `now`): fixed half-life, integer-rounded math, zero
/// (dropped) from `MAX_OUTCOME_AGE_DAYS` on. Future-stamped outcomes
/// (clock drift) clamp to full weight. Deterministic: same inputs →
/// same result.
fn decay_weight_milli(recorded_at: &OffsetDateTime, now: OffsetDateTime) -> u32 {
    let age_ms = (now - *recorded_at).whole_milliseconds();
    if age_ms <= 0 {
        return 1_000;
    }
    let age_days = age_ms as f64 / 86_400_000.0;
    if age_days >= MAX_OUTCOME_AGE_DAYS as f64 {
        return 0;
    }
    let weight = (1_000.0 * (0.5f64).powf(age_days / SUITABILITY_HALF_LIFE_DAYS)).round();
    if weight <= 0.0 {
        0
    } else if weight >= 1_000.0 {
        1_000
    } else {
        weight as u32
    }
}

// --------------------------------------------------------------------------
// Tests
// --------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic test clock: one fixed epoch plus whole days —
    /// `days` advances the simulation, so decay tests never touch a
    /// real clock.
    fn clock(days_after_epoch: i64) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_700_000_000 + days_after_epoch * 86_400)
            .unwrap_or_else(|error| panic!("test clock out of range: {error}"))
    }

    fn ids(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    #[test]
    fn task_class_derivation_is_deterministic() {
        let artifacts = vec!["src/main.rs".to_owned()];
        assert_eq!(
            TaskClass::derive("anything at all", &artifacts),
            TaskClass::derive("anything at all", &artifacts)
        );
        assert_eq!(TaskClass::derive("anything", &artifacts), TaskClass::CodeEdit);
        assert_eq!(TaskClass::derive("docs", &["README.md".to_owned()]), TaskClass::Docs);
        assert_eq!(TaskClass::derive("run the tests", &[]), TaskClass::Validation);
        assert_eq!(TaskClass::derive("design the architecture", &[]), TaskClass::Design);
        assert_eq!(TaskClass::derive("research the options", &[]), TaskClass::Research);
        assert_eq!(TaskClass::derive("do a thing", &[]), TaskClass::General);
        // Same task twice → identical class (determinism acceptance).
        assert_eq!(TaskClass::derive("refactor the parser", &[]), TaskClass::CodeEdit);
        assert_eq!(TaskClass::derive("refactor the parser", &[]), TaskClass::CodeEdit);
    }

    #[test]
    fn seeded_histories_rank_differently() {
        // coder: 3 recent successes; reviewer: 3 recent failures.
        let mut good = SuitabilityIndex::default();
        let mut bad = SuitabilityIndex::default();
        for day in 0..3 {
            good.record("coder", TaskClass::CodeEdit, OutcomeKind::Success, None, clock(day), 500);
            bad.record(
                "reviewer",
                TaskClass::CodeEdit,
                OutcomeKind::Failure,
                Some("tool"),
                clock(day),
                0,
            );
        }
        let candidates = ids(&["coder", "reviewer"]);
        let ranked_good = good.rank(&candidates, TaskClass::CodeEdit, clock(4));
        assert_eq!(ranked_good.first().unwrap().agent_id, "coder");
        assert!(ranked_good.first().unwrap().score_milli > 0);
        assert!(!ranked_good.first().unwrap().reasons.is_empty());

        let ranked_bad = bad.rank(&candidates, TaskClass::CodeEdit, clock(4));
        // The failed candidate drops to LAST (never out of the list): a
        // history-less neutral beats a negative history.
        assert_eq!(ranked_bad.last().unwrap().agent_id, "reviewer");
        assert!(ranked_bad.last().unwrap().score_milli < 0);
        // Same agent, opposite histories: coder ranks above reviewer when
        // its history is full of successes, below reviewer's neutral
        // competitor when its history is failures — the history moves the
        // ranking deterministically.
        let mut flipped = SuitabilityIndex::default();
        flipped.record("reviewer", TaskClass::CodeEdit, OutcomeKind::Success, None, clock(1), 0);
        let flipped_rank = flipped.rank(&candidates, TaskClass::CodeEdit, clock(4));
        assert_eq!(flipped_rank.first().unwrap().agent_id, "reviewer");
        // The good history ranks its agent above the same agent in a
        // failed-history world — history MOVES the ranking.
        assert!(ranked_good.first().unwrap().score_milli > ranked_bad.last().unwrap().score_milli);
        // Every candidate stays in the ranking, even a fully negative one.
        assert_eq!(ranked_good.len(), 2);
        assert_eq!(ranked_bad.len(), 2);
    }

    #[test]
    fn same_history_and_query_rank_identically_twice() {
        let mut index = SuitabilityIndex::default();
        index.record("coder", TaskClass::CodeEdit, OutcomeKind::Success, None, clock(1), 1_000);
        index.record(
            "coder",
            TaskClass::CodeEdit,
            OutcomeKind::Failure,
            Some("provider"),
            clock(2),
            2_500,
        );
        index.record(
            "reviewer",
            TaskClass::CodeEdit,
            OutcomeKind::NeedsRevision,
            None,
            clock(2),
            0,
        );
        let candidates = ids(&["coder", "reviewer", "coder"]);
        let first = index.rank(&candidates, TaskClass::CodeEdit, clock(3));
        let second = index.rank(&candidates, TaskClass::CodeEdit, clock(3));
        assert_eq!(first, second);
        // Two independent indexes recording the same history agree too.
        let mut rebuilt = SuitabilityIndex::default();
        rebuilt.record("coder", TaskClass::CodeEdit, OutcomeKind::Success, None, clock(1), 1_000);
        rebuilt.record(
            "coder",
            TaskClass::CodeEdit,
            OutcomeKind::Failure,
            Some("provider"),
            clock(2),
            2_500,
        );
        rebuilt.record(
            "reviewer",
            TaskClass::CodeEdit,
            OutcomeKind::NeedsRevision,
            None,
            clock(2),
            0,
        );
        assert_eq!(first, rebuilt.rank(&candidates, TaskClass::CodeEdit, clock(3)));
    }

    #[test]
    fn old_outcomes_fade_and_eventually_drop() {
        // "veteran" succeeded 60 days ago; "rookie" succeeded today.
        // At the shared query instant the veteran is nearly 4.3
        // half-lives stale, so the fresh outcome outranks it — stale
        // evidence does not dominate.
        let mut combined = SuitabilityIndex::default();
        combined.record("veteran", TaskClass::CodeEdit, OutcomeKind::Success, None, clock(0), 0);
        combined.record("rookie", TaskClass::CodeEdit, OutcomeKind::Success, None, clock(60), 0);
        let ranked = combined.rank(&ids(&["veteran", "rookie"]), TaskClass::CodeEdit, clock(60));
        assert_eq!(ranked.first().unwrap().agent_id, "rookie");
        assert!(ranked.first().unwrap().score_milli > 0);
        assert!(ranked.last().unwrap().score_milli > 0, "still above floor, still ranked");

        // Past MAX_OUTCOME_AGE_DAYS the old outcome is dropped outright:
        // the query treats the agent as history-less (score 0, one
        // bounded reason).
        let mut ancient = SuitabilityIndex::default();
        ancient.record("ancient", TaskClass::CodeEdit, OutcomeKind::Success, None, clock(0), 0);
        let ranked = ancient.rank(&ids(&["ancient"]), TaskClass::CodeEdit, clock(100));
        assert_eq!(ranked.first().unwrap().score_milli, 0);
        assert_eq!(ranked.first().unwrap().reasons.len(), 1);
    }

    #[test]
    fn single_failure_never_excludes_and_floor_is_pinned() {
        let mut index = SuitabilityIndex::default();
        let mut recovered_single = SuitabilityIndex::default();
        for suite_index in [&mut index, &mut recovered_single] {
            suite_index.record(
                "coder",
                TaskClass::CodeEdit,
                OutcomeKind::Failure,
                Some("tool"),
                clock(0),
                0,
            );
        }
        let ranked = index.rank(&ids(&["coder"]), TaskClass::CodeEdit, clock(1));
        let failed_single_score = ranked.first().unwrap().score_milli;
        assert_eq!(ranked.len(), 1, "a failed candidate is still ranked");
        assert!(
            ranked.first().unwrap().score_milli >= MIN_SCORE_MILLI,
            "score floor holds: {} >= {MIN_SCORE_MILLI}",
            ranked.first().unwrap().score_milli
        );
        // Long failure history CLAMPS to the floor, never below it —
        // exclusion never happens and recovery is always possible.
        for day in 0..MAX_OUTCOMES_PER_KEY as i64 {
            index.record(
                "coder",
                TaskClass::CodeEdit,
                OutcomeKind::Failure,
                Some("tool"),
                clock(day),
                0,
            );
        }
        let flooded = index.score("coder", TaskClass::CodeEdit, clock(MAX_OUTCOMES_PER_KEY as i64));
        assert_eq!(flooded.score_milli, MIN_SCORE_MILLI);
        // Recovery: one fresh success lifts the single-failure standing.
        recovered_single.record(
            "coder",
            TaskClass::CodeEdit,
            OutcomeKind::Success,
            None,
            clock(2),
            0,
        );
        let recovered = recovered_single.rank(&ids(&["coder"]), TaskClass::CodeEdit, clock(3));
        assert!(
            recovered.first().unwrap().score_milli > failed_single_score,
            "a failed agent that succeeds again rises above its failed score"
        );
    }

    #[test]
    fn recovery_restores_standing() {
        // Two agents each fail, then only one recovers with a success.
        let mut recovering = SuitabilityIndex::default();
        let mut stuck = SuitabilityIndex::default();
        for index in [&mut recovering, &mut stuck] {
            index.record(
                "coder",
                TaskClass::CodeEdit,
                OutcomeKind::Failure,
                Some("tool"),
                clock(1),
                0,
            );
        }
        recovering.record("coder", TaskClass::CodeEdit, OutcomeKind::Success, None, clock(2), 0);
        let candidates = ids(&["coder"]);
        let recovered = recovering.rank(&candidates, TaskClass::CodeEdit, clock(3));
        let staled = stuck.rank(&candidates, TaskClass::CodeEdit, clock(3));
        assert!(recovered.first().unwrap().score_milli > staled.first().unwrap().score_milli);
        // The recovery cost (the failed attempt) is visible in reasons.
        assert!(recovered
            .first()
            .unwrap()
            .reasons
            .iter()
            .any(|reason| reason.contains("failure")
                || reason.contains("extra retry/recovery turns")));
    }

    #[test]
    fn spend_does_not_change_ranking() {
        // Identical outcome histories for two candidate pairs that differ
        // ONLY in the recorded (observed) spend. The ranking MUST be
        // identical — cost is recorded, never scored.
        let low_cost = seeded_with_cost(1);
        let high_cost = seeded_with_cost(250_000);
        let candidates = ids(&["coder", "reviewer"]);
        assert_eq!(
            low_cost.rank(&candidates, TaskClass::CodeEdit, clock(5)),
            high_cost.rank(&candidates, TaskClass::CodeEdit, clock(5))
        );
    }

    fn seeded_with_cost(per_success_cost_milli: u64) -> SuitabilityIndex {
        let mut index = SuitabilityIndex::default();
        for day in 0..3 {
            index.record(
                "coder",
                TaskClass::CodeEdit,
                OutcomeKind::Success,
                None,
                clock(day),
                per_success_cost_milli,
            );
            index.record("reviewer", TaskClass::CodeEdit, OutcomeKind::Denied, None, clock(day), 0);
        }
        index
    }

    #[test]
    fn bounded_store_cap_across_keys() {
        // MAX_BUCKETS keeps only the MAX_BUCKETS most-recently-recorded keys.
        let mut index = SuitabilityIndex::default();
        for day in 0..200u32 {
            let agent = format!("agent-{day}");
            index.record(
                &agent,
                TaskClass::CodeEdit,
                OutcomeKind::Success,
                None,
                clock(i64::from(day) / 2),
                0,
            );
        }
        assert!(!index.is_empty());
        assert!(mock_len(&index) <= MAX_BUCKETS, "in-memory buckets bounded");
        // Recently recorded keys survive eviction.
        let ranked = index.rank(&ids(&["agent-199"]), TaskClass::CodeEdit, clock(50));
        assert!(ranked.first().unwrap().score_milli > 0);
    }

    /// Test-only observation of the bounded store size (the map is kept
    /// private; the snapshot is the observable surface).
    fn mock_len(index: &SuitabilityIndex) -> usize {
        index.state().buckets.len()
    }

    #[test]
    fn outcome_ring_is_bounded_per_key() {
        let mut index = SuitabilityIndex::default();
        for day in 0..40 {
            index.record("coder", TaskClass::CodeEdit, OutcomeKind::Success, None, clock(day), 0);
        }
        let snapshot = index.state();
        let bucket =
            snapshot.buckets.first().unwrap_or_else(|| panic!("bucket present after records"));
        assert_eq!(bucket.bucket.outcomes.len(), MAX_OUTCOMES_PER_KEY);
    }

    #[test]
    fn state_round_trips_and_ranking_survives() {
        let mut index = SuitabilityIndex::default();
        index.record("coder", TaskClass::CodeEdit, OutcomeKind::Success, None, clock(1), 1_200);
        index.record(
            "coder",
            TaskClass::CodeEdit,
            OutcomeKind::Failure,
            Some("provider"),
            clock(2),
            3_000,
        );
        index.record("coder", TaskClass::CodeEdit, OutcomeKind::Success, None, clock(3), 2_200);
        let json = serde_json::to_value(index.state()).unwrap_or_default();
        let restored =
            SuitabilityIndex::from_state(serde_json::from_value(json).unwrap_or_default());
        assert_eq!(
            index.rank(&ids(&["coder"]), TaskClass::CodeEdit, clock(4)),
            restored.rank(&ids(&["coder"]), TaskClass::CodeEdit, clock(4)),
            "restored suitability ranks identically (resume acceptance)"
        );
    }

    #[test]
    fn restore_enforces_caps_on_oversized_state() {
        let oversized = SuitabilityState {
            buckets: (0..MAX_BUCKETS * 4)
                .map(|i| {
                    let index_key = SuitabilityKey {
                        agent_id: format!("agent-{i}"),
                        task_class: TaskClass::CodeEdit,
                    };
                    let bucket = SuitabilityBucket {
                        outcomes: (0..MAX_OUTCOMES_PER_KEY * 3)
                            .map(|j| SuitabilityOutcome {
                                recorded_at: clock(j as i64),
                                kind: OutcomeKind::Success,
                                failure_dimension: String::new(),
                                extra_turns: 0,
                                is_retry: false,
                                cost_usd_milli: 0,
                            })
                            .collect(),
                        pending_unsuccessful: 0,
                        last_seq: i as u64,
                    };
                    SuitabilityKeyedBucket { key: index_key, bucket }
                })
                .collect(),
        };
        let restored = SuitabilityIndex::from_state(oversized);
        assert_eq!(restored.state().buckets.len(), MAX_BUCKETS);
        let ranked = restored.rank(&ids(&["agent-0"]), TaskClass::CodeEdit, clock(100));
        assert!(!ranked.first().unwrap().reasons.is_empty());
    }
}
