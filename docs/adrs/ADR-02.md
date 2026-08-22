# ADR-02: Correlation / Event IDs — ULID

**Status:** Accepted
**Date:** 2025-07-10
**Deciders:** Concerto architecture

## Context

Every `Event`, session, task, and agent run needs a unique identifier that is
also useful for log correlation across the event bus, the SQLite audit log
(Phase 2), and the observability/tracing export (Phase 8). IDs need to sort
roughly by creation time so logs and audit rows read in chronological order
without a separate `created_at` join, and they need to be safe to embed in
URLs (Phase 3's backend API) and in shell-friendly log output.

## Decision

Use ULID (Universally Unique Lexicographically Sortable Identifier) via the
`ulid` crate for all correlation IDs, event IDs, session IDs, and task IDs.

## Consequences

- IDs are time-sortable: `ORDER BY id` in SQLite gives chronological order
  for free, with no extra `created_at` index.
- URL-safe, no hyphens — easier to grep out of logs and tracing output than a
  hyphenated UUID, and drops cleanly into REST paths (`/sessions/{id}`).
- 128 bits, same size as a UUID — no storage cost.
- Less universally recognized than UUID by tooling that specifically expects
  UUID format; not a concern here since IDs are internal and exposed only
  through our own API surface (`api-types`).

## Alternatives Considered

- **UUID v4:** random, no sortability, hyphenated — would need a separate
  timestamp column everywhere IDs are stored for chronological queries.
- **UUID v7 (time-ordered):** sortable like ULID, but less ergonomic string
  encoding (still hyphenated, base16) and a newer/less battle-tested crate
  ecosystem at time of decision.
- **Auto-increment integers:** trivial to sort, but not safely generatable
  client-side or across distributed components without a central counter —
  doesn't fit a local-first, potentially multi-process architecture.

---

*Last updated: 2026-08-22 (retrospective consolidation — see [README](./README.md)).*
