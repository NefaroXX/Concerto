# ADR-42: Coordinator resilience: failure-class fallback ladder

> **Archived** — this is the original text, retained verbatim. The active
> [ADR-42](../ADR-42.md) is the consolidated record incorporating the ADR-45
> model-first amendments and the ADR-35 stage-absence extension (consolidated
> 2026-08-22). See [docs/adrs/README.md](../README.md) for the current index.

**Status:** Archived original — superseded in file by the consolidated active ADR-42 (as amended by ADR-45 and extended by ADR-35)

## Context

Multi-agent subtask failure handling in `coordinator.rs` is binary:

- **Recoverable** errors (`classify_subtask_error`) retry
  the same agent and model up to `MAX_SUBTASK_ATTEMPTS = 3`. Once exhausted, the
  one-shot `escalation_attempted` set grants exactly one
  additional dispatch on the same agent/model (`retry_feedback` is appended and
  the attempt counter resets), after which the
  subtask goes straight to a blocked/`Partial` outcome.
- **Everything else** — cancellation, invalid task graph, and provider/model-
  specific hard failures such as authentication failure, context overflow,
  rate-limit ceiling, or no-affordable-model — exits immediately and gracefully
  with a `Partial`/checkpoint result.

The gap this ADR closes: a provider/model-specific hard failure is often a
property of the *assignment*, not of the *task*. An auth failure, a context
overflow, a rate-limit ceiling, or a budget block on the primary model can all
leave the underlying subtask perfectly solvable by a different model — or by
the coordinator itself. Those cases would otherwise terminate the subtask
immediately, and exhausted-recoverable errors terminate it even when a global default model
could complete the work. Both behaviours under-use the
coordinator's authority to re-route work.

## Decision

Replace the binary classification with a three-way `SubtaskFailureClass` and a
two-tier fallback ladder. `NonRecoverable` keeps immediate graceful
exit; `Recoverable` keeps the same-agent retry loop; `LimitReached` — the
new middle class — walks the ladder before any `Partial` exit.

### 1. `SubtaskFailureClass` enum (coordinator.rs)

```rust
enum SubtaskFailureClass {
    /// Transient; retry the same agent/model.
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

### 2. Global default model config (additive)

`ModelPinConfig` gains two additive fields:

```rust
pub default_model: Option<String>,
pub default_provider_config_id: Option<String>,
```

Both default to `None`. When set,
`default_model` is the "global default model" fallback: the model every role
falls back to when its own assignment cannot be used.

*(As originally written, tier 1 changed only the model on the role's bound
provider; ADR-45 revised this to be model-first with a provider-switch tier.)*

### 3. `RoutingEngine::fallback_to_default(role)`

A new method on `RoutingEngine` (`providers/src/routing.rs`) resolves the global default model for a role:

- Respects capability requirements (tool-calling) and the optional
  provider-config pairing from §2.
- Returns `OrchestratorError::NoAffordableModel` when the fallback is unset.
- Returns `OrchestratorError::PinnedModelNotFound` when the configured default
  cannot be resolved against available profiles.

It deliberately does **not** re-check the spend budget. The ladder runs exactly
when budget constraints already blocked the primary path.

### 4. Two-tier fallback ladder (`LimitReached`)

- **Tier 1 — same agent, global default model.** *(Amended by ADR-45: fires
  only when the role's serving pipe offers the default model; a tier 1b
  provider-switch was added.)*
- **Tier 2 — coordinator self-execution.** As originally written, a direct
  single-shot prompt through `planning_provider` with no tool loop, tagged
  `provider: "coordinator-self-execute"`, gated to subtasks with no
  expected-file artifact contract. *(Amended by ADR-45: tier 2 is now a full
  agent dispatch on the planning provider with a tool loop; the artifact gate
  was dropped. Extended by ADR-35: stage-absence self-execution when no stage
  agent is registered at all.)*
- **Both tiers fail → graceful `Partial`/checkpoint exit**, preserving
  `all_files`, `provider_metrics`, and sibling results.

**Self-execution is gated structurally, not configured.** A hard failure never
reassigns the subtask to another agent:
the ladder stays within the original role (model swap) or pulls the task into
the coordinator itself.

### 5. Interplay with existing paths

- The implement-stage artifact-failure replan-to-Architect path
  stays distinct and takes priority over the generic ladder for that specific
  failure class.
- Cancellation and structural errors remain `NonRecoverable` and short-circuit
  before any ladder tier.
- Ladder retries remain attempt-bounded and cancellation-aware, consistent
  with ADR-26's recovery boundaries.

### 6. Scope decisions (settled)

- **Single `default_model` string, not a list.** Per-role default lists are
  follow-up work.
- **String tagging convention over a typed `ProducedBy` field.** The
  `AgentRunResult.provider` string is the tag carrier; a structured field on the
  result type was deliberately deferred to keep the change additive.

## Consequences

### Positive

- Provider/model-specific hard failures no longer terminate a subtask that a different
  model could complete — they now have up to two recovery routes.
- Exhausted-recoverable errors gain the same ladder instead of going straight
  to blocked.
- `Partial`/checkpoint exit becomes an honest last resort that still preserves
  prior progress.
- Additive config surface: `None` defaults mean existing configs and
  checkpoints behave identically.
- Recovery never mutates the DAG.

### Negative

- `provider: "coordinator-self-execute"` is a soft string convention; a typed
  `ProducedBy` field was deferred.
- Tier 1 as originally specified is a same-provider model fallback only;
  switching the serving provider required the ADR-45 follow-up.
- The ladder guards are serialized in `GraphCheckpoint` as additive
  `#[serde(default)]` fields and restored on resume,
  so a resumed run does not re-walk the ladder.

---

*Last updated: 2026-08-22 (retrospective consolidation — see [README](../README.md)).*
