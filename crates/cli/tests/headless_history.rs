//! Offline headless-provider regression for H40. The CLI uses an isolated
//! database/config and a loopback OpenAI endpoint with a test-only credential.
// Linux application-directory lookup honors the isolated XDG paths below.
// Other Unix platforms may resolve the real user's directories instead.
#![cfg(target_os = "linux")]

use concerto_core::types::Message;
use concerto_core::CancellationToken;
use concerto_sessions::{SessionStore, SqliteSessionStore};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const CONSTRAINT: &str =
    "Preserve the public API; compatibility verification is still outstanding.";
const OBLIGATION: &str = "The migration rollback test must pass before the work is complete.";
const PRIOR_REPLY: &str = "The earlier result is provisional and has not been verified.";

fn history_message(role: &str, content: String) -> Message {
    serde_json::from_value(json!({
        "role":role, "content":content, "tool_calls":null, "tool_results":null
    }))
    .unwrap()
}

async fn read_request(socket: &mut tokio::net::TcpStream) -> Value {
    let mut bytes = Vec::new();
    let mut scratch = [0_u8; 8192];
    let header_end = loop {
        let count = socket.read(&mut scratch).await.unwrap();
        assert!(count > 0, "provider connection closed before headers");
        bytes.extend_from_slice(&scratch[..count]);
        assert!(bytes.len() <= 1024 * 1024, "fixture request exceeded its bound");
        if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break end + 4;
        }
    };
    let headers = std::str::from_utf8(&bytes[..header_end]).unwrap();
    assert!(headers.starts_with("POST /v1/chat/completions HTTP/1.1\r\n"));
    let length: usize = headers
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .map(|(_, value)| value.trim().parse().unwrap())
        .expect("JSON request has a content length");
    assert!(length <= 1024 * 1024 - header_end);
    while bytes.len() < header_end + length {
        let count = socket.read(&mut scratch).await.unwrap();
        assert!(count > 0, "provider connection closed before body");
        bytes.extend_from_slice(&scratch[..count]);
        assert!(bytes.len() <= 1024 * 1024, "fixture request exceeded its bound");
    }
    serde_json::from_slice(&bytes[header_end..header_end + length]).unwrap()
}

async fn write_response(socket: &mut tokio::net::TcpStream, request: &Value) {
    // The coordinator's weak-model transport sends `stream: false`, so the
    // fixture must answer in the same shape the request asked for. Serving only
    // SSE here makes the provider reject the body before history is observable.
    const STREAM_BODY: &str = concat!(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Status reviewed.\"},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":3}}\n\n",
        "data: [DONE]\n\n"
    );
    const JSON_BODY: &str = concat!(
        "{\"id\":\"chatcmpl-h40\",\"object\":\"chat.completion\",",
        "\"choices\":[{\"index\":0,\"message\":{\"role\":\"assistant\",\"content\":\"Status reviewed.\"},",
        "\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":3,\"total_tokens\":13}}"
    );
    let stream = request["stream"].as_bool().unwrap_or(false);
    let (content_type, body) =
        if stream { ("text/event-stream", STREAM_BODY) } else { ("application/json", JSON_BODY) };
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    socket.write_all(response.as_bytes()).await.unwrap();
}

async fn capture_headless_request() -> Value {
    let fixture = tempfile::tempdir().unwrap();
    let project = fixture.path().join("project");
    let config = fixture.path().join("config/concerto");
    let data = fixture.path().join("data/concerto");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(&config).unwrap();
    std::fs::create_dir_all(&data).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let key_name = format!("history-fixture-{}/api_key", ulid::Ulid::new());
    let config_text = format!(
        r#"schema_version = 8
[model_settings]
global_default_model = "gpt-4o"
[[model_settings.providers]]
id = "history-local"
name = "History fixture"
provider = "openai"
model = "gpt-4o"
api_base = "http://{address}/v1"
keyring_key = "{key_name}"
timeout_seconds = 15
[memory]
enabled = false
[project_context]
enabled = false
[retry]
max_attempts = 1
"#
    );
    std::fs::write(config.join("config.toml"), config_text).unwrap();

    // H39 makes explicit plaintext-path connections public. This fixture is
    // prepared on H40 and runs after the verified H39 milestone is integrated.
    let store = SqliteSessionStore::connect_path(&data.join("sessions.db")).await.unwrap();
    let project_path = camino::Utf8Path::from_path(&project).unwrap();
    let session = store
        .create_session(project_path, "openai", "gpt-4o", CancellationToken::new())
        .await
        .unwrap();
    let mut history = vec![
        history_message("User", OBLIGATION.into()),
        history_message("User", format!("{}\n{CONSTRAINT}", "background ".repeat(600))),
    ];
    history.extend((0..6).map(|turn| {
        history_message("User", format!("Context turn {turn}: {}", "context ".repeat(500)))
    }));
    history.push(history_message("Assistant", PRIOR_REPLY.into()));
    store.append_messages(session.id, &history, CancellationToken::new()).await.unwrap();
    store
        .set_active_session_for_project(project_path, session.id, CancellationToken::new())
        .await
        .unwrap();
    drop(store);

    let (request_tx, request_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let mut request_tx = Some(request_tx);
        while let Ok((mut socket, _)) = listener.accept().await {
            let request = read_request(&mut socket).await;
            if let Some(tx) = request_tx.take() {
                let _ = tx.send(request.clone());
            }
            write_response(&mut socket, &request).await;
        }
    });
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_concerto-cli"));
    command
        .env("XDG_CONFIG_HOME", fixture.path().join("config"))
        .env("XDG_DATA_HOME", fixture.path().join("data"))
        .env("XDG_CACHE_HOME", fixture.path().join("cache"))
        .env("XDG_STATE_HOME", fixture.path().join("state"))
        .env("OPENAI_API_KEY", "concerto-loopback-test-only")
        .env("NO_PROXY", "127.0.0.1,localhost")
        .env("no_proxy", "127.0.0.1,localhost")
        .env_remove("CONCERTO_PROJECT_ROOTS")
        .args(["run", "--project", project.to_str().unwrap()])
        .args([
            "Explain the current project status without changing files.",
            "--json",
            "--multi-agent",
            "--timeout",
            "30",
            "--approvals",
            "deny",
        ])
        .kill_on_drop(true);
    let child = async {
        let output = command.output().await.map_err(|error| error.to_string())?;
        if !output.status.success() {
            return Err(format!(
                "headless run failed (status {}): stdout={} stderr={}",
                output.status,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        Ok(output)
    };
    let output = tokio::time::timeout(Duration::from_secs(45), child)
        .await
        .expect("headless fixture must finish within its process bound")
        .expect("headless fixture must complete successfully");
    server.abort();
    let request = request_rx.await.expect("headless fixture must capture a provider request");
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["status"], "done");
    assert_eq!(report["mode"], "multi");
    assert_eq!(report["session_id"], session.id.to_string());
    request
}

/// Compact per-message view of the wire request, used only when the regression
/// assertion fails. Shows role, length and a head slice so clipping is visible
/// without dumping the whole conversation.
fn history_summary(messages: &[Value]) -> String {
    messages
        .iter()
        .map(|message| {
            let role = message["role"].as_str().unwrap_or("?");
            let content = message["content"].as_str().unwrap_or("");
            let head: String = content.chars().take(100).collect();
            format!("{role}[{}]: {}", content.len(), head.replace('\n', " "))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn headless_provider_request_retains_long_and_older_obligations_with_roles() {
    let request = capture_headless_request().await;
    let messages = request["messages"].as_array().unwrap();
    for required in [CONSTRAINT, OBLIGATION] {
        assert!(
            messages.iter().any(|message| message["role"] == "user"
                && message["content"].as_str().is_some_and(|text| text.contains(required))),
            "headless provider request lost a durable user constraint: {required}\n{}",
            history_summary(messages)
        );
    }
    assert_eq!(
        messages
            .iter()
            .filter(|message| message["role"] == "assistant" && message["content"] == PRIOR_REPLY)
            .count(),
        1,
        "prior assistant text must retain its role exactly once"
    );
    assert!(
        !messages.iter().any(|message| message["role"] == "user"
            && message["content"]
                .as_str()
                .is_some_and(|text| text.contains("<conversation_history>"))),
        "typed history must not also be duplicated inside the submitted task"
    );
}
