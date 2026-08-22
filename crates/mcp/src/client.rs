//! MCP stdio client: spawns a configured MCP server as a child process and
//! speaks JSON-RPC 2.0 with it over newline-delimited stdin/stdout.
//!
//! Lifecycle: [`McpClient::new`] → [`McpClient::spawn`] →
//! [`McpClient::initialize`] → `list_tools`/`call_tool`/... →
//! [`McpClient::stop`]. A client runs exactly one child process; restarting a
//! crashed server is a fresh `spawn` (the double-spawn guard refuses a second
//! `spawn` on a live client). `initialize` is never cancellable (the spec
//! forbids `notifications/cancelled` for it), so it takes no cancellation
//! token. Every other request takes a caller-supplied timeout plus a
//! [`CancellationToken`]; an elapsed call surfaces as
//! [`McpError::Timeout`] and sends `notifications/cancelled` (best-effort).
//!
//! On `stop`, the client notifies the server of in-flight cancellations,
//! closes stdin (EOF), gives the server a short grace period to exit, then
//! escalates to `kill().await` + `wait().await`. The `Drop` impl reaps any
//! child that was never stopped via `start_kill()` + a bounded `try_wait()`
//! poll, so a server is never orphaned.

use crate::error::McpError;
use crate::transport;
use crate::PROTOCOL_VERSION;
use concerto_api_types::extension::McpToolDescriptor;
use concerto_core::CancellationToken;
use concerto_core::McpServerState;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;
use tokio::io::{AsyncReadExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::{oneshot, watch, Mutex};
use tokio::time::timeout;

type PendingMap = HashMap<u64, oneshot::Sender<Result<Value, McpError>>>;

/// Upper bound on `tools/list` cursor pages, guarding against a server that
/// never stops paginating.
const MAX_TOOL_LIST_PAGES: usize = 1000;

/// How long `stop` waits for the server to exit after stdin EOF before
/// escalating to `kill`.
const GRACE_PERIOD: Duration = Duration::from_secs(2);

/// Bounded poll for the crashed server's exit status: EOF is observed the
/// moment the child's stdout pipe closes, which can precede the process
/// becoming a zombie, so `try_wait()` is retried briefly before giving up.
const CHILD_STATUS_POLLS: usize = 20;
const CHILD_STATUS_POLL_INTERVAL: Duration = Duration::from_millis(5);

/// Structured server identity reported by a successful `initialize`.
#[derive(Debug, Clone, PartialEq)]
pub struct McpServerInfo {
    /// `serverInfo.name` from the server.
    pub name: String,
    /// `serverInfo.version` from the server.
    pub version: String,
    /// `protocolVersion` the negotiation settled on (the pinned revision).
    pub protocol_version: String,
    /// `capabilities` object advertised by the server (kept raw for
    /// future resource/prompt support).
    pub capabilities: Value,
}

/// A single content block from a `tools/call` result.
#[derive(Debug, Clone, PartialEq)]
pub enum McpContent {
    /// `{ "type": "text", "text": "..." }` — the only kind the bridge
    /// consumes in v1.
    Text(String),
    /// `{ "type": "resource", "resource": { ... } }` — payload kept raw.
    Resource(Value),
    /// Any other content block shape, kept raw for diagnostics.
    Other(Value),
}

/// Outcome of a `tools/call` invocation.
#[derive(Debug, Clone, PartialEq)]
pub struct McpCallResult {
    /// Content blocks returned by the server, in order.
    pub content: Vec<McpContent>,
    /// True when the tool itself reported failure (`isError: true`). This is
    /// a *tool-level* failure (recoverable, surfaced to the model) and is
    /// distinct from a JSON-RPC error, which comes back as
    /// [`McpError::JsonRpc`].
    pub is_error: bool,
    /// The raw result object, kept for diagnostics and future UI rendering.
    pub raw: Value,
}

impl McpCallResult {
    /// Concatenation of all text content blocks, joined with `\n`.
    pub fn text(&self) -> String {
        let parts: Vec<&str> = self
            .content
            .iter()
            .filter_map(|block| match block {
                McpContent::Text(text) => Some(text.as_str()),
                _ => None,
            })
            .collect();
        parts.join("\n")
    }

    /// True when the server returned no content blocks.
    pub fn is_empty(&self) -> bool {
        self.content.is_empty()
    }
}

/// A client for one MCP stdio server process.
pub struct McpClient {
    server_id: String,
    /// The server child, shared with the reader task so it can `try_wait()`
    /// the real exit status when the output pipe closes. `None` inside the
    /// mutex once `stop` has taken the process; the `Option` in the field
    /// itself is the double-spawn guard.
    child: Option<Arc<Mutex<Option<Child>>>>,
    /// Shared write handle: the reader task upgrades a `Weak` copy of this
    /// `Arc` to reply to server requests (e.g. `ping`). `stop`/`Drop` drop
    /// the client's strong handle, closing the pipe and signaling EOF.
    stdin: Option<Arc<Mutex<ChildStdin>>>,
    pending: Arc<Mutex<PendingMap>>,
    next_id: Arc<AtomicU64>,
    server_died: Arc<AtomicBool>,
    server_info: Option<McpServerInfo>,
    /// Set while a graceful [`Self::stop`] is in flight so the reader task
    /// does not report the EOF it observes as a crash (`Failed`) — the stop
    /// path sends `Stopped` itself.
    stopping: Arc<AtomicBool>,
    /// Lifecycle state signal (ADR-43 §7), consumed by the `McpManager`
    /// watcher. Starts `Disabled`; `spawn` → `Connecting`, `initialize` →
    /// `Connected`, reader EOF/error → `Failed` (with detail in
    /// [`Self::last_failure`]), `stop` → `Stopped`.
    state_tx: watch::Sender<McpServerState>,
    /// Human-readable failure detail captured when the reader observes EOF/error.
    last_failure: Arc<std::sync::Mutex<Option<String>>>,
}

impl McpClient {
    /// Create an idle client for the given server id.
    ///
    /// The id is a label used in tool namespacing (`mcp:<server_id>:<tool>`);
    /// the non-empty / no-`:` constraint is enforced by
    /// `McpServerConfig::validate()` at config load, so no check is repeated
    /// here.
    pub fn new(server_id: &str) -> Self {
        let (state_tx, _) = watch::channel(McpServerState::Disabled);
        Self {
            server_id: server_id.to_string(),
            child: None,
            stdin: None,
            pending: Arc::new(Mutex::new(HashMap::new())),
            next_id: Arc::new(AtomicU64::new(1)),
            server_died: Arc::new(AtomicBool::new(false)),
            server_info: None,
            stopping: Arc::new(AtomicBool::new(false)),
            state_tx,
            last_failure: Arc::new(std::sync::Mutex::new(None)),
        }
    }

    /// Subscribe to the server's lifecycle state transitions
    /// ([`McpServerState`]). The `McpManager` watcher consumes this channel
    /// to publish [`EventKind::McpServerStateChanged`]
    /// (concerto_core::event::EventKind) events; the desktop/CLI can also
    /// subscribe directly for live health.
    pub fn subscribe_state(&self) -> watch::Receiver<McpServerState> {
        self.state_tx.subscribe()
    }

    /// The most recent failure detail, captured when the reader observed EOF
    /// or an I/O error (or when the manager recorded a registration failure).
    pub fn last_failure_detail(&self) -> Option<String> {
        self.last_failure.lock().unwrap_or_else(|error| error.into_inner()).clone()
    }

    /// Record a registration-time failure (manager side): stores the detail
    /// and flips the state to `Failed` so the watcher publishes the event.
    pub(crate) fn record_failure(&self, detail: String) {
        *self.last_failure.lock().unwrap_or_else(|error| error.into_inner()) = Some(detail);
        let _ = self.state_tx.send(McpServerState::Failed);
    }

    /// Spawn the server child process and start the reader/stderr tasks.
    ///
    /// `env` entries are appended to the child's environment (config-supplied
    /// env only; secrets are never stored in TOML). This is a one-shot
    /// operation: calling `spawn` again while a server is live returns
    /// [`McpError::AlreadySpawned`]. A crashed server must be restarted via a
    /// fresh `spawn` (which is allowed after the old process exited).
    pub async fn spawn(
        &mut self,
        command: &str,
        args: &[String],
        env: &[(&str, &str)],
    ) -> Result<(), McpError> {
        if self.child.is_some() {
            return Err(McpError::AlreadySpawned);
        }
        let mut cmd = Command::new(command);
        cmd.args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Safety net for the spawn-error path; the client's own `Drop`
            // reaps normally via `start_kill()` + `try_wait()`.
            .kill_on_drop(true);
        for (key, value) in env {
            cmd.env(key, value);
        }
        let mut child = cmd.spawn().map_err(McpError::from)?;
        let stdin = child.stdin.take().ok_or_else(|| pipe_error("stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| pipe_error("stdout"))?;
        let stderr = child.stderr.take().ok_or_else(|| pipe_error("stderr"))?;
        // The three pipes above are guaranteed by `Stdio::piped`; on the
        // impossible failure path `kill_on_drop(true)` kills the child.

        let stdin_shared = Arc::new(Mutex::new(stdin));
        let child_shared = Arc::new(Mutex::new(Some(child)));
        tokio::spawn(reader_task(
            stdout,
            self.pending.clone(),
            self.server_died.clone(),
            self.stopping.clone(),
            Arc::downgrade(&child_shared),
            Arc::downgrade(&stdin_shared),
            self.server_id.clone(),
            self.state_tx.clone(),
            self.last_failure.clone(),
        ));
        tokio::spawn(stderr_pump(stderr, self.server_id.clone()));

        self.child = Some(child_shared);
        self.stdin = Some(stdin_shared);
        self.server_died.store(false, Ordering::SeqCst);
        self.stopping.store(false, Ordering::SeqCst);
        self.server_info = None;
        let _ = self.state_tx.send(McpServerState::Connecting);
        Ok(())
    }

    /// Whether a live, connected server is running.
    ///
    /// Returns `false` once the server process has exited (or been stopped),
    /// even before `stop`/`Drop` reap it.
    pub fn connected(&self) -> bool {
        self.child.is_some() && self.stdin.is_some() && !self.server_died.load(Ordering::SeqCst)
    }

    /// The server identity reported by `initialize`, once initialized.
    pub fn server_info(&self) -> Option<&McpServerInfo> {
        self.server_info.as_ref()
    }

    /// Perform the `initialize` handshake and send `notifications/initialized`.
    ///
    /// Negotiates the pinned protocol version ([`PROTOCOL_VERSION`]). If the
    /// server replies with a different `protocolVersion`, or rejects
    /// `initialize` with the `-32602` + `data.supported` negotiation error,
    /// the call fails with [`McpError::VersionMismatch`] and the client should
    /// be stopped. Never cancellable per spec. Idempotent: a second call
    /// returns the cached result.
    pub async fn initialize(&mut self, timeout_secs: u64) -> Result<McpServerInfo, McpError> {
        if let Some(info) = &self.server_info {
            return Ok(info.clone());
        }
        let params = json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": { "name": "concerto", "version": env!("CARGO_PKG_VERSION") },
        });
        let response = self
            .request_internal("initialize", params, timeout_secs, CancellationToken::new(), false)
            .await?;
        let protocol_version =
            response.get("protocolVersion").and_then(Value::as_str).ok_or_else(|| {
                McpError::Protocol {
                    detail: "initialize response missing 'protocolVersion'".into(),
                }
            })?;
        if protocol_version != PROTOCOL_VERSION {
            return Err(McpError::VersionMismatch {
                supported: vec![PROTOCOL_VERSION.to_string()],
            });
        }
        let server_info = McpServerInfo {
            name: response
                .get("serverInfo")
                .and_then(|i| i.get("name"))
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string(),
            version: response
                .get("serverInfo")
                .and_then(|i| i.get("version"))
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string(),
            protocol_version: protocol_version.to_string(),
            capabilities: response.get("capabilities").cloned().unwrap_or_else(|| json!({})),
        };
        // notifications/initialized is fire-and-forget; a write failure here
        // means the server already died, so surface it.
        let notification = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
        {
            let stdin = self.stdin.as_ref().ok_or(McpError::NotConnected)?;
            let mut stdin = stdin.lock().await;
            transport::write_message(&mut *stdin, &notification).await?;
        }
        self.server_info = Some(server_info.clone());
        let _ = self.state_tx.send(McpServerState::Connected);
        Ok(server_info)
    }

    /// List the server's tools, following `nextCursor` pagination.
    ///
    /// Per the spec, an empty-string cursor is valid and means "more pages",
    /// so iteration continues while `nextCursor` is *present* (not merely
    /// non-empty) and stops when it is absent. Wire `inputSchema` (camelCase)
    /// is mapped into the shared [`McpToolDescriptor`] type.
    pub async fn list_tools(
        &mut self,
        timeout_secs: u64,
        cancel: CancellationToken,
    ) -> Result<Vec<McpToolDescriptor>, McpError> {
        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_TOOL_LIST_PAGES {
            let params = match &cursor {
                Some(c) => json!({ "cursor": c }),
                None => json!({}),
            };
            let result = self
                .request_internal("tools/list", params, timeout_secs, cancel.clone(), true)
                .await?;
            if let Some(entries) = result.get("tools").and_then(Value::as_array) {
                for entry in entries {
                    let name = entry
                        .get("name")
                        .and_then(Value::as_str)
                        .ok_or_else(|| McpError::Protocol {
                            detail: "tools/list entry missing string 'name'".into(),
                        })?
                        .to_string();
                    let description =
                        entry.get("description").and_then(Value::as_str).map(String::from);
                    let input_schema = entry.get("inputSchema").cloned().unwrap_or(Value::Null);
                    tools.push(McpToolDescriptor { name, description, input_schema });
                }
            } else if result.get("tools").is_some() {
                return Err(McpError::Protocol {
                    detail: "'tools' in tools/list result is not an array".into(),
                });
            }
            cursor = result.get("nextCursor").and_then(Value::as_str).map(String::from);
            if cursor.is_none() {
                return Ok(tools);
            }
        }
        Err(McpError::Protocol {
            detail: format!("tools/list exceeded {MAX_TOOL_LIST_PAGES} cursor pages"),
        })
    }

    /// Invoke a server tool.
    ///
    /// The server's `isError` flag is surfaced as [`McpCallResult::is_error`]
    /// (a recoverable tool-level failure), while a JSON-RPC error reply is
    /// returned as [`McpError::JsonRpc`].
    pub async fn call_tool(
        &mut self,
        name: &str,
        arguments: Value,
        timeout_secs: u64,
        cancel: CancellationToken,
    ) -> Result<McpCallResult, McpError> {
        let params = json!({ "name": name, "arguments": arguments });
        let result =
            self.request_internal("tools/call", params, timeout_secs, cancel, true).await?;
        let is_error = result.get("isError").and_then(Value::as_bool).unwrap_or(false);
        let mut content = Vec::new();
        if let Some(blocks) = result.get("content").and_then(Value::as_array) {
            for block in blocks {
                match block.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        let text = block
                            .get("text")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        content.push(McpContent::Text(text));
                    }
                    Some("resource") => {
                        content.push(McpContent::Resource(
                            block.get("resource").cloned().unwrap_or(Value::Null),
                        ));
                    }
                    _ => content.push(McpContent::Other(block.clone())),
                }
            }
        }
        Ok(McpCallResult { content, is_error, raw: result })
    }

    /// Send an arbitrary request and await its `result` object.
    ///
    /// Useful for protocol methods the client does not special-case yet
    /// (e.g. resources/prompts in a later revision).
    pub async fn request(
        &mut self,
        method: &str,
        params: Value,
        timeout_secs: u64,
        cancel: CancellationToken,
    ) -> Result<Value, McpError> {
        self.request_internal(method, params, timeout_secs, cancel, true).await
    }

    /// Send a `ping` and await the (empty) result.
    pub async fn ping(
        &mut self,
        timeout_secs: u64,
        cancel: CancellationToken,
    ) -> Result<Value, McpError> {
        self.request("ping", json!({}), timeout_secs, cancel).await
    }

    /// Gracefully shut down the server and reap the child process.
    ///
    /// Sends `notifications/cancelled` for every in-flight request, closes
    /// stdin (the server observes EOF), waits up to [`GRACE_PERIOD`] for a
    /// voluntary exit, then escalates to `kill` + `wait`. In-flight requests
    /// may still be answered by a fast server during the grace period; any
    /// leftovers are failed with [`McpError::ServerExited`]. Returns the
    /// server's exit status.
    pub async fn stop(&mut self) -> Result<std::process::ExitStatus, McpError> {
        let child_shared = self.child.take().ok_or(McpError::NotConnected)?;
        // Take the process out of the mutex so no lock is held across the
        // await points below.
        let mut child = child_shared.lock().await.take().ok_or(McpError::NotConnected)?;
        // Mark the stop before dropping stdin: the reader task will observe
        // EOF and must not report a spurious crash (Failed) — `Stopped` is
        // sent below once the child is reaped.
        self.stopping.store(true, Ordering::SeqCst);
        {
            let ids: Vec<u64> = self.pending.lock().await.keys().copied().collect();
            for id in ids {
                self.send_cancelled_notification(id, Some("client shutting down")).await;
            }
        }
        // Close stdin: the server observes EOF and should exit.
        drop(self.stdin.take());
        let status = match timeout(GRACE_PERIOD, child.wait()).await {
            Ok(Ok(status)) => status,
            Ok(Err(e)) => return Err(McpError::from(e)),
            Err(_elapsed) => {
                tracing::info!(server = %self.server_id, "server did not exit within grace period; killing");
                child.kill().await.map_err(McpError::from)?;
                child.wait().await.map_err(McpError::from)?
            }
        };
        let exit_code = status.code();
        fail_all_pending(&self.pending, || McpError::ServerExited {
            status: exit_code,
            detail: "client stopped".into(),
        })
        .await;
        let _ = self.state_tx.send(McpServerState::Stopped);
        Ok(status)
    }

    /// Core request/response exchange shared by every method.
    ///
    /// `send_cancel_notification` is false only for `initialize` (the spec
    /// forbids `notifications/cancelled` for it).
    #[allow(clippy::too_many_arguments)]
    async fn request_internal(
        &mut self,
        method: &str,
        params: Value,
        timeout_secs: u64,
        cancel: CancellationToken,
        send_cancel_notification: bool,
    ) -> Result<Value, McpError> {
        if self.server_died.load(Ordering::SeqCst) {
            return Err(McpError::NotConnected);
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });

        let write_result = {
            let stdin = match self.stdin.as_ref() {
                Some(stdin) => stdin,
                None => {
                    self.pending.lock().await.remove(&id);
                    return Err(McpError::NotConnected);
                }
            };
            let mut stdin = stdin.lock().await;
            transport::write_message(&mut *stdin, &request).await
        };
        if let Err(e) = write_result {
            // A failed stdin write means the server's read end is gone
            // (EPIPE) — the connection is definitively dead. Mark it now
            // rather than waiting for the reader task to observe EOF, so
            // `connected()` is false immediately after the failed call.
            self.server_died.store(true, Ordering::SeqCst);
            self.pending.lock().await.remove(&id);
            return Err(e);
        }

        // Watch the caller's token: resolve the pending slot if it fires
        // mid-flight. Aborted once the request settles.
        let watcher = {
            let pending = self.pending.clone();
            tokio::spawn(async move {
                cancel.cancelled().await;
                if let Some(tx) = pending.lock().await.remove(&id) {
                    let _ = tx.send(Err(McpError::Cancelled));
                }
            })
        };

        let outcome = timeout(Duration::from_secs(timeout_secs), rx).await;
        watcher.abort();
        match outcome {
            // oneshot::Receiver yields Result<T, RecvError>; timeout wraps that.
            Ok(Ok(Ok(value))) => Ok(value),
            Ok(Ok(Err(McpError::Cancelled))) => {
                if send_cancel_notification {
                    self.send_cancelled_notification(id, Some("cancelled by caller")).await;
                }
                Err(McpError::Cancelled)
            }
            Ok(Ok(Err(e))) => Err(e),
            Ok(Err(_)) => {
                // The pending slot was dropped without a value: the reader
                // removed it without sending, which is a client bug. Fail
                // loudly rather than hanging the caller.
                Err(McpError::Protocol { detail: "request slot dropped without a response".into() })
            }
            Err(_elapsed) => {
                // Do not leak the pending slot; the server's late response
                // (if any) will be ignored by the reader.
                self.pending.lock().await.remove(&id);
                if send_cancel_notification {
                    self.send_cancelled_notification(id, Some("request timed out")).await;
                }
                Err(McpError::Timeout { method: method.to_string() })
            }
        }
    }

    /// Best-effort `notifications/cancelled` for `id`. Write failures are
    /// ignored: the server may already be gone.
    async fn send_cancelled_notification(&self, id: u64, reason: Option<&str>) {
        let params = match reason {
            Some(reason) => json!({ "requestId": id, "reason": reason }),
            None => json!({ "requestId": id }),
        };
        let notification =
            json!({ "jsonrpc": "2.0", "method": "notifications/cancelled", "params": params });
        let Some(stdin) = self.stdin.as_ref() else { return };
        let mut stdin = stdin.lock().await;
        let _ = transport::write_message(&mut *stdin, &notification).await;
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        // `tokio::process::Child` detaches from the OS process on drop, which
        // would orphan the server. tokio 1.52 has no `Child::into_std`, so
        // reap synchronously: SIGKILL via `start_kill()` (sync), then poll
        // `try_wait()` (sync) until the process is reaped. Bounded to ~1s;
        // in the pathological case init reparents and reaps the orphan.
        // `try_lock` (sync) succeeds unless the reader task is mid-reap, in
        // which case `kill_on_drop(true)` on the `Child` drop covers us.
        if let Some(child) = self.child.as_mut() {
            if let Ok(mut guard) = child.try_lock() {
                if let Some(child) = guard.as_mut() {
                    let _ = child.start_kill();
                    for _ in 0..100 {
                        match child.try_wait() {
                            Ok(Some(_)) => return,
                            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(10)),
                            Err(_) => return, // already reaped or error; nothing to do
                        }
                    }
                    tracing::warn!(
                        "mcp server child did not reap within 1s of kill; leaving to init"
                    );
                }
            } else {
                tracing::debug!("mcp server child busy at drop; kill_on_drop covers it");
            }
        }
        // self.stdin is dropped here; the pipe closes and the server sees EOF.
    }
}

fn pipe_error(what: &str) -> McpError {
    McpError::Io {
        source: std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            format!("{what} pipe missing after spawn"),
        ),
    }
}

/// Reader task: consumes server stdout until EOF, dispatching server
/// requests and resolving pending requests by id. On EOF/error the server's
/// lifecycle state is flipped to `Failed` (with the exit detail) so the
/// `McpManager` watcher can publish the crash event.
#[allow(clippy::too_many_arguments)]
async fn reader_task(
    mut stdout: ChildStdout,
    pending: Arc<Mutex<PendingMap>>,
    server_died: Arc<AtomicBool>,
    stopping: Arc<AtomicBool>,
    child_weak: Weak<Mutex<Option<Child>>>,
    stdin_weak: Weak<Mutex<ChildStdin>>,
    server_id: String,
    state_tx: watch::Sender<McpServerState>,
    last_failure: Arc<std::sync::Mutex<Option<String>>>,
) {
    let mut reader = BufReader::new(&mut stdout);
    let mut buf: Vec<u8> = Vec::new();
    loop {
        match transport::read_message(&mut reader, &mut buf).await {
            Ok(Some(message)) => {
                if transport::is_server_request(&message) {
                    handle_server_request(&stdin_weak, &message, &server_id).await;
                } else if transport::is_response(&message) {
                    if let Some(id) = transport::response_id(&message) {
                        if let Some(tx) = pending.lock().await.remove(&id) {
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
) {
    let method = message.get("method").and_then(Value::as_str).unwrap_or_default();
    let Some(reply) = transport::ping_reply(message) else { return };
    if method != "ping" {
        tracing::warn!(server = %server_id, method, "ignoring unsupported server request");
        return;
    }
    let Some(stdin) = stdin_weak.upgrade() else { return };
    let mut stdin = stdin.lock().await;
    let _ = transport::write_message(&mut *stdin, &reply).await;
}

/// Stderr pump: stream the server's stderr into the log at warn level so
/// server-side diagnostics are visible without blocking (bounded 1 KiB
/// chunks).
async fn stderr_pump(mut stderr: ChildStderr, server_id: String) {
    let mut reader = BufReader::new(&mut stderr);
    let mut chunk = [0u8; 1024];
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let text = String::from_utf8_lossy(&chunk[..n]);
                for line in text.lines() {
                    tracing::warn!(server = %server_id, "mcp server stderr: {line}");
                }
            }
        }
    }
}

/// Fail every still-pending request with the error produced by `make_error`
/// (called per recipient so the error need not be `Clone`). Idempotent:
/// already-resolved slots are simply absent.
async fn fail_all_pending<F>(pending: &Arc<Mutex<PendingMap>>, make_error: F)
where
    F: Fn() -> McpError,
{
    let mut map = pending.lock().await;
    for (_, tx) in map.drain() {
        let _ = tx.send(Err(make_error()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn call_result_text_joins_text_blocks() {
        let result = McpCallResult {
            content: vec![
                McpContent::Text("hello".into()),
                McpContent::Other(json!({ "type": "image" })),
                McpContent::Text("world".into()),
            ],
            is_error: false,
            raw: json!({}),
        };
        assert_eq!(result.text(), "hello\nworld");
        assert!(!result.is_empty());
    }

    #[test]
    fn call_result_is_empty_when_no_content() {
        let result = McpCallResult { content: vec![], is_error: false, raw: json!({}) };
        assert!(result.is_empty());
        assert_eq!(result.text(), "");
    }

    #[test]
    fn idle_client_starts_disconnected() {
        let client = McpClient::new("fixture");
        assert!(!client.connected());
        assert!(client.server_info().is_none());
    }
}
