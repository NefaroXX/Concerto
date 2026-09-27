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
> **Append 2026-09-26 (group-F outcomes):** row 49 **closed** (ADR-60 S5 agent-process
> slice landed in two commits — real provider injection, then approval IPC routed
> through the same `Arc<dyn ApprovalSink>` the in-process paths use, fail-closed
> intact; residual: no embedded `run_id` on the approval wire). Row 21 **merged
> into row 17** and deleted (both halves landed: four in-process crash-window
> scenarios, then an env-gated `#[ignore]`d live-runtime eval leg that never runs
> in CI; row 17 stays OPEN on coverage breadth + live-flake quarantine). Row 33
> **cut** (the two event variants it tracked had no publish site and no consumer;
> the live path is `CycleVerdict::Escalate`). Row 44 **closed** (audit at-rest
> encryption + retention shipped in `7351128` with ADR-73 Accepted). Row 15
> **rescoped and kept OPEN at L** (per owner: "beyond 6 agents" is not a 6-agent
> cap — agents are additive with no hard cap; the re-entry is now the three real
> sub-items, cited to ADR-60 Revision v1.2 item 4 rather than the `ADR-60:112`
> IPC-overhead cost note). Verification notes for today are appended below. No
> renumbering; gaps left intentionally.
> **Append 2026-09-27 (delegation doctrine, ADR-74 Accepted):** the doctrine
> landed in four commits (`5a22405` prompt inversion, `269c344` delegation
> guard, `eefd45e` ladder hold, `115f85c` agent-axis takeover). **No row is
> opened, closed, cut, or merged** — nothing this pass implemented was a
> registered deferral. Two existing lines are annotated **minimally**: Closed #5
> (route() deletion / routing-carcass removal) to record that ADR-74 is its
> *prompt-level* counterpart, and open row 18 (coordinator restart/resume) so
> "the delegation guard is journal-checkpointed" is not mistaken for
> "cross-process continue is done". Both are one-clause additions; the table is
> not restructured and nothing is renumbered. Verification notes for today are
> appended below. ADR-74's own open items (no `can_cover` UI, no example config,
> the hold being planning-rung-only, and the coordinator dispatch prompt
> bypassing `PromptBuilder`) live in ADR-74 §Known gaps and are deliberately
> **not** register rows — none of them is a deferral this register ever
> recorded.
> **Append 2026-09-27 (shell + plugin-streaming pass):** two rows touched, no
> status change. Row 24 **rescoped and kept OPEN, size L → M** — the shell
> runtime is *not* unbuilt: Phases A and B and ADR-28 profile slices 1 **and** 2
> have all landed, and the row now records that evidence, states the real
> residual (the C verbs, Phase D, Phase E, Phase F, and two small slice-2
> requirements), and splits the work — the C verbs as one self-contained
> piece, D–F as a separate design decision. A **prerequisite** is now the
> first re-entry item: the repo plan numbers phases A–F and the research plan
> numbers them 0–5, and `TODO.md:136–137` already says to reconcile them
> before starting. Row 23 **stays OPEN and deferred**; only its trigger was
> sharpened, so the precondition is a *named* plugin kind that generates
> tokens incrementally and the cost is stated plainly (a new ABI export or
> host-fn chunk sink **plus an ABI version bump** — ADR-53 deliberately did
> neither). Two of the brief's premises for row 24 did not survive
> verification and were **not written down as claims**: slice 2 is not a
> stub (`ManagedBash` is a complete `ShellBackend` impl and
> `config/src/managed.rs` implements the versioned/offline/integrity-checked
> install and lifecycle half), and `ManagedEnvConfig` is not unused (it is
> held in `ShellSettings.managed` and populated from the live runtime).
> Row 24's stale `TODO.md:114–135` cite was corrected to `:125–146`. No
> renumbering; gaps left intentionally. Verification notes for today are
> appended below.

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
| 15 | ADR-60 Deferred item 4 — multi-level disclosure + real-embedder swap proof + scheduler/subscription generalization at high agent counts — **rescoped 2026-09-26 per owner clarification: "beyond 6 agents" is NOT a 6-agent cap** | ADR-60 Revision **v1.2 item 4** (`:150–151`: "Deferred item 4 (scheduler/subscription generalization beyond 6 agents, real-embedder swap, multi-level disclosure) remains deferred per ADR") — **not** `ADR-60:112`, which is only an IPC-overhead cost note ("acceptable at 3–6 agents; revisited only if profiling demands") and was the wrong cite for this row; ADR-60 `:31`/`:98` (v1 *scale* is 3–6 agents, but the scheduler/subscription model is "expressed with N not hardcoded anywhere"); ADR-58:247 (the same three items deferred) | **Doctrine: agents are ADDITIVE with no hard cap.** No `MAX_AGENTS` — or any total-agent limit — constant exists anywhere in `crates/`; the only agent-count-shaped cap in the gate is the **per-agent** `Semaphore` `WriteGate::max_in_flight_per_agent` (`crates/orchestrator/src/gate.rs:501,678,1084`), so nothing rejects an Nth agent and agents compose without a ceiling. The old "when profiling shows need beyond 6 agents" trigger is therefore **not** a re-entry gate: profiling pressure cannot unblock a cap that does not exist. What is genuinely open is the three sub-items themselves: **(a) multi-level disclosure** — today a *single* level, the `retrieve-memory` shortlist clamped to `DISCLOSURE_MAX_CHUNKS = 10` (`consolidation.rs:80`; `supervisor.rs:2118,2129`, `ADR-60:360`), with topic hierarchy, filter-by-relevance/recency, cross-subscriber backpressure signalling and schedule-driven pushes all behind item 4 (`ADR-60:326–329`); **(b) the real-embedder swap proof** — the swap is *real, not pending*, so this is a smaller residual than the old row implied: `ProviderEmbedder` runs `fastembed` (BAAI/bge-small-en-v1.5) on-device and is the production wiring (`memory/src/embedder.rs:42–110`; `orchestrator/src/runtime_runner.rs:1317`), so the consolidation projection no longer depends on the deterministic `feature-hash` placeholder — only the **fallback dependency** remains (retiring the `fastembed` fallback in favour of a provider-hosted embedding API, and the `EmbedderState::Unavailable` degradation path); **(c) scheduler/subscription generalization at high agent counts** — bounded slices, per-agent cursors and disclosure, characterized at 3–6 agents, not at swarm scale. | L |
| 16 | Single-project limit (one active project per process) | ROADMAP:216 (explicitly deferred/incomplete); STATUS.md | Multi-project support requires per-project scoping of process-global services | L |
| 17 | Eval end-to-end benchmark (live runtime over a real benchmark) **+ multi-agent fault-injection containment coverage (former row 21, merged 2026-09-26 as an included sub-part) — both halves landed 2026-09-26** | ROADMAP:241–245 ("a full live end-to-end eval harness over a real benchmark task remains deferred until multi-agent quality and recovery are reliable"); row 21's former source TODO.md:229–235 (injection tests for rate limits, malformed tool calls, missing executables, cancellation races, provider disconnects — the register's old `TODO.md:218–224` cite had drifted onto the *resolved* eval-`#[ignore]` item and is corrected here); ADR-26 recovery boundaries; scenarios at `crates/orchestrator/src/fault_injection.rs:76–92`; live leg at `crates/eval-runner/src/main.rs:430–490` | **Landed in two commits.** `f4bdc4f` added four **crash-window** fault-injection scenarios to the existing in-process (scripted, deterministic) suite, which had 16 prior scenarios but none expressing the durable/child boundary: **C1** specialist child dies mid-dispatch (issued, never settled) → `specialist_child_dies_mid_dispatch_audits_then_recovers`; **C2** provider disconnect at the settle boundary (after a dispatch settled, before the next decision) → `provider_disconnect_at_the_settle_boundary_preserves_settled_work`; **C3** restart from a durable checkpoint row persisted through a **real** session store, continuing the pending dispatch **exactly once** → `restart_with_preserved_checkpoint_continues_the_pending_dispatch`; **C4** cancellation racing a settle, which **re-pends rather than accepts** → `cancellation_racing_a_settle_re_pends_not_accepts`. `5daf2e7` added the live-runtime benchmark leg, gated **twice** so it never runs in CI (the `#[ignore]` attribute *and* a runtime env check): `CONCERTO_LIVE_PROXY` + `CONCERTO_LIVE_PROXY_KEY` are both required and it skips cleanly when unset; `CONCERTO_LIVE_PROXY_MODEL` is optional (default `gpt-4o-mini`) and `CONCERTO_LIVE_EVAL_SUITE` is an optional suite-dir override; run with `cargo test -p concerto-eval-runner live_runtime_benchmark_leg -- --ignored --nocapture`. **Row STAYS OPEN** — what remains is coverage *breadth*, not the mechanism, and the old ROADMAP trigger is now only half-met: the live leg drives **one** suite against **one** live endpoint, so broad live-provider / multi-model coverage is still unproven, and there is **no quarantine mechanism** for live flakiness (a red live run is indistinguishable from a real regression). **Re-entry: widen the live leg beyond a single suite/model, and land a flake-quarantine story for it.** Size stays **L** — both shipped halves were the cheap, narrow half of the original ask. | L |
| 18 | Coordinator restart/resume end-to-end (cross-process continue) — **annotated 2026-09-27, still OPEN and unchanged** | TODO.md:18–25 (Partial; e2e remains); ADR-34 D2 | After checkpoint persistence + evidence-spine resume e2e. **Not advanced by ADR-74:** the delegation guard's "has this run delegated?" test reads the *checkpointed* decision journal (`DecisionKind::DispatchSpecialist` entries) and the agent-axis takeover guard is checkpointed too (`GraphCheckpoint.specialist_takeover_attempted`, `#[serde(default)]`), so both guarantees survive a restore — but that is per-run state restoration, not the cross-process continue this row tracks, which still needs the e2e evidence named in its source cell. | L |
| 22 | ADR-47 message `parts` (canonical parts replace flat string content) | ADR-47:85–98 (deferred; flat model retained); ARCHITECTURE-V2.md:323 | **GATED DOCTRINE 2026-09-25** — stays OPEN, but gated on ADR-47's own reopen condition (:78–93): reopen **only** when a consumer proposes/needs richer message content the flat shape cannot express (multi-part tool bodies, image/file `File` parts, structured `Thinking`/`RedactedThinking` on Anthropic/Gemini paths, or structured data smuggled into `content: String`). Migration preconditions recorded 2026-09-25 — all three required before any parts work: **(a)** a named consumer need (ADR-47:80–81); **(b)** a parts-joined-text equivalence test — joining `parts` back to flat text must reproduce today's bytes, and Fix 2 strictness must not loosen (proxy-tool-call-fix.md:5,60 — content-embedded strict path buffers content to turn end); **(c)** a checkpoint/`state_json` migration plan for old rows (`orchestration_checkpoints.state_json` is `TEXT NOT NULL`, migration 019:10; `crates/sessions/src/lib.rs:150`), shipped additively with `serde(default)` + legacy-column fallback in one dedicated window, never as a side effect of an unrelated feature (ADR-47:62–65,109–110; ARCHITECTURE-V2.md §10). Verified migration-safe meanwhile: the Mimo/loose-tier tool mods operate on `ToolDefinition`/`ToolCall` **values**, not message shape — `schema_loose.rs:113` `adapt_tool_definitions(&mut [ToolDefinition])`, `:132` `unflatten_tool_arguments(&mut serde_json::Value)`, `google.rs:55` `adapt_tools_for` mutates `request.tools` — so they neither block nor depend on a parts migration. | L |
| 23 | ADR-53 per-token streaming through WASM (heartbeat landed) — **stays OPEN and deferred; trigger sharpened 2026-09-27 to a named-consumer precondition, with the cost stated** | ADR-53 `docs/adrs/ADR-53-dialect-plugins-and-plugin-heartbeat.md:137–139` ("**No per-token streaming through WASM in this ADR** — streaming is deferred (§Consequences)"), restated as a Consequences deferral at `:171–172`; the deliberate no-ABI-change stance at `:48–50`, `:117–118` and `:170` ("no breaking change, no `abi_version` bump"); the liveness gap it was opened for at `:39–41`; heartbeat landed per ADR-53 §4 (`:128`) | **Precondition: build ONLY when a plugin kind actually generates tokens incrementally.** The ABI is single-shot *by construction*, not by omission: `guest_abi.rs:7` `HOST_ABI_VERSION = 1`; the return value is a `(ptr, len)` pair packed into one `i64` (`:9–16` `RESULT_ERROR`/`pack_ptr_len`/`unpack_ptr_len`); the only three exports are `call_provider`/`call_adapter`/`call_dialect` (`:31–42`, the dialect signature documented as 6 `i32` params → `i64` at `:39`); and the host resolves the export as `get_typed_func::<(i32, i32, i32, i32, i32, i32), i64>` and makes **one** awaited call (`active_plugin.rs:149–166`). **No streaming export exists, and none can be added without a version bump.** Today the host awaits one `call_provider("complete", …)` future and maps the whole JSON result to **ONE** `CompletionChunk` (`provider_host.rs:146–149` awaited call → `:151`; `chunk_from_result` at `:155–163` reads `content`/`finish_reason` and builds a single chunk; the same single call inside the heartbeat task at `:195–199`). **The liveness case that motivated the deferral is already covered** by the landed heartbeat, which is why there is no urgency behind this row: `heartbeat_stream` (`:175–233`) interleaves `CompletionChunk::keepalive()` (`:218`) on a cadence while the call is in flight, wired from the manifest's `heartbeat_interval_secs` (`manager.rs:698–725`, `with_heartbeat`/`with_dialect`), with a no-heartbeat single-chunk fallback (`:344–354`, `futures::stream::once`). **There is NO consumer that would benefit today:** `PluginBackedProvider::stream_completion` (`provider_host.rs:287–288`) does return a real stream, collected by `PluginManager::collect_providers` (`manager.rs:744–746`) and consumed in the runtime wiring (`orchestrator/src/runtime_runner.rs:1688`) — but that stream is *keepalive chunks plus exactly one content chunk*, so a plugin that emits tokens incrementally over minutes would still deliver all of its output in a single terminal chunk. **Cost, stated so this is not scheduled as a generic "streaming" improvement:** it requires a new ABI export (or a host-fn chunk sink the guest calls repeatedly), **plus an ABI version bump** — and ADR-53 deliberately did neither (`:48–50`, `:117–118`, `:170`; §4 is titled "Plugin heartbeat — keepalive, **no streaming ABI**"). It is therefore a breaking guest-ABI change with a plugin-compatibility story, not a provider-host refactor. **Re-entry: a named plugin kind that generates tokens incrementally** (the motivating case being a plugin that wraps a slow local model); until one exists, the keepalive-plus-one-chunk stream is the correct v1. | M |
| 24 | Shell Phases C–F + ADR-28 profile slices — **rescoped 2026-09-27: Phases A/B and profile slices 1–2 have LANDED; what remains is the C verbs plus D–F, and the row is split in two** (size **L → M**) | ADR-29 (`docs/adrs/ADR-29.md:3`, Accepted) is the runtime/policy execution decision. Repo plan `docs/custom-ai-shell-plan.md:125–228` = **Phase A–F** (A at `:125` already marked "**Implemented**"; B at `:143` "**Implemented at the library boundary**"; C `:184`, D `:199`, E `:209`, F `:219–228`). Research plan `docs/research/ai-native-shell-implementation-plan.md:11–510` = **Phase 0–5** (0 at `:11`, 5 at `:458`) — a *different* numbering for overlapping work. TODO.md:125–146 (both items; **the row's old `TODO.md:114–135` cite had drifted** onto other rows and is corrected here); ROADMAP.md:183–187. Profile slices come from ADR-28, which is **archived and superseded** by ADR-30 (shell selection) + ADR-29 (`docs/adrs/archive/ADR-28.md:3–10`, "not active guidance") — so ADR-28 is historical rationale here, not live authority, and the old row's "superseded in part by ADR-30 for shell selection only" understated it | **PREREQUISITE — reconcile the phase numbering before starting anything, as `TODO.md:136–137` explicitly instructs** ("fresh phase plan starting with the `ToolManifest` schema system (reconcile its phase numbering with `custom-ai-shell-plan.md` before starting)"). Two live plans number the same work incompatibly: the repo plan uses **A–F**, the research plan uses **0–5**. Settle which numbering governs (and whether the research plan supersedes, merges into, or is discarded alongside) before any "Phase C" work is named, or the two get silently merged into a scope neither plan approved. **LANDED — recorded so this row stops implying the shell runtime is unbuilt.** *Phase A (read-only builtins + runtime):* `crates/shell/src/builtins.rs:20–27` (`standard_commands` → `help`/`project-info`/`ls-tree`/`last`), `runtime.rs:43–58` (`ShellRuntime::standard`, every command registered read-only), `parser.rs:33–60` (`parse_command_line` — quoted tokenizing, no shell expansion), `model.rs:24–31` (`CommandStatus::permits_continuation`/`is_success`). *Phase B (policy-gated external exec):* `shell/src/execution.rs:35–80` (`PolicyExecutionAdapter::execute` funnels through `ToolExecutor::execute("shell", …)`, so no command spawns a process itself) and `:73–83` (`external_commands` → `run`/`shell-run`/`shell-profiles`); `profile.rs:13` (`ShellProfileCatalog` — the shell consumes canonical config instead of keeping a second selector, per ADR-30); `config/src/shell.rs:58` `ShellProfileConfig`. *Quoting / argv-direct:* `tools/src/shell.rs:349–441` (`windows_arg_needs_quoting`, `shell_quote_windows`, `shell_quote_posix`, `shell_quote`) with `validate_cmd_args:510` rejecting arguments cmd.exe would `%`-expand, and `:614–640` `legacy_shell_plan` returning `ShellPlan::Direct` argv-direct at `:623–627` (bypass_shell, and Windows where no shell semantics are needed); `tools/src/shell_backend.rs:57–59` `command_args`. *ADR-55 containment:* `tools/src/containment.rs:1–30` (argv/cwd confinement — out-of-root `cd`/`pushd`, path-like args on mutation verbs, `xargs` pipeline laundering of a read-exempt argument, redirect writes). *CPU budget (row 45's portable layer):* `tools/src/shell.rs:65–69` `resolve_cpu_budget` (config wins, including `Some(0)` = off, else `CONCERTO_SHELL_CPU_BUDGET_SECS`), `:875–880`, `:1231–1303` (`plan_takes_cpu_backstop`, the soft-only `cpu_limit_prelude` `ulimit -S -t`, and `spawn_plan` → `ProcessHandle::run_limited`), plus `tools/src/cpu_accounting.rs`. *OS identity card:* `orchestrator/src/prompts.rs:175–180` (always appended, never empty, never errors) and `:375` `environment_card` (profile facts, else OS facts + `detect_os_default_shell`; no process spawned). *Bounded shell repair:* `orchestrator/src/shell_repair.rs` (its own doc calls it a "Phase C subset"); `MAX_SHELL_REPAIR_ATTEMPTS = 5` at `:36`, char caps at `:40–42`, and policy outcomes are **never** repaired (a denial is not coached around) with cancellation spending no turn. *Slice 1 (test profile + availability):* `config/src/shell.rs:235–242` `availability()`, documented "ADR-28 Slice 1"; `tools/src/shell_backend.rs:45–46,69–80,117–124` `check_available`; the desktop Test-profile action is live (`views/settings/shell.rs:164–183` messages, `:403` button; state field labelled "ADR-28 Slice 1" at `state.rs:144–146`). *Slice 2 (Managed Bash PoC) — also landed, and it is **not** a stub:* `config/src/managed.rs` (347 lines) does the versioned, offline, integrity-checked install ADR-28 asked for — versioned dir under `<data>/concerto/managed-bash/<version>/bash` (`:114–119`, `install_from` `:152–194`), blake3 integrity + `verify` (`:205–232`), manifest export/import (`:235–249`), `remove` (`:197–202`), bounded 2 s version probe (`:251–262`); `tools/src/shell_backend.rs:88–125` `ManagedBash` is a **complete** `ShellBackend` impl (not a placeholder) resolving through `ManagedRuntimeManager::auto_detect`; `ManagedEnvConfig` (`config/src/shell.rs:352`) is **not** unused — it is held at `:376` (`ShellSettings.managed`), re-exported at `lib.rs:67`, and populated from the live runtime at `desktop/…/views/settings/state.rs:754–762`; the install/remove/verify/export/import UI is wired (`views/settings/helpers.rs:39,55,95,109`; `settings/shell.rs:187–223` messages, `:509–544` buttons). **REMAINING GAP, stated precisely.** **(a) Phase C's `explain`/`debug`/`optimize` commands are ABSENT** — no such command exists; the intelligence is *prompt content plus repair*, not invocable commands (the plan's own "Shipped subset (2026-09-07)" note, `custom-ai-shell-plan.md:193–197`). A search of `crates/shell/src` for those names returns only `#[derive(Debug)]` attributes and one unrelated doc comment. **(b) Phase D deterministic workflows are ABSENT** — there is no workflow AST: `WorkflowAst` has **zero** matches under `crates/`, and `shell/src/model.rs:100` `Workflow` is a variant of the `CommandSource` enum (`:93–103`), i.e. a provenance tag with no AST behind it. Consequently no cycle/variable/result-type/effect validation, no checkpoints or resumable execution, no bounded retry/fallback/approval/parallel nodes, no execution trace. **(c) Phase E's shell tool/plugin ABI is ABSENT in the shell crate** — `shell/src/registry.rs:38–68` is a static in-process register (`register`/`get`/`specs` over a `RwLock<BTreeMap<..>>`), not an ABI; the real WASM ABI lives in `crates/plugins` (`guest_abi.rs`, `active_plugin.rs::call_json_export`) and **nothing bridges the shell command spec/result envelope to it**, so no shell command can be shipped as a plugin and no custom tool can add shell schemas, renderers, or effect declarations. **(d) Phase F measured self-improvement is ABSENT** — no history mining, no alias/workflow-rewrite suggestion, no validate-before-promotion gate, no provenance/comparison evidence; `shell/src/history.rs` is a bounded in-memory `VecDeque<CommandResult>` that feeds the `last` meta-command and is not a learning loop. *Slice 2 residual (small, specific):* two ADR-28 requirements are unmet — **controlled `PATH`** (`tools/src/shell_backend.rs:109–115` `effective_env` just clones the base env instead of constraining `PATH`) and **PTY-backed terminal** (a `\bpty\b` search under `crates/` returns nothing; there is no PTY dependency anywhere in the workspace). Distribution of a vetted Bash binary is explicitly the later licensing-gated slice (`config/src/managed.rs:10–13`, `views/settings/helpers.rs:36–37`). *Slice 3 (cross-platform packaging):* **not started** — consistent with a deliberately Linux-first PoC, and it is licensing/provenance-gated by ADR-28's own framing. **THE WORK IS SPLIT IN TWO, and the re-entry follows the split.** **(1) The C verbs are one self-contained piece** — prompt/LLM work that reuses the existing `ShellCommand` trait, registry and `CommandResult` envelope, with no new state machine, no persistence, and no new ABI; `explain`/`debug`/`optimize` can and should ship on their own. **(2) D–F are a separate and larger design decision** — a workflow AST is a new persistence + execution model (versioning, validation, checkpoint/resume, node scheduling), and Phase E additionally needs a shell↔WASM ABI decision; neither can ride along on an "add three commands" change. **Re-entry: (0) reconcile the phase numbering, then (1) take the C verbs as its own change, and (2) take D–F only as its own decision** — ADR-first per the repo ADR rule, since a workflow AST is exactly the kind of architectural decision that needs a record before dependent code. | M |
| 25 | Code editor integration (external editor + open-in-editor + diffs) | TODO.md:157; ROADMAP | Post-1.0 | M |
| 27 | Pricing/metadata freshness feeds the VISUAL SPEND TRACKER ONLY (rescoped 2026-09-25: display-only, spend tracker) | TODO.md:201; STATUS.md tracked follow-ups | When the display-only pricing/metadata freshness flow lands. Stale prices make usage dollars wrong; routing must never see prices. Non-goal: the coordinator must never know or care about cheap vs expensive models; no cost-based routing, no model switching on price, and no coordinator coupling of any kind. | M |
| 28 | ADR-58 P5/P6 + TOML diff + canvas DAG editor + partitioning | ADR-58:238–247; STATUS.md ~328–355 (deferred P5 Studio, P6 items, TOML diff, run-one-stage, multi-executor partitioning) | Post-P6 roadmap phase (explicit ADR-58 deferral items) | L |
| 29 | ADR-43 server mode / SSE / marketplace / persistent desktop state / TOML secrets | ADR-43 §3 v1 note (MCP server mode, SSE transport, marketplace/registry, keyring-backed tokens, persistent desktop state deferred); docs/skills.md:202 | When the API surface / web UI materializes | L |
| 30 | Plugin hot-reload / remote / registry / marketplace | TODO.md:108–110 | Requires community/registry story (ADR-21, ADR-43 deferred) | L |
| 31 | L1/STM/PersonaMem memory items (typed extraction, scene memory, persona eval) | TODO.md:58–99 (L1 70–80, STM 65–70, heuristics 89–95, PersonaMem 96–99) | Post-memory relayout (ADR-63/64) | M |
| 32 | Certified evolution (profile-guided CI, safety gates) | TODO.md:286–293; ROADMAP:189–193 — `[dangling-cite]` the referenced `docs/research/certified-universal-evolution.md` (cited at TODO.md:287, ROADMAP:190) is missing on disk | When certified-evolution research plan lands (restore/rewrite the research doc or re-scope) | L |
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
| 45 | Shell-command CPU rate limiting (threat gap #6) — **portable layer shipped 2026-09-26 (row STAYS OPEN: Windows + cgroup v2 residual)** | security-threat-model.md §6 :346–351 (~12 h; cgroup/ulimit integration). **Landed 2026-09-26 in `13cba1c`:** Linux `/proc` watchdog sampling aggregate process-group user+sys CPU (`tools::cpu_accounting`, `CpuBudget` + `ProcessHandle::run_limited`) that SIGKILLs the group and returns an explicit budget error; POSIX `ulimit -S -t N` soft backstop prelude on wrapped plans (soft-only: `SIGXCPU` is recognisable, a hard `SIGKILL` is not); budgets **default off** and the existing wall-clock timeout is unchanged; `CONCERTO_SHELL_CPU_BUDGET_SECS` escape hatch (config wins, unparsable/0 = off). Composes with row 36 — inside a container the `ulimit` prelude rides along and the runtime is not asked to re-impose a CPU ceiling. **Residual: no CPU enforcement on Windows** (no safe Job Object path; row 36's Windows decision governs this too) and **cgroup v2 is not implemented** (needs privileged cgroupfs writes / `unsafe`) | A superseding ADR for the Windows Job Object path (row 36's options (a)/(b)/(c)) and/or an explicit cgroup-v2 decision. **Tracked sub-item:** the CPU budget has **no TOML config field yet** — `ShellConfig::cpu_budget_secs` (`crates/tools/src/shell.rs:220`) is `None` in every production call site (`with_profile` / `allow_all` / `new`), so the env var is the only operator knob; add a config key when the budget is wired for operators. | M |
| 46 | Plugin network egress filtering (threat gap #7) | security-threat-model.md §6 :355–360 (~8 h; network capability allowlist) | When a security milestone is scheduled | M |
| 48 | Codebase-world-class Phase 3 criterion benchmarks + CI benchmark gate | world-class-plan.md:186–208 (Phase 3 group); TODO.md:153–154; part of the Phases 1–5 group (TODO.md:148–156) | When the Phase 3 benchmark milestone is scheduled (criterion suite + CI gate) | M |

## Closed appendix (each line verified with a repo source)

1. GitHub Models — retired provider (STATUS:16–18, README, ROADMAP:214, ADR-52).
2. Weighted-sum hybrid ranking — superseded by RRF (ADR-22; ADR-63:7–10).
3. WRR (weighted round-robin) fairness — superseded by structural per-agent in-flight isolation (ADR-60 §D1/D2 note).
4. Shell argv/cwd containment (ADR-55) — landed (`crates/tools/src/containment.rs`; `shell.rs:4,668`; STATUS.md:72 asserts 21d4d3e landing).
5. route() deletion / routing carcass — landed (commit `cbb09b0`; ADR-71:19–22; TODO.md:163–166). **Annotated 2026-09-27, unchanged and still closed:** ADR-74 (`5a22405`/`269c344`) is the *prompt-level* counterpart of this line, not a reversal of it — that commit deleted the deterministic keyword router in code, this one deleted the hardcoded role→capability reasoning and the "you are a full agent" self-execution license from the coordinator's dispatch prompt, leaving the runtime roster as the only source of who can be called. Nothing in ADR-74 consults `route()` or reintroduces routing as control flow, and `crates/core/src/intent.rs` is untouched by all four delegation-doctrine commits.
6. ReviewResume module removal — landed (CHANGELOG:33–34; commit `cbb09b0`).
7. Threat sanitizer + hash pinning (threat gaps #2, #8) — closed (security-threat-model.md, 2026-09-19 / 2026-09-24).
8. Proxy tool-call parsing Fixes 1–3 — landed (ROADMAP:168; missing-providers.md:51; flat/content-embedded legacy residual tracked in TODO.md:190–195).
9. Tier-1 + Tier-2 providers — implemented 2026-09-24 (missing-providers.md: 22 provider ids registered, :5–33; "Tier 1 & 2 DONE", :53–57; ROADMAP:46–52 "Provider reach (2026-09-24)" — config-first factory covers all 22).
10. Eval `#[ignore]` un-ignore — resolved 2026-09-24 (TODO.md:207–217; TESTING.md).
11. Agent-loop wildcard-panic guard — resolved 2026-09-24 (TODO.md:225–236).
12. FTS BM25 `rank()` wired into retrieval — stale-verified, closed 2026-09-24 (chunk FTS always BM25 rank-ordered: `fts.rs:148` `ORDER BY rank`, RRF fusion `rag.rs:412-459`; stored 1.0 inert by construction `sync.rs:61,130`; proven by tests `fts.rs:404-450,458-498`).
13. Memory encryption for sensitive data (threat gap #9) — **closed 2026-09-26 per owner decision**, with the residual recorded rather than dropped. The safe-reachable scope shipped in `1c38c9f`: `concerto_core::SecretString` (zero-on-drop `zeroize` wipe of the backing buffer, `Debug`/`Display` render a redaction marker, `expose()` returns a borrow so call sites stop cloning keys, credential store and `ProviderConfig` resolve through `get_secret`, `PendingConfig`/`ProviderRequest` no longer derive `Debug` over a raw key, and the Google connector scrubs its `?key=` URL out of transport errors) — module docs at `crates/core/src/secret.rs`, tests cover the drop wipe, redaction (direct / embedded / collection / credential-store / provider-request / wizard-config) and the Google diagnostic scrub. **Residual, unimplemented by design rather than by oversight: `mlock`/`madvise` RAM pinning.** No safe abstraction for it exists in the dependency graph — `nix` (present only as a `signal` + `feature` cfg gate in `crates/tools`/`crates/plugins`) exposes it as a raw-pointer `unsafe fn`, and the workspace hard-denies `unsafe_code`, so the RAM-pinning half of threat gap #9 stays open work. Closure is therefore scoped: the *safe-reachable* mitigation is closed, the pinning half is not.
14. Review/validation escalation events (threat: informational evidence; no terminal conversion) — **row #33 cut 2026-09-26 per owner decision; closed 2026-09-26 in `9385ecf`.** The row tracked two `EventKind` variants, `ReviewCycleEscalated` and `ValidationEscalated`, cited at `core/src/event.rs:386,394` as "event-bearing only". Both were **dead weight**: neither had a publish site, and the live escalation path is verdict-driven and event-free — `progress::CycleVerdict::Escalate` stops the coordinator's decision loop (`coordinator.rs:10817`) and surfaces an informational `AgentThought`. The commit removed the variants plus every now-dead reference in the same sweep (transcript rendering arms, desktop activity-translation arms, fault-injection test-observer arms/fields — `crates/core/src/event.rs`, `crates/core/src/transcript.rs`, `crates/desktop/src/runtime.rs`, `crates/orchestrator/src/fault_injection.rs`), and left an in-place comment at `event.rs:394–397` recording *why* no escalation event kind is part of the live contract, so the vocabulary does not drift back. **Not a miscitation and not a cancellation:** the deferral was real once, the implementation it named was simply never written because the design moved to a verdict, and the honest resolution is to delete the placeholder rather than to keep a task that no longer has a consumer. `EventKind` is `#[non_exhaustive]`, so external exhaustive matches already carried a wildcard and the live `Escalate` path is untouched. The `#33` numbering gap is left intentionally (no renumbering), matching the row 10 / 12 / 14 / 19 / 21 / 26 precedent.
15. Audit-log at-rest encryption + retention (threat gap #5; former row 19, merged 2026-09-25) — **row #44 closed 2026-09-26; shipped in `7351128` with ADR-73 (Accepted).** Both halves the row was merged around landed together, which is what the 2026-09-25 merge rationale predicted: `crates/sessions/src/at_rest.rs` (SQLCipher via `bundled-sqlcipher` feature unification on `libsqlite3-sys`, so the persistence layer stays plain sqlx and the key travels as `PRAGMA key`; 32-byte hex key resolved `CONCERTO_AUDIT_DB_ENCRYPTION_KEY` → OS keychain account `audit/db_encryption_key` → generate-once-and-store, failing closed with **no plaintext fallback**; a `<db>.sqlcipher` marker so the ADR-54 plaintext-header quarantine heuristic never misclassifies a healthy encrypted store, and a **two-phase** `<db>.migrating` marker so a crash mid-swap can never leave the final marker beside a plaintext database; `sqlcipher_export` into `<db>.enc-tmp`, key-verified, then two renames with `<db>.old` swept after the first successful keyed open; statement logging disabled on keyed connections so the key pragma never reaches a log sink) and `crates/sessions/src/audit_retention.rs` (archive-then-prune into an encrypted `audit-archive.db`; a failed archive write **aborts** the prune, so rows are never deleted un-archived), with `crates/sessions/migrations/033_audit_created_at_index.sql` and the `[audit]` config section (`crates/config/src/schema.rs:840–881`). ADR-73 (`docs/adrs/ADR-73-audit-encryption-and-retention.md`) answers the question ADR-40:47 deliberately left open, amending ADR-40 §Decision item 3 only and affirming items 1, 2 and 4. **Recorded honestly, the *posture* is opt-in and the *capability* is complete:** `encrypt_at_rest` defaults to `false` and `retention_days` to `None`, so a default install is still plaintext and still grow-only — a deliberate, ADR-73-documented choice (`:151–153`, `:238`; "nothing is ever deleted or encrypted behind the user's back"), not a gap in the delivered work. `docs/security-threat-model.md` §6 gap #5 is still worded as open and is **not** reconciled by that commit; tracked as a docs follow-up, not as a new register row.
16. ADR-60 S5 agent-process slice — mock-only provider + `DenyAllApprovalSink` — **row #49 closed 2026-09-26; landed in two commits.** The row tracked the two stubs the child carried: `CONCERTO_PROVIDER` accepted only `"mock"` (`agent_process.rs:137–152`) and interactive approvals were dropped as a `DenyAllApprovalSink` (`:155`, `:292–336`), so a supervised run could never talk to a real model and could never prompt for approval. `d62ebac` (real provider injection) replaced the stub with a new `crates/orchestrator/src/agent_process_config.rs`: the child rebuilds its provider through `ProviderFactory::config_for_model` + `ProviderFactory::build` against a `CredentialStore`, i.e. the same keyring path the parent uses, and **never silently substitutes a mock** — no configured provider is the named error `NoProviderConfigured` ("refusing to fall back to mock"), an unreadable credential fails closed the same way, and the scripted mock is now an **explicit opt-in** (`MOCK_PROVIDER = "mock"`, `selects_mock()`; unset, empty or any other value does *not* select it). `7859a0a` (approval IPC) added the bridge over the existing newline-delimited stdio transport — new `IpcMethod::ApprovalRequest` / `ApprovalResolved` and an `ApprovalActionWire` projection (`ipc.rs:69–90`, `:374–390`), a child-side `ApprovalProxySink` that forwards each request to the supervisor (`bin/agent_process.rs:163–168`), and supervisor-side handlers that route to the **same `Arc<dyn ApprovalSink>` the in-process coordinator/single-agent paths use** (`SupervisorServices::approval_sink`, `supervisor.rs:764`), so supervised approvals light up the existing UI rather than a parallel system; the returned decision is applied child-side and the authoritative `ApprovalRequested`/`ApprovalResolved` audit events are published under the request's real session + correlation identity. **Fail-closed is preserved end to end**, and that was the point of routing through the same sink: no sink configured, a cancelled run (teardown), a malformed/unattributable session identity, an unknown wire decision label, a transport error, or a closed channel **all answer `deny`** (`supervisor.rs:1933–2010`; `ipc.rs:415–417` — an unrecognized `#[non_exhaustive]` label is "a protocol violation the caller answers with a deny"; the child's own default is unchanged). **Residual, recorded rather than dropped: the approval wire carries `session_id` + `correlation_id` but no embedded `run_id`.** The envelope therefore cannot attribute an approval to a specific run, because `SupervisorServices` holds no run-scope field either; closing it needs either a `core` event-contract change (widening the `ApprovalRequested`/`ApprovalResolved` shape) or run-scope plumbing through `SupervisorServices`. Neither rides along with a docs change. The two items the row's re-entry condition named — real provider injection, and approvals surfaced through the supervisor rather than always denied — are both delivered; the row is closed, not merged into another, and the `#49` numbering gap is left intentionally (no renumbering).

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

## Verification notes (2026-09-26, group-F pass)

Group-F outcomes, executed 2026-09-26 against HEAD `a2975b6`. Every commit cited
is reachable from this checkout and every claim was checked against code or an
ADR, not against a commit message alone. No builds were run for this pass.

- **Row 49 closed** (see Closed #16). Both halves verified in the tree.
  Provider injection: `crates/orchestrator/src/agent_process_config.rs:23–26`
  documents the `ProviderFactory` + keyring resolution and the no-mock-fallback
  rule; `:50–51` defines `MOCK_PROVIDER = "mock"` as the explicit opt-in, and
  its own test `mock_is_selected_only_by_the_explicit_opt_in` asserts that unset,
  empty and a real provider id all decline the mock. Approval bridge: new
  `ApprovalProxySink` at `bin/agent_process.rs:163–168`; `SupervisorServices::
  approval_sink` at `supervisor.rs:764`; the handler's own doc comment at
  `:1933–1935` states the supervisor holds the **same** `ApprovalSink` the
  in-process paths use, and the fail-closed list at `:1937–1940` is exactly the
  deny set the owner named. The `run_id` residual was **confirmed, not assumed**:
  `ApprovalActionWire` (`ipc.rs:382–390`) carries `tool_name`, `input`,
  `session_id` and `correlation_id` and no run identifier, and
  `SupervisorServices` (`supervisor.rs:740–771`) has no run-scope field — so the
  gap is real on both the wire and the services side, and closing it needs a
  `core` event-contract change or run-scope plumbing, neither of which is a docs
  change. *SHA correction:* the approval commit is `7859a0a`, not `7859a0c` as
  first reported; `7859a0c` does not resolve in this repository. The register
  records the real SHA.
- **Row 21 merged into row 17, #21 deleted.** The register cited `TODO.md:218–224`
  for row 21, but that line range now holds the **resolved** eval-`#[ignore]`
  item (TODO.md:218–228); the fault-injection item actually sits at
  `TODO.md:229–235`, and the merge corrected the citation rather than carrying a
  drifted one forward. The four scenarios are named in the module doc table at
  `fault_injection.rs:78–92` (C1–C4) and each test exists: `:1738`, `:1779`,
  `:1832`, `:1961` — the C3 test is the durable-row path (checkpoint persisted
  through a real session store) and the C4 test name confirms the re-pend
  semantics. The live leg is gated twice: the `#[ignore]` attribute *and* a
  runtime env check at `eval-runner/src/main.rs:434–440`, with the documented
  invocation and the full env contract at `:469–477`; both required vars unset
  yields a clean skip (`:480`), so it cannot reach CI. Row 17 stays OPEN on
  breadth, and the reason is stated in the row rather than left implicit.
- **Row 33 cut** (see Closed #14). Verified that the two variants are gone —
  a repo-wide search for `ReviewEscalat` / `ValidationEscalat` across
  `crates/**/*.rs` returns **zero** matches, consistent with `9385ecf` sweeping
  the transcript, desktop and test-observer arms in the same commit — and that
  the live escalation path is genuinely `CycleVerdict::Escalate` at
  `coordinator.rs:10817`. The in-place comment left at `event.rs:394–397` is
  the durable half of this closure: it explains the absence, so the vocabulary
  does not drift back.
- **Row 44 closed** (see Closed #15). Both halves verified present:
  `at_rest.rs` and `audit_retention.rs` exist with the module docs quoted, plus
  `migrations/033_audit_created_at_index.sql` and the `[audit]` config section at
  `config/src/schema.rs:840–881`; ADR-73 is **Accepted** and names `7351128` as
  its implementing commit. Recorded honestly rather than glossed: `encrypt_at_rest`
  defaults to `false` and `retention_days` to `None`, so the default posture is
  still plaintext and grow-only. That is an ADR-73-documented decision, not an
  unbuilt half — but it is why the Closed line says "capability complete, posture
  opt-in" rather than claiming the threat gap is closed outright.
- **Row 15 rescoped, kept OPEN at L.** The owner clarification was checked
  against the tree before rewriting, and the code supports it: a repo-wide search
  for `MAX_AGENTS` / `max_agents` across `crates/**/*.rs` returns **zero**
  matches, while the gate's only agent-count cap is the *per-agent* semaphore
  `WriteGate::max_in_flight_per_agent` (`gate.rs:501,678,1084`). So the old
  "when profiling shows need beyond 6 agents" trigger was not merely stale wording
  — it pointed at a cap that does not exist, which is why the row's re-entry cell
  now states the additive doctrine and lists the three real sub-items. Citation
  corrected from `ADR-60:112` to Revision **v1.2 item 4** (`:150–151`); `ADR-60:112`
  was confirmed to be the IPC-overhead cost note under "Negative / costs", so the
  old cite could not support a deferral claim. The real-embedder sub-item was
  re-scoped down on the same evidence: `ProviderEmbedder` really is
  `fastembed`-backed (BAAI/bge-small-en-v1.5, lazy init behind a mutex,
  `spawn_blocking` for the sync ONNX call — `memory/src/embedder.rs:42–110`) and is
  the production wiring at `runtime_runner.rs:1317`, so only the fallback
  dependency is outstanding, not the swap itself. `DISCLOSURE_MAX_CHUNKS = 10` and
  the single-level clamp confirmed at `consolidation.rs:80` and
  `supervisor.rs:2118,2129`.
- **Scope of this pass:** rows 15, 17, 21, 33, 44 and 49 only. All other open
  rows and the existing Closed #1–#13 lines are unchanged. No renumbering; the
  #21, #33, #44 and #49 gaps are left intentionally, matching the row 10 / 12 /
  14 / 19 / 26 precedent. **Docs only — no source edits, no builds, no test runs
  and no commit** in this pass; the tree is left for the orchestrator to commit.
- **Follow-ups flagged, not actioned here** (each would touch rows outside this
  pass's scope):
  - `docs/security-threat-model.md` §6 gap #5 is still worded as fully open and
    was **not** reconciled by `7351128`; gap #6 (row 45, partially shipped) and
    gap #1 (row 36, container path shipped / Windows declined) remain in the
    state the 2026-09-26 note above already flagged. One threat-model pass could
    close out rows 36, 44 and 45 at once.
  - `docs/TODO.md:229–235` still reads "Fault-injection tests for multi-agent
    containment. **Not started**" — now stale in the same way row 21's citation
    had drifted. The four scenarios landed in `f4bdc4f`.
  - `ROADMAP.md:241–245` still describes the live end-to-end eval harness as
    fully deferred; `5daf2e7` shipped a first env-gated leg.

## Verification notes (2026-09-27, delegation-doctrine pass)

The delegation doctrine, executed 2026-09-27 against HEAD `115f85c` on
`fix/coordinator-error-invariant`. All four commits are reachable from this
checkout; every claim below was checked in the tree, not read off a commit
message. Recorded in full in
`docs/adrs/ADR-74-delegation-doctrine-and-ladder-hold.md` (Status: Accepted —
implemented). **Docs only in this pass — no source edits, no builds, no test
runs, no commit**; the tree is left for the orchestrator to commit.

- **Scope of this pass: zero row-status changes.** Four implementation commits
  landed, and **not one of them implemented a registered deferral** — the
  register has no delegation, coordinator-supremacy, roster-customization, or
  provider-ladder-hold row to close. So nothing is opened, closed, cut, or
  merged here, the Closed appendix is unchanged, and no row is renumbered. The
  two annotations below are deliberately one-clause each, added so the register
  cannot be misread against ADR-74.
- **Closed #5 annotated, still closed.** `route()` deletion / routing-carcass
  removal is the *code-level* half of what ADR-74 does at the *prompt level*.
  Verified in the tree: no `fn route(` definition exists anywhere under
  `crates/` — the remaining `route(` hits are axum `Router::route` HTTP route
  registrations in `api-server`/`observability` plus benchmark fixtures, and the
  sole reference to the deleted function is one historical mention in the
  `crates/core/src/intent.rs:6` module doc (a source comment, out of scope for a
  docs-only pass). `crates/core/src/intent.rs` keeps only the vocabulary types
  per ADR-71's reconciliation note, and all four delegation-doctrine commits
  leave it untouched — `git show --stat` for `5a22405`/`269c344` lists only
  `crates/orchestrator/src/coordinator.rs` and `crates/core/src/event.rs`. The
  prompt-side change is the deletion of the "you are a full agent" license and
  the addition of the "Selecting a specialist" section, whose own text says the
  roster "is data, this prompt hardcodes no roles". Recorded so a future reader
  does not mistake ADR-74 for routing logic returning by another name.
- **Row 18 annotated, still OPEN at L.** The delegation guard derives
  "has this run delegated?" from the decision journal rather than a live counter
  (`has_recorded_delegation_attempt` scans `self.decision_journal.entries()` for
  `DecisionKind::DispatchSpecialist`, `coordinator.rs:2871`), and the agent-axis
  takeover guard rides in the checkpoint (`GraphCheckpoint.specialist_takeover_attempted`,
  `#[serde(default)]` so an old checkpoint restores empty rather than failing to
  load). Both therefore survive a resume — but that is per-run state
  restoration, **not** the cross-process continue this row tracks, whose
  re-entry condition (checkpoint persistence + evidence-spine resume e2e) is
  untouched by any of the four commits. Annotated so the two are not conflated.
- **The doctrine itself, as recorded.** (a) Delegation is the coordinator
  default and the blanket self-execution license is deleted; self-execution is
  permitted only on roster exhaustion (empty / disabled-or-unavailable /
  delegation-attempted-and-failed), and the exhaustion condition replaces ADR-35
  §8's *stage-absence* trigger — the guard asks only whether the roster is empty
  and never inspects which stage is staffed, so operator-chosen rosters stay
  fully customizable. (b) The guard refuses a coordinator mutating tool call
  (`write`/`shell`/`git`, and `filesystem` `write`/`delete`/`move`/`copy`; read-
  only calls untouched) with the named, policy-visible verdict
  `"Denied: delegation-required"` and a structured `delegation_required` result
  back to the model; lawful self-execution records
  `EventKind::CoordinatorSelfImplementing { .. reason }` with
  `roster-empty-or-disabled` or `delegation-attempted`, and a *refused* attempt
  records no such event. (c) The agent axis runs **before** the
  provider-escalation tiers, and coverage is configuration data —
  `CustomAgentConfig.can_cover: Vec<AgentStage>`, `#[serde(default)]`, effective
  coverage = own stage ∪ `can_cover`, a same-stage peer preferred, non-covering
  agents never selected. It **reverses** ADR-45's "never reassign a same-stage
  peer" invariant: `ladder_hard_failure_takes_over_to_same_stage_peer` replaced
  `ladder_hard_failure_never_reassigns_stages`, so the reversal is visible in
  the diff rather than smuggled. (d) A throttled planning rung with a known
  `Retry-After` is **held** and retried on the same provider — `MAX_PLANNING_HOLD`
  30 s, `MAX_PLANNING_HOLDS` 2, `MAX_PLANNING_RECOVERY_ROUNDS` 3 replacing the
  once-per-run latch, plus a re-entrancy guard — and the fallback, when it runs,
  is a **bridge**, not a demotion; auth / 404 / malformed-request /
  capability-refusal classes are not held. `can_cover` is additive:
  `SCHEMA_VERSION` stays `8`, and the desktop Studio round-trips it so a save
  cannot drop it (no UI editor yet).
- **Follow-ups flagged, not actioned here** (each is ADR-74 §Known gaps or
  belongs to a row outside this pass's scope; none is a register row today):
  - **The coordinator's dispatch prompt is not on the `PromptBuilder` path.**
    Verified: `render_dispatch_system_prompt` builds a plain `String` that is
    sent as a single `Message { role: Role::User, content: system_prompt }`
    (`coordinator.rs:10680`), so it uses none of
    `SYSTEM_PROMPT_BUILD`/`_CHAT`/`_PLAN`, has no `{working_memory}`
    placeholder, and gets no stable-head treatment —
    `with_cache_stable_prefix` is consumed only in `runtime_runner.rs` for the
    single-agent loop. **The working-memory delivery in `251d457` and the
    stable-head prefix discipline in `32d6809` therefore do not reach the
    coordinator.** Recorded as a known gap in ADR-74 and deliberately *not*
    fixed here: routing that prompt through `PromptBuilder` is a behavioural
    change to the hot dispatch path and deserves its own decision.
  - **This register's own pass note (2026-09-24) has a formatting defect at the
    `— manual` orphan fragment** (flagged by the 2026-09-26 group-F pass as worth
    a one-line repair). Still unrepaired; left alone to keep this diff scoped.
  - **Carried forward, still unreconciled:** `docs/security-threat-model.md` §6
    gaps #1, #5 and #6 are still worded as fully open (rows 36, 44, 45);
    `docs/TODO.md:229–235` still says the fault-injection item is "Not started"
    (landed in `f4bdc4f`); `ROADMAP.md:241–245` still describes the live
    end-to-end eval harness as fully deferred (`5daf2e7` shipped a first
    env-gated leg). All three were flagged by the 2026-09-26 pass and none is
    a delegation-doctrine item, so none is touched now.

## Verification notes (2026-09-27, shell + plugin-streaming pass)

Docs only — **no source edits, no builds, no test runs, no commit**; the tree is
left for the orchestrator to commit. Checked against
`fix/coordinator-error-invariant` at HEAD `1ba7c1d`. Every claim below was read
in the tree, not taken from a commit message or a plan document's own status
line. No row is opened, closed, cut, or merged; the Closed appendix is
untouched.

- **Scope of this pass: rows 23 and 24 only.** Both stay OPEN. Row 24 was
  rescoped (size **L → M**); row 23's re-entry cell was rewritten and its
  source cell extended. No renumbering; the existing gaps (10, 12, 14, 19, 21,
  26, 33, 44, 47, 49) are left intentionally.
- **Row 24 — the shell runtime is not unbuilt, and the old row said it was.**
  The register previously carried "Shell Phases C–F + slices 1–3" with the
  re-entry "When Phases C–F are scheduled from the roadmap", which read as a
  wholly unstarted runtime. That was stale in the row's favour-of-closing
  direction and is now corrected with per-phase evidence. Phase A and Phase B
  are verified landed, and independently so by the plan document's own status
  lines: `custom-ai-shell-plan.md:125` marks Phase A "**Implemented**" and
  `:143` marks Phase B "**Implemented at the library boundary**". Code anchors
  confirmed: `builtins.rs:20–27` (four read-only commands), `runtime.rs:43–58`
  (`ShellRuntime::standard`, all read-only), `parser.rs:33–60`
  (`parse_command_line`), `model.rs:24–31` (`CommandStatus` predicates),
  `execution.rs:35–80` (`PolicyExecutionAdapter::execute` → `ToolExecutor`, plus
  `external_commands` at `:73–83`), `profile.rs:13` (`ShellProfileCatalog`),
  `config/src/shell.rs:58` (`ShellProfileConfig`). The safety/identity work
  around them is also landed and was previously unrecorded on this row: ADR-55
  containment (`tools/src/containment.rs:1–30`), the row-45 CPU budget
  (`tools/src/shell.rs:65–69`, `:875–880`, `:1231–1303` + `cpu_accounting.rs`),
  the OS identity card (`orchestrator/src/prompts.rs:175–180`, `:375`
  `environment_card`), and bounded shell repair
  (`orchestrator/src/shell_repair.rs`, `MAX_SHELL_REPAIR_ATTEMPTS = 5` at `:36`).
- **Row 24 — the four gaps are real, and each was checked for absence rather
  than assumed.** (a) **Phase C verbs:** a case-insensitive search of
  `crates/shell/src` for `explain|debug|optimi[sz]e` returns only
  `#[derive(Debug)]` attributes plus one unrelated doc comment at
  `model.rs:105`. The intelligence exists as prompt content plus repair — which
  is exactly what the plan's own "Shipped subset (2026-09-07)" note records at
  `custom-ai-shell-plan.md:193–197` — but there is no invocable command.
  (b) **Phase D:** `WorkflowAst` returns **zero** matches repo-wide under
  `crates/`. `shell/src/model.rs:100` is `Workflow`, one variant of the
  `CommandSource` enum (`:93–103`), consumed by `CommandProvenance` (`:105–115`)
  — a provenance tag with no AST behind it, which is the specific confusion
  worth recording. (c) **Phase E:** `shell/src/registry.rs:38–68` is
  `register`/`get`/`specs` over a `RwLock<BTreeMap<String, Arc<dyn ShellCommand>>>`
  — a static in-process register, not an ABI. The real WASM ABI is in
  `crates/plugins` and nothing bridges the shell command spec to it.
  (d) **Phase F:** a search for `self-improv|history mining|validate-before-promotion`
  under `crates/` returns nothing relevant; `shell/src/history.rs` is a bounded
  in-memory `VecDeque<CommandResult>` feeding the `last` meta-command, not a
  learning loop.
- **Row 24 — two premises in the brief did not survive verification, and were
  deliberately not written into the row.** (i) **Slice 2 is not a stub.**
  `tools/src/shell_backend.rs:88–125` `ManagedBash` is a *complete*
  `ShellBackend` implementation (`backend_type`, `resolved_program` via
  `ManagedRuntimeManager::auto_detect`, `command_args`, `effective_env`,
  `check_available`), not a placeholder. Behind it, `crates/config/src/managed.rs`
  (347 lines) implements the ADR-28 Slice 2 PoC's *install/lifecycle* half — not
  the whole slice, which still has the two residuals below: versioned install
  under `<data>/concerto/managed-bash/<version>/bash` (`:114–119`,
  `install_from` `:152–194`), blake3 integrity with `verify` (`:205–232`),
  manifest export/import (`:235–249`), `remove` (`:197–202`), and a bounded
  2 s version probe (`:251–262`) — and it is wired to the desktop Settings UI
  end to end (`views/settings/helpers.rs:39,55,95,109`;
  `settings/shell.rs:187–223` messages and `:509–544` buttons).
  (ii) **`ManagedEnvConfig` is not unused.** It is held
  at `config/src/shell.rs:376` (`ShellSettings.managed`), re-exported at
  `config/src/lib.rs:67`, and populated from the live runtime at
  `desktop/…/views/settings/state.rs:754–762`. The row records what *is*
  genuinely missing from slice 2 instead — **controlled `PATH`**
  (`shell_backend.rs:109–115` `effective_env` clones the base env rather than
  constraining `PATH`) and the **PTY-backed terminal** (a `\bpty\b` search under
  `crates/` returns nothing; no PTY dependency exists in the workspace) — plus
  the licensing-gated vetted-binary distribution that `managed.rs:10–13` and
  `helpers.rs:36–37` already call out as a later slice.
- **Row 24 — slice 1 is also landed, contrary to the old row.** The register
  listed slice 1 as remaining. It is not: `ShellBackend::check_available` exists
  on both backends (`shell_backend.rs:45–46`, `:69–80`, `:117–124`),
  `config/src/shell.rs:235–242` `availability()` is documented "ADR-28 Slice 1",
  and the desktop Test-profile action is live
  (`views/settings/shell.rs:164–183` messages, `:403` button) with its state
  field labelled "ADR-28 Slice 1" at `state.rs:144–146`.
- **Row 24 — ADR-28's status was understated by the old row.** The old source
  cell said ADR-28 is "superseded in part by ADR-30 for shell selection only".
  ADR-28 is **archived and fully superseded** — by ADR-30 (shell selection) *and*
  ADR-29 (the AI-native runtime) — and its own header says it is "not active
  guidance" (`docs/adrs/archive/ADR-28.md:3–10`). The row now says so, because
  citing ADR-28 as live authority for the profile slices is how a reader would
  re-derive a stale scope.
- **Row 24 — the phase-numbering conflict is real and pre-existing.**
  `docs/custom-ai-shell-plan.md:125–228` numbers its phases **A–F**; the research
  plan `docs/research/ai-native-shell-implementation-plan.md:11–510` numbers
  overlapping work **0–5** (Phase 0 at `:11` starts from the `ToolManifest`
  schema system; Phase 5 at `:458`). `docs/TODO.md:136–137` already carries the
  instruction: "fresh phase plan starting with the `ToolManifest` schema system
  (reconcile its phase numbering with `custom-ai-shell-plan.md` before
  starting)". This is why reconciliation is re-entry item (0) rather than a
  footnote — the two documents currently describe the same roadmap in
  incompatible vocabularies, and either could be cited as authority for a
  "Phase 2" that means different things in each.
- **Row 24 — size change L → M, and why.** The reduction is earned, not
  optimistic: two of the four phases and two of the three profile slices are
  landed, so the tracked remainder is the C verbs plus D–F. The row also
  **splits** the scope, and the split is what makes a single size defensible:
  the C verbs are one self-contained piece reusing the existing `ShellCommand`
  trait and result envelope, while D–F is explicitly deferred to its own
  ADR-backed decision. Recorded plainly: **D–F may itself be L-sized once it is
  scheduled** — a workflow AST is a new persistence and execution model, and
  Phase E needs a shell↔WASM ABI decision. The register row now tracks the
  smaller, split scope; it does not claim D–F is cheap.
- **Row 24 — citation drift corrected.** The row cited `TODO.md:114–135`, which
  no longer holds these items; the AI-native-shell entry is at `TODO.md:125–141`
  and the profile-slices entry at `:142–146`. The row now cites `:125–146`.
  `ROADMAP.md:183–187` is cited with line numbers for the first time (it repeats
  the "Phases A and B are implemented as a library foundation" framing).
- **Row 23 — the single-shot ABI claim is stronger than "no export exists", and
  the row now says so.** The decisive evidence is not the absence of a
  streaming export name but the shape of the call itself:
  `plugins/src/active_plugin.rs:151` resolves the export as
  `get_typed_func::<(i32, i32, i32, i32, i32, i32), i64>` and `:154–166` makes
  **one** awaited `call_async`, so the guest can return exactly one
  `(ptr, len)` result per invocation. Supporting anchors: `guest_abi.rs:7`
  (`HOST_ABI_VERSION = 1`), `:9–16` (`RESULT_ERROR`, `pack_ptr_len`/
  `unpack_ptr_len` — the `i64` packing), `:31–42` (the only three exports:
  `call_provider`, `call_adapter`, `call_dialect`, with the 6-param `i64`
  signature documented at `:39`), and no streaming export anywhere in the ABI.
- **Row 23 — the one-chunk mapping and the landed heartbeat were re-verified.**
  `provider_host.rs:146–149` awaits the single `call_provider("complete", …)`
  future and `:151` hands the result to `chunk_from_result` (`:155–163`), which
  reads `content` and `finish_reason` and builds **one** `CompletionChunk`; the
  same single call is made inside the heartbeat task at `:195–199`. The liveness
  half is landed: `heartbeat_stream` (`:175–233`) interleaves
  `CompletionChunk::keepalive()` (`:218`) while the call is in flight, driven by
  the manifest's `heartbeat_interval_secs` through
  `PluginBackedProvider::with_heartbeat`/`with_dialect`
  (`plugins/src/manager.rs:698–725`), with the no-heartbeat single-chunk
  fallback at `provider_host.rs:344–354` (`futures::stream::once`). So the case
  that motivated the deferral — the host looking dead during a slow plugin
  completion — is already handled, which is the reason no urgency sits behind
  this row.
- **Row 23 — the "no consumer" claim verified end to end.** A stream *is*
  produced and consumed: `PluginBackedProvider::stream_completion`
  (`provider_host.rs:287–288`) returns a real `CompletionStream`, collected by
  `PluginManager::collect_providers` (`manager.rs:744–746`) and consumed in the
  runtime wiring (`orchestrator/src/runtime_runner.rs:1688`). What makes the row
  still correct is the *shape*: that stream is keepalive chunks plus exactly one
  content chunk, so a plugin emitting tokens incrementally over minutes would
  still deliver everything in a single terminal chunk. The row says that rather
  than claiming the stream does not exist.
- **Row 23 — the cost is now stated, and ADR-53's "§47" cite was corrected.**
  Building this needs a new ABI export or a host-fn chunk sink the guest calls
  repeatedly, **plus an ABI version bump**. ADR-53 deliberately did neither, and
  says so in three places: `docs/adrs/ADR-53-…:48–50` ("the existing
  `call_tool` / `call_provider` / `call_adapter` ABI v1 exports and the plugin
  load path — **no breaking change, no `abi_version` bump**"), `:117–118`
  ("additive-only — **NO breaking change, NO `abi_version` bump**"), and `:170`
  ("existing manifests, configs, and consumers load unchanged. No
  `abi_version` bump"), with the deferral itself recorded at `:137–139` and
  restated under Consequences at `:171–172`. §4 is titled "Plugin heartbeat —
  keepalive, **no streaming ABI**" (`:128`). The brief's "ADR-53 §47" pointed at
  the "Explicitly **not changed** by this ADR:" lead-in rather than at the
  no-bump text, so the row cites the specific lines instead. The point of stating
  the cost is to stop this being scheduled as a generic "streaming" improvement:
  it is a breaking guest-ABI change with a plugin-compatibility story.
- **Follow-ups flagged, not actioned here** (each is outside this pass's scope —
  they touch documents other than this register, or rows other than 23/24):
  - `docs/custom-ai-shell-plan.md:195` says the repair budget is "2 per failed
    tool-call id per run", but the code is `MAX_SHELL_REPAIR_ATTEMPTS = 5`
    (`orchestrator/src/shell_repair.rs:36`). The plan's Phase C "Shipped
    subset" note is stale on a number, and the plan also still describes Phase B
    as needing "live-tested integration" without saying which frontends have it.
  - `docs/TODO.md:125–141` still reads "AI-native shell Phases C–F. **Not
    started**" and `:142–146` still lists slices 1–3 as remaining. Both are
    stale in the same way row 24's citation had drifted, and the AI-native-shell
    item's own text already flags the phase-numbering reconciliation this row
    now makes re-entry item (0).
  - `ROADMAP.md:183–187` repeats the A/B-landed framing but still lists
    `explain`/`debug`/`optimize`, the workflow AST, the tool/plugin ABI and
    measured self-improvement as one undifferentiated "Later" bullet; the
    C-versus-D–F split in row 24 would give it a first sentence.
  - Still carried forward from the two prior passes and untouched:
    `docs/security-threat-model.md` §6 gaps #1, #5, #6; `docs/TODO.md:229–235`
    ("Not started" for work that landed in `f4bdc4f`); `ROADMAP.md:241–245`;
    and the `— manual` orphan fragment in the 2026-09-24 notes above (line ~170),
    still unrepaired.
