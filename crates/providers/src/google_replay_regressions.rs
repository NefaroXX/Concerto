//! Native replay distinctions must survive the stream decoder before any
//! collector or session store can retain them.

use super::*;
use futures::{stream, TryStreamExt};
use serde_json::{json, Value};

async fn decode(parts: Value) -> Vec<CompletionChunk> {
    let body = json!({
        "candidates": [{"content": {"role": "model", "parts": parts}, "finishReason": "STOP"}]
    });
    let wire = format!("data: {body}\n\ndata: [DONE]\n\n");
    decode_google_stream(stream::iter([Ok(wire.into_bytes())]), false, CancellationToken::new())
        .try_collect()
        .await
        .unwrap()
}

fn call() -> Value {
    json!({
        "functionCall": {"name": "read", "args": {"path": "source.rs"}},
        "thoughtSignature": "opaque-call-signature"
    })
}

#[tokio::test]
async fn replay_regression_signed_text_remains_distinguishable_from_unsigned_text() {
    let signed = decode(json!([{
        "text": "Here is the answer.", "thoughtSignature": "opaque-text-signature"
    }]))
    .await;
    let unsigned = decode(json!([{"text": "Here is the answer."}])).await;
    assert!(signed.last().unwrap().is_final);
    assert!(unsigned.last().unwrap().is_final);
    assert_ne!(
        serde_json::to_value(&signed).unwrap(),
        serde_json::to_value(&unsigned).unwrap(),
        "decoder erased the only signature that must be echoed on the next turn"
    );
}

#[tokio::test]
async fn replay_regression_native_part_order_remains_recoverable() {
    let text = json!({"text": "Inspect the result before continuing."});
    let text_first = decode(json!([text.clone(), call()])).await;
    let call_first = decode(json!([call(), text])).await;
    assert!(text_first.last().unwrap().is_final);
    assert!(call_first.last().unwrap().is_final);
    // Buffering executable calls until STOP is still required. Native replay
    // order must be retained separately, not recovered from emission order.
    assert_ne!(
        serde_json::to_value(&text_first).unwrap(),
        serde_json::to_value(&call_first).unwrap(),
        "distinct native histories collapsed to identical canonical streams"
    );
}

#[tokio::test]
async fn replay_regression_thought_text_is_reasoning_not_visible_answer() {
    let chunks = decode(json!([
        {"text": "Internal reasoning.", "thought": true, "thoughtSignature": "opaque-thought"},
        {"text": "Visible answer."}
    ]))
    .await;
    let visible: String = chunks.iter().map(|chunk| chunk.delta.as_str()).collect();
    let reasoning: String = chunks.iter().filter_map(|chunk| chunk.reasoning.as_deref()).collect();
    assert_eq!(visible, "Visible answer.");
    assert_eq!(reasoning, "Internal reasoning.");
}

fn assembled(chunks: Vec<CompletionChunk>) -> concerto_core::types::Message {
    let mut message = concerto_core::types::Message {
        role: concerto_core::types::Role::Assistant,
        content: String::new(),
        tool_calls: None,
        tool_results: None,
        reasoning_content: None,
        tokens_in: None,
        tokens_out: None,
        gemini_replay: None,
    };
    for chunk in chunks {
        message.content.push_str(&chunk.delta);
        if let Some(reasoning) = chunk.reasoning {
            message.reasoning_content.get_or_insert_with(String::new).push_str(&reasoning);
        }
        if let Some(call) = chunk.tool_call {
            message.tool_calls.get_or_insert_with(Vec::new).push(call);
        }
        if chunk.is_final {
            message.gemini_replay = chunk.gemini_replay;
        }
    }
    message
}

fn render(messages: Vec<concerto_core::types::Message>) -> Value {
    GeminiChatDialect.render_chat_body(
        &CompletionRequest { messages, ..Default::default() },
        "gemini",
        ReasoningEcho::IfPresent,
    )
}

#[tokio::test]
async fn replay_regression_preserves_parts_and_native_call_ids_through_storage_and_rendering() {
    use concerto_core::types::{Message, Role, ToolResult};
    use concerto_sessions::{SessionStore, SqliteSessionStore};
    let parts = json!([
        {"functionCall": {"id": "native-42", "name": "read", "args": {"path": "a.rs"}}, "thoughtSignature": "first"},
        {"text": "Keep this separate", "thoughtSignature": "second"},
        {"functionCall": {"id": "native-43", "name": "read", "args": {"path": "b.rs"}}},
        {"text": "", "thoughtSignature": "final-empty"}
    ]);
    let assistant = assembled(decode(parts.clone()).await);
    assert_eq!(assistant.tool_calls.as_ref().unwrap()[0].id, "native-42");
    let store = SqliteSessionStore::connect_in_memory().await.unwrap();
    let session = store
        .create_session(
            std::env::current_dir().unwrap().to_str().unwrap().into(),
            "google",
            "gemini",
            CancellationToken::new(),
        )
        .await
        .unwrap();
    store.append_messages(session.id, &[assistant], CancellationToken::new()).await.unwrap();
    let mut messages = store.load_messages(session.id, CancellationToken::new()).await.unwrap();
    messages.push(Message {
        role: Role::Tool,
        content: String::new(),
        tool_calls: None,
        tool_results: Some(vec![ToolResult {
            id: "native-43".into(),
            name: "read".into(),
            content: json!({"ok": true}),
        }]),
        reasoning_content: None,
        tokens_in: None,
        tokens_out: None,
        gemini_replay: None,
    });
    let wire = render(messages);
    assert_eq!(wire["contents"][0]["parts"], parts);
    assert_eq!(wire["contents"][1]["parts"][0]["functionResponse"]["id"], "native-43");
}

#[tokio::test]
async fn replay_regression_preserves_empty_signature_chunks_after_visible_text() {
    let parts = json!([{"text": "answer"}, {"text": "", "thoughtSignature": "trailer"}]);
    let events = parts
        .as_array()
        .unwrap()
        .iter()
        .map(|part| format!("data: {}\n\n", json!({"candidates":[{"content":{"parts":[part]}}]})))
        .collect::<String>();
    let wire = format!("{events}data: {{\"candidates\":[{{\"finishReason\":\"STOP\"}}]}}\n\n");
    let chunks = decode_google_stream(
        stream::iter([Ok(wire.into_bytes())]),
        false,
        CancellationToken::new(),
    )
    .try_collect()
    .await
    .unwrap();
    assert_eq!(render(vec![assembled(chunks)])["contents"][0]["parts"], parts);
}

#[tokio::test]
async fn replay_regression_incomplete_response_never_publishes_replay_or_calls() {
    let wire = format!("data: {}\n\n", json!({"candidates":[{"content":{"parts":[call()]}}]}));
    let chunks = decode_google_stream(
        stream::iter([Ok(wire.into_bytes())]),
        false,
        CancellationToken::new(),
    )
    .collect::<Vec<_>>()
    .await;
    assert!(chunks.iter().any(Result::is_err));
    assert!(chunks
        .iter()
        .filter_map(|c| c.as_ref().ok())
        .all(|c| c.gemini_replay.is_none() && c.tool_call.is_none() && !c.is_final));
}

#[tokio::test]
async fn replay_regression_capture_limits_fail_without_publishing_a_partial_turn() {
    for parts in [
        vec![json!({"text":""}); MAX_GEMINI_REPLAY_PARTS + 1],
        vec![json!({"text":"a".repeat(MAX_GEMINI_REPLAY_BYTES)})],
    ] {
        let wire = format!(
            "data: {}\n\ndata: [DONE]\n\n",
            json!({"candidates":[{"content":{"parts": parts}, "finishReason":"STOP"}]})
        );
        let chunks = decode_google_stream(
            stream::iter([Ok(wire.into_bytes())]),
            false,
            CancellationToken::new(),
        )
        .collect::<Vec<_>>()
        .await;
        assert!(chunks.iter().any(Result::is_err));
        assert!(chunks
            .iter()
            .filter_map(|c| c.as_ref().ok())
            .all(|c| c.gemini_replay.is_none() && !c.is_final));
    }
}

#[tokio::test]
async fn replay_regression_edited_content_cannot_be_overridden_by_native_replay() {
    let mut message =
        assembled(decode(json!([{"text":"original", "thoughtSignature":"signed"}])).await);
    message.content = "user-corrected".into();
    let body = render(vec![message]);
    assert_eq!(body["contents"][0]["parts"], json!([{"text":"user-corrected"}]));
}

#[tokio::test]
async fn replay_regression_loose_arguments_execute_nested_but_replay_original_parts() {
    let parts = json!([{"functionCall":{"name":"edit", "args":{"file.path":"a.rs"}}, "thoughtSignature":"opaque"}]);
    let wire = format!(
        "data: {}\n\ndata: [DONE]\n\n",
        json!({"candidates":[{"content":{"parts":parts}, "finishReason":"STOP"}]})
    );
    let chunks =
        decode_google_stream(stream::iter([Ok(wire.into_bytes())]), true, CancellationToken::new())
            .try_collect()
            .await
            .unwrap();
    let message = assembled(chunks);
    assert_eq!(message.tool_calls.as_ref().unwrap()[0].arguments, json!({"file":{"path":"a.rs"}}));
    assert_eq!(render(vec![message])["contents"][0]["parts"], parts);
}

#[tokio::test]
async fn replay_regression_cancellation_discards_buffered_native_parts_and_calls() {
    let cancel = CancellationToken::new();
    let wire = format!(
        "data: {}\n\n",
        json!({"candidates":[{"content":{"parts":[{"text":"partial", "thoughtSignature":"signed"}, call()]}}]})
    );
    let bytes = stream::iter([Ok(wire.into_bytes())]).chain(stream::pending());
    let mut chunks = decode_google_stream(bytes, false, cancel.clone());
    let first = chunks.next().await.unwrap().unwrap();
    assert_eq!(first.delta, "partial");
    assert!(first.gemini_replay.is_none());
    assert!(first.tool_call.is_none());
    cancel.cancel();
    assert!(matches!(chunks.next().await, Some(Err(ProviderError::Cancelled))));
    assert!(chunks.next().await.is_none());
}
