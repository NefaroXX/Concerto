use serde_json::Value;
use std::fmt;
use std::io;

/// Errors produced by the MCP stdio client and the [`McpTool`](crate::McpTool)
/// bridge.
///
/// The `Display` output is deliberately model- and log-readable: the bridge
/// converts a failed MCP call into a core [`ToolError`] whose message contains
/// this `Display` text.
#[derive(Debug)]
pub enum McpError {
    /// Underlying I/O failure (spawn, pipe read/write, kill, wait).
    Io { source: io::Error },
    /// JSON-RPC error reply from the server (`-32601` method not found,
    /// `-32602` invalid params, `-32603` internal, ...).
    JsonRpc {
        /// JSON-RPC error code.
        code: i64,
        /// Human-readable error message from the server.
        message: String,
        /// Optional `data` payload attached to the error object.
        data: Option<Value>,
    },
    /// Protocol violation by the server (malformed JSON, non-object message,
    /// oversized or empty line, response without result/error, ...).
    Protocol { detail: String },
    /// The server does not speak the pinned protocol version.
    ///
    /// `supported` is the server's advertised list (from the `-32602`
    /// `data.supported` negotiation payload) when the server rejected
    /// `initialize`, or the client's own pinned version when the server
    /// replied with an unknown `protocolVersion` field.
    VersionMismatch { supported: Vec<String> },
    /// A request exceeded its caller-supplied timeout.
    Timeout { method: String },
    /// The caller's cancellation token fired while a request was in flight.
    Cancelled,
    /// No server is running (never spawned, or the server already exited and
    /// the connection ended).
    NotConnected,
    /// The server process exited while the client was still active.
    ServerExited {
        /// Exit code, when the client could observe one.
        status: Option<i32>,
        /// Context about why the connection ended.
        detail: String,
    },
    /// A single wire message exceeded the framing limit (4 MiB).
    LineTooLong { len: usize },
    /// [`McpClient::spawn`](crate::McpClient::spawn) was called on a client
    /// that already has a live server process.
    AlreadySpawned,
    /// `McpManager` saw the same server id twice in config (defense in depth;
    /// `McpConfig::validate()` rejects duplicates at load time).
    DuplicateServer { server_id: String },
    /// Registering a server's tools would clobber an existing registry entry.
    /// MCP registration never silently overwrites (ADR-43 §4).
    DuplicateTool { name: String },
    /// `McpManager::start_server`/`stop_server` was asked about a server id
    /// that is not present in the active config.
    UnknownServer { server_id: String },
    /// `McpManager::start_server` was asked to start a server that is disabled
    /// (global `mcp.enabled = false` or the per-server `enabled = false`).
    ServerDisabled { server_id: String },
}

impl fmt::Display for McpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            McpError::Io { source } => write!(f, "mcp io error: {source}"),
            McpError::JsonRpc { code, message, data } => {
                write!(f, "mcp json-rpc error {code}: {message}")?;
                if let Some(data) = data {
                    write!(f, " (data: {data})")?;
                }
                Ok(())
            }
            McpError::Protocol { detail } => write!(f, "mcp protocol error: {detail}"),
            McpError::VersionMismatch { supported } => {
                write!(
                    f,
                    "mcp protocol version not supported (this client speaks '{}'): server supports {}",
                    crate::PROTOCOL_VERSION,
                    supported.join(", ")
                )
            }
            McpError::Timeout { method } => write!(f, "mcp request timed out: {method}"),
            McpError::Cancelled => write!(f, "mcp request cancelled"),
            McpError::NotConnected => write!(f, "not connected to mcp server"),
            McpError::ServerExited { status, detail } => match status {
                Some(code) => write!(f, "mcp server exited with status {code}: {detail}"),
                None => write!(f, "mcp server exited: {detail}"),
            },
            McpError::LineTooLong { len } => {
                write!(f, "mcp message of {len} bytes exceeds the 4 MiB framing limit")
            }
            McpError::AlreadySpawned => write!(f, "mcp server already spawned"),
            McpError::DuplicateServer { server_id } => {
                write!(f, "mcp server id '{server_id}' is configured more than once")
            }
            McpError::DuplicateTool { name } => {
                write!(f, "mcp tool name '{name}' collides with an already-registered tool")
            }
            McpError::UnknownServer { server_id } => {
                write!(f, "no mcp server configured with id '{server_id}'")
            }
            McpError::ServerDisabled { server_id } => {
                write!(f, "mcp server '{server_id}' is disabled in config")
            }
        }
    }
}

impl std::error::Error for McpError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            McpError::Io { source } => Some(source),
            _ => None,
        }
    }
}

impl From<io::Error> for McpError {
    fn from(source: io::Error) -> Self {
        McpError::Io { source }
    }
}
