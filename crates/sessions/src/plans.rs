//! Durable planner-plan artifacts.
//!
//! The multi-agent orchestrator persists each LLM-produced plan as a
//! self-describing JSON file so a task's plan is reproducible and auditable
//! independent of the session database. Files live under
//! `<app_data_dir>/plans/plan-<run_id>.json` and are written idempotently
//! (an existing file with the same run id is overwritten).

use crate::app_data_dir;
use crate::SessionError;
use std::path::{Path, PathBuf};

/// Manages the planner-plan directory under the Concerto data root.
///
/// A manager is rooted at `<app_data_dir>/plans`; all writes are
/// best-effort-friendly (directory creation on demand, idempotent
/// overwrite) so an unwritable data root degrades to a logged skip instead
/// of failing the run.
#[derive(Debug, Clone)]
pub struct PlansManager {
    root: PathBuf,
}

impl PlansManager {
    /// Manager rooted at `<app_data_dir>/plans`, creating the directory (and
    /// the data root itself) on demand.
    pub fn open() -> Result<Self, SessionError> {
        let root = app_data_dir()?.join("plans");
        std::fs::create_dir_all(&root).map_err(|e| SessionError::Lock(e.to_string()))?;
        Ok(Self { root })
    }

    /// Manager rooted at an explicit directory (test hermeticity, unusual
    /// installs). The directory is created on demand by [`PlansManager::write_plan`].
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The plans directory this manager owns.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Path of the artifact for `plan_id` (`plan-<plan_id>.json`).
    pub fn plan_path(&self, plan_id: &str) -> PathBuf {
        self.root.join(format!("plan-{plan_id}.json"))
    }

    /// Write (or idempotently overwrite) the pretty-printed plan JSON for
    /// `plan_id`. Returns the path written.
    pub fn write_plan(&self, plan_id: &str, artifact_json: &str) -> Result<PathBuf, SessionError> {
        std::fs::create_dir_all(&self.root).map_err(|e| SessionError::Lock(e.to_string()))?;
        let path = self.plan_path(plan_id);
        std::fs::write(&path, artifact_json).map_err(|e| SessionError::Lock(e.to_string()))?;
        Ok(path)
    }

    /// Read the persisted artifact for `plan_id`, if present.
    pub fn read_plan(&self, plan_id: &str) -> Result<Option<String>, SessionError> {
        let path = self.plan_path(plan_id);
        match std::fs::read_to_string(&path) {
            Ok(contents) => Ok(Some(contents)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(SessionError::Lock(e.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_plan_is_idempotent_and_path_is_stable() {
        let dir = tempfile::tempdir().expect("tempdir for plans manager test");
        let manager = PlansManager::at(dir.path().join("plans"));

        let first = manager.write_plan("abc123", r#"{"plan_id":"abc123"}"#).expect("write plan");
        let second =
            manager.write_plan("abc123", r#"{"plan_id":"abc123","v":2}"#).expect("rewrite plan");

        // Same id => same file, second write overwrote the first.
        assert_eq!(first, second, "rewriting a plan id must target the same file");
        assert_eq!(
            manager.read_plan("abc123").expect("read plan").as_deref(),
            Some(r#"{"plan_id":"abc123","v":2}"#),
            "the second write must overwrite the first"
        );
        // Distinct ids never collide.
        let other = manager.write_plan("xyz", "{}").expect("write other plan");
        assert_ne!(first, other);
        assert_ne!(manager.plan_path("abc123"), manager.plan_path("xyz"));
    }

    #[test]
    fn read_plan_missing_id_is_none() {
        let dir = tempfile::tempdir().expect("tempdir for plans manager test");
        let manager = PlansManager::at(dir.path().join("plans"));
        assert_eq!(
            manager.read_plan("does-not-exist").expect("read missing plan"),
            None,
            "a missing plan id must read as None, not an error"
        );
    }
}
