# ADR-26: Fault containment and recovery in multi-agent runs

**Status:** Accepted
**Date:** 2026-07-16
**Deciders:** Concerto architecture

## Context

The single-agent loop returns tool failures to the model and lets it correct a
call. Specialist agents did not share that behaviour: the Coder made one model
request, executed each returned tool call once, and reported any tool error as
`AgentOutcome::Failed`. The coordinator then converted that first failed
subtask into a run-wide `AgentLoopError`.

This made recoverable conditions such as a missing file, malformed tool input,
policy denial, or transient provider failure appear as `INTERNAL_ERROR` and
discarded the active automation run.

## Decision

Multi-agent execution uses three recovery boundaries. Tool/subtask correction
is attempt-bounded; provider transport retry follows its elapsed-time config:

1. **Provider boundary.** Specialist provider calls use the configured retry
   policy for transient provider failures. Authentication, cancellation, and
   other permanent provider errors are not retried.
2. **Tool boundary.** The Coder keeps its conversation and sends structured
   tool success or failure results back to the model. The model may correct a
   failed call within a bounded number of iterations. Policy denials are
   feedback, not coordinator panics.
3. **Subtask boundary.** A recoverable failed subtask is returned to `Pending`
   and retried with the prior failure included in its context. Independent DAG
   branches remain isolated. Repeated failure is surfaced as a specific
   exhausted-subtask blocker rather than an internal error.

Cancellation, invalid graph/configuration, unavailable model assignment, and
authentication failure remain terminal. Exhausted transient provider recovery
returns a blocked/partial result with preserved progress rather than claiming
success or exposing a generic internal error. All retry sleeps observe the
run's `CancellationToken`.

Tool-correction and subtask-attempt limits are deliberately finite. Provider
transport retry is controlled separately by `RetryConfig`; production defaults
bound it by both attempt count and elapsed time, and every wait observes
cancellation. Response creation, time to first byte, and between-chunk idle
time are separately bounded. Automation must recover from ordinary mistakes
without silently claiming success.

## Consequences

- A wrong filesystem path or tool payload can be corrected without restarting
  the run.
- Provider 429/5xx/network failures behave consistently in single- and
  multi-agent modes.
- A failed attempt consumes cost and is retained as feedback, but does not
  unblock dependants until it succeeds.
- Exhausted recovery is visible and actionable; it is not classified as an
  internal application defect.
- A specialist provider retry recreates only the current provider request. It
  never replays a complete specialist run or an already-executed tool.
