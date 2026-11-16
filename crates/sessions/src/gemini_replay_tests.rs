use super::*;
use concerto_core::provider_replay::GeminiReplay;
use concerto_core::types::Role;
use serde_json::json;

#[tokio::test]
async fn replay_regression_single_and_batch_writes_survive_database_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("replay.db");
    let store = SqliteSessionStore::connect_path(&path).await.unwrap();
    let session = store
        .create_session("/project".into(), "google", "gemini", CancellationToken::new())
        .await
        .unwrap();
    let parts = vec![json!({"text":"response", "thoughtSignature":"opaque"})];
    let message = Message {
        role: Role::Assistant,
        content: "response".into(),
        tool_calls: None,
        tool_results: None,
        reasoning_content: None,
        tokens_in: None,
        tokens_out: None,
        gemini_replay: Some(GeminiReplay::capture(parts.clone(), "response", None, &[]).unwrap()),
    };
    store.save_message(session.id, &message, 0, 0, CancellationToken::new()).await.unwrap();
    store
        .append_messages(session.id, std::slice::from_ref(&message), CancellationToken::new())
        .await
        .unwrap();
    let mut legacy = message;
    legacy.gemini_replay = None;
    store.save_message(session.id, &legacy, 0, 0, CancellationToken::new()).await.unwrap();
    store.pool.close().await;
    drop(store);
    let reopened = SqliteSessionStore::connect_path(&path).await.unwrap();
    let messages = reopened.load_messages(session.id, CancellationToken::new()).await.unwrap();
    assert_eq!(messages.len(), 3);
    for message in &messages[..2] {
        assert_eq!(message.gemini_replay.as_ref().unwrap().parts_for(message).unwrap(), parts);
    }
    assert!(messages[2].gemini_replay.is_none());
    reopened.pool.close().await;
}
