# ADR-71: Coordinator Supremacy — the coordinator is the sole master of a run

**Status:** Accepted

Composes with ADR-60 (process-per-agent supervisor, single write gate),
ADR-58 (only the coordinator is hardcoded), ADR-64/65 (evidence spine and
reuse as **coordinator context**, never a compiled authority), and ADR-66
(harness fail-loud). **Partially supersedes** ADR-19, ADR-55, ADR-58, ADR-64,
and ADR-65 on exactly the revoked points enumerated in §3's conflict table.
None of those five records is moved to `archive/`: the supersession is
scoped, not full, and each row in the ADR README carries a note to that
effect. Supersedes: none in full.

**Date:** 2026-09-24

**Deciders:** Concerto architecture + maintainer direction

> **Reconciliation note (2026-09-24, commit `cbb09b0`):** §5 ("Intent routing —
> `route()` retained as a deprecated pure advisor with inert keyword lists"),
> the matching consequence ("Deprecated-but-retained `route()` ... carry a
> small maintenance tax; removing them is a follow-up"), T4 ("`route()` and its
> keyword lists remain inert"), and the verification note ("`route()` — pure;
> zero non-test production callers") were superseded the same day: `route()`
> and its keyword corpora were **deleted** from `crates/core/src/intent.rs`.
> The module now retains only the intent **vocabulary** (`RequestedOutcome`,
> `TaskScope`, `RouterOutput`, `RouterRoute`, `RunStage`, `PlanDecision`,
> `LOW_CONFIDENCE_THRESHOLD`) — the decision to make it advisory evolved into
> removing it entirely. T4 is satisfied trivially (no `route()` exists to
> consult), and the "remove them as follow-up" tax no longer exists. The core
> ADR-71 decision (no intent topology branching; Coordinator owns run shape)
> is unaffected and remains in force.

## Context

Concerto's orchestration has accumulated multiple sources of authority that
compete with the coordinator for the right to decide what happens in a run:
hardcoded cycle terminals, an LLM planner whose output was treated as a
binding task graph, a registry that doubled as the resolvable roster, an
intent router that branched orchestration topology, and a lineage of
"compiled schedulers" (ADR-64's resolver-as-dispatch-authority, ADR-65's
`evidence_scheduler`) that translated evidence into dispatch decisions
behind the coordinator's back.

These authorities share two failure shapes:

- **Silent division of authority.** When anything other than the Coordinator
  can end a run or pick an agent, the reason a run did what it did is no
  longer a single auditable "Coordinator decided" record. The ADR-65
  evidence-spine adoption already exposed this: the `evidence_scheduler`
  became "a pipeline authority" and had to be removed (2026-09-05
  amendment), reverting dispatch to the Coordinator's `call_specialist`
  tool. Authority kept creeping back in compiled form.
- **Guards that look like errors but are really policy.** A gate denial, a
  write conflict, a context overflow, and a verification failure all end
  *something* — but they are not the same kind of run end, and conflating
  them obscures both the taxonomy and the fix. A policy denial is final for
  one operation but is still a **tool result** the Coordinator routes; a
  verifier failure is genuinely an error.

The governing principle (ADR-58) already says only the coordinator is
hardcoded. ADR-71 makes that principle load-bearing and complete: the
coordinator is the **sole master** of a run — it decides all work, all
dispatch, all ordering, all agent selection, and all termination. Every guard
and every scheduler is demoted to one of two things: a *sensor* that reports
to the coordinator, or an *advisory input* the coordinator consults. Nothing
else ends a run.

## Decision

### 1. Principle — the coordinator is the sole master

1. **One run, one master.** The coordinator owns every execution decision for
   a run: what work exists, what is dispatched, in what order, to which agent,
   and when the run stops. No compiled scheduler, router, registry, planner, or
   guard authoritatively decides any of those.
2. **An instruction runs until (a) the work is done, (b) the user
   intervenes, or (c) the coordinator errors.** These are the only run-level
   endings. A completed objective is a **Coordinator decision** — even when a
   supervisor lifecycle terminal fires cleanly, that terminal is reached
   because the Coordinator concluded the run, not because the terminal
   decided it (§"supervisor lifecycle without SubTask dispatch").
3. **All agent errors flow to the coordinator.** Every failure, denial,
   conflict, and guard finding surfaces to the Coordinator as a result to be
   routed — never around it. Where the current tree already does this it is
   affirmed: `agent_loop.rs` records a literal `gate-policy-denied`
   Coordinator Decision for every policy denial, and `coordinator.rs`
   "records Coordinator Decisions on all bypass paths" (current branch
   `fix/coordinator-error-invariant`).
4. **The coordinator is itself fallible.** Arbitration favors
   machine-recorded facts (ADR-65 §Context), but facts are **advisory
   context for the Coordinator's decision** — they never compile into a
   decision behind it. The coordinator decides; evidence informs.

### 2. Terminal taxonomy — one of exactly four classes

Every way a run (or a substantive unit of work inside it) ends is classified
exactly one way. This is the vocabulary for "what ended this run" everywhere
in the audit trail and UI.

| Class | Meaning | Members |
|---|---|---|
| **Coordinator decision** | The Coordinator concluded the objective is met, blocked, or not worth continuing; it terminates the run and records the reason. | Completed / Blocked / Partial outcomes decided by the Coordinator (ADR-60 S5 `Completed` semantics reached via a Coordinator decision). |
| **Intervention** | The user changed or stopped the run. Consent and interruption are user events, never derivable by the Coordinator or any model. | User stop/abort, mid-run user input, approval dialog outcomes. |
| **Coordinator error** | The run ends because the coordinator (or a sensor on its behalf) hit a genuine error that is not a safety guard and not user action. | Verifier failure; acceptance-gate failure; harness fail-loud (ADR-66); context-budget overflow surfaced by the context guard; write-conflict (`GateError::Conflict`, ADR-60 D5) escalating past retry. |
| **Immutable safety terminal** | A hard, non-overridable boundary. These end *authoritatively* without being coordinator errors: the Coordinator cannot, by decision, cross them. | **Deny-class** — a policy `Deny` is final for that operation, never upgradeable to `Allow` (ADR-55 §2); the op is an operation-level terminal, while the run continues via the denial routed as a tool result. **Approval consent** — mutation authority comes only from confirmed user decisions; the Coordinator cannot grant itself. **Plan binding** — an approved plan binds execution; silent re-decompose is forbidden (ADR-60 D7); divergence requires explicit user re-approval. **Budget caps** — run-level spend/cycle caps are hard bounds the **Coordinator counts against** (ADR-52 global run cap, spend caps, stage cycle caps), never a scheduler's counters. **Supervisor lifecycle without SubTask dispatch** — a supervisor lifecycle terminal that ends a run without dispatching subtasks (clean-exit-after-handshake `Completed`) is a safety terminal, reached under a Coordinator decision. |

Classification of the existing guards-that-end-runs, in one sentence each:

- **Gate denial (`gate-Denied`) / policy denial** → a **coordinator-routed
  tool error** (immutable for the op, routed for the run): the denial is final
  for that operation, the tool result returns to the Coordinator, which
  decides next.
- **Verifier failure, acceptance-gate failure, harness fail-loud,
  context-guard overflow, gate conflict** → **genuine-error terminals**
  (Coordinator-error class): they end the run only after the Coordinator
  routes them as errors.
- **Design-doc quarantine** (ADR-65 §5) → **advisory**: a quarantined doc is a
  machine-checkable flag the Coordinator acts on (revise / skip / proceed
  without a doc); it never ends a run by itself.

### 3. Conflict table — the revoked points and their resolutions

| # | Prior decision | Revoked point | ADR-71 resolution |
|---|---|---|---|
| 1 | ADR-19 §6 "Cycle detection strategy" | Hardcoded cycle terminals: Rule A (`same (AgentRole, task_hash)` 3× no progress) and Rule B (same `Issue.description` 2× + zero net change) emit `OrchestratorCycleDetected` and return a `CycleDetected` error on their own. | **Hardcoded cycle terminals → coordinator-owned guards.** Cycle/loop limits are guard *signals* the Coordinator owns and reacts to (continue / reset / abort are Coordinator decisions, as ADR-19 already had at the UI). A guard never ends a run by itself. |
| 2 | ADR-19 §10 "Task decomposition via LLM planning"; ADR-55 Phase-2b §1 (planner contract: "requires implement roles and at least one Coder task, which settles that registry-subsetting was refuted") | The planner's LLM plan (or its heuristic fallback) was treated as the binding task graph; its roster contract constrained who could be planned. | **Planner advisory-only.** A plan is advice to the Coordinator. The Coordinator decides the task graph it dispatches; planner output (and the planner's assumptions about the roster) never binds dispatch. |
| 3 | ADR-58 amendment (2026-09-05) "registry is the roster", carried into the orchestration runtime-bridge plan | The config/registry built from `custom_agents` was treated as the complete binding roster the Coordinator mechanically follows. | **Registry-is-roster advisory.** Registration is an advisory roster the Coordinator consults. ADR-58's "only the coordinator is hardcoded" is **affirmed and extended**; only the registry-as-binding roster point is scoped-superseded. |
| 4 | ADR-55 Phase-1e §2 (outcome → topology: `Execute` + `!read_only` → full topology; otherwise text-only / coordinator-only) | Intent routing branched orchestration topology: the routed outcome selected the run shape. | **No intent topology branching.** Intent never selects topology. The Coordinator owns the run shape; intent signals are advisory inputs (see §5). |
| 5 | ADR-64 §3/§4/§7 — pre-dispatch resolver as dispatch authority (`should_dispatch` verdicts `Reassign`/`CoordinatorTakeover`, plan-reuse as a planner-skip authority, role-agnostic scheduling directives); ADR-65 §6 `evidence_scheduler` (compiled dispatch function, already removed by the 2026-09-05 amendment) | Compiled schedulers that select agents, order dispatch, or end runs from evidence. | **Compiled schedulers revoked EXCEPT the resolver-as-reuse-oracle.** `resolve_batch` / `should_dispatch` are codified as a **pure reuse oracle** — never selects between agents, never orders; `Reuse` skips identical settled work, all other verdicts flow to normal dispatch (ADR-65's `evidence_scheduler` removal is affirmed). This subset is **compatible-with-supremacy, not revoked** (§4). |
| 6 | ADR-65 §5 (DesignDoc verifier lifecycle, quarantine); ADR-58 acceptance/run-once terminal stage kinds; ADR-61/67 context guard; ADR-60 D5 gate conflict; ADR-66 fail-loud | Mixed semantics for "guards that end runs". | **Guards-that-end-runs classified** per §2's taxonomy: verifier / acceptance / harness-fail-loud / context-guard-overflow / gate-conflict = genuine-error terminals; gate-Denied / policy-denial = coordinator-routed tool errors; design-doc quarantine = advisory. |

### 4. The reuse oracle survives — codified, bounded

`should_dispatch` (`crates/orchestrator/src/resolver.rs`) and `resolve_batch`
(`crates/orchestrator/src/resolver_integration.rs`) implement ADR-64 §3's
pure verdict set. Under this ADR they are explicitly **not** a scheduler and
are codified as exactly this contract:

- Pure `DispatchDecision { Reuse, Refine, Reopen, Dispatch }` — never
  `Reassign`/`CoordinatorTakeover`; agent selection and fallback are
  Coordinator judgment per the ADR-42/45 ladder.
- **Never selects between agents, never orders.** It only answers one
  question per ready work item: "is there identical, settled, currently-valid
  work that can be injected instead of dispatched?"
- `Reuse` → skip identical settled work (cached result injected; zero model
  dispatch). All other verdicts (`Refine`, `Reopen`, `Dispatch`) flow to
  **normal dispatch** — the Coordinator's `ready_tasks()` batch, unchanged.

This is compatible with supremacy because it *reduces* wasteful dispatch
without ever deciding what the Coordinator dispatches. Within the ready
batch, ordering is an internal Coordinator-loop detail (the pure
`schedule_batch` scoring in `scheduler.rs`); it is a mechanism the
Coordinator uses to run its own loop, not an authority over it.

### 5. Advisory-only inputs

The following are inputs the Coordinator may consult — and, in the current
tree, are no longer the authorities the pre-ADR-71 design sometimes intended:

- **Intent routing** — `route()` (`crates/core/src/intent.rs`) is retained as
  a **deprecated pure advisor**: it is deterministic and pure, and currently
  has **zero non-test production callers** (verified in tree on 2026-09-24).
  The keyword lists it ships (`EXECUTE_KEYWORDS`, etc.) are **inert** — kept
  for the pure function's contract and tests, not consulted as a routing
  authority. `coordinator.rs::decide_run_shape` already treats the routing
  hint as "an input, not a verdict" (advisor mode), with the Coordinator
  overriding it from session context — affirmed here.
- **Planner output** (ADR-19 §10 / ADR-55 Phase-2b) — advisory (§3.2).
- **Registry / roster** (ADR-58 amendment) — advisory (§3.3).
- **Evidence spine** (ADR-65) — facts, claims, snapshots, read dedupe, and
  resume remain the machine-recorded context the Coordinator acts on; they
  are advisory, never a compiled decision.
- **Design-doc quarantine** (ADR-65 §5) — advisory (§2).

### 6. Scoped partial supersession declarations

Each declaration is scoped to exactly the revoked point in §3; everything
else in the named ADR remains in force (the records stay in place, not in
`archive/`):

| ADR | Scoped supersession by this ADR |
|---|---|
| **ADR-19** (Multi-Agent Orchestration) | On §6 (hardcoded cycle terminals → coordinator-owned guards) and §10 / planner authority (planner advisory-only). |
| **ADR-55** (Intent routing and authorization) | On Phase-1e §2 (outcome → topology branching → no intent topology branching) and Phase-2b §1 planner roster contract (planner advisory-only). The three-tier gate, never-grant invariant, and user-event-only authorization are untouched (see §7). |
| **ADR-58** (Configurable orchestration) | On the amendment's "registry is the roster" as a binding semantic (registry advisory). The "only the coordinator is hardcoded" principle is affirmed and extended. |
| **ADR-64** (Timeline zero-waste) | On §3/§4/§7 compiled-scheduler authority, EXCEPT the §3 pure-verdict reuse oracle (`Reuse`/`Refine`/`Reopen`/`Dispatch`) which is codified as compatible (§4). |
| **ADR-65** (Evidence spine) | On §6 `evidence_scheduler` as a compiled authority (removal affirmed) and §5 quarantine reclassified advisory. The facts/snapshot/resume/read-dedupe spine is untouched. |

### 7. Explicitly NOT revoked

To forestall misreading, these stay fully in force:

- **ADR-55/56 intent gate and authorization.** The gate is the only routing
  path; the classifier can classify, never grant; `AuthorizationState`
  transitions only on confirmed user decisions; `Deny` is final. ADR-56's
  model-first classification is **not** revoked — only intent *topology
  branching* is.
- **ADR-58 "only the coordinator is hardcoded"**, now extended to "the
  coordinator is the sole master."
- **ADR-60 supervisor, single write gate, WAL-before-execute, D5 conflict
  detection, D7 plan binding.**
- **ADR-52 safety gates and run caps**, re-cast as Coordinator-counted bounds
  (§2).
- **ADR-66 harness fail-loud** — reclassified as a genuine-error terminal but
  otherwise unchanged.
- **ADR-64/65 evidence spine** (facts, snapshots, read dedupe, resume) as the
  advisor to the Coordinator.

## Consequences

- **One decision record for "why the run ended."** Every termination is
  exactly one of the four taxonomy classes, so the audit trail answers "what
  ended this run and whose call was it" without dissolving into per-guard
  vocabularies.
- **No authority behind the Coordinator.** Schedulers, routers, registries,
  and planners inform; the Coordinator decides. The evidence-scheduler
  regression is structurally prevented because there is no compiled
  decision slot left to fill.
- **Safety stays immutable.** Deny-class, consent, plan binding, budget caps,
  and supervisor lifecycle terminals are the five things the Coordinator
  cannot overrule — they are why removing compiled authority does not weaken
  the run's safety posture.
- **Reuse that cannot drift into scheduling.** The reuse oracle is bounded by
  a four-verdict pure contract, so zero-waste savings survive without
  re-creating a scheduler.
- **Costs / risks.** The Coordinator is a model and remains fallible; the
  mitigation is the immutable-safety layer plus machine-recorded advisory
  facts, never a compiled scheduler. Deprecated-but-retained `route()` and its
  inert keyword lists carry a small maintenance tax; removing them is a
  follow-up, not part of this decision.

## Verification notes (in tree, 2026-09-24)

- `route()` (`crates/core/src/intent.rs`) — pure; **zero non-test production
  callers** (grep across `crates/`).
- `coordinator.rs::decide_run_shape` — routing hint treated as "an input, not
  a verdict" (advisor mode).
- `resolver.rs` — `DispatchDecision` closed over `{ Reuse, Refine, Reopen,
  Dispatch }`; `should_dispatch` pure. `resolver_integration.rs` —
  `resolve_batch` short-circuits `Reuse`, routes all other verdicts to normal
  dispatch.
- `agent_loop.rs` — `gate-policy-denied` recorded as a Coordinator Decision;
  denial semantics unchanged (call stays denied, result routed).
- `scheduler.rs` — pure `schedule_batch` batch-order scoring (internal
  Coordinator-loop mechanism, not an authority).
- `GateError::Conflict` → `IpcErrorCode::Conflict` retryable tool error
  (ADR-60 D5). `ContextGuardProvider` → typed context-overflow error.

## Acceptance criteria

- **T1** — Every run-level terminal in the audit trail maps to exactly one of
  the four taxonomy classes.
- **T2** — No compiled scheduler selects an agent or ends a run; the reuse
  oracle's verdict set is closed over the four pure verdicts and only `Reuse`
  short-circuits dispatch.
- **T3** — A policy denial and a gate conflict do not end a run by
  themselves; they return to the Coordinator as routed tool results.
- **T4** — `route()` and its keyword lists remain inert: no production code
  path consults them as a topology or dispatch authority.
- **T5** — ADR-19/55/58/64/65 remain in place with scoped supersession
  status; none is archived.

---

*Last updated: 2026-09-24*