# ADR-68: H-04 — Breaking parameter change for `request_ack`

**Status:** Accepted

Composes with [ADR-60](./ADR-60-concurrent-agent-runtime.md) (concurrent
agent runtime, process-per-agent supervisor) and [ADR-36](./ADR-36.md)
(typed session transcript). Supersedes: none.

**Date:** 2026-09-18

**Deciders:** Concerto architecture + maintainer direction

## Context

`request_ack` currently takes `(session_id: Ulid, message, cancel)` but the
`session_id` is already bound at the executor seam (which is session-bound).
This creates a dual truth: the caller passes a session ID that the executor
ignores because it is already bound to the session. The mismatch is a
correctness risk — a caller could pass the wrong session ID and the executor
would silently accept it.

Additionally, `PendingAck` lacks a `session_id` field, so ack resolution
cannot confirm which session the ack belongs to without consulting the
executor's bound session. This is fragile in multi-session scenarios.

The breaking change mirrors the pattern established by
`request_plan_approval` (which already takes a session-bound executor).

## Decision

### 1. New signature

```rust
fn request_ack(
    &self,
    session_id: Ulid,
    message: AckMessage,
    cancel: CancellationToken,
) -> AckHandle;
```

The `session_id` parameter is retained in the signature for API clarity
(caller intent is explicit), but the executor validates it matches its
bound session — a mismatch is a hard error.

### 2. `PendingAck` gains `session_id`

```rust
pub struct PendingAck {
    pub session_id: Ulid,   // NEW
    pub message: AckMessage,
    pub created_at: DateTime<Utc>,
    // ... existing fields
}
```

This allows ack resolution to confirm session membership without consulting
the executor.

### 3. Six implementations + five doubles

All six implementations of the trait must adopt the new signature and
validate `session_id` against the bound session. Five test doubles must be
updated to match. No behavioral change beyond the validation.

### 4. Executor seam — already session-bound

The executor is already bound to a session at construction. The new
`session_id` parameter is a validation check, not a routing mechanism. The
executor rejects ack requests where `session_id != bound_session`.

### 5. No migration required

`audit_log.session_id` has been present since migration 002. The new
`PendingAck.session_id` is populated from the executor's bound session at
creation time. No data migration is needed.

### 6. Desktop single-slot: queue-or-reject-busy

The desktop UI currently renders one ack at a time. When a second ack
arrives while one is pending, the UI must either queue it or reject it
with a "busy" signal. This ADR records the decision to **queue** — the
second ack waits until the first is resolved. The queue is bounded
(default: 1 pending ack beyond the active one); overflow rejects with an
explicit error. The queue implementation is deferred to the implementation
phase; this ADR records the policy.

## Consequences

- **Breaking change.** All callers of `request_ack` must pass the
  `session_id` parameter. This is a workspace-wide mechanical change.
- **Validation at the seam.** Mismatched session IDs are caught at the
  executor boundary, not silently ignored.
- **Ack resolution is session-aware.** `PendingAck.session_id` enables
  resolution to confirm membership without executor consultation.
- **Desktop ack queue.** The UI gains a bounded pending-ack queue,
  preventing ack storms from blocking or losing requests.

## Acceptance criteria

- **A1** — All six implementations validate `session_id` against the
  bound session; mismatch is a hard error.
- **A2** — All five test doubles updated to the new signature.
- **A3** — `PendingAck` carries `session_id`; ack resolution confirms
  session membership.
- **A4** — Desktop UI queues at most one pending ack beyond the active
  one; overflow rejects with an explicit error.
- **A5** — No data migration needed; `audit_log.session_id` already
  present since 002.

---

*Last updated: 2026-09-18*
