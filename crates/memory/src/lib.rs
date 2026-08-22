#![deny(clippy::all)]
#![deny(unused_imports, unused_variables, dead_code)]
#![allow(missing_docs)]

//! `concerto-memory` — Phase 3 / Phase 4 memory subsystem.
//!
//! Three layers (Phase 3):
//! - **WorkingMemory**: In-memory FIFO buffer (last N entries), no persistence.
//! - **PersistentMemory**: JSON-file-backed memory with thread-safe read/write.
//! - **SummarizedMemory**: Wraps Working + Persistent; summarizes when working
//!   buffer exceeds a threshold.
//!
//! Phase 4 additions (new modules):
//! - `fts` — `FullTextStore` trait + SQLite FTS5 implementation.
//! - `summarizer` — `LLMSummarizer` trait + `SUMMARIZATION_PROMPT`.
//! - `budget` — `ContextBudgetAllocator`.
//! - `chunk_selector` — `ChunkSelector`.
//! - `indexer`, `embedder`, `watcher`, `rag`, `entities`, `facts`,
//!   `vector_store`, `sync`, `staleness`, `prefs`, `testing`.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Mutex;

use concerto_core::ids::Ulid;
use concerto_core::memory::{
    ChunkType, MemoryEntry, MemoryFilter, MemoryId, MemoryNamespace, MemoryQuery, ProjectId,
};
use time::OffsetDateTime;

/// Re-export core's comprehensive MemoryError.
pub use concerto_core::error::MemoryError;

// ---------------------------------------------------------------------------
// MemoryStore trait
// ---------------------------------------------------------------------------

/// Core storage trait implemented by all memory layers.
pub trait MemoryStore {
    fn insert(&mut self, entry: MemoryEntry) -> Result<MemoryId, MemoryError>;
    fn search(&self, query: &MemoryQuery) -> Result<Vec<MemoryEntry>, MemoryError>;
    fn remove(&mut self, id: MemoryId) -> Result<(), MemoryError>;
    fn clear(&mut self) -> Result<(), MemoryError>;
}

// ---------------------------------------------------------------------------
// MemoryContentSummarizer — Phase 3 sync summarizer (to be replaced by
// summarizer::LLMSummarizer in Phase 4 §3.4)
// ---------------------------------------------------------------------------

pub trait MemoryContentSummarizer: Send + Sync {
    fn summarize(&self, entries: &[MemoryEntry]) -> Result<String, MemoryError>;
}

// ---------------------------------------------------------------------------
// WorkingMemory — in-memory FIFO buffer
// ---------------------------------------------------------------------------

pub struct WorkingMemory {
    entries: VecDeque<MemoryEntry>,
    capacity: usize,
}

impl WorkingMemory {
    pub fn new(capacity: usize) -> Self {
        Self { entries: VecDeque::with_capacity(capacity), capacity }
    }
}

impl Default for WorkingMemory {
    fn default() -> Self {
        Self::new(50)
    }
}

impl MemoryStore for WorkingMemory {
    fn insert(&mut self, entry: MemoryEntry) -> Result<MemoryId, MemoryError> {
        let id = entry.id;
        if self.entries.len() >= self.capacity {
            self.entries.pop_front();
        }
        self.entries.push_back(entry);
        Ok(id)
    }

    fn search(&self, query: &MemoryQuery) -> Result<Vec<MemoryEntry>, MemoryError> {
        let query_lower = query.text.to_lowercase();
        let now = OffsetDateTime::now_utc();
        let mut results: Vec<MemoryEntry> = self
            .entries
            .iter()
            .filter(|e| {
                // Text match
                if !e.content.to_lowercase().contains(&query_lower) {
                    return false;
                }
                // Apply filters
                for filter in &query.filters {
                    match filter {
                        MemoryFilter::ChunkType(t) if *t != e.chunk_type => return false,
                        MemoryFilter::ExcludeStale => {
                            if let Some(expires) = e.expires_at {
                                if expires < now {
                                    return false;
                                }
                            }
                        }
                        // FileGlob and MinScore are not applicable to Phase 3
                        _ => {}
                    }
                }
                true
            })
            .cloned()
            .collect();
        results.truncate(query.top_k);
        Ok(results)
    }

    fn remove(&mut self, id: MemoryId) -> Result<(), MemoryError> {
        let idx = self
            .entries
            .iter()
            .position(|e| e.id == id)
            .ok_or_else(|| MemoryError::NotFound(id.to_string()))?;
        self.entries.remove(idx);
        Ok(())
    }

    fn clear(&mut self) -> Result<(), MemoryError> {
        self.entries.clear();
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// PersistentMemory — JSON-file-backed with a Mutex for thread safety
// ---------------------------------------------------------------------------

#[derive(serde::Serialize, serde::Deserialize)]
struct PersistentStore {
    entries: Vec<MemoryEntry>,
}

pub struct PersistentMemory {
    path: PathBuf,
    inner: Mutex<PersistentStore>,
}

impl PersistentMemory {
    pub fn new(path: impl Into<PathBuf>) -> Result<Self, MemoryError> {
        let path = path.into();
        let store = if path.exists() {
            let data = std::fs::read_to_string(&path)
                .map_err(|e| MemoryError::Persistence(e.to_string()))?;
            serde_json::from_str(&data).map_err(|e| MemoryError::Serialization(e.to_string()))?
        } else {
            PersistentStore { entries: Vec::new() }
        };
        Ok(Self { path, inner: Mutex::new(store) })
    }

    fn save(&self, store: &PersistentStore) -> Result<(), MemoryError> {
        let data = serde_json::to_string_pretty(store)
            .map_err(|e| MemoryError::Serialization(e.to_string()))?;
        std::fs::write(&self.path, data).map_err(|e| MemoryError::Persistence(e.to_string()))?;
        Ok(())
    }
}

impl MemoryStore for PersistentMemory {
    fn insert(&mut self, entry: MemoryEntry) -> Result<MemoryId, MemoryError> {
        let id = entry.id;
        let mut store = self
            .inner
            .lock()
            .map_err(|_| MemoryError::Persistence("persistent memory lock poisoned".into()))?;
        store.entries.push(entry);
        self.save(&store)?;
        Ok(id)
    }

    fn search(&self, query: &MemoryQuery) -> Result<Vec<MemoryEntry>, MemoryError> {
        let query_lower = query.text.to_lowercase();
        let now = OffsetDateTime::now_utc();
        let store = self
            .inner
            .lock()
            .map_err(|_| MemoryError::Persistence("persistent memory lock poisoned".into()))?;
        let mut results: Vec<MemoryEntry> = store
            .entries
            .iter()
            .filter(|e| {
                if !e.content.to_lowercase().contains(&query_lower) {
                    return false;
                }
                for filter in &query.filters {
                    match filter {
                        MemoryFilter::ChunkType(ct) if *ct != e.chunk_type => return false,
                        MemoryFilter::ExcludeStale => {
                            if let Some(expires) = e.expires_at {
                                if expires < now {
                                    return false;
                                }
                            }
                        }
                        _ => {}
                    }
                }
                true
            })
            .cloned()
            .collect();
        results.truncate(query.top_k);
        Ok(results)
    }

    fn remove(&mut self, id: MemoryId) -> Result<(), MemoryError> {
        let mut store = self
            .inner
            .lock()
            .map_err(|_| MemoryError::Persistence("persistent memory lock poisoned".into()))?;
        let len_before = store.entries.len();
        store.entries.retain(|e| e.id != id);
        if store.entries.len() == len_before {
            return Err(MemoryError::NotFound(id.to_string()));
        }
        self.save(&store)?;
        Ok(())
    }

    fn clear(&mut self) -> Result<(), MemoryError> {
        let mut store = self
            .inner
            .lock()
            .map_err(|_| MemoryError::Persistence("persistent memory lock poisoned".into()))?;
        store.entries.clear();
        self.save(&store)?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// SummarizedMemory — wraps Working + Persistent, summarizes at threshold
// ---------------------------------------------------------------------------

pub struct SummarizedMemory {
    working: WorkingMemory,
    persistent: PersistentMemory,
    summarizer: Box<dyn MemoryContentSummarizer>,
    threshold: usize,
    project_id: ProjectId,
    namespace: MemoryNamespace,
}

impl SummarizedMemory {
    pub fn new(
        persistent: PersistentMemory,
        summarizer: Box<dyn MemoryContentSummarizer>,
        capacity: usize,
        threshold: usize,
        project_id: ProjectId,
        namespace: MemoryNamespace,
    ) -> Self {
        Self {
            working: WorkingMemory::new(capacity),
            persistent,
            summarizer,
            threshold,
            project_id,
            namespace,
        }
    }

    fn maybe_summarize(&mut self) -> Result<(), MemoryError> {
        if self.working.entries.len() < self.threshold {
            return Ok(());
        }
        let entries: Vec<MemoryEntry> = self.working.entries.drain(..).collect();
        let summary = self.summarizer.summarize(&entries)?;
        let summary_entry = MemoryEntry {
            id: MemoryId(Ulid::new()),
            project_id: self.project_id.clone(),
            namespace: self.namespace.clone(),
            content: summary,
            chunk_type: ChunkType::SessionSummary,
            model_id: None,
            model_version: None,
            metadata: serde_json::json!({"type": "summary", "source_entries": entries.len()}),
            expires_at: None,
            created_at: OffsetDateTime::now_utc(),
        };
        self.persistent.insert(summary_entry)?;
        Ok(())
    }
}

impl MemoryStore for SummarizedMemory {
    fn insert(&mut self, entry: MemoryEntry) -> Result<MemoryId, MemoryError> {
        let id = self.working.insert(entry)?;
        self.maybe_summarize()?;
        Ok(id)
    }

    fn search(&self, query: &MemoryQuery) -> Result<Vec<MemoryEntry>, MemoryError> {
        let mut results = self.working.search(query)?;
        let persistent_results = self.persistent.search(query)?;
        results.extend(persistent_results);
        results.truncate(query.top_k);
        Ok(results)
    }

    fn remove(&mut self, id: MemoryId) -> Result<(), MemoryError> {
        if self.working.remove(id).is_err() {
            self.persistent.remove(id)?;
        }
        Ok(())
    }

    fn clear(&mut self) -> Result<(), MemoryError> {
        self.working.clear()?;
        self.persistent.clear()?;
        Ok(())
    }
}

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
pub mod prefs;
pub mod rag;
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    struct TestSummarizer;
    impl MemoryContentSummarizer for TestSummarizer {
        fn summarize(&self, entries: &[MemoryEntry]) -> Result<String, MemoryError> {
            let count = entries.len();
            Ok(format!("Summarized {count} entries"))
        }
    }

    fn default_project() -> ProjectId {
        ProjectId("test-project-hash".into())
    }

    fn default_namespace() -> MemoryNamespace {
        MemoryNamespace::Project(default_project())
    }

    fn make_entry(content: &str) -> MemoryEntry {
        MemoryEntry {
            id: MemoryId(Ulid::new()),
            project_id: default_project(),
            namespace: default_namespace(),
            content: content.to_string(),
            chunk_type: ChunkType::SlidingWindow,
            model_id: None,
            model_version: None,
            metadata: serde_json::json!({}),
            expires_at: None,
            created_at: OffsetDateTime::now_utc(),
        }
    }

    fn make_query(text: &str, top_k: usize) -> MemoryQuery {
        MemoryQuery {
            text: text.to_string(),
            project_id: default_project(),
            namespace: default_namespace(),
            top_k,
            filters: vec![],
        }
    }

    // -- WorkingMemory tests --

    #[test]
    fn working_insert_and_search() {
        let mut wm = WorkingMemory::new(10);
        let e1 = make_entry("hello world");
        let id = wm.insert(e1).unwrap();
        let results = wm.search(&make_query("hello", 10)).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, id);
    }

    #[test]
    fn working_capacity_eviction() {
        let mut wm = WorkingMemory::new(3);
        for i in 0..5 {
            wm.insert(make_entry(&format!("entry {i}"))).unwrap();
        }
        let results = wm.search(&make_query("entry", 10)).unwrap();
        assert_eq!(results.len(), 3);
        assert!(results.iter().any(|e| e.content == "entry 2"));
        assert!(results.iter().any(|e| e.content == "entry 3"));
        assert!(results.iter().any(|e| e.content == "entry 4"));
    }

    #[test]
    fn working_remove() {
        let mut wm = WorkingMemory::new(10);
        let e1 = make_entry("first");
        let id = wm.insert(e1).unwrap();
        assert!(wm.remove(id).is_ok());
        assert!(wm.remove(id).is_err());
    }

    #[test]
    fn working_clear() {
        let mut wm = WorkingMemory::new(10);
        wm.insert(make_entry("a")).unwrap();
        wm.insert(make_entry("b")).unwrap();
        wm.clear().unwrap();
        assert_eq!(wm.entries.len(), 0);
    }

    // -- PersistentMemory tests --

    #[test]
    fn persistent_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("memory.json");
        {
            let mut pm = PersistentMemory::new(&path).unwrap();
            pm.insert(make_entry("persistent data")).unwrap();
        }
        let pm = PersistentMemory::new(&path).unwrap();
        let results = pm.search(&make_query("persistent", 10)).unwrap();
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn persistent_load_nonexistent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing.json");
        let pm = PersistentMemory::new(&path);
        assert!(pm.is_ok());
        assert!(pm.unwrap().search(&make_query("anything", 10)).unwrap().is_empty());
    }

    // -- SummarizedMemory tests --

    #[test]
    fn summarized_threshold_triggers_summary() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("summarized.json");
        let persistent = PersistentMemory::new(&path).unwrap();
        let mut sm = SummarizedMemory::new(
            persistent,
            Box::new(TestSummarizer),
            10,
            3,
            default_project(),
            default_namespace(),
        );
        for i in 0..3 {
            sm.insert(make_entry(&format!("item {i}"))).unwrap();
        }
        let w_results = sm.working.search(&make_query("item", 10)).unwrap();
        assert_eq!(w_results.len(), 0);
        let p_results = sm.persistent.search(&make_query("Summarized", 10)).unwrap();
        assert_eq!(p_results.len(), 1);
    }

    #[test]
    fn working_zero_capacity_retains_one_entry() {
        let mut wm = WorkingMemory::new(0);
        assert!(wm.insert(make_entry("anything")).is_ok());
        // Even zero capacity allows at least one entry
        let results = wm.search(&make_query("anything", 10)).unwrap();
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn summarized_below_threshold() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("below.json");
        let persistent = PersistentMemory::new(&path).unwrap();
        let mut sm = SummarizedMemory::new(
            persistent,
            Box::new(TestSummarizer),
            10,
            5,
            default_project(),
            default_namespace(),
        );
        sm.insert(make_entry("one")).unwrap();
        sm.insert(make_entry("two")).unwrap();
        let w_results = sm.working.search(&make_query("one", 10)).unwrap();
        assert_eq!(w_results.len(), 1);
        let w_results2 = sm.working.search(&make_query("two", 10)).unwrap();
        assert_eq!(w_results2.len(), 1);
    }
}
