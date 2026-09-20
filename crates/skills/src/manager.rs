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
    /// logs every warning at `warn` level and informational notes at `info`
    /// level and discards them. Callers that need the diagnostics as data
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

/// Validate a *new* skill id for create/rename operations: non-empty after
/// trimming, a single path component (no `/`, `\`, `:`, or NUL), and not
/// `.`/`..`. The checks mirror [`validate_skill_id`]'s strict rules so a pack
/// created here can never be rejected by discovery later (the loader is
/// lenient about empty ids, but create with an empty id is meaningless). The
/// trimmed value is returned.
fn validate_new_skill_id(raw_id: &str, parent: &Path) -> Result<String, SkillsError> {
    let id = raw_id.trim();
    if id.is_empty() || id == "." || id == ".." || id.contains(['/', '\\', ':', '\0']) {
        return Err(SkillsError::InvalidId { id: raw_id.to_string(), path: parent.to_path_buf() });
    }
    Ok(id.to_string())
}

/// Serialize a manifest to TOML for disk writes. Serialization failures map
/// to `ManifestParse` with a synthetic `<serialize>` path, matching the
/// existing round-trip test convention.
fn serialize_manifest(manifest: &SkillManifest) -> Result<String, SkillsError> {
    toml::to_string(manifest).map_err(|e| SkillsError::ManifestParse {
        path: PathBuf::from("<serialize>"),
        detail: e.to_string(),
    })
}

/// Atomically write `contents` to `path`: write to a sibling temp file, then
/// rename over the target. Rename-over-existing is atomic on POSIX; on Windows
/// the destination is removed first (a small non-atomic window, but discovery
/// only reads complete files). The temp file is removed on failure.
fn write_atomic(path: &Path, contents: &str) -> Result<(), SkillsError> {
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
fn relativize_under(base: &Path, path: &Path) -> PathBuf {
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
fn epoch_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

/// Expand a configured search path for display or pre-checks: Windows-style
/// `%NAME%` environment-variable references (e.g. `%USERPROFILE%`, `%APPDATA%`)
/// and a leading `~` are resolved to their absolute form. Paths with none of
/// these tokens are returned unchanged. Returns `None` when a `~` path cannot
/// be expanded because no home directory is known.
///
/// This is the same expansion applied during discovery; it performs no I/O and
/// does not check whether the result exists.
pub fn expanded_search_path(path: &Path) -> Option<PathBuf> {
    expand_home(path)
}

/// Expand a search path: Windows-style `%NAME%` environment-variable
/// references (e.g. `%USERPROFILE%`, `%APPDATA%`), then a leading `~`
/// (`~`, `~/...`, `~\...`). Paths with none of these tokens are returned
/// unchanged; `None` means a `~` path could not be expanded because no home
/// directory is known.
fn expand_home(path: &Path) -> Option<PathBuf> {
    let expanded = expand_env_refs(&path.to_string_lossy());
    resolve_tilde(&expanded, user_home_dir())
}

/// Cross-platform `~` expansion against an explicit home directory. `~` maps
/// to `home` itself; `~/...` and `~\...` join the remainder. Non-tilde paths
/// pass through unchanged.
fn resolve_tilde(path: &str, home: Option<PathBuf>) -> Option<PathBuf> {
    match strip_tilde(path) {
        Some(rest) => Some(home?.join(rest)),
        None => Some(PathBuf::from(path)),
    }
}

/// Strip a leading `~`, `~/`, or `~\` marker, returning the path remainder
/// (empty for a bare `~`). `None` when the path does not start with `~`.
fn strip_tilde(text: &str) -> Option<&str> {
    if text == "~" {
        return Some("");
    }
    text.strip_prefix("~/").or_else(|| text.strip_prefix("~\\"))
}

/// Replace Windows-style `%NAME%` environment-variable references in a path
/// with their values. Tokens whose variable is unset — including a lone or
/// unmatched `%` — are left verbatim so the caller can still report the
/// original path. Returns the path unchanged when it contains no `%`.
fn expand_env_refs(path: &str) -> String {
    if !path.contains('%') {
        return path.to_string();
    }
    let mut result = String::with_capacity(path.len());
    let mut rest = path;
    while let Some(percent) = rest.find('%') {
        result.push_str(&rest[..percent]);
        let after_percent = &rest[percent + 1..];
        let Some(closing) = after_percent.find('%') else {
            // Unmatched `%`: keep the remainder verbatim.
            result.push_str(&rest[percent..]);
            return result;
        };
        let name = &after_percent[..closing];
        match std::env::var(name) {
            Ok(value) => result.push_str(&value),
            Err(_) => {
                result.push('%');
                result.push_str(name);
                result.push('%');
            }
        }
        rest = &after_percent[closing + 1..];
    }
    result.push_str(rest);
    result
}

/// Resolve the user's home directory, with platform-appropriate fallbacks.
///
/// - Windows: `$USERPROFILE`, then a profile path derived from `$APPDATA`
///   (`<profile>\AppData\Roaming`), then `dirs::home_dir()`.
/// - Other platforms: `dirs::home_dir()` (which consults `$HOME` and friends).
fn user_home_dir() -> Option<PathBuf> {
    for (var, derive_profile) in home_env_candidates() {
        if let Some(value) = std::env::var_os(var) {
            let path = PathBuf::from(value);
            if *derive_profile {
                if let Some(home) = profile_from_appdata(&path) {
                    return Some(home);
                }
            } else {
                return Some(path);
            }
        }
    }
    dirs::home_dir()
}

/// `(env var holding the profile, bool: true when it is `%APPDATA%`, whose
/// parent-of-parent is the profile directory)`. The Windows variables are only
/// consulted on Windows; on other platforms the list is empty and
/// `dirs::home_dir()` is authoritative.
fn home_env_candidates() -> &'static [(&'static str, bool)] {
    #[cfg(windows)]
    {
        &[("USERPROFILE", false), ("APPDATA", true)]
    }
    #[cfg(not(windows))]
    {
        &[]
    }
}

/// Derive the profile directory from a Windows `%APPDATA%` path
/// (`<profile>\AppData\Roaming`) by walking up two levels. Returns `None`
/// when the path has fewer than two parents.
fn profile_from_appdata(app_data: &Path) -> Option<PathBuf> {
    app_data.parent().and_then(Path::parent).map(Path::to_path_buf)
}

/// Resolve a path to absolute form relative to `base` (absolute paths pass
/// through unchanged).
fn absolute_path(base: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
}

/// Resolve every path in `paths` to absolute form relative to `base`.
fn resolve_all(base: &Path, paths: &[PathBuf]) -> Vec<PathBuf> {
    paths.iter().map(|p| absolute_path(base, p)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write as _;

    /// Unique temp directory removed on drop. Avoids a `tempfile` dependency
    /// (not in `[workspace.dependencies]`).
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Result<Self, SkillsError> {
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

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn write_file(path: &Path, contents: &str) -> Result<(), SkillsError> {
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
    fn write_toml_pack(dir: &Path, id: &str, instructions: &str) -> Result<(), SkillsError> {
        let toml = format!(
            "id = \"{id}\"\nname = \"Test Skill\"\nversion = \"1.0.0\"\ndescription = \"test\"\ninstructions = \"{instructions}\"\n"
        );
        write_file(&dir.join("skill.toml"), &toml)
    }

    fn discover_in(root: &Path) -> Result<Vec<SkillDescriptor>, SkillsError> {
        let manager = SkillManager::new(vec![root.to_path_buf()]);
        manager.discover()
    }

    fn ids(descriptors: &[SkillDescriptor]) -> Vec<&str> {
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
    fn discovery_expands_tilde_via_home() -> Result<(), SkillsError> {
        let temp = TempDir::new("tilde")?;
        let home = temp.path().join("home");
        write_toml_pack(&home.join("skills/pack-a"), "tilde-pack", "from home")?;

        let previous_home = std::env::var_os("HOME");
        std::env::set_var("HOME", &home);
        let discovered = SkillManager::new(vec![PathBuf::from("~/skills")]).discover();
        match previous_home {
            Some(value) => std::env::set_var("HOME", value),
            None => std::env::remove_var("HOME"),
        }

        let found = discovered?;
        assert_eq!(ids(&found), vec!["tilde-pack"]);
        Ok(())
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
    fn resolve_tilde_expands_tilde_forms_and_leaves_plain_paths() {
        let home = PathBuf::from("/home/alice");
        assert_eq!(resolve_tilde("~", Some(home.clone())), Some(home.clone()));
        assert_eq!(resolve_tilde("~/skills", Some(home.clone())), Some(home.join("skills")));
        // Windows separator form joins the same way.
        assert_eq!(resolve_tilde("~\\skills", Some(home.clone())), Some(home.join("skills")));
        assert_eq!(
            resolve_tilde("plain/skills", Some(home.clone())),
            Some(PathBuf::from("plain/skills"))
        );
        assert_eq!(resolve_tilde("~", None), None);
        assert_eq!(resolve_tilde("~/skills", None), None);
    }

    #[test]
    fn strip_tilde_handles_bare_and_separator_forms() {
        assert_eq!(strip_tilde("~"), Some(""));
        assert_eq!(strip_tilde("~/a"), Some("a"));
        assert_eq!(strip_tilde("~\\a"), Some("a"));
        assert_eq!(strip_tilde("plain"), None);
        assert_eq!(strip_tilde("~~/a"), None);
    }

    #[test]
    fn expand_env_refs_replaces_percent_vars_and_leaves_unset_alone() {
        // A distinctive variable name so concurrent tests never observe it.
        std::env::set_var("CONCERTO_SKILLS_TEST_PROFILE", "/home/alice");
        assert_eq!(expand_env_refs("%CONCERTO_SKILLS_TEST_PROFILE%/skills"), "/home/alice/skills");
        std::env::remove_var("CONCERTO_SKILLS_TEST_PROFILE");

        // Unset variables are left verbatim (callers still warn on the path).
        assert_eq!(
            expand_env_refs("%CONCERTO_SKILLS_TEST_UNSET%/skills"),
            "%CONCERTO_SKILLS_TEST_UNSET%/skills"
        );
        // No `%` at all: unchanged.
        assert_eq!(expand_env_refs("plain/path"), "plain/path");
        // Lone or unmatched `%`: remainder verbatim.
        assert_eq!(expand_env_refs("100%/skills"), "100%/skills");
        assert_eq!(expand_env_refs("a%unclosed"), "a%unclosed");
    }

    #[test]
    fn expand_home_expands_env_refs_in_path() {
        let home = std::env::temp_dir().join("concerto-skills-fake-home");
        std::env::set_var("CONCERTO_SKILLS_TEST_HOME", &home);
        let expanded = expand_home(Path::new("%CONCERTO_SKILLS_TEST_HOME%/skills"));
        std::env::remove_var("CONCERTO_SKILLS_TEST_HOME");
        assert_eq!(expanded, Some(home.join("skills")));
    }

    #[test]
    fn profile_from_appdata_walks_up_two_levels() {
        // Forward slashes parse as separators on both Windows and Unix, so the
        // derivation logic is exercised the same way on every platform.
        let app_data = PathBuf::from("C:/Users/alice/AppData/Roaming");
        assert_eq!(profile_from_appdata(&app_data), Some(PathBuf::from("C:/Users/alice")));
        // A path with fewer than two parents cannot be a Windows profile
        // (a bare component has no two parents on Unix or Windows).
        assert_eq!(profile_from_appdata(Path::new("Roaming")), None);
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
