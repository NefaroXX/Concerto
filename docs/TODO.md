# Concerto pending work (TODO)

**Last reconciled with the source tree: 2026-08-07**

This is the fine-grained, verified list of pending work. Status is per item.
"Not started" means no code exists. "Partial" means a documented gap remains
after existing work. "On branch X" means the work exists only on an unmerged
branch. Feature-sized items also appear in `ROADMAP.md`; this file is the
authoritative task list. Do not include anything here without a verifiable
reference; if a reference is stale, the entry says so.

## Persistence & Data

- **Audit-log retention policy.** Not started — `docs/adrs/ADR-40.md:47`:
  "Audit retention remains a future policy question, not a session one."
  Define retention/archival for `audit_log` rows (the log is currently
  grow-only by design).
- **Coordinator restart/resume.** Partial — orchestration checkpoints are
  persisted to the session database and restore is fallible
  (`crates/orchestrator/src/coordinator.rs:431-452` `persist_checkpoint` →
  `save_orchestration_checkpoint`, `decompose_or_restore` at :530-561,
  migrations 014/018/019), satisfying ADR-34 decision 2 at the coordinator
  layer. What remains is end-to-end verification that a Continue after
  application restart reconciles running tasks to pending without rerunning
  completed nodes.
- **Context-management consolidation (audit M-01).** Not started (deferred) —
  `crates/orchestrator/src/context_compaction.rs` (durable checkpoints),
  `memory/src/short_term.rs:63` (SummarizeOldest), `memory/src/budget.rs`, and
  legacy `memory/src/system.rs` coexist; both runtime runners invoke context
  maintenance. AUDIT_FINDINGS_CURRENT.md M-01 requires an ADR and a dedicated
  refactor before consolidation.
- **Canonical compaction pipeline (audit C-03 follow-up).** Not started —
  `SummarizeOldest` is no longer wired into production; overflow degrades to
  deterministic compaction (AUDIT_FINDINGS_CURRENT.md C-03, `runtime_runner.rs`).
  Remaining: a typed cancellable `Result` (today `usize`), removal of originals
  from the active projection only after persistence succeeds, and one
  authoritative pipeline.
- **Checkpoint v2 follow-ups (audit C-05).** Partial — v2 checkpoint
  timestamps are unrecoverable (fall back to now), and `model_assignments`
  lags one batch on resume (informational; models re-selected on resume).
  AUDIT_FINDINGS_CURRENT.md C-05 "Remains".
- **`request_ack` unscoped approval event (audit H-04 follow-up).** Not
  started — `ApprovalSink::request_ack` passes no session id, so the ack
  cannot be session-scoped; requires a trait/API change.
  AUDIT_FINDINGS_CURRENT.md H-04 "Remains".

## Memory & Retrieval

Competitive research 2026-08-06 from TencentCloud/TencentDB-Agent-Memory
(MIT; branch `feat/server_team`, tree SHA `b44c6db5`): layered memory
(L0 raw → L1 atoms → L2 scenarios → L3 persona), hybrid recall, context
offload, and a PersonaMem benchmark. Concerto already covers hybrid
RRF k=60 retrieval (`crates/memory/src/rag.rs`), SQLite+FTS5+ANN
persistence, and session recording; the entries below are the genuine
gaps. Team-hub/asset-ACL machinery is out of scope for a single-user
agent. Scheduled after the live-test phase (2026-08-06 decision).

- **Symbolic short-term memory / context offload.** Not started — requires
  an ADR before code (ADR-first rule; next number ADR-46). TencentDB
  compresses tool results into a Mermaid node-graph with `node_id` tracing,
  offloads full results to `refs/*.md` with `result_ref` drill-down, scores
  entries 0–10 by replaceability, and runs a compression cascade (mild →
  aggressive → emergency) under token pressure (−61% tokens on WideSearch).
  Source: `src/offload/*`, `src/offload-client/context-engine.ts` (branch
  `feat/server_team`, SHA `b44c6db5`). Concerto: new module in
  `crates/memory` (or small `crates/context`), hooked into tool
  execution/observability; drill-down reuses already-persisted tool logs.
  Related: audit M-01 context-management consolidation (Persistence & Data
  section above).
- **L1 typed extraction + LLM-judged dedup.** Not started — one LLM call
  segments conversation into scenes and extracts typed memories
  (persona/episodic/instruction; code mode adds work_fact/work_task/
  work_method/work_artifact with owner/deadline/status metadata); a
  separate batch LLM pass then judges new-vs-existing memories as
  `store|update|merge|skip` after top-K candidate recall (vector top-K +
  BM25 top-5) — no brittle numeric similarity threshold. Source:
  `src/core/prompts/l1-extraction.ts`, `src/core/prompts/l1-dedup.ts`,
  `src/core/record/l1-dedup.ts`. Concerto: `crates/memory` summarizer has
  chunking + `SessionSummary`/`Fact` classification but no content dedup
  today.
- **Recall budget caps + timeout guard.** Not started — per-memory and
  total recall char caps (truncate by code point, never split surrogate
  pairs), 5000 ms timeout race that skips recall with a warning instead of
  blocking the turn, and a structured failure envelope (error code in the
  result, never throw). Source: `src/config.ts` (maxResults=5,
  maxCharsPerMemory=0, maxTotalRecallChars=0, timeoutMs=5000),
  `src/core/hooks/auto-recall.ts`. Concerto: `chunk_selector.rs`/`rag.rs`
  have no char-budget allocator today.
- **Pipeline scheduling heuristics.** Not started — warmup extraction
  cadence (1→2→4→…→N before settling), resettable idle debounce (600 s),
  shutdown flush, downward-only min/max interval timers (`T_desired =
  max(now + delay, last + minInterval)`, advance only earlier). Source:
  `src/utils/pipeline-manager.ts`, `src/core/state/timer-member.ts`.
  Concerto: summarization cadence in `crates/memory`/orchestrator is
  cruder; maps to `watcher.rs`.
- **PersonaMem-style long-horizon memory eval.** Not started — benchmark
  of user-fact recall across long sessions (TencentDB claims 48% → 76%
  with their persona layer). Concerto: add to `crates/eval-runner` as a CI
  guard for future memory work.

## Plugins/WASM

- **`SandboxProfile::Containerized` (OS-level isolation).** Not started —
  `docs/adrs/ADR-21.md:29-31` lists full OS-level container isolation as
  deferred; `docs/STATUS.md:127-128` confirms the variant is declared but not
  implemented. Plugins currently run only under the WASM capability sandbox
  (with grant TTL / hash pinning per ADR-37).
- **Plugin hot-reload, remote plugins, registry.** Not started —
  `docs/adrs/ADR-21.md:30-31` deferred list; requires the community/registry
  story before a distribution path exists.

## Tools & Shell

- **AI-native shell Phases C–F.** Not started —
  `docs/custom-ai-shell-plan.md` (status line: "Phases A and B implemented as a
  library foundation; Phases C–F planned"); ADR-29 is the runtime/policy
  decision. Phase C: `explain`/`debug`/`optimize` commands. Phase D:
  serde-versioned workflow AST, checkpoints, bounded retry/approval/parallel
  nodes. Phase E: tool/plugin ABI, fixtures (starting `oxide-serve`). Phase F:
  measured self-improvement with validation before promotion.
  Research (2026-08-01, `concerto-shell`-targeted):
  [research brief](research/ai-native-shell-research-brief.md) — failure modes
  (tool hallucination, context bloat, non-determinism) + prior art (Nushell,
  etc.); [implementation plan](research/ai-native-shell-implementation-plan.md)
  — fresh phase plan starting with the `ToolManifest` schema system (reconcile
  its phase numbering with `custom-ai-shell-plan.md` before starting);
  [expanded reference](research/ai-native-shell-expanded-research.md) —
  concrete `ToolManifest` schemas, strict-mode validation, built-in tool
  reference implementations, provider-native schema conversion, hallucination
  benchmark spec.
- **Shell profile slices 1–3.** Not started — `docs/adrs/ADR-28.md` slice 0
  (Foundation) is done; slice 1 (test profile action, availability checks),
  slice 2 (Managed Bash PoC: controlled PATH, offline, versioned runtime),
  slice 3 (cross-platform packaging) remain. Note ADR-28 is superseded in part
  by ADR-30 for shell selection only.

## Desktop/TUI

- **Hybrid UI medium scope.** Merged — `dev` via PR #97 (2026-08-03):
  terminal as toggleable bottom panel with drag resize (4a12839), Memory
  Explorer as compact quick-panel section (1c916b4), glass modals and
  overlay/panel animations (ade0c7b), chat timestamps + transcript format v2
  (3d691a2), blinking cursor (f8c7b42). Minimal scope merged earlier via
  PR #49. Reference `docs/hybrid-ui-plan.md`.
- **Hybrid UI full scope.** Not started — `docs/hybrid-ui-plan.md` (post-1.0):
  split Settings into tabbed sub-views, Studio split pane, drag-and-drop agent
  assignment, focus-trap system.
- **Codebase-world-class Phases 1–5.** Not started —
  `docs/codebase-world-class-plan.md`; Phase 0 merged to `dev` via PR #63, the
  remaining phases are ~300 h. Phase 1 hotspot refactoring (desktop markdown
  renderer `widgets/markdown.rs::render` cognitive 312; oversized modules
  `studio_editor.rs`, `app.rs`, `agent_loop.rs`). Phase 2 test coverage (target
  ~1,500 tests, proptest in 5 crates, 2 fuzz targets). Phase 3 criterion
  benchmarks. Phase 4 architecture consistency (builder pattern, duplicate
  error types, cancellation audit). Phase 5 security/polish (threat model,
  secret sanitizer, hardened WASM sandbox).
- **Editor integration ("open in editor").** Not started — no code exists; the
  prior ROADMAP "Code editor integration" track planned configurable external
  editors with file/line/column launch templates and an "Open in editor"
  handoff from diffs, tool logs, chat references, and memory.
  `docs/desktop-cli-parity.md:103-104` lists the studio `Editor` as GUI-only,
  which is a different feature (in-app code editor).
- **Stale parity document.** Resolved — `docs/desktop-cli-parity.md` refreshed
  on 2026-08-03: R2/R3/R4 and P1–P7 marked implemented with source citations
  (R2 `orchestrator/src/services/summarizer.rs:43`, R3 `orchestrator/tests/
  parity.rs`, R4 former self-hosted CI grep-check, P1 `event_line` at
  `cli/src/app.rs:1123`, P2 `Screen::Sessions`, P3 `SettingsField::Provider`,
  P4 `Screen::AgentAssignments`, P5 `Screen::ToolLog`, P6 `draw_status_bar`
  chunk count at `cli/src/ui.rs:250`, P7 `switch_project` at
  `cli/src/app.rs:229`); document marked complete as of 2026-08-03.

## Providers

- **Flat tool-call parsing for OpenAI-compatible proxies.** Not started —
  `crates/providers/src/openai.rs:287` parses only nested
  `delta.tool_calls[].function.{name,arguments}`; the flat (`name`/`arguments`
  on the tool-call object), content-embedded, and non-string-arguments
  fallbacks in `docs/proxy-tool-call-fix.md` are unimplemented. OpenRouter /
  NIM / OpenAI-compatible models can silently drop tool calls today.
- **Additional OpenAI-compatible providers.** Not started —
  `docs/missing-providers.md` Tier 1 wishlist (DeepSeek, Groq, Together AI,
  Mistral, xAI, Fireworks, Cerebras, Cohere); each is a ~50-line wrapper plus
  `provider_defs.rs`, `factory.rs`, `lib.rs` model-listing, and `budget.rs`
  entries. Recommended order: DeepSeek, Groq, Together, then Tier 2.
- **Model metadata / price freshness.** Not started —
  `docs/STATUS.md:130-133`: model metadata and prices can become stale; explicit
  provider/model assignments remain authoritative today.

## Eval/Quality

- **Un-ignore eval end-to-end test.** Not started — `crates/eval/src/runner.rs:414`
  `#[ignore = "slow integration test ..."]` on `fallback_mode_runs_tests`;
  STUB-FINDINGS.md #8: the test already returns early when
  `benchmark_tasks/standard` is absent, so removing `#[ignore]` is safe on CI
  and exercises the full runner locally.
- **Fault-injection tests for multi-agent containment.** Not started —
  `docs/adrs/ADR-26.md` defines the provider/tool/subtask recovery boundaries;
  the prior ROADMAP "Automation continuity" item asked for injection tests
  covering rate limits, malformed tool calls, missing executables, cancellation
  races, and provider disconnects. (STATUS.md lists only the manual release
  confirmation "Confirm retry resume and cancellation during both generation
  and backoff", not these automated tests.)
- **Forward-compat panic footgun in the agent loop.** Not started —
  `crates/orchestrator/src/agent_loop.rs:393` matches `AgentRunExit` with
  `_ => unreachable!()`; all four variants are covered today, but a 5th variant
  in `concerto-core` would silently panic instead of failing to compile
  (`docs/STATUS.md:153-160`). Fix: return an explicit `AgentLoopError` in the
  wildcard arm.
- **Acceptance-cycle manual verification (audit C-06 follow-up).** Not
  started — acceptance is validator-owned with artifact/verification evidence,
  but the audit records a manual end-to-end build-then-accept/reject cycle on
  disk as still recommended (AUDIT_FINDINGS_CURRENT.md C-06 "Remains").
- **Duplicate public error names (audit M-08).** Not started (deferred) —
  `SessionError` (core + sessions) and `EvalError` (core + eval) are
  deliberately deferred as a breaking API change; note `MemoryError` exists
  only in core (the original audit over-stated this one).
  AUDIT_FINDINGS_CURRENT.md M-08.
- **Oversized module decomposition (audit M-05).** Partial — `settings.rs` and
  `studio_editor.rs` split into `views/settings/` and `views/studio/`;
  `agent_loop.rs` (3539), `coordinator.rs` (3295), and `runtime_runner.rs`
  (2486) remain large and need dedicated refactors with coverage.
  AUDIT_FINDINGS_CURRENT.md M-05.
- **Decorative cancellation (audit M-02).** Partial — hot paths (tools, shell,
  plugins, memory sync/watcher, sessions) honor tokens; remaining ignores in
  `core/src/executor.rs:216..362`, core traits, shell builtins,
  `memory/src/system.rs:174-457`, `plugins/src/host_fns.rs:511` (fresh token),
  and `ContextOverflowStrategy`. Closing requires trait-contract changes.
  AUDIT_FINDINGS_CURRENT.md M-02.

## Release

- **Binary installers (deb/rpm/tar).** Not started — `docs/STATUS.md:11-14`
  (no installer packages promised); today only a `.tar.gz` release build exists
  (`scripts/release.sh`; CHANGELOG 0.1.0-alpha "Release workflow building
  .tar.gz for ubuntu + macos").
- **crates.io publish.** Not started — `docs/STATUS.md:12-13` (not published;
  source builds and workspace checks are the supported path).
- **Release gate matrix.** In progress — `docs/STATUS.md:164-170` immediate
  release priorities (manual matrix on Windows + Linux desktop via
  `TESTING.md`, multi-provider-family Build runs, retry/resume/cancellation
  confirmation, policy/shell/memory checks, reproducible issue records).
- **LLM prompt-cache hit-rate optimization (cost).** Not started —
  provider prompt caching bills cached prefix tokens at a fraction of the
  write cost (Anthropic: ~0.1× read vs 1.25× write; hit rate = share of
  input tokens served from cache). opencode's cache-stability work
  (anomalyco/opencode#14743) raised cross-repo first-prompt hits
  0% → 97.6% by making the request prefix byte-stable: split the system
  prompt into a stable block (provider prompt + global instructions +
  tools) and a dynamic block (env/project files), drop per-repo fields
  from tool schemas, sort tool/skill definitions, freeze the date.
  Concerto has no prefix-stability or cache-marker support today (prompt
  construction: `crates/orchestrator` agent_loop/coordinator; adapters:
  `crates/providers`). Noted 2026-08-07 for post-live-test cost work;
  ADR first when started.

## Research

- **Certified universal evolution.** Not started (research) —
  `docs/research/certified-universal-evolution.md`; the prior ROADMAP section
  "Certified evolutionary optimization" lists the prerequisites:
  compiler-enforced confidence/evidence types, evaluator specification
  validation, deterministic isolated immutable evaluation infrastructure,
  explicit resource budgets and stop/resume semantics, and small-domain
  evidence comparable to STOKE. STATUS.md does not cover this track; it is
  roadmap-scoped research, not a promised feature.
