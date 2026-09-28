# Concerto pending work (TODO)

**Last reconciled with the source tree: 2026-09-28**

**This file is no longer the outstanding-work register.**
[`DEFERRED.md`](DEFERRED.md) is. Every deferral lands there with a source, a
re-entry condition, and a size, and each release an owner promotes met rows to
Closed with a commit SHA or ADR cite. This file holds only what the register
does not: items nobody has formally deferred.

## Why the two lists were merged

Until 2026-09-28 this file was the "authoritative task list" and carried its own
status vocabulary. That arrangement rotted, and the rot is the argument for the
change: while it was nominally current, it asserted **six items as "Not
started" that had already shipped** (audit-log retention, `request_ack`
session scoping, recall budget caps, proxy tool-call parsing, fault-injection
tests, prompt-prefix stability), cited a file that has never existed in the
tree (`AUDIT_FINDINGS_CURRENT.md`), cited an ADR at line numbers that do not
exist, and carried a `coordinator.rs` line count off by an order of magnitude
(3 295 vs ~36 000 today). One maintained register with a stated maintenance
rule is auditable; two "authoritative" lists are not.

## Not in the register

Items with no deferral decision, no re-entry condition, and no owner. Promote
one into `DEFERRED.md` when it is genuinely deferred rather than merely
unstarted.

- **Pipeline scheduling heuristics.** Not started — warmup extraction cadence
  (1→2→4→…→N before settling), resettable idle debounce (600 s), shutdown
  flush, downward-only min/max interval timers (`T_desired =
  max(now + delay, last + minInterval)`, advance only earlier). Source: the
  TencentDB `src/utils/pipeline-manager.ts` and
  `src/core/state/timer-member.ts` survey recorded under Memory & Retrieval in
  the 2026-08-07 revision of this file. Concerto: `crates/memory/src/watcher.rs`
  has a `notify_debouncer_mini` debouncer but no warmup or downward-only
  cadence; summarization cadence in the orchestrator is cruder still.
- **Shell profile slice 3 (cross-platform managed-runtime packaging).**
  ADR-28 slices 1–2 are landed (recorded in `DEFERRED.md` row 24). Slice 3 —
  cross-platform packaging of the Concerto-managed Bash runtime — is described
  in the archived ADR-28 §8 as a later slice and has no implementation and no
  register row. Not a deferral; nobody has decided it out.
- **Consent gate: persistent "always allow" (issue #22).** Not started
  (deferred in ADR-44 terms, but never registered) — the desktop out-of-root
  consent gate's Allow is process-lifetime; a persistent "always allow" was
  deferred alongside the api-server pending-approval flow. Same gating
  condition as issue #23: implement when an in-tree interactive web UI exists.
  Source: `docs/adrs/ADR-44.md:37`.
- **Out-of-root session pending-approval consent flow (issue #23).** Not
  started — session roots outside `project_roots` are refused HTTP 403 by
  design; the server-side pending-approval flow (session created pending, event
  emitted over the bus/SSE, interactive client approves/rejects) has no in-tree
  interactive HTTP client to serve. Source: `docs/adrs/ADR-44.md:37`.
- **Config for a shell CPU budget (gap #6 residual).** The portable CPU-budget
  layer shipped (`13cba1c`) but `cpu_budget_secs` is `None` in every production
  constructor (`crates/tools/src/shell.rs:220-231`) and there is no TOML key —
  the env var is the only operator knob. The Windows and cgroup-v2 halves are
  `DEFERRED.md` row 45; the missing config key is not, so it is listed here
  until someone registers it.

## Audit cleanups (`DEFERRED.md` row 34)

These six were tracked here and are now one register row. The row cites this
file by line range; **cite this section name instead** — the line ranges are
void as of 2026-09-28. They are reproduced here so the descriptions are not
lost. All six read Not started or Partial.

- **C-03 — canonical compaction pipeline.** Not started. `SummarizeOldest` is no
  longer wired into production; overflow degrades to deterministic compaction
  (`runtime_runner.rs`). Remaining: a typed cancellable `Result` (today
  `usize`), removal of originals from the active projection only after
  persistence succeeds, and one authoritative pipeline.
- **C-05 — checkpoint v2 follow-ups.** Partial. v2 checkpoint timestamps are
  unrecoverable (fall back to now), and `model_assignments` lags one batch on
  resume (informational; models re-selected on resume). The lag is by
  construction, not a defect — see the note in
  `crates/orchestrator/src/checkpoint.rs` (`restore_yields_semantically_identical_state`).
- **C-06 — acceptance-cycle manual verification.** Not started. Acceptance is
  validator-owned with artifact/verification evidence, but a manual
  build-then-accept/reject cycle on disk is still only recommended.
- **M-08 — duplicate public error names.** Not started (deferred as a breaking
  API change). `SessionError` (core + sessions) and `EvalError` (core + eval)
  are deliberate; note `MemoryError` exists only in core (the original audit
  over-stated this one).
- **M-05 — oversized module decomposition.** Partial — and worse than recorded.
  `settings.rs` and `studio_editor.rs` split into `views/settings/` and
  `views/studio/`; what remains is large: `coordinator.rs` ~36 000,
  `runtime_runner.rs` ~9 400, `agent_loop.rs` ~8 200 lines (measured
  2026-09-28). Each needs a dedicated refactor with coverage.
- **M-02 — decorative cancellation.** Partial. Hot paths (tools, shell, plugins,
  memory sync/watcher, sessions) honour tokens; remaining ignores include
  `core/src/executor.rs:216..362`, core traits, shell builtins,
  `memory/src/system.rs:174-457`, `plugins/src/host_fns.rs:511` (fresh token),
  and `ContextOverflowStrategy`. Closing requires trait-contract changes.

## Resolved — where each record now lives

Nothing below is outstanding. It is listed so a search for the old entry finds
its authority instead of re-opening the item.

| Former entry | Authority for the result |
|---|---|
| Audit-log retention policy | `DEFERRED.md` closed row 15 — `7351128`, ADR-73 Accepted (SQLCipher at rest + age-based archive-then-delete; opt-in) |
| Coordinator restart/resume | `DEFERRED.md` row 18 — checkpoints and `restore_graph` exist; no cross-process continue consumer yet |
| M-01 context-management consolidation | `DEFERRED.md` row 11 — gated behind a superseding ADR to ADR-67 M-01 |
| `request_ack` unscoped approval (H-04) | ADR-68 Accepted — `request_ack` now takes `session_id: Ulid` (`crates/core/src/traits/approval.rs:50`) |
| FTS BM25 ranking (STUB-FINDINGS #6) | `DEFERRED.md` closed row 12; `ROADMAP.md` "Real FTS BM25 ranking"; `archive/STUB-FINDINGS.md` |
| Symbolic short-term memory / context offload | `DEFERRED.md` rows 15 and 31 — topic hierarchy, backpressure, schedule-driven pushes; a scene-memory consumer beyond a label |
| L1 typed extraction + LLM-judged dedup | `DEFERRED.md` row 31 — L1 typed extraction is wired (`entities.rs:851`, `system.rs:162`, `1702d4c`) |
| Recall budget caps + timeout guard | `DEFERRED.md` closed row 20 — `92b0be4` (`RecallCostBounds`, `rag.rs:133,150-151`) |
| PersonaMem-style long-horizon memory eval | Partly shipped: the `eval-runner` persona replay tests exist (`crates/eval-runner/tests/persona_mem_recall.rs`, listed in `../TESTING.md`); the remaining coverage breadth is `DEFERRED.md` row 17 |
| `SandboxProfile::Containerized` | `DEFERRED.md` row 36 — shipped on Linux/macOS (`d00582b`, `3ae6ea5`, `fdf4800`, ADR-72 Accepted); Windows path open |
| Plugin hot-reload, remote plugins, registry | `DEFERRED.md` rows 29 and 30. (The old cite `docs/adrs/ADR-21.md:29-31` was wrong on both counts — that ADR is archived at `docs/adrs/archive/ADR-21.md`, is superseded by ADR-14, and has no deferral list at those lines.) |
| AI-native shell Phases C–F | `DEFERRED.md` row 24 — prerequisite unmet: the two shell plans still disagree on phase numbering |
| Hybrid UI medium / full scope | Medium merged (PR #97); full scope is `DEFERRED.md` row 37 |
| Codebase-world-class Phases 1–5 | `research/codebase-world-class-plan.md` is an aspirational estimate; Phase 3 shipped (`10357cd`); the real residue is `DEFERRED.md` row 34 |
| Editor integration ("open in editor") | `DEFERRED.md` closed row 26 — **cut**, not deferred. The in-app editor and diff viewer shipped; external-editor launch is a scope cut |
| Stale parity document | Resolved in place: `docs/desktop-cli-parity.md` is marked complete as of 2026-08-03 |
| Flat tool-call parsing for OpenAI-compatible proxies | Fixes 1–3 shipped (`docs/proxy-tool-call-fix.md`, 2026-09-20) plus long-tail hardening `3bc11db` (`DEFERRED.md` closed row 21) |
| Additional OpenAI-compatible providers | Tier 1 + Tier 2 shipped — 22 provider ids (`DEFERRED.md` closed row 9). Tier 3 is row 6; Doubao/StepFun/Replicate is row 7 |
| Model metadata / price freshness | `DEFERRED.md` row 27 — the SpendLog UI exists but nothing feeds it fresh prices. Explicit non-goal: no cost-based routing |
| Un-ignore eval end-to-end test | `DEFERRED.md` closed row 10; `archive/STUB-FINDINGS.md` #8 |
| Fault-injection tests for multi-agent containment | Landed — `crates/orchestrator/src/fault_injection.rs:76-92` (C1–C4, `f4bdc4f`) plus the env-gated live leg (`5daf2e7`). Breadth is `DEFERRED.md` row 17 |
| Forward-compat panic footgun in the agent loop | Fixed — `agent_loop.rs:707-711` returns a typed `AgentLoopError` |
| LLM prompt-cache hit-rate optimization | Prefix-stability mechanism landed and is **opt-in** (`cache_stable_prefix`, `context_engine.rs:19,50-56`, default `false`; `DEFERRED.md` closed row 18). Measuring hit rate remains unstarted and unregistered |
| Binary installers (deb/rpm/tar) | `DEFERRED.md` closed row 30 — **cut**, not deferred. The real gap is everything downstream of a raw binary |
| crates.io publish | `DEFERRED.md` closed row 31 — **cut**, not deferred. No blocker exists; the open question is which subset publishes |
| Release gate matrix | `DEFERRED.md` closed row 29 — reclassified as a per-release human checklist, not deferred work. See `docs/STATUS.md` "Immediate release priorities" and `../TESTING.md` |
| Certified universal evolution | `DEFERRED.md` closed row 28 — **cut** as a research programme, not backlog; its source document is deliberately git-ignored |

## Maintenance

Add an item here only when it has no deferral decision. Once it *is* deferred —
ADR, ROADMAP, or owner decision — move it to `DEFERRED.md` in the same change
and leave at most a one-line pointer here. Every claim needs a checkable
reference: a file, a line, a test name, or a commit SHA. A reference you cannot
open is not a reference.
