# Multi-agent collaboration

Multi-agent mode is explicit: enable it with the desktop toggle/default setting
or the CLI `--multi-agent` flag. Concerto does not silently turn it on because a
task appears complex.

## Roles

| Role | Primary responsibility | Writes project files? |
|---|---|---|
| Coordinator | Builds the task graph, dispatches ready work, joins results | No |
| Architect | Design and decomposition | No |
| Researcher | Project, documentation, and memory context | No |
| Coder | Implementation and repair | Yes, through policy-gated tools |
| Reviewer | Correctness, maintainability, and security review | No |
| Validator | Builds/tests and reports evidence | No direct file writes |

Write gates prevent specialists other than Coder from modifying the project.
All tool permissions still pass through the normal policy engine.

## Mode behavior

| Interaction | Expected multi-agent behavior |
|---|---|
| Chat | Coordinator alone answers; no specialists or tools |
| Plan | Coordinator alone produces a plan; no tools or file changes |
| Build | Coordinator plans a DAG and dispatches specialists whose dependencies are ready |

This distinction is part of the release test contract in
[Testing](../TESTING.md).

## Dependency-aware scheduling

Tasks are nodes in a directed acyclic graph. A blocking
`MustFinishBefore` edge prevents the dependent task from becoming ready until
its prerequisite completes. Ready independent tasks may run concurrently.

This means Reviewer and Validator must not be launched merely because they
appear in the same plan; if they depend on an implementation, they wait for
Coder output. Research handoffs likewise become available to dependent tasks.
The graph is cycle-validated before execution.

## Relationships

Relationships add semantic handoff and revision behavior to directed role
pairs. They do not replace task dependencies.

| Config value | Meaning |
|---|---|
| `supervises` | Source reviews the target; `max_cycles` limits repair rounds |
| `provides_context_to` | Source supplies research/context to target |
| `reports_to` | Source reports results/status to target |
| `owns_design` | Source owns design constraints used by target |

Rules reject self-relationships. If `max_cycles` is present it must be greater
than zero. Adding another rule for the same directed pair replaces that pair's
previous rule.

When no relationships are configured, the validated defaults are:

| From | Relationship | To | Maximum cycles |
|---|---|---|---:|
| Reviewer | `supervises` | Coder | 3 |
| Validator | `supervises` | Coder | 2 |
| Researcher | `provides_context_to` | Coder | — |
| Architect | `owns_design` | Coder | — |
| Architect | `owns_design` | Researcher | — |

The Settings relationship manager is the preferred editor. Equivalent TOML:

```toml
[multi_agent]
default_enabled = false
spend_cap_multiplier = 3.0
# ADR-42 fallback ladder tier 1: retry a hard-failed subtask with the same
# agent on a global default model. The agent's provider is bound at startup,
# so default_model must be a model offered by that role's own provider; the
# request is NOT re-routed to another provider. default_provider_config_id
# only disambiguates which routing profile matches when the model name is
# shared across providers (it does not switch the serving provider).
# default_model = "another-tool-capable-model"
# default_provider_config_id = "local-provider"

[[multi_agent.relationships]]
from = "reviewer"
to = "coder"
relationship = "supervises"
max_cycles = 3

[[multi_agent.relationships]]
from = "validator"
to = "coder"
relationship = "supervises"
max_cycles = 2

[[multi_agent.relationships]]
from = "researcher"
to = "coder"
relationship = "provides_context_to"
```

An empty `relationships` list uses the defaults; it is not equivalent to “no
relationships.”

## Models and spending

Per-role provider/model assignment is configured under
`model_settings.agent_assignments`; see [Provider and Model Configuration](models.md).
All specialist calls use the same shared session spend tracker as the policy
gate. `spend_cap_multiplier` scales the permitted multi-agent budget relative
to the normal session cap; it does not create provider quota.

## Handoffs and repair

Structured handoffs carry design, research, implementation, or review content,
plus source/target roles, task ID, and rationale. Review or validation feedback
can return to Coder within the applicable relationship cycle limit.

Tool and subtask correction is intentionally bounded. Provider transport retry
uses `[retry]`. Its production defaults are eight attempts, a 15-minute outage
fuse, a 60-second time-to-first-byte deadline, and a 120-second stream-idle
deadline:

- transient provider failures use retry/backoff;
- a Coder tool error is returned to Coder as correction context;
- recoverable specialist failures can be retried with the failure details;
- hard provider/model limits (auth failure, context overflow, no affordable
  model) walk the ADR-42 fallback ladder before any partial outcome: first the
  same agent on the global `default_model` (a model offered by that role's own
  provider — the fallback never switches the serving provider or reassigns the
  subtask to another agent), then — for subtasks with no expected artifact
  files — direct coordinator self-execution; each tier is attempt-bounded and
  cancellation-aware;
- once recovery or cycle limits are exhausted, Concerto should preserve useful
  changes and report a blocked/partial outcome;
- cancellation, invalid configuration, budget exhaustion, and genuinely fatal
  infrastructure failures can stop dispatch.

A recoverable specialist problem should not be surfaced as a generic
`INTERNAL_ERROR`. If it is, capture the events and report it as an error-
classification defect.

## CLI activation

From the top-level binary built with the CLI feature:

```bash
concerto --cli --multi-agent
```

The standalone `concerto-cli` binary also accepts `--multi-agent`. Desktop
activation is available in Settings and the chat toggle.
