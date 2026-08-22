//! ADR-58/59 (rewritten) write seams: blueprint serialization, merge-aware TOML
//! editing, and the orchestration-roster seed writer.
//!
//! Three writers live here, all atomic (temp file in the same directory, then
//! `rename` over the target):
//!
//! - [`save_blueprint`] serializes a [`Blueprint`] to the include file
//!   (`orchestration.blueprint.toml`) — ADR-59 Decision 2 init step 1 and the
//!   Studio's include-file save.
//! - [`merge_edit_toml`] surgically edits one key inside an existing TOML
//!   document through `toml_edit`'s document model, preserving comments, key
//!   order, and unedited keys. String surgery is rejected as fragile per
//!   ADR-59 Decision 3; this is the declared current, tested need behind the
//!   `toml_edit` dependency. It backs both the include-file key edits and the
//!   one-time `[orchestration]` selection write (`save_blueprint_selection`,
//!   `lib.rs`).
//! - [`seed_orchestration_roster`] bootstraps the canonical orchestration
//!   roster (`[orchestration]` standard blueprint inline + the five
//!   [`crate::schema::builtin_agent_seeds`] under `[multi_agent.custom_agents]`)
//!   into an existing config, preserving every unrelated section. It goes
//!   through the same `toml_edit` document model directly because
//!   `merge_edit_toml` only edits a single key (`Item::from_str` parses one
//!   value) and this writer owns two whole tables.
//! - [`roster_materialized`] reports whether a config has ever materialized
//!   its roster — the raw `[multi_agent.custom_agents]` key is present in the
//!   parsed document (even as `[]`, meaning all agents were deleted).
//! - [`seed_agent_roster_only`] is the orphan-shape self-heal: it materializes
//!   ONLY `[multi_agent.custom_agents]` into a config that already owns an
//!   `[orchestration]` section (whose user-edited blueprint must survive
//!   byte-for-byte), sharing [`seed_orchestration_roster`]'s agent encoding.
//! - [`save_agent_roster`] (Slice 3) persists the Studio's edited agent roster
//!   back into `[multi_agent.custom_agents]` of the config, replacing the
//!   array wholesale (merge-aware, atomic) so the written entries ARE the
//!   roster — a deletion stays deleted, and the embedded seeds never merge
//!   back in.
//!
//! A failed write never leaves a truncated file at the target: the temp file
//! is written and flushed first, `rename` then atomically replaces the target
//! on the same filesystem (ADR-59 Testing atomicity).

use std::fs;
use std::io::Write;
use std::path::Path;

use concerto_core::error::ConfigError;

use crate::blueprint::{standard_blueprint, Blueprint, ORCHESTRATION_SCHEMA_VERSION};
use crate::schema::{builtin_agent_seeds, CustomAgentConfig};

/// Serialize a [`Blueprint`] to TOML and atomically write it to `path`
/// (ADR-59 Decision 3): `toml::to_string_pretty` → temp file in the same
/// directory → `rename`. Parent directories are created first, mirroring
/// `save_config` (`lib.rs`).
pub fn save_blueprint(blueprint: &Blueprint, path: &Path) -> Result<(), ConfigError> {
    let toml_str = toml::to_string_pretty(blueprint)
        .map_err(|e| ConfigError::Load(format!("failed to serialize blueprint: {e}")))?;
    atomic_write(path, toml_str.as_bytes())
}

/// Merge-edit a single key inside the TOML document at `path`, preserving
/// comments, key order, and unedited keys (ADR-59 Decision 3).
///
/// `table_path` names the nested table holding the key (e.g.
/// `["orchestration", "blueprint"]`); intermediate tables are created if
/// absent. `value` is a raw TOML inline value (e.g. `"standard"`, `true`,
/// `1`). The result is written back atomically via temp + rename.
pub(crate) fn merge_edit_toml(
    path: &Path,
    table_path: &[&str],
    key: &str,
    value: &str,
) -> Result<(), ConfigError> {
    let raw = fs::read_to_string(path)
        .map_err(|e| ConfigError::Load(format!("failed to read {}: {e}", path.display())))?;
    let mut doc = raw
        .parse::<toml_edit::DocumentMut>()
        .map_err(|e| ConfigError::Load(format!("failed to parse {}: {e}", path.display())))?;
    let value_item = value
        .parse::<toml_edit::Item>()
        .map_err(|e| ConfigError::Load(format!("failed to parse TOML value '{value}': {e}")))?;

    let mut table = doc.as_table_mut();
    for segment in table_path {
        table = ensure_table_mut(table, segment).map_err(|message| {
            ConfigError::Load(format!(
                "cannot edit '{}' in {}: {message}",
                path.display(),
                table_path.join("."),
            ))
        })?;
    }

    // Preserve an existing key's decorations when editing it in place: the
    // key's decor carries comments on the line above, and the value's decor
    // carries trailing comments — both must survive the merge (ADR-59
    // "the merge must never drop user comments or reorder keys").
    let existing_key = table.key(key).cloned();
    let existing_value_decor =
        table.get_key_value(key).and_then(|(_, item)| item.as_value().map(|v| v.decor().clone()));
    let mut new_item = value_item;
    if let Some(decor) = existing_value_decor {
        if let toml_edit::Item::Value(mut value) = new_item {
            *value.decor_mut() = decor;
            new_item = toml_edit::Item::Value(value);
        }
    }
    match existing_key {
        Some(existing_key) => {
            table.insert_formatted(&existing_key, new_item);
        }
        None => {
            table.insert(key, new_item);
        }
    }

    atomic_write(path, doc.to_string().as_bytes())
}

/// Write `contents` to `path` atomically: create parent directories, write to
/// a temp file in the same directory, flush, then `rename` over the target.
/// On any failure the temp file is removed and any pre-existing file at
/// `path` is left untouched.
fn atomic_write(path: &Path, contents: &[u8]) -> Result<(), ConfigError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| ConfigError::Load(format!("failed to create config dir: {e}")))?;
    }
    let file_name = path.file_name().ok_or_else(|| {
        ConfigError::Load(format!("cannot write to '{}': no file name", path.display()))
    })?;
    let tmp_path =
        path.with_file_name(format!(".{}.tmp-{}", file_name.to_string_lossy(), std::process::id()));

    let write_result = (|| -> std::io::Result<()> {
        let mut tmp = fs::File::create(&tmp_path)?;
        tmp.write_all(contents)?;
        tmp.flush()?;
        Ok(())
    })();
    if let Err(error) = write_result {
        let _ = fs::remove_file(&tmp_path);
        return Err(ConfigError::Load(format!(
            "failed to write temporary file for '{}': {error}",
            path.display()
        )));
    }

    fs::rename(&tmp_path, path).map_err(|error| {
        let _ = fs::remove_file(&tmp_path);
        ConfigError::Load(format!("failed to atomically replace '{}': {error}", path.display()))
    })
}

/// Navigate to the `key` sub-table of `table`, creating it when absent.
///
/// Errors when the key exists but is not a table (the caller's value cannot
/// be merged into it). Shared by [`merge_edit_toml`] and
/// [`seed_orchestration_roster`] so both writers agree on the traversal.
fn ensure_table_mut<'a>(
    table: &'a mut toml_edit::Table,
    key: &str,
) -> Result<&'a mut toml_edit::Table, String> {
    let item = match table.entry(key) {
        toml_edit::Entry::Occupied(entry) => entry.into_mut(),
        toml_edit::Entry::Vacant(entry) => {
            entry.insert(toml_edit::Item::Table(toml_edit::Table::new()))
        }
    };
    // Promote an existing inline-table value to a real table (preserving key
    // order) so callers can descend into a key the seed writer stores inline
    // (`seed_orchestration_roster` emits `blueprint = { inline = ... }` as a
    // single inline value). Both shapes are semantically identical.
    if item.as_table().is_none() {
        if let toml_edit::Item::Value(toml_edit::Value::InlineTable(inline)) = item {
            let mut promoted = toml_edit::Table::new();
            for (key, value) in inline.iter() {
                promoted.insert(key, toml_edit::Item::Value(value.clone()));
            }
            *item = toml_edit::Item::Table(promoted);
        }
    }
    item.as_table_mut().ok_or_else(|| format!("'{key}' exists but is not a table"))
}

/// Bootstrap the canonical orchestration roster into `config_path` (ADR-58/59 (rewritten)
/// seed writer).
///
/// Exactly two tables are owned and replaced wholesale, so the write is
/// **idempotent** (a second run produces byte-identical output) and every
/// unrelated section — providers, memory, retry, comments — is preserved
/// byte-for-byte:
///
/// - `[orchestration]`: `schema_version = {ORCHESTRATION_SCHEMA_VERSION}` with
///   `blueprint.inline` set to [`standard_blueprint`], so the config resolves
///   to the current default pipeline without an include file.
/// - `[multi_agent.custom_agents]`: the five [`builtin_agent_seeds`] as
///   `[[multi_agent.custom_agents]]` entries — the config owns the roster and
///   the entries are the roster (no embedded-seed merge back in).
///
/// The structurally nested fields of each owned value (`prompt_sections`,
/// `capabilities`, the stage `fallback`, etc.) are emitted as inline tables:
/// `toml_edit`'s display layer cannot render a nested section header under a
/// multi-element array-of-tables (every element would repeat the same header),
/// while inline values round-trip through `serde` identically. The seeding
/// still goes through the same `toml_edit` document model, so unrelated
/// content is untouched and the write is atomic ([`atomic_write`]).
pub fn seed_orchestration_roster(config_path: &Path) -> Result<(), ConfigError> {
    let raw = match fs::read_to_string(config_path) {
        Ok(raw) => raw,
        Err(_) if !config_path.exists() => {
            // ADR-58/59 (rewritten) Slice 2 (first-run bootstrap): a brand-new project with no
            // config file yet — seed a minimal schema-versioned document so the
            // two owned tables land in a real file (the other write seams
            // require an existing document). Callers never seed the global file.
            format!("schema_version = {}\n", crate::schema::SCHEMA_VERSION)
        }
        Err(error) => {
            return Err(ConfigError::Load(format!(
                "failed to read {}: {error}",
                config_path.display()
            )));
        }
    };
    let mut doc = raw.parse::<toml_edit::DocumentMut>().map_err(|e| {
        ConfigError::Load(format!("failed to parse {}: {e}", config_path.display()))
    })?;

    // The owned values, serialized by the `toml` crate (which renders nested
    // structures and arrays correctly) then re-encoded as inline `toml_edit`
    // values, per the inline-tables note above.
    let orchestration = {
        let mut orchestration = toml_edit::Table::new();
        orchestration
            .insert("schema_version", toml_edit::value(ORCHESTRATION_SCHEMA_VERSION as i64));
        let mut blueprint_selection = toml_edit::InlineTable::new();
        blueprint_selection
            .insert("inline", toml_edit_value(toml_value_of(&standard_blueprint())?));
        orchestration.insert(
            "blueprint",
            toml_edit::Item::Value(toml_edit::Value::InlineTable(blueprint_selection)),
        );
        orchestration
    };
    let agents = encode_agent_array(&builtin_agent_seeds())?;

    {
        let doc_table = doc.as_table_mut();

        // [orchestration] — replaced wholesale; `Table::insert` on an existing
        // key keeps the section's position in the document.
        {
            doc_table.insert("orchestration", toml_edit::Item::Table(orchestration));
        }

        // [multi_agent.custom_agents] — replaces only the owned key, preserving
        // the rest of [multi_agent] (model_pins, relationships, presets,
        // etc.). The whole [multi_agent] table is created when the config has
        // none yet.
        {
            let multi_agent = ensure_table_mut(doc_table, "multi_agent").map_err(|message| {
                ConfigError::Load(format!("cannot seed '{}': {message}", config_path.display()))
            })?;
            multi_agent.insert("custom_agents", toml_edit::Item::ArrayOfTables(agents));
        }
    }

    atomic_write(config_path, doc.to_string().as_bytes())
}

/// Whether the config at `config_path` has ever materialized its agent roster
/// — i.e. the parsed TOML document carries a `multi_agent.custom_agents` key
/// (any content, including an empty array). This is the raw-file signal that
/// the roster is owned (ADR-58/59: "key present" means owned, and deleting
/// every agent leaves `custom_agents = []`, so even an empty array means
/// owned).
///
/// A missing file reports `false` — nothing was ever materialized. A file that
/// cannot be parsed reports `true` — seeding over a broken file is never
/// attempted; a failed seed must leave the file intact.
pub fn roster_materialized(config_path: &Path) -> bool {
    let raw = match fs::read_to_string(config_path) {
        Ok(raw) => raw,
        Err(_) => return false,
    };
    let Ok(doc) = raw.parse::<toml_edit::DocumentMut>() else {
        return true;
    };
    doc.get("multi_agent")
        .and_then(|item| item.as_table())
        .is_some_and(|table| table.contains_key("custom_agents"))
}

/// Materialize ONLY the agent roster (the five [`builtin_agent_seeds`] under
/// `[multi_agent.custom_agents]`) into `config_path`, preserving every other
/// section — including an existing `[orchestration]` blueprint — byte-for-byte
/// (ADR-58/59 orphan self-heal).
///
/// The orphan shape is a config that owns an `[orchestration]` section (the
/// Studio's stage cards staff from the blueprint) but never materialized its
/// working roster. The full [`seed_orchestration_roster`] would clobber a
/// user-edited blueprint by replacing `[orchestration]` wholesale, so this
/// writer seeds ONLY the agents key through the same `toml_edit` document
/// model. Encoding matches [`seed_orchestration_roster`] exactly (both go
/// through [`encode_agent_array`]); the write is atomic ([`atomic_write`]) and
/// **idempotent** (re-seeding yields byte-identical output). A missing file is
/// bootstrapped with only `schema_version` first, like the other merge seams.
pub fn seed_agent_roster_only(config_path: &Path) -> Result<(), ConfigError> {
    let raw = match fs::read_to_string(config_path) {
        Ok(raw) => raw,
        Err(_) if !config_path.exists() => {
            format!("schema_version = {}\n", crate::schema::SCHEMA_VERSION)
        }
        Err(error) => {
            return Err(ConfigError::Load(format!(
                "failed to read {}: {error}",
                config_path.display()
            )));
        }
    };
    let mut doc = raw.parse::<toml_edit::DocumentMut>().map_err(|error| {
        ConfigError::Load(format!("failed to parse {}: {error}", config_path.display()))
    })?;

    let agents = encode_agent_array(&builtin_agent_seeds())?;

    // [multi_agent.custom_agents] — replaces only the owned key, preserving
    // the rest of [multi_agent] and every other section (including an existing
    // [orchestration] blueprint) byte-for-byte. The whole [multi_agent] table
    // is created when the config has none yet.
    let multi_agent = ensure_table_mut(doc.as_table_mut(), "multi_agent").map_err(|message| {
        ConfigError::Load(format!("cannot seed '{}': {message}", config_path.display()))
    })?;
    multi_agent.insert("custom_agents", toml_edit::Item::ArrayOfTables(agents));

    atomic_write(config_path, doc.to_string().as_bytes())
}

/// Encode a roster of agents into a `toml_edit` array-of-tables, serializing
/// each entry through the `toml` crate then re-encoding as inline `toml_edit`
/// values (nested structures render as inline tables — the array-of-tables
/// display limitation noted on [`seed_orchestration_roster`]). Shared by
/// [`seed_orchestration_roster`] and [`seed_agent_roster_only`] so both
/// writers emit byte-identical entries.
fn encode_agent_array(
    agents: &[CustomAgentConfig],
) -> Result<toml_edit::ArrayOfTables, ConfigError> {
    let mut array = toml_edit::ArrayOfTables::new();
    for agent in agents {
        let toml::Value::Table(map) = toml_value_of(agent)? else {
            return Err(ConfigError::Load(format!(
                "failed to serialize agent seed '{}': expected a table",
                agent.id
            )));
        };
        let mut element = toml_edit::Table::new();
        for (key, value) in map {
            element.insert(key.as_str(), toml_edit::Item::Value(toml_edit_value(value)));
        }
        array.push(element);
    }
    Ok(array)
}

/// ADR-58/59 (rewritten) Slice 3: persist the Studio's agent roster back into
/// `[multi_agent.custom_agents]` of the config at `path`
/// (merge-aware, atomic temp + rename), preserving every unrelated section,
/// comment, and key order.
///
/// The array is replaced wholesale rather than diffed: the config owns the
/// roster once it declares one ([`crate::AppConfig::owns_agent_roster`]), so
/// the written entries ARE the roster — an id deleted from the list stays
/// deleted, and the embedded `builtin_agent_seeds()` are never merged back in.
/// Like [`seed_orchestration_roster`], the write is **idempotent** (saving the
/// same roster twice produces byte-identical output).
///
/// The encoding mirrors [`seed_orchestration_roster`] exactly: each agent
/// serializes through the `toml` crate then re-encodes as inline `toml_edit`
/// values, so the nested structures (`prompt_sections`, `capabilities`, the
/// `stage` tag) render as inline tables rather than nested section headers
/// (the array-of-tables display limitation). A missing file is created with
/// only the top-level schema version first, like the other merge seams.
pub fn save_agent_roster(
    config_path: &Path,
    agents: &[CustomAgentConfig],
) -> Result<(), ConfigError> {
    let raw = match fs::read_to_string(config_path) {
        Ok(raw) => raw,
        Err(_) if !config_path.exists() => {
            format!("schema_version = {}\n", crate::schema::SCHEMA_VERSION)
        }
        Err(error) => {
            return Err(ConfigError::Load(format!(
                "failed to read {}: {error}",
                config_path.display()
            )));
        }
    };
    let mut doc = raw.parse::<toml_edit::DocumentMut>().map_err(|error| {
        ConfigError::Load(format!("failed to parse {}: {error}", config_path.display()))
    })?;

    let mut array = toml_edit::ArrayOfTables::new();
    for agent in agents {
        let toml::Value::Table(map) = toml_value_of(agent)? else {
            return Err(ConfigError::Load(format!(
                "failed to serialize agent '{}': expected a table",
                agent.id
            )));
        };
        let mut element = toml_edit::Table::new();
        for (key, value) in map {
            element.insert(key.as_str(), toml_edit::Item::Value(toml_edit_value(value)));
        }
        array.push(element);
    }

    // [multi_agent.custom_agents] — replaces only the owned key, preserving
    // the rest of [multi_agent] (model_pins, relationships, presets, run
    // limits, etc.). The whole [multi_agent] table is created when the config
    // has none yet.
    let multi_agent = ensure_table_mut(doc.as_table_mut(), "multi_agent").map_err(|message| {
        ConfigError::Load(format!("cannot save roster '{}': {message}", config_path.display()))
    })?;
    multi_agent.insert("custom_agents", toml_edit::Item::ArrayOfTables(array));

    atomic_write(config_path, doc.to_string().as_bytes())
}

/// ADR-59 Slice 2 (single-arm Save): persist an edited [`Blueprint`] back INTO
/// the `[orchestration].blueprint.inline` key of the config at `path`
/// (merge-aware, atomic temp + rename), preserving every unrelated section,
/// comment, and key order. The `name`/`include` selector keys are removed so
/// the selection stays exactly-one: materializing from a name- or include-based
/// selection must never leave a dangling sibling selector next to the written
/// `inline` (exactly-one load error).
///
/// A missing file is created with only the top-level schema version first — the
/// one case this seam creates the file instead of editing it — mirroring
/// [`seed_orchestration_roster`].
pub fn save_inline_blueprint(config_path: &Path, blueprint: &Blueprint) -> Result<(), ConfigError> {
    // A brand-new project has no config file yet; seed a minimal schema-versioned
    // document so the owned key can be merged into a real one.
    let raw = match fs::read_to_string(config_path) {
        Ok(raw) => raw,
        Err(_) if !config_path.exists() => {
            format!("schema_version = {}\n", crate::schema::SCHEMA_VERSION)
        }
        Err(error) => {
            return Err(ConfigError::Load(format!(
                "failed to read {}: {error}",
                config_path.display()
            )));
        }
    };
    let mut doc = raw.parse::<toml_edit::DocumentMut>().map_err(|error| {
        ConfigError::Load(format!("failed to parse {}: {error}", config_path.display()))
    })?;

    // Serialize the blueprint through the `toml` crate then re-encode as an
    // inline `toml_edit` value (nested structures round-trip identically; see
    // [`seed_orchestration_roster`] for the inline-table rationale).
    let toml::Value::Table(map) = toml_value_of(blueprint)? else {
        return Err(ConfigError::Load(
            "failed to serialize blueprint: expected a table".to_string(),
        ));
    };
    let mut inline = toml_edit::InlineTable::new();
    for (key, value) in map {
        inline.insert(key.as_str(), toml_edit_value(value));
    }

    let orchestration =
        ensure_table_mut(doc.as_table_mut(), "orchestration").map_err(|message| {
            ConfigError::Load(format!("cannot write '{}': {message}", config_path.display()))
        })?;
    let blueprint_table = ensure_table_mut(orchestration, "blueprint").map_err(|message| {
        ConfigError::Load(format!("cannot write '{}': {message}", config_path.display()))
    })?;
    blueprint_table.insert("inline", toml_edit::Item::Value(toml_edit::Value::InlineTable(inline)));
    blueprint_table.remove("name");
    blueprint_table.remove("include");

    atomic_write(config_path, doc.to_string().as_bytes())
}

/// Serialize any `serde` value through the `toml` crate's value model.
fn toml_value_of<T: serde::Serialize>(value: &T) -> Result<toml::Value, ConfigError> {
    toml::Value::try_from(value)
        .map_err(|e| ConfigError::Load(format!("failed to serialize value: {e}")))
}

/// Re-encode a `toml` crate value as a `toml_edit` value, mapping every table
/// to an inline table so no nested section header can collide inside an
/// array-of-tables element (a `toml_edit` display limitation — see
/// [`seed_orchestration_roster`]).
fn toml_edit_value(value: toml::Value) -> toml_edit::Value {
    match value {
        toml::Value::String(s) => toml_edit::Value::from(s),
        toml::Value::Integer(i) => toml_edit::Value::from(i),
        toml::Value::Float(f) => toml_edit::Value::from(f),
        toml::Value::Boolean(b) => toml_edit::Value::from(b),
        // No seed/blueprint field carries a datetime; a string form is the
        // degradation only if one ever appears.
        toml::Value::Datetime(d) => toml_edit::Value::from(d.to_string()),
        toml::Value::Array(items) => {
            let mut array = toml_edit::Array::new();
            for item in items {
                array.push(toml_edit_value(item));
            }
            toml_edit::Value::Array(array)
        }
        toml::Value::Table(map) => {
            let mut table = toml_edit::InlineTable::new();
            for (key, item) in map {
                table.insert(key.as_str(), toml_edit_value(item));
            }
            toml_edit::Value::InlineTable(table)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blueprint::{
        named_blueprint, BlueprintSelection, ExecutionFilesDef, FallbackPersonaDef, FeedLabel,
        PipelineDef, StageCondition, StageDef, StageFlags, StageKind, BLUEPRINT_INCLUDE_FILE,
    };
    use crate::schema::builtin_agent_seeds;
    use std::path::PathBuf;

    /// Minimal `StageDef` builder mirroring `blueprint.rs`'s private helper:
    /// everything defaulted; the caller overrides the pipeline-carrying
    /// fields.
    fn stage(tag: &str, label: &str, kind: impl Into<String>) -> StageDef {
        StageDef {
            tag: tag.to_string(),
            label: label.to_string(),
            kind: kind.into(),
            version: 1,
            flags: StageFlags::default(),
            condition: StageCondition::Always,
            max_cycles: None,
            feed: None,
            primary: false,
            agents: Vec::new(),
            fallback: None,
            files: None,
        }
    }

    /// ADR-59 Testing: `Blueprint` → TOML file → parse → equal. Exercises the
    /// optional fields (`description`, `fallback`, `files`, `system_
    /// instructions`) so both `None` and `Some` survive the round-trip.
    #[test]
    fn save_blueprint_round_trips() {
        let blueprint = crate::blueprint::Blueprint {
            schema_version: ORCHESTRATION_SCHEMA_VERSION,
            name: "round-trip".to_string(),
            description: Some("A blueprint that must survive a save/parse round-trip.".to_string()),
            pipeline: PipelineDef {
                stages: vec![
                    StageDef {
                        kind: StageKind::Execution.as_str().to_string(),
                        feed: Some(FeedLabel::Execute),
                        primary: true,
                        agents: vec!["coder".into()],
                        files: Some(ExecutionFilesDef {
                            ownership: "src/".into(),
                            expected_artifacts: vec!["src/lib.rs".into()],
                        }),
                        ..stage("implement", "Implement", StageKind::Execution.as_str())
                    },
                    StageDef {
                        kind: StageKind::Review.as_str().to_string(),
                        feed: None, // explicit None feed
                        condition: StageCondition::OnGateCycle,
                        max_cycles: Some(3),
                        agents: vec!["reviewer".into()],
                        fallback: Some(FallbackPersonaDef {
                            id: "coordinator".into(),
                            label: "Coordinator".into(),
                            system_instructions: Some("verify carefully".into()),
                            capabilities: StageFlags::default(),
                        }),
                        ..stage("verify", "Verify", StageKind::Review.as_str())
                    },
                    StageDef {
                        kind: StageKind::Acceptance.as_str().to_string(),
                        feed: Some(FeedLabel::Verify),
                        fallback: None, // explicit None fallback
                        ..stage("accept", "Accept", StageKind::Acceptance.as_str())
                    },
                ],
            },
            relationships: Vec::new(),
        };

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(BLUEPRINT_INCLUDE_FILE);
        save_blueprint(&blueprint, &path).expect("save must succeed");

        let raw = std::fs::read_to_string(&path).expect("saved file must be readable");
        let parsed: crate::blueprint::Blueprint =
            toml::from_str(&raw).expect("saved TOML must parse");
        assert_eq!(parsed, blueprint, "round-trip must preserve every field (None and Some)");
    }

    /// ADR-59 Testing (merge test): editing one key preserves comments,
    /// untouched keys, and key order — only the edited key changes.
    #[test]
    fn merge_edit_preserves_comments_order_and_siblings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let original = r#"# leading comment
schema_version = 7

# providers section
[providers]
primary = "openai"
# provider model
model = "gpt-4o"

[retry]
# retry delay in ms
fixed_delay_ms = 15000
"#;
        std::fs::write(&path, original).expect("seed config");

        merge_edit_toml(&path, &["retry"], "fixed_delay_ms", "20000")
            .expect("merge edit must succeed");

        let after = std::fs::read_to_string(&path).expect("read back");
        // Comments survive.
        for comment in
            ["# leading comment", "# providers section", "# provider model", "# retry delay in ms"]
        {
            assert!(after.contains(comment), "comment lost: {comment}\n{after}");
        }
        // Untouched keys keep their values.
        for key in ["schema_version = 7", "primary = \"openai\"", "model = \"gpt-4o\""] {
            assert!(after.contains(key), "untouched key changed: {key}\n{after}");
        }
        // Only the edited key changed.
        assert!(after.contains("fixed_delay_ms = 20000"), "edited key must be updated\n{after}");
        assert!(!after.contains("fixed_delay_ms = 15000"), "old value must be replaced\n{after}");
        // Key order preserved: providers before retry, comment before its key.
        let pos = |needle: &str| after.find(needle).expect("needle present");
        assert!(pos("# providers section") < pos("# retry delay in ms"));
        assert!(pos("# retry delay in ms") < pos("fixed_delay_ms = 20000"));
    }

    /// ADR-59 Testing (selection-edit test): the surgical `[orchestration]`
    /// selection write leaves unrelated `config.toml` content byte-identical
    /// and updates only the selection key.
    #[test]
    fn merge_edit_selection_test() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let original = r#"# user config
schema_version = 7

[providers]
primary = "openai"
model = "gpt-4o"

[retry]
fixed_delay_ms = 15000

[memory]
ttl_days = 30

[orchestration]
schema_version = 1

[orchestration.blueprint]
name = "tdd"
"#;
        std::fs::write(&path, original).expect("seed config");

        let selection =
            BlueprintSelection { name: Some("standard".into()), include: None, inline: None };
        crate::save_blueprint_selection(&path, &selection).expect("selection write must succeed");

        let after = std::fs::read_to_string(&path).expect("read back");
        // Unrelated sections byte-identical (no wholesale rewrite).
        for line in [
            "# user config",
            "schema_version = 7",
            "[providers]",
            "primary = \"openai\"",
            "model = \"gpt-4o\"",
            "[retry]",
            "fixed_delay_ms = 15000",
            "[memory]",
            "ttl_days = 30",
            "[orchestration]",
            "schema_version = 1",
        ] {
            assert!(after.contains(line), "unrelated content changed: missing {line}\n{after}");
        }
        // Only the selection key changed.
        assert!(after.contains("name = \"standard\""), "selection must be updated\n{after}");
        assert!(!after.contains("name = \"tdd\""), "old selection must be replaced\n{after}");
        // Section order preserved.
        let pos = |needle: &str| after.find(needle).expect("needle present");
        assert!(pos("[providers]") < pos("[retry]"));
        assert!(pos("[retry]") < pos("[memory]"));
        assert!(pos("[memory]") < pos("[orchestration]"));
    }

    /// ADR-59 Testing (atomicity test): a failed write leaves the previous
    /// file at the target intact — nothing is truncated or partially
    /// overwritten.
    #[test]
    fn save_atomic_leaves_previous_on_failure() {
        // Root ignores the read-only bit (CAP_DAC_OVERRIDE), so the failure
        // injection below only holds on a non-root host. Skip under root
        // rather than fail CI; the non-root path stays covered.
        let is_root = std::process::Command::new("id")
            .arg("-u")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .is_some_and(|s| s.trim() == "0");
        if is_root {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("config.toml");
        let previous = "schema_version = 7\n[retry]\nfixed_delay_ms = 30000\n";
        std::fs::write(&target, previous).expect("seed previous file");
        // Make the directory read-only so the temp-file write fails; the
        // write must abort without touching the existing file. (The test host
        // runs as a non-root user, so the read-only bit is honored.)
        let original = std::fs::metadata(dir.path()).expect("dir metadata").permissions();
        let mut perms = original.clone();
        perms.set_readonly(true);
        std::fs::set_permissions(dir.path(), perms).expect("set read-only");

        let result =
            save_blueprint(&named_blueprint("standard").expect("standard exists"), &target);

        // Restore the original permissions so the tempdir can be cleaned up.
        std::fs::set_permissions(dir.path(), original).expect("restore permissions");

        assert!(result.is_err(), "write into a read-only dir must fail");
        let saved = std::fs::read_to_string(&target).expect("target must remain readable");
        assert_eq!(saved, previous, "previous file must be untouched after a failed write");
        // No temp file left behind.
        let leftovers: Vec<PathBuf> =
            std::fs::read_dir(dir.path()).expect("read dir").flatten().map(|e| e.path()).collect();
        assert_eq!(leftovers, vec![target], "no temp file may remain");
    }

    /// A minimal config carrying unrelated sections that must survive the seed
    /// write byte-for-byte.
    fn seed_config(dir: &tempfile::TempDir) -> (std::path::PathBuf, String) {
        let path = dir.path().join("config.toml");
        let original = r#"# user config
schema_version = 7

[providers]
primary = "openai"
model = "gpt-4o"

[memory]
ttl_days = 30
"#;
        std::fs::write(&path, original).expect("seed config");
        (path, original.to_string())
    }

    /// ADR-58/59 (rewritten) Testing (seed write): the roster seed leaves unrelated sections
    /// byte-identical, writes both owned tables, and the result reloads,
    /// validates, and resolves.
    #[test]
    fn seed_orchestration_roster_writes_owned_tables_and_preserves_rest() {
        let dir = tempfile::tempdir().unwrap();
        let (path, _) = seed_config(&dir);

        seed_orchestration_roster(&path).expect("seed write must succeed");

        let after = std::fs::read_to_string(&path).expect("read back");
        // Unrelated sections byte-identical.
        for line in [
            "# user config",
            "schema_version = 7",
            "[providers]",
            "primary = \"openai\"",
            "model = \"gpt-4o\"",
            "[memory]",
            "ttl_days = 30",
        ] {
            assert!(after.contains(line), "unrelated content changed: missing {line}\n{after}");
        }
        // Owned tables present: orchestration section with the standard
        // blueprint inlined (one inline value) and the five seeded agents.
        assert!(after.contains("[orchestration]"), "orchestration section missing\n{after}");
        assert!(
            after.contains("blueprint = { inline = {") && after.contains("name = \"standard\""),
            "the standard blueprint must be inlined\n{after}"
        );
        assert_eq!(
            after.matches("[[multi_agent.custom_agents]]").count(),
            5,
            "five seeded agents expected\n{after}"
        );

        let cfg =
            crate::load_config(Some(&path), None).expect("seeded config must load and validate");
        assert!(cfg.owns_agent_roster(), "a seeded config owns its roster");
        let orchestration = cfg.orchestration.expect("[orchestration] must be present");
        assert_eq!(orchestration.schema_version, ORCHESTRATION_SCHEMA_VERSION);
        assert_eq!(
            orchestration.blueprint.inline.as_ref().map(|b| b.name.as_str()),
            Some("standard"),
            "the inline blueprint must be the standard default"
        );
        let resolved = orchestration
            .resolve(&[], None)
            .expect("the seeded standard blueprint must validate and resolve");
        assert_eq!(resolved.stages.len(), 5, "standard pipeline has five stages");
        let agents = &cfg.multi_agent.expect("[multi_agent] present").custom_agents;
        assert_eq!(agents.len(), 5, "the five built-in seeds must be written");
        assert!(agents.iter().all(|a| a.stage.is_some()), "every seed is staged");
    }

    /// ADR-58/59 (rewritten) Testing (idempotency): a second seed write produces byte-identical
    /// output — the owned tables are replaced, never appended.
    #[test]
    fn seed_orchestration_roster_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let (path, _) = seed_config(&dir);

        seed_orchestration_roster(&path).expect("first seed write");
        let first = std::fs::read_to_string(&path).expect("read back");
        seed_orchestration_roster(&path).expect("second seed write");
        let second = std::fs::read_to_string(&path).expect("read back");
        assert_eq!(first, second, "re-seeding must be byte-identical");
    }

    /// ADR-58/59 (rewritten) Testing (replacement): a pre-existing `custom_agents` array and a
    /// pre-existing `[orchestration]` blueprint are replaced, not duplicated,
    /// and other `[multi_agent]` keys survive.
    #[test]
    fn seed_orchestration_roster_replaces_prior_owned_tables_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"schema_version = 7
[multi_agent]
max_concurrent_agents = 1
[[multi_agent.custom_agents]]
id = "old-agent"
name = "Old Agent"
role = "old-agent"
[orchestration.blueprint]
name = "tdd"
"#,
        )
        .expect("seed config");

        seed_orchestration_roster(&path).expect("seed write must succeed");

        let after = std::fs::read_to_string(&path).expect("read back");
        assert!(
            after.contains("max_concurrent_agents = 1"),
            "unrelated [multi_agent] key must survive\n{after}"
        );
        assert!(
            !after.contains("old-agent"),
            "prior custom_agents entries must be replaced\n{after}"
        );
        assert!(
            !after.contains("name = \"tdd\""),
            "the prior [orchestration.blueprint] name must be replaced by the inline seed\n{after}"
        );

        let cfg =
            crate::load_config(Some(&path), None).expect("replaced config must load and validate");
        assert_eq!(cfg.multi_agent.as_ref().expect("[multi_agent] present").custom_agents.len(), 5);
        assert!(cfg.owns_agent_roster(), "a seeded config owns its roster");
    }

    /// The seeds written by [`seed_orchestration_roster`] are the five
    /// `builtin_agent_seeds`, staged by tag — proving the seed source matches
    /// the embedded defaults.
    #[test]
    fn seed_writer_writes_the_embedded_seeds() {
        let seeds = builtin_agent_seeds();
        let tags: Vec<&str> =
            seeds.iter().map(|s| s.stage.as_ref().map(|t| t.as_str()).unwrap_or("")).collect();
        assert_eq!(
            tags,
            vec!["design", "research", "implement", "review", "validate"],
            "seed order and staging must match the embedded defaults"
        );
    }

    /// ADR-58/59 (rewritten) Slice 2 (first-run bootstrap): seeding a brand-new project with
    /// no config file yet creates a minimal schema-versioned file carrying the
    /// owned `[orchestration]` + `[multi_agent.custom_agents]` tables, and the
    /// result loads, validates, and resolves like any seeded config.
    #[test]
    fn seed_orchestration_roster_creates_a_missing_config_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        assert!(!path.exists(), "precondition: no config file yet");

        seed_orchestration_roster(&path).expect("seeding a missing file must succeed");

        let raw = std::fs::read_to_string(&path).expect("created config must be readable");
        assert!(
            raw.contains("[orchestration]") && raw.contains("blueprint = { inline = {"),
            "the orchestration blueprint must be seeded\n{raw}"
        );
        assert_eq!(
            raw.matches("[[multi_agent.custom_agents]]").count(),
            5,
            "five seeded agents expected\n{raw}"
        );

        let cfg =
            crate::load_config(Some(&path), None).expect("seeded config must load and validate");
        assert!(cfg.owns_agent_roster(), "a seeded config owns its roster");
        let orchestration = cfg.orchestration.expect("[orchestration] must be present");
        assert_eq!(
            orchestration.blueprint.inline.as_ref().map(|b| b.name.as_str()),
            Some("standard"),
            "the inline blueprint must be the standard default"
        );
        let resolved =
            orchestration.resolve(&[], None).expect("the seeded standard blueprint must resolve");
        assert_eq!(resolved.stages.len(), 5, "standard pipeline has five stages");
    }

    /// ADR-58/59 (rewritten) Testing (orphan contract): [`roster_materialized`]
    /// reports roster ownership by the raw `multi_agent.custom_agents` key —
    /// a missing file and a `[multi_agent]`-free document are `false`, while
    /// `custom_agents = []` (every agent deleted), a populated roster, and an
    /// unparseable file are all `true` (an empty owned roster is still owned,
    /// and seeding over a broken file is never attempted).
    #[test]
    fn roster_materialized_detects_the_owned_key() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("config.toml");

        // Missing file → never materialized.
        assert!(!roster_materialized(&base), "a missing file never owns a roster");

        // A valid document with no [multi_agent] table → never materialized.
        let no_multi = dir.path().join("no_multi.toml");
        std::fs::write(&no_multi, "schema_version = 7\n[orchestration]\nschema_version = 1\n")
            .expect("seed config");
        assert!(!roster_materialized(&no_multi), "no [multi_agent] table owns nothing");

        // Empty array (all agents deleted) → still owned: deletions stick.
        let empty = dir.path().join("empty.toml");
        std::fs::write(&empty, "schema_version = 7\n[multi_agent]\ncustom_agents = []\n")
            .expect("seed config");
        assert!(roster_materialized(&empty), "an empty custom_agents key is still owned");

        // A populated roster → owned.
        let full = dir.path().join("full.toml");
        std::fs::write(
            &full,
            "schema_version = 7\n[[multi_agent.custom_agents]]\nid = \"architect\"\n",
        )
        .expect("seed config");
        assert!(roster_materialized(&full), "a populated roster is owned");

        // Unparseable file → owned (never seed over a broken file).
        let broken = dir.path().join("broken.toml");
        std::fs::write(&broken, "[unterminated\n").expect("seed broken config");
        assert!(roster_materialized(&broken), "a broken file must not be overwritten");
    }

    /// ADR-58/59 (rewritten) Testing (orphan self-heal): seeding ONLY the
    /// agents into a config that already carries a user-edited
    /// `[orchestration]` blueprint preserves the blueprint and an unrelated
    /// section byte-for-byte, writes exactly the five seed agents, and is
    /// idempotent (a second call yields byte-identical output).
    #[test]
    fn seed_agent_roster_only_preserves_existing_orchestration_blueprint() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"schema_version = 7

[orchestration]
schema_version = 1

[orchestration.blueprint]
name = "custom-blueprint"
description = "keep me"

[skills]
enabled = true
"#,
        )
        .expect("seed config");

        seed_agent_roster_only(&path).expect("first agents-only seed");
        let first = std::fs::read_to_string(&path).expect("read back");

        // The existing [orchestration] blueprint is preserved unchanged.
        assert!(
            first.contains("\"custom-blueprint\"") && first.contains("\"keep me\""),
            "the orchestration blueprint must be preserved\n{first}"
        );
        // The unrelated [skills] section survives.
        assert!(
            first.contains("[skills]") && first.contains("enabled = true"),
            "unrelated sections must survive\n{first}"
        );
        // Exactly the five seed agents landed under [multi_agent.custom_agents].
        assert_eq!(
            first.matches("[[multi_agent.custom_agents]]").count(),
            5,
            "five seeded agents expected\n{first}"
        );
        for id in ["architect", "researcher", "coder", "reviewer", "validator"] {
            assert!(first.contains(&format!("id = \"{id}\"")), "seed agent {id} missing\n{first}");
        }

        seed_agent_roster_only(&path).expect("second agents-only seed");
        let second = std::fs::read_to_string(&path).expect("read back");
        assert_eq!(first, second, "re-seeding must be byte-identical");
    }

    /// ADR-59 Slice 2 (single-arm Save): [`save_inline_blueprint`] writes the
    /// edited blueprint back into `[orchestration].blueprint.inline`, removes
    /// the `name`/`include` selector siblings (exactly-one selection), leaves
    /// unrelated sections byte-identical, and the result reloads to the edited
    /// blueprint.
    #[test]
    fn save_inline_blueprint_preserves_sections_and_collapses_to_inline() {
        let dir = tempfile::tempdir().unwrap();
        let (path, _) = seed_config(&dir);
        // Simulate a name-based selection being materialized: the config owns a
        // `[orchestration.blueprint] name = "standard"` selector.
        crate::saving::merge_edit_toml(
            &path,
            &["orchestration", "blueprint"],
            "name",
            "\"standard\"",
        )
        .expect("seed the name selector");

        // An edited blueprint: renamed + description changed to prove the
        // serialized form round-trips.
        let mut edited = standard_blueprint();
        edited.name = "edited-standard".to_string();
        edited.description = Some("edited by the Studio".to_string());

        save_inline_blueprint(&path, &edited).expect("inline save must succeed");

        let after = std::fs::read_to_string(&path).expect("read back");
        // Unrelated sections byte-identical.
        for line in [
            "# user config",
            "schema_version = 7",
            "[providers]",
            "primary = \"openai\"",
            "[memory]",
            "ttl_days = 30",
        ] {
            assert!(after.contains(line), "unrelated content changed: missing {line}\n{after}");
        }
        // Exactly-one selection: the sibling selectors are gone, inline is the
        // only selection key.
        assert!(
            !after.contains("name = \"standard\""),
            "a dangling name selector must be removed\n{after}"
        );
        assert!(
            after.contains("name = \"edited-standard\""),
            "the edited blueprint name must be written inline\n{after}"
        );
        assert!(
            after.contains("[orchestration.blueprint]") && after.contains("inline = {"),
            "the blueprint must be stored inline\n{after}"
        );

        let cfg =
            crate::load_config(Some(&path), None).expect("edited config must load and validate");
        let inline = cfg
            .orchestration
            .expect("[orchestration] present")
            .blueprint
            .inline
            .expect("the selection must still be inline");
        assert_eq!(inline.name, "edited-standard", "the reloaded blueprint is the edited one");
        assert_eq!(
            inline.description.as_deref(),
            Some("edited by the Studio"),
            "the edited description must survive the round-trip"
        );
    }

    /// A `CustomAgentConfig` with every field exercised (stage tag, structured
    /// prompt sections with few-shot exemplars, model/provider pins, explicit
    /// capabilities) so the roster round-trip proves nothing is dropped.
    fn rich_agent(id: &str) -> CustomAgentConfig {
        use crate::schema::{AgentCapabilities, FewShotExample, PromptSections};
        CustomAgentConfig {
            id: id.to_string(),
            name: format!("Agent {id}"),
            role: id.to_string(),
            stage: Some(concerto_core::AgentStage::new("run_once")),
            prompt_sections: PromptSections {
                system_instructions: format!("You are {id}."),
                constraints: "Never touch secrets.".into(),
                output_format: "Markdown".into(),
                few_shot: vec![
                    FewShotExample { input: "in".into(), output: "out".into() },
                    FewShotExample { input: "in 2".into(), output: "out 2".into() },
                ],
            },
            model_override: Some(format!("{id}-model")),
            provider_id: Some("openai".into()),
            capabilities: AgentCapabilities {
                fs_read: Some(true),
                fs_write: Some(true),
                shell: Some(false),
                git: Some(true),
                lsp: Some(true),
                eval: Some(true),
            },
            is_custom: true,
            disabled: true,
            output_mode: concerto_core::types::OutputMode::DesignDoc,
        }
    }

    /// ADR-58/59 (rewritten) Slice 3 Testing (roster save): the edited roster
    /// lands in `[multi_agent.custom_agents]`, every unrelated section stays
    /// byte-identical, and the result reloads to exactly the written agents.
    #[test]
    fn save_agent_roster_writes_the_roster_and_preserves_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let (path, _) = seed_config(&dir);
        let roster = vec![rich_agent("planner"), rich_agent("grunt")];

        save_agent_roster(&path, &roster).expect("roster save must succeed");

        let after = std::fs::read_to_string(&path).expect("read back");
        for line in [
            "# user config",
            "schema_version = 7",
            "[providers]",
            "primary = \"openai\"",
            "model = \"gpt-4o\"",
            "[memory]",
            "ttl_days = 30",
        ] {
            assert!(after.contains(line), "unrelated content changed: missing {line}\n{after}");
        }
        assert_eq!(
            after.matches("[[multi_agent.custom_agents]]").count(),
            2,
            "the saved roster has two entries\n{after}"
        );
        assert!(after.contains("id = \"planner\""), "planner entry present\n{after}");

        let cfg = crate::load_config(Some(&path), None).expect("saved config must load");
        assert!(cfg.owns_agent_roster(), "a config with a saved roster owns it");
        let agents = &cfg.multi_agent.expect("[multi_agent] present").custom_agents;
        assert_eq!(agents, &roster, "the reloaded roster equals the written agents");
    }

    /// Slice 3 Testing (deletion persists): the array is replaced wholesale, so
    /// an id removed from the saved list is gone from the file — the embedded
    /// seeds and prior entries never merge back in. Also proves idempotency.
    #[test]
    fn save_agent_roster_replaces_prior_entries_so_deletions_persist() {
        let dir = tempfile::tempdir().unwrap();
        let (path, _) = seed_config(&dir);

        let first = vec![rich_agent("keep"), rich_agent("drop")];
        save_agent_roster(&path, &first).expect("first roster save");

        // Remove "drop" and re-save; "keep" alone must remain.
        let second = vec![rich_agent("keep")];
        save_agent_roster(&path, &second).expect("second roster save");

        let after = std::fs::read_to_string(&path).expect("read back");
        assert!(after.contains("id = \"keep\""), "kept entry present\n{after}");
        assert!(!after.contains("drop"), "deleted entry must stay deleted\n{after}");
        assert_eq!(after.matches("[[multi_agent.custom_agents]]").count(), 1);

        let cfg = crate::load_config(Some(&path), None).expect("saved config must load");
        let agents = &cfg.multi_agent.expect("[multi_agent] present").custom_agents;
        assert_eq!(agents, &second, "only the re-saved roster survives");
    }

    /// Slice 3 Testing (missing-file bootstrap): saving the roster into a
    /// brand-new project with no config file yet creates a minimal
    /// schema-versioned file carrying `[multi_agent.custom_agents]`, and the
    /// result loads and owns its roster.
    #[test]
    fn save_agent_roster_creates_a_missing_config_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        assert!(!path.exists(), "precondition: no config file yet");

        save_agent_roster(&path, &[rich_agent("first")])
            .expect("saving a missing file must create it");

        let raw = std::fs::read_to_string(&path).expect("created config must be readable");
        assert!(raw.contains("schema_version = 7"), "schema version seeded\n{raw}");
        assert_eq!(raw.matches("[[multi_agent.custom_agents]]").count(), 1, "{raw}");

        let cfg = crate::load_config(Some(&path), None).expect("created config must load");
        assert!(cfg.owns_agent_roster(), "the created config owns its roster");
    }
}
