//! Symbolic link store (ADR-69 slices 1–2).
//!
//! A pool-backed SQLite store for directed, typed edges between memory
//! chunks — the `memory_links` table created by migration 002, sharing the
//! project's SQLite pool with the vector store. Slice 1 writes links
//! (idempotently, fail-open); slice 2 adds the out-degree cap on writes
//! (ADR-69 A2) and the batched, timestamped in-degree read that the cascade
//! scorer and orphan prune consume.

use concerto_core::error::MemoryError;
use concerto_core::memory::{MemoryLink, MemoryLinkKind};
use concerto_core::CancellationToken;
use sqlx::{AssertSqlSafe, Row, SqlitePool};
use std::collections::HashMap;
use time::OffsetDateTime;

use crate::scoring::TimedLink;

/// Default per-chunk out-degree cap (ADR-69 A2), applied by [`LinkStore::put`]
/// when no explicit cap is configured: a chunk may point at at most this many
/// other chunks before further NEW links are rejected with a warning.
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

/// Pool-backed store for `memory_links` rows (ADR-69 slices 1–2).
pub struct LinkStore {
    pool: SqlitePool,
    /// New-link rejection threshold per source (ADR-69 A2). Always ≥ 1.
    max_out_degree: usize,
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
        Ok(Self { pool, max_out_degree: DEFAULT_MAX_OUT_DEGREE })
    }

    /// DDL-free constructor for read-only consumers in sibling modules
    /// (e.g. [`crate::mermaid`] graph loading): assumes the `memory_links`
    /// table already exists and never creates it.
    pub(crate) fn from_pool(pool: SqlitePool) -> Self {
        Self { pool, max_out_degree: DEFAULT_MAX_OUT_DEGREE }
    }

    /// Bound how many distinct NEW links a source may hold (ADR-69 A2).
    ///
    /// Re-writing an already-present `(source, target, type)` triple stays
    /// allowed (idempotent update). The cap is floored at 1 — a zero cap would
    /// make the store unusable rather than "no links".
    pub fn with_max_out_degree(mut self, cap: usize) -> Self {
        self.max_out_degree = cap.max(1);
        self
    }

    /// Persist one link.
    ///
    /// Idempotent (ADR-69 A1): re-writing the same `(source, target, type)`
    /// row upserts its weight instead of duplicating it. The first write's
    /// `created_at` is kept.
    ///
    /// Out-degree cap (ADR-69 A2): a NEW row is rejected with a warning once
    /// the source already holds [`LinkStore::max_out_degree`] links. The
    /// rejection is fail-open — `Ok(())`, nothing persisted — so callers
    /// treat it as advisory, never as an error that could drop a memory.
    pub async fn put(
        &self,
        link: &MemoryLink,
        cancel: CancellationToken,
    ) -> Result<(), MemoryError> {
        if cancel.is_cancelled() {
            return Err(MemoryError::Cancelled);
        }
        // Re-writing an already-present triple is an idempotent update and is
        // always allowed — the cap only gates genuinely new edges.
        let already_present: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM memory_links \
             WHERE source_id = ? AND target_id = ? AND link_type = ?",
        )
        .bind(&link.from)
        .bind(&link.to)
        .bind(link.kind.as_str())
        .fetch_one(&self.pool)
        .await
        .map_err(|e| MemoryError::RetrievalFailed(e.to_string()))?;

        if already_present == 0
            && self.out_degree(&link.from, cancel.clone()).await? >= self.max_out_degree
        {
            tracing::warn!(
                source_id = %link.from,
                target_id = %link.to,
                link_type = link.kind.as_str(),
                cap = self.max_out_degree,
                "ADR-69 out-degree cap reached; link rejected (fail-open)"
            );
            return Ok(());
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

    /// In-degree for MANY chunk ids at once, with each link's creation moment
    /// decoded to Unix seconds — the batched read the cascade scorer
    /// ([`crate::rag::retrieve_with_cascade`]) and the orphan prune
    /// ([`crate::ttl::prune_orphaned_derived`]) consume.
    ///
    /// One batched query per window of ids (bounded well under SQLite's bind
    /// limit) instead of one query per id. Unknown link types and unparseable
    /// `created_at` strings are skipped — never an error — matching the
    /// fail-open read contract established by [`row_to_link`].
    ///
    /// Returns a map keyed by target chunk id; absent ids simply have no
    /// entry.
    pub async fn incoming_links(
        &self,
        chunk_ids: &[String],
        cancel: CancellationToken,
    ) -> Result<HashMap<String, Vec<TimedLink>>, MemoryError> {
        if cancel.is_cancelled() {
            return Err(MemoryError::Cancelled);
        }
        let mut by_target: HashMap<String, Vec<TimedLink>> = HashMap::new();
        if chunk_ids.is_empty() {
            return Ok(by_target);
        }

        // SQLite's default bind-parameter limit is 999; stay well under it so
        // any caller-supplied batch size is safe.
        const BATCH: usize = 200;
        for window in chunk_ids.chunks(BATCH) {
            let placeholders = vec!["?"; window.len()].join(", ");
            let sql = format!(
                "SELECT source_id, target_id, link_type, weight, created_at \
                 FROM memory_links WHERE target_id IN ({placeholders}) \
                 ORDER BY created_at ASC, rowid ASC"
            );
            // AUDITED (sqlx 0.9 `AssertSqlSafe`): the SQL is built from static
            // fragments and a fixed `?` placeholder list; every filter value is
            // bound below, so no user input reaches the statement text.
            let mut query = sqlx::query(AssertSqlSafe(sql));
            for id in window {
                query = query.bind(id.as_str());
            }
            let rows = query
                .fetch_all(&self.pool)
                .await
                .map_err(|e| MemoryError::RetrievalFailed(e.to_string()))?;
            for row in &rows {
                let kind_str: String = row.get("link_type");
                let Some(kind) = MemoryLinkKind::parse(&kind_str) else {
                    continue;
                };
                let link = MemoryLink {
                    from: row.get("source_id"),
                    to: row.get("target_id"),
                    kind,
                    weight: row.get("weight"),
                };
                let created_at: String = row.get("created_at");
                if let Some(timed) = TimedLink::from_db(link, &created_at) {
                    by_target.entry(timed.link.to.clone()).or_default().push(timed);
                }
            }
        }
        Ok(by_target)
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

    #[tokio::test]
    async fn put_enforces_default_out_degree_cap_of_32() {
        let store = test_store().await;
        let cancel = CancellationToken::new();
        // The 32nd edge fits; the 33rd is rejected fail-open with Ok(()).
        for i in 0..DEFAULT_MAX_OUT_DEGREE {
            store
                .put(
                    &MemoryLink::new("src-1", format!("tgt-{i}"), MemoryLinkKind::References),
                    cancel.clone(),
                )
                .await
                .unwrap();
        }
        assert_eq!(
            store.out_degree("src-1", cancel.clone()).await.unwrap(),
            DEFAULT_MAX_OUT_DEGREE
        );
        store
            .put(&MemoryLink::new("src-1", "overflow", MemoryLinkKind::References), cancel.clone())
            .await
            .expect("cap rejection must be fail-open, not an error");
        assert_eq!(
            store.out_degree("src-1", cancel.clone()).await.unwrap(),
            DEFAULT_MAX_OUT_DEGREE,
            "the reject edge must not be persisted"
        );
    }

    #[tokio::test]
    async fn put_respects_configured_cap_and_allows_idempotent_update_at_cap() {
        let store = test_store().await.with_max_out_degree(2);
        let cancel = CancellationToken::new();
        store
            .put(&MemoryLink::new("src-1", "a", MemoryLinkKind::References), cancel.clone())
            .await
            .unwrap();
        store
            .put(&MemoryLink::new("src-1", "b", MemoryLinkKind::References), cancel.clone())
            .await
            .unwrap();
        store
            .put(&MemoryLink::new("src-1", "c", MemoryLinkKind::References), cancel.clone())
            .await
            .unwrap();
        assert_eq!(
            store.out_degree("src-1", cancel.clone()).await.unwrap(),
            2,
            "cap of 2 must hold"
        );

        // Re-writing an existing triple is an idempotent update, allowed even
        // at the cap (weight update must land).
        let mut updated = MemoryLink::new("src-1", "a", MemoryLinkKind::References);
        updated.weight = 0.25;
        store.put(&updated, cancel.clone()).await.unwrap();
        let links = store.links_from("src-1", cancel.clone()).await.unwrap();
        assert_eq!(links.len(), 2);
        assert!(links.iter().any(|link| link.weight == 0.25));
    }

    #[tokio::test]
    async fn incoming_links_batches_timestamps_by_target() {
        let store = test_store().await;
        let cancel = CancellationToken::new();
        store
            .put(&MemoryLink::new("src-1", "tgt-a", MemoryLinkKind::Supports), cancel.clone())
            .await
            .unwrap();
        store
            .put(&MemoryLink::new("src-2", "tgt-a", MemoryLinkKind::References), cancel.clone())
            .await
            .unwrap();
        store
            .put(&MemoryLink::new("src-3", "tgt-b", MemoryLinkKind::Contradicts), cancel.clone())
            .await
            .unwrap();

        let by_target = store
            .incoming_links(&["tgt-a".into(), "tgt-b".into(), "unknown".into()], cancel.clone())
            .await
            .unwrap();
        assert_eq!(by_target.len(), 2, "absent ids produce no entry");
        assert_eq!(by_target["tgt-a"].len(), 2, "both incoming edges must be decoded");
        assert_eq!(by_target["tgt-b"].len(), 1);
        // Timestamps are future-stable decoded Unix seconds.
        let now = OffsetDateTime::now_utc().unix_timestamp();
        assert!(by_target["tgt-a"].iter().all(|timed| timed.created_unix <= now + 5));
        assert_eq!(by_target["tgt-b"][0].link.kind, MemoryLinkKind::Contradicts);
    }

    #[tokio::test]
    async fn incoming_links_skips_unknown_kinds_and_bad_timestamps() {
        let store = test_store().await;
        // Craft rows the normal write path never makes: an unknown link_type
        // and a corrupt created_at. They are skipped, never an error.
        sqlx::query(
            "INSERT INTO memory_links (source_id, target_id, link_type, weight, created_at) \
             VALUES ('src-x', 'tgt-a', 'future_kind', 1.0, ?)",
        )
        .bind(OffsetDateTime::now_utc().to_string())
        .execute(&store.pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO memory_links (source_id, target_id, link_type, weight, created_at) \
             VALUES ('src-y', 'tgt-a', 'references', 1.0, 'not-a-timestamp')",
        )
        .execute(&store.pool)
        .await
        .unwrap();

        let by_target =
            store.incoming_links(&["tgt-a".into()], CancellationToken::new()).await.unwrap();
        assert!(
            by_target.is_empty(),
            "both malformed rows are skipped so no target entry is created"
        );
    }

    #[tokio::test]
    async fn incoming_links_empty_and_cancelled_are_cheap_and_checked() {
        let store = test_store().await;
        assert!(store.incoming_links(&[], CancellationToken::new()).await.unwrap().is_empty());

        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(matches!(
            store.incoming_links(&["a".into()], cancel).await,
            Err(MemoryError::Cancelled)
        ));
    }
}
