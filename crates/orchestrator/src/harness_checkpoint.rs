//! H13 — `checkpoint_create` / `checkpoint_inspect` / `checkpoint_restore`
//! operations over the one harness service seam.
//!
//! These turn the existing [`crate::file_checkpoint`] orchestration into
//! agent- and adapter-callable operations without a second implementation:
//! every call reaches the same `create_file_checkpoint` / `restore_file_checkpoint`
//! functions the vertical slice already exercised, through the normal
//! policy/audit executor path.
//!
//! - [`CheckpointCreateTool`] and [`CheckpointInspectTool`] are ordinary
//!   registered tools invoked through
//!   [`ToolExecutor::execute`][concerto_core::executor::ToolExecutor::execute],
//!   so a request is policy-evaluated and audited like any other tool.
//! - `checkpoint_restore` is a service-level operation
//!   ([`crate::harness_service::HarnessService::checkpoint_restore`]) because a
//!   restore must itself issue gated `filesystem` writes: it calls
//!   [`ToolExecutor::restore_checkpoint`][concerto_core::executor::ToolExecutor::restore_checkpoint],
//!   the executor's dedicated restore pipeline (per-resource policy + audit).
//!   Restore is operator/frontend-scoped **by design**: rewriting workspace
//!   state is never something an agent invokes on itself mid-run, so restore
//!   is reachable from the CLI, the desktop, and the standalone service —
//!   not from an agent's own tool registry. (The codebase already holds the
//!   precedent for making such an operation agent-reachable later — the
//!   lazy-resolver pattern behind `SharedMemoryResolver` — but this release
//!   deliberately does not.)
//!
//! No adapter may touch the checkpoint store directly: the tools own the store
//! handle, and adapters only ever see the service's [`ToolOutput`] payload.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use concerto_core::checkpoint::{
    CheckpointExclusion, CheckpointRecord, CheckpointScope, CheckpointStatus, RestoreMethod,
    RestoreOutcome,
};
use concerto_core::traits::tool::Tool;
use concerto_core::traits::PolicyEngine;
use concerto_core::types::{CapabilitySet, SessionContext, ToolOutput};
use concerto_core::{CancellationToken, ToolError};
use concerto_sessions::file_checkpoints::FileCheckpointStore;
use serde::{Deserialize, Serialize};

/// Registry name of the create operation.
pub const CHECKPOINT_CREATE_TOOL_NAME: &str = "checkpoint_create";
/// Registry name of the inspect operation.
pub const CHECKPOINT_INSPECT_TOOL_NAME: &str = "checkpoint_inspect";
/// Service-level operation name for restore.
pub const CHECKPOINT_RESTORE_TOOL_NAME: &str = "checkpoint_restore";

/// Input for `checkpoint_create`.
///
/// `paths` are canonical project-relative paths. `scope` defaults to
/// [`CheckpointScope::FilesystemWrites`] over `paths`; a `Task`/`Run` scope is
/// accepted verbatim. `extra_excluded` carries caller-declared non-file
/// effects (shell/network/database/process), which the capture loop never
/// infers — an undeclared side effect is out of the restore claim by
/// construction (see [`crate::file_checkpoint::create_file_checkpoint`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointCreateInput {
    /// Canonical project-relative paths to anchor.
    #[serde(default)]
    pub paths: Vec<String>,
    /// Explicit scope; `None` means `FilesystemWrites { paths }`.
    #[serde(default)]
    pub scope: Option<CheckpointScope>,
    /// Bounded operator annotations.
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
    /// Caller-declared, never-reversed non-file effects.
    #[serde(default)]
    pub extra_excluded: Vec<CheckpointExclusion>,
    /// Advisory source revision for the audit trail.
    #[serde(default)]
    pub source_revision: Option<String>,
}

/// Input for `checkpoint_inspect`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointInspectInput {
    /// The checkpoint id to load.
    pub checkpoint_id: String,
}

/// Input for `checkpoint_restore`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointRestoreInput {
    /// The checkpoint id to restore.
    pub checkpoint_id: String,
    /// Attribution for the restore audit trail; defaults to `harness`.
    #[serde(default)]
    pub by_agent: Option<String>,
}

/// Serializable projection of [`RestoreOutcome`] — the service's restore
/// evidence shape (the domain enum carries no serde derive). Field-for-field;
/// no outcome is collapsed or invented. Every variant carries the
/// [`RestoreMethod`] the restore actually ran, so an operator reading the
/// projection never has to reverse-engineer which restore branch ran from
/// the `status` tag alone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum HarnessRestoreOutcome {
    /// Every included resource was restored through gated writes.
    Restored {
        /// The restore method that ran (always `pre_image_store` here).
        method: RestoreMethod,
        /// Canonical paths written back (byte-exact).
        restored: Vec<String>,
        /// Paths recreated (absent at restore time, bytes banked).
        recreated: Vec<String>,
        /// Paths deleted (absent at checkpoint time).
        deleted: Vec<String>,
        /// Paths skipped because a verified journal row satisfied both halves
        /// of the skip invariant.
        verified_skips: Vec<String>,
        /// Provenance notes.
        notes: Vec<String>,
    },
    /// The checkpoint was already restored (recorded no-op).
    AlreadyRestored {
        /// The restore method the FIRST restore ran.
        method: RestoreMethod,
        /// Unix epoch millis of the first restore.
        at_ms: i64,
        /// Who performed the first restore.
        by: String,
    },
    /// The byte-carrying subset restored; `missing` names hash-only paths.
    Partial {
        /// The restore method that ran (always `pre_image_store` here).
        method: RestoreMethod,
        /// Canonical paths written back.
        restored: Vec<String>,
        /// Canonical paths without restorable bytes.
        missing: Vec<String>,
        /// Why each missing path has no bytes.
        limitations: Vec<String>,
    },
    /// Nothing was written; the operator follows `limitations`.
    RequiresOperator {
        /// The restore method (always `manual` here).
        method: RestoreMethod,
        /// Why auto-restore is unavailable.
        limitations: Vec<String>,
    },
    /// The stash pop ran outside the byte path.
    StashRestored {
        /// The restore method (always `git_stash` here).
        method: RestoreMethod,
        /// What the stash restorer reported.
        detail: String,
    },
}

impl From<RestoreOutcome> for HarnessRestoreOutcome {
    fn from(outcome: RestoreOutcome) -> Self {
        match outcome {
            RestoreOutcome::Restored {
                method,
                restored,
                recreated,
                deleted,
                verified_skips,
                notes,
            } => Self::Restored { method, restored, recreated, deleted, verified_skips, notes },
            RestoreOutcome::AlreadyRestored { method, at_ms, by } => {
                Self::AlreadyRestored { method, at_ms, by }
            }
            RestoreOutcome::Partial { method, restored, missing, limitations } => {
                Self::Partial { method, restored, missing, limitations }
            }
            RestoreOutcome::RequiresOperator { method, limitations } => {
                Self::RequiresOperator { method, limitations }
            }
            RestoreOutcome::StashRestored { method, detail } => {
                Self::StashRestored { method, detail }
            }
        }
    }
}

impl HarnessRestoreOutcome {
    /// Short human-facing summary of which restore branch ran.
    pub fn summary(&self) -> &'static str {
        match self {
            Self::Restored { .. } => "restored",
            Self::AlreadyRestored { .. } => "already-restored",
            Self::Partial { .. } => "partial",
            Self::RequiresOperator { .. } => "requires-operator",
            Self::StashRestored { .. } => "stash-restored",
        }
    }
}

/// The `checkpoint_create` operation.
pub struct CheckpointCreateTool {
    store: Arc<FileCheckpointStore>,
    project_root: PathBuf,
}

impl CheckpointCreateTool {
    /// Attach to a checkpoint store rooted at `project_root`.
    pub fn new(store: Arc<FileCheckpointStore>, project_root: PathBuf) -> Self {
        Self { store, project_root }
    }
}

#[async_trait]
impl Tool for CheckpointCreateTool {
    fn name(&self) -> &str {
        CHECKPOINT_CREATE_TOOL_NAME
    }

    fn description(&self) -> &str {
        "Anchor a file checkpoint: snapshot the checkpoint-time bytes of scoped project-relative \
         paths, plan each through the shared budget rule, and persist record + blobs. Over-budget \
         content is hash-only and declared partial; shell/network/database/process effects are \
         caller-declared exclusions and are never reversed."
    }

    fn input_schema(&self) -> serde_json::Value {
        // Hand-written schema (the checkpoint vocabulary holds `Ulid`, which
        // does not implement `JsonSchema`); it mirrors `CheckpointCreateInput`
        // exactly and the fallback is the same shape, so drift is impossible.
        serde_json::json!({
            "type": "object",
            "properties": {
                "paths": { "type": "array", "items": { "type": "string" } },
                "scope": {
                    "type": ["object", "null"],
                    "description": "FilesystemWrites{paths} | Task{task_id} | Run"
                },
                "metadata": { "type": "object", "additionalProperties": { "type": "string" } },
                "extra_excluded": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "class": { "type": "string" },
                            "path": { "type": ["string", "null"] },
                            "reason": { "type": "string" }
                        }
                    }
                },
                "source_revision": { "type": ["string", "null"] }
            }
        })
    }

    fn capability_requirements(&self) -> CapabilitySet {
        // Empty like `environment_describe`: discovery is free; whether a call
        // runs is the policy engine's decision through the executor gate.
        CapabilitySet::default()
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        _policy: &dyn PolicyEngine,
        session: &SessionContext,
        cancel: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        if cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        let parsed: CheckpointCreateInput =
            serde_json::from_value(input).map_err(|error| ToolError::ExecutionFailed {
                message: format!("invalid checkpoint_create input: {error}"),
            })?;
        let scope = parsed
            .scope
            .unwrap_or_else(|| CheckpointScope::FilesystemWrites { paths: parsed.paths.clone() });
        let record = crate::file_checkpoint::create_file_checkpoint(
            &self.store,
            &self.project_root,
            session.session_id,
            scope,
            parsed.paths,
            CHECKPOINT_CREATE_TOOL_NAME,
            parsed.metadata,
            parsed.extra_excluded,
            None,
            parsed.source_revision,
            &cancel,
        )
        .await
        .map_err(file_checkpoint_error)?;
        let data = record_data(&record);
        Ok(ToolOutput {
            summary: format!(
                "checkpoint_create id={} status={} included={} excluded={}",
                record.checkpoint_id,
                record.status.state_str(),
                record.included.len(),
                record.excluded.len(),
            ),
            data,
        })
    }
}

/// The `checkpoint_inspect` operation: read one checkpoint record.
pub struct CheckpointInspectTool {
    store: Arc<FileCheckpointStore>,
}

impl CheckpointInspectTool {
    /// Attach to a checkpoint store.
    pub fn new(store: Arc<FileCheckpointStore>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl Tool for CheckpointInspectTool {
    fn name(&self) -> &str {
        CHECKPOINT_INSPECT_TOOL_NAME
    }

    fn description(&self) -> &str {
        "Inspect one file checkpoint by id: scope, included/excluded resources, lifecycle status, \
         restore method, and the honest limitations. Read-only. Returns `found:false` when the id \
         is unknown rather than synthesizing a record."
    }

    fn input_schema(&self) -> serde_json::Value {
        // Hand-written schema mirroring `CheckpointInspectInput`.
        serde_json::json!({
            "type": "object",
            "properties": { "checkpoint_id": { "type": "string" } },
            "required": ["checkpoint_id"]
        })
    }

    fn capability_requirements(&self) -> CapabilitySet {
        CapabilitySet::default()
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        _policy: &dyn PolicyEngine,
        _session: &SessionContext,
        cancel: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        if cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        let parsed: CheckpointInspectInput =
            serde_json::from_value(input).map_err(|error| ToolError::ExecutionFailed {
                message: format!("invalid checkpoint_inspect input: {error}"),
            })?;
        let record =
            self.store.load_checkpoint_record(&parsed.checkpoint_id, &cancel).await.map_err(
                |error| ToolError::ExecutionFailed {
                    message: format!("checkpoint_inspect store read failed: {error}"),
                },
            )?;
        match record {
            Some(record) => Ok(ToolOutput {
                summary: format!(
                    "checkpoint_inspect id={} status={}",
                    record.checkpoint_id,
                    record.status.state_str()
                ),
                data: serde_json::json!({
                    "found": true,
                    "record": record_data(&record),
                }),
            }),
            None => Ok(ToolOutput {
                summary: format!("checkpoint_inspect id={} not found", parsed.checkpoint_id),
                data: serde_json::json!({ "found": false, "checkpoint_id": parsed.checkpoint_id }),
            }),
        }
    }
}

/// The service's checkpoint-record payload: the record's own serde shape plus
/// a stable `status` label already carried inside the record.
fn record_data(record: &CheckpointRecord) -> serde_json::Value {
    serde_json::to_value(record).unwrap_or_else(|error| {
        tracing::error!(%error, "failed to serialize CheckpointRecord");
        serde_json::json!({
            "checkpoint_id": record.checkpoint_id.to_string(),
            "status": record.status,
            "serialization_error": true,
        })
    })
}

/// Map a [`crate::file_checkpoint::FileCheckpointError`] onto a [`ToolError`].
pub fn file_checkpoint_error(error: crate::file_checkpoint::FileCheckpointError) -> ToolError {
    use crate::file_checkpoint::FileCheckpointError;
    match error {
        FileCheckpointError::Cancelled => ToolError::Cancelled,
        other => ToolError::ExecutionFailed { message: other.to_string() },
    }
}

/// Convenience: the canonical scope for a create input (exposed for callers
/// that need to echo the effective scope without re-deriving it).
pub fn effective_scope(input: &CheckpointCreateInput) -> CheckpointScope {
    input
        .scope
        .clone()
        .unwrap_or_else(|| CheckpointScope::FilesystemWrites { paths: input.paths.clone() })
}

/// Whether a status is a disposal/refusal state a caller must surface. Kept
/// beside the record projection so desktop/CLI render the same truth.
pub fn status_is_refusal(status: &CheckpointStatus) -> bool {
    matches!(
        status,
        CheckpointStatus::Expired { .. }
            | CheckpointStatus::Superseded
            | CheckpointStatus::Partial { .. }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restore_outcome_projection_is_field_for_field() {
        let outcome = RestoreOutcome::Restored {
            method: RestoreMethod::PreImageStore,
            restored: vec!["a".into()],
            recreated: vec!["b".into()],
            deleted: vec!["c".into()],
            verified_skips: vec!["d".into()],
            notes: vec!["n".into()],
        };
        let projected: HarnessRestoreOutcome = outcome.into();
        assert_eq!(projected.summary(), "restored");
        let value = serde_json::to_value(&projected).expect("serializes");
        assert_eq!(value["status"], "restored");
        assert_eq!(value["method"], "pre_image_store");
        assert_eq!(value["restored"][0], "a");
        assert_eq!(value["verified_skips"][0], "d");
    }

    #[test]
    fn already_restored_and_partial_round_trip() {
        for outcome in [
            HarnessRestoreOutcome::AlreadyRestored {
                method: RestoreMethod::PreImageStore,
                at_ms: 7,
                by: "agent".into(),
            },
            HarnessRestoreOutcome::Partial {
                method: RestoreMethod::PreImageStore,
                restored: vec!["x".into()],
                missing: vec!["y".into()],
                limitations: vec!["hash-only".into()],
            },
        ] {
            let json = serde_json::to_string(&outcome).expect("serializes");
            let back: HarnessRestoreOutcome = serde_json::from_str(&json).expect("round-trips");
            assert_eq!(back, outcome);
        }
    }

    #[test]
    fn effective_scope_defaults_to_filesystem_writes() {
        let input = CheckpointCreateInput {
            paths: vec!["src/lib.rs".into()],
            scope: None,
            metadata: BTreeMap::new(),
            extra_excluded: Vec::new(),
            source_revision: None,
        };
        assert_eq!(
            effective_scope(&input),
            CheckpointScope::FilesystemWrites { paths: vec!["src/lib.rs".into()] }
        );
    }

    #[test]
    fn refusal_statuses_are_flagged() {
        assert!(status_is_refusal(&CheckpointStatus::Superseded));
        assert!(status_is_refusal(&CheckpointStatus::Partial { missing: vec!["a".into()] }));
        assert!(!status_is_refusal(&CheckpointStatus::Active));
    }
}
