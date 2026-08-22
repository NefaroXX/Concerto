-- ADR-60 D3: per-subscriber whiteboard subscriptions + persisted cursors
-- (protocol 0.2.0 whiteboard-subscription push).
--
-- One row per subscriber (the registered agent_id). `scopes` is the compact
-- JSON serialization of the subscriber's `Vec<WhiteboardScope>` topic list
-- (`WhiteboardKind` topics), declared at handshake. `cursor_gate_seq` is the
-- subscriber's acknowledged position in the whiteboard log: a `gate_seq`
-- consistent-cut coordinate ("everything <= cursor already acked and
-- applied"). It advances ONLY on the agent's `AckWhiteboard` — never at
-- enqueue — so a stalled (full) mailbox never advances the cursor and nothing
-- is lost: on registration/restart the supervisor drains from the persisted
-- cursor and resumes pushing (at-least-once, resume-from-cursor).
--
-- The raw `whiteboard_events` log remains append-only and untouched; this
-- table is per-subscriber projection state keyed by subscriber, not session
-- (a subscriber outlives sessions, mirroring the detached-audit convention).

CREATE TABLE whiteboard_subscriptions (
    subscriber_id    TEXT    PRIMARY KEY,
    scopes           TEXT    NOT NULL,
    cursor_gate_seq  INTEGER NOT NULL DEFAULT 0
);