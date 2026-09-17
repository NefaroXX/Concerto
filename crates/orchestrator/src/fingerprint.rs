//! Content-addressed semantic identity and artifact fingerprints for
//! zero-waste orchestration (ADR-64 Phase 3).
//!
//! This module provides **pure, deterministic** hashing primitives that
//! Phase 4's pre-dispatch resolver will call before every model dispatch.
//! It is purely a derivation layer — no DB writes, no new tables, no async I/O.
//!
//! # Canonical encoding format (STABLE — Phase 4 and tests depend on this)
//!
//! A [`SemanticKey`] is a blake3-256 digest of the five components encoded as
//! a **length-prefixed binary blob**:
//!
//! ```text
//! [4 bytes big-endian: len(objective_hash)]  objective_hash (UTF-8)
//! [4 bytes big-endian: len(plan_version)]    plan_version   (UTF-8)
//! [4 bytes big-endian: len(work_intent)]     work_intent    (UTF-8, normalised)
//! [4 bytes big-endian: len(output_contract)] output_contract(UTF-8)
//! [4 bytes big-endian: N]                    N = number of dependency keys
//!   for each dep (sorted lexicographically by hex):
//!     [4 bytes big-endian: len(dep_hex)]     dep_hex        (UTF-8)
//! ```
//!
//! - All lengths are `u32` big-endian (max component size: 4 GiB, absurd for
//!   this use-case).
//! - Dependency keys are **sorted lexicographically by their hex digest** before
//!   encoding — set semantics.
//! - The entire byte slice is fed to blake3; the resulting 32-byte digest is
//!   encoded as a 64-character lowercase hex string.
//! - A [`SemanticKey`] stores both the 64-char hex digest and the original
//!   five components so `Debug` output and `parse()` are lossless.
//!
//! # Reuse of existing hashing
//!
//! - `work_intent_hash` wraps [`crate::hash::normalize_description`] (shared
//!   canonicalization with `SubTaskHasher`).
//! - `objective_hash` and plan `content_hash` follow the same blake3 pattern
//!   already used in `coordinator.rs:2263` and `plan_approval.rs:173`.

use std::fmt;

use crate::hash::normalize_description;
use crate::timeline::TimelineProjection;

// ---------------------------------------------------------------------------
// SemanticKey
// ---------------------------------------------------------------------------

/// Stable, role-agnostic identity for a unit of work.
///
/// Deterministic and recomputable from the same five inputs.  Agent identity
/// is **never** part of the key.  See the module-level docs for the canonical
/// binary encoding that maps these components to the hex digest.
#[derive(Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct SemanticKey {
    /// 64-character lowercase hex blake3 digest.
    hex: String,
    /// Original components — retained for lossless round-trip and debug.
    components: SemanticComponents,
}

/// The five components that fully determine a [`SemanticKey`].
///
/// Stored alongside the digest so `parse()` is lossless and `Debug` output
/// shows what the key actually means.
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct SemanticComponents {
    /// blake3 hex of the objective text (the run-level goal).
    pub objective_hash: String,
    /// Content hash of the plan artifact governing this work.
    pub plan_version: String,
    /// Normalised description of the specific work intent.
    pub work_intent: String,
    /// Description/hash of the expected output contract.
    pub output_contract: String,
    /// Semantic keys of direct predecessors, sorted lexicographically.
    pub dependency_keys: Vec<String>,
}

impl SemanticKey {
    /// Compute a stable, content-addressed semantic key.
    ///
    /// `dependency_keys` are sorted before encoding (set semantics).
    pub fn compute(
        objective_hash: &str,
        plan_version: &str,
        work_intent: &str,
        output_contract: &str,
        dependency_keys: &[String],
    ) -> Self {
        let mut sorted_deps = dependency_keys.to_vec();
        sorted_deps.sort();

        // Normalise work_intent exactly as the other description hashing paths
        // do, so semantically identical work always produces the same key
        // regardless of casing/whitespace in the caller's description. The
        // caller must not need to remember to pre-normalise.
        let normalised_intent = normalize_description(work_intent);

        let mut buf = Vec::new();
        push_length_prefixed(&mut buf, objective_hash);
        push_length_prefixed(&mut buf, plan_version);
        push_length_prefixed(&mut buf, &normalised_intent);
        push_length_prefixed(&mut buf, output_contract);
        // Dependency count.
        buf.extend_from_slice(&(sorted_deps.len() as u32).to_be_bytes());
        for dep in &sorted_deps {
            push_length_prefixed(&mut buf, dep);
        }

        let digest = blake3::hash(&buf);
        let hex = digest.to_hex().to_string();

        Self {
            hex,
            components: SemanticComponents {
                objective_hash: objective_hash.to_owned(),
                plan_version: plan_version.to_owned(),
                work_intent: normalised_intent,
                output_contract: output_contract.to_owned(),
                dependency_keys: sorted_deps,
            },
        }
    }

    /// 64-character lowercase hex digest.
    pub fn hex(&self) -> &str {
        &self.hex
    }

    /// Borrow the structured components.
    pub fn components(&self) -> &SemanticComponents {
        &self.components
    }

    /// Lossless round-trip: parse a previously-encoded key back.
    ///
    /// The input must be the exact 64-char hex string produced by a prior
    /// [`SemanticKey::hex()`] call.  Returns `Err` if the string is not a
    /// valid 64-char hex digest.
    pub fn parse(hex: &str) -> Result<Self, SemanticKeyError> {
        if hex.len() != 64 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(SemanticKeyError::InvalidHex);
        }
        Ok(Self {
            hex: hex.to_lowercase(),
            components: SemanticComponents {
                // Components are not recoverable from the digest alone;
                // callers who need the components must retain the original
                // compute inputs.  We store a placeholder so the key is
                // still usable for equality checks.
                objective_hash: String::new(),
                plan_version: String::new(),
                work_intent: String::new(),
                output_contract: String::new(),
                dependency_keys: Vec::new(),
            },
        })
    }
}

impl AsRef<str> for SemanticKey {
    fn as_ref(&self) -> &str {
        &self.hex
    }
}

impl fmt::Debug for SemanticKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "SemanticKey({} | obj={} plan={} intent={} contract={} deps={})",
            self.hex,
            truncate(&self.components.objective_hash, 12),
            truncate(&self.components.plan_version, 12),
            truncate(&self.components.work_intent, 40),
            truncate(&self.components.output_contract, 12),
            self.components.dependency_keys.len(),
        )
    }
}

impl fmt::Display for SemanticKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.hex)
    }
}

// ---------------------------------------------------------------------------
// SemanticKeyError
// ---------------------------------------------------------------------------

/// Why parsing a semantic key hex string failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SemanticKeyError {
    /// Input is not a valid 64-character lowercase hex digest.
    #[error("semantic key must be exactly 64 lowercase hex characters")]
    InvalidHex,
}

// ---------------------------------------------------------------------------
// ArtifactFingerprint
// ---------------------------------------------------------------------------

/// Typed fingerprints for each artifact kind the Phase 4 resolver will compare.
///
/// Every variant carries a canonical `content_hash` (blake3 hex) that the
/// resolver uses for equality checks.  The additional fields identify *which*
/// artifact the hash belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum ArtifactFingerprint {
    /// A plan artifact identified by `plan_id`.
    Plan {
        /// Plan identifier (stable across revisions).
        plan_id: String,
        /// blake3 hex of the plan text.
        content_hash: String,
    },
    /// Research output identified by topic.
    Research {
        /// Topic fingerprint (normalised topic string or blake3 of it).
        topic_fingerprint: String,
        /// blake3 hex of the research content.
        content_hash: String,
    },
    /// A file observation (read or listed) at a specific path.
    Observation {
        /// Workspace-relative file path.
        path: String,
        /// blake3 hex of the file content at observation time.
        content_hash: String,
    },
    /// A verification result (test run, lint check, etc.).
    Verification {
        /// Kind of verification (e.g. `"test"`, `"clippy"`, `"fmt"`).
        kind: String,
        /// blake3 hex of the verification output.
        content_hash: String,
    },
    /// A file that was read as input to work.
    Input {
        /// Workspace-relative file path.
        path: String,
        /// blake3 hex of the file content.
        content_hash: String,
    },
    /// A file that was written as output of work.
    Output {
        /// Workspace-relative file path.
        path: String,
        /// blake3 hex of the written content.
        content_hash: String,
    },
    /// A dependency on another work item, identified by its semantic key.
    Dependency {
        /// Semantic key of the dependency.
        key: SemanticKey,
    },
}

// ---------------------------------------------------------------------------
// Fingerprint helpers
// ---------------------------------------------------------------------------

/// Fingerprint a plan artifact.
///
/// `content_hash` should be blake3 hex of the plan text (caller provides;
/// typically from [`crate::plan_approval::plan_artifact_hash`]).
pub fn fingerprint_plan(plan_id: &str, content_hash: String) -> ArtifactFingerprint {
    ArtifactFingerprint::Plan { plan_id: plan_id.to_owned(), content_hash }
}

/// Fingerprint a file observation.
pub fn fingerprint_observation(path: &str, content_hash: String) -> ArtifactFingerprint {
    ArtifactFingerprint::Observation { path: path.to_owned(), content_hash }
}

/// Fingerprint a file read as input.
pub fn fingerprint_input(path: &str, content_hash: String) -> ArtifactFingerprint {
    ArtifactFingerprint::Input { path: path.to_owned(), content_hash }
}

/// Fingerprint a file written as output.
pub fn fingerprint_output(path: &str, content_hash: String) -> ArtifactFingerprint {
    ArtifactFingerprint::Output { path: path.to_owned(), content_hash }
}

/// Fingerprint a verification result.
pub fn fingerprint_verification(kind: &str, content_hash: String) -> ArtifactFingerprint {
    ArtifactFingerprint::Verification { kind: kind.to_owned(), content_hash }
}

// ---------------------------------------------------------------------------
// work_intent_hash
// ---------------------------------------------------------------------------

/// Normalise and hash a work-intent description.
///
/// Uses [`normalize_description`] (shared with `SubTaskHasher`) then blake3.
/// Returns the full 64-char hex digest.
pub fn work_intent_hash(description: &str) -> String {
    let normalised = normalize_description(description);
    blake3::hash(normalised.as_bytes()).to_hex().to_string()
}

// ---------------------------------------------------------------------------
// Timeline predicate
// ---------------------------------------------------------------------------

/// **Existence-only** predicate: does the projection contain any entry whose
/// content hash equals `content_hash`?
///
/// This deliberately does NOT check input/dependency freshness — a matching
/// entry may be stale if a dependency changed after it was produced. Phase 4's
/// resolver must build its own `is_reusable` decision on top that also verifies
/// dependency freshness before ever returning `Reuse`. The explicit `contains`
/// naming exists to prevent a false-positive `Reuse` landmine.
pub fn timeline_contains_hash(projection: &TimelineProjection, content_hash: &str) -> bool {
    projection.events.iter().any(|e| e.content_hash() == content_hash)
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Write a length-prefixed string into `buf`: `[4-byte BE length][UTF-8 bytes]`.
fn push_length_prefixed(buf: &mut Vec<u8>, s: &str) {
    let bytes = s.as_bytes();
    buf.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    buf.extend_from_slice(bytes);
}

/// Truncate a string for compact debug display, without splitting a
/// multi-byte UTF-8 character (java.lang.String style byte-slicing would).
fn truncate(s: &str, max_chars: usize) -> &str {
    if s.len() <= max_chars {
        s
    } else {
        &s[..s.floor_char_boundary(max_chars)]
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::timeline::TimelineEvent;

    // ---- SemanticKey tests ----

    /// Helper: build a key with sensible defaults.
    fn sample_key(deps: Vec<String>) -> SemanticKey {
        SemanticKey::compute(
            "obj-abc123",
            "plan-v1",
            "implement the foo bar",
            "src/foo.rs exists and compiles",
            &deps,
        )
    }

    #[test]
    fn semantic_key_deterministic_same_inputs_same_key() {
        let k1 = sample_key(vec![]);
        let k2 = sample_key(vec![]);
        assert_eq!(k1.hex(), k2.hex(), "same inputs must produce the same digest");
    }

    #[test]
    fn semantic_key_changes_on_objective_hash() {
        let base = sample_key(vec![]);
        let changed = SemanticKey::compute(
            "obj-DIFFERENT", // changed
            "plan-v1",
            "implement the foo bar",
            "src/foo.rs exists and compiles",
            &[],
        );
        assert_ne!(base.hex(), changed.hex());
    }

    #[test]
    fn semantic_key_changes_on_plan_version() {
        let base = sample_key(vec![]);
        let changed = SemanticKey::compute(
            "obj-abc123",
            "plan-v2", // changed
            "implement the foo bar",
            "src/foo.rs exists and compiles",
            &[],
        );
        assert_ne!(base.hex(), changed.hex());
    }

    #[test]
    fn semantic_key_changes_on_work_intent() {
        let base = sample_key(vec![]);
        let changed = SemanticKey::compute(
            "obj-abc123",
            "plan-v1",
            "implement the BAZ qux", // changed
            "src/foo.rs exists and compiles",
            &[],
        );
        assert_ne!(base.hex(), changed.hex());
    }

    #[test]
    fn semantic_key_changes_on_output_contract() {
        let base = sample_key(vec![]);
        let changed = SemanticKey::compute(
            "obj-abc123",
            "plan-v1",
            "implement the foo bar",
            "src/foo.rs AND src/bar.rs exist", // changed
            &[],
        );
        assert_ne!(base.hex(), changed.hex());
    }

    #[test]
    fn semantic_key_changes_on_dependency_set() {
        let base = sample_key(vec!["dep-a".to_owned()]);
        let changed = SemanticKey::compute(
            "obj-abc123",
            "plan-v1",
            "implement the foo bar",
            "src/foo.rs exists and compiles",
            &["dep-b".to_owned()], // different dep
        );
        assert_ne!(base.hex(), changed.hex());
    }

    /// Parameterised over all five components: any single change produces a
    /// different key.
    #[test]
    fn semantic_key_sensitivity_all_five_components() {
        let deps = vec!["dep-x".to_owned()];
        let base = SemanticKey::compute("obj", "plan", "intent", "contract", &deps);

        // Change objective_hash.
        let k1 = SemanticKey::compute("OBJ", "plan", "intent", "contract", &deps);
        assert_ne!(base.hex(), k1.hex(), "objective_hash change");

        // Change plan_version.
        let k2 = SemanticKey::compute("obj", "PLAN", "intent", "contract", &deps);
        assert_ne!(base.hex(), k2.hex(), "plan_version change");

        // Change work_intent (a genuinely different intent, not just casing —
        // casing/whitespace is normalised to the same key by design).
        let k3 = SemanticKey::compute("obj", "plan", "refactor", "contract", &deps);
        assert_ne!(base.hex(), k3.hex(), "work_intent change");

        // Change output_contract.
        let k4 = SemanticKey::compute("obj", "plan", "intent", "CONTRACT", &deps);
        assert_ne!(base.hex(), k4.hex(), "output_contract change");

        // Change dependency set (add an element).
        let k5 = SemanticKey::compute(
            "obj",
            "plan",
            "intent",
            "contract",
            &["dep-x".to_owned(), "dep-y".to_owned()],
        );
        assert_ne!(base.hex(), k5.hex(), "dependency set change");
    }

    /// Agent identity must NOT affect the key.
    #[test]
    fn semantic_key_agent_agnostic() {
        let key_a = SemanticKey::compute("obj", "plan", "intent", "contract", &[]);
        let key_b = SemanticKey::compute("obj", "plan", "intent", "contract", &[]);
        assert_eq!(
            key_a.hex(),
            key_b.hex(),
            "identical inputs with different (implicit) agent ids produce the same key"
        );
    }

    /// `compute` must normalise work_intent internally so semantically
    /// identical descriptions differing only in casing/whitespace produce the
    /// SAME key (callers must not need to pre-normalise).
    #[test]
    fn semantic_key_normalises_work_intent() {
        let a = SemanticKey::compute("obj", "plan", "Implement the Foo Bar", "contract", &[]);
        let b = SemanticKey::compute("obj", "plan", "implement  the   foo bar", "contract", &[]);
        assert_eq!(
            a.hex(),
            b.hex(),
            "equivalent descriptions differing in casing/whitespace must collide"
        );
        // And the stored component holds the normalised form.
        assert_eq!(a.components().work_intent, "implement the foo bar");
    }

    /// Dependency set order must not matter (set semantics).
    #[test]
    fn semantic_key_dependency_order_insensitive() {
        let deps_a = vec!["dep-c".to_owned(), "dep-a".to_owned(), "dep-b".to_owned()];
        let deps_b = vec!["dep-a".to_owned(), "dep-b".to_owned(), "dep-c".to_owned()];
        let k_a = SemanticKey::compute("obj", "plan", "intent", "contract", &deps_a);
        let k_b = SemanticKey::compute("obj", "plan", "intent", "contract", &deps_b);
        assert_eq!(k_a.hex(), k_b.hex(), "dependency order must not change the key");
    }

    /// hex() / parse() round-trip: parse produces the same hex digest.
    /// Note: components are not recoverable from a one-way hash; parse()
    /// returns a key usable for equality checks but with empty components.
    #[test]
    fn semantic_key_hex_parse_roundtrip() {
        let original = sample_key(vec!["dep-1".to_owned(), "dep-2".to_owned()]);
        let hex_string = original.hex().to_owned();

        let parsed = SemanticKey::parse(&hex_string).expect("parse valid hex");
        assert_eq!(original.hex(), parsed.hex(), "round-trip hex must match");

        // AsRef<str> agrees with hex().
        assert_eq!(original.as_ref(), parsed.as_ref());
    }

    #[test]
    fn semantic_key_parse_rejects_invalid_hex() {
        assert_eq!(SemanticKey::parse("not-hex"), Err(SemanticKeyError::InvalidHex));
        assert_eq!(SemanticKey::parse(""), Err(SemanticKeyError::InvalidHex));
        // "g" is not a hex digit.
        assert_eq!(
            SemanticKey::parse(&"g".repeat(64)),
            Err(SemanticKeyError::InvalidHex),
            "non-hex chars rejected"
        );
        assert_eq!(
            SemanticKey::parse(&"a".repeat(63)),
            Err(SemanticKeyError::InvalidHex),
            "too short"
        );
        assert_eq!(
            SemanticKey::parse(&"a".repeat(65)),
            Err(SemanticKeyError::InvalidHex),
            "too long"
        );
    }

    // ---- ArtifactFingerprint tests ----

    #[test]
    fn fingerprint_plan_deterministic() {
        let f1 = fingerprint_plan("plan-1", "hash-aaa".to_owned());
        let f2 = fingerprint_plan("plan-1", "hash-aaa".to_owned());
        assert_eq!(f1, f2);
    }

    #[test]
    fn fingerprint_plan_differs_on_content_change() {
        let f1 = fingerprint_plan("plan-1", "hash-aaa".to_owned());
        let f2 = fingerprint_plan("plan-1", "hash-bbb".to_owned());
        assert_ne!(f1, f2);
    }

    #[test]
    fn fingerprint_observation_deterministic() {
        let f1 = fingerprint_observation("src/lib.rs", "h1".to_owned());
        let f2 = fingerprint_observation("src/lib.rs", "h1".to_owned());
        assert_eq!(f1, f2);
    }

    #[test]
    fn fingerprint_observation_differs_on_content_change() {
        let f1 = fingerprint_observation("src/lib.rs", "h1".to_owned());
        let f2 = fingerprint_observation("src/lib.rs", "h2".to_owned());
        assert_ne!(f1, f2);
    }

    #[test]
    fn fingerprint_observation_differs_on_path_change() {
        let f1 = fingerprint_observation("src/lib.rs", "h1".to_owned());
        let f2 = fingerprint_observation("src/main.rs", "h1".to_owned());
        assert_ne!(f1, f2);
    }

    #[test]
    fn fingerprint_input_deterministic() {
        let f1 = fingerprint_input("Cargo.toml", "h1".to_owned());
        let f2 = fingerprint_input("Cargo.toml", "h1".to_owned());
        assert_eq!(f1, f2);
    }

    #[test]
    fn fingerprint_input_differs_on_content_change() {
        let f1 = fingerprint_input("Cargo.toml", "h1".to_owned());
        let f2 = fingerprint_input("Cargo.toml", "h2".to_owned());
        assert_ne!(f1, f2);
    }

    #[test]
    fn fingerprint_output_deterministic() {
        let f1 = fingerprint_output("out.txt", "h1".to_owned());
        let f2 = fingerprint_output("out.txt", "h1".to_owned());
        assert_eq!(f1, f2);
    }

    #[test]
    fn fingerprint_output_differs_on_content_change() {
        let f1 = fingerprint_output("out.txt", "h1".to_owned());
        let f2 = fingerprint_output("out.txt", "h2".to_owned());
        assert_ne!(f1, f2);
    }

    #[test]
    fn fingerprint_verification_deterministic() {
        let f1 = fingerprint_verification("test", "h1".to_owned());
        let f2 = fingerprint_verification("test", "h1".to_owned());
        assert_eq!(f1, f2);
    }

    #[test]
    fn fingerprint_verification_differs_on_kind_change() {
        let f1 = fingerprint_verification("test", "h1".to_owned());
        let f2 = fingerprint_verification("clippy", "h1".to_owned());
        assert_ne!(f1, f2);
    }

    #[test]
    fn fingerprint_verification_differs_on_content_change() {
        let f1 = fingerprint_verification("test", "h1".to_owned());
        let f2 = fingerprint_verification("test", "h2".to_owned());
        assert_ne!(f1, f2);
    }

    // ---- work_intent_hash tests ----

    #[test]
    fn work_intent_hash_deterministic() {
        let h1 = work_intent_hash("Implement the Foo feature");
        let h2 = work_intent_hash("Implement the Foo feature");
        assert_eq!(h1, h2);
    }

    #[test]
    fn work_intent_hash_normalizes() {
        // normalize_description lowercases, strips punctuation, collapses whitespace.
        let h1 = work_intent_hash("  Hello,   World!  ");
        let h2 = work_intent_hash("hello world");
        assert_eq!(h1, h2);
    }

    #[test]
    fn work_intent_hash_differs_on_content_change() {
        let h1 = work_intent_hash("add feature X");
        let h2 = work_intent_hash("remove feature X");
        assert_ne!(h1, h2);
    }

    // ---- timeline_contains_hash tests ----

    #[test]
    fn timeline_contains_hash_true_when_matching_entry_exists() {
        let projection = TimelineProjection {
            events: vec![TimelineEvent::WroteFile {
                gate_seq: 1,
                path: "src/lib.rs".to_owned(),
                content_hash: "abc123".to_owned(),
                created_at: 0,
            }],
            checkpoints: Vec::new(),
            plan_artifacts: Vec::new(),
            completed_results: std::collections::HashMap::new(),
        };
        assert!(timeline_contains_hash(&projection, "abc123"));
    }

    #[test]
    fn timeline_contains_hash_false_when_no_match() {
        let projection = TimelineProjection {
            events: vec![TimelineEvent::WroteFile {
                gate_seq: 1,
                path: "src/lib.rs".to_owned(),
                content_hash: "abc123".to_owned(),
                created_at: 0,
            }],
            checkpoints: Vec::new(),
            plan_artifacts: Vec::new(),
            completed_results: std::collections::HashMap::new(),
        };
        assert!(!timeline_contains_hash(&projection, "def456"));
    }

    #[test]
    fn timeline_contains_hash_false_when_empty() {
        let projection = TimelineProjection {
            events: Vec::new(),
            checkpoints: Vec::new(),
            plan_artifacts: Vec::new(),
            completed_results: std::collections::HashMap::new(),
        };
        assert!(!timeline_contains_hash(&projection, "abc123"));
    }

    #[test]
    fn timeline_contains_hash_false_when_content_differs() {
        let projection = TimelineProjection {
            events: vec![TimelineEvent::WroteFile {
                gate_seq: 1,
                path: "src/lib.rs".to_owned(),
                content_hash: "abc123".to_owned(),
                created_at: 0,
            }],
            checkpoints: Vec::new(),
            plan_artifacts: Vec::new(),
            completed_results: std::collections::HashMap::new(),
        };
        // Same path but different content hash.
        assert!(!timeline_contains_hash(&projection, "abc123different"));
    }

    // ---- Dependency key in SemanticKey ----

    #[test]
    fn semantic_key_with_dependencies_deterministic() {
        let deps = vec!["dep-a".to_owned(), "dep-b".to_owned()];
        let k1 = SemanticKey::compute("obj", "plan", "intent", "contract", &deps);
        let k2 = SemanticKey::compute("obj", "plan", "intent", "contract", &deps);
        assert_eq!(k1.hex(), k2.hex());
    }

    #[test]
    fn semantic_key_empty_deps_vs_nonempty_deps_differ() {
        let k_empty = SemanticKey::compute("obj", "plan", "intent", "contract", &[]);
        let k_with =
            SemanticKey::compute("obj", "plan", "intent", "contract", &["dep-a".to_owned()]);
        assert_ne!(k_empty.hex(), k_with.hex());
    }

    // ---- AsRef<str> and Display ----

    #[test]
    fn semantic_key_as_ref_str() {
        let key = sample_key(vec![]);
        let r: &str = key.as_ref();
        assert_eq!(r, key.hex());
    }

    #[test]
    fn semantic_key_display() {
        let key = sample_key(vec![]);
        assert_eq!(format!("{key}"), key.hex());
    }

    // ---- Debug shows components ----

    #[test]
    fn semantic_key_debug_shows_components() {
        let key = sample_key(vec![]);
        let dbg = format!("{key:?}");
        assert!(dbg.contains("SemanticKey("), "Debug starts with SemanticKey(");
        assert!(dbg.contains("obj=obj-abc123"), "Debug shows objective hash");
        assert!(dbg.contains("plan=plan-v1"), "Debug shows plan version");
        assert!(dbg.contains("intent=implement the foo bar"), "Debug shows work intent");
    }
}
