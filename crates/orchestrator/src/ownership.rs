//! Artifact ownership and conflict control (issue #61, parent #51).
//!
//! Durable-in-intent, gate-local-in-fact ownership over canonical
//! project-relative artifact paths: first writer auto-acquires at the write
//! decision (the in-gate acquire is atomic with the write decision), a
//! non-owner write to an owned artifact is REJECTED fail-closed with the
//! owner and its acquiring event named, releases are evented, and transfers
//! only ever happen through validated coordinator decisions — never a steal.
//!
//! Model:
//!
//! - **The [`crate::gate::WriteGate`] is the enforcement point** (ADR-60 D4
//!   single write chokepoint). The table is an in-process mirror of the
//!   ADR-60 D5 lock table (`WriteGate.locks` — same shape, same lifecycle
//!   discipline): acquisitions are stamped by the gate itself on
//!   `write-applied`, foreign writes are refused before any WAL append.
//! - **Transfer is mediated**: a validated [`crate::decisions::CoordinatorDecision`]
//!   of kind [`crate::decisions::DecisionKind::TransferOwnership`] names the
//!   receiving agent (target) and the owned artifact paths; the gate moves
//!   the record only from the actual current owner, re-stamping the record
//!   with the decision id as the new acquiring event.
//! - **Release on settle**: when the owning agent's subtask settles
//!   (completed / failed / cancelled) or the supervisor detects the agent's
//!   exit (clean completion, crash, restart budget exhausted), the gate
//!   receives an explicit release and appends one audit event per batch.
//!   The lease is tied to the live task/agent lifecycle; no new liveness
//!   machinery is built — every release is driven by an existing settle or
//!   crash signal.
//! - **Stale**: an external modification observed outside the gate flips the
//!   record to [`OwnershipStatus::Stale`] (surfaced via the gate's conflict
//!   channel and future owner writes' audit payload); it is never silently
//!   overwritten and never a silent steal.
//! - **Reads never require ownership** — nothing in this module is consulted
//!   on any read path.
//!
//! Persistence shape: [`OwnershipRecord`] (serde-defaulted) carries the
//! checkpoint projection ([`OwnershipState`]); restoring re-derives live
//! in-memory state from it.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Ownership status of an owned artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OwnershipStatus {
    /// Held by the recorded owner.
    Owned,
    /// A write outside the gate was observed; the owner must reconcile
    /// before further conflicting expectations are granted surface.
    Stale,
}

/// One durable ownership record: a canonical artifact path → its owner, the
/// event that acquired it (the write's `call_id`, or the coordinator
/// transfer decision id), when it was acquired, and its status.
///
/// Every field is serde-defaulted so a checkpoint written before this
/// feature (or by an older producer) still deserializes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnershipRecord {
    /// Canonical workspace-root-relative artifact path.
    pub artifact: String,
    /// The owner agent id.
    #[serde(default)]
    pub owner: String,
    /// The whiteboard event id that recorded the acquisition (the write's
    /// `call_id` for an auto-acquire, the decision id for a transfer).
    #[serde(default)]
    pub acquiring_event_id: String,
    /// Unix epoch millis of the acquire/transfer.
    #[serde(default)]
    pub acquired_at_ms: i64,
    #[serde(default = "default_status")]
    pub status: OwnershipStatus,
}

/// Serialized default for [`OwnershipRecord::status`] — `owned`.
fn default_status() -> OwnershipStatus {
    OwnershipStatus::Owned
}

/// Checkpoint projection of the whole table (additive serde: absent keys on
/// older records deserialize to the empty state).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct OwnershipState {
    #[serde(default)]
    pub records: Vec<OwnershipRecord>,
}

/// The gate-side decision for one write target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OwnershipVerdict {
    /// No active record for the path — the caller may acquire atomically
    /// with the write decision.
    Acquirable,
    /// The path is owned by `owner` (stale status does NOT change this: a
    /// stale record still blocks a foreign write until released/transferred).
    HeldBy { owner: String, acquiring_event_id: String },
}

/// Error surfaced when ownership cannot legitimately change hands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OwnershipTransferError {
    /// The artifact is not owned (or not owned by the claimed former owner).
    NotOwned { artifact: String },
    /// The claimed former owner does not match the record's owner.
    ForeignOwner { artifact: String, actual_owner: String },
}

/// Which lifecycle mutation to record in an `ownership-event` payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OwnershipAction {
    Transfer,
    Release,
    // Nothing is recorded as a gate-side crash signal kind yet; releases
    // driven by crash detection carry the reason string instead.
    Stale,
}

/// The in-memory ownership table: canonical artifact path → record.
///
/// Gate-process-local by design (mirroring the ADR-60 D5 lock table): the
/// durable authority for every acquire-at-write is the `write-applied` row
/// itself and the explicit `ownership-event` transfers/releases/stales, and
/// the checkpoint projection ([`OwnershipState`]) re-derives state across
/// runs.
impl Default for OwnershipTable {
    fn default() -> Self {
        Self::new()
    }
}

pub struct OwnershipTable {
    records: BTreeMap<String, OwnershipRecord>,
}

impl OwnershipTable {
    pub fn new() -> Self {
        Self { records: BTreeMap::new() }
    }

    /// The verdict for `agent` writing `artifact`.
    pub fn verdict(&self, agent: &str, artifact: &str) -> OwnershipVerdict {
        match self.records.get(artifact) {
            Some(record) if record.owner != agent => OwnershipVerdict::HeldBy {
                owner: record.owner.clone(),
                acquiring_event_id: record.acquiring_event_id.clone(),
            },
            _ => OwnershipVerdict::Acquirable,
        }
    }

    /// Acquire the missing artifacts for `agent` in one atomic batch (the
    /// write decision). Paths the agent already owns (idempotent re-entry)
    /// and paths owned by someone else (caller's contract says none — every
    /// versioned target was verdict-checked) are skipped. Returns the paths
    /// this call newly claimed so the caller can roll them back on failure.
    pub fn acquire_missing(
        &mut self,
        agent: &str,
        artifacts: &[String],
        acquiring_event_id: &str,
        now_ms: i64,
    ) -> Vec<String> {
        let mut acquired = Vec::new();
        for artifact in artifacts {
            match self.records.get(artifact) {
                Some(_) => {}
                None => {
                    self.records.insert(
                        artifact.clone(),
                        OwnershipRecord {
                            artifact: artifact.clone(),
                            owner: agent.to_owned(),
                            acquiring_event_id: acquiring_event_id.to_owned(),
                            acquired_at_ms: now_ms,
                            status: OwnershipStatus::Owned,
                        },
                    );
                    acquired.push(artifact.clone());
                }
            }
        }
        acquired
    }

    /// Roll back acquires whose write decision did not commit (the WAL
    /// append failed). Only paths this process inserted for THIS decision
    /// are removed (the caller passes exactly the batch it inserted).
    pub fn rollback_acquires(&mut self, artifacts: &[String]) {
        for artifact in artifacts {
            self.records.remove(artifact);
        }
    }

    /// Release every artifact the artifacts list names that is held by
    /// `owner`; returns the released subset (idempotent — unknown or
    /// foreign-held paths stay).
    pub fn release_paths(&mut self, owner: &str, artifacts: &[String]) -> Vec<String> {
        let mut released = Vec::new();
        for artifact in artifacts {
            if let Some(record) = self.records.get(artifact) {
                if record.owner == owner {
                    self.records.remove(artifact);
                    released.push(artifact.clone());
                }
            }
        }
        released
    }

    /// Release every artifact held by `owner` (settle/crash release).
    /// Returns the released paths.
    pub fn release_agent(&mut self, owner: &str) -> Vec<String> {
        let released: Vec<String> = self
            .records
            .iter()
            .filter(|(_, record)| record.owner == owner)
            .map(|(artifact, _)| artifact.clone())
            .collect();
        for artifact in &released {
            self.records.remove(artifact);
        }
        released
    }

    /// Mediated transfer: `from_owner` → `to_agent`, re-stamping the
    /// acquiring event with the coordinator decision id. Only the actual
    /// current owner can initiate and only the actual record moves.
    pub fn transfer(
        &mut self,
        artifact: &str,
        from_owner: &str,
        to_agent: &str,
        decision_event_id: &str,
        now_ms: i64,
    ) -> Result<OwnershipRecord, OwnershipTransferError> {
        let record = self
            .records
            .get(artifact)
            .ok_or_else(|| OwnershipTransferError::NotOwned { artifact: artifact.to_owned() })?;
        if record.owner != from_owner {
            return Err(OwnershipTransferError::ForeignOwner {
                artifact: artifact.to_owned(),
                actual_owner: record.owner.clone(),
            });
        }
        let transferred = OwnershipRecord {
            artifact: artifact.to_owned(),
            owner: to_agent.to_owned(),
            acquiring_event_id: decision_event_id.to_owned(),
            acquired_at_ms: now_ms,
            status: OwnershipStatus::Owned,
        };
        self.records.insert(artifact.to_owned(), transferred.clone());
        Ok(transferred)
    }

    /// An external modification flips an Owned record to Stale. Unowned or
    /// already-stale records are left untouched. Returns the path when a
    /// record was flipped.
    pub fn mark_stale(&mut self, artifact: &str) -> Option<String> {
        if let Some(record) = self.records.get_mut(artifact) {
            if record.status == OwnershipStatus::Owned {
                record.status = OwnershipStatus::Stale;
                return Some(artifact.to_owned());
            }
        }
        None
    }

    /// Whether `agent` currently holds `artifact` (Owned or Stale — both
    /// count as holding; stale ownership keeps the honest-conflict channel
    /// open rather than letting a foreigner in through the stale door).
    pub fn holds(&self, agent: &str, artifact: &str) -> bool {
        self.records.get(artifact).is_some_and(|record| record.owner == agent)
    }

    /// The status of `artifact`, if held.
    pub fn status_of(&self, artifact: &str) -> Option<OwnershipStatus> {
        self.records.get(artifact).map(|record| record.status)
    }

    /// Deterministic snapshot of the whole table (checkpoint).
    pub fn records(&self) -> Vec<OwnershipRecord> {
        self.records.values().cloned().collect()
    }

    /// Re-derive the table from a checkpoint projection. Replaces the whole
    /// in-memory state (the projection is authoritative for a resumed run).
    pub fn restore(&mut self, state: &OwnershipState) {
        self.records =
            state.records.iter().cloned().map(|record| (record.artifact.clone(), record)).collect();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn acquire(table: &mut OwnershipTable, owner: &str, path: &str) {
        table.acquire_missing(owner, &[path.to_owned()], "evt-acq", 1_000);
    }

    #[test]
    fn unowned_target_is_acquirable_then_owned_by_first_writer() {
        let mut table = OwnershipTable::new();
        assert_eq!(table.verdict("a", "src/main.rs"), OwnershipVerdict::Acquirable);
        let acquired = table.acquire_missing("a", &["src/main.rs".into()], "w1", 1);
        assert_eq!(acquired, vec!["src/main.rs"]);
        assert_eq!(table.verdict("a", "src/main.rs"), OwnershipVerdict::Acquirable);
        assert_eq!(
            table.verdict("b", "src/main.rs"),
            OwnershipVerdict::HeldBy { owner: "a".into(), acquiring_event_id: "w1".into() }
        );
    }

    #[test]
    fn rollback_removes_only_the_failed_batch() {
        let mut table = OwnershipTable::new();
        acquire(&mut table, "a", "keep.rs");
        let rolled = table.acquire_missing("a", &["dropped.rs".into()], "w2", 2);
        assert_eq!(rolled, vec!["dropped.rs"]);
        table.rollback_acquires(&rolled);
        assert!(table.holds("a", "keep.rs"));
        assert!(!table.holds("a", "dropped.rs"));
    }

    #[test]
    fn release_is_owner_scoped() {
        let mut table = OwnershipTable::new();
        acquire(&mut table, "a", "a.rs");
        acquire(&mut table, "b", "b.rs");
        let released = table.release_paths("a", &["a.rs".to_owned(), "b.rs".to_owned()]);
        assert_eq!(released, vec!["a.rs"]);
        assert!(table.holds("b", "b.rs"));
    }

    #[test]
    fn release_agent_releases_everything_the_agent_holds() {
        let mut table = OwnershipTable::new();
        acquire(&mut table, "a", "one.rs");
        acquire(&mut table, "a", "two.rs");
        acquire(&mut table, "b", "three.rs");
        let released = table.release_agent("a");
        assert_eq!(released.len(), 2);
        assert!(table.holds("b", "three.rs"));
        assert!(!table.holds("a", "one.rs"));
    }

    #[test]
    fn transfer_moves_the_record_and_restamps_the_acquiring_event() {
        let mut table = OwnershipTable::new();
        acquire(&mut table, "a", "x.rs");
        let transferred = table
            .transfer("x.rs", "a", "b", "dec-1", 2_000)
            .unwrap_or_else(|e| panic!("transfer refused: {e:?}"));
        assert_eq!(transferred.owner, "b");
        assert_eq!(transferred.acquiring_event_id, "dec-1");
        assert_eq!(
            table.verdict("a", "x.rs"),
            OwnershipVerdict::HeldBy { owner: "b".into(), acquiring_event_id: "dec-1".into() }
        );
    }

    #[test]
    fn transfer_from_a_non_owner_is_refused() {
        let mut table = OwnershipTable::new();
        acquire(&mut table, "a", "x.rs");
        assert!(matches!(
            table.transfer("x.rs", "b", "c", "dec-1", 2_000),
            Err(OwnershipTransferError::ForeignOwner { actual_owner, .. }) if actual_owner == "a"
        ));
        assert!(matches!(
            table.transfer("missing.rs", "a", "c", "dec-2", 2_000),
            Err(OwnershipTransferError::NotOwned { .. })
        ));
    }

    #[test]
    fn external_modification_flips_owned_records_stale() {
        let mut table = OwnershipTable::new();
        acquire(&mut table, "a", "x.rs");
        assert_eq!(table.mark_stale("x.rs"), Some("x.rs".to_owned()),);
        assert_eq!(table.status_of("x.rs"), Some(OwnershipStatus::Stale));
        // Stale is absorbing until a release/transfer resolves it.
        assert_eq!(table.mark_stale("x.rs"), None);
        // Foreign writes stay blocked by a stale record.
        assert!(matches!(table.verdict("b", "x.rs"), OwnershipVerdict::HeldBy { .. }));
        // And a stale flip on an unowned path is a no-op.
        assert_eq!(table.mark_stale("unowned.rs"), None);
    }

    #[test]
    fn checkpoint_projection_round_trips() {
        let mut table = OwnershipTable::new();
        acquire(&mut table, "a", "x.rs");
        let _ = table.mark_stale("x.rs");
        acquire(&mut table, "b", "y.rs");
        let state = OwnershipState { records: table.records() };
        let json = serde_json::to_string(&state).unwrap_or_else(|e| panic!("serialize: {e}"));
        let restored: OwnershipState =
            serde_json::from_str(&json).unwrap_or_else(|e| panic!("deserialize: {e}"));
        let mut other = OwnershipTable::new();
        other.restore(&restored);
        assert!(other.holds("b", "y.rs"));
        assert_eq!(other.status_of("x.rs"), Some(OwnershipStatus::Stale));
    }
}
