//! H14 benchmark: verification latency and redundant-run coalescing.
//!
//! Measures the wall time of a project-wide check through the production
//! [`VerifierService`] and demonstrates that repeated identical checks within
//! one workspace generation collapse to a single execution. The spawn/eval
//! seams are counted with the public [`CountingSpawner`] wrapper; the same
//! count assertion the H05 coalescing test makes is restated here as a
//! benchmark premise, so a coalescing regression shows up as both a wall-time
//! change and a failing bench target.
//!
//! Hermetic and bounded: a tempdir workspace, an in-process stub spawner (no
//! network, no container runtime, no real `cargo`), and a small per-spawn
//! delay chosen only so the coalesced (cache-hit) arm is visibly cheaper than
//! the cold arm. No absolute value is asserted — the numbers are the baseline.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use camino::Utf8PathBuf;
use concerto_core::types::{EvalProvenance, EvalResult, TestRunner};
use concerto_core::CancellationToken;
use concerto_eval::{EvalEngine, EvalError};
use concerto_orchestrator::verify::{
    CountingSpawner, LspDiagnostics, LspQueryError, SpawnError, SpawnOutput, VerifierService,
    VerifyRequest, VerifyScope, VerifySelector, VerifySpawner,
};
use criterion::{black_box, criterion_group, criterion_main, BatchSize, Criterion};

/// Per-spawn delay for the stub: enough that the cold (real-spawn) arm is
/// measurably more expensive than the coalesced (cache-hit) arm, small enough
/// that the whole target stays CI-appropriate.
const STUB_SPAWN_DELAY: Duration = Duration::from_millis(1);

/// In-process spawner with no processes, network, or container runtime.
struct StubSpawner {
    spawn_delay: Duration,
}

impl StubSpawner {
    fn new() -> Self {
        Self { spawn_delay: STUB_SPAWN_DELAY }
    }

    fn canned_eval() -> EvalResult {
        EvalResult {
            runner: TestRunner::Cargo,
            exit_code: 0,
            passed: true,
            duration_ms: 1,
            output_tail: "test result: ok".to_string(),
            coverage: None,
            provenance: EvalProvenance::Harness,
        }
    }
}

#[async_trait]
impl VerifySpawner for StubSpawner {
    async fn spawn(
        &self,
        _program: &str,
        _args: &[&str],
        _cwd: &Path,
        _timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<SpawnOutput, SpawnError> {
        tokio::select! {
            () = cancel.cancelled() => Err(SpawnError::Cancelled),
            () = tokio::time::sleep(self.spawn_delay) => Ok(SpawnOutput {
                success: true,
                stdout: String::new(),
                stderr: String::new(),
                code: Some(0),
            }),
        }
    }

    async fn run_eval(
        &self,
        _eval: &EvalEngine,
        _dir: &Path,
        _files: &[Utf8PathBuf],
        cancel: &CancellationToken,
    ) -> Result<EvalResult, EvalError> {
        if cancel.is_cancelled() {
            return Err(EvalError::Cancelled);
        }
        Ok(Self::canned_eval())
    }

    async fn lsp_diagnostics(
        &self,
        _project_root: &Path,
        _file: &Path,
        cancel: &CancellationToken,
    ) -> Result<LspDiagnostics, LspQueryError> {
        if cancel.is_cancelled() {
            return Err(LspQueryError::Cancelled);
        }
        Ok(LspDiagnostics { diagnostics: Vec::new(), analyzed: true })
    }

    fn set_governed_eval(&self, _eval: EvalEngine) {}
}

/// Write the minimal workspace the project-scope adapters detect.
fn fixture(dir: &Path) {
    std::fs::write(dir.join("Cargo.toml"), "[package]\nname = \"bench\"\n").expect("Cargo.toml");
    std::fs::create_dir_all(dir.join("src")).expect("src dir");
    std::fs::write(dir.join("src/a.rs"), "pub fn a() {}\n").expect("a.rs");
    std::fs::write(dir.join("src/b.rs"), "pub fn b() {}\n").expect("b.rs");
}

/// A fresh service over `dir` with a counting stub spawner (empty caches).
fn fresh_service(dir: &Path) -> (Arc<VerifierService>, Arc<CountingSpawner>) {
    let counting = Arc::new(CountingSpawner::new(Arc::new(StubSpawner::new())));
    let service = Arc::new(VerifierService::with_spawner(
        dir.to_path_buf(),
        EvalEngine::new(dir),
        counting.clone(),
    ));
    (service, counting)
}

fn bench_verification(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let dir = tempfile::tempdir().expect("tempdir");
    fixture(dir.path());
    let dir_path = dir.path().to_path_buf();

    let project_build = VerifyRequest {
        scope: VerifyScope::Project,
        selector: VerifySelector::BuildCheck,
        timeout: Duration::from_secs(30),
        overlay_pending: false,
    };
    let project_all = VerifyRequest::project();

    // --- Coalescing premise: two identical project requests share one spawn
    //     and one harness run (the H05 assertion, restated at the bench). ---
    {
        let (service, counting) = fresh_service(dir.path());
        let cancel = CancellationToken::new();
        rt.block_on(async {
            service.verify(&project_all, &cancel).await.expect("first verify");
        });
        let spawns_after_first = counting.spawn_count();
        let evals_after_first = counting.eval_count();
        rt.block_on(async {
            service.verify(&project_all, &cancel).await.expect("second verify");
        });
        assert_eq!(
            counting.spawn_count(),
            spawns_after_first,
            "coalescing premise: a repeat project check adds no spawn"
        );
        assert_eq!(
            counting.eval_count(),
            evals_after_first,
            "coalescing premise: a repeat project check adds no harness run"
        );
    }

    let mut group = c.benchmark_group("verify");

    // Cold project-scope build check: a fresh service each iteration, so the
    // workspace-wide `cargo check` spawn actually runs.
    {
        let request = project_build.clone();
        group.bench_function("project_build_cold", |b| {
            let request = request.clone();
            b.to_async(&rt).iter_batched(
                || fresh_service(&dir_path).0,
                |service| {
                    let request = request.clone();
                    async move {
                        let records = service
                            .verify(&request, &CancellationToken::new())
                            .await
                            .expect("verify runs");
                        black_box(records.len())
                    }
                },
                BatchSize::SmallInput,
            );
        });
    }

    // Coalesced project-scope build check: one warm service; the spawn cache
    // is keyed on the workspace generation, so the timed calls reuse it.
    {
        let (service, counting) = fresh_service(dir.path());
        let cancel = CancellationToken::new();
        rt.block_on(async {
            service.verify(&project_build, &cancel).await.expect("warm verify");
        });
        let warm_spawns = counting.spawn_count();
        assert_eq!(warm_spawns, 1, "premise: the warm call ran one build spawn");
        let request = project_build.clone();
        group.bench_function("project_build_coalesced", |b| {
            let request = request.clone();
            b.to_async(&rt).iter(|| {
                let request = request.clone();
                let service = service.clone();
                async move {
                    let records = service
                        .verify(&request, &CancellationToken::new())
                        .await
                        .expect("verify runs");
                    black_box(records.len())
                }
            });
        });
    }

    // All-adapter project run: the general verification latency (build +
    // harness + existence + LSP over the fixture workspace).
    {
        let request = project_all.clone();
        group.bench_function("project_all_cold", |b| {
            let request = request.clone();
            b.to_async(&rt).iter_batched(
                || fresh_service(&dir_path).0,
                |service| {
                    let request = request.clone();
                    async move {
                        let records = service
                            .verify(&request, &CancellationToken::new())
                            .await
                            .expect("verify runs");
                        black_box(records.len())
                    }
                },
                BatchSize::SmallInput,
            );
        });
    }

    group.finish();
}

criterion_group!(benches, bench_verification);
criterion_main!(benches);
