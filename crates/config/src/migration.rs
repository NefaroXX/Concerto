//! Schema migration for `AppConfig`.
//!
//! Migrations convert a loaded config with an older `schema_version` to the
//! current version by inserting defaults for newly added sections. This
//! enables backward-compatible config loading without manual edits.
//!
//! # Current migrations
//!
//! | From | To | Changes |
//! |------|----|---------|
//! | 1    | 2  | Add `observability: None`, add `multi_agent` fields with defaults |
//! | 2    | 3  | Add `model_settings: None`, add `id`/`name` fields to `ProviderConfig` |
//! | 3    | 4  | Add `shell_settings: None` (ADR-28 shell profiles) |
//! | 4    | 5  | Add `[skills]`/`[mcp]` sections, serde-defaulted to `None` (ADR-43) |
//! | 5    | 6  | Drop `mode`/`[intent]` — intent gate is the only routing path (ADR-55 Phase 1e) |
//! | 6    | 7  | Add `[intent]` classifier keys, defaulted on (ADR-55 Phase 2c; ADR-56) |
//!
//! # Policy
//!
//! - Migrations never modify or delete existing user fields.
//! - Migrations only insert `Option` fields with `None` or sensible defaults.
//! - If a migration cannot be applied, it returns `ConfigError::Load`.
//! - Loading a config at the current schema version is a no-op.

use crate::schema::AppConfig;
use concerto_core::error::ConfigError;

/// Migrate an `AppConfig` from any older schema version to the current
/// version (`SCHEMA_VERSION`). Returns the migrated config (possibly
/// unchanged if already current).
pub fn migrate_config(config: AppConfig) -> Result<AppConfig, ConfigError> {
    if config.schema_version > crate::schema::SCHEMA_VERSION {
        return Err(ConfigError::SchemaMismatch {
            found: config.schema_version,
            expected: crate::schema::SCHEMA_VERSION,
        });
    }

    let mut current = config;

    // Chain migrations sequentially so each step can assume it's only
    // advancing one version.
    while current.schema_version < crate::schema::SCHEMA_VERSION {
        match current.schema_version {
            1 => current = migrate_v1_to_v2(current)?,
            2 => current = migrate_v2_to_v3(current)?,
            3 => current = migrate_v3_to_v4(current)?,
            4 => current = migrate_v4_to_v5(current)?,
            5 => current = migrate_v5_to_v6(current)?,
            6 => current = migrate_v6_to_v7(current)?,
            v => {
                return Err(ConfigError::SchemaMismatch {
                    found: v,
                    expected: crate::schema::SCHEMA_VERSION,
                });
            }
        }
    }

    Ok(current)
}

/// v1 → v2: Add observability and multi-agent default fields.
fn migrate_v1_to_v2(config: AppConfig) -> Result<AppConfig, ConfigError> {
    Ok(AppConfig {
        // Preserve all existing v1 fields as-is.
        schema_version: 2,
        primary_provider: config.primary_provider,
        primary_provider_config: config.primary_provider_config,
        fallback_provider: config.fallback_provider,
        fallback_provider_config: config.fallback_provider_config,
        ollama_base_url: config.ollama_base_url,
        session_spend_cap_usd: config.session_spend_cap_usd,
        policy: config.policy,
        // v1 had no multi_agent field; default to None.
        multi_agent: config.multi_agent,
        // Add v2 field with disabled default.
        observability: None,
        // v2 didn't have model_settings; default to None (migrate to v3 later).
        model_settings: None,
        // v2 didn't have plugins; default to None (migrate to v3 later).
        plugins: None,
        // v5 adds skills/mcp (default None via serde; filled later).
        skills: None,
        mcp: None,
        // v2 didn't have updates; default to None.
        updates: None,
        // Retry config: preserve loaded value (older configs default via serde).
        retry: config.retry,
        // Memory config is additive and defaults during deserialization.
        memory: config.memory,
        // v4 adds shell_settings (default filled by migrate_v3_to_v4).
        shell_settings: None,
        // v4 adds project_roots (ADR-44); preserve any deserialized value.
        project_roots: config.project_roots,
        // v5 adds [context] (ADR-48); additive Option defaults to None.
        context: None,
        // v7 adds [intent] (ADR-55 Phase 2c); filled by migrate_v6_to_v7.
        intent: None,
        // [tools] is additive and default-on; old configs keep it None.
        tool_settings: None,
        orchestration: None,
        // ADR-58 P2+P3: derived state, filled by the load seam; never in files.
        resolved_blueprint: None,
    })
}

/// v2 → v3: Add `model_settings: None`, enable `id`/`name` on ProviderConfig.
fn migrate_v2_to_v3(config: AppConfig) -> Result<AppConfig, ConfigError> {
    Ok(AppConfig {
        schema_version: 3,
        primary_provider: config.primary_provider,
        primary_provider_config: config.primary_provider_config,
        fallback_provider: config.fallback_provider,
        fallback_provider_config: config.fallback_provider_config,
        ollama_base_url: config.ollama_base_url,
        session_spend_cap_usd: config.session_spend_cap_usd,
        policy: config.policy,
        multi_agent: config.multi_agent,
        observability: config.observability,
        // v3 adds model_settings (default None for backward compat).
        model_settings: None,
        // v3 adds plugins (default None for backward compat).
        plugins: None,
        // v5 adds skills/mcp (default None via serde; filled later).
        skills: None,
        mcp: None,
        // v3 adds updates (default None for backward compat).
        updates: None,
        // Retry config: preserve loaded value (older configs default via serde).
        retry: config.retry,
        // Memory config is additive and defaults during deserialization.
        memory: config.memory,
        // v4 adds shell_settings (default filled below).
        shell_settings: None,
        // v4 adds project_roots (ADR-44); preserve any deserialized value.
        project_roots: config.project_roots,
        // v5 adds [context] (ADR-48); preserve any deserialized value.
        context: config.context,
        // v7 adds [intent] (ADR-55 Phase 2c); filled by migrate_v6_to_v7.
        intent: None,
        // [tools] is additive and default-on; old configs keep it None.
        tool_settings: None,
        orchestration: None,
        // ADR-58 P2+P3: derived state, filled by the load seam; never in files.
        resolved_blueprint: None,
    })
}

/// v3 → v4: Add `shell_settings`, defaulting to the preset list (ADR-28).
fn migrate_v3_to_v4(mut config: AppConfig) -> Result<AppConfig, ConfigError> {
    if config.shell_settings.is_none() {
        config.shell_settings = Some(crate::shell::default_shell_settings());
    }
    config.schema_version = 4;
    Ok(config)
}

/// v4 → v5: Add `[skills]` and `[mcp]` sections (ADR-43).
///
/// Both new fields are `Option` and serde-default to `None` during
/// deserialization, so this step is a version bump only — no user fields are
/// modified and nothing is deleted (insert-only policy).
fn migrate_v4_to_v5(mut config: AppConfig) -> Result<AppConfig, ConfigError> {
    config.schema_version = 5;
    Ok(config)
}

/// v5 → v6: Drop `mode` and `[intent]` (ADR-55 Phase 1e).
///
/// The intent gate is now the ONLY routing path and the Build/Chat/Plan mode
/// picker is removed, so `AppConfig` no longer carries either key. The fields
/// cease to exist in the struct; stale TOML keys are ignored at load because
/// `AppConfig` has no `deny_unknown_fields`. Version bump only — insert-only
/// policy, no user fields are modified or deleted.
fn migrate_v5_to_v6(mut config: AppConfig) -> Result<AppConfig, ConfigError> {
    config.schema_version = 6;
    Ok(config)
}

/// v6 → v7: Add `[intent]` classifier keys (ADR-55 Phase 2c; ADR-56).
///
/// Insert-only, mirroring `migrate_v3_to_v4`'s fill style: when the section is
/// absent it is inserted with defaults (classifier enabled, no model, 0.7
/// threshold — ADR-56 §2 flipped the Phase 2c default pin to on). Re-adding
/// `[intent]` does NOT resurrect the `mode`/`enabled` keys dropped at v5→v6 —
/// v7 adds only the three classifier keys and the gate stays always-on. The
/// version bump mirrors `migrate_v4_to_v5`/`migrate_v5_to_v6`.
fn migrate_v6_to_v7(mut config: AppConfig) -> Result<AppConfig, ConfigError> {
    if config.intent.is_none() {
        config.intent = Some(crate::schema::IntentConfig::default());
    }
    config.schema_version = 7;
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{MemoryConfig, RetryConfig};

    /// A minimal v1 config fixture — has schema_version: 1 and no
    /// observability field.
    fn v1_fixture() -> AppConfig {
        AppConfig {
            schema_version: 1,
            primary_provider: Some("anthropic".into()),
            primary_provider_config: None,
            fallback_provider: None,
            fallback_provider_config: None,
            ollama_base_url: Some("http://localhost:11434".into()),
            session_spend_cap_usd: Some(5.0),
            policy: None,
            multi_agent: None,
            observability: None,
            model_settings: None,
            plugins: None,
            skills: None,
            mcp: None,
            updates: None,
            retry: RetryConfig::default(),
            memory: MemoryConfig::default(),
            shell_settings: None,
            project_roots: Vec::new(),
            context: None,
            intent: None,
            tool_settings: None,
            orchestration: None,
            resolved_blueprint: None,
        }
    }

    fn v2_fixture() -> AppConfig {
        AppConfig {
            schema_version: 2,
            primary_provider: Some("openai".into()),
            primary_provider_config: None,
            fallback_provider: None,
            fallback_provider_config: None,
            ollama_base_url: None,
            session_spend_cap_usd: None,
            policy: None,
            multi_agent: None,
            observability: None,
            model_settings: None,
            plugins: None,
            skills: None,
            mcp: None,
            updates: None,
            retry: RetryConfig::default(),
            memory: MemoryConfig::default(),
            shell_settings: None,
            project_roots: Vec::new(),
            context: None,
            intent: None,
            tool_settings: None,
            orchestration: None,
            resolved_blueprint: None,
        }
    }

    #[test]
    fn v1_to_v2_adds_observability_default() {
        let v1 = v1_fixture();
        let v2 = migrate_config(v1).expect("v1→v2 migration should succeed");
        assert_eq!(v2.schema_version, 7);
        assert_eq!(v2.observability, None);
        assert_eq!(v2.primary_provider.as_deref(), Some("anthropic"));
        assert_eq!(v2.session_spend_cap_usd, Some(5.0));
    }

    #[test]
    fn v2_to_v3_adds_model_settings_default() {
        let v2 = v2_fixture();
        let v3 = migrate_config(v2).expect("v2→v3 migration should succeed");
        assert_eq!(v3.schema_version, 7);
        assert_eq!(v3.model_settings, None);
        assert_eq!(v3.primary_provider.as_deref(), Some("openai"));
    }

    #[test]
    fn v3_config_passes_through_unchanged() {
        let v3 = AppConfig {
            schema_version: 3,
            primary_provider: Some("anthropic".into()),
            primary_provider_config: None,
            fallback_provider: None,
            fallback_provider_config: None,
            ollama_base_url: None,
            session_spend_cap_usd: None,
            policy: None,
            multi_agent: None,
            observability: None,
            model_settings: None,
            plugins: None,
            skills: None,
            mcp: None,
            updates: None,
            retry: RetryConfig::default(),
            memory: MemoryConfig::default(),
            shell_settings: None,
            project_roots: Vec::new(),
            context: None,
            intent: None,
            tool_settings: None,
            orchestration: None,
            resolved_blueprint: None,
        };
        let result = migrate_config(v3.clone()).expect("v3 should pass through");
        assert_eq!(result.schema_version, 7);
        assert_eq!(result.primary_provider.as_deref(), Some("anthropic"));
        assert_eq!(result.model_settings, None);
    }

    #[test]
    fn unknown_schema_version_errors() {
        let bad = AppConfig { schema_version: 99, ..v1_fixture() };
        let err = migrate_config(bad).unwrap_err();
        assert!(
            matches!(err, ConfigError::SchemaMismatch { found: 99, .. }),
            "expected SchemaMismatch error, got {err:?}"
        );
    }

    #[test]
    fn v3_to_v4_adds_shell_settings_with_default_profiles() {
        let v3 = AppConfig { schema_version: 3, shell_settings: None, ..v1_fixture() };
        let v4 = migrate_config(v3).expect("v3→v4 migration should succeed");
        assert_eq!(v4.schema_version, 7);
        assert!(v4.shell_settings.is_some(), "v4 must have shell_settings populated");

        let ss = v4.shell_settings.unwrap();
        assert!(!ss.profiles.is_empty(), "default shell settings must have at least one profile");
        assert!(!ss.selected_profile.is_empty(), "a profile must be selected");
    }

    #[test]
    fn v4_to_v5_adds_skills_and_mcp_defaults() {
        let v4 = AppConfig { schema_version: 4, ..v1_fixture() };
        let v5 = migrate_config(v4).expect("v4→v5 migration should succeed");
        assert_eq!(v5.schema_version, 7);
        // Insert-only: the new sections default to None, user fields untouched.
        assert_eq!(v5.skills, None);
        assert_eq!(v5.mcp, None);
        assert_eq!(v5.primary_provider.as_deref(), Some("anthropic"));
        assert_eq!(v5.session_spend_cap_usd, Some(5.0));
    }

    #[test]
    fn v5_to_v6_drops_mode_and_intent_keys() {
        let v5 = AppConfig { schema_version: 5, ..v1_fixture() };
        let v6 = migrate_config(v5).expect("v5→v6 migration should succeed");
        assert_eq!(v6.schema_version, 7);
        // The struct no longer carries `mode`/`[intent]`; stale TOML keys are
        // ignored at load because AppConfig has no deny_unknown_fields.
        assert_eq!(v6.primary_provider.as_deref(), Some("anthropic"));
        assert_eq!(v6.session_spend_cap_usd, Some(5.0));
    }

    /// ADR-56 §2: v6 configs migrate with `[intent]` inserted and defaulted
    /// (classifier ON — the superseding default-pin flip). Re-adding the
    /// section does NOT resurrect the v6-dropped `mode`/`enabled` keys — only
    /// the three classifier keys.
    #[test]
    fn v6_to_v7_inserts_intent_section_with_defaults() {
        let v6 = AppConfig { schema_version: 6, ..v1_fixture() };
        let v7 = migrate_config(v6).expect("v6→v7 migration should succeed");
        assert_eq!(v7.schema_version, 7);
        let intent = v7.intent.expect("v7 must carry the [intent] section");
        assert!(intent.classifier_enabled, "classifier defaults to on (ADR-56)");
        assert_eq!(intent.classifier_model, None, "no classifier model by default");
        assert_eq!(
            intent.classifier_confidence_threshold,
            concerto_core::LOW_CONFIDENCE_THRESHOLD,
            "threshold defaults to LOW_CONFIDENCE_THRESHOLD (0.7)"
        );
        assert_eq!(v7.primary_provider.as_deref(), Some("anthropic"));
        assert_eq!(v7.session_spend_cap_usd, Some(5.0));
    }

    /// ADR-55 Phase 2c §2: an existing `[intent]` section survives the
    /// migration untouched (insert-only), including the classifier keys.
    #[test]
    fn v6_to_v7_preserves_existing_intent_section() {
        let mut v6 = AppConfig { schema_version: 6, ..v1_fixture() };
        v6.intent = Some(crate::schema::IntentConfig {
            classifier_enabled: true,
            classifier_model: Some("claude-sonnet".into()),
            classifier_confidence_threshold: 0.85,
        });
        let v7 = migrate_config(v6).expect("v6→v7 migration should succeed");
        let intent = v7.intent.expect("existing [intent] section must be preserved");
        assert!(intent.classifier_enabled);
        assert_eq!(intent.classifier_model.as_deref(), Some("claude-sonnet"));
        assert_eq!(intent.classifier_confidence_threshold, 0.85);
    }
}
