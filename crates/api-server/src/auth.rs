//! API key authentication middleware for the API server.
//!
//! Provides a tower `Layer` that checks requests for a valid API key
//! (via `Authorization: Bearer <key>`). If `CONCERTO_API_KEY` is unset,
//! all requests are allowed (the caller is responsible for binding only
//! to localhost in that case — enforced at startup in `main.rs`).

use axum::{
    extract::Request,
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
};

/// Constant-time string comparison to prevent timing attacks.
///
/// Compares every byte pair regardless of early mismatch, so response
/// timing does not leak information about the correct key.
fn constant_time_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        // Length comparison itself leaks the length difference, but this is
        // acceptable: the attacker already knows the expected header format
        // ("Bearer <key>") and the key length is not secret.
        return false;
    }
    let a_bytes = a.as_bytes();
    let b_bytes = b.as_bytes();
    let mut diff: u8 = 0;
    for i in 0..a.len() {
        diff |= a_bytes[i] ^ b_bytes[i];
    }
    diff == 0
}

/// Axum middleware layer for API key authentication.
///
/// - Skips auth for `/v1/health`.
/// - If `CONCERTO_API_KEY` is set, requires `Authorization: Bearer <key>`.
/// - If `CONCERTO_API_KEY` is unset, permits all requests (startup gate
///   ensures this is safe by rejecting non-localhost binds without a key).
pub async fn auth_layer(req: Request, next: Next) -> Response {
    // Always allow health checks without authentication.
    if req.uri().path() == "/v1/health" {
        return next.run(req).await;
    }

    let api_key = std::env::var("CONCERTO_API_KEY").ok();

    if let Some(key) = api_key {
        if key.is_empty() {
            // Key env var exists but is empty — treat as unset.
            return next.run(req).await;
        }

        // Require a valid Bearer token.
        let auth_header = req.headers().get(axum::http::header::AUTHORIZATION);
        match auth_header {
            Some(value) => {
                let value_str = value.to_str().unwrap_or("");
                let expected = format!("Bearer {key}");
                if constant_time_eq(value_str, &expected) {
                    return next.run(req).await;
                }
                tracing::warn!("API auth failed: invalid Authorization header");
            }
            None => {
                tracing::warn!("API auth failed: missing Authorization header");
            }
        }

        (StatusCode::UNAUTHORIZED, "Invalid or missing API key").into_response()
    } else {
        // No API key configured — allow all requests.
        // The startup check in main.rs ensures we are bound to localhost
        // when there is no key, so this is safe.
        next.run(req).await
    }
}

/// Serialises access to the `CONCERTO_API_KEY` env var across EVERY test
/// module in this crate (auth tests here + router-based tests in
/// `routes.rs`). The env var is process-global, so two independent mutexes
/// would let parallel test threads race on it. Test-only.
#[cfg(test)]
pub(crate) static CONCERTO_API_KEY_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::Body,
        http::{self, Request, StatusCode},
        middleware::from_fn,
        routing::get,
        Router,
    };
    use std::sync::PoisonError;
    use tower::ServiceExt;

    /// Helper: build a minimal router with the auth middleware applied.
    fn auth_test_router() -> Router {
        async fn ok_handler() -> &'static str {
            "OK"
        }
        Router::new()
            .route("/test", get(ok_handler))
            .route("/v1/health", get(ok_handler))
            .layer(from_fn(auth_layer))
    }

    // ------------------------------------------------------------------
    // Existing tests for constant_time_eq
    // ------------------------------------------------------------------

    #[test]
    fn constant_time_eq_matches() {
        assert!(constant_time_eq("Bearer abc123", "Bearer abc123"));
    }

    #[test]
    fn constant_time_eq_differs() {
        assert!(!constant_time_eq("Bearer abc123", "Bearer abc124"));
    }

    #[test]
    fn constant_time_eq_different_length() {
        assert!(!constant_time_eq("Bearer abc", "Bearer abcd"));
    }

    #[test]
    fn constant_time_eq_empty() {
        assert!(constant_time_eq("", ""));
    }

    // ------------------------------------------------------------------
    // auth_layer middleware tests
    // ------------------------------------------------------------------

    /// When a valid `CONCERTO_API_KEY` is set and the request carries a
    /// matching `Authorization: Bearer <key>` header, the middleware
    /// passes the request through to the handler (200).
    ///
    /// Synchronous (block_on) to hold CONCERTO_API_KEY_LOCK across the whole test
    /// without holding a MutexGuard across an await point.
    #[test]
    fn auth_layer_with_valid_api_key() {
        let _lock = CONCERTO_API_KEY_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        std::env::set_var("CONCERTO_API_KEY", "test-key-123");
        let rt = tokio::runtime::Runtime::new().unwrap();
        let app = auth_test_router();
        let response = rt.block_on(async {
            app.oneshot(
                Request::builder()
                    .uri("/test")
                    .header(http::header::AUTHORIZATION, "Bearer test-key-123")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
        });
        std::env::remove_var("CONCERTO_API_KEY");
        assert_eq!(response.status(), StatusCode::OK);
    }

    /// When an invalid API key is provided, the middleware returns 401.
    #[test]
    fn auth_layer_with_invalid_api_key() {
        let _lock = CONCERTO_API_KEY_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        std::env::set_var("CONCERTO_API_KEY", "correct-key");
        let rt = tokio::runtime::Runtime::new().unwrap();
        let app = auth_test_router();
        let response = rt.block_on(async {
            app.oneshot(
                Request::builder()
                    .uri("/test")
                    .header(http::header::AUTHORIZATION, "Bearer wrong-key")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
        });
        std::env::remove_var("CONCERTO_API_KEY");
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    /// When no `Authorization` header is present, the middleware returns 401.
    #[test]
    fn auth_layer_with_missing_api_key() {
        let _lock = CONCERTO_API_KEY_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        std::env::set_var("CONCERTO_API_KEY", "secret");
        let rt = tokio::runtime::Runtime::new().unwrap();
        let app = auth_test_router();
        let response = rt.block_on(async {
            app.oneshot(Request::builder().uri("/test").body(Body::empty()).unwrap()).await.unwrap()
        });
        std::env::remove_var("CONCERTO_API_KEY");
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    /// When `CONCERTO_API_KEY` is set to an empty string, the middleware
    /// treats it as unset and allows all requests through.
    #[test]
    fn auth_layer_with_empty_api_key_env() {
        let _lock = CONCERTO_API_KEY_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        std::env::set_var("CONCERTO_API_KEY", "");
        let rt = tokio::runtime::Runtime::new().unwrap();
        let app = auth_test_router();
        let response = rt.block_on(async {
            app.oneshot(Request::builder().uri("/test").body(Body::empty()).unwrap()).await.unwrap()
        });
        std::env::remove_var("CONCERTO_API_KEY");
        assert_eq!(response.status(), StatusCode::OK);
    }

    /// The `/v1/health` endpoint is always bypassed, even when a valid
    /// API key is configured and no auth header is sent.
    #[test]
    fn auth_layer_bypass_for_health() {
        let _lock = CONCERTO_API_KEY_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        std::env::set_var("CONCERTO_API_KEY", "secret");
        let rt = tokio::runtime::Runtime::new().unwrap();
        let app = auth_test_router();
        let response = rt.block_on(async {
            app.oneshot(Request::builder().uri("/v1/health").body(Body::empty()).unwrap())
                .await
                .unwrap()
        });
        std::env::remove_var("CONCERTO_API_KEY");
        assert_eq!(response.status(), StatusCode::OK);
    }
}
