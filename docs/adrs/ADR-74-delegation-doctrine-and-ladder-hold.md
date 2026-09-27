# ADR-74: Delegation Doctrine — delegate by default, hold a rung before demoting it

**Status:** Accepted (implemented)

**Refines** [ADR-71](./ADR-71-coordinator-supremacy.md) (coordinator supremacy) —
it does **not** contradict it: the Coordinator remains the sole master of the
run, and every decision below is still a Coordinator decision the model or the
Coordinator's own loop makes. Supremacy said *who* decides; this ADR says *what
the Coordinator is permitted to decide with*. **Amends** [ADR-35](./ADR-35.md)
§8 ("Coordinator self-execution") on the exhaustion condition only — §8's
guardrails, the shared-executor rule, the `provider: "coordinator-self-execute"`
sentinel, and the `coordinator` reserved-id rule are unchanged.
**Refines** the [ADR-42](./ADR-42.md)/[ADR-45](./ADR-45.md) ladder by inserting
the agent axis ahead of provider escalation and by making the fallback a
*bridge* rather than a demotion. Composes with ADR-58 (config owns the roster),
ADR-59 (Studio is one surface over config), ADR-52 (budget caps are
Coordinator-counted), and ADR-62 (self-execution runs through the same
`ToolExecutor`, policy engine, and `VirtualFs` as every specialist). Supersedes:
none in full. Number 74 is the next free number after ADR-73; no ADR number has
been reused (09, 13, 15, 17, 18 and 51 remain unused, per the README's
consolidation note).

**Date:** 2026-09-27

**Deciders:** Concerto architecture + maintainer direction

**Implemented by:** four commits on `fix/coordinator-error-invariant`, all green
(4725 tests, fmt/clippy clean):
`5a22405` (prompt inversion), `269c344` (delegation guard),
`eefd45e` (ladder hold), `115f85c` (agent-axis takeover).

## Context

ADR-71 settled that the Coordinator is the sole master of a run and that
routing is not control flow. Two things were still wrong in the tree, and they
were the same wrong thing seen from two ends.

**The Coordinator was its own cheapest specialist.** The dispatch prompt
(`COORDINATOR_DISPATCH_PROMPT`, `crates/orchestrator/src/coordinator.rs:254`)
granted a blanket self-execution license — *"If no registered specialist fits
(or none is registered for the work at all), do the work yourself with your own
tools — you are a full agent"* — and the `<dispatch_budget>` advisory priced
every specialist call against doing the work in-house. "No specialist *fits*"
is a judgment the Coordinator is always free to make, and it is exactly the
judgment a model under dispatch pressure makes about the cheapest option. The
roster the operator configured was therefore advisory in the way that mattered
least: the Coordinator could route around all of it. That is a quiet
contradiction of ADR-58 ("config owns the pipeline") and ADR-71 §5
("registry / roster — advisory"), because an advisory roster the master ignores
is not a roster.

**Nothing recorded the difference.** When the Coordinator did the work with its
own `write`/`shell`/`git` tools, the result was indistinguishable from a
delegated dispatch in the decision journal and the event stream. There was no
verdict, no event, and no refusal — just work that happened.

**The ladder demoted a provider that was about to recover.** The planning
recovery path (ADR-42/45 semantics, owned by the Coordinator since the compiled
scheduler was removed) treated a `RetryExhausted` whose cause was throttling as
an ordinary provider failure and immediately retried the run on the fallback
pipes. A provider asking for a 3-second cooldown was abandoned at second 1 for
a different provider. The fallback pipe then became the planning pipe, and the
rung that was merely *breathing* was demoted for the rest of the run. Separately,
the per-subtask ladder explicitly **refused** to reassign a hard-failed subtask
to a same-stage peer — a rule that inverted the doctrine: swapping a *model* was
allowed, swapping to an equally-capable *agent* was forbidden.

**The gap that ties them together.** Both failures are the same class: the
system preferred a *swap* (a different provider, or the Coordinator itself) over
a *re-ask* (a different registered specialist, or the same provider after its
cooldown). The fix is one doctrine, applied on three axes.

## Decision

### 1. Delegation is the default; self-execution is an exhaustion case

The dispatch prompt is inverted (`5a22405`):

- **Delegation is the DEFAULT action** for implementation, review, and
  validation work. The prompt says so in the imperative, names the reason
  ("the registered specialists are the run's capacity"), and forbids the
  cost-substitution: *"Do not keep the work in-house because a call feels
  costly."*
- **The "you are a full agent" license is deleted.** Not softened, not
  re-scoped — deleted, and a regression test asserts the string is absent from
  the rendered prompt.
- **Self-execution is permitted ONLY on roster exhaustion**, enumerated in the
  prompt as exactly three cases: **(a)** the roster is empty, **(b)** the roster
  is disabled or unavailable, **(c)** delegation was attempted and genuinely
  failed. While any specialist remains that could do the work, the Coordinator
  must delegate.
- **Retry and re-target before "impossible".** A failed or
  `needs_revision` dispatch is retried with corrective notes, or the *same work*
  is re-targeted to another registered specialist, before the work is treated as
  undelegable.
- **The ledger is for auditability, not deterrence.** The prompt's step 4 no
  longer uses the decision record as a reason to avoid a call; the
  `<dispatch_budget>` advisory no longer discourages one. The run-wide ceiling
  (ADR-52) is re-framed as forcing *prioritization* — not as pricing a needed
  dispatch against in-house work.
- **Role selection is derived from the runtime roster.** A new "Selecting a
  specialist" section states the roster is data and that *this prompt hardcodes
  no roles*. Stage kinds appear only as *guidance* ("prefer the agent whose
  declared stage matches the work"), with the ADR-35 rule preserved verbatim: a
  stage tag is **context, not a dispatch rule** — never a fixed pipeline, and a
  stage-less agent is a full participant. No agent id and no role name is
  compiled into the prompt.

### 2. The guard — refuse coordinator self-mutation before any delegation

The prompt alone is not a control. `269c344` adds a machine-enforced guard
(`guard_self_execution`, `coordinator.rs:2889`) on the Coordinator's own
`ToolExecutor` tool-call path:

| Situation | Outcome |
|---|---|
| Mutating tool call, roster **non-empty**, run has recorded **no** delegation attempt | **Refused.** `EventKind::PolicyVerdict { tool_name, verdict: "Denied: delegation-required" }` is published, and the model reads a structured `{"error": "delegation_required", "verdict": …, "message": …}` back. No write, no shell, no git. |
| Mutating tool call, roster empty (or all-disabled — disabled agents never register) | Permitted, and recorded (§3) with reason `roster-empty-or-disabled`. |
| Mutating tool call, a delegation attempt is already recorded | Permitted, and recorded with reason `delegation-attempted`. |
| Any **read-only** tool call | Unrestricted. Always. |

Properties that make the guard a control rather than a nudge:

- **Fail-closed, with a named verdict.** The refusal is a first-class
  `PolicyVerdict` on the bus, so it appears in the same audit surface as any
  other policy denial (ADR-62) — not as a silent no-op or a prose complaint.
  The model is told *why* in its own tool result, so the loop can recover by
  dispatching rather than by retrying the same call.
- **A precise, narrow mutating set.** `is_mutating_self_execution_tool`
  (`coordinator.rs:2633`) classifies exactly `write`, `shell`, `git`, plus the
  `filesystem` tool's destructive operations (`write` / `delete` / `move` /
  `copy`). Read-only calls — `filesystem` `read`/`list`/`exists`, LSP, consult,
  investigate — are never classified, so the Coordinator keeps its
  evidence-gathering power in full. The Coordinator can still *look* at
  everything and *touch* nothing without delegating first.
- **The roster's stage composition is deliberately NOT inspected.** The guard
  asks one question — is the roster empty? — and never "is the right stage
  staffed?". Whether an operator staffs design, implement, review, or nothing
  is their business (ADR-58, ADR-35's open stage vocabulary). This is the
  load-bearing rejection of the tempting design; see §Rejected alternatives.
- **Derived from the checkpointed journal.** "Has this run delegated?" is read
  from the decision journal (`DecisionKind::DispatchSpecialist` entries), not
  from a live counter, so the guarantee survives a resume. A malformed or
  validator-rejected `call_specialist` is never recorded as a dispatch decision
  and therefore does not unlock self-execution.
- **Consistent with ADR-35 §8.** When the Coordinator does take the work itself,
  it still goes through the same `ToolExecutor`, policy engine, `VirtualFs`,
  approval sink, and cancellation token as every specialist, with the
  `provider: "coordinator-self-execute"` sentinel. The guard changes *when*
  self-execution is lawful, never *how* it executes.

### 3. Lawful self-execution is recorded, not just permitted

`EventKind::CoordinatorSelfImplementing { run_id, session_id, tool_name,
reason }` (`crates/core/src/event.rs:706`) fires on every mutating
self-execution that the guard allows, with `reason` naming the exhaustion case
(`roster-empty-or-disabled` or `delegation-attempted`). A *refused*
self-execution deliberately publishes **no** such event — the refusal is the
`PolicyVerdict`, and a refusal is not self-work. The consequence: coordinator
self-work is no longer indistinguishable from delegation in the event stream, so
"who did this?" is answerable from the audit trail alone.

### 4. Coverage is configuration — the agent axis before the provider axis

`115f85c` adds the roster half of the doctrine:

| Key | Type | Default | Meaning |
|---|---|---|---|
| `can_cover` | `Vec<AgentStage>` | `[]` | **Additional** stage tags this agent can cover beyond its own `stage` (e.g. an architect with `can_cover = ["implement"]`). |

- **`CustomAgentConfig.can_cover`** (`crates/config/src/schema.rs:2188`) is
  `#[serde(default)]` and **additive** — an existing config keeps its exact
  meaning and **`SCHEMA_VERSION` stays `8`** (the ADR-70 / ADR-73 precedent).
  The field is merged like every other agent field: a non-empty user
  `can_cover` wins, an empty one inherits the seed's
  (`merge_custom_over_seed`, `crates/orchestrator/src/registry.rs:87`).
- **Effective coverage = own stage ∪ `can_cover`**
  (`AgentRegistry::effective_coverage`, `crates/orchestrator/src/registry.rs:393`).
  Empty for a stage-less agent with no configured coverage. A stage-less agent
  that *does* list `can_cover` is fully eligible — coverage is data, not a
  property of having a stage.
- **`AgentRegistry::takeover_candidate(target_stage, exclude)`** (`registry.rs:413`)
  returns the
  first candidate in stable id order: **a same-stage peer first, then any agent
  whose coverage includes the target stage.** The excluded (failing) agent is
  never its own candidate, and a non-covering agent is never selected.
- **Ordering in `attempt_fallback_ladder`:** the agent axis is consulted
  **before the provider-escalation tiers** — after the same-agent default-model
  retry (ADR-42 tier 1, which changes no provider and no agent) and before
  ADR-45 tier 1b (default provider) and tier 2 (coordinator self-execution). A
  successful takeover returns immediately and publishes a ladder note; a failed
  or unattributable takeover publishes a note and the ladder continues. The
  takeover dispatch counts against the run-wide cap (ADR-52) and is guarded
  once per task by `specialist_takeover_attempted`, which is **checkpointed**
  (`GraphCheckpoint.specialist_takeover_attempted`, `#[serde(default)]`, so old
  checkpoints restore empty and simply allow one attempt).
- **Two axes, named as such.** Changing *which specialist* takes the work and
  re-targeting a *chosen specialist* to a different model/provider are separate
  choices; both are preferable to self-execution. The prompt states this so the
  model prefers re-targeting a specialist over demoting the run.
- **Config-driven v1, like ADR-43's `[skills]`/`[mcp]`:** the desktop Studio
  **round-trips** `can_cover` (`AgentConfig.can_cover` in
  `crates/desktop/src/views/orchestration_studio.rs`, both conversion directions)
  so a Studio save cannot silently drop it, but there is **no UI editor** for the
  field yet. Config is the source of truth.

### 5. Ladder hold — hold a cooling-down rung, bridge rather than demote

`eefd45e` makes the provider axis obey the same re-ask-before-swap rule:

- **The hint survives.** `ProviderError::RetryExhausted` gains a `retry_after`
  field, populated from the final attempt's provider delay
  (`crates/providers/src/retry.rs:523`), plus a `retry_after_hint()` accessor on
  `ProviderError` (`crates/core/src/error.rs:348`) that also reads the live
  `RateLimit` and `HttpStatus` variants and returns `None` when the provider gave
  no hint. Before this, the cooldown the provider asked for was discarded at the
  retry boundary.
- **A throttled exhaustion with a known `Retry-After` is HELD, not abandoned.**
  The Coordinator waits out the cooldown (cancellably) and retries **the same
  provider**. A rung that recovers stays the primary pipe: no swap, no
  demotion, no new decision about who is in charge.
- **The fallback, when it runs, is a BRIDGE.** Only when the hold budget is
  spent, or the rung still fails after its cooldown, does the fallback run — and
  the decision trail names it a bridge rather than an abandonment. The reasons are
  explicit: `planning-provider-hold`, `planning-provider-hold-recovered`
  (held *and* recovered on the same rung), `planning-provider-hold-failed-bridging`
  (the hold failed, so the bridge is running), and the bridge's own
  `{tag}-bridged-recovered` vs `{tag}-abandoned` pair.
- **Terminal classes are untouched.** Auth failures, 404/model-not-found,
  malformed requests, and capability refusals escalate exactly as before: a
  class that will not heal on a timer is not held. A *non-throttle* class that
  happens to carry a hint is likewise not held.
- **Bounded, and cancellation-prompt.** The once-per-run
  `planning_recovery_attempted` latch is **removed** and replaced by three
  named bounds plus a re-entrancy guard:

  | Constant | Value | Bounds |
  |---|---|---|
  | `MAX_PLANNING_HOLD` | `30s` | Ceiling on a **single** hold wait, whatever the provider's hint asks for. A rogue hint cannot park a run indefinitely. |
  | `MAX_PLANNING_HOLDS` | `2` | Holds per run. After the second, the rung bridges rather than waits again. |
  | `MAX_PLANNING_RECOVERY_ROUNDS` | `3` | Planning-recovery rounds per run, replacing the one-shot latch — recovery is no longer single-use, still bounded. |
  | `planning_recovery_in_progress` | — | Re-entrancy guard: a nested `decompose_task` failure cannot start a second, concurrent recovery. This carries the latch's recursion-stopping duty explicitly. |

  Cancellation is checked before and after the sleep, so an abort ends a hold
  promptly and never dispatches afterwards. All three budgets are run-scoped and
  reset at the start of every `run`.

### 6. Relationship to the settled records

| Record | Disposition |
|---|---|
| **ADR-71** (coordinator supremacy) | **Refined, not contradicted.** The Coordinator is still the sole master: §1 is a prompt doctrine it follows, §2 is a guard *it* enforces and can route around only by dispatching, §3–§5 are Coordinator-owned recovery decisions. Nothing here adds a compiled authority that selects an agent, orders work, or ends a run outside the Coordinator. §2's guard is a policy refusal — ADR-71's "immutable safety terminal" class, operation-level and final for the call, with the run continuing via the denial routed as a tool result. |
| **ADR-35 §8** (coordinator self-execution) | **Amended on the exhaustion condition only.** §8's trigger 1 was *"stage absence — no registered agent for a lifecycle stage"*; the lawful condition is now **roster exhaustion** (empty / disabled-unavailable / delegation-attempted-and-failed). A roster that is non-empty but unstaged for the work at hand is **not** an exhaustion case: the Coordinator must delegate to whoever is registered. §8's other content — the shared executor, the guardrails, the `coordinator-self-execute` sentinel, `self_execute_attempted`, the deferred self-review refinement — is unchanged, as is the `coordinator` reserved id, which is never registered from config and therefore never appears in the roster the guard inspects. |
| **ADR-42 / ADR-45** (fallback ladder) | **Refined.** The agent axis is inserted before provider escalation (§4), and the fallback becomes a bridge (§5). The "never reassign a same-stage peer" invariant is **deliberately reversed** by owner doctrine: the same-stage peer is the *preferred* takeover target. Tier numbering itself is unchanged. |
| **ADR-58 / ADR-59** (config owns the roster) | **Extended, in the same direction.** `can_cover` is more configuration data the roster owns, and the Studio round-trips it. No new UI surface. |
| **ADR-52** (safety gates) | Unchanged. A takeover dispatch counts against the run-wide cap; the hold and hold budgets are run-scoped bounds the Coordinator counts, not a scheduler's counters. |

## Rejected alternatives

| # | Alternative | Why rejected |
|---|---|---|
| 1 | **Gate delegation on the roster's stage composition** — refuse a coordinator self-execution unless the work's stage is staffed (the literal ADR-35 §8 "stage absence" trigger, as a hard gate). | **It constrains user customization.** A roster of one stage-less agent would be permanently un-deletable for implement work, and an operator who deliberately staffs only design would have the tool refuse to finish their own run. Staffing is the operator's decision (ADR-58); the guard asks only whether a roster exists. `delegation_is_never_gated_on_stage_staffing` exists to keep this from drifting back. |
| 2 | **Hardcoded role→capability / role→stage tables** — encode in Rust which agent can do which stage, and drive both the prompt's role guidance and takeover eligibility from that table. | **Same reason, and it also contradicts ADR-58/ADR-35's open-vocabulary stage design.** It would make a new agent unusable until someone edited and shipped code, which is precisely what `can_cover` as config data removes. Coverage is now data; `takeover_candidate_uses_configured_can_cover` covers a stage-less agent purely through config. |
| 3 | **A hardcoded escalation ladder that abandons a cooling-down provider** — keep the pre-`eefd45e` behaviour: a throttled `RetryExhausted` immediately escalates to the fallback pipes. | **Premature demotion.** It demotes a rung seconds before its cooldown elapses, and the fallback then silently becomes the planning pipe. Holding is strictly better whenever a hint exists; the terminal classes that must escalate still do. The hold is bounded (`MAX_PLANNING_HOLD`, `MAX_PLANNING_HOLDS`) so "hold" cannot become "hang". |
| 4 | **Ladder-hold option (a) — wait-then-escalate**: hold the run until the cooldown elapses, *then* escalate to a fallback anyway. | Considered and rejected in favour of **(b) bridge-then-resume**, which is what shipped. (a) pays the full wait **and still demotes**: it is strictly worse than (b) whenever the rung recovers, and strictly worse than the old behaviour when it does not. It buys nothing either way. |
| 5 | **Ladder-hold option (c) — capped wait**: wait up to a bound, then escalate, without ever retrying the rung. | Considered and rejected in favour of **(b)**, as above. (c) is the bounded-wait half of (a) — it limits the wait but keeps the demotion, so it inherits (a)'s defect (a recovered primary pipe is thrown away) while adding a timer that is a guess rather than the provider's own hint. What (b) needed was the cap *and* the resume, and the cap did ship — as `MAX_PLANNING_HOLD` bounding the hold, not as a substitute for it. |
| 6 | **Keeping the once-per-run planning-recovery latch** while adding holds. | Rejected: with a latch, a run gets exactly one recovery, so a hold that fails leaves the run with no further options. Replaced by a bounded round budget plus an explicit re-entrancy guard, which is strictly more capable and still cannot recurse. |
| 7 | **Routing the coordinator's own dispatch prompt through `PromptBuilder`** (to pick up `{working_memory}` and the stable-head path) as part of this work. | Deliberately out of scope. It is a real gap and it is recorded as a known gap in §Known gaps, not silently bundled into a doctrine change — the two changes have different risk profiles and different review surfaces. |

## Consequences

- **The configured roster is finally load-bearing.** A specialist the operator
  registered is now the capacity the Coordinator reaches for by default, and
  cannot be bypassed with a mutating tool before one dispatch is attempted.
- **The audit trail answers "who did this?"** A delegated dispatch, a recorded
  coordinator self-execution (with its reason), and a refused self-execution are
  three distinct, separately visible things.
- **Refusals are recoverable and legible.** The model gets a named reason and
  the operator gets a `PolicyVerdict` — the guard teaches the loop to dispatch
  rather than to stall.
- **Customization got wider, not narrower.** `can_cover` lets an operator make
  any agent cover any stage without a code change, and a stage-less agent is a
  full participant everywhere in the delegation path.
- **Rate-limited providers are used instead of skipped.** A throttled planning
  rung is held and retried; the fallback is a bridge that is labelled as one.
- **The same-stage peer is now the first escalation, not a forbidden one.**
  This *reverses* a previously deliberate invariant, and the reversing test
  (`ladder_hard_failure_takes_over_to_same_stage_peer`) replaces the test that
  asserted the old rule, so the reversal is visible in the diff rather than
  smuggled.
- **Costs / risks.** The guard adds a policy refusal to a path that previously
  never refused, so a run that used to self-execute now costs a dispatch round
  trip first. A hold trades wall-clock for provider fidelity (bounded at 2×30 s
  per run, cancellable). Agent-axis takeover consumes a real model dispatch
  from the ADR-52 cap. And the three-axis doctrine is more moving parts than
  the two it replaced, which is why each has a named test.

## Verification notes (in tree, 2026-09-27, HEAD `115f85c`)

- **Prompt doctrine** (`coordinator.rs`):
  `dispatch_prompt_makes_delegation_the_default_and_self_execution_an_exhaustion_case`
  asserts the rendered prompt contains "Delegation is the DEFAULT action", the
  enumerated exhaustion sentence, and "this prompt hardcodes no roles" —
  **and** that it does **not** contain "you are a full agent".
- **The guard** (`coordinator.rs`):
  `coordinator_mutating_self_execution_is_refused_before_any_delegation` (the
  named `PolicyVerdict` and the model's `delegation_required` result, plus the
  no-`CoordinatorSelfImplementing`-on-refusal assertion),
  `mutating_self_execution_classification_is_precise` (`write`/`shell`/`git`
  mutating; `filesystem` mutating only for `write`/`delete`/`move`/`copy`;
  read-only untouched),
  `coordinator_self_executes_when_roster_is_empty` (empty roster writes
  through the same executor and records `CoordinatorSelfImplementing` with
  `roster-empty-or-disabled` — the old stage-absence test was retargeted to the
  exhaustion condition), and `delegation_is_never_gated_on_stage_staffing` (a
  single stage-less roster agent is still delegation-eligible).
- **Ladder hold** (`coordinator.rs`):
  `planning_hold_recovers_on_same_provider_without_bridging`,
  `planning_hold_is_not_one_shot_and_can_hold_twice`,
  `planning_hold_observes_cancellation`, and
  `planning_hold_ignores_non_throttle_class_with_hint`.
- **Agent axis** (`registry.rs`): `takeover_candidate_prefers_same_stage_peer`
  (same-stage peer preferred, self excluded, non-covering stage rejected) and
  `takeover_candidate_uses_configured_can_cover` (a stage-less agent with
  `can_cover = ["implement"]` is eligible).
- **Ladder ordering** (`coordinator.rs`):
  `ladder_hard_failure_takes_over_to_same_stage_peer` — a hard-failed design
  subtask is rescued by `architect-alt` and the run reaches `Completed`, with
  the "Agent-axis takeover" note on the bus. It replaced
  `ladder_hard_failure_never_reassigns_stages`, whose assertion was the exact
  invariant this doctrine reverses.
- **Config plumbing:** `can_cover` is `#[serde(default)]` in
  `CustomAgentConfig` with no `SCHEMA_VERSION` change (still `8`); the desktop
  Studio round-trips it in both `agent_to_custom` and `custom_to_agent`, and
  seeds/defaults set it to `Vec::new()`.
- **Checkpoint plumbing:** `specialist_takeover_attempted` is `#[serde(default)]`
  in both `CheckpointContext` and `GraphCheckpoint` — additive, so an old
  checkpoint restores empty rather than failing to load.
- **Suite:** the four commits report fmt clean, clippy `-D warnings` clean, and
  4725 workspace tests green.

## Known gaps and follow-ups

- **The coordinator's dispatch prompt is not on the `PromptBuilder` path.**
  `render_dispatch_system_prompt` builds a plain `String` and it is sent as a
  single `Message { role: Role::User, content: system_prompt }`
  (`coordinator.rs:10680`). It does **not** use
  `concerto_core::types::SYSTEM_PROMPT_BUILD` / `_CHAT` / `_PLAN`, has **no
  `{working_memory}` placeholder**, and gets **no stable-head / cache-prefix
  treatment** — `with_cache_stable_prefix` and the ADR-048 prefix discipline
  reach the single-agent loop through `runtime_runner.rs` only. The
  consequences of `251d457` (working-memory block actually delivered) and
  `32d6809` (stable-head prefix discipline) therefore **do not reach the
  Coordinator**, which is the component that makes the most dispatch decisions
  and would benefit most from a cacheable prefix. Recorded here as a known gap
  and a follow-up, deliberately **not** fixed in this change: routing the
  Coordinator's prompt through `PromptBuilder` is a behavioural change to the
  hot dispatch path and deserves its own decision and review.
- **No UI for `can_cover`.** The Studio preserves the field but cannot edit it;
  an operator sets it in config. This is the ADR-43 `[skills]`/`[mcp]`
  config-driven-v1 shape, not an omission.
- **No example config.** `docs/config.toml.example` does not yet show a
  `[[multi_agent.custom_agents]]` block at all, so `can_cover` has no
  commented example anywhere outside this ADR.
- **The hold is on the planning rung only.** `recover_planning_rung` /`hold_rung_and_retry`
  cover the coordinator's *planning-provider* recovery
  (`attempt_planning_provider_recovery`). The per-subtask
  `attempt_fallback_ladder` has no hold: a throttled specialist dispatch still
  walks the agent axis and the provider tiers without waiting out a cooldown.
  Extending the hold there is a natural follow-up, bounded by the same
  constants.
- **Mutating classification is name-based.** `is_mutating_self_execution_tool`
  matches tool *names* (`write`, `shell`, `git`) and one `operation` argument.
  A future aliased or renamed mutating tool would not be classified and would
  escape the guard. Deriving the set from the executor's declared capabilities
  would be more robust; it was left out to keep this change to doctrine.

## Residuals

- **The guard does not prevent *bad* delegation.** It guarantees an attempt
  occurred, not that the right agent was called or that the call was good. The
  prompt doctrine and the suitability advisory carry quality; the guard carries
  only the "no silent in-house work" invariant.
- **Exhaustion case (c) is self-certified.** "Delegation attempted and genuinely
  failed" is satisfied by a *recorded* dispatch decision, whatever its outcome.
  A coordinator that dispatches once, ignores the failure, and then works
  in-house is inside the letter of the doctrine. Closing that would require
  judging outcomes, which is a Coordinator decision this ADR deliberately does
  not take over.
- **`can_cover` is a promise, not a capability.** Declaring that an agent can
  cover `implement` does not give it the tool-calling capability an implement
  agent needs; capability resolution (ADR-35 / ADR-49) is unchanged and still
  governs what the agent can actually do.
- **Takeover is one attempt per task.** `specialist_takeover_attempted` bounds
  the agent axis the same way `self_execute_attempted` bounds tier 2 — a
  pathological second failure escalates on the provider axis without a second
  takeover.
- **A hold spends wall-clock on a provider that may never recover.** Bounded to
  two 30-second holds per run, cancellable, and only ever on a throttled
  exhaustion that carried a hint — but the bound is a constant, not an SLO
  derived from the provider's own backoff curve.

---

*Last updated: 2026-09-27*
