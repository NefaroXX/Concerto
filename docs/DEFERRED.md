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

## Open register

| # | Item | Source + date | Re-entry condition | Size |
|---|------|---------------|--------------------|------|
| 1 | `cache_stable_prefix` wired into engine/request assembly (stable-head prefix discipline) | ADR-48 D1; knob exists at `crates/config/src/schema.rs:649,683` ("the ADR-048 gap knob") but is not wired into any engine/request path | When prompt-cache prefix stability is exercised (TODO.md "prompt-cache stability" ~271–282) | S |
| 2 | Responses `tokens_out` / `tokens_in` written per message from provider-reported usage | ADR-48:27–29,111 — columns exist but are always `0` (inert); per-iteration totals surface resolved provider only | When real provider-reported usage accounting lands (ADR-48 §4/§5; nothing wired today) | S |
| 3 | `.wasm` plugin-file watcher (hot reload on file change) | `[verify]` no in-repo source found for a `.wasm` watcher; nearest is the memory re-index watcher (ROADMAP, ADR-57) | Verify scope first: if "plugin hot-reload" was meant, row 30 applies | S |
| 4 | Per-session ack queue (bounded depth + ack policy) | ADR-68 §6 (queue decision pooled at approval, ~78–97; prefer in-flight budget instead per note) | Implementation phase of ADR-68 desktop ack queue; revisit at desktop persistence work (ADR-43) | S |
| 5 | Recall budget caps + recall timeout (char caps, 5000 ms timeout race, skip-with-warning, capability-envelope payload) | TODO.md memory section (caps/timeout ~81–88) | When a char-budget allocator + timeout guard land in `chunk_selector.rs`/`rag.rs` | M |
| 6 | Tier-3 SDKs — Copilot / Bedrock / Azure / Vertex / watsonx | docs/missing-providers.md Tier 3 (open) | Per-provider wrapper + pairwise parity test; Tier 1/2 already done | L |
| 7 | Doubao / StepFun / Replicate providers | `[unplanned]` — no repo register (zero matches for these names) | Add explicit provider rows if planning changes | L |
| 8 | Vercel Gateway decision | `[verify]` researched in docs/research/multi-provider-resilience.md (Vercel AI SDK among gateways); no in-repo decision record | Confirm whether a dedicated gateway row is wanted | S |
| 9 | Setup wizard per-variant auth flows | `[verify]` wizard exists (`crates/config/src/setup.rs`); per-variant auth steps unverified | Verify scope; no dedicated row in repo docs | M |
| 10 | Live-API smoke tests against real provider endpoints | `[verify]` no in-repo source (TESTING.md has manual live-test matrices only) | Verify; CI uses mocks today | M |
| 11 | M-01: Estimator deduplication + `rag_pct` configurability | ADR-67 follow-ups (~92–98) | When Estimator/rag_pct consolidation is taken up | S |
| 12 | ChunkSelector char-budget allocator | TODO.md:88 ("no char budget across the selected chunks") | With recall budget caps (row 5) | S |
| 13 | Codebase cascade S1/S2/S3 (link store, scoring, slicing) | ADR-69 slices (gated chain; ADR-69-symbolic-cascade.md) | Per-slice gates as designed; risk-managed three slices | L |
| 14 | ThreadSpawn fallback for non-WASM spawn | `[verify]` no symbol/source found | Re-entry on live fault-injection test demand | M |
| 15 | Supervisor beyond 6 agents + D3 + D6 real-embedder swap + multi-level disclosure | ADR-60:112 ("acceptable at 3–6 agents; revisited only if profiling demands"); ADR-58:247 (scheduler/subscription generalization, real-embedder swap, multi-level disclosure deferred) | When profiling shows need beyond 6 agents (with D3/D6 generalization) | L |
| 16 | Single-project limit (one active project per process) | ROADMAP:216 (explicitly deferred/incomplete); STATUS.md | Multi-project support requires per-project scoping of process-global services | L |
| 17 | Eval end-to-end benchmark task (live runtime over real benchmark) | ROADMAP:238–242 | When multi-agent quality + recovery are reliable (ROADMAP ~241) | L |
| 18 | Coordinator restart/resume end-to-end (cross-process continue) | TODO.md:18–25 (Partial; e2e remains); ADR-34 D2 | After checkpoint persistence + evidence-spine resume e2e | L |
| 19 | Audit-log retention policy | ADR-40:47 ("future policy question"); TODO.md:14–17 | When an audit-log retention policy is written | S |
| 20 | FTS BM25 `rank()` wired into retrieval | ROADMAP:162; `crates/memory/src/sync.rs:59,90` writes neutral score 1.0 | When real BM25 rank replaces the neutral 1.0 scorer | M |
| 21 | Fault-injection tests for multi-agent containment (rate limits, malformed tool calls, missing executables, cancellation races, provider disconnects) | TODO.md:218–224 | ADR-26 boundaries; part of live-test phase | L |
| 22 | ADR-47 message `parts` (canonical parts replace flat string content) | ADR-47:85–98 (deferred; flat model retained); ARCHITECTURE-V2.md:323 | With ADR-46/48 reasoning + parts split (Phase 2) | L |
| 23 | ADR-53 per-token streaming through WASM (heartbeat landed) | ADR-53:137–139 (streaming through WASM deferred; heartbeat keepalive landed per ADR-53/57) | Verify streaming scope remains deferred | M |
| 24 | Shell Phases C–F + slices 1–3 (explain/debug/optimize, workflow AST, tool ABI, measured self-improvement) | TODO.md:114–135; ADR-29; ROADMAP | When Phases C–F are scheduled from the roadmap | L |
| 25 | Code editor integration (external editor + open-in-editor + diffs) | TODO.md:157; ROADMAP | Post-1.0 | M |
| 26 | ADR-49 flat→parts canonicalization (do NOT mark complete) | ADR-49:85–98; TODO.md:87–88 | With message-parts work (row 22) | L |
| 27 | Model metadata / price freshness | TODO.md:201; STATUS.md tracked follow-ups | When model metadata freshness flow lands | M |
| 28 | ADR-58 P5/P6 + TOML diff + canvas DAG editor + partitioning | ADR-58:238–247; STATUS.md ~328–355 (deferred P5 Studio, P6 items, TOML diff, run-one-stage, multi-executor partitioning) | Post-P6 roadmap phase (explicit ADR-58 deferral items) | L |
| 29 | ADR-43 server mode / SSE / marketplace / persistent desktop state / TOML secrets | ADR-43 §3 v1 note (MCP server mode, SSE transport, marketplace/registry, keyring-backed tokens, persistent desktop state deferred); docs/skills.md:202 | When the API surface / web UI materializes | L |
| 30 | Plugin hot-reload / remote / registry / marketplace | TODO.md:108–110 | Requires community/registry story (ADR-21, ADR-43 deferred) | L |
| 31 | L1/STM/PersonaMem memory items (typed extraction, scene memory, persona eval) | TODO.md:58–99 (L1 70–80, STM 65–70, heuristics 89–95, PersonaMem 96–99) | Post-memory relayout (ADR-63/64) | M |
| 32 | Certified evolution (profile-guided CI, safety gates) | TODO.md:286–293; ROADMAP | When certified-evolution research plan lands | L |
| 33 | Review/validation escalation (informational evidence; no terminal conversion) | ADR-35 §9/amendment; TODO.md:48–52; core/src/event.rs:386,394 (event-bearing only) | When coordinator decides terminal escalation (ADR-35 amended 2026-09-05) | S |
| 34 | C-05/C-06/C-03 + M-08/M-05/M-02 audit cleanups | TODO.md:38–41 (C-05), 237 (C-06), 32–33 (C-03), 241 (M-08), 246 (M-05), 251 (M-02); AUDIT_FINDINGS_CURRENT.md | When module refactors + coverage are scheduled | L |
| 35 | STATUS-tracked follow-ups (ENV_LOCK, glyphs, multiline, P4/ADR-59 deferrals, release checklist) | STATUS.md "Tracked follow-ups" ~328–355 | Per tracked follow-up row; each release | M |

## Closed appendix (each line verified with a repo source)

1. GitHub Models — retired provider (STATUS:16–18, README, ROADMAP:214, ADR-52).
2. Weighted-sum hybrid ranking — superseded by RRF (ADR-22; ADR-63:7–10).
3. WRR (weighted round-robin) fairness — superseded by structural per-agent in-flight isolation (ADR-60 §D1/D2 note).
4. Shell argv/cwd containment (ADR-55) — landed (`crates/tools/src/containment.rs`; `shell.rs:4,668`; STATUS.md:72 asserts 21d4d3e landing).
5. route() deletion / routing carcass — landed (commit `cbb09b0`; ADR-71:19–22; TODO.md:163–166).
6. ReviewResume module removal — landed (CHANGELOG:33–34; commit `cbb09b0`).
7. Threat sanitizer + hash pinning (threat gaps #2, #8) — closed (security-threat-model.md, 2026-09-19 / 2026-09-24).
8. Proxy tool-call parsing Fixes 1–3 — landed (ROADMAP:168; missing-providers.md:51; flat/content-embedded legacy residual tracked in TODO.md:190–195).
9. Tier-1 + Tier-2 providers — implemented (missing-providers.md: 22 providers delivered).
10. Eval `#[ignore]` un-ignore — resolved 2026-09-24 (TODO.md:207–217; TESTING.md).
11. Agent-loop wildcard-panic guard — resolved 2026-09-24 (TODO.md:225–236).

## Verification notes (2026-09-24)

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
  ThreadSpawn fallback, Doubao/StepFun/Replicate ([unplanned]), Vercel Gateway
  decision (research-doc mention only), setup-wizard per-variant auth
  (setup.rs exists; variants unverified), live-API smoke tests (manual matrices
  only).
- Source trail: `docs/TODO.md`, `docs/STATUS.md`, `ROADMAP.md`,
  `docs/missing-providers.md`, `docs/security-threat-model.md`,
  `docs/adrs/ADR-{21,35,43,47,48,49,53,55,60,67,69}.md`, `docs/ARCHITECTURE-V2.md`,
  `crates/config/src/schema.rs:649,683`, `crates/memory/src/sync.rs:59,90`,
  `crates/core/src/event.rs:386,394`.
