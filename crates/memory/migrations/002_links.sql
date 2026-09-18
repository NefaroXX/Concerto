-- Migration 002: Symbolic link store (ADR-69 slice 1).
--
-- Directed, typed edges between memory chunks. Written as a side effect of
-- the L1 dedup update/merge path (a new chunk "supersedes" or "merges" the
-- chunk it replaces) and of the `refs` / `result_ref` metadata carried on
-- those paths. Writes are append-only and idempotent: re-indexing upserts
-- the same (source_id, target_id, link_type) row instead of duplicating it.
--
-- `link_type` is intentionally free-form TEXT with no CHECK constraint: the
-- ADR's broader vocabulary ('supports', 'contradicts', 'extends') arrives in
-- slice 2, and unknown kinds are skipped at the Rust boundary, fail-open, so
-- an older binary never misinterprets a newer row.

CREATE TABLE IF NOT EXISTS memory_links (
    source_id  TEXT    NOT NULL,
    target_id  TEXT    NOT NULL,
    link_type  TEXT    NOT NULL,
    weight     REAL    NOT NULL DEFAULT 1.0,
    created_at TEXT    NOT NULL,
    expires_at TEXT,             -- TTL-based decay (slice 2); NULL until then
    PRIMARY KEY (source_id, target_id, link_type)
);

-- Slice-2 in-degree scoring reads target-first; keep that lookup indexed.
CREATE INDEX IF NOT EXISTS idx_memory_links_target ON memory_links(target_id);