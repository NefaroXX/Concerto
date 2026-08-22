//! Benchmark for SQLite vector store operations and hybrid retrieval.
//!
//! Measures insert throughput, brute-force cosine similarity search latency
//! at various corpus sizes, and hybrid (FTS + vector) retrieval fusion.
//! Uses synthetic random embedding vectors so results are independent of any
//! particular embedding model.
//!
//! All stores use an in-memory SQLite database to isolate I/O from the
//! benchmark measurement.

use std::sync::Arc;

use camino::Utf8PathBuf;
use concerto_core::memory::{
    ChunkType, EmbeddingRecord, MemoryChunk, MemoryNamespace, MemoryQuery, ProjectId,
};
use concerto_core::CancellationToken;
use concerto_memory::fts::{FullTextStore, SqliteFullTextStore};
use concerto_memory::rag::HybridRetriever;
use concerto_memory::vector_store::{SqliteVectorStore, VectorStore};
use criterion::{black_box, criterion_group, criterion_main, Criterion};
use time::OffsetDateTime;

/// Dimensionality for synthetic embedding vectors (matches default fastembed).
const EMBEDDING_DIM: usize = 384;

/// Generate a random unit-normalized embedding vector.
fn random_embedding() -> Vec<f32> {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    let mut v: Vec<f32> = (0..EMBEDDING_DIM).map(|_| rng.gen::<f32>()).collect();
    let norm: f64 = v.iter().map(|x| (*x as f64).powi(2)).sum::<f64>().sqrt();
    if norm > 0.0 {
        for x in &mut v {
            *x = (*x as f64 / norm) as f32;
        }
    }
    v
}

fn make_record(id: &str, project: &ProjectId, content: &str) -> EmbeddingRecord {
    let utc_now = OffsetDateTime::now_utc();
    EmbeddingRecord {
        id: id.into(),
        project_id: project.clone(),
        chunk_hash: blake3::hash(content.as_bytes()).to_hex().to_string(),
        content: content.into(),
        file_path: Utf8PathBuf::from("/src/lib.rs"),
        start_line: Some(1),
        end_line: Some(10),
        chunk_type: ChunkType::Function,
        vector: random_embedding(),
        model_id: "benchmark".into(),
        model_version: "0.1.0".into(),
        stale: false,
        created_at: utc_now,
    }
}

fn make_chunk(id: &str, project: &ProjectId, content: &str) -> MemoryChunk {
    MemoryChunk {
        id: id.into(),
        project_id: project.clone(),
        namespace: MemoryNamespace::Project(project.clone()),
        content: content.into(),
        file_path: Some(Utf8PathBuf::from("/src/lib.rs")),
        start_line: Some(1),
        end_line: Some(10),
        chunk_type: ChunkType::Function,
        score: 1.0,
        model_id: "benchmark".into(),
        model_version: "0.1.0".into(),
    }
}

/// Seed both the vector and FTS stores with `count` chunks.
async fn seed_stores(
    vector_store: &SqliteVectorStore,
    fts_store: &SqliteFullTextStore,
    project: &ProjectId,
    count: usize,
) {
    // Seed vector store in batches of 100 to keep insert benchmark independent.
    const BATCH: usize = 100;
    for batch_start in (0..count).step_by(BATCH) {
        let batch_end = (batch_start + BATCH).min(count);
        let mut records = Vec::with_capacity(batch_end - batch_start);
        for i in batch_start..batch_end {
            let content = format!(
                "Benchmark function compute_{i} handles fibonacci numbers, \
                 cache lookups, and database connections with vector search similarity."
            );
            records.push(make_record(&format!("v_{i}"), project, &content));
        }
        vector_store.store(&records, CancellationToken::new()).await.unwrap();
    }

    // Seed FTS store.
    for i in 0..count {
        let content = match i % 5 {
            0 => format!("The function compute_{i} calculates fibonacci numbers using an iterative approach with constant memory allocation"),
            1 => format!("The struct Config{i} holds database connection parameters including host, port, username, password, and database name for PostgreSQL connections"),
            2 => format!("Error handling in validate_input_{i} checks for null values, out-of-range integers, and malformed email addresses before processing"),
            3 => format!("The cache implementation Cache{i} uses a Least Recently Used eviction policy with O(1) average time complexity for get and put operations"),
            _ => format!("Authentication middleware Auth{i} verifies JWT tokens by checking the signature, expiration time, and issuer claims before allowing access"),
        };
        fts_store
            .insert(
                &make_chunk(&format!("v_{i}"), project, &content),
                project,
                CancellationToken::new(),
            )
            .await
            .unwrap();
    }
}

// ---------------------------------------------------------------------------
// Vector store insert benchmarks
// ---------------------------------------------------------------------------

fn bench_vector_insert(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut group = c.benchmark_group("vector_insert");

    for (batch_size, label) in &[(1, "single"), (10, "batch_10"), (100, "batch_100")] {
        let pid = ProjectId(format!("insert_bench_{label}"));
        let pool =
            rt.block_on(async { sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap() });
        let store = rt.block_on(async { SqliteVectorStore::new(pool).await.unwrap() });

        // Pre-build records outside the measured loop.
        let records: Vec<EmbeddingRecord> = (0..*batch_size)
            .map(|i| {
                make_record(
                    &format!("insert_{label}_{i}"),
                    &pid,
                    &format!("Insert benchmark content for record {i}"),
                )
            })
            .collect();

        group.bench_function(format!("store/{label}"), |b| {
            b.to_async(&rt).iter(|| async {
                // Clone records each iteration (vectors are ~384 f32s each).
                let batch: Vec<EmbeddingRecord> = records
                    .iter()
                    .map(|r| EmbeddingRecord {
                        id: format!("{}_{}", r.id, rand::random::<u16>()),
                        ..r.clone()
                    })
                    .collect();
                let _ = store.store(&batch, CancellationToken::new()).await;
            });
        });
    }

    group.finish();
}

// ---------------------------------------------------------------------------
// Vector search benchmarks (brute-force cosine similarity)
// ---------------------------------------------------------------------------

fn bench_vector_search(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut group = c.benchmark_group("vector_search");

    for (corpus_size, label) in &[(100, "100"), (1_000, "1k"), (5_000, "5k")] {
        let pid = ProjectId(format!("search_bench_{label}"));
        let pool =
            rt.block_on(async { sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap() });
        let vstore = rt.block_on(async { SqliteVectorStore::new(pool).await.unwrap() });
        let fstore_pool =
            rt.block_on(async { sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap() });
        let fstore = rt.block_on(async { SqliteFullTextStore::new(fstore_pool).await.unwrap() });

        rt.block_on(seed_stores(&vstore, &fstore, &pid, *corpus_size));

        let query_vec = random_embedding();

        let bench_name = format!("search/{label}_corpus");
        let qv = query_vec.clone();
        group.bench_function(&bench_name, |b| {
            b.to_async(&rt).iter(|| async {
                let _ = black_box(
                    vstore.search(&pid, &qv, black_box(10), CancellationToken::new()).await,
                );
            });
        });
    }

    group.finish();
}

// ---------------------------------------------------------------------------
// Vector list (browse) benchmarks
// ---------------------------------------------------------------------------

fn bench_vector_list(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut group = c.benchmark_group("vector_list");

    for (corpus_size, label) in &[(100, "100"), (1_000, "1k"), (5_000, "5k")] {
        let pid = ProjectId(format!("list_bench_{label}"));
        let pool =
            rt.block_on(async { sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap() });
        let vstore = rt.block_on(async { SqliteVectorStore::new(pool).await.unwrap() });
        let fstore_pool =
            rt.block_on(async { sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap() });
        let fstore = rt.block_on(async { SqliteFullTextStore::new(fstore_pool).await.unwrap() });

        rt.block_on(seed_stores(&vstore, &fstore, &pid, *corpus_size));

        group.bench_function(format!("list/{label}_corpus"), |b| {
            b.to_async(&rt).iter(|| async {
                let _ = black_box(vstore.list(&pid, black_box(50), CancellationToken::new()).await);
            });
        });
    }

    group.finish();
}

// ---------------------------------------------------------------------------
// Hybrid retrieval benchmark (FTS + vector search + RRF fusion)
// ---------------------------------------------------------------------------

fn bench_hybrid_retrieval(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut group = c.benchmark_group("hybrid_retrieval");

    for (corpus_size, label) in &[(100, "100"), (1_000, "1k")] {
        let pid = ProjectId(format!("hybrid_bench_{label}"));
        let vpool =
            rt.block_on(async { sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap() });
        let vstore = rt.block_on(async { SqliteVectorStore::new(vpool).await.unwrap() });
        let fpool =
            rt.block_on(async { sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap() });
        let fstore = rt.block_on(async { SqliteFullTextStore::new(fpool).await.unwrap() });

        rt.block_on(seed_stores(&vstore, &fstore, &pid, *corpus_size));

        let retriever = HybridRetriever::new(Arc::new(vstore), Arc::new(fstore));

        let query = MemoryQuery {
            text: "fibonacci numbers".into(),
            project_id: pid.clone(),
            namespace: MemoryNamespace::Project(pid),
            top_k: 10,
            filters: vec![],
        };
        let embedding = random_embedding();

        group.bench_function(format!("retrieve/{label}_corpus"), |b| {
            let q = query.clone();
            let emb = embedding.clone();
            b.to_async(&rt).iter(|| async {
                let _ = black_box(
                    retriever
                        .retrieve(
                            black_box(&q),
                            black_box(&emb),
                            black_box(None),
                            CancellationToken::new(),
                        )
                        .await,
                );
            });
        });
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_vector_insert,
    bench_vector_search,
    bench_vector_list,
    bench_hybrid_retrieval,
);
criterion_main!(benches);
