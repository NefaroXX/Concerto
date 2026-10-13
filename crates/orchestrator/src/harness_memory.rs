//! H13 — `memory.recall` / `memory.inspect` operations over the one harness
//! service seam.
//!
//! `memory.recall` is the evidence-bearing recall (`MemoryStore::retrieve_with_evidence`)
//! surfaced as an operation, so every adapter sees the same source/status/why
//! attribution the single-agent prompt already carries (H11). `memory.inspect`
//! reads one chunk's links and evidence score through the memory crate's
//! read-only loader.
//!
//! Both are ordinary registered tools invoked through
//! [`ToolExecutor::execute`][concerto_core::executor::ToolExecutor::execute];
//! the store handle lives inside the tool, never in an adapter.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use concerto_core::memory::{MemoryNamespace, MemoryQuery, ProjectId};
use concerto_core::traits::memory::MemoryStore;
use concerto_core::traits::tool::Tool;
use concerto_core::traits::PolicyEngine;
use concerto_core::types::{CapabilitySet, SessionContext, ToolOutput};
use concerto_core::{CancellationToken, ToolError};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Registry name of the recall operation.
pub const MEMORY_RECALL_TOOL_NAME: &str = "memory.recall";
/// Registry name of the inspect operation.
pub const MEMORY_INSPECT_TOOL_NAME: &str = "memory.inspect";

/// Default recall bound when the caller omits `top_k`.
pub const MEMORY_RECALL_DEFAULT_TOP_K: u32 = 5;
/// Hard recall ceiling (the disclosure window's outer bound): a caller can ask
/// for more than the default, never for an unbounded dump.
pub const MEMORY_RECALL_MAX_TOP_K: u32 = 50;

/// Input for `memory.recall`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MemoryRecallInput {
    /// Free-text query.
    pub query: String,
    /// Maximum results; clamped to [`MEMORY_RECALL_MAX_TOP_K`].
    #[serde(default)]
    pub top_k: Option<u32>,
}

/// Input for `memory.inspect`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MemoryInspectInput {
    /// The chunk id to inspect.
    pub chunk_id: String,
}

/// Resolves a project's memory store lazily.
///
/// The runtime registers `memory.recall` into the shared registry *before* the
/// project memory store is initialised (the registry is frozen into the
/// executor first), so the tool resolves the store from the run's shared
/// handle at call time. A resolver that finds no store (memory disabled or
/// init failed) yields an honest empty recall, never a fake one.
pub trait MemoryStoreResolver: Send + Sync {
    /// The current project's store, or `None` when memory is unavailable.
    fn resolve(&self) -> Option<Arc<dyn MemoryStore>>;
}

/// Where a `MemoryRecallTool` gets its store.
enum RecallSource {
    /// A fixed store (CLI/desktop standalone service, tests).
    Fixed(Arc<dyn MemoryStore>),
    /// The runtime's shared handle, resolved per call.
    Lazy(Arc<dyn MemoryStoreResolver>),
}

impl RecallSource {
    fn resolve(&self) -> Option<Arc<dyn MemoryStore>> {
        match self {
            Self::Fixed(store) => Some(store.clone()),
            Self::Lazy(resolver) => resolver.resolve(),
        }
    }
}

/// The `memory.recall` operation.
pub struct MemoryRecallTool {
    source: RecallSource,
    project_id: ProjectId,
    /// Default `top_k` when the caller omits one (from `[harness].recall_limit`).
    default_top_k: u32,
}

impl MemoryRecallTool {
    /// Attach to a memory store scoped to `project_id`.
    pub fn new(store: Arc<dyn MemoryStore>, project_id: ProjectId) -> Self {
        Self {
            source: RecallSource::Fixed(store),
            project_id,
            default_top_k: MEMORY_RECALL_DEFAULT_TOP_K,
        }
    }

    /// Attach to a lazily-resolved store (the runtime's shared handle).
    pub fn lazy(resolver: Arc<dyn MemoryStoreResolver>, project_id: ProjectId) -> Self {
        Self {
            source: RecallSource::Lazy(resolver),
            project_id,
            default_top_k: MEMORY_RECALL_DEFAULT_TOP_K,
        }
    }

    /// Override the default `top_k` (clamped to the operation ceiling).
    #[must_use]
    pub fn with_default_top_k(mut self, default_top_k: u32) -> Self {
        self.default_top_k = default_top_k.clamp(1, MEMORY_RECALL_MAX_TOP_K);
        self
    }
}

#[async_trait]
impl Tool for MemoryRecallTool {
    fn name(&self) -> &str {
        MEMORY_RECALL_TOOL_NAME
    }

    fn description(&self) -> &str {
        "Recall project memory as evidence-bearing chunks: each result carries its source \
         attribution, truth status, the derivation (why), model freshness, and the degraded/\
         interrupted flags. Rank order is relevance only, never freshness. Read-only."
    }

    fn input_schema(&self) -> serde_json::Value {
        let root = schemars::schema_for!(MemoryRecallInput);
        serde_json::to_value(&root).unwrap_or_else(|error| {
            tracing::error!(%error, "failed to serialize MemoryRecallInput schema");
            serde_json::json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string" },
                    "top_k": { "type": ["integer", "null"] }
                },
                "required": ["query"]
            })
        })
    }

    fn capability_requirements(&self) -> CapabilitySet {
        CapabilitySet::default()
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        _policy: &dyn PolicyEngine,
        _session: &SessionContext,
        cancel: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let parsed: MemoryRecallInput = serde_json::from_value(input).map_err(|error| {
            ToolError::ExecutionFailed { message: format!("invalid memory.recall input: {error}") }
        })?;
        let top_k =
            parsed.top_k.unwrap_or(self.default_top_k).clamp(1, MEMORY_RECALL_MAX_TOP_K) as usize;
        let Some(store) = self.source.resolve() else {
            // Memory disabled / not yet initialised: an honest empty recall
            // with the reason stated, never a synthesized result.
            return Ok(ToolOutput {
                summary: "memory.recall unavailable: no project memory store".to_string(),
                data: serde_json::json!({
                    "chunks": [],
                    "total": 0,
                    "degraded": false,
                    "interrupted": false,
                    "unavailable": true,
                }),
            });
        };
        let query = MemoryQuery {
            text: parsed.query,
            project_id: self.project_id.clone(),
            namespace: MemoryNamespace::Project(self.project_id.clone()),
            top_k,
            filters: Vec::new(),
        };
        let chunks = store.retrieve_with_evidence(&query, cancel).await.map_err(|error| {
            ToolError::ExecutionFailed { message: format!("memory.recall failed: {error}") }
        })?;
        let degraded = chunks.iter().any(|chunk| chunk.degraded);
        let interrupted = chunks.iter().any(|chunk| chunk.interrupted);
        let total = chunks.len();
        let data = serde_json::json!({
            "chunks": chunks,
            "total": total,
            "degraded": degraded,
            "interrupted": interrupted,
        });
        Ok(ToolOutput {
            summary: format!(
                "memory.recall chunks={total}{}{}",
                if degraded { " degraded" } else { "" },
                if interrupted { " interrupted" } else { "" },
            ),
            data,
        })
    }
}

/// The `memory.inspect` operation: read one chunk's links and score.
pub struct MemoryInspectTool {
    db_path: PathBuf,
    project_id: ProjectId,
}

impl MemoryInspectTool {
    /// Attach to a memory database scoped to `project_id`.
    pub fn new(db_path: PathBuf, project_id: ProjectId) -> Self {
        Self { db_path, project_id }
    }
}

#[async_trait]
impl Tool for MemoryInspectTool {
    fn name(&self) -> &str {
        MEMORY_INSPECT_TOOL_NAME
    }

    fn description(&self) -> &str {
        "Inspect one memory chunk by id: content, outgoing/incoming links, and evidence score. \
         Read-only. Returns `found:false` for an unknown id or an absent database rather than \
         synthesizing a chunk."
    }

    fn input_schema(&self) -> serde_json::Value {
        let root = schemars::schema_for!(MemoryInspectInput);
        serde_json::to_value(&root).unwrap_or_else(|error| {
            tracing::error!(%error, "failed to serialize MemoryInspectInput schema");
            serde_json::json!({
                "type": "object",
                "properties": { "chunk_id": { "type": "string" } },
                "required": ["chunk_id"]
            })
        })
    }

    fn capability_requirements(&self) -> CapabilitySet {
        CapabilitySet::default()
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        _policy: &dyn PolicyEngine,
        _session: &SessionContext,
        cancel: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        if cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        let parsed: MemoryInspectInput = serde_json::from_value(input).map_err(|error| {
            ToolError::ExecutionFailed { message: format!("invalid memory.inspect input: {error}") }
        })?;
        let detail = concerto_memory::mermaid::load_chunk_detail(
            &self.db_path,
            &self.project_id,
            &parsed.chunk_id,
            cancel,
        )
        .await
        .map_err(|error| ToolError::ExecutionFailed {
            message: format!("memory.inspect failed: {error}"),
        })?;
        match detail {
            Some(detail) => Ok(ToolOutput {
                summary: format!(
                    "memory.inspect id={} links_out={} links_in={}",
                    detail.chunk_id,
                    detail.out_links.len(),
                    detail.in_links.len(),
                ),
                data: serde_json::json!({
                    "found": true,
                    "chunk": {
                        "chunk_id": detail.chunk_id,
                        "content": detail.content,
                        "score": detail.score,
                        "out_links": detail.out_links,
                        "in_links": detail.in_links,
                    }
                }),
            }),
            None => Ok(ToolOutput {
                summary: format!("memory.inspect id={} not found", parsed.chunk_id),
                data: serde_json::json!({ "found": false, "chunk_id": parsed.chunk_id }),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recall_default_and_ceiling_are_applied() {
        assert_eq!(
            MemoryRecallInput { query: "x".into(), top_k: None }
                .top_k
                .unwrap_or(MEMORY_RECALL_DEFAULT_TOP_K)
                .clamp(1, MEMORY_RECALL_MAX_TOP_K),
            MEMORY_RECALL_DEFAULT_TOP_K
        );
        assert_eq!(1_000u32.clamp(1, MEMORY_RECALL_MAX_TOP_K), MEMORY_RECALL_MAX_TOP_K);
    }

    #[test]
    fn recall_input_schema_requires_query() {
        let store: Arc<dyn MemoryStore> = Arc::new(concerto_core::traits::memory::NullMemoryStore);
        let tool = MemoryRecallTool::new(store, ProjectId("p".into()));
        let schema = tool.input_schema();
        assert_eq!(schema["required"][0], "query");
    }

    #[test]
    fn inspect_input_schema_requires_chunk_id() {
        let tool = MemoryInspectTool::new(PathBuf::from("/nonexistent.db"), ProjectId("p".into()));
        let schema = tool.input_schema();
        assert_eq!(schema["required"][0], "chunk_id");
    }
}
