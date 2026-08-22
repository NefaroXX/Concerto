//! Benchmark for SQLite FTS5 search latency.
//!
//! Measures search throughput across various corpus sizes and query
//! selectivity levels. Uses an in-memory SQLite database to isolate
//! storage I/O from the benchmark measurement.

use camino::Utf8PathBuf;
use concerto_core::memory::{ChunkType, MemoryChunk, MemoryNamespace, ProjectId};
use concerto_core::CancellationToken;
use concerto_memory::fts::{FullTextStore, SqliteFullTextStore};
use criterion::{black_box, criterion_group, criterion_main, Criterion};

fn make_chunk(id: &str, content: &str, project: &ProjectId) -> MemoryChunk {
    MemoryChunk {
        id: id.into(),
        project_id: project.clone(),
        namespace: MemoryNamespace::Project(project.clone()),
        content: content.into(),
        file_path: Some(Utf8PathBuf::from("/src/lib.rs")),
        start_line: Some(1),
        end_line: Some(5),
        chunk_type: ChunkType::Function,
        score: 0.0,
        model_id: String::new(),
        model_version: String::new(),
    }
}

/// Seed the store with `count` chunks.
async fn seed_store(store: &SqliteFullTextStore, project: &ProjectId, count: usize) {
    for i in 0..count {
        let content = match i % 5 {
            0 => format!("The function compute_{i} calculates fibonacci numbers using an iterative approach with constant memory allocation"),
            1 => format!("The struct Config{i} holds database connection parameters including host, port, username, password, and database name for PostgreSQL connections"),
            2 => format!("Error handling in validate_input_{i} checks for null values, out-of-range integers, and malformed email addresses before processing"),
            3 => format!("The cache implementation Cache{i} uses a Least Recently Used eviction policy with O(1) average time complexity for get and put operations"),
            _ => format!("Authentication middleware Auth{i} verifies JWT tokens by checking the signature, expiration time, and issuer claims before allowing access"),
        };
        store
            .insert(
                &make_chunk(&format!("chunk_{i}"), &content, project),
                project,
                CancellationToken::new(),
            )
            .await
            .unwrap();
    }
}

fn bench_fts_search(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut group = c.benchmark_group("fts_search");

    // --- Search benchmarks at various corpus sizes ---
    let queries: &[(&str, &str)] = &[
        ("high_selectivity", "fibonacci"),
        ("medium_selectivity", "database"),
        ("low_selectivity", "the"),
    ];

    for (corpus_size, label) in &[(100, "100"), (1_000, "1k"), (10_000, "10k")] {
        let pid = ProjectId(format!("bench_{label}"));
        let pool =
            rt.block_on(async { sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap() });
        let store = rt.block_on(async { SqliteFullTextStore::new(pool).await.unwrap() });
        rt.block_on(seed_store(&store, &pid, *corpus_size));

        for (selectivity, query) in queries {
            let bench_name = format!("search/{label}_corpus/{selectivity}");
            let q = query.to_string();
            group.bench_function(&bench_name, |b| {
                b.to_async(&rt).iter(|| async {
                    let _ = store.search(&q, &pid, black_box(10), CancellationToken::new()).await;
                });
            });
        }
    }

    // --- Insert benchmark ---
    {
        let pool =
            rt.block_on(async { sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap() });
        let store = rt.block_on(async { SqliteFullTextStore::new(pool).await.unwrap() });
        let pid = ProjectId("insert_bench".into());
        let chunk = make_chunk(
            "bench_insert",
            "Benchmark content for measuring insert throughput into the FTS5 index",
            &pid,
        );

        group.bench_function("insert/single_chunk", |b| {
            b.to_async(&rt).iter(|| async {
                let _ = store.insert(&chunk, &pid, CancellationToken::new()).await;
            });
        });
    }

    // --- Delete by project (1k corpus) ---
    {
        let pool =
            rt.block_on(async { sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap() });
        let store = rt.block_on(async { SqliteFullTextStore::new(pool).await.unwrap() });
        let pid = ProjectId("delete_bench".into());
        rt.block_on(seed_store(&store, &pid, 1_000));

        group.bench_function("delete/by_project_1k", |b| {
            b.to_async(&rt).iter(|| async {
                let _ = store.delete_by_project(&pid, CancellationToken::new()).await;
            });
        });
    }

    group.finish();
}

criterion_group!(benches, bench_fts_search);
criterion_main!(benches);
