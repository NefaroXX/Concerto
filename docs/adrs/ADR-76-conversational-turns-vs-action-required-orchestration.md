# ADR-76: Conversational Turns vs. Action-Required Orchestration

**Status:** Accepted

**Refines** [ADR-71](./ADR-71-coordinator-supremacy.md) (coordinator
supremacy) and [ADR-74](./ADR-74-delegation-doctrine-and-ladder-hold.md)
(delegation doctrine) — it contradicts neither. ADR-71 settled *who* decides a
run (the Coordinator, alone) and ADR-74 settled *what the Coordinator may
decide with* (delegation by default for implementation/review/validation).
This ADR settles a third, orthogonal axis: **which turns carry a mandatory
specialist-dispatch requirement** and which leave the decision wholly to the
Coordinator's judgment. Supersedes: none in full. Number 76 is the next free
number after ADR-75; no ADR number has been reused.

**Date:** 2026-10-01

**Deciders:** Concerto architecture + maintainer direction

**Implemented by:** issue #145 on `dev` (branch base `f7da416`);
`TaskExecutionMode::CoordinatorDecides`, the runtime entry's structural mode
classification, and the ActionRequired-scoped prose-only guard.

## Context

Under full local agency (ADR-55 §1, ADR-71), the runtime set
`action_required = true` for **every** non-forced run. The coordinator owned
every turn, but the task mode that reached its decision loop was always
`TaskExecutionMode::ActionRequired`. Two consequences followed.

First, the coordinator's decision loop carries a **prose-only dispatch guard**
(`run_dispatch_session`): when an ActionRequired session replies without a
tool call and the dispatch graph is empty, the loop re-prompts the model with
an explicit dispatch instruction up to `MAX_PROSE_STOP_REPROMPTS` times, then
escalates to the planning-recovery ladder and, failing that, pauses the run
`Partial` with a preserved checkpoint. This is correct for real work — an
ActionRequired run whose completion claim rests on an empty graph is
vacuously true ("nothing to do" degenerates into "everything done"). It is
wrong for ordinary conversation. A message such as `Hi there` reached the
coordinator as ActionRequired, was re-prompted five times to dispatch a
specialist, and could end `Partial` despite a perfectly good direct answer.

Second, there was no mode in the vocabulary for "the coordinator owns this
turn and may answer directly **or** delegate if the work needs it". The
existing modes were `AnswerOnly` (no orchestration/work permitted; the
single-agent loop reads prose as completion) and `ActionRequired` (work is
mandatory). Neither expresses the conversational-but-action-capable middle
ground that the coordinator's own decision loop actually implements.

The temptation to avoid is a hard-coded hello/hi intent router. ADR-71 makes
the coordinator the sole decision-maker; a keyword table that pre-classifies
"conversation" from "work" reintroduces exactly the compiled authority ADR-71
revoked, and would misroute any phrasing the table did not anticipate.

## Decision

### 1. Three execution modes, distinguished by *requirement*, not *capability*

`TaskExecutionMode` carries three modes:

| Mode | Orchestration permitted | Specialist dispatch mandatory |
|---|---|---|
| `AnswerOnly` | No | No |
| `CoordinatorDecides` | **Optional** (coordinator's call) | No |
| `ActionRequired` | Yes | **Yes** |

The mode is a property of the **run's requirement**, not of the coordinator's
capability. Executive capability (tool support, top topology resolution, the
supervised path) is a separate predicate keyed on "not AnswerOnly"; the mode
alone decides whether the mandatory-dispatch guards arm.

### 2. The runtime entry classifies structurally; the coordinator decides

The runtime entry (`run_shared_agent`) classifies the **run shape** — not the
user's words — into an execution mode:

- an approved-plan Apply is real work ⇒ `ActionRequired`;
- a checkpoint-governed resume continues prior (possibly action-required)
  work ⇒ `ActionRequired`, so a stalled action-required run cannot resume
  into a prose-exempt mode;
- a forced single-agent run keeps the action-capable `ActionRequired` loop
  (unchanged);
- **every other coordinator-owned turn ⇒ `CoordinatorDecides`**, which is the
  default for ordinary conversation and informational requests.

This rule is a structural shape decision (Apply / resume / forced-single-agent
/ otherwise), never a string match. When the shape is ambiguous the entry
defaults the coordinator path to `CoordinatorDecides` and lets the coordinator
decide — per ADR-71, the coordinator remains the sole decision-maker for the
turn.

### 3. Only `ActionRequired` arms the mandatory-dispatch guards

> **Amended by the [addendum](#addendum-2026-10-04-the-obligation-model-and-combined-dispatch-guard)
> below:** mode is the *first* arm of the combined guard, not the only one.
> The statement in this section describes the mode term as shipped with issue
> #145; the same merge train on `dev` later added the obligation and
> promised-plan arms.

Every guard that enforces "an action-required run cannot close on an empty
dispatch graph" is scoped to `matches!(mode, TaskExecutionMode::ActionRequired
{ .. })`:

- the prose-only dispatch guard and its bounded re-prompts in
  `run_dispatch_session`;
- the prose-only escalation in `decompose_task` / the evidence-resume path;
- the vacuous-completion and zero-work guards in `execute_graph`;
- the unattempted-implementation and resume-drive guards.

`CoordinatorDecides` behaves like `AnswerOnly` for guard **exemption** (prose
completion is a valid outcome) while, unlike `AnswerOnly`, permitting
delegation. The guard is not made permissive: for an `ActionRequired` task
with an available capable specialist, the coordinator still cannot satisfy the
task by prose alone. This ADR does **not** globally disable `ActionRequired`.

### 4. No word router

The classification is structural only. No hello/hi/greeting corpus, no
keyword table, no model call classifies an utterance as conversational. The
coordinator's own decision loop decides whether a `CoordinatorDecides` turn
requires work; if it does, `call_specialist` remains available and the same
delegation doctrine (ADR-74) applies.

## Consequences

- `Hi there` completes with a direct coordinator response, zero specialist
  dispatches, and no manufactured delegation, on a `CoordinatorDecides` root
  task. Covered by
  `coordinator_decides_prose_only_run_completes_without_dispatch`.
- A `CoordinatorDecides` turn that the coordinator judges to require work
  still dispatches normally — the mode removes the *requirement*, not the
  *capability*. Covered by `coordinator_decides_run_may_still_dispatch`.
- An `ActionRequired` run that closes on an empty graph is unchanged: five
  bounded re-prompts, the planning-recovery escalation, then `Partial` with a
  preserved checkpoint. Covered by the existing
  `prose_only_action_required_run_is_partial_and_resumable`.
- Cancellation, retry, dispatch, iteration caps, spend tracking, and the
  checkpoint/resume path are untouched: the mode only changes which guard
  predicate arms.
- `AnswerOnly` is unchanged: still a no-orchestration mode read by the
  single-agent loop.

## Rejected alternatives

- **Hard-coded greeting router.** Reintroduces the compiled authority ADR-71
  revoked; brittle against unanticipated phrasing.
- **Globally relaxing the prose-only guard.** Would let genuine
  action-required work close on prose, defeating ADR-74's mandatory-dispatch
  invariant and the vacuous-completion guard's purpose.
- **A fourth mode or a separate "conversation" entry path.** The coordinator
  already owns every turn (ADR-71); the missing piece was the mode vocabulary
  and the guard's scope, not a new orchestration path.

## Addendum (2026-10-04): the obligation model and combined dispatch guard

**Amends §3 in place — no new ADR number, no supersession.** The merge that
landed ADR-76 on `dev` also shipped `crates/orchestrator/src/obligations.rs`
(the `ObligationLedger`) and the coordinator's combined `dispatch_guard_arms`
predicate, so §3's "only `ActionRequired` arms the mandatory-dispatch guards"
is incomplete as written: mode is the first arm of a three-arm predicate, not
the whole of it.

### The obligation model

- **The graph is the source of truth; the ledger is a derived view.**
  `ObligationLedger::sync_from_graph` rebuilds obligations from the `TaskGraph`
  rows the checkpoint already persists (subtask statuses + dependency edges),
  so interrupt/cancel/retry/resume continuity rides existing persistence. There
  is no second obligation store, no keyword router, and no greeting list: the
  coordinator interprets intent and creates graph work; the module only
  validates the resulting state.
- **Work is declared or graphed, then enforced.** `declare_obligations` creates
  `Declared` (not-yet-dispatched) nodes — Outstanding obligations enforceable
  *before* any dispatch runs; `update_obligations` revises only
  still-undispatched work. Obligations chain investigate → implement → verify →
  explain along the graph's dependency edges and settle only through validated
  transitions (`Complete` / `Block` / `Fail` / `Retry` / `Supersede`); blocked
  and failed work must be retried, and terminal states stay terminal.
- **Evidence is structured, never prose.** A completion claim is backed by
  whiteboard event ids / artifact paths recorded by a validated `Complete`
  transition; an evidenceless `Complete` on an execution-kind obligation is
  rejected. Agent prose (`SubTask::deliverable`) is never copied into the
  ledger: a subtask synced as `Completed` takes its state from the graph (the
  authoritative store) and carries an **empty** `evidence` set until a
  transition fills it.
- **Conversation changes nothing.** A prose turn neither resets, satisfies, nor
  replaces obligations — the unchanged graph re-derives to the same view.
  Communication and execution stay concurrent (a mixed turn is lawful); the
  prose half discharges nothing.

### `dispatch_guard_arms` = mode OR open graph obligations OR promised plan

The coordinator's `dispatch_guard_arms(task, graph, all_files)` — the single
funnel shared by the prose-only, vacuous-completion, zero-work,
unattempted-implementation, and resume-drive guards — arms when **any** of:

1. **mode** — `requires_mandatory_dispatch(task)`, i.e. an `ActionRequired`
   run; or
2. **open graph obligations** — `obligation_dispatch_pending(graph)`: any
   Investigate / Implement / Verify obligation still open, in
   `CoordinatorDecides` exactly as in `ActionRequired`; or
3. **promised plan without code** — the run promised or was approved on a plan
   and produced no code artifact: an unattempted plan is work even over an
   empty graph.

Any one source arms; no source disarms another. §3 survives as term 1, not as
the whole predicate: `CoordinatorDecides` is prose-exempt *only* while it holds
no declared/graphed work and owes no code.

### Residual (explicit)

**The guarantee covers declared/graphed work only.** Work the coordinator never
declares with `declare_obligations` and never adds to the task graph during a
`CoordinatorDecides` turn is indistinguishable from conversation: ADR-71 makes
the coordinator the sole decision-maker for the turn, and §4 of this ADR
forbids a word router, so nothing may infer latent work from the user's
phrasing. A user who asks for real work that the coordinator chooses not to
declare or graph receives prose and no dispatch — by design, not by defect.
What is guaranteed: once work is declared or graphed, prose cannot close the
run over it, in any mode.
