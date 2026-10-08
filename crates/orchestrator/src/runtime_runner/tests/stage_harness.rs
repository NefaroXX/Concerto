//! Local approval/policy/provider test-harness plus the first stage-tracker
//! and auto-Apply (A5) coverage for `runtime_runner_impl`.
//!
//! Mechanical extraction (NORM S24G): the local `ApprovalTestHarness`,
//! `TestAudit`, `WriteFileTool`, `ScriptedProvider` and `FailingProvider`
//! fixtures, the `make_tool_call` / `make_services` / `make_executor` /
//! `drain_stage_events` helpers, the harness banner, the ADR-55 §4 auto-Apply
//! banner, and the first six tests (`stage_tracker_publishes_only_transitions`
//! through `a5_same_objective_durable_binding_auto_applies`) move verbatim out
//! of `runtime_runner::runtime_runner_tests`, so test names and assertions are
//! unchanged. `use super::super::*;` keeps the parent (`runtime_runner_impl`)
//! items in scope.
//!
//! Visibility (S24-D precedent): the harness items whose consumers are still
//! inline in the donor until slices H and I — `TestAudit` (the stage_feed
//! test), `ScriptedProvider` / `FailingProvider` / `make_tool_call` /
//! `make_services` / `make_executor` / `drain_stage_events` (the two
//! `stage_tracker_*` sequence tests) — are `pub(in crate::runtime_runner_impl)`
//! so `runtime_runner_tests` keeps importing them at their new path.
//! `ApprovalTestHarness` and `WriteFileTool` stay private: every user is in
//! this file.

use super::super::*;
use crate::plan_approval::plan_artifact_hash;
use crate::services::ServicesBuilder;
use async_trait::async_trait;
use concerto_core::error::PolicyError;
use concerto_core::error::ToolError;
use concerto_core::event::EventKind;
use concerto_core::event::EventReceiver;
use concerto_core::traits::approval::ApprovalDecision;
use concerto_core::traits::policy::AuditEntry;
use concerto_core::traits::provider::CompletionStream;
use concerto_core::traits::tool::Tool;
use concerto_core::types::{
    CapabilitySet, CompletionChunk, CompletionRequest, TokenBudget, ToolCall, ToolOutput,
};
use futures::stream;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};

// -----------------------------------------------------------------------
// Local ApprovalTestHarness (can't import from concerto_core::testing
// because that module is cfg(test) for the core crate, not re-exported
// to downstream crate test builds).
// -----------------------------------------------------------------------

struct ApprovalTestHarness {
    decisions: VecDeque<ApprovalDecision>,
}

impl ApprovalTestHarness {
    fn always_approve() -> Self {
        Self { decisions: VecDeque::new() }
    }
}

#[async_trait]
impl ApprovalSink for ApprovalTestHarness {
    async fn request_approval(
        &self,
        _action: &concerto_core::types::PolicyAction<'_>,
        _cancel: CancellationToken,
    ) -> ApprovalDecision {
        self.decisions.clone().into_iter().next().unwrap_or(ApprovalDecision::Approve)
    }
    async fn approve_all_for_session(&self, _session_id: Ulid, _cancel: CancellationToken) {}
    async fn request_ack(
        &self,
        _session_id: Ulid,
        _message: &str,
        _cancel: CancellationToken,
    ) -> bool {
        true // auto-acknowledge in tests
    }
}

pub(in crate::runtime_runner_impl) struct TestAudit;
#[async_trait]
impl AuditLog for TestAudit {
    async fn record(
        &self,
        _entry: AuditEntry,
        _cancel: CancellationToken,
    ) -> Result<(), PolicyError> {
        Ok(())
    }
}

/// A tool that simulates writing a file (file-changing tool), so the
/// agent_loop marks the run as having changed files.
struct WriteFileTool;
#[async_trait]
impl Tool for WriteFileTool {
    fn name(&self) -> &str {
        "write_file"
    }
    fn description(&self) -> &str {
        "writes content to a file"
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({})
    }
    fn capability_requirements(&self) -> CapabilitySet {
        CapabilitySet::default()
    }
    async fn execute(
        &self,
        _input: serde_json::Value,
        _policy: &dyn concerto_core::traits::policy::PolicyEngine,
        _session: &SessionContext,
        _cancel: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput {
            summary: "file written".into(),
            data: serde_json::json!({"file_path": "/tmp/test.rs"}),
        })
    }
}

/// A mock LLM provider that returns a predefined sequence of tool-call
/// batches. Each call to `stream_completion` advances to the next batch.
pub(in crate::runtime_runner_impl) struct ScriptedProvider {
    responses: Vec<Vec<ToolCall>>,
    call_count: AtomicUsize,
}

impl ScriptedProvider {
    pub(in crate::runtime_runner_impl) fn new(responses: Vec<Vec<ToolCall>>) -> Self {
        Self { responses, call_count: AtomicUsize::new(0) }
    }
}

#[async_trait]
impl LlmProvider for ScriptedProvider {
    fn provider_name(&self) -> &'static str {
        "scripted"
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
        let idx = self.call_count.fetch_add(1, Ordering::SeqCst);
        let tool_calls = self.responses.get(idx).cloned().unwrap_or_default();
        let chunks: Vec<_> = if tool_calls.is_empty() {
            vec![CompletionChunk {
                reasoning: None,
                delta: String::new(),
                tool_call: None,
                is_final: true,
                usage: None,
            }]
        } else {
            tool_calls
                .into_iter()
                .map(|tc| CompletionChunk {
                    reasoning: None,
                    delta: String::new(),
                    tool_call: Some(tc),
                    is_final: false,
                    usage: None,
                })
                .collect()
        };
        Ok(Box::pin(stream::iter(chunks.into_iter().map(Ok))))
    }
}

/// A provider that always fails with a non-transient error, so the run
/// fails fast (no retry sleep).
pub(in crate::runtime_runner_impl) struct FailingProvider;
#[async_trait]
impl LlmProvider for FailingProvider {
    fn provider_name(&self) -> &'static str {
        "failing"
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
        Err(ProviderError::NotConfigured)
    }
}

pub(in crate::runtime_runner_impl) fn make_tool_call(name: &str, text: &str) -> ToolCall {
    ToolCall {
        id: "call_1".into(),
        name: name.into(),
        arguments: serde_json::json!({"text": text}),

        ..Default::default()
    }
}

/// Shared services for a direct `execute_agent_loop` invocation, with an
/// always-approve approval sink and default config.
pub(in crate::runtime_runner_impl) fn make_services(bus: EventBus) -> SharedServices {
    let approval = Arc::new(ApprovalTestHarness::always_approve());
    ServicesBuilder::new(bus, AppConfig::default(), approval).build()
}

/// An executor whose registry contains the file-changing `WriteFileTool`
/// behind an allow-all policy, so tool calls execute without approvals.
pub(in crate::runtime_runner_impl) fn make_executor() -> Arc<ToolExecutor> {
    let mut registry = ToolRegistry::default();
    registry.register(Box::new(WriteFileTool));
    let allow_all = vec![PolicyRule::AutoApprove(Condition::Always)];
    let policy = Arc::new(SimplePolicyEngine::new(allow_all, Arc::new(TestAudit)));
    Arc::new(
        ToolExecutor::new(Arc::new(registry), policy)
            .with_approval_sink(Arc::new(ApprovalTestHarness::always_approve())),
    )
}

/// Drain all buffered `RunStageChanged` events for `session_id` in order.
pub(in crate::runtime_runner_impl) fn drain_stage_events(
    receiver: &mut EventReceiver,
    session_id: Ulid,
) -> Vec<RunStage> {
    let mut stages = Vec::new();
    while let Ok(event) = receiver.try_recv() {
        if event.session_id == session_id {
            if let EventKind::RunStageChanged { stage, .. } = &event.kind {
                stages.push(*stage);
            }
        }
    }
    stages
}

#[test]
fn stage_tracker_publishes_only_transitions() {
    let bus = EventBus::new(256);
    let mut receiver = bus.subscribe();
    let session_id = Ulid::new();
    let task_id = TaskId::new();
    let mut tracker = StageTracker::new(bus, session_id, task_id);

    tracker.set(RunStage::Understand);
    tracker.set(RunStage::Understand); // duplicate — must not re-publish
    tracker.set(RunStage::Inspect);
    tracker.set(RunStage::Execute);
    tracker.set(RunStage::Execute); // duplicate — must not re-publish
    tracker.set(RunStage::Complete);

    let mut seen = Vec::new();
    while let Ok(event) = receiver.try_recv() {
        if let EventKind::RunStageChanged { task_id: got, stage } = &event.kind {
            assert_eq!(*got, task_id, "RunStageChanged carries the run's task id");
            assert_eq!(event.correlation_id, task_id.0);
            assert_eq!(event.session_id, session_id);
            seen.push(*stage);
        }
    }
    assert_eq!(
        seen,
        vec![RunStage::Understand, RunStage::Inspect, RunStage::Execute, RunStage::Complete],
        "exactly one event per stage transition, duplicates deduped"
    );
}

/// The auto-Apply interception is keyed purely on a stored binding for the
/// EXACT objective — no keyword/route gate. A resume phrase resolves no
/// binding and the run proceeds under full local agency.
#[test]
fn resume_phrase_never_arms_auto_apply_without_a_binding() {
    assert!(is_resume_request("continue"), "premise: the checkpoint branch still sees it");
    assert!(
        plan_registry().pending(Ulid::new(), "0123456789abcdef0123456789abcdef").is_none(),
        "no stored binding means no interception"
    );
}

/// Live-fix (restart-safe auto-Apply): a durable binding in the session
/// DB rehydrates into the once-empty in-process registry so a confident
/// Execute after an app restart still auto-Applies the real persisted
/// plan (ADR-55 §4), with its original age preserved.
#[tokio::test]
async fn durable_binding_rehydrates_for_auto_apply() {
    use concerto_sessions::{PlanBindingRecord, SqliteSessionStore};

    let store: Arc<dyn SessionStore> =
        Arc::new(SqliteSessionStore::connect_in_memory().await.expect("in-memory store"));
    let session = Ulid::new();
    let created_at =
        time::OffsetDateTime::from_unix_timestamp(1_700_000_000).expect("valid timestamp");
    let record = PlanBindingRecord {
        session_id: session,
        objective_hash: "obj-hash-1".to_owned(),
        plan_id: "plan-1".to_owned(),
        plan_text: "step 1: build verdict".to_owned(),
        source_revision: Some("abc1234".to_owned()),
        artifact_hash: Some(plan_artifact_hash("step 1: build verdict")),
        created_at,
    };
    store.save_plan_binding(&record, CancellationToken::new()).await.expect("durable save");

    // Simulate a restart: the process registry holds nothing for this
    // session; rehydration must restore the binding WITH its original age.
    let binding = rehydrate_durable_binding(store.as_ref(), session, CancellationToken::new())
        .await
        .expect("rehydrated binding");
    assert_eq!(binding.plan_id(), "plan-1");
    assert_eq!(binding.created_at(), created_at, "original age preserved");

    // The interception resolves the re-seeded binding for its objective.
    let resolved =
        resolve_auto_apply_binding(session, "obj-hash-1", Some(&store), CancellationToken::new())
            .await
            .expect("resolution succeeds");
    assert_eq!(
        resolved.map(|b| b.plan_id().to_owned()),
        Some("plan-1".to_owned()),
        "a restart-safe binding for the same objective auto-Applies"
    );
}

// ------------------------------------------------------------------
// ADR-55 §4: plan→Execute auto-Apply — a confident
// Execute over a stored binding executes the persisted plan outright,
// hash-verified; drift on the exact-objective leg is a LOUD failure,
// never a silent re-decompose.
// ------------------------------------------------------------------

/// A5: an objective's exact-objective binding resolves for auto-Apply —
/// the ORIGINAL plan objective, plan text, source revision and age all
/// preserved. No keyword/route is consulted.
#[test]
fn a5_exact_objective_binding_auto_applies() {
    let session = Ulid::new();
    let hash = "0123456789abcdef0123456789abcdef".to_owned();
    plan_registry().insert(
        session,
        PlanBinding::restored(
            "plan-1".into(),
            hash.clone(),
            Some("abc1234".into()),
            "step 1: build verdict".into(),
            Some(plan_artifact_hash("step 1: build verdict")),
            time::OffsetDateTime::from_unix_timestamp(1_700_000_000).expect("valid timestamp"),
        ),
    );

    let resolved = tokio::runtime::Runtime::new()
        .expect("runtime")
        .block_on(resolve_auto_apply_binding(session, &hash, None, CancellationToken::new()))
        .expect("resolution succeeds");
    let binding = resolved.expect("the exact-objective binding auto-Applies");
    assert_eq!(binding.plan_id(), "plan-1");
    assert_eq!(binding.objective_hash(), hash, "the ORIGINAL plan objective");
    assert_eq!(binding.plan_text(), "step 1: build verdict");
    assert_eq!(binding.source_revision(), Some("abc1234"));
    plan_registry().remove(session, &hash);
}

/// A5 (loud-fail on drift): a stored binding whose plan text no longer
/// matches its creation-time artifact hash must fail the run LOUDLY —
/// never silently fall through to a fresh re-decompose of the same
/// objective (ADR-55 §4).
#[tokio::test]
async fn a5_drifted_exact_objective_binding_loud_fails_never_redecomposes() {
    let session = Ulid::new();
    let hash = "0123456789abcdef0123456789abcdef".to_owned();
    plan_registry().insert(
        session,
        PlanBinding::restored(
            "plan-1".into(),
            hash.clone(),
            None,
            "step 1: build verdict AND delete everything".into(),
            Some(plan_artifact_hash("step 1: build verdict")),
            time::OffsetDateTime::now_utc(),
        ),
    );

    let resolved = resolve_auto_apply_binding(session, &hash, None, CancellationToken::new()).await;
    assert!(
        resolved.is_err(),
        "a drifted exact-objective binding is a loud failure, not a fall-through"
    );
    plan_registry().remove(session, &hash);
}

/// A5: with no in-process exact-objective hit, the session-newest DURABLE
/// binding is rehydrated and auto-Applied — but ONLY when its objective
/// hash matches the run's objective. For the same objective, "execute the
/// stored plan" works after a restart with no `approve` click.
#[tokio::test]
async fn a5_same_objective_durable_binding_auto_applies() {
    use concerto_sessions::SqliteSessionStore;

    let store: Arc<dyn SessionStore> =
        Arc::new(SqliteSessionStore::connect_in_memory().await.expect("in-memory store"));
    let session = Ulid::new();
    store
        .save_plan_binding(
            &PlanBindingRecord {
                session_id: session,
                objective_hash: "obj-hash-original".to_owned(),
                plan_id: "plan-1".to_owned(),
                plan_text: "step 1: build verdict".to_owned(),
                source_revision: Some("abc1234".to_owned()),
                artifact_hash: Some(plan_artifact_hash("step 1: build verdict")),
                created_at: time::OffsetDateTime::from_unix_timestamp(1_700_000_000)
                    .expect("valid timestamp"),
            },
            CancellationToken::new(),
        )
        .await
        .expect("durable save");

    let resolved = resolve_auto_apply_binding(
        session,
        "obj-hash-original",
        Some(&store),
        CancellationToken::new(),
    )
    .await
    .expect("resolution succeeds");
    let binding = resolved.expect("the same-objective durable plan auto-Applies");
    assert_eq!(binding.plan_id(), "plan-1");
    assert_eq!(
        binding.objective_hash(),
        "obj-hash-original",
        "the binding keeps the ORIGINAL plan objective"
    );
    assert_eq!(binding.plan_text(), "step 1: build verdict");
}
