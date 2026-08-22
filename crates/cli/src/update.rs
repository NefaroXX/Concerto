//! Non-blocking startup update check against crates.io.
//!
//! Fires an async, timeout-bounded GET to the crates.io API, compares
//! semver, and logs at `info!` level if a newer version is available.
//! Never blocks startup, never auto-downloads.

use std::time::Duration;
use tracing::{info, warn};

/// Current version of this crate, compiled at build time.
const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// crates.io API endpoint for checking the latest version.
const CRATES_IO_API: &str = "https://crates.io/api/v1/crates/concerto";

/// Timeout for the entire update check, including DNS and response parsing.
const UPDATE_CHECK_TIMEOUT: Duration = Duration::from_secs(2);

/// Check for a newer version on crates.io.
///
/// This function spawns a background task and returns immediately.
/// It logs the result at `info!` level (or `warn!` on failure).
pub fn check_for_updates() {
    let current = CURRENT_VERSION.to_string();
    tokio::spawn(async move {
        match do_check(&current).await {
            Ok(Some(update_info)) => {
                info!(
                    current = %update_info.current,
                    latest = %update_info.latest,
                    "A newer version of concerto is available: {}",
                    update_info.latest
                );
            }
            Ok(None) => {
                info!(version = %current, "concerto is up to date");
            }
            Err(e) => {
                warn!(error = %e, "Failed to check for updates");
            }
        }
    });
}

struct UpdateInfo {
    current: String,
    latest: String,
}

async fn do_check(current_version: &str) -> Result<Option<UpdateInfo>, String> {
    do_check_with(current_version, CRATES_IO_API, UPDATE_CHECK_TIMEOUT).await
}

async fn do_check_with(
    current_version: &str,
    endpoint: &str,
    timeout: Duration,
) -> Result<Option<UpdateInfo>, String> {
    tokio::time::timeout(timeout, fetch_update(current_version, endpoint, timeout))
        .await
        .map_err(|_| format!("update check timed out after {timeout:?}"))?
}

async fn fetch_update(
    current_version: &str,
    endpoint: &str,
    timeout: Duration,
) -> Result<Option<UpdateInfo>, String> {
    let client = reqwest::Client::builder()
        .timeout(timeout)
        .user_agent("concerto/update-check")
        .build()
        .map_err(|e| format!("failed to create HTTP client: {e}"))?;

    let resp = client.get(endpoint).send().await.map_err(|e| {
        if e.is_timeout() {
            format!("update check timed out after {timeout:?}")
        } else {
            format!("failed to fetch crates.io: {e}")
        }
    })?;

    let body: serde_json::Value =
        resp.json().await.map_err(|e| format!("failed to parse crates.io response: {e}"))?;

    let latest_version = body
        .get("crate")
        .and_then(|c| c.get("newest_version"))
        .and_then(|v| v.as_str())
        .ok_or("missing newest_version in response")?;

    let current = semver::Version::parse(current_version)
        .map_err(|e| format!("failed to parse current version: {e}"))?;
    let latest = semver::Version::parse(latest_version)
        .map_err(|e| format!("failed to parse latest version: {e}"))?;

    if latest > current {
        Ok(Some(UpdateInfo {
            current: current_version.to_string(),
            latest: latest_version.to_string(),
        }))
    } else {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_version_is_valid_semver() {
        assert!(semver::Version::parse(CURRENT_VERSION).is_ok());
    }

    #[tokio::test]
    async fn stalled_response_returns_timeout_error() {
        let listener =
            tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind local stalled server");
        let endpoint = format!(
            "http://{}/api/v1/crates/concerto",
            listener.local_addr().expect("read local server address")
        );
        let server = tokio::spawn(async move {
            let (_socket, _) = listener.accept().await.expect("accept update-check request");
            tokio::time::sleep(Duration::from_secs(5)).await;
        });

        let timeout = Duration::from_millis(200);
        let start = std::time::Instant::now();
        let result = do_check_with("0.1.0", &endpoint, timeout).await;
        server.abort();

        let elapsed = start.elapsed();
        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("stalled response should time out"),
        };
        assert!(error.contains("update check timed out"), "unexpected timeout error: {error}");
        assert!(elapsed < Duration::from_secs(10), "update check took too long: {elapsed:?}");
    }
}
