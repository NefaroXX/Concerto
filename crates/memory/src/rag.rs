//! RAG pipeline — hybrid retrieval over code chunks.
//!
//! `HybridRetriever` combines BM25 full-text search with vector
//! similarity search using reciprocal-rank fusion (RRF) scoring.

use concerto_core::CancellationToken;
use std::sync::Arc;

use concerto_core::error::MemoryError;
use concerto_core::memory::{FtsResult, MemoryChunk, MemoryQuery, ProjectId, VectorResult};

use crate::embedder_health::{EmbedderHealth, EMBEDDER_DEGRADED_NOTICE};
use crate::fts::FullTextStore;
use crate::vector_store::VectorStore;

/// RRF fusion constant (prevents division by zero).
const RRF_K: f64 = 60.0;

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
        let project_id = &query.project_id;
        let k = top_k.unwrap_or(query.top_k).max(1);

        // When the project's embedder is broken (ADR-39) we skip vector ranking
        // entirely and return FTS-only results carrying an explicit notice.
        // Only the hybrid retriever (which owns both stores) can produce a real
        // FTS fallback, so the degraded code path lives here rather than in the
        // vector store, which has no FTS access.
        if EmbedderHealth::for_project(project_id).is_broken(std::time::Instant::now()) {
            // Mirror the healthy path's headroom: fetch the doubled candidate
            // set so post-order filters (e.g. RRF/truncate) don't over-truncate
            // below the requested top_k (N3).
            let fetch_k = k * 2;
            let fts_results = self
                .fts_store
                .search(&query.text, project_id, fetch_k, cancel)
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
                })
                .collect();
            degraded.truncate(k);
            return degraded;
        }

        let fetch_k = k * 2;

        let (vector_results, fts_results) = tokio::join!(
            self.vector_store.search(project_id, embedding, fetch_k, cancel.clone()),
            self.fts_store.search(&query.text, project_id, fetch_k, cancel),
        );

        let vector_results = vector_results.unwrap_or_default();
        let fts_results = fts_results.unwrap_or_default();

        let mut fused = fuse_results(&vector_results, &fts_results, RRF_K);
        fused.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
        fused.truncate(k);
        fused
    }
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

    for (rank, vr) in vector_results.iter().enumerate() {
        let entry = scores.entry(vr.chunk_id.clone()).or_insert((0.0, 0.0));
        entry.0 = 1.0 / (k + (rank as f64));
        // Store content for later use
        content_map.insert(vr.chunk_id.clone(), vr.content.clone());
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
        })
        .collect();
    results.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    results
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_vector_result(chunk_id: &str, score: f64, content: &str) -> VectorResult {
        VectorResult { chunk_id: chunk_id.into(), score, content: content.into() }
    }

    fn make_fts_result(chunk_id: &str, score: f64, content: &str) -> FtsResult {
        FtsResult { chunk_id: chunk_id.into(), score, content: content.into() }
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
}
