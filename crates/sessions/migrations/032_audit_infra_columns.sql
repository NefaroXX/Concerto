-- Audit-log infrastructure columns (MCP/plugin failures, denials, violations).
--
-- Adds machine-readable attribution for infra-failure rows written through
-- `AuditLog::record_infra`:
--   * error_kind — failure category (spawn_failed, duplicate_tool, wasm_trap,
--     fuel_exhausted, capability_denied, grant_pruned_hash, ...).
--   * server_id  — MCP server id, when the failure concerns an MCP server.
--   * plugin_id  — WASM plugin id, when the failure concerns a plugin.
--
-- All columns are nullable so rows written by earlier migrations (and policy
-- decision rows, which carry none of these) default to NULL, keeping the
-- change backward compatible. Infra rows are written with `session_id = NULL`
-- so they are not tied to (or pruned with) an agent session; the audit log
-- already allows that since 021_audit_session_nullable.sql.

ALTER TABLE audit_log ADD COLUMN error_kind TEXT;
ALTER TABLE audit_log ADD COLUMN server_id TEXT;
ALTER TABLE audit_log ADD COLUMN plugin_id TEXT;
