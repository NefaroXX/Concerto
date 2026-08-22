//! Memory write serializer — ensures no concurrent long-term memory writes.
//!
//! Wraps a `MemoryStore` and serialises all `store()` calls via a
//! `tokio::sync::Semaphore` with a single permit. Reads (`retrieve`)
//! and invalidations pass through without acquiring the semaphore.

use std::sync::Arc;

use concerto_core::error::MemoryError;
use concerto_core::memory::{MemoryChunk, MemoryEntry, MemoryId, MemoryQuery};
use concerto_core::traits::memory::MemoryStore;
use concerto_core::CancellationToken;
use tokio::sync::Semaphore;

/// Serialises memory store operations while allowing concurrent reads.
///
/// The semaphore guarantees that only one `store()` call executes at a
/// time, satisfying the Phase 5 ROADMAP requirement that long-term
/// memory writes must not overlap.
pub struct MemoryWriteSerializer {
    inner: Arc<dyn MemoryStore>,
    semaphore: Arc<Semaphore>,
}

impl MemoryWriteSerializer {
    /// Create a new serializer wrapping the given `MemoryStore`.
    pub fn new(inner: Arc<dyn MemoryStore>) -> Self {
        Self { inner, semaphore: Arc::new(Semaphore::new(1)) }
    }

    /// Store a memory entry, serialised with other concurrent stores.
    ///
    /// Acquires the single permit on the internal semaphore before
    /// delegating to the wrapped `MemoryStore::store`.
    pub async fn store(
        &self,
        entry: MemoryEntry,
        _cancel: CancellationToken,
    ) -> Result<MemoryId, MemoryError> {
        let _permit = self.semaphore.acquire().await.map_err(|e| {
            MemoryError::Persistence(format!("failed to acquire store semaphore: {e}"))
        })?;
        self.inner.store(entry, _cancel.clone()).await
    }

    /// Retrieve memory chunks matching the query.
    ///
    /// Passes through directly to the wrapped store without acquiring
    /// the semaphore.
    pub async fn retrieve(
        &self,
        query: &MemoryQuery,
        _cancel: CancellationToken,
    ) -> Result<Vec<MemoryChunk>, MemoryError> {
        self.inner.retrieve(query, _cancel.clone()).await
    }

    /// Invalidate a memory entry by id.
    ///
    /// Passes through directly to the wrapped store without acquiring
    /// the semaphore.
    pub async fn invalidate(
        &self,
        id: MemoryId,
        _cancel: CancellationToken,
    ) -> Result<(), MemoryError> {
        self.inner.invalidate(id, _cancel.clone()).await
    }

    /// Return a reference to the underlying `MemoryStore` as an `Arc`.
    pub fn store_ref(&self) -> Arc<dyn MemoryStore> {
        Arc::clone(&self.inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use concerto_core::ids::Ulid;
    use concerto_core::memory::{MemoryNamespace, ProjectId};
    use std::time::Duration;
    use tokio::time::sleep;

    /// A mock `MemoryStore` that tracks store calls and simulates delay.
    struct MockStore {
        store_count: std::sync::atomic::AtomicUsize,
        delay_ms: u64,
    }

    #[async_trait::async_trait]
    impl MemoryStore for MockStore {
        async fn retrieve(
            &self,
            _query: &MemoryQuery,
            _cancel: CancellationToken,
        ) -> Result<Vec<MemoryChunk>, MemoryError> {
            Ok(vec![])
        }

        async fn store(
            &self,
            _entry: MemoryEntry,
            _cancel: CancellationToken,
        ) -> Result<MemoryId, MemoryError> {
            self.store_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if self.delay_ms > 0 {
                sleep(Duration::from_millis(self.delay_ms)).await;
            }
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

    fn make_entry() -> MemoryEntry {
        MemoryEntry {
            id: MemoryId(Ulid::new()),
            project_id: ProjectId("test".into()),
            namespace: MemoryNamespace::Global { user_id_hash: "test".into() },
            content: "test content".into(),
            chunk_type: concerto_core::memory::ChunkType::Fact,
            model_id: None,
            model_version: None,
            metadata: serde_json::Value::Null,
            expires_at: None,
            created_at: time::OffsetDateTime::now_utc(),
        }
    }

    #[tokio::test]
    async fn store_serialises_concurrent_calls() {
        let store = Arc::new(MockStore {
            store_count: std::sync::atomic::AtomicUsize::new(0),
            delay_ms: 50,
        });
        let serializer = MemoryWriteSerializer::new(store.clone());

        let entry1 = make_entry();
        let entry2 = make_entry();

        let start = std::time::Instant::now();
        let (r1, r2) = tokio::join!(
            serializer.store(entry1, CancellationToken::new()),
            serializer.store(entry2, CancellationToken::new()),
        );
        let elapsed = start.elapsed();

        // Both should succeed
        assert!(r1.is_ok());
        assert!(r2.is_ok());

        // With 50ms delay each, serialised execution should take at least 100ms
        assert!(elapsed >= Duration::from_millis(100));

        let count = store.store_count.load(std::sync::atomic::Ordering::SeqCst);
        assert_eq!(count, 2);
    }

    #[tokio::test]
    async fn retrieve_passes_through_without_semaphore() {
        let store = Arc::new(MockStore {
            store_count: std::sync::atomic::AtomicUsize::new(0),
            delay_ms: 0,
        });
        let serializer = MemoryWriteSerializer::new(store);

        let query = MemoryQuery {
            text: "test".into(),
            project_id: ProjectId("test".into()),
            namespace: MemoryNamespace::Global { user_id_hash: "test".into() },
            top_k: 5,
            filters: vec![],
        };

        let result = serializer.retrieve(&query, CancellationToken::new()).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn invalidate_passes_through_without_semaphore() {
        let store = Arc::new(MockStore {
            store_count: std::sync::atomic::AtomicUsize::new(0),
            delay_ms: 0,
        });
        let serializer = MemoryWriteSerializer::new(store);

        let id = MemoryId(Ulid::new());
        let result = serializer.invalidate(id, CancellationToken::new()).await;
        assert!(result.is_ok());
    }
}
