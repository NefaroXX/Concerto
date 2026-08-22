-- Migration 014: checkpoints table for session checkpointing
CREATE TABLE checkpoints (
    id BLOB NOT NULL PRIMARY KEY,
    session_id BLOB NOT NULL REFERENCES sessions(id),
    task_id BLOB NOT NULL,
    label TEXT NOT NULL,
    virtual_fs_snapshot TEXT NOT NULL,
    sequence_num BIGINT NOT NULL,
    created_at TEXT NOT NULL
);
