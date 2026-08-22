//! Project-index exclusion rules.
//!
//! Background memory indexing treats project files as a data-egress boundary:
//! version-control/build directories and credential-shaped files are always
//! excluded, while `.gitignore`, `.concertoignore`, an optional configured
//! ignore file, and explicit patterns provide project-specific exclusions.

use std::path::{Component, Path, PathBuf};

use camino::Utf8PathBuf;
use concerto_core::error::MemoryError;
use glob::{MatchOptions, Pattern};

use crate::indexer::IndexConfig;

const SENSITIVE_DIRECTORIES: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    ".ssh",
    ".gnupg",
    ".aws",
    ".azure",
    ".docker",
    ".kube",
    ".pulumi",
    ".terraform",
];
const SENSITIVE_FILE_NAMES: &[&str] = &[
    ".credentials",
    ".git-credentials",
    ".netrc",
    ".npmrc",
    ".pypirc",
    "credentials",
    "credentials.json",
    "credentials.toml",
    "credentials.yaml",
    "credentials.yml",
    ".secret",
    ".secrets",
    "secrets.json",
    "secrets.toml",
    "secrets.yaml",
    "secrets.yml",
    "id_dsa",
    "id_ecdsa",
    "id_ed25519",
    "id_rsa",
];
const SENSITIVE_EXTENSIONS: &[&str] =
    &["asc", "gpg", "jks", "kdbx", "key", "keystore", "p12", "pem", "pfx"];

#[derive(Debug)]
struct IgnoreRule {
    pattern: Pattern,
    negated: bool,
    directory_only: bool,
    component_only: bool,
    anchored: bool,
    base: String,
}

impl IgnoreRule {
    fn parse(line: &str, base: &str) -> Result<Option<Self>, glob::PatternError> {
        let mut value = line.trim();
        if value.is_empty() || value.starts_with('#') {
            return Ok(None);
        }
        if value.starts_with("\\#") {
            let unescaped = value.strip_prefix('\\').unwrap_or(value);
            value = unescaped;
        }

        let negated = value.starts_with('!');
        if negated {
            value = value.strip_prefix('!').unwrap_or(value);
        } else if value.starts_with("\\!") {
            let unescaped = value.strip_prefix('\\').unwrap_or(value);
            value = unescaped;
        }

        let anchored = value.starts_with('/');
        value = value.trim_start_matches('/');
        let directory_only = value.ends_with('/');
        value = value.trim_end_matches('/');
        if value.is_empty() {
            return Ok(None);
        }

        let component_only = !value.contains('/');
        let pattern = Pattern::new(value)?;
        Ok(Some(Self {
            pattern,
            negated,
            directory_only,
            component_only,
            anchored,
            base: base.to_string(),
        }))
    }

    fn matches(&self, relative: &str, is_dir: bool) -> bool {
        let scoped = if self.base.is_empty() {
            relative
        } else {
            let prefix = format!("{}/", self.base);
            match relative.strip_prefix(&prefix) {
                Some(scoped) => scoped,
                None => return false,
            }
        };
        let options = MatchOptions {
            case_sensitive: !cfg!(windows),
            require_literal_separator: true,
            require_literal_leading_dot: false,
        };
        let components: Vec<&str> = scoped.split('/').filter(|part| !part.is_empty()).collect();
        if components.is_empty() {
            return false;
        }

        if self.component_only {
            let last = if self.directory_only && !is_dir {
                components.len().saturating_sub(1)
            } else {
                components.len()
            };
            let candidates = &components[..last];
            if self.anchored {
                return candidates
                    .first()
                    .is_some_and(|component| self.pattern.matches_with(component, options));
            }
            return candidates
                .iter()
                .any(|component| self.pattern.matches_with(component, options));
        }

        let last = if self.directory_only && !is_dir {
            components.len().saturating_sub(1)
        } else {
            components.len()
        };
        (1..=last).any(|len| {
            let candidate = components[..len].join("/");
            self.pattern.matches_with(&candidate, options)
        })
    }
}

/// Compiled exclusion policy for one indexing run.
#[derive(Debug)]
pub(crate) struct IndexIgnoreMatcher {
    root: PathBuf,
    hard_rules: Vec<IgnoreRule>,
    ignore_rules: Vec<IgnoreRule>,
    control_files: Vec<Utf8PathBuf>,
}

impl IndexIgnoreMatcher {
    pub(crate) fn new(config: &IndexConfig) -> Result<Self, MemoryError> {
        let configured_root = config.project_dir.as_std_path();
        let root = std::fs::canonicalize(configured_root)
            .unwrap_or_else(|_| configured_root.to_path_buf());

        let hard_rules = config
            .exclude_patterns
            .iter()
            .map(|line| IgnoreRule::parse(line, ""))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| MemoryError::IndexingFailed {
                path: config.project_dir.clone(),
                reason: format!("invalid configured memory exclude pattern: {error}"),
            })?
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        let mut ignore_rules = Vec::new();
        let gitignore = root.join(".gitignore");
        let concertoignore = root.join(".concertoignore");
        let mut gitignore_paths = discover_gitignore_files(&root, &hard_rules);
        if !gitignore_paths.contains(&gitignore) {
            gitignore_paths.push(gitignore.clone());
        }
        gitignore_paths.sort_by_key(|path| path.components().count());
        let mut control_paths = gitignore_paths.clone();
        control_paths.push(concertoignore.clone());
        for path in gitignore_paths {
            load_optional_rules(&root, &path, false, &mut ignore_rules)?;
        }
        load_optional_rules(&root, &concertoignore, false, &mut ignore_rules)?;

        if let Some(ignore_file) = &config.ignore_file {
            let path = if ignore_file.is_absolute() {
                ignore_file.as_std_path().to_path_buf()
            } else {
                root.join(ignore_file.as_std_path())
            };
            if path != gitignore && path != concertoignore {
                control_paths.push(path.clone());
                load_optional_rules(&root, &path, true, &mut ignore_rules)?;
            }
        }

        let control_files = control_paths
            .into_iter()
            .filter_map(|path| path.strip_prefix(&root).ok().map(Path::to_path_buf))
            .filter_map(|path| Utf8PathBuf::from_path_buf(path).ok())
            .collect();
        Ok(Self { root, hard_rules, ignore_rules, control_files })
    }

    pub(crate) fn relative_path(&self, path: &Path) -> Result<Utf8PathBuf, MemoryError> {
        let absolute = if path.is_absolute() { path.to_path_buf() } else { self.root.join(path) };
        let relative =
            absolute.strip_prefix(&self.root).map_err(|_| MemoryError::IndexingFailed {
                path: Utf8PathBuf::from(path.to_string_lossy().as_ref()),
                reason: "path is outside the configured project root".into(),
            })?;
        if relative.components().any(|component| component == Component::ParentDir) {
            return Err(MemoryError::IndexingFailed {
                path: Utf8PathBuf::from(path.to_string_lossy().as_ref()),
                reason: "path escapes the configured project root".into(),
            });
        }
        Utf8PathBuf::from_path_buf(relative.to_path_buf()).map_err(|path| {
            MemoryError::IndexingFailed {
                path: Utf8PathBuf::from(path.to_string_lossy().as_ref()),
                reason: "path is not valid UTF-8".into(),
            }
        })
    }

    pub(crate) fn is_ignored(&self, path: &Path, is_dir: bool) -> bool {
        let Ok(relative) = self.relative_path(path) else {
            return true;
        };
        let normalized = relative.as_str().replace('\\', "/");
        if self.is_hard_ignored_relative(&normalized, is_dir) {
            return true;
        }

        let mut ignored = false;
        for rule in &self.ignore_rules {
            if rule.matches(&normalized, is_dir) {
                ignored = !rule.negated;
            }
        }
        ignored
    }

    pub(crate) fn is_hard_ignored(&self, path: &Path, is_dir: bool) -> bool {
        let Ok(relative) = self.relative_path(path) else {
            return true;
        };
        self.is_hard_ignored_relative(&relative.as_str().replace('\\', "/"), is_dir)
    }

    fn is_hard_ignored_relative(&self, normalized: &str, is_dir: bool) -> bool {
        if normalized.is_empty() {
            return false;
        }
        self.control_files.iter().any(|path| path.as_str().replace('\\', "/") == normalized)
            || is_sensitive_path(normalized)
            || self.hard_rules.iter().any(|rule| rule.matches(normalized, is_dir))
    }
}

fn load_optional_rules(
    root: &Path,
    path: &Path,
    required: bool,
    rules: &mut Vec<IgnoreRule>,
) -> Result<(), MemoryError> {
    match std::fs::read_to_string(path) {
        Ok(contents) => {
            let base = path
                .parent()
                .and_then(|parent| parent.strip_prefix(root).ok())
                .map(|base| base.to_string_lossy().replace('\\', "/"))
                .unwrap_or_default();
            for (line_number, line) in contents.lines().enumerate() {
                match IgnoreRule::parse(line, &base) {
                    Ok(Some(rule)) => rules.push(rule),
                    Ok(None) => {}
                    Err(error) => {
                        return Err(MemoryError::IndexingFailed {
                            path: Utf8PathBuf::from(path.to_string_lossy().as_ref()),
                            reason: format!(
                                "invalid ignore pattern on line {}: {error}",
                                line_number + 1
                            ),
                        });
                    }
                }
            }
            Ok(())
        }
        Err(error) if !required && error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(MemoryError::IndexingFailed {
            path: Utf8PathBuf::from(path.to_string_lossy().as_ref()),
            reason: format!("failed to read ignore file: {error}"),
        }),
    }
}

fn discover_gitignore_files(root: &Path, hard_rules: &[IgnoreRule]) -> Vec<PathBuf> {
    walkdir::WalkDir::new(root)
        .into_iter()
        .filter_entry(|entry| {
            let Ok(relative) = entry.path().strip_prefix(root) else {
                return false;
            };
            let normalized = relative.to_string_lossy().replace('\\', "/");
            normalized.is_empty()
                || (!is_sensitive_path(&normalized)
                    && !hard_rules
                        .iter()
                        .any(|rule| rule.matches(&normalized, entry.file_type().is_dir())))
        })
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file() && entry.file_name() == ".gitignore")
        .map(|entry| entry.into_path())
        .collect()
}

fn is_sensitive_path(relative: &str) -> bool {
    let components: Vec<String> =
        relative.split('/').map(|component| component.to_ascii_lowercase()).collect();
    if components.iter().any(|component| SENSITIVE_DIRECTORIES.contains(&component.as_str())) {
        return true;
    }

    let Some(file_name) = components.last() else {
        return false;
    };
    if file_name.starts_with(".env")
        || file_name.starts_with(".secrets.")
        || SENSITIVE_FILE_NAMES.contains(&file_name.as_str())
    {
        return true;
    }

    Path::new(file_name)
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| SENSITIVE_EXTENSIONS.contains(&extension))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn config_for(root: &Path) -> IndexConfig {
        IndexConfig {
            project_dir: Utf8PathBuf::from_path_buf(root.to_path_buf()).unwrap(),
            ..IndexConfig::default()
        }
    }

    #[test]
    fn excludes_sensitive_files_even_when_not_gitignored() {
        let root = tempdir().unwrap();
        let matcher = IndexIgnoreMatcher::new(&config_for(root.path())).unwrap();

        assert!(matcher.is_ignored(&root.path().join(".env"), false));
        assert!(matcher.is_ignored(&root.path().join(".env.local"), false));
        assert!(matcher.is_ignored(&root.path().join("keys/server.pem"), false));
        assert!(matcher.is_ignored(&root.path().join(".ssh/config"), false));
        assert!(!matcher.is_ignored(&root.path().join("src/key.rs"), false));
    }

    #[test]
    fn concertoignore_can_refine_gitignore_without_reincluding_secrets() {
        let root = tempdir().unwrap();
        std::fs::write(root.path().join(".gitignore"), "generated/\n*.log\n").unwrap();
        std::fs::write(root.path().join(".concertoignore"), "!keep.log\n.env\n!.env\n").unwrap();
        let matcher = IndexIgnoreMatcher::new(&config_for(root.path())).unwrap();

        assert!(matcher.is_ignored(&root.path().join("generated/code.rs"), false));
        assert!(matcher.is_ignored(&root.path().join("debug.log"), false));
        assert!(!matcher.is_ignored(&root.path().join("keep.log"), false));
        assert!(matcher.is_ignored(&root.path().join(".env"), false));
    }

    #[test]
    fn configured_patterns_and_ignore_file_are_honoured() {
        let root = tempdir().unwrap();
        std::fs::write(root.path().join("memory.ignore"), "private-notes/\n").unwrap();
        let mut config = config_for(root.path());
        config.exclude_patterns.push("snapshots/".into());
        config.ignore_file = Some("memory.ignore".into());
        let matcher = IndexIgnoreMatcher::new(&config).unwrap();

        assert!(matcher.is_ignored(&root.path().join("snapshots/state.json"), false));
        assert!(matcher.is_ignored(&root.path().join("private-notes/plan.md"), false));
        assert!(matcher.is_ignored(&root.path().join("memory.ignore"), false));
        assert!(!matcher.is_ignored(&root.path().join("src/lib.rs"), false));
    }

    #[test]
    fn nested_gitignore_rules_are_scoped_to_their_directory() {
        let root = tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("crates/one")).unwrap();
        std::fs::create_dir_all(root.path().join("crates/two")).unwrap();
        std::fs::write(root.path().join("crates/one/.gitignore"), "*.generated.rs\n").unwrap();
        let matcher = IndexIgnoreMatcher::new(&config_for(root.path())).unwrap();

        assert!(matcher.is_ignored(&root.path().join("crates/one/model.generated.rs"), false));
        assert!(!matcher.is_ignored(&root.path().join("crates/two/model.generated.rs"), false));
        assert!(matcher.is_ignored(&root.path().join("crates/one/.gitignore"), false));
    }

    #[test]
    fn paths_outside_project_are_rejected() {
        let root = tempdir().unwrap();
        let other = tempdir().unwrap();
        let matcher = IndexIgnoreMatcher::new(&config_for(root.path())).unwrap();

        assert!(matcher.relative_path(&other.path().join("outside.rs")).is_err());
        assert!(matcher.is_ignored(&other.path().join("outside.rs"), false));
    }
}
