# ADR-87: Persist root provider turns independently of rendered summaries

- Date: 2026-11-16
- Status: Accepted — full local battery and all 12 exact-head CI checks passed on 2dbf25a
- Related: ADR-36, ADR-48, ADR-85, ADR-86; H39 in the harness reliability log

## Problem

Run settlement wrote a summary to `messages`, while the original assistant
calls, reasoning, native replay state and tool results remained in memory.
A later provider failure could therefore erase completed work from the next
request. Serialization tests alone did not cover this production boundary.

## Decision

The single-agent loop and in-process coordinator dispatch loop append completed
assistant turns before tool execution and append each completed tool result.
Text-driver repair replies and continuation instructions have the same durable
ordering. Initial submissions and delivered inbox messages retain their existing
owners. Incomplete provider attempts never become completed conversation turns.

Each assistant turn owns its provider-reported input and output usage. Run-level
metrics account for every completed request, including repair requests before a
later error. They reset between external runs and accumulate across continuation
rounds. Settlement does not insert another assistant row with aggregate usage.

Runtime facts also survive: verification may run after the last provider
reply, and a blocked or failed run needs a durable termination reason. Labeled
system observations record task identity, actual completion status, changed
paths, compact verification outcomes/evidence references and termination detail.
They carry no token usage and cannot impersonate original assistant turns.
Full verifier output remains in the evidence stores. These observations are
included when context is reassembled from the durable transcript.

The separate typed `transcript` recorder owns rendered status replies, errors and final
summaries. A status reply must not insert a second assistant message between its
original tool call and result. Existing stored summaries remain readable; this
change cannot reconstruct provider turns that earlier runs never saved.

Persistence is best effort and explicitly degraded on failure. Completed work
uses a separate cleanup cancellation domain with a ten-second deadline; caller
cancellation releases it too. Ambiguous writes are not automatically retried.
This does not provide exactly-once execution or an atomic transaction spanning
a tool side effect and its database result.

## Interrupted exchanges

Context assembly preserves the stored source transcript. For each assistant
call batch, it groups recorded tool responses before corrective/user messages.
Missing responses receive request-only `outcome_unknown` annotations: the call
may have executed, and workspace/audit evidence must be inspected before retry.
These annotations do not claim success, execute tools, grant approval, or enter
the durable source as fabricated execution evidence. Native assistant replay
state and recorded results remain intact. Matching is scoped to each assistant
turn rather than globally to a possibly reused call ID.

## Verification and limits

Regression coverage exercises real disk-store reopen, context assembly and a
fresh captured provider request after success and later failure; cancellation;
repair success, later failure and exhaustion; follow-up delivery; continuation
and separate-run usage; recorded status replies; interrupted exchanges; and
bounded cleanup. Full battery results belong in the work log.

This milestone covers root single-agent and coordinator dispatch history.
Independent specialist and supervised-process histories still need their own
agent-scoped persistence and restore contract. They must not be mixed into the
root provider stream. H39 remains open for that work. H40 separately tracks the
headless coordinator's clipped history selection.
