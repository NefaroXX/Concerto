-- ADR-60 D3: durable whiteboard event log (append-only source of truth for
-- concurrent-agent runs) — S1 vertical-slice substrate.
--
-- The whiteboard carries findings, decisions, write applications/rejections,
-- failures, plan/design artifacts and consolidations. Ordering is a central
-- sequencer (research brief §4, option A): `gate_seq` is the global total
-- order — the consistent-cut coordinate for checkpoints ("everything ≤ seq S")
-- — and `agent_seq` is the per-agent monotonic sequence used for per-agent
-- recovery. Both are assigned at insert time by the append path (S1) and by
-- the supervisor's write gate (S2) under a BEGIN IMMEDIATE transaction, so
-- concurrent appends never collide.
--
-- `session_id` is nullable with no foreign key: whiteboard events outlive
-- session pruning (detached-audit convention, same as migration 021 on
-- audit_log, but without the FK because the log is keyed by agent/plan, not
-- session). `plan_id` is the future #152 structured-state key (ADR-60 D7,
-- plan_bindings is a different concern — no FK here either).
--
-- `content_hash` is a deterministic blake3 fingerprint of the canonical event
-- fields (see whiteboard.rs) assigned by the caller's attested content;
-- `pre_image_hash` is filled by the write gate for write events (hash of the
-- target before apply) and is NULL for non-write events. The raw log is never
-- summarized or deleted (audit-trail requirement, D3).

CREATE TABLE whiteboard_events (
    event_id        TEXT    PRIMARY KEY,
    gate_seq        INTEGER NOT NULL UNIQUE,
    agent_id        TEXT    NOT NULL,
    agent_seq       INTEGER NOT NULL,
    kind            TEXT    NOT NULL,
    scope           TEXT    NOT NULL DEFAULT '',
    session_id      TEXT,
    plan_id         TEXT,
    causation       TEXT,
    payload         TEXT    NOT NULL,
    content_hash    TEXT    NOT NULL,
    pre_image_hash  TEXT,
    created_at      INTEGER NOT NULL
);

-- Backstop for the per-agent sequence (assigned under the write lock, so this
-- is defense in depth, not the primary mechanism).
CREATE UNIQUE INDEX IF NOT EXISTS idx_whiteboard_agent_seq
    ON whiteboard_events(agent_id, agent_seq);
-- Subscriber catch-up / replay cursors and future #152 structured-state reads.
CREATE INDEX IF NOT EXISTS idx_whiteboard_session_gate
    ON whiteboard_events(session_id, gate_seq);
CREATE INDEX IF NOT EXISTS idx_whiteboard_scope_gate
    ON whiteboard_events(scope, gate_seq);
CREATE INDEX IF NOT EXISTS idx_whiteboard_plan_gate
    ON whiteboard_events(plan_id, gate_seq);