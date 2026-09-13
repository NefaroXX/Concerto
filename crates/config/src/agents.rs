//! Per-agent configuration files — the single source of truth for the agent
//! roster.
//!
//! Each agent lives in its own file at `<global-config-dir>/agents/<id>.toml`.
//! The file holds *all* of that agent's settings (id/name/role, stage,
//! `prompt_sections`, model override + provider, capabilities, the `disabled`
//! flag, and the structured submission `output_mode`) plus a per-file
//! `schema_version`. There is no other authoritative roster store: the load
//! seam in [`crate::load_config_layers`] merges the files into
//! `multi_agent.custom_agents` so every existing consumer (Studio, runtime
//! role resolution, registry) reads the file-sourced roster through the same
//! types it already used.
//!
//! ## Seed / migrate rules
//!
//! - **Directory absent** → not yet initialized. The roster is materialized
//!   from the hardcoded [`builtin_agent_seeds`] (the existing template
//!   source), unless an inline `[multi_agent.custom_agents]` roster already
//!   exists in the global config — in that case those entries are exported
//!   *once* (logged) before files rule. A materialized-but-empty inline roster
//!   (`custom_agents = []`, every agent deleted) materializes an empty
//!   directory: deletions stick, nothing is resurrected.
//! - **Directory present** → files rule. A missing file is an intentional
//!   deletion and is never reseeded. A file whose `schema_version` is newer
//!   than this build refuses loudly; older versions migrate forward.
//! - The coordinator is excluded everywhere: never seeded, never listed, never
//!   persisted (hardcoded, maintainer decision 2026-09).
//!
//! Writes are atomic temp + rename via [`crate::saving::atomic_write`], the
//! same discipline the config save seams use.

use std::path::{Path, PathBuf};

use concerto_core::error::ConfigError;

use crate::schema::{builtin_agent_seeds, AppConfig, CustomAgentConfig};

/// Per-agent file schema version. Bump for a breaking on-disk shape change;
/// older files migrate forward additively, newer files refuse loudly.
pub const AGENT_FILE_SCHEMA_VERSION: u32 = 1;

/// Directory name (under the global config dir) holding the per-agent files.
pub const AGENTS_DIR_NAME: &str = "agents";

/// The reserved, code-constructed coordinator id/role. Never on disk.
fn is_coordinator(agent: &CustomAgentConfig) -> bool {
    agent.id.eq_ignore_ascii_case("coordinator") || agent.role.eq_ignore_ascii_case("coordinator")
}

/// A validated, versioned per-agent file.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentFile {
    /// Per-file schema version.
    pub schema_version: u32,
    /// The agent's full settings.
    pub agent: CustomAgentConfig,
}

impl AgentFile {
    /// Wrap a roster entry with the current file schema version.
    pub fn new(agent: CustomAgentConfig) -> Self {
        Self { schema_version: AGENT_FILE_SCHEMA_VERSION, agent }
    }

    /// Serialize to TOML with `schema_version` as the first key.
    ///
    /// The agent is serialized through `toml::Value` and the version key is
    /// spliced into the resulting table, so the emitted document is a flat
    /// agent table plus one version scalar — no `serde(flatten)` ordering
    /// pitfalls.
    pub fn to_toml_string(&self) -> Result<String, ConfigError> {
        let mut value = toml::Value::try_from(&self.agent).map_err(|error| {
            ConfigError::Load(format!("failed to serialize agent '{}': {error}", self.agent.id))
        })?;
        let toml::Value::Table(ref mut table) = value else {
            return Err(ConfigError::Load(format!(
                "failed to serialize agent '{}': expected a table",
                self.agent.id
            )));
        };
        table.insert(
            "schema_version".to_string(),
            toml::Value::Integer(i64::from(self.schema_version)),
        );
        toml::to_string_pretty(&value).map_err(|error| {
            ConfigError::Load(format!("failed to encode agent '{}': {error}", self.agent.id))
        })
    }

    /// Parse a per-agent file, migrating older versions forward.
    ///
    /// A missing `schema_version` is treated as version 0 (pre-versioned) and
    /// migrates to the current version. A version newer than
    /// [`AGENT_FILE_SCHEMA_VERSION`] is a loud error — settings are never
    /// silently dropped.
    pub fn from_toml_str(raw: &str, path: &Path) -> Result<Self, ConfigError> {
        let value: toml::Value = toml::from_str(raw).map_err(|error| {
            ConfigError::Load(format!("failed to parse agent file '{}': {error}", path.display()))
        })?;
        let toml::Value::Table(mut table) = value else {
            return Err(ConfigError::Load(format!(
                "agent file '{}' must be a table",
                path.display()
            )));
        };
        let schema_version = match table.remove("schema_version") {
            None => 0,
            Some(toml::Value::Integer(version)) if version >= 0 => {
                u32::try_from(version).map_err(|_| {
                    ConfigError::InvalidValue(format!(
                        "agent file '{}' has out-of-range schema_version {version}",
                        path.display()
                    ))
                })?
            }
            Some(other) => {
                return Err(ConfigError::InvalidValue(format!(
                    "agent file '{}' has a non-integer schema_version ({other})",
                    path.display()
                )));
            }
        };
        let agent = toml::Value::Table(table).try_into::<CustomAgentConfig>().map_err(|error| {
            ConfigError::Load(format!("failed to decode agent file '{}': {error}", path.display()))
        })?;
        AgentFile { schema_version, agent }.migrate(path)
    }

    /// Additive forward migration. No structural steps exist at v1; a
    /// pre-versioned (v0) file simply adopts the current version. Future
    /// versions append match arms here — never drop fields.
    pub fn migrate(self, path: &Path) -> Result<Self, ConfigError> {
        if self.schema_version > AGENT_FILE_SCHEMA_VERSION {
            return Err(ConfigError::InvalidValue(format!(
                "agent file '{}' uses schema_version {} which is newer than this build \
                 supports ({}); refusing to load rather than dropping settings",
                path.display(),
                self.schema_version,
                AGENT_FILE_SCHEMA_VERSION
            )));
        }
        let mut migrated = self;
        // Future: `if migrated.schema_version < 2 { ... }` additive steps.
        migrated.schema_version = AGENT_FILE_SCHEMA_VERSION;
        Ok(migrated)
    }
}

impl TryFrom<toml::Value> for CustomAgentConfig {
    type Error = toml::de::Error;

    fn try_from(value: toml::Value) -> Result<Self, Self::Error> {
        value.try_into()
    }
}

/// The `agents/` directory belonging to the global config file at
/// `config_path`.
///
/// Errors when the config path has no parent directory (nothing to anchor the
/// directory to).
pub fn agents_dir_for_config(config_path: &Path) -> Result<PathBuf, ConfigError> {
    config_path.parent().map(|parent| parent.join(AGENTS_DIR_NAME)).ok_or_else(|| {
        ConfigError::Load(format!(
            "cannot resolve an agents directory for '{}': the path has no parent",
            config_path.display()
        ))
    })
}

/// Validate that an agent id is a safe single path component.
fn validate_agent_id(id: &str) -> Result<(), ConfigError> {
    if id.is_empty() {
        return Err(ConfigError::InvalidValue("agent id must be non-empty".to_string()));
    }
    if id == "." || id == ".." {
        return Err(ConfigError::InvalidValue(format!("agent id '{id}' is not a valid file name")));
    }
    if id.chars().any(|c| c == '/' || c == '\\' || c == '\0' || c.is_control()) {
        return Err(ConfigError::InvalidValue(format!(
            "agent id '{id}' contains a path separator or control character and cannot be a file name"
        )));
    }
    Ok(())
}

/// The file path for `id` inside `dir`.
pub fn agent_file_path(dir: &Path, id: &str) -> Result<PathBuf, ConfigError> {
    validate_agent_id(id)?;
    Ok(dir.join(format!("{id}.toml")))
}

/// Write one agent to `<dir>/<id>.toml` atomically.
pub fn write_agent_file(dir: &Path, agent: &CustomAgentConfig) -> Result<PathBuf, ConfigError> {
    if is_coordinator(agent) {
        return Err(ConfigError::InvalidValue(
            "the coordinator is hardcoded and cannot be persisted to an agent file".to_string(),
        ));
    }
    let path = agent_file_path(dir, &agent.id)?;
    let contents = AgentFile::new(agent.clone()).to_toml_string()?;
    crate::saving::atomic_write(&path, contents.as_bytes())?;
    Ok(path)
}

/// Delete the file for `id` if present. Returns whether a file was removed.
///
/// Deleting a file is the deletion-sticks semantics: the directory remains
/// initialized, so a later load will not reseed the id.
pub fn delete_agent_file(dir: &Path, id: &str) -> Result<bool, ConfigError> {
    let path = agent_file_path(dir, id)?;
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(ConfigError::Load(format!(
            "failed to delete agent file '{}': {error}",
            path.display()
        ))),
    }
}

/// Replace the on-disk roster under `dir` with `agents`.
///
/// Writes each entry atomically, then removes every `*.toml` file whose stem
/// is no longer part of the roster — so a removal in the editor deletes the
/// file and sticks. The coordinator is filtered out. Ids are validated before
/// any write so a bad entry never leaves a half-applied roster.
pub fn save_agent_roster_files(
    dir: &Path,
    agents: &[CustomAgentConfig],
) -> Result<Vec<String>, ConfigError> {
    let mut wanted: Vec<&CustomAgentConfig> =
        agents.iter().filter(|agent| !is_coordinator(agent)).collect();
    // Deterministic write order (file listing order is otherwise
    // filesystem-dependent); stable for tests and diffs.
    wanted.sort_by(|a, b| a.id.cmp(&b.id));

    let mut seen: Vec<&str> = Vec::with_capacity(wanted.len());
    for agent in &wanted {
        validate_agent_id(&agent.id)?;
        if seen.contains(&agent.id.as_str()) {
            return Err(ConfigError::InvalidValue(format!(
                "duplicate agent id '{}' in the roster",
                agent.id
            )));
        }
        seen.push(agent.id.as_str());
    }

    std::fs::create_dir_all(dir).map_err(|error| {
        ConfigError::Load(format!("failed to create agents dir '{}': {error}", dir.display()))
    })?;

    let mut written = Vec::with_capacity(wanted.len());
    for agent in &wanted {
        write_agent_file(dir, agent)?;
        written.push(agent.id.clone());
    }

    // Deletion pass: remove stale `*.toml` files not in the new roster.
    let entries = std::fs::read_dir(dir).map_err(|error| {
        ConfigError::Load(format!("failed to read agents dir '{}': {error}", dir.display()))
    })?;
    for entry in entries {
        let entry = entry.map_err(|error| {
            ConfigError::Load(format!("failed to read agents dir '{}': {error}", dir.display()))
        })?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("toml") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        if !seen.contains(&stem) {
            std::fs::remove_file(&path).map_err(|error| {
                ConfigError::Load(format!(
                    "failed to delete removed agent file '{}': {error}",
                    path.display()
                ))
            })?;
        }
    }
    Ok(written)
}

/// Load the file-backed roster from `dir`.
///
/// `Ok(None)` means the directory does not exist: files are not (yet) the
/// authoritative source, so callers fall back to the inline config roster.
/// `Ok(Some(roster))` means files rule — the returned list is exactly the
/// agents on disk (possibly empty after deletions). Coordinator files are
/// ignored (never listed) and duplicate ids are a loud error.
pub fn load_agent_roster(dir: &Path) -> Result<Option<Vec<CustomAgentConfig>>, ConfigError> {
    if !dir.is_dir() {
        return Ok(None);
    }
    let entries = std::fs::read_dir(dir).map_err(|error| {
        ConfigError::Load(format!("failed to read agents dir '{}': {error}", dir.display()))
    })?;
    let mut paths: Vec<PathBuf> = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| {
            ConfigError::Load(format!("failed to read agents dir '{}': {error}", dir.display()))
        })?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) == Some("toml") {
            paths.push(path);
        }
    }
    paths.sort();

    let mut roster = Vec::with_capacity(paths.len());
    for path in paths {
        let raw = std::fs::read_to_string(&path).map_err(|error| {
            ConfigError::Load(format!("failed to read agent file '{}': {error}", path.display()))
        })?;
        let file = AgentFile::from_toml_str(&raw, &path)?;
        if is_coordinator(&file.agent) {
            // Defensive: a hand-dropped coordinator file is ignored, never
            // listed, and never overwrites the hardcoded coordinator.
            tracing::warn!(
                path = %path.display(),
                "ignoring coordinator entry in the agents directory (hardcoded agent)"
            );
            continue;
        }
        if roster.iter().any(|existing: &CustomAgentConfig| existing.id == file.agent.id) {
            return Err(ConfigError::InvalidValue(format!(
                "duplicate agent id '{}' across the agents directory",
                file.agent.id
            )));
        }
        roster.push(file.agent);
    }
    Ok(Some(roster))
}

/// Outcome of [`ensure_agent_files`], for logging and tests.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct EnsureAgentFilesOutcome {
    /// The agents directory did not exist and was created.
    pub dir_created: bool,
    /// The directory existed already: files rule, nothing was written.
    pub already_initialized: bool,
    /// The roster was written from an existing inline config roster.
    pub exported_inline: bool,
    /// The roster was written from the hardcoded builtin seeds.
    pub seeded_defaults: bool,
    /// Ids written to disk, in deterministic order.
    pub written: Vec<String>,
}

/// Parse the inline `[multi_agent.custom_agents]` roster out of a config file
/// without running it through the layered load seam.
fn inline_roster(config_path: &Path) -> Result<Vec<CustomAgentConfig>, ConfigError> {
    match std::fs::read_to_string(config_path) {
        Ok(raw) => {
            let config: AppConfig = toml::from_str(&raw).map_err(|error| {
                ConfigError::Load(format!(
                    "failed to parse '{}' while migrating its inline roster: {error}",
                    config_path.display()
                ))
            })?;
            Ok(config
                .multi_agent
                .map(|multi| multi.custom_agents)
                .unwrap_or_default()
                .into_iter()
                .filter(|agent| !is_coordinator(agent))
                .collect())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(ConfigError::Load(format!(
            "failed to read '{}' while migrating its inline roster: {error}",
            config_path.display()
        ))),
    }
}

/// Materialize the per-agent files for `global_config_path` when they do not
/// yet exist.
///
/// The directory's presence is the initialization marker: once it exists,
/// files rule and this is a strict no-op (a missing file is an intentional
/// deletion). When it is absent the roster is exported once from an existing
/// inline config roster (global, then an optional legacy project file), or
/// seeded from [`builtin_agent_seeds`] when no inline roster exists. A
/// materialized-but-empty inline roster creates an empty directory.
///
/// If any inline entry cannot be exported, the whole migration aborts with an
/// error *before* the directory is created: the inline entry stays readable
/// and authoritative rather than being silently dropped.
pub fn ensure_agent_files(
    global_config_path: &Path,
    legacy_project_config_path: Option<&Path>,
) -> Result<EnsureAgentFilesOutcome, ConfigError> {
    let dir = agents_dir_for_config(global_config_path)?;
    if dir.is_dir() {
        return Ok(EnsureAgentFilesOutcome { already_initialized: true, ..Default::default() });
    }

    // Source decision. `roster_materialized` reports the raw key presence even
    // for `custom_agents = []` (every agent deleted), which must map to an
    // empty directory — never a reseed.
    let materialized = crate::saving::roster_materialized(global_config_path);
    let global_inline = inline_roster(global_config_path)?;
    let source = if !global_inline.is_empty() {
        (global_inline, true)
    } else if materialized {
        (Vec::new(), true)
    } else if let Some(project_path) = legacy_project_config_path {
        let project_inline = inline_roster(project_path)?;
        if project_inline.is_empty() {
            (Vec::new(), false)
        } else {
            (project_inline, true)
        }
    } else {
        (Vec::new(), false)
    };
    let (mut roster, from_inline) = source;

    let exported_inline = from_inline;
    let seeded_defaults = !from_inline;
    if seeded_defaults {
        roster = builtin_agent_seeds().into_iter().filter(|agent| !is_coordinator(agent)).collect();
    }

    // Validate ids before creating the directory so a bad entry cannot leave a
    // half-materialized (or empty) authoritative store.
    for agent in &roster {
        validate_agent_id(&agent.id)?;
    }

    std::fs::create_dir_all(&dir).map_err(|error| {
        ConfigError::Load(format!("failed to create agents dir '{}': {error}", dir.display()))
    })?;
    // `save_agent_roster_files` is idempotent for a just-created empty dir.
    let written = save_agent_roster_files(&dir, &roster)?;

    if exported_inline {
        tracing::info!(
            dir = %dir.display(),
            count = written.len(),
            "migrated inline agent roster to per-agent config files (one-time)"
        );
    } else if seeded_defaults {
        tracing::info!(
            dir = %dir.display(),
            count = written.len(),
            "seeded per-agent config files from the builtin defaults"
        );
    }

    Ok(EnsureAgentFilesOutcome {
        dir_created: true,
        already_initialized: false,
        exported_inline,
        seeded_defaults,
        written,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{AgentCapabilities, PromptSections};
    use concerto_core::types::OutputMode;

    fn sample_agent(id: &str) -> CustomAgentConfig {
        CustomAgentConfig {
            id: id.to_string(),
            name: format!("Agent {id}"),
            role: id.to_string(),
            prompt_sections: PromptSections {
                system_instructions: format!("You are {id}."),
                constraints: "be careful".into(),
                output_format: "text".into(),
                few_shot: Vec::new(),
            },
            model_override: Some("model-x".into()),
            provider_id: Some("provider-1".into()),
            capabilities: AgentCapabilities { fs_write: Some(true), ..Default::default() },
            is_custom: true,
            disabled: true,
            output_mode: OutputMode::Freeform,
            ..Default::default()
        }
    }

    fn write_config(path: &Path, raw: &str) {
        std::fs::write(path, raw).expect("write config fixture");
    }

    #[test]
    fn agent_file_round_trips_all_fields() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");
        let agents_dir = agents_dir_for_config(&config_path).expect("agents dir");
        let agent = sample_agent("custom-1");

        write_agent_file(&agents_dir, &agent).expect("write agent file");
        let roster = load_agent_roster(&agents_dir).expect("load").expect("dir exists");
        assert_eq!(roster, vec![agent]);
    }

    #[test]
    fn agent_file_missing_schema_version_migrates_to_current() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("legacy.toml");
        // No schema_version: pre-versioned shape migrates to current.
        std::fs::write(&path, "id = \"legacy\"\nname = \"Legacy\"\nrole = \"legacy\"\n")
            .expect("write");
        let file = AgentFile::from_toml_str(&std::fs::read_to_string(&path).expect("read"), &path)
            .expect("migrate");
        assert_eq!(file.schema_version, AGENT_FILE_SCHEMA_VERSION);
        assert_eq!(file.agent.id, "legacy");
    }

    #[test]
    fn future_schema_version_is_refused_loudly() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("future.toml");
        std::fs::write(&path, "schema_version = 99\nid = \"x\"\nname = \"X\"\nrole = \"x\"\n")
            .expect("write");
        let err = AgentFile::from_toml_str(&std::fs::read_to_string(&path).expect("read"), &path)
            .expect_err("future version must be refused");
        let message = err.to_string();
        assert!(message.contains("newer"), "{message}");
        assert!(message.contains("refusing"), "{message}");
    }

    #[test]
    fn missing_agents_dir_is_not_authoritative() {
        let dir = tempfile::tempdir().expect("tempdir");
        let agents_dir = dir.path().join(AGENTS_DIR_NAME);
        assert!(
            load_agent_roster(&agents_dir).expect("load").is_none(),
            "an absent directory means files do not rule"
        );
    }

    #[test]
    fn ensure_seeds_builtin_defaults_when_no_inline_roster_exists() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");
        write_config(&config_path, "schema_version = 8\n");

        let outcome = ensure_agent_files(&config_path, None).expect("ensure");
        assert!(outcome.dir_created);
        assert!(outcome.seeded_defaults);
        assert!(!outcome.exported_inline);
        assert_eq!(outcome.written.len(), builtin_agent_seeds().len());

        let agents_dir = agents_dir_for_config(&config_path).expect("dir");
        let roster = load_agent_roster(&agents_dir).expect("load").expect("dir exists");
        assert_eq!(roster.len(), builtin_agent_seeds().len());
        assert!(!roster.iter().any(is_coordinator), "the coordinator is never seeded");
    }

    #[test]
    fn ensure_exports_existing_inline_roster_once() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");
        write_config(
            &config_path,
            r#"schema_version = 8
[multi_agent]
[[multi_agent.custom_agents]]
id = "coder"
name = "Coder"
role = "coder"
"#,
        );

        let outcome = ensure_agent_files(&config_path, None).expect("ensure");
        assert!(outcome.exported_inline, "inline roster must be exported");
        assert!(!outcome.seeded_defaults);
        assert_eq!(outcome.written, vec!["coder".to_string()]);

        let serialized = std::fs::read_to_string(config_path.with_file_name("agents/coder.toml"))
            .expect("coder file");
        assert!(serialized.contains("schema_version = 1"), "{serialized}");

        // Second call: the directory exists, so it is a strict no-op.
        let second = ensure_agent_files(&config_path, None).expect("ensure again");
        assert!(second.already_initialized);
        assert!(!second.dir_created);
        assert!(second.written.is_empty());
    }

    #[test]
    fn materialized_empty_inline_roster_creates_empty_dir_without_reseeding() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");
        write_config(&config_path, "schema_version = 8\n[multi_agent]\ncustom_agents = []\n");

        let outcome = ensure_agent_files(&config_path, None).expect("ensure");
        assert!(outcome.dir_created);
        assert!(!outcome.seeded_defaults, "a materialized empty roster must not reseed");
        assert!(outcome.written.is_empty());

        let agents_dir = agents_dir_for_config(&config_path).expect("dir");
        let roster = load_agent_roster(&agents_dir).expect("load").expect("dir exists");
        assert!(roster.is_empty(), "deletions stick: no seed resurrection");
    }

    #[test]
    fn invalid_inline_id_aborts_export_without_creating_the_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");
        write_config(
            &config_path,
            r#"schema_version = 8
[multi_agent]
[[multi_agent.custom_agents]]
id = "bad/id"
name = "Bad"
role = "bad"
"#,
        );

        let err = ensure_agent_files(&config_path, None).expect_err("invalid id must abort");
        assert!(err.to_string().contains("bad/id"), "{err}");
        let agents_dir = agents_dir_for_config(&config_path).expect("dir");
        assert!(
            !agents_dir.exists(),
            "no directory is created when an entry cannot be exported (inline stays readable)"
        );
    }

    #[test]
    fn legacy_project_roster_is_exported_when_global_has_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        let global = dir.path().join("config.toml");
        write_config(&global, "schema_version = 8\n");
        let project = dir.path().join(".concerto.toml");
        write_config(
            &project,
            r#"schema_version = 8
[multi_agent]
[[multi_agent.custom_agents]]
id = "legacy-project-agent"
name = "Legacy"
role = "legacy-project-agent"
"#,
        );

        let outcome = ensure_agent_files(&global, Some(&project)).expect("ensure");
        assert!(outcome.exported_inline);
        assert_eq!(outcome.written, vec!["legacy-project-agent".to_string()]);
    }

    #[test]
    fn save_roster_files_writes_and_deletes_so_removals_stick() {
        let dir = tempfile::tempdir().expect("tempdir");
        let agents_dir = dir.path().join(AGENTS_DIR_NAME);
        let first = vec![sample_agent("alpha"), sample_agent("beta")];
        save_agent_roster_files(&agents_dir, &first).expect("initial save");

        // Remove beta: its file must be deleted, alpha kept.
        save_agent_roster_files(&agents_dir, &[sample_agent("alpha")]).expect("second save");
        let roster = load_agent_roster(&agents_dir).expect("load").expect("dir");
        assert_eq!(roster.len(), 1);
        assert_eq!(roster[0].id, "alpha");
    }

    #[test]
    fn coordinator_is_never_persisted_or_listed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let agents_dir = dir.path().join(AGENTS_DIR_NAME);
        let coordinator = CustomAgentConfig {
            id: "coordinator".into(),
            role: "coordinator".into(),
            ..Default::default()
        };
        assert!(write_agent_file(&agents_dir, &coordinator).is_err());

        // A roster containing a coordinator entry filters it out silently.
        let written = save_agent_roster_files(&agents_dir, &[coordinator, sample_agent("coder")])
            .expect("save");
        assert_eq!(written, vec!["coder".to_string()]);
        let roster = load_agent_roster(&agents_dir).expect("load").expect("dir");
        assert_eq!(roster.len(), 1);
        assert!(!roster.iter().any(is_coordinator));
    }

    #[test]
    fn duplicate_ids_in_the_roster_are_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let agents_dir = dir.path().join(AGENTS_DIR_NAME);
        let dupes = vec![sample_agent("same"), sample_agent("same")];
        assert!(save_agent_roster_files(&agents_dir, &dupes).is_err());
    }

    #[test]
    fn load_roster_refuses_a_duplicate_id_across_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let agents_dir = dir.path().join(AGENTS_DIR_NAME);
        std::fs::create_dir_all(&agents_dir).expect("dir");
        // Two files claiming the same content id (different file stems).
        let content = AgentFile::new(sample_agent("dup")).to_toml_string().expect("ser");
        std::fs::write(agents_dir.join("a.toml"), &content).expect("write");
        std::fs::write(agents_dir.join("b.toml"), &content).expect("write");
        assert!(load_agent_roster(&agents_dir).is_err());
    }
}
