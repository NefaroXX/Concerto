//! Per-client request rate limiting for the API server.
//!
//! Closes threat gap #4 (`docs/security-threat-model.md` §6: "No Rate Limiting
//! for API Server") with a fixed-window counter per client IP, applied as an
//! axum middleware layer around the whole router.
//!
//! The limiter is opt-in: it is disabled unless `CONCERTO_API_RATE_LIMIT` is
//! set, so an unconfigured server behaves exactly as before.
//!
//! Configuration follows this crate's existing `CONCERTO_API_*` env-var
//! convention:
//!
//! | Variable | Meaning | Default |
//! |----------|---------|---------|
//! | `CONCERTO_API_RATE_LIMIT` | requests allowed per client per window; unset, empty, `0`, or malformed ⇒ disabled | disabled |
//! | `CONCERTO_API_RATE_LIMIT_WINDOW_SECS` | window length in seconds | `60` |

use axum::{
    extract::{ConnectInfo, Request, State},
    http::{header, HeaderValue, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use std::{
    collections::HashMap,
    net::{IpAddr, Ipv6Addr, SocketAddr},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

/// Enables rate limiting: maximum requests per client per window.
///
/// Unset (the default), empty, `0`, or malformed ⇒ throttling disabled.
pub const RATE_LIMIT_ENV: &str = "CONCERTO_API_RATE_LIMIT";

/// Overrides the window length in seconds (must be ≥ 1).
pub const RATE_LIMIT_WINDOW_ENV: &str = "CONCERTO_API_RATE_LIMIT_WINDOW_SECS";

/// Window length used when `RATE_LIMIT_WINDOW_ENV` is unset or invalid.
const DEFAULT_WINDOW_SECS: u64 = 60;

/// Once this many clients are tracked, expired windows are pruned. Bounds
/// memory under a many-source-address flood.
///
/// This is a *threshold*, not a strict cap: the map is pruned when it reaches
/// this size, so it can hold the clients active in the current window plus up
/// to this many more (a sustained flood of distinct keys can exceed it before
/// the next prune). It is a memory-safety backstop, not a hard ceiling on
/// distinct clients.
const PRUNE_THRESHOLD: usize = 4096;

/// Path that never consumes budget — mirrors the health bypass in
/// [`crate::auth::auth_layer`] so liveness probes cannot be starved.
const HEALTH_PATH: &str = "/v1/health";

/// Rate-limit configuration for one server process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateLimitConfig {
    /// Maximum requests a single client may send per window.
    pub max_requests: u32,
    /// Length of the counting window, in seconds.
    pub window_secs: u64,
}

impl RateLimitConfig {
    /// Reads configuration from the environment; `None` means disabled.
    ///
    /// Every unusable value (unset, empty, `0`, non-numeric, window `< 1`)
    /// degrades to "unthrottled" instead of failing startup: a broken throttle
    /// setting must not take the API down, and the pre-existing behaviour of
    /// the server is to accept every request.
    pub fn from_env() -> Option<Self> {
        let raw = std::env::var(RATE_LIMIT_ENV).ok()?;
        let raw = raw.trim();
        if raw.is_empty() {
            return None;
        }
        let max_requests: u32 = match raw.parse() {
            Ok(0) => return None,
            Ok(value) => value,
            Err(_) => {
                tracing::warn!(
                    value = raw,
                    "malformed {RATE_LIMIT_ENV}; rate limiting disabled (fail-open)"
                );
                return None;
            }
        };
        let window_secs = std::env::var(RATE_LIMIT_WINDOW_ENV)
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .filter(|secs| *secs > 0)
            .unwrap_or(DEFAULT_WINDOW_SECS);
        Some(Self { max_requests, window_secs })
    }
}

/// Verdict for one checked request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// The request fits in the client's current window.
    Allowed,
    /// The client exhausted its window; `retry_after_secs` is the time left
    /// before the window reopens.
    Denied { retry_after_secs: u64 },
}

/// Limiter-state failure (currently: a poisoned counter lock).
#[derive(Debug, thiserror::Error)]
#[error("rate limiter state unavailable: {0}")]
pub struct LimiterError(String);

/// Fixed-window per-client request counter.
///
/// A fixed window rather than a token bucket to match the stated requirement
/// ("requests per window") and to mirror `RpmLimiter` in `concerto-core`, which
/// uses the same counting model.
#[derive(Debug)]
pub struct RateLimiter {
    config: RateLimitConfig,
    buckets: Mutex<HashMap<String, Bucket>>,
}

/// Per-client counter for the window that is currently open.
#[derive(Debug, Clone, Copy)]
struct Bucket {
    /// Requests counted in the current window.
    used: u32,
    /// When the current window opened.
    window_start: Instant,
}

impl RateLimiter {
    /// Create a limiter for `config`.
    pub fn new(config: RateLimitConfig) -> Self {
        Self { config, buckets: Mutex::new(HashMap::new()) }
    }

    /// Count one request from `client` and decide whether it may proceed.
    ///
    /// Denied requests do not consume budget, so hammering the server never
    /// pushes a client's own window forward.
    pub fn check(&self, client: &str) -> Result<Decision, LimiterError> {
        let mut buckets = self.buckets.lock().map_err(|error| LimiterError(error.to_string()))?;
        let window = Duration::from_secs(self.config.window_secs);
        let now = Instant::now();

        // Keep the map bounded: past the prune threshold, drop windows that
        // already closed. No unbounded growth over process lifetime.
        if buckets.len() >= PRUNE_THRESHOLD {
            buckets.retain(|_, bucket| now.saturating_duration_since(bucket.window_start) < window);
        }

        let bucket =
            buckets.entry(client.to_string()).or_insert(Bucket { used: 0, window_start: now });
        if now.saturating_duration_since(bucket.window_start) >= window {
            bucket.used = 0;
            bucket.window_start = now;
        }

        if bucket.used >= self.config.max_requests {
            let remaining =
                window.saturating_sub(now.saturating_duration_since(bucket.window_start));
            return Ok(Decision::Denied { retry_after_secs: ceil_secs(remaining) });
        }

        bucket.used += 1;
        Ok(Decision::Allowed)
    }
}

/// Whole seconds to wait, rounded up so a client honouring `Retry-After` is
/// never told to retry before the window actually reopens (minimum 1s).
fn ceil_secs(duration: Duration) -> u64 {
    (duration.as_secs() + u64::from(duration.subsec_nanos() > 0)).max(1)
}

/// Shared limiter state attached to the router with `from_fn_with_state`.
#[derive(Clone, Default)]
pub struct RateLimitState {
    /// `None` (the default) disables throttling entirely.
    limiter: Option<Arc<RateLimiter>>,
}

impl RateLimitState {
    /// Throttling off: every request passes.
    pub fn disabled() -> Self {
        Self { limiter: None }
    }

    /// Throttling on with an explicit configuration.
    pub fn enabled(config: RateLimitConfig) -> Self {
        Self { limiter: Some(Arc::new(RateLimiter::new(config))) }
    }

    /// Build from the environment; unthrottled when disabled.
    ///
    /// Logs the effective configuration so operators can see whether the
    /// server is protected (startup `info`) — request-level throttling is
    /// logged at `warn` by the middleware.
    pub fn from_env() -> Self {
        match RateLimitConfig::from_env() {
            Some(config) => {
                tracing::info!(
                    max_requests = config.max_requests,
                    window_secs = config.window_secs,
                    "per-client rate limiting enabled"
                );
                Self::enabled(config)
            }
            None => {
                tracing::debug!("per-client rate limiting disabled");
                Self::disabled()
            }
        }
    }
}

/// Axum middleware enforcing the per-client limit (layered around the whole
/// router, so it runs before [`crate::auth::auth_layer`]).
///
/// Behaviour notes:
/// - Requests with no peer address (a router built without
///   `into_make_service_with_connect_info`) are not throttled: without an IP
///   there is no per-client isolation, and a shared fallback bucket would let
///   one unidentifiable client starve everyone else.
/// - `/v1/health` bypasses the limiter so probes cannot be starved.
///
/// **Failure policy: fail-open.** If limiter state is unavailable the request
/// proceeds with a `warn`. Rate limiting here is an availability safeguard,
/// not a security boundary — the request would have been legitimate either
/// way — so failing closed would turn a bookkeeping fault into a self-inflicted
/// outage. This is deliberately the opposite of the fail-closed `RpmLimiter`
/// in `concerto-core`, which gates provider spend and write operations where a
/// stale *allow* is the costly error.
pub async fn rate_limit_layer(
    State(state): State<RateLimitState>,
    req: Request,
    next: Next,
) -> Response {
    let Some(limiter) = state.limiter.clone() else {
        // Disabled configuration: identical to the unthrottled server.
        return next.run(req).await;
    };
    if req.uri().path() == HEALTH_PATH {
        return next.run(req).await;
    }
    let Some(client) = client_key(&req) else {
        return next.run(req).await;
    };

    match limiter.check(&client) {
        Ok(Decision::Allowed) => next.run(req).await,
        Ok(Decision::Denied { retry_after_secs }) => {
            // Observability without PII: the client address is deliberately
            // absent from the log (IPs are personal data); the counters below
            // show that throttling is active and how hard it is hitting.
            tracing::warn!(
                max_requests = limiter.config.max_requests,
                window_secs = limiter.config.window_secs,
                retry_after_secs,
                "rate limit exceeded: request throttled with 429"
            );
            throttled_response(retry_after_secs)
        }
        Err(error) => {
            // Fail open (see the doc comment on this function).
            tracing::warn!("rate limiter unavailable; allowing request ({error})");
            next.run(req).await
        }
    }
}

/// Client key: the direct peer IP, or `None` when the server was built without
/// connect info.
///
/// Proxy headers such as `X-Forwarded-For` are deliberately ignored: they are
/// client-controlled, so honouring them would let an attacker rotate keys and
/// sidestep the limiter. The port is excluded so a client that reconnects
/// (new ephemeral port) shares one bucket.
///
/// IPv6 peers are keyed on their /64 network prefix, not the full address: a
/// single client is routinely delegated a /64 (or larger) and can rotate the
/// low 64 bits for free, which would otherwise let it sidestep the limit. The
/// trade-off is that distinct clients inside the same /64 share one bucket —
/// which matches how IPv6 allocations are actually made. IPv4 keeps its full
/// address (its allocations are not made in large free-to-rotate blocks).
fn client_key(req: &Request) -> Option<String> {
    let ip = req.extensions().get::<ConnectInfo<SocketAddr>>()?.0.ip();
    Some(rate_limit_key(ip))
}

/// Canonical bucket key for `ip`: unchanged for IPv4, /64-aggregated for IPv6.
fn rate_limit_key(ip: IpAddr) -> String {
    match ip {
        IpAddr::V4(v4) => v4.to_string(),
        IpAddr::V6(v6) => {
            // Zero the interface identifier, keeping the /64 network prefix.
            let mut prefix = [0u8; 16];
            prefix[..8].copy_from_slice(&v6.octets()[..8]);
            format!("{}/64", Ipv6Addr::from(prefix))
        }
    }
}

/// 429 with `Retry-After`.
fn throttled_response(retry_after_secs: u64) -> Response {
    let mut response = (StatusCode::TOO_MANY_REQUESTS, "rate limit exceeded").into_response();
    // Derived from integer arithmetic, so this cannot fail in practice; a
    // missing header would degrade advice, not correctness.
    if let Ok(value) = HeaderValue::from_str(&retry_after_secs.to_string()) {
        response.headers_mut().insert(header::RETRY_AFTER, value);
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request, middleware::from_fn_with_state, routing::get, Router};
    use std::net::Ipv4Addr;
    use std::sync::PoisonError;
    use tower::ServiceExt;

    /// Serialises access to the `CONCERTO_API_RATE_LIMIT*` env vars across
    /// tests in this module (the vars are process-global).
    static RATE_LIMIT_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    const TEST_CONFIG: RateLimitConfig = RateLimitConfig { max_requests: 3, window_secs: 60 };

    fn test_router(state: RateLimitState) -> Router {
        async fn ok_handler() -> &'static str {
            "OK"
        }
        Router::new()
            .route("/test", get(ok_handler))
            .route(HEALTH_PATH, get(ok_handler))
            .layer(from_fn_with_state(state, rate_limit_layer))
    }

    /// Request to `uri` as if it came from `<a>.<b>.<c>.<d>:54321`.
    fn client_request(uri: &str, a: u8, b: u8, c: u8, d: u8) -> Request<Body> {
        let mut req = Request::builder().uri(uri).body(Body::empty()).expect("static body");
        let addr = SocketAddr::from((Ipv4Addr::new(a, b, c, d), 54321));
        req.extensions_mut().insert(ConnectInfo(addr));
        req
    }

    async fn send(app: &Router, uri: &str, a: u8, b: u8, c: u8, d: u8) -> Response {
        app.clone().oneshot(client_request(uri, a, b, c, d)).await.expect("request")
    }

    /// Request to `uri` as if it came from `addr` (`:54321`).
    fn client_request_v6(uri: &str, addr: Ipv6Addr) -> Request<Body> {
        let mut req = Request::builder().uri(uri).body(Body::empty()).expect("static body");
        let addr = SocketAddr::new(IpAddr::V6(addr), 54321);
        req.extensions_mut().insert(ConnectInfo(addr));
        req
    }

    async fn send_v6(app: &Router, uri: &str, addr: Ipv6Addr) -> Response {
        app.clone().oneshot(client_request_v6(uri, addr)).await.expect("request")
    }

    /// Requests at or below the limit pass through to the handler.
    #[test]
    fn under_limit_requests_pass() {
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        let app = test_router(RateLimitState::enabled(TEST_CONFIG));
        rt.block_on(async {
            for _ in 0..TEST_CONFIG.max_requests {
                let response = send(&app, "/test", 192, 0, 2, 1).await;
                assert_eq!(response.status(), StatusCode::OK);
            }
        });
    }

    /// The next request past the limit is rejected with 429 and a usable
    /// `Retry-After` header.
    #[test]
    fn over_limit_returns_429_with_retry_after() {
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        let app = test_router(RateLimitState::enabled(TEST_CONFIG));
        rt.block_on(async {
            for _ in 0..TEST_CONFIG.max_requests {
                let response = send(&app, "/test", 192, 0, 2, 2).await;
                assert_eq!(response.status(), StatusCode::OK);
            }

            let response = send(&app, "/test", 192, 0, 2, 2).await;
            assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
            let retry_after = response
                .headers()
                .get(header::RETRY_AFTER)
                .expect("Retry-After header on 429")
                .to_str()
                .expect("Retry-After is ASCII")
                .parse::<u64>()
                .expect("Retry-After is a whole number of seconds");
            assert!(retry_after >= 1, "Retry-After must give the client a positive delay");
            assert!(
                retry_after <= TEST_CONFIG.window_secs,
                "Retry-After must not exceed the window"
            );

            // A denied request does not consume budget, so the counter is
            // still exactly at the limit (still denied, not erroring).
            let response = send(&app, "/test", 192, 0, 2, 2).await;
            assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        });
    }

    /// One client's flood must not throttle a different client.
    #[test]
    fn per_client_isolation() {
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        let app = test_router(RateLimitState::enabled(RateLimitConfig {
            max_requests: 1,
            window_secs: 60,
        }));
        rt.block_on(async {
            // Flood from client A until throttled.
            assert_eq!(send(&app, "/test", 198, 51, 100, 1).await.status(), StatusCode::OK);
            assert_eq!(
                send(&app, "/test", 198, 51, 100, 1).await.status(),
                StatusCode::TOO_MANY_REQUESTS
            );

            // Client B is untouched by A's flood: it gets its full budget of
            // one request (a shared bucket would throttle it immediately),
            // and is then limited only by its own window.
            assert_eq!(send(&app, "/test", 198, 51, 100, 2).await.status(), StatusCode::OK);
            assert_eq!(
                send(&app, "/test", 198, 51, 100, 2).await.status(),
                StatusCode::TOO_MANY_REQUESTS
            );

            // And A stays throttled.
            assert_eq!(
                send(&app, "/test", 198, 51, 100, 1).await.status(),
                StatusCode::TOO_MANY_REQUESTS
            );
        });
    }

    /// Two IPv6 addresses in the same /64 share one bucket, so a client cannot
    /// rotate the low 64 bits to sidestep the limit.
    #[test]
    fn ipv6_addresses_in_same_64_share_a_bucket() {
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        let app = test_router(RateLimitState::enabled(RateLimitConfig {
            max_requests: 1,
            window_secs: 60,
        }));
        let a: Ipv6Addr = "2001:db8:1:2::1".parse().expect("v6 addr");
        let b: Ipv6Addr = "2001:db8:1:2:ffff:ffff:ffff:ffff".parse().expect("v6 addr");
        rt.block_on(async {
            assert_eq!(send_v6(&app, "/test", a).await.status(), StatusCode::OK);
            assert_eq!(
                send_v6(&app, "/test", b).await.status(),
                StatusCode::TOO_MANY_REQUESTS,
                "a rotation within the same /64 must share the bucket"
            );
        });
    }

    /// Distinct /64 prefixes stay isolated: one client's flood must not throttle
    /// another network.
    #[test]
    fn distinct_ipv6_64_prefixes_are_isolated() {
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        let app = test_router(RateLimitState::enabled(RateLimitConfig {
            max_requests: 1,
            window_secs: 60,
        }));
        let a: Ipv6Addr = "2001:db8:1:2::1".parse().expect("v6 addr");
        let b: Ipv6Addr = "2001:db8:1:3::1".parse().expect("v6 addr");
        rt.block_on(async {
            assert_eq!(send_v6(&app, "/test", a).await.status(), StatusCode::OK);
            assert_eq!(send_v6(&app, "/test", a).await.status(), StatusCode::TOO_MANY_REQUESTS);
            // A different /64 gets its full budget.
            assert_eq!(send_v6(&app, "/test", b).await.status(), StatusCode::OK);
            assert_eq!(send_v6(&app, "/test", b).await.status(), StatusCode::TOO_MANY_REQUESTS);
        });
    }

    /// IPv4 keying is unchanged: the full address, not an aggregate.
    #[test]
    fn ipv4_client_key_is_the_full_address() {
        let key = rate_limit_key(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1)));
        assert_eq!(key, "192.0.2.1");
        // Distinct IPv4 addresses in the same /24 remain distinct buckets.
        assert_ne!(key, rate_limit_key(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 2))));
    }

    /// With the config disabled (env unset — the default), every request
    /// passes regardless of how often a single client hammers the server.
    #[test]
    fn disabled_config_passes_everything() {
        let _lock = RATE_LIMIT_ENV_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        std::env::remove_var(RATE_LIMIT_ENV);
        std::env::remove_var(RATE_LIMIT_WINDOW_ENV);

        assert_eq!(RateLimitConfig::from_env(), None, "unset config must disable throttling");

        let rt = tokio::runtime::Runtime::new().expect("runtime");
        let app = test_router(RateLimitState::from_env());
        rt.block_on(async {
            for _ in 0..(TEST_CONFIG.max_requests * 10) {
                let response = send(&app, "/test", 203, 0, 113, 1).await;
                assert_eq!(response.status(), StatusCode::OK);
            }
        });
    }

    /// Config gating: enabled by env with a default window, and malformed or
    /// zero values degrade to disabled rather than failing startup.
    #[test]
    fn config_from_env_gates_throttling() {
        let _lock = RATE_LIMIT_ENV_LOCK.lock().unwrap_or_else(PoisonError::into_inner);

        std::env::set_var(RATE_LIMIT_ENV, "100");
        std::env::remove_var(RATE_LIMIT_WINDOW_ENV);
        assert_eq!(
            RateLimitConfig::from_env(),
            Some(RateLimitConfig { max_requests: 100, window_secs: DEFAULT_WINDOW_SECS }),
            "unset window must fall back to the 60s default"
        );

        std::env::set_var(RATE_LIMIT_WINDOW_ENV, "30");
        assert_eq!(
            RateLimitConfig::from_env(),
            Some(RateLimitConfig { max_requests: 100, window_secs: 30 })
        );

        std::env::set_var(RATE_LIMIT_WINDOW_ENV, "0");
        assert_eq!(
            RateLimitConfig::from_env(),
            Some(RateLimitConfig { max_requests: 100, window_secs: DEFAULT_WINDOW_SECS }),
            "a zero window must fall back to the default"
        );

        std::env::set_var(RATE_LIMIT_ENV, "lots");
        assert_eq!(RateLimitConfig::from_env(), None, "malformed limit disables throttling");

        std::env::set_var(RATE_LIMIT_ENV, "0");
        assert_eq!(RateLimitConfig::from_env(), None, "an explicit 0 disables throttling");

        std::env::remove_var(RATE_LIMIT_ENV);
        std::env::remove_var(RATE_LIMIT_WINDOW_ENV);
    }

    /// Liveness probes are never throttled, even under a flood from the same
    /// client (mirrors the `/v1/health` bypass in `auth_layer`).
    #[test]
    fn health_endpoint_bypasses_limiter() {
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        let app = test_router(RateLimitState::enabled(RateLimitConfig {
            max_requests: 1,
            window_secs: 60,
        }));
        rt.block_on(async {
            assert_eq!(send(&app, "/test", 192, 0, 2, 3).await.status(), StatusCode::OK);
            assert_eq!(
                send(&app, "/test", 192, 0, 2, 3).await.status(),
                StatusCode::TOO_MANY_REQUESTS
            );
            for _ in 0..3 {
                assert_eq!(send(&app, HEALTH_PATH, 192, 0, 2, 3).await.status(), StatusCode::OK);
            }
        });
    }

    /// Fail-open: a poisoned limiter lock must not block traffic.
    #[test]
    fn poisoned_limiter_state_fails_open() {
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        let limiter =
            Arc::new(RateLimiter::new(RateLimitConfig { max_requests: 1, window_secs: 60 }));

        // Poison the counter lock by panicking while it is held, then recover
        // the test thread.
        let poison_target = limiter.clone();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = poison_target.buckets.lock().unwrap_or_else(PoisonError::into_inner);
            panic!("poison the rate limiter lock");
        }));
        assert!(result.is_err(), "the poisoning panic must have run");

        let state = RateLimitState { limiter: Some(limiter) };
        let app = test_router(state);
        rt.block_on(async {
            for _ in 0..3 {
                let response = send(&app, "/test", 192, 0, 2, 4).await;
                assert_eq!(
                    response.status(),
                    StatusCode::OK,
                    "limiter-state faults must fail open, not 429"
                );
            }
        });
    }

    /// Requests without connect info are not throttled (no per-client key).
    #[test]
    fn missing_connect_info_is_not_throttled() {
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        let app = test_router(RateLimitState::enabled(RateLimitConfig {
            max_requests: 1,
            window_secs: 60,
        }));
        rt.block_on(async {
            for _ in 0..3 {
                let req = Request::builder().uri("/test").body(Body::empty()).expect("body");
                let response = app.clone().oneshot(req).await.expect("request");
                assert_eq!(response.status(), StatusCode::OK);
            }
        });
    }

    /// `ceil_secs` rounds partial seconds up and never returns 0.
    #[test]
    fn retry_after_is_rounded_up_to_whole_seconds() {
        assert_eq!(ceil_secs(Duration::ZERO), 1);
        assert_eq!(ceil_secs(Duration::from_millis(1)), 1);
        assert_eq!(ceil_secs(Duration::from_millis(999)), 1);
        assert_eq!(ceil_secs(Duration::from_secs(2)), 2);
        assert_eq!(ceil_secs(Duration::from_millis(2_001)), 3);
    }
}
