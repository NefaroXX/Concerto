# ADR-64: Timeline-driven zero-waste orchestration

**Status:** Proposed (draft)

Brute-force supersedes nothing; this ADR **extends** ADR-52 (orchestration
safety gates), **generalizes** the ADR-60 D7 approved-plan skip, and **composes
with** ADR-42/45 (fallback ladder, coordinator self-execution) and ADR-58
(only the coordinator is hardcoded; everything else is config data).
Supersedes: none in full.

**Date:** 2026-09-02

**Deciders:** Concerto architecture + maintainer direction

## Context

Concerto's orchestrator re-invokes agents for work that is already done. The
observable failure (audit-log `build accord` run) shows a coder re-dispatched
with a **fresh TaskId** after a zero-file "success", re-reading the same files
it had already inspected, and burning repeated model dispatches on unchanged
work before failing as `Partial`. That specific redo was amplified by an
unrelated provider tool-call defect (null→`{}` argument coercion), which is
**out of scope** here and tracked separately. But the orchestration layer
itself lacks the structure to avoid redoing work even once the provider defect
is fixed:

- **No stable work identity.** Fresh `TaskId`s are minted per dispatch, per
  revision, and per graph regeneration, so the scheduler cannot tell "same
  work, unchanged" from "new work".
- **No reuse decision.** The dispatch loop (`coordinator.rs:2306`) dispatches
  every `ready_tasks()` node; the only reuse is the narrow ADR-60 D7 "approved
  plan seeds the DesignDoc and skips the architect". There is no general
  check for "this deliverable already exists / this knowledge is already
  established / this plan is already valid".
- **No timeline context.** `AgentContext` carries `previous_results`,
  `working_memory`, `retrieved_chunks`, `expected_artifacts`, but no compact
  statement of *what is already known, already complete, unchanged, and
  remaining*. Agents must re-discover state: a planner re-plans, a coder re-
  reads files to confirm existence, a researcher re-researches established
  facts.
- **No audit of reuse.** Every model dispatch is a cost; there is currently no
  record of *why* a dispatch happened or *why* it was (or wasn't) avoided.

Governing principle (ADR-58): **only the coordinator is hardcoded; everything
else is config data.** The five specialists are seed templates, deletable and
removable. Any reuse mechanism must therefore be a **coordinator-internal,
role-agnostic** policy — it must survive having the architect, researcher,
coder, or reviewer removed, and it must let the coordinator *reassign* or
*take over* rather than fail.

## Decision

### 1. A durable, evidence-based timeline is the source of truth

Introduce a **typed timeline projection** over Concerto's existing durable
sources — the whiteboard log, the write gate / WAL events, checkpoints, the
audit log, and completed results. The projection is a **pure derived view**,
not a new storage table: it is recomputable, idempotently, from the durable
logs (ADR-60 D4 WAL-first invariant), so it can never drift from the source of
truth and has no migration surface.

A `TimelineEntry` records, for every meaningful unit of work:

| field | meaning |
|-------|---------|
| `semantic_key` | stable identity: `hash(objective_version, plan_version, work_intent, output_contract, dependency_keys)` |
| `kind` | Plan / Research / FileObservation / TaskResult / Finding / Verification |
| `content_hash` | blake3 of the entry's payload |
| `producer` | agent id + model (informational only — never part of `semantic_key`) |
| `inputs` | dependency keys + file observations the work depended on |
| `evidence` | what makes it still valid (files, hashes, gate sequences) |
| `gate_seq` | whiteboard ordering for staleness comparisons |
| `invalidated_by` | later evidence that reopens/supersedes this entry |

The timeline is **bounded when injected**: agents receive a compact, typed,
task-specific slice — never the raw transcript.

**Semantic-key derivation order** (the projection is recomputed from durable
logs, so this is a pure fold, not stored state): the projection function reads
the checkpoint's task graph, the plan artifact, and the expected-artifacts map;
walks subtasks in dependency order; and computes each entry's `semantic_key`
from the subtask description (`work_intent`), the plan's content hash
(`plan_version`), the checkpoint's objective hash (`objective_version`), the
task's expected artifacts (`output_contract`), and the already-computed
semantic_keys of its predecessors (`dependency_keys`). `gate_seq` and
`content_hash` come directly from the whiteboard events. Because it is a pure
fold, the projection is rebuilt idempotently and differs only when the source
logs differ.

### 2. Stable semantic work identity (role-agnostic)

Reuse is impossible while a fresh `TaskId` is the only identifier. Give every
work item a stable **semantic key** derived from the objective, plan version,
work intent, expected output contract, and dependency identities. **Agent
identity is explicitly excluded** from the key, so:

- research produced by the researcher remains reusable if that agent is removed;
- a completed coder deliverable remains valid across agent roster changes;
- revision cycles, graph regeneration, and checkpoint/resume all compare the
  *same* work by the *same* key instead of minting new identities.

### 3. A deterministic decision before every model dispatch

Before any model call, resolve the work item through a pure function:

```
should_dispatch(work_item, timeline, completed_results, world_state)
        -> Reuse | Refine | Reopen | Dispatch | Reassign | CoordinatorTakeover
```

- **Reuse**: identical semantic key, inputs unchanged, evidence valid → inject
  the cached result; **zero model dispatch**.
- **Refine**: valid partial result exists, but explicit remaining gaps → a
  small, targeted dispatch framed as refinement (never full redo of
  established portions).
- **Reopen**: a dependency or a depended-on file changed → dispatch with the
  change as the explicit reason.
- **Dispatch**: required work has no prior valid result.
- **Reassign**: the configured agent/model is unavailable or unsuitable →
  ADR-42/45 fallback ladder.
- **CoordinatorTakeover**: no configured agent can perform the required work →
  coordinator self-execution (ADR-45 Tier 2).

**Purity boundary:** `Reuse` / `Refine` / `Reopen` / `Dispatch` are **pure,
deterministic** and must **not consume an LLM call**; these four are directly
property-testable. `Reassign` / `CoordinatorTakeover` are **state-dependent** —
they read current agent availability, provider health, and the live registry,
which change during a run (crash, rate-limit, config reload) — and fall
through to coordinator *judgement*. That is exactly where the judgement belongs
and never a hardcoded role check. `world_state` in the signature is a narrowed
struct over the live registry + provider health, not the full `CoordinatorAgent`.

**Invalidation evidence** (any of these reopens/refines a previously valid
entry):
- a dependency's `content_hash` changed;
- a depended-on file was written with a later `gate_seq`;
- the entry's evidence is superseded by a later consolidation/finding;
- an explicit user or resolver enrichment request.

### 4. Plan semantics — reuse a valid plan

A plan is a versioned artifact:

```
plan_id, objective_hash, plan_content_hash, source_revision,
approval_state, assumptions, dependencies
```

- A valid, current plan for the same objective → **no architect and no planner
  dispatch**. The planner's subtask decomposition, dependency graph, and
  expected artifacts are taken from the cached plan artifact, not re-derived.
- Relevant source change → impact analysis; patch only affected plan nodes
  (refine), not a full replan.
- Objective or assumptions fundamentally changed → replan.
- Plan ownership is **artifact-based, not agent-based**: removing the architect
  after planning never invalidates an approved plan, and planning a fresh task
  with no architect routes to a planning-capable agent, the default planner
  provider, or the coordinator — never a hardcoded "must call architect".

**This generalizes ADR-60 D7 in two dimensions, and the second is a new
behavioral change that needs its own invalidation evidence.** D7 skipped only
the *architect* when a seeded DesignDoc was present; the planner was still
invoked on every run. This ADR **additionally skips the planner** when a valid
plan exists. That planner-skip requires stronger invalidation evidence than the
architect-skip, because the cached decomposition must stay correct against the
world: a source change that affects a plan node's inputs must trigger
`Refine` (re-patch the node), never `Reuse`. The connection to §3's invalidation
evidence ("a depended-on file was written with a later gate_seq") is
load-bearing here.

**Pure/LLM boundary on impact analysis:** classifying *that* a source change
requires plan revision is deterministic (evidence: a depended-on file was
written later). But determining *which* plan nodes are affected — the scope of
the refinement — requires semantic understanding of the source↔plan relationship
and is a **coordinator-model decision**. The resolver classifies the need
(`Reopen`/`Refine`) from evidence; the coordinator decides scope.

### 5. Research semantics — research is gap-driven

Research output becomes structured knowledge:

```
topic_fingerprint, findings, provenance, dependencies,
freshness_policy, unresolved_questions, contradiction_links
```

The resolver runs research **only** when:
- no relevant knowledge exists (gap), or
- a dependency changed and the affected findings must be refreshed (targeted),
  or
- a contradiction is detected and must be resolved, or
- an explicit enrichment request is made.

Research never runs merely because the pipeline configures a research stage.

### 6. File-context semantics — existence from the timeline, content preloaded

A coder (or any working agent) receives a **workspace capsule**:
known paths + existence + content hashes + observation sequence; preloaded
relevant current file excerpts; changes made earlier in the run; expected
outputs; and the specific work not yet done.

- **Existence and unchanged-metadata** come from the timeline / index, so an
  agent does not `list`/`read` a file merely to confirm it exists.
- **Relevant current contents** are preloaded into the capsule.
- An agent **rereads only** when the observation is stale, incomplete, or the
  agent explicitly needs additional content.
- An unchanged read is served from a read-through cache without redoing
  filesystem work.

Editing is still safe: writing a file always requires current content, which
the capsule provides; the rule forbids *redundant reconnaissance*, not
*necessary reads*.

Capsule budget is explicit and matches the existing context-engine constants
(`memory_prompt.rs`):

| Constant | Value | Rationale |
|----------|-------|-----------|
| `MAX_TIMELINE_ENTRIES` | 30 | Matches `MAX_TASKS` (24) + planning overhead |
| `MAX_ACTIVE_FILES` | 12 | Matches `MAX_PREVIOUS_RESULTS` (8) × 1.5 |
| `MAX_ENTRY_CHARS` | 400 | Half of `MAX_DETAIL_CHARS` (800) for balance |
| `MAX_OBSERVATION_SEQ` | 200 | Cap on served observation sequence length |

### 7. Agent-removability invariant

Scheduling resolves each work requirement **by capability and stage kind,
never by role name**:

```
work requirement → capable agents → preferred model/provider
        → fallback agent/model (ADR-42/45) → coordinator takeover
```

The timeline, fingerprints, and resolver are **coordinator-side only**. Agents
never see or depend on the timeline directly — they receive a pre-built
`TimelineContext`. Removing an agent removes its future entries; it never
invalidates artifacts it already produced, and never changes how other agents are
scheduled. This preserves ADR-58's "only the coordinator is hardcoded".

### 8. Audit of reuse

Every resolver verdict (`Reuse`, `Refine`, `Reopen`, `Dispatch`, `Reassign`,
`CoordinatorTakeover`), and the evidence behind it, is recorded to the durable
log/audit so the cost of each dispatch is attributable and reducible.

## Consequences

**Positive**
- Re-running a valid plan → zero planner/architect calls.
- Re-running an unchanged completed graph → zero model calls.
- Existing valid research → zero research calls; one stale finding → exactly
  one targeted research call.
- Unchanged file observations → no redundant reconnaissance reads.
- Removing the architect/researcher/coder → reassignment or coordinator
  takeover, never failure.
- Every reuse decision is audited and therefore measurable.

**Negative / costs**
- New coordinator-side state (timeline projection + fingerprints). Bounded;
  derived from durable logs, no new table.
- New complexity in the dispatch loop; mitigated by extracting the resolver as
  a pure, testable module.
- Hash-based reuse requires careful invalidation to avoid false reuse; a
  file-state secondary check guards against mismatch.

**Risks & mitigations**
- *False reuse*: blake3 collision negligible; add a secondary check comparing
  actual file state against the entry's evidence.
- *Crash between event and projection*: projection is recomputable from the
  WAL-first durable log; rebuild is idempotent and is **triggered lazily at
  dispatch-batch boundaries**, not eagerly on every whiteboard append.
- *Context bloat*: `TimelineContext` is bounded by fan-in, `MAX_ACTIVE_FILES`,
  and capped entry counts.
- *Hardcoded-role regression*: the resolver and capsule are keyed on semantic
  identity + capabilities/stage kinds, never role names; enforced by review.

## Relationship to prior ADRs
- **Amends/extends ADR-52** (safety gates): the run cap and plan artifacts
  remain; this adds the per-dispatch reuse decision.
- **Composes with/amends ADR-60 D7** (approved-plan skips architect): the
  architect-skip now also extends to a planner-skip (see §4); plan-reuse becomes
  the default for any valid plan.
- **Subsumes ADR-60 D6 (consolidation + WorkingMemory injection)**:
  ADR-64 §5's structured, timeline-native knowledge model replaces D6's
  SqliteVectorStore consolidation; D6's bi-temporal metadata and
  invalidate-not-delete semantics carry forward. D6's planned *WorkingMemory
  injection into supervised runs* is subsumed by this ADR's Phase 2 (timeline
  projection enriches `WorkingMemorySnapshot`) + Phase 5 (capsules replace raw
  `working_memory` in agent prompts). Tracked jointly; D6 no longer advances on
  its own timeline.
- **Composes with ADR-42/45** (fallback ladder, coordinator self-execution):
  `Reassign` and `CoordinatorTakeover` are the ladder, surfaced as resolver
  verdicts.
- **Complies with ADR-58** (only the coordinator is hardcoded): this is a
  coordinator-internal policy, role-agnostic by construction.
- **Composes with ADR-48/63** (context engine / memory): the `TimelineContext`
  is assembled deterministically like the context engine, and long-term
  `retrieved_chunks` remain the memory source.

## Implementation phases
1. **This ADR** (accepted before code).
2. **Timeline projection** — typed projection over durable events; enrich
   `WorkingMemorySnapshot` rather than replace it.
3. **Semantic keys + fingerprints** — fingerprint plans, research, observations,
   inputs/outputs/dependencies/verification.
4. **Pre-dispatch resolver** — pure `should_dispatch` before the
   `graph.ready_tasks()` batch.
5. **Task context capsules** — role-agnostic, task-specific context packets.
6. **Unify special cases** — route the ADR-60 D7 skip, zero-file revision,
   research-stage heuristic, and retry/revision paths through the resolver.
7. **Zero-waste proof** — end-to-end tests for every reuse/invalidation/removal
   case.

Phases 4 (resolver: *whether* to dispatch) and 5 (capsule: *what* to send) are
logically independent and may proceed in parallel.

## Status drivers
- Maintainer direction: zero-waste orchestration; agents get timeline context;
  no re-plan if a plan exists; no re-read merely to confirm existence; no
  research without a gap/staleness/refinement reason; agents stay configurable
  and removable; coordinator reassigns or takes over.
- The provider `null`-argument tool-call defect that amplified the observed
  redo is a **separate issue**, out of scope, solved independently.
