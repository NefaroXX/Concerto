-- Durable plan bindings (ADR-55 Phase 2b live-fix, restart-safe Apply dialog).
--
-- Mirrors the process-scoped in-memory `PlanApprovalRegistry` on disk so a
-- natural-language approval ("i approve the plan") offered after an app
-- restart can still arm the real Apply/Replan dialog: the newest binding for
-- the session is consulted before falling through to the generic intent gate.
--
-- Semantics mirror the registry: keyed by (session_id, objective_hash),
-- newest-wins per key (UPSERT), and rows are deleted when a plan is applied
-- so a later bare approval ("yes") cannot re-arm a dialog for an
-- already-executed plan.
CREATE TABLE IF NOT EXISTS plan_bindings (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL,
    objective_hash TEXT NOT NULL,
    plan_id TEXT NOT NULL,
    plan_text TEXT NOT NULL,
    source_revision TEXT,
    created_at_ms INTEGER NOT NULL,
    UNIQUE (session_id, objective_hash)
);

CREATE INDEX IF NOT EXISTS idx_plan_bindings_session_created
    ON plan_bindings (session_id, created_at_ms DESC);