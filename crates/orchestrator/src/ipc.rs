//! Supervisor/agent IPC protocol types — ADR-60 vertical slice S3.
//!
//! The shared, typed message contract for the bespoke supervisor ↔ agent
//! protocol (ADR-60 D2): newline-delimited JSON-RPC 2.0 over stdio. This
//! module defines the wire types (envelopes, methods, params, results, error
//! taxonomy), the transport framing, and the versioned handshake helpers.
//!
//! ## Framing
//!
//! [`write_message`] emits one compact-JSON message per line (terminated by
//! `\n`) and flushes the writer before returning; [`read_message`] reads
//! exactly one line back, bounded by `max_len`
//! (default [`MAX_MESSAGE_BYTES`]). Clean EOF between lines yields
//! `Ok(None)`; EOF mid-message is [`IpcTransportError::Closed`]; a line over
//! the cap is [`IpcTransportError::Oversized`]; malformed frame bytes (empty
//! line or unparseable JSON) surface as [`IpcTransportError::Io`] with
//! `ErrorKind::InvalidData`, and invalid UTF-8 as
//! [`IpcTransportError::InvalidUtf8`].
//!
//! ## Versioning
//!
//! [`PROTOCOL_VERSION`] is the semver the two ends negotiate at handshake;
//! both sides fail loudly on mismatch (ADR-60 D2). [`IpcErrorCode::VersionMismatch`]
//! is the `-32000` boundary error used to surface that failure.
//!
//! ## Error taxonomy lives at the boundary
//!
//! [`IpcError`] / [`IpcErrorCode`] map every failure onto a small closed set:
//! the standard JSON-RPC 2.0 codes (`-32700`..=`-32603`) for protocol errors,
//! and the supervisor-domain codes (`-32000`..=`-32004`) for gate and
//! lifecycle outcomes. [`IpcError::from_gate`] is the single adapter from the
//! gate's error type to the wire, so agents never need to interpret
//! `GateError::*` shapes directly.

use crate::gate::{GateError, GateOutcome, GateRequest};
use concerto_core::types::ToolDefinition;
use concerto_sessions::whiteboard::{NewWhiteboardEvent, WhiteboardEvent, WhiteboardScope};
use serde::{Deserialize, Serialize};

/// Semver of the supervisor/agent IPC protocol (ADR-60 D2). Negotiated during
/// handshake; both sides reject the peer on mismatch. `0.2.0` adds the ADR-60
/// D3 whiteboard-subscription push methods and the wire-optional
/// `subscriptions` handshake field; the change is additive, so no
/// renegotiation logic exists.
pub const PROTOCOL_VERSION: &str = "0.2.0";

/// The request methods an agent (or the supervisor) may send. Kebab-case on
/// the wire (`serde(rename_all)`), matching JSON-RPC method naming.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum IpcMethod {
    /// Execute a tool through the supervisor's write gate (ADR-60 D4).
    ExecuteTool,
    /// Append an event to the whiteboard log (ADR-60 D3).
    PublishEvent,
    /// Retrieve memory chunks for an agent (ADR-60 D6).
    RetrieveMemory,
    /// Fetch the supervisor's tool registry — the single source of truth for
    /// the tools an agent may call (ADR-60 S5 agent-process entry).
    ListTools,
    /// Supervisor liveness/readiness signal (ADR-60 D1).
    Heartbeat,
    /// Versioned handshake at process startup.
    Handshake,
    /// Push a whiteboard slice to an agent (ADR-60 D3 subscription push;
    /// supervisor → agent notification).
    WhiteboardSlice,
    /// Agent acknowledgement of a pushed slice (ADR-60 D3; advances the
    /// supervisor's persisted cursor).
    AckWhiteboard,
}

impl IpcMethod {
    /// The method's wire name (kebab-case, mirroring `#[serde(rename_all)]`).
    pub fn as_str(&self) -> &'static str {
        match self {
            IpcMethod::ExecuteTool => "execute-tool",
            IpcMethod::PublishEvent => "publish-event",
            IpcMethod::RetrieveMemory => "retrieve-memory",
            IpcMethod::ListTools => "list-tools",
            IpcMethod::Heartbeat => "heartbeat",
            IpcMethod::Handshake => "handshake",
            IpcMethod::WhiteboardSlice => "whiteboard-slice",
            IpcMethod::AckWhiteboard => "ack-whiteboard",
        }
    }
}

/// One JSON-RPC 2.0 request envelope (agent → supervisor, or vice versa).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IpcRequest {
    /// JSON-RPC version tag, always the literal `"2.0"`.
    pub jsonrpc: String,
    /// Request id; echoed verbatim on the matching response.
    pub id: u64,
    /// The method being invoked.
    pub method: IpcMethod,
    /// Method-specific payload.
    pub params: IpcParams,
}

/// One JSON-RPC 2.0 response envelope (supervisor → agent).
///
/// Exactly one of `result` / `error` is `Some` in a well-formed response; both
/// fields are held as `Option` so a decoder can tolerate the absent twin
/// instead of failing the whole parse.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IpcResponse {
    /// JSON-RPC version tag, always the literal `"2.0"`.
    pub jsonrpc: String,
    /// Request id echoed from the request this responds to.
    pub id: u64,
    /// Success payload when the method completed.
    pub result: Option<IpcResult>,
    /// Error payload when the method failed.
    pub error: Option<IpcError>,
}

/// A one-way JSON-RPC 2.0 notification envelope (no response expected), e.g.
/// an outbound `heartbeat` or `shutdown`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IpcNotification {
    /// JSON-RPC version tag, always the literal `"2.0"`.
    pub jsonrpc: String,
    /// The method being notified.
    pub method: IpcMethod,
    /// Method-specific payload.
    pub params: IpcParams,
}

/// Method-specific request payloads. Adjacently tagged
/// (`{"type": ..., "value": ...}`) with kebab-case variant names for a stable,
/// round-trip-safe wire form across heterogeneous payload shapes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "kebab-case")]
pub enum IpcParams {
    /// A gated tool write (idempotency keyed by `request.call_id`).
    ExecuteTool {
        /// The write request to evaluate and execute (ADR-60 D4).
        request: GateRequest,
    },
    /// Append an event to the whiteboard log (ADR-60 D3).
    PublishEvent {
        /// The caller-attested event; the supervisor assigns ordering.
        event: NewWhiteboardEvent,
    },
    /// Query the shared memory spine for an agent (ADR-60 D6).
    RetrieveMemory {
        /// Free-text retrieval query.
        query: String,
        /// Agent whose memory is being queried.
        agent_id: String,
        /// Maximum chunks to return.
        limit: u32,
    },
    /// Fetch the supervisor's tool registry (ADR-60 S5).
    ListTools {
        /// Agent requesting the registry (informational; the supervisor
        /// binds it to the registered process like every other method).
        agent_id: String,
    },
    /// Supervisor liveness/readiness heartbeat (ADR-60 D1).
    Heartbeat {
        /// Agent emitting the heartbeat.
        agent_id: String,
        /// Monotonic per-agent sequence; lets the supervisor detect gaps.
        seq: u64,
        /// Unix epoch milliseconds (UTC).
        timestamp_ms: i64,
        /// Free-form status string (`running`, `ready`, ...).
        status: String,
    },
    /// Versioned handshake at startup (ADR-60 D2).
    Handshake {
        /// The peer's `PROTOCOL_VERSION`.
        protocol_version: String,
        /// Agent identity this process serves.
        agent_id: String,
        /// Capability flags/negotiation metadata as free-form JSON.
        capabilities: serde_json::Value,
        /// Whiteboard scopes this agent subscribes to (ADR-60 D3). Wire-
        /// optional for back-compat: older handshakes never carried the field,
        /// and `Option` deserializes an absent wire field to `None` (the same
        /// additive contract as `#[serde(default)]` on
        /// `GateRequest.base_versions`). `None` means "no subscriptions" —
        /// the agent receives no whiteboard slices.
        subscriptions: Option<Vec<WhiteboardScope>>,
    },
    /// Push a contiguous run of whiteboard events to a subscriber (ADR-60 D3;
    /// supervisor → agent `IpcNotification`, no response expected).
    WhiteboardSlice {
        /// The target subscription (the registered agent_id).
        subscription_id: String,
        /// The contiguous, total-ordered run of matching events with
        /// `gate_seq > cursor_gate_seq` up to `end_gate_seq` (inclusive).
        events: Vec<WhiteboardEvent>,
        /// The consistent-cut coordinate of the last event in `events` — the
        /// caller's new high-water mark and the value it acks back.
        end_gate_seq: u64,
    },
    /// Agent acknowledgement of a pushed slice (ADR-60 D3; agent → supervisor
    /// request). The supervisor persists `cursor_gate_seq =
    /// MAX(cursor_gate_seq, end_gate_seq)` — monotonic, never lowers — so
    /// delivery is at-least-once and a crash between apply and ack re-delivers
    /// from the stale cursor.
    AckWhiteboard {
        /// The highest `gate_seq` the agent applied contiguously.
        end_gate_seq: u64,
    },
}

/// Method-specific success payloads (mirrors [`IpcParams`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "kebab-case")]
pub enum IpcResult {
    /// Result of a gated tool execution.
    ExecuteTool {
        /// The gate's durable outcome (replay-aware).
        outcome: GateOutcome,
    },
    /// The stored whiteboard row appended by the log (gate_seq assigned).
    PublishEvent {
        /// The persisted event, including log-assigned ordering.
        stored: WhiteboardEvent,
    },
    /// Retrieved memory chunks, best-match first.
    RetrieveMemory {
        /// Query results; empty when nothing matched.
        chunks: Vec<MemoryChunk>,
    },
    /// The supervisor's tool registry (ADR-60 S5).
    ListTools {
        /// Tool definitions the agent may present to the model.
        tools: Vec<ToolDefinition>,
    },
    /// Supervisor acknowledgement of a heartbeat.
    Heartbeat {
        /// `true` when the heartbeat was accepted.
        accepted: bool,
    },
    /// Handshake outcome.
    Handshake {
        /// The supervisor's `PROTOCOL_VERSION` (echo/probe).
        protocol_version: String,
        /// `true` when versions matched and the connection is accepted.
        accepted: bool,
        /// Supervisor build/version string for diagnostics.
        supervisor_version: String,
    },
    /// Echo of an agent's whiteboard-slice acknowledgement (ADR-60 D3):
    /// mirrors the persisted consistent-cut coordinate. The supervisor
    /// persists `cursor_gate_seq = MAX(cursor_gate_seq, end_gate_seq)`.
    AckWhiteboard {
        /// The acknowledged consistent-cut coordinate.
        end_gate_seq: u64,
    },
}

/// One retrieved memory chunk (ADR-60 D6).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryChunk {
    /// The chunk's text content.
    pub text: String,
    /// Memory kind label (e.g. `fact`, `consolidation`).
    pub kind: String,
    /// Retrieval similarity score, higher is more relevant.
    pub score: f64,
    /// Whiteboard `event_id` this chunk originated from, when known.
    pub source_event_id: Option<String>,
}

/// JSON-RPC 2.0 error object with the supervisor-domain closed code set.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IpcError {
    /// Numeric error code.
    pub code: IpcErrorCode,
    /// Short, human-readable error message.
    pub message: String,
    /// Optional structured detail for the error.
    pub data: Option<serde_json::Value>,
}

impl IpcError {
    /// Build an error without structured detail.
    pub fn new(code: IpcErrorCode, message: impl Into<String>) -> Self {
        Self { code, message: message.into(), data: None }
    }

    /// Map a write-gate error onto the wire taxonomy (ADR-60 D4 boundary).
    ///
    /// - `Denied` → [`IpcErrorCode::GateDenied`], message is the denial reason.
    /// - `Cancelled` → [`IpcErrorCode::Cancelled`].
    /// - `Conflict` → [`IpcErrorCode::Conflict`], message carries the mismatch.
    /// - every other variant → [`IpcErrorCode::GateError`], Display message.
    pub fn from_gate(error: &GateError) -> Self {
        match error {
            GateError::Denied { reason, .. } => Self::new(IpcErrorCode::GateDenied, reason.clone()),
            GateError::Cancelled => Self::new(IpcErrorCode::Cancelled, error.to_string()),
            GateError::Conflict { .. } => Self::new(IpcErrorCode::Conflict, error.to_string()),
            other => Self::new(IpcErrorCode::GateError, other.to_string()),
        }
    }
}

/// Closed error-code set for the supervisor/agent protocol: the standard
/// JSON-RPC 2.0 codes plus the supervisor-domain range. Serialized as its
/// `i32` discriminant on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "i32", into = "i32")]
#[repr(i32)]
pub enum IpcErrorCode {
    /// Invalid JSON was received (standard JSON-RPC).
    ParseError = -32700,
    /// The JSON is valid but not a request/response (standard JSON-RPC).
    InvalidRequest = -32600,
    /// Unknown method (standard JSON-RPC).
    MethodNotFound = -32601,
    /// Invalid method parameters (standard JSON-RPC).
    InvalidParams = -32602,
    /// Internal JSON-RPC error (standard JSON-RPC).
    Internal = -32603,
    /// Handshake protocol-version mismatch (supervisor domain).
    VersionMismatch = -32000,
    /// The write gate denied the request (`GateError::Denied`).
    GateDenied = -32001,
    /// The write gate failed for any other reason.
    GateError = -32002,
    /// The operation was cancelled.
    Cancelled = -32003,
    /// The supervisor/agent was stopped.
    Stopped = -32004,
    /// ADR-60 D5 optimistic concurrency violation (`GateError::Conflict`):
    /// the write's `base_version` no longer matches the target.
    Conflict = -32005,
}

impl std::fmt::Display for IpcErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?} ({})", self, *self as i32)
    }
}

impl From<IpcErrorCode> for i32 {
    fn from(code: IpcErrorCode) -> Self {
        code as i32
    }
}

impl TryFrom<i32> for IpcErrorCode {
    type Error = String;

    fn try_from(value: i32) -> Result<Self, String> {
        match value {
            -32700 => Ok(Self::ParseError),
            -32600 => Ok(Self::InvalidRequest),
            -32601 => Ok(Self::MethodNotFound),
            -32602 => Ok(Self::InvalidParams),
            -32603 => Ok(Self::Internal),
            -32000 => Ok(Self::VersionMismatch),
            -32001 => Ok(Self::GateDenied),
            -32002 => Ok(Self::GateError),
            -32003 => Ok(Self::Cancelled),
            -32004 => Ok(Self::Stopped),
            -32005 => Ok(Self::Conflict),
            other => Err(format!("unknown IpcErrorCode discriminant: {other}")),
        }
    }
}

/// Maximum size of one newline-delimited message, in bytes. Pass this as the
/// `max_len` argument to [`read_message`] for the default cap (16 MiB); it
/// bounds a misbehaving peer from exhausting memory with one unbounded line.
pub const MAX_MESSAGE_BYTES: usize = 16 * 1024 * 1024;

/// Chunk size used when reading a message. Small enough that the carry
/// between calls (bytes after a newline that arrived in the same read) stays
/// bounded.
const READ_CHUNK_BYTES: usize = 4096;

/// A transport-level framing failure, surfaced before any JSON-RPC semantics
/// are applied to the message.
#[derive(Debug)]
pub enum IpcTransportError {
    /// The underlying stream failed; malformed frame bytes are also wrapped
    /// here with `ErrorKind::InvalidData`.
    Io(std::io::Error),
    /// One message (line) exceeded the size cap.
    Oversized { len: usize, max_len: usize },
    /// A line contained bytes that are not valid UTF-8.
    InvalidUtf8,
    /// The peer closed the stream in the middle of a message.
    Closed,
}

impl std::fmt::Display for IpcTransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "IPC transport I/O error: {error}"),
            Self::Oversized { len, max_len } => {
                write!(f, "IPC message of {len} bytes exceeds {max_len}-byte limit")
            }
            Self::InvalidUtf8 => write!(f, "IPC message is not valid UTF-8"),
            Self::Closed => write!(f, "IPC stream closed mid-message"),
        }
    }
}

impl std::error::Error for IpcTransportError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Oversized { .. } | Self::InvalidUtf8 | Self::Closed => None,
        }
    }
}

impl From<std::io::Error> for IpcTransportError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

/// Serialize `message` into one newline-delimited frame (message + `\n`),
/// mirroring the MCP stdio framing. Rejects messages over
/// [`MAX_MESSAGE_BYTES`]. Shared by the async [`write_message`] and by
/// blocking (std-process) writers, so every sender funnels through exactly
/// one framing implementation.
pub fn serialize_frame(message: &serde_json::Value) -> Result<Vec<u8>, IpcTransportError> {
    let mut line = serde_json::to_vec(message).map_err(|error| {
        IpcTransportError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("failed to serialize message: {error}"),
        ))
    })?;
    if line.len() > MAX_MESSAGE_BYTES {
        return Err(IpcTransportError::Oversized { len: line.len(), max_len: MAX_MESSAGE_BYTES });
    }
    // `serde_json`'s compact form escapes newlines, so a raw `\n` byte can
    // only be a bug; reject it rather than silently breaking framing.
    if line.contains(&b'\n') {
        return Err(IpcTransportError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "message would break newline framing",
        )));
    }
    line.push(b'\n');
    Ok(line)
}

/// Serialize `message` and write it as one newline-delimited line (message +
/// `\n`), mirroring the MCP stdio framing. Rejects messages over
/// [`MAX_MESSAGE_BYTES`].
///
/// The writer is flushed before returning, so one awaited call delivers one
/// frame to the OS. This matters for `tokio::io::Stdout`, whose unflushed
/// writes sit in a shared buffer drained by a background flusher thread:
/// relying on that timing made supervised-child output nondeterministic
/// under load (whole bursts could stay unobserved for a child's lifetime).
pub async fn write_message<W>(
    writer: &mut W,
    message: &serde_json::Value,
) -> Result<(), IpcTransportError>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::AsyncWriteExt;

    let line = serialize_frame(message)?;
    writer.write_all(&line).await?;
    writer.flush().await?;
    Ok(())
}

/// Read exactly one newline-delimited message from `reader`.
///
/// `buf` is a caller-owned carry buffer that persists across calls: bytes
/// read past a newline (which land in the same OS read) stay in `buf` for the
/// next call instead of being dropped. Start with an empty `Vec` and keep
/// reusing it for the lifetime of the connection. `max_len` caps a single
/// line; pass [`MAX_MESSAGE_BYTES`] for the default.
///
/// Returns `Ok(None)` on clean EOF at a line boundary. EOF mid-message (bytes
/// that do not form a complete JSON value) is [`IpcTransportError::Closed`]; a
/// final line without a trailing newline is parsed when complete. A line over
/// `max_len` is [`IpcTransportError::Oversized`].
pub async fn read_message<R>(
    reader: &mut R,
    buf: &mut Vec<u8>,
    max_len: usize,
) -> Result<Option<serde_json::Value>, IpcTransportError>
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;

    // `buf` only grows within one call (bytes are never removed from the
    // front), so the newline scan is incremental: each iteration examines only
    // the bytes appended since the last scan. Rescanning from the head each
    // time would make a near-`max_len` line quadratic, and 16 MiB lines are
    // explicitly supported.
    let mut scan_from = 0;
    loop {
        // A complete line is already in the carry buffer.
        if let Some(rel) = buf[scan_from..].iter().position(|&b| b == b'\n') {
            let pos = scan_from + rel;
            // Split off the terminator first so the tail (bytes after the
            // newline) survives as the carry for the next call, then pop the
            // `\n` off the line and move the line out of `buf`.
            let tail = buf.split_off(pos + 1);
            buf.pop(); // '\n'
            let line = std::mem::replace(buf, tail);
            return parse_line(&line, max_len).map(Some);
        }
        scan_from = buf.len();

        // Unterminated line exceeds the cap. Checked before each read so a
        // peer that never emits a newline cannot grow the buffer unbounded.
        if buf.len() > max_len {
            return Err(IpcTransportError::Oversized { len: buf.len(), max_len });
        }

        let mut chunk = [0u8; READ_CHUNK_BYTES];
        let n = reader.read(&mut chunk).await?;
        if n == 0 {
            // EOF. An empty carry means the peer closed cleanly at a line
            // boundary; a final unterminated line is parsed defensively, and
            // anything incomplete means the stream died mid-message.
            if buf.is_empty() {
                return Ok(None);
            }
            let line = std::mem::take(buf);
            if let Ok(value) = parse_line(&line, max_len) {
                return Ok(Some(value));
            }
            return Err(IpcTransportError::Closed);
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

/// Parse one line (newline already removed) into a JSON message.
fn parse_line(line: &[u8], max_len: usize) -> Result<serde_json::Value, IpcTransportError> {
    if line.len() > max_len {
        return Err(IpcTransportError::Oversized { len: line.len(), max_len });
    }
    // Tolerate a trailing `\r` (CRLF) defensively; `\n` is the only
    // terminator defined by the framing.
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    if line.is_empty() {
        return Err(IpcTransportError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "empty line is not a valid IPC message",
        )));
    }
    let text = std::str::from_utf8(line).map_err(|_| IpcTransportError::InvalidUtf8)?;
    serde_json::from_str(text).map_err(|error| {
        IpcTransportError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("malformed IPC message: {error}"),
        ))
    })
}

/// `client_hello`: the agent's opening handshake payload (ADR-60 D2).
///
/// The agent declares no whiteboard subscriptions here — the caller that
/// wires subscriptions (ADR-60 D3 spawn config) constructs `Handshake`
/// params directly with `subscriptions: Some(..)`.
/// `client_hello`: the supervisor's handshake request to a child agent
/// (ADR-60 D2/D3). `subscriptions` declares the whiteboard topic scopes the
/// supervisor will push to this agent (ADR-60 D3 push surface); `None` (or an
/// absent wire field, back-compat) means "no subscription push".
pub fn client_hello(
    agent_id: &str,
    capabilities: serde_json::Value,
    subscriptions: Option<Vec<WhiteboardScope>>,
) -> IpcParams {
    IpcParams::Handshake {
        protocol_version: PROTOCOL_VERSION.to_owned(),
        agent_id: agent_id.to_owned(),
        capabilities,
        subscriptions,
    }
}

/// `server_accept`: the supervisor acknowledges a matched protocol version.
pub fn server_accept() -> IpcResult {
    IpcResult::Handshake {
        protocol_version: PROTOCOL_VERSION.to_owned(),
        accepted: true,
        supervisor_version: PROTOCOL_VERSION.to_owned(),
    }
}

/// `server_reject`: the supervisor refuses the handshake.
///
/// The result shape carries only `accepted: false`; `reason` has no wire
/// field yet and is retained for the supervisor's peer-facing diagnostics in
/// a later slice.
pub fn server_reject(reason: &str) -> IpcResult {
    let _ = reason;
    IpcResult::Handshake {
        protocol_version: PROTOCOL_VERSION.to_owned(),
        accepted: false,
        supervisor_version: PROTOCOL_VERSION.to_owned(),
    }
}

/// Validate a peer's announced protocol version; exact match only (ADR-60 D2).
pub fn validate_version(peer: &str) -> Result<(), IpcError> {
    if peer == PROTOCOL_VERSION {
        Ok(())
    } else {
        Err(IpcError::new(
            IpcErrorCode::VersionMismatch,
            format!(
                "protocol version mismatch: peer speaks {peer}, supervisor speaks {PROTOCOL_VERSION}"
            ),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use concerto_sessions::whiteboard::WhiteboardKind;
    use serde_json::json;
    use std::collections::BTreeMap;

    /// Minimal `GateRequest` construction, matching `gate.rs` tests.
    fn gate_request(call_id: &str) -> GateRequest {
        GateRequest {
            call_id: call_id.to_owned(),
            agent_id: "agent-a".to_owned(),
            tool: "gate_test".to_owned(),
            input: json!({}),
            session_id: None,
            scope: "fs".to_owned(),
            plan_id: None,
            causation: None,
            base_versions: BTreeMap::new(),
        }
    }

    /// Minimal `GateOutcome`, matching the values the gate produces.
    fn gate_outcome() -> GateOutcome {
        GateOutcome {
            event_id: "call-1".to_owned(),
            gate_seq: 1,
            replayed: false,
            result: json!({ "ok": true }),
        }
    }

    /// Caller-attested event fields for `PublishEvent` params.
    fn new_event() -> NewWhiteboardEvent {
        NewWhiteboardEvent {
            event_id: "evt-1".to_owned(),
            agent_id: "agent-a".to_owned(),
            kind: WhiteboardKind::Decision,
            scope: "".to_owned(),
            session_id: None,
            plan_id: None,
            causation: None,
            payload: json!({ "note": "x" }),
            pre_image_hash: None,
            created_at: 1_700_000_000_000,
        }
    }

    /// Full stored row for `PublishEvent` results.
    fn stored_event() -> WhiteboardEvent {
        WhiteboardEvent {
            event_id: "evt-1".to_owned(),
            gate_seq: 1,
            agent_seq: 1,
            agent_id: "agent-a".to_owned(),
            kind: WhiteboardKind::Decision,
            scope: "".to_owned(),
            session_id: None,
            plan_id: None,
            causation: None,
            payload: json!({ "note": "x" }),
            content_hash: "deadbeef".to_owned(),
            pre_image_hash: None,
            created_at: 1_700_000_000_000,
        }
    }

    /// Serialize to JSON, decode back, and assert value equality.
    fn round_trip<T>(value: &T)
    where
        T: Serialize + for<'de> Deserialize<'de> + PartialEq + std::fmt::Debug,
    {
        let encoded = serde_json::to_string(value).expect("serialize");
        let decoded: T = serde_json::from_str(&encoded).expect("deserialize");
        assert_eq!(&decoded, value, "round-trip mismatch via {encoded}");
    }

    #[test]
    fn params_round_trip_execute_tool() {
        round_trip(&IpcParams::ExecuteTool { request: gate_request("call-1") });
    }

    #[test]
    fn params_round_trip_publish_event() {
        round_trip(&IpcParams::PublishEvent { event: new_event() });
    }

    #[test]
    fn params_round_trip_retrieve_memory() {
        round_trip(&IpcParams::RetrieveMemory {
            query: "file layout".to_owned(),
            agent_id: "agent-a".to_owned(),
            limit: 8,
        });
    }

    #[test]
    fn params_round_trip_heartbeat() {
        round_trip(&IpcParams::Heartbeat {
            agent_id: "agent-a".to_owned(),
            seq: 42,
            timestamp_ms: 1_700_000_000_000,
            status: "ready".to_owned(),
        });
    }

    #[test]
    fn params_round_trip_handshake() {
        round_trip(&IpcParams::Handshake {
            protocol_version: PROTOCOL_VERSION.to_owned(),
            agent_id: "agent-a".to_owned(),
            capabilities: json!({ "fs_write": true }),
            subscriptions: None,
        });
    }

    #[test]
    fn params_round_trip_handshake_with_subscriptions() {
        round_trip(&IpcParams::Handshake {
            protocol_version: PROTOCOL_VERSION.to_owned(),
            agent_id: "agent-a".to_owned(),
            capabilities: json!({ "fs_write": true }),
            subscriptions: Some(vec![WhiteboardScope {
                topics: vec![WhiteboardKind::WriteApplied, WhiteboardKind::Decision],
            }]),
        });
    }

    #[test]
    fn params_round_trip_whiteboard_slice() {
        round_trip(&IpcParams::WhiteboardSlice {
            subscription_id: "agent-b".to_owned(),
            events: vec![stored_event()],
            end_gate_seq: 1,
        });
    }

    #[test]
    fn params_round_trip_ack_whiteboard() {
        round_trip(&IpcParams::AckWhiteboard { end_gate_seq: 42 });
    }

    /// An older-style handshake JSON without the `subscriptions` field must
    /// still decode: `Option` is the wire-compat mechanism (absent → `None`),
    /// so a 0.1.0 handshake is accepted by a 0.2.0 reader just as it was by a
    /// 0.1.0 reader. Also proves the field is not required.
    #[test]
    fn handshake_without_subscriptions_deserializes_to_none() {
        let old_style = json!({
            "type": "handshake",
            "value": {
                "protocol_version": "0.1.0",
                "agent_id": "agent-a",
                "capabilities": { "fs_write": true }
            }
        });
        let params: IpcParams =
            serde_json::from_value(old_style).expect("older handshake must decode");
        match params {
            IpcParams::Handshake { subscriptions, protocol_version, .. } => {
                assert_eq!(subscriptions, None, "absent wire field deserializes to None");
                assert_eq!(protocol_version, "0.1.0");
            }
            other => panic!("expected Handshake, got {other:?}"),
        }
    }

    #[test]
    fn result_round_trip_execute_tool() {
        round_trip(&IpcResult::ExecuteTool { outcome: gate_outcome() });
    }

    #[test]
    fn result_round_trip_publish_event() {
        round_trip(&IpcResult::PublishEvent { stored: stored_event() });
    }

    #[test]
    fn result_round_trip_retrieve_memory() {
        round_trip(&IpcResult::RetrieveMemory {
            chunks: vec![MemoryChunk {
                text: "gate writes are WAL-before-execute".to_owned(),
                kind: "fact".to_owned(),
                score: 0.93,
                source_event_id: Some("evt-1".to_owned()),
            }],
        });
    }

    #[test]
    fn result_round_trip_heartbeat() {
        round_trip(&IpcResult::Heartbeat { accepted: true });
    }

    #[test]
    fn result_round_trip_handshake() {
        round_trip(&IpcResult::Handshake {
            protocol_version: PROTOCOL_VERSION.to_owned(),
            accepted: true,
            supervisor_version: "concerto 0.1.0".to_owned(),
        });
    }

    #[test]
    fn result_round_trip_ack_whiteboard() {
        round_trip(&IpcResult::AckWhiteboard { end_gate_seq: 42 });
    }

    #[test]
    fn new_methods_have_kebab_case_wire_names() {
        assert_eq!(IpcMethod::WhiteboardSlice.as_str(), "whiteboard-slice");
        assert_eq!(IpcMethod::AckWhiteboard.as_str(), "ack-whiteboard");
        assert_eq!(
            serde_json::to_string(&IpcMethod::WhiteboardSlice).expect("serialize"),
            "\"whiteboard-slice\""
        );
        assert_eq!(
            serde_json::to_string(&IpcMethod::AckWhiteboard).expect("serialize"),
            "\"ack-whiteboard\""
        );
    }

    #[test]
    fn params_and_result_round_trip_list_tools() {
        round_trip(&IpcParams::ListTools { agent_id: "agent-a".to_owned() });
        let definition = ToolDefinition {
            name: "write_file".to_owned(),
            description: "write a file".to_owned(),
            parameters: serde_json::json!({ "type": "object" }),
        };
        let result = IpcResult::ListTools { tools: vec![definition] };
        round_trip(&result);
        let encoded = serde_json::to_value(&result).expect("encode");
        let decoded: IpcResult = serde_json::from_value(encoded).expect("decode");
        match decoded {
            IpcResult::ListTools { tools } => {
                assert_eq!(tools.len(), 1);
                assert_eq!(tools[0].name, "write_file");
            }
            other => panic!("wrong variant decoded: {other:?}"),
        }
    }

    #[test]
    fn request_response_pair_round_trip() {
        let request = IpcRequest {
            jsonrpc: "2.0".to_owned(),
            id: 7,
            method: IpcMethod::ExecuteTool,
            params: IpcParams::ExecuteTool { request: gate_request("call-1") },
        };
        let response = IpcResponse {
            jsonrpc: "2.0".to_owned(),
            id: 7,
            result: Some(IpcResult::ExecuteTool { outcome: gate_outcome() }),
            error: None,
        };
        round_trip(&request);
        round_trip(&response);
    }

    #[test]
    fn error_code_numeric_mapping() {
        let codes = [
            (IpcErrorCode::ParseError, -32700),
            (IpcErrorCode::InvalidRequest, -32600),
            (IpcErrorCode::MethodNotFound, -32601),
            (IpcErrorCode::InvalidParams, -32602),
            (IpcErrorCode::Internal, -32603),
            (IpcErrorCode::VersionMismatch, -32000),
            (IpcErrorCode::GateDenied, -32001),
            (IpcErrorCode::GateError, -32002),
            (IpcErrorCode::Cancelled, -32003),
            (IpcErrorCode::Stopped, -32004),
        ];
        for (code, number) in codes {
            // Serializes as its numeric discriminant (serde as i32).
            assert_eq!(i32::from(code), number);
            assert_eq!(
                serde_json::to_string(&code).expect("serialize"),
                format!("{number}"),
                "{code:?} must serialize as its integer code"
            );
            // Deserializes back from the numeric wire form.
            let decoded: IpcErrorCode =
                serde_json::from_str(&format!("{number}")).expect("deserialize");
            assert_eq!(decoded, code);
        }
        // Round-trip the whole error object too.
        round_trip(&IpcError::new(IpcErrorCode::GateDenied, "blocked"));
    }

    #[test]
    fn from_gate_maps_every_variant() {
        let cases = [
            (
                GateError::Denied { event_id: "call-1".to_owned(), reason: "deny".to_owned() },
                IpcErrorCode::GateDenied,
                "deny",
            ),
            (GateError::Policy("engine down".to_owned()), IpcErrorCode::GateError, "policy"),
            (GateError::Whiteboard("log full".to_owned()), IpcErrorCode::GateError, "whiteboard"),
            (GateError::Execution("tool oom".to_owned()), IpcErrorCode::GateError, "execution"),
            (GateError::PreImage("read failed".to_owned()), IpcErrorCode::GateError, "pre-image"),
            (GateError::Cancelled, IpcErrorCode::Cancelled, "cancelled"),
            (
                GateError::Conflict {
                    event_id: "call-9".to_owned(),
                    reason: "base_version mismatch".to_owned(),
                },
                IpcErrorCode::Conflict,
                "optimistic conflict",
            ),
            (
                GateError::InvalidRequest("bad call_id".to_owned()),
                IpcErrorCode::GateError,
                "invalid request",
            ),
        ];
        for (gate, expected_code, message_fragment) in cases {
            let error = IpcError::from_gate(&gate);
            assert_eq!(error.code, expected_code, "{gate:?}");
            assert!(
                error.message.contains(message_fragment),
                "{gate:?} message {:?} missing fragment {message_fragment:?}",
                error.message
            );
            assert!(error.data.is_none(), "{gate:?} maps with no structured data");
        }
    }

    // --- S3b: framing + handshake helpers ---

    fn notification_value() -> serde_json::Value {
        serde_json::to_value(IpcNotification {
            jsonrpc: "2.0".to_owned(),
            method: IpcMethod::Heartbeat,
            params: IpcParams::Heartbeat {
                agent_id: "agent-a".to_owned(),
                seq: 1,
                timestamp_ms: 1_700_000_000_000,
                status: "ready".to_owned(),
            },
        })
        .expect("serialize notification")
    }

    async fn read_from(
        data: &[u8],
        max_len: usize,
    ) -> Result<Option<serde_json::Value>, IpcTransportError> {
        let mut reader: &[u8] = data;
        let mut buf = Vec::new();
        read_message(&mut reader, &mut buf, max_len).await
    }

    #[tokio::test]
    async fn frame_round_trip_notification() {
        let original = notification_value();
        let mut wire = Vec::new();
        write_message(&mut wire, &original).await.expect("write frame");
        assert!(wire.ends_with(b"\n"));
        let parsed = read_from(&wire, MAX_MESSAGE_BYTES)
            .await
            .expect("read frame")
            .expect("not EOF at message boundary");
        assert_eq!(parsed, original);
        let decoded: IpcNotification = serde_json::from_value(parsed).expect("decode notification");
        assert_eq!(decoded.method, IpcMethod::Heartbeat);
    }

    #[tokio::test]
    async fn two_messages_in_one_buffer_parse_as_two() {
        let mut wire = Vec::new();
        write_message(&mut wire, &json!({ "jsonrpc": "2.0", "id": 1, "method": "heartbeat" }))
            .await
            .expect("write message 1");
        write_message(&mut wire, &json!({ "jsonrpc": "2.0", "id": 2, "method": "heartbeat" }))
            .await
            .expect("write message 2");
        let mut reader: &[u8] = &wire;
        let mut buf = Vec::new();
        let first =
            read_message(&mut reader, &mut buf, MAX_MESSAGE_BYTES).await.expect("read message 1");
        let second =
            read_message(&mut reader, &mut buf, MAX_MESSAGE_BYTES).await.expect("read message 2");
        assert_eq!(first.expect("message 1").get("id"), Some(&json!(1)));
        assert_eq!(second.expect("message 2").get("id"), Some(&json!(2)));
        assert!(read_message(&mut reader, &mut buf, MAX_MESSAGE_BYTES)
            .await
            .expect("read after both")
            .is_none());
    }

    #[tokio::test]
    async fn message_without_trailing_newline_parses() {
        let parsed = read_from(
            b"{\"jsonrpc\":\"2.0\",\"id\":9,\"method\":\"heartbeat\"}",
            MAX_MESSAGE_BYTES,
        )
        .await
        .expect("read frame")
        .expect("not EOF");
        assert_eq!(parsed.get("id"), Some(&json!(9)));
    }

    #[tokio::test]
    async fn eof_mid_message_is_closed() {
        let err = read_from(b"{\"jsonrpc\":\"2.0\",\"method\":\"heartb", MAX_MESSAGE_BYTES)
            .await
            .expect_err("truncated message must fail");
        assert!(matches!(err, IpcTransportError::Closed), "got {err:?}");
    }

    #[tokio::test]
    async fn empty_line_is_a_parse_error() {
        // Decision: a blank line is malformed frame content, not a closed
        // stream. It surfaces as `Io(InvalidData)` so a caller can reply with
        // a JSON-RPC parse/invalid-request error and keep reading.
        let err = read_from(b"\n", MAX_MESSAGE_BYTES).await.expect_err("empty line must fail");
        assert!(
            matches!(&err, IpcTransportError::Io(error) if error.kind() == std::io::ErrorKind::InvalidData),
            "got {err:?}"
        );
    }

    #[tokio::test]
    async fn oversized_line_is_rejected() {
        let mut line = vec![b'a'; MAX_MESSAGE_BYTES + 1];
        line.push(b'\n');
        let err = read_from(&line, MAX_MESSAGE_BYTES).await.expect_err("oversized line must fail");
        match err {
            IpcTransportError::Oversized { len, max_len } => {
                assert!(len > max_len, "len {len} must exceed cap {max_len}");
                assert_eq!(max_len, MAX_MESSAGE_BYTES);
            }
            other => panic!("expected Oversized, got {other:?}"),
        }
    }

    #[test]
    fn validate_version_exact_match_is_ok() {
        assert!(validate_version(PROTOCOL_VERSION).is_ok());
    }

    #[test]
    fn validate_version_mismatch_is_version_mismatch_error() {
        let error = validate_version("9.9.9").expect_err("mismatch must fail");
        assert_eq!(error.code, IpcErrorCode::VersionMismatch);
        assert_eq!(i32::from(error.code), -32000);
        assert!(error.message.contains("9.9.9"), "message {:?}", error.message);
        assert!(error.message.contains(PROTOCOL_VERSION), "message {:?}", error.message);
    }

    #[test]
    fn client_hello_shape() {
        let value =
            serde_json::to_value(client_hello("agent-a", json!({ "fs_write": true }), None))
                .expect("serialize handshake params");
        assert_eq!(value["type"], json!("handshake"));
        assert_eq!(value["value"]["protocol_version"], json!(PROTOCOL_VERSION));
        assert_eq!(value["value"]["agent_id"], json!("agent-a"));
        assert_eq!(value["value"]["capabilities"], json!({ "fs_write": true }));
        // No subscriptions declared: the wire field round-trips null for "no
        // subscriptions" and is optional on decode.
        assert_eq!(value["value"]["subscriptions"], json!(null));
        // Method names are kebab-case on the wire too.
        assert_eq!(
            serde_json::to_string(&IpcMethod::Handshake).expect("serialize method"),
            "\"handshake\""
        );
    }

    #[test]
    fn client_hello_declares_subscriptions_when_configured() {
        let value = serde_json::to_value(client_hello(
            "agent-a",
            json!({}),
            Some(vec![WhiteboardScope { topics: vec![WhiteboardKind::Decision] }]),
        ))
        .expect("serialize handshake params");
        assert_eq!(
            value["value"]["subscriptions"],
            json!([{ "topics": ["decision"] }]),
            "configured scopes are declared as a non-null array"
        );
    }

    #[test]
    fn server_accept_shape() {
        let value = serde_json::to_value(server_accept()).expect("serialize handshake result");
        assert_eq!(value["type"], json!("handshake"));
        assert_eq!(value["value"]["protocol_version"], json!(PROTOCOL_VERSION));
        assert_eq!(value["value"]["accepted"], json!(true));
        assert_eq!(value["value"]["supervisor_version"], json!(PROTOCOL_VERSION));
    }

    #[test]
    fn server_reject_shape() {
        let value = serde_json::to_value(server_reject("version mismatch"))
            .expect("serialize handshake result");
        assert_eq!(value["type"], json!("handshake"));
        assert_eq!(value["value"]["protocol_version"], json!(PROTOCOL_VERSION));
        assert_eq!(value["value"]["accepted"], json!(false));
        assert_eq!(value["value"]["supervisor_version"], json!(PROTOCOL_VERSION));
    }
}
