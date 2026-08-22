CREATE TABLE agent_run_results (
    id               TEXT    PRIMARY KEY,
    task_id          TEXT    NOT NULL REFERENCES subtasks(id),
    session_id       TEXT    NOT NULL REFERENCES sessions(id),
    role             TEXT    NOT NULL,
    outcome          TEXT    NOT NULL,
    summary          TEXT    NOT NULL,
    files_modified   TEXT    NOT NULL DEFAULT '[]',
    tool_call_count  INTEGER NOT NULL DEFAULT 0,
    cost_usd         REAL    NOT NULL DEFAULT 0.0,
    latency_ms       INTEGER NOT NULL DEFAULT 0,
    created_at       INTEGER NOT NULL
);
CREATE INDEX idx_agent_results_task    ON agent_run_results(task_id);
CREATE INDEX idx_agent_results_session ON agent_run_results(session_id);
