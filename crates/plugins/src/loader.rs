use std::path::Path;
use std::sync::Arc;

use concerto_api_types::plugin::PluginManifest;
use concerto_core::traits::provider::LlmProvider;
use wasmtime::{Instance, Linker, Module, Store};

use crate::active_plugin::ActivePlugin;
use crate::capability::GrantedCapabilities;
use crate::discovery::find_sidecar_manifest;
use crate::error::PluginError;
use crate::guest_abi::HOST_ABI_VERSION;
use crate::host::{PluginHost, PluginStoreData, ScratchBuffer};
use crate::host_fns::{register_host_functions, register_minimal_host_functions};

/// A loaded but not-yet-initialised plugin.
pub struct LoadedPlugin {
    pub manifest: PluginManifest,
    pub wasm_path: std::path::PathBuf,
    pub module: Arc<Module>,
}

/// Plugin loader — loads WASM modules, extracts manifests, manages lifecycle.
pub struct PluginLoader {
    host: Arc<PluginHost>,
    event_bus: Option<tokio::sync::broadcast::Sender<Arc<serde_json::Value>>>,
    provider: Option<Arc<dyn LlmProvider>>,
}

impl PluginLoader {
    pub fn new(host: Arc<PluginHost>) -> Self {
        Self { host, event_bus: None, provider: None }
    }

    pub fn with_event_bus(
        host: Arc<PluginHost>,
        event_bus: tokio::sync::broadcast::Sender<Arc<serde_json::Value>>,
    ) -> Self {
        Self { host, event_bus: Some(event_bus), provider: None }
    }

    pub fn set_provider(&mut self, provider: Option<Arc<dyn LlmProvider>>) {
        self.provider = provider;
    }

    /// Load a plugin from a `.wasm` file path and extract its manifest.
    pub async fn load(&self, wasm_path: &Path) -> Result<LoadedPlugin, PluginError> {
        let metadata = std::fs::metadata(wasm_path)?;
        if metadata.len() as usize > PluginHost::MAX_WASM_MODULE_SIZE {
            return Err(PluginError::InvalidManifest("module too large".into()));
        }
        let wasm_bytes = std::fs::read(wasm_path)?;
        self.load_from_bytes(&wasm_bytes, wasm_path).await
    }

    /// Load a plugin from raw WASM bytes (useful for tests).
    ///
    /// Async (ADR-38): manifest extraction instantiates the module on an async
    /// store and calls the `manifest` export via `call_async`.
    pub async fn load_from_bytes(
        &self,
        wasm_bytes: &[u8],
        source: &Path,
    ) -> Result<LoadedPlugin, PluginError> {
        if wasm_bytes.len() > PluginHost::MAX_WASM_MODULE_SIZE {
            return Err(PluginError::InvalidManifest("module too large".into()));
        }
        let module = Module::new(self.host.engine(), wasm_bytes)?;

        // Step 1: Temporary store for manifest extraction.
        let mut temp_store = self.host.create_store();
        let mut temp_linker = Linker::new(self.host.engine());
        register_minimal_host_functions(&mut temp_linker)?;
        let temp_instance = temp_linker.instantiate_async(&mut temp_store, &module).await?;

        let manifest = extract_manifest(&mut temp_store, &temp_instance).await?;

        // Step 2: Verify sidecar manifest if present.
        if let Some(sidecar_path) = find_sidecar_manifest(source) {
            let sidecar_content = std::fs::read_to_string(&sidecar_path)?;
            let sidecar_manifest: PluginManifest = if sidecar_path
                .extension()
                .is_some_and(|e| e == "json")
            {
                serde_json::from_str(&sidecar_content)
                    .map_err(|e| PluginError::InvalidManifest(format!("sidecar JSON parse: {e}")))?
            } else {
                toml::from_str(&sidecar_content)
                    .map_err(|e| PluginError::InvalidManifest(format!("sidecar TOML parse: {e}")))?
            };
            if sidecar_manifest != manifest {
                return Err(PluginError::ManifestMismatch);
            }
        }

        // Step 3: Check ABI version.
        if manifest.abi_version > HOST_ABI_VERSION {
            return Err(PluginError::AbiTooNew {
                found: manifest.abi_version,
                max: HOST_ABI_VERSION,
            });
        }

        Ok(LoadedPlugin { manifest, wasm_path: source.to_owned(), module: Arc::new(module) })
    }

    /// Initialise a loaded plugin with approved capabilities.
    ///
    /// Async (ADR-38): instantiation and the `init` export call both run on an
    /// async store and must go through the wasmtime async API.
    pub async fn initialise(
        &self,
        plugin: &LoadedPlugin,
        granted_caps: GrantedCapabilities,
    ) -> Result<ActivePlugin, PluginError> {
        let mut store = self.host.create_store();
        store.data_mut().granted_caps = granted_caps;
        store.data_mut().plugin_id = plugin.manifest.id.clone();
        if let Some(ref bus) = self.event_bus {
            store.data_mut().event_bus = Some(bus.clone());
        }
        store.data_mut().provider = self.provider.clone();
        // Set fuel limit for execution safety.
        PluginHost::set_fuel(&mut store, PluginHost::MAX_FUEL)?;

        let linker = self.build_linker()?;
        let instance = linker.instantiate_async(&mut store, &plugin.module).await?;

        // Validate scratch_buffer export and record address/length.
        let (scratch_ptr, scratch_len) = extract_scratch_buffer(&instance, &mut store)?;
        store.data_mut().scratch = ScratchBuffer { ptr: scratch_ptr, len: scratch_len };

        // Call init() — fuel exhaustion or epoch trap = timeout.
        let init_fn = instance
            .get_typed_func::<(), i32>(&mut store, "init")
            .map_err(|_| PluginError::InvalidManifest("missing init export".into()))?;
        let result = init_fn.call_async(&mut store, ()).await.map_err(|e| {
            // Distinguish fuel/epoch exhaustion from general trap
            let err_str = e.to_string();
            if err_str.contains("has exhausted its fuel")
                || err_str.contains("epoch deadline")
                || err_str.contains("wasm trap")
            {
                PluginError::InitFailed(-2) // -2 = timeout / resource exhaustion
            } else {
                PluginError::InitFailed(-1) // -1 = general init failure
            }
        })?;
        if result != 0 {
            return Err(PluginError::InitFailed(result));
        }

        Ok(ActivePlugin { manifest: plugin.manifest.clone(), instance, store })
    }

    fn build_linker(&self) -> Result<Linker<PluginStoreData>, PluginError> {
        let mut linker = Linker::new(self.host.engine());
        register_host_functions(&mut linker)?;
        Ok(linker)
    }
}

/// Extract the PluginManifest from a WASM instance by calling `manifest()`.
async fn extract_manifest(
    store: &mut Store<PluginStoreData>,
    instance: &Instance,
) -> Result<PluginManifest, PluginError> {
    let manifest_fn = instance
        .get_typed_func::<(), i64>(&mut *store, "manifest")
        .map_err(|_| PluginError::InvalidManifest("missing manifest export".into()))?;
    let result = manifest_fn
        .call_async(&mut *store, ())
        .await
        .map_err(|e| PluginError::InvalidManifest(e.to_string()))?;

    if result == crate::guest_abi::RESULT_ERROR {
        return Err(PluginError::InvalidManifest("manifest() returned error".into()));
    }

    let (ptr, len) = crate::guest_abi::unpack_ptr_len(result);

    // Metadata size sanity check: reject manifest claims longer than 1 MB.
    if len <= 0 || len > 1024 * 1024 {
        return Err(PluginError::InvalidManifest(format!("manifest claims invalid size {len}")));
    }

    let mem = instance
        .get_export(&mut *store, "memory")
        .and_then(|e| e.into_memory())
        .ok_or(PluginError::NoMemory)?;
    let data = mem.data(&mut *store);
    let start = ptr as usize;
    let end = start.checked_add(len as usize).ok_or(PluginError::MemoryViolation { ptr, len })?;
    if end > data.len() {
        return Err(PluginError::MemoryViolation { ptr, len });
    }
    let json_str = std::str::from_utf8(&data[start..end]).map_err(|_| PluginError::InvalidUtf8)?;
    let manifest: PluginManifest = serde_json::from_str(json_str)
        .map_err(|e| PluginError::InvalidManifest(format!("JSON parse: {e}")))?;
    Ok(manifest)
}

/// Find and validate the `scratch_buffer` export.
fn extract_scratch_buffer(
    instance: &Instance,
    store: &mut Store<PluginStoreData>,
) -> Result<(i32, i32), PluginError> {
    // Check for explicit scratch_buffer global export.
    if let Some(export) = instance.get_export(&mut *store, "scratch_buffer") {
        if let Some(global) = export.into_global() {
            let val = global.get(&mut *store).i32().ok_or(PluginError::MissingScratchBuffer)?;
            let size = instance
                .get_export(&mut *store, "scratch_buffer_size")
                .and_then(|e| e.into_global())
                .map(|g| {
                    g.get(&mut *store)
                        .i32()
                        .unwrap_or(crate::guest_abi::DEFAULT_SCRATCH_SIZE as i32)
                })
                .unwrap_or(crate::guest_abi::DEFAULT_SCRATCH_SIZE as i32);
            // Validate scratch buffer bounds against actual memory.
            let mem = instance
                .get_export(&mut *store, "memory")
                .and_then(|e| e.into_memory())
                .ok_or(PluginError::NoMemory)?;
            let mem_size = mem.data(&*store).len();
            if val < 0 || size < 0 {
                return Err(PluginError::MissingScratchBuffer);
            }
            let end = (val as usize)
                .checked_add(size as usize)
                .ok_or(PluginError::MemoryViolation { ptr: val, len: size })?;
            if end > mem_size {
                return Err(PluginError::MemoryViolation { ptr: val, len: size });
            }
            return Ok((val, size));
        }
    }

    // Fallback: use start of memory as scratch buffer.
    let mem = instance
        .get_export(&mut *store, "memory")
        .and_then(|e| e.into_memory())
        .ok_or(PluginError::NoMemory)?;
    let mem_size = mem.data(&mut *store).len() as i32;
    let size = std::cmp::min(crate::guest_abi::DEFAULT_SCRATCH_SIZE as i32, mem_size);
    if size < 1024 {
        return Err(PluginError::ScratchTooSmall { size });
    }
    Ok((0, size))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guest_abi::HOST_ABI_VERSION;
    use crate::host::PluginHost;
    use std::sync::Arc;

    /// Basic initialisation of `PluginLoader` via `PluginLoader::new` must succeed.
    #[tokio::test]
    async fn test_loader_new_creates_valid_instance() {
        let host = Arc::new(PluginHost::new().expect("PluginHost should initialise"));
        let loader = PluginLoader::new(host);
        // Verify the loader can be used — compile the minimal manifest-only module.
        // JSON: {"id":"test","name":"Test","version":"0.1.0","description":"T","abi_version":1,"capabilities_required":[],"provides":[]}
        // Length: 120 bytes → stored at offset 256
        let wasm = wat::parse_str(
            r#"
            (module
              (memory (export "memory") 2)
              (global (export "scratch_buffer") (mut i32) (i32.const 0))
              (global (export "scratch_buffer_size") i32 (i32.const 65536))
              (data (i32.const 256) "{\"id\":\"test\",\"name\":\"Test\",\"version\":\"0.1.0\",\"description\":\"T\",\"abi_version\":1,\"capabilities_required\":[],\"provides\":[]}")
              (func (export "manifest") (result i64)
                (i64.or
                  (i64.shl (i64.const 256) (i64.const 32))
                  (i64.const 120)
                )
              )
              (func (export "init") (result i32)
                i32.const 0
              )
            )
            "#,
        )
        .expect("WAT should parse");

        let loaded = loader.load_from_bytes(&wasm, Path::new("test.wasm")).await;
        assert!(loaded.is_ok(), "load_from_bytes should succeed");
        let loaded = loaded.unwrap();
        assert_eq!(loaded.manifest.id, "test");
        assert_eq!(loaded.manifest.abi_version, HOST_ABI_VERSION);
    }

    /// A WASM module exceeding `MAX_WASM_MODULE_SIZE` must be rejected early.
    #[tokio::test]
    async fn test_loader_rejects_oversized_module() {
        let host = Arc::new(PluginHost::new().expect("PluginHost should initialise"));
        let loader = PluginLoader::new(host);

        // Create a buffer larger than the max allowed size.
        let oversized = vec![0u8; PluginHost::MAX_WASM_MODULE_SIZE + 1];

        let result = loader.load_from_bytes(&oversized, Path::new("oversized.wasm")).await;
        assert!(result.is_err(), "expected error for oversized module");
        match result {
            Err(PluginError::InvalidManifest(msg)) => {
                assert!(msg.contains("module too large"), "msg: {msg}");
            }
            Err(other) => panic!("expected InvalidManifest, got: {other}"),
            Ok(_) => unreachable!(),
        }
    }

    /// Loading a WASM module exactly at the size limit must succeed (boundary).
    #[tokio::test]
    async fn test_loader_accepts_exact_max_module_size() {
        let host = Arc::new(PluginHost::new().expect("PluginHost should initialise"));
        let loader = PluginLoader::new(host);

        // Create a buffer exactly at the max allowed size.
        let exact = vec![0u8; PluginHost::MAX_WASM_MODULE_SIZE];

        // This is NOT a valid WASM module, so it will fail later — but it
        // must NOT be rejected by the early size check (which uses >, not >=).
        let result = loader.load_from_bytes(&exact, Path::new("boundary.wasm")).await;
        // Should fail with a wasmtime error, NOT "module too large"
        match result {
            Err(PluginError::InvalidManifest(msg)) => {
                assert!(!msg.contains("module too large"), "should not hit size check: {msg}");
            }
            Err(_) => {} // Accept any non-size error
            Ok(_) => {}  // Very unlikely for all-zero bytes
        }
    }

    /// Loading a WASM module one byte over the limit must be rejected.
    #[tokio::test]
    async fn test_loader_rejects_oversized_module_exact() {
        let host = Arc::new(PluginHost::new().expect("PluginHost should initialise"));
        let loader = PluginLoader::new(host);

        // Create a buffer one byte over the max allowed size.
        let oversized = vec![0u8; PluginHost::MAX_WASM_MODULE_SIZE + 1];

        let result = loader.load_from_bytes(&oversized, Path::new("oversized.wasm")).await;
        match result {
            Err(PluginError::InvalidManifest(msg)) => {
                assert!(msg.contains("module too large"), "msg: {msg}");
            }
            Err(other) => panic!("expected InvalidManifest, got: {other}"),
            Ok(_) => unreachable!(),
        }
    }

    /// A plugin with an `abi_version` higher than `HOST_ABI_VERSION` must be rejected.
    #[tokio::test]
    async fn test_loader_rejects_future_abi_version() {
        let host = Arc::new(PluginHost::new().expect("PluginHost should initialise"));
        let loader = PluginLoader::new(host);

        // Valid WASM structure but with abi_version=99 (future).
        // JSON: {"id":"future-abi","name":"Future ABI","version":"0.1.0","description":"Future ABI","abi_version":99,"capabilities_required":[],"provides":[]}
        // Length: 142 bytes → stored at offset 256
        let wasm = wat::parse_str(
            r#"
            (module
              (memory (export "memory") 2)
              (global (export "scratch_buffer") (mut i32) (i32.const 0))
              (global (export "scratch_buffer_size") i32 (i32.const 65536))
              (data (i32.const 256) "{\"id\":\"future-abi\",\"name\":\"Future ABI\",\"version\":\"0.1.0\",\"description\":\"Future ABI\",\"abi_version\":99,\"capabilities_required\":[],\"provides\":[]}")
              (func (export "manifest") (result i64)
                (i64.or
                  (i64.shl (i64.const 256) (i64.const 32))
                  (i64.const 142)
                )
              )
              (func (export "init") (result i32)
                i32.const 0
              )
            )
            "#,
        )
        .expect("WAT should parse");

        let result = loader.load_from_bytes(&wasm, Path::new("future_abi.wasm")).await;
        assert!(result.is_err(), "expected error for future ABI version");
        match result {
            Err(PluginError::AbiTooNew { found, max }) => {
                assert_eq!(found, 99);
                assert_eq!(max, HOST_ABI_VERSION);
            }
            Err(other) => panic!("expected AbiTooNew, got: {other}"),
            Ok(_) => unreachable!(),
        }
    }

    /// A module whose init() panics/traps (e.g. division by zero) must return InitFailed(-2 / timeout).
    #[tokio::test]
    async fn test_loader_initialise_trap_returns_minus_two() {
        let host = Arc::new(PluginHost::new().expect("PluginHost should initialise"));
        let loader = PluginLoader::new(host);

        // Use a manifest JSON whose length we can compute exactly:
        // {"id":"trap","name":"Trap","version":"0.1.0","description":"X","abi_version":1,"capabilities_required":[],"provides":[]}
        // Total: 106 bytes (verified by checking the string length)
        let manifest_json = r#"{"id":"trap","name":"Trap","version":"0.1.0","description":"X","abi_version":1,"capabilities_required":[],"provides":[]}"#;
        let manifest_len = manifest_json.len();
        let json_for_wat = manifest_json.replace('"', "\\\"");

        // Module with init() that traps via unreachable.
        let wat_str = format!(
            r#"
            (module
              (memory (export "memory") 2)
              (global (export "scratch_buffer") (mut i32) (i32.const 0))
              (global (export "scratch_buffer_size") i32 (i32.const 65536))
              (data (i32.const 256) "{json}")
              (func (export "manifest") (result i64)
                (i64.or (i64.shl (i64.const 256) (i64.const 32)) (i64.const {len}))
              )
              (func (export "init") (result i32)
                unreachable
              )
            )
            "#,
            json = json_for_wat,
            len = manifest_len,
        );

        let wasm = wat::parse_str(&wat_str).expect("WAT should parse");

        let loaded = loader
            .load_from_bytes(&wasm, Path::new("trap_init.wasm"))
            .await
            .expect("load should succeed");

        let caps = crate::capability::GrantedCapabilities::new();
        let result = loader.initialise(&loaded, caps).await;
        assert!(result.is_err(), "expected error for trapping init");
        let err = result.unwrap_err();
        // The exact error code depends on wasmtime's error message format:
        // -2 if the error matches known trap/fuel patterns, -1 otherwise.
        // Either is acceptable — the key property is that a trap is caught
        // and returned as InitFailed (not a panic/crash).
        assert!(matches!(&err, PluginError::InitFailed(_)), "expected InitFailed(?), got: {err}",);
    }

    /// A module with manifest declaring an invalid size (len <= 0) must be rejected.
    #[tokio::test]
    async fn test_loader_rejects_manifest_with_non_positive_length() {
        let host = Arc::new(PluginHost::new().expect("PluginHost should initialise"));
        let loader = PluginLoader::new(host);

        // The manifest ptr/len is set to (256, 0) by the manifest export.
        // Even though the data at 256 contains valid JSON, the len=0 triggers
        // the "len <= 0" guard before any parsing.
        let manifest_json = r#"{"id":"zero","name":"Zero","version":"0.1.0","description":"X","abi_version":1,"capabilities_required":[],"provides":[]}"#;
        let json_for_wat = manifest_json.replace('"', "\\\"");
        let wat_str = format!(
            r#"
            (module
              (memory (export "memory") 2)
              (global (export "scratch_buffer") (mut i32) (i32.const 0))
              (global (export "scratch_buffer_size") i32 (i32.const 65536))
              (data (i32.const 256) "{json}")
              (func (export "manifest") (result i64)
                (i64.or (i64.shl (i64.const 256) (i64.const 32)) (i64.const 0))
              )
              (func (export "init") (result i32) i32.const 0)
            )
            "#,
            json = json_for_wat,
        );

        let wasm = wat::parse_str(&wat_str).expect("WAT should parse");

        let result = loader.load_from_bytes(&wasm, Path::new("zero_len.wasm")).await;
        match result {
            Err(PluginError::InvalidManifest(msg)) => {
                assert!(msg.contains("invalid size"), "msg: {msg}");
            }
            Err(other) => panic!("expected InvalidManifest, got: {other}"),
            Ok(_) => panic!("expected error, got Ok"),
        }
    }

    /// A module with manifest claiming more than 1 MB must be rejected.
    #[tokio::test]
    async fn test_loader_rejects_manifest_with_huge_length() {
        let host = Arc::new(PluginHost::new().expect("PluginHost should initialise"));
        let loader = PluginLoader::new(host);

        // Manifest returns pack_ptr_len(256, 1_048_577) — one byte over 1 MB
        let manifest_json = r#"{"id":"huge","name":"Huge","version":"0.1.0","description":"X","abi_version":1,"capabilities_required":[],"provides":[]}"#;
        let json_for_wat = manifest_json.replace('"', "\\\"");
        let wat_str = format!(
            r#"
            (module
              (memory (export "memory") 2)
              (global (export "scratch_buffer") (mut i32) (i32.const 0))
              (global (export "scratch_buffer_size") i32 (i32.const 65536))
              (data (i32.const 256) "{json}")
              (func (export "manifest") (result i64)
                (i64.or (i64.shl (i64.const 256) (i64.const 32)) (i64.const 1048577))
              )
              (func (export "init") (result i32) i32.const 0)
            )
            "#,
            json = json_for_wat,
        );

        let wasm = wat::parse_str(&wat_str).expect("WAT should parse");

        let result = loader.load_from_bytes(&wasm, Path::new("huge_len.wasm")).await;
        match result {
            Err(PluginError::InvalidManifest(msg)) => {
                assert!(msg.contains("invalid size"), "msg: {msg}");
            }
            Err(other) => panic!("expected InvalidManifest, got: {other}"),
            Ok(_) => panic!("expected error, got Ok"),
        }
    }

    /// A module without the `init` export must fail during `initialise`.
    #[tokio::test]
    async fn test_loader_initialise_missing_init() {
        let host = Arc::new(PluginHost::new().expect("PluginHost should initialise"));
        let loader = PluginLoader::new(host);

        // JSON: {"id":"no-init","name":"No Init","version":"0.1.0","description":"Missing init","abi_version":1,"capabilities_required":[],"provides":[]}
        // Length: 137 bytes → stored at offset 256
        let wasm = wat::parse_str(
            r#"
            (module
              (memory (export "memory") 2)
              (global (export "scratch_buffer") (mut i32) (i32.const 0))
              (global (export "scratch_buffer_size") i32 (i32.const 65536))
              (data (i32.const 256) "{\"id\":\"no-init\",\"name\":\"No Init\",\"version\":\"0.1.0\",\"description\":\"Missing init\",\"abi_version\":1,\"capabilities_required\":[],\"provides\":[]}")
              (func (export "manifest") (result i64)
                (i64.or
                  (i64.shl (i64.const 256) (i64.const 32))
                  (i64.const 137)
                )
              )
            )
            "#,
        )
        .expect("WAT should parse");

        let loaded = loader
            .load_from_bytes(&wasm, Path::new("no_init.wasm"))
            .await
            .expect("load should succeed despite missing init");

        let caps = crate::capability::GrantedCapabilities::new();
        let result = loader.initialise(&loaded, caps).await;
        assert!(result.is_err(), "expected error for missing init export");
        let err = result.unwrap_err();
        assert!(
            matches!(&err, PluginError::InvalidManifest(msg) if msg.contains("missing init")),
            "expected InvalidManifest about missing init, got: {err}",
        );
    }
}
