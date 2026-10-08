//! `SkillManager` — skill-pack discovery, loading, and management.
//!
//! A skill pack is a directory containing a `skill.toml` manifest or a
//! `SKILL.md` file with YAML-subset front matter. When both are present,
//! `skill.toml` wins. Discovery walks each search path to a bounded depth,
//! parses every pack it finds, resolves all paths to absolute form, and
//! returns descriptors deterministically sorted by id (duplicate ids keep the
//! first occurrence). Discovery is lenient: a broken pack is logged and
//! skipped, never aborting the rest of the scan.
//!
//! [`SkillManager`] also manages packs on disk for the settings UI:
//! [`SkillManager::create_pack`] writes a new `skill.toml` pack,
//! [`SkillManager::update_pack`] rewrites an existing pack's `skill.toml`
//! in place, and [`SkillManager::delete_pack`] removes a pack from discovery
//! by renaming its manifest file(s) to hidden `.deleted-<stamp>-*` backups
//! (reversible, git-style). CRUD is confined to explicit pack directories;
//! every attempted write is preceded by id/path validation.
//!
//! Search paths may contain `~` or Windows-style `%VAR%` references (e.g.
//! `%APPDATA%`); they are expanded before scanning. [`SkillManager::discover`]
//! logs warnings at `warn` level and informational notes at `info` level;
//! [`SkillManager::discover_with_report`] returns all of them as data so
//! callers (e.g. a settings UI) can surface them, and
//! [`expanded_search_path`] expands a single configured path for display
//! without scanning.

use crate::error::SkillsError;
use crate::frontmatter::parse_front_matter;
use crate::manifest::parse_skill_toml;
use concerto_api_types::extension::{SkillDescriptor, SkillManifest};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use tracing::{debug, info, warn};

// The path-expansion cluster (`expanded_search_path` and its `~` / `%VAR%`
// helpers), extracted to `manager/manager_paths.rs`; `expanded_search_path`
// is re-exported so the public `concerto_skills::expanded_search_path` path
// keeps resolving unchanged, and the three helpers this file still calls
// (`expand_home`, `absolute_path`, `resolve_all`) are imported below.
mod manager_paths;
pub use manager_paths::expanded_search_path;
use manager_paths::{absolute_path, expand_home, resolve_all};

// The skill-pack CRUD cluster (`create_pack` / `update_pack` / `delete_pack`
// plus the atomic-write helpers only they use), extracted to
// `manager/manager_crud.rs` as a second `impl SkillManager` block. No `use`
// re-export is needed: only inherent methods move, and they stay `pub` (this
// crate's public API, called by `concerto-cli` and the desktop settings UI).
mod manager_crud;

/// Maximum nesting depth of a skill pack relative to a search path root.
/// The root itself is depth 0; packs deeper than this are not discovered.
const MAX_SKILL_DEPTH: usize = 4;

/// Outcome of a skill discovery pass, including the diagnostics that
/// [`SkillManager::discover`] would otherwise only log. A settings UI can
/// surface these to explain a "no skills found" result — missing or invalid
/// search paths, packs skipped for malformed manifests, duplicate ids.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DiscoveryReport {
    /// The searched directories after `~` / `%VAR%` expansion, in configured
    /// order. Paths that could not be expanded are omitted (they are reported
    /// in [`DiscoveryReport::warnings`] instead).
    pub resolved_paths: Vec<PathBuf>,
    /// Discovered skill packs, deterministically sorted by id (duplicate ids
    /// collapsed).
    pub descriptors: Vec<SkillDescriptor>,
    /// Human-readable per-path and per-pack diagnostics.
    pub warnings: Vec<String>,
    /// Informational diagnostics (not failures) from the same pass: e.g. a
    /// pack whose manifest id is empty was loaded under its pack directory's
    /// name. [`SkillManager::discover`] logs these at `info` level; a settings
    /// UI can surface them as quiet notes.
    pub notes: Vec<String>,
}

/// Discovers and loads local skill packs (ADR-43, decision 1).
///
/// `SkillManager` holds no configuration state: search paths and enabled ids
/// are passed in as parameters so the crate does not depend on `concerto-config`.
#[derive(Debug, Clone)]
pub struct SkillManager {
    search_paths: Vec<PathBuf>,
}

impl SkillManager {
    /// Create a manager for the given search paths. `~` and Windows-style
    /// `%VAR%` references are expanded lazily at discovery time.
    pub fn new(search_paths: Vec<PathBuf>) -> Self {
        Self { search_paths }
    }

    /// Discover every skill pack under the configured search paths.
    ///
    /// - Missing search paths are warned about and skipped (not an error).
    /// - Directories without a manifest are silently not skills.
    /// - A pack that fails to load (malformed manifest, invalid id, unreadable
    ///   directory) is warned about and skipped; discovery of the remaining
    ///   packs continues.
    /// - Results are sorted by id; duplicate ids keep the first occurrence
    ///   (deterministic: lowest `(id, path)` wins) and are warned about.
    ///
    /// This is a convenience over [`SkillManager::discover_with_report`] that
    /// logs every warning at `warn` level, informational notes and one
    /// per-pack summary (id, version, path, instruction chars) at `info`
    /// level, and discards them. Callers that need the diagnostics as data
    /// (e.g. a settings UI) should call [`SkillManager::discover_with_report`]
    /// directly.
    pub fn discover(&self) -> Result<Vec<SkillDescriptor>, SkillsError> {
        let report = self.discover_with_report()?;
        for warning in &report.warnings {
            warn!(warning = %warning, "skill discovery warning");
        }
        for note in &report.notes {
            info!(note = %note, "using directory name as id");
        }
        // One `info` line per loaded pack proves which packs were discovered
        // and how large each one's instruction text is. Ids, version, path,
        // and size only — never the instruction content.
        for descriptor in &report.descriptors {
            info!(
                id = %descriptor.id,
                version = %descriptor.manifest.version,
                path = %descriptor.pack_dir.display(),
                chars = descriptor.instructions.chars().count(),
                "skill pack loaded"
            );
        }
        Ok(report.descriptors)
    }

    /// Discover every skill pack and return the diagnostics alongside them.
    ///
    /// Equivalent to [`SkillManager::discover`], but the diagnostics are
    /// returned as [`DiscoveryReport`] data (resolved search paths, discovered
    /// descriptors, and per-path/per-pack warnings plus informational notes)
    /// so a caller can surface them instead of relying on `tracing`. Discovery
    /// semantics are otherwise identical. This function never fails on a
    /// broken pack or a missing path; `Err` is reserved for genuinely
    /// unexpected conditions.
    pub fn discover_with_report(&self) -> Result<DiscoveryReport, SkillsError> {
        let mut found: Vec<(PathBuf, SkillDescriptor)> = Vec::new();
        let mut failures: Vec<SkillsError> = Vec::new();
        let mut warnings: Vec<String> = Vec::new();
        let mut notes: Vec<String> = Vec::new();
        let mut resolved_paths: Vec<PathBuf> = Vec::new();
        for raw in &self.search_paths {
            let Some(root) = expand_home(raw) else {
                warnings.push(format!(
                    "skill search path `{}` uses `~` but no home directory is known; skipping",
                    raw.display()
                ));
                continue;
            };
            resolved_paths.push(root.clone());
            if !root.is_dir() {
                warnings.push(format!(
                    "skill search path `{}` is missing or not a directory; skipping",
                    root.display()
                ));
                continue;
            }
            self.walk(&root, 0, &mut found, &mut failures, &mut notes);
        }
        for failure in &failures {
            warnings.push(format!("skill pack failed to load; skipping it: {failure}"));
        }

        // Deterministic dedup: sort by (id, path), keep the first of any id.
        found.sort_by(|(path_a, desc_a), (path_b, desc_b)| {
            desc_a.id.cmp(&desc_b.id).then_with(|| path_a.cmp(path_b))
        });
        let mut seen: HashSet<String> = HashSet::new();
        let mut discovered = Vec::with_capacity(found.len());
        for (_, descriptor) in found {
            if !seen.insert(descriptor.id.clone()) {
                warnings.push(format!(
                    "duplicate skill id `{}`; keeping first occurrence",
                    descriptor.id
                ));
                continue;
            }
            discovered.push(descriptor);
        }
        Ok(DiscoveryReport { resolved_paths, descriptors: discovered, warnings, notes })
    }

    /// Filter discovered skills to those explicitly enabled.
    ///
    /// `None` returns every skill; `Some(ids)` returns only the matching
    /// skills, in caller order, with unknown ids warned about and skipped.
    /// Each requested id is whitespace-trimmed before matching; ids that trim
    /// to empty are warned about and skipped.
    pub fn resolve_enabled<'a>(
        &self,
        all: &'a [SkillDescriptor],
        enabled_ids: Option<&[String]>,
    ) -> Vec<&'a SkillDescriptor> {
        let Some(ids) = enabled_ids else {
            return all.iter().collect();
        };
        let by_id: HashMap<&str, &'a SkillDescriptor> =
            all.iter().map(|d| (d.id.as_str(), d)).collect();
        let mut resolved = Vec::with_capacity(ids.len());
        for id in ids {
            let trimmed = id.trim();
            if trimmed.is_empty() {
                warn!(id = %id, "enabled skill id is empty after trimming; skipping");
                continue;
            }
            match by_id.get(trimmed) {
                Some(descriptor) => resolved.push(*descriptor),
                None => {
                    warn!(id = %trimmed, "enabled skill id not found among discovered skills; skipping")
                }
            }
        }
        resolved
    }

    /// Recursively walk `dir` to `MAX_SKILL_DEPTH`, collecting skill packs and
    /// per-directory/per-pack load failures. A directory that is itself a pack
    /// is a leaf: it is not descended into.
    fn walk(
        &self,
        dir: &Path,
        depth: usize,
        out: &mut Vec<(PathBuf, SkillDescriptor)>,
        failures: &mut Vec<SkillsError>,
        notes: &mut Vec<String>,
    ) {
        if depth > MAX_SKILL_DEPTH {
            return;
        }
        match self.load_pack(dir, notes) {
            Ok(Some(pack)) => {
                out.push((dir.to_path_buf(), pack));
                return;
            }
            Ok(None) => {}
            Err(failure) => {
                failures.push(failure);
                return;
            }
        }
        if depth < MAX_SKILL_DEPTH {
            let entries = match fs::read_dir(dir) {
                Ok(entries) => entries,
                Err(source) => {
                    failures.push(SkillsError::Io { path: dir.to_path_buf(), source });
                    return;
                }
            };
            let mut children = Vec::new();
            for entry in entries {
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(source) => {
                        failures.push(SkillsError::Io { path: dir.to_path_buf(), source });
                        continue;
                    }
                };
                let file_type = match entry.file_type() {
                    Ok(file_type) => file_type,
                    Err(source) => {
                        failures.push(SkillsError::Io { path: entry.path(), source });
                        continue;
                    }
                };
                if file_type.is_dir() {
                    children.push(entry.path());
                }
            }
            children.sort();
            for child in children {
                self.walk(&child, depth + 1, out, failures, notes);
            }
        }
    }

    /// Load the pack at `dir`, if it has a manifest. `None` means the
    /// directory is not a skill pack. `skill.toml` wins over `SKILL.md` when
    /// both are present.
    fn load_pack(
        &self,
        dir: &Path,
        notes: &mut Vec<String>,
    ) -> Result<Option<SkillDescriptor>, SkillsError> {
        let toml_path = dir.join("skill.toml");
        let md_path = dir.join("SKILL.md");
        if toml_path.exists() {
            if md_path.exists() {
                debug!(
                    dir = %dir.display(),
                    "both skill.toml and SKILL.md present; skill.toml wins"
                );
            }
            return self.load_toml_pack(dir, &toml_path, notes).map(Some);
        }
        if md_path.exists() {
            return self.load_md_pack(dir, &md_path, notes).map(Some);
        }
        Ok(None)
    }

    /// Load a `skill.toml`-based pack, resolving `instructions_path`,
    /// resources, and the instruction text.
    fn load_toml_pack(
        &self,
        dir: &Path,
        manifest_path: &Path,
        notes: &mut Vec<String>,
    ) -> Result<SkillDescriptor, SkillsError> {
        let text = fs::read_to_string(manifest_path)
            .map_err(|e| SkillsError::Io { path: manifest_path.to_path_buf(), source: e })?;
        let mut manifest = parse_skill_toml(&text, manifest_path)?;
        let (id, used_directory_name) = validate_skill_id(&manifest.id, dir)?;
        if used_directory_name {
            notes.push(format!("Skill '{id}' loaded via directory name"));
        }
        manifest.id = id.clone();

        // `instructions_path` takes precedence over inline `instructions`
        // when both are set (the manifest contract says at most one).
        let instructions = match manifest.instructions_path.clone() {
            Some(relative) => {
                let absolute = absolute_path(dir, &relative);
                let text = fs::read_to_string(&absolute)
                    .map_err(|e| SkillsError::Io { path: absolute.clone(), source: e })?;
                manifest.instructions_path = Some(absolute);
                text
            }
            None => manifest.instructions.clone().unwrap_or_default(),
        };

        let resource_paths = resolve_all(dir, &manifest.resources);
        manifest.resources = resource_paths.clone();

        Ok(SkillDescriptor {
            id: manifest.id.clone(),
            manifest,
            instructions,
            pack_dir: dir.to_path_buf(),
            resource_paths,
        })
    }

    /// Load a `SKILL.md`-based pack. The markdown body below the closing `---`
    /// delimiter is the instruction text; a front-matter `instructions` key is
    /// used only when the body is empty.
    fn load_md_pack(
        &self,
        dir: &Path,
        manifest_path: &Path,
        notes: &mut Vec<String>,
    ) -> Result<SkillDescriptor, SkillsError> {
        let text = fs::read_to_string(manifest_path)
            .map_err(|e| SkillsError::Io { path: manifest_path.to_path_buf(), source: e })?;
        let fm = parse_front_matter(&text).map_err(|detail| SkillsError::FrontMatter {
            path: manifest_path.to_path_buf(),
            detail,
        })?;

        let raw_id = fm.id.as_deref().unwrap_or_default();
        let (id, used_directory_name) = validate_skill_id(raw_id, dir)?;
        if used_directory_name {
            notes.push(format!("Skill '{id}' loaded via directory name"));
        }

        let instructions = if fm.body.trim().is_empty() {
            fm.instructions.clone().unwrap_or_default()
        } else {
            fm.body.clone()
        };
        let resource_paths = resolve_all(dir, &fm.resources);

        let manifest = SkillManifest {
            id: id.clone(),
            name: fm.name.unwrap_or_default(),
            version: fm.version.unwrap_or_default(),
            description: fm.description.unwrap_or_default(),
            instructions_path: None,
            instructions: if instructions.is_empty() { None } else { Some(instructions.clone()) },
            tools: fm.tools,
            resources: resource_paths.clone(),
        };

        Ok(SkillDescriptor {
            id,
            manifest,
            instructions,
            pack_dir: dir.to_path_buf(),
            resource_paths,
        })
    }
}

/// Resolve a skill id from a manifest, attaching the pack directory to
/// [`SkillsError::InvalidId`] for diagnostics.
///
/// An id that trims to empty is *not* an error: it falls back to the pack
/// directory's file name (e.g. a `SKILL.md` in `…/deepwork` without an `id`
/// loads as `deepwork`). Returns the resolved id and whether the directory
/// name was used. Non-empty ids that still contain a path separator, `:`, or
/// NUL after trimming stay strict errors.
fn validate_skill_id(raw_id: &str, dir: &Path) -> Result<(String, bool), SkillsError> {
    let trimmed = raw_id.trim();
    if trimmed.is_empty() {
        if let Some(name) = dir.file_name().filter(|name| !name.is_empty()) {
            return Ok((name.to_string_lossy().into_owned(), true));
        }
        return Err(SkillsError::InvalidId { id: raw_id.to_string(), path: dir.to_path_buf() });
    }
    if trimmed.contains(['/', '\\', '\0', ':']) {
        return Err(SkillsError::InvalidId { id: raw_id.to_string(), path: dir.to_path_buf() });
    }
    Ok((trimmed.to_string(), false))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write as _;

    /// Unique temp directory removed on drop. Avoids a `tempfile` dependency
    /// (not in `[workspace.dependencies]`).
    pub(super) struct TempDir(PathBuf);

    impl TempDir {
        pub(super) fn new(tag: &str) -> Result<Self, SkillsError> {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|e| SkillsError::Io {
                    path: PathBuf::from("<system clock>"),
                    source: std::io::Error::other(e),
                })?
                .as_nanos();
            let dir = std::env::temp_dir()
                .join(format!("concerto-skills-test-{tag}-{}-{nanos}", std::process::id()));
            fs::create_dir_all(&dir)
                .map_err(|e| SkillsError::Io { path: dir.clone(), source: e })?;
            Ok(Self(dir))
        }

        pub(super) fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    pub(super) fn write_file(path: &Path, contents: &str) -> Result<(), SkillsError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| SkillsError::Io { path: parent.to_path_buf(), source: e })?;
        }
        let mut file = File::create(path)
            .map_err(|e| SkillsError::Io { path: path.to_path_buf(), source: e })?;
        file.write_all(contents.as_bytes())
            .map_err(|e| SkillsError::Io { path: path.to_path_buf(), source: e })?;
        Ok(())
    }

    /// Write a minimal `skill.toml` pack with inline instructions.
    pub(super) fn write_toml_pack(
        dir: &Path,
        id: &str,
        instructions: &str,
    ) -> Result<(), SkillsError> {
        let toml = format!(
            "id = \"{id}\"\nname = \"Test Skill\"\nversion = \"1.0.0\"\ndescription = \"test\"\ninstructions = \"{instructions}\"\n"
        );
        write_file(&dir.join("skill.toml"), &toml)
    }

    pub(super) fn discover_in(root: &Path) -> Result<Vec<SkillDescriptor>, SkillsError> {
        let manager = SkillManager::new(vec![root.to_path_buf()]);
        manager.discover()
    }

    pub(super) fn ids(descriptors: &[SkillDescriptor]) -> Vec<&str> {
        descriptors.iter().map(|d| d.id.as_str()).collect()
    }

    #[test]
    fn discovery_finds_packs_and_skips_non_packs() -> Result<(), SkillsError> {
        let temp = TempDir::new("discover")?;
        write_toml_pack(&temp.path().join("pack-a"), "a", "alpha")?;
        // A pack at depth 4 must be found.
        write_toml_pack(&temp.path().join("nested/one/two/pack-b"), "b", "beta")?;
        // Non-pack dirs are silently skipped.
        write_file(&temp.path().join("not-a-pack/readme.txt"), "plain file")?;
        fs::create_dir_all(temp.path().join("empty-dir"))
            .map_err(|e| SkillsError::Io { path: temp.path().join("empty-dir"), source: e })?;

        let found = discover_in(temp.path())?;
        assert_eq!(ids(&found), vec!["a", "b"]);
        Ok(())
    }

    #[test]
    fn discovery_respects_depth_bound() -> Result<(), SkillsError> {
        let temp = TempDir::new("depth")?;
        // Depth 4 (root=0): found.
        write_toml_pack(&temp.path().join("n1/n2/n3/pack-ok"), "ok", "found")?;
        // Depth 5: not found.
        write_toml_pack(&temp.path().join("n1/n2/n3/n4/pack-deep"), "deep", "hidden")?;

        let found = discover_in(temp.path())?;
        assert_eq!(ids(&found), vec!["ok"]);
        Ok(())
    }

    #[test]
    fn skill_toml_full_parse_resolves_paths() -> Result<(), SkillsError> {
        let temp = TempDir::new("toml-full")?;
        let dir = temp.path().join("pack");
        write_file(
            &dir.join("skill.toml"),
            r#"
id = "rust-style"
name = "Rust Style"
version = "2.1.0"
description = "Style guidance"
instructions = "Prefer cargo nextest."
tools = ["cargo nextest run", "cargo clippy"]
resources = ["templates/style.md"]
unknown_field = "ignored"
"#,
        )?;

        let found = discover_in(temp.path())?;
        assert_eq!(found.len(), 1);
        let descriptor = &found[0];
        assert_eq!(descriptor.id, "rust-style");
        assert_eq!(descriptor.manifest.name, "Rust Style");
        assert_eq!(descriptor.manifest.version, "2.1.0");
        assert_eq!(descriptor.manifest.tools, vec!["cargo nextest run", "cargo clippy"]);
        assert_eq!(descriptor.resource_paths, vec![dir.join("templates/style.md")]);
        assert_eq!(descriptor.manifest.resources, descriptor.resource_paths);
        assert_eq!(descriptor.instructions, "Prefer cargo nextest.");
        Ok(())
    }

    #[test]
    fn skill_toml_defaults_and_instructions_path() -> Result<(), SkillsError> {
        let temp = TempDir::new("toml-defaults")?;
        let dir = temp.path().join("pack");
        write_file(&dir.join("instructions.md"), "Do the thing.")?;
        write_file(
            &dir.join("skill.toml"),
            "id = \"minimal\"\ninstructions_path = \"instructions.md\"\n",
        )?;

        let found = discover_in(temp.path())?;
        assert_eq!(found.len(), 1);
        let descriptor = &found[0];
        assert_eq!(descriptor.manifest.name, "");
        assert_eq!(descriptor.manifest.version, "");
        assert_eq!(descriptor.manifest.description, "");
        assert!(descriptor.manifest.tools.is_empty());
        assert_eq!(descriptor.manifest.instructions_path, Some(dir.join("instructions.md")));
        assert_eq!(descriptor.instructions, "Do the thing.");
        Ok(())
    }

    #[test]
    fn skill_md_front_matter_and_body() -> Result<(), SkillsError> {
        let temp = TempDir::new("md")?;
        let dir = temp.path().join("pack");
        write_file(
            &dir.join("SKILL.md"),
            r#"---
# leading comment
id: commit-style
name: Commit Style
version: 0.2.0
description: "Conventional commits, as a skill"
tools:
  - cargo test
  - cargo fmt
resources:
  - examples/commit.md
---
# Commit Style

Always use conventional commits:

- feat: add feature
- fix: repair bug
"#,
        )?;

        let found = discover_in(temp.path())?;
        assert_eq!(found.len(), 1);
        let descriptor = &found[0];
        assert_eq!(descriptor.id, "commit-style");
        assert_eq!(descriptor.manifest.name, "Commit Style");
        assert_eq!(descriptor.manifest.description, "Conventional commits, as a skill");
        assert_eq!(descriptor.manifest.tools, vec!["cargo test", "cargo fmt"]);
        assert_eq!(
            descriptor.manifest.instructions.as_deref(),
            Some("# Commit Style\n\nAlways use conventional commits:\n\n- feat: add feature\n- fix: repair bug")
        );
        assert_eq!(
            descriptor.instructions,
            "# Commit Style\n\nAlways use conventional commits:\n\n- feat: add feature\n- fix: repair bug"
        );
        assert_eq!(descriptor.resource_paths, vec![dir.join("examples/commit.md")]);
        assert!(descriptor.manifest.instructions_path.is_none());
        Ok(())
    }

    #[test]
    fn front_matter_error_includes_manifest_path() -> Result<(), SkillsError> {
        let temp = TempDir::new("md-fm")?;
        // Missing closing delimiter.
        let dir = temp.path().join("pack-a");
        write_file(&dir.join("SKILL.md"), "---\nid: broken\n")?;

        let manager = SkillManager::new(vec![]);
        let err = match manager.load_pack(&dir, &mut Vec::new()) {
            Err(e) => e,
            Ok(_) => panic!("expected a front-matter error"),
        };
        assert!(
            matches!(
                &err,
                SkillsError::FrontMatter { path, detail }
                    if path == &dir.join("SKILL.md") && detail.contains("closing `---`")
            ),
            "unexpected error: {err}"
        );
        Ok(())
    }

    #[test]
    fn malformed_front_matter_is_skipped_not_fatal() -> Result<(), SkillsError> {
        let temp = TempDir::new("md-bad")?;
        // Missing closing delimiter.
        write_file(&temp.path().join("pack-a/SKILL.md"), "---\nid: broken\n")?;
        // Missing opening delimiter.
        write_file(&temp.path().join("pack-b/SKILL.md"), "# just a doc\n\nno front matter\n")?;
        // A healthy pack alongside survives discovery.
        write_toml_pack(&temp.path().join("good"), "good", "ok")?;

        let found = discover_in(temp.path())?;
        assert_eq!(ids(&found), vec!["good"]);
        Ok(())
    }

    #[test]
    fn skill_toml_error_includes_manifest_path() -> Result<(), SkillsError> {
        let temp = TempDir::new("toml-fm")?;
        let dir = temp.path().join("pack");
        write_file(&dir.join("skill.toml"), "id = [unclosed\n")?;

        let manager = SkillManager::new(vec![]);
        let err = match manager.load_pack(&dir, &mut Vec::new()) {
            Err(e) => e,
            Ok(_) => panic!("expected a manifest error"),
        };
        assert!(
            matches!(
                &err,
                SkillsError::ManifestParse { path, .. } if path == &dir.join("skill.toml")
            ),
            "unexpected error: {err}"
        );
        Ok(())
    }

    #[test]
    fn malformed_skill_toml_is_skipped_not_fatal() -> Result<(), SkillsError> {
        let temp = TempDir::new("toml-bad")?;
        write_file(&temp.path().join("pack/skill.toml"), "id = [unclosed\n")?;
        write_toml_pack(&temp.path().join("good"), "good", "ok")?;

        let found = discover_in(temp.path())?;
        assert_eq!(ids(&found), vec!["good"]);
        Ok(())
    }

    #[test]
    fn discover_skips_bad_packs_and_keeps_good_ones() -> Result<(), SkillsError> {
        let temp = TempDir::new("mixed")?;
        write_toml_pack(&temp.path().join("good"), "good", "ok")?;
        // Whitespace id in `skill.toml` → loads under the directory name.
        write_file(&temp.path().join("bad-id/skill.toml"), "id = \"   \"\n")?;
        // Malformed front matter in `SKILL.md`.
        write_file(&temp.path().join("bad-md/SKILL.md"), "---\nid: broken\n")?;
        // Malformed TOML in `skill.toml`.
        write_file(&temp.path().join("bad-toml/skill.toml"), "id = [unclosed\n")?;

        let found = discover_in(temp.path())?;
        assert_eq!(ids(&found), vec!["bad-id", "good"]);
        Ok(())
    }

    #[test]
    fn skill_toml_wins_over_skill_md() -> Result<(), SkillsError> {
        let temp = TempDir::new("both")?;
        let dir = temp.path().join("pack");
        write_file(&dir.join("skill.toml"), "id = \"toml-wins\"\ninstructions = \"from toml\"\n")?;
        write_file(&dir.join("SKILL.md"), "---\nid: md-loses\ntools:\n  - x\n---\nfrom md\n")?;

        let found = discover_in(temp.path())?;
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, "toml-wins");
        assert_eq!(found[0].instructions, "from toml");
        Ok(())
    }

    #[test]
    fn duplicate_ids_keep_first_deterministically() -> Result<(), SkillsError> {
        let temp = TempDir::new("dup")?;
        write_toml_pack(&temp.path().join("aa-dup"), "dup", "from a")?;
        write_toml_pack(&temp.path().join("bb-dup"), "dup", "from b")?;

        let found = discover_in(temp.path())?;
        assert_eq!(found.len(), 1);
        // Deterministic: lowest (id, path) wins → "aa-dup".
        assert_eq!(found[0].instructions, "from a");
        Ok(())
    }

    #[test]
    fn missing_search_path_is_warned_and_skipped() -> Result<(), SkillsError> {
        let temp = TempDir::new("missing-path")?;
        write_toml_pack(&temp.path().join("pack-a"), "a", "alpha")?;
        let manager =
            SkillManager::new(vec![temp.path().join("does-not-exist"), temp.path().to_path_buf()]);
        let found = manager.discover()?;
        assert_eq!(ids(&found), vec!["a"]);
        Ok(())
    }

    #[test]
    fn discover_with_report_surfaces_paths_and_warnings() -> Result<(), SkillsError> {
        let temp = TempDir::new("report")?;
        write_toml_pack(&temp.path().join("pack-a"), "a", "alpha")?;
        // A broken pack, a duplicate id, a missing search path, and a pack
        // whose manifest id is empty (loaded under its directory name).
        write_file(&temp.path().join("broken/skill.toml"), "id = [unclosed\n")?;
        write_toml_pack(&temp.path().join("dup-1"), "dup", "from 1")?;
        write_toml_pack(&temp.path().join("dup-2"), "dup", "from 2")?;
        write_file(&temp.path().join("nameless/skill.toml"), "id = \"   \"\n")?;

        let manager =
            SkillManager::new(vec![temp.path().join("does-not-exist"), temp.path().to_path_buf()]);
        let report = manager.discover_with_report()?;

        // Resolved paths mirror configured order, including the missing one.
        assert_eq!(
            report.resolved_paths,
            vec![temp.path().join("does-not-exist"), temp.path().to_path_buf()]
        );
        assert_eq!(ids(&report.descriptors), vec!["a", "dup", "nameless"]);
        assert!(
            report.warnings.iter().any(|w| w.contains("does-not-exist") && w.contains("missing")),
            "missing search path must be reported: {:?}",
            report.warnings
        );
        assert!(
            report.warnings.iter().any(|w| w.contains("failed to load")),
            "broken pack must be reported: {:?}",
            report.warnings
        );
        assert!(
            report.warnings.iter().any(|w| w.contains("duplicate skill id")),
            "duplicate ids must be reported: {:?}",
            report.warnings
        );
        assert!(
            report.notes.iter().any(|n| n == "Skill 'nameless' loaded via directory name"),
            "directory-name fallback must be reported as a note: {:?}",
            report.notes
        );
        Ok(())
    }

    #[test]
    fn expanded_search_path_is_public_expansion() {
        // Plain paths pass through unchanged.
        assert_eq!(
            expanded_search_path(Path::new("plain/path")),
            Some(PathBuf::from("plain/path"))
        );
        // `%VAR%` references resolve; a distinctive name so parallel tests
        // never observe it.
        std::env::set_var("CONCERTO_SKILLS_TEST_PROFILE", "/home/alice");
        assert_eq!(
            expanded_search_path(Path::new("%CONCERTO_SKILLS_TEST_PROFILE%/skills")),
            Some(PathBuf::from("/home/alice/skills"))
        );
        std::env::remove_var("CONCERTO_SKILLS_TEST_PROFILE");
        // A leading `~` resolves against the real home directory when known.
        match dirs::home_dir() {
            Some(home) => {
                assert_eq!(expanded_search_path(Path::new("~/skills")), Some(home.join("skills")))
            }
            None => assert_eq!(expanded_search_path(Path::new("~/skills")), None),
        }
    }

    #[test]
    fn resolve_enabled_none_returns_all() {
        let all = vec![descriptor("zeta"), descriptor("alpha"), descriptor("mid")];
        let manager = SkillManager::new(vec![]);
        let resolved = manager.resolve_enabled(&all, None);
        let id_list: Vec<&str> = resolved.iter().map(|d| d.id.as_str()).collect();
        assert_eq!(id_list, vec!["zeta", "alpha", "mid"]);
    }

    #[test]
    fn resolve_enabled_preserves_caller_order_and_skips_unknown() {
        let all = vec![descriptor("zeta"), descriptor("alpha"), descriptor("mid")];
        let manager = SkillManager::new(vec![]);
        let enabled = vec!["mid".to_string(), "alpha".to_string(), "nope".to_string()];
        let resolved = manager.resolve_enabled(&all, Some(&enabled));
        let id_list: Vec<&str> = resolved.iter().map(|d| d.id.as_str()).collect();
        assert_eq!(id_list, vec!["mid", "alpha"]);
    }

    #[test]
    fn resolve_enabled_trims_ids_and_skips_empty() {
        let all = vec![descriptor("zeta"), descriptor("alpha"), descriptor("mid")];
        let manager = SkillManager::new(vec![]);
        let enabled =
            vec!["  mid ".to_string(), " alpha".to_string(), "\t".to_string(), "nope".to_string()];
        let resolved = manager.resolve_enabled(&all, Some(&enabled));
        let id_list: Vec<&str> = resolved.iter().map(|d| d.id.as_str()).collect();
        assert_eq!(id_list, vec!["mid", "alpha"]);
    }

    #[test]
    fn invalid_id_error_includes_pack_directory() -> Result<(), SkillsError> {
        let temp = TempDir::new("bad-id")?;
        // Path separator / colon / backslash ids in `skill.toml` (strict).
        let slash_dir = temp.path().join("toml-slash");
        write_file(&slash_dir.join("skill.toml"), "id = \"a/b\"\n")?;
        let colon_dir = temp.path().join("toml-colon");
        write_file(&colon_dir.join("skill.toml"), "id = \"a:b\"\n")?;
        let backslash_dir = temp.path().join("toml-backslash");
        write_file(&backslash_dir.join("skill.toml"), "id = \"a\\\\b\"\n")?;
        // A separator id through `SKILL.md` front matter.
        let md_dir = temp.path().join("md-slash");
        write_file(&md_dir.join("SKILL.md"), "---\nid: a/b\n---\nbody\n")?;

        let manager = SkillManager::new(vec![]);
        for pack_dir in [slash_dir, colon_dir, backslash_dir, md_dir] {
            let err = match manager.load_pack(&pack_dir, &mut Vec::new()) {
                Err(e) => e,
                Ok(_) => panic!("expected an InvalidId error for `{}`", pack_dir.display()),
            };
            assert!(
                matches!(&err, SkillsError::InvalidId { path, .. } if path == &pack_dir),
                "unexpected error for `{}`: {err}",
                pack_dir.display()
            );
        }
        Ok(())
    }

    #[test]
    fn empty_or_whitespace_ids_fall_back_to_directory_name() -> Result<(), SkillsError> {
        let temp = TempDir::new("fallback-id")?;
        // Whitespace id in `skill.toml`.
        let toml_dir = temp.path().join("toml-pack");
        write_file(&toml_dir.join("skill.toml"), "id = \"   \"\n")?;
        // Whitespace id in `SKILL.md` front matter.
        let md_dir = temp.path().join("md-pack");
        write_file(&md_dir.join("SKILL.md"), "---\nid:  \n---\nbody\n")?;
        // Missing id in `SKILL.md` front matter.
        let md_missing = temp.path().join("md-missing");
        write_file(&md_missing.join("SKILL.md"), "---\nname: no id\n---\nbody\n")?;
        // Explicit empty quoted id in `SKILL.md` front matter.
        let md_empty = temp.path().join("md-empty");
        write_file(&md_empty.join("SKILL.md"), "---\nid: \"\"\n---\nbody\n")?;

        let manager = SkillManager::new(vec![]);
        for pack_dir in [&toml_dir, &md_dir, &md_missing, &md_empty] {
            let mut notes = Vec::new();
            let Some(descriptor) = manager.load_pack(pack_dir, &mut notes)? else {
                panic!("pack at `{}` must load via directory-name fallback", pack_dir.display());
            };
            let name = pack_dir.file_name().expect("pack dir has a name").to_string_lossy();
            assert_eq!(descriptor.id, name, "id must fall back to the directory name");
            assert_eq!(notes, vec![format!("Skill '{name}' loaded via directory name")]);
        }
        Ok(())
    }

    #[test]
    fn empty_id_packs_load_via_directory_name_not_skipped() -> Result<(), SkillsError> {
        let temp = TempDir::new("empty-id-continue")?;
        write_file(&temp.path().join("toml-pack/skill.toml"), "id = \"   \"\n")?;
        write_file(&temp.path().join("md-pack/SKILL.md"), "---\nid:\n---\nbody\n")?;
        write_toml_pack(&temp.path().join("good"), "good", "ok")?;

        let manager = SkillManager::new(vec![temp.path().to_path_buf()]);
        let report = manager.discover_with_report()?;
        // Empty/whitespace manifest ids load under their pack directory names;
        // discovery never skips a pack for an empty id (it only notes it).
        assert_eq!(ids(&report.descriptors), vec!["good", "md-pack", "toml-pack"]);
        assert_eq!(
            report.notes,
            vec![
                "Skill 'md-pack' loaded via directory name".to_string(),
                "Skill 'toml-pack' loaded via directory name".to_string(),
            ]
        );
        Ok(())
    }

    #[test]
    fn discover_loads_bom_and_crlf_md_packs() -> Result<(), SkillsError> {
        let temp = TempDir::new("md-bom-crlf")?;
        // Windows-style `SKILL.md`: UTF-8 BOM + CRLF line endings.
        let dir = temp.path().join("pack");
        write_file(
            &dir.join("SKILL.md"),
            "\u{FEFF}---\r\nid: bom-crlf\r\nname: Bom Crlf\r\n---\r\nbody text\r\n",
        )?;

        let found = discover_in(temp.path())?;
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, "bom-crlf");
        assert_eq!(found[0].instructions, "body text");
        Ok(())
    }

    #[test]
    fn discovered_descriptor_round_trips_through_json() -> Result<(), SkillsError> {
        let temp = TempDir::new("json")?;
        write_toml_pack(&temp.path().join("pack-a"), "json-pack", "round trip")?;
        let found = discover_in(temp.path())?;

        let json = serde_json::to_string(&found[0]).map_err(|e| SkillsError::ManifestParse {
            path: PathBuf::from("<serialize>"),
            detail: e.to_string(),
        })?;
        let back: SkillDescriptor =
            serde_json::from_str(&json).map_err(|e| SkillsError::ManifestParse {
                path: PathBuf::from("<deserialize>"),
                detail: e.to_string(),
            })?;
        assert_eq!(back, found[0]);
        Ok(())
    }

    fn descriptor(id: &str) -> SkillDescriptor {
        SkillDescriptor {
            id: id.to_string(),
            manifest: SkillManifest {
                id: id.to_string(),
                name: String::new(),
                version: String::new(),
                description: String::new(),
                instructions_path: None,
                instructions: None,
                tools: Vec::new(),
                resources: Vec::new(),
            },
            instructions: String::new(),
            pack_dir: PathBuf::new(),
            resource_paths: Vec::new(),
        }
    }
}
