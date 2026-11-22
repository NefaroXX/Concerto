//! Failed and repaired provider requests retain measured usage at completion.

use super::*;
use concerto_core::types::CompletionUsage;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Clone, Copy)]
enum Scenario {
    NativeThenFailure,
    RepairThenFailure,
    RepairExhaustion,
    RepairSuccess,
}

struct PricedProvider {
    scenario: Scenario,
    calls: AtomicUsize,
    completed: AtomicUsize,
}

#[async_trait::async_trait]
impl LlmProvider for PricedProvider {
    async fn stream_completion(
        &self,
        _request: CompletionRequest,
        _cancel: CancellationToken,
    ) -> Result<CompletionStream, concerto_core::error::ProviderError> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        if call > 0
            && matches!(self.scenario, Scenario::NativeThenFailure | Scenario::RepairThenFailure)
        {
            return Err(concerto_core::error::ProviderError::AuthFailure);
        }
        self.completed.fetch_add(1, Ordering::SeqCst);
        let native = matches!(self.scenario, Scenario::NativeThenFailure);
        let chunk = CompletionChunk {
            delta: if call > 0 && matches!(self.scenario, Scenario::RepairSuccess) {
                "Finished after correcting the response.".into()
            } else if native {
                "working".into()
            } else {
                "<tool_calls>\nnot json\n</tool_calls>".into()
            },
            tool_call: native.then(|| ToolCall {
                id: "review-call".into(),
                name: "write_file".into(),
                arguments: serde_json::json!({"path": "review-fixture.rs"}),
                ..Default::default()
            }),
            reasoning: None,
            gemini_replay: None,
            is_final: true,
            usage: Some(CompletionUsage {
                prompt_tokens: Some(1_000),
                completion_tokens: Some(200),
                ..Default::default()
            }),
        };
        Ok(Box::pin(futures::stream::iter(vec![Ok(chunk)])))
    }

    fn context_capacity(&self, _model: &str) -> concerto_core::types::TokenBudget {
        concerto_core::types::TokenBudget::new(128_000, 4_096)
    }

    fn approximate_cost(&self, tokens_in: u64, tokens_out: u64) -> f64 {
        (tokens_in + tokens_out) as f64 / 100_000.0
    }

    fn provider_name(&self) -> &'static str {
        "review-priced"
    }
}

async fn run_scenario(scenario: Scenario) -> (usize, concerto_core::types::AgentHistory) {
    let provider = Arc::new(PricedProvider {
        scenario,
        calls: AtomicUsize::new(0),
        completed: AtomicUsize::new(0),
    });
    let agent = fallback_agent(provider.clone(), CapturingAudit::new())
        .with_advertised_tool_support(Some(matches!(scenario, Scenario::NativeThenFailure)));
    let directory = tempfile::tempdir().expect("disk-backed fixture directory");
    let mut context = ctx();
    context.session.project_dir = directory.path().to_path_buf();
    let history = context.agent_history.clone();
    let error = agent
        .run(&fallback_task(), context, "review-priced-model", CancellationToken::new())
        .await
        .expect_err("scripted failure must remain an error");
    match scenario {
        Scenario::RepairExhaustion => assert!(error.to_string().contains("repair exhausted")),
        _ => assert!(matches!(
            error,
            OrchestratorError::Provider(concerto_core::error::ProviderError::AuthFailure)
        )),
    }
    let completed = provider.completed.load(Ordering::SeqCst);
    assert!(completed > 0, "the provider must complete at least one priced request");
    (completed, history)
}

fn assert_paid_usage(completed: usize, history: &concerto_core::types::AgentHistory) {
    let usage = history.usage();
    assert_eq!(usage.measured_requests as usize, completed);
    assert_eq!(usage.tokens_in, completed as u64 * 1_000, "paid input tokens lost");
    assert_eq!(usage.tokens_out, completed as u64 * 200, "paid output tokens lost");
    assert!((usage.cost_usd - completed as f64 * 0.012).abs() < 1e-9, "paid cost lost");
}

#[tokio::test]
async fn native_later_failure_keeps_completed_request_usage() {
    let (completed, history) = run_scenario(Scenario::NativeThenFailure).await;
    assert_eq!(completed, 1);
    assert_paid_usage(completed, &history);
}

#[tokio::test]
async fn repair_provider_failure_keeps_completed_request_usage() {
    let (completed, history) = run_scenario(Scenario::RepairThenFailure).await;
    assert_eq!(completed, 1);
    assert_paid_usage(completed, &history);
}

#[tokio::test]
async fn repair_exhaustion_keeps_all_completed_request_usage() {
    let (completed, history) = run_scenario(Scenario::RepairExhaustion).await;
    assert_eq!(completed, crate::tool_driver::MAX_REPAIR_ATTEMPTS as usize + 1);
    assert_paid_usage(completed, &history);
    let replies: Vec<_> =
        history.produced().into_iter().filter(|m| m.role == Role::Assistant).collect();
    assert_eq!(replies.len(), completed, "the final malformed reply must also survive");
    assert!(replies.iter().all(|m| m.tokens_in == Some(1_000) && m.tokens_out == Some(200)));
}

#[tokio::test]
async fn successful_repair_prices_each_request_once_and_preserves_per_turn_usage() {
    let provider = Arc::new(PricedProvider {
        scenario: Scenario::RepairSuccess,
        calls: AtomicUsize::new(0),
        completed: AtomicUsize::new(0),
    });
    let agent = fallback_agent(provider.clone(), CapturingAudit::new());
    let context = ctx();
    let history = context.agent_history.clone();
    let result =
        agent.run(&fallback_task(), context, "mock", CancellationToken::new()).await.unwrap();
    assert_eq!(provider.completed.load(Ordering::SeqCst), 2);
    assert_paid_usage(2, &history);
    assert_eq!(result.tokens_in, 2_000);
    assert_eq!(result.tokens_out, 400);
    assert!((result.cost_usd - 0.024).abs() < 1e-9);
    let replies: Vec<_> =
        history.produced().into_iter().filter(|m| m.role == Role::Assistant).collect();
    assert_eq!(replies.len(), 2);
    assert!(
        replies.iter().all(|m| m.tokens_in == Some(1_000) && m.tokens_out == Some(200)),
        "repair usage must not be merged onto the final assistant turn"
    );
}

fn priced_profile() -> concerto_providers::model::ModelProfile {
    concerto_providers::model::ModelProfile {
        profile: concerto_core::types::RoutingProfile {
            provider_config_id: "priced".into(),
            provider: "review-priced".into(),
            model: "priced-model".into(),
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

async fn assert_failed_attempt_is_durable_and_budgeted(scenario: Scenario, completed: u64) {
    use concerto_sessions::{SessionStore, SqliteSessionStore};
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sessions.db");
    let store = Arc::new(SqliteSessionStore::connect_path(&path).await.unwrap());
    let session = store
        .create_session(
            camino::Utf8Path::new("."),
            "review-priced",
            "priced-model",
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let mut task = fallback_task();
    task.session_id = session.id;
    let context = AgentContext::new(concerto_core::types::SessionContext::new(
        session.id,
        directory.path().to_path_buf(),
    ));
    let provider = Arc::new(PricedProvider {
        scenario,
        calls: AtomicUsize::new(0),
        completed: AtomicUsize::new(0),
    });
    let agent = fallback_agent(provider.clone(), CapturingAudit::new())
        .with_advertised_tool_support(Some(matches!(scenario, Scenario::NativeThenFailure)));
    let mut registry = crate::registry::AgentRegistry::new();
    registry.register(Arc::new(agent));
    let tracker = Arc::new(concerto_sessions::spend::SpendTracker::new(Some(0.005), None, None));
    let runner = crate::agent_runner::AgentRunner::new(
        Arc::new(registry),
        EventBus::new(128),
        tracker.clone(),
    )
    .with_session_store(Some(store.clone()));
    let first = runner
        .run(
            AgentId::new("coder"),
            &task,
            context.clone(),
            &priced_profile(),
            CancellationToken::new(),
        )
        .await;
    assert!(first.is_err(), "the scripted failure must remain terminal");
    assert_eq!(provider.completed.load(Ordering::SeqCst) as u64, completed);
    assert!((tracker.session_total() - completed as f64 * 0.012).abs() < 1e-9);
    let second = runner
        .run(AgentId::new("coder"), &task, context, &priced_profile(), CancellationToken::new())
        .await;
    assert!(matches!(second, Err(OrchestratorError::NoBudgetForDelegation)));
    assert_eq!(
        provider.completed.load(Ordering::SeqCst) as u64,
        completed,
        "no request after the cap"
    );
    drop(runner);
    drop(store);
    let reopened = SqliteSessionStore::connect_path(&path).await.unwrap();
    let records = reopened.list_spend_records(session.id, CancellationToken::new()).await.unwrap();
    assert_eq!(records.len(), 1, "exactly one attributed failed-attempt record");
    let record = &records[0];
    assert_eq!(record.agent_id.as_deref(), Some("coder"));
    assert_eq!(record.task_id, Some(task.id.0));
    assert_eq!(record.provider, "review-priced");
    assert_eq!(record.model, "priced-model");
    assert_eq!(record.tokens_in, completed * 1_000);
    assert_eq!(record.tokens_out, completed * 200);
    assert!((record.cost_usd - completed as f64 * 0.012).abs() < 1e-9);
}

#[tokio::test]
async fn failed_native_and_repair_requests_survive_database_reopen_and_exhaust_budget() {
    for (scenario, completed) in [
        (Scenario::NativeThenFailure, 1),
        (Scenario::RepairThenFailure, 1),
        (Scenario::RepairExhaustion, u64::from(crate::tool_driver::MAX_REPAIR_ATTEMPTS) + 1),
    ] {
        assert_failed_attempt_is_durable_and_budgeted(scenario, completed).await;
    }
}
