//! `AgentRunner` — runs an expert agent on a subtask, emits lifecycle
//! events, and records cost and latency metrics.

use std::collections::HashMap;
use std::sync::Arc;

use concerto_core::error::ProviderError;
use concerto_core::event::{EventBus, EventKind};
use concerto_core::ids::Ulid;
use concerto_core::traits::agent::ExpertAgent;
use concerto_core::traits::provider::LlmProvider;
use concerto_core::types::{AgentContext, AgentId, AgentOutcome, AgentRunResult, SubTask};
use concerto_core::{CancellationToken, OrchestratorError};
use concerto_providers::model::ModelProfile;
use concerto_providers::routing::CostEstimator;
use concerto_sessions::spend::SpendTracker;

use crate::registry::AgentRegistry;

/// True when an error represents a cancelled run rather than a genuine
/// failure — callers emit a cancellation lifecycle event for these instead
/// of a failure event.
fn is_cancellation_error(error: &OrchestratorError) -> bool {
    matches!(
        error,
        OrchestratorError::Cancelled
            | OrchestratorError::Tool(concerto_core::ToolError::Cancelled)
            | OrchestratorError::Provider(ProviderError::Cancelled)
    )
}

/// Publish the live spend snapshot for a session after a provider call
/// settles, plus the cap signal the session total now crosses.
///
/// Always publishes `EventKind::SpendUpdated` with the tracker's session
/// total so UIs show live cost. Cap thresholds mirror
/// `SpendAccumulator::check_cap`: `pct >= 100.0` publishes
/// `SpendCapExceeded`, `pct >= 80.0` publishes `SpendCapApproaching`, and a
/// session with no cap (or a non-positive cap) only gets `SpendUpdated`.
/// The tracker's totals are read once so every event in the batch reports
/// the same snapshot.
pub(crate) fn publish_spend_events(
    bus: &EventBus,
    session_id: Ulid,
    correlation_id: Ulid,
    tracker: &SpendTracker,
) {
    let total_usd = tracker.session_total();
    let _ = bus.publish_for_session(
        session_id,
        correlation_id,
        EventKind::SpendUpdated { session_id, total_usd },
    );

    let Some(cap_usd) = tracker.session_cap() else {
        return;
    };
    if cap_usd <= 0.0 {
        return;
    }
    let pct = (total_usd / cap_usd) * 100.0;
    if pct >= 100.0 {
        let _ = bus.publish_for_session(
            session_id,
            correlation_id,
            EventKind::SpendCapExceeded { session_id, current_usd: total_usd, cap_usd },
        );
    } else if pct >= 80.0 {
        let _ = bus.publish_for_session(
            session_id,
            correlation_id,
            EventKind::SpendCapApproaching { current_usd: total_usd, cap_usd, pct },
        );
    }
}

/// Runs an expert agent on a subtask with lifecycle management.
pub struct AgentRunner {
    registry: Arc<AgentRegistry>,
    bus: EventBus,
    spend_tracker: Arc<SpendTracker>,
    global_concurrency: Arc<tokio::sync::Semaphore>,
    provider_concurrency: Arc<std::sync::Mutex<HashMap<String, Arc<tokio::sync::Semaphore>>>>,
    per_provider_limit: usize,
}

impl AgentRunner {
    /// Create a new agent runner.
    pub fn new(
        registry: Arc<AgentRegistry>,
        bus: EventBus,
        spend_tracker: Arc<SpendTracker>,
    ) -> Self {
        Self {
            registry,
            bus,
            spend_tracker,
            global_concurrency: Arc::new(tokio::sync::Semaphore::new(3)),
            provider_concurrency: Arc::new(std::sync::Mutex::new(HashMap::new())),
            per_provider_limit: 2,
        }
    }

    pub fn with_concurrency_limits(mut self, global: usize, per_provider: usize) -> Self {
        self.global_concurrency = Arc::new(tokio::sync::Semaphore::new(global.max(1)));
        self.per_provider_limit = per_provider.max(1);
        self
    }

    /// Run an agent for the given role and subtask, resolved from the
    /// registry with its bound provider.
    pub async fn run(
        &self,
        role: AgentId,
        task: &SubTask,
        context: AgentContext,
        profile: &ModelProfile,
        cancel: CancellationToken,
    ) -> Result<AgentRunResult, OrchestratorError> {
        if cancel.is_cancelled() {
            return Err(OrchestratorError::Cancelled);
        }
        let agent = self.registry.get(&role).ok_or_else(|| {
            OrchestratorError::AgentLoopError(format!("no agent registered for role {role}"))
        })?;
        self.run_with_agent(agent, role, task, context, profile, cancel).await
    }

    /// Run an agent for the given role rebuilt on a different provider
    /// (ADR-45 ladder tier 1b). Roles without a rebuild factory keep their
    /// built agent; providers only change where the factory registered one.
    pub async fn run_with_provider(
        &self,
        role: AgentId,
        provider: Arc<dyn LlmProvider>,
        task: &SubTask,
        context: AgentContext,
        profile: &ModelProfile,
        cancel: CancellationToken,
    ) -> Result<AgentRunResult, OrchestratorError> {
        if cancel.is_cancelled() {
            return Err(OrchestratorError::Cancelled);
        }
        let agent = self.registry.get_with_provider(&role, provider).ok_or_else(|| {
            OrchestratorError::AgentLoopError(format!("no agent registered for role {role}"))
        })?;
        self.run_with_agent(agent, role, task, context, profile, cancel).await
    }

    /// Shared execution path for [`Self::run`] and [`Self::run_with_provider`].
    async fn run_with_agent(
        &self,
        agent: Arc<dyn ExpertAgent>,
        role: AgentId,
        task: &SubTask,
        context: AgentContext,
        profile: &ModelProfile,
        cancel: CancellationToken,
    ) -> Result<AgentRunResult, OrchestratorError> {
        let role_name = format!("{role}");
        let model_name = profile.model_name();
        let correlation_id = task.id.0;

        let _ = self.bus.publish_for_session(
            task.session_id,
            correlation_id,
            EventKind::AgentThought {
                agent_id: role_name.clone(),
                content: format!(
                    "Queued subtask: {}\nUsing {}/{}",
                    task.description.chars().take(1_000).collect::<String>(),
                    profile.profile.provider,
                    model_name
                ),
            },
        );

        let start = std::time::Instant::now();

        let global_permit = tokio::select! {
            permit = self.global_concurrency.clone().acquire_owned() => permit.map_err(|error| {
                OrchestratorError::AgentLoopError(format!("global provider scheduler closed: {error}"))
            })?,
            _ = cancel.cancelled() => return Err(OrchestratorError::Cancelled),
        };
        let provider_semaphore = {
            let mut semaphores = self.provider_concurrency.lock().map_err(|error| {
                OrchestratorError::AgentLoopError(format!(
                    "provider scheduler lock poisoned: {error}"
                ))
            })?;
            semaphores
                .entry(profile.profile.provider_config_id.clone())
                .or_insert_with(|| Arc::new(tokio::sync::Semaphore::new(self.per_provider_limit)))
                .clone()
        };
        let provider_permit = tokio::select! {
            permit = provider_semaphore.acquire_owned() => permit.map_err(|error| {
                OrchestratorError::AgentLoopError(format!("provider scheduler closed: {error}"))
            })?,
            _ = cancel.cancelled() => return Err(OrchestratorError::Cancelled),
        };
        let reserved_cost = CostEstimator::estimate(&role, &profile.profile);
        self.spend_tracker
            .check_and_add(reserved_cost)
            .map_err(|_| OrchestratorError::NoBudgetForDelegation)?;

        let _ = self.bus.publish_for_session(
            task.session_id,
            correlation_id,
            EventKind::SubTaskStarted { task_id: task.id, role: role.clone() },
        );
        let _ = self.bus.publish_for_session(
            task.session_id,
            correlation_id,
            EventKind::AgentThought {
                agent_id: role_name.clone(),
                content: format!(
                    "Started subtask with {}/{} after waiting {} ms",
                    profile.profile.provider,
                    model_name,
                    start.elapsed().as_millis()
                ),
            },
        );

        let start = std::time::Instant::now();

        // Run the agent once — provider retry happens inside each specialist
        // agent (via with_provider_retry). The whole-agent run is never replayed.
        let result = agent.run(task, context.clone(), model_name, cancel.clone()).await;

        let latency_ms = start.elapsed().as_millis() as u64;
        drop(provider_permit);
        drop(global_permit);
        let actual_cost = result.as_ref().map_or(0.0, |run_result| run_result.cost_usd);
        self.spend_tracker.settle_reservation(reserved_cost, actual_cost);
        // Publish the live spend snapshot (and cap signal) after every
        // provider run settles so UIs show the tracker's session total.
        publish_spend_events(&self.bus, task.session_id, correlation_id, &self.spend_tracker);

        match result {
            Ok(mut run_result) => {
                run_result.latency_ms = latency_ms;
                run_result.model = profile.model_name().to_string();

                self.publish_outcome_events(correlation_id, task, &role, &run_result);

                Ok(run_result)
            }
            Err(e) => {
                let error = e.to_string();
                let cancelled = is_cancellation_error(&e) || cancel.is_cancelled();
                let _ = self.bus.publish_for_session(
                    task.session_id,
                    correlation_id,
                    EventKind::AgentThought {
                        agent_id: role_name,
                        content: if cancelled {
                            format!("Subtask cancelled: {error}")
                        } else {
                            format!("Subtask failed: {error}")
                        },
                    },
                );
                let lifecycle_event = if cancelled {
                    EventKind::SubTaskCancelled { task_id: task.id, role, reason: error }
                } else {
                    EventKind::SubTaskFailed { task_id: task.id, role, error }
                };
                let _ =
                    self.bus.publish_for_session(task.session_id, correlation_id, lifecycle_event);
                Err(e)
            }
        }
    }

    /// Publish the lifecycle event that matches a completed agent outcome.
    ///
    /// Only a genuine `Success` broadcasts `SubTaskCompleted`; revision,
    /// blocked, and failed outcomes each get their own distinct event so
    /// consumers never mistake a non-success for completion.
    ///
    /// Lifecycle events use the run's `role` (the role actually dispatched,
    /// which may differ from `task.role` for coordinator fallback-ladder tier-2
    /// reassignments), keeping the `SubTaskStarted`/`SubTaskCompleted` pair
    /// consistent. For regular dispatches `role == task.role`, so the payloads
    /// are unchanged.
    pub(crate) fn publish_outcome_events(
        &self,
        correlation_id: Ulid,
        task: &SubTask,
        role: &AgentId,
        run_result: &AgentRunResult,
    ) {
        let session_id = task.session_id;
        let task_id = task.id;
        let role_name = format!("{role}");
        let role = role.clone();
        let summary = run_result.summary.clone();
        match run_result.outcome.clone() {
            AgentOutcome::Success => {
                let _ = self.bus.publish_for_session(
                    session_id,
                    correlation_id,
                    EventKind::AgentThought {
                        agent_id: role_name.to_string(),
                        content: format!("Completed subtask: {summary}"),
                    },
                );
                let _ = self.bus.publish_for_session(
                    session_id,
                    correlation_id,
                    EventKind::SubTaskCompleted { task_id, role, outcome: summary },
                );
            }
            AgentOutcome::NeedsRevision { reason } => {
                let _ = self.bus.publish_for_session(
                    session_id,
                    correlation_id,
                    EventKind::AgentThought {
                        agent_id: role_name.to_string(),
                        content: format!("Subtask needs revision: {reason}"),
                    },
                );
                let _ = self.bus.publish_for_session(
                    session_id,
                    correlation_id,
                    EventKind::SubTaskNeedsRevision { task_id, role, reason },
                );
            }
            AgentOutcome::Blocked { on } => {
                let _ = self.bus.publish_for_session(
                    session_id,
                    correlation_id,
                    EventKind::AgentThought {
                        agent_id: role_name.to_string(),
                        content: format!("Subtask blocked on {on:?}"),
                    },
                );
                let _ = self.bus.publish_for_session(
                    session_id,
                    correlation_id,
                    EventKind::SubTaskBlocked { task_id, role, on },
                );
            }
            AgentOutcome::Failed { error } => {
                let _ = self.bus.publish_for_session(
                    session_id,
                    correlation_id,
                    EventKind::AgentThought {
                        agent_id: role_name.to_string(),
                        content: format!("Subtask failed: {error}"),
                    },
                );
                let _ = self.bus.publish_for_session(
                    session_id,
                    correlation_id,
                    EventKind::SubTaskFailed { task_id, role, error },
                );
            }
            // Forward-compat: any future outcome variant must pick a lifecycle
            // event explicitly rather than silently broadcasting completion.
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use concerto_core::event::EventReceiver;
    use concerto_core::ids::Ulid;
    use concerto_core::types::{
        AgentOutcome, RoutingProfile, SessionContext, SubTaskStatus, TaskId,
    };

    use crate::testing::MockExpertAgent;

    use concerto_core::error::ProviderError;

    /// Build a standard Researcher subtask for use as the runner input.
    fn make_task(task_id: TaskId, session_id: Ulid) -> SubTask {
        SubTask {
            id: task_id,
            parent_id: None,
            session_id,
            role: AgentId::new("researcher"),
            description: "test task".into(),
            status: SubTaskStatus::Pending,
            dependencies: Vec::new(),
            deliverable: None,
            created_at: time::OffsetDateTime::now_utc(),
            completed_at: None,
        }
    }

    /// Build a ModelProfile for the mock provider.
    fn make_profile() -> ModelProfile {
        ModelProfile {
            profile: RoutingProfile {
                provider_config_id: "mock".into(),
                provider: "mock".into(),
                model: "mock-model".into(),
                cost_per_1k_tokens: 0.0,
                avg_latency_ms: 0,
                context_window: 8_192,
                supports_tool_calling: true,
                base_url: None,
                description: None,
            },
            context_window: 8_192,
            supports_tool_calling: true,
            base_url: None,
            description: None,
        }
    }

    /// Build a standard AgentRunner with a single mock agent.
    fn make_runner(mocks: Vec<MockExpertAgent>) -> AgentRunner {
        make_runner_with_bus(mocks).0
    }

    /// Build a runner plus its event bus so tests can collect published events.
    fn make_runner_with_bus(mocks: Vec<MockExpertAgent>) -> (AgentRunner, EventBus) {
        let registry = Arc::new(AgentRegistry::from_mocks(mocks));
        let bus = EventBus::new(32);
        (AgentRunner::new(registry, bus.clone(), Arc::new(SpendTracker::default())), bus)
    }

    /// Convenience: run a researcher subtask once with fresh session/context.
    async fn run_researcher(
        runner: &AgentRunner,
        cancel: CancellationToken,
    ) -> Result<AgentRunResult, OrchestratorError> {
        let session = SessionContext::new(Ulid::new(), std::path::PathBuf::from("/tmp"));
        let task = make_task(TaskId::new(), session.session_id);
        runner
            .run(
                AgentId::new("researcher"),
                &task,
                AgentContext::new(session),
                &make_profile(),
                cancel,
            )
            .await
    }

    /// Run a researcher mock once and return the run result plus every
    /// lifecycle event published to the bus during the run.
    async fn run_researcher_and_collect(
        mocks: Vec<MockExpertAgent>,
    ) -> (Result<AgentRunResult, OrchestratorError>, Vec<EventKind>) {
        let (runner, bus) = make_runner_with_bus(mocks);
        let mut rx = bus.subscribe();
        let session = SessionContext::new(Ulid::new(), std::path::PathBuf::from("/tmp"));
        let task = make_task(TaskId::new(), session.session_id);
        let result = runner
            .run(
                AgentId::new("researcher"),
                &task,
                AgentContext::new(session),
                &make_profile(),
                CancellationToken::new(),
            )
            .await;
        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event.kind.clone());
        }
        (result, events)
    }

    // ------------------------------------------------------------------
    // Spend live events (issue #93, phase 2)
    // ------------------------------------------------------------------

    /// Drain all events currently buffered on the bus receiver.
    fn drain_events(rx: &mut EventReceiver) -> Vec<EventKind> {
        let mut kinds = Vec::new();
        while let Ok(event) = rx.try_recv() {
            kinds.push(event.kind.clone());
        }
        kinds
    }

    #[test]
    fn spend_events_below_80_publishes_updated_only() {
        let bus = EventBus::new(32);
        let mut rx = bus.subscribe();
        let tracker = SpendTracker::new(Some(1.0), None, None);
        tracker.record(0.5);
        let session_id = Ulid::new();
        let correlation_id = Ulid::new();

        publish_spend_events(&bus, session_id, correlation_id, &tracker);

        let kinds = drain_events(&mut rx);
        assert_eq!(kinds.len(), 1, "expected a single SpendUpdated event: {kinds:?}");
        match &kinds[0] {
            EventKind::SpendUpdated { session_id: sid, total_usd } => {
                assert_eq!(*sid, session_id);
                assert!((total_usd - 0.5).abs() < 1e-9);
            }
            other => panic!("expected SpendUpdated, got {other:?}"),
        }
    }

    #[test]
    fn spend_events_crossing_80_publishes_approaching() {
        let bus = EventBus::new(32);
        let mut rx = bus.subscribe();
        let tracker = SpendTracker::new(Some(1.0), None, None);
        tracker.record(0.8);
        let session_id = Ulid::new();
        let correlation_id = Ulid::new();

        publish_spend_events(&bus, session_id, correlation_id, &tracker);

        let kinds = drain_events(&mut rx);
        assert!(
            kinds.iter().any(|kind| matches!(
                kind,
                EventKind::SpendUpdated { total_usd, .. } if (*total_usd - 0.8).abs() < 1e-9
            )),
            "expected SpendUpdated, got {kinds:?}"
        );
        assert!(
            kinds.iter().any(|kind| matches!(
                kind,
                EventKind::SpendCapApproaching { current_usd, cap_usd, pct }
                    if (*current_usd - 0.8).abs() < 1e-9
                        && (*cap_usd - 1.0).abs() < 1e-9
                        && *pct >= 80.0
            )),
            "expected SpendCapApproaching, got {kinds:?}"
        );
        assert!(
            !kinds.iter().any(|kind| matches!(kind, EventKind::SpendCapExceeded { .. })),
            "approaching must not publish SpendCapExceeded: {kinds:?}"
        );
    }

    #[test]
    fn spend_events_crossing_100_publishes_exceeded() {
        let bus = EventBus::new(32);
        let mut rx = bus.subscribe();
        // `check_and_add` would deny a second add at the cap, so use the
        // actual-spend `record` path — it retains spend above the cap, the
        // same way `settle_reservation` keeps over-cap actual cost.
        let tracker = SpendTracker::new(Some(1.0), None, None);
        tracker.record(0.6);
        tracker.record(0.5);
        let session_id = Ulid::new();
        let correlation_id = Ulid::new();

        publish_spend_events(&bus, session_id, correlation_id, &tracker);

        let kinds = drain_events(&mut rx);
        assert!(
            kinds.iter().any(|kind| matches!(
                kind,
                EventKind::SpendUpdated { total_usd, .. } if (*total_usd - 1.1).abs() < 1e-9
            )),
            "expected SpendUpdated, got {kinds:?}"
        );
        assert!(
            kinds.iter().any(|kind| matches!(
                kind,
                EventKind::SpendCapExceeded { session_id: sid, current_usd, cap_usd }
                    if *sid == session_id
                        && (*current_usd - 1.1).abs() < 1e-9
                        && (*cap_usd - 1.0).abs() < 1e-9
            )),
            "expected SpendCapExceeded, got {kinds:?}"
        );
        assert!(
            !kinds.iter().any(|kind| matches!(kind, EventKind::SpendCapApproaching { .. })),
            "exceeded must not publish SpendCapApproaching: {kinds:?}"
        );
    }

    #[test]
    fn spend_events_without_cap_publishes_updated_only() {
        let bus = EventBus::new(32);
        let mut rx = bus.subscribe();
        let tracker = SpendTracker::default(); // no session cap
        tracker.record(0.5);
        let session_id = Ulid::new();
        let correlation_id = Ulid::new();

        publish_spend_events(&bus, session_id, correlation_id, &tracker);

        let kinds = drain_events(&mut rx);
        assert_eq!(kinds.len(), 1, "no-cap session gets SpendUpdated only: {kinds:?}");
        assert!(matches!(kinds[0], EventKind::SpendUpdated { .. }));
    }

    /// A completed run settles its actual cost and publishes `SpendUpdated`
    /// with the tracker's session total so live cost displays update.
    #[tokio::test]
    async fn successful_run_publishes_spend_updated_after_settlement() {
        let run_result = AgentRunResult {
            task_id: TaskId::new(),
            role: AgentId::new("researcher"),
            outcome: AgentOutcome::Success,
            summary: "done".into(),
            files_modified: Vec::new(),
            tool_call_count: 0,
            cost_usd: 0.015,
            latency_ms: 0,
            provider: "mock".into(),
            model: "mock-model".into(),
            tokens_in: 0,
            tokens_out: 0,
        };
        let (result, events) = run_researcher_and_collect(vec![MockExpertAgent::sequence(
            AgentId::new("researcher"),
            vec![Ok(run_result)],
        )])
        .await;
        assert!(matches!(result.unwrap().outcome, AgentOutcome::Success));
        let updated: Vec<f64> = events
            .iter()
            .filter_map(|kind| match kind {
                EventKind::SpendUpdated { total_usd, .. } => Some(*total_usd),
                _ => None,
            })
            .collect();
        assert_eq!(updated.len(), 1, "expected one SpendUpdated event: {events:?}");
        assert!(
            (updated[0] - 0.015).abs() < 1e-9,
            "expected settled actual cost, got {:?}",
            updated
        );
    }

    // ------------------------------------------------------------------
    // Error paths
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn unregistered_role_returns_error() {
        let runner = make_runner(vec![MockExpertAgent::always_succeed(
            AgentId::new("coder"),
            "i am a coder",
        )]);
        let session = SessionContext::new(Ulid::new(), std::path::PathBuf::from("/tmp"));
        let task = make_task(TaskId::new(), session.session_id);

        let err = runner
            .run(
                AgentId::new("architect"), // not registered
                &task,
                AgentContext::new(session),
                &make_profile(),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();

        assert!(
            matches!(&err, OrchestratorError::AgentLoopError(msg) if msg.contains("architect"))
        );
    }

    #[tokio::test]
    async fn needs_revision_outcome_is_propagated() {
        let runner = make_runner(vec![MockExpertAgent::always_revise(
            AgentId::new("researcher"),
            "missing citations",
        )]);
        let result = run_researcher(&runner, CancellationToken::new()).await.unwrap();
        assert!(matches!(result.outcome, AgentOutcome::NeedsRevision { .. }));
    }

    #[tokio::test]
    async fn failed_outcome_is_propagated() {
        let runner = make_runner(vec![MockExpertAgent::always_fail(
            AgentId::new("researcher"),
            "api returned 500",
        )]);
        let result = run_researcher(&runner, CancellationToken::new()).await.unwrap();
        assert!(matches!(result.outcome, AgentOutcome::Failed { .. }));
    }

    // ------------------------------------------------------------------
    // Lifecycle event mapping (audit H-05)
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn success_outcome_publishes_completed_only() {
        let (result, events) = run_researcher_and_collect(vec![MockExpertAgent::always_succeed(
            AgentId::new("researcher"),
            "done",
        )])
        .await;
        assert!(matches!(result.unwrap().outcome, AgentOutcome::Success));
        assert!(
            events.iter().any(|kind| matches!(kind, EventKind::SubTaskCompleted { .. })),
            "expected SubTaskCompleted for a Success outcome, got {events:?}"
        );
        assert!(
            !events.iter().any(|kind| matches!(
                kind,
                EventKind::SubTaskNeedsRevision { .. }
                    | EventKind::SubTaskBlocked { .. }
                    | EventKind::SubTaskFailed { .. }
                    | EventKind::SubTaskCancelled { .. }
            )),
            "Success outcome must not publish other lifecycle events: {events:?}"
        );
    }

    #[tokio::test]
    async fn needs_revision_outcome_publishes_revision_not_completed() {
        let (result, events) = run_researcher_and_collect(vec![MockExpertAgent::always_revise(
            AgentId::new("researcher"),
            "missing citations",
        )])
        .await;
        assert!(matches!(result.unwrap().outcome, AgentOutcome::NeedsRevision { .. }));
        let reasons: Vec<_> = events
            .iter()
            .filter_map(|kind| match kind {
                EventKind::SubTaskNeedsRevision { reason, .. } => Some(reason.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(reasons, vec!["missing citations".to_string()]);
        assert!(
            !events.iter().any(|kind| matches!(kind, EventKind::SubTaskCompleted { .. })),
            "NeedsRevision outcome must not publish SubTaskCompleted: {events:?}"
        );
    }

    #[tokio::test]
    async fn blocked_outcome_publishes_blocked_not_completed() {
        let blocker = TaskId::new();
        let run_result = AgentRunResult {
            task_id: TaskId::new(),
            role: AgentId::new("researcher"),
            outcome: AgentOutcome::Blocked { on: vec![blocker] },
            summary: "blocked".into(),
            files_modified: Vec::new(),
            tool_call_count: 0,
            cost_usd: 0.0,
            latency_ms: 0,
            provider: "mock".into(),
            model: "mock-model".into(),
            tokens_in: 0,
            tokens_out: 0,
        };
        let (result, events) = run_researcher_and_collect(vec![MockExpertAgent::sequence(
            AgentId::new("researcher"),
            vec![Ok(run_result)],
        )])
        .await;
        assert!(matches!(result.unwrap().outcome, AgentOutcome::Blocked { .. }));
        let blocked_on: Vec<_> = events
            .iter()
            .filter_map(|kind| match kind {
                EventKind::SubTaskBlocked { on, .. } => Some(on.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(blocked_on, vec![vec![blocker]]);
        assert!(
            !events.iter().any(|kind| matches!(kind, EventKind::SubTaskCompleted { .. })),
            "Blocked outcome must not publish SubTaskCompleted: {events:?}"
        );
    }

    #[tokio::test]
    async fn failed_outcome_publishes_failed_not_completed() {
        let (result, events) = run_researcher_and_collect(vec![MockExpertAgent::always_fail(
            AgentId::new("researcher"),
            "api returned 500",
        )])
        .await;
        assert!(matches!(result.unwrap().outcome, AgentOutcome::Failed { .. }));
        let errors: Vec<_> = events
            .iter()
            .filter_map(|kind| match kind {
                EventKind::SubTaskFailed { error, .. } => Some(error.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(errors, vec!["api returned 500".to_string()]);
        assert!(
            !events.iter().any(|kind| matches!(kind, EventKind::SubTaskCompleted { .. })),
            "Failed outcome must not publish SubTaskCompleted: {events:?}"
        );
    }

    #[tokio::test]
    async fn cancelled_run_publishes_cancelled_not_failed() {
        let (result, events) = run_researcher_and_collect(vec![MockExpertAgent::sequence(
            AgentId::new("researcher"),
            vec![Err(OrchestratorError::Provider(ProviderError::Cancelled))],
        )])
        .await;
        assert!(result.is_err());
        let cancelled: Vec<_> = events
            .iter()
            .filter_map(|kind| match kind {
                EventKind::SubTaskCancelled { reason, .. } => Some(reason.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(cancelled.len(), 1, "expected exactly one SubTaskCancelled event: {events:?}");
        assert!(
            !events.iter().any(|kind| matches!(kind, EventKind::SubTaskFailed { .. })),
            "cancelled run must not publish SubTaskFailed: {events:?}"
        );
    }

    #[tokio::test]
    async fn cancellation_aborts_execution() {
        let cancel = CancellationToken::new();
        cancel.cancel(); // pre-cancel

        let runner = make_runner(vec![MockExpertAgent::always_succeed(
            AgentId::new("researcher"),
            "should not run",
        )]);
        let err = run_researcher(&runner, cancel).await.unwrap_err();
        assert!(matches!(err, OrchestratorError::Cancelled));
    }

    #[tokio::test]
    async fn non_retryable_error_is_not_retried() {
        // The runner has no outer retry loop; any error is propagated
        // immediately on the first failure.
        let runner = make_runner(vec![MockExpertAgent::sequence(
            AgentId::new("researcher"),
            vec![Err(OrchestratorError::AgentLoopError("internal error".into()))],
        )]);
        let err = run_researcher(&runner, CancellationToken::new()).await.unwrap_err();
        assert!(
            matches!(err, OrchestratorError::AgentLoopError(ref msg) if msg == "internal error")
        );
    }

    // ------------------------------------------------------------------
    // Retry / cancellation
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn provider_error_is_propagated_without_outer_retry() {
        // AgentRunner no longer wraps the agent call in an outer retry loop.
        // Provider errors are propagated directly on the first failure;
        // per-request retry still occurs inside each specialist agent.
        let runner = make_runner(vec![MockExpertAgent::sequence(
            AgentId::new("researcher"),
            vec![Err(OrchestratorError::Provider(ProviderError::HttpStatus {
                status: 503,
                retry_after: None,
                message: "Service Unavailable".into(),
            }))],
        )]);
        let err = run_researcher(&runner, CancellationToken::new()).await.unwrap_err();
        assert!(
            matches!(
                &err,
                OrchestratorError::Provider(ProviderError::HttpStatus { status: 503, .. })
            ),
            "expected HttpStatus(503) propagated directly, got {err}"
        );
    }

    #[tokio::test]
    async fn provider_cancelled_is_propagated() {
        // ProviderError::Cancelled is now propagated as-is (no outer retry
        // loop to translate it to OrchestratorError::Cancelled).
        let runner = make_runner(vec![MockExpertAgent::sequence(
            AgentId::new("researcher"),
            vec![Err(OrchestratorError::Provider(ProviderError::Cancelled))],
        )]);
        let err = run_researcher(&runner, CancellationToken::new()).await.unwrap_err();
        assert!(
            matches!(&err, OrchestratorError::Provider(ProviderError::Cancelled)),
            "expected Provider(Cancelled), got {err}"
        );
    }

    // ------------------------------------------------------------------
    // Success path — model, latency, cost
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn success_sets_model_and_latency() {
        let task_id = TaskId::new();
        let ok_result = AgentRunResult {
            task_id,
            role: AgentId::new("researcher"),
            outcome: AgentOutcome::Success,
            summary: "done".into(),
            files_modified: Vec::new(),
            tool_call_count: 3,
            cost_usd: 0.015,
            latency_ms: 0, // runner overwrites this
            provider: "mock".into(),
            model: "will-be-overwritten".into(),
            tokens_in: 100,
            tokens_out: 50,
        };
        let runner = make_runner(vec![MockExpertAgent::sequence(
            AgentId::new("researcher"),
            vec![Ok(ok_result)],
        )]);
        let result = run_researcher(&runner, CancellationToken::new()).await.unwrap();

        assert!(matches!(result.outcome, AgentOutcome::Success));
        // Runner overwrites model with the profile model name.
        assert_eq!(result.model, "mock-model");
        // Cost should be preserved from the agent result (not overwritten).
        assert!((result.cost_usd - 0.015).abs() < f64::EPSILON);
    }
}
