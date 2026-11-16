//! Continuity regressions exercise durable compaction, not display excerpts.

use super::*;
use camino::Utf8Path;
use concerto_core::error::ProviderError;
use concerto_core::traits::provider::{CompletionStream, LlmProvider};
use concerto_core::types::{CompletionRequest, TokenBudget};
use concerto_providers::adapters::{
    AnthropicChatDialect, Dialect, GeminiChatDialect, ReasoningEcho,
};
use concerto_providers::context_guard::ContextGuardProvider;
use concerto_sessions::SqliteSessionStore;
use std::sync::Mutex;

fn message(role: Role, content: impl Into<String>) -> Message {
    Message {
        gemini_replay: None,
        role,
        content: content.into(),
        tool_calls: None,
        tool_results: None,
        reasoning_content: None,
        tokens_in: None,
        tokens_out: None,
    }
}

fn policy() -> CompactionPolicy {
    CompactionPolicy {
        trigger_tokens: 10,
        retain_user_turns: 2,
        minimum_user_turns: 4,
        max_summary_chars: 500,
        max_message_excerpt_chars: 200,
        merge_width: 2,
    }
}

async fn store_and_session() -> (Arc<SqliteSessionStore>, Ulid) {
    let store = Arc::new(SqliteSessionStore::connect_in_memory().await.unwrap());
    let session = store
        .create_session(
            Utf8Path::new(env!("CARGO_MANIFEST_DIR")),
            "provider",
            "model",
            CancellationToken::new(),
        )
        .await
        .unwrap();
    (store, session.id)
}

fn turns(from: usize, to: usize) -> Vec<Message> {
    (from..to)
        .flat_map(|turn| {
            [
                message(Role::User, format!("Continue task {turn}")),
                message(
                    Role::Assistant,
                    format!("Progress on task {turn}; {}", "details ".repeat(40)),
                ),
            ]
        })
        .collect()
}

async fn materialized(
    store: &SqliteSessionStore,
    session: Ulid,
    messages: &[Message],
) -> Vec<Message> {
    maintain_checkpoints(store, session, messages, policy(), CancellationToken::new(), None)
        .await
        .unwrap();
    let checkpoints =
        load_context_checkpoints(store, session, CancellationToken::new()).await.unwrap();
    assert!(!checkpoints.is_empty(), "fixture must exercise compaction");
    materialize_history(messages, &checkpoints)
}

#[tokio::test]
async fn compaction_regression_preserves_constraint_after_long_user_preamble() {
    const CONSTRAINT: &str =
        "Never modify the production database; implement this task using fixtures only.";
    let (store, session) = store_and_session().await;
    let mut messages = turns(0, 8);
    messages[0].content = format!("Project background: {}\n{CONSTRAINT}", "context ".repeat(100));
    store.append_messages(session, &messages, CancellationToken::new()).await.unwrap();
    let active = materialized(store.as_ref(), session, &messages).await;
    assert!(active.iter().any(|entry| entry.content.contains(CONSTRAINT)),
        "the original user constraint must survive in active model input, not just the audit transcript");
}

#[tokio::test]
async fn compaction_regression_hierarchy_preserves_constraint_visible_in_leaf() {
    const CONSTRAINT: &str = "Preserve all existing public APIs.";
    let (store, session) = store_and_session().await;
    let mut messages = turns(0, 6);
    messages[0].content = format!("{} {CONSTRAINT}", "Background ".repeat(8));
    store.append_messages(session, &messages, CancellationToken::new()).await.unwrap();
    let before = materialized(store.as_ref(), session, &messages).await;
    assert!(
        before.iter().any(|entry| entry.content.contains(CONSTRAINT)),
        "fixture control: the leaf checkpoint still contains the constraint"
    );

    let followup = turns(6, 10);
    store.append_messages(session, &followup, CancellationToken::new()).await.unwrap();
    messages.extend(followup);
    let active = materialized(store.as_ref(), session, &messages).await;
    let checkpoints =
        load_context_checkpoints(store.as_ref(), session, CancellationToken::new()).await.unwrap();
    assert!(
        checkpoints.iter().any(|entry| entry.checkpoint.level > 0),
        "fixture must exercise a hierarchical checkpoint merge"
    );
    assert!(
        active.iter().any(|entry| entry.content.contains(CONSTRAINT)),
        "merging durable summaries must not erase a still-active user constraint"
    );
}

#[tokio::test]
async fn compaction_regression_preserves_original_roles_and_instruction_order() {
    let (store, session) = store_and_session().await;
    let mut messages = turns(0, 8);
    messages[0].content = "Use format A for the export.".into();
    messages[2].content = "Correction: use format B for the export.".into();
    store.append_messages(session, &messages, CancellationToken::new()).await.unwrap();
    let active = materialized(store.as_ref(), session, &messages).await;
    let expected: Vec<_> = messages.iter().map(|entry| (&entry.role, &entry.content)).collect();
    let actual: Vec<_> = active
        .iter()
        .filter(|entry| matches!(entry.role, Role::User | Role::Assistant))
        .map(|entry| (&entry.role, &entry.content))
        .collect();
    assert_eq!(actual, expected, "compaction must preserve conversational authority and order");
}

#[tokio::test]
async fn compaction_regression_preserves_unfinished_assistant_commitment() {
    const OBLIGATION: &str =
        "Rollback verification is unfinished; do that before reporting completion.";
    let (store, session) = store_and_session().await;
    let mut messages = turns(0, 8);
    messages[1].content = format!("{}\n{OBLIGATION}", "Progress details. ".repeat(100));
    store.append_messages(session, &messages, CancellationToken::new()).await.unwrap();
    let active = materialized(store.as_ref(), session, &messages).await;
    assert!(active
        .iter()
        .any(|entry| entry.role == Role::Assistant && entry.content == messages[1].content));
}

struct CaptureProvider(Arc<Mutex<Option<CompletionRequest>>>);

#[async_trait::async_trait]
impl LlmProvider for CaptureProvider {
    async fn stream_completion(
        &self,
        request: CompletionRequest,
        _cancel: CancellationToken,
    ) -> Result<CompletionStream, ProviderError> {
        *self.0.lock().unwrap() = Some(request);
        Ok(Box::pin(futures::stream::empty()))
    }

    fn context_capacity(&self, _model: &str) -> TokenBudget {
        TokenBudget::new(10_000, 512)
    }

    fn approximate_cost(&self, _input: u64, _output: u64) -> f64 {
        0.0
    }

    fn provider_name(&self) -> &'static str {
        "capture"
    }
}

#[tokio::test]
async fn compaction_regression_reload_to_provider_retains_text_and_protocol_pairs() {
    let (store, session) = store_and_session().await;
    let mut messages = vec![message(Role::System, "Keep the project constraints.")];
    for turn in 0..8 {
        messages.push(message(Role::User, format!("Task {turn}: preserve public API {turn}.")));
        let [mut call, result] = super::tests::tool_exchange(turn);
        call.content = format!("Task {turn}: rollback verification remains unfinished.");
        call.reasoning_content = Some("private replay reasoning".into());
        messages.extend([call, result]);
        messages
            .push(message(Role::Assistant, format!("Task {turn}: implementation in progress.")));
    }
    store.append_messages(session, &messages, CancellationToken::new()).await.unwrap();
    maintain_checkpoints(
        store.as_ref(),
        session,
        &messages,
        policy(),
        CancellationToken::new(),
        None,
    )
    .await
    .unwrap();
    // Reload from the store, with no caller history to rescue lost obligations.
    let active = refresh_and_materialize_with_policy(
        store.clone(),
        session,
        &[],
        policy(),
        CancellationToken::new(),
        None,
    )
    .await
    .unwrap();
    assert!(estimate_messages_tokens(&active) < estimate_messages_tokens(&messages));
    assert!(estimate_messages_tokens(&messages) > 10_000);
    let captured = Arc::new(Mutex::new(None));
    let guard = ContextGuardProvider::new(Arc::new(CaptureProvider(captured.clone())), "model");
    let _stream = guard
        .stream_completion(
            CompletionRequest { messages: active, max_tokens: Some(512), ..Default::default() },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let request = captured.lock().unwrap().take().unwrap();
    let conversation = |entries: &[Message]| {
        entries
            .iter()
            .filter(|entry| matches!(entry.role, Role::User | Role::Assistant))
            .map(|entry| (entry.role.clone(), entry.content.clone()))
            .collect::<Vec<_>>()
    };
    assert_eq!(conversation(&request.messages), conversation(&messages));
    for dialect in [&AnthropicChatDialect as &dyn Dialect, &GeminiChatDialect] {
        let wire =
            dialect.render_chat_body(&request, "model", ReasoningEcho::IfPresent).to_string();
        assert!(
            wire.contains(&messages[0].content),
            "system policy missing on {} wire",
            dialect.kind()
        );
        for (_, content) in conversation(&messages) {
            assert!(
                wire.contains(&content),
                "conversation missing on {} wire: {content}",
                dialect.kind()
            );
        }
    }
    let calls: Vec<_> = request
        .messages
        .iter()
        .flat_map(|entry| entry.tool_calls.iter().flatten())
        .map(|call| call.id.as_str())
        .collect();
    let results: Vec<_> = request
        .messages
        .iter()
        .flat_map(|entry| entry.tool_results.iter().flatten())
        .map(|result| result.id.as_str())
        .collect();
    assert_eq!(calls, ["read-6", "read-7"]);
    assert_eq!(calls, results, "compacted calls and results must disappear together");
    let original_tail = &messages[messages.len() - 8..];
    let actual_tail = &request.messages[request.messages.len() - 8..];
    assert_eq!(
        serde_json::to_value(actual_tail).unwrap(),
        serde_json::to_value(original_tail).unwrap()
    );
    let persisted = store.load_messages(session, CancellationToken::new()).await.unwrap();
    assert_eq!(serde_json::to_value(&persisted).unwrap(), serde_json::to_value(&messages).unwrap());
    assert_eq!(persisted.iter().filter(|entry| entry.tool_results.is_some()).count(), 8);
}

#[test]
fn replay_regression_compaction_cannot_reintroduce_removed_calls() {
    let mut assistant = message(Role::Assistant, "retain this commitment");
    let call = concerto_core::types::ToolCall {
        id: "id".into(),
        name: "read".into(),
        arguments: serde_json::json!({}),
        thought_signature: None,
    };
    assistant.tool_calls = Some(vec![call.clone()]);
    assistant.gemini_replay = Some(
        concerto_core::provider_replay::GeminiReplay::capture(
            vec![serde_json::json!({"functionCall":{"name":"read","args":{}}})],
            &assistant.content,
            None,
            &[call],
        )
        .unwrap(),
    );
    let retained = retain_conversation(&assistant).unwrap();
    assert_eq!(retained.content, assistant.content);
    assert!(retained.tool_calls.is_none());
    assert!(retained.gemini_replay.is_none());
    assert!(assistant.gemini_replay.is_some(), "source transcript stays intact");
}
