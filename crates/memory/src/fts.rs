//! Full-text search store trait and result type.
//!
//! The `FullTextStore` trait abstracts BM25 full-text search behind a
//! simple async interface. The primary implementation is
//! `Sqlite5FullTextStore` using SQLite FTS5.
//!
//! This trait lives in its own module so both the vector store and RAG
//! pipeline can import it without circular dependencies.

use async_trait::async_trait;
use concerto_core::CancellationToken;

use concerto_core::error::MemoryError;
use concerto_core::memory::{FtsResult, MemoryChunk, ProjectId};

/// Full-text search store — BM25 via SQLite FTS5.
///
/// Every chunk stored in the vector store must also be indexed here so
/// the hybrid retriever can combine vector similarity + BM25 scores.
/// The `ChunkSyncService` in `sync.rs` wraps both stores and keeps them
/// in sync.
#[async_trait]
pub trait FullTextStore: Send + Sync {
    /// Index a chunk for full-text search.
    async fn insert(
        &self,
        chunk: &MemoryChunk,
        project_id: &ProjectId,
        cancel: CancellationToken,
    ) -> Result<(), MemoryError>;

    /// Remove a chunk from the FTS index.
    async fn delete(
        &self,
        chunk_id: &str,
        project_id: &ProjectId,
        cancel: CancellationToken,
    ) -> Result<(), MemoryError>;

    /// Search for chunks matching `query` using BM25 ranking.
    async fn search(
        &self,
        query: &str,
        project_id: &ProjectId,
        top_k: usize,
        cancel: CancellationToken,
    ) -> Result<Vec<FtsResult>, MemoryError>;

    /// Remove all chunks for a project (used when a project is removed).
    async fn delete_by_project(
        &self,
        project_id: &ProjectId,
        cancel: CancellationToken,
    ) -> Result<(), MemoryError>;
}

use sqlx::{Row, SqlitePool};

/// SQLite‑backed full‑text search store using the FTS5 extension.
///
/// The store creates a virtual table `fts_store` with three columns:
/// * `chunk_id` – the identifier of the memory chunk (UNINDEXED).
/// * `project_id` – the owning project identifier (UNINDEXED).
/// * `content` – the full text that will be indexed by FTS5.
///
/// The table is created on construction if it does not already exist.
/// All operations are performed asynchronously via a shared `SqlitePool`.
pub struct SqliteFullTextStore {
    pool: SqlitePool,
}

impl SqliteFullTextStore {
    /// Create a new store backed by the given SQLite connection pool.
    ///
    /// This will ensure the required FTS5 virtual table exists.
    pub async fn new(pool: SqlitePool) -> Result<Self, MemoryError> {
        sqlx::query(
            "CREATE VIRTUAL TABLE IF NOT EXISTS fts_store USING fts5(
                chunk_id UNINDEXED,
                project_id UNINDEXED,
                content
            );",
        )
        .execute(&pool)
        .await
        .map_err(|e| MemoryError::Persistence(e.to_string()))?;
        Ok(Self { pool })
    }
}

#[async_trait]
impl FullTextStore for SqliteFullTextStore {
    async fn insert(
        &self,
        chunk: &MemoryChunk,
        project_id: &ProjectId,
        _cancel: CancellationToken,
    ) -> Result<(), MemoryError> {
        let mut transaction =
            self.pool.begin().await.map_err(|error| MemoryError::Persistence(error.to_string()))?;
        sqlx::query("DELETE FROM fts_store WHERE chunk_id = ? AND project_id = ?")
            .bind(&chunk.id)
            .bind(&project_id.0)
            .execute(&mut *transaction)
            .await
            .map_err(|error| MemoryError::Persistence(error.to_string()))?;
        sqlx::query("INSERT INTO fts_store (chunk_id, project_id, content) VALUES (?, ?, ?)")
            .bind(&chunk.id)
            .bind(&project_id.0)
            .bind(&chunk.content)
            .execute(&mut *transaction)
            .await
            .map_err(|error| MemoryError::Persistence(error.to_string()))?;
        transaction.commit().await.map_err(|error| MemoryError::Persistence(error.to_string()))?;
        Ok(())
    }

    async fn delete(
        &self,
        chunk_id: &str,
        project_id: &ProjectId,
        _cancel: CancellationToken,
    ) -> Result<(), MemoryError> {
        sqlx::query("DELETE FROM fts_store WHERE chunk_id = ? AND project_id = ?")
            .bind(chunk_id)
            .bind(&project_id.0)
            .execute(&self.pool)
            .await
            .map_err(|e| MemoryError::Persistence(e.to_string()))?;
        Ok(())
    }

    async fn search(
        &self,
        query: &str,
        project_id: &ProjectId,
        top_k: usize,
        _cancel: CancellationToken,
    ) -> Result<Vec<FtsResult>, MemoryError> {
        // FTS5 rejects empty query strings; return empty results early.
        if query.trim().is_empty() {
            return Ok(Vec::new());
        }

        // FTS5 `rank` returns negative values for BM25 relevance
        // (closer to 0 = more relevant). We negate it so higher is better.
        let rows = sqlx::query(
            "SELECT chunk_id, content, rank FROM fts_store WHERE fts_store MATCH ? AND project_id = ? ORDER BY rank LIMIT ?",
        )
        .bind(query)
        .bind(&project_id.0)
        .bind(top_k as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| MemoryError::RetrievalFailed(e.to_string()))?;

        let results = rows
            .into_iter()
            .map(|row| {
                let rank: f64 = row.get("rank");
                FtsResult {
                    chunk_id: row.get::<String, _>("chunk_id"),
                    score: -rank,
                    content: row.get::<String, _>("content"),
                }
            })
            .collect();
        Ok(results)
    }

    async fn delete_by_project(
        &self,
        project_id: &ProjectId,
        _cancel: CancellationToken,
    ) -> Result<(), MemoryError> {
        sqlx::query("DELETE FROM fts_store WHERE project_id = ?")
            .bind(&project_id.0)
            .execute(&self.pool)
            .await
            .map_err(|e| MemoryError::Persistence(e.to_string()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use concerto_core::memory::{ChunkType, MemoryNamespace};
    use sqlx::sqlite::SqlitePoolOptions;

    fn make_chunk(id: &str, content: &str, project_id: &ProjectId) -> MemoryChunk {
        MemoryChunk {
            id: id.into(),
            project_id: project_id.clone(),
            namespace: MemoryNamespace::Project(project_id.clone()),
            content: content.into(),
            file_path: Some("src/lib.rs".into()),
            start_line: Some(1),
            end_line: Some(1),
            chunk_type: ChunkType::Function,
            score: 0.0,
            model_id: "test".into(),
            model_version: "1".into(),
        }
    }

    async fn make_store() -> (SqliteFullTextStore, SqlitePool) {
        let pool =
            SqlitePoolOptions::new().max_connections(1).connect("sqlite::memory:").await.unwrap();
        let store = SqliteFullTextStore::new(pool.clone()).await.unwrap();
        (store, pool)
    }

    #[tokio::test]
    async fn reinserting_a_chunk_replaces_the_fts_row() {
        let (store, _pool) = make_store().await;
        let project_id = ProjectId("project".into());
        let mut chunk = make_chunk("chunk", "old searchable phrase", &project_id);
        store.insert(&chunk, &project_id, CancellationToken::new()).await.unwrap();
        chunk.content = "new searchable phrase".into();
        store.insert(&chunk, &project_id, CancellationToken::new()).await.unwrap();

        assert!(store
            .search("old", &project_id, 5, CancellationToken::new())
            .await
            .unwrap()
            .is_empty());
        assert_eq!(
            store.search("new", &project_id, 5, CancellationToken::new()).await.unwrap().len(),
            1
        );
    }

    #[tokio::test]
    async fn search_empty_store_returns_empty() {
        let (store, _pool) = make_store().await;
        let project_id = ProjectId("empty".into());
        let results =
            store.search("anything", &project_id, 5, CancellationToken::new()).await.unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn search_project_isolation() {
        let (store, _pool) = make_store().await;
        let proj_a = ProjectId("proj_a".into());
        let proj_b = ProjectId("proj_b".into());
        store
            .insert(
                &make_chunk("c1", "unique content for A", &proj_a),
                &proj_a,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        store
            .insert(
                &make_chunk("c2", "unique content for B", &proj_b),
                &proj_b,
                CancellationToken::new(),
            )
            .await
            .unwrap();

        assert_eq!(
            store.search("unique", &proj_a, 5, CancellationToken::new()).await.unwrap().len(),
            1
        );
        assert_eq!(
            store.search("unique", &proj_b, 5, CancellationToken::new()).await.unwrap().len(),
            1
        );
    }

    #[tokio::test]
    async fn delete_by_project_purges_all() {
        let (store, _pool) = make_store().await;
        let project_id = ProjectId("delete_me".into());
        store
            .insert(
                &make_chunk("c1", "content one", &project_id),
                &project_id,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        store
            .insert(
                &make_chunk("c2", "content two", &project_id),
                &project_id,
                CancellationToken::new(),
            )
            .await
            .unwrap();

        store.delete_by_project(&project_id, CancellationToken::new()).await.unwrap();
        let results =
            store.search("content", &project_id, 5, CancellationToken::new()).await.unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn delete_individual_chunk() {
        let (store, _pool) = make_store().await;
        let project_id = ProjectId("del".into());
        store
            .insert(
                &make_chunk("keep", "keep this", &project_id),
                &project_id,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        store
            .insert(
                &make_chunk("remove", "remove this", &project_id),
                &project_id,
                CancellationToken::new(),
            )
            .await
            .unwrap();

        store.delete("remove", &project_id, CancellationToken::new()).await.unwrap();

        let results = store.search("this", &project_id, 5, CancellationToken::new()).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].chunk_id, "keep");
    }

    #[tokio::test]
    async fn search_top_k_limits_results() {
        let (store, _pool) = make_store().await;
        let project_id = ProjectId("topk".into());
        for i in 0..5 {
            let id = format!("c{i}");
            store
                .insert(
                    &make_chunk(&id, "searchable text", &project_id),
                    &project_id,
                    CancellationToken::new(),
                )
                .await
                .unwrap();
        }

        let results =
            store.search("searchable", &project_id, 3, CancellationToken::new()).await.unwrap();
        assert_eq!(results.len(), 3);
    }

    #[tokio::test]
    async fn search_ranking_better_match_first() {
        let (store, _pool) = make_store().await;
        let project_id = ProjectId("rank".into());
        store
            .insert(
                &make_chunk("less", "unique rare terms here", &project_id),
                &project_id,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        store
            .insert(
                &make_chunk("more", "unique rare terms appear repeatedly", &project_id),
                &project_id,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let results = store
            .search("unique rare terms", &project_id, 5, CancellationToken::new())
            .await
            .unwrap();
        assert!(!results.is_empty());
    }

    #[tokio::test]
    async fn insert_same_id_twice_does_not_duplicate() {
        let (store, _pool) = make_store().await;
        let project_id = ProjectId("dedup".into());
        let chunk = make_chunk("dup", "deduplication test", &project_id);
        store.insert(&chunk, &project_id, CancellationToken::new()).await.unwrap();
        store.insert(&chunk, &project_id, CancellationToken::new()).await.unwrap();
        let results =
            store.search("deduplication", &project_id, 5, CancellationToken::new()).await.unwrap();
        assert_eq!(results.len(), 1);
    }

    #[tokio::test]
    async fn delete_nonexistent_chunk_does_not_error() {
        let (store, _pool) = make_store().await;
        let project_id = ProjectId("nonexist".into());
        store.delete("no-such-chunk", &project_id, CancellationToken::new()).await.unwrap();
    }

    /// FTS search returns results ranked by relevance.
    #[tokio::test]
    async fn fts_search_returns_ranked_results() {
        let (store, _pool) = make_store().await;
        let project_id = ProjectId("fts-test".into());
        let cancel = CancellationToken::new();

        store
            .insert(
                &chunk("more", "fox fox fox fox fox fox fox fox fox fox", &project_id),
                &project_id,
                cancel.clone(),
            )
            .await
            .unwrap();
        store
            .insert(
                &chunk("less", "the quick brown fox jumps over the lazy dog", &project_id),
                &project_id,
                cancel.clone(),
            )
            .await
            .unwrap();
        store
            .insert(
                &chunk("none", "jumps over the lazy dog", &project_id),
                &project_id,
                cancel.clone(),
            )
            .await
            .unwrap();

        let results = store.search("fox", &project_id, 10, cancel.clone()).await.unwrap();
        assert!(!results.is_empty(), "should find at least one result for 'fox'");
        // The chunk with many "fox" repetitions should be ranked highest
        assert_eq!(results[0].chunk_id, "more", "most relevant result should be first");
        // Verify that BM25 scores are non-zero and come from the real rank function
        for result in &results {
            assert!(result.score > 0.0, "FTS BM25 score should be positive, got {}", result.score);
        }
        // The first two results should have different BM25 scores because
        // "more" has 10x the term frequency of "less"
        assert!(
            results[0].score > results[1].score,
            "more matches should have higher BM25 score: {} vs {}",
            results[0].score,
            results[1].score,
        );
    }

    /// FTS search with an empty query returns no results.
    #[tokio::test]
    async fn fts_search_empty_query_returns_empty() {
        let (store, _pool) = make_store().await;
        let project_id = ProjectId("fts-empty".into());
        let cancel = CancellationToken::new();

        store
            .insert(&chunk("id1", "some content", &project_id), &project_id, cancel.clone())
            .await
            .unwrap();

        let results = store.search("", &project_id, 10, cancel).await.unwrap();
        assert!(results.is_empty(), "empty query should return no results");
    }

    /// Helper for FTS tests - creates a MemoryChunk.
    fn chunk(id: &str, content: &str, project_id: &ProjectId) -> MemoryChunk {
        MemoryChunk {
            id: id.into(),
            project_id: project_id.clone(),
            namespace: concerto_core::memory::MemoryNamespace::Project(project_id.clone()),
            content: content.into(),
            file_path: Some(camino::Utf8PathBuf::from("test.rs")),
            start_line: None,
            end_line: None,
            chunk_type: ChunkType::Function,
            score: 0.0,
            model_id: String::new(),
            model_version: String::new(),
        }
    }
}
