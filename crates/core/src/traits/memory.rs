//! Memory store contract — the shared trait for all memory backends.
//!
//! Implementations include layered project memory with SQLite FTS5/vector
//! hybrid retrieval and the crate-level working/persistent stores. The trait is
//! intentionally minimal because both single- and multi-agent runtimes depend
//! on this contract.

use crate::error::MemoryError;
use crate::ids::Ulid;
use crate::memory::{
    ChunkType, Confidence, EvidentChunk, FactStatusView, MemoryChunk, MemoryEntry, MemoryId,
    MemoryQuery, PathObservationSource, ProjectId, RecallSource,
};
use crate::CancellationToken;
use async_trait::async_trait;
use std::sync::Arc;

#[async_trait]
pub trait MemoryStore: Send + Sync {
    async fn retrieve(
        &self,
        query: &MemoryQuery,
        cancel: CancellationToken,
    ) -> Result<Vec<MemoryChunk>, MemoryError>;
    /// Attach the resource-fact observation seam for evidence-bearing
    /// recall (H11): a clean runtime observation verifies an item, a dirty
    /// one stales it, both cited in the derivation.
    ///
    /// Additive by design — the default is a no-op, so every existing
    /// implementor (null/global/proxy/test doubles) compiles unchanged
    /// and keeps reading the observation axis as unknown. Only the memory
    /// system overrides this (interior mutability: safe to call on a
    /// shared `Arc` at any time, including after construction, so call
    /// sites that only hold the erased `Arc<dyn MemoryStore>` handle can
    /// attach without reshaping any constructor signature). Attaching
    /// twice replaces the provider; detaching is not supported (attach
    /// the seam or leave it absent — there is no third state).
    fn attach_path_observations(&self, _source: Arc<dyn PathObservationSource>) {}
    /// Evidence-bearing recall (H11): like [`Self::retrieve`], but every
    /// result carries source attribution and status rather than a bare
    /// ranked chunk.
    ///
    /// The default impl projects [`Self::retrieve`] results with the
    /// honesty a standing-free store can offer: the source from the
    /// always-knowable kind/path rule, model freshness from the chunk's
    /// own stale flag, and [`FactStatusView::Unknown`] (or `Assumed` for
    /// model-authored claims) — never a default that reads as fine. Stores
    /// with real substrate (the memory system's link/TTL/decision stores)
    /// override this with deterministic derivation; agents must prefer
    /// this method over [`Self::retrieve`] wherever it is available.
    async fn retrieve_with_evidence(
        &self,
        query: &MemoryQuery,
        cancel: CancellationToken,
    ) -> Result<Vec<EvidentChunk>, MemoryError> {
        Ok(project_without_standing(self.retrieve(query, cancel).await?))
    }
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

    /// Whether this store carries an ADR-69 symbolic link store.
    ///
    /// Fail-open by construction: `false` for stores without one, so tests
    /// that do not opt in keep fanning out plain. Implementations with a
    /// link store override to `true`, so the production init path can be
    /// asserted through an `Arc<dyn MemoryStore>` handle without downcasting
    /// to the concrete system (the memory crate's `MemorySystem` reports the
    /// presence of the link store its builder attached).
    fn link_store_attached(&self) -> bool {
        false
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

/// Project bare chunks into evidence-bearing results with the honesty a
/// standing-free store can offer (H11): the source from the
/// always-knowable kind/path rule, model freshness from each chunk's own
/// stale flag, and [`FactStatusView::Unknown`] — or `Assumed` for
/// model-authored chunks, which are claims by construction — never a
/// default that reads as fine. Shared by the
/// [`MemoryStore::retrieve_with_evidence`] default impl and stores that
/// do not implement [`MemoryStore`] (e.g. the global memory store, whose
/// project-system routing applies it).
#[must_use]
pub fn project_without_standing(chunks: Vec<MemoryChunk>) -> Vec<EvidentChunk> {
    chunks
        .into_iter()
        .map(|chunk| {
            let source = RecallSource::for_chunk(
                chunk.chunk_type,
                chunk.file_path.as_ref().map(|path| path.as_str()),
            );
            // Authorship alone establishes claim-ness (a model-authored
            // chunk is a claim by construction); anything else whose
            // status no substrate establishes is Unknown.
            let truth = match (source, chunk.chunk_type) {
                (RecallSource::ModelAuthored, ChunkType::Fact | ChunkType::SessionSummary) => {
                    FactStatusView::Assumed
                }
                _ => FactStatusView::Unknown,
            };
            let derivation = match truth {
                FactStatusView::Assumed => {
                    "model-authored claim; this store holds no supporting evidence".to_string()
                }
                _ => "status not established by this store (no standing substrate)".to_string(),
            };
            // Model freshness from the chunk's own stale flag only —
            // rank (`chunk.score`) never enters freshness here.
            let freshness = if chunk.stale { 0.0 } else { 1.0 };
            EvidentChunk {
                chunk,
                truth,
                source,
                confidence: Confidence::Unknown,
                freshness,
                derivation,
                degraded: false,
                interrupted: false,
            }
        })
        .collect()
}
