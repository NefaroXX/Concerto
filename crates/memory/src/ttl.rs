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
use sqlx::Row;
use sqlx::SqlitePool;
use time::OffsetDateTime;

use crate::fts::FullTextStore;
use crate::links::LinkStore;
use crate::scoring::{chunk_link_score, parse_db_timestamp, ORPHAN_TRIM_SCORE};
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

/// Result of one [`TtlManager::prune_derived_summaries`] pass (ADR-65 §8)
/// or [`TtlManager::prune_orphaned_derived`] (ADR-69 slice 2): the derived
/// chunk ids removed and how many session buckets were examined (retention
/// pass only). Source chunks are never reported here — they are never touched.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PruneReport {
    /// Chunk ids removed (vector row + FTS entry) in removal order.
    pub pruned_ids: Vec<String>,
    /// Distinct session buckets (including the project-wide fallback bucket)
    /// observed during the pass.
    pub sessions_examined: usize,
}

/// One DERIVED vector row (`Fact` / `SessionSummary`) as selected by
/// [`TtlManager::load_derived_rows`].
struct DerivedRow {
    id: String,
    created_at: String,
    metadata: Option<String>,
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
        if cancel.is_cancelled() {
            return Ok(0);
        }

        // Load every live row and decide expiry in Rust instead of comparing
        // in SQL. The stored `created_at` is `OffsetDateTime::to_string()` — a
        // suffixed format (e.g. `2023-11-14 22:13:20.0 +00:00:00`) SQLite's
        // `datetime()` cannot parse, so the old
        // `datetime(created_at, '+N days') < datetime('now')` predicate was a
        // silent never-match. `parse_db_timestamp` handles the exact stored
        // shape and normalizes any stored UTC offset before comparing.
        let rows = sqlx::query(
            "SELECT id, chunk_type, created_at FROM vector_store \
             WHERE project_id = ? AND tombstone = 0",
        )
        .bind(&project_id.0)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| MemoryError::RetrievalFailed(e.to_string()))?;

        let now_unix = OffsetDateTime::now_utc().unix_timestamp();
        let mut expired: Vec<String> = Vec::new();
        for row in rows {
            let chunk_type: String = row.get("chunk_type");
            let created_at: String = row.get("created_at");
            let ttl_days = self
                .default_ttl_days
                .map_or_else(|| ttl_days_for_chunk_type(&chunk_type), i64::from);
            // Fail-open: a row whose timestamp cannot be parsed is never
            // purged (same contract as `TimedLink::from_db`).
            let Some(created) = parse_db_timestamp(&created_at) else {
                continue;
            };
            let cutoff_unix = now_unix - time::Duration::days(ttl_days).whole_seconds();
            if created.unix_timestamp() < cutoff_unix {
                expired.push(row.get("id"));
            }
        }

        if expired.is_empty() {
            return Ok(0);
        }

        let count = expired.len();

        // Tombstone each expired entry in the vector store and delete from FTS.
        for chunk_id in &expired {
            self.vector_store.tombstone(chunk_id, project_id, cancel.clone()).await?;
            let _ = self.fts_store.delete(chunk_id, project_id, cancel.clone()).await;
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
        // Mark stale in the vector store (sets stale=1 for rows whose
        // `model_version` MISMATCHES the current model — model-bump refresh).
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
        let rows = self.load_derived_rows(project_id, cancel.clone()).await?;
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
            let bucket = summary_session_bucket(row.metadata.as_deref());
            buckets.insert(bucket.clone());
            let used = kept.entry(bucket).or_insert(0);
            let over_cap = if keep_per_session > 0 && *used >= keep_per_session {
                true
            } else {
                *used += 1;
                false
            };
            let expired =
                cutoff.as_ref().is_some_and(|cutoff| row.created_at.as_str() < cutoff.as_str());
            if over_cap || expired {
                pruned.push(row.id);
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

    /// ADR-69 slice 2 orphan-evidence prune: delete DERIVED rows (`Fact` /
    /// `SessionSummary`) that carry INCOMING link evidence but score at or
    /// below [`ORPHAN_TRIM_SCORE`] after decay (0.5 on the 0–10 scale) —
    /// "cold but linked" consolidations the decay model says are no longer
    /// worth retaining.
    ///
    /// Unlinked derived rows and SOURCE chunks are never touched. Fail-open
    /// by contract: a link-store error logs a warning and prunes nothing, so
    /// this pass can never corrupt retention or drop chunks it cannot judge.
    pub async fn prune_orphaned_derived(
        &self,
        project_id: &ProjectId,
        link_store: &LinkStore,
        decay_days: Option<u16>,
        cancel: CancellationToken,
    ) -> Result<PruneReport, MemoryError> {
        let mut report = PruneReport::default();
        if cancel.is_cancelled() {
            return Ok(report);
        }
        let rows = self.load_derived_rows(project_id, cancel.clone()).await?;
        if rows.is_empty() {
            return Ok(report);
        }

        let ids: Vec<String> = rows.iter().map(|row| row.id.clone()).collect();
        let incoming = match link_store.incoming_links(&ids, cancel.clone()).await {
            Ok(map) => map,
            Err(error) => {
                tracing::warn!(
                    %error,
                    "orphaned-derived prune skipped: link store unavailable (fail-open)"
                );
                return Ok(report);
            }
        };
        if incoming.is_empty() {
            return Ok(report);
        }

        let now_unix = OffsetDateTime::now_utc().unix_timestamp();
        for row in rows {
            if cancel.is_cancelled() {
                break;
            }
            let score = match incoming.get(&row.id) {
                Some(links) if !links.is_empty() => chunk_link_score(links, decay_days, now_unix),
                _ => continue, // unlinked rows are never touched
            };
            if score <= ORPHAN_TRIM_SCORE {
                report.pruned_ids.push(row.id);
            }
        }

        self.remove_derived_rows(project_id, &report.pruned_ids, cancel).await?;
        if !report.pruned_ids.is_empty() {
            tracing::info!(
                pruned = report.pruned_ids.len(),
                "pruned low-score derived rows with cold link evidence (ADR-69 slice 2)"
            );
        }
        Ok(report)
    }

    /// Bare ADR-65 §8 row selector: the project's DERIVED (`Fact` /
    /// `SessionSummary`) non-tombstoned vector rows, newest first. Shared by
    /// the retention cap/window prune and the ADR-69 orphan-evidence prune.
    async fn load_derived_rows(
        &self,
        project_id: &ProjectId,
        cancel: CancellationToken,
    ) -> Result<Vec<DerivedRow>, MemoryError> {
        if cancel.is_cancelled() {
            return Ok(Vec::new());
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
        Ok(rows
            .into_iter()
            .map(|row| DerivedRow {
                id: row.get("id"),
                created_at: row.get("created_at"),
                metadata: row.get("metadata"),
            })
            .collect())
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

/// TTL (in days) for a `vector_store.chunk_type` debug string (the
/// `{chunk_type:?}` the write path binds). Mirrors the SQL CASE
/// `purge_expired` previously built — unknown types fall back to the file
/// TTL. The per-type windows are identical to [`suggested_ttl_days`].
fn ttl_days_for_chunk_type(chunk_type: &str) -> i64 {
    match chunk_type {
        "Function" | "Trait" | "Impl" => TTL_FUNCTION_DAYS,
        "Struct" => TTL_STRUCT_DAYS,
        "SessionSummary" | "Fact" => TTL_DECISION_DAYS,
        "SlidingWindow" => TTL_SLIDING_WINDOW_DAYS,
        _ => TTL_FILE_DAYS,
    }
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

    /// Seed a row with an EXPLICIT embedding model version (seed writes "1").
    async fn seed_version(store: &SqliteVectorStore, id: &str, model_version: &str) {
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
            model_version: model_version.into(),
            stale: false,
            created_at: OffsetDateTime::now_utc(),
        };
        store.store(&[record], CancellationToken::new()).await.unwrap();
    }

    #[tokio::test]
    async fn mark_stale_embeddings_flags_model_version_mismatches() {
        let (_pool, store, manager) = test_manager().await;
        let project = ProjectId("retention".into());
        let token = CancellationToken::new();

        // Two rows produced by the previous model version, one by the
        // current. A model-bump refresh must flag exactly the MISMATCHING
        // rows (stale=1 → tombstoned out of search) and leave the rows
        // carrying the CURRENT version fresh. The inverted `=` match marked
        // the fresh row and skipped everything actually needing re-index.
        seed_version(&store, "old-1", "1").await;
        seed_version(&store, "old-2", "1").await;
        seed_version(&store, "fresh-1", "2").await;

        let stale = manager.mark_stale_embeddings(&project, "2", token.clone()).await.unwrap();
        assert_eq!(stale, 2, "exactly the two version-1 rows go stale, got {stale}");

        let survivors = store.get_chunks(&project, &["fresh-1".into()], token).await.unwrap();
        assert_eq!(survivors.len(), 1, "the current-version row never tombstones: {survivors:?}");
        let gone =
            store.get_chunks(&project, &["old-1".into()], CancellationToken::new()).await.unwrap();
        assert!(gone.is_empty(), "the mismatching row drops out of search: {gone:?}");
    }

    // ---------------------------------------------------------------------
    // purge_expired — Rust-side timestamp comparison (created_at is stored
    // as `OffsetDateTime::to_string()`, which SQLite datetime() cannot parse)
    // ---------------------------------------------------------------------

    #[tokio::test]
    async fn purge_expired_removes_only_rows_past_their_chunk_type_ttl() {
        let (pool, store, manager) = test_manager().await;
        let project = ProjectId("retention".into());
        let token = CancellationToken::new();

        seed_source(&store, "fn-old", offset_days_ago(200)).await;
        seed_source(&store, "fn-fresh", OffsetDateTime::now_utc()).await;
        seed(&store, "fact-tween", ChunkType::Fact, "s", "fact 200d", offset_days_ago(200)).await;
        seed(&store, "sw-old", ChunkType::SlidingWindow, "s", "sw 60d", offset_days_ago(60)).await;
        seed(&store, "struct-old", ChunkType::Struct, "s", "struct 95d", offset_days_ago(95)).await;

        let purged = manager.purge_expired(&project, token.clone()).await.unwrap();
        assert_eq!(
            purged, 3,
            "fn-old(200d>90), sw-old(60d>30) and struct-old(95d>90) expire; \
             fn-fresh and the 200-day-old Fact (TTL 365) stay"
        );

        assert_eq!(count(&pool, "Function").await, 1, "fn-fresh survives");
        assert_eq!(count(&pool, "Fact").await, 1, "fact-tween is inside its 365-day window");
        assert_eq!(count(&pool, "SlidingWindow").await, 0, "sw-old hard-deleted");
        assert_eq!(count(&pool, "Struct").await, 0, "struct-old hard-deleted");

        // Reviewed: only rows past their own per-type TTL are removed — the
        // freshly stored rows and in-window derived rows are intact.
        let survivors = store
            .get_chunks(&project, &["fn-fresh".into(), "fact-tween".into()], token)
            .await
            .unwrap();
        assert_eq!(survivors.len(), 2);
    }

    #[tokio::test]
    async fn purge_expired_skips_rows_with_unparseable_timestamps() {
        let (pool, store, manager) = test_manager().await;
        let project = ProjectId("retention".into());
        let token = CancellationToken::new();

        seed_source(&store, "fn-corrupt", OffsetDateTime::now_utc()).await;
        seed_source(&store, "fn-expired", offset_days_ago(200)).await;
        sqlx::query("UPDATE vector_store SET created_at = 'garbage' WHERE id = 'fn-corrupt'")
            .execute(&pool)
            .await
            .unwrap();

        let purged = manager.purge_expired(&project, token.clone()).await.unwrap();
        assert_eq!(purged, 1, "the expired row is purged, the corrupt one is skipped");

        assert_eq!(count(&pool, "Function").await, 1, "unparseable row survives (fail-open)");
        let surviving = store.get_chunks(&project, &["fn-corrupt".into()], token).await.unwrap();
        assert_eq!(surviving.len(), 1);
    }

    #[tokio::test]
    async fn purge_expired_applies_the_default_ttl_to_every_chunk_type() {
        let (pool, store, _manager) = test_manager().await;
        let project = ProjectId("retention".into());
        let token = CancellationToken::new();
        let fts = Arc::new(SqliteFullTextStore::new(pool.clone()).await.unwrap());
        let manager = TtlManager::with_default_ttl_days(store.clone(), fts, pool.clone(), 10);

        // A 15-day-old Fact normally has a 365-day TTL, but the configured
        // default (10 days) overrides every chunk type.
        seed(&store, "fact-old", ChunkType::Fact, "s", "old", offset_days_ago(15)).await;
        seed(&store, "fact-fresh", ChunkType::Fact, "s", "fresh", OffsetDateTime::now_utc()).await;
        seed_source(&store, "fn-old", offset_days_ago(15)).await;
        seed_source(&store, "fn-fresh", OffsetDateTime::now_utc()).await;

        let purged = manager.purge_expired(&project, token.clone()).await.unwrap();
        assert_eq!(purged, 2, "default TTL applies to both old rows regardless of type");
        assert_eq!(count(&pool, "Fact").await, 1, "fact-fresh survives");
        assert_eq!(count(&pool, "Function").await, 1, "fn-fresh survives");
    }

    // ---------------------------------------------------------------------
    // ADR-69 slice 2 — orphan-evidence prune (prune_orphaned_derived)
    // ---------------------------------------------------------------------

    /// Insert a link with an EXPLICIT `created_at`. `LinkStore::put` stamps
    /// `now_utc` internally, so cold-evidence tests must write the row
    /// directly (same table, same columns the canonical path uses).
    async fn seed_link(
        pool: &sqlx::SqlitePool,
        from: &str,
        to: &str,
        kind: concerto_core::memory::MemoryLinkKind,
        created_at: OffsetDateTime,
    ) {
        sqlx::query(
            "INSERT INTO memory_links (source_id, target_id, link_type, weight, created_at) \
             VALUES (?, ?, ?, 1.0, ?)",
        )
        .bind(from)
        .bind(to)
        .bind(kind.as_str())
        .bind(created_at.to_string())
        .execute(pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn prune_orphaned_derived_removes_cold_linked_rows_only() {
        let (pool, store, manager) = test_manager().await;
        let project = ProjectId("retention".into());
        let token = CancellationToken::new();
        let link_store = LinkStore::new(pool.clone()).await.unwrap();

        // Two derived rows, one with fresh evidence and one with COLD
        // evidence; one derived row with no links at all; one SOURCE chunk
        // with an old link (must never prune).
        seed(&store, "cold-linked", ChunkType::Fact, "s", "old fact", offset_days_ago(30)).await;
        seed(&store, "fresh-linked", ChunkType::Fact, "s", "new fact", OffsetDateTime::now_utc())
            .await;
        seed(&store, "unlinked", ChunkType::Fact, "s", "no links", OffsetDateTime::now_utc()).await;
        seed_source(&store, "fn-src", offset_days_ago(30)).await;
        seed_link(
            &pool,
            "src-cold",
            "cold-linked",
            concerto_core::memory::MemoryLinkKind::Supports,
            offset_days_ago(120),
        )
        .await;
        seed_link(
            &pool,
            "src-fresh",
            "fresh-linked",
            concerto_core::memory::MemoryLinkKind::Supports,
            OffsetDateTime::now_utc(),
        )
        .await;
        seed_link(
            &pool,
            "src-old-fn",
            "fn-src",
            concerto_core::memory::MemoryLinkKind::Supports,
            offset_days_ago(120),
        )
        .await;

        let report = manager
            .prune_orphaned_derived(&project, &link_store, Some(90), token.clone())
            .await
            .unwrap();
        assert_eq!(report.pruned_ids, vec!["cold-linked".to_string()]);

        assert_eq!(count(&pool, "Fact").await, 2, "fresh-linked + unlinked remain");
        assert_eq!(count(&pool, "Function").await, 1, "source chunk with old links never pruned");
        assert!(
            store.get_chunks(&project, &["fresh-linked".into()], token).await.unwrap().len() == 1
        );
        assert!(
            store
                .get_chunks(&project, &["unlinked".into()], CancellationToken::new())
                .await
                .unwrap()
                .len()
                == 1
        );
    }

    #[tokio::test]
    async fn prune_orphaned_derived_with_decay_disabled_prunes_nothing() {
        let (pool, store, manager) = test_manager().await;
        let project = ProjectId("retention".into());
        let token = CancellationToken::new();
        let link_store = LinkStore::new(pool.clone()).await.unwrap();

        seed(&store, "cold-linked", ChunkType::Fact, "s", "old fact", offset_days_ago(30)).await;
        seed_link(
            &pool,
            "src-cold",
            "cold-linked",
            concerto_core::memory::MemoryLinkKind::Supports,
            offset_days_ago(120),
        )
        .await;

        // Decay disabled: even 120-day-old evidence scores fresh (1.0) →
        // 4.375 > ORPHAN_TRIM_SCORE, so nothing is pruned.
        let report = manager
            .prune_orphaned_derived(&project, &link_store, None, token.clone())
            .await
            .unwrap();
        assert!(report.pruned_ids.is_empty(), "no decay means no cold rows");
        assert_eq!(count(&pool, "Fact").await, 1);
    }

    #[tokio::test]
    async fn prune_orphaned_derived_fails_open_when_link_store_unavailable() {
        let (_pool, store, manager) = test_manager().await;
        let project = ProjectId("retention".into());
        let token = CancellationToken::new();

        seed(&store, "cold-linked", ChunkType::Fact, "s", "old fact", offset_days_ago(30)).await;

        // A link store whose pool is already closed: every in-degree read
        // fails, and the prune must degrade to "nothing pruned".
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("links.db");
        let options =
            sqlx::sqlite::SqliteConnectOptions::new().filename(&path).create_if_missing(true);
        let pool = sqlx::SqlitePool::connect_with(options).await.unwrap();
        let link_store = LinkStore::new(pool).await.unwrap();
        link_store.close_pool().await;

        let report =
            manager.prune_orphaned_derived(&project, &link_store, Some(90), token).await.unwrap();
        assert!(report.pruned_ids.is_empty(), "fail-open must prune nothing");
        let gone = store
            .get_chunks(&project, &["cold-linked".into()], CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(gone.len(), 1, "the row survives an unavailable link store");
    }
}
