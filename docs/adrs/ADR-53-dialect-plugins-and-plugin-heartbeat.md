# ADR-53: Dialect plugins and plugin heartbeat (Phase 6)

**Status:** Accepted (2026-08-08) — implemented in the same-or-next commit.
**Date:** 2026-08-08
**Deciders:** Concerto architecture
**Supersedes:** Phase 6 of the provider-first redesign plan
    (`docs/ARCHITECTURE-V2.md`, §7 "Phase 6 (later) — plugins for novel
    dialects, heartbeats, merge leftovers").
**Composes with:** ADR-14 (WASM plugin architecture), ADR-37 (plugin
    capability lifecycle), ADR-46/ADR-48 (reasoning-as-data, ContextEngine —
    the Phase-2 dialect seam), ADR-26 (fault containment — heartbeat keepalive).

## Context

Phase 6 of the V2 redesign calls for making novel provider wire dialects
(vLLM, Cohere, custom endpoints) expressible without recompiling the host.
The plugin host already exists for tool / provider / memory-adapter plugins
(`crates/plugins`), but the **provider plugin** path today is a single fixed
shape: `PluginBackedProvider::build_request_json`
(`crates/plugins/src/provider_host.rs:42-52`) hardcodes one
OpenAI-chat-completions-shaped request body and hands it to
`call_provider("complete", …)`. Any provider whose wire format differs from
that one shape is inexpressible as a plugin today — the wire serialization is
baked into the host.

Two structural observations bound the fix:

- **The request side is the open gap.** The Phase-2 dialect seam
  (ARCHITECTURE-V2 §2.1, ADR-46/ADR-48) already defines the intended
  interface: a `Dialect` adapter *lowers* the canonical request to wire bytes
  and *parses* the stream back to canonical events, with transport (HTTP,
  SSE, retry, auth, keepalive) living in the host. A WASM plugin is the
  natural way to make that seam extensible — but only the *serialization*
  half needs to move into WASM; transport and response parsing must stay in
  the Rust host, where the tested transport machinery (timeouts, retries,
  SSE, cancellation) already lives.
- **A plugin call is a single await, not a stream.** `stream_completion`
  awaits `plugin.call_provider("complete", …)` for the whole completion. A
  slow plugin therefore appears dead to downstream consumers that rely on
  stream liveness — there is no per-token streaming through WASM today
  (pragmatic v1), and none is being introduced here.

The gap is therefore: (1) a plugin kind that serializes the request body
into the plugin's dialect, host-side transport untouched; and (2) a liveness
signal while the host awaits a slow plugin completion.

Explicitly **not changed** by this ADR:

- the existing `call_tool` / `call_provider` / `call_adapter` ABI v1 exports
  and the plugin load path — **no breaking change, no `abi_version` bump**;
- the OpenAI-shaped body built by `build_request_json` for provider plugins
  that declare no dialect — bit-for-bit current behavior;
- the single-agent / multi-agent transports, `agent_loop`, `runtime_runner`.

## Decision

Ship a new plugin kind — `DialectPlugin` — that implements provider **wire
serialization only**, plus an optional completion keepalive. Transport and
response parsing stay in the Rust host; the plugin is a pure
string → string transformer behind a string-based host wrapper.

### 1. New plugin kind `DialectPlugin` with a 2-op ABI

- **Manifest**: `PluginProvides` gains the variant `Dialect` (externally
  tagged, `#[non_exhaustive]`). A plugin whose manifest provides `Dialect` is
  a **DialectPlugin**: it implements provider wire serialization only.
- **ABI export**: one new export, aligning with the existing
  `call_tool`/`call_provider`/`call_adapter` convention (linear memory +
  scratch buffer, 6 params, `i64` packed ptr/len result):

  ```
  call_dialect(op_ptr, op_len, input_ptr, input_len, scratch_ptr, scratch_len) -> i64
  ```

  Two ops are defined; any other op returns the error
  `"unsupported operation"`:

  - **`"render"`** — input
    `{"request": <canonical chat-completions-shaped JSON as today>, "model": "...", "echo": "always"|"if-present"}`
    → output the **wire JSON string** for the plugin's dialect. The `request`
    is exactly the canonical OpenAI-shaped body `build_request_json`
    produces today; the plugin re-renders it into its own wire format. The
    `echo` field carries the reasoning-echo policy (`"always"` |
    `"if-present"`, ADR-46) so the dialect can place
    `reasoning_content`/thinking content correctly.
  - **`"cache"`** — input a wire JSON string → output the **modified** wire
    JSON applying the dialect's cache-control semantics (prompt-cache
    breakpoints, pooled-prefix markers; ADR-48 §3). Dialects without
    caching return the input unchanged (no-op).

- **Guest SDK**: a new `plugin_entry_dialect!` macro generates the
  `call_dialect` export and the `manifest()`/`init()`/scratch plumbing for a
  guest implementing the dialect trait:

  ```rust
  const fn name() -> &'static str
  fn render_chat_body(request_json: &str, model: &str, echo: &str) -> Result<String, String>
  fn apply_cache_breakpoints(body_json: &str) -> Result<String, String>
  ```

- **Host**: new `crates/plugins/src/dialect_host.rs` — `DialectHost` wraps an
  [`ActivePlugin`] and exposes the two ops as **string-based** methods
  (`render(request_json, model, echo) -> Result<String, PluginError>` and
  `apply_cache_breakpoints(body) -> Result<String, PluginError>`). There is
  **no cross-crate trait dependency**: orchestration of a plugin-provided
  dialect happens entirely inside the plugins crate, and `provider_host`
  consumes the produced wire **string**.

### 2. `PluginBackedProvider` optional dialect — additive, v1 intact

`PluginBackedProvider` (`crates/plugins/src/provider_host.rs`) gains an
**optional dialect**: when the provider plugin declares a `Dialect` (and the
user's plugin config selects it, §3), `stream_completion` uses
`DialectHost::render` for the request body (then `apply_cache_breakpoints`)
instead of the hardcoded OpenAI shape. When no dialect is declared, the
existing `build_request_json` + `call_provider("complete", …)` path is
**unchanged**. This is additive-only — **NO breaking change, NO
`abi_version` bump**.

### 3. Explicit selection + compatibility

A dialect is used **only when the user's plugin config declares it**. The
manifest declaring `provides: [Dialect(…)]` is what makes the capability
discoverable; the user's config is what activates it. Default behavior and
all existing provider plugins are unaffected — a plugin that declares no
Dialect and a user who configures none get today's exact code path.

### 4. Plugin heartbeat — keepalive, no streaming ABI

- `PluginProvides::Provider`'s descriptor (`ProviderDescriptor`) gains an
  optional **`heartbeat_interval_secs: Option<u32>`**.
- While `stream_completion` is **awaiting the plugin call future**, it emits
  a liveness marker on that interval: `CompletionChunk::keepalive()` — an
  additive constructor producing an empty, non-terminal chunk that downstream
  consumers treat as a no-op, with zero delta (the canonical `KeepAlive` event
  already foreseen by ARCHITECTURE-V2 §2.1).
- **No per-token streaming through WASM in this ADR** — streaming is
  deferred (§Consequences). The heartbeat only proves the host-side await is
  alive; it does not stream plugin output.

### 5. Dialect plugins are pure functions

Dialect plugins perform **no host calls** — no host-function surface, no
capability grants beyond the standard plugin sandbox. They receive and
return strings. Fuel and memory limits apply as with every plugin
(`PluginHost::MAX_FUEL`, `DEFAULT_MAX_MEMORY`, scratch-buffer capacity).

## Consequences

- **Positive.** Novel wire dialects become expressible without recompiling
  the host: vLLM / Cohere / custom endpoints can be added as a DialectPlugin
  on the request side, while all tested transport (HTTP, SSE, retry, auth,
  cancellation) stays host-side. Slow plugin completions no longer look
  dead: the keepalive keeps downstream consumers honest on liveness. The
  Phase-2 dialect seam is honored with the plugin as the extensible half.
- **Negative / trade-offs.** The v1 dialect surface is **request-side only**:
  `render` and `cache` produce the wire body, and the host parses the
  response through the existing completion-response parsing. A dialect whose
  *response* shape differs from the chat-completions shape needs the
  deferred response-side work (§Deferred). Dialects opt in via config, so a
  user who wants one must configure it — no auto-detection.
- **Security.** Dialect plugins are pure string transformers confined to the
  WASM sandbox: no host calls, standard fuel/memory limits, and the
  existing plugin load/verify path. Because they never touch the network or
  the filesystem, the plugin capability model (ADR-37) is not extended; the
  transport they serialize for remains host-owned and policy-visible.
- **Migration.** None. The `Dialect` variant, the optional
  `heartbeat_interval_secs`, and the `CompletionChunk::keepalive()` marker are all
  additive serde/default additions; existing manifests, configs, and
  consumers load unchanged. No `abi_version` bump.
- **Deferred.** Per-token streaming through WASM (and the response-dialect
  parsing it implies); an MCP-server heartbeat/ping surface (the MCP client
  heartbeat stays as-is from ADR-43 — server-side liveness is not this
  ADR's concern). Out of scope, unchanged by this ADR: the deferred
  ADR-45 config items each stay exactly where the plan parked them — the
  per-role fallback-disable switch, the per-tier retry/backoff counts, and
  ladder-locked tier targeting remain unratified config-surface work, not
  dialect/plugin work.

## Review notes

- The 2-op ABI (`render`, `cache`) is deliberately minimal: rendering is the
  whole serialization seam, and caching is the one place a dialect needs
  wire-level control beyond a pure shape change (cache-control / pooled
  prefix semantics, ADR-48 §3). Everything else — parse, stream, auth,
  retry — stays host-side, so the plugin's surface cannot grow unbounded.
- `DialectHost` is string-based on purpose: a `Dialect` trait living in a
  cross-crate crate would put plugin orchestration outside the plugins
  crate; strings keep the boundary a pure ABI and let `provider_host`
  consume the wire body without new crate dependencies.
- The keepalive is emitted only *while awaiting the plugin call future*; it
  is not a general provider transport heartbeat and does not promise plugin
  progress — only host-side liveness. Streaming the actual completion
  through WASM remains deferred and is the follow-up that would make the
  keepalive redundant for the stream case.
