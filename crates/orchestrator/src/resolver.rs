//! Pre-dispatch resolver for zero-waste orchestration (ADR-64 Phase 4).
//!
//! A **pure, deterministic** function that decides, before any model call,
//! whether a work item can be reused from the timeline, refined, reopened,
//! or must be freshly dispatched. This implements ADR-64 §3's `should_dispatch`.
//!
//! # Purity boundary
//!
//! The four pure verdicts — [`DispatchDecision::Reuse`],
//! [`DispatchDecision::Refine`], [`DispatchDecision::Reopen`],
//! [`DispatchDecision::Dispatch`] — are **deterministic and side-effect
//! free**. They must not consume an LLM call.
//!
//! The stateful verdicts — [`LadderOutcome::Reassign`],
//! [`LadderOutcome::CoordinatorTakeover`] — are resolved through the
//! [`AgentLadder`] trait, which the coordinator populates from the live
//! agent registry and provider health. The ladder is **not** wired into the
//! coordinator yet (Phase 6).
//!
//! # Decision table
//!
//! | Timeline record | Inputs | Deliverable | Verdict |
//! |----------------|--------|-------------|---------|
//! | None for this semantic key | — | — | `Dispatch` |
//! | Exists | Changed | — | `Reopen` |
//! | Exists | Unchanged | Incomplete | `Refine` |
//! | Exists | Unchanged | Complete | `Reuse` |

use crate::fingerprint::ArtifactFingerprint;
use crate::timeline::TimelineEvent;
use crate::timeline::TimelineProjection;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// DispatchDecision (pure)
// ---------------------------------------------------------------------------

/// Pure verdict from the pre-dispatch resolver (ADR-64 §3).
///
/// The first four variants are **deterministic** and must not consume an LLM
/// call. `Dispatch` falls through to the stateful ladder (Phase 6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DispatchDecision {
    /// Identical semantic key + inputs unchanged + evidence valid →
    /// inject cached result; **zero model dispatch**.
    Reuse,
    /// Valid partial result exists, explicit remaining gaps →
    /// small targeted dispatch framed as refinement.
    Refine,
    /// A dependency or depended-on file changed → dispatch with the
    /// change as the explicit reason.
    Reopen,
    /// Required work has no prior valid result.
    Dispatch,
}

// ---------------------------------------------------------------------------
// WorkCandidate
// ---------------------------------------------------------------------------

/// Input to the resolver — carries the candidate's current truth so the
/// resolver can compare against the timeline's recorded state.
pub struct WorkCandidate {
    /// Stable, role-agnostic identity (ADR-64 §2).
    pub semantic_key: crate::fingerprint::SemanticKey,
    /// blake3 hex of the objective text.
    pub objective_hash: String,
    /// Content hash of the plan artifact governing this work.
    pub plan_version: String,
    /// Normalised description of the specific work intent.
    pub work_intent: String,
    /// Current content fingerprints of this item's direct inputs /
    /// dependencies.  This is what makes freshness checking possible —
    /// if any of these differ from the recorded inputs, the cached result
    /// is stale.
    pub current_inputs: Vec<ArtifactFingerprint>,
    /// Expected output contract (for completeness judgement).
    pub output_contract: String,
}

// ---------------------------------------------------------------------------
// LadderOutcome + AgentLadder (stateful surface)
// ---------------------------------------------------------------------------

/// Stateful outcome from the agent-availability ladder (ADR-42/45).
///
/// These variants are **not** pure — they depend on the live agent registry
/// and provider health, which change during a run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LadderOutcome {
    /// The configured agent/model is unavailable or unsuitable →
    /// ADR-42/45 fallback ladder.
    Reassign,
    /// No configured agent can perform the required work →
    /// coordinator self-execution (ADR-45 Tier 2).
    CoordinatorTakeover,
    /// No action possible (all fallbacks exhausted or work is impossible).
    Impossible,
}

/// RFC-like interface the coordinator (Phase 6) will populate from the live
/// agent registry and fallback ladder (ADR-42/45).  Keep it as an interface
/// now; a deterministic test fake is sufficient.
pub trait AgentLadder {
    /// Decide whether a work candidate can be assigned, reassigned, or
    /// must be taken over by the coordinator.
    fn decide(&self, work: &WorkCandidate) -> LadderOutcome;
}

// ---------------------------------------------------------------------------
// Pure resolver
// ---------------------------------------------------------------------------

/// The pure resolver.  Deterministic and side-effect free.
///
/// `projection` is the timeline — what already exists plus current content
/// hashes.  `candidate` is the work item being considered.
///
/// # Decision logic (ADR-64 §3)
///
/// 1. No prior record for this semantic key → `Dispatch`.
/// 2. Prior record exists, any input changed → `Reopen`.
/// 3. Prior record exists, inputs unchanged, deliverable incomplete → `Refine`.
/// 4. Prior record exists, inputs unchanged, deliverable complete → `Reuse`.
pub fn should_dispatch(
    projection: &TimelineProjection,
    candidate: &WorkCandidate,
) -> DispatchDecision {
    let Some(record) = find_matching_record(projection, &candidate.semantic_key) else {
        return DispatchDecision::Dispatch;
    };

    // The correctness of `find_matching_record` (most recent match via
    // `next_back()`) depends on the projection's events being ordered by
    // gate_seq. `build_timeline` guarantees this; defend against any caller
    // that constructs a projection by hand without sorting.
    debug_assert!(
        projection.events.windows(2).all(|w| w[0].gate_seq() <= w[1].gate_seq()),
        "TimelineProjection events must be ordered by gate_seq"
    );

    // Step 2: Input freshness — any recorded input that differs from the
    // current input means a dependency changed.  A change is the explicit
    // reason to reopen.
    if !inputs_fresh(&candidate.current_inputs, record.recorded_inputs) {
        return DispatchDecision::Reopen;
    }

    // Step 3: Deliverable completeness — the summary is non-empty when a
    // valid partial result exists.  If the output contract is specified but
    // not satisfied by the summary, the work is incomplete and must be
    // refined, not redone from scratch.
    // TODO(Phase6): harden completeness from summary-emptiness to actual
    // evidence (expected_artifacts existence / WroteFile events), so a
    // non-empty summary that does NOT satisfy the contract can't be Reused.
    if !candidate.output_contract.is_empty() && record.summary.is_empty() {
        return DispatchDecision::Refine;
    }

    // Step 4: Inputs unchanged + deliverable satisfies contract → Reuse.
    DispatchDecision::Reuse
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Find the most recent `SubtaskCompleted` record whose semantic key hex
/// matches the candidate.  Returns `None` when no prior work exists.
fn find_matching_record<'a>(
    projection: &'a TimelineProjection,
    key: &crate::fingerprint::SemanticKey,
) -> Option<CompletedRecord<'a>> {
    let target_hex = key.as_ref();
    projection
        .events
        .iter()
        .filter_map(|e| match e {
            TimelineEvent::SubtaskCompleted {
                semantic_key_hex,
                summary,
                files_modified,
                content_hash,
                recorded_inputs,
                ..
            } if !semantic_key_hex.is_empty() && semantic_key_hex == target_hex => {
                Some(CompletedRecord {
                    semantic_key_hex,
                    summary,
                    files_modified,
                    content_hash,
                    recorded_inputs,
                })
            }
            _ => None,
        })
        .next_back() // most recent by timeline order (events are sorted by gate_seq)
}

/// Borrowed view of a matched completion record.  Extracted from the
/// timeline event so the comparison logic does not repeat the match.
struct CompletedRecord<'a> {
    /// Retained for Phase 5 capsules (context enrichment).
    #[allow(dead_code)]
    semantic_key_hex: &'a str,
    summary: &'a str,
    /// Retained for Phase 5 capsules (file-context semantics).
    #[allow(dead_code)]
    files_modified: &'a [camino::Utf8PathBuf],
    /// Retained for Phase 5 capsules (deliverable hashing).
    #[allow(dead_code)]
    content_hash: &'a str,
    recorded_inputs: &'a [ArtifactFingerprint],
}

/// Compare two fingerprints for identity: same artifact kind and same path.
///
/// The content hash may differ (that is the staleness signal); what matters
/// here is that the two fingerprints refer to the same artifact.
fn same_fingerprint_identity(a: &ArtifactFingerprint, b: &ArtifactFingerprint) -> bool {
    match (a, b) {
        (
            ArtifactFingerprint::Observation { path: pa, .. },
            ArtifactFingerprint::Observation { path: pb, .. },
        )
        | (
            ArtifactFingerprint::Input { path: pa, .. },
            ArtifactFingerprint::Input { path: pb, .. },
        )
        | (
            ArtifactFingerprint::Output { path: pa, .. },
            ArtifactFingerprint::Output { path: pb, .. },
        ) => pa == pb,
        (
            ArtifactFingerprint::Plan { plan_id: pa, .. },
            ArtifactFingerprint::Plan { plan_id: pb, .. },
        ) => pa == pb,
        (
            ArtifactFingerprint::Research { topic_fingerprint: pa, .. },
            ArtifactFingerprint::Research { topic_fingerprint: pb, .. },
        ) => pa == pb,
        (
            ArtifactFingerprint::Verification { kind: pa, .. },
            ArtifactFingerprint::Verification { kind: pb, .. },
        ) => pa == pb,
        (
            ArtifactFingerprint::Dependency { key: ka },
            ArtifactFingerprint::Dependency { key: kb },
        ) => ka == kb,
        _ => false, // different kinds → not the same artifact
    }
}

/// Check that all recorded inputs are still fresh.
///
/// "Fresh" means every recorded input has a corresponding current input with
/// the same identity (path) and the same content hash.  Any mismatch — a
/// missing input, a new input, or a changed hash — is a staleness signal.
///
/// **Empty `recorded_inputs` is NOT evidence of freshness.** When the timeline
/// holds no recorded inputs for the work (e.g. a legacy pre-Phase-4 event, or
/// an event whose inputs were never fingerprinted), we cannot prove the prior
/// result is still valid against the current world. Returning `false` here
/// forces the resolver to `Reopen`/`Dispatch` rather than falsely `Reuse` —
/// this is the ADR-64 "evidence valid" guard against silent false reuse.
fn inputs_fresh(
    current_inputs: &[ArtifactFingerprint],
    recorded_inputs: &[ArtifactFingerprint],
) -> bool {
    // Cannot prove freshness without evidence: empty recorded inputs always
    // fail the freshness check, regardless of how empty the current inputs are.
    if recorded_inputs.is_empty() {
        return false;
    }

    // Quick length check: different count means something changed.
    if current_inputs.len() != recorded_inputs.len() {
        return false;
    }

    // Every recorded input must have a matching current input.
    recorded_inputs.iter().all(|recorded| {
        current_inputs.iter().any(|current| {
            same_fingerprint_identity(recorded, current)
                && recorded_hash(current) == recorded_hash(recorded)
        })
    })
}

/// Extract the blake3 content hash from a fingerprint.
fn recorded_hash(fp: &ArtifactFingerprint) -> &str {
    match fp {
        ArtifactFingerprint::Plan { content_hash, .. }
        | ArtifactFingerprint::Research { content_hash, .. }
        | ArtifactFingerprint::Observation { content_hash, .. }
        | ArtifactFingerprint::Verification { content_hash, .. }
        | ArtifactFingerprint::Input { content_hash, .. }
        | ArtifactFingerprint::Output { content_hash, .. } => content_hash,
        ArtifactFingerprint::Dependency { key } => key.as_ref(),
    }
}

// ---------------------------------------------------------------------------
// Audit (ADR-64 §8)
// ---------------------------------------------------------------------------

/// Audit record for a resolver verdict.  Phase 6 logs this to the durable
/// audit so the cost of each dispatch is attributable and reducible.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolverAudit {
    /// The pure verdict (or ladder fallback when dispatched).
    pub decision: DispatchDecision,
    /// The semantic key hex of the candidate (empty when the key could not
    /// be computed).
    pub semantic_key_hex: String,
    /// Human-readable reason for the decision.
    pub reason: String,
    /// Unix epoch milliseconds when the decision was made.
    pub at: i64,
}

impl ResolverAudit {
    /// Build an audit record from a decision and reason.
    pub fn new(decision: DispatchDecision, semantic_key_hex: &str, reason: &str) -> Self {
        Self {
            decision,
            semantic_key_hex: semantic_key_hex.to_owned(),
            reason: reason.to_owned(),
            at: now_ms(),
        }
    }
}

/// Current time in Unix epoch milliseconds.  Extracted for testability.
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fingerprint::SemanticKey;
    use crate::timeline::{TimelineEvent, TimelineProjection};
    use std::collections::HashMap;

    // ------------------------------------------------------------------
    // Helpers
    // ------------------------------------------------------------------

    /// Build a canonical SemanticKey for tests.
    fn make_key(work_intent: &str) -> SemanticKey {
        SemanticKey::compute("obj-test", "plan-v1", work_intent, "src/out.rs exists", &[])
    }

    /// Build a WorkCandidate with the given key and inputs.
    fn make_candidate(key: SemanticKey, inputs: Vec<ArtifactFingerprint>) -> WorkCandidate {
        WorkCandidate {
            semantic_key: key.clone(),
            objective_hash: "obj-test".to_owned(),
            plan_version: "plan-v1".to_owned(),
            work_intent: key.components().work_intent.clone(),
            current_inputs: inputs,
            output_contract: "src/out.rs exists".to_owned(),
        }
    }

    /// Build a WorkCandidate with a custom output contract.
    fn make_candidate_with_contract(
        key: SemanticKey,
        inputs: Vec<ArtifactFingerprint>,
        contract: &str,
    ) -> WorkCandidate {
        WorkCandidate {
            semantic_key: key.clone(),
            objective_hash: "obj-test".to_owned(),
            plan_version: "plan-v1".to_owned(),
            work_intent: key.components().work_intent.clone(),
            current_inputs: inputs,
            output_contract: contract.to_owned(),
        }
    }

    /// Build a SubtaskCompleted timeline event with the given semantic key.
    fn completed_event(
        key_hex: &str,
        summary: &str,
        inputs: Vec<ArtifactFingerprint>,
        gate_seq: u64,
    ) -> TimelineEvent {
        TimelineEvent::SubtaskCompleted {
            event_id: format!("evt-{gate_seq}"),
            gate_seq,
            task_id: Default::default(),
            summary: summary.to_owned(),
            files_modified: vec![],
            content_hash: blake3::hash(summary.as_bytes()).to_hex().to_string(),
            created_at: 1_700_000_000_000 + gate_seq as i64,
            semantic_key_hex: key_hex.to_owned(),
            recorded_inputs: inputs,
        }
    }

    /// Empty projection helper.
    fn empty_projection() -> TimelineProjection {
        TimelineProjection {
            events: Vec::new(),
            checkpoints: Vec::new(),
            plan_artifacts: Vec::new(),
            completed_results: HashMap::new(),
        }
    }

    // ------------------------------------------------------------------
    // Test 1: No prior record → Dispatch
    // ------------------------------------------------------------------

    #[test]
    fn no_prior_record_dispatches() {
        let key = make_key("implement feature X");
        let candidate = make_candidate(key, vec![]);
        let projection = empty_projection();
        assert_eq!(should_dispatch(&projection, &candidate), DispatchDecision::Dispatch);
    }

    // ------------------------------------------------------------------
    // Test 2: Prior record, inputs unchanged, complete → Reuse
    // ------------------------------------------------------------------

    #[test]
    fn prior_record_unchanged_inputs_complete_reuses() {
        let key = make_key("implement feature X");
        let inputs = vec![ArtifactFingerprint::Observation {
            path: "src/a.rs".to_owned(),
            content_hash: "hash-a".to_owned(),
        }];
        let candidate = make_candidate(key.clone(), inputs.clone());

        let mut projection = empty_projection();
        projection.events.push(completed_event(key.hex(), "implemented feature X", inputs, 1));
        projection.events.sort_by_key(|e| e.gate_seq());

        assert_eq!(should_dispatch(&projection, &candidate), DispatchDecision::Reuse);
    }

    /// BLOCKING regression guard (oracle review): a completed event with EMPTY
    /// recorded input evidence must NEVER yield Reuse, even when current inputs
    /// are also empty. Empty recorded inputs = cannot prove freshness = stale.
    #[test]
    fn empty_recorded_inputs_must_not_return_reuse() {
        let key = make_key("zero-input work");
        // Candidate also has no current inputs (the tempting-but-unsafe case).
        let candidate = make_candidate(key.clone(), vec![]);

        let mut projection = empty_projection();
        // Prior completed event carrying NO recorded input evidence.
        projection.events.push(completed_event(key.hex(), "done", vec![], 1));
        projection.events.sort_by_key(|e| e.gate_seq());

        // Must NOT Reuse: empty recorded inputs prove nothing about freshness.
        let decision = should_dispatch(&projection, &candidate);
        assert_ne!(
            decision,
            DispatchDecision::Reuse,
            "empty recorded input evidence must never yield Reuse"
        );
    }

    // ------------------------------------------------------------------
    // Test 3: Prior record, inputs UNCHANGED, contract incomplete → Refine
    // ------------------------------------------------------------------

    #[test]
    fn prior_record_unchanged_inputs_incomplete_refines() {
        let key = make_key("implement feature X");
        let inputs = vec![ArtifactFingerprint::Observation {
            path: "src/a.rs".to_owned(),
            content_hash: "hash-a".to_owned(),
        }];
        // Output contract specified, but summary is empty → incomplete.
        let candidate =
            make_candidate_with_contract(key.clone(), inputs.clone(), "src/out.rs exists");

        let mut projection = empty_projection();
        projection.events.push(completed_event(
            key.hex(),
            "", // empty summary → deliverable incomplete
            inputs,
            1,
        ));
        projection.events.sort_by_key(|e| e.gate_seq());

        assert_eq!(should_dispatch(&projection, &candidate), DispatchDecision::Refine);
    }

    // ------------------------------------------------------------------
    // Test 4: Prior record, inputs CHANGED → Reopen (never Reuse)
    // ------------------------------------------------------------------

    #[test]
    fn prior_record_changed_inputs_reopens() {
        let key = make_key("implement feature X");
        let recorded_inputs = vec![ArtifactFingerprint::Observation {
            path: "src/a.rs".to_owned(),
            content_hash: "hash-old".to_owned(),
        }];
        let current_inputs = vec![ArtifactFingerprint::Observation {
            path: "src/a.rs".to_owned(),
            content_hash: "hash-new".to_owned(),
        }];
        let candidate = make_candidate(key.clone(), current_inputs);

        let mut projection = empty_projection();
        projection.events.push(completed_event(
            key.hex(),
            "implemented feature X",
            recorded_inputs,
            1,
        ));
        projection.events.sort_by_key(|e| e.gate_seq());

        // Must be Reopen, NOT Reuse — the false-positive landmine.
        assert_eq!(should_dispatch(&projection, &candidate), DispatchDecision::Reopen);
    }

    // ------------------------------------------------------------------
    // Test 5: Prior record with different semantic key → Dispatch
    // ------------------------------------------------------------------

    #[test]
    fn different_semantic_key_dispatches() {
        let key_a = make_key("implement feature X");
        let key_b = make_key("implement feature Y");
        let candidate = make_candidate(key_a, vec![]);

        let mut projection = empty_projection();
        // Record for key_b, not key_a.
        projection.events.push(completed_event(key_b.hex(), "implemented feature Y", vec![], 1));
        projection.events.sort_by_key(|e| e.gate_seq());

        assert_eq!(should_dispatch(&projection, &candidate), DispatchDecision::Dispatch);
    }

    // ------------------------------------------------------------------
    // Test 6: Determinism — same inputs → same decision twice
    // ------------------------------------------------------------------

    #[test]
    fn same_inputs_same_decision_twice() {
        let key = make_key("deterministic work");
        let inputs = vec![ArtifactFingerprint::Input {
            path: "Cargo.toml".to_owned(),
            content_hash: "h1".to_owned(),
        }];
        let candidate = make_candidate(key.clone(), inputs.clone());

        let mut projection = empty_projection();
        projection.events.push(completed_event(key.hex(), "done", inputs, 1));
        projection.events.sort_by_key(|e| e.gate_seq());

        let d1 = should_dispatch(&projection, &candidate);
        let d2 = should_dispatch(&projection, &candidate);
        assert_eq!(d1, d2);
    }

    // ------------------------------------------------------------------
    // Test 7: Role-agnosticism — identical candidate with different
    //         (implicit) agent id → same decision
    // ------------------------------------------------------------------

    #[test]
    fn identical_candidates_same_decision() {
        let key = make_key("role-agnostic work");
        let inputs = vec![ArtifactFingerprint::Output {
            path: "out.rs".to_owned(),
            content_hash: "h2".to_owned(),
        }];

        let mut projection = empty_projection();
        projection.events.push(completed_event(key.hex(), "completed", inputs.clone(), 1));
        projection.events.sort_by_key(|e| e.gate_seq());

        // Two identical candidates — same decision regardless of who calls.
        let c1 = make_candidate(key.clone(), inputs.clone());
        let c2 = make_candidate(key, inputs);

        assert_eq!(should_dispatch(&projection, &c1), should_dispatch(&projection, &c2));
    }

    // ------------------------------------------------------------------
    // Test 8: Freshness enforcement — stale inputs must NOT return Reuse
    //
    // A timeline has a completed record with stale inputs.  The candidate
    // carries fresh (different) inputs.  The resolver must return Reopen,
    // NOT Reuse, proving that `timeline_contains_hash` alone is not
    // sufficient.
    // ------------------------------------------------------------------

    #[test]
    fn stale_inputs_must_not_return_reuse() {
        let key = make_key("staleness test");
        let stale_inputs = vec![ArtifactFingerprint::Observation {
            path: "src/lib.rs".to_owned(),
            content_hash: "stale-hash".to_owned(),
        }];
        let fresh_inputs = vec![ArtifactFingerprint::Observation {
            path: "src/lib.rs".to_owned(),
            content_hash: "fresh-hash".to_owned(),
        }];
        let candidate = make_candidate(key.clone(), fresh_inputs);

        let mut projection = empty_projection();
        projection.events.push(completed_event(key.hex(), "built the thing", stale_inputs, 1));
        projection.events.sort_by_key(|e| e.gate_seq());

        let decision = should_dispatch(&projection, &candidate);
        assert_eq!(
            decision,
            DispatchDecision::Reopen,
            "stale inputs must trigger Reopen, not Reuse — freshness is required"
        );
    }

    // ------------------------------------------------------------------
    // Test 9: LadderOutcome fake returns each variant correctly
    // ------------------------------------------------------------------

    /// Test double for the AgentLadder trait.
    struct FakeLadder;

    impl AgentLadder for FakeLadder {
        fn decide(&self, work: &WorkCandidate) -> LadderOutcome {
            match work.output_contract.as_str() {
                "reassign-me" => LadderOutcome::Reassign,
                "takeover-me" => LadderOutcome::CoordinatorTakeover,
                _ => LadderOutcome::Impossible,
            }
        }
    }

    #[test]
    fn fake_ladder_returns_reassign() {
        let key = make_key("reassign work");
        let candidate = make_candidate_with_contract(key, vec![], "reassign-me");
        let ladder = FakeLadder;
        assert_eq!(ladder.decide(&candidate), LadderOutcome::Reassign);
    }

    #[test]
    fn fake_ladder_returns_coordinator_takeover() {
        let key = make_key("takeover work");
        let candidate = make_candidate_with_contract(key, vec![], "takeover-me");
        let ladder = FakeLadder;
        assert_eq!(ladder.decide(&candidate), LadderOutcome::CoordinatorTakeover);
    }

    #[test]
    fn fake_ladder_returns_impossible() {
        let key = make_key("impossible work");
        let candidate = make_candidate_with_contract(key, vec![], "no-path");
        let ladder = FakeLadder;
        assert_eq!(ladder.decide(&candidate), LadderOutcome::Impossible);
    }

    // ------------------------------------------------------------------
    // Test 10: ResolverAudit round-trips through serde
    // ------------------------------------------------------------------

    #[test]
    fn resolver_audit_serde_roundtrip() {
        let audit = ResolverAudit::new(
            DispatchDecision::Reuse,
            "abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890",
            "inputs unchanged, deliverable complete",
        );
        let json = serde_json::to_string(&audit).expect("serialize");
        let deserialized: ResolverAudit = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(deserialized.decision, DispatchDecision::Reuse);
        assert_eq!(deserialized.semantic_key_hex, audit.semantic_key_hex);
        assert_eq!(deserialized.reason, audit.reason);
        assert_eq!(deserialized.at, audit.at);
    }

    // ------------------------------------------------------------------
    // Additional edge-case tests
    // ------------------------------------------------------------------

    /// Multiple timeline events: only the most recent matching key is used.
    #[test]
    fn uses_most_recent_matching_event() {
        let key = make_key("multi-event work");
        let old_inputs = vec![ArtifactFingerprint::Input {
            path: "src/a.rs".to_owned(),
            content_hash: "old".to_owned(),
        }];
        let new_inputs = vec![ArtifactFingerprint::Input {
            path: "src/a.rs".to_owned(),
            content_hash: "new".to_owned(),
        }];

        let mut projection = empty_projection();
        // Old event with stale inputs.
        projection.events.push(completed_event(key.hex(), "first attempt", old_inputs, 1));
        // New event with current inputs.
        projection.events.push(completed_event(key.hex(), "second attempt", new_inputs.clone(), 2));
        projection.events.sort_by_key(|e| e.gate_seq());

        let candidate = make_candidate(key, new_inputs);
        // The most recent event (seq 2) has matching inputs → Reuse.
        assert_eq!(should_dispatch(&projection, &candidate), DispatchDecision::Reuse);
    }

    /// Empty semantic key on the timeline event is treated as unknown identity.
    #[test]
    fn empty_semantic_key_treated_as_unknown() {
        let key = make_key("some work");
        let candidate = make_candidate(key.clone(), vec![]);

        let mut projection = empty_projection();
        // Event with empty semantic_key_hex — matches nothing.
        projection.events.push(TimelineEvent::SubtaskCompleted {
            event_id: "evt-1".to_owned(),
            gate_seq: 1,
            task_id: Default::default(),
            summary: "did something".to_owned(),
            files_modified: vec![],
            content_hash: "h1".to_owned(),
            created_at: 1_700_000_000_001,
            semantic_key_hex: String::new(), // empty
            recorded_inputs: vec![],
        });
        projection.events.sort_by_key(|e| e.gate_seq());

        assert_eq!(should_dispatch(&projection, &candidate), DispatchDecision::Dispatch);
    }

    /// Different fingerprint kinds for same path → not the same identity.
    #[test]
    fn different_fingerprint_kind_not_same_identity() {
        let key = make_key("cross-kind work");
        let recorded_inputs = vec![ArtifactFingerprint::Observation {
            path: "src/a.rs".to_owned(),
            content_hash: "h1".to_owned(),
        }];
        let current_inputs = vec![ArtifactFingerprint::Input {
            path: "src/a.rs".to_owned(),
            content_hash: "h1".to_owned(),
        }];

        let mut projection = empty_projection();
        projection.events.push(completed_event(key.hex(), "done", recorded_inputs, 1));
        projection.events.sort_by_key(|e| e.gate_seq());

        let candidate = make_candidate(key, current_inputs);
        // Observation != Input kind → inputs not fresh → Reopen.
        assert_eq!(should_dispatch(&projection, &candidate), DispatchDecision::Reopen);
    }

    /// Adding a new input (count mismatch) → inputs not fresh.
    #[test]
    fn added_input_not_fresh() {
        let key = make_key("new-input work");
        let recorded_inputs = vec![ArtifactFingerprint::Input {
            path: "src/a.rs".to_owned(),
            content_hash: "h1".to_owned(),
        }];
        let current_inputs = vec![
            ArtifactFingerprint::Input {
                path: "src/a.rs".to_owned(),
                content_hash: "h1".to_owned(),
            },
            ArtifactFingerprint::Input {
                path: "src/b.rs".to_owned(),
                content_hash: "h2".to_owned(),
            },
        ];

        let mut projection = empty_projection();
        projection.events.push(completed_event(key.hex(), "done", recorded_inputs, 1));
        projection.events.sort_by_key(|e| e.gate_seq());

        let candidate = make_candidate(key, current_inputs);
        assert_eq!(should_dispatch(&projection, &candidate), DispatchDecision::Reopen);
    }

    /// Empty output_contract with non-empty summary → Reuse (contract
    /// vacuously satisfied).
    #[test]
    fn empty_contract_nonempty_summary_reuses() {
        let key = make_key("vacuous-contract work");
        let inputs = vec![ArtifactFingerprint::Input {
            path: "x.rs".to_owned(),
            content_hash: "hx".to_owned(),
        }];
        let candidate = make_candidate_with_contract(key.clone(), inputs.clone(), "");

        let mut projection = empty_projection();
        projection.events.push(completed_event(key.hex(), "done", inputs, 1));
        projection.events.sort_by_key(|e| e.gate_seq());

        // Empty contract + non-empty summary → Reuse (vacuous satisfaction).
        assert_eq!(should_dispatch(&projection, &candidate), DispatchDecision::Reuse);
    }
}
