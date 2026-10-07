use super::*;

use crate::capability::CapabilityScope;
// --- check_url_allowed ---

#[test]
fn url_allowed_when_no_domain_restrictions() {
    let mut caps = GrantedCapabilities::new();
    caps.grant_session(CapabilityDiscriminant::NetworkOutbound, CapabilityScope::default());
    assert!(check_url_allowed(&caps, "p", "https://evil.com/malware").is_ok());
}

#[test]
fn url_denied_when_domain_not_in_allowlist() {
    let mut caps = GrantedCapabilities::new();
    let scope = CapabilityScope { domains: vec!["example.com".into()], ..Default::default() };
    caps.grant_session(CapabilityDiscriminant::NetworkOutbound, scope);
    let result = check_url_allowed(&caps, "p", "https://evil.com/malware");
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("evil.com"));
}

#[test]
fn url_allowed_when_domain_in_allowlist() {
    let mut caps = GrantedCapabilities::new();
    let scope = CapabilityScope { domains: vec!["example.com".into()], ..Default::default() };
    caps.grant_session(CapabilityDiscriminant::NetworkOutbound, scope);
    assert!(check_url_allowed(&caps, "p", "https://example.com/api").is_ok());
}

#[test]
fn url_subdomain_matches_parent_domain() {
    let mut caps = GrantedCapabilities::new();
    let scope = CapabilityScope { domains: vec!["example.com".into()], ..Default::default() };
    caps.grant_session(CapabilityDiscriminant::NetworkOutbound, scope);
    // api.example.com is a subdomain of example.com → allowed.
    assert!(check_url_allowed(&caps, "p", "https://api.example.com/v1").is_ok());
}

#[test]
fn url_not_granted_is_denied() {
    let caps = GrantedCapabilities::new();
    let result = check_url_allowed(&caps, "p", "https://example.com");
    assert!(result.is_err());
}

#[test]
fn url_rejects_invalid_url() {
    let mut caps = GrantedCapabilities::new();
    // Grant with a non-empty domain list so that URL parsing is exercised.
    let scope = CapabilityScope { domains: vec!["example.com".into()], ..Default::default() };
    caps.grant_session(CapabilityDiscriminant::NetworkOutbound, scope);
    let result = check_url_allowed(&caps, "p", "");
    assert!(result.is_err());
}

// --- check_shell_allowed ---

#[test]
fn shell_denied_when_not_granted() {
    let caps = GrantedCapabilities::new();
    let result = check_shell_allowed(&caps, "p", "echo hello");
    assert!(result.is_err());
}

#[test]
fn shell_allowed_when_no_allowlist() {
    let mut caps = GrantedCapabilities::new();
    caps.grant_session(CapabilityDiscriminant::ShellExecute, CapabilityScope::default());
    assert!(check_shell_allowed(&caps, "p", "echo hello").is_ok());
}

#[test]
fn shell_denied_when_not_in_allowlist() {
    let mut caps = GrantedCapabilities::new();
    let scope = CapabilityScope { allowlist: vec!["git *".into()], ..Default::default() };
    caps.grant_session(CapabilityDiscriminant::ShellExecute, scope);
    let result = check_shell_allowed(&caps, "p", "rm -rf /");
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("not in allowlist"));
}

#[test]
fn shell_allowed_when_exact_match_in_allowlist() {
    let mut caps = GrantedCapabilities::new();
    let scope = CapabilityScope { allowlist: vec!["echo hello".into()], ..Default::default() };
    caps.grant_session(CapabilityDiscriminant::ShellExecute, scope);
    assert!(check_shell_allowed(&caps, "p", "echo hello").is_ok());
}

#[test]
fn shell_prefix_match_rejected_exact_only() {
    let mut caps = GrantedCapabilities::new();
    let scope = CapabilityScope { allowlist: vec!["git status".into()], ..Default::default() };
    caps.grant_session(CapabilityDiscriminant::ShellExecute, scope);
    // Exact match — allowed.
    assert!(check_shell_allowed(&caps, "p", "git status").is_ok());
    // Prefix only — rejected (no more `*` suffix matching).
    let result = check_shell_allowed(&caps, "p", "git commit -m 'fix'");
    assert!(result.is_err());
}

#[test]
fn shell_injection_via_semicolon_rejected() {
    let mut caps = GrantedCapabilities::new();
    let scope = CapabilityScope { allowlist: vec!["git status".into()], ..Default::default() };
    caps.grant_session(CapabilityDiscriminant::ShellExecute, scope);
    // Shell injection attempt — semicolon chains a second command.
    let result = check_shell_allowed(&caps, "p", "git status; rm -rf /");
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("not in allowlist"));
}

#[test]
fn shell_injection_via_ampersand_rejected() {
    let mut caps = GrantedCapabilities::new();
    let scope = CapabilityScope { allowlist: vec!["ls".into()], ..Default::default() };
    caps.grant_session(CapabilityDiscriminant::ShellExecute, scope);
    // Double-amperstand chains commands.
    let result = check_shell_allowed(&caps, "p", "ls && cat /etc/passwd");
    assert!(result.is_err());
}

#[test]
fn shell_injection_via_pipe_rejected() {
    let mut caps = GrantedCapabilities::new();
    let scope = CapabilityScope { allowlist: vec!["echo hello".into()], ..Default::default() };
    caps.grant_session(CapabilityDiscriminant::ShellExecute, scope);
    // Pipe chains commands.
    let result = check_shell_allowed(&caps, "p", "echo hello | sh");
    assert!(result.is_err());
}

// --- check_path_allowed (glob enforcement) ---
//
// Note: these tests create real temp directories because
// `resolve_for_comparison` calls `canonicalize` and needs
// the path to exist on disk.

#[test]
fn path_allowed_when_no_glob_restrictions() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let file_path = root.join("src/main.rs");
    std::fs::create_dir_all(file_path.parent().unwrap()).unwrap();
    std::fs::write(&file_path, "").unwrap();

    let mut caps = GrantedCapabilities::new();
    caps.set_root(root);
    caps.grant_session(CapabilityDiscriminant::FilesystemRead, CapabilityScope::default());
    assert!(check_path_allowed(&caps, "p", file_path.to_str().unwrap(), false).is_ok());
}

#[test]
fn path_denied_when_glob_does_not_match() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let file_path = root.join("Makefile");
    std::fs::write(&file_path, "").unwrap();

    let mut caps = GrantedCapabilities::new();
    caps.set_root(root);
    let scope = CapabilityScope { globs: vec!["src/**/*.rs".into()], ..Default::default() };
    caps.grant_session(CapabilityDiscriminant::FilesystemRead, scope);
    let result = check_path_allowed(&caps, "p", file_path.to_str().unwrap(), false);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("does not match"));
}

#[test]
fn path_allowed_when_glob_matches() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let file_path = root.join("src/main.rs");
    std::fs::create_dir_all(file_path.parent().unwrap()).unwrap();
    std::fs::write(&file_path, "").unwrap();

    let mut caps = GrantedCapabilities::new();
    caps.set_root(root);
    let scope = CapabilityScope { globs: vec!["src/**/*.rs".into()], ..Default::default() };
    caps.grant_session(CapabilityDiscriminant::FilesystemRead, scope);
    assert!(check_path_allowed(&caps, "p", file_path.to_str().unwrap(), false).is_ok());
}

// --- check_path_allowed ---

/// `check_path_allowed` must reject relative paths regardless of grant state.
#[test]
fn test_check_path_allowed_rejects_relative_path() {
    let mut caps = GrantedCapabilities::new();
    caps.grant_session(CapabilityDiscriminant::FilesystemRead, CapabilityScope::default());

    let result = check_path_allowed(&caps, "p", "relative/path/file.rs", false);
    assert!(result.is_err());
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("only absolute paths"),
        "expected error about absolute paths, got: {err_msg}",
    );
}

// --- check_url_allowed ---

/// A URL that has no host component must be rejected (domain-restricted case).
#[test]
fn test_check_url_allowed_no_host() {
    let mut caps = GrantedCapabilities::new();
    caps.grant_session(
        CapabilityDiscriminant::NetworkOutbound,
        CapabilityScope { domains: vec!["example.com".into()], ..Default::default() },
    );

    // "https://" has no host — URL parsing succeeds but host is empty.
    // With domain restrictions, URL parsing is triggered so we get the error.
    let result = check_url_allowed(&caps, "p", "https://");
    assert!(result.is_err());
}

/// An empty URL must be rejected when domain restrictions are active.
#[test]
fn test_check_url_allowed_empty_with_domains() {
    let mut caps = GrantedCapabilities::new();
    caps.grant_session(
        CapabilityDiscriminant::NetworkOutbound,
        CapabilityScope { domains: vec!["example.com".into()], ..Default::default() },
    );

    // Empty string — becomes "https://" which has no host.
    let result = check_url_allowed(&caps, "p", "");
    assert!(result.is_err());
}

// ------------------------------------------------------------------
// Network egress allowlist (threat model §6 gap #7)
// ------------------------------------------------------------------

/// Grant `NetworkOutbound` with exactly `domains` as the egress allowlist.
fn granted_egress(domains: &[&str]) -> GrantedCapabilities {
    let mut caps = GrantedCapabilities::new();
    caps.grant_session(
        CapabilityDiscriminant::NetworkOutbound,
        CapabilityScope {
            domains: domains.iter().map(|d| (*d).to_string()).collect(),
            ..Default::default()
        },
    );
    caps
}

/// A configured allowlist admits a target that matches on scheme, host
/// and port simultaneously.
#[test]
fn egress_allowlist_permits_listed_scheme_host_port() {
    let caps = granted_egress(&["https://api.example.com:443"]);
    assert!(check_url_allowed(&caps, "p", "https://api.example.com:443/v1").is_ok());
}

/// A target matching no entry is refused, and the error names the rule
/// that refused it plus the offending host.
#[test]
fn egress_allowlist_denies_unlisted_host_naming_rule() {
    let caps = granted_egress(&["https://api.example.com:443"]);
    let err =
        check_url_allowed(&caps, "p", "https://evil.example.net/exfil").unwrap_err().to_string();
    assert!(err.contains(RULE_EGRESS_ALLOWLIST), "error must name the rule, got: {err}");
    assert!(err.contains("evil.example.net"), "error must name the target, got: {err}");
}

/// A scheme-qualified rule refuses the same host over another scheme.
#[test]
fn egress_allowlist_denies_scheme_mismatch() {
    let caps = granted_egress(&["https://example.com"]);
    let err = check_url_allowed(&caps, "p", "http://example.com/x").unwrap_err().to_string();
    assert!(err.contains(RULE_EGRESS_ALLOWLIST), "error must name the rule, got: {err}");
}

/// A port-qualified rule refuses the same host on another port (targets
/// without an explicit port are compared against their effective port).
#[test]
fn egress_allowlist_denies_port_mismatch() {
    let caps = granted_egress(&["example.com:8443"]);
    let err = check_url_allowed(&caps, "p", "https://example.com/").unwrap_err().to_string();
    assert!(err.contains(RULE_EGRESS_ALLOWLIST), "error must name the rule, got: {err}");
}

/// Unconfigured (`domains == []`) keeps today's fail-open behaviour: any
/// target passes, including ones a configured allowlist would refuse —
/// even when the target does not parse at all (parsing happens only
/// after the unconfigured fast path, exactly as before).
#[test]
fn egress_unconfigured_fails_open_for_any_target() {
    let caps = granted_egress(&[]);
    assert!(check_url_allowed(&caps, "p", "https://anything.example.org:8443/x").is_ok());
    assert!(check_url_allowed(&caps, "p", "not a url at all").is_ok());
}

/// A bare-host entry keeps its pre-gap-#7 meaning: any scheme, any port,
/// apex or subdomain.
#[test]
fn egress_bare_host_rule_matches_any_scheme_and_port() {
    let caps = granted_egress(&["example.com"]);
    assert!(check_url_allowed(&caps, "p", "http://example.com:9999/x").is_ok());
    assert!(check_url_allowed(&caps, "p", "https://api.example.com/").is_ok());
    assert!(check_url_allowed(&caps, "p", "https://evil.com/").is_err());
}

/// A leading `*.` is accepted and means apex-or-subdomain (it used to be
/// a dead entry that matched nothing).
#[test]
fn egress_star_prefix_matches_apex_and_subdomains() {
    let caps = granted_egress(&["*.example.com"]);
    assert!(check_url_allowed(&caps, "p", "https://api.example.com/").is_ok());
    assert!(check_url_allowed(&caps, "p", "https://example.com/").is_ok());
    assert!(check_url_allowed(&caps, "p", "https://notexample.com/").is_err());
}

/// A bare `*` is not a wildcard: it never matches, so a malformed rule
/// stays fail-closed instead of widening to "allow everything".
#[test]
fn egress_bare_star_entry_denies_everything() {
    let caps = granted_egress(&["*"]);
    let err = check_url_allowed(&caps, "p", "https://example.com/").unwrap_err().to_string();
    assert!(err.contains(RULE_EGRESS_ALLOWLIST), "error must name the rule, got: {err}");
}

/// Without the coarse capability the allowlist is never consulted; the
/// refusal names the capability rule instead.
#[test]
fn egress_ungranted_capability_names_network_capability_rule() {
    let caps = GrantedCapabilities::new();
    let err = check_url_allowed(&caps, "p", "https://example.com/").unwrap_err().to_string();
    assert!(err.contains(RULE_NETWORK_CAPABILITY), "error must name the rule, got: {err}");
}
