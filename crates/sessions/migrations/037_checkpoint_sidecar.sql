-- ADR-82 §4 (slice 5): durable file-checkpoint sidecar (migration 037).
--
-- Two tables, one migration (the ADR allows exactly one schema change here):
--
-- * `file_checkpoints` — the `CheckpointRecord` envelope per checkpoint:
--   identity + owning session + lifecycle `status` + the full record JSON +
--   creation time. The `status` COLUMN is the prune-eligibility source of
--   truth (it is what the retention job reads); `record_json` is the audit
--   envelope, patched in the same transaction whenever the column moves.
-- * `checkpoint_pre_images` — the restorable BYTES, keyed by
--   (`checkpoint_id`, `canonical_path`): content hash (the verifier),
--   `content` BLOB (the restore source; NULL for hash-only rows), byte size,
--   capture provenance (`snapshot` at creation vs `pre-image` backfilled by
--   the first post-checkpoint mutation), and capture time.
--
-- Derived data, not the durable reference (migration-030 precedent): the
-- hashes on the never-pruned whiteboard log are the reference; these rows are
-- repudiable bytes, and `rebuild_from_log` deliberately does not touch them
-- (a resource-facts rebuild neither repopulates nor deletes sidecar rows).
-- Retention (`FileCheckpointStore::prune_checkpoints`, same run as
-- `prune_audit`) deletes sidecar rows whose checkpoint is no longer Active OR
-- is older than `retention_days` — delete-only, no archive — and transitions
-- pruned-bytes checkpoints to `Expired`, withdrawing the restorable claim.
--
-- Encryption is conditional (ADR-73 interaction): these tables live inside
-- `sessions.db`, so they are encrypted WHEN the operator enables
-- `encrypt_at_rest` (same guarantee and same operator control as every other
-- table), and plaintext otherwise. No per-table keying exists.
--
-- No foreign keys (ADR-40): checkpoint rows deliberately outlive sessions.

CREATE TABLE IF NOT EXISTS file_checkpoints (
    checkpoint_id TEXT PRIMARY KEY,
    session_id    TEXT    NOT NULL,
    status        TEXT    NOT NULL,
    record_json   TEXT    NOT NULL,
    created_at    INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS checkpoint_pre_images (
    checkpoint_id  TEXT    NOT NULL,
    canonical_path TEXT    NOT NULL,
    content_hash   TEXT    NOT NULL,
    content        BLOB,
    size_bytes     INTEGER NOT NULL,
    capture_kind   TEXT    NOT NULL,
    captured_at    INTEGER NOT NULL,
    PRIMARY KEY (checkpoint_id, canonical_path)
);

CREATE INDEX IF NOT EXISTS idx_file_checkpoints_status ON file_checkpoints(status);
CREATE INDEX IF NOT EXISTS idx_file_checkpoints_created ON file_checkpoints(created_at);
CREATE INDEX IF NOT EXISTS idx_checkpoint_pre_images_checkpoint
    ON checkpoint_pre_images(checkpoint_id);
