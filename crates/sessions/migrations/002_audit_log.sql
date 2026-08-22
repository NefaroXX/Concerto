CREATE TABLE IF NOT EXISTS audit_log (
    id             TEXT    PRIMARY KEY,
    session_id     TEXT    NOT NULL REFERENCES sessions(id),
    correlation_id TEXT    NOT NULL,
    tool_name      TEXT    NOT NULL,
    verdict        TEXT    NOT NULL,
    input_hash     TEXT    NOT NULL,
    rule_matched   TEXT,
    user_response  TEXT,
    created_at     INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_audit_session ON audit_log(session_id);
