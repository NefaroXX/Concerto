//! TTL and staleness management for the memory system.
//!
//! Manages:
//! - Time-to-live expiry of memory chunks
//! - Stale embeddings (model version mismatch)
//! - Re-index scheduling for outdated chunks
//! - Reclaiming storage from expired entries

use concerto_core::CancellationToken;
use std::collections::HashMap;
use std::sync::Arc;

use concerto_core::error::MemoryError;
use concerto_core::memory::ProjectId;
use sqlx::SqlitePool;
use sqlx::{AssertSqlSafe, Row};
use time::OffsetDateTime;

use crate::fts::FullTextStore;
use crate::vector_store::VectorStore;

/// Default TTL for different chunk types (in days).
pub const TTL_FUNCTION_DAYS: i64 = 90;
pub const TTL_STRUCT_DAYS: i64 = 90;
pub const TTL_FILE_DAYS: i64 = 60;
pub const TTL_SLIDING_WINDOW_DAYS: i64 = 30;
pub const TTL_DECISION_DAYS: i64 = 365;

/// Session bucket for derived summary chunks whose vector row carries no
/// session attribution: they retain project-wide (one shared bucket).
const DERIVED_SUMMARY_PROJECT_BUCKET: &str = "\u{0}project";

/// Result of one [`TtlManager::prune_derived_summaries`] pass (ADR-65 §8):
/// the derived chunk ids removed and how many session buckets were examined.
/// Source chunks are never reported here — they are never touched.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PruneReport {
    /// Chunk ids removed (vector row + FTS entry) in removal order.
    pub pruned_ids: Vec<String>,
    /// Distinct session buckets (including the project-wide fallback bucket)
    /// observed during the pass.
    pub sessions_examined: usize,
}

/// Manages TTL expiry and re-index scheduling.
pub struct TtlManager {
    vector_store: Arc<dyn VectorStore>,
    fts_store: Arc<dyn FullTextStore>,
    pool: SqlitePool,
    default_ttl_days: Option<u16>,
}

impl TtlManager {
    pub fn new(
        vector_store: Arc<dyn VectorStore>,
        fts_store: Arc<dyn FullTextStore>,
        pool: SqlitePool,
    ) -> Self {
        Self { vector_store, fts_store, pool, default_ttl_days: None }
    }

    /// Use one user-configured retention window for every indexed chunk.
    pub fn with_default_ttl_days(
        vector_store: Arc<dyn VectorStore>,
        fts_store: Arc<dyn FullTextStore>,
        pool: SqlitePool,
        ttl_days: u16,
    ) -> Self {
        Self { vector_store, fts_store, pool, default_ttl_days: Some(ttl_days) }
    }

    /// Remove all expired entries for a project.
    ///
    /// Queries the vector store for entries whose `created_at` timestamp
    /// exceeds the TTL for their chunk type, tombstones them, and deletes
    /// the corresponding FTS entries.
    ///
    /// Returns the number of entries purged.
    pub async fn purge_expired(
        &self,
        project_id: &ProjectId,
        cancel: CancellationToken,
    ) -> Result<usize, MemoryError> {
        // Build a SQL CASE expression that maps chunk_type debug strings
        // to their TTL in days.  Unknown types get the default file TTL.
        let ttl_case = self.default_ttl_days.map_or_else(
            || {
                format!(
                    "CASE chunk_type \
                WHEN 'Function' THEN {TTL_FUNCTION_DAYS} \
                WHEN 'Struct' THEN {TTL_STRUCT_DAYS} \
                WHEN 'Trait' THEN {TTL_FUNCTION_DAYS} \
                WHEN 'Impl' THEN {TTL_FUNCTION_DAYS} \
                WHEN 'SessionSummary' THEN {TTL_DECISION_DAYS} \
                WHEN 'Fact' THEN {TTL_DECISION_DAYS} \
                WHEN 'SlidingWindow' THEN {TTL_SLIDING_WINDOW_DAYS} \
                ELSE {TTL_FILE_DAYS} \
            END"
                )
            },
            |days| days.to_string(),
        );

        // Find expired entries: created_at + TTL_days < now.
        let query = format!(
            "SELECT id FROM vector_store \
             WHERE project_id = ? \
               AND tombstone = 0 \
               AND datetime(created_at, '+' || {ttl_case} || ' days') < datetime('now')"
        );

        // AUDITED (sqlx 0.9 `AssertSqlSafe`): the SQL is built from static fragments
        // and the locally computed `{ttl_case}` CASE expression; no user input is
        // interpolated — every filter value is bound via `?`.
        let rows = sqlx::query(AssertSqlSafe(query))
            .bind(&project_id.0)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| MemoryError::RetrievalFailed(e.to_string()))?;

        if rows.is_empty() {
            return Ok(0);
        }

        let count = rows.len();

        // Tombstone each expired entry in the vector store and delete from FTS.
        for row in &rows {
            let chunk_id: String = row.get("id");
            self.vector_store.tombstone(&chunk_id, project_id, cancel.clone()).await?;
            let _ = self.fts_store.delete(&chunk_id, project_id, cancel.clone()).await;
        }

        // Compact: permanently remove tombstoned entries.
        self.vector_store.delete_tombstoned(project_id, cancel).await?;

        Ok(count)
    }

    /// Find entries with stale embeddings (model version mismatch)
    /// and mark them for re-indexing.
    ///
    /// Returns the count of stale entries found.
    pub async fn mark_stale_embeddings(
        &self,
        project_id: &ProjectId,
        current_model_version: &str,
        cancel: CancellationToken,
    ) -> Result<usize, MemoryError> {
        // Mark stale in the vector store (sets stale=1 for matching model_version).
        self.vector_store.mark_stale(project_id, current_model_version, cancel.clone()).await?;

        // Also tombstone stale entries so they drop out of search results
        // until re-indexed with the current model.
        //
        // We query for stale entries, tombstone them, and delete from FTS.
        let rows = sqlx::query(
            "SELECT id FROM vector_store WHERE project_id = ? AND stale = 1 AND tombstone = 0",
        )
        .bind(&project_id.0)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| MemoryError::RetrievalFailed(e.to_string()))?;

        let count = rows.len();

        for row in &rows {
            let chunk_id: String = row.get("id");
            self.vector_store.tombstone(&chunk_id, project_id, cancel.clone()).await?;
            let _ = self.fts_store.delete(&chunk_id, project_id, cancel.clone()).await;
        }

        // Compact tombstoned entries.
        if count > 0 {
            self.vector_store.delete_tombstoned(project_id, cancel).await?;
        }

        Ok(count)
    }

    /// Compact tombstones — permanently remove soft-deleted entries.
    pub async fn compact(
        &self,
        project_id: &ProjectId,
        cancel: CancellationToken,
    ) -> Result<(), MemoryError> {
        self.vector_store.delete_tombstoned(project_id, cancel).await
    }

    /// ADR-65 §8 retention: prune DERIVED summary chunks — `Fact` and
    /// `SessionSummary` vector rows only. Source chunks (Function, Struct,
    /// Trait, Impl, Enum, Module, Test, SlidingWindow) are never touched.
    ///
    /// Two independent rules, both applied to the derived classes:
    ///
    /// - **Count cap per session bucket**: keep the newest `keep_per_session`
    ///   rows per bucket (newer wins). `0` disables the cap. A session bucket
    ///   is the row metadata's `session_id` string when one was stored;
    ///   otherwise the rows group into one project-wide fallback bucket.
    /// - **Age window**: prune rows whose `created_at` is older than
    ///   `retention_days` days. `0` disables the window.
    ///
    /// Rows are hard-deleted from the vector store and their FTS entries are
    /// removed (best-effort) so pruned summaries can never resurface through
    /// retrieval. The pass is idempotent: a re-run finds only rows below the
    /// cap/window and prunes nothing. Cancellation between removals aborts
    /// with what was already removed (partial progress is valid: a pruned row
    /// stays pruned). Every removed id is logged.
    pub async fn prune_derived_summaries(
        &self,
        project_id: &ProjectId,
        keep_per_session: u32,
        retention_days: u16,
        cancel: CancellationToken,
    ) -> Result<PruneReport, MemoryError> {
        let mut report = PruneReport::default();
        if cancel.is_cancelled() {
            return Ok(report);
        }
        let rows = sqlx::query(
            "SELECT id, created_at, metadata FROM vector_store \
             WHERE project_id = ? AND tombstone = 0 \
               AND chunk_type IN ('Fact', 'SessionSummary') \
             ORDER BY created_at DESC, id ASC",
        )
        .bind(&project_id.0)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| MemoryError::RetrievalFailed(e.to_string()))?;
        if rows.is_empty() {
            return Ok(report);
        }

        // Age cutoff as a string in the SAME format the rows store
        // (`OffsetDateTime::to_string`), so stale rows are found with a plain
        // lexicographic comparison — no datetime parsing, no clock-in-SQL.
        let cutoff = if retention_days > 0 {
            Some(
                (OffsetDateTime::now_utc() - time::Duration::days(i64::from(retention_days)))
                    .to_string(),
            )
        } else {
            None
        };

        // Newest-first; keep the first `keep_per_session` rows per bucket.
        let mut buckets: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut kept: HashMap<String, u32> = HashMap::new();
        let mut pruned: Vec<String> = Vec::new();
        for row in rows {
            if cancel.is_cancelled() {
                break;
            }
            let id: String = row.get("id");
            let created_at: String = row.get("created_at");
            let metadata: Option<String> = row.get("metadata");
            let bucket = summary_session_bucket(metadata.as_deref());
            buckets.insert(bucket.clone());
            let used = kept.entry(bucket).or_insert(0);
            let over_cap = if keep_per_session > 0 && *used >= keep_per_session {
                true
            } else {
                *used += 1;
                false
            };
            let expired =
                cutoff.as_ref().is_some_and(|cutoff| created_at.as_str() < cutoff.as_str());
            if over_cap || expired {
                pruned.push(id);
            }
        }
        report.sessions_examined = buckets.len();
        report.pruned_ids = pruned;

        self.remove_derived_rows(project_id, &report.pruned_ids, cancel).await?;
        if !report.pruned_ids.is_empty() {
            tracing::info!(
                pruned = report.pruned_ids.len(),
                sessions = report.sessions_examined,
                "pruned derived summary chunks past retention (ADR-65 §8)"
            );
        }
        Ok(report)
    }

    /// Hard-delete pruned vector rows and their FTS entries (best-effort FTS:
    /// consolidation projections are vector-only, so the FTS row may simply
    /// not exist).
    async fn remove_derived_rows(
        &self,
        project_id: &ProjectId,
        ids: &[String],
        cancel: CancellationToken,
    ) -> Result<(), MemoryError> {
        for id in ids {
            if cancel.is_cancelled() {
                break;
            }
            sqlx::query("DELETE FROM vector_store WHERE project_id = ? AND id = ?")
                .bind(&project_id.0)
                .bind(id)
                .execute(&self.pool)
                .await
                .map_err(|e| MemoryError::Persistence(e.to_string()))?;
            let _ = self.fts_store.delete(id, project_id, cancel.clone()).await;
            tracing::debug!(chunk_id = %id, "pruned derived summary chunk (ADR-65 §8)");
        }
        Ok(())
    }
}

/// The session bucket a derived summary row groups into for retention: the
/// metadata sidecar's `session_id` string when one was stored, else the
/// project-wide fallback bucket (an unattributed row can never invent a
/// session).
fn summary_session_bucket(metadata: Option<&str>) -> String {
    metadata
        .and_then(|json| serde_json::from_str::<serde_json::Value>(json).ok())
        .and_then(|value| {
            value.get("session_id").and_then(|value| value.as_str()).map(ToOwned::to_owned)
        })
        .filter(|session| !session.is_empty())
        .unwrap_or_else(|| DERIVED_SUMMARY_PROJECT_BUCKET.to_owned())
}

/// Suggested TTL for a given chunk type (in days).
pub fn suggested_ttl_days(chunk_type: &concerto_core::memory::ChunkType) -> i64 {
    use concerto_core::memory::ChunkType;
    match chunk_type {
        ChunkType::Function | ChunkType::Struct | ChunkType::Trait | ChunkType::Impl => {
            TTL_FUNCTION_DAYS
        }
        ChunkType::SessionSummary | ChunkType::Fact => TTL_DECISION_DAYS,
        ChunkType::SlidingWindow => TTL_SLIDING_WINDOW_DAYS,
        _ => TTL_FILE_DAYS,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fts::SqliteFullTextStore;
    use crate::vector_store::SqliteVectorStore;
    use concerto_core::memory::ChunkType;
    use concerto_core::memory::EmbeddingRecord;
    use time::OffsetDateTime;

    #[test]
    fn ttl_for_function_is_90_days() {
        assert_eq!(suggested_ttl_days(&ChunkType::Function), 90);
    }

    #[test]
    fn ttl_for_session_summary_is_365_days() {
        assert_eq!(suggested_ttl_days(&ChunkType::SessionSummary), 365);
    }

    #[test]
    fn sliding_window_has_shortest_ttl() {
        let sw = suggested_ttl_days(&ChunkType::SlidingWindow);
        let func = suggested_ttl_days(&ChunkType::Function);
        assert!(sw < func);
    }

    #[test]
    fn session_bucket_falls_back_without_metadata_session() {
        assert_eq!(summary_session_bucket(Some(r#"{"k": 1}"#)), DERIVED_SUMMARY_PROJECT_BUCKET);
        assert_eq!(
            summary_session_bucket(Some(r#"{"session_id": null}"#)),
            DERIVED_SUMMARY_PROJECT_BUCKET
        );
        assert_eq!(
            summary_session_bucket(Some(r#""plain string""#)),
            DERIVED_SUMMARY_PROJECT_BUCKET
        );
        assert_eq!(summary_session_bucket(None), DERIVED_SUMMARY_PROJECT_BUCKET);
        assert_eq!(summary_session_bucket(Some("broken json")), DERIVED_SUMMARY_PROJECT_BUCKET);
        assert_eq!(summary_session_bucket(Some(r#"{"session_id": "s1"}"#)), "s1");
        assert_eq!(
            summary_session_bucket(Some(r#"{"session_id": ""}"#)),
            DERIVED_SUMMARY_PROJECT_BUCKET,
            "an empty session id invents no session"
        );
    }

    /// One row-level count of a chunk type, read straight from the table (an
    /// independent projection of what the prune deleted).
    async fn count(pool: &sqlx::SqlitePool, chunk_type: &str) -> usize {
        sqlx::query("SELECT COUNT(*) FROM vector_store WHERE chunk_type = ?")
            .bind(chunk_type)
            .fetch_one(pool)
            .await
            .unwrap()
            .get::<i64, _>(0) as usize
    }

    async fn test_manager() -> (sqlx::SqlitePool, Arc<SqliteVectorStore>, TtlManager) {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        let vector = Arc::new(SqliteVectorStore::new(pool.clone()).await.unwrap());
        let fts = Arc::new(SqliteFullTextStore::new(pool.clone()).await.unwrap());
        let manager = TtlManager::new(vector.clone(), fts, pool.clone());
        (pool, vector, manager)
    }

    fn offset_days_ago(days: i64) -> OffsetDateTime {
        OffsetDateTime::now_utc() - time::Duration::days(days)
    }

    /// Seed a derived summary row through `store_projection` — the write path
    /// that records the metadata sidecar retention groups on.
    async fn seed(
        store: &SqliteVectorStore,
        id: &str,
        chunk_type: ChunkType,
        session: &str,
        content: &str,
        created_at: OffsetDateTime,
    ) {
        let record = EmbeddingRecord {
            id: id.to_string(),
            project_id: ProjectId("retention".into()),
            chunk_hash: blake3::hash(content.as_bytes()).to_string(),
            content: content.to_string(),
            file_path: format!("derived/{id}").into(),
            start_line: None,
            end_line: None,
            chunk_type,
            vector: vec![0.25, -0.5],
            model_id: "test".into(),
            model_version: "1".into(),
            stale: false,
            created_at,
        };
        let metadata = serde_json::json!({ "session_id": session });
        store
            .store_projection(&record, &metadata, CancellationToken::new())
            .await
            .expect("seed derived row");
    }

    /// Seed a SOURCE chunk through the plain `store` path (no metadata, the
    /// same shape background indexing produces).
    async fn seed_source(store: &SqliteVectorStore, id: &str, created_at: OffsetDateTime) {
        let record = EmbeddingRecord {
            id: id.to_string(),
            project_id: ProjectId("retention".into()),
            chunk_hash: format!("hash-{id}"),
            content: format!("fn {id}() {{}}"),
            file_path: format!("src/{id}.rs").into(),
            start_line: Some(1),
            end_line: Some(1),
            chunk_type: ChunkType::Function,
            vector: vec![0.5],
            model_id: "test".into(),
            model_version: "1".into(),
            stale: false,
            created_at,
        };
        store.store(&[record], CancellationToken::new()).await.unwrap();
    }

    #[tokio::test]
    async fn prune_keeps_newest_per_session_and_never_touches_source_chunks() {
        let (pool, store, manager) = test_manager().await;
        let project = ProjectId("retention".into());
        let old = offset_days_ago(30);
        let fresh = OffsetDateTime::now_utc();
        let token = CancellationToken::new();

        // Session a: three derived rows — the count cap 2 keeps only the two
        // freshest. Session b: two rows — both kept. `c-1` carries NO session
        // key (empty) and falls into the project bucket.
        seed(&store, "a-1", ChunkType::Fact, "sess-a", "a-1 oldest", old).await;
        seed(&store, "a-2", ChunkType::SessionSummary, "sess-a", "a-2 mid", fresh).await;
        seed(&store, "a-3", ChunkType::Fact, "sess-a", "a-3 fresh", fresh).await;
        seed(&store, "b-1", ChunkType::Fact, "sess-b", "b-1 old", old).await;
        seed(&store, "b-2", ChunkType::SessionSummary, "sess-b", "b-2 fresh", fresh).await;
        seed(&store, "c-1", ChunkType::SessionSummary, "", "c-1 fallback", fresh).await;
        // A source chunk must survive every retention rule.
        seed_source(&store, "fn-src-1", old).await;

        let report = manager.prune_derived_summaries(&project, 2, 0, token.clone()).await.unwrap();
        assert_eq!(
            report.pruned_ids,
            vec!["a-1".to_string()],
            "sess-a holds 3 derived rows > cap 2: only the OLDEST is pruned"
        );
        assert_eq!(report.sessions_examined, 3, "sess-a, sess-b, and the project bucket");

        // Idempotent: a re-run has nothing left to prune.
        let again = manager.prune_derived_summaries(&project, 2, 0, token.clone()).await.unwrap();
        assert!(again.pruned_ids.is_empty(), "idempotent re-run prunes nothing");

        // Survivors: the two newest of sess-a, both of sess-b, the fallback
        // row, and the untouched source chunk.
        for id in ["a-2", "a-3", "b-1", "b-2"] {
            let chunks =
                store.get_chunks(&project, &[id.to_string()], token.clone()).await.unwrap();
            assert_eq!(chunks.len(), 1, "row {id} survives the cap");
        }
        assert_eq!(count(&pool, "Function").await, 1, "source chunk never pruned");
        assert_eq!(count(&pool, "Fact").await, 2, "a-3 + b-1 remain; a-1 pruned");
        assert_eq!(count(&pool, "SessionSummary").await, 3, "a-2 + b-2 + c-1 remain");
    }

    #[tokio::test]
    async fn prune_applies_the_age_window_to_derived_rows_only() {
        let (pool, store, manager) = test_manager().await;
        let project = ProjectId("retention".into());
        let token = CancellationToken::new();

        seed(&store, "old-fact", ChunkType::Fact, "sess-a", "stale summary", offset_days_ago(40))
            .await;
        seed(
            &store,
            "fresh-fact",
            ChunkType::Fact,
            "sess-a",
            "fresh fact",
            OffsetDateTime::now_utc(),
        )
        .await;
        seed(
            &store,
            "old-summary",
            ChunkType::SessionSummary,
            "sess-a",
            "stale summary",
            offset_days_ago(40),
        )
        .await;
        seed(
            &store,
            "fresh-summary",
            ChunkType::SessionSummary,
            "sess-a",
            "fresh summary",
            OffsetDateTime::now_utc(),
        )
        .await;
        seed_source(&store, "old-fn", offset_days_ago(400)).await;

        let report = manager.prune_derived_summaries(&project, 0, 30, token.clone()).await.unwrap();
        let mut pruned = report.pruned_ids.clone();
        pruned.sort();
        assert_eq!(
            pruned,
            vec!["old-fact".to_string(), "old-summary".to_string()],
            "age window prunes derived rows past the cutoff only"
        );

        assert_eq!(count(&pool, "Fact").await, 1, "fresh fact kept");
        assert_eq!(count(&pool, "SessionSummary").await, 1, "fresh summary kept");
        assert_eq!(count(&pool, "Function").await, 1, "old source chunk NEVER window-pruned");
    }

    #[tokio::test]
    async fn prune_with_both_rules_disabled_is_a_no_op() {
        let (_pool, store, manager) = test_manager().await;
        let project = ProjectId("retention".into());
        let token = CancellationToken::new();
        seed(&store, "f-1", ChunkType::Fact, "sess-x", "keep me", offset_days_ago(1000)).await;
        let report = manager.prune_derived_summaries(&project, 0, 0, token.clone()).await.unwrap();
        assert!(report.pruned_ids.is_empty(), "cap 0 + window 0 disable both rules");
    }

    #[tokio::test]
    async fn prune_on_a_cancelled_token_stops_immediately() {
        let (_pool, store, manager) = test_manager().await;
        let project = ProjectId("retention".into());
        seed(&store, "f-1", ChunkType::Fact, "sess-x", "x", OffsetDateTime::now_utc()).await;
        let token = CancellationToken::new();
        token.cancel();
        let report = manager.prune_derived_summaries(&project, 2, 0, token).await.unwrap();
        assert!(report.pruned_ids.is_empty(), "cancelled pass prunes nothing");
    }
}
