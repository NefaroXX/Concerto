-- Migration 001: Working memory tables for decisions and task decomposition.
--
-- These tables were previously in opencode-sessions (migration 008) but are
-- owned by opencode-memory, which manages its own database.

CREATE TABLE IF NOT EXISTS decisions (
    id             TEXT    PRIMARY KEY,
    session_id     TEXT    NOT NULL,
    task_id        TEXT,
    what           TEXT    NOT NULL,
    why            TEXT    NOT NULL,
    outcome        TEXT,
    category       TEXT    NOT NULL DEFAULT 'other',
    confidence     REAL    NOT NULL DEFAULT 0.0
                    CHECK  (confidence >= 0.0 AND confidence <= 1.0),
    superseded_by  TEXT,
    created_at     INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_decisions_session ON decisions(session_id);

CREATE TABLE IF NOT EXISTS task_nodes (
    id          TEXT    PRIMARY KEY,
    session_id  TEXT    NOT NULL,
    parent_id   TEXT,
    description TEXT    NOT NULL,
    status      TEXT    NOT NULL DEFAULT 'pending'
                CHECK  (status IN ('pending','running','done','failed','blocked')),
    blocking    TEXT    NOT NULL DEFAULT '[]',
    created_at  INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_task_nodes_session ON task_nodes(session_id);
CREATE INDEX IF NOT EXISTS idx_task_nodes_parent ON task_nodes(parent_id);
