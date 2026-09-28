# ADR-35: Tag-driven agent orchestration with Coordinator-first architecture

**Status:** Accepted (revised in place; latest revision 2026-09-27)
**Date:** 2026-08-01 (original), revised in place on 2026-08-13, 2026-09-05,
2026-09-16, and 2026-09-27

## Revision record (2026-08-13)

This document was **replaced in place**, not superseded, per the project owner's
instruction. The original framing treated the Coordinator as a scheduler that
could not work without specialists and encoded that as mandatory stage
participants ("a missing implement agent fails fast", C-06 rejection when no
validation agent exists). That was planned against the stated requirement:

> The Coordinator is the main component of the program. It gets context about
> other agents — what they are and are not allowed to do, what their jobs are —
> from the configurations in the Orchestration Studio, and it figures out how to
> delegate. The current five agents are only a known-working **default preset**;
> it must be technically possible to remove all of them and have the
> Coordinator carry the project by itself.

The revision changes: §5 (Coordinator contract), adds §8 (coordinator
self-execution), amends Phase 3 ("missing implement agent fails fast") and the
Phase 5 C-06 clause (rejection when no validation agent). Cross-references:
extends ADR-42 §4 / ADR-45 §3 — which already made ladder takeover an
executor-backed dispatch (ADR-45 rev. 2026-08-07) — with self-execution by a
true coordinator persona when the roster cannot cover the work. Historical
text from the original decision is preserved below where it still applies;
contradictions are resolved in favor of the revision.

## Context

The multi-agent orchestrator originally had three layers of hardcoded structure
that prevented users from customizing, adding, or removing agents without
modifying Rust source code. Those layers were replaced by the phased work
recorded under *Migration* below. Any mention of the retired dedicated structs
(`ArchitectAgent`, `CoderAgent`, `ResearcherAgent`, `ReviewerAgent`,
`ValidatorAgent`) refers to that pre-implementation state only.

### Layer 1: AgentRole is a closed enum
`AgentRole` (core/src/types.rs) was a `#[non_exhaustive]` enum with fixed
variants. Adding or removing an agent required recompiling every crate that
matched on it. — **Replaced** (Phase 1).

### Layer 2: Five specialist agents are distinct Rust types
Each specialist was a separate struct with hardcoded prompts, tool schemas, and
output-format expectations. — **Replaced** (Phases 2–5).

### Layer 3: Pipeline topology is hardcoded control flow
`coordinator.rs` had literal role comparisons driving an explicit state machine:
Architect → planner → Coder → review → validation. — **Replaced** (Phase 3).

## Decision

Concerto adopts a tag-driven agent architecture. The Coordinator is the only
hardcoded component; every specialist is config seed data.

### 1. Replace AgentRole (closed enum) with AgentId (open newtype)

`AgentRole` is replaced by `AgentId(String)`, a transparent newtype wrapping a
lowercase ASCII string. Six reserved constants mirror the current roles:
`COORDINATOR`, `ARCHITECT`, `RESEARCHER`, `CODER`, `REVIEWER`, `VALIDATOR`. Any
other string is a valid custom agent ID.

**Serialization**: `AgentId` serializes as its inner string (lowercase). On
deserialization, it accepts both lowercase (canonical form) and PascalCase (old
checkpoint format) for the known IDs; unknown strings are accepted as-is so old
checkpoint files remain loadable.

**Persistence**: The `role` column in `subtasks` and `agent_run_results` tables
already stores TEXT (PascalCase historically, lowercase going forward). No
schema migration needed.

### 2. Introduce AgentStage tags with open vocabulary

Every non-Coordinator agent declares a `stage` tag in its config. Five values
are well-known and typed specially; unknown strings get **Freeform** semantics
(run once, full context, no lifecycle):

| Stage     | Typed as |
|-----------|---------------------|
| `design`  | Output is parsed as a DesignDoc for artifact-ownership arbitration |
| `research`| Output is a typed research report and feeds context for implement work |
| `implement`| Freeform output; modified files are tracked for conflict detection (ADR-60 D5) |
| `review`  | Output is a typed review report; whether a reviewer is called at all is a Coordinator decision (§10) |
| `validate`| Output is an eval-runner Pass/Fail; whether a validator — or Coordinator self-verification — runs at all is a Coordinator decision (§10) |

The tags are **informational vocabulary, not a dispatch policy**: they type an
agent's output and route verifiers, and they impose no ordering. No agent is
called because its stage tag exists — the Coordinator decides whether, when,
and whom to call, and a stage-less or freeform agent is a full participant
(§9). `CollaborationRule.max_cycles` is likewise advisory context injected into
the Coordinator's prompt (a "typical cycle ceiling" it may weigh), never a
hardcoded loop bound that can terminate a run (§10).

The five default specialists are seeded with matching stage tags
(architect → `design`, researcher → `research`, coder → `implement`,
reviewer → `review`, validator → `validate`).

### 3. Collapse five specialists into one GenericSpecialistAgent

The five distinct structs are replaced by a single `GenericSpecialistAgent`
driven by a `SpecialistDefinition` (id, name, stage, prompt_sections,
capabilities, model_override, provider_id, output_mode). `ExpertAgent` exposes
`id()` and optional `stage()` (default `None` = Freeform).

The five current specialists become **seed data**: default `CustomAgentConfig`
entries shipped into a fresh config, editable and deletable like any other
custom agent. They are not Rust types.

### 4. Resolve and type work by stage tag, never by role name

Stage tags, not role names, are how a *purpose* resolves to an agent. They
never dispatch (§2, §9).

- **Resolution.** A purpose — design output, an implement task, a review
  verdict, a validation run — resolves against the registry by stage tag
  (`AgentRegistry::ids_for_stage`). A missing agent is never a hard failure:
  Phase 3's original *"a missing implement agent fails fast"* clause is not in
  force, and a pipeline with no registered implement-stage agent plans against
  the coordinator self-role instead (see §8) — the planner's "no
  implementation-stage agent is registered" check reads "no implement-stage
  agent is registered **and self-execution is unavailable**".
- **Graph execution.** `execute_graph` runs the Coordinator's decision loop
  over the ready batch. It does **not** trigger a review cycle on
  implement-stage completion, and it does **not** gate graph completion on a
  validation pass; a reviewer or validator runs only when the Coordinator calls
  it (§10). A design-stage failure leaves a **replan fallback** as evidence the
  Coordinator acts on, never an automatic re-dispatch.
- **Planning.** `decompose_task` consults the design-stage agent when the
  Coordinator chooses to, parses its output for artifact ownership, and hands
  the Coordinator a work breakdown. It does **not** pass control to the
  planner, and `TaskPlanner` output is never materialized as `SubTask`
  roles/dependencies: the plan is an advisory record the Coordinator may use or
  ignore (§9.2). The planner's prompt is templated from the registered agents
  participating in planning (design/research/implement stages) **plus the
  coordinator as a planner-eligible self-role** (see §8). Custom freeform
  agents can be targeted by name.

### 5. Coordinator: the base executor, the only hardcoded component

The Coordinator is the main component of the program. It owns delegation: it
reads what other agents are, what they are allowed to do, and what their jobs
are from configuration (id, stage, capabilities, role description) and decides
how to delegate. It must be able to carry the project alone; every registered
specialist only *replaces* the Coordinator for the stage it covers.

**Hardcoded (never configurable):**
- Constructed in code only; a config entry with id `"coordinator"` is rejected
  with a warning (`MultiAgentConfig::pipeline_warnings()`).
- Sole owner of: session context, DAG scheduling, delegation strategy, cycle
  enforcement, artifact-ownership arbitration, acceptance decisions.
- Delegation instructions and the coordinator system prompt are built-in and
  refined in code.
- Executor-backed self-execution (see §8), policy-gated like any agent.

**Configurable (exactly two surfaces):**
1. **Model selection** — provider/model pinning for the coordinator's planning
   and self-execution dispatches.
2. **A supplemental prompt section** — appended to the coordinator's actual
   system prompt; it can never interfere with, replace, or supersede the
   built-in instructions.

### 6. Stage vocabulary: open but with five well-known values

The stage tag field accepts any string; the five well-known values are typed
specially; unknown strings get Freeform behavior. A user can add a
`"security_audit"` stage agent purely through config, rename "Coder" to
"Implementer" without touching Rust, delete the review-stage agent (the
Coordinator simply has one fewer specialist to call, §10), or remove every
default agent (the roster is empty, the Coordinator self-executes — §8).

### 7. Collaboration topology: seeded stage-kind pairs, resolved against the roster

Collaboration shape is config data seeded over **stage kinds**, never agent
role ids: `default_stage_relationships()` ships Review→Execution `Supervises`
(cap 6), Acceptance→Execution `Supervises` (cap 5), Research→Execution
`ProvidesContextTo`, and Planning→{Execution, Research} `OwnsDesign`. The pairs
are resolved to concrete agent ids at runtime against the agents actually
staffing each kind; a roster that omits a kind yields no edge for pairs that
reference it, never an error. The resolved `CollaborationRule` carries
`from` / `to` / `relationship` / `max_cycles` — there is no `parallel_dispatch`
flag and no per-stage parallel/sequential switch.

These rules are **advisory context the Coordinator weighs** (§9.1), not a
compiled dispatch order: `max_cycles` in particular is a "typical cycle
ceiling" injected into the Coordinator's prompt and never a hardcoded loop
bound that can terminate a run (§10.1).

### 8. Coordinator self-execution

The Coordinator can perform every lifecycle stage itself. Registration of a
specialist is a capacity choice, never a requirement: a run whose roster is
empty is still a run, and it is the Coordinator's own capacity that carries it.

**Delegation is the default action.** Implementation, review, and validation
work is dispatched to a registered specialist. The Coordinator takes work in
house only when the **roster cannot cover the work** — a condition on the
roster as a whole, enumerated below. The dispatch prompt says so in the
imperative and names the reason: *the registered specialists are the run's
capacity*. There is no blanket "you are a full agent" license; keeping work
in-house is not a cheaper option the Coordinator weighs against a call, and a
dispatch is never declined because it looks expensive.

**Triggers (both):**

1. **Roster exhaustion.** The Coordinator performs the stage itself when, and
   only when, the roster is exhausted in exactly one of three ways:
   1. the roster is **empty** — no specialist is registered at all;
   2. the roster is **disabled or unavailable** — every registered agent is
      disabled, so nothing registered can be called;
   3. **delegation was attempted and genuinely failed** — the Coordinator
      dispatched, the dispatch did not produce the work, and retrying the call
      with corrective notes or re-targeting *the same work* to another
      registered specialist did not either.

   The condition is exhaustion of the **roster**, not staffing of a **stage**.
   A non-empty roster that happens not to be staffed for the stage at hand is
   **not** an exhaustion case: the Coordinator delegates to whoever is
   registered.

   *Why the roster, and not the stage.* A stage tag declares an agent's
   affinity, not its coverage (§2's open vocabulary; a freeform or stage-less
   agent is a full participant), so "is this stage staffed" is a question the
   tag test cannot answer correctly — and a question the Coordinator can always
   answer in its own favour at negligible cost. Under a stage-absence trigger
   the Coordinator was one cheap inference away from authoring work in house
   while registered specialists sat idle, and self-authored work is unchecked
   work: it carries none of the independent check a delegated dispatch gets, so
   a defect in it surfaces as a fabricated result rather than a failed run. That
   is the shape of the recorded defect — a rename objective whose source did
   not exist was satisfied by a subtask telling the coder to create the file
   and then rename it; the coder complied, the C-06 gate saw the declared
   deliverable, and a fabricated workspace was reported `Complete`. The gate was
   right; the fabrication happened upstream of it, where nothing was ever
   delegated (`bc40806`, `caadb2d`). Absence of a named artifact is therefore
   information to report through the existing `request_user_input` path, not an
   obstacle to route around.

   *Why not gate on stage staffing.* A stage-composition gate would constrain
   operator-chosen rosters, which ADR-58 forbids: a roster of one stage-less
   agent would be permanently undelegable for every other stage, and an
   operator who deliberately staffs only `design` would find the tool refusing
   to finish their own run. Staffing is the operator's decision; the guard asks
   only whether a roster exists.

   When the trigger holds, the Coordinator runs the stage itself: design via
   its own prompt and planning model; implement via an executor-backed tool
   loop (same shared `ToolExecutor`, policy engine, and cancellation the
   specialists use); research as needed; verification by running the task's
   declared verification commands when it holds the relevant capabilities.
2. **Ladder takeover** — a registered stage agent exhausts the recovery
   ladder (ADR-42 classification → ADR-45 provider/model tiers): the existing
   tier-2 mechanism re-dispatches the failing role rebuilt on the coordinator's
   planning provider with a full tool loop, tagged `provider:
   "coordinator-self-execute"` (ADR-42 §4, extended by ADR-45 §3). The **agent
   axis is consulted before the provider axis**: a hard-failed subtask's
   preferred takeover target is another registered specialist that can cover
   the work (`can_cover` is config data), so swapping to an equally capable
   *agent* is preferred to swapping models.

**The exhaustion condition is enforced, not merely prompted.** The prompt
alone is not a control, so the Coordinator's own `ToolExecutor` tool-call path
carries a machine-enforced guard over a precise, narrow mutating set (`write`,
`shell`, `git`, plus the `filesystem` tool's destructive `write` / `delete` /
`move` / `copy`; read-only calls are never classified, so the Coordinator keeps
its evidence-gathering power in full):

| Situation | Outcome |
|---|---|
| Mutating call, roster non-empty, run has recorded **no** delegation attempt | **Refused**, fail-closed, with a named `PolicyVerdict` (`"Denied: delegation-required"`) on the bus and a structured `{"error": "delegation_required", …}` tool result the loop can recover from by dispatching |
| Mutating call, roster empty (or all-disabled — disabled agents never register) | Permitted, and recorded with reason `roster-empty-or-disabled` |
| Mutating call, a delegation attempt is already recorded | Permitted, and recorded with reason `delegation-attempted` |
| Any read-only call | Unrestricted, always |

The record is `EventKind::CoordinatorSelfImplementing { run_id, session_id,
tool_name, reason }`, so lawful self-work is never again indistinguishable from
a delegated dispatch in the event stream — "who did this?" is answerable from
the audit trail alone. A *refused* self-execution deliberately publishes no
such event: the refusal is the `PolicyVerdict`, and a refusal is not self-work.
"Has this run delegated?" is read from the checkpointed decision journal
(`DecisionKind::DispatchSpecialist`), never from a live counter, so the
guarantee survives a resume; a malformed or validator-rejected `call_specialist`
is never recorded as a dispatch decision and therefore does not unlock
self-execution. The guard's shape is pinned by
`delegation_is_never_gated_on_stage_staffing`, which keeps the rejection above
from drifting back.

**Extends ADR-42 §4 / ADR-45 §3:** those ADRs cover failure-takeover only —
the failing *role* re-executed on the coordinator's planning provider. They do
not cover a registry that cannot be consulted at all. This section adds
self-execution by a true coordinator persona (own prompt, own executor) when
the roster is exhausted, and puts the agent axis ahead of provider escalation.
The `provider: "coordinator-self-execute"` sentinel is retained for
audit/policy/UI consumers, and the reserved `coordinator` id is never
registered from config, so it never appears in the roster the guard inspects.

**Guardrails:**
- One takeover attempt per subtask (`self_execute_attempted`); a configurable
  takeover/quota cap bounds coordinator load.
- Self-execution runs through the same executor as specialists: policy engine
  approvals, VirtualFs, audit log, capability filtering, and cancellation
  tokens apply unchanged.
- Context/cost: self-execution is metered through the existing
  `provider_metrics` / `spend_records` instrumentation with the sentinel
  provider tag.
- Self-review is **explicitly deferred** (a later refinement): the Coordinator
  does not review its own output against itself in this revision.

**Delegation knowledge (amends Phase 4 roster):** the roster the planner and
the coordinator's delegation prompt receive is enriched from config — each
agent's id, stage, capabilities, and role description — beyond the original
id+stage pair, so delegation decisions follow the Studio configuration.

**Verification (C-06):** a build task whose pipeline has no validation-stage
agent is self-verified by the Coordinator through its executor (declared
verification commands, `require_verification` semantics) when capable.
Acceptance is rejected only when verification is required and cannot be
performed at all. Vacuous-accept policy unchanged. See also §10.4, which keeps
the same invariant as a post-action safety net.

## 9. The Coordinator decides — dispatch authority, advisory planning, no compiled policy

*Revised in place 2026-09-05; clause numbers 1–5 are the cited units.* The
Coordinator contract quoted in the Revision record above is the governing one:
the Coordinator "gets context about other agents ... and figures out how to
delegate; the current five agents are only a known-working default preset". The
implementation had over-built beyond it — a pre-run planner whose output was
materialized verbatim as graph roles, blueprint staffing equality enforced
against the registry, and a compiled evidence scheduler — and those three are
revoked here. Where any other part of this document still reads as though one
of them were in force, this section governs.

### 1. Dispatch authority belongs to the Coordinator, not to code

- The Coordinator calls registered agents through a policy-gated
  `call_specialist(agent_id, task, notes)` tool. It decides *which* agent and
  *when*, from the agents' **context injected into its prompt** (id, name,
  role, declared capabilities, output mode, system instructions) plus the
  run's recorded evidence.
- No agent is called because its stage tag exists. The stage table in §2 is
  **informational vocabulary** (output-mode typing, verifier routing) — it is
  not a dispatch policy and imposes no ordering.

### 2. The planner is demoted to an advisory tool

- §4's "passes control to the planner" is revoked. `TaskPlanner` output is
  **never materialized as `SubTask` roles/dependencies** by
  `decompose_task`/`decompose_from_evidence`. It may exist only as an
  optional, coordinator-invoked advisor ("draft a work breakdown") whose plan
  is context the Coordinator may use or ignore; `PLAN.md`/plan artifacts are
  advisory records, never an authoritative workload.

### 3. Registry is the roster; staffing is never enforced

- The registry built from `custom_agents` config (ADR-58) is the roster.
  Blueprint `def.agents` staffing equality checks and drift asserts are
  **deleted**; a blueprint is advisory data at most.

### 4. No compiled dispatch policy

- There is no scheduler/decision-function that selects agents. Evidence
  (ADR-65 facts, claims, decisions) is injected into the Coordinator's
  context as guidance; the Coordinator selects, and every selection is
  recorded as an evidence-backed `Decision` event (ADR-65 §6/§7 ledger, kept).

### 5. Safety nets unchanged (post-action compensation)

- Write gates, `SimplePolicyEngine`, `VirtualFs`, the zero-work guard, and the
  checkpoint/resume ledger remain. Correctness is enforced **after** action —
  verify, attribute, gate, revise — not by pre-empting the Coordinator.

## 10. Review and validation are Coordinator decisions; the run continues until a Coordinator decision or an error

*Revised in place 2026-09-16; clause numbers 1–5 are the cited units.* §9
revokes compiled dispatch for implement-stage work; this section completes that
revocation: **review and validation are also Coordinator decisions.** The
Coordinator decides *whether* to call a reviewer or validator, *when*, and
*what to do with the verdict* — including continuing to run until it concludes
human intervention is required, or a genuine error stops it.

### 1. Review and validation are Coordinator decisions, not pipeline gates

- The auto-review trigger on any implement-stage success
  (`execute_graph` → `run_review_cycle`) is **removed**. No agent is invoked
  "because its stage tag exists" — a review-stage agent is called only when the
  Coordinator calls it, through the same policy-gated `call_specialist` tool
  as any specialist, journaled as an evidence-backed `Decision` event (ADR-65).
- The auto-validation gate at the end of graph execution
  (`execute_graph` → `run_validation_loop`) is **removed**. A validate-stage
  agent (or Coordinator self-verification) runs only when the Coordinator
  invokes it.
- `CollaborationRule.max_cycles` (seeded defaults: Review→Execution 6,
  Acceptance→Execution 5) is **advisory context injected into the Coordinator's
  prompt** (a "typical cycle ceiling" it may weigh), never a hardcoded loop
  bound that terminates the run.

### 2. The run continues until a Coordinator decision or an error

A run ends only through one of:

1. **Completed** — the Coordinator decides the objective is met (no further
   tool calls) **and** the acceptance check passes (see §10.4).
2. **AwaitingUser** — the Coordinator calls the new `request_user_input` tool
   with a reason; the run stops with a preserved checkpoint, surfaced to the
   UI so the operator can answer and resume. This is the Coordinator-side half
   of the consent gate; the interactive answer channel is tracked by
   TODO #22/#23.
3. **Partial/Failed on error** — a genuine hard error (provider failure,
   cancellation, policy denial, run-wide dispatch cap ADR-52), with the
   checkpoint ledger preserved for resume.

There is **no** hardcoded cycle-count or stage-triggered terminal stop.
`ReviewCycleEscalated` / `ValidationEscalated` events may still be published as
informational evidence, but no code path converts them into a terminal
`MultiAgentModeCompleted`.

### 3. Verdicts are evidence, and failure returns to the Coordinator

- A reviewer's verdict and a validator's Pass/Fail + eval output return as
  tool results (`call_specialist`) and become ADR-65 evidence, so the next
  decision-loop iteration sees them in the world model.
- On validation (or review) failure the Coordinator decides the response:
  re-dispatch the implementer with feedback, call a different specialist,
  re-plan, or `request_user_input`. It may retry and iterate; nothing outside
  the Coordinator forces a stop.
- The fix/revision feedback loop that `run_review_cycle` /
  `run_validation_loop` hardcoded (queue revision subtask up to max_cycles,
  then escalate) is replaced by Coordinator-chosen re-dispatch.

### 4. Acceptance stays a safety net, not a gate

- The C-06 verification invariant survives: a build task may not be reported
  `Completed` unless verification evidence exists *for this run* (an accepted
  validator Pass, or accepted Coordinator self-verification). This is enforced
  **after** the Coordinator declares completion (post-action compensation,
  per §9.5), not by pre-empting it.
- `record_acceptance` / `acceptance_rejection` remain and are invoked as part
  of the completion decision, keyed on declared verification evidence.
- `verify_expected_artifacts` (missing/placeholder artifact rejection) is
  unchanged.

### 5. Eval engine and shell profile

- The eval-runner validation engine is unchanged in operation; the
  registered-validator engine (`registry.rs`) is brought to parity with the
  Coordinator self-verify engine by attaching the configured shell profile, so
  a registered validator uses the same shell environment (PATH, aliases) as
  Coordinator self-verification. This closes the observed Windows/msys2 smoke
  gap (bare `pytest` resolution).

## Consequences

### Positive
- The Coordinator is the sole decider of *which* agent runs, *when*, and
  *why* — §9's dispatch authority now covers review and validation too, with no
  residual hardcoded stage topology.
- The Coordinator can carry a project alone; the five specialists are a
  known-working default preset, not a structural requirement.
- Users can add, remove, rename, and reconfigure agents entirely through config
  and the Orchestration Studio; removing all default agents degrades to
  coordinator self-execution, never to a failed run.
- Delegation follows configuration: the coordinator knows each agent's
  capabilities and job from the studio.
- Failure terminates runs only by Coordinator conclusion or genuine error;
  escalation becomes evidence, so a fixed-point loop can't silently end a run
  at a hardcoded cycle ceiling.
- The consent gate gains its Coordinator-side surface (`request_user_input` →
  `AwaitingUser` + checkpoint), ready to be wired to the interactive channel.
- Safety invariants are preserved: coordinator self-execution goes through the
  same policy-gated executor, VirtualFs, audit, and cancellation paths.
- C-06 verification-required semantics and artifact checks are preserved as
  post-action safety nets.
- Checkpoint backward compatibility and the `AgentId` deserializer are
  preserved.
- Default unmodified configs behave identically to the pre-revision pipeline.

### Negative
- The Coordinator may need more decisions (and hence more model dispatches) to
  reach the same outcome; previously-hardcoded review/validation loops no
  longer count against the Coordinator's budget by construction.
- Coordinator self-execution shares the coordinator's model/context budget;
  long solo runs are expensive and context-heavy (compaction applies).
- Tests that asserted the auto-gate behavior must be rewritten to assert
  Coordinator-driven invocation and verdict handling.
- Coordinator-solo runs that skip review/validation still rely on the
  Coordinator's own judgment plus the completion-time acceptance check; a
  Coordinator that never validates will be caught at completion (Partial with
  preserved checkpoint), not mid-run.
- Two code surfaces to keep aligned: the coordinator's built-in delegation
  prompt and the planner prompt roster.
- C-06 acceptance semantics become capability-dependent (self-verification
  requires the coordinator to hold the relevant capabilities or the task's
  verification must be performable without them).
- Self-review is consciously not implemented; coordinator-solo runs do not
  second-guess their own output.

### Migration

The §10 revision was implemented and verified on a feature branch merged to
`dev` via PR. Gate: fmt, clippy `-D warnings`, nextest, cargo-deny all
green; affected auto-gate tests rewritten; new tests cover Coordinator-driven
review/validation, verdict-as-evidence, `request_user_input` →
`AwaitingUser`, and completion-without-verification → Partial.

## Migration (phases 1–5, complete)

1. **Phase 1** (complete, `a356e8d`): Replace `AgentRole` with `AgentId(String)`.
2. **Phase 2** (complete, `e54ae49`, `194112e`): Add `AgentStage`, introduce
   `GenericSpecialistAgent`, register seed agents as config defaults.
3. **Phase 3** (complete, `4c2b6e8`): Rewrite coordinator topology from
   role-identity to stage-tag matching; lifecycle roles resolve via
   `AgentRegistry::ids_for_stage`; missing design/review/validate agents skip
   their phase. *(The Phase 3 "missing implement agent fails fast" clause is
   not in force — see §4. A missing implement agent does not fail the plan: the
   coordinator self-role is used, and when the roster itself is exhausted the
   Coordinator does the work — see §8.)*
4. **Phase 4** (complete): Runtime topology control (`disabled`),
   capability gating (`eval` toggles the validator's eval engine), model-first
   routing with `tool_calling_roles_for`, stage picker in the Orchestration
   Studio. *(Roster enrichment — capabilities + role descriptions — is
   delivered in §7.)*
5. **Phase 5** (complete): Typed submission modes (`output_mode`: freeform /
   design_doc / research_report / review_report), built-in specialists become
   config seeds, validator-owned acceptance (C-06). *(The
   "no validation-stage agent rejects acceptance" clause is not in force — see
   **Verification (C-06)** in §8.)*

Each phase is independently verifiable: the test suite must pass at every step
with identical behavior for default configurations.

## Phase 5 record — typed submission modes and seed migration (complete)

Commits: `57b955f` (typed DesignDoc contract, audit H-01), `98b5545` +
`8e10d08` (built-ins become config seeds, audit A-01), `ad3a72b`
(validator-owned acceptance, audit C-06). Gate: fmt, clippy `-D warnings`,
nextest, cargo-deny all green.

### Typed submission modes (`OutputMode`)

`OutputMode` (core/src/types.rs) has four variants serialized snake_case, with
`Freeform` as the serde default: `Freeform`, `DesignDoc`
(`submit_design_doc`, schema from `SubmitDesignDocInput`, legacy aliases `files`
→ `proposed_files`, `interface` → `interface_sketch`), `ResearchReport`
(`submit_research_report`), `ReviewReport` (`submit_review_report`). The runtime
forces the submission tool (`ToolChoice::Forced`), validates field-by-field,
returns structured errors, runs a bounded 3-attempt repair loop, and falls back
to tolerant text parsing for providers that ignore forced tool choice.

### Built-in specialists are now config seeds

The five dedicated structs are deleted. `builtin_agent_seeds()` in
`concerto-config` returns the five `CustomAgentConfig` seed entries with
matching `output_mode` (architect: design/DesignDoc, researcher:
research/ResearchReport, coder: implement/Freeform, reviewer:
review/ReviewReport, validator: validate/Freeform-eval-runner). The registry
merges user `custom_agents` over the seeds by id; `disabled = true` removes an
agent from the runtime topology; the reserved `coordinator` id is never
registered from config.

The generic agent gained the retired structs' behaviors: ReviewReport mode with
`report_outcome` verdict mapping and `<changed_file_context>` injection;
eval-runner mode (no LLM call, `apply_constraints`, Pass/Fail `format_summary`,
fail-fast when the engine is unavailable); freeform tool loop with
`files_modified` tracking. The coordinator's `self_execute_tier` (ADR-42/45
takeover, `provider: "coordinator-self-execute"` sentinel, single attempt per
subtask, checkpointed `self_execute_attempted`) remains the basis for §8.