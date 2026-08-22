use async_trait::async_trait;
use camino::Utf8PathBuf;
use concerto_core::error::PolicyError;
use concerto_core::traits::policy::AuditEntry;
use concerto_core::traits::PolicyEngine;
use concerto_core::types::PolicyVerdict;
use concerto_core::CancellationToken;
use std::process::Command;
use tempfile::TempDir;

/// A test policy that allows everything. Use in tool tests that need
/// to exercise the `Tool::execute` path through a real policy reference
/// without requiring approval.
#[cfg(test)]
pub struct AllowAllPolicy;

#[cfg(test)]
#[async_trait]
impl PolicyEngine for AllowAllPolicy {
    async fn evaluate(
        &self,
        _action: &concerto_core::types::PolicyAction<'_>,
        _cancel: CancellationToken,
    ) -> Result<PolicyVerdict, PolicyError> {
        Ok(PolicyVerdict::Allow)
    }

    fn audit_log(&self) -> &dyn concerto_core::traits::policy::AuditLog {
        &IgnoreAudit
    }
}

#[cfg(test)]
struct IgnoreAudit;

#[cfg(test)]
#[async_trait]
impl concerto_core::traits::policy::AuditLog for IgnoreAudit {
    async fn record(
        &self,
        _entry: AuditEntry,
        _cancel: CancellationToken,
    ) -> Result<(), PolicyError> {
        Ok(())
    }
}

/// A temporary git repository for testing git operations.
#[cfg(test)]
pub struct TempGitRepo {
    pub dir: TempDir,
    pub path: Utf8PathBuf,
}

#[cfg(test)]
impl TempGitRepo {
    /// Creates a new temp directory, initialises a git repo, adds an
    /// initial commit so the repo has a HEAD.
    pub fn new() -> Self {
        let dir = TempDir::new().expect("failed to create temp dir");
        let path = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("non-UTF-8 path");

        // git init
        let status = Command::new("git")
            .args(["init"])
            .current_dir(&path)
            .status()
            .expect("failed to run git init");
        assert!(status.success());

        // Configure user for commits
        Command::new("git")
            .args(["config", "user.email", "test@concerto.rs"])
            .current_dir(&path)
            .status()
            .expect("failed to set git user.email");
        Command::new("git")
            .args(["config", "user.name", "Concerto Test"])
            .current_dir(&path)
            .status()
            .expect("failed to set git user.name");

        // Create initial commit
        Self { dir, path }.commit_file("README.md", "# Test Repo", "initial commit")
    }

    /// Writes `content` to `path` (relative to repo root), stages, and commits.
    pub fn commit_file(&self, relative_path: &str, content: &str, message: &str) -> Self
    where
        Self: Sized,
    {
        let file_path = self.path.join(relative_path);
        std::fs::write(&file_path, content).expect("failed to write file");

        let status = Command::new("git")
            .args(["add", relative_path])
            .current_dir(&self.path)
            .status()
            .expect("failed to git add");
        assert!(status.success());

        let status = Command::new("git")
            .args(["commit", "-m", message])
            .current_dir(&self.path)
            .status()
            .expect("failed to git commit");
        assert!(status.success());

        Self { dir: TempDir::new().expect("failed to recreate temp dir"), path: self.path.clone() }
    }
}

#[cfg(test)]
impl Default for TempGitRepo {
    fn default() -> Self {
        Self::new()
    }
}

/// A builder for setting up filesystem state in tests.
/// When VirtualFs is available, call `.build_vfs()` instead of `.build()`.
#[cfg(test)]
pub struct VirtualFsTestBuilder {
    root: TempDir,
    files: Vec<(String, String)>,
}

#[cfg(test)]
impl VirtualFsTestBuilder {
    pub fn new() -> Self {
        Self { root: TempDir::new().expect("failed to create temp dir"), files: Vec::new() }
    }

    pub fn with_file(mut self, path: &str, content: &str) -> Self {
        self.files.push((path.to_string(), content.to_string()));
        self
    }

    /// Returns just the root path after writing files. Use when you don't
    /// need a VirtualFs wrapper.
    pub fn build(self) -> TempDir {
        self.write_files();
        self.root
    }

    fn write_files(&self) {
        for (relative_path, content) in &self.files {
            let file_path = self.root.path().join(relative_path);
            if let Some(parent) = file_path.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            std::fs::write(&file_path, content).expect("failed to write file");
        }
    }
}

#[cfg(test)]
impl Default for VirtualFsTestBuilder {
    fn default() -> Self {
        Self::new()
    }
}
