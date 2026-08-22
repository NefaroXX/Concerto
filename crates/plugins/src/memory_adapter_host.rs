//! Plugin-backed vector store (memory adapter).
//!
//! Wraps an [`ActivePlugin`] that exports `call_adapter` and implements
//! [`concerto_core::VectorStore`] by delegating all
//! operations to the WASM plugin via JSON messages.
//!
//! # JSON protocol
//!
//! Each operation sends a JSON object and expects a JSON object in return.
//! Vector and record types are converted manually rather than relying on
//! serde derives (which are not present on some shared types).

use concerto_core::CancellationToken;
use std::sync::Arc;

use async_trait::async_trait;
use camino::Utf8PathBuf;
use concerto_core::error::MemoryError;
use concerto_core::memory::{EmbeddingRecord, ProjectId, VectorResult};
use concerto_core::VectorStore;
use tokio::sync::Mutex;

use crate::active_plugin::ActivePlugin;

/// A `VectorStore` backed by a WASM plugin.
pub struct PluginBackedVectorStore {
    plugin: Arc<Mutex<ActivePlugin>>,
}

impl PluginBackedVectorStore {
    /// Create a new plugin-backed vector store.
    pub fn new(plugin: Arc<Mutex<ActivePlugin>>) -> Self {
        Self { plugin }
    }

    /// Call an adapter operation and return the JSON result.
    async fn call_op(
        &self,
        op: &str,
        req: &serde_json::Value,
        cancel: CancellationToken,
    ) -> Result<serde_json::Value, MemoryError> {
        if cancel.is_cancelled() {
            return Err(MemoryError::Cancelled);
        }
        let mut plugin = self.plugin.lock().await;
        if cancel.is_cancelled() {
            return Err(MemoryError::Cancelled);
        }
        // Thread the caller's cancellation token into the plugin store so
        // in-flight async host calls (e.g. `concerto.completion` invoked from
        // within `call_adapter`) observe agent cancellation (ADR-38).
        plugin.set_cancel(Some(cancel.clone()));
        plugin
            .call_adapter(op, req)
            .await
            .map_err(|e| MemoryError::Persistence(format!("plugin adapter '{op}' failed: {e}")))
    }

    /// Serialize a single EmbeddingRecord to a JSON value (manual, no serde derive).
    fn record_to_json(rec: &EmbeddingRecord) -> serde_json::Value {
        serde_json::json!({
            "id": rec.id,
            "project_id": rec.project_id.0,
            "chunk_hash": rec.chunk_hash,
            "content": rec.content,
            "file_path": rec.file_path.as_str(),
            "start_line": rec.start_line,
            "end_line": rec.end_line,
            "chunk_type": format!("{:?}", rec.chunk_type),
            "vector": rec.vector,
            "model_id": rec.model_id,
            "model_version": rec.model_version,
            "stale": rec.stale,
            "created_at": rec.created_at.to_string(),
        })
    }

    /// Parse a VectorResult from a JSON value.
    fn result_from_json(val: &serde_json::Value) -> Option<VectorResult> {
        Some(VectorResult {
            chunk_id: val.get("chunk_id")?.as_str()?.to_string(),
            score: val.get("score")?.as_f64()?,
            content: val.get("content")?.as_str()?.to_string(),
        })
    }
}

#[async_trait]
impl VectorStore for PluginBackedVectorStore {
    async fn store(
        &self,
        records: &[EmbeddingRecord],
        cancel: CancellationToken,
    ) -> Result<(), MemoryError> {
        let records_json: Vec<serde_json::Value> =
            records.iter().map(Self::record_to_json).collect();
        let req = serde_json::json!({ "records": records_json });
        self.call_op("store", &req, cancel).await?;
        Ok(())
    }

    async fn search(
        &self,
        project_id: &ProjectId,
        query: &[f32],
        top_k: usize,
        cancel: CancellationToken,
    ) -> Result<Vec<VectorResult>, MemoryError> {
        let req = serde_json::json!({
            "project_id": project_id.0,
            "query": query,
            "top_k": top_k,
        });
        let resp = self.call_op("search", &req, cancel).await?;
        let results = resp
            .get("results")
            .and_then(|r| r.as_array())
            .map(|arr| arr.iter().filter_map(Self::result_from_json).collect())
            .unwrap_or_default();
        Ok(results)
    }

    async fn list(
        &self,
        project_id: &ProjectId,
        top_k: usize,
        cancel: CancellationToken,
    ) -> Result<Vec<VectorResult>, MemoryError> {
        let req = serde_json::json!({
            "project_id": project_id.0,
            "top_k": top_k,
        });
        let resp = self.call_op("list", &req, cancel).await?;
        let results = resp
            .get("results")
            .and_then(|r| r.as_array())
            .map(|arr| arr.iter().filter_map(Self::result_from_json).collect())
            .unwrap_or_default();
        Ok(results)
    }

    async fn tombstone(
        &self,
        chunk_id: &str,
        project_id: &ProjectId,
        cancel: CancellationToken,
    ) -> Result<(), MemoryError> {
        let req = serde_json::json!({
            "chunk_id": chunk_id,
            "project_id": project_id.0,
        });
        self.call_op("tombstone", &req, cancel).await?;
        Ok(())
    }

    async fn delete_tombstoned(
        &self,
        project_id: &ProjectId,
        cancel: CancellationToken,
    ) -> Result<(), MemoryError> {
        let req = serde_json::json!({ "project_id": project_id.0 });
        self.call_op("delete_tombstoned", &req, cancel).await?;
        Ok(())
    }

    async fn mark_stale(
        &self,
        project_id: &ProjectId,
        model_version: &str,
        cancel: CancellationToken,
    ) -> Result<(), MemoryError> {
        let req = serde_json::json!({
            "project_id": project_id.0,
            "model_version": model_version,
        });
        self.call_op("mark_stale", &req, cancel).await?;
        Ok(())
    }

    async fn delete_by_project(
        &self,
        project_id: &ProjectId,
        cancel: CancellationToken,
    ) -> Result<(), MemoryError> {
        let req = serde_json::json!({ "project_id": project_id.0 });
        self.call_op("delete_by_project", &req, cancel).await?;
        Ok(())
    }

    async fn delete_by_file_path(
        &self,
        project_id: &ProjectId,
        file_path: &Utf8PathBuf,
        cancel: CancellationToken,
    ) -> Result<Vec<String>, MemoryError> {
        let req = serde_json::json!({
            "project_id": project_id.0,
            "file_path": file_path.as_str(),
        });
        let resp = self.call_op("delete_by_file_path", &req, cancel).await?;
        let ids = resp
            .get("ids")
            .and_then(|r| r.as_array())
            .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
            .unwrap_or_default();
        Ok(ids)
    }
}

impl std::fmt::Debug for PluginBackedVectorStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginBackedVectorStore").finish()
    }
}
