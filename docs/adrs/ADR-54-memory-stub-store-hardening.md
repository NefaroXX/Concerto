# ADR-54: Stub Global Memory, Self-Heal Stores, and Identify All Failures

**Status:** Accepted (2026-08-08)
**Date:** 2026-08-08
**Deciders:** maintainer (after live-test failure)
**Composes with:** ADR-46 (tiered memory), ADR-52 (orchestration safety gates)
**Supersedes:** none

## Context

A live run aborted with `INTERNAL_ERROR` because the runtime eagerly opened
`global_memory.db` at startup and a stale/unreadable file surfaced as
`(code: 14) unable to open database file`. The error had no classification
arm, so it fell through to the generic `INTERNAL_ERROR` — a message that says
nothing and helps nobody.

Investigation showed:

1. **Global memory is part of the tiered memory system, which is not fully
   implemented yet.** The store type (`GlobalMemoryStore`), the
   `MemoryNamespace::Global` variant, the `MemorySystem` wiring (all `Option`-
   checked), and the helpers exist and are tested; but there are **zero
   production consumers** — nothing ever routes a Global query in a real run.
2. The eager open was the only place the tiered system touched the real
   filesystem, and it was hard-fatal.
3. The same failure class exists for `sessions.db` and `memory/memory.db`
   (garbage/truncated file aborts the store open).
4. Several `OrchestratorError`/`ProviderError` variants had no classification
   arm and fell through to `INTERNAL_ERROR`.

## Decision

### 1. Global memory is retained but STUBBED (not opened, not removed)
- Keep the full API surface for the tiered system: `GlobalMemoryStore`
  (`crates/memory/src/global.rs`), `MemoryNamespace::Global`,
  `ProjectIdHelper::{user_id_hash, global_namespace}`, and the
  `MemorySystem::new(…, global_store: Option<…>)` wiring — `MemorySystem`
  already treats an absent global store gracefully.
- `init_memory_system` no longer opens or connects any global database; it
  passes `None`, with a comment pointing at this ADR.
- Consequence: a missing/unreadable `global_memory.db` can never abort a run.
  The tiered system re-enables by restoring the connect block when it lands.
- The pre-existing `global_memory.db` file on disk is left untouched.

### 2. Store self-heal (quarantine + recreate)
- `sessions.db` and `memory/memory.db`: when the first open fails **and the
  file is not a valid SQLite database** (magic-header check added to
  `concerto_core::helpers`), the file is renamed to
  `<name>.corrupt-<unix_utc_ts>.bak` and the open is retried once against a
  fresh database; the retry's failure surfaces the *original* error.
- A file **with a valid SQLite header is never quarantined**: a schema or
  migration failure on real data surfaces as an error instead of silently
  deleting user history.
- The open is made deterministic: after connect, a `PRAGMA schema_version`
  probe fails a garbage/truncated file at open time instead of on the first
  query.
- Rationale: an optional project store must never dead a run, and quarantine
  preserves the evidence (the `.bak` file) for forensics.

### 3. Every failure is identifying (no accidental INTERNAL_ERROR)
- New `MEMORY_INIT_FAILED` classification for the memory-init error family.
- All previously-unmapped `OrchestratorError` variants
  (`Unrecoverable`, `TaskGraphError`, `Tool`, `Memory`,
  `MultiAgentPlanFailed`, `InvalidTaskGraph`, `NoBudgetForDelegation`) and
  `ProviderError` variants (`HttpStatus`, `Serialization`, `InvalidResponse`,
  `Other`) classify to specific codes.
- **Exhaustive unit tests** iterate every known `OrchestratorError` and
  `ProviderError` variant and assert each classifies to a specific code — no
  variant may fall through to `INTERNAL_ERROR`.
- `INTERNAL_ERROR` remains reserved for genuinely unknown (future,
  `#[non_exhaustive]`) variants.

### 4. `concerto health` reports store status
- New `=== Store Status ===` section (text and `--json`): for `sessions.db`
  and `memory/memory.db` — exists / SQLite-magic-valid / state (`ok`,
  `absent`, `corrupt (rebuilt on next open)`) plus any `.corrupt-*.bak`
  quarantine backups found.
- Read-only: no database opened, no writes; deterministic offline.

## Consequences

- **Positive:** the run-aborting failure class is gone (stores self-heal;
  global memory is never opened); every surfaced error carries a specific
  code; users can diagnose store health with one command; corrupted-store
  evidence is preserved as `.bak` quarantines.
- **Negative:** an old, genuinely-broken-but-valid-header database (e.g., a
  migration mismatch) will still fail with a specific, surfaced error —
  intentional, since silently replacing it would destroy data.
- **Risk:** exhaustive classification tests must be kept in sync when new
  enum variants are added (the failing assertion names the offender).

## Review notes

- Replacement hypothesis confirmed on the user's machine for the CANTOPEN
  class; the full store-self-heal and classification are verified by unit
  tests (sessions, memory, core, CLI).
- The tiered memory system re-enables global storage with a single wiring
  change (`runtime_runner` stub block) plus an `init` fallback for
  unreadable files — no other call sites need to change.