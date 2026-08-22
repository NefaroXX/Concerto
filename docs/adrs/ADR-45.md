# ADR-45: Ladder provider switch, retry configurability, and coordinator takeover

**Status:** Accepted (2026-08-07). Extended by [ADR-35](./ADR-35.md) (rev.
2026-08-13): stage-absence coordinator self-execution — a true coordinator
persona with its own executor when no stage agent is registered at all (this
ADR's tier 2 covers re-dispatch of a *failing role* on the planning provider
only).
**Date:** 2026-08-07
**Revision:** 2026-08-07 — same-day, pre-release amendment making the ladder
**model-first**: the *model* is the axis of recovery, and the *provider* is
the pipe that serves the chosen model. Tier 1 fires only when the role's
serving pipe offers the default model (otherwise a clean skip with a ladder
note); tier 1b rebuilds the role on the default provider; tier 2 is now a real
agent dispatch on the planning provider with a full tool loop.
**Deciders:** Concerto architecture
**Amends:** ADR-42 §2 (tier-1 provider binding), ADR-42 §3 (retry cap),
ADR-42 §4 (tier-2 artifact gate)

## Context

ADR-42's two-tier ladder rescues subtasks after `LimitReached` failures, but
three structural gaps remain, observed in production as a hard stop with
"Automation paused after exhausting recovery attempts for a non-fatal
subtask" while both the global default provider and the coordinator were
healthy:

1. **Tier 1 cannot escape a failing provider (ADR-42 §2).** The specialist
   agent's provider is bound at construction time (`GenericSpecialistAgent
   .provider`, registry.rs `get_provider` closure; `ExpertAgent::run(&self,
   …, model: &str)` in core/src/traits/agent.rs). Tier 1 swaps only the model
   *name*; the request still goes to the failing provider, so a latency,
   quota, or outage of that provider repeats the failure or ends with
   `PinnedModelNotFound` when the default model is not offered by the role's
   bound provider. The revised ladder resolves this by (a) skipping tier 1
   cleanly when the role's serving pipe does not offer the default model and
   (b) adding tier 1b, which moves the role to the run's default provider.
2. **The retry cap is a hard constant.** `MAX_SUBTASK_ATTEMPTS = 3`
   (coordinator.rs:44) bounds same-agent retries regardless of the failure's
   character. Transient latency spikes — the actual observed failure — exhaust
   three attempts in seconds, then walk the ladder even though the provider is
   merely slow. Users cannot raise the ceiling.
3. **Tier 2 is artifact-gated (ADR-42 §4).** `self_execute_tier` only fires
   for subtasks with no expected artifact files, because a coordinator
   takeover was a text-only prompt with no tool loop and could never produce
   files. Real coding subtasks always carry artifact contracts, so the
   coordinator — the last functioning execution path — can never take over
   exactly the work that needs taking over. The revised tier 2 is a real
   agent dispatch with a tool loop, so the artifact gate can be dropped.

The instruction-level contract this ADR settles: **a run must complete an
instruction as long as at least one execution path functions** — the role's
own provider, the global default provider, or the coordinator itself. Only
catastrophic conditions (complete network disconnect, hard crash,
cancellation) may end a run mid-task.

## Decision

Amend the ADR-42 ladder in `CoordinatorAgent::attempt_fallback_ladder`
(coordinator.rs) to be **model-first**: the *model* is the axis of recovery
and the *provider* is the pipe that serves the chosen model. Concretely:
tier 1 fires only when the role's serving pipe offers the default model
(clean skip with a ladder note otherwise); a provider-switch tier (1b)
rebuilds the role on the run's default provider; and tier 2 becomes a real
agent dispatch on the planning provider with a full tool loop. Make the retry
ceiling and the new tier user-configurable. Guard sets remain checkpointed and
at-most-once per task per run, keeping the ladder loop-free by construction.

### 1. New tier 1b: default-provider re-dispatch (amends ADR-42 §2)

Tier 1 fires **only when the role's serving pipe actually offers the default
model**; otherwise it is skipped with a ladder note — the coordinator no
longer warns-and-proceeds across pipes or fails with `PinnedModelNotFound`.
Between tier 1 (default model on the bound provider) and tier 2 (coordinator
self-execution), the ladder gains **tier 1b**: the *same role* rebuilt on the
run's **default provider** and served with the default provider's default
model profile.

- `AgentRegistry` learns rebuild factories
  (`AgentRegistry::register_with_factory` / `get_with_provider`, registry.rs).
  `register_seeded_agents` registers one factory per role; the factory
  reconstructs the same `GenericSpecialistAgent` bound to the given provider.
- `AgentRunner` gains `run_with_provider` (agent_runner.rs), which resolves
  the role through the factory and otherwise shares the full execution path
  of `run` (lifecycle events, cost tracking, concurrency buckets keyed by the
  profile's provider config id).
- The coordinator holds `default_model_provider` + `default_model_profile` —
  the run's `default_provider` and its default model profile, resolved in
  `run_multi_agent` (runtime_runner.rs) from the same routing data used for
  role profiles (the earlier `fallback_provider`/`fallback_profile` names were
  retired as part of the model-first rework).
- Tier 1b is skipped when the **(model, pipe) pair** it would attempt — the
  default model served by the default provider's pipe — was already attempted
  by tier 1 (the role's bound provider *is* the default provider, so a rebuild
  would be a no-op repeat of tier 1). The degenerate case is noted and
  skipped, and the ladder continues at tier 2. Tier 1b is also skipped
  entirely when `default_model_fallback` is disabled (see §4).
- Guard: `default_model_provider_attempted` (checkpointed alongside the
  ADR-42 guards; the checkpoint field keeps a serde rename for old-key
  compatibility — internal, not user-facing), at most once per task per run.
- Roles registered without a factory (e.g. test mocks) cannot be rebuilt;
  tier 1b is skipped for them and the ladder continues at tier 2.

### 2. Retry configurability (amends ADR-42 §3)

- `MultiAgentConfig.max_subtask_attempts: Option<u32>` (config schema,
  default `None` → runtime default 3). The coordinator's
  `max_subtask_attempts` field replaces the `MAX_SUBTASK_ATTEMPTS` constant
  at every dispatch/outcome/blocked arm and in the resume terminal gate.
  Values below 1 clamp to 1.

### 3. Tier 2: coordinator takeover as a real agent dispatch (amends ADR-42 §4)

- `self_execute_tier` drops the `expected_artifacts.is_empty()` condition.
  When both the role's provider and the default provider have failed, the
  coordinator dispatches the role **rebuilt on the coordinator's planning
  provider** (`planning_profile`) via the registry rebuild factory, with a
  **full tool loop** — no longer a text-only prompt. The dispatch is gated on
  the role having a rebuild factory; without one, tier 2 is skipped and the
  ladder falls through to the graceful `Partial`/checkpoint exit. The result
  is still tagged with the string convention `provider:
  "coordinator-self-execute"`. An empty/whitespace result counts as a tier
  failure (never a completed deliverable), and the artifact acceptance gate
  still applies to build tasks at completion. Dependent tasks proceed on the
  coordinator's summary and the run completes rather than pausing
  mid-instruction.
- The once-per-task `self_execute_attempted` guard is unchanged.

### 4. User gate (ADR-45 knobs)

- `MultiAgentConfig.default_model_fallback: bool` (default **true**). Gates
  tier 1b: when disabled, tier 1b never fires and the ladder proceeds from
  tier 1 directly to tier 2.
- Both knobs are config-only (schema + runtime wiring); no checkpoint schema
  change beyond the new guard field (old checkpoints deserialize with empty
  guards and re-walk the ladder exactly once, bounded and safe).

## Consequences

- **Positive**: a slow/flaky role provider no longer hard-stops the run —
  tier 1b moves the work to the default provider, and tier 2 completes it as
  the coordinator when every provider fails. The retry ceiling is a user
  dial, not a code constant. Runs complete with partial (not failed) outcomes
  in every case where at least one execution path functions.
- **Negative**: tier 1b can cost more (a second provider's tokens) and takes
  longer before tier 2 fires; tier 2 re-runs the role on the planning
  provider, which can itself be slow or quota-bound. Both are deliberate
  trade-offs for completing the instruction.
- **Risks**: providers created per-fallback-add load on default-provider
  quota; guarded once per task, so no unbounded cost. The rebuild factory
  reuses the seed configuration (prompts, capabilities, output mode), so a
  rebuilt agent behaves identically to the original except for its provider.
- **Migration**: none required — `default_model_fallback` and
  `max_subtask_attempts` are optional with defaults; old checkpoints load
  unchanged.

## Review notes

- Tier 1b's degenerate check compares the **(model, pipe)** pair tier 1
  already attempted against what tier 1b would attempt: when the role's bound
  provider *is* the default provider, tier 1 has already tried the default
  model on that pipe and tier 1b would be a no-op repeat, so it is skipped.
  The pipe comparison is by provider config id, not provider type, so two
  configs of the same vendor are treated as distinct providers (a broken
  OpenAI config must be able to fall back to a healthy OpenAI config).
- The ladder order is preserved: tier 1 (cheapest: same provider, default
  model — only when the pipe offers the model) → tier 1b (provider switch) →
  tier 2 (coordinator), so default behavior on the bound provider is always
  preferred.
