# ADR-11: Multi-Instance File Locking — `fd-lock`

**Status:** Accepted
**Date:** 2025-07-21
**Deciders:** Concerto architecture

> **Current implementation:** Persistent session/memory state and active vector
> retrieval are SQLite-backed (FTS5 + vector hybrid). LanceDB is an optional,
> feature-gated alternative (`lancedb` feature, default off) — see ADR-10 — so
> the concurrent-LanceDB-writer concerns below apply only when that feature is
> enabled. The `fd-lock`/multi-instance decision remains relevant to local
> persistent files.

## Context

Concerto stores persistent data in several files: SQLite databases for
sessions, memory, entity relationships, and user preferences; LanceDB
directories for vector embeddings; and JSON or TOML files for configuration.

When multiple Concerto processes (or a single process with concurrent
async tasks) access the same data directory, two problems arise:

1. **SQLite concurrent writes** — SQLite's WAL mode allows concurrent reads
   but serializes writes. Without external coordination, two Concerto
   instances writing to the same project's memory database can corrupt data
   or cause `SQLITE_BUSY` errors.
2. **LanceDB concurrent access** — LanceDB's columnar format does not support
   concurrent writers to the same table. Two processes indexing the same
   project simultaneously could corrupt the index.

The system needs a lightweight, cross-platform file locking mechanism that
prevents concurrent writes to the same data directory.

## Decision

Use **`fd-lock`** (the `fd-lock` crate) for advisory file locking on the
Concerto data directory, combined with **SQLite WAL mode** for safe
concurrent reads.

### How It Works

```rust
use fd_lock::RwLock;
use std::fs::File;

let file = File::create(data_dir.join(".concerto.lock"))?;
let lock = RwLock::new(file);
// Acquire write lock before any write operation
let write_guard = lock.write()?;
// ... perform writes ...
drop(write_guard); // releases the lock
```

- **Write lock** — acquired before any mutation (SQLite writes, LanceDB
  writes, preference file writes). Other instances block or time out.
- **Read lock** — acquired for read-only operations. Multiple readers are
  allowed concurrently.

### Locking Scope

A single `.concerto.lock` file in the Concerto data root protects the
entire data directory. This is coarse but correct: the most expensive
operation is indexing (which only runs in one process), and the common
case is a single running instance.

### SQLite WAL Mode

All SQLite databases (`sessions.db`, `memory.db`, `prefs.db`, etc.) use
PRAGMA `journal_mode=WAL`. This allows concurrent readers without blocking,
even when a writer holds the lock. The `fd-lock` write lock serializes
writers, preventing `SQLITE_BUSY` errors across processes.

## Consequences

- **Lightweight** — `fd-lock` has no system dependencies (no `libsqlite3`,
  no external lock server). It uses platform-native `flock` / `LockFileEx`.
- **Coarse granularity** — One lock file for the entire data directory.
  Finer-grained locking (per-database, per-project) can be added later if
  contention becomes an issue.
- **No daemon required** — Unlike `docker` or `postgres` locking strategies
  that rely on a coordinating process, `fd-lock` is purely filesystem-based.
- **Stale lock handling** — If a process crashes while holding a write lock,
  the OS automatically releases the `flock`. No manual cleanup needed.

## Alternatives Considered

- **SQLite `SQLITE_BUSY` retry loop**: Simple but fragile — a long-running
  write (e.g., indexing) causes other instances to spin or fail. Rejected.
- **Single-instance mutex (named semaphore)**: Platform-specific (pthreads
  on Linux, named kernel object on Windows). Less portable than `fd-lock`.
- **No locking (rely on user discipline)**: Not acceptable — data corruption
  is a hard failure mode.

---

*Last updated: 2026-08-22 (retrospective consolidation — see [README](./README.md)).*
