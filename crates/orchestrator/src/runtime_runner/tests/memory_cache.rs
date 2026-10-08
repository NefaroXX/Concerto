//! Project-switch memory isolation (Audit G1) + memory-optional degradation
//! (ADR-65 acceptance 9) coverage for `runtime_runner_impl`.
//!
//! Mechanical extraction (NORM S24B): the lightweight in-memory stand-ins
//! (`DummyEmbedder` / `DummyVectorStore` / `DummyFullTextStore`), the
//! `active_memory_for` slot builder, and the six tests that drive them move
//! verbatim out of `runtime_runner::runtime_runner_tests`, so test names and
//! assertions are unchanged. Every fixture user is inside this span, so the
//! fixtures move with their tests — never shared, never duplicated.
//! `use super::super::*;` keeps the parent (`runtime_runner_impl`) items in
//! scope; `async_trait` is the only old test-mod import this cluster touches.

use super::super::*;
use async_trait::async_trait;

// ------------------------------------------------------------------
// Audit G1: project-switch memory isolation. The selection/reset core
// of `select_or_init_memory_services` is tested directly with lightweight
// in-memory stand-ins (no network, no SQLite): the same project reuses
// its cached store, while a different project never reuses the previous
// project's memory and drops its lifecycle entirely.
// ------------------------------------------------------------------

/// Minimal embedding generator — returns a fixed zero vector, never
/// touches fastembed or the network.
struct DummyEmbedder;

#[async_trait]
impl EmbeddingGenerator for DummyEmbedder {
    async fn embed(&self, _text: &str) -> Result<Vec<f32>, concerto_core::MemoryError> {
        Ok(vec![0.0; 4])
    }

    fn model_id(&self) -> &str {
        "dummy"
    }

    fn model_version(&self) -> &str {
        "test"
    }

    fn dims(&self) -> usize {
        4
    }
}

/// Minimal vector store — accepts writes, returns nothing on search.
struct DummyVectorStore;

#[async_trait]
impl concerto_memory::vector_store::VectorStore for DummyVectorStore {
    async fn store(
        &self,
        _records: &[concerto_core::memory::EmbeddingRecord],
        _cancel: CancellationToken,
    ) -> Result<(), concerto_core::MemoryError> {
        Ok(())
    }

    async fn search(
        &self,
        _project_id: &ProjectId,
        _query: &[f32],
        _top_k: usize,
        _cancel: CancellationToken,
    ) -> Result<Vec<concerto_core::memory::VectorResult>, concerto_core::MemoryError> {
        Ok(Vec::new())
    }

    async fn tombstone(
        &self,
        _chunk_id: &str,
        _project_id: &ProjectId,
        _cancel: CancellationToken,
    ) -> Result<(), concerto_core::MemoryError> {
        Ok(())
    }

    async fn delete_tombstoned(
        &self,
        _project_id: &ProjectId,
        _cancel: CancellationToken,
    ) -> Result<(), concerto_core::MemoryError> {
        Ok(())
    }

    async fn mark_stale(
        &self,
        _project_id: &ProjectId,
        _chunk_id: &str,
        _cancel: CancellationToken,
    ) -> Result<(), concerto_core::MemoryError> {
        Ok(())
    }

    async fn delete_by_project(
        &self,
        _project_id: &ProjectId,
        _cancel: CancellationToken,
    ) -> Result<(), concerto_core::MemoryError> {
        Ok(())
    }

    async fn delete_by_file_path(
        &self,
        _project_id: &ProjectId,
        _file_path: &camino::Utf8PathBuf,
        _cancel: CancellationToken,
    ) -> Result<Vec<String>, concerto_core::MemoryError> {
        Ok(Vec::new())
    }
}

/// Minimal full-text store — accepts writes, returns nothing on search.
struct DummyFullTextStore;

#[async_trait]
impl concerto_memory::fts::FullTextStore for DummyFullTextStore {
    async fn insert(
        &self,
        _chunk: &concerto_core::memory::MemoryChunk,
        _project_id: &ProjectId,
        _cancel: CancellationToken,
    ) -> Result<(), concerto_core::MemoryError> {
        Ok(())
    }

    async fn delete(
        &self,
        _chunk_id: &str,
        _project_id: &ProjectId,
        _cancel: CancellationToken,
    ) -> Result<(), concerto_core::MemoryError> {
        Ok(())
    }

    async fn search(
        &self,
        _query: &str,
        _project_id: &ProjectId,
        _top_k: usize,
        _cancel: CancellationToken,
    ) -> Result<Vec<concerto_core::memory::FtsResult>, concerto_core::MemoryError> {
        Ok(Vec::new())
    }

    async fn delete_by_project(
        &self,
        _project_id: &ProjectId,
        _cancel: CancellationToken,
    ) -> Result<(), concerto_core::MemoryError> {
        Ok(())
    }
}

/// A fully populated `ActiveMemoryServices` slot for `project_id` whose
/// store identity is the passed `Arc` (so reuse can be proven with
/// pointer equality).
fn active_memory_for(project_id: ProjectId, store: Arc<dyn MemoryStore>) -> ActiveMemoryServices {
    ActiveMemoryServices {
        project_id,
        store,
        reindex: Arc::new(ProjectIndexer::new(
            Arc::new(DummyEmbedder),
            EventBus::new(16),
            ProjectId("unused".into()),
        )),
        reindex_sync: Arc::new(ChunkSyncService::new(
            Arc::new(DummyVectorStore),
            Arc::new(DummyFullTextStore),
        )),
        cancel: CancellationToken::new(),
        data_dir_lock: None,
        // The test slot predates the M3a/M3b wiring; absent handles keep
        // the coordinator's write-back/retrieval inert.
        decision_store: None,
        task_tree: None,
    }
}

/// G1: a run for the same project reuses the cached store (same `Arc`
/// identity — no re-initialisation, no project boundary crossed).
#[test]
fn same_project_reuses_cached_store() {
    let project_a = ProjectId("project-a".into());
    let store_a: Arc<dyn MemoryStore> = Arc::new(NullMemoryStore);
    let memory = Arc::new(Mutex::new(Some(active_memory_for(project_a.clone(), store_a.clone()))));

    let selected = cached_store_for_project(&memory, &project_a)
        .expect("same project must reuse the cached store");
    assert!(Arc::ptr_eq(&store_a, &selected), "reuse must hand back the same store");
}

/// G1: a run for a *different* project must not reuse the previous
/// project's store — the selection returns `None`, forcing a reset +
/// fresh initialisation for the new project.
#[test]
fn different_project_never_reuses_previous_store() {
    let project_a = ProjectId("project-a".into());
    let project_b = ProjectId("project-b".into());
    let store_a: Arc<dyn MemoryStore> = Arc::new(NullMemoryStore);
    let memory = Arc::new(Mutex::new(Some(active_memory_for(project_a.clone(), store_a.clone()))));

    assert!(
        cached_store_for_project(&memory, &project_b).is_none(),
        "a project switch must never hand back the previous project's store"
    );
}

/// G1: switching projects cancels and drops the previous project's
/// lifecycle (indexer, sync service, store, cancellation token), so no
/// memory state leaks across project boundaries.
#[test]
fn project_switch_drops_previous_lifecycle() {
    let project_a = ProjectId("project-a".into());
    let store_a: Arc<dyn MemoryStore> = Arc::new(NullMemoryStore);
    let memory = Arc::new(Mutex::new(Some(active_memory_for(project_a.clone(), store_a.clone()))));
    let cancel_token = memory
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .as_ref()
        .expect("services present")
        .cancel
        .clone();

    reset_memory_services(&memory);

    assert!(
        memory.lock().unwrap_or_else(|poison| poison.into_inner()).is_none(),
        "the previous project's services must be dropped on a project switch"
    );
    assert!(
        cancel_token.is_cancelled(),
        "the previous project's lifecycle token must be cancelled"
    );
}

/// G1 end-to-end shape: after switching to a different project, even a
/// subsequent run for the *original* project must not find a stale cache
/// — the previous lifecycle is gone and a fresh initialisation happens.
#[test]
fn switch_away_and_back_requires_fresh_initialisation() {
    let project_a = ProjectId("project-a".into());
    let project_b = ProjectId("project-b".into());
    let store_a: Arc<dyn MemoryStore> = Arc::new(NullMemoryStore);
    let memory = Arc::new(Mutex::new(Some(active_memory_for(project_a.clone(), store_a.clone()))));

    // Run for project B: no reuse, previous lifecycle dropped.
    assert!(cached_store_for_project(&memory, &project_b).is_none());
    reset_memory_services(&memory);

    // Run for project A again: the cache is gone, fresh init is required.
    assert!(
        cached_store_for_project(&memory, &project_a).is_none(),
        "no stale cache may survive a project switch"
    );
}

// ------------------------------------------------------------------
// ADR-65 acceptance 9: memory is optional — disabled, absent, or a
// failed init all degrade to `NullMemoryStore`, never aborting a run.
// ------------------------------------------------------------------

/// A healthy selection hands back the SAME store (identity-checked).
#[test]
fn memory_store_or_disabled_keeps_a_healthy_store() {
    let store: Arc<dyn MemoryStore> = Arc::new(NullMemoryStore);
    let selected = memory_store_or_disabled(Ok(Some(store.clone())));
    assert!(Arc::ptr_eq(&store, &selected), "a healthy memory system is never replaced");
}

/// The degraded shapes (`None` selection and a failed init) both yield a
/// usable null store: retired-empty retrieves, silent discarding writes —
/// exactly the behavior of the store a memory-disabled run uses.
#[test]
fn memory_store_or_disabled_degraded_selection_behaves_null() {
    for selection in [Ok(None), Err(OrchestratorError::AgentLoopError("boom".into()))] {
        let selected = memory_store_or_disabled(selection);
        let query = concerto_core::memory::MemoryQuery {
            text: "anything".into(),
            project_id: concerto_core::types::ProjectId("p".into()),
            namespace: concerto_core::memory::MemoryNamespace::Project(
                concerto_core::types::ProjectId("p".into()),
            ),
            top_k: 5,
            filters: vec![],
        };
        let retrieved = tokio::runtime::Runtime::new()
            .expect("runtime")
            .block_on(selected.retrieve(&query, CancellationToken::new()));
        assert!(retrieved.expect("retrieve").is_empty(), "disabled memory retrieves nothing");

        let entry = concerto_core::memory::MemoryEntry {
            id: concerto_core::memory::MemoryId(concerto_core::ids::Ulid::new()),
            project_id: concerto_core::types::ProjectId("p".into()),
            namespace: concerto_core::memory::MemoryNamespace::Project(
                concerto_core::types::ProjectId("p".into()),
            ),
            content: "x".into(),
            chunk_type: concerto_core::memory::ChunkType::Fact,
            model_id: None,
            model_version: None,
            metadata: serde_json::Value::Null,
            expires_at: None,
            created_at: time::OffsetDateTime::now_utc(),
        };
        let stored = tokio::runtime::Runtime::new()
            .expect("runtime")
            .block_on(selected.store(entry, CancellationToken::new()));
        assert!(stored.is_ok(), "the null store accepts (and discards) writes");
    }
}
