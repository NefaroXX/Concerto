// H40 control: initial typed history has the same request contract with or
// without an interactive inbox. The CLI test separately covers runtime wiring.

use super::*;

#[tokio::test]
async fn coordinator_typed_history_preserves_obligations_with_and_without_inbox() {
    let constraint = "Keep the public API stable; compatibility is not verified.";
    let obligation = "The migration rollback check is still required.";
    let prior_reply = "The earlier result remains provisional.";
    let message = |role: &str, content: String| -> Message {
        serde_json::from_value(serde_json::json!({
            "role": role, "content": content, "tool_calls": null, "tool_results": null
        }))
        .unwrap()
    };
    let mut history = vec![
        message("User", obligation.into()),
        message("User", format!("{}\n{constraint}", "background ".repeat(600))),
    ];
    history.extend(
        (0..6).map(|turn| {
            message("User", format!("Context turn {turn}: {}", "context ".repeat(500)))
        }),
    );
    history.push(message("Assistant", prior_reply.into()));

    for interactive in [false, true] {
        let project = tempfile::tempdir().unwrap();
        let session_id = Ulid::new();
        let (coordinator, provider) = coordinator_with_turns_captured(
            EventBus::new(256),
            Arc::new(AgentRegistry::from_mocks(vec![])),
            vec![CoordinatorTurn::Text("Status reviewed.".into())],
        );
        let mut coordinator =
            coordinator.with_conversation_history(history.clone()).with_conversation_inbox(
                interactive.then(crate::conversation::ConversationInbox::default),
            );
        let output = coordinator
            .run(
                AgentTask::new_coordinator_decides(session_id, "Explain the current status"),
                AgentContext::new(concerto_core::types::SessionContext::new(
                    session_id,
                    project.path().to_path_buf(),
                )),
                CancellationToken::new(),
                None,
            )
            .await
            .unwrap();
        assert_eq!(output.final_message, "Status reviewed.");
        let requests = provider.requests.lock().unwrap().clone();
        assert_eq!(requests.len(), 1);
        let messages = &requests[0].messages;
        for required in [constraint, obligation] {
            assert!(
                messages
                    .iter()
                    .any(|message| message.role == Role::User && message.content.contains(required)),
                "missing obligation with interactive={interactive}: {required}"
            );
        }
        assert_eq!(
            messages
                .iter()
                .filter(|message| message.role == Role::Assistant && message.content == prior_reply)
                .count(),
            1,
            "assistant history must retain its role exactly once"
        );
    }
}
