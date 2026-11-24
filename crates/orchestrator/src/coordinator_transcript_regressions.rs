// Coordinator transcript capture regression.

#[tokio::test]
async fn transcript_regression_coordinator_preserves_status_call_and_matching_result() {
    let project = tempfile::tempdir().unwrap();
    let store = Arc::new(concerto_sessions::SqliteSessionStore::connect_in_memory().await.unwrap());
    let session = store
        .create_session(
            camino::Utf8Path::new(project.path().to_str().unwrap()),
            "test",
            "test-model",
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let bus = EventBus::new(256);
    let mut events = bus.subscribe();
    let recorder = crate::runtime_runner_impl::start_transcript_recorder(
        &bus,
        store.clone(),
        session.id,
        Default::default(),
    );
    let (coordinator, provider) = coordinator_with_turns_captured(
        bus,
        Arc::new(AgentRegistry::from_mocks(vec![])),
        vec![
            CoordinatorTurn::Calls(vec![ToolCall {
                id: "durable-status-call".into(),
                name: crate::conversation::REPLY_TOOL.into(),
                arguments: serde_json::json!({"message":"I am checking that now."}),
                thought_signature: Some("original-status-signature".into()),
            }]),
            CoordinatorTurn::Text("The check is complete.".into()),
        ],
    );
    let mut coordinator = coordinator
        .with_checkpoint_store(Some(store.clone()), None)
        .with_conversation_inbox(Some(crate::conversation::ConversationInbox::default()));
    let output = coordinator
        .run(
            AgentTask::new_coordinator_decides(session.id, "Explain the current status"),
            AgentContext::new(concerto_core::types::SessionContext::new(
                session.id,
                project.path().to_path_buf(),
            )),
            CancellationToken::new(),
            None,
        )
        .await
        .unwrap();
    assert_eq!(provider.turn_count(), 2);
    assert_eq!(output.final_message, "The check is complete.");
    let mut visible_status_replies = 0;
    while let Ok(event) = events.try_recv() {
        if matches!(&event.kind, EventKind::AssistantMessage { content, .. }
            if content == "I am checking that now.")
        {
            visible_status_replies += 1;
        }
    }
    assert_eq!(visible_status_replies, 1, "status reply must be visible exactly once");
    recorder.stop().await;
    let transcript = store.load_transcript(session.id, CancellationToken::new()).await.unwrap();
    assert_eq!(
        transcript
            .iter()
            .filter(|entry| matches!(entry,
                concerto_core::transcript::TranscriptEntry::Assistant { content }
                if content == "I am checking that now."
            ))
            .count(),
        1,
        "status reply must be durable exactly once in the rendered transcript"
    );
    let messages = store.load_messages(session.id, CancellationToken::new()).await.unwrap();
    let call_index = messages
        .iter()
        .position(|message| {
            message.tool_calls.as_ref().is_some_and(|calls| {
                calls.iter().any(|call| {
                    call.id == "durable-status-call"
                        && call.thought_signature.as_deref() == Some("original-status-signature")
                })
            })
        })
        .expect("coordinator must persist the original status tool-call turn");
    assert!(
        messages.get(call_index + 1).is_some_and(|message| {
            message.role == Role::Tool
                && message.tool_results.as_ref().is_some_and(|results| {
                    results.iter().any(|result| result.id == "durable-status-call")
                })
        }),
        "the matching result must follow its call without a duplicate display message"
    );
}
