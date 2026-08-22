//! Integration tests for the Concerto WASM plugin system.
//!
//! Uses WAT (WebAssembly Text Format) to define test WASM modules inline.

use std::path::Path;
use std::sync::Arc;

use concerto_api_types::plugin::CapabilityRequest;
use concerto_plugins::capability::{
    CapabilityDiscriminant, CapabilityManager, CapabilityScope, GrantedCapabilities,
};
use concerto_plugins::guest_abi::*;
use concerto_plugins::host::{PluginHost, PluginStoreData, ScratchBuffer};
use concerto_plugins::loader::PluginLoader;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Minimal WAT module exporting manifest, init, memory, scratch_buffer globals.
const TEST_PLUGIN_WAT: &str = r#"
(module
  (memory (export "memory") 1)
  (global (export "scratch_buffer") (mut i32) (i32.const 0))
  (global (export "scratch_buffer_size") i32 (i32.const 65536))
  (func (export "manifest") (result i64)
    i64.const 146
  )
  (func (export "init") (result i32)
    i32.const 0
  )
  (data (i32.const 0) "{\"id\":\"test-plugin\",\"name\":\"Test Plugin\",\"version\":\"0.1.0\",\"description\":\"A test plugin\",\"abi_version\":1,\"capabilities_required\":[],\"provides\":[]}")
)
"#;

/// Featured WAT module including canonical 6-param call_tool.
const FEATURED_PLUGIN_WAT: &str = r#"
(module
  (memory (export "memory") 1)
  (global (export "scratch_buffer") (mut i32) (i32.const 0))
  (global (export "scratch_buffer_size") i32 (i32.const 65536))
  (func (export "manifest") (result i64)
    i64.const 215
  )
  (func (export "init") (result i32)
    i32.const 0
  )
  (func (export "call_tool") (param i32 i32 i32 i32 i32 i32) (result i64)
    i64.const 0
  )
  (data (i32.const 0) "{\"id\":\"feat-plugin\",\"name\":\"Feat Plugin\",\"version\":\"0.1.0\",\"description\":\"Featured test\",\"abi_version\":1,\"capabilities_required\":[],\"provides\":[{\"Tool\":{\"name\":\"echo\",\"description\":\"Echo input\",\"input_schema\":{}}}]}")
)
"#;

fn compile_wat(wat: &str) -> Vec<u8> {
    wat::parse_str(wat).expect("WAT should parse")
}

fn test_host() -> Arc<PluginHost> {
    Arc::new(PluginHost::new().expect("PluginHost should initialise"))
}

// ---------------------------------------------------------------------------
// Guest ABI
// ---------------------------------------------------------------------------

#[test]
fn pack_unpack_roundtrip() {
    for &(ptr, len) in &[(0, 0), (42, 100), (65535, 255), (1_048_576, 65_536), (-1, -1)] {
        let packed = pack_ptr_len(ptr, len);
        let (got_ptr, got_len) = unpack_ptr_len(packed);
        assert_eq!((got_ptr, got_len), (ptr, len));
    }
}

#[test]
fn result_error_is_negative_one() {
    assert_eq!(RESULT_ERROR, -1i64);
}

#[test]
fn default_scratch_size() {
    assert_eq!(DEFAULT_SCRATCH_SIZE, 65536);
}

// ---------------------------------------------------------------------------
// Capabilities
// ---------------------------------------------------------------------------

#[test]
fn granted_cap_denies_by_default() {
    let caps = GrantedCapabilities::new();
    let req = CapabilityRequest::FilesystemRead { globs: vec![] };
    assert!(!caps.check("any-plugin", &req));
}

#[test]
fn session_grant_is_checked() {
    let mut caps = GrantedCapabilities::new();
    caps.grant_session(CapabilityDiscriminant::FilesystemRead, CapabilityScope::default());
    let req = CapabilityRequest::FilesystemRead { globs: vec![] };
    assert!(caps.check("p", &req));
}

#[test]
fn persistent_grant_checked() {
    let mut caps = GrantedCapabilities::new();
    caps.persist("p", CapabilityDiscriminant::NetworkOutbound, CapabilityScope::default());
    let req = CapabilityRequest::NetworkOutbound { domains: vec![] };
    assert!(caps.check("p", &req));
}

#[test]
fn wrong_cap_is_denied() {
    let mut caps = GrantedCapabilities::new();
    caps.grant_session(CapabilityDiscriminant::FilesystemRead, CapabilityScope::default());
    let req = CapabilityRequest::ShellExecute { allowlist: vec![] };
    assert!(!caps.check("p", &req));
}

#[test]
fn with_persistent_restores() {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let caps = GrantedCapabilities::with_persistent(
        "p",
        vec![(CapabilityDiscriminant::FilesystemRead, CapabilityScope::default(), now + 3600)],
    );
    let req = CapabilityRequest::FilesystemRead { globs: vec![] };
    assert!(caps.check("p", &req));
}

// ---------------------------------------------------------------------------
// CapabilityManager
// ---------------------------------------------------------------------------

#[test]
fn manager_loads_grants() {
    let dir = std::env::temp_dir().join("plugin_test_manager");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let mgr = CapabilityManager::open(&dir).expect("manager should open");
    let grants = mgr.load_grants("test-plugin", None);
    assert!(grants.is_empty(), "fresh manager has no grants");
}

// ---------------------------------------------------------------------------
// Plugin compilation
// ---------------------------------------------------------------------------

#[test]
fn compile_minimal_plugin() {
    let host = test_host();
    let wasm = compile_wat(TEST_PLUGIN_WAT);
    let module = wasmtime::Module::new(host.engine(), &wasm).expect("module should compile");
    assert!(module.exports().any(|e: wasmtime::ExportType<'_>| e.name() == "manifest"));
}

#[test]
fn compile_featured_plugin() {
    let host = test_host();
    let wasm = compile_wat(FEATURED_PLUGIN_WAT);
    let module = wasmtime::Module::new(host.engine(), &wasm).expect("module should compile");

    let exports: Vec<_> =
        module.exports().map(|e: wasmtime::ExportType<'_>| e.name().to_string()).collect();

    for name in &["memory", "manifest", "call_tool", "scratch_buffer", "scratch_buffer_size"] {
        assert!(exports.contains(&name.to_string()), "missing export: {name}");
    }
}

#[tokio::test]
async fn loader_instantiates_module() {
    let host = test_host();
    let wasm = compile_wat(TEST_PLUGIN_WAT);
    let loader = PluginLoader::new(host);

    let result = loader.load_from_bytes(&wasm, Path::new("test.wasm")).await;
    match result {
        Ok(loaded) => assert_eq!(loaded.manifest.id, "test-plugin"),
        Err(e) => panic!("load_from_bytes failed: {e}"),
    }
}

// ---------------------------------------------------------------------------
// Host store data
// ---------------------------------------------------------------------------

#[test]
fn store_data_defaults() {
    let data = PluginStoreData::default();
    assert!(data.plugin_id.is_empty());
    assert!(data.last_error.is_none());
    assert!(data.provider.is_none(), "provider should default to None");
    assert_eq!(data.scratch.ptr, 0);
    assert_eq!(data.scratch.len, 0);
}

#[test]
fn scratch_buffer_default() {
    let sb = ScratchBuffer::default();
    assert_eq!(sb.ptr, 0);
    assert_eq!(sb.len, 0);
}

#[test]
fn scratch_buffer_non_default() {
    let sb = ScratchBuffer { ptr: 4096, len: 65536 };
    assert_eq!(sb.ptr, 4096);
    assert_eq!(sb.len, 65536);
}

// ---------------------------------------------------------------------------
// Host function linking
// ---------------------------------------------------------------------------

#[tokio::test]
async fn linker_registers_host_functions() {
    let host = test_host();
    let mut store = host.create_store();
    let mut linker = wasmtime::Linker::new(host.engine());
    concerto_plugins::host_fns::register_host_functions(&mut linker)
        .expect("register should succeed");

    let wasm = compile_wat(TEST_PLUGIN_WAT);
    let module = wasmtime::Module::new(host.engine(), &wasm).unwrap();
    // Async store (ADR-38): instantiation must go through `instantiate_async`.
    let result = linker.instantiate_async(&mut store, &module).await;
    assert!(result.is_ok(), "linker should provide all imports: {:?}", result.err());
}

// ---------------------------------------------------------------------------
// Discovery
// ---------------------------------------------------------------------------

#[test]
fn discovery_finds_wasm_files() {
    use concerto_plugins::discovery::{DiscoveryConfig, PluginDiscovery};

    let dir = std::env::temp_dir().join("plugin_test_discovery");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    std::fs::write(dir.join("dummy.wasm"), [0x00, 0x61, 0x73, 0x6d]).unwrap();
    std::fs::write(dir.join("not.wasm"), b"not wasm").unwrap();
    std::fs::write(dir.join("readme.txt"), b"hello").unwrap();

    let config = DiscoveryConfig { search_paths: vec![dir.clone()], bundled_path: None };
    let discovery = PluginDiscovery::new(config);
    let candidates = discovery.discover().expect("discover should succeed");

    let paths: Vec<_> = candidates
        .iter()
        .map(|c| c.wasm_path.file_name().unwrap().to_string_lossy().to_string())
        .collect();
    assert!(paths.contains(&"dummy.wasm".to_string()), "should find dummy.wasm: {paths:?}");
    assert!(!paths.contains(&"readme.txt".to_string()), "should skip non-wasm");
}

#[test]
fn discovery_empty_when_no_search_paths() {
    use concerto_plugins::discovery::{DiscoveryConfig, PluginDiscovery};

    let config = DiscoveryConfig { search_paths: vec![], bundled_path: None };
    let discovery = PluginDiscovery::new(config);
    let candidates = discovery.discover().expect("discover should succeed");
    assert!(candidates.is_empty(), "no paths should yield no candidates");
}

// ---------------------------------------------------------------------------
// Error types
// ---------------------------------------------------------------------------

#[test]
fn plugin_error_display() {
    let err = concerto_plugins::error::PluginError::NoMemory;
    assert!(!format!("{err}").is_empty());
}

#[test]
fn plugin_error_is_send() {
    fn assert_send<T: Send>() {}
    assert_send::<concerto_plugins::error::PluginError>();
}

// ---------------------------------------------------------------------------
// Lifecycle: load → init → call_tool round trip
// ---------------------------------------------------------------------------

/// WAT module with a working call_tool that writes JSON to the scratch buffer.
/// Returns `{"ok":true}` for any tool call.
const ECHO_PLUGIN_WAT: &str = r#"
(module
  (memory (export "memory") 2)
  (global (export "scratch_buffer") (mut i32) (i32.const 0))
  (global (export "scratch_buffer_size") i32 (i32.const 65536))

  ;; Response data at offset 0: {"ok":true} (11 bytes)
  ;; The host reads call_tool result from scratch_ptr (=0), so response must be here.
  (data (i32.const 0) "{\"ok\":true}")

  ;; Manifest JSON at offset 256 (207 bytes)
  (data (i32.const 256) "{\"id\":\"echo-plugin\",\"name\":\"Echo Plugin\",\"version\":\"0.1.0\",\"description\":\"Echo plugin\",\"abi_version\":1,\"capabilities_required\":[],\"provides\":[{\"Tool\":{\"name\":\"echo\",\"description\":\"Echo\",\"input_schema\":{}}}]}")

  (func (export "manifest") (result i64)
    ;; Return pack_ptr_len(256, 207)
    (i64.or
      (i64.shl (i64.const 256) (i64.const 32))
      (i64.const 207)
    )
  )

  (func (export "init") (result i32)
    i32.const 0
  )

  ;; call_tool returns pack_ptr_len(0, 11)
  ;; Host reads from scratch_ptr=0 for len=11 bytes
  (func (export "call_tool") (param i32 i32 i32 i32 i32 i32) (result i64)
    (i64.or
      (i64.shl (i64.const 0) (i64.const 32))
      (i64.const 11)
    )
  )
)
"#;

/// WAT module whose init() returns 1 (failure).
const FAILING_INIT_WAT: &str = r#"
(module
  (memory (export "memory") 1)
  (global (export "scratch_buffer") (mut i32) (i32.const 0))
  (global (export "scratch_buffer_size") i32 (i32.const 65536))
  (func (export "manifest") (result i64)
    i64.const 139
  )
  (func (export "init") (result i32)
    i32.const 1
  )
  (func (export "call_tool") (param i32 i32 i32 i32 i32 i32) (result i64)
    i64.const 0
  )
  (data (i32.const 0) "{\"id\":\"fail-init\",\"name\":\"Fail Init\",\"version\":\"0.1.0\",\"description\":\"Fails init\",\"abi_version\":1,\"capabilities_required\":[],\"provides\":[]}")
)
"#;

/// WAT module with wrong ABI version (99).
const WRONG_ABI_WAT: &str = r#"
(module
  (memory (export "memory") 1)
  (global (export "scratch_buffer") (mut i32) (i32.const 0))
  (global (export "scratch_buffer_size") i32 (i32.const 65536))
  (func (export "manifest") (result i64)
    i64.const 139
  )
  (func (export "init") (result i32)
    i32.const 0
  )
  (data (i32.const 0) "{\"id\":\"wrong-abi\",\"name\":\"Wrong ABI\",\"version\":\"0.1.0\",\"description\":\"Wrong ABI\",\"abi_version\":99,\"capabilities_required\":[],\"provides\":[]}")
)
"#;

/// WAT module missing the init export.
const NO_INIT_WAT: &str = r#"
(module
  (memory (export "memory") 1)
  (global (export "scratch_buffer") (mut i32) (i32.const 0))
  (global (export "scratch_buffer_size") i32 (i32.const 65536))
  (func (export "manifest") (result i64)
    i64.const 132
  )
  (func (export "call_tool") (param i32 i32 i32 i32 i32 i32) (result i64)
    i64.const 0
  )
  (data (i32.const 0) "{\"id\":\"no-init\",\"name\":\"No Init\",\"version\":\"0.1.0\",\"description\":\"No init\",\"abi_version\":1,\"capabilities_required\":[],\"provides\":[]}")
)
"#;

fn echo_plugin_wasm() -> Vec<u8> {
    compile_wat(ECHO_PLUGIN_WAT)
}

#[tokio::test]
async fn lifecycle_load_and_initialise() {
    let host = test_host();
    let wasm = echo_plugin_wasm();
    let loader = PluginLoader::new(host);

    let loaded =
        loader.load_from_bytes(&wasm, Path::new("echo.wasm")).await.expect("load should succeed");
    assert_eq!(loaded.manifest.id, "echo-plugin");
    assert_eq!(loaded.manifest.provides.len(), 1);

    let caps = GrantedCapabilities::new();
    let active = loader.initialise(&loaded, caps).await.expect("init should succeed");
    assert_eq!(active.manifest.id, "echo-plugin");
}

#[tokio::test]
async fn lifecycle_call_tool_round_trip() {
    let host = test_host();
    let wasm = echo_plugin_wasm();
    let loader = PluginLoader::new(host);

    let loaded =
        loader.load_from_bytes(&wasm, Path::new("echo.wasm")).await.expect("load should succeed");
    let caps = GrantedCapabilities::new();
    let mut active = loader.initialise(&loaded, caps).await.expect("init should succeed");

    let input = serde_json::json!({"message": "hello"});
    let result = active.call_tool("echo", &input).await.expect("call_tool should succeed");
    assert_eq!(result, serde_json::json!({"ok": true}));
}

#[tokio::test]
async fn lifecycle_init_failure_returns_error() {
    let host = test_host();
    let wasm = compile_wat(FAILING_INIT_WAT);
    let loader = PluginLoader::new(host);

    let loaded =
        loader.load_from_bytes(&wasm, Path::new("fail.wasm")).await.expect("load should succeed");
    let caps = GrantedCapabilities::new();
    let err = loader.initialise(&loaded, caps).await.unwrap_err();
    match err {
        concerto_plugins::error::PluginError::InitFailed(code) => assert_eq!(code, 1),
        other => panic!("expected InitFailed(1), got: {other}"),
    }
}

#[tokio::test]
async fn lifecycle_missing_init_returns_error() {
    let host = test_host();
    let wasm = compile_wat(NO_INIT_WAT);
    let loader = PluginLoader::new(host);

    let loaded = loader
        .load_from_bytes(&wasm, Path::new("no_init.wasm"))
        .await
        .expect("load should succeed");
    let caps = GrantedCapabilities::new();
    let err = loader.initialise(&loaded, caps).await.unwrap_err();
    match err {
        concerto_plugins::error::PluginError::InvalidManifest(msg) => {
            assert!(msg.contains("missing init"), "error should mention init: {msg}");
        }
        other => panic!("expected InvalidManifest about init, got: {other}"),
    }
}

#[tokio::test]
async fn lifecycle_wrong_abi_version_rejected() {
    let host = test_host();
    let wasm = compile_wat(WRONG_ABI_WAT);
    let loader = PluginLoader::new(host);

    let err = loader.load_from_bytes(&wasm, Path::new("wrong_abi.wasm")).await;
    match err {
        Err(concerto_plugins::error::PluginError::AbiTooNew { found, max }) => {
            assert_eq!(found, 99);
            assert_eq!(max, HOST_ABI_VERSION);
        }
        Ok(_) => panic!("expected AbiTooNew error, got Ok"),
        Err(other) => panic!("expected AbiTooNew, got: {other}"),
    }
}

// ---------------------------------------------------------------------------
// Provider plugin WAT module
// ---------------------------------------------------------------------------

/// WAT module that exports `call_provider` and returns a canned completion.
const PROVIDER_PLUGIN_WAT: &str = r#"
(module
  (memory (export "memory") 2)
  (global (export "scratch_buffer") (mut i32) (i32.const 0))
  (global (export "scratch_buffer_size") i32 (i32.const 65536))

  ;; Response at offset 0: {"content":"Provider OK","finish_reason":"stop"}
  (data (i32.const 0) "{\"content\":\"Provider OK\",\"finish_reason\":\"stop\"}")

  ;; Manifest at offset 256
  (data (i32.const 256) "{\"id\":\"test-provider\",\"name\":\"Test Provider Plugin\",\"version\":\"0.1.0\",\"description\":\"Test provider\",\"abi_version\":1,\"capabilities_required\":[],\"provides\":[{\"Provider\":{\"name\":\"test-provider\",\"model\":\"test-model\"}}]}")

  (func (export "manifest") (result i64)
    (i64.or
      (i64.shl (i64.const 256) (i64.const 32))
      (i64.const 215)
    )
  )

  (func (export "init") (result i32)
    i32.const 0
  )

  ;; All operations return the canned response at offset 0 (48 bytes)
  (func (export "call_provider") (param i32 i32 i32 i32 i32 i32) (result i64)
    (i64.or
      (i64.shl (i64.const 0) (i64.const 32))
      (i64.const 48)
    )
  )
)
"#;

// ---------------------------------------------------------------------------
// Adapter plugin WAT module
// ---------------------------------------------------------------------------

/// WAT module that exports `call_adapter` and returns a canned empty result.
const ADAPTER_PLUGIN_WAT: &str = r#"
(module
  (memory (export "memory") 2)
  (global (export "scratch_buffer") (mut i32) (i32.const 0))
  (global (export "scratch_buffer_size") i32 (i32.const 65536))

  ;; Response at offset 0: {"results":[]}
  (data (i32.const 0) "{\"results\":[]}")

  ;; Manifest at offset 256
  (data (i32.const 256) "{\"id\":\"test-adapter\",\"name\":\"Test Adapter Plugin\",\"version\":\"0.1.0\",\"description\":\"Test adapter\",\"abi_version\":1,\"capabilities_required\":[],\"provides\":[{\"MemoryAdapter\":{\"name\":\"test-adapter\",\"kind\":\"memory\"}}]}")

  (func (export "manifest") (result i64)
    (i64.or
      (i64.shl (i64.const 256) (i64.const 32))
      (i64.const 211)
    )
  )

  (func (export "init") (result i32)
    i32.const 0
  )

  ;; All operations return the canned response at offset 0 (14 bytes)
  (func (export "call_adapter") (param i32 i32 i32 i32 i32 i32) (result i64)
    (i64.or
      (i64.shl (i64.const 0) (i64.const 32))
      (i64.const 14)
    )
  )
)
"#;

// ---------------------------------------------------------------------------
// Provider / Adapter integration tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn provider_plugin_compile_and_load() {
    let host = test_host();
    let wasm = compile_wat(PROVIDER_PLUGIN_WAT);
    let module =
        wasmtime::Module::new(host.engine(), &wasm).expect("provider module should compile");

    let mut has_call_provider = false;
    for export in module.exports() {
        if export.name() == "call_provider" {
            has_call_provider = true;
        }
    }
    assert!(has_call_provider, "module must export call_provider");
}

#[tokio::test]
async fn provider_plugin_call_provider() {
    let host = test_host();
    let wasm = compile_wat(PROVIDER_PLUGIN_WAT);
    let loader = PluginLoader::new(host);

    let loaded = loader
        .load_from_bytes(&wasm, std::path::Path::new("provider.wasm"))
        .await
        .expect("load should succeed");
    assert_eq!(loaded.manifest.id, "test-provider");
    assert!(loaded
        .manifest
        .provides
        .iter()
        .any(|p| { matches!(p, concerto_api_types::plugin::PluginProvides::Provider(_)) }));

    let caps = GrantedCapabilities::new();
    let mut active = loader.initialise(&loaded, caps).await.expect("init should succeed");

    let req = serde_json::json!({"model":"test-model","messages":[{"role":"user","content":"hi"}]});
    let result =
        active.call_provider("complete", &req).await.expect("call_provider should succeed");

    assert_eq!(result.get("content").and_then(|v| v.as_str()), Some("Provider OK"));
    assert_eq!(result.get("finish_reason").and_then(|v| v.as_str()), Some("stop"));
}

fn make_provider_wat(manifest_json: &str, response_json: &str) -> Vec<u8> {
    let response_len = response_json.len();
    let manifest_len = manifest_json.len();
    let manifest_offset = 256;

    let manifest_wat = manifest_json.replace('"', "\\\"");
    let response_wat = response_json.replace('"', "\\\"");

    let wat = format!(
        r#"(module
          (memory (export "memory") 2)
          (global (export "scratch_buffer") (mut i32) (i32.const 0))
          (global (export "scratch_buffer_size") i32 (i32.const 65536))
          (data (i32.const 0) "{response}")
          (data (i32.const {manifest_offset}) "{manifest}")
          (func (export "manifest") (result i64)
            (i64.or (i64.shl (i64.const {manifest_offset}) (i64.const 32)) (i64.const {manifest_len}))
          )
          (func (export "init") (result i32) i32.const 0)
          (func (export "call_provider") (param i32 i32 i32 i32 i32 i32) (result i64)
            (i64.or (i64.shl (i64.const 0) (i64.const 32)) (i64.const {response_len}))
          )
        )"#,
        response = response_wat,
        manifest = manifest_wat,
        manifest_offset = manifest_offset,
        manifest_len = manifest_len,
        response_len = response_len,
    );
    wat::parse_str(&wat).expect("WAT should parse")
}

#[tokio::test]
async fn provider_plugin_finish_reason_end_turn() {
    let host = test_host();
    let manifest = r#"{"id":"end-turn-provider","name":"End Turn","version":"0.1.0","description":"Returns end_turn","abi_version":1,"capabilities_required":[],"provides":[{"Provider":{"name":"end-turn","model":"et-model"}}]}"#;
    let response = r#"{"content":"End turn","finish_reason":"end_turn"}"#;
    let wasm = make_provider_wat(manifest, response);
    let loader = PluginLoader::new(host);
    let loaded = loader.load_from_bytes(&wasm, Path::new("et.wasm")).await.expect("load");
    let caps = GrantedCapabilities::new();
    let mut active = loader.initialise(&loaded, caps).await.expect("init");
    let req = serde_json::json!({"model":"x","messages":[]});
    let result = active.call_provider("complete", &req).await.expect("call_provider");
    assert_eq!(result.get("finish_reason").and_then(|v| v.as_str()), Some("end_turn"));
}

#[tokio::test]
async fn provider_plugin_finish_reason_length_not_final() {
    let host = test_host();
    let manifest = r#"{"id":"length-finish","name":"Length Finish","version":"0.1.0","description":"Finish length","abi_version":1,"capabilities_required":[],"provides":[{"Provider":{"name":"len","model":"len-model"}}]}"#;
    let response = r#"{"content":"Truncated","finish_reason":"length"}"#;
    let wasm = make_provider_wat(manifest, response);
    let loader = PluginLoader::new(host);
    let loaded = loader.load_from_bytes(&wasm, Path::new("len.wasm")).await.expect("load");
    let caps = GrantedCapabilities::new();
    let mut active = loader.initialise(&loaded, caps).await.expect("init");
    let req = serde_json::json!({"model":"x","messages":[]});
    let result = active.call_provider("complete", &req).await.expect("call_provider");
    assert_eq!(result.get("finish_reason").and_then(|v| v.as_str()), Some("length"));
}

#[tokio::test]
async fn provider_plugin_defaults_final_when_no_finish_reason() {
    let host = test_host();
    let manifest = r#"{"id":"no-reason-provider","name":"No Reason","version":"0.1.0","description":"No finish_reason","abi_version":1,"capabilities_required":[],"provides":[{"Provider":{"name":"no-reason","model":"nr-model"}}]}"#;
    let response = r#"{"content":"No reason"}"#;
    let wasm = make_provider_wat(manifest, response);
    let loader = PluginLoader::new(host);
    let loaded = loader.load_from_bytes(&wasm, Path::new("nr.wasm")).await.expect("load");
    let caps = GrantedCapabilities::new();
    let mut active = loader.initialise(&loaded, caps).await.expect("init");
    let req = serde_json::json!({"model":"x","messages":[]});
    let result = active.call_provider("complete", &req).await.expect("call_provider");
    assert!(result.get("finish_reason").is_none(), "should have no finish_reason");
    assert_eq!(result.get("content").and_then(|v| v.as_str()), Some("No reason"));
}

#[tokio::test]
async fn provider_checks_cancellation_before_plugin_call() {
    use concerto_core::error::ProviderError;
    use concerto_core::traits::provider::LlmProvider;
    use concerto_core::types::CompletionRequest;
    use concerto_core::CancellationToken;
    use concerto_plugins::provider_host::PluginBackedProvider;

    let host = test_host();
    let wasm = compile_wat(PROVIDER_PLUGIN_WAT);
    let loader = PluginLoader::new(host);
    let loaded = loader.load_from_bytes(&wasm, Path::new("cp.wasm")).await.expect("load");
    let caps = GrantedCapabilities::new();
    let active = loader.initialise(&loaded, caps).await.expect("init");
    let plugin = Arc::new(tokio::sync::Mutex::new(active));

    let provider = PluginBackedProvider::new(plugin, "test", "test-model".into());
    let cancel = CancellationToken::new();
    cancel.cancel();

    let req = CompletionRequest { model: "test".into(), messages: vec![], ..Default::default() };
    let result = provider.stream_completion(req, cancel).await;
    match result {
        Err(ProviderError::Cancelled) => {} // Expected
        Err(other) => panic!("expected Cancelled, got: {other:?}"),
        Ok(_) => panic!("expected Cancelled, got Ok"),
    }
}

#[tokio::test]
async fn provider_context_capacity_and_cost() {
    use concerto_core::traits::provider::LlmProvider;
    use concerto_plugins::provider_host::PluginBackedProvider;

    let host = test_host();
    let wasm = compile_wat(PROVIDER_PLUGIN_WAT);
    let loader = PluginLoader::new(host);
    let loaded = loader.load_from_bytes(&wasm, Path::new("cc.wasm")).await.expect("load");
    let caps = GrantedCapabilities::new();
    let active = loader.initialise(&loaded, caps).await.expect("init");
    let plugin = Arc::new(tokio::sync::Mutex::new(active));

    let provider = PluginBackedProvider::new(plugin, "cc-test", "test-model".into());

    // context_capacity: must use division (not modulo/multiplication)
    let budget = provider.context_capacity("anything");
    assert_eq!(budget.capacity, 8192);
    // TokenBudget::new(8192, 8192/4) => available = 8192 - 2048 = 6144
    // Would be 0 with % (8192 % 4 = 0) or -6144 with * (8192 * 4 = 32768, overflow)
    assert_eq!(budget.available, 6144);

    // approximate_cost: must return 0.0 (not 1.0 or -1.0)
    assert_eq!(provider.approximate_cost(0, 0), 0.0);
    assert_eq!(provider.approximate_cost(100, 200), 0.0);
    assert_eq!(provider.approximate_cost(1_000_000, 1_000_000), 0.0);
}

// ---------------------------------------------------------------------------
// Discovery — bundled_path coverage
// ---------------------------------------------------------------------------

#[test]
fn discovery_finds_wasm_files_in_bundled_path() {
    use concerto_plugins::discovery::{DiscoveryConfig, PluginDiscovery};

    let dir = std::env::temp_dir().join("plugin_test_bundled_discovery");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    std::fs::write(dir.join("alpha.wasm"), [0x00, 0x61, 0x73, 0x6d]).unwrap();
    std::fs::write(dir.join("beta.txt"), b"not wasm").unwrap();

    let config = DiscoveryConfig { search_paths: vec![], bundled_path: Some(dir.clone()) };
    let discovery = PluginDiscovery::new(config);
    let candidates = discovery.discover().expect("discover should succeed");

    let paths: Vec<_> = candidates
        .iter()
        .map(|c| c.wasm_path.file_name().unwrap().to_string_lossy().to_string())
        .collect();
    assert!(paths.contains(&"alpha.wasm".to_string()), "should find alpha.wasm: {paths:?}");
    assert!(!paths.contains(&"beta.txt".to_string()), "should skip non-wasm: {paths:?}");
}

#[test]
fn discovery_skips_missing_bundled_path() {
    use concerto_plugins::discovery::{DiscoveryConfig, PluginDiscovery};

    let missing = std::env::temp_dir().join("plugin_test_missing_bundled_dir");
    let _ = std::fs::remove_dir_all(&missing);

    let config = DiscoveryConfig { search_paths: vec![], bundled_path: Some(missing) };
    let discovery = PluginDiscovery::new(config);
    let candidates = discovery.discover().expect("discover should succeed");
    assert!(candidates.is_empty(), "missing bundled dir should yield no candidates");
}

// ---------------------------------------------------------------------------
// PluginTool — plugin_id / name
// ---------------------------------------------------------------------------

#[tokio::test]
async fn plugin_tool_identifiers() {
    use concerto_core::traits::Tool;
    use concerto_plugins::tool_bridge::PluginTool;

    let host = PluginHost::new().expect("PluginHost");
    let wasm = compile_wat(ECHO_PLUGIN_WAT);
    let loader = PluginLoader::new(Arc::new(host));
    let loaded = loader.load_from_bytes(&wasm, Path::new("echo.wasm")).await.expect("load");
    let caps = GrantedCapabilities::new();
    let active = loader.initialise(&loaded, caps).await.expect("init");
    let plugin = Arc::new(tokio::sync::Mutex::new(active));

    let tool = PluginTool::new(
        "my-plugin".into(),
        plugin,
        "echo".into(),
        "Echo input".into(),
        serde_json::json!({}),
        concerto_core::types::CapabilitySet::default(),
    );

    assert_eq!(tool.plugin_id(), "my-plugin");
    assert_eq!(tool.name(), "echo");
    assert_eq!(tool.description(), "Echo input");
    assert_eq!(tool.input_schema(), serde_json::json!({}));
    assert!(tool.capability_requirements() == concerto_core::types::CapabilitySet::default());
}

#[tokio::test]
async fn provider_plugin_via_manager() {
    use concerto_plugins::capability::{CapabilityApprovalUI, CapabilityManager};
    use concerto_plugins::host::PluginHost;
    use concerto_plugins::manager::PluginManager;

    use concerto_api_types::plugin::PluginManifest;
    use concerto_plugins::error::PluginError;

    struct AutoApprove;
    #[async_trait::async_trait]
    impl CapabilityApprovalUI for AutoApprove {
        async fn request(
            &self,
            _plugin: &PluginManifest,
            capabilities: &[concerto_api_types::plugin::CapabilityRequest],
        ) -> std::result::Result<Vec<concerto_plugins::capability::GrantDecision>, PluginError>
        {
            Ok(vec![concerto_plugins::capability::GrantDecision::Granted; capabilities.len()])
        }
    }

    let dir = std::env::temp_dir().join("plugin_test_provider_mgr");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let host = Arc::new(PluginHost::new().expect("PluginHost should initialise"));
    let cap_mgr = CapabilityManager::open(&dir).expect("CapabilityManager should open");
    let mut mgr = PluginManager::new(host, cap_mgr, None, None);

    let wasm = compile_wat(PROVIDER_PLUGIN_WAT);
    let loaded = mgr
        .load_plugin(&wasm, std::path::Path::new("provider.wasm"), &AutoApprove)
        .await
        .expect("load_plugin should succeed");

    let caps = GrantedCapabilities::new();
    mgr.initialise_plugin(&loaded, caps).await.expect("initialise should succeed");

    // Verify the plugin shows up in list_providers
    let providers = mgr.list_providers().await;
    assert!(!providers.is_empty(), "should have at least one provider");
    assert!(
        providers.iter().any(|(id, name)| id == "test-provider" && name == "test-provider"),
        "test-provider should be listed"
    );

    // Create a provider from the plugin
    let provider =
        mgr.create_provider("test-provider").await.expect("create_provider should succeed");
    assert_eq!(provider.provider_name(), "plugin:test-provider");
}

#[tokio::test]
async fn adapter_plugin_compile_and_load() {
    let host = test_host();
    let wasm = compile_wat(ADAPTER_PLUGIN_WAT);
    let module =
        wasmtime::Module::new(host.engine(), &wasm).expect("adapter module should compile");

    let mut has_call_adapter = false;
    for export in module.exports() {
        if export.name() == "call_adapter" {
            has_call_adapter = true;
        }
    }
    assert!(has_call_adapter, "module must export call_adapter");
}

#[tokio::test]
async fn adapter_plugin_call_adapter() {
    let host = test_host();
    let wasm = compile_wat(ADAPTER_PLUGIN_WAT);
    let loader = PluginLoader::new(host);

    let loaded = loader
        .load_from_bytes(&wasm, std::path::Path::new("adapter.wasm"))
        .await
        .expect("load should succeed");
    assert_eq!(loaded.manifest.id, "test-adapter");
    assert!(loaded
        .manifest
        .provides
        .iter()
        .any(|p| { matches!(p, concerto_api_types::plugin::PluginProvides::MemoryAdapter(_)) }));

    let caps = GrantedCapabilities::new();
    let mut active = loader.initialise(&loaded, caps).await.expect("init should succeed");

    let req = serde_json::json!({"project_id":"test","query":[0.1,0.2],"top_k":5});
    let result = active.call_adapter("search", &req).await.expect("call_adapter should succeed");

    let results = result.get("results").and_then(|v| v.as_array());
    assert!(results.is_some(), "response should contain results array");
}

#[tokio::test]
async fn adapter_plugin_via_manager() {
    use concerto_plugins::capability::{CapabilityApprovalUI, CapabilityManager};
    use concerto_plugins::host::PluginHost;
    use concerto_plugins::manager::PluginManager;

    use concerto_api_types::plugin::PluginManifest;
    use concerto_plugins::error::PluginError;

    struct AutoApprove;
    #[async_trait::async_trait]
    impl CapabilityApprovalUI for AutoApprove {
        async fn request(
            &self,
            _plugin: &PluginManifest,
            capabilities: &[concerto_api_types::plugin::CapabilityRequest],
        ) -> std::result::Result<Vec<concerto_plugins::capability::GrantDecision>, PluginError>
        {
            Ok(vec![concerto_plugins::capability::GrantDecision::Granted; capabilities.len()])
        }
    }

    let dir = std::env::temp_dir().join("plugin_test_adapter_mgr");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let host = Arc::new(PluginHost::new().expect("PluginHost should initialise"));
    let cap_mgr = CapabilityManager::open(&dir).expect("CapabilityManager should open");
    let mut mgr = PluginManager::new(host, cap_mgr, None, None);

    let wasm = compile_wat(ADAPTER_PLUGIN_WAT);
    let loaded = mgr
        .load_plugin(&wasm, std::path::Path::new("adapter.wasm"), &AutoApprove)
        .await
        .expect("load_plugin should succeed");

    let caps = GrantedCapabilities::new();
    mgr.initialise_plugin(&loaded, caps).await.expect("initialise should succeed");

    // Verify the plugin shows up in list_memory_adapters
    let adapters = mgr.list_memory_adapters().await;
    assert!(!adapters.is_empty(), "should have at least one adapter");
    assert!(adapters.contains(&"test-adapter".to_string()), "test-adapter should be listed");

    // Create a memory adapter from the plugin
    let store = mgr
        .create_memory_adapter("test-adapter")
        .await
        .expect("create_memory_adapter should succeed");
    // We can't easily assert trait object internals, but creation succeeded
    let _ = store;
}

// ---------------------------------------------------------------------------
// Capability enforcement during tool execution
// ---------------------------------------------------------------------------

/// WAT module that requires FilesystemRead capability.
const FS_PLUGIN_WAT: &str = r#"
(module
  (memory (export "memory") 1)
  (global (export "scratch_buffer") (mut i32) (i32.const 0))
  (global (export "scratch_buffer_size") i32 (i32.const 65536))
  (func (export "manifest") (result i64)
    i64.const 243
  )
  (func (export "init") (result i32)
    i32.const 0
  )
  (func (export "call_tool") (param i32 i32 i32 i32 i32 i32) (result i64)
    i64.const 0
  )
  (data (i32.const 0) "{\"id\":\"fs-plugin\",\"name\":\"FS Plugin\",\"version\":\"0.1.0\",\"description\":\"Needs FS\",\"abi_version\":1,\"capabilities_required\":[{\"FilesystemRead\":{\"globs\":[\"*.txt\"]}}],\"provides\":[{\"Tool\":{\"name\":\"read\",\"description\":\"Read file\",\"input_schema\":{}}}]}")
)
"#;

#[tokio::test]
async fn manifest_records_required_capabilities() {
    let host = test_host();
    let wasm = compile_wat(FS_PLUGIN_WAT);
    let loader = PluginLoader::new(host);

    let loaded =
        loader.load_from_bytes(&wasm, Path::new("fs.wasm")).await.expect("load should succeed");
    assert_eq!(loaded.manifest.capabilities_required.len(), 1);
    match &loaded.manifest.capabilities_required[0] {
        CapabilityRequest::FilesystemRead { globs } => {
            assert_eq!(globs, &vec!["*.txt".to_string()]);
        }
        other => panic!("expected FilesystemRead, got: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Resource Limits
// ---------------------------------------------------------------------------

/// WAT module that consumes fuel in a tight loop.
const FUEL_CONSUMER_WAT: &str = r#"
(module
  (memory (export "memory") 1)
  (global (export "scratch_buffer") (mut i32) (i32.const 0))
  (global (export "scratch_buffer_size") i32 (i32.const 65536))
  (func (export "manifest") (result i64)
    i64.const 222
  )
  (func (export "init") (result i32)
    i32.const 0
  )
  (func (export "call_tool") (param i32 i32 i32 i32 i32 i32) (result i64)
    ;; Infinite loop to exhaust fuel - pure computation, no memory access
    (local $i i32)
    (local.set $i (i32.const 0))
    (block $break
      (loop $continue
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        ;; Keep looping forever - will exhaust fuel
        (br_if $break (i32.eqz (i32.const 1)))
        (br $continue)
      )
    )
    i64.const 0
  )
  (data (i32.const 0) "{\"id\":\"fuel-consumer\",\"name\":\"Fuel Consumer\",\"version\":\"0.1.0\",\"description\":\"Consumes fuel\",\"abi_version\":1,\"capabilities_required\":[],\"provides\":[{\"Tool\":{\"name\":\"loop\",\"description\":\"Infinite loop\",\"input_schema\":{}}}]}")
)
"#;

#[tokio::test]
async fn fuel_limit_exhausted_returns_error() {
    let host = test_host();
    let wasm = compile_wat(FUEL_CONSUMER_WAT);
    let loader = PluginLoader::new(host);

    let loaded = loader.load_from_bytes(&wasm, Path::new("fuel.wasm")).await.expect("load");
    let caps = GrantedCapabilities::new();
    let mut plugin = loader.initialise(&loaded, caps).await.expect("init");

    let result = plugin.call_tool("loop", &serde_json::json!({})).await;

    // Should fail due to resource exhaustion (fuel, memory, or timeout)
    // The important security property is that the plugin cannot run indefinitely
    assert!(result.is_err(), "expected resource exhaustion error");
    let err_msg = result.unwrap_err().to_string();
    // Accept any error that indicates the plugin was terminated
    assert!(
        err_msg.contains("fuel")
            || err_msg.contains("energy")
            || err_msg.contains("memory")
            || err_msg.contains("violation")
            || err_msg.contains("timeout"),
        "error should indicate resource exhaustion: {err_msg}"
    );
}

/// WAT module that grows memory aggressively.
const MEMORY_GROWER_WAT: &str = r#"
(module
  (memory (export "memory") 1 1024)  ;; Start with 1 page, max 1024 pages (64MB)
  (global (export "scratch_buffer") (mut i32) (i32.const 0))
  (global (export "scratch_buffer_size") i32 (i32.const 65536))
  (func (export "manifest") (result i64)
    i64.const 216
  )
  (func (export "init") (result i32)
    i32.const 0
  )
  (func (export "call_tool") (param i32 i32 i32 i32 i32 i32) (result i64)
    ;; Try to grow memory to max
    (drop (memory.grow (i32.const 1000)))
    i64.const 0
  )
  (data (i32.const 0) "{\"id\":\"mem-grower\",\"name\":\"Memory Grower\",\"version\":\"0.1.0\",\"description\":\"Grows memory\",\"abi_version\":1,\"capabilities_required\":[],\"provides\":[{\"Tool\":{\"name\":\"grow\",\"description\":\"Grow memory\",\"input_schema\":{}}}]}")
)
"#;

#[tokio::test]
async fn memory_limit_enforced() {
    let host = test_host();
    let wasm = compile_wat(MEMORY_GROWER_WAT);
    let loader = PluginLoader::new(host);

    let loaded = loader.load_from_bytes(&wasm, Path::new("mem.wasm")).await.expect("load");
    let caps = GrantedCapabilities::new();
    let mut plugin = loader.initialise(&loaded, caps).await.expect("init");

    // Call the plugin - it will try to grow memory aggressively
    // The important security property is that the host doesn't crash and
    // memory limits are enforced (either by the plugin failing gracefully
    // or by the memory limit check)
    let result = plugin.call_tool("grow", &serde_json::json!({})).await;

    // Either succeeds (if memory.grow failed gracefully) or fails with memory limit/violation
    // The key is that the host remains stable
    match result {
        Ok(_) => {
            // Plugin completed without exceeding limits
        }
        Err(e) => {
            let err_msg = e.to_string();
            // Accept any error that indicates memory limits were enforced
            assert!(
                err_msg.contains("memory")
                    || err_msg.contains("violation")
                    || err_msg.contains("limit"),
                "error should indicate memory limit enforcement: {err_msg}"
            );
        }
    }
}

#[test]
#[allow(clippy::assertions_on_constants)]
fn fuel_constants_are_reasonable() {
    // Ensure fuel limits are set to reasonable values
    assert!(PluginHost::MAX_FUEL > 0, "MAX_FUEL should be positive");
    assert!(
        PluginHost::MAX_FUEL >= 1_000_000,
        "MAX_FUEL should be at least 1M for reasonable workloads"
    );
    assert!(PluginHost::EPOCH_DEADLINE > 0, "EPOCH_DEADLINE should be positive");
}

#[test]
#[allow(clippy::assertions_on_constants)]
fn memory_constants_are_reasonable() {
    // Ensure memory limits are set to reasonable values
    assert!(PluginHost::DEFAULT_MAX_MEMORY > 0, "DEFAULT_MAX_MEMORY should be positive");
    assert!(
        PluginHost::DEFAULT_MAX_MEMORY >= 64 * 1024 * 1024,
        "DEFAULT_MAX_MEMORY should be at least 64MB"
    );
    assert!(
        PluginHost::DEFAULT_MAX_MEMORY <= 512 * 1024 * 1024,
        "DEFAULT_MAX_MEMORY should not exceed 512MB to prevent host resource exhaustion"
    );
}

#[test]
#[allow(clippy::assertions_on_constants)]
fn module_size_limit_is_enforced() {
    // Ensure module size limit is reasonable
    assert!(PluginHost::MAX_WASM_MODULE_SIZE > 0, "MAX_WASM_MODULE_SIZE should be positive");
    assert!(
        PluginHost::MAX_WASM_MODULE_SIZE >= 10 * 1024 * 1024,
        "MAX_WASM_MODULE_SIZE should be at least 10MB"
    );
    assert!(
        PluginHost::MAX_WASM_MODULE_SIZE <= 100 * 1024 * 1024,
        "MAX_WASM_MODULE_SIZE should not exceed 100MB"
    );
}

// ---------------------------------------------------------------------------
// Host completion function
// ---------------------------------------------------------------------------

/// WAT module that imports `concerto.completion` and exports a
/// `test_completion` function for exercising the host call.
const COMPLETION_TEST_WAT: &str = r#"
(module
  (import "concerto" "completion" (func $host_completion (param i32 i32 i32 i32) (result i64)))
  (memory (export "memory") 2)
  (global (export "scratch_buffer") (mut i32) (i32.const 0))
  (global (export "scratch_buffer_size") i32 (i32.const 65536))
  (data (i32.const 0) "{\"id\":\"ct\",\"name\":\"ct\",\"version\":\"0.1.0\",\"description\":\"test\",\"abi_version\":1,\"capabilities_required\":[],\"provides\":[]}")
  (data (i32.const 256) "{\"model\":\"m\",\"messages\":[{\"role\":\"User\",\"content\":\"hi\"}]}")
  (func (export "manifest") (result i64)
    (i64.or
      (i64.shl (i64.const 0) (i64.const 32))
      (i64.const 119)
    )
  )
  (func (export "init") (result i32)
    i32.const 0
  )
  (func (export "test_completion") (result i64)
    (call $host_completion
      (i32.const 256)
      (i32.const 57)
      (i32.const 0)
      (i32.const 65536)
    )
  )
)
"#;

/// WAT tool plugin that imports `concerto.completion` and exports a canonical
/// 6-param `call_tool`. The `call_tool` export ignores its instrata and calls
/// the host `completion` import with a hardcoded request. Used by
/// `in_flight_cancellation_stops_host_call` to route a completion through the
/// tool bridge (`PluginTool::execute`).
const IN_FLIGHT_CANCEL_TOOL_WAT: &str = r#"
(module
  (import "concerto" "completion" (func $host_completion (param i32 i32 i32 i32) (result i64)))
  (memory (export "memory") 2)
  (global (export "scratch_buffer") (mut i32) (i32.const 0))
  (global (export "scratch_buffer_size") i32 (i32.const 65536))
  ;; Manifest at offset 0 (194 bytes)
  (data (i32.const 0) "{\"id\":\"inflight\",\"name\":\"inflight\",\"version\":\"0.1.0\",\"description\":\"test\",\"abi_version\":1,\"capabilities_required\":[],\"provides\":[{\"Tool\":{\"name\":\"echo\",\"description\":\"Echo\",\"input_schema\":{}}}]}")
  ;; Completion request at offset 512 (57 bytes)
  (data (i32.const 512) "{\"model\":\"m\",\"messages\":[{\"role\":\"User\",\"content\":\"hi\"}]}")
  (func (export "manifest") (result i64)
    (i64.or
      (i64.shl (i64.const 0) (i64.const 32))
      (i64.const 194)
    )
  )
  (func (export "init") (result i32)
    i32.const 0
  )
  (func (export "call_tool") (param i32 i32 i32 i32 i32 i32) (result i64)
    (call $host_completion
      (i32.const 512)
      (i32.const 57)
      (i32.const 0)
      (i32.const 65536)
    )
  )
)
"#;

/// Calling `completion` without a configured provider must return
/// `RESULT_ERROR` and set `last_error`.
#[tokio::test]
async fn completion_without_provider_returns_error() {
    let host = test_host();
    let loader = PluginLoader::new(host);

    let wasm = compile_wat(COMPLETION_TEST_WAT);
    let loaded = loader
        .load_from_bytes(&wasm, Path::new("completion_test.wasm"))
        .await
        .expect("load should succeed");
    assert_eq!(loaded.manifest.id, "ct");

    let caps = GrantedCapabilities::new();
    let mut plugin = loader.initialise(&loaded, caps).await.expect("init should succeed");

    // Call the test_completion export directly (it takes 0 params, calls the
    // host import internally with hardcoded args).
    let func = plugin
        .instance
        .get_typed_func::<(), i64>(&mut plugin.store, "test_completion")
        .expect("test_completion export should exist");

    let result = func.call_async(&mut plugin.store, ()).await.expect("call should not trap");

    assert_eq!(result, RESULT_ERROR, "expected RESULT_ERROR without a provider");

    // Verify the error message is set.
    let last_error = plugin.store.data().last_error.clone();
    assert!(last_error.is_some(), "last_error should be set");
    let err_msg = last_error.unwrap();
    assert!(
        err_msg.contains("no LLM provider"),
        "error should mention missing provider, got: {err_msg}"
    );
}

// ---------------------------------------------------------------------------
// In-flight cancellation (B1)
// ---------------------------------------------------------------------------

/// Proves that a token cancelled MID-FLIGHT reaches an in-flight host call.
///
/// A fake `LlmProvider` signals through a oneshot the moment its
/// `stream_completion` enters, then blocks on the token. A WAT tool plugin
/// calls `concerto.completion` (routed through `PluginTool::execute`, which
/// threads the caller's token into the plugin store via `set_cancel`). We:
///   1. start the tool call,
///   2. wait for the oneshot — proving the fake provider (and thus the
///      in-flight host call) is inside its await,
///   3. cancel the token while it is blocked,
///   4. assert the tool call returns promptly with `ToolError::Cancelled`
///      (bounded by a `tokio::time::timeout` so a regression cannot hang).
#[tokio::test]
async fn in_flight_cancellation_stops_host_call() {
    use concerto_core::error::ProviderError;
    use concerto_core::error::ToolError;
    use concerto_core::ids::Ulid;
    use concerto_core::policy::SimplePolicyEngine;
    use concerto_core::traits::policy::{AuditEntry, AuditLog};
    use concerto_core::traits::provider::{CompletionStream, LlmProvider};
    use concerto_core::traits::tool::Tool;
    use concerto_core::types::{CapabilitySet, CompletionRequest, SessionContext, TokenBudget};
    use concerto_core::CancellationToken;
    use concerto_plugins::tool_bridge::PluginTool;

    struct NoopAudit;
    #[async_trait::async_trait]
    impl AuditLog for NoopAudit {
        async fn record(
            &self,
            _entry: AuditEntry,
            _cancel: CancellationToken,
        ) -> Result<(), concerto_core::error::PolicyError> {
            Ok(())
        }
    }

    let policy = SimplePolicyEngine::new(vec![], Arc::new(NoopAudit));
    let session = SessionContext::new(Ulid::new(), std::path::PathBuf::from("/tmp/test-project"));

    // Fake provider that signals entry, then blocks on cancellation.
    let (entered_tx, mut entered_rx) = tokio::sync::mpsc::channel::<()>(1);
    struct GateProvider {
        entered_tx: tokio::sync::mpsc::Sender<()>,
    }
    #[async_trait::async_trait]
    impl LlmProvider for GateProvider {
        async fn stream_completion(
            &self,
            _request: CompletionRequest,
            cancel: CancellationToken,
        ) -> Result<CompletionStream, ProviderError> {
            let _ = self.entered_tx.send(()).await;
            // Block until the caller's token fires, then report cancellation.
            cancel.cancelled().await;
            Err(ProviderError::Cancelled)
        }
        fn context_capacity(&self, _model: &str) -> TokenBudget {
            TokenBudget::new(0, 0)
        }
        fn approximate_cost(&self, _tokens_in: u64, _tokens_out: u64) -> f64 {
            0.0
        }
        fn provider_name(&self) -> &'static str {
            "gate-provider"
        }
    }

    let host = Arc::new(PluginHost::new().expect("PluginHost should initialise"));
    let mut loader = PluginLoader::new(host);
    loader.set_provider(Some(Arc::new(GateProvider { entered_tx })));

    let wasm = compile_wat(IN_FLIGHT_CANCEL_TOOL_WAT);
    let loaded = loader
        .load_from_bytes(&wasm, Path::new("inflight.wasm"))
        .await
        .expect("load should succeed");
    let caps = GrantedCapabilities::new();
    let active = loader.initialise(&loaded, caps).await.expect("init should succeed");
    let plugin = Arc::new(tokio::sync::Mutex::new(active));

    let tool = Arc::new(PluginTool::new(
        "inflight".into(),
        plugin,
        "echo".into(),
        "Echo".into(),
        serde_json::json!({}),
        CapabilitySet::default(),
    ));

    let cancel = CancellationToken::new();
    let handle = {
        let tool = tool.clone();
        let cancel = cancel.clone();
        tokio::spawn(
            async move { tool.execute(serde_json::json!({}), &policy, &session, cancel).await },
        )
    };

    // 1) The in-flight host call must actually be reached and inside the
    //    provider's await before we cancel.
    tokio::time::timeout(std::time::Duration::from_secs(5), entered_rx.recv())
        .await
        .expect("fake provider must be entered within 5s")
        .expect("entered signal");

    // 2) Cancel mid-flight.
    cancel.cancel();

    // 3) The tool call must return promptly with a cancellation error.
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), handle)
        .await
        .expect("tool call must not hang after mid-flight cancellation")
        .expect("task must not panic");
    match result {
        Err(ToolError::Cancelled) => {}
        other => panic!("expected ToolError::Cancelled, got: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Dialect plugins (ADR-53)
// ---------------------------------------------------------------------------

/// Escape a Rust string for embedding as a WAT `(data ...)` segment: WAT
/// strings need `\"` for embedded quotes and `\\` for backslashes.
fn wat_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Build a WAT module that provides both a `Provider` and a `Dialect`
/// (ADR-53), with canned responses for every op.
///
/// `call_dialect` dispatches on the op name's length: 6 = `"render"`, 5 =
/// `"cache"`; any other op returns `{"error":"unsupported operation"}`.
/// `render_wire` / `cache_wire` are the *text* wire bodies the dialect
/// produces — the module JSON-encodes them because the dialect ABI returns a
/// JSON **string** (the wire body). `provider_response` is the canned JSON
/// `call_provider` returns for the `"complete"` op.
///
/// When `bad_dialect` is set, the `"render"` op returns the unsupported-op
/// error object instead, exercising the dialect error path end to end.
#[allow(clippy::too_many_arguments)]
fn make_dialect_provider_wat(
    manifest_json: &str,
    render_wire: &str,
    cache_wire: &str,
    provider_response: &str,
    bad_dialect: bool,
) -> Vec<u8> {
    let render_lit = serde_json::to_string(render_wire).expect("render_wire must serialize");
    let cache_lit = serde_json::to_string(cache_wire).expect("cache_wire must serialize");
    let error_obj = r#"{"error":"unsupported operation"}"#;

    let render_len = render_lit.len();
    let cache_len = cache_lit.len();
    let err_len = error_obj.len();
    let manifest_len = manifest_json.len();
    let response_len = provider_response.len();

    // A failing dialect makes the render op return the error object.
    let (render_target, render_out_len) =
        if bad_dialect { (1536, err_len) } else { (512, render_len) };

    let wat = format!(
        r#"(module
          (memory (export "memory") 2)
          (global (export "scratch_buffer") (mut i32) (i32.const 0))
          (global (export "scratch_buffer_size") i32 (i32.const 65536))
          (data (i32.const 512) "{render}")
          (data (i32.const 1024) "{cache}")
          (data (i32.const 1536) "{error}")
          (data (i32.const 2048) "{manifest}")
          (data (i32.const 3072) "{response}")
          (func (export "manifest") (result i64)
            (i64.or (i64.shl (i64.const 2048) (i64.const 32)) (i64.const {manifest_len}))
          )
          (func (export "init") (result i32) i32.const 0)
          ;; call_dialect(op_ptr, op_len, input_ptr, input_len, scratch_ptr, scratch_len)
          ;; op length 6 = "render", 5 = "cache", anything else = unsupported op.
          (func (export "call_dialect") (param i32 i32 i32 i32 i32 i32) (result i64)
            (if (i32.eq (local.get 1) (i32.const 6))
              (then
                (memory.copy (i32.const 0) (i32.const {render_target}) (i32.const {render_out_len}))
                (return (i64.or (i64.shl (i64.const 0) (i64.const 32)) (i64.const {render_out_len})))))
            (if (i32.eq (local.get 1) (i32.const 5))
              (then
                (memory.copy (i32.const 0) (i32.const 1024) (i32.const {cache_len}))
                (return (i64.or (i64.shl (i64.const 0) (i64.const 32)) (i64.const {cache_len})))))
            (memory.copy (i32.const 0) (i32.const 1536) (i32.const {err_len}))
            (i64.or (i64.shl (i64.const 0) (i64.const 32)) (i64.const {err_len}))
          )
          ;; call_provider restores the canned response at offset 0 (the
          ;; dialect ops have since copied their literals over it).
          (func (export "call_provider") (param i32 i32 i32 i32 i32 i32) (result i64)
            (memory.copy (i32.const 0) (i32.const 3072) (i32.const {response_len}))
            (i64.or (i64.shl (i64.const 0) (i64.const 32)) (i64.const {response_len}))
          )
        )"#,
        render = wat_escape(&render_lit),
        cache = wat_escape(&cache_lit),
        error = wat_escape(error_obj),
        manifest = wat_escape(manifest_json),
        response = wat_escape(provider_response),
        render_target = render_target,
        render_out_len = render_out_len,
        cache_len = cache_len,
        err_len = err_len,
        manifest_len = manifest_len,
        response_len = response_len,
    );
    wat::parse_str(&wat).expect("WAT should parse")
}

/// A provider plugin whose dialect rewrites the wire body with a
/// `custom_dialect` flag while preserving the original model and messages.
const DIALECT_MANIFEST: &str = r#"{"id":"dialect-provider","name":"Dialect Provider","version":"0.1.0","description":"custom wire","abi_version":1,"capabilities_required":[],"provides":[{"Provider":{"name":"dialect-provider","model":"dialect-model"}},{"Dialect":{"name":"custom-wire"}}]}"#;

/// The wire body the dialect produces for the canonical request.
const DIALECT_RENDER_WIRE: &str = r#"{"custom_dialect":true,"model":"dialect-model","messages":[{"role":"user","content":"hi"}]}"#;

/// Approval UI that grants every capability request (used to load test
/// plugins through the manager).
struct AutoApprove;

#[async_trait::async_trait]
impl concerto_plugins::capability::CapabilityApprovalUI for AutoApprove {
    async fn request(
        &self,
        _plugin: &concerto_api_types::plugin::PluginManifest,
        capabilities: &[concerto_api_types::plugin::CapabilityRequest],
    ) -> Result<
        Vec<concerto_plugins::capability::GrantDecision>,
        concerto_plugins::error::PluginError,
    > {
        Ok(vec![concerto_plugins::capability::GrantDecision::Granted; capabilities.len()])
    }
}

/// `DialectHost::render_chat_body` must hand the canonical request to the
/// plugin and return the produced wire body; a no-op cache must return the
/// body unchanged.
#[tokio::test]
async fn dialect_host_renders_custom_wire_and_cache_is_noop() {
    use concerto_core::CancellationToken;
    use concerto_plugins::dialect_host::DialectHost;

    let host = test_host();
    let wasm = make_dialect_provider_wat(
        DIALECT_MANIFEST,
        DIALECT_RENDER_WIRE,
        DIALECT_RENDER_WIRE,
        r#"{"content":"unused"}"#,
        false,
    );
    let loader = PluginLoader::new(host);
    let loaded = loader
        .load_from_bytes(&wasm, Path::new("dialect.wasm"))
        .await
        .expect("load should succeed");
    let caps = GrantedCapabilities::new();
    let active = loader.initialise(&loaded, caps).await.expect("init should succeed");
    let dialect = DialectHost::new(Arc::new(tokio::sync::Mutex::new(active)));

    let canonical = serde_json::json!({
        "model": "dialect-model",
        "messages": [{"role": "user", "content": "hi"}],
        "temperature": 0.7,
        "max_tokens": 512,
    });
    let token = CancellationToken::new();

    // render: canonical body → dialect wire body with the custom flag.
    let wire = dialect
        .render_chat_body(&canonical, "dialect-model", "if-present", &token)
        .await
        .expect("render should succeed");
    assert_eq!(wire, DIALECT_RENDER_WIRE, "render must return the dialect wire body");
    assert!(wire.contains("custom_dialect"), "wire should carry the dialect flag: {wire}");
    assert!(wire.contains("dialect-model"), "wire should preserve the model: {wire}");
    assert!(wire.contains("\"hi\""), "wire should preserve the message content: {wire}");

    // cache: a no-op dialect returns the body unchanged.
    let cached =
        dialect.apply_cache_breakpoints(&wire, &token).await.expect("cache should succeed");
    assert_eq!(cached, wire, "no-op cache must return the body unchanged");
}

/// A provider backed by a dialect plugin must stream a completion whose delta
/// is the dialect wire body (custom_dialect flag + preserved model/messages),
/// discovered and wired through the manager (Provider + Dialect descriptors).
#[tokio::test]
async fn dialect_provider_streams_wire_body_with_custom_dialect() {
    use concerto_core::types::CompletionRequest;
    use concerto_core::CancellationToken;
    use concerto_plugins::capability::CapabilityManager;
    use concerto_plugins::host::PluginHost;
    use concerto_plugins::manager::PluginManager;
    use futures::StreamExt;

    let dir = std::env::temp_dir().join("plugin_test_dialect_mgr");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let host = Arc::new(PluginHost::new().expect("PluginHost should initialise"));
    let cap_mgr = CapabilityManager::open(&dir).expect("CapabilityManager should open");
    let mut mgr = PluginManager::new(host, cap_mgr, None, None);

    let response = serde_json::json!({
        "content": DIALECT_RENDER_WIRE,
        "finish_reason": "stop",
    })
    .to_string();
    let wasm = make_dialect_provider_wat(
        DIALECT_MANIFEST,
        DIALECT_RENDER_WIRE,
        DIALECT_RENDER_WIRE,
        &response,
        false,
    );
    let loaded = mgr
        .load_plugin(&wasm, Path::new("dialect.wasm"), &AutoApprove)
        .await
        .expect("load_plugin should succeed");
    let caps = GrantedCapabilities::new();
    mgr.initialise_plugin(&loaded, caps).await.expect("initialise should succeed");

    // The plugin must be discoverable as a dialect provider.
    let dialects = mgr.list_dialects().await;
    assert!(
        dialects.contains(&"dialect-provider".to_string()),
        "dialect-provider should be listed: {dialects:?}"
    );

    // create_provider must wire the dialect in.
    let provider =
        mgr.create_provider("dialect-provider").await.expect("create_provider should succeed");
    assert_eq!(provider.provider_name(), "plugin:dialect-provider");

    let token = CancellationToken::new();
    let req =
        CompletionRequest { model: "dialect-model".into(), messages: vec![], ..Default::default() };
    let mut stream = provider.stream_completion(req, token).await.expect("stream should open");
    let chunk = tokio::time::timeout(std::time::Duration::from_secs(5), stream.next())
        .await
        .expect("first chunk within 5s")
        .expect("chunk present")
        .expect("chunk ok");
    assert!(chunk.is_final, "single-chunk stream must be final");
    assert_eq!(chunk.delta, DIALECT_RENDER_WIRE, "delta must be the dialect wire body");
    assert!(chunk.delta.contains("custom_dialect"), "delta should keep the dialect flag");
    assert!(chunk.delta.contains("dialect-model"), "delta should preserve the model");
    assert!(chunk.delta.contains("\"hi\""), "delta should preserve the message content");

    // The stream must end after the final chunk.
    let ended = tokio::time::timeout(std::time::Duration::from_secs(5), stream.next())
        .await
        .expect("stream end within 5s");
    assert!(ended.is_none(), "no further chunks expected");
}

/// A dialect that answers the render op with the unsupported-operation error
/// must fail the completion stream with that error surfaced to the caller.
#[tokio::test]
async fn dialect_rejects_unsupported_operation() {
    use concerto_core::error::ProviderError;
    use concerto_core::types::CompletionRequest;
    use concerto_core::CancellationToken;
    use concerto_plugins::capability::CapabilityManager;
    use concerto_plugins::host::PluginHost;
    use concerto_plugins::manager::PluginManager;

    let dir = std::env::temp_dir().join("plugin_test_dialect_err");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let host = Arc::new(PluginHost::new().expect("PluginHost should initialise"));
    let cap_mgr = CapabilityManager::open(&dir).expect("CapabilityManager should open");
    let mut mgr = PluginManager::new(host, cap_mgr, None, None);

    // A dialect whose render op returns the unsupported-operation error.
    let wasm = make_dialect_provider_wat(
        DIALECT_MANIFEST,
        DIALECT_RENDER_WIRE,
        DIALECT_RENDER_WIRE,
        r#"{"content":"unused"}"#,
        true,
    );
    let loaded = mgr
        .load_plugin(&wasm, Path::new("dialect_err.wasm"), &AutoApprove)
        .await
        .expect("load_plugin should succeed");
    let caps = GrantedCapabilities::new();
    mgr.initialise_plugin(&loaded, caps).await.expect("initialise should succeed");

    let provider =
        mgr.create_provider("dialect-provider").await.expect("create_provider should succeed");

    let token = CancellationToken::new();
    let req =
        CompletionRequest { model: "dialect-model".into(), messages: vec![], ..Default::default() };
    let result = provider.stream_completion(req, token).await;
    match result {
        Ok(_) => panic!("expected ProviderError::Other, got Ok"),
        Err(ProviderError::Other(msg)) => {
            assert!(
                msg.contains("unsupported operation"),
                "error should name the unsupported operation: {msg}"
            );
        }
        Err(other) => panic!("expected ProviderError::Other, got: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Completion heartbeat (ADR-53 §4)
// ---------------------------------------------------------------------------

/// Build a WAT provider module whose `call_provider` forwards the completion
/// to the configured host LLM provider via the `concerto.completion` import.
/// The hardcoded request lives at offset 512 (57 bytes, matching
/// [`COMPLETION_TEST_WAT`]).
fn make_heartbeat_provider_wat(manifest_json: &str) -> Vec<u8> {
    let manifest_len = manifest_json.len();
    let manifest_wat = wat_escape(manifest_json);
    let wat = format!(
        r#"(module
          (import "concerto" "completion" (func $host_completion (param i32 i32 i32 i32) (result i64)))
          (memory (export "memory") 2)
          (global (export "scratch_buffer") (mut i32) (i32.const 0))
          (global (export "scratch_buffer_size") i32 (i32.const 65536))
          (data (i32.const 0) "{manifest}")
          (data (i32.const 512) "{{\"model\":\"m\",\"messages\":[{{\"role\":\"User\",\"content\":\"hi\"}}]}}")
          (func (export "manifest") (result i64)
            (i64.or (i64.shl (i64.const 0) (i64.const 32)) (i64.const {manifest_len}))
          )
          (func (export "init") (result i32) i32.const 0)
          (func (export "call_provider") (param i32 i32 i32 i32 i32 i32) (result i64)
            (call $host_completion (i32.const 512) (i32.const 57) (i32.const 0) (i32.const 65536))
          )
        )"#,
        manifest = manifest_wat,
        manifest_len = manifest_len,
    );
    wat::parse_str(&wat).expect("WAT should parse")
}

/// A provider whose manifest requests `heartbeat_interval_secs: 1` must emit
/// a non-terminal keepalive chunk while a slow plugin completion is awaited,
/// then the terminal chunk once the completion resolves (ADR-53 §4).
#[tokio::test]
async fn heartbeat_provider_emits_keepalive_while_call_pending() {
    use concerto_core::error::ProviderError;
    use concerto_core::traits::provider::{CompletionStream, LlmProvider};
    use concerto_core::types::{CompletionChunk, CompletionRequest, TokenBudget};
    use concerto_core::CancellationToken;
    use concerto_plugins::capability::CapabilityManager;
    use concerto_plugins::host::PluginHost;
    use concerto_plugins::manager::PluginManager;
    use futures::StreamExt;
    use tokio::sync::Notify;

    // Fake provider that holds a completion in flight until the test releases
    // the gate with `notify_one`, then emits a single final chunk.
    struct GateProvider {
        notify: Arc<Notify>,
    }
    #[async_trait::async_trait]
    impl LlmProvider for GateProvider {
        async fn stream_completion(
            &self,
            _request: CompletionRequest,
            cancel: CancellationToken,
        ) -> Result<CompletionStream, ProviderError> {
            tokio::select! {
                _ = self.notify.notified() => {}
                _ = cancel.cancelled() => return Err(ProviderError::Cancelled),
            }
            let chunk = CompletionChunk {
                delta: "done".into(),
                reasoning: None,
                tool_call: None,
                is_final: true,
                usage: None,
            };
            let stream: CompletionStream =
                Box::pin(futures::stream::once(async move { Ok(chunk) }));
            Ok(stream)
        }
        fn context_capacity(&self, _model: &str) -> TokenBudget {
            TokenBudget::new(8192, 2048)
        }
        fn approximate_cost(&self, _tokens_in: u64, _tokens_out: u64) -> f64 {
            0.0
        }
        fn provider_name(&self) -> &'static str {
            "hb-gate"
        }
    }

    let notify = Arc::new(Notify::new());
    let dir = std::env::temp_dir().join("plugin_test_heartbeat");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let host = Arc::new(PluginHost::new().expect("PluginHost should initialise"));
    let cap_mgr = CapabilityManager::open(&dir).expect("CapabilityManager should open");
    let mut mgr = PluginManager::new(
        host,
        cap_mgr,
        None,
        Some(Arc::new(GateProvider { notify: notify.clone() })),
    );

    // Manifest requests a 1s completion keepalive.
    let manifest = r#"{"id":"hb-provider","name":"HB","version":"0.1.0","description":"heartbeat","abi_version":1,"capabilities_required":[],"provides":[{"Provider":{"name":"hb","model":"hb-model","heartbeat_interval_secs":1}}]}"#;
    let wasm = make_heartbeat_provider_wat(manifest);
    let loaded = mgr
        .load_plugin(&wasm, Path::new("hb.wasm"), &AutoApprove)
        .await
        .expect("load_plugin should succeed");
    let caps = GrantedCapabilities::new();
    mgr.initialise_plugin(&loaded, caps).await.expect("initialise should succeed");

    let provider =
        mgr.create_provider("hb-provider").await.expect("create_provider should succeed");

    let token = CancellationToken::new();
    let req =
        CompletionRequest { model: "hb-model".into(), messages: vec![], ..Default::default() };
    let mut stream = provider.stream_completion(req, token).await.expect("stream should open");

    // 1) The gate holds the plugin completion; the 1s heartbeat must emit a
    //    non-terminal keepalive before the completion resolves.
    let saw_keepalive = tokio::time::timeout(std::time::Duration::from_secs(4), async {
        let chunk = stream
            .next()
            .await
            .expect("stream must not end while the call is pending")
            .expect("chunk ok");
        assert!(!chunk.is_final, "no final chunk while the gate is held");
        assert!(chunk.delta.is_empty(), "keepalive must carry an empty delta");
    })
    .await
    .is_ok();
    assert!(saw_keepalive, "expected a keepalive chunk while the plugin call is pending");

    // 2) Release the gate; the completion resolves with the terminal chunk.
    notify.notify_one();
    let final_chunk = tokio::time::timeout(std::time::Duration::from_secs(4), async {
        loop {
            let item = stream.next().await.expect("stream ended before the final chunk");
            match item {
                Ok(c) if c.is_final => return c,
                Ok(_) => continue, // a keepalive may still be in flight
                Err(e) => panic!("stream error: {e}"),
            }
        }
    })
    .await
    .expect("final chunk within 4s");
    assert!(final_chunk.is_final, "the last chunk must be final");
    assert_eq!(final_chunk.delta, "done");
}
