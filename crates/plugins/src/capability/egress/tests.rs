use super::*;
// --- extract_url_host ---

#[test]
fn extract_https_host() {
    assert_eq!(extract_url_host("https://example.com/path").unwrap(), "example.com");
}

#[test]
fn extract_host_with_port() {
    assert_eq!(extract_url_host("http://example.com:8080/path").unwrap(), "example.com");
}

#[test]
fn extract_bare_host() {
    assert_eq!(extract_url_host("example.com").unwrap(), "example.com");
}

#[test]
fn extract_bare_host_with_path() {
    assert_eq!(extract_url_host("example.com/api/v1").unwrap(), "example.com");
}

#[test]
fn extract_host_error_on_empty() {
    assert!(extract_url_host("").is_err());
}

#[test]
fn extract_host_with_userinfo_rejected() {
    // url::Url correctly parses userinfo — we should extract just the host.
    assert_eq!(extract_url_host("https://user:pass@example.com/path").unwrap(), "example.com");
}

#[test]
fn extract_host_ip_address() {
    assert_eq!(extract_url_host("http://127.0.0.1:3000/api").unwrap(), "127.0.0.1");
}

#[test]
fn extract_host_lowercase() {
    assert_eq!(extract_url_host("https://EXAMPLE.COM/path").unwrap(), "example.com");
}

/// Malformed entries parse to `None` (never a match); well-formed ones
/// in every supported shape parse.
#[test]
fn egress_rule_parse_rejects_malformed_entries() {
    for entry in ["", "   ", "example.com:", "example.com:notaport", "*", "*.", "://host"] {
        assert!(EgressRule::parse(entry).is_none(), "expected {entry:?} to be rejected");
    }
    for entry in [
        "example.com",
        "*.example.com",
        "https://example.com",
        "*://example.com",
        "example.com:8443",
        "https://example.com:443",
        "[::1]:8080",
        "::1",
    ] {
        assert!(EgressRule::parse(entry).is_some(), "expected {entry:?} to parse");
    }
}

/// Scheme/port constraints only apply when the rule names them.
#[test]
fn egress_rule_matching_dimensions() {
    let target = extract_egress_target("https://example.com:443/x").expect("valid url");
    let admits = |entry: &str| EgressRule::parse(entry).expect("valid rule").matches(&target);
    assert!(admits("example.com"), "host-only rule ignores scheme and port");
    assert!(admits("*.example.com"));
    assert!(admits("https://example.com"));
    assert!(admits("example.com:443"));
    assert!(admits("*://example.com:443"));
    assert!(!admits("http://example.com"), "scheme mismatch must refuse");
    assert!(!admits("example.com:8443"), "port mismatch must refuse");
    assert!(!admits("other.example.com"), "host mismatch must refuse");
}
