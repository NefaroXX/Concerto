# ADR-12: Embedding Versioning

**Status:** Accepted
**Date:** 2025-08-04
**Deciders:** Concerto architecture

## Context

Concerto generates embeddings for code chunks using a local embedding
model (via `fastembed`). As embedding models improve, the system needs a
strategy to handle model upgrades without silently returning semantically
incompatible results.

The key problem: embeddings from different models occupy different vector
spaces. Querying a space with embeddings produced by model A against
vectors produced by model B yields semantically meaningless results.

Additionally, the system must:
1. **Detect when the embedding model changes** across sessions.
2. **Avoid returning stale embeddings** from an old model after upgrade.
3. **Re-index efficiently** — only chunks that need new embeddings.

## Decision

Store an **embedding model version** alongside every vector in the vector
store. On startup, compare the current model's version against the stored
version. If they differ, mark all existing vectors as stale and re-index
the project lazily (on first query or via a background task).

### Embedding Version

Each embedding model is identified by a short string:

- `fastembed/BAAI-bge-small-en-v1.5` → `"bge-small-1.5"`
- `fastembed/intfloat-multilingual-e5-small` → `"mxe5-small"`

The version string is stored:
1. **Per-vector** — as a column in the LanceDB table or as a field on the
   `EmbeddingRecord` struct.
2. **Per-project** — as a metadata key in the project's memory database
   (`current_embedding_model`).

### Upgrade Flow

```
Startup → read current_embedding_model from project metadata
         →
         Compare with configured model:
           │
           ├─ Match → normal operation (no re-index needed)
           │
           └─ Mismatch →
               1. Set stale flag on all existing vectors in this project
               2. Update current_embedding_model to new version
               3. Re-index on next idle cycle (or eagerly if the user
                  triggers "reindex")
               4. During re-index: skip chunks whose embedding version
                  already matches the new model
```

### Query Behaviour During Re-Index

Until re-index completes, the system:
- **Still returns stale results**, but with a `score` penalty applied
  (`score * 0.5` for stale vectors) to deprioritise them.
- **Includes a `stale` flag** in the result so UI consumers can indicate
  "this result may be from a previous embedding model."
- **Prioritises newly-indexed chunks** — results with the current model
  version sort above stale results at equal similarity.

### EmbeddingRecord Schema

```rust
pub struct EmbeddingRecord {
    pub id: MemoryId,
    pub chunk_id: String,
    pub project_id: ProjectId,
    pub vector: Vec<f32>,
    pub model_version: String,   // ← "bge-small-1.5"
    pub created_at: OffsetDateTime,
    pub stale: bool,             // ← true if model has changed
}
```

## Consequences

- **Backward-compatible queries** — stale results are still returned, so
  the system never goes completely blind during re-indexing.
- **Incremental re-index** — only chunks with a mismatched version string
  need regeneration. Large projects re-index gradually without a full
  downtime window.
- **Storage overhead** — one extra string column (model version) and one
  boolean column (stale) per vector. Negligible in practice.
- **Model migration test burden** — every model upgrade must be tested to
  ensure the new model produces vectors of the same dimension (or the
  vector store schema must handle dimension changes, which LanceDB does
  via table versioning).

## Alternatives Considered

- **Delete all vectors on model change**: Simple but causes a blind window
  (no results at all until full re-index). Rejected for being user-hostile.
- **Ignore version mismatches (return incompatible vectors)**: Silent
  semantic degradation — the worst outcome because it looks correct but
  is not. Rejected.
- **Per-query re-embedding**: Re-embed every chunk before each query.
  Prohibitively expensive for large projects. Rejected.

---

*Last updated: 2026-08-22 (retrospective consolidation — see [README](./README.md)).*
