//! ADR-44 §4 — local-only consent gate for opening projects outside the
//! configured `project_roots` allowlist.
//!
//! The *effective* allowlist is seeded once at startup with the canonicalized
//! configured roots and grows for the process lifetime when the user clicks
//! [Allow]; it is never written back to the config file. When the allowlist is
//! empty (roots unset) the gate is skipped entirely, preserving the permissive
//! local default.
//!
//! This module keeps the decision logic pure (no I/O) so it can be tested in
//! isolation; the modal card rendering lives here so both the project launcher
//! (first open, no app yet) and the running app can share it.

use std::path::{Path, PathBuf};

use iced::widget::{button, column, container, row, text};
use iced::{Element, Length};

/// Returns `true` when the canonical project `path` must be gated behind the
/// ADR-44 consent modal.
///
/// - Empty `allowlist` (roots unset) never gates — the permissive local
///   default.
/// - Otherwise any path that is not inside (or equal to) one of the listed
///   canonical roots is gated.
///
/// `allowlist` is the *effective* allowlist: canonicalized configured roots
/// plus every path the user has already allowed for this process.
pub fn needs_consent(path: &Path, allowlist: &[PathBuf]) -> bool {
    !allowlist.is_empty() && !confined_to_roots(path, allowlist)
}

/// Returns `true` when `path` is inside (or equal to) any canonical root.
///
/// Component-safe prefix check that mirrors the api-server's
/// `confined_to_a_root` (`crates/api-server/src/routes.rs`): `strip_prefix`
/// only matches on whole path components, so a root `/srv/proj` does not admit
/// its prefix sibling `/srv/proj2`.
fn confined_to_roots(path: &Path, roots: &[PathBuf]) -> bool {
    roots.iter().any(|root| path.strip_prefix(root).is_ok())
}

/// Canonicalize the configured `project_roots` for the effective allowlist.
///
/// A root that cannot be canonicalized (missing or not a directory) contributes
/// nothing and is skipped, mirroring the api-server. Storing only canonical
/// roots keeps [`needs_consent`] a pure function (no I/O) while still resolving
/// `..` and symlinks before the prefix comparison.
pub fn canonical_roots(configured: &[camino::Utf8PathBuf]) -> Vec<PathBuf> {
    configured.iter().filter_map(|root| std::fs::canonicalize(root.as_std_path()).ok()).collect()
}

/// Render the ADR-44 consent-gate card (title, security copy, the pending
/// path, and [Deny] / [Allow] actions).
///
/// `Message` is generic so both the project launcher and the running app can
/// reuse the card with their own message enums. Callers wrap this card in the
/// system-dialog palette backdrop and place it at the top of the dialog stack.
pub fn consent_card<'a, Message: Clone + 'a>(
    path: &Path,
    theme: &iced::Theme,
    allow: Message,
    deny: Message,
) -> Element<'a, Message> {
    let palette = theme.palette();

    let title = text("Allow project outside configured roots?").size(18);
    let muted =
        text("Allow applies for this session only — it is not saved to your configuration.")
            .size(14)
            .color(palette.text);

    let body = column![
        text("This folder is outside the project roots configured for Concerto.").size(14),
        text(path.display().to_string()).size(13).color(palette.text),
        text("Concerto's agent can read and write files in this folder. Allow only if you trust this location.")
            .size(14),
        muted,
    ]
    .spacing(8);

    let allow_btn = button(text("Allow")).style(crate::ui::button::primary).on_press(allow);
    let deny_btn = button(text("Deny")).style(crate::ui::button::secondary).on_press(deny);

    let content = column![title, body, row![deny_btn, allow_btn].spacing(10).padding(10)]
        .spacing(12)
        .padding(24);

    container(content)
        .width(Length::Fill)
        .center_x(Length::Fill)
        .center_y(Length::Fill)
        .style(crate::ui::container::modal)
        .into()
}

/// Serializes desktop tests that persist the real project registry
/// (`ProjectRegistry::save()` in the open/switch flow and `App::new`).
/// The registry's temporary file name is process-scoped, so concurrent
/// saves from parallel test threads race on the write/rename steps and
/// fail spuriously. Test-only: the desktop UI itself runs updates on a
/// single thread.
#[cfg(test)]
pub(crate) static REGISTRY_SAVE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    fn root(path: &str) -> PathBuf {
        PathBuf::from(path)
    }

    #[test]
    fn empty_roots_never_gate() {
        assert!(!needs_consent(Path::new("/tmp/anything"), &[]));
    }

    #[test]
    fn path_inside_or_equal_to_root_is_not_gated() {
        let roots = vec![root("/srv/proj")];
        assert!(!needs_consent(Path::new("/srv/proj"), &roots));
        assert!(!needs_consent(Path::new("/srv/proj/src/lib.rs"), &roots));
    }

    #[test]
    fn path_outside_root_is_gated() {
        let roots = vec![root("/srv/proj")];
        assert!(needs_consent(Path::new("/home/user/other"), &roots));
    }

    #[test]
    fn session_allow_adds_pending_path_to_allowlist() {
        let mut allowlist = vec![root("/srv/proj")];
        let pending = root("/home/user/other");
        assert!(needs_consent(&pending, &allowlist));
        // After [Allow] the canonical path enters the effective allowlist.
        allowlist.push(pending.clone());
        assert!(!needs_consent(&pending, &allowlist));
    }

    #[test]
    fn prefix_sibling_is_gated() {
        let roots = vec![root("/srv/proj")];
        // strip_prefix is component-safe: a sibling name with a shared prefix
        // is NOT inside the root.
        assert!(needs_consent(Path::new("/srv/proj2"), &roots));
        assert!(needs_consent(Path::new("/srv/proj-extra"), &roots));
    }

    #[test]
    fn canonical_roots_skips_unresolvable_roots() {
        let temp = tempfile::tempdir().unwrap();
        let real = temp.path().join("real");
        std::fs::create_dir_all(&real).unwrap();
        let configured = vec![
            camino::Utf8PathBuf::from_path_buf(real.clone())
                .unwrap_or_else(|p| camino::Utf8PathBuf::from(p.to_string_lossy().as_ref())),
            camino::Utf8PathBuf::from("/definitely/missing/root-xyz"),
        ];
        let canonical = canonical_roots(&configured);
        assert_eq!(canonical, vec![real.canonicalize().unwrap()]);
    }
}
