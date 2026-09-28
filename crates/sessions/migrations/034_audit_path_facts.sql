-- Path-shaped structured facts: expand the audit log with the operation and
-- target path of a path-shaped tool action (filesystem, git, LSP), recorded
-- alongside the ADR-28 §6 command facts.
--
-- Motivation: filesystem operations carried `command_facts: None`, so every
-- audit row left `argv`, `working_directory`, `resolved_executable` and
-- `filesystem_scope` empty and recorded only a SHA in `input_hash`. The
-- operation (`read`/`write`/`move`/`delete`/…) and the target path were
-- unrecoverable. These columns close that gap.
--
--   * path_operation        — the operation the tool attempted.
--   * attempted_path        — the path/URL exactly as supplied by the caller
--                             (URL query/fragment/userinfo already stripped).
--   * resolved_path         — the confined absolute path after workspace
--                             containment (NULL when resolution was rejected).
--   * attempted_destination — destination as supplied (move/copy only).
--   * resolved_destination  — confined absolute destination (move/copy only).
--
-- All columns are nullable so rows written by earlier migrations default to
-- NULL, keeping the change backward compatible. `input_hash` is retained: it
-- is still recorded and existing consumers depend on it.

ALTER TABLE audit_log ADD COLUMN path_operation TEXT;
ALTER TABLE audit_log ADD COLUMN attempted_path TEXT;
ALTER TABLE audit_log ADD COLUMN resolved_path TEXT;
ALTER TABLE audit_log ADD COLUMN attempted_destination TEXT;
ALTER TABLE audit_log ADD COLUMN resolved_destination TEXT;
