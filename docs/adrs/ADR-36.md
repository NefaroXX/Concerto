# ADR-36: Durable typed session transcript

**Status:** Accepted — complete (implemented, Phase 5 of audit remediation H-06, 2026-08-01)
**Date:** 2026-08-01
**Deciders:** Concerto architecture

## Context

Audit finding H-06 "Restored chat cannot reproduce live orchestration transcript": both text-only and multi-agent modes persist only the top-level user message and the final assistant message (runtime_runner.rs:1480-1491, 1826-1838 in the pre-fix tree). Tool calls, approvals, delegation, errors, summaries and intermediate agent text are visible live (rendered from the EventBus) but vanish on restore. The desktop additionally keeps a LOCAL transcript.json (ChatEntry list) per project+session which is machine-local, not in the sessions DB, and unusable by the CLI. The sessions DB already persists the full event stream (session_events table + SessionReplayer), but events carry only input hashes and detail summaries — not a UI-faithful, correlated sequence (tool-call start/finish are separate events; approval outcomes are separate events; the user message is NOT an event at all). Live rendering logic lives in desktop runtime.rs translate_* functions; the CLI renders live events too.

> **Historical scope:** The Decision and Consequences below describe the
> implemented design as of Phase 5 completion (2026-08-01) — the design that
> shipped, not a proposal. The phased build-out and its verification are
> recorded in the *Phase complete (2026-08-01)* amendment below.

## Decision

Introduce ONE canonical, durable, typed transcript in the sessions DB that both live and restored UIs render from:

### 1. Core model (concerto_core)

`TranscriptEntry` enum in a new `crates/core/src/transcript.rs` (re-exported from lib), derive Debug, Clone, PartialEq, Serialize, Deserialize. Variants:

- `User { content: String }`
- `Assistant { content: String }`
- `Thinking { agent: String, content: String }`
- `ToolCall { tool_name: String, detail: String, status: TranscriptToolStatus }`
- `Activity { agent: String, content: String }`
- `Error { content: String }`
- `Summary { content: String }`
- `Completion { multi_agent: bool, completed: bool, files: Vec<String>, project_root: Option<String> }`
- `TranscriptToolStatus` enum: Running, Completed, Failed, Allowed, Denied, Cancelled.
- A canonical `pub fn from_event(&EventKind, correlation_state) -> Option<TranscriptEntry>` mapping used by the recorder: ToolExecutionStarted→ToolCall{Running}, ToolExecutionFinished→ToolCall{Completed/Failed}, ToolTimeout→ToolCall{Failed}, ApprovalResolved{true/false}→ToolCall{Allowed/Denied}, ApprovalTimeout→ToolCall{Cancelled}, AgentThought→Thinking, AssistantMessage→Assistant, SubTask*/DelegationDecided/AgentHandoff/ReviewCycle*/ValidationCycle*/RoutingDecided/MultiAgentMode*/BudgetDowngradeTriggered/OrchestratorCycleDetected/TaskStarted/TaskCompleted/TaskFailed/AgentStateChanged/ProviderRetry*→Activity (agent label + human sentence mirroring the desktop activity() strings in crates/desktop/src/runtime.rs), ErrorOccurred→Error, SummarizationStarted/Completed→Summary. Noise events (TokenUsed, CostIncurred, PolicyVerdict, SpendUpdated, Indexing*, ShellOutputChunk, SessionSaved, Eval*, Undo*, Memory*, ContextWindowApproaching, CycleBudgetExceeded, SpendCap*, RateLimitEnforced, ProviderCallCompleted, AutoUpdateAvailable, Observability*, Lsp*, OpenAPIDocGenerated, SandboxProfileActivated, EntityExtracted, FactExtracted, FactExpired, StaleVectorsDetected, ReindexQueued, EmbeddingModelMismatch, MemoryConflict) → None.

### 2. One tool-call entry per invocation

ToolExecutionStarted pushes a Running entry; ToolExecutionFinished/Timeout/Approval events UPDATE the last Running entry with the same tool_name instead of pushing a new one (mirrors desktop chat.rs add_tool_call/update_tool_call correlation; Running entries with no terminal event are settled as Cancelled at run end).

### 3. Persistence (concerto_sessions)

New migration `020_transcript_entries.sql` — table `transcript_entries(id TEXT PK, session_id TEXT NOT NULL REFERENCES sessions(id), sequence_num INTEGER NOT NULL, entry TEXT NOT NULL /*JSON*/, created_at INTEGER NOT NULL)` + unique index on (session_id, sequence_num). SessionStore trait gains `append_transcript(session_id, &[TranscriptEntry], cancel) -> Result<(), SessionError>` and `load_transcript(session_id, cancel) -> Result<Vec<TranscriptEntry>, SessionError>` (ordered by sequence_num). Implement in SqliteSessionStore; add round-trip tests. The messages table, session_events table, and all existing SessionStore methods remain unchanged (backward compatibility; messages still serve model-context reconstruction).

### 4. Recorder (concerto_orchestrator runtime_runner.rs)

`start_transcript_recorder(bus, store, session_id)` alongside the existing start_event_recorder — subscribes to the durable bus, converts each event via the core mapping with correlation state, appends entries in small batches (flush at stop). Called in create_session_and_recorder (BOTH modes — text-only currently does not persist the user message; the transcript fixes that). At run start, runtime_runner appends a User entry with req.input; at run end (both modes), appends the final Assistant entry (output.final_message) and a Completion entry (multi_agent, completed, files_modified, project_root). This replaces the current "persist only user + final assistant" append_messages calls for UI purposes (message-table writes may stay for context reconstruction, or be removed if unused — fixer decides with evidence).

### 5. Restored rendering

- Desktop: `select_session` first tries `load_transcript`; converts TranscriptEntry → views::chat::ChatEntry (ids assigned by State::from_entries; Running tool calls cancelled by from_entries as today). Falls back to the local transcript.json / messages_to_entries ONLY when the DB transcript is empty (legacy sessions).
- CLI: `restore_active_session` loads `load_transcript` and renders typed lines (user, assistant, tool calls with status, activity, errors, completion summary); falls back to load_recent_messages for empty transcripts.
- Live UI keeps rendering from the live event stream; the recorder derives the transcript from that same stream with equivalent semantics — the parity test below proves restored == live.

### 6. Proof bar tests

- sessions: transcript append/load round-trip, ordering, empty load.
- orchestrator: scripted sequence test — publish a fixed event sequence (user, AgentThought, ToolExecutionStarted, ToolExecutionFinished, ApprovalResolved, SubTaskCreated, ErrorOccurred) through a bus with recorder; assert load_transcript equals the expected typed sequence (including tool-call correlation merging and approval status).
- desktop: restore-from-transcript test — build ChatEntry list from a scripted TranscriptEntry vec; assert it equals the live-built ChatEntry list for the same scripted event sequence (this is the "live and restored render the same sequence" proof).
- parity test: both frontends load identical transcript entries from the same store.

## Consequences

- Restored sessions (desktop + CLI) reproduce the live orchestration transcript: user prompt, all assistant text, thinking, correlated tool calls with approval outcomes, delegation activity, errors, summaries, completion summary.
- Text-only mode gains durable user-message persistence (previously missing).
- New table + 2 trait methods; messages/events tables untouched; old sessions render via fallback.
- Transcript is UI-faithful but NOT a full payload record: tool args/results remain hash+detail only (full payloads stay in the audit log); a future richer record is a separate ADR.
- Slight write amplification: one small INSERT per transcript-relevant event (batched).

## Phase complete (2026-08-01)

This section records the completion of the H-06 remediation phase (stages 1-3
of the build-out below). It **amends** — not supersedes — this ADR: every
Decision above stands as implemented.

| Commit | Stage |
|---|---|
| `f10b530` | 1 — core model + migration 020 + `append_transcript`/`load_transcript`: `TranscriptEntry`/`TranscriptToolStatus` in concerto_core, `transcript_entries` table, `SessionStore` trait methods implemented in `SqliteSessionStore` (messages/events tables untouched) |
| `0b28d26` | 2 — recorder: `start_transcript_recorder` with tool-call correlation (terminal/approval events update the last Running entry with the same tool name), settle-on-stop (Running → Cancelled), and user/final/completion entries in both modes — including the text-only user-message persistence fix |
| `768162e` | 3 — desktop + CLI restore with fallback priority: DB transcript > local transcript.json > messages |

Gate after Phase 5: fmt, clippy `-D warnings`, nextest 1735/1735, cargo-deny
all green.

**Accepted divergences** (canonical restored forms, intentionally different
from the live rendering; asserted by `restored_approval_activity_and_summary_renderings_are_documented`):

- `SubTaskCreated` Activity wording differs from the live graph view: the
  recorder persists a canonical sentence ("Decomposed task T1 into specialist
  subtask: …") while the live graph view uses the "[Coordinator → coder]"-style
  wording.
- Approval outcomes render as a canonical single `ToolCall` status
  (Allowed/Denied/Cancelled) on restore, where the live UI pushes a separate
  Running entry before the approval resolves.
- Assistant entries restore with `streaming: false` (streaming flags are not
  persisted).
- Running tool calls settle to `Cancelled` at run end (no terminal event), in
  both the persisted transcript and restored views.

## Requirements checklist (verification bar for this phase — all met 2026-08-01)

1. Source-level invariant: one TranscriptEntry model in core; recorder and restore paths use it. ✓ — `transcript_to_entries_maps_all_variants` (desktop) and `transcript_lines_renders_all_entry_variants` (CLI) exercise the full variant surface on both restore paths.
2. Integration tests at owning boundaries (sessions, orchestrator, desktop). ✓ — `transcript_round_trip_preserves_order` (sessions), `transcript_recorder_correlates_tool_calls_into_single_entries` (orchestrator), `restored_transcript_matches_live_rendering` (desktop), `both_frontends_load_identical_history` (parity).
3. Both-modes coverage (text-only and multi-agent) for user/assistant/completion entries. ✓ — recorder wiring tests in both dispatch paths; parity test `both_frontends_load_identical_history`.
4. Correlation correctness (tool call merging, approval statuses, settle-on-stop). ✓ — `transcript_recorder_correlates_tool_calls_into_single_entries`.
5. Backward compatibility (messages/events tables untouched; fallback paths). ✓ — `append_transcript`/`load_transcript` are additive on `SessionStore`; legacy sessions fall back to transcript.json / messages on both frontends.
6. Green full workspace gate: fmt, clippy `-D warnings`, nextest 1735/1735, cargo deny. ✓
