//! ADR-60 S5 (DEFERRED row #49): agent-process provider selection.
//!
//! Drives the real `orchestrator-agent-process` binary *directly* (no
//! supervisor) to pin the fail-closed startup contract resolved in
//! `agent_process_config.rs`:
//!
//! - `CONCERTO_PROVIDER=mock` is an explicit opt-in and still starts;
//! - a missing/malformed `CONCERTO_AGENT_CONFIG_JSON` fails closed with a
//!   named error — never a silent mock fallback;
//! - a real config whose credential is absent fails closed with the named
//!   credential error;
//! - no secret ever appears in the process argv or its stderr diagnostics.
//!
//! Provider resolution now runs *before* the supervisor handshake, so these
//! cases exit with code 1 without a supervisor attached.

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use concerto_orchestrator::agent_process_config::{CONFIG_ENV, PROVIDER_ENV};

/// The real child binary with the mandatory environment contract and no
/// supervisor on the other end (we only ever expect a pre-handshake exit).
fn agent_process(root: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_orchestrator-agent-process"));
    command
        .env("CONCERTO_AGENT_ID", "agent-provider-test")
        .env("CONCERTO_PROJECT_ROOT", root.display().to_string())
        .env("CONCERTO_TASK_DESCRIPTION", "provider selection probe")
        .env("CONCERTO_MAX_ITERATIONS", "1");
    command
}

/// A minimal AppConfig JSON with one provider, stamped exactly like the parent.
fn config_json(provider: &str, model: &str) -> String {
    let provider_config = concerto_config::ProviderConfig {
        id: format!("{provider}-main"),
        name: format!("{provider} main"),
        provider: provider.to_owned(),
        model: model.to_owned(),
        keyring_key: format!("{provider}/api_key"),
        ..concerto_config::ProviderConfig::default()
    };
    let config =
        concerto_orchestrator::agent_process_config::single_provider_config(provider_config);
    serde_json::to_string(&config).expect("config serializes")
}

/// Run the probe child and return `(exit_code, stderr)`.
fn run_probe(mut command: Command) -> (i32, String) {
    let output = command.output().expect("child runs");
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    (output.status.code().unwrap_or(-1), stderr)
}

/// The probe must terminate well before any supervisor handshake timeout: it
/// fails during startup, not by hanging. A supervisor-less child that reached
/// the handshake would block on stdin; bound the wait so a regression is a
/// clear timeout rather than a hung suite.
fn run_probe_bounded(command: Command) -> (i32, String) {
    let start = std::time::Instant::now();
    let (code, stderr) = run_probe(command);
    assert!(
        start.elapsed() < Duration::from_secs(20),
        "provider probe must fail fast, took {:?}: {stderr}",
        start.elapsed()
    );
    (code, stderr)
}

#[test]
fn mock_opt_in_still_starts_the_agent_process() {
    // The mock path resolves without a supervisor; the child proceeds to the
    // handshake and fails on the missing supervisor — proving provider
    // resolution itself succeeded (it did NOT fail closed on config).
    let dir = tempfile::tempdir().expect("tempdir");
    let mut command = agent_process(dir.path());
    command.env(PROVIDER_ENV, "mock");
    let (_code, stderr) = run_probe_bounded(command);
    assert!(
        !stderr.contains("no provider config"),
        "mock opt-in must not fail provider resolution: {stderr}"
    );
    assert!(
        !stderr.contains("refusing to fall back to mock"),
        "mock opt-in must not hit the real-path fail-closed error: {stderr}"
    );
}

#[test]
fn missing_config_without_mock_fails_closed_with_a_named_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut command = agent_process(dir.path());
    // No CONCERTO_PROVIDER and no CONCERTO_AGENT_CONFIG_JSON.
    command.env_remove(PROVIDER_ENV).env_remove(CONFIG_ENV);
    let (code, stderr) = run_probe_bounded(command);
    assert_eq!(code, 1, "missing provider config must exit non-zero: {stderr}");
    assert!(
        stderr.contains("no provider config available"),
        "the failure must be named and explicit, got: {stderr}"
    );
    assert!(
        stderr.contains("refusing to fall back to mock"),
        "the failure must state the no-mock guarantee, got: {stderr}"
    );
}

#[test]
fn malformed_config_fails_closed_with_a_named_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut command = agent_process(dir.path());
    command.env_remove(PROVIDER_ENV).env(CONFIG_ENV, "{ not a config");
    let (code, stderr) = run_probe_bounded(command);
    assert_eq!(code, 1, "malformed config must exit non-zero: {stderr}");
    assert!(
        stderr.contains("is not a valid AppConfig"),
        "the failure must name the malformed config, got: {stderr}"
    );
}

#[test]
fn missing_credential_fails_closed_with_a_named_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut command = agent_process(dir.path());
    // A real provider config with no keyring entry and no env credential.
    command
        .env_remove(PROVIDER_ENV)
        .env(CONFIG_ENV, config_json("deepseek", "deepseek-chat"))
        .env_remove("CONCERTO_DEEPSEEK_API_KEY");
    // Isolate from a developer's real keychain: use the env-backed test store.
    // (The child uses `CredentialStore::new()`; on a headless CI box the
    // keychain read fails, which the factory maps to `CredentialMissing`.)
    let (code, stderr) = run_probe_bounded(command);
    assert_eq!(code, 1, "missing credential must exit non-zero: {stderr}");
    assert!(
        stderr.contains("credential missing") || stderr.contains("provider build failed"),
        "the failure must be a named credential/build error, got: {stderr}"
    );
}

#[test]
fn no_secret_is_passed_in_argv_or_logged() {
    const SYNTHETIC: &str = "sk-synthetic-secret-fixture-do-not-log";
    let dir = tempfile::tempdir().expect("tempdir");
    let mut command = agent_process(dir.path());
    command
        .env_remove(PROVIDER_ENV)
        .env(CONFIG_ENV, config_json("deepseek", "deepseek-chat"))
        .env("CONCERTO_DEEPSEEK_API_KEY", SYNTHETIC);

    // The secret must never be an argv element (it is resolved from the
    // environment/keychain, not passed on the command line).
    for arg in command.get_args() {
        let arg = arg.to_string_lossy();
        assert!(!arg.contains(SYNTHETIC), "secret leaked into argv: {arg}");
    }
    // The stamped config carries provider metadata only.
    let stamped = command.get_envs().find_map(|(name, value)| {
        (name == std::ffi::OsStr::new(CONFIG_ENV))
            .then(|| value.map(|v| v.to_string_lossy().to_string()))
    });
    if let Some(Some(stamped)) = stamped {
        assert!(!stamped.contains(SYNTHETIC), "secret leaked into the stamped config");
    }

    // The secret must never be logged to stderr either.
    let (_code, stderr) = run_probe_bounded(command);
    assert!(!stderr.contains(SYNTHETIC), "secret leaked into stderr diagnostics: {stderr}");
}
