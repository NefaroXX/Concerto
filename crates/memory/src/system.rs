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
use crate::embedder_health::{EmbedderHealth, EMBEDDER_DEGRADED_NOTICE};
use crate::fts::FullTextStore;
use crate::global::GlobalMemoryStore;
use crate::rag::HybridRetriever;
use crate::sync::ChunkSyncService;
use crate::task_tree::TaskTreeStore;
use crate::vector_store::VectorStore;

/// The integrated Phase 4 memory system.
///
/// Wraps all memory layers and presents a unified `MemoryStore` interface.
pub struct MemorySystem {
    retriever: HybridRetriever,
    sync: ChunkSyncService,
    decision_store: DecisionStore,
    task_tree: TaskTreeStore,
    budget: ContextBudgetAllocator,
    embedder: Option<Arc<dyn EmbeddingGenerator>>,
    project_id: ProjectId,
    /// Optional global (user-scoped) memory store backed by a separate
    /// SQLite database. When present, `Global` namespace queries and
    /// writes are routed here instead of the project-scoped stores.
    global_store: Option<Arc<GlobalMemoryStore>>,
}

impl MemorySystem {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        vector_store: Arc<dyn VectorStore>,
        fts_store: Arc<dyn FullTextStore>,
        decision_store: DecisionStore,
        task_tree: TaskTreeStore,
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
        }
    }
    /// Access the decision store.
    pub fn decisions(&self) -> &DecisionStore {
        &self.decision_store
    }
    /// Access the task tree store.
    pub fn task_tree(&self) -> &TaskTreeStore {
        &self.task_tree
    }
    /// Access the budget allocator.
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
        let mut degraded_window = false;
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
                    degraded_window = health.record_failure(std::time::Instant::now()).is_some();
                    Vec::new()
                }
            }
        } else {
            tracing::trace!("vector search disabled: no embedder configured");
            Vec::new()
        };
        let results =
            self.retriever.retrieve(query, &embedding, Some(query.top_k), cancel.clone()).await;

        // S2 (ADR-39): surface the degraded notice to the user. The API-level
        // notice on the retriever's `FusedResult` carries no channel through
        // `MemoryChunk` (core memory types are frozen this wave), so surface it
        // via a log once per broken window (aligned with the query-path window
        // transition above / the indexer's event line).
        if degraded_window && results.iter().any(|r| r.notice.is_some()) {
            tracing::warn!(
                project_id = %query.project_id.0,
                "{}",
                EMBEDDER_DEGRADED_NOTICE
            );
        }
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
                    },
                };
                chunk.score = result.score;
                chunk.content = result.content;
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
                    },
                };
                chunk.score = result.score;
                chunk.content = result.content;
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

        let chunk_hash = blake3::hash(entry.content.as_bytes()).to_string();

        let record = EmbeddingRecord {
            id: entry.id.0.to_string(),
            project_id: self.project_id.clone(),
            chunk_hash,
            content: entry.content,
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
            .store(&record, cancel)
            .await
            .map_err(|e| CoreMemoryError::Persistence(format!("failed to store memory: {e}")))?;

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
    use crate::testing::{InMemoryFullTextStore, InMemoryVectorStore, MockEmbeddingGenerator};
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
        let decision_store = DecisionStore::new();
        let task_tree = TaskTreeStore::new();
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
        let decision_store = DecisionStore::new();
        let task_tree = TaskTreeStore::new();
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
            DecisionStore::new(),
            TaskTreeStore::new(),
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
            DecisionStore::new(),
            TaskTreeStore::new(),
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
            DecisionStore::new(),
            TaskTreeStore::new(),
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
}
