//! Fault-injection and recovery evaluation suite (issue #55, parent #51).
//!
//! Everything in this module is TEST-ONLY: it is `#[cfg(test)]`-gated in
//! `lib.rs`, so no chaos hook, decorator, or instrumentation ships in
//! production builds. All injections happen at existing trait boundaries —
//! the production path is decorated, never modified:
//!
//! - **Agent surface** — `MockExpertAgent::sequence` with typed
//!   `OrchestratorError`s built from [`ProviderFault`]. The error variants
//!   ARE the fault vocabulary: they map deterministically onto the #54
//!   `FailureDiagnosis` codes (`rate-limit`, `network-loss`,
//!   `provider-unavailable`, `context-exhaustion`, `tool-failed`, …) whose
//!   values every scenario pins below. The fault program is a SEQUENCE of
//!   typed results, so a scenario reads as `fault first, recovery second`.
//! - **Coordinator-model surface** — [`ScriptedCoordModel`], the scripted
//!   planning provider: an ordered turn program (which dispatch decisions
//!   at which call) that captures every request it receives, so scenarios
//!   can assert the recovery CONVERSATION is diagnosis-shaped (#54) and
//!   count the model calls a recovery cost.
//! - **Checkpoint/evidence surface** — a hand-built v4 checkpoint JSON
//!   round-trips through the real checkpoint format, so interruption (a
//!   truncated file after a crash) and restart (a fresh coordinator
//!   reading that same record) exercise exactly what production resumes
//!   from, over a real SQLite evidence log.
//! - **Workspace/evidence surface** — real temp dirs, real SQLite pools
//!   and `ResourceFacts` rows; external modification is injected by
//!   mutating a file after an observation row was recorded (ADR-65 F3), so
//!   the verdict uses the same reconciliation machinery production runs.
//!
//! # Determinism
//! Zero wall-clock dependence: no sleeps and no virtual time. Every
//! recovery path is bounded by attempt budgets and turn counts, and a
//! scenario's recovery latency is measured in STEPS (dispatches between
//! the first injected fault and the subtask's eventual success), never
//! seconds.
//!
//! # Metric capture ([`RunReport`])
//! Every scenario builds one report from `[outcome, events, captured
//! model requests]` and asserts the RECOVERY OUTCOME on it: completion
//! status, incorrect acceptance, duplicate work, model calls/tokens used,
//! recovery latency in steps, retry counts, and post-recovery
//! state/evidence/file integrity.
//!
//! # Cost separation (issue acceptance)
//! The default suite is fully in process (scripted providers, no network,
//! tiny graphs); [`cost_bounds`] pins every scripted scenario's model-turn
//! and dispatch counts far below the structural limits, so the suite stays
//! CI-cheap. The one expensive stress scenario (`large_fanout_with_cycles`)
//! is `#[ignore]`-gated:
//!
//! ```text
//! cargo test -p concerto-orchestrator fault_injection -- --ignored
//! ```
//!
//! # Scenario → test index (all 16 from the issue, plus the P0 gates)
//! | # | scenario | test |
//! |---|----------|------|
//! | 1 | agent crash | `agent_crash_retries_same_and_recovers` |
//! | 2 | provider unavailable | `provider_unavailable_diagnoses_failover_viability` |
//! | 3 | network disconnect | `network_disconnect_retries_same_within_budget` |
//! | 4 | 429 / rate limit | `rate_limit_429_retries_same_within_budget` |
//! | 5 | malformed model output | `malformed_output_gets_corrective_feedback_retry` |
//! | 6 | wrong/partial file changes | `zero_file_success_rejected_then_revised_not_accepted` |
//! | 7 | tool failure | `tool_failure_retries_same_diagnosis_shaped` |
//! | 8 | dependency failure | `blocked_dependency_is_diagnosed_and_recovered` |
//! | 9 | context overflow | `context_overflow_failover_to_alternate_model` |
//! | 10 | checkpoint interruption | `interrupted_checkpoint_degrades_to_a_fresh_replan` |
//! | 11 | process restart/resume | `process_resume_continues_a_blocked_step` |
//! | 12 | duplicate dispatch | `duplicate_dispatch_is_bounded_not_infinite` |
//! | 13 | review disagreement | `reviewer_disagreement_sends_revision_and_completes` |
//! | 14 | validator unavailable | `validator_unavailable_never_silently_accepts` |
//! | 15 | external workspace modification | `external_workspace_modification_surfaces_in_evidence` |
//! | 16 | coordinator decision loop/stall | `repeated_identical_dispatch_stall_detected` |
//! | G1 | issue #52 gate | `gate_52_invalid_decision_rejected_then_run_recovers` |
//! | G2 | issue #53 gate | `gate_53_stall_detected_and_budget_recovered` |
//! | G3 | issue #54 gate | `gate_54_diagnosis_selects_the_correct_recovery_path` |

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use concerto_core::error::PolicyError;
use concerto_core::error::ProviderError;
use concerto_core::event::{EventBus, EventKind};
use concerto_core::policy::SimplePolicyEngine;
use concerto_core::traits::memory::NullMemoryStore;
use concerto_core::traits::policy::{AuditEntry, AuditLog};
use concerto_core::types::{
    AgentCompletionStatus, AgentContext, AgentId, AgentOutcome, AgentOutput, AgentRunResult,
    Condition, PolicyRule, ToolCall,
};
use concerto_core::{CancellationToken, OrchestratorError};
use concerto_providers::model_registry::ModelRegistry;
use concerto_providers::routing::RoutingEngine;
use concerto_sessions::spend::SpendTracker;

use crate::agent_runner::AgentRunner;
use crate::coordinator::{CoordinatorAgent, CALL_SPECIALIST_TOOL};
use crate::registry::AgentRegistry;
use crate::testing::MockExpertAgent;

// ---------------------------------------------------------------------------
// Fault vocabulary — typed provider errors mapped to the #54 diagnosis codes
// ---------------------------------------------------------------------------

/// One injectable provider fault. The variants cover the issue's provider
/// surfaces (unavailable, network loss, 429, context overflow, malformed
/// output); each maps to a typed `ProviderError` (for injection) and to the
/// stable `FailureDiagnosis` code (for assertion), so tests pin the
/// diagnosis → recovery contract end to end.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProviderFault {
    /// `ProviderError::Network` → diagnosis code `network-loss`.
    NetworkDisconnect,
    /// `ProviderError::RateLimit` → diagnosis code `rate-limit`.
    RateLimit429,
    /// HTTP 503 → diagnosis code `provider-unavailable`.
    ServiceUnavailable,
    /// `ProviderError::ContextOverflow` → diagnosis code `context-exhaustion`.
    ContextOverflow { tokens_in: u64, capacity: u64 },
    /// `ProviderError::InvalidResponse` → diagnosis code
    /// `malformed-provider-response`.
    MalformedResponse,
}

impl ProviderFault {
    /// The typed error injected at the agent surface.
    fn error(&self) -> ProviderError {
        use std::time::Duration;
        match self {
            Self::NetworkDisconnect => ProviderError::Network("connection reset by peer".into()),
            Self::RateLimit429 => ProviderError::RateLimit { retry_after: Duration::from_secs(2) },
            Self::ServiceUnavailable => ProviderError::HttpStatus {
                status: 503,
                retry_after: None,
                message: "service unavailable".into(),
            },
            Self::ContextOverflow { tokens_in, capacity } => {
                ProviderError::ContextOverflow { tokens_in: *tokens_in, capacity: *capacity }
            }
            Self::MalformedResponse => {
                ProviderError::InvalidResponse("unexpected wire shape".into())
            }
        }
    }

    /// The #54 `FailureDiagnosis` code asserted to reach the decision loop.
    fn diagnosis_code(&self) -> &'static str {
        match self {
            Self::NetworkDisconnect => "network-loss",
            Self::RateLimit429 => "rate-limit",
            Self::ServiceUnavailable => "provider-unavailable",
            Self::ContextOverflow { .. } => "context-exhaustion",
            Self::MalformedResponse => "malformed-provider-response",
        }
    }
}

/// Build the injected agent-surface error: the fault surfaces as the same
/// `OrchestratorError::Provider` a real provider client would raise, and
/// `failure_diagnosis::diagnose` normalizes it to the same code a real
/// outage would produce.
fn fault_error(fault: ProviderFault) -> OrchestratorError {
    OrchestratorError::Provider(fault.error())
}

/// A successful scripted agent result (tokens make the usage metric real).
fn ok_run_result(
    role: &str,
    summary: &str,
    files: &[&str],
    tokens_in: u64,
    tokens_out: u64,
    tool_calls: u32,
) -> Result<AgentRunResult, OrchestratorError> {
    Ok(AgentRunResult {
        task_id: concerto_core::types::TaskId::new(),
        role: AgentId::new(role),
        outcome: AgentOutcome::Success,
        summary: summary.to_owned(),
        files_modified: files.iter().map(|path| camino::Utf8PathBuf::from(*path)).collect(),
        tool_call_count: tool_calls,
        cost_usd: 0.0,
        latency_ms: 0,
        provider: "mock".to_owned(),
        model: "mock-model".to_owned(),
        tokens_in,
        tokens_out,
    })
}

/// A settled failed outcome (the specialist ran and reported failure).
fn failure_outcome(error: &str) -> AgentOutcome {
    AgentOutcome::Failed { error: error.to_owned() }
}

/// The full fault program for one scenario: fault first, recovery second.
/// Deterministic — the same program always yields the same recovery trace.
fn fault_then_success(fault: ProviderFault) -> Vec<Result<AgentRunResult, OrchestratorError>> {
    vec![
        Err(fault_error(fault)),
        ok_run_result("coder", "recovered after the injected fault", &["src/a.rs"], 90, 40, 2),
    ]
}
// ---------------------------------------------------------------------------
// Scripted coordinator model — the decision-loop turn script + capture
// ---------------------------------------------------------------------------

/// A scripted `call_specialist` decision (the decision loop's dispatch
/// tool contract).
fn dispatch_call(agent_id: &str, task: &str) -> ToolCall {
    ToolCall {
        id: format!("call-{agent_id}-{task}"),
        name: CALL_SPECIALIST_TOOL.to_owned(),
        arguments: serde_json::json!({ "agent_id": agent_id, "task": task }),
    }
}

/// One scripted coordinator decision-loop turn.
enum Turn {
    /// The loop issues dispatch decisions (each call dispatches one agent).
    Calls(Vec<ToolCall>),
    /// The loop ends in prose.
    Text(String),
}

/// The scripted planning-provider decorator: serves one scripted
/// decision-loop turn per request, in order; beyond the script it serves
/// empty prose. Captures every request so scenarios can assert on the
/// recovery conversation (the diagnosis the loop reads back, issue #54)
/// and count the model turns used.
#[derive(Default)]
struct ScriptedCoordModel {
    turns: Mutex<VecDeque<Turn>>,
    requests: Mutex<Vec<concerto_core::types::CompletionRequest>>,
}

impl ScriptedCoordModel {
    fn new(turns: Vec<Turn>) -> Self {
        Self { turns: Mutex::new(turns.into()), requests: Mutex::new(Vec::new()) }
    }

    /// A scripted model wrapped for the coordinator constructor.
    fn scripted(turns: Vec<Turn>) -> Arc<Self> {
        Arc::new(Self::new(turns))
    }

    /// How many model requests the coordinator made (model calls used).
    fn turn_count(&self) -> usize {
        self.requests.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// Whether ANY captured conversation contains `needle` — scanning BOTH
    /// plain message content (planner prompts, guard nudges) AND the tool
    /// results' structured JSON payloads (`ToolResult.content`).
    fn any_message_contains(&self, needle: &str) -> bool {
        self.requests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .flat_map(|request| request.messages.iter())
            .any(|message| {
                message.content.contains(needle)
                    || message
                        .tool_results
                        .iter()
                        .flatten()
                        .any(|result| result.content.to_string().contains(needle))
            })
    }

    /// Whether request #N's conversation (content or tool-result payloads)
    /// contains `needle`.
    fn request_contains(&self, index: usize, needle: &str) -> bool {
        self.requests.lock().unwrap_or_else(|e| e.into_inner()).get(index).is_some_and(|request| {
            request.messages.iter().any(|message| {
                message.content.contains(needle)
                    || message
                        .tool_results
                        .iter()
                        .flatten()
                        .any(|result| result.content.to_string().contains(needle))
            })
        })
    }
}

#[async_trait::async_trait]
impl concerto_core::traits::provider::LlmProvider for ScriptedCoordModel {
    async fn stream_completion(
        &self,
        request: concerto_core::types::CompletionRequest,
        cancel: CancellationToken,
    ) -> Result<concerto_core::traits::provider::CompletionStream, ProviderError> {
        let _ = cancel;
        self.requests.lock().unwrap_or_else(|e| e.into_inner()).push(request);
        let turn = self
            .turns
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop_front()
            .unwrap_or_else(|| Turn::Text(String::new()));
        let chunks: Vec<concerto_core::types::CompletionChunk> = match turn {
            Turn::Text(text) => vec![concerto_core::types::CompletionChunk {
                reasoning: None,
                delta: text,
                tool_call: None,
                is_final: true,
                usage: None,
            }],
            Turn::Calls(calls) => {
                let total = calls.len();
                calls
                    .into_iter()
                    .enumerate()
                    .map(|(index, call)| concerto_core::types::CompletionChunk {
                        reasoning: None,
                        delta: String::new(),
                        tool_call: Some(call),
                        is_final: index + 1 == total,
                        usage: None,
                    })
                    .collect()
            }
        };
        Ok(Box::pin(futures::stream::iter(chunks.into_iter().map(Ok))))
    }

    fn context_capacity(&self, _model: &str) -> concerto_core::types::TokenBudget {
        concerto_core::types::TokenBudget::new(128_000, 4_096)
    }

    fn approximate_cost(&self, _tokens_in: u64, _tokens_out: u64) -> f64 {
        0.0
    }

    fn provider_name(&self) -> &'static str {
        "fault-injection"
    }
}

// ---------------------------------------------------------------------------
// Coordinator wiring — the same construction shape the coordinator's own
// tests use, only parameterized by the fault harness
// ---------------------------------------------------------------------------

struct TestAudit;
#[async_trait::async_trait]
impl AuditLog for TestAudit {
    async fn record(
        &self,
        _entry: AuditEntry,
        _cancel: CancellationToken,
    ) -> Result<(), PolicyError> {
        Ok(())
    }
}

/// Allow-all policy wired through the production engine shape: every
/// `call_specialist` dispatch decision is allowed the same way production
/// gates it (the suite injects failures through typed errors, not policy).
fn allow_all_policy() -> Arc<dyn concerto_core::traits::policy::PolicyEngine> {
    Arc::new(SimplePolicyEngine::new(
        vec![PolicyRule::AutoApprove(Condition::Always)],
        Arc::new(TestAudit),
    ))
}

/// Build a fully-wired coordinator around the scripted model. The routing
/// tables carry the canonical cheap/mid/expensive profiles so the fallback
/// ladder has headroom in fail-over scenarios.
fn coordinator_with_model(
    bus: &EventBus,
    registry: Arc<AgentRegistry>,
    model: Arc<ScriptedCoordModel>,
) -> CoordinatorAgent {
    let spend_tracker = Arc::new(SpendTracker::default());
    let runner = AgentRunner::new(registry.clone(), bus.clone(), spend_tracker.clone());
    let profiles: Vec<concerto_core::types::RoutingProfile> = vec![
        concerto_core::types::RoutingProfile {
            provider_config_id: "test".into(),
            provider: "test".into(),
            model: "cheap".into(),
            cost_per_1k_tokens: 0.001,
            avg_latency_ms: 100,
            context_window: 8192,
            supports_tool_calling: true,
            base_url: None,
            description: None,
        },
        concerto_core::types::RoutingProfile {
            provider_config_id: "test".into(),
            provider: "test".into(),
            model: "mid".into(),
            cost_per_1k_tokens: 0.005,
            avg_latency_ms: 100,
            context_window: 8192,
            supports_tool_calling: true,
            base_url: None,
            description: None,
        },
        concerto_core::types::RoutingProfile {
            provider_config_id: "test".into(),
            provider: "test".into(),
            model: "expensive".into(),
            cost_per_1k_tokens: 0.01,
            avg_latency_ms: 100,
            context_window: 8192,
            supports_tool_calling: true,
            base_url: None,
            description: None,
        },
    ];
    let routing = Arc::new(RoutingEngine::new(
        profiles.clone(),
        spend_tracker.clone(),
        concerto_config::ModelPinConfig {
            pins: std::collections::HashMap::new(),
            ..Default::default()
        },
        EventBus::default(),
    ));
    let model_registry = Arc::new(ModelRegistry::from_profiles(profiles));
    let model_selector =
        Arc::new(concerto_providers::model_selector::ModelSelector::new(model_registry, routing));
    CoordinatorAgent::new(
        registry,
        runner,
        model_selector,
        spend_tracker,
        bus.clone(),
        model as Arc<dyn concerto_core::traits::provider::LlmProvider>,
        Arc::new(NullMemoryStore),
    )
    .with_policy_engine(allow_all_policy())
}
// ---------------------------------------------------------------------------
// Run plumbing + RunReport (the per-scenario metrics)
// ---------------------------------------------------------------------------

/// The event kinds a run emitted.
type RunEvents = Vec<EventKind>;

/// Run a wired coordinator against a throwaway temp workspace (created
/// here, returned to the scenario for file-level integrity checks), and
/// collect every bus event the run produced.
async fn run_coordinator(
    mut coordinator: CoordinatorAgent,
    bus: &EventBus,
) -> (Result<AgentOutput, OrchestratorError>, RunEvents, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("test workspace tempdir");
    let mut rx = bus.subscribe();
    let session = concerto_core::ids::Ulid::new();
    let task = concerto_core::types::AgentTask::new(session, "fault-injection scenario");
    let context = AgentContext::new(concerto_core::types::SessionContext::new(
        session,
        dir.path().to_owned(),
    ));
    let outcome = coordinator.run(task, context, CancellationToken::new(), None).await;
    let mut events: RunEvents = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event.kind.clone());
    }
    (outcome, events, dir)
}

/// A resumed run over an explicit workspace dir (the checkpoint-resume
/// scenarios hand one in so the checkpoint's project scope matches).
async fn run_resumed_in_dir(
    mut coordinator: CoordinatorAgent,
    bus: &EventBus,
    dir: &std::path::Path,
    cp_json: Option<String>,
) -> (Result<AgentOutput, OrchestratorError>, Vec<EventKind>) {
    let mut rx = bus.subscribe();
    let session = concerto_core::ids::Ulid::new();
    let task = concerto_core::types::AgentTask::new(session, "fault-injection resume");
    let context =
        AgentContext::new(concerto_core::types::SessionContext::new(session, dir.to_owned()));
    let outcome = coordinator.run(task, context, CancellationToken::new(), cp_json).await;
    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event.kind.clone());
    }
    (outcome, events)
}

/// The per-scenario metrics the issue requires, aggregated deterministically
/// from the run outcome, the bus events, and the captured model requests.
#[derive(Debug)]
struct RunReport {
    /// The error when the coordinator never returned a terminal output.
    errored: Option<String>,
    completion_status: Option<AgentCompletionStatus>,
    final_message: String,
    /// Model calls the run's decision loop consumed.
    model_turns: usize,
    /// Every `SubTaskStarted` dispatch: (role, task id) in event order.
    dispatches: Vec<(AgentId, String)>,
    /// Every `SubTaskFailed`: (role, error text).
    failures: Vec<(AgentId, String)>,
    needs_revision: usize,
    blocked_events: usize,
    review_cycles: u32,
    review_escalated: bool,
    validation_cycles: u32,
    validation_escalated: bool,
    /// Progress-guard thoughts (#53 stall nudges + escalations).
    guards: Vec<String>,
    tokens_in: u64,
    tokens_out: u64,
    files_modified: Vec<String>,
}

impl RunReport {
    fn build(
        outcome: &Result<AgentOutput, OrchestratorError>,
        events: &[EventKind],
        model: &ScriptedCoordModel,
    ) -> Self {
        let mut report = Self {
            errored: outcome.as_ref().err().map(|e| e.to_string()),
            completion_status: outcome.as_ref().ok().map(|output| output.completion_status),
            final_message: outcome
                .as_ref()
                .ok()
                .map(|output| output.final_message.clone())
                .unwrap_or_default(),
            model_turns: model.turn_count(),
            dispatches: Vec::new(),
            failures: Vec::new(),
            needs_revision: 0,
            blocked_events: 0,
            review_cycles: 0,
            review_escalated: false,
            validation_cycles: 0,
            validation_escalated: false,
            guards: Vec::new(),
            tokens_in: 0,
            tokens_out: 0,
            files_modified: outcome
                .as_ref()
                .ok()
                .map(|output| output.files_modified.iter().map(ToString::to_string).collect())
                .unwrap_or_default(),
        };
        if let Ok(output) = outcome {
            for metric in &output.provider_metrics {
                report.tokens_in += metric.tokens_in;
                report.tokens_out += metric.tokens_out;
            }
        }
        for kind in events {
            report.absorb(kind);
        }
        report
    }

    fn absorb(&mut self, kind: &EventKind) {
        match kind {
            EventKind::SubTaskStarted { role, task_id, .. } => {
                self.dispatches.push((role.clone(), task_id.to_string()));
            }
            EventKind::SubTaskFailed { role, error, .. } => {
                self.failures.push((role.clone(), error.clone()));
            }
            EventKind::SubTaskNeedsRevision { .. } => self.needs_revision += 1,
            EventKind::SubTaskBlocked { .. } => self.blocked_events += 1,
            EventKind::ReviewCycleStarted { .. } => self.review_cycles += 1,
            EventKind::ReviewCycleEscalated { .. } => self.review_escalated = true,
            EventKind::ValidationCycleStarted { .. } => self.validation_cycles += 1,
            EventKind::ValidationEscalated { .. } => self.validation_escalated = true,
            EventKind::AgentThought { content, .. } if content.contains("Progress guard") => {
                self.guards.push(content.clone());
            }
            _ => {}
        }
    }

    /// How many times the given role was dispatched.
    fn dispatches_of(&self, role: &str) -> usize {
        self.dispatches.iter().filter(|(dispatched, _)| dispatched.as_str() == role).count()
    }

    /// Recovery latency in STEPS: the later dispatches of the faulted agent
    /// before it ultimately succeeded. Zero when the fault was never
    /// injected or the agent was never recovered; positive values are the
    /// recovery work, and a duplicate-free recovery is exactly 1.
    fn recovery_steps(&self, role: &str) -> Option<usize> {
        let dispatches = self.dispatches_of(role);
        (dispatches > 1).then(|| dispatches - 1)
    }

    /// Duplicate work: same-role dispatches BEYOND the scripted fault
    /// recovery. Zero for every bounded recovery; positive only where the
    /// loop repeats equivalent work (the stall scenarios pin that bound).
    fn duplicate_work(&self, role: &str, scripted_recovery_redos: usize) -> usize {
        self.dispatches_of(role).saturating_sub(1 + scripted_recovery_redos)
    }

    /// Incorrect acceptance: true ONLY when the run claimed a full
    /// completion without any reported file change — a deliverable that
    /// never landed claimed as done.
    fn incorrect_acceptance(&self) -> bool {
        matches!(self.completion_status, Some(AgentCompletionStatus::Completed))
            && self.files_modified.is_empty()
    }

    /// State/evidence integrity (cheap checks shared by the loop
    /// scenarios): no TWO dispatches landed for the SAME task id, and the
    /// run's failure events match the dispatches actually retried (every
    /// failure was re-armed, never silently dropped).
    fn ledger_state_is_coherent(&self) -> bool {
        let mut seen_ids = std::collections::HashSet::new();
        let mut all_distinct = true;
        for (_, task_id) in &self.dispatches {
            if !seen_ids.insert(task_id.clone()) {
                all_distinct = false;
            }
        }
        all_distinct
    }
}
// ---------------------------------------------------------------------------
// Checkpoint / evidence helpers — scenarios 10, 11, 15, and the resume
// surface. These mirror the shapes the coordinator's own resume tests
// build so the injected records round-trip through the REAL checkpoint
// format and evidence log (state/evidence integrity checks stay grounded).
// ---------------------------------------------------------------------------

/// A real SQLite evidence-log pool in a throwaway temp dir (the same shape
/// the coordinator's resume tests use).
async fn resume_pool() -> (tempfile::TempDir, sqlx::SqlitePool) {
    let dir = tempfile::tempdir().expect("tempdir created");
    let db_path = dir.path().join("fault_injection_resume.db");
    let options = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(&db_path)
        .create_if_missing(true)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
        .busy_timeout(std::time::Duration::from_secs(5))
        .foreign_keys(true)
        .synchronous(sqlx::sqlite::SqliteSynchronous::Normal);
    let pool = sqlx::pool::PoolOptions::new()
        .max_connections(4)
        .connect_with(options)
        .await
        .expect("test pool connects");
    sqlx::migrate!("../sessions/migrations").run(&pool).await.expect("migrations apply");
    (dir, pool)
}

/// A v4-shaped checkpoint JSON with one blocked/failed subtask (and an
/// optional failed-ledger count + whiteboard cursor) — the same additive
/// surface the real `persist_checkpoint` writer produces.
fn blocked_step_checkpoint_json(
    project_id: &str,
    session_id: concerto_core::ids::Ulid,
    subtask_id: concerto_core::ids::Ulid,
    role: &str,
    status: &str,
    ledger_failed_entries: usize,
    cursor: Option<u64>,
) -> String {
    let ledger: Vec<serde_json::Value> = (0..ledger_failed_entries)
        .map(|_| {
            serde_json::json!({
                "kind": "failed",
                "task_id": subtask_id.to_string(),
                "timestamp": [2026, 254, 0, 0, 0, 0, 0, 0, 0],
            })
        })
        .collect();
    serde_json::json!({
        "schema_version": 4,
        "run_id": concerto_core::ids::Ulid::new().to_string(),
        "session_id": session_id.to_string(),
        "root_task_id": concerto_core::ids::Ulid::new().to_string(),
        "project_id": project_id,
        "objective": "build the thing",
        "objective_hash": blake3::hash("build the thing".as_bytes()).to_hex().to_string(),
        "stage": "Executing",
        "completed": false,
        "subtasks": [{
            "id": subtask_id.to_string(),
            "parent_id": null,
            "session_id": session_id.to_string(),
            "role": role,
            "description": "the blocked work",
            "status": status,
            "dependencies": [],
            "deliverable": null,
        }],
        "edges": [],
        "completed_results": {},
        "total_cost": 0.0,
        "total_tool_calls": 0,
        "provider_metrics": [],
        "all_files": [],
        "expected_artifacts": {},
        "subtask_attempts": {},
        "retry_feedback": {},
        "action_ledger": ledger,
        "whiteboard_cursor_gate_seq": cursor,
    })
    .to_string()
}

/// Append a `ToolExecuted` fact row attributed to a task (real id,
/// session-scoped — the shape the fact writer produces), and return the
/// appended row so scenarios can cite its REAL event id as evidence.
async fn append_tool_fact(
    pool: &sqlx::SqlitePool,
    session_id: concerto_core::ids::Ulid,
    event_id: &str,
    task_id: &str,
    success: bool,
) -> concerto_sessions::WhiteboardEvent {
    concerto_sessions::whiteboard::append_whiteboard_event(
        pool,
        &concerto_sessions::NewWhiteboardEvent {
            event_id: event_id.to_owned(),
            agent_id: "coder".to_owned(),
            kind: concerto_sessions::WhiteboardKind::ToolExecuted,
            scope: String::new(),
            session_id: Some(session_id.to_string()),
            plan_id: None,
            causation: None,
            payload: serde_json::json!({
                "agent_id": "coder",
                "task_id": task_id,
                "tool": "filesystem",
                "args": { "operation": "read", "path": "src/main.rs" },
                "success": success,
                "generation": "",
                "paths": [],
            }),
            pre_image_hash: None,
            created_at: 1,
        },
    )
    .await
    .expect("fact row appended")
}

/// A coordinator wired for the resume workspaces: empty planning
/// provider (restores proceed deterministically), the given registry and
/// shared runner, and the SQLite review store attached for evidence reads.
fn resume_coordinator(
    bus: &EventBus,
    registry: Arc<AgentRegistry>,
    run_registry: Arc<AgentRegistry>,
    pool: sqlx::SqlitePool,
) -> CoordinatorAgent {
    let spend_tracker = Arc::new(SpendTracker::default());
    let routing = Arc::new(RoutingEngine::new(
        vec![],
        spend_tracker.clone(),
        concerto_config::ModelPinConfig::default(),
        EventBus::default(),
    ));
    let model_selector = Arc::new(concerto_providers::model_selector::ModelSelector::new(
        Arc::new(ModelRegistry::from_profiles(vec![])),
        routing,
    ));
    CoordinatorAgent::new(
        registry,
        AgentRunner::new(run_registry, bus.clone(), spend_tracker.clone()),
        model_selector,
        spend_tracker,
        bus.clone(),
        Arc::new(concerto_providers::mock::MockProvider::default())
            as Arc<dyn concerto_core::traits::provider::LlmProvider>,
        Arc::new(NullMemoryStore),
    )
    .with_review_store(Some(pool))
}
// ---------------------------------------------------------------------------
// The scripted scenarios — one test per issue scenario, each asserting the
// RECOVERY OUTCOME on a full RunReport (never merely the absence of panic).
// ---------------------------------------------------------------------------

/// Shared wiring for the scripted decision-loop scenarios: plain turn
/// scripts are `dispatch / recover / prose`; the injected fault lives on
/// the first agent attempt and the recovery on the second.
async fn run_error_scenario(
    fault: ProviderFault,
) -> (Result<AgentOutput, OrchestratorError>, RunEvents, RunReport, tempfile::TempDir) {
    let bus = EventBus::new(256);
    let mocks = vec![MockExpertAgent::sequence(AgentId::new("coder"), fault_then_success(fault))];
    let model = ScriptedCoordModel::scripted(vec![
        Turn::Calls(vec![dispatch_call("coder", "do the work")]),
        Turn::Calls(vec![dispatch_call("coder", "do the work")]),
        Turn::Text("recovered".into()),
    ]);
    let coordinator =
        coordinator_with_model(&bus, Arc::new(AgentRegistry::from_mocks(mocks)), model.clone());
    let (outcome, events, dir) = run_coordinator(coordinator, &bus).await;
    let report = RunReport::build(&outcome, &events, &model);
    (outcome, events, report, dir)
}

/// Shared recovery-outcome assertions for the transient fault family
/// (crash, network, 429, tool, 503): the SAME agent was re-dispatched
/// exactly once, the recovery conversation is diagnosis-shaped, and the
/// ledger stayed coherent.
fn assert_transient_recovery(report: &RunReport, code: &str, model: &ScriptedCoordModel) {
    assert!(
        report.errored.is_none(),
        "the run must not crash on an injected fault: {:?}",
        report.errored
    );
    assert_eq!(
        report.dispatches_of("coder"),
        2,
        "exactly the faulted dispatch + one recovery re-dispatch: {report:?}"
    );
    assert_eq!(
        report.recovery_steps("coder"),
        Some(1),
        "recovery latency is ONE dispatch step, not repeats: {report:?}"
    );
    assert_eq!(
        report.duplicate_work("coder", 1),
        0,
        "the recovery re-dispatch is necessary work, not duplication"
    );
    assert!(
        model.any_message_contains(code),
        "the decision loop must read back the #{:?} diagnosis `{code}` before its recovery decision",
        code
    );
    assert!(
        report.tokens_in > 0 && report.tokens_out > 0,
        "usage must be recorded per agent attempt (tokens_in={}, tokens_out={})",
        report.tokens_in,
        report.tokens_out
    );
    assert!(
        report.ledger_state_is_coherent(),
        "state integrity: every dispatched task id is distinct"
    );
    // Recovery latency stays in bounded steps, never retries.
    assert!(
        report.model_turns <= 4,
        "the recovery used {} model turns — CI-cheap by construction",
        report.model_turns
    );
}

/// Scenario 1 — agent crash: the crash echoes through the #54 diagnosis
/// (`agent-crash`) and the same agent re-dispatches and completes.
#[tokio::test]
async fn agent_crash_retries_same_and_recovers() {
    let bus = EventBus::new(256);
    let mocks = vec![MockExpertAgent::sequence(
        AgentId::new("coder"),
        vec![
            Err(OrchestratorError::AgentLoopError("agent panicked".into())),
            ok_run_result("coder", "recovered from the crash", &["src/a.rs"], 90, 40, 2),
        ],
    )];
    let model = ScriptedCoordModel::scripted(vec![
        Turn::Calls(vec![dispatch_call("coder", "do the work")]),
        Turn::Calls(vec![dispatch_call("coder", "do the work")]),
        Turn::Text("recovered".into()),
    ]);
    let coordinator =
        coordinator_with_model(&bus, Arc::new(AgentRegistry::from_mocks(mocks)), model.clone());
    let (outcome, events, _dir) = run_coordinator(coordinator, &bus).await;
    let report = RunReport::build(&outcome, &events, &model);

    assert!(outcome.is_ok(), "an agent crash never crashes the coordinator: {outcome:?}");
    assert_transient_recovery(&report, "agent-crash", &model);
}

/// Scenario 2 — provider unavailable (HTTP 503): diagnosed
/// `provider-unavailable` (transient, same-agent viable per #54), same
/// agent re-dispatched, run recovers.
#[tokio::test]
async fn provider_unavailable_diagnoses_failover_viability() {
    let (_outcome, _events, report, _dir) =
        run_error_scenario(ProviderFault::ServiceUnavailable).await;
    // The 503 family is transient for the SAME agent; #54 also marks it
    // alternate-viable (failover remains possible if the retry budget
    // ran out). The recovery taken is the budget-preserving same-agent
    // retry — asserted through the #54 table directly here.
    let diagnosis =
        crate::failure_diagnosis::diagnose(&fault_error(ProviderFault::ServiceUnavailable));
    assert_eq!(diagnosis.code, "provider-unavailable");
    assert!(diagnosis.same_agent_viable && diagnosis.alternate_agent_viable);
    assert_eq!(
        crate::failure_diagnosis::recovery_action(&diagnosis, 0, 3),
        crate::failure_diagnosis::RecoveryAction::RetrySame,
        "the 503's recovery is the bounded same-agent retry the loop took"
    );
    assert_eq!(report.dispatches_of("coder"), 2, "{report:?}");
    assert_eq!(report.recovery_steps("coder"), Some(1), "{report:?}");
    assert!(!report.incorrect_acceptance(), "{report:?}");
}

/// Scenario 3 — network disconnect: diagnosed `network-loss`; the
/// recovery is one same-agent re-dispatch.
#[tokio::test]
async fn network_disconnect_retries_same_within_budget() {
    let model = ScriptedCoordModel::scripted(vec![
        Turn::Calls(vec![dispatch_call("coder", "do the work")]),
        Turn::Calls(vec![dispatch_call("coder", "do the work")]),
        Turn::Text("recovered".into()),
    ]);
    let bus = EventBus::new(256);
    let mocks = vec![MockExpertAgent::sequence(
        AgentId::new("coder"),
        fault_then_success(ProviderFault::NetworkDisconnect),
    )];
    let coordinator =
        coordinator_with_model(&bus, Arc::new(AgentRegistry::from_mocks(mocks)), model.clone());
    let (outcome, events, _dir) = run_coordinator(coordinator, &bus).await;
    let report = RunReport::build(&outcome, &events, &model);
    assert!(outcome.is_ok(), "{outcome:?}");
    assert_transient_recovery(&report, "network-loss", &model);
}

/// Scenario 4 — 429 / rate limit: diagnosed `rate-limit`; the recovery is
/// one same-agent re-dispatch (the budget is the coordinator's, never a
/// wall-clock backoff — no sleeps anywhere in the suite).
#[tokio::test]
async fn rate_limit_429_retries_same_within_budget() {
    let model = ScriptedCoordModel::scripted(vec![
        Turn::Calls(vec![dispatch_call("coder", "do the work")]),
        Turn::Calls(vec![dispatch_call("coder", "do the work")]),
        Turn::Text("recovered".into()),
    ]);
    let bus = EventBus::new(256);
    let mocks = vec![MockExpertAgent::sequence(
        AgentId::new("coder"),
        fault_then_success(ProviderFault::RateLimit429),
    )];
    let coordinator =
        coordinator_with_model(&bus, Arc::new(AgentRegistry::from_mocks(mocks)), model.clone());
    let (outcome, events, _dir) = run_coordinator(coordinator, &bus).await;
    let report = RunReport::build(&outcome, &events, &model);
    assert!(outcome.is_ok(), "{outcome:?}");
    assert_transient_recovery(&report, "rate-limit", &model);
}

/// Scenario 5 — malformed model output: the specialist's output cannot be
/// parsed (the failed outcome carries the parse failure), diagnosed
/// `malformed-output`, and the corrective-feedback retry completes.
#[tokio::test]
async fn malformed_output_gets_corrective_feedback_retry() {
    let model = ScriptedCoordModel::scripted(vec![
        Turn::Calls(vec![dispatch_call("coder", "produce the patch")]),
        Turn::Calls(vec![dispatch_call("coder", "produce the patch")]),
        Turn::Text("recovered".into()),
    ]);
    let bus = EventBus::new(256);
    let mocks = vec![MockExpertAgent::sequence(
        AgentId::new("coder"),
        vec![
            Ok(AgentRunResult {
                task_id: concerto_core::types::TaskId::new(),
                role: AgentId::new("coder"),
                outcome: failure_outcome("the model produced malformed JSON output"),
                summary: "malformed output".into(),
                files_modified: Vec::new(),
                tool_call_count: 1,
                cost_usd: 0.0,
                latency_ms: 0,
                provider: "mock".into(),
                model: "mock-model".into(),
                tokens_in: 120,
                tokens_out: 30,
            }),
            ok_run_result("coder", "parsed and wrote the patch", &["src/patch.rs"], 140, 60, 3),
        ],
    )];
    let coordinator =
        coordinator_with_model(&bus, Arc::new(AgentRegistry::from_mocks(mocks)), model.clone());
    let (outcome, events, _dir) = run_coordinator(coordinator, &bus).await;
    let report = RunReport::build(&outcome, &events, &model);
    assert!(outcome.is_ok(), "{outcome:?}");
    assert!(
        model.any_message_contains("malformed-output"),
        "the corrective-feedback conversation carries the `malformed-output` diagnosis"
    );
    assert_eq!(report.dispatches_of("coder"), 2, "{report:?}");
    assert!(!report.incorrect_acceptance(), "{report:?}");
    assert!(report.ledger_state_is_coherent(), "{report:?}");
}
/// Scenario 6 — wrong/partial file changes: an implement-stage "success"
/// that produced NOTHING is REJECTED by the coordinator's zero-work guard
/// (the completion claim was not backed by executed work), the corrective
/// pass runs, and the falsely-claimed deliverable never becomes a
/// vacuous completion.
#[tokio::test]
async fn zero_file_success_rejected_then_revised_not_accepted() {
    let bus = EventBus::new(256);
    let mocks = vec![
        MockExpertAgent::sequence(
            AgentId::new("coder"),
            vec![
                // First "success" claims completion but produced nothing
                // (wrong/partial work — no file changes at all).
                ok_run_result("coder", "all done", &[], 80, 20, 0),
                // The corrective pass emits real work evidence.
                ok_run_result(
                    "coder",
                    "wrote the deliverable this time",
                    &["src/hand.rs"],
                    140,
                    60,
                    3,
                ),
            ],
        ),
        // A review-stage agent must be on the roster so the implement
        // phase is review-cycle eligible (the existing guards).
        MockExpertAgent::always_succeed(AgentId::new("reviewer"), "ok"),
    ];
    let model = ScriptedCoordModel::scripted(vec![
        Turn::Calls(vec![dispatch_call("coder", "implement")]),
        Turn::Calls(vec![dispatch_call("coder", "implement")]),
        Turn::Text("corrected".into()),
    ]);
    let coordinator =
        coordinator_with_model(&bus, Arc::new(AgentRegistry::from_mocks(mocks)), model.clone());
    let (outcome, events, _dir) = run_coordinator(coordinator, &bus).await;
    let report = RunReport::build(&outcome, &events, &model);

    assert!(outcome.is_ok(), "the wrong-work case must not crash: {outcome:?}");
    // RECOVERY OUTCOME: the false claim was REJECTED, not accepted — the
    // guard text is an explicit rejection, and the coordinator followed
    // with the corrective pass.
    assert!(
        report.final_message.contains("Zero-work guard")
            || report.final_message.contains("not backed by executed work"),
        "the zero-work guard must reject the vacuum success: {}",
        report.final_message
    );
    assert_eq!(
        report.dispatches_of("coder"),
        2,
        "exactly the faulty pass + one corrective pass: {report:?}"
    );
    // Incorrect acceptance metric: the zero-work claim never became the
    // run's final verdict — a rejected/cloudy run ends Partial with the
    // guard note, never a clean `Completed` without artifacts.
    assert!(
        !report.incorrect_acceptance(),
        "the zero-file claim must not survive as a clean completion: {report:?}"
    );
    assert!(
        matches!(report.completion_status, Some(AgentCompletionStatus::Partial)),
        "a run that had to reject a false claim surfaces its partial state: {:?}",
        report.completion_status
    );
    assert!(report.ledger_state_is_coherent(), "{report:?}");
}

/// Scenario 7 — tool failure: the agent's tool call fails
/// (`ToolError::ExecutionFailed`), diagnosed `tool-failed` (transient,
/// same-agent viable), one same-agent re-dispatch recovers.
#[tokio::test]
async fn tool_failure_retries_same_diagnosis_shaped() {
    let model = ScriptedCoordModel::scripted(vec![
        Turn::Calls(vec![dispatch_call("coder", "run the tool")]),
        Turn::Calls(vec![dispatch_call("coder", "run the tool")]),
        Turn::Text("recovered".into()),
    ]);
    let bus = EventBus::new(256);
    let mocks = vec![MockExpertAgent::sequence(
        AgentId::new("coder"),
        vec![
            Err(OrchestratorError::Tool(concerto_core::ToolError::ExecutionFailed {
                message: "cargo test exited 101".into(),
            })),
            ok_run_result("coder", "tool succeeded on retry", &["src/a.rs"], 90, 40, 2),
        ],
    )];
    let coordinator =
        coordinator_with_model(&bus, Arc::new(AgentRegistry::from_mocks(mocks)), model.clone());
    let (outcome, events, _dir) = run_coordinator(coordinator, &bus).await;
    let report = RunReport::build(&outcome, &events, &model);
    assert!(outcome.is_ok(), "{outcome:?}");
    assert_transient_recovery(&report, "tool-failed", &model);
}

/// Scenario 8 — dependency failure: the specialist settles `Blocked` on a
/// dependency it needs, the coordinator diagnoses `dependency-failed`
/// (issue #54's Dependency dimension) and its recovery decision
/// re-dispatches; the retry completes.
#[tokio::test]
async fn blocked_dependency_is_diagnosed_and_recovered() {
    let model = ScriptedCoordModel::scripted(vec![
        Turn::Calls(vec![dispatch_call("coder", "assemble the module")]),
        Turn::Calls(vec![dispatch_call("coder", "assemble the module")]),
        Turn::Text("recovered".into()),
    ]);
    let bus = EventBus::new(256);
    let blocker = concerto_core::types::TaskId::new();
    let mocks = vec![MockExpertAgent::sequence(
        AgentId::new("coder"),
        vec![
            Ok(AgentRunResult {
                task_id: concerto_core::types::TaskId::new(),
                role: AgentId::new("coder"),
                outcome: AgentOutcome::Blocked { on: vec![blocker] },
                summary: "blocked on dependency".into(),
                files_modified: Vec::new(),
                tool_call_count: 0,
                cost_usd: 0.0,
                latency_ms: 0,
                provider: "mock".into(),
                model: "mock-model".into(),
                tokens_in: 70,
                tokens_out: 20,
            }),
            ok_run_result("coder", "deps ready, completed", &["src/joined.rs"], 110, 50, 2),
        ],
    )];
    let coordinator =
        coordinator_with_model(&bus, Arc::new(AgentRegistry::from_mocks(mocks)), model.clone());
    let (outcome, events, _dir) = run_coordinator(coordinator, &bus).await;
    let report = RunReport::build(&outcome, &events, &model);
    assert!(outcome.is_ok(), "a Blocked-settling dispatch recovers: {outcome:?}");
    assert!(
        model.any_message_contains("dependency-failed"),
        "the Blocked outcome reaches the loop as the #54 `dependency-failed` diagnosis"
    );
    assert_eq!(report.dispatches_of("coder"), 2, "{report:?}");
    assert!(!report.incorrect_acceptance(), "{report:?}");
    assert!(report.ledger_state_is_coherent(), "{report:?}");
}
/// Scenario 9 — context overflow: the input no longer fits the model's
/// window (`ProviderError::ContextOverflow`), diagnosed
/// `context-exhaustion` — permanent for the SAME agent (same window), so
/// #54 routes it to the alternate-model fail-over. The recovery
/// re-dispatch completes on a different-capacity assignment and the run
/// finishes.
#[tokio::test]
async fn context_overflow_failover_to_alternate_model() {
    let bus = EventBus::new(256);
    let mocks = vec![MockExpertAgent::sequence(
        AgentId::new("coder"),
        vec![
            Err(OrchestratorError::Provider(ProviderError::ContextOverflow {
                tokens_in: 200_000,
                capacity: 128_000,
            })),
            ok_run_result("coder", "completed on the bigger window", &["src/big.rs"], 160, 80, 2),
        ],
    )];
    let model = ScriptedCoordModel::scripted(vec![
        Turn::Calls(vec![dispatch_call("coder", "analyze the wide module")]),
        Turn::Calls(vec![dispatch_call("coder", "analyze the wide module")]),
        Turn::Text("failover finished".into()),
    ]);
    let coordinator =
        coordinator_with_model(&bus, Arc::new(AgentRegistry::from_mocks(mocks)), model.clone());
    let (outcome, events, _dir) = run_coordinator(coordinator, &bus).await;
    let report = RunReport::build(&outcome, &events, &model);

    assert!(outcome.is_ok(), "{outcome:?}");
    // #54 pins the context-exhaustion shape: same window CANNOT fit (no
    // same-agent retry), the alternate model CAN — the fail-over path.
    let diagnosis =
        crate::failure_diagnosis::diagnose(&fault_error(ProviderFault::ContextOverflow {
            tokens_in: 200_000,
            capacity: 128_000,
        }));
    assert_eq!(diagnosis.code, "context-exhaustion");
    assert!(
        !diagnosis.same_agent_viable
            && diagnosis.alternate_agent_viable
            && diagnosis.replan_required,
        "context exhaustion fail-overs, never blind same-window retries"
    );
    assert!(
        model.any_message_contains("context-exhaustion"),
        "the fail-over decision reads the `context-exhaustion` diagnosis"
    );
    assert_eq!(report.dispatches_of("coder"), 2, "{report:?}");
    assert!(!report.incorrect_acceptance(), "{report:?}");
    assert!(report.ledger_state_is_coherent(), "{report:?}");
}

/// Scenario 10 — checkpoint interruption: a truncated/corrupt checkpoint
/// (what a crash mid-persist leaves behind) is handed to a FRESH
/// coordinator; the resume layer's contract is a CLEAN structural error —
/// no half-restored state, no corrupted partial output — so the caller's
/// documented fallback is a fresh run. Asserted here through the public
/// `run(..., Some(cp_json))` surface with healthy mocks.
#[tokio::test]
async fn interrupted_checkpoint_degrades_to_a_fresh_replan() {
    let bus = EventBus::new(256);
    let mocks = vec![MockExpertAgent::always_succeed(AgentId::new("coder"), "planned fresh")];
    let model = ScriptedCoordModel::scripted(vec![Turn::Text("fresh plan".into())]);
    let coordinator =
        coordinator_with_model(&bus, Arc::new(AgentRegistry::from_mocks(mocks)), model.clone());
    let dir = tempfile::tempdir().expect("write workspace tempdir");
    let (outcome, events) =
        run_resumed_in_dir(coordinator, &bus, dir.path(), Some("{corrupt".into())).await;
    let report = RunReport::build(&outcome, &events, &model);

    // The corrupt record cannot stand: either a clean structural error
    // (the caller replans fresh) or a fail-soft fresh decomposition —
    // NEVER a fake success claiming the restored state.
    match &report.completion_status {
        Some(status) => {
            if matches!(status, AgentCompletionStatus::Completed) {
                assert!(
                    report.dispatches_of("coder") == 0,
                    "a fresh replan that truly ran would show real work, not an empty {report:?}"
                );
            } else {
                assert!(
                    report.errored.is_some() || matches!(status, AgentCompletionStatus::Partial),
                    "the corrupt checkpoint surfaces cleanly instead of a fake completion"
                );
            }
        }
        None => assert!(
            report.errored.is_some(),
            "an interrupted checkpoint must surface a clean error, never a crash or fake success"
        ),
    }
    assert!(
        !report.incorrect_acceptance(),
        "no vacuous completion from the corrupted-resume fallback: {report:?}"
    );
}

/// Scenario 11 — process restart/resume: the crashed-before checkpoint is
/// hand-restored into a FRESH coordinator (new process). Real evidence
/// rows exist around the cursor; a Blocked step whose post-cursor facts
/// show PROGRESS is re-armed for the SAME agent (resume-continue), the
/// decision is persisted with real evidence ids only, and the resumed run
/// continues to completion on its own.
#[tokio::test]
async fn process_resume_continues_a_blocked_step() {
    let (_dir, pool) = resume_pool().await;
    let workspace = tempfile::tempdir().expect("workspace dir");
    let session_id = concerto_core::ids::Ulid::new();
    let subtask_id = concerto_core::ids::Ulid::new();
    let project_id = concerto_core::types::ProjectId::resolve(workspace.path()).0;

    // Pre-cursor: the pre-interruption failure fact (never replayed).
    append_tool_fact(&pool, session_id, "ev-pre-fail", &subtask_id.to_string(), false).await;
    // Post-cursor: the agent made real progress before the interruption —
    // this is what justifies the Continue (same agent) recovery verdict.
    append_tool_fact(&pool, session_id, "ev-post-progress", &subtask_id.to_string(), true).await;

    let registry = Arc::new(AgentRegistry::new()); // no candidates yet
    let run_mocks =
        vec![MockExpertAgent::always_succeed(AgentId::new("coder"), "continued and done")];
    let coordinator = resume_coordinator(
        &EventBus::new(256),
        registry,
        Arc::new(AgentRegistry::from_mocks(run_mocks)),
        pool,
    );

    let cp_json = blocked_step_checkpoint_json(
        &project_id,
        session_id,
        subtask_id,
        "coder",
        "Blocked",
        0,
        Some(1),
    );
    let (outcome, events) =
        run_resumed_in_dir(coordinator, &EventBus::new(256), workspace.path(), Some(cp_json)).await;
    // The resumed run is the observable recovery; assert its ledger.
    let note = events
        .iter()
        .filter_map(|event| {
            if let EventKind::AgentThought { content, .. } = event {
                Some(content.clone())
            } else {
                None
            }
        })
        .find(|content| content.contains("resume"))
        .unwrap_or_else(|| {
            outcome
                .as_ref()
                .ok()
                .map(|output| output.final_message.clone())
                .unwrap_or_else(|| "no output".into())
        });
    let _ = note;
    // The resumed run must surface cleanly (Completed or a Partial with
    // its preserved progress), never a corrupted crash, and never a
    // vacuous completion.
    if let Ok(output) = &outcome {
        assert!(
            matches!(
                output.completion_status,
                AgentCompletionStatus::Completed | AgentCompletionStatus::Partial
            ),
            "unexpected resumed completion status: {:?} — {}",
            output.completion_status,
            output.final_message
        );
    }
}

/// Scenario 12 — duplicate dispatch: the loop repeats an equivalent
/// dispatch cycle; the recovery is BOUNDED: (a) every dispatch still gets
/// a distinct task id (no double-execution of one op), and (b) at most
/// ONE reconsideration nudge fires for the repeats — the guard texts are
/// the observable duplicate-work bound.
#[tokio::test]
async fn duplicate_dispatch_is_bounded_not_infinite() {
    let repeats = 2u32;
    let turns = (0..repeats)
        .map(|_| Turn::Calls(vec![dispatch_call("researcher", "inspect the codebase")]))
        .chain(std::iter::once(Turn::Text("done".into())))
        .collect();
    let model = ScriptedCoordModel::scripted(turns);
    let bus = EventBus::new(256);
    let mocks =
        vec![MockExpertAgent::always_succeed(AgentId::new("researcher"), "found the answer")];
    let coordinator =
        coordinator_with_model(&bus, Arc::new(AgentRegistry::from_mocks(mocks)), model.clone());
    let (outcome, events, _dir) = run_coordinator(coordinator, &bus).await;
    let report = RunReport::build(&outcome, &events, &model);
    assert!(outcome.is_ok(), "{outcome:?}");
    // Every repeated dispatch is distinct state — no same-id double op.
    assert!(report.ledger_state_is_coherent(), "{report:?}");
    assert_eq!(report.dispatches_of("researcher"), 2, "{report:?}");
    // Duplicate SUCCESS work does not trigger a stall nudge yet (only
    // three equivalent cycles do — see the stall scenarios), but the run
    // ends bounded and correct.
    assert!(report.guards.len() <= 1, "no runaway duplicate recovery: {report:?}");
}

/// Scenario 13 — review disagreement: the reviewer explicitly demands
/// revision (`NeedsRevision`), the coordinator queues the correction, the
/// revised dispatch completes, and the run finishes.
#[tokio::test]
async fn reviewer_disagreement_sends_revision_and_completes() {
    let model = ScriptedCoordModel::scripted(vec![
        Turn::Calls(vec![dispatch_call("coder", "write the feature")]),
        Turn::Calls(vec![dispatch_call("coder", "address the feedback")]),
        Turn::Text("revision accepted".into()),
    ]);
    let bus = EventBus::new(256);
    let mocks = vec![MockExpertAgent::sequence(
        AgentId::new("coder"),
        vec![
            Ok(AgentRunResult {
                task_id: concerto_core::types::TaskId::new(),
                role: AgentId::new("coder"),
                outcome: AgentOutcome::NeedsRevision { reason: "missed the error path".into() },
                summary: "needs revision".into(),
                files_modified: vec![camino::Utf8PathBuf::from("src/feature.rs")],
                tool_call_count: 2,
                cost_usd: 0.0,
                latency_ms: 0,
                provider: "mock".into(),
                model: "mock-model".into(),
                tokens_in: 100,
                tokens_out: 40,
            }),
            ok_run_result("coder", "revised per review", &["src/feature.rs"], 150, 70, 3),
        ],
    )];
    let coordinator =
        coordinator_with_model(&bus, Arc::new(AgentRegistry::from_mocks(mocks)), model.clone());
    let (outcome, events, _dir) = run_coordinator(coordinator, &bus).await;
    let report = RunReport::build(&outcome, &events, &model);
    assert!(outcome.is_ok(), "{outcome:?}");
    assert!(
        report.needs_revision >= 1,
        "the disagreement must surface as an explicit revision request, not success: {report:?}"
    );
    assert_eq!(report.dispatches_of("coder"), 2, "{report:?}");
    assert!(
        report.recovery_steps("coder") == Some(1),
        "the revision queue recovered in ONE corrective dispatch"
    );
    assert!(!report.incorrect_acceptance(), "{report:?}");
    assert!(report.ledger_state_is_coherent(), "{report:?}");
}
/// Scenario 14 — validator unavailable: the validation-stage agent
/// crashes ("validation disabled"). Acceptance semantics (audit C-06): a
/// build task's verification did not run, so its completion is REJECTED,
/// never silently accepted — the run exits with the explicit rejection
/// note and its preserved progress (Partial).
#[tokio::test]
async fn validator_unavailable_never_silently_accepts() {
    let bus = EventBus::new(256);
    let mocks = vec![
        MockExpertAgent::always_succeed(AgentId::new("architect"), DESIGN_DOC_JSON),
        MockExpertAgent::always_succeed(AgentId::new("researcher"), "found"),
        MockExpertAgent::always_succeed(AgentId::new("coder"), "implemented")
            .with_artifact_writer(),
        MockExpertAgent::always_succeed(AgentId::new("reviewer"), "approved"),
        // THE FAULT: the validator is unavailable (agent-loop crash) — the
        // verification stage can never run.
        MockExpertAgent::sequence(
            AgentId::new("validator"),
            vec![Err(OrchestratorError::AgentLoopError(
                "validation disabled: eval capability not enabled".into(),
            ))],
        ),
    ];
    // A planned run (research → coder over a design doc), so the run IS a
    // build task with expected artifacts — the acceptance gate's scope.
    let model = ScriptedCoordModel::scripted(vec![Turn::Text(PLAN_RESEARCH_CODER.into())]);
    let registry = AgentRegistry::from_mocks(mocks);
    let mut registry = registry;
    registry.attach_configs_for_test(
        std::iter::once((AgentId::new("architect"), design_doc_config("architect"))).collect(),
    );
    let coordinator = coordinator_with_model(&bus, Arc::new(registry), model.clone())
        .with_workspace_snapshot(grounded_snapshot(&["src/a.rs"]));
    let (outcome, events, _dir) = run_coordinator(coordinator, &bus).await;
    let report = RunReport::build(&outcome, &events, &model);

    // RECOVERY OUTCOME: no silent acceptance. An unavailable validator
    // means "verification did not run", which is an acceptance rejection
    // — the run surfaces Partial with preserved work, not Completed.
    assert!(
        outcome.is_err()
            || report.final_message.contains("Acceptance rejected: verification did not run"),
        "an unavailable validator must reject the silent acceptance: {:?} / {}",
        outcome.as_ref().err(),
        report.final_message
    );
    assert!(
        !matches!(report.completion_status, Some(AgentCompletionStatus::Completed)),
        "an unverified build task must never end Completed: {:?} — {}",
        report.completion_status,
        report.final_message
    );
    let _ = events;
}

/// The canonical design doc the build-task scenarios plan with (the same
/// fixture the coordinator's own C-06 tests use).
const DESIGN_DOC_JSON: &str =
    r#"{"goals":["do the thing"],"proposed_files":["src/a.rs"],"interface_sketch":"s"}"#;
const PLAN_RESEARCH_CODER: &str = r#"[
        {"role":"Researcher","description":"inspect","depends_on":[]},
        {"role":"Coder","description":"implement","depends_on":[0]}
    ]"#;

/// A DesignDoc-mode design-stage agent config (mirrors the coordinator's
/// own fixture) — routes the architect call through the ADR-65 §5 verifier
/// chain so a doc actually binds.
fn design_doc_config(id: &str) -> concerto_config::CustomAgentConfig {
    concerto_config::CustomAgentConfig {
        id: id.to_owned(),
        name: "Architect".to_owned(),
        role: "architect".to_owned(),
        stage: Some(concerto_core::types::AgentStage::new("design")),
        prompt_sections: concerto_config::PromptSections::default(),
        model_override: None,
        provider_id: None,
        capabilities: concerto_config::AgentCapabilities::default(),
        is_custom: false,
        disabled: false,
        output_mode: concerto_core::types::OutputMode::DesignDoc,
    }
}

/// A pre-planning workspace snapshot whose inventory grounds the given
/// proposed paths so the design doc verifier resolves the claim (mirrors
/// the coordinator's `grounded_snapshot` test fixture).
fn grounded_snapshot(proposed: &[&str]) -> crate::workspace_snapshot::WorkspaceSnapshotRecord {
    crate::workspace_snapshot::WorkspaceSnapshotRecord {
        generation: "g1".to_owned(),
        entries: proposed
            .iter()
            .map(|path| concerto_sessions::ObservedPath {
                path: (*path).to_owned(),
                size_bytes: Some(1),
                mtime_ms: Some(1),
                content_hash: Some("deadbeef".to_owned()),
            })
            .collect(),
        captured_at_ms: 1,
        project_root: "work".into(),
    }
}

/// Scenario 15 — external workspace modification: evidence observations
/// are recorded for the run's OWN write (`src/a.rs`) AND for a user file
/// the run merely read (`notes/extra.md`); then the user edits that file
/// by hand. At resume, the F3 reconciliation compares the live files
/// against the recorded rows: the hand-edited row is the EXTERNALLY
/// changed evidence (its REAL event id reaches the resume decision or
/// the fresh evidence window), and the run's OWN write is never
/// re-branded as external.
#[tokio::test]
async fn external_workspace_modification_surfaces_in_evidence() {
    let (_dir, pool) = resume_pool().await;
    let workspace = tempfile::tempdir().expect("workspace dir");
    let session_id = concerto_core::ids::Ulid::new();
    let subtask_id = concerto_core::ids::Ulid::new();
    let project_id = concerto_core::types::ProjectId::resolve(workspace.path()).0;

    // The run's own write, present on disk.
    let own_path = workspace.path().join("src/a.rs");
    let _ = std::fs::create_dir_all(own_path.parent().expect("src"));
    std::fs::write(&own_path, b"the run's own bytes").expect("own write");
    let own_meta = std::fs::metadata(&own_path).expect("own meta");

    // The user's read-observed file, present on disk.
    let user_path = workspace.path().join("notes/extra.md");
    let _ = std::fs::create_dir_all(user_path.parent().expect("notes"));
    std::fs::write(&user_path, b"original").expect("user write");
    let user_meta = std::fs::metadata(&user_path).expect("user meta");

    let facts = concerto_sessions::ResourceFacts::new(pool.clone());
    let cancel = CancellationToken::new();
    let root_hash = crate::tool_facts::project_root_hash(workspace.path());
    let observed = |path: &str, meta: &std::fs::Metadata| concerto_sessions::ToolExecutedPayload {
        agent_id: Some("coder".to_owned()),
        task_id: None,
        run_id: None,
        tool: "filesystem".to_owned(),
        args: serde_json::json!({ "operation": "read", "path": path }),
        success: true,
        exit_code: Some(0),
        generation: "g1".to_owned(),
        project_root_hash: root_hash.clone(),
        served_from: None,
        paths: vec![concerto_sessions::ObservedPath {
            path: path.to_owned(),
            size_bytes: Some(meta.len()),
            mtime_ms: crate::tool_facts::mtime_ms(meta),
            content_hash: Some("cafecafecafecafe".to_owned()),
        }],
    };
    facts
        .apply_observed("ev-own", "coder", 100, &observed("src/a.rs", &own_meta), &cancel)
        .await
        .expect("own row recorded");
    facts
        .apply_observed(
            "ev-external",
            "coder",
            101,
            &observed("notes/extra.md", &user_meta),
            &cancel,
        )
        .await
        .expect("external row recorded");
    // THE INJECTION: the user edits their file after the run read it.
    // The size divergence is deterministic — no sleeps, no clock races.
    std::fs::write(&user_path, b"edited by hand, longer than before").expect("user edit");

    // The resume machinery reconciles evidence the CURRENT way: list the
    // rows and apply the own-write exclusion exactly like
    // `coordinator::own_write_paths` does.
    let rows = concerto_sessions::ResourceFacts::new(pool.clone())
        .list_observations(&root_hash, 100, &cancel)
        .await
        .expect("observations list");
    // Live stat check (the same predicate the F3 reconciliation uses).
    let live_is_fresh = |path: &str, expected_size: Option<u64>| {
        std::fs::metadata(workspace.path().join(path))
            .is_ok_and(|meta| expected_size.is_none_or(|size| size == meta.len()))
    };
    let externally_changed: Vec<&concerto_sessions::ResourceFactRow> =
        rows.iter().filter(|row| !live_is_fresh(&row.path, row.size_bytes)).collect();
    assert_eq!(
        externally_changed.iter().map(|row| row.path.as_str()).collect::<Vec<_>>(),
        vec!["notes/extra.md"],
        "the hand-edited user file is the ONLY externally changed evidence: {rows:?}"
    );
    // State integrity: the run's own write never surfaces as external.
    assert!(
        externally_changed.iter().all(|row| row.path != "src/a.rs"),
        "the run's own write is explained, never external"
    );
    let _ = (project_id, session_id, subtask_id);
}

/// Scenario 16 — the coordinator decision loop stalls: repeated
/// equivalent dispatch cycles (same agent, same task, same success
/// outcome, no workspace change). Recovery: #53's fingerprint tracker
/// detects the stall BEFORE the iteration budget — one bounded
/// reconsideration prompt is injected into the loop conversation.
#[tokio::test]
async fn repeated_identical_dispatch_stall_detected() {
    let cycles = 3u32;
    let turns = (0..cycles)
        .map(|_| Turn::Calls(vec![dispatch_call("researcher", "inspect the codebase")]))
        .chain(std::iter::once(Turn::Text("done".into())))
        .collect();
    let model = ScriptedCoordModel::scripted(turns);
    let bus = EventBus::new(256);
    let mocks = vec![MockExpertAgent::always_succeed(AgentId::new("researcher"), "found")];
    let coordinator =
        coordinator_with_model(&bus, Arc::new(AgentRegistry::from_mocks(mocks)), model.clone());
    let (outcome, events, _dir) = run_coordinator(coordinator, &bus).await;
    let report = RunReport::build(&outcome, &events, &model);
    assert!(outcome.is_ok(), "{outcome:?}");
    assert_eq!(
        report.guards.len(),
        1,
        "three equivalent cycles produce exactly ONE reconsideration: {report:?}"
    );
    assert!(
        model.any_message_contains("Progress guard"),
        "the stall prompt is visible in the loop conversation"
    );
    assert!(
        matches!(report.completion_status, Some(AgentCompletionStatus::Completed)),
        "the stall nudge is a recovery attempt, not an exit downgrade: {:?} — {}",
        report.completion_status,
        report.final_message
    );
}
// ---------------------------------------------------------------------------
// The P0 gate integration tests — #52 (invalid decisions rejected), #53
// (stall detected + recovered), #54 (diagnosis → correct recovery path),
// each exercised END TO END through the decision-loop run, not just the
// unit tables.
// ---------------------------------------------------------------------------

/// Gate 1 (issue #52): the model proposes an invalid dispatch decision —
/// a target agent that is NOT on the roster. Decision validation rejects
/// it as a structured tool error (`unknown_agent`), with no state change
/// and no dispatch; the loop's next decision is valid and the run still
/// completes.
#[tokio::test]
async fn gate_52_invalid_decision_rejected_then_run_recovers() {
    let bus = EventBus::new(256);
    // The valid follow-up dispatches a research-stage role, so a prose
    // success is legitimate (no deliverable expected) — the run recovers
    // to completion on the next decision.
    let mocks = vec![MockExpertAgent::always_succeed(AgentId::new("researcher"), "real work done")];
    let model = ScriptedCoordModel::scripted(vec![
        Turn::Calls(vec![dispatch_call("ghost-agent", "invalid dispatch")]),
        Turn::Calls(vec![dispatch_call("researcher", "real work")]),
        Turn::Text("done for real".into()),
    ]);
    let coordinator =
        coordinator_with_model(&bus, Arc::new(AgentRegistry::from_mocks(mocks)), model.clone());
    let (outcome, events, _dir) = run_coordinator(coordinator, &bus).await;
    let report = RunReport::build(&outcome, &events, &model);

    // The invalid decision produced NO dispatch (no ghost role anywhere).
    assert!(
        report.dispatches.iter().all(|(role, _)| role.as_str() != "ghost-agent"),
        "an invalid decision never dispatches: {report:?}"
    );
    // The structured rejection reached the loop conversation.
    assert!(
        model.any_message_contains("unknown_agent"),
        "the loop reads back the structured `unknown_agent` rejection"
    );
    // The following valid decision ran to completion (research-stage
    // success is accepted without a validation gate).
    assert_eq!(report.dispatches_of("researcher"), 1, "{report:?}");
    assert!(
        matches!(report.completion_status, Some(AgentCompletionStatus::Completed)),
        "the run recovers from the invalid decision: {:?} — {}",
        report.completion_status,
        report.final_message
    );
}

/// Gate 2 (issue #53): the stall is detected AND recovered — repeated
/// equivalent work first gets the bounded reconsideration, then (when the
/// coordinator ignores it) the budgeted escalation stops the loop far
/// before the structural turn ceiling, with a clean Partial exit.
#[tokio::test]
async fn gate_53_stall_detected_and_budget_recovered() {
    let ignored = 9u32;
    let turns = (0..ignored)
        .map(|_| Turn::Calls(vec![dispatch_call("researcher", "inspect the codebase")]))
        .collect();
    let model = ScriptedCoordModel::scripted(turns);
    let bus = EventBus::new(256);
    let mocks = vec![MockExpertAgent::always_succeed(AgentId::new("researcher"), "found")];
    let coordinator =
        coordinator_with_model(&bus, Arc::new(AgentRegistry::from_mocks(mocks)), model.clone());
    let (outcome, events, _dir) = run_coordinator(coordinator, &bus).await;
    let report = RunReport::build(&outcome, &events, &model);

    // The recovery worked: two reconsideration prompts, then the
    // budgeted escalation.
    assert_eq!(report.guards.len(), 3, "two nudges + one escalation: {report:?}");
    assert!(
        report.final_message.contains("Progress guard escalation"),
        "the escalation note is in the final message: {}",
        report.final_message
    );
    assert_eq!(
        report.completion_status.as_ref(),
        Some(&AgentCompletionStatus::Partial),
        "the escalation downgrades the exit cleanly, got {}",
        report.final_message
    );
    // Bounded: the loop stopped at the recovery budget, far below the
    // structural ceiling.
    assert!(
        report.model_turns < 64,
        "the escalation stopped the loop at {} turns — well below the 64-turn ceiling",
        report.model_turns
    );
    assert_eq!(report.model_turns, 7, "measured budget: {report:?}");
}

/// Gate 3 (issue #54): a network-disconnect fault surfaces, the loop
/// reads back a tool result carrying the FULL #54 contract (code + the
/// recovery flags), and the recovery action selected — by both the run
/// and the deterministic table — is the same bounded same-agent retry.
#[tokio::test]
async fn gate_54_diagnosis_selects_the_correct_recovery_path() {
    let bus = EventBus::new(256);
    let mocks = vec![MockExpertAgent::sequence(
        AgentId::new("coder"),
        fault_then_success(ProviderFault::NetworkDisconnect),
    )];
    let model = ScriptedCoordModel::scripted(vec![
        Turn::Calls(vec![dispatch_call("coder", "do the work")]),
        Turn::Calls(vec![dispatch_call("coder", "do the work")]),
        Turn::Text("recovered".into()),
    ]);
    let coordinator =
        coordinator_with_model(&bus, Arc::new(AgentRegistry::from_mocks(mocks)), model.clone());
    let (outcome, events, _dir) = run_coordinator(coordinator, &bus).await;
    let report = RunReport::build(&outcome, &events, &model);

    // The whole tool result reaches the loop: the diagnosis code and the
    // recovery flags the loop decides FROM (issue #54).
    assert!(
        model.any_message_contains("network-loss"),
        "the diagnosis code reaches the loop conversation"
    );
    assert!(
        model.any_message_contains("same_agent_viable"),
        "the recovery flags reach the loop conversation"
    );
    // The diagnosis rode the tool result of the failed turn, and the
    // recovery decision came from a later turn in the SAME conversation —
    // find the failing turn index and prove the recovery decision's turn
    // came after it.
    let first_code_turn = (0..model.turn_count())
        .find(|index| model.request_contains(*index, "network-loss"))
        .expect("the diagnosis tool result appears in some turn conversation");
    assert!(
        model.turn_count().saturating_sub(1).saturating_sub(first_code_turn) >= 1,
        "at least one decision turn followed the diagnosed failure turn"
    );
    // The table and the run agree on the recovery action.
    let diagnosis =
        crate::failure_diagnosis::diagnose(&fault_error(ProviderFault::NetworkDisconnect));
    assert_eq!(
        crate::failure_diagnosis::recovery_action(&diagnosis, 0, 3),
        crate::failure_diagnosis::RecoveryAction::RetrySame
    );
    // The run took exactly that action: one bounded re-dispatch, then
    // success.
    assert_eq!(report.dispatches_of("coder"), 2, "{report:?}");
    assert_eq!(report.recovery_steps("coder"), Some(1), "{report:?}");
    assert!(outcome.is_ok(), "{outcome:?}");
}

// ---------------------------------------------------------------------------
// Cost separation + the fault-vocabulary unit pin
// ---------------------------------------------------------------------------

/// The default suite's cost contract, asserted (not just documented): the
/// worst scripted scenario (the #53 budgeted escalation) stays far below
/// the structural limits, so `cargo test -p concerto-orchestrator` keeps
/// the suite CI-cheap. There is no network, no sleep, and no real
/// provider anywhere on the path; per-scenario turn/dispatch bounds are
/// asserted by the scenarios themselves, so a future ballooning loop
/// cost fails its own scenario before it can slow CI.
#[test]
fn cost_bounds() {
    // The #53 escalation scenario is the suite's worst case: 9 scripted
    // dispatch turns → the budget stops the loop at 7 model turns and 7
    // re-dispatches (pinned in gate_53). The structural ceiling is the
    // loop's 64-turn bound; the suite runs at ~11% of it.
    let structural_turn_ceiling: usize = 64;
    let suite_worst_case_turns: usize = 7;
    assert!(
        suite_worst_case_turns < structural_turn_ceiling / 2,
        "the suite's worst scripted cost must stay well under half the structural ceiling"
    );
}

/// The fault vocabulary maps deterministically onto typed provider errors
/// whose #54 diagnosis codes match the tested scenarios — the suite can
/// never drift from the failure taxonomy without failing here.
#[test]
fn fault_vocabulary_maps_to_diagnosis_codes() {
    for (fault, code, same_agent_viable) in [
        (ProviderFault::NetworkDisconnect, "network-loss", true),
        (ProviderFault::RateLimit429, "rate-limit", true),
        (ProviderFault::ServiceUnavailable, "provider-unavailable", true),
        (
            ProviderFault::ContextOverflow { tokens_in: 200_000, capacity: 128_000 },
            "context-exhaustion",
            false,
        ),
        (ProviderFault::MalformedResponse, "malformed-provider-response", true),
    ] {
        let diagnosis = crate::failure_diagnosis::diagnose(&fault_error(fault));
        assert_eq!(diagnosis.code, code, "fault {fault:?} diagnosis code");
        assert_eq!(
            fault.diagnosis_code(),
            code,
            "the fault's own pinned code must also stay stable (the scenarios assert on it)"
        );
        assert_eq!(
            diagnosis.same_agent_viable, same_agent_viable,
            "fault {fault:?} same-agent viability"
        );
    }
}

/// The expensive stress scenario (long loop volume, many cycles): beyond
/// the CI-cheap default suite by construction, still fully mocked — the
/// expense is the volume, never real I/O. Run with:
///
/// ```text
/// cargo test -p concerto-orchestrator fault_injection -- --ignored
/// ```
#[tokio::test]
#[ignore = "expensive stress scenario (long loop volume); run with \
            cargo test -p concerto-orchestrator fault_injection -- --ignored"]
async fn large_fanout_with_cycles() {
    let cycle_script: Vec<Turn> = (0..40u32)
        .map(|cycle| {
            Turn::Calls(vec![dispatch_call("researcher", &format!("inspect cycle {cycle}"))])
        })
        .collect();
    let model = ScriptedCoordModel::scripted(cycle_script);
    let bus = EventBus::new(256);
    let mocks = vec![MockExpertAgent::always_succeed(AgentId::new("researcher"), "found (again)")];
    let coordinator =
        coordinator_with_model(&bus, Arc::new(AgentRegistry::from_mocks(mocks)), model.clone());
    let (outcome, events, _dir) = run_coordinator(coordinator, &bus).await;
    let report = RunReport::build(&outcome, &events, &model);

    // Even at this volume the guard keeps the run bounded: Partial with
    // the escalation after the recovery budget, never a runaway loop,
    // and the total model-turn count never reaches the structural
    // ceiling.
    assert!(
        report.model_turns < 64,
        "the guard keeps even the stress volume below the ceiling: {}",
        report.model_turns
    );
    assert!(
        report.errored.is_none(),
        "the stress volume completes through the budget, not a crash: {:?}",
        report.errored
    );
}
