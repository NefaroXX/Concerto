# Session migrations

SQLite migrations for the sessions crate, applied by `sqlx::migrate!` (see
`crates/sessions/src/lib.rs` and `crates/orchestrator/src/runtime_runner.rs`).
`sqlx` applies migrations in filename order (zero-padded numeric prefix) and
records applied files in the `_sqlx_migrations` table, so files are never
re-run and must not be edited once they have shipped.

## Numbering convention

- `NNN_description.sql`, zero-padded to three digits.
- New migrations take the next free number — currently **024**.
- Do not renumber existing files: gaps are permanent and harmless.

## Known gaps (005, 006, 009, 010, 011)

These were **never committed** in any branch of this repo's history:

- `git log --all --diff-filter=D -- crates/sessions/migrations/` shows zero
  deletions, and no commit ever added a 005/006/009/010/011 file.
- The Phase-3 MVP commit (`73048bb`) added 002-004 and 007 in one shot,
  already skipping 005-006; Phase 5 (`67f6278`) added 012-013, skipping
  009-011.

The gaps are presumed squashed or renumbered during early development, before
those large phase commits (which bundled many uncommitted steps). The numbers
stay intentionally unused; the next migration is 024.
