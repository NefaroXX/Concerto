//! RAG pipeline — hybrid retrieval over code chunks.
//!
//! `HybridRetriever` combines BM25 full-text search with vector
//! similarity search using reciprocal-rank fusion (RRF) scoring.

use async_trait::async_trait;
use concerto_core::CancellationToken;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use concerto_core::error::MemoryError;
use concerto_core::memory::{FtsResult, MemoryChunk, MemoryQuery, ProjectId, VectorResult};

use crate::embedder_health::{EmbedderHealth, EMBEDDER_DEGRADED_NOTICE};
use crate::fts::FullTextStore;
use crate::scoring::LINK_SCORE_MAX;
use crate::vector_store::VectorStore;

/// RRF fusion constant (prevents division by zero).
const RRF_K: f64 = 60.0;

/// Candidate-pool tier of the ADR-69 link cascade: how much extra candidate
/// headroom to fetch/fuse, and how strongly link evidence may re-rank the
/// fused results.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CascadeTier {
    /// 1× headroom; link evidence may move a result by γ = 0.05 (≤ 0.5 rank
    /// slots at a 10-point scale).
    #[default]
    Mild,
    /// 2× headroom, γ = 0.10.
    Aggressive,
    /// 4× headroom, γ = 0.20 — the ADR-69 A6 cap. Never exceeded.
    Emergency,
}

impl CascadeTier {
    /// Candidate multiplyer applied over the healthy path's `k * 2` headroom.
    pub const fn candidate_multiplier(self) -> usize {
        match self {
            Self::Mild => 1,
            Self::Aggressive => 2,
            Self::Emergency => 4,
        }
    }

    /// Link-evidence re-rank strength (capped at 0.2 by ADR-69 A6).
    pub const fn gamma(self) -> f64 {
        match self {
            Self::Mild => 0.05,
            Self::Aggressive => 0.10,
            Self::Emergency => 0.20,
        }
    }

    /// The next-stronger tier, hunting for more candidates when the current
    /// fused set cannot fill `top_k`. `Emergency` is the ceiling.
    pub const fn escalate(self) -> Self {
        match self {
            Self::Mild => Self::Aggressive,
            Self::Aggressive => Self::Emergency,
            Self::Emergency => Self::Emergency,
        }
    }
}

/// Tuning for the ADR-69 slice-2 link cascade.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LinkCascadeConfig {
    /// Evidence decay window in days. `Some(0)` disables decay, as does
    /// `None`. Defaults to the ADR-69 A5 floor via [`Default`]; the runtime
    /// resolves an unconfigured `[memory]` window to the same floor.
    pub decay_days: Option<u16>,
    /// Tier the cascade starts at. Defaults to [`CascadeTier::Mild`].
    pub start_tier: CascadeTier,
    /// Hard budget for one evidence-scoring pass. Defaults to 250 ms.
    pub score_timeout: Duration,
}

impl Default for LinkCascadeConfig {
    fn default() -> Self {
        Self {
            decay_days: Some(crate::scoring::DECAY_FLOOR_DAYS as u16),
            start_tier: CascadeTier::Mild,
            score_timeout: Duration::from_millis(250),
        }
    }
}

/// Scores chunk ids by the strength of their symbolic-link evidence
/// (ADR-69 slice 2). Implemented for the production link store by
/// [`crate::system::StoreLinkScorer`]; tests script their own.
#[async_trait]
pub trait LinkScorer: Send + Sync {
    /// Map chunk ids to 0–10 evidence scores. Chunks absent from the map
    /// score 0 (no supporting evidence). Fail-open: an `Err` signals the
    /// cascade to skip the reorder, never to fail the query.
    async fn link_scores(
        &self,
        chunk_ids: &[String],
        cancel: CancellationToken,
    ) -> Result<HashMap<String, f64>, MemoryError>;
}

/// A fused result entry after RRF scoring.
#[derive(Debug, Clone)]
pub struct FusedResult {
    pub chunk_id: String,
    pub score: f64,
    pub vector_score: f64,
    pub fts_score: f64,
    pub content: String,
    /// Set when the embedder is broken for the project (ADR-39): the result
    /// is FTS-only and carries an explicit notice to the caller.
    pub notice: Option<std::sync::Arc<str>>,
    /// `true` when the backing row is stale (older embedding model version).
    /// Stale results are retained but rank-demoted behind fresh ones (ADR-12).
    pub stale: bool,
}

/// Hybrid retriever combining BM25 and vector search.
pub struct HybridRetriever {
    vector_store: Arc<dyn VectorStore>,
    fts_store: Arc<dyn FullTextStore>,
}

impl HybridRetriever {
    pub fn new(vector_store: Arc<dyn VectorStore>, fts_store: Arc<dyn FullTextStore>) -> Self {
        Self { vector_store, fts_store }
    }

    pub async fn browse(
        &self,
        project_id: &concerto_core::memory::ProjectId,
        top_k: usize,
        cancel: CancellationToken,
    ) -> Vec<VectorResult> {
        self.vector_store.list(project_id, top_k, cancel).await.unwrap_or_default()
    }

    pub async fn load_chunks(
        &self,
        project_id: &ProjectId,
        chunk_ids: &[String],
        cancel: CancellationToken,
    ) -> Result<Vec<MemoryChunk>, MemoryError> {
        self.vector_store.get_chunks(project_id, chunk_ids, cancel).await
    }

    pub fn supports_chunk_metadata(&self) -> bool {
        self.vector_store.supports_chunk_metadata()
    }

    /// Recall chunks similar to a not-yet-stored entry WITHOUT rank fusion.
    ///
    /// This is the candidate recall for the L1 dedup pass
    /// ([`crate::system::MemorySystem::store`]): the LLM judge needs the top
    /// candidate *contents* (and ids) to decide store / update / merge /
    /// skip, so it compares semantics — fused rank scores would be
    /// misleading here. Consequently no RRF applies and `RRF_K` is untouched:
    /// vector hits come first (in store order), FTS-only hits are appended
    /// (in BM25 order), duplicates across the two sides are dropped keeping
    /// the vector hit.
    pub async fn recall_candidates(
        &self,
        project_id: &ProjectId,
        query_text: &str,
        embedding: &[f32],
        vector_top_k: usize,
        fts_top_k: usize,
        cancel: CancellationToken,
    ) -> Vec<FusedResult> {
        let (vector_results, fts_results) = tokio::join!(
            self.vector_store.search(project_id, embedding, vector_top_k, cancel.clone()),
            self.fts_store.search(query_text, project_id, fts_top_k, cancel),
        );
        let vector_results = vector_results.unwrap_or_default();
        let fts_results = fts_results.unwrap_or_default();

        let mut results: Vec<FusedResult> =
            Vec::with_capacity(vector_results.len() + fts_results.len());
        let mut seen_chunk_ids: HashSet<String> = HashSet::with_capacity(results.capacity());
        for vector_result in vector_results {
            seen_chunk_ids.insert(vector_result.chunk_id.clone());
            results.push(FusedResult {
                chunk_id: vector_result.chunk_id,
                score: vector_result.score,
                vector_score: vector_result.score,
                fts_score: 0.0,
                content: vector_result.content,
                notice: None,
                stale: vector_result.stale,
            });
        }
        for fts_result in fts_results {
            if seen_chunk_ids.insert(fts_result.chunk_id.clone()) {
                results.push(FusedResult {
                    chunk_id: fts_result.chunk_id,
                    score: fts_result.score,
                    vector_score: 0.0,
                    fts_score: fts_result.score,
                    content: fts_result.content,
                    notice: None,
                    stale: fts_result.stale,
                });
            }
        }
        results
    }

    /// Retrieve context for a memory query using hybrid search.
    ///
    /// 1. Vector search on `embedding`
    /// 2. BM25 FT search on `query.text`
    /// 3. Reciprocal-rank fusion of both result sets
    /// 4. Return top-k by fused score
    ///
    /// `top_k` overrides `query.top_k` if supplied.
    pub async fn retrieve(
        &self,
        query: &MemoryQuery,
        embedding: &[f32],
        top_k: Option<usize>,
        cancel: CancellationToken,
    ) -> Vec<FusedResult> {
        let k = top_k.unwrap_or(query.top_k).max(1);

        // When the project's embedder is broken (ADR-39) only the hybrid
        // retriever (which owns both stores) can produce the FTS-only
        // fallback, so the degraded branch lives here.
        if EmbedderHealth::for_project(&query.project_id).is_broken(std::time::Instant::now()) {
            return self.degraded_fts_results(query, k, cancel).await;
        }
        self.fetch_fused(query, embedding, k, 1, cancel).await
    }

    /// Retrieve like [`Self::retrieve`] but re-rank the fused top-k with
    /// ADR-69 slice-2 link evidence — "the cascade".
    ///
    /// Semantics:
    /// 1. Fetch/fuse at `start_tier`'s candidate headroom.
    /// 2. Escalate to the next tier and refetch while the fused set cannot
    ///    fill `top_k` — a short set means the plain fetch truncated early,
    ///    so a wider candidate pool may surface linked chunks. `Emergency`
    ///    is the ceiling; the returned set is never grown beyond `top_k`.
    /// 3. Score the fused chunk ids through `scorer` under `score_timeout`.
    /// 4. Re-sort non-destructively by `rank + γ·(10 − score)` — see
    ///    [`cascade_key`]. The reorder never drops or adds a result; it only
    ///    adjusts order within `top_k`.
    ///
    /// Fail-open by design (ADR-69): any scorer error, timeout, or
    /// cancellation leaves the plain fused order untouched.
    #[allow(clippy::too_many_arguments)]
    pub async fn retrieve_with_cascade(
        &self,
        query: &MemoryQuery,
        embedding: &[f32],
        top_k: Option<usize>,
        start_tier: CascadeTier,
        scorer: &dyn LinkScorer,
        config: &LinkCascadeConfig,
        cancel: CancellationToken,
    ) -> Vec<FusedResult> {
        let k = top_k.unwrap_or(query.top_k).max(1);

        if EmbedderHealth::for_project(&query.project_id).is_broken(std::time::Instant::now()) {
            return self.degraded_fts_results(query, k, cancel).await;
        }

        let mut tier = start_tier;
        let mut fused = self
            .fetch_fused(query, embedding, k, tier.candidate_multiplier(), cancel.clone())
            .await;
        while fused.len() < k && tier != CascadeTier::Emergency {
            tier = tier.escalate();
            fused = self
                .fetch_fused(query, embedding, k, tier.candidate_multiplier(), cancel.clone())
                .await;
        }

        let ids: Vec<String> = fused.iter().map(|result| result.chunk_id.clone()).collect();
        let scores = self.score_links(scorer, &ids, config.score_timeout, cancel).await;
        if scores.is_empty() {
            return fused;
        }

        let gamma = tier.gamma();
        let mut ranked: Vec<(usize, FusedResult)> = fused.into_iter().enumerate().collect();
        ranked.sort_by(|(left_rank, left), (right_rank, right)| {
            left.stale.cmp(&right.stale).then_with(|| {
                cascade_key(*left_rank, left, &scores, gamma)
                    .partial_cmp(&cascade_key(*right_rank, right, &scores, gamma))
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
        });
        ranked.into_iter().map(|(_, result)| result).collect()
    }

    /// FTS-only fallback for a project whose embedder is broken (ADR-39).
    /// Mirrors the healthy path's doubled candidate headroom so post-order
    /// truncation never starves `top_k` (N3); every result carries the
    /// degraded notice and no vector score.
    async fn degraded_fts_results(
        &self,
        query: &MemoryQuery,
        k: usize,
        cancel: CancellationToken,
    ) -> Vec<FusedResult> {
        let fetch_k = k * 2;
        let fts_results = self
            .fts_store
            .search(&query.text, &query.project_id, fetch_k, cancel)
            .await
            .unwrap_or_default();
        let mut degraded: Vec<FusedResult> = fts_results
            .into_iter()
            .enumerate()
            .map(|(rank, fr)| FusedResult {
                chunk_id: fr.chunk_id,
                score: 1.0 / (k as f64 + rank as f64),
                vector_score: 0.0,
                fts_score: 1.0 / (k as f64 + rank as f64),
                content: fr.content,
                notice: Some(EMBEDDER_DEGRADED_NOTICE.into()),
                stale: false,
            })
            .collect();
        degraded.truncate(k);
        degraded
    }

    /// Fuse both stores at `k·2·multiplier` candidate headroom (N3: keeping
    /// headroom above `top_k` so fusion/truncation cannot over-truncate),
    /// demote stale rows behind fresh ones (ADR-12 two-tier rule), and
    /// truncate to `k`.
    async fn fetch_fused(
        &self,
        query: &MemoryQuery,
        embedding: &[f32],
        k: usize,
        multiplier: usize,
        cancel: CancellationToken,
    ) -> Vec<FusedResult> {
        let fetch_k = k * 2 * multiplier;

        let (vector_results, fts_results) = tokio::join!(
            self.vector_store.search(&query.project_id, embedding, fetch_k, cancel.clone()),
            self.fts_store.search(&query.text, &query.project_id, fetch_k, cancel),
        );

        let mut fused = fuse_results(
            &vector_results.unwrap_or_default(),
            &fts_results.unwrap_or_default(),
            RRF_K,
        );
        fused.sort_by(|a, b| {
            a.stale
                .cmp(&b.stale)
                .then_with(|| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal))
        });
        fused.truncate(k);
        fused
    }

    /// One bounded, fail-open evidence-scoring pass. Any error, timeout, or
    /// cancellation yields an empty map → the cascade keeps the plain order.
    async fn score_links(
        &self,
        scorer: &dyn LinkScorer,
        ids: &[String],
        budget: Duration,
        cancel: CancellationToken,
    ) -> HashMap<String, f64> {
        if ids.is_empty() {
            return HashMap::new();
        }
        match tokio::time::timeout(budget, scorer.link_scores(ids, cancel)).await {
            Ok(Ok(scores)) => scores,
            Ok(Err(error)) => {
                tracing::warn!(%error, "link scoring failed; cascade disabled for this query");
                HashMap::new()
            }
            Err(_elapsed) => {
                tracing::warn!(
                    timeout_ms = budget.as_millis(),
                    "link scoring timed out; cascade disabled for this query"
                );
                HashMap::new()
            }
        }
    }
}

/// ADR-69 A4/A6 ordering key for the cascade reorder: `rank + γ·(10 − score)`.
///
/// γ caps the movement at `0.2·10 = 2` rank slots, so link evidence augments
/// RRF but never dominates it. Chunks without a score are treated as 0
/// (replaceable / unsupported). Lower key = earlier.
fn cascade_key(
    rank: usize,
    result: &FusedResult,
    scores: &HashMap<String, f64>,
    gamma: f64,
) -> f64 {
    let link_score =
        scores.get(&result.chunk_id).copied().unwrap_or(0.0).clamp(0.0, LINK_SCORE_MAX);
    rank as f64 + gamma * (LINK_SCORE_MAX - link_score)
}

/// Fuse two result sets using reciprocal-rank fusion.
fn fuse_results(
    vector_results: &[VectorResult],
    fts_results: &[FtsResult],
    k: f64,
) -> Vec<FusedResult> {
    use std::collections::HashMap;

    // Map chunk_id -> (vector_score, fts_score)
    let mut scores: HashMap<String, (f64, f64)> = HashMap::new();
    // Map chunk_id -> content from vector results (if any)
    let mut content_map: HashMap<String, String> = HashMap::new();
    // Map chunk_id -> staleness, carried from the vector store (authoritative
    // for model-version staleness; the FTS table has no staleness column).
    let mut stale_map: HashMap<String, bool> = HashMap::new();

    for (rank, vr) in vector_results.iter().enumerate() {
        let entry = scores.entry(vr.chunk_id.clone()).or_insert((0.0, 0.0));
        entry.0 = 1.0 / (k + (rank as f64));
        // Store content for later use
        content_map.insert(vr.chunk_id.clone(), vr.content.clone());
        stale_map.insert(vr.chunk_id.clone(), vr.stale);
    }

    for (rank, fr) in fts_results.iter().enumerate() {
        let entry = scores.entry(fr.chunk_id.clone()).or_insert((0.0, 0.0));
        entry.1 = 1.0 / (k + (rank as f64));

        content_map.insert(fr.chunk_id.clone(), fr.content.clone());
    }

    let mut results: Vec<FusedResult> = scores
        .into_iter()
        .map(|(chunk_id, (v_score, f_score))| FusedResult {
            chunk_id: chunk_id.clone(),
            score: v_score + f_score,
            vector_score: v_score,
            fts_score: f_score,
            content: content_map.get(&chunk_id).cloned().unwrap_or_default(),
            notice: None,
            stale: stale_map.get(&chunk_id).copied().unwrap_or(false),
        })
        .collect();
    // `fuse_results` ranks purely by fused score (RRF ordinal purity is
    // preserved — the two-tier demotion happens in `retrieve`); this sort is
    // only a stable order for callers that rely on score ordering.
    results.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    results
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_vector_result(chunk_id: &str, score: f64, content: &str) -> VectorResult {
        VectorResult { chunk_id: chunk_id.into(), score, content: content.into(), stale: false }
    }

    fn make_fts_result(chunk_id: &str, score: f64, content: &str) -> FtsResult {
        FtsResult { chunk_id: chunk_id.into(), score, content: content.into(), stale: false }
    }

    fn make_vector_result_stale(chunk_id: &str, score: f64, content: &str) -> VectorResult {
        VectorResult { chunk_id: chunk_id.into(), score, content: content.into(), stale: true }
    }

    #[test]
    fn rrf_prefers_common_results() {
        let vector = vec![make_vector_result("a", 0.9, ""), make_vector_result("b", 0.8, "")];
        let fts = vec![make_fts_result("a", 0.85, ""), make_fts_result("c", 0.7, "")];

        let result = fuse_results(&vector, &fts, RRF_K);
        assert_eq!(result.len(), 3);

        // 'a' appears in both → higher score than 'b' or 'c' alone
        let a = result.iter().find(|r| r.chunk_id == "a").unwrap();
        let b = result.iter().find(|r| r.chunk_id == "b").unwrap();
        let c = result.iter().find(|r| r.chunk_id == "c").unwrap();

        assert!(a.score > b.score);
        assert!(a.score > c.score);
    }

    #[test]
    fn rrf_empty_inputs() {
        let result = fuse_results(&[], &[], RRF_K);
        assert!(result.is_empty());
    }

    #[test]
    fn fts_only_match_has_content() {
        let vector = vec![];
        let fts = vec![make_fts_result("x", 0.9, "some content")];
        let result = fuse_results(&vector, &fts, RRF_K);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].content, "some content");
    }

    #[test]
    fn vector_only_results() {
        let vector = vec![make_vector_result("a", 0.9, "vector content")];
        let fts = vec![];
        let result = fuse_results(&vector, &fts, RRF_K);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].chunk_id, "a");
        assert!(result[0].vector_score > 0.0);
        assert_eq!(result[0].fts_score, 0.0);
        assert_eq!(result[0].content, "vector content");
    }

    #[test]
    fn fts_only_results() {
        let vector = vec![];
        let fts = vec![make_fts_result("b", 0.8, "fts content")];
        let result = fuse_results(&vector, &fts, RRF_K);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].chunk_id, "b");
        assert_eq!(result[0].vector_score, 0.0);
        assert!(result[0].fts_score > 0.0);
        assert_eq!(result[0].content, "fts content");
    }

    #[test]
    fn fuse_results_propagates_fts_content_when_both_match() {
        let vector = vec![make_vector_result("a", 0.9, "vector content")];
        let fts = vec![make_fts_result("a", 0.85, "fts content")];
        let result = fuse_results(&vector, &fts, RRF_K);
        assert_eq!(result.len(), 1);
        // FTS content wins because it's inserted last into content_map
        assert_eq!(result[0].content, "fts content");
    }

    /// ADR-12: `fuse_results` carries the vector store's staleness flag
    /// through to the fused result (the FTS table has no staleness column,
    /// so the vector store is authoritative for model-version staleness).
    #[test]
    fn fuse_results_propagates_stale_from_vector_result() {
        let vector = vec![make_vector_result_stale("a", 0.9, "stale content")];
        let fts = vec![make_fts_result("a", 0.8, "stale content")];
        let result = fuse_results(&vector, &fts, RRF_K);
        assert_eq!(result.len(), 1);
        assert!(result[0].stale, "staleness must survive fusion");
    }

    #[test]
    fn rrf_rank_affects_score() {
        let vector = vec![
            make_vector_result("first", 0.9, ""),
            make_vector_result("second", 0.8, ""),
            make_vector_result("third", 0.7, ""),
        ];
        let fts = vec![];
        let result = fuse_results(&vector, &fts, RRF_K);

        // Higher rank = lower index = higher score
        let first = result.iter().find(|r| r.chunk_id == "first").unwrap();
        let second = result.iter().find(|r| r.chunk_id == "second").unwrap();
        let third = result.iter().find(|r| r.chunk_id == "third").unwrap();

        assert!(first.vector_score > second.vector_score);
        assert!(second.vector_score > third.vector_score);
    }

    #[test]
    fn large_k_flattens_score_differences() {
        let vector = vec![make_vector_result("a", 0.9, ""), make_vector_result("b", 0.8, "")];
        let fts = vec![];

        let normal = fuse_results(&vector, &fts, 1.0); // K = 1
        let flat = fuse_results(&vector, &fts, 1000.0); // K = 1000

        let _a_normal = normal.iter().find(|r| r.chunk_id == "a").unwrap();
        let a_flat = flat.iter().find(|r| r.chunk_id == "a").unwrap();
        let b_flat = flat.iter().find(|r| r.chunk_id == "b").unwrap();

        // With large K, scores are more similar
        let diff_flat = (a_flat.score - b_flat.score).abs();
        assert!(diff_flat < 0.01, "large K should make scores nearly equal, diff={diff_flat}");
    }

    /// RRF with empty inputs returns empty results.
    #[test]
    fn rrf_with_empty_inputs_returns_empty() {
        let result = fuse_results(&[], &[], RRF_K);
        assert!(result.is_empty());
    }

    /// RRF with only vector results returns results ranked by vector score.
    #[test]
    fn rrf_with_only_vector_results() {
        let vector = vec![make_vector_result("a", 0.9, ""), make_vector_result("b", 0.5, "")];
        let result = fuse_results(&vector, &[], RRF_K);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].chunk_id, "a", "higher-scored item should be first");
        assert_eq!(result[1].chunk_id, "b", "lower-scored item should be second");
    }

    /// Degraded search: when the project's embedder is broken, `retrieve`
    /// returns FTS-only results carrying the notice; recovery clears it
    /// (ADR-39).
    #[tokio::test]
    async fn degraded_search_returns_fts_only_with_notice_and_recovers() {
        use concerto_core::memory::{ChunkType, MemoryChunk, MemoryNamespace};

        let vs = Arc::new(crate::testing::InMemoryVectorStore::new());
        let fts = Arc::new(crate::testing::InMemoryFullTextStore::new());
        let pid = ProjectId("degraded-proj".into());
        let chunk = MemoryChunk {
            id: "c1".into(),
            project_id: pid.clone(),
            namespace: MemoryNamespace::Project(pid.clone()),
            content: "unique hello world marker".into(),
            file_path: Some(camino::Utf8PathBuf::from("a.rs")),
            start_line: Some(1),
            end_line: Some(1),
            chunk_type: ChunkType::Function,
            score: 1.0,
            model_id: "m".into(),
            model_version: "1".into(),
            stale: false,
        };
        fts.insert(&chunk, &pid, CancellationToken::new()).await.unwrap();
        let retriever = HybridRetriever::new(vs, fts);
        let query = MemoryQuery {
            text: "world marker".into(),
            project_id: pid.clone(),
            namespace: MemoryNamespace::Project(pid.clone()),
            top_k: 5,
            filters: vec![],
        };

        // Broken: FTS-only results with a notice and no vector component.
        let health = crate::embedder_health::EmbedderHealth::for_project(&pid);
        health.record_failure(std::time::Instant::now());
        let broken = retriever.retrieve(&query, &[0.1; 8], Some(5), CancellationToken::new()).await;
        assert!(!broken.is_empty(), "FTS fallback must still return matching chunks");
        assert!(
            broken.iter().all(|r| r.notice.is_some() && r.vector_score == 0.0),
            "every degraded result must carry the notice and no vector score"
        );

        // Recovery: success clears the broken state → notice is None again.
        health.record_success();
        let recovered =
            retriever.retrieve(&query, &[0.1; 8], Some(5), CancellationToken::new()).await;
        assert!(recovered.iter().all(|r| r.notice.is_none()), "notice cleared on recovery");
    }

    /// ADR-12 two-tier demotion: in `retrieve`, a stale (old embedding model)
    /// row ranks behind a fresh row even when the stale row has the higher
    /// fused RRF score. Fresh-first ordering is a hard rank rule, not a score
    /// tie-break (no score scaling — RRF ordinal purity is preserved).
    #[tokio::test]
    async fn retrieve_ranks_fresh_before_stale_regardless_of_score() {
        use concerto_core::memory::{ChunkType, EmbeddingRecord, MemoryNamespace};

        let pid = ProjectId("adam12-proj".into());
        // InMemoryVectorStore::search sorts by chunk_id, so "a_stale" is
        // vector rank 0 (highest RRF fused score) and "z_fresh" rank 1 —
        // without demotion the stale row would come first.
        let stale_record = EmbeddingRecord {
            id: "a_stale".into(),
            project_id: pid.clone(),
            chunk_hash: "h0".into(),
            content: "stale content".into(),
            file_path: "a.rs".into(),
            start_line: Some(1),
            end_line: Some(2),
            chunk_type: ChunkType::Function,
            vector: vec![0.1, 0.2, 0.3],
            model_id: "old-model".into(),
            model_version: "1.0".into(),
            stale: true,
            created_at: time::OffsetDateTime::now_utc(),
        };
        let fresh_record = EmbeddingRecord {
            id: "z_fresh".into(),
            project_id: pid.clone(),
            chunk_hash: "h1".into(),
            content: "fresh content".into(),
            file_path: "b.rs".into(),
            start_line: Some(1),
            end_line: Some(2),
            chunk_type: ChunkType::Function,
            vector: vec![0.4, 0.5, 0.6],
            model_id: "new-model".into(),
            model_version: "2.0".into(),
            stale: false,
            created_at: time::OffsetDateTime::now_utc(),
        };

        let vs = Arc::new(crate::testing::InMemoryVectorStore::with_records(vec![
            (pid.clone(), stale_record),
            (pid.clone(), fresh_record),
        ]));
        let fts = Arc::new(crate::testing::InMemoryFullTextStore::new());
        let retriever = HybridRetriever::new(vs, fts);
        let query = MemoryQuery {
            text: "irrelevant".into(),
            project_id: pid.clone(),
            namespace: MemoryNamespace::Project(pid.clone()),
            top_k: 2,
            filters: vec![],
        };

        let results =
            retriever.retrieve(&query, &[1.0; 8], Some(2), CancellationToken::new()).await;
        assert_eq!(results.len(), 2, "both rows are candidates at top_k=2");
        // Prove the premise: the stale row's fused score is higher, so only
        // demotion (not raw scoring) can put the fresh row first.
        let stale = results.iter().find(|r| r.chunk_id == "a_stale").unwrap();
        let fresh = results.iter().find(|r| r.chunk_id == "z_fresh").unwrap();
        assert!(
            stale.score > fresh.score,
            "stale row must outscore fresh row for a meaningful test"
        );
        assert_eq!(
            results[0].chunk_id, "z_fresh",
            "fresh row must outrank the higher-scoring stale row"
        );
        assert_eq!(results[1].chunk_id, "a_stale");
        assert!(!results[0].stale && results[1].stale);
    }

    // ------------------------------------------------------------------
    // ADR-69 slice 2 — link cascade (retrieve_with_cascade)
    // ------------------------------------------------------------------

    /// Delegates everything to an inner in-memory store but counts `search`
    /// calls, so tier escalation is observable.
    struct CountingVectorStore {
        inner: Arc<crate::testing::InMemoryVectorStore>,
        searches: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait]
    impl VectorStore for CountingVectorStore {
        async fn store(
            &self,
            records: &[concerto_core::memory::EmbeddingRecord],
            cancel: CancellationToken,
        ) -> Result<(), MemoryError> {
            self.inner.store(records, cancel).await
        }

        async fn search(
            &self,
            project_id: &ProjectId,
            query: &[f32],
            top_k: usize,
            cancel: CancellationToken,
        ) -> Result<Vec<VectorResult>, MemoryError> {
            self.searches.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.inner.search(project_id, query, top_k, cancel).await
        }

        async fn tombstone(
            &self,
            chunk_id: &str,
            project_id: &ProjectId,
            cancel: CancellationToken,
        ) -> Result<(), MemoryError> {
            self.inner.tombstone(chunk_id, project_id, cancel).await
        }

        async fn delete_tombstoned(
            &self,
            project_id: &ProjectId,
            cancel: CancellationToken,
        ) -> Result<(), MemoryError> {
            self.inner.delete_tombstoned(project_id, cancel).await
        }

        async fn mark_stale(
            &self,
            project_id: &ProjectId,
            model_version: &str,
            cancel: CancellationToken,
        ) -> Result<(), MemoryError> {
            self.inner.mark_stale(project_id, model_version, cancel).await
        }

        async fn delete_by_project(
            &self,
            project_id: &ProjectId,
            cancel: CancellationToken,
        ) -> Result<(), MemoryError> {
            self.inner.delete_by_project(project_id, cancel).await
        }

        async fn delete_by_file_path(
            &self,
            project_id: &ProjectId,
            file_path: &camino::Utf8PathBuf,
            cancel: CancellationToken,
        ) -> Result<Vec<String>, MemoryError> {
            self.inner.delete_by_file_path(project_id, file_path, cancel).await
        }
    }

    /// Fixed score map: anything not listed scores 0.
    struct ScriptedScorer(HashMap<String, f64>);

    #[async_trait]
    impl LinkScorer for ScriptedScorer {
        async fn link_scores(
            &self,
            chunk_ids: &[String],
            _cancel: CancellationToken,
        ) -> Result<HashMap<String, f64>, MemoryError> {
            Ok(chunk_ids
                .iter()
                .filter_map(|id| self.0.get(id).map(|score| (id.clone(), *score)))
                .collect())
        }
    }

    struct ErrorScorer;

    #[async_trait]
    impl LinkScorer for ErrorScorer {
        async fn link_scores(
            &self,
            _chunk_ids: &[String],
            _cancel: CancellationToken,
        ) -> Result<HashMap<String, f64>, MemoryError> {
            Err(MemoryError::RetrievalFailed("scorer down".into()))
        }
    }

    /// Scores after a delay longer than any test budget: exercises timeout
    /// fail-open under a real clock.
    struct SleepingScorer {
        scores: HashMap<String, f64>,
        delay: Duration,
    }

    #[async_trait]
    impl LinkScorer for SleepingScorer {
        async fn link_scores(
            &self,
            _chunk_ids: &[String],
            _cancel: CancellationToken,
        ) -> Result<HashMap<String, f64>, MemoryError> {
            tokio::time::sleep(self.delay).await;
            Ok(self.scores.clone())
        }
    }

    /// Two deterministic records: `InMemoryVectorStore::search` sorts by
    /// chunk_id, so "a_plain" is vector rank 0 and "b_linked" rank 1 in the
    /// plain fused order (`[a_plain, b_linked]`).
    fn cascade_seeded_retriever() -> (HybridRetriever, ProjectId) {
        use concerto_core::memory::{ChunkType, EmbeddingRecord};

        let pid = ProjectId("cascade-proj".into());
        let records = ["a_plain", "b_linked"]
            .iter()
            .map(|id| EmbeddingRecord {
                id: (*id).into(),
                project_id: pid.clone(),
                chunk_hash: format!("h-{id}"),
                content: format!("content of {id}"),
                file_path: format!("{id}.rs").into(),
                start_line: Some(1),
                end_line: Some(1),
                chunk_type: ChunkType::Function,
                vector: vec![0.1; 4],
                model_id: "m".into(),
                model_version: "1".into(),
                stale: false,
                created_at: time::OffsetDateTime::now_utc(),
            })
            .collect::<Vec<_>>();
        let vs = Arc::new(crate::testing::InMemoryVectorStore::with_records(
            records.into_iter().map(|r| (pid.clone(), r)).collect(),
        ));
        let fts = Arc::new(crate::testing::InMemoryFullTextStore::new());
        (HybridRetriever::new(vs, fts), pid)
    }

    fn cascade_query(pid: ProjectId, top_k: usize) -> MemoryQuery {
        use concerto_core::memory::MemoryNamespace;
        MemoryQuery {
            text: "zzz-no-fts-match".into(),
            project_id: pid.clone(),
            namespace: MemoryNamespace::Project(pid),
            top_k,
            filters: vec![],
        }
    }

    #[tokio::test]
    async fn cascade_reorders_linked_chunk_ahead_at_emergency_gamma() {
        let (retriever, pid) = cascade_seeded_retriever();
        let query = cascade_query(pid, 2);
        let scorer = ScriptedScorer(HashMap::from([
            ("a_plain".to_string(), 0.0),
            ("b_linked".to_string(), 10.0),
        ]));
        let config = LinkCascadeConfig {
            decay_days: Some(90),
            start_tier: CascadeTier::Emergency,
            score_timeout: Duration::from_millis(250),
        };

        let results = retriever
            .retrieve_with_cascade(
                &query,
                &[0.1; 4],
                Some(2),
                config.start_tier,
                &scorer,
                &config,
                CancellationToken::new(),
            )
            .await;

        assert_eq!(results.len(), 2);
        // keys: a_plain = 0 + 0.2·(10−0) = 2.0; b_linked = 1 + 0.2·(10−10) = 1.0
        assert_eq!(results[0].chunk_id, "b_linked", "linked, high-score chunk promotes");
        assert_eq!(results[1].chunk_id, "a_plain");
    }

    #[tokio::test]
    async fn cascade_without_evidence_keeps_rrf_order() {
        let (retriever, pid) = cascade_seeded_retriever();
        let query = cascade_query(pid, 2);
        // Empty score map → no reorder at all (scores never separate order
        // from plain RRF).
        let scorer = ScriptedScorer(HashMap::new());
        let config = LinkCascadeConfig {
            decay_days: Some(90),
            start_tier: CascadeTier::Emergency,
            score_timeout: Duration::from_millis(250),
        };

        let results = retriever
            .retrieve_with_cascade(
                &query,
                &[0.1; 4],
                Some(2),
                config.start_tier,
                &scorer,
                &config,
                CancellationToken::new(),
            )
            .await;

        assert_eq!(
            results.iter().map(|r| r.chunk_id.as_str()).collect::<Vec<_>>(),
            vec!["a_plain", "b_linked"]
        );
    }

    #[tokio::test]
    async fn cascade_escalates_search_tiers_when_set_cannot_fill_top_k() {
        use concerto_core::memory::{ChunkType, EmbeddingRecord, MemoryNamespace};

        let pid = ProjectId("cascade-escalate".into());
        let records = ["a_plain", "b_linked"]
            .iter()
            .map(|id| EmbeddingRecord {
                id: (*id).into(),
                project_id: pid.clone(),
                chunk_hash: format!("h-{id}"),
                content: format!("content of {id}"),
                file_path: format!("{id}.rs").into(),
                start_line: Some(1),
                end_line: Some(1),
                chunk_type: ChunkType::Function,
                vector: vec![0.1; 4],
                model_id: "m".into(),
                model_version: "1".into(),
                stale: false,
                created_at: time::OffsetDateTime::now_utc(),
            })
            .collect::<Vec<_>>();
        let inner = Arc::new(crate::testing::InMemoryVectorStore::with_records(
            records.into_iter().map(|r| (pid.clone(), r)).collect(),
        ));
        let searches = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let vs = Arc::new(CountingVectorStore { inner, searches: searches.clone() });
        let fts = Arc::new(crate::testing::InMemoryFullTextStore::new());
        let retriever = HybridRetriever::new(vs, fts);
        let query = MemoryQuery {
            text: "zzz-no-fts-match".into(),
            project_id: pid.clone(),
            namespace: MemoryNamespace::Project(pid),
            top_k: 5,
            filters: vec![],
        };
        let scorer = ScriptedScorer(HashMap::new());
        let config = LinkCascadeConfig {
            decay_days: Some(90),
            start_tier: CascadeTier::Mild,
            score_timeout: Duration::from_millis(250),
        };

        let results = retriever
            .retrieve_with_cascade(
                &query,
                &[0.1; 4],
                Some(5),
                config.start_tier,
                &scorer,
                &config,
                CancellationToken::new(),
            )
            .await;

        // 2 records can never fill top_k=5: Mild → Aggressive → Emergency.
        assert_eq!(results.len(), 2, "the returned set is never grown past what exists");
        assert_eq!(
            searches.load(std::sync::atomic::Ordering::Relaxed),
            3,
            "start + two escalations must each refetch"
        );
    }

    #[tokio::test]
    async fn cascade_fails_open_on_scorer_error_and_timeout() {
        let (retriever, pid) = cascade_seeded_retriever();
        let query = cascade_query(pid, 2);
        let config = LinkCascadeConfig {
            decay_days: Some(90),
            start_tier: CascadeTier::Emergency,
            score_timeout: Duration::from_millis(50),
        };

        // Scorer error → plain RRF order, query still succeeds.
        let errored = retriever
            .retrieve_with_cascade(
                &query,
                &[0.1; 4],
                Some(2),
                config.start_tier,
                &ErrorScorer,
                &config,
                CancellationToken::new(),
            )
            .await;
        assert_eq!(
            errored.iter().map(|r| r.chunk_id.as_str()).collect::<Vec<_>>(),
            vec!["a_plain", "b_linked"],
            "scorer error must not re-rank or fail the query"
        );

        // Scorer slower than the budget → timeout → plain RRF order.
        let sleeping = SleepingScorer {
            scores: HashMap::from([("b_linked".to_string(), 10.0)]),
            delay: Duration::from_millis(200),
        };
        let timed_out = retriever
            .retrieve_with_cascade(
                &query,
                &[0.1; 4],
                Some(2),
                config.start_tier,
                &sleeping,
                &config,
                CancellationToken::new(),
            )
            .await;
        assert_eq!(
            timed_out.iter().map(|r| r.chunk_id.as_str()).collect::<Vec<_>>(),
            vec!["a_plain", "b_linked"],
            "timeout must not re-rank"
        );
    }
}
