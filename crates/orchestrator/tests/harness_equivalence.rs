//! H13 equivalence acceptance: the same request through each covered adapter
//! produces equivalent semantics and equivalent evidence.
//!
//! Covered adapters proven here (see `docs/harness-access.md`):
//!
//! - **In-process service** — a [`HarnessService`] over the real executor,
//!   exercised for `memory.recall`, `environment.describe`, and the checkpoint
//!   lifecycle.
//! - **Standalone service** (the seam the CLI and desktop both call) — built
//!   by `build_standalone_harness`; `environment.describe` must be stable
//!   across builds and the checkpoint lifecycle must round-trip real bytes.
//!
//! The CLI adapter is proven end-to-end in `concerto-cli`'s
//! `dispatch::harness` tests (the same service payload, serialized verbatim),
//! and the desktop adapter in `concerto-desktop`'s `views::harness` tests (the
//! panel renders the same `HarnessInvocation`). Supervised recall evidence is
//! proven in `gate_proxy`'s wire-reconstruction test.
//!
//! Fixtures live under `tests/harness_equivalence_work/` (disk-backed, never
//! `/tmp`).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use concerto_core::error::MemoryError;
use concerto_core::executor::ToolExecutor;
use concerto_core::memory::{
    ChunkType, Confidence, EvidentChunk, FactStatusView, MemoryChunk, MemoryNamespace, MemoryQuery,
    ProjectId, RecallSource,
};
use concerto_core::policy::SimplePolicyEngine;
use concerto_core::traits::memory::MemoryStore;
use concerto_core::traits::policy::{AuditEntry, AuditLog};
use concerto_core::types::{Condition, PolicyRule, SessionContext, ToolRegistry};
use concerto_core::CancellationToken;
use concerto_orchestrator::harness_memory::{MemoryRecallTool, MEMORY_RECALL_TOOL_NAME};
use concerto_orchestrator::harness_service::{
    build_standalone_harness, ApproveInvocationSink, HarnessService,
};
use serde_json::Value;

/// Serializes tests that mutate `XDG_DATA_HOME`/`XDG_CONFIG_HOME` in this
/// process (integration test binaries run their tests in parallel).
static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn workdir(name: &str) -> PathBuf {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("harness_equivalence_work")
        .join(format!("{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("workdir created");
    root
}

/// Isolate the app data/config dirs for one test and restore them on drop.
struct XdgGuard {
    previous_data: Option<String>,
    previous_config: Option<String>,
    root: PathBuf,
}

impl XdgGuard {
    fn new(root: &Path) -> Self {
        let previous_data = std::env::var("XDG_DATA_HOME").ok();
        let previous_config = std::env::var("XDG_CONFIG_HOME").ok();
        std::env::set_var("XDG_DATA_HOME", root.join("data"));
        std::env::set_var("XDG_CONFIG_HOME", root.join("config"));
        Self { previous_data, previous_config, root: root.to_path_buf() }
    }
}

impl Drop for XdgGuard {
    fn drop(&mut self) {
        match &self.previous_data {
            Some(value) => std::env::set_var("XDG_DATA_HOME", value),
            None => std::env::remove_var("XDG_DATA_HOME"),
        }
        match &self.previous_config {
            Some(value) => std::env::set_var("XDG_CONFIG_HOME", value),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Strip nondeterministic per-invocation fields so two equivalent payloads
/// compare equal (`run_id`/`session_id`/`collected_at_ms` are identity/time,
/// not semantics).
fn normalize_environment(mut data: Value) -> Value {
    if let Some(object) = data.as_object_mut() {
        for key in ["run_id", "session_id", "collected_at_ms", "freshness"] {
            object.remove(key);
        }
        if let Some(executables) = object.get_mut("executables").and_then(Value::as_array_mut) {
            for executable in executables {
                if let Some(executable) = executable.as_object_mut() {
                    executable.remove("observed_at_ms");
                }
            }
        }
    }
    data
}

/// Deterministic store double: returns the same evidence-bearing chunk every
/// time, so the service adapter can be compared against a direct store call.
struct FixedEvidenceStore {
    evidence: Vec<EvidentChunk>,
}

#[async_trait]
impl MemoryStore for FixedEvidenceStore {
    async fn retrieve(
        &self,
        _query: &MemoryQuery,
        _cancel: CancellationToken,
    ) -> Result<Vec<MemoryChunk>, MemoryError> {
        Ok(self.evidence.iter().map(|item| item.chunk.clone()).collect())
    }

    async fn retrieve_with_evidence(
        &self,
        _query: &MemoryQuery,
        _cancel: CancellationToken,
    ) -> Result<Vec<EvidentChunk>, MemoryError> {
        Ok(self.evidence.clone())
    }

    async fn store(
        &self,
        _entry: concerto_core::memory::MemoryEntry,
        _cancel: CancellationToken,
    ) -> Result<concerto_core::memory::MemoryId, MemoryError> {
        Err(MemoryError::Persistence("read-only test double".into()))
    }

    async fn invalidate(
        &self,
        id: concerto_core::memory::MemoryId,
        _cancel: CancellationToken,
    ) -> Result<(), MemoryError> {
        Err(MemoryError::NotFound(id.to_string()))
    }
}

struct TestAudit;

#[async_trait]
impl AuditLog for TestAudit {
    async fn record(
        &self,
        _entry: AuditEntry,
        _cancel: CancellationToken,
    ) -> Result<(), concerto_core::error::PolicyError> {
        Ok(())
    }
}

fn fixed_evidence_chunk() -> EvidentChunk {
    let project_id = ProjectId("equivalence-project".into());
    EvidentChunk {
        chunk: MemoryChunk {
            id: "chunk-1".into(),
            project_id: project_id.clone(),
            namespace: MemoryNamespace::Project(project_id),
            content: "gate writes are WAL-before-execute".into(),
            file_path: Some("src/lib.rs".into()),
            start_line: Some(1),
            end_line: Some(3),
            chunk_type: ChunkType::Fact,
            score: 0.9,
            model_id: "test".into(),
            model_version: "1".into(),
            stale: false,
        },
        truth: FactStatusView::Assumed,
        source: RecallSource::ModelAuthored,
        confidence: Confidence::Unknown,
        freshness: 1.0,
        derivation: "model-authored claim; no supporting evidence in range".into(),
        degraded: false,
        interrupted: false,
    }
}

/// The `memory.recall` service adapter must return exactly what the store's
/// evidence-bearing recall returns — no lossy reimplementation, same evidence.
#[tokio::test]
async fn memory_recall_service_matches_direct_store_recall() {
    let store: Arc<dyn MemoryStore> =
        Arc::new(FixedEvidenceStore { evidence: vec![fixed_evidence_chunk()] });
    let baseline = store
        .retrieve_with_evidence(
            &MemoryQuery {
                text: "gate".into(),
                project_id: ProjectId("equivalence-project".into()),
                namespace: MemoryNamespace::Project(ProjectId("equivalence-project".into())),
                top_k: 5,
                filters: Vec::new(),
            },
            CancellationToken::new(),
        )
        .await
        .expect("baseline recall");

    let mut registry = ToolRegistry::default();
    registry.register(Box::new(MemoryRecallTool::new(
        store.clone(),
        ProjectId("equivalence-project".into()),
    )));
    let policy = Arc::new(SimplePolicyEngine::new(
        vec![PolicyRule::AutoApprove(Condition::Always)],
        Arc::new(TestAudit),
    ));
    let executor = Arc::new(ToolExecutor::new(Arc::new(registry), policy));
    let session = SessionContext::new(concerto_core::ids::Ulid::new(), std::env::temp_dir());
    let service = HarnessService::new(executor, session);

    let invocation = service
        .memory_recall(serde_json::json!({ "query": "gate" }), CancellationToken::new())
        .await
        .expect("service recall runs");
    assert_eq!(invocation.operation, MEMORY_RECALL_TOOL_NAME);

    // The service payload carries the same evidence fields the direct store
    // returned — source, status, derivation, degraded, interrupted.
    let chunks = invocation.data["chunks"].as_array().expect("chunks array");
    assert_eq!(chunks.len(), baseline.len());
    assert_eq!(chunks[0]["truth"], baseline[0].truth.as_str());
    assert_eq!(chunks[0]["source"], baseline[0].source.as_str());
    assert_eq!(chunks[0]["derivation"], baseline[0].derivation);
    assert_eq!(chunks[0]["degraded"], false);
    assert_eq!(chunks[0]["interrupted"], false);
}

/// The standalone service (CLI/desktop seam) is stable across builds: two
/// equivalent `environment.describe` requests normalize to the same payload.
#[tokio::test]
async fn environment_describe_is_equivalent_across_standalone_builds() {
    let _guard = ENV_LOCK.lock().await;
    let root = workdir("env");
    let project = root.join("project");
    std::fs::create_dir_all(&project).expect("project dir");
    let _xdg = XdgGuard::new(&root);
    let config = concerto_config::AppConfig::default();

    let first = build_standalone_harness(&project, &config, false, Arc::new(ApproveInvocationSink))
        .await
        .expect("first harness");
    let first = first
        .service
        .environment_describe(serde_json::json!({ "detail": "full" }), CancellationToken::new())
        .await
        .expect("first describe");

    let second =
        build_standalone_harness(&project, &config, false, Arc::new(ApproveInvocationSink))
            .await
            .expect("second harness");
    let second = second
        .service
        .environment_describe(serde_json::json!({ "detail": "full" }), CancellationToken::new())
        .await
        .expect("second describe");

    assert_eq!(first.operation, second.operation);
    assert_eq!(
        normalize_environment(first.data),
        normalize_environment(second.data),
        "equivalent requests must produce equivalent environment evidence"
    );
}

/// The checkpoint lifecycle through the standalone service: create anchors the
/// bytes, a modification is reverted by restore, and the record carries its
/// evidence (status/included/limitations).
#[tokio::test]
async fn checkpoint_lifecycle_round_trips_through_the_standalone_service() {
    let _guard = ENV_LOCK.lock().await;
    let root = workdir("checkpoint");
    let project = root.join("project");
    std::fs::create_dir_all(&project).expect("project dir");
    std::fs::write(project.join("a.txt"), "original\n").expect("seed");
    let _xdg = XdgGuard::new(&root);
    let config = concerto_config::AppConfig::default();

    let harness =
        build_standalone_harness(&project, &config, false, Arc::new(ApproveInvocationSink))
            .await
            .expect("harness");

    let created = harness
        .service
        .checkpoint_create(serde_json::json!({ "paths": ["a.txt"] }), CancellationToken::new())
        .await
        .expect("create");
    assert_eq!(created.data["status"], "active");
    assert_eq!(created.data["included"][0]["canonical_path"], "a.txt");

    // Change the file, then restore through the same seam.
    std::fs::write(project.join("a.txt"), "changed\n").expect("modify");
    let id = created.data["checkpoint_id"].as_str().expect("id").to_string();
    let restored = harness
        .service
        .checkpoint_restore(
            serde_json::json!({ "checkpoint_id": id, "by_agent": "equivalence-test" }),
            CancellationToken::new(),
        )
        .await
        .expect("restore");
    assert_eq!(restored.data["status"], "restored");
    assert_eq!(std::fs::read_to_string(project.join("a.txt")).expect("read"), "original\n");

    // Inspect reports the transitioned record (evidence, not a claim).
    let inspected = harness
        .service
        .checkpoint_inspect(serde_json::json!({ "checkpoint_id": id }), CancellationToken::new())
        .await
        .expect("inspect");
    assert_eq!(inspected.data["found"], true);
    assert_eq!(inspected.data["record"]["status"]["restored"]["by"], "equivalence-test");
}
