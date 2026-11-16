# ADR-86: Gemini native response replay

**Status:** Proposed — implementation and full local verification complete; exact-head CI pending
**Date:** 2026-11-15

## Problem

Canonical assistant text and a separate tool-call vector cannot represent
native text/call ordering or signatures attached to text. Gemini thought text
also entered the visible answer channel. Its decoder generated local IDs even
when a function call supplied a native ID, preventing exact response matching.

Google requires signatures to remain on their original parts, signed parts not
to be merged, parallel calls to retain order, and native function-call IDs to
be returned with their results. Streaming may end with an empty signed text
part. Sources: [thought signatures](https://ai.google.dev/gemini-api/docs/generate-content/thought-signatures),
[Gemini 3 streaming](https://ai.google.dev/gemini-api/docs/generate-content/gemini-3),
and [function calling](https://ai.google.dev/gemini-api/docs/generate-content/function-calling).

## Decision

- Add optional `gemini_replay` to `CompletionChunk` and `Message`. It contains
  ordered native JSON parts and a BLAKE3 digest of canonical text, reasoning
  and tool calls. The digest detects stale state after edits; it provides no
  authentication or additional authority. Other dialects ignore the field.
- Retain each received part unchanged, including unknown native fields and
  empty text. Route `thought: true` text to reasoning. Native IDs become
  canonical call IDs; absent IDs retain the existing local fallback.
- Bound one turn to 8 MiB of serialized parts and 8,192 parts. Exceeding either
  limit returns an error; no completed replay state escapes. Buffering of
  executable tool calls retains the existing completion boundary.
- Only terminal chunks carry replay state. `CompletedTurn` carries it through
  both collectors and the agent, specialist and coordinator history builders.
  Failed attempts never become successful turns, even after a terminal chunk.
- Migration 042 adds nullable `messages.gemini_replay`; single and batch writes
  serialize it and load restores it. Absent JSON fields default to `None` and
  `None` is omitted on serialization, preserving legacy/provider-plugin input.
- Gemini rendering replays native parts only while the canonical digest
  matches. Edited messages use canonical rendering; this may require a fresh
  model turn if the provider cannot accept the edited signed history. Native
  function-response IDs are emitted only when the corresponding retained
  native call supplied one. Local fallback IDs are never invented on the wire.
- Context estimates include opaque native payloads. Durable compaction clears
  replay state when replacing old assistant exchanges with conversational
  text; removed tool calls must never return through retained native parts.
  The full source message remains unchanged in storage. Text-driver repair
  retains each malformed reply's own state; a repaired response does not
  inherit the preceding response's reasoning. Synthesized calls clear replay.

## Limits and verification

Already-lost legacy part order and signatures cannot be reconstructed.
Lossless replay here applies to retained complete messages, not to compacted
historical tool exchanges. Source-storage round trips are distinct from runtime
transcript persistence: current run settlement stores summaries, and H39 tracks
that unresolved boundary. This change does not claim whole-run restart fidelity.

Required checks include the original failing decoder regressions, both
collectors, SQLite restart, actual orchestration history builders, edited and
legacy messages, overflow accounting, incomplete/cancelled streams and capture
bounds. Full workspace tests, warning-denied Clippy, every local CI gate and all
12 exact-head GitHub checks must pass before acceptance.
