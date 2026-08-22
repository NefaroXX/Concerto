-- Migration 018: align checkpoint timestamps with the SessionStore unix-time contract.
-- Migration 014 declared created_at as TEXT, so SQLite applied text affinity to
-- bound integer timestamps and sqlx could not decode them as i64.
ALTER TABLE checkpoints RENAME TO checkpoints_legacy;

CREATE TABLE checkpoints (
    id BLOB NOT NULL PRIMARY KEY,
    session_id BLOB NOT NULL REFERENCES sessions(id),
    task_id BLOB NOT NULL,
    label TEXT NOT NULL,
    virtual_fs_snapshot TEXT NOT NULL,
    sequence_num BIGINT NOT NULL,
    created_at INTEGER NOT NULL
);

INSERT INTO checkpoints (
    id,
    session_id,
    task_id,
    label,
    virtual_fs_snapshot,
    sequence_num,
    created_at
)
SELECT
    id,
    session_id,
    task_id,
    label,
    virtual_fs_snapshot,
    sequence_num,
    CAST(created_at AS INTEGER)
FROM checkpoints_legacy;

DROP TABLE checkpoints_legacy;
