//! Content-addressed hashing for cycle detection and semantic identity.
//!
//! [`SubTaskHasher`] produces stable task hashes for cycle detection using
//! blake3 of a normalised description + sorted dependency IDs.
//!
//! [`normalize_description`] is the shared canonicalization used by both
//! `SubTaskHasher` and `crate::fingerprint` (semantic keys / work-intent
//! hashing).  Its contract is fixed: lowercase, strip non-alphanumeric
//! characters, collapse whitespace to single spaces, trim.

use concerto_core::types::TaskId;

/// Canonicalize a text description for hashing.
///
/// **Contract** (stable — `crate::fingerprint` depends on this):
/// 1. Lowercase the entire string.
/// 2. Strip every character that is not alphanumeric and not whitespace.
/// 3. Collapse runs of whitespace into a single ASCII space.
/// 4. Trim leading/trailing whitespace.
///
/// Two descriptions that differ only in capitalization, punctuation, or
/// extraneous whitespace produce the same normalised form.
pub fn normalize_description(description: &str) -> String {
    description
        .to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace())
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Produces the `task_hash` consumed by `OrchestratorState` cycle detection.
pub struct SubTaskHasher;

impl SubTaskHasher {
    /// Compute a stable hash for a subtask description and its dependencies.
    ///
    /// Uses [`normalize_description`] then blake3 of `"normalised|deps"`.
    pub fn compute(description: &str, dependencies: &[TaskId]) -> String {
        let normalised = normalize_description(description);

        let mut dep_ids: Vec<String> = dependencies.iter().map(|d| d.to_string()).collect();
        dep_ids.sort();

        let hash_input = format!("{}|{}", normalised, dep_ids.join(","));
        let hash = blake3::hash(hash_input.as_bytes());
        hash.to_hex()[..16].to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_inputs_same_hash() {
        let deps = [TaskId::new(), TaskId::new()];
        let h1 = SubTaskHasher::compute("Add feature X", &deps);
        let h2 = SubTaskHasher::compute("Add feature X", &deps);
        assert_eq!(h1, h2);
    }

    #[test]
    fn whitespace_normalised() {
        let deps = [TaskId::new()];
        let h1 = SubTaskHasher::compute("Add   feature   X", &deps);
        let h2 = SubTaskHasher::compute("Add feature X", &deps);
        assert_eq!(h1, h2);
    }

    #[test]
    fn different_descriptions_different_hash() {
        let deps = [TaskId::new()];
        let h1 = SubTaskHasher::compute("Add feature X", &deps);
        let h2 = SubTaskHasher::compute("Remove feature X", &deps);
        assert_ne!(h1, h2);
    }
}
