CREATE TABLE IF NOT EXISTS transcript_entries (
    id             TEXT    PRIMARY KEY,
    session_id     TEXT    NOT NULL REFERENCES sessions(id),
    sequence_num   INTEGER NOT NULL,
    entry          TEXT    NOT NULL,
    created_at     INTEGER NOT NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_transcript_entries_seq
    ON transcript_entries(session_id, sequence_num);
