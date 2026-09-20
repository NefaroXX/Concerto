//! Error taxonomy for `concerto-skills`.

use std::fmt;
use std::path::{Path, PathBuf};

/// Errors produced by skill-pack discovery and loading.
#[derive(Debug, thiserror::Error)]
pub enum SkillsError {
    /// An I/O operation failed while accessing a path.
    Io {
        /// The path being accessed when the error occurred.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// A `skill.toml` manifest could not be parsed or validated.
    ManifestParse {
        /// Path to the manifest file.
        path: PathBuf,
        /// Human-readable parse/validation detail.
        detail: String,
    },
    /// A skill id is invalid: no usable directory name to fall back to when
    /// the manifest id is empty, or the trimmed id contains a path separator,
    /// `:`, or NUL.
    InvalidId {
        /// The offending id as written in the manifest (may be empty).
        id: String,
        /// Directory of the skill pack the manifest belongs to.
        path: PathBuf,
    },
    /// The YAML-subset front matter of a `SKILL.md` file is malformed.
    FrontMatter {
        /// Path to the `SKILL.md` file.
        path: PathBuf,
        /// Human-readable parse detail.
        detail: String,
    },
    /// A skill pack directory already exists at the target of a create.
    AlreadyExists {
        /// Path of the pack directory that already exists.
        path: PathBuf,
    },
    /// A directory expected to be a skill pack has neither `skill.toml` nor
    /// `SKILL.md` (or does not exist).
    NotAPack {
        /// Path of the directory expected to be a skill pack.
        path: PathBuf,
    },
    /// The skill pack uses a format that the requested CRUD operation cannot
    /// safely modify or create.
    UnsupportedFormat {
        /// Path of the skill pack directory.
        path: PathBuf,
        /// Human-readable reason.
        detail: String,
    },
}

impl fmt::Display for SkillsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => {
                write!(f, "I/O error while accessing `{}`: {source}", display_path(path))
            }
            Self::ManifestParse { path, detail } => {
                write!(f, "failed to parse skill manifest at `{}`: {detail}", display_path(path))
            }
            Self::InvalidId { id, path } => {
                let path = display_path(path);
                if id.trim().is_empty() {
                    // An empty id renders as `(empty)` instead of dangling
                    // backticks around nothing (`invalid skill id `` at ...`).
                    write!(f, "invalid skill id (empty) at `{path}`")
                } else {
                    write!(
                        f,
                        "invalid skill id `{id}` at `{path}`: must not contain `/`, `\\`, `:`, or NUL"
                    )
                }
            }
            Self::FrontMatter { path, detail } => {
                write!(f, "failed to parse YAML front matter in `{}`: {detail}", display_path(path))
            }
            Self::AlreadyExists { path } => {
                write!(f, "skill pack already exists at `{}`", display_path(path))
            }
            Self::NotAPack { path } => write!(
                f,
                "no skill manifest (`skill.toml` or `SKILL.md`) found in `{}`",
                display_path(path)
            ),
            Self::UnsupportedFormat { path, detail } => write!(
                f,
                "skill pack at `{}` cannot be modified this way: {detail}",
                display_path(path)
            ),
        }
    }
}

/// Render a path for user-facing messages. On Windows, forward slashes are
/// normalized to backslashes so a mixed path such as
/// `C:\Users\alice/.local/share/...` (a `~/.local/share/...` literal joined
/// onto a `%PROFILE%` root) reads consistently as
/// `C:\Users\alice\.local\share\...`. Other platforms render the lossy form
/// unchanged.
fn display_path(path: &Path) -> String {
    let lossy = path.to_string_lossy();
    #[cfg(windows)]
    {
        lossy.replace('/', "\\")
    }
    #[cfg(not(windows))]
    {
        lossy.into_owned()
    }
}
