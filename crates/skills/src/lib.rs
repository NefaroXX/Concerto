#![deny(clippy::all)]
#![deny(unused_imports, unused_variables, dead_code)]
#![allow(missing_docs)]

//! `concerto-skills` — skill-pack discovery and loading (ADR-43, Phase B).
//!
//! A skill is a local directory containing a manifest — either `skill.toml`
//! or a `SKILL.md` file with YAML front matter — plus optional instruction and
//! resource files. Skills never execute code; they are instruction packs that
//! the orchestrator injects into prompts (Task 4). This crate only discovers,
//! parses, and loads packs; it holds no configuration state (`SkillManager`
//! receives search paths and enabled ids as parameters, so there is no
//! dependency on `concerto-config`).
//!
//! The shared data model (`SkillManifest`, `SkillDescriptor`) is reused from
//! `concerto-api-types` — see `crates/api-types/src/extension.rs`.
//!
//! # Discovery
//!
//! [`SkillManager::discover`] expands a leading `~` (and Windows-style `%VAR%`
//! references such as `%USERPROFILE%`/`%APPDATA%`) in each search path, then
//! walks the tree to a bounded depth (4 levels below the search path root)
//! looking for directories that contain a manifest. Directories without a
//! manifest are silently not skills; missing search paths are warned about and
//! skipped; a malformed pack is warned about and skipped, so one broken pack
//! never aborts discovery of the rest. Results are sorted by id, and duplicate
//! ids keep the first occurrence (deterministically: lowest `(id, path)` wins).
//!
//! A manifest whose `id` trims to empty is not a failure: discovery falls back
//! to the pack directory's name as the id (e.g. a pack in `…/deepwork` without
//! an id loads as `deepwork`) and records an informational note.
//!
//! [`SkillManager::discover_with_report`] performs the same discovery but
//! returns the diagnostics as a [`DiscoveryReport`] (resolved search paths,
//! descriptors, per-path/per-pack warnings, and informational notes) so UI
//! callers can surface why a "no skills found" result happened;
//! [`expanded_search_path`] expands a single configured path for display
//! without scanning.
//!
//! # Manifest formats
//!
//! ## `skill.toml`
//!
//! TOML with the same field names as the shared data model. Only `id` is
//! required and must be non-empty after trimming (an empty id loads under the
//! pack directory's name; see Discovery above). Optional fields default to
//! `""` (`name`, `version`, `description`), `None` (`instructions`,
//! `instructions_path`), or an empty list (`tools`, `resources`). Unknown
//! fields are ignored. `instructions` is inline text; `instructions_path` is a
//! path relative to the pack directory (when both are set,
//! `instructions_path` wins). All paths are resolved to absolute form in the
//! returned descriptor.
//!
//! ```toml
//! id = "rust-testing"
//! name = "Rust Testing"
//! version = "1.0.0"
//! description = "Cargo verification guidance"
//! instructions = "Prefer cargo nextest."
//! tools = ["cargo nextest run", "cargo clippy"]
//! resources = ["fixtures/sample.rs"]
//! ```
//!
//! ## `SKILL.md`
//!
//! YAML-subset front matter between `---` delimiter lines at the start of the
//! file; the markdown body below the closing `---` is the instruction text.
//! When both formats are present in one directory, `skill.toml` wins.
//!
//! ```markdown
//! ---
//! id: commit-style
//! name: Commit Style
//! tools:
//!   - cargo test
//!   - cargo fmt
//! ---
//! # Commit Style
//!
//! Always use conventional commits.
//! ```
//!
//! # YAML front-matter subset
//!
//! The front matter is parsed by a small built-in parser — not a general YAML
//! implementation — because `serde_yaml` and `yaml-rust` are deprecated or
//! unmaintained. The subset supports exactly:
//!
//! - Scalar `key: value` lines for the keys `id`, `name`, `version`,
//!   `description`, and `instructions`. An empty value (`key:` alone) leaves
//!   the field unset and defaults apply.
//! - List blocks for `tools` and `resources`: a `key:` line followed by
//!   `- item` lines. Inline values for these keys are an error.
//! - Single- or double-quoted scalars (taken literally; no escape processing)
//!   and unquoted scalars.
//! - `#` comments: full-line comments (first non-whitespace character is `#`)
//!   and inline comments (a `#` preceded by whitespace, outside quotes).
//! - Unknown keys are ignored (matching `skill.toml`'s unknown-field
//!   behavior); repeated keys keep the last value.
//!
//! Not supported (an error): nested structures, flow lists (`[a, b]`), block
//! scalars (`|`, `>`), anchors/aliases, escapes, multi-document `---`.
//!
//! Malformed front matter (missing or unterminated delimiters, malformed
//! lines, unterminated quotes) yields [`SkillsError::FrontMatter`] with the
//! file path and a reason. The body below the closing `---` is the
//! instruction text; a front-matter `instructions` key is used only when the
//! body is empty.
//!
//! # Management (CRUD)
//!
//! [`SkillManager::create_pack`] writes a new `skill.toml` pack at
//! `parent/<id>` (id validated, parent created when missing, atomic write),
//! [`SkillManager::update_pack`] rewrites an existing pack's `skill.toml` in
//! place (id is not editable — it follows the directory name), and
//! [`SkillManager::delete_pack`] renames the manifest file(s) to hidden
//! `.deleted-<stamp>-*` backups so the pack leaves discovery but stays
//! recoverable. SKILL.md-only packs can be deleted but not updated by this
//! API ([`SkillsError::UnsupportedFormat`]).
//!
//! # Errors
//!
//! All fallible operations return [`SkillsError`] (thiserror): `Io`,
//! `ManifestParse`, `InvalidId`, `FrontMatter`, `AlreadyExists`, `NotAPack`,
//! and `UnsupportedFormat`. `InvalidId` carries the offending id and the
//! skill pack directory. The library (non-test) code contains no
//! `unwrap()`/`expect()`.

mod error;
mod frontmatter;
mod manager;
mod manifest;

pub use concerto_api_types::extension::{SkillDescriptor, SkillManifest};
pub use error::SkillsError;
pub use manager::{expanded_search_path, DiscoveryReport, SkillManager};
