# ADR-16: Context Overflow Strategy — Tiered Budget with LLM Summarization

**Status:** Accepted (Updated for Phase 4)
**Date:** 2025-07-15
**Deciders:** Concerto architecture

## Context

The agent loop accumulates conversation history (system prompt, user
messages, tool calls, observations) across iterations. Without a bound,
this history grows until it exceeds the model's context window, causing
either truncation (losing recent context) or a hard error.

Phase 3 used a simple threshold-triggered `SummarizeOldest` strategy:
when the working buffer exceeds a count threshold, call an LLM to
summarize the oldest entries into a single compressed entry.

Phase 4 introduces a layered memory system with multiple competing
consumers for the context budget. The overflow strategy must account
for:

1. **RAG retrieval results** — chunks fetched from the vector store for
   the current query.
2. **Working memory** — recent conversation turns and tool results.
3. **Summarized history** — compressed representations of older turns.
4. **System prompt overhead** — tool definitions, policy rules, identity.

## Decision

Use a **tiered budget allocation** with **LLM-based summarization** as
the overflow mechanism, governed by a `ContextBudgetAllocator`.

### Budget Allocation

```
┌──────────────────────────────────────────────┐
│           Total Context Budget               │  (e.g., 128K tokens)
├────────────┬──────────┬──────────────────────┤
│  RAG Pool  │ Working  │  Conversation Pool   │
│  (25%)     │ (10%)    │  (65%)               │
│            │          │                      │
│ Vector     │ Decision │  System prompt       │
│ chunks     │ history  │  + user messages     │
│ Entity     │ Task     │  + tool calls        │
| facts      │ tree     │  + observations      │
│            │          │  + summaries          │
└────────────┴──────────┴──────────────────────┘
```

The allocator reserves fixed percentages per pool. When any pool
overflows, that pool's items are evicted using pool-specific strategies:

| Pool | Overflow Strategy | Eviction Order |
|---|---|---|
| RAG | Drop lowest-score chunks | Sort by relevance score; keep top N |
| Working | Summarize oldest entries | Compress 3+ entries into 1 summary |
| Conversation | Summarize oldest turns | Compress oldest user+assistant pairs |

### Overflow Triggers

Overflow is checked **before every LLM completion call** in the agent
loop. The system estimates the token count of each pool's contents
using a model-specific tokenizer (or a conservative character-count
approximation when the exact tokenizer is unavailable).

### Summarization Pipeline

When the working or conversation pool exceeds 85% of its allocated
budget, a summarization pass is triggered:

1. **Select candidates** — oldest N entries that together exceed the
   overage.
2. **Call LLM** — use the `LlmProvider` (same model as the agent, or a
   faster model if configured) to produce a concise summary.
3. **Replace** — remove the original entries and insert the summary
   at the same position.
4. **Store in persistent memory** — the summary is also written to the
   `PersistentMemory` layer so it survives session restarts.

### Model-Specific Capacity Constants

Each model declares its maximum context window via `LlmProvider::context_window()`:

```rust
pub trait LlmProvider: Send + Sync {
    fn context_window(&self) -> usize;
    // ...
}
```

The `ContextBudgetAllocator` reads this value and computes pool budgets
at runtime. This means:
- **GPT-4 (8K)** → RAG: 2K, Working: 0.8K, Conversation: 5.2K
- **Claude 3.5 Sonnet (200K)** → RAG: 50K, Working: 20K, Conversation: 130K
- **Local CodeLlama (32K)** → RAG: 8K, Working: 3.2K, Conversation: 20.8K

### Phase 4 Integration

The `MemorySystem` wraps this strategy:

```rust
impl MemoryStore for MemorySystem {
    async fn retrieve(&self, query: &MemoryQuery) -> Result<Vec<MemoryChunk>, Error> {
        // Returns results with budget-constrained top_k
        // (capped by RAG pool size)
    }

    async fn store(&self, entry: MemoryEntry) -> Result<MemoryId, Error> {
        // Writes via ChunkSyncService to both vector + FTS
        // Triggers overflow check if working pool exceeds 85%
    }
}
```

## Consequences

- **Predictable context usage** — each pool has a hard ceiling, preventing
  any single component from starving others.
- **Graceful degradation** — when RAG pool is full, only the most relevant
  chunks are retained; low-scoring chunks are dropped first.
- **LLM summarization cost** — summarization calls consume tokens and time.
  Mitigation: summarization is triggered infrequently (only on overflow)
  and uses a configurable model (defaults to the agent model).
- **Fractional budgets are a heuristic** — 25/10/65 split is a default.
  Users can override via preferences (see `UserPrefsStore`, ADR-11). The
  split may need adjustment based on real-world usage patterns.
- **Overflow check overhead** — token counting before every LLM call adds
  latency. Mitigation: use a fast approximate counter (character count / 4)
  and only run exact tokenization when the estimate exceeds 80% of budget.

## Phase 3 → Phase 4 Changes

| Aspect | Phase 3 | Phase 4 |
|---|---|---|
| Overflow trigger | Simple entry count threshold | Budget-aware token estimation |
| Eviction strategy | `SummarizeOldest` only | Tiered per-pool strategies |
| Memory integration | Standalone | Integrated via `MemorySystem` |
| Persistence | JSON file | Vector store + FTS + SQLite |
| Model awareness | Fixed constant | Dynamic via `context_window()` |

---

*Last updated: 2026-08-22 (retrospective consolidation — see [README](./README.md)).*
