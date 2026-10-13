use std::env;
use std::path::PathBuf;
use std::process::exit;

use concerto_config::AppConfig;
use concerto_core::error::EvalError;
use concerto_core::policy::SimplePolicyEngine;
use concerto_core::traits::approval::ApprovalDecision;
use concerto_core::traits::memory::{MemoryStore, NullMemoryStore};
use concerto_core::traits::provider::LlmProvider;
use concerto_core::types::{AgentTask, BenchmarkReport, ToolRegistry};
use concerto_core::CancellationToken;
use concerto_eval::runner::{AgentFactory, AgentRunResult};
use concerto_eval::EvalEngine;
use concerto_eval::EvalRunner;
use concerto_orchestrator::agent_loop::AgentLoop;
use concerto_orchestrator::prompts::PromptBuilder;
use concerto_providers::factory::ProviderFactory;
use concerto_sessions::SessionStore;
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
    let mut variant: Option<String> = None;
    let mut preparation: Option<String> = None;
    let mut pricing_fingerprint: Option<String> = None;
    let agent_driven = env::args().any(|arg| arg.starts_with("--config="));

    for arg in env::args().skip(1) {
        if let Some(val) = arg.strip_prefix("--suite=") {
            suite_path = PathBuf::from(val);
        } else if let Some(val) = arg.strip_prefix("--baseline=") {
            baseline_path = Some(PathBuf::from(val));
        } else if let Some(val) = arg.strip_prefix("--config=") {
            config_path = Some(PathBuf::from(val));
        } else if let Some(val) = arg.strip_prefix("--variant=") {
            variant = Some(val.to_string());
        } else if let Some(val) = arg.strip_prefix("--cache-preparation=") {
            preparation = Some(val.to_string());
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

        // Build the config-driven agent factory: the provider is built per
        // task from the config's primary provider (no provider configured is
        // reported, never silently mocked).
        if !config.cache_pricing.is_empty() {
            pricing_fingerprint = Some(
                blake3::hash(
                    serde_json::to_string(&config.cache_pricing)
                        .expect("price table serializes")
                        .as_bytes(),
                )
                .to_hex()
                .to_string(),
            );
        }
        let provider_config = config.primary_provider_config.clone();
        runner = runner.with_agent_factory(config_agent_factory(
            provider_config,
            config.cache_pricing.clone(),
            config
                .context
                .as_ref()
                .and_then(|context| context.cache_stable_prefix)
                .unwrap_or(false),
        ));
    } else {
        // No config provided - fallback to test-suite-only mode
        eprintln!("No --config provided. Running in test-suite-only fallback mode. Use --config=PATH to enable agent-driven evaluation.");
    }

    // Run the evaluation suite
    let mut report = match runner.run_suite().await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Eval run failed: {}", e);
            exit(1);
        }
    };

    report.metadata = Some(serde_json::json!({
        "schema_version": 1, "variant": variant, "cache_preparation": preparation,
        "cache_preparation_source": "operator_label_not_provider_proof",
        "runtime_mode": if agent_driven { "single_agent_fixture" } else { "test_suite_only" },
        "policy_mode": if agent_driven { "permissive_fixture" } else { "test_suite_only" },
        "cost_source": if pricing_fingerprint.is_some() { "configured_estimate" } else { "legacy_nominal_estimate" },
        "pricing_fingerprint": pricing_fingerprint,
        "reported_costs_are_subtotals_when_usage_is_missing": true,
    }));

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

/// The agent factory the `--config` path installs: build the task's provider
/// from the config's primary provider, then run the shared agent runtime. A
/// config with no primary provider yields the historical explicit failure
/// result (never a mock substitution).
fn config_agent_factory(
    provider_config: Option<concerto_config::ProviderConfig>,
    pricing: std::collections::BTreeMap<String, concerto_core::cache_usage::CachePricing>,
    stable_prefix: bool,
) -> AgentFactory {
    Arc::new(move |description: String, project_dir: PathBuf| {
        let provider_config = provider_config.clone();
        let pricing = pricing.clone();
        Box::pin(async move {
            let model =
                provider_config.as_ref().map(|config| config.model.clone()).unwrap_or_default();
            let config_fingerprint = blake3::hash(
                serde_json::to_string(&provider_config)
                    .map_err(|error| error.to_string())?
                    .as_bytes(),
            )
            .to_hex()
            .to_string();
            let creds = concerto_config::CredentialStore::new();
            let provider = match provider_config.as_ref() {
                Some(pc) => ProviderFactory::build(pc, &creds)
                    .map_err(|e| format!("failed to build provider: {e}"))?,
                None => return Ok(no_provider_result()),
            };
            run_agent(
                provider,
                description,
                project_dir,
                stable_prefix,
                model,
                Some(config_fingerprint),
                pricing,
            )
            .await
        })
    })
}

/// The historical "no primary_provider_config" result: an explicit,
/// non-completed benchmark outcome (never a silently mocked success).
fn no_provider_result() -> AgentRunResult {
    AgentRunResult {
        session_id: None,
        telemetry: None,
        completed: false,
        files_modified: Vec::new(),
        tool_call_count: 0,
        final_message: "No primary_provider_config in config file".to_string(),
        total_tokens: 0,
        total_cost_usd: 0.0,
        interventions: 0,
    }
}

/// An agent factory over an already-built provider — the live-runtime eval leg
/// (row #17) installs this with a provider built from the live env contract.
#[cfg(test)]
fn provider_agent_factory(provider: Arc<dyn LlmProvider>, model: String) -> AgentFactory {
    Arc::new(move |description: String, project_dir: PathBuf| {
        let provider = provider.clone();
        let model = model.clone();
        Box::pin(async move {
            run_agent(provider, description, project_dir, false, model, None, Default::default())
                .await
        })
    })
}

/// Run one benchmark task on the shared agent runtime: filesystem/shell tools,
/// permissive eval policy, the canonical build system prompt, and the single
/// agent loop. Shared by the config-driven factory and the live eval leg, so
/// both exercise exactly the same runtime.
async fn run_agent(
    provider: Arc<dyn LlmProvider>,
    description: String,
    project_dir: PathBuf,
    stable_prefix: bool,
    model: String,
    configuration_fingerprint: Option<String>,
    pricing: std::collections::BTreeMap<String, concerto_core::cache_usage::CachePricing>,
) -> Result<AgentRunResult, String> {
    // Create tool registry
    let mut registry = ToolRegistry::default();
    let project_dir_utf8 = camino::Utf8PathBuf::from_path_buf(project_dir.clone())
        .map_err(|_| format!("project_dir must be valid UTF-8: {}", project_dir.display()))?;
    let store = Arc::new(
        concerto_sessions::SqliteSessionStore::connect_shared()
            .await
            .map_err(|error| format!("benchmark persistence failed: {error}"))?,
    );
    let session = store
        .create_session(
            &project_dir_utf8,
            provider.provider_name(),
            &model,
            CancellationToken::new(),
        )
        .await
        .map_err(|error| error.to_string())?;
    let bus = concerto_core::event::EventBus::new(256);
    let recording =
        concerto_sessions::recording::EventRecording::start(&bus, store.clone(), session.id);
    let task = AgentTask::new(session.id, description);
    let _ = bus.publish_for_session(
        session.id,
        task.id.0,
        concerto_core::event::EventKind::BenchmarkRunMetadata {
            task_id: task.id,
            metadata: serde_json::json!({
                "schema_version": 1, "run_id": session.id, "runtime_mode": "single_agent_fixture",
                "policy_mode": "permissive_fixture", "max_iterations": 10,
                "cache_stable_prefix": stable_prefix,
                "model": model, "provider_configuration_fingerprint": configuration_fingerprint,
                "starting_workspace_fingerprint": workspace_fingerprint(&project_dir)?,
                "workspace_exclusions": [".git", "target", "node_modules", "symlinks"],
                "runtime_source_checkout_commit": std::process::Command::new("git").args(["rev-parse", "HEAD"])
                    .current_dir(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.."))
                    .output().ok().filter(|output| output.status.success())
                    .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string()),
            }),
        },
    );
    registry.register(Box::new(FilesystemTool::new(project_dir_utf8)));
    registry.register(Box::new(ShellTool::new()));
    let registry = Arc::new(registry);

    // Create policy engine with permissive rules (expert mode)
    // Empty rules means expert mode - all tools auto-approved
    let policy = Arc::new(SimplePolicyEngine::new(Vec::new(), store.audit_sink()));

    // Create tool executor with auto-approval
    let approval_sink = Arc::new(AllowAllApprovalSink {});
    let tool_executor = Arc::new(
        concerto_core::executor::ToolExecutor::new(registry.clone(), policy.clone())
            .with_approval_sink(approval_sink.clone()),
    );

    // Create eval engine
    let eval_engine = EvalEngine::new(&project_dir);

    // Create prompt builder with the canonical build system prompt (ADR-55
    // §8: the Build prompt constant in concerto-core is the single
    // source of truth).
    let prompt_builder = PromptBuilder::new(concerto_core::types::SYSTEM_PROMPT_BUILD)
        .with_cache_stable_prefix(stable_prefix);

    // Create memory store - use NullMemoryStore for eval (no persistent
    // memory needed)
    let memory: Arc<dyn MemoryStore> = Arc::new(NullMemoryStore {});

    // Create undo manager (not used in eval, but required by AgentLoop)
    let undo_manager = Arc::new(std::sync::Mutex::new(concerto_tools::undo::UndoManager::new(
        project_dir.as_path(),
    )));

    // Create agent loop
    let mut agent_loop = AgentLoop::with_project_root(
        bus.clone(),
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
        None, // budget_allocator
    )
    .with_usage_model(model);

    // Create cancellation token
    let cancel = CancellationToken::new();

    // Preserve evidence for failures as well as successes. Await the durable
    // drain before computing any report; a persistence failure is explicit.
    let outcome = agent_loop.run(task, cancel).await;
    recording
        .finish()
        .await
        .map_err(|error| format!("benchmark event recording failed: {error}"))?;
    let mut report = concerto_sessions::cache::load_cache_report(
        store.as_ref(),
        session.id,
        CancellationToken::new(),
    )
    .await
    .map_err(|error| error.to_string())?;
    if !pricing.is_empty() {
        report.reprice(&pricing);
    }
    let telemetry = report.summary;
    let total_tokens = telemetry.reported_input_tokens + telemetry.reported_output_tokens;
    let total_cost_usd = telemetry.known_estimated_cost_usd;
    let audit = store
        .load_audit_log(
            session.id,
            &concerto_sessions::AuditLogFilter::default(),
            CancellationToken::new(),
        )
        .await
        .map_err(|error| error.to_string())?;
    let interventions = audit
        .iter()
        .filter(|row| row.verdict == "Deny" || row.verdict == "RequireApproval")
        .count() as u32;
    let (completed, files_modified, tool_call_count, final_message) = match outcome {
        Ok(output) => (
            output.completion_status == concerto_core::types::AgentCompletionStatus::Completed,
            output.files_modified.into_iter().map(|path| path.to_string()).collect(),
            output.tool_call_count,
            output.final_message,
        ),
        Err(error) => (false, Vec::new(), 0, format!("Agent failed: {error}")),
    };
    Ok(AgentRunResult {
        completed,
        files_modified,
        tool_call_count,
        final_message,
        total_tokens,
        total_cost_usd,
        interventions,
        session_id: Some(session.id),
        telemetry: Some(telemetry),
    })
}

fn workspace_fingerprint(root: &std::path::Path) -> Result<String, String> {
    fn visit(
        root: &std::path::Path,
        dir: &std::path::Path,
        hash: &mut blake3::Hasher,
    ) -> Result<(), String> {
        let mut entries = std::fs::read_dir(dir)
            .map_err(|error| error.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let kind = entry.file_type().map_err(|error| error.to_string())?;
            let path = entry.path();
            if kind.is_symlink() {
                continue;
            }
            if kind.is_dir() {
                if !matches!(entry.file_name().to_str(), Some(".git" | "target" | "node_modules")) {
                    visit(root, &path, hash)?;
                }
            } else if kind.is_file() {
                let relative = path
                    .strip_prefix(root)
                    .map_err(|error| error.to_string())?
                    .to_str()
                    .ok_or("workspace path is not UTF-8")?
                    .replace('\\', "/");
                let content = std::fs::read(&path).map_err(|error| error.to_string())?;
                hash.update(&(relative.len() as u64).to_le_bytes());
                hash.update(relative.as_bytes());
                hash.update(&(content.len() as u64).to_le_bytes());
                hash.update(&content);
            }
        }
        Ok(())
    }
    let mut hash = blake3::Hasher::new();
    visit(root, root, &mut hash)?;
    Ok(hash.finalize().to_hex().to_string())
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

    let measured = |report: &BenchmarkReport| {
        !report.individual_results.is_empty()
            && report.individual_results.iter().all(|result| {
                result.telemetry.as_ref().is_some_and(|telemetry| {
                    telemetry.cache.calls > 0
                        && telemetry.missing_usage_calls == 0
                        && telemetry.missing_cost_calls == 0
                })
            })
    };
    let efficiency_comparable = measured(current) && measured(&baseline);
    if !efficiency_comparable {
        eprintln!(
            "Cost/token comparisons unavailable: provider telemetry is incomplete or legacy."
        );
    }
    let price_basis = |report: &BenchmarkReport| {
        report.metadata.as_ref().map(|metadata| {
            (metadata.get("pricing_fingerprint").cloned(), metadata.get("cost_source").cloned())
        })
    };
    let costs_comparable = efficiency_comparable && price_basis(current) == price_basis(&baseline);
    if !costs_comparable {
        eprintln!("Cost comparison unavailable: missing measurements or different price versions.");
    }
    // Cost: higher is bad.
    if costs_comparable && baseline.avg_cost_usd > 0.0 {
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
    if efficiency_comparable && baseline.avg_tokens > 0 {
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

    async fn request_ack(
        &self,
        _session_id: concerto_core::ids::Ulid,
        _message: &str,
        _cancel: concerto_core::CancellationToken,
    ) -> bool {
        true
    }
}

// ---------------------------------------------------------------------------
// Live-runtime eval leg (deferred row #17)
//
// Runs a REAL benchmark scenario over a REAL provider runtime. Ignored by
// default AND env-gated twice (the `#[ignore]` attribute plus the runtime
// check below), so it can never run in CI and never requires a network in the
// default suite. It is a manual release check, not a merge gate: the outcome
// is emitted as one JSON line with pass/fail/skip + reason.
// ---------------------------------------------------------------------------
#[cfg(test)]
mod live_eval_tests {
    use super::*;
    use concerto_providers::openai::OpenAiProvider;

    /// The live leg's env contract, reusing the in-tree `CONCERTO_LIVE_PROXY`
    /// convention from the provider pairwise harness. `None` means "not
    /// configured" — a skip, never a failure.
    fn live_endpoint_from_env() -> Option<(String, String, String)> {
        let base = std::env::var("CONCERTO_LIVE_PROXY").ok()?;
        let key = std::env::var("CONCERTO_LIVE_PROXY_KEY").ok()?;
        if base.trim().is_empty() || key.trim().is_empty() {
            return None;
        }
        let model = std::env::var("CONCERTO_LIVE_PROXY_MODEL")
            .unwrap_or_else(|_| "gpt-4o-mini".to_string());
        Some((base.trim_end_matches('/').to_string(), key, model))
    }

    /// One serialized live-leg result: the manual release check reads
    /// `status`/`reason` from stdout.
    #[derive(serde::Serialize)]
    struct LiveLegOutcome {
        leg: &'static str,
        status: &'static str,
        reason: String,
    }

    impl LiveLegOutcome {
        fn skip(reason: impl Into<String>) -> Self {
            Self { leg: LIVE_LEG, status: "skip", reason: reason.into() }
        }

        fn emit(&self) {
            println!("{}", serde_json::to_string(self).unwrap_or_default());
        }
    }

    const LIVE_LEG: &str = "live-runtime-benchmark";

    /// Live leg: a real benchmark suite driven by a real provider built from
    /// the `CONCERTO_LIVE_PROXY*` env contract.
    ///
    /// ```text
    /// CONCERTO_LIVE_PROXY=https://host/v1          # endpoint base URL
    /// CONCERTO_LIVE_PROXY_KEY=...                  # per-session key, never committed
    /// CONCERTO_LIVE_PROXY_MODEL=...                # optional (default gpt-4o-mini)
    /// CONCERTO_LIVE_EVAL_SUITE=...                 # optional suite dir override
    /// cargo test -p concerto-eval-runner live_runtime_benchmark_leg -- --ignored --nocapture
    /// ```
    #[tokio::test]
    #[ignore = "live network: set CONCERTO_LIVE_PROXY + CONCERTO_LIVE_PROXY_KEY, run with --ignored"]
    async fn live_runtime_benchmark_leg() {
        let Some((base, key, model)) = live_endpoint_from_env() else {
            let outcome =
                LiveLegOutcome::skip("CONCERTO_LIVE_PROXY / CONCERTO_LIVE_PROXY_KEY not set");
            outcome.emit();
            eprintln!("skipped: {}", outcome.reason);
            return;
        };

        let suite =
            std::env::var("CONCERTO_LIVE_EVAL_SUITE").map(PathBuf::from).unwrap_or_else(|_| {
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../eval/benchmark_tasks/standard")
            });
        if !suite.exists() {
            let outcome =
                LiveLegOutcome::skip(format!("benchmark suite not found: {}", suite.display()));
            outcome.emit();
            eprintln!("skipped: {}", outcome.reason);
            return;
        }

        let provider: Arc<dyn LlmProvider> =
            Arc::new(OpenAiProvider::new(key, model.clone(), 120).with_api_base(base));
        let runner = EvalRunner::new(&suite)
            .with_timeout(600)
            .with_agent_factory(provider_agent_factory(provider, model));

        match runner.run_suite().await {
            Ok(report) => {
                let status =
                    if report.task_count > 0 && report.pass_rate > 0.0 { "pass" } else { "fail" };
                LiveLegOutcome {
                    leg: LIVE_LEG,
                    status,
                    reason: format!(
                        "tasks={} pass_rate={:.3}",
                        report.task_count, report.pass_rate
                    ),
                }
                .emit();
                // Manual release check, not a merge gate: the run must at least
                // discover and execute one benchmark task.
                assert!(report.task_count > 0, "the live suite discovered no tasks");
            }
            Err(error) => {
                let reason = format!("live eval run failed: {error}");
                LiveLegOutcome { leg: LIVE_LEG, status: "fail", reason: reason.clone() }.emit();
                panic!("{reason}");
            }
        }
    }
}
