-- Read-derived version claims outlive the gate that returned the bytes.
-- These rows are protection state, not write decisions or whiteboard events.
CREATE TABLE gate_read_scopes (
    project_root TEXT NOT NULL,
    session_id TEXT NOT NULL,
    saturated INTEGER NOT NULL DEFAULT 0 CHECK (saturated IN (0, 1)),
    PRIMARY KEY (project_root, session_id)
);
CREATE TABLE gate_read_versions (
    project_root TEXT NOT NULL,
    session_id TEXT NOT NULL,
    agent_id TEXT NOT NULL,
    path TEXT NOT NULL,
    version TEXT NOT NULL,
    PRIMARY KEY (project_root, session_id, agent_id, path)
);
CREATE INDEX gate_read_versions_session ON gate_read_versions(session_id);
CREATE INDEX gate_read_scopes_session ON gate_read_scopes(session_id);

-- Old transcripts may retain file contents but have no durable claims. Never
-- silently turn such missing provenance into a hash of the current disk.
CREATE TABLE gate_read_legacy_sessions (session_id TEXT PRIMARY KEY NOT NULL);
INSERT INTO gate_read_legacy_sessions (session_id)
SELECT id FROM sessions;
