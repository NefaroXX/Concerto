//! TTL and staleness management for the memory system.
//!
//! Manages:
//! - Time-to-live expiry of memory chunks
//! - Stale embeddings (model version mismatch)
//! - Re-index scheduling for outdated chunks
//! - Reclaiming storage from expired entries

use concerto_core::CancellationToken;
use std::sync::Arc;

use concerto_core::error::MemoryError;
use concerto_core::memory::ProjectId;
use sqlx::SqlitePool;
use sqlx::{AssertSqlSafe, Row};

use crate::fts::FullTextStore;
use crate::vector_store::VectorStore;

/// Default TTL for different chunk types (in days).
pub const TTL_FUNCTION_DAYS: i64 = 90;
pub const TTL_STRUCT_DAYS: i64 = 90;
pub const TTL_FILE_DAYS: i64 = 60;
pub const TTL_SLIDING_WINDOW_DAYS: i64 = 30;
pub const TTL_DECISION_DAYS: i64 = 365;

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
    use concerto_core::memory::ChunkType;

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
}
