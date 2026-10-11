-- Chunked checkpoint pre-image store (migration 039): the completing step
-- for byte-exact restore above the per-chunk size and for binary files.
--
-- Migration 037 banked one BLOB per path in `checkpoint_pre_images`,
-- capped at `CHECKPOINT_BLOB_LIMIT_BYTES` (64 KiB), with binary excluded by
-- the planning rule — so larger files and binary files degraded to
-- hash-only. This migration adds chunked storage beside it:
--
-- * `checkpoint_pre_image_chunks` — the restorable BYTES, keyed by
--   (`checkpoint_id`, `canonical_path`, `chunk_index`): the whole-file
--   content hash (the verifier, repeated on every chunk so a chunk subset
--   alone still names its file), the `content` BLOB (one
--   `CHECKPOINT_BLOB_LIMIT_BYTES`-capped slice, never NULL), the chunk's
--   byte size, the whole-file byte size (reassembly must reproduce exactly
--   this many bytes), capture provenance (`snapshot` at creation vs
--   `pre-image` backfilled by the first post-checkpoint mutation), and
--   capture time.
--
-- Reassembly rule (enforced by the store, not the schema): chunks for one
-- path must form a dense `0..=max` index run whose concatenated bytes hash
-- to `content_hash` at exactly `size_bytes` — anything else (gap, size
-- mismatch, hash mismatch, tamper) reads as hash-only/missing, never as
-- bytes. Hash stays the durable reference; chunks are the restorable cache
-- (ADR-82 R3 byte-is-repudiable doctrine, unchanged).
--
-- Back-compat: `checkpoint_pre_images` is NOT dropped or altered — older
-- checkpoints with single-BLOB rows keep restoring through the legacy read
-- path. New captures write chunks only. Retention deletes from both tables
-- under the same status/age predicate and transitions pruned-bytes
-- checkpoints to `Expired` (restorable claim withdrawn from that instant).
--
-- Derived data, not the durable reference (migration-030 precedent): the
-- hashes on the never-pruned whiteboard log are the reference; these rows
-- are repudiable bytes, and `rebuild_from_log` deliberately does not touch
-- them. Encryption is conditional (ADR-73 interaction): these tables live
-- inside `sessions.db`, encrypted WHEN the operator enables
-- `encrypt_at_rest`, plaintext otherwise. No foreign keys (ADR-40):
-- checkpoint rows deliberately outlive sessions.
-- No per-checkpoint total is enforced by the schema (SQLite has no
-- cross-row budget constraint worth the trigger machinery); the store
-- enforces `CHECKPOINT_TOTAL_LIMIT_BYTES` at its boundary.

CREATE TABLE IF NOT EXISTS checkpoint_pre_image_chunks (
    checkpoint_id  TEXT    NOT NULL,
    canonical_path TEXT    NOT NULL,
    chunk_index    INTEGER NOT NULL,
    content_hash   TEXT    NOT NULL,
    content        BLOB    NOT NULL,
    chunk_size     INTEGER NOT NULL,
    size_bytes     INTEGER NOT NULL,
    capture_kind   TEXT    NOT NULL,
    captured_at    INTEGER NOT NULL,
    PRIMARY KEY (checkpoint_id, canonical_path, chunk_index)
);

CREATE INDEX IF NOT EXISTS idx_checkpoint_pre_image_chunks_checkpoint
    ON checkpoint_pre_image_chunks(checkpoint_id);
CREATE INDEX IF NOT EXISTS idx_checkpoint_pre_image_chunks_path
    ON checkpoint_pre_image_chunks(checkpoint_id, canonical_path);
