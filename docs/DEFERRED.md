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
> **Append 2026-09-27 (owner decisions 2026-09-27, rows 4/25/28/37/9/29):**
> two rows **cut** and moved to the Closed appendix with their scope recorded so
> the cuts stay reversible — row 25 (external-editor integration; the in-app
> editor and diff viewer are landed and were *not* cut) and row 28 (ADR-58 P5/P6
> + TOML diff + canvas DAG editor + multi-executor partitioning, cut because the
> orchestration-studio UI is scheduled for a separate refactor). One row
> **rescoped and kept OPEN**: row 37 (hybrid UI full scope), size **L → M**,
> because the Minimal and Medium tiers are landed and what remains is
> finish-and-polish with no correctness content. One row **corrected**: row 9's
> `[verify]` flag and "unverified" framing are removed — the setup wizard has a
> real source and real substance; the row stays OPEN and deferred. One row
> **reviewed and left exactly as it was**: row 29 (ADR-43 server mode / SSE /
> marketplace / persistent state / TOML secrets) — still deferred, no status or
> size change, with the factual state recorded in the notes below. Row 4 was
> read and left untouched. No renumbering; numbering gaps left intentional.
> Verification notes for today are appended below.
> **Append 2026-09-27 (row 4 closed — desktop ack queue shipped in `8e3c350`):**
> row 4 **closed**. ADR-68 §6's deferred "implementation phase" is now the
> shipped state, and it shipped in the direction §6 chose rather than in the
> opposite one an earlier note recorded: the desktop ack cell is a depth-bounded
> `VecDeque<PendingAck>` FIFO under `MAX_PENDING_ACKS = 2` — the active dialog
> plus one queued, the exact bound §6 names — with explicit fail-closed overflow
> rejection (`AckQueueError::QueueFull` / `StateUnavailable`) surfaced to the UI
> as an `ErrorOccurred` toast rather than a silent drop, session-gated
> resolution preserved through the queue, and an "Acknowledgement 1 of N
> pending" indicator that leaves the single-ack modal layout byte-for-byte
> unchanged. The row's stale pre-queue note ("single slot", "reject-busy went
> the other way on purpose") is **deleted as superseded** — it described the
> pre-queue tree. One residual is recorded rather than dropped: an overflow
> refusal audits as `RequestAbort`, because `ApprovalSink::request_ack` returns
> only `bool`, so an overflow is distinguishable from a user cancel by message
> text but not by a distinct verdict; closing that needs a widened sink
> contract, out of scope. No renumbering; the `#4` gap is left intentionally.
> Verification notes for today are appended below.
> **Append 2026-09-27 (rows 3 / 30 / 29 — plugin watcher cut, registry fold, row
> 30 rescope):** row 3 (`.wasm` plugin-file watcher) **cut as a DUPLICATE** of
> row 30's hot-reload clause: the row's own re-entry said "if 'plugin hot-reload'
> was meant, row 30 applies", and it does. Row 30's **registry/marketplace clause
> is folded into row 29**, and row 30 is **rescoped to what remains — hot reload +
> remote plugin loading — size L → S**. Row 29's **status and size are unchanged**;
> only its marketplace clause was deepened, because the registry half turned out
> to hinge on a **trust-model precondition rather than a feature**: plugins load
> with **no signature or provenance check** (ADR-37 defers registry signature
> verification to post-v1.0), so a registry changes what the user is being asked
> to trust. One premise supplied for this pass was **verified false and corrected
> in the notes below**: ADR-37's hash-pinning hashes the **plugin binary**, not
> the granted capabilities — which *strengthens* the re-approval story for hot
> reload rather than weakening it. Two stale cites in row 30 were corrected
> (`TODO.md:108–110` was the PersonaMem item; `ADR-21.md` is an archived 12-line
> stub). No renumbering; the `#3` gap is left intentionally. Verification notes
> for today are appended below.
> **Append 2026-09-27 (rows 32 / 35 — certified-evolution cut, STATUS-checklist
> reclassification):** two rows **cut** and moved to the Closed appendix as #20
> and #21, for different reasons that are recorded separately. **Row 32**
> (certified evolution / profile-guided CI): its cited
> `docs/research/certified-universal-evolution.md` does not exist **and is
> deliberately git-ignored** (`.gitignore:54`, under the `:53` comment "Private
> research — not for public until ready"), so the register had been citing a
> document that *cannot* exist in a public clone — and the track is research,
> not backlog. **Row 35** (STATUS-tracked follow-ups): a **reclassification, not
> a cancellation** — those items are per-release *human checklists* hanging off
> `TESTING.md` and `docs/live-test-template.md` (neither has a "done" state,
> because a fresh sheet is filled in per build/OS/provider), so a DEFERRED row
> with a re-entry condition and an owner mislabelled them as debt. Two premises
> in the row-32 brief did not survive verification and are **not** written down
> as claims: `confidence` is *not* unique in the tree (85 matches — the *name*
> collides; the *construct* is absent) and `evidence` is *not* absent either
> (ADR-65's evidence spine is landed; what is missing is a type-level promotion
> gate). Two cites were corrected: row 35's multiline item lives in
> `views/orchestration_studio.rs:4114`, not `app.rs:4114`, and row 32's two cites
> had drifted off the item entirely. `TESTING.md`, the live-test templates and
> `docs/STATUS.md` are **untouched** — they remain the source of truth. No
> renumbering; the `#32` and `#35` gaps are left intentionally. Verification
> notes for today are appended below.
> **Append 2026-09-27 (rows 39 / 40 — release-and-packaging cut, crates.io
> publish cut):** two rows **cut** and moved to the Closed appendix as #22 and
> #23, both on the owner decision of 2026-09-27 that each is wanted
> **eventually** — so both cuts are **RESUMABLE, not cancelled**: the verified
> state is recorded such that the work can be picked up without repeating any
> of the investigation done here. They are cut together because they share a
> single source bullet (`ROADMAP.md:197–198`) and a single unexercised release
> pipeline, but they rest on **different** findings and are recorded
> separately. **Row 39** (binary installers deb/rpm/tar) was cut because the row
> **implied much less built state than actually exists**: a real 4-target
> tag-triggered release pipeline is already in the repo and publishes a GitHub
> Release with auto-generated notes — and the genuinely missing part is only
> everything downstream of a raw binary (no committed installer definitions, no
> code signing anywhere, no working update channel, no aarch64-linux or
> arm-windows). The load-bearing fact recorded in the appendix is that the
> pipeline **has never been run**: the repo has **zero git tags**, so the `v*`
> trigger has never fired. **Row 40** (crates.io publish) was cut because its
> re-entry condition named a blocker that does not exist — `publish = false` is
> one line at `Cargo.toml:37`, licence is already `MIT OR Apache-2.0` and
> allowed by `deny.toml:101–104`, and there are **no git or out-of-workspace
> path dependencies**, so nothing outside the workspace needs publishing first.
> What the appendix records instead is the **decision implicitly made**: the
> real question is the *subset*, because the graph is coupled (desktop carries
> 13 internal edges, eval-runner 8, plugins exposes core + api-types), so
> publishing everything means freezing the whole internal API at `0.1.0`. The
> realistic option recorded is: publish the `concerto` binary (and possibly
> `concerto-plugin-sdk`) and keep the rest `publish = false`. **Four premises
> from the brief did not survive verification and are corrected in the notes
> below rather than repeated as claims:** the workspace has **25 members, not
> 26**; **20** crates lack a `description`, not ~21; `repository` **is**
> defined at `Cargo.toml:36` but **no crate inherits it**, which is a
> different (and much smaller) piece of work than "no repository link"; and
> the path-only-without-`version` edges number **8 across 6 crates**, not 4.
> Two claims were *sharpened* rather than contradicted: desktop's public
> surface names `core`/`config`/`tools`/`plugins`/`sessions` types, **not**
> orchestrator or memory (those are private `use`s behind a real dependency
> coupling); and `crates/cli/src/update.rs` is **not dead code** — it is wired
> at `crates/cli/src/lib.rs:182–185` — so what is inert is its *target*, plus
> the newly-found fact that `[updates].update_endpoint`
> (`config/src/schema.rs:941–942`) is **declared, documented, example'd and read
> by nobody**. **Both rows' cites had drifted and are corrected in the
> appendix**: `TODO.md:260–263` and `TODO.md:264–265` are the audit M-05 and
> M-02 items, not these rows (the real entries are `docs/TODO.md:271–274` and
> `:275–276`), and `ROADMAP:194` is a line **inside row 32's now-cut
> certified-evolution bullet** — both release rows were citing another closed
> row's source (the real cite is `ROADMAP.md:197–198`). `TODO.md`,
> `ROADMAP.md` and `docs/STATUS.md` are **untouched** and remain the source of
> truth. No renumbering; the `#39` and `#40` gaps are left intentionally,
> matching the row 3 / 4 / 10 / 12 / 14 / 19 / 21 / 25 / 26 / 28 / 32 / 33 / 35 /
> 44 / 47 / 49 precedent. Verification notes for today are appended below.

## Open register

| # | Item | Source + date | Re-entry condition | Size |
|---|------|---------------|--------------------|------|
| 1 | `cache_stable_prefix` wired into engine/request assembly (stable-head prefix discipline) | ADR-48 D1; knob exists at `crates/config/src/schema.rs:649,683` ("the ADR-048 gap knob") but is not wired into any engine/request path | When prompt-cache prefix stability is exercised (TODO.md "prompt-cache stability" ~271–282) | S |
| 2 | Responses `tokens_out` / `tokens_in` written per message from provider-reported usage | ADR-48:27–29,111 — columns exist but are always `0` (inert); per-iteration totals surface resolved provider only | When real provider-reported usage accounting lands (ADR-48 §4/§5; nothing wired today) | S |
| 5 | Recall budget caps + recall timeout (char caps, 5000 ms timeout race, skip-with-warning, capability-envelope payload; includes the ChunkSelector char-budget allocator across the selected chunks — TODO.md:88 "no char budget across the selected chunks"; naming trap: the `chunk_selector.rs` constants are compaction selection, not recall) | TODO.md memory section (caps/timeout ~81–88) | When a char-budget allocator + timeout guard land in `chunk_selector.rs`/`rag.rs` | M |
| 6 | Tier-3 SDKs — Copilot / Bedrock / Azure / Vertex / watsonx | docs/missing-providers.md Tier 3 (open) | Per-provider wrapper + pairwise parity test; Tier 1/2 already done | L |
| 7 | Doubao / StepFun / Replicate providers | `[unplanned]` — no repo register (zero matches for these names) | Add explicit provider rows if planning changes | L |
| 8 | Vercel Gateway decision | `[verify]` researched in docs/research/multi-provider-resilience.md (Vercel AI SDK among gateways); no in-repo decision record | Confirm whether a dedicated gateway row is wanted | S |
| 9 | Setup wizard per-variant auth flows — **re-verified 2026-09-27: NOT unverified. The `[verify]` flag and the "per-variant auth steps unverified" framing are removed; the row stays OPEN and deferred per owner decision 2026-09-27** | **The wizard exists and is wired.** `SetupWizard::run` (`crates/config/src/setup.rs:236–264`) runs provider → key → model → working dir → policy and returns a `PendingConfig`; `PendingConfig::save` / `save_overwrite` (`:109–119`) write the TOML and push the key to the OS keychain via `CredentialStore` (ADR-04 — the key is never written to TOML). Hooked into the CLI at **first-run and `--reconfigure`** (`crates/cli/src/lib.rs:106–170`), which deliberately does *not* call `run()`: it re-drives the same prompts individually (`:126–152`) so a **live model probe** can be slotted between the key and the model steps — `list_models_for_provider_blocking(provider_name, &api_key, None)` at `:141–146` feeding `set_available_models` (`:146`) for a numbered picker. **There is ZERO desktop onboarding wiring:** a search of `crates/desktop` for `SetupWizard` / `setup::` / `needs_setup` / `run_wizard` returns no hits, so the wizard is reachable only from a terminal. **The per-variant auth gap, precisely:** one generic `prompt_api_key` (`setup.rs:314–317` — `"API key (leave blank for local models): "`, taking no provider argument) serves **all seven `ProviderKind`s** (`:40–48` — OpenAI, Anthropic, Ollama, NvidiaNim, OpenRouter, OpenCodeZen, Other), and the **only per-kind delta anywhere in the wizard is `default_model`** (`:51–61`). There is no OAuth / device-code / token-exchange variant for any kind, and `Other` is a bare custom provider id with no base-URL or credential-shape step | **Two separable pieces, deliberately not merged here.** (a) Per-kind auth flows — an auth step that varies by provider kind, not a second generic key prompt. (b) A desktop onboarding surface, so the first-run experience does not require a terminal. Either can be taken alone; (a) is a `setup.rs` change, (b) is new UI. Re-entry is whichever the owner schedules first | M |
| 11 | M-01: Estimator deduplication + `rag_pct` configurability | ADR-67 follow-ups (~92–98) | When Estimator/rag_pct consolidation is taken up | S |
| 13 | Codebase cascade S1/S2/S3 (link store, scoring, slicing) | ADR-69 slices; IMPLEMENTED on fix/coordinator-error-invariant 2026-09-24 (S1 link store + write path, S1 activation, S2 scoring/decay/caps, purge fix, S3 observability — commits 60841dd/af98afe/224f0f9/7713b8d/7584d2d; tests green incl multi-hop guard) | CLOSED on merge of that branch + production proof (link verdicts firing, cascade reorder observed in a live run) | L |
| 15 | ADR-60 Deferred item 4 — multi-level disclosure + real-embedder swap proof + scheduler/subscription generalization at high agent counts — **rescoped 2026-09-26 per owner clarification: "beyond 6 agents" is NOT a 6-agent cap** | ADR-60 Revision **v1.2 item 4** (`:150–151`: "Deferred item 4 (scheduler/subscription generalization beyond 6 agents, real-embedder swap, multi-level disclosure) remains deferred per ADR") — **not** `ADR-60:112`, which is only an IPC-overhead cost note ("acceptable at 3–6 agents; revisited only if profiling demands") and was the wrong cite for this row; ADR-60 `:31`/`:98` (v1 *scale* is 3–6 agents, but the scheduler/subscription model is "expressed with N not hardcoded anywhere"); ADR-58:247 (the same three items deferred) | **Doctrine: agents are ADDITIVE with no hard cap.** No `MAX_AGENTS` — or any total-agent limit — constant exists anywhere in `crates/`; the only agent-count-shaped cap in the gate is the **per-agent** `Semaphore` `WriteGate::max_in_flight_per_agent` (`crates/orchestrator/src/gate.rs:501,678,1084`), so nothing rejects an Nth agent and agents compose without a ceiling. The old "when profiling shows need beyond 6 agents" trigger is therefore **not** a re-entry gate: profiling pressure cannot unblock a cap that does not exist. What is genuinely open is the three sub-items themselves: **(a) multi-level disclosure** — today a *single* level, the `retrieve-memory` shortlist clamped to `DISCLOSURE_MAX_CHUNKS = 10` (`consolidation.rs:80`; `supervisor.rs:2118,2129`, `ADR-60:360`), with topic hierarchy, filter-by-relevance/recency, cross-subscriber backpressure signalling and schedule-driven pushes all behind item 4 (`ADR-60:326–329`); **(b) the real-embedder swap proof** — the swap is *real, not pending*, so this is a smaller residual than the old row implied: `ProviderEmbedder` runs `fastembed` (BAAI/bge-small-en-v1.5) on-device and is the production wiring (`memory/src/embedder.rs:42–110`; `orchestrator/src/runtime_runner.rs:1317`), so the consolidation projection no longer depends on the deterministic `feature-hash` placeholder — only the **fallback dependency** remains (retiring the `fastembed` fallback in favour of a provider-hosted embedding API, and the `EmbedderState::Unavailable` degradation path); **(c) scheduler/subscription generalization at high agent counts** — bounded slices, per-agent cursors and disclosure, characterized at 3–6 agents, not at swarm scale. | L |
| 16 | Single-project limit (one active project per process) | ROADMAP:216 (explicitly deferred/incomplete); STATUS.md | Multi-project support requires per-project scoping of process-global services | L |
| 17 | Eval end-to-end benchmark (live runtime over a real benchmark) **+ multi-agent fault-injection containment coverage (former row 21, merged 2026-09-26 as an included sub-part) — both halves landed 2026-09-26** | ROADMAP:241–245 ("a full live end-to-end eval harness over a real benchmark task remains deferred until multi-agent quality and recovery are reliable"); row 21's former source TODO.md:229–235 (injection tests for rate limits, malformed tool calls, missing executables, cancellation races, provider disconnects — the register's old `TODO.md:218–224` cite had drifted onto the *resolved* eval-`#[ignore]` item and is corrected here); ADR-26 recovery boundaries; scenarios at `crates/orchestrator/src/fault_injection.rs:76–92`; live leg at `crates/eval-runner/src/main.rs:430–490` | **Landed in two commits.** `f4bdc4f` added four **crash-window** fault-injection scenarios to the existing in-process (scripted, deterministic) suite, which had 16 prior scenarios but none expressing the durable/child boundary: **C1** specialist child dies mid-dispatch (issued, never settled) → `specialist_child_dies_mid_dispatch_audits_then_recovers`; **C2** provider disconnect at the settle boundary (after a dispatch settled, before the next decision) → `provider_disconnect_at_the_settle_boundary_preserves_settled_work`; **C3** restart from a durable checkpoint row persisted through a **real** session store, continuing the pending dispatch **exactly once** → `restart_with_preserved_checkpoint_continues_the_pending_dispatch`; **C4** cancellation racing a settle, which **re-pends rather than accepts** → `cancellation_racing_a_settle_re_pends_not_accepts`. `5daf2e7` added the live-runtime benchmark leg, gated **twice** so it never runs in CI (the `#[ignore]` attribute *and* a runtime env check): `CONCERTO_LIVE_PROXY` + `CONCERTO_LIVE_PROXY_KEY` are both required and it skips cleanly when unset; `CONCERTO_LIVE_PROXY_MODEL` is optional (default `gpt-4o-mini`) and `CONCERTO_LIVE_EVAL_SUITE` is an optional suite-dir override; run with `cargo test -p concerto-eval-runner live_runtime_benchmark_leg -- --ignored --nocapture`. **Row STAYS OPEN** — what remains is coverage *breadth*, not the mechanism, and the old ROADMAP trigger is now only half-met: the live leg drives **one** suite against **one** live endpoint, so broad live-provider / multi-model coverage is still unproven, and there is **no quarantine mechanism** for live flakiness (a red live run is indistinguishable from a real regression). **Re-entry: widen the live leg beyond a single suite/model, and land a flake-quarantine story for it.** Size stays **L** — both shipped halves were the cheap, narrow half of the original ask. | L |
| 18 | Coordinator restart/resume end-to-end (cross-process continue) — **annotated 2026-09-27, still OPEN and unchanged** | TODO.md:18–25 (Partial; e2e remains); ADR-34 D2 | After checkpoint persistence + evidence-spine resume e2e. **Not advanced by ADR-74:** the delegation guard's "has this run delegated?" test reads the *checkpointed* decision journal (`DecisionKind::DispatchSpecialist` entries) and the agent-axis takeover guard is checkpointed too (`GraphCheckpoint.specialist_takeover_attempted`, `#[serde(default)]`), so both guarantees survive a restore — but that is per-run state restoration, not the cross-process continue this row tracks, which still needs the e2e evidence named in its source cell. | L |
| 22 | ADR-47 message `parts` (canonical parts replace flat string content) | ADR-47:85–98 (deferred; flat model retained); ARCHITECTURE-V2.md:323 | **GATED DOCTRINE 2026-09-25** — stays OPEN, but gated on ADR-47's own reopen condition (:78–93): reopen **only** when a consumer proposes/needs richer message content the flat shape cannot express (multi-part tool bodies, image/file `File` parts, structured `Thinking`/`RedactedThinking` on Anthropic/Gemini paths, or structured data smuggled into `content: String`). Migration preconditions recorded 2026-09-25 — all three required before any parts work: **(a)** a named consumer need (ADR-47:80–81); **(b)** a parts-joined-text equivalence test — joining `parts` back to flat text must reproduce today's bytes, and Fix 2 strictness must not loosen (proxy-tool-call-fix.md:5,60 — content-embedded strict path buffers content to turn end); **(c)** a checkpoint/`state_json` migration plan for old rows (`orchestration_checkpoints.state_json` is `TEXT NOT NULL`, migration 019:10; `crates/sessions/src/lib.rs:150`), shipped additively with `serde(default)` + legacy-column fallback in one dedicated window, never as a side effect of an unrelated feature (ADR-47:62–65,109–110; ARCHITECTURE-V2.md §10). Verified migration-safe meanwhile: the Mimo/loose-tier tool mods operate on `ToolDefinition`/`ToolCall` **values**, not message shape — `schema_loose.rs:113` `adapt_tool_definitions(&mut [ToolDefinition])`, `:132` `unflatten_tool_arguments(&mut serde_json::Value)`, `google.rs:55` `adapt_tools_for` mutates `request.tools` — so they neither block nor depend on a parts migration. | L |
| 23 | ADR-53 per-token streaming through WASM (heartbeat landed) — **stays OPEN and deferred; trigger sharpened 2026-09-27 to a named-consumer precondition, with the cost stated** | ADR-53 `docs/adrs/ADR-53-dialect-plugins-and-plugin-heartbeat.md:137–139` ("**No per-token streaming through WASM in this ADR** — streaming is deferred (§Consequences)"), restated as a Consequences deferral at `:171–172`; the deliberate no-ABI-change stance at `:48–50`, `:117–118` and `:170` ("no breaking change, no `abi_version` bump"); the liveness gap it was opened for at `:39–41`; heartbeat landed per ADR-53 §4 (`:128`) | **Precondition: build ONLY when a plugin kind actually generates tokens incrementally.** The ABI is single-shot *by construction*, not by omission: `guest_abi.rs:7` `HOST_ABI_VERSION = 1`; the return value is a `(ptr, len)` pair packed into one `i64` (`:9–16` `RESULT_ERROR`/`pack_ptr_len`/`unpack_ptr_len`); the only three exports are `call_provider`/`call_adapter`/`call_dialect` (`:31–42`, the dialect signature documented as 6 `i32` params → `i64` at `:39`); and the host resolves the export as `get_typed_func::<(i32, i32, i32, i32, i32, i32), i64>` and makes **one** awaited call (`active_plugin.rs:149–166`). **No streaming export exists, and none can be added without a version bump.** Today the host awaits one `call_provider("complete", …)` future and maps the whole JSON result to **ONE** `CompletionChunk` (`provider_host.rs:146–149` awaited call → `:151`; `chunk_from_result` at `:155–163` reads `content`/`finish_reason` and builds a single chunk; the same single call inside the heartbeat task at `:195–199`). **The liveness case that motivated the deferral is already covered** by the landed heartbeat, which is why there is no urgency behind this row: `heartbeat_stream` (`:175–233`) interleaves `CompletionChunk::keepalive()` (`:218`) on a cadence while the call is in flight, wired from the manifest's `heartbeat_interval_secs` (`manager.rs:698–725`, `with_heartbeat`/`with_dialect`), with a no-heartbeat single-chunk fallback (`:344–354`, `futures::stream::once`). **There is NO consumer that would benefit today:** `PluginBackedProvider::stream_completion` (`provider_host.rs:287–288`) does return a real stream, collected by `PluginManager::collect_providers` (`manager.rs:744–746`) and consumed in the runtime wiring (`orchestrator/src/runtime_runner.rs:1688`) — but that stream is *keepalive chunks plus exactly one content chunk*, so a plugin that emits tokens incrementally over minutes would still deliver all of its output in a single terminal chunk. **Cost, stated so this is not scheduled as a generic "streaming" improvement:** it requires a new ABI export (or a host-fn chunk sink the guest calls repeatedly), **plus an ABI version bump** — and ADR-53 deliberately did neither (`:48–50`, `:117–118`, `:170`; §4 is titled "Plugin heartbeat — keepalive, **no streaming ABI**"). It is therefore a breaking guest-ABI change with a plugin-compatibility story, not a provider-host refactor. **Re-entry: a named plugin kind that generates tokens incrementally** (the motivating case being a plugin that wraps a slow local model); until one exists, the keepalive-plus-one-chunk stream is the correct v1. | M |
| 24 | Shell Phases C–F + ADR-28 profile slices — **rescoped 2026-09-27: Phases A/B and profile slices 1–2 have LANDED; what remains is the C verbs plus D–F, and the row is split in two** (size **L → M**) | ADR-29 (`docs/adrs/ADR-29.md:3`, Accepted) is the runtime/policy execution decision. Repo plan `docs/custom-ai-shell-plan.md:125–228` = **Phase A–F** (A at `:125` already marked "**Implemented**"; B at `:143` "**Implemented at the library boundary**"; C `:184`, D `:199`, E `:209`, F `:219–228`). Research plan `docs/research/ai-native-shell-implementation-plan.md:11–510` = **Phase 0–5** (0 at `:11`, 5 at `:458`) — a *different* numbering for overlapping work. TODO.md:125–146 (both items; **the row's old `TODO.md:114–135` cite had drifted** onto other rows and is corrected here); ROADMAP.md:183–187. Profile slices come from ADR-28, which is **archived and superseded** by ADR-30 (shell selection) + ADR-29 (`docs/adrs/archive/ADR-28.md:3–10`, "not active guidance") — so ADR-28 is historical rationale here, not live authority, and the old row's "superseded in part by ADR-30 for shell selection only" understated it | **PREREQUISITE — reconcile the phase numbering before starting anything, as `TODO.md:136–137` explicitly instructs** ("fresh phase plan starting with the `ToolManifest` schema system (reconcile its phase numbering with `custom-ai-shell-plan.md` before starting)"). Two live plans number the same work incompatibly: the repo plan uses **A–F**, the research plan uses **0–5**. Settle which numbering governs (and whether the research plan supersedes, merges into, or is discarded alongside) before any "Phase C" work is named, or the two get silently merged into a scope neither plan approved. **LANDED — recorded so this row stops implying the shell runtime is unbuilt.** *Phase A (read-only builtins + runtime):* `crates/shell/src/builtins.rs:20–27` (`standard_commands` → `help`/`project-info`/`ls-tree`/`last`), `runtime.rs:43–58` (`ShellRuntime::standard`, every command registered read-only), `parser.rs:33–60` (`parse_command_line` — quoted tokenizing, no shell expansion), `model.rs:24–31` (`CommandStatus::permits_continuation`/`is_success`). *Phase B (policy-gated external exec):* `shell/src/execution.rs:35–80` (`PolicyExecutionAdapter::execute` funnels through `ToolExecutor::execute("shell", …)`, so no command spawns a process itself) and `:73–83` (`external_commands` → `run`/`shell-run`/`shell-profiles`); `profile.rs:13` (`ShellProfileCatalog` — the shell consumes canonical config instead of keeping a second selector, per ADR-30); `config/src/shell.rs:58` `ShellProfileConfig`. *Quoting / argv-direct:* `tools/src/shell.rs:349–441` (`windows_arg_needs_quoting`, `shell_quote_windows`, `shell_quote_posix`, `shell_quote`) with `validate_cmd_args:510` rejecting arguments cmd.exe would `%`-expand, and `:614–640` `legacy_shell_plan` returning `ShellPlan::Direct` argv-direct at `:623–627` (bypass_shell, and Windows where no shell semantics are needed); `tools/src/shell_backend.rs:57–59` `command_args`. *ADR-55 containment:* `tools/src/containment.rs:1–30` (argv/cwd confinement — out-of-root `cd`/`pushd`, path-like args on mutation verbs, `xargs` pipeline laundering of a read-exempt argument, redirect writes). *CPU budget (row 45's portable layer):* `tools/src/shell.rs:65–69` `resolve_cpu_budget` (config wins, including `Some(0)` = off, else `CONCERTO_SHELL_CPU_BUDGET_SECS`), `:875–880`, `:1231–1303` (`plan_takes_cpu_backstop`, the soft-only `cpu_limit_prelude` `ulimit -S -t`, and `spawn_plan` → `ProcessHandle::run_limited`), plus `tools/src/cpu_accounting.rs`. *OS identity card:* `orchestrator/src/prompts.rs:175–180` (always appended, never empty, never errors) and `:375` `environment_card` (profile facts, else OS facts + `detect_os_default_shell`; no process spawned). *Bounded shell repair:* `orchestrator/src/shell_repair.rs` (its own doc calls it a "Phase C subset"); `MAX_SHELL_REPAIR_ATTEMPTS = 5` at `:36`, char caps at `:40–42`, and policy outcomes are **never** repaired (a denial is not coached around) with cancellation spending no turn. *Slice 1 (test profile + availability):* `config/src/shell.rs:235–242` `availability()`, documented "ADR-28 Slice 1"; `tools/src/shell_backend.rs:45–46,69–80,117–124` `check_available`; the desktop Test-profile action is live (`views/settings/shell.rs:164–183` messages, `:403` button; state field labelled "ADR-28 Slice 1" at `state.rs:144–146`). *Slice 2 (Managed Bash PoC) — also landed, and it is **not** a stub:* `config/src/managed.rs` (347 lines) does the versioned, offline, integrity-checked install ADR-28 asked for — versioned dir under `<data>/concerto/managed-bash/<version>/bash` (`:114–119`, `install_from` `:152–194`), blake3 integrity + `verify` (`:205–232`), manifest export/import (`:235–249`), `remove` (`:197–202`), bounded 2 s version probe (`:251–262`); `tools/src/shell_backend.rs:88–125` `ManagedBash` is a **complete** `ShellBackend` impl (not a placeholder) resolving through `ManagedRuntimeManager::auto_detect`; `ManagedEnvConfig` (`config/src/shell.rs:352`) is **not** unused — it is held at `:376` (`ShellSettings.managed`), re-exported at `lib.rs:67`, and populated from the live runtime at `desktop/…/views/settings/state.rs:754–762`; the install/remove/verify/export/import UI is wired (`views/settings/helpers.rs:39,55,95,109`; `settings/shell.rs:187–223` messages, `:509–544` buttons). **REMAINING GAP, stated precisely.** **(a) Phase C's `explain`/`debug`/`optimize` commands are ABSENT** — no such command exists; the intelligence is *prompt content plus repair*, not invocable commands (the plan's own "Shipped subset (2026-09-07)" note, `custom-ai-shell-plan.md:193–197`). A search of `crates/shell/src` for those names returns only `#[derive(Debug)]` attributes and one unrelated doc comment. **(b) Phase D deterministic workflows are ABSENT** — there is no workflow AST: `WorkflowAst` has **zero** matches under `crates/`, and `shell/src/model.rs:100` `Workflow` is a variant of the `CommandSource` enum (`:93–103`), i.e. a provenance tag with no AST behind it. Consequently no cycle/variable/result-type/effect validation, no checkpoints or resumable execution, no bounded retry/fallback/approval/parallel nodes, no execution trace. **(c) Phase E's shell tool/plugin ABI is ABSENT in the shell crate** — `shell/src/registry.rs:38–68` is a static in-process register (`register`/`get`/`specs` over a `RwLock<BTreeMap<..>>`), not an ABI; the real WASM ABI lives in `crates/plugins` (`guest_abi.rs`, `active_plugin.rs::call_json_export`) and **nothing bridges the shell command spec/result envelope to it**, so no shell command can be shipped as a plugin and no custom tool can add shell schemas, renderers, or effect declarations. **(d) Phase F measured self-improvement is ABSENT** — no history mining, no alias/workflow-rewrite suggestion, no validate-before-promotion gate, no provenance/comparison evidence; `shell/src/history.rs` is a bounded in-memory `VecDeque<CommandResult>` that feeds the `last` meta-command and is not a learning loop. *Slice 2 residual (small, specific):* two ADR-28 requirements are unmet — **controlled `PATH`** (`tools/src/shell_backend.rs:109–115` `effective_env` just clones the base env instead of constraining `PATH`) and **PTY-backed terminal** (a `\bpty\b` search under `crates/` returns nothing; there is no PTY dependency anywhere in the workspace). Distribution of a vetted Bash binary is explicitly the later licensing-gated slice (`config/src/managed.rs:10–13`, `views/settings/helpers.rs:36–37`). *Slice 3 (cross-platform packaging):* **not started** — consistent with a deliberately Linux-first PoC, and it is licensing/provenance-gated by ADR-28's own framing. **THE WORK IS SPLIT IN TWO, and the re-entry follows the split.** **(1) The C verbs are one self-contained piece** — prompt/LLM work that reuses the existing `ShellCommand` trait, registry and `CommandResult` envelope, with no new state machine, no persistence, and no new ABI; `explain`/`debug`/`optimize` can and should ship on their own. **(2) D–F are a separate and larger design decision** — a workflow AST is a new persistence + execution model (versioning, validation, checkpoint/resume, node scheduling), and Phase E additionally needs a shell↔WASM ABI decision; neither can ride along on an "add three commands" change. **Re-entry: (0) reconcile the phase numbering, then (1) take the C verbs as its own change, and (2) take D–F only as its own decision** — ADR-first per the repo ADR rule, since a workflow AST is exactly the kind of architectural decision that needs a record before dependent code. | M |
| 27 | Pricing/metadata freshness feeds the VISUAL SPEND TRACKER ONLY (rescoped 2026-09-25: display-only, spend tracker) | TODO.md:201; STATUS.md tracked follow-ups | When the display-only pricing/metadata freshness flow lands. Stale prices make usage dollars wrong; routing must never see prices. Non-goal: the coordinator must never know or care about cheap vs expensive models; no cost-based routing, no model switching on price, and no coordinator coupling of any kind. | M |
| 29 | ADR-43 server mode / SSE / marketplace / persistent desktop state / TOML secrets — **reviewed 2026-09-27 (owner decision): still deferred, no status and no size change. Marketplace sub-item DEEPENED 2026-09-27** (the registry/marketplace clause folded in from row 30, which is rescoped to hot reload + remote loading; **status and size still unchanged**) — **MARKETPLACE/REGISTRY, verified ABSENT: no catalog, no index, no version pinning and no signature verification exist in `crates/plugins`**; `registry` in that crate means only the core `ToolRegistry` tool-registration type (`manager.rs:340,442`, `tool_bridge.rs:207,230`), never a plugin registry. **The load-bearing precondition is the trust model, not a feature: plugins load with NO signature or provenance check** — the repo's own stated position is ADR-37's Alternatives entry, "**Plugin registry signature verification:** most secure, but introduces a dependency on a registry and key infrastructure. **Deferred to post-v1.0**" (`ADR-37.md:104–105`). The only integrity mechanism is ADR-37 grant hash-pinning, and it is worth being exact about what that is and is not: each persisted grant stores `manifest_hash` = **SHA-256 of the loaded WASM binary** (`ADR-37.md:39–40`; computed over the bytes at `manager.rs:193–197` via `capability.rs:9–15` `sha256_hex`), compared against the current binary on every load (`capability.rs:450–454` `load_for_plugin(… wasm_hash …)`, `:364–366` `hash_mismatch`), with prune-on-mismatch forcing a fresh prompt (`manager.rs:205–229`; filter + prune-persist at `capability.rs:465–505`). So it binds an approval to **the exact bytes of a binary** — a **re-approval** mechanism ("the binary changed since you approved it"), **not** an **authenticity** mechanism: nothing anywhere establishes *who published* a file. A registry therefore changes the trust question from "I chose this local file" to "someone published this, and it is executed with no signature check". **Re-entry is a provenance / trust-model decision (ADR-first per the repo ADR rule), not a feature to schedule.** | ADR-43 §3 v1 note (MCP server mode, SSE transport, marketplace/registry, keyring-backed tokens, persistent desktop state deferred); docs/skills.md:202 | When the API surface / web UI materializes | L |
| 30 | Plugin **hot reload + remote plugin loading** — **rescoped 2026-09-27: the registry/marketplace clause is FOLDED INTO row 29** (which now carries the no-signature-check trust-model precondition), **so this row is only what remains** (size **L → S**) | `docs/TODO.md:119–121` ("**Plugin hot-reload, remote plugins, registry.** Not started — `docs/adrs/ADR-21.md:30-31` deferred list; requires the community/registry story before a distribution path exists") — **the row's old `TODO.md:108–110` cite was wrong and is corrected here: those lines are the PersonaMem long-horizon-memory-eval item**, not plugins. Note the TODO's own `ADR-21.md:30-31` pointer is also stale: `ADR-21.md` is a 12-line **archived stub** superseded by ADR-14 (`ADR-21.md:3–8`), so the deferred list is no longer reachable at that cite. ADR-21 (Archived, superseded by ADR-14); ADR-43 §3 v1 deferral | **HOT RELOAD — verified ABSENT in any form, and the existing discovery path is additive by design rather than half-built.** There is no reload, invalidate, or mtime handling anywhere in `crates/plugins`; `refresh_new_plugins` (`manager.rs:549`) deliberately **skips already-active plugins** (`:583–585`, "plugin refresh: already active — skipping") so a refresh never displaces a plugin a running agent holds. The reload seam is therefore clean and already built: `PluginManager.active` is a `HashMap<String, Arc<Mutex<ActivePlugin>>>` (`manager.rs:64`), unload is `unload_plugin` / `unload_without_registry` (`:439`, `:466`), and re-load is `load_plugin` (`:161`). **The one real obstacle is that `ActivePlugin` owns a `wasmtime::Store` (`active_plugin.rs:10–14`) that cannot be swapped in place**, so a reload must be drop → re-`load_from_bytes` / `initialise` (`loader.rs:60`, `:111`) → re-register tools (`tool_bridge.rs:230`) — never an in-place module swap. There is **no module cache to invalidate**: the `Engine` is reusable and each load builds its own `Arc<Module>` (`loader.rs:68`, `:104`). **The real design question is RE-APPROVAL, and the machinery already exists** — ADR-37 hash-pinning means a changed `.wasm` loses its persisted grants automatically via prune-on-load (row 29's clause for the mechanism), so a reload should wire that existing behaviour rather than invent new grant machinery. **REMOTE LOADING — verified ABSENT, and recorded as a deliberate non-goal so it is not later mistaken for an oversight:** the loader only ever reads a **local file** (`loader.rs:52` `std::fs::read(wasm_path)`, then `:60` `load_from_bytes`), and the only http in the crate is **guest egress** — `host_fns.rs:437` `host_http_get`, gated by `check_url_allowed` against the grant allowlist (`:453`) — an outbound network *capability for plugins*, never a load source. Remote loading stays out of scope until the trust model in row 29 is decided, because a remote source has no local-file provenance story at all. **Re-entry: when plugin iteration during development warrants a watcher** (hot reload); remote loading is gated on row 29's trust-model decision. | S |
| 31 | L1/STM/PersonaMem memory items (typed extraction, scene memory, persona eval) | TODO.md:58–99 (L1 70–80, STM 65–70, heuristics 89–95, PersonaMem 96–99) | Post-memory relayout (ADR-63/64) | M |
| 34 | C-05/C-06/C-03 + M-08/M-05/M-02 audit cleanups | TODO.md:38–41 (C-05), 237 (C-06), 32–33 (C-03), 241 (M-08), 246 (M-05), 251 (M-02); AUDIT_FINDINGS_CURRENT.md | When module refactors + coverage are scheduled | L |
| 36 | Containerized sandbox bundle — `SandboxProfile::Containerized` OS-level isolation — **shipped 2026-09-26 for the container path (row STAYS OPEN: only the Windows path is outstanding)** | TODO.md:103; ROADMAP:235; security-threat-model.md §6 gap #1 (:310–315, "No Containerized Plugin Sandbox"); was a real stub — variant declared but not implemented (architecture.md:250, STATUS.md:272), plugins ran only under the WASM capability sandbox. **Landed 2026-09-26 in three slices:** `d00582b` (ADR-72 + enforceable core half — `core::sandbox` docker/podman runtime detection, `SimplePolicyEngine::check_sandbox` fail-closed admission gate, rules `sandbox_containerized_runtime_unavailable` / `_unenforceable`), `3ae6ea5` (shell invocations actually routed through the container — `tools::container` builds the `docker`/`podman run` argv from a planned `ShellPlan` as `ShellPlan::Direct`, opt-in `ShellTool::with_container`, `command_facts` audits the container argv), `fdf4800` (`CommandRouting` marker — closes the fail-open where `Containerized` was selected without routing) | **Windows path only.** Windows Job Objects need `unsafe` FFI (`windows-sys` `CreateJobObjectW` / `AssignProcessToJobObject`), which the workspace hard-denies (`[workspace.lints] unsafe_code`). ADR-72 §5 records v1 as **unsupported on Windows** (probe returns `Unavailable` → `Containerized` refused, fail-closed and honest), not shipped. Recorded options for a future Windows story: **(a)** a narrow, audited `unsafe` exception; **(b)** the safe high-level `windows` crate — safe Job Object bindings, no `unsafe` in our code, but a new dependency requiring its own ADR; **(c)** accept Linux/macOS-only with loud documentation. **Re-entry: a superseding ADR that chooses among (a)/(b)/(c).** | L |
| 37 | Hybrid UI full scope (tabbed Settings, Studio split pane, drag-and-drop agent assignment, focus-trap) — **rescoped 2026-09-27 per owner: Minimal + Medium tiers are LANDED, what remains is finish-and-polish with no correctness content. Row STAYS OPEN and deferred post-1.0** (size **L → M**) | **Full scope defined at `docs/hybrid-ui-plan.md:89`** — "All of Medium, plus split Settings into tabbed sub-views, convert Studio to split pane, add drag-and-drop agent assignment, animated panels, focus-trap system." **Citation corrected:** the row's old `TODO.md:145` had drifted onto an unrelated ADR-28 shell-profile note; the full-scope item is at `TODO.md:156–158` and `ROADMAP:147–149`; the landed medium scope is `TODO.md:150–155`. `docs/hybrid-ui-plan.md` is standalone, not part of the world-class plan | **LANDED — recorded so this row stops implying the hybrid UI is unbuilt** (the plan's own status list at `hybrid-ui-plan.md:306–317`; `TODO.md:150–155` for PR #49 Minimal and PR #97 Medium, merged 2026-08-03, commits `1c916b4` memory quick-panel, `4a12839` terminal bottom panel, `ade0c7b` glass modals + overlay/panel animations, `3d691a2` timestamps + transcript format v2, `f8c7b42` blinking cursor). Code anchors verified in the tree: the `SubView` overlay enum at `crates/desktop/src/views/chat.rs:23–25` (`Main`/`Diff`/`AgentGraph`/`ToolLog`/`SpendLog`/`Runtime`) with keyboard routing at `app.rs:1218–1222`; the shared animation layer at `app.rs:498` `ease_out_cubic` and `:4030`; the terminal as a toggleable bottom panel with drag resize (`app.rs:398` `terminal_panel_height`, `:2033–2046`, `:4030`); the memory quick-panel section (`views/quick_panel.rs`, `views/memory.rs`). **REMAINS**, as `crates/desktop/AGENTS.md:65–66` records: "⬜ State lifecycle: lazy init for infrequently used views" and "⬜ Full scope: tabbed Settings, Studio split pane, focus trap" — **plus drag-and-drop agent assignment, which is named in the plan (`:89`), `TODO.md:156–158` and `ROADMAP:147–149` but is *not* on the AGENTS.md ⬜ line** (that line names three items; the plan names four). Precisely, as of 2026-09-27: **(a) tabbed Settings — not done.** Settings is still one scrolling page of **9 collapsible sections** with a jump-sidebar (`views/settings/message.rs:372–382` `SectionId::ALL` = Theme, Providers, Assignments, Policy, Relationships, Retry, Memory, Shell, Extensions; rendered via `collapsible_section` at `views/settings/mod.rs:1049–1054`, sidebar at `:1060–1070`). The *Extensions hub* alone is tabbed, via `ExtensionTab` (`message.rs:342`, wired `mod.rs:1177–1201` — Skills / MCP / Plugins / ProjectContext), so there is no `SectionId::Skills`/`SectionId::Mcp` to tab away from: Skills and MCP are `ExtensionTab` variants inside `SectionId::Extensions`, not top-level sections. **(b) Studio split pane — absent** (Studio renders as a single surface, `views/orchestration_studio.rs`). **(c) drag-and-drop agent assignment — absent.** **(d) focus-trap system — absent.** **(e) lazy-init state lifecycle — absent.** **Size L → M, and why:** the reduction is earned, not optimistic — the two tiers that carried the hours are landed (Minimal ~20–30h and Medium ~60–90h per `hybrid-ui-plan.md:62`/`:75`), and every remaining item is layout/information-architecture polish with **no correctness, policy, or data-integrity content**, so the remainder is a finish pass rather than a design problem. **One caveat stated so "M" is not read as "free":** the plan's Medium item "SubView routing fully replaces `Page` for Chat-adjacent views" (`:83`) and its step 6 "Remove unused `Page` variants" (`:300`) are **not** done — `Page::DiffViewer` and `Page::ToolLog` are still routed and rendered (`app.rs:3973–3974`) and are still reachable from the context bar (`views/context_bar.rs:29–30`) and the quick panel (`views/quick_panel.rs:146`), so collapsing the `Page`/`SubView` dual path is real de-duplication work inside the remaining scope. **LAZY-INIT is the one remaining item with a felt, user-visible payoff** (it is the only one that changes startup cost and responsiveness rather than looks) and **may be pulled forward** ahead of the rest. Re-entry: post-1.0. | M |
| 38 | Flat/content-embedded tool-call parsing residual (beyond proxy Fixes 1–3) | TODO.md:190–195; ROADMAP:165–168 (residual after Fixes 1–3 landed, see Closed #8); docs/proxy-tool-call-fix.md | When sanitized proxy fixtures + pairwise verification land against real OpenAI-compatible proxies | S |
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

17. Code editor integration (external editor / open-in-editor) — **row #25 cut 2026-09-27 per owner decision.** The row covered the **external** editor track only: configurable external editors with file/line/column launch templates, plus an "Open in editor" handoff from diffs, tool logs, chat references and memory (`ROADMAP.md:188–191`; `docs/TODO.md:168–173`, which already read "**Not started** — no code exists". The register's own `TODO.md:157` cite had drifted onto the hybrid-UI item and is corrected here). **Verified absent, not assumed:** a search of `crates/` for `open_in_editor` returns **zero** hits, and there is no `$EDITOR`/`$VISUAL` wiring anywhere — the only `EDITOR` occurrences in the whole tree are an unrelated shell-env fixture in `crates/config/src/shell.rs:958–963` (test setup, not an editor launch). **Recorded honestly so the cut is reversible: nothing neighbouring the row went with it.** An in-app editor with syntax highlighting and LSP integration is landed — `crates/desktop/src/views/code_editor/mod.rs:1–7` (hierarchical file tree, multi-line editor with `iced_highlighter` highlighting, open/save/new/delete, and hover / diagnostics / go-to-definition via `concerto-lsp`), routed as `Page::Editor` (`app.rs:81`) and rendered at `app.rs:3999` — and a diff viewer is landed (`Page::DiffViewer` at `app.rs:79`, rendered `app.rs:3974`, widget in `widgets/diff_viewer.rs`). `TODO.md:172–173` already distinguishes the two features ("the studio `Editor` … is a different feature (in-app code editor)"). **If the external-editor handoff is ever wanted again it composes with the in-app editor; it does not replace it.** The in-app editor is what a file/line handoff would target, and an external protocol would be an additional launch path layered on top of it, never a substitute for it. This is a **scope** cut, not a claim that external editing is unwanted. Two stale lines left for a later docs pass, flagged not fixed: `ROADMAP.md:190–191` still frames the embedded editor as "only considered later if it clearly beats a reliable external-editor protocol" (the embedded editor has since landed, so the comparison it sets up no longer holds), and `TODO.md:172–173` cites `docs/desktop-cli-parity.md:103-104` for the `Editor` row, which is actually at `:96–97`.
18. ADR-58 P5/P6 + TOML diff + canvas DAG editor + multi-executor partitioning — **row #28 cut 2026-09-27 per owner decision**, on the grounds that the orchestration-studio UI is scheduled for a separate refactor and every item here would have to be re-specified against that new UI. **What the row covered, recorded so the cut is auditable** — ADR-58's own Deferred/Sequencing table (`docs/adrs/ADR-58-configurable-orchestration.md:238–247`): **P5 (pending)** = "Migration runner for legacy `multi_agent` configs; export-merge hardening" (`:243`); **P6 (deferred)** = "Graph/DAG config support; run-one-stage simulation; freeze decisions" (`:244`); **"Explicitly deferred"** = "TOML diff view of include changes; canvas DAG editor; multi-executor artifact partitioning (primary stays a plain flag)" (`:246`). ADR-59 carries the same tail (`docs/adrs/ADR-59-studio-blueprint-editor.md:172–177` — P5 migration runner + export-merge hardening; P6 "freeze/stable-surface decisions"; "Diff view and DAG canvas editor: post-P4 stretch, not planned here"), and `docs/STATUS.md:346–349` mirrors it as tracked follow-up #4. **State of each item, checked rather than assumed** — and it is *not* a uniform blank: **(a) the migration runner is genuinely ABSENT.** No code converts a legacy `multi_agent`-only config into an `[orchestration]` blueprint, and ADR-59:169–170 still states the pre-P5 condition as current ("legacy `multi_agent` remains authoritative only while `[orchestration]` is absent"). `crates/config/src/migration.rs` exists but is *schema_version* step migration (v1→v2 and so on, filling fields with defaults) — a different mechanism, not the P5 runner. **(b) export-merge hardening is substantially LANDED as a side effect, not absent** — `crates/config/src/saving.rs:1–40` documents exactly that: merge-aware `toml_edit` writers (`merge_edit_toml` preserving comments, key order and unedited keys), every writer atomic via temp-file-plus-`rename`, and `seed_orchestration_roster` / `save_agent_roster` writing only the keys they own and preserving the rest of the document byte-for-byte. This is the behaviour P5's "export-merge hardening" asked for; ADR-59:99 lists "post-seed `config.toml` wholesale rewriting" as a **non-goal**, which the merge-aware writers are the reason for. **(c) Graph/DAG *config* support, (d) run-one-stage simulation, (e) the Studio-level freeze / stable-surface decision, (f) the TOML diff view, (g) the canvas DAG editor and (h) multi-executor partitioning are all ABSENT — no implementation and no scaffold.** Searches of `crates/` for `run_one_stage`, `freeze_decision`, `export_merge`, `migration_runner`, `multi_executor`, `executor_partition`, `toml_diff`/`TomlDiff`, `stable_surface`, `SimpleTier`/`simple_tier` return **zero** matches. The `DAG` and `Freeze` symbols that *do* exist are a different, already-landed subsystem and are **not** this row: `orchestrator/src/graph.rs` + `scheduler.rs` + `task_transform.rs` are the runtime `SubTask` task-DAG, including a real `TaskTransformSpec::Freeze` (`task_transform.rs:163–189`, `apply_freeze` at `:542–618`) behind the model-driven "reconsider" decision. None of that is a *config-authorable* graph, a run-one-stage simulation, or a config-surface freeze; the Studio remains a roster/relationships/blueprint CRUD editor (`views/orchestration_studio.rs`). One source inconsistency recorded, not fixed: `docs/STATUS.md:347` labels a "**Studio Simple tier**" as part of P5, but ADR-58's revised decision demoted the tier notion entirely (the blueprint catalog is "advisory data", seeds are materialized into user config) and no "Simple tier" string exists in either the ADR or the code. **This cut is a scoping decision for a UI plan that is being replaced — it is NOT a claim that these capabilities are unwanted.** The P5 migration runner in particular is real standing debt independent of any UI, and after the studio refactor its natural home is whatever that new UI is specced against; if it is wanted before then, it is a config-layer task with no UI dependency at all.

19. Per-session ack queue (bounded depth + ack policy) — **row #4 closed 2026-09-27; landed in `8e3c350`.** ADR-68 §6 (`docs/adrs/ADR-68-h04-session-ack-breaking-param.md:78–86`) recorded only the *policy* — queue a second ack behind the active one, bounded at "1 pending ack beyond the active one", with overflow rejecting via an explicit error — and explicitly deferred the implementation to "the implementation phase". That phase is now the shipped state, and it shipped in the direction §6 chose rather than the opposite one an earlier register note recorded. `SharedPendingAck` is no longer a single `Option` but a depth-bounded FIFO, `Arc<Mutex<VecDeque<PendingAck>>>` (`desktop/src/widgets/capability_dialog.rs:137`), with new `enqueue_ack` (`:151`) and `ack_queue_position` (`:180`), and `ack_view` / `resolve_ack` retargeted to the queue front. **The bound is `MAX_PENDING_ACKS = 2`** (`:98`, rationale on the constant at `:84–97`) — the active dialog plus one queued, which is the bound ADR-68 §6 names, and deliberately tiny because an ack is a *blocking, user-facing* confirmation: one active is what the user can actually read, the single queued slot stops a concurrent prompt from overwriting the first, and a deeper queue would only delay the *second* prompt past the point where its requester is still waiting while letting an ack storm grow UI state without bound. **Overflow is an explicit rejection, never a silent drop.** `AckQueueError` (`:105–114`) has exactly the two fail-closed cases — `QueueFull { capacity }` when the bound is already reached (the new request never displaces an already-pending ack), and `StateUnavailable` when the shared lock is poisoned (the desktop cannot prove the request was recorded, and must not report "no pending acks" for a request that was actually made). The sink maps either to `false` (abort the task) **and** publishes a fresh `DesktopEvent::ErrorOccurred` (`app.rs:4776–4778`), which the app renders as an error toast — so *shown*, *queued* and *refused* are three distinguishable user-visible outcomes instead of one silent no-op. **Session gating is preserved through the queue:** `resolve_ack` pops the front entry and answers it only when its `session_id` matches the caller, restoring a non-matching entry to the front so its owning session still sees its dialog (`capability_dialog.rs:233–246`) — a stale or cross-session entry can never answer another run's prompt, and because resolution drains in order the queued second ack cannot jump the active first. **Dialog indicator:** `ack_queue_position` returns `None` for ≤1 pending, so the single-ack modal layout is byte-for-byte unchanged, and `Some("Acknowledgement 1 of N pending")` for N≥2 (`:180–185`), inserted into the modal only in that case (`:207–213`) so the user can see they have N outstanding acknowledgements rather than believing the displayed one is the only prompt. Supporting call sites: `DesktopApprovalSink::request_ack` enqueues through `enqueue_ack` (`app.rs:4760`) and the status-bar dialog-waiting check moved from `is_some()` to `!is_empty()` (`views/status_bar.rs:58`). **Audit-seam reachability was a finding, not a gap — stated plainly because it is the one part of the row whose work was smaller than the row implied.** The production wiring already existed and predates this commit: `AgentLoop::setup_undo_stash` (`orchestrator/src/agent_loop.rs:1225–1227`) → `ToolExecutionBackend::record_ack_decision` → `InProcessGateBackend` → `ToolExecutor::record_ack_decision`, attributable by `git log` to `26e3fc9` ("ADR-66 phase 1"). The deliverable was therefore a **reachability test** — `in_process_gate::tests::record_ack_decision_writes_request_continue_and_abort_rows` (`orchestrator/src/in_process_gate.rs:554`) — proving the seam writes a `RequestContinue` row for a continuing ack and a `RequestAbort` row for a refused one; alongside it, a now-false doc comment on `ToolExecutor::record_ack_decision` claiming that "nothing calls this in production" was corrected to name the real call chain (`core/src/executor.rs:298–303`). **Residual, recorded rather than dropped: an overflow refusal is audited as `RequestAbort`.** The `ApprovalSink::request_ack` contract returns only `bool`, so the sink cannot hand the audit seam a reason — in the trail an overflow refusal is separable from a user cancel **only by message text, not by a distinct verdict**. Closing that needs a widened `ApprovalSink::request_ack` (a typed decision instead of a bare `bool`), a contract change reaching every sink, and is explicitly out of scope for this commit; it is recorded here so the closure is not read as claiming full audit fidelity for the refusal path. The `#4` numbering gap is left intentionally (no renumbering), matching the row 10 / 12 / 14 / 19 / 21 / 25 / 26 / 28 / 33 / 44 / 47 / 49 precedent.

20. Certified evolution (profile-guided CI, safety gates) — **row #32 cut
    2026-09-27 per owner decision.** The row's whole subject is a research
    programme, and the dangling cite that made it a bad register row is
    **explained, not accidental**. `docs/research/certified-universal-evolution.md`
    does not exist on this checkout, and it *cannot* be committed: `.gitignore:54`
    ignores exactly that path, under the comment on `:53` — "**Private research —
    not for public until ready**". `git check-ignore -v` confirms
    `.gitignore:54` is the matching rule, and `docs/research/` holds six tracked
    files, none of them this one. So the register — and `TODO.md:298` and
    `ROADMAP.md:193` alongside it — has been citing a document that is
    deliberately withheld from the repository. A reader could not re-derive the
    scope or ever close the row from what is in the tree. **Two of the row's own
    cites had also drifted off the item, and are corrected here:** `TODO.md:286–293`
    is the **prompt-cache-stability** item (row 1's subject), not this one — the
    certified-evolution item is at `TODO.md:297–304` with its prerequisite list
    at `:300–303`; and `ROADMAP:189–193` straddles the **code-editor** item
    (`:188–191`, cut as Closed #17) and only the first two lines of this one —
    the item is `ROADMAP.md:192–196`.
    **No scaffold exists, but two of the brief's "absent" premises were false and
    are recorded corrected rather than repeated.** (a) **`confidence` is not
    unique in the tree** — a case-insensitive search of `crates/**/*.rs` returns
    **85 matches**. What is absent is the *construct*, not the *name*: every
    occurrence is a bare `f32` scalar used for routing, recall ranking, or
    consultation triggering — the intent-routing `RouterOutput::confidence` and
    `LOW_CONFIDENCE_THRESHOLD` (`core/src/intent.rs:15,135`; ADR-55 vocabulary
    whose constant the ADR-56 LLM classifier reuses as its re-route threshold,
    configured at `config/src/schema.rs:239,269–305`), memory-decision confidence
    with a `0.0..=1.0` CHECK constraint and supersession tracking
    (`core/src/memory.rs:303,313`; `memory/src/decision_store.rs`,
    `memory/src/entities.rs:78,804–805`), and the issue-#59 deterministic
    low-confidence consultation trigger (`orchestrator/src/consultation.rs:52,292`).
    None of them is a *type*, and none carries evidence — so "compiler-enforced
    confidence/evidence types" is genuinely unbuilt, but a claim of "no
    `confidence` in the tree" would have been falsified by a single grep and is
    deliberately not made. (b) **`evidence` is not absent either.** ADR-65's
    evidence spine is a large landed subsystem — `AcceptanceEvidence`
    (`orchestrator/src/checkpoint.rs:69–103`) plus `design_doc_verifier.rs`,
    `resume.rs`, `world_model.rs`, `tool_facts.rs`, `read_cache.rs`. What is
    missing is the certified-evolution *meaning* of it: a **type-level gate on
    promoting a self-modification**, not a record of what happened during a run.
    The vocabulary collides; the construct does not exist.
    **The neighbours are not it — checked rather than assumed.** `crates/eval` is
    a real, substantial Phase 3 harness (`lib.rs:5–8`, `categories.rs`,
    `scenarios.rs`, `persona_mem.rs`, ~30 task fixtures across six categories),
    but what it does is detect the project's own test runner (`cargo`, `npm`,
    `pytest`, `make`; `lib.rs:7`) and run that suite in a working directory
    (`run_in_dir`, `:267–274`; fallback path `:291–294`) — no isolation, no
    immutability, no evaluator-spec validation, no candidate-vs-baseline
    comparison. It is an agent-task harness, not an evolutionary evaluator. The
    seven benches are criterion micro-benchmarks (`core/benches/serde.rs`,
    `core/benches/policy.rs`, `orchestrator/benches/task_graph.rs`,
    `memory/benches/vector_retrieval.rs`, `memory/benches/fts_search.rs`,
    `providers/benches/provider_streaming.rs`, `tools/benches/virtual_fs.rs`) —
    no profile-guided CI gate, and row 48 already carries the Phase 3
    criterion-benchmark + CI-gate item. `orchestrator/src/fault_injection.rs:76–92`
    is the G1–G3 issue-gate rows plus the C1–C4 crash-window table — row 17's
    in-process, deterministic runtime-robustness suite, not an evolution
    evaluator. A search of `crates/**/*.rs` for `evaluator spec` / `profile.guid`
    / `evolution` / `self.improv` returns no evaluator-spec type and no promotion
    gate; the `promote`/`mutation` hits are all file-write policy. **`STOKE`
    occurs in exactly two places repo-wide**, both prose in the two cited lines
    (`ROADMAP.md:196`, `TODO.md:303`), and zero times in code — so no
    STOKE-comparable evidence exists.
    **What it would have required**, verbatim from `TODO.md:300–303` and
    `ROADMAP.md:192–196`: compiler-enforced confidence/evidence types; evaluator
    specification validation; deterministic isolated **immutable** evaluation
    infrastructure; explicit resource budgets with stop/resume semantics;
    small-domain evidence comparable to STOKE. Of those five, the tree has at
    most one *adjacent* item — resource budgets, via rows 36/45's CPU budget and
    the existing wall-clock timeout — and that budget bounds a shell command, not
    an evolution run. **Why it is cut:** `TODO.md:303–304` says the track's own
    status outright — "STATUS.md does not cover this track; it is roadmap-scoped
    research, not a promised feature." It is a research programme, not a backlog
    item, and a row whose re-entry condition names a withheld document is
    unresolvable by construction: leaving it open with an unresolvable cite is
    worse than removing it. **The cut is reversible and nothing is lost:** the
    track's prerequisites remain written down in `TODO.md` and `ROADMAP.md` in
    the repository, and if the research lands it returns as a **new row with a
    real in-repo source**. No other register row duplicates this track.

21. STATUS-tracked follow-ups (ENV_LOCK, glyphs, multiline, ADR-59/P4
    deferrals, orchestration-editor checklist, release-priority matrix) — **row
    #35 cut 2026-09-27 per owner decision; a RECLASSIFICATION, not a
    cancellation.** These are **not deferred engineering work**. They are
    per-release *human checklists* that hang off two living forms, because no
    automated check can verify rendering or layout: **`TESTING.md`** (290 lines)
    opens "Use one copy of this sheet per build, operating system, and
    provider/model combination. Mark each result **Pass**, **Fail**, **Blocked**,
    or **Not tested**" (`:1–6`) and carries a 14-field environment table
    (`:12–29`), the automated-checks result table (`:53–60`), the acceptance-bar
    section mapping the audit's 12 end-to-end scenarios to named automated tests
    (`:62–92`), and the area sheets — default desktop, multi-agent, cancellation,
    memory restart/projects, spend chip and Spend Log, shell and policy, skills
    and MCP (`:113–257`); and **`docs/live-test-template.md`** (76 lines), the
    same per-build/OS/provider form with the same four verdicts (`:1–7`), the
    environment table, nine generic key-test rows plus the six Studio rows
    (`:42–47`), the automated-checks block and table (`:49–66`), and the
    outcome/funding-notes form. Because a **fresh sheet is filled in per
    build/OS/provider combination**, these have no terminal state: they will
    never reach zero, and "reach zero" is not a concept that applies to them.
    Carrying them in a register of *deferred work* — with a re-entry condition,
    a size, and an implied owner — mislabelled living checklists as debt.
    **Current state of each, checked in the tree today so nothing is lost — all
    six are OPEN.**
    1. **`CONFIG_ENV_LOCK` env-restore race hardening — OPEN.** `STATUS.md:328–333`
       asks for restore-before-assertion or an RAII guard. The lock exists
       (`crates/desktop/src/app.rs:4943`, `CONFIG_ENV_LOCK`, documented as
       mirroring `PROJECT_ROOTS_ENV_LOCK` in concerto-config and `ENV_LOCK` in
       concerto-cli), and the panic-safe pattern is real and written down at
       `app.rs:8028–8033` ("restore the env BEFORE any assertion so a panic
       cannot leak the redirect"). Most guarded tests do follow it —
       `first_studio_open_auto_seeds_the_orchestration_roster` (guard `:5223`,
       restore `:5245–5249`), the `:5308` test (`:5330–5334`),
       `startup_config_load_failure_marks_config_broken` (`:5365`, restore
       `:5377–5380`), `navigate_to_studio_changes_page` (`:6839`, restore
       `:6845–6848`), and the global-key import-refusal test (`:8632`, restore
       `:8654–8657`). **It is not applied universally, which is exactly the
       finding:** `save_materializes_a_name_selection_inline_into_the_global_config`
       asserts at `:5565` while its restore sits at `:5598–5599`, and the test
       guarded at `:5705` asserts at `:5733–5736` before restoring at
       `:5742–5745`. An assertion panic in either unwinds past the restore and
       leaks the `XDG_CONFIG_HOME` redirect into parallel tests. No RAII guard
       type exists in the module.
    2. **Glyph-font coverage — OPEN, cosmetic and text-paired.** `STATUS.md:334–340`.
       The glyphs `🛡 ➜ ⛓` are `semantics_glyph`
       (`views/orchestration_studio.rs:909–915`), paired with `semantics_label`
       text (`:917–926`) in both the kind-picker options and the row's semantics
       tag, under a doc comment at `:907` that the affordance is "never color
       alone". Still no bundled icon font and no explicit iced fallback; the
       worst case is cosmetic tofu, which is why `STATUS.md` ranks the options
       (swap orphan glyphs for text **S** / symbol fallback font **M** / bundle
       an icon set **L**).
    3. **Multiline system-instructions — OPEN.** `STATUS.md:341–345`. The
       fallback-persona input is still a single-line
       `text_input("System Instructions", …)` at
       **`views/orchestration_studio.rs:4114–4116`**, not upgraded to the iced
       `text_editor`, with no edit plumbing and no tests. **Citation correction:**
       the framing pointed at `app.rs:4114–4122`, which is the SubView overlay
       title bar — close button at `:4113–4115`, the
       `Main`/`Diff`/`AgentGraph`/`ToolLog`/`SpendLog`/`Runtime` title match at
       `:4117–4124` — and has nothing to do with the input. The real site is in
       the Studio view.
    4. **ADR-59 / P4 deferrals — OPEN, and two of the five are already tracked
       elsewhere.** `STATUS.md:346–349` (tracked follow-up #4) mirrors
       `ADR-59:172–177`: P5 migration runner + export-merge hardening, P6
       freeze/stable-surface decisions, and "Diff view and DAG canvas editor:
       post-P4 stretch, not planned here". The **TOML diff view and canvas DAG
       editor (P6)** are already recorded in the now-cut row 28's Closed #18
       entry — cross-referenced here rather than duplicated. Of the rest,
       **export-merge hardening substantially LANDED** as a side effect of the
       single-arm Save work: `crates/config/src/saving.rs:1–40` documents
       merge-aware `toml_edit` writers that preserve comments, key order and
       unedited keys, all atomic via temp-file-plus-`rename`, with
       `seed_orchestration_roster` / `save_agent_roster` writing only the keys
       they own. The **migration runner is genuinely absent** — no code converts
       a legacy `multi_agent`-only config into `[orchestration]`, ADR-59:169–170
       still states the pre-P5 condition as current, and
       `config/src/migration.rs` is *schema_version* step migration, a different
       mechanism. Run-one-stage simulation and the freeze/stable-surface decision
       are likewise absent with no scaffold. `STATUS.md:347`'s "Studio Simple
       tier" label still has no counterpart in ADR-58 or in the code.
    5. **Orchestration-editor manual checklist — OPEN.** `STATUS.md:350–355`
       names six rows added to `docs/live-test-template.md`; they are present at
       `:42–47` with their Result cells blank. "Run before the next release;
       automated tests cannot see rendering/layout issues" is the entire reason
       this is a form rather than a task.
    6. **Release-priority matrix — OPEN.** `STATUS.md:357–365`, "Immediate
       release priorities", items 1–5. **Note the boundary:** this section sits
       *outside* the "Tracked follow-ups" block (`STATUS.md:323–355`), so the
       row's own `~328–355` cite never actually covered it. Recorded here
       explicitly so it is not lost along with the row.
    **The living checklists REMAIN the source of truth after this cut.** Nothing
    was deleted from `TESTING.md`, `docs/live-test-template.md`,
    `docs/live-test-*.md`, or `docs/STATUS.md` — this pass edited only
    `docs/DEFERRED.md`. Every item above is still written down, in the place a
    release engineer will actually look, and the removal from this register is a
    **reclassification** out of "deferred work with an owner and a size", not a
    cancellation of any item.

22. Binary installers (deb/rpm/tar) — **row #39 cut 2026-09-27 per owner
    decision, on intent "we will do that eventually" — a CUT, not a
    cancellation, and resumable without re-investigation.** The row read as a
    blank ticket; it is not, and the reason for cutting it is that **the
    release pipeline it was waiting for already exists.** The honest summary is
    *"a working tag-triggered cross-platform release pipeline is built and has
    never been run; everything downstream of a raw binary is absent."*
    **Two of the row's own cites had drifted off the item and are corrected
    here.** Its `TODO.md:260–263` is the **audit M-05 oversized-module** item
    (`:257–261`), not installers; the real entry is **`docs/TODO.md:271–274`**
    ("**Binary installers (deb/rpm/tar).** Not started — `docs/STATUS.md:11-14`
    … today only a `.tar.gz` release build exists (`scripts/release.sh`)").
    Its `ROADMAP:194` is a line **inside the certified-evolution item**
    (`:192–196`, since cut as Closed #20 — so both release rows were citing
    another closed row's source); the real line is **`ROADMAP.md:197–198`**
    ("**Binary installers and crates.io publishing:** currently only a `.tar.gz`
    release build exists"), and note it covers row 40 as a single combined
    bullet. `STATUS.md:11–14` is correct and untouched.
    **BUILT (more than the row implied).** A real pipeline exists and was
    verified end to end by reading it: `.github/workflows/release.yml` triggers
    on `v*` tag pushes (`:13–16`) with `permissions: contents: write` (`:18–19`);
    its build job is a **4-target matrix** (`:32–45`) — `x86_64-unknown-linux-gnu`
    (`:34–36`), `x86_64-pc-windows-msvc` (`:37–39`, `.exe` suffix),
    `aarch64-apple-darwin` (`:40–42`), `x86_64-apple-darwin` (`:43–45`) — with
    `fail-fast: false` (`:31`) so one platform's failure still reports the full
    picture; it installs the iced/wgpu Linux graphics libraries (`:55–62`), builds
    `cargo build --release --target … -p concerto` (`:70`), stages a per-target
    artifact (`:72–83`), and a second `release` job (`:85–102`) publishes via
    **`softprops/action-gh-release@v2`** with **`generate_release_notes: true`**
    (`:98–102`). The local path is `scripts/release.sh` (54 lines):
    `cargo build --release --workspace` (`:13`), a tarball named
    `concerto-${VERSION}-${OS}-${ARCH}.tar.gz` (`:29`, built at `:31–33`) plus a
    `.sha256` sidecar (`:35–37`), and an **OPTIONAL** `cargo deb` guarded by
    `command -v cargo-deb` that warns-and-skips rather than failing (`:39–48`).
    **The pipeline has never been exercised — this is the single most important
    fact for anyone resuming it.** The repo currently has **zero git tags**, so
    the `v*` trigger has never fired, the matrix has never been proven on any of
    the four targets, and the "every target succeeds" precondition on the
    release job is untested. `docs/TODO.md:271–274` calls the state "only a
    `.tar.gz` release build exists", which understates `release.yml` — and
    `release.sh` and `release.yml` are **two independent, unreconciled paths**
    (workspace-wide vs `-p concerto`; tarball+checksum vs raw per-target binary;
    optional deb vs nothing) with no single documented release procedure.
    **GENUINELY MISSING, checked rather than assumed.** (a) **No packaging
    definitions of any kind are committed.** A repo-wide search for `cargo-deb`,
    `package.metadata.deb`, `cargo-rpm`, `metadata.rpm`, `NSIS`, `WiX`,
    `AppImage`, `.desktop`, `winget`, `scoop`, `homebrew`, `flatpak` and
    `snapcraft` returns hits in exactly **two** places: the four `cargo-deb`
    lines in `scripts/release.sh` (`:3`, `:39`, `:40`, `:47`), and one **prose**
    line in a research brief, `docs/research/ai-native-shell-research-brief.md:34`
    ("Cross-platform distribution: Homebrew, Winget, Cargo, Nix, pre-built
    binaries"), which is a wishlist sentence, not a definition. So: no
    installer definition exists, and the `cargo deb` call has **no
    `[package.metadata.deb]` table behind it** — it would produce nothing as
    written. (b) **No code signing anywhere.** A search for `codesign`,
    `notarytool`, `notariz`, `signtool`, `cosign`, `GPG_KEY`, `apple-id` and
    `import-signing` returns **zero matches repo-wide** — not in `release.yml`,
    not in `release.sh`. macOS and Windows artifacts are therefore unsigned, and
    a notarial/gatekeeper story is absent. (c) **No update channel**, and the
    precise state is better and worse than "none exists": `crates/cli/src/update.rs`
    is a **notification-only** check whose own module doc says "Never blocks
    startup, **never auto-downloads**" (`:1–5`); it fires a 2-second-timeout GET
    (`:17`, `:59–61`) at the hardcoded crates.io API endpoint for a `concerto`
    crate (`:14`) and only `info!`-logs a newer version (`:23–43`). It is
    **genuinely wired** — `crates/cli/src/lib.rs:182–185` gates it on
    `config.updates.check_on_startup` and calls it — and an `[updates]` config
    section exists (`crates/config/src/schema.rs:932–942`: `check_on_startup`
    defaulting to **true** at `:945–947`, and `update_endpoint: Option<String>`
    at `:941–942` documented "`None` = use crates.io API"). **But
    `update_endpoint` is read by no code at all** — the only reference outside its
    own definition is the re-export at `config/src/lib.rs:63`, while
    `update.rs:14` hardcodes the crates.io URL. So the custom-endpoint seam is
    **declared, documented and example'd (`docs/config.toml.example:237–241`,
    which ships `check_on_startup = false` and a commented
    `update_endpoint = "https://example.invalid/concerto/version"`) but
    unwired** — the same defect shape this register already flags for row 45's
    `ShellConfig::cpu_budget_secs`. With a default config the check therefore
    fires against a crate that is not published; `fetch_update` (`:64–90`) never
    checks `resp.status()`, so the 404's JSON body instead fails at the
    `newest_version` lookup (`:86–90`) and the error arm logs at **`warn!`**
    (`:38–40`), not `error!`. (d) **Two build targets are missing**: no
    `aarch64-unknown-linux-gnu` and no arm/aarch64 Windows — a notable gap given
    `AGENTS.md` records a Raspberry-Pi ARM host as a dev machine. (e) **The
    desktop GUI is not separately packaged**, and this is a design fact rather
    than a gap: `crates/concerto/Cargo.toml:11–14` sets `default = ["desktop"]`
    with `desktop`/`cli` as optional feature-gated deps (`:13–14`), and
    `release.yml:70` builds `-p concerto` with **defaults**, so the Iced GUI
    **rides inside the single `concerto` binary**. There is no second artifact,
    no `.app` bundle, and no installer for the GUI distinct from the CLI.
    **How a user obtains a binary today: by building from source, only.**
    `README.md:150–164` documents `cargo run -p concerto-desktop --release`
    (`:150`), `cargo run -p concerto-cli --release` (`:156`) and the frontend
    selector `cargo run -p concerto -- --desktop` / `--features cli -- --cli`
    (`:162–164`) — `cargo run`, never a download; and `docs/STATUS.md:11–14` is
    explicit that "Source builds and the automated workspace checks are the
    supported distribution path" and that the project "does not currently
    promise binary installer packages" (`:14`). **Recorded so the cut is
    reversible:** everything the work needs is still written down here and in
    `docs/TODO.md:271–274` / `ROADMAP.md:197–198`; the packaging decision that
    remains is *which* installer formats and whether a signing story is funded,
    and the mechanical baseline is a pipeline that builds and publishes
    unsigned raw binaries but has never been executed.

23. crates.io publish — **row #40 cut 2026-09-27 per owner decision, on intent
    "we will do that eventually" — a CUT, not a cancellation, and resumable
    without re-investigation.** The row's own re-entry condition ("Release
    decision + metadata audit") named a blocker that **does not exist**; the
    audit below is the resumption record, and it also records the decision that
    was implicitly made by writing `publish = false` and never revisiting it.
    **The same two drifted cites as row 39 apply** and are corrected here: its
    `TODO.md:264–265` is the **audit M-02 decorative-cancellation** item
    (`:262–267`), not publishing; the real entry is **`docs/TODO.md:275–276`**
    ("**crates.io publish.** Not started — `docs/STATUS.md:12-13` (not published;
    source builds and workspace checks are the supported path)"), and its
    `ROADMAP:194` is likewise inside the certified-evolution bullet
    (`:192–196`, Closed #20) — the real cite is the combined
    **`ROADMAP.md:197–198`**. `STATUS.md:12–13` is correct and untouched.
    **The flip itself is one line, and this is verified.** `publish = false` is
    declared exactly once, in `[workspace.package]` at **`Cargo.toml:37`**, and
    every member inherits it with `publish.workspace = true` in its `[package]`
    block (`crates/core/Cargo.toml:6`, `crates/concerto/Cargo.toml:6`, and the
    same line in each of the others). The workspace has **25 members**
    (`Cargo.toml:4–28`; `AGENTS.md` agrees at "25 crates") — *not* 26, which is
    worth stating so a resuming agent does not go looking for a 26th.
    **Two of the row's named blockers are absent, and should not be
    re-litigated.** (a) **Licence is not a blocker**: `license = "MIT OR
    Apache-2.0"` (`Cargo.toml:35`) is inherited by every crate, and
    `deny.toml:101–104` allows both (`"MIT"`, `"Apache-2.0"`, alongside
    `Apache-2.0 WITH LLVM-exception`, BSD-2/3, Unicode-3.0, ISC). (b) **There is
    no unpublished-dependency blocker**: a search for `git = ` across
    `crates/*/Cargo.toml` returns **zero** hits, and every internal `path =`
    points at `../<crate>`. The only non-`../` `path =` lines in the workspace
    are four `[[bin]]`/`[[bench]]` *target* paths, not dependencies
    (`eval-runner:34`, `mcp:29`, `orchestrator:57` and `:61`). So nothing
    outside the workspace would need publishing first.
    **The real question is the crate SUBSET, because the graph is coupled —
    this is the finding that makes the row worth cutting rather than merely
    deferring.** Publishing "everything" is not one line, it is publishing the
    whole coupled graph at `0.1.0` with unstable APIs: `crates/desktop/Cargo.toml:12–24`
    declares **13** internal dependency edges (all correctly carrying
    `version = "0.1.0"` alongside `path`); `crates/eval-runner/Cargo.toml:12–19`
    pulls in **8** internal crates; `crates/plugins/Cargo.toml:13–14` exposes
    `concerto-core` and `concerto-api-types`. **Correction to one framing that
    is easy to assert and wrong:** desktop's *public* surface does **not**
    uniformly name orchestrator/memory types. What is public is
    `pub bus: concerto_core::event::EventBus` (`app.rs:303`, inside
    `pub struct App` at `:278`), `pub cancel_token: concerto_core::CancellationToken`
    (`:317`), `pub config`/`pub global_config` as `concerto_config::AppConfig`
    (`:304`, `:307`), `pub git_summary: Option<concerto_tools::git::RepositorySummary>`
    (`:426`), `pub plugin_manager: Option<concerto_plugins::manager::SharedPluginManager>`
    (`views/settings/state.rs:176`), and two `pub fn` signatures
    (`views/diff.rs:105` takes `&mut VirtualFs` → `concerto_core::ToolError`;
    `views/tool_log.rs:102` takes `&[concerto_sessions::replay::StoredEvent]`).
    `concerto-orchestrator` and `concerto-memory` **are** depended on
    (`:14`, `:13`) and used, but through **private** `use` statements
    (`app.rs:35–40`, `services/session_handler.rs:16`) — so the coupling is real
    but it is a *dependency-edge* coupling, and "the public API takes
    orchestrator/session/memory types" would not survive one grep. **The
    decision this cut implicitly records: publish the `concerto` binary — and
    possibly `concerto-plugin-sdk`, which is the one genuinely reusable public
    surface — and keep the remaining 23–24 crates `publish = false`.** That is
    the only option that publishes something usable without freezing the entire
    internal API at `0.1.0`.
    **Mechanical work, recorded so it is not rediscovered.** (i) **20 of the 25
    crates have no `description`**, which crates.io hard-requires. Exactly five
    have one: `concerto-plugin-sdk` (`plugin-sdk/Cargo.toml:3`) and the four
    `test-*-plugin-wasm` crates (each `:3`). (ii) **`repository` is *defined but
    never inherited*** — this is the single most misleading item in the row. The
    workspace sets `repository = "https://github.com/NefaroXX/Concerto"` at
    **`Cargo.toml:36`**, but **no crate opts in**: every `[package]` block is
    exactly `name` / `version.workspace` / `edition.workspace` /
    `license.workspace` / `publish.workspace` (e.g. `core/Cargo.toml:2–6`), with
    no `repository.workspace = true`. So the value is already written and a
    resuming agent needs only one added line per crate, not a new URL. No crate
    has `homepage`, `readme`, `documentation`, `authors`, `keywords` or
    `categories` at all. (iii) **8 internal dependency edges across 6 crates
    carry `path` with NO `version`**, which crates.io rejects: the optional
    `concerto-cli` / `concerto-desktop` deps (`crates/concerto/Cargo.toml:17–18`),
    `concerto-core` / `concerto-config` (`crates/observability/Cargo.toml:23–24`),
    and all four `test-*-plugin-wasm → concerto-plugin-sdk` (`:13` in each).
    (iv) **The remaining 73 internal edges hardcode `version = "0.1.0"` as an
    inline literal**, and internal crates are **absent from
    `[workspace.dependencies]` entirely** (no `concerto-*` entry in
    `Cargo.toml:60–204`; no crate uses `concerto-x.workspace = true`). So
    bumping the single-sourced `version` at `Cargo.toml:32` does **not**
    propagate: 73 literals plus 8 path-only edges need lockstep manual editing.
    (v) **The `test-*-plugin-wasm` crates should be excluded regardless of the
    subset decision** — they are `crate-type = ["cdylib"]` **test fixtures**
    (`:10` in all four, alongside `description` at `:3`), built only for the
    `wasm32-wasip2` integration tests. Publishable in principle, meaningless in
    practice.
    **Supporting process state.** Version is single-sourced at `Cargo.toml:32`
    (`0.1.0`) and inherited everywhere. `CHANGELOG.md` (668 lines) is
    Keep-a-Changelog + SemVer with the preamble at `:1–6` and a single
    `## [Unreleased]` section at `:8` — **no released version has ever been
    cut**, consistent with the zero git tags row 39 records. `CONTRIBUTING.md`
    has **no release-process section** (its headings are Development setup,
    Looking for a first contribution?, Before opening an issue, Branches/
    commits/pull requests, Required checks, Code expectations, Tests, Quality
    gates, Architecture decisions, Documentation); "release" appears only at
    `:135` and `:155` as pointers to `TESTING.md`.
    **One dead-behaviour note, corrected from an easy misreading:**
    `crates/cli/src/update.rs` is **not unreferenced code** — it is wired at
    `crates/cli/src/lib.rs:182–185` behind `check_on_startup`. What is inert is
    its *target*: it queries `https://crates.io/api/v1/crates/concerto`
    (`:14`) for a crate this workspace does not publish, so the check cannot
    succeed until a publish actually happens (and `update_endpoint` stays
    unwired either way, per Closed #22(c)). **Recorded so the cut is
    reversible:** the flip, the subset decision, and the whole metadata audit
    are written down above; resuming means choosing the subset, adding
    `description`/`repository` inheritance, and fixing the 8 versionless edges —
    not re-investigating why the row existed.

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

## Verification notes (2026-09-27, owner decisions pass)

Owner decisions of 2026-09-27 covering rows 4, 25, 28, 37, 9 and 29, executed
2026-09-27 against `fix/coordinator-error-invariant` at HEAD `2cada54`. Every
claim below was read in the tree or in the cited source document, not taken from
a commit message. **Docs only — no source edits, no builds, no test runs, no
commit**; the tree is left for the orchestrator to commit.

- **Scope of this pass: rows 25 and 28 cut, row 37 rescoped, row 9 corrected,
  row 29 annotated-only, row 4 read-only.** Two rows leave the open table and
  land in the Closed appendix as #17 and #18. No renumbering: the open-table
  gaps are now 10, 12, 14, 19, 21, **25**, 26, **28**, 33, 44, 47, 49, all left
  intentionally, matching the row 10 / 12 / 14 / 19 / 26 precedent.
- **Row 25 cut** (see Closed #17). The external-editor integration is verifiably
  unbuilt — zero `open_in_editor` hits under `crates/`, no `$EDITOR`/`$VISUAL`
  wiring (the only `EDITOR` strings in the tree are a shell-env test fixture at
  `config/src/shell.rs:958–963`). What the cut explicitly does **not** touch is
  recorded in the appendix so the decision is reversible: the in-app editor is
  landed (`views/code_editor/mod.rs:1–7` — file tree, `iced_highlighter`
  highlighting, file ops, and LSP hover/diagnostics/go-to-definition through
  `concerto-lsp`; `Page::Editor` at `app.rs:81`, rendered `app.rs:3999`) and the
  diff viewer is landed (`Page::DiffViewer` at `app.rs:79`, rendered `app.rs:3974`).
  Two citation problems surfaced and are fixed in the appendix: the row's
  `TODO.md:157` had drifted onto the hybrid-UI item (the editor entry is
  `TODO.md:168–173`), and `TODO.md:172–173` in turn cites
  `docs/desktop-cli-parity.md:103-104` for the `Editor` row, which is actually at
  `:96–97`. Also flagged, not fixed (outside this pass): `ROADMAP.md:190–191`
  still frames the embedded editor as a maybe-later alternative to an external
  protocol, which no longer holds now that the embedded editor has shipped.
- **Row 28 cut** (see Closed #18). The cut is justified by the owner's reason —
  the orchestration-studio UI is scheduled for a separate refactor and every
  item would be re-specified against it — and the appendix records ADR-58's own
  P5/P6 wording (`:240–247`) plus the ADR-59 and `STATUS.md` mirrors so the cut
  is auditable. **One correction to the brief, because the evidence disagrees with
  a uniform "all absent" reading:** P5 has two halves and only one is absent. The
  **migration runner is genuinely absent** — no code converts a legacy
  `multi_agent`-only config into `[orchestration]`, and ADR-59:169–170 still
  states that pre-P5 condition as current. But **export-merge hardening is
  substantially landed** as a side effect of the single-arm Save work:
  `config/src/saving.rs:1–40` documents merge-aware `toml_edit` writers that
  preserve comments, key order and unedited keys, all atomic via
  temp-file-plus-`rename`, with `seed_orchestration_roster` / `save_agent_roster`
  writing only the keys they own. `config/src/migration.rs` is *schema_version*
  step migration, which is a different mechanism and does not satisfy P5.
  Everything else — config-authorable Graph/DAG, run-one-stage simulation, the
  Studio-level freeze / stable-surface decision, the TOML diff view, the canvas
  DAG editor, multi-executor partitioning — is absent with no scaffold
  (`run_one_stage`, `freeze_decision`, `migration_runner`, `multi_executor`,
  `executor_partition`, `toml_diff`, `stable_surface`, `simple_tier` all return
  zero hits). The `DAG`/`Freeze` code that does exist
  (`graph.rs`, `scheduler.rs`, `task_transform.rs:163–189`, `apply_freeze`
  `:542–618`) is the landed runtime task-DAG and the model-driven "reconsider"
  freeze — **not** a config-surface graph or freeze — and the appendix says so, so
  a later reader does not mistake one for the other. `STATUS.md:347`'s "Studio
  Simple tier" label has no counterpart in ADR-58 (which demoted the tier
  notion) or in the code; recorded as a source inconsistency, not fixed. The cut
  is recorded as a scoping decision about a superseded UI plan, explicitly **not**
  as a claim that the capabilities are unwanted, and the migration runner is
  called out as standing debt that has no UI dependency at all.
- **Row 37 rescoped L → M, kept OPEN and deferred post-1.0.** The reduction is
  earned: the plan's Minimal (~20–30h) and Medium (~60–90h) tiers are landed, per
  the plan's own status list (`hybrid-ui-plan.md:306–317`) and `TODO.md:150–155`
  (PR #49 Minimal, PR #97 Medium, merged 2026-08-03), and the code confirms it —
  the `SubView` overlay enum (`views/chat.rs:23–25`, with `SpendLog` and
  `Runtime` added since the plan was written) routed by keyboard at
  `app.rs:1218–1222`, the shared animation layer (`app.rs:498` `ease_out_cubic`,
  `:4030`), the terminal bottom panel with drag resize (`app.rs:398`, `:2033–2046`),
  and the memory quick-panel. What remains has **no correctness, policy or
  data-integrity content** — it is layout and information-architecture polish —
  which is what makes a single M defensible. **Two corrections to the brief,
  recorded because the accurate version changes what the row should say.**
  (i) **There is no `SectionId::Skills` or `SectionId::Mcp`.** Settings is one
  scrolling page of **9 collapsible sections** — `SectionId::ALL` at
  `views/settings/message.rs:372–382` (Theme, Providers, Assignments, Policy,
  Relationships, Retry, Memory, Shell, Extensions), rendered through
  `collapsible_section` at `mod.rs:1049–1054` with a jump-sidebar at `:1060–1070`.
  Skills and MCP are `ExtensionTab` variants (`message.rs:342`, wired
  `mod.rs:1177–1201` — Skills / MCP / Plugins / ProjectContext) *inside* the one
  `SectionId::Extensions` hub. So "tabbed Settings" is unbuilt at the top level
  and a *hub* of four sub-tabs already exists; the row now describes it that way
  rather than naming sections that do not exist. (ii) **Drag-and-drop agent
  assignment is not on the `crates/desktop/AGENTS.md` ⬜ line.** That line
  (`:65–66`) names two items — lazy-init, and "Full scope: tabbed Settings,
  Studio split pane, focus trap" — while the plan (`:89`), `TODO.md:156–158` and
  `ROADMAP:147–149` name four. The row cites both sources so the residual is not
  read as smaller than it is. **Also flagged as real work inside the remaining
  scope:** the plan's Medium item "SubView routing fully replaces `Page` for
  Chat-adjacent views" (`:83`) and its step 6 "Remove unused `Page` variants"
  (`:300`) are **not** done — `Page::DiffViewer` and `Page::ToolLog` are still
  routed and rendered (`app.rs:3973–3974`) and remain reachable from the context
  bar (`views/context_bar.rs:29–30`) and the quick panel
  (`views/quick_panel.rs:146`), so the `Page`/`SubView` dual path is live
  duplication, not a cosmetic leftover. Per the brief, **LAZY-INIT is called out
  as the one remaining item with a felt payoff** (it is the only one that changes
  startup cost and responsiveness rather than looks) and may be pulled forward.
  Citation correction applied: the row's `TODO.md:145` had drifted onto an
  unrelated ADR-28 shell-profile note; the item is at `TODO.md:156–158`.
- **Row 9 corrected — it was never "unverified".** The `[verify]` flag and the
  "per-variant auth steps unverified" framing are removed, and the row now states
  what is actually true: the wizard exists and is wired, and the auth gap is
  real, specific and named. **Wiring:** `SetupWizard::run`
  (`config/src/setup.rs:236–264`) = provider → key → model → working dir →
  policy, returning a `PendingConfig` whose `save`/`save_overwrite` (`:109–119`)
  write TOML and push the key to the OS keychain (ADR-04; the key is never in
  TOML). The CLI hooks it at first-run **and** `--reconfigure`
  (`cli/src/lib.rs:106–170`) and deliberately does *not* call `run()` — it
  re-drives the same prompts individually (`:126–152`) so a **live model probe**
  can be slotted in between: `list_models_for_provider_blocking` at `:141–146`
  feeding `set_available_models` at `:146` for a numbered picker. **Desktop
  onboarding: zero wiring**, confirmed by search — `SetupWizard`, `setup::`,
  `needs_setup` and `run_wizard` return no hits anywhere under `crates/desktop`,
  so the wizard is reachable only from a terminal. **The per-variant gap,
  precisely:** one generic `prompt_api_key` (`:314–317`, no provider argument)
  serves all **seven** `ProviderKind`s (`:40–48`), and the only per-kind delta
  anywhere in the wizard is `default_model` (`:51–61`); there is no OAuth,
  device-code, or token-exchange variant for any kind. The row stays OPEN and
  deferred per owner decision, and its re-entry cell now splits the two
  separable pieces (per-kind auth is a `setup.rs` change; desktop onboarding is
  new UI) so neither implies the other.
- **Row 29 reviewed, confirmed still deferred, left otherwise untouched.** No
  status change and no size change — only a dated marker in the Item cell. The
  factual state, checked in the tree: **SSE is LANDED** — `SseAdapter::from_bus`
  (`api-server/src/sse.rs:17–20`) bridging the `EventBus` to an axum SSE stream
  filtered by `TaskId`, served as `GET /v1/sessions/{id}/tasks/{tid}/stream`
  (`api-server/src/routes.rs:345–366`). **Server mode is LANDED as a binary** —
  `api-server/src/main.rs:101–107` binds a `TcpListener` and serves the router
  (with the non-loopback bind warning at `:93–99` and connect-info for the
  per-client rate-limit key at `:105–107`). **Marketplace is ABSENT:** a
  repo-wide search for `marketplace` / `registry_url` / `plugin_registry`
  returns **zero code hits** — the only four matches are doc statements of the
  deferral itself (this register rows 29 and 30, `skills-mcp-extensions-plan.md:21`,
  `ADR-43:162`). **Persistent desktop state is ABSENT:** the desktop extension
  surface is config-driven with next-run semantics (`docs/skills.md:201–202`,
  "live toggles via a held runtime handle are deferred (ADR-43 §3 v1 note)"), and
  the desktop settings state is explicitly transient — `settings/state.rs:25–27`,
  "Transient view state: never persisted and never arms the dirty flag". **TOML
  secrets are absent BY DESIGN, not by omission:** credentials are keyring-only
  (`config/src/credentials.rs:6–12`, ADR-04, with `from_env()` as the test-mode
  env-var path), and the MCP server schema states it outright
  (`config/src/schema.rs:1156–1157`, "Secrets are never stored in TOML (keyring
  integration is deferred)"); ADR-43:163 lists keyring-backed MCP tokens as
  deferred. So three of the row's five sub-items are either landed or
  permanently out of scope, and the two that remain real (marketplace, persistent
  live state) are exactly the ones a later web/API UI would need. Noted for a
  future pass, not actioned: this row's compound framing now understates the
  landed half, so when it is next promoted or re-scoped it should be split
  rather than closed as a unit.
- **Row 4 — this pass's read-only finding is SUPERSEDED and was corrected, not
  left to rot.** As recorded below earlier today, the row was read and left
  untouched, on the basis that the tree showed a *single-slot* ack cell where
  "reject-busy went the other way on purpose" — the opposite of ADR-68 §6's
  queue decision. That description applied to the pre-queue tree and no longer
  describes the code: `8e3c350` ("feat(desktop): bounded per-session
  acknowledgement queue (DEFERRED row 4)") landed the queue, so the row is now
  **closed** and removed from the open table, with the shipped state recorded in
  Closed #19 and a full verification entry appended below. The stale pre-queue
  sentences are **deleted**, not annotated. Two facts from the original read do
  survive the change and are carried forward here so nothing real is lost: (i)
  the **session-scoping half of ADR-68 is landed** — `PendingAck.session_id`,
  now carried per queue entry rather than on a lone cell, satisfying A3 and the
  executor-boundary validation of A1; (ii) the row's source cell carried a
  **dangling clause**, "prefer in-flight budget instead per note", citing a note
  that exists nowhere — a repo-wide search for "in-flight budget" /
  "inflight_budget" returns exactly one hit, this register's own row 4. That
  clause disappears with the row; noted here so a future reader does not go
  hunting for a recommendation that was never written down.
- **Follow-ups flagged, not actioned here** (all outside this pass's scope —
  they touch other documents or other rows):
  - `docs/TODO.md:42–45` still reads "`request_ack` unscoped approval event (audit
    H-04 follow-up). **Not started** — `ApprovalSink::request_ack` passes no
    session id" and cites `AUDIT_FINDINGS_CURRENT.md` H-04 "Remains". That is
    stale in the same way row 21's citation had drifted: the signature is
    `request_ack(session_id, message, cancel)` and `PendingAck` carries
    `session_id` (ADR-68 landed). The *unscoped approval **event*** half may
    still be open, but the item as written describes a defect that is fixed.
  - `ROADMAP.md:188–191` (code editor integration) still treats the embedded
    editor as a maybe-later alternative, which the landed `views/code_editor`
    contradicts; and `docs/desktop-cli-parity.md:96–97` remains the correct cite
    for the GUI-only `Editor` that `TODO.md:172–173` mispoints at `:103–104`.
  - `crates/desktop/AGENTS.md:50` still says the hybrid Minimal + Medium scope is
    "pending review/merge" on `feat/ui-depth-improvements`, while
    `TODO.md:150–155` and `hybrid-ui-plan.md:306–317` record it merged into `dev`
    via PR #97 on 2026-08-03. Row 37's own citations are corrected; that AGENTS.md
    line is not.
  - `docs/STATUS.md:346–349` is the mirror of the now-cut row 28 and still lists
    the deferred P5/P6 items as tracked follow-up #4 without saying the register
    row was cut. Left alone here (a STATUS reconciliation touches row 35's
    source); worth one pass.
  - Still carried forward from the three prior passes and untouched:
    `docs/security-threat-model.md` §6 gaps #1, #5, #6; `docs/TODO.md:229–235`
    ("Not started" for work that landed in `f4bdc4f`); `ROADMAP.md:241–245`;
    and the `— manual` orphan fragment in the 2026-09-24 notes above (line ~170),
    still unrepaired.

## Verification notes (2026-09-27, row 4 closure)

Follow-up pass closing register **row 4** (per-session ack queue), executed
2026-09-27 on `fix/coordinator-error-invariant` at HEAD `8e3c350`. **Docs only —
no source edits, no builds, no test runs, no commit**; the tree is left for the
orchestrator to commit alongside the other staged row changes. Every claim below
was re-read in the tree at HEAD, not taken from the commit message.

- **Row 4 closed — see Closed #19.** Its re-entry condition ("Implementation
  phase of ADR-68 desktop ack queue") has been reached and answered *in the
  direction ADR-68 §6 chose*. The desktop ack cell is now a depth-bounded FIFO,
  not a single slot: `SharedPendingAck = Arc<Mutex<VecDeque<PendingAck>>>`
  (`desktop/src/widgets/capability_dialog.rs:137`), bounded by
  `MAX_PENDING_ACKS = 2` (`:98`) — the "at most one pending ack beyond the active
  one" of ADR-68 §6, expressed as the active dialog plus one queued. The row is
  removed from the open table and its `#4` gap left intentionally, matching the
  row 10 / 12 / 14 / 19 / 21 / 25 / 26 / 28 / 33 / 44 / 47 / 49 precedent. No
  other row touched; no renumbering.
- **The bound's rationale is documented in code, and recorded here because it is
  the non-obvious design choice in this commit** (`capability_dialog.rs:84–97`):
  an ack is a *blocking, user-facing* confirmation, not a background job. One
  active is what the user can actually read; the one queued slot exists so a
  concurrent prompt (another run/session) cannot overwrite the first; a deeper
  queue would only delay the *second* prompt past the point where its requester
  is still waiting, and would let an ack storm grow UI state without bound. Two
  is the smallest depth that covers the real concurrent case without becoming an
  ack buffer.
- **Overflow is an explicit refusal, never a silent drop — verified, both
  variants.** `AckQueueError` (`:105–114`) carries exactly two fail-closed
  cases: `QueueFull { capacity }` (the new request is refused outright and never
  displaces an already-pending ack) and `StateUnavailable` (the shared lock is
  poisoned, so the desktop cannot prove the request was recorded and must not
  report "no pending acks" for a request that was actually made). The sink maps
  either to `false` — abort the task — **and** publishes a new
  `DesktopEvent::ErrorOccurred` (`app.rs:4776–4778`) that the app renders as an
  error toast. That toast is what makes *shown*, *queued* and *refused* three
  distinguishable user-visible outcomes rather than one silent no-op; the
  `Display` impl (`:116–128`) is what carries the difference to the user.
- **Session gating is preserved through the queue, and FIFO order cannot be
  jumped.** `resolve_ack` (`:233–246`) pops the front entry and answers it only
  when its `session_id` matches the caller; a non-matching entry is pushed back
  to the front so its owning session still sees its dialog. This is the
  pre-existing session check retargeted to the queue front, not a new check —
  worth stating because queueing is exactly the change that could have weakened
  it. Resolution drains in order, so the queued second ack cannot overtake the
  active first.
- **Dialog indicator keeps the common case byte-for-byte unchanged.**
  `ack_queue_position` (`:180–185`) returns `None` for ≤1 pending and
  `Some("Acknowledgement 1 of N pending")` for N≥2; the count line is pushed into
  the modal's children only in the `Some` case (`:207–213`). A single-pending ack
  therefore renders exactly as it did pre-queue. Note for accuracy: this helper
  is private (`fn`, not `pub`) — an internal render concern, not new API.
- **Supporting call sites confirmed:** `DesktopApprovalSink::request_ack`
  enqueues through `enqueue_ack` (`app.rs:4760`) and the status-bar
  dialog-waiting check moved from `is_some()` to `!is_empty()`
  (`views/status_bar.rs:58`). Both are the minimum to keep the new queue
  coherent — the `is_some()` check would have gone stale silently otherwise.
- **The audit-seam half was a reachability *finding*, not missing wiring — and
  this corrects the brief's framing, so it is recorded explicitly.** The
  production chain `AgentLoop::setup_undo_stash` (`agent_loop.rs:1225–1227`) →
  `ToolExecutionBackend::record_ack_decision` → `InProcessGateBackend` →
  `ToolExecutor::record_ack_decision` already existed, and `git log` attributes
  it to `26e3fc9` ("ADR-66 phase 1 — per-model tool capability, fail-loud
  seams, Gemini loose schemas"), i.e. it predates `8e3c350`. What actually landed
  in this commit is the **test that proves reachability** —
  `in_process_gate::tests::record_ack_decision_writes_request_continue_and_abort_rows`
  (`orchestrator/src/in_process_gate.rs:554`), covering a `RequestContinue` row
  for a continuing ack and a `RequestAbort` row for a refused one — plus the
  correction of a doc comment on `ToolExecutor::record_ack_decision` that falsely
  claimed "nothing calls this in production", now naming the real chain
  (`core/src/executor.rs:298–303`). Practical consequence: the row's audit half
  was smaller than the row implied, and a future reader should not expect a
  second wiring change here.
- **Residual recorded, not fixed — the one honest gap in this closure.** An
  overflow refusal audits as `RequestAbort`, because `ApprovalSink::request_ack`
  returns only `bool`: the sink cannot hand the audit seam a reason, so in the
  trail an overflow is distinguishable from a user cancel **only by message
  text, not by a distinct verdict**. Fixing it requires widening
  `ApprovalSink::request_ack` to a typed decision — a contract change reaching
  every sink — which is out of scope for this commit and stated in Closed #19 so
  the closure is not read as claiming full audit fidelity on the refusal path.
- **The row's stale pre-queue note was deleted, not annotated.** It survived in
  the 2026-09-27 owner-decisions notes above as "single slot" / "reject-busy went
  the other way on purpose"; both described the pre-`8e3c350` tree. That whole
  bullet is replaced by a short superseded-pointer, with the two facts from it
  that still hold (the landed session-scoping half of ADR-68; the dangling
  "prefer in-flight budget instead per note" clause, which cites a note that
  exists nowhere) carried forward so nothing real is lost. The historical header
  append for the owner-decisions pass still says "Row 4 was read and left
  untouched" — that is left as-written, because it accurately describes *that*
  pass, and this section plus the new header append supersede it.

## Verification notes (2026-09-27, rows 3 / 30 / 29 — plugin watcher cut, registry fold, row 30 rescope)

Executed 2026-09-27 on the current checkout. **Docs only — no source edits, no
builds, no test runs, no commit**; the tree is left for the orchestrator to commit
alongside the other staged row changes. Every claim below was re-read in the tree,
not carried over from a prior note. **No other row touched; no renumbering; the
`#3` gap left intentionally**, matching the row 4 / 10 / 12 / 14 / 19 / 21 / 25 /
26 / 28 / 33 / 44 / 47 / 49 precedent.

- **Row 3 cut as a duplicate of row 30, and the cut is safe because the row said so
  itself.** Its re-entry read "Verify scope first: if 'plugin hot-reload' was meant,
  row 30 applies" — and it was, which is why row 30's registry clause was folded
  into row 29 and row 30 rescoped to hot reload + remote loading rather than
  deleted. Nothing is lost: the watcher work now lives in row 30's hot-reload
  clause, and the desktop already names it as a follow-up.
- **No `.wasm` watcher exists in `crates/` or `docs/` — verified, and the only two
  watchers in the repo are unrelated to plugins.** The memory re-index watcher is
  `crates/memory/src/watcher.rs` (`notify` import at `:11`, `FileWatcher` at `:31`,
  the debouncer-holding `FileWatch` at `:39–43`, `ReindexQueueDrainer` at `:149`).
  The desktop config watcher is **`crates/desktop/src/config_watch.rs`** (not under
  `views/`) — `notify` import at `:19`, and its own module doc at `:1–14` states it
  "mirrors the memory-crate watcher shape" for **config files** only. Both watch
  source/config trees; neither has any plugin, WASM, or `.wasm` awareness. A
  search of `crates/plugins` for `hot.reload` / `reload_plugin` / `unload` /
  `invalidate` / `mtime` / `watcher` returns **no watcher and no reload path at
  all** — only the two legitimate `unload_plugin` / `unload_without_registry`
  methods and unrelated `wasmtime::` matches.
- **Row 3's only mentions elsewhere are this register and `docs/TODO.md:119` —
  and the desktop already calls it a documented follow-up.** `app.rs:2533–2534`,
  on `refresh_plugin_providers`: "A full `.wasm` filesystem watcher is a
  documented follow-up; until then this save-time pass is the trigger." That
  save-time pass is the only re-discovery trigger in the tree, and it calls
  `refresh_new_plugins` — which is exactly row 30's hot-reload clause.
- **The brief's premise about ADR-37 hash-pinning is INVERTED, and the register
  records the corrected fact — this is the one substantive correction in this pass.**
  The premise was that the pinning "hashes the capabilities granted, NOT the
  plugin code". It does the opposite: `manager.rs:193–197` computes
  `Some(crate::capability::sha256_hex(wasm_bytes))` — over the **WASM binary
  bytes** — and only when `capabilities_required` is non-empty. That is the code;
  ADR-37 states the same in prose at `:39–40` ("SHA-256 of the loaded WASM binary at
  the time of approval") and `:41–43` ("if `manifest_hash` is `Some` and the
  current binary hash differs, the grant is treated as stale … re-prompted"), with
  `:95–96` adding "The hash pinning catches binary tampering regardless of clock
  state", and `ROADMAP.md:69–70` describing it as "30-day TTL, SHA-256 hash
  pinning". The `sha256_hex` cite is right (`capability.rs:9–15`); the
  `load_grants_with_report` cite is **not** `:453` — `:453` is the `wasm_hash`
  parameter of the private `load_for_plugin`; `load_grants_with_report` is at
  `capability.rs:669–674` and delegates to it. **Consequence for the rows: this
  correction strengthens both.** The brief's conclusion (wire the existing grant
  behaviour, invent nothing new) is right for a better reason than given — a
  changed `.wasm` provably loses its grants.
- **The corrected trust model, stated precisely in row 29 because the distinction
  is the whole re-entry argument: hash-pinning is a RE-APPROVAL mechanism, not an
  AUTHENTICITY mechanism.** It answers "did this file change since I approved it?"
  Nothing in the tree answers "who published this file?" — and ADR-37 says so
  itself, listing "**Plugin registry signature verification**" under Alternatives
  Considered as "most secure, but introduces a dependency on a registry and key
  infrastructure. **Deferred to post-v1.0**" (`ADR-37.md:104–105`). So with no
  signature check anywhere, a registry genuinely moves the trust question from "I
  chose this local file" to "someone published this, and it is executed with no
  signature check" — which is why row 29's marketplace re-entry is a provenance /
  trust-model decision (ADR-first) rather than a schedulable feature. Row 29's
  status (OPEN) and size (L) are unchanged, as instructed; only the marketplace
  clause was deepened.
- **Registry/marketplace absence re-confirmed and its `registry`-name collision
  disambiguated.** No catalog, index, version pinning, or signature verification
  exists in `crates/plugins`. The crate's own `registry` identifiers are all the
  core `concerto_core::types::ToolRegistry` — the tool-registration type at
  `manager.rs:340,349,442` and `tool_bridge.rs:207,230` — never a plugin registry.
  The doc-side absence was already verified in this register's earlier row-29
  pass: zero code hits for `marketplace` / `registry_url` / `plugin_registry`, with
  the only four matches being statements of the deferral itself (this register's
  rows 29 and 30, `skills-mcp-extensions-plan.md:21`, `ADR-43:162`). Row 29's
  marketplace clause now merges that with the signature finding rather than
  restating it.
- **Row 30 rescoped to hot reload + remote loading, size L → S, with the seam
  recorded so the work is not rediscovered from scratch.** Hot reload is absent in
  *any* form, and `refresh_new_plugins` (`manager.rs:549`) makes the additive
  behaviour explicit: already-active plugins are skipped at `:583–585` ("plugin
  refresh: already active — skipping"). That is a deliberate non-displacement rule,
  not a missing branch. The clean seams: `PluginManager.active` is a
  `HashMap<String, Arc<Mutex<ActivePlugin>>>` (`manager.rs:64`), unload at `:439`
  and `:466`, re-load at `:161`. **The single genuine obstacle is recorded
  precisely:** `ActivePlugin` owns `wasmtime::Store<PluginStoreData>`
  (`active_plugin.rs:10–14`), which cannot be swapped in place, so a reload must be
  drop → re-`load_from_bytes` / `initialise` (`loader.rs:60`, `:111`) →
  re-register tools (`tool_bridge.rs:230`). And there is **no module cache to
  invalidate** — the `Engine` is reusable (`loader.rs:68`) and each load builds its
  own `Arc<Module>` (`loader.rs:104`). Row 30 therefore names the RE-APPROVAL story
  as the actual design question, with the corrected hash-pinning fact as the
  existing mechanism to wire.
- **Remote loading is verified ABSENT and recorded as a deliberate non-goal.** The
  loader reads a local file only — `loader.rs:52` `std::fs::read(wasm_path)`, then
  `:60` `load_from_bytes`. The only http in `crates/plugins` is **guest egress**:
  `host_fns.rs:437` `host_http_get`, gated by `check_url_allowed` against the grant
  allowlist at `:453`. That is an outbound network *capability granted to a plugin*
  — the opposite direction from a load source, and not a partial remote loader. It
  is written into row 30 as a non-goal gated on row 29's trust-model decision so a
  later reader does not mistake it for an oversight.
- **Two stale cites in row 30 corrected, following the precedent of rows 9, 17 and
  24.** (a) The row cited `TODO.md:108–110`; those lines are the **PersonaMem**
  long-horizon-memory-eval item. The correct cite is `TODO.md:119–121` ("Plugin
  hot-reload, remote plugins, registry. Not started — …"). (b) That TODO line in
  turn points at "`ADR-21.md:30-31` deferred list", but `ADR-21.md` is a **12-line
  archived stub** (`:3–8`, "Archived — superseded by ADR-14 … This stub is not
  active guidance"), so that cite cannot be resolved; the live plugin design is
  ADR-14. Row 30 now records both corrections instead of inheriting a cite that
  points at the wrong subject.

## Verification notes (2026-09-27, rows 32 / 35 — certified-evolution cut, STATUS-checklist reclassification)

Executed 2026-09-27 on the current checkout. **Docs only — no source edits, no
builds, no test runs, no commit**; the tree is left for the orchestrator to commit
alongside the other doc changes already in the working tree. Every claim below was
re-read in the tree or in the cited document, not carried over from a prior note.
**No other row touched; no renumbering; the `#32` and `#35` gaps are left
intentionally**, matching the row 3 / 4 / 10 / 12 / 14 / 19 / 21 / 25 / 26 / 28 /
33 / 44 / 47 / 49 precedent.

- **Scope of this pass: rows 32 and 35 cut**, landing in the Closed appendix as
  #20 and #21. The two cuts rest on different grounds and are recorded separately
  on purpose: row 32 is a **research programme with an unresolvable cite**, row
  35 is a **reclassification of living checklists**. The table keeps its 5
  columns; both rows were deleted whole rather than left as stubs.
- **Row 32 — the dangling cite is deliberate, and that is the finding, not an
  excuse.** `docs/research/certified-universal-evolution.md` is absent from disk
  *and* ignored by `.gitignore:54`, whose path is that file exactly; the comment
  on `.gitignore:53` reads "**Private research — not for public until ready**".
  `git check-ignore -v docs/research/certified-universal-evolution.md` reports the
  matching rule as `.gitignore:54`, and `docs/research/` contains six tracked
  files, none of them this one. So the register has been citing, as the source of
  a deferral, a document that **cannot exist in this repository** — which is why
  a `[dangling-cite]` flag was the right annotation and why the row could never be
  closed from what is in the tree. Recorded as **explained, not accidental**.
- **Row 32 — two of the row's own cites had drifted off the item, and both are
  corrected in the appendix.** The old `TODO.md:286–293` is the
  **prompt-cache-stability** entry (which is row 1's subject, not this row's); the
  certified-evolution item is `TODO.md:297–304`, with the prerequisite list at
  `:300–303`. The old `ROADMAP:189–193` straddles the **code-editor** entry
  (`:188–191`, since cut as Closed #17) and only the first two lines of this one;
  the item is `ROADMAP.md:192–196`. The row was therefore citing two documents at
  the wrong offsets *and* a third document that is git-ignored — the cut removes a
  row that was already unciteable, which is the honest way to describe it.
- **Row 32 — TWO premises in the brief FAILED verification and are deliberately
  not written into the row as claims. This is the substantive part of the note.**
  (a) **"The only `confidence` in the tree is an intent-routing threshold" is
  FALSE.** A case-insensitive search of `crates/**/*.rs` returns **85 matches**,
  across three unrelated subsystems: intent routing (`core/src/intent.rs:15,135`
  — `LOW_CONFIDENCE_THRESHOLD` + `RouterOutput::confidence`, ADR-55 vocabulary
  whose constant the ADR-56 LLM classifier reuses as its re-route threshold, with
  the operator knob at `config/src/schema.rs:239,269–305`); memory-decision
  confidence (`core/src/memory.rs:303,313`, plus
  `memory/src/decision_store.rs:94–108,158–168`,
  `memory/src/entities.rs:78,804–805` — a `0.0..=1.0` SQLite CHECK constraint
  with supersession tracking); and the issue-#59 low-confidence consultation
  trigger (`orchestrator/src/consultation.rs:52,71,292`;
  `coordinator.rs:16930,16938`). **The conclusion survives anyway, on a better
  basis:** every one of those is a bare `f32` scalar for routing, recall ranking
  or consultation triggering — none is a *type*, and none carries evidence. So
  "compiler-enforced confidence/evidence types" is genuinely unbuilt, but the
  register must not carry a "no confidence in the tree" claim, because one grep
  falsifies it. (b) **"No evidence types" is also FALSE, and by a wide margin.**
  ADR-65's evidence spine is a large, landed subsystem: `AcceptanceEvidence`
  (`orchestrator/src/checkpoint.rs:69–103`), `design_doc_verifier.rs`
  (`:75–88` reason taxonomy, `:112–127` gathered-evidence struct, `:204–216`
  claim resolution), `resume.rs` (`:16`, `:192` `RefreshEvidence`),
  `world_model.rs` (`:141–182` resolved-question evidence ids), plus
  `tool_facts.rs` and `read_cache.rs`. What is missing is the
  certified-evolution *sense* of the word: a **type-level gate on promoting a
  self-modification**, not a record of what happened in a run. The name collides;
  the construct does not exist. Writing the brief's version would have put two
  false claims into a permanent record.
- **Row 32 — the neighbours are not it, and each was checked for what it actually
  does rather than waved off by name.** `crates/eval` is real and substantial
  (six modules; `lib.rs:5–8` "Phase 3 evaluation harness"; `categories.rs`,
  `scenarios.rs`, `persona_mem.rs`; ~30 fixtures under `benchmark_tasks/`) — but
  its contract is to *detect the project's own test runner* (`cargo`, `npm`,
  `pytest`, `make`; `lib.rs:7`) and run that suite in a working directory
  (`run_in_dir` `:267–274`; the honest-`Fallback` cargo path `:291–294`; the
  evidence-resolved root logic `:91–112`). No isolation, no immutability, no
  evaluator-spec validation, no candidate-vs-baseline comparison. It answers
  "can the agent finish this task?", not "does this candidate modification
  improve a metric?". The **seven benches are criterion micro-benchmarks**
  (`core/benches/serde.rs`, `core/benches/policy.rs`,
  `orchestrator/benches/task_graph.rs`, `memory/benches/vector_retrieval.rs`,
  `memory/benches/fts_search.rs`, `providers/benches/provider_streaming.rs`,
  `tools/benches/virtual_fs.rs`) — no profile-guided CI gate, and the Phase 3
  criterion-benchmark + CI-gate work is already carried by **row 48**, so nothing
  is being dropped by this cut. `orchestrator/src/fault_injection.rs:76–92` is
  the G1–G3 issue-gate rows plus the C1–C4 crash-window table — **row 17's**
  in-process, deterministic runtime-robustness suite, and its own module doc
  (`:78–85`) says so. A search of `crates/**/*.rs` for `evaluator spec` /
  `profile.guid` / `evolution` / `self.improv` returns no evaluator-spec type and
  no promotion gate; the `promote`/`mutation` hits are all file-write policy
  (`core/src/authorization.rs`, `core/src/policy.rs`,
  `config/src/saving.rs:181–191`). **`STOKE` occurs in exactly two places
  repo-wide** — `ROADMAP.md:196` and `TODO.md:303`, both prose in the two cited
  lines — and zero times in any `.rs` file (the `crates/providers/src/google.rs`
  hits are `candidatesTokenCount`, a false positive). So there is no
  STOKE-comparable evidence and never has been.
- **Row 32 — the required scope is recorded verbatim so the cut is reversible.**
  From `TODO.md:300–303` and `ROADMAP.md:192–196`: compiler-enforced
  confidence/evidence types, evaluator specification validation, deterministic
  isolated **immutable** evaluation infrastructure, explicit resource budgets
  with stop/resume semantics, and small-domain evidence comparable to STOKE. Of
  those five, the tree has at most one *adjacent* item — resource budgets, via
  rows 36/45's CPU budget and the existing wall-clock timeout — and that budget
  bounds a **shell command**, not an evolution run. `TODO.md:303–304` supplies
  the cut's own authority: "STATUS.md does not cover this track; it is
  roadmap-scoped research, not a promised feature."
- **Row 35 — the mislabelling, established from the forms themselves rather than
  asserted.** `TESTING.md:1–6` and `docs/live-test-template.md:1–7` both open by
  requiring one sheet per build / OS / provider-model combination, and both
  prescribe the same four verdicts — **Pass / Fail / Blocked / Not tested**.
  `TESTING.md` is 290 lines: environment table `:12–29`, automated-checks results
  `:53–60`, the acceptance-bar section that maps the audit's 12 end-to-end
  scenarios onto named automated tests `:62–92`, and the area sheets `:113–257`
  (default desktop `:113`, … cancellation `:196–203`, memory restart `:205–214`,
  spend `:216–226`, shell and policy `:228–241`, skills and MCP `:243–257`).
  `docs/live-test-template.md` is 76 lines: environment table `:11–25`, nine
  generic key-test rows plus the six Studio rows `:42–47`, automated checks
  `:49–66`, outcome and funding notes `:68–76`. Every result cell is meant to be
  filled in, per build, every release. **These have no terminal state**, so
  "promote to Closed when the re-entry condition is met" is a category error for
  them, and an entry with a size and an implied owner reads as debt.
- **Row 35 — all six items are OPEN, and the state of each was re-verified today
  so the removal loses nothing.** (i) **ENV_LOCK race hardening: OPEN.** The lock
  is real (`app.rs:4943`), and so is the panic-safe pattern, documented at
  `app.rs:8028–8033`. Most guarded tests follow it (guard/restore pairs
  `:5223`/`:5245–5249`, `:5308`/`:5330–5334`, `:5365`/`:5377–5380`,
  `:6839`/`:6845–6848`, `:8632`/`:8654–8657`) — **but not universally**, which is
  the substance: `save_materializes_a_name_selection_inline_into_the_global_config`
  asserts at `app.rs:5565` while restoring at `:5598–5599`, and the test guarded
  at `:5705` asserts at `:5733–5736` before restoring at `:5742–5745`. A panic in
  either unwinds past the restore and leaks the redirect into parallel tests, and
  no RAII guard type exists in the module. (ii) **Glyph-font coverage: OPEN**,
  and still cosmetic/text-paired as `STATUS.md` claims — `semantics_glyph`
  (`views/orchestration_studio.rs:909–915`, the `🛡 ➜ ⛓` set) is paired with
  `semantics_label` (`:917–926`) in both the kind picker and the row's semantics
  tag, under the `:907` comment that the affordance is "never color alone"; no
  bundled icon font, no explicit iced fallback. (iii) **Multiline
  system-instructions: OPEN** — see the citation correction below. (iv)
  **ADR-59/P4 deferrals: OPEN**, and the two ADR-59 items that overlap the
  now-cut **row 28** (the TOML diff view and the canvas DAG editor, P6) are
  **already recorded there** in Closed #18, so they are cross-referenced here
  rather than duplicated; **export-merge hardening actually LANDED as a side
  effect** — `crates/config/src/saving.rs:1–40` documents merge-aware `toml_edit`
  writers preserving comments, key order and unedited keys, all atomic via
  temp-file-plus-`rename`, with `seed_orchestration_roster` / `save_agent_roster`
  writing only the keys they own — **while the migration runner did not** (no
  code converts a legacy `multi_agent`-only config into `[orchestration]`;
  ADR-59:169–170 still states the pre-P5 condition as current; and
  `config/src/migration.rs` is *schema_version* step migration, a different
  mechanism). (v) **Orchestration-editor checklist: OPEN** — the six rows are
  present at `docs/live-test-template.md:42–47` with blank Result cells, matching
  `STATUS.md:350–355`. (vi) **Release-priority matrix: OPEN** —
  `STATUS.md:357–365`, "Immediate release priorities" items 1–5. Note the
  boundary: that section sits **outside** the "Tracked follow-ups" block
  (`STATUS.md:323–355`), so the row's own `~328–355` cite never covered it;
  recorded here so it is not lost with the row.
- **Row 35 — ONE citation in the brief was wrong and is corrected, because the
  wrong file would have sent a reader to the SubView title bar.** The brief cited
  `crates/desktop/src/app.rs:4114–4122` for the multiline system-instructions
  input. `app.rs:4113–4115` is the SubView overlay **close button** and
  `:4117–4124` is the `Main`/`Diff`/`AgentGraph`/`ToolLog`/`SpendLog`/`Runtime`
  **title match** — unrelated to any input. The real site is
  **`crates/desktop/src/views/orchestration_studio.rs:4114–4116`**,
  `text_input("System Instructions", &p.system_instructions)`, still a
  single-line `text_input` and not upgraded to `text_editor`, confirmed OPEN. The
  same view also carries the sibling long-text inputs at `:4117–4122`
  ("Constraints & Safety", "Output Format"), so the repo's long-text pattern is
  uniform here and the upgrade is a real slice, not a one-line widget swap.
- **Row 35 — the reclassification is explicit, and nothing was deleted.** This
  pass edited **only** `docs/DEFERRED.md`. `TESTING.md` (290 lines),
  `docs/live-test-template.md` (76 lines), the ready-made `docs/live-test-*.md`
  copies, and `docs/STATUS.md` (376 lines, `Tracked follow-ups` at `:323–355`,
  `Immediate release priorities` at `:357–365`) are all untouched and **remain
  the source of truth** for every one of the six items. Anyone looking for the
  ENV_LOCK fix, the glyph work, the multiline upgrade, the ADR-59 tail, the
  Studio checklist or the release matrix will find them exactly where a release
  engineer looks today. This is a **reclassification** out of "deferred work
  with a re-entry condition, a size and an owner" — not a cancellation, and not a
  judgement that the items are unimportant.
- **Follow-ups flagged, not actioned here** (outside this pass's scope):
  - `docs/STATUS.md:346–349` is the mirror of the now-cut **row 28** and still
    lists the deferred P5/P6 items as tracked follow-up #4 without recording
    that the register row was cut. Still unreconciled — previously flagged by the
    2026-09-27 owner-decisions pass, and deliberately left alone here too.
  - `docs/TODO.md:297–304` and `ROADMAP.md:192–196` still cite the git-ignored
    `docs/research/certified-universal-evolution.md`. The dangling cites are now
    *explained* (and the register no longer carries them), but the two source
    documents still point at a file that cannot exist in a public clone. A future
    pass should either un-ignore the file or reword those two lines to say the
    design is private.
  - Still carried forward from the three prior passes and untouched:
    `docs/security-threat-model.md` §6 gaps #1, #5, #6; `docs/TODO.md:229–235`
    ("Not started" for work that landed in `f4bdc4f`); `ROADMAP.md:241–245`; and
    the `— manual` orphan fragment in the 2026-09-24 notes above, still
    unrepaired.

## Verification notes (2026-09-27, rows 39 / 40 — release-and-packaging cut, crates.io publish cut)

Executed 2026-09-27 on the current checkout. **Docs only — no source edits, no
builds, no test runs, no commit**; the tree is left for the orchestrator to
commit alongside the other doc changes already in the working tree. Every claim
below was re-read in the tree or in the cited document, not carried over from a
prior note or from the pass brief. **No other row touched; no renumbering; the
`#39` and `#40` gaps are left intentionally**, matching the row 3 / 4 / 10 / 12 /
14 / 19 / 21 / 25 / 26 / 28 / 32 / 33 / 35 / 44 / 47 / 49 precedent.

- **Scope of this pass: rows 39 and 40 cut**, landing in the Closed appendix as
  #22 and #23. **Both cuts are RESUMABLE and neither is a cancellation.** The
  owner decision recorded on 2026-09-27 for each is that the work is wanted
  *eventually*, so the purpose of cutting is to get the rows out of the active
  register while writing down enough verified state that resuming needs **no
  re-investigation**. The state is recorded in the two appendix lines; this
  section records the verification behind them, including the claims that were
  corrected.
- **The two rows share a source bullet and a pipeline, but not a finding** —
  hence separate entries. `ROADMAP.md:197–198` is one combined bullet ("Binary
  installers **and** crates.io publishing"), `docs/TODO.md:271–276` is two
  adjacent Release-section entries, and both rows depend on the same never-run
  `release.yml`. But row 39's substance is *a working pipeline plus an absent
  packaging layer*, while row 40's is *a one-line flip whose real cost is the
  crate subset*. Merging them would have hidden both.
- **Row 39 — the "missing" state is real but starts much later than the row
  implied, and the single most important fact is that nothing has been
  exercised.** `release.yml` is 102 lines and complete: `v*` tag trigger
  (`:13–16`), `permissions: contents: write` (`:18–19`), `fail-fast: false`
  (`:31`), a 4-target matrix (`:32–45` — `x86_64-unknown-linux-gnu`,
  `x86_64-pc-windows-msvc` with `.exe` suffix, `aarch64-apple-darwin`,
  `x86_64-apple-darwin`), the iced/wgpu Linux graphics libs (`:55–62`),
  `cargo build --release --target … -p concerto` (`:70`), artifact staging
  (`:72–83`), and a `release` job publishing through
  **`softprops/action-gh-release@v2`** with **`generate_release_notes: true`**
  (`:98–102`). `scripts/release.sh` is 54 lines: workspace build (`:13`),
  `concerto-${VERSION}-${OS}-${ARCH}.tar.gz` (`:29`, `:31–33`), `.sha256`
  (`:35–37`), and an **optional** `cargo deb` behind `command -v cargo-deb` that
  warns-and-skips (`:39–48`). **`git tag` returns zero tags**, so the `v*`
  trigger has never fired, the matrix is unproven on all four targets, and the
  release job's "every target succeeded" precondition is untested. Two
  independent and **unreconciled** release paths exist (workspace-wide vs
  `-p concerto`; tarball+checksum vs raw binary; optional deb vs none) with no
  single documented procedure — recorded because a resuming agent should not
  assume the two are one system.
- **Row 39 — every "absent" claim was checked, and two of them turned out to be
  more nuanced than "absent".** (a) **No packaging definitions are committed:**
  a repo-wide search for `cargo-deb`, `package.metadata.deb`, `cargo-rpm`,
  `metadata.rpm`, `NSIS`, `WiX`, `AppImage`, `.desktop`, `winget`, `scoop`,
  `homebrew`, `flatpak`, `snapcraft` returns hits in exactly **two** files — the
  four `cargo-deb` lines in `scripts/release.sh` (`:3`, `:39`, `:40`, `:47`) and
  one **prose** line, `docs/research/ai-native-shell-research-brief.md:34`
  ("Homebrew, Winget, Cargo, Nix, pre-built binaries"). So the `cargo deb` call
  has **no `[package.metadata.deb]` table behind it** and would produce nothing
  as written. The brief's phrasing — "the only hits are in
  `scripts/release.sh`" — is corrected to name the research-brief line too;
  it is a wishlist sentence, not a definition. (b) **No code signing: zero
  matches repo-wide** for `codesign`, `notarytool`, `notariz`, `signtool`,
  `cosign`, `GPG_KEY`, `apple-id`, `import-signing` — so macOS and Windows
  artifacts are unsigned and there is no notarial story.
- **Row 39 — the update story is a half-built seam, and this is the finding that
  the brief's "no update channel" hid.** `crates/cli/src/update.rs:1–5` is
  notification-only and says so in its own module doc ("Never blocks startup,
  **never auto-downloads**"), with a 2s timeout (`:17`, `:59–61`) and a hardcoded
  crates.io endpoint (`:14`). But an `[updates]` config section **does** exist
  (`crates/config/src/schema.rs:932–942`): `check_on_startup` defaulting to
  **true** (`:945–947`) and `update_endpoint: Option<String>` (`:941–942`,
  documented "`None` = use crates.io API"). The CLI **does** honour the first
  (`crates/cli/src/lib.rs:182–185`) and the second is **read by nothing** — its
  only reference outside its own definition is the re-export at
  `config/src/lib.rs:63`, and `update.rs:14` hardcodes the URL. So
  `update_endpoint` is declared, documented, and example'd
  (`docs/config.toml.example:237–241`) but **unwired** — the same defect shape
  this register already flags for row 45's `ShellConfig::cpu_budget_secs`.
  Behavioural consequence, stated precisely: with a default config the check
  fires at a crate that is not published; `fetch_update` (`:64–90`) **never
  checks `resp.status()`**, so the 404's JSON body fails at the `newest_version`
  lookup (`:86–90`) and the error arm logs at **`warn!`** (`:38–40`), not
  `error!`. The example config avoids this by shipping
  `check_on_startup = false`.
- **Row 39 — the GUI packaging question has a definite answer, and it is "not
  separately packaged" by design.** `crates/concerto/Cargo.toml:11–14` sets
  `default = ["desktop"]` with the frontend deps optional and feature-gated
  (`:13–14`), and `release.yml:70` builds `-p concerto` **with defaults**, so
  the Iced GUI rides **inside the single `concerto` binary**. There is no second
  artifact, no `.app` bundle, and no GUI-distinct installer. A resuming agent
  should not plan one without first deciding to split the binary. How a user
  gets a binary today: build from source — `README.md:150–164` documents
  `cargo run -p concerto-desktop --release` (`:150`),
  `cargo run -p concerto-cli --release` (`:156`) and the selector forms
  (`:162–164`), never a download; `docs/STATUS.md:11–14` states source builds
  are the supported distribution path and that installer packages are "not
  currently promise[d]" (`:14`).
- **Row 40 — the row's own re-entry condition named a blocker that does not
  exist, which is why the row is cut rather than kept pending.** `publish = false`
  is declared **once**, at `Cargo.toml:37`, and inherited by every member via
  `publish.workspace = true` (`crates/core/Cargo.toml:6`,
  `crates/concerto/Cargo.toml:6`, same line in the rest) — so the flip is one
  line. **Licence is not a blocker**: `MIT OR Apache-2.0` (`Cargo.toml:35`), both
  allowed by `deny.toml:101–104`. **Nor is any dependency**: `git = ` has **zero**
  hits across `crates/*/Cargo.toml`, and every internal `path =` points at
  `../<crate>` — the only non-`../` `path =` lines in the workspace are four
  `[[bin]]`/`[[bench]]` **target** paths (`eval-runner:34`, `mcp:29`,
  `orchestrator:57`/`:61`). So nothing outside the workspace would have to be
  published first, and a resuming agent should not re-audit either of these.
- **Row 40 — the real question is the SUBSET, and it is the coupling.** Verified
  edge counts: `crates/desktop/Cargo.toml:12–24` declares **13** internal deps;
  `crates/eval-runner/Cargo.toml:12–19` pulls **8**; `crates/plugins/Cargo.toml:13–14`
  exposes core + api-types. So "publish all" means publishing the whole coupled
  graph at `0.1.0` with unstable APIs. **The decision this cut implicitly
  records — publish the `concerto` binary (and possibly `concerto-plugin-sdk`,
  the one genuinely reusable public surface), keep the other 23–24 at
  `publish = false`** — is the only option that ships something usable without
  freezing the whole internal API.
- **Row 40 — FOUR brief counts were wrong and are corrected, because each would
  have sent a resuming agent looking for something that is not there.**
  (a) **25 members, not 26** (`Cargo.toml:4–28`; `AGENTS.md` agrees at "25
  crates"). (b) **20 crates lack a `description`, not ~21** — exactly five have
  one: `concerto-plugin-sdk` (`plugin-sdk/Cargo.toml:3`) and the four
  `test-*-plugin-wasm` (each `:3`). (c) **`repository` is *defined but never
  inherited*** — `Cargo.toml:36` sets
  `repository = "https://github.com/NefaroXX/Concerto"`, but every `[package]`
  block is exactly name / version / edition / license / publish (e.g.
  `core/Cargo.toml:2–6`) with **no `repository.workspace = true`** anywhere.
  "No repository link" is thus a *one-line-per-crate inheritance add*, not a
  value to invent. No crate has `homepage`, `readme`, `documentation`, `authors`,
  `keywords` or `categories` at all. (d) **8 internal edges across 6 crates
  carry `path` with no `version`**, not 4 — the brief's own enumeration listed
  2 + 2 + 4: `concerto-cli` / `concerto-desktop` (`crates/concerto/Cargo.toml:17–18`),
  `concerto-core` / `concerto-config` (`crates/observability/Cargo.toml:23–24`),
  and all four `test-*-plugin-wasm → concerto-plugin-sdk` (`:13` each).
- **Row 40 — the lockstep hazard is bigger than the brief stated, and is
  mechanical.** The other **73** internal edges hardcode `version = "0.1.0"` as
  an **inline literal**, and internal crates are **absent from
  `[workspace.dependencies]` entirely** (no `concerto-*` entry in
  `Cargo.toml:60–204`; no `concerto-x.workspace = true` anywhere). So the
  single-sourced `version` at `Cargo.toml:32` does **not** propagate: 73 literals
  plus 8 versionless edges need lockstep manual editing on any bump. Verified by
  tallying the internal dep lines by form: 16× core, 10× config, 8× api-types,
  6× sessions, 5× tools, 5× providers, 4× orchestrator, 4× memory, 4× eval,
  3× skills, 3× plugins, 3× lsp, 2× mcp, 4× plugin-sdk (versionless), 1× each of
  desktop / core / config / cli (versionless) = 81 internal edges.
- **Row 40 — the `test-*-plugin-wasm` crates should be excluded regardless of
  the subset decision.** All four are `crate-type = ["cdylib"]` (`:10` in each —
  the brief cited `:8`, which is `publish.workspace`) and are test fixtures built
  only for the `wasm32-wasip2` integration tests. Publishable in principle,
  meaningless in practice. Separately, the version field is inherited from
  `Cargo.toml:32` everywhere, `CHANGELOG.md` (668 lines) is Keep-a-Changelog +
  SemVer with the preamble at `:1–6` and a single `## [Unreleased]` at `:8` —
  **no released version has ever been cut**, consistent with row 39's zero tags
  — and `CONTRIBUTING.md` has **no release-process section** (headings: Development
  setup, Looking for a first contribution?, Before opening an issue, Branches /
  commits / PRs, Required checks, Code expectations, Tests, Quality gates,
  Architecture decisions, Documentation; "release" appears only at `:135` and
  `:155` as pointers to `TESTING.md`).
- **Row 40 — one claim was sharpened rather than contradicted: the update check
  is NOT dead code.** The brief called `crates/cli/src/update.rs:1–40` a
  "dead-code note". It is **wired**: `crates/cli/src/lib.rs:182–185` computes
  `config.updates.as_ref().is_none_or(|u| u.check_on_startup)` and calls
  `check_for_updates()`. What is inert is its **target** — the hardcoded
  `https://crates.io/api/v1/crates/concerto` (`:14`) for a crate this workspace
  does not publish — so the check cannot succeed until a publish actually
  happens, and `update_endpoint` stays unwired either way. Writing "dead code"
  would have been falsified by one grep for the call site; the appendix records
  the corrected version and cross-references Closed #22(c).
- **Row 40 — the desktop coupling claim was true in spirit and false in
  detail, so the precise form is recorded instead.** The brief said
  "`crates/desktop/Cargo.toml:12-22` public API takes
  concerto-orchestrator/sessions/memory types". Two corrections: the internal
  block is **`:12–24`** (13 edges, not 11), and the public surface does **not**
  name orchestrator or memory types. What is public is
  `pub bus: concerto_core::event::EventBus` (`app.rs:303`, in `pub struct App`
  at `:278`), `pub cancel_token: concerto_core::CancellationToken` (`:317`),
  `pub config`/`pub global_config` as `concerto_config::AppConfig` (`:304`,
  `:307`), `pub git_summary: Option<concerto_tools::git::RepositorySummary>`
  (`:426`), `pub plugin_manager: Option<concerto_plugins::manager::SharedPluginManager>`
  (`views/settings/state.rs:176`), and two `pub fn` signatures
  (`views/diff.rs:105`, `views/tool_log.rs:102`). `concerto-orchestrator` and
  `concerto-memory` **are** depended on (`:14`, `:13`) and used, but via
  **private** `use`s (`app.rs:35–40`, `services/session_handler.rs:16`); a
  search for `pub use concerto_*` in `crates/desktop/src` returns **no files
  found**. The coupling is real, but it is a *dependency-edge* coupling — so the
  register does not claim a public-API coupling it cannot demonstrate.
- **BOTH rows — their `TODO.md` and `ROADMAP` cites had drifted off the items,
  and the corrections are recorded in the appendix.** `TODO.md:260–263` is the
  **audit M-05 oversized-module** entry (`:257–261`); `TODO.md:264–265` is the
  **audit M-02 decorative-cancellation** entry (`:262–267`). The real entries are
  **`docs/TODO.md:271–274`** (installers) and **`:275–276`** (publish), under the
  `## Release` heading at `:269`. `ROADMAP:194` is a line **inside the
  certified-evolution bullet** (`:192–196` — row 32, already cut as Closed #20),
  so **both release rows were citing another closed row's source**; the real
  cite is the combined **`ROADMAP.md:197–198`**. `STATUS.md:11–14` and
  `:12–13` are correct and untouched. `docs/TODO.md`, `ROADMAP.md` and
  `docs/STATUS.md` are **not modified by this pass** and remain the source of
  truth for the release track.
- **Follow-ups flagged, not actioned here** (outside this pass's scope):
  - `docs/TODO.md:271–274` still says installers are "Not started" and, via its
    `CHANGELOG 0.1.0-alpha "Release workflow building .tar.gz for ubuntu +
    macos"` reference, implies a release workflow that is now known to be
    unexercised (zero tags). `ROADMAP.md:197–198` still says "currently only a
    `.tar.gz` release build exists", which understates `release.yml`. Neither
    source document records the row cut. A future pass should reconcile both
    against Closed #22.
  - `crates/config/src/schema.rs:941–942` — `[updates].update_endpoint` is read
    by no code. Either wire it into `crates/cli/src/update.rs` (replacing the
    hardcoded `CRATES_IO_API` at `:14`) or drop the field and the
    `docs/config.toml.example:240` example, so the config surface stops
    promising a capability that does not exist. **This is a source change and is
    deliberately not made here.**
  - `crates/concerto/Cargo.toml:17–18` optional `cli`/`desktop` deps carry no
    `version`, which will block a publish of the `concerto` crate specifically —
    i.e. it sits directly on the *recommended* subset from Closed #23, not on a
    discarded one.
  - Still carried forward from the prior passes and untouched:
    `docs/security-threat-model.md` §6 gaps #1, #5, #6; `docs/TODO.md:229–235`;
    `ROADMAP.md:241–245`; the `docs/STATUS.md:346–349` mirror of cut row 28; the
    git-ignored `docs/research/certified-universal-evolution.md` cites in
    `docs/TODO.md:297–304` and `ROADMAP.md:192–196`; and the `— manual` orphan
    fragment in the 2026-09-24 notes, still unrepaired.
