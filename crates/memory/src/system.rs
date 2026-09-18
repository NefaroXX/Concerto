//! Phase 4 integrated memory system.
//!
//! `MemorySystem` wraps all Phase 4 components into a single
//! `concerto_core::traits::memory::MemoryStore` implementation that the
//! orchestrator can use as a drop-in replacement for Phase 3's basic
//! working memory.

use camino::Utf8PathBuf;
use glob::{MatchOptions, Pattern};
use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use concerto_core::error::MemoryError as CoreMemoryError;
use concerto_core::memory::{
    ChunkType, EmbeddingRecord, MemoryChunk, MemoryEntry, MemoryFilter, MemoryId, MemoryNamespace,
    MemoryQuery, ProjectId,
};
use concerto_core::traits::memory::MemoryStore;
use concerto_core::CancellationToken;

use tracing;

use crate::budget::ContextBudgetAllocator;
use crate::decision_store::DecisionStore;
use crate::embedder::EmbeddingGenerator;
use crate::embedder_health::EmbedderHealth;
use crate::entities::{L1Candidate, L1DedupJudge, L1DedupVerdict};
use crate::fts::FullTextStore;
use crate::global::GlobalMemoryStore;
use crate::rag::HybridRetriever;
use crate::sync::ChunkSyncService;
use crate::task_tree::TaskTreeStore;
use crate::vector_store::VectorStore;

/// How many vector candidates the L1 dedup judge may compare against.
///
/// Kept small: the judge only needs the handful of genuinely similar chunks
/// (see [`HybridRetriever::recall_candidates`]).
const DEDUP_VECTOR_TOP_K: usize = 5;
/// How many full-text candidates the L1 dedup judge may compare against.
const DEDUP_FTS_TOP_K: usize = 5;

/// The integrated Phase 4 memory system.
///
/// Wraps all memory layers and presents a unified `MemoryStore` interface.
pub struct MemorySystem {
    retriever: HybridRetriever,
    sync: ChunkSyncService,
    decision_store: Arc<DecisionStore>,
    task_tree: Arc<TaskTreeStore>,
    budget: ContextBudgetAllocator,
    embedder: Option<Arc<dyn EmbeddingGenerator>>,
    project_id: ProjectId,
    /// Optional global (user-scoped) memory store backed by a separate
    /// SQLite database. When present, `Global` namespace queries and
    /// writes are routed here instead of the project-scoped stores.
    global_store: Option<Arc<GlobalMemoryStore>>,
    /// Optional L1 dedup judge (ADR-46 symbolic offload). When set, every
    /// project-namespace `store` first recalls the top similar chunks and
    /// asks the judge whether to store / update / merge / skip. Fail-open:
    /// judge or recall errors store the entry unchanged.
    dedup_judge: Option<L1DedupJudge>,
}

impl MemorySystem {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        vector_store: Arc<dyn VectorStore>,
        fts_store: Arc<dyn FullTextStore>,
        decision_store: Arc<DecisionStore>,
        task_tree: Arc<TaskTreeStore>,
        embedder: Option<Arc<dyn EmbeddingGenerator>>,
        project_id: ProjectId,
        global_store: Option<Arc<GlobalMemoryStore>>,
    ) -> Self {
        let sync = ChunkSyncService::new(vector_store.clone(), fts_store.clone());
        let retriever = HybridRetriever::new(vector_store, fts_store);
        let budget = ContextBudgetAllocator::default();
        Self {
            retriever,
            sync,
            decision_store,
            task_tree,
            budget,
            embedder,
            project_id,
            global_store,
            dedup_judge: None,
        }
    }
    /// Access the decision store.
    ///
    /// Returns the shared `Arc` so a caller that also holds the same handle
    /// (e.g. the coordinator's Phase 6 M3 write-back/read-back wiring) reads
    /// and writes the SAME in-memory store — never a divergent copy.
    pub fn decisions(&self) -> &Arc<DecisionStore> {
        &self.decision_store
    }
    /// Access the task tree store.
    ///
    /// Returns the shared `Arc` (see [`Self::decisions`]).
    pub fn task_tree(&self) -> &Arc<TaskTreeStore> {
        &self.task_tree
    }
    /// Access the RAG context budget allocator.
    ///
    /// RAG-only (ADR-16/ADR-48): the allocator provides the score-ordered RAG
    /// bound via `truncate_to_rag_limit`. Working-memory and conversation
    /// budgets are owned by the orchestrator's `ContextEngine` +
    /// `ContextGuardProvider`, not by this system. `retrieve`/`store` never
    /// apply the allocator themselves — the RAG bound is applied by the caller
    /// that injects retrieved chunks into the prompt.
    pub fn budget(&self) -> &ContextBudgetAllocator {
        &self.budget
    }
    /// The project ID this system is scoped to.
    pub fn project_id(&self) -> &ProjectId {
        &self.project_id
    }

    /// Access the embedder, if configured.
    pub fn embedder(&self) -> Option<&Arc<dyn EmbeddingGenerator>> {
        self.embedder.as_ref()
    }

    /// Enable the L1 dedup judge for [`MemoryStore::store`] (ADR-46
    /// symbolic offload).
    ///
    /// When configured, every project-namespace `store` first recalls the
    /// top similar chunks and asks `judge` whether the new entry should be
    /// stored as-is, skipped (duplicate), stored after superseding an
    /// existing chunk (`update`), or merged with one (`merge`). The judge is
    /// extra latency per write and is advisory-only and fail-open: any
    /// judge / recall error logs a warning and stores the entry unchanged.
    pub fn with_dedup_judge(mut self, judge: L1DedupJudge) -> Self {
        self.dedup_judge = Some(judge);
        self
    }

    /// Decide how a project-namespace entry relates to already-stored chunks.
    ///
    /// Fail-open by construction: without a judge, on cancellation, when
    /// recall yields no candidates, or when the judge errors, the entry is
    /// stored unchanged as its own chunk.
    async fn decide_dedup(
        &self,
        entry: &MemoryEntry,
        vector: &[f32],
        cancel: CancellationToken,
    ) -> DedupDecision {
        let Some(judge) = self.dedup_judge.as_ref() else {
            return DedupDecision::store(entry, vector);
        };
        if cancel.is_cancelled() {
            return DedupDecision::store(entry, vector);
        }

        let recalled = self
            .retriever
            .recall_candidates(
                &self.project_id,
                &entry.content,
                vector,
                DEDUP_VECTOR_TOP_K,
                DEDUP_FTS_TOP_K,
                cancel.clone(),
            )
            .await;
        if recalled.is_empty() {
            // Nothing to compare against — no duplication possible.
            return DedupDecision::store(entry, vector);
        }
        let candidates: Vec<L1Candidate> = recalled
            .into_iter()
            .map(|candidate| L1Candidate {
                chunk_id: candidate.chunk_id,
                content: candidate.content,
            })
            .collect();

        let verdict = match judge.judge(&entry.content, &candidates).await {
            Ok(verdict) => verdict,
            Err(error) => {
                tracing::warn!(%error, "l1 dedup judge failed; storing memory unchanged");
                return DedupDecision::store(entry, vector);
            }
        };

        match verdict {
            L1DedupVerdict::Store => DedupDecision::store(entry, vector),
            L1DedupVerdict::Skip => DedupDecision::Skip,
            L1DedupVerdict::Update { target_id } => DedupDecision::Store {
                content: entry.content.clone(),
                vector: vector.to_vec(),
                tombstone_after: Some(target_id),
            },
            L1DedupVerdict::Merge { target_id } => {
                let Some(target) =
                    candidates.iter().find(|candidate| candidate.chunk_id == target_id)
                else {
                    tracing::warn!(
                        target_id,
                        "l1 dedup merge target missing from candidates; storing unchanged"
                    );
                    return DedupDecision::store(entry, vector);
                };
                let merged_content = format!("{}\n{}", entry.content, target.content);
                let merged_vector = if let Some(embedder) = self.embedder.as_ref() {
                    match embedder.embed(&merged_content).await {
                        Ok(embedding) => embedding,
                        Err(error) => {
                            tracing::warn!(
                                %error,
                                "l1 dedup merge re-embedding failed; keeping original vector"
                            );
                            vector.to_vec()
                        }
                    }
                } else {
                    Vec::new()
                };
                DedupDecision::Store {
                    content: merged_content,
                    vector: merged_vector,
                    tombstone_after: Some(target_id),
                }
            }
        }
    }
}

/// Outcome of the L1 dedup pass for one store call.
enum DedupDecision {
    /// The judge ruled the entry is already represented; the store is
    /// skipped entirely (the existing chunk stays canonical).
    Skip,
    /// Write the entry with the given (possibly merged) content/vector, then
    /// supersede `tombstone_after` — if any — AFTER the write so a failure
    /// can never leave the only copy of a memory gone.
    Store { content: String, vector: Vec<f32>, tombstone_after: Option<String> },
}

impl DedupDecision {
    /// The default fail-open decision: store the entry unchanged.
    fn store(entry: &MemoryEntry, vector: &[f32]) -> Self {
        DedupDecision::Store {
            content: entry.content.clone(),
            vector: vector.to_vec(),
            tombstone_after: None,
        }
    }
}

#[async_trait]

impl MemoryStore for MemorySystem {
    async fn retrieve(
        &self,
        query: &MemoryQuery,
        cancel: CancellationToken,
    ) -> Result<Vec<MemoryChunk>, CoreMemoryError> {
        // Route global namespace queries to the global store.
        if matches!(query.namespace, MemoryNamespace::Global { .. }) {
            return match self.global_store {
                Some(ref store) => store.retrieve(query, cancel).await,
                None => Ok(Vec::new()),
            };
        }

        validate_scope(&self.project_id, &query.project_id, &query.namespace)?;

        // Compute query embedding if an embedder is configured; otherwise
        // fall back to full-text-only retrieval.
        let embedding = if let Some(ref embedder) = self.embedder {
            match embedder.embed(&query.text).await {
                Ok(embedding) => embedding,
                Err(error) => {
                    tracing::warn!(
                        %error,
                        "query embedding unavailable; falling back to full-text memory retrieval"
                    );
                    // S1 (ADR-39): record the failure on the QUERY path too, so
                    // a broken embedder is registered even with no concurrent
                    // indexing. This makes the retriever's degraded (FTS-only)
                    // fallback and its notice reachable. `record_failure`
                    // returns `Some` only on the first failure of a (new)
                    // broken window — exactly one transition per window.
                    let health = EmbedderHealth::for_project(&query.project_id);
                    let _ = health.record_failure(std::time::Instant::now());
                    Vec::new()
                }
            }
        } else {
            tracing::trace!("vector search disabled: no embedder configured");
            Vec::new()
        };
        let results =
            self.retriever.retrieve(query, &embedding, Some(query.top_k), cancel.clone()).await;

        let ids: Vec<String> = results.iter().map(|result| result.chunk_id.clone()).collect();
        let metadata = self.retriever.load_chunks(&query.project_id, &ids, cancel.clone()).await?;
        let metadata_is_authoritative = self.retriever.supports_chunk_metadata();
        let mut metadata_by_id: HashMap<String, MemoryChunk> =
            metadata.into_iter().map(|chunk| (chunk.id.clone(), chunk)).collect();
        let file_patterns = compile_file_patterns(&query.filters)?;

        Ok(results
            .into_iter()
            .filter_map(|result| {
                let mut chunk = match metadata_by_id.remove(&result.chunk_id) {
                    Some(chunk) => chunk,
                    None if metadata_is_authoritative => return None,
                    None => MemoryChunk {
                        id: result.chunk_id.clone(),
                        project_id: query.project_id.clone(),
                        namespace: MemoryNamespace::Project(query.project_id.clone()),
                        content: result.content.clone(),
                        file_path: None,
                        start_line: None,
                        end_line: None,
                        chunk_type: ChunkType::SlidingWindow,
                        score: result.score,
                        model_id: String::new(),
                        model_version: String::new(),
                        stale: result.stale,
                    },
                };
                chunk.score = result.score;
                chunk.content = result.content;
                chunk.stale = result.stale;
                matches_filters(&chunk, &query.filters, &file_patterns).then_some(chunk)
            })
            .collect())
    }

    async fn browse(
        &self,
        project_id: &ProjectId,
        top_k: usize,
        _cancel: CancellationToken,
    ) -> Result<Vec<MemoryChunk>, CoreMemoryError> {
        if project_id != &self.project_id {
            return Err(CoreMemoryError::CrossProjectLeakage);
        }
        let results = self.retriever.browse(project_id, top_k, _cancel.clone()).await;
        let ids: Vec<String> = results.iter().map(|result| result.chunk_id.clone()).collect();
        let metadata_is_authoritative = self.retriever.supports_chunk_metadata();
        let mut metadata_by_id: HashMap<String, MemoryChunk> = self
            .retriever
            .load_chunks(project_id, &ids, _cancel.clone())
            .await?
            .into_iter()
            .map(|chunk| (chunk.id.clone(), chunk))
            .collect();
        Ok(results
            .into_iter()
            .filter_map(|result| {
                let mut chunk = match metadata_by_id.remove(&result.chunk_id) {
                    Some(chunk) => chunk,
                    None if metadata_is_authoritative => return None,
                    None => MemoryChunk {
                        id: result.chunk_id.clone(),
                        project_id: project_id.clone(),
                        namespace: MemoryNamespace::Project(project_id.clone()),
                        content: result.content.clone(),
                        file_path: None,
                        start_line: None,
                        end_line: None,
                        chunk_type: ChunkType::SlidingWindow,
                        score: result.score,
                        model_id: String::new(),
                        model_version: String::new(),
                        stale: result.stale,
                    },
                };
                chunk.score = result.score;
                chunk.content = result.content;
                chunk.stale = result.stale;
                Some(chunk)
            })
            .collect())
    }

    async fn store(
        &self,
        entry: MemoryEntry,
        cancel: CancellationToken,
    ) -> Result<MemoryId, CoreMemoryError> {
        // Route global namespace entries to the global store.
        if matches!(entry.namespace, MemoryNamespace::Global { .. }) {
            return match self.global_store {
                Some(ref store) => store.store(&entry, cancel).await,
                None => Err(CoreMemoryError::RetrievalFailed(
                    "global memory store is not configured".into(),
                )),
            };
        }

        validate_scope(&self.project_id, &entry.project_id, &entry.namespace)?;
        let vector = if let Some(ref embedder) = self.embedder {
            embedder.embed(&entry.content).await.map_err(|e| CoreMemoryError::EmbeddingFailed {
                reason: format!("embedding generation failed: {e}"),
            })?
        } else {
            Vec::new()
        };

        // L1 dedup (ADR-46 symbolic offload): an optional LLM judge decides
        // store / update / merge / skip against the already-stored chunks.
        // Fail-open: any judge or recall problem stores the entry unchanged.
        let decision = self.decide_dedup(&entry, &vector, cancel.clone()).await;

        let DedupDecision::Store { content, vector, tombstone_after } = decision else {
            // Skip: the judge ruled the entry is already represented.
            return Ok(entry.id);
        };

        let chunk_hash = blake3::hash(content.as_bytes()).to_string();

        let record = EmbeddingRecord {
            id: entry.id.0.to_string(),
            project_id: self.project_id.clone(),
            chunk_hash,
            content,
            file_path: Utf8PathBuf::from("memory"),
            start_line: None,
            end_line: None,
            chunk_type: entry.chunk_type,
            vector,
            model_id: entry.model_id.unwrap_or_default(),
            model_version: entry.model_version.unwrap_or_default(),
            stale: false,
            created_at: entry.created_at,
        };

        self.sync
            .store(&record, cancel.clone())
            .await
            .map_err(|e| CoreMemoryError::Persistence(format!("failed to store memory: {e}")))?;

        // Supersede the replaced/merged chunk ONLY after the write succeeded,
        // so a tombstone failure is logged-and-kept rather than data loss.
        if let Some(target_id) = tombstone_after {
            self.sync.tombstone(&target_id, &self.project_id, cancel).await.map_err(|e| {
                CoreMemoryError::Persistence(format!("failed to supersede memory {target_id}: {e}"))
            })?;
        }

        Ok(entry.id)
    }

    async fn invalidate(
        &self,
        id: MemoryId,
        cancel: CancellationToken,
    ) -> Result<(), CoreMemoryError> {
        // Try global store first if configured (ignore NotFound so project
        // entries can still be invalidated below).
        if let Some(ref store) = self.global_store {
            match store.invalidate(id, cancel.clone()).await {
                Ok(()) => return Ok(()),
                Err(CoreMemoryError::NotFound(_)) => { /* not a global entry */ }
                Err(e) => return Err(e),
            }
        }

        self.sync
            .tombstone(&id.0.to_string(), &self.project_id, cancel)
            .await
            .map_err(|e| CoreMemoryError::Persistence(format!("failed to invalidate memory: {e}")))
    }

    async fn invalidate_chunk(
        &self,
        id: &str,
        cancel: CancellationToken,
    ) -> Result<(), CoreMemoryError> {
        // Try global store first if configured (ignore NotFound so project
        // entries can still be invalidated below).
        if let Some(ref store) = self.global_store {
            match store.invalidate_chunk(id, cancel.clone()).await {
                Ok(()) => return Ok(()),
                Err(CoreMemoryError::NotFound(_)) => { /* not a global entry */ }
                Err(e) => return Err(e),
            }
        }

        self.sync
            .tombstone(id, &self.project_id, cancel)
            .await
            .map_err(|e| CoreMemoryError::Persistence(format!("failed to invalidate memory: {e}")))
    }
}

fn validate_scope(
    store_project: &ProjectId,
    requested_project: &ProjectId,
    namespace: &MemoryNamespace,
) -> Result<(), CoreMemoryError> {
    if store_project != requested_project {
        return Err(CoreMemoryError::CrossProjectLeakage);
    }
    match namespace {
        MemoryNamespace::Project(namespace_project) if namespace_project == requested_project => {
            Ok(())
        }
        MemoryNamespace::Project(_) => Err(CoreMemoryError::CrossProjectLeakage),
        // Global namespace is allowed — it is handled by the global store
        // before validate_scope is reached in retrieve/store, but browse()
        // still uses validate_scope for project-scoped browsing.
        MemoryNamespace::Global { .. } => Ok(()),
        _ => Err(CoreMemoryError::RetrievalFailed("unknown namespace".into())),
    }
}

fn compile_file_patterns(filters: &[MemoryFilter]) -> Result<Vec<Pattern>, CoreMemoryError> {
    filters
        .iter()
        .filter_map(|filter| match filter {
            MemoryFilter::FileGlob(pattern) => Some(pattern),
            _ => None,
        })
        .map(|pattern| {
            Pattern::new(pattern).map_err(|error| {
                CoreMemoryError::RetrievalFailed(format!(
                    "invalid memory file glob '{pattern}': {error}"
                ))
            })
        })
        .collect()
}

fn matches_filters(
    chunk: &MemoryChunk,
    filters: &[MemoryFilter],
    file_patterns: &[Pattern],
) -> bool {
    let options = MatchOptions {
        case_sensitive: !cfg!(windows),
        require_literal_separator: true,
        require_literal_leading_dot: false,
    };
    let mut file_pattern_index = 0usize;
    for filter in filters {
        match filter {
            MemoryFilter::ChunkType(chunk_type) if chunk.chunk_type != *chunk_type => return false,
            MemoryFilter::MinScore(threshold) if chunk.score < *threshold => return false,
            MemoryFilter::FileGlob(_) => {
                let Some(pattern) = file_patterns.get(file_pattern_index) else {
                    return false;
                };
                file_pattern_index += 1;
                let Some(path) = &chunk.file_path else {
                    return false;
                };
                if !pattern.matches_path_with(path.as_std_path(), options) {
                    return false;
                }
            }
            MemoryFilter::ExcludeStale | MemoryFilter::ChunkType(_) | MemoryFilter::MinScore(_) => {
            }
            _ => {}
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{
        FakeSummarizer, InMemoryFullTextStore, InMemoryVectorStore, MockEmbeddingGenerator,
    };
    use concerto_core::error::MemoryError;
    use concerto_core::memory::{ChunkType, MemoryNamespace, VectorResult};
    use std::sync::Mutex;
    use time::OffsetDateTime;

    struct TestVectorStore {
        last_query: Arc<Mutex<Option<Vec<f32>>>>,
    }

    impl TestVectorStore {
        fn new() -> Self {
            Self { last_query: Arc::new(Mutex::new(None)) }
        }
        fn get_last(&self) -> Option<Vec<f32>> {
            self.last_query.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl VectorStore for TestVectorStore {
        async fn store(
            &self,
            _records: &[EmbeddingRecord],
            _cancel: CancellationToken,
        ) -> Result<(), MemoryError> {
            Ok(())
        }
        async fn search(
            &self,
            _project_id: &ProjectId,
            query: &[f32],
            _top_k: usize,
            _cancel: CancellationToken,
        ) -> Result<Vec<VectorResult>, MemoryError> {
            *self.last_query.lock().unwrap() = Some(query.to_vec());
            Ok(vec![])
        }
        async fn tombstone(
            &self,
            _chunk_id: &str,
            _project_id: &ProjectId,
            _cancel: CancellationToken,
        ) -> Result<(), MemoryError> {
            Ok(())
        }
        async fn delete_tombstoned(
            &self,
            _project_id: &ProjectId,
            _cancel: CancellationToken,
        ) -> Result<(), MemoryError> {
            Ok(())
        }
        async fn mark_stale(
            &self,
            _project_id: &ProjectId,
            _model_version: &str,
            _cancel: CancellationToken,
        ) -> Result<(), MemoryError> {
            Ok(())
        }
        async fn delete_by_project(
            &self,
            _project_id: &ProjectId,
            _cancel: CancellationToken,
        ) -> Result<(), MemoryError> {
            Ok(())
        }
        async fn delete_by_file_path(
            &self,
            _project_id: &ProjectId,
            _file_path: &Utf8PathBuf,
            _cancel: CancellationToken,
        ) -> Result<Vec<String>, MemoryError> {
            Ok(vec![])
        }
    }

    fn make_query(text: &str) -> MemoryQuery {
        MemoryQuery {
            text: text.to_string(),
            top_k: 5,
            project_id: ProjectId("test".into()),
            namespace: MemoryNamespace::Project(ProjectId("test".into())),
            filters: vec![],
        }
    }

    struct FailingEmbeddingGenerator;

    #[async_trait]
    impl EmbeddingGenerator for FailingEmbeddingGenerator {
        async fn embed(&self, _text: &str) -> Result<Vec<f32>, MemoryError> {
            Err(MemoryError::EmbeddingFailed { reason: "offline".into() })
        }

        fn model_id(&self) -> &str {
            "failing"
        }

        fn model_version(&self) -> &str {
            "1"
        }

        fn dims(&self) -> usize {
            8
        }
    }

    #[tokio::test]
    async fn retrieve_uses_embedder() {
        let vector_store = Arc::new(TestVectorStore::new());
        let fts_store = Arc::new(InMemoryFullTextStore::new());
        let decision_store = Arc::new(DecisionStore::new());
        let task_tree = Arc::new(TaskTreeStore::new());
        let embedder: Option<Arc<dyn EmbeddingGenerator>> =
            Some(Arc::new(MockEmbeddingGenerator::new(384)));
        let system = MemorySystem::new(
            vector_store.clone(),
            fts_store,
            decision_store,
            task_tree,
            embedder,
            ProjectId("test".into()),
            None,
        );
        let query = make_query("hello world");
        let _ = system.retrieve(&query, CancellationToken::new()).await.unwrap();
        let recorded = vector_store.get_last().expect("search called");
        assert!(!recorded.is_empty(), "embedding should be non-empty");
    }

    #[tokio::test]
    async fn retrieve_without_embedder_uses_empty_vector() {
        let vector_store = Arc::new(TestVectorStore::new());
        let fts_store = Arc::new(InMemoryFullTextStore::new());
        let decision_store = Arc::new(DecisionStore::new());
        let task_tree = Arc::new(TaskTreeStore::new());
        let system = MemorySystem::new(
            vector_store.clone(),
            fts_store,
            decision_store,
            task_tree,
            None,
            ProjectId("test".into()),
            None,
        );
        let query = make_query("hello world");
        let _ = system.retrieve(&query, CancellationToken::new()).await.unwrap();
        let recorded = vector_store.get_last().expect("search called");
        assert!(recorded.is_empty(), "embedding should be empty when no embedder");
    }

    #[tokio::test]
    async fn embedding_failure_falls_back_to_attributed_fts_results() {
        // Use a project id distinct from the other `test`-project tests: the
        // query-path record_failure (S1) marks this project's embedder broken,
        // which must not leak into parallel "test"-project tests.
        let project_id = ProjectId("attributed-fts".into());
        let vector_store = Arc::new(InMemoryVectorStore::new());
        let fts_store = Arc::new(InMemoryFullTextStore::new());
        let sync = ChunkSyncService::new(vector_store.clone(), fts_store.clone());
        let record = EmbeddingRecord {
            id: "chunk".into(),
            project_id: project_id.clone(),
            chunk_hash: "hash".into(),
            content: "offline fallback phrase".into(),
            file_path: "src/lib.rs".into(),
            start_line: Some(7),
            end_line: Some(9),
            chunk_type: ChunkType::Function,
            vector: vec![0.0; 8],
            model_id: "test".into(),
            model_version: "1".into(),
            stale: false,
            created_at: OffsetDateTime::now_utc(),
        };
        sync.store(&record, CancellationToken::new()).await.unwrap();
        let system = MemorySystem::new(
            vector_store,
            fts_store,
            Arc::new(DecisionStore::new()),
            Arc::new(TaskTreeStore::new()),
            Some(Arc::new(FailingEmbeddingGenerator)),
            project_id.clone(),
            None,
        );
        let mut query = make_query("fallback");
        query.project_id = project_id.clone();
        query.namespace = MemoryNamespace::Project(project_id.clone());
        query.filters = vec![
            MemoryFilter::ChunkType(ChunkType::Function),
            MemoryFilter::FileGlob("src/*.rs".into()),
        ];

        let results = system.retrieve(&query, CancellationToken::new()).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].project_id, project_id);
        assert_eq!(results[0].file_path.as_deref(), Some(camino::Utf8Path::new("src/lib.rs")));
        assert_eq!(results[0].start_line, Some(7));
        assert_eq!(results[0].end_line, Some(9));
    }

    #[tokio::test]
    async fn rejects_cross_project_queries() {
        let vector_store = Arc::new(InMemoryVectorStore::new());
        let fts_store = Arc::new(InMemoryFullTextStore::new());
        let system = MemorySystem::new(
            vector_store,
            fts_store,
            Arc::new(DecisionStore::new()),
            Arc::new(TaskTreeStore::new()),
            None,
            ProjectId("test".into()),
            None,
        );
        let mut cross_project = make_query("anything");
        cross_project.project_id = ProjectId("other".into());
        cross_project.namespace = MemoryNamespace::Project(cross_project.project_id.clone());
        assert!(matches!(
            system.retrieve(&cross_project, CancellationToken::new()).await,
            Err(MemoryError::CrossProjectLeakage)
        ));
    }

    #[tokio::test]
    async fn global_namespace_returns_empty_when_no_store_configured() {
        let vector_store = Arc::new(InMemoryVectorStore::new());
        let fts_store = Arc::new(InMemoryFullTextStore::new());
        let system = MemorySystem::new(
            vector_store,
            fts_store,
            Arc::new(DecisionStore::new()),
            Arc::new(TaskTreeStore::new()),
            None,
            ProjectId("test".into()),
            None,
        );
        let mut global = make_query("anything");
        global.namespace = MemoryNamespace::Global { user_id_hash: "user".into() };
        // Without a global store, global queries return empty (not an error)
        let results = system.retrieve(&global, CancellationToken::new()).await.unwrap();
        assert!(results.is_empty());
    }

    // ---------------------------------------------------------------------
    // L1 dedup judge (with_dedup_judge)
    // ---------------------------------------------------------------------

    /// Seed one fact chunk into freshly created in-memory stores, routing the
    /// write through `ChunkSyncService` so both sides agree on the record.
    async fn dedup_seed(
        project_id: &ProjectId,
        id: &str,
        content: &str,
        vector_store: Arc<InMemoryVectorStore>,
        fts_store: Arc<InMemoryFullTextStore>,
    ) {
        let sync = ChunkSyncService::new(vector_store, fts_store);
        let record = EmbeddingRecord {
            id: id.into(),
            project_id: project_id.clone(),
            chunk_hash: "hash".into(),
            content: content.into(),
            file_path: "memory".into(),
            start_line: None,
            end_line: None,
            chunk_type: ChunkType::Fact,
            vector: vec![0.0; 8],
            model_id: "test".into(),
            model_version: "1".into(),
            stale: false,
            created_at: OffsetDateTime::now_utc(),
        };
        sync.store(&record, CancellationToken::new()).await.unwrap();
    }

    fn dedup_system(
        vector_store: Arc<InMemoryVectorStore>,
        fts_store: Arc<InMemoryFullTextStore>,
        judge: Option<L1DedupJudge>,
    ) -> MemorySystem {
        let system = MemorySystem::new(
            vector_store,
            fts_store,
            Arc::new(DecisionStore::new()),
            Arc::new(TaskTreeStore::new()),
            Some(Arc::new(MockEmbeddingGenerator::new(384))),
            ProjectId("test".into()),
            None,
        );
        match judge {
            Some(judge) => system.with_dedup_judge(judge),
            None => system,
        }
    }

    fn dedup_entry(content: &str) -> MemoryEntry {
        MemoryEntry {
            id: MemoryId(ulid::Ulid::new()),
            project_id: ProjectId("test".into()),
            namespace: MemoryNamespace::Project(ProjectId("test".into())),
            content: content.into(),
            chunk_type: ChunkType::Fact,
            model_id: None,
            model_version: None,
            metadata: serde_json::json!({}),
            expires_at: None,
            created_at: OffsetDateTime::now_utc(),
        }
    }

    async fn stored_records(
        vector_store: &InMemoryVectorStore,
        project_id: &ProjectId,
    ) -> Vec<VectorResult> {
        vector_store.search(project_id, &[0.1; 8], 100, CancellationToken::new()).await.unwrap()
    }

    #[tokio::test]
    async fn store_without_dedup_judge_writes_plainly() {
        let vector_store = Arc::new(InMemoryVectorStore::new());
        let fts_store = Arc::new(InMemoryFullTextStore::new());
        let system = dedup_system(vector_store.clone(), fts_store, None);
        let entry = dedup_entry("the user prefers rust");
        let stored = system.store(entry.clone(), CancellationToken::new()).await.unwrap();
        assert_eq!(stored, entry.id);

        let records = stored_records(&vector_store, &ProjectId("test".into())).await;
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].content, "the user prefers rust");
    }

    #[tokio::test]
    async fn store_skips_near_duplicate_without_writing() {
        let project_id = ProjectId("test".into());
        let vector_store = Arc::new(InMemoryVectorStore::new());
        let fts_store = Arc::new(InMemoryFullTextStore::new());
        dedup_seed(
            &project_id,
            "existing-1",
            "the user's favorite food is tacos",
            vector_store.clone(),
            fts_store.clone(),
        )
        .await;

        let judge = L1DedupJudge::new(Arc::new(FakeSummarizer::new(
            r#"{"action": "skip", "target_id": null}"#,
        )));
        let system = dedup_system(vector_store.clone(), fts_store, Some(judge));
        let entry = dedup_entry("the user's favorite food is tacos");
        let stored = system.store(entry.clone(), CancellationToken::new()).await.unwrap();
        assert_eq!(stored, entry.id);

        let records = stored_records(&vector_store, &project_id).await;
        assert_eq!(records.len(), 1, "skip must not write a second chunk");
        assert_eq!(records[0].chunk_id, "existing-1");
    }

    #[tokio::test]
    async fn store_merges_and_tombstones_target() {
        let project_id = ProjectId("test".into());
        let vector_store = Arc::new(InMemoryVectorStore::new());
        let fts_store = Arc::new(InMemoryFullTextStore::new());
        dedup_seed(
            &project_id,
            "existing-1",
            "the user's preferred color is blue",
            vector_store.clone(),
            fts_store.clone(),
        )
        .await;

        let judge = L1DedupJudge::new(Arc::new(FakeSummarizer::new(
            r#"{"action": "merge", "target_id": "existing-1"}"#,
        )));
        let system = dedup_system(vector_store.clone(), fts_store, Some(judge));
        system
            .store(dedup_entry("the user's preferred color is teal"), CancellationToken::new())
            .await
            .unwrap();

        let records = stored_records(&vector_store, &project_id).await;
        assert_eq!(records.len(), 1, "merge must supersede the target chunk");
        assert!(records[0].content.contains("teal"));
        assert!(
            records[0].content.contains("blue"),
            "merged content must include the target's content: {}",
            records[0].content
        );
    }

    #[tokio::test]
    async fn store_update_tombstones_target() {
        let project_id = ProjectId("test".into());
        let vector_store = Arc::new(InMemoryVectorStore::new());
        let fts_store = Arc::new(InMemoryFullTextStore::new());
        dedup_seed(
            &project_id,
            "existing-1",
            "the user's favorite city is london",
            vector_store.clone(),
            fts_store.clone(),
        )
        .await;

        let judge = L1DedupJudge::new(Arc::new(FakeSummarizer::new(
            r#"{"action": "update", "target_id": "existing-1"}"#,
        )));
        let system = dedup_system(vector_store.clone(), fts_store, Some(judge));
        system
            .store(dedup_entry("the user's favorite city is paris"), CancellationToken::new())
            .await
            .unwrap();

        let records = stored_records(&vector_store, &project_id).await;
        assert_eq!(records.len(), 1, "update must supersede the target chunk");
        assert_eq!(records[0].content, "the user's favorite city is paris");
        assert!(!records[0].content.contains("london"));
    }

    #[tokio::test]
    async fn store_falls_back_to_plain_insert_on_judge_error() {
        let project_id = ProjectId("test".into());
        let vector_store = Arc::new(InMemoryVectorStore::new());
        let fts_store = Arc::new(InMemoryFullTextStore::new());
        dedup_seed(
            &project_id,
            "existing-1",
            "a pre-existing fact",
            vector_store.clone(),
            fts_store.clone(),
        )
        .await;

        let judge = L1DedupJudge::new(Arc::new(FakeSummarizer::new_err("judge down")));
        let system = dedup_system(vector_store.clone(), fts_store, Some(judge));
        system.store(dedup_entry("an entirely new fact"), CancellationToken::new()).await.unwrap();

        // Fail-open: the judge's error must not drop the memory.
        let records = stored_records(&vector_store, &project_id).await;
        assert_eq!(records.len(), 2);
        assert!(records.iter().any(|r| r.content == "an entirely new fact"));
    }
}
