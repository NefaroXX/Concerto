//! Issue #65 — external workspace changes as first-class, reconcilable
//! records.
//!
//! Concerto's local-first model assumes the workspace is exactly what a run
//! left it (the write gate's optimistic `base_version` conflicts catch
//! concurrent writes at the next coordinated write). An ambient workspace —
//! a human editor, a build tool, a git operation, another Concerto instance —
//! can change files the run neither owns nor has ever written. Issue #65
//! makes such changes explicit: one [`ExternalChangeRecord`] per affected
//! path, produced either by the live wait wake
//! (`workspace-generation-changed`) or by the F3 resume reconciliation, and
//! surfaced to the world model as decision risks.
//!
//! A path change is "external" precisely when the run's OWN writes do not
//! explain it: paths in the run's write set are never classified external
//! (checkpoint `all_files`, the live ledger's `all_files`, and every completed
//! result's `files_modified` — the same set the F3 reconciliation excludes).
//! A record may still carry a `known_owner` when the write-gate ownership
//! table holds the path: an ownership-hopeful change stays a
//! [`conflicts_with_coordinator_work`](ExternalChangeRecord::conflicts_with_coordinator_work)
//! event until the Coordinator re-reads, transfers, or reconciles it.

use serde::{Deserialize, Serialize};

use concerto_sessions::SnapshotEntry;

/// Upper bound on the retained record list, so a long run with a chatty
/// workspace cannot grow coordinator state without limit. Eviction keeps the
/// NEWEST records — the ones a decision is about to act on.
pub const MAX_EXTERNAL_CHANGE_RECORDS: usize = 64;

/// One detected external workspace change, scoped to a single affected path.
///
/// Reconcile-facing fields are additive (`#[serde(default)]`); `id` and
/// `detected_at_ms` are required identity metadata every producer stamps.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalChangeRecord {
    /// Stable unique record id (a `Ulid` string), so eviction and dedup are
    /// unambiguous even when two distinct changes touch the same path.
    pub id: String,
    /// Unix epoch milliseconds at detection time.
    pub detected_at_ms: i64,
    /// The workspace snapshot generation BEFORE the change — the run's
    /// baseline identity (the checkpoint/start generation).
    #[serde(default)]
    pub previous_generation: Option<String>,
    /// The workspace snapshot generation AFTER the change — a fresh capture
    /// taken at detection time.
    #[serde(default)]
    pub current_generation: Option<String>,
    /// Canonical workspace-root-relative paths the change touched. Exactly
    /// one path per record, so owner/conflict attribution stays honest.
    #[serde(default)]
    pub affected_paths: Vec<String>,
    /// The write-gate ownership-table owner (an agent role or name) of the
    /// affected path, when the run holds an ownership record. `None` means
    /// the change is unrelated to any coordinator-held work.
    #[serde(default)]
    pub known_owner: Option<String>,
    /// The graph task dispatched to [`Self::known_owner`], when attributable.
    #[serde(default)]
    pub known_task: Option<String>,
    /// True when the changed path conflicts with coordinator work: it is
    /// ownership-held (in-flight) or named as a planned/expected artifact.
    /// Default `false` (the conservative state for records that predate the
    /// flag) keeps the shape additive.
    #[serde(default)]
    pub conflicts_with_coordinator_work: bool,
}

impl ExternalChangeRecord {
    /// Builds a new record. The `id` is a fresh `Ulid`, so no two records
    /// collide even when the same path changes twice in one run.
    pub fn new(
        affected_paths: Vec<String>,
        previous_generation: Option<String>,
        current_generation: Option<String>,
        known_owner: Option<String>,
        known_task: Option<String>,
        conflicts_with_coordinator_work: bool,
        detected_at_ms: i64,
    ) -> Self {
        Self {
            id: concerto_core::ids::Ulid::new().to_string(),
            detected_at_ms,
            previous_generation,
            current_generation,
            affected_paths,
            known_owner,
            known_task,
            conflicts_with_coordinator_work,
        }
    }

    /// The single affected path, when any (records are emitted per path).
    pub fn first_path(&self) -> Option<&str> {
        self.affected_paths.first().map(|path| path.as_str())
    }

    /// Whether this change conflicts with the run's held or planned work.
    pub fn conflicts_with_coordinator_work(&self) -> bool {
        self.conflicts_with_coordinator_work
    }
}

/// Deterministic path-level diff between two captured inventories: paths
/// added, removed, or whose recorded identity (size, mtime, content hash)
/// changed. Sorted by path; unchanged paths are never listed. Order of the
/// input inventories is irrelevant.
pub fn diff_workspace_entries(previous: &[SnapshotEntry], fresh: &[SnapshotEntry]) -> Vec<String> {
    let previous_by_path: std::collections::HashMap<&str, &SnapshotEntry> =
        previous.iter().map(|entry| (entry.path.as_str(), entry)).collect();
    let fresh_paths: std::collections::HashSet<&str> =
        fresh.iter().map(|entry| entry.path.as_str()).collect();

    let mut changed: Vec<String> = Vec::new();
    for entry in fresh {
        let before = previous_by_path.get(entry.path.as_str());
        if before == Some(&entry) {
            continue;
        }
        changed.push(entry.path.clone());
    }
    for entry in previous {
        if !fresh_paths.contains(entry.path.as_str()) {
            changed.push(entry.path.clone());
        }
    }

    changed.sort();
    changed.dedup();
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(
        path: &str,
        size: Option<u64>,
        mtime: Option<u64>,
        hash: Option<&str>,
    ) -> SnapshotEntry {
        SnapshotEntry {
            path: path.to_owned(),
            size_bytes: size,
            mtime_ms: mtime,
            content_hash: hash.map(str::to_owned),
        }
    }

    #[test]
    fn diff_reports_added_changed_removed_and_never_unchanged() {
        let previous = vec![
            entry("src/a.rs", Some(10), Some(1), Some("h1")),
            entry("src/b.rs", Some(20), Some(2), Some("h2")),
            entry("gone.rs", Some(5), Some(3), Some("h3")),
        ];
        let fresh = vec![
            entry("src/a.rs", Some(10), Some(1), Some("h1")),
            entry("src/b.rs", Some(25), Some(9), Some("h4")), // written over
            entry("src/new.rs", Some(1), Some(4), Some("h5")), // added
        ];

        let changed = diff_workspace_entries(&previous, &fresh);
        assert_eq!(
            changed,
            vec!["gone.rs".to_owned(), "src/b.rs".to_owned(), "src/new.rs".to_owned()],
            "unchanged a.rs is absent; changed, added, and removed paths are listed"
        );
    }

    #[test]
    fn mtime_touch_without_content_change_is_a_change() {
        // Any recorded identity change (mtime here) yields a diff — the same
        // signal `compute_generation` feeds into the generation id.
        let previous = vec![entry("src/a.rs", Some(10), Some(1), Some("h1"))];
        let fresh = vec![entry("src/a.rs", Some(10), Some(99), Some("h1"))];
        assert_eq!(diff_workspace_entries(&previous, &fresh), vec!["src/a.rs".to_owned()]);
    }

    #[test]
    fn record_round_trips_and_old_records_deserialize() {
        let record = ExternalChangeRecord {
            id: "rec-1".to_owned(),
            detected_at_ms: 1_000,
            previous_generation: Some("gen-a".to_owned()),
            current_generation: Some("gen-b".to_owned()),
            affected_paths: vec!["src/a.rs".to_owned()],
            known_owner: Some("coder".to_owned()),
            known_task: Some("task-1".to_owned()),
            conflicts_with_coordinator_work: true,
        };
        let json = serde_json::to_value(&record).expect("record serializes");
        let back: ExternalChangeRecord = serde_json::from_value(json).expect("record deserializes");
        assert_eq!(back, record);
        // Additive fields default: a record carrying only the identity keys
        // still deserializes; the conservative conflict flag defaults false.
        let minimal = serde_json::json!({
            "id": "rec-0",
            "detected_at_ms": 0,
        });
        let minimal_back: ExternalChangeRecord =
            serde_json::from_value(minimal).expect("additive fields default");
        assert_eq!(minimal_back.affected_paths, Vec::<String>::new());
        assert_eq!(minimal_back.known_owner, None);
        assert_eq!(minimal_back.previous_generation, None);
        assert!(!minimal_back.conflicts_with_coordinator_work);
    }

    #[test]
    fn constructor_stamps_a_unique_id_and_first_path() {
        let a = ExternalChangeRecord::new(
            vec!["src/a.rs".to_owned()],
            None,
            None,
            None,
            None,
            false,
            5,
        );
        let b = ExternalChangeRecord::new(
            vec!["src/b.rs".to_owned()],
            None,
            None,
            None,
            None,
            false,
            5,
        );
        assert_ne!(a.id, b.id, "each record carries a unique id");
        assert_eq!(a.first_path(), Some("src/a.rs"));
        assert!(!a.conflicts_with_coordinator_work());
    }
}
