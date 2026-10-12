//! H11 — deterministic evidence-bearing recall status.
//!
//! Derives, per recalled chunk, the five-way answer the plan requires —
//! fresh / stale / contradicted / superseded / unknown — from real
//! substrate only:
//!
//! - link scoring ([`crate::scoring`], read through [`crate::links`]):
//!   live `Contradicts` links contradict, `Supersedes`/`Merges` links
//!   supersede, in-window positive links corroborate, all-decayed links
//!   read as [`FactStatusView::OutsideWindow`];
//! - TTL ([`crate::ttl::suggested_ttl_days`]): a row past its per-type
//!   retention window is [`FactStatusView::Stale`];
//! - decision provenance: a chunk citing a superseded decision is
//!   [`FactStatusView::Superseded`];
//! - resource facts: a clean, generation-matching observation verifies
//!   (the world-model A-CLEAN rule); a dirty or generation-mismatched one
//!   stales. Fed through the [`PathObservationSource`] seam because the
//!   memory crate cannot see the sessions pool — an unwired seam reads as
//!   unknown, never as fine.
//!
//! Ranking-independence is structural, not conventional:
//! [`ChunkStanding`] — the only input [`derive_recall_verdict`] reads —
//! has NO score or rank field, so embedding similarity, RRF fusion, and
//! BM25 rank cannot reach truth or freshness even by accident. The
//! `recall_status_ignores_rank_scores` test in [`crate::evidence`] pins
//! this by holding the query constant and varying only rank.
//!
//! G-NOUPGRADE holds throughout: only an executed observation (the
//! resource-fact seam) verifies; link corroboration never upgrades a
//! model-authored claim past [`FactStatusView::Assumed`].

use std::collections::HashMap;

use async_trait::async_trait;
use concerto_core::memory::{ChunkType, FactStatusView, MemoryLinkKind, RecallSource};
use concerto_core::CancellationToken;
use time::OffsetDateTime;

use crate::recall::truncate_to_chars;
use crate::scoring::{chunk_link_score, decay_factor, kind_weight, TimedLink};
use crate::ttl::suggested_ttl_days;

/// Upper bound on a derivation string (bounded prompt surface).
pub const MAX_DERIVATION_CHARS: usize = 240;

/// The file-path marker the project-namespace store path writes for
/// stored entries (`MemorySystem::store`): such a path names no real file,
/// so it never counts as index observation.
pub const STORED_ENTRY_PATH: &str = "memory";

/// One resource-fact observation handed to recall through the
/// [`PathObservationSource`] seam: the honesty bits of a `resource_facts`
/// row, without the sessions-pool dependency.
#[derive(Debug, Clone, PartialEq)]
pub struct PathObservation {
    /// `true` means the workspace state is uncertain (never served clean).
    pub dirty: bool,
    /// Workspace generation of the latest observation, when recorded.
    pub generation: Option<String>,
    /// The whiteboard event the observation is attributable to, when known.
    pub last_event_id: Option<String>,
}

/// Optional resource-fact feed for recall (service seam).
///
/// The memory crate cannot see the sessions pool, so the orchestrator side
/// implements this over `resource_facts` and attaches it to the memory
/// system. Every method is cancellation-aware; an error or a missing
/// provider reads every path as unobserved (unknown), never as verified.
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

/// A decision this chunk cites (from its metadata sidecar), with whether
/// the decision journal records it as superseded.
#[derive(Debug, Clone, PartialEq)]
pub struct CitedDecision {
    /// The cited decision id.
    pub id: String,
    /// `true` when the decision store records `superseded_by` for it.
    pub superseded: bool,
}

/// Per-chunk substrate snapshot: everything status derivation may read.
///
/// Deliberately has NO score or rank field — ranking cannot leak into
/// freshness or status by construction (see the module docs).
#[derive(Debug, Clone)]
pub struct ChunkStanding {
    /// The chunk's kind (authorship rule).
    pub chunk_type: ChunkType,
    /// The chunk's file path, when it names a real file.
    pub file_path: Option<String>,
    /// ADR-12 model-version staleness flag.
    pub stale_model: bool,
    /// When the backing row was written (TTL substrate; `None` unknown).
    pub created_at: Option<OffsetDateTime>,
    /// The row's JSON metadata sidecar (authorship/provenance/decision
    /// cites; `None` when the row carries none).
    pub metadata: Option<serde_json::Value>,
    /// Incoming links with decoded timestamps (link substrate).
    pub incoming: Vec<TimedLink>,
    /// A cited decision, when the metadata names one.
    pub cited_decision: Option<CitedDecision>,
    /// A resource-fact observation for the chunk's path, when the seam
    /// provided one.
    pub path_observation: Option<PathObservation>,
}

/// The per-recall context derivation needs beyond one chunk's standing.
#[derive(Debug, Clone)]
pub struct DerivationContext {
    /// Evidence decay window in days (`None` disables decay, as does
    /// `Some(0)` — matching [`crate::scoring::decay_factor`]).
    pub decay_days: Option<u16>,
    /// The caller's clock (unix seconds, UTC).
    pub now_unix: i64,
    /// The current workspace snapshot generation, when known. `None`
    /// takes the lax A-CLEAN branch (a clean row verifies without a
    /// generation to match against) — the memory system holds no
    /// generation, so this documents exactly what "no current generation
    /// known" means there.
    pub current_generation: Option<String>,
}

/// The derived five-way answer for one chunk.
#[derive(Debug, Clone, PartialEq)]
pub struct RecallVerdict {
    /// Truth status from deterministic substrate (never from rank/score).
    pub truth: FactStatusView,
    /// Where the item comes from.
    pub source: RecallSource,
    /// Model freshness (`false` = current model, `true` = superseded
    /// model): freshness of the *model*, never of the rank.
    pub stale_model: bool,
    /// Why this status, in words (bounded to [`MAX_DERIVATION_CHARS`]).
    pub derivation: String,
}

/// Derive one chunk's verdict. Pure and deterministic: the same standing
/// and context always yield the same verdict. Precedence (first match
/// wins):
///
/// 1. TTL-expired → `Stale`.
/// 2. A live (in-window) `Contradicts` link → `Contradicted`.
/// 3. A `Supersedes`/`Merges` link, or a cited superseded decision →
///    `Superseded`.
/// 4. Superseded embedding model → `Stale` (a model-generation change).
/// 5. Resource observation: dirty or generation-mismatched → `Stale`;
///    clean and generation-matching → `Verified` (the only upgrade path —
///    an executed observation).
/// 6. Links exist but all decayed → `OutsideWindow`.
/// 7. Model-authored → `Assumed` (a claim; support never verifies).
/// 8. Otherwise → `Unknown`, with the derivation citing whatever age or
///    corroboration is known (or that nothing speaks at all).
#[must_use]
pub fn derive_recall_verdict(standing: &ChunkStanding, ctx: &DerivationContext) -> RecallVerdict {
    let source = resolve_source(standing);
    let stale_model = standing.stale_model;
    let derivation;
    let truth;

    if let Some(expired) = ttl_expiry(standing, ctx) {
        derivation = expired;
        truth = FactStatusView::Stale;
    } else if let Some(contradicted) = live_contradiction(standing, ctx) {
        derivation = contradicted;
        truth = FactStatusView::Contradicted;
    } else if let Some(superseded) = supersession(standing) {
        derivation = superseded;
        truth = FactStatusView::Superseded;
    } else if stale_model {
        derivation = "superseded embedding model (content may still be right)".to_string();
        truth = FactStatusView::Stale;
    } else if let Some(observed) = resource_observation(standing, ctx) {
        derivation = observed.1;
        truth = observed.0;
    } else if all_evidence_decayed(standing, ctx) {
        derivation = format!(
            "corroborating evidence decayed beyond the {}-day window",
            ctx.decay_days.unwrap_or(0)
        );
        truth = FactStatusView::OutsideWindow;
    } else if source == RecallSource::ModelAuthored {
        derivation = claim_derivation(standing, ctx);
        truth = FactStatusView::Assumed;
    } else {
        derivation = unknown_derivation(standing, ctx, &source);
        truth = FactStatusView::Unknown;
    }

    let mut bounded = derivation;
    truncate_to_chars(&mut bounded, MAX_DERIVATION_CHARS);
    RecallVerdict { truth, source, stale_model, derivation: bounded }
}

/// Authorship/provenance source from kind, path, and metadata sidecar.
///
/// Metadata refines the cheap kind/path rule: a consolidation provenance
/// sidecar (`source_event_ids`) reads as `Derived`; L1 extraction markers
/// (`l1_kind` / `derived_from`) read as `ModelAuthored`. The
/// [`STORED_ENTRY_PATH`] sentinel never counts as a real file.
fn resolve_source(standing: &ChunkStanding) -> RecallSource {
    if let Some(metadata) = standing.metadata.as_ref() {
        if metadata
            .get("source_event_ids")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|ids| !ids.is_empty())
        {
            return RecallSource::Derived;
        }
        if metadata.get("l1_kind").is_some() || metadata.get("derived_from").is_some() {
            return RecallSource::ModelAuthored;
        }
    }
    match standing.chunk_type {
        ChunkType::Fact | ChunkType::SessionSummary => RecallSource::ModelAuthored,
        _ => match standing.file_path.as_deref() {
            Some(path) if !path.is_empty() && path != STORED_ENTRY_PATH => {
                RecallSource::IndexedFile
            }
            _ => RecallSource::Unknown,
        },
    }
}

/// TTL-expiry derivation, or `None` when the row is in-window or its age
/// is unknown. Reuses the [`suggested_ttl_days`] windows, so recall and
/// the [`crate::ttl`] purge can never disagree on what "expired" means.
fn ttl_expiry(standing: &ChunkStanding, ctx: &DerivationContext) -> Option<String> {
    let created = standing.created_at?;
    let window_days = suggested_ttl_days(&standing.chunk_type);
    let age_secs = ctx.now_unix - created.unix_timestamp();
    if age_secs < window_days * 86_400 {
        return None;
    }
    let age_days = age_secs.max(0) / 86_400;
    Some(format!(
        "past its {window_days}-day retention window (written {age_days}d ago); \
         no revalidation recorded"
    ))
}

/// Whether a link still carries live evidence (in-window). Decay disabled
/// means every link is live.
fn link_is_live(created_unix: i64, ctx: &DerivationContext) -> bool {
    let age_days = (ctx.now_unix - created_unix) as f64 / 86_400.0;
    decay_factor(age_days, ctx.decay_days) > 0.0
}

/// Contradiction derivation citing the newest live `Contradicts` link, or
/// `None` when no live contradiction exists.
fn live_contradiction(standing: &ChunkStanding, ctx: &DerivationContext) -> Option<String> {
    let newest = standing
        .incoming
        .iter()
        .filter(|timed| timed.link.kind == MemoryLinkKind::Contradicts)
        .filter(|timed| link_is_live(timed.created_unix, ctx))
        .max_by_key(|timed| timed.created_unix)?;
    let age_days = (ctx.now_unix - newest.created_unix).max(0) / 86_400;
    Some(format!("contradicted by {} ({age_days}d old)", newest.link.from))
}

/// Supersession derivation: the newest superseding link wins over a cited
/// superseded decision (chunk-level replacement is the nearer cause).
fn supersession(standing: &ChunkStanding) -> Option<String> {
    if let Some(newest) = standing
        .incoming
        .iter()
        .filter(|timed| {
            timed.link.kind == MemoryLinkKind::Supersedes
                || timed.link.kind == MemoryLinkKind::Merges
        })
        .max_by_key(|timed| timed.created_unix)
    {
        if newest.link.kind == MemoryLinkKind::Supersedes {
            return Some(format!("superseded by {}", newest.link.from));
        }
        return Some(format!("merged into {}", newest.link.from));
    }
    if let Some(cited) = standing.cited_decision.as_ref() {
        if cited.superseded {
            return Some(format!("cites superseded decision {}", cited.id));
        }
    }
    None
}

/// Resource-fact observation verdict (the world-model A-CLEAN/A-DIRTY
/// rules at recall): dirty or generation-mismatched rows stale the item;
/// a clean row whose generation matches (or with no current generation
/// known) verifies it — the ONLY upgrade path, and only from an executed
/// observation.
fn resource_observation(
    standing: &ChunkStanding,
    ctx: &DerivationContext,
) -> Option<(FactStatusView, String)> {
    let observation = standing.path_observation.as_ref()?;
    if observation.dirty {
        let event = observation.last_event_id.as_deref().unwrap_or("unattributed");
        return Some((
            FactStatusView::Stale,
            format!(
                "resource-fact row dirty (last observation {event}); workspace state uncertain"
            ),
        ));
    }
    match (&observation.generation, &ctx.current_generation) {
        (Some(row_gen), Some(current)) if row_gen != current => Some((
            FactStatusView::Stale,
            format!("observed at generation {row_gen}; workspace now at {current}"),
        )),
        _ => {
            let event = observation.last_event_id.as_deref().unwrap_or("unattributed");
            let generation = observation.generation.as_deref().map_or_else(
                || "unknown generation".to_string(),
                |gen| format!("generation {gen}"),
            );
            Some((
                FactStatusView::Verified,
                format!("clean resource-fact observation {event} ({generation})"),
            ))
        }
    }
}

/// `true` when incoming links exist but every one decayed beyond the
/// window — the item may once have been corroborated, but nothing in
/// range corroborates it now.
fn all_evidence_decayed(standing: &ChunkStanding, ctx: &DerivationContext) -> bool {
    !standing.incoming.is_empty()
        && standing.incoming.iter().all(|timed| !link_is_live(timed.created_unix, ctx))
}

/// The claim derivation: a model-authored item stays a claim, with its
/// live support level stated (support never verifies — G-NOUPGRADE).
fn claim_derivation(standing: &ChunkStanding, ctx: &DerivationContext) -> String {
    let (supporting, score) = live_support(standing, ctx);
    if supporting == 0 {
        "model-authored claim; no supporting evidence in range".to_string()
    } else {
        format!(
            "model-authored claim supported by {supporting} link(s) \
             (corroboration {score:.1}/10); support never verifies"
        )
    }
}

/// The unknown derivation: cites whatever age or corroboration is known,
/// or states that nothing speaks at all — never a default that reads as
/// fine.
fn unknown_derivation(
    standing: &ChunkStanding,
    ctx: &DerivationContext,
    source: &RecallSource,
) -> String {
    let (supporting, score) = live_support(standing, ctx);
    let corroboration = if supporting == 0 {
        "no corroborating evidence in range".to_string()
    } else {
        format!("supported by {supporting} link(s) (corroboration {score:.1}/10)")
    };
    match standing.created_at {
        Some(created) => {
            let window_days = suggested_ttl_days(&standing.chunk_type);
            let age_days = (ctx.now_unix - created.unix_timestamp()).max(0) / 86_400;
            let origin = match source {
                RecallSource::IndexedFile => standing
                    .file_path
                    .as_deref()
                    .map_or_else(|| "indexed".to_string(), |path| format!("indexed from {path}")),
                RecallSource::Derived => "derived".to_string(),
                _ => "written".to_string(),
            };
            format!(
                "{origin} {age_days}d ago, in {window_days}-day window; {corroboration}; \
                 no executed observation"
            )
        }
        None => {
            let corroboration = corroboration.to_lowercase();
            format!("no substrate establishes status ({corroboration})")
        }
    }
}

/// Live corroboration: the count of links with a positive decayed
/// contribution plus the shared [`chunk_link_score`] (corroboration, not
/// ranking — the same score the cascade reorder cites).
fn live_support(standing: &ChunkStanding, ctx: &DerivationContext) -> (usize, f64) {
    let supporting = standing
        .incoming
        .iter()
        .filter(|timed| {
            let age_days = (ctx.now_unix - timed.created_unix) as f64 / 86_400.0;
            kind_weight(timed.link.kind)
                * timed.link.weight.clamp(0.0, 1.0)
                * decay_factor(age_days, ctx.decay_days)
                > 0.0
        })
        .count();
    let score = chunk_link_score(&standing.incoming, ctx.decay_days, ctx.now_unix);
    (supporting, score)
}

#[cfg(test)]
mod tests {
    use super::*;
    use concerto_core::memory::MemoryLink;

    const NOW_UNIX: i64 = 1_784_000_000;

    fn ctx() -> DerivationContext {
        DerivationContext { decay_days: Some(90), now_unix: NOW_UNIX, current_generation: None }
    }

    fn standing() -> ChunkStanding {
        ChunkStanding {
            chunk_type: ChunkType::Function,
            file_path: Some("src/lib.rs".to_string()),
            stale_model: false,
            created_at: Some(
                OffsetDateTime::from_unix_timestamp(NOW_UNIX - 2 * 86_400).expect("fixed time"),
            ),
            metadata: None,
            incoming: Vec::new(),
            cited_decision: None,
            path_observation: None,
        }
    }

    fn link(from: &str, kind: MemoryLinkKind, age_days: i64) -> TimedLink {
        TimedLink {
            link: MemoryLink::new(from, "chunk", kind),
            created_unix: NOW_UNIX - age_days * 86_400,
        }
    }

    #[test]
    fn ttl_expiry_stales_with_window_and_age() {
        let mut standing = standing();
        standing.created_at =
            Some(OffsetDateTime::from_unix_timestamp(NOW_UNIX - 200 * 86_400).expect("fixed time"));
        let verdict = derive_recall_verdict(&standing, &ctx());
        assert_eq!(verdict.truth, FactStatusView::Stale);
        assert!(verdict.derivation.contains("90-day"), "names the window: {}", verdict.derivation);
        assert!(verdict.derivation.contains("200d"), "names the age: {}", verdict.derivation);
    }

    #[test]
    fn live_contradiction_contradicts_and_cites() {
        let mut standing = standing();
        standing.incoming.push(link("critic", MemoryLinkKind::Contradicts, 4));
        let verdict = derive_recall_verdict(&standing, &ctx());
        assert_eq!(verdict.truth, FactStatusView::Contradicted);
        assert!(verdict.derivation.contains("critic"), "cites the source: {}", verdict.derivation);
    }

    #[test]
    fn decayed_contradiction_does_not_contradict() {
        let mut standing = standing();
        // Beyond the 90-day window the contradiction is dead evidence, and
        // with nothing else live the item reads OutsideWindow.
        standing.incoming.push(link("critic", MemoryLinkKind::Contradicts, 120));
        let verdict = derive_recall_verdict(&standing, &ctx());
        assert_eq!(verdict.truth, FactStatusView::OutsideWindow);
    }

    #[test]
    fn supersession_link_supersedes() {
        let mut standing = standing();
        standing.incoming.push(link("v2", MemoryLinkKind::Supersedes, 4));
        let verdict = derive_recall_verdict(&standing, &ctx());
        assert_eq!(verdict.truth, FactStatusView::Superseded);
        assert!(verdict.derivation.contains("v2"));
    }

    #[test]
    fn cited_superseded_decision_supersedes() {
        let mut standing = standing();
        standing.cited_decision =
            Some(CitedDecision { id: "decision-1".to_string(), superseded: true });
        let verdict = derive_recall_verdict(&standing, &ctx());
        assert_eq!(verdict.truth, FactStatusView::Superseded);
        assert!(verdict.derivation.contains("decision-1"), "cites it: {}", verdict.derivation);

        // A live (non-superseded) cite changes nothing: the item falls
        // through to the unknown arm with its age stated.
        standing.cited_decision =
            Some(CitedDecision { id: "decision-1".to_string(), superseded: false });
        let live = derive_recall_verdict(&standing, &ctx());
        assert_eq!(live.truth, FactStatusView::Unknown);
    }

    #[test]
    fn stale_model_stales_without_invalidating_content() {
        let mut standing = standing();
        standing.stale_model = true;
        let verdict = derive_recall_verdict(&standing, &ctx());
        assert_eq!(verdict.truth, FactStatusView::Stale);
        assert!(verdict.stale_model);
        assert!(
            verdict.derivation.contains("may still be right"),
            "honest: {}",
            verdict.derivation
        );
    }

    #[test]
    fn dirty_observation_stales() {
        let mut standing = standing();
        standing.path_observation = Some(PathObservation {
            dirty: true,
            generation: Some("g1".to_string()),
            last_event_id: Some("ev-9".to_string()),
        });
        let verdict = derive_recall_verdict(&standing, &ctx());
        assert_eq!(verdict.truth, FactStatusView::Stale);
        assert!(verdict.derivation.contains("ev-9"));
    }

    #[test]
    fn clean_matching_observation_verifies() {
        let mut standing = standing();
        standing.path_observation = Some(PathObservation {
            dirty: false,
            generation: Some("g1".to_string()),
            last_event_id: Some("ev-3".to_string()),
        });
        let verdict = derive_recall_verdict(&standing, &ctx());
        assert_eq!(verdict.truth, FactStatusView::Verified);
        assert!(verdict.derivation.contains("ev-3"));
    }

    #[test]
    fn generation_mismatch_stales() {
        let mut standing = standing();
        standing.path_observation = Some(PathObservation {
            dirty: false,
            generation: Some("g1".to_string()),
            last_event_id: Some("ev-3".to_string()),
        });
        let mut ctx = ctx();
        ctx.current_generation = Some("g2".to_string());
        let verdict = derive_recall_verdict(&standing, &ctx);
        assert_eq!(verdict.truth, FactStatusView::Stale);
        assert!(verdict.derivation.contains("g1") && verdict.derivation.contains("g2"));
    }

    #[test]
    fn model_authored_stays_a_claim_with_support_stated() {
        let mut standing = standing();
        standing.chunk_type = ChunkType::Fact;
        standing.file_path = Some(STORED_ENTRY_PATH.to_string());
        let verdict = derive_recall_verdict(&standing, &ctx());
        assert_eq!(verdict.truth, FactStatusView::Assumed);
        assert_eq!(verdict.source, RecallSource::ModelAuthored);
        assert!(verdict.derivation.contains("no supporting evidence"), "{}", verdict.derivation);

        standing.incoming.push(link("supporter", MemoryLinkKind::Supports, 1));
        let supported = derive_recall_verdict(&standing, &ctx());
        assert_eq!(supported.truth, FactStatusView::Assumed, "support never verifies");
        assert!(supported.derivation.contains("never verifies"), "{}", supported.derivation);
    }

    #[test]
    fn corroborated_indexed_chunk_is_unknown_truth_with_fresh_derivation() {
        let mut standing = standing();
        standing.incoming.push(link("supporter", MemoryLinkKind::Supports, 1));
        let verdict = derive_recall_verdict(&standing, &ctx());
        // No executed observation: truth stays unestablished, but the
        // derivation says exactly what is known (age, corroboration).
        assert_eq!(verdict.truth, FactStatusView::Unknown);
        assert!(!verdict.stale_model);
        assert!(verdict.derivation.contains("2d ago"), "cites age: {}", verdict.derivation);
        assert!(
            verdict.derivation.contains("supported by 1"),
            "cites support: {}",
            verdict.derivation
        );
        assert!(verdict.derivation.contains("no executed observation"));
    }

    #[test]
    fn bare_chunk_with_no_substrate_is_unknown() {
        let standing = ChunkStanding {
            chunk_type: ChunkType::Function,
            file_path: None,
            stale_model: false,
            created_at: None,
            metadata: None,
            incoming: Vec::new(),
            cited_decision: None,
            path_observation: None,
        };
        let verdict = derive_recall_verdict(&standing, &ctx());
        assert_eq!(verdict.truth, FactStatusView::Unknown);
        assert_eq!(verdict.source, RecallSource::Unknown);
        assert!(verdict.derivation.contains("no substrate establishes status"));
    }

    #[test]
    fn derivation_strings_stay_bounded() {
        let mut standing = standing();
        standing.file_path = Some("x".repeat(500));
        standing.incoming.push(link(&"y".repeat(500), MemoryLinkKind::Contradicts, 1));
        let verdict = derive_recall_verdict(&standing, &ctx());
        assert!(verdict.derivation.chars().count() <= MAX_DERIVATION_CHARS);
    }

    #[test]
    fn contradiction_outranks_model_staleness() {
        let mut standing = standing();
        standing.stale_model = true;
        standing.incoming.push(link("critic", MemoryLinkKind::Contradicts, 1));
        let verdict = derive_recall_verdict(&standing, &ctx());
        assert_eq!(verdict.truth, FactStatusView::Contradicted, "precedence is positional");
    }
}
