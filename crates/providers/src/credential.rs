//! Wire-credential resolution and HTTP error mapping for the OpenCode
//! provider family.
//!
//! # Status of the anonymous ("free tier") HTTP path
//!
//! A previous change (`2f63274`, feature `opencode-free-tier`) ported
//! OpenCode's own keyless behaviour by sending the literal credential
//! `Authorization: Bearer public` to the Zen relay. **That path does not
//! work.** Live testing (2026-09-30) shows the relay answers `403
//! FreeTierError` for 7 of the 8 zero-cost models and `429` for the rest;
//! anonymous HTTP access to the Zen relay is refused server-side. The
//! supported route to OpenCode's free models is a local `opencode serve`
//! instance, reached through the `opencode-local` provider type
//! ([`crate::opencode_local`]).
//!
//! The credential substitution to `"public"` has therefore been removed. What
//! remains here:
//!
//! - [`map_opencode_http_error`] — the `FreeTierRefused` surface, which
//!   reports the relay's `403 FreeTierError` refusal honestly (a model that
//!   requires an OpenCode-signed session) instead of collapsing it to a
//!   generic auth failure.
//! - [`resolve_opencode_local_password`] — the HTTP Basic password a local
//!   `opencode serve` instance expects, resolved keyring-first with the
//!   server's own `OPENCODE_SERVER_PASSWORD` env var as the fallback.

use std::time::Duration;

use concerto_config::{CredentialStore, ProviderConfig};
use concerto_core::error::ProviderError;
use concerto_core::SecretString;
use reqwest::StatusCode;

/// The environment variable both `opencode serve` and this client read for the
/// server's HTTP Basic password.
///
/// Mirrors upstream `packages/opencode/src/server/auth.ts`, which reads
/// `OPENCODE_SERVER_PASSWORD` and defaults the username to `opencode`.
pub(crate) const OPENCODE_SERVER_PASSWORD_ENV: &str = "OPENCODE_SERVER_PASSWORD";

/// Resolve the password for a local `opencode serve` instance.
///
/// Resolution order mirrors [`ProviderConfig::effective_api_key`]:
///
/// 1. the config's `keyring_key` in the OS keychain (or, in test mode, the
///    derived `CONCERTO_<KEY>` / `OPENCODE_RS_<KEY>` env var);
/// 2. the `<PROVIDER>_API_KEY` env var (built from `provider.to_uppercase()`,
///    so for `opencode-local` it is the unexportable `OPENCODE-LOCAL_API_KEY`);
/// 3. [`OPENCODE_SERVER_PASSWORD_ENV`] — the variable the server itself reads,
///    and the practical way to configure this provider from a shell.
///
/// The server answers `401` without a valid password, so a missing credential
/// is reported as [`ProviderError::CredentialMissing`] at build time rather
/// than deferred to the first request.
pub(crate) fn resolve_opencode_local_password(
    config: &ProviderConfig,
    creds: &CredentialStore,
) -> Result<SecretString, ProviderError> {
    if let Ok(secret) = config.effective_api_key(creds) {
        if !secret.expose().is_empty() {
            return Ok(secret);
        }
    }
    if let Ok(password) = std::env::var(OPENCODE_SERVER_PASSWORD_ENV) {
        if !password.is_empty() {
            return Ok(SecretString::from(password));
        }
    }
    Err(ProviderError::CredentialMissing {
        provider: if config.name.trim().is_empty() {
            config.provider.clone()
        } else {
            config.name.clone()
        },
    })
}

/// Map a non-success OpenCode response to a [`ProviderError`], giving the
/// free tier its own honest state.
///
/// Only active when `free_tier` is set (the `opencode-free-tier` feature's
/// `opencode-free` type); every other request falls through to the shared
/// [`crate::retry::map_http_error`] so shipped behaviour is unchanged.
///
/// - **403 with a `FreeTierError` body** → [`ProviderError::FreeTierRefused`]
///   without a wait hint: the model is only served to an OpenCode-signed
///   session, so a real key is required.
/// - everything else → the shared mapping, byte-identical to before.
///
/// The former anonymous-`429` daily-cap branch was removed together with the
/// `Bearer public` credential: with no anonymous request there is no
/// IP-keyed daily cap to distinguish, and a keyed `429` is an ordinary
/// retryable rate limit.
pub(crate) fn map_opencode_http_error(
    status: StatusCode,
    body: &str,
    retry_after: Option<Duration>,
    free_tier: bool,
) -> ProviderError {
    if free_tier && status.as_u16() == 403 && body.contains("FreeTierError") {
        return ProviderError::FreeTierRefused { retry_after: None, message: body.to_string() };
    }
    crate::retry::map_http_error(status, body, retry_after)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `OPENCODE_SERVER_PASSWORD` is the server's own variable name; the client
    /// must read exactly it.
    #[test]
    fn server_password_env_var_name_is_pinned() {
        assert_eq!(OPENCODE_SERVER_PASSWORD_ENV, "OPENCODE_SERVER_PASSWORD");
    }

    /// A free-tier `403 FreeTierError` is surfaced as an eligibility refusal
    /// with the server's explanation, not a generic auth failure.
    #[test]
    fn free_tier_403_surfaces_the_eligibility_refusal() {
        let error = map_opencode_http_error(
            StatusCode::FORBIDDEN,
            r#"{"error":{"type":"FreeTierError","message":"OpenCode's free tier can only be used from within OpenCode"}}"#,
            None,
            true,
        );
        match error {
            ProviderError::FreeTierRefused { retry_after, message } => {
                assert_eq!(retry_after, None);
                assert!(message.contains("FreeTierError"));
            }
            other => panic!("expected FreeTierRefused, got {other:?}"),
        }
    }

    /// With the feature off — and for any non-free-tier request — the mapping
    /// is byte-identical to the shared one: a 403 is an ordinary auth failure
    /// and a 429 an ordinary retryable rate limit.
    #[test]
    fn non_free_tier_mapping_is_unchanged() {
        let error = map_opencode_http_error(StatusCode::TOO_MANY_REQUESTS, "body", None, false);
        assert!(matches!(error, ProviderError::RateLimit { .. }), "got {error:?}");
        let error = map_opencode_http_error(StatusCode::FORBIDDEN, "body", None, false);
        assert!(matches!(error, ProviderError::AuthFailure), "got {error:?}");
    }

    /// Free-tier mode without the `FreeTierError` marker keeps the shared
    /// mapping (only the eligibility refusal gets the dedicated variant).
    #[test]
    fn free_tier_mode_without_marker_keeps_shared_mapping() {
        let error = map_opencode_http_error(StatusCode::FORBIDDEN, "plain", None, true);
        assert!(matches!(error, ProviderError::AuthFailure), "got {error:?}");
    }
}
