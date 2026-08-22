# ADR-22: Hybrid Retriever Ranking – Reciprocal Rank Fusion (RRF)

**Status:** Accepted
**Date:** 2025-08-06
**Deciders:** Concerto architecture

## Context

The ROADMAP.md Phase 4 RAG pipeline originally specified a weighted‑sum ranking formula:

```
0.7 * vector_score + 0.3 * bm25_score
```

During implementation the code in `crates/memory/src/rag.rs` (line 14) defines a constant `RRF_K = 60.0` and applies reciprocal‑rank fusion (RRF) to combine the vector and BM25 scores.

## Decision

Adopt reciprocal‑rank fusion (RRF) with `k = 60` as the ranking method for the hybrid retriever. The implementation already uses this approach, so no code change is required.

## Rationale

- Vector similarity scores (cosine) and BM25 scores live on different scales; a weighted sum would require explicit score normalisation, which was never added.
- RRF operates on ranks only, avoiding any need to normalise heterogeneous scores.
- The algorithm is simple, well‑understood, and widely used in production RAG systems.
- Using RRF keeps the ranking logic robust and easy to maintain.

## Consequences

- Ranking is ordinal (relative order) rather than cardinal; absolute score values are not meaningful.
- No score normalisation code is needed, reducing complexity.
- Future changes to weighting would require a different approach, but the current RRF setup satisfies the project's needs.

## Alternatives Considered

- **Weighted‑sum formula** – would need score normalisation across vector and BM25 scores, adding complexity and potential bugs.
- **Learning‑to‑rank** – overkill for the current use case and would introduce a training pipeline.

---

*Last updated: 2026-08-22 (retrospective consolidation — see [README](./README.md)).*
