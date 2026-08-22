// Orchestrates execution of a suite of evaluation benchmark tasks.

use std::fs;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;

use camino::Utf8PathBuf;

use crate::EvalEngine;
use concerto_core::error::EvalError;
use concerto_core::types::{BenchmarkReport, BenchmarkResult, EvalTask};
use concerto_core::CancellationToken;

/// The result of running a single agent task.
pub struct AgentRunResult {
    pub completed: bool,
    pub files_modified: Vec<String>,
    pub tool_call_count: u32,
    pub final_message: String,
    pub total_tokens: u64,
    pub total_cost_usd: f64,
    pub interventions: u32,
}

/// Factory function that creates an agent runner for a given task.
///
/// Takes (task_description, project_dir) and returns a future that produces
/// an `AgentRunResult`. This avoids the circular dependency between eval and
/// orchestrator — the concrete agent construction lives in the caller.
pub type AgentFactory = Arc<
    dyn Fn(String, PathBuf) -> Pin<Box<dyn Future<Output = Result<AgentRunResult, String>> + Send>>
        + Send
        + Sync,
>;

/// Helper function to recursively copy a directory.
fn copy_dir(src: &Path, dst: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        // Never copy a nested `target/` directory: a snapshot that points at a
        // previously built project would drag in a huge, unnecessary build dir.
        if entry.file_name() == "target" {
            continue;
        }
        let path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&path, &dst_path)?;
        } else {
            fs::copy(&path, &dst_path)?;
        }
    }
    Ok(())
}

/// On-disk scratch root for task snapshots.
///
/// Uses the crate's own `target/eval-scratch` directory (gitignored via
/// `/crates/*/target`) instead of the OS temp dir, which is often a RAM-backed
/// tmpfs (`/tmp`) — copying snapshots and running nested cargo builds there
/// can exhaust memory on machines where `/tmp` is not on disk.
fn scratch_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target").join("eval-scratch")
}

/// If the copied project has a `Cargo.toml` without a `[workspace]` table,
/// append an empty one so that nested `cargo` invocations do not walk up to
/// the parent workspace and fail with "current package believes it's in a
/// workspace when it's not".
fn isolate_workspace(project_dir: &Path) -> std::io::Result<()> {
    let manifest = project_dir.join("Cargo.toml");
    if !manifest.exists() {
        return Ok(());
    }
    let content = fs::read_to_string(&manifest)?;
    if !content.contains("[workspace]") {
        let mut updated = content;
        if !updated.ends_with('\n') {
            updated.push('\n');
        }
        updated.push_str("\n[workspace]\n");
        fs::write(&manifest, updated)?;
    }
    Ok(())
}

/// Runner for a benchmark suite.
pub struct EvalRunner {
    /// Directory containing benchmark task sub‑directories.
    suite_dir: PathBuf,
    /// Timeout per task in seconds.
    task_timeout_secs: u64,
    /// Factory that creates agent runners for each task.
    /// When `None`, the runner falls back to the old behavior (just run tests).
    agent_factory: Option<AgentFactory>,
    /// Optional category filter.
    category: Option<String>,
}

impl EvalRunner {
    /// Create a new runner for the given suite directory.
    pub fn new(suite_dir: impl Into<PathBuf>) -> Self {
        Self {
            suite_dir: suite_dir.into(),
            task_timeout_secs: 300,
            agent_factory: None,
            category: None,
        }
    }

    /// Override the per‑task timeout.
    pub fn with_timeout(mut self, secs: u64) -> Self {
        self.task_timeout_secs = secs;
        self
    }

    /// Attach an agent factory for real agent-driven evaluation.
    pub fn with_agent_factory(mut self, factory: AgentFactory) -> Self {
        self.agent_factory = Some(factory);
        self
    }

    /// Set the category filter.
    pub fn with_category(mut self, category: impl Into<String>) -> Self {
        self.category = Some(category.into());
        self
    }

    /// Get the current category filter.
    pub fn category(&self) -> Option<&str> {
        self.category.as_deref()
    }

    /// Load all benchmark tasks from the suite directory.
    fn load_tasks(&self) -> Result<Vec<EvalTask>, EvalError> {
        let mut tasks = Vec::new();
        for entry in fs::read_dir(&self.suite_dir)
            .map_err(|e| EvalError::HarnessSetupFailed(e.to_string()))?
        {
            let entry = entry.map_err(|e| EvalError::HarnessSetupFailed(e.to_string()))?;
            if !entry
                .file_type()
                .map_err(|e| EvalError::HarnessSetupFailed(e.to_string()))?
                .is_dir()
            {
                continue;
            }
            let task_json = entry.path().join("task.json");
            if !task_json.exists() {
                continue;
            }
            let data = fs::read_to_string(&task_json)
                .map_err(|e| EvalError::HarnessSetupFailed(e.to_string()))?;
            let mut task: EvalTask = serde_json::from_str(&data)
                .map_err(|e| EvalError::HarnessSetupFailed(e.to_string()))?;
            // Resolve relative snapshot paths (the committed task.json files use
            // "./", which must mean "this task's own directory") so a task never
            // points at the process CWD (e.g. the whole repo) accidentally.
            if task.project_snapshot_path.is_relative() {
                let resolved = entry.path().join(task.project_snapshot_path.as_std_path());
                task.project_snapshot_path = Utf8PathBuf::from_path_buf(resolved).map_err(|p| {
                    EvalError::HarnessSetupFailed(format!(
                        "task snapshot path is not valid UTF-8: {}",
                        p.display()
                    ))
                })?;
            }
            tasks.push(task);
        }
        Ok(tasks)
    }

    /// Execute a single task and produce a benchmark result.
    async fn run_task(&self, task: &EvalTask) -> Result<BenchmarkResult, EvalError> {
        let start = Instant::now();

        // Scratch copy of the task snapshot, created on disk (never in the
        // RAM-backed OS temp dir) so nested cargo builds stay off tmpfs.
        fs::create_dir_all(scratch_root())
            .map_err(|e| EvalError::HarnessSetupFailed(e.to_string()))?;
        let temp_dir = tempfile::tempdir_in(scratch_root())
            .map_err(|e| EvalError::HarnessSetupFailed(e.to_string()))?;
        let temp_path = temp_dir.path().to_path_buf();

        copy_dir(task.project_snapshot_path.as_std_path(), &temp_path)
            .map_err(|e| EvalError::HarnessSetupFailed(e.to_string()))?;

        // Prevent nested cargo from discovering the parent workspace.
        isolate_workspace(&temp_path).map_err(|e| EvalError::HarnessSetupFailed(e.to_string()))?;

        if let Some(ref factory) = self.agent_factory {
            // Agent-driven mode: run agent on the scratch copy, verify results.

            // Run the agent via the factory.
            let agent_result = tokio::time::timeout(
                std::time::Duration::from_secs(self.task_timeout_secs),
                (factory)(task.description.clone(), temp_path.clone()),
            )
            .await
            .map_err(|_| EvalError::TaskTimeout { timeout_ms: self.task_timeout_secs * 1000 })?
            .map_err(|e| EvalError::HarnessSetupFailed(e.to_string()))?;

            // Verify: run the project's test suite after the agent ran.
            let engine = EvalEngine::new(&temp_path);
            let cancel = CancellationToken::new();
            let eval_res = engine
                .run(cancel)
                .await
                .map_err(|e| EvalError::HarnessSetupFailed(e.to_string()))?;

            // Compute files_correctly_modified from expected_outcomes.
            let files_correctly_modified = if task.expected_outcomes.is_empty() {
                0.0
            } else {
                let mut correct = 0u32;
                for outcome in &task.expected_outcomes {
                    let mut all_match = true;
                    for expected_file in &outcome.files_changed {
                        let file_path = temp_path.join(expected_file);
                        if !file_path.exists() {
                            all_match = false;
                            break;
                        }
                        // Check min_patterns if specified.
                        if !outcome.min_patterns.is_empty() {
                            if let Ok(content) = fs::read_to_string(&file_path) {
                                for pattern in &outcome.min_patterns {
                                    if !content.contains(pattern.as_str()) {
                                        all_match = false;
                                        break;
                                    }
                                }
                            }
                        }
                    }
                    if all_match {
                        correct += 1;
                    }
                }
                correct as f64 / task.expected_outcomes.len() as f64
            };

            let time_ms = start.elapsed().as_millis() as u64;

            Ok(BenchmarkResult {
                task_id: task.id,
                task_description: task.description.clone(),
                completed: agent_result.completed && eval_res.passed,
                interventions: agent_result.interventions,
                total_tokens: agent_result.total_tokens,
                total_cost_usd: agent_result.total_cost_usd,
                time_to_complete_ms: time_ms,
                tests_passing_after: eval_res.passed,
                files_correctly_modified: files_correctly_modified as f32,
                agent_outcome: agent_result.final_message,
            })
        } else {
            // Fallback mode: just run the (scratch-copied) project's test suite.
            let engine = EvalEngine::new(&temp_path);
            let cancel = CancellationToken::new();
            let eval_res = tokio::time::timeout(
                std::time::Duration::from_secs(self.task_timeout_secs),
                engine.run(cancel),
            )
            .await
            .map_err(|_| EvalError::TaskTimeout { timeout_ms: self.task_timeout_secs * 1000 })?
            .map_err(|e| EvalError::HarnessSetupFailed(e.to_string()))?;

            let time_ms = start.elapsed().as_millis() as u64;

            Ok(BenchmarkResult {
                task_id: task.id,
                task_description: task.description.clone(),
                completed: eval_res.passed,
                interventions: 0,
                total_tokens: 0,
                total_cost_usd: 0.0,
                time_to_complete_ms: time_ms,
                tests_passing_after: eval_res.passed,
                files_correctly_modified: if eval_res.passed { 1.0 } else { 0.0 },
                agent_outcome: "fallback: test-suite-only".into(),
            })
        }
    }

    /// Execute the suite and produce a benchmark report.
    pub async fn run_suite(&self) -> Result<BenchmarkReport, EvalError> {
        let tasks = self.load_tasks()?;
        let mut results = Vec::new();

        for task in &tasks {
            let result = self.run_task(task).await?;
            results.push(result);
        }

        // Aggregate metrics.
        let task_count = results.len();
        let pass_rate = if task_count == 0 {
            0.0
        } else {
            results.iter().filter(|r| r.tests_passing_after).count() as f64 / task_count as f64
        };
        let avg_latency_ms = if task_count == 0 {
            0
        } else {
            results.iter().map(|r| r.time_to_complete_ms).sum::<u64>() / task_count as u64
        };
        let avg_cost_usd = if task_count == 0 {
            0.0
        } else {
            results.iter().map(|r| r.total_cost_usd).sum::<f64>() / task_count as f64
        };
        let avg_tokens = if task_count == 0 {
            0
        } else {
            results.iter().map(|r| r.total_tokens).sum::<u64>() / task_count as u64
        };
        let avg_interventions = if task_count == 0 {
            0.0
        } else {
            results.iter().map(|r| r.interventions as f32).sum::<f32>() / task_count as f32
        };

        Ok(BenchmarkReport {
            task_count,
            pass_rate,
            avg_latency_ms,
            avg_cost_usd,
            avg_tokens,
            avg_interventions,
            individual_results: results,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn new_creates_runner_with_defaults() {
        let runner = EvalRunner::new("/tmp");
        assert_eq!(runner.task_timeout_secs, 300);
        assert!(runner.agent_factory.is_none());
        assert!(runner.category.is_none());
    }

    #[test]
    fn with_timeout_overrides_default() {
        let runner = EvalRunner::new("/tmp").with_timeout(600);
        assert_eq!(runner.task_timeout_secs, 600);
    }

    #[test]
    fn with_category_sets_filter() {
        let runner = EvalRunner::new("/tmp").with_category("bug_fix");
        assert_eq!(runner.category.as_deref(), Some("bug_fix"));
    }

    #[test]
    fn category_returns_none_by_default() {
        let runner = EvalRunner::new("/tmp");
        assert!(runner.category().is_none());
    }

    #[test]
    fn load_tasks_empty_directory_returns_empty_vec() {
        let dir = tempfile::tempdir().unwrap();
        // Empty directory with no subdirectories → no tasks.
        let runner = EvalRunner::new(dir.path());
        let tasks = runner.load_tasks().expect("empty dir should not error");
        assert!(tasks.is_empty());
    }

    #[test]
    fn load_tasks_skips_non_directories() {
        let dir = tempfile::tempdir().unwrap();
        // A file at the top level should be skipped (not a directory).
        std::fs::write(dir.path().join("task.json"), "{}").unwrap();
        let runner = EvalRunner::new(dir.path());
        let tasks = runner.load_tasks().expect("should skip files");
        assert!(tasks.is_empty());
    }

    #[test]
    fn empty_suite_returns_zero_tasks_report() {
        let dir = tempfile::tempdir().unwrap();
        let runner = EvalRunner::new(dir.path());
        // Run the suite with no tasks — should produce a zero-count report.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let report = rt.block_on(runner.run_suite()).expect("empty suite should not error");
        assert_eq!(report.task_count, 0);
        assert_eq!(report.pass_rate, 0.0);
        assert_eq!(report.avg_latency_ms, 0);
        assert!(report.individual_results.is_empty());
    }

    #[test]
    fn copy_dir_creates_destination() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("file.txt"), "hello").unwrap();
        copy_dir(src.path(), dst.path()).expect("copy should succeed");
        assert!(dst.path().join("file.txt").exists());
        let content = std::fs::read_to_string(dst.path().join("file.txt")).unwrap();
        assert_eq!(content, "hello");
    }

    #[test]
    fn copy_dir_with_nested_directories() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        fs::create_dir(src.path().join("sub")).unwrap();
        std::fs::write(src.path().join("sub").join("nested.txt"), "nested").unwrap();
        copy_dir(src.path(), dst.path()).expect("nested copy should succeed");
        assert!(dst.path().join("sub").join("nested.txt").exists());
        let content = std::fs::read_to_string(dst.path().join("sub").join("nested.txt")).unwrap();
        assert_eq!(content, "nested");
    }

    #[test]
    fn loads_all_tasks_from_suite() {
        let suite_dir =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("benchmark_tasks").join("standard");
        if !suite_dir.exists() {
            eprintln!("suite dir not found, skipping");
            return;
        }
        let runner = EvalRunner::new(&suite_dir);
        let tasks = runner.load_tasks().unwrap();
        assert!(!tasks.is_empty(), "should load at least one task");
    }

    #[tokio::test]
    async fn noop_agent_fails_benchmark() {
        // A no-op factory that never modifies files should produce
        // completed=false or files_correctly_modified=0.0.
        let factory: AgentFactory = Arc::new(|_desc, _dir| {
            Box::pin(async {
                Ok(AgentRunResult {
                    completed: false,
                    files_modified: vec![],
                    tool_call_count: 0,
                    final_message: "noop".into(),
                    total_tokens: 0,
                    total_cost_usd: 0.0,
                    interventions: 0,
                })
            })
        });

        let suite_dir =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("benchmark_tasks").join("standard");
        if !suite_dir.exists() {
            return;
        }
        let runner = EvalRunner::new(&suite_dir).with_agent_factory(factory).with_timeout(60);
        let report = runner.run_suite().await.unwrap();
        // At least one task should have completed=false.
        assert!(
            report.individual_results.iter().any(|r| !r.completed),
            "noop agent should produce at least one incomplete result"
        );
    }

    #[tokio::test]
    async fn fallback_mode_runs_tests() {
        let suite_dir =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("benchmark_tasks").join("standard");
        if !suite_dir.exists() {
            return;
        }
        let runner = EvalRunner::new(&suite_dir).with_timeout(60);
        let report = runner.run_suite().await.unwrap();
        // The standard suite contains multiple Rust benchmark tasks; the exact
        // count changes over time, so just verify the runner discovered and
        // reported at least one task.
        assert!(report.task_count > 0, "suite should contain at least one task");
    }
}
