use super::*;
use concerto_core::provider_replay::GeminiReplay;
use concerto_core::types::CompletionChunk;
use concerto_providers::adapters::{Dialect, GeminiChatDialect, ReasoningEcho};
use concerto_sessions::{SessionStore, SqliteSessionStore};
use futures::stream;
use serde_json::json;

#[tokio::test]
async fn replay_regression_collectors_preserve_terminal_state_through_storage_and_rendering() {
    let parts = vec![
        json!({"text":"answer", "thoughtSignature":"signed-text"}),
        json!({"text":"", "thoughtSignature":"last"}),
    ];
    let replay = GeminiReplay::capture(parts.clone(), "answer", None, &[]).unwrap();
    let chunks = vec![
        CompletionChunk { delta: "answer".into(), ..CompletionChunk::keepalive() },
        CompletionChunk {
            is_final: true,
            gemini_replay: Some(replay),
            ..CompletionChunk::keepalive()
        },
    ];
    for timed in [false, true] {
        let stream: CompletionStream = Box::pin(stream::iter(chunks.clone().into_iter().map(Ok)));
        let turn = if timed {
            collect_stream_with_timeouts(
                stream,
                &CancellationToken::new(),
                Duration::from_secs(1),
                Duration::from_secs(1),
            )
            .await
            .unwrap()
        } else {
            collect_stream(stream).await.unwrap()
        };
        let message = Message {
            role: Role::Assistant,
            content: turn.text,
            reasoning_content: turn.reasoning,
            tool_calls: (!turn.tool_calls.is_empty()).then_some(turn.tool_calls),
            tool_results: None,
            gemini_replay: turn.gemini_replay,
            tokens_in: None,
            tokens_out: None,
        };
        let store = SqliteSessionStore::connect_in_memory().await.unwrap();
        let session = store
            .create_session("/project".into(), "google", "gemini", CancellationToken::new())
            .await
            .unwrap();
        store.append_messages(session.id, &[message], CancellationToken::new()).await.unwrap();
        let messages = store.load_messages(session.id, CancellationToken::new()).await.unwrap();
        let body = GeminiChatDialect.render_chat_body(
            &CompletionRequest { messages, ..Default::default() },
            "gemini",
            ReasoningEcho::IfPresent,
        );
        assert_eq!(body["contents"][0]["parts"], json!(parts));
    }
}

#[tokio::test]
async fn replay_regression_collectors_discard_incomplete_or_failed_attempts() {
    let replay =
        GeminiReplay::capture(vec![json!({"text":"partial"})], "partial", None, &[]).unwrap();
    for final_seen in [false, true] {
        for timed in [false, true] {
            let chunks = vec![
                Ok(CompletionChunk {
                    delta: "partial".into(),
                    gemini_replay: Some(replay.clone()),
                    is_final: final_seen,
                    ..CompletionChunk::keepalive()
                }),
                Err(ProviderError::StreamTransport("connection lost".into())),
            ];
            let stream: CompletionStream = Box::pin(stream::iter(chunks));
            if timed {
                assert!(collect_stream_with_timeouts(
                    stream,
                    &CancellationToken::new(),
                    Duration::from_secs(1),
                    Duration::from_secs(1)
                )
                .await
                .is_err());
            } else {
                assert!(collect_stream(stream).await.is_err());
            }
        }
    }
}
