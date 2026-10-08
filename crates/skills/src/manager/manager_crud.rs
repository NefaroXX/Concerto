//! Skill-pack CRUD: `create_pack`, `update_pack`, `delete_pack`, and the
//! atomic-write helpers only they use.
//!
//! `SkillManager`'s on-disk management API (ADR-43 — the settings UI's pack
//! editor) lives in this second `impl SkillManager` block, together with id
//! validation, TOML serialization, atomic writes, resource relativization,
//! and the backup-stamp clock, so the parent `manager.rs` keeps discovery and
//! loading focused. The three methods stay `pub`: they are part of this
//! crate's public API (called from `concerto-cli` and the desktop settings
//! UI), so their effective visibility is unchanged. Their test cluster
//! (`skill_manifest` plus the CRUD tests) moves with them and shares the
//! parent's test fixtures through `pub(super)`.

use super::*;

impl SkillManager {
    /// Create a new `skill.toml` skill pack at `parent/<manifest.id>`.
    ///
    /// * The id is validated: non-empty single path component after trimming
    ///   (no `/`, `\`, `:`, NUL; not `.`/`..`). The manifest's `id` is
    ///   normalized to the trimmed value so the written manifest stays
    ///   consistent with its directory name.
    /// * `parent` is created when missing.
    /// * Any existing directory at the target path fails with
    ///   [`SkillsError::AlreadyExists`].
    /// * A manifest carrying `instructions_path` cannot be persisted by create
    ///   ([`SkillsError::UnsupportedFormat`]) — this API writes inline
    ///   `instructions` only.
    /// * The manifest is serialized to TOML and written atomically (temp file
    ///   + rename) so discovery never observes a partial manifest.
    ///
    /// Returns the created pack directory.
    pub fn create_pack(
        &self,
        parent: &Path,
        manifest: &SkillManifest,
    ) -> Result<PathBuf, SkillsError> {
        let id = validate_new_skill_id(&manifest.id, parent)?;
        let pack_dir = parent.join(&id);
        if pack_dir.exists() {
            return Err(SkillsError::AlreadyExists { path: pack_dir });
        }
        if manifest.instructions_path.is_some() {
            return Err(SkillsError::UnsupportedFormat {
                path: pack_dir,
                detail: "create_pack persists inline `instructions`; an `instructions_path` manifest cannot be created this way".into(),
            });
        }
        let mut manifest = manifest.clone();
        manifest.id = id;
        let toml = serialize_manifest(&manifest)?;

        fs::create_dir_all(parent)
            .map_err(|e| SkillsError::Io { path: parent.to_path_buf(), source: e })?;
        fs::create_dir(&pack_dir)
            .map_err(|e| SkillsError::Io { path: pack_dir.clone(), source: e })?;

        if let Err(error) = write_atomic(&pack_dir.join("skill.toml"), &toml) {
            let _ = fs::remove_dir_all(&pack_dir);
            return Err(error);
        }
        Ok(pack_dir)
    }

    /// Rewrite the `skill.toml` manifest of an existing pack in place.
    ///
    /// * The pack directory must exist and contain `skill.toml`. A
    ///   `SKILL.md`-only pack is [`SkillsError::UnsupportedFormat`] (edit it
    ///   by hand or recreate it); a directory with no manifest at all is
    ///   [`SkillsError::NotAPack`].
    /// * The manifest `id` is not editable: it is forced to the pack
    ///   directory's name so identity and path stay in sync.
    /// * Inline `instructions` always win: when present, any
    ///   `instructions_path` is cleared. A manifest with `instructions_path`
    ///   but no inline instructions is [`SkillsError::UnsupportedFormat`]
    ///   because this API does not manage instruction files.
    /// * `resources` resolving inside the pack directory are stored relative
    ///   to it; other paths pass through unchanged.
    /// * The rewritten manifest is written atomically.
    pub fn update_pack(
        &self,
        pack_dir: &Path,
        manifest: &SkillManifest,
    ) -> Result<(), SkillsError> {
        if !pack_dir.is_dir() {
            return Err(SkillsError::NotAPack { path: pack_dir.to_path_buf() });
        }
        let toml_path = pack_dir.join("skill.toml");
        if !toml_path.exists() {
            if pack_dir.join("SKILL.md").exists() {
                return Err(SkillsError::UnsupportedFormat {
                    path: pack_dir.to_path_buf(),
                    detail: "SKILL.md packs cannot be updated; edit the file directly or recreate the pack".into(),
                });
            }
            return Err(SkillsError::NotAPack { path: pack_dir.to_path_buf() });
        }

        let dir_name =
            pack_dir.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let id = validate_new_skill_id(&dir_name, pack_dir)?;

        let mut manifest = manifest.clone();
        manifest.id = id;
        if manifest.instructions.is_some() {
            manifest.instructions_path = None;
        } else if manifest.instructions_path.is_some() {
            return Err(SkillsError::UnsupportedFormat {
                path: pack_dir.to_path_buf(),
                detail: "an `instructions_path` manifest cannot be persisted without managing instruction files; use inline `instructions`".into(),
            });
        }
        manifest.resources =
            manifest.resources.iter().map(|r| relativize_under(pack_dir, r)).collect();

        write_atomic(&toml_path, &serialize_manifest(&manifest)?)
    }

    /// Delete a skill pack by renaming its manifest file(s) to hidden
    /// `.deleted-<stamp>-skill.toml` / `.deleted-<stamp>-SKILL.md` backups in
    /// place (one shared stamp per call), so the pack disappears from
    /// discovery but the committed files remain recoverable (reversible
    /// delete). Both manifest formats are backed up when both are present.
    ///
    /// Returns the backup paths. A directory with no manifest is
    /// [`SkillsError::NotAPack`].
    pub fn delete_pack(&self, pack_dir: &Path) -> Result<Vec<PathBuf>, SkillsError> {
        if !pack_dir.is_dir() {
            return Err(SkillsError::NotAPack { path: pack_dir.to_path_buf() });
        }
        let mut manifests: Vec<(&'static str, PathBuf)> = Vec::with_capacity(2);
        for name in ["skill.toml", "SKILL.md"] {
            let path = pack_dir.join(name);
            if path.exists() {
                manifests.push((name, path));
            }
        }
        if manifests.is_empty() {
            return Err(SkillsError::NotAPack { path: pack_dir.to_path_buf() });
        }

        let stamp = epoch_nanos().to_string();
        let mut backups = Vec::with_capacity(manifests.len());
        for (name, path) in manifests {
            let backup = pack_dir.join(format!(".deleted-{stamp}-{name}"));
            fs::rename(&path, &backup)
                .map_err(|e| SkillsError::Io { path: path.clone(), source: e })?;
            backups.push(backup);
        }
        Ok(backups)
    }
}

/// Validate a *new* skill id for create/rename operations: non-empty after
/// trimming, a single path component (no `/`, `\`, `:`, or NUL), and not
/// `.`/`..`. The checks mirror [`validate_skill_id`]'s strict rules so a pack
/// created here can never be rejected by discovery later (the loader is
/// lenient about empty ids, but create with an empty id is meaningless). The
/// trimmed value is returned.
pub(super) fn validate_new_skill_id(raw_id: &str, parent: &Path) -> Result<String, SkillsError> {
    let id = raw_id.trim();
    if id.is_empty() || id == "." || id == ".." || id.contains(['/', '\\', ':', '\0']) {
        return Err(SkillsError::InvalidId { id: raw_id.to_string(), path: parent.to_path_buf() });
    }
    Ok(id.to_string())
}

/// Serialize a manifest to TOML for disk writes. Serialization failures map
/// to `ManifestParse` with a synthetic `<serialize>` path, matching the
/// existing round-trip test convention.
pub(super) fn serialize_manifest(manifest: &SkillManifest) -> Result<String, SkillsError> {
    toml::to_string(manifest).map_err(|e| SkillsError::ManifestParse {
        path: PathBuf::from("<serialize>"),
        detail: e.to_string(),
    })
}

/// Atomically write `contents` to `path`: write to a sibling temp file, then
/// rename over the target. Rename-over-existing is atomic on POSIX; on Windows
/// the destination is removed first (a small non-atomic window, but discovery
/// only reads complete files). The temp file is removed on failure.
pub(super) fn write_atomic(path: &Path, contents: &str) -> Result<(), SkillsError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let temp_path = parent.join(format!(".{name}.tmp-{}", epoch_nanos()));
    let result = fs::write(&temp_path, contents).and_then(|()| {
        if cfg!(windows) && path.exists() {
            fs::remove_file(path)?;
        }
        fs::rename(&temp_path, path)
    });
    if let Err(source) = result {
        let _ = fs::remove_file(&temp_path);
        return Err(SkillsError::Io { path: path.to_path_buf(), source });
    }
    Ok(())
}

/// Strip `base` from `path` when `path` is absolute and resolves inside
/// `base`, returning a relative path; otherwise pass `path` through unchanged.
/// Keeps pack-relative `resources` stored relative on update.
pub(super) fn relativize_under(base: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        if let Ok(relative) = path.strip_prefix(base) {
            return relative.to_path_buf();
        }
    }
    path.to_path_buf()
}

/// Monotonic-ish wall-clock timestamp in nanoseconds since the Unix epoch,
/// used to make temp and backup file names unique. Falls back to `0` when the
/// clock is before the epoch (in practice never).
pub(super) fn epoch_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    // The shared fixtures (`TempDir`, `write_file`, `write_toml_pack`,
    // `discover_in`, `ids`) stay in the parent's test module — marked
    // `pub(super)` — so these relocated CRUD tests and the discovery tests
    // there use one definition instead of duplicated scaffolding.
    use super::super::tests::{discover_in, ids, write_file, write_toml_pack, TempDir};

    /// A minimal manifest for CRUD tests.
    fn skill_manifest(id: &str, instructions: Option<&str>) -> SkillManifest {
        SkillManifest {
            id: id.to_string(),
            name: "Test Skill".into(),
            version: "1.0.0".into(),
            description: "test".into(),
            instructions_path: None,
            instructions: instructions.map(str::to_string),
            tools: Vec::new(),
            resources: Vec::new(),
        }
    }

    #[test]
    fn create_pack_writes_discoverable_pack() -> Result<(), SkillsError> {
        let temp = TempDir::new("crud-create")?;
        let parent = temp.path().join("skills");
        let manager = SkillManager::new(vec![]);
        let pack_dir = manager
            .create_pack(&parent, &skill_manifest("fresh-skill", Some("Do the new thing.")))?;
        assert_eq!(pack_dir, parent.join("fresh-skill"));
        assert!(pack_dir.join("skill.toml").exists());

        let found = discover_in(&parent)?;
        assert_eq!(ids(&found), vec!["fresh-skill"]);
        assert_eq!(found[0].instructions, "Do the new thing.");
        assert_eq!(found[0].manifest.version, "1.0.0");
        assert_eq!(found[0].pack_dir, pack_dir);
        Ok(())
    }

    #[test]
    fn create_pack_creates_missing_parent() -> Result<(), SkillsError> {
        let temp = TempDir::new("crud-parent")?;
        let parent = temp.path().join("deeply/nested/skills");
        let manager = SkillManager::new(vec![]);
        manager.create_pack(&parent, &skill_manifest("x", Some("body")))?;
        assert!(parent.join("x/skill.toml").exists());
        Ok(())
    }

    #[test]
    fn create_pack_rejects_unsafe_ids() -> Result<(), SkillsError> {
        let temp = TempDir::new("crud-id")?;
        let parent = temp.path().join("skills");
        let manager = SkillManager::new(vec![]);
        let unsafe_ids = ["..", ".", "a/b", "a\\b", "a\0b", "a:b", "  "];
        for bad in unsafe_ids {
            let err = manager
                .create_pack(&parent, &skill_manifest(bad, Some("body")))
                .expect_err("unsafe id must be rejected");
            assert!(
                matches!(&err, SkillsError::InvalidId { path, .. } if path == &parent),
                "for id {bad:?}: {err}"
            );
        }
        Ok(())
    }

    #[test]
    fn create_pack_normalizes_whitespace_padded_id() -> Result<(), SkillsError> {
        let temp = TempDir::new("crud-trim")?;
        let parent = temp.path().join("skills");
        let manager = SkillManager::new(vec![]);
        let pack_dir =
            manager.create_pack(&parent, &skill_manifest("  trimmed-id  ", Some("x")))?;
        assert_eq!(pack_dir, parent.join("trimmed-id"));
        let found = discover_in(&parent)?;
        assert_eq!(ids(&found), vec!["trimmed-id"]);
        Ok(())
    }

    #[test]
    fn create_pack_rejects_existing_pack() -> Result<(), SkillsError> {
        let temp = TempDir::new("crud-exists")?;
        let parent = temp.path().join("skills");
        write_toml_pack(&parent.join("take"), "take", "existing")?;
        let manager = SkillManager::new(vec![]);
        let err = manager
            .create_pack(&parent, &skill_manifest("take", Some("x")))
            .expect_err("must fail");
        assert!(
            matches!(&err, SkillsError::AlreadyExists { path } if path == &parent.join("take")),
            "{err}"
        );
        Ok(())
    }

    #[test]
    fn create_pack_rejects_instructions_path_without_creating_dir() -> Result<(), SkillsError> {
        let temp = TempDir::new("crud-ip")?;
        let parent = temp.path().join("skills");
        let mut manifest = skill_manifest("x", Some("inline"));
        manifest.instructions_path = Some(PathBuf::from("inst.md"));
        let manager = SkillManager::new(vec![]);
        let err = manager.create_pack(&parent, &manifest).expect_err("must fail");
        assert!(matches!(err, SkillsError::UnsupportedFormat { .. }), "{err}");
        assert!(!parent.join("x").exists(), "no pack directory may be left behind");
        Ok(())
    }

    #[test]
    fn update_pack_rewrites_manifest_and_relativizes_resources() -> Result<(), SkillsError> {
        let temp = TempDir::new("crud-update")?;
        let parent = temp.path().join("skills");
        write_toml_pack(&parent.join("mine"), "mine", "v1")?;
        let pack_dir = parent.join("mine");

        let mut manifest = skill_manifest("OTHER-ID", Some("v2 instructions"));
        manifest.name = "Updated".into();
        manifest.version = "2.0.0".into();
        manifest.description = "updated".into();
        manifest.tools = vec!["cargo test".into()];
        // An `instructions_path` is cleared while inline instructions win.
        manifest.instructions_path = Some(PathBuf::from("should-be-cleared.md"));
        manifest.resources = vec![pack_dir.join("fixtures/sample.md")];

        let manager = SkillManager::new(vec![]);
        manager.update_pack(&pack_dir, &manifest)?;

        // Resources are stored relative to the pack dir on disk (the loader
        // re-absolutizes `manifest.resources` in the descriptor, so assert on
        // the raw file).
        let raw = fs::read_to_string(pack_dir.join("skill.toml"))
            .map_err(|e| SkillsError::Io { path: pack_dir.join("skill.toml"), source: e })?;
        assert!(
            raw.contains("fixtures/sample.md") && !raw.contains("skills/mine/fixtures"),
            "resources must be persisted relative to the pack dir, got: {raw}"
        );

        let found = discover_in(&parent)?;
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, "mine");
        assert_eq!(found[0].manifest.name, "Updated");
        assert_eq!(found[0].manifest.version, "2.0.0");
        assert_eq!(found[0].manifest.instructions_path, None);
        assert_eq!(found[0].instructions, "v2 instructions");
        assert_eq!(found[0].resource_paths, vec![pack_dir.join("fixtures/sample.md")]);
        Ok(())
    }

    #[test]
    fn update_pack_rejects_skill_md_packs() -> Result<(), SkillsError> {
        let temp = TempDir::new("crud-update-md")?;
        let dir = temp.path().join("md-pack");
        write_file(&dir.join("SKILL.md"), "---\nid: md\n---\nbody\n")?;
        let manager = SkillManager::new(vec![]);
        let err =
            manager.update_pack(&dir, &skill_manifest("md", Some("x"))).expect_err("must fail");
        assert!(matches!(err, SkillsError::UnsupportedFormat { .. }), "{err}");
        Ok(())
    }

    #[test]
    fn update_pack_rejects_non_pack_dir() -> Result<(), SkillsError> {
        let temp = TempDir::new("crud-update-notpack")?;
        let dir = temp.path().join("plain");
        fs::create_dir_all(&dir).map_err(|e| SkillsError::Io { path: dir.clone(), source: e })?;
        let missing = temp.path().join("missing");
        let manager = SkillManager::new(vec![]);
        for bad in [&dir, &missing] {
            let err =
                manager.update_pack(bad, &skill_manifest("x", Some("x"))).expect_err("must fail");
            assert!(matches!(err, SkillsError::NotAPack { .. }), "{err}");
        }
        Ok(())
    }

    #[test]
    fn update_pack_rejects_instructions_path_without_inline() -> Result<(), SkillsError> {
        let temp = TempDir::new("crud-update-ip")?;
        let parent = temp.path().join("skills");
        write_toml_pack(&parent.join("x"), "x", "orig")?;
        let mut manifest = skill_manifest("x", None);
        manifest.instructions_path = Some(PathBuf::from("inst.md"));
        let manager = SkillManager::new(vec![]);
        let err = manager.update_pack(&parent.join("x"), &manifest).expect_err("must fail");
        assert!(matches!(err, SkillsError::UnsupportedFormat { .. }), "{err}");
        Ok(())
    }

    #[test]
    fn delete_pack_renames_manifest_to_hidden_backup() -> Result<(), SkillsError> {
        let temp = TempDir::new("crud-delete")?;
        let parent = temp.path().join("skills");
        write_toml_pack(&parent.join("gone"), "gone", "bye")?;
        let pack_dir = parent.join("gone");
        let manager = SkillManager::new(vec![]);
        let backups = manager.delete_pack(&pack_dir)?;
        assert_eq!(backups.len(), 1);
        assert!(backups[0].starts_with(&pack_dir));
        assert!(backups[0].to_string_lossy().ends_with("-skill.toml"));
        assert!(!pack_dir.join("skill.toml").exists());
        assert!(backups[0].exists());
        assert!(discover_in(&parent)?.is_empty());
        Ok(())
    }

    #[test]
    fn delete_pack_backs_up_both_manifests() -> Result<(), SkillsError> {
        let temp = TempDir::new("crud-delete-both")?;
        let pack_dir = temp.path().join("both");
        write_toml_pack(&pack_dir, "both", "toml")?;
        write_file(&pack_dir.join("SKILL.md"), "---\nid: both\n---\nmd\n")?;
        let manager = SkillManager::new(vec![]);
        let backups = manager.delete_pack(&pack_dir)?;
        assert_eq!(backups.len(), 2);
        assert!(backups.iter().any(|b| b.to_string_lossy().ends_with("-skill.toml")));
        assert!(backups.iter().any(|b| b.to_string_lossy().ends_with("-SKILL.md")));
        assert!(!pack_dir.join("skill.toml").exists());
        assert!(!pack_dir.join("SKILL.md").exists());
        Ok(())
    }

    #[test]
    fn delete_pack_rejects_non_pack_dir() -> Result<(), SkillsError> {
        let temp = TempDir::new("crud-delete-notpack")?;
        let dir = temp.path().join("plain");
        fs::create_dir_all(&dir).map_err(|e| SkillsError::Io { path: dir.clone(), source: e })?;
        let manager = SkillManager::new(vec![]);
        let err = manager.delete_pack(&dir).expect_err("must fail");
        assert!(matches!(err, SkillsError::NotAPack { .. }), "{err}");
        Ok(())
    }

    #[test]
    fn relativize_under_leaves_outside_or_relative_paths_unchanged() {
        let base = Path::new("/home/alice/skills/mine");
        assert_eq!(
            relativize_under(base, &base.join("fixtures/sample.md")),
            PathBuf::from("fixtures/sample.md")
        );
        // Outside the base: unchanged.
        assert_eq!(
            relativize_under(base, Path::new("/elsewhere/sample.md")),
            PathBuf::from("/elsewhere/sample.md")
        );
        // Already relative: unchanged.
        assert_eq!(
            relativize_under(base, Path::new("fixtures/sample.md")),
            PathBuf::from("fixtures/sample.md")
        );
    }
}
