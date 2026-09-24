//! Recall cost bounds — char-budget allocator, timeout contract, and the
//! structured recall capability envelope (TODO.md "Recall budget caps +
//! timeout guard").
//!
//! **Naming trap.** [`crate::chunk_selector`] selects messages to *compact*
//! when the context window is under pressure (`target_recovery_pct`,
//! `trigger_pct`, `target_pct`); those constants have nothing to do with
//! retrieval. Recall allocation — how ONE recall pass may spend characters
//! and wall clock — lives here and is consumed by
//! [`crate::rag::HybridRetriever`].
//!
//! Recall is advisory: a breach never fails the turn. Char caps truncate the
//! selected results (the budget is distributed across them, never spent
//! whole on the top-k) and a timeout skips the pass entirely, with a warning
//! and a structured [`RecallFailure`] code instead of an `Err`.

use std::fmt;
use std::time::Duration;

/// Default per-recall wall-clock budget: 5000 ms (TODO.md "Recall budget
/// caps + timeout guard").
pub const DEFAULT_RECALL_TIMEOUT_MS: u64 = 5000;

/// Hard cost bounds for a single recall pass.
///
/// Every char field counts Unicode scalar values (`char`s) and reads `0` as
/// "no cap", matching the reference config (`maxCharsPerMemory=0`,
/// `maxTotalRecallChars=0`) so a default construction only adds the timeout
/// guard to today's behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecallCostBounds {
    /// Result cap applied on top of the query's `top_k`. `0` defers to
    /// `top_k` (the default: a browse-style query may legitimately ask for
    /// many results, and `top_k` already bounds it).
    pub max_results: usize,
    /// Per-memory content cap in chars. `0` = unlimited.
    pub max_chars_per_memory: usize,
    /// Total budget shared by ALL selected results, in chars. `0` =
    /// unlimited. Distributed across the results by
    /// [`allocate_char_budget`] rather than spent whole on the first ones.
    pub max_total_chars: usize,
    /// Wall-clock budget for one recall pass. On breach the pass is skipped
    /// (warning + empty result) instead of blocking the turn.
    pub timeout: Duration,
}

impl Default for RecallCostBounds {
    fn default() -> Self {
        Self {
            max_results: 0,
            max_chars_per_memory: 0,
            max_total_chars: 0,
            timeout: Duration::from_millis(DEFAULT_RECALL_TIMEOUT_MS),
        }
    }
}

impl RecallCostBounds {
    /// The timeout as whole milliseconds (the envelope's wire-friendly form).
    pub fn timeout_ms(&self) -> u64 {
        u64::try_from(self.timeout.as_millis()).unwrap_or(u64::MAX)
    }

    /// The capability envelope: the declared cost bounds of one recall pass.
    ///
    /// A caller reads this to know what a recall may cost — bounded results,
    /// bounded characters, bounded wall clock — before it asks for one.
    pub fn envelope(&self) -> RecallCapabilityEnvelope {
        RecallCapabilityEnvelope {
            max_results: self.max_results,
            max_chars_per_memory: self.max_chars_per_memory,
            max_total_chars: self.max_total_chars,
            timeout_ms: self.timeout_ms(),
        }
    }
}

/// Declared cost bounds of the recall capability (the capability-envelope
/// payload). `0` means "no cap" for every field, mirroring
/// [`RecallCostBounds`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecallCapabilityEnvelope {
    /// Result cap on top of the query's `top_k`; `0` defers to `top_k`.
    pub max_results: usize,
    /// Per-memory content cap in chars; `0` = unlimited.
    pub max_chars_per_memory: usize,
    /// Total char budget across all selected results; `0` = unlimited.
    pub max_total_chars: usize,
    /// Per-recall wall-clock budget in milliseconds.
    pub timeout_ms: u64,
}

/// Structured failure for a recall that was skipped. Recall never returns an
/// `Err` for a budget breach (the failure travels as a warning carrying this
/// code, and the recall yields no results) — it must never block the turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecallFailure {
    /// Machine-readable failure code.
    pub code: RecallFailureCode,
    /// Human-readable detail.
    pub message: String,
}

impl RecallFailure {
    /// The pass exceeded its wall-clock budget and was skipped.
    pub fn timed_out(timeout: Duration) -> Self {
        Self {
            code: RecallFailureCode::Timeout,
            message: format!("recall exceeded its {} ms budget", timeout.as_millis()),
        }
    }
}

impl fmt::Display for RecallFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

/// Machine-readable recall failure codes (structured failure envelope).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecallFailureCode {
    /// The pass exceeded [`RecallCostBounds::timeout`] and was skipped.
    Timeout,
}

impl fmt::Display for RecallFailureCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Timeout => f.write_str("recall_timeout"),
        }
    }
}

/// Distribute `budget` chars across `char_counts` by equal-share
/// water-filling: every selected entry gets an equal slice of what remains,
/// an entry that needs less than its slice returns the difference, and the
/// remainder is redistributed to the entries that can still use more.
///
/// This is the "budget across the selected chunks, not top-k whole"
/// allocator: the budget is never spent whole on the first entry while
/// later entries get nothing.
///
/// Returns one cap per input (same order). The caps sum to at most `budget`
/// and never exceed their entry's `char_counts[i]`.
pub fn allocate_char_budget(char_counts: &[usize], budget: usize) -> Vec<usize> {
    let mut allocated = vec![0usize; char_counts.len()];
    let mut remaining = budget;
    // Zero-length entries need no budget and never occupy an active slot.
    let mut active: Vec<usize> =
        (0..char_counts.len()).filter(|&idx| char_counts[idx] > 0).collect();

    while remaining > 0 && !active.is_empty() {
        let share = remaining / active.len();
        if share == 0 {
            // Fewer chars left than active entries: hand them out one at a
            // time in rank order (each bounded by its own need).
            for &idx in &active {
                if remaining == 0 {
                    break;
                }
                allocated[idx] += 1;
                remaining -= 1;
            }
            break;
        }
        let mut still_active = Vec::with_capacity(active.len());
        for &idx in &active {
            // Invariant: `allocated[idx] < char_counts[idx]` for active entries.
            let want = char_counts[idx] - allocated[idx];
            let give = want.min(share);
            allocated[idx] += give;
            remaining -= give;
            if allocated[idx] < char_counts[idx] {
                still_active.push(idx);
            }
        }
        // Every active entry has `want >= 1`, so each round with `share >= 1`
        // consumes at least `active.len()` chars — the loop terminates.
        active = still_active;
    }
    allocated
}

/// Truncate `text` in place to at most `max_chars` Unicode scalar values,
/// cutting only at a `char` boundary (the UTF-8 analogue of "never split a
/// surrogate pair").
pub fn truncate_to_chars(text: &mut String, max_chars: usize) {
    if text.chars().count() <= max_chars {
        return;
    }
    // `nth` is `Some` here (the count exceeded `max_chars`); the fallback
    // keeps the text whole rather than guessing a byte offset.
    let boundary = text.char_indices().nth(max_chars).map_or(text.len(), |(idx, _)| idx);
    text.truncate(boundary);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_is_split_evenly_when_every_entry_is_hungry() {
        let alloc = allocate_char_budget(&[100, 100], 100);
        assert_eq!(alloc, vec![50, 50], "an equal split, not first-come-whole");
        assert_eq!(alloc.iter().sum::<usize>(), 100, "the budget is fully used");
    }

    /// The allocator redistributes: an entry that needs less than its share
    /// returns the difference instead of hoarding it.
    #[test]
    fn budget_redistributes_the_surplus_of_small_entries() {
        // Round 1: share 40 each → [40, 10, 10]; 60 chars left for entry 0.
        let alloc = allocate_char_budget(&[100, 10, 10], 120);
        assert_eq!(alloc, vec![100, 10, 10]);
        assert_eq!(alloc.iter().sum::<usize>(), 120);
    }

    /// Budget smaller than the total demand: every selected entry still gets
    /// a slice (no entry is dropped whole).
    #[test]
    fn budget_smaller_than_demand_reaches_every_entry() {
        let alloc = allocate_char_budget(&[10, 10, 10], 8);
        assert_eq!(alloc, vec![3, 3, 2]);
        assert!(alloc.iter().all(|&cap| cap > 0), "no entry is starved to zero");
        assert_eq!(alloc.iter().sum::<usize>(), 8, "never exceeds the budget");
    }

    #[test]
    fn zero_length_entries_never_consume_budget() {
        let alloc = allocate_char_budget(&[0, 40], 10);
        assert_eq!(alloc, vec![0, 10]);
    }

    #[test]
    fn empty_input_and_zero_budget_are_inert() {
        assert!(allocate_char_budget(&[], 100).is_empty());
        assert_eq!(allocate_char_budget(&[5, 5], 0), vec![0, 0]);
    }

    /// Char caps cut at a `char` boundary: multibyte content is never split
    /// into invalid UTF-8.
    #[test]
    fn truncate_never_splits_a_code_point() {
        let mut text = "héllo wörld".to_string(); // 11 chars, 13 bytes
        truncate_to_chars(&mut text, 5);
        assert_eq!(text, "héllo");
        assert!(text.is_char_boundary(text.len()), "truncation stays on a boundary");

        let mut emoji = "🚀🚀🚀".to_string();
        truncate_to_chars(&mut emoji, 2);
        assert_eq!(emoji, "🚀🚀");

        let mut short = "abc".to_string();
        truncate_to_chars(&mut short, 10);
        assert_eq!(short, "abc", "content under the cap is untouched");
    }

    /// Envelope declaration: the bounds are reported verbatim, in ms.
    #[test]
    fn envelope_declares_the_configured_cost_bounds() {
        let bounds = RecallCostBounds {
            max_results: 5,
            max_chars_per_memory: 800,
            max_total_chars: 4000,
            timeout: Duration::from_millis(5000),
        };
        assert_eq!(
            bounds.envelope(),
            RecallCapabilityEnvelope {
                max_results: 5,
                max_chars_per_memory: 800,
                max_total_chars: 4000,
                timeout_ms: 5000,
            }
        );
    }

    /// The default envelope keeps today's caps unlimited and declares the
    /// 5000 ms timeout guard.
    #[test]
    fn default_envelope_declares_timeout_and_unlimited_chars() {
        let envelope = RecallCostBounds::default().envelope();
        assert_eq!(envelope.timeout_ms, DEFAULT_RECALL_TIMEOUT_MS);
        assert_eq!(envelope.max_results, 0, "0 defers to the query's top_k");
        assert_eq!(envelope.max_chars_per_memory, 0);
        assert_eq!(envelope.max_total_chars, 0);
    }

    #[test]
    fn timeout_failure_carries_a_structured_code() {
        let failure = RecallFailure::timed_out(Duration::from_millis(5000));
        assert_eq!(failure.code, RecallFailureCode::Timeout);
        assert_eq!(failure.code.to_string(), "recall_timeout");
        assert!(failure.to_string().contains("recall_timeout"));
        assert!(failure.to_string().contains("5000"));
    }
}
