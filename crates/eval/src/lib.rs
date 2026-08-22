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
use concerto_core::types::{EvalResult, TestRunner};
use concerto_core::CancellationToken;
use concerto_tools::shell_backend::ShellProfileFactory;
use thiserror::Error;
use tokio::process::Command;

pub mod categories;
pub mod runner;
pub mod scenarios;
pub use runner::EvalRunner;

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum EvalError {
    #[error("eval harness setup failed: {0}")]
    HarnessSetupFailed(String),
    #[error("test runner failed: {0}")]
    TestRunnerFailed(String),
    #[error("timed out after {0:?}")]
    Timeout(Duration),
    #[error("cancelled")]
    Cancelled,
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

// ---------------------------------------------------------------------------
// EvalEngine
// ---------------------------------------------------------------------------

pub struct EvalEngine {
    project_dir: PathBuf,
    timeout: Duration,
    shell_profile: Option<ShellProfileConfig>,
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
        }
    }

    /// Create a new eval engine with a custom timeout.
    pub fn with_timeout(project_dir: impl Into<PathBuf>, timeout: Duration) -> Self {
        Self { project_dir: project_dir.into(), timeout, shell_profile: None }
    }

    /// Run validation commands through a configured build/validation profile.
    #[must_use]
    pub fn with_shell_profile(mut self, profile: ShellProfileConfig) -> Self {
        self.shell_profile = Some(profile);
        self
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
            .current_dir(&self.project_dir)
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
            Ok(Ok(out)) => {
                let exit_code = out.status.code().unwrap_or(-1);
                let passed = out.status.success();
                let combined = String::from_utf8_lossy(&out.stdout).to_string()
                    + &String::from_utf8_lossy(&out.stderr);
                let output_tail = if combined.len() > 2000 {
                    combined[combined.len() - 2000..].to_string()
                } else {
                    combined
                };
                Ok(EvalResult {
                    runner,
                    exit_code,
                    passed,
                    duration_ms,
                    output_tail,
                    coverage: None,
                })
            }
            Ok(Err(e)) => Err(EvalError::Io(e)),
            Err(_elapsed) => Ok(EvalResult {
                runner,
                exit_code: -1,
                passed: false,
                duration_ms,
                output_tail: "timed out".into(),
                coverage: None,
            }),
        }
    }

    /// Run the test suite (full) – default entry point.
    pub async fn run(&self, cancel: CancellationToken) -> Result<EvalResult, EvalError> {
        let runner = Self::detect_runner(&self.project_dir);
        self.run_for_runner(runner, cancel).await
    }

    async fn run_for_runner(
        &self,
        runner: TestRunner,
        cancel: CancellationToken,
    ) -> Result<EvalResult, EvalError> {
        // Default args per runner
        let default_args: &[&str] = match &runner {
            TestRunner::Cargo => &["test"],
            TestRunner::Npm => &["test"],
            TestRunner::Pytest => &[],
            TestRunner::Make => &["test"],
            TestRunner::Unknown(s) => {
                return Err(EvalError::HarnessSetupFailed(format!("cannot run tests: {s}")));
            }
            _ => &[],
        };
        self.run_with_args(runner, default_args, cancel).await
    }

    /// Run tests scoped to changed paths; falls back to full suite if mapping unavailable.
    pub async fn run_scoped(
        &self,
        changed_paths: &[camino::Utf8PathBuf],
        cancel: CancellationToken,
    ) -> Result<EvalResult, EvalError> {
        let runner = Self::detect_runner(&self.project_dir);
        if let Some(arg_vec) = Self::scoped_args(&runner, changed_paths) {
            let args_ref: Vec<&str> = arg_vec.iter().map(|s| s.as_str()).collect();
            self.run_with_args(runner, &args_ref, cancel).await
        } else {
            self.run_for_runner(runner, cancel).await
        }
    }

    /// Map changed file paths to test runner arguments.
    /// Returns None if mapping is uncertain or unsupported.
    fn scoped_args(
        runner: &TestRunner,
        changed_paths: &[camino::Utf8PathBuf],
    ) -> Option<Vec<String>> {
        match runner {
            TestRunner::Cargo => {
                use std::collections::HashSet;
                let mut modules: HashSet<String> = HashSet::new();
                for path in changed_paths {
                    let path_str = path.to_string();
                    if !path_str.starts_with("src/") || !path_str.ends_with(".rs") {
                        return None;
                    }
                    // Strip "src/" prefix and ".rs" suffix
                    let mut rel = &path_str["src/".len()..path_str.len() - ".rs".len()];
                    // Handle mod.rs case
                    if rel.ends_with("/mod") {
                        rel = &rel[..rel.len() - "/mod".len()];
                    }
                    let module_path = rel.replace('/', "::");
                    modules.insert(module_path);
                }
                if modules.is_empty() {
                    return Some(vec!["test".to_string()]);
                }
                // Build args: cargo test -- <module patterns>
                let mut args = vec!["test".to_string(), "--".to_string()];
                let pattern = modules.iter().cloned().collect::<Vec<_>>().join(" ");
                args.push(pattern);
                Some(args)
            }
            TestRunner::Pytest => {
                // Return paths as-is for pytest (could be used as arguments)
                Some(changed_paths.iter().map(|p| p.to_string()).collect())
            }
            TestRunner::Npm | TestRunner::Make => None,
            TestRunner::Unknown(_) => None,
            _ => None,
        }
    }

    // -----------------------------------------------------------------------
    // Coverage tracking
    // -----------------------------------------------------------------------

    /// Detect whether a coverage tool is available on the system.
    pub fn detect_coverage_tool() -> Option<concerto_core::types::CoverageTool> {
        // Check cargo-llvm-cov first (most common in Rust projects).
        if std::process::Command::new("cargo")
            .arg("llvm-cov")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok()
        {
            return Some(concerto_core::types::CoverageTool::LlvmCov);
        }
        // Fall back to cargo-tarpaulin.
        if std::process::Command::new("cargo")
            .arg("tarpaulin")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok()
        {
            return Some(concerto_core::types::CoverageTool::Tarpaulin);
        }
        None
    }

    /// Parse `cargo llvm-cov --json` output.
    fn parse_llvm_cov_json(raw: &str) -> Option<concerto_core::types::CoverageInfo> {
        // Expected JSON structure (simplified):
        // {
        //   "data": [{
        //     "totals": {
        //       "lines": {"count": N, "covered": M, "percent": P},
        //       "functions": {"count": N, "covered": M, "percent": P},
        //       "branches": {"count": N, "covered": M, "percent": P}
        //     }
        //   }]
        // }
        #[derive(serde::Deserialize)]
        struct LlvmCovReport {
            data: Vec<LlvmCovData>,
        }
        #[derive(serde::Deserialize)]
        struct LlvmCovData {
            totals: LlvmCovTotals,
        }
        #[derive(serde::Deserialize)]
        struct LlvmCovTotals {
            lines: LlvmCovMetric,
            #[serde(default)]
            functions: Option<LlvmCovMetric>,
            #[serde(default)]
            branches: Option<LlvmCovMetric>,
        }
        #[derive(serde::Deserialize)]
        struct LlvmCovMetric {
            percent: f64,
        }

        let report: LlvmCovReport = serde_json::from_str(raw).ok()?;
        let totals = report.data.into_iter().next()?.totals;
        Some(concerto_core::types::CoverageInfo {
            tool: concerto_core::types::CoverageTool::LlvmCov,
            line_percent: totals.lines.percent,
            function_percent: totals.functions.map(|m| m.percent),
            branch_percent: totals.branches.map(|m| m.percent),
            raw_tail: raw.chars().rev().take(1000).collect::<String>().chars().rev().collect(),
        })
    }

    /// Parse `cargo tarpaulin --out Json` output.
    fn parse_tarpaulin_json(raw: &str) -> Option<concerto_core::types::CoverageInfo> {
        // Expected JSON structure:
        // {
        //   "coverage_percent": 75.0,
        //   "line_coverage": [{"covered": 1, "uncovered": 0, ...}, ...]
        // }
        #[derive(serde::Deserialize)]
        struct TarpaulinReport {
            #[serde(default)]
            coverage_percent: Option<f64>,
        }
        let report: TarpaulinReport = serde_json::from_str(raw).ok()?;
        let line_percent = report.coverage_percent.unwrap_or(0.0);
        Some(concerto_core::types::CoverageInfo {
            tool: concerto_core::types::CoverageTool::Tarpaulin,
            line_percent,
            function_percent: None,
            branch_percent: None,
            raw_tail: raw.chars().rev().take(1000).collect::<String>().chars().rev().collect(),
        })
    }

    /// Run coverage collection only (standalone), returning CoverageInfo.
    pub async fn run_coverage(
        &self,
        _cancel: CancellationToken,
    ) -> Result<concerto_core::types::CoverageInfo, EvalError> {
        let tool = Self::detect_coverage_tool().ok_or_else(|| {
            EvalError::HarnessSetupFailed(
                "no coverage tool found — install cargo-llvm-cov or cargo-tarpaulin".into(),
            )
        })?;

        let output = match tool {
            concerto_core::types::CoverageTool::LlvmCov => {
                let mut cmd = std::process::Command::new("cargo");
                cmd.args(["llvm-cov", "--json", "--output-path", "/dev/stdout"]);
                cmd.current_dir(&self.project_dir);
                cmd.stdout(std::process::Stdio::piped());
                cmd.stderr(std::process::Stdio::piped());
                let child = cmd.spawn().map_err(|e| {
                    EvalError::HarnessSetupFailed(format!("failed to spawn cargo llvm-cov: {e}"))
                })?;
                let output = child.wait_with_output().map_err(EvalError::Io)?;
                if output.status.success() {
                    let raw = String::from_utf8_lossy(&output.stdout).to_string();
                    Self::parse_llvm_cov_json(&raw).ok_or_else(|| {
                        EvalError::HarnessSetupFailed("failed to parse llvm-cov JSON output".into())
                    })?
                } else {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    return Err(EvalError::HarnessSetupFailed(format!(
                        "llvm-cov failed: {stderr}"
                    )));
                }
            }
            concerto_core::types::CoverageTool::Tarpaulin => {
                let mut cmd = std::process::Command::new("cargo");
                cmd.args(["tarpaulin", "--out", "Json", "--output-dir", "/tmp"]);
                cmd.current_dir(&self.project_dir);
                cmd.stdout(std::process::Stdio::piped());
                cmd.stderr(std::process::Stdio::piped());
                let child = cmd.spawn().map_err(|e| {
                    EvalError::HarnessSetupFailed(format!("failed to spawn cargo tarpaulin: {e}"))
                })?;
                let output = child.wait_with_output().map_err(EvalError::Io)?;
                if output.status.success() {
                    // tarpaulin writes to a file; also captures stdout
                    let stdout_raw = String::from_utf8_lossy(&output.stdout).to_string();
                    Self::parse_tarpaulin_json(&stdout_raw).ok_or_else(|| {
                        EvalError::HarnessSetupFailed(
                            "failed to parse tarpaulin JSON output".into(),
                        )
                    })?
                } else {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    return Err(EvalError::HarnessSetupFailed(format!(
                        "tarpaulin failed: {stderr}"
                    )));
                }
            }
            concerto_core::types::CoverageTool::Other(_) => {
                return Err(EvalError::HarnessSetupFailed("unsupported coverage tool".into()));
            }
            _ => {
                return Err(EvalError::HarnessSetupFailed("unsupported coverage tool".into()));
            }
        };

        Ok(output)
    }

    /// Run the test suite and collect coverage in a single pass.
    /// This runs `cargo llvm-cov` (or the detected tool) which runs tests AND instruments coverage.
    pub async fn run_with_coverage(
        &self,
        cancel: CancellationToken,
    ) -> Result<concerto_core::types::EvalResult, EvalError> {
        let start = std::time::Instant::now();
        let tool = Self::detect_coverage_tool()
            .ok_or_else(|| EvalError::HarnessSetupFailed("no coverage tool found".into()))?;

        let runner = Self::detect_runner(&self.project_dir);
        let (passed, exit_code, duration_ms, output_tail, coverage) = match &runner {
            TestRunner::Cargo => {
                match &tool {
                    concerto_core::types::CoverageTool::LlvmCov => {
                        // `cargo llvm-cov` runs tests and collects coverage in one command.
                        let mut command = if let Some(profile) = &self.shell_profile {
                            let backend = ShellProfileFactory::backend_for(profile);
                            backend.check_available(profile).map_err(|e| {
                                EvalError::HarnessSetupFailed(format!(
                                    "build shell profile '{}' is unavailable: {e}",
                                    profile.name
                                ))
                            })?;
                            let full =
                                "cargo llvm-cov --json --output-path /dev/stdout".to_string();
                            let mut cmd = Command::new(backend.resolved_program(profile));
                            cmd.args(backend.command_args(profile, &full));
                            let base = std::env::vars().collect();
                            cmd.envs(backend.effective_env(profile, &base));
                            cmd
                        } else {
                            let mut cmd = Command::new("cargo");
                            cmd.args(["llvm-cov", "--json", "--output-path", "/dev/stdout"]);
                            cmd
                        };
                        let child = command
                            .current_dir(&self.project_dir)
                            .stdout(std::process::Stdio::piped())
                            .stderr(std::process::Stdio::piped())
                            .kill_on_drop(true)
                            .spawn()?;
                        let output = tokio::select! {
                            res = tokio::time::timeout(self.timeout, child.wait_with_output()) => res,
                            _ = cancel.cancelled() => return Err(EvalError::Cancelled),
                        };

                        let actual_duration = start.elapsed().as_millis() as u64;
                        match output {
                            Ok(Ok(out)) => {
                                let passed = out.status.success();
                                let exit_code = out.status.code().unwrap_or(-1);
                                let combined = String::from_utf8_lossy(&out.stdout).to_string()
                                    + &String::from_utf8_lossy(&out.stderr);
                                let tail = if combined.len() > 2000 {
                                    combined[combined.len() - 2000..].to_string()
                                } else {
                                    combined.clone()
                                };
                                let cov = Self::parse_llvm_cov_json(&combined);
                                (passed, exit_code, actual_duration, tail, cov)
                            }
                            Ok(Err(e)) => return Err(EvalError::Io(e)),
                            Err(_) => (false, -1, actual_duration, "timed out".into(), None),
                        }
                    }
                    concerto_core::types::CoverageTool::Tarpaulin => {
                        // `cargo tarpaulin` also runs tests with coverage.
                        let mut command = if let Some(profile) = &self.shell_profile {
                            let backend = ShellProfileFactory::backend_for(profile);
                            backend.check_available(profile).map_err(|e| {
                                EvalError::HarnessSetupFailed(format!(
                                    "build shell profile '{}' is unavailable: {e}",
                                    profile.name
                                ))
                            })?;
                            let full = "cargo tarpaulin --out Json --output-dir /tmp".to_string();
                            let mut cmd = Command::new(backend.resolved_program(profile));
                            cmd.args(backend.command_args(profile, &full));
                            let base = std::env::vars().collect();
                            cmd.envs(backend.effective_env(profile, &base));
                            cmd
                        } else {
                            let mut cmd = Command::new("cargo");
                            cmd.args(["tarpaulin", "--out", "Json", "--output-dir", "/tmp"]);
                            cmd
                        };
                        let child = command
                            .current_dir(&self.project_dir)
                            .stdout(std::process::Stdio::piped())
                            .stderr(std::process::Stdio::piped())
                            .kill_on_drop(true)
                            .spawn()?;
                        let output = tokio::select! {
                            res = tokio::time::timeout(self.timeout, child.wait_with_output()) => res,
                            _ = cancel.cancelled() => return Err(EvalError::Cancelled),
                        };

                        let actual_duration = start.elapsed().as_millis() as u64;
                        match output {
                            Ok(Ok(out)) => {
                                let passed = out.status.success();
                                let exit_code = out.status.code().unwrap_or(-1);
                                let stdout_raw = String::from_utf8_lossy(&out.stdout).to_string();
                                let stderr_raw = String::from_utf8_lossy(&out.stderr);
                                let combined = stdout_raw.clone() + &stderr_raw;
                                let tail = if combined.len() > 2000 {
                                    combined[combined.len() - 2000..].to_string()
                                } else {
                                    combined.clone()
                                };
                                let cov = Self::parse_tarpaulin_json(&stdout_raw);
                                (passed, exit_code, actual_duration, tail, cov)
                            }
                            Ok(Err(e)) => return Err(EvalError::Io(e)),
                            Err(_) => (false, -1, actual_duration, "timed out".into(), None),
                        }
                    }
                    concerto_core::types::CoverageTool::Other(_) => {
                        return Err(EvalError::HarnessSetupFailed(
                            "unsupported coverage tool".into(),
                        ));
                    }
                    _ => {
                        return Err(EvalError::HarnessSetupFailed(
                            "unsupported coverage tool".into(),
                        ));
                    }
                }
            }
            // For non-Cargo runners, run tests normally, then collect coverage separately.
            _ => {
                let eval = self.run_for_runner(runner.clone(), cancel.clone()).await?;
                let cov = self.run_coverage(cancel).await.ok();
                (eval.passed, eval.exit_code, eval.duration_ms, eval.output_tail, cov)
            }
        };

        Ok(concerto_core::types::EvalResult {
            runner,
            exit_code,
            passed,
            duration_ms,
            output_tail,
            coverage,
        })
    }

    /// Run scoped tests with coverage.
    pub async fn run_scoped_with_coverage(
        &self,
        changed_paths: &[camino::Utf8PathBuf],
        cancel: CancellationToken,
    ) -> Result<concerto_core::types::EvalResult, EvalError> {
        let runner = Self::detect_runner(&self.project_dir);
        if runner != TestRunner::Cargo {
            // Scoped coverage only works with Cargo for now.
            return self.run_with_coverage(cancel).await;
        }
        // Fall back to scoped tests without coverage instrumentation for Cargo,
        // since llvm-cov doesn't easily support --package filtering like `cargo test -p`.
        // We run tests scoped, then collect full-project coverage.
        let eval = self.run_scoped(changed_paths, cancel.clone()).await?;
        let cov = self.run_coverage(cancel).await.ok();
        Ok(concerto_core::types::EvalResult {
            runner,
            exit_code: eval.exit_code,
            passed: eval.passed,
            duration_ms: eval.duration_ms,
            output_tail: eval.output_tail,
            coverage: cov,
        })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use concerto_config::ShellBackendType;

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
            .run_for_runner(TestRunner::Cargo, CancellationToken::new())
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
        let json = r#"{"coverage_percent": 82.5}"#;
        let info = EvalEngine::parse_tarpaulin_json(json).expect("should parse");
        assert_eq!(info.tool, concerto_core::types::CoverageTool::Tarpaulin);
        assert!((info.line_percent - 82.5).abs() < 0.01);
        assert!(info.function_percent.is_none());
        assert!(info.branch_percent.is_none());
    }

    #[test]
    fn parse_tarpaulin_json_malformed_returns_none() {
        assert!(EvalEngine::parse_tarpaulin_json("not json").is_none());
        assert!(EvalEngine::parse_tarpaulin_json(r#"{"coverage_percent": "abc"}"#).is_none());
    }

    #[test]
    fn parse_tarpaulin_json_missing_percent_field() {
        let json = r#"{"other_field": 42}"#;
        let info = EvalEngine::parse_tarpaulin_json(json).expect("should parse with default");
        assert!((info.line_percent - 0.0).abs() < 0.01);
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
    fn scoped_args_cargo_returns_filtered_pattern() {
        let paths = vec![Utf8PathBuf::from("src/lib.rs")];
        let args = EvalEngine::scoped_args(&TestRunner::Cargo, &paths);
        assert_eq!(args, Some(vec!["test".to_string(), "--".to_string(), "lib".to_string()]));
    }

    #[test]
    fn scoped_args_cargo_non_src_returns_none() {
        let paths = vec![Utf8PathBuf::from("tests/integration.rs")];
        let args = EvalEngine::scoped_args(&TestRunner::Cargo, &paths);
        assert!(args.is_none(), "paths outside src/ should return None");
    }

    #[test]
    fn scoped_args_cargo_mod_rs_strips_mod() {
        let paths = vec![Utf8PathBuf::from("src/cli/mod.rs")];
        let args = EvalEngine::scoped_args(&TestRunner::Cargo, &paths);
        assert_eq!(args, Some(vec!["test".to_string(), "--".to_string(), "cli".to_string()]));
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
