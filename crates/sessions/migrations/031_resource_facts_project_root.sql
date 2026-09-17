-- ADR-65 F5c: scope `resource_facts` per project root (migration 031).
--
-- Phase 4 (migrations 029/030) keyed `resource_facts` by `path` alone
-- (`path TEXT PRIMARY KEY`). With more than one project root sharing a session
-- store, two roots observing the same relative path (both have `src/main.rs`,
-- for example) silently overwrote each other's rows — the derived evidence
-- spine thrashed across projects, so one root's clean observation could be
-- clobbered by (or mistaken for) another root's.
--
-- A single ADD COLUMN cannot fix this: SQLite cannot alter the primary key in
-- place, and leaving `path` as the PK with a separate hash column would keep
-- trashing rows across roots. The row identity is the pair
-- `(project_root_hash, path)`, so this migration rebuilds the table with that
-- composite primary key.
--
-- Legacy rows (written before roots were recorded) keep their observations
-- under the empty root hash `''` and are preserved verbatim — rebuild and
-- historical audit still see them. They are never served (the serve path
-- requires a matching non-empty root hash), just kept for attribution.
--
-- SQLite ALTER TABLE cannot change the primary key in place, and the rebuild
-- is small (bounded per-path rows), so: create the new-shape table, copy every
-- legacy row, drop the legacy table, rename, and recreate the dirty index —
-- now scoped per root.

CREATE TABLE resource_facts_new (
    path                 TEXT    NOT NULL,
    project_root_hash    TEXT    NOT NULL,  -- blake3 hex of the canonical project root ('' for legacy rows)
    generation           TEXT    NOT NULL,  -- content-addressed generation id (ADR-65 §2)
    size_bytes           INTEGER,
    mtime_ms             INTEGER,
    content_hash         TEXT,
    last_event_id        TEXT,
    last_agent_id        TEXT,
    observed_at          INTEGER NOT NULL,
    dirty                INTEGER NOT NULL DEFAULT 1,
    content_cached       TEXT,
    content_cached_bytes INTEGER,
    PRIMARY KEY (project_root_hash, path)
);

INSERT INTO resource_facts_new (
    path, project_root_hash, generation, size_bytes, mtime_ms, content_hash,
    last_event_id, last_agent_id, observed_at, dirty, content_cached, content_cached_bytes
)
SELECT path, '', generation, size_bytes, mtime_ms, content_hash,
       last_event_id, last_agent_id, observed_at, dirty, content_cached, content_cached_bytes
FROM resource_facts;

DROP TABLE resource_facts;

ALTER TABLE resource_facts_new RENAME TO resource_facts;

-- Scoped clean/inventory queries (`WHERE dirty = 0 AND project_root_hash = ?`)
-- now lead with the dirty flag then the root hash.
CREATE INDEX idx_resource_facts_dirty_path ON resource_facts(dirty, project_root_hash);