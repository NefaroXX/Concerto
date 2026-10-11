-- ADR-82 slice 5 follow-up (the slice-1 debt): promote the canonical
-- accounting identity out of the `user_response` JSON envelope into
-- dedicated `audit_log` columns.
--
-- ADR-82 slice 1 recorded `canonical_tool` / `canonical_operation` by folding
-- them into the free-form `user_response` column as a compact JSON envelope,
-- because (a) no migration was in that slice's scope and (b) the retention
-- archive (`audit_retention.rs`) copies `audit_log` with `SELECT *` into an
-- explicitly-listed archive table — the two had to change in lockstep. This
-- migration is that lockstep change: the archive DDL and INSERT column list
-- gained the same two columns in the same physical position.
--
--   * `canonical_tool` — the canonical policy-view tool name of the recorded
--     action (`filesystem` for a `write` alias call): the identity policy
--     evaluated and the one accounting keys on.
--   * `canonical_operation` — the canonical operation of the recorded action
--     (`write` even when the caller's raw input named no `operation`).
--
-- `tool_name` keeps recording the requested (registered) name for
-- attribution; these columns carry the accounting identity (ADR-82
-- "canonical effect key"). Both are nullable TEXT, appended at the end of the
-- physical column order (after `result_facts`, matching the archive schema),
-- so every pre-038 row defaults to NULL and the change is additive: an older
-- binary ignores the columns it does not know, a newer binary on an old
-- database gets them here.
--
-- Backfill (same migration, JSON1 — built into SQLite since 3.38; the
-- bundled SQLCipher build is based on SQLite 3.45): rows written by the
-- slice-1 envelope writer get their columns populated by parsing
-- `user_response` — only when it is a JSON object carrying TEXT
-- `canonical_tool` / `canonical_operation` keys. Every other row (plain
-- text, other JSON shapes, NULL) is left untouched with NULL columns, and
-- `user_response` itself is NEVER rewritten: it keeps exactly what the
-- writer recorded. The guarded UPDATE is idempotent even though sqlx
-- records the migration as applied exactly once. The CASE gates the JSON
-- type probes because SQLite does not guarantee WHERE-term evaluation
-- order — a bare `json_valid(x) AND json_type(x, ...)` could probe an
-- invalid JSON value and error out.

ALTER TABLE audit_log ADD COLUMN canonical_tool TEXT;
ALTER TABLE audit_log ADD COLUMN canonical_operation TEXT;

UPDATE audit_log
SET canonical_tool      = json_extract(user_response, '$.canonical_tool'),
    canonical_operation = json_extract(user_response, '$.canonical_operation')
WHERE canonical_tool IS NULL
  AND canonical_operation IS NULL
  AND CASE
        WHEN json_valid(user_response)
             AND json_type(user_response, '$') = 'object'
        THEN json_type(user_response, '$.canonical_tool') = 'text'
          OR json_type(user_response, '$.canonical_operation') = 'text'
        ELSE 0
      END;
