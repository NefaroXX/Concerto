//! Durable file checkpoints (ADR-82 §4, slice 5): the vocabulary core owns.
//!
//! A [`CheckpointRecord`] anchors the checkpoint-time state of a set of file
//! paths so a later [`ToolExecutor`](crate::executor::ToolExecutor) restore
//! can return byte-exact content. The durable pre-image **bytes** live in the
//! `concerto-sessions` sidecar table (migration 037); this module owns the
//! record envelope, the budget rule, and the collaborator traits the executor
//! restores through — never the storage itself (layering: core names no
//! downstream crate).
//!
//! Kind semantics (locked by slice 5): `kind` is **provenance** — what the
//! checkpoint anchors relative to the last write-gate pre-image (`pre_hash`):
//! `FileCreate` came into existence (`pre` absent, `post` present),
//! `FileOverwrite` was modified (`pre` and `post` present), `FileDelete` was
//! removed or was absent (`post` absent). The **restore action follows the
//! post state**, not the kind: post bytes present → write them back (recreating
//! the file when it was deleted since); post absent → ensure the path is
//! deleted (a no-op when still absent). Deletion restores are exact; only
//! byte-carrying restores need the sidecar.
//!
//! Partial-milestone honesty (ADR-82 Accepted Limitations §1-3): byte-exact
//! restore covers small (≤ [`CHECKPOINT_BLOB_LIMIT_BYTES`]) UTF-8 text
//! pre-images only. Oversized and binary targets stay hash-only: the blake3
//! hash is a verifier, never a reconstruction source. Shell side-effects,
//! network, database, and process effects are never reversed (`excluded`
//! covers those classes). The streaming/block-based pre-image store that
//! completes binary restore is deferred, as is untracked-file restore beyond
//! the git-stash gap (`RestoreMethod::GitStash` inherits `undo.rs`'s missing
//! `-u`).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::ids::Ulid;
use crate::types::SessionContext;
use crate::CancellationToken;

// ---------------------------------------------------------------------------
// Budgets
// ---------------------------------------------------------------------------

/// Current `CheckpointRecord` envelope schema version.
///
/// Named-version policy (per the `GraphCheckpoint` precedent): this exact
/// version loads as-is; anything else is rejected by [`CheckpointRecord::from_json`].
/// There are no legacy versions to migrate — the record is greenfield in this
/// slice — so the supported set is exactly `{1}`.
pub const CHECKPOINT_RECORD_SCHEMA_VERSION: u32 = 1;

/// Own byte budget for the checkpoint sidecar's `content` BLOB column.
///
/// Deliberately **not** an alias of the evidence-path `CACHE_LIMIT_BYTES`
/// (`concerto-sessions` `resource_facts`): that bound scopes the read-dedupe
/// cache ("maximum cached read content size … keeping the cache a small,
/// bounded side table"), while this one scopes a restoration store. Same
/// numeric value today (64 KiB), but the two must evolve independently — an
/// evidence-path tuning must never silently change restore guarantees, and a
/// restore-ceiling change must never resize the read cache. The column is a
/// BLOB and this limit is a byte bound on it, never a text bound.
pub const CHECKPOINT_BLOB_LIMIT_BYTES: usize = 64 * 1024;

/// Bound on `CheckpointRecord.metadata` entries (operator-supplied annotations only).
pub const CHECKPOINT_METADATA_MAX_ENTRIES: usize = 16;
/// Bound on one metadata key, in bytes.
pub const CHECKPOINT_METADATA_MAX_KEY_BYTES: usize = 64;
/// Bound on one metadata value, in bytes.
pub const CHECKPOINT_METADATA_MAX_VALUE_BYTES: usize = 512;
/// Bound on `CheckpointRecord.limitations` entries.
pub const CHECKPOINT_LIMITATIONS_MAX_ENTRIES: usize = 16;
/// Bound on one limitation string, in bytes.
pub const CHECKPOINT_LIMITATION_MAX_BYTES: usize = 512;
/// Sanity bound on a canonical checkpoint path, in bytes.
pub const CHECKPOINT_PATH_MAX_BYTES: usize = 4096;

/// Standard limitation recording the slice-5 partial scope on any checkpoint
/// that carries hash-only (unrestorable-bytes) resources.
pub const PARTIAL_SCOPE_LIMITATION: &str = "partial milestone (ADR-82 slice 5): byte-exact restore covers small (≤ 64 KiB) UTF-8 text pre-images only; binary and larger files are hash-only (the hash verifies drift but cannot reconstruct bytes) and restore as Manual; the streaming/block-based pre-image store is deferred";

// ---------------------------------------------------------------------------
// Record envelope
// ---------------------------------------------------------------------------

/// What a file checkpoint scopes: an explicit path set, one task, or the run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointScope {
    /// Explicit canonical (project-relative) paths.
    FilesystemWrites {
        /// Canonical paths in scope, deduplicated and sorted by the producer.
        paths: Vec<String>,
    },
    /// Every filesystem mutation attributed to one task.
    Task {
        /// The task whose mutations are scoped.
        task_id: crate::types::TaskId,
    },
    /// Every filesystem mutation of the owning session/run.
    Run,
}

/// Provenance of one checkpointed resource: what the checkpoint anchors
/// relative to the last write-gate pre-image (see the module docs). The
/// restore action follows the **post** state, not this kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointResourceKind {
    /// Anchored file came into existence (`pre` absent, `post` present).
    FileCreate,
    /// Anchored file was modified (`pre` and `post` present).
    FileOverwrite,
    /// Anchored file was removed, or was absent and untouched (`post` absent).
    FileDelete,
}

/// Locator for the durable bytes of one checkpointed resource: the sidecar
/// row key (`checkpoint_id`, `canonical_path`). The row's `content_hash`
/// verifies the bytes before any restore writes them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointByteRef {
    /// The checkpoint that banked the bytes (doubles as the idempotency key).
    pub checkpoint_id: String,
    /// Canonical (project-relative) path whose bytes were banked.
    pub canonical_path: String,
}

/// One file path anchored by a checkpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointResource {
    /// Provenance relative to the pre-image (see [`CheckpointResourceKind`]).
    pub kind: CheckpointResourceKind,
    /// Canonical (project-relative) path.
    pub canonical_path: String,
    /// Blake3 hex of the last write-gate pre-image for the path, when the
    /// log knew one (provenance/audit only — never a restore source).
    #[serde(default)]
    pub pre_hash: Option<String>,
    /// Blake3 hex of the checkpoint-time content (`None` when absent then).
    #[serde(default)]
    pub post_hash: Option<String>,
    /// Sidecar locator for the checkpoint-time bytes (`None` for hash-only
    /// resources: absent-at-checkpoint paths need no bytes, oversized/binary
    /// paths cannot bank them in this slice).
    #[serde(default)]
    pub byte_ref: Option<CheckpointByteRef>,
    /// Checkpoint-time byte size (`None` when absent then).
    #[serde(default)]
    pub size_bytes: Option<u64>,
}

/// Effect classes a file checkpoint explicitly does **not** reverse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointExclusionClass {
    /// Shell side-effects (commands run, processes spawned, files touched
    /// outside the scoped paths by a shell tool).
    ShellSideEffect,
    /// Network effects (requests sent, remote state changed).
    Network,
    /// Database effects (rows written outside the file scope).
    Database,
    /// Process effects (daemons started, signals sent).
    Process,
    /// Byte restore unavailable in this slice: oversized (> budget) or binary
    /// (non-UTF-8) content is hash-only. The `reason` names which.
    OversizedFile,
}

/// One explicitly unrestored effect or unrestorable-bytes path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointExclusion {
    /// The never-reversed class (or `OversizedFile` for hash-only bytes).
    pub class: CheckpointExclusionClass,
    /// Canonical path, when the exclusion is about a file.
    #[serde(default)]
    pub path: Option<String>,
    /// Why this is excluded (operator-readable, bounded by the producer).
    pub reason: String,
}

/// Lifecycle of a checkpoint record.
///
/// **Retention invariant (ADR-82 §4):** prune-eligibility is gated on this
/// status. A checkpoint advertised as restorable (`Active`) is never
/// prune-eligible; once its bytes are pruned it transitions to `Expired` and
/// the restorable claim is withdrawn from that instant. No consumer may treat
/// an `Expired` (or `Superseded`) checkpoint as restorable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointStatus {
    /// Restorable claim live: every byte-carrying resource still has bytes.
    Active,
    /// Restored once; a second restore is a recorded no-op (idempotency).
    Restored {
        /// Unix epoch millis of the restore.
        at_ms: i64,
        /// Agent id that performed the restore.
        by: String,
    },
    /// Replaced by a newer checkpoint; bytes may be collected.
    Superseded,
    /// Bytes pruned (or retention released): restorable claim withdrawn, the
    /// record remains for audit. Never restorable.
    Expired {
        /// Why the claim was withdrawn (e.g. retention prune).
        reason: String,
    },
    /// Some scoped resources are hash-only (`missing` names them); the rest
    /// restore normally.
    Partial {
        /// Canonical paths without restorable bytes.
        missing: Vec<String>,
    },
}

impl CheckpointStatus {
    /// The durable single-word state stored on the sidecar's record column.
    #[must_use]
    pub fn state_str(&self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Restored { .. } => "restored",
            Self::Superseded => "superseded",
            Self::Expired { .. } => "expired",
            Self::Partial { .. } => "partial",
        }
    }

    /// Whether the status still advertises restorability (`Active` or
    /// `Partial`). `Manual`-method checkpoints additionally surface operator
    /// handling through the restore outcome, never as a silent skip.
    #[must_use]
    pub fn advertises_restorable(&self) -> bool {
        matches!(self, Self::Active | Self::Partial { .. })
    }
}

/// How a checkpoint restores.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RestoreMethod {
    /// Restore from sidecar pre-image bytes through gated `filesystem` writes.
    PreImageStore,
    /// Restore by delegating to the existing git-stash `UndoManager`
    /// (inherits its untracked-file gap: stashes without `-u`).
    GitStash,
    /// Nothing is auto-restored; the operator follows `limitations`.
    Manual,
}

/// Durable file-checkpoint record (ADR-82 §4).
///
/// `checkpoint_id` is a ULID and doubles as the idempotency key: re-restoring
/// the same id is a recorded no-op, never a second mutation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointRecord {
    /// Envelope schema version ([`CHECKPOINT_RECORD_SCHEMA_VERSION`]).
    pub schema_version: u32,
    /// Checkpoint identity + idempotency key.
    pub checkpoint_id: Ulid,
    /// Owning session.
    pub session_id: Ulid,
    /// What the checkpoint scopes.
    pub scope: CheckpointScope,
    /// Anchored file resources.
    #[serde(default)]
    pub included: Vec<CheckpointResource>,
    /// Explicitly unrestored effects / hash-only paths.
    #[serde(default)]
    pub excluded: Vec<CheckpointExclusion>,
    /// Bounded operator annotations.
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
    /// Lifecycle state (the retention invariant keys on this).
    pub status: CheckpointStatus,
    /// How restore proceeds.
    pub restore_method: RestoreMethod,
    /// Honest ceilings (MemoryEvidence-style: surfaced, never collapsed).
    #[serde(default)]
    pub limitations: Vec<String>,
    /// Agent id that created the checkpoint.
    pub created_by: String,
    /// Creation time, Unix epoch millis (UTC).
    pub created_at_ms: i64,
    /// Source revision for the audit trail (advisory, never a scope bound).
    #[serde(default)]
    pub source_revision: Option<String>,
}

/// Errors building or decoding a [`CheckpointRecord`].
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum CheckpointRecordError {
    /// JSON decoding failed.
    #[error("checkpoint record decode failed: {0}")]
    Serialization(String),
    /// Schema version outside the supported named set.
    #[error(
        "unsupported checkpoint record schema version {0}: this runtime supports v{CHECKPOINT_RECORD_SCHEMA_VERSION} only"
    )]
    UnsupportedVersion(u32),
    /// A construction bound was violated (metadata/limitation/path caps).
    #[error("invalid checkpoint record: {0}")]
    Validation(String),
}

impl From<serde_json::Error> for CheckpointRecordError {
    fn from(err: serde_json::Error) -> Self {
        Self::Serialization(err.to_string())
    }
}

impl CheckpointRecord {
    /// Build a record, enforcing the metadata/limitation/path bounds.
    ///
    /// Bounds are fail-fast here (caller-supplied annotations violating them
    /// is a caller bug); capture-time byte failures are fail-soft one layer up
    /// (they downgrade the resource to hash-only, never fail creation).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        checkpoint_id: Ulid,
        session_id: Ulid,
        scope: CheckpointScope,
        included: Vec<CheckpointResource>,
        excluded: Vec<CheckpointExclusion>,
        metadata: BTreeMap<String, String>,
        status: CheckpointStatus,
        restore_method: RestoreMethod,
        limitations: Vec<String>,
        created_by: String,
        created_at_ms: i64,
        source_revision: Option<String>,
    ) -> Result<Self, CheckpointRecordError> {
        if metadata.len() > CHECKPOINT_METADATA_MAX_ENTRIES {
            return Err(CheckpointRecordError::Validation(format!(
                "metadata holds {} entries, bound is {CHECKPOINT_METADATA_MAX_ENTRIES}",
                metadata.len()
            )));
        }
        for (key, value) in &metadata {
            if key.len() > CHECKPOINT_METADATA_MAX_KEY_BYTES {
                return Err(CheckpointRecordError::Validation(format!(
                    "metadata key exceeds {CHECKPOINT_METADATA_MAX_KEY_BYTES} bytes"
                )));
            }
            if value.len() > CHECKPOINT_METADATA_MAX_VALUE_BYTES {
                return Err(CheckpointRecordError::Validation(format!(
                    "metadata value exceeds {CHECKPOINT_METADATA_MAX_VALUE_BYTES} bytes"
                )));
            }
        }
        if limitations.len() > CHECKPOINT_LIMITATIONS_MAX_ENTRIES {
            return Err(CheckpointRecordError::Validation(format!(
                "limitations hold {} entries, bound is {CHECKPOINT_LIMITATIONS_MAX_ENTRIES}",
                limitations.len()
            )));
        }
        for limitation in &limitations {
            if limitation.len() > CHECKPOINT_LIMITATION_MAX_BYTES {
                return Err(CheckpointRecordError::Validation(format!(
                    "limitation exceeds {CHECKPOINT_LIMITATION_MAX_BYTES} bytes"
                )));
            }
        }
        for resource in &included {
            if resource.canonical_path.len() > CHECKPOINT_PATH_MAX_BYTES {
                return Err(CheckpointRecordError::Validation(format!(
                    "checkpoint path exceeds {CHECKPOINT_PATH_MAX_BYTES} bytes"
                )));
            }
        }
        if created_by.is_empty() {
            return Err(CheckpointRecordError::Validation(
                "created_by must name the creating agent".to_string(),
            ));
        }
        Ok(Self {
            schema_version: CHECKPOINT_RECORD_SCHEMA_VERSION,
            checkpoint_id,
            session_id,
            scope,
            included,
            excluded,
            metadata,
            status,
            restore_method,
            limitations,
            created_by,
            created_at_ms,
            source_revision,
        })
    }

    /// Deserialize with the named-version policy: the current version loads
    /// as-is; anything else is rejected (there are no legacy versions to
    /// migrate — the record is greenfield in this slice).
    pub fn from_json(json: &str) -> Result<Self, CheckpointRecordError> {
        let record: Self = serde_json::from_str(json)?;
        match record.schema_version {
            CHECKPOINT_RECORD_SCHEMA_VERSION => Ok(record),
            other => Err(CheckpointRecordError::UnsupportedVersion(other)),
        }
    }

    /// Serialize the record envelope.
    pub fn to_json(&self) -> Result<String, CheckpointRecordError> {
        Ok(serde_json::to_string(self)?)
    }
}

// ---------------------------------------------------------------------------
// Capture planning (pure, shared by every capture seam)
// ---------------------------------------------------------------------------

/// Outcome of planning one path's capture: at most one of the two is `Some`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedCapture {
    /// The anchored resource (`None` when the path is hash-only/excluded).
    pub included: Option<CheckpointResource>,
    /// The honesty entry (`None` when bytes are banked).
    pub excluded: Option<CheckpointExclusion>,
}

/// Apply the byte-budget rule to one path's checkpoint-time observation.
///
/// * `post_bytes` `Some` (file present): banked when `len <=
///   CHECKPOINT_BLOB_LIMIT_BYTES` **and** valid UTF-8 (this slice restores
///   text through the `filesystem` write tool); otherwise hash-only +
///   `OversizedFile` exclusion with a reason naming which ceiling hit.
/// * `post_bytes` `None` (absent): included as-is — ensuring deletion needs
///   no bytes, so absence restores exactly.
///
/// `pre_hash` is provenance only (the last write-gate pre-image known for the
/// path); it never decides restorability. Kind follows the (pre, post) pair
/// per [`CheckpointResourceKind`].
pub fn plan_capture(
    checkpoint_id: &Ulid,
    canonical_path: &str,
    pre_hash: Option<String>,
    post_bytes: Option<&[u8]>,
) -> PlannedCapture {
    let Some(bytes) = post_bytes else {
        let kind = CheckpointResourceKind::FileDelete;
        return PlannedCapture {
            included: Some(CheckpointResource {
                kind,
                canonical_path: canonical_path.to_owned(),
                pre_hash,
                post_hash: None,
                byte_ref: None,
                size_bytes: None,
            }),
            excluded: None,
        };
    };
    let post_hash = blake3::hash(bytes).to_hex().to_string();
    let size_bytes = Some(bytes.len() as u64);
    let kind = match pre_hash {
        Some(_) => CheckpointResourceKind::FileOverwrite,
        None => CheckpointResourceKind::FileCreate,
    };
    if bytes.len() > CHECKPOINT_BLOB_LIMIT_BYTES {
        return PlannedCapture {
            included: Some(CheckpointResource {
                kind,
                canonical_path: canonical_path.to_owned(),
                pre_hash,
                post_hash: Some(post_hash),
                byte_ref: None,
                size_bytes,
            }),
            excluded: Some(CheckpointExclusion {
                class: CheckpointExclusionClass::OversizedFile,
                path: Some(canonical_path.to_owned()),
                reason: format!(
                    "{} bytes exceeds CHECKPOINT_BLOB_LIMIT_BYTES ({CHECKPOINT_BLOB_LIMIT_BYTES}): hash-only; byte restore unavailable in this slice",
                    bytes.len()
                ),
            }),
        };
    }
    if std::str::from_utf8(bytes).is_err() {
        return PlannedCapture {
            included: Some(CheckpointResource {
                kind,
                canonical_path: canonical_path.to_owned(),
                pre_hash,
                post_hash: Some(post_hash),
                byte_ref: None,
                size_bytes,
            }),
            excluded: Some(CheckpointExclusion {
                class: CheckpointExclusionClass::OversizedFile,
                path: Some(canonical_path.to_owned()),
                reason: "binary (non-UTF-8) content: hash-only; byte restore deferred to the streaming store".to_string(),
            }),
        };
    }
    PlannedCapture {
        included: Some(CheckpointResource {
            kind,
            canonical_path: canonical_path.to_owned(),
            pre_hash,
            post_hash: Some(post_hash),
            byte_ref: Some(CheckpointByteRef {
                checkpoint_id: checkpoint_id.to_string(),
                canonical_path: canonical_path.to_owned(),
            }),
            size_bytes,
        }),
        excluded: None,
    }
}

/// The canonical mutating-target grammar for `filesystem` calls (ADR-82 slice
/// 5 capture seam): `write`/`delete` mutate `path`; `move` mutates `path` and
/// `destination`; `copy` mutates `destination` only (its source is read-only —
/// the copy reads whatever is there, so no concurrent write is lost).
///
/// This is the single grammar both the executor's pre-execution hook and the
/// write gate's versioned targets key on, so the two seams can never drift.
pub fn versioned_fs_targets(tool_name: &str, input: &serde_json::Value) -> Vec<String> {
    if tool_name != "filesystem" {
        return Vec::new();
    }
    let Some(operation) = input.get("operation").and_then(serde_json::Value::as_str) else {
        return Vec::new();
    };
    let path = input.get("path").and_then(serde_json::Value::as_str);
    let destination = input.get("destination").and_then(serde_json::Value::as_str);
    match operation {
        "write" | "delete" => path.into_iter().map(str::to_owned).collect(),
        "move" => {
            let mut targets = Vec::with_capacity(2);
            if let Some(source) = path {
                targets.push(source.to_owned());
            }
            if let Some(destination) = destination {
                targets.push(destination.to_owned());
            }
            targets
        }
        "copy" => destination.into_iter().map(str::to_owned).collect(),
        _ => Vec::new(),
    }
}

/// Unix epoch milliseconds (UTC). The restore audit stamps (`Restored.at_ms`)
/// use wall-clock time from the restoring process.
#[must_use]
pub fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Restore collaborators (layering: defined here, implemented downstream)
// ---------------------------------------------------------------------------

/// Sidecar bytes for one checkpointed path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreImageBytes {
    /// Restorable bytes banked by a capture seam.
    Stored {
        /// The checkpoint-time bytes.
        bytes: Vec<u8>,
        /// Blake3 hex recorded beside the bytes (verified before restore).
        hash: String,
        /// When the bytes were banked, Unix epoch millis.
        captured_at_ms: i64,
        /// Whether the bytes were snapshotted at checkpoint creation
        /// (`Snapshot`) or backfilled by the first gated/direct mutation
        /// after it (`PreImage` — exact only when nothing changed between
        /// creation and that mutation; the outcome notes the provenance).
        capture_kind: CaptureKind,
    },
    /// Hash-only row: the hash verifies drift but cannot reconstruct bytes.
    HashOnly {
        /// Recorded hash, when the row exists.
        hash: Option<String>,
    },
    /// No sidecar row for this checkpoint+path.
    Absent,
}

/// Provenance of banked sidecar bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureKind {
    /// Snapshotted at checkpoint creation: checkpoint-time content by
    /// construction.
    Snapshot,
    /// Backfilled by the first gated (or direct-path) mutation after
    /// creation: exact when nothing else changed in between; the restore
    /// outcome carries this provenance so consumers never assume more.
    PreImage,
}

impl CaptureKind {
    /// The durable single-word spelling stored on the sidecar row.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Snapshot => "snapshot",
            Self::PreImage => "pre-image",
        }
    }

    /// Parse the durable spelling; unknown values read `None` (never a guess).
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "snapshot" => Some(Self::Snapshot),
            "pre-image" => Some(Self::PreImage),
            _ => None,
        }
    }
}

/// Errors from a [`CheckpointStore`] implementation.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum CheckpointStoreError {
    /// The store backend failed.
    #[error("checkpoint store failed: {0}")]
    Storage(String),
    /// Record decoding failed.
    #[error("checkpoint record decode failed: {0}")]
    Serialization(String),
    /// The operation was cancelled.
    #[error("checkpoint store operation cancelled")]
    Cancelled,
}

/// Durable side of a file checkpoint: the record envelope plus the sidecar
/// pre-image bytes. Implemented by `concerto-sessions` (`FileCheckpointStore`);
/// the executor restores through this trait so core never names the store.
#[async_trait::async_trait]
pub trait CheckpointStore: Send + Sync {
    /// Load the record envelope (`None` when the id is unknown).
    async fn load_record(
        &self,
        checkpoint_id: &str,
        cancel: CancellationToken,
    ) -> Result<Option<CheckpointRecord>, CheckpointStoreError>;

    /// Load one path's sidecar bytes.
    async fn load_bytes(
        &self,
        checkpoint_id: &str,
        canonical_path: &str,
        cancel: CancellationToken,
    ) -> Result<PreImageBytes, CheckpointStoreError>;

    /// Transition `Active` (or `Partial`) → `Restored{at_ms, by}`. Returns
    /// `false` (without changing anything) when the checkpoint is already
    /// `Restored` — the idempotent re-restore path.
    async fn mark_restored(
        &self,
        checkpoint_id: &str,
        at_ms: i64,
        by: &str,
        cancel: CancellationToken,
    ) -> Result<bool, CheckpointStoreError>;

    /// Transition to `Partial{missing}` after a restore that banked some
    /// paths but found others byte-less.
    async fn mark_partial(
        &self,
        checkpoint_id: &str,
        missing: Vec<String>,
        cancel: CancellationToken,
    ) -> Result<(), CheckpointStoreError>;
}

/// Read side of the ownership table for restore: `None` means unowned (the
/// restore may proceed — the gate auto-acquires on the gated writes); `Some`
/// names the holding agent. Implemented by the orchestrator around the write
/// gate's table; a foreign owner aborts the whole restore before any write
/// (fail-closed, surfaced — never a silent overwrite).
pub trait OwnershipOracle: Send + Sync + std::fmt::Debug {
    /// The agent holding `canonical_path`, if any.
    fn owner_of(&self, canonical_path: &str) -> Option<String>;
}

/// Git-stash side of a `RestoreMethod::GitStash` restore. Implemented by the
/// orchestrator around `UndoManager`; `Ok` carries a human-readable detail
/// for the outcome, `Err` carries the reason (surfaced, never retried blindly).
pub trait StashRestorer: Send + Sync + std::fmt::Debug {
    /// Pop the stash backing `checkpoint` (which must be a `GitStash` record).
    fn restore_stash(&self, checkpoint: &CheckpointRecord) -> Result<String, String>;
}

/// Pre-execution capture sink for the direct (ungated) executor path: bank the
/// current bytes of each about-to-be-mutated canonical path for every active
/// checkpoint covering it. Fail-soft by contract — implementations swallow
/// their own errors (capture failure downgrades the resource, never fails the
/// write) — so this returns `()` and the executor never branches on it.
#[async_trait::async_trait]
pub trait MutationCaptureSink: Send + Sync + std::fmt::Debug {
    /// Bank current bytes for `canonical_paths` (project-relative) under
    /// `session`'s project dir.
    async fn capture_before_mutation(
        &self,
        canonical_paths: Vec<String>,
        session: &SessionContext,
        cancel: CancellationToken,
    );
}

// ---------------------------------------------------------------------------
// Restore outcome
// ---------------------------------------------------------------------------

/// One foreign-owned path that aborted a restore before any write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnershipConflictInfo {
    /// The contested canonical path.
    pub path: String,
    /// The holding agent.
    pub owner: String,
}

/// Outcome of [`ToolExecutor::restore_checkpoint`](crate::executor::ToolExecutor::restore_checkpoint).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestoreOutcome {
    /// Every included resource was restored through gated writes.
    Restored {
        /// Canonical paths written back (byte-exact).
        restored: Vec<String>,
        /// Paths recreated (absent at restore time, bytes banked).
        recreated: Vec<String>,
        /// Paths deleted (absent at checkpoint time).
        deleted: Vec<String>,
        /// Provenance notes (e.g. pre-image-captured bytes) — honest
        /// packaging, never silently exact.
        notes: Vec<String>,
    },
    /// The checkpoint was already `Restored`: no write ran (recorded no-op;
    /// the first restore's `Restored{at, by}` is the record).
    AlreadyRestored {
        /// When the first restore ran, Unix epoch millis.
        at_ms: i64,
        /// Who ran the first restore.
        by: String,
    },
    /// The byte-carrying subset restored; `missing` names the hash-only paths
    /// (their `limitations` explain why). The checkpoint transitioned to
    /// `Partial{missing}`.
    Partial {
        /// Canonical paths written back.
        restored: Vec<String>,
        /// Canonical paths without restorable bytes.
        missing: Vec<String>,
        /// Why each missing path has no bytes.
        limitations: Vec<String>,
    },
    /// `RestoreMethod::Manual`: nothing was written; the operator follows
    /// `limitations`.
    RequiresOperator {
        /// Why auto-restore is unavailable.
        limitations: Vec<String>,
    },
    /// `RestoreMethod::GitStash`: the stash pop ran outside the byte path.
    StashRestored {
        /// What the stash restorer reported.
        detail: String,
    },
}

/// Errors from [`ToolExecutor::restore_checkpoint`](crate::executor::ToolExecutor::restore_checkpoint).
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum RestoreError {
    /// Unknown checkpoint id.
    #[error("checkpoint not found: {0}")]
    NotFound(String),
    /// The checkpoint's status/method refuses auto-restore (`Expired`,
    /// `Superseded` — never advertise restorable without bytes).
    #[error("checkpoint {id} is not restorable: {reason}")]
    NotRestorable {
        /// The checkpoint id.
        id: String,
        /// Why (`expired`, `superseded`, …).
        reason: String,
    },
    /// Foreign-owned paths aborted the restore before any write (fail-closed).
    #[error("restore refused: {0:?}")]
    OwnershipConflict(Vec<OwnershipConflictInfo>),
    /// A restore write was denied by policy (fail-closed, mid-restore abort;
    /// the checkpoint status is left unchanged so the failure is retriable).
    #[error("restore write denied by policy: {0}")]
    PolicyDenied(String),
    /// A restore write failed (mid-restore abort; status unchanged).
    #[error("restore write failed: {0}")]
    ExecutionFailed(String),
    /// The restore was cancelled.
    #[error("restore cancelled")]
    Cancelled,
    /// The checkpoint store failed.
    #[error("restore store failed: {0}")]
    Store(#[from] CheckpointStoreError),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_ids() -> (Ulid, Ulid) {
        (Ulid::new(), Ulid::new())
    }

    fn minimal_record() -> CheckpointRecord {
        let (checkpoint_id, session_id) = test_ids();
        CheckpointRecord::new(
            checkpoint_id,
            session_id,
            CheckpointScope::FilesystemWrites { paths: vec!["a.txt".to_string()] },
            Vec::new(),
            Vec::new(),
            BTreeMap::new(),
            CheckpointStatus::Active,
            RestoreMethod::PreImageStore,
            Vec::new(),
            "agent-a".to_string(),
            1_700_000_000_000,
            None,
        )
        .expect("minimal record builds")
    }

    #[test]
    fn record_round_trips_current_version() {
        let record = minimal_record();
        let json = record.to_json().expect("serialize");
        let back = CheckpointRecord::from_json(&json).expect("decode");
        assert_eq!(record, back);
        assert_eq!(back.schema_version, CHECKPOINT_RECORD_SCHEMA_VERSION);
    }

    #[test]
    fn record_rejects_unknown_versions() {
        let record = minimal_record();
        let mut value = serde_json::to_value(&record).expect("to value");
        for bad in [0_u32, 2, 999] {
            value["schema_version"] = serde_json::json!(bad);
            let json = serde_json::to_string(&value).expect("to json");
            match CheckpointRecord::from_json(&json) {
                Err(CheckpointRecordError::UnsupportedVersion(v)) => assert_eq!(v, bad),
                other => panic!("version {bad} must be rejected, got {other:?}"),
            }
        }
    }

    #[test]
    fn plan_capture_banks_small_text() {
        let id = Ulid::new();
        let planned = plan_capture(&id, "a.txt", None, Some(b"hello"));
        let included = planned.included.expect("banked");
        assert_eq!(included.kind, CheckpointResourceKind::FileCreate);
        assert!(planned.excluded.is_none());
        let byte_ref = included.byte_ref.expect("byte ref");
        assert_eq!(byte_ref.checkpoint_id, id.to_string());
        assert_eq!(byte_ref.canonical_path, "a.txt");
        assert_eq!(included.post_hash, Some(blake3::hash(b"hello").to_hex().to_string()));
        assert_eq!(included.size_bytes, Some(5));
    }

    #[test]
    fn plan_capture_marks_overwrite_provenance() {
        let id = Ulid::new();
        let planned = plan_capture(&id, "a.txt", Some("pre-hash".to_string()), Some(b"new"));
        let included = planned.included.expect("banked");
        assert_eq!(included.kind, CheckpointResourceKind::FileOverwrite);
        assert_eq!(included.pre_hash.as_deref(), Some("pre-hash"));
    }

    #[test]
    fn plan_capture_absent_path_is_exact_deletion() {
        let id = Ulid::new();
        let planned = plan_capture(&id, "gone.txt", Some("pre".to_string()), None);
        let included = planned.included.expect("anchored");
        assert_eq!(included.kind, CheckpointResourceKind::FileDelete);
        assert!(included.byte_ref.is_none(), "absence needs no bytes");
        assert!(planned.excluded.is_none(), "absence restores exactly — no honesty entry needed");
        // Untouched-and-absent anchors the same way.
        let planned = plan_capture(&id, "never.txt", None, None);
        let included = planned.included.expect("anchored");
        assert_eq!(included.kind, CheckpointResourceKind::FileDelete);
        assert!(planned.excluded.is_none());
    }

    #[test]
    fn plan_capture_oversized_is_hash_only_with_reason() {
        let id = Ulid::new();
        let big = vec![b'x'; CHECKPOINT_BLOB_LIMIT_BYTES + 1];
        let planned = plan_capture(&id, "big.txt", None, Some(&big));
        let included = planned.included.expect("still anchored for drift detection");
        assert!(included.byte_ref.is_none(), "no bytes banked above budget");
        assert!(included.post_hash.is_some(), "hash stays as the verifier");
        let excluded = planned.excluded.expect("honesty entry");
        assert_eq!(excluded.class, CheckpointExclusionClass::OversizedFile);
        assert!(excluded.reason.contains("CHECKPOINT_BLOB_LIMIT_BYTES"));
        // Exactly at budget still banks.
        let exact = vec![b'y'; CHECKPOINT_BLOB_LIMIT_BYTES];
        let planned = plan_capture(&id, "edge.txt", None, Some(&exact));
        assert!(planned.excluded.is_none());
        assert!(planned.included.expect("edge banks").byte_ref.is_some());
    }

    #[test]
    fn plan_capture_binary_is_hash_only_with_reason() {
        let id = Ulid::new();
        let planned = plan_capture(&id, "bin.dat", None, Some(&[0xff, 0xfe, 0x00, 0x01]));
        let included = planned.included.expect("anchored");
        assert!(included.byte_ref.is_none(), "binary never banks in this slice");
        let excluded = planned.excluded.expect("honesty entry");
        assert_eq!(excluded.class, CheckpointExclusionClass::OversizedFile);
        assert!(excluded.reason.contains("binary"), "reason names the real ceiling");
    }

    #[test]
    fn record_bounds_reject_oversized_annotations() {
        let (checkpoint_id, session_id) = test_ids();
        let mut metadata = BTreeMap::new();
        for i in 0..CHECKPOINT_METADATA_MAX_ENTRIES + 1 {
            metadata.insert(format!("k{i}"), "v".to_string());
        }
        let err = CheckpointRecord::new(
            checkpoint_id,
            session_id,
            CheckpointScope::Run,
            Vec::new(),
            Vec::new(),
            metadata,
            CheckpointStatus::Active,
            RestoreMethod::PreImageStore,
            Vec::new(),
            "agent-a".to_string(),
            0,
            None,
        )
        .expect_err("metadata over bound must fail");
        assert!(matches!(err, CheckpointRecordError::Validation(_)));

        let err = CheckpointRecord::new(
            checkpoint_id,
            session_id,
            CheckpointScope::Run,
            Vec::new(),
            Vec::new(),
            BTreeMap::new(),
            CheckpointStatus::Active,
            RestoreMethod::PreImageStore,
            Vec::new(),
            String::new(),
            0,
            None,
        )
        .expect_err("empty created_by must fail");
        assert!(matches!(err, CheckpointRecordError::Validation(_)));
    }

    #[test]
    fn status_state_strings_gate_retention() {
        assert_eq!(CheckpointStatus::Active.state_str(), "active");
        assert_eq!(
            CheckpointStatus::Restored { at_ms: 1, by: "a".to_string() }.state_str(),
            "restored"
        );
        assert_eq!(CheckpointStatus::Superseded.state_str(), "superseded");
        assert_eq!(CheckpointStatus::Expired { reason: "r".to_string() }.state_str(), "expired");
        assert_eq!(CheckpointStatus::Partial { missing: Vec::new() }.state_str(), "partial");
        assert!(CheckpointStatus::Active.advertises_restorable());
        assert!(
            CheckpointStatus::Partial { missing: vec!["x".to_string()] }.advertises_restorable()
        );
        assert!(!CheckpointStatus::Expired { reason: "r".to_string() }.advertises_restorable());
        assert!(!CheckpointStatus::Superseded.advertises_restorable());
    }

    #[test]
    fn capture_kind_round_trips_durable_spelling() {
        assert_eq!(CaptureKind::Snapshot.as_str(), "snapshot");
        assert_eq!(CaptureKind::PreImage.as_str(), "pre-image");
        assert_eq!(CaptureKind::parse("snapshot"), Some(CaptureKind::Snapshot));
        assert_eq!(CaptureKind::parse("pre-image"), Some(CaptureKind::PreImage));
        assert_eq!(CaptureKind::parse("legacy-text"), None, "unknown kinds never guessed");
    }

    #[test]
    fn versioned_fs_targets_keys_on_canonical_effect() {
        use serde_json::json;
        assert_eq!(
            versioned_fs_targets("filesystem", &json!({"operation": "write", "path": "a.txt"})),
            vec!["a.txt"]
        );
        assert_eq!(
            versioned_fs_targets("filesystem", &json!({"operation": "delete", "path": "a.txt"})),
            vec!["a.txt"]
        );
        assert_eq!(
            versioned_fs_targets(
                "filesystem",
                &json!({"operation": "move", "path": "a.txt", "destination": "b.txt"})
            ),
            vec!["a.txt", "b.txt"]
        );
        assert_eq!(
            versioned_fs_targets(
                "filesystem",
                &json!({"operation": "copy", "path": "a.txt", "destination": "b.txt"})
            ),
            vec!["b.txt"],
            "copy source is read-only, never a versioned target"
        );
        assert!(versioned_fs_targets("filesystem", &json!({"operation": "read", "path": "a.txt"}))
            .is_empty());
        assert!(versioned_fs_targets("shell", &json!({"operation": "write", "path": "a.txt"}))
            .is_empty());
        assert!(versioned_fs_targets("filesystem", &json!({"path": "a.txt"})).is_empty());
    }
}
