CREATE TABLE IF NOT EXISTS tasks (
    id          TEXT    PRIMARY KEY,
    session_id  TEXT    NOT NULL REFERENCES sessions(id),
    description TEXT    NOT NULL,
    status      TEXT    NOT NULL DEFAULT 'running',
    created_at  INTEGER NOT NULL,
    completed_at INTEGER
);
CREATE INDEX IF NOT EXISTS idx_tasks_session ON tasks(session_id);
