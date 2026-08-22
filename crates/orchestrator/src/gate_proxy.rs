//! Gate-proxy client and backends — ADR-60 S5 (agent-process entry).
//!
//! The child agent process speaks the supervisor protocol over stdio
//! ([`crate::ipc`]). This module owns the client side of that conversation
//! and adapts it onto the interfaces the single-agent loop consumes:
//!
//! - [`GateProxyClient`] — request/response plumbing with correlation ids,
//!   the D2 handshake answer, out-of-band response handling (heartbeat
//!   pings while a request is in flight), and the `list-tools` registry
//!   fetch (D6 supplement: the gate owns the tool registry).
//! - [`GateProxyBackend`] — [`ToolExecutionBackend`]: every tool call is a
//!   gated write (`execute-tool`) through the supervisor's single write gate
//!   (ADR-60 D4). Tool definitions are cached at connect.
//! - [`GateProxyMemoryStore`] — the loop's memory spine is a facade over
//!   `retrieve-memory` (ADR-60 D6); stores/invalidations are logged and
//!   dropped agent-side (memory ingestion is a supervisor concern).
//!
//! The client is sequential by construction: the loop awaits each request
//! before issuing the next, so at most one request is in flight per agent
//! process.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use concerto_core::error::{MemoryError, ToolError};
use concerto_core::ids::Ulid;
use concerto_core::memory::{
    ChunkType, MemoryChunk, MemoryEntry, MemoryId, MemoryNamespace, MemoryQuery, ProjectId,
};
use concerto_core::traits::memory::MemoryStore;
use concerto_core::types::{SessionContext, ToolDefinition, ToolOutput};
use concerto_core::CancellationToken;
use thiserror::Error;

use crate::exec_backend::ToolExecutionBackend;
use crate::gate::GateRequest;
use crate::ipc::{
    self, IpcErrorCode, IpcMethod, IpcNotification, IpcParams, IpcRequest, IpcResponse, IpcResult,
    IpcTransportError, MAX_MESSAGE_BYTES,
};

/// Errors from the gate-proxy client (child-agent side).
#[derive(Debug, Error)]
pub enum GateProxyError {
    /// The supervisor closed the connection (EOF), or was never there.
    #[error("supervisor connection closed")]
    Closed,
    /// The peer sent a message that violates the protocol contract.
    #[error("protocol violation: {0}")]
    Protocol(String),
    /// The supervisor answered with an error response.
    #[error("supervisor error {code}: {message}")]
    Supervisor { code: IpcErrorCode, message: String },
    /// Transport/framing failure while talking to the supervisor.
    #[error("ipc transport error: {0}")]
    Transport(#[from] IpcTransportError),
    /// Message serialization/deserialization failure.
    #[error("message codec error: {0}")]
    Codec(#[from] serde_json::Error),
    /// The operation was cancelled before completion.
    #[error("gate proxy cancelled")]
    Cancelled,
}

/// The child-side protocol client: one request in flight at a time, ids
/// correlated sequentially, and supervisor-initiated requests (heartbeat
/// pings) answered transparently while awaiting a response.
pub struct GateProxyClient {
    agent_id: String,
    stdin: tokio::io::Stdin,
    stdout: tokio::io::Stdout,
    next_id: AtomicU64,
    /// Caller-owned carry buffer for the framing contract (`ipc.rs`).
    buf: Vec<u8>,
}

impl GateProxyClient {
    /// Bind to the process's stdio and complete the versioned handshake
    /// (ADR-60 D2). The supervisor initiates the handshake as the spawn-side
    /// client; this side answers `server_accept` and rejects a version
    /// mismatch.
    pub async fn connect(agent_id: String) -> Result<Self, GateProxyError> {
        let mut client = Self {
            agent_id,
            stdin: tokio::io::stdin(),
            stdout: tokio::io::stdout(),
            next_id: AtomicU64::new(1),
            buf: Vec::new(),
        };
        // The handshake is a one-shot exchange: every branch below resolves
        // (accepted, rejected, or transport error), so there is no retry loop.
        match client.next_message().await? {
            Some(value) => {
                let request = serde_json::from_value::<IpcRequest>(value).map_err(|_| {
                    GateProxyError::Protocol(
                        "expected the supervisor's handshake request first".to_owned(),
                    )
                })?;
                if request.method != IpcMethod::Handshake {
                    return Err(GateProxyError::Protocol(format!(
                        "expected handshake, got {:?}",
                        request.method
                    )));
                }
                let version = match &request.params {
                    IpcParams::Handshake { protocol_version, .. } => protocol_version.clone(),
                    _ => {
                        return Err(GateProxyError::Protocol(
                            "handshake params missing".to_owned(),
                        ));
                    }
                };
                let result = if version == ipc::PROTOCOL_VERSION {
                    ipc::server_accept()
                } else {
                    ipc::server_reject("protocol version mismatch")
                };
                client.send_response(request.id, Some(result), None).await?;
                if version != ipc::PROTOCOL_VERSION {
                    return Err(GateProxyError::Protocol(format!(
                        "protocol version mismatch: agent {version}, supervisor {}",
                        ipc::PROTOCOL_VERSION
                    )));
                }
                Ok(client)
            }
            None => Err(GateProxyError::Closed),
        }
    }

    /// Fetch the supervisor's tool registry (the single source of truth).
    pub async fn list_tools(&mut self) -> Result<Vec<ToolDefinition>, GateProxyError> {
        let response = self
            .request(IpcMethod::ListTools, IpcParams::ListTools { agent_id: self.agent_id.clone() })
            .await?;
        let tools = match response.result {
            Some(IpcResult::ListTools { tools }) => tools,
            _ => return Err(GateProxyError::Protocol("list-tools result missing".to_owned())),
        };
        Ok(tools)
    }

    /// Round-trip one request and return the matched response.
    async fn request(
        &mut self,
        method: IpcMethod,
        params: IpcParams,
    ) -> Result<IpcResponse, GateProxyError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let request = IpcRequest { jsonrpc: "2.0".to_owned(), id, method, params };
        self.send_request(&request).await?;
        loop {
            match self.next_message().await? {
                Some(value) => {
                    // A response shaped message (id + result/error, no
                    // method) is the supervisor's answer to our request.
                    if value.get("method").is_none()
                        && (value.get("result").is_some() || value.get("error").is_some())
                    {
                        let response = serde_json::from_value::<IpcResponse>(value)
                            .map_err(GateProxyError::Codec)?;
                        if response.id != id {
                            return Err(GateProxyError::Protocol(format!(
                                "expected response id {id}, got {}",
                                response.id
                            )));
                        }
                        return Ok(response);
                    }
                    // Supervisor-initiated request while we await: answer
                    // heartbeat pings, reject everything else loudly.
                    if let Ok(request) = serde_json::from_value::<IpcRequest>(value.clone()) {
                        match request.method {
                            IpcMethod::Heartbeat => {
                                self.send_response(
                                    request.id,
                                    Some(IpcResult::Heartbeat { accepted: true }),
                                    None,
                                )
                                .await?;
                            }
                            _ => {
                                self.send_response(
                                    request.id,
                                    None,
                                    Some(ipc::IpcError::new(
                                        IpcErrorCode::InvalidRequest,
                                        "agent process does not serve this method",
                                    )),
                                )
                                .await?;
                            }
                        }
                        continue;
                    }
                    // Unrequested notification: nothing to answer.
                    if let Ok(_notification) = serde_json::from_value::<IpcNotification>(value) {
                        continue;
                    }
                    return Err(GateProxyError::Protocol(
                        "unparseable line while awaiting response".to_owned(),
                    ));
                }
                None => return Err(GateProxyError::Closed),
            }
        }
    }

    /// Read the next inbound line, mapping framing failures to [`GateProxyError`].
    async fn next_message(&mut self) -> Result<Option<serde_json::Value>, GateProxyError> {
        match ipc::read_message(&mut self.stdin, &mut self.buf, MAX_MESSAGE_BYTES).await {
            Ok(Some(value)) => Ok(Some(value)),
            Ok(None) => Ok(None),
            Err(IpcTransportError::Closed) => Err(GateProxyError::Closed),
            Err(error) => Err(GateProxyError::Transport(error)),
        }
    }

    /// Serialize and write one request line.
    async fn send_request(&mut self, request: &IpcRequest) -> Result<(), GateProxyError> {
        let value = serde_json::to_value(request)?;
        ipc::write_message(&mut self.stdout, &value).await.map_err(GateProxyError::Transport)
    }

    /// Serialize and write one response line (exactly one of `result` /
    /// `error` is `Some`).
    async fn send_response(
        &mut self,
        id: u64,
        result: Option<IpcResult>,
        error: Option<ipc::IpcError>,
    ) -> Result<(), GateProxyError> {
        let response = IpcResponse { jsonrpc: "2.0".to_owned(), id, result, error };
        let value = serde_json::to_value(&response)?;
        ipc::write_message(&mut self.stdout, &value).await.map_err(GateProxyError::Transport)
    }
}

/// The executor backend every loop tool call flows through when supervised:
/// forward to the gate, rebuild the [`ToolOutput`] the loop consumes from
/// the gate's durable outcome (the full output rides the wire, ADR-60 S5).
pub struct GateProxyBackend {
    client: Arc<tokio::sync::Mutex<GateProxyClient>>,
    agent_id: String,
    definitions: std::sync::OnceLock<Vec<ToolDefinition>>,
}

impl GateProxyBackend {
    /// The write scope this agent's tool calls are gated under; `"fs"` today.
    const SCOPE: &'static str = "fs";

    /// Bind to the supervisor and cache the published tool registry.
    pub async fn new(
        client: Arc<tokio::sync::Mutex<GateProxyClient>>,
        agent_id: String,
    ) -> Result<Self, GateProxyError> {
        let mut guard = client.lock().await;
        let tools = guard.list_tools().await?;
        drop(guard);
        let backend = Self { client, agent_id, definitions: std::sync::OnceLock::new() };
        let _ = backend.definitions.set(tools);
        Ok(backend)
    }

    /// The underlying client, for the process entry to publish its final
    /// whiteboard event and to drain/send out-of-band traffic.
    pub fn client(&self) -> Arc<tokio::sync::Mutex<GateProxyClient>> {
        self.client.clone()
    }

    /// Append one agent-attested event to the whiteboard log (ADR-60 D3).
    pub async fn publish_event(
        &self,
        event: concerto_sessions::whiteboard::NewWhiteboardEvent,
    ) -> Result<concerto_sessions::whiteboard::WhiteboardEvent, GateProxyError> {
        let response = self
            .client
            .lock()
            .await
            .request(IpcMethod::PublishEvent, IpcParams::PublishEvent { event })
            .await?;
        match response.result {
            Some(IpcResult::PublishEvent { stored }) => Ok(stored),
            _ => Err(GateProxyError::Protocol("publish-event result missing".to_owned())),
        }
    }
}

#[async_trait]
impl ToolExecutionBackend for GateProxyBackend {
    fn tool_definitions(&self) -> Vec<ToolDefinition> {
        self.definitions.get().cloned().unwrap_or_default()
    }

    async fn execute(
        &self,
        tool_name: &str,
        input: serde_json::Value,
        call_id: &str,
        session: &SessionContext,
        cancel: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let mut input = input;
        // ADR-60 D5: `base_versions` is a gate-level concurrency claim map,
        // not tool input — lift it out of the payload and forward the rest.
        // An agent may declare its own claims (a declared claim wins over the
        // supervisor's always-on injection, per target); when it does not,
        // the supervisor stamps each mutated target's current pre-image hash
        // before submission.
        let mut base_versions = BTreeMap::new();
        if let Some(map) = input.as_object_mut() {
            if let Some(serde_json::Value::Object(claims)) = map.remove("base_versions") {
                for (target, claim) in claims {
                    if let serde_json::Value::String(hash) = claim {
                        base_versions.insert(target, hash);
                    }
                }
            }
        }

        let request = GateRequest {
            call_id: call_id.to_owned(),
            // Attribution is bound to the registered process by the
            // supervisor; this value is informational only.
            agent_id: self.agent_id.clone(),
            tool: tool_name.to_owned(),
            input,
            session_id: Some(session.session_id.to_string()),
            scope: Self::SCOPE.to_owned(),
            plan_id: None,
            causation: None,
            base_versions,
        };
        let response = self
            .client
            .lock()
            .await
            .request(IpcMethod::ExecuteTool, IpcParams::ExecuteTool { request })
            .await
            .map_err(gate_proxy_to_tool_error)?;
        if cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        if let Some(error) = response.error {
            let code = error.code;
            return Err(gate_proxy_to_tool_error(GateProxyError::Supervisor {
                code,
                message: error.message,
            }));
        }
        let outcome = match response.result {
            Some(IpcResult::ExecuteTool { outcome }) => outcome,
            _ => {
                return Err(gate_proxy_to_tool_error(GateProxyError::Protocol(
                    "execute-tool result missing".to_owned(),
                )));
            }
        };
        serde_json::from_value::<ToolOutput>(outcome.result).map_err(|error| {
            ToolError::ExecutionFailed {
                message: format!("gate outcome did not carry a ToolOutput: {error}"),
            }
        })
    }

    async fn record_ack_decision(
        &self,
        _session_id: Ulid,
        _correlation_id: Ulid,
        _message: &str,
        _acknowledged: bool,
        _cancel: CancellationToken,
    ) {
        tracing::warn!(
            "agent process: ack decisions are recorded supervisor-side in the ADR-60 model; \
             skipping the agent-side audit write"
        );
    }
}

/// The loop's memory facade when supervised: retrieval crosses to the
/// supervisor's memory spine (`retrieve-memory`, ADR-60 D6); stores and
/// invalidations are supervisor concerns and are logged, not forwarded.
pub struct GateProxyMemoryStore {
    client: Arc<tokio::sync::Mutex<GateProxyClient>>,
    agent_id: String,
    project_id: ProjectId,
}

impl GateProxyMemoryStore {
    /// A memory spine facade bound to the supervisor connection.
    pub fn new(
        client: Arc<tokio::sync::Mutex<GateProxyClient>>,
        agent_id: String,
        project_id: ProjectId,
    ) -> Self {
        Self { client, agent_id, project_id }
    }
}

#[async_trait]
impl MemoryStore for GateProxyMemoryStore {
    async fn retrieve(
        &self,
        query: &MemoryQuery,
        cancel: CancellationToken,
    ) -> Result<Vec<MemoryChunk>, MemoryError> {
        if cancel.is_cancelled() {
            return Err(MemoryError::Cancelled);
        }
        let response = self
            .client
            .lock()
            .await
            .request(
                IpcMethod::RetrieveMemory,
                IpcParams::RetrieveMemory {
                    query: query.text.clone(),
                    agent_id: self.agent_id.clone(),
                    limit: query.top_k.max(1) as u32,
                },
            )
            .await
            .map_err(|error| MemoryError::Persistence(error.to_string()))?;
        if let Some(error) = response.error {
            return Err(MemoryError::Persistence(format!(
                "supervisor retrieve-memory failed {:?}: {}",
                error.code, error.message
            )));
        }
        let chunks = match response.result {
            Some(IpcResult::RetrieveMemory { chunks }) => chunks,
            _ => return Err(MemoryError::Persistence("retrieve-memory result missing".to_owned())),
        };
        // The wire chunk is a projection (text/kind/score); rebuild the
        // store chunk with placeholder metadata — the whitespace/prompt
        // consumers only read content and score today (ADR-60 D6 linkage
        // lands in a later chunk).
        Ok(chunks
            .into_iter()
            .map(|chunk| MemoryChunk {
                id: chunk.source_event_id.unwrap_or_default(),
                project_id: self.project_id.clone(),
                namespace: MemoryNamespace::Project(self.project_id.clone()),
                content: chunk.text,
                file_path: None,
                start_line: None,
                end_line: None,
                chunk_type: ChunkType::Test,
                score: chunk.score,
                model_id: String::new(),
                model_version: String::new(),
            })
            .collect())
    }

    async fn store(
        &self,
        _entry: MemoryEntry,
        _cancel: CancellationToken,
    ) -> Result<MemoryId, MemoryError> {
        tracing::warn!(
            "agent process: memory stores are supervisor-side in the ADR-60 model; \
             dropping the agent-side store entry"
        );
        Ok(MemoryId(Ulid::new()))
    }

    async fn invalidate(
        &self,
        _id: MemoryId,
        _cancel: CancellationToken,
    ) -> Result<(), MemoryError> {
        tracing::warn!("agent process: memory invalidation is supervisor-side; ignoring");
        Ok(())
    }
}

/// Map a proxy failure onto the loop's tool error taxonomy. Shared with the
/// in-process backend parity test, which asserts the two paths cannot drift.
pub(crate) fn gate_proxy_to_tool_error(error: GateProxyError) -> ToolError {
    match error {
        GateProxyError::Closed => ToolError::ExecutionFailed {
            message: "supervisor connection closed during gated execution".to_owned(),
        },
        GateProxyError::Cancelled => ToolError::Cancelled,
        GateProxyError::Supervisor { code, message } => {
            ToolError::ExecutionFailed { message: gate_rejection_message(code, &message) }
        }
        other => ToolError::ExecutionFailed { message: other.to_string() },
    }
}

/// Render a supervisor gate rejection the way the loop's tool error surface
/// presents it.
///
/// Shared by the supervised backend (which receives the flattened
/// `(code, message)` over the wire) and the in-process backend (which maps a
/// local [`GateError`] through [`crate::ipc::IpcError::from_gate`] first), so
/// agents see a byte-identical error string no matter which path they run
/// under.
pub(crate) fn gate_rejection_message(code: IpcErrorCode, message: &str) -> String {
    format!("gate rejected the write ({code:?}): {message}")
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::gate::GateOutcome;

    /// The supervisor answers `execute-tool` with the gate outcome; the
    /// backend must reconstruct a full [`ToolOutput`] from `outcome.result`
    /// (ADR-60 S5) so the child loop sees the materialized result.
    #[test]
    fn execute_outcome_round_trips_as_full_tool_output() {
        let output = ToolOutput {
            summary: "Wrote 17 bytes to hello.txt".to_owned(),
            data: serde_json::json!({ "path": "hello.txt", "size": 17 }),
        };
        let request: IpcRequest = IpcRequest {
            jsonrpc: "2.0".to_owned(),
            id: 1,
            method: IpcMethod::ExecuteTool,
            params: IpcParams::ExecuteTool {
                request: GateRequest {
                    call_id: "call-1".to_owned(),
                    agent_id: "agent-a".to_owned(),
                    tool: "write_file".to_owned(),
                    input: serde_json::json!({ "operation": "write" }),
                    session_id: None,
                    scope: "fs".to_owned(),
                    plan_id: None,
                    causation: None,
                    base_versions: BTreeMap::new(),
                },
            },
        };
        // Serialize and deserialize exactly like the wire/`list-tools` path.
        let on_wire = serde_json::to_value(request).expect("request serializes");
        let _request: IpcRequest = serde_json::from_value(on_wire).expect("request round-trips");

        let response = IpcResponse {
            jsonrpc: "2.0".to_owned(),
            id: 1,
            result: Some(IpcResult::ExecuteTool {
                outcome: GateOutcome {
                    event_id: "call-1".to_owned(),
                    gate_seq: 1,
                    replayed: false,
                    result: serde_json::to_value(&output).expect("ToolOutput serializes"),
                },
            }),
            error: None,
        };
        let encoded = serde_json::to_value(&response).expect("response serializes");
        let decoded: IpcResponse = serde_json::from_value(encoded).expect("response decodes");
        let outcome = match decoded.result.expect("result present") {
            IpcResult::ExecuteTool { outcome } => outcome,
            other => panic!("unexpected result variant: {other:?}"),
        };
        let round_tripped: ToolOutput =
            serde_json::from_value(outcome.result).expect("outcome.result decodes as ToolOutput");
        assert_eq!(round_tripped.summary, output.summary);
        assert_eq!(round_tripped.data["path"], serde_json::json!("hello.txt"));
    }
}
