# Concerto WASM Plugin System

The host executes WASM tool, provider, and memory-adapter plugins. Tool names
are always `plugin:<id>:<tool>`. Runtime discovery requires approved persisted
capabilities; discovery never grants authority (ADR-78).

## What tool plugins can do

- Register tools that agents can call
- Access filesystem (with `FilesystemRead`/`FilesystemWrite` capability)
- Make HTTP requests (with `NetworkOutbound` capability)
- Execute shell commands (with `ShellExecute` capability)
- Emit events to the event bus
- Log messages via the host `log` function

## What plugins cannot do yet

- Provider wrappers execute `call_provider`, but plugin-backed providers are
  restricted to AnswerOnly requests; tool-carrying requests are refused.
- Memory adapters execute `call_adapter` through `PluginBackedVectorStore`;
  selecting one as the runtime memory backend remains unsupported.
- Host completion requests require an explicitly attached LLM provider. The
  shared runtime does not attach one to the plugin manager by default.

## ABI Overview

### Required Exports

Every plugin must export the common memory, scratch, manifest, and init exports,
plus the call export for its kind (`call_tool`, `call_provider`, or `call_adapter`):

| Export | Signature | Description |
|--------|-----------|-------------|
| `manifest` | `() -> i64` | Returns packed pointer/length of JSON manifest |
| `call_tool` | `(i32, i32, i32, i32, i32, i32) -> i64` | Calls a tool (canonical ABI v1) |
| `init` | `() -> i32` | Initializes plugin (returns 0 on success) |
| `memory` | `memory` | WASM linear memory |
| `scratch_buffer` | `global (mut i32)` | Scratch buffer pointer (set by host) |
| `scratch_buffer_size` | `global i32` | Scratch buffer capacity (default 64 KiB) |

### `call_tool` Parameters

```text
call_tool(
    name_ptr:  i32,   // pointer to tool name in linear memory
    name_len:  i32,   // length of tool name
    input_ptr: i32,   // pointer to JSON input in linear memory
    input_len: i32,   // length of JSON input
    scratch_ptr: i32, // pointer to scratch buffer for result output
    scratch_len: i32, // capacity of scratch buffer
) -> i64             // packed (ptr, len) or RESULT_ERROR (-1)
```

### Result Encoding

Return values are encoded as a packed `i64`: high 32 bits = pointer into WASM linear memory, low 32 bits = byte length. `RESULT_ERROR` (`-1`) indicates an error occurred; the plugin's `last_error` function can retrieve the error string.

### Scratch Buffer

The host sets `scratch_buffer` (a mutable global) to point to a region in linear memory. The plugin uses this region for `manifest()` output. For `call_tool()`, the host passes a scratch buffer via the last two parameters (`scratch_ptr`, `scratch_len`).

- Default size: 64 KiB
- Host scratch/input/output ceiling: 1 MiB; `resize_scratch` rejects larger requests
- Plugin can call `resize_scratch(new_size)` to grow the buffer (returns 0 on success, -1 on failure)

### Host Functions

Plugins import these from the `concerto` module:

| Import | Signature | Capability Required | Description |
|--------|-----------|---------------------|-------------|
| `log` | `(level_ptr: i32, level_len: i32, msg_ptr: i32, msg_len: i32) -> ()` | None | Log a message at the given level |
| `last_error` | `(scratch_ptr: i32, scratch_len: i32) -> i64` | None | Retrieve the last error string |
| `resize_scratch` | `(new_size: i32) -> i32` | None | Resize the scratch buffer |
| `read_file` | `(path_ptr: i32, path_len: i32, scratch_ptr: i32, scratch_len: i32) -> i64` | FilesystemRead | Read a file's contents |
| `write_file` | `(path_ptr: i32, path_len: i32, content_ptr: i32, content_len: i32) -> i32` | FilesystemWrite | Write UTF-8 content through shared VirtualFs tracking |
| `http_get` | `(url_ptr: i32, url_len: i32, scratch_ptr: i32, scratch_len: i32) -> i64` | NetworkOutbound | Policy-gated HTTP GET (30s total timeout, 1 MiB body cap) |
| `shell_exec` | `(cmd_ptr: i32, cmd_len: i32, scratch_ptr: i32, scratch_len: i32) -> i64` | ShellExecute | Execute a shell command (30s timeout) |
| `emit_event` | `(event_ptr: i32, event_len: i32) -> ()` | None | Emit a JSON event to the event bus |
| `completion` | `(req_ptr: i32, req_len: i32, scratch_ptr: i32, scratch_len: i32) -> i64` | None | Async completion via an attached provider (30s, bounded output) |

## Guest SDK

The `concerto-plugin-sdk` crate provides ABI constants, helpers, and the `plugin_entry!` macro for building plugins in Rust. It is `#![no_std]` compatible and targets `wasm32-wasip2`.

### Minimal Rust Plugin Example

```rust,ignore
use concerto_plugin_sdk::plugin_entry;

fn manifest() -> &'static [u8] {
    br#"{
        "id": "my-plugin",
        "name": "My Plugin",
        "version": "0.1.0",
        "description": "A simple plugin",
        "abi_version": 1,
        "capabilities_required": [],
        "provides": [
            {
                "Tool": {
                    "name": "hello",
                    "description": "Say hello",
                    "input_schema": {
                        "type": "object",
                        "properties": {
                            "name": { "type": "string" }
                        },
                        "required": ["name"]
                    }
                }
            }
        ]
    }"#
}

fn call_tool(name: &str, args: &str) -> String {
    match name {
        "hello" => r#"{"message":"Hello from WASM plugin!"}"#.to_string(),
        _ => r#"{"error":"unknown tool"}"#.to_string(),
    }
}

plugin_entry!(manifest, call_tool);
```

The `plugin_entry!` macro generates the required WASM exports (`manifest`, `call_tool` with canonical ABI v1 6-parameter signature, `init`, `scratch_buffer`, `scratch_buffer_size`). The default `init()` returns 0 (success); override by writing your own `#[export_name = "init"]` function after the macro invocation.

## Manifest Format

```json
{
    "id": "unique-plugin-id",
    "name": "Plugin Name",
    "version": "0.1.0",
    "description": "What the plugin does",
    "abi_version": 1,
    "capabilities_required": [
        { "FilesystemRead": { "globs": ["*.txt"] } },
        { "NetworkOutbound": { "domains": ["api.example.com"] } }
    ],
    "provides": [
        {
            "Tool": {
                "name": "tool_name",
                "description": "What the tool does",
                "input_schema": { "type": "object", "properties": {} }
            }
        }
    ]
}
```

### `capabilities_required` Variants

| Variant | Fields | Description |
|---------|--------|-------------|
| `FilesystemRead` | `globs: Vec<String>` | Read files matching glob patterns (empty = all files) |
| `FilesystemWrite` | `globs: Vec<String>` | Write files matching glob patterns (empty = all files) |
| `NetworkOutbound` | `domains: Vec<String>` | HTTP requests to allowed domains (empty = all domains) |
| `ShellExecute` | `allowlist: Vec<String>` | Shell commands in allowlist (empty = all commands; non-empty entries match the whole command exactly) |
| `Other` | `description: String` | Application-specific capability (not enforced by the host) |

### `provides` Variants

| Variant | Fields | Status |
|---------|--------|--------|
| `Tool` | `name, description, input_schema` | Executable |
| `Provider` | `name, model` | Executable wrapper |
| `MemoryAdapter` | `name, kind` | Executable wrapper |

## Sidecar Manifests

A `.manifest.json` (or `.toml` for legacy) file next to the `.wasm` file is checked at load time. The sidecar manifest must match the embedded manifest exactly; a mismatch causes a load failure. This provides a human-readable metadata source without requiring WASM inspection.

Discovery searches in order:
1. `plugin.wasm.manifest.json` (preferred)
2. `plugin.wasm.toml` (legacy fallback)

## Building

```sh
rustup target add wasm32-wasip2
cargo build -p my-plugin --target wasm32-wasip2 --release
```

The `concerto-plugin-sdk` crate compiles under `#![no_std]` with `extern crate alloc` for wasm32 targets.

## Installing

Place the `.wasm` file in one of:
- A directory specified in `plugins.search_paths` in `config.toml`
- The default XDG data directory: `~/.local/share/concerto/plugins/`

Configure via `config.toml` (at `~/.config/concerto/config.toml`):

```toml
[plugins]
enabled = true
search_paths = ["/path/to/plugins"]
auto_load = true
```

## Capability Approval

When a plugin requires capabilities:

- **Desktop installation**: The capability dialog shows the full requested
  scopes and offers binary-pinned approval for 30 days or denial.
- **CLI interactive**: A text prompt asks for grant (g), always allow (a), or deny (d).
- **CLI non-interactive**: All capabilities are auto-denied.

Runtime activation accepts only unexpired persistent approvals for the current
SHA-256 binary and exact manifest scopes. Revocation is re-read on each load;
changed binaries and incomplete approval decisions fail closed. Session-only
installer approval cannot authorize a later run. Grants live in the manager's
plugin data directory as `plugin_cap_grants.json`.

## Security

- **WASM sandboxing**: All plugin code runs inside a `wasmtime` instance with no access to the host OS except through explicitly imported host functions.
- **Fuel-based execution limits**: Each plugin starts with 1,000,000 fuel units; exhaustion traps execution.
- **Epoch-based interruption**: An epoch ticker periodically increments the engine epoch counter; stores set an epoch deadline as a belt-and-suspenders measure.
- **Memory size limits**: StoreLimits enforce 64 MiB linear memory and bounded
  tables/instance counts during manifest extraction, init, and growth.
- **Host effects**: File/shell effects use the active run's ToolExecutor,
  VirtualFs, and configured shell. HTTP initial and redirect URLs each pass
  through shared policy, including global network denial. Init has no run
  authority. Revoked/previous-run handles are disabled.
- **Unauthorized host calls**: If a plugin calls a host function without the required capability, its violation counter is incremented. After 1 violation (`MAX_VIOLATIONS = 1`), the plugin is disabled.
- **Module size limit**: WASM modules larger than 10 MB are rejected at load time.

## Debugging

Common errors and their causes:

| Error | Cause |
|-------|-------|
| `"missing init export"` | Plugin doesn't export `init()` — add the export or use the `plugin_entry!` macro |
| `"ABI version too new"` | Plugin targets a newer ABI (`abi_version > 1`) than the host supports |
| `"sidecar manifest mismatch"` | The `.manifest.json` sidecar does not match the embedded manifest |
| `"capability denied"` | Plugin tried to use a capability not granted by the user |
| `"tool call failed"` | General tool execution error — check plugin logs and `last_error` |
| `"has exhausted its fuel"` | Plugin exceeded its fuel budget (infinite loop or expensive computation) |
| `"init failed with code N"` | Plugin's `init()` returned a non-zero exit code |

### Logging

Plugin log output appears under the `plugin` target in Concerto's `tracing` output. Set `RUST_LOG=plugin=trace` to see verbose plugin logging.

```sh
RUST_LOG=plugin=trace concerto
```

## Architecture

- **Plugin SDK** (`crates/plugin-sdk/`): Guest-side SDK for building plugins. Provides `#![no_std]` ABI helpers and the `plugin_entry!` macro.
- **Plugin Host** (`crates/plugins/`): Runtime for loading and executing WASM plugins via `wasmtime`.
- **PluginManager**: Central lifecycle management — discovery, loading, capability approval, initialisation, tool registration, and lifecycle tracking.
- **CapabilityManager**: Handles capability grants (session + persistent) with scope-aware enforcement (globs, domains, allowlists).
- **ToolBridge**: Wraps plugin-provided tools as `dyn Tool` instances for the agent's `ToolRegistry`.
- **Loader**: Compiles WASM modules, extracts manifests, validates sidecar files, checks ABI version, and calls `init()`.
- **Host Functions**: Exposes `read_file`, `write_file`, `http_get`, `shell_exec`, `emit_event`, `log`, `last_error`, `resize_scratch`, and async `completion`.
