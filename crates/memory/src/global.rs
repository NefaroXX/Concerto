//! Global (user-scoped) memory store backed by a separate SQLite database.
//!
//! This store holds user-level preferences, facts, and working memory that
//! apply across all projects. Each entry is scoped by `user_id_hash` so
//! different users on the same machine never see each other's global state.
//!
//! Schema is intentionally simple — a KV-like table with an FTS5 index for
//! text search. No embedding pipeline, no chunk metadata.

use camino::Utf8Path;
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

    /// Open (or create) the global memory database at `path`, self-healing a
    /// corrupted file (ADR-54 §2/§97).
    ///
    /// When the first open fails **and** the file is not a valid SQLite
    /// database, the file is quarantined to `<name>.corrupt-<ts>.bak` and the
    /// open is retried once against a fresh database. A file with a valid
    /// SQLite header (a schema problem on real data) is never quarantined —
    /// the original error is surfaced so user data is never silently deleted.
    ///
    /// Callers on the run path treat any error here as fail-soft: global memory
    /// is optional and an unreadable database must never abort a run.
    pub async fn connect(path: &Utf8Path) -> Result<Self, CoreMemoryError> {
        match Self::try_connect(path).await {
            Ok(store) => Ok(store),
            Err(original) => {
                match concerto_core::helpers::quarantine_corrupt_db_file(path.as_std_path()) {
                    Some(quarantine) => {
                        tracing::warn!(
                            path = %path,
                            quarantine = %quarantine.display(),
                            "global memory database was not a valid SQLite file; quarantined \
                             corrupted file and retrying with a fresh database"
                        );
                        match Self::try_connect(path).await {
                            Ok(store) => Ok(store),
                            // The retry failed too — surface the original
                            // failure so the cause is never masked.
                            Err(_) => Err(original),
                        }
                    }
                    None => Err(original),
                }
            }
        }
    }

    /// Open the database without quarantine recovery, failing deterministically
    /// at open time (including a `PRAGMA schema_version` probe) so a
    /// garbage/truncated file is not deferred to the first query.
    async fn try_connect(path: &Utf8Path) -> Result<Self, CoreMemoryError> {
        let options = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(path.as_std_path())
            .create_if_missing(true);

        let pool = SqlitePool::connect_with(options).await.map_err(|e| {
            CoreMemoryError::Persistence(format!("failed to open global memory db: {e}"))
        })?;

        let _schema_version: i64 =
            sqlx::query_scalar("PRAGMA schema_version;").fetch_one(&pool).await.map_err(|e| {
                CoreMemoryError::Persistence(format!("global memory db header check failed: {e}"))
            })?;

        sqlx::query("PRAGMA journal_mode=WAL;")
            .execute(&pool)
            .await
            .map_err(|e| CoreMemoryError::Persistence(format!("failed to set WAL mode: {e}")))?;

        Self::new(pool).await
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
                stale: false,
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

    #[tokio::test]
    async fn connect_creates_database_and_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = camino::Utf8PathBuf::from_path_buf(dir.path().join("global_memory.db")).unwrap();

        let store = GlobalMemoryStore::connect(&path).await.expect("connect");
        assert!(path.is_file(), "connect must create the database file");

        let entry = make_global_entry("connected hello", "user-connect");
        store.store(&entry, CancellationToken::new()).await.expect("store");
        let results = store
            .retrieve(&make_global_query("hello", "user-connect"), CancellationToken::new())
            .await
            .expect("retrieve");
        assert_eq!(results.len(), 1);
        assert!(results[0].content.contains("connected hello"));

        // A second connect succeeds against the existing store.
        GlobalMemoryStore::connect(&path).await.expect("reconnect");
    }

    /// ADR-54 §2/§97 self-heal: a garbage file at the store path is quarantined
    /// to `<name>.corrupt-<ts>.bak` and a fresh database is created; a file with
    /// a valid SQLite header is NEVER quarantined — the original error is
    /// surfaced so real data is never silently deleted.
    #[tokio::test]
    async fn connect_self_heals_garbage_file_but_never_a_valid_header_file() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = camino::Utf8PathBuf::from_path_buf(dir.path().join("global.db")).unwrap();

        // Garbage file -> quarantine + fresh store on retry.
        std::fs::write(db_path.as_std_path(), b"this is definitely not a sqlite database file")
            .unwrap();
        let store = GlobalMemoryStore::connect(&db_path).await;
        assert!(store.is_ok(), "connect must recover from a garbage db file");
        let quarantine_count = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().contains(".corrupt-"))
            .count();
        assert_eq!(quarantine_count, 1, "exactly one quarantine backup expected");
        assert!(db_path.is_file(), "a fresh global db must exist after recovery");

        // Valid SQLite header but broken contents -> error surfaced, file kept.
        let valid_header = camino::Utf8PathBuf::from_path_buf(dir.path().join("valid.db")).unwrap();
        std::fs::write(
            valid_header.as_std_path(),
            *b"SQLite format 3\0followed-by-garbage-that-is-not-a-real-database",
        )
        .unwrap();
        let result = GlobalMemoryStore::connect(&valid_header).await;
        assert!(result.is_err(), "valid-header but broken db must fail, not self-heal");
        let touched = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .any(|e| e.file_name().to_string_lossy().starts_with("valid.db.corrupt"));
        assert!(!touched, "valid-header file must never be quarantined");
    }
}
