# ADR-14: Plugin Architecture — WASM with Capability-Secure Host ABI

**Status:** Accepted
**Date:** 2025-07-14
**Deciders:** Concerto architecture

## Context

The roadmap defines a plugin system (Phase 7) that lets third-party code
extend Concerto with custom tools, LLM providers, and memory adapters
without modifying the core binary. Phase 2 designs the plugin *contract* —
the types and traits that define what a plugin is and how it declares what
it needs — but does not implement the WASM runtime or plugin loader, which
remain Phase 7.

Three requirements drive the plugin architecture:

1. **Isolation:** a buggy or malicious plugin must not crash the host, leak
   memory, or access filesystem paths outside its declared scope.
2. **No ambient authority:** a plugin cannot do anything its manifest has
   not explicitly requested — no reading `~/.ssh/` unless it declared
   `FilesystemRead { globs: ["~/.ssh/**"] }`.
3. **Language-agnostic:** external developers should not be forced into
   Rust. The plugin ABI must be reachable from any language that compiles
   to WASM (or can embed a WASM runtime).

## Decision

Use **WebAssembly (WASM)** as the plugin runtime, with a **capability-based
security model** where the plugin declares its needs in a `PluginManifest`
at registration time, and the host grants only those capabilities at
instantiation.

### Host ABI Shape

The plugin lifecycle has three phases:

**1. Registration** — the host reads `PluginManifest` from the WASM module's
custom sections or a sidecar `plugin.toml`:

```rust
pub struct PluginManifest {
    pub id: String,
    pub version: String,
    pub name: String,
    pub description: String,
    pub capabilities_required: Vec<CapabilityRequest>,
    pub provides: Vec<PluginProvides>,
}
```

**2. Instantiation** — the host allocates a WASM `Store`, injects only the
requested capabilities as imported functions, and calls the plugin's `init`
entry point.

```rust
pub enum CapabilityRequest {
    FilesystemRead  { globs: Vec<String> },
    FilesystemWrite { globs: Vec<String> },
    NetworkOutbound { domains: Vec<String> },
    ShellExecute    { allowlist: Vec<String> },
}
```

**3. Invocation** — the host calls plugin-provided tools via a
JSON-serialised request/response contract over WASM linear memory:

```rust
pub enum PluginProvides {
    Tool(ToolDescriptor),
    Provider(ProviderDescriptor),
    MemoryAdapter(AdapterDescriptor),
}
```

### Key Design Constraints

- **No ambient filesystem access.** WASM has no native filesystem access;
  the host provides `fd_read`/`fd_write`-style imports only for paths the
  manifest declared. This is enforced by the WASM runtime (wasmtime or
  wasmer), not by convention.
- **Tools are the primary extension surface** (`PluginProvides::Tool`).
  Provider and memory adapter plugins share the same manifest shape but are
  secondary — most plugins are expected to provide tools.
- **JSON over shared memory.** Plugin ↔ host communication uses a simple
  request/response encoding: the host writes a JSON request into WASM
  memory, calls an exported function, and reads the JSON response. No
  shared‑memory concurrency or channels — the plugin is synchronous from
  the host's perspective.
- **Manifest is sidecar, not in-code.** The `PluginManifest` can be embedded
  in the WASM binary as a custom section or provided as a separate
  `plugin.toml` alongside the `.wasm` file. Both paths produce the same
  `PluginManifest` struct at registration time.

### Approval Flow Deferred to Phase 3

The roadmap's Phase 2 plan proposed a `MockApprovalChannel` test helper for
the policy engine's `RequireApproval` verdict. During implementation it was
determined that the approval channel abstraction requires a UI consumer
(Phase 3's CLI/desktop) to be useful — without one, `RequireApproval`
verdicts correctly map to `ToolError::PolicyDenied` in the executor. The
approval channel type (`ApprovalTx`/`ApprovalRx`) and its mock will be
designed and implemented in Phase 3 alongside the approval UI.

## Consequences

- **Strong isolation guarantees by construction.** WASM's sandbox is not an
  afterthought — it is the runtime's core security property. A plugin
  reading `~/.ssh/id_rsa` without declaring it is impossible at the WASM
  level, not just a convention.
- **Language-agnostic ABI.** Any language with a WASM target (Rust, C/C++,
  Go, Zig, AssemblyScript) can write plugins. The ABI is JSON-over-linear-
  memory, not Rust-specific type layout.
- **Plugin size and startup cost.** WASM modules are typically 100s of KB,
  and wasmtime instantiation is sub-millisecond for simple modules. This
  meets the "no perceptible startup delay" requirement for tool plugins.
- **Runtime dependency in Phase 7.** The plugin types designed in Phase 2
  have zero runtime cost — they are pure data structures. The WASM runtime
  (`wasmtime` or `wasmer`) is added in Phase 7 when the loader is
  implemented.
- **Simpler-than-WASI surface.** WASI's full POSIX-like surface is not
  appropriate here. We provide a narrow, capability-gated import surface
  that exposes only what the plugin explicitly requested — strictly less
  authority than WASI preview 2 would grant by default.
- **Tools are synchronous for now.** Long-running plugin operations (e.g. a
  custom provider streaming tokens) will need an async ABI extension in
  Phase 7. The Phase 2 surface assumes synchronous request/response, which
  covers the vast majority of tool plugins.

## Alternatives Considered

- **Native dynamic linking (`dlopen` / `LoadLibrary`):** no sandbox, no
  capability gating. A plugin can call `libc::system`, read any file, crash
  the host with a segfault, or leak memory that the host cannot reclaim.
  Rejected because isolation is a hard requirement.
- **Lua / Rhai / embedded scripting:** excellent isolation story, easy for
  plugin authors, but limits plugin authors to the embedded language (or a
  Rust-to-Lua/Rhai binding generator). WASM lets authors use their language
  of choice and compile to the common ABI.
- **gRPC / sidecar process:** strongest isolation (separate process, OS
  security boundaries), but adds the operational complexity of managing
  subprocess lifecycle, port allocation, and IPC serialisation latency.
  Suitable for future "remote plugin" support (Phase 8), but overkill for
  the local-plugin use case that covers 95% of expected plugins.
- **`tower-lsp`-style protocol (stdio IPC):** simple to implement, but
  shares the subprocess lifecycle complexity of gRPC without the structured
  RPC framework — every plugin is a separate binary. WASM is a single file
  with no external dependencies.

---

*Last updated: 2026-08-22 (retrospective consolidation — see [README](./README.md)).*
