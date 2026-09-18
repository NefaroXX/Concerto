//! PersonaMem long-session user-fact recall benchmarks.
//!
//! Loads `recall.json` task fixtures and runs recall queries against any
//! [`concerto_core::traits::MemoryStore`]. Each query records where the chunk
//! containing the expected fact ranked (1-based), and a task passes only when
//! at least `min_recall_rate` of its queries satisfy their per-query
//! `min_rank`.
//!
//! This is the regression suite for the L1 typed extraction + dedup work
//! (ADR-46 symbolic offload): after a long session stores many facts and
//! distractors, stable user facts must still surface near the top of hybrid
//! retrieval — no ranking drift, no silent dedup merges burying the seed
//! corpus.
//!
//! The module deliberately depends on `concerto-core` types ONLY (not
//! `concerto-memory`), so runner binaries can stay decoupled from the store
//! implementation.

use std::path::Path;

use concerto_core::memory::{MemoryNamespace, MemoryQuery, ProjectId};
use concerto_core::traits::MemoryStore;
use concerto_core::CancellationToken;
use serde::Deserialize;

use crate::EvalError;

/// File name of the recall task fixture inside a task directory.
pub const RECALL_TASK_FILE: &str = "recall.json";

/// A single recall query: search for `query` and require the fact chunk
/// containing `expected` to rank no worse than `min_rank` (1-based).
#[derive(Debug, Clone, Deserialize)]
pub struct RecallQuery {
    pub query: String,
    pub expected: String,
    /// Best allowed (1-based) rank for the chunk containing `expected`.
    pub min_rank: usize,
}

/// A recall benchmark task: a seed corpus, distractors, and ranked queries.
#[derive(Debug, Clone, Deserialize)]
pub struct RecallTask {
    pub id: String,
    pub description: String,
    /// Facts stored first — "the long session corpus" the PersonaMem system
    /// accumulates.
    pub seed_facts: Vec<String>,
    /// Distractor facts stored after the seeds. They must not shadow the
    /// seeds' recall.
    pub distractors: Vec<String>,
    pub queries: Vec<RecallQuery>,
    /// Fraction of queries that must satisfy their `min_rank` for the task
    /// to pass.
    #[serde(default = "default_recall_rate")]
    pub min_recall_rate: f64,
}

fn default_recall_rate() -> f64 {
    1.0
}

/// The outcome of one recall query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecallOutcome {
    pub query: String,
    pub expected: String,
    pub min_rank: usize,
    /// 1-based rank of the first chunk whose content contains `expected`, or
    /// `None` when retrieval failed or the chunk was not returned.
    pub hit_rank: Option<usize>,
}

impl RecallOutcome {
    /// The query satisfied its bound: the expected chunk was returned and
    /// ranked no worse than `min_rank`.
    pub fn passed(&self) -> bool {
        self.hit_rank.is_some_and(|rank| rank <= self.min_rank)
    }
}

/// Load a recall task fixture from `task_dir/recall.json`.
pub fn load_recall_task(task_dir: &Path) -> Result<RecallTask, EvalError> {
    let path = task_dir.join(RECALL_TASK_FILE);
    let raw = std::fs::read_to_string(&path).map_err(|error| {
        EvalError::HarnessSetupFailed(format!(
            "failed to read recall task {}: {error}",
            path.display()
        ))
    })?;
    serde_json::from_str(&raw).map_err(|error| {
        EvalError::HarnessSetupFailed(format!(
            "failed to parse recall task {}: {error}",
            path.display()
        ))
    })
}

/// Run every query of `task` against `memory`, returning one outcome each.
///
/// `project_id` must be the project the corpus was stored under; queries use
/// the `Project` namespace (`top_k` sized to the strictest bound plus one
/// slot of headroom so a rank exactly at `min_rank` is always observable).
pub async fn run_recall_queries(
    memory: &dyn MemoryStore,
    task: &RecallTask,
    project_id: &ProjectId,
    cancel: CancellationToken,
) -> Vec<RecallOutcome> {
    let max_min_rank = task.queries.iter().map(|query| query.min_rank).max().unwrap_or(1).max(1);
    let top_k = max_min_rank + 1;

    let mut outcomes = Vec::with_capacity(task.queries.len());
    for recall_query in &task.queries {
        let memory_query = MemoryQuery {
            text: recall_query.query.clone(),
            project_id: project_id.clone(),
            namespace: MemoryNamespace::Project(project_id.clone()),
            top_k,
            filters: vec![],
        };
        // A retrieval failure is observable as "no hit" — it does not abort
        // the whole task, but it does count as a failed outcome.
        let hit_rank = match memory.retrieve(&memory_query, cancel.clone()).await {
            Ok(chunks) => chunks
                .iter()
                .position(|chunk| chunk.content.contains(recall_query.expected.as_str()))
                .map(|index| index + 1),
            Err(_) => None,
        };
        outcomes.push(RecallOutcome {
            query: recall_query.query.clone(),
            expected: recall_query.expected.clone(),
            min_rank: recall_query.min_rank,
            hit_rank,
        });
    }
    outcomes
}

/// Fraction of outcomes that passed their rank bound (0.0 for no queries).
pub fn recall_rate(outcomes: &[RecallOutcome]) -> f64 {
    if outcomes.is_empty() {
        return 0.0;
    }
    let passed = outcomes.iter().filter(|outcome| outcome.passed()).count();
    passed as f64 / outcomes.len() as f64
}
