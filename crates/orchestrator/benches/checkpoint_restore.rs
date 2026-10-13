//! H14 benchmark: checkpoint create/restore latency and retained bytes.
//!
//! Times the durable sidecar (`FileCheckpointStore`) at representative sizes —
//! small text, multi-chunk binary within the per-checkpoint budget, and a large
//! multi-chunk file — and records bytes retained versus bytes written. Restore
//! is measured at the sidecar reassembly seam (`CheckpointStore::load_bytes`),
//! which dominates a byte restore; the gated filesystem writes that replay
//! those bytes are covered by the restore-correctness tests, not here.
//!
//! Hermetic and bounded: a file-backed tempdir SQLite database with the real
//! migrations, no network, no container runtime. No absolute value is asserted
//! — the numbers are the baseline for future comparison.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use concerto_core::checkpoint::{
    now_millis, CheckpointByteRef, CheckpointRecord, CheckpointResource, CheckpointResourceKind,
    CheckpointScope, CheckpointStatus, CheckpointStore, PreImageBytes, RestoreMethod,
    CHECKPOINT_BLOB_LIMIT_BYTES,
};
use concerto_core::ids::Ulid;
use concerto_core::CancellationToken;
use concerto_sessions::file_checkpoints::FileCheckpointStore;
use criterion::{black_box, criterion_group, criterion_main, BatchSize, Criterion};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions};

/// One representative fixture: a path and the exact bytes written.
#[derive(Clone)]
struct Fixture {
    path: &'static str,
    bytes: Vec<u8>,
}

/// Small text, multi-chunk binary within budget, and a large multi-chunk file.
fn fixtures() -> Vec<Fixture> {
    let text = b"hello, checkpoint\n".repeat(64);
    let binary: Vec<u8> =
        (0..CHECKPOINT_BLOB_LIMIT_BYTES + 4096).map(|i| (i % 251) as u8).collect();
    let large = vec![b'q'; CHECKPOINT_BLOB_LIMIT_BYTES * 3 + 7];
    vec![
        Fixture { path: "small.txt", bytes: text },
        Fixture { path: "bin.dat", bytes: binary },
        Fixture { path: "large.txt", bytes: large },
    ]
}

/// File-backed pool with the real sessions migrations (hermetic tempdir).
async fn test_pool(dir: &Path) -> SqlitePool {
    let options = SqliteConnectOptions::new()
        .filename(dir.join("checkpoints.db"))
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .busy_timeout(Duration::from_secs(5))
        .foreign_keys(true)
        .synchronous(sqlx::sqlite::SqliteSynchronous::Normal);
    let pool = SqlitePoolOptions::new()
        .max_connections(4)
        .connect_with(options)
        .await
        .expect("bench pool connects");
    sqlx::migrate!("../sessions/migrations").run(&pool).await.expect("migrations apply");
    pool
}

fn banked(path: &str, bytes_len: usize, cid: &Ulid) -> CheckpointResource {
    CheckpointResource {
        kind: CheckpointResourceKind::FileOverwrite,
        canonical_path: path.to_owned(),
        pre_hash: None,
        post_hash: Some("post".to_owned()),
        byte_ref: Some(CheckpointByteRef {
            checkpoint_id: cid.to_string(),
            canonical_path: path.to_owned(),
        }),
        size_bytes: Some(bytes_len as u64),
    }
}

/// A fresh record + blobs for one create iteration (unique id per iteration, so
/// the sidecar primary key never collides).
fn fresh_checkpoint(fixtures: &[Fixture]) -> (CheckpointRecord, BTreeMap<String, Vec<u8>>) {
    let cid = Ulid::new();
    let included: Vec<CheckpointResource> =
        fixtures.iter().map(|f| banked(f.path, f.bytes.len(), &cid)).collect();
    let record = CheckpointRecord::new(
        cid,
        Ulid::new(),
        CheckpointScope::FilesystemWrites {
            paths: fixtures.iter().map(|f| f.path.to_owned()).collect(),
        },
        included,
        Vec::new(),
        BTreeMap::new(),
        CheckpointStatus::Active,
        RestoreMethod::PreImageStore,
        Vec::new(),
        "bench".to_owned(),
        now_millis(),
        None,
    )
    .expect("bench record builds");
    let blobs = fixtures.iter().map(|f| (f.path.to_owned(), f.bytes.clone())).collect();
    (record, blobs)
}

/// The bytes the sidecar retains for one path (panics if it did not bank them).
async fn load_retained(store: &FileCheckpointStore, checkpoint_id: &str, path: &str) -> usize {
    match store.load_bytes(checkpoint_id, path, CancellationToken::new()).await {
        Ok(PreImageBytes::Stored { bytes, .. }) => bytes.len(),
        Ok(other) => panic!("{path} must retain bytes, got {other:?}"),
        Err(error) => panic!("load_bytes({path}) failed: {error:?}"),
    }
}

fn bench_checkpoint(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let dir = tempfile::tempdir().expect("tempdir");
    let store = rt.block_on(async { FileCheckpointStore::new(test_pool(dir.path()).await) });
    let fixtures = fixtures();
    let written_total: usize = fixtures.iter().map(|f| f.bytes.len()).sum();

    // --- Create: one checkpoint banking all representative sizes. ---
    {
        let fixtures = fixtures.clone();
        let store = store.clone();
        c.bench_function("checkpoint/create_representative_sizes", |b| {
            let fixtures = fixtures.clone();
            let store = store.clone();
            b.to_async(&rt).iter_batched(
                || fresh_checkpoint(&fixtures),
                |(record, blobs)| {
                    let store = store.clone();
                    async move {
                        let report = store
                            .create_checkpoint(&record, &blobs, &CancellationToken::new())
                            .await
                            .expect("create");
                        black_box((report.stored, report.hash_only))
                    }
                },
                BatchSize::SmallInput,
            );
        });
    }

    // --- Restore reassembly: one checkpoint created once, then the sidecar
    //     bytes reassembled on every timed iteration. Retained == written is
    //     asserted as the bench premise (the accounting invariant). ---
    let (record, blobs) = fresh_checkpoint(&fixtures);
    let checkpoint_id = record.checkpoint_id.to_string();
    rt.block_on(async {
        store
            .create_checkpoint(&record, &blobs, &CancellationToken::new())
            .await
            .expect("seed create");
    });
    let retained_total: usize = rt.block_on(async {
        let mut total = 0usize;
        for fixture in &fixtures {
            total += load_retained(&store, &checkpoint_id, fixture.path).await;
        }
        total
    });
    assert_eq!(
        retained_total, written_total,
        "premise: retained bytes equal bytes written ({retained_total} vs {written_total})"
    );

    c.bench_function("checkpoint/restore_reassembly_representative_sizes", |b| {
        let store = store.clone();
        let checkpoint_id = checkpoint_id.clone();
        let paths: Vec<&'static str> = fixtures.iter().map(|f| f.path).collect();
        b.to_async(&rt).iter(|| {
            let store = store.clone();
            let checkpoint_id = checkpoint_id.clone();
            let paths = paths.clone();
            async move {
                let mut total = 0usize;
                for path in &paths {
                    total += load_retained(&store, &checkpoint_id, path).await;
                }
                assert_eq!(total, written_total, "retained bytes stay equal to bytes written");
                black_box(total)
            }
        });
    });
}

criterion_group!(benches, bench_checkpoint);
criterion_main!(benches);
