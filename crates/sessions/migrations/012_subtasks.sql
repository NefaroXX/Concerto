CREATE TABLE subtasks (
    id           TEXT    PRIMARY KEY,
    parent_id    TEXT,
    session_id   TEXT    NOT NULL REFERENCES sessions(id),
    role         TEXT    NOT NULL,
    description  TEXT    NOT NULL,
    status       TEXT    NOT NULL DEFAULT 'pending'
                 CHECK  (status IN ('pending','blocked','running',
                                    'awaiting_review','needs_revision',
                                    'completed','failed')),
    dependencies TEXT    NOT NULL DEFAULT '[]',
    deliverable  TEXT,
    graph_json   TEXT,
    created_at   INTEGER NOT NULL,
    completed_at INTEGER
);
CREATE INDEX idx_subtasks_session ON subtasks(session_id);
CREATE INDEX idx_subtasks_parent  ON subtasks(parent_id);
