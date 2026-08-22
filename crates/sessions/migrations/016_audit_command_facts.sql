-- ADR-28 §6/§7: expand the audit log with structured command-execution facts
-- (resolved executable, argv, working directory, profile id, network/destructive
-- /filesystem classifications) and post-execution results (exit code, duration,
-- toolchain version). All new columns are nullable so rows written by earlier
-- migrations default to NULL/empty, keeping the change backward compatible.

ALTER TABLE audit_log ADD COLUMN profile_id TEXT;
ALTER TABLE audit_log ADD COLUMN resolved_executable TEXT;
ALTER TABLE audit_log ADD COLUMN argv TEXT;
ALTER TABLE audit_log ADD COLUMN working_directory TEXT;
ALTER TABLE audit_log ADD COLUMN network_requested INTEGER;
ALTER TABLE audit_log ADD COLUMN filesystem_scope TEXT;
ALTER TABLE audit_log ADD COLUMN destructive_classification TEXT;
ALTER TABLE audit_log ADD COLUMN exit_code INTEGER;
ALTER TABLE audit_log ADD COLUMN duration_ms INTEGER;
ALTER TABLE audit_log ADD COLUMN toolchain_version TEXT;
