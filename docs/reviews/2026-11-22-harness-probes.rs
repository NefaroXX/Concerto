//! Review-only regressions loaded under generic::tests by the companion runner.
//! These execute the real GenericSpecialistAgent; only the provider is scripted.

use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Clone, Copy)]
enum Scenario {
    NativeThenFailure,
    RepairThenFailure,
    RepairExhaustion,
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
        if call > 0 && !matches!(self.scenario, Scenario::RepairExhaustion) {
            return Err(concerto_core::error::ProviderError::AuthFailure);
        }
        self.completed.fetch_add(1, Ordering::SeqCst);
        let native = matches!(self.scenario, Scenario::NativeThenFailure);
        let chunk = CompletionChunk {
            delta: if native {
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
    eprintln!("completed requests={completed}; accumulated usage={usage:?}");
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
}
