//! Coverage collection shares discovery, execution and result validation.

use crate::{test_output, EvalEngine, EvalError};
use concerto_core::types::{CoverageInfo, CoverageTool, EvalResult, TestRunner};
use concerto_core::{CancellationToken, ToolError};
use concerto_tools::process::{ProcessHandle, ProcessOutput};
use concerto_tools::shell_backend::ShellProfileFactory;
use std::path::Path;
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;
use tokio::process::Command;

const REPORT_LIMIT: u64 = 16 * 1024 * 1024;
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

impl EvalEngine {
    /// Synchronous ambient discovery for callers outside an async run.
    /// Each version probe is bounded; coverage runs use profile-aware async
    /// discovery instead, with the run's cancellation token and deadline.
    pub fn detect_coverage_tool() -> Option<CoverageTool> {
        for (name, tool) in coverage_tools() {
            let Ok(mut child) = std::process::Command::new("cargo")
                .args([name, "--version"])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
            else {
                continue;
            };
            let start = Instant::now();
            loop {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        if status.success() {
                            return Some(tool);
                        }
                        break;
                    }
                    Ok(None) if start.elapsed() < PROBE_TIMEOUT => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    _ => {
                        let _ = child.kill();
                        let _ = child.wait();
                        break;
                    }
                }
            }
        }
        None
    }

    /// Parse `cargo llvm-cov --json` output.
    pub(super) fn parse_llvm_cov_json(raw: &str) -> Option<concerto_core::types::CoverageInfo> {
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
            count: u64,
            covered: u64,
            percent: f64,
        }
        impl LlvmCovMetric {
            fn valid(&self) -> bool {
                self.covered <= self.count && (0.0..=100.0).contains(&self.percent)
            }
        }

        let report: LlvmCovReport = serde_json::from_str(raw).ok()?;
        // A single export describes this run. Never silently select one of
        // several datasets and present it as whole-project coverage.
        if report.data.len() != 1 {
            return None;
        }
        let totals = report.data.into_iter().next()?.totals;
        if !totals.lines.valid()
            || totals.functions.as_ref().is_some_and(|metric| !metric.valid())
            || totals.branches.as_ref().is_some_and(|metric| !metric.valid())
        {
            return None;
        }
        Some(concerto_core::types::CoverageInfo {
            tool: concerto_core::types::CoverageTool::LlvmCov,
            line_percent: totals.lines.percent,
            function_percent: totals.functions.map(|m| m.percent),
            branch_percent: totals.branches.map(|m| m.percent),
            raw_tail: raw.chars().rev().take(1000).collect::<String>().chars().rev().collect(),
        })
    }

    /// Parse `cargo tarpaulin --out Json` output.
    pub(super) fn parse_tarpaulin_json(raw: &str) -> Option<concerto_core::types::CoverageInfo> {
        // Released Tarpaulin writes aggregate percentages and counts to
        // tarpaulin-report.json. Missing measurements are not measured zero.
        #[derive(serde::Deserialize)]
        struct TarpaulinReport {
            coverage: f64,
            covered: u64,
            coverable: u64,
        }
        let report: TarpaulinReport = serde_json::from_str(raw).ok()?;
        if !(0.0..=100.0).contains(&report.coverage) || report.covered > report.coverable {
            return None;
        }
        let line_percent = report.coverage;
        Some(concerto_core::types::CoverageInfo {
            tool: concerto_core::types::CoverageTool::Tarpaulin,
            line_percent,
            function_percent: None,
            branch_percent: None,
            raw_tail: raw.chars().rev().take(1000).collect::<String>().chars().rev().collect(),
        })
    }

    /// Collect coverage with executed, passing tests. Missing, malformed or
    /// incomplete evidence is an error, never a measured zero or silent None.
    pub async fn run_coverage(&self, cancel: CancellationToken) -> Result<CoverageInfo, EvalError> {
        let result = self.run_with_coverage(cancel).await?;
        if !result.passed {
            return Err(EvalError::TestRunnerFailed(result.output_tail));
        }
        result.coverage.ok_or_else(|| {
            EvalError::HarnessSetupFailed("coverage run returned no measurement".into())
        })
    }

    /// Run Cargo tests and collect a fresh report in a single instrumented pass.
    pub async fn run_with_coverage(
        &self,
        cancel: CancellationToken,
    ) -> Result<EvalResult, EvalError> {
        // Never create files, probe tools or spawn outside the configured gate.
        if self.process_executor.is_some() {
            return Err(EvalError::HarnessSetupFailed("native coverage execution is not implemented; run coverage through the governed shell tool".into()));
        }
        if cancel.is_cancelled() {
            return Err(EvalError::Cancelled);
        }
        if Self::detect_runner(&self.project_dir) != TestRunner::Cargo {
            return Err(EvalError::HarnessUnavailable("coverage requires a Cargo project".into()));
        }
        let start = Instant::now();
        let deadline = tokio::time::Instant::now() + self.timeout;
        let (name, tool) = self.discover_coverage(deadline, &cancel).await?;
        let (directory, report, args) = self.coverage_report(name)?;
        let output = self
            .coverage_command(
                &args.iter().map(String::as_str).collect::<Vec<_>>(),
                deadline,
                &cancel,
            )
            .await?;
        let mut result = test_output::completed_result(
            TestRunner::Cargo,
            output.exit_code,
            start.elapsed().as_millis() as u64,
            &output.stdout,
            &output.stderr,
        );
        // Failing commands often leave partial or absent reports. Preserve
        // the command failure and never attach a potentially stale metric.
        if output.exit_code == 0 {
            let raw = self.read_coverage_report(&report, deadline, &cancel).await?;
            result.coverage = Some(
                match tool {
                    CoverageTool::LlvmCov => Self::parse_llvm_cov_json(&raw),
                    CoverageTool::Tarpaulin => Self::parse_tarpaulin_json(&raw),
                    _ => None,
                }
                .ok_or_else(|| {
                    EvalError::HarnessSetupFailed(format!("invalid {name} coverage report"))
                })?,
            );
        }
        // TempDir also cleans up on every earlier error/cancellation path.
        directory.close()?;
        result.duration_ms = start.elapsed().as_millis() as u64;
        Ok(result)
    }

    /// Paths are not a safe test-dependency map. Use one unfiltered coverage
    /// run for scoped requests too. The selected tool controls supported test
    /// types (for example, llvm-cov omits doctests on stable Rust).
    pub async fn run_scoped_with_coverage(
        &self,
        _changed_paths: &[camino::Utf8PathBuf],
        cancel: CancellationToken,
    ) -> Result<EvalResult, EvalError> {
        self.run_with_coverage(cancel).await
    }

    async fn discover_coverage(
        &self,
        deadline: tokio::time::Instant,
        cancel: &CancellationToken,
    ) -> Result<(&'static str, CoverageTool), EvalError> {
        for (name, tool) in coverage_tools() {
            let probe_deadline = deadline.min(tokio::time::Instant::now() + PROBE_TIMEOUT);
            match self.coverage_command(&[name, "--version"], probe_deadline, cancel).await {
                Ok(output) if output.exit_code == 0 => return Ok((name, tool)),
                Ok(_) => continue,
                Err(EvalError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        Err(EvalError::HarnessUnavailable(
            "no coverage tool found; install cargo-llvm-cov or cargo-tarpaulin".into(),
        ))
    }

    async fn coverage_command(
        &self,
        args: &[&str],
        deadline: tokio::time::Instant,
        cancel: &CancellationToken,
    ) -> Result<ProcessOutput, EvalError> {
        let mut command = if let Some(profile) = &self.shell_profile {
            let backend = ShellProfileFactory::backend_for(profile);
            backend.check_available(profile).map_err(|error| {
                EvalError::HarnessSetupFailed(format!(
                    "build shell profile is unavailable: {error}"
                ))
            })?;
            // Every argument is a fixed flag or a generated ASCII relative
            // report path. Project paths (including spaces) travel via cwd.
            let full = format!("cargo {}", args.join(" "));
            let mut command = Command::new(backend.resolved_program(profile));
            command.args(backend.command_args(profile, &full));
            let base = std::env::vars().collect();
            command.envs(backend.effective_env(profile, &base));
            command
        } else {
            let mut command = Command::new("cargo");
            command.args(args);
            command
        };
        command.current_dir(&self.project_dir);
        let remaining = deadline
            .checked_duration_since(tokio::time::Instant::now())
            .filter(|remaining| !remaining.is_zero())
            .ok_or(EvalError::Timeout(self.timeout))?;
        ProcessHandle::run_checked(command, remaining, cancel.clone()).await.map_err(|error| {
            match error {
                ToolError::Cancelled => EvalError::Cancelled,
                ToolError::Timeout { .. } => EvalError::Timeout(remaining),
                ToolError::Io(error) => EvalError::Io(error),
                other => EvalError::TestRunnerFailed(other.to_string()),
            }
        })
    }

    fn coverage_report(
        &self,
        tool: &str,
    ) -> Result<(tempfile::TempDir, std::path::PathBuf, Vec<String>), EvalError> {
        let target = self.project_dir.join("target");
        std::fs::create_dir_all(&target)?;
        let dir = tempfile::Builder::new().prefix("concerto-coverage-").tempdir_in(target)?;
        let name = dir.path().file_name().and_then(|name| name.to_str()).ok_or_else(|| {
            EvalError::HarnessSetupFailed("invalid coverage directory name".into())
        })?;
        if !name.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-') {
            return Err(EvalError::HarnessSetupFailed("unsafe coverage directory name".into()));
        }
        let relative = format!("target/{name}");
        let (file, args) = match tool {
            "llvm-cov" => (
                "llvm-cov.json",
                vec![
                    tool.to_string(),
                    "--json".into(),
                    "--summary-only".into(),
                    "--output-path".into(),
                    format!("{relative}/llvm-cov.json"),
                ],
            ),
            "tarpaulin" => (
                "tarpaulin-report.json",
                vec![
                    tool.to_string(),
                    "--out".into(),
                    "Json".into(),
                    "--output-dir".into(),
                    relative,
                ],
            ),
            _ => return Err(EvalError::HarnessSetupFailed("unsupported coverage tool".into())),
        };
        let report = dir.path().join(file);
        Ok((dir, report, args))
    }

    async fn read_coverage_report(
        &self,
        report: &Path,
        deadline: tokio::time::Instant,
        cancel: &CancellationToken,
    ) -> Result<String, EvalError> {
        let read = async {
            if !tokio::fs::symlink_metadata(report).await?.is_file() {
                return Err(EvalError::HarnessSetupFailed(
                    "coverage report is not a regular file".into(),
                ));
            }
            let file = tokio::fs::File::open(report).await?;
            let mut raw = String::new();
            file.take(REPORT_LIMIT + 1).read_to_string(&mut raw).await?;
            if raw.len() as u64 > REPORT_LIMIT {
                return Err(EvalError::HarnessSetupFailed(
                    "coverage report exceeds 16 MiB limit".into(),
                ));
            }
            Ok(raw)
        };
        tokio::select! {
            biased;
            () = cancel.cancelled() => Err(EvalError::Cancelled),
            result = tokio::time::timeout_at(deadline, read) => {
                result.map_err(|_| EvalError::Timeout(self.timeout))?
            }
        }
    }
}

fn coverage_tools() -> [(&'static str, CoverageTool); 2] {
    [("llvm-cov", CoverageTool::LlvmCov), ("tarpaulin", CoverageTool::Tarpaulin)]
}
