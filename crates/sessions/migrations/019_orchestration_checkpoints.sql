CREATE TABLE IF NOT EXISTS orchestration_checkpoints (
    session_id      TEXT    PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,
    run_id          TEXT    NOT NULL,
    root_task_id    TEXT    NOT NULL,
    project_id      TEXT    NOT NULL,
    objective_hash  TEXT    NOT NULL,
    schema_version  INTEGER NOT NULL,
    source_revision TEXT,
    sequence_num    INTEGER NOT NULL,
    state_json      TEXT    NOT NULL,
    completed       INTEGER NOT NULL DEFAULT 0,
    updated_at      INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_orchestration_checkpoint_run
    ON orchestration_checkpoints(run_id);

