//! Symbolic link store (ADR-69 slice 1).
//!
//! A pool-backed SQLite store for directed, typed edges between memory
//! chunks — the `memory_links` table created by migration 002, sharing the
//! project's SQLite pool with the vector store. Slice 1 only *writes* links
//! (idempotently, fail-open); scoring, decay, and TTL belong to slice 2.

use concerto_core::error::MemoryError;
use concerto_core::memory::{MemoryLink, MemoryLinkKind};
use concerto_core::CancellationToken;
use sqlx::{Row, SqlitePool};
use time::OffsetDateTime;

/// Default per-chunk out-degree cap (ADR-69 A2).
///
/// Slice 2 enforces this cap (and makes it configurable) — slice 1 only
/// exposes the constant so the enforcement point has one obvious home and a
/// cap is never skipped for lack of a number. Not enforced here.
pub const DEFAULT_MAX_OUT_DEGREE: usize = 32;

/// Mirrors migration 002 so a pool created directly (without the migration
/// runner) still works; the migration stays the canonical path.
const CREATE_MEMORY_LINKS: &str = r#"
CREATE TABLE IF NOT EXISTS memory_links (
    source_id  TEXT    NOT NULL,
    target_id  TEXT    NOT NULL,
    link_type  TEXT    NOT NULL,
    weight     REAL    NOT NULL DEFAULT 1.0,
    created_at TEXT    NOT NULL,
    expires_at TEXT,
    PRIMARY KEY (source_id, target_id, link_type)
)
"#;

/// Pool-backed store for `memory_links` rows (ADR-69 slice 1).
pub struct LinkStore {
    pool: SqlitePool,
}

impl LinkStore {
    /// Create a link store over the given pool, ensuring the table exists.
    pub async fn new(pool: SqlitePool) -> Result<Self, MemoryError> {
        sqlx::query(CREATE_MEMORY_LINKS)
            .execute(&pool)
            .await
            .map_err(|e| MemoryError::Persistence(e.to_string()))?;
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_memory_links_target ON memory_links(target_id)",
        )
        .execute(&pool)
        .await
        .map_err(|e| MemoryError::Persistence(e.to_string()))?;
        Ok(Self { pool })
    }

    /// Persist one link.
    ///
    /// Idempotent (ADR-69 A1): re-writing the same `(source, target, type)`
    /// row upserts its weight instead of duplicating it. The first write's
    /// `created_at` is kept.
    pub async fn put(
        &self,
        link: &MemoryLink,
        cancel: CancellationToken,
    ) -> Result<(), MemoryError> {
        if cancel.is_cancelled() {
            return Err(MemoryError::Cancelled);
        }
        let created_at = OffsetDateTime::now_utc().to_string();
        sqlx::query(
            r#"
            INSERT INTO memory_links (source_id, target_id, link_type, weight, created_at)
            VALUES (?, ?, ?, ?, ?)
            ON CONFLICT(source_id, target_id, link_type) DO UPDATE SET
                weight = excluded.weight
            "#,
        )
        .bind(&link.from)
        .bind(&link.to)
        .bind(link.kind.as_str())
        .bind(link.weight)
        .bind(&created_at)
        .execute(&self.pool)
        .await
        .map_err(|e| MemoryError::Persistence(format!("failed to persist memory link: {e}")))?;
        Ok(())
    }

    /// Persist many links in order.
    ///
    /// Slice-1 callers treat any error as advisory (fail-open), so no
    /// per-link best-effort is done here — an error stops the batch and the
    /// caller logs and continues with the memory store.
    pub async fn put_many(
        &self,
        links: &[MemoryLink],
        cancel: CancellationToken,
    ) -> Result<(), MemoryError> {
        for link in links {
            self.put(link, cancel.clone()).await?;
        }
        Ok(())
    }

    /// All links whose source is `chunk_id`, oldest first (stable order).
    pub async fn links_from(
        &self,
        chunk_id: &str,
        cancel: CancellationToken,
    ) -> Result<Vec<MemoryLink>, MemoryError> {
        if cancel.is_cancelled() {
            return Err(MemoryError::Cancelled);
        }
        let rows = sqlx::query(
            "SELECT source_id, target_id, link_type, weight FROM memory_links \
             WHERE source_id = ? ORDER BY created_at ASC, rowid ASC",
        )
        .bind(chunk_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| MemoryError::RetrievalFailed(e.to_string()))?;
        rows.iter().map(row_to_link).collect()
    }

    /// All links whose target is `chunk_id` (the in-degree that slice-2
    /// scoring reads).
    pub async fn links_to(
        &self,
        chunk_id: &str,
        cancel: CancellationToken,
    ) -> Result<Vec<MemoryLink>, MemoryError> {
        if cancel.is_cancelled() {
            return Err(MemoryError::Cancelled);
        }
        let rows = sqlx::query(
            "SELECT source_id, target_id, link_type, weight FROM memory_links \
             WHERE target_id = ? ORDER BY created_at ASC, rowid ASC",
        )
        .bind(chunk_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| MemoryError::RetrievalFailed(e.to_string()))?;
        rows.iter().map(row_to_link).collect()
    }

    /// Number of links leaving `chunk_id` — the measure slice 2's degree cap
    /// compares against [`DEFAULT_MAX_OUT_DEGREE`].
    pub async fn out_degree(
        &self,
        chunk_id: &str,
        cancel: CancellationToken,
    ) -> Result<usize, MemoryError> {
        if cancel.is_cancelled() {
            return Err(MemoryError::Cancelled);
        }
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM memory_links WHERE source_id = ?")
                .bind(chunk_id)
                .fetch_one(&self.pool)
                .await
                .map_err(|e| MemoryError::RetrievalFailed(e.to_string()))?;
        Ok(count as usize)
    }

    /// Test-only: close the underlying pool so a subsequent write deterministically
    /// fails — exercises the write-path fail-open contract.
    #[cfg(test)]
    pub(crate) async fn close_pool(&self) {
        self.pool.close().await;
    }
}

fn row_to_link(row: &sqlx::sqlite::SqliteRow) -> Result<MemoryLink, MemoryError> {
    let from: String = row.get("source_id");
    let to: String = row.get("target_id");
    let kind_str: String = row.get("link_type");
    let kind = MemoryLinkKind::parse(&kind_str).ok_or_else(|| {
        MemoryError::RetrievalFailed(format!(
            "unknown memory link type '{kind_str}' on {from} -> {to}"
        ))
    })?;
    Ok(MemoryLink { from, to, kind, weight: row.get("weight") })
}

/// Derive `References` links from entry metadata on the merge/update write
/// paths (ADR-69 slice 1):
///
/// - `metadata["refs"]` — an array of chunk ids the entry references.
/// - `metadata["result_ref"]` — a single chunk id the entry is a result of.
///
/// Both map to a default-weight `References` link from `from`. Malformed or
/// missing values are skipped, never an error — consistent with the slice-1
/// fail-open contract.
pub fn links_from_metadata(from: &str, metadata: &serde_json::Value) -> Vec<MemoryLink> {
    let mut links = Vec::new();
    if let Some(refs) = metadata.get("refs").and_then(|value| value.as_array()) {
        for value in refs {
            if let Some(chunk_id) = value.as_str().filter(|id| !id.is_empty()) {
                links.push(MemoryLink::new(from, chunk_id, MemoryLinkKind::References));
            }
        }
    }
    if let Some(result_ref) =
        metadata.get("result_ref").and_then(|value| value.as_str()).filter(|id| !id.is_empty())
    {
        links.push(MemoryLink::new(from, result_ref, MemoryLinkKind::References));
    }
    links
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_store() -> LinkStore {
        // `max_connections(1)` so every :memory: checkout sees the same DB.
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        LinkStore::new(pool).await.unwrap()
    }

    #[test]
    fn metadata_links_read_refs_and_result_ref() {
        let metadata = serde_json::json!({
            "refs": ["chunk-a", "chunk-b"],
            "result_ref": "chunk-c",
            "unrelated": "ignored"
        });
        let links = links_from_metadata("src-1", &metadata);
        assert_eq!(links.len(), 3);
        assert!(links.iter().all(|link| link.kind == MemoryLinkKind::References));
        assert!(links.iter().all(|link| link.weight == 1.0));
        assert_eq!(links[0].to, "chunk-a");
        assert_eq!(links[1].to, "chunk-b");
        assert_eq!(links[2].to, "chunk-c");
    }

    #[test]
    fn metadata_links_fail_open_on_wrong_shapes() {
        let metadata = serde_json::json!({
            "refs": "chunk-a",           // not an array
            "result_ref": ["chunk-c"],   // not a string
        });
        assert!(links_from_metadata("src-1", &metadata).is_empty());
        assert!(links_from_metadata("src-1", &serde_json::Value::Null).is_empty());
        assert!(links_from_metadata("src-1", &serde_json::json!({})).is_empty());
        // Empty / blank ids are skipped.
        let blank = serde_json::json!({"refs": ["", "ok"], "result_ref": ""});
        let links = links_from_metadata("src-1", &blank);
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].to, "ok");
    }

    #[tokio::test]
    async fn put_and_read_round_trip() {
        let store = test_store().await;
        let link = MemoryLink::new("src-1", "tgt-1", MemoryLinkKind::Supersedes);
        store.put(&link, CancellationToken::new()).await.unwrap();

        let from = store.links_from("src-1", CancellationToken::new()).await.unwrap();
        assert_eq!(from, vec![link.clone()]);
        let to = store.links_to("tgt-1", CancellationToken::new()).await.unwrap();
        assert_eq!(to, vec![link.clone()]);
        assert_eq!(store.out_degree("src-1", CancellationToken::new()).await.unwrap(), 1);
        assert_eq!(store.out_degree("tgt-1", CancellationToken::new()).await.unwrap(), 0);
        assert_eq!(store.links_from("nobody", CancellationToken::new()).await.unwrap().len(), 0);
    }

    #[tokio::test]
    async fn put_is_idempotent_and_upserts_weight() {
        let store = test_store().await;
        let mut link = MemoryLink::new("src-1", "tgt-1", MemoryLinkKind::References);
        store.put(&link, CancellationToken::new()).await.unwrap();
        link.weight = 0.5;
        store.put(&link, CancellationToken::new()).await.unwrap();

        let from = store.links_from("src-1", CancellationToken::new()).await.unwrap();
        assert_eq!(from.len(), 1, "re-writing a link must not duplicate it");
        assert_eq!(from[0].weight, 0.5, "the weight must be updated in place");
    }

    #[tokio::test]
    async fn put_many_persists_all() {
        let store = test_store().await;
        let links = vec![
            MemoryLink::new("src-1", "a", MemoryLinkKind::References),
            MemoryLink::new("src-1", "b", MemoryLinkKind::Merges),
        ];
        store.put_many(&links, CancellationToken::new()).await.unwrap();
        let from = store.links_from("src-1", CancellationToken::new()).await.unwrap();
        assert_eq!(from.len(), 2);
        assert_eq!(store.out_degree("src-1", CancellationToken::new()).await.unwrap(), 2);
    }

    #[tokio::test]
    async fn cancelled_put_is_rejected() {
        let store = test_store().await;
        let cancel = CancellationToken::new();
        cancel.cancel();
        let link = MemoryLink::new("src-1", "tgt-1", MemoryLinkKind::References);
        assert!(matches!(store.put(&link, cancel).await, Err(MemoryError::Cancelled)));
    }
}
