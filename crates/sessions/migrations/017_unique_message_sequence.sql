CREATE UNIQUE INDEX IF NOT EXISTS idx_messages_session_sequence
    ON messages(session_id, sequence_num);
