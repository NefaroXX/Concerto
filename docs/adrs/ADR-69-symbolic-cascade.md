# ADR-69: Symbolic cascade — link store, scoring, and observability in slices

**Status:** Accepted

Composes with [ADR-65](./ADR-65-evidence-spine.md) (facts on the
append-only chain), [ADR-63](./ADR-63-memory-subsystem.md) (SQLite hybrid
vector/FTS retrieval), and [ADR-60](./ADR-60-concurrent-agent-runtime.md)
(concurrent agent runtime). Supersedes: none.

**Date:** 2026-09-18

**Deciders:** Concerto architecture + maintainer direction

## Context

Concerto's memory subsystem (ADR-63) retrieves chunks by vector similarity
and BM25 lexical rank. There is no mechanism to propagate relevance scores
across related chunks — a high-relevance chunk that references another
chunk's content does not boost the referenced chunk's rank. This matters for:

- **Multi-hop reasoning.** A fact established in chunk A that supports
  chunk B's relevance should lift B's score.
- **Decay propagation.** Freshness signals should flow through links so
  that a recently-updated chunk boosts its linked neighbors.
- **Observability.** The current black-box ranker makes it hard to explain
  why a chunk ranked where it did.

The proposal is a symbolic link store with scoring, decay, and Mermaid
visualization — implemented in three slices to manage risk and validate
cost/latency before proceeding.

## Decision

### Slice 1: Link store + write path (M, 3-5 days)

A lightweight link store backed by SQLite stores directed, typed edges
between memory chunks:

```sql
CREATE TABLE memory_links (
    source_id TEXT NOT NULL,
    target_id TEXT NOT NULL,
    link_type TEXT NOT NULL,  -- 'references', 'supports', 'contradicts', 'extends'
    weight REAL NOT NULL DEFAULT 1.0,
    created_at TEXT NOT NULL,
    expires_at TEXT,          -- TTL-based decay
    PRIMARY KEY (source_id, target_id, link_type)
);
```

**Write path:** links are created as a side effect of chunk indexing —
when a new chunk is indexed, the indexer optionally emits links to
existing chunks (manual or heuristic). The write path is append-only
(link creation is idempotent; re-indexing does not duplicate links).

**Caps:**
- **Degree cap:** per-chunk out-degree capped at 32 (configurable);
  excess links are rejected with a warning.
- **TTL:** links default to 30-day TTL; re-indexing refreshes the TTL.
- **Fail-open:** link store errors do not block chunk indexing or
  retrieval; they are logged as warnings.

### Slice 2: Scoring + decay (M, 5-8 days)

Retrieval rank is augmented by a link-score component:

```
final_score = α * vector_score + β * bm25_score + γ * link_score
```

where `link_score` is derived from:
- **In-degree** of the target (how many chunks reference it).
- **Link weight** (typed: `supports` > `references` > `extends` > `contradicts`).
- **Decay:** `weight * exp(-λ * age)` where `λ` is configurable per link type.

**Caps on scoring:**
- `γ` (link-score weight) is capped at 0.2 — links augment, never dominate.
- Decay floor: links older than 90 days contribute 0 to scoring (but are
  not deleted).
- Circular link detection: simple BFS with degree cap prevents infinite
  propagation.

**Gating:** Slice 2 starts only after Slice 1 measurements confirm
acceptable cost/latency (link writes < 5ms p99, retrieval overhead < 10ms
p99 on a 10k-chunk index).

### Slice 3: Mermaid + UI + multi-hop eval (S-M, 3-5 days)

- **Mermaid rendering:** a `render_links_mermaid(chunk_id)` function
  produces a Mermaid diagram of the chunk's local link neighborhood.
  **Caps:** max 50 nodes, max 100 edges in the diagram; beyond that,
  truncate with an ellipsis node.
- **Desktop integration:** a "Show Links" action in the memory browser
  renders the Mermaid diagram inline.
- **Multi-hop evaluation:** a test harness measures whether link-scored
  retrieval improves multi-hop question answering over a baseline
  (no links). Results are recorded in `docs/` as a benchmark report.

**Gating:** Slice 3 starts only after Slice 2 is deployed and link-score
improvements are measured.

## Consequences

- **Gradual rollout.** Three slices with measurement gates prevent
  premature investment in scoring before the store is validated.
- **Fail-open.** Link store failures never block core retrieval; the
  system degrades to the current vector+BM25 baseline.
- **Observable.** Mermaid diagrams and benchmark reports make the link
  system inspectable.
- **Bounded.** Degree caps, TTL, decay floor, and scoring caps prevent
  unbounded growth and runaway propagation.
- **Cost-gated.** Slices 2 and 3 require measured cost/latency validation
  before proceeding — no speculative investment.

## Acceptance criteria

### Slice 1
- **A1** — `memory_links` table exists; link writes are idempotent.
- **A2** — Degree cap enforced; excess links rejected with warning.
- **A3** — TTL refreshes on re-index; fail-open on link store errors.

### Slice 2
- **A4** — Retrieval rank includes link-score component with configurable
  weights.
- **A5** — Decay applied; 90-day floor; circular detection prevents
  infinite propagation.
- **A6** — `γ` capped at 0.2; link score never dominates.

### Slice 3
- **A7** — Mermaid rendering with node/edge caps.
- **A8** — Desktop "Show Links" action renders diagram inline.
- **A9** — Multi-hop benchmark report comparing link-scored vs. baseline.

---

*Last updated: 2026-09-18*
