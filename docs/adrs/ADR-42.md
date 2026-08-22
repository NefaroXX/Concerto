# ADR-42: Coordinator resilience: failure-class fallback ladder

**Status:** Accepted (2026-08-04) — amended by [ADR-45](./ADR-45.md)
(§§2–4, including the rev. 2026-08-07 change making tier 2 a full agent
dispatch on the planning provider), extended by [ADR-35](./ADR-35.md)
(rev. 2026-08-13: stage-absence coordinator self-execution).
**Date:** 2026-08-04
**Deciders:** Concerto architecture
**Extends:** ADR-35 §5 (Coordinator-only contract), ADR-26 (fault containment and recovery)

## Context

Multi-agent subtask failure handling in `coordinator.rs` is binary:

- **Recoverable** errors (`classify_subtask_error`, coordinator.rs:62) retry
  the same agent and model up to `MAX_SUBTASK_ATTEMPTS = 3`. Once exhausted, the
  one-shot `escalation_attempted` set (coordinator.rs:425) grants exactly one
  additional dispatch on the same agent/model (`retry_feedback` is appended and
  the attempt counter resets, keeping reporting canonical 3/3), after which the
  subtask goes straight to a blocked/`Partial` outcome.
- **Everything else** — cancellation, invalid task graph, and provider/model-
  specific hard failures such as authentication failure, context overflow,
  rate-limit ceiling, or no-affordable-model — exits immediately and gracefully
  with a `Partial`/checkpoint result (coordinator.rs:1126).

The gap this ADR closes: a provider/model-specific hard failure is often a
property of the *assignment*, not of the *task*. An auth failure, a context
overflow, a rate-limit ceiling, or a budget block on the primary model can all
leave the underlying subtask perfectly solvable by a different model — or by
the coordinator itself. Today those cases terminate the subtask immediately, and
exhausted-recoverable errors terminate it even when a global default model
could complete the work. Both behaviours under-use the
coordinator's authority to re-route work, and both surface as silent task
abandonment rather than an honest attempt at recovery.

## Decision

Replace the binary classification with a three-way `SubtaskFailureClass` and a
two-tier fallback ladder. `NonRecoverable` keeps today's immediate graceful
exit; `Recoverable` keeps today's same-agent retry loop; `LimitReached` — the
new middle class — walks the ladder before any `Partial` exit.

### 1. `SubtaskFailureClass` enum (coordinator.rs)

```rust
enum SubtaskFailureClass {
    /// Transient; retry the same agent/model (today's recoverable path).
    Recoverable,
    /// Retries exhausted, or provider/model-specific hard failure (auth,
    /// context overflow, rate-limit ceiling, no-affordable-model). The task
    /// may still be solvable — walk the fallback ladder.
    LimitReached,
    /// Cancellation, invalid task graph, structural errors. Exit immediately;
    /// no ladder.
    NonRecoverable,
}
```

`LimitReached` absorbs two disjoint inputs: (a) retries exhausted after a
`Recoverable` classification, and (b) the provider/model-specific hard-failure
family previously classed as terminal. It does **not** absorb cancellation or
structural errors — those stay `NonRecoverable` and short-circuit before any
ladder tier.

### 2. Global default model config (additive)

`ModelPinConfig` (config/schema.rs:686) gains two additive fields:

```rust
pub default_model: Option<String>,
pub default_provider_config_id: Option<String>,
```

Both default to `None`, which is bit-for-bit the current behaviour. When set,
`default_model` is the "global default model" fallback: the model every role
falls back to when its own assignment cannot be used.

`default_provider_config_id` optionally selects *which profile entry* supplies
the default model (disambiguation when several providers offer the same model
name). It does **not** switch the provider serving the request: agents keep the
provider they were bound to at runtime construction, so tier 1 changes the
*model* on the role's own provider only. `default_model` must therefore be a
model offered by the role's bound provider; the coordinator warns when the
resolved profile's provider config differs from the role's configured provider.

### 3. `RoutingEngine::fallback_to_default(role)`

A new method on `RoutingEngine` (providers/src/routing.rs, adjacent to the
existing `retry_or_downgrade` at line 219) resolves the global default model for
a role:

- Respects capability requirements (tool-calling) and the optional
  provider-config pairing from §2.
- Returns `OrchestratorError::NoAffordableModel` when the fallback is unset.
- Returns `OrchestratorError::PinnedModelNotFound` when the configured default
  cannot be resolved against available profiles.

It deliberately does **not** re-check the spend budget. The ladder runs exactly
when budget constraints already blocked the primary path, so re-evaluating the
budget against the fallback would defeat its purpose.

### 4. Two-tier fallback ladder (`LimitReached`)

Invoked by the coordinator when `class == LimitReached`. Replaces both the old
straight-to-blocked path for exhausted-recoverable errors and the old immediate
non-recoverable exit for provider/model-specific errors.

- **Tier 1 — same agent, global default model.** The subtask is re-dispatched
  on the same agent role with the same construction-time bound provider, but
  the model is swapped to the configured global default
  (`MultiAgentConfig.default_model`, resolved via
  `RoutingEngine::fallback_to_default` / `ModelSelector`, §3). The agent is not
  replaced, no new stage is set up, and the task is not reassigned to a
  different agent. If the default pin's provider differs from the role's bound
  provider, the coordinator warns and proceeds with the best-effort model name
  (the provider stays the role's bound one). The tier fires at most once per
  task per run (`default_model_attempted` guard).
- **Tier 2 — coordinator self-execution.** If the default-model swap fails (or
  is not applicable), the coordinator takes over the subtask itself via the
  `self_execute` path: a direct single-shot prompt through `planning_provider`
  with no tool loop, tagged with the string convention
  `provider: "coordinator-self-execute"` on the existing
  `AgentRunResult.provider` field (core/types.rs:1273) — a string convention,
  not a typed `ProducedBy` field (see §6). Gated to subtasks with **no
  file-artifact contract** (`expected_artifacts.is_empty()`): self-execution
  never fabricates files, so any expected artifact disqualifies the tier. One
  shot per task per run (`self_execute_attempted` guard). The run emits
  `SubTaskStarted`/`SubTaskCompleted` lifecycle events and records **real
  spend**: `tokens_in`/`tokens_out` estimated via the char-count heuristic
  (chars/4, matching the single-agent loop's accounting), cost via
  `planning_provider.approximate_cost`, recorded into the shared spend tracker
  and the persisted audit row. A provider response that is empty/whitespace is
  treated as a tier failure (falls through to `Exhausted`), never as a completed
  deliverable. Roles with no registered agent short-circuit the whole ladder
  (config error, not a recovery case).
  **Amended by ADR-45 (rev. 2026-08-07):** tier 2 is now a full agent
  dispatch — the role rebuilt on the coordinator's planning provider with a
  full tool loop, gated on the role having a rebuild factory. The
  `expected_artifacts` gate no longer applies. See [ADR-45](./ADR-45.md) §3.
- **Both tiers fail → existing graceful `Partial`/checkpoint exit, unchanged.**
  This preserves `all_files`, `provider_metrics`, and sibling results exactly as
  today (ADR-26); the checkpoint exit becomes the true last resort instead of
  the immediate response to a provider/model failure, and the session never
  hard-stops on an agent hard failure.

**Self-execution is gated structurally, not configured.** Tier 2 runs only when
the subtask's `expected_artifacts` set is empty; there is no new configuration
surface for this choice — the artifact contracts from ADR-35 are the single
source of truth. A hard failure never reassigns the subtask to another agent:
the ladder stays within the original role (model swap) or pulls the task into
the coordinator itself.

### 5. Interplay with existing paths

- The implement-stage artifact-failure replan-to-Architect path
  (`replan_attempts`, coordinator.rs:367, fired around coordinator.rs:1430)
  stays distinct and takes priority over the generic ladder for that specific
  failure class: an implement subtask that exhausted retries because expected
  artifacts were not produced still goes to the design-stage agent first.
- Cancellation and structural errors remain `NonRecoverable` and short-circuit
  before any ladder tier.
- Tier 1–2 retries remain attempt-bounded and cancellation-aware, consistent
  with ADR-26's recovery boundaries.

### 6. Scope decisions (settled)

- **Single `default_model` string, not a list.** Per-role default lists are
  follow-up work; a single global default covers the motivating case.
- **Self-execute gated by empty `expected_artifacts`.** Coordinator
  self-execution is a single-shot prompt without a tool loop; any expected-file
  contract cannot be satisfied that way, so subtasks with expected artifacts
  fall through to `Partial` after Tier 1. Empty provider output also falls
  through (no false "completed" deliverables). *Amended by ADR-45 (rev.
  2026-08-07): tier 2 dispatches the rebuilt role with a full tool loop and
  the artifact gate is dropped; see [ADR-45](./ADR-45.md).*
- **String tagging convention over a typed `ProducedBy` field.** The
  `AgentRunResult.provider` string is the tag carrier; a structured field on the
  result type was deliberately deferred to keep the change additive.

## Consequences

### Positive

- Provider/model-specific hard failures (auth, context overflow, rate-limit
  ceiling, no-affordable-model) no longer terminate a subtask that a different
  model could complete — they now have up to two recovery routes.
- Exhausted-recoverable errors gain the same ladder instead of going straight
  to blocked.
- `Partial`/checkpoint exit becomes an honest last resort that still preserves
  `all_files`, `provider_metrics`, and sibling results (ADR-26 semantics
  unchanged).
- Additive config surface: `None` defaults mean existing configs and
  checkpoints behave identically.
- Recovery never mutates the DAG: both tiers keep the subtask on its original
  stage and role (model swap) or pull it into the coordinator, so stage
  topology, `all_files`, and sibling results stay stable.

### Negative

- `provider: "coordinator-self-execute"` is a soft string convention; a typed
  `ProducedBy` field was deferred (scope).
- Subtasks with any expected file artifact cannot use Tier 2 and fall through
  to `Partial` after Tier 1.
- Tier 1 is a same-provider model fallback only; switching the serving provider
  for a role is not expressible (agents bind providers at construction) and
  requires follow-up plumbing if cross-provider fallback is ever needed.
- `default_model` is a single global string; per-role defaults require follow-up
  work.
- The ladder guards (`default_model_attempted`, `self_execute_attempted`,
  `escalation_attempted`) are serialized in `GraphCheckpoint` as additive
  `#[serde(default)]` fields (`schema_version` stays 3) and restored on resume,
  so a resumed run does not re-walk the ladder.
- A hard failure cannot be offloaded to a peer agent: if both tiers fail, the
  coordinator exits the session gracefully with a `Partial`/checkpoint outcome
  rather than re-dispatching to another role.

## Related ADRs

- ADR-35 §5: Coordinator-only hardcoded contract — the coordinator is the sole
  owner of cross-agent state and DAG scheduling, which is where the ladder
  lives; the stage/artifact contracts ADR-35 introduced define which subtasks
  are eligible for coordinator self-execution (empty `expected_artifacts`).
- ADR-26: Fault containment and recovery — the retry boundaries and the
  `Partial`/checkpoint last-resort semantics this ladder extends without
  replacing.
