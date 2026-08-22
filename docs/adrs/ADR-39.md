# ADR-39: Embedder Degradation Handling — Stale-Mark, Backoff Pause, Explicit Event, FTS-Only Fallback with Notice

**Status:** Accepted (2026-08-02)
**Date:** 2026-08-02
**Deciders:** Concerto architecture

## Context

On embedder error (model not downloaded / offline / provider failure), the
indexer currently stores a zero-vector placeholder and logs via
`tracing::warn!` only:

- `crates/memory/src/indexer.rs` (~lines 195-208): the embed best-effort block
  substitutes `vec![0.0; self.embedder.dims()]` on
  `self.embedder.embed(&chunk).await` returning `Err`, then proceeds to write an
  `EmbeddingRecord` with that zero vector.

The zero vector is stored as a real similarity row. Vector search then
ranks all such chunks as identical/near-identical and returns garbage; FTS
keeps working. Because the only signal is a `tracing::warn`, degradation is
silent from the user's perspective — semantic search silently returns
wrong results.

`EventKind` lives in `crates/core/src/event.rs` (enum begins ~line 20). The
enum is `#[non_exhaustive]` and the `sanitized()` match already has a catch-all
`other => other` arm, so adding a variant requires updating match sites, but the
surface for that is small and expected.

## Decision

1. **Never store zero-vector placeholder rows that participate in vector
   similarity.** On embed error the chunk is recorded for FTS with the vector
   ABSENT/EMPTY (no similarity row), so vector queries cannot return those
   chunks and cannot produce garbage rankings.
2. **Track per-project embedder health** — a broken state plus a
   consecutive-failure count plus a backoff deadline. On failure: enter broken
   state; recompute exponential backoff (bounded, e.g. 5s → 120s cap, doubling
   per consecutive failure or an equivalent bounded scheme); a failure during a
   run pauses further embedding attempts for the remaining files of that run
   (FTS chunk recording continues). Backoff is re-entrant: once the current
   window expires a later failure opens a fresh window (backoff re-starts at
   5s), not a permanently-raised floor.
3. **Emit an explicit bus event** on the transition into broken state (per
   backoff window first failure) — a new `EventKind` variant, e.g.
   `EmbedderFailed { project_id, reason }`, so clients get a user-visible
   signal. Adding an `EventKind` variant requires exhaustive coverage at match
   sites; compiler/late-fail through the repo is acceptable and expected.
4. **Vector-search API distinguishes state**: when the embedder is broken for a
   project, vector search falls back to FTS-only and the result carries an
   explicit notice flag (e.g. `degraded: "embedder unavailable — semantic
   search degraded to full-text only"`). When the embedder recovers (a later
   success), the broken state clears and future chunks get vectors;
   previously-recorded vector-less (stale) chunks remain vector-less until a
   future maintenance/re-embed path — explicit **non-goal** of this ADR, stubbed
   and deferred.
5. **No schema change is mandated**: whether a sentinel (empty/absent vector)
   suffices vs a new column is an implementation detail. Prefer avoiding a
   migration if a sentinel suffices; if a column is unavoidable (e.g. a stale
   marker on previously-vector-less rows), the migration must follow the
   crate's migration runner.
6. **CancellationToken parity**: the indexer backoff/pause must remain
   cancellable. Embedding retries must **never** occur inside the
   debouncer/std-thread paths — they are async executor paths only.

## Consequences

### Positive
- Silent semantic corruption is gone: semantic search without a working
  embedder returns FTS results plus a `degraded` notice instead of garbage.
- Backoff bounds churn; the broken state and recovery are observable.

### Negative / Risks
- Small behavior change: semantic search temporarily returns FTS results with a
  notice rather than vector-ranked results.
- Adds one `EventKind` variant (`EmbedderFailed`) and requires updating its
  match sites across the repo.
- Tests must cover: broken → FTS fallback, recovery clears the broken state,
  no zero-vector rows are written, the backoff cap, and event emission.

### Fallback
- If a sentinel proves insufficient to mark stale rows, fall back to a schema
  column shipped via the crate's migration runner, keeping the rest of this
  decision unchanged.

## Alternatives Considered

- **(a) Keep zero-vector placeholders + warn** — rejected: silent garbage
  rankings in vector search.
- **(b) Abort the whole indexing run on embed failure** — rejected: FTS must
  keep working; only the embedding step degrades.
- **(c) Persistent schema migration now** — deferred: prefer the empty/absent
  sentinel; add a column only if the stale-marker requires it (follows the
  migration runner).

## Relationship to Other ADRs

- Complements the memory / FTS+vector hybrid model (see
  `docs/architecture.md`, `docs/crate-graph.md`) without changing its schema by
  default.
- Follows the event-system convention (ADR-05-era) that significant state
  changes get typed `EventKind` variants on the `EventBus` rather than ad-hoc
  `tracing` lines.
- Continues the long-session robustness work on
  `fix/long-session-robustness` (the parent of ADR-38's host-function work).