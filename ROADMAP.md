# Concerto roadmap

This document describes active priorities. It is not a completion claim. The
original phase plan is preserved by Git history and the architectural decisions
in `docs/adrs/`; current behavior is documented in
[Current Status](docs/STATUS.md), and the fine-grained pending-work list lives
in [docs/TODO.md](docs/TODO.md).

## Product direction

Concerto aims to be a local-first automation environment in which AI agents can
work on real projects for long periods without hiding decisions, discarding
useful progress, or requiring a restart after every recoverable problem.

The design priorities are:

1. correct, reviewable changes;
2. recovery and continuity;
3. explicit user control over providers, models, tools, shells, and cost;
4. reproducible evaluation rather than unverified capability claims;
5. native performance and composable Rust components.

## Now (dev, implemented)

The 24-crate workspace has completed the audit-remediation programme
(Phases 0–6, PR #77) and the ADR-34 through ADR-40 run of durability and
lifecycle decisions. What is true on `dev` today:

- **Durable orchestration runtime (ADR-34):** one provider retry boundary
  (time-to-first-byte and idle timeouts, never replaying a specialist run),
  orchestration checkpoints persisted to the session database with fallible
  restore, budget-driven deterministic context compaction, bounded parallel
  specialist execution.
- **Tag-driven agent orchestration (ADR-35):** the five built-in specialists
  are config-driven seeds backed by `GenericSpecialistAgent`; typed
  `SubmitDesignDocInput` contract with generated schemas and structured repair;
  validator-owned acceptance with artifact/verification evidence.
- **Durable typed session transcript (ADR-36):** `transcript_entries` table,
  tool-call correlation, live and restored frontends render the same sequence.
- **Plugin capability grant lifecycle (ADR-37):** 30-day TTL, SHA-256 hash
  pinning, revocation via `concerto plugin list|revoke` and desktop Settings.
- **Async WASM host functions (ADR-38):** wasmtime async support so host calls
  do not block the plugin executor; working epoch-deadline interruption.
- **Embedder degradation handling (ADR-39):** stale-mark, backoff pause,
  FTS-only fallback with a user-visible `EmbedderDegraded` notice.
- **Audit log outlives session pruning (ADR-40):** append-only `audit_log`
  with a nullable `session_id` (`ON DELETE SET NULL`); `concerto sessions
  prune` deletes sessions, messages, spend, tasks, checkpoints, and transcript
  in one transaction without touching the audit decision trail.
- **LSP tools registered by default:** `GetHover`, `FindReferences`,
  `RenameSymbol`, `GetDiagnostics`, `GetSemanticTokens`, `GetCodeActions`,
  `ExecuteCodeAction`, and `GetInlayHints` are registered unconditionally in
  `runtime_runner.rs` (lines 1226–1233); the LSP server starts lazily on first
  use. `GitTool` is registered at line 1221.
- **Skills and MCP extensions (ADR-43):** new `concerto-skills` and
  `concerto-mcp` crates. Skills are local instruction packs (`skill.toml` /
  `SKILL.md`) discovered by `SkillManager` and injected into every prompt path
  as a budgeted, truncation-marked `## Skills` section (`SkillsContext`) — they
  never execute code. The MCP client spawns stdio servers (protocol pin
  `2025-11-25`, newline-delimited JSON-RPC), registers collision-checked tools
  as `mcp:<server_id>:<tool_name>`, defaults unmatched `mcp:*` tools to
  `RequireApproval`, and publishes `McpServerStateChanged` events on crash.
  Desktop Settings gains config-driven v1 Skills/MCP sections; the CLI adds
  `concerto extensions list` (read-only).
- **Provider/memory-adapter plugin execution:** `PluginBackedProvider` and
  `PluginBackedVectorStore` implement `LlmProvider`/`VectorStore`; guest SDK
  macros `plugin_entry_provider!` / `plugin_entry_adapter!`.
- **Hybrid chat-centric UI (minimal scope):** Diff, Agent Graph, and Tool Log
  open as overlay modals within Chat (Ctrl+D / Ctrl+L); merged via PR #49.
- **Codebase-world-class Phase 0** merged via PR #63: `missing_docs` policy on
  crate roots, proptest for the shell parser (which caught a real bug),
  LSP integration tests, `run_shared_agent` decomposed into phases.
- **Evidence spine (ADR-65, Phases 1–8, branch `feat/evidence-spine`):**
  the whiteboard log is now the append-only evidence chain. `ToolExecuted` and
  `WorkspaceSnapshot` facts are recorded on the execution hot path with agent
  attribution; a derived `resource_facts` table (migrations 029–031) provides
  the fast path for read deduplication and workspace-state queries. The
  hardcoded `design → research → implement` fallback is replaced by
  evidence-driven scheduling (`evidence_scheduler.rs`); a deterministic
  DesignDoc verifier (`design_doc_verifier.rs`) resolves proposed-file intents
  against the snapshot and `resource_facts`, quarantining hallucinated docs.
  Continuation restores state at the whiteboard cursor (`resume.rs`,
  checkpoint schema v4) and never dispatches architect/researcher without a
  recorded, evidence-backed decision. Vectors stay strictly derived
  (aggregate-only consolidation, retention controls, correct with vector
  memory disabled). Security remediation F1–F5 (canonical per-root keys,
  serve gates, fresh digest, content purge) hardened the read-cache serve
  path. Verification: 3421 tests green, clippy/fmt clean, full workspace
  build green.
- **Frontend parity groundwork:** shared `ServicesBuilder`/`RequestBuilder`,
  unified `ContextOverflowStrategy`, structured `Vec<Message>` history, and a
  durable transcript restore path used by both frontends.
- **Release hardening:** bounded durable event-bus backlog with health flag,
  debounced memory re-index watcher, size-bounded log rotation, `sessions
  prune`, centralized provider retry/backoff.

## In progress

- `fix/orchestrator-always-on-base-version` carries ADR-60 D5 always-on,
  per-target write-conflict detection (supervisor stamping + in-process
  write-gate parity for the single-agent loop) on top of `dev`, pending merge.
- `feat/ui-depth-improvements` was merged into `dev` via PR #97 (2026-08-03):
  hybrid UI medium scope — terminal as a toggleable bottom panel with drag
  resize, Memory Explorer as a compact quick-panel section, glass modals and
  overlay/panel animations, chat timestamps and transcript format v2, blinking
  streaming cursor. Tracked by `docs/hybrid-ui-plan.md`; full scope is still
  pending (see Next).
- `feat/codebase-world-class` was merged into `dev` (PR #63); Phase 0 is
  complete there and Phases 1–5 are unstarted (see Next).

## Next

Feature-sized items planned next. The fine-grained, verified task list with
file/line references is `docs/TODO.md`; this section keeps only the items that
shape the roadmap.

- **Hybrid UI full scope** (post-1.0): split Settings into tabbed sub-views,
  Studio split pane, drag-and-drop agent assignment, focus-trap system —
  `docs/hybrid-ui-plan.md`.
- **Codebase-world-class Phases 1–5** (~300 h, from
  `docs/codebase-world-class-plan.md`): hotspot refactoring (desktop markdown
  renderer, largest modules), test-coverage targets, criterion benchmarks,
  architecture consistency (builder pattern, duplicate error types,
  cancellation audit), security/polish (threat model, secret sanitizer,
  hardened WASM sandbox).
- **Coordinator restart/resume:** checkpoint persistence to the session
  database exists (`coordinator.rs::persist_checkpoint`); ADR-65 Phase 7
  (evidence-driven resume at the whiteboard cursor, checkpoint schema v4) is
  implemented on `feat/evidence-spine`. The remaining gap is cross-process
  restart-resume so a Continue after application restart reconciles running
  tasks to pending without rerunning completed graph nodes (ADR-34 decision 2).
- **Real FTS BM25 ranking:** propagate SQLite FTS5 `rank()` into hybrid
  retrieval instead of the neutral `score: 1.0` written by the sync layer
  (`crates/memory/src/sync.rs:59,90`) — quick win from STUB-FINDINGS #6.
- **Un-ignore the eval end-to-end test** (`crates/eval/src/runner.rs:414`); the
  fixture guard already makes it a safe no-op when benchmark data is absent
  (STUB-FINDINGS #8).
- **Provider reach:** flat/content-embedded tool-call parsing for
  OpenAI-compatible proxies (`docs/proxy-tool-call-fix.md`), then named
  wrappers for high-demand OpenAI-compatible providers (DeepSeek, Groq,
  Together, Mistral, xAI, Fireworks, Cerebras, Cohere —
  `docs/missing-providers.md`).
- **Fault-injection tests** for the multi-agent containment boundaries in
  ADR-26 (rate limits, malformed tool calls, missing executables, cancellation
  races, provider disconnects).
- **Audit-log retention policy:** define retention/archival for `audit_log`
  rows; ADR-40 explicitly leaves this to a future policy.

## Later

Deliberately deferred until the live-testing cycle provides evidence; none of
these are promised near-term features.

- **AI-native shell Phases C–F** (`docs/custom-ai-shell-plan.md`, ADR-29):
  `explain`/`debug`/`optimize` commands, deterministic workflow AST with
  checkpoints, tool/plugin ABI and fixtures, measured self-improvement. Phases
  A and B are implemented as a library foundation; the runtime is not yet the
  desktop terminal.
- **Code editor integration:** configurable external editors with
  file/line/column launch templates, and an "Open in editor" handoff from
  diffs, tool logs, chat references, and memory. An embedded editor is only
  considered later if it clearly beats a reliable external-editor protocol.
- **Certified evolutionary optimization:** the exploratory design in
  `docs/research/certified-universal-evolution.md` stays research until
  compiler-enforced confidence/evidence types, evaluator specification
  validation, deterministic isolated evaluation infrastructure, explicit
  resource budgets, and small-domain evidence comparable to STOKE exist.
- **Binary installers and crates.io publishing:** currently only a `.tar.gz`
  release build exists; see the Release section of `docs/TODO.md`.
- **Memory-grounded resume (Phase 6 M3, live-test-gated):** make resume
  *informed* rather than purely mechanical. Three increments, all on existing
  stores (no schema change, no new ADR at this scope): (a) **run-scoped
  priming** — seed `retrieve_memory_context` (coordinator.rs:1184) with the
  run's `plan_id` so a resumed specialist receives *this run's* stored
  decisions, not task-text matches alone; (b) **outcome write-back** — at
  each `SubTaskCompleted`, persist a deliverable summary + artifact list into
  the decision/task stores (`crates/memory/src/{decision_store,task_tree}.rs`)
  so memory can answer *why* a task finished, not just *that* it did;
  (c) **plan↔worktree drift gadget** — compare `expected_artifacts` from the
  plan artifact (`<data>/plans/plan-<id>.json`, ADR-52) against the
  indexer/watcher's file state on resume, emitting a `PlanDrift` signal.
  Exit gate: three tests — `resume_with_memory_still_grounds`,
  `kill_midrun_resume_produces_grounded_plan`,
  `plan_drift_detected_on_tampered_worktree`. Parked here until the live
  stress/interrupt cycle (see `docs/STATUS.md`) produces evidence on
  fidelity needs.

## Explicitly deferred or incomplete

- **Single active project per process (accepted limitation).** Desktop
  (`switch_project_dir`, `crates/desktop/src/app.rs`) and CLI (`switch_project`,
  `crates/cli/src/app.rs`) track one active project; switching rebuilds all
  per-project state (VirtualFs, memory system, session manager, chat state) and
  reloads config. `ProjectRegistry` (`crates/config/src/projects.rs`) holds a
  single `active` project plus a recent list — no concurrent multi-project state
  — and process-global services assume one project context by design (EventBus:
  events carry `project_id` but all subscribers see all events; PluginManager;
  SkillManager search paths; McpManager config). Great for now, but this will
  limit future goals: running multiple projects concurrently in one process
  (e.g., the API server hosting simultaneous runs) would share these
  process-global registries, so a multi-project feature needs per-project
  scoping of process-global services or one isolated process/context per
  project. Everything else about isolation is fine today: sessions are isolated
  per session and bound per project (`project_dir` key in the shared
  `sessions.db`), memory rows are `project_id`-scoped with `CrossProjectLeakage`
  validation (`crates/memory/src/system.rs`), the per-project config layer
  (`.concerto.toml`) reloads on switch, and tool filesystem access is rooted at
  the project dir (`resolve_path`, `crates/tools/src/common.rs`).
- **Sandbox / execution isolation.** `SandboxProfile::Containerized` is
  declared but not implemented; plugins run under the WASM capability sandbox,
  which is not complete OS-level isolation (ADR-21, `docs/STATUS.md`).
- **Evaluator end-to-end runner.** The sole end-to-end eval test is
  `#[ignore]`d; full pipeline coverage is deferred until multi-agent quality
  and recovery are reliable.
- **Replacing SQLite memory storage** without a measured need.
- **Claiming global optimality** for an unbounded real-world software problem.
- Audit-log retention, provider reach, and the AI-shell phases are parked in
  Next/Later above rather than silently dropped.

## Architecture decision index

ADR numbers are never reused; gaps (09, 13, 15, 17, 18) are reserved historical
numbers. Files are uniformly named `docs/adrs/ADR-NN.md`.

| ADR | Topic | Status |
|---|---|---|
| [01](docs/adrs/ADR-01.md) | Git implementation strategy | Accepted |
| [02](docs/adrs/ADR-02.md) | Correlation / event IDs — ULID | Accepted |
| [03](docs/adrs/ADR-03.md) | Configuration — `figment` | Accepted |
| [04](docs/adrs/ADR-04.md) | Credential storage — `keyring` | Accepted |
| [05](docs/adrs/ADR-05.md) | Diff computation | Accepted |
| [06](docs/adrs/ADR-06.md) | File watching | Accepted |
| [07](docs/adrs/ADR-07.md) | Terminal UI — `ratatui` | Accepted |
| [08](docs/adrs/ADR-08.md) | Desktop UI — `iced` | Accepted |
| [10](docs/adrs/ADR-10.md) | Vector store — superseded | Superseded in implementation by SQLite vector/FTS store |
| [11](docs/adrs/ADR-11.md) | Multi-instance file locking | Accepted |
| [12](docs/adrs/ADR-12.md) | Embedding versioning | Accepted |
| [14](docs/adrs/ADR-14.md) | Plugin architecture — WASM | Accepted |
| [16](docs/adrs/ADR-16.md) | Context overflow strategy | Accepted (updated for Phase 4) |
| [19](docs/adrs/ADR-19.md) | Multi-agent orchestration | Accepted; routing superseded by ADR-24 |
| [20](docs/adrs/ADR-20.md) | Desktop UI architecture | Accepted |
| [21](docs/adrs/ADR-21.md) | WASM plugin implementation | Partially superseded by implementation |
| [22](docs/adrs/ADR-22.md) | Hybrid retrieval ranking — RRF | Accepted |
| [23](docs/adrs/ADR-23.md) | Architecture knowledge-graph snapshot | Accepted |
| [24](docs/adrs/ADR-24.md) | Deterministic provider/model routing | Superseded by ADR-31 |
| [25](docs/adrs/ADR-25.md) | Tool JSON schemas from Rust types | Accepted |
| [26](docs/adrs/ADR-26.md) | Multi-agent fault containment/recovery | Accepted |
| [27](docs/adrs/ADR-27.md) | Integrated terminal lifecycle | Accepted |
| [28](docs/adrs/ADR-28.md) | Shell profiles and toolchain | Superseded in part by ADR-30 (shell selection) |
| [29](docs/adrs/ADR-29.md) | AI-native shell runtime/policy execution | Accepted |
| [30](docs/adrs/ADR-30.md) | Unified agent shell selection | Accepted |
| [31](docs/adrs/ADR-31.md) | Model-first selection with internal routing | Accepted |
| [32](docs/adrs/ADR-32.md) | Explicit provider failures, safe policy defaults | Accepted |
| [33](docs/adrs/ADR-33.md) | Shared frontend project/runtime context | Accepted |
| [34](docs/adrs/ADR-34.md) | Durable orchestration runtime | Accepted |
| [35](docs/adrs/ADR-35.md) | Tag-driven agent orchestration | Accepted (Phases 1–5 complete) |
| [36](docs/adrs/ADR-36.md) | Durable typed session transcript | Accepted — complete |
| [37](docs/adrs/ADR-37.md) | Plugin capability grant lifecycle | Accepted |
| [38](docs/adrs/ADR-38.md) | Async WASM host functions | Accepted — implemented |
| [39](docs/adrs/ADR-39.md) | Embedder degradation handling | Accepted |
| [40](docs/adrs/ADR-40.md) | Audit log append-only / session pruning | Accepted |
| [41](docs/adrs/ADR-41.md) | Spend surfaces in status bar; no Dashboard page | Accepted |
| [42](docs/adrs/ADR-42.md) | Coordinator resilience: failure-class fallback ladder | Accepted |
| [43](docs/adrs/ADR-43.md) | Skills, MCP client, and extension manager | Accepted |
| [65](docs/adrs/ADR-65-evidence-spine.md) | Evidence spine — facts, claims, decisions on one append-only chain | Accepted |

When current behavior supersedes an ADR decision, update that ADR's status or
add a superseding ADR; do not silently rewrite its historical context.
