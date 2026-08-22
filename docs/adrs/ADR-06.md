# ADR-06: Filesystem Watch — `notify`

**Status:** Accepted
**Date:** 2025-07-12
**Deciders:** Concerto architecture

## Context

Phase 4's long-term memory needs live re-indexing: when a watched file
changes, the index should reflect it within 5 seconds (Phase 4 exit
criterion), and file deletions should trigger immediate vector tombstoning.
This needs a cross-platform filesystem watcher — inotify on Linux, FSEvents
on macOS, ReadDirectoryChangesW on Windows — behind one API, since the
project ships on all three.

## Decision

Use the `notify` crate for all filesystem watching, both for Phase 4's live
re-indexing and any future need to detect external changes to watched
project files.

## Consequences

- One watcher abstraction covers all three target platforms with native
  backends rather than polling, keeping CPU usage low for idle projects.
- Watch events still need debouncing/coalescing in Phase 4 (a single `save`
  in some editors fires multiple raw filesystem events) — `notify` doesn't
  do this itself; budget for a small debounce layer when Phase 4 starts.
- Exclusion patterns (`.git/`, `target/`, `node_modules/`, etc., plus
  `.concerto-ignore`) are applied at the application layer, not by
  `notify` itself — `notify` watches what it's told to watch.

## Alternatives Considered

- **Polling (stat-based diff on a timer):** simpler, no dependency, but
  wastes CPU on idle projects and the 5-second re-index target becomes a
  trade-off against poll frequency rather than a near-immediate native
  event.
- **`watchexec` (as a library):** more of a CLI tool than a library
  dependency for embedding; `notify` is the lower-level primitive it's
  itself built on.

---

*Last updated: 2026-08-22 (retrospective consolidation — see [README](./README.md)).*
