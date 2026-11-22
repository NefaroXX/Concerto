//! `AgentRunner` — runs an expert agent on a subtask, emits lifecycle
//! events, and records cost and latency metrics.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use concerto_core::error::ProviderError;
use concerto_core::event::{EventBus, EventKind, ThinkingKind};
use concerto_core::ids::Ulid;
use concerto_core::traits::agent::ExpertAgent;
use concerto_core::traits::provider::LlmProvider;
use concerto_core::types::{AgentContext, AgentId, AgentOutcome, AgentRunResult, SubTask};
use concerto_core::{CancellationToken, OrchestratorError};
use concerto_providers::model::ModelProfile;
use concerto_sessions::spend::{SpendRecord, SpendTracker};
use concerto_sessions::SessionStore;

use crate::agents::execution_state::display_reason;
use crate::cost::AgentCostEstimator;
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
    /// Resolved blueprint facade backing stage-kind cost estimation for the
    /// pre-dispatch budget reservation (`None` prices the flat default).
    blueprint_facade: Option<concerto_config::BlueprintFacade>,
    /// Agent-scoped history store (H39b). `None` leaves history untouched;
    /// the multi-agent runtime injects the session store, while the
    /// single-agent loop keeps its own history path.
    session_store: Option<Arc<dyn SessionStore>>,
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
            blueprint_facade: None,
            session_store: None,
        }
    }

    /// Attach the session store backing agent-scoped history (H39b).
    pub fn with_session_store(mut self, store: Option<Arc<dyn SessionStore>>) -> Self {
        self.session_store = store;
        self
    }

    /// Attach the resolved blueprint facade backing stage-kind cost estimation.
    pub fn with_blueprint_facade(
        mut self,
        facade: Option<concerto_config::BlueprintFacade>,
    ) -> Self {
        self.blueprint_facade = facade;
        self
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
        mut context: AgentContext,
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
                kind: ThinkingKind::Headline,
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
        let reserved_cost =
            AgentCostEstimator::estimate(&role, &profile.profile, self.blueprint_facade.as_ref());
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
                kind: ThinkingKind::Headline,
            },
        );

        let start = std::time::Instant::now();

        // H39b: restore this role's prior turns as prompt context before the
        // run. A failed read must not abort the dispatch — the specialist
        // simply starts without restored history.
        if let Some(store) = &self.session_store {
            match store.load_agent_messages(task.session_id, &role_name, cancel.clone()).await {
                Ok(messages) => {
                    // A cancelled or interrupted run can leave an assistant
                    // tool call whose result never arrived. Repair it only for
                    // this prompt: bound the restored window, add request-only
                    // unknown-outcome results for unanswered calls, then bound
                    // again because the synthetic results add messages. The
                    // persisted stream stays raw.
                    let seed = concerto_core::types::bound_agent_history_seed(messages);
                    let seed = crate::provider_transcript::restore_tool_responses(seed);
                    context.agent_history =
                        concerto_core::types::AgentHistory::with_seed_bounded(seed);
                }
                Err(error) => {
                    tracing::warn!(
                        session_id = %task.session_id,
                        role = %role_name,
                        error = %error,
                        "agent history load failed; continuing with an empty seed"
                    );
                }
            }
        }

        // Run the agent once — provider retry happens inside each specialist
        // agent (via with_provider_retry). The whole-agent run is never replayed.
        let result = agent.run(task, context.clone(), model_name, cancel.clone()).await;

        let latency_ms = start.elapsed().as_millis() as u64;
        drop(provider_permit);
        drop(global_permit);
        // R4/H39: usage is run-owned and survives an agent error. The agent
        // records each completed request on the shared history handle, so a
        // later provider failure cannot refund tokens already consumed.
        let usage = context.agent_history.usage();
        let actual_cost =
            result.as_ref().map_or_else(|_| usage.cost_usd, |run_result| run_result.cost_usd);
        self.spend_tracker.settle_reservation(reserved_cost, actual_cost);
        // Publish the live spend snapshot (and cap signal) after every
        // provider run settles so UIs show the tracker's session total.
        publish_spend_events(&self.bus, task.session_id, correlation_id, &self.spend_tracker);

        // H39b: persist only the turns this run completed, even when the run
        // ended in cancellation. The write uses a fresh token so a cancelled
        // run still keeps its completed history; failures are logged and never
        // affect settlement or the run's result.
        if let Some(store) = &self.session_store {
            // Persist the raw produced turns. An interrupted trailing tool
            // call is durable evidence; the next load repairs it in memory
            // rather than losing the call or inventing a result here.
            let produced = context.agent_history.produced();
            if !produced.is_empty() {
                let append = store.append_agent_messages(
                    task.session_id,
                    &role_name,
                    &produced,
                    CancellationToken::new(),
                );
                match tokio::time::timeout(Duration::from_secs(10), append).await {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => tracing::warn!(
                        session_id = %task.session_id,
                        role = %role_name,
                        error = %error,
                        "agent history append failed; run result is unaffected"
                    ),
                    Err(_) => tracing::warn!(
                        session_id = %task.session_id,
                        role = %role_name,
                        "agent history append timed out; run result is unaffected"
                    ),
                }
            }
        }

        match result {
            Ok(mut run_result) => {
                run_result.latency_ms = latency_ms;
                run_result.model = profile.model_name().to_string();

                self.publish_outcome_events(correlation_id, task, &role, &run_result);

                Ok(run_result)
            }
            Err(e) => {
                // Failed in-process attempts consume real tokens but never
                // reach the coordinator's `provider_metrics` collection (that
                // only sees `Ok` results). Persist this attempt's attributed
                // usage here, exactly once, from the run-owned accumulator.
                // Success path leaves persistence to the run's settlement.
                if usage.measured_requests > 0 {
                    if let Some(store) = &self.session_store {
                        let record = SpendRecord {
                            id: Ulid::new(),
                            session_id: task.session_id,
                            task_id: Some(task.id.0),
                            agent_id: Some(role_name.clone()),
                            provider: profile.profile.provider.clone(),
                            model: profile.model_name().to_string(),
                            tokens_in: usage.tokens_in,
                            tokens_out: usage.tokens_out,
                            cost_usd: usage.cost_usd,
                            created_at: time::OffsetDateTime::now_utc(),
                        };
                        let write = store.record_spend(record, CancellationToken::new());
                        match tokio::time::timeout(Duration::from_secs(10), write).await {
                            Ok(Ok(())) => {}
                            Ok(Err(error)) => tracing::warn!(
                                session_id = %task.session_id,
                                role = %role_name,
                                error = %error,
                                "failed-attempt spend persistence failed; live settlement is unaffected"
                            ),
                            Err(_) => tracing::warn!(
                                session_id = %task.session_id,
                                role = %role_name,
                                "failed-attempt spend persistence timed out; live settlement is unaffected"
                            ),
                        }
                    }
                }
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
                        // Terminal failure is a bucket digest (Headline);
                        // cancellation stays Detail with the other internals.
                        kind: if cancelled { ThinkingKind::Detail } else { ThinkingKind::Headline },
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
                        kind: ThinkingKind::Headline,
                    },
                );
                let _ = self.bus.publish_for_session(
                    session_id,
                    correlation_id,
                    EventKind::SubTaskCompleted { task_id, role, outcome: summary },
                );
            }
            AgentOutcome::NeedsRevision { reason } => {
                let reason = display_reason(&reason);
                let _ = self.bus.publish_for_session(
                    session_id,
                    correlation_id,
                    EventKind::AgentThought {
                        agent_id: role_name.to_string(),
                        content: format!("Subtask needs revision: {reason}"),
                        kind: ThinkingKind::Detail,
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
                        kind: ThinkingKind::Detail,
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
                        // Terminal failure surfaces as the bucket digest.
                        kind: ThinkingKind::Headline,
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
            verification: Vec::new(),
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
            verification: Vec::new(),
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
            verification: Vec::new(),
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

    // ------------------------------------------------------------------
    // Score-accordion tiers: terminal completion/failure thoughts are
    // Headline digests; cancellation and decomposition internals stay Detail.
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn failed_outcome_thought_is_headline() {
        let (_, events) = run_researcher_and_collect(vec![MockExpertAgent::always_fail(
            AgentId::new("researcher"),
            "api returned 500",
        )])
        .await;
        let kinds: Vec<_> = events
            .iter()
            .filter_map(|kind| match kind {
                EventKind::AgentThought { content, kind, .. }
                    if content.contains("Subtask failed") =>
                {
                    Some(*kind)
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            kinds,
            vec![concerto_core::event::ThinkingKind::Headline],
            "terminal failure thought must be Headline: {events:?}"
        );
    }

    #[tokio::test]
    async fn error_path_failure_thought_is_headline_but_cancel_stays_detail() {
        // Non-cancellation error → Headline.
        let (_, events) = run_researcher_and_collect(vec![MockExpertAgent::sequence(
            AgentId::new("researcher"),
            vec![Err(OrchestratorError::AgentLoopError("boom".into()))],
        )])
        .await;
        let kinds: Vec<_> = events
            .iter()
            .filter_map(|kind| match kind {
                EventKind::AgentThought { content, kind, .. }
                    if content.contains("Subtask failed") =>
                {
                    Some(*kind)
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            kinds,
            vec![concerto_core::event::ThinkingKind::Headline],
            "error-path failure thought must be Headline: {events:?}"
        );

        // Cancellation → Detail.
        let (_, cancelled_events) = run_researcher_and_collect(vec![MockExpertAgent::sequence(
            AgentId::new("researcher"),
            vec![Err(OrchestratorError::Provider(ProviderError::Cancelled))],
        )])
        .await;
        let cancelled_kinds: Vec<_> = cancelled_events
            .iter()
            .filter_map(|kind| match kind {
                EventKind::AgentThought { content, kind, .. }
                    if content.contains("Subtask cancelled") =>
                {
                    Some(*kind)
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            cancelled_kinds,
            vec![concerto_core::event::ThinkingKind::Detail],
            "cancellation thought must stay Detail: {cancelled_events:?}"
        );
    }

    // ------------------------------------------------------------------
    // H39b: agent-scoped history persist/restore
    // ------------------------------------------------------------------

    use concerto_core::traits::agent::ExpertAgent;
    use concerto_core::types::{Message, Role};
    use concerto_sessions::SqliteSessionStore;

    #[derive(Clone, Copy)]
    enum HistoryMockOutcome {
        Success,
        Cancelled,
    }

    /// Minimal expert-agent mock for the history tests: observes the restored
    /// seed, optionally fires cancellation, records its scripted turns through
    /// `context.agent_history`, then returns the scripted outcome.
    struct HistoryMockAgent {
        id: AgentId,
        turns: Vec<Message>,
        outcome: HistoryMockOutcome,
        cancel_on_run: Option<CancellationToken>,
        seed_seen: Arc<std::sync::Mutex<Option<Vec<Message>>>>,
    }

    impl HistoryMockAgent {
        fn new(
            id: AgentId,
            turns: Vec<Message>,
            outcome: HistoryMockOutcome,
            cancel_on_run: Option<CancellationToken>,
        ) -> (Self, Arc<std::sync::Mutex<Option<Vec<Message>>>>) {
            let seed_seen = Arc::new(std::sync::Mutex::new(None));
            (Self { id, turns, outcome, cancel_on_run, seed_seen: seed_seen.clone() }, seed_seen)
        }
    }

    #[async_trait::async_trait]
    impl ExpertAgent for HistoryMockAgent {
        fn id(&self) -> AgentId {
            self.id.clone()
        }

        fn stage(&self) -> Option<concerto_core::types::AgentStage> {
            None
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
            *self.seed_seen.lock().unwrap_or_else(|error| error.into_inner()) =
                Some(context.agent_history.seeded().to_vec());
            if let Some(cancel) = &self.cancel_on_run {
                cancel.cancel();
            }
            let mut recorded = 0;
            context.agent_history.record_from(&self.turns, &mut recorded);
            match self.outcome {
                HistoryMockOutcome::Success => Ok(AgentRunResult {
                    verification: Vec::new(),
                    task_id: TaskId::new(),
                    role: self.id.clone(),
                    outcome: AgentOutcome::Success,
                    summary: "done".into(),
                    files_modified: Vec::new(),
                    tool_call_count: 0,
                    cost_usd: 0.0,
                    latency_ms: 0,
                    provider: "mock".into(),
                    model: "mock-model".into(),
                    tokens_in: 0,
                    tokens_out: 0,
                }),
                HistoryMockOutcome::Cancelled => Err(OrchestratorError::Cancelled),
            }
        }
    }

    fn history_message(role: Role, content: &str) -> Message {
        Message {
            gemini_replay: None,
            role,
            content: content.to_string(),
            tool_calls: None,
            tool_results: None,
            reasoning_content: None,
            tokens_in: None,
            tokens_out: None,
        }
    }

    fn runner_with_history_mock(
        mock: HistoryMockAgent,
        store: Option<Arc<dyn SessionStore>>,
    ) -> AgentRunner {
        let mut registry = AgentRegistry::new();
        registry.register(Arc::new(mock));
        AgentRunner::new(Arc::new(registry), EventBus::new(256), Arc::new(SpendTracker::default()))
            .with_session_store(store)
    }

    async fn in_memory_store_with_session() -> (Arc<SqliteSessionStore>, Ulid) {
        let store = Arc::new(
            SqliteSessionStore::connect_in_memory().await.expect("in-memory store should open"),
        );
        let session = store
            .create_session(
                camino::Utf8Path::new("/tmp/h39b"),
                "mock",
                "mock-model",
                CancellationToken::new(),
            )
            .await
            .expect("session should be created");
        (store, session.id)
    }

    #[tokio::test]
    async fn agent_history_persists_to_agent_stream_not_root() {
        let (store, session_id) = in_memory_store_with_session().await;
        let turns = vec![
            history_message(Role::User, "investigate"),
            history_message(Role::Assistant, "findings"),
        ];
        let (mock, seed_seen) = HistoryMockAgent::new(
            AgentId::new("researcher"),
            turns,
            HistoryMockOutcome::Success,
            None,
        );
        let runner = runner_with_history_mock(mock, Some(store.clone()));
        let session = SessionContext::new(session_id, std::path::PathBuf::from("/tmp"));
        let task = make_task(TaskId::new(), session_id);

        let result = runner
            .run(
                AgentId::new("researcher"),
                &task,
                AgentContext::new(session),
                &make_profile(),
                CancellationToken::new(),
            )
            .await
            .expect("run should succeed");
        assert!(matches!(result.outcome, AgentOutcome::Success));
        assert!(
            seed_seen
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .as_ref()
                .is_some_and(|seed| seed.is_empty()),
            "a fresh agent stream starts with an empty seed"
        );

        let root = store.load_messages(session_id, CancellationToken::new()).await.unwrap();
        assert!(root.is_empty(), "agent history must never touch root messages: {root:?}");
        let stream = store
            .load_agent_messages(session_id, "researcher", CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(stream.len(), 2);
        assert_eq!(stream[0].role, Role::User);
        assert_eq!(stream[0].content, "investigate");
        assert_eq!(stream[1].role, Role::Assistant);
        assert_eq!(stream[1].content, "findings");
    }

    #[tokio::test]
    async fn agent_history_restores_seed_and_appends_without_duplicates() {
        let (store, session_id) = in_memory_store_with_session().await;
        let seed = vec![
            history_message(Role::User, "first question"),
            history_message(Role::Assistant, "first answer"),
        ];
        store
            .append_agent_messages(session_id, "researcher", &seed, CancellationToken::new())
            .await
            .unwrap();
        let delta = vec![history_message(Role::Assistant, "follow-up")];
        let (mock, seed_seen) = HistoryMockAgent::new(
            AgentId::new("researcher"),
            delta,
            HistoryMockOutcome::Success,
            None,
        );
        let runner = runner_with_history_mock(mock, Some(store.clone()));
        let session = SessionContext::new(session_id, std::path::PathBuf::from("/tmp"));
        let task = make_task(TaskId::new(), session_id);

        runner
            .run(
                AgentId::new("researcher"),
                &task,
                AgentContext::new(session),
                &make_profile(),
                CancellationToken::new(),
            )
            .await
            .expect("run should succeed");

        let observed = seed_seen
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
            .expect("mock must observe the seed");
        assert_eq!(observed.len(), 2, "restored seed is visible to the agent");
        assert_eq!(observed[0].content, "first question");
        assert_eq!(observed[1].content, "first answer");

        let stream = store
            .load_agent_messages(session_id, "researcher", CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(stream.len(), 3, "seed plus exactly one delta, no duplicate seed");
        assert_eq!(stream[0].content, "first question");
        assert_eq!(stream[1].content, "first answer");
        assert_eq!(stream[2].content, "follow-up");
        let root = store.load_messages(session_id, CancellationToken::new()).await.unwrap();
        assert!(root.is_empty());
    }

    #[tokio::test]
    async fn cancelled_run_persists_turns_completed_before_cancellation() {
        let (store, session_id) = in_memory_store_with_session().await;
        let cancel = CancellationToken::new();
        let (mock, _) = HistoryMockAgent::new(
            AgentId::new("researcher"),
            vec![history_message(Role::Assistant, "partial result")],
            HistoryMockOutcome::Cancelled,
            Some(cancel.clone()),
        );
        let runner = runner_with_history_mock(mock, Some(store.clone()));
        let session = SessionContext::new(session_id, std::path::PathBuf::from("/tmp"));
        let task = make_task(TaskId::new(), session_id);

        let error = runner
            .run(
                AgentId::new("researcher"),
                &task,
                AgentContext::new(session),
                &make_profile(),
                cancel.clone(),
            )
            .await
            .expect_err("cancelled run must return an error");
        assert!(matches!(error, OrchestratorError::Cancelled));
        assert!(cancel.is_cancelled());

        let stream = store
            .load_agent_messages(session_id, "researcher", CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(stream.len(), 1, "the pre-cancellation turn must survive");
        assert_eq!(stream[0].content, "partial result");
        let root = store.load_messages(session_id, CancellationToken::new()).await.unwrap();
        assert!(root.is_empty());
    }

    #[tokio::test]
    async fn agent_history_persists_interrupted_tail_and_repairs_on_restore() {
        let (store, session_id) = in_memory_store_with_session().await;
        let mut submission = history_message(Role::Assistant, "");
        submission.tool_calls = Some(vec![concerto_core::types::ToolCall {
            id: "call_submit".into(),
            name: "submit_design_doc".into(),
            arguments: serde_json::json!({ "goals": ["ship"] }),
            thought_signature: None,
        }]);
        let turns = vec![history_message(Role::User, "design it"), submission];
        let (mock, _) = HistoryMockAgent::new(
            AgentId::new("architect"),
            turns,
            HistoryMockOutcome::Success,
            None,
        );
        let runner = runner_with_history_mock(mock, Some(store.clone()));
        let session = SessionContext::new(session_id, std::path::PathBuf::from("/tmp"));
        let task = make_task(TaskId::new(), session_id);

        runner
            .run(
                AgentId::new("architect"),
                &task,
                AgentContext::new(session),
                &make_profile(),
                CancellationToken::new(),
            )
            .await
            .expect("run should succeed");

        let stream = store
            .load_agent_messages(session_id, "architect", CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(stream.len(), 2, "the interrupted call is durable evidence: {stream:?}");
        assert_eq!(stream[0].content, "design it");
        assert_eq!(
            stream[1].tool_calls.as_ref().map(|calls| calls[0].id.as_str()),
            Some("call_submit")
        );

        // A second run observes the repaired seed: the recorded result is kept
        // and the unanswered call gets a request-only unknown-outcome result.
        let (second, seed_seen) = HistoryMockAgent::new(
            AgentId::new("architect"),
            Vec::new(),
            HistoryMockOutcome::Success,
            None,
        );
        let second_runner = runner_with_history_mock(second, Some(store.clone()));
        second_runner
            .run(
                AgentId::new("architect"),
                &task,
                AgentContext::new(SessionContext::new(
                    session_id,
                    std::path::PathBuf::from("/tmp"),
                )),
                &make_profile(),
                CancellationToken::new(),
            )
            .await
            .expect("second run should succeed");

        let observed = seed_seen
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
            .expect("mock must observe the repaired seed");
        assert_eq!(observed.len(), 3, "repair adds exactly one synthetic result: {observed:?}");
        assert_eq!(observed[1].tool_calls.as_ref().map(Vec::len), Some(1));
        let synthetic = observed[2]
            .tool_results
            .as_ref()
            .and_then(|results| results.first())
            .expect("synthetic result");
        assert_eq!(synthetic.id, "call_submit");
        assert_eq!(synthetic.content["status"], "outcome_unknown");

        let persisted = store
            .load_agent_messages(session_id, "architect", CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(persisted.len(), 2, "the synthetic result is never persisted");
    }

    /// Mock whose completed requests report measured usage, then a later
    /// provider request fails. It records usage on the run-owned history
    /// exactly as the real specialist loops do.
    struct UsageThenFailAgent {
        id: AgentId,
    }

    #[async_trait::async_trait]
    impl ExpertAgent for UsageThenFailAgent {
        fn id(&self) -> AgentId {
            self.id.clone()
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
            // Two completed requests before the third fails. A last-request
            // implementation would lose the first request's tokens/cost.
            context.agent_history.record_usage(600, 100, 0.006);
            context.agent_history.record_usage(400, 100, 0.004);
            Err(OrchestratorError::Provider(ProviderError::Network("boom".into())))
        }
    }

    #[tokio::test]
    async fn failed_run_settles_measured_usage_and_persists_it_once() {
        let (store, session_id) = in_memory_store_with_session().await;
        let tracker = Arc::new(SpendTracker::new(Some(0.005), None, None));
        let mut registry = AgentRegistry::new();
        registry.register(Arc::new(UsageThenFailAgent { id: AgentId::new("researcher") }));
        let runner = AgentRunner::new(Arc::new(registry), EventBus::new(64), tracker.clone())
            .with_session_store(Some(store.clone()));
        let session = SessionContext::new(session_id, std::path::PathBuf::from("/tmp"));
        let task = make_task(TaskId::new(), session_id);

        let error = runner
            .run(
                AgentId::new("researcher"),
                &task,
                AgentContext::new(session),
                &make_profile(),
                CancellationToken::new(),
            )
            .await
            .expect_err("the provider failure must remain the run's error");
        assert!(error.to_string().contains("boom"), "original error retained: {error}");
        assert!(
            (tracker.session_total() - 0.01).abs() < f64::EPSILON,
            "live spend must retain every completed request, not just the last"
        );

        let spend = store.list_spend_records(session_id, CancellationToken::new()).await.unwrap();
        assert_eq!(spend.len(), 1, "failed-attempt usage persisted exactly once: {spend:?}");
        assert_eq!(spend[0].agent_id.as_deref(), Some("researcher"));
        assert_eq!(spend[0].tokens_in, 1_000);
        assert_eq!(spend[0].tokens_out, 200);
        assert!((spend[0].cost_usd - 0.01).abs() < f64::EPSILON);

        let second_error = runner
            .run(
                AgentId::new("researcher"),
                &make_task(TaskId::new(), session_id),
                AgentContext::new(SessionContext::new(
                    session_id,
                    std::path::PathBuf::from("/tmp"),
                )),
                &make_profile(),
                CancellationToken::new(),
            )
            .await
            .expect_err("exhausted cap must refuse a subsequent dispatch");
        assert!(matches!(second_error, OrchestratorError::NoBudgetForDelegation));
    }

    #[tokio::test]
    async fn agent_history_append_error_does_not_fail_the_run() {
        // The store has no session row, so the agent-scoped append fails its
        // foreign key. History persistence is fail-soft: the run still
        // settles and returns its result.
        let store = Arc::new(
            SqliteSessionStore::connect_in_memory().await.expect("in-memory store should open"),
        );
        let unknown_session = Ulid::new();
        let (mock, _) = HistoryMockAgent::new(
            AgentId::new("researcher"),
            vec![history_message(Role::Assistant, "would be persisted")],
            HistoryMockOutcome::Success,
            None,
        );
        let runner = runner_with_history_mock(mock, Some(store.clone()));
        let session = SessionContext::new(unknown_session, std::path::PathBuf::from("/tmp"));
        let task = make_task(TaskId::new(), unknown_session);

        let result = runner
            .run(
                AgentId::new("researcher"),
                &task,
                AgentContext::new(session),
                &make_profile(),
                CancellationToken::new(),
            )
            .await
            .expect("a history write failure must not fail the run");
        assert!(matches!(result.outcome, AgentOutcome::Success));

        let stream = store
            .load_agent_messages(unknown_session, "researcher", CancellationToken::new())
            .await
            .unwrap();
        assert!(stream.is_empty(), "failed append leaves no rows");
    }
}
