//! PersonaMem recall guard test (ADR-46 symbolic offload).
//!
//! Seeds the real SQLite-backed memory stores (SqliteVectorStore +
//! SqliteFullTextStore) with the `persona_mem/cold_ask` task's seed facts and
//! distractors, then verifies the hybrid recall surfaces each expected fact
//! within its rank bound. This is the CI regression guard for the L1 typed
//! extraction + LLM dedup work: stable user facts must keep ranking near the
//! top after long sessions of unrelated stores.
//!
//! Runs against the repository's disk-backed `target/eval-scratch` directory
//! (never `/tmp` — it is RAM-backed in this environment).

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use concerto_core::error::MemoryError;
use concerto_core::memory::{ChunkType, MemoryEntry, MemoryId, MemoryNamespace, ProjectId};
use concerto_core::traits::MemoryStore;
use concerto_core::CancellationToken;
use concerto_eval::persona_mem::{load_recall_task, recall_rate, run_recall_queries};
use concerto_memory::decision_store::DecisionStore;
use concerto_memory::embedder::EmbeddingGenerator;
use concerto_memory::fts::SqliteFullTextStore;
use concerto_memory::system::MemorySystem;
use concerto_memory::task_tree::TaskTreeStore;
use concerto_memory::vector_store::SqliteVectorStore;
use sqlx::sqlite::SqlitePoolOptions;
use time::OffsetDateTime;

/// Deterministic, token-hash embedding generator so the recall path is
/// exercised end-to-end without any external model in CI.
struct TokenHashEmbedder {
    dims: usize,
}

impl TokenHashEmbedder {
    fn new(dims: usize) -> Self {
        Self { dims }
    }
}

#[async_trait]
impl EmbeddingGenerator for TokenHashEmbedder {
    async fn embed(&self, text: &str) -> Result<Vec<f32>, MemoryError> {
        let mut counts = vec![0.0f32; self.dims];
        for token in text.split(|ch: char| !ch.is_alphanumeric()).filter(|token| !token.is_empty())
        {
            use std::hash::{Hash, Hasher};
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            token.to_lowercase().hash(&mut hasher);
            let index = (hasher.finish() as usize) % self.dims;
            counts[index] += 1.0;
        }
        let norm = counts.iter().map(|value| value * value).sum::<f32>().sqrt();
        if norm == 0.0 {
            return Ok(vec![0.0; self.dims]);
        }
        Ok(counts.into_iter().map(|value| value / norm).collect())
    }

    fn model_id(&self) -> &str {
        "token-hash/test"
    }

    fn model_version(&self) -> &str {
        "1"
    }

    fn dims(&self) -> usize {
        self.dims
    }
}

fn fact_entry(project_id: &ProjectId, content: &str) -> MemoryEntry {
    MemoryEntry {
        id: MemoryId(ulid::Ulid::new()),
        project_id: project_id.clone(),
        namespace: MemoryNamespace::Project(project_id.clone()),
        content: content.to_string(),
        chunk_type: ChunkType::Fact,
        model_id: Some("token-hash/test".into()),
        model_version: Some("1".into()),
        metadata: serde_json::json!({"type": "persona"}),
        expires_at: None,
        created_at: OffsetDateTime::now_utc(),
    }
}

#[tokio::test]
async fn persona_mem_cold_ask_recall_passes() {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let scratch_dir = Path::new(manifest_dir).join("../../target/eval-scratch");
    std::fs::create_dir_all(&scratch_dir).expect("create eval-scratch dir");
    // Canonicalize so the file path has no `..` segments for SQLite.
    let scratch_dir = scratch_dir.canonicalize().expect("canonicalize eval-scratch dir");
    let db_path = scratch_dir.join(format!("persona_mem_recall-{}.sqlite", std::process::id()));
    if db_path.exists() {
        std::fs::remove_file(&db_path).expect("remove stale scratch db");
    }

    let options = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(&db_path)
        .create_if_missing(true)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
        .synchronous(sqlx::sqlite::SqliteSynchronous::Normal);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .expect("connect scratch sqlite pool");

    let vector_store = Arc::new(SqliteVectorStore::new(pool.clone()).await.expect("vector store"));
    let fts_store = Arc::new(SqliteFullTextStore::new(pool).await.expect("fts store"));
    let project_id = ProjectId(ulid::Ulid::new().to_string());
    let memory: Arc<dyn MemoryStore> = Arc::new(MemorySystem::new(
        vector_store,
        fts_store,
        Arc::new(DecisionStore::new()),
        Arc::new(TaskTreeStore::new()),
        Some(Arc::new(TokenHashEmbedder::new(384))),
        project_id.clone(),
        None,
    ));

    let task_dir = Path::new(manifest_dir).join("../eval/benchmark_tasks/persona_mem/cold_ask");
    let task = load_recall_task(&task_dir).expect("load cold_ask recall task");
    assert!(task.queries.len() > 1, "fixture must have recall queries");

    // Seeds first ("the long session corpus"), then distractors.
    for fact in task.seed_facts.iter().chain(task.distractors.iter()) {
        memory
            .store(fact_entry(&project_id, fact), CancellationToken::new())
            .await
            .expect("seed fact store");
    }

    let outcomes =
        run_recall_queries(memory.as_ref(), &task, &project_id, CancellationToken::new()).await;
    let rate = recall_rate(&outcomes);
    assert!(
        rate >= task.min_recall_rate,
        "persona_mem cold_ask recall {:.1}% < required {:.1}%; outcomes: {outcomes:#?}",
        rate * 100.0,
        task.min_recall_rate * 100.0,
    );
}
