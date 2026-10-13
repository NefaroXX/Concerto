//! Row 13 proof — link verdicts firing and a cascade reorder on the LIVE
//! cascade path.
//!
//! The DEFERRED register (row 13) records the ADR-69 cascade slices as
//! code-complete and parks the row on "production proof only: link verdicts
//! actually firing, and a cascade reorder observed in a live run". The
//! existing cascade tests in `rag.rs` use a *scripted* scorer; this test
//! exercises the production wiring end to end:
//!
//! - real `SqliteVectorStore` + `SqliteFullTextStore` (the stores the runtime
//!   builds),
//! - a real `LinkStore` holding real `memory_links` rows,
//! - the production `StoreLinkScorer` the `MemorySystem` constructs,
//! - `MemorySystem::retrieve_with_evidence` — the live recall path.
//!
//! It asserts on the RENDERED evidence (order + truth + derivation), not on
//! internal counters: the reorder and the verdict must be observable to a
//! consumer. If the reorder did not fire here, this test would say so.

use std::sync::Arc;

use async_trait::async_trait;
use concerto_core::error::MemoryError;
use concerto_core::memory::{
    ChunkType, EmbeddingRecord, EvidentChunk, FactStatusView, MemoryLink, MemoryLinkKind,
    MemoryNamespace, MemoryQuery, ProjectId,
};
use concerto_core::traits::memory::MemoryStore;
use concerto_core::CancellationToken;
use concerto_memory::decision_store::DecisionStore;
use concerto_memory::embedder::EmbeddingGenerator;
use concerto_memory::fts::SqliteFullTextStore;
use concerto_memory::links::LinkStore;
use concerto_memory::maintenance::MaintenanceLedger;
use concerto_memory::rag::{CascadeTier, LinkCascadeConfig};
use concerto_memory::system::MemorySystem;
use concerto_memory::task_tree::TaskTreeStore;
use concerto_memory::vector_store::{SqliteVectorStore, VectorStore};

/// A query embedder that returns a fixed vector, so chunk vectors control the
/// vector rank deterministically.
struct FixedEmbedder {
    vector: Vec<f32>,
}

#[async_trait]
impl EmbeddingGenerator for FixedEmbedder {
    async fn embed(&self, _text: &str) -> Result<Vec<f32>, MemoryError> {
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

fn project() -> ProjectId {
    ProjectId("row13-live-cascade".into())
}

fn record(id: &str, vector: Vec<f32>) -> EmbeddingRecord {
    EmbeddingRecord {
        id: id.into(),
        project_id: project(),
        chunk_hash: format!("hash-{id}"),
        content: format!("content of {id}"),
        file_path: format!("src/{id}.rs").into(),
        start_line: Some(1),
        end_line: Some(1),
        chunk_type: ChunkType::Function,
        vector,
        model_id: "fixed-test-model".into(),
        model_version: "1".into(),
        stale: false,
        created_at: time::OffsetDateTime::now_utc(),
    }
}

/// Build the live memory system over `pool`, optionally engaging the cascade.
async fn system(
    pool: &sqlx::SqlitePool,
    embedder: Arc<dyn EmbeddingGenerator>,
    link_store: &Arc<LinkStore>,
    with_cascade: bool,
) -> MemorySystem {
    let vector_store = Arc::new(SqliteVectorStore::new(pool.clone()).await.unwrap());
    let fts_store = Arc::new(SqliteFullTextStore::new(pool.clone()).await.unwrap());
    let system = MemorySystem::new(
        vector_store,
        fts_store,
        Arc::new(DecisionStore::new()),
        Arc::new(TaskTreeStore::new()),
        Some(embedder),
        project(),
        None,
    )
    .with_link_store(link_store.clone());
    if with_cascade {
        system.with_link_cascade(LinkCascadeConfig {
            decay_days: Some(90),
            start_tier: CascadeTier::Emergency,
            score_timeout: std::time::Duration::from_millis(250),
        })
    } else {
        system
    }
}

fn query() -> MemoryQuery {
    MemoryQuery {
        // No FTS token matches, so ranking is purely the vector order plus
        // the cascade — a clean live-cascade demonstration.
        text: "zzz-no-fts-match".into(),
        project_id: project(),
        namespace: MemoryNamespace::Project(project()),
        top_k: 3,
        filters: vec![],
    }
}

fn order(chunks: &[EvidentChunk]) -> Vec<&str> {
    chunks.iter().map(|chunk| chunk.chunk.id.as_str()).collect()
}

#[tokio::test]
async fn link_verdicts_fire_and_the_live_cascade_reorders() {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    let ledger = Arc::new(MaintenanceLedger::connect(pool.clone()).await.unwrap());
    let vector_store = SqliteVectorStore::new(pool.clone()).await.unwrap();

    // Three chunks with strictly decreasing vector similarity to the query
    // vector [1, 0, 0]: top (1.0) > promoted (0.8) > other (0.6).
    vector_store
        .store(
            &[
                record("top", vec![1.0, 0.0, 0.0]),
                record("promoted", vec![0.8, 0.6, 0.0]),
                record("other", vec![0.6, 0.8, 0.0]),
            ],
            CancellationToken::new(),
        )
        .await
        .unwrap();

    // The link store the runtime builds, with the H12 ledger attached so the
    // verdict applications are also recorded as maintenance outcomes.
    let link_store = Arc::new(
        LinkStore::new(pool.clone())
            .await
            .unwrap()
            .with_maintenance_ledger(ledger.clone(), project().0.clone()),
    );
    let token = CancellationToken::new();

    // Four fresh `Supports` links promote `promoted` to link score 8.5.
    for source in ["s1", "s2", "s3", "s4"] {
        link_store
            .put(&MemoryLink::new(source, "promoted", MemoryLinkKind::Supports), token.clone())
            .await
            .unwrap();
    }
    // A live contradiction and a live supersession — the standing-changing
    // verdicts the register wants observed firing.
    link_store
        .put(&MemoryLink::new("critic", "top", MemoryLinkKind::Contradicts), token.clone())
        .await
        .unwrap();
    link_store
        .put(&MemoryLink::new("v2", "other", MemoryLinkKind::Supersedes), token.clone())
        .await
        .unwrap();

    let embedder: Arc<dyn EmbeddingGenerator> =
        Arc::new(FixedEmbedder { vector: vec![1.0, 0.0, 0.0] });

    // Plain RRF order (no cascade): the vector order.
    let plain = system(&pool, embedder.clone(), &link_store, false)
        .await
        .retrieve_with_evidence(&query(), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(order(&plain), vec!["top", "promoted", "other"], "plain RRF keeps the vector order");

    // Live cascade order: link evidence re-ranks.
    let cascaded = system(&pool, embedder, &link_store, true)
        .await
        .retrieve_with_evidence(&query(), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(
        order(&cascaded),
        vec!["promoted", "top", "other"],
        "the live cascade must promote the strongly-supported chunk past the \
         vector leader; if this fails the reorder did not fire in practice"
    );

    // The verdicts fire on the live path: contradiction and supersession are
    // rendered with the evidence that produced them.
    let top = cascaded.iter().find(|c| c.chunk.id == "top").unwrap();
    assert_eq!(top.truth, FactStatusView::Contradicted, "live contradiction fires");
    assert!(top.derivation.contains("critic"), "cites the contradictor: {}", top.derivation);

    let other = cascaded.iter().find(|c| c.chunk.id == "other").unwrap();
    assert_eq!(other.truth, FactStatusView::Superseded, "live supersession fires");
    assert!(other.derivation.contains("v2"), "cites the superseder: {}", other.derivation);

    let promoted = cascaded.iter().find(|c| c.chunk.id == "promoted").unwrap();
    assert!(
        promoted.derivation.contains("supported by 4"),
        "the corroboration that drove the reorder is cited: {}",
        promoted.derivation
    );

    // H12: the two standing-changing verdicts are also recorded as durable
    // maintenance outcomes (the corroboration links are not maintenance).
    let outcomes = ledger.list(20, CancellationToken::new()).await.unwrap();
    assert!(
        outcomes
            .iter()
            .any(|r| r.kind == concerto_memory::maintenance::MaintenanceKind::Contradiction
                && r.target == "top"),
        "the contradiction application is recorded: {outcomes:?}"
    );
    assert!(
        outcomes
            .iter()
            .any(|r| r.kind == concerto_memory::maintenance::MaintenanceKind::Supersession
                && r.target == "other"),
        "the supersession application is recorded: {outcomes:?}"
    );
}
