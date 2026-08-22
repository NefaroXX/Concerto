//! Virtual filesystem for staging file changes before committing to disk.
//!
//! All writes mutate an in-memory map only. Changes are flushed to the real
//! filesystem only when `commit_to_disk` is called.
//!
//! Commits are deterministic and validated. The entire staged set is checked
//! before anything is applied (preflight), entries are applied in ascending
//! byte order of their paths (never `HashMap` iteration order), and any
//! apply-phase failure returns a structured [`CommitError`] listing exactly
//! which paths were applied and which were not.

use camino::{Utf8Path, Utf8PathBuf};
use concerto_core::text::normalize_typographic;
use concerto_core::ToolError;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::collections::HashSet;

/// Returns the reserved Windows device name ([`crate::containment::WINDOWS_DEVICE_NAMES`])
/// when `path`'s final basename is one, matched case-insensitively. Only the
/// exact basename matches — `nul.txt` and `com10` are ordinary files and
/// return `None`. Windows normally refuses these names, but the `\\?\`
/// extended-length path prefix bypasses that check, so a literal 0-byte file
/// named `nul` can be materialized; the filesystem read/write/delete paths
/// reject such a target up front instead of touching a device or creating a
/// real file.
pub(crate) fn reserved_device_name(path: &Utf8Path) -> Option<&'static str> {
    let name = path.file_name()?;
    crate::containment::WINDOWS_DEVICE_NAMES
        .iter()
        .copied()
        .find(|device| name.eq_ignore_ascii_case(device))
}

/// Human-readable rejection reason for a reserved Windows device name.
pub(crate) fn reserved_device_reason(device: &str) -> String {
    format!("reserved Windows device name '{device}' cannot be used as a file path")
}

/// Rejects a resolved path whose final basename is a Windows reserved device
/// name (e.g. `nul`) with a clear error instead of creating/mutating a real
/// file (or hitting the device). `nul.txt` and `com10` are unaffected.
pub(crate) fn reject_reserved_device_name(path: &Utf8Path) -> Result<(), ToolError> {
    match reserved_device_name(path) {
        Some(device) => Err(ToolError::ExecutionFailed { message: reserved_device_reason(device) }),
        None => Ok(()),
    }
}

/// A single entry in the virtual filesystem.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum VirtualFsEntry {
    /// File exists on disk and has not been modified.
    Original { path: Utf8PathBuf, content: String },
    /// File exists on disk and has been modified in-memory.
    Modified { path: Utf8PathBuf, original: String, current: String },
    /// File existed on disk but has been deleted in-memory.
    Deleted { path: Utf8PathBuf, original: String },
    /// File did not exist on disk and has been created in-memory.
    Created { path: Utf8PathBuf, current: String },
}

impl VirtualFsEntry {
    /// Returns the path associated with this entry.
    pub fn path(&self) -> &Utf8Path {
        match self {
            VirtualFsEntry::Original { path, .. } => path,
            VirtualFsEntry::Modified { path, .. } => path,
            VirtualFsEntry::Deleted { path, .. } => path,
            VirtualFsEntry::Created { path, .. } => path,
        }
    }

    /// Returns the current content of this entry, if any.
    pub fn current_content(&self) -> Option<&str> {
        match self {
            VirtualFsEntry::Original { content, .. } => Some(content),
            VirtualFsEntry::Modified { current, .. } => Some(current),
            VirtualFsEntry::Deleted { .. } => None,
            VirtualFsEntry::Created { current, .. } => Some(current),
        }
    }

    /// Returns the original content of this entry, if it existed on disk.
    pub fn original_content(&self) -> Option<&str> {
        match self {
            VirtualFsEntry::Original { content, .. } => Some(content),
            VirtualFsEntry::Modified { original, .. } => Some(original),
            VirtualFsEntry::Deleted { original, .. } => Some(original),
            VirtualFsEntry::Created { .. } => None,
        }
    }
}

/// Lightweight snapshot for checkpoint/restore operations.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct VirtualFsSnapshot {
    entries: HashMap<Utf8PathBuf, VirtualFsEntry>,
}

impl VirtualFsSnapshot {
    /// Creates a new empty snapshot.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the number of entries in the snapshot.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns true if the snapshot contains no entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// In-memory virtual filesystem backed by a `HashMap`.
///
/// All operations mutate the internal map only. Changes are not written to
/// the real filesystem until `commit_to_disk` is called.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct VirtualFs {
    entries: HashMap<Utf8PathBuf, VirtualFsEntry>,
}

/// Reads a file from disk as text, tolerating binary content.
///
/// Files are read as raw bytes and decoded as UTF-8: valid text comes back
/// verbatim, while non-UTF-8 (binary) content yields a short informative
/// placeholder instead of an error, so reading or staging a binary file can
/// never fail a tool call. The placeholder carries the byte count for
/// transparency and deliberately omits the raw bytes.
fn read_disk_text(path: &std::path::Path) -> Result<String, ToolError> {
    let bytes = std::fs::read(path).map_err(ToolError::Io)?;
    match String::from_utf8(bytes) {
        Ok(text) => Ok(text),
        Err(decoded) => Ok(format!(
            "[binary file: {} bytes — contents not decoded]",
            decoded.into_bytes().len()
        )),
    }
}

impl VirtualFs {
    /// Returns a list of paths that have been modified, created, or deleted.
    pub fn changed_paths(&self) -> Vec<&Utf8Path> {
        self.entries
            .iter()
            .filter(|(_, entry)| !matches!(entry, VirtualFsEntry::Original { .. }))
            .map(|(path, _)| path.as_path())
            .collect()
    }
    /// Creates a new empty virtual filesystem.
    pub fn new() -> Self {
        Self::default()
    }

    /// Reads the content of a file at the given path.
    ///
    /// Returns an error if the file does not exist or has been deleted.
    pub fn read(&self, path: &Utf8Path) -> Result<String, ToolError> {
        reject_reserved_device_name(path)?;
        let entry = self.entries.get(path).ok_or_else(|| ToolError::ExecutionFailed {
            message: format!("file not found: {path}"),
        })?;

        match entry {
            VirtualFsEntry::Deleted { .. } => Err(ToolError::ExecutionFailed {
                message: format!("file has been deleted: {path}"),
            }),
            VirtualFsEntry::Original { content, .. } => Ok(content.clone()),
            VirtualFsEntry::Modified { current, .. } => Ok(current.clone()),
            VirtualFsEntry::Created { current, .. } => Ok(current.clone()),
        }
    }

    /// Writes content to a file at the given path.
    ///
    /// If the file exists, it becomes `Modified`. If it does not exist, it
    /// becomes `Created`.
    pub fn write(&mut self, path: &Utf8Path, content: String) -> Result<(), ToolError> {
        // Normalize typographic Unicode at the VFS write boundary (defense in
        // depth on top of the filesystem tool): any caller that stages text
        // produced by a model — the tool, the desktop diff viewer, plugins —
        // lands ASCII quotes/dashes instead of smart punctuation that would
        // break parsing. The normalizer returns a borrow when the input is
        // already clean, so plain content is zero-allocation. Only file
        // content is normalized, never the path.
        let content = normalize_typographic(&content).into_owned();
        let path_buf = path.to_path_buf();

        match self.entries.get_mut(&path_buf) {
            Some(VirtualFsEntry::Original { content: orig_content, .. }) => {
                let original = orig_content.clone();
                let current = content; // use the method parameter
                self.entries.insert(
                    path_buf,
                    VirtualFsEntry::Modified { path: path.to_path_buf(), original, current },
                );
            }
            Some(VirtualFsEntry::Modified { original, .. }) => {
                let original = original.clone();
                self.entries.insert(
                    path_buf,
                    VirtualFsEntry::Modified {
                        path: path.to_path_buf(),
                        original,
                        current: content,
                    },
                );
            }
            Some(VirtualFsEntry::Deleted { original, .. }) => {
                let original = original.clone();
                self.entries.insert(
                    path_buf,
                    VirtualFsEntry::Modified {
                        path: path.to_path_buf(),
                        original,
                        current: content,
                    },
                );
            }
            Some(VirtualFsEntry::Created { .. }) => {
                self.entries.insert(
                    path_buf,
                    VirtualFsEntry::Created { path: path.to_path_buf(), current: content },
                );
            }
            None => {
                self.entries.insert(
                    path_buf,
                    VirtualFsEntry::Created { path: path.to_path_buf(), current: content },
                );
            }
        }

        Ok(())
    }

    /// Appends content to a file at the given path.
    ///
    /// Returns an error if the file does not exist or has been deleted.
    pub fn append(&mut self, path: &Utf8Path, content: &str) -> Result<(), ToolError> {
        let existing = self.read(path)?;
        // Normalize appended content as well. `write` normalizes too
        // (idempotent, zero-cost for clean input), but normalizing here keeps
        // `append` safe even if its implementation ever stops routing through
        // `write`.
        let content = normalize_typographic(content);
        self.write(path, format!("{existing}{content}"))
    }

    /// Deletes a file at the given path.
    ///
    /// Returns an error if the file does not exist.
    /// Checks if a path exists in the virtual filesystem (staged state).
    pub fn exists(&self, path: &Utf8Path) -> bool {
        match self.entries.get(path) {
            Some(VirtualFsEntry::Deleted { .. }) => false,
            Some(_) => true,
            None => false,
        }
    }

    /// Checks if a path exists on the real filesystem.
    pub fn exists_on_disk(&self, path: &Utf8Path) -> bool {
        path.as_std_path().exists()
    }

    /// Deletes a file at the given path.
    pub fn delete(&mut self, path: &Utf8Path) -> Result<(), ToolError> {
        // Check existence first to avoid borrow conflicts.
        if !self.entries.contains_key(path) {
            return Err(ToolError::ExecutionFailed { message: format!("file not found: {path}") });
        }

        let path_buf = path.to_path_buf();

        // Read the entry's original content (if any) then remove it.
        let original = match self.entries.get(path) {
            Some(VirtualFsEntry::Original { content, .. }) => Some(content.clone()),
            Some(VirtualFsEntry::Modified { original, .. }) => Some(original.clone()),
            Some(VirtualFsEntry::Created { .. }) => None,
            Some(VirtualFsEntry::Deleted { .. }) => {
                return Err(ToolError::ExecutionFailed {
                    message: format!("file already deleted: {path}"),
                });
            }
            None => {
                return Err(ToolError::ExecutionFailed {
                    message: format!("file not found: {path}"),
                });
            }
        };

        match original {
            Some(original) => {
                self.entries.insert(
                    path_buf,
                    VirtualFsEntry::Deleted { path: path.to_path_buf(), original },
                );
            }
            None => {
                self.entries.remove(&path_buf);
            }
        }

        Ok(())
    }

    /// Stages deletion of a file. If the file is not already in the VFS, it will be loaded from disk and then deleted.
    pub fn stage_delete(&mut self, path: &Utf8Path) -> Result<(), ToolError> {
        // A reserved device name is never a deletable file: on Windows the
        // bare name addresses the device (and a `\\?\` extended path can
        // address a literal `nul` file), so reject it explicitly.
        reject_reserved_device_name(path)?;
        if self.entries.contains_key(path) {
            return self.delete(path);
        }
        if path.as_std_path().exists() {
            // Load from disk as Original then delete. Binary content is
            // tolerated (placeholder text) so deleting binary files works.
            let content = read_disk_text(path.as_std_path())?;
            self.entries.insert(
                path.to_path_buf(),
                VirtualFsEntry::Original { path: path.to_path_buf(), content: content.clone() },
            );
            return self.delete(path);
        }
        Err(ToolError::ExecutionFailed { message: format!("file not found: {path}") })
    }

    /// Lists the entries in a directory.
    ///
    /// Returns paths that are direct children of the given directory path.
    pub fn list_dir(&self, dir: &Utf8Path) -> Result<Vec<Utf8PathBuf>, ToolError> {
        let mut results = Vec::new();
        let dir_with_sep = format!("{dir}/");

        for path in self.entries.keys() {
            let path_str = path.as_str();
            if path_str.starts_with(&dir_with_sep) {
                let relative = &path_str[dir_with_sep.len()..];
                if !relative.contains('/') {
                    results.push(path.clone());
                }
            }
        }

        results.sort();
        Ok(results)
    }

    /// Creates a directory entry in the virtual filesystem.
    ///
    /// Directories are tracked implicitly by the paths of their children.
    /// This method ensures the directory path is valid but does not create
    /// a separate entry.
    pub fn create_dir(&mut self, path: &Utf8Path) -> Result<(), ToolError> {
        // Directories are implicit in the virtual filesystem.
        // We validate the path is not already a file.
        if let Some(entry) = self.entries.get(path) {
            return match entry {
                VirtualFsEntry::Original { .. }
                | VirtualFsEntry::Modified { .. }
                | VirtualFsEntry::Created { .. } => Err(ToolError::ExecutionFailed {
                    message: format!("path is a file, not a directory: {path}"),
                }),
                VirtualFsEntry::Deleted { .. } => {
                    // A deleted file at this path is fine; the directory can be created.
                    Ok(())
                }
            };
        }
        Ok(())
    }

    /// Reads a file from the real filesystem and registers it as an `Original`
    /// entry. Returns the file content.
    ///
    /// Staged changes are never clobbered: staged state wins over disk state.
    /// If the path already holds a staged `Modified`, `Created`, or `Deleted`
    /// entry, that entry is preserved and only the disk content is returned.
    /// Paths with no staged entry — or a clean `Original` entry — are
    /// (re)registered from the current disk state.
    ///
    /// Non-UTF-8 (binary) files do not fail the read: they are returned (and
    /// registered) as an informative `[binary file: N bytes …]` placeholder.
    pub fn read_disk(&mut self, path: &Utf8Path) -> Result<String, ToolError> {
        // Reading a reserved device name would address the Windows device
        // (or, behind `\\?\`, a literal `nul` file) rather than a real file;
        // reject it with the same message as the write/delete paths.
        reject_reserved_device_name(path)?;
        let content = read_disk_text(path.as_std_path())?;
        match self.entries.get(path) {
            Some(VirtualFsEntry::Original { .. }) | None => {
                self.entries.insert(
                    path.to_path_buf(),
                    VirtualFsEntry::Original { path: path.to_path_buf(), content: content.clone() },
                );
            }
            Some(VirtualFsEntry::Modified { .. })
            | Some(VirtualFsEntry::Created { .. })
            | Some(VirtualFsEntry::Deleted { .. }) => {
                // Staged change present: keep it; do not overwrite it.
            }
        }
        Ok(content)
    }

    /// Moves a file from one path to another.
    ///
    /// Returns an error if the source does not exist or the destination
    /// already exists.
    pub fn move_file(&mut self, from: &Utf8Path, to: &Utf8Path) -> Result<(), ToolError> {
        let content = self.read(from)?;

        if self.entries.contains_key(to) || self.exists_on_disk(to) {
            return Err(ToolError::ExecutionFailed {
                message: format!("destination already exists: {to}"),
            });
        }

        self.delete(from)?;
        self.write(to, content)?;

        Ok(())
    }

    /// Copies a file from one path to another.
    ///
    /// Returns an error if the source does not exist or the destination
    /// already exists.
    pub fn copy_file(&mut self, from: &Utf8Path, to: &Utf8Path) -> Result<(), ToolError> {
        let content = self.read(from)?;

        if self.entries.contains_key(to) || self.exists_on_disk(to) {
            return Err(ToolError::ExecutionFailed {
                message: format!("destination already exists: {to}"),
            });
        }

        self.write(to, content)?;
        Ok(())
    }

    /// Commits all virtual entries to the real filesystem.
    ///
    /// Writes `Modified` and `Created` entries, removes `Deleted` entries.
    /// Returns a `CommitReport` with counts of created, modified, and deleted
    /// files.
    ///
    /// The commit is all-or-nothing with respect to validation: the entire
    /// staged set is validated first (path legality, staged file/directory
    /// conflicts, parent-directory availability) and if any entry fails
    /// validation, nothing is applied and a [`CommitError`] listing every
    /// offending path is returned.
    ///
    /// Entries are applied in ascending byte order of their paths, so the
    /// result never depends on `HashMap` iteration order. Every write
    /// materializes its parent directory (`create_dir_all`), and byte order
    /// guarantees a parent path is applied before any of its children, so
    /// parent directories always exist before child files are written.
    ///
    /// If the apply phase still fails part-way (e.g. a concurrent I/O error),
    /// the returned [`CommitError`] lists exactly which paths were applied and
    /// which failed, in the deterministic apply order, so callers can skip the
    /// applied paths and re-attempt the failed ones.
    pub fn commit_to_disk(&self) -> Result<CommitReport, CommitError> {
        let rejected = preflight_paths(
            &self.entries,
            &self.entries.keys().cloned().collect::<Vec<Utf8PathBuf>>(),
        );
        if !rejected.is_empty() {
            return Err(CommitError { applied: Vec::new(), rejected, failed: Vec::new() });
        }

        // Deterministic apply order: ascending byte order of paths. Byte order
        // also guarantees parents are applied before their children.
        let mut sorted: Vec<&VirtualFsEntry> = self.entries.values().collect();
        sorted.sort_by(|a, b| a.path().cmp(b.path()));

        let mut created = 0usize;
        let mut modified = 0usize;
        let mut deleted = 0usize;
        let mut applied: Vec<Utf8PathBuf> = Vec::new();
        let mut failed: Vec<CommitFailure> = Vec::new();

        for entry in sorted {
            match entry {
                VirtualFsEntry::Original { .. } => {}
                VirtualFsEntry::Modified { path, current, .. } => {
                    match std::fs::write(path.as_std_path(), current) {
                        Ok(()) => {
                            applied.push(path.clone());
                            modified += 1;
                        }
                        Err(error) => failed
                            .push(CommitFailure { path: path.clone(), reason: error.to_string() }),
                    }
                }
                VirtualFsEntry::Deleted { path, .. } => {
                    match std::fs::remove_file(path.as_std_path()) {
                        Ok(()) => {
                            applied.push(path.clone());
                            deleted += 1;
                        }
                        Err(error) => failed
                            .push(CommitFailure { path: path.clone(), reason: error.to_string() }),
                    }
                }
                VirtualFsEntry::Created { path, current, .. } => {
                    let parent_result = match path.parent() {
                        Some(parent) if !parent.as_str().is_empty() => {
                            std::fs::create_dir_all(parent.as_std_path())
                        }
                        _ => Ok(()),
                    };
                    if let Err(error) = parent_result {
                        failed
                            .push(CommitFailure { path: path.clone(), reason: error.to_string() });
                        continue;
                    }
                    match std::fs::write(path.as_std_path(), current) {
                        Ok(()) => {
                            applied.push(path.clone());
                            created += 1;
                        }
                        Err(error) => failed
                            .push(CommitFailure { path: path.clone(), reason: error.to_string() }),
                    }
                }
            }
        }

        if failed.is_empty() {
            Ok(CommitReport { created, modified, deleted })
        } else {
            Err(CommitError { applied, rejected: Vec::new(), failed })
        }
    }

    /// Reverts a specific diff hunk for a file.
    ///
    /// `hunk_index` is the zero-based index of the hunk to reject.
    /// Returns an error if the path does not exist or the hunk index is invalid.
    pub fn reject_hunk(&mut self, path: &Utf8Path, hunk_index: usize) -> Result<(), ToolError> {
        self.reject_hunks(path, &[hunk_index])
    }

    /// Re-evaluate all rejected change hunks from the entry's original and
    /// current content in one pass. This avoids hunk indexes shifting after an
    /// earlier rejection.
    pub fn reject_hunks(
        &mut self,
        path: &Utf8Path,
        hunk_indices: &[usize],
    ) -> Result<(), ToolError> {
        let entry = self.entries.get(path).ok_or_else(|| ToolError::ExecutionFailed {
            message: format!("file not found: {path}"),
        })?;

        let original = match entry {
            VirtualFsEntry::Original { content, .. } => content.clone(),
            VirtualFsEntry::Modified { original, .. } => original.clone(),
            VirtualFsEntry::Deleted { original, .. } => original.clone(),
            VirtualFsEntry::Created { .. } => String::new(),
        };

        let current = match entry {
            VirtualFsEntry::Original { content, .. } => content.clone(),
            VirtualFsEntry::Modified { current, .. } => current.clone(),
            VirtualFsEntry::Deleted { .. } => String::new(),
            VirtualFsEntry::Created { current, .. } => current.clone(),
        };
        let rejected = hunk_indices.iter().copied().collect::<HashSet<_>>();
        let new_content = crate::diff::reject_change_hunks(&original, &current, &rejected)?;

        // Update the entry.
        if matches!(entry, VirtualFsEntry::Created { .. }) {
            if new_content.is_empty() {
                self.entries.remove(path);
            } else {
                self.entries.insert(
                    path.to_path_buf(),
                    VirtualFsEntry::Created { path: path.to_path_buf(), current: new_content },
                );
            }
        } else if new_content == original {
            self.entries.insert(
                path.to_path_buf(),
                VirtualFsEntry::Original { path: path.to_path_buf(), content: original },
            );
        } else {
            self.entries.insert(
                path.to_path_buf(),
                VirtualFsEntry::Modified {
                    path: path.to_path_buf(),
                    original,
                    current: new_content,
                },
            );
        }

        Ok(())
    }

    /// Materialize only the reviewed paths. A missing VFS entry means a
    /// rejected created file and is removed from disk.
    ///
    /// Paths are applied in ascending byte order so the outcome does not
    /// depend on `HashMap` iteration order. The whole requested set is
    /// validated before anything is written: if any path is illegal or
    /// conflicts with a staged ancestor, nothing is applied and an error
    /// listing the offending paths is returned.
    pub fn materialize_paths(&self, paths: &[Utf8PathBuf]) -> Result<(), ToolError> {
        let mut sorted = paths.to_vec();
        sorted.sort();

        let rejected = preflight_paths(&self.entries, &sorted);
        if !rejected.is_empty() {
            let mut message = format!("materialize rejected {} path(s):", rejected.len());
            for failure in &rejected {
                use std::fmt::Write;
                write!(&mut message, "\n  {}: {}", failure.path, failure.reason).ok();
            }
            return Err(ToolError::ExecutionFailed { message });
        }

        for path in &sorted {
            match self.entries.get(path) {
                Some(VirtualFsEntry::Original { content, .. }) => {
                    std::fs::write(path.as_std_path(), content).map_err(ToolError::Io)?;
                }
                Some(VirtualFsEntry::Modified { current, .. })
                | Some(VirtualFsEntry::Created { current, .. }) => {
                    if let Some(parent) = path.parent() {
                        if !parent.as_str().is_empty() {
                            std::fs::create_dir_all(parent.as_std_path()).map_err(ToolError::Io)?;
                        }
                    }
                    std::fs::write(path.as_std_path(), current).map_err(ToolError::Io)?;
                }
                Some(VirtualFsEntry::Deleted { .. }) | None => {
                    match std::fs::remove_file(path.as_std_path()) {
                        Ok(()) => {}
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                        Err(error) => return Err(ToolError::Io(error)),
                    }
                }
            }
        }
        Ok(())
    }

    /// Creates a cheap snapshot of the current virtual filesystem state.
    pub fn snapshot(&self) -> VirtualFsSnapshot {
        VirtualFsSnapshot { entries: self.entries.clone() }
    }

    /// Restores the virtual filesystem from a snapshot.
    pub fn restore(&mut self, snapshot: VirtualFsSnapshot) {
        self.entries = snapshot.entries;
    }

    /// Returns the number of entries in the virtual filesystem.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns true if the virtual filesystem contains no entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Returns a reference to an entry by path.
    pub fn get(&self, path: &Utf8Path) -> Option<&VirtualFsEntry> {
        self.entries.get(path)
    }

    /// Inserts an entry into the virtual filesystem.
    pub fn insert(&mut self, entry: VirtualFsEntry) {
        let path = entry.path().to_path_buf();
        self.entries.insert(path, entry);
    }

    /// Removes any staged entry for `path`, making the real filesystem the
    /// authority for it again.
    ///
    /// Unlike `delete` (which stages a `Deleted` entry), this drops the entry
    /// entirely. Use it when an external writer — e.g. the desktop editor's
    /// Save/Delete — has already changed the on-disk file: keeping a stale
    /// staged entry would shadow the newer disk state on the next read.
    ///
    /// No-op when nothing is staged for `path`.
    pub fn unstage(&mut self, path: &Utf8Path) {
        self.entries.remove(path);
    }
}

/// Report of a commit operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitReport {
    pub created: usize,
    pub modified: usize,
    pub deleted: usize,
}

/// A single path that could not be committed to disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitFailure {
    /// The path that could not be committed.
    pub path: Utf8PathBuf,
    /// Human-readable reason for the failure.
    pub reason: String,
}

/// Error returned by [`VirtualFs::commit_to_disk`] when the staged set cannot
/// be applied in full.
///
/// Validation is all-or-nothing: when `rejected` is non-empty, nothing was
/// written to disk and callers may fix the offending entries and retry. When
/// the apply phase fails part-way (`rejected` is empty and `failed` is not),
/// `applied` lists exactly which entries were already written, in the
/// deterministic apply order, so a caller can recover by skipping `applied`
/// paths and re-attempting `failed` paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitError {
    /// Paths successfully applied before the failure (deterministic order).
    pub applied: Vec<Utf8PathBuf>,
    /// Paths rejected by preflight validation; nothing was applied.
    pub rejected: Vec<CommitFailure>,
    /// Paths that failed during the apply phase.
    pub failed: Vec<CommitFailure>,
}

impl std::fmt::Display for CommitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "virtual fs commit failed")?;
        if !self.rejected.is_empty() {
            write!(f, "; preflight rejected {} path(s):", self.rejected.len())?;
            for failure in &self.rejected {
                write!(f, "\n  {}: {}", failure.path, failure.reason)?;
            }
        }
        if !self.applied.is_empty() {
            write!(f, "; applied {} path(s) before failure:", self.applied.len())?;
            for path in &self.applied {
                write!(f, "\n  {path}")?;
            }
        }
        if !self.failed.is_empty() {
            write!(f, "; apply failed for {} path(s):", self.failed.len())?;
            for failure in &self.failed {
                write!(f, "\n  {}: {}", failure.path, failure.reason)?;
            }
        }
        Ok(())
    }
}

impl std::error::Error for CommitError {}

/// Returns a reason when `path` is not safe to commit: it must be absolute
/// with no `.`/`..` components.
///
/// Callers anchor paths to the workspace root with `resolve_path`, which
/// produces canonical absolute paths; this is a second line of defense so a
/// commit can never write relative to the process working directory or escape
/// via parent traversal.
fn path_legality_error(path: &Utf8Path) -> Option<String> {
    if path.as_str().is_empty() {
        return Some("path is empty".into());
    }
    if !path.is_absolute() {
        return Some(
            "path is not absolute; commit requires a path resolved against the workspace root"
                .into(),
        );
    }
    if path
        .components()
        .any(|c| matches!(c, camino::Utf8Component::ParentDir | camino::Utf8Component::CurDir))
    {
        return Some("path contains '.' or '..' components".into());
    }
    None
}

/// Returns the nearest staged ancestor of `path`.
///
/// A staged entry at an ancestor is always a conflict: file entries
/// (`Original`/`Modified`/`Created`) occupy the ancestor path as a file, and a
/// staged deletion removes a file there, so a child cannot be created under
/// it either way.
fn staged_ancestor(
    entries: &HashMap<Utf8PathBuf, VirtualFsEntry>,
    path: &Utf8Path,
) -> Option<Utf8PathBuf> {
    let mut parent = path.parent();
    while let Some(p) = parent {
        if p.as_str().is_empty() {
            break;
        }
        if entries.contains_key(p) {
            return Some(p.to_path_buf());
        }
        parent = p.parent();
    }
    None
}

/// Checks the disk state observable before applying `path`. Returns a reason
/// when an ancestor exists as a file (blocking directory creation) or when the
/// path itself exists as a directory (blocking a file write or deletion).
///
/// This check is best-effort: disk state can change between validation and
/// application, so apply-phase failures are still reported via
/// [`CommitError::failed`].
fn disk_state_failure(path: &Utf8Path, is_deletion: bool) -> Option<String> {
    let mut parent = path.parent();
    while let Some(p) = parent {
        if p.as_str().is_empty() {
            break;
        }
        if p.as_std_path().is_file() {
            return Some(format!("parent directory '{p}' exists as a file"));
        }
        parent = p.parent();
    }
    let on_disk = path.as_std_path();
    if on_disk.is_dir() {
        return Some(if is_deletion {
            "path exists as a directory; only files can be deleted".into()
        } else {
            "path exists as a directory; a file is expected".into()
        });
    }
    None
}

/// Validates a set of paths (the full staged set for a commit, or a
/// caller-selected subset for `materialize_paths`) before anything is applied.
/// Returns the list of offending paths; an empty list means the operation is
/// safe to apply.
fn preflight_paths(
    entries: &HashMap<Utf8PathBuf, VirtualFsEntry>,
    paths: &[Utf8PathBuf],
) -> Vec<CommitFailure> {
    let mut failures = Vec::new();
    for path in paths {
        if let Some(reason) = path_legality_error(path) {
            failures.push(CommitFailure { path: path.clone(), reason });
            continue;
        }
        // A reserved Windows device name must never be materialized as a real
        // file: on Windows the `\\?\` extended-path prefix bypasses the
        // reserved-name check, so writing `nul` would create a literal 0-byte
        // file instead of an error.
        if let Some(device) = reserved_device_name(path) {
            failures
                .push(CommitFailure { path: path.clone(), reason: reserved_device_reason(device) });
            continue;
        }
        if let Some(ancestor) = staged_ancestor(entries, path) {
            failures.push(CommitFailure {
                path: path.clone(),
                reason: format!(
                    "conflicts with staged path '{ancestor}': a staged path cannot contain children"
                ),
            });
            continue;
        }
        let is_deletion = matches!(entries.get(path), Some(VirtualFsEntry::Deleted { .. }) | None);
        if let Some(reason) = disk_state_failure(path, is_deletion) {
            failures.push(CommitFailure { path: path.clone(), reason });
        }
    }
    failures
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn virtual_fs_read_write() {
        let mut fs = VirtualFs::new();
        fs.write(Utf8Path::new("test.txt"), "hello".to_string()).unwrap();
        assert_eq!(fs.read(Utf8Path::new("test.txt")).unwrap(), "hello");
    }

    #[test]
    fn virtual_fs_append() {
        let mut fs = VirtualFs::new();
        fs.write(Utf8Path::new("test.txt"), "hello".to_string()).unwrap();
        fs.append(Utf8Path::new("test.txt"), " world").unwrap();
        assert_eq!(fs.read(Utf8Path::new("test.txt")).unwrap(), "hello world");
    }

    #[test]
    fn write_normalizes_typographic_content() {
        let mut fs = VirtualFs::new();
        fs.write(
            Utf8Path::new("test.txt"),
            "let s = \u{201C}smart\u{201D} \u{2011}nb \u{2013}en \u{2192} arrow \u{2018}q\u{2019};"
                .to_string(),
        )
        .unwrap();
        assert_eq!(
            fs.read(Utf8Path::new("test.txt")).unwrap(),
            "let s = \"smart\" -nb -en -> arrow 'q';"
        );
    }

    #[test]
    fn write_preserves_em_dash_in_vfs() {
        let mut fs = VirtualFs::new();
        fs.write(Utf8Path::new("test.txt"), "prose \u{2014} keep".to_string()).unwrap();
        assert_eq!(fs.read(Utf8Path::new("test.txt")).unwrap(), "prose \u{2014} keep");
    }

    #[test]
    fn append_normalizes_typographic_content() {
        let mut fs = VirtualFs::new();
        fs.write(Utf8Path::new("test.txt"), "let a = 1;".to_string()).unwrap();
        fs.append(Utf8Path::new("test.txt"), " // \u{201C}done\u{201D} \u{2192} \u{2013}").unwrap();
        assert_eq!(fs.read(Utf8Path::new("test.txt")).unwrap(), "let a = 1; // \"done\" -> -");
    }

    #[test]
    fn virtual_fs_delete() {
        let mut fs = VirtualFs::new();
        fs.write(Utf8Path::new("test.txt"), "hello".to_string()).unwrap();
        fs.delete(Utf8Path::new("test.txt")).unwrap();
        assert!(fs.read(Utf8Path::new("test.txt")).is_err());
    }

    #[test]
    fn virtual_fs_unstage_removes_staged_entry() {
        let mut fs = VirtualFs::new();
        fs.write(Utf8Path::new("test.txt"), "staged".to_string()).unwrap();
        assert!(fs.get(Utf8Path::new("test.txt")).is_some());
        fs.unstage(Utf8Path::new("test.txt"));
        assert!(fs.get(Utf8Path::new("test.txt")).is_none());
        assert!(fs.read(Utf8Path::new("test.txt")).is_err());
        assert!(fs.changed_paths().is_empty());
        assert!(fs.is_empty());
    }

    #[test]
    fn virtual_fs_unstage_is_noop_for_unknown_path() {
        let mut fs = VirtualFs::new();
        fs.write(Utf8Path::new("a.txt"), "x".to_string()).unwrap();
        fs.unstage(Utf8Path::new("nope.txt"));
        // Unrelated entries are untouched.
        assert_eq!(fs.read(Utf8Path::new("a.txt")).unwrap(), "x");
        assert_eq!(fs.len(), 1);
    }

    #[test]
    fn virtual_fs_unstage_differs_from_delete_staging() {
        let mut fs = VirtualFs::new();
        fs.write(Utf8Path::new("test.txt"), "hello".to_string()).unwrap();
        fs.unstage(Utf8Path::new("test.txt"));
        // No Deleted entry remains: a later commit must not apply a phantom
        // deletion just because the editor dropped a stale staged entry.
        assert!(fs.changed_paths().is_empty());
    }

    #[test]
    fn virtual_fs_move_file() {
        let mut fs = VirtualFs::new();
        fs.write(Utf8Path::new("a.txt"), "hello пере".to_string()).unwrap();
        fs.move_file(Utf8Path::new("a.txt"), Utf8Path::new("b.txt")).unwrap();
        assert!(fs.read(Utf8Path::new("a.txt")).is_err());
        assert_eq!(fs.read(Utf8Path::new("b.txt")).unwrap(), "hello пере");
    }

    #[test]
    fn virtual_fs_copy_file() {
        let mut fs = VirtualFs::new();
        fs.write(Utf8Path::new("a.txt"), "hello".to_string()).unwrap();
        fs.copy_file(Utf8Path::new("a.txt"), Utf8Path::new("b.txt")).unwrap();
        assert_eq!(fs.read(Utf8Path::new("a.txt")).unwrap(), "hello");
        assert_eq!(fs.read(Utf8Path::new("b.txt")).unwrap(), "hello");
    }

    #[test]
    fn virtual_fs_snapshot_restore() {
        let mut fs = VirtualFs::new();
        fs.write(Utf8Path::new("test.txt"), "hello".to_string()).unwrap();
        let snapshot = fs.snapshot();
        fs.write(Utf8Path::new("test.txt"), "world".to_string()).unwrap();
        fs.restore(snapshot);
        assert_eq!(fs.read(Utf8Path::new("test.txt")).unwrap(), "hello");
    }

    #[test]
    fn virtual_fs_commit_to_disk() {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let file_path = root.join("test.txt");

        let mut fs = VirtualFs::new();
        fs.write(&file_path, "hello".to_string()).unwrap();
        let report = fs.commit_to_disk().unwrap();
        assert_eq!(report.created, 1);
        assert_eq!(report.modified, 0);
        assert_eq!(report.deleted, 0);

        let content = std::fs::read_to_string(file_path.as_std_path()).unwrap();
        assert_eq!(content, "hello");
    }

    #[test]
    fn virtual_fs_reject_hunk() {
        let mut fs = VirtualFs::new();
        let original = "line1\nline2\nline3\n";
        let current = "line1\nmodified\nline3\n";
        fs.write(Utf8Path::new("test.txt"), original.to_string()).unwrap();
        // Simulate modification by replacing the entry
        fs.insert(VirtualFsEntry::Modified {
            path: Utf8PathBuf::from("test.txt"),
            original: original.to_string(),
            current: current.to_string(),
        });

        fs.reject_hunk(Utf8Path::new("test.txt"), 0).unwrap();
        assert_eq!(fs.read(Utf8Path::new("test.txt")).unwrap(), original);
    }

    #[test]
    fn rejected_hunks_are_materialized_to_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(dir.path().join("review.txt")).unwrap();
        let original = "zero\none\ntwo\nthree\n";
        let changed = "zero\nONE\ntwo\nTHREE\n";
        std::fs::write(path.as_std_path(), changed).unwrap();
        let mut fs = VirtualFs::new();
        fs.insert(VirtualFsEntry::Modified {
            path: path.clone(),
            original: original.into(),
            current: changed.into(),
        });
        fs.reject_hunks(&path, &[0]).unwrap();
        fs.materialize_paths(std::slice::from_ref(&path)).unwrap();
        assert_eq!(std::fs::read_to_string(path.as_std_path()).unwrap(), "zero\none\ntwo\nTHREE\n");
    }

    #[test]
    fn virtual_fs_list_dir() {
        let mut fs = VirtualFs::new();
        fs.write(Utf8Path::new("dir/a.txt"), "a".to_string()).unwrap();
        fs.write(Utf8Path::new("dir/b.txt"), "b".to_string()).unwrap();
        fs.write(Utf8Path::new("dir/sub/c.txt"), "c".to_string()).unwrap();

        let entries = fs.list_dir(Utf8Path::new("dir")).unwrap();
        assert_eq!(entries.len(), 2);
        assert!(entries.contains(&Utf8PathBuf::from("dir/a.txt")));
        assert!(entries.contains(&Utf8PathBuf::from("dir/b.txt")));
    }

    #[test]
    fn virtual_fs_entry_variants() {
        let entry = VirtualFsEntry::Original {
            path: Utf8PathBuf::from("test.txt"),
            content: "hello".to_string(),
        };
        assert_eq!(entry.path(), Utf8Path::new("test.txt"));
        assert_eq!(entry.current_content(), Some("hello"));
        assert_eq!(entry.original_content(), Some("hello"));

        let entry = VirtualFsEntry::Modified {
            path: Utf8PathBuf::from("test.txt"),
            original: "hello".to_string(),
            current: "world".to_string(),
        };
        assert_eq!(entry.current_content(), Some("world"));
        assert_eq!(entry.original_content(), Some("hello"));

        let entry = VirtualFsEntry::Deleted {
            path: Utf8PathBuf::from("test.txt"),
            original: "hello".to_string(),
        };
        assert_eq!(entry.current_content(), None);
        assert_eq!(entry.original_content(), Some("hello"));

        let entry = VirtualFsEntry::Created {
            path: Utf8PathBuf::from("test.txt"),
            current: "hello".to_string(),
        };
        assert_eq!(entry.current_content(), Some("hello"));
        assert_eq!(entry.original_content(), None);
    }

    /// Verify that a path completely outside the project root does not
    /// start with the root prefix.
    #[test]
    fn path_outside_root_does_not_start_with_root() {
        let root = Utf8PathBuf::from("/home/user/project");
        let outside = Utf8PathBuf::from("/etc/passwd");
        assert!(
            !outside.starts_with(&root),
            "absolute path outside root should not start with root"
        );
    }

    /// Verify that an absolute path replaces the root when joined.
    #[test]
    fn absolute_path_replaces_root() {
        let root = Utf8PathBuf::from("/home/user/project");
        let absolute = Utf8PathBuf::from("/etc/passwd");
        let normalized = root.join(&absolute);
        // On Linux, Path::join discards root when the second component is absolute.
        assert_eq!(
            normalized, absolute,
            "absolute path should replace the root rather than stay within it"
        );
    }

    /// Recursively snapshot a directory as a sorted (relative path, content)
    /// list so two trees can be compared for equality.
    fn snapshot_tree(root: &Utf8Path) -> Vec<(String, String)> {
        let mut files = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let read_dir = std::fs::read_dir(dir.as_std_path()).unwrap();
            for entry in read_dir {
                let entry = entry.unwrap();
                let path = Utf8PathBuf::from_path_buf(entry.path()).unwrap();
                if path.is_dir() {
                    stack.push(path);
                } else {
                    let content = std::fs::read_to_string(path.as_std_path()).unwrap();
                    let relative = path.strip_prefix(root).unwrap().to_string();
                    files.push((relative, content));
                }
            }
        }
        files.sort();
        files
    }

    #[test]
    fn commit_is_deterministic_across_runs() {
        // The same staged set committed into two fresh roots must produce
        // identical disk state, independent of HashMap iteration order.
        let stage = |root: &Utf8Path| {
            let mut fs = VirtualFs::new();
            fs.write(&root.join("z.txt"), "z".to_string()).unwrap();
            fs.write(&root.join("dir/a.txt"), "a".to_string()).unwrap();
            fs.write(&root.join("dir/sub/b.txt"), "b".to_string()).unwrap();
            fs.write(&root.join("m.txt"), "changed".to_string()).unwrap();
            fs.commit_to_disk().unwrap();
        };
        let dir_a = tempfile::tempdir().unwrap();
        let dir_b = tempfile::tempdir().unwrap();
        let root_a = Utf8PathBuf::from_path_buf(dir_a.path().to_path_buf()).unwrap();
        let root_b = Utf8PathBuf::from_path_buf(dir_b.path().to_path_buf()).unwrap();
        stage(&root_a);
        stage(&root_b);
        assert_eq!(snapshot_tree(&root_a), snapshot_tree(&root_b));
    }

    #[test]
    fn commit_partial_failure_reports_applied_and_failed_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();

        let fail_path = root.join("b.txt");
        std::fs::write(fail_path.as_std_path(), "original").unwrap();

        let mut fs = VirtualFs::new();
        fs.stage_delete(&fail_path).unwrap(); // staged Deleted
                                              // Remove the file behind the VFS's back so the apply phase fails.
        std::fs::remove_file(fail_path.as_std_path()).unwrap();

        fs.write(&root.join("a.txt"), "a".to_string()).unwrap();
        fs.write(&root.join("c.txt"), "c".to_string()).unwrap();

        let err = fs.commit_to_disk().unwrap_err();
        // Applied in ascending byte order; the failing deletion is reported.
        assert_eq!(err.applied, vec![root.join("a.txt"), root.join("c.txt")]);
        assert!(err.rejected.is_empty());
        assert_eq!(err.failed.len(), 1);
        assert_eq!(err.failed[0].path, fail_path);
        assert!(err.to_string().contains("b.txt"));
    }

    #[test]
    fn commit_preflight_conflict_applies_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();

        let mut fs = VirtualFs::new();
        // A staged file at `file.txt` conflicts with a staged child under it.
        fs.write(&root.join("file.txt"), "file".to_string()).unwrap();
        fs.write(&root.join("file.txt/child.txt"), "child".to_string()).unwrap();

        let err = fs.commit_to_disk().unwrap_err();
        assert!(err.applied.is_empty());
        assert!(err.failed.is_empty());
        assert_eq!(err.rejected.len(), 1);
        assert!(err.rejected[0].path.ends_with("file.txt/child.txt"));
        assert!(err.to_string().contains("file.txt"));

        // Nothing was applied to disk.
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn commit_preflight_rejects_relative_and_traversal_paths() {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();

        let mut fs = VirtualFs::new();
        fs.write(Utf8Path::new("relative.txt"), "x".to_string()).unwrap();
        fs.write(&root.join("ok.txt"), "y".to_string()).unwrap();

        let err = fs.commit_to_disk().unwrap_err();
        assert!(err.applied.is_empty());
        assert_eq!(err.rejected.len(), 1);
        assert_eq!(err.rejected[0].path, Utf8PathBuf::from("relative.txt"));
        assert!(err.to_string().contains("not absolute"));
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);

        // Parent traversal is rejected too, before anything touches disk.
        let mut fs2 = VirtualFs::new();
        fs2.write(Utf8Path::new("/tmp/opencode/../escape.txt"), "z".to_string()).unwrap();
        let err2 = fs2.commit_to_disk().unwrap_err();
        assert!(!err2.rejected.is_empty());
        assert!(err2.rejected[0].reason.contains("'..'"));
    }

    #[test]
    fn read_disk_preserves_staged_changes() {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();

        // A staged Modified entry survives read_disk.
        let path = root.join("m.txt");
        std::fs::write(path.as_std_path(), "original").unwrap();
        let mut fs = VirtualFs::new();
        fs.read_disk(&path).unwrap(); // Original
        fs.write(&path, "staged".to_string()).unwrap(); // Modified
        fs.read_disk(&path).unwrap(); // must NOT clobber
        assert_eq!(fs.read(&path).unwrap(), "staged");
        assert!(matches!(
            fs.get(&path),
            Some(VirtualFsEntry::Modified { current, .. }) if current == "staged"
        ));

        // A staged Deleted entry survives read_disk (is not resurrected).
        let del_path = root.join("d.txt");
        std::fs::write(del_path.as_std_path(), "gone").unwrap();
        let mut fs2 = VirtualFs::new();
        fs2.stage_delete(&del_path).unwrap(); // Deleted
        fs2.read_disk(&del_path).unwrap();
        assert!(!fs2.exists(&del_path));
        assert!(matches!(fs2.get(&del_path), Some(VirtualFsEntry::Deleted { .. })));

        // With no staged entry, read_disk registers a clean Original.
        let mut fs3 = VirtualFs::new();
        let content = fs3.read_disk(&path).unwrap();
        assert_eq!(content, "original");
        assert!(matches!(fs3.get(&path), Some(VirtualFsEntry::Original { .. })));
    }

    #[test]
    fn read_disk_returns_binary_placeholder_and_never_fails() {
        let dir = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(dir.path().join("binary.bin")).unwrap();
        std::fs::write(path.as_std_path(), b"\xFF\x00binary\xFE").unwrap();

        let mut fs = VirtualFs::new();
        let content = fs.read_disk(&path).unwrap();
        assert!(content.contains("binary file"), "expected placeholder, got: {content}");
        assert!(content.contains("bytes"), "placeholder should carry a byte count: {content}");
        // The file still registers as an Original entry without crashing.
        assert!(matches!(fs.get(&path), Some(VirtualFsEntry::Original { .. })));
    }

    #[test]
    fn read_disk_returns_valid_utf8_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(dir.path().join("text.txt")).unwrap();
        std::fs::write(path.as_std_path(), "héllo wörld\n").unwrap();

        let mut fs = VirtualFs::new();
        assert_eq!(fs.read_disk(&path).unwrap(), "héllo wörld\n");
    }

    #[test]
    fn stage_delete_does_not_fail_on_binary_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = Utf8PathBuf::from_path_buf(dir.path().join("binary.bin")).unwrap();
        std::fs::write(path.as_std_path(), b"\xFF\xFE\x00").unwrap();

        let mut fs = VirtualFs::new();
        fs.stage_delete(&path).unwrap();
        assert!(!fs.exists(&path));
        assert!(matches!(fs.get(&path), Some(VirtualFsEntry::Deleted { .. })));
    }

    #[test]
    fn materialize_paths_rejects_conflict_before_writing() {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        let dir_path = root.join("existing_dir");
        std::fs::create_dir_all(dir_path.as_std_path()).unwrap();

        let mut fs = VirtualFs::new();
        // A Created file staged where a directory already exists on disk.
        fs.write(&root.join("existing_dir"), "should not land".to_string()).unwrap();
        let good_path = root.join("good.txt");
        fs.write(&good_path, "good".to_string()).unwrap();

        let err =
            fs.materialize_paths(&[good_path.clone(), root.join("existing_dir")]).unwrap_err();
        assert!(err.to_string().contains("existing_dir"));
        assert!(!good_path.as_std_path().exists());
        assert_eq!(std::fs::read_dir(dir_path.as_std_path()).unwrap().count(), 0);
    }

    // -----------------------------------------------------------------------
    // Windows reserved device names (nul/con/prn/aux/com1..9/lpt1..9).
    // -----------------------------------------------------------------------

    #[test]
    fn reserved_device_names_rejected_by_read_and_delete_paths() {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        for name in ["nul", "NUL", "Con", "PRN", "aux", "com1", "com9", "lpt1", "lpt9"] {
            let path = root.join(name);
            let assert_reserved = |result: Result<(), ToolError>, op: &str| {
                let err = result.unwrap_err();
                let text = err.to_string();
                assert!(
                    text.contains("reserved Windows device name")
                        && text.to_ascii_lowercase().contains(&name.to_ascii_lowercase()),
                    "{op} '{name}' must produce the reserved-name error, got: {text}"
                );
                assert!(matches!(err, ToolError::ExecutionFailed { .. }));
            };

            let mut fs = VirtualFs::new();
            assert_reserved(fs.read(&path).map(|_| ()), "read");
            assert_reserved(fs.read_disk(&path).map(|_| ()), "read_disk");
            assert_reserved(fs.stage_delete(&path), "stage_delete");
            let detected = reserved_device_name(&path).expect("device name must be detected");
            assert_eq!(detected, name.to_ascii_lowercase(), "{name} must be detected");
        }
    }

    #[test]
    fn commit_and_materialize_reject_reserved_device_names() {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();

        // Staging itself is allowed (keeps move/copy semantics untouched), but
        // a staged device-name entry must be rejected by preflight before
        // anything materializes on disk.
        let mut fs = VirtualFs::new();
        fs.write(&root.join("nul"), "x".to_string()).unwrap();
        let err = fs.commit_to_disk().unwrap_err();
        assert!(err.applied.is_empty() && err.failed.is_empty());
        assert_eq!(err.rejected.len(), 1);
        assert!(err.rejected[0].reason.contains("reserved Windows device name 'nul'"));
        assert!(!root.join("nul").as_std_path().exists(), "device file must never materialize");

        // materialize_paths rejects a device-name path the same way.
        let err = fs.materialize_paths(&[root.join("NUL")]).unwrap_err();
        assert!(err.to_string().contains("reserved Windows device name"));
        assert!(!root.join("NUL").as_std_path().exists());
    }

    #[test]
    fn device_lookalike_names_stay_ordinary_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();

        // `nul.txt`, `CON.txt`, and `com10` are not device names (exact
        // basename match only): staging, reading, and committing them works
        // like any other file.
        let mut fs = VirtualFs::new();
        for name in ["nul.txt", "CON.txt", "com10"] {
            let path = root.join(name);
            assert_eq!(reserved_device_name(&path), None, "{name} must not be a device name");
            fs.write(&path, "content".to_string()).unwrap();
            assert_eq!(fs.read(&path).unwrap(), "content");
        }
        fs.commit_to_disk().unwrap();
        assert!(root.join("nul.txt").as_std_path().is_file());
        assert!(root.join("CON.txt").as_std_path().is_file());
        assert!(root.join("com10").as_std_path().is_file());
    }
}
