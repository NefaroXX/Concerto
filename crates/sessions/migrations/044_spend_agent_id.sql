-- Per-agent spend attribution (H39b). Existing rows read back as NULL (root /
-- unattributed), so the column is additive and backward compatible.
ALTER TABLE spend_records ADD COLUMN agent_id TEXT;
CREATE INDEX IF NOT EXISTS idx_spend_session_agent ON spend_records(session_id, agent_id);
