# ADR-30: Unified Agent Shell Selection

**Status:** Accepted
**Date:** 2026-07-19
**Deciders:** Concerto architecture
**Supersedes:** ADR-28 sections 2, 4, and 5 where they define independent shell bindings

## Context

ADR-28 introduced separate shell choices for the interactive terminal, agent
tools, and build validation. That allows Settings to display one selected shell
while the agent runner silently uses another. It also lets validation exercise a
different environment from the one that produced the code.

Agents are Concerto's primary shell consumer. The terminal and validator must
not own independent defaults that can drift from agent execution.

## Decision

`ShellSettings` stores one `selected_profile`. The desktop orchestrator, CLI
agent runner, validation engine, integrated terminal, and AI-native shell runtime
all resolve that same profile.

Settings labels this choice **Agent execution shell** and explains the other
consumers. The Terminal page displays the selected profile but does not provide
a private override.

The profile catalog contains shells detected on the current host plus profiles
the user explicitly adds. Legacy placeholder presets and stale detection results
are removed when settings resolve; current detection results are refreshed.

Old configs containing `defaults.interactive_terminal`, `defaults.agent_shell`,
and `defaults.build_validation` remain readable. Migration selects the old
`agent_shell` value because agent execution is authoritative. On the next save,
only `selected_profile` is persisted. Missing selections fall back to the
preferred detected shell, then the first configured profile.

Explicit per-command profile selection inside the custom shell remains allowed
for a single command. It does not change Concerto's configured default.

## Consequences

- Selecting a shell changes agent execution first and predictably.
- Agent commands and validation run in the same executable, environment, PATH,
  and working-directory policy.
- The terminal reflects the agent environment without sharing process state.
- Existing configurations migrate without destructive schema changes.
- Supporting genuinely separate shells later requires an explicit scoped
  override design, not another collection of implicit defaults.
