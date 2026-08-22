-- ADR-55 Phase 1d §4: expand the audit log with schema-derived
-- intent-decision columns (bound plan id, source revision the plan was
-- approved at) alongside the existing `user_response` JSON envelope, which is
-- retained for replay/backward compatibility. All new columns are nullable so
-- rows written by earlier migrations default to NULL, keeping the change
-- backward compatible.

ALTER TABLE audit_log ADD COLUMN plan_id TEXT;
ALTER TABLE audit_log ADD COLUMN source_revision TEXT;
