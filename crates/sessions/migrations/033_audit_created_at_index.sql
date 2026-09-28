-- Audit-log retention index (row 19: retention/archival).
--
-- `prune_audit` (crates/sessions/src/audit_retention.rs) deletes rows older
-- than the configured window with `DELETE FROM audit_log WHERE created_at < ?`
-- and copies them into an attached archive first. Without an index on
-- `created_at` that predicate is a full table scan over a log that grows
-- monotonically; the index turns the scan into a bounded range seek.
--
-- `idx_audit_session` (002) cannot serve this query: it is keyed on
-- `session_id`, and prune is time-based across all sessions.

CREATE INDEX IF NOT EXISTS idx_audit_created_at ON audit_log(created_at);
