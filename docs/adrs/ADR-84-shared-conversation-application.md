# ADR-84: Shared conversation application

Status: Accepted — 2026-10-27

## Problem

The terminal and desktop called the same execution runtime but separately
owned submission, session preparation, cancellation, checkpoint selection and
completion handling. Fixing a conversation interaction in one interface did
not fix the other. A busy composer rejected follow-ups, and a new question
could discard unfinished work because its text differed from the objective.

## Decision

The desktop is a graphical presentation of the same application as the TUI.
`concerto_orchestrator::conversation::ConversationController` owns the
interactive conversation lifecycle. Both adapters submit through
`submit_configured`, execute the returned `ConversationTurn`, and apply
`settle` before updating their views. Session creation/loading, provider
preflight, request construction, cancellation, checkpoint state and stale
completion rejection have one implementation. Renderers retain widget state,
keyboard handling, native dialogs and approval presentation.

Interactive messages always address the coordinator. Users do not select a
single/multi-agent mode to move between discussion and work. Typed conversation
history accompanies each message; the coordinator chooses whether to answer
or use its tools. Headless execution retains its explicit mode contract.

An unfinished checkpoint stays attached to its conversation. A follow-up
re-enters the coordinator with the original objective, graph and history.
Answering a question may settle that message while leaving the work Partial
and resumable. It does not discharge obligations or weaken completion checks.
`reply_to_user` publishes and persists an answer while the decision loop keeps
running, so status questions and informative corrections need not stop work.
Only the coordinator interprets whether the user wants to act, discuss, pause
or replace work; there is no keyword-based chat/work classifier.

The shared inbox accepts follow-ups during work and delivers them at the next
coordinator decision boundary. Graph execution yields before scheduling another
batch. The shared runner continues queued messages in the same session across
that boundary or a finishing run. It does not interrupt a tool or specialist
mid-operation. Stop cancels through the existing cancellation token.

The queue holds at most 32 messages of at most 128 KiB each; continuation across
runtime boundaries is also bounded. Rejected submissions retain the composer.
Unconsumed accepted messages return to the composer on cancellation, error or
limits. Messages are persisted before model delivery. Queued, unconsumed input
is in memory and is not crash-durable. Transcript finalization drains event
messages before writing the last answer.

Interactive conversations use the in-process coordinator because the optional
supervised one-shot child protocol has no operator inbox. Existing policy,
approval and write gates remain in effect. This does not introduce live
cross-process control of a different terminal/desktop instance.

## Maintenance and verification

Conversation behavior belongs in the shared controller/runtime, with regression
tests there. Frontend tests verify input, rendering and adapter wiring. A fix
to shared behavior must not grow a second frontend implementation.

Tests cover follow-ups during provider calls, continued work after a visible
reply, typed history, questions over unfinished obligations, queue bounds,
cancellation recovery, late completions, transcript ordering and both composer
adapters. The remaining settings, inspection and editor parity work described
in ADR-80 is separate from this conversation ownership boundary.
