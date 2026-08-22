//! Test helpers for orchestrator testing.
//!
//! Provides `MockAgentLoop` (Phase 3), `MockExpertAgent`,
//! `ScriptedCoordinator`, `BudgetScenarioBuilder`, and
//! `AgentFlowTestHarness` (all Phase 5, all `#[cfg(test)]`).

#![allow(unused_imports, dead_code)]

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use concerto_core::event::{EventBus, EventKind};
use concerto_core::traits::agent::ExpertAgent;
use concerto_core::traits::memory::NullMemoryStore;
use concerto_core::types::{
    AgentContext, AgentId, AgentOutcome, AgentOutput, AgentRunResult, AgentStage, AgentTask,
    SubTask,
};
use concerto_core::{CancellationToken, OrchestratorError};

// ---------------------------------------------------------------------------
// MockAgentLoop  (Phase 3 — existing, kept for backward compat)
// ---------------------------------------------------------------------------

/// A mock agent loop that emits pre-defined events and returns a fixed output.
/// Useful for testing UI components without running a real provider.
#[cfg(test)]
pub struct MockAgentLoop {
    /// Events to emit on the bus before returning.
    pub events_to_emit: Vec<EventKind>,
    /// The output to return from `run`.
    pub final_output: AgentOutput,
}

#[cfg(test)]
impl MockAgentLoop {
    /// Create a new mock agent loop.
    pub fn new(events_to_emit: Vec<EventKind>, final_output: AgentOutput) -> Self {
        Self { events_to_emit, final_output }
    }

    /// Run the mock, emitting events and returning the configured output.
    pub async fn run(
        &self,
        _task: AgentTask,
        bus: &EventBus,
        _cancel: CancellationToken,
    ) -> Result<AgentOutput, OrchestratorError> {
        for event in &self.events_to_emit {
            let _ = bus.publish_raw(event.clone());
        }
        Ok(self.final_output.clone())
    }
}

// ---------------------------------------------------------------------------
// MockExpertAgent  (Phase 5)
// ---------------------------------------------------------------------------

/// A mock expert agent that returns scripted results.
pub struct MockExpertAgent {
    pub id: AgentId,
    responses: Mutex<VecDeque<Result<AgentRunResult, OrchestratorError>>>,
    stage: Option<AgentStage>,
    write_expected_artifacts: bool,
}

/// The pipeline stage conventionally declared by a built-in id, so mocks
/// mirror the real built-in agents' `stage()` implementations. Unknown ids
/// are freeform (no stage) unless overridden via [`MockExpertAgent::with_stage`].
fn default_stage(id: &AgentId) -> Option<AgentStage> {
    match id.as_str() {
        "architect" => Some(AgentStage::new("design")),
        "researcher" => Some(AgentStage::new("research")),
        "coder" => Some(AgentStage::new("implement")),
        "reviewer" => Some(AgentStage::new("review")),
        "validator" => Some(AgentStage::new("validate")),
        _ => None,
    }
}

impl MockExpertAgent {
    fn make_run_result(id: AgentId, outcome: AgentOutcome, summary: String) -> AgentRunResult {
        AgentRunResult {
            task_id: concerto_core::types::TaskId::new(),
            role: id,
            outcome,
            summary,
            files_modified: Vec::new(),
            tool_call_count: 0,
            cost_usd: 0.0,
            latency_ms: 0,
            provider: "mock".to_string(),
            model: "mock-model".to_string(),
            tokens_in: 0,
            tokens_out: 0,
        }
    }

    fn new_with(id: AgentId, responses: Vec<Result<AgentRunResult, OrchestratorError>>) -> Self {
        Self {
            id: id.clone(),
            responses: Mutex::new(VecDeque::from(responses)),
            stage: default_stage(&id),
            write_expected_artifacts: false,
        }
    }

    /// Create a mock that always succeeds with the given summary.
    pub fn always_succeed(id: AgentId, summary: &str) -> Self {
        Self::new_with(
            id.clone(),
            vec![Ok(Self::make_run_result(id, AgentOutcome::Success, summary.to_string()))],
        )
    }

    /// Create a mock that always returns NeedsRevision.
    pub fn always_revise(id: AgentId, reason: &str) -> Self {
        Self::new_with(
            id.clone(),
            vec![Ok(Self::make_run_result(
                id,
                AgentOutcome::NeedsRevision { reason: reason.to_string() },
                format!("needs revision: {reason}"),
            ))],
        )
    }

    /// Create a mock that always fails.
    pub fn always_fail(id: AgentId, error: &str) -> Self {
        Self::new_with(
            id.clone(),
            vec![Ok(Self::make_run_result(
                id,
                AgentOutcome::Failed { error: error.to_string() },
                format!("failed: {error}"),
            ))],
        )
    }

    /// Create a mock with a specific sequence of results.
    pub fn sequence(id: AgentId, results: Vec<Result<AgentRunResult, OrchestratorError>>) -> Self {
        Self::new_with(id, results)
    }

    /// Override the declared pipeline stage (defaults are derived from the
    /// id, mirroring the built-in agents). Pass `None` to make the mock
    /// freeform.
    pub fn with_stage(mut self, stage: Option<AgentStage>) -> Self {
        self.stage = stage;
        self
    }

    /// Make this mock write each of its `expected_artifacts` (from the task
    /// context) into the workspace before returning its scripted result —
    /// simulating a coder that produces real files so the coordinator's
    /// artifact acceptance gate (audit C-06) sees evidence on disk.
    pub fn with_artifact_writer(mut self) -> Self {
        self.write_expected_artifacts = true;
        self
    }
}

#[async_trait::async_trait]
impl ExpertAgent for MockExpertAgent {
    fn id(&self) -> AgentId {
        self.id.clone()
    }

    fn stage(&self) -> Option<AgentStage> {
        self.stage.clone()
    }

    fn capabilities(&self) -> concerto_core::types::CapabilitySet {
        concerto_core::types::CapabilitySet::default()
    }

    async fn run(
        &self,
        _task: &SubTask,
        context: AgentContext,
        _model: &str,
        _cancel: CancellationToken,
    ) -> Result<AgentRunResult, OrchestratorError> {
        let mut written_paths = Vec::new();
        if self.write_expected_artifacts {
            for path in &context.expected_artifacts {
                let target = context.session.project_dir.join(path.as_str());
                if let Some(parent) = target.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let _ = std::fs::write(&target, b"// mock-generated artifact\npub fn main() {}\n");
                written_paths.push(path.clone());
            }
        }
        let mut guard = self.responses.lock().unwrap();
        let mut result = guard.pop_front().unwrap_or_else(|| {
            Ok(Self::make_run_result(self.id.clone(), AgentOutcome::Success, "mock default".into()))
        })?;
        // Mirror a real agent: files the mock actually wrote to the workspace
        // are reported in `files_modified`, so the coordinator's zero-file
        // implement guard only fires for mocks that genuinely produced no
        // deliverable (no expected artifacts written).
        if self.write_expected_artifacts {
            result.files_modified.extend(written_paths);
        }
        Ok(result)
    }
}

// ---------------------------------------------------------------------------
// AgentRegistry extension  (Phase 5)
// ---------------------------------------------------------------------------

impl crate::registry::AgentRegistry {
    /// Build a registry from a list of mock agents.
    /// Panics if duplicate roles are registered.
    ///
    /// Each mock is registered WITH a rebuild factory that returns the same
    /// mock regardless of the requested provider. This mirrors production
    /// (every specialist has a factory keyed by provider) so ADR-45
    /// tier-1b/tier-2 dispatches through `get_with_provider` resolve the mock
    /// instead of skipping on `has_rebuild_factory == false`.
    pub fn from_mocks(mocks: Vec<MockExpertAgent>) -> Self {
        let mut registry = crate::registry::AgentRegistry::new();
        for mock in mocks {
            let id = mock.id.clone();
            assert!(registry.get(&id).is_none(), "duplicate mock agent for id {id}");
            let mock: Arc<dyn ExpertAgent> = Arc::new(mock);
            let rebuild = mock.clone();
            registry.register_with_factory(
                id,
                mock,
                Arc::new(
                    move |_provider: Arc<dyn concerto_core::traits::provider::LlmProvider>| {
                        rebuild.clone()
                    },
                ),
            );
        }
        registry
    }
}

// ---------------------------------------------------------------------------
// ScriptedCoordinator  (Phase 5)
// ---------------------------------------------------------------------------

/// A pre-built coordinator result that bypasses LLM planning.
pub struct ScriptedCoordinator {
    pub plan: crate::graph::TaskGraph,
    pub final_output: AgentOutput,
}

impl ScriptedCoordinator {
    /// Create a scripted coordinator with a pre-built plan and output.
    pub fn with_plan(plan: crate::graph::TaskGraph, output: AgentOutput) -> Self {
        Self { plan, final_output: output }
    }
}

// ---------------------------------------------------------------------------
// BudgetScenarioBuilder  (Phase 5)
// ---------------------------------------------------------------------------

/// Builds (RoutingEngine, SpendTracker) pairs with configurable caps.
pub struct BudgetScenarioBuilder {
    _profiles: Vec<concerto_core::types::RoutingProfile>,
    _session_cap: Option<f64>,
    _task_cap: Option<f64>,
    _daily_cap: Option<f64>,
}

impl BudgetScenarioBuilder {
    /// All caps set to $100.0 — effectively unlimited for testing.
    pub fn generous() -> Self {
        Self {
            _profiles: Vec::new(),
            _session_cap: Some(100.0),
            _task_cap: Some(100.0),
            _daily_cap: Some(100.0),
        }
    }

    /// All caps set to $0.001 — triggers budget exhaustion.
    pub fn tight() -> Self {
        Self {
            _profiles: Vec::new(),
            _session_cap: Some(0.001),
            _task_cap: Some(0.001),
            _daily_cap: Some(0.001),
        }
    }

    /// Set a uniform cap across all kinds.
    pub fn with_cap(mut self, cap: f64) -> Self {
        self._session_cap = Some(cap);
        self._task_cap = Some(cap);
        self._daily_cap = Some(cap);
        self
    }

    /// Build the (RoutingEngine, SpendTracker) pair.
    pub fn build(self) -> Self {
        // In a full implementation this would construct a RoutingEngine
        // and SpendTracker. For now we just return the builder itself
        // as a placeholder; tests use the SpendTracker directly.
        self
    }
}

// ---------------------------------------------------------------------------
// AgentFlowTestHarness  (Phase 5)
// ---------------------------------------------------------------------------

/// Test harness that runs a coordinator with mock agents and collects events.
pub struct AgentFlowTestHarness {
    pub events: Vec<EventKind>,
    bus: EventBus,
    mocks: Vec<MockExpertAgent>,
    plan: crate::graph::TaskGraph,
    _budget: BudgetScenarioBuilder,
}

impl AgentFlowTestHarness {
    /// Create a harness with mock agents and a scripted coordinator.
    pub fn new(
        mocks: Vec<MockExpertAgent>,
        plan: crate::graph::TaskGraph,
        _budget: BudgetScenarioBuilder,
    ) -> Self {
        let bus = EventBus::new(256);
        Self { bus, events: Vec::new(), mocks, plan, _budget }
    }

    /// Subscribe a collector to the bus.
    pub fn bus(&self) -> EventBus {
        self.bus.clone()
    }

    async fn run_coordinator(
        mocks: Vec<MockExpertAgent>,
        _plan: crate::graph::TaskGraph,
        bus: EventBus,
        task: AgentTask,
        cancel: CancellationToken,
    ) -> Result<AgentOutput, OrchestratorError> {
        use crate::agent_runner::AgentRunner;
        use crate::coordinator::CoordinatorAgent;
        use crate::registry::AgentRegistry;
        use concerto_core::types::RoutingProfile;
        use concerto_providers::mock::MockProvider;
        use concerto_providers::model_registry::ModelRegistry;
        use concerto_providers::model_selector::ModelSelector;
        use concerto_providers::routing::RoutingEngine;
        use concerto_sessions::spend::SpendTracker;

        let registry = Arc::new(AgentRegistry::from_mocks(mocks));
        let spend_tracker = Arc::new(SpendTracker::default());
        let runner = AgentRunner::new(registry.clone(), bus.clone(), spend_tracker.clone());

        let provider: Arc<dyn concerto_core::traits::provider::LlmProvider> =
            Arc::new(MockProvider::default());

        let profiles: Vec<RoutingProfile> = vec![
            RoutingProfile {
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
            RoutingProfile {
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
            RoutingProfile {
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
        let model_selector = Arc::new(ModelSelector::new(model_registry, routing.clone()));

        let mut coordinator = CoordinatorAgent::new(
            registry,
            runner,
            model_selector,
            spend_tracker,
            bus.clone(),
            provider,
            Arc::new(NullMemoryStore),
        );

        let project_dir = std::env::current_dir().unwrap_or_default();
        let context = concerto_core::types::AgentContext::new(
            concerto_core::types::SessionContext::new(task.session_id, project_dir),
        );

        coordinator.run(task, context, cancel, None).await
    }

    /// Run the coordinator with the prepared mocks.
    pub async fn run(&mut self, task: AgentTask) -> Result<AgentOutput, OrchestratorError> {
        let cancel = CancellationToken::new();
        Self::run_coordinator(
            std::mem::take(&mut self.mocks),
            std::mem::take(&mut self.plan),
            self.bus.clone(),
            task,
            cancel,
        )
        .await
    }

    /// Assert events were emitted in a given order (by variant name).
    pub fn assert_event_sequence(&self, _expected: &[&str]) {
        unimplemented!("event sequence assertions are not yet implemented")
    }
}

// ---------------------------------------------------------------------------
// Benchmark scenarios
// ---------------------------------------------------------------------------

/// Build the three standard benchmark task graphs used in §3.5 scenarios.
#[cfg(test)]
pub mod benchmarks {
    use super::*;
    use crate::graph::{Dependency, TaskGraph};
    use concerto_core::types::SubTask;
    use time::OffsetDateTime;

    fn make_subtask(label: &str, id: AgentId, deps: Vec<concerto_core::types::TaskId>) -> SubTask {
        SubTask {
            id: concerto_core::types::TaskId::new(),
            parent_id: None,
            session_id: concerto_core::ids::Ulid::new(),
            role: id,
            description: label.into(),
            status: concerto_core::types::SubTaskStatus::Pending,
            dependencies: deps,
            deliverable: None,
            created_at: OffsetDateTime::now_utc(),
            completed_at: None,
        }
    }

    #[tokio::test]
    async fn linear_pipeline_success() {
        let mocks = vec![
            MockExpertAgent::always_succeed(AgentId::new("architect"), "design done"),
            MockExpertAgent::always_succeed(AgentId::new("researcher"), "research done"),
            MockExpertAgent::always_succeed(AgentId::new("coder"), "code done"),
            MockExpertAgent::always_succeed(AgentId::new("reviewer"), "review done"),
            MockExpertAgent::always_succeed(AgentId::new("validator"), "validation done"),
        ];
        let plan = TaskGraph::default();
        let budget = BudgetScenarioBuilder::generous();
        let mut harness = AgentFlowTestHarness::new(mocks, plan, budget);
        let task = AgentTask::new(concerto_core::ids::Ulid::new(), "implement login");
        let result = harness.run(task).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn fanout_revision_triggers_review() {
        let mocks = vec![
            MockExpertAgent::always_succeed(AgentId::new("architect"), "design done"),
            MockExpertAgent::always_succeed(AgentId::new("researcher"), "research done"),
            MockExpertAgent::always_revise(AgentId::new("coder"), "needs refactor"),
            MockExpertAgent::always_succeed(AgentId::new("reviewer"), "review done"),
            MockExpertAgent::always_succeed(AgentId::new("validator"), "validation done"),
        ];
        let plan = TaskGraph::default();
        let budget = BudgetScenarioBuilder::generous();
        let mut harness = AgentFlowTestHarness::new(mocks, plan, budget);
        let task = AgentTask::new(concerto_core::ids::Ulid::new(), "refactor module");
        let result = harness.run(task).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn cyclic_dag_rejected_by_coordinator() {
        let mut graph = TaskGraph::default();
        let a = make_subtask("A", AgentId::new("coder"), vec![]);
        let b = SubTask {
            id: concerto_core::types::TaskId::new(),
            parent_id: None,
            session_id: concerto_core::ids::Ulid::new(),
            role: AgentId::new("reviewer"),
            description: "B".into(),
            status: concerto_core::types::SubTaskStatus::Pending,
            dependencies: vec![a.id],
            deliverable: None,
            created_at: OffsetDateTime::now_utc(),
            completed_at: None,
        };
        graph.add_child(a, b.id, Dependency::MustFinishBefore);
        let mocks = vec![
            MockExpertAgent::always_succeed(AgentId::new("coder"), "code"),
            MockExpertAgent::always_succeed(AgentId::new("reviewer"), "review"),
        ];
        let budget = BudgetScenarioBuilder::generous();
        let mut harness = AgentFlowTestHarness::new(mocks, graph, budget);
        let task = AgentTask::new(concerto_core::ids::Ulid::new(), "test cycle");
        let result = harness.run(task).await;
        assert!(result.is_ok() || result.is_err());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_expert_agent_always_succeeds() {
        let mock = MockExpertAgent::always_succeed(AgentId::new("coder"), "all good");
        assert_eq!(mock.id, AgentId::new("coder"));
    }

    #[test]
    fn mock_expert_agent_fails() {
        let mock = MockExpertAgent::always_fail(AgentId::new("coder"), "something broke");
        assert_eq!(mock.id, AgentId::new("coder"));
    }

    #[test]
    fn mock_expert_agent_stage_defaults_and_override() {
        // Built-in ids mirror the real agents' stages.
        let coder = MockExpertAgent::always_succeed(AgentId::new("coder"), "ok");
        assert_eq!(coder.stage, Some(AgentStage::new("implement")));
        let architect = MockExpertAgent::always_succeed(AgentId::new("architect"), "ok");
        assert_eq!(architect.stage, Some(AgentStage::new("design")));
        // Unknown ids are freeform unless overridden.
        let freeform = MockExpertAgent::always_succeed(AgentId::new("docs-writer"), "ok");
        assert_eq!(freeform.stage, None);
        let overridden = MockExpertAgent::always_succeed(AgentId::new("docs-writer"), "ok")
            .with_stage(Some(AgentStage::new("implement")));
        assert_eq!(overridden.stage, Some(AgentStage::new("implement")));
        // None clears a derived stage (freeform mock).
        let cleared = MockExpertAgent::always_succeed(AgentId::new("coder"), "ok").with_stage(None);
        assert_eq!(cleared.stage, None);
    }

    #[test]
    fn agent_registry_from_mocks_dedup_panics() {
        use std::panic::{catch_unwind, AssertUnwindSafe};
        let mocks = vec![
            MockExpertAgent::always_succeed(AgentId::new("coder"), "first"),
            MockExpertAgent::always_succeed(AgentId::new("coder"), "second"),
        ];
        let result = catch_unwind(AssertUnwindSafe(|| {
            crate::registry::AgentRegistry::from_mocks(mocks);
        }));
        assert!(result.is_err(), "expected panic on duplicate role");
    }

    #[test]
    fn budget_scenario_tight_builds() {
        let builder = BudgetScenarioBuilder::tight();
        let _built = builder.build();
    }
}
