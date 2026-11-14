#![deny(clippy::all)]
#![deny(unused_imports, unused_variables, dead_code)]
#![allow(missing_docs)]

//! `concerto-eval` — Phase 3 evaluation harness.
//!
//! Detects the project's test runner (`cargo`, `npm`, `pytest`, `make`) and
//! runs the test suite, returning an `EvalResult`.

use std::path::{Path, PathBuf};

use std::time::Duration;

use concerto_config::ShellProfileConfig;
use concerto_core::types::{EvalProvenance, EvalResult, TestRunner};
use concerto_core::CancellationToken;
use concerto_tools::shell_backend::ShellProfileFactory;
use thiserror::Error;
use tokio::process::Command;

/// Governed process execution supplied by the application's tool executor.
#[async_trait::async_trait]
pub trait EvalProcessExecutor: Send + Sync {
    async fn execute(
        &self,
        program: &str,
        args: &[&str],
        cwd: &Path,
        cancel: CancellationToken,
    ) -> Result<concerto_core::types::ToolOutput, EvalError>;
}

pub mod categories;
mod coverage;
#[cfg(test)]
mod coverage_regressions;
pub mod observation;
pub mod persona_mem;
pub mod runner;
pub mod scenarios;
mod test_output;
pub use runner::EvalRunner;

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum EvalError {
    /// No supported test harness applies to this existing project directory.
    /// Distinct from a configured harness that could not be started.
    #[error("eval harness unavailable: {0}")]
    HarnessUnavailable(String),
    #[error("eval harness setup failed: {0}")]
    HarnessSetupFailed(String),
    #[error("test runner failed: {0}")]
    TestRunnerFailed(String),
    /// The governed process executor refused the spawn under policy. Stays
    /// distinct from [`Self::TestRunnerFailed`] so verification adapters can
    /// report a denial honestly instead of as a command failure — a denied
    /// check never ran and must never fall back to an ungoverned spawn.
    #[error("policy denied: {0}")]
    Denied(String),
    #[error("timed out after {0:?}")]
    Timeout(Duration),
    #[error("cancelled")]
    Cancelled,
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// One finished governed-or-direct command: exit code plus captured streams.
/// Adapters map this to their records; denial/timeout/cancellation travel as
/// [`EvalError`] so they stay distinguishable from a command failure.
#[derive(Debug, Clone)]
pub struct CommandOutput {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl CommandOutput {
    /// True only for exit code 0 (the command's own success signal).
    pub fn success(&self) -> bool {
        self.exit_code == 0
    }
}

// ---------------------------------------------------------------------------
// EvalEngine
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct EvalEngine {
    project_dir: PathBuf,
    timeout: Duration,
    shell_profile: Option<ShellProfileConfig>,
    process_executor: Option<std::sync::Arc<dyn EvalProcessExecutor>>,
}

impl EvalEngine {
    /// Create a new eval engine for the given project directory.
    /// The test runner is detected when validation runs so projects created
    /// after this engine is constructed are handled correctly.
    pub fn new(project_dir: impl Into<PathBuf>) -> Self {
        Self {
            project_dir: project_dir.into(),
            timeout: Duration::from_secs(300),
            shell_profile: None,
            process_executor: None,
        }
    }

    /// Create a new eval engine with a custom timeout.
    pub fn with_timeout(project_dir: impl Into<PathBuf>, timeout: Duration) -> Self {
        Self {
            project_dir: project_dir.into(),
            timeout,
            shell_profile: None,
            process_executor: None,
        }
    }

    #[must_use]
    pub fn with_process_executor(
        mut self,
        executor: std::sync::Arc<dyn EvalProcessExecutor>,
    ) -> Self {
        self.process_executor = Some(executor);
        self
    }

    /// The governed process executor, when one is installed: the same object
    /// the harness routes test runs through (the run's policy-gated `shell`
    /// tool path). Verification build-check spawns reuse it so a policy
    /// decision applies exactly as it would for the equivalent `shell` tool
    /// call — one execution chokepoint, no second path (ADR-62).
    pub fn process_executor(&self) -> Option<std::sync::Arc<dyn EvalProcessExecutor>> {
        self.process_executor.clone()
    }

    /// Run one adapter-chosen command (`program args...` in `cwd`), bounded by
    /// `timeout` and `cancel`.
    ///
    /// With a [`Self::with_process_executor`] executor installed the command
    /// travels the governed policy path (denials surface as
    /// [`EvalError::Denied`] and are never retried ungoverned); without one it
    /// spawns directly, preserving the legacy sweep behavior for contexts
    /// with no executor. The caller never chooses the command — adapters pick
    /// it; only the working directory varies per run.
    pub async fn run_command(
        &self,
        program: &str,
        args: &[&str],
        cwd: &Path,
        timeout: Duration,
        cancel: CancellationToken,
    ) -> Result<CommandOutput, EvalError> {
        if cancel.is_cancelled() {
            return Err(EvalError::Cancelled);
        }
        if let Some(executor) = &self.process_executor {
            let execution_cancel = cancel.child_token();
            let execution = executor.execute(program, args, cwd, execution_cancel.clone());
            tokio::pin!(execution);
            let output = tokio::select! {
                result = &mut execution => result?,
                () = cancel.cancelled() => {
                    execution_cancel.cancel();
                    let _ = execution.await;
                    return Err(EvalError::Cancelled);
                }
                () = tokio::time::sleep(timeout) => {
                    execution_cancel.cancel();
                    let _ = execution.await;
                    return Err(EvalError::Timeout(timeout));
                }
            };
            let exit_code = output
                .data
                .get("exit_code")
                .and_then(serde_json::Value::as_i64)
                .and_then(|code| i32::try_from(code).ok())
                .ok_or_else(|| {
                    EvalError::TestRunnerFailed("native executor returned no exit code".into())
                })?;
            let stdout =
                output.data.get("stdout").and_then(serde_json::Value::as_str).unwrap_or_default();
            let stderr =
                output.data.get("stderr").and_then(serde_json::Value::as_str).unwrap_or_default();
            return Ok(CommandOutput {
                exit_code,
                stdout: stdout.to_string(),
                stderr: stderr.to_string(),
            });
        }
        let mut command = Command::new(program);
        command.args(args);
        let child = command
            .current_dir(cwd)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        let output = tokio::select! {
            output = tokio::time::timeout(timeout, child.wait_with_output()) => output,
            () = cancel.cancelled() => return Err(EvalError::Cancelled),
        };
        match output {
            Ok(Ok(out)) => Ok(CommandOutput {
                exit_code: out.status.code().unwrap_or(-1),
                stdout: String::from_utf8_lossy(&out.stdout).to_string(),
                stderr: String::from_utf8_lossy(&out.stderr).to_string(),
            }),
            Ok(Err(error)) => Err(EvalError::Io(error)),
            Err(_elapsed) => Err(EvalError::Timeout(timeout)),
        }
    }

    /// Run validation commands through a configured build/validation profile.
    #[must_use]
    pub fn with_shell_profile(mut self, profile: ShellProfileConfig) -> Self {
        self.shell_profile = Some(profile);
        self
    }

    /// Manifest file names [`Self::detect_runner`] recognizes, in priority
    /// order (the first match inside a directory decides the runner).
    const RUNNER_MANIFESTS: [&str; 5] =
        ["Cargo.toml", "package.json", "pyproject.toml", "setup.py", "Makefile"];

    fn manifest_in_dir(dir: &Path) -> Option<&'static str> {
        Self::RUNNER_MANIFESTS.iter().find(|manifest| dir.join(manifest).exists()).copied()
    }

    /// Resolve the directory the eval harness should run in, preferring the
    /// manifest-bearing directory nearest the run's modified files
    /// (validation-root resolution from run evidence).
    ///
    /// Agents legitimately build inside a subdirectory of the session root
    /// (e.g. a "create a new Cargo project" instruction); runner detection
    /// then fails at the root with `Unknown("no config file found")` even
    /// though a fully valid project exists one level down. For every
    /// evidence path kept under the project root, each ancestor directory
    /// up to (and including) the root itself is checked for a runner
    /// manifest; the deepest manifest-bearing directory (closest to the
    /// files) wins, ties break on the lexicographically smallest path for
    /// determinism. When nothing better exists — no manifest anywhere, no
    /// evidence at all, or the nearest manifest IS the root — the project
    /// root is returned, preserving today's behavior including the honest
    /// `Unknown("no config file found")` failure message.
    pub fn resolve_manifest_root(
        project_root: &Path,
        modified_files: &[camino::Utf8PathBuf],
    ) -> PathBuf {
        let root_depth = project_root.components().count();
        let mut best: Option<(usize, PathBuf)> = None;
        for file in modified_files {
            // Evidence under the root only: an absolute path outside the
            // root, or a relative path escaping it, carries no signal.
            let resolved = if file.is_absolute() {
                PathBuf::from(file.as_str())
            } else {
                project_root.join(file.as_std_path())
            };
            if !resolved.starts_with(project_root) {
                continue;
            }
            for ancestor in resolved.ancestors() {
                let depth = ancestor.components().count();
                if depth < root_depth {
                    break;
                }
                if Self::manifest_in_dir(ancestor).is_some() {
                    let dir_depth = depth;
                    let dir_path = ancestor.to_path_buf();
                    best = match &best {
                        Some((best_depth, best_path))
                            if *best_depth > dir_depth
                                || (*best_depth == dir_depth && *best_path <= dir_path) =>
                        {
                            best.clone()
                        }
                        _ => Some((dir_depth, dir_path)),
                    };
                    break;
                }
            }
        }
        best.map_or_else(|| project_root.to_path_buf(), |(_, path)| path)
    }

    /// Detect the test runner by checking for known config files.
    pub fn detect_runner(project_dir: &Path) -> TestRunner {
        if project_dir.join("Cargo.toml").exists() {
            TestRunner::Cargo
        } else if project_dir.join("package.json").exists() {
            TestRunner::Npm
        } else if project_dir.join("pyproject.toml").exists()
            || project_dir.join("setup.py").exists()
        {
            TestRunner::Pytest
        } else if project_dir.join("Makefile").exists() {
            TestRunner::Make
        } else {
            TestRunner::Unknown("no config file found".into())
        }
    }

    /// Return the command string that would be executed.
    pub fn dry_run(&self) -> String {
        match Self::detect_runner(&self.project_dir) {
            TestRunner::Cargo => "cargo test".into(),
            TestRunner::Npm => "npm test".into(),
            TestRunner::Pytest => "pytest".into(),
            TestRunner::Make => "make test".into(),
            TestRunner::Unknown(s) => format!("unknown runner: {s}"),
            _ => "unknown runner".into(),
        }
    }

    /// Run the test suite with explicit argument list.
    async fn run_with_args(
        &self,
        runner: TestRunner,
        args: &[&str],
        project_dir: &Path,
        cancel: CancellationToken,
    ) -> Result<EvalResult, EvalError> {
        let start = std::time::Instant::now();
        let cmd = match &runner {
            TestRunner::Cargo => "cargo",
            TestRunner::Npm => "npm",
            TestRunner::Pytest => "pytest",
            TestRunner::Make => "make",
            TestRunner::Unknown(s) => {
                return Err(EvalError::HarnessSetupFailed(format!("cannot run tests: {s}")));
            }
            _ => "unknown",
        };
        if let Some(executor) = &self.process_executor {
            let execution_cancel = cancel.child_token();
            let execution = executor.execute(cmd, args, project_dir, execution_cancel.clone());
            tokio::pin!(execution);
            let output = tokio::select! {
                result = &mut execution => result?,
                _ = tokio::time::sleep(self.timeout) => {
                    execution_cancel.cancel();
                    let _ = execution.await;
                    return Err(EvalError::Timeout(self.timeout));
                }
            };
            let exit_code = output
                .data
                .get("exit_code")
                .and_then(serde_json::Value::as_i64)
                .and_then(|code| i32::try_from(code).ok())
                .ok_or_else(|| {
                    EvalError::TestRunnerFailed("native executor returned no exit code".into())
                })?;
            return Ok(test_output::completed_result(
                runner,
                exit_code,
                u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
                output.data.get("stdout").and_then(serde_json::Value::as_str).unwrap_or_default(),
                output.data.get("stderr").and_then(serde_json::Value::as_str).unwrap_or_default(),
            ));
        }
        let mut command = if let Some(profile) = &self.shell_profile {
            let backend = ShellProfileFactory::backend_for(profile);
            backend.check_available(profile).map_err(|error| {
                EvalError::HarnessSetupFailed(format!(
                    "build shell profile '{}' is unavailable: {error}",
                    profile.name
                ))
            })?;
            let full_command =
                std::iter::once(cmd).chain(args.iter().copied()).collect::<Vec<_>>().join(" ");
            let mut command = Command::new(backend.resolved_program(profile));
            command.args(backend.command_args(profile, &full_command));
            let base = std::env::vars().collect();
            command.envs(backend.effective_env(profile, &base));
            command
        } else {
            let mut command = Command::new(cmd);
            command.args(args);
            command
        };
        let child = command
            .current_dir(project_dir)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        let output = tokio::select! {
            output = tokio::time::timeout(self.timeout, child.wait_with_output()) => output,
            _ = cancel.cancelled() => return Err(EvalError::Cancelled),
        };
        let duration_ms = start.elapsed().as_millis() as u64;
        match output {
            Ok(Ok(out)) => Ok(test_output::completed_result(
                runner,
                out.status.code().unwrap_or(-1),
                duration_ms,
                &String::from_utf8_lossy(&out.stdout),
                &String::from_utf8_lossy(&out.stderr),
            )),
            Ok(Err(e)) => Err(EvalError::Io(e)),
            Err(_elapsed) => Ok(EvalResult {
                runner,
                exit_code: -1,
                passed: false,
                duration_ms,
                output_tail: "timed out".into(),
                coverage: None,
                provenance: EvalProvenance::Harness,
            }),
        }
    }

    /// Run the test suite (full) – default entry point.
    pub async fn run(&self, cancel: CancellationToken) -> Result<EvalResult, EvalError> {
        self.run_in_dir(&self.project_dir, cancel).await
    }

    /// Run the full test suite in `project_dir` — the evidence-resolved entry
    /// point for validators that resolve their working root from the run's
    /// modified files (see [`Self::resolve_manifest_root`]).
    pub async fn run_in_dir(
        &self,
        project_dir: &Path,
        cancel: CancellationToken,
    ) -> Result<EvalResult, EvalError> {
        let runner = Self::detect_runner(project_dir);
        self.run_for_runner(runner, project_dir, cancel).await
    }

    async fn run_for_runner(
        &self,
        runner: TestRunner,
        project_dir: &Path,
        cancel: CancellationToken,
    ) -> Result<EvalResult, EvalError> {
        if cancel.is_cancelled() {
            return Err(EvalError::Cancelled);
        }
        // Keep the ancestor-Cargo fallback. Absence of a supported harness
        // in an existing directory is unavailability, not an execution error;
        // an invalid/missing project directory must still report its failure.
        if let TestRunner::Unknown(reason) = &runner {
            if Self::cargo_project_in_ancestors(project_dir) {
                return self.run_fallback_cargo_test(project_dir, reason, cancel).await;
            }
            if !std::fs::metadata(project_dir)?.is_dir() {
                return Err(EvalError::HarnessSetupFailed(format!(
                    "project root is not a directory: {}",
                    project_dir.display()
                )));
            }
            return Err(EvalError::HarnessUnavailable(reason.clone()));
        }
        // Default args per runner
        let default_args: &[&str] = match &runner {
            TestRunner::Cargo => &["test"],
            TestRunner::Npm => &["test"],
            TestRunner::Pytest => &[],
            TestRunner::Make => &["test"],
            // Handled above; unreachable, but keep the match total.
            TestRunner::Unknown(_) => &[],
            _ => &[],
        };
        self.run_with_args(runner, default_args, project_dir, cancel).await
    }

    /// No-config fallback: run a plain `cargo test` in `project_dir` and mark
    /// the result [`EvalProvenance::Fallback`] with a note naming the missing
    /// harness config. A pass also requires confirmed executed Rust tests.
    async fn run_fallback_cargo_test(
        &self,
        project_dir: &Path,
        reason: &str,
        cancel: CancellationToken,
    ) -> Result<EvalResult, EvalError> {
        let mut result =
            self.run_with_args(TestRunner::Cargo, &["test"], project_dir, cancel).await?;
        result.provenance = EvalProvenance::Fallback;
        result.output_tail = format!(
            "eval fallback: no harness config found ({reason}); ran plain `cargo test`.\n{}",
            result.output_tail
        );
        Ok(result)
    }

    /// True when `dir` or any ancestor carries a `Cargo.toml`, i.e. a plain
    /// `cargo test` run from `dir` can resolve a package to validate. Cargo
    /// itself searches ancestors for the manifest, so a manifest-free run root
    /// inside a Cargo project is still validatable; a directory with no Cargo
    /// project anywhere is not.
    fn cargo_project_in_ancestors(dir: &Path) -> bool {
        dir.ancestors().any(|ancestor| ancestor.join("Cargo.toml").exists())
    }

    /// Run tests scoped to changed paths; fall back to the full suite when no
    /// reliable mapping exists. Cargo always uses the full suite at the resolved
    /// root: Rust module/test names cannot be inferred safely from file paths.
    pub async fn run_scoped(
        &self,
        changed_paths: &[camino::Utf8PathBuf],
        cancel: CancellationToken,
    ) -> Result<EvalResult, EvalError> {
        self.run_scoped_in_dir(&self.project_dir, changed_paths, cancel).await
    }

    /// Run tests scoped to changed paths in `project_dir` (evidence-resolved
    /// variant of [`Self::run_scoped`]). The scoped-args mapping still keys
    /// off the project-root-relative spellings of `changed_paths`, mirroring
    /// [`Self::run_scoped`]'s semantics at the configured root.
    pub async fn run_scoped_in_dir(
        &self,
        project_dir: &Path,
        changed_paths: &[camino::Utf8PathBuf],
        cancel: CancellationToken,
    ) -> Result<EvalResult, EvalError> {
        let runner = Self::detect_runner(project_dir);
        if let Some(arg_vec) = Self::scoped_args(&runner, changed_paths) {
            let args_ref: Vec<&str> = arg_vec.iter().map(|s| s.as_str()).collect();
            self.run_with_args(runner, &args_ref, project_dir, cancel).await
        } else {
            self.run_for_runner(runner, project_dir, cancel).await
        }
    }

    /// Map changed file paths to test runner arguments.
    /// Returns None if mapping is uncertain or unsupported.
    fn scoped_args(
        runner: &TestRunner,
        changed_paths: &[camino::Utf8PathBuf],
    ) -> Option<Vec<String>> {
        match runner {
            // File paths are not test dependencies: crate roots, #[path]
            // modules and integration tests defeat name-based guesses. Even a
            // nonempty filtered run can omit the tests affected by an edit.
            TestRunner::Cargo => None,
            TestRunner::Pytest => {
                // Return paths as-is for pytest (could be used as arguments)
                Some(changed_paths.iter().map(|p| p.to_string()).collect())
            }
            TestRunner::Npm | TestRunner::Make => None,
            TestRunner::Unknown(_) => None,
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use concerto_config::ShellBackendType;

    struct GovernedExecutor {
        deny: bool,
        wait_for_cancel: bool,
        calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }
    #[async_trait::async_trait]
    impl EvalProcessExecutor for GovernedExecutor {
        async fn execute(
            &self,
            program: &str,
            args: &[&str],
            _cwd: &Path,
            cancel: CancellationToken,
        ) -> Result<concerto_core::types::ToolOutput, EvalError> {
            use std::sync::atomic::Ordering;
            self.calls.fetch_add(1, Ordering::SeqCst);
            assert_eq!(program, "cargo");
            assert_eq!(args, &["test"]);
            if self.wait_for_cancel {
                cancel.cancelled().await;
                return Err(EvalError::Cancelled);
            }
            if self.deny {
                return Err(EvalError::TestRunnerFailed("policy denied".into()));
            }
            let stdout = format!(
                "test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; \
                 0 filtered out; finished in 0.00s\n{}",
                "🙂".repeat(2100),
            );
            Ok(concerto_core::types::ToolOutput {
                summary: "validated".into(),
                data: serde_json::json!({"exit_code":0,"stdout":stdout,"stderr":""}),
            })
        }
    }

    // verifies: governed validation uses explicit argv, keeps Unicode tails intact, and propagates denials.
    #[tokio::test]
    async fn validation_uses_governed_process_executor() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let directory = tempfile::tempdir().expect("project");
        std::fs::write(directory.path().join("Cargo.toml"), "[package]").expect("manifest");
        for deny in [false, true] {
            let calls = std::sync::Arc::new(AtomicUsize::new(0));
            let engine = EvalEngine::new(directory.path())
                .with_shell_profile(ShellProfileConfig {
                    executable: "unavailable-system-shell".into(),
                    ..Default::default()
                })
                .with_process_executor(std::sync::Arc::new(GovernedExecutor {
                    deny,
                    wait_for_cancel: false,
                    calls: calls.clone(),
                }));
            let result = engine.run(CancellationToken::new()).await;
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            if deny {
                assert!(result.is_err());
            } else {
                let result = result.expect("validation");
                assert!(result.passed);
                assert_eq!(result.output_tail.chars().count(), 2000);
            }
            assert!(engine.run_with_coverage(CancellationToken::new()).await.is_err());
            assert!(engine.run_coverage(CancellationToken::new()).await.is_err());
            assert!(engine.run_scoped_with_coverage(&[], CancellationToken::new()).await.is_err());
            assert_eq!(calls.load(Ordering::SeqCst), 1, "coverage must not bypass the gate");
            assert!(!directory.path().join("target").exists());
        }
    }

    // verifies: an eval deadline cancels and awaits backend cleanup instead of abandoning the process future.
    #[tokio::test(start_paused = true)]
    async fn governed_validation_timeout_cancels_backend() {
        let directory = tempfile::tempdir().expect("project");
        std::fs::write(directory.path().join("Cargo.toml"), "[package]").expect("manifest");
        let engine = EvalEngine::with_timeout(directory.path(), Duration::from_secs(1))
            .with_process_executor(std::sync::Arc::new(GovernedExecutor {
                deny: false,
                wait_for_cancel: true,
                calls: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            }));
        assert!(matches!(engine.run(CancellationToken::new()).await, Err(EvalError::Timeout(_))));
    }

    #[test]
    fn detect_runner_cargo() {
        let dir = tempfile::tempdir().unwrap();
        let cargo = dir.path().join("Cargo.toml");
        std::fs::write(&cargo, "[package]\nname = \"test\"\n").unwrap();
        assert_eq!(EvalEngine::detect_runner(dir.path()), TestRunner::Cargo);
    }

    #[test]
    fn detect_runner_npm() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path().join("package.json");
        std::fs::write(&pkg, "{}\n").unwrap();
        assert_eq!(EvalEngine::detect_runner(dir.path()), TestRunner::Npm);
    }

    #[test]
    fn detect_runner_pytest_pyproject() {
        let dir = tempfile::tempdir().unwrap();
        let pyproject = dir.path().join("pyproject.toml");
        std::fs::write(&pyproject, "[build-system]\n").unwrap();
        assert_eq!(EvalEngine::detect_runner(dir.path()), TestRunner::Pytest);
    }

    #[test]
    fn detect_runner_makefile() {
        let dir = tempfile::tempdir().unwrap();
        let makefile = dir.path().join("Makefile");
        std::fs::write(&makefile, "test:\n\techo ok\n").unwrap();
        assert_eq!(EvalEngine::detect_runner(dir.path()), TestRunner::Make);
    }

    #[test]
    fn detect_runner_unknown() {
        let dir = tempfile::tempdir().unwrap();
        match EvalEngine::detect_runner(dir.path()) {
            TestRunner::Unknown(_) => {} // expected
            _ => panic!("expected Unknown for empty directory"),
        }
    }

    #[test]
    fn detect_runner_unknown_message_is_stable() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            EvalEngine::detect_runner(dir.path()),
            TestRunner::Unknown("no config file found".into())
        );
    }

    /// Validation-root resolution: a manifest inside a build subdirectory
    /// wins — the agent's `hexview/`-style layout validates in the subdir.
    #[test]
    fn resolve_manifest_root_prefers_subdir_manifest() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("hexview/src")).unwrap();
        std::fs::write(dir.path().join("hexview/Cargo.toml"), "[package]\n").unwrap();
        let files = [
            camino::Utf8PathBuf::from("hexview/src/main.rs"),
            camino::Utf8PathBuf::from("."),
            camino::Utf8PathBuf::from("notes.txt"),
        ];
        assert_eq!(
            EvalEngine::resolve_manifest_root(dir.path(), &files),
            dir.path().join("hexview")
        );
    }

    /// The root-manifest shape resolves to the root (unchanged behavior).
    #[test]
    fn resolve_manifest_root_manifest_at_root_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "[package]\n").unwrap();
        let files = vec![camino::Utf8PathBuf::from("src/lib.rs")];
        assert_eq!(EvalEngine::resolve_manifest_root(dir.path(), &files), dir.path());
    }

    /// No manifest anywhere keeps the project root — and the honest
    /// `Unknown("no config file found")` failure stays in place.
    #[test]
    fn resolve_manifest_root_no_manifest_keeps_root_and_honest_failure() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/main.rs"), "fn main() {}\n").unwrap();
        let files = vec![camino::Utf8PathBuf::from("sub/main.rs")];
        assert_eq!(EvalEngine::resolve_manifest_root(dir.path(), &files), dir.path());
        assert_eq!(
            EvalEngine::detect_runner(dir.path()),
            TestRunner::Unknown("no config file found".into())
        );
    }

    /// No evidence at all falls back to the project root.
    #[test]
    fn resolve_manifest_root_no_evidence_falls_back_to_root() {
        let dir = tempfile::tempdir().unwrap();
        let files: Vec<camino::Utf8PathBuf> = Vec::new();
        assert_eq!(EvalEngine::resolve_manifest_root(dir.path(), &files), dir.path());
    }

    /// A manifest both at the root and deeper resolves to the DEEPER one —
    /// the directory nearest the evidence files.
    #[test]
    fn resolve_manifest_root_nearest_wins_over_root() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "[package]\n").unwrap();
        std::fs::create_dir_all(dir.path().join("app/src")).unwrap();
        std::fs::write(dir.path().join("app/Cargo.toml"), "[package]\n").unwrap();
        let files = vec![camino::Utf8PathBuf::from("app/src/main.rs")];
        assert_eq!(EvalEngine::resolve_manifest_root(dir.path(), &files), dir.path().join("app"));
    }

    /// Evidence outside the project root (absolute) carries no signal.
    #[test]
    fn resolve_manifest_root_ignores_out_of_root_evidence() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("Cargo.toml"), "[package]\n").unwrap();
        let files =
            vec![camino::Utf8PathBuf::from_path_buf(outside.path().join("src/x.rs")).unwrap()];
        assert_eq!(EvalEngine::resolve_manifest_root(dir.path(), &files), dir.path());
    }

    #[test]
    fn dry_run_known_runners() {
        let dir = tempfile::tempdir().unwrap();
        let cargo = dir.path().join("Cargo.toml");
        std::fs::write(&cargo, "").unwrap();
        let engine = EvalEngine::new(dir.path());
        assert_eq!(engine.dry_run(), "cargo test");
    }

    #[tokio::test]
    async fn unavailable_build_shell_fails_before_validation_spawn() {
        let dir = tempfile::tempdir().unwrap();
        let profile = ShellProfileConfig {
            id: "missing".to_owned(),
            name: "Missing build shell".to_owned(),
            backend: ShellBackendType::System,
            executable: "definitely-not-a-real-build-shell".to_owned(),
            ..Default::default()
        };
        let engine = EvalEngine::new(dir.path()).with_shell_profile(profile);

        let error = engine
            .run_for_runner(TestRunner::Cargo, dir.path(), CancellationToken::new())
            .await
            .expect_err("missing profile must fail before spawn");

        assert!(error.to_string().contains("Missing build shell"));
    }

    #[test]
    fn dry_run_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let engine = EvalEngine::new(dir.path());
        assert!(engine.dry_run().contains("unknown runner"));
    }

    #[test]
    fn detects_manifest_created_after_engine_construction() {
        let dir = tempfile::tempdir().unwrap();
        let engine = EvalEngine::new(dir.path());
        assert!(engine.dry_run().contains("unknown runner"));

        std::fs::write(dir.path().join("Cargo.toml"), "").unwrap();

        assert_eq!(engine.dry_run(), "cargo test");
    }

    // -----------------------------------------------------------------------
    // No-config fallback tests
    // -----------------------------------------------------------------------

    /// Missing config no longer hard-fails: with a Cargo project reachable, a
    /// plain `cargo test` runs and the result is marked with fallback
    /// provenance. `dir` has no manifest itself but survives with a trivial
    /// Cargo.toml as evidence of a Rust project to validate.
    #[tokio::test]
    async fn missing_config_runs_plain_cargo_test_fallback() {
        let dir = tempfile::tempdir().unwrap();
        // A Cargo manifest in an ancestor but not in `dir` itself: detection
        // fails at `dir`, the fallback resolves the project upward.
        let project_root = dir.path().join("project");
        std::fs::create_dir_all(project_root.join("src")).unwrap();
        std::fs::write(project_root.join("Cargo.toml"), "[workspace]\nmembers = []\n").unwrap();
        std::fs::write(project_root.join("src/lib.rs"), "pub fn f() {}\n").unwrap();
        let build_dir = project_root.join("no-config-here");
        std::fs::create_dir_all(&build_dir).unwrap();

        assert!(matches!(EvalEngine::detect_runner(&build_dir), TestRunner::Unknown(_)));
        let engine = EvalEngine::new(&build_dir);

        let result = engine
            .run(CancellationToken::new())
            .await
            .expect("the no-config fallback must return a result, not hard-fail");

        assert_eq!(result.runner, TestRunner::Cargo);
        assert_eq!(result.provenance, EvalProvenance::Fallback);
        assert!(
            result.output_tail.contains("eval fallback"),
            "the fallback must be reported: {}",
            result.output_tail
        );
        assert!(!result.passed, "the empty workspace has no tests to pass");
    }

    /// The scoped entry point (the coordinator's validation path) falls back
    /// identically.
    #[tokio::test]
    async fn missing_config_scoped_runs_plain_cargo_test_fallback() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "[workspace]\nmembers = []\n").unwrap();
        std::fs::write(dir.path().join("Makefile"), "test:\n\t@true\n").unwrap();
        // `detect_runner` finds the Makefile, so this is a harness run; use a
        // nested manifest-free dir to exercise the fallback via scoped args.
        let nested = dir.path().join("nested");
        std::fs::create_dir_all(nested.join("src")).unwrap();
        let engine = EvalEngine::new(&nested);
        let changed = vec![camino::Utf8PathBuf::from("src/lib.rs")];

        let result = engine
            .run_scoped(&changed, CancellationToken::new())
            .await
            .expect("scoped validation must fall back, not hard-fail");

        assert_eq!(result.provenance, EvalProvenance::Fallback);
        assert!(result.output_tail.contains("eval fallback"));
    }

    /// Absence of a supported harness is distinct from an execution failure;
    /// callers can report unavailability without inventing a passing run.
    #[tokio::test]
    async fn no_config_and_no_cargo_project_reports_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let engine = EvalEngine::new(dir.path());
        let error = engine
            .run(CancellationToken::new())
            .await
            .expect_err("no Cargo project and no harness must stay a clean error");
        assert!(error.to_string().contains("no config file found"), "{error}");
        assert!(matches!(error, EvalError::HarnessUnavailable(_)));
    }

    #[tokio::test]
    async fn unavailable_harness_does_not_hide_invalid_roots_or_cancellation() {
        let dir = tempfile::tempdir().unwrap();
        let error = EvalEngine::new(dir.path().join("missing"))
            .run(CancellationToken::new())
            .await
            .unwrap_err();
        assert!(matches!(error, EvalError::Io(ref error)
            if error.kind() == std::io::ErrorKind::NotFound));
        let file = dir.path().join("file.txt");
        std::fs::write(&file, "not a project directory").unwrap();
        let error = EvalEngine::new(file).run(CancellationToken::new()).await.unwrap_err();
        assert!(matches!(error, EvalError::HarnessSetupFailed(_)));
        let cancel = CancellationToken::new();
        cancel.cancel();
        let error = EvalEngine::new(dir.path()).run(cancel).await.unwrap_err();
        assert!(matches!(error, EvalError::Cancelled));
    }

    /// A detected harness keeps `Harness` provenance (no false fallback mark).
    #[tokio::test]
    async fn detected_harness_keeps_harness_provenance() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Makefile"), "test:\n\t@true\n").unwrap();
        let engine = EvalEngine::new(dir.path());
        let result = engine.run(CancellationToken::new()).await.expect("make test runs");
        assert_eq!(result.provenance, EvalProvenance::Harness);
        assert!(!result.output_tail.contains("eval fallback"));
    }

    /// Legacy serialized results (no provenance field) default to `Harness`.
    #[test]
    fn legacy_eval_result_defaults_to_harness_provenance() {
        let json = r#"{
            "runner": "Cargo",
            "exit_code": 0,
            "passed": true,
            "duration_ms": 100,
            "output_tail": "ok"
        }"#;
        let result: EvalResult = serde_json::from_str(json).unwrap();
        assert_eq!(result.provenance, EvalProvenance::Harness);
    }

    // -----------------------------------------------------------------------
    // Coverage tests
    // -----------------------------------------------------------------------

    #[test]
    fn parse_llvm_cov_json_valid() {
        let json = r#"{
            "data": [{
                "totals": {
                    "lines": {"count": 100, "covered": 75, "percent": 75.0},
                    "functions": {"count": 50, "covered": 40, "percent": 80.0},
                    "branches": {"count": 30, "covered": 20, "percent": 66.7}
                }
            }],
            "version": "1"
        }"#;
        let info = EvalEngine::parse_llvm_cov_json(json).expect("should parse");
        assert_eq!(info.tool, concerto_core::types::CoverageTool::LlvmCov);
        assert!((info.line_percent - 75.0).abs() < 0.01);
        assert!((info.function_percent.unwrap() - 80.0).abs() < 0.01);
        assert!((info.branch_percent.unwrap() - 66.7).abs() < 0.01);
    }

    #[test]
    fn parse_llvm_cov_json_no_functions_no_branches() {
        let json = r#"{
            "data": [{
                "totals": {
                    "lines": {"count": 10, "covered": 5, "percent": 50.0}
                }
            }]
        }"#;
        let info = EvalEngine::parse_llvm_cov_json(json).expect("should parse");
        assert!((info.line_percent - 50.0).abs() < 0.01);
        assert!(info.function_percent.is_none());
        assert!(info.branch_percent.is_none());
    }

    #[test]
    fn parse_llvm_cov_json_malformed_returns_none() {
        assert!(EvalEngine::parse_llvm_cov_json("not valid json").is_none());
        assert!(EvalEngine::parse_llvm_cov_json("{}").is_none());
    }

    #[test]
    fn parse_tarpaulin_json_valid() {
        let json = r#"{"coverage": 82.5, "covered": 33, "coverable": 40}"#;
        let info = EvalEngine::parse_tarpaulin_json(json).expect("should parse");
        assert_eq!(info.tool, concerto_core::types::CoverageTool::Tarpaulin);
        assert!((info.line_percent - 82.5).abs() < 0.01);
        assert!(info.function_percent.is_none());
        assert!(info.branch_percent.is_none());
    }

    #[test]
    fn parse_tarpaulin_json_malformed_returns_none() {
        assert!(EvalEngine::parse_tarpaulin_json("not json").is_none());
        assert!(EvalEngine::parse_tarpaulin_json(
            r#"{"coverage": "abc", "covered": 0, "coverable": 40}"#
        )
        .is_none());
    }

    #[test]
    fn parse_tarpaulin_json_missing_percent_field() {
        let json = r#"{"other_field": 42}"#;
        assert!(EvalEngine::parse_tarpaulin_json(json).is_none());
    }

    #[test]
    fn coverage_info_serde_round_trip() {
        let info = concerto_core::types::CoverageInfo {
            tool: concerto_core::types::CoverageTool::LlvmCov,
            line_percent: 75.0,
            function_percent: Some(80.0),
            branch_percent: Some(66.7),
            raw_tail: "some output".into(),
        };
        let json = serde_json::to_string(&info).unwrap();
        let deser: concerto_core::types::CoverageInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(deser.tool, info.tool);
        assert!((deser.line_percent - 75.0).abs() < 0.01);
        assert!((deser.function_percent.unwrap() - 80.0).abs() < 0.01);
    }

    #[test]
    fn eval_result_with_coverage_serde() {
        let inner = concerto_core::types::CoverageInfo {
            tool: concerto_core::types::CoverageTool::LlvmCov,
            line_percent: 90.0,
            function_percent: None,
            branch_percent: None,
            raw_tail: String::new(),
        };
        let result = EvalResult {
            runner: TestRunner::Cargo,
            exit_code: 0,
            passed: true,
            duration_ms: 100,
            output_tail: "all tests passed".into(),
            coverage: Some(inner),
            provenance: EvalProvenance::Harness,
        };
        let json = serde_json::to_string(&result).unwrap();
        let deser: EvalResult = serde_json::from_str(&json).unwrap();
        assert!(deser.passed);
        assert!(deser.coverage.is_some());
        let cov = deser.coverage.unwrap();
        assert!((cov.line_percent - 90.0).abs() < 0.01);
    }

    #[test]
    fn eval_result_without_coverage_deserializes_legacy() {
        // Without coverage field (legacy format), it should deserialize with coverage = None
        let json = r#"{
            "runner": "Cargo",
            "exit_code": 0,
            "passed": true,
            "duration_ms": 100,
            "output_tail": "ok"
        }"#;
        let result: EvalResult = serde_json::from_str(json).unwrap();
        assert!(result.passed);
        assert!(result.coverage.is_none());
    }

    // -----------------------------------------------------------------------
    // Constructor and configuration tests
    // -----------------------------------------------------------------------

    #[test]
    fn with_timeout_sets_custom_duration() {
        let dir = tempfile::tempdir().unwrap();
        let custom = Duration::from_secs(120);
        let engine = EvalEngine::with_timeout(dir.path(), custom);
        assert_eq!(engine.timeout, custom);
    }

    #[test]
    fn detect_runner_setup_py() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("setup.py"), "from setuptools import setup\n").unwrap();
        assert_eq!(EvalEngine::detect_runner(dir.path()), TestRunner::Pytest);
    }

    #[test]
    fn dry_run_for_npm() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("package.json"), "{}").unwrap();
        let engine = EvalEngine::new(dir.path());
        assert_eq!(engine.dry_run(), "npm test");
    }

    #[test]
    fn dry_run_for_pytest() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("pyproject.toml"), "[build-system]\n").unwrap();
        let engine = EvalEngine::new(dir.path());
        assert_eq!(engine.dry_run(), "pytest");
    }

    #[test]
    fn dry_run_for_make() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Makefile"), "test:\n\techo ok\n").unwrap();
        let engine = EvalEngine::new(dir.path());
        assert_eq!(engine.dry_run(), "make test");
    }

    // -----------------------------------------------------------------------
    // Scoped argument tests
    // -----------------------------------------------------------------------

    use camino::Utf8PathBuf;

    #[test]
    fn scoped_args_cargo_falls_back_without_guessing_test_names() {
        for paths in [
            vec![],
            vec!["src/lib.rs"],
            vec!["src/main.rs"],
            vec!["src/cli/mod.rs"],
            vec!["src/first.rs", "src/second.rs"],
            vec!["tests/integration.rs"],
        ] {
            let paths: Vec<_> = paths.into_iter().map(Utf8PathBuf::from).collect();
            assert_eq!(EvalEngine::scoped_args(&TestRunner::Cargo, &paths), None);
        }
    }

    #[test]
    fn scoped_args_npm_returns_none() {
        let paths = vec![Utf8PathBuf::from("src/index.ts")];
        let args = EvalEngine::scoped_args(&TestRunner::Npm, &paths);
        assert!(args.is_none());
    }

    #[test]
    fn scoped_args_pytest_returns_paths() {
        let paths = vec![Utf8PathBuf::from("tests/test_foo.py")];
        let args = EvalEngine::scoped_args(&TestRunner::Pytest, &paths);
        assert_eq!(args, Some(vec!["tests/test_foo.py".to_string()]));
    }
}
