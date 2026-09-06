# ADR-35: Tag-driven agent orchestration with Coordinator-first architecture

**Status:** Accepted (Revised 2026-08-13)
**Date:** 2026-08-01 (original), 2026-08-13 (revision)

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
executor-backed dispatch (ADR-45 rev. 2026-08-07) — with stage-absence
self-execution by a true coordinator persona. Historical text from the original
decision is preserved below where it still applies; contradictions are resolved
in favor of the revision.

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

Every non-Coordinator agent declares a `stage` tag in its config. Five known
values have Coordinator algorithms; unknown strings get **Freeform** semantics
(run once, full context, no lifecycle):

| Stage     | Coordinator behavior |
|-----------|---------------------|
| `design`  | Runs first; output parsed as DesignDoc for artifact ownership arbitration |
| `research`| Runs before implement; output feeds context for implement-stage agents |
| `implement`| Receives artifact ownership from design stage; modified files tracked for conflict detection |
| `review`  | Receives implement output; loop with implement stage up to max_cycles |
| `validate`| Runs test suite; loop with implement stage up to max_cycles |

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

### 4. Make pipeline topology data-driven from stage tags

The Coordinator's stage-sequencing logic matches stage tags, not role names:

- **`decompose_task`**: finds the registered design-stage agent (if any), runs
  it, parses output for artifact ownership, then passes control to the planner,
  which knows the full set of registered agents and their stages.
- **`execute_graph`**: implement-stage completion triggers review cycles;
  design-stage completion triggers replan.
- **`run_review_cycle` / `run_validation_loop`**: resolved from the registry by
  stage tag with configured `CollaborationRule` limits.
- **Planner**: its prompt is templated from the registered agents participating
  in planning (design/research/implement stages) **plus the coordinator as a
  planner-eligible self-role** (see §8). Custom freeform agents can be targeted
  by name.

> **Amendment (2026-08-13):** Phase 3's original clause — *"a missing
> implement agent fails fast"* — is revoked. A pipeline with no registered
> implement-stage agent plans against the coordinator self-role instead of
> failing (planner's "no implementation-stage agent is registered" check
> becomes "no implement-stage agent is registered **and self-execution is
> unavailable**").

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

The stage tag field accepts any string; the five known values get Coordinator
algorithms; unknown strings get Freeform behavior. A user can add a
`"security_audit"` stage agent purely through config, renames "Coder" to
"Implementer" without touching Rust, deletes the review-stage agent (review
skips), or removes every default agent (coordinator self-executes — §8).

### 7. Parallel dispatch model

Multiple agents in the same stage default to **parallel** for research and
implement, **sequential** for review and validate, encoded as
`parallel_dispatch: bool` on `CollaborationRule`.

### 8. Coordinator self-execution (Revision 2026-08-13)

The Coordinator can perform every lifecycle stage itself. Registration of a
stage agent is an optimization/delegation choice, never a requirement.

**Triggers (both):**
1. **Stage absence** — no registered agent for a lifecycle stage (e.g. all
   default agents removed): the Coordinator performs the stage itself —
   design via its own prompt and planning model; implement via an
   executor-backed tool loop (same shared `ToolExecutor`, policy engine, and
   cancellation the specialists use); research as needed; verification by
   running the task's declared verification commands when it holds the
   relevant capabilities.
2. **Ladder takeover** — a registered stage agent exhausts the recovery
ladder (ADR-42 classification → ADR-45 provider/model tiers): the existing
tier-2 mechanism re-dispatches the failing role rebuilt on the coordinator's
planning provider with a full tool loop, tagged `provider:
"coordinator-self-execute"` (ADR-42 §4, extended by ADR-45 §3).

**Extends ADR-42 §4 / ADR-45 §3:** those ADRs cover failure-takeover only —
the failing *role* re-executed on the coordinator's planning provider. They do
not cover a fully empty registry. This revision adds stage-absence
self-execution by a true coordinator persona (own prompt, own executor) when
no stage agent is registered at all. The `provider: "coordinator-self-execute"`
sentinel is retained for audit/policy/UI consumers.

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

### Verification (amends Phase 5 C-06)

Original C-06: a build task whose pipeline has no validation-stage agent is
accepted-rejected ("verification did not run"). **Amended:** a build task with
no validation-stage agent is self-verified by the Coordinator through its
executor (declared verification commands, `require_verification` semantics)
when capable. Acceptance is rejected only when verification is required and
cannot be performed at all. Vacuous-accept policy unchanged.

## Amendment (2026-09-05) — the Coordinator decides; planner and stage sequencing are advisory

Revised **in place**, not superseded (per the project owner's standing
instruction: no new ADR numbers). This amendment reconciles the document with
the requirement already quoted in the Revision record above — the Coordinator
"gets context about other agents ... and figures out how to delegate; the
current five agents are only a known-working default preset". The **code**
over-built beyond this contract: a pre-run planner whose output was
materialized verbatim as graph roles, blueprint staffing equality enforced
against the registry, and a compiled evidence scheduler. All three are revoked
here; contradictions elsewhere in this document resolve in favor of this
amendment.

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

## Consequences

### Positive
- The Coordinator can carry a project alone; the five specialists are a
  known-working default preset, not a structural requirement.
- Users can add, remove, rename, and reconfigure agents entirely through config
  and the Orchestration Studio; removing all default agents degrades to
  coordinator self-execution, never to a failed run.
- Delegation follows configuration: the coordinator knows each agent's
  capabilities and job from the studio.
- Safety invariants are preserved: coordinator self-execution goes through the
  same policy-gated executor, VirtualFs, audit, and cancellation paths.
- Checkpoint backward compatibility and the `AgentId` deserializer are
  preserved.
- Default unmodified configs behave identically to the pre-revision pipeline.

### Negative
- Coordinator self-execution shares the coordinator's model/context budget;
  long solo runs are expensive and context-heavy (compaction applies).
- Two code surfaces to keep aligned: the coordinator's built-in delegation
  prompt and the planner prompt roster.
- C-06 acceptance semantics become capability-dependent (self-verification
  requires the coordinator to hold the relevant capabilities or the task's
  verification must be performable without them).
- Self-review is consciously not implemented; coordinator-solo runs do not
  second-guess their own output.

## Migration (phases 1–5, complete)

1. **Phase 1** (complete, `a356e8d`): Replace `AgentRole` with `AgentId(String)`.
2. **Phase 2** (complete, `e54ae49`, `194112e`): Add `AgentStage`, introduce
   `GenericSpecialistAgent`, register seed agents as config defaults.
3. **Phase 3** (complete, `4c2b6e8`): Rewrite coordinator topology from
   role-identity to stage-tag matching; lifecycle roles resolve via
   `AgentRegistry::ids_for_stage`; missing design/review/validate agents skip
   their phase. *(The Phase 3 "missing implement agent fails fast" clause is
   revoked by the 2026-08-13 revision — see §4 amendment. A missing implement
   agent now means the coordinator self-executes.)*
4. **Phase 4** (complete): Runtime topology control (`disabled`),
   capability gating (`eval` toggles the validator's eval engine), model-first
   routing with `tool_calling_roles_for`, stage picker in the Orchestration
   Studio. *(Roster enrichment — capabilities + role descriptions — is
   deferred to the implementation of the 2026-08-13 revision.)*
5. **Phase 5** (complete): Typed submission modes (`output_mode`: freeform /
   design_doc / research_report / review_report), built-in specialists become
   config seeds, validator-owned acceptance (C-06). *(The
   "no validation-stage agent rejects acceptance" clause is amended by the
   2026-08-13 revision — see Verification above.)*

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