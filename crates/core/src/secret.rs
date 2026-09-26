//! Zero-on-drop holder for secret material that lives in process RAM.
//!
//! Threat model `docs/security-threat-model.md` §6 gap #9 ("No Memory
//! Encryption for Sensitive Data"): a memory dump, a crash report, or an
//! accidental `{:?}` on a struct that happens to carry a provider key is
//! enough to hand an attacker a credential. This module closes the parts of
//! that gap that can be closed **without** `unsafe` code:
//!
//! 1. **Zero-on-drop** — [`SecretString`] wipes its backing buffer with
//!    `zeroize` (volatile writes the compiler may not elide) when dropped, so
//!    the secret does not outlive its holder in a freed-but-readable heap slot.
//! 2. **No rendering** — `Debug` and `Display` both print a redaction marker
//!    instead of bytes, so a `{:?}`/`{}` on any struct embedding a secret
//!    cannot leak it into a log line or an error string.
//! 3. **Explicit reads** — [`SecretString::expose`] hands back a `&str`
//!    reference instead of cloning, so call sites pass the secret by
//!    reference rather than materializing extra `String` copies.
//!
//! Deliberately **not** covered here: `mlock`/`madvise` page pinning. See the
//! DEFERRED row 47 report for the exact reason (no safe abstraction exists in
//! the dependency graph, and the workspace denies `unsafe_code`).

use std::fmt;

use zeroize::Zeroize;

/// Count of buffers wiped so far; test builds only.
///
/// `Drop` for [`SecretString`] is one line, but "the value was wiped" is a
/// behavioral contract, not something a reader should have to trace by eye.
/// The counter lets the drop test assert that the wipe path actually ran for
/// a value that went out of scope, without reading freed memory.
#[cfg(test)]
static SECRET_WIPES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// A secret string that is zeroed on drop and never rendered.
///
/// ```
/// use concerto_core::secret::SecretString;
///
/// let key = SecretString::new("sk-synthetic-example");
/// assert_eq!(key.expose(), "sk-synthetic-example");
/// assert_eq!(format!("{key:?}"), "SecretString([REDACTED])");
/// assert_eq!(format!("{key}"), "[REDACTED]");
/// ```
pub struct SecretString {
    inner: String,
}

impl SecretString {
    /// Wrap `value` so it is zeroed when this holder is dropped.
    pub fn new(value: impl Into<String>) -> Self {
        Self { inner: value.into() }
    }

    /// Deliberate read access to the plaintext.
    ///
    /// Returns a borrow, not a copy: pass the `&str` along rather than
    /// calling `.to_string()` on it, so the secret exists in as few live
    /// buffers as possible.
    pub fn expose(&self) -> &str {
        &self.inner
    }

    /// Whether the secret is empty (e.g. a local-model setup with no key).
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Length of the plaintext in bytes. Never renders the plaintext itself.
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// Wipe the backing buffer in place and record that it happened.
    ///
    /// `zeroize` overwrites the whole allocation (initialized bytes and spare
    /// capacity alike) with zeroes using volatile writes, then drops the
    /// length to 0. The allocation stays reserved, so no fresh unzeroed copy
    /// is made by a shrink.
    fn wipe(&mut self) {
        self.inner.zeroize();
        #[cfg(test)]
        SECRET_WIPES.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}

impl Drop for SecretString {
    fn drop(&mut self) {
        self.wipe();
    }
}

impl Clone for SecretString {
    /// Clone the plaintext into a second holder.
    ///
    /// Both holders wipe independently on drop. Prefer passing `&SecretString`
    /// (or `expose()`) over cloning — this exists for the call sites that
    /// genuinely need an owned second copy.
    fn clone(&self) -> Self {
        Self { inner: self.inner.clone() }
    }
}

impl Default for SecretString {
    fn default() -> Self {
        Self::new(String::new())
    }
}

impl From<String> for SecretString {
    /// Move an existing `String` in without copying its buffer.
    fn from(value: String) -> Self {
        Self { inner: value }
    }
}

impl From<&str> for SecretString {
    fn from(value: &str) -> Self {
        Self { inner: value.to_owned() }
    }
}

impl fmt::Debug for SecretString {
    /// Redacted on purpose: a secret must never reach a log line, a panic
    /// message, or a `{:?}` of any struct that embeds one.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretString([REDACTED])")
    }
}

impl fmt::Display for SecretString {
    /// Redacted on purpose: `{}` on a secret must not render it either.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[REDACTED]")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Clearly-marked synthetic fixture. Never a real credential.
    const SYNTHETIC_SECRET: &str = "sk-synthetic-fixture-0123456789abcdef";

    #[test]
    fn expose_returns_the_plaintext_by_reference() {
        let secret = SecretString::new(SYNTHETIC_SECRET);
        assert_eq!(secret.expose(), SYNTHETIC_SECRET);
        assert_eq!(secret.len(), SYNTHETIC_SECRET.len());
        assert!(!secret.is_empty());
        assert!(SecretString::default().is_empty());
    }

    /// The wipe path must actually run when a holder goes out of scope —
    /// this is the zero-on-drop contract, asserted without touching freed
    /// memory.
    #[test]
    fn drop_runs_the_wipe() {
        let before = SECRET_WIPES.load(std::sync::atomic::Ordering::SeqCst);
        {
            let secret = SecretString::new(SYNTHETIC_SECRET);
            assert_eq!(secret.expose(), SYNTHETIC_SECRET);
            // Drop explicitly so the assertion below is about this value.
            drop(secret);
        }
        let after = SECRET_WIPES.load(std::sync::atomic::Ordering::SeqCst);
        assert_eq!(
            after,
            before + 1,
            "dropping a SecretString must run the zeroing wipe exactly once"
        );
    }

    /// The wipe zeroes the buffer in place (capacity is retained rather than
    /// the string being replaced by a fresh allocation), and the plaintext is
    /// no longer readable through `expose`.
    #[test]
    fn wipe_zeroes_the_buffer_in_place() {
        let mut secret = SecretString::new(SYNTHETIC_SECRET);
        let capacity_before = secret.inner.capacity();
        assert!(capacity_before >= SYNTHETIC_SECRET.len());

        secret.wipe();

        assert!(secret.expose().is_empty(), "wipe must clear the readable length");
        assert!(
            secret.inner.capacity() >= capacity_before,
            "wipe must keep the same buffer instead of reallocating"
        );
    }

    /// Direct check of the primitive the wipe relies on: `zeroize` overwrites
    /// the bytes of a buffer with zeroes rather than merely dropping the
    /// value. A fixed-size array is used because it is the only shape whose
    /// contents stay observable after zeroing without reading freed memory.
    #[test]
    fn zeroize_primitive_overwrites_every_byte() {
        let mut buf = [0u8; 64];
        buf[..SYNTHETIC_SECRET.len()].copy_from_slice(SYNTHETIC_SECRET.as_bytes());
        assert!(buf.starts_with(SYNTHETIC_SECRET.as_bytes()));

        buf.zeroize();

        assert_eq!(buf.len(), 64, "zeroize must not resize a fixed buffer");
        assert!(buf.iter().all(|&b| b == 0), "every byte must be overwritten");
    }

    #[test]
    fn debug_never_renders_secret_bytes() {
        let secret = SecretString::new(SYNTHETIC_SECRET);
        let rendered = format!("{secret:?}");
        assert!(!rendered.contains(SYNTHETIC_SECRET), "Debug leaked the secret: {rendered}");
        assert!(!rendered.contains("sk-synthetic"), "Debug leaked a secret prefix: {rendered}");
        assert_eq!(rendered, "SecretString([REDACTED])");
    }

    #[test]
    fn display_never_renders_secret_bytes() {
        let secret = SecretString::new(SYNTHETIC_SECRET);
        let rendered = format!("{secret}");
        assert!(!rendered.contains(SYNTHETIC_SECRET), "Display leaked the secret: {rendered}");
        assert_eq!(rendered, "[REDACTED]");
    }

    /// The realistic leak is not `{:?}` on the secret itself but `{:?}` on a
    /// struct that embeds it (a provider, a pending config, an event payload).
    #[test]
    fn debug_of_an_embedding_struct_never_renders_secret_bytes() {
        #[derive(Debug)]
        // The fields exist only to exercise a derived Debug; nothing reads
        // them back.
        #[allow(dead_code)]
        struct Holder {
            provider: String,
            key: SecretString,
        }

        let holder =
            Holder { provider: "openai".to_string(), key: SecretString::new(SYNTHETIC_SECRET) };

        let rendered = format!("{holder:?}");
        assert!(!rendered.contains(SYNTHETIC_SECRET), "embedded Debug leaked: {rendered}");
        assert!(!rendered.contains("sk-synthetic"), "embedded Debug leaked a prefix: {rendered}");
        assert!(rendered.contains("[REDACTED]"), "redaction marker missing: {rendered}");
        assert!(rendered.contains("openai"), "non-secret fields must still render: {rendered}");
    }

    #[test]
    fn secrets_in_a_collection_do_not_render() {
        let secrets = vec![SecretString::new(SYNTHETIC_SECRET)];
        let rendered = format!("{secrets:?}");
        assert!(!rendered.contains(SYNTHETIC_SECRET), "collection Debug leaked: {rendered}");
    }

    #[test]
    fn clone_produces_an_independent_holder() {
        let original = SecretString::new(SYNTHETIC_SECRET);
        let copied = original.clone();
        assert_eq!(copied.expose(), SYNTHETIC_SECRET);
        drop(original);
        assert_eq!(copied.expose(), SYNTHETIC_SECRET, "clones must not share state");
    }

    #[test]
    fn from_string_moves_without_an_extra_read() {
        let secret = SecretString::from(SYNTHETIC_SECRET.to_string());
        assert_eq!(secret.expose(), SYNTHETIC_SECRET);
        let borrowed = SecretString::from(SYNTHETIC_SECRET);
        assert_eq!(borrowed.expose(), SYNTHETIC_SECRET);
    }
}
