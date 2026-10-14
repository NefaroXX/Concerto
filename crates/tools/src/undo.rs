//! Undo manager for git‑stash based session rollback.
//!
//! Provides a simple stack of stashes created before tool execution and the
//! ability to pop them back on undo. All operations are synchronous and use
//! `std::process::Command` to invoke the system `git` binary.

use std::path::PathBuf;
use std::process::Command;
use time::OffsetDateTime;

use concerto_core::ids::Ulid;
use concerto_core::{TaskId, ToolError, UndoError};

/// Entry tracking a single stash created for a session/task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UndoEntry {
    pub session_id: Ulid,
    pub task_id: TaskId,
    pub stash_message: String,
    pub timestamp: OffsetDateTime,
}

/// Manager that records stashes and can roll them back.
#[derive(Debug)]
pub struct UndoManager {
    project_dir: PathBuf,
    stack: Vec<UndoEntry>,
}

impl UndoManager {
    /// Create a new manager for the given project directory.
    pub fn new(project_dir: impl Into<PathBuf>) -> Self {
        Self { project_dir: project_dir.into(), stack: Vec::new() }
    }

    /// Create a git stash for the given session/task.
    ///
    /// Returns [`ToolError::Io`] when the `git` binary itself cannot be
    /// spawned (H02 construction-site typing); a failing-but-spawned `git`
    /// (non-zero exit) still surfaces its stderr as `ExecutionFailed`.
    pub fn commit(&mut self, session_id: Ulid, task_id: TaskId) -> Result<(), ToolError> {
        // Ensure we are inside a git repository.
        let rev_parse = Command::new("git")
            .args(["rev-parse", "--git-dir"])
            .current_dir(&self.project_dir)
            .output();
        let rev_output = rev_parse.map_err(|e| {
            ToolError::Io(std::io::Error::new(e.kind(), format!("git rev-parse failed: {e}")))
        })?;
        if !rev_output.status.success() {
            return Err(ToolError::ExecutionFailed {
                message: String::from_utf8_lossy(&rev_output.stderr).into_owned(),
            });
        }

        // Build stash message.
        let stash_msg = format!("concerto-{session_id}-{task_id}");
        let stash = Command::new("git")
            .args(["stash", "push", "-m", &stash_msg])
            .current_dir(&self.project_dir)
            .output()
            .map_err(|e| {
                ToolError::Io(std::io::Error::new(e.kind(), format!("git stash push failed: {e}")))
            })?;
        if !stash.status.success() {
            return Err(ToolError::ExecutionFailed {
                message: String::from_utf8_lossy(&stash.stderr).into_owned(),
            });
        }

        // Record the entry.
        self.stack.push(UndoEntry {
            session_id,
            task_id,
            stash_message: stash_msg,
            timestamp: OffsetDateTime::now_utc(),
        });
        Ok(())
    }

    /// Pop the stash associated with the given session/task.
    ///
    /// Returns `UndoError::StashNotFound` if no matching entry exists, or
    /// `UndoError::StashPopFailed` if the git command fails.
    pub fn rollback(&mut self, task_id: TaskId, session_id: Ulid) -> Result<(), UndoError> {
        // Find the entry index.
        let idx = self
            .stack
            .iter()
            .position(|e| e.session_id == session_id && e.task_id == task_id)
            .ok_or(UndoError::StashNotFound { session_id })?;

        let stash_msg = &self.stack[idx].stash_message;

        // List all stashes and find the one matching our message.
        let list_output = Command::new("git")
            .args(["stash", "list"])
            .current_dir(&self.project_dir)
            .output()
            .map_err(|e| UndoError::StashPopFailed {
                reason: format!("git stash list failed: {e}"),
            })?;
        let stash_ref = String::from_utf8_lossy(&list_output.stdout)
            .lines()
            .find(|line| line.contains(stash_msg.as_str()))
            .and_then(|line| line.split(':').next())
            .map(|s| s.to_string())
            .ok_or(UndoError::StashNotFound { session_id })?;

        // Pop the specific stash (not stash@{0}).
        let pop = Command::new("git")
            .args(["stash", "pop", &stash_ref])
            .current_dir(&self.project_dir)
            .output()
            .map_err(|e| UndoError::StashPopFailed {
                reason: format!("git stash pop failed: {e}"),
            })?;
        if !pop.status.success() {
            return Err(UndoError::StashPopFailed {
                reason: String::from_utf8_lossy(&pop.stderr).into_owned(),
            });
        }

        // Remove the entry.
        self.stack.remove(idx);
        Ok(())
    }

    /// Return a slice of all recorded entries.
    pub fn list(&self) -> &[UndoEntry] {
        &self.stack
    }

    /// Remove all entries belonging to the given session.
    pub fn clear_session(&mut self, session_id: Ulid) {
        self.stack.retain(|e| e.session_id != session_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write;
    use std::path::Path;
    use std::process::Command;
    use tempfile::TempDir;

    /// Create a minimal git repo at `dir` with an initial commit.
    fn init_git_repo(dir: &Path) {
        Command::new("git").args(["init"]).current_dir(dir).output().expect("git init failed");
        // Configure user to avoid commit warnings.
        Command::new("git")
            .args(["config", "user.email", "test@example.com"])
            .current_dir(dir)
            .output()
            .expect("git config email failed");
        Command::new("git")
            .args(["config", "user.name", "Test User"])
            .current_dir(dir)
            .output()
            .expect("git config name failed");

        // Create an initial commit so git stash push works.
        let readme = dir.join("README.md");
        std::fs::write(&readme, "# test\n").expect("write readme");
        Command::new("git").args(["add", "."]).current_dir(dir).output().expect("git add failed");
        Command::new("git")
            .args(["commit", "-m", "initial"])
            .current_dir(dir)
            .output()
            .expect("git commit failed");
    }

    #[test]
    fn construction_and_list() {
        let tmp = TempDir::new().unwrap();
        init_git_repo(tmp.path());
        let mgr = UndoManager::new(tmp.path());
        assert!(mgr.list().is_empty());
    }

    /// H02 construction-site typing: a `git` spawn failure (here: the
    /// project directory does not exist, so the child fails before exec)
    /// surfaces as the structural `ToolError::Io` — classified by
    /// `ErrorKind`, never by message markers — with the pre-typing
    /// "git rev-parse failed" verb riding the io payload. Context presence
    /// never flips classification: the same kind with only the bare OS text
    /// yields the identical ledger + envelope disposition.
    #[test]
    fn commit_spawn_failure_yields_typed_io_with_context() -> Result<(), Box<dyn std::error::Error>>
    {
        let dir = TempDir::new()?;
        let missing = dir.path().join("missing-project");
        let native_error = match Command::new("git")
            .args(["rev-parse", "--git-dir"])
            .current_dir(&missing)
            .output()
        {
            Err(error) => error,
            Ok(_) => panic!("spawn in a missing project dir must fail"),
        };
        let mut mgr = UndoManager::new(missing);
        let error = match mgr.commit(Ulid::new(), TaskId::new()) {
            Err(error) => error,
            Ok(_) => panic!("missing project dir must fail"),
        };
        let kind = match &error {
            ToolError::Io(io) => io.kind(),
            other => panic!("expected typed ToolError::Io, got {other:?}"),
        };
        // Windows reports NotADirectory here; Unix reports NotFound.
        assert_eq!(kind, native_error.kind(), "the OS verdict is preserved");
        let display = error.to_string();
        assert!(display.contains("git rev-parse failed"), "missing spawn verb: {display}");
        assert!(error.is_deterministic_failure(), "the kind classifies structurally");
        let failure = concerto_core::tool_failure::ToolFailure::from_error(
            &error,
            concerto_core::tool_failure::ToolFailureContext::builtin(),
        );
        assert_eq!(failure.code, "exec.io_error", "the envelope is typed");
        assert_eq!(
            failure.disposition,
            concerto_core::tool_failure::ToolDisposition::DoNotReIssue,
            "the disposition derives from the kind"
        );
        let bare =
            ToolError::Io(std::io::Error::new(kind, "No such file or directory (os error 2)"));
        assert_eq!(
            error.is_deterministic_failure(),
            bare.is_deterministic_failure(),
            "classification is kind-for-kind, context text is inert"
        );
        let bare_failure = concerto_core::tool_failure::ToolFailure::from_error(
            &bare,
            concerto_core::tool_failure::ToolFailureContext::builtin(),
        );
        assert_eq!(failure.code, bare_failure.code, "same envelope code");
        assert_eq!(
            failure.disposition, bare_failure.disposition,
            "same disposition with and without context text"
        );
        Ok(())
    }

    #[test]
    fn clear_session_removes_entries() {
        let tmp = TempDir::new().unwrap();
        init_git_repo(tmp.path());
        // Modify a tracked file to create a dirty state for git stash.
        let file_path = tmp.path().join("README.md");
        let mut f = File::create(&file_path).unwrap();
        writeln!(f, "change").unwrap();
        let mut mgr = UndoManager::new(tmp.path());
        let session = Ulid::new();
        let task = TaskId::new();
        mgr.commit(session, task).unwrap();
        assert_eq!(mgr.list().len(), 1);
        mgr.clear_session(session);
        assert!(mgr.list().is_empty());
    }

    #[test]
    fn rollback_missing_entry_returns_error() {
        let tmp = TempDir::new().unwrap();
        init_git_repo(tmp.path());
        let mut mgr = UndoManager::new(tmp.path());
        let session = Ulid::new();
        let task = TaskId::new();
        let err = mgr.rollback(task, session).unwrap_err();
        match err {
            UndoError::StashNotFound { session_id } => assert_eq!(session_id, session),
            _ => panic!("unexpected error variant"),
        }
    }
}
