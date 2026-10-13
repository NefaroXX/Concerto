//! Phase 4 memory types — the shared shape of everything the memory
//! subsystem produces and consumes.
//!
//! These types are used by the indexer, vector store, RAG pipeline,
//! summarizer, and orchestrator. Every crate in the workspace that
//! touches memory imports from here rather than defining its own
//! ad-hoc variants.

use crate::ids::Ulid;
// Re-use the canonical ProjectId from types — do not redefine it here.
pub use crate::types::ProjectId;
use crate::CancellationToken;
use async_trait::async_trait;
use camino::Utf8PathBuf;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::HashMap;
use time::OffsetDateTime;

// ---------------------------------------------------------------------------
// MemoryId
// ---------------------------------------------------------------------------

/// Uniquely identifies a memory entry.
///
/// Backed by a ULID so entries are time-sortable without a separate
/// timestamp index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MemoryId(pub Ulid);

impl std::fmt::Display for MemoryId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

// ---------------------------------------------------------------------------
// MemoryNamespace
// ---------------------------------------------------------------------------

/// Memory namespaces for project-scoped and global storage.
///
/// - `Project(ProjectId)` — per-project facts, embeddings, entities.
/// - `Global { user_id_hash }` — user-level preferences and facts that
///   apply across all projects. The hash is `blake3(username + hostname)`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum MemoryNamespace {
    /// Project-scoped memory.
    Project(ProjectId),
    /// Global, user-scoped memory.
    Global { user_id_hash: String },
}

// ---------------------------------------------------------------------------
// ChunkType
// ---------------------------------------------------------------------------

/// The kind of content a chunk represents.
///
/// The current indexer creates simple language-aware line chunks for recognized
/// files and uses `SlidingWindow` for unknown extensions. Semantic variants are
/// retained for stored/generated entries and future chunkers. `SessionSummary`
/// and `Fact` are produced by the summarizer and entity extractor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum ChunkType {
    Function,
    Struct,
    Trait,
    Impl,
    Enum,
    Module,
    Test,
    /// Non-code or unknown file extension — sliding-window chunked.
    SlidingWindow,
    /// LLM-produced session summary.
    SessionSummary,
    /// Extracted architectural fact.
    Fact,
}

// ---------------------------------------------------------------------------
// MemoryFilter
// ---------------------------------------------------------------------------

/// Filter applied to a memory query before ranking.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum MemoryFilter {
    /// Restrict to a single chunk type.
    ChunkType(ChunkType),
    /// Glob pattern for file paths (e.g. `src/**/*.rs`).
    FileGlob(String),
    /// Minimum hybrid score threshold.
    MinScore(f64),
    /// Exclude entries whose `expires_at` is in the past.
    ExcludeStale,
}

// ---------------------------------------------------------------------------
// MemoryQuery
// ---------------------------------------------------------------------------

/// A query against the memory store.
///
/// Carries the raw text, project scope, namespace, and optional filters.
/// The vector store and FTS both consume this shape.
#[derive(Debug, Clone)]
pub struct MemoryQuery {
    pub text: String,
    pub project_id: ProjectId,
    pub namespace: MemoryNamespace,
    pub top_k: usize,
    pub filters: Vec<MemoryFilter>,
}

// ---------------------------------------------------------------------------
// MemoryChunk
// ---------------------------------------------------------------------------

/// A single chunk returned by the hybrid retriever.
///
/// The `score` is the combined hybrid score (vector similarity + BM25).
/// `model_id` and `model_version` identify which embedding model produced
/// the vector so the staleness checker can detect drift.
///
/// H11: serializable and comparable so the evidence-bearing recall
/// projection ([`EvidentChunk`]) can carry the chunk itself.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryChunk {
    pub id: String,
    pub project_id: ProjectId,
    pub namespace: MemoryNamespace,
    pub content: String,
    pub file_path: Option<Utf8PathBuf>,
    pub start_line: Option<u32>,
    pub end_line: Option<u32>,
    pub chunk_type: ChunkType,
    pub score: f64,
    pub model_id: String,
    pub model_version: String,
    /// `true` when the row was produced by an older embedding model version
    /// and has not been re-indexed yet.  Stale rows are still searchable but
    /// are rank-demoted behind fresh rows in hybrid retrieval results.
    pub stale: bool,
}

// ---------------------------------------------------------------------------
// MemoryEntry
// ---------------------------------------------------------------------------

/// A persisted memory entry in any store layer.
///
/// This is the write-side counterpart of `MemoryChunk` (which is the
/// read-side retrieval result). Entries carry full metadata including
/// the embedding model version so the staleness manager can detect
/// model drift and trigger re-indexing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryEntry {
    pub id: MemoryId,
    pub project_id: ProjectId,
    pub namespace: MemoryNamespace,
    pub content: String,
    pub chunk_type: ChunkType,
    pub model_id: Option<String>,
    pub model_version: Option<String>,
    pub metadata: serde_json::Value,
    pub expires_at: Option<OffsetDateTime>,
    pub created_at: OffsetDateTime,
}

// ---------------------------------------------------------------------------
// EmbeddingRecord
// ---------------------------------------------------------------------------

/// A stored embedding vector with its metadata.
///
/// Written by the indexer, consumed by the vector store. The `stale` flag
/// is set by `EmbeddingVersionChecker` when the embedding model version
/// changes.
#[derive(Debug, Clone)]
pub struct EmbeddingRecord {
    pub id: String,
    pub project_id: ProjectId,
    pub chunk_hash: String,
    pub content: String,
    pub file_path: Utf8PathBuf,
    pub start_line: Option<u32>,
    pub end_line: Option<u32>,
    pub chunk_type: ChunkType,
    pub vector: Vec<f32>,
    pub model_id: String,
    pub model_version: String,
    pub stale: bool,
    pub created_at: OffsetDateTime,
}

// ---------------------------------------------------------------------------
// MemoryLink — symbolic links between chunks (ADR-69)
// ---------------------------------------------------------------------------

/// The kind of a symbolic link between two memory chunks (ADR-69 slice 1).
///
/// Slice 1 emits `Supersedes` and `Merges` from the L1 dedup judge's
/// update/merge verdicts, and `References` for the `refs` / `result_ref`
/// metadata carried on those paths. Slice 2 adds the evidence vocabulary
/// (`supports`, `contradicts`, `extends`) consumed by the cascade scorer.
/// The enum is `#[non_exhaustive]` so every downstream match stays explicit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum MemoryLinkKind {
    /// The source supersedes the target — the target is now stale.
    Supersedes,
    /// The source extends / folds in the target's content.
    Merges,
    /// The source references the target for context.
    References,
    /// The source independently supports / corroborates the target
    /// (ADR-69 slice 2 evidence).
    Supports,
    /// The source contradicts the target (ADR-69 slice 2 evidence).
    Contradicts,
    /// The source elaborates / extends the target (ADR-69 slice 2 evidence).
    Extends,
}

impl MemoryLinkKind {
    /// The stable string form used in the `memory_links.link_type` column
    /// and in serialized metadata.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Supersedes => "supersedes",
            Self::Merges => "merges",
            Self::References => "references",
            Self::Supports => "supports",
            Self::Contradicts => "contradicts",
            Self::Extends => "extends",
        }
    }

    /// Parse a stored `link_type` string back into a link kind.
    ///
    /// Returns `None` for anything unrecognized (for example a future
    /// vocabulary read by an older binary) so callers fail open by
    /// skipping the link instead of failing the read.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "supersedes" => Some(Self::Supersedes),
            "merges" => Some(Self::Merges),
            "references" => Some(Self::References),
            "supports" => Some(Self::Supports),
            "contradicts" => Some(Self::Contradicts),
            "extends" => Some(Self::Extends),
            _ => None,
        }
    }
}

/// A directed, typed edge between two memory chunks (ADR-69 slice 1).
///
/// Written as a side effect of the L1 dedup update/merge paths, where the
/// newly stored chunk points at the chunk it supersedes or merges. The
/// `weight` default of `1.0` is the neutral strength used once slice-2
/// scoring reads links back.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryLink {
    /// The source chunk id.
    pub from: String,
    /// The target chunk id.
    pub to: String,
    /// The relationship from `from` to `to`.
    pub kind: MemoryLinkKind,
    /// Link strength for later scoring; `1.0` unless overridden.
    pub weight: f64,
}

impl MemoryLink {
    /// A default-weight (`1.0`) link from `from` to `to`.
    pub fn new(from: impl Into<String>, to: impl Into<String>, kind: MemoryLinkKind) -> Self {
        Self { from: from.into(), to: to.into(), kind, weight: 1.0 }
    }
}

// ---------------------------------------------------------------------------
// Decision types
// ---------------------------------------------------------------------------

/// Uniquely identifies a decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DecisionId(pub Ulid);

impl std::fmt::Display for DecisionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Category of a decision for filtering and analysis.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum DecisionCategory {
    Architecture,
    Implementation,
    Test,
    Tooling,
    Other,
}

/// A recorded decision with confidence and supersession tracking.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Decision {
    pub id: DecisionId,
    pub session_id: Ulid,
    pub task_id: Option<crate::types::TaskId>,
    pub what: String,
    pub why: String,
    pub outcome: Option<String>,
    pub category: DecisionCategory,
    pub confidence: f32,
    pub superseded_by: Option<DecisionId>,
    pub created_at: OffsetDateTime,
}

impl Decision {
    /// Read-site lift of the stored model self-report into [`Confidence`].
    ///
    /// Additive and trivial by design: the `confidence: f32` column is
    /// untouched (no storage change), and this interprets it as
    /// [`ConfidenceSource::ModelSelfReport`] — uncalibrated by construction.
    /// A stored NaN, infinite, or out-of-range report becomes
    /// [`Confidence::Unknown`] rather than a clamped invention: the store
    /// never promised a calibration, so a broken one reads as absent.
    #[must_use]
    pub fn confidence_evidence(&self) -> Confidence {
        Confidence::known(f64::from(self.confidence), ConfidenceSource::ModelSelfReport)
            .unwrap_or(Confidence::Unknown)
    }
}

// ---------------------------------------------------------------------------
// ADR-82 slice 7 — MemoryEvidence vocabulary (truth / relevance /
// corroboration / confidence / freshness axes)
// ---------------------------------------------------------------------------

/// Ceiling of the corroboration axis: the `0..=LINK_SCORE_MAX` range of
/// [`EvidenceScore`].
///
/// Single source of truth for the bound. The memory crate's
/// `scoring::LINK_SCORE_MAX` aliases this constant, so the cascade reorder,
/// the orphan prune, and this vocabulary can never drift apart.
pub const EVIDENCE_SCORE_MAX: f64 = 10.0;

/// Where a [`Confidence::Known`] value was reported from.
///
/// `#[non_exhaustive]`: new calibration sources arrive as new variants, and
/// every downstream match must stay explicit. This enum is never constructed
/// outside `concerto-core` today (read-site lifts live next to the stored
/// fields they lift), so the exhaustiveness cost lands only on future
/// constructors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum ConfidenceSource {
    /// The model reported its own certainty (e.g. `Decision.confidence`).
    /// Uncalibrated by construction — a self-report, not a measurement.
    ModelSelfReport,
    /// An empirical calibration of past accuracy (reserved: no producer
    /// emits this yet; the variant exists so the vocabulary need not break
    /// when calibration lands).
    RuntimeCalibration,
}

impl std::fmt::Display for ConfidenceSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ModelSelfReport => write!(f, "model-self-report"),
            Self::RuntimeCalibration => write!(f, "runtime-calibration"),
        }
    }
}

/// How much the stored evidence should be trusted — and whether that
/// question even has an answer.
///
/// `Unknown` is a first-class state, not a zero: it has **no numeric
/// accessor** (not even `Option<f64>` — the only exit is pattern matching),
/// renders as the literal word `unknown` (never `0.0`, per the world-model
/// `bounded()` render discipline: a missing calibration must never read as
/// measured certainty-of-nothing), and deserializes from the literal string
/// `"unknown"`.
///
/// There is deliberately **no clamping constructor**: clamping a confidence
/// invents certainty the evidence never had. The only `Known` path is the
/// rejecting [`Self::known`], which refuses NaN, infinities, and
/// out-of-range values.
///
/// There is deliberately **no `From<f64>`** (in either direction) and no
/// `Option<Confidence>` anywhere: `None` beside `Some(Unknown)` would be two
/// different unknowns, and a missing key collapsing to `Known(0.0)` would
/// re-open the exact drift hazard this type closes. Struct fields holding a
/// `Confidence` use `#[serde(default)]` so a missing key means `Unknown` —
/// one unknown, spelled one way. (Absence of the forbidden impls is
/// constructor-plus-convention-plus-test enforcement, not a proof: a caller
/// holding a raw `f64` can still deliberately call `known()` with the wrong
/// number. Verified by grep — see the test module: no
/// `impl From<…> for Confidence`, no `Option<Confidence>`, no `value()`
/// accessor must appear in this file.)
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Confidence {
    /// No calibration exists for this evidence. Renders `unknown`.
    Unknown,
    /// A validated 0.0..=1.0 calibration with its reporting source.
    Known {
        /// Validated at construction and deserialization: always finite and
        /// within `0.0..=1.0`. Readable only by pattern matching.
        value: f64,
        source: ConfidenceSource,
    },
}

impl Default for Confidence {
    /// Missing keys deserialize to `Unknown` (supports `#[serde(default)]`).
    fn default() -> Self {
        Self::Unknown
    }
}

impl Confidence {
    /// The absence constructor.
    #[must_use]
    pub fn unknown() -> Self {
        Self::Unknown
    }

    /// The only `Known` path: rejecting, never clamping. Returns `None`
    /// for NaN, infinities, and values outside `0.0..=1.0` — including a
    /// `Known(0.0)` attempt with an invalid source is impossible by
    /// construction (the source travels with the value).
    #[must_use]
    pub fn known(value: f64, source: ConfidenceSource) -> Option<Self> {
        if valid_unit_value(value) {
            Some(Self::Known { value, source })
        } else {
            None
        }
    }

    /// Whether any calibration exists.
    #[must_use]
    pub fn is_known(&self) -> bool {
        matches!(self, Self::Known { .. })
    }

    /// The reporting source of a `Known` value, `None` for `Unknown`.
    /// This exposes the *source*, never the number — the numeric exit
    /// remains pattern matching alone.
    #[must_use]
    pub fn source(&self) -> Option<ConfidenceSource> {
        match self {
            Self::Known { source, .. } => Some(*source),
            Self::Unknown => None,
        }
    }
}

impl std::fmt::Display for Confidence {
    /// `Unknown` renders as the literal word `unknown` — never `0.0`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unknown => write!(f, "unknown"),
            Self::Known { value, source } => write!(f, "{value:.2} ({source})"),
        }
    }
}

impl Serialize for Confidence {
    /// `Unknown` serializes as the literal string `"unknown"`; `Known`
    /// serializes as `{"value": <0.0..=1.0>, "source": <kebab-case>}`.
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Unknown => serializer.serialize_str("unknown"),
            Self::Known { value, source } => {
                KnownConfidenceView { value: *value, source: *source }.serialize(serializer)
            }
        }
    }
}

/// Serde view of the `Known` arm (construction still goes through
/// [`Confidence::known`], so validation cannot be bypassed).
#[derive(Serialize, Deserialize)]
struct KnownConfidenceView {
    value: f64,
    source: ConfidenceSource,
}

impl<'de> Deserialize<'de> for Confidence {
    /// Boundary enforcement: accepts the literal `"unknown"` and validated
    /// `{"value", "source"}` maps only. Rejects NaN, infinities,
    /// out-of-range values, and every unknown-collapsing shape (`null`,
    /// bare numbers, other strings, sequences, and maps missing either
    /// key) — a hostile or stale payload never reaches an invalid
    /// `Confidence`. A *missing* key is not handled here: enclosing structs
    /// declare `#[serde(default)]` so absence means `Unknown`.
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ConfidenceVisitor;

        impl<'de> serde::de::Visitor<'de> for ConfidenceVisitor {
            type Value = Confidence;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "the string \"unknown\" or a {{\"value\", \"source\"}} map")
            }

            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Self::Value, E> {
                if value == "unknown" {
                    Ok(Confidence::Unknown)
                } else {
                    Err(E::invalid_value(serde::de::Unexpected::Str(value), &self))
                }
            }

            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                map: A,
            ) -> Result<Self::Value, A::Error> {
                let view = KnownConfidenceView::deserialize(
                    serde::de::value::MapAccessDeserializer::new(map),
                )?;
                Confidence::known(view.value, view.source).ok_or_else(|| {
                    serde::de::Error::invalid_value(
                        serde::de::Unexpected::Float(view.value),
                        &"a finite value within 0.0..=1.0",
                    )
                })
            }
        }

        deserializer.deserialize_any(ConfidenceVisitor)
    }
}

/// Whether `value` is a usable unit-range calibration: finite and within
/// `0.0..=1.0`. NaN fails the range check (unordered), infinities fail both.
fn valid_unit_value(value: f64) -> bool {
    value.is_finite() && (0.0..=1.0).contains(&value)
}

/// Query-time relevance of a chunk to the query, `0.0..=1.0`.
///
/// A distinct newtype from [`EvidenceScore`] on purpose: relevance (does
/// this match the query?) and corroboration (do other chunks support this?)
/// share neither range nor meaning, so they share no conversions either —
/// no `From<f64>` in either direction and no `From`/`Into` between the two
/// (grep-verified, see the test module). Two constructors: rejecting
/// [`Self::new`] for validated inputs, clamping [`Self::clamped`] for
/// order-preserving lifts of unbounded rank signals (NaN floors to `0.0`:
/// an unordered signal ranks last, never first).
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(transparent)]
pub struct Relevance(f64);

impl Relevance {
    /// Rejecting constructor: `None` for NaN, infinities, and values
    /// outside `0.0..=1.0`.
    #[must_use]
    pub fn new(value: f64) -> Option<Self> {
        if valid_unit_value(value) {
            Some(Self(value))
        } else {
            None
        }
    }

    /// Clamping constructor for lifting unbounded rank signals: NaN maps to
    /// `0.0` (`f64::clamp` would panic on NaN), everything else clamps to
    /// `0.0..=1.0` order-preservingly.
    #[must_use]
    pub fn clamped(value: f64) -> Self {
        if value.is_nan() {
            Self(0.0)
        } else {
            Self(value.clamp(0.0, 1.0))
        }
    }

    /// The validated score.
    #[must_use]
    pub fn value(&self) -> f64 {
        self.0
    }
}

impl<'de> Deserialize<'de> for Relevance {
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

/// Query-time corroboration of a chunk by its incoming links,
/// `0.0..=[`EVIDENCE_SCORE_MAX`]` (the `chunk_link_score` range).
///
/// A distinct newtype from [`Relevance`] on purpose: corroboration (do other
/// chunks support this?) and relevance (does this match the query?) share
/// neither range nor meaning, so they share no conversions either — no
/// `From<f64>` in either direction and no `From`/`Into` between the two
/// (grep-verified, see the test module). Two constructors: rejecting
/// [`Self::new`] for validated inputs, clamping [`Self::clamped`] for lifts
/// of scorer outputs that may overshoot (NaN floors to `0.0`: an unordered
/// signal corroborates nothing).
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(transparent)]
pub struct EvidenceScore(f64);

impl EvidenceScore {
    /// Rejecting constructor: `None` for NaN, infinities, and values
    /// outside `0.0..=[`EVIDENCE_SCORE_MAX`]`.
    #[must_use]
    pub fn new(value: f64) -> Option<Self> {
        if value.is_finite() && (0.0..=EVIDENCE_SCORE_MAX).contains(&value) {
            Some(Self(value))
        } else {
            None
        }
    }

    /// Clamping constructor for lifting scorer outputs: NaN maps to `0.0`
    /// (`f64::clamp` would panic on NaN), everything else clamps to
    /// `0.0..=[`EVIDENCE_SCORE_MAX`]` order-preservingly.
    #[must_use]
    pub fn clamped(value: f64) -> Self {
        if value.is_nan() {
            Self(0.0)
        } else {
            Self(value.clamp(0.0, EVIDENCE_SCORE_MAX))
        }
    }

    /// The validated score.
    #[must_use]
    pub fn value(&self) -> f64 {
        self.0
    }
}

impl<'de> Deserialize<'de> for EvidenceScore {
    /// Boundary enforcement: rejects NaN, infinities, and out-of-range
    /// values at deserialization, mirroring [`Self::new`].
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = f64::deserialize(deserializer)?;
        Self::new(value).ok_or_else(|| {
            serde::de::Error::invalid_value(
                serde::de::Unexpected::Float(value),
                &"a finite value within 0.0..=EVIDENCE_SCORE_MAX",
            )
        })
    }
}

/// Truth status of a memory item, in core vocabulary.
///
/// Core may not name the real `FactStatus` (it lives in the orchestrator,
/// which sits above core — dependency-layering rule), so this enum mirrors
/// its *shape* without naming it; the conversion lives in the orchestrator
/// (`FactStatus::to_view`), the only crate that sees both. `OutsideWindow`,
/// `Superseded`, and `Unknown` have no orchestrator source: memory assigns
/// them at retrieval — `OutsideWindow` when the corroborating evidence has
/// decayed beyond the window (the item may once have been true, but nothing
/// in range corroborates it now), `Superseded` when a newer chunk or a
/// superseded decision replaced the item, and `Unknown` when no substrate
/// establishes a status at all (never a default that reads as fine).
///
/// `#[non_exhaustive]` (review hardening): a future variant must be a
/// compile-visible act at every downstream match — no match may silently
/// keep meaning what it meant. There is no constructor from any score
/// type — truth is never computed from a number (G-NOUPGRADE preserved by
/// construction: no API takes an `f64` and returns a `FactStatusView`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum FactStatusView {
    /// Backed by an executed observation.
    Verified,
    /// Backed only by an assertion.
    Assumed,
    /// Invalidated by a newer write or a generation change.
    Stale,
    /// An executed observation contradicts the claim.
    Contradicted,
    /// Corroborating evidence decayed beyond the window. Memory-side only:
    /// assigned at retrieval, never by the orchestrator conversion.
    OutsideWindow,
    /// A newer chunk (or a superseded decision the item cites) replaced
    /// this item. Memory-side only: assigned at retrieval from link and
    /// decision-provenance substrate, never by the orchestrator conversion.
    Superseded,
    /// No substrate establishes a status for this item. Memory-side only:
    /// assigned at retrieval when nothing — links, TTL, observations,
    /// authorship — speaks. Never a default that reads as fine.
    Unknown,
}

impl FactStatusView {
    /// The stable string form used in serialized evidence.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Verified => "verified",
            Self::Assumed => "assumed",
            Self::Stale => "stale",
            Self::Contradicted => "contradicted",
            Self::OutsideWindow => "outside-window",
            Self::Superseded => "superseded",
            Self::Unknown => "unknown",
        }
    }

    /// Parse the stable string form back (H13): used by the supervised
    /// `retrieve-memory` facade to rebuild wire standing. `None` for an
    /// unrecognized label, so a caller can fall back to the standing-free
    /// projection rather than inventing a verdict.
    #[must_use]
    pub fn parse(label: &str) -> Option<Self> {
        match label {
            "verified" => Some(Self::Verified),
            "assumed" => Some(Self::Assumed),
            "stale" => Some(Self::Stale),
            "contradicted" => Some(Self::Contradicted),
            "outside-window" => Some(Self::OutsideWindow),
            "superseded" => Some(Self::Superseded),
            "unknown" => Some(Self::Unknown),
            _ => None,
        }
    }
}

impl std::fmt::Display for FactStatusView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

// ---------------------------------------------------------------------------
// H11 evidence-bearing recall — source attribution + trait-safe projection
// ---------------------------------------------------------------------------

/// Where a recalled memory item comes from (H11 source attribution).
///
/// Assigned at retrieval from recall-visible substrate only — chunk kind,
/// file path, and stored metadata sidecars — never from rank or score.
/// Like [`FactStatusView`], deliberately **not** `#[non_exhaustive]` so a
/// new source is a compile-visible act at every match site.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RecallSource {
    /// Backed by an executed runtime observation (ADR-65: runtime authors
    /// observed facts). Assigned at retrieval ONLY through the
    /// resource-fact observation seam when it verifies the item (a clean,
    /// generation-matching row) — never by assertion, never from chunk
    /// authorship. Reachable in production on every `run_shared_agent` run
    /// with a sessions DB pool open (CLI and desktop runs alike); a recall
    /// from a handle that skipped the attach (e.g. the desktop's
    /// pre-init-only path) never sees this source.
    RuntimeObserved,
    /// Indexer-observed source chunk (a real file path the indexer read).
    IndexedFile,
    /// LLM-produced claim (`Fact` / `SessionSummary` chunk kinds, L1
    /// extraction metadata): a claim until relevant evidence supports it
    /// (ADR-65), so its truth stays [`FactStatusView::Assumed`].
    ModelAuthored,
    /// Consolidation projection carrying a provenance sidecar
    /// (`source_event_ids`): derived, never directly observed.
    Derived,
    /// No substrate establishes a source for this item.
    Unknown,
}

impl RecallSource {
    /// The stable string form used in serialized evidence.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::RuntimeObserved => "runtime-observed",
            Self::IndexedFile => "indexed-file",
            Self::ModelAuthored => "model-authored",
            Self::Derived => "derived",
            Self::Unknown => "unknown",
        }
    }

    /// Parse the stable string form back (H13): used by the supervised
    /// `retrieve-memory` facade to rebuild wire attribution. `None` for an
    /// unrecognized label, so a caller falls back rather than inventing a
    /// source.
    #[must_use]
    pub fn parse(label: &str) -> Option<Self> {
        match label {
            "runtime-observed" => Some(Self::RuntimeObserved),
            "indexed-file" => Some(Self::IndexedFile),
            "model-authored" => Some(Self::ModelAuthored),
            "derived" => Some(Self::Derived),
            "unknown" => Some(Self::Unknown),
            _ => None,
        }
    }

    /// The cheap, always-knowable source rule: authorship from the chunk
    /// kind, index observation from a real file path. Stored-entry and
    /// projection metadata refine this further at full-standing sites
    /// (see `concerto-memory::recall_status`); this rule alone never
    /// claims more than the kind and path establish.
    #[must_use]
    pub fn for_chunk(chunk_type: ChunkType, file_path: Option<&str>) -> Self {
        match chunk_type {
            ChunkType::Fact | ChunkType::SessionSummary => Self::ModelAuthored,
            _ if file_path.is_some_and(|path| !path.is_empty()) => Self::IndexedFile,
            _ => Self::Unknown,
        }
    }
}

impl std::fmt::Display for RecallSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// One recalled chunk with its evidence-bearing verdict (H11).
///
/// The trait-safe projection of the read-time `MemoryEvidence` composite:
/// everything here is a core type so the [`crate::traits::memory`]
/// contract can return it without depending on the memory crate. The full
/// composite (relevance, corroboration score, drift stamps) stays in
/// `concerto-memory::evidence`; this struct carries what an agent needs to
/// answer "what is this and why should I trust it": source attribution,
/// status, the derivation in words, model freshness, and whether retrieval
/// ran degraded.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvidentChunk {
    /// The recalled chunk (its `score` remains the rank signal only —
    /// never freshness; see `derivation`).
    pub chunk: MemoryChunk,
    /// Truth status from deterministic substrate (never from rank/score).
    pub truth: FactStatusView,
    /// Where the item comes from.
    pub source: RecallSource,
    /// Calibration. [`Confidence::Unknown`] when no calibration exists
    /// (the memory path holds none today).
    #[serde(default)]
    pub confidence: Confidence,
    /// Model freshness (`1.0` current model, `0.0` superseded model):
    /// freshness of the *model*, never of the rank. Content age is out of
    /// scope here — see `derivation`.
    pub freshness: f64,
    /// Why this status, in words: which substrate spoke (or that none
    /// did). Bounded at assembly; never carries scores-as-truth.
    pub derivation: String,
    /// `true` when retrieval ran vector-disabled (FTS-only fallback):
    /// reduced capability, surfaced — never hidden. Missing on the wire
    /// means `false` (additive read-time form).
    #[serde(default)]
    pub degraded: bool,
    /// `true` when the recall was interrupted (cancelled) before standing
    /// was fully established: the verdicts below may rest on incomplete
    /// substrate. Machine-readable on purpose — a caller distinguishes an
    /// interrupted recall from a completed recall that genuinely found
    /// nothing WITHOUT parsing prose. Missing on the wire means `false`.
    #[serde(default)]
    pub interrupted: bool,
}

impl EvidentChunk {
    /// Validated constructor: `freshness` must be finite within
    /// `0.0..=1.0` (mirroring the rejecting `Freshness::new` rule without
    /// depending on the memory crate). Returns `None` for NaN, infinities,
    /// and out-of-range values — there is no clamping path that would
    /// invent a freshness the substrate never had.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        chunk: MemoryChunk,
        truth: FactStatusView,
        source: RecallSource,
        confidence: Confidence,
        freshness: f64,
        derivation: String,
        degraded: bool,
        interrupted: bool,
    ) -> Option<Self> {
        if freshness.is_finite() && (0.0..=1.0).contains(&freshness) {
            Some(Self {
                chunk,
                truth,
                source,
                confidence,
                freshness,
                derivation,
                degraded,
                interrupted,
            })
        } else {
            None
        }
    }
}

// ---------------------------------------------------------------------------
// H11 observation seam — resource-fact honesty bits without the sessions
// dependency (the memory crate cannot see the sessions pool, so the
// orchestrator implements this trait over `resource_facts` and attaches
// it to the memory system).
// ---------------------------------------------------------------------------

/// One resource-fact observation handed to recall through the
/// [`PathObservationSource`] seam: the honesty bits of a `resource_facts`
/// row — whether the workspace state is uncertain, which generation the
/// observation belongs to, and which event attests it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PathObservation {
    /// `true` means the workspace state is uncertain (never served clean).
    pub dirty: bool,
    /// Workspace generation of the latest observation, when recorded.
    #[serde(default)]
    pub generation: Option<String>,
    /// The whiteboard event the observation is attributable to, when known.
    #[serde(default)]
    pub last_event_id: Option<String>,
}

/// Optional resource-fact feed for recall (service seam).
///
/// The orchestrator side implements this over `resource_facts` and
/// attaches it via [`crate::traits::memory::MemoryStore::attach_path_observations`].
/// Every method is cancellation-aware; an error, a missing row, or a
/// missing provider reads the path as unobserved (unknown), never as
/// verified. The trait is infallible by contract — fail-open lives inside
/// the provider, never as an error the recall must handle.
#[async_trait]
pub trait PathObservationSource: Send + Sync {
    /// Look up observations for real file paths (bounded: at most one
    /// recall's ids). Absent paths simply have no entry.
    async fn observe_paths(
        &self,
        file_paths: &[String],
        cancel: CancellationToken,
    ) -> HashMap<String, PathObservation>;
}

// ---------------------------------------------------------------------------
// TaskNode — task decomposition tree
// ---------------------------------------------------------------------------

/// Uniquely identifies a task node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TaskNodeId(pub Ulid);

impl std::fmt::Display for TaskNodeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Status of a task node in the decomposition tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum TaskStatus {
    Pending,
    Running,
    Done,
    Failed,
    Blocked,
}

impl TaskStatus {
    /// Return the SQL-storable string representation.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Blocked => "blocked",
        }
    }

    /// Parse from a string (as stored in SQLite).
    pub fn parse_status(s: &str) -> Self {
        match s {
            "running" => Self::Running,
            "done" => Self::Done,
            "failed" => Self::Failed,
            "blocked" => Self::Blocked,
            _ => Self::Pending,
        }
    }
}

/// A node in the task decomposition tree.
///
/// Tasks form a DAG where each node can have children and blocking
/// dependencies. The tree is stored in SQLite and rendered as XML
/// for injection into the working memory prompt block.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskNode {
    pub id: TaskNodeId,
    pub session_id: Ulid,
    pub description: String,
    pub status: TaskStatus,
    pub parent_id: Option<TaskNodeId>,
    pub children: Vec<TaskNodeId>,
    pub blocking: Vec<TaskNodeId>,
    pub created_at: OffsetDateTime,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkingMemorySnapshot {
    pub id: Ulid,
    pub session_id: Ulid,
    pub decisions: Vec<Decision>,
    pub task_tree: Vec<TaskNode>,
    pub created_at: OffsetDateTime,
}

// ---------------------------------------------------------------------------
// FtsResult
// ---------------------------------------------------------------------------

/// A single result from a full-text search.
#[derive(Debug, Clone)]
pub struct FtsResult {
    pub chunk_id: String,
    pub score: f64,
    pub content: String,
    /// Whether the backing row is stale (older model version).
    pub stale: bool,
}

// ---------------------------------------------------------------------------
// VectorResult
// ---------------------------------------------------------------------------

/// A single result from a vector similarity search.
#[derive(Debug, Clone)]
pub struct VectorResult {
    pub chunk_id: String,
    pub score: f64,
    pub content: String,
    /// Whether the backing row is stale (older model version).
    pub stale: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fts_result_fields() {
        let r = FtsResult {
            chunk_id: "chunk1".into(),
            score: 0.95,
            content: "matched text".into(),
            stale: false,
        };
        assert_eq!(r.chunk_id, "chunk1");
        assert_eq!(r.score, 0.95);
        assert_eq!(r.content, "matched text");
        assert!(!r.stale);
    }

    #[test]
    fn vector_result_fields() {
        let r = VectorResult {
            chunk_id: "vec1".into(),
            score: 0.87,
            content: "vector content".into(),
            stale: false,
        };
        assert_eq!(r.chunk_id, "vec1");
        assert_eq!(r.score, 0.87);
        assert_eq!(r.content, "vector content");
        assert!(!r.stale);
    }

    #[test]
    fn fts_result_score_non_negative() {
        // BM25 scores from FTS5 (negated rank) are positive and unbounded.
        for score in [0.0, 0.5, 1.0, 3.2] {
            let r = FtsResult { chunk_id: "c".into(), score, content: String::new(), stale: false };
            assert!(r.score >= 0.0, "FTS score must be non-negative, got {score}");
        }
    }

    #[test]
    fn memory_link_kind_round_trip() {
        for (kind, expected) in [
            (MemoryLinkKind::Supersedes, "supersedes"),
            (MemoryLinkKind::Merges, "merges"),
            (MemoryLinkKind::References, "references"),
            (MemoryLinkKind::Supports, "supports"),
            (MemoryLinkKind::Contradicts, "contradicts"),
            (MemoryLinkKind::Extends, "extends"),
        ] {
            assert_eq!(kind.as_str(), expected);
            assert_eq!(MemoryLinkKind::parse(kind.as_str()), Some(kind));
        }
        assert_eq!(MemoryLinkKind::parse(""), None);
        assert_eq!(MemoryLinkKind::parse("SUPERSEDES"), None, "kinds are lowercase");
    }

    #[test]
    fn memory_link_new_defaults_weight() {
        let link = MemoryLink::new("a", "b", MemoryLinkKind::Supersedes);
        assert_eq!(link.from, "a");
        assert_eq!(link.to, "b");
        assert_eq!(link.kind, MemoryLinkKind::Supersedes);
        assert_eq!(link.weight, 1.0);
    }

    // --- ADR-82 slice 7: axis-separation vocabulary -----------------------
    //
    // Non-interchangeability is constructor-plus-convention-plus-test
    // enforcement, not absolute type safety: a caller holding a raw `f64`
    // can still deliberately construct the wrong newtype. What the types DO
    // guarantee is enforced here and at the serde boundary. The absences
    // below are additionally grep-verified; these patterns must return
    // nothing in this file:
    //   `impl From<.*> for (Confidence|Relevance|EvidenceScore)`
    //   `impl From<(Confidence|Relevance|EvidenceScore)>`
    //   `Option<Confidence>`
    //   `fn value(&self)` on `Confidence` (numeric exit is pattern matching)

    #[test]
    fn confidence_unknown_renders_as_the_literal_word() {
        assert_eq!(Confidence::Unknown.to_string(), "unknown");
        assert_eq!(Confidence::default(), Confidence::Unknown);
        assert!(!Confidence::Unknown.is_known());
        assert_eq!(Confidence::Unknown.source(), None);
        // The word, never a number.
        let json = serde_json::to_string(&Confidence::Unknown).expect("serializes");
        assert_eq!(json, "\"unknown\"");
        let back: Confidence = serde_json::from_str(&json).expect("round-trips");
        assert_eq!(back, Confidence::Unknown);
    }

    #[test]
    fn confidence_known_round_trips_with_source() {
        let known = Confidence::known(0.8, ConfidenceSource::ModelSelfReport).expect("valid");
        assert!(known.is_known());
        assert_eq!(known.source(), Some(ConfidenceSource::ModelSelfReport));
        assert_eq!(known.to_string(), "0.80 (model-self-report)");
        let json = serde_json::to_string(&known).expect("serializes");
        let back: Confidence = serde_json::from_str(&json).expect("round-trips");
        assert_eq!(back, known);
    }

    #[test]
    fn confidence_known_zero_is_not_unknown() {
        // 0.0 is a measured certainty-of-nothing, distinct from absence.
        let zero = Confidence::known(0.0, ConfidenceSource::ModelSelfReport).expect("0.0 valid");
        assert_ne!(zero, Confidence::Unknown);
        assert_eq!(zero.to_string(), "0.00 (model-self-report)");
        let json = serde_json::to_string(&zero).expect("serializes");
        assert_ne!(json, "\"unknown\"");
        let back: Confidence = serde_json::from_str(&json).expect("round-trips");
        assert_eq!(back, zero);
    }

    #[test]
    fn confidence_known_rejects_nan_infinite_and_out_of_range() {
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -0.1, -1.0, 1.1, 2.0] {
            assert!(
                Confidence::known(bad, ConfidenceSource::ModelSelfReport).is_none(),
                "must reject {bad}"
            );
        }
        // Boundaries are inclusive.
        assert!(Confidence::known(0.0, ConfidenceSource::ModelSelfReport).is_some());
        assert!(Confidence::known(1.0, ConfidenceSource::RuntimeCalibration).is_some());
    }

    #[test]
    fn confidence_deserialize_rejects_unknown_collapsing_inputs() {
        // Every shape that would collapse "no calibration" into a number —
        // or a number into an unvalidated calibration — is refused.
        for raw in [
            "null",
            "0.8",
            "0.0",
            "\"\"",
            "\"UNKNOWN\"",
            "\"none\"",
            "[]",
            "{}",
            "{\"value\": 0.8}",
            "{\"source\": \"model-self-report\"}",
            "{\"value\": 1.5, \"source\": \"model-self-report\"}",
            "{\"value\": -0.1, \"source\": \"model-self-report\"}",
            "{\"value\": 0.8, \"source\": \"no-such-source\"}",
        ] {
            assert!(serde_json::from_str::<Confidence>(raw).is_err(), "must refuse {raw:?}");
        }
        // serde_json has no NaN literal: the closest hostile input is
        // rejected before a float is ever produced.
        assert!(serde_json::from_str::<Confidence>("NaN").is_err());
    }

    #[test]
    fn confidence_missing_key_defaults_to_unknown() {
        // The single-unknown rule: enclosing structs declare
        // `#[serde(default)]`, so absence means Unknown — never a second
        // unknown channel and never a silent 0.0.
        #[derive(Deserialize, PartialEq, Debug)]
        struct Holder {
            #[serde(default)]
            confidence: Confidence,
        }
        let decoded: Holder = serde_json::from_str("{}").expect("missing key loads");
        assert_eq!(decoded.confidence, Confidence::Unknown);
        let decoded: Holder =
            serde_json::from_str("{\"confidence\": \"unknown\"}").expect("word loads");
        assert_eq!(decoded.confidence, Confidence::Unknown);
    }

    #[test]
    fn relevance_rejects_and_clamps_on_distinct_paths() {
        assert!(Relevance::new(0.5).is_some());
        assert!(Relevance::new(0.0).is_some());
        assert!(Relevance::new(1.0).is_some());
        for bad in [f64::NAN, f64::INFINITY, -0.1, 1.1] {
            assert!(Relevance::new(bad).is_none(), "rejecting path must refuse {bad}");
        }
        assert_eq!(Relevance::clamped(2.0).value(), 1.0);
        assert_eq!(Relevance::clamped(-3.0).value(), 0.0);
        // NaN floors (clamp would panic); an unordered signal ranks last.
        assert_eq!(Relevance::clamped(f64::NAN).value(), 0.0);
        // Order is preserved through the clamping lift.
        assert!(Relevance::clamped(0.2).value() < Relevance::clamped(0.3).value());
    }

    #[test]
    fn evidence_score_range_is_the_shared_max() {
        assert_eq!(EVIDENCE_SCORE_MAX, 10.0);
        assert!(EvidenceScore::new(0.0).is_some());
        assert!(EvidenceScore::new(EVIDENCE_SCORE_MAX).is_some());
        for bad in [f64::NAN, f64::INFINITY, -0.1, EVIDENCE_SCORE_MAX + 0.1] {
            assert!(EvidenceScore::new(bad).is_none(), "rejecting path must refuse {bad}");
        }
        assert_eq!(EvidenceScore::clamped(99.0).value(), EVIDENCE_SCORE_MAX);
        assert_eq!(EvidenceScore::clamped(-1.0).value(), 0.0);
        assert_eq!(EvidenceScore::clamped(f64::NAN).value(), 0.0);
    }

    #[test]
    fn score_newtypes_reject_invalid_wire_values() {
        for raw in ["1.5", "-0.1", "null", "\"x\""] {
            assert!(serde_json::from_str::<Relevance>(raw).is_err(), "relevance: {raw:?}");
        }
        for raw in ["10.1", "-0.1", "null", "\"x\""] {
            assert!(serde_json::from_str::<EvidenceScore>(raw).is_err(), "evidence: {raw:?}");
        }
        let relevance: Relevance = serde_json::from_str("0.5").expect("valid relevance");
        assert_eq!(relevance.value(), 0.5);
        let score: EvidenceScore = serde_json::from_str("7.5").expect("valid score");
        assert_eq!(score.value(), 7.5);
    }

    #[test]
    fn fact_status_view_serializes_kebab_case() {
        for (view, expected) in [
            (FactStatusView::Verified, "\"verified\""),
            (FactStatusView::Assumed, "\"assumed\""),
            (FactStatusView::Stale, "\"stale\""),
            (FactStatusView::Contradicted, "\"contradicted\""),
            (FactStatusView::OutsideWindow, "\"outside-window\""),
            (FactStatusView::Superseded, "\"superseded\""),
            (FactStatusView::Unknown, "\"unknown\""),
        ] {
            let json = serde_json::to_string(&view).expect("serializes");
            assert_eq!(json, expected);
            assert_eq!(view.to_string(), view.as_str());
            let back: FactStatusView = serde_json::from_str(&json).expect("round-trips");
            assert_eq!(back, view);
        }
    }

    #[test]
    fn fact_status_view_parse_round_trips_and_rejects_unknown_labels() {
        for view in [
            FactStatusView::Verified,
            FactStatusView::Assumed,
            FactStatusView::Stale,
            FactStatusView::Contradicted,
            FactStatusView::OutsideWindow,
            FactStatusView::Superseded,
            FactStatusView::Unknown,
        ] {
            assert_eq!(FactStatusView::parse(view.as_str()), Some(view));
        }
        assert_eq!(FactStatusView::parse("not-a-status"), None);
    }

    #[test]
    fn recall_source_parse_round_trips_and_rejects_unknown_labels() {
        for source in [
            RecallSource::RuntimeObserved,
            RecallSource::IndexedFile,
            RecallSource::ModelAuthored,
            RecallSource::Derived,
            RecallSource::Unknown,
        ] {
            assert_eq!(RecallSource::parse(source.as_str()), Some(source));
        }
        assert_eq!(RecallSource::parse("not-a-source"), None);
    }

    #[test]
    fn recall_source_rule_never_claims_more_than_kind_and_path() {
        use ChunkType as T;
        // Model-authored kinds are claims whatever the path.
        assert_eq!(RecallSource::for_chunk(T::Fact, Some("src/a.rs")), RecallSource::ModelAuthored);
        assert_eq!(RecallSource::for_chunk(T::SessionSummary, None), RecallSource::ModelAuthored);
        // Indexed kinds with a real path are indexer-observed.
        assert_eq!(
            RecallSource::for_chunk(T::Function, Some("src/a.rs")),
            RecallSource::IndexedFile
        );
        assert_eq!(
            RecallSource::for_chunk(T::SlidingWindow, Some("notes.md")),
            RecallSource::IndexedFile
        );
        // No path, non-claim kind: nothing established.
        assert_eq!(RecallSource::for_chunk(T::Function, None), RecallSource::Unknown);
        assert_eq!(RecallSource::for_chunk(T::Function, Some("")), RecallSource::Unknown);
        // Sources render in kebab-case and round-trip.
        for (source, expected) in [
            (RecallSource::RuntimeObserved, "\"runtime-observed\""),
            (RecallSource::IndexedFile, "\"indexed-file\""),
            (RecallSource::ModelAuthored, "\"model-authored\""),
            (RecallSource::Derived, "\"derived\""),
            (RecallSource::Unknown, "\"unknown\""),
        ] {
            let json = serde_json::to_string(&source).expect("serializes");
            assert_eq!(json, expected);
            assert_eq!(source.to_string(), source.as_str());
            let back: RecallSource = serde_json::from_str(&json).expect("round-trips");
            assert_eq!(back, source);
        }
    }

    #[test]
    fn evident_chunk_rejects_invalid_freshness() {
        let project_id = ProjectId("p".into());
        let chunk = MemoryChunk {
            id: "c".into(),
            project_id: project_id.clone(),
            namespace: MemoryNamespace::Project(project_id),
            content: "c".into(),
            file_path: None,
            start_line: None,
            end_line: None,
            chunk_type: ChunkType::Fact,
            score: 0.9,
            model_id: String::new(),
            model_version: String::new(),
            stale: false,
        };
        // Boundaries are inclusive; NaN/infinities/out-of-range refused —
        // no clamping path invents a freshness.
        assert!(EvidentChunk::new(
            chunk.clone(),
            FactStatusView::Assumed,
            RecallSource::ModelAuthored,
            Confidence::Unknown,
            1.0,
            "claim".into(),
            false,
            false,
        )
        .is_some());
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -0.1, 1.1] {
            assert!(
                EvidentChunk::new(
                    chunk.clone(),
                    FactStatusView::Unknown,
                    RecallSource::Unknown,
                    Confidence::Unknown,
                    bad,
                    "unknown".into(),
                    false,
                    false,
                )
                .is_none(),
                "must refuse freshness {bad}"
            );
        }
    }

    #[test]
    fn interrupted_flag_defaults_false_on_the_wire() {
        // The interruption signal is additive: payloads written before it
        // existed load as uninterrupted, never as interrupted.
        let json = serde_json::json!({
            "chunk": {
                "id": "c",
                "project_id": "p",
                "namespace": {"Project": "p"},
                "content": "c",
                "file_path": null,
                "start_line": null,
                "end_line": null,
                "chunk_type": "Fact",
                "score": 0.0,
                "model_id": "",
                "model_version": "",
                "stale": false,
            },
            "truth": "unknown",
            "source": "unknown",
            "confidence": "unknown",
            "freshness": 1.0,
            "derivation": "unknown",
            "degraded": false,
        });
        let evident: EvidentChunk = serde_json::from_value(json).expect("pre-signal payload loads");
        assert!(!evident.interrupted);
    }

    #[test]
    fn decision_confidence_lift_is_read_only_and_honest() {
        use crate::ids::Ulid;
        let mut decision = Decision {
            id: DecisionId(Ulid::nil()),
            session_id: Ulid::nil(),
            task_id: None,
            what: "w".into(),
            why: "y".into(),
            outcome: None,
            category: DecisionCategory::Architecture,
            confidence: 0.7,
            superseded_by: None,
            created_at: OffsetDateTime::now_utc(),
        };
        // The stored field is untouched by the lift.
        let lifted = decision.confidence_evidence();
        assert_eq!(lifted.source(), Some(ConfidenceSource::ModelSelfReport));
        assert_eq!(decision.confidence, 0.7);
        match lifted {
            Confidence::Known { value, .. } => {
                assert!((value - f64::from(0.7_f32)).abs() < 1e-6);
            }
            Confidence::Unknown => panic!("0.7 must lift to Known"),
        }
        // A broken stored report reads as absent, never as clamped truth.
        decision.confidence = f32::NAN;
        assert_eq!(decision.confidence_evidence(), Confidence::Unknown);
        decision.confidence = 2.0;
        assert_eq!(decision.confidence_evidence(), Confidence::Unknown);
    }
}
