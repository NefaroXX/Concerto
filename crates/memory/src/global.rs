//! Global (user-scoped) memory store backed by a separate SQLite database.
//!
//! This store holds user-level preferences, facts, and working memory that
//! apply across all projects. Each entry is scoped by `user_id_hash` so
//! different users on the same machine never see each other's global state.
//!
//! Schema is intentionally simple — a KV-like table with an FTS5 index for
//! text search. No embedding pipeline, no chunk metadata.

use concerto_core::error::MemoryError as CoreMemoryError;
use concerto_core::memory::{
    ChunkType, MemoryChunk, MemoryEntry, MemoryId, MemoryNamespace, MemoryQuery, ProjectId,
};
use concerto_core::CancellationToken;
use sqlx::SqlitePool;
use time::OffsetDateTime;

/// Global memory store backed by a dedicated SQLite pool.
///
/// Each row is keyed by `(user_id_hash, id)` and stores simple text content.
/// Queries use a substring (`LIKE`) match — no embedding or FTS5 for this
/// first pass, though an FTS5 index can be layered on top later if needed.
pub struct GlobalMemoryStore {
    pool: SqlitePool,
}

impl GlobalMemoryStore {
    /// Open (or create) the global memory table and its indexes.
    pub async fn new(pool: SqlitePool) -> Result<Self, CoreMemoryError> {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS global_memory (
                id TEXT PRIMARY KEY,
                user_id_hash TEXT NOT NULL,
                content TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            )",
        )
        .execute(&pool)
        .await
        .map_err(|e| CoreMemoryError::Persistence(format!("create global_memory table: {e}")))?;

        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_global_memory_user_id
             ON global_memory(user_id_hash, id)",
        )
        .execute(&pool)
        .await
        .map_err(|e| CoreMemoryError::Persistence(format!("create global_memory index: {e}")))?;

        Ok(Self { pool })
    }

    /// Insert or upsert a global memory entry.
    ///
    /// The entry *must* carry a `MemoryNamespace::Global { user_id_hash }`
    /// namespace — other namespaces are rejected.
    pub async fn store(
        &self,
        entry: &MemoryEntry,
        cancel: CancellationToken,
    ) -> Result<MemoryId, CoreMemoryError> {
        if cancel.is_cancelled() {
            return Err(CoreMemoryError::RetrievalFailed("cancelled".into()));
        }

        let user_id_hash = match &entry.namespace {
            MemoryNamespace::Global { user_id_hash } => user_id_hash.clone(),
            _ => {
                return Err(CoreMemoryError::RetrievalFailed(
                    "cannot store non-global entry in GlobalMemoryStore".into(),
                ))
            }
        };

        let now = OffsetDateTime::now_utc().unix_timestamp();

        sqlx::query(
            "INSERT INTO global_memory (id, user_id_hash, content, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(id) DO UPDATE SET
                content = excluded.content,
                updated_at = excluded.updated_at",
        )
        .bind(entry.id.0.to_string())
        .bind(&user_id_hash)
        .bind(&entry.content)
        .bind(entry.created_at.unix_timestamp())
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(|e| CoreMemoryError::Persistence(format!("global store insert: {e}")))?;

        Ok(entry.id)
    }

    /// Retrieve global memory entries matching the query.
    ///
    /// Only entries scoped to the query's `user_id_hash` are returned. The
    /// search is a simple substring match (`LIKE '%text%'`) on the `content`
    /// column.
    pub async fn retrieve(
        &self,
        query: &MemoryQuery,
        cancel: CancellationToken,
    ) -> Result<Vec<MemoryChunk>, CoreMemoryError> {
        if cancel.is_cancelled() {
            return Err(CoreMemoryError::RetrievalFailed("cancelled".into()));
        }

        let user_id_hash = match &query.namespace {
            MemoryNamespace::Global { user_id_hash } => user_id_hash.clone(),
            _ => {
                return Err(CoreMemoryError::RetrievalFailed(
                    "cannot query non-global namespace in GlobalMemoryStore".into(),
                ))
            }
        };

        let search_pattern = format!("%{}%", query.text);
        let limit = query.top_k.max(1) as i64;

        let rows = sqlx::query_as::<_, (String, String, String, i64, i64)>(
            "SELECT id, user_id_hash, content, created_at, updated_at
             FROM global_memory
             WHERE user_id_hash = ?1 AND content LIKE ?2
             ORDER BY updated_at DESC
             LIMIT ?3",
        )
        .bind(&user_id_hash)
        .bind(&search_pattern)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| CoreMemoryError::Persistence(format!("global retrieve: {e}")))?;

        Ok(rows
            .into_iter()
            .map(|(id, uid, content, _created_at, _updated_at)| MemoryChunk {
                id,
                project_id: ProjectId(user_id_hash.clone()),
                namespace: MemoryNamespace::Global { user_id_hash: uid },
                content,
                file_path: None,
                start_line: None,
                end_line: None,
                chunk_type: ChunkType::Fact,
                score: 1.0,
                model_id: String::new(),
                model_version: String::new(),
            })
            .collect())
    }

    /// Delete a global memory entry by its ULID.
    pub async fn invalidate(
        &self,
        id: MemoryId,
        _cancel: CancellationToken,
    ) -> Result<(), CoreMemoryError> {
        let affected = sqlx::query("DELETE FROM global_memory WHERE id = ?1")
            .bind(id.0.to_string())
            .execute(&self.pool)
            .await
            .map_err(|e| CoreMemoryError::Persistence(format!("global delete: {e}")))?;

        if affected.rows_affected() == 0 {
            return Err(CoreMemoryError::NotFound(id.to_string()));
        }
        Ok(())
    }

    /// Delete a global memory entry by its string ID.
    pub async fn invalidate_chunk(
        &self,
        id: &str,
        _cancel: CancellationToken,
    ) -> Result<(), CoreMemoryError> {
        let affected = sqlx::query("DELETE FROM global_memory WHERE id = ?1")
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(|e| CoreMemoryError::Persistence(format!("global delete chunk: {e}")))?;

        if affected.rows_affected() == 0 {
            return Err(CoreMemoryError::NotFound(id.to_string()));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use concerto_core::ids::Ulid;

    fn make_global_entry(content: &str, user_id_hash: &str) -> MemoryEntry {
        MemoryEntry {
            id: MemoryId(Ulid::new()),
            project_id: ProjectId(user_id_hash.to_string()),
            namespace: MemoryNamespace::Global { user_id_hash: user_id_hash.to_string() },
            content: content.to_string(),
            chunk_type: ChunkType::Fact,
            model_id: None,
            model_version: None,
            metadata: serde_json::json!({}),
            expires_at: None,
            created_at: OffsetDateTime::now_utc(),
        }
    }

    fn make_global_query(text: &str, user_id_hash: &str) -> MemoryQuery {
        MemoryQuery {
            text: text.to_string(),
            project_id: ProjectId(user_id_hash.to_string()),
            namespace: MemoryNamespace::Global { user_id_hash: user_id_hash.to_string() },
            top_k: 10,
            filters: vec![],
        }
    }

    async fn create_store() -> GlobalMemoryStore {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        GlobalMemoryStore::new(pool).await.unwrap()
    }

    #[tokio::test]
    async fn store_and_retrieve_same_user() {
        let store = create_store().await;
        let uid = "user1";
        let entry = make_global_entry("hello from user1", uid);
        let stored_id = store.store(&entry, CancellationToken::new()).await.expect("store");
        assert_eq!(stored_id, entry.id);

        let query = make_global_query("hello", uid);
        let results = store.retrieve(&query, CancellationToken::new()).await.expect("retrieve");
        assert_eq!(results.len(), 1);
        assert!(results[0].content.contains("hello from user1"));
    }

    #[tokio::test]
    async fn retrieve_isolated_by_user() {
        let store = create_store().await;
        let uid1 = "alice";
        let uid2 = "bob";

        store
            .store(&make_global_entry("alice secret", uid1), CancellationToken::new())
            .await
            .unwrap();
        store
            .store(&make_global_entry("bob secret", uid2), CancellationToken::new())
            .await
            .unwrap();

        let results = store
            .retrieve(&make_global_query("secret", uid1), CancellationToken::new())
            .await
            .unwrap();
        // Only alice's entry should match
        assert_eq!(results.len(), 1);
        assert!(results[0].content.contains("alice secret"));
    }

    #[tokio::test]
    async fn store_rejects_project_namespace() {
        let store = create_store().await;
        let entry = MemoryEntry {
            id: MemoryId(Ulid::new()),
            project_id: ProjectId("proj".into()),
            namespace: MemoryNamespace::Project(ProjectId("proj".into())),
            content: "project entry".into(),
            chunk_type: ChunkType::Function,
            model_id: None,
            model_version: None,
            metadata: serde_json::json!({}),
            expires_at: None,
            created_at: OffsetDateTime::now_utc(),
        };
        let result = store.store(&entry, CancellationToken::new()).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn invalidate_removes_entry() {
        let store = create_store().await;
        let uid = "user1";
        let entry = make_global_entry("to delete", uid);
        store.store(&entry, CancellationToken::new()).await.unwrap();

        store.invalidate(entry.id, CancellationToken::new()).await.unwrap();

        let results = store
            .retrieve(&make_global_query("to delete", uid), CancellationToken::new())
            .await
            .unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn invalidate_nonexistent_returns_error() {
        let store = create_store().await;
        let result = store.invalidate(MemoryId(Ulid::new()), CancellationToken::new()).await;
        assert!(result.is_err());
    }
}
