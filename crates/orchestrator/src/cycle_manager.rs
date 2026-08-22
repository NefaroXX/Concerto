//! Cycle managers for review and validation loops.
//!
//! Tracks how many cycles have been consumed and enforces upper limits.

use concerto_core::types::TaskId;
use concerto_core::OrchestratorError;
use std::collections::HashMap;

/// Tracks review cycle count per task. Max defaults to 3.
pub struct ReviewCycleManager {
    max_cycles: u32,
    cycles: HashMap<TaskId, u32>,
}

impl ReviewCycleManager {
    /// Create with a custom max cycle limit.
    pub fn new(max_cycles: u32) -> Self {
        Self { max_cycles, cycles: HashMap::new() }
    }

    /// Advance the cycle counter. Returns `Err(MaxReviewCyclesExceeded)`
    /// if the limit has been reached.
    pub fn next_cycle(&mut self, task_id: TaskId) -> Result<u32, OrchestratorError> {
        let count = self.cycles.entry(task_id).or_insert(0);
        *count += 1;
        if *count > self.max_cycles {
            Err(OrchestratorError::MaxReviewCyclesExceeded { task_id, cycles: self.max_cycles })
        } else {
            Ok(*count)
        }
    }

    /// Replace the configured cycle limit. Per-task counters are keyed to the
    /// previous limit, so they are cleared.
    pub fn set_max_cycles(&mut self, max_cycles: u32) {
        self.max_cycles = max_cycles;
        self.cycles.clear();
    }

    /// The current cycle limit.
    pub fn max_cycles(&self) -> u32 {
        self.max_cycles
    }

    /// Reset all cycle state.
    pub fn reset(&mut self, task_id: &TaskId) {
        self.cycles.remove(task_id);
    }
}

impl Default for ReviewCycleManager {
    fn default() -> Self {
        Self::new(3)
    }
}

/// Tracks validation cycle count per task. Max defaults to 2.
pub struct ValidationCycleManager {
    max_cycles: u32,
    cycles: HashMap<TaskId, u32>,
}

impl ValidationCycleManager {
    /// Create with a custom max cycle limit.
    pub fn new(max_cycles: u32) -> Self {
        Self { max_cycles, cycles: HashMap::new() }
    }

    /// Advance the cycle counter. Returns `Err(MaxValidationCyclesExceeded)`
    /// if the limit has been reached.
    pub fn next_cycle(&mut self, task_id: TaskId) -> Result<u32, OrchestratorError> {
        let count = self.cycles.entry(task_id).or_insert(0);
        *count += 1;
        if *count > self.max_cycles {
            Err(OrchestratorError::MaxValidationCyclesExceeded { task_id, cycles: self.max_cycles })
        } else {
            Ok(*count)
        }
    }

    /// Replace the configured cycle limit. Per-task counters are keyed to the
    /// previous limit, so they are cleared.
    pub fn set_max_cycles(&mut self, max_cycles: u32) {
        self.max_cycles = max_cycles;
        self.cycles.clear();
    }

    /// The current cycle limit.
    pub fn max_cycles(&self) -> u32 {
        self.max_cycles
    }

    /// Reset all cycle state.
    pub fn reset(&mut self, task_id: &TaskId) {
        self.cycles.remove(task_id);
    }
}

impl Default for ValidationCycleManager {
    fn default() -> Self {
        Self::new(2)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use concerto_core::types::TaskId;

    #[test]
    fn review_cycles_hit_limit() {
        let tid = TaskId::new();
        let mut mgr = ReviewCycleManager::new(3);
        assert!(mgr.next_cycle(tid).is_ok());
        assert!(mgr.next_cycle(tid).is_ok());
        assert!(mgr.next_cycle(tid).is_ok());
        assert!(mgr.next_cycle(tid).is_err());
    }

    #[test]
    fn validation_cycles_hit_limit() {
        let tid = TaskId::new();
        let mut mgr = ValidationCycleManager::new(2);
        assert!(mgr.next_cycle(tid).is_ok());
        assert!(mgr.next_cycle(tid).is_ok());
        assert!(mgr.next_cycle(tid).is_err());
    }

    #[test]
    fn reset_clears_state() {
        let tid = TaskId::new();
        let mut mgr = ReviewCycleManager::new(1);
        assert!(mgr.next_cycle(tid).is_ok());
        assert!(mgr.next_cycle(tid).is_err());
        mgr.reset(&tid);
        assert!(mgr.next_cycle(tid).is_ok());
    }

    #[test]
    fn default_review_limit() {
        let mgr = ReviewCycleManager::default();
        assert_eq!(mgr.max_cycles, 3);
    }

    #[test]
    fn default_validation_limit() {
        let mgr = ValidationCycleManager::default();
        assert_eq!(mgr.max_cycles, 2);
    }

    #[test]
    fn reset_clears_only_specified_task() {
        let tid_a = TaskId::new();
        let tid_b = TaskId::new();
        let mut mgr = ReviewCycleManager::new(1);
        assert!(mgr.next_cycle(tid_a).is_ok());
        assert!(mgr.next_cycle(tid_b).is_ok());
        mgr.reset(&tid_a);
        // tid_a has been reset so its counter is fresh (count=1) -> Ok.
        assert!(mgr.next_cycle(tid_a).is_ok());
        // tid_b was NOT reset; this is its 2nd cycle -> count=2 > max_cycles=1 -> Err.
        assert!(mgr.next_cycle(tid_b).is_err());
    }

    #[test]
    fn independent_task_counters() {
        let tid_a = TaskId::new();
        let tid_b = TaskId::new();
        let mut mgr = ReviewCycleManager::new(1);
        assert!(mgr.next_cycle(tid_a).is_ok());
        assert!(mgr.next_cycle(tid_b).is_ok());
        assert!(mgr.next_cycle(tid_a).is_err());
        assert!(mgr.next_cycle(tid_b).is_err());
    }

    #[test]
    fn set_max_cycles_raises_and_lowers_the_limit() {
        let tid = TaskId::new();
        let mut mgr = ReviewCycleManager::new(1);
        assert!(mgr.next_cycle(tid).is_ok());
        assert!(matches!(
            mgr.next_cycle(tid),
            Err(OrchestratorError::MaxReviewCyclesExceeded { cycles: 1, .. })
        ));
        // Raising the limit clears the per-task counter, so the next cycle is 1.
        mgr.set_max_cycles(3);
        assert!(matches!(mgr.next_cycle(tid), Ok(1)));
        assert!(matches!(mgr.next_cycle(tid), Ok(2)));
        assert!(matches!(mgr.next_cycle(tid), Ok(3)));
        assert!(matches!(
            mgr.next_cycle(tid),
            Err(OrchestratorError::MaxReviewCyclesExceeded { cycles: 3, .. })
        ));

        let tid = TaskId::new();
        let mut mgr = ValidationCycleManager::new(1);
        assert!(mgr.next_cycle(tid).is_ok());
        assert!(matches!(
            mgr.next_cycle(tid),
            Err(OrchestratorError::MaxValidationCyclesExceeded { cycles: 1, .. })
        ));
        // Raising the limit clears the per-task counter, so the next cycle is 1.
        mgr.set_max_cycles(2);
        assert!(matches!(mgr.next_cycle(tid), Ok(1)));
        assert!(matches!(mgr.next_cycle(tid), Ok(2)));
        assert!(matches!(
            mgr.next_cycle(tid),
            Err(OrchestratorError::MaxValidationCyclesExceeded { cycles: 2, .. })
        ));
    }

    #[test]
    fn set_max_cycles_updates_getter() {
        let mut review = ReviewCycleManager::new(1);
        assert_eq!(review.max_cycles(), 1);
        review.set_max_cycles(7);
        assert_eq!(review.max_cycles(), 7);
        review.set_max_cycles(2);
        assert_eq!(review.max_cycles(), 2);

        let mut validation = ValidationCycleManager::new(1);
        assert_eq!(validation.max_cycles(), 1);
        validation.set_max_cycles(5);
        assert_eq!(validation.max_cycles(), 5);
        validation.set_max_cycles(2);
        assert_eq!(validation.max_cycles(), 2);
    }
}
