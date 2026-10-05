use concerto_core::error::ConfigError;
use concerto_core::SecretString;

use crate::legacy;

/// Secure credential storage (ADR-04). Production path uses the OS keychain
/// via `keyring`. Test mode reads `CONCERTO_<KEY>` env vars so CI never
/// touches a real keychain (see "Secure Credential Policy" in the roadmap).
pub struct CredentialStore {
    test_mode: bool,
}

impl CredentialStore {
    /// Production constructor: backed by the OS keychain.
    pub fn new() -> Self {
        Self { test_mode: false }
    }

    /// Test-mode constructor: backed by environment variables instead of
    /// the OS keychain. Used in CI and unit tests exclusively.
    pub fn from_env() -> Self {
        Self { test_mode: true }
    }

    /// Retrieve a credential.
    ///
    /// In test mode, reads `CONCERTO_<KEY>` from the environment. In
    /// production, reads the `concerto` keyring service.
    ///
    /// Prefer [`Self::get_secret`] for call sites that go on to hand the
    /// value to a long-lived holder; this method returns a plain `String`
    /// for the existing callers that only test presence or forward once.
    pub fn get(&self, account: &str) -> Result<String, ConfigError> {
        self.fetch(account)
    }

    /// Like [`Self::get`], but the credential comes back in a zero-on-drop,
    /// non-rendering [`SecretString`] instead of a plain `String`.
    ///
    /// Use this whenever the value survives past the statement that read it
    /// (a provider instance, a pending config): the holder wipes the buffer
    /// when it drops and redacts itself under `Debug`/`Display`, so a
    /// stray `{:?}` in a log line cannot leak it.
    pub fn get_secret(&self, account: &str) -> Result<SecretString, ConfigError> {
        self.fetch(account).map(SecretString::from)
    }

    /// Shared read path for [`Self::get`] and [`Self::get_secret`].
    ///
    /// Test mode reads `CONCERTO_<KEY>` from the environment; production
    /// reads the `concerto` keyring service.
    fn fetch(&self, account: &str) -> Result<String, ConfigError> {
        if self.test_mode {
            return std::env::var(Self::new_env_key(account))
                .map_err(|_| ConfigError::CredentialMissing(account.to_string()));
        }

        let entry = keyring::Entry::new(legacy::NEW_KEYRING_SERVICE, account)
            .map_err(|e| ConfigError::Keychain(e.to_string()))?;
        entry.get_password().map_err(|_| ConfigError::CredentialMissing(account.to_string()))
    }

    /// Write a credential to the `concerto` keyring service.
    pub fn set(&self, account: &str, value: &str) -> Result<(), ConfigError> {
        if self.test_mode {
            return Err(ConfigError::Keychain(
                "cannot write credentials in test mode; set the env var instead".into(),
            ));
        }

        let entry = keyring::Entry::new(legacy::NEW_KEYRING_SERVICE, account)
            .map_err(|e| ConfigError::Keychain(e.to_string()))?;
        entry.set_password(value).map_err(|e| ConfigError::Keychain(e.to_string()))
    }

    /// Delete a credential from the `concerto` keyring service.
    pub fn delete(&self, account: &str) -> Result<(), ConfigError> {
        if self.test_mode {
            return Err(ConfigError::Keychain("cannot delete credentials in test mode".into()));
        }

        let entry = keyring::Entry::new(legacy::NEW_KEYRING_SERVICE, account)
            .map_err(|e| ConfigError::Keychain(e.to_string()))?;
        entry.delete_credential().map_err(|e| ConfigError::Keychain(e.to_string()))
    }

    /// Check if a credential exists.
    pub fn exists(&self, account: &str) -> bool {
        self.get(account).is_ok()
    }

    /// `"anthropic/api_key"` -> `"CONCERTO_ANTHROPIC_API_KEY"`
    fn new_env_key(account: &str) -> String {
        format!("{}{}", legacy::NEW_ENV_PREFIX, account.to_uppercase().replace(['/', '-'], "_"),)
    }
}

impl Default for CredentialStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mode_reads_env_var() {
        std::env::set_var("CONCERTO_ANTHROPIC_API_KEY", "sk-test-123");
        let store = CredentialStore::from_env();
        assert_eq!(store.get("anthropic/api_key").unwrap(), "sk-test-123");
        std::env::remove_var("CONCERTO_ANTHROPIC_API_KEY");
    }

    #[test]
    fn test_mode_missing_key_errors() {
        let store = CredentialStore::from_env();
        assert!(store.get("nonexistent/key").is_err());
    }

    /// `get_secret` must hand back the same bytes as `get` while refusing to
    /// render them. Fixture is synthetic — never a real credential.
    #[test]
    fn get_secret_reads_the_same_credential_and_redacts_it() {
        const SYNTHETIC: &str = "sk-synthetic-credential-fixture";
        std::env::set_var("CONCERTO_CREDENTIAL_FIXTURE", SYNTHETIC);
        let store = CredentialStore::from_env();

        let secret = store.get_secret("credential/fixture").expect("fixture must resolve");
        assert_eq!(secret.expose(), SYNTHETIC);
        assert_eq!(secret.expose(), store.get("credential/fixture").unwrap());

        let rendered = format!("{secret:?} | {secret}");
        assert!(!rendered.contains(SYNTHETIC), "secret leaked into formatting: {rendered}");
        assert!(rendered.contains("[REDACTED]"), "redaction marker missing: {rendered}");

        std::env::remove_var("CONCERTO_CREDENTIAL_FIXTURE");
    }

    #[test]
    fn get_secret_missing_key_errors_like_get() {
        let store = CredentialStore::from_env();
        assert!(store.get_secret("nonexistent/key").is_err());
    }

    #[test]
    fn new_env_key_format_is_correct() {
        let key = CredentialStore::new_env_key("anthropic/api_key");
        assert_eq!(key, "CONCERTO_ANTHROPIC_API_KEY");
    }

    #[test]
    fn set_in_test_mode_returns_error() {
        let store = CredentialStore::from_env();
        let err = store.set("any/key", "any-value").unwrap_err();
        assert!(format!("{err}").contains("test mode"));
    }

    #[test]
    fn delete_in_test_mode_returns_error() {
        let store = CredentialStore::from_env();
        let err = store.delete("any/key").unwrap_err();
        assert!(format!("{err}").contains("test mode"));
    }

    #[test]
    fn default_store_is_not_test_mode() {
        let store = CredentialStore::new();
        // The production constructor must not be env-backed.
        assert!(!store.test_mode);
        // NB: the CredentialMissing-vs-Keychain distinction for `get` on a
        // missing key depends on the OS credential backend: on Linux without
        // a Secret Service daemon (headless CI), `keyring::Entry::open`
        // surfaces a platform error and the code maps it to
        // ConfigError::Keychain. That behavior is environment-dependent, so
        // it is exercised by live tests (docs/live-test-template.md) instead
        // of this unit test — keeping the suite deterministic everywhere.
    }
}
