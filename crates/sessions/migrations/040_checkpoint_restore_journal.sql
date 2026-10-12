-- Crash-recovery journal for checkpoint restore (H07 recovery half,
-- migration 040).
--
-- Slice 5 left restore crash-unsafe: an interrupted restore was "re-run it
-- and it converges" with no durable record of per-resource progress, no
-- resume path, and no accurate partial-progress reporting across a process
-- kill. These two tables close that gap beside the sidecar they describe:
--
-- * `checkpoint_restore_journal` — one row per (checkpoint, path): the
--   durable per-resource progress of restore (`pending` | `running` |
--   `done` | `failed` | `missing`) plus the `content_hash` verified when the
--   row moved to `done` (the hash the resume path re-verifies against disk
--   before skipping a write) and the latest `attempt_id`/`by_agent`.
--   Upserts are monotonic: `running` never overwrites `done` (a concurrent
--   duplicate submission cannot regress a landed write); `done` always wins.
--   The executor enforces the guard in SQL (`WHERE journal.state != 'done'
--   OR excluded.state = 'done'`), and the in-memory test fake mirrors it.
-- * `checkpoint_restore_attempts` — one row per restore attempt
--   (`attempt_id` PRIMARY KEY, `INSERT OR IGNORE` so re-submitting the same
--   attempt id replays instead of forking): who started when, and the
--   terminal `outcome` (`running` | `restored` | `partial` | `failed` |
--   `cancelled`) stamped by `finish_restore_attempt`.
--
-- Why a table, not whiteboard events: progress is mutable keyed state with
-- atomic upsert + monotonic guards and efficient per-checkpoint reads; the
-- whiteboard is append-only with global ordering, so reconstructing "what
-- landed" would need a full log scan plus compaction, and it is never
-- pruned while this journal shares the checkpoint's retention fate
-- (encryption is likewise conditional via `sessions.db`, ADR-73, same as
-- the sidecar). Lifecycle transitions themselves stay where slice 5 put
-- them (the `file_checkpoints.status` column + patched `record_json`); the
-- journal is the per-attempt durability record beside them, not a new
-- authorization surface (ADR-65: journal writes are bookkeeping and never
-- bypass the policy gate or the ownership checks).
--
-- Honesty rules enforced by the restore path, not the schema:
--
-- * A `done` row is a *candidate* for skipping, never proof: resume
--   re-reads the file from `project_dir` and skips only when its bytes hash
--   to the recorded `content_hash` AND that hash agrees with the record's
--   `post_hash` (a resource marked done but modified since is re-restored,
--   never blindly skipped).
-- * A lost or unreadable journal degrades to "unknown progress, re-verify"
--   (full rewrite, every write still gated + ownership-checked), never to
--   "assume done". Journal write failures are fail-soft (warn, continue);
--   byte failures stay fail-closed (`Partial`, never `Restored`).
-- * Journal rows are audit-retained like the record envelopes: retention
--   never deletes them (one row per checkpoint+path is bounded by the
--   checkpoint's own cardinality; attempts are one small row per call).
--
-- No foreign keys (ADR-40): journal rows deliberately outlive sessions.

CREATE TABLE IF NOT EXISTS checkpoint_restore_journal (
    checkpoint_id  TEXT    NOT NULL,
    canonical_path TEXT    NOT NULL,
    state          TEXT    NOT NULL,
    content_hash   TEXT,
    attempt_id     TEXT    NOT NULL,
    by_agent       TEXT    NOT NULL,
    updated_at     INTEGER NOT NULL,
    detail         TEXT    NOT NULL DEFAULT '',
    PRIMARY KEY (checkpoint_id, canonical_path)
);

CREATE INDEX IF NOT EXISTS idx_checkpoint_restore_journal_checkpoint
    ON checkpoint_restore_journal(checkpoint_id);

CREATE TABLE IF NOT EXISTS checkpoint_restore_attempts (
    attempt_id    TEXT    PRIMARY KEY,
    checkpoint_id TEXT    NOT NULL,
    by_agent      TEXT    NOT NULL,
    started_at    INTEGER NOT NULL,
    finished_at   INTEGER,
    outcome       TEXT    NOT NULL DEFAULT 'running'
);

CREATE INDEX IF NOT EXISTS idx_checkpoint_restore_attempts_checkpoint
    ON checkpoint_restore_attempts(checkpoint_id);
