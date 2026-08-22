//! `SubTaskHasher` — produces stable task hashes for cycle detection.
//!
//! Uses blake3 of a normalised description + sorted dependency IDs so that
//! equivalent subtasks produce the same hash.

use concerto_core::types::TaskId;

/// Produces the `task_hash` consumed by `OrchestratorState` cycle detection.
pub struct SubTaskHasher;

impl SubTaskHasher {
    /// Compute a stable hash for a subtask description and its dependencies.
    ///
    /// Normalisation: lowercase, collapse whitespace, strip punctuation.
    pub fn compute(description: &str, dependencies: &[TaskId]) -> String {
        let normalised: String = description
            .to_lowercase()
            .chars()
            .filter(|c| c.is_alphanumeric() || c.is_whitespace())
            .collect::<String>()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");

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
