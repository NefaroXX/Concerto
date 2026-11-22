//! Agent-process entry — ADR-60 S5.
//!
//! The real single-agent loop ([`AgentLoop`]) as a supervised child process.
//! The supervisor spawns this binary, drives the versioned stdio handshake
//! (ADR-60 D2), and answers every write-path request (`execute-tool`,
//! `publish-event`, `retrieve-memory`, `list-tools`) over the protocol
//! defined in [`concerto_orchestrator::ipc`]. The loop's executor call site
//! is the gate proxy ([`GateProxyBackend`]), so every tool call is a gated
//! write through the supervisor's single write gate (ADR-60 D4) — the
//! process boundary is the attribution boundary.
//!
//! The slice runs exactly one task per process: the task arrives via the
//! environment, the loop runs to completion, a terminal whiteboard event is
//! published (`subtask-completed` / `failure`), and the process exits. The
//! supervisor owns restarts. On Linux the child additionally arms
//! `PR_SET_PDEATHSIG` so a supervisor crash tears it down instead of leaking
//! it (ADR-60 D1 orphan cleanup).
//!
//! ## Environment contract
//!
//! | Variable | Meaning |
//! |----------|---------|
//! | `CONCERTO_AGENT_ID` | Registered agent identity (required). |
//! | `CONCERTO_PROJECT_ROOT` | Project directory the loop is scoped to (required). |
//! | `CONCERTO_TASK_DESCRIPTION` | The task objective (required). |
//! | `CONCERTO_MAX_ITERATIONS` | Loop iteration cap (default 25). |
//! | `CONCERTO_AGENT_CONFIG_JSON` | The parent's resolved [`AppConfig`] as JSON; the real provider is rebuilt from it (required unless `CONCERTO_PROVIDER=mock`). |
//! | `CONCERTO_PROVIDER` | `mock` selects the explicit mock opt-in (tests/fixtures only); unset or any other value uses the real config/credential path. |
//! | `CONCERTO_MOCK_SCRIPT_JSON` | Optional per-turn `CompletionChunk` script for the mock provider. |
//! | `CONCERTO_PLAN_ID` | Optional approved plan id (ADR-60 D7 ledger enrichment); stamps every gated write and the terminal event. |
//! | `CONCERTO_AGENT_SKILLS_SECTION` | Optional pre-rendered skills section injected into the system prompt; the parent renders it once (ADR-43). |
//! | `CONCERTO_AGENT_HISTORY_PATH` | Optional parent-owned accumulated history file (typed JSON array) for restore and append. |
//! | `CONCERTO_AGENT_HISTORY_SEED_LEN` | Optional exact repaired seed message count stamped by the parent; the child never rewrites the accumulated file. |
//! | `CONCERTO_AGENT_METRICS_PATH` | Optional parent-owned JSON Lines sidecar for this process's provider metrics. |
//!
//! ## Stdout discipline
//!
//! Stdout carries protocol frames only. All diagnostics go to stderr (or are
//! dropped — the slice installs no tracing subscriber, so `tracing` macros
//! are no-ops and the binary logs important path events with `eprintln!`).
//!
//! ## Exit codes
//!
//! - `0` — the task completed; a `subtask-completed` event was published.
//! - `1` — fatal failure: bad environment, supervisor gone/version-mismatch,
//!   or the task failed; a best-effort `failure` event is published first.
//!
//! ## Deferred to later chunks
//!
//! - Interactive approval surfacing — the child's approval sink bridges to the
//!   supervisor over IPC ([`ApprovalProxySink`]); when the supervisor cannot
//!   answer, the child denies (fail-closed default, unchanged).
//! - Memory stores/invalidations are supervisor-side (D6); the child's
//!   memory store is a retrieval facade.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use concerto_config::CredentialStore;
use concerto_core::event::EventBus;
use concerto_core::ids::Ulid;
use concerto_core::memory::ProjectId;
use concerto_core::traits::approval::ApprovalSink;
use concerto_core::traits::provider::LlmProvider;
use concerto_core::types::{system_prompt_for, AgentTask, Message, ProviderMetrics};
use concerto_core::{CancellationToken, RequestedOutcome};
use concerto_eval::EvalEngine;
use concerto_orchestrator::agent_process_config::{
    self, AgentProcessProviderError, CONFIG_ENV, PROVIDER_ENV,
};
use concerto_orchestrator::gate_proxy::{
    ApprovalProxySink, GateProxyBackend, GateProxyClient, GateProxyMemoryStore,
};
use concerto_orchestrator::prompts::PromptBuilder;
use concerto_orchestrator::supervised_history::{self, HISTORY_ENV, METRICS_ENV};
use concerto_sessions::whiteboard::{NewWhiteboardEvent, WhiteboardKind};
use concerto_tools::undo::UndoManager;
use serde_json::json;

/// Environment variable carrying the parent-rendered skills section (ADR-43).
///
/// The parent renders the budgeted section once from its runtime-owned
/// `SkillsContext` and hands it over as-is, so the child's system prompt
/// matches the parent's without the child running discovery itself.
const SKILLS_SECTION_ENV: &str = "CONCERTO_AGENT_SKILLS_SECTION";

/// The process entry; all failures map onto exit codes (see module docs).
#[tokio::main]
async fn main() {
    std::process::exit(run().await);
}

/// Execute one task against the supervisor and return the process exit code.
async fn run() -> i32 {
    let agent_id = match std::env::var("CONCERTO_AGENT_ID") {
        Ok(id) if !id.is_empty() => id,
        _ => {
            eprintln!("agent-process: CONCERTO_AGENT_ID is required");
            return 1;
        }
    };
    let project_root = match std::env::var("CONCERTO_PROJECT_ROOT") {
        Ok(root) if !root.is_empty() => PathBuf::from(root),
        _ => {
            eprintln!("agent-process: CONCERTO_PROJECT_ROOT is required");
            return 1;
        }
    };
    if !project_root.is_dir() {
        eprintln!("agent-process: project root is not a directory: {project_root:?}");
        return 1;
    }
    let description = match std::env::var("CONCERTO_TASK_DESCRIPTION") {
        Ok(text) if !text.is_empty() => text,
        _ => {
            eprintln!("agent-process: CONCERTO_TASK_DESCRIPTION is required");
            return 1;
        }
    };
    let max_iterations = std::env::var("CONCERTO_MAX_ITERATIONS")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(25);
    // ADR-60 D7 ledger enrichment: a plan-driven run hands its approved plan
    // id to every child; the child mirrors it onto each gated write and its
    // terminal whiteboard events so `fold_ledger` can attribute them.
    let plan_id = std::env::var("CONCERTO_PLAN_ID").ok().filter(|plan_id| !plan_id.is_empty());
    // H39b step 3: the parent owns a per-run directory and passes the paths.
    // The child never touches the session database; it restores the accumulated
    // file and appends its own completed turns back to it.
    let history_path =
        std::env::var(HISTORY_ENV).ok().filter(|path| !path.is_empty()).map(PathBuf::from);
    let metrics_path =
        std::env::var(METRICS_ENV).ok().filter(|path| !path.is_empty()).map(PathBuf::from);
    let mut initial_messages = Vec::new();
    if let Some(path) = &history_path {
        match supervised_history::restore_for_child(path).await {
            Ok(messages) => {
                // A crash can leave an assistant tool call with no result in
                // the accumulated file. `restore_for_child` repairs that in
                // memory only; the file stays byte-identical so the parent's
                // seed-length harvest offset remains exact.
                initial_messages = messages;
            }
            Err(error) => eprintln!(
                "agent-process: failed to restore supervised history {}: {error}",
                path.display()
            ),
        }
    }

    // ADR-60 D1 orphan cleanup: on Linux, ask the kernel to SIGTERM this
    // process when its parent (the supervisor) dies. Best-effort — see
    // `install_parent_death_signal`.
    #[cfg(target_os = "linux")]
    install_parent_death_signal();

    // Resolve the provider before touching the supervisor: a missing config or
    // credential is a startup failure, and failing here (exit 1, no handshake)
    // keeps the supervisor's spawn accounting simple and the failure loud.
    let provider = match resolve_provider() {
        Ok(provider) => provider,
        Err(error) => {
            eprintln!("agent-process: {error}");
            return 1;
        }
    };

    // Bind to the supervisor: handshake (D2) then the tool registry (the
    // gate owns what the loop may present to the model).
    let client = match GateProxyClient::connect(agent_id.clone()).await {
        Ok(client) => client,
        Err(error) => {
            eprintln!("agent-process: supervisor connection failed: {error}");
            return 1;
        }
    };
    let client = Arc::new(tokio::sync::Mutex::new(client));
    let backend =
        match GateProxyBackend::new(client.clone(), agent_id.clone(), plan_id.clone()).await {
            Ok(backend) => Arc::new(backend),
            Err(error) => {
                eprintln!("agent-process: tool registry fetch failed: {error}");
                return 1;
            }
        };

    let bus = EventBus::default();
    // ADR-60 S5 approval bridge: the sink forwards each request to the
    // supervisor, which routes it to the same frontend approval surface the
    // in-process paths use. No answer (no sink, cancellation, transport
    // failure) denies — the fail-closed default, now because the channel said
    // so.
    let approval: Arc<dyn ApprovalSink> = Arc::new(ApprovalProxySink::new(client.clone()));
    let undo_manager = Arc::new(std::sync::Mutex::new(UndoManager::new(&project_root)));
    let eval = EvalEngine::new(&project_root);
    // ADR-43: the parent renders the skills section once and stamps it as an
    // env var; the child appends it verbatim to its system prompt. Absent or
    // empty means "no skills injected", exactly like a disabled config.
    let skills_section = std::env::var(SKILLS_SECTION_ENV).ok().filter(|s| !s.is_empty());
    let prompt_builder = PromptBuilder::with_skills(
        system_prompt_for(RequestedOutcome::Execute),
        skills_context_from_section(skills_section),
    );
    let memory = Arc::new(GateProxyMemoryStore::new(
        client.clone(),
        agent_id.clone(),
        ProjectId(agent_id.clone()),
    ));

    let mut agent = concerto_orchestrator::agent_loop::AgentLoop::with_project_root(
        bus,
        approval,
        provider,
        // The gate-proxy backend needs no local executor; the coercion keeps
        // the loop's seam (ADR-60 S5 executor call-site swap).
        backend.clone(),
        memory,
        undo_manager,
        eval,
        prompt_builder,
        max_iterations,
        false,
        project_root,
        None,
    )
    .with_initial_messages(initial_messages);
    let history_sink = Arc::new(Mutex::new(Vec::new()));
    let metrics_sink = Arc::new(Mutex::new(Vec::new()));
    if history_path.is_some() {
        agent = agent.with_message_sink(Some(history_sink.clone()));
    }
    if metrics_path.is_some() {
        agent = agent.with_provider_metrics_sink(Some(metrics_sink.clone()));
    }

    let task = AgentTask::new(Ulid::new(), description.clone());
    let cancel = CancellationToken::new();
    let flush_cancel = CancellationToken::new();
    let flush_handle = if history_path.is_some() || metrics_path.is_some() {
        Some(tokio::spawn(flush_periodically(
            history_path.clone(),
            history_sink.clone(),
            metrics_path.clone(),
            metrics_sink.clone(),
            flush_cancel.clone(),
        )))
    } else {
        None
    };

    let outcome = agent.run(task.clone(), cancel).await;
    flush_cancel.cancel();
    if let Some(handle) = flush_handle {
        if let Err(error) = handle.await {
            eprintln!("agent-process: supervised history flush task failed: {error}");
        }
    }
    if let Some(path) = &history_path {
        flush_turns(&history_sink, path).await;
    }
    if let Some(path) = &metrics_path {
        flush_metrics(&metrics_sink, path).await;
    }

    match outcome {
        Ok(output) => {
            let metrics = provider_metrics_payload(&output.provider_metrics, &agent_id);
            let mut event = terminal_event(
                &agent_id,
                &task,
                WhiteboardKind::SubtaskCompleted,
                json!({
                    "task_id": task.id.to_string(),
                    "status": "completed",
                    "provider_metrics": metrics,
                }),
            );
            event.plan_id = plan_id;
            publish_best_effort(&backend, event).await;
            eprintln!("agent-process: task completed");
            0
        }
        Err(error) => {
            eprintln!("agent-process: task failed: {error}");
            let mut event = terminal_event(
                &agent_id,
                &task,
                WhiteboardKind::Failure,
                json!({ "task_id": task.id.to_string(), "error": error.to_string() }),
            );
            event.plan_id = plan_id;
            publish_best_effort(&backend, event).await;
            1
        }
    }
}

/// Flush completed turns every 500 ms until cancelled, then once more.
async fn flush_periodically(
    history_path: Option<PathBuf>,
    history_sink: Arc<Mutex<Vec<Message>>>,
    metrics_path: Option<PathBuf>,
    metrics_sink: Arc<Mutex<Vec<ProviderMetrics>>>,
    cancel: CancellationToken,
) {
    let mut ticker = tokio::time::interval(Duration::from_millis(500));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            // Cleanup uses its own token, so a cancelled run still flushes
            // the turns it already completed.
            _ = cancel.cancelled() => break,
            _ = ticker.tick() => {}
        }
        if let Some(path) = &history_path {
            flush_turns(&history_sink, path).await;
        }
        if let Some(path) = &metrics_path {
            flush_metrics(&metrics_sink, path).await;
        }
    }
    if let Some(path) = &history_path {
        flush_turns(&history_sink, path).await;
    }
    if let Some(path) = &metrics_path {
        flush_metrics(&metrics_sink, path).await;
    }
}

/// Drain and append completed turns. A failed append logs and leaves the run
/// otherwise unaffected (the parent also sees the terminal event).
async fn flush_turns(sink: &Mutex<Vec<Message>>, path: &Path) {
    let batch = {
        let mut guard = match sink.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if guard.is_empty() {
            return;
        }
        std::mem::take(&mut *guard)
    };
    if let Err(error) = supervised_history::append_turns(path, &batch).await {
        eprintln!("agent-process: failed to append supervised history {}: {error}", path.display());
    }
}

/// Drain and append provider metrics.
async fn flush_metrics(sink: &Mutex<Vec<ProviderMetrics>>, path: &Path) {
    let batch = {
        let mut guard = match sink.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if guard.is_empty() {
            return;
        }
        std::mem::take(&mut *guard)
    };
    if let Err(error) = supervised_history::append_metrics(path, &batch).await {
        eprintln!("agent-process: failed to append supervised metrics {}: {error}", path.display());
    }
}

/// Compact additive terminal-event metrics. The IPC protocol is unchanged;
/// the supervisor reads this payload and writes the authoritative rows.
fn provider_metrics_payload(metrics: &[ProviderMetrics], agent_id: &str) -> serde_json::Value {
    json!(metrics
        .iter()
        .map(|metric| json!({
            "agent_id": agent_id,
            "provider": metric.provider,
            "model": metric.model,
            "tokens_in": metric.tokens_in,
            "tokens_out": metric.tokens_out,
            "cost_usd": metric.cost_usd,
            "latency_ms": metric.latency_ms,
        }))
        .collect::<Vec<_>>())
}

/// Ask Linux to deliver `SIGTERM` to this process when its parent exits
/// (ADR-60 D1 orphan cleanup). The supervisor spawns this binary directly —
/// no shell wrapper, no `setsid` (see `Supervisor::spawn_inner`) — so this
/// process *is* the direct child and `PR_SET_PDEATHSIG`, which survives
/// `execve`, fires exactly when the supervisor dies.
///
/// Best-effort by design: on failure we warn and continue rather than refuse
/// to start. Normal shutdown is unaffected (stdin-close → grace → SIGKILL
/// escalation lives supervisor-side), and the narrow startup race where the
/// supervisor dies before this call self-heals — the subsequent handshake
/// hits EOF on the dead pipes and `run` returns exit code 1.
#[cfg(target_os = "linux")]
fn install_parent_death_signal() {
    // SAFETY: `prctl(PR_SET_PDEATHSIG, SIGTERM)` sets a per-process kernel
    // option; it takes no pointers and touches none of our memory. This is
    // the binary's only `unsafe`, permitted because libc exposes no safe
    // wrapper for the call. The workspace denies `unsafe_code`; this scoped
    // allow is the deliberate, documented exception.
    #[allow(unsafe_code)]
    let result = unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) };
    if result != 0 {
        // No tracing subscriber is installed in this binary (module docs);
        // stderr is the diagnostic channel.
        eprintln!(
            "agent-process: prctl(PR_SET_PDEATHSIG) failed ({result}); orphan cleanup \
             degrades to stdio EOF detection"
        );
    }
}

/// Resolve the loop's provider.
///
/// `CONCERTO_PROVIDER=mock` is the explicit, documented opt-in (tests and
/// fixtures). Every other value — and an unset variable — selects the real
/// path: the parent's stamped [`AppConfig`] is parsed, the default provider is
/// resolved from it, and the provider is built through the credential store
/// (OS keychain, with the `CONCERTO_*_API_KEY` env fallback). There is no
/// mock fallback anywhere on this path.
fn resolve_provider() -> Result<Arc<dyn LlmProvider>, AgentProcessProviderError> {
    let provider_env = std::env::var(PROVIDER_ENV).ok();
    if agent_process_config::selects_mock(provider_env.as_deref()) {
        let script = std::env::var("CONCERTO_MOCK_SCRIPT_JSON").ok();
        return agent_process_config::build_mock_provider(script.as_deref())
            .map(|provider| Arc::new(provider) as Arc<dyn LlmProvider>);
    }
    let config_json = std::env::var(CONFIG_ENV).ok();
    let config = agent_process_config::parse_app_config(config_json.as_deref())?;
    let (provider_config, model_override) = agent_process_config::resolve_provider_config(&config)?;
    agent_process_config::build_provider(
        &provider_config,
        model_override.as_deref(),
        &CredentialStore::new(),
    )
}

/// Build a `SkillsContext` carrying the parent-rendered section verbatim.
///
/// Returns `None` when the parent injected nothing, so the prompt assembly is
/// byte-identical to a run with no skills (the section is never fabricated).
fn skills_context_from_section(
    section: Option<String>,
) -> Option<Arc<concerto_orchestrator::skills_context::SkillsContext>> {
    concerto_orchestrator::skills_context::SkillsContext::from_rendered_section(section)
}

/// A terminal whiteboard event describing this process's task outcome.
fn terminal_event(
    agent_id: &str,
    task: &AgentTask,
    kind: WhiteboardKind,
    payload: serde_json::Value,
) -> NewWhiteboardEvent {
    NewWhiteboardEvent {
        event_id: Ulid::new().to_string(),
        // The supervisor rebinds attribution to the registered process; the
        // agent_id here is informational.
        agent_id: agent_id.to_owned(),
        kind,
        scope: "task".to_owned(),
        session_id: Some(task.session_id.to_string()),
        plan_id: None,
        causation: None,
        payload,
        pre_image_hash: None,
        created_at: unix_ms(),
    }
}

/// Best-effort terminal event publish: the process is exiting either way.
async fn publish_best_effort(backend: &GateProxyBackend, event: NewWhiteboardEvent) {
    if let Err(error) = backend.publish_event(event).await {
        eprintln!("agent-process: failed to publish terminal whiteboard event: {error}");
    }
}

/// Unix epoch milliseconds (UTC).
fn unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}
