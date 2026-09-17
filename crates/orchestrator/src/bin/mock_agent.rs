//! Mock-agent fixture binary for supervisor integration tests — ADR-60 S4a.
//!
//! The supervisor (a later chunk of slice S4) spawns this binary as a child
//! and drives it over stdio with the versioned supervisor/agent protocol
//! defined in [`concerto_orchestrator::ipc`]: newline-delimited JSON-RPC 2.0,
//! one compact message per line.
//!
//! This fixture is deliberately dumb and deterministic:
//!
//! - A `handshake` request is always answered with [`ipc::server_accept`].
//!   Version negotiation is the supervisor's job; the mock has nothing to
//!   probe, so `accepted` is always `true`.
//! - A `heartbeat` request is acknowledged (`accepted: true`); a `heartbeat`
//!   notification is ignored (JSON-RPC notifications get no response).
//! - Any other request is rejected with `-32601 MethodNotFound`, so an
//!   unexpected supervisor call fails loudly instead of hanging the test.
//! - A message that cannot be decoded as a request/notification is answered
//!   with `-32700 ParseError` (`id: 0`).
//! - Reply-shaped messages (an `id` with a `result`/`error` and no `method`)
//!   are answered with silence — they are the supervisor's responses to the
//!   requests this fixture emits, and nothing needs to read them back.
//!
//! The loop is bounded to [`MAX_MESSAGES`] inbound messages, then the process
//! exits 0 — a runaway test session (e.g. a supervisor that never closes
//! stdin) cannot keep this fixture alive forever.
//!
//! ## Exit codes
//!
//! - `0` — graceful: clean EOF on stdin, EOF mid-message, [`MAX_MESSAGES`]
//!   reached, or the stdout pipe closed (supervisor gone).
//! - `1` — fatal framing/internal failure that would wedge the test loop
//!   (oversized line, invalid UTF-8, genuine stdin I/O error).
//!
//! ## Test knobs (environment)
//!
//! - `MOCK_AGENT_EXIT_AFTER=N` — exit after replying to N inbound messages
//!   (the handshake counts as the first). Lets a test create a child that
//!   terminates right after startup, exercising the supervisor's clean-exit
//!   and restart paths. Default: never exit early.
//! - `MOCK_AGENT_EXIT_STATUS=N` — the exit code used when
//!   `MOCK_AGENT_EXIT_AFTER` fires (default `0`). `0` is the ADR-60 S5
//!   terminal "task completed" exit; non-zero is treated as a crash.
//! - `MOCK_AGENT_HEARTBEATS=N` — immediately after the handshake reply,
//!   emit N `heartbeat` notifications (sequences `1..=N`, status `ready`)
//!   so a test can observe the supervisor recording liveness. Default:
//!   no outbound heartbeats.
//! - `MOCK_AGENT_TOOL_REQUESTS=N` — after the handshake reply, emit N
//!   `execute-tool` requests (wire ids `100..`, call ids `mock-call-N`,
//!   tool `gate_test`). The wire `agent_id` is deliberately `spoofed-agent`
//!   — never the registered id — so tests can prove the supervisor binds
//!   attribution to the registered process. Default: none.
//! - `MOCK_AGENT_PUBLISH=N` — after the handshake reply, emit N
//!   `publish-event` requests (wire ids `200..`, event ids `mock-event-N`,
//!   kind `finding`), also tagged `spoofed-agent`. Default: none.
//! - `MOCK_AGENT_RETRIEVE=N` — after the handshake reply, emit N
//!   `retrieve-memory` requests (wire ids `300..`, query
//!   `supervisor memory query`, limit 3). Default: none.
//! - `MOCK_AGENT_STORE=N` — after the handshake reply, emit N `store-memory`
//!   requests (wire ids `500..`, content `mock-store-N`, kind `fact`,
//!   no expiry). Default: none.
//! - `MOCK_AGENT_INVALIDATE=N` — after the handshake reply, emit N
//!   `invalidate-memory` requests (wire ids `600..`, a freshly generated
//!   ULID each). Default: none.
//!
//! ### Whiteboard-slice consumption (ADR-60 D3 crash-window tests)
//!
//! The stock fixture ignores `whiteboard-slice` notifications. Two optional
//! knobs turn it into a consuming subscriber for the Ack→cursor window:
//!
//! - `MOCK_AGENT_ACK_SLICES=1` — on every received slice, emit an
//!   `ack-whiteboard` request for its `end_gate_seq` (wire ids `400..`,
//!   fire-and-forget).
//! - `MOCK_AGENT_CRASH_ONCE_FILE=<path>` — on the FIRST received slice of
//!   this fixture lineage: create `<path>` and exit 1 BEFORE acking,
//!   simulating a subscriber dying between slice delivery and cursor update.
//!   The file is cross-incarnation memory: the supervisor's restarted child
//!   sees the marker exists and proceeds to consume + ack instead of
//!   crashing again.
//!
//! Outbound emission order is deterministic: heartbeats, then tool requests,
//! then publishes, then retrieves, then stores, then invalidates — all in one
//! burst after the handshake.
//!
//! Every outbound frame is written synchronously and flushed before the
//! helper returns (blocking stdio, mirroring the supervisor's parent-side
//! writes through [`ipc::serialize_frame`]). Delivery must not depend on
//! `tokio::io::Stdout`'s background flusher: its scheduling delays were
//! observed to leave the whole post-handshake burst undelivered for a child's
//! lifetime under load, wedging supervisor-loop tests.

use std::io::Write as IoWrite;
use std::time::{SystemTime, UNIX_EPOCH};

use concerto_core::ids::Ulid;
use concerto_core::memory::ChunkType;
use concerto_orchestrator::gate::GateRequest;
use concerto_orchestrator::ipc::{
    self, IpcError, IpcErrorCode, IpcMethod, IpcNotification, IpcParams, IpcRequest, IpcResponse,
    IpcResult, IpcTransportError, MemoryEntryWire, MAX_MESSAGE_BYTES,
};
use concerto_sessions::whiteboard::{NewWhiteboardEvent, WhiteboardKind};
use serde_json::{json, Value};
use std::collections::BTreeMap;

/// Wire `agent_id` this fixture sends on all write-path requests — never the
/// registered id. Tests assert the supervisor binds attribution to the
/// registered process instead of trusting the wire (ADR-60 D4).
const SPOOFED_AGENT_ID: &str = "spoofed-agent";

/// Maximum number of inbound messages this fixture processes before exiting.
/// Guards runaway test sessions: a supervisor that keeps talking without
/// closing stdin still cannot keep the fixture alive forever.
const MAX_MESSAGES: usize = 1024;

#[tokio::main]
async fn main() {
    // No `unwrap`/`expect` anywhere in this fixture: every fallible step is
    // matched explicitly and mapped onto a process exit code.
    std::process::exit(run().await);
}

/// Process messages until EOF, the bounded-session cap, or a fatal failure.
/// Returns the process exit code.
async fn run() -> i32 {
    // Test knobs (see module docs).
    let exit_at = std::env::var("MOCK_AGENT_EXIT_AFTER")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(u64::MAX);
    let exit_status = std::env::var("MOCK_AGENT_EXIT_STATUS")
        .ok()
        .and_then(|v| v.parse::<i32>().ok())
        .unwrap_or(0);
    let heartbeats = knob("MOCK_AGENT_HEARTBEATS");
    let tool_requests = knob("MOCK_AGENT_TOOL_REQUESTS");
    let publishes = knob("MOCK_AGENT_PUBLISH");
    let retrieves = knob("MOCK_AGENT_RETRIEVE");
    let stores = knob("MOCK_AGENT_STORE");
    let invalidates = knob("MOCK_AGENT_INVALIDATE");
    // When set, every JSON line received on stdin is appended (re-serialized)
    // to this file. Test observability only: lets a supervisor e2e assert
    // exactly what the supervisor wrote on the wire (e.g. `whiteboard-slice`
    // notifications).
    let log_path = std::env::var("MOCK_AGENT_LOG_FILE").ok();
    // Whiteboard-slice consumption knobs (module docs: crash-window tests).
    let ack_slices = std::env::var("MOCK_AGENT_ACK_SLICES").is_ok();
    let crash_once_file = std::env::var("MOCK_AGENT_CRASH_ONCE_FILE").ok();
    let mut next_ack_id = 400u64;

    let mut stdin = tokio::io::stdin();
    // Synchronous frame sink (see module docs): every write is flushed
    // before returning, so delivery never depends on a background flusher.
    let mut stdout = std::io::stdout();
    // Caller-owned carry buffer reused across `read_message` calls (see the
    // framing contract in `ipc.rs`).
    let mut buf = Vec::new();

    // Replies emitted so far (drives `MOCK_AGENT_EXIT_AFTER`).
    let mut replies = 0u64;
    // Outbound traffic already emitted (once, right after the handshake).
    let mut outbound_sent = false;

    for _ in 0..MAX_MESSAGES {
        match ipc::read_message(&mut stdin, &mut buf, MAX_MESSAGE_BYTES).await {
            // Clean EOF at a line boundary, or EOF mid-message: the
            // supervisor is shutting down (ADR-60 D1: close stdin → child
            // exits → reap). Nothing left to answer on either path.
            Ok(None) => return 0,
            Err(IpcTransportError::Closed) => return 0,
            // Malformed frame content (empty line / invalid JSON line):
            // `ipc.rs` surfaces these as `Io(InvalidData)` so the caller can
            // reply with a JSON-RPC parse error and keep reading.
            Err(IpcTransportError::Io(error))
                if error.kind() == std::io::ErrorKind::InvalidData =>
            {
                if let Some(code) = write_reply(&mut stdout, &parse_error_response()) {
                    return code;
                }
                replies += 1;
                if replies >= exit_at {
                    exit_before_dropping().await;
                    return exit_status;
                }
            }
            // Framing violations that are unrecoverable for this loop (the
            // overlong carry buffer would re-trigger `Oversized` forever):
            // fail loudly instead of wedging the test session.
            Err(error @ (IpcTransportError::Oversized { .. } | IpcTransportError::InvalidUtf8)) => {
                eprintln!("orchestrator-mock-agent: unrecoverable framing error: {error}");
                return 1;
            }
            // Genuine stdin I/O failure (not a framing marker).
            Err(error @ IpcTransportError::Io(_)) => {
                eprintln!("orchestrator-mock-agent: stdin I/O error: {error}");
                return 1;
            }
            Ok(Some(value)) => {
                if let Some(path) = &log_path {
                    if let Ok(line) = serde_json::to_string(&value) {
                        if let Ok(mut file) =
                            std::fs::OpenOptions::new().create(true).append(true).open(path)
                        {
                            let _ = std::io::Write::write_all(&mut file, line.as_bytes());
                            let _ = std::io::Write::write_all(&mut file, b"\n");
                        }
                    }
                }
                // ADR-60 D3 crash-window consumption: a `whiteboard-slice`
                // notification is either the deterministic crash (first slice
                // of the lineage: die before acking — the supervisor's cursor
                // has not advanced, so redelivery is guaranteed) or, on the
                // restarted incarnation, consumed + acked via a
                // fire-and-forget `ack-whiteboard` request.
                if let Some(end_gate_seq) = whiteboard_slice_end(&value) {
                    if let Some(marker) = &crash_once_file {
                        if !std::path::Path::new(marker).exists() {
                            if let Err(error) = std::fs::write(marker, b"slice-before-ack") {
                                eprintln!(
                                    "orchestrator-mock-agent: crash marker write failed: {error}"
                                );
                                return 1;
                            }
                            exit_before_dropping().await;
                            return 1;
                        }
                    }
                    if ack_slices {
                        let ack = IpcRequest {
                            jsonrpc: "2.0".to_owned(),
                            id: next_ack_id,
                            method: IpcMethod::AckWhiteboard,
                            params: IpcParams::AckWhiteboard { end_gate_seq },
                        };
                        next_ack_id += 1;
                        match serde_json::to_value(&ack) {
                            Ok(ack_value) => {
                                if let Some(code) = write_json(&mut stdout, &ack_value) {
                                    return code;
                                }
                            }
                            Err(error) => {
                                eprintln!(
                                    "orchestrator-mock-agent: failed to serialize ack: {error}"
                                );
                                return 1;
                            }
                        }
                    }
                    continue;
                }
                match handle_message_typed(value) {
                    MaybeReply::Write(response, agent_id) => {
                        if let Some(code) = write_reply(&mut stdout, &response) {
                            return code;
                        }
                        replies += 1;
                        if replies >= exit_at {
                            exit_before_dropping().await;
                            return exit_status;
                        }
                        // After the handshake reply, offer the outbound
                        // traffic the test asked for (in-line, so this
                        // fixture needs no concurrent writer).
                        if !outbound_sent {
                            outbound_sent = true;
                            if let Some(agent_id) = agent_id {
                                if let Some(code) = write_outbound(
                                    &mut stdout,
                                    &agent_id,
                                    heartbeats,
                                    tool_requests,
                                    publishes,
                                    retrieves,
                                    stores,
                                    invalidates,
                                ) {
                                    return code;
                                }
                            }
                        }
                    }
                    MaybeReply::Silent => {}
                }
            }
        }
    }
    // Bounded-session cap reached: exit cleanly.
    0
}

/// What dispatching one inbound message produced.
enum MaybeReply {
    /// A response to write, plus the agent id from a handshake request (if
    /// the message was one), for the outbound-heartbeat knob. Boxed: an
    /// [`IpcResponse`] carries the full result union and is large.
    Write(Box<IpcResponse>, Option<String>),
    /// A notification that is legitimately answered with silence.
    Silent,
}

/// Wrapper kept for the unit tests: the reply a message deserves, or `None`
/// for properly-silent notifications.
#[cfg(test)]
fn handle_message(value: Value) -> Option<IpcResponse> {
    match handle_message_typed(value) {
        MaybeReply::Write(response, _) => Some(*response),
        MaybeReply::Silent => None,
    }
}

/// Read one integer test knob from the environment (absent or unparseable →
/// `0`).
fn knob(name: &str) -> u64 {
    std::env::var(name).ok().and_then(|v| v.parse::<u64>().ok()).unwrap_or(0)
}

/// Defensive pause before exiting right after a reply.
///
/// Replies and bursts are written synchronously and flushed before the
/// writing helper returns, so normally nothing is left in flight. The short
/// sleep is pure defense-in-depth for fast-exit paths
/// (`MOCK_AGENT_EXIT_AFTER`, crash-window knob): an exotic stdout
/// redirection that buffers again must never lose the fixture's final bytes
/// to [`std::process::exit`].
async fn exit_before_dropping() {
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
}

/// Emit the configured outbound traffic after the handshake reply, in
/// deterministic order: heartbeat notifications, then `execute-tool`
/// requests (ids `100..`), `publish-event` requests (`200..`),
/// `retrieve-memory` requests (`300..`), `store-memory` requests (`500..`),
/// and `invalidate-memory` requests (`600..`).
///
/// Mirrors `write_reply`'s error contract: `Some(exit_code)` on failure.
#[allow(clippy::too_many_arguments)]
fn write_outbound<W>(
    writer: &mut W,
    agent_id: &str,
    heartbeats: u64,
    tools: u64,
    publishes: u64,
    retrieves: u64,
    stores: u64,
    invalidates: u64,
) -> Option<i32>
where
    W: IoWrite,
{
    for seq in 1..=heartbeats {
        let notification = IpcNotification {
            jsonrpc: "2.0".to_owned(),
            method: IpcMethod::Heartbeat,
            params: IpcParams::Heartbeat {
                agent_id: agent_id.to_owned(),
                seq,
                timestamp_ms: now_millis(),
                status: "ready".to_owned(),
            },
        };
        let Some(value) = to_value(&notification) else {
            return Some(1);
        };
        if let Some(code) = write_json(writer, &value) {
            return Some(code);
        }
    }
    for index in 0..tools {
        let request = IpcRequest {
            jsonrpc: "2.0".to_owned(),
            id: 100 + index,
            method: IpcMethod::ExecuteTool,
            params: IpcParams::ExecuteTool {
                request: GateRequest {
                    call_id: format!("mock-call-{index}"),
                    // Deliberately spoofed: the supervisor must bind
                    // attribution to the registered process (ADR-60 D4).
                    agent_id: SPOOFED_AGENT_ID.to_owned(),
                    tool: "gate_test".to_owned(),
                    input: json!({ "probe": index }),
                    session_id: None,
                    scope: "fs".to_owned(),
                    plan_id: None,
                    causation: Some("mock-cause".to_owned()),
                    // No concurrency claims by default. Caller-declared
                    // `base_versions` travel inside the scripted turn JSON
                    // (the tool-call arguments), not via a separate knob.
                    base_versions: BTreeMap::new(),
                },
            },
        };
        let Some(value) = to_value(&request) else {
            return Some(1);
        };
        if let Some(code) = write_json(writer, &value) {
            return Some(code);
        }
    }
    for index in 0..publishes {
        let request = IpcRequest {
            jsonrpc: "2.0".to_owned(),
            id: 200 + index,
            method: IpcMethod::PublishEvent,
            params: IpcParams::PublishEvent {
                event: NewWhiteboardEvent {
                    event_id: format!("mock-event-{index}"),
                    // Spoofed like the tool requests.
                    agent_id: SPOOFED_AGENT_ID.to_owned(),
                    kind: WhiteboardKind::Finding,
                    scope: "mock".to_owned(),
                    session_id: None,
                    plan_id: None,
                    causation: Some("mock-cause".to_owned()),
                    payload: json!({ "seq": index }),
                    pre_image_hash: None,
                    created_at: now_millis(),
                },
            },
        };
        let Some(value) = to_value(&request) else {
            return Some(1);
        };
        if let Some(code) = write_json(writer, &value) {
            return Some(code);
        }
    }
    for index in 0..retrieves {
        let request = IpcRequest {
            jsonrpc: "2.0".to_owned(),
            id: 300 + index,
            method: IpcMethod::RetrieveMemory,
            params: IpcParams::RetrieveMemory {
                query: "supervisor memory query".to_owned(),
                agent_id: SPOOFED_AGENT_ID.to_owned(),
                limit: 3,
            },
        };
        let Some(value) = to_value(&request) else {
            return Some(1);
        };
        if let Some(code) = write_json(writer, &value) {
            return Some(code);
        }
    }
    for index in 0..stores {
        let request = IpcRequest {
            jsonrpc: "2.0".to_owned(),
            id: 500 + index,
            method: IpcMethod::StoreMemory,
            params: IpcParams::StoreMemory {
                // Content-only projection: the supervisor assigns the entry
                // id and binds project scoping at the spine (ADR-60 D6).
                entry: MemoryEntryWire {
                    content: format!("mock-store-{index}"),
                    chunk_type: ChunkType::Fact,
                    model_id: None,
                    model_version: None,
                    metadata: json!({ "seq": index }),
                    expires_at_ms: None,
                    created_at_ms: now_millis(),
                },
            },
        };
        let Some(value) = to_value(&request) else {
            return Some(1);
        };
        if let Some(code) = write_json(writer, &value) {
            return Some(code);
        }
    }
    for index in 0..invalidates {
        let request = IpcRequest {
            jsonrpc: "2.0".to_owned(),
            id: 600 + index,
            method: IpcMethod::InvalidateMemory,
            params: IpcParams::InvalidateMemory { memory_id: Ulid::new().to_string() },
        };
        let Some(value) = to_value(&request) else {
            return Some(1);
        };
        if let Some(code) = write_json(writer, &value) {
            return Some(code);
        }
    }
    None
}

/// Serialize a message, surfacing failures on stderr (exit-code contract
/// handled by the caller).
fn to_value(message: &impl serde::Serialize) -> Option<Value> {
    match serde_json::to_value(message) {
        Ok(value) => Some(value),
        Err(error) => {
            eprintln!("orchestrator-mock-agent: failed to serialize message: {error}");
            None
        }
    }
}

/// Unix epoch milliseconds (UTC).
fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

/// Serialize and write one JSON message line, flushed before returning.
///
/// Same error contract as `write_reply`: `Some(exit_code)` when the write
/// failed and the loop must stop — `0` when the supervisor's read end closed
/// (broken stdout pipe — graceful teardown), `1` on an internal failure.
/// `None` means the message is on the wire.
fn write_json<W>(writer: &mut W, value: &Value) -> Option<i32>
where
    W: IoWrite,
{
    sync_write_frame(writer, value)
}

/// Blocking-write one serialized frame and flush it before returning — the
/// fixture's single outbound funnel (the supervisor's parent-side writes use
/// the same `serialize_frame` + `write_all` + `flush` pattern).
///
/// Error contract shared by every writing helper: `Some(exit_code)` when the
/// loop must stop — `0` when the supervisor's read end closed (broken stdout
/// pipe — graceful teardown), `1` on any other internal failure. `None`
/// means the frame was delivered to the OS.
fn sync_write_frame<W>(writer: &mut W, value: &Value) -> Option<i32>
where
    W: IoWrite,
{
    let frame = match ipc::serialize_frame(value) {
        Ok(frame) => frame,
        Err(error) => {
            eprintln!("orchestrator-mock-agent: failed to frame message: {error}");
            return Some(1);
        }
    };
    match IoWrite::write_all(writer, &frame).and_then(|()| IoWrite::flush(writer)) {
        Ok(()) => None,
        Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => {
            eprintln!("orchestrator-mock-agent: write failed, supervisor gone: {error}");
            Some(0)
        }
        Err(error) => {
            eprintln!("orchestrator-mock-agent: stdout write failed: {error}");
            Some(1)
        }
    }
}

/// The `end_gate_seq` of a `whiteboard-slice` NOTIFICATION line, or `None`.
/// Shape-probed (not fully deserialized) on purpose: the fixture only needs
/// the ack coordinate, and a malformed slice must not crash the fixture
/// outside the deterministic crash window.
fn whiteboard_slice_end(value: &Value) -> Option<u64> {
    let method = value.get("method").and_then(|m| m.as_str())?;
    if method != "whiteboard-slice" {
        return None;
    }
    // Wire shape: params are internally tagged (`type`) with the payload
    // under `value`, so the ack coordinate lives at params.value.end_gate_seq.
    value
        .get("params")
        .and_then(|params| params.get("value"))
        .and_then(|payload| payload.get("end_gate_seq"))
        .and_then(|seq| seq.as_u64())
}

/// Dispatch one decoded message: requests first (a request also decodes as a
/// notification, so this order matters), then notifications, then the
/// parse-error fallback.
fn handle_message_typed(value: Value) -> MaybeReply {
    // Supervisor replies come back with an `id` and a `result`/`error`, no
    // `method`; nothing reads them back, so answer with silence. Checked
    // before request/notification decoding (a response fails both anyway).
    if value.get("method").is_none()
        && (value.get("result").is_some() || value.get("error").is_some())
    {
        return MaybeReply::Silent;
    }
    if let Ok(request) = serde_json::from_value::<IpcRequest>(value.clone()) {
        let agent_id = match &request.params {
            IpcParams::Handshake { agent_id, .. } => Some(agent_id.clone()),
            _ => None,
        };
        return MaybeReply::Write(Box::new(respond_to_request(&request)), agent_id);
    }
    if let Ok(notification) = serde_json::from_value::<IpcNotification>(value) {
        return match respond_to_notification(&notification) {
            Some(response) => MaybeReply::Write(Box::new(response), None),
            None => MaybeReply::Silent,
        };
    }
    MaybeReply::Write(Box::new(parse_error_response()), None)
}

/// Build the reply to a request, keyed by method.
fn respond_to_request(request: &IpcRequest) -> IpcResponse {
    let (result, error) = match request.method {
        IpcMethod::Handshake => (Some(ipc::server_accept()), None),
        IpcMethod::Heartbeat => (Some(IpcResult::Heartbeat { accepted: true }), None),
        // `ExecuteTool`/`PublishEvent`/`RetrieveMemory` are supervisor-side
        // responsibilities; this fixture agent supports none of them.
        _ => (
            None,
            Some(IpcError::new(IpcErrorCode::MethodNotFound, "mock agent: unsupported method")),
        ),
    };
    IpcResponse { jsonrpc: "2.0".to_owned(), id: request.id, result, error }
}

/// Acknowledge a `heartbeat` notification by staying silent (JSON-RPC
/// notifications get no response). Any other notification is protocol misuse
/// by the test driver, so surface it loudly (`MethodNotFound`, id `0`)
/// instead of silently dropping it.
fn respond_to_notification(notification: &IpcNotification) -> Option<IpcResponse> {
    match notification.method {
        IpcMethod::Heartbeat => None,
        _ => Some(IpcResponse {
            jsonrpc: "2.0".to_owned(),
            id: 0,
            result: None,
            error: Some(IpcError::new(
                IpcErrorCode::MethodNotFound,
                "mock agent: unsupported method",
            )),
        }),
    }
}

/// JSON-RPC `-32700` reply for a message that cannot be decoded into a
/// request or notification.
fn parse_error_response() -> IpcResponse {
    IpcResponse {
        jsonrpc: "2.0".to_owned(),
        id: 0,
        result: None,
        error: Some(IpcError::new(IpcErrorCode::ParseError, "mock agent: malformed message")),
    }
}

/// Serialize and write one response line, flushed before returning.
///
/// Returns `Some(exit_code)` when the write failed and the loop must stop:
/// `0` when the supervisor's read end closed (broken stdout pipe — graceful
/// teardown), `1` on an internal serialization failure. `None` means the
/// response is on the wire and the loop continues.
fn write_reply<W>(writer: &mut W, response: &IpcResponse) -> Option<i32>
where
    W: IoWrite,
{
    let value = match serde_json::to_value(response) {
        Ok(value) => value,
        Err(error) => {
            eprintln!("orchestrator-mock-agent: failed to serialize response: {error}");
            return Some(1);
        }
    };
    sync_write_frame(writer, &value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use concerto_orchestrator::ipc::{IpcParams, PROTOCOL_VERSION};
    use serde_json::json;

    fn heartbeat_params() -> IpcParams {
        IpcParams::Heartbeat {
            agent_id: "agent-a".to_owned(),
            seq: 1,
            timestamp_ms: 1_700_000_000_000,
            status: "ready".to_owned(),
        }
    }

    fn request_value(id: u64, method: IpcMethod, params: IpcParams) -> Value {
        serde_json::to_value(IpcRequest { jsonrpc: "2.0".to_owned(), id, method, params })
            .expect("serialize request")
    }

    fn notification_value(method: IpcMethod, params: IpcParams) -> Value {
        serde_json::to_value(IpcNotification { jsonrpc: "2.0".to_owned(), method, params })
            .expect("serialize notification")
    }

    #[test]
    fn handshake_request_answers_with_server_accept() {
        let request = request_value(
            7,
            IpcMethod::Handshake,
            IpcParams::Handshake {
                protocol_version: PROTOCOL_VERSION.to_owned(),
                agent_id: "agent-a".to_owned(),
                capabilities: json!({ "fs_write": true }),
                subscriptions: None,
            },
        );
        let response = handle_message(request).expect("handshake must get a reply");
        assert_eq!(response.id, 7);
        assert!(response.error.is_none());
        let result = response.result.expect("handshake succeeds");
        assert!(matches!(result, IpcResult::Handshake { accepted: true, .. }));
    }

    #[test]
    fn heartbeat_request_is_acknowledged() {
        let response = handle_message(request_value(2, IpcMethod::Heartbeat, heartbeat_params()))
            .expect("heartbeat request must get a reply");
        assert_eq!(response.id, 2);
        assert!(response.error.is_none());
        assert_eq!(response.result, Some(IpcResult::Heartbeat { accepted: true }));
    }

    #[test]
    fn unsupported_method_request_gets_method_not_found() {
        // `PublishEvent` is a supervisor-side method; the fixture rejects it.
        let response =
            handle_message(request_value(3, IpcMethod::PublishEvent, heartbeat_params()))
                .expect("unsupported request must get a reply");
        assert_eq!(response.id, 3);
        assert!(response.result.is_none());
        let error = response.error.expect("unsupported method errors");
        assert_eq!(error.code, IpcErrorCode::MethodNotFound);
        assert_eq!(error.message, "mock agent: unsupported method");
    }

    #[test]
    fn memory_write_requests_get_method_not_found() {
        // `store-memory` / `invalidate-memory` are supervisor-side methods;
        // this agent-side fixture rejects both loudly (same contract as the
        // other unsupported supervisor methods).
        let store = request_value(
            9,
            IpcMethod::StoreMemory,
            IpcParams::StoreMemory {
                entry: MemoryEntryWire {
                    content: "mock".to_owned(),
                    chunk_type: ChunkType::Fact,
                    model_id: None,
                    model_version: None,
                    metadata: json!({}),
                    expires_at_ms: None,
                    created_at_ms: 1_700_000_000_000,
                },
            },
        );
        let response = handle_message(store).expect("store-memory must get a reply");
        assert_eq!(response.id, 9);
        assert_eq!(
            response.error.expect("store-memory rejected").code,
            IpcErrorCode::MethodNotFound
        );

        let invalidate = request_value(
            10,
            IpcMethod::InvalidateMemory,
            IpcParams::InvalidateMemory { memory_id: "01ARZ3NDEKTSV4RRFFQ69G5FAV".to_owned() },
        );
        let response = handle_message(invalidate).expect("invalidate-memory must get a reply");
        assert_eq!(response.id, 10);
        assert_eq!(
            response.error.expect("invalidate-memory rejected").code,
            IpcErrorCode::MethodNotFound
        );
    }

    /// The `MOCK_AGENT_STORE` / `MOCK_AGENT_INVALIDATE` knobs emit the memory
    /// write-path requests in the documented id bands (`500..` / `600..`)
    /// with the correct wire shapes.
    #[test]
    fn outbound_emits_store_and_invalidate_requests() {
        let mut wire: Vec<u8> = Vec::new();
        let result = write_outbound(&mut wire, "agent-a", 0, 0, 0, 0, 2, 1);
        assert!(result.is_none(), "emission must succeed");
        let lines: Vec<Value> = String::from_utf8(wire)
            .expect("frames are utf-8")
            .lines()
            .map(|line| serde_json::from_str(line).expect("each frame parses"))
            .collect();
        assert_eq!(lines.len(), 3, "two stores then one invalidate");

        for (index, value) in lines[..2].iter().enumerate() {
            assert_eq!(value["id"], json!(500 + index as u64));
            assert_eq!(value["method"], json!("store-memory"));
            let entry = &value["params"]["value"]["entry"];
            assert_eq!(entry["content"], json!(format!("mock-store-{index}")));
            assert_eq!(entry["chunk_type"], json!("Fact"));
            assert_eq!(entry["expires_at_ms"], json!(null), "no expiry by default");
            assert!(entry["created_at_ms"].as_i64().is_some(), "creation time is millis");
        }
        assert_eq!(lines[2]["id"], json!(600));
        assert_eq!(lines[2]["method"], json!("invalidate-memory"));
        assert!(
            lines[2]["params"]["value"]["memory_id"].is_string(),
            "invalidate carries a ULID string"
        );
    }

    #[test]
    fn heartbeat_notification_is_ignored() {
        let response = handle_message(notification_value(IpcMethod::Heartbeat, heartbeat_params()));
        assert!(response.is_none(), "notifications must not be answered");
    }

    #[test]
    fn unexpected_notification_gets_method_not_found_with_id_zero() {
        let response =
            handle_message(notification_value(IpcMethod::PublishEvent, heartbeat_params()))
                .expect("unexpected notification still surfaces loudly");
        assert_eq!(response.id, 0);
        assert!(response.result.is_none());
        assert_eq!(
            response.error.expect("unexpected notification errors").code,
            IpcErrorCode::MethodNotFound
        );
    }

    #[test]
    fn supervisor_response_is_answered_with_silence() {
        // Reply-shaped messages (id + result/error, no method) are the
        // supervisor's responses to this fixture's outbound requests.
        let response = handle_message(json!({
            "jsonrpc": "2.0",
            "id": 100,
            "result": { "type": "execute-tool", "value": { "outcome": { "event_id": "mock-call-0", "gate_seq": 1, "replayed": false, "result": { "ok": true } } } }
        }));
        assert!(response.is_none(), "responses must not be answered");
    }

    #[test]
    fn undecodable_message_gets_parse_error_with_id_zero() {
        // Missing `params` decodes as neither a request nor a notification.
        let response = handle_message(json!({ "jsonrpc": "2.0", "id": 9, "method": "handshake" }))
            .expect("parse failure must get a reply");
        assert_eq!(response.id, 0);
        assert!(response.result.is_none());
        assert_eq!(response.error.expect("parse error reported").code, IpcErrorCode::ParseError);
    }
}
