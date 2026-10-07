use super::*;
#[test]
fn granted_cap_check_returns_false_by_default() {
    let caps = GrantedCapabilities::new();
    let req = CapabilityRequest::FilesystemRead { globs: vec![] };
    assert!(!caps.check("test-plugin", &req));
}

#[test]
fn session_grant_is_checked() {
    let mut caps = GrantedCapabilities::new();
    caps.grant_session(CapabilityDiscriminant::FilesystemRead, CapabilityScope::default());
    let req = CapabilityRequest::FilesystemRead { globs: vec![] };
    assert!(caps.check("test-plugin", &req));
}

#[test]
fn persistent_grant_survives_recreation() {
    let mut caps = GrantedCapabilities::new();
    caps.persist(
        "test-plugin",
        CapabilityDiscriminant::NetworkOutbound,
        CapabilityScope::default(),
    );
    let req = CapabilityRequest::NetworkOutbound { domains: vec![] };
    assert!(caps.check("test-plugin", &req));
}

#[test]
fn ungranted_cap_is_denied() {
    let caps = GrantedCapabilities::new();
    let req = CapabilityRequest::ShellExecute { allowlist: vec![] };
    assert!(!caps.check("test-plugin", &req));
}

#[test]
fn loaded_grants_are_checked() {
    let caps = GrantedCapabilities::with_persistent(
        "test-plugin",
        vec![(
            CapabilityDiscriminant::FilesystemRead,
            CapabilityScope::default(),
            now_unix() + 3600,
        )],
    );
    let req = CapabilityRequest::FilesystemRead { globs: vec![] };
    assert!(caps.check("test-plugin", &req));
    let req2 = CapabilityRequest::NetworkOutbound { domains: vec![] };
    assert!(!caps.check("test-plugin", &req2));
}

// --- get_scope ---

#[test]
fn get_scope_returns_none_for_ungranted() {
    let caps = GrantedCapabilities::new();
    assert!(caps.get_scope("p", &CapabilityDiscriminant::FilesystemRead).is_none());
}

#[test]
fn get_scope_returns_session_scope() {
    let mut caps = GrantedCapabilities::new();
    let scope = CapabilityScope { globs: vec!["src/**/*.rs".into()], ..Default::default() };
    caps.grant_session(CapabilityDiscriminant::FilesystemRead, scope.clone());
    let got = caps.get_scope("p", &CapabilityDiscriminant::FilesystemRead);
    assert_eq!(got, Some(&scope));
}

#[test]
fn get_scope_returns_persistent_scope() {
    let mut caps = GrantedCapabilities::new();
    let scope = CapabilityScope { domains: vec!["api.example.com".into()], ..Default::default() };
    caps.persist("p", CapabilityDiscriminant::NetworkOutbound, scope.clone());
    let got = caps.get_scope("p", &CapabilityDiscriminant::NetworkOutbound);
    assert_eq!(got, Some(&scope));
}

// --- per-call TTL enforcement (audit finding H3) ---

#[test]
fn expired_persistent_grant_denied_at_call_time() {
    let caps = GrantedCapabilities::with_persistent(
        "p",
        vec![(CapabilityDiscriminant::FilesystemRead, CapabilityScope::default(), now_unix() - 10)],
    );
    let req = CapabilityRequest::FilesystemRead { globs: vec![] };
    assert!(!caps.check("p", &req), "persistent grant past its TTL must be denied at call time");
    assert!(
        caps.get_scope("p", &CapabilityDiscriminant::FilesystemRead).is_none(),
        "expired persistent grant must not yield a scope"
    );
}

#[test]
fn unexpired_persistent_grant_allowed_at_call_time() {
    let caps = GrantedCapabilities::with_persistent(
        "p",
        vec![(
            CapabilityDiscriminant::FilesystemRead,
            CapabilityScope::default(),
            now_unix() + 3600,
        )],
    );
    let req = CapabilityRequest::FilesystemRead { globs: vec![] };
    assert!(caps.check("p", &req), "unexpired persistent grant must be allowed");
    assert!(
        caps.get_scope("p", &CapabilityDiscriminant::FilesystemRead).is_some(),
        "unexpired persistent grant must yield a scope"
    );
}

#[test]
fn expired_persistent_grant_falls_through_to_session_grant() {
    let mut caps = GrantedCapabilities::with_persistent(
        "p",
        vec![(CapabilityDiscriminant::FilesystemRead, CapabilityScope::default(), now_unix() - 10)],
    );
    caps.grant_session(CapabilityDiscriminant::FilesystemRead, CapabilityScope::default());
    let req = CapabilityRequest::FilesystemRead { globs: vec![] };
    assert!(
        caps.check("p", &req),
        "a still-current session grant must authorize despite the expired persistent grant"
    );
    assert!(
        caps.get_scope("p", &CapabilityDiscriminant::FilesystemRead).is_some(),
        "session scope must still be returned when the persistent grant expired"
    );
}

// ------------------------------------------------------------------
// CapabilityDiscriminant / CapabilityScope conversions
// ------------------------------------------------------------------

/// Every `CapabilityRequest` variant maps to the correct `CapabilityDiscriminant`.
#[test]
fn test_capability_discriminant_from_request_all_variants() {
    use concerto_api_types::plugin::CapabilityRequest;

    let cases: Vec<(CapabilityRequest, CapabilityDiscriminant)> = vec![
        (
            CapabilityRequest::FilesystemRead { globs: vec![] },
            CapabilityDiscriminant::FilesystemRead,
        ),
        (
            CapabilityRequest::FilesystemWrite { globs: vec![] },
            CapabilityDiscriminant::FilesystemWrite,
        ),
        (
            CapabilityRequest::NetworkOutbound { domains: vec![] },
            CapabilityDiscriminant::NetworkOutbound,
        ),
        (
            CapabilityRequest::ShellExecute { allowlist: vec![] },
            CapabilityDiscriminant::ShellExecute,
        ),
        (CapabilityRequest::Other { description: "custom".into() }, CapabilityDiscriminant::Other),
    ];

    for (req, expected) in &cases {
        let disc: CapabilityDiscriminant = req.into();
        assert_eq!(disc, *expected, "mismatch for {req:?}: expected {expected:?}, got {disc:?}",);
    }
}

/// Every `CapabilityRequest` variant produces the correct `CapabilityScope`.
#[test]
fn test_capability_scope_from_request_all_variants() {
    use concerto_api_types::plugin::CapabilityRequest;

    let cases: Vec<(CapabilityRequest, CapabilityScope)> = vec![
        (
            CapabilityRequest::FilesystemRead { globs: vec!["*.rs".into()] },
            CapabilityScope { globs: vec!["*.rs".into()], ..Default::default() },
        ),
        (
            CapabilityRequest::FilesystemWrite { globs: vec!["/tmp/*".into()] },
            CapabilityScope { globs: vec!["/tmp/*".into()], ..Default::default() },
        ),
        (
            CapabilityRequest::NetworkOutbound { domains: vec!["example.com".into()] },
            CapabilityScope { domains: vec!["example.com".into()], ..Default::default() },
        ),
        (
            CapabilityRequest::ShellExecute { allowlist: vec!["git status".into()] },
            CapabilityScope { allowlist: vec!["git status".into()], ..Default::default() },
        ),
        (CapabilityRequest::Other { description: "custom".into() }, CapabilityScope::default()),
    ];

    for (req, expected) in &cases {
        let scope: CapabilityScope = req.into();
        assert_eq!(scope, *expected, "mismatch for {req:?}: expected {expected:?}, got {scope:?}",);
    }
}
