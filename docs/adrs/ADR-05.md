# ADR-05: Diff Computation — `imara-diff`

**Status:** Accepted
**Date:** 2025-07-12
**Deciders:** Concerto architecture

## Context

The diff viewer (Phase 2/3/6) needs unified diffs computed from two sources:
the in-session `VirtualFs` (original vs. pending content, before anything is
written to disk) and git diffs (via `gix`, ADR-01). Both need line-level
hunks with accurate context for per-hunk accept/reject. Performance matters
because the Phase 6 side-by-side diff viewer's exit criterion is handling
1000+ line files "without jank."

## Decision

Use `imara-diff` for all diff computation, wrapped in a shared `DiffResult`
type (hunks + line context + additions/deletions) that both the `VirtualFs`
path and the `gix` diff path produce, regardless of source.

## Consequences

- One diff representation flows through to both the CLI (ANSI-colored inline
  diff) and the Iced diff viewer (syntax-highlighted side-by-side) — the UI
  layers never compute diffs themselves, only render `DiffResult`.
- `imara-diff` is byte-level and fast, which matters for the 1000+ line file
  exit criterion in Phase 6.
- Decoupling the diff *algorithm* from the diff *source* (`VirtualFs` vs.
  `gix`) means a future algorithm swap (if one is ever needed) touches one
  module, not the UI.

## Alternatives Considered

- **`similar` crate:** also pure Rust and commonly used, but `imara-diff` was
  chosen for its performance characteristics on the kind of byte-level diffs
  this project produces most often (full-file rewrites from `write_file`
  tool calls, not just line insertions).
- **Shelling out to `diff -u`:** no dependency, but loses structured hunk
  data needed for per-hunk accept/reject — would require parsing unified
  diff text back into structured form, which is more fragile than computing
  it structured in the first place.

---

*Last updated: 2026-08-22 (retrospective consolidation — see [README](./README.md)).*
