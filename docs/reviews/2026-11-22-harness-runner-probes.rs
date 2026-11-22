//! Review-only runner settlement regression; reuses the candidate's mock agent.

use super::*;

#[tokio::test]
async fn cloned_fallback_context_must_not_charge_prior_failed_attempt_again() {
    let tracker = Arc::new(SpendTracker::new(None, None, None));
    let mut registry = AgentRegistry::new();
    registry.register(Arc::new(UsageThenFailAgent { id: AgentId::new("researcher") }));
    let runner = AgentRunner::new(Arc::new(registry), EventBus::new(64), tracker.clone());
    let directory = tempfile::tempdir().expect("disk-backed fixture directory");
    let session_id = Ulid::new();
    let context =
        AgentContext::new(SessionContext::new(session_id, directory.path().to_path_buf()));
    let task = make_task(TaskId::new(), session_id);
    for _ in 0..2 {
        runner
            .run(
                AgentId::new("researcher"),
                &task,
                context.clone(),
                &make_profile(),
                CancellationToken::new(),
            )
            .await
            .expect_err("scripted later provider failure");
    }
    eprintln!("two attempts at $0.01 each; settled total={}", tracker.session_total());
    assert!(
        (tracker.session_total() - 0.02).abs() < 1e-9,
        "each failed attempt must settle only its own completed requests"
    );
}
