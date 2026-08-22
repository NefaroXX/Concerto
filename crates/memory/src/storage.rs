//! SQLite-backed persistent storage for working memory data.

use concerto_core::error::MemoryError;
use concerto_core::ids::Ulid;
use concerto_core::memory::{
    Decision, DecisionCategory, DecisionId, TaskNode, TaskNodeId, TaskStatus,
};
use concerto_core::types::TaskId;
use sqlx::sqlite::SqliteConnectOptions;
use sqlx::{Row, SqlitePool};
use time::OffsetDateTime;

/// A SQLite-backed database for working memory data.
///
/// Wraps a `SqlitePool` and runs migrations on connect.
/// The primary API is async; sync wrappers (try_*) use
/// `tokio::runtime::Handle::current().block_on()`.
pub struct MemoryDb {
    pool: SqlitePool,
}

impl MemoryDb {
    /// Open (or create) the database at `path` and run pending migrations.
    ///
    /// Self-heals a damaged file (ADR-54): when the first open fails and the
    /// file is not a valid SQLite database, the file is quarantined to
    /// `<name>.corrupt-<ts>.bak` and the open is retried once against a fresh
    /// database. Files with a valid SQLite header (schema or migration
    /// problems on real data) are never quarantined — the original error is
    /// surfaced so user data is never silently deleted.
    pub async fn connect(path: &camino::Utf8Path) -> Result<Self, MemoryError> {
        match Self::try_connect(path).await {
            Ok(db) => Ok(db),
            Err(original) => {
                match concerto_core::helpers::quarantine_corrupt_db_file(path.as_std_path()) {
                    Some(quarantine) => {
                        tracing::warn!(
                            path = %path,
                            quarantine = %quarantine.display(),
                            "memory database was not a valid SQLite file; quarantined corrupted file and retrying with a fresh database"
                        );
                        match Self::try_connect(path).await {
                            Ok(db) => Ok(db),
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

    /// Open the database without quarantine recovery, failing
    /// deterministically at open time (including a `PRAGMA schema_version`
    /// probe) so a garbage/truncated file is not deferred to the first query.
    async fn try_connect(path: &camino::Utf8Path) -> Result<Self, MemoryError> {
        let options =
            SqliteConnectOptions::new().filename(path.as_std_path()).create_if_missing(true);

        let pool = SqlitePool::connect_with(options)
            .await
            .map_err(|e| MemoryError::Persistence(format!("failed to open memory db: {e}")))?;

        // Read the SQLite header so a garbage/truncated file fails the open
        // deterministically instead of surfacing later on the first query.
        let _schema_version: i64 = sqlx::query_scalar("PRAGMA schema_version;")
            .fetch_one(&pool)
            .await
            .map_err(|e| MemoryError::Persistence(format!("memory db header check failed: {e}")))?;

        sqlx::query("PRAGMA journal_mode=WAL;")
            .execute(&pool)
            .await
            .map_err(|e| MemoryError::Persistence(format!("failed to set WAL mode: {e}")))?;

        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .map_err(|e| MemoryError::Persistence(format!("migration failed: {e}")))?;

        Ok(Self { pool })
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    // -----------------------------------------------------------------------
    // Decision CRUD (async)
    // -----------------------------------------------------------------------

    pub async fn insert_decision(&self, d: &Decision) -> Result<(), MemoryError> {
        let category_str =
            serde_json::to_string(&d.category).unwrap_or_else(|_| "\"other\"".into());
        sqlx::query(
            "INSERT INTO decisions (id, session_id, task_id, what, why, outcome, \
             category, confidence, superseded_by, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10) \
             ON CONFLICT(id) DO UPDATE SET \
             what=excluded.what, why=excluded.why, outcome=excluded.outcome, \
             category=excluded.category, confidence=excluded.confidence, \
             superseded_by=excluded.superseded_by",
        )
        .bind(d.id.0.to_string())
        .bind(d.session_id.to_string())
        .bind(d.task_id.map(|t| t.to_string()))
        .bind(&d.what)
        .bind(&d.why)
        .bind(&d.outcome)
        .bind(&category_str)
        .bind(d.confidence)
        .bind(d.superseded_by.map(|s| s.0.to_string()))
        .bind(d.created_at.unix_timestamp())
        .execute(&self.pool)
        .await
        .map_err(|e| MemoryError::Persistence(format!("insert decision failed: {e}")))?;
        Ok(())
    }

    pub async fn get_decision(&self, id: DecisionId) -> Result<Option<Decision>, MemoryError> {
        let row = sqlx::query(
            "SELECT id, session_id, task_id, what, why, outcome, category, \
             confidence, superseded_by, created_at \
             FROM decisions WHERE id = ?1",
        )
        .bind(id.0.to_string())
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| MemoryError::Persistence(format!("query decision failed: {e}")))?;

        row.as_ref().map(row_to_decision).transpose()
    }

    pub async fn list_decisions(&self) -> Result<Vec<Decision>, MemoryError> {
        let rows = sqlx::query(
            "SELECT id, session_id, task_id, what, why, outcome, category, \
             confidence, superseded_by, created_at \
             FROM decisions ORDER BY created_at DESC",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| MemoryError::Persistence(format!("list decisions failed: {e}")))?;

        rows.iter().map(row_to_decision).collect()
    }

    pub async fn delete_decision(&self, id: DecisionId) -> Result<(), MemoryError> {
        sqlx::query("DELETE FROM decisions WHERE id = ?1")
            .bind(id.0.to_string())
            .execute(&self.pool)
            .await
            .map_err(|e| MemoryError::Persistence(format!("delete decision failed: {e}")))?;
        Ok(())
    }

    pub async fn update_decision_confidence(
        &self,
        id: DecisionId,
        confidence: f32,
    ) -> Result<(), MemoryError> {
        let affected = sqlx::query("UPDATE decisions SET confidence = ?1 WHERE id = ?2")
            .bind(confidence)
            .bind(id.0.to_string())
            .execute(&self.pool)
            .await
            .map_err(|e| MemoryError::Persistence(format!("update confidence failed: {e}")))?;

        if affected.rows_affected() == 0 {
            return Err(MemoryError::RetrievalFailed(format!("decision {id} not found")));
        }
        Ok(())
    }

    pub async fn supersede_decision(
        &self,
        id: DecisionId,
        superseded_by: DecisionId,
    ) -> Result<(), MemoryError> {
        let affected = sqlx::query("UPDATE decisions SET superseded_by = ?1 WHERE id = ?2")
            .bind(superseded_by.0.to_string())
            .bind(id.0.to_string())
            .execute(&self.pool)
            .await
            .map_err(|e| MemoryError::Persistence(format!("supersede failed: {e}")))?;

        if affected.rows_affected() == 0 {
            return Err(MemoryError::RetrievalFailed(format!("decision {id} not found")));
        }
        Ok(())
    }

    pub async fn list_decisions_by_category(
        &self,
        category: DecisionCategory,
    ) -> Result<Vec<Decision>, MemoryError> {
        let category_str = serde_json::to_string(&category).unwrap_or_else(|_| "\"other\"".into());
        let rows = sqlx::query(
            "SELECT id, session_id, task_id, what, why, outcome, category, \
             confidence, superseded_by, created_at \
             FROM decisions WHERE category = ?1 ORDER BY created_at DESC",
        )
        .bind(&category_str)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| MemoryError::Persistence(format!("list decisions by category failed: {e}")))?;

        rows.iter().map(row_to_decision).collect()
    }

    // -----------------------------------------------------------------------
    // TaskNode CRUD (async)
    // -----------------------------------------------------------------------

    pub async fn upsert_task_node(&self, n: &TaskNode) -> Result<(), MemoryError> {
        let blocking_str: String =
            serde_json::to_string(&n.blocking.iter().map(|b| b.0.to_string()).collect::<Vec<_>>())
                .unwrap_or_else(|_| "[]".into());

        sqlx::query(
            "INSERT INTO task_nodes (id, session_id, parent_id, description, \
             status, blocking, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
             ON CONFLICT(id) DO UPDATE SET \
             parent_id=excluded.parent_id, description=excluded.description, \
             status=excluded.status, blocking=excluded.blocking",
        )
        .bind(n.id.0.to_string())
        .bind(n.session_id.to_string())
        .bind(n.parent_id.map(|p| p.0.to_string()))
        .bind(&n.description)
        .bind(n.status.as_str())
        .bind(&blocking_str)
        .bind(n.created_at.unix_timestamp())
        .execute(&self.pool)
        .await
        .map_err(|e| MemoryError::Persistence(format!("upsert task_node failed: {e}")))?;
        Ok(())
    }

    pub async fn get_task_node(&self, id: TaskNodeId) -> Result<Option<TaskNode>, MemoryError> {
        let row = sqlx::query(
            "SELECT id, session_id, parent_id, description, status, \
             blocking, created_at \
             FROM task_nodes WHERE id = ?1",
        )
        .bind(id.0.to_string())
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| MemoryError::Persistence(format!("query task_node failed: {e}")))?;

        row.as_ref().map(row_to_task_node).transpose()
    }

    pub async fn list_task_nodes(&self) -> Result<Vec<TaskNode>, MemoryError> {
        let rows = sqlx::query(
            "SELECT id, session_id, parent_id, description, status, \
             blocking, created_at \
             FROM task_nodes ORDER BY created_at",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| MemoryError::Persistence(format!("list task_nodes failed: {e}")))?;

        rows.iter().map(row_to_task_node).collect()
    }

    pub async fn list_task_nodes_by_status(
        &self,
        status: TaskStatus,
    ) -> Result<Vec<TaskNode>, MemoryError> {
        let rows = sqlx::query(
            "SELECT id, session_id, parent_id, description, status, \
             blocking, created_at \
             FROM task_nodes WHERE status = ?1 ORDER BY created_at",
        )
        .bind(status.as_str())
        .fetch_all(&self.pool)
        .await
        .map_err(|e| MemoryError::Persistence(format!("list task_nodes by status failed: {e}")))?;

        rows.iter().map(row_to_task_node).collect()
    }

    pub async fn list_root_task_nodes(&self) -> Result<Vec<TaskNode>, MemoryError> {
        let rows = sqlx::query(
            "SELECT id, session_id, parent_id, description, status, \
             blocking, created_at \
             FROM task_nodes WHERE parent_id IS NULL ORDER BY created_at",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| MemoryError::Persistence(format!("list root task_nodes failed: {e}")))?;

        rows.iter().map(row_to_task_node).collect()
    }

    pub async fn list_child_task_nodes(
        &self,
        parent_id: TaskNodeId,
    ) -> Result<Vec<TaskNode>, MemoryError> {
        let rows = sqlx::query(
            "SELECT id, session_id, parent_id, description, status, \
             blocking, created_at \
             FROM task_nodes WHERE parent_id = ?1 ORDER BY created_at",
        )
        .bind(parent_id.0.to_string())
        .fetch_all(&self.pool)
        .await
        .map_err(|e| MemoryError::Persistence(format!("list child task_nodes failed: {e}")))?;

        rows.iter().map(row_to_task_node).collect()
    }

    pub async fn update_task_node_status(
        &self,
        id: TaskNodeId,
        status: TaskStatus,
    ) -> Result<(), MemoryError> {
        let affected = sqlx::query("UPDATE task_nodes SET status = ?1 WHERE id = ?2")
            .bind(status.as_str())
            .bind(id.0.to_string())
            .execute(&self.pool)
            .await
            .map_err(|e| {
                MemoryError::Persistence(format!("update task_node status failed: {e}"))
            })?;

        if affected.rows_affected() == 0 {
            return Err(MemoryError::RetrievalFailed(format!("task node {id} not found")));
        }
        Ok(())
    }

    pub async fn delete_task_node(&self, id: TaskNodeId) -> Result<(), MemoryError> {
        sqlx::query("DELETE FROM task_nodes WHERE id = ?1")
            .bind(id.0.to_string())
            .execute(&self.pool)
            .await
            .map_err(|e| MemoryError::Persistence(format!("delete task_node failed: {e}")))?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Synchronous wrappers (for sync DecisionStore/TaskTreeStore)
    // -----------------------------------------------------------------------

    pub fn try_insert_decision(&self, d: &Decision) -> Result<(), MemoryError> {
        let handle = tokio::runtime::Handle::current();
        handle.block_on(self.insert_decision(d))
    }

    pub fn try_update_decision_confidence(
        &self,
        id: DecisionId,
        confidence: f32,
    ) -> Result<(), MemoryError> {
        let handle = tokio::runtime::Handle::current();
        handle.block_on(self.update_decision_confidence(id, confidence))
    }

    pub fn try_supersede_decision(
        &self,
        id: DecisionId,
        superseded_by: DecisionId,
    ) -> Result<(), MemoryError> {
        let handle = tokio::runtime::Handle::current();
        handle.block_on(self.supersede_decision(id, superseded_by))
    }

    pub fn try_delete_decision(&self, id: DecisionId) -> Result<(), MemoryError> {
        let handle = tokio::runtime::Handle::current();
        handle.block_on(self.delete_decision(id))
    }

    pub fn try_upsert_task_node(&self, n: &TaskNode) -> Result<(), MemoryError> {
        let handle = tokio::runtime::Handle::current();
        handle.block_on(self.upsert_task_node(n))
    }

    pub fn try_delete_task_node(&self, id: TaskNodeId) -> Result<(), MemoryError> {
        let handle = tokio::runtime::Handle::current();
        handle.block_on(self.delete_task_node(id))
    }

    pub fn try_update_task_node_status(
        &self,
        id: TaskNodeId,
        status: TaskStatus,
    ) -> Result<(), MemoryError> {
        let handle = tokio::runtime::Handle::current();
        handle.block_on(self.update_task_node_status(id, status))
    }
}

// ---------------------------------------------------------------------------
// Row mapping helpers
// ---------------------------------------------------------------------------

fn row_to_decision(r: &sqlx::sqlite::SqliteRow) -> Result<Decision, MemoryError> {
    let id_str: String = r.get("id");
    let session_id_str: String = r.get("session_id");
    let category_str: String = r.get("category");
    let created_at_ts: i64 = r.get("created_at");

    let id = DecisionId(
        Ulid::from_string(&id_str)
            .map_err(|e| MemoryError::Persistence(format!("invalid decision id ULID: {e}")))?,
    );
    let session_id = Ulid::from_string(&session_id_str)
        .map_err(|e| MemoryError::Persistence(format!("invalid session_id ULID: {e}")))?;

    let category: DecisionCategory =
        serde_json::from_str(&category_str).unwrap_or(DecisionCategory::Other);

    Ok(Decision {
        id,
        session_id,
        task_id: r
            .get::<Option<String>, _>("task_id")
            .and_then(|s| Some(TaskId(Ulid::from_string(&s).ok()?))),
        what: r.get("what"),
        why: r.get("why"),
        outcome: r.get("outcome"),
        category,
        confidence: r.get("confidence"),
        superseded_by: r
            .get::<Option<String>, _>("superseded_by")
            .and_then(|s| Some(DecisionId(Ulid::from_string(&s).ok()?))),
        created_at: OffsetDateTime::from_unix_timestamp(created_at_ts)
            .map_err(|e| MemoryError::Persistence(format!("invalid timestamp: {e}")))?,
    })
}

fn row_to_task_node(r: &sqlx::sqlite::SqliteRow) -> Result<TaskNode, MemoryError> {
    let id_str: String = r.get("id");
    let session_id_str: String = r.get("session_id");
    let blocking_str: String = r.get("blocking");
    let created_at_ts: i64 = r.get("created_at");

    let id = TaskNodeId(
        Ulid::from_string(&id_str)
            .map_err(|e| MemoryError::Persistence(format!("invalid task_node id ULID: {e}")))?,
    );
    let session_id = Ulid::from_string(&session_id_str)
        .map_err(|e| MemoryError::Persistence(format!("invalid session_id ULID: {e}")))?;

    let blocking: Vec<TaskNodeId> = serde_json::from_str::<Vec<String>>(&blocking_str)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|s| Ulid::from_string(&s).ok())
        .map(TaskNodeId)
        .collect();

    Ok(TaskNode {
        id,
        session_id,
        parent_id: r
            .get::<Option<String>, _>("parent_id")
            .and_then(|s| Some(TaskNodeId(Ulid::from_string(&s).ok()?))),
        description: r.get("description"),
        status: TaskStatus::parse_status(&r.get::<String, _>("status")),
        children: Vec::new(),
        blocking,
        created_at: OffsetDateTime::from_unix_timestamp(created_at_ts)
            .map_err(|e| MemoryError::Persistence(format!("invalid timestamp: {e}")))?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    /// Self-heal (ADR-54): a garbage file at the store path is quarantined to
    /// `<name>.corrupt-<ts>.bak` and a fresh database is created; a file with
    /// a valid SQLite header is NEVER quarantined — the original error is
    /// surfaced so real data is never silently deleted.
    async fn connect_self_heals_garbage_file_but_never_a_valid_header_file() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = camino::Utf8PathBuf::from_path_buf(dir.path().join("memory.db")).unwrap();

        // Garbage file -> quarantine + fresh store on retry.
        std::fs::write(db_path.as_std_path(), b"this is definitely not a sqlite database file")
            .unwrap();
        let db = MemoryDb::connect(&db_path).await;
        assert!(db.is_ok(), "connect must recover from a garbage db file");
        let quarantine_count = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().contains(".corrupt-"))
            .count();
        assert_eq!(quarantine_count, 1, "exactly one quarantine backup expected");
        assert!(db_path.is_file(), "a fresh memory.db must exist after recovery");
        // A second connect succeeds against the rebuilt store.
        MemoryDb::connect(&db_path).await.unwrap();

        // Valid SQLite header but broken contents -> error surfaced, file kept.
        let valid_header = camino::Utf8PathBuf::from_path_buf(dir.path().join("valid.db")).unwrap();
        std::fs::write(
            valid_header.as_std_path(),
            *b"SQLite format 3\0followed-by-garbage-that-is-not-a-real-database",
        )
        .unwrap();
        let result = MemoryDb::connect(&valid_header).await;
        assert!(result.is_err(), "valid-header but broken db must fail, not self-heal");
        let touched = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .any(|e| e.file_name().to_string_lossy().starts_with("valid.db.corrupt"));
        assert!(!touched, "valid-header file must never be quarantined");
    }
}
