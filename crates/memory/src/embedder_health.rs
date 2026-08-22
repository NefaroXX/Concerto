//! Per-project embedder health (ADR-39).
//!
//! Tracks a broken state, a consecutive-failure count, and a bounded
//! exponential backoff deadline per project so the indexer and the hybrid
//! search path observe the same health.
//!
//! On the first failure of a window the embedder is marked **broken** and a
//! backoff deadline is scheduled: `min(120s, 5s * 2^consecutive_failures)`.
//! While broken, further embedding attempts are paused (the indexer records
//! chunks FTS-only without a vector). Once the deadline elapses a later
//! attempt resumes; a success clears the broken state and resets the count.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use concerto_core::memory::ProjectId;

/// Human-readable notice attached to degraded (FTS-only) search results.
pub const EMBEDDER_DEGRADED_NOTICE: &str =
    "embedder unavailable \u{2014} semantic search degraded to full-text only";

/// Backoff base delay (first failure window starts at this, i.e. 5s for the
/// very first failure because the delay is computed from the pre-increment
/// consecutive-failure count — see [`EmbedderHealth::record_failure`]).
const BACKOFF_INITIAL_SECS: u64 = 5;
/// Upper bound on the backoff delay.
const BACKOFF_CAP_SECS: u64 = 120;

/// Per-project embedder health shared by the indexer and the search path.
#[derive(Debug)]
pub struct EmbedderHealth {
    inner: Mutex<Health>,
}

#[derive(Debug)]
struct Health {
    consecutive_failures: u32,
    broken: bool,
    backoff_until: Option<Instant>,
}

impl Default for EmbedderHealth {
    fn default() -> Self {
        Self::new()
    }
}

impl EmbedderHealth {
    /// Create a fresh (healthy) handle. Prefer [`Self::for_project`] so the
    /// indexer and the search path share the same state.
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Health {
                consecutive_failures: 0,
                broken: false,
                backoff_until: None,
            }),
        }
    }

    /// Get-or-create the shared handle for a project.
    ///
    /// A process-wide registry is used because the indexer and the vector
    /// store are constructed independently across crates (orchestrator,
    /// watcher), so a single source of per-project truth avoids threading
    /// the handle through every construction site.
    pub fn for_project(project_id: &ProjectId) -> Arc<EmbedderHealth> {
        static REGISTRY: OnceLock<Mutex<HashMap<String, Arc<EmbedderHealth>>>> = OnceLock::new();
        let map = REGISTRY.get_or_init(|| Mutex::new(HashMap::new()));
        let mut guard = map.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.entry(project_id.0.clone()).or_insert_with(|| Arc::new(EmbedderHealth::new())).clone()
    }

    /// Whether embedding attempts should be skipped right now (broken and
    /// within the backoff window).
    pub fn is_broken(&self, now: Instant) -> bool {
        let guard = self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.broken && guard.backoff_until.is_some_and(|until| now < until)
    }

    /// Record a failure. Returns `Some(delay)` only on the first failure of a
    /// (new) broken window — the caller should emit the degraded-state event
    /// exactly once per window. `None` means the embedder was already broken.
    pub fn record_failure(&self, now: Instant) -> Option<Duration> {
        let mut guard = self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let was_broken = guard.broken && guard.backoff_until.is_some_and(|until| now < until);
        // Compute the delay from the CURRENT count, THEN increment, so the
        // first failure of a window yields `backoff_delay_seconds(0) == 5s`
        // (ADR-39 / N1: backoff must start at 5s, not 10s).
        let delay = Duration::from_secs(backoff_delay_seconds(guard.consecutive_failures));
        guard.consecutive_failures = guard.consecutive_failures.saturating_add(1);
        guard.broken = true;
        guard.backoff_until = Some(now + delay);
        if was_broken {
            None
        } else {
            Some(delay)
        }
    }

    /// Record a successful embedding: clears the broken state and resets the
    /// consecutive-failure count. Returns `true` if it was previously broken
    /// (i.e. a recovery occurred).
    pub fn record_success(&self) -> bool {
        let mut guard = self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let was_broken = guard.broken;
        guard.consecutive_failures = 0;
        guard.broken = false;
        guard.backoff_until = None;
        was_broken
    }
}

/// Bounded exponential backoff: `min(cap, initial * 2^consecutive_failures)`.
/// Used by both the live logic and unit tests for the 120s cap.
pub fn backoff_delay_seconds(consecutive_failures: u32) -> u64 {
    let multiplier = 2u64.checked_pow(consecutive_failures).unwrap_or(u64::MAX);
    BACKOFF_INITIAL_SECS.saturating_mul(multiplier).min(BACKOFF_CAP_SECS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_starts_low_and_caps_at_120s() {
        assert_eq!(backoff_delay_seconds(0), 5);
        assert_eq!(backoff_delay_seconds(1), 10);
        assert_eq!(backoff_delay_seconds(2), 20);
        assert_eq!(backoff_delay_seconds(3), 40);
        assert_eq!(backoff_delay_seconds(4), 80);
        assert_eq!(backoff_delay_seconds(5), 120);
        assert_eq!(backoff_delay_seconds(100), 120, "caps at 120s");
    }

    #[test]
    fn consecutive_failures_enter_broken_once_and_emit_once() {
        let health = EmbedderHealth::new();
        let t0 = Instant::now();
        // First failure of a window → transition; backoff starts at 5s (ADR-39
        // / N1: delay is computed from the count BEFORE this failure).
        assert_eq!(health.record_failure(t0), Some(Duration::from_secs(5)));
        assert!(health.is_broken(t0));
        // Already broken → no new window → no event.
        assert!(health.record_failure(t0).is_none());
        assert!(health.record_failure(t0).is_none());
    }

    #[test]
    fn success_clears_broken_and_resets_count() {
        let health = EmbedderHealth::new();
        let t0 = Instant::now();
        assert!(health.record_failure(t0).is_some());
        assert!(health.is_broken(t0));
        assert!(health.record_success(), "recovery should report it was broken");
        assert!(!health.is_broken(t0));
        // Two failures after recovery → count is small again (no long window).
        assert!(health.record_failure(t0).is_some());
    }

    #[test]
    fn window_expiry_allows_retry_and_rebreak_emits_again() {
        let health = EmbedderHealth::new();
        let t0 = Instant::now();
        assert!(health.record_failure(t0).is_some());
        // After the window the embedder is no longer considered broken.
        let t_far = t0 + Duration::from_secs(1000);
        assert!(!health.is_broken(t_far));
        // A fresh failure after an elapsed window is a new window → emit again.
        assert!(health.record_failure(t_far).is_some());
    }
}
