-- ADR-40: audit log is append-only and outlives session pruning.
-- Rebuild audit_log with a nullable session_id carrying ON DELETE SET NULL so
-- that deleting a session (e.g. `concerto sessions prune`) detaches rather
-- than deletes its audit rows. All columns from 002 + 016 are preserved and
-- idx_audit_session is recreated.
--
-- Safe inside a transaction: audit_log is a leaf table (nothing references
-- it) and PRAGMA foreign_keys is not toggled, so the table swap is atomic.

CREATE TABLE audit_log_new (
    id             TEXT    PRIMARY KEY,
    session_id     TEXT    REFERENCES sessions(id) ON DELETE SET NULL,
    correlation_id TEXT    NOT NULL,
    tool_name      TEXT    NOT NULL,
    verdict        TEXT    NOT NULL,
    input_hash     TEXT    NOT NULL,
    rule_matched   TEXT,
    user_response  TEXT,
    created_at     INTEGER NOT NULL,
    profile_id     TEXT,
    resolved_executable TEXT,
    argv           TEXT,
    working_directory  TEXT,
    network_requested  INTEGER,
    filesystem_scope   TEXT,
    destructive_classification TEXT,
    exit_code      INTEGER,
    duration_ms    INTEGER,
    toolchain_version TEXT
);

INSERT INTO audit_log_new (id, session_id, correlation_id, tool_name, verdict,
    input_hash, rule_matched, user_response, created_at, profile_id,
    resolved_executable, argv, working_directory, network_requested,
    filesystem_scope, destructive_classification, exit_code, duration_ms,
    toolchain_version)
SELECT id, session_id, correlation_id, tool_name, verdict, input_hash,
    rule_matched, user_response, created_at, profile_id, resolved_executable,
    argv, working_directory, network_requested, filesystem_scope,
    destructive_classification, exit_code, duration_ms, toolchain_version
FROM audit_log;

DROP TABLE audit_log;

ALTER TABLE audit_log_new RENAME TO audit_log;

CREATE INDEX idx_audit_session ON audit_log(session_id);