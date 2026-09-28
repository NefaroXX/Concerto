# ADR-40: Audit Log is Append-Only and Outlives Session Pruning

**Status:** Accepted (2026-08-02) — §Decision item 3 superseded by
[ADR-73](./ADR-73-audit-encryption-and-retention.md) (2026-09-26) and restated
below in its settled form, so item 3 is read here, not skipped; items 1, 2
and 4 remain in force unchanged. Not archived: the amendment is scoped to one
clause.
**Date:** 2026-08-02
**Deciders:** Concerto architecture

## Context

`concerto sessions prune` (H-6 of the long-session robustness wave) deletes a session and all of its dependent
rows in one transaction. The audit log (`crates/sessions/migrations/002` +
`016`) records every policy decision (`tool_name`, `verdict`, `input_hash`,
`rule_matched`, `user_response`, timestamp, plus structured command facts from
ADR-28). It is write-only today (no read/query path) and is part of the
security envelope described in `SECURITY_BOUNDARIES.md`: `SimplePolicyEngine`
emits a decision record, then the tool runs.

The first H-6 implementation treated `audit_log` as just another dependent
table and deleted its rows alongside the session. That defeats the audit
trail's purpose: an audit log is only meaningful while it is complete. A
maintenance command that silently shortens the decision record makes later
forensic review (what was approved/denied, for which reason, against which
input hash) impossible, and it does so without any space-pressure justification:
audit rows are small (a few hundred bytes) and grow only with policy decisions,
not with message volume.

A second, purely mechanical constraint: `audit_log.session_id` was
`TEXT NOT NULL REFERENCES sessions(id)` with no `ON DELETE` action, so deleting
a session row while audit rows still referenced it raised a foreign-key
violation. Removing the rows was the path of least resistance, but not the
correct one.

## Decision

1. **`audit_log` is append-only and never trimmed by session lifecycle
   operations.** `delete_session` stops deleting audit rows. Session pruning
   reduces the *working* data (messages, events, spend, tasks, checkpoints,
   transcript) but never the decision record.
2. **Detach, don't delete.** Migration `021_audit_session_nullable.sql`
   rebuilds `audit_log` with `session_id TEXT REFERENCES sessions(id)
   ON DELETE SET NULL` (nullable, no `NOT NULL`). When a session row is
   deleted, its audit rows survive with `session_id` set to NULL — the full
   decision facts (verdict, input hash, rule, tool, timestamp, structured
   command facts) stay reviewable; only the (already gone) owning session
   pointer is nulled. Because the audit log is write-only today, no reader
   needs to handle both shapes; any future reader must treat `session_id` as
   nullable.
3. **Audit retention is a time-based policy, never a session-lifecycle one.**
   Session pruning does not reach the log (item 1), and a bound on the log is
   an *age-based, operator-configured* decision that must not be folded into
   session deletion: the two act on different axes, so neither may be a
   consequence of the other. As settled by
   [ADR-73](./ADR-73-audit-encryption-and-retention.md) (2026-09-26, Accepted,
   implemented in `7351128`), that bound is **archive, then delete, verified**:
   rows strictly older than `retention_days` are copied into a keyed
   `audit-archive.db` and only then removed from the hot `audit_log`, in one
   transaction, and the archive write is **verified before** any delete, so a
   row is never removed without a copy. It is configured through the global
   config `[audit]` section (`retention_days`, `archive_dir`) and **defaults to
   off**, so a default install keeps item 1's exact grow-only behavior; `0` is
   folded away at the config seam and rejected by the prune path rather than
   meaning "delete everything". The archive carries the same at-rest key, and
   the enclosing `sessions.db` — the log included — is encrypted at rest under
   `[audit] encrypt_at_rest` (also default off). There is no size-based bound:
   retention is age-based only, as this item framed the question. The
   distinction is load-bearing and unchanged — aging a row out never deletes a
   session, and pruning a session never ages a row out.
4. The table rebuild copies all current columns (002 + 016 additions) and
   recreates `idx_audit_session`; this is executed in the standard sqlx
   migration transaction.

## Consequences

- `concerto sessions prune` output and docs no longer claim audit rows are
  removed; the CLI prints the same session bullets as before (audit is
  implicit).
- The `delete_session` implementation drops the explicit
  `DELETE FROM audit_log` statement and relies on the `ON DELETE SET NULL`
  foreign key (enforced because every new connection sets
  `PRAGMA foreign_keys = ON`).
- Space: sessions keep their audit rows after pruning; this is deliberate and
  costs only the tiny audit-row footprint. Correctness/record-keeping beats
  a marginal space win for a log that is supposed to be durable. The hot log
  is bounded separately and by age only (item 3), and only when an operator
  opts in.
- Compatibility: existing databases migrate in place; existing rows keep their
  `session_id`; only future deletes null it.
- Supersedes nothing; amends the H-6 PRD's draft behavior (which was
  "delete audit with the session") as decided during review.