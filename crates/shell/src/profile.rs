use std::collections::BTreeMap;

use concerto_config::{ProfileAvailability, ShellBackendType, ShellProfileConfig, ShellSettings};
use thiserror::Error;

/// Validated, deterministic runtime view of Concerto's configured shell profiles.
///
/// Persistence and OS discovery belong to `concerto-config`. The AI-native shell
/// consumes that canonical configuration so its selected interpreter cannot
/// drift from Settings, the integrated terminal, or the agent shell binding.
#[derive(Clone, Debug)]
pub struct ShellProfileCatalog {
    profiles: BTreeMap<String, ShellProfileConfig>,
    selected_profile: String,
}

impl ShellProfileCatalog {
    /// Build a catalog from the canonical application settings and select the
    /// profile configured for agent execution.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid/duplicate profiles or a missing binding.
    pub fn from_settings(settings: &ShellSettings) -> Result<Self, ShellProfileError> {
        Self::from_profiles(
            settings.profiles.clone(),
            Some(settings.selected_profile_id().to_owned()),
        )
    }

    /// Build a catalog from explicitly supplied canonical profiles.
    ///
    /// This constructor is useful for tests and embedded frontends that have
    /// already resolved their configuration layer.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid or duplicate profiles, an empty catalog,
    /// or a selected id that is not present.
    pub fn from_profiles(
        profiles: impl IntoIterator<Item = ShellProfileConfig>,
        selected_profile: Option<String>,
    ) -> Result<Self, ShellProfileError> {
        let mut by_id = BTreeMap::new();
        for profile in profiles {
            validate_profile(&profile)?;
            let id = profile.id.clone();
            if by_id.insert(id.clone(), profile).is_some() {
                return Err(ShellProfileError::DuplicateId(id));
            }
        }
        if by_id.is_empty() {
            return Err(ShellProfileError::NoProfiles);
        }

        let selected_profile = selected_profile
            .or_else(|| by_id.contains_key("system-default").then(|| "system-default".to_owned()))
            .or_else(|| by_id.keys().next().cloned())
            .ok_or(ShellProfileError::NoProfiles)?;
        if !by_id.contains_key(&selected_profile) {
            return Err(ShellProfileError::UnknownSelected(selected_profile));
        }

        Ok(Self { profiles: by_id, selected_profile })
    }

    #[must_use]
    pub fn profiles(&self) -> Vec<&ShellProfileConfig> {
        self.profiles.values().collect()
    }

    #[must_use]
    pub fn selected(&self) -> Option<&ShellProfileConfig> {
        self.profiles.get(&self.selected_profile)
    }

    #[must_use]
    pub fn get(&self, id: &str) -> Option<&ShellProfileConfig> {
        self.profiles.get(id)
    }
}

/// Shell profile configuration error.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum ShellProfileError {
    #[error("invalid shell profile id `{0}`; use lowercase letters, digits, and hyphens")]
    InvalidId(String),
    #[error("shell profile `{0}` has an empty display name")]
    EmptyName(String),
    #[error("shell profile `{0}` has no executable configured")]
    EmptyProgram(String),
    #[error("duplicate shell profile id `{0}`")]
    DuplicateId(String),
    #[error("no shell profiles are configured")]
    NoProfiles,
    #[error("selected shell profile `{0}` does not exist")]
    UnknownSelected(String),
}

fn validate_profile(profile: &ShellProfileConfig) -> Result<(), ShellProfileError> {
    if !valid_profile_id(&profile.id) {
        return Err(ShellProfileError::InvalidId(profile.id.clone()));
    }
    if profile.name.trim().is_empty() {
        return Err(ShellProfileError::EmptyName(profile.id.clone()));
    }
    if profile.executable.trim().is_empty() && profile.backend != ShellBackendType::Managed {
        return Err(ShellProfileError::EmptyProgram(profile.id.clone()));
    }
    Ok(())
}

fn valid_profile_id(id: &str) -> bool {
    !id.is_empty()
        && !id.starts_with('-')
        && !id.ends_with('-')
        && id.bytes().all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

#[must_use]
pub fn profile_is_available(profile: &ShellProfileConfig) -> bool {
    matches!(profile.availability(), ProfileAvailability::Available)
}

#[cfg(test)]
mod tests {
    use concerto_config::WorkingDirBehavior;

    use super::*;

    fn profile(id: &str) -> ShellProfileConfig {
        ShellProfileConfig {
            id: id.to_owned(),
            name: format!("{id} shell"),
            backend: ShellBackendType::System,
            executable: "sh".to_owned(),
            default_working_dir: WorkingDirBehavior::ProjectRoot,
            ..Default::default()
        }
    }

    #[test]
    fn catalog_uses_canonical_selected_profile() {
        let settings = ShellSettings::new(
            vec![profile("interactive"), profile("agent")],
            "agent".to_owned(),
            None,
        );

        let catalog =
            ShellProfileCatalog::from_settings(&settings).expect("valid canonical settings");

        assert_eq!(catalog.selected().map(|item| item.id.as_str()), Some("agent"));
    }

    #[test]
    fn duplicate_ids_are_rejected() {
        let error = ShellProfileCatalog::from_profiles(
            [profile("duplicate"), profile("duplicate")],
            Some("duplicate".to_owned()),
        )
        .expect_err("duplicate ids must fail");

        assert_eq!(error, ShellProfileError::DuplicateId("duplicate".to_owned()));
    }

    #[test]
    fn managed_profile_may_resolve_executable_at_runtime() {
        let managed = ShellProfileConfig {
            id: "managed-bash".to_owned(),
            name: "Managed Bash".to_owned(),
            backend: ShellBackendType::Managed,
            executable: String::new(),
            ..Default::default()
        };

        assert!(
            ShellProfileCatalog::from_profiles([managed], Some("managed-bash".to_owned())).is_ok()
        );
    }

    #[test]
    fn rejects_empty_catalog() {
        let error = ShellProfileCatalog::from_profiles([], None::<String>)
            .expect_err("empty catalog must fail");
        assert_eq!(error, ShellProfileError::NoProfiles);
    }

    #[test]
    fn rejects_unknown_selected_profile() {
        let error = ShellProfileCatalog::from_profiles(
            [profile("bash"), profile("zsh")],
            Some("missing".to_owned()),
        )
        .expect_err("unknown selected must fail");
        assert_eq!(error, ShellProfileError::UnknownSelected("missing".to_owned()));
    }

    #[test]
    fn rejects_invalid_id_with_hyphen_prefix() {
        let invalid = ShellProfileConfig {
            id: "-invalid".to_owned(),
            name: "Invalid".to_owned(),
            backend: ShellBackendType::System,
            executable: "sh".to_owned(),
            ..Default::default()
        };
        let error = ShellProfileCatalog::from_profiles([invalid], Some("-invalid".to_owned()))
            .expect_err("hyphen-prefixed id must fail");
        assert_eq!(error, ShellProfileError::InvalidId("-invalid".to_owned()));
    }

    #[test]
    fn rejects_empty_name() {
        let invalid = ShellProfileConfig {
            id: "empty-name".to_owned(),
            name: "   ".to_owned(),
            backend: ShellBackendType::System,
            executable: "sh".to_owned(),
            ..Default::default()
        };
        let error = ShellProfileCatalog::from_profiles([invalid], Some("empty-name".to_owned()))
            .expect_err("empty name must fail");
        assert_eq!(error, ShellProfileError::EmptyName("empty-name".to_owned()));
    }

    #[test]
    fn rejects_empty_executable_for_system_backend() {
        let invalid = ShellProfileConfig {
            id: "no-exe".to_owned(),
            name: "No executable".to_owned(),
            backend: ShellBackendType::System,
            executable: String::new(),
            ..Default::default()
        };
        let error = ShellProfileCatalog::from_profiles([invalid], Some("no-exe".to_owned()))
            .expect_err("empty executable must fail for System backend");
        assert_eq!(error, ShellProfileError::EmptyProgram("no-exe".to_owned()));
    }

    #[test]
    fn falls_back_to_first_profile_when_none_selected() {
        let catalog = ShellProfileCatalog::from_profiles(
            [profile("first"), profile("second")],
            None::<String>,
        )
        .expect("no selected should fall back to first profile");
        assert_eq!(catalog.selected().map(|p| p.id.as_str()), Some("first"));
    }

    #[test]
    fn falls_back_to_system_default_when_available() {
        let system = ShellProfileConfig {
            id: "system-default".to_owned(),
            name: "System Default".to_owned(),
            backend: ShellBackendType::System,
            executable: "sh".to_owned(),
            ..Default::default()
        };
        let catalog =
            ShellProfileCatalog::from_profiles([system, profile("other")], None::<String>)
                .expect("system-default should be preferred");
        assert_eq!(catalog.selected().map(|p| p.id.as_str()), Some("system-default"));
    }

    #[test]
    fn get_returns_specific_profile_by_id() {
        let catalog = ShellProfileCatalog::from_profiles(
            [profile("bash"), profile("zsh")],
            Some("bash".to_owned()),
        )
        .expect("valid catalog");
        assert!(catalog.get("bash").is_some());
        assert!(catalog.get("zsh").is_some());
        assert!(catalog.get("missing").is_none());
    }
}
