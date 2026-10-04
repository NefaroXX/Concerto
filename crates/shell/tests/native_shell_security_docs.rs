//! Guards the two scope-limitation notes in `docs/native-shell-security.md`.
//!
//! Both notes are security findings: dropping either one silently widens what
//! a reader believes the controls guarantee.

use std::path::PathBuf;

fn security_doc() -> String {
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/native-shell-security.md");
    std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()))
}

/// The paragraph (table row or text block) starting at `anchor`, up to the
/// next blank line or the end of the document.
fn paragraph<'a>(doc: &'a str, anchor: &str) -> &'a str {
    let start = doc.find(anchor).unwrap_or_else(|| panic!("anchor missing from doc: {anchor}"));
    let rest = &doc[start..];
    rest.split("\n\n").next().unwrap_or(rest)
}

// verifies: the protected-paths row states that it binds only the built-in
// filesystem tool, not native run commands or container mounts.
#[test]
fn protected_paths_scope_limit_is_documented_with_the_setting() {
    let doc = security_doc();
    let row = paragraph(&doc, "| `protected_paths` |");
    assert!(row.contains("built-in filesystem tool"), "scope limit lost: {row}");
    assert!(row.contains("native `run` commands"), "run exclusion lost: {row}");
    assert!(row.contains("container project mounts"), "container-mount exclusion lost: {row}");
    assert!(row.contains("does NOT constrain"), "limitation must be explicit: {row}");
}

// verifies: the Windows note says cancellation kills only the direct child and
// that descendants in a new process group survive.
#[test]
fn windows_cancellation_documents_direct_child_only() {
    let doc = security_doc();
    let para = paragraph(&doc, "Windows cancellation terminates only the direct child");
    assert!(para.contains("Descendants survive it"), "survivors must be stated: {para}");
    assert!(para.contains("new process group"), "process-group escape must be stated: {para}");
    assert!(para.contains("Job Object"), "missing Job Object boundary note: {para}");
}
