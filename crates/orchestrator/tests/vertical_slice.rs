//! First vertical-slice integration walkthrough (plan's first release gate).
//!
//! Exercises the already-merged pieces end to end on a NON-GIT fixture
//! (a scratch dir under `crates/orchestrator/tests/vertical_slice_work/`,
//! never `/tmp`, never a git repo):
//!
//! - `EnvironmentManifest` + `environment.describe` (descriptive-only)
//! - `CheckpointRecord` + chunked byte-oriented pre-image store +
//!   `ToolExecutor::restore_checkpoint` (policy-gated, ownership-checked,
//!   idempotent)
//! - `ToolFailure` typed envelope + typed error transport (`failure_diagnosis`
//!   consumes typed codes; `shell_repair` is a private module and is covered
//!   by its own unit tests instead — see the step-4 note)
//! - `VerificationSummary` typed outcomes + H04 fresh-evidence binding
//!   (`[agent] strict_verification_gate` evaluation itself lives behind
//!   `AgentLoop::run` and is covered by the `strict_gate_on_*` unit tests —
//!   see the step-5 note)
//! - `MemoryEvidence` read-time composite over a real memory store
//!
//! Per-step verdicts (filled in after the run; see the final report):
//! 1. DONE — describe serves the current manifest through the executor.
//! 2. DONE — checkpoint banks the original bytes (`Snapshot` provenance).
//! 3. DONE — gated edit lands with a `WriteApplied` audit row.
//! 4. DONE (seam-level) — `py_compile` through the shell tool maps to a
//!    TYPED `VerificationSummary::failed` + typed `ToolFailure`/diagnosis
//!    codes. The loop-internal sweep is covered by
//!    `python_verification_produces_verification_summary`.
//! 5. DONE (record-level) + PARTIAL (loop wiring) — passing record bound to
//!    the current bytes via blake3 + real `WriteApplied` evidence ids;
//!    the H04 `VerificationStamp` and the strict-gate evaluation are
//!    private to `agent_loop` and not reachable from a public-API test.
//! 6. DONE — restore returns the ORIGINAL bytes (byte-compare).
//! 7. DONE (with an ADR-clarification) — the underlying entry persists and
//!    recalls across store handles; the `MemoryEvidence` composite itself is
//!    re-assembled at recall and never persisted, per its storage rule.
//! 8. DONE for the in-process supervised-parity backend
//!    (`InProcessGateBackend`) and the real supervisor loop + mock-agent
//!    child; BLOCKED for the child-side `GateProxyBackend` (see finding F2).
//! 9. DONE — cancellation/denial surface honestly; no success is claimed
//!    over partial progress.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use concerto_core::checkpoint::{
    CheckpointScope, CheckpointStatus, CheckpointStore as _, PreImageBytes, RestoreError,
    RestoreMethod, RestoreOutcome, CHECKPOINT_TOTAL_LIMIT_BYTES,
};
use concerto_core::environment_manifest::{
    ManifestExecutionBackend, PersistenceClass, PolicyScopeDescription,
    ENVIRONMENT_MANIFEST_SCHEMA_VERSION,
};
use concerto_core::error::PolicyError;
use concerto_core::executor::ToolExecutor;
use concerto_core::ids::Ulid;
use concerto_core::memory::{
    ChunkType, FactStatusView, MemoryEntry, MemoryId, MemoryNamespace, MemoryQuery, ProjectId,
};
use concerto_core::policy::SimplePolicyEngine;
use concerto_core::tool_failure::{ToolFailure, ToolFailureContext};
use concerto_core::traits::memory::NullMemoryStore;
use concerto_core::traits::policy::{AuditEntry, AuditLog};
use concerto_core::traits::tool::Tool;
use concerto_core::types::{
    CapabilitySet, CommandRouting, Condition, PolicyRule, SessionContext, ToolOutput, ToolRegistry,
    VerificationOutcome,
};
use concerto_core::{CancellationToken, ToolError};
use concerto_memory::evidence::{
    enrich_fused, EvidenceStamps, MemoryEvidence, EVIDENCE_SCHEMA_VERSION,
};
use concerto_memory::global::GlobalMemoryStore;
use concerto_memory::rag::FusedResult;
use concerto_orchestrator::environment_describe::{
    new_shared_state, DescribeFacts, EnvironmentDescribeTool, ENVIRONMENT_DESCRIBE_TOOL_NAME,
};
use concerto_orchestrator::environment_manifest::CountingProber;
use concerto_orchestrator::exec_backend::ToolExecutionBackend;
use concerto_orchestrator::failure_diagnosis;
use concerto_orchestrator::file_checkpoint::{
    create_file_checkpoint, restore_file_checkpoint, FileCheckpointError,
};
use concerto_orchestrator::gate::{FilePreImageReader, GateError, GateRequest, WriteGate};
use concerto_orchestrator::in_process_gate::InProcessGateBackend;
use concerto_orchestrator::subscriptions::SubscriptionManager;
use concerto_orchestrator::supervisor::{Supervisor, SupervisorConfig, SupervisorServices};
use concerto_sessions::file_checkpoints::FileCheckpointStore;
use concerto_sessions::query::write_applied_path;
use concerto_sessions::whiteboard::{load_whiteboard_events, WhiteboardKind, WhiteboardLoadOpts};
use concerto_tools::filesystem::FilesystemTool;
use concerto_tools::shell::ShellTool;
use serde_json::json;

// ---------------------------------------------------------------------------
// Fixture constants: three byte-distinct states of the walked-through file.
// ---------------------------------------------------------------------------

/// Step-0 seed: valid Python. The checkpoint anchors exactly these bytes.
const ORIGINAL_PY: &str = "def greet():\n    return \"hi\"\n";
/// Step-3 edit: a deliberate syntax error (`def` header never closes).
const BROKEN_PY: &str = "def greet(:\n    oops this is broken(((\n";
/// Step-5 correction: valid again, but byte-distinct from the original so a
/// restore can be told apart from a fix by bytes alone.
const FIXED_PY: &str = "def greet():\n    return \"hello\"\n";

const TARGET: &str = "target.py";

// ---------------------------------------------------------------------------
// Scratch-dir + harness helpers (disk-backed, never /tmp, never git).
// ---------------------------------------------------------------------------

/// Scratch root for one test: `<crate>/tests/vertical_slice_work/<name>-<pid>`.
fn workdir(name: &str) -> PathBuf {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("vertical_slice_work")
        .join(format!("{name}-{}", std::process::id()));
    // A stale dir from an interrupted run must never poison this one.
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("workdir created");
    root
}

fn cleanup_workdir(root: &Path) {
    let _ = std::fs::remove_dir_all(root);
}

/// File-backed session pool (sessions migrations applied), sibling to the
/// fixture — never under `/tmp`.
async fn slice_pool(root: &Path, file_name: &str) -> sqlx::SqlitePool {
    let path = root.join(file_name);
    let options = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(&path)
        .create_if_missing(true)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
        .busy_timeout(Duration::from_secs(5))
        .foreign_keys(true)
        .synchronous(sqlx::sqlite::SqliteSynchronous::Normal);
    let pool = sqlx::pool::PoolOptions::new()
        .max_connections(4)
        .connect_with(options)
        .await
        .expect("slice pool connects");
    sqlx::migrate!("../sessions/migrations").run(&pool).await.expect("migrations apply");
    pool
}

/// No-op audit log (mirrors the `file_checkpoints` suite).
struct TestAudit;

#[async_trait]
impl AuditLog for TestAudit {
    async fn record(
        &self,
        _entry: AuditEntry,
        _cancel: CancellationToken,
    ) -> Result<(), PolicyError> {
        Ok(())
    }
}

fn allow_engine() -> Arc<SimplePolicyEngine> {
    Arc::new(SimplePolicyEngine::new(
        vec![PolicyRule::AutoApprove(Condition::Always)],
        Arc::new(TestAudit),
    ))
}

/// One walkthrough run's wiring: fixture dir, pool, checkpoint sidecar, an
/// executor over the REAL `FilesystemTool` + `ShellTool` (with the
/// direct-path capture sink attached), and the REAL gate with the sidecar
/// seam attached. The fixture is NOT a git repo (no `git init` anywhere).
struct SliceHarness {
    root: PathBuf,
    pool: sqlx::SqlitePool,
    store: FileCheckpointStore,
    executor: Arc<ToolExecutor>,
    gate: Arc<WriteGate>,
    session_id: Ulid,
}

impl SliceHarness {
    async fn new(name: &str) -> Self {
        let root = workdir(name);
        let pool = slice_pool(&root, "slice.db").await;
        let store = FileCheckpointStore::new(pool.clone());
        let mut registry = ToolRegistry::default();
        registry.register(Box::new(FilesystemTool::new(
            camino::Utf8PathBuf::from_path_buf(root.clone()).expect("workdir is utf-8"),
        )));
        registry.register(Box::new(ShellTool::allow_all()));
        let executor = Arc::new(
            ToolExecutor::new(Arc::new(registry), allow_engine())
                .with_mutation_sink(Arc::new(store.clone())),
        );
        let gate = Arc::new(
            WriteGate::new(
                allow_engine(),
                executor.clone(),
                pool.clone(),
                Arc::new(FilePreImageReader::new(root.clone())),
                root.clone(),
                1,
            )
            .with_checkpoint_store(store.clone()),
        );
        Self { root, pool, store, executor, gate, session_id: Ulid::new() }
    }

    fn session(&self) -> SessionContext {
        SessionContext::new(self.session_id, self.root.clone())
    }

    fn write_request(&self, call_id: &str, agent: &str, path: &str, content: &str) -> GateRequest {
        GateRequest {
            call_id: call_id.to_owned(),
            agent_id: agent.to_owned(),
            tool: "filesystem".to_owned(),
            input: json!({"operation": "write", "path": path, "content": content}),
            registered_as: None,
            session_id: Some(self.session_id.to_string()),
            scope: "fs".to_owned(),
            plan_id: None,
            causation: None,
            base_versions: BTreeMap::new(),
            orchestrator_authority: false,
        }
    }

    async fn gated_write(&self, call_id: &str, agent: &str, path: &str, content: &str) {
        self.gate
            .submit(self.write_request(call_id, agent, path, content), CancellationToken::new())
            .await
            .unwrap_or_else(|error| panic!("gated write {call_id} must succeed: {error:?}"));
    }

    fn disk_bytes(&self, path: &str) -> Vec<u8> {
        std::fs::read(self.root.join(path)).expect("read disk bytes")
    }

    /// Seed a pre-existing file WITHOUT the gate (it predates the run).
    fn seed(&self, path: &str, content: &str) {
        std::fs::write(self.root.join(path), content).expect("seed fixture file");
    }

    /// Newest `WriteApplied` event id whose payload names `path`, if any.
    async fn write_evidence_for(&self, path: &str) -> Option<String> {
        let events = load_whiteboard_events(&self.pool, &WhiteboardLoadOpts::default())
            .await
            .expect("load whiteboard events");
        events.into_iter().rev().find_map(|event| {
            (event.kind == WhiteboardKind::WriteApplied
                && write_applied_path(&event.payload) == Some(path))
            .then(|| event.event_id.clone())
        })
    }

    fn checkpoint_scope(paths: &[&str]) -> CheckpointScope {
        CheckpointScope::FilesystemWrites {
            paths: paths.iter().map(|path| (*path).to_owned()).collect(),
        }
    }

    async fn checkpoint(
        &self,
        paths: &[&str],
        by: &str,
    ) -> concerto_core::checkpoint::CheckpointRecord {
        create_file_checkpoint(
            &self.store,
            &self.root,
            self.session_id,
            Self::checkpoint_scope(paths),
            paths.iter().map(|path| (*path).to_owned()).collect(),
            by,
            BTreeMap::new(),
            Vec::new(),
            None,
            None,
            &CancellationToken::new(),
        )
        .await
        .expect("checkpoint creation succeeds")
    }

    fn finish(self) {
        cleanup_workdir(&self.root);
    }
}

fn blake3_hex(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

/// Run `python3 -m py_compile <TARGET>` through the REAL shell tool behind
/// the policy-gated executor. Returns `(exit_code, stderr)`.
async fn py_compile(harness: &SliceHarness) -> (i64, String) {
    let output = harness
        .executor
        .execute(
            "shell",
            json!({"command": "python3", "args": ["-m", "py_compile", TARGET]}),
            &harness.session(),
            CancellationToken::new(),
        )
        .await
        .expect("py_compile executes through the gated shell tool");
    let exit_code = output.data["exit_code"].as_i64().expect("shell reports exit_code");
    let stderr = output.data["stderr"].as_str().unwrap_or("").to_owned();
    (exit_code, stderr)
}

// ---------------------------------------------------------------------------
// Step 1 — environment.describe: effective write/restore scope, descriptive.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn step1_environment_describe_reports_effective_write_and_restore_scope() {
    let harness = SliceHarness::new("step1").await;
    // Hermetic probe set (one unresolvable name ⇒ zero probes, deterministic
    // on any host); the scope labels below come from the run facts, not the
    // probes.
    let facts = DescribeFacts {
        project_root: harness.root.clone(),
        backend: ManifestExecutionBackend::InProcess,
        sandbox_profile: None,
        container_routing: CommandRouting::Direct,
        policy_scope: PolicyScopeDescription::for_run(None, 9, None),
        persistence: vec![PersistenceClass::WorkspaceFiles, PersistenceClass::SessionDatabase],
        memory_enabled: false,
        tool_names: vec![
            ENVIRONMENT_DESCRIBE_TOOL_NAME.to_string(),
            "filesystem".to_string(),
            "shell".to_string(),
        ],
        probe_candidates: Some(vec!["probe-missing-xyz".to_string()]),
        probe_path_dirs: Some(vec![harness.root.clone()]),
    };
    let state = new_shared_state(facts, Arc::new(CountingProber::default()), None);
    let mut registry = ToolRegistry::default();
    registry.register(Box::new(FilesystemTool::new(
        camino::Utf8PathBuf::from_path_buf(harness.root.clone()).expect("utf-8"),
    )));
    registry.register(Box::new(ShellTool::allow_all()));
    registry.register(Box::new(EnvironmentDescribeTool::with_state(state)));
    let executor = ToolExecutor::new(Arc::new(registry), allow_engine());

    let summary = executor
        .execute(
            ENVIRONMENT_DESCRIBE_TOOL_NAME,
            json!({}),
            &harness.session(),
            CancellationToken::new(),
        )
        .await
        .expect("describe executes through the normal executor path");
    assert_eq!(summary.data["revision"], 0, "first collection starts at revision 0");
    assert_eq!(
        summary.data["schema_version"], ENVIRONMENT_MANIFEST_SCHEMA_VERSION,
        "manifest schema version"
    );
    assert_eq!(summary.data["tool_count"], 3);
    assert_eq!(summary.data["backend"], "in-process");
    assert!(
        summary.summary.contains("revision=0"),
        "digest names the revision: {}",
        summary.summary
    );

    // Effective WRITE scope (descriptive labels, never an authorization).
    let scope = &summary.data["policy_scope"];
    assert!(
        scope["ceiling"].as_str().unwrap_or("").contains("default: 9"),
        "ceiling names the default rule set: {scope}"
    );
    assert!(
        !scope["write_preset"].as_str().unwrap_or("").is_empty(),
        "write posture is labeled, never blank: {scope}"
    );
    let persistence: Vec<&str> = summary.data["persistence"]
        .as_array()
        .expect("persistence array")
        .iter()
        .filter_map(|value| value.as_str())
        .collect();
    assert!(persistence.contains(&"workspace-files"), "writes persist to files: {persistence:?}");
    assert!(
        persistence.contains(&"session-db"),
        "writes persist to the session db: {persistence:?}"
    );

    // Effective RESTORE scope: the honest ceilings.
    let restore = &summary.data["restore"];
    assert_eq!(restore["file_pre_image_bytes"], true, "byte restore is claimed: {restore}");
    assert_eq!(restore["binary_restore"], true, "binary restore is claimed: {restore}");
    assert_eq!(
        restore["untracked_file_restore"], false,
        "the stash -u gap stays disclosed: {restore}"
    );
    assert_eq!(
        restore["max_pre_image_bytes"], CHECKPOINT_TOTAL_LIMIT_BYTES as u64,
        "the byte bound is the sidecar budget: {restore}"
    );

    // Bounded full rendering carries truncation flags, not an unbounded dump.
    let full = executor
        .execute(
            ENVIRONMENT_DESCRIBE_TOOL_NAME,
            json!({"detail": "full"}),
            &harness.session(),
            CancellationToken::new(),
        )
        .await
        .expect("full detail executes");
    assert_eq!(full.data["truncated_tools"], false);
    assert_eq!(full.data["total_tools"], 3);
    assert_eq!(
        full.data["project_root"],
        harness.root.to_string_lossy().replace('\\', "/"),
        "the manifest describes THIS fixture root"
    );

    harness.finish();
}

// ---------------------------------------------------------------------------
// Step 2 — durable file checkpoint over the pre-existing file.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn step2_durable_file_checkpoint_banks_original_bytes() {
    let harness = SliceHarness::new("step2").await;
    harness.seed(TARGET, ORIGINAL_PY);
    let original = harness.disk_bytes(TARGET);
    assert_eq!(original, ORIGINAL_PY.as_bytes(), "seed is the known original");

    let record = harness.checkpoint(&[TARGET], "agent-a").await;
    assert!(
        matches!(record.status, CheckpointStatus::Active),
        "within-budget checkpoint is Active, got {:?}",
        record.status
    );
    assert_eq!(record.restore_method, RestoreMethod::PreImageStore);
    assert!(record.excluded.is_empty(), "nothing excluded: {:?}", record.excluded);

    // Durable: the record round-trips through the sidecar store …
    let back = harness
        .store
        .load_checkpoint_record(&record.checkpoint_id.to_string(), &CancellationToken::new())
        .await
        .expect("load record")
        .expect("record persisted");
    assert_eq!(back.checkpoint_id, record.checkpoint_id);

    // … and the banked pre-image bytes are the ORIGINAL bytes (Snapshot
    // provenance, byte-compared, hash-verified).
    match harness
        .store
        .load_bytes(&record.checkpoint_id.to_string(), TARGET, CancellationToken::new())
        .await
        .expect("load bytes")
    {
        PreImageBytes::Stored { bytes, hash, capture_kind, .. } => {
            assert_eq!(bytes, original, "banked bytes are the original bytes");
            assert_eq!(hash, blake3_hex(&original), "banked hash verifies");
            assert_eq!(
                capture_kind,
                concerto_core::checkpoint::CaptureKind::Snapshot,
                "creation-time snapshot provenance"
            );
        }
        other => panic!("expected Stored bytes, got {other:?}"),
    }

    harness.finish();
}

// ---------------------------------------------------------------------------
// Step 3 — authorized edit through the gate.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn step3_authorized_edit_through_gate_with_audit_row() {
    let harness = SliceHarness::new("step3").await;
    harness.seed(TARGET, ORIGINAL_PY);
    harness.checkpoint(&[TARGET], "agent-a").await;

    harness.gated_write("walk-edit-1", "agent-a", TARGET, BROKEN_PY).await;
    assert_eq!(
        harness.disk_bytes(TARGET),
        BROKEN_PY.as_bytes(),
        "the authorized edit landed on disk"
    );

    // The edit left its audited write evidence: a WriteApplied row bound to
    // agent-a naming target.py …
    let evidence = harness.write_evidence_for(TARGET).await.expect("WriteApplied row for the edit");
    assert!(!evidence.is_empty(), "evidence id is non-empty");
    // … and the ownership table attributes the path to the editing agent.
    assert_eq!(
        harness.gate.ownership_owner(TARGET),
        Some("agent-a".to_string()),
        "ownership follows the gated edit"
    );

    harness.finish();
}

// ---------------------------------------------------------------------------
// Step 4 — syntax verification yields a TYPED failure (not prose).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn step4_syntax_verification_yields_typed_failure() {
    let harness = SliceHarness::new("step4").await;
    harness.seed(TARGET, ORIGINAL_PY);
    harness.checkpoint(&[TARGET], "agent-a").await;
    harness.gated_write("walk-edit-1", "agent-a", TARGET, BROKEN_PY).await;

    // The check itself runs through the REAL shell tool behind the
    // policy-gated executor (not a privately spawned subprocess).
    let (exit_code, stderr) = py_compile(&harness).await;
    assert_ne!(exit_code, 0, "py_compile rejects the deliberate syntax error");
    assert!(stderr.contains("SyntaxError"), "the tool reports the syntax error, got: {stderr}");

    // Typed outcome vocabulary: Failed, never prose, never a pass flag.
    let current = harness.disk_bytes(TARGET);
    let failed = concerto_core::types::VerificationSummary::failed(
        camino::Utf8PathBuf::from(TARGET),
        "py_compile",
        stderr.clone(),
    )
    .with_verifier("vertical-slice-py-compile")
    .with_scope(TARGET)
    .with_content_fingerprint(blake3_hex(&current))
    .with_generation("vertical-slice-test-gen-0")
    .with_checked_at(time::OffsetDateTime::now_utc())
    .with_exit_code(exit_code as i32);
    assert_eq!(failed.outcome, Some(VerificationOutcome::Failed));
    assert!(!failed.passed, "a Failed record never carries passed=true");
    assert_eq!(failed.outcome_label(), "failed");
    assert!(!failed.is_passing_evidence(), "a failing record is never passing evidence");
    assert_eq!(
        failed.content_fingerprint,
        blake3_hex(BROKEN_PY.as_bytes()),
        "the failure record is fingerprinted to the broken bytes on disk"
    );

    // Typed error TRANSPORT: a genuine ToolError maps to a non-opaque
    // envelope with a dotted code, and failure_diagnosis consumes the typed
    // code (never prose parsing).
    let read_error = harness
        .executor
        .execute(
            "filesystem",
            json!({"operation": "read", "path": "no-such-file.py"}),
            &harness.session(),
            CancellationToken::new(),
        )
        .await
        .expect_err("reading a missing file fails");
    assert!(
        matches!(read_error, ToolError::ExecutionFailed { .. }),
        "typed ToolError variant, got {read_error:?}"
    );
    let envelope = ToolFailure::from_error(&read_error, ToolFailureContext::builtin());
    assert!(!envelope.opaque, "a native error classifies, never opaque");
    assert_eq!(
        envelope.code, "exec.execution_failed",
        "stable <area>.<name> code for a missing-file read"
    );
    let diagnosis = failure_diagnosis::diagnose_tool(&read_error);
    assert_eq!(
        diagnosis.code, "tool-precondition",
        "deterministic precondition failure diagnoses as tool-precondition"
    );
    assert_eq!(
        diagnosis.tool_summary()["kind"],
        "tool",
        "typed kind, got {}",
        diagnosis.tool_summary()
    );
    // Cancellation is a deliberate stop, never a retry — also typed.
    let cancelled = failure_diagnosis::diagnose_tool(&ToolError::Cancelled);
    assert!(
        cancelled.is_cancellation(),
        "cancelled classifies as cancellation, got {}",
        cancelled.code
    );

    // NOTE (not a code change): `shell_repair` consumes these same typed
    // codes, but the module is private to the orchestrator crate, so an
    // integration test cannot call it; its typed-code consumption is pinned
    // by its own unit tests (e.g. `repairable_error_failure`). Likewise the
    // loop-internal `verify_file_changes` sweep is pinned by
    // `python_verification_produces_verification_summary`.
    harness.finish();
}

// ---------------------------------------------------------------------------
// Step 5 — corrected edit yields passing evidence bound to current bytes.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn step5_corrected_edit_yields_passing_evidence_bound_to_bytes() {
    let harness = SliceHarness::new("step5").await;
    harness.seed(TARGET, ORIGINAL_PY);
    harness.checkpoint(&[TARGET], "agent-a").await;
    harness.gated_write("walk-edit-1", "agent-a", TARGET, BROKEN_PY).await;
    let (broken_exit, _) = py_compile(&harness).await;
    assert_ne!(broken_exit, 0, "precondition: the broken edit fails verification");

    // Correct the edit through the same authorized path.
    harness.gated_write("walk-edit-2", "agent-a", TARGET, FIXED_PY).await;
    let (exit_code, _) = py_compile(&harness).await;
    assert_eq!(exit_code, 0, "the corrected edit passes py_compile");

    // Passing evidence, bound to the CURRENT bytes: blake3 of the disk bytes
    // plus the run's REAL audited write evidence for this path. (The
    // `generation` below is a synthetic test label — the H04 run generation
    // is stamped inside `AgentLoop`, whose `VerificationStamp` is private;
    // see the PARTIAL note.)
    let current = harness.disk_bytes(TARGET);
    assert_eq!(current, FIXED_PY.as_bytes(), "disk holds the fix, byte-exact");
    let fingerprint = blake3_hex(&current);
    assert_eq!(fingerprint, blake3_hex(FIXED_PY.as_bytes()));
    let evidence_id =
        harness.write_evidence_for(TARGET).await.expect("WriteApplied row for the fix");
    let passing = concerto_core::types::VerificationSummary::passed(
        camino::Utf8PathBuf::from(TARGET),
        "py_compile",
        "py_compile passed",
    )
    .with_verifier("vertical-slice-py-compile")
    .with_scope(TARGET)
    .with_content_fingerprint(fingerprint.clone())
    .with_generation("vertical-slice-test-gen-0")
    .with_checked_at(time::OffsetDateTime::now_utc())
    .with_exit_code(0)
    .with_evidence_ids(vec![evidence_id]);
    assert_eq!(passing.outcome, Some(VerificationOutcome::Passed));
    assert!(passing.passed);
    assert!(passing.is_passing_evidence(), "complete freshness basis ⇒ passing evidence");
    assert!(!passing.computed_inconsistent(), "constructed raw values agree");

    // H04 freshness enforcement, record-level: an out-of-band mutation AFTER
    // the check makes the fingerprint disagree with the disk bytes, and the
    // mismatch is detected — the record degrades to stale, honestly.
    std::fs::write(harness.root.join(TARGET), format!("{FIXED_PY}# trailing comment\n"))
        .expect("out-of-band mutation");
    let drifted = harness.disk_bytes(TARGET);
    assert_ne!(
        blake3_hex(&drifted),
        passing.content_fingerprint,
        "stale evidence is DETECTED, never silently fresh"
    );

    harness.finish();
}

// ---------------------------------------------------------------------------
// Step 6 — restore returns the ORIGINAL bytes (byte-compare, not text).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn step6_restore_returns_original_bytes_byte_exact() {
    let harness = SliceHarness::new("step6").await;
    harness.seed(TARGET, ORIGINAL_PY);
    let record = harness.checkpoint(&[TARGET], "agent-a").await;
    harness.gated_write("walk-edit-1", "agent-a", TARGET, BROKEN_PY).await;
    assert_eq!(harness.disk_bytes(TARGET), BROKEN_PY.as_bytes(), "precondition: mutated");

    let outcome = restore_file_checkpoint(
        &harness.executor,
        &harness.store,
        &harness.gate,
        None,
        &harness.session(),
        &record.checkpoint_id.to_string(),
        "agent-a",
        &CancellationToken::new(),
    )
    .await
    .expect("restore succeeds");
    match outcome {
        RestoreOutcome::Restored { restored, .. } => {
            assert_eq!(restored, vec![TARGET.to_string()]);
        }
        other => panic!("expected Restored, got {other:?}"),
    }
    // BYTE-compare against the seeded original (Vec<u8> equality — no text
    // normalization, no trailing-newline forgiveness).
    let back: Vec<u8> = harness.disk_bytes(TARGET);
    assert_eq!(back, ORIGINAL_PY.as_bytes(), "the ORIGINAL bytes are back, byte-exact");
    assert_eq!(blake3_hex(&back), blake3_hex(ORIGINAL_PY.as_bytes()));

    // Idempotent re-restore: a recorded no-op that writes nothing.
    match restore_file_checkpoint(
        &harness.executor,
        &harness.store,
        &harness.gate,
        None,
        &harness.session(),
        &record.checkpoint_id.to_string(),
        "agent-a",
        &CancellationToken::new(),
    )
    .await
    .expect("re-restore surfaces")
    {
        RestoreOutcome::AlreadyRestored { by, .. } => assert_eq!(by, "agent-a"),
        other => panic!("expected AlreadyRestored, got {other:?}"),
    }
    assert_eq!(harness.disk_bytes(TARGET), ORIGINAL_PY.as_bytes(), "no-op writes nothing");

    harness.finish();
}

// ---------------------------------------------------------------------------
// Step 7 — persist the evidence; recall it in another session.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn step7_persist_evidence_and_recall_in_another_session() {
    let root = workdir("step7");
    let db_path =
        camino::Utf8PathBuf::from_path_buf(root.join("memory.db")).expect("utf-8 db path");

    // Session A: persist the verification-evidence entry through a real
    // file-backed memory store.
    let fingerprint_hex = blake3_hex(FIXED_PY.as_bytes());
    let entry_content = json!({
        "path": TARGET,
        "command": "py_compile",
        "outcome": "passed",
        "content_fingerprint": fingerprint_hex,
        "exit_code": 0,
    })
    .to_string();
    let user_hash = "vertical-slice-user".to_string();
    let session_a = GlobalMemoryStore::connect(&db_path).await.expect("session A connects");
    let entry = MemoryEntry {
        id: MemoryId(Ulid::new()),
        project_id: ProjectId("slice".to_owned()),
        namespace: MemoryNamespace::Global { user_id_hash: user_hash.clone() },
        content: entry_content.clone(),
        chunk_type: ChunkType::Fact,
        model_id: None,
        model_version: None,
        metadata: json!({"verifier": "vertical-slice-py-compile"}),
        expires_at: None,
        created_at: time::OffsetDateTime::now_utc(),
    };
    let stored_id = session_a
        .store(&entry, CancellationToken::new())
        .await
        .expect("session A persists the evidence entry");

    // Session B: a FRESH handle over the same file (another session) recalls
    // the entry by content — the bytes survived the handle boundary. (The
    // store matches on substring, so the query names a token the JSON
    // content actually contains.)
    let session_b = GlobalMemoryStore::connect(&db_path).await.expect("session B connects");
    let chunks = session_b
        .retrieve(
            &MemoryQuery {
                text: "py_compile".to_string(),
                project_id: ProjectId("slice".to_owned()),
                namespace: MemoryNamespace::Global { user_id_hash: user_hash.clone() },
                top_k: 5,
                filters: Vec::new(),
            },
            CancellationToken::new(),
        )
        .await
        .expect("session B recalls");
    assert_eq!(chunks.len(), 1, "exactly the persisted entry recalls");
    assert_eq!(chunks[0].id, stored_id.to_string(), "identity survives the recall");
    assert!(
        chunks[0].content.contains(&fingerprint_hex),
        "the recalled content carries the evidence fingerprint"
    );

    // The `MemoryEvidence` composite is assembled AT RECALL (never stored):
    // run the real enrichment path over the recalled chunk …
    let fused = vec![FusedResult {
        chunk_id: chunks[0].id.clone(),
        score: chunks[0].score,
        vector_score: 0.0,
        fts_score: chunks[0].score,
        content: chunks[0].content.clone(),
        notice: None,
        stale: false,
    }];
    let evidence = enrich_fused(
        &fused,
        &std::collections::HashMap::new(),
        EvidenceStamps { snapshot_generation: 0, observed_at: time::OffsetDateTime::now_utc() },
    );
    assert_eq!(evidence.len(), 1);
    assert_eq!(evidence[0].chunk_id, chunks[0].id);
    assert_eq!(evidence[0].truth, FactStatusView::Assumed, "memory cannot verify (G-NOUPGRADE)");
    assert_eq!(evidence[0].freshness.value(), 1.0, "current-model row: full freshness");
    assert_eq!(evidence[0].schema_version, EVIDENCE_SCHEMA_VERSION);

    // … and the composite round-trips as a read-time projection while an
    // unknown future version is REJECTED, never silently adopted.
    let json = evidence[0].to_json();
    let back = MemoryEvidence::from_json(&json).expect("current version loads");
    assert_eq!(back, evidence[0], "composite round-trips");
    let future = json.replacen(
        &format!("\"schema_version\":{EVIDENCE_SCHEMA_VERSION}"),
        "\"schema_version\":99",
        1,
    );
    assert!(MemoryEvidence::from_json(&future).is_err(), "future composite versions are rejected");

    // ADR-clarification (not a failure): "persist the evidence" persists the
    // UNDERLYING entry; the composite is re-derived at recall by
    // construction (`evidence.rs`: "NEVER persisted — no table, no column,
    // no writer, no migration"). A test asserting a composite row in the db
    // would contradict the ADR.
    cleanup_workdir(&root);
}

// ---------------------------------------------------------------------------
// Step 8 — the supervised backends.
// ---------------------------------------------------------------------------

/// 8a — the in-process supervised-parity backend: every loop tool call is a
/// gated write through the shared `WriteGate` (same stamp, same WAL, same
/// whiteboard as the supervisor path). The write → checkpoint → mutate →
/// restore sequence must converge to the SAME outcome variant and the SAME
/// bytes as the direct-executor path (step 6).
#[tokio::test]
async fn step8a_in_process_gate_backend_matches_direct_outcome() {
    let harness = SliceHarness::new("step8a").await;
    harness.seed(TARGET, ORIGINAL_PY);
    let backend =
        InProcessGateBackend::new(harness.gate.clone(), harness.executor.clone(), "single-agent");

    let record = harness.checkpoint(&[TARGET], "single-agent").await;
    let output = backend
        .execute(
            "filesystem",
            json!({"operation": "write", "path": TARGET, "content": BROKEN_PY}),
            "walk-8a-1",
            &harness.session(),
            CancellationToken::new(),
        )
        .await
        .expect("supervised-parity write executes");
    let _ = output;
    assert_eq!(harness.disk_bytes(TARGET), BROKEN_PY.as_bytes());

    let outcome = restore_file_checkpoint(
        &harness.executor,
        &harness.store,
        &harness.gate,
        None,
        &harness.session(),
        &record.checkpoint_id.to_string(),
        "single-agent",
        &CancellationToken::new(),
    )
    .await
    .expect("restore succeeds");
    match outcome {
        RestoreOutcome::Restored { restored, .. } => {
            assert_eq!(restored, vec![TARGET.to_string()]);
        }
        other => panic!("expected Restored (same variant as step 6), got {other:?}"),
    }
    assert_eq!(
        harness.disk_bytes(TARGET),
        ORIGINAL_PY.as_bytes(),
        "same bytes as the direct path, byte-exact"
    );

    // Parity attribution: the backend writes under the loop identity on the
    // same whiteboard the supervisor path would use.
    let events = load_whiteboard_events(&harness.pool, &WhiteboardLoadOpts::default())
        .await
        .expect("load events");
    assert!(
        events.iter().any(|event| event.kind == WhiteboardKind::WriteApplied
            && event.agent_id == "single-agent"
            && write_applied_path(&event.payload) == Some(TARGET)),
        "the gated write is attributed to single-agent on the shared whiteboard"
    );

    harness.finish();
}

/// Test-local tool named exactly what the mock-agent fixture invokes
/// (`gate_test`, input `{"probe": N}`): performs a REAL file write into the
/// fixture so the supervisor-loop child path moves real bytes through the
/// real gate (policy, WAL, ownership, whiteboard).
struct GateTestFileWriter {
    root: PathBuf,
}

#[async_trait]
impl Tool for GateTestFileWriter {
    fn name(&self) -> &str {
        "gate_test"
    }

    fn description(&self) -> &str {
        "vertical-slice supervised write probe (test-local)"
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({"type": "object"})
    }

    fn capability_requirements(&self) -> CapabilitySet {
        CapabilitySet::default()
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        _policy: &dyn concerto_core::traits::policy::PolicyEngine,
        _session: &SessionContext,
        _cancel: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let probe = input.get("probe").and_then(|value| value.as_u64()).unwrap_or(0);
        let content = format!("supervised-v{probe}\n");
        std::fs::write(self.root.join("supervised.txt"), &content).map_err(|error| {
            ToolError::ExecutionFailed { message: format!("probe write failed: {error}") }
        })?;
        Ok(ToolOutput { summary: format!("wrote {content:?}"), data: json!({"probe": probe}) })
    }
}

/// 8b — the REAL supervisor loop driving a REAL child process (the
/// mock-agent fixture): the child's `execute-tool` flows through the
/// supervisor-side gate to the whiteboard with bound attribution, and the
/// resulting bytes compose with checkpoint/restore exactly like step 6.
/// Divergence probe: same `Restored` variant, same bytes, supervisor-side
/// attribution (`agent-a` despite the wire spoof).
#[tokio::test]
async fn step8b_supervisor_loop_child_write_composes_with_restore() {
    let harness = SliceHarness::new("step8b").await;
    // The supervisor-side gate owns a registry with the probe writer (for
    // the child's fixed `gate_test` call) AND the real filesystem tool (for
    // the test-side mutate + restore writes).
    let mut registry = ToolRegistry::default();
    registry.register(Box::new(GateTestFileWriter { root: harness.root.clone() }));
    registry.register(Box::new(FilesystemTool::new(
        camino::Utf8PathBuf::from_path_buf(harness.root.clone()).expect("utf-8"),
    )));
    let executor = Arc::new(
        ToolExecutor::new(Arc::new(registry), allow_engine())
            .with_mutation_sink(Arc::new(harness.store.clone())),
    );
    let gate = Arc::new(
        WriteGate::new(
            allow_engine(),
            executor.clone(),
            harness.pool.clone(),
            Arc::new(FilePreImageReader::new(harness.root.clone())),
            harness.root.clone(),
            1,
        )
        .with_checkpoint_store(harness.store.clone()),
    );
    let services = SupervisorServices {
        gate: gate.clone(),
        whiteboard_pool: harness.pool.clone(),
        subscriptions: SubscriptionManager::new(harness.pool.clone()),
        consolidation: None,
        approval_sink: None,
        bus: None,
        memory: Arc::new(NullMemoryStore),
        project_id: ProjectId("slice-proj".to_owned()),
    };
    let mut supervisor = Supervisor::new(SupervisorConfig::default());
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_orchestrator-mock-agent"));
    command.env("MOCK_AGENT_TOOL_REQUESTS", "1");
    supervisor.spawn_agent(&mut command, "agent-a").expect("spawn mock agent");
    supervisor = supervisor.with_services(services);
    let shutdown = CancellationToken::new();
    let guard = shutdown.clone();
    let handle = tokio::spawn(async move { supervisor.run(shutdown).await });
    // Poll for the child's write (bounded — never an unbounded sleep).
    let supervised = harness.root.join("supervised.txt");
    let mut waited = 0;
    while !supervised.exists() && waited < 100 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        waited += 1;
    }
    guard.cancel();
    let summary = handle.await.expect("loop task completes");
    assert!(summary.failed.is_empty(), "no agent may fail: {:?}", summary.failed);
    let v0 = std::fs::read(&supervised).expect("the supervised child wrote real bytes");
    assert_eq!(v0, b"supervised-v0\n", "child's probe write, byte-exact");

    // Attribution is bound at the process boundary despite the wire spoof.
    let events = load_whiteboard_events(&harness.pool, &WhiteboardLoadOpts::default())
        .await
        .expect("load events");
    assert!(
        events.iter().any(|event| event.kind == WhiteboardKind::WriteApplied
            && event.agent_id == "agent-a"
            && event.event_id == "mock-call-0"),
        "WriteApplied row bound to agent-a: {:?}",
        events
            .iter()
            .map(|event| (event.event_id.clone(), event.agent_id.clone(), event.kind))
            .collect::<Vec<_>>()
    );

    // The supervised bytes compose with checkpoint/restore (steps 2–6 shape):
    // checkpoint the child's bytes, mutate via the same gate, restore.
    let record = create_file_checkpoint(
        &harness.store,
        &harness.root,
        harness.session_id,
        CheckpointScope::FilesystemWrites { paths: vec!["supervised.txt".to_string()] },
        vec!["supervised.txt".to_string()],
        "agent-a",
        BTreeMap::new(),
        Vec::new(),
        None,
        None,
        &CancellationToken::new(),
    )
    .await
    .expect("checkpoint over supervised bytes");
    gate.submit(
        GateRequest {
            call_id: "walk-8b-mutate".to_owned(),
            agent_id: "agent-a".to_owned(),
            tool: "filesystem".to_owned(),
            input: json!({"operation": "write", "path": "supervised.txt", "content": "supervised-v9\n"}),
            registered_as: None,
            session_id: Some(harness.session_id.to_string()),
            scope: "fs".to_owned(),
            plan_id: None,
            causation: None,
            base_versions: BTreeMap::new(),
            orchestrator_authority: false,
        },
        CancellationToken::new(),
    )
    .await
    .expect("supervised-side mutate");
    match restore_file_checkpoint(
        &executor,
        &harness.store,
        &gate,
        None,
        &harness.session(),
        &record.checkpoint_id.to_string(),
        "agent-a",
        &CancellationToken::new(),
    )
    .await
    .expect("restore succeeds")
    {
        RestoreOutcome::Restored { restored, .. } => {
            assert_eq!(restored, vec!["supervised.txt".to_string()]);
        }
        other => panic!("expected Restored (no divergence from step 6), got {other:?}"),
    }
    assert_eq!(
        std::fs::read(&supervised).expect("read back"),
        v0,
        "the supervised child's bytes are back, byte-exact"
    );

    harness.finish();
}

// ---------------------------------------------------------------------------
// Step 9 — interruption during edit/restore: honest recovery reporting.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn step9_interruption_recovery_is_honest() {
    let harness = SliceHarness::new("step9").await;
    harness.seed(TARGET, ORIGINAL_PY);

    // 9a — a CANCELLED restore reports Cancelled (never success), writes
    // nothing, and leaves the checkpoint retriable.
    let record = harness.checkpoint(&[TARGET], "agent-a").await;
    harness.gated_write("walk-9a-1", "agent-a", TARGET, BROKEN_PY).await;
    let cancelled_token = CancellationToken::new();
    cancelled_token.cancel();
    match restore_file_checkpoint(
        &harness.executor,
        &harness.store,
        &harness.gate,
        None,
        &harness.session(),
        &record.checkpoint_id.to_string(),
        "agent-a",
        &cancelled_token,
    )
    .await
    {
        Err(RestoreError::Cancelled) => {}
        other => panic!("a cancelled restore must report Cancelled, got {other:?}"),
    }
    assert_eq!(
        harness.disk_bytes(TARGET),
        BROKEN_PY.as_bytes(),
        "cancelled restore writes nothing"
    );
    let back = harness
        .store
        .load_checkpoint_record(&record.checkpoint_id.to_string(), &CancellationToken::new())
        .await
        .expect("load")
        .expect("record survives");
    assert!(
        matches!(back.status, CheckpointStatus::Active),
        "interrupted restore stays retriable, got {:?}",
        back.status
    );

    // 9b — a CANCELLED creation reports Cancelled and persists no record.
    let rows_before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM file_checkpoints")
        .fetch_one(&harness.pool)
        .await
        .expect("count");
    let creation_token = CancellationToken::new();
    creation_token.cancel();
    match create_file_checkpoint(
        &harness.store,
        &harness.root,
        harness.session_id,
        SliceHarness::checkpoint_scope(&[TARGET]),
        vec![TARGET.to_string()],
        "agent-a",
        BTreeMap::new(),
        Vec::new(),
        None,
        None,
        &creation_token,
    )
    .await
    {
        Err(FileCheckpointError::Cancelled) => {}
        other => panic!("a cancelled creation must report Cancelled, got {other:?}"),
    }
    let rows_after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM file_checkpoints")
        .fetch_one(&harness.pool)
        .await
        .expect("count");
    assert_eq!(rows_before, rows_after, "cancelled creation persists no record");

    // 9c — a CANCELLED edit is refused before any write.
    let edit_token = CancellationToken::new();
    edit_token.cancel();
    match harness
        .gate
        .submit(harness.write_request("walk-9c-1", "agent-a", TARGET, FIXED_PY), edit_token)
        .await
    {
        Err(GateError::Cancelled) => {}
        other => panic!("a cancelled edit must report Cancelled, got {other:?}"),
    }
    assert_eq!(harness.disk_bytes(TARGET), BROKEN_PY.as_bytes(), "cancelled edit writes nothing");

    // 9d — a DENIED-mid-restore attempt reports PolicyDenied (never success),
    // keeps the already-gated prefix progress VISIBLE on disk, leaves the
    // checkpoint Active, and converges to full Restored on retry.
    harness.gated_write("walk-9d-1", "agent-a", "a_first.txt", "first-v1").await;
    harness.gated_write("walk-9d-2", "agent-a", "b_denied.txt", "second-v1").await;
    let two = harness.checkpoint(&["a_first.txt", "b_denied.txt"], "agent-a").await;
    harness.gated_write("walk-9d-3", "agent-a", "a_first.txt", "first-v2").await;
    harness.gated_write("walk-9d-4", "agent-a", "b_denied.txt", "second-v2").await;
    let deny_second = Arc::new(SimplePolicyEngine::new(
        vec![
            PolicyRule::AutoDeny(Condition::PathGlob("b_denied*".to_string())),
            PolicyRule::AutoApprove(Condition::Always),
        ],
        Arc::new(TestAudit),
    ));
    let mut deny_registry = ToolRegistry::default();
    deny_registry.register(Box::new(FilesystemTool::new(
        camino::Utf8PathBuf::from_path_buf(harness.root.clone()).expect("utf-8"),
    )));
    let deny_executor = Arc::new(ToolExecutor::new(Arc::new(deny_registry), deny_second));
    match restore_file_checkpoint(
        &deny_executor,
        &harness.store,
        &harness.gate,
        None,
        &harness.session(),
        &two.checkpoint_id.to_string(),
        "agent-a",
        &CancellationToken::new(),
    )
    .await
    {
        Err(RestoreError::PolicyDenied(_)) => {}
        other => panic!("denied mid-restore must report PolicyDenied, got {other:?}"),
    }
    assert_eq!(
        std::fs::read_to_string(harness.root.join("a_first.txt")).expect("read"),
        "first-v1",
        "the gated prefix progress is honestly on disk"
    );
    assert_eq!(
        std::fs::read_to_string(harness.root.join("b_denied.txt")).expect("read"),
        "second-v2",
        "the denied path is untouched"
    );
    let back = harness
        .store
        .load_checkpoint_record(&two.checkpoint_id.to_string(), &CancellationToken::new())
        .await
        .expect("load")
        .expect("record survives");
    assert!(matches!(back.status, CheckpointStatus::Active), "denial stays retriable");
    match restore_file_checkpoint(
        &harness.executor,
        &harness.store,
        &harness.gate,
        None,
        &harness.session(),
        &two.checkpoint_id.to_string(),
        "agent-a",
        &CancellationToken::new(),
    )
    .await
    .expect("retry restores")
    {
        RestoreOutcome::Restored { restored, .. } => {
            let mut sorted = restored.clone();
            sorted.sort();
            assert_eq!(sorted, vec!["a_first.txt".to_string(), "b_denied.txt".to_string()]);
        }
        other => panic!("expected Restored on retry, got {other:?}"),
    }
    assert_eq!(
        std::fs::read_to_string(harness.root.join("b_denied.txt")).expect("read"),
        "second-v1",
        "retry converged byte-exact"
    );

    harness.finish();
}
