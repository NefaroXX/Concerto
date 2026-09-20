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
    /// A skill id is invalid: empty/whitespace-only, not a single path
    /// component (`.`/`..`), or containing a path separator or NUL after
    /// trimming.
    #[error(
        "invalid skill id `{id}` at `{path}`: must be a non-empty single path component after trimming"
    )]
    InvalidId {
        /// The offending id as written in the manifest (or trimmed to empty).
        id: String,
        /// Directory of the skill pack the manifest belongs to.
        path: PathBuf,
    },
    /// The YAML-subset front matter of a `SKILL.md` file is malformed.
    #[error("failed to parse YAML front matter in `{path}`: {detail}")]
    FrontMatter {
        /// Path to the `SKILL.md` file.
        path: PathBuf,
        /// Human-readable parse detail.
        detail: String,
    },
    /// A skill pack directory already exists at the target of a create.
    #[error("skill pack already exists at `{path}`")]
    AlreadyExists {
        /// Path of the pack directory that already exists.
        path: PathBuf,
    },
    /// A directory expected to be a skill pack has neither `skill.toml` nor
    /// `SKILL.md` (or does not exist).
    #[error("no skill manifest (`skill.toml` or `SKILL.md`) found in `{path}`")]
    NotAPack {
        /// Path of the directory expected to be a skill pack.
        path: PathBuf,
    },
    /// The skill pack uses a format that the requested CRUD operation cannot
    /// safely modify or create.
    #[error("skill pack at `{path}` cannot be modified this way: {detail}")]
    UnsupportedFormat {
        /// Path of the skill pack directory.
        path: PathBuf,
        /// Human-readable reason.
        detail: String,
    },
}
