//! Global-only orchestration strip/import — the "orchestration is GLOBAL
//! ONLY" enforcement half of the write seams (maintainer decision, 2026-09).
//!
//! This cluster owns the raw TOML key set the load path IGNORES at the
//! project layer ([`GLOBAL_ONLY_ORCHESTRATION_KEYS`]) and the four seams that
//! act on it: declare ([`declared_project_orchestration_keys`]), strip
//! ([`strip_project_orchestration_keys`]), explicit import
//! ([`import_project_orchestration_to_global`]) and explicit removal
//! ([`remove_project_orchestration_keys`]). The function bodies move verbatim
//! from `saving.rs`; the parent module re-exports them so every existing
//! `crate::saving::{...}` path — including the crate-root re-exports in
//! `lib.rs` — keeps resolving unchanged. The colocated tests pin the
//! strip/declare/import edges: comments and key order survive a strip, and a
//! conflicting import refuses with nothing written to either file.

use std::fs;
use std::path::Path;

use concerto_core::error::ConfigError;

use super::{atomic_write, ensure_table_mut};

/// The GLOBAL-ONLY orchestration key set (maintainer decision, 2026-09:
/// "orchestration is GLOBAL ONLY"). These raw TOML keys are IGNORED at the
/// project layer on every load: the `[orchestration]` table entirely, the
/// `[multi_agent.custom_agents]` roster, and the `[multi_agent.model_pins]`
/// legacy assignment map (its only reader is the dispatch-assignment model
/// resolution in `runtime_runner::legacy_pins_from_config` — it feeds
/// neither policy rules nor spend machinery). Everything else in a project
/// file stays layered exactly as before (relationships, presets, run limits,
/// spend caps, policy rules).
pub const GLOBAL_ONLY_ORCHESTRATION_KEYS: &[&str] =
    &["orchestration", "multi_agent.custom_agents", "multi_agent.model_pins"];

/// Outcome of [`strip_project_orchestration_keys`]: the document to merge and
/// which global-only keys were removed (empty = nothing to strip).
pub struct ProjectOrchestrationStrip {
    /// The project document with the global-only keys removed. Byte-identical
    /// to the input when `removed_keys` is empty.
    pub document: String,
    /// The removed keys, named as [`GLOBAL_ONLY_ORCHESTRATION_KEYS`] entries.
    pub removed_keys: Vec<String>,
}

/// Remove the [`GLOBAL_ONLY_ORCHESTRATION_KEYS`] from a parsed document,
/// returning the removed raw TOML key names in document order.
fn remove_global_only_orchestration_keys(doc: &mut toml_edit::DocumentMut) -> Vec<String> {
    let mut removed = Vec::new();
    if doc.get("orchestration").is_some() {
        doc.remove("orchestration");
        removed.push("orchestration".to_string());
    }
    if let Some(table) = doc.get_mut("multi_agent").and_then(|item| item.as_table_mut()) {
        for key in ["custom_agents", "model_pins"] {
            if table.remove(key).is_some() {
                removed.push(format!("multi_agent.{key}"));
            }
        }
    }
    removed
}
/// Strip the global-only orchestration keys ([`GLOBAL_ONLY_ORCHESTRATION_KEYS`])
/// from a raw project config document, preserving every other key, comment,
/// and key order through the `toml_edit` document model.
///
/// Returns `None` when the document declares none of the ignored keys — the
/// caller must then merge the raw text unmodified (the load path stays
/// byte-identical in that case). An unparseable document is an error; the load
/// seam surfaces it the same way it surfaces a figment parse failure.
pub fn strip_project_orchestration_keys(
    raw: &str,
) -> Result<Option<ProjectOrchestrationStrip>, ConfigError> {
    let mut doc = raw
        .parse::<toml_edit::DocumentMut>()
        .map_err(|e| ConfigError::Load(format!("failed to parse project config: {e}")))?;
    let removed_keys = remove_global_only_orchestration_keys(&mut doc);
    if removed_keys.is_empty() {
        return Ok(None);
    }
    Ok(Some(ProjectOrchestrationStrip { document: doc.to_string(), removed_keys }))
}

/// Which global-only orchestration keys the raw TOML config file at
/// `config_path` declares (missing file / unparseable file → empty). Used by
/// the Studio banner (project declares keys the load path ignores) and by
/// the auto-seed guard.
pub fn declared_project_orchestration_keys(config_path: &Path) -> Vec<String> {
    let raw = match fs::read_to_string(config_path) {
        Ok(raw) => raw,
        Err(_) => return Vec::new(),
    };
    let Ok(doc) = raw.parse::<toml_edit::DocumentMut>() else {
        return Vec::new();
    };
    global_only_keys_declared_in(doc.as_table())
}

/// Names of the [`GLOBAL_ONLY_ORCHESTRATION_KEYS`] an already-parsed TOML
/// table declares. Shared by the declare/strip/import helpers so all three
/// agree on the ignored-key set.
fn global_only_keys_declared_in(table: &toml_edit::Table) -> Vec<String> {
    let mut declared = Vec::new();
    if table.contains_key("orchestration") {
        declared.push("orchestration".to_string());
    }
    if let Some(multi_agent) = table.get("multi_agent").and_then(|item| item.as_table()) {
        for key in ["custom_agents", "model_pins"] {
            if multi_agent.contains_key(key) {
                declared.push(format!("multi_agent.{key}"));
            }
        }
    }
    declared
}

/// The explicit import action (no silent motion, no silent deletion):
/// copy the global-only orchestration keys from the project file into the
/// GLOBAL file, then remove them from the project file — one user action,
/// atomic on both sides.
///
/// Conflict rule: the import REFUSES (writes nothing, to either file) when
/// the global file already declares any of the keys being imported — user
/// global data is never silently overwritten. The refusal is returned as
/// [`ImportOrchestrationOutcome::Conflict`] naming the colliding keys; the
/// caller surfaces the message.
///
/// The global write completes first, then the project-side removal. If the
/// project removal fails after the global write succeeded, the imported data
/// is already safe in the global layer and the error says so; the project
/// file keeps the (load-ignored) keys and a retry reports
/// [`ImportOrchestrationOutcome::Conflict`] naming them — the user must
/// delete them from the project file manually, which is the documented
/// recovery rather than a silent re-motion.
///
/// A missing global file is created (minimal `schema_version` document, like
/// the other merge seams); a missing project file and either an unparseable
/// document are errors. `NothingToImport` is returned when the project file
/// declares none of the global-only keys.
pub fn import_project_orchestration_to_global(
    project_path: &Path,
    global_path: &Path,
) -> Result<ImportOrchestrationOutcome, ConfigError> {
    let mut lock = crate::shell_security::config_lock(global_path)?;
    let _guard = lock.write().map_err(|e| ConfigError::Load(e.to_string()))?;
    let project_raw = fs::read_to_string(project_path).map_err(|e| {
        ConfigError::Load(format!("failed to read {}: {e}", project_path.display()))
    })?;
    let project_doc = project_raw.parse::<toml_edit::DocumentMut>().map_err(|e| {
        ConfigError::Load(format!("failed to parse {}: {e}", project_path.display()))
    })?;

    // Deep-copy the declared items out of the project document before any
    // mutation, so the global write never depends on project-side state.
    let mut copies: Vec<(&'static str, toml_edit::Item)> = Vec::new();
    if let Some(item) = project_doc.as_table().get("orchestration") {
        copies.push(("orchestration", item.clone()));
    }
    if let Some(multi_agent) =
        project_doc.as_table().get("multi_agent").and_then(|item| item.as_table())
    {
        for key in ["custom_agents", "model_pins"] {
            if let Some(item) = multi_agent.get(key) {
                copies.push((key, item.clone()));
            }
        }
    }
    if copies.is_empty() {
        return Ok(ImportOrchestrationOutcome::NothingToImport);
    }

    // Build the global document resolved to write. Read it only when it
    // exists — the conflict check needs it anyway.
    let mut global_doc = match fs::read_to_string(global_path) {
        Ok(global_raw) => global_raw.parse::<toml_edit::DocumentMut>().map_err(|e| {
            ConfigError::Load(format!("failed to parse {}: {e}", global_path.display()))
        })?,
        Err(_) if !global_path.exists() => {
            let mut doc = toml_edit::DocumentMut::new();
            doc.insert("schema_version", toml_edit::value(crate::schema::SCHEMA_VERSION as i64));
            doc
        }
        Err(error) => {
            return Err(ConfigError::Load(format!(
                "failed to read {}: {error}",
                global_path.display()
            )));
        }
    };

    // Conflict guard: refuse when the global file already declares any of
    // the keys being imported (never silently overwrite user global data).
    let conflicting = global_only_keys_declared_in(global_doc.as_table());
    if !conflicting.is_empty() {
        return Ok(ImportOrchestrationOutcome::Conflict { keys: conflicting });
    }

    // Copy into the global document. `orchestration` is inserted wholesale;
    // the roster/pins keys slot into the global `[multi_agent]` table
    // (created when the global config has none yet).
    for (key, item) in &copies {
        if *key == "orchestration" {
            global_doc.as_table_mut().insert("orchestration", item.clone());
        } else {
            let global_multi_agent = ensure_table_mut(global_doc.as_table_mut(), "multi_agent")
                .map_err(|message| {
                    ConfigError::Load(format!(
                        "cannot import '[multi_agent.{key}]' into {}: {message}",
                        global_path.display()
                    ))
                })?;
            global_multi_agent.insert(key, item.clone());
        }
    }
    atomic_write(global_path, global_doc.to_string().as_bytes()).map_err(|error| {
        ConfigError::Load(format!("import aborted — the project file is untouched: {error}"))
    })?;

    // The global layer now owns the data; remove the keys from the project
    // file (atomic). A failure here is reported with recovery guidance — the
    // keys are load-ignored, so config behavior is already consistent.
    let declared: Vec<String> = copies
        .iter()
        .map(|(key, _)| match *key {
            "orchestration" => "orchestration",
            "custom_agents" => "multi_agent.custom_agents",
            _ => "multi_agent.model_pins",
        })
        .map(str::to_string)
        .collect();
    if let Err(error) = remove_project_orchestration_keys(project_path) {
        return Err(ConfigError::Load(format!(
            "imported {cpp} into the global config but failed to remove them from {}: {error}; \
             delete the orchestration keys from the project file manually — they are ignored \
             at load",
            project_path.display(),
            cpp = declared.join(", ")
        )));
    }
    tracing::warn!(
        keys = %declared.join(", "),
        project = %project_path.display(),
        global = %global_path.display(),
        "orchestration keys imported into the global config and removed from the project config"
    );
    Ok(ImportOrchestrationOutcome::Imported { imported: declared })
}

/// Remove the global-only orchestration keys from the TOML document at
/// `path`, in place (merge-aware, atomic), preserving every other key,
/// comment, and key order. Used by the explicit import action only — the
/// load path never writes files.
pub fn remove_project_orchestration_keys(path: &Path) -> Result<(), ConfigError> {
    let mut lock = crate::shell_security::config_lock(path)?;
    let _guard = lock.write().map_err(|e| ConfigError::Load(e.to_string()))?;
    let raw = fs::read_to_string(path)
        .map_err(|e| ConfigError::Load(format!("failed to read {}: {e}", path.display())))?;
    let mut doc = raw
        .parse::<toml_edit::DocumentMut>()
        .map_err(|e| ConfigError::Load(format!("failed to parse {}: {e}", path.display())))?;
    remove_global_only_orchestration_keys(&mut doc);
    atomic_write(path, doc.to_string().as_bytes())
}

/// What [`import_project_orchestration_to_global`] did:
/// **Imported** names the keys relocated to the global file and removed from
/// the project file; **Conflict** names the keys the GLOBAL file already
/// declares (nothing written anywhere — the user resolves the conflict
/// manually); **NothingToImport** is returned when the project file declares
/// none of the global-only keys.
#[derive(Debug)]
pub enum ImportOrchestrationOutcome {
    Imported { imported: Vec<String> },
    Conflict { keys: Vec<String> },
    NothingToImport,
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- global-only orchestration enforcement (maintainer decision 2026-09) ----

    /// A tiny project-shaped document with a preset.
    fn project_with(sections: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".concerto.toml");
        std::fs::write(&path, sections).unwrap();
        (dir, path)
    }

    /// The raw strip removes exactly the ignored set and preserves everything
    /// else — comments and key order included.
    #[test]
    fn strip_removes_the_global_only_keys_and_preserves_the_rest() {
        let raw = r#"# header comment
schema_version = 7
session_spend_cap_usd = 2.0
[orchestration]
schema_version = 1
[orchestration.blueprint]
name = "tdd"
[multi_agent]
max_concurrent_agents = 3
model_pins = { coder = "local-model" }
[[multi_agent.custom_agents]]
id = "proj-agent"
name = "Proj"
role = "proj-agent"
"#;
        let stripped =
            strip_project_orchestration_keys(raw).expect("parsed").expect("keys to strip");
        assert_eq!(
            stripped.removed_keys,
            vec!["orchestration", "multi_agent.custom_agents", "multi_agent.model_pins"]
        );
        let doc = &stripped.document;
        assert!(doc.contains("# header comment"), "comments survive\n{doc}");
        assert!(doc.contains("session_spend_cap_usd = 2.0"));
        assert!(doc.contains("max_concurrent_agents = 3"));
        assert!(!doc.contains("orchestration"), "the whole table is gone\n{doc}");
        assert!(!doc.contains("proj-agent"), "the roster is gone\n{doc}");
        assert!(!doc.contains("local-model"), "the pins are gone\n{doc}");
        // The stripped document remains valid TOML: it parses.
        assert!(doc.parse::<toml_edit::DocumentMut>().is_ok(), "{doc}");
    }

    /// Nothing to strip → `None`: the documented contract for the
    /// byte-identical no-op load path.
    #[test]
    fn strip_returns_none_when_no_global_only_keys_are_declared() {
        let (dir, path) = project_with("schema_version = 7\nsession_spend_cap_usd = 2.0\n");
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(strip_project_orchestration_keys(&raw).expect("parsed").is_none());
        let _ = dir;
    }

    /// An unparseable project document is an error — the load seam must not
    /// silently merge a broken file (figment used to surface the same shape).
    #[test]
    fn strip_errors_on_an_unparseable_document() {
        assert!(strip_project_orchestration_keys("[unterminated\n").is_err());
    }

    /// The raw declare helper reports exactly the ignored keys present —
    /// individually and combined — and nothing for a missing/unparseable
    /// file or an unrelated document.
    #[test]
    fn declared_keys_reports_each_ignored_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".concerto.toml");

        assert!(declared_project_orchestration_keys(&path).is_empty(), "missing file");

        std::fs::write(&path, "schema_version = 7\n[providers]\nprimary = \"x\"\n").unwrap();
        assert!(declared_project_orchestration_keys(&path).is_empty(), "unrelated file");

        std::fs::write(&path, "schema_version = 7\n[orchestration]\nschema_version = 1\n[multi_agent]\nmodel_pins = {}\n")
            .unwrap();
        assert_eq!(
            declared_project_orchestration_keys(&path),
            vec!["orchestration", "multi_agent.model_pins"]
        );

        std::fs::write(&path, "schema_version = 7\n[[multi_agent.custom_agents]]\nid = \"a\"\n")
            .unwrap();
        assert_eq!(declared_project_orchestration_keys(&path), vec!["multi_agent.custom_agents"]);

        std::fs::write(&path, "[unterminated\n").unwrap();
        assert!(declared_project_orchestration_keys(&path).is_empty(), "unparseable file");
    }

    /// Import round-trip: the declared keys land in the GLOBAL file (the
    /// merged config then resolves them from the global layer), the project
    /// file loses them, and the project's unrelated keys stay untouched.
    #[test]
    fn import_moves_the_keys_to_global_and_off_the_project() {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("config.toml");
        std::fs::write(&global, "schema_version = 7\nsession_spend_cap_usd = 1.0\n").unwrap();
        let project = dir.path().join(".concerto.toml");
        let project_before_keys = r#"# project header
schema_version = 7
session_spend_cap_usd = 2.0
[orchestration]
schema_version = 1
[orchestration.blueprint]
name = "tdd"
[multi_agent]
max_concurrent_agents = 3
model_pins = { coder = "local-model" }
[[multi_agent.custom_agents]]
id = "proj-agent"
name = "Proj"
role = "proj-agent"
"#;
        std::fs::write(&project, project_before_keys).expect("seed project");

        let outcome =
            import_project_orchestration_to_global(&project, &global).expect("import succeeds");
        let imported = match outcome {
            ImportOrchestrationOutcome::Imported { imported } => imported,
            other => panic!("expected Imported, got {other:?}"),
        };
        assert_eq!(
            imported,
            vec!["orchestration", "multi_agent.custom_agents", "multi_agent.model_pins"]
        );

        // The global file gained the keys.
        let gdoc = std::fs::read_to_string(&global).unwrap();
        assert!(gdoc.contains("[orchestration]"), "orchestration moved\n{gdoc}");
        assert!(gdoc.contains("name = \"tdd\""), "the selection moved\n{gdoc}");
        assert!(gdoc.contains("proj-agent"), "the roster moved\n{gdoc}");
        assert!(gdoc.contains("local-model"), "the pins moved\n{gdoc}");
        assert!(gdoc.contains("session_spend_cap_usd = 1.0"), "global data intact\n{gdoc}");

        // The project file lost them and keeps its other content.
        let pdoc = std::fs::read_to_string(&project).unwrap();
        assert!(pdoc.contains("# project header"), "comments survive\n{pdoc}");
        assert!(pdoc.contains("session_spend_cap_usd = 2.0"), "project spend cap intact\n{pdoc}");
        assert!(pdoc.contains("max_concurrent_agents = 3"), "project run limit intact\n{pdoc}");
        assert!(!pdoc.contains("proj-agent"), "roster removed\n{pdoc}");
        assert!(!pdoc.contains("[orchestration]"), "orchestration removed\n{pdoc}");

        // Reload shape: the merged config now resolves orchestration from the
        // global layer (the exact-one seam holds), while the project's
        // non-orchestration keys still apply.
        let cfg = crate::load_config(Some(&global), Some(dir.path()))
            .expect("post-import config must load");
        assert_eq!(
            cfg.orchestration.as_ref().expect("orchestration present").blueprint.name.as_deref(),
            Some("tdd"),
            "the imported selection resolves from the global layer"
        );
        assert_eq!(cfg.session_spend_cap_usd, Some(2.0), "project overrides still apply");
    }

    /// Conflict rule (pinned): the import REFUSES — with the colliding keys
    /// named and NOTHING written to either file — when the global file
    /// already declares any of the keys. User global data is never silently
    /// overwritten.
    #[test]
    fn import_refuses_with_conflicting_global_keys() {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("config.toml");
        let global_before = "schema_version = 7\n[orchestration]\nschema_version = 1\n";
        std::fs::write(&global, global_before).unwrap();
        let (pdir, project) = project_with(
            "schema_version = 7\n[orchestration]\nschema_version = 1\n\
             [orchestration.blueprint]\nname = \"tdd\"\n",
        );
        let project_before = std::fs::read_to_string(&project).unwrap();

        let outcome =
            import_project_orchestration_to_global(&project, &global).expect("no hard error");
        match outcome {
            ImportOrchestrationOutcome::Conflict { keys } => {
                assert_eq!(keys, vec!["orchestration"], "the collision is named");
            }
            other => panic!("expected Conflict, got {other:?}"),
        }
        // Neither file changed.
        assert_eq!(
            std::fs::read_to_string(&global).unwrap(),
            global_before,
            "the global config must stay untouched"
        );
        assert_eq!(
            std::fs::read_to_string(&project).unwrap(),
            project_before,
            "the project config must stay untouched"
        );
        let _ = pdir;
    }
}
