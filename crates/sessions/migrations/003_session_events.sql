CREATE TABLE IF NOT EXISTS session_events (
    id             TEXT    PRIMARY KEY,
    session_id     TEXT    NOT NULL REFERENCES sessions(id),
    sequence_num   INTEGER NOT NULL,
    correlation_id TEXT    NOT NULL,
    event_kind     TEXT    NOT NULL,
    payload        TEXT    NOT NULL,
    created_at     INTEGER NOT NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_session_events_seq
    ON session_events(session_id, sequence_num);
