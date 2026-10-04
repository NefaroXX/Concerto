//! Regression contracts for the extension security review.
use concerto_api_types::plugin::{CapabilityRequest, PluginManifest};
use concerto_core::{
    error::ToolError,
    policy::SimplePolicyEngine,
    traits::{
        policy::{AuditEntry, AuditLog},
        PolicyEngine, Tool,
    },
    types::{CapabilitySet, Condition, PolicyRule, SessionContext, ToolOutput, ToolRegistry},
    CancellationToken, ToolExecutor,
};
use concerto_plugins::{
    capability::{
        check_path_allowed, CapabilityApprovalUI, CapabilityDiscriminant, CapabilityManager,
        CapabilityScope, GrantDecision, GrantedCapabilities,
    },
    error::PluginError,
    host::{PluginExecutionContext, PluginHost},
    loader::PluginLoader,
    manager::PluginManager,
};
use std::{path::Path, sync::Arc};

fn module(caps: Vec<CapabilityRequest>, body: &str) -> Vec<u8> {
    let manifest = PluginManifest {
        id: "regression".into(),
        name: "Regression".into(),
        version: "1".into(),
        description: "test".into(),
        abi_version: 1,
        capabilities_required: caps,
        provides: vec![],
    };
    let json = serde_json::to_string(&manifest).unwrap();
    wat::parse_str(format!(
        r#"(module
      (memory (export "memory") 2)
      (global (export "scratch_buffer") i32 (i32.const 0))
      (global (export "scratch_buffer_size") i32 (i32.const 65536))
      (data (i32.const 256) "{}")
      (data (i32.const 0) "true")
      (func (export "manifest") (result i64) i64.const {})
      (func (export "init") (result i32) i32.const 0)
      (func (export "call_tool") (param i32 i32 i32 i32 i32 i32) (result i64) {} i64.const 4))"#,
        json.replace('"', "\\\""),
        (256u64 << 32) | json.len() as u64,
        body
    ))
    .unwrap()
}
struct EmptyApproval;
#[async_trait::async_trait]
impl CapabilityApprovalUI for EmptyApproval {
    async fn request(
        &self,
        _: &PluginManifest,
        _: &[CapabilityRequest],
    ) -> Result<Vec<GrantDecision>, PluginError> {
        Ok(vec![])
    }
}
struct NoopAudit;
#[async_trait::async_trait]
impl AuditLog for NoopAudit {
    async fn record(
        &self,
        _: AuditEntry,
        _: CancellationToken,
    ) -> Result<(), concerto_core::error::PolicyError> {
        Ok(())
    }
}
/// Verifies init with audit and registration are safe inside a current-thread Tokio runtime.
#[tokio::test]
async fn async_startup_with_audit_does_not_panic() {
    let dir = tempfile::tempdir().unwrap();
    let host = Arc::new(PluginHost::new().unwrap());
    let mut manager =
        PluginManager::new(host, CapabilityManager::open(dir.path()).unwrap(), None, None);
    manager.set_audit_log(Arc::new(NoopAudit));
    let loaded = manager
        .load_plugin(&module(vec![], ""), Path::new("regression.wasm"), &EmptyApproval)
        .await
        .unwrap();
    manager.initialise_plugin(&loaded, GrantedCapabilities::new()).await.unwrap();
    manager.register_tools("regression", &mut Default::default()).unwrap();
}
/// Verifies missing approval decisions cannot authorize a required capability.
#[tokio::test]
async fn incomplete_approval_fails_closed() {
    let dir = tempfile::tempdir().unwrap();
    let host = Arc::new(PluginHost::new().unwrap());
    let mut manager =
        PluginManager::new(host, CapabilityManager::open(dir.path()).unwrap(), None, None);
    let result = manager
        .load_plugin(
            &module(vec![CapabilityRequest::FilesystemWrite { globs: vec![] }], ""),
            Path::new("regression.wasm"),
            &EmptyApproval,
        )
        .await;
    assert!(result.is_err());
}
/// Verifies memory.grow is refused during the call, before allocating beyond 64 MiB.
#[tokio::test]
async fn guest_memory_growth_is_limited_during_execution() {
    let host = Arc::new(PluginHost::new().unwrap());
    let loader = PluginLoader::new(host);
    let wasm = module(vec![], "i32.const 1024 memory.grow i32.const -1 i32.ne if unreachable end");
    let loaded = loader.load_from_bytes(&wasm, Path::new("regression.wasm")).await.unwrap();
    let mut plugin = loader.initialise(&loaded, GrantedCapabilities::new()).await.unwrap();
    assert_eq!(plugin.call_tool("test", &serde_json::json!({})).await.unwrap(), true);
}
/// Verifies idle time does not consume the next call's epoch budget.
#[tokio::test]
async fn each_call_resets_the_epoch_deadline() {
    let host = Arc::new(PluginHost::new().unwrap());
    let loader = PluginLoader::new(host.clone());
    let loaded =
        loader.load_from_bytes(&module(vec![], ""), Path::new("regression.wasm")).await.unwrap();
    let mut plugin = loader.initialise(&loaded, GrantedCapabilities::new()).await.unwrap();
    for _ in 0..=PluginHost::EPOCH_DEADLINE {
        host.engine().increment_epoch();
    }
    assert_eq!(plugin.call_tool("test", &serde_json::json!({})).await.unwrap(), true);
}

struct PersistApproval;
#[async_trait::async_trait]
impl CapabilityApprovalUI for PersistApproval {
    async fn request(
        &self,
        _: &PluginManifest,
        capabilities: &[CapabilityRequest],
    ) -> Result<Vec<GrantDecision>, PluginError> {
        Ok(vec![GrantDecision::GrantedPersistent; capabilities.len()])
    }
}
/// Verifies a retained manager re-reads durable revocations rather than restoring stale grants.
#[tokio::test]
async fn external_revocation_survives_next_runtime_load() {
    let dir = tempfile::tempdir().unwrap();
    let host = Arc::new(PluginHost::new().unwrap());
    let mut manager =
        PluginManager::new(host, CapabilityManager::open(dir.path()).unwrap(), None, None);
    let wasm = module(vec![CapabilityRequest::FilesystemRead { globs: vec![] }], "");
    manager.load_plugin(&wasm, Path::new("regression.wasm"), &PersistApproval).await.unwrap();
    CapabilityManager::open(dir.path()).unwrap().revoke_plugin("regression").unwrap();
    let result = manager
        .load_plugin(
            &wasm,
            Path::new("regression.wasm"),
            &concerto_plugins::capability::DenyUnapproved,
        )
        .await;
    assert!(result.is_err(), "revocation cannot be undone by stale manager state");
}
/// Verifies a changed binary cannot reuse a prior binary's approval.
#[tokio::test]
async fn changed_binary_requires_new_approval() {
    let dir = tempfile::tempdir().unwrap();
    let host = Arc::new(PluginHost::new().unwrap());
    let mut manager =
        PluginManager::new(host, CapabilityManager::open(dir.path()).unwrap(), None, None);
    let caps = vec![CapabilityRequest::FilesystemRead { globs: vec![] }];
    manager
        .load_plugin(&module(caps.clone(), ""), Path::new("regression.wasm"), &PersistApproval)
        .await
        .unwrap();
    assert!(manager
        .load_plugin(
            &module(caps, "nop"),
            Path::new("regression.wasm"),
            &concerto_plugins::capability::DenyUnapproved
        )
        .await
        .is_err());
}
fn host_module(import: &str, export: &str, data: &str) -> Vec<u8> {
    let json = r#"{"id":"host-test","name":"Host","version":"1","description":"test","abi_version":1,"capabilities_required":[],"provides":[]}"#;
    wat::parse_str(format!(
        r#"(module {}
      (memory (export "memory") 2)
      (global (export "scratch_buffer") i32 (i32.const 65536))
      (global (export "scratch_buffer_size") i32 (i32.const 65536))
      (data (i32.const 256) "{}")
      (data (i32.const 1024) "{}")
      (func (export "manifest") (result i64) i64.const {})
      (func (export "init") (result i32) i32.const 0)
      {})"#,
        import,
        json.replace('"', "\\\""),
        data.replace('\\', "\\\\").replace('"', "\\\""),
        (256u64 << 32) | json.len() as u64,
        export
    ))
    .unwrap()
}
/// Verifies plugin writes use the shared diff/undo overlay and policy denial prevents another effect.
#[tokio::test]
async fn plugin_writes_use_virtual_fs_and_nested_policy() {
    use concerto_core::{
        policy::SimplePolicyEngine,
        types::{Condition, PolicyRule, SessionContext, ToolRegistry},
        ToolExecutor,
    };
    use concerto_plugins::{
        capability::{CapabilityDiscriminant, CapabilityScope},
        host::{PluginExecutionContext, PluginHostContext},
    };
    let dir = tempfile::tempdir().unwrap();
    let root = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    let path = root.join("staged.txt");
    let vfs = Arc::new(std::sync::Mutex::new(concerto_tools::virtual_fs::VirtualFs::default()));
    for allow in [true, false] {
        let mut registry = ToolRegistry::default();
        registry.register(Box::new(concerto_tools::filesystem::FilesystemTool::new_shared(
            root.clone(),
            vfs.clone(),
        )));
        let rules = if allow {
            vec![PolicyRule::AutoApprove(Condition::Always)]
        } else {
            vec![PolicyRule::AutoDeny(Condition::Always)]
        };
        let executor = Arc::new(ToolExecutor::new(
            Arc::new(registry),
            Arc::new(SimplePolicyEngine::new(rules, Arc::new(NoopAudit))),
        ));
        let context: PluginHostContext =
            Arc::new(std::sync::RwLock::new(Some(PluginExecutionContext {
                executor: Arc::downgrade(&executor),
                session: SessionContext::new(
                    concerto_core::ids::Ulid::new(),
                    dir.path().to_path_buf(),
                ),
            })));
        let mut loader = PluginLoader::new(Arc::new(PluginHost::new().unwrap()));
        loader.set_execution_context(context);
        let wasm = host_module(
            r#"(import "concerto" "write_file" (func $write (param i32 i32 i32 i32) (result i32)))"#,
            &format!(
                r#"(func (export "effect") (result i32) i32.const 1024 i32.const {} i32.const 0 i32.const 3 call $write)"#,
                path.as_str().len()
            ),
            path.as_str(),
        );
        let loaded = loader.load_from_bytes(&wasm, Path::new("host-test.wasm")).await.unwrap();
        let mut caps = GrantedCapabilities::new();
        caps.set_root(dir.path().to_path_buf());
        caps.grant_session(CapabilityDiscriminant::FilesystemWrite, CapabilityScope::default());
        let mut plugin = loader.initialise(&loaded, caps).await.unwrap();
        let func = plugin.instance.get_typed_func::<(), i32>(&mut plugin.store, "effect").unwrap();
        let result = func.call_async(&mut plugin.store, ()).await.unwrap();
        assert_eq!(result, if allow { 0 } else { -1 });
        assert!(vfs.lock().unwrap().exists(&path), "shared overlay must track the write");
        assert_eq!(std::fs::read(&path).unwrap(), vec![0, 0, 0]);
    }
}
struct DenialAudit(std::sync::atomic::AtomicBool);
#[async_trait::async_trait]
impl AuditLog for DenialAudit {
    async fn record(
        &self,
        entry: AuditEntry,
        _: CancellationToken,
    ) -> Result<(), concerto_core::error::PolicyError> {
        if entry.verdict.eq_ignore_ascii_case("deny") {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        Ok(())
    }
}
/// Verifies global network denial still blocks a plugin with an approved outbound capability.
#[tokio::test]
async fn plugin_http_obeys_global_network_denial() {
    use concerto_core::{
        policy::SimplePolicyEngine,
        types::{Condition, PolicyRule, SessionContext, ToolRegistry},
        ToolExecutor,
    };
    use concerto_plugins::{
        capability::{CapabilityDiscriminant, CapabilityScope},
        host::PluginExecutionContext,
    };
    let audit = Arc::new(DenialAudit(std::sync::atomic::AtomicBool::new(false)));
    let executor = Arc::new(ToolExecutor::new(
        Arc::new(ToolRegistry::default()),
        Arc::new(SimplePolicyEngine::new(
            vec![PolicyRule::DenyNetworkEgress(Condition::Always)],
            audit.clone(),
        )),
    ));
    let context = Arc::new(std::sync::RwLock::new(Some(PluginExecutionContext {
        executor: Arc::downgrade(&executor),
        session: SessionContext::new(
            concerto_core::ids::Ulid::new(),
            std::env::current_dir().unwrap(),
        ),
    })));
    let mut loader = PluginLoader::new(Arc::new(PluginHost::new().unwrap()));
    loader.set_execution_context(context);
    let url = "http://127.0.0.1:9/blocked";
    let wasm = host_module(
        r#"(import "concerto" "http_get" (func $get (param i32 i32 i32 i32) (result i64)))"#,
        &format!(
            r#"(func (export "effect") (result i64) i32.const 1024 i32.const {} i32.const 65536 i32.const 65536 call $get)"#,
            url.len()
        ),
        url,
    );
    let loaded = loader.load_from_bytes(&wasm, Path::new("host-test.wasm")).await.unwrap();
    let mut caps = GrantedCapabilities::new();
    caps.grant_session(CapabilityDiscriminant::NetworkOutbound, CapabilityScope::default());
    let mut plugin = loader.initialise(&loaded, caps).await.unwrap();
    let func = plugin.instance.get_typed_func::<(), i64>(&mut plugin.store, "effect").unwrap();
    assert_eq!(
        func.call_async(&mut plugin.store, ()).await.unwrap(),
        concerto_plugins::guest_abi::RESULT_ERROR
    );
    // The policy sink is reached before opening any network connection.
    assert!(
        audit.0.load(std::sync::atomic::Ordering::SeqCst),
        "global deny must decide the concrete HTTP operation"
    );
}

// ---------------------------------------------------------------------------
// Work-order W7 regression contracts.
// ---------------------------------------------------------------------------

/// Test double for the `filesystem` tool.
///
/// Records the exact path string each host-function effect forwards to the
/// executor and the session it was invoked under, and can omit `content` from
/// its output so missing-content handling is observable. This lets the W7
/// contracts assert path resolution and per-run context binding without a real
/// on-disk write.
struct RecordingFilesystem {
    paths: Arc<std::sync::Mutex<Vec<String>>>,
    sessions: Arc<std::sync::Mutex<Vec<String>>>,
    include_content: bool,
}

#[async_trait::async_trait]
impl Tool for RecordingFilesystem {
    fn name(&self) -> &str {
        "filesystem"
    }

    fn description(&self) -> &str {
        "recording filesystem test double"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }

    fn capability_requirements(&self) -> CapabilitySet {
        CapabilitySet::default()
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        _policy: &dyn PolicyEngine,
        session: &SessionContext,
        _cancel: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let path =
            input.get("path").and_then(serde_json::Value::as_str).unwrap_or_default().to_string();
        self.paths.lock().unwrap().push(path.clone());
        self.sessions.lock().unwrap().push(session.session_id.to_string());
        let mut data = serde_json::json!({ "path": path });
        if self.include_content {
            data["content"] = serde_json::Value::String("ok".into());
        }
        Ok(ToolOutput { summary: "recording".into(), data })
    }
}

/// Defect 1: `GrantedCapabilities::new()` leaves `root_dir` unset. Path checks
/// must fail closed — no root configured means no path is permitted — instead
/// of skipping root confinement and glob scope.
#[test]
fn default_capabilities_without_root_dir_deny_paths() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("secret.txt");
    std::fs::write(&file, "classified").unwrap();

    let mut caps = GrantedCapabilities::new();
    caps.grant_session(CapabilityDiscriminant::FilesystemRead, CapabilityScope::default());

    let result = check_path_allowed(&caps, "p", file.to_str().unwrap(), false);
    let err = result.expect_err("no-root grants must deny every path");
    assert!(
        err.to_string().contains("no root directory configured"),
        "expected fail-closed no-root denial, got: {err}"
    );
}

/// Defect 2: host file effects must forward the path that was actually
/// canonicalized for confinement (not the raw caller string), so a symlink or
/// `.`/`..` component cannot be swapped between validation and execution.
#[tokio::test]
async fn host_file_effects_pass_resolved_paths_to_executor() {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    std::fs::create_dir_all(root.join("sub")).unwrap();
    std::fs::write(root.join("real.txt"), "x").unwrap();

    // Read through `sub/../real.txt`; write a new file through `sub/./new.txt`.
    let read_str = root.join("sub").join("..").join("real.txt").to_str().unwrap().to_string();
    let write_str = root.join("sub").join(".").join("new.txt").to_str().unwrap().to_string();

    let paths = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sessions = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut registry = ToolRegistry::default();
    registry.register(Box::new(RecordingFilesystem {
        paths: paths.clone(),
        sessions,
        include_content: true,
    }));
    let executor = Arc::new(ToolExecutor::new(
        Arc::new(registry),
        Arc::new(SimplePolicyEngine::new(
            vec![PolicyRule::AutoApprove(Condition::Always)],
            Arc::new(NoopAudit),
        )),
    ));
    let context = Arc::new(std::sync::RwLock::new(Some(PluginExecutionContext {
        executor: Arc::downgrade(&executor),
        session: SessionContext::new(concerto_core::ids::Ulid::new(), root.clone()),
    })));
    let mut loader = PluginLoader::new(Arc::new(PluginHost::new().unwrap()));
    loader.set_execution_context(context);

    let write_off = 1024 + read_str.len();
    let wasm = host_module(
        r#"(import "concerto" "read_file" (func $read (param i32 i32 i32 i32) (result i64)))
           (import "concerto" "write_file" (func $write (param i32 i32 i32 i32) (result i32)))"#,
        &format!(
            r#"(func (export "read_effect") (result i64) i32.const 1024 i32.const {rl} i32.const 65536 i32.const 65536 call $read)
               (func (export "write_effect") (result i32) i32.const {wo} i32.const {wl} i32.const 256 i32.const 1 call $write)"#,
            rl = read_str.len(),
            wo = write_off,
            wl = write_str.len(),
        ),
        &format!("{read_str}{write_str}"),
    );
    let loaded = loader.load_from_bytes(&wasm, Path::new("host-test.wasm")).await.unwrap();
    let mut caps = GrantedCapabilities::new();
    caps.set_root(root.clone());
    caps.grant_session(CapabilityDiscriminant::FilesystemRead, CapabilityScope::default());
    caps.grant_session(CapabilityDiscriminant::FilesystemWrite, CapabilityScope::default());
    let mut plugin = loader.initialise(&loaded, caps).await.unwrap();

    let read_fn =
        plugin.instance.get_typed_func::<(), i64>(&mut plugin.store, "read_effect").unwrap();
    read_fn.call_async(&mut plugin.store, ()).await.unwrap();
    let write_fn =
        plugin.instance.get_typed_func::<(), i32>(&mut plugin.store, "write_effect").unwrap();
    write_fn.call_async(&mut plugin.store, ()).await.unwrap();

    let expected_read = std::fs::canonicalize(root.join("real.txt")).unwrap();
    let expected_write = std::fs::canonicalize(root.join("sub")).unwrap().join("new.txt");
    assert_eq!(
        paths.lock().unwrap().clone(),
        vec![
            expected_read.to_string_lossy().into_owned(),
            expected_write.to_string_lossy().into_owned()
        ],
        "executor must receive the resolved canonical paths, never the raw caller strings"
    );
}

/// Defect 3: a tool result without a `content` field must surface as a host
/// error (RESULT_ERROR + `last_error`), never as a successful empty file.
#[tokio::test]
async fn host_read_file_missing_content_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    std::fs::write(root.join("f.txt"), "x").unwrap();
    let path_str = root.join("f.txt").to_str().unwrap().to_string();

    let mut registry = ToolRegistry::default();
    registry.register(Box::new(RecordingFilesystem {
        paths: Arc::new(std::sync::Mutex::new(Vec::new())),
        sessions: Arc::new(std::sync::Mutex::new(Vec::new())),
        include_content: false,
    }));
    let executor = Arc::new(ToolExecutor::new(
        Arc::new(registry),
        Arc::new(SimplePolicyEngine::new(
            vec![PolicyRule::AutoApprove(Condition::Always)],
            Arc::new(NoopAudit),
        )),
    ));
    let context = Arc::new(std::sync::RwLock::new(Some(PluginExecutionContext {
        executor: Arc::downgrade(&executor),
        session: SessionContext::new(concerto_core::ids::Ulid::new(), root.clone()),
    })));
    let mut loader = PluginLoader::new(Arc::new(PluginHost::new().unwrap()));
    loader.set_execution_context(context);
    let wasm = host_module(
        r#"(import "concerto" "read_file" (func $read (param i32 i32 i32 i32) (result i64)))"#,
        &format!(
            r#"(func (export "effect") (result i64) i32.const 1024 i32.const {len} i32.const 65536 i32.const 65536 call $read)"#,
            len = path_str.len(),
        ),
        &path_str,
    );
    let loaded = loader.load_from_bytes(&wasm, Path::new("host-test.wasm")).await.unwrap();
    let mut caps = GrantedCapabilities::new();
    caps.set_root(root.clone());
    caps.grant_session(CapabilityDiscriminant::FilesystemRead, CapabilityScope::default());
    let mut plugin = loader.initialise(&loaded, caps).await.unwrap();

    let func = plugin.instance.get_typed_func::<(), i64>(&mut plugin.store, "effect").unwrap();
    let result = func.call_async(&mut plugin.store, ()).await.unwrap();
    assert_eq!(
        result,
        concerto_plugins::guest_abi::RESULT_ERROR,
        "missing content must not be reported as a successful empty read"
    );
    assert!(
        plugin.store.data().last_error.is_some(),
        "missing content must record last_error for the guest"
    );
}

/// Defect 4: each activation captures the run context it was initialised with.
/// Starting run B must not revoke an in-flight effect belonging to run A, and
/// run B must never fall back to run A's context once A has ended.
#[tokio::test]
async fn activation_contexts_are_isolated_per_run() {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    std::fs::write(root.join("f.txt"), "x").unwrap();
    let path_str = root.join("f.txt").to_str().unwrap().to_string();

    let sessions = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut registry = ToolRegistry::default();
    registry.register(Box::new(RecordingFilesystem {
        paths: Arc::new(std::sync::Mutex::new(Vec::new())),
        sessions: sessions.clone(),
        include_content: true,
    }));
    let executor = Arc::new(ToolExecutor::new(
        Arc::new(registry),
        Arc::new(SimplePolicyEngine::new(
            vec![PolicyRule::AutoApprove(Condition::Always)],
            Arc::new(NoopAudit),
        )),
    ));

    let session_a = concerto_core::ids::Ulid::new();
    let session_b = concerto_core::ids::Ulid::new();
    let ctx_a = Arc::new(std::sync::RwLock::new(Some(PluginExecutionContext {
        executor: Arc::downgrade(&executor),
        session: SessionContext::new(session_a, root.clone()),
    })));
    let ctx_b = Arc::new(std::sync::RwLock::new(Some(PluginExecutionContext {
        executor: Arc::downgrade(&executor),
        session: SessionContext::new(session_b, root.clone()),
    })));

    let mut loader = PluginLoader::new(Arc::new(PluginHost::new().unwrap()));
    let wasm = host_module(
        r#"(import "concerto" "read_file" (func $read (param i32 i32 i32 i32) (result i64)))"#,
        &format!(
            r#"(func (export "effect") (result i64) i32.const 1024 i32.const {len} i32.const 65536 i32.const 65536 call $read)"#,
            len = path_str.len(),
        ),
        &path_str,
    );
    let loaded = loader.load_from_bytes(&wasm, Path::new("host-test.wasm")).await.unwrap();
    let mut caps = GrantedCapabilities::new();
    caps.set_root(root.clone());
    caps.grant_session(CapabilityDiscriminant::FilesystemRead, CapabilityScope::default());

    loader.set_execution_context(ctx_a.clone());
    let mut plugin_a = loader.initialise(&loaded, caps.clone()).await.unwrap();
    loader.set_execution_context(ctx_b.clone());
    let mut plugin_b = loader.initialise(&loaded, caps).await.unwrap();

    // Run A's effect is still in flight when run B starts: it must resolve
    // against A's session, not B's.
    let func_a =
        plugin_a.instance.get_typed_func::<(), i64>(&mut plugin_a.store, "effect").unwrap();
    func_a.call_async(&mut plugin_a.store, ()).await.unwrap();
    // Run B resolves against B's session.
    let func_b =
        plugin_b.instance.get_typed_func::<(), i64>(&mut plugin_b.store, "effect").unwrap();
    func_b.call_async(&mut plugin_b.store, ()).await.unwrap();

    // Run A ends. B must keep using its own context, never A's stale one.
    *ctx_a.write().unwrap() = None;
    func_b.call_async(&mut plugin_b.store, ()).await.unwrap();

    assert_eq!(
        sessions.lock().unwrap().clone(),
        vec![session_a.to_string(), session_b.to_string(), session_b.to_string()],
        "each activation must resolve against its own run context"
    );
}
