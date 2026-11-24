-- Per-agent message streams (H39b). Root conversation history stays in
-- `messages`; specialist/agent-process history is stored here keyed by
-- `(session_id, agent_id)` with its own independent sequence. Shapes mirror
-- `messages` (001/042) so `Message` round-trips without loss.
CREATE TABLE IF NOT EXISTS agent_messages (
    id                 TEXT    PRIMARY KEY,
    session_id         TEXT    NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    agent_id           TEXT    NOT NULL,
    agent_sequence_num INTEGER NOT NULL,
    role               TEXT    NOT NULL,
    content            TEXT    NOT NULL,
    tool_calls         TEXT,
    tool_results       TEXT,
    reasoning_content  TEXT,
    tokens_in          INTEGER NOT NULL DEFAULT 0,
    tokens_out         INTEGER NOT NULL DEFAULT 0,
    gemini_replay      TEXT
);
CREATE INDEX IF NOT EXISTS idx_agent_messages_stream
    ON agent_messages(session_id, agent_id, agent_sequence_num);
