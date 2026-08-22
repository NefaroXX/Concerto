//! Vector similarity store trait.
//!
//! Abstracts vector persistence and ANN / brute-force search behind a
//! single trait so backends (SQLite, plug-in) are swappable.

use async_trait::async_trait;
use camino::Utf8PathBuf;

use crate::error::MemoryError;
use crate::memory::{EmbeddingRecord, MemoryChunk, ProjectId, VectorResult};
use crate::CancellationToken;

/// Vector similarity store.
///
/// Implementations must support:
/// - Per-project namespace isolation (vectors from project A must
///   never appear in project B queries).
/// - Per-vector metadata (model version, chunk hash).
/// - Tombstone (soft delete) for file deletions.
/// - Staleness marking for model version changes.
#[async_trait]
pub trait VectorStore: Send + Sync {
    /// Store embedding records.
    async fn store(
        &self,
        records: &[EmbeddingRecord],
        cancel: CancellationToken,
    ) -> Result<(), MemoryError>;

    /// Search for the top-k most similar vectors to `query` within
    /// the given project namespace.
    async fn search(
        &self,
        project_id: &ProjectId,
        query: &[f32],
        top_k: usize,
        cancel: CancellationToken,
    ) -> Result<Vec<VectorResult>, MemoryError>;

    /// List current chunks for browsing in management UIs.
    async fn list(
        &self,
        _project_id: &ProjectId,
        _top_k: usize,
        _cancel: CancellationToken,
    ) -> Result<Vec<VectorResult>, MemoryError> {
        Ok(Vec::new())
    }

    /// Load complete metadata for specific chunks. Backends that do not yet
    /// support metadata lookup may return an empty list; the canonical SQLite
    /// backend implements this for attributed retrieval and filtering.
    async fn get_chunks(
        &self,
        _project_id: &ProjectId,
        _chunk_ids: &[String],
        _cancel: CancellationToken,
    ) -> Result<Vec<MemoryChunk>, MemoryError> {
        Ok(Vec::new())
    }

    /// Whether `get_chunks` returns authoritative metadata for this backend.
    fn supports_chunk_metadata(&self) -> bool {
        false
    }

    /// Soft-delete a chunk (tombstone) so it no longer appears in
    /// search results.
    async fn tombstone(
        &self,
        chunk_id: &str,
        project_id: &ProjectId,
        cancel: CancellationToken,
    ) -> Result<(), MemoryError>;

    /// Permanently remove all tombstoned vectors for a project.
    async fn delete_tombstoned(
        &self,
        project_id: &ProjectId,
        cancel: CancellationToken,
    ) -> Result<(), MemoryError>;

    /// Mark all vectors stale for a project (triggered by model
    /// version mismatch).
    async fn mark_stale(
        &self,
        project_id: &ProjectId,
        model_version: &str,
        cancel: CancellationToken,
    ) -> Result<(), MemoryError>;

    /// Remove all vectors for a project (used when a project is
    /// explicitly removed from the system).
    async fn delete_by_project(
        &self,
        project_id: &ProjectId,
        cancel: CancellationToken,
    ) -> Result<(), MemoryError>;

    /// Remove all vectors for a specific file in a project. Returns the
    /// chunk ids that were removed so the FTS index can be purged too.
    /// Used by incremental re-indexing so a changed file does not leave
    /// orphaned stale chunks behind.
    async fn delete_by_file_path(
        &self,
        project_id: &ProjectId,
        file_path: &Utf8PathBuf,
        cancel: CancellationToken,
    ) -> Result<Vec<String>, MemoryError>;
}
