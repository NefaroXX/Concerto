//! Plugin installation safety valve.
//!
//! Deployed `.wasm` files and their sidecar manifests are written to
//! [`crate::discovery::plugins_dir`] only through this module — never by
//! hand-dropping files. Every install goes through the strict pipeline:
//!
//! 1. **Validate** — [`PluginInstaller::validate`] compiles the module on an
//!    async store, extracts and verifies the in-module manifest, checks the
//!    ABI version, verifies a sidecar manifest if one sits next to the source,
//!    enforces the module size cap, and rejects linear memories whose declared
//!    size could exceed the host's memory cap.
//! 2. **Approve** — callers present the validated manifest's required
//!    capabilities to the user for approval *before* any file is written.
//! 3. **Write** — [`PluginInstaller::write`] performs an atomic rename into the
//!    install directory under the plugin id (restricted to a safe character
//!    set, so an id can never escape the directory), copying and reconciling
//!    sidecar manifests.
//!
//! Removal is symmetric via [`delete_plugin_file`].

use std::path::{Path, PathBuf};
use std::sync::Arc;

use concerto_api_types::plugin::PluginManifest;
use wasmtime::{ExternType, Module};

use crate::capability::sha256_hex;
use crate::discovery::find_sidecar_manifest;
use crate::error::PluginError;
use crate::host::PluginHost;
use crate::loader::PluginLoader;

/// A plugin that passed strict validation and is therefore safe to install.
#[derive(Debug)]
pub struct ValidatedPlugin {
    /// Manifest extracted from the validated module (and its sidecar, if any).
    pub manifest: PluginManifest,
    /// SHA-256 of the exact bytes that will be written to disk (ADR-37 hash
    /// pinning against the capability store).
    pub sha256: String,
    /// The validated `.wasm` bytes.
    pub wasm_bytes: Arc<[u8]>,
    /// Source file the module was validated from (may live outside the
    /// install directory).
    pub source: PathBuf,
    /// Effective linear-memory bound in bytes honoured by the memory check.
    pub memory_max_bytes: usize,
}

/// Result of installing a plugin.
#[derive(Debug)]
pub struct InstalledPlugin {
    pub manifest: PluginManifest,
    /// The installed `.wasm` path inside the install directory.
    pub wasm_path: PathBuf,
    /// `true` when the destination already held a `.wasm` for this id and the
    /// install replaced it (callers must revoke the stale hash-pinned grants
    /// before prompting for the new binary — ADR-37).
    pub replaced: bool,
}

/// Installs validated plugins into the canonical plugins directory.
///
/// Holds the shared [`PluginHost`] so validation reuses the same engine and
/// host-function linker the runtime uses.
pub struct PluginInstaller {
    host: Arc<PluginHost>,
}

impl PluginInstaller {
    pub fn new(host: Arc<PluginHost>) -> Self {
        Self { host }
    }

    /// Strictly validate a `.wasm` file for installation.
    ///
    /// Compiles the module, extracts the manifest through the `manifest`
    /// export (the same pipeline the loader uses at run time), verifies any
    /// sidecar next to the source, checks the ABI version, and enforces the
    /// module-size and linear-memory caps. Does not write anything.
    pub async fn validate(&self, source: &Path) -> Result<ValidatedPlugin, PluginError> {
        match source.extension().map(|e| e.to_string_lossy()) {
            Some(ext) if ext == "wasm" => {}
            _ => {
                return Err(PluginError::InvalidManifest(
                    "only .wasm plugin files can be installed".into(),
                ));
            }
        }
        let wasm_bytes: Arc<[u8]> = Arc::from(std::fs::read(source)?);
        // Size check before compiling so a huge file never reaches the engine.
        if wasm_bytes.len() > PluginHost::MAX_WASM_MODULE_SIZE {
            return Err(PluginError::InvalidManifest("module too large".into()));
        }
        let loader = PluginLoader::new(self.host.clone());
        let loaded = loader.load_from_bytes(&wasm_bytes, source).await?;
        let memory_max_bytes = reject_oversized_memory(&loaded.module)? as usize;
        let sha256 = sha256_hex(&wasm_bytes);
        Ok(ValidatedPlugin {
            manifest: loaded.manifest,
            sha256,
            wasm_bytes,
            source: source.to_owned(),
            memory_max_bytes,
        })
    }

    /// Atomically write a validated plugin into `dest_dir`.
    ///
    /// The destination file is `<dest_dir>/<id>.wasm`, written via a
    /// same-directory temp file and rename so a crash can never leave a
    /// truncated module that discovery would pick up. Sidecar manifests are
    /// copied from the source (`<id>.manifest.json` preferred, `<id>.toml`
    /// legacy) and stale sidecars of a previous install are removed so a
    /// replace can never mismatch against the old manifest.
    pub fn write(
        &self,
        plugin: &ValidatedPlugin,
        dest_dir: &Path,
    ) -> Result<InstalledPlugin, PluginError> {
        let id = &plugin.manifest.id;
        if !is_safe_plugin_id(id) {
            return Err(PluginError::InvalidManifest(format!(
                "plugin id {id:?} contains characters outside the safe set [A-Za-z0-9._-]"
            )));
        }
        std::fs::create_dir_all(dest_dir)?;

        // Sidecar handling: discovered names are `<id>.manifest.json`
        // (preferred) and `<id>.toml` (legacy) — see `find_sidecar_manifest`.
        // The dest name is resolved here so stale cleanup compares like for
        // like.
        let sidecar: Option<(String, Vec<u8>)> = match find_sidecar_manifest(&plugin.source) {
            Some(path) => match path.extension().map(|e| e.to_string_lossy().into_owned()) {
                Some(ext) if ext == "json" => {
                    Some((format!("{id}.manifest.json"), std::fs::read(&path)?))
                }
                Some(ext) if ext == "toml" => Some((format!("{id}.toml"), std::fs::read(&path)?)),
                _ => None,
            },
            None => None,
        };

        let wasm_dest = dest_dir.join(format!("{id}.wasm"));
        let replaced = wasm_dest.exists();

        // Remove sidecars that are not being rewritten on this install so a
        // replaced plugin never carries the previous binary's manifest.
        let sidecar_name = sidecar.as_ref().map(|(name, _)| name.clone());
        for ext in ["manifest.json", "toml"] {
            let keep = sidecar_name.as_ref().is_some_and(|name| name == &format!("{id}.{ext}"));
            if !keep {
                let stale = dest_dir.join(format!("{id}.{ext}"));
                if stale.exists() {
                    std::fs::remove_file(&stale)?;
                }
            }
        }

        let tmp = dest_dir.join(format!(".{id}.install-{}.tmp", std::process::id()));
        let result = (|| -> Result<InstalledPlugin, PluginError> {
            std::fs::write(&tmp, plugin.wasm_bytes.as_ref())?;
            match std::fs::rename(&tmp, &wasm_dest) {
                Ok(()) => {}
                Err(first) => {
                    // Unix renames over an existing file; Windows does not.
                    // Retry once after removing the destination.
                    if wasm_dest.exists() {
                        std::fs::remove_file(&wasm_dest)?;
                        std::fs::rename(&tmp, &wasm_dest)?;
                    } else {
                        return Err(first.into());
                    }
                }
            }
            if let Some((name, bytes)) = sidecar {
                std::fs::write(dest_dir.join(name), bytes)?;
            }
            Ok(InstalledPlugin {
                manifest: plugin.manifest.clone(),
                wasm_path: wasm_dest,
                replaced,
            })
        })();
        if result.is_err() && tmp.exists() {
            let _ = std::fs::remove_file(&tmp);
        }
        result
    }
}

/// Remove an installed plugin's `.wasm` and its sidecar manifests.
///
/// Tolerates missing files so delete is idempotent (the file may already have
/// been removed by a previous attempt). The plugin's capability grants are
/// NOT touched here — callers revoke them through the capability manager so
/// the persisted store stays consistent with the filesystem.
pub fn delete_plugin_file(wasm_path: &Path) -> Result<(), PluginError> {
    let stem = wasm_path.file_stem().ok_or_else(|| {
        PluginError::InvalidManifest(format!("invalid plugin path {}", wasm_path.display()))
    })?;
    let dir = wasm_path.parent().ok_or_else(|| {
        PluginError::InvalidManifest(format!("invalid plugin path {}", wasm_path.display()))
    })?;
    if wasm_path.exists() {
        std::fs::remove_file(wasm_path)?;
    }
    // Sidecar names mirror `find_sidecar_manifest`: `{stem}.manifest.json`
    // (preferred) and `{stem}.toml` (legacy).
    for ext in ["manifest.json", "toml"] {
        let sidecar = dir.join(format!("{}.{ext}", stem.to_string_lossy()));
        if sidecar.exists() {
            std::fs::remove_file(sidecar)?;
        }
    }
    Ok(())
}

/// Reject modules whose linear memory could exceed the host memory cap.
///
/// The engine allows dynamic memories to grow beyond
/// [`PluginHost::DEFAULT_MAX_MEMORY`]; this structural check keeps deployed
/// plugins bounded at install time. A declared maximum counts over the
/// fallback to the module's declared minimum when no maximum is given (the
/// module then starts at that size). Returns the effective bound in bytes.
fn reject_oversized_memory(module: &Module) -> Result<u64, PluginError> {
    let limit = PluginHost::DEFAULT_MAX_MEMORY as u64;
    for export in module.exports() {
        if export.name() != "memory" {
            continue;
        }
        let ExternType::Memory(mem) = export.ty() else {
            // An export named "memory" that is not a memory is harmless.
            continue;
        };
        let page_size = mem.page_size();
        let min_bytes = mem
            .minimum()
            .checked_mul(page_size)
            .ok_or_else(|| PluginError::InvalidManifest("module memory size overflow".into()))?;
        if min_bytes > limit {
            return Err(PluginError::InvalidManifest(format!(
                "module memory minimum {min_bytes} bytes exceeds the {} byte cap",
                PluginHost::DEFAULT_MAX_MEMORY
            )));
        }
        let bound_bytes = match mem.maximum() {
            Some(max_pages) => {
                let bytes = max_pages.checked_mul(page_size).ok_or_else(|| {
                    PluginError::InvalidManifest("module memory size overflow".into())
                })?;
                if bytes > limit {
                    return Err(PluginError::InvalidManifest(format!(
                        "module memory maximum {bytes} bytes exceeds the {} byte cap",
                        PluginHost::DEFAULT_MAX_MEMORY
                    )));
                }
                bytes
            }
            None => min_bytes,
        };
        return Ok(bound_bytes);
    }
    // A module without an explicit memory export is fine (the loader checks
    // for the scratch scaffolding at initialisation).
    Ok(0)
}

/// Plugin ids become filenames; only a strict safe set may be used so an id
/// can never escape the plugins directory.
fn is_safe_plugin_id(id: &str) -> bool {
    !id.is_empty()
        && id != "."
        && id != ".."
        && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-')
}

#[cfg(test)]
mod tests {
    use super::*;
    use concerto_api_types::plugin::PluginManifest;
    use std::time::Instant;

    fn install_dir() -> tempfile::TempDir {
        tempfile::tempdir().expect("temp dir should be created")
    }

    /// Build a minimal valid tool plugin module with the given in-module
    /// manifest id and an optional memory declaration.
    fn plugin_wat(id: &str, memory: &str) -> String {
        let manifest = format!(
            "{{\"id\":\"{id}\",\"name\":\"{id}\",\"version\":\"0.1.0\",\"description\":\"{id}\",\"abi_version\":1,\"capabilities_required\":[],\"provides\":[]}}"
        );
        // Quotes must be escaped for the WAT string literal.
        let escaped = manifest.replace('"', r#"\""#);
        format!(
            r#"(module
  {memory}
  (global (export "scratch_buffer") (mut i32) (i32.const 0))
  (global (export "scratch_buffer_size") i32 (i32.const 65536))
  (data (i32.const 256) "{escaped}")
  (func (export "manifest") (result i64)
    (i64.or (i64.shl (i64.const 256) (i64.const 32)) (i64.const {len})))
  (func (export "init") (result i32) i32.const 0)
)"#,
            len = manifest.len(),
        )
    }

    fn write_wasm(dir: &std::path::Path, name: &str, id: &str) -> std::path::PathBuf {
        let wat = plugin_wat(id, r#"(memory (export "memory") 2)"#);
        let bytes = wat::parse_str(&wat).expect("WAT should compile");
        let path = dir.join(name);
        std::fs::write(&path, bytes).expect("wasm should be written");
        path
    }

    /// A sidecar manifest exactly matching the manifest embedded by
    /// `plugin_wat` (id/name/description all equal, version 0.1.0, no
    /// capabilities, no provides) — anything else trips ManifestMismatch.
    fn exact_manifest_json(id: &str) -> String {
        serde_json::to_string(&PluginManifest {
            id: id.to_string(),
            version: "0.1.0".to_string(),
            name: id.to_string(),
            description: id.to_string(),
            abi_version: 1,
            capabilities_required: vec![],
            provides: vec![],
        })
        .expect("manifest json")
    }

    #[tokio::test]
    async fn validate_accepts_a_valid_plugin() {
        let tmp = install_dir();
        let source = write_wasm(tmp.path(), "good.wasm", "good-plugin");
        let installer = PluginInstaller::new(Arc::new(PluginHost::new().expect("host")));
        let validated = installer.validate(&source).await.expect("validate should pass");
        assert_eq!(validated.manifest.id, "good-plugin");
        assert_eq!(validated.sha256.len(), 64, "sha256 is hex");
        assert!(!validated.wasm_bytes.is_empty());
    }

    #[tokio::test]
    async fn validate_rejects_non_wasm_extension() {
        let tmp = install_dir();
        let path = tmp.path().join("notes.txt");
        std::fs::write(&path, b"not a plugin").expect("file should be written");
        let installer = PluginInstaller::new(Arc::new(PluginHost::new().expect("host")));
        let err = installer.validate(&path).await.expect_err("extension gate should reject");
        assert!(matches!(err, PluginError::InvalidManifest(_)));
    }

    #[tokio::test]
    async fn validate_rejects_oversized_module() {
        let tmp = install_dir();
        let path = tmp.path().join("big.wasm");
        std::fs::write(&path, vec![0u8; PluginHost::MAX_WASM_MODULE_SIZE + 1])
            .expect("file should be written");
        let installer = PluginInstaller::new(Arc::new(PluginHost::new().expect("host")));
        let err = installer.validate(&path).await.expect_err("size gate should reject");
        assert!(matches!(err, PluginError::InvalidManifest(_)));
    }

    #[tokio::test]
    async fn validate_rejects_memory_max_over_cap() {
        let tmp = install_dir();
        let wat = plugin_wat("big-mem", r#"(memory (export "memory") 1 4096)"#);
        let bytes = wat::parse_str(&wat).expect("WAT should compile");
        let path = tmp.path().join("big-mem.wasm");
        std::fs::write(&path, bytes).expect("wasm should be written");
        let installer = PluginInstaller::new(Arc::new(PluginHost::new().expect("host")));
        let err = installer.validate(&path).await.expect_err("memory cap should reject");
        assert!(matches!(err, PluginError::InvalidManifest(_)));
    }

    #[tokio::test]
    async fn validate_rejects_memory_min_over_cap() {
        let tmp = install_dir();
        let wat = plugin_wat("huge-min", r#"(memory (export "memory") 2000)"#);
        let bytes = wat::parse_str(&wat).expect("WAT should compile");
        let path = tmp.path().join("huge-min.wasm");
        std::fs::write(&path, bytes).expect("wasm should be written");
        let installer = PluginInstaller::new(Arc::new(PluginHost::new().expect("host")));
        let err = installer.validate(&path).await.expect_err("memory min should reject");
        assert!(matches!(err, PluginError::InvalidManifest(_)));
    }

    #[tokio::test]
    async fn validate_records_effective_memory_bound() {
        let tmp = install_dir();
        let wat = plugin_wat("bounds", r#"(memory (export "memory") 1 1024)"#);
        let bytes = wat::parse_str(&wat).expect("WAT should compile");
        let path = tmp.path().join("bounds.wasm");
        std::fs::write(&path, bytes).expect("wasm should be written");
        let installer = PluginInstaller::new(Arc::new(PluginHost::new().expect("host")));
        let validated = installer.validate(&path).await.expect("validate should pass");
        // 1024 pages * 64 KiB = exactly the host cap.
        assert_eq!(validated.memory_max_bytes, PluginHost::DEFAULT_MAX_MEMORY);
    }

    #[test]
    fn safe_id_gate() {
        assert!(is_safe_plugin_id("my-plugin.v2_0"));
        assert!(!is_safe_plugin_id(""));
        assert!(!is_safe_plugin_id("."));
        assert!(!is_safe_plugin_id(".."));
        assert!(!is_safe_plugin_id("../evil"));
        assert!(!is_safe_plugin_id("a/b"));
        assert!(!is_safe_plugin_id("a\\b"));
        assert!(!is_safe_plugin_id("sp ace"));
    }

    #[test]
    fn write_installs_atomically_with_id_named_file() {
        let tmp = install_dir();
        let source_dir = tempfile::tempdir().expect("source dir");
        let source = write_wasm(source_dir.path(), "src.wasm", "installed-1");
        let installer = PluginInstaller::new(Arc::new(PluginHost::new().expect("host")));
        let rt =
            tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
        let validated = rt.block_on(installer.validate(&source)).expect("validate");
        let installed = installer.write(&validated, tmp.path()).expect("install");
        assert!(!installed.replaced, "first install is not a replace");
        assert_eq!(installed.wasm_path, tmp.path().join("installed-1.wasm"));
        assert!(installed.wasm_path.exists());
        // No stray temp file left behind.
        let leftovers: Vec<_> = std::fs::read_dir(tmp.path())
            .expect("dir")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".install-") && n.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp files must be cleaned up");
    }

    #[test]
    fn write_copies_json_sidecar_with_discovered_name() {
        let tmp = install_dir();
        let source_dir = tempfile::tempdir().expect("source dir");
        let source = write_wasm(source_dir.path(), "src.wasm", "sidecar-json");
        let sidecar = source.with_extension("manifest.json");
        std::fs::write(&sidecar, exact_manifest_json("sidecar-json"))
            .expect("sidecar should be written");

        let installer = PluginInstaller::new(Arc::new(PluginHost::new().expect("host")));
        let rt =
            tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
        let validated = rt.block_on(installer.validate(&source)).expect("validate");
        installer.write(&validated, tmp.path()).expect("install");

        let dest_wasm = tmp.path().join("sidecar-json.wasm");
        let dest_sidecar =
            find_sidecar_manifest(&dest_wasm).expect("installed sidecar must be discoverable");
        assert!(dest_sidecar.ends_with("sidecar-json.manifest.json"));
    }

    #[test]
    fn write_copies_toml_legacy_sidecar() {
        let tmp = install_dir();
        let source_dir = tempfile::tempdir().expect("source dir");
        let source = write_wasm(source_dir.path(), "src.wasm", "sidecar-toml");
        let manifest = "id = \"sidecar-toml\"\nversion = \"0.1.0\"\nname = \"sidecar-toml\"\ndescription = \"sidecar-toml\"\nabi_version = 1\ncapabilities_required = []\nprovides = []\n".to_string();
        let sidecar = source.with_extension("toml");
        std::fs::write(&sidecar, manifest).expect("sidecar should be written");

        let installer = PluginInstaller::new(Arc::new(PluginHost::new().expect("host")));
        let rt =
            tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
        let validated = rt.block_on(installer.validate(&source)).expect("validate");
        installer.write(&validated, tmp.path()).expect("install");

        let dest_wasm = tmp.path().join("sidecar-toml.wasm");
        let dest_sidecar =
            find_sidecar_manifest(&dest_wasm).expect("installed sidecar must be discoverable");
        assert!(dest_sidecar.ends_with("sidecar-toml.toml"));
    }

    #[test]
    fn write_replaces_and_cleans_stale_sidecar() {
        let tmp = install_dir();
        let source_dir = tempfile::tempdir().expect("source dir");
        let installer = PluginInstaller::new(Arc::new(PluginHost::new().expect("host")));

        // First install: TOML legacy sidecar.
        let source_toml = write_wasm(source_dir.path(), "a.wasm", "replace-me");
        let toml_manifest = "id = \"replace-me\"\nversion = \"0.1.0\"\nname = \"replace-me\"\ndescription = \"replace-me\"\nabi_version = 1\ncapabilities_required = []\nprovides = []\n".to_string();
        std::fs::write(source_toml.with_extension("toml"), toml_manifest)
            .expect("sidecar should be written");
        let rt =
            tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
        let first = rt.block_on(installer.validate(&source_toml)).expect("validate");
        let installed = installer.write(&first, tmp.path()).expect("install");
        assert!(!installed.replaced);
        assert!(tmp.path().join("replace-me.toml").exists());

        // Second install: same id with a JSON-manifest sidecar, from another
        // source file.
        let source_json = write_wasm(source_dir.path(), "b.wasm", "replace-me");
        std::fs::write(
            source_json.with_extension("manifest.json"),
            exact_manifest_json("replace-me"),
        )
        .expect("sidecar should be written");
        let second = rt.block_on(installer.validate(&source_json)).expect("validate");
        let installed = installer.write(&second, tmp.path()).expect("replace");
        assert!(installed.replaced, "second install replaces the first");
        assert!(
            !tmp.path().join("replace-me.toml").exists(),
            "stale TOML sidecar must be cleaned up"
        );
        assert!(find_sidecar_manifest(&tmp.path().join("replace-me.wasm"))
            .expect("json sidecar present")
            .ends_with("replace-me.manifest.json"));
    }

    #[test]
    fn write_rejects_unsafe_id() {
        let tmp = install_dir();
        let source_dir = tempfile::tempdir().expect("source dir");
        let installer = PluginInstaller::new(Arc::new(PluginHost::new().expect("host")));
        let rt =
            tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
        // A valid module whose manifest declares a traversal id.
        let wat = plugin_wat("../../evil", r#"(memory (export "memory") 2)"#);
        let bytes = wat::parse_str(&wat).expect("WAT should compile");
        let source = source_dir.path().join("evil.wasm");
        std::fs::write(&source, bytes).expect("wasm should be written");
        let validated = rt.block_on(installer.validate(&source)).expect("validate");
        let err = installer.write(&validated, tmp.path()).expect_err("unsafe id must be rejected");
        assert!(matches!(err, PluginError::InvalidManifest(_)));
    }

    #[test]
    fn write_replaces_without_sidecar_removes_stale_sidecar() {
        let tmp = install_dir();
        let source_dir = tempfile::tempdir().expect("source dir");
        let installer = PluginInstaller::new(Arc::new(PluginHost::new().expect("host")));
        let rt =
            tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");

        let with_sidecar = write_wasm(source_dir.path(), "a.wasm", "cleanup-me");
        std::fs::write(
            with_sidecar.with_extension("manifest.json"),
            exact_manifest_json("cleanup-me"),
        )
        .expect("sidecar should be written");
        let first = rt.block_on(installer.validate(&with_sidecar)).expect("validate");
        installer.write(&first, tmp.path()).expect("install");
        assert!(tmp.path().join("cleanup-me.manifest.json").exists());

        // Reinstall the same id from a source that has no sidecar.
        let bare = write_wasm(source_dir.path(), "b.wasm", "cleanup-me");
        let second = rt.block_on(installer.validate(&bare)).expect("validate");
        installer.write(&second, tmp.path()).expect("replace");
        assert!(
            !tmp.path().join("cleanup-me.manifest.json").exists(),
            "sidecar of the replaced install must be removed"
        );
        assert!(find_sidecar_manifest(&tmp.path().join("cleanup-me.wasm")).is_none());
    }

    #[test]
    fn delete_removes_wasm_and_sidecars() {
        let tmp = install_dir();
        let source_dir = tempfile::tempdir().expect("source dir");
        let installer = PluginInstaller::new(Arc::new(PluginHost::new().expect("host")));
        let rt =
            tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
        let source = write_wasm(source_dir.path(), "src.wasm", "delete-me");
        std::fs::write(source.with_extension("manifest.json"), exact_manifest_json("delete-me"))
            .expect("sidecar should be written");
        let validated = rt.block_on(installer.validate(&source)).expect("validate");
        let installed = installer.write(&validated, tmp.path()).expect("install");

        delete_plugin_file(&installed.wasm_path).expect("delete should succeed");
        assert!(!installed.wasm_path.exists());
        assert!(!tmp.path().join("delete-me.manifest.json").exists());
        assert!(!tmp.path().join("delete-me.toml").exists());
    }

    #[test]
    fn delete_tolerates_missing_wasm() {
        let tmp = install_dir();
        let missing = tmp.path().join("gone.wasm");
        // Must not error when the file is already gone (idempotent delete).
        delete_plugin_file(&missing).expect("missing delete should be a no-op");
    }

    #[test]
    fn validate_is_bounded_wall_clock_for_bad_modules() {
        // Regression guard: validating arbitrary user-chosen files must never
        // hang. A module whose `manifest` spins is trapped by the store's fuel
        // budget / epoch deadline during extraction.
        let wat = r#"(module
            (memory (export "memory") 1)
            (func (export "manifest") (result i64)
              (loop (br 0)))
        )"#;
        let bytes = wat::parse_str(wat).expect("WAT should compile");
        let tmp = install_dir();
        let path = tmp.path().join("spinner.wasm");
        std::fs::write(&path, bytes).expect("wasm should be written");
        let installer = PluginInstaller::new(Arc::new(PluginHost::new().expect("host")));
        let rt =
            tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
        let start = Instant::now();
        let result = rt.block_on(installer.validate(&path));
        assert!(result.is_err(), "a spinning manifest export must fail validation");
        assert!(
            start.elapsed() < std::time::Duration::from_secs(10),
            "validation of a hostile module must terminate promptly"
        );
    }
}
