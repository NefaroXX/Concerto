# ADR-10: Vector Store — LanceDB

> **Archived** — superseded by [ADR-63](../ADR-63-memory-subsystem.md) (SQLite
> hybrid vector/FTS store; consolidated 2026-08-22). See
> [docs/adrs/README.md](../README.md) for the current index. Retained verbatim
> as the historical rationale for embedded-vector-storage requirements; not
> active guidance. LanceDB support was removed entirely in pre-release cleanup;
> `SqliteVectorStore` is the only vector store.

**Status:** Superseded by ADR-63 (originally: superseded in implementation by the SQLite vector/FTS store)

## Context

Concerto requires a vector database for storing and querying code chunk
embeddings. The system needs to:

1. **Store embeddings** for code chunks (functions, structs, modules, etc.)
   indexed by project, with associated metadata (file path, chunk type,
   timestamps).
2. **Perform approximate nearest-neighbour (ANN) search** to find semantically
   similar chunks given a query embedding.
3. **Support per-project namespaces** — many projects indexed on the same
   machine, each isolated.
4. **Operate locally** — no external server, no network dependency. The
   system must work fully offline.
5. **Support cancellation** — all operations must accept a
   `CancellationToken` so the system never hangs during shutdown.

Two main candidates were evaluated: **Qdrant** (client-server) and **LanceDB**
(embedded).

## Decision (historical)

Use **LanceDB** (the `lancedb` Rust crate) as the vector store.

### Why LanceDB Over Qdrant

| Criteria | LanceDB | Qdrant |
|---|---|---|
| Architecture | Embedded (same process) | Client-server (separate process) |
| Local-first | ✅ Yes — no server needed | ❌ Requires running `qdrant` sidecar |
| Offline | ✅ Fully offline | ❌ Server must be reachable |
| Per-project isolation | ✅ Directory-based namespaces | ❌ Multi-tenant requires server config |
| Cancellation | ✅ Via `tokio::select!` on query futures | ✅ Client also supports |
| Embedding storage | ✅ Lance columnar format | ✅ Payload + vectors |
| Rust crate maturity | Active, 0.x | Mature, well-documented |
| Build complexity | Pulls `lance` + DataFusion deps | Pulls gRPC + tonic deps |

The deciding factor is **architecture**: Qdrant requires running a separate
server daemon, which violates the local-first, single-binary design goal.
LanceDB is a library — the application opens a database directory and runs
queries in-process. This aligns with the existing pattern of SQLite for
relational data (used by `sessions`, `memory` stores).

### Per-Project Namespace Strategy

Each project gets its own LanceDB database directory under the Concerto
data directory:

```
~/.local/share/concerto/vectors/{project_hash}/
```

This provides:
- **Natural isolation** — no cross-project vector pollution
- **Clean deletion** — removing a project = removing a directory
- **No schema sharing** — different projects can use different embedding
  models (though not simultaneously within one project)

### Cancellation Support

All LanceDB query operations return `Future`s that can be raced against
a `CancellationToken` using `tokio::select!`:

```rust
tokio::select! {
    results = table.search(&query_embedding).limit(n).execute() => {
        // handle results
    }
    _ = cancellation_token.cancelled() => {
        return Err(MemoryError::OperationCancelled);
    }
}
```

## Consequences

- **No server process** — reduces operational complexity and startup latency.
- **Dependency weight** — `lancedb` pulls in `lance` and DataFusion, which
  add compile time. This is acceptable for a dependency that is only used
  by the `concerto-memory` crate.
- **0.x maturity risk** — LanceDB's Rust crate is pre-1.0. Mitigation: the
  `VectorStore` trait abstracts all LanceDB-specific code behind a
  trait boundary. If LanceDB becomes unmaintainable, a new implementation
  can be swapped in without changing any caller.
- **Directory locking** — concurrent access to a project's vector store from
  multiple Concerto processes must be prevented by the existing `fd-lock`
  mechanism (see ADR-11).

## Alternatives Considered

- **Qdrant**: Rejected due to server dependency (violates local-first goal).
- **SQLite + `sqlite-vec`**: The `sqlite-vec` extension is experimental and
  lacks the maturity for production use.
- **In-memory HNSW**: A custom HNSW implementation over `fastembed` vectors
  stored in SQLite. Feasible, but recreating ANN indexing logic is risky.
- **pgvector**: Requires PostgreSQL — violates single-binary and local-first
  goals.
- **Plain SQLite + cosine similarity** (the adopted successor): no extra
  engine dependency at all; exhaustive cosine scan over per-project chunks
  is fast enough at project scale, and FTS5 hybrid retrieval (ADR-63)
  carries recall. This is what Concerto ships today.

---

*Last updated: 2026-08-22 (retrospective consolidation — see [README](../README.md)).*
