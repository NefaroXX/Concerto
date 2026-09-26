# Deferred Work Register (docs/DEFERRED.md)

**Purpose.** Single, authoritative register of work that has been explicitly
*deferred* (not cancelled, not forgotten) in the Concerto codebase — anything
marked deferred in an ADR, `docs/TODO.md`, `ROADMAP.md`, `docs/STATUS.md`, or
other repo documentation.

**Maintenance rule.** *Every* deferral lands here with a re-entry condition.
Each release, an owner reconciles this register against the ADRs, TODO.md,
STATUS.md and ROADMAP.md, promotes any item whose re-entry condition has been
met from Open to Closed, drops nothing silently, and re-verifies one closed
claim per closed row; anything that fails verification moves back to Open with
a note.

> **Reconciled:** 2026-09-24 (against checkout `fix/coordinator-error-invariant`,
> HEAD `c21b4e1`). Open rows follow the 2026-09-24 inventory, each with its
> source + date, a re-entry condition, and a size. Rows flagged `[verify]` had
> no traceable repo source and are marked for confirmation; rows flagged
> `[unplanned]` have no repo register at all.
> **Append 2026-09-24:** open rows 36–49 added (isolation, UI, provider-parsing
> residual, release, memory-grounded resume, security-threat-model gaps #3–#9,
> Phase 3 benchmarks, ADR-60 S5 agent-process slice); Tier-1/2 provider closure
> in the Closed appendix dated/sourced; row 32 flagged `[dangling-cite]`.
> **Append 2026-09-26 (owner decisions 2026-09-25/26):** row 47 **closed** (the
> safe-reachable scope of threat gap #9 landed in `1c38c9f`; the RAM-pinning
> residual is recorded in the Closed appendix, not quietly dropped). Rows 36 and
> 45 annotated with the shipped state (`d00582b`/`3ae6ea5`/`fdf4800` and
> `13cba1c`) and **kept OPEN** — what remains is the Windows path (row 36's
> superseding-ADR decision also governs row 45) plus a CPU-budget config key.
> No other rows touched; no renumbering; numbering gaps left intentional.

## Open register

| # | Item | Source + date | Re-entry condition | Size |
|---|------|---------------|--------------------|------|
| 1 | `cache_stable_prefix` wired into engine/request assembly (stable-head prefix discipline) | ADR-48 D1; knob exists at `crates/config/src/schema.rs:649,683` ("the ADR-048 gap knob") but is not wired into any engine/request path | When prompt-cache prefix stability is exercised (TODO.md "prompt-cache stability" ~271–282) | S |
| 2 | Responses `tokens_out` / `tokens_in` written per message from provider-reported usage | ADR-48:27–29,111 — columns exist but are always `0` (inert); per-iteration totals surface resolved provider only | When real provider-reported usage accounting lands (ADR-48 §4/§5; nothing wired today) | S |
| 3 | `.wasm` plugin-file watcher (hot reload on file change) | `[verify]` no in-repo source found for a `.wasm` watcher; nearest is the memory re-index watcher (ROADMAP, ADR-57) | Verify scope first: if "plugin hot-reload" was meant, row 30 applies | S |
| 4 | Per-session ack queue (bounded depth + ack policy) | ADR-68 §6 (queue decision pooled at approval, ~78–97; prefer in-flight budget instead per note) | Implementation phase of ADR-68 desktop ack queue; revisit at desktop persistence work (ADR-43) | S |
| 5 | Recall budget caps + recall timeout (char caps, 5000 ms timeout race, skip-with-warning, capability-envelope payload; includes the ChunkSelector char-budget allocator across the selected chunks — TODO.md:88 "no char budget across the selected chunks"; naming trap: the `chunk_selector.rs` constants are compaction selection, not recall) | TODO.md memory section (caps/timeout ~81–88) | When a char-budget allocator + timeout guard land in `chunk_selector.rs`/`rag.rs` | M |
| 6 | Tier-3 SDKs — Copilot / Bedrock / Azure / Vertex / watsonx | docs/missing-providers.md Tier 3 (open) | Per-provider wrapper + pairwise parity test; Tier 1/2 already done | L |
| 7 | Doubao / StepFun / Replicate providers | `[unplanned]` — no repo register (zero matches for these names) | Add explicit provider rows if planning changes | L |
| 8 | Vercel Gateway decision | `[verify]` researched in docs/research/multi-provider-resilience.md (Vercel AI SDK among gateways); no in-repo decision record | Confirm whether a dedicated gateway row is wanted | S |
| 9 | Setup wizard per-variant auth flows | `[verify]` wizard exists (`crates/config/src/setup.rs`); per-variant auth steps unverified | Verify scope; no dedicated row in repo docs | M |
| 11 | M-01: Estimator deduplication + `rag_pct` configurability | ADR-67 follow-ups (~92–98) | When Estimator/rag_pct consolidation is taken up | S |
| 13 | Codebase cascade S1/S2/S3 (link store, scoring, slicing) | ADR-69 slices; IMPLEMENTED on fix/coordinator-error-invariant 2026-09-24 (S1 link store + write path, S1 activation, S2 scoring/decay/caps, purge fix, S3 observability — commits 60841dd/af98afe/224f0f9/7713b8d/7584d2d; tests green incl multi-hop guard) | CLOSED on merge of that branch + production proof (link verdicts firing, cascade reorder observed in a live run) | L |
| 15 | Supervisor beyond 6 agents + D3 + D6 real-embedder swap + multi-level disclosure | ADR-60:112 ("acceptable at 3–6 agents; revisited only if profiling demands"); ADR-58:247 (scheduler/subscription generalization, real-embedder swap, multi-level disclosure deferred) | When profiling shows need beyond 6 agents (with D3/D6 generalization) | L |
| 16 | Single-project limit (one active project per process) | ROADMAP:216 (explicitly deferred/incomplete); STATUS.md | Multi-project support requires per-project scoping of process-global services | L |
| 17 | Eval end-to-end benchmark task (live runtime over real benchmark) | ROADMAP:238–242 | When multi-agent quality + recovery are reliable (ROADMAP ~241) | L |
| 18 | Coordinator restart/resume end-to-end (cross-process continue) | TODO.md:18–25 (Partial; e2e remains); ADR-34 D2 | After checkpoint persistence + evidence-spine resume e2e | L |
| 21 | Fault-injection tests for multi-agent containment (rate limits, malformed tool calls, missing executables, cancellation races, provider disconnects) | TODO.md:218–224 | ADR-26 boundaries; part of live-test phase | L |
| 22 | ADR-47 message `parts` (canonical parts replace flat string content) | ADR-47:85–98 (deferred; flat model retained); ARCHITECTURE-V2.md:323 | **GATED DOCTRINE 2026-09-25** — stays OPEN, but gated on ADR-47's own reopen condition (:78–93): reopen **only** when a consumer proposes/needs richer message content the flat shape cannot express (multi-part tool bodies, image/file `File` parts, structured `Thinking`/`RedactedThinking` on Anthropic/Gemini paths, or structured data smuggled into `content: String`). Migration preconditions recorded 2026-09-25 — all three required before any parts work: **(a)** a named consumer need (ADR-47:80–81); **(b)** a parts-joined-text equivalence test — joining `parts` back to flat text must reproduce today's bytes, and Fix 2 strictness must not loosen (proxy-tool-call-fix.md:5,60 — content-embedded strict path buffers content to turn end); **(c)** a checkpoint/`state_json` migration plan for old rows (`orchestration_checkpoints.state_json` is `TEXT NOT NULL`, migration 019:10; `crates/sessions/src/lib.rs:150`), shipped additively with `serde(default)` + legacy-column fallback in one dedicated window, never as a side effect of an unrelated feature (ADR-47:62–65,109–110; ARCHITECTURE-V2.md §10). Verified migration-safe meanwhile: the Mimo/loose-tier tool mods operate on `ToolDefinition`/`ToolCall` **values**, not message shape — `schema_loose.rs:113` `adapt_tool_definitions(&mut [ToolDefinition])`, `:132` `unflatten_tool_arguments(&mut serde_json::Value)`, `google.rs:55` `adapt_tools_for` mutates `request.tools` — so they neither block nor depend on a parts migration. | L |
| 23 | ADR-53 per-token streaming through WASM (heartbeat landed) | ADR-53:137–139 (streaming through WASM deferred; heartbeat keepalive landed per ADR-53/57) | Verify streaming scope remains deferred | M |
| 24 | Shell Phases C–F + slices 1–3 (explain/debug/optimize, workflow AST, tool ABI, measured self-improvement) | TODO.md:114–135; ADR-29; ROADMAP | When Phases C–F are scheduled from the roadmap | L |
| 25 | Code editor integration (external editor + open-in-editor + diffs) | TODO.md:157; ROADMAP | Post-1.0 | M |
| 27 | Pricing/metadata freshness feeds the VISUAL SPEND TRACKER ONLY (rescoped 2026-09-25: display-only, spend tracker) | TODO.md:201; STATUS.md tracked follow-ups | When the display-only pricing/metadata freshness flow lands. Stale prices make usage dollars wrong; routing must never see prices. Non-goal: the coordinator must never know or care about cheap vs expensive models; no cost-based routing, no model switching on price, and no coordinator coupling of any kind. | M |
| 28 | ADR-58 P5/P6 + TOML diff + canvas DAG editor + partitioning | ADR-58:238–247; STATUS.md ~328–355 (deferred P5 Studio, P6 items, TOML diff, run-one-stage, multi-executor partitioning) | Post-P6 roadmap phase (explicit ADR-58 deferral items) | L |
| 29 | ADR-43 server mode / SSE / marketplace / persistent desktop state / TOML secrets | ADR-43 §3 v1 note (MCP server mode, SSE transport, marketplace/registry, keyring-backed tokens, persistent desktop state deferred); docs/skills.md:202 | When the API surface / web UI materializes | L |
| 30 | Plugin hot-reload / remote / registry / marketplace | TODO.md:108–110 | Requires community/registry story (ADR-21, ADR-43 deferred) | L |
| 31 | L1/STM/PersonaMem memory items (typed extraction, scene memory, persona eval) | TODO.md:58–99 (L1 70–80, STM 65–70, heuristics 89–95, PersonaMem 96–99) | Post-memory relayout (ADR-63/64) | M |
| 32 | Certified evolution (profile-guided CI, safety gates) | TODO.md:286–293; ROADMAP:189–193 — `[dangling-cite]` the referenced `docs/research/certified-universal-evolution.md` (cited at TODO.md:287, ROADMAP:190) is missing on disk | When certified-evolution research plan lands (restore/rewrite the research doc or re-scope) | L |
| 33 | Review/validation escalation (informational evidence; no terminal conversion) | ADR-35 §9/amendment; TODO.md:48–52; core/src/event.rs:386,394 (event-bearing only) | When coordinator decides terminal escalation (ADR-35 amended 2026-09-05) | S |
| 34 | C-05/C-06/C-03 + M-08/M-05/M-02 audit cleanups | TODO.md:38–41 (C-05), 237 (C-06), 32–33 (C-03), 241 (M-08), 246 (M-05), 251 (M-02); AUDIT_FINDINGS_CURRENT.md | When module refactors + coverage are scheduled | L |
| 35 | STATUS-tracked follow-ups (ENV_LOCK, glyphs, multiline, P4/ADR-59 deferrals, release checklist) | STATUS.md "Tracked follow-ups" ~328–355 | Per tracked follow-up row; each release | M |
| 36 | Containerized sandbox bundle — `SandboxProfile::Containerized` OS-level isolation — **shipped 2026-09-26 for the container path (row STAYS OPEN: only the Windows path is outstanding)** | TODO.md:103; ROADMAP:235; security-threat-model.md §6 gap #1 (:310–315, "No Containerized Plugin Sandbox"); was a real stub — variant declared but not implemented (architecture.md:250, STATUS.md:272), plugins ran only under the WASM capability sandbox. **Landed 2026-09-26 in three slices:** `d00582b` (ADR-72 + enforceable core half — `core::sandbox` docker/podman runtime detection, `SimplePolicyEngine::check_sandbox` fail-closed admission gate, rules `sandbox_containerized_runtime_unavailable` / `_unenforceable`), `3ae6ea5` (shell invocations actually routed through the container — `tools::container` builds the `docker`/`podman run` argv from a planned `ShellPlan` as `ShellPlan::Direct`, opt-in `ShellTool::with_container`, `command_facts` audits the container argv), `fdf4800` (`CommandRouting` marker — closes the fail-open where `Containerized` was selected without routing) | **Windows path only.** Windows Job Objects need `unsafe` FFI (`windows-sys` `CreateJobObjectW` / `AssignProcessToJobObject`), which the workspace hard-denies (`[workspace.lints] unsafe_code`). ADR-72 §5 records v1 as **unsupported on Windows** (probe returns `Unavailable` → `Containerized` refused, fail-closed and honest), not shipped. Recorded options for a future Windows story: **(a)** a narrow, audited `unsafe` exception; **(b)** the safe high-level `windows` crate — safe Job Object bindings, no `unsafe` in our code, but a new dependency requiring its own ADR; **(c)** accept Linux/macOS-only with loud documentation. **Re-entry: a superseding ADR that chooses among (a)/(b)/(c).** | L |
| 37 | Hybrid UI full scope (tabbed Settings, Studio split pane, drag-and-drop agent assignment, focus-trap) | TODO.md:145; ROADMAP:147 (post-1.0); docs/hybrid-ui-plan.md — standalone, not part of the world-class plan | Post-1.0 | L |
| 38 | Flat/content-embedded tool-call parsing residual (beyond proxy Fixes 1–3) | TODO.md:190–195; ROADMAP:165–168 (residual after Fixes 1–3 landed, see Closed #8); docs/proxy-tool-call-fix.md | When sanitized proxy fixtures + pairwise verification land against real OpenAI-compatible proxies | S |
| 39 | Binary installers (deb/rpm/tar) | TODO.md:260–263; ROADMAP:194 (Later); STATUS.md:11–14 (no installer packages promised today; only `.tar.gz` via scripts/release.sh) | When a release/distribution decision is made | M |
| 40 | crates.io publish | TODO.md:264–265; ROADMAP:194 (Later); STATUS.md:12–13 (not published; `publish = false` in workspace Cargo.toml) | Release decision + metadata audit (workspace `publish=false`, licence, repository links) | M |
| 41 | Memory-grounded resume — Phase 6 M3 (a) run-scoped priming, (b) outcome write-back, (c) plan↔worktree drift gadget | ROADMAP:196–212 (live-test-gated; exit gate = three tests, :208–212; no schema change, no new ADR at this scope) | When live stress/interrupt evidence lands + the three tests pass | M |
| 42 | Windows shell quoting weakness (threat gap #3) | security-threat-model.md §6 :323–328 (~4 h; cmd.exe quoting weaker than POSIX) | When a security milestone is scheduled (prefer `bypass_shell` on Windows) | S |
| 43 | API server per-client rate limiting (threat gap #4) | security-threat-model.md §6 :332–337 (~4 h) | When a security milestone is scheduled | S |
| 44 | Audit-log at-rest encryption (threat gap #5) — **includes the audit-log retention policy** (former row 19, merged 2026-09-25: retention bounds the encrypted vault) | security-threat-model.md §6 :339–344 (~8 h; SQLCipher / encrypt SQLite); retention part: ADR-40:47 ("Audit retention remains a future policy question, not a session one" — no age-/size-based audit-only truncation, any future one belongs in its own ADR) + TODO.md:14–17 (define retention/archival for `audit_log`; the log is grow-only by design) | When a security milestone is scheduled; the retention/archival policy settles in the same window, since the retention bounds are what bound the encrypted store | M |
| 45 | Shell-command CPU rate limiting (threat gap #6) — **portable layer shipped 2026-09-26 (row STAYS OPEN: Windows + cgroup v2 residual)** | security-threat-model.md §6 :346–351 (~12 h; cgroup/ulimit integration). **Landed 2026-09-26 in `13cba1c`:** Linux `/proc` watchdog sampling aggregate process-group user+sys CPU (`tools::cpu_accounting`, `CpuBudget` + `ProcessHandle::run_limited`) that SIGKILLs the group and returns an explicit budget error; POSIX `ulimit -S -t N` soft backstop prelude on wrapped plans (soft-only: `SIGXCPU` is recognisable, a hard `SIGKILL` is not); budgets **default off** and the existing wall-clock timeout is unchanged; `CONCERTO_SHELL_CPU_BUDGET_SECS` escape hatch (config wins, unparsable/0 = off). Composes with row 36 — inside a container the `ulimit` prelude rides along and the runtime is not asked to re-impose a CPU ceiling. **Residual: no CPU enforcement on Windows** (no safe Job Object path; row 36's Windows decision governs this too) and **cgroup v2 is not implemented** (needs privileged cgroupfs writes / `unsafe`) | A superseding ADR for the Windows Job Object path (row 36's options (a)/(b)/(c)) and/or an explicit cgroup-v2 decision. **Tracked sub-item:** the CPU budget has **no TOML config field yet** — `ShellConfig::cpu_budget_secs` (`crates/tools/src/shell.rs:220`) is `None` in every production call site (`with_profile` / `allow_all` / `new`), so the env var is the only operator knob; add a config key when the budget is wired for operators. | M |
| 46 | Plugin network egress filtering (threat gap #7) | security-threat-model.md §6 :355–360 (~8 h; network capability allowlist) | When a security milestone is scheduled | M |
| 48 | Codebase-world-class Phase 3 criterion benchmarks + CI benchmark gate | world-class-plan.md:186–208 (Phase 3 group); TODO.md:153–154; part of the Phases 1–5 group (TODO.md:148–156) | When the Phase 3 benchmark milestone is scheduled (criterion suite + CI gate) | M |
| 49 | ADR-60 S5 agent-process slice — mock-only provider + `DenyAllApprovalSink` | crates/orchestrator/src/bin/agent_process.rs (`CONCERTO_PROVIDER` accepts only "mock", :137–152; interactive approvals dropped as `DenyAllApprovalSink`, :155, :292–336 — "ADR-60 deferred" per code comment) | When supervisor wiring completes: real provider injection + approvals/acks surfaced through the supervisor (today always denied, never a real approval channel) | S |

## Closed appendix (each line verified with a repo source)

1. GitHub Models — retired provider (STATUS:16–18, README, ROADMAP:214, ADR-52).
2. Weighted-sum hybrid ranking — superseded by RRF (ADR-22; ADR-63:7–10).
3. WRR (weighted round-robin) fairness — superseded by structural per-agent in-flight isolation (ADR-60 §D1/D2 note).
4. Shell argv/cwd containment (ADR-55) — landed (`crates/tools/src/containment.rs`; `shell.rs:4,668`; STATUS.md:72 asserts 21d4d3e landing).
5. route() deletion / routing carcass — landed (commit `cbb09b0`; ADR-71:19–22; TODO.md:163–166).
6. ReviewResume module removal — landed (CHANGELOG:33–34; commit `cbb09b0`).
7. Threat sanitizer + hash pinning (threat gaps #2, #8) — closed (security-threat-model.md, 2026-09-19 / 2026-09-24).
8. Proxy tool-call parsing Fixes 1–3 — landed (ROADMAP:168; missing-providers.md:51; flat/content-embedded legacy residual tracked in TODO.md:190–195).
9. Tier-1 + Tier-2 providers — implemented 2026-09-24 (missing-providers.md: 22 provider ids registered, :5–33; "Tier 1 & 2 DONE", :53–57; ROADMAP:46–52 "Provider reach (2026-09-24)" — config-first factory covers all 22).
10. Eval `#[ignore]` un-ignore — resolved 2026-09-24 (TODO.md:207–217; TESTING.md).
11. Agent-loop wildcard-panic guard — resolved 2026-09-24 (TODO.md:225–236).
12. FTS BM25 `rank()` wired into retrieval — stale-verified, closed 2026-09-24 (chunk FTS always BM25 rank-ordered: `fts.rs:148` `ORDER BY rank`, RRF fusion `rag.rs:412-459`; stored 1.0 inert by construction `sync.rs:61,130`; proven by tests `fts.rs:404-450,458-498`).
13. Memory encryption for sensitive data (threat gap #9) — **closed 2026-09-26 per owner decision**, with the residual recorded rather than dropped. The safe-reachable scope shipped in `1c38c9f`: `concerto_core::SecretString` (zero-on-drop `zeroize` wipe of the backing buffer, `Debug`/`Display` render a redaction marker, `expose()` returns a borrow so call sites stop cloning keys, credential store and `ProviderConfig` resolve through `get_secret`, `PendingConfig`/`ProviderRequest` no longer derive `Debug` over a raw key, and the Google connector scrubs its `?key=` URL out of transport errors) — module docs at `crates/core/src/secret.rs`, tests cover the drop wipe, redaction (direct / embedded / collection / credential-store / provider-request / wizard-config) and the Google diagnostic scrub. **Residual, unimplemented by design rather than by oversight: `mlock`/`madvise` RAM pinning.** No safe abstraction for it exists in the dependency graph — `nix` (present only as a `signal` + `feature` cfg gate in `crates/tools`/`crates/plugins`) exposes it as a raw-pointer `unsafe fn`, and the workspace hard-denies `unsafe_code`, so the RAM-pinning half of threat gap #9 stays open work. Closure is therefore scoped: the *safe-reachable* mitigation is closed, the pinning half is not.

## Verification notes (2026-09-24)

- 2026-09-25 append: **row 14 cut 2026-09-25 per owner decision** — no symbol
  and no source exist. A repo-wide case-insensitive search for `ThreadSpawn` /
  `thread_spawn` matches only this register itself (this row and the
  remaining-unknowns note below); there is no `ThreadSpawn` type, module, or
  spawn-fallback path in the tree, and the row's own re-entry condition ("live
  fault-injection test demand") is hypothetical rather than a cited deferral.
  Cutting it removes a task that had nothing to resume. The #14 numbering gap is
  left intentionally (no renumbering), matching the row 10 / row 12 / row 26
  precedent. Note: `ThreadSpawn` was also removed from the remaining-unknowns
  list below, since that list enumerates live `[verify]` rows only.
- 2026-09-25 append: **row 19 merged into row 44** (audit-log retention policy →
  an included sub-part of audit-log at-rest encryption) and row 19 deleted. Both
  citations were re-verified first — `docs/adrs/ADR-40.md:47` ("Audit retention
  remains a future policy question, not a session one"; any future audit-only
  truncation belongs in its own ADR) and `docs/TODO.md:14–17` (`audit_log` is
  grow-only by design). They are folded rather than dropped because retention is
  what bounds the encrypted store: the threat gap (#5) and the policy question
  are settled in the same window or not at all. Row 44's size stays **M** — the
  ~8 h encryption work dominates; the S-sized retention policy is a decision plus
  a prune path, not additional headline effort. Row 44's re-entry condition is
  unchanged in trigger ("when a security milestone is scheduled") and extended to
  state that retention settles alongside it. The #19 numbering gap is left
  intentionally (no renumbering).
 — manual
  live matrices in TESTING.md remain the practice; no automated canary job
  planned. Cut by owner decision 2026-09-25 ("weed"), not as a miscitation:
  the row was already flagged `[verify]` with no in-repo source (CI uses
  mocks today), and what it proposed to automate is the manual matrix sheet,
  which is not being replaced. The #10 numbering gap is left intentionally
  (no renumbering), matching the row 12 / row 26 precedent.
- 2026-09-25 append: **row 26 cut 2026-09-25 as miscited duplicate of #22 (no
  separate task behind the label)**. Its two citations both resolve to other
  work: `ADR-49:85–98` is the **config-catalog flattening** deferred item
  ("merging `ProviderConfig` / `ModelPinConfig` / `MultiAgentConfig` into a
  single catalog schema"), not message parts — ADR-47 is the sole message-parts
  authority; and `TODO.md:87–88` now cites the **L1-extraction** source paths
  (`src/core/prompts/l1-extraction.ts`, `src/core/prompts/l1-dedup.ts`,
  `src/core/record/l1-dedup.ts`) under the L1 typed-extraction item, carried by
  row 31. The row's own re-entry condition ("with message-parts work (row 22)")
  confirmed the duplication. The #26 numbering gap is left intentionally (no
  renumbering), matching the row 12 precedent. The real ADR-49 flattening
  deferral remains recorded in its own ADR and is not tracked here.
- 2026-09-25 append: row 22 annotated **GATED DOCTRINE** and kept OPEN. The
  gate is ADR-47's own wording ("Reopen when a consumer proposes/needs **richer
  message content that the flat shape cannot express**", ADR-47:78–93), not a
  re-scoped trigger. Row size (L) and source cell were left unchanged. The
  Mimo/loose-tier tool mods were checked to be migration-safe (they act on
  `ToolDefinition`/`ToolCall` values, not message shape).
- Row 12 (ChunkSelector char-budget allocator) merged into row 5 (recall
  budget caps + timeout) on 2026-09-24; the #12 numbering gap is left
  intentionally (no renumbering).
- Open row 20 (FTS BM25 `rank()` wired into retrieval) was closed 2026-09-24
  as **stale-verified** — by the time it was registered, the deferral's
  re-entry condition was already met in code (see Closed #12 for evidence).
- Every closed claim was verified by grep/read against a repo source before
  listing. Two inventory "closed" claims failed verification and are instead
  carried as open rows flagged `[verify]`: **health echo** (no feature named
  "health echo" exists; `concerto health` subcommand is real and landed —
  README:251–267, ADR-49, ADR-54 — if that was the intended referent, move to
  Closed) and **plugin/MCP static registration** (only a research-doc mention
  at docs/research/ai-native-shell-implementation-plan.md:26; no completion
  evidence).
- ADR-55 v2 landing SHAs (21d4d3e, 3cb251d, 44f2deb, 9ec27f3) are **not
  reachable** from this checkout (HEAD `c21b4e1`); closure is verified by code
  presence (`containment.rs`, `shell.rs:4,668`) + STATUS.md:72 rather than by
  git objects. Containing hardening commits (`ff3c768`, `27263cd`, `edfa67c`,
  `40965ac`) are present.
- Remaining unknowns (flagged `[verify]` in the open table): `.wasm` watcher,
  Doubao/StepFun/Replicate ([unplanned]), Vercel Gateway
  decision (research-doc mention only), setup-wizard per-variant auth
  (setup.rs exists; variants unverified).
- 2026-09-24 append: `docs/research/certified-universal-evolution.md` is cited
  at TODO.md:287 and ROADMAP:190 but is absent from `docs/research/` on this
  checkout — carried on open row 32 as `[dangling-cite]` (annotated, not
  deleted). The `live-test-skills-mcp.md` Nextest expectation was refreshed to
  the latest documented full-workspace count (3421, ROADMAP evidence-spine
  verification) after the eval `#[ignore]` was removed (Closed #10); the exact
  count is re-confirmed on the next live-test run, not by a fresh suite run
  here.
- Source trail: `docs/TODO.md`, `docs/STATUS.md`, `ROADMAP.md`,
  `docs/missing-providers.md`, `docs/security-threat-model.md`,
  `docs/adrs/ADR-{21,35,43,47,48,49,53,55,60,67,69}.md`, `docs/ARCHITECTURE-V2.md`,
  `crates/config/src/schema.rs:649,683`, `crates/memory/src/sync.rs:61,130`,
  `crates/memory/src/fts.rs:148,404-450,458-498`, `crates/memory/src/rag.rs:412-459`,
  `crates/core/src/event.rs:386,394`.

## Verification notes (2026-09-26)

Owner decisions of 2026-09-25/26, executed 2026-09-26 against HEAD `fdf4800`.
All three commits cited below are reachable from this checkout; every claim was
checked against code or an ADR, not against commit messages alone.

- **Row 47 closed** (see Closed #13). Verified in the tree, not just in the
  commit body: `crates/core/src/secret.rs` documents the three closures
  (zero-on-drop, no-rendering `Debug`/`Display`, explicit `&str` reads via
  `expose()`) and states at lines 19–21 that `mlock`/`madvise` page pinning is
  deliberately **not** covered. The *reason* recorded in the appendix was
  re-checked against the dependency graph: `nix` is a direct dependency of
  `crates/tools` and `crates/plugins` only, with `default-features = false` and
  `features = ["signal"]` / `["signal", "feature"]`, and it is used solely for
  signal handling plus the `sysconf` cfg gate; `mlock`/`madvise` would arrive as
  raw-pointer `unsafe fn`, which `[workspace.lints]` denies via `unsafe_code`.
  No safe `mlock` wrapper is present anywhere in `Cargo.lock`. Closure is scoped
  to the safe-reachable mitigation and the residual is stated in the appendix —
  it is not recorded as done. The `#47` numbering gap is left intentionally (no
  renumbering), matching the row 10 / 12 / 14 / 19 / 26 precedent.
- **Row 36 annotated, kept OPEN** (size unchanged at **L**). All three slices
  verified present: ADR-72 exists at
  `docs/adrs/ADR-72-containerized-sandbox-profile.md` (with the design landed in
  `d00582b`), routing in `3ae6ea5`, and the `CommandRouting` marker in
  `fdf4800`. The row is **not** closed because the remaining work is the Windows
  path, and ADR-72 §5 explicitly declines to ship it: "Decision (v1): the
  `Containerized` profile is **not supported on Windows**", with the probe
  returning `Unavailable` and the engine refusing
  `sandbox_containerized_runtime_unavailable` — fail-closed, but a decline, not a
  delivery. §5 also names the blocker independently of the ADR prose: the
  repository hard-denies `unsafe_code`, which blocks the `windows-sys` FFI
  (`CreateJobObjectW` / `AssignProcessToJobObject`). Verified in the graph: no
  `windows` or `windows-sys` entry in any workspace `Cargo.toml` — both appear
  in `Cargo.lock` only as transitive packages of other crates, so option (b) is
  genuinely a **new direct dependency** and option (a) a lint-denial change,
  neither of which can ride along with a doc update. The row's re-entry was
  rewritten from the stale "when post-1.0 isolation design lands" to the actual
  gate: a **superseding ADR choosing among (a) audited `unsafe` exception /
  (b) safe `windows` crate / (c) documented Linux-macOS-only**.
- **Row 45 annotated, kept OPEN** (size unchanged at **M**). The portable layer
  is verified in the tree: `crates/tools/src/cpu_accounting.rs` (process-group
  CPU sampling), `ProcessHandle::run_limited` in `crates/tools/src/process.rs`,
  and `ShellConfig::cpu_budget_secs` defaulting to `None` with the
  `CONCERTO_SHELL_CPU_BUDGET_SECS` fallback in `crates/tools/src/shell.rs:58`
  (config wins, including `Some(0)` = off). Two residuals are recorded. First,
  **Windows**: `cpu_accounting::supported()` is a pure `cfg` gate that is
  Linux-only, argv-direct and cmd.exe-verbatim plans skip the `ulimit` prelude
  and rely on that watchdog, so on Windows neither backstop applies and there is
  no CPU enforcement at all — governed by row 36's Windows decision, cross-
  referenced rather than duplicated. Second, **cgroup v2**: not implemented;
  it needs privileged cgroupfs writes, which is not available under the current
  no-`unsafe` posture. The "no TOML config field yet" sub-item is tracked in the
  row's re-entry cell: `cpu_budget_secs` appears **only** in
  `crates/tools/src/shell.rs` and in that crate's tests — no config-crate schema
  field and no production call site sets it (`ShellTool::with_profile`,
  `allow_all`, and `new` all take the `Default`), so today the env var is the
  only operator knob. Noted as a small tracked sub-item rather than folded into
  the headline row, since it is a config-wiring task, not new enforcement work.
- **Scope of this pass:** rows 36, 45, and 47 only. All other open rows, the
  earlier verification notes, and the existing Closed #1–#12 lines are unchanged.
  No renumbering; gaps left intentional. Not verified here (out of scope, and
  flagged as follow-up rather than asserted): `docs/security-threat-model.md` §6
  gaps #1, #6 and #9 still carry their original open text — unlike gap #8, none
  of them is marked `✅ DONE`, so the threat model and this register currently
  disagree for row 36 (partially shipped), row 45 (partially shipped) and row 47
  (closed with residual). That file should be reconciled in a separate docs pass.
- Pre-existing formatting defect noted, **not** fixed in this pass: the 2026-09-24
  notes above contain a bare `— manual` fragment on its own line, left behind by the
  2026-09-25 row-10 cut, which orphans the tail of that bullet. Left alone to
  keep this diff to the owner decisions above; worth a one-line repair.
