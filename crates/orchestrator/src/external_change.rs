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

/// One planned-artifact drift finding (Phase 6 M3c).
///
/// A *plan drift* is an artifact the plan expects to exist but that is absent
/// from the live workspace, where the run's own recorded writes do not explain
/// the absence. It is deliberately distinct from an
/// [`ExternalChangeRecord`]: F3 reports divergences of *observed* paths
/// (content/size/mtime changed, or an observed file vanished); plan drift
/// reports the planned-but-absent class — a planned artifact the interrupted
/// run never materialised, or one externally removed without ever being
/// recorded as a run write. A path explained by the run's own writes is F3's
/// domain and is never double-reported here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanDrift {
    /// The plan artifact id (`plan-<plan_id>.json`) this drift is scoped to;
    /// `None` when the checkpoint carried no plan id. Additive field —
    /// consumers must not rely on it being present.
    pub plan_id: Option<String>,
    /// Project-root-relative forward-slash paths the plan expected but that
    /// are absent from the live inventory and unexplained by the run's own
    /// writes. Sorted and deduplicated.
    pub affected_paths: Vec<String>,
    /// Declared entries with no parseable file path (prose a model emitted
    /// where a path was expected). Logged as a malformed declaration, NEVER
    /// counted as drift — see [`crate::declared_artifacts`]. Sorted and
    /// deduplicated. Additive field: older records deserialize as empty.
    #[serde(default)]
    pub unverifiable: Vec<String>,
}

impl PlanDrift {
    /// Whether any planned artifact drifted.
    pub fn is_empty(&self) -> bool {
        self.affected_paths.is_empty()
    }
}

/// Compare the plan's expected artifacts against the live workspace inventory
/// (Phase 6 M3c, gate `plan_drift_detected_on_tampered_worktree`).
///
/// `expected` are the plan-declared artifact paths (project-root-relative);
/// `live` is the current [`SnapshotEntry`] inventory; `own_written` holds the
/// canonical project-root-relative paths the run recorded writing (the same
/// write set the F3 reconciliation excludes). An expected path drifts when it
/// is absent from `live` AND not in `own_written`.
///
/// Non-file expectations (empty, directory — trailing `/` — or glob patterns)
/// cannot be verified against a file inventory and are skipped rather than
/// reported as false drift. Prose declarations (a description where a path
/// was expected) are collected into [`PlanDrift::unverifiable`] instead —
/// they are a malformed declaration, never workspace drift.
pub fn detect_plan_drift(
    plan_id: Option<&str>,
    expected: &[camino::Utf8PathBuf],
    live: &[SnapshotEntry],
    own_written: &std::collections::HashSet<String>,
) -> PlanDrift {
    let classification = classify_artifact_drift(expected, &[], live, own_written);
    PlanDrift {
        plan_id: plan_id.map(str::to_owned),
        affected_paths: classification.affected_paths(),
        unverifiable: classification.unverifiable,
    }
}

// ---------------------------------------------------------------------------
// Phase 6 M3c — classified diff, live re-verification, investigation
// ---------------------------------------------------------------------------

/// The Phase 6 M3c step-1 classification: every declared artifact of the
/// COMPLETED subtasks sorted into a typed finding (or declared prose), with
/// the run's own writes excluded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriftClassification {
    /// Findings, sorted and deduplicated by path.
    pub diff: Vec<concerto_core::event::PlanDriftDiffEntry>,
    /// Declared entries with no parseable file path (prose). Never drift.
    pub unverifiable: Vec<String>,
}

impl DriftClassification {
    /// The classified paths (the event's `affected_paths`), sorted.
    pub fn affected_paths(&self) -> Vec<String> {
        self.diff.iter().map(|entry| entry.path.clone()).collect()
    }

    /// Whether the classification found nothing to investigate.
    pub fn is_empty(&self) -> bool {
        self.diff.is_empty()
    }
}

/// Phase 6 M3c step 1: classify the plan's expected artifacts against the
/// run-start baseline AND the resume's live inventory.
///
/// - `expected` — plan-declared artifact paths of the COMPLETED subtasks.
/// - `baseline` — the run-start workspace inventory (the checkpoint's
///   `WorkspaceSnapshot` event payload). Empty when the log window no longer
///   carries it: the classifier then degrades to the missing-only reading
///   (an absent baseline can neither prove alteration nor addition).
/// - `live` — the resume's fresh inventory.
/// - `own_written` — paths the run recorded writing; an own write explains
///   the divergence and is never drift.
///
/// Non-file expectations are skipped and prose declarations land in
/// [`DriftClassification::unverifiable`], exactly as [`detect_plan_drift`]
/// does.
pub fn classify_artifact_drift(
    expected: &[camino::Utf8PathBuf],
    baseline: &[SnapshotEntry],
    live: &[SnapshotEntry],
    own_written: &std::collections::HashSet<String>,
) -> DriftClassification {
    use crate::declared_artifacts::{classify, DeclaredArtifact};

    let live_by_path: std::collections::HashMap<&str, &SnapshotEntry> =
        live.iter().map(|entry| (entry.path.as_str(), entry)).collect();
    let baseline_by_path: std::collections::HashMap<&str, &SnapshotEntry> =
        baseline.iter().map(|entry| (entry.path.as_str(), entry)).collect();

    let mut diff: Vec<concerto_core::event::PlanDriftDiffEntry> = Vec::new();
    let mut unverifiable: Vec<String> = Vec::new();
    for path in expected {
        let normalized = match classify(path.as_str()) {
            // Not a concrete file path; the inventory cannot speak to it.
            DeclaredArtifact::NonFile => continue,
            // Prose — a declaration problem, reported separately.
            DeclaredArtifact::Unverifiable => {
                unverifiable.push(path.as_str().to_owned());
                continue;
            }
            DeclaredArtifact::Path(normalized) => normalized,
        };
        // The run's own write explains any divergence: F3's domain, never
        // plan drift (double-reporting is worse than under-reporting).
        if own_written.contains(&normalized) {
            continue;
        }
        let class = match live_by_path.get(normalized.as_str()) {
            // Absent from the live inventory: planned-but-absent.
            None => concerto_core::event::PlanDriftDiffClass::Missing,
            Some(live_entry) => match baseline_by_path.get(normalized.as_str()) {
                Some(base_entry) => {
                    if identity_differs(base_entry, live_entry) {
                        concerto_core::event::PlanDriftDiffClass::AlteredHash
                    } else {
                        // Present and unchanged since the run start: intact.
                        continue;
                    }
                }
                None => {
                    // No run-start record for this path. Only a NON-EMPTY
                    // baseline can prove the file is an addition: with no
                    // baseline at all, presence alone is not a finding.
                    if baseline.is_empty() {
                        continue;
                    }
                    concerto_core::event::PlanDriftDiffClass::New
                }
            },
        };
        diff.push(concerto_core::event::PlanDriftDiffEntry { path: normalized, class });
    }
    diff.sort_by(|left, right| left.path.cmp(&right.path));
    diff.dedup_by(|left, right| left.path == right.path);
    unverifiable.sort();
    unverifiable.dedup();
    DriftClassification { diff, unverifiable }
}

/// Whether two inventory entries disagree on recorded identity. The content
/// hash is authoritative when both sides captured one (files ≤ 64 KiB);
/// otherwise a size disagreement stands in. A side with no comparable
/// identity yields no opinion — an mtime-only touch is not content drift.
fn identity_differs(left: &SnapshotEntry, right: &SnapshotEntry) -> bool {
    match (&left.content_hash, &right.content_hash) {
        (Some(left_hash), Some(right_hash)) => left_hash != right_hash,
        _ => match (left.size_bytes, right.size_bytes) {
            (Some(left_size), Some(right_size)) => left_size != right_size,
            _ => false,
        },
    }
}

/// One plan-drift investigation (Phase 6 M3c steps 1–2): the classified
/// snapshot-vs-checkpoint diff plus the live-filesystem re-read of every
/// finding. Assembled by the resume path before the resume evaluation, so
/// the evaluation can weigh confirmed drift as workspace-change evidence.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlanDriftInvestigation {
    /// Classified findings for the plan's COMPLETED-subtask artifacts.
    pub diff: Vec<concerto_core::event::PlanDriftDiffEntry>,
    /// Live re-read of each finding, in `diff` order.
    pub reverify: Vec<concerto_core::event::PlanDriftReverifyEntry>,
    /// Declared prose — logged, never drift, never re-verified.
    pub unverifiable: Vec<String>,
}

impl PlanDriftInvestigation {
    /// Whether the classification found nothing to investigate.
    pub fn is_empty(&self) -> bool {
        self.diff.is_empty()
    }

    /// The classified paths (the event's `affected_paths`).
    pub fn affected_paths(&self) -> Vec<String> {
        self.diff.iter().map(|entry| entry.path.clone()).collect()
    }

    /// Step 2's clear verdict: findings existed but every one of them reads
    /// back intact — the resume continues unchanged and publishes nothing.
    pub fn is_cleared(&self) -> bool {
        !self.diff.is_empty()
            && self
                .reverify
                .iter()
                .all(|entry| entry.status == concerto_core::event::PlanDriftReverifyStatus::Intact)
    }

    /// Whether at least one finding survived re-verification as real drift
    /// (the step-3 replan trigger).
    pub fn confirmed_paths(&self) -> Vec<String> {
        self.reverify
            .iter()
            .filter(|entry| entry.status.is_confirmed_drift())
            .map(|entry| entry.path.clone())
            .collect()
    }

    /// Whether re-reading a finding failed for a reason other than absence —
    /// step 5's "re-verification genuinely failed" halt.
    pub fn reverify_failed(&self) -> bool {
        self.reverify
            .iter()
            .any(|entry| entry.status == concerto_core::event::PlanDriftReverifyStatus::Unverified)
    }
}

/// Phase 6 M3c steps 1–2 in one pass: classify, then re-read each finding
/// from the live filesystem.
///
/// `project_root` resolves the relative paths; `baseline` is the run-start
/// inventory the re-read compares against (`None` reference ⇒ presence-only
/// read). Bounded and synchronous by design: the findings are the plan's
/// declared artifacts (a small, known set) and each content read is capped
/// at the snapshot's own 64 KiB hashing limit — the same bound the
/// inventory walk applies to a whole tree.
pub fn investigate_plan_drift(
    expected: &[camino::Utf8PathBuf],
    baseline: &[SnapshotEntry],
    live: &[SnapshotEntry],
    own_written: &std::collections::HashSet<String>,
    project_root: &std::path::Path,
) -> PlanDriftInvestigation {
    let classification = classify_artifact_drift(expected, baseline, live, own_written);
    let baseline_by_path: std::collections::HashMap<&str, &SnapshotEntry> =
        baseline.iter().map(|entry| (entry.path.as_str(), entry)).collect();
    let reverify = classification
        .diff
        .iter()
        .map(|finding| {
            let reference = baseline_by_path.get(finding.path.as_str()).copied();
            let status = reverify_finding(project_root, &finding.path, finding.class, reference);
            concerto_core::event::PlanDriftReverifyEntry { path: finding.path.clone(), status }
        })
        .collect();
    PlanDriftInvestigation {
        diff: classification.diff,
        reverify,
        unverifiable: classification.unverifiable,
    }
}

/// Phase 6 M3c step 2: re-read one classified finding from the live
/// filesystem.
///
/// - absent (or the expectation resolves to a directory) ⇒ [`Gone`] /
///   [`Diverged`];
/// - present, `appeared` (`New` finding) ⇒ [`Diverged`]: a file that
///   post-dates the run start is a divergence by definition, its presence
///   is not evidence of the run's own work;
/// - present, with a baseline entry whose identity disagrees ⇒ [`Diverged`];
/// - present otherwise ⇒ [`Intact`] (the finding is cleared silently);
/// - any other I/O failure ⇒ [`Unverified`] (re-verification genuinely
///   failed — the resume halts Partial with the diff rather than guessing).
///
/// [`Gone`]: concerto_core::event::PlanDriftReverifyStatus::Gone
/// [`Diverged`]: concerto_core::event::PlanDriftReverifyStatus::Diverged
/// [`Intact`]: concerto_core::event::PlanDriftReverifyStatus::Intact
/// [`Unverified`]: concerto_core::event::PlanDriftReverifyStatus::Unverified
pub fn reverify_finding(
    project_root: &std::path::Path,
    path: &str,
    class: concerto_core::event::PlanDriftDiffClass,
    reference: Option<&SnapshotEntry>,
) -> concerto_core::event::PlanDriftReverifyStatus {
    use concerto_core::event::PlanDriftReverifyStatus;
    use std::io::ErrorKind;

    let absolute = project_root.join(path);
    match std::fs::metadata(&absolute) {
        Err(error) if error.kind() == ErrorKind::NotFound => return PlanDriftReverifyStatus::Gone,
        Err(_) => return PlanDriftReverifyStatus::Unverified,
        // The plan expects a file: a directory (or a broken symlink's
        // target) at that path is not the artifact the plan declared.
        Ok(metadata) if !metadata.is_file() => return PlanDriftReverifyStatus::Diverged,
        Ok(_) => {}
    }
    if class == concerto_core::event::PlanDriftDiffClass::New {
        return PlanDriftReverifyStatus::Diverged;
    }
    let Some(fresh) =
        crate::workspace_snapshot::snapshot_file_entry(project_root, absolute.as_path())
    else {
        // Stat'ed a file but could not inventory it (read/strip failure).
        return PlanDriftReverifyStatus::Unverified;
    };
    match reference {
        None => PlanDriftReverifyStatus::Intact,
        Some(reference) if identity_differs(reference, &fresh) => PlanDriftReverifyStatus::Diverged,
        Some(_) => PlanDriftReverifyStatus::Intact,
    }
}

/// The Phase 6 M3c step-5 re-dispatch state a resume leaves behind: the
/// confirmed drift, the completed subtasks it re-armed Pending, and their
/// human role labels. Held by the coordinator until the completion tail so a
/// re-dispatch that never settles can attach the diff to its Partial note.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanDriftRedispatch {
    /// Confirmed drifted paths the re-dispatch exists to repair.
    pub paths: Vec<String>,
    /// Re-armed node ids (the affected COMPLETED subtasks).
    pub nodes: Vec<concerto_core::TaskId>,
    /// Role labels of [`Self::nodes`], for human reports.
    pub labels: Vec<String>,
    /// The full classification, carried so the completion tail can attach the
    /// drift to its guard note if the re-dispatch never settles.
    pub diff: Vec<concerto_core::event::PlanDriftDiffEntry>,
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

    // ------------------------------------------------------------------
    // Phase 6 M3c — plan drift
    // ------------------------------------------------------------------

    fn expected(paths: &[&str]) -> Vec<camino::Utf8PathBuf> {
        paths.iter().map(camino::Utf8PathBuf::from).collect()
    }

    #[test]
    fn plan_drift_reports_planned_but_absent_artifacts() {
        let live = vec![entry("src/present.rs", Some(1), Some(1), Some("h"))];
        let own = std::collections::HashSet::new();
        let drift = detect_plan_drift(
            Some("plan-1"),
            &expected(&["src/present.rs", "src/never_written.rs"]),
            &live,
            &own,
        );
        assert_eq!(drift.plan_id.as_deref(), Some("plan-1"));
        assert_eq!(drift.affected_paths, vec!["src/never_written.rs".to_owned()]);
        assert!(!drift.is_empty());
    }

    #[test]
    fn plan_drift_is_never_empty_when_every_artifact_present() {
        let live = vec![
            entry("src/a.rs", Some(1), Some(1), Some("h")),
            entry("src/b.rs", Some(2), Some(2), Some("h2")),
        ];
        let own = std::collections::HashSet::new();
        let drift =
            detect_plan_drift(Some("plan-1"), &expected(&["src/a.rs", "src/b.rs"]), &live, &own);
        assert!(drift.is_empty());
        assert!(drift.affected_paths.is_empty());
    }

    #[test]
    fn plan_drift_excludes_paths_explained_by_the_runs_own_writes() {
        // The run recorded writing `src/mine.rs`; its absence is F3's external
        // change to report, never a duplicate plan-drift finding.
        let live: Vec<SnapshotEntry> = vec![];
        let own: std::collections::HashSet<String> =
            ["src/mine.rs".to_owned()].into_iter().collect();
        let drift = detect_plan_drift(
            Some("plan-1"),
            &expected(&["src/mine.rs", "src/other.rs"]),
            &live,
            &own,
        );
        assert_eq!(drift.affected_paths, vec!["src/other.rs".to_owned()]);
    }

    #[test]
    fn plan_drift_skips_non_file_expectations() {
        let live: Vec<SnapshotEntry> = vec![entry("src/a.rs", Some(1), Some(1), Some("h"))];
        let own = std::collections::HashSet::new();
        let drift = detect_plan_drift(
            Some("plan-1"),
            &expected(&["src/", "src/*.rs", "src/a.rs", ""]),
            &live,
            &own,
        );
        assert!(drift.is_empty(), "directories, globs, and empty paths are not drift");
    }

    #[test]
    fn plan_drift_normalizes_leading_dot_slash_and_backslashes() {
        let live: Vec<SnapshotEntry> = vec![entry("src/a.rs", Some(1), Some(1), Some("h"))];
        let own = std::collections::HashSet::new();
        let drift = detect_plan_drift(
            Some("plan-1"),
            &expected(&["./src/a.rs", ".\\src\\b.rs"]),
            &live,
            &own,
        );
        assert_eq!(drift.affected_paths, vec!["src/b.rs".to_owned()]);
    }

    /// `unverifiable` is additive: a record serialized before the field
    /// existed still deserializes as an empty list.
    #[test]
    fn plan_drift_unverifiable_field_is_additive() {
        let legacy = serde_json::json!({
            "plan_id": "plan-1",
            "affected_paths": ["src/a.rs"],
        });
        let drift: PlanDrift = serde_json::from_value(legacy).expect("older records deserialize");
        assert_eq!(drift.affected_paths, vec!["src/a.rs".to_owned()]);
        assert!(drift.unverifiable.is_empty());
    }

    /// The regression this classification exists for: descriptions emitted
    /// where file paths were declared are NEVER drift — and a genuinely
    /// absent real path still is (no overcorrection).
    #[test]
    fn plan_drift_classifies_description_entries_as_unverifiable_not_missing() {
        let live: Vec<SnapshotEntry> = vec![entry("src/present.rs", Some(1), Some(1), Some("h"))];
        let own = std::collections::HashSet::new();
        let descriptions = [
            "DESIGN.md: Comprehensive design document as specified in the requirements.",
            "src/components/StatusIndicator/StatusIcon.tsx: Component for rendering status icons.",
        ];
        let mut declared = expected(&["src/present.rs", "src/never_written.rs"]);
        declared.extend(expected(&descriptions));

        let drift = detect_plan_drift(Some("plan-1"), &declared, &live, &own);

        assert_eq!(
            drift.affected_paths,
            vec!["src/never_written.rs".to_owned()],
            "only the genuinely absent real path drifts"
        );
        let mut want_unverifiable = descriptions.map(str::to_owned).to_vec();
        want_unverifiable.sort();
        assert_eq!(drift.unverifiable, want_unverifiable, "descriptions are unverifiable");
        assert!(!drift.is_empty(), "a real missing path still reports");

        // Prose-only declarations emit NO drift at all.
        let prose_only = detect_plan_drift(Some("plan-1"), &expected(&descriptions), &live, &own);
        assert!(prose_only.is_empty(), "prose-only declarations are never drift");
        assert_eq!(prose_only.unverifiable.len(), 2);
    }

    // ------------------------------------------------------------------
    // Phase 6 M3c steps 1–2 — classified diff + live re-verification
    // ------------------------------------------------------------------

    fn class_of(
        classification: &DriftClassification,
        path: &str,
    ) -> Option<concerto_core::event::PlanDriftDiffClass> {
        classification.diff.iter().find(|finding| finding.path == path).map(|finding| finding.class)
    }

    /// Step 1 against a run-start baseline: missing, altered-hash, and new
    /// are distinct classes; intact paths and own writes produce no finding,
    /// prose is declaration quality.
    #[test]
    fn classify_artifact_drift_separates_missing_altered_new_and_intact() {
        use concerto_core::event::PlanDriftDiffClass;

        let baseline = vec![
            entry("src/intact.rs", Some(10), Some(1), Some("h-intact")),
            entry("src/altered.rs", Some(10), Some(1), Some("h-old")),
            entry("src/gone.rs", Some(7), Some(1), Some("h-gone")),
        ];
        let live = vec![
            entry("src/intact.rs", Some(10), Some(2), Some("h-intact")),
            entry("src/altered.rs", Some(11), Some(2), Some("h-new")),
            entry("src/appeared.rs", Some(3), Some(2), Some("h-new-file")),
        ];
        let own: std::collections::HashSet<String> = ["src/own.rs".to_owned()].into();
        let declared = expected(&[
            "src/intact.rs",
            "src/altered.rs",
            "src/gone.rs",
            "src/appeared.rs",
            "src/own.rs",
            "DESIGN.md: The design document.",
        ]);

        let classification = classify_artifact_drift(&declared, &baseline, &live, &own);

        assert_eq!(
            class_of(&classification, "src/gone.rs"),
            Some(PlanDriftDiffClass::Missing),
            "planned artifact absent from the live inventory"
        );
        assert_eq!(
            class_of(&classification, "src/altered.rs"),
            Some(PlanDriftDiffClass::AlteredHash),
            "content hash diverged from the run-start baseline"
        );
        assert_eq!(
            class_of(&classification, "src/appeared.rs"),
            Some(PlanDriftDiffClass::New),
            "present now but absent at run start"
        );
        assert_eq!(class_of(&classification, "src/intact.rs"), None, "unchanged is not drift");
        assert_eq!(class_of(&classification, "src/own.rs"), None, "own writes are never drift");
        assert_eq!(
            classification.affected_paths(),
            // Sorted and deduplicated by path — the canonical payload order.
            vec![
                "src/altered.rs".to_owned(),
                "src/appeared.rs".to_owned(),
                "src/gone.rs".to_owned(),
            ]
        );
        assert_eq!(classification.unverifiable, vec!["DESIGN.md: The design document.".to_owned()]);
    }

    /// Without a run-start baseline the classification degrades to the
    /// missing-only reading: presence alone can neither prove alteration
    /// nor addition.
    #[test]
    fn classify_artifact_drift_without_baseline_degrades_to_missing_only() {
        use concerto_core::event::PlanDriftDiffClass;

        let live = vec![
            entry("src/altered.rs", Some(11), Some(2), Some("h-new")),
            entry("src/appeared.rs", Some(3), Some(2), Some("h-new-file")),
        ];
        let declared = expected(&["src/altered.rs", "src/appeared.rs", "src/gone.rs"]);

        let classification =
            classify_artifact_drift(&declared, &[], &live, &std::collections::HashSet::new());

        assert_eq!(classification.affected_paths(), vec!["src/gone.rs".to_owned()]);
        assert_eq!(class_of(&classification, "src/gone.rs"), Some(PlanDriftDiffClass::Missing));
        assert!(classification.unverifiable.is_empty());
    }

    /// Step 2: the live filesystem decides — presence with the run-start
    /// identity clears, a divergent identity or absence confirms.
    #[test]
    fn reverify_finding_reads_back_the_live_filesystem() {
        use concerto_core::event::{PlanDriftDiffClass, PlanDriftReverifyStatus};

        let directory = tempfile::tempdir().expect("tempdir for re-verification");
        let root = directory.path();
        std::fs::create_dir_all(root.join("src")).expect("src dir");
        std::fs::write(root.join("src/kept.rs"), b"fn main() {}\n").expect("write artifact");
        let kept_hash = blake3::hash(b"fn main() {}\n").to_hex().to_string();
        let kept_reference = entry("src/kept.rs", Some(13), None, Some(&kept_hash));
        let stale_reference = entry("src/kept.rs", Some(13), None, Some("h-stale"));

        assert_eq!(
            reverify_finding(
                root,
                "src/kept.rs",
                PlanDriftDiffClass::Missing,
                Some(&kept_reference)
            ),
            PlanDriftReverifyStatus::Intact,
            "present with the run-start hash clears the finding"
        );
        assert_eq!(
            reverify_finding(
                root,
                "src/kept.rs",
                PlanDriftDiffClass::AlteredHash,
                Some(&stale_reference)
            ),
            PlanDriftReverifyStatus::Diverged,
            "present but not the run-start content"
        );
        assert_eq!(
            reverify_finding(root, "src/kept.rs", PlanDriftDiffClass::Missing, None),
            PlanDriftReverifyStatus::Intact,
            "no baseline entry: presence alone clears"
        );
        assert_eq!(
            reverify_finding(root, "src/kept.rs", PlanDriftDiffClass::New, None),
            PlanDriftReverifyStatus::Diverged,
            "a file that post-dates the run start is a divergence"
        );
        assert_eq!(
            reverify_finding(root, "src/never.rs", PlanDriftDiffClass::Missing, None),
            PlanDriftReverifyStatus::Gone,
            "absent from the live filesystem"
        );
        assert_eq!(
            reverify_finding(root, "src", PlanDriftDiffClass::Missing, None),
            PlanDriftReverifyStatus::Diverged,
            "a directory where the plan declared a file"
        );
        // The read disagrees with a stale baseline entry ⇒ diverged.
        assert_eq!(
            reverify_finding(
                root,
                "src/kept.rs",
                PlanDriftDiffClass::Missing,
                Some(&stale_reference)
            ),
            PlanDriftReverifyStatus::Diverged
        );
    }

    /// Steps 1–2 together: a tampered worktree confirms, a present artifact
    /// the snapshot missed clears silently.
    #[test]
    fn investigate_plan_drift_confirms_tampered_and_clears_present_artifacts() {
        use concerto_core::event::PlanDriftReverifyStatus;

        let directory = tempfile::tempdir().expect("tempdir for investigation");
        let root = directory.path();
        std::fs::create_dir_all(root.join("src")).expect("src dir");
        std::fs::write(root.join("src/kept.rs"), b"fn main() {}\n").expect("write artifact");
        let kept_hash = blake3::hash(b"fn main() {}\n").to_hex().to_string();

        let baseline = vec![
            entry("src/kept.rs", Some(13), None, Some(&kept_hash)),
            entry("src/gone.rs", Some(7), None, Some("h-gone")),
        ];
        // The resume's snapshot missed `src/kept.rs` (skip-listed subtree)
        // and never saw `src/gone.rs` (removed under it).
        let live: Vec<SnapshotEntry> = Vec::new();
        let declared = expected(&["src/kept.rs", "src/gone.rs"]);

        let investigation = investigate_plan_drift(
            &declared,
            &baseline,
            &live,
            &std::collections::HashSet::new(),
            root,
        );

        assert_eq!(
            investigation.confirmed_paths(),
            vec!["src/gone.rs".to_owned()],
            "only the artifact actually gone survives re-verification"
        );
        assert!(!investigation.is_cleared(), "confirmed drift is never cleared");
        assert!(!investigation.reverify_failed());
        assert_eq!(investigation.reverify[0].status, PlanDriftReverifyStatus::Gone);
        assert_eq!(investigation.reverify[1].status, PlanDriftReverifyStatus::Intact);

        // Every finding intact ⇒ step 2 clears the whole investigation.
        std::fs::write(root.join("src/gone.rs"), b"restored\n").expect("restore artifact");
        let restored_hash = blake3::hash(b"restored\n").to_hex().to_string();
        let baseline = vec![
            entry("src/kept.rs", Some(13), None, Some(&kept_hash)),
            entry("src/gone.rs", Some(9), None, Some(&restored_hash)),
        ];
        let cleared = investigate_plan_drift(
            &declared,
            &baseline,
            &live,
            &std::collections::HashSet::new(),
            root,
        );
        assert!(cleared.is_cleared(), "present with the run-start hash clears");
        assert!(cleared.confirmed_paths().is_empty());
    }
}
