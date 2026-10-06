use concerto_core::error::ToolError;
use concerto_core::CancellationToken;
use lsp_types::{InitializeParams, WorkspaceFolder};
use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{oneshot, Mutex};
use tokio::time::{timeout, Duration};

type PendingMap = HashMap<u64, oneshot::Sender<Result<serde_json::Value, ToolError>>>;
type DiagMap = HashMap<String, Vec<serde_json::Value>>;

/// Bound on a single blocking read of the server's stdout (one header line or
/// the body chunk).
///
/// Cancellation is the prompt exit path — it is observed *during* a blocked
/// read, not only between messages. This timeout is the backstop for a server
/// that stalls mid-frame, or goes silent, while no cancellation ever comes,
/// so the reader task and its stdout pipe cannot be pinned open indefinitely.
/// It is deliberately much longer than the 30s request bound: `LspManager`
/// caches one client per project for the whole session and an exited reader
/// is never restarted, so a short bound would sever a healthy-but-quiet
/// server (rust-analyzer is silent between edits) and strand later requests.
/// Diagnostics already delivered live in the in-memory map; nothing needs to
/// be persisted when the reader exits.
const READER_READ_TIMEOUT: Duration = Duration::from_secs(300);

/// Bound on the best-effort `exit` notification write in [`LspClient::stop`]:
/// a server that stopped draining its stdin must not hold shutdown open.
const EXIT_WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// Bound on reaping the child after SIGKILL in [`LspClient::stop`].
const KILL_WAIT_TIMEOUT: Duration = Duration::from_secs(5);

/// Bound on the unix `kill(1)` helper used for process-group escalation.
#[cfg(unix)]
const GROUP_KILL_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug)]
pub struct LspClient {
    project_dir: PathBuf,
    server_cmd: String,
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    pending: Arc<Mutex<PendingMap>>,
    next_id: Arc<AtomicU64>,
    diagnostics: Arc<Mutex<DiagMap>>,
}

impl LspClient {
    pub fn new<P: AsRef<Path>>(project_dir: P, server_cmd: impl Into<String>) -> Self {
        Self {
            project_dir: project_dir.as_ref().to_path_buf(),
            server_cmd: server_cmd.into(),
            child: None,
            stdin: None,
            pending: Arc::new(Mutex::new(HashMap::new())),
            next_id: Arc::new(AtomicU64::new(1)),
            diagnostics: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    async fn write_message(&mut self, msg: &str) -> Result<(), ToolError> {
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| ToolError::LspError { message: "stdin not available".into() })?;
        stdin
            .write_all(msg.as_bytes())
            .await
            .map_err(|e| ToolError::LspError { message: e.to_string() })?;
        stdin.flush().await.map_err(|e| ToolError::LspError { message: e.to_string() })
    }

    pub async fn send_request(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, ToolError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.pending.lock().await;
            pending.insert(id, tx);
        }
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let body = serde_json::to_string(&request)
            .map_err(|e| ToolError::LspError { message: e.to_string() })?;
        let framed = format!("Content-Length: {}\r\n\r\n{}", body.len(), body);
        self.write_message(&framed).await?;
        match timeout(Duration::from_secs(30), rx).await {
            Ok(Ok(res)) => res,
            Ok(Err(_)) => Err(ToolError::LspError { message: "response channel closed".into() }),
            Err(_) => Err(ToolError::Timeout { timeout_secs: 30 }),
        }
    }

    pub async fn send_notification(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<(), ToolError> {
        let notification = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        let body = serde_json::to_string(&notification)
            .map_err(|e| ToolError::LspError { message: e.to_string() })?;
        let framed = format!("Content-Length: {}\r\n\r\n{}", body.len(), body);
        self.write_message(&framed).await
    }

    pub async fn start(&mut self, cancel: CancellationToken) -> Result<(), ToolError> {
        if cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        if self.child.is_some() {
            return Ok(());
        }
        let mut cmd = Command::new(&self.server_cmd);
        cmd.current_dir(&self.project_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            // Safety net: a client dropped without `stop` still kills the
            // server instead of orphaning it (mirrors concerto-mcp).
            .kill_on_drop(true);
        #[cfg(unix)]
        {
            // The child leads its own process group so `stop` can SIGKILL
            // every descendant it spawned, not just the direct child
            // (mirrors concerto-tools / concerto-mcp spawns).
            cmd.process_group(0);
        }
        let mut child = cmd.spawn().map_err(|e| ToolError::LspError {
            message: format!("Failed to spawn LSP server: {}", e),
        })?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| ToolError::LspError { message: "No stdout from LSP server".into() })?;
        self.stdin = child.stdin.take();
        self.child = Some(child);
        let pending = self.pending.clone();
        let diagnostics = self.diagnostics.clone();
        let cancel_for_loop = cancel.clone();
        tokio::spawn(run_reader(
            stdout,
            pending,
            diagnostics,
            cancel_for_loop,
            READER_READ_TIMEOUT,
        ));
        // Handshake
        let uri = lsp_types::Uri::from_str(&format!("file://{}", self.project_dir.display()))
            .map_err(|e| ToolError::LspError { message: e.to_string() })?;
        let init_params = InitializeParams {
            process_id: Some(std::process::id()),
            capabilities: Default::default(),
            workspace_folders: Some(vec![WorkspaceFolder {
                uri,
                name: self.project_dir.to_string_lossy().to_string(),
            }]),
            ..Default::default()
        };
        let _init_res = self
            .send_request(
                "initialize",
                serde_json::to_value(init_params)
                    .map_err(|e| ToolError::LspError { message: e.to_string() })?,
            )
            .await?;
        // ignore init_res
        self.send_notification("initialized", serde_json::json!({})).await?;
        Ok(())
    }

    /// Shut the server down with a bounded sequence: `shutdown` request
    /// (already capped by `send_request`'s 30s timeout), best-effort `exit`
    /// notification, then a process-group SIGKILL and a bounded reap.
    ///
    /// Returns [`ToolError::Cancelled`] immediately when `cancel` has already
    /// fired, so an already-cancelled stop never touches the process.
    pub async fn stop(&mut self, cancel: CancellationToken) -> Result<(), ToolError> {
        if cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        // `shutdown` is bounded by `send_request`'s existing 30s response
        // timeout: a hung server cannot hold stop() open on this line.
        let _ = self.send_request("shutdown", serde_json::json!({})).await;
        // `exit` is best-effort; bound the write too, for a server that has
        // stopped draining stdin (a full pipe would otherwise block here).
        let _ = timeout(EXIT_WRITE_TIMEOUT, self.send_notification("exit", serde_json::json!({})))
            .await;
        if let Some(mut child) = self.child.take() {
            // SIGKILL the whole process group led by the child, then the
            // direct child — the same escalation concerto-tools'
            // `kill_process_group` performs, so descendants that inherited
            // the stdout pipe cannot keep it (and the reader) open.
            kill_process_group(&mut child).await;
            // The reap is bounded as well: even SIGKILL cannot hurry an
            // unkillable process, and stop() must still return. Dropping the
            // child re-issues the kill through `kill_on_drop(true)` and
            // tokio's orphan reaper finishes the job if we time out here.
            let _ = timeout(KILL_WAIT_TIMEOUT, child.wait()).await;
        }
        Ok(())
    }

    pub async fn get_diagnostics(&self, file_path: &str) -> Vec<serde_json::Value> {
        let map = self.diagnostics.lock().await;
        map.get(file_path).cloned().unwrap_or_default()
    }
}

/// Read framed LSP messages from the server's stdout until the stream ends,
/// `cancel` fires, or a single read exceeds `read_timeout`.
///
/// Cancellation and the read bound are observed *during* a blocked read — not
/// only between messages — so a stalled server cannot pin this task and its
/// pipe open forever. Any of the three exits simply returns: diagnostics
/// already parsed live in the shared in-memory map, and in-flight requests
/// fail through `send_request`'s own 30s timeout.
async fn run_reader<R: AsyncRead + Unpin>(
    stdout: R,
    pending: Arc<Mutex<PendingMap>>,
    diagnostics: Arc<Mutex<DiagMap>>,
    cancel: CancellationToken,
    read_timeout: Duration,
) {
    let mut reader = BufReader::new(stdout);
    let mut headers = String::new();
    loop {
        if cancel.is_cancelled() {
            return;
        }
        headers.clear();
        loop {
            let mut line = String::new();
            let Some(read) = read_bounded(&cancel, read_timeout, reader.read_line(&mut line)).await
            else {
                return;
            };
            match read {
                Ok(0) => return,
                Ok(_) => {
                    if line == "\r\n" || line == "\n" {
                        break;
                    }
                    headers.push_str(&line);
                }
                Err(_) => return,
            }
        }
        let mut content_length: usize = 0;
        for header in headers.lines() {
            if let Some(val) = header.strip_prefix("Content-Length:") {
                if let Ok(len) = val.trim().parse() {
                    content_length = len;
                }
            }
        }
        if content_length == 0 {
            continue;
        }
        let mut body = vec![0u8; content_length];
        let Some(read) = read_bounded(&cancel, read_timeout, reader.read_exact(&mut body)).await
        else {
            return;
        };
        if read.is_err() {
            return;
        }
        if cancel.is_cancelled() {
            return;
        }
        let msg: serde_json::Value = match serde_json::from_slice(&body) {
            Ok(v) => v,
            Err(_) => continue,
        };
        // Handle response or notification
        if let Some(id) = msg.get("id").and_then(|i| i.as_u64()) {
            let sender_opt = {
                let mut pending = pending.lock().await;
                pending.remove(&id)
            };
            if let Some(sender) = sender_opt {
                let result = msg
                    .get("result")
                    .cloned()
                    .ok_or_else(|| ToolError::LspError { message: "Missing result".into() });
                let _ = sender.send(result);
            }
        } else if let Some(method) = msg.get("method").and_then(|m| m.as_str()) {
            if method == "textDocument/publishDiagnostics" {
                if let Some(params) = msg.get("params") {
                    if let (Some(uri), Some(diags)) = (params.get("uri"), params.get("diagnostics"))
                    {
                        if let Some(uri_str) = uri.as_str() {
                            let path = uri_str.trim_start_matches("file://");
                            let mut diags_map = diagnostics.lock().await;
                            diags_map.insert(
                                path.to_string(),
                                diags.as_array().cloned().unwrap_or_default(),
                            );
                        }
                    }
                }
            }
        }
    }
}

/// Await one read bounded by `cancel` and `read_timeout`.
///
/// Returns `None` when the token fires or the bound elapses — both end the
/// reader task — and `Some(result)` when the read itself completed (including
/// EOF and I/O errors, which the caller classifies).
async fn read_bounded<T>(
    cancel: &CancellationToken,
    read_timeout: Duration,
    read: impl Future<Output = T>,
) -> Option<T> {
    tokio::select! {
        _ = cancel.cancelled() => None,
        result = timeout(read_timeout, read) => result.ok(),
    }
}

/// SIGKILL the whole process group led by `child` and, belt-and-braces, the
/// direct child itself.
///
/// The child is its own group leader (spawned via `process_group(0)`), so a
/// negative-pid signal reaches every descendant it spawned — mirroring
/// `concerto-tools`' `kill_process_group`. That crate sends the signal
/// through `nix`; `concerto-lsp` must not gain a dependency for this one
/// escalation, so the same signal goes through the system `kill(1)` helper,
/// bounded by [`GROUP_KILL_TIMEOUT`] and best-effort: `start_kill()` still
/// covers the direct child, and `kill_on_drop(true)` covers the drop.
#[cfg(unix)]
async fn kill_process_group(child: &mut Child) {
    if let Some(pid) = child.id() {
        if pid > 0 {
            // `--` ends option parsing so the negative pid is read as the
            // target (a process group), never as a flag or signal.
            let mut cmd = Command::new("kill");
            cmd.arg("-9")
                .arg("--")
                .arg(format!("-{pid}"))
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            let _ = timeout(GROUP_KILL_TIMEOUT, cmd.status()).await;
        }
    }
    let _ = child.start_kill();
}

/// Non-unix: no process groups to signal, so the direct child is the whole
/// tree this client can reach (mirroring concerto-mcp's Windows branch).
#[cfg(not(unix))]
async fn kill_process_group(child: &mut Child) {
    let _ = child.start_kill();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verify that the constructor initialises all fields to their default/empty state.
    #[test]
    fn test_client_constructor_sets_defaults() {
        let client = LspClient::new("/tmp/proj", "rust-analyzer");

        assert_eq!(client.project_dir.to_str(), Some("/tmp/proj"));
        assert_eq!(client.server_cmd, "rust-analyzer");
        assert!(client.child.is_none());
        assert!(client.stdin.is_none());
        assert_eq!(client.next_id.load(std::sync::atomic::Ordering::SeqCst), 1);

        // Internal maps start empty.
        let pending_empty = client.pending.try_lock().unwrap().is_empty();
        assert!(pending_empty);
    }

    /// Calling `send_request` before the server has started must fail because stdin
    /// has not been set up yet.
    #[tokio::test]
    async fn test_client_send_request_fails_without_stdin() {
        let mut client = LspClient::new("/tmp", "rust-analyzer");
        let result = client.send_request("textDocument/hover", serde_json::json!({})).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("stdin not available"));
    }

    /// Calling `send_notification` before the server has started must fail.
    #[tokio::test]
    async fn test_client_send_notification_fails_without_stdin() {
        let mut client = LspClient::new("/tmp", "rust-analyzer");
        let result = client.send_notification("initialized", serde_json::json!({})).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("stdin not available"));
    }

    /// `stop` on a client that was never started should return `Ok(())` without
    /// attempting to kill a non-existent child process.
    #[tokio::test]
    async fn test_client_stop_returns_ok_when_not_started() {
        let mut client = LspClient::new("/tmp", "rust-analyzer");
        let cancel = tokio_util::sync::CancellationToken::new();
        let result = client.stop(cancel).await;
        assert!(result.is_ok());
    }

    /// `get_diagnostics` for a file path that has never received diagnostics
    /// should return an empty `Vec`.
    #[tokio::test]
    async fn test_client_get_diagnostics_returns_empty_for_unknown_file() {
        let client = LspClient::new("/tmp", "rust-analyzer");
        let result = client.get_diagnostics("/nonexistent/file.rs").await;
        assert!(result.is_empty());
    }

    /// Two separate `LspClient` instances must have isolated diagnostics maps.
    #[tokio::test]
    async fn test_client_isolation_between_instances() {
        let client_a = LspClient::new("/tmp/a", "server-a");
        let client_b = LspClient::new("/tmp/b", "server-b");

        // Insert a diagnostic into client_a's map directly (for testing only).
        {
            let mut map = client_a.diagnostics.lock().await;
            map.insert("/tmp/a/file.rs".into(), vec![serde_json::json!({"severity": 1})]);
        }

        // client_b must not see client_a's diagnostics.
        let b_diags = client_b.get_diagnostics("/tmp/a/file.rs").await;
        assert!(b_diags.is_empty());

        // client_a must still see its own.
        let a_diags = client_a.get_diagnostics("/tmp/a/file.rs").await;
        assert_eq!(a_diags.len(), 1);
    }

    // ------------------------------------------------------------------
    // A01: bounded reader + bounded stop
    // ------------------------------------------------------------------

    /// Shared empty reader state for the `run_reader` tests.
    fn reader_inputs() -> (Arc<Mutex<PendingMap>>, Arc<Mutex<DiagMap>>) {
        (Arc::new(Mutex::new(HashMap::new())), Arc::new(Mutex::new(HashMap::new())))
    }

    /// A reader blocked mid-header must exit when the token fires: the
    /// cancellation A01 targets is observed *during* the blocked read, not
    /// only between messages.
    #[tokio::test]
    async fn reader_exits_promptly_when_cancel_fires_during_a_blocked_read() {
        let (writer, reader) = tokio::io::duplex(64);
        let (pending, diagnostics) = reader_inputs();
        let cancel = CancellationToken::new();
        let handle = tokio::spawn(run_reader(
            reader,
            pending,
            diagnostics,
            cancel.clone(),
            Duration::from_secs(30),
        ));
        // Nothing is ever written: the task parks inside `read_line`.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!handle.is_finished(), "reader must still be blocked before cancel");
        cancel.cancel();
        timeout(Duration::from_secs(2), handle)
            .await
            .expect("reader must exit promptly on cancel")
            .expect("reader task must not panic");
        // Keep the writer (the read end's peer) alive until after the
        // assertions so EOF cannot be mistaken for a cancellation exit.
        drop(writer);
    }

    /// A partial header that never completes must not pin the reader: the
    /// per-read bound ends the task. Virtual time keeps the test instant, and
    /// the writer stays open so only the bound can end the read.
    #[tokio::test(start_paused = true)]
    async fn reader_exits_when_a_stalled_read_exceeds_its_bound() {
        let (mut writer, reader) = tokio::io::duplex(64);
        writer.write_all(b"Content-Length: 12\r\n").await.expect("buffered write");
        let (pending, diagnostics) = reader_inputs();
        let cancel = CancellationToken::new();
        let handle =
            tokio::spawn(run_reader(reader, pending, diagnostics, cancel, Duration::from_secs(5)));
        // The reader consumes the partial header, then blocks on the blank
        // line that never arrives; auto-advance fires the 5s bound.
        timeout(Duration::from_secs(60), handle)
            .await
            .expect("the read bound must end the reader task")
            .expect("reader task must not panic");
        drop(writer);
    }

    /// Regression guard for extracting `run_reader` out of `start`: a
    /// well-framed response still reaches its pending request, and
    /// cancellation then ends the task.
    #[tokio::test]
    async fn reader_delivers_a_framed_response_to_its_pending_request() {
        let (mut writer, reader) = tokio::io::duplex(4096);
        let (pending, diagnostics) = reader_inputs();
        let (tx, rx) = oneshot::channel();
        pending.lock().await.insert(7, tx);
        let cancel = CancellationToken::new();
        let handle = tokio::spawn(run_reader(
            reader,
            pending,
            diagnostics,
            cancel.clone(),
            Duration::from_secs(30),
        ));
        let body =
            serde_json::json!({"jsonrpc": "2.0", "id": 7, "result": {"ok": true}}).to_string();
        let framed = format!("Content-Length: {}\r\n\r\n{}", body.len(), body);
        writer.write_all(framed.as_bytes()).await.expect("buffered write");
        let result = timeout(Duration::from_secs(2), rx)
            .await
            .expect("response must arrive within the bound")
            .expect("sender must not be dropped")
            .expect("result must be Ok");
        assert_eq!(result, serde_json::json!({"ok": true}));
        cancel.cancel();
        timeout(Duration::from_secs(2), handle)
            .await
            .expect("reader must exit on cancel")
            .expect("reader task must not panic");
    }

    /// Hung-server simulation: `tail` reads stdin and never answers, so
    /// `start` fails via the 30s `initialize` timeout but leaves a live child
    /// behind — the state `stop` must clean up. Every wait in `stop` is
    /// bounded (request timeout, exit write, group-kill reap), so the whole
    /// shutdown completes with the child taken. Time is virtual
    /// (`start_paused`), costing milliseconds of real time; the wall-clock
    /// assert guards against a genuinely blocking shutdown.
    #[cfg(unix)]
    #[tokio::test(start_paused = true)]
    async fn stop_is_bounded_when_the_server_never_responds() {
        let wall = std::time::Instant::now();
        let mut client = LspClient::new("/tmp", "tail");
        let cancel = CancellationToken::new();

        let started = client.start(cancel.clone()).await;
        assert!(started.is_err(), "a silent server must fail the initialize handshake");

        let stopped = timeout(Duration::from_secs(120), client.stop(cancel)).await;
        assert!(stopped.expect("stop must complete").is_ok());
        assert!(client.child.is_none(), "the child must be taken and reaped");
        assert!(
            wall.elapsed() < Duration::from_secs(10),
            "stop must not consume real time ({:?})",
            wall.elapsed()
        );
    }
}
