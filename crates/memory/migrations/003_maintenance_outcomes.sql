-- Migration 003: Observed maintenance outcomes (H12, ADR-65).
--
-- Durable record of maintenance EVENTS performed against this memory
-- database: revalidation, supersession, contradiction, expiry and orphan
-- pruning. Each row says WHAT was acted on (`target` + `target_kind`), the
-- evidence cited (`evidence`, a JSON array of ids), the outcome
-- (`completed` | `degraded` | `failed`) with a reason, WHO performed it
-- (`actor`), WHEN (`observed_at`, unix seconds UTC), and the project the
-- pass ran for (`project_id`).
--
-- Why rows, not log lines: per ADR-65 §3/§4 the runtime authors observed
-- facts and decisions cite existing evidence. A maintenance run that could
-- not complete (store locked, embedder down, cancelled mid-pass) MUST be
-- recorded as `degraded`/`failed` — an absent record for an attempted run
-- is the defect H12 closes. Log lines are not queryable and not durable
-- across the process boundary that continuity spans.
--
-- Identity: targets are memory-native ids (chunk ids, decision ids) and the
-- sweep target is the project id. This deliberately does NOT reuse the
-- `resource_facts` keying (`project_root_hash` + `canonical_project_path`):
-- that scheme identifies workspace *file paths*, and a maintenance event
-- acts on a stored memory object, not a path. The memory database is already
-- per-project, and `project_id` records which project a sweep ran for.
--
-- `kind`/`outcome` are free-form TEXT with no CHECK constraint: the Rust
-- boundary parses them and skips (fail-open) anything unrecognized, so an
-- older binary reading a newer row never misinterprets it and never errors.
-- The table is append-only: outcomes are never updated or deleted by this
-- code — the ledger is an observation log, not mutable state.

CREATE TABLE IF NOT EXISTS maintenance_outcomes (
    id          TEXT    PRIMARY KEY,
    kind        TEXT    NOT NULL,
    target      TEXT    NOT NULL,
    target_kind TEXT    NOT NULL,
    evidence    TEXT    NOT NULL DEFAULT '[]',
    outcome     TEXT    NOT NULL,
    reason      TEXT    NOT NULL DEFAULT '',
    actor       TEXT    NOT NULL,
    project_id  TEXT    NOT NULL,
    observed_at INTEGER NOT NULL
);

-- Point lookups: "was this memory ever maintained?" (per-target reads).
CREATE INDEX IF NOT EXISTS idx_maintenance_outcomes_target
    ON maintenance_outcomes(target);

-- Sweep history for one project, newest first.
CREATE INDEX IF NOT EXISTS idx_maintenance_outcomes_project
    ON maintenance_outcomes(project_id, observed_at);
