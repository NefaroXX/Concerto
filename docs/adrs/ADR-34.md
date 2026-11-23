# ADR-34: Durable orchestration runtime

**Status:** Accepted
**Date:** 2026-07-27
**Deciders:** Concerto architecture

## Context

Long multi-agent runs currently combine several independent mechanisms:

- provider retry is implemented both around individual requests and around a
  complete specialist run;
- graph checkpoints are returned to the desktop as transient JSON;
- the live broadcast event channel is also the persistence recorder's input;
- shared working memory is constructed separately from the task graph; and
- context reduction is applied after each iteration without proving that the
  active request is under pressure or becomes smaller.

Those seams permit indefinite provider waits, repeated side effects, lost
resume state, cross-session checkpoint reuse, incomplete event history, and
context growth during "compaction".

## Decision

Concerto uses the following runtime contracts:

1. A logical provider request has one retry boundary. It includes response
   headers and the complete response stream, has finite attempt and elapsed
   limits, and applies separate time-to-first-byte and stream-idle deadlines.
   A specialist run is never replayed as transport recovery.
2. The session database is authoritative for active orchestration checkpoints.
   A checkpoint is versioned and scoped by session, project, root task, run,
   objective, and source revision. Running tasks are reconciled to pending
   after a process restart.
3. The durable event recorder consumes a bounded per-process queue (see
   **Amendment (2026-11-17)**). Broadcast remains the low-latency UI fan-out and
   is not the persistence boundary.
4. The task graph and its completed results materialize the typed working
   memory supplied to specialists. Checkpoints include that snapshot.
5. Context reduction is request-budget driven. It is a no-op below its trigger
   and must strictly reduce estimated active tokens. Durable transcript data is
   not deleted merely because a provider request needs a smaller projection.
6. Parallel specialist execution is bounded globally and per provider. Planned
   Coder tasks own disjoint artifact sets or are serialized.

## Consequences

- A provider outage pauses with a resumable partial result instead of waiting
  forever.
- Retrying transport cannot repeat completed tool side effects.
- Continue can recover after restart without rerunning completed graph nodes or
  accepting a checkpoint from another project/session.
- Live UI consumers may still lag; the durable queue preserves retained events
  but is no longer guaranteed complete under sustained consumer stalls (see
  **Amendment (2026-11-17)**).
- Specialist prompts receive smaller role-relevant projections and can be
  reconstructed consistently after restart.

## Amendment (2026-11-17) — the durable event queue is bounded and overflow is counted

The original decision called the durable recorder queue "lossless." It is now
bounded at `DURABLE_QUEUE_CAPACITY = 8192` events per subscriber with
drop-oldest overflow. Delivery is lossless only while the consumer keeps up: on
overflow the oldest events are discarded, the retained suffix stays in publish
order, and the cumulative loss is exposed as
`DurableHealth::dropped_events`. The publisher never blocks on a stalled
consumer, and `lagging` latches on overflow and clears when the backlog drains.
The superseded `DURABLE_PENDING_LAG = 65536` threshold is gone.

The session store remains authoritative for persistence. Consumers detect a
gap by comparing `DurableEventReceiver::dropped_events()` across receive
iterations: `EventRecording::finish` fails closed on an observed gap, the
durable event and transcript recorders log a rate-limited error, and
`--events-json` writes an `events_dropped` record before the next line. A
consumer that needs a complete audit trail must drain fast enough or reconcile
from the store; the event stream is not an unbounded lossless log.

