//! Error taxonomy for `concerto-skills`.

use std::path::PathBuf;

/// Errors produced by skill-pack discovery and loading.
#[derive(Debug, thiserror::Error)]
pub enum SkillsError {
    /// An I/O operation failed while accessing a path.
    #[error("I/O error while accessing `{path}`: {source}")]
    Io {
        /// The path being accessed when the error occurred.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// A `skill.toml` manifest could not be parsed or validated.
    #[error("failed to parse skill manifest at `{path}`: {detail}")]
    ManifestParse {
        /// Path to the manifest file.
        path: PathBuf,
        /// Human-readable parse/validation detail.
        detail: String,
    },
    /// A skill id is empty or whitespace-only.
    #[error("invalid skill id `{id}`: must be a non-empty string after trimming")]
    InvalidId {
        /// The offending id as written in the manifest.
        id: String,
    },
    /// The YAML-subset front matter of a `SKILL.md` file is malformed.
    #[error("failed to parse YAML front matter in `{path}`: {detail}")]
    FrontMatter {
        /// Path to the `SKILL.md` file.
        path: PathBuf,
        /// Human-readable parse detail.
        detail: String,
    },
}
