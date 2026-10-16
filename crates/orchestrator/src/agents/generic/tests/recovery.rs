//! Real specialist, filesystem, verification and durable checkpoint recovery.

use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

struct RecoveryProvider {
    responses: std::sync::Mutex<std::collections::VecDeque<CompletionChunk>>,
    interrupt_when_empty: bool,
    calls: AtomicUsize,
}

#[async_trait::async_trait]
impl LlmProvider for RecoveryProvider {
    async fn stream_completion(
        &self,
        _request: CompletionRequest,
        cancel: CancellationToken,
    ) -> Result<CompletionStream, concerto_core::error::ProviderError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let next = self.responses.lock().unwrap_or_else(|error| error.into_inner()).pop_front();
        match next {
            Some(chunk) => Ok(Box::pin(futures::stream::iter(vec![Ok(chunk)]))),
            None if self.interrupt_when_empty => {
                cancel.cancel();
                Err(concerto_core::error::ProviderError::Cancelled)
            }
            None => Err(concerto_core::error::ProviderError::Other("script exhausted".into())),
        }
    }

    fn context_capacity(&self, _model: &str) -> concerto_core::types::TokenBudget {
        concerto_core::types::TokenBudget::new(128_000, 4_096)
    }

    fn approximate_cost(&self, _tokens_in: u64, _tokens_out: u64) -> f64 {
        0.0
    }
    fn provider_name(&self) -> &'static str {
        "recovery-fixture"
    }
}

fn turn(id: &str, tool: &str, arguments: serde_json::Value) -> CompletionChunk {
    CompletionChunk {
        delta: "Execute the next required operation".into(),
        tool_call: Some(ToolCall {
            id: id.into(),
            name: tool.into(),
            arguments,
            ..Default::default()
        }),
        is_final: true,
        reasoning: None,
        usage: None,
    }
}

fn recovery_agent(
    root: &std::path::Path,
    pool: sqlx::SqlitePool,
    chunks: Vec<CompletionChunk>,
    interrupt: bool,
) -> Result<(GenericSpecialistAgent, Arc<crate::verify::VerifierService>), Box<dyn std::error::Error>>
{
    let root_utf8 = camino::Utf8PathBuf::from_path_buf(root.to_path_buf())
        .map_err(|_| std::io::Error::other("fixture path is not UTF-8"))?;
    let service =
        Arc::new(crate::verify::VerifierService::new(root.to_path_buf(), EvalEngine::new(root)));
    let mut tools = concerto_core::types::ToolRegistry::default();
    tools.register(Box::new(concerto_tools::filesystem::FilesystemTool::new(root_utf8)));
    tools.register(Box::new(crate::verify::VerifyRunTool::with_service(service.clone())));
    let executor = Arc::new(ToolExecutor::new(
        Arc::new(tools),
        Arc::new(concerto_core::policy::SimplePolicyEngine::new(
            vec![PolicyRule::AutoApprove(Condition::Always)],
            Arc::new(NullAudit),
        )),
    ));
    let provider = Arc::new(RecoveryProvider {
        responses: std::sync::Mutex::new(chunks.into()),
        interrupt_when_empty: interrupt,
        calls: AtomicUsize::new(0),
    });
    let agent = GenericSpecialistAgent::new(
        AgentId::new("coder"),
        "Coder".into(),
        Some(AgentStage::new("implement")),
        provider,
        Some(executor),
        EventBus::new(128),
        RetryPolicy::default(),
        PromptSections::default(),
        AgentCapabilities::default(),
    )
    .with_verifier_service(service.clone())
    .with_tool_facts(Some(crate::tool_facts::ToolFactContext::new(Some(pool), "coder")));
    Ok((agent, service))
}

#[tokio::test]
async fn real_specialist_recovers_failed_check_from_disk_without_repeating_effects(
) -> Result<(), Box<dyn std::error::Error>> {
    use crate::checkpoint::{self, CheckpointContext, CheckpointScope, CheckpointStage};
    use concerto_core::types::{SessionContext, SubTaskStatus, VerificationOutcome};
    let workspace = tempfile::tempdir()?;
    let storage = tempfile::tempdir()?;
    let options = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(storage.path().join("recovery.db"))
        .create_if_missing(true)
        .foreign_keys(true);
    let pool =
        sqlx::pool::PoolOptions::new().max_connections(2).connect_with(options.clone()).await?;
    sqlx::migrate!("../sessions/migrations").run(&pool).await?;
    let session =
        SessionContext::new(concerto_core::ids::Ulid::new(), workspace.path().to_path_buf());
    sqlx::query("INSERT INTO sessions (id, created_at, project_dir, provider, model) VALUES (?, 0, ?, 'fixture', 'mock')")
        .bind(session.session_id.to_string()).bind(workspace.path().to_string_lossy().as_ref())
        .execute(&pool).await?;
    let mut task =
        SubTask::new(session.session_id, AgentId::new("coder"), "Repair and verify index.html");
    let mut context = AgentContext::new(session);
    context.run_id = Some(concerto_core::ids::Ulid::new().to_string());
    let broken = "<html><head><link rel=\"stylesheet\" href=\"missing.css\"></head><body>retained</body></html>";
    let check = || {
        turn(
            "check",
            "verify.run",
            serde_json::json!({
                "scope": "index.html", "verifier": "existence", "overlay_pending": false,
            }),
        )
    };
    let (first, service) = recovery_agent(
        workspace.path(),
        pool.clone(),
        vec![
            turn(
                "initial-write",
                "filesystem",
                serde_json::json!({"operation": "write", "path": "index.html", "content": broken}),
            ),
            check(),
        ],
        true,
    )?;
    let unfinished = first.run(&task, context.clone(), "mock", CancellationToken::new()).await?;
    assert_eq!(std::fs::read_to_string(workspace.path().join("index.html"))?, broken);
    let AgentOutcome::NeedsRevision { reason } = &unfinished.outcome else {
        return Err(std::io::Error::other("interrupted specialist lost its progress").into());
    };
    let progress = crate::agents::execution_state::continuation(reason)
        .ok_or_else(|| std::io::Error::other("missing continuation"))?;
    assert_eq!(progress.stop_code, "specialist-interrupted");
    assert_eq!(
        progress.successful_calls.iter().filter(|call| call.call_id == "initial-write").count(),
        1
    );
    assert!(unfinished
        .verification
        .iter()
        .any(|record| record.outcome == Some(VerificationOutcome::Failed)));
    assert!(unfinished.verification.iter().all(|record| !record.evidence_ids.is_empty()));
    task.status = SubTaskStatus::NeedsRevision;
    let mut graph = crate::graph::TaskGraph::new();
    graph.add_root(task.clone());
    let scope = CheckpointScope {
        run_id: context
            .run_id
            .as_deref()
            .ok_or_else(|| std::io::Error::other("missing run"))?
            .parse()?,
        session_id: task.session_id,
        root_task_id: task.id,
        project_id: context.session.project_id.0.clone(),
        objective: task.description.clone(),
        objective_hash: blake3::hash(task.description.as_bytes()).to_hex().to_string(),
        source_revision: None,
        sequence_num: 7,
    };
    let checkpoint = checkpoint::build_checkpoint(
        &scope,
        CheckpointStage::Executing,
        None,
        &context.working_memory,
        &graph,
        &HashMap::from([(task.id, unfinished)]),
        0.0,
        2,
        &[],
        &[],
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        &CheckpointContext::default(),
    );
    let state = serde_json::to_string(&checkpoint)?;
    sqlx::query("INSERT INTO orchestration_checkpoints (session_id, run_id, root_task_id, project_id, objective_hash, schema_version, sequence_num, state_json, completed, updated_at) VALUES (?, ?, ?, ?, ?, ?, 7, ?, 0, 0)")
        .bind(task.session_id.to_string()).bind(scope.run_id.to_string()).bind(task.id.to_string())
        .bind(&scope.project_id).bind(&scope.objective_hash).bind(i64::from(checkpoint.schema_version))
        .bind(state).execute(&pool).await?;
    drop(first);
    drop(service);
    tokio::time::timeout(std::time::Duration::from_secs(5), pool.close()).await?;

    let pool = sqlx::pool::PoolOptions::new().max_connections(2).connect_with(options).await?;
    let state: String =
        sqlx::query_scalar("SELECT state_json FROM orchestration_checkpoints WHERE session_id = ?")
            .bind(task.session_id.to_string())
            .fetch_one(&pool)
            .await?;
    let restored = checkpoint::GraphCheckpoint::from_json(&state)?;
    assert_eq!(restored.run_id, scope.run_id);
    assert_eq!(restored.objective_hash, scope.objective_hash);
    let graph = checkpoint::restore_graph(&restored)?;
    assert_eq!(graph.get(&task.id).map(|node| node.status), Some(SubTaskStatus::NeedsRevision));
    context.previous_results.extend(restored.completed_results.into_values());
    let fixed = "<html><body>retained and repaired</body></html>";
    let (second, service) = recovery_agent(
        workspace.path(),
        pool.clone(),
        vec![
            turn(
                "initial-write",
                "filesystem",
                serde_json::json!({"operation": "write", "path": "index.html", "content": broken}),
            ),
            turn(
                "repair-write",
                "filesystem",
                serde_json::json!({"operation": "write", "path": "index.html", "content": fixed}),
            ),
            check(),
            CompletionChunk {
                delta: "Repaired and verified".into(),
                is_final: true,
                reasoning: None,
                tool_call: None,
                usage: None,
            },
        ],
        false,
    )?;
    let finished = second.run(&task, context, "mock", CancellationToken::new()).await?;
    assert_eq!(finished.task_id, task.id);
    assert_eq!(finished.outcome, AgentOutcome::Success);
    assert_eq!(finished.tool_call_count, 2);
    assert_eq!(std::fs::read_to_string(workspace.path().join("index.html"))?, fixed);
    assert!(!verification::has_failure(&finished.verification));
    assert!(
        service.check_ledger(&finished.verification, &CancellationToken::new()).await.fresh > 0
    );
    let events = concerto_sessions::whiteboard::load_whiteboard_events(
        &pool,
        &concerto_sessions::whiteboard::WhiteboardLoadOpts {
            after_gate_seq: 0,
            session_id: Some(task.session_id.to_string()),
            scope: None,
            limit: 100,
        },
    )
    .await?;
    assert_eq!(events.iter().filter(|event| event.payload["args"]["content"] == broken).count(), 1);
    assert_eq!(events.iter().filter(|event| event.payload["args"]["content"] == fixed).count(), 1);
    std::fs::write(workspace.path().join("index.html"), "changed after check")?;
    assert_eq!(
        service.check_ledger(&finished.verification, &CancellationToken::new()).await.fresh,
        0
    );
    drop(second);
    drop(service);
    tokio::time::timeout(std::time::Duration::from_secs(5), pool.close()).await?;
    Ok(())
}
