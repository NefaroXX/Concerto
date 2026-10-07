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
//!
//! The stdout reader loop (message dispatch, EOF/failure recording, exit
//! polling) lives in the `reader` submodule; this module keeps the lifecycle,
//! request deadlines, and the pending-map spine.

// NORM S12: the reader-loop cluster (dispatch, failure record, exit poll,
// ping reply) lives in `reader`; lifecycle, request deadlines and the
// pending-map guard stay here.
mod reader;

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
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncReadExt;
#[cfg(not(windows))]
use tokio::process::Child;
use tokio::process::{ChildStderr, ChildStdin, Command};
#[cfg(windows)]
type Child = Box<dyn process_wrap::tokio::ChildWrapper>;
use tokio::sync::{oneshot, watch, Mutex};
use tokio::time::timeout;

type PendingMap = HashMap<u64, oneshot::Sender<Result<Value, McpError>>>;

/// Upper bound on `tools/list` cursor pages, guarding against a server that
/// never stops paginating.
const MAX_TOOL_LIST_PAGES: usize = 1000;

/// How long `stop` waits for the server to exit after stdin EOF before
/// escalating to `kill`.
const GRACE_PERIOD: Duration = Duration::from_secs(2);

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
    process_id: Option<u32>,
    /// The server child, shared with the reader task so it can `try_wait()`
    /// the real exit status when the output pipe closes. `None` inside the
    /// mutex once `stop` has taken the process; the `Option` in the field
    /// itself is the double-spawn guard.
    child: Option<Arc<Mutex<Option<Child>>>>,
    /// Shared write handle: the reader task upgrades a `Weak` copy of this
    /// `Arc` to reply to server requests (e.g. `ping`). `stop`/`Drop` drop
    /// the client's strong handle, closing the pipe and signaling EOF.
    stdin: Option<Arc<Mutex<ChildStdin>>>,
    pending: Arc<std::sync::Mutex<PendingMap>>,
    next_id: Arc<AtomicU64>,
    server_died: Arc<AtomicBool>,
    server_info: Option<McpServerInfo>,
    redactions: Vec<concerto_core::SecretString>,
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
            process_id: None,
            child: None,
            stdin: None,
            pending: Arc::new(std::sync::Mutex::new(HashMap::new())),
            next_id: Arc::new(AtomicU64::new(1)),
            server_died: Arc::new(AtomicBool::new(false)),
            server_info: None,
            redactions: Vec::new(),
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
        cmd.env_clear();
        for key in [
            "PATH",
            "SystemRoot",
            "WINDIR",
            "COMSPEC",
            "PATHEXT",
            "HOME",
            "USERPROFILE",
            "TEMP",
            "TMP",
            "TMPDIR",
            "LANG",
        ] {
            if let Some(value) = std::env::var_os(key) {
                cmd.env(key, value);
            }
        }
        #[cfg(unix)]
        cmd.process_group(0);
        for (key, value) in env {
            cmd.env(key, value);
        }
        #[cfg(not(windows))]
        let mut child = cmd.spawn().map_err(McpError::from)?;
        #[cfg(windows)]
        let mut child = {
            let mut wrapped = process_wrap::tokio::CommandWrap::from(cmd);
            wrapped.wrap(process_wrap::tokio::KillOnDrop).wrap(process_wrap::tokio::JobObject);
            wrapped.spawn().map_err(McpError::from)?
        };
        self.process_id = child.id();
        #[cfg(windows)]
        let (stdin, stdout, stderr) =
            (child.stdin().take(), child.stdout().take(), child.stderr().take());
        #[cfg(not(windows))]
        let (stdin, stdout, stderr) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take());
        let stdin = stdin.ok_or_else(|| pipe_error("stdin"))?;
        let stdout = stdout.ok_or_else(|| pipe_error("stdout"))?;
        let stderr = stderr.ok_or_else(|| pipe_error("stderr"))?;
        // The three pipes above are guaranteed by `Stdio::piped`; on the
        // impossible failure path `kill_on_drop(true)` kills the child.

        let stdin_shared = Arc::new(Mutex::new(stdin));
        let child_shared = Arc::new(Mutex::new(Some(child)));
        // Thin delegate: the reader loop and its helpers live in `reader`.
        tokio::spawn(reader::reader_task(
            stdout,
            self.pending.clone(),
            self.server_died.clone(),
            self.stopping.clone(),
            Arc::downgrade(&child_shared),
            Arc::downgrade(&stdin_shared),
            self.server_id.clone(),
            self.state_tx.clone(),
            self.last_failure.clone(),
            self.process_id,
        ));
        let redactions: Vec<_> = env
            .iter()
            .filter(|(_, value)| !value.is_empty())
            .map(|(_, value)| concerto_core::SecretString::from(*value))
            .collect();
        self.redactions = redactions.clone();
        tokio::spawn(stderr_pump(stderr, self.server_id.clone(), redactions));

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
        let result = timeout(
            Duration::from_secs(timeout_secs.clamp(1, 300)),
            self.initialize_inner(timeout_secs),
        )
        .await;
        match result {
            Ok(result) => result,
            Err(_) => {
                self.server_died.store(true, Ordering::SeqCst);
                terminate_process_id(self.process_id);
                terminate_shared_tree(&self.child);
                Err(McpError::Timeout { method: "initialize".into() })
            }
        }
    }
    async fn initialize_inner(&mut self, timeout_secs: u64) -> Result<McpServerInfo, McpError> {
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
        tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(McpError::Cancelled),
            result = timeout(Duration::from_secs(timeout_secs.clamp(1, 300)), self.list_tools_inner(timeout_secs, cancel.clone())) => result.unwrap_or_else(|_| Err(McpError::Timeout { method: "tools/list".into() })),
        }
    }
    async fn list_tools_inner(
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
        let mut result =
            self.request_internal("tools/call", params, timeout_secs, cancel, true).await?;
        let is_error = result.get("isError").and_then(Value::as_bool).unwrap_or(false);
        if is_error {
            redact_diagnostic_value(&mut result, &self.redactions);
        }
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
            let ids: Vec<u64> =
                self.pending.lock().unwrap_or_else(|e| e.into_inner()).keys().copied().collect();
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
                terminate_server_tree(&mut child);
                child.start_kill().map_err(McpError::from)?;
                child.wait().await.map_err(McpError::from)?
            }
        };
        // The parent may exit while descendants still hold inherited pipes.
        terminate_process_id(self.process_id.take());
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
        self.pending.lock().unwrap_or_else(|e| e.into_inner()).insert(id, tx);
        let mut guard = RequestGuard {
            id,
            pending: self.pending.clone(),
            child: self.child.clone(),
            process_id: self.process_id,
            died: self.server_died.clone(),
            write_started: false,
            write_complete: false,
        };
        let request = json!({"jsonrpc":"2.0", "id":id, "method":method, "params":params});
        let exchange = async {
            let stdin = self.stdin.as_ref().ok_or(McpError::NotConnected)?;
            let mut stdin = stdin.lock().await;
            guard.write_started = true;
            transport::write_message(&mut *stdin, &request).await?;
            guard.write_complete = true;
            drop(stdin);
            rx.await.map_err(|_| McpError::Protocol {
                detail: "request slot dropped without a response".into(),
            })?
        };
        let outcome = tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(McpError::Cancelled),
            result = timeout(Duration::from_secs(timeout_secs.clamp(1, 300)), exchange) =>
                result.unwrap_or_else(|_| Err(McpError::Timeout { method: method.into() })),
        };
        if outcome.is_err() && guard.write_started && !guard.write_complete {
            self.server_died.store(true, Ordering::SeqCst);
        }
        let notify = send_cancel_notification
            && guard.write_complete
            && matches!(&outcome, Err(McpError::Cancelled | McpError::Timeout { .. }));
        drop(guard);
        if notify {
            self.send_cancelled_notification(id, Some("request cancelled or timed out")).await;
        }
        outcome.map_err(|error| match error {
            McpError::JsonRpc { code, message, mut data } => {
                if let Some(data) = data.as_mut() {
                    redact_diagnostic_value(data, &self.redactions);
                }
                McpError::JsonRpc {
                    code,
                    message: redact_diagnostic_text(message, &self.redactions),
                    data,
                }
            }
            error => error,
        })
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
        let mut started = false;
        let result = timeout(Duration::from_millis(100), async {
            let mut stdin = stdin.lock().await;
            started = true;
            transport::write_message(&mut *stdin, &notification).await
        })
        .await;
        if started && !matches!(result, Ok(Ok(()))) {
            self.server_died.store(true, Ordering::SeqCst);
            terminate_process_id(self.process_id);
            terminate_shared_tree(&self.child);
        }
    }
}

struct RequestGuard {
    id: u64,
    pending: Arc<std::sync::Mutex<PendingMap>>,
    child: Option<Arc<Mutex<Option<Child>>>>,
    process_id: Option<u32>,
    died: Arc<AtomicBool>,
    write_started: bool,
    write_complete: bool,
}
impl Drop for RequestGuard {
    fn drop(&mut self) {
        self.pending.lock().unwrap_or_else(|e| e.into_inner()).remove(&self.id);
        if self.write_started && !self.write_complete {
            self.died.store(true, Ordering::SeqCst);
            terminate_process_id(self.process_id);
            if let Some(shared) = &self.child {
                if let Ok(mut child) = shared.try_lock() {
                    if let Some(child) = child.as_mut() {
                        terminate_server_tree(child);
                        let _ = child.start_kill();
                    }
                }
            }
        }
    }
}
fn terminate_server_tree(child: &mut Child) {
    #[cfg(windows)]
    let _ = child.start_kill(); // The retained Job Object outlives its immediate parent.
    #[cfg(not(windows))]
    terminate_process_id(child.id());
}
fn terminate_process_id(id: Option<u32>) {
    #[cfg(unix)]
    if let Some(id) = id {
        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(-(id as i32)),
            nix::sys::signal::Signal::SIGKILL,
        );
    }
    #[cfg(not(unix))]
    let _ = id;
}

fn terminate_shared_tree(child: &Option<Arc<Mutex<Option<Child>>>>) {
    if let Some(child) = child {
        if let Ok(mut guard) = child.try_lock() {
            if let Some(child) = guard.as_mut() {
                terminate_server_tree(child);
            }
        }
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        terminate_process_id(self.process_id.take());
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
                    terminate_server_tree(child);
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

/// Stderr pump: stream the server's stderr into the log at warn level so
/// server-side diagnostics are visible without blocking (bounded 1 KiB
/// chunks).
async fn stderr_pump(
    mut stderr: ChildStderr,
    server_id: String,
    redactions: Vec<concerto_core::SecretString>,
) {
    let mut chunk = [0u8; 1024];
    let mut line = Vec::new();
    let mut oversized = false;
    while let Ok(n) = stderr.read(&mut chunk).await {
        if n == 0 {
            if !line.is_empty() && !oversized {
                log_stderr_line(&server_id, &line, &redactions);
            }
            break;
        }
        for byte in &chunk[..n] {
            if *byte == b'\n' {
                if oversized {
                    tracing::warn!(server = %server_id, "mcp oversized stderr line omitted");
                } else {
                    log_stderr_line(&server_id, &line, &redactions);
                }
                line.clear();
                oversized = false;
            } else if line.len() < 4096 && !oversized {
                line.push(*byte);
            } else {
                line.clear();
                oversized = true;
            }
        }
    }
}
fn redact_diagnostic_text(mut text: String, secrets: &[concerto_core::SecretString]) -> String {
    for secret in secrets {
        if !secret.expose().is_empty() {
            text = text.replace(secret.expose(), "[redacted]");
        }
    }
    text
}
fn redact_diagnostic_value(value: &mut Value, secrets: &[concerto_core::SecretString]) {
    match value {
        Value::String(text) => *text = redact_diagnostic_text(std::mem::take(text), secrets),
        Value::Array(items) => {
            items.iter_mut().for_each(|item| redact_diagnostic_value(item, secrets))
        }
        Value::Object(items) => {
            items.values_mut().for_each(|item| redact_diagnostic_value(item, secrets))
        }
        _ => {}
    }
}
fn log_stderr_line(server_id: &str, bytes: &[u8], secrets: &[concerto_core::SecretString]) {
    let line = redact_diagnostic_text(String::from_utf8_lossy(bytes).into_owned(), secrets);
    tracing::warn!(server = %server_id, "mcp server stderr: {line}");
}

/// Fail every still-pending request with the error produced by `make_error`
/// (called per recipient so the error need not be `Clone`). Idempotent:
/// already-resolved slots are simply absent.
async fn fail_all_pending<F>(pending: &Arc<std::sync::Mutex<PendingMap>>, make_error: F)
where
    F: Fn() -> McpError,
{
    let mut map = pending.lock().unwrap_or_else(|e| e.into_inner());
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
