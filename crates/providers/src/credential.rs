//! Wire-credential resolution for OpenCode's unauthenticated ("free tier")
//! provider behaviour.
//!
//! This is the ONE place that decides what credential an OpenCode request
//! carries. Both the OpenAI-compatible connector ([`crate::openai`]) and the
//! OpenCode connector ([`crate::opencode`], for the Responses and
//! Anthropic-Messages legs) route their auth header through
//! [`resolve_wire_credential`], so the three states below can never drift
//! apart.
//!
//! # Why `"public"` is the credential
//!
//! When no key is configured, the upstream OpenCode client installs the
//! **literal string** `"public"` as the SDK `apiKey`:
//!
//! - V1 `packages/opencode/src/provider/provider.ts`:
//!   `options: ok ? {} : { apiKey: "public" }`
//! - V2 `packages/core/src/plugin/provider/opencode.ts`:
//!   `if (!hasKey) provider.request.body.apiKey = "public"`
//!
//! Because the Zen provider uses `@ai-sdk/openai-compatible`, that value is
//! emitted verbatim on the wire as `Authorization: Bearer public`. It is
//! **not** an empty string and **not** an omitted header.
//!
//! The server maps it straight back to "no key". Quoted from
//! `packages/console/app/src/routes/zen/util/handler.ts:104`:
//!
//! ```text
//! const zenApiKey = rawZenApiKey === "public" ? undefined : rawZenApiKey
//! ```
//!
//! `undefined` selects the **anonymous** path — `modelInfo.allowAnonymous`
//! chooses `createIpRateLimiter` instead of `createKeyRateLimiter`. The value
//! is therefore a sentinel, not a secret: it grants nothing and authenticates
//! nobody. Sending it on a request that *has* a real key would be wrong, and
//! sending it when free-tier mode is off would change shipped behaviour, so
//! the decision is explicit below.

use std::time::Duration;

use concerto_core::error::ProviderError;
use reqwest::StatusCode;

/// The literal credential OpenCode's relay interprets as "no API key".
///
/// See the module docs for the `handler.ts:104` mapping that turns it back
/// into the anonymous path.
pub(crate) const OPENCODE_ANONYMOUS_CREDENTIAL: &str = "public";

/// The three possible wire credentials for an OpenCode request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WireCredential<'a> {
    /// A real API key was resolved; send it verbatim.
    Key(&'a str),
    /// Free-tier mode, no key: the literal [`OPENCODE_ANONYMOUS_CREDENTIAL`].
    Anonymous,
    /// No key and not free-tier: preserve the historical empty credential
    /// (today's shipped behaviour) — never substitute `public`.
    Empty,
}

impl WireCredential<'_> {
    /// The value to place after `Bearer ` or in `x-api-key`.
    pub(crate) fn expose(&self) -> &str {
        match self {
            WireCredential::Key(key) => key,
            WireCredential::Anonymous => OPENCODE_ANONYMOUS_CREDENTIAL,
            WireCredential::Empty => "",
        }
    }

    /// Whether this is the anonymous free-tier sentinel. Used to select the
    /// free-tier error mapping (a keyed 429 is an ordinary rate limit).
    pub(crate) fn is_anonymous(&self) -> bool {
        matches!(self, WireCredential::Anonymous)
    }
}

/// Resolve the wire credential from the configured key and free-tier mode.
///
/// The three states are deliberately explicit:
///
/// 1. a real (non-empty) key → [`WireCredential::Key`] (`Bearer <key>`);
/// 2. no key + free-tier mode → [`WireCredential::Anonymous`]
///    (`Bearer public`, the server's anonymous path);
/// 3. no key + not free-tier → [`WireCredential::Empty`] (the historical
///    empty credential; the header shape is unchanged).
pub(crate) fn resolve_wire_credential(api_key: &str, free_tier: bool) -> WireCredential<'_> {
    if !api_key.is_empty() {
        WireCredential::Key(api_key)
    } else if free_tier {
        WireCredential::Anonymous
    } else {
        WireCredential::Empty
    }
}

/// Map a non-success OpenCode response to a [`ProviderError`], giving the
/// free tier its own honest states.
///
/// Only active in free-tier mode (`free_tier == true`); every other request
/// falls through to the shared [`crate::retry::map_http_error`] so shipped
/// behaviour is unchanged.
///
/// - **429 on an anonymous request** → [`ProviderError::FreeTierRefused`] with
///   the server's `retry-after`. This is the IP-keyed anonymous daily cap
///   (`FreeUsageLimitError`); it cannot succeed by retrying, so it must never
///   be classified as a retryable [`ProviderError::RateLimit`]. Any 429 while
///   anonymous is treated as the cap, so a body-less response cannot fall back
///   to the hammering path.
/// - **403 with a `FreeTierError` body** → [`ProviderError::FreeTierRefused`]
///   without a wait hint: the model is only served to an OpenCode-signed
///   session, so a real key is required.
/// - everything else → the shared mapping, byte-identical to before.
pub(crate) fn map_opencode_http_error(
    status: StatusCode,
    body: &str,
    retry_after: Option<Duration>,
    free_tier: bool,
    anonymous: bool,
) -> ProviderError {
    if free_tier {
        if status.as_u16() == 429 && anonymous {
            return ProviderError::FreeTierRefused { retry_after, message: body.to_string() };
        }
        if status.as_u16() == 403 && body.contains("FreeTierError") {
            return ProviderError::FreeTierRefused { retry_after: None, message: body.to_string() };
        }
    }
    crate::retry::map_http_error(status, body, retry_after)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn real_key_wins_over_free_tier() {
        let credential = resolve_wire_credential("sk-live", true);
        assert_eq!(credential, WireCredential::Key("sk-live"));
        assert_eq!(credential.expose(), "sk-live");
        assert!(!credential.is_anonymous());
    }

    #[test]
    fn keyless_free_tier_is_the_public_sentinel() {
        let credential = resolve_wire_credential("", true);
        assert_eq!(credential, WireCredential::Anonymous);
        assert_eq!(credential.expose(), "public");
        assert!(credential.is_anonymous());
    }

    #[test]
    fn keyless_non_free_tier_stays_empty() {
        let credential = resolve_wire_credential("", false);
        assert_eq!(credential, WireCredential::Empty);
        assert_eq!(credential.expose(), "");
        assert!(!credential.is_anonymous());
    }

    #[test]
    fn anonymous_429_is_a_non_retryable_free_tier_refusal() {
        let error = map_opencode_http_error(
            StatusCode::TOO_MANY_REQUESTS,
            r#"{"error":{"type":"FreeUsageLimitError"}}"#,
            Some(Duration::from_secs(48_700)),
            true,
            true,
        );
        match error {
            ProviderError::FreeTierRefused { retry_after, .. } => {
                assert_eq!(retry_after, Some(Duration::from_secs(48_700)));
            }
            other => panic!("expected FreeTierRefused, got {other:?}"),
        }
    }

    #[test]
    fn keyed_429_stays_a_retryable_rate_limit() {
        let error = map_opencode_http_error(
            StatusCode::TOO_MANY_REQUESTS,
            "slow down",
            Some(Duration::from_secs(5)),
            true,
            false,
        );
        assert!(matches!(error, ProviderError::RateLimit { .. }), "got {error:?}");
    }

    #[test]
    fn free_tier_403_surfaces_the_eligibility_refusal() {
        let error = map_opencode_http_error(
            StatusCode::FORBIDDEN,
            r#"{"error":{"type":"FreeTierError","message":"OpenCode's free tier can only be used from within OpenCode"}}"#,
            None,
            true,
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

    #[test]
    fn flag_off_mapping_is_unchanged() {
        // A non-free-tier 429 must still be the ordinary retryable rate limit.
        let error =
            map_opencode_http_error(StatusCode::TOO_MANY_REQUESTS, "body", None, false, false);
        assert!(matches!(error, ProviderError::RateLimit { .. }), "got {error:?}");
        // A non-free-tier 403 must still be the ordinary auth failure.
        let error = map_opencode_http_error(StatusCode::FORBIDDEN, "body", None, false, false);
        assert!(matches!(error, ProviderError::AuthFailure), "got {error:?}");
    }
}
