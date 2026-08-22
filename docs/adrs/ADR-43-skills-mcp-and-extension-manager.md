# ADR-43: Skills, MCP client, and extension manager

**Status:** Accepted (2026-08-04)
**Date:** 2026-08-04
**Deciders:** Concerto architecture
**Extends:** ADR-14 (plugin architecture — WASM), ADR-37 (plugin capability grant lifecycle), ADR-26 (fault containment and recovery)
**Plan:** `docs/skills-mcp-extensions-plan.md`

## Context

Concerto extends itself through WASM plugins (tools, providers, memory adapters).
Two gaps remain before it can be a practical local-first agent platform:

1. **Skills.** There is no mechanism to carry reusable, project- or session-scoped
   instruction packs ("prefer cargo nextest", "no unwrap in library crates",
   team style guides). Agents today receive instructions only via config
   prompts, which are global and coarse-grained.
2. **MCP.** The Model Context Protocol ecosystem is the de-facto standard for
   exposing external tools (filesystem, GitHub, databases, browsers). Concerto
   has zero MCP support; users with existing MCP servers cannot use them, and
   the WASM plugin ABI is not a drop-in replacement for the MCP ecosystem.
3. **Management surface.** Plugins are manageable today (Settings → Plugins,
   capability grants, CLI), but there is no unified view for plugins + skills +
   MCP, and `PluginManager` is not runtime-owned, so the desktop cannot act on
   live state.

Constraints from the codebase: everything that touches the filesystem or runs
shells goes through `ToolExecutor` + `SimplePolicyEngine`; `ToolRegistry::register`
silently overwrites on name collision (`crates/core/src/types.rs:952`);
`Condition::ToolName` is exact-match only (`crates/core/src/policy.rs:107`);
`tokio::process::Child` detaches on drop (orphan risk); config schema is v4 with
insert-only migrations; workspace MSRV 1.88, fmt/clippy pinned to 1.96.0.

## Decision

Ship skills, an MCP **client**, and a unified extension-management surface in
one feature train, governed by the decisions below.

### 1. Skills are local filesystem packs — not WASM, not MCP

A skill is a directory (or packaged unit) under configured search paths
containing `skill.toml` or `SKILL.md` (YAML front matter) plus optional
`instructions.md` and resource files. The manifest carries
`id/name/version/description/instructions/tools`. Skills are discovered and
parsed by a new `concerto-skills` crate depending on `concerto-core` only;
`SkillManager` receives search paths and enabled ids as parameters (no config
dependency, no graph cycle). Skills never execute code; they inject text.

### 2. MCP is client-only; remote tools bridge into the `Tool` trait

A new `concerto-mcp` crate implements the MCP **client** side (stdio
transport only in v1; SSE feature-gated later). Each remote tool becomes an
`McpTool` implementing `concerto_core::Tool`
(`execute(input: serde_json::Value, policy, session, cancel) -> Result<ToolOutput, ToolError>`),
registered in the same `ToolRegistry` the agent uses. Concerto does **not**
become an MCP server in v1, and WASM plugins are not replaced.

**Protocol pin:** MCP stdio JSON-RPC 2.0 with **newline-delimited framing** —
one JSON-RPC message per line on stdin/stdout (this is the normative framing
in every published spec revision; LSP-style Content-Length framing is an early
SDK artifact and is NOT used). Protocol revision pinned as a crate constant:
**`2025-11-25`** (the latest fully stable revision; the official SDKs'
`LATEST_PROTOCOL_VERSION`; the 2026-07-28 stateless revision is beta and
explicitly deferred). Surface implemented: `initialize` handshake +
`notifications/initialized`, cursor-paginated `tools/list`, `tools/call`
(with `isError` and JSON-RPC error channels distinguished), `ping` reply,
`notifications/cancelled` on timeout/cancel. There is no `requests/shutdown`
method in MCP — shutdown is transport-level: close stdin, wait for exit with a
grace period, then kill. Timeouts: input-driven `timeout_secs` (server
default, hard cap) mapping elapsed calls to `ToolError::Timeout`, never
`Cancelled`.

### 3. One "Extensions" surface, three backends, Settings sections

The UI exposes one extension view with type tags `Plugin | Skill | MCP`,
backed by `PluginManager` (existing), `SkillManager` (new), `McpManager`
(new). In the desktop this is two new collapsible **sections** in
 `views/settings` (`SectionId::Skills`, `SectionId::Mcp`) beside the existing
 Plugins section — no tabbed-Settings refactor. `McpManager` is **runtime-owned**:
constructed in the runtime layer as a shared `Arc` in `SharedServices`; CLI gets
a minimal `extensions list` subcommand arm.

> **v1 scoping note (Task 7, Aug 2026).** The desktop builds a *fresh*
> `ServicesBuilder` per agent run and does not hold the runtime-owned managers,
> so the v1 desktop sections are **config-driven**: toggles edit the pending
> config and take effect on the next run, and MCP offers a one-off
> "Test connection" probe (temporary client, spawn → initialize → list_tools →
> stop). The runtime still owns the real `McpManager`; handing a live handle to
> a persistent desktop state is deferred until the desktop holds shared
> services across runs.

### 4. MCP tool naming and registry safety

MCP tools are namespaced `mcp:<server_id>:<tool_name>`. `server_id` is
validated at config load (non-empty, no `:`). Because
`ToolRegistry::register` silently overwrites on collision, MCP registration
performs a `get()`-check first and fails loudly with a descriptive error;
duplicate server ids and collisions with existing plugin/MCP tools are
rejected, not silently clobbered. A registry collision test is added.

### 5. Skill activation is explicit and default-off

Skills activate only when enabled in config (`[skills]` section:
`enabled`, `search_paths`, `auto_load`, optional `enabled_ids`) plus an
optional per-session toggle; default state is off. Enabled-skill instructions
are injected into **both** prompt paths — the single-agent `PromptBuilder`
(`crates/orchestrator/src/prompts.rs`) and the coordinator specialist
assembly (`agents/generic.rs::build_prompt`, `memory_prompt.rs`) — under a
hard character budget enforced inside the existing `ContextBudgetAllocator`,
truncated with a clear marker. Skill `tools` lists are prompt-level
suggestions only in v1; no silent stripping of policy-gated tools.

### 6. MCP tools are ordinary tools under `ToolExecutor`

MCP tool execution goes through `ToolExecutor` exactly like every other tool:
policy, spend, audit, and events apply unchanged. To make server-level rules
expressible, `SimplePolicyEngine` gains a **prefix/glob tool-name condition**
(alongside exact `ToolName`). Default posture for unmatched `mcp:*` tools is
**RequireApproval** via a startup preset rule appended last (overridable by
explicit user rules) — never implicit auto-approve for opaque, network-capable
tools. `mcp.enabled` defaults to `false` until the user opts in.

### 7. Process lifecycle and security boundary

MCP servers run as child processes. Lifecycle follows the `LspClient` shape
(`Option<Child>` + `Option<ChildStdin>`, double-spawn guard, shutdown request
then `kill().await` + `wait().await` on stop) **plus** a `Drop` impl so no
server is orphaned on panic, cancellation, or teardown; cancel tokens are
wired into the reader task and call waits; stderr is captured to `tracing`.
Server crash marks its tools unavailable and emits a new
`EventKind::McpServerStateChanged { server_id, state, error }` event; the
agent loop never panics on MCP failure. **The security boundary is OS process
isolation plus policy** — MCP servers are not run inside the WASM sandbox and
Concerto does not mediate their side effects beyond policy gating and
explicit enablement. Environment passed to servers comes from config `env`;
secrets are never stored in TOML (keyring integration deferred until needed).

### 8. Config: v4 → v5, insert-only

`[skills]` and `[mcp]` sections mirror the existing `PluginConfig` shape
(`enabled`/`search_paths`/`auto_load`; per-server `enabled`/`command`/`args`/
`env`/`timeout_secs`). `SCHEMA_VERSION` bumps 4 → 5 with an insert-only
migration (defaults only, no field deletion), per the existing
`migration.rs` policy. `docs/config.toml.example` is updated in the same
commit. TOML config shapes (`SkillsConfig`, `McpConfig`, `McpServerConfig`)
live in `concerto-config` mirroring the `PluginConfig` precedent; the shared
domain types (`SkillManifest`, `SkillDescriptor`, `McpTransport`,
`McpToolDescriptor`, `ExtensionKind`) live in `concerto-api-types`, which
desktop, CLI, and orchestrator already depend on.

## Consequences

- **Positive.** Skills become portable, project-local context packs; the MCP
  ecosystem becomes usable under the existing policy/spend/audit machinery;
  one Settings surface manages all extension kinds; no new agent loop; no new
  heavy dependencies (tokio process + serde already in the workspace).
- **Negative / trade-offs.** MCP tools are opaque to policy internals (only
  name-level gating plus explicit enablement; `DenyNetworkEgress` cannot see
  inside a server's traffic); MCP servers are trusted processes by design;
  skills add context-window pressure that must be budgeted; a new child-process
  surface adds lifecycle risk (mitigated by the Drop contract above).
- **Deferred.** MCP server mode; SSE transport; marketplace/registry;
  per-skill WASM code execution; keyring-backed MCP tokens; tabbed Settings;
  live (non-config) desktop toggles via a held runtime handle (see §3 v1 note).
- **Consistency.** Wasmtime pin (v28), cargo-deny exceptions, and the
  no-`unwrap`-in-library rule are unaffected.

## Related ADRs

- ADR-14 — plugin architecture (WASM): plugins stay; MCP is an additional tool source.
- ADR-37 — plugin capability grant lifecycle: unchanged; MCP uses policy, not grants.
- ADR-26 — fault containment and recovery: MCP crash handling follows its principles.
- ADR-16 — context overflow strategy: skill budget rides the existing allocator.
