// Production transcript capture regressions.

#[derive(Default)]
struct TranscriptCaptureProvider {
    requests: std::sync::Mutex<Vec<CompletionRequest>>,
}

#[async_trait]
impl LlmProvider for TranscriptCaptureProvider {
    fn provider_name(&self) -> &'static str {
        "transcript-capture"
    }

    fn context_capacity(&self, _model: &str) -> concerto_core::types::TokenBudget {
        concerto_core::types::TokenBudget::new(128_000, 4_096)
    }

    fn approximate_cost(&self, _tokens_in: u64, _tokens_out: u64) -> f64 {
        0.0
    }

    async fn stream_completion(
        &self,
        request: CompletionRequest,
        _cancel: CancellationToken,
    ) -> Result<CompletionStream, ProviderError> {
        self.requests.lock().unwrap().push(request);
        Ok(Box::pin(stream::iter(vec![Ok(CompletionChunk {
            delta: "Saved context received.".into(),
            is_final: true,
            ..CompletionChunk::keepalive()
        })])))
    }
}

async fn transcript_regression_run(
    second: Result<Vec<CompletionChunk>, ProviderError>,
    iterations: u32,
) -> Vec<Message> {
    let should_fail = second.is_err();
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("transcript.db");
    let store =
        Arc::new(concerto_sessions::SqliteSessionStore::connect_path(&database).await.unwrap());
    let session = store
        .create_session(
            camino::Utf8Path::new(dir.path().to_str().unwrap()),
            "google",
            "gemini",
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let call = ToolCall {
        id: "original-call".into(),
        name: "echo".into(),
        arguments: serde_json::json!({"text":"evidence"}),
        thought_signature: Some("opaque-call-signature".into()),
    };
    let native_parts = vec![
        serde_json::json!({"text":"Recorded model reasoning.", "thought":true}),
        serde_json::json!({"text":"I must verify the evidence before declaring completion."}),
        serde_json::json!({"functionCall":{"id":call.id,"name":call.name,"args":call.arguments}, "thoughtSignature":"opaque-call-signature"}),
    ];
    let replay = concerto_core::provider_replay::GeminiReplay::capture(
        native_parts,
        "I must verify the evidence before declaring completion.",
        Some("Recorded model reasoning."),
        std::slice::from_ref(&call),
    )
    .unwrap();
    let provider = Arc::new(CountingScriptedProvider::new(vec![
        Ok(vec![CompletionChunk {
            delta: "I must verify the evidence before declaring completion.".into(),
            reasoning: Some("Recorded model reasoning.".into()),
            tool_call: Some(call),
            gemini_replay: Some(replay),
            usage: Some(CompletionUsage {
                prompt_tokens: Some(11),
                completion_tokens: Some(7),
                ..Default::default()
            }),
            is_final: true,
        }]),
        second,
    ]));
    let mut agent = make_loop_with_dir(
        provider.clone(),
        Arc::new(ApprovalTestHarness::always_approve()),
        iterations,
        dir.path().to_str().unwrap(),
    )
    .with_session_store(Some(store.clone()));
    let result = agent
        .run(
            AgentTask::new_with_execution_mode(
                session.id,
                "Inspect the evidence",
                TaskExecutionMode::AnswerOnly,
            ),
            CancellationToken::new(),
        )
        .await;
    assert_eq!(
        provider.calls.load(Ordering::SeqCst),
        2,
        "the loop must reach the second provider turn"
    );
    assert_eq!(result.is_err(), should_fail, "unexpected run outcome: {result:?}");
    if let Ok(output) = result {
        assert_eq!(
            output.completion_status,
            concerto_core::types::AgentCompletionStatus::Completed
        );
    }
    let metrics = agent.provider_metrics();
    let expected_usage = if should_fail { (11, 7) } else { (24, 12) };
    assert_eq!((metrics[0].tokens_in, metrics[0].tokens_out), expected_usage);
    let saved = store.load_messages(session.id, CancellationToken::new()).await.unwrap();
    assert_eq!(
        (
            saved.iter().filter_map(|m| m.tokens_in).sum::<u64>(),
            saved.iter().filter_map(|m| m.tokens_out).sum::<u64>()
        ),
        expected_usage,
        "each completed request is counted once in durable history"
    );
    // Drop the loop and its store, then open an independent disk connection.
    // No old loop memory or fallback history may supply the next request.
    drop(agent);
    drop(store);
    let store =
        Arc::new(concerto_sessions::SqliteSessionStore::connect_path(&database).await.unwrap());
    let history = crate::context_engine::ContextEngine::new()
        .assemble(store.clone(), session.id, &[], CancellationToken::new(), None)
        .await
        .unwrap();
    let captured = Arc::new(TranscriptCaptureProvider::default());
    let mut resumed = make_loop_with_dir(
        captured.clone(),
        Arc::new(ApprovalTestHarness::always_approve()),
        2,
        dir.path().to_str().unwrap(),
    )
    .with_session_store(Some(store))
    .with_initial_messages(history);
    let output = resumed
        .run(
            AgentTask::new_with_execution_mode(
                session.id,
                "Continue from the saved evidence",
                TaskExecutionMode::AnswerOnly,
            ),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(output.completion_status, concerto_core::types::AgentCompletionStatus::Completed);
    let requests = captured.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    requests[0].messages.clone()
}

fn assert_transcript_retains_completed_exchange(messages: &[Message]) {
    let assistant = messages.iter().find(|message| message.tool_calls.is_some()).expect(
        "a completed provider tool-call turn must be stored, not replaced by the run summary",
    );
    assert_eq!(assistant.content, "I must verify the evidence before declaring completion.");
    assert_eq!(assistant.reasoning_content.as_deref(), Some("Recorded model reasoning."));
    let call = &assistant.tool_calls.as_ref().unwrap()[0];
    assert_eq!(call.id, "original-call");
    assert_eq!(call.thought_signature.as_deref(), Some("opaque-call-signature"));
    let replay = assistant.gemini_replay.as_ref().expect("native replay must survive settlement");
    let parts = replay.parts_for(assistant).expect("reloaded canonical turn must match its replay");
    assert_eq!(parts[2]["thoughtSignature"], "opaque-call-signature");
    assert!(
        messages.iter().any(|message| message
            .tool_results
            .as_ref()
            .is_some_and(|results| results.iter().any(|result| result.id == call.id))),
        "the matching executed tool result must also be stored"
    );
}

#[tokio::test]
async fn transcript_regression_success_preserves_original_provider_exchange() {
    let messages = transcript_regression_run(
        Ok(vec![CompletionChunk {
            delta: "Evidence verified.".into(),
            usage: Some(CompletionUsage {
                prompt_tokens: Some(13),
                completion_tokens: Some(5),
                ..Default::default()
            }),
            is_final: true,
            ..CompletionChunk::keepalive()
        }]),
        4,
    )
    .await;
    assert_transcript_retains_completed_exchange(&messages);
}

#[tokio::test]
async fn transcript_regression_later_provider_failure_preserves_completed_exchange() {
    let messages = transcript_regression_run(Err(ProviderError::AuthFailure), 4).await;
    assert_transcript_retains_completed_exchange(&messages);
}

#[tokio::test]
async fn transcript_regression_reusing_loop_does_not_bill_previous_run() {
    let provider = Arc::new(CountingScriptedProvider::new(vec![
        Ok(vec![CompletionChunk {
            delta: "First answer.".into(),
            usage: Some(CompletionUsage {
                prompt_tokens: Some(11),
                completion_tokens: Some(7),
                ..Default::default()
            }),
            is_final: true,
            ..CompletionChunk::keepalive()
        }]),
        Ok(vec![CompletionChunk {
            delta: "Second answer.".into(),
            usage: Some(CompletionUsage {
                prompt_tokens: Some(13),
                completion_tokens: Some(5),
                ..Default::default()
            }),
            is_final: true,
            ..CompletionChunk::keepalive()
        }]),
    ]));
    let mut agent = make_loop(provider, Arc::new(ApprovalTestHarness::always_approve()), 2);
    for expected in [(11, 7), (13, 5)] {
        let output = agent
            .run(
                AgentTask::new_with_execution_mode(
                    Ulid::new(),
                    "Answer",
                    TaskExecutionMode::AnswerOnly,
                ),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(
            (output.provider_metrics[0].tokens_in, output.provider_metrics[0].tokens_out),
            expected
        );
    }
}

fn transcript_usage_chunk(text: &str) -> CompletionChunk {
    CompletionChunk {
        delta: text.into(),
        usage: Some(CompletionUsage {
            prompt_tokens: Some(5),
            completion_tokens: Some(3),
            ..Default::default()
        }),
        is_final: true,
        ..CompletionChunk::keepalive()
    }
}

#[tokio::test]
async fn transcript_regression_text_repairs_preserve_each_completed_turn_and_its_usage() {
    let malformed = "<tool_calls>not json</tool_calls>";
    let valid = r#"<tool_calls>[{"name":"echo","arguments":{"text":"evidence"}}]</tool_calls>"#;
    for ending in ["success", "provider_failure", "exhaustion"] {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(
            concerto_sessions::SqliteSessionStore::connect_path(&dir.path().join("repairs.db"))
                .await
                .unwrap(),
        );
        let session = store
            .create_session(
                camino::Utf8Path::new(dir.path().to_str().unwrap()),
                "test",
                "test",
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let (script, completed_requests) = match ending {
            "success" => (
                vec![
                    Ok(vec![transcript_usage_chunk(malformed)]),
                    Ok(vec![transcript_usage_chunk(valid)]),
                    Ok(vec![transcript_usage_chunk("Evidence checked.")]),
                ],
                3,
            ),
            "provider_failure" => (
                vec![Ok(vec![transcript_usage_chunk(malformed)]), Err(ProviderError::AuthFailure)],
                1,
            ),
            _ => {
                let count = 1 + crate::tool_driver::MAX_REPAIR_ATTEMPTS as usize;
                (vec![Ok(vec![transcript_usage_chunk(malformed)]); count], count)
            }
        };
        let provider = Arc::new(CountingScriptedProvider::new(script));
        let mut agent = make_loop_with_dir(
            provider,
            Arc::new(ApprovalTestHarness::always_approve()),
            4,
            dir.path().to_str().unwrap(),
        )
        .with_session_store(Some(store.clone()))
        .with_advertised_tool_support(Some(false));
        let output = agent
            .run(
                AgentTask::new_with_execution_mode(
                    session.id,
                    "Inspect evidence",
                    TaskExecutionMode::AnswerOnly,
                ),
                CancellationToken::new(),
            )
            .await;
        assert_eq!(output.is_ok(), ending == "success", "{ending}: {output:?}");
        let messages = store.load_messages(session.id, CancellationToken::new()).await.unwrap();
        let assistants: Vec<_> = messages.iter().filter(|m| m.role == Role::Assistant).collect();
        assert_eq!(assistants.len(), completed_requests, "{ending}: {messages:?}");
        for assistant in assistants {
            assert_eq!((assistant.tokens_in, assistant.tokens_out), (Some(5), Some(3)), "{ending}");
        }
        let metrics = agent.provider_metrics();
        assert_eq!(
            (metrics[0].tokens_in, metrics[0].tokens_out),
            (5 * completed_requests as u64, 3 * completed_requests as u64),
            "{ending}"
        );
        assert_eq!(
            messages.iter().filter(|m| m.content == malformed).count(),
            if ending == "exhaustion" { completed_requests } else { 1 }
        );
    }
}

#[tokio::test]
async fn transcript_regression_delivered_followup_is_persisted_once() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(
        concerto_sessions::SqliteSessionStore::connect_path(&dir.path().join("followup.db"))
            .await
            .unwrap(),
    );
    let session = store
        .create_session(
            camino::Utf8Path::new(dir.path().to_str().unwrap()),
            "test",
            "test",
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let inbox = crate::conversation::ConversationInbox::default();
    inbox.send("Keep the original evidence intact".into()).unwrap();
    let provider = Arc::new(TranscriptCaptureProvider::default());
    let mut agent = make_loop_with_dir(
        provider.clone(),
        Arc::new(ApprovalTestHarness::always_approve()),
        2,
        dir.path().to_str().unwrap(),
    )
    .with_session_store(Some(store.clone()))
    .with_conversation_inbox(Some(inbox));
    agent
        .run(
            AgentTask::new_with_execution_mode(
                session.id,
                "Inspect evidence",
                TaskExecutionMode::AnswerOnly,
            ),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let messages = store.load_messages(session.id, CancellationToken::new()).await.unwrap();
    assert_eq!(
        messages.iter().filter(|m| m.content == "Keep the original evidence intact").count(),
        1
    );
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0]
            .messages
            .iter()
            .filter(|m| m.content == "Keep the original evidence intact")
            .count(),
        1
    );
}

#[tokio::test]
async fn transcript_regression_cancellation_keeps_completed_tool_result() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(
        concerto_sessions::SqliteSessionStore::connect_path(&dir.path().join("cancelled.db"))
            .await
            .unwrap(),
    );
    let session = store
        .create_session(
            camino::Utf8Path::new(dir.path().to_str().unwrap()),
            "test",
            "test",
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let cancel = CancellationToken::new();
    let (tool, calls) = ScriptedShellTool::with_cancel_on_execute(
        ScriptedShellOutcome::Exit(1, "completed stdout".into(), "completed stderr".into()),
        Some(cancel.clone()),
    );
    let provider = Arc::new(ScriptedProvider::new(vec![vec![ToolCall {
        id: "completed-before-cancel".into(),
        name: "shell".into(),
        arguments: serde_json::json!({"command":"check"}),
        ..Default::default()
    }]]));
    let mut agent = make_loop_with_extra_tools(
        provider.clone(),
        Arc::new(ApprovalTestHarness::always_approve()),
        4,
        vec![Box::new(tool)],
    )
    .with_session_store(Some(store.clone()));
    agent.project_root = dir.path().to_path_buf();
    let result = agent
        .run(
            AgentTask::new_with_execution_mode(
                session.id,
                "Inspect evidence",
                TaskExecutionMode::AnswerOnly,
            ),
            cancel.clone(),
        )
        .await;
    assert!(cancel.is_cancelled());
    assert!(matches!(result, Err(OrchestratorError::Cancelled)), "{result:?}");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(provider.call_count.load(Ordering::SeqCst), 1);
    let messages = store.load_messages(session.id, CancellationToken::new()).await.unwrap();
    let result = messages
        .iter()
        .filter_map(|m| m.tool_results.as_ref())
        .flatten()
        .find(|r| r.id == "completed-before-cancel")
        .expect("completed result remains durable");
    assert!(result.content.to_string().contains("completed stderr"), "{result:?}");
}

#[tokio::test]
async fn transcript_regression_continuation_counts_all_rounds_once() {
    let messages = transcript_regression_run(
        Ok(vec![CompletionChunk {
            delta: "Evidence verified.".into(),
            usage: Some(CompletionUsage {
                prompt_tokens: Some(13),
                completion_tokens: Some(5),
                ..Default::default()
            }),
            is_final: true,
            ..CompletionChunk::keepalive()
        }]),
        1,
    )
    .await;
    assert_transcript_retains_completed_exchange(&messages);
}

#[tokio::test]
async fn transcript_regression_incomplete_provider_attempt_is_not_durable() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(
        concerto_sessions::SqliteSessionStore::connect_path(&dir.path().join("incomplete.db"))
            .await
            .unwrap(),
    );
    let session = store
        .create_session(
            camino::Utf8Path::new(dir.path().to_str().unwrap()),
            "test",
            "test",
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let provider = Arc::new(CountingScriptedProvider::new(vec![
        Ok(vec![CompletionChunk {
            delta: "Unfinished generation".into(),
            tool_call: Some(ToolCall {
                id: "unfinished".into(),
                name: "shell".into(),
                arguments: serde_json::json!({"command":"check"}),
                ..Default::default()
            }),
            is_final: false,
            ..CompletionChunk::keepalive()
        }]),
        Err(ProviderError::AuthFailure),
    ]));
    let (tool, calls) = ScriptedShellTool::new(ScriptedShellOutcome::Exit(0, "".into(), "".into()));
    let mut agent = make_loop_with_extra_tools(
        provider.clone(),
        Arc::new(ApprovalTestHarness::always_approve()),
        4,
        vec![Box::new(tool)],
    )
    .with_session_store(Some(store.clone()));
    agent.project_root = dir.path().to_path_buf();
    let result = agent
        .run(
            AgentTask::new_with_execution_mode(
                session.id,
                "Inspect evidence",
                TaskExecutionMode::AnswerOnly,
            ),
            CancellationToken::new(),
        )
        .await;
    assert!(
        matches!(result, Err(OrchestratorError::Provider(ProviderError::AuthFailure))),
        "{result:?}"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        provider.calls.load(Ordering::SeqCst),
        2,
        "unfinished attempt is retried without executing it"
    );
    let messages = store.load_messages(session.id, CancellationToken::new()).await.unwrap();
    assert_eq!(messages.len(), 2, "user input and runtime failure are durable");
    assert_eq!(messages[0].role, Role::User);
    assert_eq!(messages[1].role, Role::System);
    assert!(messages[1].content.contains("Concerto runtime outcome"));
    assert!(messages
        .iter()
        .all(|message| message.tool_calls.is_none() && message.tool_results.is_none()));
    assert_eq!(
        (agent.provider_metrics()[0].tokens_in, agent.provider_metrics()[0].tokens_out),
        (0, 0)
    );
}

#[tokio::test]
async fn transcript_regression_resuming_missing_result_does_not_execute_historical_call() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("interrupted.db");
    let store =
        Arc::new(concerto_sessions::SqliteSessionStore::connect_path(&database).await.unwrap());
    let session = store
        .create_session(
            camino::Utf8Path::new(dir.path().to_str().unwrap()),
            "test",
            "test",
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let original: Message = serde_json::from_value(serde_json::json!({
        "role":"Assistant", "content":"Check the workspace", "tool_results":null,
        "tool_calls":[{"id":"interrupted-call", "name":"shell", "arguments":{"command":"check"}}]
    }))
    .unwrap();
    store.append_messages(session.id, &[original], CancellationToken::new()).await.unwrap();
    drop(store);
    let store =
        Arc::new(concerto_sessions::SqliteSessionStore::connect_path(&database).await.unwrap());
    let history = crate::context_engine::ContextEngine::new()
        .assemble(store.clone(), session.id, &[], CancellationToken::new(), None)
        .await
        .unwrap();
    let provider = Arc::new(TranscriptCaptureProvider::default());
    let (tool, calls) = ScriptedShellTool::new(ScriptedShellOutcome::Exit(0, "".into(), "".into()));
    let mut agent = make_loop_with_extra_tools(
        provider.clone(),
        Arc::new(ApprovalTestHarness::always_approve()),
        2,
        vec![Box::new(tool)],
    )
    .with_session_store(Some(store.clone()))
    .with_initial_messages(history);
    agent.project_root = dir.path().to_path_buf();
    agent
        .run(
            AgentTask::new_with_execution_mode(
                session.id,
                "Explain the interrupted check",
                TaskExecutionMode::AnswerOnly,
            ),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 0, "history replay must not execute old calls");
    {
        let requests = provider.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        let results: Vec<_> =
            requests[0].messages.iter().filter_map(|m| m.tool_results.as_ref()).flatten().collect();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "interrupted-call");
        assert_eq!(results[0].content["status"], "outcome_unknown");
    }
    let saved = store.load_messages(session.id, CancellationToken::new()).await.unwrap();
    assert!(
        saved.iter().all(|m| m.tool_results.is_none()),
        "synthetic recovery is not executed evidence"
    );
}

#[tokio::test]
async fn transcript_regression_post_provider_verification_survives_disk_reopen() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("Cargo.toml"), "[workspace]\n").unwrap();
    let database = dir.path().join("outcome.db");
    let store =
        Arc::new(concerto_sessions::SqliteSessionStore::connect_path(&database).await.unwrap());
    let session = store
        .create_session(
            camino::Utf8Path::from_path(dir.path()).unwrap(),
            "test",
            "test",
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let provider = Arc::new(ScriptedProvider::new(vec![
        vec![completion_test_write("output.txt", "done")],
        vec![],
    ]));
    let mut agent = make_loop_with_fs_tool(
        dir.path(),
        provider,
        Arc::new(ApprovalTestHarness::always_approve()),
        4,
    )
    .with_session_store(Some(store.clone()));
    agent.eval = EvalEngine::new(dir.path())
        .with_process_executor(Arc::new(CompletionEvalExecutor("denied")));
    let output = agent
        .run(
            AgentTask::new_action_required(session.id, "Write the output file"),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(output.completion_status, concerto_core::types::AgentCompletionStatus::Partial);
    assert!(output
        .verification
        .iter()
        .any(|check| check.outcome == Some(VerificationOutcome::Error)));
    drop(agent);
    drop(store);
    let store =
        Arc::new(concerto_sessions::SqliteSessionStore::connect_path(&database).await.unwrap());
    let history = crate::context_engine::ContextEngine::new()
        .assemble(store, session.id, &[], CancellationToken::new(), None)
        .await
        .unwrap();
    let provider = Arc::new(TranscriptCaptureProvider::default());
    let mut resumed = make_loop_with_dir(
        provider.clone(),
        Arc::new(ApprovalTestHarness::always_approve()),
        2,
        dir.path().to_str().unwrap(),
    )
    .with_initial_messages(history);
    resumed
        .run(
            AgentTask::new_with_execution_mode(
                session.id,
                "Explain what remains",
                TaskExecutionMode::AnswerOnly,
            ),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    let observation = requests[0]
        .messages
        .iter()
        .find(|message| {
            message.content.contains("Concerto runtime outcome")
                && message.content.contains("Partial")
        })
        .expect("verification after the last provider turn must reach the next request");
    assert_eq!(observation.role, Role::System, "runtime facts must not impersonate a model answer");
    assert_eq!((observation.tokens_in, observation.tokens_out), (None, None));
    let record: serde_json::Value =
        serde_json::from_str(observation.content.split_once('\n').unwrap().1).unwrap();
    assert!(record["verification"]
        .as_array()
        .unwrap()
        .iter()
        .any(|check| check["outcome"] == "error"));
}

#[tokio::test]
async fn transcript_regression_partial_exit_preserves_termination_reason() {
    let store = Arc::new(concerto_sessions::SqliteSessionStore::connect_in_memory().await.unwrap());
    let session = store
        .create_session(camino::Utf8Path::new("/project"), "test", "test", CancellationToken::new())
        .await
        .unwrap();
    let agent = make_loop(
        Arc::new(ScriptedProvider::new(vec![])),
        Arc::new(ApprovalTestHarness::always_approve()),
        2,
    )
    .with_session_store(Some(store.clone()));
    let task = AgentTask::new(session.id, "Inspect evidence");
    agent.persist_run_start(&task, CancellationToken::new()).await;
    let mut output = agent_output_base();
    output.task_id = task.id;
    output.session_id = session.id;
    output.final_message = "Blocked: verification is still outstanding".into();
    output.completion_status = concerto_core::types::AgentCompletionStatus::Partial;
    agent.persist_run_partial(session.id, &output, CancellationToken::new()).await;
    let messages = store.load_messages(session.id, CancellationToken::new()).await.unwrap();
    assert!(
        messages.iter().any(|message| message.role == Role::System
            && message.content.contains("Blocked: verification is still outstanding")),
        "partial settlement must preserve its reason for the next request"
    );
}
