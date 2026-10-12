//! Vector similarity store trait.
//!
//! Abstracts vector persistence and ANN / brute-force search behind a
//! single trait so backends (SQLite, plug-in) are swappable.

use async_trait::async_trait;
use camino::Utf8PathBuf;

use crate::error::MemoryError;
use crate::memory::{EmbeddingRecord, MemoryChunk, ProjectId, VectorResult};
use crate::CancellationToken;

/// Information about a stored row needed for lazy re-index decisions.
#[derive(Debug, Clone)]
pub struct RowIndexFact {
    /// The chunk id.
    pub id: String,
    /// The model version that produced this row.
    pub model_version: String,
    /// `true` if this is an ADR-39 FTS-only sentinel (empty vector, no metadata).
    pub is_sentinel: bool,
}

/// Recall-visible provenance of one stored row (H11 TTL + authorship
/// substrate): when the row was written and what its metadata sidecar
/// says. Rows whose timestamp cannot be parsed, or backends that do not
/// track provenance, report `created_at: None` — the recall derivation
/// then reads TTL as unknown rather than inventing an age.
#[derive(Debug, Clone)]
pub struct ChunkProvenance {
    /// When the row was written (`None` when unknown or unparseable).
    pub created_at: Option<time::OffsetDateTime>,
    /// The row's JSON metadata sidecar (`None` when the row carries none).
    pub metadata: Option<serde_json::Value>,
}

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

    /// Mark stale every vector whose embedding model version no longer
    /// matches the current one (`model_version != current_model_version`) — a
    /// model bump leaves the CURRENT version's rows fresh and everything
    /// produced by earlier versions gets re-indexed.
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

    /// Return per-row index facts (id, model version, sentinel-ness) for a
    /// project.  Used by lazy re-indexing to skip chunks that are already at
    /// the live embedding model version.
    ///
    /// Backends that do not yet implement this return an empty list (the
    /// default), which makes lazy re-indexing fall back to re-embedding
    /// everything — correct, just not lazy.
    async fn row_index_facts(
        &self,
        _project_id: &ProjectId,
        _cancel: CancellationToken,
    ) -> Result<Vec<RowIndexFact>, MemoryError> {
        Ok(Vec::new())
    }

    /// Return recall-visible provenance for the given chunk ids, keyed by
    /// chunk id (H11 TTL + authorship substrate). Absent ids simply have no
    /// entry — the recall derivation reads them as unknown provenance.
    ///
    /// Backends that do not track provenance return an empty map (the
    /// default); recall stays honest by reporting unknown rather than
    /// inventing ages. Callers pass at most one recall's ids (bounded by
    /// `top_k`), so this is one bounded lookup, never a table scan.
    async fn chunk_provenance(
        &self,
        _project_id: &ProjectId,
        _chunk_ids: &[String],
        _cancel: CancellationToken,
    ) -> Result<std::collections::HashMap<String, ChunkProvenance>, MemoryError> {
        Ok(std::collections::HashMap::new())
    }
}
