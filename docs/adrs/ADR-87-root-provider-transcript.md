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

## Specialist-scoped history and live spend (H39b amendment, 2026-11-22)

In-process specialists use the same durable-turn rule on an agent-scoped
stream. `agent_messages` is keyed by `(session_id, role)` and never mixed into
root `messages`; `AgentRunner` restores the most recent 512 rows, repairs
unanswered tool calls request-only for the prompt, and appends only the turns
produced by the current run. The restored seed is never re-appended. History
I/O is best effort: load or append failure degrades the prompt/durability but
does not fail or settle the run.

Provider usage is accumulated on the run-owned `AgentHistory` handle after
every completed request. On failure the history still carries the tokens and
provider-priced cost for requests that completed before the error, so live
spend settlement retains them and a subsequent dispatch is refused when the
cap is exhausted. The failed in-process attempt is also persisted as one
attributed `spend_records` row (agent id, tokens, cost) when a session store is
attached; the success path leaves spend persistence to run settlement, so one
attempt is never counted twice. This closes the earlier gap where a later
provider error erased usage already consumed.

A specialist assistant turn may request several tools. The assistant batch is
recorded before execution and each completed tool result is appended as it
finishes. If a tool pauses for approval, the ordinary early return preserves
the batch and prior results; the paused call remains unresolved on disk and is
repaired as request-only `outcome_unknown` on the next load. Regression
coverage drives a real two-call batch where the first write succeeds and the
second call pauses, then asserts the file, the recorded batch/result, and the
repaired second outcome.

## Verification and limits

Regression coverage exercises real disk-store reopen, context assembly and a
fresh captured provider request after success and later failure; cancellation;
repair success, later failure and exhaustion; follow-up delivery; continuation
and separate-run usage; recorded status replies; interrupted exchanges; and
bounded cleanup. Full battery results belong in the work log.

The root milestone covers single-agent and coordinator dispatch history.
In-process specialist history is now agent-scoped and restored per the
amendment above. Supervised child history remains parent-owned temp-file
history with parent-side harvest (ADR-60 S5), never the root provider stream;
its remaining crash-window and compaction limits are recorded in the H39b work
log. H40's headless clipped-history selection remains separately tracked.
