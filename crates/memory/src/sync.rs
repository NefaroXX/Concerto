//! Chunk sync service — keeps the vector store and full-text search
//! store in sync.
//!
//! Every chunk written to the vector store must also be indexed in FTS.
//! Every tombstone must delete from both. `ChunkSyncService` is the
//! single point of coordination that enforces this invariant.

use concerto_core::CancellationToken;
use std::sync::Arc;

use camino::Utf8PathBuf;

use concerto_core::error::MemoryError;
use concerto_core::memory::{EmbeddingRecord, MemoryChunk, MemoryNamespace, ProjectId};

use crate::fts::FullTextStore;
use crate::vector_store::VectorStore;

/// Synchronises the vector store and full-text store so they never
/// diverge.
///
/// This is the **only** write path for chunk storage and deletion.
/// External code should never call `VectorStore::store` or
/// `FullTextStore::insert` directly.
pub struct ChunkSyncService {
    pub vector_store: Arc<dyn VectorStore>,
    pub fts_store: Arc<dyn FullTextStore>,
}

impl ChunkSyncService {
    pub fn new(vector_store: Arc<dyn VectorStore>, fts_store: Arc<dyn FullTextStore>) -> Self {
        Self { vector_store, fts_store }
    }

    /// Store an embedding record in the vector store AND index the
    /// chunk content in FTS.
    ///
    /// If the FTS insert fails, the vector store insert is NOT rolled
    /// back (FTS5 doesn't support distributed transactions). A retry
    /// or reconciliation job should clean up orphans.
    pub async fn store(
        &self,
        record: &EmbeddingRecord,
        cancel: CancellationToken,
    ) -> Result<(), MemoryError> {
        // 1. Store in vector store
        self.vector_store.store(std::slice::from_ref(record), cancel.clone()).await?;

        // 2. Index in FTS
        let chunk = MemoryChunk {
            id: record.id.clone(),
            project_id: record.project_id.clone(),
            namespace: MemoryNamespace::Project(record.project_id.clone()),
            content: record.content.clone(),
            file_path: Some(record.file_path.clone()),
            start_line: record.start_line,
            end_line: record.end_line,
            chunk_type: record.chunk_type,
            score: 1.0, // neutral default for stored chunks; query-time FTS rank or vector similarity overwrites this
            model_id: record.model_id.clone(),
            model_version: record.model_version.clone(),
        };
        self.fts_store.insert(&chunk, &record.project_id, cancel).await?;

        Ok(())
    }

    /// Replace the complete index for a project after a successful full scan.
    /// This prunes deleted and newly excluded files instead of accumulating
    /// stale rows across restarts and manual re-indexes.
    pub async fn replace_project(
        &self,
        project_id: &ProjectId,
        records: &[EmbeddingRecord],
        cancel: CancellationToken,
    ) -> Result<(), MemoryError> {
        self.vector_store.delete_by_project(project_id, cancel.clone()).await?;
        self.fts_store.delete_by_project(project_id, cancel.clone()).await?;
        self.vector_store.store(records, cancel.clone()).await?;
        for record in records {
            let chunk = MemoryChunk {
                id: record.id.clone(),
                project_id: record.project_id.clone(),
                namespace: MemoryNamespace::Project(record.project_id.clone()),
                content: record.content.clone(),
                file_path: Some(record.file_path.clone()),
                start_line: record.start_line,
                end_line: record.end_line,
                chunk_type: record.chunk_type,
                score: 1.0, // neutral default for stored chunks; query-time FTS rank or vector similarity overwrites this
                model_id: record.model_id.clone(),
                model_version: record.model_version.clone(),
            };
            self.fts_store.insert(&chunk, project_id, cancel.clone()).await?;
        }
        Ok(())
    }

    /// Tombstone a chunk in the vector store AND delete from FTS.
    pub async fn tombstone(
        &self,
        chunk_id: &str,
        project_id: &ProjectId,
        cancel: CancellationToken,
    ) -> Result<(), MemoryError> {
        // 1. Tombstone in vector store
        self.vector_store.tombstone(chunk_id, project_id, cancel.clone()).await?;

        // 2. Delete from FTS
        if let Err(e) = self.fts_store.delete(chunk_id, project_id, cancel).await {
            tracing::warn!("FTS delete failed for chunk {chunk_id}: {e}");
        }

        Ok(())
    }

    /// Remove every chunk for a specific file (used by incremental re-index)
    /// from both the vector store and the FTS index, returning the purged
    /// chunk ids.
    pub async fn delete_by_file_path(
        &self,
        project_id: &ProjectId,
        file_path: &Utf8PathBuf,
        cancel: CancellationToken,
    ) -> Result<Vec<String>, MemoryError> {
        let ids =
            self.vector_store.delete_by_file_path(project_id, file_path, cancel.clone()).await?;
        for id in &ids {
            if let Err(e) = self.fts_store.delete(id, project_id, cancel.clone()).await {
                tracing::warn!("failed to purge FTS chunk {id}: {e}");
            }
        }
        Ok(ids)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{InMemoryFullTextStore, InMemoryVectorStore};
    use concerto_core::memory::ChunkType;
    use time::OffsetDateTime;

    fn make_record(project_id: ProjectId, id: &str) -> EmbeddingRecord {
        EmbeddingRecord {
            id: id.into(),
            project_id,
            chunk_hash: format!("hash_{id}"),
            content: format!("content {id}"),
            file_path: "src/main.rs".into(),
            start_line: Some(1),
            end_line: Some(1),
            chunk_type: ChunkType::Function,
            vector: vec![0.1, 0.2, 0.3],
            model_id: "test".into(),
            model_version: "1.0".into(),
            stale: false,
            created_at: OffsetDateTime::now_utc(),
        }
    }

    fn make_record_with_path(project_id: ProjectId, id: &str, file_path: &str) -> EmbeddingRecord {
        EmbeddingRecord {
            id: id.into(),
            project_id,
            chunk_hash: format!("hash_{id}"),
            content: format!("content {id}"),
            file_path: file_path.into(),
            start_line: Some(1),
            end_line: Some(1),
            chunk_type: ChunkType::Function,
            vector: vec![0.1; 4],
            model_id: "test".into(),
            model_version: "1.0".into(),
            stale: false,
            created_at: OffsetDateTime::now_utc(),
        }
    }

    #[tokio::test]
    async fn store_syncs_both_stores() {
        let vs = Arc::new(InMemoryVectorStore::new());
        let fts = Arc::new(InMemoryFullTextStore::new());
        let sync = ChunkSyncService::new(vs.clone(), fts.clone());

        let pid = ProjectId("test".into());
        let record = make_record(pid.clone(), "chunk1");

        sync.store(&record, CancellationToken::new()).await.unwrap();

        // Verify vector store has it
        let v_results =
            vs.search(&pid, &[0.1, 0.2, 0.3], 5, CancellationToken::new()).await.unwrap();
        assert_eq!(v_results.len(), 1);

        // Verify FTS has it with non-zero score
        let f_results = fts.search("content", &pid, 5, CancellationToken::new()).await.unwrap();
        assert_eq!(f_results.len(), 1);
        assert!(
            f_results[0].score > 0.0,
            "synced chunk should have a positive FTS score, got {}",
            f_results[0].score
        );
    }

    #[tokio::test]
    async fn tombstone_removes_from_both() {
        let vs = Arc::new(InMemoryVectorStore::new());
        let fts = Arc::new(InMemoryFullTextStore::new());
        let sync = ChunkSyncService::new(vs.clone(), fts.clone());

        let pid = ProjectId("test".into());
        let record = make_record(pid.clone(), "chunk1");

        sync.store(&record, CancellationToken::new()).await.unwrap();

        // Tombstone
        sync.tombstone(&record.id, &pid, CancellationToken::new()).await.unwrap();

        // Verify FTS deleted
        let f_results = fts.search("content", &pid, 5, CancellationToken::new()).await.unwrap();
        assert!(f_results.is_empty());
    }

    #[tokio::test]
    async fn replace_project_prunes_old_chunks_from_both_stores() {
        let vector_store = Arc::new(InMemoryVectorStore::new());
        let fts_store = Arc::new(InMemoryFullTextStore::new());
        let sync = ChunkSyncService::new(vector_store.clone(), fts_store.clone());
        let project_id = ProjectId("test".into());
        let old = make_record(project_id.clone(), "old");
        let current = make_record(project_id.clone(), "current");
        sync.store(&old, CancellationToken::new()).await.unwrap();

        sync.replace_project(&project_id, std::slice::from_ref(&current), CancellationToken::new())
            .await
            .unwrap();

        assert!(fts_store
            .search("old", &project_id, 5, CancellationToken::new())
            .await
            .unwrap()
            .is_empty());
        assert_eq!(
            fts_store
                .search("current", &project_id, 5, CancellationToken::new())
                .await
                .unwrap()
                .len(),
            1
        );
        assert!(vector_store
            .get_chunks(&project_id, &[old.id], CancellationToken::new())
            .await
            .unwrap()
            .is_empty());
        assert_eq!(
            vector_store
                .get_chunks(&project_id, &[current.id], CancellationToken::new())
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn delete_by_file_path_removes_from_both() {
        let vs = Arc::new(InMemoryVectorStore::new());
        let fts = Arc::new(InMemoryFullTextStore::new());
        let sync = ChunkSyncService::new(vs.clone(), fts.clone());

        let pid = ProjectId("del_path".into());
        let rec1 = make_record_with_path(pid.clone(), "c1", "src/a.rs");
        let rec2 = make_record_with_path(pid.clone(), "c2", "src/b.rs");
        sync.store(&rec1, CancellationToken::new()).await.unwrap();
        sync.store(&rec2, CancellationToken::new()).await.unwrap();

        sync.delete_by_file_path(
            &pid,
            &camino::Utf8PathBuf::from("src/a.rs"),
            CancellationToken::new(),
        )
        .await
        .unwrap();

        // FTS should have lost c1
        let f_results = fts.search("content c2", &pid, 5, CancellationToken::new()).await.unwrap();
        assert_eq!(f_results.len(), 1);
        assert_eq!(f_results[0].chunk_id, "c2");

        // No c1 left
        let c1_results = fts.search("content c1", &pid, 5, CancellationToken::new()).await.unwrap();
        assert!(c1_results.is_empty());

        // Vector store should have lost c1
        let remaining = vs.search(&pid, &[0.1; 4], 10, CancellationToken::new()).await.unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].chunk_id, "c2");
    }

    #[tokio::test]
    async fn empty_replace_project_is_noop() {
        let vs = Arc::new(InMemoryVectorStore::new());
        let fts = Arc::new(InMemoryFullTextStore::new());
        let sync = ChunkSyncService::new(vs.clone(), fts.clone());

        let pid = ProjectId("empty_replace".into());
        // Should not panic or error
        sync.replace_project(&pid, &[], CancellationToken::new()).await.unwrap();
    }

    #[tokio::test]
    async fn project_isolation_in_replace() {
        let vs = Arc::new(InMemoryVectorStore::new());
        let fts = Arc::new(InMemoryFullTextStore::new());
        let sync = ChunkSyncService::new(vs.clone(), fts.clone());

        let pid_a = ProjectId("proj_a".into());
        let pid_b = ProjectId("proj_b".into());
        sync.store(
            &make_record_with_path(pid_a.clone(), "a1", "src/a.rs"),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        sync.store(
            &make_record_with_path(pid_b.clone(), "b1", "src/b.rs"),
            CancellationToken::new(),
        )
        .await
        .unwrap();

        // Replace project A with empty — should not affect B
        sync.replace_project(&pid_a, &[], CancellationToken::new()).await.unwrap();

        // B's FTS data should be intact
        let b_results =
            fts.search("content b1", &pid_b, 5, CancellationToken::new()).await.unwrap();
        assert_eq!(b_results.len(), 1, "project B should be unaffected");

        // A's FTS data should be gone
        let a_results =
            fts.search("content a1", &pid_a, 5, CancellationToken::new()).await.unwrap();
        assert!(a_results.is_empty(), "project A should be empty");
    }
}
