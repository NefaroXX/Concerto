#![deny(clippy::all)]
#![deny(unused_imports, unused_variables, dead_code)]
#![allow(missing_docs)]

//! `concerto-memory` — memory subsystem.
//!
//! The live implementation is the Phase 4 module set below. The former
//! Phase 3 in-memory / JSON layers (`WorkingMemory`, `PersistentMemory`,
//! `SummarizedMemory`) were removed: the agent-loop summaries and the
//! ADR-60 D6 consolidation projection superseded them, and they had no
//! production callers.
//!
//! Phase 4 modules:
//! - `fts` — `FullTextStore` trait + SQLite FTS5 implementation.
//! - `summarizer` — `LLMSummarizer` trait + `SUMMARIZATION_PROMPT`.
//! - `budget` — `ContextBudgetAllocator`.
//! - `chunk_selector` — `ChunkSelector`.
//! - `indexer`, `embedder`, `watcher`, `rag`, `entities`, `facts`,
//!   `vector_store`, `sync`, `staleness`, `prefs`, `testing`.

/// Re-export core's comprehensive `MemoryError`.
pub use concerto_core::error::MemoryError;

// ---------------------------------------------------------------------------
// Phase 4 module declarations
// ---------------------------------------------------------------------------

pub mod budget;
pub mod chunk_selector;
pub mod decision_store;
pub mod embedder;
pub mod embedder_health;
pub mod entities;
pub mod fts;
pub mod global;
mod ignore_rules;
pub mod indexer;
pub mod links;
pub mod mermaid;
pub mod prefs;
pub mod rag;
pub mod scoring;
pub mod short_term;
pub mod storage;
pub mod summarizer;
pub mod sync;
pub mod system;
pub mod task_tree;
pub mod treesitter;
pub mod ttl;
pub mod vector_store;
pub mod watcher;

#[cfg(test)]
pub mod testing;
