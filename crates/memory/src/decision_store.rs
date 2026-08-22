//! Decision store — persistence layer for agent decisions.
//!
//! Stores decisions with confidence tracking, cross-session continuity,
//! and supersession tracking.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;

use concerto_core::error::MemoryError;
use concerto_core::memory::{Decision, DecisionCategory, DecisionId};

use crate::storage::MemoryDb;

/// Persistent store for agent decisions.
///
/// Uses an in-memory `HashMap` as primary storage. When `db` is provided
/// (via [`with_db`](Self::with_db)), all writes are mirrored to SQLite
/// so decisions survive process restarts.
pub struct DecisionStore {
    decisions: Mutex<HashMap<DecisionId, Decision>>,
    db: Option<Arc<MemoryDb>>,
}

impl DecisionStore {
    /// Create a new in-memory-only decision store.
    pub fn new() -> Self {
        Self { decisions: Mutex::new(HashMap::new()), db: None }
    }

    /// Create a decision store with optional SQLite persistence.
    pub fn with_db(db: Option<Arc<MemoryDb>>) -> Self {
        Self { decisions: Mutex::new(HashMap::new()), db }
    }

    /// Hydrate existing decisions and mirror subsequent changes to the same DB.
    pub async fn load(db: Arc<MemoryDb>) -> Result<Self, MemoryError> {
        let decisions = db
            .list_decisions()
            .await?
            .into_iter()
            .map(|decision| (decision.id, decision))
            .collect();
        Ok(Self { decisions: Mutex::new(decisions), db: Some(db) })
    }

    /// Insert a decision.
    pub fn insert(&self, decision: Decision) -> Result<(), MemoryError> {
        let mut store = self
            .decisions
            .lock()
            .map_err(|_| MemoryError::Persistence("decision store lock poisoned".into()))?;
        store.insert(decision.id, decision.clone());
        if let Some(ref db) = self.db {
            let db = db.clone();
            spawn_persistence(
                "insert decision",
                async move { db.insert_decision(&decision).await },
            );
        }
        Ok(())
    }

    /// Retrieve a decision by ID.
    pub fn get(&self, id: DecisionId) -> Result<Option<Decision>, MemoryError> {
        let store = self
            .decisions
            .lock()
            .map_err(|_| MemoryError::Persistence("decision store lock poisoned".into()))?;
        Ok(store.get(&id).cloned())
    }

    /// List all decisions.
    pub fn list_all(&self) -> Result<Vec<Decision>, MemoryError> {
        let store = self
            .decisions
            .lock()
            .map_err(|_| MemoryError::Persistence("decision store lock poisoned".into()))?;
        Ok(store.values().cloned().collect())
    }

    /// List decisions by category.
    pub fn list_by_category(
        &self,
        category: DecisionCategory,
    ) -> Result<Vec<Decision>, MemoryError> {
        let store = self
            .decisions
            .lock()
            .map_err(|_| MemoryError::Persistence("decision store lock poisoned".into()))?;
        Ok(store.values().filter(|d| d.category == category).cloned().collect())
    }

    /// Update the confidence of a decision.
    pub fn update_confidence(
        &self,
        id: DecisionId,
        new_confidence: f32,
    ) -> Result<(), MemoryError> {
        let mut store = self
            .decisions
            .lock()
            .map_err(|_| MemoryError::Persistence("decision store lock poisoned".into()))?;
        if let Some(d) = store.get_mut(&id) {
            d.confidence = new_confidence;
            if let Some(ref db) = self.db {
                let db = db.clone();
                spawn_persistence("update decision confidence", async move {
                    db.update_decision_confidence(id, new_confidence).await
                });
            }
            Ok(())
        } else {
            Err(MemoryError::RetrievalFailed(format!("decision {id} not found")))
        }
    }

    /// Mark a decision as superseded by another.
    pub fn supersede(&self, id: DecisionId, superseded_by: DecisionId) -> Result<(), MemoryError> {
        let mut store = self
            .decisions
            .lock()
            .map_err(|_| MemoryError::Persistence("decision store lock poisoned".into()))?;
        if let Some(d) = store.get_mut(&id) {
            d.superseded_by = Some(superseded_by);
            if let Some(ref db) = self.db {
                let db = db.clone();
                spawn_persistence("supersede decision", async move {
                    db.supersede_decision(id, superseded_by).await
                });
            }
            Ok(())
        } else {
            Err(MemoryError::RetrievalFailed(format!("decision {id} not found")))
        }
    }

    /// Remove a decision.
    pub fn delete(&self, id: DecisionId) -> Result<(), MemoryError> {
        let mut store = self
            .decisions
            .lock()
            .map_err(|_| MemoryError::Persistence("decision store lock poisoned".into()))?;
        store.remove(&id);
        if let Some(ref db) = self.db {
            let db = db.clone();
            spawn_persistence("delete decision", async move { db.delete_decision(id).await });
        }
        Ok(())
    }
}

impl Default for DecisionStore {
    fn default() -> Self {
        Self::new()
    }
}

fn spawn_persistence<F>(operation: &'static str, future: F)
where
    F: std::future::Future<Output = Result<(), MemoryError>> + Send + 'static,
{
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => {
            std::mem::drop(handle.spawn(async move {
                if let Err(error) = future.await {
                    tracing::warn!(%error, %operation, "SQLite decision persistence failed");
                }
            }));
        }
        Err(error) => {
            tracing::warn!(%error, %operation, "decision persistence requires an async runtime");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use concerto_core::ids::Ulid;
    use time::OffsetDateTime;

    fn make_decision(id_str: &str) -> (DecisionId, Decision) {
        let id = DecisionId(Ulid::from_string(id_str).unwrap_or_else(|_| {
            // Generate a deterministic Ulid from the string bytes
            let mut bytes = [0u8; 16];
            let s = id_str.as_bytes();
            let len = s.len().min(16);
            bytes[..len].copy_from_slice(&s[..len]);
            bytes[0] = bytes[0].max(1); // ensure non-zero for valid Ulid
            Ulid::from_bytes(bytes)
        }));
        let d = Decision {
            id,
            session_id: Ulid::nil(),
            task_id: None,
            what: format!("test decision {id_str}"),
            why: "test".into(),
            outcome: None,
            category: DecisionCategory::Architecture,
            confidence: 0.8,
            superseded_by: None,
            created_at: OffsetDateTime::now_utc(),
        };
        (d.id, d)
    }

    #[test]
    fn insert_and_get() {
        let store = DecisionStore::new();
        let (id, d) = make_decision("d1");
        store.insert(d).unwrap();
        let retrieved = store.get(id).unwrap().unwrap();
        assert_eq!(retrieved.what, "test decision d1");
        assert!((retrieved.confidence - 0.8).abs() < f32::EPSILON);
    }

    #[test]
    fn update_confidence() {
        let store = DecisionStore::new();
        let (id, d) = make_decision("d1");
        store.insert(d).unwrap();
        store.update_confidence(id, 0.95).unwrap();
        let retrieved = store.get(id).unwrap().unwrap();
        assert!((retrieved.confidence - 0.95).abs() < f32::EPSILON);
    }

    #[test]
    fn supersede_decision() {
        let store = DecisionStore::new();
        let (id1, d1) = make_decision("d1");
        let (id2, d2) = make_decision("d2");
        store.insert(d1).unwrap();
        store.insert(d2).unwrap();
        store.supersede(id1, id2).unwrap();
        let retrieved = store.get(id1).unwrap().unwrap();
        assert_eq!(retrieved.superseded_by, Some(id2));
    }
}
