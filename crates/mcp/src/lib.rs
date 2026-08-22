#![deny(clippy::all)]
#![deny(unused_imports, unused_variables, dead_code)]
#![allow(missing_docs)]

//! `concerto-mcp` — MCP stdio client and tool bridge (ADR-43, decision 2;
//! plan §6 Phase C).
//!
//! This crate implements the **client** side of the Model Context Protocol
//! over a stdio transport: it spawns a configured MCP server as a child
//! process, speaks JSON-RPC 2.0 with it, and bridges every remote tool into
//! the shared [`Tool`](concerto_core::traits::tool::Tool) trait so MCP tools
//! flow through the normal `ToolExecutor` (policy, spend, audit, events).
//! Concerto is never an MCP *server* in v1.
//!
//! # Protocol pin
//!
//! The client pins MCP revision **`2025-11-25`** ([`PROTOCOL_VERSION`]) — the
//! latest fully stable revision (the official SDKs' `LATEST_PROTOCOL_VERSION`).
//! The 2026-07-28 stateless revision is beta and explicitly deferred.
//! Surface implemented: `initialize` + `notifications/initialized`,
//! cursor-paginated `tools/list`, `tools/call` (with the `isError` and
//! JSON-RPC error channels kept distinct), `ping` reply (for legacy servers
//! that issue requests to the client), and `notifications/cancelled` on
//! timeout. There is **no `requests/shutdown` method** in MCP — shutdown is
//! transport-level (see Lifecycle).
//!
//! # Framing
//!
//! STDIO framing is **newline-delimited JSON**: exactly one JSON-RPC message
//! per line on stdin/stdout, with no `Content-Length` headers. This is the
//! normative framing in every published MCP spec revision; LSP-style
//! `Content-Length` framing is an early-SDK artifact and is deliberately NOT
//! used. Messages must not contain embedded newlines: `serde_json`'s compact
//! serialization never emits one (string values are escaped), and the writer
//! rejects any payload that would break framing. A single message is bounded
//! to 4 MiB so a misbehaving server cannot exhaust client memory with one
//! unbounded line.
//!
//! # Lifecycle contract
//!
//! [`McpClient::spawn`] starts exactly one child process per client
//! (double-spawn guard); restarting a crashed server is a fresh
//! [`McpClient::spawn`]. [`McpClient::stop`] is the graceful path: it sends
//! `notifications/cancelled` for every in-flight request, closes stdin (EOF),
//! waits up to a grace period for the server to exit, then escalates to
//! `kill().await` + `wait().await` (tokio's `kill()` is SIGKILL on POSIX;
//! SIGTERM→SIGKILL escalation is a later revision). The `Drop` impl reaps any
//! child that was never stopped: `tokio::process::Child` detaches from the OS
//! process on drop, which would orphan the server, so `Drop` sends SIGKILL via
//! `start_kill()` and polls `try_wait()` until the child is reaped (bounded,
//! ~1s). **No server is ever orphaned**, including on panic or teardown.
//!
//! # Timeouts
//!
//! Every request is bounded by a caller-supplied timeout; an elapsed call
//! surfaces as [`McpError::Timeout`] (and, through the [`McpTool`] bridge, as
//! [`ToolError::Timeout`](concerto_core::error::ToolError::Timeout) — never
//! `Cancelled`, per plan amendment AMEND-A5). `initialize` is never cancelled
//! (the spec forbids it). The bridge enforces a hard per-call cap of
//! [`HARD_TIMEOUT_CAP_SECS`] seconds on top of the input- or config-driven
//! timeout.
//!
//! # Security boundary
//!
//! MCP servers run as trusted child processes; the boundary is OS process
//! isolation plus policy gating (ADR-43, decision 7). Environment passed to a
//! server comes from the config `env` map only; secrets are never stored in
//! TOML.

pub mod client;
pub mod error;
pub mod manager;
pub mod tool_bridge;
mod transport;

pub use client::{McpCallResult, McpClient, McpServerInfo};
pub use error::McpError;
pub use manager::McpManager;
pub use tool_bridge::{McpTool, HARD_TIMEOUT_CAP_SECS};

/// MCP protocol revision pinned for this client.
///
/// `2025-11-25` is the latest fully stable revision (the official SDKs'
/// `LATEST_PROTOCOL_VERSION`); the 2026-07-28 stateless revision is beta and
/// explicitly deferred (ADR-43, decision 2).
pub const PROTOCOL_VERSION: &str = "2025-11-25";
