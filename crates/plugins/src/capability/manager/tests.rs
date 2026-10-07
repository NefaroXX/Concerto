use super::*;
// ------------------------------------------------------------------
// ADR-37 — grant lifecycle: TTL expiry, hash pinning, legacy migration,
// revocation (exercised through the public `CapabilityManager` API by
// writing `plugin_cap_grants.json` directly into a temp data dir).
// ------------------------------------------------------------------

/// Write `json` as `plugin_cap_grants.json` inside `dir` (creating it).
fn write_grants_json(dir: &std::path::Path, json: &str) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("plugin_cap_grants.json"), json).unwrap();
}

#[test]
fn grant_ttl_expiry_filters_grant() {
    let dir = std::env::temp_dir().join("cap_ttl_expiry_test");
    let _ = std::fs::remove_dir_all(&dir);
    write_grants_json(
        &dir,
        r#"{"my-plugin":[{"disc":"FilesystemRead","globs":[],"domains":[],"allowlist":[],"created_at":0,"expires_at":1,"manifest_hash":"abc"}]}"#,
    );
    let cap_mgr = CapabilityManager::open(&dir).unwrap();
    let grants = cap_mgr.load_grants("my-plugin", None);
    assert!(grants.is_empty(), "grant past its TTL must be filtered out");
}

#[test]
fn grant_within_ttl_loads() {
    let dir = std::env::temp_dir().join("cap_ttl_within_test");
    let _ = std::fs::remove_dir_all(&dir);
    write_grants_json(
        &dir,
        r#"{"my-plugin":[{"disc":"FilesystemRead","globs":["src/**/*.rs"],"domains":[],"allowlist":[],"created_at":0,"expires_at":18446744073709551615,"manifest_hash":"abc"}]}"#,
    );
    let cap_mgr = CapabilityManager::open(&dir).unwrap();
    let grants = cap_mgr.load_grants("my-plugin", None);
    assert_eq!(grants.len(), 1, "non-expired grant must load");
    assert_eq!(grants[0].0, CapabilityDiscriminant::FilesystemRead);
    assert_eq!(grants[0].1.globs, vec!["src/**/*.rs"], "scope globs must round-trip");
    assert_eq!(
        grants[0].2, 18_446_744_073_709_551_615,
        "expires_at must round-trip into the in-memory model"
    );
}

#[test]
fn hash_mismatch_filters_grant() {
    // Each behavior uses its own store file: prune-on-load destroys the
    // mismatched grant, so a single store cannot serve all three checks.
    let grants_json = r#"{"my-plugin":[{"disc":"FilesystemRead","globs":[],"domains":[],"allowlist":[],"created_at":0,"expires_at":9999999999,"manifest_hash":"abc"}]}"#;

    // Current hash differs from the pinned one → stale, filtered out.
    let dir = std::env::temp_dir().join("cap_hash_mismatch_test");
    let _ = std::fs::remove_dir_all(&dir);
    write_grants_json(&dir, grants_json);
    let cap_mgr = CapabilityManager::open(&dir).unwrap();
    assert!(cap_mgr.load_grants("my-plugin", Some("def")).is_empty());

    // Matching hash → grant loads (from a fresh, un-pruned store).
    let dir = std::env::temp_dir().join("cap_hash_match_test");
    let _ = std::fs::remove_dir_all(&dir);
    write_grants_json(&dir, grants_json);
    let cap_mgr = CapabilityManager::open(&dir).unwrap();
    assert_eq!(cap_mgr.load_grants("my-plugin", Some("abc")).len(), 1);

    // `None` skips hash pinning entirely → grant loads.
    let dir = std::env::temp_dir().join("cap_hash_none_test");
    let _ = std::fs::remove_dir_all(&dir);
    write_grants_json(&dir, grants_json);
    let cap_mgr = CapabilityManager::open(&dir).unwrap();
    assert_eq!(cap_mgr.load_grants("my-plugin", None).len(), 1);
}

#[test]
fn hash_mismatch_pruned_from_disk() {
    let dir = std::env::temp_dir().join("cap_hash_mismatch_prune_test");
    let _ = std::fs::remove_dir_all(&dir);
    write_grants_json(
        &dir,
        r#"{"my-plugin":[{"disc":"FilesystemRead","globs":[],"domains":[],"allowlist":[],"created_at":0,"expires_at":9999999999,"manifest_hash":"abc"}]}"#,
    );
    let cap_mgr = CapabilityManager::open(&dir).unwrap();
    // Loading with a differing hash prunes the stale grant (ADR-37
    // prune-on-load): a fresh manager over the same file must see nothing.
    assert!(cap_mgr.load_grants("my-plugin", Some("def")).is_empty());
    drop(cap_mgr);
    let reopened = CapabilityManager::open(&dir).unwrap();
    assert!(
        reopened.load_grants("my-plugin", None).is_empty(),
        "the hash-mismatched grant must be removed from disk after first load"
    );
    assert!(
        !reopened.list_granted_plugins().contains(&"my-plugin".to_string()),
        "pruned plugin must not be listed as granted"
    );
}

#[test]
fn legacy_grant_format_migrates_with_ttl() {
    let dir = std::env::temp_dir().join("cap_legacy_migration_test");
    let _ = std::fs::remove_dir_all(&dir);
    write_grants_json(&dir, r#"{"my-plugin":["FilesystemRead","BogusCap"]}"#);
    let cap_mgr = CapabilityManager::open(&dir).unwrap();
    let grants = cap_mgr.load_grants("my-plugin", None);
    assert_eq!(
        grants.len(),
        1,
        "only valid discriminants survive legacy migration (BogusCap dropped)"
    );
    assert_eq!(grants[0].0, CapabilityDiscriminant::FilesystemRead);
}

#[test]
fn revoke_plugin_removes_grants() {
    let dir = std::env::temp_dir().join("cap_revoke_test");
    let _ = std::fs::remove_dir_all(&dir);
    write_grants_json(
        &dir,
        r#"{"my-plugin":[{"disc":"FilesystemRead","globs":[],"domains":[],"allowlist":[],"created_at":0,"expires_at":9999999999,"manifest_hash":"abc"}]}"#,
    );
    let cap_mgr = CapabilityManager::open(&dir).unwrap();
    assert_eq!(cap_mgr.load_grants("my-plugin", None).len(), 1);
    cap_mgr.revoke_plugin("my-plugin").unwrap();
    assert!(cap_mgr.load_grants("my-plugin", None).is_empty());
    assert!(
        !cap_mgr.list_granted_plugins().contains(&"my-plugin".to_string()),
        "revoked plugin must not be listed"
    );
}

#[test]
fn list_granted_plugins_skips_expired() {
    let dir = std::env::temp_dir().join("cap_list_plugins_test");
    let _ = std::fs::remove_dir_all(&dir);
    write_grants_json(
        &dir,
        r#"{"valid-plugin":[{"disc":"FilesystemRead","globs":[],"domains":[],"allowlist":[],"created_at":0,"expires_at":9999999999,"manifest_hash":"abc"}],"expired-plugin":[{"disc":"FilesystemRead","globs":[],"domains":[],"allowlist":[],"created_at":0,"expires_at":1,"manifest_hash":"abc"}]}"#,
    );
    let cap_mgr = CapabilityManager::open(&dir).unwrap();
    let listed = cap_mgr.list_granted_plugins();
    assert!(listed.contains(&"valid-plugin".to_string()));
    assert!(!listed.contains(&"expired-plugin".to_string()));
}
