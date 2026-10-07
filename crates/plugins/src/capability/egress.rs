//! Network egress allowlist parsing and matching.

use crate::error::PluginError;

use super::policy::RULE_EGRESS_ALLOWLIST;

/// Scheme / host / port triple parsed out of a target URL so an egress rule
/// can be matched against every dimension it constrains.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct EgressTarget {
    /// Lowercased URL scheme (no trailing `:`).
    scheme: String,
    /// Lowercased host, IPv6 brackets stripped (`::1`, not `[::1]`).
    host: String,
    /// Effective port — `port_or_known_default()`, so `https://x` yields `443`.
    /// `None` only for a non-special scheme with no explicit port.
    port: Option<u16>,
}

/// One parsed egress allowlist entry: `[scheme://]host[:port]`.
///
/// Parsing is deliberately strict and *fail-closed*: an entry that cannot be
/// interpreted (empty host, non-numeric port, bare `*`) yields `None` and is
/// never treated as a match. A malformed rule can therefore only narrow what
/// is reachable, never widen it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct EgressRule {
    /// `None` = any scheme (omitted or `*`).
    scheme: Option<String>,
    /// Exact or parent-domain pattern, lowercased.
    host: String,
    /// `None` = any port.
    port: Option<u16>,
}

impl EgressRule {
    /// Parse a single allowlist entry. Returns `None` when the entry can
    /// never match a well-formed target (see the type-level docs).
    fn parse(entry: &str) -> Option<Self> {
        let entry = entry.trim();
        if entry.is_empty() {
            return None;
        }
        let (scheme, rest) = match entry.split_once("://") {
            Some((scheme, rest)) => {
                let scheme = scheme.trim().to_ascii_lowercase();
                if scheme.is_empty() {
                    return None;
                }
                ((scheme != "*").then_some(scheme), rest.trim())
            }
            None => (None, entry),
        };
        let (host, port) = split_rule_host_port(rest)?;
        let host = host.to_ascii_lowercase();
        // `*` and `*.` are not host patterns — reject them outright so a
        // wildcard-escape can never read as "allow every host".
        let bare = host.strip_prefix("*.").unwrap_or(&host);
        if bare.is_empty() || bare == "*" {
            return None;
        }
        Some(Self { scheme, host, port })
    }

    /// Whether this rule admits `target`. Every dimension the rule names
    /// must match; an unnamed dimension is unconstrained.
    fn matches(&self, target: &EgressTarget) -> bool {
        if let Some(scheme) = &self.scheme {
            if *scheme != target.scheme {
                return false;
            }
        }
        if let Some(port) = self.port {
            if target.port != Some(port) {
                return false;
            }
        }
        host_pattern_matches(&self.host, &target.host)
    }
}

/// Split `[scheme://]host[:port]`'s host/port part.
///
/// Handles three shapes: bracketed IPv6 (`[::1]:8443`), a single `:` that
/// must be a numeric port, and everything else treated as a bare host (which
/// also covers an unbracketed IPv6 literal such as `::1`).
fn split_rule_host_port(rest: &str) -> Option<(&str, Option<u16>)> {
    if rest.is_empty() {
        return None;
    }
    if let Some(inner) = rest.strip_prefix('[') {
        let close = inner.find(']')?;
        let host = &inner[..close];
        let tail = &inner[close + 1..];
        let port = match tail {
            "" => None,
            _ => Some(tail.strip_prefix(':')?.parse::<u16>().ok()?),
        };
        return (!host.is_empty()).then_some((host, port));
    }
    match rest.matches(':').count() {
        0 => Some((rest, None)),
        1 => {
            let (host, port) = rest.split_once(':')?;
            // A non-numeric or out-of-range port makes the whole entry
            // unusable rather than silently dropping the port constraint.
            let port = port.parse::<u16>().ok()?;
            (!host.is_empty()).then_some((host, Some(port)))
        }
        // Two or more colons and no brackets → the literal is an IPv6
        // address with no port component.
        _ => Some((rest, None)),
    }
}

/// Exact or parent-domain match: `example.com` covers `example.com` and any
/// `sub.example.com`. A leading `*.` is normalized away (it used to be a
/// dead entry that matched nothing).
fn host_pattern_matches(pattern: &str, host: &str) -> bool {
    let base = pattern.strip_prefix("*.").unwrap_or(pattern);
    if base.is_empty() {
        return false;
    }
    host == base || host.ends_with(&format!(".{base}"))
}

/// Split `url` into its scheme/host/port triple for egress matching.
///
/// Keeps the normalization this check has always applied: trim, treat a
/// scheme-less `host/path` form as `https`, and lowercase the host.
pub(super) fn extract_egress_target(url: &str) -> Result<EgressTarget, PluginError> {
    let url = url.trim();
    if url.is_empty() {
        return Err(PluginError::CapabilityDenied("URL has no host: (empty)".into()));
    }

    // If no scheme is present, url::Url::parse requires one — prepend
    // a dummy scheme so bare `host/path` forms parse correctly.
    let url_to_parse = if url.contains("://") { url.to_string() } else { format!("https://{url}") };

    let parsed = url::Url::parse(&url_to_parse)
        .map_err(|e| PluginError::CapabilityDenied(format!("invalid URL: {e}")))?;

    let host = parsed
        .host_str()
        .ok_or_else(|| PluginError::CapabilityDenied(format!("URL has no host: {url}")))?
        .to_lowercase();
    // `host_str()` serializes IPv6 with brackets; strip them so targets and
    // rule entries share one canonical host form.
    let host =
        host.strip_prefix('[').and_then(|h| h.strip_suffix(']')).unwrap_or(&host).to_string();

    Ok(EgressTarget {
        scheme: parsed.scheme().to_ascii_lowercase(),
        host,
        port: parsed.port_or_known_default(),
    })
}

/// Extract the hostname part from a URL string.
///
/// Returns the lowercased host portion, or an error if the URL cannot
/// be parsed or has no host. Test-only convenience over
/// [`extract_egress_target`] (the production path needs scheme/port too).
#[cfg(test)]
fn extract_url_host(url: &str) -> Result<String, PluginError> {
    extract_egress_target(url).map(|target| target.host)
}

/// Evaluate the configured egress allowlist against `target`.
///
/// Default-deny: at least one entry must admit the target. Entries that fail
/// to parse are warned about once per check and never count as a match.
pub(super) fn match_egress_allowlist(
    plugin_id: &str,
    allowlist: &[String],
    target: &EgressTarget,
    url: &str,
) -> Result<(), PluginError> {
    let mut invalid: Vec<&str> = Vec::new();
    for entry in allowlist {
        match EgressRule::parse(entry) {
            Some(rule) if rule.matches(target) => return Ok(()),
            Some(_) => {}
            None => invalid.push(entry.as_str()),
        }
    }
    if !invalid.is_empty() {
        tracing::warn!(
            plugin_id,
            ?invalid,
            "invalid network egress allowlist entries never match; they grant nothing"
        );
    }
    Err(PluginError::CapabilityDenied(format!(
        "{RULE_EGRESS_ALLOWLIST}: network egress to '{url}' denied \
         (scheme={} host={} port={:?}); allowlist: {allowlist:?}",
        target.scheme, target.host, target.port,
    )))
}

#[cfg(test)]
mod tests;
