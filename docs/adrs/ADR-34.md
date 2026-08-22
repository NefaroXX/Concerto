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
3. The durable event recorder consumes a lossless per-process queue. Broadcast
   remains the low-latency UI fan-out and is not the persistence boundary.
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
- Live UI consumers may still lag, but durable replay remains complete.
- Specialist prompts receive smaller role-relevant projections and can be
  reconstructed consistently after restart.

