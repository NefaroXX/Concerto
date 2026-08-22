//! `SkillManager` — skill-pack discovery and loading.
//!
//! A skill pack is a directory containing a `skill.toml` manifest or a
//! `SKILL.md` file with YAML-subset front matter. When both are present,
//! `skill.toml` wins. Discovery walks each search path to a bounded depth,
//! parses every pack it finds, resolves all paths to absolute form, and
//! returns descriptors deterministically sorted by id (duplicate ids keep the
//! first occurrence).

use crate::error::SkillsError;
use crate::frontmatter::parse_front_matter;
use crate::manifest::parse_skill_toml;
use concerto_api_types::extension::{SkillDescriptor, SkillManifest};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use tracing::{debug, warn};

/// Maximum nesting depth of a skill pack relative to a search path root.
/// The root itself is depth 0; packs deeper than this are not discovered.
const MAX_SKILL_DEPTH: usize = 4;

/// Discovers and loads local skill packs (ADR-43, decision 1).
///
/// `SkillManager` holds no configuration state: search paths and enabled ids
/// are passed in as parameters so the crate does not depend on `concerto-config`.
#[derive(Debug, Clone)]
pub struct SkillManager {
    search_paths: Vec<PathBuf>,
}

impl SkillManager {
    /// Create a manager for the given search paths. `~` is expanded lazily at
    /// discovery time via `dirs::home_dir`.
    pub fn new(search_paths: Vec<PathBuf>) -> Self {
        Self { search_paths }
    }

    /// Discover every skill pack under the configured search paths.
    ///
    /// - Missing search paths are warned about and skipped (not an error).
    /// - Directories without a manifest are silently not skills.
    /// - Malformed manifests fail loudly with the offending path.
    /// - Results are sorted by id; duplicate ids keep the first occurrence
    ///   (deterministic: lowest `(id, path)` wins) and are warned about.
    pub fn discover(&self) -> Result<Vec<SkillDescriptor>, SkillsError> {
        let mut found: Vec<(PathBuf, SkillDescriptor)> = Vec::new();
        for raw in &self.search_paths {
            let Some(root) = expand_home(raw) else {
                warn!(
                    path = %raw.display(),
                    "skill search path uses `~` but no home directory is known; skipping"
                );
                continue;
            };
            if !root.is_dir() {
                warn!(
                    path = %root.display(),
                    "skill search path missing or not a directory; skipping"
                );
                continue;
            }
            self.walk(&root, 0, &mut found)?;
        }

        // Deterministic dedup: sort by (id, path), keep the first of any id.
        found.sort_by(|(path_a, desc_a), (path_b, desc_b)| {
            desc_a.id.cmp(&desc_b.id).then_with(|| path_a.cmp(path_b))
        });
        let mut seen: HashSet<String> = HashSet::new();
        let mut discovered = Vec::with_capacity(found.len());
        for (_, descriptor) in found {
            if !seen.insert(descriptor.id.clone()) {
                warn!(id = %descriptor.id, "duplicate skill id; keeping first occurrence");
                continue;
            }
            discovered.push(descriptor);
        }
        Ok(discovered)
    }

    /// Filter discovered skills to those explicitly enabled.
    ///
    /// `None` returns every skill; `Some(ids)` returns only the matching
    /// skills, in caller order, with unknown ids warned about and skipped.
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
            match by_id.get(id.as_str()) {
                Some(descriptor) => resolved.push(*descriptor),
                None => {
                    warn!(id = %id, "enabled skill id not found among discovered skills; skipping")
                }
            }
        }
        resolved
    }

    /// Recursively walk `dir` to `MAX_SKILL_DEPTH`, collecting skill packs.
    /// A directory that is itself a pack is a leaf: it is not descended into.
    fn walk(
        &self,
        dir: &Path,
        depth: usize,
        out: &mut Vec<(PathBuf, SkillDescriptor)>,
    ) -> Result<(), SkillsError> {
        if depth > MAX_SKILL_DEPTH {
            return Ok(());
        }
        if let Some(pack) = self.load_pack(dir)? {
            out.push((dir.to_path_buf(), pack));
            return Ok(());
        }
        if depth < MAX_SKILL_DEPTH {
            let entries = fs::read_dir(dir)
                .map_err(|e| SkillsError::Io { path: dir.to_path_buf(), source: e })?;
            let mut children = Vec::new();
            for entry in entries {
                let entry =
                    entry.map_err(|e| SkillsError::Io { path: dir.to_path_buf(), source: e })?;
                let file_type = entry
                    .file_type()
                    .map_err(|e| SkillsError::Io { path: entry.path(), source: e })?;
                if file_type.is_dir() {
                    children.push(entry.path());
                }
            }
            children.sort();
            for child in children {
                self.walk(&child, depth + 1, out)?;
            }
        }
        Ok(())
    }

    /// Load the pack at `dir`, if it has a manifest. `None` means the
    /// directory is not a skill pack. `skill.toml` wins over `SKILL.md` when
    /// both are present.
    fn load_pack(&self, dir: &Path) -> Result<Option<SkillDescriptor>, SkillsError> {
        let toml_path = dir.join("skill.toml");
        let md_path = dir.join("SKILL.md");
        if toml_path.exists() {
            if md_path.exists() {
                debug!(
                    dir = %dir.display(),
                    "both skill.toml and SKILL.md present; skill.toml wins"
                );
            }
            return self.load_toml_pack(dir, &toml_path).map(Some);
        }
        if md_path.exists() {
            return self.load_md_pack(dir, &md_path).map(Some);
        }
        Ok(None)
    }

    /// Load a `skill.toml`-based pack, resolving `instructions_path`,
    /// resources, and the instruction text.
    fn load_toml_pack(
        &self,
        dir: &Path,
        manifest_path: &Path,
    ) -> Result<SkillDescriptor, SkillsError> {
        let text = fs::read_to_string(manifest_path)
            .map_err(|e| SkillsError::Io { path: manifest_path.to_path_buf(), source: e })?;
        let mut manifest = parse_skill_toml(&text, manifest_path)?;

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

        Ok(SkillDescriptor { id: manifest.id.clone(), manifest, instructions, resource_paths })
    }

    /// Load a `SKILL.md`-based pack. The markdown body below the closing `---`
    /// delimiter is the instruction text; a front-matter `instructions` key is
    /// used only when the body is empty.
    fn load_md_pack(
        &self,
        dir: &Path,
        manifest_path: &Path,
    ) -> Result<SkillDescriptor, SkillsError> {
        let text = fs::read_to_string(manifest_path)
            .map_err(|e| SkillsError::Io { path: manifest_path.to_path_buf(), source: e })?;
        let fm = parse_front_matter(&text).map_err(|detail| SkillsError::FrontMatter {
            path: manifest_path.to_path_buf(),
            detail,
        })?;

        let raw_id = fm.id.unwrap_or_default();
        let id = raw_id.trim().to_string();
        if id.is_empty() {
            return Err(SkillsError::InvalidId { id: raw_id });
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

        Ok(SkillDescriptor { id, manifest, instructions, resource_paths })
    }
}

/// Expand a leading `~` to the user's home directory. Paths without a `~`
/// are returned unchanged; `None` means a `~` path could not be expanded
/// because no home directory is known.
fn expand_home(path: &Path) -> Option<PathBuf> {
    let text = path.to_string_lossy();
    if text == "~" {
        return dirs::home_dir();
    }
    if let Some(rest) = text.strip_prefix("~/") {
        let home = dirs::home_dir()?;
        return Some(home.join(rest));
    }
    Some(path.to_path_buf())
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
    fn malformed_front_matter_errors_with_path() -> Result<(), SkillsError> {
        let temp = TempDir::new("md-bad")?;
        // Missing closing delimiter.
        let dir = temp.path().join("pack-a");
        write_file(&dir.join("SKILL.md"), "---\nid: broken\n")?;
        // Missing opening delimiter.
        let dir_b = temp.path().join("pack-b");
        write_file(&dir_b.join("SKILL.md"), "# just a doc\n\nno front matter\n")?;

        let manager = SkillManager::new(vec![temp.path().to_path_buf()]);
        let err = match manager.discover() {
            Ok(_) => panic!("expected a front-matter error"),
            Err(e) => e,
        };
        assert!(
            matches!(
                &err,
                SkillsError::FrontMatter { path, detail }
                    if path == &dir.join("SKILL.md") && detail.contains("closing `---`")
            ),
            "unexpected error: {err}"
        );

        // Clear pack-a so the next discover reaches pack-b and surfaces its
        // distinct error (missing opening delimiter).
        fs::remove_dir_all(&dir).map_err(|e| SkillsError::Io { path: dir.clone(), source: e })?;
        let err = match manager.discover() {
            Ok(_) => panic!("expected a front-matter error"),
            Err(e) => e,
        };
        assert!(
            matches!(
                &err,
                SkillsError::FrontMatter { path, detail }
                    if path == &dir_b.join("SKILL.md") && detail.contains("opening `---`")
            ),
            "unexpected error: {err}"
        );
        Ok(())
    }

    #[test]
    fn malformed_skill_toml_errors_with_path() -> Result<(), SkillsError> {
        let temp = TempDir::new("toml-bad")?;
        let dir = temp.path().join("pack");
        write_file(&dir.join("skill.toml"), "id = [unclosed\n")?;

        let manager = SkillManager::new(vec![temp.path().to_path_buf()]);
        let err = match manager.discover() {
            Ok(_) => panic!("expected a manifest error"),
            Err(e) => e,
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
    fn empty_or_whitespace_id_is_error() -> Result<(), SkillsError> {
        let temp = TempDir::new("bad-id")?;
        // Whitespace id in skill.toml.
        let toml_dir = temp.path().join("toml-pack");
        write_file(&toml_dir.join("skill.toml"), "id = \"   \"\n")?;
        // Whitespace id in SKILL.md front matter.
        let md_dir = temp.path().join("md-pack");
        write_file(&md_dir.join("SKILL.md"), "---\nid:  \n---\nbody\n")?;
        // Missing id in SKILL.md front matter.
        let md_missing = temp.path().join("md-missing");
        write_file(&md_missing.join("SKILL.md"), "---\nname: no id\n---\nbody\n")?;

        let manager = SkillManager::new(vec![temp.path().to_path_buf()]);
        for (pack_dir, expected_path) in [
            (toml_dir.clone(), toml_dir.join("skill.toml")),
            (md_dir.clone(), md_dir.join("SKILL.md")),
            (md_missing.clone(), md_missing.join("SKILL.md")),
        ] {
            let err = match manager.discover() {
                Ok(_) => panic!("expected an InvalidId error for `{expected_path:?}`"),
                Err(e) => e,
            };
            assert!(
                matches!(&err, SkillsError::InvalidId { id } if id.trim().is_empty()),
                "unexpected error for `{expected_path:?}`: {err}"
            );
            // Remove the offending pack so the next iteration reaches the next one.
            fs::remove_dir_all(&pack_dir)
                .map_err(|e| SkillsError::Io { path: pack_dir.clone(), source: e })?;
        }
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
            resource_paths: Vec::new(),
        }
    }
}
