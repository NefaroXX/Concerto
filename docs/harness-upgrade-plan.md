# Harness-upgrade plan — reconciled status and live task register

This is the reconciled implementation plan for the harness-upgrade effort
(H00–H14). It exists so a future owner can verify the plan's completion claims
without re-reading any conversation: every item names the artifacts it produced,
the evidence that establishes it, and what remains honestly open. It follows the
repo's existing tracking conventions — the outstanding-work register is
[`docs/DEFERRED.md`](DEFERRED.md), current behavior is
[`docs/STATUS.md`](STATUS.md), and priorities are [`ROADMAP.md`](../ROADMAP.md);
this document does not replace any of them.

**Last reconciled:** 2026-10-13, against branch `feat/h14-measurement-gates`
(off `dev` at `ac74b8b`).

The governing H01 scoping decision is
[`ADR-82`](adrs/ADR-82-harness-contract-surface.md); H10's decision is
[`ADR-83`](adrs/ADR-83-sandbox-backend-decision.md). Measurement buckets (assert
/ benchmark / live-procedure / declared-unmeasured) live in
[`docs/harness-measurement.md`](harness-measurement.md).

## Status

Legend: **Done** = landed on `dev` with in-tree evidence. **Done, scoped** =
landed with an explicit, declared ceiling. **Open** = not landed.

| Item | What it was | Landed as (PR / commit) | Evidence | Status |
| --- | --- | --- | --- | --- |
| **H00** | Grounding audit of the existing contract surface | design input to ADR-82; ADR-62 amended 2026-10-09 | `ADR-82` "Context" (the cited live defects); `ADR-62` 2026-10-09 amendment (VirtualFs staging correction) | Done |
| **H01** | Harness contract surface (extend, don't fork) — ADR-82 slices 1–7 | #296 (s1 canonical effect key), #297 (s2 `ToolFailure`), #299 (s3 outcomes), #300 (s4 gate), #301 (s5 checkpoint), #302 (s6 manifest), #303 (s7 memory evidence); then #308 (audit canonical columns, `d777203`), #310 (`cd12018`), #311 (`da6cf63`), #313 (deprecation removal, `1e82b18`) | `crates/core/src/{error,types,checkpoint}.rs`; per-slice compatibility + behavior tests named in `ADR-82` §"Implementation slices" | Done, scoped |
| **H01 (integration)** | First vertical slice end-to-end (release-gate seed) | #314 (`763e0b2`) | `crates/orchestrator/tests/vertical_slice.rs` | Done |
| **H02** | Typed error transport end to end (typed codes replace prose classification) | #304; construction-site `ToolError::Io` typing #309 (`313433b`, `43f2be4`) | `crates/core/src/error.rs` per-variant map + `crates/shell` conversion; `ADR-82` §1 | Done |
| **H03** | Agent-callable `environment.describe` | #305 (`948e3cc`) | `crates/orchestrator/src/environment_describe.rs`; descriptor tests in `crates/core/src/sandbox.rs` | Done |
| **H04** | Fresh-passing-evidence enforcement for the strict gate | #306 (`0e2ddf5`, `8032620`) | `crates/orchestrator/src/verify.rs` freshness predicates; `agent_loop::decide_exit` gate tests | Done (opt-in posture) |
| **H05** | `verify.run` — verification as a callable service | #318 (`6dc60b4`, `758325a`) | `crates/orchestrator/src/verify.rs` (adapters, coalescing, governed spawns) | Done |
| **H06** | Durable file checkpoints (pre-image bytes) | #301 (`2862f6d`, `fea01d6`) + chunked byte store #311 (`da6cf63`) | `crates/core/src/checkpoint.rs`; `crates/sessions/src/file_checkpoints.rs`; `crates/orchestrator/tests/file_checkpoints.rs` | Done, scoped |
| **H07** | Executor-seam pre-image / rollback dispatch; crash recovery (H07b) | pre-image capture in #301/#311; crash-recovery journal #319 (`f99168a`, `50c816c`) | `crates/orchestrator/src/file_checkpoint.rs`; restore journal + resume tests; `crates/orchestrator/tests/file_checkpoints.rs` | Done, scoped |
| **H08** | Managed experiment workspaces with review + promotion | #320 (`50fad05`) | `crates/orchestrator/src/experiment.rs`; `crates/orchestrator/tests/experiment.rs` | Done |
| **H09** | Restore-coverage preflight + managed DB / process adapters | #321 (`dafd84e`) | `crates/orchestrator/src/managed_rollback.rs` (honest-limits index); `crates/memory/src/db_checkpoint.rs`; `crates/orchestrator/src/managed_process.rs` | Done, scoped |
| **H10** | Portable sandbox capability interface + backend decision | #322 (`425eaef`, ADR-83 `84a7fdc`, carry-forward `a378735`) | `crates/core/src/sandbox.rs`; `ADR-83` | Done, scoped |
| **H11** | Evidence-bearing recall with deterministic status derivation | #323 (`1bbe65e`, `e3b96d9`, `2f6ee6a`, `44530fa`) | `crates/memory/src/evidence.rs`, `recall_status.rs`, `system.rs` (`retrieve_with_evidence`) | Done |
| **H12** | Observed maintenance outcomes + continuity proof | #324 (`4b08b1b`, `637faaa`, `85594dd`, `794384d`) | `crates/memory/src/maintenance.rs`; `crates/memory/tests/h12_acceptance.rs`, `row13_cascade.rs` | Done, with residuals |
| **H13** | Common harness access — one service seam, CLI/desktop, settings | #325 (this branch's base) | `crates/orchestrator/src/harness_service.rs`; `docs/harness-access.md`; `crates/orchestrator/tests/harness_equivalence.rs` | Done, declared coverage |
| **H14** | Measurement and release gates | this branch (`feat/h14-measurement-gates`) | `docs/harness-measurement.md`; two new orchestrator benches; three new asserting tests; the DEFERRED row below | Done (observational) |

## Live task register

Each row carries a source, a re-entry condition, and a size, matching
[`DEFERRED.md`](DEFERRED.md)'s maintenance rule. Rows already owned by the
outstanding register are referenced, not duplicated.

| # | Item | What remains | Source | Re-entry condition | Size |
| --- | --- | --- | --- | --- | --- |
| D-18 | Cross-process continue | `restore_graph`/`load_checkpoint` exist but no CLI/desktop path reconstructs and continues a run from its checkpoint. | `DEFERRED.md` row 18 | A CLI/desktop path that reconstructs a run from its checkpoint + e2e test | L |
| D-48 | External harness access | No external path meets the bar; needs its own run-scoped transport/auth/lifecycle decision. Declared out of scope in `harness-access.md`. | `DEFERRED.md` row 48 (new); `harness-access.md` §"External access"; `EXTERNAL_ACCESS_NON_COVERAGE` | Someone writes that decision | S |
| D-13r | Cascade reorder reachability | **Closed — arithmetic, not an observation gap.** Under the implemented key `cascade_key = rank + γ·(LINK_SCORE_MAX − score)` with `LINK_SCORE_MAX = 10.0`, swapping ranks `i < j` requires `(j − i) < γ·(score_j − score_i) ≤ γ·LINK_SCORE_MAX`. Ranks are integers, so `Mild` (0.05·10 = 0.5 slots) and `Aggressive` (0.10·10 = 1.0 slots, ties only) **cannot** reorder at all, and only `Emergency` (0.20·10 = 2 slots) can. This matches the `CascadeTier` doc comments verbatim, so the Mild/Aggressive tiers are **widening** tiers — their `candidate_multiplier` does real work when the fused set cannot fill `top_k` — not reordering tiers. The earlier framing ("Mild was not directly observed to reorder") implied a pending observation; there is nothing to observe. Evaluated and **rejected**: skipping the link-scoring pass at inert tiers. The scoring output is not only the reorder input — it is the corroboration axis of `MemoryEvidence::evidence_score` (`crates/memory/src/evidence.rs:398`), which reports how much link support a result has. Skipping it at `Mild`/`Aggressive` would have reported "supported by 0 links" for genuinely supported chunks to save a per-recall cost, so the pass now runs at every tier and `tier_can_reorder` documents the reachability arithmetic instead. Open, not closed: ADR-69 line 68 states the formula as a weighted sum `α·vector + β·bm25 + γ·link_score`, while the implementation uses a rank penalty; the A6 cap of 0.2 keeps its intent but changed scale meaning. Whether ADR-69 needs an amendment is an owner call, not settled here. | `crates/memory/src/rag.rs` (`tier_can_reorder`, `CascadeTier` docs); `ADR-69` A6 + the §formula line | An owner decision on whether ADR-69's stated formula should be amended to match the implementation | S |
| D-41r | Resume stress/test fidelity | **Closed** (branch `test/h18-resume-stress`, `bf3ef46`): the evidence is now N=3 consecutive interrupt/resume cycles on one session over a real SQLite `DecisionStore::load(MemoryDb)` hydrated fresh per cycle, each cycle interrupting at a different point, every cycle honestly interrupted with a resumable checkpoint and grounded in the disk-persisted architect decision. (4) decided (Option B): the Phase-0 recall already routes through `retrieve_with_evidence` (H13) and plan chunks are standing-free by construction (documented on `retrieve_memory_context`). Only bound remaining: the specialists are mocks (no live provider can run in CI, and the stop must be deterministic). | `DEFERRED.md` row 41 residual; `bf3ef46` | A CI-runnable live specialist (or external-provider interrupt) that keeps the run deterministic | S |
| D-09m | Multi-adapter restore reachability | A rollback across experiment effects is not a single coherent workspace restore: live process memory, non-managed processes/grandchildren, shell/network effects, FTS/vector/reindex state, and DB schema/DDL are unclaimed and refused (or qualified) by construction. `preflight` reports this honestly; the reachability ceiling stands. | `crates/orchestrator/src/managed_rollback.rs` honest-limits index; `ADR-83` §Residuals | A backend that claims (and failure-injection-proves) any listed kind, or an explicit acknowledged scope that still refuses them | M |
| D-17r | Eval breadth + live-flake quarantine | H14 adds a three-fixture `harness_behavior` corpus + procedure, but the live leg still defaults to the `standard` suite only, and there is no quarantine mechanism for a live-leg flake. | `DEFERRED.md` row 17; `docs/harness-measurement.md` §Live procedure | A second suite/model wired into the live leg + a flake policy | M |
| D-36r | Windows container sandbox | Inherited from ADR-72 §5; ADR-83 explicitly does not reopen it. | `DEFERRED.md` row 36; `ADR-83` §3 | A superseding ADR choosing (a) narrow audited unsafe, (b) the `windows` crate, or (c) Linux/macOS-only with loud docs | L |
| D-SB | Sandbox checkpoint backend | ADR-83 ships the capability interface only; the sandbox declares isolation and restores nothing. A future backend must declare per-effect support and failure-injection-prove each claim. | `ADR-83` §Residuals; `NO_SANDBOX_RESTORE_MILESTONE` | The evidence bar in `ADR-83` §Residuals is met | L |

Not in this register because they are postures rather than debt: the strict
verification gate is **opt-in by design** (default-off; H04 enforcement is
landed, and enabling it per-run is an operator choice), and the `[harness]`
settings are client/operator-controlled by design (`harness-access.md` §Shared
settings).

## How to verify a claim above

1. `git log --oneline` for each PR/commit in the status table.
2. Run the measurement buckets in `docs/harness-measurement.md` (assertions,
   benches, live procedure).
3. For scoped items, read the named "honest-limits index" or "§Residuals"
   section — the ceiling is stated there, not hidden.
4. For DEFERRED-owned rows, read the row in `docs/DEFERRED.md`; H14 did not
   reopen rows 13/18/31/41 (13/41 were already promoted with precise residuals;
   18 stays open) and added **one new row (48)** for external access.
