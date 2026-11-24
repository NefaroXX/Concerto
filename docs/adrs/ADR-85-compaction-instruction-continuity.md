# ADR-85: Preserve conversational obligations through compaction

**Status:** Accepted — full local battery and all 12 exact-head CI checks passed
**Date:** 2026-11-14
**Amends:** ADR-48's lossy active-window contract

## Problem

Durable compaction replaces old user and assistant messages with bounded
excerpts, then clips those excerpts again during hierarchical merges. The
provider guard independently drops old conversation groups and clips messages.
An instruction or unfinished commitment after either cutoff disappears from
the next model request even though the audit transcript retains it. Moving
surviving text into an untrusted system summary also changes its authority.

No deterministic prefix, keyword list or importance score can establish that
arbitrary omitted conversational text contains no remaining obligation.
In-loop LLM summarization remains excluded by ADR-48.

## Decision

Keep user text and assistant conversational text verbatim, in their original
roles and relative order, across durable leaf and hierarchical compaction.
This includes corrections: preserve their order rather than guessing which
earlier requirement has been superseded. Source messages, not checkpoint
excerpts, supply this text after reload, including for existing checkpoints.

Historical tool exchanges remain eligible for deterministic reduction.
Checkpoint summaries are bounded, explicitly untrusted evidence about those
exchanges. They do not replace user instructions or assistant commitments.
Compacted tool calls and results must be removed together; the recent tail
keeps its complete protocol records. Reasoning remains durable in the source
transcript and is not copied into checkpoints, as in ADR-48.

The final provider guard must preserve user and assistant text too. A literal
marker such as `<working_memory>` in a user message is not provenance and
cannot authorize clipping that message. Working memory, run state, prior
conversation and handoffs remain mandatory even inside system messages;
they carry active obligations. Optional retrieved/file evidence budgets and
historical tool-output reduction remain available. If required
conversation still exceeds the input budget, return `ContextOverflow` before
calling the inner provider. The retry path obeys the same invariant.

Provider wire adapters must retain the text that reaches this boundary.
Anthropic and Gemini combine all system messages in source order, rather than
letting a checkpoint overwrite the policy prompt. Gemini retains assistant
text alongside function-call parts. Opaque thought signatures stay attached
to their original function-call parts. Anthropic's explicit cache boundary
remains confined to the first system message, even with later checkpoints.

## Costs and limits

Text-only conversations cannot shrink indefinitely without a separately
specified mechanism for resolving or replacing obligations. They may now
reach explicit context overflow sooner. Tool-heavy conversations can still
reclaim historical payload and reasoning space. Do not claim a fixed-size
active window or lossless preservation of all historical tool output.

This decision preserves model-visible instructions and commitments. It cannot
prove that a model obeys them, infer task completion, or replace the durable
verification-state checks. Provider token estimates remain approximate.

## Verification required before acceptance

- Long-preamble instructions, hierarchical merges, role/order and unfinished
  assistant commitments survive reload and materialization.
- Provider reduction and retry preserve conversational text or reject before
  an inner call; user-authored markers do not grant reduction permission.
- Anthropic and Gemini wire requests retain every system message; Gemini
  retains assistant text alongside tool calls without altering signatures.
- Tool-heavy input shrinks while source messages remain unchanged; retained
  tool calls/results remain coherent and recent records survive verbatim.
- Existing context-engine, provider and workspace suites, warning-denied
  Clippy, complete local CI battery and exact-head remote checks pass.
