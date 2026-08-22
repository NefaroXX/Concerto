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
//! supervisor owns restarts.
//!
//! ## Environment contract
//!
//! | Variable | Meaning |
//! |----------|---------|
//! | `CONCERTO_AGENT_ID` | Registered agent identity (required). |
//! | `CONCERTO_PROJECT_ROOT` | Project directory the loop is scoped to (required). |
//! | `CONCERTO_TASK_DESCRIPTION` | The task objective (required). |
//! | `CONCERTO_MAX_ITERATIONS` | Loop iteration cap (default 25). |
//! | `CONCERTO_PROVIDER` | `mock` (the only wiring in the slice; default). |
//! | `CONCERTO_MOCK_SCRIPT_JSON` | Optional per-turn [`CompletionChunk`] script for the mock provider. |
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
//! - Real provider wiring (config/credentials) and skills injection.
//! - Interactive approval surfacing — the child's approval sink denies;
//!   approvals are a supervisor/UI concern in the new model.
//! - Memory stores/invalidations are supervisor-side (D6); the child's
//!   memory store is a retrieval facade.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use concerto_core::event::EventBus;
use concerto_core::ids::Ulid;
use concerto_core::memory::ProjectId;
use concerto_core::traits::approval::{ApprovalDecision, ApprovalSink};
use concerto_core::traits::provider::LlmProvider;
use concerto_core::types::{system_prompt_for, AgentTask, CompletionChunk};
use concerto_core::{CancellationToken, RequestedOutcome};
use concerto_eval::EvalEngine;
use concerto_orchestrator::gate_proxy::{GateProxyBackend, GateProxyClient, GateProxyMemoryStore};
use concerto_orchestrator::prompts::PromptBuilder;
use concerto_providers::mock::MockProvider;
use concerto_sessions::whiteboard::{NewWhiteboardEvent, WhiteboardKind};
use concerto_tools::undo::UndoManager;
use serde_json::json;

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
    let backend = match GateProxyBackend::new(client.clone(), agent_id.clone()).await {
        Ok(backend) => Arc::new(backend),
        Err(error) => {
            eprintln!("agent-process: tool registry fetch failed: {error}");
            return 1;
        }
    };

    let provider: Arc<dyn LlmProvider> = match std::env::var("CONCERTO_PROVIDER").as_deref() {
        Ok("mock") | Err(_) => match mock_provider() {
            Ok(provider) => Arc::new(provider),
            Err(error) => {
                eprintln!("agent-process: {error}");
                return 1;
            }
        },
        Ok(other) => {
            eprintln!(
                "agent-process: CONCERTO_PROVIDER={other} is not wired in the ADR-60 S5 slice \
                 (only \"mock\" is available)"
            );
            return 1;
        }
    };

    let bus = EventBus::default();
    let approval: Arc<dyn ApprovalSink> = Arc::new(DenyAllApprovalSink);
    let undo_manager = Arc::new(std::sync::Mutex::new(UndoManager::new(&project_root)));
    let eval = EvalEngine::new(&project_root);
    let prompt_builder = PromptBuilder::new(system_prompt_for(RequestedOutcome::Execute));
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
        None,
    );

    let task = AgentTask::new(Ulid::new(), description.clone());
    let cancel = CancellationToken::new();
    match agent.run(task.clone(), cancel).await {
        Ok(_output) => {
            let event = terminal_event(
                &agent_id,
                &task,
                WhiteboardKind::SubtaskCompleted,
                json!({ "task_id": task.id.to_string(), "status": "completed" }),
            );
            publish_best_effort(&backend, event).await;
            eprintln!("agent-process: task completed");
            0
        }
        Err(error) => {
            eprintln!("agent-process: task failed: {error}");
            let event = terminal_event(
                &agent_id,
                &task,
                WhiteboardKind::Failure,
                json!({ "task_id": task.id.to_string(), "error": error.to_string() }),
            );
            publish_best_effort(&backend, event).await;
            1
        }
    }
}

/// Build the mock provider, with an optional scripted conversation.
fn mock_provider() -> Result<MockProvider, String> {
    match std::env::var("CONCERTO_MOCK_SCRIPT_JSON") {
        Ok(script_json) => serde_json::from_str::<Vec<Vec<CompletionChunk>>>(&script_json)
            .map(MockProvider::scripted)
            .map_err(|error| {
                format!("CONCERTO_MOCK_SCRIPT_JSON is not a valid chunk script: {error}")
            }),
        Err(_) => Ok(MockProvider::default()),
    }
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

/// Child-side approval sink: approvals and user acknowledgments are a
/// supervisor/UI concern in the ADR-60 model (the gate owns policy, the
/// supervisor owns the UI surface). The child denies and logs the drop.
struct DenyAllApprovalSink;

impl DenyAllApprovalSink {
    fn deny(reason: &str) {
        eprintln!("agent-process: {reason}");
    }
}

#[async_trait::async_trait]
impl ApprovalSink for DenyAllApprovalSink {
    async fn request_approval(
        &self,
        _action: &concerto_core::types::PolicyAction<'_>,
        _cancel: CancellationToken,
    ) -> ApprovalDecision {
        Self::deny(
            "approval request dropped: interactive approvals are supervisor-side and not yet \
             wired (ADR-60 deferred)",
        );
        ApprovalDecision::Deny
    }

    async fn approve_all_for_session(&self, _session_id: Ulid, _cancel: CancellationToken) {
        Self::deny(
            "approve-all request dropped: interactive approvals are supervisor-side (ADR-60 \
             deferred)",
        );
    }

    async fn request_ack(&self, _message: &str, _cancel: CancellationToken) -> bool {
        Self::deny(
            "user acknowledgment request dropped: ack surfacing is supervisor-side (ADR-60 \
             deferred); aborting the current task",
        );
        false
    }
}
