# MCP client (Model Context Protocol)

Concerto speaks the **client** side of the Model Context Protocol over a stdio
transport (ADR-43, decision 2). Each configured server is spawned as a child
process; its tools are bridged into the shared tool registry as
`mcp:<server_id>:<tool_name>` and flow through the normal `ToolExecutor` —
policy, spend, audit, and events apply unchanged. Concerto is never an MCP
*server* in v1, and WASM plugins are not replaced.

The client, manager, and tool bridge live in `concerto-mcp` (`crates/mcp/`):
`client.rs` (`McpClient`), `manager.rs` (`McpManager`), `tool_bridge.rs`
(`McpTool`).

## Protocol pin

The client pins MCP revision **`2025-11-25`** (`PROTOCOL_VERSION` in
`crates/mcp/src/lib.rs`) — the latest fully stable revision (the official SDKs'
`LATEST_PROTOCOL_VERSION`). The 2026-07-28 stateless revision is beta and
explicitly deferred.

- **Framing:** newline-delimited JSON-RPC 2.0 — exactly one JSON-RPC message
  per line on stdin/stdout, with **no `Content-Length` headers**. Newline
  framing is the normative framing in every published MCP spec revision;
  LSP-style `Content-Length` framing is an early-SDK artifact and is
  deliberately NOT used. Messages must not contain embedded newlines (compact
  `serde_json` serialization never emits one; the writer rejects any payload
  that would break framing). A single message is bounded to 4 MiB so a
  misbehaving server cannot exhaust client memory with one unbounded line.
- **No `requests/shutdown` method exists in MCP** — shutdown is
  transport-level: close stdin, wait for a voluntary exit within a grace
  period, then kill (see Lifecycle).
- **Surface implemented:** `initialize` + `notifications/initialized`,
  cursor-paginated `tools/list`, `tools/call` (the server's `isError` flag and
  JSON-RPC errors are kept distinct), `ping` reply (for legacy servers that
  issue requests to the client), and `notifications/cancelled` on timeout.

## Lifecycle

`McpClient` follows the `LspClient` shape: `new` → `spawn` → `initialize` →
`list_tools`/`call_tool` → `stop`.

- **One child process per client** — a second `spawn` on a live client fails
  (`AlreadySpawned`). Restarting a crashed server is a fresh `spawn` after the
  old process exits.
- **Graceful stop** (`McpClient::stop`): sends `notifications/cancelled` for
  every in-flight request, closes stdin (the server observes EOF), waits up to
  a 2-second grace period for a voluntary exit, then escalates to
  `kill().await` + `wait().await` (SIGKILL on POSIX; SIGTERM→SIGKILL escalation
  is a later revision).
- **Never orphaned:** the `Drop` impl SIGKILLs (`start_kill`) any child that
  was never stopped and polls `try_wait()` (bounded, ~1s) so no server is left
  behind on panic, cancellation, or teardown.
- **States:** `Disabled` (idle) → `Connecting` (spawn) → `Connected`
  (`initialize`) → `Failed` (reader EOF/error or registration failure) →
  `Stopped` (graceful stop). Subscribe via `McpClient::subscribe_state()`.

### Crash and tools-unavailable semantics

- The reader task observes stdout EOF or an I/O error, captures the exit status
  and a human-readable failure detail, and flips the state to `Failed`.
- A per-server watcher in `McpManager` publishes
  `EventKind::McpServerStateChanged { server_id, state, error }` for every
  transition (`Connecting`, `Connected`, `Failed`, `Stopped`).
- A `Failed` transition **clears the server handle's tools** — tools are marked
  unavailable: the UI lists zero tools, and later runs do not re-register them
  until the server is reconnected via `McpManager::start_server`. Tools already
  handed to the current run's registry are not revoked; a call against a dead
  server fails cleanly through the bridge (`ToolError`).
- One bad server never blocks the rest: a server whose
  spawn/`initialize`/`tools/list` fails is marked `Failed` and skipped, and
  startup always completes.

## Registration and naming

- Tools are namespaced **`mcp:<server_id>:<tool_name>`** (`McpTool`).
- `server_id` is validated at config load (`McpConfig::validate`): non-empty,
  no `:`, unique. Duplicate ids are rejected there; `McpManager` re-checks as
  defense in depth.
- Registration is **collision-checked** (ADR-43 §4): `ToolRegistry::get` is
  consulted for every namespaced name, and a hit is a hard error
  (`McpError::DuplicateTool`) — nothing is ever silently clobbered. At startup
  a duplicate tool name rolls back that server's partially-registered tools,
  marks the server `Failed`, and logs loudly before continuing.
- `McpManager` is runtime-owned: it is constructed once from `McpConfig` +
  `EventBus` and spawns nothing. `register_tools` is called at startup **after**
  plugin tools (so MCP can never clobber them) and again per agent run — the
  runtime builds a fresh `ToolRegistry` per run, and tools of an
  already-connected server are re-bridged from the manager's handles (clones;
  no re-spawn). `start_server`/`stop_server` support live UI toggles;
  `stop_all` tears everything down gracefully.

## Timeouts

Every request is bounded by a caller-supplied timeout. For bridge calls
(`McpTool::execute`):

- `timeout_secs` is a concerto-side reserved input key, consumed by the bridge
  and stripped before forwarding — it never collides with a server-defined
  argument. Default comes from the server config (60s);
  `min(requested or default, 300)` is the enforced hard cap
  (`HARD_TIMEOUT_CAP_SECS`).
- An elapsed call surfaces as `ToolError::Timeout` (via `McpError::Timeout`) —
  never `Cancelled`. `initialize` is never cancellable (the spec forbids
  `notifications/cancelled` for it) and takes no cancellation token.

## Configuration

```toml
[mcp]
enabled = false   # default off until you opt in

[[mcp.servers]]
id = "filesystem"               # non-empty, no ':', unique
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/safe/path"]
env = {}                        # extra child env; no secrets in TOML
enabled = true                  # per-server switch (default true)
timeout_secs = 60               # per-call default; hard cap in the bridge
```

`McpServerConfig` fields: `id`, `command`, `args`, `env`, `enabled`,
`timeout_secs`. `env` entries are appended to the child's environment and come
from the config only — **secrets are never stored in TOML** (keyring-backed MCP
tokens are deferred).

## Security boundary

- **OS process isolation plus policy** (ADR-43, decision 7): MCP servers run as
  trusted child processes — not inside the WASM sandbox — and Concerto does not
  mediate their side effects beyond policy gating and explicit enablement.
- **`mcp.enabled` defaults to `false`**; servers start only when you opt in.
- **Default posture:** the orchestrator appends an `mcp:*` →
  `RequireApproval` preset rule *after* user rules
  (`crates/orchestrator/src/runtime_runner.rs`), so unmatched MCP tools are
  never implicitly auto-approved. Rules are first-match-wins, so an explicit
  user rule (e.g. allow `mcp:github:*`) placed earlier keeps precedence.
- **Server-level rules** are expressible via the `ToolNamePrefix` policy
  condition (e.g. `mcp:github:`); see [policy-rules.md](policy-rules.md).
- MCP tools are opaque to policy internals: gating is by tool name plus
  explicit enablement, and `DenyNetworkEgress` cannot see inside a server's
  traffic.

## Desktop and CLI

- **Desktop Settings → MCP:** v1 is display + enable + probe only
  (ADR-43 §3 v1 note). The master `mcp.enabled` toggle and per-server `enabled`
  flags edit the *pending* config and take effect on the next run (the desktop
  builds a fresh `ServicesBuilder` per agent run and does not hold the
  runtime-owned manager). "Test connection" spawns a temporary client
  (spawn → initialize → list_tools → stop) and reports the discovered tool
  names or a sanitized error. Servers are added/removed in the config file.
- **CLI:** `concerto extensions list` shows `mcp.enabled` and the configured
  servers (read-only in v1).

## fixture-mcp-server

`crates/mcp/src/bin/fixture_mcp_server.rs` is a minimal stdio server used by the
`concerto-mcp` integration tests; it doubles as a readable reference for the
wire format. It speaks newline-delimited JSON-RPC 2.0 with protocol version
`2025-11-25` and handles each request on its own thread. Tools: `echo` (returns
its input), `fail` (reports a tool-level `isError`), `slow` (blocks, for
timeout tests), and `crash` (exits 1 mid-request). Env knobs:
`FIXTURE_CRASH_ON_START=1` (exit 1 at startup), `FIXTURE_PID_FILE=<path>`
(write the pid), `FIXTURE_VERSION=<v>` (report a custom protocol version),
`FIXTURE_REJECT_INITIALIZE=1` (reject `initialize` with the `-32602`
negotiation error).

To drive it manually, build the binary and point a server at it:

```toml
[[mcp.servers]]
id = "fixture"
command = "/path/to/target/debug/fixture-mcp-server"
```

See [ADR-43](adrs/ADR-43-skills-mcp-and-extension-manager.md) for the
architecture record and [skills.md](skills.md) for the sibling skills feature.
