CREATE TABLE IF NOT EXISTS project_active_sessions (
    project_dir TEXT PRIMARY KEY NOT NULL,
    session_id  TEXT NOT NULL,
    updated_at  INTEGER NOT NULL,
    FOREIGN KEY(session_id) REFERENCES sessions(id) ON DELETE CASCADE
);
