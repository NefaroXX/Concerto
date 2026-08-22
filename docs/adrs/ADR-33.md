# ADR-33: Shared frontend project and runtime context

**Status:** Accepted
**Date:** 2026-07-23
**Deciders:** Concerto architecture

## Context

The desktop and terminal frontends depend on the same backend crates but build
different application contexts. They selected projects differently, loaded
different configuration layers, constructed different orchestration paths, and
did not consistently create or resume persistent sessions. The lifetime-wide
`sessions.lock` also prevented the two frontends from using SQLite at the same
time.

This made frontend choice alter execution, policy, memory, and persistence
semantics rather than only presentation.

## Decision

- A shared project registry owns the active and recent canonical project paths.
  An explicit frontend selection updates that registry.
- Project identity is derived from a best-effort canonical path in every
  subsystem.
- Both frontends load the same effective configuration layers for the selected
  project. Global settings writes use the global layer as their base and reload
  the effective project configuration afterward, so project and environment
  overrides are never promoted into the global file.
- Both frontends execute through `run_shared_agent`; frontend-specific manual
  coordinator construction is removed.
- Every interactive run resolves a real project session before execution and
  records its messages, metrics, and event stream in the shared session store.
- SQLite WAL and busy-timeout concurrency replace the process-lifetime
  exclusive lock.
- The accepted safe policy preset from ADR-32 remains the default in every
  frontend. Approval presentation is frontend-specific, but the decision
  semantics are shared.
- Memory initialization and configuration are shared. Decision and task-tree
  stores use the same SQLite database and canonical project namespace; the
  enabled switch gates both frontends, TTL is applied during startup cleanup,
  fast mode explicitly selects a null memory store, and project switches or
  shutdown cancel the prior background index/watch generation.
- Runtime events are persisted against their real session and rehydrate the
  desktop tool log. Both processes append tracing output to the same
  application log.
- Diff review restores a stable pre-review VFS snapshot, applies all rejected
  change hunks in one pass, and materializes the reviewed result back to disk.

## Consequences

- A session created in either frontend can be resumed by the other.
- Desktop and CLI can run concurrently without an artificial lock failure.
- Project configuration, policy, routing, memory, and logs no longer depend on
  which frontend launched the run.
- Frontends retain control over presentation, project pickers, cancellation,
  and approval dialogs without duplicating backend execution logic.
