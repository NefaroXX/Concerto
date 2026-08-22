//! Test harness fakes and helpers for the memory subsystem.
//!
//! All items are `#[cfg(test)]`-gated so they do not appear in release
//! builds. Every memory unit test in CI uses these fakes instead of
//! real external vector stores, `fastembed` model downloads, or filesystem
//! watchers.

use concerto_core::CancellationToken;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use camino::Utf8PathBuf;

use crate::embedder::EmbeddingGenerator;
use crate::fts::FullTextStore;
use crate::vector_store::VectorStore;
use async_trait::async_trait;
use concerto_core::error::MemoryError;
use concerto_core::memory::{
    ChunkType, EmbeddingRecord, FtsResult, MemoryChunk, MemoryNamespace, ProjectId, VectorResult,
};

// ---------------------------------------------------------------------------
// InMemoryVectorStore
// ---------------------------------------------------------------------------

/// A `VectorStore` implementation backed by an in-memory `HashMap`.
///
/// Panics on cross-project leakage (asserts `project_id` matches).
/// Perfect for unit tests — no external vector store required.
///
/// NOTE: The `VectorStore` trait is defined in [`crate::vector_store`];
/// this is a test double that follows the same shape.
#[cfg(test)]
pub struct InMemoryVectorStore {
    data: Arc<Mutex<HashMap<ProjectId, Vec<EmbeddingRecord>>>>,
}

#[cfg(test)]
impl InMemoryVectorStore {
    pub fn new() -> Self {
        Self { data: Arc::new(Mutex::new(HashMap::new())) }
    }
}

#[cfg(test)]
impl Default for InMemoryVectorStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
impl InMemoryVectorStore {
    /// Seed the store with pre-built records.
    pub fn with_records(records: Vec<(ProjectId, EmbeddingRecord)>) -> Self {
        let mut map: HashMap<ProjectId, Vec<EmbeddingRecord>> = HashMap::new();
        for (pid, record) in records {
            map.entry(pid).or_default().push(record);
        }
        Self { data: Arc::new(Mutex::new(map)) }
    }
}

#[cfg(test)]
#[async_trait]
impl VectorStore for InMemoryVectorStore {
    async fn store(
        &self,
        records: &[EmbeddingRecord],
        _cancel: CancellationToken,
    ) -> Result<(), MemoryError> {
        let mut map = self.data.lock().unwrap();
        for record in records {
            let project_records = map.entry(record.project_id.clone()).or_default();
            project_records.retain(|existing| existing.id != record.id);
            project_records.push(record.clone());
        }
        Ok(())
    }

    async fn search(
        &self,
        project_id: &ProjectId,
        query: &[f32],
        top_k: usize,
        _cancel: CancellationToken,
    ) -> Result<Vec<VectorResult>, MemoryError> {
        if query.is_empty() {
            return Ok(Vec::new());
        }
        let map = self.data.lock().unwrap();
        let records = map.get(project_id).ok_or_else(|| {
            MemoryError::RetrievalFailed(format!("no records for project {project_id}"))
        })?;
        let mut results: Vec<VectorResult> = records
            .iter()
            .map(|r| VectorResult {
                chunk_id: r.id.clone(),
                score: 0.5,
                content: r.content.clone(),
            })
            .collect();
        results.sort_by(|a, b| a.chunk_id.cmp(&b.chunk_id));
        results.truncate(top_k);
        Ok(results)
    }

    async fn get_chunks(
        &self,
        project_id: &ProjectId,
        chunk_ids: &[String],
        _cancel: CancellationToken,
    ) -> Result<Vec<MemoryChunk>, MemoryError> {
        let map = self.data.lock().unwrap();
        let Some(records) = map.get(project_id) else {
            return Ok(Vec::new());
        };
        Ok(chunk_ids
            .iter()
            .filter_map(|chunk_id| records.iter().find(|record| &record.id == chunk_id))
            .map(|record| MemoryChunk {
                id: record.id.clone(),
                project_id: project_id.clone(),
                namespace: MemoryNamespace::Project(project_id.clone()),
                content: record.content.clone(),
                file_path: Some(record.file_path.clone()),
                start_line: record.start_line,
                end_line: record.end_line,
                chunk_type: record.chunk_type,
                score: 0.0,
                model_id: record.model_id.clone(),
                model_version: record.model_version.clone(),
            })
            .collect())
    }

    fn supports_chunk_metadata(&self) -> bool {
        true
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
        project_id: &ProjectId,
        _model_version: &str,
        _cancel: CancellationToken,
    ) -> Result<(), MemoryError> {
        let mut map = self.data.lock().unwrap();
        if let Some(records) = map.get_mut(project_id) {
            for record in records.iter_mut() {
                record.stale = true;
            }
        }
        Ok(())
    }

    async fn delete_by_project(
        &self,
        project_id: &ProjectId,
        _cancel: CancellationToken,
    ) -> Result<(), MemoryError> {
        let mut map = self.data.lock().unwrap();
        map.remove(project_id);
        Ok(())
    }

    async fn delete_by_file_path(
        &self,
        project_id: &ProjectId,
        file_path: &Utf8PathBuf,
        _cancel: CancellationToken,
    ) -> Result<Vec<String>, MemoryError> {
        let mut map = self.data.lock().unwrap();
        if let Some(records) = map.get_mut(project_id) {
            let ids: Vec<String> = records
                .iter()
                .filter(|r| &r.file_path == file_path)
                .map(|r| r.id.clone())
                .collect();
            records.retain(|r| &r.file_path != file_path);
            Ok(ids)
        } else {
            Ok(Vec::new())
        }
    }
}

// ---------------------------------------------------------------------------
// FakeIndexer
// ---------------------------------------------------------------------------

/// An indexer that returns pre-seeded chunks regardless of project dir.
#[cfg(test)]
pub struct FakeIndexer {
    pub chunks: Vec<EmbeddingRecord>,
}

#[cfg(test)]
impl FakeIndexer {
    pub fn new(chunks: Vec<EmbeddingRecord>) -> Self {
        Self { chunks }
    }
}

// ---------------------------------------------------------------------------
// MockEmbeddingGenerator
// ---------------------------------------------------------------------------

/// Deterministic embedding generator for tests.
///
/// Defaults to 384-dimensional zero vectors. Use `seeded()` to get
/// distinct vectors for similarity tests.
#[cfg(test)]
pub struct MockEmbeddingGenerator {
    dim: usize,
    seed: u64,
}

#[cfg(test)]
impl MockEmbeddingGenerator {
    pub fn new(dim: usize) -> Self {
        Self { dim, seed: 0 }
    }

    pub fn seeded(seed: u64) -> Self {
        Self { dim: 384, seed }
    }
}

#[cfg(test)]
#[async_trait]
impl EmbeddingGenerator for MockEmbeddingGenerator {
    async fn embed(&self, _text: &str) -> Result<Vec<f32>, MemoryError> {
        // Deterministic pseudo-random vector for similarity comparison
        let mut vec = Vec::with_capacity(self.dim);
        for i in 0..self.dim {
            let val =
                ((self.seed ^ (i as u64).wrapping_mul(6364136223846793005)) % 1000) as f32 / 1000.0;
            vec.push(val);
        }
        Ok(vec)
    }

    fn model_id(&self) -> &str {
        "mock-model"
    }

    fn model_version(&self) -> &str {
        "0.1.0"
    }

    fn dims(&self) -> usize {
        self.dim
    }
}

// ---------------------------------------------------------------------------
// FakeSummarizer
// ---------------------------------------------------------------------------

/// A summarizer that returns a fixed string — for unit tests.
/// Re-exported from `summarizer::FakeSummarizer`.
#[cfg(test)]
pub use crate::summarizer::FakeSummarizer;

// ---------------------------------------------------------------------------
// InMemoryFullTextStore
// ---------------------------------------------------------------------------

/// In-memory FTS store for unit tests.
#[cfg(test)]
pub struct InMemoryFullTextStore {
    data: Arc<Mutex<HashMap<(ProjectId, String), String>>>,
}

#[cfg(test)]
impl InMemoryFullTextStore {
    pub fn new() -> Self {
        Self { data: Arc::new(Mutex::new(HashMap::new())) }
    }
}

#[cfg(test)]
impl Default for InMemoryFullTextStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[async_trait]
impl FullTextStore for InMemoryFullTextStore {
    async fn insert(
        &self,
        chunk: &MemoryChunk,
        project_id: &ProjectId,
        _cancel: CancellationToken,
    ) -> Result<(), MemoryError> {
        let mut map = self.data.lock().unwrap();
        map.insert((project_id.clone(), chunk.id.clone()), chunk.content.clone());
        Ok(())
    }

    async fn delete(
        &self,
        chunk_id: &str,
        project_id: &ProjectId,
        _cancel: CancellationToken,
    ) -> Result<(), MemoryError> {
        let mut map = self.data.lock().unwrap();
        map.remove(&(project_id.clone(), chunk_id.to_string()));
        Ok(())
    }

    async fn search(
        &self,
        query: &str,
        project_id: &ProjectId,
        top_k: usize,
        _cancel: CancellationToken,
    ) -> Result<Vec<FtsResult>, MemoryError> {
        let map = self.data.lock().unwrap();
        let query_lower = query.to_lowercase();
        let mut results: Vec<FtsResult> = map
            .iter()
            .filter(|((stored_project, _), content)| {
                stored_project == project_id && content.to_lowercase().contains(&query_lower)
            })
            .map(|((_, id), content)| FtsResult {
                chunk_id: id.clone(),
                score: 1.0,
                content: content.clone(),
            })
            .collect();
        results.truncate(top_k);
        Ok(results)
    }

    async fn delete_by_project(
        &self,
        project_id: &ProjectId,
        _cancel: CancellationToken,
    ) -> Result<(), MemoryError> {
        let mut map = self.data.lock().unwrap();
        map.retain(|(stored_project, _), _| stored_project != project_id);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// MemoryStoreTestBuilder
// ---------------------------------------------------------------------------

/// Builder for constructing pre-seeded test memory stores.
#[cfg(test)]
pub struct MemoryStoreTestBuilder {
    chunks: Vec<(ProjectId, EmbeddingRecord)>,
    facts: Vec<(ProjectId, String)>,
}

#[cfg(test)]
impl MemoryStoreTestBuilder {
    pub fn new() -> Self {
        Self { chunks: Vec::new(), facts: Vec::new() }
    }
}

#[cfg(test)]
impl Default for MemoryStoreTestBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
impl MemoryStoreTestBuilder {
    pub fn with_chunk(mut self, project_id: ProjectId, chunk: EmbeddingRecord) -> Self {
        self.chunks.push((project_id, chunk));
        self
    }

    pub fn with_fact(mut self, project_id: ProjectId, fact: impl Into<String>) -> Self {
        self.facts.push((project_id, fact.into()));
        self
    }

    /// Build a pre-seeded `InMemoryVectorStore`.
    pub fn build_vector_store(self) -> InMemoryVectorStore {
        InMemoryVectorStore::with_records(self.chunks)
    }

    /// Build a pre-seeded `InMemoryFullTextStore`.
    pub fn build_fts_store(&self) -> InMemoryFullTextStore {
        InMemoryFullTextStore::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_memory_vector_store_roundtrip() {
        let store = InMemoryVectorStore::new();
        let pid = ProjectId("test".into());
        let record = EmbeddingRecord {
            id: "chunk1".into(),
            project_id: pid.clone(),
            chunk_hash: "hash1".into(),
            content: "test content".into(),
            file_path: "main.rs".into(),
            start_line: Some(1),
            end_line: Some(1),
            chunk_type: ChunkType::Function,
            vector: vec![0.1, 0.2, 0.3],
            model_id: "test".into(),
            model_version: "1.0".into(),
            stale: false,
            created_at: time::OffsetDateTime::now_utc(),
        };

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            store.store(&[record], CancellationToken::new()).await.unwrap();
            let results =
                store.search(&pid, &[0.1, 0.2, 0.3], 5, CancellationToken::new()).await.unwrap();
            assert_eq!(results.len(), 1);
            assert_eq!(results[0].chunk_id, "chunk1");
        });
    }

    #[test]
    fn in_memory_fts_search() {
        let store = InMemoryFullTextStore::new();
        let pid = ProjectId("test".into());
        let chunk = MemoryChunk {
            id: "chunk1".into(),
            project_id: pid.clone(),
            namespace: MemoryNamespace::Project(pid.clone()),
            content: "Rust is a systems programming language".into(),
            file_path: None,
            start_line: None,
            end_line: None,
            chunk_type: ChunkType::SlidingWindow,
            score: 1.0,
            model_id: "test".into(),
            model_version: "1.0".into(),
        };

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            store.insert(&chunk, &pid, CancellationToken::new()).await.unwrap();
            let results = store.search("Rust", &pid, 5, CancellationToken::new()).await.unwrap();
            assert_eq!(results.len(), 1);
            assert_eq!(results[0].chunk_id, "chunk1");

            let no_results =
                store.search("Python", &pid, 5, CancellationToken::new()).await.unwrap();
            assert!(no_results.is_empty());
        });
    }
}
