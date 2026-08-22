use concerto_api_types::plugin::PluginManifest;
use concerto_core::CancellationToken;
use wasmtime::Store;

use crate::error::PluginError;
use crate::guest_abi::RESULT_ERROR;
use crate::host::PluginStoreData;

/// A fully initialised and active plugin ready to handle tool calls.
pub struct ActivePlugin {
    pub manifest: PluginManifest,
    pub instance: wasmtime::Instance,
    pub store: Store<PluginStoreData>,
}

impl ActivePlugin {
    /// Set the cancellation token observed by in-flight async host calls.
    ///
    /// Threads the caller's token into the plugin store data so host functions
    /// such as `concerto.completion` observe agent/tool-call cancellation
    /// instead of a fresh per-call token (ADR-38). `None` restores the
    /// documented fallback (host functions use a fresh token).
    pub fn set_cancel(&mut self, cancel: Option<CancellationToken>) {
        self.store.data_mut().cancel = cancel;
    }

    /// Call a tool export on this plugin.
    pub async fn call_tool(
        &mut self,
        name: &str,
        input: &serde_json::Value,
    ) -> Result<serde_json::Value, PluginError> {
        self.call_json_export("call_tool", name, input)
            .await
            .map_err(|e| PluginError::ToolCallFailed(e.to_string()))
    }

    /// Call the `call_provider` export with an operation name and JSON request.
    ///
    /// `operation` is the provider operation name (e.g. `"complete"`, `"list_models"`),
    /// `req` is the JSON request body.
    pub async fn call_provider(
        &mut self,
        operation: &str,
        req: &serde_json::Value,
    ) -> Result<serde_json::Value, PluginError> {
        self.call_json_export("call_provider", operation, req)
            .await
            .map_err(|e| PluginError::PluginProviderFailed(e.to_string()))
    }

    /// Call the `call_adapter` export with an operation name and JSON request.
    ///
    /// `operation` is the adapter operation name (e.g. `"store"`, `"search"`),
    /// `req` is the JSON request body.
    pub async fn call_adapter(
        &mut self,
        operation: &str,
        req: &serde_json::Value,
    ) -> Result<serde_json::Value, PluginError> {
        self.call_json_export("call_adapter", operation, req)
            .await
            .map_err(|e| PluginError::PluginAdapterFailed(e.to_string()))
    }

    /// Call the `call_dialect` export with an operation name and JSON input
    /// (ADR-53).
    ///
    /// `operation` is one of the two dialect ops (`"render"` or `"cache"`);
    /// `input` is the JSON envelope the host defines for that op. The plugin
    /// returns a JSON **string** — the dialect's wire body (or modified wire
    /// body for `"cache"`).
    pub async fn call_dialect(
        &mut self,
        operation: &str,
        input: &serde_json::Value,
    ) -> Result<serde_json::Value, PluginError> {
        self.call_json_export(crate::guest_abi::EXPORT_CALL_DIALECT, operation, input)
            .await
            .map_err(|e| PluginError::PluginDialectFailed(e.to_string()))
    }

    /// Generic dispatch: call any WASM export with shape `(op: &str, json: &Value) -> Value`.
    ///
    /// Writes `op_name` and JSON-serialized `input` into linear memory past
    /// the scratch area, then calls the 6-parameter export named `export_name`.
    /// The result is read back from the scratch buffer and deserialized.
    ///
    /// # Resource Limits
    ///
    /// Before each call:
    /// - Fuel is replenished to `PluginHost::MAX_FUEL` to ensure a fresh computation budget
    /// - Memory usage is checked against `PluginHost::DEFAULT_MAX_MEMORY`
    ///
    /// This prevents fuel exhaustion from accumulating across multiple calls and
    /// ensures plugins cannot exceed memory limits.
    async fn call_json_export(
        &mut self,
        export_name: &str,
        op_name: &str,
        input: &serde_json::Value,
    ) -> Result<serde_json::Value, PluginError> {
        // Replenish fuel before each call to ensure fresh computation budget.
        // This prevents fuel exhaustion from accumulating across multiple calls.
        if let Err(e) = self.store.set_fuel(crate::host::PluginHost::MAX_FUEL) {
            tracing::warn!("failed to replenish fuel: {e}");
        }

        // Check memory usage before call to prevent exceeding limits.
        let mem = self
            .instance
            .get_export(&mut self.store, "memory")
            .and_then(|e| e.into_memory())
            .ok_or(PluginError::NoMemory)?;

        let current_memory = mem.data_size(&self.store);
        if current_memory > crate::host::PluginHost::DEFAULT_MAX_MEMORY {
            return Err(PluginError::MemoryLimitExceeded {
                current: current_memory,
                max: crate::host::PluginHost::DEFAULT_MAX_MEMORY,
            });
        }

        let op_bytes = op_name.as_bytes();
        let input_json =
            serde_json::to_string(input).map_err(|e| PluginError::ToolCallFailed(e.to_string()))?;
        let input_bytes = input_json.as_bytes();

        let scratch_ptr = self.store.data().scratch.ptr;
        let scratch_len = self.store.data().scratch.len;

        let op_offset = scratch_len + 16;
        let input_offset = op_offset + op_bytes.len() as i32 + 8;

        let mem_size = mem.data(&self.store).len();
        let needed = (input_offset + input_bytes.len() as i32) as usize;
        if needed > mem_size {
            return Err(PluginError::MemoryViolation {
                ptr: input_offset,
                len: input_bytes.len() as i32,
            });
        }

        mem.write(&mut self.store, op_offset as usize, op_bytes)
            .map_err(PluginError::MemoryWrite)?;
        mem.write(&mut self.store, input_offset as usize, input_bytes)
            .map_err(PluginError::MemoryWrite)?;

        let func = self
            .instance
            .get_typed_func::<(i32, i32, i32, i32, i32, i32), i64>(&mut self.store, export_name)
            .map_err(|_| PluginError::InvalidManifest(format!("missing {export_name} export")))?;

        let result = func
            .call_async(
                &mut self.store,
                (
                    op_offset,
                    op_bytes.len() as i32,
                    input_offset,
                    input_bytes.len() as i32,
                    scratch_ptr,
                    scratch_len,
                ),
            )
            .await;

        // M2: the caller's token was threaded into the store (via `set_cancel`)
        // for the duration of this wasm call. Reset it now, on BOTH the Ok and
        // Err paths, so a stale token cannot leak to a later direct call. Safe
        // because the caller holds the per-plugin mutex for the whole call, so
        // there is no concurrent access to `self.store.data_mut().cancel`.
        self.store.data_mut().cancel = None;

        let result = result.map_err(|e| PluginError::ToolCallFailed(e.to_string()))?;

        if result == RESULT_ERROR {
            let err_msg =
                self.store.data().last_error.clone().unwrap_or_else(|| "unknown error".into());
            return Err(PluginError::ToolCallFailed(err_msg));
        }

        let (_ptr, len) = crate::guest_abi::unpack_ptr_len(result);
        let output_bytes = mem
            .data(&self.store)
            .get(scratch_ptr as usize..(scratch_ptr + len) as usize)
            .ok_or_else(|| PluginError::MemoryViolation { ptr: scratch_ptr, len })?;

        let output: serde_json::Value = serde_json::from_slice(output_bytes)
            .map_err(|e| PluginError::ToolCallFailed(e.to_string()))?;
        Ok(output)
    }
}

impl std::fmt::Debug for ActivePlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActivePlugin").field("manifest", &self.manifest).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guest_abi::HOST_ABI_VERSION;
    use crate::host::PluginHost;
    use crate::loader::PluginLoader;
    use concerto_api_types::plugin::PluginManifest;
    use std::path::Path;
    use std::sync::Arc;

    // ------------------------------------------------------------------
    // Helpers
    // ------------------------------------------------------------------

    /// Minimal WAT module WITHOUT a memory export – used to exercise the
    /// NoMemory error path in `call_json_export`.
    const NO_MEMORY_WAT: &str = r#"
    (module
      (func (export "init") (result i32) i32.const 0)
      (func (export "manifest") (result i64) i64.const -1)
    )
    "#;

    /// WAT module WITH memory (2 pages) and scratch_buffer globals but no
    /// call_tool / call_provider / call_adapter exports.
    const MEMORY_NO_EXPORT_WAT: &str = r#"
    (module
      (memory (export "memory") 2)
      (global (export "scratch_buffer") (mut i32) (i32.const 0))
      (global (export "scratch_buffer_size") i32 (i32.const 65536))
      (func (export "init") (result i32) i32.const 0)
      (func (export "manifest") (result i64) i64.const -1)
    )
    "#;

    fn compile_wat(wat: &str) -> Vec<u8> {
        wat::parse_str(wat).expect("WAT should parse")
    }

    /// Build a bare `ActivePlugin` with the given exports available (used when
    /// `load_from_bytes` would reject the module due to missing manifest).
    async fn create_bare_plugin(wasm_bytes: &[u8], manifest: PluginManifest) -> ActivePlugin {
        let host = Arc::new(PluginHost::new().expect("PluginHost should initialise"));
        let _module =
            wasmtime::Module::new(host.engine(), wasm_bytes).expect("module should compile");

        let mut store = host.create_store();
        let linker = {
            let mut l = wasmtime::Linker::new(host.engine());
            crate::host_fns::register_minimal_host_functions(&mut l)
                .expect("register host functions");
            l
        };

        let instance = linker.instantiate_async(&mut store, &_module).await.expect("instantiate");

        // Set up scratch buffer if available.
        if let Some(export) = instance.get_export(&mut store, "scratch_buffer") {
            if let Some(global) = export.into_global() {
                let ptr = global.get(&mut store).i32().unwrap_or(0);
                let size = instance
                    .get_export(&mut store, "scratch_buffer_size")
                    .and_then(|e| e.into_global())
                    .map(|g| g.get(&mut store).i32().unwrap_or(65536))
                    .unwrap_or(65536);
                store.data_mut().scratch = crate::host::ScratchBuffer { ptr, len: size };
            }
        }

        ActivePlugin { manifest, instance, store }
    }

    // ------------------------------------------------------------------
    // Tests
    // ------------------------------------------------------------------

    /// `call_tool` on a plugin without a memory export must return
    /// `PluginError::ToolCallFailed` wrapping "WASM module has no exported memory".
    #[tokio::test]
    async fn test_active_plugin_no_memory_call_tool() {
        let host = Arc::new(PluginHost::new().expect("PluginHost should initialise"));
        let wasm = compile_wat(NO_MEMORY_WAT);
        let _module = wasmtime::Module::new(host.engine(), &wasm).expect("module should compile");

        let manifest = PluginManifest {
            id: "no-mem".into(),
            name: "No Memory".into(),
            version: "0.1.0".into(),
            description: "Plugin without memory".into(),
            abi_version: HOST_ABI_VERSION,
            capabilities_required: vec![],
            provides: vec![],
        };

        let mut plugin = create_bare_plugin(&wasm, manifest).await;
        let result = plugin.call_tool("test", &serde_json::json!({})).await;
        assert!(result.is_err(), "expected error, got Ok");
        let err = result.unwrap_err();
        assert!(
            matches!(&err, PluginError::ToolCallFailed(msg) if msg.contains("no exported memory")),
            "expected ToolCallFailed with 'no exported memory', got: {err}",
        );
    }

    /// `call_provider` on a plugin without a memory export must return
    /// `PluginError::PluginProviderFailed` wrapping "WASM module has no exported memory".
    #[tokio::test]
    async fn test_active_plugin_no_memory_call_provider() {
        let host = Arc::new(PluginHost::new().expect("PluginHost should initialise"));
        let wasm = compile_wat(NO_MEMORY_WAT);
        let _module = wasmtime::Module::new(host.engine(), &wasm).expect("module should compile");

        let manifest = PluginManifest {
            id: "no-mem-provider".into(),
            name: "No Memory Provider".into(),
            version: "0.1.0".into(),
            description: "Provider without memory".into(),
            abi_version: HOST_ABI_VERSION,
            capabilities_required: vec![],
            provides: vec![],
        };

        let mut plugin = create_bare_plugin(&wasm, manifest).await;
        let result = plugin.call_provider("complete", &serde_json::json!({})).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(&err, PluginError::PluginProviderFailed(msg) if msg.contains("no exported memory")),
            "expected PluginProviderFailed with 'no exported memory', got: {err}",
        );
    }

    /// `call_adapter` on a plugin without a memory export must return
    /// `PluginError::PluginAdapterFailed` wrapping "WASM module has no exported memory".
    #[tokio::test]
    async fn test_active_plugin_no_memory_call_adapter() {
        let host = Arc::new(PluginHost::new().expect("PluginHost should initialise"));
        let wasm = compile_wat(NO_MEMORY_WAT);
        let _module = wasmtime::Module::new(host.engine(), &wasm).expect("module should compile");

        let manifest = PluginManifest {
            id: "no-mem-adapter".into(),
            name: "No Memory Adapter".into(),
            version: "0.1.0".into(),
            description: "Adapter without memory".into(),
            abi_version: HOST_ABI_VERSION,
            capabilities_required: vec![],
            provides: vec![],
        };

        let mut plugin = create_bare_plugin(&wasm, manifest).await;
        let result = plugin.call_adapter("search", &serde_json::json!({})).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(&err, PluginError::PluginAdapterFailed(msg) if msg.contains("no exported memory")),
            "expected PluginAdapterFailed with 'no exported memory', got: {err}",
        );
    }

    /// Debug formatting must include the manifest id.
    #[tokio::test]
    async fn test_active_plugin_debug_format() {
        let host = Arc::new(PluginHost::new().expect("PluginHost should initialise"));
        let loader = PluginLoader::new(host);

        // Use the ECHO_PLUGIN_WAT pattern from integration tests – data at
        // non-zero offset with explicit packed return.
        let wasm = compile_wat(
            r#"
            (module
              (memory (export "memory") 2)
              (global (export "scratch_buffer") (mut i32) (i32.const 0))
              (global (export "scratch_buffer_size") i32 (i32.const 65536))
              ;; Manifest JSON at offset 256
              (data (i32.const 256) "{\"id\":\"debug-plugin\",\"name\":\"Debug Plugin\",\"version\":\"0.1.0\",\"description\":\"Debug test\",\"abi_version\":1,\"capabilities_required\":[],\"provides\":[]}")
              (func (export "manifest") (result i64)
                (i64.or
                  (i64.shl (i64.const 256) (i64.const 32))
                  (i64.const 145)
                )
              )
              (func (export "init") (result i32)
                i32.const 0
              )
            )
            "#,
        );
        let loaded = loader
            .load_from_bytes(&wasm, Path::new("debug.wasm"))
            .await
            .expect("load should succeed");
        assert_eq!(loaded.manifest.id, "debug-plugin");
        let caps = crate::capability::GrantedCapabilities::new();
        let plugin = loader.initialise(&loaded, caps).await.expect("init should succeed");

        let debug_str = format!("{:?}", plugin);
        assert!(
            debug_str.contains("debug-plugin"),
            "Debug output should contain plugin id: {debug_str}"
        );
        assert!(
            debug_str.contains("ActivePlugin"),
            "Debug output should contain struct name: {debug_str}"
        );
    }

    /// `call_tool` on a plugin that has memory but no `call_tool` export must
    /// return a `ToolCallFailed` error mentioning the missing export.
    #[tokio::test]
    async fn test_active_plugin_missing_export_call_tool() {
        let host = Arc::new(PluginHost::new().expect("PluginHost should initialise"));
        let wasm = compile_wat(MEMORY_NO_EXPORT_WAT);
        let _module = wasmtime::Module::new(host.engine(), &wasm).expect("module should compile");

        let manifest = PluginManifest {
            id: "no-call-tool".into(),
            name: "No CallTool".into(),
            version: "0.1.0".into(),
            description: "Missing call_tool".into(),
            abi_version: HOST_ABI_VERSION,
            capabilities_required: vec![],
            provides: vec![],
        };

        let mut plugin = create_bare_plugin(&wasm, manifest).await;
        let result = plugin.call_tool("test", &serde_json::json!({})).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(&err, PluginError::ToolCallFailed(msg) if msg.contains("missing call_tool export")),
            "expected ToolCallFailed about missing call_tool export, got: {err}",
        );
    }

    /// `call_provider` on a plugin that has memory but no `call_provider` export must
    /// return a `PluginProviderFailed` error mentioning the missing export.
    #[tokio::test]
    async fn test_active_plugin_missing_export_call_provider() {
        let host = Arc::new(PluginHost::new().expect("PluginHost should initialise"));
        let wasm = compile_wat(MEMORY_NO_EXPORT_WAT);
        let _module = wasmtime::Module::new(host.engine(), &wasm).expect("module should compile");

        let manifest = PluginManifest {
            id: "no-call-provider".into(),
            name: "No CallProvider".into(),
            version: "0.1.0".into(),
            description: "Missing call_provider".into(),
            abi_version: HOST_ABI_VERSION,
            capabilities_required: vec![],
            provides: vec![],
        };

        let mut plugin = create_bare_plugin(&wasm, manifest).await;
        let result = plugin.call_provider("complete", &serde_json::json!({})).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(&err, PluginError::PluginProviderFailed(msg) if msg.contains("missing call_provider export")),
            "expected PluginProviderFailed about missing call_provider export, got: {err}",
        );
    }
}
