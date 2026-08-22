use concerto_config::RetryConfig;
use concerto_core::error::{PolicyError, ProviderError};
use concerto_core::event::{EventBus, EventKind};
use concerto_core::ids::Ulid;
use concerto_core::types::TaskId;
use concerto_core::CancellationToken;
use concerto_core::RpmLimiter;
use reqwest::header::{HeaderMap, RETRY_AFTER};
use reqwest::StatusCode;
use std::future::Future;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Retry classification
// ---------------------------------------------------------------------------

/// Classification of a transient provider failure. Decouples the retry policy
/// from provider-specific error strings so every provider can funnel its
/// failures through one decision function.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RetryClass {
    RateLimited,
    Overloaded,
    ServiceUnavailable,
    GatewayFailure,
    Network,
    ConnectionReset,
    RequestTimeout,
    StreamIdleTimeout,
}

/// Structured outcome of classifying a [`ProviderError`].
#[derive(Debug, Clone)]
pub struct RetryDecision {
    pub retryable: bool,
    pub class: Option<RetryClass>,
    pub provider_delay: Option<Duration>,
    pub reason: String,
}

/// Decide whether a provider error warrants a retry and (if so) what delay the
/// provider itself asked for.
///
/// Non-retryable conditions (auth failure, permission denied, invalid request,
/// context overflow, malformed/invalid responses, cancellation, spend/policy
/// rejection, and mid-stream idle timeouts after output has begun) return
/// `retryable: false`. Transient conditions (rate limits, 5xx, timeouts before
/// any output, network blips) return `retryable: true`.
pub fn classify_provider_error(error: &ProviderError) -> RetryDecision {
    match error {
        ProviderError::RateLimit { retry_after } => RetryDecision {
            retryable: true,
            class: Some(RetryClass::RateLimited),
            provider_delay: Some(*retry_after),
            reason: "provider rate limit".into(),
        },

        ProviderError::HttpStatus { status, retry_after, .. }
            if matches!(*status, 408 | 429 | 500 | 502 | 503 | 504) =>
        {
            RetryDecision {
                retryable: true,
                class: Some(match *status {
                    408 => RetryClass::RequestTimeout,
                    429 => RetryClass::RateLimited,
                    502 | 504 => RetryClass::GatewayFailure,
                    503 => RetryClass::ServiceUnavailable,
                    _ => RetryClass::Overloaded,
                }),
                provider_delay: *retry_after,
                reason: format!("transient HTTP status {}", *status),
            }
        }

        ProviderError::Network(_) => RetryDecision {
            retryable: true,
            class: Some(RetryClass::Network),
            provider_delay: None,
            reason: "temporary network failure".into(),
        },

        // A `stream-idle` timeout fires only after the first chunk has arrived:
        // the shared collector applies the ttfb deadline to the first chunk and
        // the idle deadline to every subsequent chunk. So an idle timeout means
        // output has already flowed; retrying would recreate the request after
        // partial output and risk divergent/duplicate work. Never retry it.
        ProviderError::Timeout { phase: "stream-idle", .. } => RetryDecision {
            retryable: false,
            class: Some(RetryClass::StreamIdleTimeout),
            provider_delay: None,
            reason: "stream idle timeout after output began; retry could duplicate partial work"
                .into(),
        },

        ProviderError::Timeout { phase, .. } => RetryDecision {
            retryable: true,
            class: Some(RetryClass::RequestTimeout),
            provider_delay: None,
            reason: format!("provider {phase} timeout"),
        },

        ProviderError::Cancelled => RetryDecision {
            retryable: false,
            class: None,
            provider_delay: None,
            reason: "request cancelled".into(),
        },

        _ => RetryDecision {
            retryable: false,
            class: None,
            provider_delay: None,
            reason: error.to_string(),
        },
    }
}

/// Parse a `Retry-After` (or `retry-after-ms`) header from a response.
///
/// Supports:
/// * `retry-after-ms` / `Retry-After-Ms` — integer milliseconds.
/// * `Retry-After` as an integer number of seconds.
/// * `Retry-After` as an HTTP-date (RFC 7231).
///
/// Absurd provider hints are *not* clamped here; the policy clamps to its
/// configured maximum so providers cannot force multi-hour sleeps.
pub fn parse_retry_after(headers: &HeaderMap) -> Option<Duration> {
    const RETRY_AFTER_MS_NAMES: [&str; 2] = ["retry-after-ms", "Retry-After-Ms"];

    for name in RETRY_AFTER_MS_NAMES {
        if let Some(value) = headers.get(name) {
            if let Ok(raw) = value.to_str() {
                if let Ok(ms) = raw.trim().parse::<u64>() {
                    return Some(Duration::from_millis(ms));
                }
            }
        }
    }

    let value = headers.get(RETRY_AFTER)?;
    let raw = value.to_str().ok()?.trim();

    if let Ok(seconds) = raw.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }

    let date = httpdate::parse_http_date(raw).ok()?;
    let now = std::time::SystemTime::now();
    date.duration_since(now).ok()
}

// ---------------------------------------------------------------------------
// Retry policy
// ---------------------------------------------------------------------------

/// Runtime retry policy built from [`RetryConfig`].
///
/// One shared implementation is used by every provider request boundary so
/// behaviour is consistent and not copy-pasted per provider.
#[derive(Debug, Clone, Default)]
pub struct RetryPolicy {
    config: RetryConfig,
    rate_limiter: Option<std::sync::Arc<RpmLimiter>>,
}

/// Per-attempt state passed to [`RetryPolicy::evaluate`].
#[derive(Debug, Clone)]
pub struct RetryState {
    pub attempt: u32,
    pub started_at: Instant,
}

/// Decision returned by [`RetryPolicy::evaluate`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum RetryOutcome {
    /// Retry after `delay`. `attempt` is the attempt that just failed.
    RetryAfter { delay: Duration, attempt: u32, reason: String, source: RetryDelaySource },
    /// Do not retry this error.
    DoNotRetry { reason: String },
    /// The elapsed-time fuse tripped; give up.
    Exhausted { elapsed: Duration, reason: String },
}

/// Where a retry delay came from, for diagnostics/UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RetryDelaySource {
    ProviderHeader,
    FixedOverride,
    ExponentialBackoff,
}

impl RetryPolicy {
    pub fn new(config: RetryConfig) -> Self {
        Self { config, rate_limiter: None }
    }

    pub fn config(&self) -> &RetryConfig {
        &self.config
    }

    pub fn with_rate_limiter(mut self, rate_limiter: std::sync::Arc<RpmLimiter>) -> Self {
        self.rate_limiter = Some(rate_limiter);
        self
    }

    /// Classify an error and decide the next action.
    pub fn evaluate(&self, state: &RetryState, decision: &RetryDecision) -> RetryOutcome {
        if !self.config.enabled {
            return RetryOutcome::DoNotRetry {
                reason: "automatic provider retry is disabled".into(),
            };
        }

        if !decision.retryable {
            return RetryOutcome::DoNotRetry { reason: decision.reason.clone() };
        }

        let elapsed = state.started_at.elapsed();

        if state.attempt >= self.config.max_attempts {
            return RetryOutcome::Exhausted {
                elapsed,
                reason: format!(
                    "{}; maximum attempt count ({}) reached",
                    decision.reason, self.config.max_attempts
                ),
            };
        }

        if let Some(max_seconds) = self.config.max_elapsed_seconds {
            if elapsed >= Duration::from_secs(max_seconds) {
                return RetryOutcome::Exhausted { elapsed, reason: decision.reason.clone() };
            }
        }

        let (delay, source) = if self.config.respect_retry_after {
            if let Some(delay) = decision.provider_delay {
                (delay, RetryDelaySource::ProviderHeader)
            } else {
                self.configured_delay(state.attempt)
            }
        } else {
            self.configured_delay(state.attempt)
        };

        // Clamp absurd provider-supplied delays to the configured maximum.
        let delay = if source == RetryDelaySource::ProviderHeader {
            delay.min(Duration::from_millis(self.config.max_delay_ms))
        } else {
            delay
        };

        if let Some(max_seconds) = self.config.max_elapsed_seconds {
            let max_elapsed = Duration::from_secs(max_seconds);
            if elapsed.saturating_add(delay) >= max_elapsed {
                return RetryOutcome::Exhausted {
                    elapsed,
                    reason: format!(
                        "{}; next retry would exceed the {:?} outage fuse",
                        decision.reason, max_elapsed
                    ),
                };
            }
        }

        RetryOutcome::RetryAfter {
            delay,
            attempt: state.attempt,
            reason: decision.reason.clone(),
            source,
        }
    }

    fn configured_delay(&self, attempt: u32) -> (Duration, RetryDelaySource) {
        if let Some(ms) = self.config.fixed_delay_ms {
            return (Duration::from_millis(ms), RetryDelaySource::FixedOverride);
        }

        let exponent = attempt.saturating_sub(1) as i32;
        let calculated =
            self.config.initial_delay_ms as f64 * self.config.multiplier.powi(exponent);

        // `max` guards against negative multipliers; `min` enforces the ceiling.
        let capped = calculated.min(self.config.max_delay_ms as f64).max(0.0) as u64;

        let delay = Duration::from_millis(capped);
        let delay = if self.config.jitter { apply_jitter(delay) } else { delay };

        (delay, RetryDelaySource::ExponentialBackoff)
    }
}

/// Full-jitter: pick a uniform random delay in `[0, delay]`. Never negative,
/// never exceeds the configured maximum.
fn apply_jitter(delay: Duration) -> Duration {
    let max_ms = delay.as_millis().min(u64::MAX as u128) as u64;

    if max_ms == 0 {
        return Duration::ZERO;
    }

    Duration::from_millis(fastrand::u64(0..=max_ms))
}

// ---------------------------------------------------------------------------
// Retry wrapping at the provider request boundary
// ---------------------------------------------------------------------------

/// Run `operation` with retry/backoff according to `policy`.
///
/// The closure must recreate (or safely resend) the same logical provider
/// request on each attempt — request bodies and streams are reconstructed, and
/// the same session/task/conversation state is preserved. Sleeps are
/// cancellable via `cancel`; provider retries never consume an agent
/// iteration.
///
/// Retry status is published on `bus` (scheduled / recovered / exhausted) using
/// the supplied `session_id` and `task_id` so the UI can show status without
/// flooding conversation history.
pub async fn with_provider_retry<T, F, Fut>(
    policy: &RetryPolicy,
    bus: &EventBus,
    session_id: Ulid,
    task_id: TaskId,
    provider_name: &str,
    cancel: &CancellationToken,
    mut operation: F,
) -> Result<T, ProviderError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, ProviderError>>,
{
    let started_at = Instant::now();
    let mut attempt = 1_u32;

    loop {
        if cancel.is_cancelled() {
            return Err(ProviderError::Cancelled);
        }
        if let Some(rate_limiter) = &policy.rate_limiter {
            rate_limiter.acquire(provider_name, cancel).await.map_err(|error| match error {
                PolicyError::Cancelled => ProviderError::Cancelled,
                other => {
                    ProviderError::Other(format!("provider request scheduler failed: {other}"))
                }
            })?;
        }

        let operation_result = tokio::select! {
            result = operation() => result,
            _ = cancel.cancelled() => return Err(ProviderError::Cancelled),
        };

        match operation_result {
            Ok(value) => {
                if attempt > 1 {
                    let _ = bus.publish_for_session(
                        session_id,
                        task_id.0,
                        EventKind::ProviderRetryRecovered {
                            session_id,
                            task_id,
                            attempts: attempt,
                            elapsed_ms: started_at.elapsed().as_millis() as u64,
                        },
                    );
                }

                return Ok(value);
            }

            Err(error) => {
                let decision = classify_provider_error(&error);
                let state = RetryState { attempt, started_at };
                // Raw provider wait signal (e.g. `Retry-After`), uncapped —
                // distinct from the locally clamped `delay` so "provider said
                // wait 6h" survives in the telemetry event.
                let retry_after_ms = decision.provider_delay.map(|d| d.as_millis() as u64);

                match policy.evaluate(&state, &decision) {
                    RetryOutcome::RetryAfter { delay, attempt: failed_attempt, reason, source } => {
                        let _ = bus.publish_for_session(
                            session_id,
                            task_id.0,
                            EventKind::ProviderRetryScheduled {
                                session_id,
                                task_id,
                                attempt: failed_attempt,
                                delay_ms: delay.as_millis() as u64,
                                reason,
                                source: format!("{source:?}"),
                                retry_after_ms,
                            },
                        );

                        tokio::select! {
                            _ = tokio::time::sleep(delay) => {}
                            _ = cancel.cancelled() => {
                                return Err(ProviderError::Cancelled);
                            }
                        }

                        attempt = failed_attempt.saturating_add(1);
                    }

                    RetryOutcome::DoNotRetry { reason } => {
                        let _ = bus.publish_for_session(
                            session_id,
                            task_id.0,
                            EventKind::ProviderRetryExhausted {
                                session_id,
                                task_id,
                                attempts: attempt,
                                elapsed_ms: started_at.elapsed().as_millis() as u64,
                                reason: reason.clone(),
                                retry_after_ms,
                            },
                        );
                        return Err(error);
                    }

                    RetryOutcome::Exhausted { elapsed, reason } => {
                        let _ = bus.publish_for_session(
                            session_id,
                            task_id.0,
                            EventKind::ProviderRetryExhausted {
                                session_id,
                                task_id,
                                attempts: attempt,
                                elapsed_ms: elapsed.as_millis() as u64,
                                reason: reason.clone(),
                                retry_after_ms,
                            },
                        );
                        return Err(ProviderError::RetryExhausted {
                            attempts: attempt,
                            elapsed,
                            last_error: reason,
                        });
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// HTTP error mapping helpers (used by every provider adapter)
// ---------------------------------------------------------------------------

/// Map an HTTP status code and optional response body to a [`ProviderError`].
///
/// 401/403 → `AuthFailure` (never retry)
/// 429 → `RateLimit` (retry after parsed `Retry-After` header or fallback 5s)
/// 408/500/502/503/504 → `HttpStatus` (retryable transient)
/// everything else → `HttpStatus` (non-retryable, e.g. 400/404)
pub fn map_http_error(
    status: StatusCode,
    body: &str,
    retry_after: Option<Duration>,
) -> ProviderError {
    match status.as_u16() {
        401 | 403 => ProviderError::AuthFailure,
        429 => {
            ProviderError::RateLimit { retry_after: retry_after.unwrap_or(Duration::from_secs(5)) }
        }
        408 | 500 | 502 | 503 | 504 => ProviderError::HttpStatus {
            status: status.as_u16(),
            retry_after,
            message: format!("transient API error {status}: {body}"),
        },
        other => ProviderError::HttpStatus {
            status: other,
            retry_after: None,
            message: format!("API error {status}: {body}"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_retry_uses_initial_delay() {
        let policy = RetryPolicy::new(RetryConfig { jitter: false, ..RetryConfig::default() });
        let state = RetryState { attempt: 1, started_at: Instant::now() };
        let decision = RetryDecision {
            retryable: true,
            class: Some(RetryClass::RateLimited),
            provider_delay: None,
            reason: "rate limited".into(),
        };
        match policy.evaluate(&state, &decision) {
            RetryOutcome::RetryAfter { delay, source, .. } => {
                assert_eq!(delay, Duration::from_millis(2_000));
                assert_eq!(source, RetryDelaySource::ExponentialBackoff);
            }
            other => panic!("expected retry, got {other:?}"),
        }
    }

    #[test]
    fn delay_grows_exponentially() {
        let policy = RetryPolicy::new(RetryConfig { jitter: false, ..RetryConfig::default() });
        let decision = RetryDecision {
            retryable: true,
            class: Some(RetryClass::Overloaded),
            provider_delay: None,
            reason: "overloaded".into(),
        };
        let d1 = configured_delay_of(&policy, 1);
        let d2 = configured_delay_of(&policy, 2);
        let d3 = configured_delay_of(&policy, 3);
        assert_eq!(d1, Duration::from_millis(2_000));
        assert_eq!(d2, Duration::from_millis(4_000));
        assert_eq!(d3, Duration::from_millis(8_000));
        let _ = decision;
    }

    #[test]
    fn delay_is_capped_at_max() {
        let policy = RetryPolicy::new(RetryConfig {
            jitter: false,
            initial_delay_ms: 2_000,
            max_delay_ms: 5_000,
            multiplier: 2.0,
            ..RetryConfig::default()
        });
        let decision = RetryDecision {
            retryable: true,
            class: Some(RetryClass::Overloaded),
            provider_delay: None,
            reason: "overloaded".into(),
        };
        // attempt 5 -> 2*2^4 = 32s, clamped to 5s.
        let d = configured_delay_of(&policy, 5);
        assert_eq!(d, Duration::from_millis(5_000));
        let _ = decision;
    }

    #[test]
    fn fixed_delay_overrides_exponential_backoff() {
        let policy = RetryPolicy::new(RetryConfig {
            fixed_delay_ms: Some(10_000),
            jitter: false,
            ..RetryConfig::default()
        });
        let state = RetryState { attempt: 7, started_at: Instant::now() };
        let decision = RetryDecision {
            retryable: true,
            class: Some(RetryClass::RateLimited),
            provider_delay: None,
            reason: "rate limited".into(),
        };
        match policy.evaluate(&state, &decision) {
            RetryOutcome::RetryAfter { delay, source, .. } => {
                assert_eq!(delay, Duration::from_secs(10));
                assert_eq!(source, RetryDelaySource::FixedOverride);
            }
            other => panic!("expected retry, got {other:?}"),
        }
    }

    #[test]
    fn provider_retry_after_overrides_fixed_delay() {
        let policy = RetryPolicy::new(RetryConfig {
            fixed_delay_ms: Some(10_000),
            respect_retry_after: true,
            jitter: false,
            ..RetryConfig::default()
        });
        let state = RetryState { attempt: 1, started_at: Instant::now() };
        let decision = RetryDecision {
            retryable: true,
            class: Some(RetryClass::RateLimited),
            provider_delay: Some(Duration::from_secs(3)),
            reason: "rate limited".into(),
        };
        match policy.evaluate(&state, &decision) {
            RetryOutcome::RetryAfter { delay, source, .. } => {
                assert_eq!(delay, Duration::from_secs(3));
                assert_eq!(source, RetryDelaySource::ProviderHeader);
            }
            other => panic!("expected retry, got {other:?}"),
        }
    }

    #[test]
    fn fixed_delay_overrides_provider_when_respect_false() {
        let policy = RetryPolicy::new(RetryConfig {
            fixed_delay_ms: Some(60_000),
            respect_retry_after: false,
            jitter: false,
            ..RetryConfig::default()
        });
        let state = RetryState { attempt: 1, started_at: Instant::now() };
        let decision = RetryDecision {
            retryable: true,
            class: Some(RetryClass::RateLimited),
            provider_delay: Some(Duration::from_secs(3)),
            reason: "rate limited".into(),
        };
        match policy.evaluate(&state, &decision) {
            RetryOutcome::RetryAfter { delay, source, .. } => {
                assert_eq!(delay, Duration::from_secs(60));
                assert_eq!(source, RetryDelaySource::FixedOverride);
            }
            other => panic!("expected retry, got {other:?}"),
        }
    }

    #[test]
    fn non_retryable_returns_do_not_retry() {
        let policy = RetryPolicy::new(RetryConfig::default());
        let state = RetryState { attempt: 1, started_at: Instant::now() };
        let decision = classify_provider_error(&ProviderError::AuthFailure);
        assert!(!decision.retryable);
        match policy.evaluate(&state, &decision) {
            RetryOutcome::DoNotRetry { .. } => {}
            other => panic!("expected do-not-retry, got {other:?}"),
        }
    }

    #[test]
    fn elapsed_fuse_returns_exhausted() {
        let policy = RetryPolicy::new(RetryConfig {
            max_elapsed_seconds: Some(0),
            ..RetryConfig::default()
        });
        let state = RetryState { attempt: 1, started_at: Instant::now() };
        let decision = RetryDecision {
            retryable: true,
            class: Some(RetryClass::Overloaded),
            provider_delay: None,
            reason: "overloaded".into(),
        };
        match policy.evaluate(&state, &decision) {
            RetryOutcome::Exhausted { .. } => {}
            other => panic!("expected exhausted, got {other:?}"),
        }
    }

    #[test]
    fn jitter_stays_within_bounds() {
        let policy = RetryPolicy::new(RetryConfig {
            jitter: true,
            initial_delay_ms: 1_000,
            max_delay_ms: 1_000,
            ..RetryConfig::default()
        });
        for _ in 0..1_000 {
            let (delay, _) = policy.configured_delay(1);
            assert!(delay <= Duration::from_millis(1_000));
        }
    }

    #[test]
    fn attempt_arithmetic_does_not_overflow() {
        let policy = RetryPolicy::new(RetryConfig::default());
        let state = RetryState { attempt: u32::MAX, started_at: Instant::now() };
        let decision = RetryDecision {
            retryable: true,
            class: Some(RetryClass::Overloaded),
            provider_delay: None,
            reason: "overloaded".into(),
        };
        // Must not panic / wrap; attempt+1 saturates.
        let _ = policy.evaluate(&state, &decision);
    }

    #[test]
    fn classify_http_status_503_retryable() {
        let d = classify_provider_error(&ProviderError::HttpStatus {
            status: 503,
            retry_after: None,
            message: "unavailable".into(),
        });
        assert!(d.retryable);
        assert_eq!(d.class, Some(RetryClass::ServiceUnavailable));
    }

    #[test]
    fn classify_http_status_400_not_retryable() {
        let d = classify_provider_error(&ProviderError::HttpStatus {
            status: 400,
            retry_after: None,
            message: "bad request".into(),
        });
        assert!(!d.retryable);
    }

    #[test]
    fn classify_network_retryable() {
        let d = classify_provider_error(&ProviderError::Network("conn reset".into()));
        assert!(d.retryable);
        assert_eq!(d.class, Some(RetryClass::Network));
    }

    #[test]
    fn classify_ttfb_timeout_retryable() {
        let d = classify_provider_error(&ProviderError::Timeout {
            phase: "time-to-first-byte",
            timeout: Duration::from_secs(60),
        });
        assert!(d.retryable, "ttfb timeout should be retryable");
        assert_eq!(d.class, Some(RetryClass::RequestTimeout));
    }

    #[test]
    fn classify_stream_idle_timeout_not_retryable() {
        let d = classify_provider_error(&ProviderError::Timeout {
            phase: "stream-idle",
            timeout: Duration::from_secs(120),
        });
        assert!(!d.retryable, "stream-idle timeout must not be retryable");
        assert_eq!(d.class, Some(RetryClass::StreamIdleTimeout));
        assert!(d.reason.contains("duplicate"));
    }

    #[test]
    fn ttfb_timeout_policy_retries() {
        let policy = RetryPolicy::new(RetryConfig { jitter: false, ..RetryConfig::default() });
        let state = RetryState { attempt: 1, started_at: Instant::now() };
        let decision = classify_provider_error(&ProviderError::Timeout {
            phase: "time-to-first-byte",
            timeout: Duration::from_secs(60),
        });
        assert!(decision.retryable);
        match policy.evaluate(&state, &decision) {
            RetryOutcome::RetryAfter { .. } => {}
            other => panic!("expected retry, got {other:?}"),
        }
    }

    #[test]
    fn stream_idle_timeout_policy_do_not_retry() {
        let policy = RetryPolicy::new(RetryConfig::default());
        let state = RetryState { attempt: 1, started_at: Instant::now() };
        let decision = classify_provider_error(&ProviderError::Timeout {
            phase: "stream-idle",
            timeout: Duration::from_secs(120),
        });
        assert!(!decision.retryable);
        match policy.evaluate(&state, &decision) {
            RetryOutcome::DoNotRetry { .. } => {}
            other => panic!("expected do-not-retry, got {other:?}"),
        }
    }

    /// Helper to read the configured (non-provider) delay for an attempt.
    fn configured_delay_of(policy: &RetryPolicy, attempt: u32) -> Duration {
        let (delay, _) = policy.configured_delay(attempt);
        delay
    }

    /// 429 (Rate Limit) should be retryable with RateLimited class.
    #[test]
    fn classify_http_status_429_rate_limit_retryable() {
        let d = classify_provider_error(&ProviderError::HttpStatus {
            status: 429,
            retry_after: Some(std::time::Duration::from_secs(5)),
            message: "too many requests".into(),
        });
        assert!(d.retryable, "429 should be retryable");
        assert_eq!(d.class, Some(RetryClass::RateLimited));
    }
}
