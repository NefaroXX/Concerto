//! Stdout reader loop for one MCP stdio server.
//!
//! [`reader_task`] consumes newline-delimited JSON-RPC messages until EOF,
//! answering server→client requests (e.g. `ping`) through a `Weak` stdin
//! handle and resolving pending client requests by response id. On EOF or a
//! read failure it flips `server_died`, records the failure detail for
//! [`McpClient::last_failure_detail`](crate::client::McpClient::last_failure_detail),
//! and fails every still-pending request with [`McpError::ServerExited`], so a
//! crashed server surfaces as an error instead of a hung caller. A response
//! for an id that is no longer registered (its request already timed out or
//! was cancelled) is logged and ignored.
//!
//! The loop owns no client state: [`crate::client::McpClient::spawn`] passes
//! every shared handle (pending map, lifecycle flags, `Weak` child/stdin
//! handles) as a parameter, and keeps the request deadlines, stop sequence and
//! `Drop` reap in [`crate::client`].

use super::Child;
use super::{fail_all_pending, terminate_process_id, terminate_shared_tree, PendingMap};
use crate::error::McpError;
use crate::transport;
use concerto_core::McpServerState;
use serde_json::Value;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;
use tokio::io::BufReader;
use tokio::process::{ChildStdin, ChildStdout};
use tokio::sync::{watch, Mutex};
use tokio::time::timeout;

/// Bounded poll for the crashed server's exit status: EOF is observed the
/// moment the child's stdout pipe closes, which can precede the process
/// becoming a zombie, so `try_wait()` is retried briefly before giving up.
const CHILD_STATUS_POLLS: usize = 20;
const CHILD_STATUS_POLL_INTERVAL: Duration = Duration::from_millis(5);

/// Reader task: consumes server stdout until EOF, dispatching server
/// requests and resolving pending requests by id. On EOF/error the server's
/// lifecycle state is flipped to `Failed` (with the exit detail) so the
/// `McpManager` watcher can publish the crash event.
#[allow(clippy::too_many_arguments)]
pub(super) async fn reader_task(
    mut stdout: ChildStdout,
    pending: Arc<std::sync::Mutex<PendingMap>>,
    server_died: Arc<AtomicBool>,
    stopping: Arc<AtomicBool>,
    child_weak: Weak<Mutex<Option<Child>>>,
    stdin_weak: Weak<Mutex<ChildStdin>>,
    server_id: String,
    state_tx: watch::Sender<McpServerState>,
    last_failure: Arc<std::sync::Mutex<Option<String>>>,
    process_id: Option<u32>,
) {
    let mut reader = BufReader::new(&mut stdout);
    let mut buf: Vec<u8> = Vec::new();
    loop {
        match transport::read_message(&mut reader, &mut buf).await {
            Ok(Some(message)) => {
                if transport::is_server_request(&message) {
                    if !handle_server_request(&stdin_weak, &message, &server_id).await {
                        server_died.store(true, Ordering::SeqCst);
                        terminate_process_id(process_id);
                        terminate_shared_tree(&child_weak.upgrade());
                        record_reader_failure(
                            &last_failure,
                            &state_tx,
                            &server_id,
                            &None,
                            "server request reply timed out or failed",
                        );
                        fail_all_pending(&pending, || McpError::NotConnected).await;
                        break;
                    }
                } else if transport::is_response(&message) {
                    if let Some(id) = transport::response_id(&message) {
                        if let Some(tx) =
                            pending.lock().unwrap_or_else(|e| e.into_inner()).remove(&id)
                        {
                            let _ = tx.send(transport::extract_result(&message));
                        } else {
                            tracing::warn!(server = %server_id, id, "response for unknown request id; ignoring");
                        }
                    }
                } else if transport::is_notification(&message) {
                    // id-less notifications (initialized, cancelled, ...) are
                    // acknowledged implicitly; nothing to do.
                }
                // Notifications (method, no id) are ignored silently.
            }
            Ok(None) => {
                server_died.store(true, Ordering::SeqCst);
                let status = child_exit_code(&child_weak).await;
                terminate_process_id(process_id);
                terminate_shared_tree(&child_weak.upgrade());
                if !stopping.load(Ordering::SeqCst) {
                    record_reader_failure(
                        &last_failure,
                        &state_tx,
                        &server_id,
                        &status,
                        "server closed its output (EOF)",
                    );
                }
                fail_all_pending(&pending, || McpError::ServerExited {
                    status,
                    detail: "server closed its output (EOF)".into(),
                })
                .await;
                tracing::info!(server = %server_id, "server closed stdout; connection ended");
                break;
            }
            Err(e) => {
                server_died.store(true, Ordering::SeqCst);
                tracing::error!(server = %server_id, error = %e, "reader failure; disconnecting");
                let status = child_exit_code(&child_weak).await;
                terminate_process_id(process_id);
                terminate_shared_tree(&child_weak.upgrade());
                if !stopping.load(Ordering::SeqCst) {
                    record_reader_failure(
                        &last_failure,
                        &state_tx,
                        &server_id,
                        &status,
                        &e.to_string(),
                    );
                }
                fail_all_pending(&pending, || McpError::ServerExited {
                    status,
                    detail: e.to_string(),
                })
                .await;
                break;
            }
        }
    }
}

/// Record the reader-observed failure detail and flip the server state to
/// `Failed` so the manager watcher publishes the crash event. The state send
/// is a no-op if it is already `Failed` (e.g. the manager recorded a
/// registration failure first), but the detail is always refreshed.
fn record_reader_failure(
    last_failure: &Arc<std::sync::Mutex<Option<String>>>,
    state_tx: &watch::Sender<McpServerState>,
    server_id: &str,
    status: &Option<i32>,
    detail: &str,
) {
    let detail = match status {
        Some(code) => format!("mcp server '{server_id}' exited with status {code}: {detail}"),
        None => format!("mcp server '{server_id}' exited: {detail}"),
    };
    *last_failure.lock().unwrap_or_else(|error| error.into_inner()) = Some(detail);
    let _ = state_tx.send(McpServerState::Failed);
}

/// Best-effort exit code of the server child once the reader observed
/// EOF/error. `try_wait()` reaps a zombie without blocking; the process may
/// still be transitioning to a zombie when EOF is first observed, so the
/// status is polled briefly. Returns `None` if the process is still alive
/// (e.g. it closed stdout deliberately) or the shared handle is gone (`stop`
/// took it).
async fn child_exit_code(child_weak: &Weak<Mutex<Option<Child>>>) -> Option<i32> {
    let child = child_weak.upgrade()?;
    for _ in 0..CHILD_STATUS_POLLS {
        let status = child.lock().await.as_mut().and_then(|c| c.try_wait().ok()).flatten();
        if let Some(status) = status {
            return status.code();
        }
        tokio::time::sleep(CHILD_STATUS_POLL_INTERVAL).await;
    }
    None
}

/// Handle a server→client request: reply to `ping` with an empty result and
/// log-and-ignore other methods. Replies are written through a `Weak` handle
/// so they never keep the stdin pipe open past `stop`.
async fn handle_server_request(
    stdin_weak: &Weak<Mutex<ChildStdin>>,
    message: &Value,
    server_id: &str,
) -> bool {
    let method = message.get("method").and_then(Value::as_str).unwrap_or_default();
    let Some(reply) = transport::ping_reply(message) else { return true };
    if method != "ping" {
        tracing::warn!(server = %server_id, method, "ignoring unsupported server request");
        return true;
    }
    let Some(stdin) = stdin_weak.upgrade() else { return false };
    matches!(
        timeout(Duration::from_millis(100), async {
            let mut stdin = stdin.lock().await;
            transport::write_message(&mut *stdin, &reply).await
        })
        .await,
        Ok(Ok(()))
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::Instant;
    use tokio::sync::oneshot;

    /// Pending map holding one `(id, sender)` slot and its receiver.
    fn pending_with(id: u64) -> (PendingMap, oneshot::Receiver<Result<Value, McpError>>) {
        let (tx, rx) = oneshot::channel();
        let mut pending = PendingMap::new();
        pending.insert(id, tx);
        (pending, rx)
    }

    fn failure_detail_of(last_failure: &Arc<std::sync::Mutex<Option<String>>>) -> Option<String> {
        last_failure.lock().unwrap_or_else(|error| error.into_inner()).clone()
    }

    #[test]
    fn record_failure_includes_exit_status_in_detail() {
        let (state_tx, mut state_rx) = watch::channel(McpServerState::Connecting);
        let last_failure = Arc::new(std::sync::Mutex::new(None));
        record_reader_failure(&last_failure, &state_tx, "srv", &Some(3), "boom");
        assert_eq!(
            failure_detail_of(&last_failure).as_deref(),
            Some("mcp server 'srv' exited with status 3: boom")
        );
        assert_eq!(*state_rx.borrow_and_update(), McpServerState::Failed);
    }

    #[test]
    fn record_failure_refreshes_detail_when_already_failed() {
        let (state_tx, state_rx) = watch::channel(McpServerState::Failed);
        let last_failure = Arc::new(std::sync::Mutex::new(Some("manager detail".into())));
        record_reader_failure(
            &last_failure,
            &state_tx,
            "srv",
            &None,
            "server closed its output (EOF)",
        );
        assert_eq!(
            failure_detail_of(&last_failure).as_deref(),
            Some("mcp server 'srv' exited: server closed its output (EOF)")
        );
        assert_eq!(*state_rx.borrow(), McpServerState::Failed);
    }

    #[tokio::test]
    async fn handle_server_request_ignores_message_without_id() {
        let stdin = Weak::<Mutex<ChildStdin>>::new();
        let message = json!({ "jsonrpc": "2.0", "method": "notifications/progress" });
        assert!(handle_server_request(&stdin, &message, "srv").await);
    }

    #[tokio::test]
    async fn handle_server_request_ignores_unsupported_method() {
        let stdin = Weak::<Mutex<ChildStdin>>::new();
        let message = json!({ "jsonrpc": "2.0", "id": 3, "method": "resources/list" });
        assert!(handle_server_request(&stdin, &message, "srv").await);
    }

    #[tokio::test]
    async fn handle_server_request_fails_when_stdin_gone() {
        let stdin = Weak::<Mutex<ChildStdin>>::new();
        let message = json!({ "jsonrpc": "2.0", "id": 4, "method": "ping" });
        assert!(!handle_server_request(&stdin, &message, "srv").await);
    }

    #[tokio::test]
    async fn child_exit_code_returns_none_without_child_handle() {
        let weak = Weak::<Mutex<Option<Child>>>::new();
        assert_eq!(child_exit_code(&weak).await, None);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn child_exit_code_gives_up_after_bounded_polls() {
        let mut cmd = tokio::process::Command::new("sleep");
        cmd.arg("30").kill_on_drop(true);
        let child = cmd.spawn().expect("spawn sleeper");
        let shared = Arc::new(Mutex::new(Some(child)));
        let started = Instant::now();
        assert_eq!(child_exit_code(&Arc::downgrade(&shared)).await, None);
        let elapsed = started.elapsed();
        assert!(
            elapsed >= Duration::from_millis(80) && elapsed < Duration::from_secs(1),
            "exit-status poll must run its ~100ms budget, took {elapsed:?}"
        );
        // Dropping `shared` kills the sleeper via `kill_on_drop`.
    }

    /// Shared handles one scripted [`reader_task`] run acted on. The child
    /// itself is not retained: every script exits on its own and `kill_on_drop`
    /// reaps a straggler when the harness drops the last strong handle.
    #[cfg(unix)]
    struct Harness {
        server_died: Arc<AtomicBool>,
        pending: Arc<std::sync::Mutex<PendingMap>>,
        last_failure: Arc<std::sync::Mutex<Option<String>>>,
        state: watch::Receiver<McpServerState>,
    }

    /// Run [`reader_task`] over the stdout of a `sh` producer emitting
    /// `script`, wired exactly as [`crate::client::McpClient::spawn`] wires it
    /// (no `process_id`: the harness owns no process-group semantics).
    #[cfg(unix)]
    async fn drive_reader(script: &str, pending: PendingMap, stopping: bool) -> Harness {
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c")
            .arg(script)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        cmd.process_group(0);
        let mut child = cmd.spawn().expect("spawn sh producer");
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let stdin_shared = Arc::new(Mutex::new(stdin));
        let child_shared = Arc::new(Mutex::new(Some(child)));
        let pending = Arc::new(std::sync::Mutex::new(pending));
        let server_died = Arc::new(AtomicBool::new(false));
        let stopping_flag = Arc::new(AtomicBool::new(stopping));
        let last_failure = Arc::new(std::sync::Mutex::new(None));
        let (state_tx, state_rx) = watch::channel(McpServerState::Connecting);
        timeout(
            Duration::from_secs(10),
            reader_task(
                stdout,
                pending.clone(),
                server_died.clone(),
                stopping_flag.clone(),
                Arc::downgrade(&child_shared),
                Arc::downgrade(&stdin_shared),
                "fixture".to_string(),
                state_tx,
                last_failure.clone(),
                None,
            ),
        )
        .await
        .expect("reader must finish");
        Harness { server_died, pending, last_failure, state: state_rx }
    }

    /// Registered ids resolve; a response for an id that was never registered
    /// (or whose request already timed out and left the map) is ignored.
    #[cfg(unix)]
    #[tokio::test]
    async fn reader_resolves_registered_response_and_ignores_unregistered_id() {
        let (pending, rx) = pending_with(7);
        let harness = drive_reader(
            r#"printf '%s\n' '{"jsonrpc":"2.0","id":7,"result":{"ok":true}}' '{"jsonrpc":"2.0","id":999,"result":{}}'"#,
            pending,
            false,
        )
        .await;
        let outcome = rx.await.expect("registered sender survives").expect("response resolves");
        assert_eq!(outcome, json!({ "ok": true }));
        assert!(harness.pending.lock().unwrap_or_else(|e| e.into_inner()).is_empty());
        assert!(harness.server_died.load(Ordering::SeqCst));
        let detail = failure_detail_of(&harness.last_failure).unwrap_or_default();
        assert!(detail.contains("server closed its output (EOF)"));
        assert_eq!(*harness.state.borrow(), McpServerState::Failed);
    }

    /// An empty stream is a clean EOF: the connection fails with the exit
    /// detail rather than hanging the pending caller.
    #[cfg(unix)]
    #[tokio::test]
    async fn reader_fails_pending_on_empty_stream_eof() {
        let (pending, rx) = pending_with(3);
        let harness = drive_reader("true", pending, false).await;
        let error =
            rx.await.expect("registered sender survives").expect_err("pending must fail on EOF");
        match error {
            McpError::ServerExited { detail, .. } => {
                assert!(detail.contains("server closed its output (EOF)"));
            }
            other => panic!("expected ServerExited, got {other:?}"),
        }
        assert!(harness.server_died.load(Ordering::SeqCst));
    }

    /// A malformed line terminates the loop and fails every pending request
    /// with the parser's detail.
    #[cfg(unix)]
    #[tokio::test]
    async fn reader_surfaces_malformed_line_to_pending_request() {
        let (pending, rx) = pending_with(3);
        let harness = drive_reader(r#"printf 'not-json\n'"#, pending, false).await;
        let error = rx
            .await
            .expect("registered sender survives")
            .expect_err("pending must fail on malformed input");
        match error {
            McpError::ServerExited { detail, .. } => {
                assert!(detail.contains("malformed JSON-RPC message"));
            }
            other => panic!("expected ServerExited, got {other:?}"),
        }
        let detail = failure_detail_of(&harness.last_failure).unwrap_or_default();
        assert!(detail.contains("malformed JSON-RPC message"));
    }

    /// A server `ping` is answered on stdin; the script validates the exact
    /// reply bytes before emitting a normal response, so resolving id 8 proves
    /// the reply write succeeded (a failed write would take the reader's
    /// reply-failure branch instead).
    #[cfg(unix)]
    #[tokio::test]
    async fn reader_replies_to_server_ping_before_eof() {
        let (pending, rx) = pending_with(8);
        let harness = drive_reader(
            r#"
printf '%s\n' '{"jsonrpc":"2.0","id":5,"method":"ping"}'
IFS= read -r line
case "$line" in
  *'"id":5'*'"result":{}'* | *'"result":{}'*'"id":5'*)
    printf '%s\n' '{"jsonrpc":"2.0","id":8,"result":{"validated":true}}'
    ;;
esac
"#,
            pending,
            false,
        )
        .await;
        let outcome =
            rx.await.expect("registered sender survives").expect("child saw the exact reply");
        assert_eq!(outcome, json!({ "validated": true }));
        let detail = failure_detail_of(&harness.last_failure).unwrap_or_default();
        assert!(detail.contains("server closed its output (EOF)"));
    }

    /// EOF observed while `stop` is in flight must not be recorded as a crash.
    #[cfg(unix)]
    #[tokio::test]
    async fn reader_skips_failure_record_while_stopping() {
        let harness = drive_reader("true", PendingMap::new(), true).await;
        assert!(
            failure_detail_of(&harness.last_failure).is_none(),
            "a graceful stop must not be recorded as a crash"
        );
        assert_eq!(*harness.state.borrow(), McpServerState::Connecting);
        assert!(harness.server_died.load(Ordering::SeqCst));
    }
}
