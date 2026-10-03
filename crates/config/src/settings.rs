//! Validated, merge-aware settings edits shared by command frontends.

use std::path::{Path, PathBuf};

use figment::{
    providers::{Format, Serialized, Toml},
    Figment,
};
use serde_json::Value;
use toml_edit::{DocumentMut, Item};

use crate::{AppConfig, ConfigError, CustomAgentConfig};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsScope {
    Global,
    Project,
}

pub struct SettingsEditor {
    pub global_path: PathBuf,
    pub project_root: PathBuf,
    pub scope: SettingsScope,
}

impl SettingsEditor {
    pub fn path(&self) -> PathBuf {
        match self.scope {
            SettingsScope::Global => self.global_path.clone(),
            SettingsScope::Project => {
                self.project_root.join(crate::legacy::NEW_PROJECT_CONFIG_FILE)
            }
        }
    }

    /// `None` removes a saved override; `Some` is a TOML value. The returned
    /// document is also the dry-run preview. Environment overrides never mask
    /// validation errors or become persisted settings.
    pub fn edit(
        &self,
        key: &str,
        value: Option<&str>,
        dry_run: bool,
    ) -> Result<String, ConfigError> {
        self.edit_batch(&[(key, value)], dry_run)
    }

    /// Apply related settings changes as one validated document replacement.
    pub fn edit_batch(
        &self,
        edits: &[(&str, Option<&str>)],
        dry_run: bool,
    ) -> Result<String, ConfigError> {
        for &(key, _) in edits {
            if key == "schema_version" || key.starts_with("schema_version.") {
                return Err(invalid("schema_version is managed by configuration migrations"));
            }
            if self.scope == SettingsScope::Project {
                for global_key in crate::saving::GLOBAL_ONLY_ORCHESTRATION_KEYS {
                    if overlaps(key, global_key) {
                        return Err(invalid(format!("{key} is global only; use --global")));
                    }
                }
            }
            if overlaps(key, "multi_agent.custom_agents") || key == "multi_agent" {
                return Err(invalid("edit the canonical roster with `agents` commands"));
            }
        }
        let path = self.path();
        let raw = match std::fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                format!("schema_version = {}\n", crate::SCHEMA_VERSION)
            }
            Err(error) => return Err(invalid(format!("cannot read {}: {error}", path.display()))),
        };
        let mut edited = raw;
        for &(key, value) in edits {
            edited = edit_document(&edited, key, value)?;
        }
        // Deserialize before the loader repairs ids or consults agent files:
        // the serializer gives the actual accepted schema, including optional
        // fields. Compare only the edited subtree, preserving legacy unknown
        // keys elsewhere rather than deleting or promoting them.
        let typed: AppConfig = Figment::new()
            .merge(Serialized::defaults(AppConfig::default()))
            .merge(Toml::string(&edited))
            .extract()
            .map_err(|error| invalid(error.to_string()))?;
        for &(key, value) in edits {
            if value.is_some() {
                let accepted =
                    serde_json::to_value(&typed).map_err(|error| invalid(error.to_string()))?;
                let supplied: toml::Value =
                    toml::from_str(&edited).map_err(|error| invalid(error.to_string()))?;
                let supplied =
                    serde_json::to_value(supplied).map_err(|error| invalid(error.to_string()))?;
                let accepted = value_at(&accepted, key)
                    .ok_or_else(|| invalid(format!("unknown setting: {key}")))?;
                let supplied = value_at(&supplied, key)
                    .ok_or_else(|| invalid(format!("missing setting: {key}")))?;
                reject_unknown_keys(supplied, accepted, key)?;
            }
        }
        match self.scope {
            SettingsScope::Global => {
                crate::load_config_documents(
                    Some(&self.global_path),
                    None,
                    false,
                    Some(&edited),
                    None,
                )?;
            }
            SettingsScope::Project => {
                crate::load_config_documents(
                    Some(&self.global_path),
                    Some(&self.project_root),
                    false,
                    None,
                    Some(&edited),
                )?;
            }
        }
        if !dry_run {
            crate::saving::atomic_write(&path, edited.as_bytes())?;
        }
        Ok(edited)
    }
}

fn overlaps(key: &str, other: &str) -> bool {
    key == other || key.starts_with(&format!("{other}.")) || other.starts_with(&format!("{key}."))
}

fn invalid(message: impl Into<String>) -> ConfigError {
    ConfigError::InvalidValue(message.into())
}

/// Dotted paths accept numeric array indexes; indexes never silently append.
pub fn value_at<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    key.split('.').try_fold(value, |node, segment| {
        if let Some(array) = node.as_array() {
            array.get(segment.parse::<usize>().ok()?)
        } else {
            node.get(segment)
        }
    })
}

pub fn reject_unknown_keys(
    supplied: &Value,
    accepted: &Value,
    path: &str,
) -> Result<(), ConfigError> {
    if let Some(map) = supplied.as_object() {
        for (key, value) in map {
            let child = accepted
                .get(key)
                .ok_or_else(|| invalid(format!("unknown setting: {path}.{key}")))?;
            reject_unknown_keys(value, child, &format!("{path}.{key}"))?;
        }
    } else if let (Some(supplied), Some(accepted)) = (supplied.as_array(), accepted.as_array()) {
        for (index, (value, child)) in supplied.iter().zip(accepted).enumerate() {
            reject_unknown_keys(value, child, &format!("{path}.{index}"))?;
        }
    }
    Ok(())
}

/// Edit one TOML path while preserving comments, order, and untouched keys.
pub fn edit_document(raw: &str, key: &str, value: Option<&str>) -> Result<String, ConfigError> {
    let segments: Vec<&str> = key.split('.').collect();
    if segments.iter().any(|part| part.is_empty()) {
        return Err(invalid("setting paths must contain non-empty dot-separated segments"));
    }
    let mut document: DocumentMut =
        raw.parse().map_err(|error| invalid(format!("invalid TOML: {error}")))?;
    let replacement = value
        .map(|raw| {
            let document: DocumentMut = format!("value = {raw}\n")
                .parse()
                .map_err(|error| invalid(format!("invalid TOML value: {error}")))?;
            if document.as_table().len() != 1 {
                return Err(invalid("expected one TOML value"));
            }
            Ok(document["value"].clone())
        })
        .transpose()?;
    edit_item(document.as_item_mut(), &segments, replacement)?;
    Ok(document.to_string())
}

fn edit_item(
    node: &mut Item,
    segments: &[&str],
    replacement: Option<Item>,
) -> Result<(), ConfigError> {
    let segment = segments[0];
    if node.is_none() {
        if replacement.is_none() {
            return Err(invalid("setting is not present in this scope"));
        }
        *node = Item::Table(toml_edit::Table::new());
    }
    // Inline tables and arrays of inline records use Value rather than Item.
    if let Some(value) = node.as_value_mut() {
        return edit_value(value, segments, replacement);
    }
    if let Some(array) = node.as_array_of_tables_mut() {
        let index = segment.parse::<usize>().map_err(|_| invalid("expected an array index"))?;
        let table = array.get_mut(index).ok_or_else(|| invalid("array index is out of range"))?;
        if segments.len() == 1 {
            return Err(invalid("replace or remove an entire record array instead"));
        }
        return edit_table(table, &segments[1..], replacement);
    }
    let table = node.as_table_mut().ok_or_else(|| invalid("setting path is not a table"))?;
    edit_table(table, segments, replacement)
}

fn edit_table(
    table: &mut toml_edit::Table,
    segments: &[&str],
    replacement: Option<Item>,
) -> Result<(), ConfigError> {
    let segment = segments[0];
    if segments.len() > 1 {
        if !table.contains_key(segment) && replacement.is_none() {
            return Err(invalid("setting is not present in this scope"));
        }
        return edit_item(table.entry(segment).or_insert(Item::None), &segments[1..], replacement);
    }
    if let Some(mut replacement) = replacement {
        if let (Some(old), Some(new)) =
            (table.get(segment).and_then(Item::as_value), replacement.as_value_mut())
        {
            *new.decor_mut() = old.decor().clone();
        }
        if let Some(key) = table.key(segment).cloned() {
            table.insert_formatted(&key, replacement);
        } else {
            table.insert(segment, replacement);
        }
    } else if table.remove(segment).is_none() {
        return Err(invalid("setting is not present in this scope"));
    }
    Ok(())
}

fn edit_value(
    node: &mut toml_edit::Value,
    segments: &[&str],
    replacement: Option<Item>,
) -> Result<(), ConfigError> {
    let segment = segments[0];
    if let Some(table) = node.as_inline_table_mut() {
        if segments.len() == 1 {
            if let Some(replacement) = replacement {
                let mut value =
                    replacement.into_value().map_err(|_| invalid("expected a TOML value"))?;
                if let Some(old) = table.get(segment) {
                    *value.decor_mut() = old.decor().clone();
                }
                table.insert(segment, value);
            } else if table.remove(segment).is_none() {
                return Err(invalid("setting is not present in this scope"));
            }
            return Ok(());
        }
        if !table.contains_key(segment) {
            if replacement.is_none() {
                return Err(invalid("setting is not present in this scope"));
            }
            table.insert(segment, toml_edit::Value::InlineTable(toml_edit::InlineTable::new()));
        }
        let child = table.get_mut(segment).ok_or_else(|| invalid("missing table"))?;
        return edit_value(child, &segments[1..], replacement);
    }
    if let Some(array) = node.as_array_mut() {
        let index = segment.parse::<usize>().map_err(|_| invalid("expected an array index"))?;
        if index >= array.len() {
            return Err(invalid("array index is out of range"));
        }
        if segments.len() == 1 {
            if let Some(replacement) = replacement {
                let value =
                    replacement.into_value().map_err(|_| invalid("expected a TOML value"))?;
                array.replace(index, value);
            } else {
                array.remove(index);
            }
            return Ok(());
        }
        let child = array.get_mut(index).ok_or_else(|| invalid("array index is out of range"))?;
        return edit_value(child, &segments[1..], replacement);
    }
    Err(invalid("setting path crosses a scalar value"))
}

/// Save an edited roster through the same authoritative file store as Studio.
/// Load-time stage validation happens before any file is changed.
pub fn save_settings_roster(
    global_path: &Path,
    roster: &[CustomAgentConfig],
) -> Result<(), ConfigError> {
    let path = global_path.to_path_buf();
    let config = crate::load_global_config(Some(&path))?;
    if let Some(resolved) = &config.resolved_blueprint {
        crate::validate_custom_agents(resolved, roster, config.orchestration.is_some())?;
    }
    if roster.iter().any(|agent| agent.id.eq_ignore_ascii_case("coordinator")) {
        return Err(invalid("the coordinator is constructed by the runtime; edit multi_agent.coordinator_prompt instead"));
    }
    let dir = crate::agents_dir_for_config(global_path)?;
    crate::save_agent_roster_files(&dir, roster)?;
    Ok(())
}

/// A populated schema example makes optional sections discoverable. It is
/// documentation, not the effective config or a change to runtime defaults.
pub fn settings_example() -> AppConfig {
    AppConfig {
        policy: Some(crate::PolicyConfig {
            rules: vec![crate::PolicyRuleDef {
                action: "require_approval".into(),
                condition: crate::ConditionDef::Always { always: true },
            }],
            time_window: Some(crate::schema::PolicyTimeWindowConfig {
                start_hour: 9,
                end_hour: 17,
                timezone: "UTC".into(),
                auto_approve_below_usd: 0.0,
            }),
            approval_timeout_secs: None,
        }),
        multi_agent: Some(crate::MultiAgentConfig::default()),
        model_settings: Some(crate::ModelSettings {
            providers: vec![crate::ProviderConfig::default()],
            ..Default::default()
        }),
        observability: Some(Default::default()),
        plugins: Some(Default::default()),
        skills: Some(Default::default()),
        mcp: Some(crate::McpConfig {
            enabled: false,
            servers: vec![crate::McpServerConfig {
                id: "example".into(),
                command: "example-server".into(),
                args: vec!["--argument".into()],
                env: Some(Default::default()),
                enabled: true,
                timeout_secs: Some(60),
            }],
        }),
        project_context: Some(Default::default()),
        updates: Some(Default::default()),
        context: Some(Default::default()),
        tool_settings: Some(Default::default()),
        audit: Some(Default::default()),
        shell_settings: Some(Default::default()),
        orchestration: Some(Default::default()),
        ..Default::default()
    }
}

/// Environment additions may contain credentials; never print their values
/// during normal settings inspection or dry-run previews.
pub fn redact_settings(value: &mut Value) {
    if let Some(map) = value.as_object_mut() {
        for (key, value) in map {
            if key == "env" {
                if let Some(env) = value.as_object_mut() {
                    for value in env.values_mut() {
                        *value = Value::String("[REDACTED]".into());
                    }
                }
            } else {
                redact_settings(value);
            }
        }
    } else if let Some(array) = value.as_array_mut() {
        for value in array {
            redact_settings(value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn editor(dir: &Path) -> SettingsEditor {
        SettingsEditor {
            global_path: dir.join("config.toml"),
            project_root: dir.join("project"),
            scope: SettingsScope::Global,
        }
    }

    #[test]
    fn invalid_batch_keeps_all_prior_settings_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let editor = editor(dir.path());
        editor.edit("memory.enabled", Some("true"), false).unwrap();
        let before = std::fs::read_to_string(editor.path()).unwrap();
        assert!(editor
            .edit_batch(
                &[("memory.enabled", Some("false")), ("retry.max_attempts", Some("0"))],
                false
            )
            .is_err());
        assert_eq!(std::fs::read_to_string(editor.path()).unwrap(), before);
    }

    #[test]
    fn edits_preserve_comments_and_reject_bad_or_unknown_values_before_writing() {
        let dir = tempfile::tempdir().unwrap();
        let editor = editor(dir.path());
        let raw = "schema_version = 8\n# retain\n[retry]\nenabled = true # trailing\n";
        std::fs::write(editor.path(), raw).unwrap();
        editor.edit("retry.enabled", Some("false"), false).unwrap();
        let written = std::fs::read_to_string(editor.path()).unwrap();
        assert!(written.contains("# retain") && written.contains("false # trailing"));
        for (key, value) in [
            ("retry.max_attempts", "0"),
            ("retry.enabld", "true"),
            ("retry.enabled", "\"yes\""),
            ("memory", "{ enabled = true, typo = 1 }"),
        ] {
            assert!(editor.edit(key, Some(value), false).is_err(), "{key}");
            assert_eq!(std::fs::read_to_string(editor.path()).unwrap(), written);
        }
    }

    #[test]
    fn project_edits_keep_global_values_separate_and_unset_restores_inheritance() {
        let dir = tempfile::tempdir().unwrap();
        let mut editor = editor(dir.path());
        editor.edit("memory.enabled", Some("true"), false).unwrap();
        let global = std::fs::read_to_string(&editor.global_path).unwrap();
        editor.scope = SettingsScope::Project;
        assert!(editor.edit("orchestration.blueprint.name", Some("\"tdd\""), false).is_err());
        editor.edit("memory.enabled", Some("false"), false).unwrap();
        assert_eq!(std::fs::read_to_string(&editor.global_path).unwrap(), global);
        let config =
            crate::load_config(Some(&editor.global_path), Some(&editor.project_root)).unwrap();
        assert!(!config.memory.enabled);
        editor.edit("memory.enabled", None, false).unwrap();
        assert!(
            crate::load_config(Some(&editor.global_path), Some(&editor.project_root))
                .unwrap()
                .memory
                .enabled
        );
    }

    #[test]
    fn dry_run_does_not_create_files_and_indexed_records_validate() {
        let dir = tempfile::tempdir().unwrap();
        let editor = editor(dir.path());
        editor.edit("mcp.enabled", Some("true"), true).unwrap();
        assert!(!editor.path().exists());
        editor
            .edit(
                "mcp.servers",
                Some("[{id = 'demo', command = 'demo', args = ['two words']}]"),
                false,
            )
            .unwrap();
        editor.edit("mcp.servers.0.enabled", Some("false"), false).unwrap();
        let config = crate::load_global_config(Some(&editor.global_path)).unwrap();
        let mcp = config.mcp.unwrap();
        assert!(!mcp.servers[0].enabled);
        assert_eq!(mcp.servers[0].args, ["two words"]);
        assert!(editor.edit("mcp.servers.0.typo", Some("1"), false).is_err());
        assert!(editor.edit("mcp.servers.1.enabled", Some("true"), false).is_err());
    }

    #[test]
    fn authoritative_roster_uses_files_and_does_not_accept_silent_inline_edits() {
        let dir = tempfile::tempdir().unwrap();
        let editor = editor(dir.path());
        let mut roster = crate::builtin_agent_seeds();
        roster[0].prompt_sections.system_instructions = "multiline\ninstructions".into();
        save_settings_roster(&editor.global_path, &roster).unwrap();
        let config = crate::load_global_config(Some(&editor.global_path)).unwrap();
        assert!(config.agent_files_authoritative);
        assert_eq!(
            config
                .multi_agent
                .unwrap()
                .custom_agents
                .iter()
                .find(|a| a.id == roster[0].id)
                .unwrap(),
            &roster[0]
        );
        assert!(editor.edit("multi_agent.custom_agents", Some("[]"), false).is_err());
    }

    #[test]
    fn edits_support_array_of_tables_and_inline_tables_without_dropping_comments() {
        let raw = "[[mcp.servers]]\nid = 'test'\ncommand = 'demo' # keep\nenv = { TOKEN = 'synthetic', OTHER = 'value' }\n";
        let updated = edit_document(raw, "mcp.servers.0.command", Some("'changed'")).unwrap();
        assert!(updated.contains("'changed' # keep"));
        let updated = edit_document(&updated, "mcp.servers.0.env.TOKEN", None).unwrap();
        assert!(!updated.contains("synthetic"));
        assert!(updated.contains("OTHER"));
        let mut value: Value =
            serde_json::to_value(toml::from_str::<toml::Value>(&updated).unwrap()).unwrap();
        redact_settings(&mut value);
        assert!(!value.to_string().contains("value"));
    }
}
