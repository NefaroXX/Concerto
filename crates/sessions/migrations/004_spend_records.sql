CREATE TABLE IF NOT EXISTS spend_records (
    id          TEXT    PRIMARY KEY,
    session_id  TEXT    NOT NULL REFERENCES sessions(id),
    task_id     TEXT,
    provider    TEXT    NOT NULL,
    model       TEXT    NOT NULL,
    tokens_in   INTEGER NOT NULL,
    tokens_out  INTEGER NOT NULL,
    cost_usd    REAL    NOT NULL,
    created_at  INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_spend_session ON spend_records(session_id);
