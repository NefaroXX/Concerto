use std::env;
use std::path::PathBuf;
use std::process::exit;

use concerto_config::AppConfig;
use concerto_core::error::EvalError;
use concerto_core::policy::SimplePolicyEngine;
use concerto_core::traits::approval::ApprovalDecision;
use concerto_core::traits::memory::{MemoryStore, NullMemoryStore};
use concerto_core::types::{AgentTask, BenchmarkReport, ToolRegistry};
use concerto_core::CancellationToken;
use concerto_eval::runner::{AgentFactory, AgentRunResult};
use concerto_eval::EvalEngine;
use concerto_eval::EvalRunner;
use concerto_orchestrator::agent_loop::AgentLoop;
use concerto_orchestrator::prompts::PromptBuilder;
use concerto_providers::factory::ProviderFactory;
use concerto_tools::filesystem::FilesystemTool;
use concerto_tools::shell::ShellTool;
use std::sync::Arc;

#[tokio::main]
async fn main() {
    // Parse command line arguments
    let mut suite_path = PathBuf::from("benchmark_tasks/standard");
    let mut baseline_path: Option<PathBuf> = None;
    let mut config_path: Option<PathBuf> = None;
    let mut category: Option<String> = None;

    for arg in env::args().skip(1) {
        if let Some(val) = arg.strip_prefix("--suite=") {
            suite_path = PathBuf::from(val);
        } else if let Some(val) = arg.strip_prefix("--baseline=") {
            baseline_path = Some(PathBuf::from(val));
        } else if let Some(val) = arg.strip_prefix("--config=") {
            config_path = Some(PathBuf::from(val));
        } else if let Some(val) = arg.strip_prefix("--category=") {
            category = Some(val.to_string());
        }
    }

    // If --category is provided and --suite is not explicitly overridden, adjust suite_path
    if let Some(ref cat) = category {
        if suite_path.to_string_lossy() == "benchmark_tasks/standard" {
            suite_path = PathBuf::from(format!("benchmark_tasks/{}", cat));
        }
    }

    // Create EvalRunner
    let mut runner = EvalRunner::new(&suite_path);
    if let Some(cat) = category {
        runner = runner.with_category(cat);
    }

    // If config is provided, build agent factory
    if let Some(config_path) = config_path {
        // Load config
        let content = match std::fs::read_to_string(&config_path) {
            Ok(content) => content,
            Err(e) => {
                eprintln!("failed to read config {}: {e}", config_path.display());
                exit(1);
            }
        };
        let config: AppConfig = match toml::from_str(&content) {
            Ok(config) => config,
            Err(e) => {
                eprintln!("failed to parse config {}: {e}", config_path.display());
                exit(1);
            }
        };

        // Clone config fields that the factory closure needs (it's Fn, so it can't move them out).
        let provider_config = config.primary_provider_config.clone();

        // Build agent factory
        let agent_factory: AgentFactory =
            Arc::new(move |description: String, project_dir: PathBuf| {
                let provider_config = provider_config.clone();
                let future = async move {
                    // Create provider from config
                    let creds = concerto_config::CredentialStore::new();
                    let provider = match provider_config.as_ref() {
                        Some(pc) => ProviderFactory::build(pc, &creds)
                            .map_err(|e| format!("failed to build provider: {e}"))?,
                        None => {
                            return Ok(AgentRunResult {
                                completed: false,
                                files_modified: Vec::new(),
                                tool_call_count: 0,
                                final_message: "No primary_provider_config in config file"
                                    .to_string(),
                                total_tokens: 0,
                                total_cost_usd: 0.0,
                                interventions: 0,
                            });
                        }
                    };

                    // Create tool registry
                    let mut registry = ToolRegistry::default();
                    let project_dir_utf8 = camino::Utf8PathBuf::from_path_buf(project_dir.clone())
                        .map_err(|_| {
                            format!("project_dir must be valid UTF-8: {}", project_dir.display())
                        })?;
                    registry.register(Box::new(FilesystemTool::new(project_dir_utf8)));
                    registry.register(Box::new(ShellTool::new()));
                    let registry = Arc::new(registry);

                    // Create policy engine with permissive rules (expert mode)
                    // Empty rules means expert mode - all tools auto-approved
                    let policy =
                        Arc::new(SimplePolicyEngine::new(Vec::new(), Arc::new(NullAuditLog {})));

                    // Create tool executor with auto-approval
                    let approval_sink = Arc::new(AllowAllApprovalSink {});
                    let tool_executor = Arc::new(
                        concerto_core::executor::ToolExecutor::new(
                            registry.clone(),
                            policy.clone(),
                        )
                        .with_approval_sink(approval_sink.clone()),
                    );

                    // Create eval engine
                    let eval_engine = EvalEngine::new(&project_dir);

                    // Create prompt builder with the canonical build system
                    // prompt (ADR-55 Phase 1e: the Build prompt constant in
                    // concerto-core is the single source of truth).
                    let prompt_builder =
                        PromptBuilder::new(concerto_core::types::SYSTEM_PROMPT_BUILD);

                    // Create memory store - use NullMemoryStore for eval (no persistent memory needed)
                    let memory: Arc<dyn MemoryStore> = Arc::new(NullMemoryStore {});

                    // Create undo manager (not used in eval, but required by AgentLoop)
                    let undo_manager = Arc::new(std::sync::Mutex::new(
                        concerto_tools::undo::UndoManager::new(project_dir.as_path()),
                    ));

                    // Create agent loop
                    let mut agent_loop = AgentLoop::with_project_root(
                        concerto_core::event::EventBus::new(256),
                        approval_sink,
                        provider,
                        tool_executor,
                        memory,
                        undo_manager,
                        eval_engine,
                        prompt_builder,
                        10,    // max_iterations
                        false, // fast mode
                        project_dir,
                        None, // overflow_strategy
                        None, // budget_allocator
                    );

                    // Create cancellation token
                    let cancel = CancellationToken::new();

                    // Run the agent
                    let task = AgentTask::new(concerto_core::ids::Ulid::new(), description);
                    match agent_loop.run(task, cancel).await {
                        Ok(output) => {
                            let final_message = output.final_message;
                            let total_tokens = final_message.len() as u64; // Approximation
                            Ok(AgentRunResult {
                                completed: true,
                                files_modified: output
                                    .files_modified
                                    .into_iter()
                                    .map(|p| p.to_string())
                                    .collect(),
                                tool_call_count: output.tool_call_count,
                                final_message,
                                total_tokens,
                                total_cost_usd: 0.0, // Not tracked in eval
                                interventions: 0,    // Not tracked in eval
                            })
                        }
                        Err(_) => Ok(AgentRunResult {
                            completed: false,
                            files_modified: Vec::new(),
                            tool_call_count: 0,
                            final_message: "Agent failed to complete".to_string(),
                            total_tokens: 0,
                            total_cost_usd: 0.0,
                            interventions: 0,
                        }),
                    }
                };
                Box::pin(future)
            });

        // Attach agent factory to runner
        runner = runner.with_agent_factory(agent_factory);
    } else {
        // No config provided - fallback to test-suite-only mode
        eprintln!("No --config provided. Running in test-suite-only fallback mode. Use --config=PATH to enable agent-driven evaluation.");
    }

    // Run the evaluation suite
    let report = match runner.run_suite().await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Eval run failed: {}", e);
            exit(1);
        }
    };

    // If baseline provided, compare with 5% threshold.
    if let Some(baseline_file) = baseline_path {
        match compare_with_baseline(&report, &baseline_file) {
            Ok(()) => {
                eprintln!("No regression detected vs baseline.");
            }
            Err(e) => {
                eprintln!("REGRESSION DETECTED: {}", e);
                // Print report JSON for CI visibility.
                println!("{}", serde_json::to_string_pretty(&report).unwrap_or_default());
                exit(1);
            }
        }
    }

    // Output report JSON.
    println!("{}", serde_json::to_string_pretty(&report).unwrap_or_default());
}

/// Compare current report against a baseline, returning Err if any metric
/// regresses by more than 5%.
fn compare_with_baseline(
    current: &BenchmarkReport,
    baseline_path: &PathBuf,
) -> Result<(), EvalError> {
    let data = std::fs::read_to_string(baseline_path)
        .map_err(|e| EvalError::HarnessSetupFailed(e.to_string()))?;
    let baseline: BenchmarkReport =
        serde_json::from_str(&data).map_err(|e| EvalError::HarnessSetupFailed(e.to_string()))?;

    let threshold = 0.05; // 5%
    let mut regressions: Vec<String> = Vec::new();

    // Track the worst regression for structured error fields.
    let mut worst_delta_pct: f64 = 0.0;
    let mut worst_baseline: f64 = 0.0;
    let mut worst_current: f64 = 0.0;

    // PassRate: lower is bad (degradation if current < baseline * 0.95).
    if baseline.pass_rate > 0.0 {
        let delta = (baseline.pass_rate - current.pass_rate) / baseline.pass_rate;
        if delta > threshold {
            regressions.push(format!(
                "PassRate: degraded by {:.1}% ({:.4} -> {:.4})",
                delta * 100.0,
                baseline.pass_rate,
                current.pass_rate
            ));
            if delta > worst_delta_pct {
                worst_delta_pct = delta;
                worst_baseline = baseline.pass_rate;
                worst_current = current.pass_rate;
            }
        }
    }

    // Latency: higher is bad.
    if baseline.avg_latency_ms > 0 {
        let delta = (current.avg_latency_ms as f64 - baseline.avg_latency_ms as f64)
            / baseline.avg_latency_ms as f64;
        if delta > threshold {
            regressions.push(format!(
                "AvgLatencyMs: degraded by {:.1}% ({} -> {})",
                delta * 100.0,
                baseline.avg_latency_ms,
                current.avg_latency_ms
            ));
            if delta > worst_delta_pct {
                worst_delta_pct = delta;
                worst_baseline = baseline.avg_latency_ms as f64;
                worst_current = current.avg_latency_ms as f64;
            }
        }
    }

    // Cost: higher is bad.
    if baseline.avg_cost_usd > 0.0 {
        let delta = (current.avg_cost_usd - baseline.avg_cost_usd) / baseline.avg_cost_usd;
        if delta > threshold {
            regressions.push(format!(
                "AvgCostUsd: degraded by {:.1}% ({:.4} -> {:.4})",
                delta * 100.0,
                baseline.avg_cost_usd,
                current.avg_cost_usd
            ));
            if delta > worst_delta_pct {
                worst_delta_pct = delta;
                worst_baseline = baseline.avg_cost_usd;
                worst_current = current.avg_cost_usd;
            }
        }
    }

    // Tokens: higher is bad.
    if baseline.avg_tokens > 0 {
        let delta =
            (current.avg_tokens as f64 - baseline.avg_tokens as f64) / baseline.avg_tokens as f64;
        if delta > threshold {
            regressions.push(format!(
                "AvgTokens: degraded by {:.1}% ({} -> {})",
                delta * 100.0,
                baseline.avg_tokens,
                current.avg_tokens
            ));
            if delta > worst_delta_pct {
                worst_delta_pct = delta;
                worst_baseline = baseline.avg_tokens as f64;
                worst_current = current.avg_tokens as f64;
            }
        }
    }

    // Interventions: higher is bad.
    if baseline.avg_interventions > 0.0 {
        let delta = (current.avg_interventions as f64 - baseline.avg_interventions as f64)
            / baseline.avg_interventions as f64;
        if delta > threshold {
            regressions.push(format!(
                "AvgInterventions: degraded by {:.1}% ({:.2} -> {:.2})",
                delta * 100.0,
                baseline.avg_interventions,
                current.avg_interventions
            ));
            if delta > worst_delta_pct {
                worst_delta_pct = delta;
                worst_baseline = baseline.avg_interventions as f64;
                worst_current = current.avg_interventions as f64;
            }
        }
    }

    if regressions.is_empty() {
        Ok(())
    } else {
        Err(EvalError::RegressionDetected {
            metric: regressions.join(", "),
            delta_pct: worst_delta_pct * 100.0,
            baseline: worst_baseline,
            current: worst_current,
        })
    }
}

// NullAuditLog implements AuditLog trait for eval to avoid sqlite dependency
struct NullAuditLog {}

#[async_trait::async_trait]
impl concerto_core::traits::policy::AuditLog for NullAuditLog {
    async fn record(
        &self,
        _entry: concerto_core::traits::policy::AuditEntry,
        _cancel: concerto_core::CancellationToken,
    ) -> Result<(), concerto_core::error::PolicyError> {
        // No-op for eval - just log to tracing if needed
        Ok(())
    }
}

// AllowAllApprovalSink implements ApprovalSink to auto-approve all tools
struct AllowAllApprovalSink {}

#[async_trait::async_trait]
impl concerto_core::traits::approval::ApprovalSink for AllowAllApprovalSink {
    async fn request_approval(
        &self,
        _action: &concerto_core::types::PolicyAction<'_>,
        _cancel: concerto_core::CancellationToken,
    ) -> ApprovalDecision {
        ApprovalDecision::Approve
    }

    async fn approve_all_for_session(
        &self,
        _session_id: concerto_core::ids::Ulid,
        _cancel: concerto_core::CancellationToken,
    ) {
    }

    async fn request_ack(&self, _message: &str, _cancel: concerto_core::CancellationToken) -> bool {
        true
    }
}
