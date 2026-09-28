//! Declared-artifact identity — the ONE shared rule for what counts as a
//! plan-declared file path (audit C-06 acceptance + Phase 6 M3c plan drift).
//!
//! Plans declare artifacts as plain strings, and models routinely emit prose
//! (`"DESIGN.md: Comprehensive design document as specified in the
//! requirements."`) where a bare path was expected. Existence checks must
//! never treat such prose as a missing file:
//!
//! * [`path_without_description`] — declaration side: strips a
//!   `": <description>"` suffix off a path-like head at the ingest boundary,
//!   so delivered work verifies as the real path it names.
//! * [`classify`] — check side: partitions any declaration into a concrete
//!   path, a non-file expectation (directory/glob — vacuous), or prose
//!   (`Unverifiable`). Prose is reported as
//!   [`UNVERIFIABLE_REASON`] — never as `missing`.

use std::borrow::Cow;

/// The C-06 violation reason for a declared artifact that carries no
/// parseable file path. Deliberately distinct from `missing:` — an
/// unverifiable declaration is a malformed plan input, not absent work.
pub const UNVERIFIABLE_REASON: &str = "undeclared/unverifiable: declaration carries no file path";

/// A plan-declared artifact, classified for existence checking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeclaredArtifact {
    /// A concrete workspace-relative file path (normalized: forward slashes,
    /// no leading `./`) — safe to resolve against the project root.
    Path(String),
    /// Not a file at all (empty, a directory, or a glob): the inventory
    /// cannot speak to it, so it is skipped rather than reported.
    NonFile,
    /// Prose (or anything else without a parseable path). NEVER `missing` —
    /// it is undeclared/unverifiable, a declaration-quality problem.
    Unverifiable,
}

/// Normalize a workspace-relative path for inventory comparison: forward
/// slashes only, no leading `./`. The snapshot inventory stores paths in
/// this canonical form.
pub fn normalize_relative_path(path: &str) -> String {
    let slash_normalized = path.replace('\\', "/");
    slash_normalized.strip_prefix("./").unwrap_or(&slash_normalized).to_owned()
}

/// Classify one declared artifact for existence checking.
///
/// Order matters: non-file shapes are recognized first (a glob may contain
/// whitespace without being prose), then whitespace marks prose — a rule
/// mirroring `resolver::parse_expected_artifacts`, which already ignores
/// whitespace-bearing contract lines. Residual risk: a legitimate path that
/// itself contains spaces is classified [`DeclaredArtifact::Unverifiable`]
/// (it is checked as a declaration problem, never as `missing`).
pub fn classify(raw: &str) -> DeclaredArtifact {
    let normalized = normalize_relative_path(raw);
    if normalized.is_empty()
        || normalized.ends_with('/')
        || normalized.contains('*')
        || normalized.contains('?')
    {
        return DeclaredArtifact::NonFile;
    }
    if normalized.chars().any(char::is_whitespace) {
        return DeclaredArtifact::Unverifiable;
    }
    DeclaredArtifact::Path(normalized)
}

/// Declaration side: the file path out of a `"<path>: <description>"`
/// declaration, or `raw` unchanged when there is no description suffix.
///
/// A split happens only when the head is non-empty, whitespace-free, and
/// path-like (contains `/` or `.`) — so `"DESIGN.md: Comprehensive design
/// document…"` yields `DESIGN.md`, while genuine prose (`"Note: see this"`,
/// `"Some free-form sentence."`) stays whole and therefore classifies as
/// [`DeclaredArtifact::Unverifiable`] downstream. Splitting stops at the
/// FIRST `": "`.
pub fn path_without_description(raw: &str) -> Cow<'_, str> {
    let Some((head, _tail)) = raw.split_once(": ") else {
        return Cow::Borrowed(raw);
    };
    let path_like = head.contains('/') || head.contains('.');
    if head.is_empty() || head.chars().any(char::is_whitespace) || !path_like {
        return Cow::Borrowed(raw);
    }
    Cow::Borrowed(head)
}

/// Declaration side: strip description suffixes off a declared artifact
/// list (see [`path_without_description`]), preserving order. Paths are
/// otherwise left exactly as declared — inventory comparison and existence
/// checking normalize via [`classify`].
pub fn declared_paths<S: AsRef<str>>(raw: &[S]) -> Vec<camino::Utf8PathBuf> {
    raw.iter()
        .map(|entry| {
            camino::Utf8PathBuf::from(path_without_description(entry.as_ref()).into_owned())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const DESIGN_DOC_ENTRY: &str =
        "DESIGN.md: Comprehensive design document as specified in the requirements.";
    const STATUS_ICON_ENTRY: &str =
        "src/components/StatusIndicator/StatusIcon.tsx: Component for rendering status icons.";

    #[test]
    fn prose_declarations_are_unverifiable_never_paths() {
        for entry in [DESIGN_DOC_ENTRY, STATUS_ICON_ENTRY, "Some free-form sentence."] {
            assert_eq!(classify(entry), DeclaredArtifact::Unverifiable, "{entry:?}");
        }
    }

    #[test]
    fn classify_recognizes_paths_directories_and_globs() {
        assert_eq!(classify("src/lib.rs"), DeclaredArtifact::Path("src/lib.rs".into()));
        assert_eq!(classify("./src/lib.rs"), DeclaredArtifact::Path("src/lib.rs".into()));
        assert_eq!(classify("src\\lib.rs"), DeclaredArtifact::Path("src/lib.rs".into()));
        assert_eq!(classify(""), DeclaredArtifact::NonFile);
        assert_eq!(classify("src/"), DeclaredArtifact::NonFile);
        assert_eq!(classify("src/*.rs"), DeclaredArtifact::NonFile);
    }

    #[test]
    fn declaration_split_strips_path_like_description_suffixes_only() {
        assert_eq!(path_without_description(DESIGN_DOC_ENTRY), "DESIGN.md");
        assert_eq!(
            path_without_description(STATUS_ICON_ENTRY),
            "src/components/StatusIndicator/StatusIcon.tsx"
        );
        // Plain paths and prose stay untouched (prose is left for `classify`).
        assert_eq!(path_without_description("src/lib.rs"), "src/lib.rs");
        assert_eq!(path_without_description("Note: see this"), "Note: see this");
        assert_eq!(path_without_description("docs/release notes.md"), "docs/release notes.md");
        assert_eq!(
            path_without_description("DESIGN.md: "),
            "DESIGN.md",
            "an empty description suffix still names a path"
        );
    }

    #[test]
    fn declared_paths_strips_descriptions_and_keeps_paths_verbatim() {
        let paths = declared_paths(&[
            DESIGN_DOC_ENTRY.to_owned(),
            "./src/other.rs".to_owned(),
            "src/".to_owned(),
        ]);
        assert_eq!(
            paths,
            vec![
                camino::Utf8PathBuf::from("DESIGN.md"),
                // Splits descriptions only; the path itself is verbatim
                // (`classify` normalizes at check time).
                camino::Utf8PathBuf::from("./src/other.rs"),
                // Non-files pass through: `classify` gates them at check time.
                camino::Utf8PathBuf::from("src/"),
            ]
        );
    }
}
