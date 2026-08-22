# ADR-21: WASM Plugin Implementation — Runtime, Host ABI, Capability Model

> **Archived** — superseded by [ADR-14](../ADR-14.md) (consolidated 2026-08-22),
> which now carries the full living plugin-system design this ADR recorded;
> async host functions are specified separately in
> [ADR-38](../ADR-38.md). See [docs/adrs/README.md](../README.md) for the
> current index. Retained as the implementation-phase record; not active
> guidance.

**Status:** Superseded by ADR-14 consolidation

## Context

ADR-14 established the high-level plugin architecture (WASM with capability-secure host ABI). This ADR records the detailed implementation decisions that arose while implementing it — decisions that ADR-14 intentionally left open.

### Requirements

1. **Runtime choice**: pick a WASM runtime and justify.
2. **Host function ABI**: define the calling convention, error propagation, and scratch-buffer protocol.
3. **Capability grant lifecycle**: design how grants move from manifest → user approval → runtime enforcement → persistence.
4. **Scoping**: decide which plugin types reach production readiness and which are deferred.
5. **Manifest source**: define where plugin manifests live and how conflicts are resolved.
6. **Async strategy**: decide how to handle async host functions in a synchronous WASM guest model.

## Decisions

### 1. Runtime: `wasmtime` 28.x

Use `wasmtime` (not wasmer). Rationale:

- **Pure Rust** — no C/C++ bindings, no FFI risk, aligns with the project's "pure Rust" principle.
- **Strong sandbox** — fine-grained `Config` controls for WASM features, memory limits, and CPU fuel.
- **Best WASI trajectory** — primary maintainer of the WASM spec and WASI proposals.
- **Async support** — `Func::new_async` enables bridging async host operations into the synchronous WASM execution model.
- **License** — Apache-2.0, compatible with the project's license.

Pinned to `wasmtime = "28"` at workspace level (upgrading to v43+ requires a `func_wrap` API migration; tracked in `deny.toml`). Disable features not needed: `cache`, `debug-builtins`, `profiling`. Keep `cranelift` as the default compiler.

**Config restrictions applied at engine creation:**

| Feature | Setting | Rationale |
|---------|---------|-----------|
| `wasm_multi_memory` | `false` | Single memory simplifies ABI |
| `wasm_simd` | `false` | Reduces attack surface, not needed |
| `wasm_bulk_memory` | `true` | Needed for `memory.copy` / `memory.fill` |
| `wasm_reference_types` | `false` | Not required |
| `wasm_tail_call` | `false` | Not required |
| `max_memory_size` | 256MB | Per-plugin memory cap |
| `async_support` | `true` | Enables optional async host functions |

### 2. Host Function Signature Convention

All host functions share a common calling convention:

```
Return type: i64 — packed (ptr, len) for success, RESULT_ERROR (-1) for error
Argument convention: (ptr: i32, len: i32) for string parameters
Scratch buffer: (scratch_ptr: i32, scratch_len: i32) for return data
```

**Packed return encoding:**

```rust
const RESULT_ERROR: i64 = -1i64;

fn pack_ptr_len(ptr: i32, len: i32) -> i64 {
    (ptr as i64) << 32 | (len as i64 & 0xFFFF_FFFF)
}
```

**Error propagation:**

- Functions that return data use `pack_ptr_len(ptr, len)` for success.
- `RESULT_ERROR` (`-1i64`) indicates an error occurred.
- After any error, the guest can call `concerto_last_error(scratch_ptr, scratch_len) -> i64` to read a UTF-8 error string into its scratch buffer.
- `concerto_last_error` returns `pack_ptr_len` for the error string, or `RESULT_ERROR` if no error is pending.

**Memory safety (hard rule):** Every host function MUST validate WASM linear memory bounds before reading or writing:

```rust
let mem = caller.get_export("memory").and_then(|e| e.into_memory()).ok_or("no memory")?;
let data = mem.data(&caller);
if (ptr as usize).checked_add(len as usize).map_or(true, |end| end > data.len()) {
    return Err(PluginError::MemoryViolation { ptr, len });
}
```

### 3. Capability Grant Lifecycle

Grants flow through three stages:

1. **Manifest declaration** — plugin declares required capabilities in `PluginManifest.capabilities_required`.
2. **User approval** — at load time, the user sees the requested capabilities in a dialog (Iced) or stdin prompt (CLI) and chooses: Allow, Deny, or Allow This Session.
3. **Runtime enforcement** — every cap-gated host function checks the grant set before executing. If the capability was not granted, the function returns `RESULT_ERROR` and sets `last_error`.

**Grant persistence:**

- "Allow" → stored in the capability store as `HashMap<PluginId, HashSet<CapabilityRequestDiscriminant>>`.
- "Allow This Session" → held in-memory only (`GrantedCapabilities.session_grants`).
- Revocable from Settings panel (desktop) or CLI; see ADR-37 for the TTL / hash-pinning / revocation lifecycle.

**Capability matching at host-function call time:**

- `CapabilityRequestDiscriminant` is used for coarse-grained approval matching.
- Path/domain patterns within a capability (e.g., specific file globs or URL domains) are checked at call time inside the host function against the full `CapabilityRequest`.

### 4. Host Import Table

| Name | Signature | Capability | Status |
|------|-----------|------------|--------|
| `concerto_log` | `(level_ptr, level_len, msg_ptr, msg_len) -> ()` | None | Ships |
| `concerto_emit_event` | `(event_json_ptr, event_json_len) -> ()` | None | Ships |
| `concerto_read_file` | `(path_ptr, path_len, scratch_ptr, scratch_len) -> i64` | `FilesystemRead` | Ships |
| `concerto_write_file` | `(path_ptr, path_len, content_ptr, content_len) -> i32` | `FilesystemWrite` | Ships |
| `concerto_http_get` | `(url_ptr, url_len, scratch_ptr, scratch_len) -> i64` | `NetworkOutbound` | Ships |
| `concerto_completion` | `(req_ptr, req_len, scratch_ptr, scratch_len) -> i64` | None (provider call) | Implemented (see below) |
| `concerto_last_error` | `(scratch_ptr, scratch_len) -> i64` | None | Ships |
| `concerto_resize_scratch` | `(new_size: i32) -> i32` | None | Ships |

### 5. Async Strategy

The `completion_request` host function (the only inherently async one) was first shipped as a sync stub, then implemented via wasmtime's async support — the final design is specified in ADR-38 (`Config::async_support(true)`, `func_wrap_async`, shared runtime handle, caller cancellation threading).

### 6. Scratch-Buffer Protocol

The guest exports a static scratch buffer (default 64 KiB) as a WASM memory region. The host validates the symbol exists during `init()` and records its address and length.

**Flow:**
1. Guest passes `(scratch_ptr, scratch_len)` to host functions that return data.
2. Host writes the result JSON into the scratch buffer, then returns `pack_ptr_len(ptr, len)` of the written data.
3. If the result exceeds `scratch_len`, the host returns `RESULT_ERROR` and sets last-error to `"scratch_overflow:<required_bytes>"`.
4. Guest may call `concerto_resize_scratch(required_size)` to grow WASM memory and retry.

**Memory allocation convention:**
- Guest allocates its own memory for arguments (strings it passes to host).
- Guest provides a scratch buffer for returns.
- `concerto_resize_scratch` removes the need for a guest-side allocator for most cases.

### 7. Manifest Dual-Source

Plugin manifests can come from two sources:

1. **Embedded** — WASM custom section read at module load time.
2. **Sidecar** — `plugin.toml` file next to the `.wasm` file.

**Resolution order:**
1. Sidecar file is read first (if present).
2. Embedded manifest is extracted from the WASM module.
3. If both exist, they MUST match. Mismatch → `PluginError::ManifestMismatch`, plugin refused.
4. If only one exists, use it.

This enables plugin authors to provide metadata without recompiling, while the embedded manifest serves as the authoritative source of truth.

### 8. ABI Versioning

- `PluginManifest.abi_version` is checked at load time.
- Host declares `HOST_ABI_VERSION` constant (baseline: `1`).
- If plugin `abi_version > HOST_ABI_VERSION` → `PluginError::AbiTooNew`.
- If plugin `abi_version < HOST_ABI_VERSION` → host MAY load with backwards-compatible imports (baseline: load = 1 only).
- ABI version is bumped when breaking changes are made to host import signatures or semantics.

### 9. Engine Sharing

- Single shared `wasmtime::Engine` for compiled-module cache across all plugins.
- Per-plugin `Store` for isolation of plugin mutable state.

This avoids recompiling modules on every load while keeping plugin state fully isolated.

## Consequences

### Positive

- Clear, simple host ABI that any language targeting WASM can implement.
- Capability model that prevents ambient authority without requiring OS-level sandboxing.
- Single-engine architecture is efficient for multi-plugin workloads.
- Scratch-buffer protocol avoids requiring a WASM allocator for simple plugins.
- Dual manifest sources enable flexible development workflows.

### Negative

- Async host functions require `wasmtime`'s `Func::new_async`, which adds complexity when enabled (ADR-38).
- Scratch-buffer retry loop adds a small constant overhead for large return values.
- Without WASI, plugin authors must use the SDK crate or manually implement the ABI.

---

*Last updated: 2026-08-22 (retrospective consolidation — see [README](../README.md)).*
