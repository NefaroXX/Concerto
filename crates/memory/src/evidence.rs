//! ADR-82 slice 7 — `MemoryEvidence`: a read-time derived composite.
//!
//! The epistemic axes — truth status, relevance, evidence/corroboration
//! score, confidence, and freshness — stay separate and non-interchangeable;
//! they are never collapsed into one number. The composite is the ONLY place
//! the relevance and evidence scores appear together, side by side, labeled.
//!
//! Storage rule: [`MemoryEvidence`] is assembled at retrieval (the
//! [`crate::rag`] `retrieve_with_cascade` enrichment path) and NEVER
//! persisted — no table, no column, no writer, no migration. The
//! `snapshot_generation` / `observed_at` drift stamps let a consumer detect
//! that the composite has drifted from its sources; detection, not
//! prevention (ADR-82 limitation 4).
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
//!   ([`Confidence`]), and freshness ([`Freshness`], `0.0..=1.0`) are
//!   separate fields with distinct types and ranges. No `From`/`Into`
//!   converts between them or from a raw `f64`.
//! - Enforcement is constructor-plus-convention-plus-test, not absolute
//!   type safety: a caller holding a raw `f64` can still deliberately build
//!   the wrong newtype. What the types guarantee is that no score can
//!   silently *become* truth or confidence — every crossing is an explicit,
//!   reviewed constructor call.

use std::collections::HashMap;

use concerto_core::memory::{Confidence, EvidenceScore, FactStatusView, Relevance};
use serde::{Deserialize, Deserializer, Serialize};
use time::OffsetDateTime;

use crate::rag::FusedResult;

/// Current read-time schema version for [`MemoryEvidence`].
///
/// Named-version policy (per the ADR-82 compatibility section and the
/// checkpoint precedent): the current version loads as-is; unknown (future)
/// versions are rejected by [`MemoryEvidence::from_json`], never silently
/// accepted. There are no legacy versions: the composite never persisted
/// before this slice.
pub const EVIDENCE_SCHEMA_VERSION: u16 = 1;

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
    /// explicitly or defaulted to [`FactStatusView::Assumed`] by
    /// [`enrich_fused`] (memory holds no executed observation, so it
    /// cannot verify — G-NOUPGRADE).
    pub truth: FactStatusView,
    /// Query relevance (`0.0..=1.0`), lifted from the fused rank signal.
    pub relevance: Relevance,
    /// Link corroboration (`0.0..=[`EVIDENCE_SCORE_MAX`]`).
    pub evidence_score: EvidenceScore,
    /// Calibration. Missing on the wire means [`Confidence::Unknown`]
    /// (single-unknown rule — never `Option<Confidence>`).
    #[serde(default)]
    pub confidence: Confidence,
    /// Model freshness (`0.0..=1.0`); content age is out of scope here.
    pub freshness: Freshness,
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
        relevance: Relevance,
        evidence_score: EvidenceScore,
        confidence: Confidence,
        freshness: Freshness,
        snapshot_generation: u64,
        observed_at: OffsetDateTime,
    ) -> Self {
        Self {
            schema_version: EVIDENCE_SCHEMA_VERSION,
            chunk_id,
            truth,
            relevance,
            evidence_score,
            confidence,
            freshness,
            snapshot_generation,
            observed_at,
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
/// link-score map.
///
/// The enrichment-path defaults, each documented where it can mislead:
///
/// - `truth` is [`FactStatusView::Assumed`]: memory holds no executed
///   observation, so it cannot verify (G-NOUPGRADE). The orchestrator may
///   overwrite truth with its `FactStatus::to_view` conversion; memory never
///   upgrades it from any score here.
/// - `relevance` is [`Relevance::clamped`] over the fused RRF score: an
///   order-preserving lift of a rank signal, not a calibrated probability.
///   RRF scores cluster near zero, so relevance values from different
///   queries are not comparable — only the order within one pass.
/// - `evidence_score` is [`EvidenceScore::clamped`] over the link-score
///   map; chunks absent from the map score `0.0` (no supporting evidence —
///   the same convention as the cascade reorder key).
/// - `confidence` is [`Confidence::Unknown`]: the retrieval path holds no
///   calibration for a chunk.
/// - `freshness` is model freshness from the fused stale flag
///   ([`Freshness::fresh`] / [`Freshness::stale_model`]); content age is
///   unknown — see [`MemoryEvidence::observed_at`].
#[must_use]
pub fn enrich_fused(
    results: &[FusedResult],
    scores: &HashMap<String, f64>,
    stamps: EvidenceStamps,
) -> Vec<MemoryEvidence> {
    results
        .iter()
        .map(|result| {
            MemoryEvidence::assemble(
                result.chunk_id.clone(),
                FactStatusView::Assumed,
                Relevance::clamped(result.score),
                EvidenceScore::clamped(scores.get(&result.chunk_id).copied().unwrap_or(0.0)),
                Confidence::Unknown,
                if result.stale { Freshness::stale_model() } else { Freshness::fresh() },
                stamps.snapshot_generation,
                stamps.observed_at,
            )
        })
        .collect()
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
        // Truth is assumed, never derived from the 8.0 evidence score.
        assert_eq!(first.truth, FactStatusView::Assumed);
        assert_eq!(first.confidence, Confidence::Unknown);
        assert_eq!(first.evidence_score.value(), 8.0);
        assert_eq!(first.freshness, Freshness::fresh());
        assert_eq!(first.snapshot_generation, 7);

        let second = &evidence[1];
        // Absent from the score map: no supporting evidence, not an error.
        assert_eq!(second.evidence_score.value(), 0.0);
        assert_eq!(second.freshness, Freshness::stale_model());
        // Relevance order follows the fused order.
        assert!(first.relevance.value() > second.relevance.value());
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
            Relevance::new(1.0).expect("valid"),
            EvidenceScore::new(EVIDENCE_SCORE_MAX).expect("valid"),
            Confidence::Unknown,
            Freshness::fresh(),
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
            Relevance::new(0.9).expect("valid"),
            EvidenceScore::new(9.0).expect("valid"),
            Confidence::known(0.75, concerto_core::memory::ConfidenceSource::ModelSelfReport)
                .expect("valid"),
            Freshness::fresh(),
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
            Relevance::clamped(0.5),
            EvidenceScore::clamped(5.0),
            Confidence::Unknown,
            Freshness::fresh(),
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
            Relevance::clamped(0.5),
            EvidenceScore::clamped(5.0),
            Confidence::Unknown,
            Freshness::fresh(),
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
            Relevance::clamped(0.1),
            EvidenceScore::clamped(1.0),
            Confidence::Unknown,
            Freshness::fresh(),
            1,
            stamps().observed_at,
        );
        let mut value = serde_json::to_value(&evidence).expect("serializes");
        value["schema_version"] = serde_json::json!(EVIDENCE_SCHEMA_VERSION + 1);
        assert!(MemoryEvidence::from_json(&value.to_string()).is_err());
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
