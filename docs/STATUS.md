# Concerto current status

**Last reconciled with the source tree: 2026-08-24**

This is the public status source of truth. “Implemented” means the code path
exists and has automated coverage; it does not mean every provider, model,
operating system, or long-running workflow has completed live testing.

## Release stage

Concerto is pre-release software entering wider live testing. Source builds and
the automated workspace checks are the supported distribution path. The
project is not published to crates.io and does not currently promise binary
installer packages.

> **Explicitly deferred** (see `ROADMAP.md`):
> - **Sandbox / execution isolation** — `SandboxProfile::Containerized` is not
>   implemented; WASM capability enforcement is not complete OS-level isolation.
> - **Evaluator end-to-end runner** — the sole end-to-end test in
>   `concerto-eval` (`crates/eval/src/runner.rs:414`) is `#[ignore]`d; the full
>   runner pipeline lacks fast unit coverage.

## Implemented and testable

- **Single-agent execution:** streaming plan/act/observe loop, registered tools,
  bounded continuation, cycle detection, cancellation, budget checks, and
  preservation of partial progress.
- **Multi-agent execution:** Coordinator plus Architect, Researcher, Coder,
  Reviewer, and Validator; dependency-aware task scheduling; configurable
  directed relationships and cycle limits; per-role provider/model assignments;
  shared spend tracking and validation/repair cycles.
- **Configurable orchestration (ADR-58/59 revised):** only the coordinator
  engine is hardcoded — by design; everything else is config data. Open stage
  kinds (six known kinds are vocabulary/defaults, unknown kinds dispatch
  generically), relaxed guardrail rulebook (integrity + safety only), fallback
  personas, per-stage masks/feed/condition/max-cycles. The five specialists
  (architect/researcher/coder/reviewer/validator) are seed templates
  materialized into the project config on first Studio open (silent, idempotent
  auto-seed); once the config owns the roster (custom agents or
  `[orchestration]`), deleted seeds never come back — in the Studio or at
  runtime. Desktop Studio is a one-surface editor: full agent/stage/
  relationship CRUD with a locked coordinator row, free-text stage kinds with
  known-kind suggestions, per-field rulebook validation surfacing, and a
  single-arm Save (merge-aware atomic writes; never navigates). The named
  blueprint catalog (standard/tdd/docs-only/research-only) is seed data;
  Settings→Relationships is hidden while orchestration is active.
- **Recovery:** centralized configurable provider retry/backoff; retry-after support; Coder
  tool errors returned to the model for correction; recoverable specialist
  failures retried with context; exhausted bounded subtask/tool work reported as blocked
  or partially complete instead of a generic internal error.
- **Providers:** OpenAI, Anthropic, Google Gemini, OpenRouter, Ollama, NVIDIA NIM,
  and OpenCode-compatible endpoints through a shared streaming interface.
- **Routing:** explicit role assignments and model overrides; otherwise
  compatible budget-aware selection. Researcher, Coder, and Validator require
  tool-call support. Capability tiers have been removed.
- **Tools and policy:** filesystem and shell tools through `ToolExecutor`,
  first-match policy rules, an independent shell denylist, virtual filesystem
  changes, diff review, and audit events.
- **Intent gate and intent-gated authorization (ADR-55; ADR-56 model-first
  classifier):** deterministic intent router + user confirmation (load-bearing)
  authorizes mutations; an optional LLM intent classifier classifies — never
  grants. ADR-56 makes the classifier the model-first primary decider (default
  flips to enabled; two deterministic fast paths — negation read-only override
  and ≤48-char smalltalk — run before it; the full deterministic chain stays
  the offline/fail-soft fallback), superseding the ADR-55 Phase 2c
  off-by-default, AskUser-only placement. Phase 2 complete as of 2026-08-11 —
  2a status chip (`df5c2e7`) and 2b orchestration depth (`5818642`) landed
  earlier; the remaining v2 items landed via `3cb251d` (audit columns),
  `21d4d3e` (shell containment), `44f2deb` (dialog verification), `9ec27f3`
  (2c classifier).
- **Persistence:** SQLite sessions, append-only audit log, replay primitives,
  shared token/USD spend tracking, and project-scoped state.
- **Memory:** SQLite FTS5/vector hybrid retrieval with reciprocal-rank fusion,
  local `fastembed` embeddings, tree-sitter AST chunking, project
  isolation, a debounced file watcher for re-indexing, and automatic
  full-text-only fallback when local embeddings are unavailable.
- **Desktop:** Iced chat, agent graph, memory explorer, tool log, diff viewer,
  integrated terminal, settings, screenshot capture, provider/model quick
  selection, policy editor, relationship manager, and shell profiles. The
  Dashboard page is removed: recent sessions live in Chat's empty state, live
  session spend in the status-bar chip (palette warning at ≥80% of the session
  cap, danger at ≥100%), and a Spend Log modal lists persisted per-call spend
  records.
- **CLI:** independent `ratatui` frontend with chat, approvals, diffs, provider
  setup, and optional multi-agent execution.
- **Shells:** installed host shells are detected; users may add custom profiles;
  one authoritative agent shell selection is shared with validation and the
  desktop terminal. The `concerto-shell` crate also contains a typed AI-native
  command runtime foundation.
- **Evaluation:** 10 standard tasks, categorized `bug_fix`,
  `library_with_tests`, `simple_cli_tool`, and `small_web_api` tasks, plus
  multi-agent/reviewer scenarios. Coverage tracking support for `cargo llvm-cov`
  and `cargo tarpaulin` with line/function/branch metrics surfaced in validator
  output.
- **Extensibility:** WASM tool/provider/memory-adapter plugin loading, manifests,
  capability checks, lifecycle management with grant TTL/hash pinning/revocation
  (ADR-37), guest SDK, and three example plugins (tool, provider, adapter).
- **Skills (ADR-43):** local instruction packs (`skill.toml` or `SKILL.md` +
  resources) discovered by `SkillManager` (`crates/skills`) and injected into
  every prompt path as one budgeted, truncation-marked `## Skills` section by
  `SkillsContext` (`crates/orchestrator/src/skills_context.rs`). Skills never
  execute code. `[skills]` config is disabled by default.
- **MCP client (ADR-43):** stdio-only JSON-RPC servers via `concerto-mcp`
  (protocol pin `2025-11-25`, newline-delimited framing). Tools register
  collision-checked as `mcp:<server_id>:<tool_name>` and run through the normal
  `ToolExecutor` (policy, spend, audit, events); unmatched `mcp:*` defaults to
  `RequireApproval`. Server crash clears its tools and publishes
  `EventKind::McpServerStateChanged`. `[mcp]` config is disabled by default.
- **Skills/MCP surfaces (v1):** desktop Settings has config-driven Skills and
  MCP collapsible sections (enable + probe; edits take effect next run); the
  CLI adds read-only `concerto extensions list`.
- **API and observability:** authenticated Axum API/SSE routes, OpenAPI support,
  Prometheus, OpenTelemetry OTLP HTTP, Langfuse export, and structured tracing.

## Maturing in live tests

- Multi-agent output quality and task decomposition across providers with
  different tool-call formats and rate limits.
- Recovery during real connectivity loss, retry backoff, cancellation, and
  model-generated invalid tool calls.
- Cross-platform shell discovery, quoting, working-directory behavior, and
  selected-shell consistency across agents, validation, and terminal.
- Policy usability and the safety/automation balance of starter rules.
- Memory restart, re-index, project switching, first-use embedding download,
  and spend-record persistence.
- Long autonomous multi-file changes and the usefulness of partial results when
  validation cannot be completed.
- MCP server discovery, crash recovery, and cross-platform stdio behavior with
  real servers; skill-pack authoring ergonomics.

The expected manual release checks are maintained in [Testing](../TESTING.md).

## What's new (unreleased)

- 2026-08-24: **ADR-60 Phases 1–4 — supervised orchestrator + replay harness (branch `fix/orchestrator-supervisor-wiring`)** — Phase 1 thin-slice supervisor wiring (agent-process facade `gate_proxy.rs`/`supervisor.rs`, `Completed` semantics, budget-overrun 600s teardown); Phase 2 whiteboard-verified plan binding (plan_id + session bound to `CoordinatorAgent`, verified via whiteboard); Phase 3 review-cycle resumability Deferred 3 (whiteboard `ReviewStatePayload` events, WAL-before-invoke snapshots, `load_review_resume` rehydration, crash-safe replay); Phase 4 consolidation + replay harness + interpreter-probe fix (`agent_loop.rs` `python`→`python3` probe, Windows 577/1→578/0, Linux 578/0). CI verified 2026-08-24 Linux: fmt ✅ clippy ✅ build ✅ wasm ✅ deny ✅ ui-colors ✅ — workspace 3011 passed / 0 failed (orchestrator 578/0).
- 2026-08-20: **ADR-60 D5 always-on write-conflict detection** on branch
  `fix/orchestrator-always-on-base-version` — versioned gated writes now carry
  per-target `base_versions` claims (`GateRequest.base_version` single became a
  `base_versions: BTreeMap` map with `#[serde(default)]` wire back-compat;
  `gate::versioned_targets()` is the single source of truth; move claims
  source+destination, copy claims destination only, the copy source is
  attributed-but-not-claim-stamped). The supervisor stamps claims at request
  arrival (`stamp_base_versions`), so conflicts surface as retryable tool
  errors (`IpcErrorCode::Conflict` `-32005`, zero whiteboard rows) instead of
  silent last-writer-wins. The legacy single-agent loop gets the same
  protection in-process via `InProcessGateBackend` (WAL in the session DB;
  falls back to the plain executor with a loud `warn!` when the pool is
  unavailable). Fresh-create concurrent creates stay last-writer-wins by
  design.
- **Streaming chat reveal (issue #147):** the desktop chat previously showed
  final messages instantly; the TUI had none. `Message::AddAssistant` seeds
  the reveal at ~500 chars/s on the 16 ms tick; auto-finalizes; cached
  `MarkdownDoc` parses re-render, not re-parse; thinking previews, entrance
  fades, and palette shimmer join; no `streaming: true` entry persists. TUI
  parity: `UiLine` lines, per-line state on `App`, 16 ms poll (100 ms idle)
  at the same 8 chars / 16 ms; restored transcripts are not animated.
- 2026-08-15: ADR-58/59 revised in place (maintainer direction) — open stage
  kinds, config-owned rosters with no seed resurrection, silent auto-seed of
  the five specialist templates, one-surface Studio CRUD with locked
  coordinator and single-arm Save (`1052b94`, `1d6d7e2`, `f8a6f79` on `dev`).
- **Audit remediation Phases 0–6 complete (PR #77):** the audit-finding
  remediation programme is merged to `dev`, including session scoping,
  approval timeouts, transactional persistence, VFS determinism, checkpoint
  v3, the durable typed transcript, and automated acceptance-bar gaps.
- **Durability run ADR-34→40 complete:** durable orchestration runtime
  (ADR-34), tag-driven agent orchestration (ADR-35), durable typed transcript
  (ADR-36), plugin grant lifecycle (ADR-37), async WASM host functions
  (ADR-38), embedder degradation handling (ADR-39), and append-only audit log
  with session pruning (ADR-40) are all implemented on `dev`. The per-ADR
  details are listed in the [ROADMAP "Now" section](../ROADMAP.md) and in each
  ADR.
- **Codebase-world-class Phase 0 merged (PR #63):** `missing_docs` policy on
  crate roots, proptest for the shell parser, LSP integration tests, and
  `run_shared_agent` decomposition. Phases 1–5 are pending — see
  [TODO.md](TODO.md).
- **Hybrid chat-centric layout (minimal scope):** Diff, Agent Graph, and Tool
  Log now open as overlay modals within the Chat canvas instead of switching
  pages. Keyboard shortcuts toggle: Ctrl+D (Diff), Ctrl+L (Tool Log). Tool Log
  removed from sidebar — accessible via shortcut. Settings and Studio remain
  full pages (Memory and Terminal moved to the quick panel / bottom panel; the
  Dashboard page was later removed — see the spend-surfaces note below). Merged
  via PR #49. See
  [hybrid-ui-plan.md](hybrid-ui-plan.md).
- **Spend surfaces; no Dashboard page (ADR-41, PRs #98–#101):** the Dashboard
  page is removed — recent sessions live in Chat's empty state, live session
  spend shows in the status-bar chip (palette warning at ≥80% of the session
  cap, danger at ≥100%), and a Spend Log modal (SubView overlay) lists
  persisted per-call spend records. The runtime publishes the existing
  `SpendUpdated`/cap events after each settled call and persists one
  `SpendRecord` per call (multi-agent records carry the root task id). CSV
  export and the Ctrl+Shift+S shortcut are gone from the desktop UI (the CLI
  remains the file-export path); daily-total output is stubbed until daily
  tracking is enabled.
- **Plugin execution for providers and memory adapters:** WASM plugins can now
  export `call_provider` and `call_adapter` functions alongside the existing
  `call_tool`. Host-side `PluginBackedProvider` and `PluginBackedVectorStore`
  wrappers implement `LlmProvider` and `VectorStore` respectively. Guest SDK
  macros `plugin_entry_provider!` and `plugin_entry_adapter!` generate the WASM
  exports. Plugin-provided providers are auto-discovered at load time.
- **Skills and MCP (ADR-43):** two new crates — `concerto-skills` (instruction
  packs, never executed) and `concerto-mcp` (stdio JSON-RPC servers, protocol
  `2025-11-25`). MCP tools are namespaced `mcp:<server_id>:<tool_name>`,
  collision-checked at registration, policy-gated with a `RequireApproval`
  default for unmatched `mcp:*`, and cleared (with a state-change event) when a
  server crashes. Config schema v5 adds `[skills]` and `[mcp]`. Desktop
  Settings gains config-driven v1 Skills/MCP sections (next-run semantics); the
  CLI adds `concerto extensions list`.
- **Vector-store dependency removed.** The optional feature-gated store was
  deleted in pre-release cleanup; `SqliteVectorStore` (SQLite + cosine
  similarity) is the only vector store. Cold compile time dropped from ~8
  minutes to ~30 seconds.
- **`VectorStore` trait moved to `concerto_core`.** The trait now lives in
  `concerto_core::VectorStore`, eliminating `concerto-memory` as a dependency
  of `concerto-plugins` and removing the transitive vector-store dependency.
- **Bounded, observed durable event-bus backlog:** durable subscribers (spend
  tracking, audit, replay) stay lossless but warn at 4096 pending events and
  latch a lag health flag at 65536, surfaced via `EventBus::durable_health()`;
  dead subscribers are pruned on publish and subscribe.
- **Debounced memory re-index watcher:** 1s debounce with a bounded,
  deduplicated queue and a rate-limited overflow warning instead of unbounded
  queue growth.
- **Embedder degradation handling (ADR-39):** embed failures write no
  zero-vector rows on any vector backend; per-project health with exponential
  backoff (5s→120s);
  semantic search degrades to full-text-only with a user-visible
  `EmbedderDegraded` notice in the CLI and recovers automatically.
- **Size-bounded application log rotation:** 5 MiB max per file with 2 backups,
  fail-safe to stderr; `concerto logs show` is unchanged.
- **`concerto sessions prune` subcommand:** maintenance-only, opt-in pruning
  with required `--older-than <days>` and `--keep <n>` (default 5 protects the
  newest), `--dry-run` preview, and `--all-projects` (default scopes to the
  current project); active sessions are always skipped; the session and its dependent
  rows (messages, events, spend, tasks, checkpoints, transcript) are deleted in
  one transaction; the audit-log decision trail is preserved (ADR-40, migration
  021 detaches `session_id` via `ON DELETE SET NULL` instead of deleting).

- `SandboxProfile::Containerized` is not implemented, and WASM capability
  enforcement is not complete OS-level isolation.
- SQLite is the only vector-store backend.
- The AI-native shell is not yet the desktop terminal runtime and does not yet
  implement the full self-improving workflow described in its plan.
- Model metadata and prices can become stale; explicit provider/model
  assignments remain authoritative.
- Coverage infrastructure supports `cargo llvm-cov` and `cargo tarpaulin` but
  coverage collection requires the relevant tool to be installed.

### Incomplete implementations (stubs / dead code paths)

These are structural stubs — code that compiles and has tests but does not
actually work, in order of impact:

- **FTS sync score default is misleading** (`sync.rs:59,90`). Embedding records
  are synced to FTS with `score: 1.0` and comment `// neutral default for
  stored chunks; query-time FTS rank or vector similarity overwrites this`.
  Downstream budget/filter code (`budget.rs`, `system.rs`) may inspect the
  stored `score` outside of query-time score assignment, which is why the sync
  layer writes a neutral `1.0` instead of a zero that could make stored chunks
  look irrelevant. RRF fusion in `rag.rs` uses rank position, not the stored
  score. Tracked as a TODO entry ("FTS BM25 ranking not wired").
- **Eval runner integration test is `#[ignore]`d**
  (`crates/eval/src/runner.rs:414`, `concerto-eval`). The sole end-to-end test
  for task discovery → execution → reporting is skipped by default because it
  compiles a Rust project. No fast unit test covers the runner pipeline.
  Tracked as a TODO entry ("Un-ignore eval end-to-end test").
- **Forward-compat panic footgun in the agent loop** (`agent_loop.rs:393`).
  `AgentRunExit` is `#[non_exhaustive]` (4 variants today), which forces a
  `_ => unreachable!()` wildcard arm in the continuation match. All four
  variants are explicitly covered today, so the arm is genuinely unreachable —
  but if a 5th variant is ever added to `concerto-core`, this arm silently
  becomes a runtime panic instead of a compile error. Fix would be to return
  an explicit error (e.g. `AgentLoopError`) in the wildcard arm when the enum
  gains variants.

## In-flight branches

- **`feat/streaming-chat-reveal`** — closed: merged into `dev` via PR #151
  (2026-08-15): typewriter reveal wired into the desktop chat completion path
  with cached markdown parsing, thinking/tool-chip/completion entrance
  animations, and TUI reveal parity (issue #147).
- **`feat/ui-depth-improvements`** is closed — merged into `dev` via PR #97
  (2026-08-03): hybrid UI medium scope (terminal bottom panel with drag resize,
  Memory Explorer as a compact quick-panel section, glass modals and
  overlay/panel animations, chat timestamps and transcript format v2, blinking
  streaming cursor). See [hybrid-ui-plan.md](hybrid-ui-plan.md).
- **`feat/codebase-world-class`** is closed — merged into `dev` via PR #63;
  Phase 0 is complete and Phases 1–5 are pending (see [TODO.md](TODO.md)).

## Tracked follow-ups

Non-blocking work captured during the ADR-59 P4 close-out (2026-08-15).
None are defects; each is tracked here so nothing found in review is lost.

1. **Test-infra: `CONFIG_ENV_LOCK` env-restore race hardening** (low, ~30
   min). A few guarded-init/save tests in `crates/desktop/src/app.rs` restore
   `XDG_CONFIG_HOME` only at the end of the test, so an assertion panic can
   leak the redirect into parallel tests. Fix: restore before assertions (the
   panic-safe pattern the other tests already follow) or an RAII guard. No
   behavior change.
2. **UI polish: glyph-font coverage** (non-blocking, verified). The Studio's
   Unicode glyphs (`🛡 ➜ ⛓ ⚠ ▴ ▾ ·` in
   `crates/desktop/src/views/orchestration_studio.rs`) rely on system-font
   coverage; no bundled icon font or explicit iced fallback. All glyphs are
   paired with text labels, so worst case is cosmetic tofu. Future options
   (ranked): swap orphan glyphs for text (S), load a symbol fallback font
   (M), bundle an icon set (L).
3. **UI polish: multiline system-instructions** (non-blocking, documented).
   The fallback-persona "System instructions" input is a single-line
   `text_input` per the repo's long-text pattern (Inspector precedent); a
   future slice could upgrade it to the iced `text_editor` widget with its
   own edit plumbing + tests.
4. **Deferred per ADR-59** (roadmap, not defects): TOML diff view of include
   changes; canvas DAG editor (P6); Studio Simple tier + migration runner +
   export-merge hardening (P5); run-one-stage simulation (P6). See the
   ADR-59 Status record and ADR-58 phasing table.
5. **Release readiness: orchestration-editor manual checklist** — six rows
   added to `docs/live-test-template.md` (auto-seed on first open, roster CRUD
   save round-trip after restart, deleted-seed persistence across restart,
   unknown-kind stage round-trip, Settings→Relationships hide, validation
   surfacing). Run before the next release; automated tests cannot see
   rendering/layout issues.

## Immediate release priorities

1. Complete the manual test matrix on Windows and at least one Linux desktop.
2. Exercise single- and multi-agent Build with more than one provider family.
3. Confirm retry resume and cancellation during both generation and backoff.
4. Confirm policy defaults, selected-shell propagation, memory restart/re-index,
   and spend chip/record persistence.
5. Record failed scenarios as reproducible issues and keep the workspace checks
   green after each fix.

Longer-term work belongs in [ROADMAP.md](../ROADMAP.md), not in this status
document.

## Design reference

- [Hybrid chat-centric UI plan](hybrid-ui-plan.md) — architectural shift from
  flat page navigation to a chat-centric layout with inline panels and modals.
  Minimal scope (SubView overlays for Diff, Agent Graph, Tool Log) is merged on
  `dev` (PR #49) and medium scope via PR #97 (2026-08-03); full scope is
  tracked in [TODO.md](TODO.md).
