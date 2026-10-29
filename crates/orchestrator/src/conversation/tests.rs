use super::*;
use concerto_core::types::{AgentCompletionStatus, TaskId};

fn start(controller: &mut ConversationController, input: &str) -> ConversationTurn {
    match controller.submit(input.to_owned()) {
        Ok(Submission::Start(turn)) => turn,
        _ => panic!("expected a new turn"),
    }
}

fn completion(turn: &ConversationTurn, session_id: Ulid) -> ConversationCompletion {
    ConversationCompletion {
        turn_id: turn.id(),
        session_id: Some(session_id),
        reply_only: true,
        undelivered: Vec::new(),
        result: Ok(AgentOutput {
            task_id: TaskId::new(),
            session_id,
            final_message: "Hello".into(),
            files_modified: Vec::new(),
            tool_call_count: 0,
            eval_result: None,
            tool_events: Vec::new(),
            verification: Vec::new(),
            project_root: None,
            completion_status: AgentCompletionStatus::Completed,
            provider_metrics: Vec::new(),
            checkpoint_json: None,
            provider_error_class: None,
        }),
    }
}

#[tokio::test]
async fn conversation_accepts_followups_without_starting_another_run() {
    let mut controller = ConversationController::default();
    let turn = start(&mut controller, "Fix the bug");
    assert!(matches!(controller.submit("What did you find?".into()), Ok(Submission::Delivered)));
    assert!(matches!(controller.submit("Keep the public API".into()), Ok(Submission::Delivered)));
    let messages = turn.inbox.receive(None, None, Ulid::new(), &turn.cancel).await.unwrap();
    assert_eq!(
        messages.iter().map(|message| message.content.as_str()).collect::<Vec<_>>(),
        ["What did you find?", "Keep the public API"]
    );
    assert!(!turn.inbox.has_pending());
    assert_eq!(controller.active_turn, Some(turn.id()));
    assert_eq!(controller.status, RunStatus::Running);
}

#[test]
fn chat_work_chat_keeps_the_same_session() {
    let mut controller = ConversationController::default();
    let id = Ulid::new();
    for input in ["Let's discuss the design", "Go ahead and implement it", "Explain the change"] {
        let turn = start(&mut controller, input);
        assert!(controller.settle(&completion(&turn, id)));
        assert_eq!(controller.session_id, Some(id));
        assert_eq!(controller.status, RunStatus::Idle);
    }
}

#[test]
fn late_completion_cannot_replace_another_session_or_finish_its_run() {
    let mut controller = ConversationController::default();
    let old_turn = start(&mut controller, "Old work");
    let current = Ulid::new();
    controller.select_session(Some(current));
    assert!(old_turn.cancel.is_cancelled());
    let new_turn = start(&mut controller, "New work");
    assert!(!controller.settle(&completion(&old_turn, Ulid::new())));
    assert_eq!(controller.session_id, Some(current));
    assert_eq!(controller.active_turn, Some(new_turn.id()));
    assert_eq!(controller.status, RunStatus::Running);
}

#[test]
fn stopping_preserves_undelivered_messages_and_rejects_more_input() {
    let mut controller = ConversationController::default();
    let turn = start(&mut controller, "Work");
    assert!(matches!(controller.submit("A follow-up".into()), Ok(Submission::Delivered)));
    controller.cancel();
    assert!(turn.cancel.is_cancelled());
    assert_eq!(controller.status, RunStatus::Cancelling);
    assert!(controller.submit("Another".into()).is_err());
    assert_eq!(turn.inbox.close(), ["A follow-up"]);
}

#[test]
fn full_inbox_rejects_input_without_discarding_accepted_messages() {
    let mut controller = ConversationController::default();
    let turn = start(&mut controller, "Work");
    for n in 0..MAX_PENDING_MESSAGES {
        assert!(matches!(controller.submit(format!("Message {n}")), Ok(Submission::Delivered)));
    }
    assert!(controller.submit("Overflow".into()).is_err());
    assert_eq!(turn.inbox.close().len(), MAX_PENDING_MESSAGES);
}

#[test]
fn completion_race_preserves_the_draft_instead_of_accepting_into_a_closed_inbox() {
    let mut controller = ConversationController::default();
    let turn = start(&mut controller, "Work");
    turn.inbox.close();
    assert!(controller.submit("Follow-up".into()).is_err());
    assert!(controller.settle(&completion(&turn, Ulid::new())));
    assert!(matches!(controller.submit("Follow-up".into()), Ok(Submission::Start(_))));
}

#[test]
fn queued_input_crosses_a_graph_boundary_before_the_inbox_closes() {
    let mut controller = ConversationController::default();
    let turn = start(&mut controller, "Work");
    controller.submit("Discuss the result".into()).ok();
    turn.inbox.yield_to_input();
    assert!(turn.inbox.is_handoff());
    assert_eq!(turn.inbox.next_turn_or_close().as_deref(), Some("Discuss the result"));
    assert!(!turn.inbox.is_handoff());
    assert!(matches!(controller.submit("One more detail".into()), Ok(Submission::Delivered)));
    assert_eq!(turn.inbox.next_turn_or_close().as_deref(), Some("One more detail"));
    assert!(turn.inbox.next_turn_or_close().is_none());
    assert!(controller.submit("Too late".into()).is_err());
}

#[tokio::test]
async fn status_reply_is_persisted_and_visible_without_closing_the_inbox() {
    let store: Arc<dyn SessionStore> =
        Arc::new(concerto_sessions::SqliteSessionStore::connect_in_memory().await.unwrap());
    let cancel = CancellationToken::new();
    let session = store
        .create_session(camino::Utf8Path::new("/project"), "test", "test", cancel.clone())
        .await
        .unwrap();
    let bus = EventBus::new(16);
    let mut events = bus.subscribe();
    let task = concerto_core::types::AgentTask::new_coordinator_decides(session.id, "Work");
    let result = reply_to_user(
        &serde_json::json!({"message": "Tests are running"}),
        Some(&store),
        &bus,
        &task,
        &cancel,
    )
    .await;
    assert_eq!(result["continue_work"], true);
    assert!(
        matches!(&events.try_recv().unwrap().kind, EventKind::AssistantMessage { content, .. } if content == "Tests are running")
    );
    let history = store.load_messages(session.id, cancel.clone()).await.unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].role, Role::Assistant);
    cancel.cancel();
    let rejected = reply_to_user(
        &serde_json::json!({"message": "Cancelled reply"}),
        Some(&store),
        &bus,
        &task,
        &cancel,
    )
    .await;
    assert!(rejected.get("error").is_some());
    assert!(events.try_recv().is_err());
}

#[test]
fn invalid_input_never_starts_work() {
    let mut controller = ConversationController::default();
    assert!(controller.submit(" \n".into()).is_err());
    assert!(controller.submit("x".repeat(MAX_MESSAGE_BYTES + 1)).is_err());
    assert_eq!(controller.status, RunStatus::Idle);
}

#[tokio::test]
async fn delivered_input_is_persisted_once_for_history_and_replay() {
    let store: Arc<dyn SessionStore> =
        Arc::new(concerto_sessions::SqliteSessionStore::connect_in_memory().await.unwrap());
    let cancel = CancellationToken::new();
    let session = store
        .create_session(camino::Utf8Path::new("/project"), "test", "test", cancel.clone())
        .await
        .unwrap();
    let inbox = ConversationInbox::default();
    inbox.send("Keep the API stable".into()).unwrap();
    let messages = inbox.receive(Some(&store), None, session.id, &cancel).await.unwrap();
    assert_eq!(messages.len(), 1);
    assert!(inbox.receive(Some(&store), None, session.id, &cancel).await.unwrap().is_empty());
    let manager = ProjectSessionManager::from_store(store.clone());
    assert_eq!(manager.load_recent_messages(session.id, cancel.clone()).await.unwrap().len(), 1);
    assert_eq!(
        store.load_transcript(session.id, cancel).await.unwrap(),
        [TranscriptEntry::User { content: "Keep the API stable".into() }]
    );
}

#[tokio::test]
async fn cancelled_persistence_returns_accepted_input_to_recovery_queue() {
    let store: Arc<dyn SessionStore> =
        Arc::new(concerto_sessions::SqliteSessionStore::connect_in_memory().await.unwrap());
    let cancel = CancellationToken::new();
    let inbox = ConversationInbox::default();
    inbox.send("Keep me".into()).unwrap();
    cancel.cancel();
    assert!(inbox.receive(Some(&store), None, Ulid::new(), &cancel).await.is_err());
    assert_eq!(inbox.close(), ["Keep me"]);
}
