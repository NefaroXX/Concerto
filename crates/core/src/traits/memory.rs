//! Memory store contract — the shared trait for all memory backends.
//!
//! Implementations include layered project memory with SQLite FTS5/vector
//! hybrid retrieval and the crate-level working/persistent stores. The trait is
//! intentionally minimal because both single- and multi-agent runtimes depend
//! on this contract.

use crate::error::MemoryError;
use crate::ids::Ulid;
use crate::memory::{MemoryChunk, MemoryEntry, MemoryId, MemoryQuery, ProjectId};
use crate::CancellationToken;
use async_trait::async_trait;

#[async_trait]
pub trait MemoryStore: Send + Sync {
    async fn retrieve(
        &self,
        query: &MemoryQuery,
        cancel: CancellationToken,
    ) -> Result<Vec<MemoryChunk>, MemoryError>;
    /// Browse recent project chunks without requiring a search term.
    async fn browse(
        &self,
        _project_id: &ProjectId,
        _top_k: usize,
        _cancel: CancellationToken,
    ) -> Result<Vec<MemoryChunk>, MemoryError> {
        Ok(Vec::new())
    }
    async fn store(
        &self,
        entry: MemoryEntry,
        cancel: CancellationToken,
    ) -> Result<MemoryId, MemoryError>;
    async fn invalidate(&self, id: MemoryId, cancel: CancellationToken) -> Result<(), MemoryError>;
    /// Invalidate an indexed chunk whose identifier may not be a ULID.
    async fn invalidate_chunk(
        &self,
        id: &str,
        cancel: CancellationToken,
    ) -> Result<(), MemoryError> {
        let ulid = Ulid::from_string(id).map_err(|_| MemoryError::NotFound(id.to_string()))?;
        self.invalidate(MemoryId(ulid), cancel).await
    }
}

/// A memory store that does nothing — always returns empty results and
/// silently discards writes. Useful for tests and eval benchmarks where
/// memory persistence is not needed.
#[derive(Default)]
pub struct NullMemoryStore;

#[async_trait]
impl MemoryStore for NullMemoryStore {
    async fn retrieve(
        &self,
        _query: &MemoryQuery,
        _cancel: CancellationToken,
    ) -> Result<Vec<MemoryChunk>, MemoryError> {
        Ok(Vec::new())
    }

    async fn store(
        &self,
        _entry: MemoryEntry,
        _cancel: CancellationToken,
    ) -> Result<MemoryId, MemoryError> {
        Ok(MemoryId(Ulid::new()))
    }

    async fn invalidate(
        &self,
        _id: MemoryId,
        _cancel: CancellationToken,
    ) -> Result<(), MemoryError> {
        Ok(())
    }
}
