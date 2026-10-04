# ADR-79: Extension authorization and host execution

Status: Accepted
Date: 2026-10-03

## Context
Runtime discovery bypassed binary-pinned capability approval. Plugin host file
and shell effects also bypassed the shared executor, overlay, and shell settings.
MCP request deadlines covered responses but excluded lock waits and pipe writes.

## Decision
Discovery never creates grants. Runtime activation uses the capability manager
with a deny-by-default approval interface: install-time persistent approvals
must match the current binary, scope, and TTL. Incomplete approval responses
fail closed. The default plugin directory participates when no paths are set.

Plugin file and shell effects use a run-scoped context containing a weak reference
to the existing ToolExecutor and the current SessionContext. Effects use ordinary
executor authority, canonical filesystem/shell names, VirtualFs and configured
shell restrictions. No context means effects are refused, including during init.
The weak reference avoids an executor/registry/plugin ownership cycle. A new run
invalidates the preceding context; revocation continues to clear live grants.

Wasmtime StoreLimits bound memory and tables during instantiation and growth.
Each export resets fuel and epoch deadlines. Guest pointer arithmetic and host
output collection are bounded. Host network calls enforce approved egress on
initial and redirect targets, and observe cancellation and deadlines.

MCP deadlines encompass client lock acquisition, writing, and response receipt.
A partially written cancelled/timed-out request invalidates the transport.
Child environments contain only platform launch essentials plus explicitly
configured values. Secret-like environment keys require keyring references;
configuration never serializes the resolved secret. Desktop argument editing uses
JSON arrays for exact round trips and displays the executable trust boundary.
MCP stop/drop kills process groups on Unix and retained Job Objects on Windows.

## Consequences
Previously auto-approved plugins require persistent approval in Settings or CLI.
Existing plaintext MCP credentials must move to the OS keychain. MCP subprocesses
remain trusted user processes; this does not introduce remote HTTP/OAuth support
or an OS sandbox. Native UI and platform process checks remain release checks.
