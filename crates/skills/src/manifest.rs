//! `skill.toml` manifest parsing.
//!
//! `skill.toml` uses the same field names as `SKILL.md` front matter and maps
//! onto the shared `concerto_api_types::SkillManifest` data model. Unknown
//! fields are ignored (serde's default behavior); missing optional fields fall
//! back to defaults (`""`, empty vec, `None`).

use crate::error::SkillsError;
use concerto_api_types::extension::SkillManifest;
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// Serde shape of `skill.toml`. Only `id` is required; `Option` fields default
/// to `None` when absent.
#[derive(Debug, Deserialize)]
pub(crate) struct RawSkillToml {
    pub id: String,
    pub name: Option<String>,
    pub version: Option<String>,
    pub description: Option<String>,
    pub instructions: Option<String>,
    pub instructions_path: Option<String>,
    pub tools: Option<Vec<String>>,
    pub resources: Option<Vec<String>>,
}

impl RawSkillToml {
    /// Convert into the shared `SkillManifest`, applying defaults. The id is
    /// trimmed but deliberately NOT validated here: id validation happens at
    /// the loader (`manager::load_toml_pack`), where the pack directory is
    /// known, so `SkillsError::InvalidId` can carry the pack path.
    fn into_manifest(self) -> SkillManifest {
        SkillManifest {
            id: self.id.trim().to_string(),
            name: self.name.unwrap_or_default(),
            version: self.version.unwrap_or_default(),
            description: self.description.unwrap_or_default(),
            instructions_path: self.instructions_path.map(PathBuf::from),
            instructions: self.instructions,
            tools: self.tools.unwrap_or_default(),
            resources: self.resources.unwrap_or_default().into_iter().map(PathBuf::from).collect(),
        }
    }
}

/// Parse the text of a `skill.toml` file into a `SkillManifest`.
///
/// Only TOML syntax errors are reported here; id validation (empty or
/// whitespace-only) is the loader's responsibility because `InvalidId` needs
/// the pack directory for diagnostics.
pub(crate) fn parse_skill_toml(text: &str, path: &Path) -> Result<SkillManifest, SkillsError> {
    let raw: RawSkillToml = toml::from_str(text).map_err(|e| SkillsError::ManifestParse {
        path: path.to_path_buf(),
        detail: e.to_string(),
    })?;
    Ok(raw.into_manifest())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_manifest_parses() {
        let manifest = parse_skill_toml(
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
            Path::new("skill.toml"),
        )
        .expect("parse should succeed");
        assert_eq!(manifest.id, "rust-style");
        assert_eq!(manifest.name, "Rust Style");
        assert_eq!(manifest.version, "2.1.0");
        assert_eq!(manifest.instructions.as_deref(), Some("Prefer cargo nextest."));
        assert_eq!(manifest.tools, vec!["cargo nextest run", "cargo clippy"]);
        assert_eq!(manifest.resources, vec![PathBuf::from("templates/style.md")]);
        assert!(manifest.instructions_path.is_none());
    }

    #[test]
    fn missing_optionals_default() {
        let manifest = parse_skill_toml("id = \"minimal\"\n", Path::new("skill.toml"))
            .expect("parse should succeed");
        assert_eq!(manifest.id, "minimal");
        assert_eq!(manifest.name, "");
        assert_eq!(manifest.version, "");
        assert_eq!(manifest.description, "");
        assert!(manifest.instructions.is_none());
        assert!(manifest.instructions_path.is_none());
        assert!(manifest.tools.is_empty());
        assert!(manifest.resources.is_empty());
    }

    #[test]
    fn instructions_path_parses() {
        let manifest = parse_skill_toml(
            "id = \"x\"\ninstructions_path = \"instructions.md\"\n",
            Path::new("skill.toml"),
        )
        .expect("parse should succeed");
        assert_eq!(manifest.instructions_path, Some(PathBuf::from("instructions.md")));
    }

    #[test]
    fn missing_id_is_manifest_error() {
        let err = parse_skill_toml("name = \"No Id\"\n", Path::new("skill.toml"))
            .expect_err("should error");
        assert!(matches!(err, SkillsError::ManifestParse { .. }));
    }

    #[test]
    fn whitespace_id_is_trimmed_at_parse_level() {
        // Id validation (which needs the pack directory for `InvalidId`) lives
        // in the loader; the parser only normalizes the id.
        let manifest = parse_skill_toml("id = \"   \"\n", Path::new("skill.toml"))
            .expect("parse should succeed");
        assert_eq!(manifest.id, "");
    }

    #[test]
    fn malformed_toml_is_manifest_error() {
        let err = parse_skill_toml("id = [unclosed\n", Path::new("skill.toml"))
            .expect_err("should error");
        assert!(matches!(err, SkillsError::ManifestParse { .. }));
    }
}
