//! SQLite-based vector store implementation.
//!
//! Provides [`SqliteVectorStore`], the primary `VectorStore` implementation.
//! The trait itself lives in [`concerto_core::traits::vector_store::VectorStore`].
//!
//! Every chunk written to the vector store must also be indexed in the
//! full-text store (see `sync.rs` — `ChunkSyncService`).

// Re-export the trait so downstream crates that use `crate::vector_store::VectorStore`
// (or `concerto_memory::vector_store::VectorStore`) still resolve.
pub use concerto_core::VectorStore;

use async_trait::async_trait;
use camino::Utf8PathBuf;
use concerto_core::CancellationToken;
use serde_json;
use sqlx::{Row, SqlitePool};

use concerto_core::error::MemoryError;
use concerto_core::memory::{
    ChunkType, EmbeddingRecord, MemoryChunk, MemoryNamespace, ProjectId, VectorResult,
};

/// SQLite based implementation of `VectorStore`.
pub struct SqliteVectorStore {
    pool: SqlitePool,
}

const CREATE_VECTOR_STORE: &str = r#"
CREATE TABLE IF NOT EXISTS vector_store (
    id TEXT NOT NULL,
    project_id TEXT NOT NULL,
    chunk_hash TEXT NOT NULL,
    content TEXT NOT NULL,
    file_path TEXT NOT NULL,
    start_line INTEGER,
    end_line INTEGER,
    chunk_type TEXT NOT NULL,
    vector BLOB NOT NULL,
    model_id TEXT NOT NULL,
    model_version TEXT NOT NULL,
    stale INTEGER NOT NULL,
    tombstone INTEGER NOT NULL,
    created_at TEXT NOT NULL,
    metadata TEXT,
    PRIMARY KEY (project_id, id)
)
"#;

/// Encodes a vector of `f32` values as raw little-endian bytes for BLOB storage.
fn encode_vector(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|f| f.to_le_bytes()).collect()
}

/// Decodes raw little-endian bytes back into a vector of `f32` values.
///
/// The caller must guarantee `bytes.len()` is a multiple of 4.
fn decode_vector(bytes: &[u8]) -> Vec<f32> {
    bytes.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect()
}

/// One stored projection row with its JSON metadata sidecar, as returned by
/// [`SqliteVectorStore::projections_by_path`] (ADR-60 D6 consolidation
/// discovery: find prior projections of a whiteboard group so they can be
/// superseded — invalidate-not-delete — with their provenance cited).
#[derive(Debug, Clone)]
pub struct ProjectionRow {
    /// The chunk id.
    pub chunk_id: String,
    /// The chunk content.
    pub content: String,
    /// Whether the row is tombstoned (invalidated but retained).
    pub tombstoned: bool,
    /// The JSON metadata sidecar, when one was stored.
    pub metadata: Option<serde_json::Value>,
}

impl SqliteVectorStore {
    /// Creates a new `SqliteVectorStore` and ensures the required table exists.
    pub async fn new(pool: SqlitePool) -> Result<Self, MemoryError> {
        sqlx::query(CREATE_VECTOR_STORE)
            .execute(&pool)
            .await
            .map_err(|e| MemoryError::Persistence(e.to_string()))?;
        ensure_column(&pool, "start_line", "INTEGER").await?;
        ensure_column(&pool, "end_line", "INTEGER").await?;
        // ADR-60 D6 consolidation projections: JSON provenance/bi-temporal
        // sidecar per chunk (source event ids, world vs ingestion time).
        // Nullable — plain index chunks carry none.
        ensure_column(&pool, "metadata", "TEXT").await?;
        // Must run before `ensure_composite_primary_key`: the recreate step below
        // rewrites the `vector` column as BLOB, so converting legacy JSON TEXT
        // first guarantees `ensure_composite_primary_key`'s plain INSERT...SELECT
        // copies already-binary rows into the BLOB column.
        ensure_binary_vector_column(&pool).await?;
        ensure_composite_primary_key(&pool).await?;
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_vector_store_project ON vector_store(project_id)",
        )
        .execute(&pool)
        .await
        .map_err(|error| MemoryError::Persistence(error.to_string()))?;
        Ok(Self { pool })
    }

    /// Store one projection chunk together with its JSON metadata sidecar in a
    /// single transaction (ADR-60 D6).
    ///
    /// The row upsert mirrors [`VectorStore::store`] for one record; the
    /// metadata column is written in the SAME transaction so a projection is
    /// never discoverable without its provenance. Idempotent by chunk id: a
    /// re-run of the same consolidation pass upserts the identical id instead
    /// of duplicating it.
    pub async fn store_projection(
        &self,
        record: &EmbeddingRecord,
        metadata: &serde_json::Value,
        _cancel: CancellationToken,
    ) -> Result<(), MemoryError> {
        let vector_bytes = encode_vector(&record.vector);
        let metadata_json = serde_json::to_string(metadata)
            .map_err(|error| MemoryError::Persistence(error.to_string()))?;
        let mut tx =
            self.pool.begin().await.map_err(|e| MemoryError::Persistence(e.to_string()))?;
        sqlx::query(
            r#"
            INSERT INTO vector_store (
                id, project_id, chunk_hash, content, file_path, start_line, end_line,
                chunk_type, vector, model_id, model_version, stale, tombstone, created_at,
                metadata
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 0, ?, ?)
            ON CONFLICT(project_id, id) DO UPDATE SET
                chunk_hash = excluded.chunk_hash,
                content = excluded.content,
                file_path = excluded.file_path,
                start_line = excluded.start_line,
                end_line = excluded.end_line,
                chunk_type = excluded.chunk_type,
                vector = excluded.vector,
                model_id = excluded.model_id,
                model_version = excluded.model_version,
                stale = excluded.stale,
                tombstone = excluded.tombstone,
                created_at = excluded.created_at,
                metadata = excluded.metadata
            "#,
        )
        .bind(&record.id)
        .bind(&record.project_id.0)
        .bind(&record.chunk_hash)
        .bind(&record.content)
        .bind(record.file_path.as_str())
        .bind(record.start_line.map(i64::from))
        .bind(record.end_line.map(i64::from))
        .bind(format!("{:?}", record.chunk_type))
        .bind(vector_bytes)
        .bind(&record.model_id)
        .bind(&record.model_version)
        .bind(if record.stale { 1i64 } else { 0i64 })
        .bind(record.created_at.to_string())
        .bind(&metadata_json)
        .execute(&mut *tx)
        .await
        .map_err(|e| MemoryError::Persistence(e.to_string()))?;
        tx.commit().await.map_err(|e| MemoryError::Persistence(e.to_string()))?;
        Ok(())
    }

    /// Every projection row stored under `file_path` for the project —
    /// tombstoned rows included, so a caller superseding an older projection
    /// can read its provenance before invalidating it (ADR-60 D6
    /// invalidate-not-delete with cited event ids).
    pub async fn projections_by_path(
        &self,
        project_id: &ProjectId,
        file_path: &str,
        _cancel: CancellationToken,
    ) -> Result<Vec<ProjectionRow>, MemoryError> {
        let rows = sqlx::query(
            "SELECT id, content, tombstone, metadata FROM vector_store \
             WHERE project_id = ? AND file_path = ? ORDER BY created_at ASC",
        )
        .bind(&project_id.0)
        .bind(file_path)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| MemoryError::RetrievalFailed(e.to_string()))?;
        rows.into_iter()
            .map(|row| {
                let metadata_json: Option<String> = row.get("metadata");
                let metadata = match metadata_json {
                    Some(json) => Some(serde_json::from_str(&json).map_err(|error| {
                        MemoryError::Persistence(format!(
                            "invalid projection metadata JSON on chunk {}: {error}",
                            row.get::<String, _>("id")
                        ))
                    })?),
                    None => None,
                };
                Ok(ProjectionRow {
                    chunk_id: row.get("id"),
                    content: row.get("content"),
                    tombstoned: row.get::<i64, _>("tombstone") != 0,
                    metadata,
                })
            })
            .collect()
    }
}

async fn ensure_column(
    pool: &SqlitePool,
    column_name: &str,
    column_type: &str,
) -> Result<(), MemoryError> {
    let columns = sqlx::query("PRAGMA table_info(vector_store)")
        .fetch_all(pool)
        .await
        .map_err(|error| MemoryError::Persistence(error.to_string()))?;
    if columns.iter().any(|row| row.get::<String, _>("name").as_str() == column_name) {
        return Ok(());
    }
    let statement = format!("ALTER TABLE vector_store ADD COLUMN {column_name} {column_type}");
    sqlx::query(&statement)
        .execute(pool)
        .await
        .map_err(|error| MemoryError::Persistence(error.to_string()))?;
    Ok(())
}

async fn ensure_composite_primary_key(pool: &SqlitePool) -> Result<(), MemoryError> {
    let columns = sqlx::query("PRAGMA table_info(vector_store)")
        .fetch_all(pool)
        .await
        .map_err(|error| MemoryError::Persistence(error.to_string()))?;
    let project_is_key = columns
        .iter()
        .any(|row| row.get::<String, _>("name") == "project_id" && row.get::<i64, _>("pk") > 0);
    if project_is_key {
        return Ok(());
    }

    let mut transaction =
        pool.begin().await.map_err(|error| MemoryError::Persistence(error.to_string()))?;
    sqlx::query("ALTER TABLE vector_store RENAME TO vector_store_legacy")
        .execute(&mut *transaction)
        .await
        .map_err(|error| MemoryError::Persistence(error.to_string()))?;
    sqlx::query(CREATE_VECTOR_STORE)
        .execute(&mut *transaction)
        .await
        .map_err(|error| MemoryError::Persistence(error.to_string()))?;
    sqlx::query(
        "INSERT OR REPLACE INTO vector_store (
            id, project_id, chunk_hash, content, file_path, start_line, end_line,
            chunk_type, vector, model_id, model_version, stale, tombstone, created_at
         )
         SELECT id, project_id, chunk_hash, content, file_path, start_line, end_line,
            chunk_type, vector, model_id, model_version, stale, tombstone, created_at
         FROM vector_store_legacy",
    )
    .execute(&mut *transaction)
    .await
    .map_err(|error| MemoryError::Persistence(error.to_string()))?;
    sqlx::query("DROP TABLE vector_store_legacy")
        .execute(&mut *transaction)
        .await
        .map_err(|error| MemoryError::Persistence(error.to_string()))?;
    transaction.commit().await.map_err(|error| MemoryError::Persistence(error.to_string()))?;
    Ok(())
}

/// Migrates a legacy `vector TEXT` column (JSON-encoded floats) to raw
/// little-endian `f32` BLOB storage.
///
/// The conversion cannot be expressed in SQL, so rows are copied Rust-side,
/// decoding each JSON vector and re-encoding it as bytes. Rows with unparseable
/// JSON are stored with an empty vector so `search`/`list` continue to exclude
/// them while their content is preserved for `get_chunks`. No-op when the
/// column is already `BLOB`.
async fn ensure_binary_vector_column(pool: &SqlitePool) -> Result<(), MemoryError> {
    let columns = sqlx::query("PRAGMA table_info(vector_store)")
        .fetch_all(pool)
        .await
        .map_err(|error| MemoryError::Persistence(error.to_string()))?;
    let vector_type = columns
        .iter()
        .find(|row| row.get::<String, _>("name") == "vector")
        .map(|row| row.get::<String, _>("type").to_uppercase());
    if vector_type.as_deref() == Some("BLOB") {
        return Ok(());
    }

    let mut transaction =
        pool.begin().await.map_err(|error| MemoryError::Persistence(error.to_string()))?;
    sqlx::query("ALTER TABLE vector_store RENAME TO vector_store_legacy")
        .execute(&mut *transaction)
        .await
        .map_err(|error| MemoryError::Persistence(error.to_string()))?;
    sqlx::query(CREATE_VECTOR_STORE)
        .execute(&mut *transaction)
        .await
        .map_err(|error| MemoryError::Persistence(error.to_string()))?;

    let rows = sqlx::query(
        "SELECT id, project_id, chunk_hash, content, file_path, start_line, end_line, \
         chunk_type, vector, model_id, model_version, stale, tombstone, created_at \
         FROM vector_store_legacy",
    )
    .fetch_all(&mut *transaction)
    .await
    .map_err(|error| MemoryError::Persistence(error.to_string()))?;

    for row in rows {
        let vector_json: String = row.get("vector");
        let vector_bytes = match serde_json::from_str::<Vec<f32>>(&vector_json) {
            Ok(v) => encode_vector(&v),
            // Unparseable JSON: preserve the row with an empty vector. The
            // empty BLOB keeps `search`/`list` from surfacing it (zero-norm /
            // empty-vector guards) while `get_chunks` retains the content.
            Err(_) => encode_vector(&[]),
        };
        sqlx::query(
            "INSERT OR REPLACE INTO vector_store (
                id, project_id, chunk_hash, content, file_path, start_line, end_line,
                chunk_type, vector, model_id, model_version, stale, tombstone, created_at
             )
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(row.get::<String, _>("id"))
        .bind(row.get::<String, _>("project_id"))
        .bind(row.get::<String, _>("chunk_hash"))
        .bind(row.get::<String, _>("content"))
        .bind(row.get::<String, _>("file_path"))
        .bind(row.get::<Option<i64>, _>("start_line"))
        .bind(row.get::<Option<i64>, _>("end_line"))
        .bind(row.get::<String, _>("chunk_type"))
        .bind(vector_bytes)
        .bind(row.get::<String, _>("model_id"))
        .bind(row.get::<String, _>("model_version"))
        .bind(row.get::<i64, _>("stale"))
        .bind(row.get::<i64, _>("tombstone"))
        .bind(row.get::<String, _>("created_at"))
        .execute(&mut *transaction)
        .await
        .map_err(|error| MemoryError::Persistence(error.to_string()))?;
    }

    sqlx::query("DROP TABLE vector_store_legacy")
        .execute(&mut *transaction)
        .await
        .map_err(|error| MemoryError::Persistence(error.to_string()))?;
    transaction.commit().await.map_err(|error| MemoryError::Persistence(error.to_string()))?;
    Ok(())
}

fn parse_chunk_type(value: &str) -> ChunkType {
    match value {
        "Function" => ChunkType::Function,
        "Struct" => ChunkType::Struct,
        "Trait" => ChunkType::Trait,
        "Impl" => ChunkType::Impl,
        "Enum" => ChunkType::Enum,
        "Module" => ChunkType::Module,
        "Test" => ChunkType::Test,
        "SessionSummary" => ChunkType::SessionSummary,
        "Fact" => ChunkType::Fact,
        _ => ChunkType::SlidingWindow,
    }
}

#[async_trait]
impl VectorStore for SqliteVectorStore {
    async fn store(
        &self,
        records: &[EmbeddingRecord],
        _cancel: CancellationToken,
    ) -> Result<(), MemoryError> {
        let mut tx =
            self.pool.begin().await.map_err(|e| MemoryError::Persistence(e.to_string()))?;
        for rec in records {
            let vector_bytes = encode_vector(&rec.vector);
            sqlx::query(
                r#"
                INSERT INTO vector_store (
                    id, project_id, chunk_hash, content, file_path, start_line, end_line, chunk_type, vector, model_id, model_version, stale, tombstone, created_at
                ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 0, ?)
                ON CONFLICT(project_id, id) DO UPDATE SET
                    chunk_hash = excluded.chunk_hash,
                    content = excluded.content,
                    file_path = excluded.file_path,
                    start_line = excluded.start_line,
                    end_line = excluded.end_line,
                    chunk_type = excluded.chunk_type,
                    vector = excluded.vector,
                    model_id = excluded.model_id,
                    model_version = excluded.model_version,
                    stale = excluded.stale,
                    tombstone = 0,
                    created_at = excluded.created_at
                "#
            )
            .bind(&rec.id)
            .bind(&rec.project_id.0)
            .bind(&rec.chunk_hash)
            .bind(&rec.content)
            .bind(rec.file_path.as_str())
            .bind(rec.start_line.map(i64::from))
            .bind(rec.end_line.map(i64::from))
            .bind(format!("{:?}", rec.chunk_type))
            .bind(vector_bytes)
            .bind(&rec.model_id)
            .bind(&rec.model_version)
            .bind(if rec.stale { 1i64 } else { 0i64 })
            .bind(rec.created_at.to_string())
            .execute(&mut *tx)
            .await
            .map_err(|e| MemoryError::Persistence(e.to_string()))?;
        }
        tx.commit().await.map_err(|e| MemoryError::Persistence(e.to_string()))?;
        Ok(())
    }

    async fn search(
        &self,
        project_id: &ProjectId,
        query: &[f32],
        top_k: usize,
        _cancel: CancellationToken,
    ) -> Result<Vec<VectorResult>, MemoryError> {
        let rows = sqlx::query(
            r#"
            SELECT id, content, vector FROM vector_store
            WHERE project_id = ? AND tombstone = 0 AND stale = 0
            "#,
        )
        .bind(&project_id.0)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| MemoryError::RetrievalFailed(e.to_string()))?;

        // Pre-compute query norm.
        let query_norm: f64 = query.iter().map(|v| (*v as f64).powi(2)).sum::<f64>().sqrt();
        if query_norm == 0.0 {
            return Ok(vec![]);
        }

        let mut results: Vec<VectorResult> = rows
            .into_iter()
            .filter_map(|row| {
                let id: String = row.get("id");
                let content: String = row.get("content");
                let vec_bytes: Vec<u8> = row.get("vector");
                if !vec_bytes.len().is_multiple_of(4) {
                    return None;
                }
                let stored_vec = decode_vector(&vec_bytes);
                let dot: f64 = query
                    .iter()
                    .zip(stored_vec.iter())
                    .map(|(a, b)| (*a as f64) * (*b as f64))
                    .sum();
                let stored_norm: f64 =
                    stored_vec.iter().map(|v| (*v as f64).powi(2)).sum::<f64>().sqrt();
                if stored_norm == 0.0 {
                    return None;
                }
                let score = dot / (query_norm * stored_norm);
                Some(VectorResult { chunk_id: id, score, content })
            })
            .collect();

        results.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
        results.truncate(top_k);
        Ok(results)
    }

    async fn list(
        &self,
        project_id: &ProjectId,
        top_k: usize,
        _cancel: CancellationToken,
    ) -> Result<Vec<VectorResult>, MemoryError> {
        let rows = sqlx::query(
            "SELECT id, content, vector FROM vector_store \
             WHERE project_id = ? AND tombstone = 0 AND stale = 0 \
             ORDER BY created_at DESC LIMIT ?",
        )
        .bind(&project_id.0)
        .bind(top_k as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| MemoryError::RetrievalFailed(e.to_string()))?;

        Ok(rows
            .into_iter()
            .filter_map(|row| {
                // ADR-39: exclude FTS-only sentinel rows (empty/zero-norm
                // vectors, never valid similarity rows) the same way `search`
                // does — mirror the list surface, not just the search surface.
                let vec_bytes: Vec<u8> = row.get("vector");
                if vec_bytes.is_empty() || !vec_bytes.len().is_multiple_of(4) {
                    return None;
                }
                Some(VectorResult {
                    chunk_id: row.get("id"),
                    score: 1.0,
                    content: row.get("content"),
                })
            })
            .collect())
    }

    async fn get_chunks(
        &self,
        project_id: &ProjectId,
        chunk_ids: &[String],
        _cancel: CancellationToken,
    ) -> Result<Vec<MemoryChunk>, MemoryError> {
        let mut chunks = Vec::with_capacity(chunk_ids.len());
        for chunk_id in chunk_ids {
            let row = sqlx::query(
                "SELECT id, content, file_path, start_line, end_line, chunk_type, \
                 model_id, model_version FROM vector_store \
                 WHERE id = ? AND project_id = ? AND tombstone = 0 AND stale = 0",
            )
            .bind(chunk_id)
            .bind(&project_id.0)
            .fetch_optional(&self.pool)
            .await
            .map_err(|error| MemoryError::RetrievalFailed(error.to_string()))?;
            if let Some(row) = row {
                let start_line = row
                    .get::<Option<i64>, _>("start_line")
                    .and_then(|line| u32::try_from(line).ok());
                let end_line =
                    row.get::<Option<i64>, _>("end_line").and_then(|line| u32::try_from(line).ok());
                chunks.push(MemoryChunk {
                    id: row.get("id"),
                    project_id: project_id.clone(),
                    namespace: MemoryNamespace::Project(project_id.clone()),
                    content: row.get("content"),
                    file_path: Some(Utf8PathBuf::from(row.get::<String, _>("file_path"))),
                    start_line,
                    end_line,
                    chunk_type: parse_chunk_type(&row.get::<String, _>("chunk_type")),
                    score: 0.0,
                    model_id: row.get("model_id"),
                    model_version: row.get("model_version"),
                });
            }
        }
        Ok(chunks)
    }

    fn supports_chunk_metadata(&self) -> bool {
        true
    }

    async fn tombstone(
        &self,
        chunk_id: &str,
        project_id: &ProjectId,
        _cancel: CancellationToken,
    ) -> Result<(), MemoryError> {
        sqlx::query("UPDATE vector_store SET tombstone = 1 WHERE id = ? AND project_id = ?")
            .bind(chunk_id)
            .bind(&project_id.0)
            .execute(&self.pool)
            .await
            .map_err(|e| MemoryError::Persistence(e.to_string()))?;
        Ok(())
    }

    async fn delete_tombstoned(
        &self,
        project_id: &ProjectId,
        _cancel: CancellationToken,
    ) -> Result<(), MemoryError> {
        sqlx::query("DELETE FROM vector_store WHERE project_id = ? AND tombstone = 1")
            .bind(&project_id.0)
            .execute(&self.pool)
            .await
            .map_err(|e| MemoryError::Persistence(e.to_string()))?;
        Ok(())
    }

    async fn mark_stale(
        &self,
        project_id: &ProjectId,
        model_version: &str,
        _cancel: CancellationToken,
    ) -> Result<(), MemoryError> {
        sqlx::query("UPDATE vector_store SET stale = 1 WHERE project_id = ? AND model_version = ?")
            .bind(&project_id.0)
            .bind(model_version)
            .execute(&self.pool)
            .await
            .map_err(|e| MemoryError::Persistence(e.to_string()))?;
        Ok(())
    }

    async fn delete_by_project(
        &self,
        project_id: &ProjectId,
        _cancel: CancellationToken,
    ) -> Result<(), MemoryError> {
        sqlx::query("DELETE FROM vector_store WHERE project_id = ?")
            .bind(&project_id.0)
            .execute(&self.pool)
            .await
            .map_err(|e| MemoryError::Persistence(e.to_string()))?;
        Ok(())
    }

    async fn delete_by_file_path(
        &self,
        project_id: &ProjectId,
        file_path: &Utf8PathBuf,
        _cancel: CancellationToken,
    ) -> Result<Vec<String>, MemoryError> {
        let ids = sqlx::query("SELECT id FROM vector_store WHERE project_id = ? AND file_path = ?")
            .bind(&project_id.0)
            .bind(file_path.as_str())
            .fetch_all(&self.pool)
            .await
            .map_err(|e| MemoryError::Persistence(e.to_string()))?
            .into_iter()
            .map(|row| row.get::<String, _>("id"))
            .collect();
        sqlx::query("DELETE FROM vector_store WHERE project_id = ? AND file_path = ?")
            .bind(&project_id.0)
            .bind(file_path.as_str())
            .execute(&self.pool)
            .await
            .map_err(|e| MemoryError::Persistence(e.to_string()))?;
        Ok(ids)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;
    use time::OffsetDateTime;

    async fn memory_pool() -> SqlitePool {
        SqlitePoolOptions::new().max_connections(1).connect("sqlite::memory:").await.unwrap()
    }

    fn record(project_id: ProjectId, id: &str, content: &str) -> EmbeddingRecord {
        EmbeddingRecord {
            id: id.into(),
            project_id,
            chunk_hash: blake3::hash(content.as_bytes()).to_string(),
            content: content.into(),
            file_path: "src/lib.rs".into(),
            start_line: Some(1),
            end_line: Some(1),
            chunk_type: ChunkType::Function,
            vector: vec![0.1, 0.2],
            model_id: "test".into(),
            model_version: "1".into(),
            stale: false,
            created_at: OffsetDateTime::now_utc(),
        }
    }

    #[tokio::test]
    async fn same_chunk_id_is_isolated_by_project() {
        let store = SqliteVectorStore::new(memory_pool().await).await.unwrap();
        let project_a = ProjectId("a".into());
        let project_b = ProjectId("b".into());
        store
            .store(&[record(project_a.clone(), "same", "content a")], CancellationToken::new())
            .await
            .unwrap();
        store
            .store(&[record(project_b.clone(), "same", "content b")], CancellationToken::new())
            .await
            .unwrap();

        let a =
            store.get_chunks(&project_a, &["same".into()], CancellationToken::new()).await.unwrap();
        let b =
            store.get_chunks(&project_b, &["same".into()], CancellationToken::new()).await.unwrap();
        assert_eq!(a[0].content, "content a");
        assert_eq!(b[0].content, "content b");
    }

    #[tokio::test]
    async fn migrates_legacy_global_primary_key() {
        let pool = memory_pool().await;
        sqlx::query(
            "CREATE TABLE vector_store (
                id TEXT PRIMARY KEY,
                project_id TEXT NOT NULL,
                chunk_hash TEXT NOT NULL,
                content TEXT NOT NULL,
                file_path TEXT NOT NULL,
                chunk_type TEXT NOT NULL,
                vector TEXT NOT NULL,
                model_id TEXT NOT NULL,
                model_version TEXT NOT NULL,
                stale INTEGER NOT NULL,
                tombstone INTEGER NOT NULL,
                created_at TEXT NOT NULL
            )",
        )
        .execute(&pool)
        .await
        .unwrap();

        let store = SqliteVectorStore::new(pool.clone()).await.unwrap();
        let project_a = ProjectId("a".into());
        let project_b = ProjectId("b".into());
        store
            .store(&[record(project_a.clone(), "same", "content a")], CancellationToken::new())
            .await
            .unwrap();
        store
            .store(&[record(project_b.clone(), "same", "content b")], CancellationToken::new())
            .await
            .unwrap();

        assert_eq!(
            store
                .get_chunks(&project_a, &["same".into()], CancellationToken::new())
                .await
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            store
                .get_chunks(&project_b, &["same".into()], CancellationToken::new())
                .await
                .unwrap()
                .len(),
            1
        );
        let columns =
            sqlx::query("PRAGMA table_info(vector_store)").fetch_all(&pool).await.unwrap();
        assert!(columns.iter().any(|row| {
            row.get::<String, _>("name") == "project_id" && row.get::<i64, _>("pk") > 0
        }));
    }

    /// Storing a record and retrieving it returns the correct content.
    #[tokio::test]
    async fn store_and_retrieve_single_record() {
        let store = SqliteVectorStore::new(memory_pool().await).await.unwrap();
        let project = ProjectId("single".into());
        let rec = record(project.clone(), "chunk1", "hello world");
        store.store(&[rec], CancellationToken::new()).await.unwrap();
        let chunks =
            store.get_chunks(&project, &["chunk1".into()], CancellationToken::new()).await.unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].content, "hello world");
    }

    /// Getting a non-existent chunk returns an empty vec.
    #[tokio::test]
    async fn get_nonexistent_chunk_returns_empty() {
        let store = SqliteVectorStore::new(memory_pool().await).await.unwrap();
        let project = ProjectId("missing".into());
        let chunks = store
            .get_chunks(&project, &["no-such-id".into()], CancellationToken::new())
            .await
            .unwrap();
        assert!(chunks.is_empty());
    }

    /// `encode_vector`/`decode_vector` round-trip exactly: no arithmetic is
    /// performed, so `f32` values survive byte-for-byte.
    #[test]
    fn encode_decode_round_trip() {
        let v = vec![0.2345, -1.5, std::f32::consts::PI, 0.0, -0.0001];
        let bytes = encode_vector(&v);
        assert_eq!(bytes.len(), v.len() * 4);
        assert_eq!(decode_vector(&bytes), v);
    }

    /// Vectors round-trip through raw BLOB storage: store, search, and list all
    /// observe the same floats.
    #[tokio::test]
    async fn vector_round_trips_through_blob_storage() {
        let store = SqliteVectorStore::new(memory_pool().await).await.unwrap();
        let project = ProjectId("blob".into());
        let mut rec = record(project.clone(), "chunk1", "round trip content");
        rec.vector = vec![0.2345, -1.5, std::f32::consts::PI, 0.0, -0.0001];
        store.store(&[rec], CancellationToken::new()).await.unwrap();

        let results = store
            .search(
                &project,
                &[0.2345, -1.5, std::f32::consts::PI, 0.0, -0.0001],
                5,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].chunk_id, "chunk1");
        assert_eq!(results[0].content, "round trip content");
        assert!(results[0].score > 0.999, "score was {}", results[0].score);

        let listed = store.list(&project, 5, CancellationToken::new()).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].chunk_id, "chunk1");
    }

    /// A legacy DB with a JSON TEXT vector column (no start_line/end_line, and
    /// a single-column PK) is migrated to BLOB storage by `new`.
    #[tokio::test]
    async fn migrates_legacy_json_vector_to_blob() {
        let pool = memory_pool().await;
        sqlx::query(
            "CREATE TABLE vector_store (
                id TEXT PRIMARY KEY,
                project_id TEXT NOT NULL,
                chunk_hash TEXT NOT NULL,
                content TEXT NOT NULL,
                file_path TEXT NOT NULL,
                chunk_type TEXT NOT NULL,
                vector TEXT NOT NULL,
                model_id TEXT NOT NULL,
                model_version TEXT NOT NULL,
                stale INTEGER NOT NULL,
                tombstone INTEGER NOT NULL,
                created_at TEXT NOT NULL
            )",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO vector_store (
                id, project_id, chunk_hash, content, file_path, chunk_type, vector,
                model_id, model_version, stale, tombstone, created_at
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 0, 0, ?)",
        )
        .bind("legacy1")
        .bind("legacy")
        .bind("hash")
        .bind("legacy content")
        .bind("src/legacy.rs")
        .bind("Function")
        .bind("[0.25, -0.5, 1.0]")
        .bind("model")
        .bind("1")
        .bind(time::OffsetDateTime::now_utc().to_string())
        .execute(&pool)
        .await
        .unwrap();

        let store = SqliteVectorStore::new(pool.clone()).await.unwrap();
        let project = ProjectId("legacy".into());
        let results =
            store.search(&project, &[0.25, -0.5, 1.0], 5, CancellationToken::new()).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].chunk_id, "legacy1");
        assert_eq!(results[0].content, "legacy content");
        assert!(results[0].score > 0.999, "score was {}", results[0].score);

        let columns =
            sqlx::query("PRAGMA table_info(vector_store)").fetch_all(&pool).await.unwrap();
        let vector_type = columns
            .iter()
            .find(|row| row.get::<String, _>("name") == "vector")
            .map(|row| row.get::<String, _>("type").to_uppercase());
        assert_eq!(vector_type.as_deref(), Some("BLOB"));
    }

    /// The production upgrade path: a DB matching the HEAD schema (composite
    /// `(project_id, id)` PK, `start_line`/`end_line` present, TEXT vector)
    /// migrates to BLOB, with `ensure_composite_primary_key` no-oping and an
    /// FTS-only sentinel row (ADR-39) excluded from search/list but preserved
    /// for `get_chunks`.
    #[tokio::test]
    async fn migrates_legacy_json_vector_to_blob_composite_pk() {
        let pool = memory_pool().await;
        sqlx::query(
            "CREATE TABLE vector_store (
                id TEXT NOT NULL,
                project_id TEXT NOT NULL,
                chunk_hash TEXT NOT NULL,
                content TEXT NOT NULL,
                file_path TEXT NOT NULL,
                start_line INTEGER,
                end_line INTEGER,
                chunk_type TEXT NOT NULL,
                vector TEXT NOT NULL,
                model_id TEXT NOT NULL,
                model_version TEXT NOT NULL,
                stale INTEGER NOT NULL,
                tombstone INTEGER NOT NULL,
                created_at TEXT NOT NULL,
                PRIMARY KEY (project_id, id)
            )",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO vector_store (
                id, project_id, chunk_hash, content, file_path, start_line, end_line,
                chunk_type, vector, model_id, model_version, stale, tombstone, created_at
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 0, 0, ?)",
        )
        .bind("normal1")
        .bind("upgrade")
        .bind("hash")
        .bind("upgrade content")
        .bind("src/upgrade.rs")
        .bind(1i64)
        .bind(10i64)
        .bind("Function")
        .bind("[0.25, -0.5, 1.0]")
        .bind("model")
        .bind("1")
        .bind(time::OffsetDateTime::now_utc().to_string())
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO vector_store (
                id, project_id, chunk_hash, content, file_path, start_line, end_line,
                chunk_type, vector, model_id, model_version, stale, tombstone, created_at
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 0, 0, ?)",
        )
        .bind("sentinel1")
        .bind("upgrade")
        .bind("hash2")
        .bind("fts only content")
        .bind("src/fts.rs")
        .bind(2i64)
        .bind(2i64)
        .bind("SessionSummary")
        .bind("[]")
        .bind("model")
        .bind("1")
        .bind(time::OffsetDateTime::now_utc().to_string())
        .execute(&pool)
        .await
        .unwrap();

        let store = SqliteVectorStore::new(pool.clone()).await.unwrap();
        let project = ProjectId("upgrade".into());

        let results =
            store.search(&project, &[0.25, -0.5, 1.0], 5, CancellationToken::new()).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].chunk_id, "normal1");
        assert_eq!(results[0].content, "upgrade content");
        assert!(results[0].score > 0.999, "score was {}", results[0].score);

        let listed = store.list(&project, 5, CancellationToken::new()).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].chunk_id, "normal1");

        let sentinel_chunks = store
            .get_chunks(&project, &["sentinel1".into()], CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(sentinel_chunks.len(), 1);
        assert_eq!(sentinel_chunks[0].content, "fts only content");

        let columns =
            sqlx::query("PRAGMA table_info(vector_store)").fetch_all(&pool).await.unwrap();
        let vector_type = columns
            .iter()
            .find(|row| row.get::<String, _>("name") == "vector")
            .map(|row| row.get::<String, _>("type").to_uppercase());
        assert_eq!(vector_type.as_deref(), Some("BLOB"));
    }

    /// ADR-60 D6: `store_projection` writes the chunk row and its JSON
    /// metadata sidecar atomically; `projections_by_path` reads both back —
    /// including tombstoned rows, so a superseding pass can cite provenance.
    #[tokio::test]
    async fn store_projection_round_trips_row_and_metadata() {
        let store = SqliteVectorStore::new(memory_pool().await).await.unwrap();
        let project = ProjectId("projection".into());
        let mut rec = record(project.clone(), "proj-1", "consolidated summary");
        rec.file_path = "whiteboard/plan-1".into();
        let metadata = serde_json::json!({
            "kind": "adr60-d6-consolidation",
            "source_event_ids": ["e1", "e2"],
        });
        store.store_projection(&rec, &metadata, CancellationToken::new()).await.unwrap();

        let rows = store
            .projections_by_path(&project, "whiteboard/plan-1", CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].chunk_id, "proj-1");
        assert_eq!(rows[0].content, "consolidated summary");
        assert!(!rows[0].tombstoned);
        assert_eq!(rows[0].metadata.as_ref(), Some(&metadata));

        // Idempotent re-store of the same id upserts instead of duplicating.
        store.store_projection(&rec, &metadata, CancellationToken::new()).await.unwrap();
        let rows = store
            .projections_by_path(&project, "whiteboard/plan-1", CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);

        // A different path is invisible to this query.
        let other = store
            .projections_by_path(&project, "whiteboard/plan-2", CancellationToken::new())
            .await
            .unwrap();
        assert!(other.is_empty());
    }

    /// A legacy DB whose `vector_store` table predates the `metadata` column
    /// gains it via `new` (the same additive-migration path as start/end_line).
    #[tokio::test]
    async fn legacy_table_gains_the_metadata_column() {
        let pool = memory_pool().await;
        sqlx::query(
            "CREATE TABLE vector_store (
                id TEXT NOT NULL,
                project_id TEXT NOT NULL,
                chunk_hash TEXT NOT NULL,
                content TEXT NOT NULL,
                file_path TEXT NOT NULL,
                start_line INTEGER,
                end_line INTEGER,
                chunk_type TEXT NOT NULL,
                vector BLOB NOT NULL,
                model_id TEXT NOT NULL,
                model_version TEXT NOT NULL,
                stale INTEGER NOT NULL,
                tombstone INTEGER NOT NULL,
                created_at TEXT NOT NULL,
                PRIMARY KEY (project_id, id)
            )",
        )
        .execute(&pool)
        .await
        .unwrap();

        let store = SqliteVectorStore::new(pool.clone()).await.unwrap();
        let columns =
            sqlx::query("PRAGMA table_info(vector_store)").fetch_all(&pool).await.unwrap();
        assert!(
            columns.iter().any(|row| row.get::<String, _>("name") == "metadata"),
            "metadata column added"
        );
        // The migrated table still serves plain stores and projection reads.
        let project = ProjectId("legacy-meta".into());
        store
            .store(&[record(project.clone(), "c1", "content")], CancellationToken::new())
            .await
            .unwrap();
        let rows = store
            .projections_by_path(&project, "src/lib.rs", CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].metadata, None);
    }
}
