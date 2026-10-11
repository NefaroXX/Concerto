//! File-checkpoint create/restore orchestration (ADR-82 §4, slice 5).
//!
//! Thin wiring over the three slice-5 pieces: the core [`CheckpointRecord`]
//! vocabulary, the sessions [`FileCheckpointStore`] sidecar, and the
//! [`ToolExecutor::restore_checkpoint`](concerto_core::executor::ToolExecutor::restore_checkpoint)
//! pipeline. This module owns nothing durable itself:
//!
//! * [`create_file_checkpoint`] snapshots the checkpoint-time bytes of scoped
//!   paths (the authoritative capture — the gate/executor seams only backfill
//!   gaps `INSERT OR IGNORE`), plans every path through the shared budget rule,
//!   and persists record + blobs.
//! * [`restore_file_checkpoint`] restores through the executor with the gate's
//!   ownership table as the oracle and the existing [`UndoManager`] behind
//!   `RestoreMethod::GitStash`.
//! * [`GateOwnershipOracle`] adapts the gate's table to the core oracle trait;
//!   [`UndoStashRestorer`] adapts the git-stash manager to the core stash trait.
//!
//! Partial-milestone honesty: over-budget content is hash-only and surfaces
//! `Manual` + [`PARTIAL_SCOPE_LIMITATION`](concerto_core::checkpoint::PARTIAL_SCOPE_LIMITATION);
//! shell/network/database/process effects are caller-declared exclusions and
//! are never reversed. Symlink/permission/xattr metadata is not captured.
//! Binary files within the per-checkpoint total budget restore byte-exact
//! through the chunked pre-image store.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use concerto_core::checkpoint::{
    CheckpointExclusion, CheckpointRecord, CheckpointResource, CheckpointScope, CheckpointStatus,
    RestoreMethod, CHECKPOINT_TOTAL_LIMIT_BYTES, PARTIAL_SCOPE_LIMITATION,
};
use concerto_core::ids::Ulid;
use concerto_core::types::SessionContext;
use concerto_core::{CancellationToken, TaskId};
use concerto_sessions::file_checkpoints::{
    default_method_for_plan, plan_checkpoint_resource, FileCheckpointStore,
};
use concerto_sessions::SessionError;
use concerto_tools::undo::UndoManager;
use thiserror::Error;

use crate::gate::WriteGate;

/// Errors creating or restoring a file checkpoint at this layer.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum FileCheckpointError {
    /// The sidecar store failed.
    #[error("file checkpoint store failed: {0}")]
    Store(#[from] SessionError),
    /// The record envelope was invalid.
    #[error("invalid file checkpoint: {0}")]
    Record(String),
    /// A filesystem read failed at creation (fail-soft per path: the path
    /// degrades to hash-only, creation continues).
    #[error("checkpoint capture I/O failed for {path}: {message}")]
    Capture {
        /// The canonical path that could not be read.
        path: String,
        /// The I/O failure.
        message: String,
    },
    /// The operation was cancelled.
    #[error("file checkpoint operation cancelled")]
    Cancelled,
    /// The scope carries nothing to anchor.
    #[error("file checkpoint scope is empty: {0}")]
    EmptyScope(String),
}

fn check_cancel(cancel: &CancellationToken) -> Result<(), FileCheckpointError> {
    if cancel.is_cancelled() {
        Err(FileCheckpointError::Cancelled)
    } else {
        Ok(())
    }
}

/// Read side of the gate's ownership table as a core [`OwnershipOracle`](concerto_core::checkpoint::OwnershipOracle):
/// `None` means unowned (restore may proceed — the gated writes
/// auto-attribute); `Some` names the holding agent.
pub struct GateOwnershipOracle {
    gate: Arc<WriteGate>,
}

impl std::fmt::Debug for GateOwnershipOracle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GateOwnershipOracle").finish_non_exhaustive()
    }
}

impl GateOwnershipOracle {
    /// Adapt the gate's live ownership table.
    pub fn new(gate: Arc<WriteGate>) -> Self {
        Self { gate }
    }
}

impl concerto_core::checkpoint::OwnershipOracle for GateOwnershipOracle {
    fn owner_of(&self, canonical_path: &str) -> Option<String> {
        self.gate.ownership_owner(canonical_path)
    }
}

/// `RestoreMethod::GitStash` behind the existing [`UndoManager`]: restore pops
/// the stash committed at checkpoint creation. Inherits the manager's
/// untracked-file gap (stash without `-u`) — declared, not fixed here.
#[derive(Debug)]
pub struct UndoStashRestorer {
    manager: Arc<Mutex<UndoManager>>,
    session_id: Ulid,
    task_id: TaskId,
}

impl UndoStashRestorer {
    /// Adapt a shared undo manager plus the stashed identity.
    pub fn new(manager: Arc<Mutex<UndoManager>>, session_id: Ulid, task_id: TaskId) -> Self {
        Self { manager, session_id, task_id }
    }
}

impl concerto_core::checkpoint::StashRestorer for UndoStashRestorer {
    fn restore_stash(&self, _checkpoint: &CheckpointRecord) -> Result<String, String> {
        let mut manager =
            self.manager.lock().map_err(|error| format!("undo manager lock poisoned: {error}"))?;
        manager
            .rollback(self.task_id, self.session_id)
            .map(|()| {
                format!("git stash popped for session {} task {}", self.session_id, self.task_id)
            })
            .map_err(|error| error.to_string())
    }
}

/// Stash restorer that always refuses: used when no [`UndoManager`] is wired
/// (a `GitStash` restore without one surfaces loudly instead of panicking).
#[derive(Debug, Default)]
struct RefusingStashRestorer;

impl concerto_core::checkpoint::StashRestorer for RefusingStashRestorer {
    fn restore_stash(&self, _checkpoint: &CheckpointRecord) -> Result<String, String> {
        Err("checkpoint restores via git stash but no UndoManager is wired".to_string())
    }
}

/// Anchor a file checkpoint: snapshot the checkpoint-time bytes of every
/// scoped path, plan each through the shared budget rule, and persist record
/// + blobs.
///
/// * `paths` are canonical project-relative paths. Present files snapshot
///   their bytes (banked — binary included — when within the per-checkpoint
///   total budget, hash-only + excluded when over it); absent files anchor
///   as exact deletions; unreadable files degrade to hash-only + excluded
///   (fail-soft per path — creation continues).
/// * `extra_excluded` carries caller-declared non-file effects (shell side
///   effects, network, database, process) — always excluded, never reversed.
///   **Contract:** the capture loop covers filesystem paths only and never
///   infers side effects, so a `Run`- or `Task`-scoped checkpoint REQUIRES
///   the caller to populate `extra_excluded` for every shell, network,
///   database, and process effect observed in scope. An undeclared side
///   effect is out of the restore claim by construction.
/// * `method_override` forces [`RestoreMethod::GitStash`] for stash-backed
///   checkpoints (commit the stash BEFORE calling); otherwise the method
///   follows the plan (`Manual` when nothing banked bytes).
///
/// An empty `FilesystemWrites` scope is rejected (nothing to anchor);
/// `Task`/`Run` scopes anchor the given paths as the creation snapshot and
/// rely on the capture seams for later mutations.
#[allow(clippy::too_many_arguments)]
pub async fn create_file_checkpoint(
    store: &FileCheckpointStore,
    project_root: &Path,
    session_id: Ulid,
    scope: CheckpointScope,
    paths: Vec<String>,
    created_by: &str,
    metadata: BTreeMap<String, String>,
    extra_excluded: Vec<CheckpointExclusion>,
    method_override: Option<RestoreMethod>,
    source_revision: Option<String>,
    cancel: &CancellationToken,
) -> Result<CheckpointRecord, FileCheckpointError> {
    check_cancel(cancel)?;
    if matches!(&scope, CheckpointScope::FilesystemWrites { paths } if paths.is_empty())
        && paths.is_empty()
    {
        return Err(FileCheckpointError::EmptyScope(
            "FilesystemWrites scope names no paths".to_string(),
        ));
    }

    let checkpoint_id = Ulid::new();
    let created_at_ms = concerto_core::checkpoint::now_millis();

    // Deduplicate, keep deterministic order.
    let mut ordered = paths;
    ordered.sort();
    ordered.dedup();

    let mut included: Vec<CheckpointResource> = Vec::new();
    let mut excluded: Vec<CheckpointExclusion> = Vec::new();
    let mut blobs: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let mut limitations: Vec<String> = Vec::new();
    // Running banked total: the per-checkpoint budget half of the rule (the
    // per-path half lives in `plan_capture`). First-fit with honest
    // downgrade — a file that would overflow the total degrades to
    // hash-only + excluded, never a silent skip.
    let mut banked_total: usize = 0;

    for path in &ordered {
        check_cancel(cancel)?;
        let absolute = project_root.join(path);
        let post_bytes: Option<Vec<u8>> = match tokio::fs::read(&absolute).await {
            Ok(bytes) => Some(bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                // Fail-soft per path: hash-only + excluded, creation continues.
                excluded.push(CheckpointExclusion {
                    class: concerto_core::checkpoint::CheckpointExclusionClass::OversizedFile,
                    path: Some(path.clone()),
                    reason: format!(
                        "pre-image unreadable ({error}): hash-only; byte restore unavailable"
                    ),
                });
                limitations.push(format!("{path}: capture failed ({error}); anchored hash-only"));
                // Anchor the path as absent-unknown: the restore treats it as
                // missing bytes (Partial), never as a claimed deletion.
                included.push(CheckpointResource {
                    kind: concerto_core::checkpoint::CheckpointResourceKind::FileDelete,
                    canonical_path: path.clone(),
                    pre_hash: None,
                    post_hash: None,
                    byte_ref: None,
                    size_bytes: None,
                });
                continue;
            }
        };
        // Provenance only: the last write-gate pre-image known for the path.
        // A store failure here degrades the hash (never the capture).
        let pre_hash = match store.latest_write_pre_hash(path, cancel).await {
            Ok(hash) => hash,
            Err(error) => {
                tracing::debug!(
                    path = %path,
                    %error,
                    "checkpoint creation continues without pre-image provenance (fail-soft)"
                );
                None
            }
        };
        let (planned_included, planned_excluded) =
            plan_checkpoint_resource(&checkpoint_id, path, pre_hash, post_bytes.as_deref());
        if let Some(mut resource) = planned_included {
            if resource.byte_ref.is_some() {
                // Per-path bankable; apply the running-total half.
                match post_bytes {
                    Some(bytes)
                        if banked_total.saturating_add(bytes.len())
                            <= CHECKPOINT_TOTAL_LIMIT_BYTES =>
                    {
                        banked_total = banked_total.saturating_add(bytes.len());
                        blobs.insert(path.clone(), bytes);
                    }
                    Some(bytes) => {
                        // Overflow: honest downgrade to hash-only (the hash
                        // stays as the drift verifier; the exclusion names
                        // the real ceiling).
                        let size = bytes.len();
                        resource.post_hash = Some(blake3::hash(&bytes).to_hex().to_string());
                        resource.size_bytes = Some(size as u64);
                        resource.byte_ref = None;
                        excluded.push(CheckpointExclusion {
                            class: concerto_core::checkpoint::CheckpointExclusionClass::OversizedFile,
                            path: Some(path.clone()),
                            reason: format!(
                                "{path}: {banked_total} bytes already banked + {size} bytes exceeds CHECKPOINT_TOTAL_LIMIT_BYTES ({CHECKPOINT_TOTAL_LIMIT_BYTES}): hash-only; byte restore unavailable above the per-checkpoint total budget"
                            ),
                        });
                    }
                    None => {
                        // Unreachable by construction (`plan_capture` banks
                        // only for present files); defend by downgrading
                        // rather than claiming bytes that do not exist.
                        resource.byte_ref = None;
                        excluded.push(CheckpointExclusion {
                            class: concerto_core::checkpoint::CheckpointExclusionClass::OversizedFile,
                            path: Some(path.clone()),
                            reason: format!(
                                "{path}: planned bytes vanished before snapshotting: hash-only; byte restore unavailable"
                            ),
                        });
                    }
                }
            }
            included.push(resource);
        }
        if let Some(exclusion) = planned_excluded {
            excluded.push(exclusion);
        }
    }

    excluded.extend(extra_excluded);
    // Surface caller-declared (non-file) exclusions in `limitations` too:
    // `excluded` is the machine record, `limitations` is the honest summary.
    for exclusion in &excluded {
        if exclusion.path.is_none() {
            limitations.push(format!(
                "excluded {}: {}",
                exclusion_class_str(&exclusion.class),
                exclusion.reason
            ));
        }
    }
    let has_unrestorable_bytes = excluded.iter().any(|e| {
        matches!(e.class, concerto_core::checkpoint::CheckpointExclusionClass::OversizedFile)
    });
    if has_unrestorable_bytes
        && !limitations.iter().any(|l| l.contains("CHECKPOINT_TOTAL_LIMIT_BYTES"))
    {
        limitations.push(PARTIAL_SCOPE_LIMITATION.to_string());
    }

    let missing: Vec<String> = excluded.iter().filter_map(|e| e.path.clone()).collect();
    let status = if missing.is_empty() {
        CheckpointStatus::Active
    } else {
        CheckpointStatus::Partial { missing }
    };
    let restore_method = method_override.unwrap_or_else(|| default_method_for_plan(&included));

    let record = CheckpointRecord::new(
        checkpoint_id,
        session_id,
        scope,
        included,
        excluded,
        metadata,
        status,
        restore_method,
        limitations,
        created_by.to_owned(),
        created_at_ms,
        source_revision,
    )
    .map_err(|error| FileCheckpointError::Record(error.to_string()))?;
    store.create_checkpoint(&record, &blobs, cancel).await?;
    Ok(record)
}

/// Anchor a stash-backed checkpoint: commit the working tree to the stash
/// first, then record a `GitStash` checkpoint (bytes live in the stash, not
/// the sidecar — no scoped-path snapshot is taken).
#[allow(clippy::too_many_arguments)]
pub async fn create_git_stash_checkpoint(
    store: &FileCheckpointStore,
    undo: &Arc<Mutex<UndoManager>>,
    session_id: Ulid,
    task_id: TaskId,
    scope: CheckpointScope,
    created_by: &str,
    extra_excluded: Vec<CheckpointExclusion>,
    source_revision: Option<String>,
    cancel: &CancellationToken,
) -> Result<CheckpointRecord, FileCheckpointError> {
    check_cancel(cancel)?;
    {
        let mut manager = undo
            .lock()
            .map_err(|_| FileCheckpointError::Record("undo manager lock poisoned".to_string()))?;
        manager.commit(session_id, task_id).map_err(|error| {
            FileCheckpointError::Record(format!("git stash commit failed: {error}"))
        })?;
    }
    let checkpoint_id = Ulid::new();
    let mut metadata = BTreeMap::new();
    metadata.insert("stash_task_id".to_string(), task_id.to_string());
    let mut limitations = Vec::new();
    for exclusion in &extra_excluded {
        if exclusion.path.is_none() {
            limitations.push(format!(
                "excluded {}: {}",
                exclusion_class_str(&exclusion.class),
                exclusion.reason
            ));
        }
    }
    limitations
        .push("git-stash restore inherits the untracked-file gap (stash without -u)".to_string());
    let record = CheckpointRecord::new(
        checkpoint_id,
        session_id,
        scope,
        Vec::new(),
        extra_excluded,
        metadata,
        CheckpointStatus::Active,
        RestoreMethod::GitStash,
        limitations,
        created_by.to_owned(),
        concerto_core::checkpoint::now_millis(),
        source_revision,
    )
    .map_err(|error| FileCheckpointError::Record(error.to_string()))?;
    store.create_checkpoint(&record, &BTreeMap::new(), cancel).await?;
    Ok(record)
}

/// Restore a file checkpoint through the executor: policy-gated per-resource
/// writes, gate-ownership pre-check, idempotent re-restore.
///
/// `stash` wires [`UndoManager`] for `GitStash` records; `None` surfaces a
/// loud refusal for those records instead of panicking. The stash identity
/// comes from a `Task`-scoped record's task id, else the record's
/// `stash_task_id` metadata (written by [`create_git_stash_checkpoint`]).
#[allow(clippy::too_many_arguments)]
pub async fn restore_file_checkpoint(
    executor: &concerto_core::executor::ToolExecutor,
    store: &FileCheckpointStore,
    gate: &Arc<WriteGate>,
    stash: Option<&Arc<Mutex<UndoManager>>>,
    session: &SessionContext,
    checkpoint_id: &str,
    by_agent: &str,
    cancel: &CancellationToken,
) -> Result<concerto_core::checkpoint::RestoreOutcome, concerto_core::checkpoint::RestoreError> {
    use concerto_core::checkpoint::{CheckpointScope, CheckpointStore as _};
    if cancel.is_cancelled() {
        return Err(concerto_core::checkpoint::RestoreError::Cancelled);
    }
    let oracle = GateOwnershipOracle::new(gate.clone());
    // Resolve the stash identity ahead of the restore so the restorer is
    // fully determined before any write runs.
    let stash_restorer: Box<dyn concerto_core::checkpoint::StashRestorer> = match stash {
        Some(manager) => {
            let record = store
                .load_record(checkpoint_id, cancel.clone())
                .await
                .map_err(concerto_core::checkpoint::RestoreError::Store)?;
            match record {
                Some(record) => {
                    let task_id = match &record.scope {
                        CheckpointScope::Task { task_id } => Some(*task_id),
                        _ => record
                            .metadata
                            .get("stash_task_id")
                            .and_then(|raw| raw.parse::<Ulid>().ok())
                            .map(TaskId),
                    };
                    match task_id {
                        Some(task_id) => Box::new(UndoStashRestorer::new(
                            manager.clone(),
                            record.session_id,
                            task_id,
                        )),
                        None => Box::new(RefusingStashRestorer),
                    }
                }
                None => Box::new(RefusingStashRestorer),
            }
        }
        None => Box::new(RefusingStashRestorer),
    };
    executor
        .restore_checkpoint(
            checkpoint_id,
            store,
            &oracle,
            stash_restorer.as_ref(),
            session,
            by_agent,
            cancel.clone(),
        )
        .await
}

fn exclusion_class_str(
    class: &concerto_core::checkpoint::CheckpointExclusionClass,
) -> &'static str {
    match class {
        concerto_core::checkpoint::CheckpointExclusionClass::ShellSideEffect => "shell side-effect",
        concerto_core::checkpoint::CheckpointExclusionClass::Network => "network",
        concerto_core::checkpoint::CheckpointExclusionClass::Database => "database",
        concerto_core::checkpoint::CheckpointExclusionClass::Process => "process",
        concerto_core::checkpoint::CheckpointExclusionClass::OversizedFile => {
            "oversized/binary file"
        }
    }
}
