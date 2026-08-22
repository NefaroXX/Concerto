//! Benchmark for VirtualFs operations — write, read, and commit.
//!
//! Measures:
//! - Sequential write throughput (N files into virtual fs)
//! - Bulk commit to disk (tempdir)
//! - Mixed read/write patterns

use camino::Utf8Path;
use concerto_tools::virtual_fs::VirtualFs;
use criterion::{black_box, criterion_group, criterion_main, Criterion};

/// Write `count` files to the VirtualFs with given content size.
fn write_files(fs: &mut VirtualFs, count: usize, content_size: usize) {
    let content = "x".repeat(content_size);
    for i in 0..count {
        fs.write(
            Utf8Path::new(&format!("src/module_{i}.rs")),
            format!("{content}\n// end of module_{i}\n"),
        )
        .unwrap();
    }
}

fn bench_virtual_fs(c: &mut Criterion) {
    let mut group = c.benchmark_group("virtual_fs");

    // --- Write 100 small files ---
    group.bench_function("write/100_files_1kb", |b| {
        b.iter(|| {
            let mut fs = VirtualFs::new();
            write_files(&mut fs, 100, 1_000);
            black_box(fs.changed_paths().len());
        });
    });

    // --- Write 1000 small files ---
    group.bench_function("write/1000_files_1kb", |b| {
        b.iter(|| {
            let mut fs = VirtualFs::new();
            write_files(&mut fs, 1000, 1_000);
            black_box(fs.changed_paths().len());
        });
    });

    // --- Write 100 medium files (10KB each) ---
    group.bench_function("write/100_files_10kb", |b| {
        b.iter(|| {
            let mut fs = VirtualFs::new();
            write_files(&mut fs, 100, 10_000);
            black_box(fs.changed_paths().len());
        });
    });

    // --- Read after write (1000 files) ---
    group.bench_function("read/1000_files_1kb", |b| {
        let mut fs = VirtualFs::new();
        write_files(&mut fs, 1000, 1_000);
        b.iter(|| {
            for i in 0..1000 {
                let name = format!("src/module_{i}.rs");
                let path = Utf8Path::new(&name);
                let content = fs.read(path).unwrap();
                black_box(content.len());
            }
        });
    });

    // --- Commit 100 files to real disk (all Created entries) ---
    group.bench_function("commit/100_files_1kb", |b| {
        b.iter_batched(
            || {
                let dir = tempfile::tempdir().unwrap();
                let mut fs = VirtualFs::new();
                for i in 0..100 {
                    let path = dir.path().join(format!("module_{i}.rs"));
                    let utf8_path = Utf8Path::from_path(&path).unwrap().to_owned();
                    fs.write(&utf8_path, format!("file content for module {i}")).unwrap();
                }
                (dir, fs)
            },
            |(_dir, fs)| {
                let report = fs.commit_to_disk().unwrap();
                black_box(report);
            },
            criterion::BatchSize::SmallInput,
        );
    });

    // --- Commit 1000 files to real disk (all Created entries) ---
    group.bench_function("commit/1000_files_1kb", |b| {
        b.iter_batched(
            || {
                let dir = tempfile::tempdir().unwrap();
                let mut fs = VirtualFs::new();
                for i in 0..1000 {
                    let path = dir.path().join(format!("module_{i}.rs"));
                    let utf8_path = Utf8Path::from_path(&path).unwrap().to_owned();
                    fs.write(&utf8_path, format!("file content for module {i}")).unwrap();
                }
                (dir, fs)
            },
            |(_dir, fs)| {
                let report = fs.commit_to_disk().unwrap();
                black_box(report);
            },
            criterion::BatchSize::SmallInput,
        );
    });

    // --- Snapshot (1000 files) ---
    group.bench_function("snapshot/1000_files", |b| {
        let mut fs = VirtualFs::new();
        write_files(&mut fs, 1000, 1_000);
        b.iter(|| {
            let snap = fs.snapshot();
            black_box(snap.len());
        });
    });

    // --- Restore (1000 files) ---
    group.bench_function("restore/1000_files", |b| {
        let mut fs = VirtualFs::new();
        write_files(&mut fs, 1000, 1_000);
        let snap = fs.snapshot();
        b.iter(|| {
            let mut clone_fs = VirtualFs::new();
            clone_fs.restore(snap.clone());
            black_box(clone_fs.changed_paths().len());
        });
    });

    group.finish();
}

criterion_group!(benches, bench_virtual_fs);
criterion_main!(benches);
