# ADR-52: Orchestration safety gates — global run cap, plan artifacts, exit gate

**Status:** Accepted (2026-08-08) — implemented in commit `84b1a1a`
    (Phase 5 M1, "global run cap, durable plan artifacts, multi-failure exit
    gate").
**Date:** 2026-08-08
**Deciders:** Concerto architecture
**Supersedes:** Phase 5 of the provider-first redesign plan
    (`docs/ARCHITECTURE-V2.md`) — the orchestration-polish items this ADR
    finalizes.
**Composes with:** ADR-42/ADR-45 (fallback ladder, `Partial` machinery) — the
    run-cap exit reuses the ladder-exhaustion `Partial` path; ADR-46/ADR-48
    (reasoning-as-data, ContextEngine) are orthogonal and unchanged.

## Context

Phase 5 of the V2 redesign calls for orchestration polish: a subagent-isolation
audit, planner-as-data persistence, step caps, a run-wide doom guard, and an
exit gate proving a multi-agent run still completes when some models fail. A
read-only audit of the Phase-5 items established that the *isolation* half was
already closed before this ADR:

- Specialists run with a **session-scoped `VirtualFs`** — the tool sandbox
  confines all filesystem operations to the session root (ADR-44).
- **Role write gates** (capability-gated executor policy) leave only the
  Coder able to mutate files; every other specialist is write-gated.
- **Fresh `AgentContext` windows per task** — only a summary/deliverable,
  never a full transcript, returns to the coordinator's history (ADR-48
  documents the same boundary for per-task specialist prompts).
- **Per-attempt caps** already bound retry/fallback work
  (`MultiAgentConfig.max_subtask_attempts`, ADR-42/ADR-45 §2) and review/
  validation cycles are capped by `CollaborationRule.max_cycles`.

This ADR therefore covers **only the added gaps**: it adds no new isolation —
the three decisions below close the gaps left in the plan's "loop guards" and
"planner-as-data file persistence" and "exit gate" lines.

Explicitly **not changed** by this ADR:

- the `VirtualFs` isolation model (session-scoped sandbox and its audit path);
- capability-gated executor policy semantics;
- the single-agent `agent_loop` / `runtime_runner` transport (SSE, retry,
  provider wiring) — no provider or transport behavior was altered.

## Decision

Ship three safety and auditability gates in the multi-agent coordinator:

### 1. Global run cap (doom guard) — `max_total_iterations`

`MultiAgentConfig` gains `max_total_iterations: Option<usize>` (config
schema, `#[serde(default)]`):

- **Default `None`** = unlimited (bit-for-bit current behavior). `Some(0)` is
  treated as **off** (the coordinator filters `.filter(|cap| *cap > 0)`).
- The coordinator mirrors it as `CoordinatorAgent.max_total_iterations` and
  tracks a monotonic per-run **`model_dispatch_count`**, reset to `0` at the
  start of each `execute_graph` invocation (so a fresh run and a
  resume-from-checkpoint both restart the counter).
- **What counts**: every real model dispatch inside one multi-agent run —
  every ready **batch** dispatch (`+batch.len()` at the batch boundary) and
  **every fallback-ladder tier re-dispatch** (tier 1 default-model swap, tier
  1b default-provider rebuild, tier 2 coordinator takeover — each `+1`).
- **What does not count** (documented trade-off): the **planner/design stage**
  and the **review and validation loops**. They run outside
  `execute_graph`'s batch/ladder dispatches, so a run's design/review budget is
  not charged against the cap. The cap therefore bounds *execution* spend, not
  total LLM cost — accepted because the review/validation loops are
  cycle-capped independently, and a run budgeted for planning but starved at
  `execute_graph` can still spend its review/validation cycles.
- **Enforcement**: the cap is checked **at batch boundaries, before the next
  ready batch is dispatched** (`iteration_cap_reached()` at dispatch point 2a).
  When `model_dispatch_count >= cap`, the coordinator pauses with a **`Partial`
  outcome through the same machinery as ladder exhaustion** — a
  `MultiAgentModeCompleted` event, a checkpoint, and final message
  `"Automation paused after reaching the run-wide dispatch cap ({cap} total
  model dispatches). Existing workspace changes and session context were
  preserved."` — preserving `all_files`, `provider_metrics`, and sibling
  results (ADR-26 semantics).
- A batch of width N advances the counter by N, so the cap can overshoot by at
  most the width of the batch already in flight (typically 2–3 ready tasks) —
  accepted given narrow graph width.

### 2. Planner-as-data — durable plan artifacts

`TaskPlanner::plan` now returns a `PlanOutcome` instead of a raw
`Vec<PlannedSubTask>`:

```
PlanOutcome { tasks: Vec<PlannedSubTask>, artifact: PlanArtifact }
PlanArtifact {
    plan_id: String,               // run-scoped, ULID; names the file
    task_description: String,      // task text, readable without the DB
    tasks: Vec<PlanArtifactTask>,  // id/role/description/dependencies/expected_artifacts
}
```

- `PlanArtifactTask` keeps plain strings so the on-disk JSON is stable and
  human-readable regardless of internal ID type evolution.
- `PlanArtifact::from_planned(task, planned)` renders a fresh plan with a new
  ULID `plan_id`; `PlanArtifact::from_graph(plan_id, task, graph,
  expected_artifacts)` renders a restore/checkpoint path using the checkpoint's
  `run_id` as the plan id.
- The coordinator persists the **pretty-printed JSON** via
  `concerto_sessions::plans::PlansManager` to
  **`<app_data_dir>/plans/plan-<plan_id>.json`**, idempotently — an existing
  file with the same plan id is overwritten, so the plans dir is a complete
  history of every plan execution. An unwritable/unavailable data root degrades
  to a logged skip and a `None` plan id on the event; the run proceeds.
- `EventKind::MultiAgentModeStarted` gains an **additive `plan_id:
  Option<String>`** field (additive — consumers must not rely on it being
  present). Transcript and desktop translations ignore it (pattern-matched as
  `..`).
- **Restore-from-checkpoint** re-renders and re-persists the artifact from the
  resumed graph (`PlanArtifact::from_graph`) — idempotent, so a resumed run's
  plan continues to appear in the plans dir.
- **`app_data_dir()` in `concerto-sessions` is now the single source of truth
  for every on-disk data root** (sessions DB, memory, plugin capability store,
  audit, plans): `dirs::data_dir().join("concerto")` (`$XDG_DATA_HOME`
  hermetic under tests). The previous ad-hoc `dirs::data_dir().join("concerto")`
  derivations in `runtime_runner` (memory, plugins, audit) and the session
  store converged on this one function.

### 3. Multi-failure exit gate — testing policy

The Phase-5 exit gate ("e2e run with 2 of the models intentionally failing,
output still completes") is institutionalized at the **unit level** in the
coordinator integration tests, **not** as a chaos harness:

- **`multi_failure_exit_gate_run_still_completes`**: a 5-role graph (all
  independent roots in one wide batch) where the **researcher** fails its first
  dispatch with a hard `LimitReached` (auth) and the **coder** fails with a
  budget error — both are **rescued by the tier-1 default-model ladder**
  (`ModelPinConfig.default_model = "mid"`), the run exits **`Completed`** (not
  `Partial`), the rescued summaries surface in `SubTaskCompleted` events, and
  no "Fallback ladder exhausted" agent note is emitted.
- **`max_total_iterations_caps_dispatch_at_batch_boundary`** pins the cap
  semantics: cap=1 pauses `Partial` before the second batch; a cap covering the
  run's dispatches completes; `Some(0)` behaves as unlimited.
- Supporting tests: plan-artifact render / round-trip / persist-to-plans-dir
  (`crates/orchestrator/src/planner.rs`, `crates/sessions/src/plans.rs`) and a
  config round-trip for the new field.

## Consequences

- **Positive.** Unbounded multi-agent runs can no longer burn tokens forever —
  a missed ladder or a replan spiral now stops at a configured dispatch budget
  with a **preserved** checkpoint and workspace, not an open-ended spend.
  Plans are reproducible and auditable independent of the session DB; every
  run carries a link to its durable plan. The exit gate for "2 of N models
  fail" is now pinned by a regression test, so the ladder's resilience is
  verified over time.
- **Negative / trade-offs.** The cap counts execution dispatches only;
  planner/design and review/validation loops are outside the counter
  (documented in §1). Artifacts are audit/linkage data — **not** the restore
  source: checkpoints remain authoritative for resume; the plan file is a
  by-product, not an input. `app_data_dir()` consolidation moves the data root
  decision into concerto-sessions — a behavior change only if callers
  previously used function name mismatches; test hermeticity keeps them
  deterministic under `XDG_DATA_HOME`.
- **Risks**: a cap set too low can pause a legitimate long run at `Partial` —
  the default `None` keeps current behavior, and the pause message names the
  cap explicitly so operators can raise it. An idempotent overwrite of the plan
  file with a reused plan id is by design (the same run re-persisted).
- **Migration**: **none** — all additions (config field, event field,
  `app_data_dir()` consolidation) are additive with `None` defaults; old
  configs, checkpoints, and persisted data load unchanged; the new event field
  is `Option<String>`.

## Review notes

- The cap has a `Some(0) = off` special case because a config tool that
  "un-writes" a value may emit `0` for "no limit"; treating 0 as off makes the
  off switch unambiguous without a separate boolean.
- Counting the ladder tiers is deliberate: a fallback ladder **re-request is a
  model dispatch** — otherwise a doom guard that excludes retry would let a
  run spin past its budget through repeated tiers.
- The plan file is intentionally not the restore source: checkpoints already
  carry the full graph + expected artifacts (and are the resume path), so a
  second authoritative store would invite drift. The plan dir is an
  attachment for linkage/audit (to reproduce what the run was asked to do),
  which the session DB alone does not answer in a stable, human-readable form.