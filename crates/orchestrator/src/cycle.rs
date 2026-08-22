//! Cycle budget tracker — detects repeated identical tool calls.
//!
//! Prevents the agent from getting stuck in a loop by firing a
//! `CycleDetected` error when the same (tool_name, input_hash) pair
//! appears `limit` times in a row.

use concerto_core::OrchestratorError;
use std::collections::HashMap;

/// Tracks how many times each (tool, input hash) pair has been called.
#[derive(Debug, Clone)]
pub struct CycleBudgetTracker {
    /// Maximum identical calls before cycle detection fires.
    limit: u32,
    /// Map of (tool_name, input_hash) → call count.
    calls: HashMap<(String, String), u32>,
}

impl CycleBudgetTracker {
    /// Create a new tracker with the given limit.
    /// Default limit should be 3.
    pub fn new(limit: u32) -> Self {
        Self { limit, calls: HashMap::new() }
    }

    /// Record a tool call. Returns `CycleDetected` if the same
    /// (tool_name, input_hash) pair has been seen `limit` or more times.
    pub fn record(&mut self, tool_name: &str, input_hash: &str) -> Result<(), OrchestratorError> {
        let key = (tool_name.to_string(), input_hash.to_string());
        let count = self.calls.entry(key.clone()).or_insert(0);
        *count += 1;

        if *count >= self.limit {
            return Err(OrchestratorError::CycleDetected {
                tool_name: tool_name.to_string(),
                count: *count,
            });
        }

        Ok(())
    }

    /// Reset the tracker for a new task.
    pub fn reset(&mut self) {
        self.calls.clear();
    }

    /// Returns the number of unique (tool, input) pairs tracked.
    pub fn len(&self) -> usize {
        self.calls.len()
    }

    /// Returns true if no calls have been tracked.
    pub fn is_empty(&self) -> bool {
        self.calls.is_empty()
    }
}

impl Default for CycleBudgetTracker {
    fn default() -> Self {
        Self::new(3)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_call_passes() {
        let mut tracker = CycleBudgetTracker::new(3);
        assert!(tracker.record("shell", "hash1").is_ok());
    }

    #[test]
    fn second_call_with_same_input_passes() {
        let mut tracker = CycleBudgetTracker::new(3);
        tracker.record("shell", "hash1").unwrap();
        assert!(tracker.record("shell", "hash1").is_ok());
    }

    #[test]
    fn third_call_with_same_input_fires_cycle_detected() {
        let mut tracker = CycleBudgetTracker::new(3);
        tracker.record("shell", "hash1").unwrap();
        tracker.record("shell", "hash1").unwrap();
        let err = tracker.record("shell", "hash1").unwrap_err();
        assert!(matches!(err, OrchestratorError::CycleDetected { .. }));
    }

    #[test]
    fn different_inputs_dont_trigger() {
        let mut tracker = CycleBudgetTracker::new(3);
        tracker.record("shell", "hash1").unwrap();
        tracker.record("shell", "hash2").unwrap();
        tracker.record("shell", "hash3").unwrap();
        // Each is seen once — no cycle
        assert!(tracker.record("shell", "hash1").is_ok());
    }

    #[test]
    fn different_tools_dont_interfere() {
        let mut tracker = CycleBudgetTracker::new(3);
        tracker.record("shell", "hash1").unwrap();
        tracker.record("filesystem", "hash1").unwrap();
        tracker.record("shell", "hash1").unwrap();
        // shell:hash1 seen twice, filesystem:hash1 seen twice — each at count=2, no cycle
        assert!(tracker.record("filesystem", "hash1").is_ok());
    }

    #[test]
    fn reset_clears_state() {
        let mut tracker = CycleBudgetTracker::new(3);
        tracker.record("shell", "hash1").unwrap();
        tracker.record("shell", "hash1").unwrap();
        tracker.reset();
        assert!(tracker.is_empty());
        assert!(tracker.record("shell", "hash1").is_ok());
    }

    #[test]
    fn default_limit_is_3() {
        let tracker = CycleBudgetTracker::default();
        assert_eq!(tracker.limit, 3);
    }
}
