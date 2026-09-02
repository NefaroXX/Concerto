# AGENTS.md — Concerto

Compact, verified guidance for AI coding agents in this repo. Trust executable
sources (Cargo.toml, CI, scripts) over this prose. Last reconciled with the
current workspace layout and CI.

## What this repo is
A pure-Rust Cargo workspace (crates/): a local-first, policy-governed AI coding
agent. `concerto-core` is the foundation; everything depends upward on it. There
is **no TypeScript/Bun code** despite earlier plans — don't look for it.

## Workspace layout (24 crates)
All members live under `crates/` and depend upward on `concerto-core`.
- `core` — traits, `EventBus`, `SimplePolicyEngine`, `ToolExecutor`, error taxonomy
- `providers` — LLM providers + factory/routing: OpenAI, Anthropic, Google, OpenRouter, Ollama, Nvidia NIM, OpenCode Zen
- `orchestrator` — `AgentLoop` (single-agent) + `CoordinatorAgent` (multi-agent, 5 specialists, write gates)
- `tools` — `VirtualFs` overlay + shell/git tools (all policy-gated)
- `shell` — typed AI-native command runtime consuming canonical shell profiles
- `sessions` — SQLite persistence, audit log, replay, spend tracking
- `memory` — SQLite FTS5+vector hybrid search, local embeddings, re-index watcher
- `config` — layered `figment` config + keyring credential storage
- `desktop` — Iced 0.14 GUI (chat, diffs, agent graph)
- `cli` — `ratatui` TUI
- `plugins` / `plugin-sdk` — WASM plugin host ABI + capability manager + guest SDK
- `skills` — local instruction-pack discovery/loading (`SkillManager`); never executes code
- `mcp` — MCP stdio client + `McpManager` + `McpTool` bridge
- `api-types` / `api-server` — shared types + Axum API
- `eval` / `eval-runner` — benchmark harness + runner binary
- `lsp`, `observability`, `concerto` (entry binary)
- `test-plugin-wasm`, `test-provider-plugin-wasm`, `test-adapter-plugin-wasm` — WASM plugin examples/end-to-end tests

Deeper maps: `docs/architecture.md`, `docs/crate-graph.md`, `ROADMAP.md`.

## Build & verify commands
```bash
cargo build --workspace                 # full build (needs system deps, see below)
cargo fmt --all -- --check              # rustfmt: max_width=100, use_small_heuristics="Max"
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace                  # or: cargo nextest run --workspace (CI)
cargo test -p concerto-<crate>          # single crate
cargo nextest run -p concerto-<crate> <testname>   # one focused test
cargo deny check                        # licenses / duplicates / advisories (deny.toml)
cargo audit
```
- **Build location:** never clone, build, test, or set `CARGO_TARGET_DIR` to a
  path under `/tmp`. This environment mounts `/tmp` as a RAM-backed filesystem;
  keep Cargo artifacts in the repository's `target/` directory and use
  disk-backed repository worktrees for isolation.
- GitHub Actions at `.github/workflows/ci.yml` runs fmt, clippy, test, and
  deny as separate jobs without `needs:`, so they run in parallel (alongside
  build/wasm/ui-color jobs). Match `.github/workflows/ci.yml` locally before
  pushing.

- CI sets `RUSTFLAGS="-D warnings"`; treat rustc warnings as errors locally too.
  `[workspace.lints]` already denies `clippy::all` and `unsafe_code`, so never
  add `unsafe` and keep warnings clean.
- CI pins fmt/clippy to toolchain **1.96.0**; the workspace MSRV declared in
  `Cargo.toml` is **1.88**. Use a current stable locally — just keep fmt/clippy
  output matching 1.96.0.

## Non-obvious environment requirements
- **System libraries**: a from-scratch full-workspace build needs a C toolchain
  and platform development libraries used by protobuf/SQLite/TLS/keyring and
  Iced/wgpu (X11/Wayland/GL/Vulkan on Linux). The self-hosted runner already has
  them; package names differ across distributions.
- **WASM target**: CI runs `rustup target add wasm32-wasip2` before building —
  required to compile any `test-*-plugin-wasm` crate. Those crates fail without
  the target.
- **Keychain in tests**: test-mode credential access is selected explicitly per
  call via `CredentialStore::from_env()` (env-var backed, no keyring). The only
  test reaching the real keychain (`config::credentials::default_store_is_not_test_mode`)
  asserts constructor semantics without touching the backend, so no special
  environment is required; `CONCERTO_TEST_MODE=1` is documented for parity with
  CI but is not read by any code.
- **Repo-local opencode config**: `opencode.json` exists (schema-only, no
  overrides) and `.opencode/skills/concerto-maintainer/SKILL.md` carries the
  repo-maintenance instruction pack. Instruction sources: this file,
  `CONTRIBUTING.md`, `SECURITY_BOUNDARIES.md`, and that skill.

## Conventions that differ from defaults
- **Branching**: feature/fix branches must be merged to `dev` via PR. `main`
  is never worked on directly.
- **Commits**: conventional `type(scope): summary`; atomic, one logical change;
  short `fix/`, `feat/`, `refactor/`, or `docs/` branches. Commit history
  doubles as session memory — explain *why* in the body.
- **No `unwrap()`/`expect()` in library crates** (enforced by review).
  `expect()` allowed only in a binary `main()` for mandatory startup config,
  with an explicit message.
- **`CancellationToken`** is threaded through every async op; a missing one is a
  defect, not a style nit.
- **ADRs before code**: new architectural decisions get `docs/adrs/ADR-NN.md`
  *before* dependent code. Don't quietly contradict a settled ADR — write a
  superseding one. ADR numbers are never reused; superseded ADRs stay with
  Status updated, never deleted.
- **UI colors**: desktop `views/` and `ui/` must use palette colors
  (`theme.palette.*`), never `Color::from_rgb`/`from_rgba`,
  `Color::{BLACK,WHITE,TRANSPARENT}`, or hex literals. Enforced by
  `scripts/check-hardcoded-colors.sh` in CI; `widgets/` and `theme/` are exempt.

## Gotchas — do NOT "fix" these without review
- **cargo-deny exceptions are intentional**: many `RUSTSEC-*` advisories are
  ignored for unmaintained *transitive* deps, and `wasmtime` is pinned to
  **v28** (v43+ needs a `func_wrap` API migration). Read `deny.toml`
  justification/scope before changing anything.
- **Dependencies**: historical phase comments are not authorization to retain
  unused crates. Add a dependency only for a current, tested need. Wasmtime is
  pinned to the patched 24.0.x line for the current RustSec advisory; do not
  move it to an unpatched release without checking the advisory and API.
- **Policy gates**: every file write / shell / git op is policy-gated and
  reversible via git stash/branch rollback. Don't bypass `SimplePolicyEngine` or
  `VirtualFs`.

## Key entrypoints (start reading here)
- Single-agent loop: `crates/orchestrator/src/agent_loop.rs`
- Supervised agent-process (ADR-60 S5): `crates/orchestrator/src/gate_proxy.rs` (agent-process facade), `crates/orchestrator/src/supervisor.rs` (Completed semantics, ADR-60 S5)
- Multi-agent coordinator: `crates/orchestrator/src/coordinator.rs`
- Provider abstraction + factory: `crates/core/src/traits/provider.rs`, `crates/providers/src/factory.rs`
- Tool execution safety: `crates/tools/src/virtual_fs.rs` (`VirtualFs`), `crates/core/src/policy.rs`
- Plugin provider execution: `crates/plugins/src/provider_host.rs` (`PluginBackedProvider`)
- Plugin memory adapter execution: `crates/plugins/src/memory_adapter_host.rs` (`PluginBackedVectorStore`)
- Generic WASM dispatch: `crates/plugins/src/active_plugin.rs` (`call_json_export`)
- Plugin manager (load, create, collect): `crates/plugins/src/manager.rs`
- Guest SDK macros: `crates/plugin-sdk/src/lib.rs` (`plugin_entry!`, `plugin_entry_provider!`, `plugin_entry_adapter!`)
- Skill discovery/loading: `crates/skills/src/manager.rs` (`SkillManager`), `crates/skills/src/lib.rs`
- MCP stdio client: `crates/mcp/src/client.rs` (`McpClient`); runtime manager: `crates/mcp/src/manager.rs` (`McpManager`); tool bridge: `crates/mcp/src/tool_bridge.rs`
- Skill injection into prompts: `crates/orchestrator/src/skills_context.rs` (`SkillsContext`)
- App roots: `crates/desktop/src/app.rs` (Iced), `crates/cli/src/` (ratatui)
- Event bus (everything flows through it): `crates/core/src/event.rs`

## Plugin system (WASM)
- Three plugin kinds: `ToolPlugin`, `ProviderPlugin`, `MemoryAdapterPlugin`.
- Each kind has a corresponding host-side wrapper and guest-side SDK macro.
- The ABI uses linear memory + scratch buffer + exported functions (`allocate`, `deallocate`, `call_tool`, `call_provider`, `call_adapter`).
- Plugins are loaded from configured `.wasm` files; MCP tools arrive from external stdio servers via `concerto-mcp` (see Skills & MCP below).
- Provider and memory-adapter plugins are auto-discovered at CLI startup via `PluginManager::collect_providers()` / `collect_memory_adapters()`. The single-agent loop uses the first discovered plugin-backed provider if configured.
- Integration tests in `crates/plugins/tests/integration.rs` cover WAT-compiled provider and adapter plugins end-to-end.

## Skills & MCP (ADR-43)
- Skills are local instruction packs (`skill.toml` or `SKILL.md` + resources) discovered by `SkillManager` (`crates/skills/`) and injected into prompts by `SkillsContext` (`crates/orchestrator/src/skills_context.rs`); they never execute code. Config: `[skills]` (`enabled` default **false**, `search_paths`, `auto_load`, `enabled_ids`, `max_chars`).
- MCP servers are stdio child processes (protocol pin `2025-11-25`, newline-delimited JSON-RPC — no `Content-Length` framing). Tools are namespaced `mcp:<server_id>:<tool_name>`, collision-checked on registration, and policy-gated like any tool: unmatched `mcp:*` → `RequireApproval` preset. `[mcp]` defaults to disabled; no secrets in TOML.
- Docs: `docs/skills.md`, `docs/mcp.md`. Desktop Settings → Skills/MCP are config-driven v1 (next-run semantics); CLI has `concerto extensions list`.

## LanceDB feature gate
LanceDB support was removed in pre-release cleanup; `SqliteVectorStore` is the only vector store.

## Where to learn more
- `README.md` (status, quick start), `CONTRIBUTING.md` (process), `SECURITY_BOUNDARIES.md`
- `docs/architecture.md`, `docs/crate-graph.md`, `docs/adrs/`, `ROADMAP.md`, `docs/STATUS.md`
- `TESTING.md` (automated checks + manual release verification sheet)
- CI: `.github/workflows/ci.yml` (GitHub Actions).
