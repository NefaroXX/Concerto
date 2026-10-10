//! Behavioral fixtures for specialist settlement and retained progress.

use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

struct FixtureWriteTool {
    failures: AtomicUsize,
}

#[async_trait::async_trait]
impl concerto_core::traits::tool::Tool for FixtureWriteTool {
    fn name(&self) -> &str {
        "write_file"
    }

    fn description(&self) -> &str {
        "Write a fixture file, with a controlled initial failure"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "content": {"type": "string"}
            },
            "required": ["path", "content"]
        })
    }

    fn capability_requirements(&self) -> CapabilitySet {
        CapabilitySet::default()
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        _policy: &dyn concerto_core::traits::policy::PolicyEngine,
        session: &concerto_core::types::SessionContext,
        _cancel: CancellationToken,
    ) -> Result<ToolOutput, concerto_core::ToolError> {
        if self
            .failures
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |count| count.checked_sub(1))
            .is_ok()
        {
            return Err(concerto_core::ToolError::ExecutionFailed { message: "disk full".into() });
        }
        let path = input["path"].as_str().expect("fixture path validated by the guard");
        let content = input["content"].as_str().expect("fixture content validated by the guard");
        std::fs::write(session.project_dir.join(path), content)?;
        Ok(ToolOutput { summary: format!("Wrote {path}"), data: serde_json::json!({"path": path}) })
    }
}

fn fixture_agent(failures: usize, chunks: Vec<CompletionChunk>) -> GenericSpecialistAgent {
    let mut registry = concerto_core::types::ToolRegistry::default();
    registry.register(Box::new(FixtureWriteTool { failures: AtomicUsize::new(failures) }));
    let executor = ToolExecutor::new(
        Arc::new(registry),
        Arc::new(concerto_core::policy::SimplePolicyEngine::new(
            vec![PolicyRule::AutoApprove(Condition::Always)],
            Arc::new(NullAudit),
        )),
    );
    GenericSpecialistAgent::new(
        AgentId::new("coder"),
        "Coder".into(),
        Some(AgentStage::new("implement")),
        Arc::new(SequencedProvider::new(chunks)),
        Some(Arc::new(executor)),
        EventBus::new(128),
        RetryPolicy::default(),
        PromptSections::default(),
        AgentCapabilities::default(),
    )
}

fn write_turn(id: &str, path: &str, content: &str) -> CompletionChunk {
    CompletionChunk {
        delta: "Continuing the change".into(),
        reasoning: None,
        tool_call: Some(ToolCall {
            id: id.into(),
            name: "write_file".into(),
            arguments: serde_json::json!({"path": path, "content": content}),
            ..Default::default()
        }),
        is_final: true,
        usage: None,
    }
}

fn final_turn() -> CompletionChunk {
    CompletionChunk {
        delta: "The requested change is complete.".into(),
        reasoning: None,
        tool_call: None,
        is_final: true,
        usage: None,
    }
}

fn fixture_context(root: &std::path::Path) -> (SubTask, AgentContext) {
    let session = concerto_core::types::SessionContext::new(
        concerto_core::ids::Ulid::new(),
        root.to_path_buf(),
    );
    let task = SubTask::new(session.session_id, AgentId::new("coder"), "Repair the fixture");
    (task, AgentContext::new(session))
}

#[tokio::test]
async fn iteration_limit_returns_revision_with_files_and_serializable_progress() {
    let dir = tempfile::tempdir().unwrap();
    let chunks = (0..MAX_TOOL_ITERATIONS)
        .map(|turn| write_turn(&format!("write-{turn}"), "a.rs", "retained"))
        .collect();
    let agent = fixture_agent(0, chunks);
    let (task, context) = fixture_context(dir.path());
    let result = agent.run(&task, context, "mock", CancellationToken::new()).await.unwrap();
    assert_eq!(result.tool_call_count, MAX_TOOL_ITERATIONS);
    assert_eq!(std::fs::read_to_string(dir.path().join("a.rs")).unwrap(), "retained");
    assert_eq!(result.files_modified, vec![camino::Utf8PathBuf::from("a.rs")]);
    let encoded = serde_json::to_string(&result).unwrap();
    let restored: AgentRunResult = serde_json::from_str(&encoded).unwrap();
    let AgentOutcome::NeedsRevision { reason } = restored.outcome else {
        panic!("a still-active tool loop must not complete at its iteration bound");
    };
    let note = crate::agents::execution_state::continuation(&reason).unwrap();
    assert_eq!(note.task_id, task.id.to_string());
    assert_eq!(note.successful_calls.len(), MAX_TOOL_ITERATIONS as usize);
    assert_eq!(note.stop_code, "specialist-turn-limit");
}

#[tokio::test]
async fn corrected_operation_can_finish_after_observed_success() {
    let dir = tempfile::tempdir().unwrap();
    let agent = fixture_agent(
        1,
        vec![
            write_turn("failed", "a.rs", "initial"),
            write_turn("corrected", "a.rs", "repaired"),
            final_turn(),
        ],
    );
    let (task, context) = fixture_context(dir.path());
    let result = agent.run(&task, context, "mock", CancellationToken::new()).await.unwrap();
    assert_eq!(result.outcome, AgentOutcome::Success);
    assert_eq!(result.tool_call_count, 2);
    assert_eq!(std::fs::read_to_string(dir.path().join("a.rs")).unwrap(), "repaired");
}

#[tokio::test]
async fn successful_sibling_edit_does_not_hide_unresolved_failure() {
    let dir = tempfile::tempdir().unwrap();
    let agent = fixture_agent(
        1,
        vec![
            write_turn("failed", "a.rs", "missing"),
            write_turn("sibling", "b.rs", "retained"),
            final_turn(),
        ],
    );
    let (task, context) = fixture_context(dir.path());
    let result = agent.run(&task, context, "mock", CancellationToken::new()).await.unwrap();
    assert!(!dir.path().join("a.rs").exists());
    assert_eq!(std::fs::read_to_string(dir.path().join("b.rs")).unwrap(), "retained");
    let AgentOutcome::NeedsRevision { reason } = &result.outcome else {
        panic!("unrelated successful work cannot erase the failed a.rs operation");
    };
    let note = crate::agents::execution_state::continuation(reason).unwrap();
    assert_eq!(note.outstanding_calls.len(), 1);
    assert_eq!(note.outstanding_calls[0].call_id, "failed");
    assert_eq!(note.successful_calls[0].call_id, "sibling");
}

#[tokio::test]
async fn continuing_a_task_cannot_forget_its_unresolved_operations() {
    let dir = tempfile::tempdir().unwrap();
    let (task, mut context) = fixture_context(dir.path());
    let first = fixture_agent(1, vec![write_turn("failed", "a.rs", "initial"), final_turn()]);
    let unfinished =
        first.run(&task, context.clone(), "mock", CancellationToken::new()).await.unwrap();
    assert!(matches!(unfinished.outcome, AgentOutcome::NeedsRevision { .. }));
    context.previous_results.push(unfinished);
    let idle = fixture_agent(0, vec![final_turn()]);
    let still_open =
        idle.run(&task, context.clone(), "mock", CancellationToken::new()).await.unwrap();
    assert!(matches!(still_open.outcome, AgentOutcome::NeedsRevision { .. }));
    context.previous_results.push(still_open);
    let repaired = fixture_agent(0, vec![write_turn("repaired", "a.rs", "fixed"), final_turn()]);
    let finished = repaired.run(&task, context, "mock", CancellationToken::new()).await.unwrap();
    assert_eq!(finished.outcome, AgentOutcome::Success);
    assert_eq!(std::fs::read_to_string(dir.path().join("a.rs")).unwrap(), "fixed");
}

#[tokio::test]
async fn rejected_arguments_cannot_be_followed_by_a_false_completion_claim() {
    let dir = tempfile::tempdir().unwrap();
    let invalid = CompletionChunk {
        delta: "Doing work".into(),
        tool_call: Some(ToolCall {
            id: "invalid".into(),
            name: "write_file".into(),
            arguments: serde_json::json!({"content": "unchanged"}),
            ..Default::default()
        }),
        ..final_turn()
    };
    let agent = fixture_agent(0, vec![invalid, final_turn()]);
    let (task, context) = fixture_context(dir.path());
    let result = agent.run(&task, context, "mock", CancellationToken::new()).await.unwrap();
    assert!(matches!(result.outcome, AgentOutcome::NeedsRevision { .. }));
    assert!(result.files_modified.is_empty());
}

#[tokio::test]
async fn requested_tool_without_executor_is_an_unfinished_task() {
    let dir = tempfile::tempdir().unwrap();
    let agent = GenericSpecialistAgent::new(
        AgentId::new("coder"),
        "Coder".into(),
        None,
        Arc::new(SequencedProvider::new(vec![write_turn("no-backend", "a.rs", "x")])),
        None,
        EventBus::new(128),
        RetryPolicy::default(),
        PromptSections::default(),
        AgentCapabilities::default(),
    );
    let (task, mut context) = fixture_context(dir.path());
    let result = agent.run(&task, context.clone(), "mock", CancellationToken::new()).await.unwrap();
    let AgentOutcome::NeedsRevision { reason } = &result.outcome else {
        panic!("unexecuted tool requests are not completed work");
    };
    assert!(reason.contains("specialist-no-executor"));
    assert!(!dir.path().join("a.rs").exists());
    context.previous_results.push(result);
    let idle = fixture_agent(0, vec![final_turn()]);
    let still_open =
        idle.run(&task, context.clone(), "mock", CancellationToken::new()).await.unwrap();
    assert!(matches!(still_open.outcome, AgentOutcome::NeedsRevision { .. }));
    context.previous_results.push(still_open);
    let repaired = fixture_agent(0, vec![write_turn("executed", "a.rs", "fixed"), final_turn()]);
    let finished = repaired.run(&task, context, "mock", CancellationToken::new()).await.unwrap();
    assert_eq!(finished.outcome, AgentOutcome::Success);
    assert_eq!(std::fs::read_to_string(dir.path().join("a.rs")).unwrap(), "fixed");
}
