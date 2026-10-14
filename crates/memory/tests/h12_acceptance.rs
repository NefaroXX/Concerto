//! H12 acceptance scenario — maintenance and continuity proof, end to end.
//!
//! Session A records evidence; session B recalls it with attribution; the
//! source then changes; the next recall marks or revalidates it appropriately;
//! a contradiction remains visible across a maintenance pass. The whole thing
//! is repeated with vector retrieval disabled. Every assertion is on the
//! RENDERED evidence (source + status + reason), never on internal counters.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use concerto_core::memory::{
    ChunkType, EmbeddingRecord, EvidentChunk, FactStatusView, MemoryLink, MemoryLinkKind,
    MemoryNamespace, MemoryQuery, PathObservation, PathObservationSource, ProjectId, RecallSource,
};
use concerto_core::traits::memory::MemoryStore;
use concerto_core::CancellationToken;
use concerto_memory::decision_store::DecisionStore;
use concerto_memory::embedder::EmbeddingGenerator;
use concerto_memory::fts::SqliteFullTextStore;
use concerto_memory::links::LinkStore;
use concerto_memory::maintenance::{MaintenanceKind, MaintenanceLedger};
use concerto_memory::storage::MemoryDb;
use concerto_memory::sync::ChunkSyncService;
use concerto_memory::system::MemorySystem;
use concerto_memory::task_tree::TaskTreeStore;
use concerto_memory::ttl::TtlManager;
use concerto_memory::vector_store::SqliteVectorStore;

const FILE_PATH: &str = "src/lib.rs";

struct FixedEmbedder {
    vector: Vec<f32>,
}

#[async_trait]
impl EmbeddingGenerator for FixedEmbedder {
    async fn embed(&self, _text: &str) -> Result<Vec<f32>, concerto_core::error::MemoryError> {
        Ok(self.vector.clone())
    }
    fn model_id(&self) -> &str {
        "fixed-test-model"
    }
    fn model_version(&self) -> &str {
        "1"
    }
    fn dims(&self) -> usize {
        self.vector.len()
    }
}

/// A controllable source-observation seam (H11's `PathObservationSource`): the
/// "source changes" step flips the row dirty, and a later re-observation
/// flips it clean again (the revalidation).
struct ScriptedObservations {
    rows: Mutex<HashMap<String, PathObservation>>,
}

impl ScriptedObservations {
    fn new() -> Self {
        Self { rows: Mutex::new(HashMap::new()) }
    }

    fn set(&self, path: &str, dirty: bool, event: &str) {
        self.rows.lock().unwrap().insert(
            path.to_string(),
            PathObservation {
                dirty,
                generation: Some("g1".to_string()),
                last_event_id: Some(event.to_string()),
            },
        );
    }
}

#[async_trait]
impl PathObservationSource for ScriptedObservations {
    async fn observe_paths(
        &self,
        file_paths: &[String],
        _cancel: CancellationToken,
    ) -> HashMap<String, PathObservation> {
        let rows = self.rows.lock().unwrap();
        file_paths
            .iter()
            .filter_map(|path| rows.get(path).map(|row| (path.clone(), row.clone())))
            .collect()
    }
}

fn project() -> ProjectId {
    ProjectId("h12-acceptance".into())
}

fn query() -> MemoryQuery {
    MemoryQuery {
        text: "continuity marker".into(),
        project_id: project(),
        namespace: MemoryNamespace::Project(project()),
        top_k: 5,
        filters: vec![],
    }
}

async fn build_system(
    pool: &sqlx::SqlitePool,
    embedder: Option<Arc<dyn EmbeddingGenerator>>,
    link_store: &Arc<LinkStore>,
    observations: Arc<ScriptedObservations>,
) -> MemorySystem {
    // The stores are rebuilt over the same file-backed pool: a genuinely
    // fresh handle, i.e. another session.
    let vector_store = Arc::new(SqliteVectorStore::new(pool.clone()).await.expect("vector opens"));
    let fts_store = Arc::new(SqliteFullTextStore::new(pool.clone()).await.expect("fts opens"));
    MemorySystem::new(
        vector_store,
        fts_store,
        Arc::new(DecisionStore::new()),
        Arc::new(TaskTreeStore::new()),
        embedder,
        project(),
        None,
    )
    .with_link_store(link_store.clone())
    .with_path_observations(observations)
}

fn only_recall(chunks: &[EvidentChunk]) -> &EvidentChunk {
    assert_eq!(chunks.len(), 1, "exactly the one recorded evidence recalls: {chunks:?}");
    &chunks[0]
}

async fn recall(system: &MemorySystem) -> Vec<EvidentChunk> {
    system.retrieve_with_evidence(&query(), CancellationToken::new()).await.unwrap()
}

/// The acceptance walk, run once with vectors enabled and once disabled.
async fn run_acceptance(vector_enabled: bool) {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = camino::Utf8PathBuf::from_path_buf(dir.path().join("memory.db")).expect("utf-8");
    let db = Arc::new(MemoryDb::connect(&db_path).await.expect("memory db opens (migrations run)"));
    let pool = db.pool().clone();

    let ledger = Arc::new(MaintenanceLedger::connect(pool.clone()).await.unwrap());
    let link_store = Arc::new(
        LinkStore::new(pool.clone())
            .await
            .unwrap()
            .with_maintenance_ledger(ledger.clone(), project().0.clone()),
    );

    let observations = Arc::new(ScriptedObservations::new());
    // Session A records evidence: an indexed chunk backed by a real file, and
    // a clean observation of that source.
    observations.set(FILE_PATH, false, "ev-clean-1");
    let vector = if vector_enabled { vec![1.0, 0.0, 0.0] } else { Vec::new() };
    let record = EmbeddingRecord {
        id: "evidence-1".into(),
        project_id: project(),
        chunk_hash: "hash-evidence-1".into(),
        content: "session continuity marker alpha".into(),
        file_path: FILE_PATH.into(),
        start_line: Some(1),
        end_line: Some(1),
        chunk_type: ChunkType::Function,
        vector: vector.clone(),
        model_id: "fixed-test-model".into(),
        model_version: "1".into(),
        stale: false,
        created_at: time::OffsetDateTime::now_utc(),
    };
    // The canonical indexed write path (vector + FTS), exactly as the indexer
    // writes it.
    let seeder_vector = Arc::new(SqliteVectorStore::new(pool.clone()).await.unwrap());
    let seeder_fts = Arc::new(SqliteFullTextStore::new(pool.clone()).await.unwrap());
    ChunkSyncService::new(seeder_vector, seeder_fts)
        .store(&record, CancellationToken::new())
        .await
        .expect("session A records the evidence");

    // Session B: a FRESH handle over the same file — the bytes survived the
    // handle boundary and the recall carries attribution.
    let embedder: Option<Arc<dyn EmbeddingGenerator>> =
        vector_enabled.then(|| Arc::new(FixedEmbedder { vector: vec![1.0, 0.0, 0.0] }) as Arc<_>);
    let session_b = build_system(&pool, embedder.clone(), &link_store, observations.clone()).await;

    let first = recall(&session_b).await;
    let evidence = only_recall(&first);
    assert_eq!(evidence.chunk.id, "evidence-1", "identity survives the session boundary");
    assert_eq!(evidence.source, RecallSource::RuntimeObserved, "clean observation verifies");
    assert_eq!(evidence.truth, FactStatusView::Verified);
    assert!(
        evidence.derivation.contains("ev-clean-1"),
        "the recall cites the observation: {}",
        evidence.derivation
    );

    // The source then changes: the observation goes dirty.
    observations.set(FILE_PATH, true, "ev-dirty-2");
    let changed = recall(&session_b).await;
    let evidence = only_recall(&changed);
    assert_eq!(evidence.truth, FactStatusView::Stale, "a changed source marks the item stale");
    assert!(
        evidence.derivation.contains("dirty") && evidence.derivation.contains("ev-dirty-2"),
        "the stale derivation names the dirty observation: {}",
        evidence.derivation
    );

    // Revalidation: the source is re-observed clean; the next recall
    // re-verifies it (the item is revalidated, not merely kept stale).
    observations.set(FILE_PATH, false, "ev-clean-3");
    let revalidated = recall(&session_b).await;
    let evidence = only_recall(&revalidated);
    assert_eq!(evidence.truth, FactStatusView::Verified, "a revalidation re-verifies");
    assert!(
        evidence.derivation.contains("ev-clean-3"),
        "the revalidation cites the new observation: {}",
        evidence.derivation
    );

    // A contradiction is applied (a link verdict). It must remain visible.
    link_store
        .put(
            &MemoryLink::new("critic", "evidence-1", MemoryLinkKind::Contradicts),
            CancellationToken::new(),
        )
        .await
        .expect("contradiction link applied");
    let contradicted = recall(&session_b).await;
    let evidence = only_recall(&contradicted);
    assert_eq!(evidence.truth, FactStatusView::Contradicted, "the contradiction is visible");
    assert!(
        evidence.derivation.contains("critic"),
        "cites the contradictor: {}",
        evidence.derivation
    );

    // A maintenance pass runs. The contradiction must survive it — still
    // visible and correctly attributed, never swept.
    let ttl = TtlManager::new(
        Arc::new(SqliteVectorStore::new(pool.clone()).await.unwrap()),
        Arc::new(SqliteFullTextStore::new(pool.clone()).await.unwrap()),
        pool.clone(),
    );
    ttl.purge_expired(&project(), CancellationToken::new()).await.expect("maintenance pass runs");
    let after_maintenance = recall(&session_b).await;
    let evidence = only_recall(&after_maintenance);
    assert_eq!(
        evidence.truth,
        FactStatusView::Contradicted,
        "the contradiction survives a maintenance pass: {}",
        evidence.derivation
    );
    assert!(evidence.derivation.contains("critic"));

    // H12: the maintenance that happened is durable, not a log line — the
    // contradiction application and the pass are both recorded.
    let outcomes = ledger.list(50, CancellationToken::new()).await.unwrap();
    assert!(
        outcomes.iter().any(|r| r.kind == MaintenanceKind::Contradiction
            && r.target == "evidence-1"
            && r.outcome == concerto_memory::maintenance::MaintenanceOutcome::Completed),
        "the contradiction application is recorded: {outcomes:?}"
    );
    assert!(
        outcomes.iter().any(|r| r.kind == MaintenanceKind::Expiry),
        "the maintenance pass is recorded: {outcomes:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn acceptance_with_vector_retrieval() {
    run_acceptance(true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn acceptance_with_vector_retrieval_disabled() {
    run_acceptance(false).await;
}
