//! Decision store — persistence layer for agent decisions.
//!
//! Stores decisions with confidence tracking, cross-session continuity,
//! and supersession tracking.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;

use concerto_core::error::MemoryError;
use concerto_core::memory::{Decision, DecisionCategory, DecisionId};

use crate::maintenance::{
    MaintenanceContext, MaintenanceKind, MaintenanceLedger, MaintenanceRecord,
    MaintenanceTargetKind,
};
use crate::storage::MemoryDb;

/// Persistent store for agent decisions.
///
/// Uses an in-memory `HashMap` as primary storage. When `db` is provided
/// (via [`with_db`](Self::with_db)), all writes are mirrored to SQLite
/// so decisions survive process restarts.
pub struct DecisionStore {
    decisions: Mutex<HashMap<DecisionId, Decision>>,
    db: Option<Arc<MemoryDb>>,
    /// H12: optional maintenance ledger. When attached, `supersede` records
    /// a durable `Supersession` observation (by whom, on what evidence).
    /// Absent means no ledger was wired (in-memory-only stores) — the
    /// supersession still happens; only its durable observation is skipped.
    maintenance: Option<MaintenanceContext>,
}

impl DecisionStore {
    /// Create a new in-memory-only decision store.
    pub fn new() -> Self {
        Self { decisions: Mutex::new(HashMap::new()), db: None, maintenance: None }
    }

    /// Create a decision store with optional SQLite persistence.
    pub fn with_db(db: Option<Arc<MemoryDb>>) -> Self {
        Self { decisions: Mutex::new(HashMap::new()), db, maintenance: None }
    }

    /// Hydrate existing decisions and mirror subsequent changes to the same DB.
    pub async fn load(db: Arc<MemoryDb>) -> Result<Self, MemoryError> {
        let decisions = db
            .list_decisions()
            .await?
            .into_iter()
            .map(|decision| (decision.id, decision))
            .collect();
        Ok(Self { decisions: Mutex::new(decisions), db: Some(db), maintenance: None })
    }

    /// Attach the H12 maintenance ledger so `supersede` records a durable
    /// `Supersession` outcome under `project_id` (the memory database is
    /// per-project, but the store does not hold the id itself).
    #[must_use]
    pub fn with_maintenance_ledger(
        mut self,
        ledger: Arc<MaintenanceLedger>,
        project_id: impl Into<String>,
    ) -> Self {
        self.maintenance = Some(MaintenanceContext::new(ledger, "decision-store", project_id));
        self
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
    ///
    /// H12: when a maintenance ledger is attached, a durable `Supersession`
    /// observation is recorded (target = the superseded decision, evidence =
    /// the superseding decision). Recording mirrors the existing SQLite
    /// persistence contract: it is spawned and fail-soft, so it can never
    /// change the supersession's own result.
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
            if let Some(ref maintenance) = self.maintenance {
                let record = MaintenanceRecord::completed(
                    MaintenanceKind::Supersession,
                    id.0.to_string(),
                    MaintenanceTargetKind::Decision,
                    maintenance.actor.clone(),
                    maintenance.project_id.clone(),
                )
                .with_evidence(vec![superseded_by.0.to_string()]);
                let ledger = maintenance.ledger.clone();
                spawn_persistence("record supersession outcome", async move {
                    ledger.record(&record).await
                });
            }
            Ok(())
        } else {
            if let Some(ref maintenance) = self.maintenance {
                let record = MaintenanceRecord::failed(
                    MaintenanceKind::Supersession,
                    id.0.to_string(),
                    MaintenanceTargetKind::Decision,
                    "supersession target decision not found",
                    maintenance.actor.clone(),
                    maintenance.project_id.clone(),
                )
                .with_evidence(vec![superseded_by.0.to_string()]);
                let ledger = maintenance.ledger.clone();
                spawn_persistence("record failed supersession outcome", async move {
                    ledger.record(&record).await
                });
            }
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
    use concerto_core::CancellationToken;
    use time::OffsetDateTime;

    use crate::maintenance::{
        MaintenanceKind as Mk, MaintenanceOutcome, MaintenanceTargetKind as Mtk,
    };

    /// Poll the ledger until `pred` matches or the bounded yields run out —
    /// the record is written by a spawned task (the SQLite-persistence
    /// contract), so the test must let the runtime run it.
    async fn wait_for_outcome(
        ledger: &MaintenanceLedger,
        target: &str,
        pred: impl Fn(&MaintenanceRecord) -> bool,
    ) -> Option<MaintenanceRecord> {
        for _ in 0..200 {
            tokio::task::yield_now().await;
            let rows = ledger.for_target(target, CancellationToken::new()).await.unwrap();
            if let Some(found) = rows.into_iter().find(|record| pred(record)) {
                return Some(found);
            }
        }
        None
    }

    async fn test_ledger() -> (sqlx::SqlitePool, Arc<MaintenanceLedger>) {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        let ledger = Arc::new(MaintenanceLedger::connect(pool.clone()).await.unwrap());
        (pool, ledger)
    }

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

    #[tokio::test]
    async fn supersede_records_a_durable_supersession_outcome() {
        let (_pool, ledger) = test_ledger().await;
        let store = DecisionStore::new().with_maintenance_ledger(ledger.clone(), "proj-1");
        let (id1, d1) = make_decision("d1");
        let (id2, d2) = make_decision("d2");
        store.insert(d1).unwrap();
        store.insert(d2).unwrap();
        store.supersede(id1, id2).unwrap();

        let target = id1.0.to_string();
        let found = wait_for_outcome(&ledger, &target, |_| true).await;
        let record = found.expect("the supersession outcome is recorded");
        assert_eq!(record.kind, Mk::Supersession);
        assert_eq!(record.outcome, MaintenanceOutcome::Completed);
        assert_eq!(record.target_kind, Mtk::Decision);
        assert_eq!(record.evidence, vec![id2.0.to_string()]);
        assert_eq!(record.actor, "decision-store");
        assert_eq!(record.project_id, "proj-1");
    }

    #[tokio::test]
    async fn supersede_of_a_missing_decision_records_failed() {
        let (_pool, ledger) = test_ledger().await;
        let store = DecisionStore::new().with_maintenance_ledger(ledger.clone(), "proj-1");
        let (missing, _) = make_decision("missing");
        let (newer, d) = make_decision("newer");
        store.insert(d).unwrap();

        assert!(store.supersede(missing, newer).is_err(), "the target does not exist");

        let target = missing.0.to_string();
        let record = wait_for_outcome(&ledger, &target, |_| true)
            .await
            .expect("the failed supersession is recorded, never absent");
        assert_eq!(record.outcome, MaintenanceOutcome::Failed);
        assert!(record.reason.contains("not found"), "{}", record.reason);
    }
}
