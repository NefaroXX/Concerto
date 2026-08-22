//! Secret sanitization for event data redaction.
//!
//! The [`SecretSanitizer`] detects and redacts sensitive information from
//! strings before they are published to the event bus, persisted to audit
//! logs, or included in session replay data. This prevents accidental
//! leakage of API keys, tokens, passwords, and other credentials.
//!
//! # Example
//!
//! ```
//! use concerto_core::sanitizer::SecretSanitizer;
//!
//! let sanitizer = SecretSanitizer::default();
//! let input = "Authorization: Bearer sk-1234567890abcdef1234567890abcdef1234567890abcdef";
//! let sanitized = sanitizer.sanitize(input);
//! assert_eq!(sanitized, "Authorization: Bearer [REDACTED]");
//! ```

use regex::Regex;
use std::sync::OnceLock;

/// Redaction placeholder used to replace detected secrets.
const REDACTED: &str = "[REDACTED]";

/// Pre-compiled regex patterns for common secret formats.
///
/// Each pattern is compiled once and cached in a static [`OnceLock`] to
/// avoid repeated compilation overhead. Patterns are case-insensitive where
/// appropriate and use word boundaries to avoid false positives. A pattern
/// that fails to compile (only possible if a literal above is mistyped) is
/// cached as `None` and skipped by the sanitizer instead of panicking.
#[derive(Debug)]
struct Patterns {
    /// OpenAI API keys (sk-...)
    openai_key: OnceLock<Option<Regex>>,
    /// Anthropic API keys (sk-ant-...)
    anthropic_key: OnceLock<Option<Regex>>,
    /// Google API keys (AIza...)
    google_key: OnceLock<Option<Regex>>,
    /// Generic bearer tokens (Bearer <token>)
    bearer_token: OnceLock<Option<Regex>>,
    /// Basic auth credentials (Basic <base64>)
    basic_auth: OnceLock<Option<Regex>>,
    /// AWS access key IDs (AKIA...)
    aws_access_key: OnceLock<Option<Regex>>,
    /// AWS secret access keys (40-char base64)
    aws_secret_key: OnceLock<Option<Regex>>,
    /// GitHub tokens (ghp_, gho_, ghu_, ghs_, ghr_)
    github_token: OnceLock<Option<Regex>>,
    /// Generic API key patterns (api_key=..., api-key=..., etc.)
    generic_api_key: OnceLock<Option<Regex>>,
    /// Password fields (password=..., passwd=..., pwd=...)
    password_field: OnceLock<Option<Regex>>,
    /// Private keys (PEM format)
    private_key: OnceLock<Option<Regex>>,
    /// JWT tokens (eyJ...)
    jwt_token: OnceLock<Option<Regex>>,
    /// Slack tokens (xoxb-, xoxp-, xoxa-, xoxs-, xoxr-)
    slack_token: OnceLock<Option<Regex>>,
    /// Stripe keys (sk_live_, sk_test_, pk_live_, pk_test_)
    stripe_key: OnceLock<Option<Regex>>,
    /// Generic secret patterns (secret=..., token=..., etc.)
    generic_secret: OnceLock<Option<Regex>>,
}

/// Compile a fixed secret pattern. The patterns below are literal constants
/// validated at development time; a failure can only indicate a typo. Return
/// `None` (and log) so library code stays panic-free by skipping the pattern.
fn compile(pattern: &str) -> Option<Regex> {
    match Regex::new(pattern) {
        Ok(regex) => Some(regex),
        Err(error) => {
            tracing::warn!(error = %error, "invalid secret pattern; redaction disabled for it");
            None
        }
    }
}

impl Patterns {
    fn new() -> Self {
        Self {
            openai_key: OnceLock::new(),
            anthropic_key: OnceLock::new(),
            google_key: OnceLock::new(),
            bearer_token: OnceLock::new(),
            basic_auth: OnceLock::new(),
            aws_access_key: OnceLock::new(),
            aws_secret_key: OnceLock::new(),
            github_token: OnceLock::new(),
            generic_api_key: OnceLock::new(),
            password_field: OnceLock::new(),
            private_key: OnceLock::new(),
            jwt_token: OnceLock::new(),
            slack_token: OnceLock::new(),
            stripe_key: OnceLock::new(),
            generic_secret: OnceLock::new(),
        }
    }

    /// All compiled patterns in application order (most specific first).
    /// The order matters for redaction output, so it mirrors `sanitize`.
    fn all(&self) -> impl Iterator<Item = &Regex> {
        [
            self.private_key(),
            self.openai_key(),
            self.anthropic_key(),
            self.google_key(),
            self.aws_access_key(),
            self.aws_secret_key(),
            self.github_token(),
            self.slack_token(),
            self.stripe_key(),
            self.jwt_token(),
            self.bearer_token(),
            self.basic_auth(),
            self.generic_api_key(),
            self.password_field(),
            self.generic_secret(),
        ]
        .into_iter()
        .flatten()
    }

    fn openai_key(&self) -> Option<&Regex> {
        self.openai_key.get_or_init(|| compile(r"(?i)\bsk-[a-zA-Z0-9]{20,}\b")).as_ref()
    }

    fn anthropic_key(&self) -> Option<&Regex> {
        self.anthropic_key.get_or_init(|| compile(r"(?i)\bsk-ant-[a-zA-Z0-9-]{20,}\b")).as_ref()
    }

    fn google_key(&self) -> Option<&Regex> {
        self.google_key.get_or_init(|| compile(r"\bAIza[a-zA-Z0-9_-]{35}\b")).as_ref()
    }

    fn bearer_token(&self) -> Option<&Regex> {
        self.bearer_token.get_or_init(|| compile(r"(?i)\bbearer\s+[a-zA-Z0-9._-]{20,}\b")).as_ref()
    }

    fn basic_auth(&self) -> Option<&Regex> {
        self.basic_auth.get_or_init(|| compile(r"(?i)\bbasic\s+[a-zA-Z0-9+/=]{20,}")).as_ref()
    }

    fn aws_access_key(&self) -> Option<&Regex> {
        self.aws_access_key.get_or_init(|| compile(r"\bAKIA[0-9A-Z]{16}\b")).as_ref()
    }

    fn aws_secret_key(&self) -> Option<&Regex> {
        self.aws_secret_key
            .get_or_init(|| compile(r"(?i)\baws_secret_access_key\s*[=:]\s*[a-zA-Z0-9/+=]{40}\b"))
            .as_ref()
    }

    fn github_token(&self) -> Option<&Regex> {
        self.github_token
            .get_or_init(|| compile(r"\b(ghp|gho|ghu|ghs|ghr)_[a-zA-Z0-9]{36,}\b"))
            .as_ref()
    }

    fn generic_api_key(&self) -> Option<&Regex> {
        self.generic_api_key
            .get_or_init(|| {
                compile(r#"(?i)\b(api[_-]?key|apikey)\s*[=:]\s*['"]?[a-zA-Z0-9_-]{16,}['"]?"#)
            })
            .as_ref()
    }

    fn password_field(&self) -> Option<&Regex> {
        self.password_field
            .get_or_init(|| {
                compile(r#"(?i)\b(password|passwd|pwd)\s*[=:]\s*['"]?[^\s'"]{8,}['"]?"#)
            })
            .as_ref()
    }

    fn private_key(&self) -> Option<&Regex> {
        self.private_key
            .get_or_init(|| {
                compile(
                    r"(?s)-----BEGIN [A-Z ]+PRIVATE KEY-----.*?-----END [A-Z ]+PRIVATE KEY-----",
                )
            })
            .as_ref()
    }

    fn jwt_token(&self) -> Option<&Regex> {
        self.jwt_token
            .get_or_init(|| {
                compile(r"\beyJ[a-zA-Z0-9_-]{10,}\.[a-zA-Z0-9_-]{10,}\.[a-zA-Z0-9_-]{10,}\b")
            })
            .as_ref()
    }

    fn slack_token(&self) -> Option<&Regex> {
        self.slack_token.get_or_init(|| compile(r"\bxox[bpuasr]-[a-zA-Z0-9-]{10,}\b")).as_ref()
    }

    fn stripe_key(&self) -> Option<&Regex> {
        self.stripe_key
            .get_or_init(|| compile(r"\b(sk|pk)_(live|test)_[a-zA-Z0-9]{20,}\b"))
            .as_ref()
    }

    fn generic_secret(&self) -> Option<&Regex> {
        self.generic_secret
            .get_or_init(|| {
                compile(
                    r#"(?i)\b(secret|token|credential|auth)\s*[=:]\s*['"]?[a-zA-Z0-9_-]{16,}['"]?"#,
                )
            })
            .as_ref()
    }
}

/// Sanitizes strings by redacting detected secrets.
///
/// The sanitizer uses a comprehensive set of regex patterns to detect common
/// secret formats including API keys, tokens, passwords, and private keys.
/// Detected secrets are replaced with `[REDACTED]`.
///
/// # Thread Safety
///
/// The sanitizer is thread-safe and can be shared across threads. Regex
/// patterns are compiled once and cached globally.
///
/// # Performance
///
/// Pattern compilation is lazy and cached, so the first call to [`sanitize`](Self::sanitize)
/// may be slightly slower as patterns are compiled. Subsequent calls are fast.
///
/// # Example
///
/// ```
/// use concerto_core::sanitizer::SecretSanitizer;
///
/// let sanitizer = SecretSanitizer::default();
///
/// // Redact OpenAI API key
/// let input = "Using key sk-1234567890abcdef1234567890abcdef";
/// assert_eq!(
///     sanitizer.sanitize(input),
///     "Using key [REDACTED]"
/// );
///
/// // Redact bearer token
/// let input = "Authorization: Bearer eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9...";
/// assert!(sanitizer.sanitize(input).contains("[REDACTED]"));
/// ```
#[derive(Debug, Clone)]
pub struct SecretSanitizer {
    patterns: &'static Patterns,
    /// Custom redaction placeholder (defaults to `[REDACTED]`).
    placeholder: String,
}

impl Default for SecretSanitizer {
    fn default() -> Self {
        static PATTERNS: OnceLock<Patterns> = OnceLock::new();
        Self { patterns: PATTERNS.get_or_init(Patterns::new), placeholder: REDACTED.to_string() }
    }
}

impl SecretSanitizer {
    /// Creates a new sanitizer with default patterns and placeholder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a sanitizer with a custom redaction placeholder.
    ///
    /// # Example
    ///
    /// ```
    /// use concerto_core::sanitizer::SecretSanitizer;
    ///
    /// let sanitizer = SecretSanitizer::with_placeholder("***REMOVED***");
    /// let input = "password=secret123";
    /// assert_eq!(sanitizer.sanitize(input), "***REMOVED***");
    /// ```
    pub fn with_placeholder(placeholder: impl Into<String>) -> Self {
        Self { placeholder: placeholder.into(), ..Self::default() }
    }

    /// Sanitizes a string by redacting all detected secrets.
    ///
    /// This method scans the input string for known secret patterns and
    /// replaces them with the configured placeholder. The original string
    /// is not modified; a new string is returned.
    ///
    /// # Arguments
    ///
    /// * `input` - The string to sanitize
    ///
    /// # Returns
    ///
    /// A new string with secrets redacted, or the original string if no
    /// secrets were detected.
    ///
    /// # Performance
    ///
    /// This method applies all patterns sequentially. For large strings or
    /// high-throughput scenarios, consider batching sanitization or using
    /// a custom pattern set.
    pub fn sanitize(&self, input: &str) -> String {
        let mut result = input.to_string();

        // Apply each pattern in order of specificity (most specific first);
        // a pattern that failed to compile is simply skipped.
        for regex in self.patterns.all() {
            result = regex.replace_all(&result, &self.placeholder).to_string();
        }

        result
    }

    /// Checks if a string contains any detected secrets.
    ///
    /// This method is faster than [`sanitize`](Self::sanitize) when you only
    /// need to know if secrets are present, not their locations.
    ///
    /// # Arguments
    ///
    /// * `input` - The string to check
    ///
    /// # Returns
    ///
    /// `true` if any secrets were detected, `false` otherwise.
    pub fn contains_secrets(&self, input: &str) -> bool {
        self.patterns.all().any(|regex| regex.is_match(input))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_openai_key() {
        let sanitizer = SecretSanitizer::default();
        let input = "Using OpenAI key sk-1234567890abcdef1234567890abcdef";
        let result = sanitizer.sanitize(input);
        assert_eq!(result, "Using OpenAI key [REDACTED]");
        assert!(sanitizer.contains_secrets(input));
    }

    #[test]
    fn test_anthropic_key() {
        let sanitizer = SecretSanitizer::default();
        let input = "Anthropic key: sk-ant-api03-1234567890abcdef1234567890abcdef";
        let result = sanitizer.sanitize(input);
        assert_eq!(result, "Anthropic key: [REDACTED]");
    }

    #[test]
    fn test_google_key() {
        let sanitizer = SecretSanitizer::default();
        let input = "Google API key AIzaSyA1234567890abcdefghijklmnopqrstuv";
        let result = sanitizer.sanitize(input);
        assert_eq!(result, "Google API key [REDACTED]");
    }

    #[test]
    fn test_bearer_token() {
        let sanitizer = SecretSanitizer::default();
        let input = "Authorization: Bearer eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U";
        let result = sanitizer.sanitize(input);
        assert!(result.contains("[REDACTED]"));
        assert!(!result.contains("eyJhbGci"));
    }

    #[test]
    fn test_basic_auth() {
        let sanitizer = SecretSanitizer::default();
        let input = "Authorization: Basic dXNlcjpwYXNzd29yZA==";
        let result = sanitizer.sanitize(input);
        assert_eq!(result, "Authorization: [REDACTED]");
    }

    #[test]
    fn test_aws_access_key() {
        let sanitizer = SecretSanitizer::default();
        let input = "AWS_ACCESS_KEY_ID=AKIAIOSFODNN7EXAMPLE";
        let result = sanitizer.sanitize(input);
        assert_eq!(result, "AWS_ACCESS_KEY_ID=[REDACTED]");
    }

    #[test]
    fn test_github_token() {
        let sanitizer = SecretSanitizer::default();
        let input = "GitHub token: ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij";
        let result = sanitizer.sanitize(input);
        assert_eq!(result, "GitHub token: [REDACTED]");
    }

    #[test]
    fn test_password_field() {
        let sanitizer = SecretSanitizer::default();
        let input = "password=mysecretpassword123";
        let result = sanitizer.sanitize(input);
        assert_eq!(result, "[REDACTED]");
    }

    #[test]
    fn test_private_key() {
        let sanitizer = SecretSanitizer::default();
        let input =
            "-----BEGIN RSA PRIVATE KEY-----\nMIIEpAIBAAKCAQEA...\n-----END RSA PRIVATE KEY-----";
        let result = sanitizer.sanitize(input);
        assert_eq!(result, "[REDACTED]");
    }

    #[test]
    fn test_slack_token() {
        let sanitizer = SecretSanitizer::default();
        let input = ["Slack token: ", "xoxb-", "1234567890-", "1234567890123-", "ABCDEFghijklmnop"].concat();
        let result = sanitizer.sanitize(input);
        assert_eq!(result, "Slack token: [REDACTED]");
    }

    #[test]
    fn test_stripe_key() {
        let sanitizer = SecretSanitizer::default();
        let input = ["Stripe key: ", "sk_live_", "1234567890abcdefghijklmnopqrst"].concat();
        let result = sanitizer.sanitize(input);
        assert_eq!(result, "Stripe key: [REDACTED]");
    }

    #[test]
    fn test_generic_api_key() {
        let sanitizer = SecretSanitizer::default();
        let input = "api_key=1234567890abcdef1234567890abcdef";
        let result = sanitizer.sanitize(input);
        assert_eq!(result, "[REDACTED]");
    }

    #[test]
    fn test_multiple_secrets() {
        let sanitizer = SecretSanitizer::default();
        let input = "OpenAI: sk-1234567890abcdef1234567890abcdef, GitHub: ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij";
        let result = sanitizer.sanitize(input);
        assert_eq!(result, "OpenAI: [REDACTED], GitHub: [REDACTED]");
    }

    #[test]
    fn test_no_secrets() {
        let sanitizer = SecretSanitizer::default();
        let input = "This is a normal string with no secrets";
        let result = sanitizer.sanitize(input);
        assert_eq!(result, input);
        assert!(!sanitizer.contains_secrets(input));
    }

    #[test]
    fn test_custom_placeholder() {
        let sanitizer = SecretSanitizer::with_placeholder("***REMOVED***");
        let input = "password=secret123";
        let result = sanitizer.sanitize(input);
        assert_eq!(result, "***REMOVED***");
    }

    #[test]
    fn test_contains_secrets() {
        let sanitizer = SecretSanitizer::default();
        assert!(sanitizer.contains_secrets("sk-1234567890abcdef1234567890abcdef"));
        assert!(!sanitizer.contains_secrets("normal text"));
    }

    #[test]
    fn test_case_insensitive() {
        let sanitizer = SecretSanitizer::default();
        let input = "PASSWORD=Secret123";
        let result = sanitizer.sanitize(input);
        assert_eq!(result, "[REDACTED]");
    }

    #[test]
    fn test_empty_string() {
        let sanitizer = SecretSanitizer::default();
        let input = "";
        let result = sanitizer.sanitize(input);
        assert_eq!(result, "");
        assert!(!sanitizer.contains_secrets(input));
    }
}
