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
//!   provider-scoped `OPENCODE_LOCAL_API_KEY` and the server's own
//!   `OPENCODE_SERVER_PASSWORD` env vars as fallbacks.

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

/// The provider-scoped env var for the same password.
///
/// The shared [`ProviderConfig::effective_api_key`] path builds the name from
/// `provider.to_uppercase()`, so for `opencode-local` it looks for
/// `OPENCODE-LOCAL_API_KEY` — a hyphenated name a shell cannot export. This
/// underscored, exportable form is read explicitly instead.
pub(crate) const OPENCODE_LOCAL_API_KEY_ENV: &str = "OPENCODE_LOCAL_API_KEY";

/// Serializes every test that mutates the process-global `opencode-local`
/// password env vars, across the credential, factory, and discovery test
/// modules. A test that must observe them absent cannot race one that sets
/// them.
#[cfg(test)]
pub(crate) static OPENCODE_LOCAL_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Pick the password from an already-resolved explicit credential, falling
/// back to the provider env vars when it is empty.
///
/// The env order is most-specific first: [`OPENCODE_LOCAL_API_KEY_ENV`] then
/// the server's own [`OPENCODE_SERVER_PASSWORD_ENV`]. The latter is never
/// shadowed by the unexportable hyphenated name because that name is not read
/// at all.
pub(crate) fn opencode_local_password(explicit: &str) -> Option<SecretString> {
    if !explicit.trim().is_empty() {
        return Some(SecretString::from(explicit));
    }
    password_from_env()
}

/// The first non-empty provider env var, in priority order.
fn password_from_env() -> Option<SecretString> {
    for name in [OPENCODE_LOCAL_API_KEY_ENV, OPENCODE_SERVER_PASSWORD_ENV] {
        if let Ok(value) = std::env::var(name) {
            if !value.is_empty() {
                return Some(SecretString::from(value));
            }
        }
    }
    None
}

/// Resolve the password for a local `opencode serve` instance.
///
/// Resolution order:
///
/// 1. the config's `keyring_key` in the OS keychain (or, in test mode, the
///    derived `CONCERTO_<KEY>` / `OPENCODE_RS_<KEY>` env var);
/// 2. [`OPENCODE_LOCAL_API_KEY_ENV`] — the exportable provider-scoped var;
/// 3. [`OPENCODE_SERVER_PASSWORD_ENV`] — the variable the server itself reads,
///    and the practical way to configure this provider from a shell.
///
/// Step 1 deliberately uses [`ProviderConfig::api_key`] rather than
/// [`ProviderConfig::effective_api_key`]: the latter's derived name
/// (`OPENCODE-LOCAL_API_KEY`) cannot be exported, so relying on it would make
/// the env fallback unreachable. The server answers `401` without a valid
/// password, so a missing credential is reported as
/// [`ProviderError::CredentialMissing`] at build time rather than deferred to
/// the first request.
pub(crate) fn resolve_opencode_local_password(
    config: &ProviderConfig,
    creds: &CredentialStore,
) -> Result<SecretString, ProviderError> {
    if let Ok(secret) = config.api_key(creds) {
        if !secret.expose().is_empty() {
            return Ok(secret);
        }
    }
    if let Some(password) = password_from_env() {
        return Ok(password);
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
    use crate::testing::RestoreEnvVar;

    /// Both env var names are pinned: the server reads
    /// `OPENCODE_SERVER_PASSWORD`; the exportable provider-scoped form is the
    /// underscored `OPENCODE_LOCAL_API_KEY`.
    #[test]
    fn password_env_var_names_are_pinned() {
        assert_eq!(OPENCODE_SERVER_PASSWORD_ENV, "OPENCODE_SERVER_PASSWORD");
        assert_eq!(OPENCODE_LOCAL_API_KEY_ENV, "OPENCODE_LOCAL_API_KEY");
    }

    /// An explicit credential always wins over either env var.
    #[test]
    fn explicit_password_wins_over_env() {
        let _lock = OPENCODE_LOCAL_ENV_LOCK.lock().expect("env lock");
        let _server = RestoreEnvVar::set(OPENCODE_SERVER_PASSWORD_ENV, "from-server");
        let _local = RestoreEnvVar::set(OPENCODE_LOCAL_API_KEY_ENV, "from-local");
        let selected = opencode_local_password("explicit").expect("explicit wins");
        assert_eq!(selected.expose(), "explicit");
    }

    /// With no explicit credential, the exportable provider-scoped var is used.
    #[test]
    fn local_api_key_env_is_used_when_explicit_is_empty() {
        let _lock = OPENCODE_LOCAL_ENV_LOCK.lock().expect("env lock");
        let _server = RestoreEnvVar::without(OPENCODE_SERVER_PASSWORD_ENV);
        let _local = RestoreEnvVar::set(OPENCODE_LOCAL_API_KEY_ENV, "from-local");
        assert_eq!(opencode_local_password("").expect("env fallback").expose(), "from-local");
    }

    /// The server's own variable is reachable when the provider-scoped var is
    /// absent — it is not shadowed by the unexportable hyphenated name.
    #[test]
    fn server_password_env_is_reachable_when_local_api_key_is_absent() {
        let _lock = OPENCODE_LOCAL_ENV_LOCK.lock().expect("env lock");
        let _local = RestoreEnvVar::without(OPENCODE_LOCAL_API_KEY_ENV);
        let _server = RestoreEnvVar::set(OPENCODE_SERVER_PASSWORD_ENV, "from-server");
        assert_eq!(opencode_local_password("").expect("env fallback").expose(), "from-server");
    }

    /// No explicit credential and neither env var set: no password.
    #[test]
    fn no_password_anywhere_is_none() {
        let _lock = OPENCODE_LOCAL_ENV_LOCK.lock().expect("env lock");
        let _server = RestoreEnvVar::without(OPENCODE_SERVER_PASSWORD_ENV);
        let _local = RestoreEnvVar::without(OPENCODE_LOCAL_API_KEY_ENV);
        assert!(opencode_local_password("").is_none());
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
