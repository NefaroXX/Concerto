# ADR-63: Memory Subsystem — SQLite Hybrid Vector/FTS Retrieval with Local Embeddings

**Status:** Accepted
**Date:** 2026-08-19
**Deciders:** Concerto architecture
**Related crates:** `concerto-memory`, `concerto-core`
**Supersedes:** [ADR-10](./archive/ADR-10.md) (LanceDB — archived; SQLite is
the only vector store). Ranking method per [ADR-22](./ADR-22.md); degradation
handling per [ADR-39](./ADR-39.md); embedding versioning per
[ADR-12](./ADR-12.md).

## Context

Long-term project memory needs semantic + lexical retrieval over a local
codebase corpus: chunk the project's files, embed the chunks, index them, and
answer hybrid queries — fully offline, per-project isolated, cancellable, and
without an external database server or cloud embedding API.

The original vector-store decision (archived ADR-10) chose an embedded
columnar engine. Pre-release cleanup removed that dependency entirely:
cold compile time dropped from ~8 minutes to ~30 seconds, and project-scale
corpora never needed ANN indexing beyond what SQLite provides directly.
The `VectorStore` trait moved to `concerto-core`, so plugins and memory
consumers depend on the abstraction without depending on this crate's storage.

## Decision

The active long-term memory path in `concerto-memory`:

1. **Walk and filter.** Recursive directory walk (`walkdir`/`glob`) over the
   selected project; exclusion patterns (`.git/`, `target/`,
   `node_modules/`, `.concerto-ignore`) are applied at the application layer;
   only supported file types are indexed.
2. **Chunk.** Tree-sitter AST-aware chunking for supported languages (Rust,
   Python, Go, TypeScript); line-based chunks for recognized code/text
   elsewhere; sliding-window chunks for other text.
3. **Embed locally.** BGE-small embeddings through `fastembed` — no network
   calls at query time; first use may download model data, which is surfaced,
   not silent.
4. **Store in SQLite.** Chunks, embeddings (with cosine similarity at query
   time), and FTS5 text live in one SQLite database per data root.
   `SqliteVectorStore` is the **only** vector store; there is no feature gate
   and no alternative backend.
5. **Fuse with RRF.** Vector results and BM25 FTS5 results combine via
   reciprocal-rank fusion (k = 60) — rank-based, so heterogeneous score scales
   never need normalization (ADR-22).
6. **Isolate by project.** Queries are scoped by canonical project identity;
   projects never see each other's chunks.
7. **Refresh live.** A debounced `notify` watcher (ADR-06): 1-second debounce,
   bounded deduplicated queue, rate-limited overflow warning — changed files
   re-index within seconds; deletions tombstone their vectors.

### Embedding lifecycle

- **Versioning (ADR-12).** Every record carries its embedding-model version;
  on mismatch the project's vectors are marked stale and re-indexed lazily,
  with stale results still returned (penalized and flagged) rather than going
  blind.
- **Degradation (ADR-39).** Embed failures never write zero-vector rows:
  affected chunks are recorded for FTS only; per-project health tracks
  consecutive failures with bounded exponential backoff (5 s → 120 s);
  transition-to-broken emits a typed bus event; vector search degrades to
  FTS-only with a user-visible notice until recovery.
- **Self-heal (ADR-54).** A corrupt store file is quarantined as
  `<name>.corrupt-<ts>.bak` and recreated on next open; valid-header files are
  never silently destroyed.
- **Global tier stubbed (ADR-54).** The cross-project global namespace exists
  in the type system but opens no database file in production wiring until
  consumers exist — a missing/unreadable global DB can never abort a run.

### Consumers

`concerto-orchestrator` retrieves context for prompts under the context
budget (ADR-16/48); both frontends expose a Memory explorer; the coordinator's
consolidation path planned by ADR-60 writes episodic summaries back into this
same store as a projection of the whiteboard log — never replacing it.

## Consequences

- Zero external services: memory works offline end-to-end; no vector engine
  dependency keeps builds fast and audit surface small.
- Exhaustive cosine scan over per-project chunk counts is fast enough at
  codebase scale; if corpora grow orders of magnitude, the `VectorStore` trait
  is the swap point.
- RRF means absolute scores are not comparable across queries — ranking is
  ordinal; consumers must not treat fused scores as calibrated probabilities.
- FTS sync rows carry a neutral stored score by design; query-time scoring
  always overwrites it (documented in STATUS known-stubs).
- Re-index storms after large refactors are bounded by the debounced queue;
  watch events during active runs cancel cleanly with the run's token.

## Alternatives Considered

- **Embedded columnar vector engine (archived ADR-10):** removed — dependency
  weight and build cost outweighed ANN capabilities never exercised at
  project scale.
- **Qdrant/pgvector servers:** rejected — violate local-first single-binary
  operation.
- **Cloud embedding APIs:** rejected — offline requirement and privacy;
  local BGE-small quality is sufficient for code retrieval.
- **Tantivy instead of FTS5:** deferred — FTS5 already ships inside the
  existing SQLite substrate; revisit only if measured recall demands it.
- **Weighted-sum hybrid scoring:** rejected in favor of RRF (ADR-22) — no
  score normalization across heterogeneous scales.

## References

- Retrieval/fusion: `crates/memory/src/rag.rs`; indexing:
  `crates/memory/src/indexer.rs`; watcher: `crates/memory/src/watcher.rs`;
  prefs: `crates/memory/src/prefs.rs`
- Trait boundary: `concerto_core::VectorStore` (`crates/core`)
- Related: ADR-06 (filesystem watch), ADR-11 (single-instance locking),
  ADR-12 (embedding versioning), ADR-16/48 (context budget consumption),
  ADR-22 (RRF), ADR-39 (degradation), ADR-54 (stub/global-memory hardening),
  archived ADR-10 (superseded LanceDB decision)

---

*Decision codified from inception 2025-07-10; document stabilized 2026-08-19
(retrospective consolidation — see [README](./README.md)).*
