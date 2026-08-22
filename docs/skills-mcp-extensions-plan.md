# Skills + MCP + Extension Manager — Integration Plan

**Status:** Accepted — executable plan (amended 2026-08-04)
**Repo:** `NefaroXX/Concerto` (GitHub) · **Branch:** `feat/skills-mcp-extensions` → PR into `dev`
**Architecture record:** `docs/adrs/ADR-43-skills-mcp-and-extension-manager.md`
**Validation:** Repo fact-check + architecture review completed 2026-08-04; this document
incorporates every correction and amendment from that review. Corrections are marked
**CORR** (factual) and **AMEND** (required change).

---

## 0. Goals and non-goals

### Goals
1. **Skills** — first-class, local skill packs (prompt/context + optional tools) that agents can load per project/session.
2. **MCP** — client support for external MCP servers; expose their tools through the existing `ToolRegistry` / `ToolExecutor` / policy path.
3. **UI manager** — desktop (and minimal CLI) surface to list, enable/disable, configure, and grant capabilities for plugins, skills, and MCP servers.

### Non-goals (v1)
- MCP **server** mode (Concerto as an MCP server).
- Community skill/MCP registry or auto-download marketplace.
- Replacing WASM plugins with MCP (plugins stay; MCP is an additional source of tools).
- Full OS isolation for MCP processes beyond process + policy + allowlists.
- **CORR:** no tabbed-Settings refactor; the Extensions surface uses the existing collapsible-section pattern.

### Design principles (match Concerto)
- Everything that can change the filesystem or run shell still goes through **`ToolExecutor` + policy**.
- Prefer **composition** over new agent loops: skills inject context; MCP/WASM contribute tools.
- **ADR before code** for architectural decisions (ADR-43 is written and accepted).
- Pure Rust; no TypeScript/Bun in this workspace.
- Conventional commits; atomic PRs.

---

## 1. Conceptual model

| Concept | What it is in Concerto | How it reaches the agent |
|---------|------------------------|--------------------------|
| **WASM plugin** | Existing `PluginManager` tools/providers/adapters | Tools registered in `ToolRegistry` |
| **Skill** | Directory or packaged unit: metadata + instructions + optional resource files + optional tool refs | Injected into system/context messages; may enable a subset of tools |
| **MCP server** | External process (stdio) implementing MCP | Tools discovered at connect time → bridged as `dyn Tool` |
| **UI manager** | Settings sections for Plugins / Skills / MCP | Controls enablement, paths, env, grants, health |

**Unified "extension" view (UI only):** one list with type tags `Plugin | Skill | MCP`, backed by three managers underneath.

---

## 2. Validation findings (2026-08-04) — facts the plan was corrected against

| # | Plan claim | Verified fact |
|---|-----------|---------------|
| CORR-1 | Next ADR number is 34 | **ADR-43 is next** (ADR-42 exists, 2026-08-04). ADR index lives in ROADMAP.md §"Architecture decision index" and is missing 41/42 — fix while adding 43. |
| CORR-2 | Workspace has 22 crates | **26 members** (incl. 3 `test-*-plugin-wasm` crates + `eval-runner`). |
| CORR-3 | Settings at `views/settings.rs`, tabs | **Module** `views/settings/mod.rs`, collapsible **sections** (`SectionId`); no tab machinery. Extensions = two new sections. |
| CORR-4 | CLI parses subcommands via clap | **Manual dispatch** in `run_cli_inner()`; add a match arm. |
| CORR-5 | ROADMAP already plans MCP/skills | **No mention anywhere** (incl. "Explicitly deferred") — Phase E adds, not updates. |
| CORR-6 | No MCP/skills infrastructure exists | **Confirmed** (grep: zero MCP refs in code; zero "skill" refs). |
| CORR-7 | Config schema v5 "if needed" | **Needed**: `SCHEMA_VERSION = 4` today; insert-only migrations v1→v4 are the house pattern. |
| CORR-8 | New deps needed (tokio-process, reqwest) | **None**: `tokio` (process) and `reqwest` already workspace-wide. |
| CORR-9 | PluginManager list APIs | `list_plugins()`, `get_plugin_info()` exist with these exact names. |
| CORR-10 | Child-process test pattern exists | **None** — the MCP fixture server is the first; use scripted stdio mock + `tokio::process`. |

---

## 3. Required amendments (from architecture review)

- **AMEND-A1 — Registry collision hardening.** `ToolRegistry::register()` is a bare `HashMap::insert` (silent overwrite, `types.rs:952`). MCP registration must `get()`-check first and fail loudly on collision; validate `server_id` at config load (reject `:`); add a registry collision test.
- **AMEND-A2 — Process lifecycle contract.** `tokio::process::Child` detaches on drop. Copy the `LspClient` shape (`crates/lsp/src/client.rs`: `Option<Child>`, double-spawn guard, kill+wait on stop) **plus** a `Drop` impl, cancel token wired into reader task + call wait, stderr → `tracing`. No orphaned MCP servers on panic/cancel/teardown.
- **AMEND-A3 — Policy gating granularity + default posture.** `Condition::ToolName` is exact-match only; add a **prefix/glob tool-name condition** to `SimplePolicyEngine` so rules like "deny all `mcp:*` except `mcp:github:*`" are expressible. Default posture for unmatched `mcp:*` tools: **RequireApproval** (startup preset rule appended last, overridable) — never implicit auto-approve for opaque network-capable tools.
- **AMEND-A4 — Skill injection on both prompt paths + budget allocator.** Inject into `PromptBuilder` (single-agent) **and** coordinator specialist prompt assembly (`agents/generic.rs::build_prompt`, `memory_prompt.rs`). Skill char budget must participate in the existing `ContextBudgetAllocator` (agent_loop.rs), not a standalone truncation.
- **AMEND-A5 — Per-call MCP timeout.** `Tool` trait has no timeout. MCP bridge: input-driven `timeout_secs` (shell-tool precedent) + hard cap; elapsed → `ToolError::Timeout`, not `Cancelled`.
- **AMEND-A6 — Runtime-owned manager lifetime.** `PluginManager` is built locally in `load_and_configure_plugins` and never exposed to the UI. `McpManager` must be **runtime-owned** (built in the runtime layer, shared `Arc` handed to desktop + CLI handles); Skills/MCP settings sections act on live state. Verify at implementation how desktop currently obtains PluginManager; align.
- **AMEND-A7 — Config v5 mirrors `PluginConfig`.** `[skills]`/`[mcp]` shaped like `PluginConfig` (`enabled`, `search_paths`/`auto_load`; per-server `enabled`/`command`/`args`/`env`), `Option` defaults, insert-only migration v4→v5, `docs/config.toml.example` in the same commit.

---

## 4. Phase A — ADR, types, config

### A1. ADR (done in Task 1)
`docs/adrs/ADR-43-skills-mcp-and-extension-manager.md` — Accepted. Decisions:
1. Skills are **local filesystem packs**, not WASM and not MCP.
2. MCP is **client-only**, tools bridged into the existing `Tool` trait.
3. One UI Extensions surface; three backends (`PluginManager`, `SkillManager`, `McpManager`); implemented as Settings **sections** (Plugins exists; add Skills, MCP).
4. MCP tool names namespaced `mcp:<server_id>:<tool_name>`; `server_id` validated (no `:`); registry registration is collision-checked (AMEND-A1).
5. Skill activation: explicit enable in config + optional per-session toggle; default off for safety.
6. Policy: MCP tools are normal tools under `ToolExecutor`; prefix tool-name condition added (AMEND-A3); unmatched `mcp:*` → RequireApproval preset.
7. Supplement (must be in the ADR): protocol revision pin + Content-Length framing (see §6); per-call timeout default and `ToolError::Timeout` mapping; process lifecycle contract (AMEND-A2); env passing (secrets never in TOML); security boundary = OS process isolation + policy; config v5 insert-only; `EventKind::McpServerStateChanged` variant in core; both prompt paths (AMEND-A4).

### A2. Config schema (v4 → v5)
Extend `concerto-config` + `docs/config.toml.example` (mirrors `[plugins]`):

```toml
[skills]
enabled = true  # default is false (ADR decision 5); opt in per project
search_paths = ["~/.local/share/concerto/skills", "./.concerto/skills"]
auto_load = true
# enabled_ids = ["rust-testing", "commit-style"]  # empty = all discovered when auto_load

[mcp]
enabled = false  # default off until user opts in
# [[mcp.servers]]
# id = "filesystem"
# command = "npx"
# args = ["-y", "@modelcontextprotocol/server-filesystem", "/safe/path"]
# env = {}
# enabled = true
# timeout_secs = 60  # per-call default, hard cap enforced in bridge
```

Keep `[plugins]` as-is. No secrets in TOML (keyring later if MCP needs tokens).

### A3. Core types
TOML config shapes (`SkillsConfig`, `McpConfig`, `McpServerConfig`) live in `concerto-config` (mirrors `PluginConfig`). Shared domain types in `concerto-api-types` (already the home of `PluginManifest`/`ToolDescriptor`; desktop/cli/orchestrator all depend on it):
- `SkillManifest { id, name, version, description, instructions_path or inline, tools: Vec<String>, resources: Vec<PathBuf> }`
- `SkillDescriptor` (loaded, resolved paths)
- `McpTransport::{Stdio, …}`
- `McpToolDescriptor` (from MCP `tools/list`)
- `ExtensionKind { Plugin, Skill, Mcp }` for the UI

### Acceptance (Phase A)
- ADR-43 + ROADMAP index rows (41, 42, 43) merged to `dev` via PR.
- Config parses with defaults; schema v5 migration insert-only; `docs/config.toml.example` updated.
- Types compile; no runtime behavior yet.

---

## 5. Phase B — Skills core

### B1. `concerto-skills` crate (new)
- Depends on **`concerto-core` only** (paths/serde); `SkillManager` takes search paths + enabled ids as parameters — no config dependency, no graph cycle.
- Discovers `SKILL.md` (YAML front matter + body) or `skill.toml` + optional `instructions.md` under search paths; reuses the `PluginDiscoveryCfg` pattern (`runtime_runner.rs:1158`).
- Manifest format (v1): `skill.toml` with `id/name/version/description/instructions/tools`; Markdown-first `SKILL.md` compatible.

### B2. Injection point (AMEND-A4)
- Single-agent: `PromptBuilder` system-message assembly (`crates/orchestrator/src/prompts.rs`).
- Multi-agent: specialist prompt assembly (`agents/generic.rs::build_prompt`, `memory_prompt.rs`).
- Concatenate enabled-skill instructions into a bounded "Skills" section; budget via `ContextBudgetAllocator` (char budget, truncate with clear marker).

### B3. Optional tool scoping
If a skill lists `tools`, only those tools are *suggested* in the prompt; **no silent stripping** of policy-gated tools in v1.

### Acceptance (Phase B)
- Unit tests: discover, parse, enable/disable, budget truncation.
- Single-agent chat shows skill text in context (mock provider / snapshot of built messages).
- Docs: `docs/skills.md` (Phase E).

---

## 6. Phase C — MCP client core

### C1. `concerto-mcp` crate (new)
Depends on `concerto-core` (Tool trait, errors) + tokio (process) + serde/json. **No new heavy deps** (CORR-8).
- **Protocol pin (verified 2026-08 against modelcontextprotocol.io):** MCP stdio JSON-RPC 2.0 with **newline-delimited framing** (one message per line — NOT LSP-style Content-Length, which is an early-SDK artifact). Revision pinned as a crate constant: **`2025-11-25`** (latest fully stable; official SDK `LATEST_PROTOCOL_VERSION`; the 2026-07-28 stateless revision is beta and deferred). See ADR-43.
- Surface: `initialize` + `notifications/initialized`, cursor-paginated `tools/list`, `tools/call` (distinguish `isError` results from JSON-RPC errors), `ping` reply, `notifications/cancelled` on timeout/cancel. No `requests/shutdown` exists in MCP — shutdown is transport-level: close stdin, grace wait, kill (POSIX SIGTERM→SIGKILL escalation).

### C2. Lifecycle (AMEND-A2)
- `LspClient`-shaped: `Option<Child>` + `Option<ChildStdin>`, double-spawn guard, `stop()` = shutdown request then `kill().await` + `wait().await`; **plus** `Drop` impl for panic/teardown paths; cancel token wired into reader task and call waits; stderr → `tracing`.

### C3. `McpManager` (AMEND-A6)
- Started from config, runtime-owned (shared `Arc`), handed to desktop + CLI.
- Health: `Connected / Failed / Disabled`; server crash → tools marked unavailable + `EventKind::McpServerStateChanged { server_id, state, error }` (new core event variant); no panic in the agent loop.
- Each remote tool → `McpTool` implementing `concerto_core::Tool`; registered into the same `ToolRegistry` the agent uses, namespaced `mcp:<server_id>:<tool_name>`, **collision-checked** (AMEND-A1).
- All execution through **`ToolExecutor`** — policy, spend, audit, events apply automatically.

### C4. Timeouts (AMEND-A5)
- Input-driven `timeout_secs` (default from server config, hard cap e.g. 300s); elapsed → `ToolError::Timeout`.

### C5. Policy integration (AMEND-A3)
- New prefix/glob tool-name condition in `SimplePolicyEngine`.
- Startup preset: unmatched `mcp:*` → `RequireApproval`, overridable by explicit rules.
- Example rules documented in `docs/policy-rules.md` (Phase E).

### Acceptance (Phase C)
- Integration test with a **fixture MCP server** (scripted stdio mock in `tests/` — first child-process test in the repo).
- Policy deny blocks MCP tool calls (incl. prefix rules).
- Tool log / audit shows namespaced MCP tools.

---

## 7. Phase D — UI manager

### D1. Desktop: Settings → new sections (CORR-3, AMEND-A6)
`crates/desktop/src/views/settings/` — add `SectionId::Skills` and `SectionId::Mcp` (collapsible sections, not tabs). Palette colors only (`theme.palette.*`); `scripts/check-hardcoded-colors.sh` must pass.
1. **Plugins** — existing section; list from `PluginManager::list_plugins()` / `get_plugin_info()`; status; tools; capability grants; enable/disable unload.
2. **Skills** — discovered skills; toggle enabled; path; instruction preview (read-only).
3. **MCP** — server list; enable; edit command/args/env; connect/disconnect; last error; list of discovered tools.

Actions: reload discovery · test MCP connection. Async actions follow the existing pattern (`ShellProfileTest → ShellProfileTestResult` via `iced::Task`).

### D2. Navigation
Settings sections only; no quick-panel in v1.

### D3. CLI (CORR-4)
New match arm in `run_cli_inner()` — `concerto extensions list|enable|disable` (skills + mcp servers). Non-interactive: config.toml remains source of truth.

### D4. Capability dialog
MCP uses no WASM capability grants — **policy + explicit enable** is the control plane. OS process isolation is the boundary for any network/fs access by the MCP server's own process; document in `docs/mcp.md`.

### Acceptance (Phase D)
- Desktop can enable a skill and see it applied on the next message.
- Desktop can enable an MCP server, see tools, deny via policy, approve and run.
- No hardcoded colors; script passes.

---

## 8. Phase E — Docs, STATUS, tests
- `docs/STATUS.md`, `ROADMAP.md` (add entries — CORR-5), `AGENTS.md` (remove "no MCP infrastructure exists"; document skills), `docs/architecture.md` + `crate-graph.md` (26 crates + new edges).
- New: `docs/skills.md`, `docs/mcp.md`; `docs/policy-rules.md` gains MCP examples.
- Workspace tests green; nextest; no `unwrap` in library code.
- TESTING.md manual matrix row: Skills + MCP on Linux desktop.

---

## 9. Crate / file layout

```
crates/
  skills/          # NEW: discovery, manifest, SkillManager  (dep: core only)
  mcp/             # NEW: client, McpManager, McpTool bridge (dep: core + tokio/serde)
  plugins/         # EXISTING: unchanged ABI
  core/            # policy prefix condition, EventKind variant, ToolRegistry hardening
  config/          # [skills], [mcp], v4→v5 migration
  orchestrator/    # skill injection (both prompt paths, budget allocator)
  desktop/         # Settings → Skills + MCP sections
  cli/             # extensions subcommand
docs/
  adrs/ADR-43-skills-mcp-and-extension-manager.md
  skills-mcp-extensions-plan.md   # this file
  skills.md, mcp.md               # Phase E
```

Wire new crates into workspace `Cargo.toml` (`publish = false` inherited).

---

## 10. Task checklist (execution order)

```text
TASK 1 — ADR + plan        ✔ committed on feat/skills-mcp-extensions
  - docs/adrs/ADR-43-….md (Accepted), ROADMAP index rows 41/42/43
  - docs/skills-mcp-extensions-plan.md (this file)
  - Commit: docs(adr): skills, MCP client, extension manager

TASK 2 — Config + types
  - schema v4→v5 ([skills], [mcp] mirroring PluginConfig), insert-only migration
  - api-types: SkillManifest, SkillDescriptor, McpServerConfig, McpTransport,
    McpToolDescriptor, ExtensionKind
  - docs/config.toml.example
  - Tests: parse defaults, migration v4→v5, server_id validation
  - Commit: feat(config): skills and mcp configuration

TASK 3 — SkillManager
  - concerto-skills crate; discover/parse skill.toml + SKILL.md; enable/disable
  - Unit tests (discover, parse, budget truncation lives in Task 4)
  - Commit: feat(skills): SkillManager discovery and loading

TASK 4 — Skill injection
  - Inject into PromptBuilder + coordinator specialist prompts (AMEND-A4)
  - Budget via ContextBudgetAllocator; truncation marker
  - Tests: message-assembly snapshots; budget enforcement
  - Commit: feat(orchestrator): inject skill instructions into context

TASK 5 — MCP client
  - concerto-mcp crate: stdio JSON-RPC (Content-Length), initialize/tools/list/
    tools/call, lifecycle per AMEND-A2, timeouts per AMEND-A5
  - Fixture stdio mock server in tests/
  - Commit: feat(mcp): stdio MCP client and tool bridge

TASK 6 — McpManager + registry + policy
  - Runtime-owned McpManager; namespaced collision-checked registration (A1)
  - Prefix tool-name policy condition + mcp:* RequireApproval preset (A3)
  - EventKind::McpServerStateChanged; crash/timeout failure modes
  - Tests: fixture E2E, policy deny, audit naming
  - Commit: feat(mcp): McpManager and ToolRegistry integration

TASK 7 — Desktop Extensions sections     ✔ committed on feat/skills-mcp-extensions
  - SectionId::Skills + SectionId::Mcp in views/settings; palette colors only
  - Enable/disable, status, tool list, errors, test-connection
  - Config-driven v1: toggles edit pending config, apply on next run (desktop
    builds fresh SharedServices per run; ADR-43 §3 v1 note); one-off MCP probe
  - CLI: concerto extensions list (D3)
  - Commit: feat(desktop): extensions manager in Settings
  - Verified: fmt, clippy -D warnings (desktop+cli), desktop 244 + cli 98
    tests, color script OK

TASK 8 — Docs + STATUS     ✔ committed on feat/skills-mcp-extensions
  - docs/skills.md, docs/mcp.md, architecture, crate-graph, AGENTS.md,
    STATUS, ROADMAP, TESTING.md matrix row
  - Commit: docs: skills and MCP user and architecture guides

TASK 9 — PR     ✔ PR #111 open into dev (https://github.com/NefaroXX/Concerto/pulls/111)
  - cargo fmt, clippy -D warnings, cargo test (workspace or nextest)
  - Open PR into dev with test notes
```

---

## 11. Risks and mitigations

| Risk | Mitigation |
|------|------------|
| MCP protocol drift | Revision pinned in ADR + crate constant; stdio-only in v1; SSE feature-gated later |
| Context bloat from skills | Hard char budget inside ContextBudgetAllocator; truncate with clear marker (AMEND-A4) |
| Untrusted MCP tools | Default `mcp.enabled = false`; `mcp:*` → RequireApproval preset; prefix policy conditions (AMEND-A3) |
| Registry name collisions | Collision-checked registration; `server_id` validation; registry test (AMEND-A1) |
| Process leaks / orphans | LspClient-shaped lifecycle + Drop impl; cancel wired everywhere; stderr → tracing (AMEND-A2) |
| Hung tools/call stalls loop | Per-call timeout → ToolError::Timeout, input-driven + hard cap (AMEND-A5) |
| UI scope creep / dead toggles | Settings sections only; runtime-owned manager (AMEND-A6) |
| Graph cycles | skills/mcp depend downward on core only; managers get config as parameters |
| CI constraints (1.88 MSRV, 1.96 fmt) | Keep deps to tokio/serde (already in tree); fixture mock server is tiny |
| Library-code panics | No unwrap/expect in library crates (review enforced) |

---

## 12. What "done" means for v1
- User can drop a skill pack into a search path, enable it, and the agent receives its instructions.
- User can configure an MCP stdio server, enable it, see tools, and call them under policy.
- Settings → Skills / MCP sections show plugins, skills, and MCP with status and toggles.
- No MCP infrastructure claim remains in AGENTS.md; STATUS lists Skills/MCP as implemented with limitations.
