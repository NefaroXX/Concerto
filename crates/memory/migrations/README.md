# Memory migrations

SQLite migrations for the `concerto-memory` crate, applied by `sqlx::migrate!`
(see `crates/memory/src/storage.rs::MemoryDb::try_connect`). `sqlx` applies
migrations in filename order (zero-padded numeric prefix) and records applied
files in the `_sqlx_migrations` table, so files are never re-run and must not
be edited once they have shipped.

## Numbering convention

- `NNN_description.sql`, zero-padded to three digits.
- New migrations take the next free number — currently **004**.
- Do not renumber existing files: gaps are permanent and harmless.

## Files

- `001_working_memory.sql` — `decisions` and `task_nodes`.
- `002_links.sql` — `memory_links` (ADR-69 slice 1).
- `003_maintenance_outcomes.sql` — `maintenance_outcomes` (H12): the durable
  ledger of observed maintenance events (revalidation, supersession,
  contradiction, expiry, orphan pruning) with their outcome and reason.

## Why maintenance outcomes live here (not in `crates/sessions`)

The maintenance actions this ledger observes — `DecisionStore::supersede`, the
ADR-69 link-verdict application, and `TtlManager`'s expiry / orphan-prune /
revalidation sweeps — all act on memory state held in THIS database and run
against THIS pool. Recording them here keeps the observation beside the state
it describes and lets each action emit its own outcome without an unwired
service seam (an unwired seam would reintroduce the "absent record" failure
H12 exists to close). The sessions-side migration number (041) is unrelated:
that database holds session/audit/resource-fact state, not memory objects.
