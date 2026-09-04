-- ADR-65 evidence spine: derived `resource_facts` table (migration 029).
--
-- This is a **derived projection, not a source of truth**: the authoritative
-- record is the `whiteboard_events` log (026), from which this table is fully
-- rebuildable forward (idempotent, recomputable — ADR-64 derived-view rule,
-- ADR-65 §4). Materialized because the read-dedupe and dirty checks sit on the
-- execution hot path and need indexed lookups.
--
-- Per path the table caches the workspace-generation facts most recently
-- observed (by a `ToolExecuted` fact or a `WorkspaceSnapshot`): generation,
-- size, mtime, content hash, and the attribution of the observation
-- (`last_event_id` / `last_agent_id` / `observed_at`).
--
-- `dirty` (1 = dirty/uncertain, 0 = clean) is the honesty bit: a row is clean
-- only when snapshot/observe state equals workspace reality. Dirtying events
-- (`WriteApplied`, watcher change hints, shell/git side effects) flip it to 1
-- but never rewrite the observation columns — the cached observation history
-- survives for audit and reconciliation. A path with no row at all is also
-- "uncertain" and must execute normally; `lookup` answers `None`.
--
-- Writes to this table happen only through the `resource_facts` store module;
-- it is never edited in place by agents.

CREATE TABLE IF NOT EXISTS resource_facts (
    path            TEXT    PRIMARY KEY,
    generation      INTEGER NOT NULL,
    size_bytes      INTEGER,
    mtime_ms        INTEGER,
    content_hash    TEXT,
    last_event_id   TEXT,
    last_agent_id   TEXT,
    observed_at     INTEGER NOT NULL,
    dirty           INTEGER NOT NULL DEFAULT 1
);
CREATE INDEX IF NOT EXISTS idx_resource_facts_dirty_path ON resource_facts(dirty, path);