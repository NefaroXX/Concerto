# ADR-38: Async WASM Host Functions (wasmtime `async_support`) for Plugin Host Calls

**Status:** Accepted — implemented (2026-08-02)
**Date:** 2026-08-02
**Deciders:** Concerto architecture

## Context

Audit finding **H4** of the long-session robustness audit: the plugin host
creates a **new tokio `Runtime` per host-function call** from WASM plugins.

In `crates/plugins/src/host_fns.rs` (~lines 516-522), each host call that needs
to reach async host services (provider streams, event bus, capability dialogs,
filesystem) constructs `tokio::runtime::Runtime::new()` and calls `block_on` on
it, passing a fresh `CancellationToken`:

- `Runtime::new()` spawns a new OS thread per call. On hot paths (per-message
  provider calls) this thread-spawn cost is significant and unbounded.
- The fresh `CancellationToken` means a cancelled agent run is invisible to
  in-flight host calls — they cannot observe the parent cancellation and keep
  running to completion.

Constraints framing this decision:

- **wasmtime is pinned to v28.0.1** (see `deny.toml` justification: upgrading
  to v43+ requires a `func_wrap` API migration). Do **not** upgrade as part of
  this work.
- Current execution model: `Config::async_support(false)` (the default), sync
  `Linker::func_wrap` callbacks, `Func::call` from sync contexts, plus the
  per-call `Runtime::new()` bridging described above.

## Decision

1. **Enable `Config::async_support(true)`** on the shared engine and migrate
   plugin host functions to wasmtime's async API: `Linker::func_wrap_async` +
   `Func::call_async` (the canonical wasmtime v28 pattern).
2. **Eliminate per-call `Runtime::new()` + `block_on`.** Host functions run on
   the plugin host's async runtime. Where wasmtime async cannot drive a given
   call site (e.g. sync execution paths inside the host that invoke wasm), use
   ONE shared runtime handle (`tokio::runtime::Handle`) with
   `block_on`/`spawn_blocking` — never a fresh `Runtime` per call.
3. **Thread the caller's `CancellationToken` into plugin store data** so async
   host functions observe agent cancellation instead of a fresh per-call token.
4. **Keep the execution-safety belts intact**: fuel consumption, epoch
   interruption (with the deadline correction from audit finding M2), memory
   reservation, module size cap, and `MAX_VIOLATIONS` fail-closed behavior.
5. **Constraints honored**:
   - All host functions must be registered via `func_wrap_async` when called
     through async paths — a sync `func_wrap` cannot be invoked from an async
     caller and vice versa; the `Linker` registration must be consistent per
     function.
   - `PluginStoreData` must remain `Send` — it is (plain data types + `Arc`
     handles).
   - Wasm execution inside a poll is synchronous, so long-running async host
     work must not block the wasmtime async executor — use
     `epoch_deadline_async_yield_and_update` and/or
     `spawn_blocking`/`Handle::block_on` for host-side awaits as appropriate.
6. **Scope guard**: this is an internal host-mechanics change. The plugin ABI
   (linear memory + scratch buffer + exported
   `allocate`/`deallocate`/`call_tool`/`call_provider`/`call_adapter`) and the
   guest SDK (`plugin_entry!` macros) are **unchanged**; the three plugin kinds
   (Tool, Provider, MemoryAdapter) keep their existing interfaces.

## Consequences

### Positive
- Hot-path host calls no longer pay the `Runtime::new()` thread-spawn cost.
- Cancellation propagates from the agent run to in-flight plugin host calls.
- The host-function layer takes the canonical wasmtime v28 async shape.

### Negative / Risks
- Async mode changes execution semantics. Any missed conversion of a sync
  `func_wrap` to `func_wrap_async` (or a sync `call` of an async function) is a
  compile error, but subtle regressions — store data `Send` bounds,
  re-entrancy, `block_on` deadlocks if called from within the async executor
  thread — must be covered by the full plugin integration test suite
  (`crates/plugins/tests/integration.rs`) and the WAT-compiled
  provider/adapter plugin tests.

### Fallback
- If a call site cannot be made async cleanly, the shared-`Handle` `block_on`
  pattern is acceptable per call — but **never** a fresh `Runtime`.

## Alternatives Considered

- **(a) Keep per-call `Runtime`** — rejected: thread-per-call cost on hot
  paths, and no cancellation visibility.
- **(b) Upgrade wasmtime to v43+** — rejected: `deny.toml` pins v28 pending a
  `func_wrap` migration; out of scope for this audit.
- **(c) Shared `Runtime::block_on` with sync `func_wrap`** — acceptable
  interim, but `async_support` is the canonical v28 path and allows proper
  cancellation.
- **(d) Drop wasm host-call bridging entirely; move plugins to an
  out-of-process model** — rejected: far larger scope.

## Relationship to Other ADRs

- Continues the wasmtime sandbox model (ADR-21) with an execution-mechanics
  change inside the host, not a policy change.
- Related to ADR-37 (plugin grant lifecycle — per-call TTL enforcement) — the
  grant checks run through the same host-call path that becomes async.
- The audit fix commits on `fix/long-session-robustness` also cover M2 (epoch
  deadline correction) and H3 (revoke signaling); ADR-38 addresses H4.
