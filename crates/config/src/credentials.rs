use concerto_core::error::ConfigError;

use crate::legacy;

/// Secure credential storage (ADR-04). Production path uses the OS keychain
/// via `keyring`. Test mode reads `CONCERTO_<KEY>` (or `OPENCODE_RS_<KEY>`
/// for legacy compatibility) env vars so CI never touches a real keychain
/// (see "Secure Credential Policy" in the roadmap).
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

    /// Retrieve a credential, with legacy fallback.
    ///
    /// In test mode, tries `CONCERTO_<KEY>` first, then `OPENCODE_RS_<KEY>`.
    /// In production, tries the `concerto` keyring service first, then
    /// `opencode-rs`.
    pub fn get(&self, account: &str) -> Result<String, ConfigError> {
        if self.test_mode {
            // Try new env prefix first, then legacy prefix
            let env_key = Self::new_env_key(account);
            if let Ok(val) = std::env::var(&env_key) {
                return Ok(val);
            }
            let legacy_key = Self::legacy_env_key(account);
            return std::env::var(&legacy_key)
                .map_err(|_| ConfigError::CredentialMissing(account.to_string()));
        }

        // Try new keyring service first
        if let Ok(entry) = keyring::Entry::new(legacy::NEW_KEYRING_SERVICE, account) {
            if let Ok(password) = entry.get_password() {
                return Ok(password);
            }
        }

        // Legacy fallback: try old keyring service
        let entry = keyring::Entry::new(legacy::OLD_KEYRING_SERVICE, account)
            .map_err(|e| ConfigError::Keychain(e.to_string()))?;
        entry.get_password().map_err(|_| ConfigError::CredentialMissing(account.to_string()))
    }

    /// Write a credential to the new keyring service only.
    /// Old credentials are never written — callers should use [`Self::get`]
    /// for reads which automatically falls back.
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

    /// Delete a credential from the new keyring service only.
    /// Old credentials are left in place — they will be found by [`Self::get`]
    /// fallback as long as they exist.
    pub fn delete(&self, account: &str) -> Result<(), ConfigError> {
        if self.test_mode {
            return Err(ConfigError::Keychain("cannot delete credentials in test mode".into()));
        }

        let entry = keyring::Entry::new(legacy::NEW_KEYRING_SERVICE, account)
            .map_err(|e| ConfigError::Keychain(e.to_string()))?;
        entry.delete_credential().map_err(|e| ConfigError::Keychain(e.to_string()))
    }

    /// Check if a credential exists (new or legacy service).
    pub fn exists(&self, account: &str) -> bool {
        self.get(account).is_ok()
    }

    /// `"anthropic/api_key"` -> `"CONCERTO_ANTHROPIC_API_KEY"`
    fn new_env_key(account: &str) -> String {
        format!("{}{}", legacy::NEW_ENV_PREFIX, account.to_uppercase().replace(['/', '-'], "_"),)
    }

    /// `"anthropic/api_key"` -> `"OPENCODE_RS_ANTHROPIC_API_KEY"`
    fn legacy_env_key(account: &str) -> String {
        format!("{}{}", legacy::OLD_ENV_PREFIX, account.to_uppercase().replace(['/', '-'], "_"),)
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
    fn test_mode_falls_back_to_legacy_prefix() {
        std::env::set_var("OPENCODE_RS_OPENAI_API_KEY", "sk-legacy-999");
        let store = CredentialStore::from_env();
        assert_eq!(store.get("openai/api_key").unwrap(), "sk-legacy-999");
        std::env::remove_var("OPENCODE_RS_OPENAI_API_KEY");
    }

    #[test]
    fn test_mode_new_prefix_takes_precedence() {
        std::env::set_var("CONCERTO_TEST_KEY", "new-value");
        std::env::set_var("OPENCODE_RS_TEST_KEY", "old-value");
        let store = CredentialStore::from_env();
        assert_eq!(store.get("test/key").unwrap(), "new-value");
        std::env::remove_var("CONCERTO_TEST_KEY");
        std::env::remove_var("OPENCODE_RS_TEST_KEY");
    }

    #[test]
    fn test_mode_missing_key_errors() {
        let store = CredentialStore::from_env();
        assert!(store.get("nonexistent/key").is_err());
    }

    #[test]
    fn new_env_key_format_is_correct() {
        let key = CredentialStore::new_env_key("anthropic/api_key");
        assert_eq!(key, "CONCERTO_ANTHROPIC_API_KEY");
    }

    #[test]
    fn legacy_env_key_format_is_correct() {
        let key = CredentialStore::legacy_env_key("openai/api_key");
        assert_eq!(key, "OPENCODE_RS_OPENAI_API_KEY");
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
