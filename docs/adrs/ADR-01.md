# ADR-01: Git Library — `gitoxide` (`gix`)

**Status:** Accepted
**Date:** 2025-07-10
**Deciders:** Concerto architecture

## Context

Every agent run that writes to the filesystem needs git integration: stash-based
session rollback (ADR-15), diff computation alongside `imara-diff` (ADR-05), and
later (Phase 2) a full git tool (`status`, `diff`, `add`, `commit`, `branch_*`,
`stash_*`, `log`, `restore`). The project ships as a single static binary with no
runtime dependencies — pulling in `git2` means linking `libgit2`, which means a C
dependency and a more complicated cross-compilation/packaging story for Phase 8's
Linux/Windows/macOS binaries.

## Decision

Use `gix` (the `gitoxide` project) for all git operations. Pure Rust, no C
dependency, keeps the single-binary distribution story simple.

## Consequences

- Single-binary builds stay simple across all three target platforms.
- `gix`'s API surface is younger than `git2`'s and may not cover every
  operation needed by Phase 2's git tool. **Mitigation (see Risk Register):**
  check API coverage before Phase 2 begins; fall back to shelling out to the
  `git` binary (wrapped behind the same internal trait) as a last resort for
  any specific missing operation, rather than dropping `gix` wholesale.
- No `libgit2` version-skew issues to track.

## Alternatives Considered

- **`git2` (libgit2 bindings):** mature, complete API coverage, but pulls in a
  C dependency — rejected on the single-binary / pure-Rust constraint (see
  Guiding Principle 5).
- **Shelling out to the `git` CLI exclusively:** no dependency at all, but
  loses structured error handling and requires git to be installed and on
  `PATH` — acceptable as a targeted fallback, not as the primary strategy.

---

*Last updated: 2026-08-22 (retrospective consolidation — see [README](./README.md)).*
