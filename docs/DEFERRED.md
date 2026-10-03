# Deferred Work Register

**Purpose.** The single register of work Concerto has explicitly deferred — not
cancelled, not forgotten — plus a condensed record of what has since been closed
or cut. A deferral is only trustworthy if its residual is stated precisely and
re-checked against the tree, so every row here carries a source, a re-entry
condition, and a size.

**Maintenance rule.** Every deferral carries a re-entry condition. Each release,
an owner reconciles this register against the ADRs, `docs/TODO.md`,
`ROADMAP.md` and the code: promote any row whose re-entry condition is met to
`Closed`, cut rows whose subject no longer exists (recording the reason so the
cut stays reversible), and drop nothing silently. Re-verify one closed claim per
release; a claim that no longer holds moves back to Outstanding with a note. Do
not add append-history blocks, per-pass verification notes, or `[verify]` flags
to this file — those belong in the commit that made the change, and a claim that
cannot be traced to a file, line, or commit is not written down at all.

**Last reconciled:** 2026-09-28, against checkout `fix/coordinator-error-invariant`
(HEAD `caadb2d`), cross-checked against `dev` (`edba420`).

## Outstanding

| # | Item | What remains | Source | Re-entry condition | Size |
|---|------|--------------|--------|--------------------|------|
| 6 | Tier-3 provider SDKs (L) | No Copilot / Bedrock / Azure / Vertex / watsonx wrapper exists in `crates/providers`. These are full agent SDKs with their own runtime, tool invocation, session lifecycle and streaming, not API endpoints. | `docs/missing-providers.md:59` (Tier 3 "still open") | Per-provider wrapper plus a pairwise parity test; Tier 1/2 are done (`missing-providers.md:53-57`) | L |
| 7 | Doubao / StepFun / Replicate providers (L) | Zero source matches in `crates/`. No register entry, no plan, no scaffold. | No in-repo source (owner-deferred) | Planning reopens provider reach | L |
| 8 | Vercel Gateway decision (S) | Research prose only. No ADR, no row, no code path. | `docs/research/multi-provider-resilience.md:32,51` | Someone wants a gateway layer and writes the decision record | S |
| 9 | Setup wizard: per-kind auth + desktop onboarding (M) | Two separable pieces. (a) One generic `prompt_api_key` (`crates/config/src/setup.rs:314`) serves all seven `ProviderKind`s; the only per-kind delta anywhere is `default_model` (`:51-61`) — no OAuth/device-code/token-exchange variant. (b) Zero desktop onboarding: no `SetupWizard` reference anywhere in `crates/desktop`, so first run needs a terminal. | `crates/config/src/setup.rs:40-61,314`; `docs/TODO.md` | Either piece can be taken alone; the desktop surface is the user-visible one | M |
| 11 | M-01: estimator dedup + `rag_pct` configurability (S) | Both items are ADR-67 follow-ups. `rag_pct` is absent from `crates/config/src/schema.rs`; the dedup cache is unbuilt. The superseding-ADR gate is unmet: selecting `SummarizeOldest` in production needs a superseding ADR to ADR-67 M-01, whose gate is the in-run slot in `runtime_runner.rs`. | `docs/adrs/ADR-67-m01-context-pool-consolidation.md:92-98`; `crates/memory/src/short_term.rs:9-16` | A superseding ADR to ADR-67 M-01 | S |
| 13 | Codebase cascade S1/S2/S3 — ADR-69 slices (L) | All three slices are code-complete and merged (S1 link store + write path `60841dd`, S2 scoring/decay/caps `224f0f9`, S3 observability `7584d2d`; all reachable from `dev` and `origin/main`). What remains is **production proof only**: link verdicts actually firing, and a cascade reorder observed in a live run. | `docs/adrs/ADR-69-symbolic-cascade.md:34,63,87` | Live-run evidence of link-driven re-ranking | L |
| 15 | ADR-60 deferred item 4 — multi-level disclosure + scheduler/subscription generalization (L) | The embedder swap is done (`crates/memory/src/embedder.rs:45` — `fastembed` BAAI/bge-small-en-v1.5 is production wiring) and disclosure is confirmed single-level (`DISCLOSURE_MAX_CHUNKS = 10`, `crates/orchestrator/src/consolidation.rs:80`, consumed at `supervisor.rs:2142`). Remains: topic hierarchy, filter-by-relevance/recency, cross-subscriber backpressure, schedule-driven pushes, and the scheduler/subscription model at high agent counts. Note agents are additive — no `MAX_AGENTS`-style cap exists anywhere, so profiling pressure cannot unblock a cap that does not exist. | `docs/adrs/ADR-60-concurrent-agent-runtime.md:150-151` (deferred item 4), `:326-329` | A real multi-subscriber disclosure consumer, or agent counts that make the current model measurably inadequate | L |
| 16 | Single-project limit — **accepted limitation, not a task** (L) | Desktop and CLI track one active project; `ProjectRegistry` holds a single `active` plus a recent list; process-global services (EventBus subscribers, PluginManager, SkillManager search paths, McpManager config) assume one project context. Everything else about isolation is sound (sessions keyed by `project_dir`, memory rows `project_id`-scoped, per-project `.concerto.toml`, tool paths rooted at the project dir). | `ROADMAP.md:219` ("accepted limitation"); `crates/orchestrator/src/runtime_runner.rs:575-580` (`ActiveMemoryServices`) | A concrete need for concurrent multi-project runs (e.g. the API server hosting simultaneous runs) | L |
| 17 | Eval breadth + live-flake quarantine (M) | Landed: four crash-window fault-injection scenarios (`crates/orchestrator/src/fault_injection.rs:76-92`, C1–C4, `f4bdc4f`) plus an env-gated live-runtime leg that never runs in CI (`crates/eval-runner/src/main.rs:430-490`, `5daf2e7`). Remains: coverage breadth beyond one suite/model (six benchmark task suites exist under `crates/eval/benchmark_tasks/`, only `standard` is the live-leg default), and a quarantine mechanism so a live-leg flake is triaged rather than blocking a release. | `ROADMAP.md:241-245`; `docs/TODO.md` (Resolved table, eval entries) | A second suite/model wired into the live leg, plus a flake policy | M |
| 18 | Cross-process continue (L) | `restore_graph` (`crates/orchestrator/src/checkpoint.rs:736`) and `load_checkpoint` (`crates/sessions/src/lib.rs:344`) exist, but `concerto sessions resume` (`crates/cli/src/lib.rs:817-841`) only loads the session, re-selects its project, and prints "will resume" — no run is ever continued. No cross-process-continue consumer exists in the CLI or the desktop. | `docs/TODO.md` (Resolved table, coordinator restart/resume — was "Partial; e2e remains"); `docs/adrs/ADR-34.md` decision 2 | A CLI/desktop path that reconstructs a run from its checkpoint, plus the e2e test | L |
| 22 | ADR-47 message `parts` (L) | Gated doctrine, not scheduled work. No `enum Part` and no `parts` field exists; the flat `content: String` is retained deliberately. Re-entry requires **all three**: (a) a named consumer need; (b) a parts-joined-text equivalence test proving the join reproduces today's bytes; (c) a checkpoint/`state_json` migration plan, shipped additively with `serde(default)` and a legacy-column fallback in one dedicated window. | `docs/adrs/ADR-47-message-parts.md:78-93` (trigger to reopen) | A consumer that must express richer content than a flat string can hold | L |
| 23 | Per-token streaming through WASM (M) | Single-shot by construction, not by omission. `HOST_ABI_VERSION = 1` (`crates/plugins/src/guest_abi.rs:7`); the three exports return one packed `(ptr, len)` and the host makes one awaited call mapped to exactly one `CompletionChunk` (`crates/plugins/src/provider_host.rs:146-163`). The liveness case the deferral existed for is covered by the landed heartbeat. Cost to reverse: a new ABI export (or host-fn chunk sink) **plus a version bump** — ADR-53 deliberately did neither. | `docs/adrs/ADR-53-dialect-plugins-and-plugin-heartbeat.md:137-139,171-172` | A plugin kind that generates tokens incrementally | M |
| 24 | AI-native shell Phases C–F (M) | Phases A and B plus ADR-28 profile slices 1–2 are landed. **Prerequisite unmet:** `docs/TODO.md` (Resolved table, AI-native shell entry) records that the repo plan numbers phases A–F (`docs/custom-ai-shell-plan.md:125-228`) and the research plan numbers them 0–5 (`docs/research/ai-native-shell-implementation-plan.md:11,458`), and they still disagree. Reconcile first, then: C verbs (`explain`/`debug`/`optimize`), D (serde-versioned workflow AST, checkpoints, bounded retry/approval/parallel nodes), E (tool/plugin ABI, fixtures), F (measured self-improvement with validation before promotion). | `docs/TODO.md` (Resolved table, AI-native shell entry); `docs/custom-ai-shell-plan.md:184,199,209,219`; `docs/adrs/ADR-29.md` (Accepted) | Phase numbering reconciled between the two plans, then Phase C taken | M |
| 27 | Pricing / metadata freshness feeds the spend tracker only (M) | Nothing feeds the SpendLog UI fresh prices, so usage dollars go stale. The SpendLog view exists (`crates/desktop/src/views/chat.rs:36`). **Non-goal, stated so it is not re-litigated:** no cost-based routing, no model switching on price — the coordinator must never see or care about cheap vs expensive models. | `docs/TODO.md` (Resolved table, model metadata / price freshness); `docs/STATUS.md:130-133` | The display-only freshness flow is scheduled | M |
| 29 | Plugin marketplace + persistent desktop extension state (L) | No catalog, index, version pinning or signature verification exists in `crates/plugins` (`registry` there means only the core `ToolRegistry`). **The precondition is a trust model, not a feature:** plugins load with no provenance check, and ADR-37 defers registry signature verification to post-v1.0 — so a registry changes the question from "I chose this local file" to "someone published this, executed unchecked". The TOML-secrets sub-item is a **decided non-goal** (keyring-only). MCP SSE/server mode is a separate ADR-43 deferral, not here. | `docs/adrs/ADR-37.md:104-105`; `docs/adrs/ADR-43-skills-mcp-and-extension-manager.md:85-89,162` | A provenance / trust-model ADR | L |
| 30 | Plugin hot reload + remote plugin loading (S) | No reload, invalidate or mtime handling exists in `crates/plugins`; `refresh_new_plugins` deliberately skips already-active plugins (`crates/plugins/src/manager.rs:584`, "already active — skipping") so a refresh never displaces a plugin a running agent holds. The seam is clean: drop → re-load → re-register. Note the real obstacle is that `ActivePlugin` owns a `wasmtime::Store` that cannot be swapped in place. Remote loading is a deliberate non-goal until the row-29 trust model settles. | `docs/TODO.md` (Resolved table, plugin hot-reload — the ADR-21 cite in the old entry was wrong: that ADR is archived at `docs/adrs/archive/ADR-21.md`, is superseded by ADR-14, and has no deferral list) | The trust model from row 29, for remote loading; a demand for reloading a `.wasm` in place, for hot reload | S |
| 31 | Scene memory, persona / long-horizon eval, and the STM gate (M) | L1 typed extraction is wired into the store path (`crates/memory/src/entities.rs:851`, `system.rs:162`, `1702d4c`). `SummarizeOldest` exists as tested library API (`crates/memory/src/short_term.rs:71`, `38bd697`) but is **opt-in and unwired**, blocked behind an unmet superseding-ADR gate (`:9-16`). Scene memory is only a light `Option<String>` label on `L1ExtractedMemory` (`entities.rs:857`) — "scene segmentation light", nothing more. | `docs/TODO.md` (Resolved table, L1 / persona entries); `docs/adrs/ADR-67-m01-context-pool-consolidation.md` | A scene-memory consumer beyond a label; a superseding ADR to ADR-67 M-01 for the STM strategy | M |
| 34 | Audit cleanups: C-03 / C-05 / C-06 / M-08 / M-05 / M-02 (L) | All six still read Not started or Partial. C-03 canonical compaction pipeline (not started), C-05 checkpoint-v2 follow-ups (partial), C-06 acceptance-cycle manual verification (not started), M-08 duplicate public error names (not started), M-05 oversized modules (partial — and **worse than recorded**: `coordinator.rs` is now ~36k lines), M-02 decorative cancellation (partial). | `docs/TODO.md` § "Audit cleanups (`DEFERRED.md` row 34)" (the six items live there; the old `docs/AUDIT_FINDINGS_CURRENT.md` cite names a file that has never existed in the tree) | Module refactors and coverage are scheduled | L |
| 36 | Containerized sandbox — **Windows path only** (L) | The container path is landed and enforced (`d00582b` runtime detection + fail-closed admission gate, `3ae6ea5` shell invocations routed through `docker`/`podman run`, `fdf4800` the required routing marker). Windows Job Objects need `unsafe` FFI, which `[workspace.lints]` hard-denies; ADR-72 §5 records v1 as unsupported on Windows (fails closed, honestly). Re-entry is a **superseding ADR choosing** (a) a narrow audited `unsafe` exception, (b) the safe high-level `windows` crate (new dependency, own ADR), or (c) accept Linux/macOS-only with loud docs. | `docs/adrs/ADR-72-containerized-sandbox-profile.md:5-6,165-179`; `docs/STATUS.md:169,274` | A superseding ADR choosing (a), (b) or (c) | L |
| 37 | Hybrid UI — finish and polish only (M) | The Minimal and Medium tiers are landed. What remains has **no correctness content**: tabbed Settings sub-views, Studio split pane, drag-and-drop agent assignment, animated panels, focus trap, plus lazy state init for infrequently used views. | `crates/desktop/AGENTS.md:65-66`; `docs/hybrid-ui-plan.md:89`; `docs/TODO.md` (Resolved table, hybrid UI) | Post-1.0 UI work | M |
| 41 | Memory-grounded resume M3 — live stress/interrupt evidence (M) | M3a/b/c are code-complete and the exit-gate tests are **present and passing-shape in the tree**: `resume_with_memory_still_grounds` (`crates/orchestrator/src/coordinator.rs:33159`), `kill_midrun_resume_produces_grounded_plan` (`:33825`), `plan_drift_detected_on_tampered_worktree` (`:33429`, `PlanDrift` at `crates/core/src/event.rs:325`; write-back `f4cd6e9`). What remains is the evidence the ROADMAP actually parks it on: a live stress/interrupt cycle producing fidelity evidence. | `ROADMAP.md:199-215` (exit gate + parking condition) | A live stress/interrupt run that shows whether memory-grounded resume is faithful | M |
| 45 | Shell CPU limiting — Windows, cgroup v2, config key (M) | The portable layer is landed (`13cba1c`): Linux `/proc` process-group CPU watchdog, a POSIX `ulimit -S -t` soft backstop, budgets off by default, env-var escape hatch. Remains: no Windows enforcement (governed by the row-36 ADR), cgroup v2 unimplemented, and `cpu_budget_secs` is `None` in every production constructor (`crates/tools/src/shell.rs:231`) with **no TOML key** — the env var is the only operator knob. | `docs/security-threat-model.md:346-351`; `crates/tools/src/shell.rs:220-231` | The row-36 Windows decision; an operator-facing config key | M |

Numbering is preserved from the historical register, including its gaps — rows
were merged and cut over time and are not renumbered.

## Closed

Retired or superseded, before this register existed:

1. GitHub Models provider — retired upstream (ADR-52).
2. Weighted-sum hybrid ranking — superseded by RRF (ADR-22).
3. WRR (weighted round-robin) fairness — superseded by structural per-agent in-flight isolation (ADR-60).
4. Shell argv/cwd containment — landed, `cbb09b0` era + `crates/tools/src/containment.rs` (ADR-55).
5. `route()` deletion / routing carcass — landed `cbb09b0` (2026-09-24); ADR-74 later removed the prompt-level counterpart.
6. `ReviewResume` module removal — landed `cbb09b0` (2026-09-24).
7. Threat sanitizer + grant hash pinning (gaps #2, #8) — closed 2026-09-24.
8. Proxy tool-call parsing fixes 1–3 — landed (ROADMAP:168); the long tail later closed as row 38.
9. Tier-1 + Tier-2 providers — landed 2026-09-24 (22 provider ids registered).
10. Eval `#[ignore]` un-ignore — resolved 2026-09-24.
11. Agent-loop wildcard-panic guard — resolved 2026-09-24.
12. FTS BM25 `rank()` wiring — stale-verified, closed 2026-09-24.
13. Memory encryption for sensitive data (gap #9) — safe-reachable scope landed `1c38c9f` (2026-09-26); `mlock`/`madvise` RAM pinning is unimplementable here (no safe abstraction, `unsafe_code` denied), recorded as accepted.

Closed during reconciliation:

14. Review/validation escalation events — dead `EventKind` variants with no publish site; removed `9385ecf` (2026-09-26); the live path is `CycleVerdict::Escalate`.
15. Audit-log at-rest encryption + retention (gap #5) — landed `7351128` with ADR-73 Accepted (2026-09-26); posture is opt-in by design.
16. ADR-60 S5 agent-process slice — real provider injection `d62ebac` + approval IPC through the shared `ApprovalSink` `7859a0a` (2026-09-26); residual: no `run_id` on the approval wire.
17. Per-session ack queue — landed `8e3c350` (2026-09-27), `MAX_PENDING_ACKS = 2` FIFO with fail-closed overflow; residual: an overflow refusal audits as `RequestAbort` (the sink returns a bare `bool`).
18. Row 1 `cache_stable_prefix` — wired into request assembly, `32d6809` (2026-09-25) (`runtime_runner.rs:2146`, `prompts.rs:55`).
19. Row 2 per-message token accounting — provider-reported usage persisted to message rows, `22d495b` + `a72741f` (2026-09-25) (`message_row_usage`, `runtime_runner.rs:4567,4695`).
20. Row 5 recall cost bounds — char-budget allocator + 5000 ms timeout + skip-with-warning, `92b0be4` (2026-09-24) (`RecallCostBounds`, `rag.rs:133,150-151`).
21. Row 38 proxy parsing long tail — hardened against the remaining forms, `3bc11db` (2026-09-26).
22. Row 42 Windows shell quoting — `cmd_verbatim_launch`, `da611e4` (2026-09-26) (`tools/src/shell.rs:339`, `process.rs`).
23. Row 43 API rate limiting — fixed-window per-client limiter with IPv6 /64 buckets, `61f34b7` + `d8e7bb0` (2026-09-26) (`api-server/src/rate_limit.rs:119`).
24. Row 46 plugin network egress filtering — domain allowlist, `450cb58` (2026-09-26) (`plugins/src/capability.rs:88`).
25. Row 48 Phase-3 benchmarks + CI gate — two-tier gate, `10357cd` (2026-09-27) plus a `bench` job in `.github/workflows/ci.yml` and `bench-baseline.yml`.

Cut (resumable; scope recorded so the cut can be reversed without re-investigation):

26. External code-editor integration (row 25, cut 2026-09-27) — a scope cut, not "unwanted": the in-app editor and diff viewer are landed and were not cut.
27. ADR-58 P5/P6 + TOML diff + canvas DAG editor (row 28, cut 2026-09-27) — every item would need re-specification against the scheduled studio-UI refactor; the P5 migration runner is real standing debt independent of any UI.
28. Certified evolution (row 32, cut 2026-09-27) — a research programme, not backlog; its source document is deliberately git-ignored, so a row citing it was unresolvable by construction.
29. STATUS-tracked follow-ups (row 35, cut 2026-09-27) — a reclassification, not a cancellation: these are per-release human checklists (`TESTING.md`, `docs/live-test-template.md`) that have no terminal state and are not deferred work.
30. Binary installers deb/rpm/tar (row 39, cut 2026-09-27) — wanted eventually; the 4-target tag-triggered release pipeline already exists and has never run (the repo has zero tags), so the real gap is everything downstream of a raw binary.
31. crates.io publish (row 40, cut 2026-09-27) — wanted eventually; no blocker exists (licence allowed, no external path/git deps), so the open question is which subset is published, since the internal graph is coupled.
32. Row 49 issue #135 add-linkage — `FailureDiagnosis` gained `decision_id`/`artifact_path` (serde-additive, `#[serde(default)]`) and the `call_specialist` model-selection/dispatch/settle failure surfaces attach them, so the failure-diagnosis → `OpenProblem` feed records a real `subject_decision_id` + `blocks` path and the question resolves through Q-RESOLVE-LINKED (PR #154). Residual, accepted: surfaces that know no decision (graph-execution failures, tool/provider faults, consult/investigate failures) still open unlinkable questions that stand and age (Q-RESOLVE-UNLINKABLE) — see the world-model module rules.

## Maintenance

Every deferral in this register carries a re-entry condition. Reconcile per
release: re-read the ADRs, `docs/TODO.md`, `ROADMAP.md` and the code; promote met
rows to Closed with a commit SHA or an ADR cite; cut rows whose subject no longer
exists, recording the reason so the cut stays reversible; never drop a row
silently. Re-verify one closed claim per release and move a failed claim back to
Outstanding with a note.

The re-verification pass is a source-code review, not a build. Do not add
per-pass append blocks, "Verification notes" archaeology, or `[verify]` flags to
this file — a claim that cannot be traced to a file, line, or commit does not
belong here.

**Last reconciled:** 2026-09-28 (checkout `fix/coordinator-error-invariant`,
HEAD `caadb2d`; `dev` at `edba420`). Rows 13 and 41 were corrected in this pass
against the tree: the ADR-69 cascade slices are already merged into `dev` and
`origin/main` (the "awaiting merge" premise was stale), and the row-41 exit-gate
tests are present in `coordinator.rs` rather than absent.
