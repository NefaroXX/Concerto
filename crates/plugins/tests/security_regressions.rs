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
