//! H08 — managed experiment workspaces: isolated copies for edits, builds,
//! and tests, with explicit review and promotion.
//!
//! An experiment workspace is an **isolated copy** of the host project where
//! agent edits, builds, and tests run without touching the user's working
//! tree. Nothing flows back implicitly: [`promote_experiment`] is the single
//! explicit, authorized operation that carries experiment changes into the
//! host, and it refuses host-conflicting paths instead of overwriting them.
//!
//! # The dirty-baseline rule (the plan's explicit warning)
//!
//! The baseline is a **live-disk enumeration at creation time**: every regular
//! file under the host root is read as bytes and hashed (blake3), except the
//! [`EXPERIMENT_EXCLUDED_DIR_NAMES`] subtrees and symlinks (skipped and
//! recorded, never followed). Git HEAD, the index, and `git status` are
//! **never consulted**. A dirty tracked modification and an untracked file are
//! indistinguishable inputs here — both are live-disk bytes — so both land in
//! the baseline and in the workspace with the same content and hash.
//!
//! * [`ExperimentMode::Copy`] populates the workspace from exactly this
//!   enumeration (non-git plain copy).
//! * [`ExperimentMode::GitWorktree`] creates a detached worktree at the base
//!   revision and then overlays every enumerated file whose bytes differ from
//!   (or are absent in) the worktree — so dirty and untracked state lands in
//!   the worktree too, instead of silently starting from a clean HEAD.
//!
//! # Promotion and conflict detection
//!
//! Promotion carries a review binding: [`review_experiment`] diffs the
//! experiment against the baseline and returns an [`ExperimentDiff`] stamped
//! with the baseline digest and the observed experiment-state digest;
//! [`promote_experiment`] re-observes and refuses when either no longer
//! matches (stale review — the baseline or the experiment moved on — so the
//! reviewer never approves ghosts).
//!
//! For each experiment-changed path (experiment bytes differ from baseline,
//! counting create/delete transitions) the promotion re-reads the host:
//!
//! * host bytes == baseline → **clean**, applied through one gated
//!   `filesystem` write/delete that stamps the baseline hash as the gate
//!   `base_version` (a second race check at submit time, reusing the
//!   ownership/version machinery);
//! * host bytes != baseline → **conflict**: refused, recorded in the
//!   [`PromotionReport`] with baseline/host/experiment hashes, nothing
//!   written for that path;
//! * host bytes == experiment bytes → **converged**: no write needed,
//!   recorded as already-matching.
//!
//! Paths the experiment never touched are never written, even when the host
//! drifted under them (reported as notes, not conflicts — there is nothing to
//! promote for them). Foreign-owned apply targets abort the whole promotion
//! **before any write** (fail-closed, like restore); a policy denial aborts
//! mid-promotion with the applied subset named, and the record stays `Active`
//! so the failure is retriable.
//!
//! Promotion writes travel [`crate::gate::WriteGate::submit`] — the same
//! policy-gated, ownership-checked, whiteboard-audited chokepoint every agent
//! write funnels through — with `content_b64` byte-exact bodies, exactly the
//! shape restore replays. There is no separate promotion writer.
//!
//! A last-instant race (host write landing between the promotion's re-read and
//! the gated submit) is resolved by the gate's standard hunk policy: disjoint
//! hunks merge with a WAL note, same-hunk collisions refuse as conflicts and
//! are recorded. The window is one re-read-to-submit per path.
//!
//! # Semantics table (what the plan calls out)
//!
//! | Concern | Semantics |
//! |---|---|
//! | Shell effects escaping the workspace | **NOT isolated.** Shell spawns run on the host OS with the host environment; only filesystem paths under the workspace root are isolated. A shell tool with a host cwd (or host absolute paths) can touch the host tree, the network, or daemons. Isolation covers project files reached via workspace-relative filesystem ops. |
//! | Generated files | **Excluded.** Any path with an [`EXPERIMENT_EXCLUDED_DIR_NAMES`] component is outside the baseline, the copy, and promotion — in both directions. Build outputs created inside the experiment are never promoted back. |
//! | Dependency caches (`target/`, `node_modules/`, …) | **Excluded and never carried.** The experiment starts with cold caches and repopulates its own; promotion never copies them back. (This also keeps copies small and bounded.) |
//! | Host mounts | **v1: none.** No host path is mounted or shared into the workspace. Symlinks are skipped and recorded, never followed or copied — a symlink pointing at the host would otherwise defeat isolation silently. |
//! | Absolute-path / outside-root tool inputs | **Defeat isolation where they apply.** The guard is [`workspace_contains`]: workspace-relative filesystem ops stay inside; absolute host paths and `..` escapes resolve outside and must be refused by the caller. Shell tools take raw OS paths with no canonicalization at all. |
//!
//! # Separation of claims (isolation ≠ restoration)
//!
//! An experiment workspace is **not** a rollback system and **not** a
//! checkpoint restore. Removing or abandoning an experiment never touches the
//! host. If promotion (or anything else) touches the host and the operator
//! wants it undone, that goes through the checkpoint path
//! ([`crate::file_checkpoint`]), never through the experiment path. The
//! experiment record carries no restorability claim; checkpoint records carry
//! no isolation claim.
//!
//! # Runtime authorship (ADR-65)
//!
//! The agent supplies only identities (paths, agent ids); every record here is
//! constructed and transitioned by these runtime functions, and every host
//! mutation is a gated write attributed to the promoting agent on the
//! whiteboard. The experiment record is a runtime-authored fact.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use concerto_core::ids::Ulid;
use concerto_core::types::SessionContext;
use concerto_core::CancellationToken;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::gate::{GateError, GateRequest, WriteGate};

// ---------------------------------------------------------------------------
// Bounds and exclusions
// ---------------------------------------------------------------------------

/// Schema version of [`ExperimentRecord`]'s serialized form. Named-version
/// policy (per the `GraphCheckpoint`/`CheckpointRecord` precedent): this exact
/// version loads as-is; anything else is rejected.
pub const EXPERIMENT_SCHEMA_VERSION: u32 = 1;

/// Upper bound on files in one experiment baseline. Creation refuses beyond
/// this (fail-closed with [`ExperimentError::Budget`]) rather than copying a
/// silently partial tree.
pub const EXPERIMENT_MAX_FILES: usize = 10_000;

/// Upper bound on summed baseline bytes in one experiment. See
/// [`EXPERIMENT_MAX_FILES`].
pub const EXPERIMENT_MAX_TOTAL_BYTES: u64 = 256 * 1024 * 1024;

/// Upper bound on one file's bytes, creation and promotion alike. A file over
/// this refuses creation ([`ExperimentError::Budget`]); an experiment-side
/// file over this refuses promotion per path (`refused_oversized` on the
/// report — the honest downgrade, mirroring checkpoint hash-only honesty).
pub const EXPERIMENT_MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;

/// Streaming I/O buffer for hashing (never loads a whole file for a hash).
pub const EXPERIMENT_HASH_BUF_BYTES: usize = 64 * 1024;

/// Cap on captured child-process stderr carried in [`ExperimentError::Git`].
pub const EXPERIMENT_GIT_STDERR_LIMIT: usize = 2000;

/// Directory/file names excluded from the baseline, the copy, the overlay,
/// and promotion **at any depth**: generated output, dependency caches, and
/// version-control control dirs. Related to (but intentionally not aliased
/// with) the workspace-snapshot inventory skips: that list scopes planning
/// context, this list is the experiment isolation boundary — the two must
/// evolve independently.
///
/// `.git` is always excluded: in copy mode the workspace is non-git by
/// design, and in worktree mode `<ws>/.git` is the worktree gitdir pointer
/// that the overlay must never overwrite.
pub const EXPERIMENT_EXCLUDED_DIR_NAMES: &[&str] = &[
    ".git",
    ".hg",
    "target",
    "node_modules",
    "__pycache__",
    ".venv",
    "venv",
    ".tox",
    "dist",
    "build",
    ".next",
    "coverage",
    ".cache",
];

/// True when any path component of `canonical` (a forward-slash relative
/// path) is on the exclusion list.
#[must_use]
pub fn is_excluded_canonical(canonical: &str) -> bool {
    canonical.split('/').any(|component| EXPERIMENT_EXCLUDED_DIR_NAMES.contains(&component))
}

// ---------------------------------------------------------------------------
// Record vocabulary
// ---------------------------------------------------------------------------

/// How the experiment workspace is materialized.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExperimentMode {
    /// Plain recursive copy of the live-disk baseline. The workspace is
    /// non-git (no `.git` is ever copied).
    Copy,
    /// Detached git worktree at the base revision, then overlaid with the
    /// live-disk baseline (dirty + untracked included — see the module docs).
    GitWorktree,
}

/// Why a path or subtree is outside the experiment claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExperimentExclusionClass {
    /// Generated output or dependency cache
    /// ([`EXPERIMENT_EXCLUDED_DIR_NAMES`]).
    GeneratedCache,
    /// Symlink: skipped and recorded, never followed or copied.
    Symlink,
    /// Worktree control file (the `<ws>/.git` gitdir pointer).
    WorktreeControl,
}

/// One explicitly out-of-claim path or subtree (the honesty list, mirroring
/// checkpoint `excluded`: machine record, surfaced — never a silent skip).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExperimentExclusion {
    /// The out-of-claim class.
    pub class: ExperimentExclusionClass,
    /// Canonical path (or subtree root), when the exclusion is about one.
    #[serde(default)]
    pub path: Option<String>,
    /// Why this is excluded (operator-readable).
    pub reason: String,
}

/// One baselined file: canonical forward-slash path plus the checkpoint-time
/// content identity. Creation establishes the invariant that both are `Some`
/// (every enumerated file is read and hashed; a file vanishing mid-capture
/// aborts creation rather than anchoring a guess).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BaselineEntry {
    /// Canonical (project-relative, forward-slash) path.
    pub canonical_path: String,
    /// Blake3 hex of the baseline bytes.
    pub baseline_hash: String,
    /// Baseline byte size.
    pub baseline_len: u64,
}

/// Lifecycle of an experiment record. Promotion never rewrites history: a
/// partial promotion leaves `Active` with the report in `last_report` so the
/// review → fix → promote loop continues; only a conflict-free promotion
/// transitions to `Promoted`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExperimentStatus {
    /// Open for edits, review, and promotion.
    Active,
    /// A conflict-free promotion applied every experiment change.
    Promoted,
    /// Withdrawn by the operator; the workspace (if any) is scratch to
    /// remove. Never touches the host.
    Abandoned,
}

/// One host-conflicting path a promotion refused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromotionConflict {
    /// Canonical path refused.
    pub path: String,
    /// Baseline hash (always known — conflicts only arise on baselined or
    /// created paths; `None` for created-in-experiment paths).
    #[serde(default)]
    pub baseline_hash: Option<String>,
    /// Host hash at promotion time (`None` when the host deleted the path).
    #[serde(default)]
    pub host_hash: Option<String>,
    /// Experiment hash at promotion time (`None` when the experiment deleted
    /// the path).
    #[serde(default)]
    pub experiment_hash: Option<String>,
    /// Why the path was refused (operator-readable).
    pub reason: String,
}

/// What one promotion did, including what it refused. Returned by value and
/// embedded in the record's `last_report` — the durable effects record.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromotionReport {
    /// Overwritten host files (present at baseline, changed in experiment,
    /// unchanged on host).
    #[serde(default)]
    pub promoted: Vec<String>,
    /// Created host files (absent at baseline and on host, present in
    /// experiment).
    #[serde(default)]
    pub recreated: Vec<String>,
    /// Deleted host files (present at baseline and on host, deleted in
    /// experiment).
    #[serde(default)]
    pub deleted: Vec<String>,
    /// Experiment-changed paths needing no write (host already holds the
    /// experiment bytes).
    #[serde(default)]
    pub already_matching: Vec<String>,
    /// Host-conflicting paths refused (nothing written for them).
    #[serde(default)]
    pub conflicts: Vec<PromotionConflict>,
    /// Host-only drift on experiment-untouched paths (surfaced, never
    /// written, never conflicts — there is nothing to promote for them).
    #[serde(default)]
    pub host_drift_untouched: Vec<String>,
    /// Experiment-side files over [`EXPERIMENT_MAX_FILE_BYTES`] (refused —
    /// promotion never loads unbounded bytes).
    #[serde(default)]
    pub refused_oversized: Vec<String>,
    /// Provenance notes (converged skips, hunk-merge races, …).
    #[serde(default)]
    pub notes: Vec<String>,
}

/// Outcome of [`promote_experiment`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromotionOutcome {
    /// Every experiment change applied; the record transitioned to
    /// `Promoted`.
    Promoted {
        /// The effects record (also embedded in the record).
        report: PromotionReport,
    },
    /// The clean subset applied; conflicts/refusals recorded. The record
    /// stays `Active` with the report embedded — review, fix, promote again.
    Partial {
        /// The effects record (also embedded in the record).
        report: PromotionReport,
    },
    /// The record was already `Promoted`: no write ran (recorded no-op
    /// carrying the first promotion's report).
    AlreadyPromoted {
        /// The first promotion's effects record.
        report: PromotionReport,
    },
}

/// The explicit review artifact: experiment-vs-baseline diff stamped with the
/// baseline digest it was computed against. Promotion requires a fresh one —
/// the reviewer approves exactly this diff, never a moved-on workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExperimentDiff {
    /// The reviewed experiment.
    pub experiment_id: String,
    /// [`ExperimentRecord::baseline_digest`] at review time (promotion
    /// binding — a review computed against any other baseline is stale).
    pub baseline_digest: String,
    /// Content digest of the observed experiment states at review time (see
    /// [`experiment_state_digest`]): promotion re-observes and refuses when
    /// the experiment moved since the review — approving ghosts is
    /// impossible even when the baseline is unchanged.
    pub experiment_digest: String,
    /// Present in both, bytes differ.
    #[serde(default)]
    pub changed: Vec<String>,
    /// Absent at baseline, present in experiment.
    #[serde(default)]
    pub created: Vec<String>,
    /// Present at baseline, absent in experiment.
    #[serde(default)]
    pub deleted: Vec<String>,
    /// Present in both, bytes identical.
    #[serde(default)]
    pub unchanged: usize,
    /// Experiment-side files over [`EXPERIMENT_MAX_FILE_BYTES`] (promotion
    /// will refuse these — the reviewer sees them here, not as a surprise).
    #[serde(default)]
    pub oversized: Vec<String>,
}

/// The managed experiment workspace record (runtime-authored fact, ADR-65:
/// constructed and transitioned by this module's functions, never by agent
/// prose).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExperimentRecord {
    /// Envelope schema version ([`EXPERIMENT_SCHEMA_VERSION`]).
    pub schema_version: u32,
    /// Experiment identity (ULID).
    pub experiment_id: String,
    /// Absolute host project root the baseline was enumerated from.
    pub host_root: PathBuf,
    /// Absolute experiment workspace root (scratch — safe to delete).
    pub workspace_root: PathBuf,
    /// How the workspace was materialized.
    pub mode: ExperimentMode,
    /// Agent id that requested the experiment (audit attribution only —
    /// authorship stays with the runtime).
    pub created_by: String,
    /// Creation time, Unix epoch millis (UTC).
    pub created_at_ms: i64,
    /// The live-disk baseline (sorted by path at creation).
    #[serde(default)]
    pub baseline: Vec<BaselineEntry>,
    /// Content-addressed workspace generation of the host at creation
    /// (advisory audit trail, via the workspace-snapshot inventory — never a
    /// scope bound).
    #[serde(default)]
    pub baseline_generation: String,
    /// Explicitly out-of-claim paths/subtrees (honesty list).
    #[serde(default)]
    pub excluded: Vec<ExperimentExclusion>,
    /// Lifecycle state.
    pub status: ExperimentStatus,
    /// Latest promotion's effects record (`None` before the first promotion).
    #[serde(default)]
    pub last_report: Option<PromotionReport>,
}

impl ExperimentRecord {
    /// Content-addressed digest of the baseline (sorted path/hash/len fold):
    /// the review binding. Any baseline change yields a new digest, so a
    /// review stamped against a moved-on baseline can never pass as fresh.
    #[must_use]
    pub fn baseline_digest(&self) -> String {
        let mut sorted: Vec<&BaselineEntry> = self.baseline.iter().collect();
        sorted.sort_by(|a, b| a.canonical_path.cmp(&b.canonical_path));
        let mut hasher = blake3::Hasher::new();
        for entry in sorted {
            hasher.update(entry.canonical_path.as_bytes());
            hasher.update(&[1]);
            hasher.update(entry.baseline_hash.as_bytes());
            hasher.update(&entry.baseline_len.to_be_bytes());
        }
        hasher.finalize().to_hex().to_string()
    }

    /// Deserialize with the named-version policy: the current version loads
    /// as-is; anything else is rejected (greenfield record — no legacy
    /// versions to migrate).
    pub fn from_json(json: &str) -> Result<Self, ExperimentError> {
        let record: Self = serde_json::from_str(json).map_err(|error| {
            ExperimentError::Invalid(format!("experiment record decode failed: {error}"))
        })?;
        if record.schema_version != EXPERIMENT_SCHEMA_VERSION {
            return Err(ExperimentError::Invalid(format!(
                "unsupported experiment record schema version {}: this runtime supports v{EXPERIMENT_SCHEMA_VERSION} only",
                record.schema_version
            )));
        }
        Ok(record)
    }

    /// Serialize the record envelope (caller persists — the runtime authors,
    /// the caller stores).
    pub fn to_json(&self) -> Result<String, ExperimentError> {
        serde_json::to_string(self)
            .map_err(|error| ExperimentError::Invalid(format!("experiment serialize: {error}")))
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// One foreign-owned path that aborted a promotion before any write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromotionOwnershipConflict {
    /// The contested canonical path.
    pub path: String,
    /// The holding agent.
    pub owner: String,
}

/// Errors from experiment workspace operations.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ExperimentError {
    /// Unknown experiment id (promotion/review against a dropped record).
    #[error("experiment not found: {0}")]
    NotFound(String),
    /// The operation was cancelled.
    #[error("experiment operation cancelled")]
    Cancelled,
    /// A filesystem I/O failure.
    #[error("experiment I/O failed for {path}: {message}")]
    Io {
        /// The path being handled.
        path: String,
        /// The I/O failure.
        message: String,
    },
    /// A git worktree operation failed.
    #[error("experiment git worktree failed: {0}")]
    Git(String),
    /// A size/count bound refused the operation (fail-closed, never partial).
    #[error("experiment budget refused: {0}")]
    Budget(String),
    /// Invalid roots, ids, or review binding.
    #[error("invalid experiment request: {0}")]
    Invalid(String),
    /// The record is `Promoted` or `Abandoned`: the requested transition is
    /// closed (promotion of a finished record is a recorded no-op or a
    /// refusal, never a second mutation).
    #[error("experiment {id} is already finished ({status}): {reason}")]
    AlreadyFinished {
        /// The experiment id.
        id: String,
        /// The terminal status.
        status: &'static str,
        /// Why the transition is closed.
        reason: String,
    },
    /// The review was computed against a different baseline digest, a
    /// different experiment, or the experiment moved since the review
    /// (re-review — never promote ghosts).
    #[error("stale experiment review for {id}: {detail}")]
    StaleReview {
        /// The experiment id.
        id: String,
        /// What went stale (names the mismatched digests).
        detail: String,
    },
    /// Foreign-owned apply targets aborted the promotion before any write
    /// (fail-closed, surfaced — never a silent overwrite).
    #[error("promotion refused: {conflicts:?} (already applied: {applied:?})")]
    OwnershipConflict {
        /// The contested paths with their owners.
        conflicts: Vec<PromotionOwnershipConflict>,
        /// Paths already applied before the abort (empty on the pre-check
        /// path — the common case).
        applied: Vec<String>,
    },
    /// A promotion write was denied by policy (fail-closed abort; the record
    /// stays `Active` so the failure is retriable; the applied subset is
    /// named).
    #[error("promotion write denied by policy for {path}: {rule} (already applied: {applied:?})")]
    PolicyDenied {
        /// The denied canonical path.
        path: String,
        /// The policy rule/reason.
        rule: String,
        /// Paths already applied before the denial.
        applied: Vec<String>,
    },
    /// A promotion write failed mid-flight (fail-closed abort; retriable).
    #[error("promotion write failed for {path}: {reason} (already applied: {applied:?})")]
    PromotionFailed {
        /// The failed canonical path.
        path: String,
        /// Why it failed.
        reason: String,
        /// Paths already applied before the failure.
        applied: Vec<String>,
    },
}

fn check_cancel(cancel: &CancellationToken) -> Result<(), ExperimentError> {
    if cancel.is_cancelled() {
        Err(ExperimentError::Cancelled)
    } else {
        Ok(())
    }
}

/// Unix epoch milliseconds (UTC).
#[must_use]
pub fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

/// Char-boundary-safe truncation for captured child stderr.
fn truncate_chars(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let mut out: String = text.chars().take(limit).collect();
    out.push_str("…(truncated)");
    out
}

// ---------------------------------------------------------------------------
// Pure promotion logic (no I/O — the conflict matrix, fully unit-tested)
// ---------------------------------------------------------------------------

/// Observed content state of one path: content hash when present.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObservedState {
    /// The file exists with this blake3 hex.
    Present {
        /// Blake3 hex of the observed bytes.
        hash: String,
    },
    /// The file is absent.
    Absent,
}

impl ObservedState {
    /// The content hash (`None` for absence) for equality against a baseline
    /// hash (`None` = absent at baseline).
    #[must_use]
    pub fn hash_opt(&self) -> Option<&str> {
        match self {
            Self::Present { hash } => Some(hash.as_str()),
            Self::Absent => None,
        }
    }
}

/// One path's promotion verdict from the (baseline, host, experiment) triple.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PathVerdict {
    /// Both sides match the baseline: nothing to do.
    Unchanged,
    /// The experiment moved and the host did not: apply (write or delete).
    Apply,
    /// Both moved to the same bytes: no write needed.
    Converged,
    /// Both moved to different bytes (or the host moved under an
    /// experiment-side transition): refuse and record.
    Conflict,
    /// The experiment never touched the path but the host drifted: surface
    /// as a note, never write, never a conflict.
    HostDrift,
}

/// Classify one path from its baseline hash (`None` = absent at baseline),
/// host state, and experiment state.
///
/// The rule, exactly: experiment-changed (`experiment != baseline`) and
/// host-unchanged (`host == baseline`) applies; experiment-changed and
/// host-changed refuses unless host and experiment agree (converged);
/// experiment-untouched paths are never written.
fn classify_path(
    baseline_hash: Option<&str>,
    host: &ObservedState,
    experiment: &ObservedState,
) -> PathVerdict {
    let experiment_changed = experiment.hash_opt() != baseline_hash;
    let host_changed = host.hash_opt() != baseline_hash;
    match (experiment_changed, host_changed) {
        (false, false) => PathVerdict::Unchanged,
        (false, true) => PathVerdict::HostDrift,
        (true, false) => PathVerdict::Apply,
        (true, true) => {
            if host.hash_opt() == experiment.hash_opt() {
                PathVerdict::Converged
            } else {
                PathVerdict::Conflict
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Copy-budget rule (pure — fail-closed before any byte moves)
// ---------------------------------------------------------------------------

/// Evaluate the copy budget over an enumerated file list (`(canonical path,
/// byte size)`): count, summed-bytes, and per-file ceilings.
///
/// `Err(Budget)` names the ceiling hit. Called after enumeration and before
/// any copy, so a refusal never leaves a partial workspace claimed as
/// complete (creation cleans up and reports).
pub fn evaluate_copy_budget(files: &[(String, u64)]) -> Result<(), ExperimentError> {
    if files.len() > EXPERIMENT_MAX_FILES {
        return Err(ExperimentError::Budget(format!(
            "{} files exceed EXPERIMENT_MAX_FILES ({EXPERIMENT_MAX_FILES}): refusing a silently partial copy",
            files.len()
        )));
    }
    let mut total: u64 = 0;
    for (path, size) in files {
        if *size > EXPERIMENT_MAX_FILE_BYTES {
            return Err(ExperimentError::Budget(format!(
                "{path}: {size} bytes exceeds EXPERIMENT_MAX_FILE_BYTES ({EXPERIMENT_MAX_FILE_BYTES})"
            )));
        }
        total = total.saturating_add(*size);
        if total > EXPERIMENT_MAX_TOTAL_BYTES {
            return Err(ExperimentError::Budget(format!(
                "total {total} bytes exceeds EXPERIMENT_MAX_TOTAL_BYTES ({EXPERIMENT_MAX_TOTAL_BYTES}): refusing a silently partial copy"
            )));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Root validation + workspace guard
// ---------------------------------------------------------------------------

/// Validate creation roots: both absolute, neither nested in the other (a
/// workspace under the host would be re-enumerated by the next experiment;
/// a host under the workspace is a copy-onto-itself hazard), the workspace
/// fresh (refusing to reuse or merge into an existing dir), the host an
/// existing directory.
pub fn validate_roots(host_root: &Path, workspace_root: &Path) -> Result<(), ExperimentError> {
    if !host_root.is_absolute() || !workspace_root.is_absolute() {
        return Err(ExperimentError::Invalid(
            "host and workspace roots must both be absolute".to_string(),
        ));
    }
    if host_root == workspace_root {
        return Err(ExperimentError::Invalid("host and workspace roots must differ".to_string()));
    }
    if workspace_root.starts_with(host_root) {
        return Err(ExperimentError::Invalid(
            "workspace root must not nest under the host root (it would be re-enumerated by the next baseline)".to_string(),
        ));
    }
    if host_root.starts_with(workspace_root) {
        return Err(ExperimentError::Invalid(
            "host root must not nest under the workspace root".to_string(),
        ));
    }
    if workspace_root.exists() {
        return Err(ExperimentError::Invalid(format!(
            "workspace root {} already exists: refusing to reuse or merge into an existing directory",
            workspace_root.display()
        )));
    }
    if !host_root.is_dir() {
        return Err(ExperimentError::Invalid(format!(
            "host root {} is not an existing directory",
            host_root.display()
        )));
    }
    Ok(())
}

/// Guard for workspace-relative tool paths: reduce `raw` to its canonical
/// workspace-relative key, or `None` when it escapes (absolute host path,
/// `..` above the root, or empty). Absolute-path and shell-raw inputs bypass
/// this guard by construction — that is the documented host-mount/absolute-
/// path limit, not a gap in this function.
#[must_use]
pub fn workspace_contains(workspace_root: &Path, raw: &str) -> Option<String> {
    crate::tool_facts::canonical_project_path(workspace_root, raw)
}

// ---------------------------------------------------------------------------
// Disk I/O helpers (bounded, cancellation-checked)
// ---------------------------------------------------------------------------

/// One enumerated host file: canonical path plus byte size (budgets run on
/// sizes before any content moves).
#[derive(Debug, Clone)]
struct EnumeratedFile {
    canonical: String,
    size: u64,
}

/// Iterative live-disk enumeration of `host_root`: regular files as
/// forward-slash canonical paths with sizes, excluded subtrees pruned (and
/// recorded once at the pruned root), symlinks skipped and recorded (never
/// followed).
///
/// This is the dirty-baseline substrate: it reads the directory tree, never
/// git state — tracked, dirty, and untracked files are uniform inputs.
async fn enumerate_host_files(
    host_root: &Path,
    cancel: &CancellationToken,
) -> Result<(Vec<EnumeratedFile>, Vec<ExperimentExclusion>), ExperimentError> {
    // `spawn_blocking`: the walk is synchronous std::fs (callers already run
    // off the async executor; an explicit thread keeps it off executor
    // threads, per the blocking-work rule).
    let root = host_root.to_path_buf();
    tokio::task::spawn_blocking(move || enumerate_host_files_blocking(&root))
        .await
        .map_err(|error| ExperimentError::Io {
            path: host_root.display().to_string(),
            message: error.to_string(),
        })?
        .map(|(files, excluded)| {
            // Surface cancellation observed anywhere in the walk as
            // Cancelled rather than a partial tree (fail-closed).
            if cancel.is_cancelled() {
                Err(ExperimentError::Cancelled)
            } else {
                Ok((files, excluded))
            }
        })?
}

/// Explicit stack, no recursion (bounded memory on deep trees).
fn enumerate_host_files_blocking(
    host_root: &Path,
) -> Result<(Vec<EnumeratedFile>, Vec<ExperimentExclusion>), ExperimentError> {
    let mut files = Vec::new();
    let mut excluded = Vec::new();
    let mut total: u64 = 0;
    let mut stack: Vec<PathBuf> = vec![host_root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let read_dir = match std::fs::read_dir(&dir) {
            Ok(read_dir) => read_dir,
            Err(error) => {
                return Err(ExperimentError::Io {
                    path: dir.display().to_string(),
                    message: error.to_string(),
                });
            }
        };
        for entry in read_dir {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    return Err(ExperimentError::Io {
                        path: dir.display().to_string(),
                        message: error.to_string(),
                    });
                }
            };
            // `file_type` does not follow symlinks: symlinks surface here
            // and are skipped below, never traversed.
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(error) => {
                    return Err(ExperimentError::Io {
                        path: entry.path().display().to_string(),
                        message: error.to_string(),
                    });
                }
            };
            if file_type.is_symlink() {
                excluded.push(ExperimentExclusion {
                    class: ExperimentExclusionClass::Symlink,
                    path: Some(relative_canonical(host_root, &entry.path())?),
                    reason: "symlink skipped (never followed or copied: a host-pointing link would defeat isolation silently)".to_string(),
                });
                continue;
            }
            let path = entry.path();
            let name =
                path.file_name().map(|name| name.to_string_lossy().to_string()).unwrap_or_default();
            if EXPERIMENT_EXCLUDED_DIR_NAMES.contains(&name.as_str()) {
                excluded.push(ExperimentExclusion {
                    class: ExperimentExclusionClass::GeneratedCache,
                    path: Some(relative_canonical(host_root, &path)?),
                    reason: format!(
                        "{name}: generated output / dependency cache / control dir — outside the baseline, copy, and promotion in both directions"
                    ),
                });
                continue;
            }
            if file_type.is_dir() {
                stack.push(path);
            } else if file_type.is_file() {
                let size = match entry.metadata() {
                    Ok(metadata) => metadata.len(),
                    Err(error) => {
                        return Err(ExperimentError::Io {
                            path: path.display().to_string(),
                            message: error.to_string(),
                        });
                    }
                };
                let canonical = relative_canonical(host_root, &path)?;
                if is_excluded_canonical(&canonical) {
                    continue;
                }
                files.push(EnumeratedFile { canonical, size });
                total = total.saturating_add(size);
                // Early fail-closed: no point enumerating a tree already
                // over budget (also bounds this walk's own memory).
                if files.len() > EXPERIMENT_MAX_FILES || total > EXPERIMENT_MAX_TOTAL_BYTES {
                    evaluate_copy_budget(
                        &files
                            .iter()
                            .map(|file| (file.canonical.clone(), file.size))
                            .collect::<Vec<_>>(),
                    )?;
                }
            }
        }
    }
    files.sort_by(|a, b| a.canonical.cmp(&b.canonical));
    Ok((files, excluded))
}

/// Forward-slash path of `path` relative to `root` (`Err` — never a guess —
/// when the prefix does not hold, which cannot happen for walked entries but
/// must still be total).
fn relative_canonical(root: &Path, path: &Path) -> Result<String, ExperimentError> {
    let relative = path.strip_prefix(root).map_err(|_| {
        ExperimentError::Invalid(format!("path {} escapes root {}", path.display(), root.display()))
    })?;
    let mut parts = Vec::new();
    for component in relative.components() {
        use std::path::Component;
        match component {
            Component::Normal(part) => parts.push(part.to_string_lossy().to_string()),
            // Walked entries never produce these; refuse rather than
            // normalize something unexpected.
            Component::ParentDir
            | Component::RootDir
            | Component::Prefix(_)
            | Component::CurDir => {
                return Err(ExperimentError::Invalid(format!(
                    "path {} has a non-normal component under root {}",
                    path.display(),
                    root.display()
                )));
            }
        }
    }
    if parts.is_empty() {
        return Err(ExperimentError::Invalid(format!(
            "path {} is the root itself",
            path.display()
        )));
    }
    Ok(parts.join("/"))
}

/// Stream-hash one file (64 KiB buffer — never loads the whole file for a
/// hash). Returns `(blake3 hex, byte size)`.
async fn hash_file(path: &Path) -> Result<(String, u64), std::io::Error> {
    // Synchronous read on a blocking thread would be ideal; the files here
    // are already size-bounded, but hashing still streams so a 64 MiB file
    // never fully materializes for identity alone.
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || hash_file_blocking(&path))
        .await
        .map_err(|error| std::io::Error::other(error.to_string()))?
}

fn hash_file_blocking(path: &Path) -> Result<(String, u64), std::io::Error> {
    use std::io::Read as _;
    let file = std::fs::File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut size: u64 = 0;
    let mut reader = std::io::BufReader::with_capacity(EXPERIMENT_HASH_BUF_BYTES, file);
    let mut buf = vec![0u8; EXPERIMENT_HASH_BUF_BYTES];
    loop {
        let read = reader.read(&mut buf)?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
        size = size.saturating_add(read as u64);
    }
    Ok((hasher.finalize().to_hex().to_string(), size))
}

/// Observe one path's current state under `root`: streamed hash when present,
/// `Absent` on missing. Any other I/O failure is an error (fail-closed —
/// an unreadable host file is never promoted over or conflict-guessed).
async fn observe_path(root: &Path, canonical: &str) -> Result<ObservedState, ExperimentError> {
    let absolute = root.join(canonical);
    match tokio::fs::symlink_metadata(&absolute).await {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                return Err(ExperimentError::Invalid(format!(
                    "{canonical}: path is a symlink at promotion time; refusing to promote through a link"
                )));
            }
            if !metadata.file_type().is_file() {
                return Err(ExperimentError::Invalid(format!(
                    "{canonical}: path is not a regular file at promotion time"
                )));
            }
            if metadata.len() > EXPERIMENT_MAX_FILE_BYTES {
                // Oversized is classified by the caller via the size; here
                // report the state without hashing unbounded bytes.
                return Err(ExperimentError::Budget(format!(
                    "{canonical}: {} bytes exceeds EXPERIMENT_MAX_FILE_BYTES ({EXPERIMENT_MAX_FILE_BYTES})",
                    metadata.len()
                )));
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ObservedState::Absent);
        }
        Err(error) => {
            return Err(ExperimentError::Io {
                path: absolute.display().to_string(),
                message: error.to_string(),
            });
        }
    }
    match hash_file(&absolute).await {
        Ok((hash, _)) => Ok(ObservedState::Present { hash }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(ObservedState::Absent),
        Err(error) => Err(ExperimentError::Io {
            path: absolute.display().to_string(),
            message: error.to_string(),
        }),
    }
}

// ---------------------------------------------------------------------------
// Creation
// ---------------------------------------------------------------------------

/// Create a managed experiment workspace: enumerate the live-disk baseline,
/// materialize the workspace per `mode`, and return the runtime-authored
/// record.
///
/// * `host_root` / `workspace_root` must both be absolute; the workspace must
///   be fresh (see [`validate_roots`]).
/// * `base_revision` selects the worktree base in
///   [`ExperimentMode::GitWorktree`] (`None` = `HEAD`); ignored in `Copy`.
/// * Budgets fail closed before any byte moves; a mid-copy failure
///   best-effort removes the half-built workspace and reports the I/O error
///   (the record is never returned, so nothing claims the partial tree).
pub async fn create_experiment(
    host_root: &Path,
    workspace_root: &Path,
    mode: ExperimentMode,
    base_revision: Option<&str>,
    created_by: &str,
    cancel: &CancellationToken,
) -> Result<ExperimentRecord, ExperimentError> {
    check_cancel(cancel)?;
    if created_by.is_empty() {
        return Err(ExperimentError::Invalid(
            "created_by must name the requesting agent".to_string(),
        ));
    }
    validate_roots(host_root, workspace_root)?;
    check_cancel(cancel)?;

    let (enumerated, mut excluded) = enumerate_host_files(host_root, cancel).await?;
    check_cancel(cancel)?;
    evaluate_copy_budget(
        &enumerated.iter().map(|file| (file.canonical.clone(), file.size)).collect::<Vec<_>>(),
    )?;

    // Advisory audit trail (never a scope bound): the host's
    // content-addressed generation at creation.
    let host_entries = crate::workspace_snapshot::capture_workspace_snapshot(host_root, cancel);
    let baseline_generation = crate::workspace_snapshot::generation_for(&host_entries);
    check_cancel(cancel)?;

    let materialize = match mode {
        ExperimentMode::Copy => {
            materialize_copy(host_root, workspace_root, &enumerated, cancel).await
        }
        ExperimentMode::GitWorktree => {
            materialize_worktree(host_root, workspace_root, base_revision, &enumerated, cancel)
                .await
        }
    };
    let mut baseline = match materialize {
        Ok(baseline) => baseline,
        Err(error) => {
            cleanup_failed_workspace(host_root, workspace_root, mode, cancel).await;
            return Err(error);
        }
    };
    baseline.sort_by(|a, b| a.canonical_path.cmp(&b.canonical_path));

    // The worktree's own `.git` control file (gitdir pointer) is real and
    // must be named in the honesty list even though the component exclusion
    // already kept it out of every byte path.
    if mode == ExperimentMode::GitWorktree {
        excluded.push(ExperimentExclusion {
            class: ExperimentExclusionClass::WorktreeControl,
            path: Some(".git".to_string()),
            reason: "worktree gitdir pointer: version-control plumbing, never copied, overlaid, or promoted".to_string(),
        });
    }

    Ok(ExperimentRecord {
        schema_version: EXPERIMENT_SCHEMA_VERSION,
        experiment_id: Ulid::new().to_string(),
        host_root: host_root.to_path_buf(),
        workspace_root: workspace_root.to_path_buf(),
        mode,
        created_by: created_by.to_owned(),
        created_at_ms: now_millis(),
        baseline,
        baseline_generation,
        excluded,
        status: ExperimentStatus::Active,
        last_report: None,
    })
}

/// Copy-mode materialization: read each baselined file (size-bounded),
/// write it under the workspace, and anchor its hash. A file vanishing
/// mid-capture aborts creation (fail-closed — a point-in-time capture with a
/// hole is not a baseline).
async fn materialize_copy(
    host_root: &Path,
    workspace_root: &Path,
    enumerated: &[EnumeratedFile],
    cancel: &CancellationToken,
) -> Result<Vec<BaselineEntry>, ExperimentError> {
    tokio::fs::create_dir_all(workspace_root).await.map_err(|error| ExperimentError::Io {
        path: workspace_root.display().to_string(),
        message: error.to_string(),
    })?;
    let mut baseline = Vec::with_capacity(enumerated.len());
    for file in enumerated {
        check_cancel(cancel)?;
        let source = host_root.join(&file.canonical);
        let bytes = match tokio::fs::read(&source).await {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(ExperimentError::Io {
                    path: source.display().to_string(),
                    message: format!(
                        "file vanished between enumeration and copy ({error}); aborting creation rather than anchoring a partial baseline"
                    ),
                });
            }
            Err(error) => {
                return Err(ExperimentError::Io {
                    path: source.display().to_string(),
                    message: error.to_string(),
                });
            }
        };
        if bytes.len() as u64 > EXPERIMENT_MAX_FILE_BYTES {
            return Err(ExperimentError::Budget(format!(
                "{}: {} bytes exceeds EXPERIMENT_MAX_FILE_BYTES ({EXPERIMENT_MAX_FILE_BYTES})",
                file.canonical,
                bytes.len()
            )));
        }
        let destination = workspace_root.join(&file.canonical);
        if let Some(parent) = destination.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|error| ExperimentError::Io {
                path: parent.display().to_string(),
                message: error.to_string(),
            })?;
        }
        tokio::fs::write(&destination, &bytes).await.map_err(|error| ExperimentError::Io {
            path: destination.display().to_string(),
            message: error.to_string(),
        })?;
        baseline.push(BaselineEntry {
            canonical_path: file.canonical.clone(),
            baseline_hash: blake3::hash(&bytes).to_hex().to_string(),
            baseline_len: bytes.len() as u64,
        });
    }
    Ok(baseline)
}

/// Worktree-mode materialization: detached worktree at the base revision,
/// then overlay of the live-disk baseline (dirty + untracked carried in).
async fn materialize_worktree(
    host_root: &Path,
    workspace_root: &Path,
    base_revision: Option<&str>,
    enumerated: &[EnumeratedFile],
    cancel: &CancellationToken,
) -> Result<Vec<BaselineEntry>, ExperimentError> {
    let base = base_revision.unwrap_or("HEAD").to_owned();
    git_worktree_add(host_root, workspace_root, &base, cancel).await?;
    check_cancel(cancel)?;
    // Overlay: every enumerated file whose worktree bytes differ (or are
    // absent) is carried over from the host. Same bytes, same baseline hash
    // as copy mode — the two modes anchor identically.
    let mut baseline = Vec::with_capacity(enumerated.len());
    for file in enumerated {
        check_cancel(cancel)?;
        let source = host_root.join(&file.canonical);
        let bytes = match tokio::fs::read(&source).await {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(ExperimentError::Io {
                    path: source.display().to_string(),
                    message: format!(
                        "file vanished between enumeration and overlay ({error}); aborting creation rather than anchoring a partial baseline"
                    ),
                });
            }
            Err(error) => {
                return Err(ExperimentError::Io {
                    path: source.display().to_string(),
                    message: error.to_string(),
                });
            }
        };
        if bytes.len() as u64 > EXPERIMENT_MAX_FILE_BYTES {
            return Err(ExperimentError::Budget(format!(
                "{}: {} bytes exceeds EXPERIMENT_MAX_FILE_BYTES ({EXPERIMENT_MAX_FILE_BYTES})",
                file.canonical,
                bytes.len()
            )));
        }
        let current = workspace_root.join(&file.canonical);
        let needs_overlay = match tokio::fs::read(&current).await {
            Ok(current_bytes) => current_bytes != bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
            Err(error) => {
                return Err(ExperimentError::Io {
                    path: current.display().to_string(),
                    message: error.to_string(),
                });
            }
        };
        if needs_overlay {
            if let Some(parent) = current.parent() {
                tokio::fs::create_dir_all(parent).await.map_err(|error| ExperimentError::Io {
                    path: parent.display().to_string(),
                    message: error.to_string(),
                })?;
            }
            tokio::fs::write(&current, &bytes).await.map_err(|error| ExperimentError::Io {
                path: current.display().to_string(),
                message: error.to_string(),
            })?;
        }
        baseline.push(BaselineEntry {
            canonical_path: file.canonical.clone(),
            baseline_hash: blake3::hash(&bytes).to_hex().to_string(),
            baseline_len: bytes.len() as u64,
        });
    }
    Ok(baseline)
}

/// Run `git -C <host> worktree add --detach <workspace> <base>` on a blocking
/// thread (synchronous child wait stays off executor threads). Cancellation
/// is honored before (and the caller re-checks after) — there is no
/// mid-`wait` abort, matching the codebase's bounded-spawn posture for
/// short-lived plumbing commands.
async fn git_worktree_add(
    host_root: &Path,
    workspace_root: &Path,
    base: &str,
    cancel: &CancellationToken,
) -> Result<(), ExperimentError> {
    check_cancel(cancel)?;
    let host = host_root.to_path_buf();
    let workspace = workspace_root.to_path_buf();
    let base = base.to_owned();
    let output = tokio::task::spawn_blocking(move || {
        std::process::Command::new("git")
            .arg("-C")
            .arg(&host)
            .args(["worktree", "add", "--detach"])
            .arg(&workspace)
            .arg(&base)
            .output()
    })
    .await
    .map_err(|error| ExperimentError::Git(format!("git worktree spawn failed: {error}")))?
    .map_err(|error| ExperimentError::Git(format!("git worktree could not run: {error}")))?;
    if !output.status.success() {
        let stderr =
            truncate_chars(&String::from_utf8_lossy(&output.stderr), EXPERIMENT_GIT_STDERR_LIMIT);
        return Err(ExperimentError::Git(format!(
            "git worktree add --detach failed (not a git repo, unknown revision, or git missing?): {stderr}"
        )));
    }
    Ok(())
}

/// Best-effort removal of a half-built workspace after a failed creation.
/// Copy mode removes the directory; worktree mode additionally unregisters
/// the worktree. Errors are swallowed (the creation error is the one that
/// matters) — the caller documents that a failed creation claims nothing.
async fn cleanup_failed_workspace(
    host_root: &Path,
    workspace_root: &Path,
    mode: ExperimentMode,
    _cancel: &CancellationToken,
) {
    if mode == ExperimentMode::GitWorktree {
        let host = host_root.to_path_buf();
        let workspace = workspace_root.to_path_buf();
        let _ = tokio::task::spawn_blocking(move || {
            std::process::Command::new("git")
                .arg("-C")
                .arg(&host)
                .args(["worktree", "remove", "--force"])
                .arg(&workspace)
                .output()
        })
        .await;
    }
    let _ = tokio::fs::remove_dir_all(workspace_root).await;
}

// ---------------------------------------------------------------------------
// Review (explicit, host-touching-nothing)
// ---------------------------------------------------------------------------

/// Content digest of observed experiment states: blake3 over the sorted
/// (path, hash-or-absent) pairs. The review binding's second half — any
/// experiment-side change (content, create, delete) moves it, so promotion
/// re-observes and refuses when the experiment moved since the review.
#[must_use]
pub fn experiment_state_digest(states: &BTreeMap<String, ObservedState>) -> String {
    let mut hasher = blake3::Hasher::new();
    for (path, state) in states {
        hasher.update(path.as_bytes());
        match state {
            ObservedState::Present { hash } => {
                hasher.update(&[1]);
                hasher.update(hash.as_bytes());
            }
            ObservedState::Absent => {
                hasher.update(&[0]);
            }
        }
    }
    hasher.finalize().to_hex().to_string()
}

/// Walk the experiment workspace with the SAME exclusions as creation (a
/// build output appearing after creation stays out-of-claim exactly as at
/// baseline) and observe every in-claim path's state.
///
/// Returns the state map plus oversized paths (recorded as `Absent` in the
/// map so they surface in `oversized`, never as deletions — promotion
/// refuses them regardless of host state). Shared by review and promotion
/// so both classify identical observations.
async fn scan_experiment_states(
    workspace_root: &Path,
    cancel: &CancellationToken,
) -> Result<(BTreeMap<String, ObservedState>, Vec<String>), ExperimentError> {
    let mut states: BTreeMap<String, ObservedState> = BTreeMap::new();
    let mut oversized = Vec::new();
    let mut stack: Vec<PathBuf> = vec![workspace_root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        check_cancel(cancel)?;
        let mut stream = match tokio::fs::read_dir(&dir).await {
            Ok(stream) => stream,
            Err(error) => {
                return Err(ExperimentError::Io {
                    path: dir.display().to_string(),
                    message: error.to_string(),
                });
            }
        };
        let mut entries = Vec::new();
        loop {
            match stream.next_entry().await {
                Ok(Some(entry)) => entries.push(entry),
                Ok(None) => break,
                Err(error) => {
                    return Err(ExperimentError::Io {
                        path: dir.display().to_string(),
                        message: error.to_string(),
                    });
                }
            }
        }
        for entry in entries {
            check_cancel(cancel)?;
            let file_type = match entry.file_type().await {
                Ok(file_type) => file_type,
                Err(error) => {
                    return Err(ExperimentError::Io {
                        path: entry.path().display().to_string(),
                        message: error.to_string(),
                    });
                }
            };
            if file_type.is_symlink() {
                continue;
            }
            let path = entry.path();
            let name =
                path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            if EXPERIMENT_EXCLUDED_DIR_NAMES.contains(&name.as_str()) {
                continue;
            }
            if file_type.is_dir() {
                stack.push(path);
            } else if file_type.is_file() {
                let canonical = match relative_canonical(workspace_root, &path) {
                    Ok(canonical) => canonical,
                    Err(_) => continue,
                };
                if is_excluded_canonical(&canonical) {
                    continue;
                }
                let size = match entry.metadata().await {
                    Ok(metadata) => metadata.len(),
                    Err(error) => {
                        return Err(ExperimentError::Io {
                            path: path.display().to_string(),
                            message: error.to_string(),
                        });
                    }
                };
                if size > EXPERIMENT_MAX_FILE_BYTES {
                    oversized.push(canonical.clone());
                    states.insert(canonical, ObservedState::Absent);
                    continue;
                }
                match hash_file(&path).await {
                    Ok((hash, _)) => {
                        states.insert(canonical, ObservedState::Present { hash });
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(ExperimentError::Io {
                            path: path.display().to_string(),
                            message: error.to_string(),
                        });
                    }
                }
            }
        }
    }
    oversized.sort();
    Ok((states, oversized))
}

/// Diff the experiment workspace against the record's baseline: the explicit
/// review artifact promotion requires. Read-only on both trees — review
/// never touches the host.
///
/// The returned diff carries the record's current
/// [`ExperimentRecord::baseline_digest`] plus the observed
/// [`experiment_state_digest`]; promotion binds on both, so a review is
/// fresh only until the baseline, the experiment id, or the experiment
/// states change.
pub async fn review_experiment(
    record: &ExperimentRecord,
    cancel: &CancellationToken,
) -> Result<ExperimentDiff, ExperimentError> {
    check_cancel(cancel)?;
    if record.status != ExperimentStatus::Active {
        return Err(ExperimentError::AlreadyFinished {
            id: record.experiment_id.clone(),
            status: status_str(record.status),
            reason: "only an Active experiment can be reviewed".to_string(),
        });
    }
    let (states, oversized) = scan_experiment_states(&record.workspace_root, cancel).await?;
    let experiment_digest = experiment_state_digest(&states);

    let baseline: BTreeMap<&str, &BaselineEntry> =
        record.baseline.iter().map(|entry| (entry.canonical_path.as_str(), entry)).collect();
    let mut changed = Vec::new();
    let mut created = Vec::new();
    let mut deleted = Vec::new();
    let mut unchanged = 0usize;
    for entry in &record.baseline {
        check_cancel(cancel)?;
        match states.get(entry.canonical_path.as_str()) {
            Some(ObservedState::Present { hash }) if hash == &entry.baseline_hash => {
                unchanged += 1;
            }
            Some(ObservedState::Present { .. }) => changed.push(entry.canonical_path.clone()),
            // Oversized experiment files were recorded as `Absent` above so
            // they surface in `oversized`, never as deletions.
            Some(ObservedState::Absent) | None => {
                if oversized.iter().any(|path| path == &entry.canonical_path) {
                    changed.push(entry.canonical_path.clone());
                } else {
                    deleted.push(entry.canonical_path.clone());
                }
            }
        }
    }
    for (path, state) in &states {
        if !baseline.contains_key(path.as_str()) && *state != ObservedState::Absent {
            created.push(path.clone());
        }
    }
    changed.sort();
    created.sort();
    deleted.sort();
    Ok(ExperimentDiff {
        experiment_id: record.experiment_id.clone(),
        baseline_digest: record.baseline_digest(),
        experiment_digest,
        changed,
        created,
        deleted,
        unchanged,
        oversized,
    })
}

fn status_str(status: ExperimentStatus) -> &'static str {
    match status {
        ExperimentStatus::Active => "active",
        ExperimentStatus::Promoted => "promoted",
        ExperimentStatus::Abandoned => "abandoned",
    }
}

// ---------------------------------------------------------------------------
// Promotion (explicit, authorized, conflict-detecting)
// ---------------------------------------------------------------------------

/// One experiment-side apply operation, resolved before any write.
#[derive(Debug, Clone)]
enum ApplyOp {
    /// Overwrite a host file with these experiment bytes.
    Write {
        /// Canonical path.
        path: String,
        /// Experiment bytes (size-bounded at scan time).
        bytes: Vec<u8>,
    },
    /// Delete a host file (absent in experiment, present at baseline).
    Delete {
        /// Canonical path.
        path: String,
    },
}

/// Promote the reviewed experiment changes into the host workspace.
///
/// * `review` must be fresh ([`review_experiment`] output whose digest still
///   matches the record — otherwise [`ExperimentError::StaleReview`] and the
///   host is untouched).
/// * Already-`Promoted` records return [`PromotionOutcome::AlreadyPromoted`]
///   (recorded no-op carrying the first report); `Abandoned` records refuse.
/// * Host writes go through `gate` ([`WriteGate::submit`], one gated
///   `filesystem` op per path with the baseline hash stamped as the gate
///   `base_version`); `host_session` must be rooted at the record's host
///   root; `promoter` is the attributed agent.
/// * Ownership conflicts abort before any write; policy denials and write
///   failures abort with the applied subset named (the record stays `Active`
///   — every abort is retriable).
pub async fn promote_experiment(
    record: &mut ExperimentRecord,
    review: &ExperimentDiff,
    gate: &WriteGate,
    host_session: &SessionContext,
    promoter: &str,
    cancel: &CancellationToken,
) -> Result<PromotionOutcome, ExperimentError> {
    check_cancel(cancel)?;
    if promoter.is_empty() {
        return Err(ExperimentError::Invalid("promoter must name the promoting agent".to_string()));
    }
    match record.status {
        ExperimentStatus::Promoted => {
            return Ok(PromotionOutcome::AlreadyPromoted {
                report: record.last_report.clone().unwrap_or_default(),
            });
        }
        ExperimentStatus::Abandoned => {
            return Err(ExperimentError::AlreadyFinished {
                id: record.experiment_id.clone(),
                status: status_str(record.status),
                reason: "an abandoned experiment never promotes".to_string(),
            });
        }
        ExperimentStatus::Active => {}
    }
    if review.experiment_id != record.experiment_id {
        return Err(ExperimentError::Invalid(format!(
            "review targets experiment {} but the record is {}",
            review.experiment_id, record.experiment_id
        )));
    }
    let digest = record.baseline_digest();
    if review.baseline_digest != digest {
        return Err(ExperimentError::StaleReview {
            id: record.experiment_id.clone(),
            detail: format!(
                "review baseline digest {} != record baseline digest {digest}: re-review before promoting",
                review.baseline_digest
            ),
        });
    }
    if host_session.project_dir != record.host_root {
        return Err(ExperimentError::Invalid(format!(
            "host session root {} != experiment host root {}: promotion only writes into its own host",
            host_session.project_dir.display(),
            record.host_root.display()
        )));
    }

    // Re-observe the experiment with the SAME scan review uses: the review
    // is the human-readable artifact, this scan is the machine binding. When
    // the experiment moved since the review, the digests differ and
    // promotion refuses (stale review) with the host untouched.
    let (experiment_states, scanned_oversized) =
        scan_experiment_states(&record.workspace_root, cancel).await?;
    let experiment_digest = experiment_state_digest(&experiment_states);
    if review.experiment_digest != experiment_digest {
        return Err(ExperimentError::StaleReview {
            id: record.experiment_id.clone(),
            detail: format!(
                "review experiment digest {} != current experiment digest {experiment_digest}: the experiment moved since the review — re-review before promoting",
                review.experiment_digest
            ),
        });
    }

    // Scan universe: every baselined path plus every currently observed
    // experiment path (a file created after the review is discovered here —
    // and rejected above as a digest mismatch — never silently promoted).
    let mut interesting: BTreeSet<String> = BTreeSet::new();
    for entry in &record.baseline {
        interesting.insert(entry.canonical_path.clone());
    }
    for path in experiment_states.keys() {
        interesting.insert(path.clone());
    }
    // Bound the union before touching it (baseline ≤ MAX_FILES by
    // construction; the scan is the same bounded walk review uses — the cap
    // below is defense-in-depth with a clear error).
    if interesting.len() > EXPERIMENT_MAX_FILES * 2 {
        return Err(ExperimentError::Budget(format!(
            "promotion scan of {} paths exceeds twice EXPERIMENT_MAX_FILES: refusing",
            interesting.len()
        )));
    }

    let baseline_map: BTreeMap<&str, &BaselineEntry> =
        record.baseline.iter().map(|entry| (entry.canonical_path.as_str(), entry)).collect();
    let mut report = PromotionReport::default();
    let mut apply_paths: Vec<String> = Vec::new();

    for path in &interesting {
        check_cancel(cancel)?;
        // Defense-in-depth containment: promotion only ever addresses
        // canonical baseline/scan paths (workspace-relative by
        // construction). Anything unresolvable under the host root is an
        // internal invariant violation, never a write.
        if workspace_contains(&record.host_root, path).as_deref() != Some(path.as_str()) {
            return Err(ExperimentError::Invalid(format!(
                "{path}: promotion path escapes the host root"
            )));
        }
        let baseline_hash: Option<&str> =
            baseline_map.get(path.as_str()).map(|entry| entry.baseline_hash.as_str());
        let host = observe_promotion_state(&record.host_root, path).await?;
        let experiment = experiment_states.get(path).cloned().unwrap_or(ObservedState::Absent);
        match classify_path(baseline_hash, &host, &experiment) {
            PathVerdict::Unchanged => {}
            PathVerdict::HostDrift => {
                report.host_drift_untouched.push(path.clone());
            }
            PathVerdict::Converged => {
                report.already_matching.push(path.clone());
                report.notes.push(format!(
                    "{path}: host already holds the experiment bytes (converged — no write)"
                ));
            }
            PathVerdict::Conflict => {
                report.conflicts.push(PromotionConflict {
                    path: path.clone(),
                    baseline_hash: baseline_hash.map(str::to_owned),
                    host_hash: host.hash_opt().map(str::to_owned),
                    experiment_hash: experiment.hash_opt().map(str::to_owned),
                    reason: "host changed since the baseline: refusing to overwrite host bytes (resolve on the host or in a fresh experiment, then promote again)".to_string(),
                });
            }
            PathVerdict::Apply => {
                apply_paths.push(path.clone());
            }
        }
    }

    // Oversized experiment-side files refuse regardless of host state
    // (promotion never loads unbounded bytes) — surfaced beside conflicts.
    // The set comes from the live scan (not the review's word — same
    // observations the digest binding verified).
    for path in &scanned_oversized {
        if !apply_paths.iter().any(|apply| apply == path) {
            continue;
        }
        apply_paths.retain(|apply| apply != path);
        report.refused_oversized.push(path.clone());
        report.notes.push(format!(
            "{path}: experiment file exceeds EXPERIMENT_MAX_FILE_BYTES ({EXPERIMENT_MAX_FILE_BYTES}): refused (carry it to the host by hand)"
        ));
    }

    // Ownership pre-check over the whole apply set BEFORE any write
    // (fail-closed, mirroring restore's Phase 1): unowned passes (gated
    // writes auto-acquire), self-owned passes, foreign-owned aborts.
    let mut ownership_conflicts = Vec::new();
    for path in &apply_paths {
        if let Some(owner) = gate.ownership_owner(path) {
            if owner != promoter {
                ownership_conflicts.push(PromotionOwnershipConflict { path: path.clone(), owner });
            }
        }
    }
    if !ownership_conflicts.is_empty() {
        // The pre-scan already embedded any host-drift conflicts found
        // above into the record's last report side-channel? No — the abort
        // happens before the report is stored. Surface conflicts found so
        // far inside the error's debug shape is lossy; instead record the
        // effects observed before the abort in `last_report` without
        // transitioning status (retriable, fully auditable).
        let mut abort_report = report.clone();
        abort_report.notes.push(format!(
            "promotion aborted before any write: {} foreign-owned path(s)",
            ownership_conflicts.len()
        ));
        record.last_report = Some(abort_report);
        return Err(ExperimentError::OwnershipConflict {
            conflicts: ownership_conflicts,
            applied: Vec::new(),
        });
    }

    // Resolve apply operations (experiment bytes for writes — size-bounded;
    // deletes need no bytes). Sorted path order: deterministic.
    apply_paths.sort();
    let mut ops: Vec<ApplyOp> = Vec::with_capacity(apply_paths.len());
    for path in &apply_paths {
        check_cancel(cancel)?;
        let experiment_file = record.workspace_root.join(path);
        match tokio::fs::read(&experiment_file).await {
            Ok(bytes) => {
                if bytes.len() as u64 > EXPERIMENT_MAX_FILE_BYTES {
                    report.refused_oversized.push(path.clone());
                    continue;
                }
                ops.push(ApplyOp::Write { path: path.clone(), bytes });
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                ops.push(ApplyOp::Delete { path: path.clone() });
            }
            Err(error) => {
                return Err(ExperimentError::Io {
                    path: experiment_file.display().to_string(),
                    message: error.to_string(),
                });
            }
        }
    }

    // Apply through the gate, one path at a time, re-verifying the host
    // immediately before each submit (closes the review-to-promote TOCTOU to
    // one re-read-to-submit per path; a still-later race falls back to the
    // gate's hunk policy — see the module docs).
    let mut applied: Vec<String> = Vec::new();
    for op in &ops {
        check_cancel(cancel)?;
        let path = match op {
            ApplyOp::Write { path, .. } => path,
            ApplyOp::Delete { path } => path,
        };
        let baseline_hash: Option<&str> =
            baseline_map.get(path.as_str()).map(|entry| entry.baseline_hash.as_str());
        let host = observe_promotion_state(&record.host_root, path).await?;
        if host.hash_opt() != baseline_hash {
            // The host moved under us after the scan: convert to a
            // recorded conflict (same shape as scan-time conflicts).
            report.conflicts.push(PromotionConflict {
                path: path.clone(),
                baseline_hash: baseline_hash.map(str::to_owned),
                host_hash: host.hash_opt().map(str::to_owned),
                experiment_hash: match op {
                    ApplyOp::Write { bytes, .. } => {
                        Some(blake3::hash(bytes).to_hex().to_string())
                    }
                    ApplyOp::Delete { .. } => None,
                },
                reason: "host changed between the promotion scan and this write: refusing (re-review and promote again)".to_string(),
            });
            continue;
        }
        let request = promotion_request(
            &record.experiment_id,
            promoter,
            host_session,
            path,
            op,
            baseline_hash,
        );
        match gate.submit(request, cancel.clone()).await {
            Ok(_) => {
                applied.push(path.clone());
                match op {
                    ApplyOp::Write { .. } => {
                        if baseline_hash.is_none() {
                            report.recreated.push(path.clone());
                        } else {
                            report.promoted.push(path.clone());
                        }
                    }
                    ApplyOp::Delete { .. } => {
                        report.deleted.push(path.clone());
                    }
                }
            }
            Err(GateError::Conflict { reason, .. }) => {
                // Same-hunk collision or lock contention at submit: the
                // gate refused — record it as a conflict, continue with
                // the remaining paths (Partial outcome below).
                let experiment_hash = match op {
                    ApplyOp::Write { bytes, .. } => Some(blake3::hash(bytes).to_hex().to_string()),
                    ApplyOp::Delete { .. } => None,
                };
                let host_now = observe_promotion_state(&record.host_root, path)
                    .await
                    .ok()
                    .and_then(|state| state.hash_opt().map(str::to_owned));
                report.conflicts.push(PromotionConflict {
                    path: path.clone(),
                    baseline_hash: baseline_hash.map(str::to_owned),
                    host_hash: host_now,
                    experiment_hash,
                    reason: format!("gate refused the promotion write: {reason}"),
                });
            }
            Err(GateError::OwnershipConflict { path: contested, owner, .. }) => {
                record.last_report = Some(report.clone());
                return Err(ExperimentError::OwnershipConflict {
                    conflicts: vec![PromotionOwnershipConflict { path: contested, owner }],
                    applied,
                });
            }
            Err(GateError::Denied { reason, .. }) => {
                record.last_report = Some(report.clone());
                return Err(ExperimentError::PolicyDenied {
                    path: path.clone(),
                    rule: reason,
                    applied,
                });
            }
            Err(GateError::Cancelled) => {
                record.last_report = Some(report.clone());
                return Err(ExperimentError::Cancelled);
            }
            Err(error) => {
                record.last_report = Some(report.clone());
                return Err(ExperimentError::PromotionFailed {
                    path: path.clone(),
                    reason: error.to_string(),
                    applied,
                });
            }
        }
    }

    report.promoted.sort();
    report.recreated.sort();
    report.deleted.sort();
    report.already_matching.sort();
    report.host_drift_untouched.sort();
    report.refused_oversized.sort();
    report.conflicts.sort_by(|a, b| a.path.cmp(&b.path));
    let clean = report.conflicts.is_empty() && report.refused_oversized.is_empty();
    if clean {
        record.status = ExperimentStatus::Promoted;
        record.last_report = Some(report.clone());
        Ok(PromotionOutcome::Promoted { report })
    } else {
        record.status = ExperimentStatus::Active;
        record.last_report = Some(report.clone());
        Ok(PromotionOutcome::Partial { report })
    }
}

/// Observe one promotion path's state, mapping oversized to an error the
/// caller records as `refused_oversized` (host side) or re-checks
/// (experiment side resolves bytes at op build — the size gate there).
async fn observe_promotion_state(
    root: &Path,
    canonical: &str,
) -> Result<ObservedState, ExperimentError> {
    match observe_path(root, canonical).await {
        Ok(state) => Ok(state),
        Err(ExperimentError::Budget(_)) => {
            // Oversized: the caller decides per side. The host side can
            // never be "clean" (its bytes are unhashable within budget) —
            // report absence-of-proof as a state the classifier refuses to
            // apply over. Represent as a sentinel the classifier cannot
            // equal to any baseline: re-read failure would be worse (abort),
            // and guessing bytes would be dishonest. Promotion treats this
            // via the oversized review list; reaching here for the host
            // means the host file is huge — classify as conflict-shaped by
            // returning a state that matches nothing.
            //
            // Concretely: hash the refusal itself (stable, content-free) so
            // `host != baseline` always holds and the path lands in
            // Conflict (baselined) — never Apply.
            Ok(ObservedState::Present {
                hash: format!(
                    "oversized-unhashable:{}",
                    blake3::hash(canonical.as_bytes()).to_hex()
                ),
            })
        }
        Err(error) => Err(error),
    }
}

/// One gated promotion write: `filesystem` write (`content_b64`, byte-exact
/// like restore) or delete, with the baseline hash stamped as the gate
/// `base_version` for present-at-baseline targets (absent-at-baseline
/// creates carry no claim — documented last-writer-wins with the scan +
/// re-read as the guard).
fn promotion_request(
    experiment_id: &str,
    promoter: &str,
    host_session: &SessionContext,
    path: &str,
    op: &ApplyOp,
    baseline_hash: Option<&str>,
) -> GateRequest {
    use base64::Engine as _;
    let input = match op {
        ApplyOp::Write { bytes, .. } => serde_json::json!({
            "operation": "write",
            "path": path,
            "content_b64": base64::engine::general_purpose::STANDARD.encode(bytes),
        }),
        ApplyOp::Delete { .. } => serde_json::json!({
            "operation": "delete",
            "path": path,
        }),
    };
    let mut base_versions = BTreeMap::new();
    if let Some(hash) = baseline_hash {
        base_versions.insert(path.to_owned(), hash.to_owned());
    }
    GateRequest {
        call_id: format!("exp-promote-{experiment_id}-{}", Ulid::new()),
        agent_id: promoter.to_owned(),
        tool: "filesystem".to_owned(),
        input,
        registered_as: None,
        session_id: Some(host_session.session_id.to_string()),
        scope: "experiment-promotion".to_owned(),
        plan_id: None,
        causation: None,
        base_versions,
        orchestrator_authority: false,
    }
}

// ---------------------------------------------------------------------------
// Abandon + cleanup (never touches the host)
// ---------------------------------------------------------------------------

/// Mark the record `Abandoned` (pure status flip — the host is untouched by
/// construction; workspace removal is a separate explicit step).
///
/// Returns `false` (without changing anything) when the record is already
/// finished.
pub fn mark_abandoned(record: &mut ExperimentRecord) -> bool {
    if record.status != ExperimentStatus::Active {
        return false;
    }
    record.status = ExperimentStatus::Abandoned;
    true
}

/// Remove a copy-mode workspace directory. Best-effort errors surface as
/// [`ExperimentError::Io`] — the record transition (`mark_abandoned`) is
/// independent and infallible.
pub async fn remove_copy_workspace(workspace_root: &Path) -> Result<(), ExperimentError> {
    tokio::fs::remove_dir_all(workspace_root).await.map_err(|error| ExperimentError::Io {
        path: workspace_root.display().to_string(),
        message: error.to_string(),
    })
}

/// Remove a worktree-mode workspace: unregister via `git worktree remove
/// --force`, then ensure the directory is gone. Surfaces failures (a failed
/// unregister leaves git metadata behind — the error names it instead of
/// pretending cleanliness).
pub async fn remove_worktree_workspace(
    host_root: &Path,
    workspace_root: &Path,
    cancel: &CancellationToken,
) -> Result<(), ExperimentError> {
    check_cancel(cancel)?;
    let host = host_root.to_path_buf();
    let workspace = workspace_root.to_path_buf();
    let output = tokio::task::spawn_blocking(move || {
        std::process::Command::new("git")
            .arg("-C")
            .arg(&host)
            .args(["worktree", "remove", "--force"])
            .arg(&workspace)
            .output()
    })
    .await
    .map_err(|error| ExperimentError::Git(format!("git worktree spawn failed: {error}")))?
    .map_err(|error| ExperimentError::Git(format!("git worktree could not run: {error}")))?;
    if !output.status.success() {
        let stderr =
            truncate_chars(&String::from_utf8_lossy(&output.stderr), EXPERIMENT_GIT_STDERR_LIMIT);
        return Err(ExperimentError::Git(format!(
            "git worktree remove --force failed: {stderr} (unregister by hand: git -C {} worktree prune)",
            host_root.display()
        )));
    }
    // `worktree remove` takes the directory; a leftover (untracked extras
    // the flag kept) is removed explicitly so scratch never lingers.
    if workspace_root.exists() {
        tokio::fs::remove_dir_all(workspace_root).await.map_err(|error| ExperimentError::Io {
            path: workspace_root.display().to_string(),
            message: error.to_string(),
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash_of(text: &str) -> String {
        blake3::hash(text.as_bytes()).to_hex().to_string()
    }

    fn present(text: &str) -> ObservedState {
        ObservedState::Present { hash: hash_of(text) }
    }

    #[test]
    fn conflict_matrix_covers_all_triples() {
        let base_hash = hash_of("base");
        let base = Some(base_hash.as_str());
        // (experiment_changed, host_changed) x agreement.
        assert_eq!(classify_path(base, &present("base"), &present("base")), PathVerdict::Unchanged);
        assert_eq!(classify_path(base, &present("host"), &present("base")), PathVerdict::HostDrift);
        assert_eq!(classify_path(base, &present("base"), &present("exp")), PathVerdict::Apply);
        assert_eq!(classify_path(base, &present("same"), &present("same")), PathVerdict::Converged);
        assert_eq!(classify_path(base, &present("host"), &present("exp")), PathVerdict::Conflict);
        // Transitions: delete/create vs baseline.
        assert_eq!(
            classify_path(base, &present("base"), &ObservedState::Absent),
            PathVerdict::Apply,
            "experiment delete + clean host applies"
        );
        assert_eq!(
            classify_path(base, &present("host"), &ObservedState::Absent),
            PathVerdict::Conflict,
            "experiment delete + host edit refuses"
        );
        assert_eq!(
            classify_path(None, &ObservedState::Absent, &present("new")),
            PathVerdict::Apply,
            "experiment create + absent host applies"
        );
        assert_eq!(
            classify_path(None, &present("host-new"), &present("new")),
            PathVerdict::Conflict,
            "experiment create + host create refuses"
        );
        assert_eq!(
            classify_path(None, &present("new"), &present("new")),
            PathVerdict::Converged,
            "both created identical bytes converges"
        );
        assert_eq!(
            classify_path(None, &ObservedState::Absent, &ObservedState::Absent),
            PathVerdict::Unchanged
        );
        // Host deleted a path the experiment never touched: surfaced, never
        // written.
        assert_eq!(
            classify_path(base, &ObservedState::Absent, &present("base")),
            PathVerdict::HostDrift
        );
    }

    #[test]
    fn copy_budget_fails_closed_with_named_ceiling() {
        assert!(evaluate_copy_budget(&[]).is_ok());
        assert!(evaluate_copy_budget(&[("a.txt".to_string(), 10)]).is_ok());
        let too_many: Vec<(String, u64)> =
            (0..EXPERIMENT_MAX_FILES + 1).map(|i| (format!("f{i}"), 1)).collect();
        match evaluate_copy_budget(&too_many) {
            Err(ExperimentError::Budget(message)) => {
                assert!(message.contains("EXPERIMENT_MAX_FILES"), "{message}");
            }
            other => panic!("count ceiling must refuse, got {other:?}"),
        }
        match evaluate_copy_budget(&[("big.bin".to_string(), EXPERIMENT_MAX_FILE_BYTES + 1)]) {
            Err(ExperimentError::Budget(message)) => {
                assert!(message.contains("EXPERIMENT_MAX_FILE_BYTES"), "{message}");
            }
            other => panic!("per-file ceiling must refuse, got {other:?}"),
        }
        // Exactly at each ceiling still passes.
        assert!(
            evaluate_copy_budget(&[("edge.bin".to_string(), EXPERIMENT_MAX_FILE_BYTES)]).is_ok()
        );
    }

    #[test]
    fn roots_validation_refuses_nesting_reuse_and_missing_host() {
        let host = Path::new("/repo/host");
        // Workspace under host.
        assert!(validate_roots(host, Path::new("/repo/host/ws")).is_err());
        // Host under workspace.
        assert!(validate_roots(host, Path::new("/repo")).is_err());
        // Identical.
        assert!(validate_roots(host, host).is_err());
        // Relative.
        assert!(validate_roots(Path::new("rel"), Path::new("/abs/ws")).is_err());
    }

    #[test]
    fn exclusions_hit_generated_caches_at_any_depth() {
        assert!(is_excluded_canonical("target/debug/app"));
        assert!(is_excluded_canonical("node_modules/pkg/index.js"));
        assert!(is_excluded_canonical(".git/objects/x"));
        assert!(is_excluded_canonical("a/b/__pycache__/c.pyc"));
        assert!(!is_excluded_canonical("src/main.rs"));
        assert!(!is_excluded_canonical("targeted/file.rs"), "prefix match is not exclusion");
    }

    #[test]
    fn workspace_guard_rejects_escapes() {
        let ws = Path::new("/scratch/exp-1");
        assert_eq!(workspace_contains(ws, "sub/file.rs").as_deref(), Some("sub/file.rs"));
        assert_eq!(workspace_contains(ws, "/scratch/exp-1/a.rs").as_deref(), Some("a.rs"));
        assert!(workspace_contains(ws, "/etc/passwd").is_none(), "absolute host path escapes");
        assert!(workspace_contains(ws, "../host/x.rs").is_none(), ".. escape refused");
        assert!(workspace_contains(ws, "/scratch/other/y.rs").is_none(), "sibling root refused");
    }

    #[test]
    fn record_round_trips_current_version_only() {
        let record = ExperimentRecord {
            schema_version: EXPERIMENT_SCHEMA_VERSION,
            experiment_id: "exp-1".to_string(),
            host_root: PathBuf::from("/repo"),
            workspace_root: PathBuf::from("/scratch/exp-1"),
            mode: ExperimentMode::Copy,
            created_by: "agent-a".to_string(),
            created_at_ms: 1_700_000_000_000,
            baseline: vec![BaselineEntry {
                canonical_path: "a.txt".to_string(),
                baseline_hash: hash_of("hi"),
                baseline_len: 2,
            }],
            baseline_generation: "gen".to_string(),
            excluded: Vec::new(),
            status: ExperimentStatus::Active,
            last_report: None,
        };
        let json = record.to_json().expect("serialize");
        let back = ExperimentRecord::from_json(&json).expect("decode");
        assert_eq!(record, back);
        assert_eq!(back.baseline_digest(), record.baseline_digest());
        // Digest binds the baseline: any change moves it (review freshness).
        let mut moved = record.clone();
        moved.baseline[0].baseline_hash = hash_of("other");
        assert_ne!(moved.baseline_digest(), record.baseline_digest());
        // Unknown versions rejected (no legacy migration — greenfield).
        let mut value = serde_json::to_value(&record).expect("to value");
        value["schema_version"] = serde_json::json!(999);
        assert!(ExperimentRecord::from_json(&serde_json::to_string(&value).expect("json")).is_err());
    }

    #[test]
    fn review_binding_digests_move_with_state() {
        let mut states = BTreeMap::new();
        states.insert("a.txt".to_string(), present("one"));
        states.insert("b.txt".to_string(), present("two"));
        let digest = experiment_state_digest(&states);
        // Same observations, any insertion order: same digest (BTreeMap
        // iteration is sorted).
        let mut reordered = BTreeMap::new();
        reordered.insert("b.txt".to_string(), present("two"));
        reordered.insert("a.txt".to_string(), present("one"));
        assert_eq!(experiment_state_digest(&states), experiment_state_digest(&reordered));
        // Any change moves it.
        states.insert("a.txt".to_string(), present("changed"));
        assert_ne!(experiment_state_digest(&states), digest);
        states.insert("a.txt".to_string(), present("one"));
        assert_eq!(experiment_state_digest(&states), digest);
        states.insert("a.txt".to_string(), ObservedState::Absent);
        assert_ne!(experiment_state_digest(&states), digest);
    }
}
