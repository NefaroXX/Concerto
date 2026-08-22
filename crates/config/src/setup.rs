//! First-run setup wizard.
//!
//! Provides an interactive CLI prompt sequence for initial configuration:
//! provider selection, API key, model, working directory, and policy mode.
//! The wizard is generic over reader/writer for testability.

use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::CredentialStore;

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum SetupError {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("TOML serialization error: {0}")]
    Toml(#[from] toml::ser::Error),
    #[error("validation error: {0}")]
    Validation(String),
    #[error("config file already exists: {0}")]
    ConfigExists(PathBuf),
}

// ---------------------------------------------------------------------------
// Provider model
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub enum ProviderKind {
    OpenAI,
    Anthropic,
    Ollama,
    Nvidianim,
    OpenRouter,
    OpenCodeZen,
    Other,
}

impl ProviderKind {
    fn default_model(&self) -> &'static str {
        match self {
            ProviderKind::OpenAI => "gpt-4o",
            ProviderKind::Anthropic => "claude-3-opus-20240229",
            ProviderKind::Ollama => "llama3",
            ProviderKind::Nvidianim => "meta/llama-3.3-70b-instruct",
            ProviderKind::OpenRouter => "openai/gpt-4o",
            ProviderKind::OpenCodeZen => "deepseek-v4-flash",
            ProviderKind::Other => "custom",
        }
    }
}

// ---------------------------------------------------------------------------
// Policy rule builder
// ---------------------------------------------------------------------------

/// Build a single policy rule value for the generated config:
/// `{ action = "<action>", condition = { always = true } }`.
fn policy_rule(action: &str) -> toml::Value {
    let mut condition = toml::Table::new();
    condition.insert("always".to_string(), toml::Value::Boolean(true));
    let mut rule = toml::Table::new();
    rule.insert("action".to_string(), toml::Value::String(action.to_string()));
    rule.insert("condition".to_string(), toml::Value::Table(condition));
    toml::Value::Table(rule)
}

// ---------------------------------------------------------------------------
// PendingConfig — wizard results before persisting
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingConfig {
    pub provider: String,
    pub api_key: String,
    pub model: String,
    pub working_dir: PathBuf,
    pub policy_mode: String,
}

impl PendingConfig {
    /// Save the configuration to a TOML file and persist the API key
    /// to the OS keychain via `CredentialStore` (ADR-04). The API key is
    /// never written to the TOML file.
    ///
    /// Fails with `ConfigExists` if the file already exists. Use
    /// [`save_overwrite`] to replace an existing config (e.g. for
    /// `--reconfigure`).
    pub fn save(
        &self,
        config_path: &Path,
        credentials: &CredentialStore,
    ) -> Result<(), SetupError> {
        if config_path.exists() {
            return Err(SetupError::ConfigExists(config_path.to_path_buf()));
        }
        self.write_config(config_path, credentials)
    }

    /// Save the configuration, overwriting any existing file.
    ///
    /// Used by `--reconfigure` which re-runs the wizard on an existing
    /// installation.
    pub fn save_overwrite(
        &self,
        config_path: &Path,
        credentials: &CredentialStore,
    ) -> Result<(), SetupError> {
        self.write_config(config_path, credentials)
    }

    /// Shared implementation: persist the API key and write the TOML config.
    fn write_config(
        &self,
        config_path: &Path,
        credentials: &CredentialStore,
    ) -> Result<(), SetupError> {
        // Persist API key to OS keychain (not TOML — see ADR-04).
        if !self.api_key.is_empty() {
            let account = format!("{}/api_key", self.provider);
            credentials
                .set(&account, &self.api_key)
                .map_err(|e| SetupError::Validation(format!("failed to store API key: {e}")))?;
        }

        if let Some(parent) = config_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let keyring_key = format!("{}/api_key", self.provider);

        // Build the document structurally so that free-text fields (above all
        // the model name) are serialized as TOML string values instead of being
        // interpolated as raw document fragments. Interpolation allowed a model
        // name containing `"` and newlines to inject extra tables/keys into the
        // generated config (issue #104).
        let mut provider = toml::Table::new();
        provider.insert("id".to_string(), toml::Value::String(format!("prov_{}", self.provider)));
        provider.insert("name".to_string(), toml::Value::String(self.provider.clone()));
        provider.insert("provider".to_string(), toml::Value::String(self.provider.clone()));
        provider.insert("model".to_string(), toml::Value::String(self.model.clone()));
        provider.insert("keyring_key".to_string(), toml::Value::String(keyring_key));
        provider.insert("timeout_seconds".to_string(), toml::Value::Integer(30));

        let mut model_settings = toml::Table::new();
        model_settings
            .insert("global_default_model".to_string(), toml::Value::String(self.model.clone()));
        model_settings.insert(
            "providers".to_string(),
            toml::Value::Array(vec![toml::Value::Table(provider)]),
        );

        let mut root = toml::Table::new();
        root.insert("schema_version".to_string(), toml::Value::Integer(4));
        root.insert("model_settings".to_string(), toml::Value::Table(model_settings));

        // `policy_mode` is one of a fixed set of wizard choices, never free
        // text; only emit a `[policy]` table when the rules are non-empty.
        let rules = match self.policy_mode.as_str() {
            "strict" | "conservative" => vec![policy_rule("require_approval")],
            "autonomous" | "expert" => vec![policy_rule("auto_approve")],
            _ => vec![], // "safe" / legacy "permissive" use the built-in safe default.
        };
        if !rules.is_empty() {
            let mut policy = toml::Table::new();
            policy.insert("rules".to_string(), toml::Value::Array(rules));
            root.insert("policy".to_string(), toml::Value::Table(policy));
        }

        let serialized = toml::to_string(&root)?;
        // Static, non-user-controlled preamble (as in the original template).
        let toml_str =
            format!("# Concerto configuration (generated by setup wizard)\n{serialized}");
        std::fs::write(config_path, toml_str)?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// SetupWizard
// ---------------------------------------------------------------------------

/// Interactive first-run setup wizard.
pub struct SetupWizard<R: BufRead, W: Write> {
    reader: R,
    writer: W,
    /// Optional pre-fetched model list for live model picker.
    /// When set, the model prompt shows a numbered selection instead of
    /// a free-text input. Populated by [`Self::set_available_models`].
    available_models: Option<Vec<String>>,
}

/// Default wizard using stdin/stdout.
pub type DefaultWizard = SetupWizard<io::StdinLock<'static>, io::Stdout>;

impl<R: BufRead, W: Write> SetupWizard<R, W> {
    pub fn new(reader: R, writer: W) -> Self {
        Self { reader, writer, available_models: None }
    }

    /// Set available models for the live model picker.
    ///
    /// When set, [`Self::prompt_model`] displays a numbered list of models
    /// to choose from instead of a free-text input.
    pub fn set_available_models(&mut self, models: Vec<String>) {
        self.available_models = if models.is_empty() { None } else { Some(models) };
    }

    /// Run the wizard and return a `PendingConfig`.
    pub fn run(&mut self) -> Result<PendingConfig, SetupError> {
        self.writeln("=== Concerto Setup Wizard ===")?;
        self.writeln("")?;

        let provider = self.prompt_provider()?;
        let api_key = self.prompt_api_key()?;
        let model = self.prompt_model(&provider)?;
        let working_dir = self.prompt_working_dir()?;
        let policy_mode = self.prompt_policy()?;

        self.writeln("")?;
        self.writeln("Setup complete! Configuration will be saved.")?;

        Ok(PendingConfig {
            provider: match provider {
                ProviderKind::OpenAI => "openai".to_string(),
                ProviderKind::Anthropic => "anthropic".to_string(),
                ProviderKind::Ollama => "ollama".to_string(),
                ProviderKind::Nvidianim => "nim".to_string(),
                ProviderKind::OpenRouter => "openrouter".to_string(),
                ProviderKind::OpenCodeZen => "opencode".to_string(),
                ProviderKind::Other => "other".to_string(),
            },
            api_key,
            model,
            working_dir,
            policy_mode,
        })
    }

    /// Check if setup is needed (config file missing).
    pub fn needs_setup(config_path: &Path) -> bool {
        !config_path.exists()
    }

    // ---- helpers ----

    fn writeln(&mut self, msg: &str) -> Result<(), SetupError> {
        writeln!(self.writer, "{msg}").map_err(SetupError::Io)?;
        self.writer.flush().map_err(SetupError::Io)
    }

    fn write(&mut self, msg: &str) -> Result<(), SetupError> {
        write!(self.writer, "{msg}").map_err(SetupError::Io)?;
        self.writer.flush().map_err(SetupError::Io)
    }

    fn read_line(&mut self) -> Result<String, SetupError> {
        let mut buf = String::new();
        self.reader.read_line(&mut buf)?;
        Ok(buf.trim().to_string())
    }

    pub fn prompt_provider(&mut self) -> Result<ProviderKind, SetupError> {
        self.writeln("Select LLM provider:")?;
        self.writeln("  1) OpenAI")?;
        self.writeln("  2) Anthropic")?;
        self.writeln("  3) Ollama (local)")?;
        self.writeln("  4) NVIDIA NIM")?;
        self.writeln("  5) OpenRouter")?;
        self.writeln("  6) OpenCode Zen")?;
        self.writeln("  7) Other")?;
        loop {
            self.write("Choice [1-7]: ")?;
            let input = self.read_line()?;
            match input.as_str() {
                "1" => return Ok(ProviderKind::OpenAI),
                "2" => return Ok(ProviderKind::Anthropic),
                "3" => return Ok(ProviderKind::Ollama),
                "4" => return Ok(ProviderKind::Nvidianim),
                "5" => return Ok(ProviderKind::OpenRouter),
                "6" => return Ok(ProviderKind::OpenCodeZen),
                "7" => return Ok(ProviderKind::Other),
                _ => self.writeln("Invalid choice, please enter 1-7.")?,
            }
        }
    }

    pub fn prompt_api_key(&mut self) -> Result<String, SetupError> {
        self.write("API key (leave blank for local models): ")?;
        self.read_line()
    }

    pub fn prompt_model(&mut self, provider: &ProviderKind) -> Result<String, SetupError> {
        // Clone to avoid borrow conflict with self.write()/self.writeln()
        let models = self.available_models.clone();
        if let Some(ref model_list) = models {
            let count = model_list.len();
            // Live model picker — show numbered list
            self.writeln("Available models:")?;
            for (i, model) in model_list.iter().enumerate() {
                self.writeln(&format!("  {}) {}", i + 1, model))?;
            }
            self.write(&format!(
                "Select model [1-{}] or press Enter for default ({}): ",
                count,
                provider.default_model()
            ))?;
            let input = self.read_line()?;
            if input.is_empty() {
                return Ok(provider.default_model().to_string());
            }
            // Try numeric selection first
            if let Ok(n) = input.parse::<usize>() {
                if n >= 1 && n <= count {
                    return Ok(model_list[n - 1].clone());
                }
            }
            // Fall back to free-text
            Ok(input)
        } else {
            // Free-text prompt (original behavior)
            let default = provider.default_model();
            self.write(&format!("Model [{default}]: "))?;
            let input = self.read_line()?;
            if input.is_empty() {
                Ok(default.to_string())
            } else {
                Ok(input)
            }
        }
    }

    pub fn prompt_working_dir(&mut self) -> Result<PathBuf, SetupError> {
        let default = std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .to_string_lossy()
            .to_string();
        self.write(&format!("Working directory [{default}]: "))?;
        let input = self.read_line()?;
        if input.is_empty() {
            Ok(PathBuf::from(&default))
        } else {
            Ok(PathBuf::from(input))
        }
    }

    pub fn prompt_policy(&mut self) -> Result<String, SetupError> {
        self.writeln("Select policy mode:")?;
        self.writeln("  1) Safe (recommended) — allow reads; ask before changes and commands")?;
        self.writeln("  2) Strict — ask before every tool call")?;
        self.writeln("  3) Autonomous — auto-approve tool calls (hard safety blocks remain)")?;
        loop {
            self.write("Choice [1-3]: ")?;
            let input = self.read_line()?;
            match input.as_str() {
                "1" => return Ok("safe".into()),
                "2" => return Ok("strict".into()),
                "3" => return Ok("autonomous".into()),
                _ => self.writeln("Invalid choice, please enter 1-3.")?,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn wizard_with_input(input: &str) -> SetupWizard<io::BufReader<Cursor<Vec<u8>>>, Vec<u8>> {
        let reader = io::BufReader::new(Cursor::new(input.as_bytes().to_vec()));
        let writer = Vec::new();
        SetupWizard::new(reader, writer)
    }

    #[test]
    fn needs_setup_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        assert!(SetupWizard::<io::StdinLock, io::Stdout>::needs_setup(&path));
    }

    #[test]
    fn needs_setup_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "").unwrap();
        assert!(!SetupWizard::<io::StdinLock, io::Stdout>::needs_setup(&path));
    }

    #[test]
    fn wizard_selects_openai() {
        let input = "1\nsk-test-key\n\ngpt-4o\n.\n1\n";
        let mut wiz = wizard_with_input(input);
        let config = wiz.run().expect("wizard failed");
        assert_eq!(config.provider, "openai");
        assert_eq!(config.api_key, "sk-test-key");
        assert_eq!(config.model, "gpt-4o");
    }

    #[test]
    fn wizard_selects_ollama_without_key() {
        let input = "3\n\nllama3\n.\n2\n";
        let mut wiz = wizard_with_input(input);
        let config = wiz.run().expect("wizard failed");
        assert_eq!(config.provider, "ollama");
        assert_eq!(config.api_key, "");
        assert_eq!(config.model, "llama3");
        assert_eq!(config.policy_mode, "strict");
    }

    #[test]
    fn pending_config_save_and_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let config = PendingConfig {
            provider: "openai".into(),
            api_key: "sk-test".into(),
            model: "gpt-4o".into(),
            working_dir: PathBuf::from("/tmp"),
            policy_mode: "conservative".into(),
        };
        // In test mode, CredentialStore::set() returns an error, so we expect
        // save to fail on the credentials step rather than silently losing the key.
        // This confirms the key-persist path is actually wired.
        let creds = CredentialStore::from_env();
        let result = config.save(&path, &creds);
        assert!(result.is_err(), "save should fail in test mode because key write is blocked");
        assert!(matches!(result.unwrap_err(), SetupError::Validation(_)));
    }

    #[test]
    fn pending_config_save_without_key_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let config = PendingConfig {
            provider: "ollama".into(),
            api_key: String::new(), // no key — test that empty key skips credential write
            model: "llama3".into(),
            working_dir: PathBuf::from("/tmp"),
            policy_mode: "safe".into(),
        };
        let creds = CredentialStore::from_env();
        config.save(&path, &creds).expect("save with empty key should succeed");
        assert!(path.exists());
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("schema_version = 4"));
        assert!(!content.contains("primary_provider"));
        assert!(content.contains("[model_settings]"));
        assert!(content.contains(r#"global_default_model = "llama3""#));
        assert!(content.contains(r#"model = "llama3""#));
        // "safe" mode uses built-in defaults — no policy section.
        assert!(!content.contains("[policy]"));
        let parsed: crate::AppConfig = toml::from_str(&content).expect("generated TOML must parse");
        assert!(parsed.primary_provider.is_none());
        assert_eq!(
            parsed
                .model_settings
                .as_ref()
                .and_then(|settings| settings.global_default_model.as_deref()),
            Some("llama3"),
        );
    }

    #[test]
    fn pending_config_save_existing_fails() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "").unwrap();
        let config = PendingConfig {
            provider: "openai".into(),
            api_key: "sk-test".into(),
            model: "gpt-4o".into(),
            working_dir: PathBuf::from("/tmp"),
            policy_mode: "conservative".into(),
        };
        let creds = CredentialStore::from_env();
        match config.save(&path, &creds) {
            Err(SetupError::ConfigExists(_)) => {} // expected
            _ => panic!("expected ConfigExists error"),
        }
    }

    #[test]
    fn pending_config_save_overwrite_succeeds_on_existing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "old content").unwrap();
        let config = PendingConfig {
            provider: "ollama".into(),
            api_key: String::new(),
            model: "llama3".into(),
            working_dir: PathBuf::from("/tmp"),
            policy_mode: "expert".into(),
        };
        let creds = CredentialStore::from_env();
        config
            .save_overwrite(&path, &creds)
            .expect("save_overwrite should succeed on existing file");
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(!content.contains("old content"));
        // "autonomous" mode should generate auto_approve policy
        assert!(content.contains("auto_approve"));
        assert!(content.contains("always = true"));
    }

    /// Regression test for issue #104: a free-text model name containing `"`
    /// and newlines must not be able to inject extra TOML tables/keys (e.g. a
    /// `[policy]` block with `auto_approve`). The generated file must parse and
    /// contain only the intended structure, with the literal model text escaped
    /// into a string value.
    #[test]
    fn pending_config_save_rejects_model_toml_injection() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        // Attempts to close the model string and inject a `[policy]` table with
        // auto-approval rules.
        let malicious =
            "x\"\n[policy]\nrules = [{ action = \"auto_approve\", condition = { always = true } }]\n#";
        let config = PendingConfig {
            provider: "ollama".into(),
            api_key: String::new(),
            model: malicious.into(),
            working_dir: PathBuf::from("/tmp"),
            // "safe" is unrelated to "strict": the generated config must have no
            // `[policy]` table at all, so the injection cannot smuggle one in.
            policy_mode: "safe".into(),
        };
        let creds = CredentialStore::from_env();
        config.save(&path, &creds).expect("save with malicious model name should succeed");

        let content = std::fs::read_to_string(&path).unwrap();
        let doc: toml::Value = toml::from_str(&content).expect("generated TOML must parse");

        // Only the intended top-level keys; no injected `[policy]`.
        let mut keys: Vec<String> =
            doc.as_table().expect("root must be a table").keys().cloned().collect();
        keys.sort();
        assert_eq!(keys, vec!["model_settings".to_string(), "schema_version".to_string()]);
        assert!(doc.get("policy").is_none(), "injected [policy] table must not be present");

        // The literal (malicious) model text is preserved in both fields.
        let model_settings = doc.get("model_settings").expect("model_settings must exist");
        assert_eq!(
            model_settings.get("global_default_model").expect("global_default_model must exist"),
            &toml::Value::String(malicious.to_string()),
        );
        let providers = model_settings
            .get("providers")
            .and_then(|v| v.as_array())
            .expect("providers must be an array");
        let provider = providers.first().expect("one provider must exist");
        assert_eq!(
            provider.get("model").expect("model must exist"),
            &toml::Value::String(malicious.to_string())
        );

        // Typed parse confirms there is no policy and the exact model string.
        let parsed: crate::AppConfig = toml::from_str(&content).expect("generated TOML must parse");
        assert!(parsed.policy.is_none());
        assert_eq!(
            parsed.model_settings.as_ref().and_then(|s| s.global_default_model.as_deref()),
            Some(malicious),
        );
    }

    /// Regression test for issue #104: an ordinary model name must round-trip
    /// exactly through the generated TOML.
    #[test]
    fn pending_config_save_model_name_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let model = "gpt-4o".to_string();
        let config = PendingConfig {
            provider: "openai".into(),
            api_key: String::new(),
            model: model.clone(),
            working_dir: PathBuf::from("/tmp"),
            policy_mode: "safe".into(),
        };
        let creds = CredentialStore::from_env();
        config.save(&path, &creds).expect("save should succeed for a normal model name");

        let content = std::fs::read_to_string(&path).unwrap();
        let doc: toml::Value = toml::from_str(&content).expect("generated TOML must parse");
        let model_settings = doc.get("model_settings").expect("model_settings must exist");
        assert_eq!(
            model_settings.get("global_default_model").expect("global_default_model must exist"),
            &toml::Value::String(model.clone()),
        );
        let providers = model_settings
            .get("providers")
            .and_then(|v| v.as_array())
            .expect("providers must be an array");
        let provider = providers.first().expect("one provider must exist");
        assert_eq!(provider.get("model").expect("model must exist"), &toml::Value::String(model));
    }

    /// Regression test for issue #104: every TOML metacharacter and control
    /// character in a model name must round-trip as a literal string value —
    /// never as structure. Runs over a matrix of hostile strings.
    #[test]
    fn pending_config_save_model_name_escapes_all_toml_metacharacters() {
        let hostile = [
            "quote\"inside",
            "back\\slash",
            "hash#comment",
            "equals=sign",
            "brackets[]{}",
            "comma,colon:",
            "newline\nline",
            "tab\tindent",
            "control\u{7}bell",
            "unicode—中🎯",
            "x\"\n[policy]\n",
        ];
        for (i, model) in hostile.iter().enumerate() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("config.toml");
            let config = PendingConfig {
                provider: "ollama".into(),
                api_key: String::new(),
                model: (*model).to_string(),
                working_dir: PathBuf::from("/tmp"),
                policy_mode: "safe".into(),
            };
            let creds = CredentialStore::from_env();
            config.save(&path, &creds).expect("save must succeed for hostile model name");

            let content = std::fs::read_to_string(&path).unwrap();
            let doc: toml::Value = toml::from_str(&content).unwrap_or_else(|e| {
                panic!("generated TOML must parse for model #{i} {model:?}: {e}")
            });
            // No injected structure: only the intended top-level keys.
            let mut keys: Vec<&str> = doc.as_table().unwrap().keys().map(String::as_str).collect();
            keys.sort_unstable();
            assert_eq!(
                keys,
                ["model_settings", "schema_version"],
                "unexpected keys for model #{i}"
            );
            // Literal round-trip of the hostile string.
            let ms = doc.get("model_settings").unwrap().as_table().unwrap();
            assert_eq!(
                ms.get("global_default_model"),
                Some(&toml::Value::String((*model).to_string())),
                "model #{i} did not round-trip literally"
            );
        }
    }
}
