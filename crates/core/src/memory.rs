//! Phase 4 memory types — the shared shape of everything the memory
//! subsystem produces and consumes.
//!
//! These types are used by the indexer, vector store, RAG pipeline,
//! summarizer, and orchestrator. Every crate in the workspace that
//! touches memory imports from here rather than defining its own
//! ad-hoc variants.

use crate::ids::Ulid;
// Re-use the canonical ProjectId from types — do not redefine it here.
pub use crate::types::ProjectId;
use camino::Utf8PathBuf;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

// ---------------------------------------------------------------------------
// MemoryId
// ---------------------------------------------------------------------------

/// Uniquely identifies a memory entry.
///
/// Backed by a ULID so entries are time-sortable without a separate
/// timestamp index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MemoryId(pub Ulid);

impl std::fmt::Display for MemoryId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

// ---------------------------------------------------------------------------
// MemoryNamespace
// ---------------------------------------------------------------------------

/// Memory namespaces for project-scoped and global storage.
///
/// - `Project(ProjectId)` — per-project facts, embeddings, entities.
/// - `Global { user_id_hash }` — user-level preferences and facts that
///   apply across all projects. The hash is `blake3(username + hostname)`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum MemoryNamespace {
    /// Project-scoped memory.
    Project(ProjectId),
    /// Global, user-scoped memory.
    Global { user_id_hash: String },
}

// ---------------------------------------------------------------------------
// ChunkType
// ---------------------------------------------------------------------------

/// The kind of content a chunk represents.
///
/// The current indexer creates simple language-aware line chunks for recognized
/// files and uses `SlidingWindow` for unknown extensions. Semantic variants are
/// retained for stored/generated entries and future chunkers. `SessionSummary`
/// and `Fact` are produced by the summarizer and entity extractor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum ChunkType {
    Function,
    Struct,
    Trait,
    Impl,
    Enum,
    Module,
    Test,
    /// Non-code or unknown file extension — sliding-window chunked.
    SlidingWindow,
    /// LLM-produced session summary.
    SessionSummary,
    /// Extracted architectural fact.
    Fact,
}

// ---------------------------------------------------------------------------
// MemoryFilter
// ---------------------------------------------------------------------------

/// Filter applied to a memory query before ranking.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum MemoryFilter {
    /// Restrict to a single chunk type.
    ChunkType(ChunkType),
    /// Glob pattern for file paths (e.g. `src/**/*.rs`).
    FileGlob(String),
    /// Minimum hybrid score threshold.
    MinScore(f64),
    /// Exclude entries whose `expires_at` is in the past.
    ExcludeStale,
}

// ---------------------------------------------------------------------------
// MemoryQuery
// ---------------------------------------------------------------------------

/// A query against the memory store.
///
/// Carries the raw text, project scope, namespace, and optional filters.
/// The vector store and FTS both consume this shape.
#[derive(Debug, Clone)]
pub struct MemoryQuery {
    pub text: String,
    pub project_id: ProjectId,
    pub namespace: MemoryNamespace,
    pub top_k: usize,
    pub filters: Vec<MemoryFilter>,
}

// ---------------------------------------------------------------------------
// MemoryChunk
// ---------------------------------------------------------------------------

/// A single chunk returned by the hybrid retriever.
///
/// The `score` is the combined hybrid score (vector similarity + BM25).
/// `model_id` and `model_version` identify which embedding model produced
/// the vector so the staleness checker can detect drift.
#[derive(Debug, Clone)]
pub struct MemoryChunk {
    pub id: String,
    pub project_id: ProjectId,
    pub namespace: MemoryNamespace,
    pub content: String,
    pub file_path: Option<Utf8PathBuf>,
    pub start_line: Option<u32>,
    pub end_line: Option<u32>,
    pub chunk_type: ChunkType,
    pub score: f64,
    pub model_id: String,
    pub model_version: String,
}

// ---------------------------------------------------------------------------
// MemoryEntry
// ---------------------------------------------------------------------------

/// A persisted memory entry in any store layer.
///
/// This is the write-side counterpart of `MemoryChunk` (which is the
/// read-side retrieval result). Entries carry full metadata including
/// the embedding model version so the staleness manager can detect
/// model drift and trigger re-indexing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryEntry {
    pub id: MemoryId,
    pub project_id: ProjectId,
    pub namespace: MemoryNamespace,
    pub content: String,
    pub chunk_type: ChunkType,
    pub model_id: Option<String>,
    pub model_version: Option<String>,
    pub metadata: serde_json::Value,
    pub expires_at: Option<OffsetDateTime>,
    pub created_at: OffsetDateTime,
}

// ---------------------------------------------------------------------------
// EmbeddingRecord
// ---------------------------------------------------------------------------

/// A stored embedding vector with its metadata.
///
/// Written by the indexer, consumed by the vector store. The `stale` flag
/// is set by `EmbeddingVersionChecker` when the embedding model version
/// changes.
#[derive(Debug, Clone)]
pub struct EmbeddingRecord {
    pub id: String,
    pub project_id: ProjectId,
    pub chunk_hash: String,
    pub content: String,
    pub file_path: Utf8PathBuf,
    pub start_line: Option<u32>,
    pub end_line: Option<u32>,
    pub chunk_type: ChunkType,
    pub vector: Vec<f32>,
    pub model_id: String,
    pub model_version: String,
    pub stale: bool,
    pub created_at: OffsetDateTime,
}

// ---------------------------------------------------------------------------
// Decision types
// ---------------------------------------------------------------------------

/// Uniquely identifies a decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DecisionId(pub Ulid);

impl std::fmt::Display for DecisionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Category of a decision for filtering and analysis.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum DecisionCategory {
    Architecture,
    Implementation,
    Test,
    Tooling,
    Other,
}

/// A recorded decision with confidence and supersession tracking.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Decision {
    pub id: DecisionId,
    pub session_id: Ulid,
    pub task_id: Option<crate::types::TaskId>,
    pub what: String,
    pub why: String,
    pub outcome: Option<String>,
    pub category: DecisionCategory,
    pub confidence: f32,
    pub superseded_by: Option<DecisionId>,
    pub created_at: OffsetDateTime,
}

// ---------------------------------------------------------------------------
// TaskNode — task decomposition tree
// ---------------------------------------------------------------------------

/// Uniquely identifies a task node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TaskNodeId(pub Ulid);

impl std::fmt::Display for TaskNodeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Status of a task node in the decomposition tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum TaskStatus {
    Pending,
    Running,
    Done,
    Failed,
    Blocked,
}

impl TaskStatus {
    /// Return the SQL-storable string representation.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Blocked => "blocked",
        }
    }

    /// Parse from a string (as stored in SQLite).
    pub fn parse_status(s: &str) -> Self {
        match s {
            "running" => Self::Running,
            "done" => Self::Done,
            "failed" => Self::Failed,
            "blocked" => Self::Blocked,
            _ => Self::Pending,
        }
    }
}

/// A node in the task decomposition tree.
///
/// Tasks form a DAG where each node can have children and blocking
/// dependencies. The tree is stored in SQLite and rendered as XML
/// for injection into the working memory prompt block.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskNode {
    pub id: TaskNodeId,
    pub session_id: Ulid,
    pub description: String,
    pub status: TaskStatus,
    pub parent_id: Option<TaskNodeId>,
    pub children: Vec<TaskNodeId>,
    pub blocking: Vec<TaskNodeId>,
    pub created_at: OffsetDateTime,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkingMemorySnapshot {
    pub id: Ulid,
    pub session_id: Ulid,
    pub decisions: Vec<Decision>,
    pub task_tree: Vec<TaskNode>,
    pub created_at: OffsetDateTime,
}

// ---------------------------------------------------------------------------
// FtsResult
// ---------------------------------------------------------------------------

/// A single result from a full-text search.
#[derive(Debug, Clone)]
pub struct FtsResult {
    pub chunk_id: String,
    pub score: f64,
    pub content: String,
}

// ---------------------------------------------------------------------------
// VectorResult
// ---------------------------------------------------------------------------

/// A single result from a vector similarity search.
#[derive(Debug, Clone)]
pub struct VectorResult {
    pub chunk_id: String,
    pub score: f64,
    pub content: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fts_result_fields() {
        let r =
            FtsResult { chunk_id: "chunk1".into(), score: 0.95, content: "matched text".into() };
        assert_eq!(r.chunk_id, "chunk1");
        assert_eq!(r.score, 0.95);
        assert_eq!(r.content, "matched text");
    }

    #[test]
    fn vector_result_fields() {
        let r =
            VectorResult { chunk_id: "vec1".into(), score: 0.87, content: "vector content".into() };
        assert_eq!(r.chunk_id, "vec1");
        assert_eq!(r.score, 0.87);
        assert_eq!(r.content, "vector content");
    }

    #[test]
    fn fts_result_score_non_negative() {
        // BM25 scores from FTS5 (negated rank) are positive and unbounded.
        for score in [0.0, 0.5, 1.0, 3.2] {
            let r = FtsResult { chunk_id: "c".into(), score, content: String::new() };
            assert!(r.score >= 0.0, "FTS score must be non-negative, got {score}");
        }
    }
}
