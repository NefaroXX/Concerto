-- ADR-60 D5 whiteboard checkpoints: gate-boundary snapshots for reversibility and deterministic replay.
CREATE TABLE IF NOT EXISTS whiteboard_checkpoints (
    id TEXT PRIMARY KEY,
    gate_seq INTEGER NOT NULL,
    snapshot TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_whiteboard_checkpoints_gate_seq ON whiteboard_checkpoints(gate_seq);
