//! `concerto run` — one-shot headless agent execution.
//!
//! Runs the real agent runtime (the same `run_shared_agent` entry the TUI and
//! desktop use) for a single prompt and exits with a status code an external
//! orchestrator can branch on. Output is a single human-readable message, or
//! one compact JSON object on stdout with `--json`; every diagnostic goes to
//! stderr so `--json` stdout stays machine-parseable.
//!
//! Two non-interactive approval postures are available. The default `deny`
//! never silently auto-approves: any action whose policy verdict requires
//! approval is refused and the run surfaces as `awaiting_approval` (exit 3).
//! `auto` is the explicit opt-in for trusted, non-interactive environments and
//! still flows through `SimplePolicyEngine` and the audit log.

use std::io::{IsTerminal, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use concerto_config::AppConfig;
use concerto_core::event::EventBus;
use concerto_core::ids::Ulid;
use concerto_core::traits::approval::{ApprovalDecision, ApprovalSink};
use concerto_core::types::{
    AgentCompletionStatus, AgentOutput, PolicyAction, MAX_CONTINUATION_ROUNDS_PREFIX,
    USER_INPUT_REQUIRED_PREFIX,
};
use concerto_core::{CancellationToken, OrchestratorError};
use concerto_orchestrator::harness_service::ApproveInvocationSink;
use concerto_orchestrator::runtime_runner::{
    memory_enabled, run_shared_agent, AgentRunRequest, SharedServices,
};
use concerto_orchestrator::services::{RequestBuilder, ServicesBuilder};
use concerto_orchestrator::session_manager::{ProjectSessionManager, SessionManagerConfig};
use futures::future::BoxFuture;
use serde_json::{json, Value};

/// Exit codes the `run` interface contract promises to an external caller.
pub(crate) const EXIT_DONE: i32 = 0;
pub(crate) const EXIT_ERROR: i32 = 1;
pub(crate) const EXIT_NEEDS_USER: i32 = 2;
pub(crate) const EXIT_AWAITING_APPROVAL: i32 = 3;
pub(crate) const EXIT_BLOCKED: i32 = 4;
pub(crate) const EXIT_ITERATION_CAP: i32 = 5;
pub(crate) const EXIT_TIMEOUT: i32 = 6;

/// Non-interactive approval posture selected by `--approvals`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ApprovalMode {
    /// Fail closed: deny every policy-gated action (the default).
    Deny,
    /// Explicit opt-in: approve every policy-gated action.
    Auto,
}

/// How the prompt text is supplied.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PromptSpec {
    /// Read all of stdin to EOF.
    Stdin,
    /// Use the given literal text.
    Literal(String),
}

/// Fully parsed `concerto run` invocation, prompt resolved.
#[derive(Debug, Clone)]
pub(crate) struct RunOptions {
    pub(crate) prompt: String,
    pub(crate) project: PathBuf,
    pub(crate) multi_agent: bool,
    pub(crate) fast: bool,
    pub(crate) json: bool,
    pub(crate) approvals: ApprovalMode,
    pub(crate) timeout: Option<Duration>,
}

/// Parsed but prompt-unresolved request.
#[derive(Debug)]
struct RunRequest {
    prompt: PromptSpec,
    project: PathBuf,
    multi_agent: bool,
    fast: bool,
    json: bool,
    approvals: ApprovalMode,
    timeout: Option<Duration>,
}

/// Parse outcome: either a help request or a runnable request.
#[derive(Debug)]
enum ParsedRun {
    Help,
    Options(Box<RunRequest>),
}

/// The one prompt builders and external callers share.
fn usage() -> String {
    format!(
        "concerto run — run a single agent task headlessly\n\
         \n\
         USAGE:\n\
         \x20   concerto run [PROMPT] [OPTIONS]\n\
         \n\
         PROMPT is one shell-quoted argument, or `-` / omitted to read stdin\n\
         (stdin must be piped, never a TTY).\n\
         \n\
         OPTIONS:\n\
         \x20   -p, --project <DIR>      Project root (default: current project)\n\
         \x20   -m, --multi-agent        Use coordinator multi-agent orchestration\n\
         \x20   -f, --fast               Skip memory retrieval for trivial tasks\n\
         \x20       --approvals <MODE>   deny (default, fail-closed) | auto\n\
         \x20       --timeout <SECS>     Wall-clock limit; on expiry exit {EXIT_TIMEOUT}\n\
         \x20       --json               Emit one JSON object on stdout\n\
         \x20   -h, --help               Print this help\n\
         \n\
         EXIT CODES: {EXIT_DONE} done, {EXIT_ERROR} error, {EXIT_NEEDS_USER} needs_user, \
         {EXIT_AWAITING_APPROVAL} awaiting_approval, {EXIT_BLOCKED} blocked, \
         {EXIT_ITERATION_CAP} iteration_cap, {EXIT_TIMEOUT} timeout\n\
         \n\
         NOTES:\n\
         \x20   `run`'s options must follow `run` (e.g. `concerto run --json`).\n\
         \x20   With `--json`, stdout is a single JSON object; a well-formed error\n\
         \x20   object is NOT guaranteed when argument parsing itself fails."
    )
}

/// Entry point from the CLI dispatcher. `default_project` is the project the
/// surrounding dispatcher already resolved (when `run` was reached through the
/// normal subcommand path); `run`'s own `-p/--project` still wins.
pub(crate) fn run_run_subcommand(
    args: &[String],
    default_project: Option<&Path>,
) -> anyhow::Result<()> {
    let request = match parse_run_args(args, default_project)? {
        ParsedRun::Help => {
            print!("{}", usage());
            return Ok(());
        }
        ParsedRun::Options(request) => *request,
    };
    // Captured before the request is consumed so a post-parse failure can still
    // honour `--json` and report through the same single-object contract.
    let mode = if request.multi_agent { "multi" } else { "single" };
    let json_mode = request.json;

    // Everything after a successful parse flows through `emit`, so failures
    // here (empty prompt, runtime creation, config/session/provider resolution)
    // still produce a JSON error object instead of exiting with empty stdout.
    // Pre-parse usage errors are intentionally left to propagate: stdout JSON
    // cannot be promised when the flags themselves were unparseable.
    let outcome = match run_request(request) {
        Ok(outcome) => outcome,
        Err(error) => error_outcome(json_mode, mode, &error),
    };
    emit(&outcome);
    std::process::exit(outcome.exit_code);
}

/// Post-parse pipeline: resolve the prompt, build a runtime, and execute. Any
/// failure is converted to an error outcome by the caller.
fn run_request(request: RunRequest) -> anyhow::Result<RunOutcome> {
    let options = RunOptions {
        prompt: resolve_prompt(&request.prompt)?,
        project: request.project,
        multi_agent: request.multi_agent,
        fast: request.fast,
        json: request.json,
        approvals: request.approvals,
        timeout: request.timeout,
    };
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(execute(options))
}

/// Parse `run`'s own options. Returns [`ParsedRun::Help`] for `-h/--help`.
fn parse_run_args(args: &[String], default_project: Option<&Path>) -> anyhow::Result<ParsedRun> {
    let mut project = default_project.map(Path::to_path_buf);
    let mut multi_agent = false;
    let mut fast = false;
    let mut json = false;
    let mut approvals = ApprovalMode::Deny;
    let mut timeout: Option<Duration> = None;
    let mut positionals: Vec<String> = Vec::new();

    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        match arg {
            "-h" | "--help" => return Ok(ParsedRun::Help),
            "-m" | "--multi-agent" => multi_agent = true,
            "-f" | "--fast" => fast = true,
            "--json" => json = true,
            "-p" | "--project" => {
                index += 1;
                let value = args
                    .get(index)
                    .ok_or_else(|| anyhow::anyhow!("--project requires a directory"))?;
                project = Some(PathBuf::from(value));
            }
            "--approvals" => {
                index += 1;
                let value = args
                    .get(index)
                    .ok_or_else(|| anyhow::anyhow!("--approvals requires a mode (deny|auto)"))?;
                approvals = match value.as_str() {
                    "deny" => ApprovalMode::Deny,
                    "auto" => ApprovalMode::Auto,
                    other => {
                        anyhow::bail!("unknown --approvals mode '{other}' (expected deny|auto)")
                    }
                };
            }
            "--timeout" => {
                index += 1;
                let value =
                    args.get(index).ok_or_else(|| anyhow::anyhow!("--timeout requires seconds"))?;
                let secs: u64 = value.parse().map_err(|_| {
                    anyhow::anyhow!("invalid --timeout '{value}' (expected seconds)")
                })?;
                if secs == 0 {
                    anyhow::bail!("--timeout must be greater than 0 seconds");
                }
                timeout = Some(Duration::from_secs(secs));
            }
            "-" => {}
            value if value.starts_with('-') => {
                anyhow::bail!("unknown option '{value}'\n{}", usage())
            }
            value => positionals.push(value.to_string()),
        }
        index += 1;
    }

    // One or more positionals form the prompt; an absent prompt (or `-`) reads stdin.
    let prompt = if positionals.is_empty() {
        PromptSpec::Stdin
    } else {
        PromptSpec::Literal(positionals.join(" "))
    };

    // Validate and resolve the project exactly like the interactive path: a
    // nonexistent `-p` directory must fail fast with a clear message
    // (`ProjectRegistry::select` performs the `is_dir` check) rather than
    // surfacing later as an opaque session-database error.
    let project = crate::resolve_project_dir(project.as_deref())?;

    Ok(ParsedRun::Options(Box::new(RunRequest {
        prompt,
        project,
        multi_agent,
        fast,
        json,
        approvals,
        timeout,
    })))
}

/// Read the prompt from stdin when the caller did not supply one literally.
fn resolve_prompt(spec: &PromptSpec) -> anyhow::Result<String> {
    let stdin = std::io::stdin();
    let is_tty = stdin.is_terminal();
    resolve_prompt_from(spec, stdin.lock(), is_tty)
}

/// Reader-injectable prompt resolution (unit-testable without a real stdin).
///
/// Both paths reject an empty or whitespace-only prompt: an agent run with no
/// instruction is always a caller mistake, and failing here gives a clear
/// message and exit 1 instead of a confusing provider/agent error downstream.
fn resolve_prompt_from<R: Read>(
    spec: &PromptSpec,
    mut reader: R,
    is_tty: bool,
) -> anyhow::Result<String> {
    let text = match spec {
        PromptSpec::Literal(text) => text.clone(),
        PromptSpec::Stdin => {
            if is_tty {
                anyhow::bail!(
                    "no prompt provided and stdin is a terminal; pass PROMPT or pipe text on stdin"
                );
            }
            let mut buffer = String::new();
            reader.read_to_string(&mut buffer)?;
            buffer
        }
    };
    if text.trim().is_empty() {
        anyhow::bail!("prompt is empty; pass a non-empty PROMPT or pipe non-empty text on stdin");
    }
    Ok(text)
}

/// Executes one agent run. Production delegates to the shared runtime runner;
/// tests inject a deterministic implementation.
pub(crate) trait AgentRunner: Send + Sync {
    fn run(
        &self,
        request: AgentRunRequest,
        services: SharedServices,
    ) -> BoxFuture<'_, Result<AgentOutput, OrchestratorError>>;
}

/// Production runner: the same entry the TUI and desktop call.
struct SharedAgentRunner;

impl AgentRunner for SharedAgentRunner {
    fn run(
        &self,
        request: AgentRunRequest,
        services: SharedServices,
    ) -> BoxFuture<'_, Result<AgentOutput, OrchestratorError>> {
        Box::pin(run_shared_agent(request, services))
    }
}

/// Structured result of a run, decoupled from stdout so it is unit-testable.
pub(crate) struct RunOutcome {
    pub(crate) exit_code: i32,
    pub(crate) json_mode: bool,
    pub(crate) status: &'static str,
    pub(crate) message: String,
    /// Human-readable session/files lines printed to stderr in text mode
    /// (never in `--json` mode), after the status/exit line that `emit` renders
    /// from the fields. Composed here so tests can assert the content without
    /// capturing process streams.
    pub(crate) text_summary: Vec<String>,
    pub(crate) json: Value,
    pub(crate) stderr: Vec<String>,
}

/// Approval sink that records the fact a gated action was refused, then denies.
///
/// The single-agent loop runs under orchestrator authority, so the intent gate
/// auto-allows ordinary local mutations; only **Consequential** actions
/// (network egress, destructive filesystem/shell operations, secrets, install)
/// still reach the approval sink. Recording the refusal lets the caller report
/// a truthful `awaiting_approval` instead of a misleading success.
struct DenyRecordingSink {
    denied: Arc<AtomicBool>,
    first_action: Arc<Mutex<Option<String>>>,
}

#[async_trait::async_trait]
impl ApprovalSink for DenyRecordingSink {
    async fn request_approval(
        &self,
        action: &PolicyAction<'_>,
        _cancel: CancellationToken,
    ) -> ApprovalDecision {
        self.denied.store(true, Ordering::SeqCst);
        let mut first = self.first_action.lock().unwrap_or_else(|error| error.into_inner());
        if first.is_none() {
            *first = Some(action.tool_name.to_string());
        }
        ApprovalDecision::Deny
    }

    async fn approve_all_for_session(&self, _session_id: Ulid, _cancel: CancellationToken) {}

    /// Advisory warnings (e.g. "project is not a git repo, changes cannot be
    /// auto-undone") are acknowledged so a headless run can proceed. This is
    /// not a fail-open: the security-relevant gate is [`Self::request_approval`],
    /// which still denies every policy-gated action.
    async fn request_ack(
        &self,
        _session_id: Ulid,
        _message: &str,
        _cancel: CancellationToken,
    ) -> bool {
        true
    }
}

/// Build config, a persistent session manager, and the production runner, then
/// run once.
async fn execute(options: RunOptions) -> anyhow::Result<RunOutcome> {
    let config_path = concerto_config::default_config_path();
    let config = concerto_config::load_config(config_path.as_ref(), Some(&options.project))?;
    let session_manager = Arc::new(
        ProjectSessionManager::connect_with_config(SessionManagerConfig {
            git_auto_init: config
                .tool_settings
                .as_ref()
                .is_none_or(|settings| settings.git_auto_init),
        })
        .await
        .map_err(|error| anyhow::anyhow!("could not open session database: {error}"))?,
    );
    Ok(execute_with_runner(options, Arc::new(SharedAgentRunner), config, Some(session_manager))
        .await)
}

/// Testable core: build the request/services around an injected runner and map
/// its outcome to a status and exit code. No stdout, no process exit.
pub(crate) async fn execute_with_runner(
    options: RunOptions,
    runner: Arc<dyn AgentRunner>,
    config: AppConfig,
    session_manager: Option<Arc<ProjectSessionManager>>,
) -> RunOutcome {
    let mode = if options.multi_agent { "multi" } else { "single" };
    let mut stderr = Vec::new();
    if options.approvals == ApprovalMode::Auto {
        stderr.push(
            "warning: --approvals auto auto-approves every policy-gated action; \
             use only in trusted, non-interactive environments"
                .to_string(),
        );
    }

    let denied = Arc::new(AtomicBool::new(false));
    let first_action: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let sink: Arc<dyn ApprovalSink> = match options.approvals {
        ApprovalMode::Auto => Arc::new(ApproveInvocationSink),
        ApprovalMode::Deny => Arc::new(DenyRecordingSink {
            denied: denied.clone(),
            first_action: first_action.clone(),
        }),
    };

    let bus = EventBus::new(1024);
    let mut services_builder = ServicesBuilder::new(bus, config.clone(), sink);
    if let Some(manager) = session_manager {
        services_builder = services_builder.with_session_manager(manager);
    }
    let services = services_builder.build();

    let cancel = CancellationToken::new();
    let request =
        RequestBuilder::new(options.prompt.clone(), options.project.clone(), cancel.clone())
            .with_single_agent(!options.multi_agent)
            .with_memory_enabled(memory_enabled(options.fast, config.memory.enabled))
            .build();

    let timed_out = Arc::new(AtomicBool::new(false));
    let timeout_task = options.timeout.map(|duration| {
        let cancel = cancel.clone();
        let timed_out = timed_out.clone();
        tokio::spawn(async move {
            tokio::time::sleep(duration).await;
            timed_out.store(true, Ordering::SeqCst);
            cancel.cancel();
        })
    });

    let result = runner.run(request, services).await;

    if let Some(task) = timeout_task {
        task.abort();
    }

    if timed_out.load(Ordering::SeqCst) {
        return outcome_from(
            EXIT_TIMEOUT,
            "timeout",
            &options,
            mode,
            None,
            "run exceeded the configured --timeout",
            diagnostics(&mut stderr, &denied, &first_action),
        );
    }

    match result {
        Ok(output) => {
            let (status, exit_code) = classify(&output, denied.load(Ordering::SeqCst));
            let message = output.summary();
            outcome_from(
                exit_code,
                status,
                &options,
                mode,
                Some(&output),
                &message,
                diagnostics(&mut stderr, &denied, &first_action),
            )
        }
        Err(error) => outcome_from(
            EXIT_ERROR,
            "error",
            &options,
            mode,
            None,
            &error.to_string(),
            diagnostics(&mut stderr, &denied, &first_action),
        ),
    }
}

/// Append a diagnostic naming the first refused action; leave stderr unchanged
/// otherwise.
fn diagnostics(
    stderr: &mut Vec<String>,
    denied: &Arc<AtomicBool>,
    first_action: &Arc<Mutex<Option<String>>>,
) -> Vec<String> {
    if denied.load(Ordering::SeqCst) {
        let action = first_action
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
            .unwrap_or_else(|| "policy-gated action".to_string());
        stderr.push(format!(
            "denied policy approval for '{action}' (headless default); \
             re-run with --approvals auto to permit it"
        ));
    }
    std::mem::take(stderr)
}

#[allow(clippy::too_many_arguments)]
fn outcome_from(
    exit_code: i32,
    status: &'static str,
    options: &RunOptions,
    mode: &str,
    output: Option<&AgentOutput>,
    message: &str,
    stderr: Vec<String>,
) -> RunOutcome {
    let session_id = output.map(|output| output.session_id);
    let json = build_json(mode, status, session_id, message, output);
    RunOutcome {
        exit_code,
        json_mode: options.json,
        status,
        message: message.to_string(),
        text_summary: text_summary(session_id, output),
        json,
        stderr,
    }
}

/// Compose the text-mode stderr detail lines: the session id and the
/// changed-file summary. The `status: … (exit N)` line is rendered by [`emit`]
/// from the outcome fields. Printed on every text-mode invocation (success
/// included), so an orchestrator reading stderr always sees them.
fn text_summary(session_id: Option<Ulid>, output: Option<&AgentOutput>) -> Vec<String> {
    let mut lines = vec![format!(
        "session: {}",
        session_id.map(|id| id.to_string()).unwrap_or_else(|| "none".to_string())
    )];
    let files: Vec<String> = output
        .map(|output| output.files_modified.iter().map(|path| path.to_string()).collect())
        .unwrap_or_default();
    if files.is_empty() {
        lines.push("files: 0".to_string());
    } else {
        lines.push(format!("files: {}", files.len()));
        lines.extend(files.into_iter().map(|file| format!("  {file}")));
    }
    lines
}

/// Convert a post-parse failure into a reportable outcome. Under `--json` this
/// yields the same single-object contract as a completed run, with
/// `status:"error"` and exit 1.
fn error_outcome(json_mode: bool, mode: &str, error: &anyhow::Error) -> RunOutcome {
    let message = error.to_string();
    let json = build_json(mode, "error", None, &message, None);
    RunOutcome {
        exit_code: EXIT_ERROR,
        json_mode,
        status: "error",
        message,
        text_summary: text_summary(None, None),
        json,
        stderr: Vec::new(),
    }
}

/// Map a completed run to its published status and exit code.
///
/// The refusal signal wins over a `Partial`/`Completed` classification: a run
/// whose gated action was denied is not a silent success.
fn classify(output: &AgentOutput, denied: bool) -> (&'static str, i32) {
    if denied {
        return ("awaiting_approval", EXIT_AWAITING_APPROVAL);
    }
    match output.completion_status {
        AgentCompletionStatus::Completed => ("done", EXIT_DONE),
        AgentCompletionStatus::AwaitingUser => ("awaiting_approval", EXIT_AWAITING_APPROVAL),
        AgentCompletionStatus::Partial => {
            let message = output.final_message.as_str();
            if message.starts_with(USER_INPUT_REQUIRED_PREFIX) {
                ("needs_user", EXIT_NEEDS_USER)
            } else if message.starts_with(MAX_CONTINUATION_ROUNDS_PREFIX) {
                ("iteration_cap", EXIT_ITERATION_CAP)
            } else {
                ("blocked", EXIT_BLOCKED)
            }
        }
        // `AgentCompletionStatus` is `#[non_exhaustive]`; treat any future
        // partial-ish state conservatively.
        _ => ("blocked", EXIT_BLOCKED),
    }
}

/// Compose the published JSON object. `output` is absent for error/timeout.
fn build_json(
    mode: &str,
    status: &str,
    session_id: Option<Ulid>,
    final_message: &str,
    output: Option<&AgentOutput>,
) -> Value {
    let (files, tools, tokens_in, tokens_out, cost) = match output {
        Some(output) => (
            Value::Array(
                output.files_modified.iter().map(|path| Value::String(path.to_string())).collect(),
            ),
            output.tool_call_count,
            output.provider_metrics.iter().map(|metric| metric.tokens_in).sum::<u64>(),
            output.provider_metrics.iter().map(|metric| metric.tokens_out).sum::<u64>(),
            output.provider_metrics.iter().map(|metric| metric.cost_usd).sum::<f64>(),
        ),
        None => (Value::Array(Vec::new()), 0, 0, 0, 0.0),
    };
    json!({
        "mode": mode,
        "status": status,
        "session_id": session_id.map(|id| id.to_string()),
        "final_message": final_message,
        "files_modified": files,
        "tool_call_count": tools,
        "tokens_in": tokens_in,
        "tokens_out": tokens_out,
        "cost_usd": cost,
    })
}

/// Write the outcome to stdout/stderr: JSON stays a single object; diagnostics
/// never contaminate it.
///
/// Text mode prints the final message to stdout and, on stderr, the
/// status/exit line followed by the session/files summary (and any
/// diagnostics). `--json` mode prints only the one JSON object to stdout; even
/// then, diagnostics stay on stderr.
fn emit(outcome: &RunOutcome) {
    if outcome.json_mode {
        println!("{}", outcome.json);
    } else {
        println!("{}", outcome.message);
        eprintln!("status: {} (exit {})", outcome.status, outcome.exit_code);
        for line in &outcome.text_summary {
            eprintln!("{line}");
        }
    }
    for line in &outcome.stderr {
        eprintln!("{line}");
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    use concerto_core::error::ProviderError;
    use concerto_core::policy::SimplePolicyEngine;
    use concerto_core::traits::memory::{MemoryStore, NullMemoryStore};
    use concerto_core::traits::policy::AuditLog;
    use concerto_core::traits::provider::{CompletionStream, LlmProvider};
    use concerto_core::types::{
        AgentTask, CompletionChunk, CompletionRequest, TaskId, TokenBudget, ToolCall, ToolRegistry,
    };
    use concerto_core::PolicyPresets;
    use concerto_eval::EvalEngine;
    use concerto_orchestrator::agent_loop::AgentLoop;
    use concerto_orchestrator::prompts::PromptBuilder;
    use concerto_sessions::SqliteSessionStore;
    use concerto_tools::filesystem::FilesystemTool;
    use concerto_tools::shell::ShellTool;
    use std::collections::VecDeque;
    use tempfile::TempDir;

    /// One pre-scripted provider response.
    #[derive(Clone)]
    enum TestResponse {
        Text(String),
        ToolCall(ToolCall),
        Done,
    }

    /// A minimal scripted `LlmProvider` local to this crate's tests. The shared
    /// `concerto_providers::testing::ScriptedProvider` is `#[cfg(test)]`-gated
    /// inside the providers crate (`providers/src/lib.rs`), so it is not
    /// exported to a dependent crate's test build; this fake stands in for it.
    /// Responses are consumed once, so successive completion calls model a real
    /// multi-turn conversation.
    struct TestProvider {
        responses: Mutex<VecDeque<TestResponse>>,
    }

    impl TestProvider {
        fn text(text: &str) -> Self {
            Self {
                responses: Mutex::new(VecDeque::from(vec![
                    TestResponse::Text(text.to_string()),
                    TestResponse::Done,
                ])),
            }
        }

        fn tool_call(name: &str, arguments: Value) -> Self {
            Self {
                responses: Mutex::new(VecDeque::from(vec![
                    TestResponse::ToolCall(ToolCall {
                        id: "call_test".to_string(),
                        name: name.to_string(),
                        arguments,
                        ..Default::default()
                    }),
                    TestResponse::Done,
                ])),
            }
        }
    }

    #[async_trait::async_trait]
    impl LlmProvider for TestProvider {
        fn provider_name(&self) -> &'static str {
            "test"
        }

        fn context_capacity(&self, _model: &str) -> TokenBudget {
            TokenBudget::new(128_000, 4_096)
        }

        fn approximate_cost(&self, _tokens_in: u64, _tokens_out: u64) -> f64 {
            0.0
        }

        async fn stream_completion(
            &self,
            _request: CompletionRequest,
            _cancel: CancellationToken,
        ) -> Result<CompletionStream, ProviderError> {
            // Drain this turn's scripted responses; later turns see an empty
            // queue and emit only a terminal chunk.
            let drained: Vec<TestResponse> = {
                let mut guard = self.responses.lock().unwrap_or_else(|error| error.into_inner());
                std::mem::take(&mut *guard).into_iter().collect()
            };
            let mut script: Vec<TestResponse> = drained;
            if !script.iter().any(|response| matches!(response, TestResponse::Done)) {
                script.push(TestResponse::Done);
            }
            let stream = futures::stream::iter(script.into_iter().map(|response| {
                Ok(match response {
                    TestResponse::Text(delta) => CompletionChunk {
                        delta,
                        reasoning: None,
                        tool_call: None,
                        is_final: false,
                        usage: None,
                    },
                    TestResponse::ToolCall(tool_call) => CompletionChunk {
                        delta: String::new(),
                        reasoning: None,
                        tool_call: Some(tool_call),
                        is_final: false,
                        usage: None,
                    },
                    TestResponse::Done => CompletionChunk {
                        delta: String::new(),
                        reasoning: None,
                        tool_call: None,
                        is_final: true,
                        usage: None,
                    },
                })
            }));
            Ok(Box::pin(stream))
        }
    }

    async fn in_memory_manager() -> (Arc<ProjectSessionManager>, Arc<dyn AuditLog>) {
        let store = Arc::new(
            SqliteSessionStore::connect_in_memory().await.expect("in-memory session store"),
        );
        let audit: Arc<dyn AuditLog> = store.audit_sink();
        let manager = Arc::new(ProjectSessionManager::from_store(store));
        (manager, audit)
    }

    /// Deterministic runner: the real tool/policy/agent-loop stack with a
    /// scripted provider. It uses the approval sink the CLI installed in
    /// `services`, so deny/auto behavior is exercised for real.
    struct ScriptedRunner {
        provider: Arc<dyn LlmProvider>,
        audit: Arc<dyn AuditLog>,
    }

    impl AgentRunner for ScriptedRunner {
        fn run(
            &self,
            request: AgentRunRequest,
            services: SharedServices,
        ) -> BoxFuture<'_, Result<AgentOutput, OrchestratorError>> {
            Box::pin(async move {
                let project = request.project_dir.clone();
                let project_utf8 = camino::Utf8PathBuf::from_path_buf(project.clone())
                    .map_err(|_| OrchestratorError::AgentLoopError("non-UTF-8 project".into()))?;
                let manager = services.session_manager.clone().ok_or_else(|| {
                    OrchestratorError::AgentLoopError("test runner needs a session manager".into())
                })?;
                let session = manager
                    .get_or_create_active_session(
                        &project_utf8,
                        self.provider.provider_name(),
                        "scripted",
                        CancellationToken::new(),
                    )
                    .await
                    .map_err(|error| OrchestratorError::AgentLoopError(error.to_string()))?;

                let mut registry = ToolRegistry::default();
                registry.register(Box::new(FilesystemTool::new(project_utf8)));
                registry.register(Box::new(ShellTool::new()));
                let registry = Arc::new(registry);
                let policy = Arc::new(SimplePolicyEngine::new(
                    PolicyPresets::default_rules(),
                    self.audit.clone(),
                ));
                let executor = Arc::new(
                    concerto_core::executor::ToolExecutor::new(registry, policy)
                        .with_approval_sink(services.approval_sink.clone()),
                );
                let memory: Arc<dyn MemoryStore> = Arc::new(NullMemoryStore);
                let undo = Arc::new(std::sync::Mutex::new(concerto_tools::undo::UndoManager::new(
                    project.as_path(),
                )));
                let eval_engine = EvalEngine::new(&project);
                let prompt_builder = PromptBuilder::new(concerto_core::types::SYSTEM_PROMPT_BUILD);
                let mut agent_loop = AgentLoop::with_project_root(
                    services.bus.clone(),
                    services.approval_sink.clone(),
                    self.provider.clone(),
                    executor,
                    memory,
                    undo,
                    eval_engine,
                    prompt_builder,
                    10,
                    false,
                    project,
                    None,
                );
                let task = AgentTask::new(session.session_id, request.input.clone());
                agent_loop.run(task, request.cancel_token.clone()).await
            })
        }
    }

    /// Runner that parks until cancelled — drives the wall-clock timeout path.
    struct HangingRunner;

    impl AgentRunner for HangingRunner {
        fn run(
            &self,
            request: AgentRunRequest,
            _services: SharedServices,
        ) -> BoxFuture<'_, Result<AgentOutput, OrchestratorError>> {
            Box::pin(async move {
                request.cancel_token.cancelled().await;
                Err(OrchestratorError::AgentLoopError("cancelled".into()))
            })
        }
    }

    /// Runner returning a canned [`AgentOutput`] — lets the pure outcome
    /// composition (text summary, JSON) be asserted without a real agent loop.
    struct StaticRunner {
        output: AgentOutput,
    }

    impl AgentRunner for StaticRunner {
        fn run(
            &self,
            _request: AgentRunRequest,
            _services: SharedServices,
        ) -> BoxFuture<'_, Result<AgentOutput, OrchestratorError>> {
            let output = self.output.clone();
            Box::pin(async move { Ok(output) })
        }
    }

    fn options(project: &Path, prompt: &str, json: bool) -> RunOptions {
        RunOptions {
            prompt: prompt.to_string(),
            project: project.to_path_buf(),
            multi_agent: false,
            fast: false,
            json,
            approvals: ApprovalMode::Deny,
            timeout: None,
        }
    }

    #[tokio::test]
    async fn completed_run_reports_done_with_session_id() {
        let dir = TempDir::new().expect("tempdir");
        let (manager, audit) = in_memory_manager().await;
        let runner: Arc<dyn AgentRunner> = Arc::new(ScriptedRunner {
            provider: Arc::new(TestProvider::text("hello from the agent")),
            audit,
        });
        let outcome = execute_with_runner(
            options(dir.path(), "say hello", true),
            runner,
            AppConfig::default(),
            Some(manager),
        )
        .await;
        assert_eq!(outcome.exit_code, EXIT_DONE, "message: {}", outcome.message);
        assert_eq!(outcome.status, "done", "message: {}", outcome.message);
        assert_eq!(outcome.json["status"], "done");
        assert!(
            outcome.json["session_id"].is_string(),
            "completed run must publish a session id: {}",
            outcome.json
        );
        assert_eq!(outcome.json["mode"], "single");
        assert!(
            outcome.json["final_message"].as_str().is_some_and(|m| m.contains("hello")),
            "final message missing: {}",
            outcome.json
        );
    }

    #[tokio::test]
    async fn denied_gated_action_reports_awaiting_approval() {
        let dir = TempDir::new().expect("tempdir");
        let (manager, audit) = in_memory_manager().await;
        // `filesystem` `delete` is a Consequential action, so even the
        // single-agent loop's orchestrator-authority path keeps it under the
        // approval gate — exactly the class the non-interactive sink must deny.
        let provider = TestProvider::tool_call(
            "filesystem",
            json!({"operation": "delete", "path": "out.txt"}),
        );
        let runner: Arc<dyn AgentRunner> =
            Arc::new(ScriptedRunner { provider: Arc::new(provider), audit });
        let outcome = execute_with_runner(
            options(dir.path(), "delete a file", true),
            runner,
            AppConfig::default(),
            Some(manager),
        )
        .await;
        assert_eq!(outcome.exit_code, EXIT_AWAITING_APPROVAL, "message: {}", outcome.message);
        assert_eq!(outcome.status, "awaiting_approval", "message: {}", outcome.message);
        assert_eq!(outcome.json["status"], "awaiting_approval");
        assert!(
            outcome.stderr.iter().any(|line| line.contains("denied policy approval")),
            "expected a refusal diagnostic: {:?}",
            outcome.stderr
        );
    }

    #[tokio::test]
    async fn auto_approvals_permits_the_gated_action() {
        let dir = TempDir::new().expect("tempdir");
        let (manager, audit) = in_memory_manager().await;
        let provider = TestProvider::tool_call(
            "filesystem",
            json!({"operation": "delete", "path": "out.txt"}),
        );
        let runner: Arc<dyn AgentRunner> =
            Arc::new(ScriptedRunner { provider: Arc::new(provider), audit });
        let mut opts = options(dir.path(), "delete a file", true);
        opts.approvals = ApprovalMode::Auto;
        let outcome = execute_with_runner(opts, runner, AppConfig::default(), Some(manager)).await;
        // The action was permitted, so the run must not surface as
        // awaiting_approval; it may still be blocked/partial for other reasons.
        assert_ne!(outcome.status, "awaiting_approval");
        assert!(
            outcome.stderr.iter().any(|line| line.contains("--approvals auto")),
            "expected the auto-approval warning: {:?}",
            outcome.stderr
        );
        assert!(
            outcome.stderr.iter().all(|line| !line.contains("denied policy approval")),
            "auto mode must not record a refusal: {:?}",
            outcome.stderr
        );
    }

    #[tokio::test]
    async fn wall_clock_timeout_reports_timeout_exit_6() {
        let dir = TempDir::new().expect("tempdir");
        let (manager, _audit) = in_memory_manager().await;
        let mut opts = options(dir.path(), "hang", true);
        opts.timeout = Some(Duration::from_millis(50));
        let outcome =
            execute_with_runner(opts, Arc::new(HangingRunner), AppConfig::default(), Some(manager))
                .await;
        assert_eq!(outcome.exit_code, EXIT_TIMEOUT);
        assert_eq!(outcome.status, "timeout");
        assert_eq!(outcome.json["status"], "timeout");
    }

    #[test]
    fn parse_rejects_unknown_option() {
        let error = parse_run_args(&["--nope".to_string()], None).expect_err("must reject");
        assert!(error.to_string().contains("unknown option"));
    }

    #[test]
    fn parse_rejects_nonexistent_project_directory() {
        // M3: the `-p` fast path must validate like the interactive path.
        let error = parse_run_args(
            &[
                "--project".to_string(),
                "/definitely/does/not/exist/xyz-12345".to_string(),
                "hi".to_string(),
            ],
            None,
        )
        .expect_err("nonexistent project must fail fast");
        assert!(error.to_string().contains("does not exist"), "{error}");
    }

    #[test]
    fn parse_rejects_zero_timeout() {
        let error = parse_run_args(&["--timeout".to_string(), "0".to_string()], None)
            .expect_err("zero timeout must be rejected");
        assert!(error.to_string().contains("greater than 0"), "{error}");
    }

    #[test]
    fn resolve_prompt_rejects_empty_input() {
        // m1: empty/whitespace-only prompts are rejected on both paths, and a
        // TTY stdin is still refused.
        assert!(resolve_prompt_from(&PromptSpec::Stdin, &b""[..], false).is_err());
        assert!(resolve_prompt_from(&PromptSpec::Stdin, &b"   \n\t"[..], false).is_err());
        assert!(resolve_prompt_from(&PromptSpec::Stdin, &b"x"[..], true).is_err());
        assert!(
            resolve_prompt_from(&PromptSpec::Literal("   ".to_string()), &b""[..], false).is_err()
        );
        let ok =
            resolve_prompt_from(&PromptSpec::Stdin, &b"hello\n"[..], false).expect("non-empty");
        assert_eq!(ok, "hello\n");
    }

    #[test]
    fn error_outcome_is_json_parseable_and_exits_one() {
        // M2: post-parse failures must still emit a full JSON object under
        // `--json`, so an external caller never sees empty stdout.
        let error = anyhow::anyhow!("no LLM provider configured");
        let outcome = error_outcome(true, "single", &error);
        assert_eq!(outcome.exit_code, EXIT_ERROR);
        assert_eq!(outcome.status, "error");
        assert_eq!(outcome.json["status"], "error");
        assert_eq!(outcome.json["mode"], "single");
        assert_eq!(outcome.json["session_id"], Value::Null);
        for key in [
            "mode",
            "status",
            "session_id",
            "final_message",
            "files_modified",
            "tool_call_count",
            "tokens_in",
            "tokens_out",
            "cost_usd",
        ] {
            assert!(outcome.json.get(key).is_some(), "missing key {key}: {}", outcome.json);
        }
        assert!(
            outcome.json["final_message"].as_str().is_some_and(|m| m.contains("no LLM provider")),
            "error message missing: {}",
            outcome.json
        );
    }

    #[tokio::test]
    async fn text_mode_summary_includes_status_session_and_files() {
        // M1: text mode always reports status/session/files on stderr.
        let dir = TempDir::new().expect("tempdir");
        let (manager, _audit) = in_memory_manager().await;
        let mut output = sample_partial();
        output.completion_status = AgentCompletionStatus::Completed;
        output.final_message = "all done".to_string();
        output.files_modified =
            vec![camino::Utf8PathBuf::from("src/a.rs"), camino::Utf8PathBuf::from("src/b.rs")];
        let session_id = output.session_id;
        let outcome = execute_with_runner(
            options(dir.path(), "do it", false),
            Arc::new(StaticRunner { output }),
            AppConfig::default(),
            Some(manager),
        )
        .await;
        assert!(!outcome.json_mode);
        assert_eq!(outcome.exit_code, EXIT_DONE);
        assert_eq!(outcome.status, "done");
        let joined = outcome.text_summary.join("\n");
        assert!(joined.contains(&format!("session: {session_id}")), "{joined}");
        assert!(joined.contains("files: 2"), "{joined}");
        assert!(joined.contains("src/a.rs"), "{joined}");
        assert!(joined.contains("src/b.rs"), "{joined}");
    }

    #[test]
    fn parse_reads_all_flags() {
        let dir = TempDir::new().expect("tempdir");
        let args = vec![
            "do".to_string(),
            "the".to_string(),
            "thing".to_string(),
            "--multi-agent".to_string(),
            "--fast".to_string(),
            "--json".to_string(),
            "--approvals".to_string(),
            "auto".to_string(),
            "--timeout".to_string(),
            "30".to_string(),
        ];
        let ParsedRun::Options(request) = parse_run_args(&args, Some(dir.path())).expect("parse")
        else {
            panic!("expected options");
        };
        assert_eq!(request.prompt, PromptSpec::Literal("do the thing".to_string()));
        assert!(request.multi_agent);
        assert!(request.fast);
        assert!(request.json);
        assert_eq!(request.approvals, ApprovalMode::Auto);
        assert_eq!(request.timeout, Some(Duration::from_secs(30)));
        assert!(request.project.is_dir(), "project must resolve to a real directory");
    }

    #[test]
    fn parse_without_prompt_reads_stdin() {
        let dir = TempDir::new().expect("tempdir");
        let ParsedRun::Options(request) =
            parse_run_args(&["-".to_string()], Some(dir.path())).expect("parse")
        else {
            panic!("expected options");
        };
        assert_eq!(request.prompt, PromptSpec::Stdin);
    }

    #[test]
    fn parse_help_short_circuits() {
        assert!(matches!(
            parse_run_args(&["--help".to_string()], None).expect("parse"),
            ParsedRun::Help
        ));
    }

    #[test]
    fn classify_maps_partial_message_prefixes() {
        // Built from the shared producer constants so classifier and producer
        // cannot drift apart.
        let mut output = sample_partial();
        output.final_message = format!("{USER_INPUT_REQUIRED_PREFIX} which file?");
        assert_eq!(classify(&output, false), ("needs_user", EXIT_NEEDS_USER));

        output.final_message = format!("{MAX_CONTINUATION_ROUNDS_PREFIX} (3).");
        assert_eq!(classify(&output, false), ("iteration_cap", EXIT_ITERATION_CAP));

        output.final_message = "Blocked: no file changes".into();
        assert_eq!(classify(&output, false), ("blocked", EXIT_BLOCKED));

        assert_eq!(classify(&output, true), ("awaiting_approval", EXIT_AWAITING_APPROVAL));
    }

    fn sample_partial() -> AgentOutput {
        AgentOutput {
            task_id: TaskId::new(),
            session_id: concerto_core::ids::new_id(),
            final_message: String::new(),
            files_modified: Vec::new(),
            tool_call_count: 0,
            eval_result: None,
            tool_events: Vec::new(),
            verification: Vec::new(),
            project_root: None,
            completion_status: AgentCompletionStatus::Partial,
            provider_metrics: Vec::new(),
            checkpoint_json: None,
        }
    }
}
