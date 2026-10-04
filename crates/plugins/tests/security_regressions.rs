//! Regression contracts for the extension security review.
use concerto_api_types::plugin::{CapabilityRequest, PluginManifest};
use concerto_core::{
    traits::policy::{AuditEntry, AuditLog},
    CancellationToken,
};
use concerto_plugins::{
    capability::{CapabilityApprovalUI, CapabilityManager, GrantDecision, GrantedCapabilities},
    error::PluginError,
    host::PluginHost,
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
