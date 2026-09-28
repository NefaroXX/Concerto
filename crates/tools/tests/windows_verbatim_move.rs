//! Windows-only reproduction/regression for the "verbatim-rooted existence
//! check" defect.
//!
//! The project root is stored with the Windows VERBATIM prefix (`\\?\C:\...`).
//! A real file created in that root was reported missing by `move` in 0-1 ms
//! even though `list` enumerated the same directory seconds earlier. This test
//! exercises the real [`resolve_path`] and the real
//! [`VirtualFs::move_file`] path against a verbatim-rooted temp directory
//! containing a real file.
//!
//! Windows-only and `#[ignore]`d by default so the Linux CI is unaffected.
//! Run it on a Windows machine with:
//!
//! ```text
//! cargo test -p concerto-tools --test windows_verbatim_move -- --ignored --nocapture
//! ```

#![cfg(windows)]

use camino::{Utf8Path, Utf8PathBuf};
use concerto_tools::common::resolve_path;
use concerto_tools::virtual_fs::{VirtualFs, VirtualFsEntry};

/// Build the verbatim (`\\?\`) form of a plain absolute Windows path, as the
/// configuration stores the project root on Windows.
fn verbatim(path: &Utf8Path) -> Utf8PathBuf {
    Utf8PathBuf::from(format!(r"\\?\{}", path.as_str()))
}

#[test]
#[ignore = "Windows-only: cargo test -p concerto-tools --test windows_verbatim_move -- --ignored --nocapture"]
fn verbatim_rooted_existing_file_is_found_and_moved() {
    let dir = tempfile::tempdir().expect("create temp dir");
    let plain_root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf-8 root");
    let verbatim_root = verbatim(&plain_root);

    // A real, existing file in the verbatim-rooted workspace.
    std::fs::write(plain_root.join("file_test.txt"), "").expect("seed file on disk");

    let source =
        resolve_path(&verbatim_root, Utf8Path::new("file_test.txt")).expect("resolve source");
    assert!(
        source.as_std_path().exists(),
        "resolved source must exist on disk (verbatim form): {source}"
    );

    let dest = resolve_path(&verbatim_root, Utf8Path::new("file_test_pass.txt"))
        .expect("resolve destination");

    let mut vfs = VirtualFs::new();
    vfs.move_file(&source, &dest).expect("moving an existing verbatim-rooted file must succeed");
    assert!(
        matches!(
            vfs.get(&dest),
            Some(VirtualFsEntry::Created { .. }) | Some(VirtualFsEntry::Modified { .. })
        ),
        "destination must be staged after the move"
    );

    // Fail-closed: a genuinely absent source still reports not-found.
    let missing =
        resolve_path(&verbatim_root, Utf8Path::new("ghost.txt")).expect("resolve missing source");
    let missing_dest = resolve_path(&verbatim_root, Utf8Path::new("ghost_pass.txt"))
        .expect("resolve missing destination");
    let mut probe = VirtualFs::new();
    let error = probe
        .move_file(&missing, &missing_dest)
        .expect_err("a missing verbatim-rooted source must not move");
    assert!(
        error.to_string().contains("file not found"),
        "expected a not-found error, got: {error}"
    );
}
