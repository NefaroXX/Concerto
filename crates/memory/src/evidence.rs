//! ADR-82 slice 7 — `MemoryEvidence`: a read-time derived composite.
//! H11 wires it into the live recall path with deterministic status.
//!
//! The epistemic axes — truth status, relevance, evidence/corroboration
//! score, confidence, and model freshness — stay separate and
//! non-interchangeable; they are never collapsed into one number. The
//! composite is the ONLY place the relevance and evidence scores appear
//! together, side by side, labeled.
//!
//! Storage rule: [`MemoryEvidence`] is assembled at retrieval (the
//! [`crate::rag`] evidence-bearing enrichment path) and NEVER persisted —
//! no table, no column, no writer, no migration. The
//! `snapshot_generation` / `observed_at` drift stamps let a consumer detect
//! that the composite has drifted from its sources; detection, not
//! prevention (ADR-82 limitation 4).
//!
//! H11 source/status rule: every recall answers *what this is* (`source`),
//! *how it stands* (`truth`), and *why* (`derivation`, bounded prose
//! citing the substrate that spoke — or stating that none did). Truth
//! comes from [`crate::recall_status::derive_recall_verdict`] over link,
//! TTL, decision, and observation substrate; rank and score never enter it
//! (ranking-independence is structural: the standing input has no score
//! field). An item whose status cannot be established reads
//! [`FactStatusView::Unknown`] — never a default that reads as fine.
//!
//! Axis rules (shared with the core vocabulary in
//! `concerto_core::memory`):
//!
//! - Truth is never computed from any score. The only truth inputs are an
//!   explicit [`FactStatusView`] (G-NOUPGRADE: grounding never upgrades a
//!   status — only an executed observation verifies) or the orchestrator's
//!   `FactStatus::to_view` conversion. There is no constructor, helper, or
//!   deserialization path from an `f64` to a [`FactStatusView`].
//! - Relevance ([`Relevance`], `0.0..=1.0`), corroboration
//!   ([`EvidenceScore`], `0.0..=[`EVIDENCE_SCORE_MAX`]`), calibration
//!   ([`Confidence`]), and model freshness ([`Freshness`], `0.0..=1.0`)
//!   are separate fields with distinct types and ranges. No
//!   `From`/`Into` converts between them or from a raw `f64`. The field
//!   is named `model_freshness`, never `freshness`, so it cannot be
//!   misread beside `truth`: TTL staleness speaks on `truth`, model
//!   currency on `model_freshness`, and they are allowed to disagree.
//! - Enforcement is constructor-plus-convention-plus-test, not absolute
//!   type safety: a caller holding a raw `f64` can still deliberately build
//!   the wrong newtype. What the types guarantee is that no score can
//!   silently *become* truth or confidence — every crossing is an explicit,
//!   reviewed constructor call.

use std::collections::HashMap;

use concerto_core::memory::{
    Confidence, EvidenceScore, EvidentChunk, FactStatusView, MemoryChunk, RecallSource, Relevance,
};
use serde::{Deserialize, Deserializer, Serialize};
use time::OffsetDateTime;

use crate::rag::FusedResult;
use crate::recall_status::{ChunkStanding, DerivationContext};

/// Current read-time schema version for [`MemoryEvidence`].
///
/// Named-version policy (per the ADR-82 compatibility section and the
/// checkpoint precedent): the current version loads as-is; unknown (future)
/// versions are rejected by [`MemoryEvidence::from_json`], never silently
/// accepted. No version ever persisted anywhere (the storage rule forbids
/// it), so each bump supersedes its predecessors with no migration path: an
/// older payload is rejected like any other non-current version. The bumps
/// matter because a reader would otherwise silently misread what changed —
/// exactly the silent misread versions exist to catch. v3 added the
/// `interrupted` recall-interruption signal. v4 renames `freshness` to
/// `model_freshness`, so the field name carries its model-only meaning
/// beside `truth` (a TTL-expired row is `truth: Stale` with
/// `model_freshness: 1.0` — orthogonal axes, not a contradiction). The
/// rename is deliberately a hard load failure in both directions — an old
/// reader must never read the new field under the old, ambiguous name, and
/// a new reader must never invent a default where the field is missing.
pub const EVIDENCE_SCHEMA_VERSION: u16 = 4;

/// Retrieval-time freshness projection, `0.0..=1.0` decay multiplier.
///
/// Coarse by construction: the fused retrieval path carries only the
/// vector store's model-version staleness flag, not content age, so
/// freshness here means *model freshness* — `1.0` is a current-model row,
/// `0.0` is a superseded-model row (rank-demoted per ADR-12; its content
/// may still be right). Content age is unknown at this path: consumers
/// must consult the [`MemoryEvidence::observed_at`] drift stamp. Use the
/// named constructors ([`Self::fresh`], [`Self::stale_model`]) rather than
/// bare floats so the semantics stay attached; [`Self::new`] rejects and
/// [`Self::clamped`] clamps like the core score newtypes, with NaN flooring
/// to `0.0`. No `From<f64>`, no conversions to the other score newtypes.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(transparent)]
pub struct Freshness(f64);

impl Freshness {
    /// A current-model row: full freshness credit.
    #[must_use]
    pub fn fresh() -> Self {
        Self(1.0)
    }

    /// A superseded-model row: no freshness credit. The content may still
    /// be right — this marks the *model*, not the content.
    #[must_use]
    pub fn stale_model() -> Self {
        Self(0.0)
    }

    /// Rejecting constructor: `None` for NaN, infinities, and values
    /// outside `0.0..=1.0`.
    #[must_use]
    pub fn new(value: f64) -> Option<Self> {
        if value.is_finite() && (0.0..=1.0).contains(&value) {
            Some(Self(value))
        } else {
            None
        }
    }

    /// Clamping constructor: NaN maps to `0.0`, everything else clamps to
    /// `0.0..=1.0` order-preservingly.
    #[must_use]
    pub fn clamped(value: f64) -> Self {
        if value.is_nan() {
            Self(0.0)
        } else {
            Self(value.clamp(0.0, 1.0))
        }
    }

    /// The validated multiplier.
    #[must_use]
    pub fn value(&self) -> f64 {
        self.0
    }
}

impl<'de> Deserialize<'de> for Freshness {
    /// Boundary enforcement: rejects NaN, infinities, and out-of-range
    /// values at deserialization, mirroring [`Self::new`].
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = f64::deserialize(deserializer)?;
        Self::new(value).ok_or_else(|| {
            serde::de::Error::invalid_value(
                serde::de::Unexpected::Float(value),
                &"a finite value within 0.0..=1.0",
            )
        })
    }
}

/// Read-time derived composite of the memory epistemic axes.
///
/// Assembled at retrieval, never persisted. Every axis keeps its own typed
/// field; the two scores appear together here only, side by side, labeled —
/// there is no combined or collapsed score anywhere in this struct.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryEvidence {
    /// Read-time schema version (see [`EVIDENCE_SCHEMA_VERSION`]).
    pub schema_version: u16,
    /// The chunk this evidence describes.
    pub chunk_id: String,
    /// Truth status. Never derived from any score: either supplied
    /// explicitly or derived by [`enrich_with_standing`] from link, TTL,
    /// decision, and observation substrate — or [`FactStatusView::Unknown`]
    /// when no substrate establishes a status (memory holds no executed
    /// observation of its own, so it cannot verify — G-NOUPGRADE).
    pub truth: FactStatusView,
    /// Where the item comes from (H11 source attribution).
    pub source: RecallSource,
    /// Query relevance (`0.0..=1.0`), lifted from the fused rank signal.
    pub relevance: Relevance,
    /// Link corroboration (`0.0..=[`EVIDENCE_SCORE_MAX`]`).
    pub evidence_score: EvidenceScore,
    /// Calibration. Missing on the wire means [`Confidence::Unknown`]
    /// (single-unknown rule — never `Option<Confidence>`). The retrieval
    /// path holds no calibration for a chunk, so this stays `Unknown`
    /// until a calibrated producer exists.
    #[serde(default)]
    pub confidence: Confidence,
    /// Model-version freshness (`0.0..=1.0`), named so it cannot be
    /// misread beside [`Self::truth`]: TTL-driven staleness lives in
    /// `truth` (a TTL-expired row is `truth: Stale` with
    /// `model_freshness: 1.0` when its embedding model is current), and
    /// content age is out of scope here — see [`Self::derivation`] and
    /// [`Self::observed_at`].
    pub model_freshness: Freshness,
    /// Why this status, in words: which substrate spoke, or that none did
    /// (H11 derivation, bounded at assembly).
    pub derivation: String,
    /// `true` when the result came from the vector-disabled FTS-only
    /// fallback: reduced capability, surfaced — never hidden. Missing on
    /// the wire means `false` (additive read-time form).
    #[serde(default)]
    pub degraded: bool,
    /// `true` when the recall was interrupted (cancelled) before standing
    /// was fully established: the verdict may rest on incomplete
    /// substrate. Machine-readable on purpose — a caller distinguishes an
    /// interrupted recall from a completed recall that genuinely found
    /// nothing WITHOUT parsing prose. Missing on the wire means `false`.
    #[serde(default)]
    pub interrupted: bool,
    /// Generation of the workspace snapshot the retrieval ran against.
    /// A consumer holding an older generation has a drifted composite.
    pub snapshot_generation: u64,
    /// When the composite was assembled. A consumer comparing this against
    /// a newer store state has a drifted composite.
    pub observed_at: OffsetDateTime,
}

/// Drift stamps for one enrichment pass: the snapshot generation the
/// retrieval ran against and the assembly instant.
#[derive(Debug, Clone, Copy)]
pub struct EvidenceStamps {
    pub snapshot_generation: u64,
    pub observed_at: OffsetDateTime,
}

impl MemoryEvidence {
    /// Total explicit constructor: every axis is a separate typed argument,
    /// so a caller cannot pass a score where truth belongs. There is no
    /// deriving constructor — scores never become truth here.
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn assemble(
        chunk_id: String,
        truth: FactStatusView,
        source: RecallSource,
        relevance: Relevance,
        evidence_score: EvidenceScore,
        confidence: Confidence,
        model_freshness: Freshness,
        derivation: String,
        degraded: bool,
        interrupted: bool,
        snapshot_generation: u64,
        observed_at: OffsetDateTime,
    ) -> Self {
        Self {
            schema_version: EVIDENCE_SCHEMA_VERSION,
            chunk_id,
            truth,
            source,
            relevance,
            evidence_score,
            confidence,
            model_freshness,
            derivation,
            degraded,
            interrupted,
            snapshot_generation,
            observed_at,
        }
    }

    /// Project this composite into the trait-safe [`EvidentChunk`] an agent
    /// receives: the chunk plus source attribution, status, derivation,
    /// model freshness, and the degraded/interrupted flags.
    #[must_use]
    pub fn to_evident_chunk(&self, chunk: MemoryChunk) -> EvidentChunk {
        // `model_freshness` left this struct through the validated
        // `Freshness` newtype, so it is finite within `0.0..=1.0` by
        // construction.
        let model_freshness = self.model_freshness.value();
        debug_assert!(model_freshness.is_finite() && (0.0..=1.0).contains(&model_freshness));
        EvidentChunk {
            chunk,
            truth: self.truth,
            source: self.source,
            confidence: self.confidence,
            model_freshness,
            derivation: self.derivation.clone(),
            degraded: self.degraded,
            interrupted: self.interrupted,
        }
    }

    /// Serialize the read-time projection.
    #[must_use]
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{\"schema_version\":0}".to_string())
    }

    /// Deserialize with schema-version enforcement: the current version
    /// loads as-is (a missing `confidence` key reads as
    /// [`Confidence::Unknown`]); unknown (future) versions are rejected,
    /// never silently accepted. Unknown *fields* are ignored (additive
    /// read-time form).
    pub fn from_json(json: &str) -> Result<Self, String> {
        let evidence: Self = serde_json::from_str(json)
            .map_err(|error| format!("failed to deserialize memory evidence: {error}"))?;
        if evidence.schema_version != EVIDENCE_SCHEMA_VERSION {
            return Err(format!(
                "unsupported memory-evidence schema version {}: this runtime supports v{}",
                evidence.schema_version, EVIDENCE_SCHEMA_VERSION
            ));
        }
        Ok(evidence)
    }
}

/// Assemble one [`MemoryEvidence`] per fused result from the cascade's
/// link-score map, with full per-chunk standing (H11).
///
/// Truth, source, and derivation come from
/// [`crate::recall_status::derive_recall_verdict`] over the caller's
/// standing map — link, TTL, decision, and observation substrate. Chunks
/// with no standing entry (a standing lookup failed) read
/// [`FactStatusView::Unknown`]: a failed lookup establishes nothing. The
/// remaining axes keep the [`enrich_fused`] conventions below.
///
/// The enrichment-path conventions, each documented where it can mislead:
///
/// - `relevance` is [`Relevance::clamped`] over the fused RRF score: an
///   order-preserving lift of a rank signal, not a calibrated probability.
///   RRF scores cluster near zero, so relevance values from different
///   queries are not comparable — only the order within one pass. Rank
///   NEVER enters truth, source, model freshness, or derivation: the
///   standing input has no score field, so leakage is structural, not
///   conventional.
/// - `evidence_score` is [`EvidenceScore::clamped`] over the link-score
///   map; chunks absent from the map score `0.0` (no supporting evidence —
///   the same convention as the cascade reorder key).
/// - `confidence` is [`Confidence::Unknown`]: the retrieval path holds no
///   calibration for a chunk.
/// - `model_freshness` is model freshness from the fused stale flag
///   ([`Freshness::fresh`] / [`Freshness::stale_model`]); content age is
///   out of scope — see the derivation and [`MemoryEvidence::observed_at`].
/// - `degraded` follows the fused result's degraded notice (the
///   vector-disabled FTS-only fallback), so reduced capability surfaces on
///   every result it touched.
#[must_use]
pub fn enrich_with_standing(
    results: &[FusedResult],
    scores: &HashMap<String, f64>,
    stamps: EvidenceStamps,
    standings: &HashMap<String, ChunkStanding>,
    ctx: &DerivationContext,
) -> Vec<MemoryEvidence> {
    results
        .iter()
        .map(|result| {
            let degraded = result.notice.is_some();
            // The interruption signal is per-recall (from the derivation
            // context), not per-chunk: every composite of an interrupted
            // recall carries it, so no caller can mistake the recall for
            // complete. Fail-open philosophy preserved — the recall still
            // serves — but the interruption is explicit, never silent.
            let interrupted = ctx.interrupted;
            let (truth, source, model_freshness, derivation) = match standings.get(&result.chunk_id)
            {
                Some(standing) => {
                    let verdict = crate::recall_status::derive_recall_verdict(standing, ctx);
                    let derivation = if interrupted {
                        // The verdict describes whatever substrate was
                        // present, but assembly stopped early — say so on
                        // the verdict itself, not just beside it.
                        format!(
                            "{}; recall interrupted, further substrate unchecked",
                            verdict.derivation
                        )
                    } else {
                        verdict.derivation
                    };
                    (
                        verdict.truth,
                        verdict.source,
                        if verdict.stale_model {
                            Freshness::stale_model()
                        } else {
                            Freshness::fresh()
                        },
                        derivation,
                    )
                }
                None => (
                    FactStatusView::Unknown,
                    RecallSource::Unknown,
                    if result.stale { Freshness::stale_model() } else { Freshness::fresh() },
                    if interrupted {
                        "recall interrupted before standing was established; \
                         status unestablished"
                            .to_string()
                    } else {
                        "standing unavailable for this chunk; status unestablished".to_string()
                    },
                ),
            };
            let mut derivation = derivation;
            crate::recall::truncate_to_chars(
                &mut derivation,
                crate::recall_status::MAX_DERIVATION_CHARS,
            );
            MemoryEvidence::assemble(
                result.chunk_id.clone(),
                truth,
                source,
                Relevance::clamped(result.score),
                EvidenceScore::clamped(scores.get(&result.chunk_id).copied().unwrap_or(0.0)),
                Confidence::Unknown,
                model_freshness,
                derivation,
                degraded,
                interrupted,
                stamps.snapshot_generation,
                stamps.observed_at,
            )
        })
        .collect()
}

/// Assemble one [`MemoryEvidence`] per fused result from the cascade's
/// link-score map, without chunk standing.
///
/// This is [`enrich_with_standing`] with an empty standing map: the
/// retriever sees scores, staleness flags, and degraded notices, but no
/// chunk kinds, links, or observations — so truth and source read
/// [`FactStatusView::Unknown`] / [`RecallSource::Unknown`], never a
/// default that reads as fine. The memory system refines these composites
/// with standing before they reach an agent.
#[must_use]
pub fn enrich_fused(
    results: &[FusedResult],
    scores: &HashMap<String, f64>,
    stamps: EvidenceStamps,
) -> Vec<MemoryEvidence> {
    let ctx = DerivationContext {
        decay_days: Some(crate::scoring::DECAY_FLOOR_DAYS as u16),
        now_unix: stamps.observed_at.unix_timestamp(),
        current_generation: None,
        // A standing-free enrichment is complete by construction (there is
        // no standing to interrupt) — interruption is set by callers that
        // assemble standing and get cancelled mid-way.
        interrupted: false,
    };
    enrich_with_standing(results, scores, stamps, &HashMap::new(), &ctx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use concerto_core::memory::EVIDENCE_SCORE_MAX;

    // Non-interchangeability is constructor-plus-convention-plus-test
    // enforcement, not absolute type safety. The absences below are
    // additionally grep-verified; these patterns must return nothing in
    // this file and in `concerto-core/src/memory.rs`:
    //   `impl From<.*> for (Confidence|Relevance|EvidenceScore|Freshness|MemoryEvidence)`
    //   `Option<Confidence>`

    fn stamps() -> EvidenceStamps {
        EvidenceStamps {
            snapshot_generation: 7,
            observed_at: OffsetDateTime::from_unix_timestamp(1_784_000_000).expect("fixed time"),
        }
    }

    fn fused(chunk_id: &str, score: f64, stale: bool) -> FusedResult {
        FusedResult {
            chunk_id: chunk_id.into(),
            score,
            vector_score: score,
            fts_score: 0.0,
            content: "content".into(),
            notice: None,
            stale,
        }
    }

    #[test]
    fn enrich_defaults_keep_axes_separate() {
        let results = vec![fused("a", 0.03, false), fused("b", 0.01, true)];
        let mut scores = HashMap::new();
        scores.insert("a".to_string(), 8.0);
        let evidence = enrich_fused(&results, &scores, stamps());
        assert_eq!(evidence.len(), 2);

        let first = &evidence[0];
        assert_eq!(first.chunk_id, "a");
        // Truth is unknown without standing — never derived from the 8.0
        // evidence score, and never a default that reads as fine.
        assert_eq!(first.truth, FactStatusView::Unknown);
        assert_eq!(first.source, RecallSource::Unknown);
        assert!(first.derivation.contains("unestablished"), "says why: {}", first.derivation);
        assert!(!first.degraded);
        assert_eq!(first.confidence, Confidence::Unknown);
        assert_eq!(first.evidence_score.value(), 8.0);
        assert_eq!(first.model_freshness, Freshness::fresh());
        assert_eq!(first.snapshot_generation, 7);

        let second = &evidence[1];
        // Absent from the score map: no supporting evidence, not an error.
        assert_eq!(second.evidence_score.value(), 0.0);
        assert_eq!(second.model_freshness, Freshness::stale_model());
        // Relevance order follows the fused order.
        assert!(first.relevance.value() > second.relevance.value());
    }

    /// H11 ranking-independence: the query held constant, only rank varied.
    /// Truth, source, model freshness, and derivation must not move when
    /// scores do — relevance (the labeled rank signal) is the only axis
    /// allowed to follow rank.
    #[test]
    fn recall_status_ignores_rank_scores() {
        use crate::recall_status::{ChunkStanding, DerivationContext};
        use concerto_core::memory::ChunkType;

        let mut standings = HashMap::new();
        standings.insert(
            "a".to_string(),
            ChunkStanding {
                chunk_type: ChunkType::Function,
                file_path: Some("src/lib.rs".to_string()),
                stale_model: false,
                created_at: Some(
                    OffsetDateTime::from_unix_timestamp(1_784_000_000 - 86_400)
                        .expect("fixed time"),
                ),
                metadata: None,
                incoming: Vec::new(),
                cited_decision: None,
                path_observation: None,
            },
        );
        let ctx = DerivationContext {
            decay_days: Some(90),
            now_unix: 1_784_000_000,
            current_generation: None,
            interrupted: false,
        };
        let scores = HashMap::new();

        // Same chunk, top-ranked vs bottom-ranked: the verdict is identical.
        let top =
            enrich_with_standing(&[fused("a", 0.9999, false)], &scores, stamps(), &standings, &ctx);
        let bottom =
            enrich_with_standing(&[fused("a", 0.0001, false)], &scores, stamps(), &standings, &ctx);
        assert_eq!(top.len(), 1);
        assert_eq!(bottom.len(), 1);
        assert_eq!(top[0].truth, bottom[0].truth);
        assert_eq!(top[0].source, bottom[0].source);
        assert_eq!(top[0].model_freshness, bottom[0].model_freshness);
        assert_eq!(top[0].derivation, bottom[0].derivation);
        assert_eq!(top[0].evidence_score, bottom[0].evidence_score);
        // …while relevance — the labeled rank signal — does follow rank,
        // proving the test varies something real.
        assert!(
            top[0].relevance.value() > bottom[0].relevance.value(),
            "relevance must track rank ({} vs {})",
            top[0].relevance.value(),
            bottom[0].relevance.value()
        );
    }

    /// Blocking review finding: an interrupted recall must NOT produce
    /// output indistinguishable from a completed recall that genuinely
    /// found nothing. The interruption travels machine-readable
    /// (`interrupted`) AND in prose, on every composite.
    #[test]
    fn interrupted_enrichment_is_distinguishable_from_genuine_unknown() {
        use crate::recall_status::DerivationContext;

        let results = vec![fused("a", 0.5, false)];
        let scores = HashMap::new();
        let complete = DerivationContext {
            decay_days: Some(90),
            now_unix: 1_784_000_000,
            current_generation: None,
            interrupted: false,
        };
        let halted = DerivationContext { interrupted: true, ..complete.clone() };

        let done = enrich_with_standing(&results, &scores, stamps(), &HashMap::new(), &complete);
        let cut = enrich_with_standing(&results, &scores, stamps(), &HashMap::new(), &halted);
        assert_eq!(done.len(), 1);
        assert_eq!(cut.len(), 1);
        // Same shape, same Unknown truth — the completed recall genuinely
        // established nothing.
        assert_eq!(done[0].truth, FactStatusView::Unknown);
        assert_eq!(cut[0].truth, FactStatusView::Unknown);
        assert!(!done[0].interrupted);
        // …but the interrupted recall says so, without parsing prose.
        assert!(cut[0].interrupted);
        assert_ne!(done[0].derivation, cut[0].derivation);
        assert!(cut[0].derivation.contains("interrupted"), "visible: {}", cut[0].derivation);
        // …and the flag survives the wire.
        let back = MemoryEvidence::from_json(&cut[0].to_json()).expect("round-trips");
        assert!(back.interrupted);
        assert_eq!(back, cut[0]);
    }

    #[test]
    fn missing_interrupted_key_reads_as_false() {
        let evidence = MemoryEvidence::assemble(
            "c".to_string(),
            FactStatusView::Unknown,
            RecallSource::Unknown,
            Relevance::clamped(0.5),
            EvidenceScore::clamped(5.0),
            Confidence::Unknown,
            Freshness::fresh(),
            "status unestablished".to_string(),
            false,
            false,
            1,
            stamps().observed_at,
        );
        let mut value = serde_json::to_value(&evidence).expect("serializes");
        value.as_object_mut().expect("object").remove("interrupted");
        let back = MemoryEvidence::from_json(&value.to_string()).expect("missing key loads");
        assert!(!back.interrupted);
    }

    #[test]
    fn degraded_notice_surfaces_on_the_composite() {
        let mut result = fused("a", 0.5, false);
        result.notice = Some("embedder degraded".into());
        let evidence = enrich_fused(&[result], &HashMap::new(), stamps());
        assert_eq!(evidence.len(), 1);
        assert!(evidence[0].degraded, "the FTS-only condition must surface, not hide");
        // …and it survives the wire.
        let back = MemoryEvidence::from_json(&evidence[0].to_json()).expect("round-trips");
        assert!(back.degraded);
    }

    #[test]
    fn enrich_clamps_hostile_scorer_outputs() {
        let results = vec![fused("a", f64::NAN, false), fused("b", 99.0, false)];
        let mut scores = HashMap::new();
        scores.insert("a".to_string(), f64::NAN);
        scores.insert("b".to_string(), 1_000.0);
        let evidence = enrich_fused(&results, &scores, stamps());
        assert_eq!(evidence[0].relevance.value(), 0.0);
        assert_eq!(evidence[0].evidence_score.value(), 0.0);
        assert_eq!(evidence[1].evidence_score.value(), EVIDENCE_SCORE_MAX);
    }

    #[test]
    fn high_relevance_with_unknown_confidence_survives_the_composite() {
        // The axis-separation case: maximal match strength with no
        // calibration must be representable, must render "unknown" (never
        // 0.0), and must survive a wire round-trip unchanged.
        let evidence = MemoryEvidence::assemble(
            "chunk-1".to_string(),
            FactStatusView::Assumed,
            RecallSource::ModelAuthored,
            Relevance::new(1.0).expect("valid"),
            EvidenceScore::new(EVIDENCE_SCORE_MAX).expect("valid"),
            Confidence::Unknown,
            Freshness::fresh(),
            "model-authored claim".to_string(),
            false,
            false,
            3,
            stamps().observed_at,
        );
        assert_eq!(evidence.confidence.to_string(), "unknown");
        let json = evidence.to_json();
        assert!(json.contains("\"unknown\""), "Unknown renders as the word: {json}");
        let back = MemoryEvidence::from_json(&json).expect("round-trips");
        assert_eq!(back, evidence);
        assert_eq!(back.confidence, Confidence::Unknown);
        assert_eq!(back.relevance.value(), 1.0);
        assert_eq!(back.evidence_score.value(), EVIDENCE_SCORE_MAX);
    }

    #[test]
    fn known_confidence_and_truth_survive_the_composite() {
        let evidence = MemoryEvidence::assemble(
            "chunk-2".to_string(),
            FactStatusView::Verified,
            RecallSource::RuntimeObserved,
            Relevance::new(0.9).expect("valid"),
            EvidenceScore::new(9.0).expect("valid"),
            Confidence::known(0.75, concerto_core::memory::ConfidenceSource::ModelSelfReport)
                .expect("valid"),
            Freshness::fresh(),
            "clean resource-fact observation ev-1".to_string(),
            false,
            false,
            3,
            stamps().observed_at,
        );
        let back = MemoryEvidence::from_json(&evidence.to_json()).expect("round-trips");
        assert_eq!(back, evidence);
    }

    #[test]
    fn missing_confidence_key_reads_as_unknown() {
        let evidence = MemoryEvidence::assemble(
            "c".to_string(),
            FactStatusView::Assumed,
            RecallSource::ModelAuthored,
            Relevance::clamped(0.5),
            EvidenceScore::clamped(5.0),
            Confidence::Unknown,
            Freshness::fresh(),
            "model-authored claim".to_string(),
            false,
            false,
            1,
            stamps().observed_at,
        );
        let mut value = serde_json::to_value(&evidence).expect("serializes");
        value.as_object_mut().expect("object").remove("confidence");
        let back = MemoryEvidence::from_json(&value.to_string()).expect("missing key loads");
        assert_eq!(back.confidence, Confidence::Unknown);
    }

    #[test]
    fn invalid_confidence_rejected_at_the_evidence_boundary() {
        let template = MemoryEvidence::assemble(
            "c".to_string(),
            FactStatusView::Assumed,
            RecallSource::ModelAuthored,
            Relevance::clamped(0.5),
            EvidenceScore::clamped(5.0),
            Confidence::Unknown,
            Freshness::fresh(),
            "model-authored claim".to_string(),
            false,
            false,
            1,
            stamps().observed_at,
        );
        let base = serde_json::to_value(&template).expect("serializes");
        for confidence in [
            serde_json::json!(0.8),
            serde_json::json!(null),
            serde_json::json!({"value": 1.5, "source": "model-self-report"}),
        ] {
            let mut value = base.clone();
            value["confidence"] = confidence.clone();
            assert!(
                MemoryEvidence::from_json(&value.to_string()).is_err(),
                "must refuse confidence {confidence}"
            );
        }
    }

    #[test]
    fn unknown_future_schema_version_rejected() {
        let evidence = MemoryEvidence::assemble(
            "c".to_string(),
            FactStatusView::Assumed,
            RecallSource::ModelAuthored,
            Relevance::clamped(0.1),
            EvidenceScore::clamped(1.0),
            Confidence::Unknown,
            Freshness::fresh(),
            "model-authored claim".to_string(),
            false,
            false,
            1,
            stamps().observed_at,
        );
        let mut value = serde_json::to_value(&evidence).expect("serializes");
        value["schema_version"] = serde_json::json!(EVIDENCE_SCHEMA_VERSION + 1);
        assert!(MemoryEvidence::from_json(&value.to_string()).is_err());
    }

    #[test]
    fn missing_degraded_key_reads_as_false() {
        let evidence = MemoryEvidence::assemble(
            "c".to_string(),
            FactStatusView::Unknown,
            RecallSource::Unknown,
            Relevance::clamped(0.5),
            EvidenceScore::clamped(5.0),
            Confidence::Unknown,
            Freshness::fresh(),
            "status unestablished".to_string(),
            false,
            false,
            1,
            stamps().observed_at,
        );
        let mut value = serde_json::to_value(&evidence).expect("serializes");
        value.as_object_mut().expect("object").remove("degraded");
        let back = MemoryEvidence::from_json(&value.to_string()).expect("missing key loads");
        assert!(!back.degraded);
    }

    #[test]
    fn evident_projection_carries_source_status_and_why() {
        use concerto_core::memory::{ChunkType, MemoryChunk, MemoryNamespace, ProjectId};

        let project_id = ProjectId("p".into());
        let chunk = MemoryChunk {
            id: "c".into(),
            project_id: project_id.clone(),
            namespace: MemoryNamespace::Project(project_id),
            content: "content".into(),
            file_path: Some("src/lib.rs".into()),
            start_line: None,
            end_line: None,
            chunk_type: ChunkType::Function,
            score: 0.02,
            model_id: String::new(),
            model_version: String::new(),
            stale: false,
        };
        let evidence = MemoryEvidence::assemble(
            "c".to_string(),
            FactStatusView::Contradicted,
            RecallSource::IndexedFile,
            Relevance::clamped(0.02),
            EvidenceScore::clamped(3.0),
            Confidence::Unknown,
            Freshness::fresh(),
            "contradicted by critic".to_string(),
            false,
            false,
            1,
            stamps().observed_at,
        );
        let evident = evidence.to_evident_chunk(chunk);
        assert_eq!(evident.truth, FactStatusView::Contradicted);
        assert_eq!(evident.source, RecallSource::IndexedFile);
        assert_eq!(evident.derivation, "contradicted by critic");
        assert_eq!(evident.model_freshness, 1.0);
        assert!(!evident.degraded);
        // The rank signal rides along labeled as the chunk score — never
        // as model freshness.
        assert_eq!(evident.chunk.score, 0.02);
    }

    /// H16 honesty seam: recall TTL staleness (`truth`) and model-version
    /// currency (`model_freshness`) are orthogonal axes, and the field
    /// name must not blur them. A chunk past its retention window on a
    /// CURRENT embedding model reads `truth: Stale` with
    /// `model_freshness: Freshness::fresh()` — a legitimate combination,
    /// not a contradiction. If either field read as a generic
    /// "freshness", an agent would either distrust a current-model row or
    /// trust an expired one.
    #[test]
    fn ttl_expired_stale_truth_keeps_current_model_freshness() {
        use crate::recall_status::{ChunkStanding, DerivationContext};
        use concerto_core::memory::{ChunkType, MemoryChunk, MemoryNamespace, ProjectId};

        let now_unix = 1_784_000_000;
        let mut standings = HashMap::new();
        standings.insert(
            "c".to_string(),
            ChunkStanding {
                chunk_type: ChunkType::Function,
                file_path: Some("src/lib.rs".to_string()),
                stale_model: false,
                created_at: Some(
                    OffsetDateTime::from_unix_timestamp(now_unix - 200 * 86_400)
                        .expect("fixed time"),
                ),
                metadata: None,
                incoming: Vec::new(),
                cited_decision: None,
                path_observation: None,
            },
        );
        let ctx = DerivationContext {
            decay_days: Some(90),
            now_unix,
            current_generation: None,
            interrupted: false,
        };
        let evidence = enrich_with_standing(
            &[fused("c", 0.5, false)],
            &HashMap::new(),
            stamps(),
            &standings,
            &ctx,
        );
        assert_eq!(evidence.len(), 1);
        // TTL expiry speaks on truth — 200d against a 90-day window.
        assert_eq!(evidence[0].truth, FactStatusView::Stale);
        assert!(
            evidence[0].derivation.contains("retention window"),
            "derivation names the TTL: {}",
            evidence[0].derivation
        );
        // …while model currency stays at full credit: the embedding model
        // is current, and only the model axis may say otherwise.
        assert_eq!(evidence[0].model_freshness, Freshness::fresh());

        // The split survives the agent-facing projection unchanged.
        let project_id = ProjectId("p".into());
        let chunk = MemoryChunk {
            id: "c".into(),
            project_id: project_id.clone(),
            namespace: MemoryNamespace::Project(project_id),
            content: "content".into(),
            file_path: Some("src/lib.rs".into()),
            start_line: None,
            end_line: None,
            chunk_type: ChunkType::Function,
            score: 0.5,
            model_id: String::new(),
            model_version: String::new(),
            stale: false,
        };
        let evident = evidence[0].to_evident_chunk(chunk);
        assert_eq!(evident.truth, FactStatusView::Stale);
        assert_eq!(evident.model_freshness, 1.0);
    }

    #[test]
    fn freshness_constructors_and_wire_validation() {
        assert_eq!(Freshness::fresh().value(), 1.0);
        assert_eq!(Freshness::stale_model().value(), 0.0);
        assert!(Freshness::new(0.5).is_some());
        assert!(Freshness::new(f64::NAN).is_none());
        assert!(Freshness::new(1.1).is_none());
        assert_eq!(Freshness::clamped(f64::NAN).value(), 0.0);
        let fresh: Freshness = serde_json::from_str("1.0").expect("valid");
        assert_eq!(fresh, Freshness::fresh());
        assert!(serde_json::from_str::<Freshness>("1.5").is_err());
        assert!(serde_json::from_str::<Freshness>("null").is_err());
    }
}
