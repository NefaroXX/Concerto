//! Managed Bash runtime manager (ADR-28 Slice 2).
//!
//! This is the "Managed Bash PoC": a Concerto-owned Bash runtime that lives
//! entirely under the user's data directory
//! (`<data>/concerto/managed-bash/<version>/bash`) and never touches the
//! global `PATH`, the OS registry, user shell config, or project files. It is
//! versioned, offline, integrity-checked, and supports import/export of its
//! manifest.
//!
//! The PoC "install" adopts an existing local Bash (e.g. the system `bash`)
//! into the managed directory. The real distribution of a vetted Bash binary
//! (download/package) is the later, licensing-gated slice; this module only
//! needs a local source to exercise the full lifecycle.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use thiserror::Error;

/// Errors surfaced by the managed runtime manager.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ManagedRuntimeError {
    #[error("managed runtime data directory unavailable: {0}")]
    DataDirUnavailable(String),

    #[error("source executable not found: {0}")]
    SourceNotFound(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("integrity mismatch for {path}: expected {expected}, got {actual}")]
    IntegrityMismatch { path: String, expected: String, actual: String },
}

/// Integrity record for a single file (blake3 hash).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IntegrityInfo {
    pub algorithm: String,
    pub hash: String,
}

/// One tool shipped inside the managed runtime.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolEntry {
    pub name: String,
    pub path: PathBuf,
    pub version: Option<String>,
    pub integrity: Option<IntegrityInfo>,
}

/// The tool manifest: every tool the runtime ships.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct ToolManifest {
    pub tools: Vec<ToolEntry>,
}

/// The runtime manifest: version + the bash executable + integrity + tools.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RuntimeManifest {
    /// Human-readable runtime version (taken from `bash --version`).
    pub version: String,
    /// Absolute path to the managed `bash` executable.
    pub bash_executable: PathBuf,
    /// Raw `bash --version` first line.
    pub bash_version: String,
    /// Install time, RFC3339 UTC.
    pub installed_at: String,
    /// True once installed from a local source (no network required).
    pub offline: bool,
    /// Whether integrity verification is enforced.
    pub integrity_enabled: bool,
    /// Integrity of the runtime itself (the `bash` binary).
    pub runtime_integrity: IntegrityInfo,
    /// The tool manifest.
    pub tool_manifest: ToolManifest,
}

/// Per-tool integrity verification outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum IntegrityStatus {
    Ok,
    Mismatch { expected: String, actual: String },
    Missing,
    Unknown,
}

/// Per-tool integrity entry returned by [`ManagedRuntimeManager::verify`].
#[derive(Debug, Clone)]
pub struct IntegrityEntry {
    pub name: String,
    pub path: PathBuf,
    pub status: IntegrityStatus,
}

/// Aggregate integrity report.
#[derive(Debug, Clone)]
pub struct IntegrityReport {
    pub runtime_ok: bool,
    pub entries: Vec<IntegrityEntry>,
}

/// Manages the on-disk managed Bash runtime under a root directory.
pub struct ManagedRuntimeManager {
    root: PathBuf,
}

impl ManagedRuntimeManager {
    /// Root under the user's data dir: `<data>/concerto/managed-bash`.
    pub fn for_data_dir() -> Result<Self, ManagedRuntimeError> {
        let data = dirs::data_dir().ok_or_else(|| {
            ManagedRuntimeError::DataDirUnavailable("dirs::data_dir returned None".into())
        })?;
        Ok(Self { root: data.join("concerto").join("managed-bash") })
    }

    /// Detect the installed runtime from the default data dir, if present.
    pub fn auto_detect() -> Option<RuntimeManifest> {
        Self::for_data_dir().ok().and_then(|m| m.detect())
    }

    /// Build a manager rooted at an explicit directory (used by tests).
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// Root directory this manager operates on.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Path of the runtime manifest file.
    pub fn manifest_path(&self) -> PathBuf {
        self.root.join("runtime-manifest.json")
    }

    fn detect(&self) -> Option<RuntimeManifest> {
        let path = self.manifest_path();
        let content = std::fs::read_to_string(&path).ok()?;
        let manifest: RuntimeManifest = serde_json::from_str(&content).ok()?;
        if manifest.bash_executable.is_file() {
            Some(manifest)
        } else {
            None
        }
    }

    /// Adopt `source` (a local Bash executable) into the managed directory,
    /// recording a versioned, integrity-checked manifest. Returns the manifest.
    pub fn install_from(&self, source: &Path) -> Result<RuntimeManifest, ManagedRuntimeError> {
        if !source.is_file() {
            return Err(ManagedRuntimeError::SourceNotFound(source.display().to_string()));
        }
        let version = Self::version_of(source).unwrap_or_else(|| "unknown".into());
        let install_dir = self.root.join(sanitize(&version));
        std::fs::create_dir_all(&install_dir)?;
        let dest = install_dir.join("bash");
        std::fs::copy(source, &dest)?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&dest)?.permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&dest, perms)?;
        }

        let hash = hash_file(&dest)?;
        let integrity = IntegrityInfo { algorithm: "blake3".into(), hash };
        let tool_manifest = ToolManifest {
            tools: vec![ToolEntry {
                name: "bash".into(),
                path: dest.clone(),
                version: Some(version.clone()),
                integrity: Some(integrity.clone()),
            }],
        };
        let manifest = RuntimeManifest {
            version: version.clone(),
            bash_executable: dest.clone(),
            bash_version: version,
            installed_at: chrono::Utc::now().to_rfc3339(),
            offline: true,
            integrity_enabled: true,
            runtime_integrity: integrity,
            tool_manifest,
        };
        std::fs::write(self.manifest_path(), serde_json::to_string_pretty(&manifest)?)?;
        Ok(manifest)
    }

    /// Remove the entire managed runtime directory.
    pub fn remove(&self) -> Result<(), ManagedRuntimeError> {
        if self.root.exists() {
            std::fs::remove_dir_all(&self.root)?;
        }
        Ok(())
    }

    /// Verify integrity of the runtime and every recorded tool.
    pub fn verify(
        &self,
        manifest: &RuntimeManifest,
    ) -> Result<IntegrityReport, ManagedRuntimeError> {
        let mut entries = Vec::new();
        for tool in &manifest.tool_manifest.tools {
            let actual = hash_file(&tool.path).ok();
            let expected = tool.integrity.as_ref().map(|i| i.hash.clone());
            let status = match (&actual, &expected) {
                (Some(a), Some(e)) if a == e => IntegrityStatus::Ok,
                (Some(a), Some(e)) => {
                    IntegrityStatus::Mismatch { expected: e.clone(), actual: a.clone() }
                }
                (None, _) => IntegrityStatus::Missing,
                _ => IntegrityStatus::Unknown,
            };
            entries.push(IntegrityEntry {
                name: tool.name.clone(),
                path: tool.path.clone(),
                status,
            });
        }
        let runtime_ok = match hash_file(&manifest.bash_executable) {
            Ok(h) => h == manifest.runtime_integrity.hash,
            Err(_) => false,
        };
        Ok(IntegrityReport { runtime_ok, entries })
    }

    /// Serialize a manifest to pretty JSON (export).
    pub fn export_manifest(manifest: &RuntimeManifest) -> Result<String, ManagedRuntimeError> {
        Ok(serde_json::to_string_pretty(manifest)?)
    }

    /// Parse a previously exported manifest (import). Validates that the
    /// referenced bash executable still exists on disk.
    pub fn import_manifest(json: &str) -> Result<RuntimeManifest, ManagedRuntimeError> {
        let manifest: RuntimeManifest = serde_json::from_str(json)?;
        if !manifest.bash_executable.is_file() {
            return Err(ManagedRuntimeError::SourceNotFound(
                manifest.bash_executable.display().to_string(),
            ));
        }
        Ok(manifest)
    }

    /// Best-effort, bounded version probe (`<exe> --version`, first line).
    fn version_of(exe: &Path) -> Option<String> {
        let exe = exe.to_path_buf();
        let (tx, rx) = std::sync::mpsc::channel::<Option<String>>();
        let _ = std::thread::spawn(move || {
            let v = std::process::Command::new(&exe).arg("--version").output().ok().and_then(|o| {
                String::from_utf8_lossy(&o.stdout).lines().next().map(|l| l.trim().to_string())
            });
            let _ = tx.send(v);
        });
        rx.recv_timeout(std::time::Duration::from_secs(2)).ok().flatten()
    }
}

/// Blake3 hash of a file's bytes, as a hex string.
fn hash_file(path: &Path) -> std::io::Result<String> {
    let bytes = std::fs::read(path)?;
    Ok(blake3::hash(&bytes).to_hex().to_string())
}

/// Replace any non-alphanumeric character so a version string is a safe dir name.
fn sanitize(value: &str) -> String {
    value.chars().map(|c| if c.is_alphanumeric() { c } else { '_' }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_fake_bash(dir: &Path, name: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, "#!/bin/sh\necho hi\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&p).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&p, perms).unwrap();
        }
        p
    }

    #[test]
    fn install_detect_verify_remove_lifecycle() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = ManagedRuntimeManager::new(tmp.path().join("managed-bash"));
        assert!(mgr.detect().is_none(), "fresh manager should detect nothing");

        let source = make_fake_bash(tmp.path(), "bash-src");
        let manifest = mgr.install_from(&source).expect("install should succeed");
        assert!(manifest.bash_executable.is_file());
        // version_of probes `<source> --version`; the fake script echoes "hi",
        // so the recorded version reflects whatever the probe captured.
        assert!(!manifest.version.is_empty());

        let detected = mgr.detect().expect("should detect installed runtime");
        assert_eq!(detected.bash_executable, manifest.bash_executable);

        let report = mgr.verify(&detected).expect("verify should succeed");
        assert!(report.runtime_ok, "integrity should match after install");
        assert!(report.entries.iter().all(|e| e.status == IntegrityStatus::Ok));

        mgr.remove().expect("remove should succeed");
        assert!(mgr.detect().is_none(), "should detect nothing after remove");
    }

    #[test]
    fn export_import_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = ManagedRuntimeManager::new(tmp.path().join("managed-bash"));
        let source = make_fake_bash(tmp.path(), "bash-src");
        let manifest = mgr.install_from(&source).unwrap();

        let json = ManagedRuntimeManager::export_manifest(&manifest).unwrap();
        let imported = ManagedRuntimeManager::import_manifest(&json).unwrap();
        assert_eq!(imported.bash_executable, manifest.bash_executable);
    }

    #[test]
    fn import_rejects_missing_executable() {
        let bad = RuntimeManifest {
            version: "x".into(),
            bash_executable: PathBuf::from("/no/such/bash"),
            bash_version: "x".into(),
            installed_at: "2026-01-01T00:00:00Z".into(),
            offline: true,
            integrity_enabled: true,
            runtime_integrity: IntegrityInfo {
                algorithm: "blake3".into(),
                hash: "deadbeef".into(),
            },
            tool_manifest: ToolManifest { tools: vec![] },
        };
        let json = serde_json::to_string_pretty(&bad).unwrap();
        assert!(ManagedRuntimeManager::import_manifest(&json).is_err());
    }
}
