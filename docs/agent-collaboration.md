# Multi-agent collaboration

Multi-agent mode is explicit: enable it with the desktop toggle/default setting
or the CLI `--multi-agent` flag. Concerto does not silently turn it on because a
task appears complex.

There is no Chat/Plan/Build mode picker. The `mode` key was retired at schema
v6 and the picker removed with it; the intent *gate* remains always-on, but it
routes authorization, not topology. The Coordinator decides shape of work in
every mode. See [Configuration and schema in
architecture.md](architecture.md#configuration-and-schema).

## Who is in the roster

Agents are **configuration, not code** (ADR-58). The orchestrator compiles no
agent table: the roster is the built-in seeds merged with whatever
`[[multi_agent.custom_agents]]` you declare, and the dispatch prompt hardcodes
no roles or ids at all (ADR-74). The five seeds are:

| Seed id | Stage tag | Output mode |
|---|---|---|
| `architect` | `design` | `DesignDoc` |
| `researcher` | `research` | `ResearchReport` |
| `coder` | `implement` | `Freeform` |
| `reviewer` | `review` | review report |
| `validator` | `validate` | validation report |

`coordinator` is reserved: it is constructed in code, is never registered from
config, and therefore never appears in the roster the delegation guard inspects.

### Stage tags, not role names

A seed's `stage` is one tag from an open affinity vocabulary, not a fixed
pipeline position. ADR-35 is explicit that a stage tag is **context, not a
dispatch rule**, and a stage-less agent is a full participant. Two knobs extend
an agent's reach:

- **`stage`** — the agent's own tag.
- **`can_cover`** — *additional* tags this agent can cover, additive to `stage`.
  Effective coverage is `stage ∪ can_cover`
  (`AgentRegistry::effective_coverage`). A stage-less agent that lists
  `can_cover = ["implement"]` is fully eligible for implementation work.

`can_cover` is the same shape as ADR-43's `[skills]`/`[mcp]`: config-driven v1.
The desktop Studio round-trips it so a save cannot silently drop it, but there is
**no UI editor** for it yet. Config is the source of truth.

## Who can write

Write access is derived from the stage kind, not from a hardcoded per-role table
(`StageKind::default_capability_mask`, ADR-58 D1). Exactly one kind grants
anything:

| Stage kind | Default write mask | Meaning |
|---|---|---|
| `Execution` | `fs_write` + `shell` | may modify the project |
| every other kind | none | read/analysis only |

An agent's raw `capabilities` flags default to unset and are *resolved* from that
mask, so on the `standard` blueprint only the `implement` stage can write. Every
tool call still passes the normal `ToolExecutor` and policy engine regardless of
capability — capabilities decide what is permitted, policy decides whether it is
allowed. See [policy-rules.md](policy-rules.md).

## The standard pipeline

The `standard` blueprint is five stages. Stages are ordered; a relationship
connects them.

| Order | Stage tag | Kind | Seed agent | Primary | Fallback persona |
|---:|---|---|---|:---:|---|
| 1 | `design` | `Planning` | `architect` | | |
| 2 | `research` | `Research` | `researcher` | | |
| 3 | `implement` | `Execution` | `coder` | ✓ | |
| 4 | `review` | `Review` | `reviewer` | | `coordinator` |
| 5 | `validate` | `Acceptance` | `validator` | | `coordinator` |

Other shipped blueprints (`tdd`, `docs`, `analysis`) reorder or resplit these
stages; see `BlueprintKind` in `crates/config/src/blueprint.rs`. The blueprint is
yours to edit in the Studio or in config — nothing above is compiled in.

## Relationships

Relationships add semantic handoff and revision behavior to directed **stage
pairs**. They do not replace task dependencies, and their endpoints are stage
tags, not agent ids.

| `kind` | Semantics |
|---|---|
| `supervises` | source reviews the target; bounded repair rounds |
| `provides_context_to` | source supplies research/context to target |
| `reports_to` | legacy closed-list kind, folded into the open catalog as `supervises` |
| `owns_design` | source owns design constraints used by target |

The kind vocabulary is **open** (ADR-58): an unknown kind that is not a
registered catalog entry is a hard config error, but new kinds can be declared.
Rules reject self-relationships, `max_cycles = 0` is rejected, and re-declaring a
directed pair replaces the previous rule.

### Cycle budgets

`max_cycles` is a **per-stage** property with a per-kind engine default, not a
per-relationship field:

| Stage kind | Default max cycles |
|---|---:|
| `Review` | 6 |
| `Acceptance` | 5 |
| everything else | 1 |

A stage with an explicit `max_cycles` overrides its kind default. The sum of
stage caps is bounded by the blueprint rulebook.

### Defaults

With no relationships configured, the `standard` blueprint validates to:

| From stage | Kind | To stage |
|---|---|---|
| `review` | `supervises` | `implement` |
| `validate` | `supervises` | `implement` |
| `research` | `provides_context_to` | `implement` |
| `design` | `owns_design` | `implement` |
| `design` | `owns_design` | `research` |

## The Coordinator's delegation doctrine (ADR-74)

This is the part most likely to surprise you, so it is stated plainly: **the
Coordinator delegates by default and may not quietly do the work itself.**

- Delegation is the default action for implementation, review, and validation
  work. The blanket "you are a full agent" self-execution license is deleted
  from the dispatch prompt, and a regression test asserts its absence.
- Self-execution is lawful on **roster exhaustion** only, enumerated as
  exactly three cases: the roster is empty, the roster is disabled or
  unavailable, or delegation was attempted and genuinely failed.
- A `delegation-required` guard enforces this on the Coordinator's own tool
  path, not just in the prompt. A mutating call (`write`, `shell`, `git`, and
  the `filesystem` tool's destructive operations) is **refused** with a named
  `PolicyVerdict` when the roster is non-empty and no delegation has been
  recorded. Read-only calls are never refused. The refusal is fail-closed and
  arrives back to the model as a structured tool error, so the loop recovers by
  dispatching rather than by stalling.
- Lawful self-execution is *recorded*: `CoordinatorSelfImplementing` carries the
  exhaustion reason. A refused one publishes no such event — the refusal is the
  `PolicyVerdict`. So "who did this?" is answerable from the audit trail alone.
- The guard asks only "is the roster empty?". It never inspects which stage is
  staffed, because that would constrain operator-chosen rosters.

The Coordinator executes through the same `ToolExecutor`, policy engine,
`VirtualFs`, approval sink, and cancellation token as every specialist, tagged
with the `coordinator-self-execute` sentinel.

## Dependency-aware scheduling

Tasks are nodes in a directed acyclic graph. A blocking `MustFinishBefore` edge
prevents the dependent task from becoming ready until its prerequisite completes.
Ready independent tasks may run concurrently. The graph is cycle-validated before
execution.

This means a review or validate task must not be launched merely because it
appears in the same plan; if it depends on an implementation, it waits. Research
handoffs likewise become available to dependent tasks only once their
prerequisite lands.

## The failure ladder

Subtask failures are classified as `Recoverable` (retry the same agent),
`LimitReached` (retries exhausted, or a hard provider/model failure: auth,
context overflow, no affordable model), or `NonRecoverable` (cancellation,
structural). `LimitReached` walks two independent axes, and the **agent axis
comes before provider escalation**:

| # | Tier | What changes | ADR |
|---:|---|---|---|
| 1 | Same agent, global default model | neither agent nor provider | ADR-42 |
| 2 | **Agent-axis takeover** — a same-stage peer, then any agent whose coverage includes the target stage | the agent | ADR-74 |
| 3 | Default-provider re-target | the provider | ADR-45 |
| 4 | Coordinator self-execution — only for subtasks with no expected file artifact, and only on roster exhaustion | who does the work | ADR-45, ADR-74 |

The failing agent is never its own takeover candidate, and a non-covering agent
is never selected. A successful takeover returns immediately and publishes a
ladder note; a failed or unattributable takeover publishes a note and the ladder
continues. Tier 2 is attempted **once per task** (checkpointed, so a resume does
not reset it), and the dispatch counts against the run-wide cap.

Note that ADR-74 **deliberately reverses** an earlier invariant: a hard-failed
subtask *is* reassigned to a same-stage peer. That peer is now the preferred
target rather than a forbidden one. `ladder_hard_failure_takes_over_to_same_stage_peer`
replaced the test that asserted the old rule, so the reversal is visible in the
diff rather than smuggled.

### Ladder hold — a cooling-down rung is held, not abandoned

The planning rung follows a different rule, and it is a deliberate asymmetry: a
throttled provider is **held**, not demoted.

- `RetryExhausted` carries a `retry_after` hint, populated from the final
  attempt's provider delay and also read from live `RateLimit` and `HttpStatus`
  variants.
- A throttled exhaustion with a known hint waits out the cooldown
  (cancellation-checked) and retries **the same provider**. A rung that recovers
  stays the primary pipe.
- Only when the hold budget is spent, or the rung still fails after its cooldown,
  does the fallback run — and the decision trail calls it a **bridge**, not an
  abandonment.
- Auth failures, 404/model-not-found, malformed requests, and capability refusals
  escalate immediately: a class that will not heal on a timer is not held.

| Constant | Value | Bounds |
|---|---:|---|
| `MAX_PLANNING_HOLD` | 30 s | ceiling on a *single* hold, whatever the hint asks for |
| `MAX_PLANNING_HOLDS` | 2 | holds per run; after the second, bridge rather than wait |
| `MAX_PLANNING_RECOVERY_ROUNDS` | 3 | planning-recovery rounds per run |
| `planning_recovery_in_progress` | — | re-entrancy guard; no nested recovery |

The hold currently covers the **planning rung only**. The per-subtask ladder in
the table above has no hold: a throttled specialist dispatch still walks the
agent axis and the provider tiers without waiting out a cooldown. Extending it is
a known follow-up, bounded by the same constants.

## Models and spending

Per-agent provider/model assignment lives under
`model_settings.agent_assignments`, keyed by the roster id. See
[Provider and Model Configuration](models.md). Tool-calling requirements are
*derived* from the blueprint and capabilities, not a hardcoded role table; on the
`standard` blueprint the result is `researcher`, `coder`, and `validator`, but
that set is the default outcome, not a rule.

All specialist calls share the session spend tracker with the policy gate.
`spend_cap_multiplier` scales the permitted multi-agent budget relative to the
normal session cap; it creates no provider quota.

## Bounded repair

Structured handoffs carry design, research, implementation, or review content,
plus source/target, task id, and rationale. Review or validation feedback can
return to the implementer within the applicable stage cycle budget.

Tool and subtask correction is intentionally bounded. Provider transport retry
uses `[retry]`, whose production defaults are **8** attempts, a **15-minute**
outage fuse, a **120-second** time-to-first-byte deadline, and a **300-second**
stream-idle deadline:

- transient provider failures use retry/backoff;
- a tool error is returned to the implementer as correction context;
- recoverable specialist failures are retried with the failure details;
- once recovery or cycle limits are exhausted, Concerto preserves useful changes
  and reports a blocked/partial outcome;
- cancellation, invalid configuration, budget exhaustion, and genuinely fatal
  infrastructure failures can stop dispatch.

A recoverable specialist problem should not surface as a generic
`INTERNAL_ERROR`. If it does, capture the events and report it as an
error-classification defect.

## CLI activation

From the top-level binary built with the CLI feature:

```bash
concerto --cli --multi-agent
```

The standalone `concerto-cli` binary also accepts `--multi-agent`. Desktop
activation is available in Settings and the chat toggle. Note that CLI
subcommands (`audit`, `health`, `providers`, …) are only reachable with `--cli`
on a desktop-capable build.
