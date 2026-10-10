//! ADR-82 §2 — `EnvironmentManifest` collector (slice 6).
//!
//! Builds the run's descriptive environment declaration once per run from the
//! registry (tool identities via the slice-1 canonical-effect service), the
//! session pool (whiteboard persistence), and the workspace paths — then
//! caches it on the coordinator for context injection beside the snapshot
//! digest. Probes are lazy and freshness-stamped: executable observations are
//! taken at run start / resume / explicit refresh only, never per prompt.
//!
//! **This module is the only code that spawns probes.** Context injection
//! ([`EnvironmentManifest::summary`][concerto_core::environment_manifest::EnvironmentManifest::summary])
//! is a pure render over the cached manifest and performs zero probes —
//! pinned by `probe_count_is_one_set_per_run_and_zero_per_prompt` below.
//!
//! The manifest is descriptive, never an authorization token: nothing here
//! feeds the policy engine, the gate, or the executor's decision paths. The
//! collector *reads* the registry's canonical identities (accounting views,
//! like the audit rows) but never authorizes anything with them.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use concerto_core::environment_manifest::{
    EnvironmentManifest, ExecutableObservation, FreshnessStamp, ManifestExecutionBackend,
    ManifestToolEntry, PersistenceClass, PolicyScopeDescription, RestoreCapabilities,
    ENVIRONMENT_MANIFEST_SCHEMA_VERSION,
};
use concerto_core::executor::ToolExecutor;
use concerto_core::types::{CommandRouting, SandboxProfile};
use concerto_core::CancellationToken;
use concerto_sessions::whiteboard::{append_whiteboard_event, load_session_events_of_kind};
use concerto_sessions::{NewWhiteboardEvent, SessionError, WhiteboardEvent, WhiteboardKind};
use thiserror::Error;
use tracing::warn;

/// Author attribution for manifest events. The runtime — not a model — authors
/// `EnvironmentObserved` rows, matching the snapshot barrier's `coordinator`
/// convention.
const MANIFEST_AGENT_ID: &str = "coordinator";

/// Upper bound on one version-probe subprocess (`<exe> --version`).
pub const MANIFEST_PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// Bounded executable-probe candidate set: point-in-time observations, never
/// an exhaustive `PATH` dump (accepted limitation 6).
pub const DEFAULT_PROBE_CANDIDATES: &[&str] = &["cargo", "git", "node", "python3"];

/// Version-string cap: the first line of `<exe> --version`, truncated here so
/// a chatty tool cannot bloat the manifest.
pub const PROBE_VERSION_MAX_CHARS: usize = 128;

/// How many of the newest `environment-observed` rows a restore scans: the
/// log holds at most one manifest per run, so this window is generous.
const RESTORE_SCAN_LIMIT: usize = 32;

/// Errors collecting or persisting the manifest. Collection itself is
/// fail-soft at the barrier ([`run_manifest_barrier`] returns `None` rather
/// than failing the run); this error surfaces only where a caller explicitly
/// requires persistence.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ManifestError {
    /// The whiteboard store failed.
    #[error("environment manifest store failed: {0}")]
    Store(#[from] SessionError),
    /// The stored payload failed envelope validation.
    #[error("invalid environment manifest payload: {0}")]
    Record(String),
}

/// Who answers `<exe> --version` for the collector's executable probes.
///
/// The production implementation spawns the resolved executable with a bound.
/// Tests inject a counting stub, which is how the probe-count test pins the
/// "one probe set per run, none per prompt" discipline.
#[async_trait]
pub trait VersionProber: Send + Sync {
    /// Query one resolved executable's version string, or `None` when it does
    /// not answer (missing, non-executable, timeout, cancellation, or any
    /// spawn/wait failure — all fail soft).
    async fn query_version(&self, exe: &Path, cancel: &CancellationToken) -> Option<String>;
}

/// Production prober: `<exe> --version` with [`MANIFEST_PROBE_TIMEOUT`],
/// killed on expiry, cancelled cooperatively.
pub struct SystemVersionProber {
    /// Per-probe timeout.
    pub timeout: Duration,
}

impl Default for SystemVersionProber {
    fn default() -> Self {
        Self { timeout: MANIFEST_PROBE_TIMEOUT }
    }
}

#[async_trait]
impl VersionProber for SystemVersionProber {
    async fn query_version(&self, exe: &Path, cancel: &CancellationToken) -> Option<String> {
        if cancel.is_cancelled() {
            return None;
        }
        let child = tokio::process::Command::new(exe)
            .arg("--version")
            .kill_on_drop(true)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .ok()?;
        let output =
            tokio::time::timeout(self.timeout, child.wait_with_output()).await.ok()?.ok()?;
        if cancel.is_cancelled() {
            return None;
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let text = if stdout.trim().is_empty() { stderr } else { stdout };
        let first = text.lines().next().unwrap_or("").trim();
        if first.is_empty() {
            return None;
        }
        Some(truncate_chars(first, PROBE_VERSION_MAX_CHARS))
    }
}

/// Counting test prober: answers a fixed version for every query and records
/// how many queries ran, so tests pin the probe discipline exactly.
#[derive(Debug, Default)]
pub struct CountingProber {
    calls: AtomicUsize,
}

impl CountingProber {
    /// How many version queries have run so far.
    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl VersionProber for CountingProber {
    async fn query_version(&self, _exe: &Path, _cancel: &CancellationToken) -> Option<String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Some("test-tool 0.0".to_string())
    }
}

/// Resolve `name` against `dirs` (a `PATH` split): the first existing file
/// wins. `name` must be a bare file name — anything carrying a separator is
/// rejected rather than resolved.
///
/// On Unix the candidate must also carry an execute bit; elsewhere mere
/// existence suffices (the version probe still has to answer).
fn resolve_in_path(name: &str, dirs: &[PathBuf]) -> Option<PathBuf> {
    if name.is_empty() || name.contains('/') || name.contains('\\') {
        return None;
    }
    dirs.iter().map(|dir| dir.join(name)).find(|path| is_runnable(path))
}

#[cfg(unix)]
fn is_runnable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.is_file() && path.metadata().is_ok_and(|meta| meta.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_runnable(path: &Path) -> bool {
    path.is_file()
}

/// Split the process `PATH` into directories, skipping empty entries.
fn path_dirs_from_env() -> Vec<PathBuf> {
    std::env::var_os("PATH").map_or_else(Vec::new, |paths| {
        std::env::split_paths(&paths).filter(|dir| !dir.as_os_str().is_empty()).collect()
    })
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        text.to_string()
    } else {
        text.chars().take(max_chars).collect()
    }
}

/// Unix epoch milliseconds (UTC); `0` on a pre-epoch clock (never expected).
fn unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

/// Build the run's tool inventory from the executor's registry (ADR-82 slice
/// 1): every registered tool under both its invoked name and its canonical
/// policy-view identity, sorted by registered name so the manifest is
/// deterministic regardless of registry hash order.
///
/// Unknown tools cannot occur here (the registry is the source), but a tool
/// whose canonical effect is unavailable (supervised backends own no
/// registry) falls back to its registered name for both identities —
///
/// the pair stays truthful either way.
pub fn inventory_from_executor(executor: &ToolExecutor) -> Vec<ManifestToolEntry> {
    let mut entries: Vec<ManifestToolEntry> = executor
        .tool_definitions()
        .iter()
        .map(|definition| {
            let canonical = executor
                .canonical_effect(&definition.name, &serde_json::Value::Null)
                .map(|effect| effect.policy_name)
                .unwrap_or_else(|| definition.name.clone());
            ManifestToolEntry {
                registered_name: definition.name.clone(),
                canonical_policy_name: canonical,
            }
        })
        .collect();
    entries.sort_by(|a, b| a.registered_name.cmp(&b.registered_name));
    entries
}

/// Inputs for one manifest collection. Everything the collector needs is
/// supplied here — it performs no registry, config, or pool reads of its own
/// beyond the explicit probe spawns and (via the barrier) persistence.
pub struct ManifestInputs<'a> {
    /// Run identity (fresh ULID per run; preserved across resume by restore).
    pub run_id: String,
    /// Owning session identity.
    pub session_id: String,
    /// The workspace root the manifest describes.
    pub project_root: &'a Path,
    /// Which execution backend the run dispatches through (descriptive).
    pub backend: ManifestExecutionBackend,
    /// The run-level sandbox profile, when one was configured.
    pub sandbox_profile: Option<SandboxProfile>,
    /// The run's command-routing assertion.
    pub container_routing: CommandRouting,
    /// Tool inventory under both identities (see [`inventory_from_executor`]).
    pub inventory: Vec<ManifestToolEntry>,
    /// Descriptive policy scope labels (see [`PolicyScopeDescription`]).
    pub policy_scope: PolicyScopeDescription,
    /// Which effect classes survive the run.
    pub persistence: Vec<PersistenceClass>,
    /// Whether the memory index persists for this run (adds `MemoryIndex`).
    pub memory_enabled: bool,
    /// Previous revision for an explicit re-collection (`None` for a run's
    /// first collection — the revision starts at `0`).
    pub previous_revision: Option<u64>,
    /// Probe candidate override (`None` selects [`DEFAULT_PROBE_CANDIDATES`).
    pub candidates: Option<&'a [&'a str]>,
    /// `PATH`-directory override for hermetic tests (`None` reads `PATH`).
    pub path_dirs: Option<Vec<PathBuf>>,
}

/// Collect one manifest revision: identities from the inputs, root hash in
/// the `tool_facts::project_root_hash` grammar, restore ceilings from
/// checkpoint-record coverage, and one bounded probe set for the executable
/// observations.
///
/// Cancellation is cooperative and fail-soft: a cancelled token stops further
/// probes (remaining observations are simply absent) but the manifest is
/// still returned — a description must never fail its run.
pub async fn collect_environment_manifest(
    inputs: ManifestInputs<'_>,
    prober: &dyn VersionProber,
    cancel: &CancellationToken,
) -> EnvironmentManifest {
    let candidates: &[&str] = inputs.candidates.unwrap_or(DEFAULT_PROBE_CANDIDATES);
    let path_dirs = inputs.path_dirs.unwrap_or_else(path_dirs_from_env);
    let mut executables = Vec::with_capacity(candidates.len());
    for name in candidates {
        if cancel.is_cancelled() {
            break;
        }
        let resolved = resolve_in_path(name, &path_dirs);
        let version = match &resolved {
            Some(path) => prober.query_version(path, cancel).await,
            None => None,
        };
        executables.push(ExecutableObservation {
            name: (*name).to_string(),
            resolved_path: resolved.map(|path| path.to_string_lossy().into_owned()),
            version,
            observed_at_ms: unix_ms(),
        });
    }

    let revision = inputs.previous_revision.map_or(0, |prev| prev.saturating_add(1));
    let collected_at_ms = unix_ms();
    let mut persistence = inputs.persistence;
    if inputs.memory_enabled && !persistence.contains(&PersistenceClass::MemoryIndex) {
        persistence.push(PersistenceClass::MemoryIndex);
    }

    EnvironmentManifest {
        schema_version: ENVIRONMENT_MANIFEST_SCHEMA_VERSION,
        run_id: inputs.run_id,
        session_id: inputs.session_id,
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        backend: Some(inputs.backend),
        sandbox_profile: inputs.sandbox_profile,
        container_routing: inputs.container_routing,
        project_root: inputs.project_root.to_string_lossy().replace('\\', "/"),
        project_root_hash: crate::tool_facts::project_root_hash(inputs.project_root),
        tools: inputs.inventory,
        executables,
        policy_scope: Some(inputs.policy_scope),
        persistence,
        restore: Some(RestoreCapabilities::current()),
        collected_at_ms,
        revision,
        freshness: Some(FreshnessStamp { collected_at_ms, revision }),
    }
}

/// Append the manifest as an `EnvironmentObserved` whiteboard event (the
/// authoritative per-run record resume restores from).
///
/// The payload is the manifest JSON itself — every field
/// `#[serde(default)]`-tolerant, so the row stays decodable as the envelope
/// grows additively.
pub async fn persist_manifest(
    pool: &sqlx::SqlitePool,
    session_id: &str,
    manifest: &EnvironmentManifest,
) -> Result<WhiteboardEvent, ManifestError> {
    let payload =
        serde_json::to_value(manifest).map_err(|error| ManifestError::Record(error.to_string()))?;
    let event = NewWhiteboardEvent {
        event_id: concerto_core::ids::Ulid::new().to_string(),
        agent_id: MANIFEST_AGENT_ID.to_string(),
        kind: WhiteboardKind::EnvironmentObserved,
        scope: String::new(),
        session_id: Some(session_id.to_string()),
        plan_id: None,
        causation: None,
        payload,
        pre_image_hash: None,
        created_at: manifest.collected_at_ms,
    };
    Ok(append_whiteboard_event(pool, &event).await?)
}

/// Restore the run's manifest from its newest `EnvironmentObserved` row.
///
/// This is the resume path: it performs **zero probes** (it takes no prober)
/// and returns `None` — fail-soft — when no row exists or the newest payload
/// fails envelope validation (including an unknown future `schema_version`,
/// which is rejected, never silently adopted).
pub async fn restore_environment_manifest(
    pool: &sqlx::SqlitePool,
    session_id: &str,
) -> Option<EnvironmentManifest> {
    let events = load_session_events_of_kind(
        pool,
        session_id,
        WhiteboardKind::EnvironmentObserved,
        RESTORE_SCAN_LIMIT,
    )
    .await
    .ok()?;
    let newest = events.last()?;
    let json = serde_json::to_string(&newest.payload).ok()?;
    EnvironmentManifest::from_json(&json).ok()
}

/// Manifest barrier beside the snapshot barrier: on resume, restore the
/// stored manifest (no probes); otherwise collect one probe set and persist
/// it best-effort. Always returns the manifest for context injection when one
/// could be produced — persistence failures degrade to a warning, never a
/// run failure.
pub async fn run_manifest_barrier(
    pool: Option<&sqlx::SqlitePool>,
    inputs: ManifestInputs<'_>,
    resume: bool,
    prober: &dyn VersionProber,
    cancel: &CancellationToken,
) -> Option<EnvironmentManifest> {
    if resume {
        if let Some(pool) = pool {
            if let Some(restored) = restore_environment_manifest(pool, &inputs.session_id).await {
                return Some(restored);
            }
            warn!(
                "no restorable environment manifest for resumed session — collecting a fresh one"
            );
        }
    }
    let manifest = collect_environment_manifest(inputs, prober, cancel).await;
    if let Some(pool) = pool {
        let session_id = manifest.session_id.clone();
        if let Err(error) = persist_manifest(pool, &session_id, &manifest).await {
            warn!(
                %error,
                "failed to persist EnvironmentObserved event; run continues with the in-memory manifest"
            );
        }
    } else {
        warn!(
            "no whiteboard pool available — EnvironmentObserved event not persisted; \
             planning still receives the manifest summary"
        );
    }
    Some(manifest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use concerto_core::policy::SimplePolicyEngine;
    use concerto_core::traits::policy::{AuditLog, PolicyEngine};
    use concerto_core::traits::tool::Tool as _;
    use concerto_core::types::{Condition, PolicyRule, ToolRegistry};
    use concerto_tools::filesystem::{FilesystemTool, WriteTool};
    use std::sync::Arc;

    /// No-op audit log for tests.
    struct TestAudit;

    #[async_trait]
    impl AuditLog for TestAudit {
        async fn record(
            &self,
            _entry: concerto_core::traits::policy::AuditEntry,
            _cancel: CancellationToken,
        ) -> Result<(), concerto_core::error::PolicyError> {
            Ok(())
        }
    }

    fn allow_all_executor() -> ToolExecutor {
        let allow_all = vec![PolicyRule::AutoApprove(Condition::Always)];
        let policy = Arc::new(SimplePolicyEngine::new(allow_all, Arc::new(TestAudit)));
        let registry = ToolRegistry::default();
        ToolExecutor::new(Arc::new(registry), policy)
    }

    fn test_inputs<'a>(
        project_root: &'a Path,
        inventory: Vec<ManifestToolEntry>,
    ) -> ManifestInputs<'a> {
        ManifestInputs {
            run_id: "run-1".to_string(),
            session_id: "sess-1".to_string(),
            project_root,
            backend: ManifestExecutionBackend::InProcess,
            sandbox_profile: None,
            container_routing: CommandRouting::Direct,
            inventory,
            policy_scope: PolicyScopeDescription::unspecified(),
            persistence: vec![PersistenceClass::WorkspaceFiles],
            memory_enabled: false,
            previous_revision: None,
            candidates: Some(&[]),
            path_dirs: Some(Vec::new()),
        }
    }

    #[test]
    fn inventory_lists_both_identities_sorted_with_alias_canonicalized() {
        let root: camino::Utf8PathBuf = "/proj".into();
        let mut registry = ToolRegistry::default();
        registry.register(Box::new(FilesystemTool::new(root.clone())));
        registry.register(Box::new(WriteTool::new(root)));
        let allow_all = vec![PolicyRule::AutoApprove(Condition::Always)];
        let policy = Arc::new(SimplePolicyEngine::new(allow_all, Arc::new(TestAudit)));
        let executor = ToolExecutor::new(Arc::new(registry), policy);

        let inventory = inventory_from_executor(&executor);
        let pairs: Vec<(&str, &str)> = inventory
            .iter()
            .map(|entry| (entry.registered_name.as_str(), entry.canonical_policy_name.as_str()))
            .collect();
        assert!(
            pairs.contains(&("filesystem", "filesystem")),
            "canonical tool under its own identity: {pairs:?}"
        );
        assert!(
            pairs.contains(&("write", "filesystem")),
            "the write alias canonicalizes to filesystem (slice-1 identity): {pairs:?}"
        );
        let mut sorted = pairs.clone();
        sorted.sort_unstable();
        assert_eq!(pairs, sorted, "inventory order is deterministic");
    }

    #[tokio::test]
    async fn probe_count_is_one_set_per_run_and_zero_per_prompt() {
        let dir = tempfile::tempdir().expect("tempdir created");
        for name in ["probe-aaa", "probe-bbb"] {
            let path = dir.path().join(name);
            std::fs::write(&path, b"#!/bin/sh\necho test-tool 9.9\n").expect("write fake exe");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mut perms = std::fs::metadata(&path).expect("metadata").permissions();
                perms.set_mode(0o755);
                std::fs::set_permissions(&path, perms).expect("chmod +x");
            }
        }
        let root = tempfile::tempdir().expect("tempdir created");
        let candidates: &[&str] = &["probe-aaa", "probe-bbb", "probe-missing-xyz"];
        let inputs = ManifestInputs {
            candidates: Some(candidates),
            path_dirs: Some(vec![dir.path().to_path_buf()]),
            ..test_inputs(root.path(), Vec::new())
        };
        let prober = CountingProber::default();

        let manifest =
            collect_environment_manifest(inputs, &prober, &CancellationToken::new()).await;
        assert_eq!(manifest.executables.len(), 3, "one observation per candidate");
        assert_eq!(prober.calls(), 2, "version probes run only for resolved executables");
        assert!(
            manifest.executables.iter().any(|exe| exe.name == "probe-missing-xyz"
                && exe.resolved_path.is_none()
                && exe.version.is_none()),
            "unresolved candidates still record an (empty) observation"
        );
        for exe in &manifest.executables {
            assert!(exe.observed_at_ms > 0, "every observation is freshness-stamped");
        }

        // Rendering context summaries performs zero further probes.
        for _ in 0..3 {
            let _ = manifest.summary();
        }
        assert_eq!(prober.calls(), 2, "no per-prompt re-probe");

        // An explicit re-collection runs exactly one more probe set and bumps
        // the revision.
        let refresh = ManifestInputs {
            candidates: Some(candidates),
            path_dirs: Some(vec![dir.path().to_path_buf()]),
            previous_revision: Some(manifest.revision),
            ..test_inputs(root.path(), Vec::new())
        };
        let second =
            collect_environment_manifest(refresh, &prober, &CancellationToken::new()).await;
        assert_eq!(prober.calls(), 4, "exactly one more probe set on refresh");
        assert_eq!(second.revision, manifest.revision + 1);
        assert_eq!(second.run_id, manifest.run_id, "re-collection keeps the run identity");
    }

    #[tokio::test]
    async fn cancelled_collection_skips_probes_but_still_returns_a_manifest() {
        let root = tempfile::tempdir().expect("tempdir created");
        let cancel = CancellationToken::new();
        cancel.cancel();
        let inputs =
            ManifestInputs { candidates: Some(&["cargo"]), ..test_inputs(root.path(), Vec::new()) };
        let prober = CountingProber::default();
        let manifest = collect_environment_manifest(inputs, &prober, &cancel).await;
        assert!(manifest.executables.is_empty(), "cancelled probes are absent, not failed");
        assert_eq!(prober.calls(), 0);
        assert_eq!(manifest.schema_version, ENVIRONMENT_MANIFEST_SCHEMA_VERSION);
    }

    #[tokio::test]
    async fn system_prober_returns_none_without_spawning_on_cancel() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let prober = SystemVersionProber::default();
        let none = prober.query_version(Path::new("/definitely/not/here"), &cancel).await;
        assert!(none.is_none());
    }

    #[tokio::test]
    async fn policy_evaluation_is_unchanged_with_and_without_a_manifest() {
        // Structural fact under test: `PolicyEngine::evaluate` takes a
        // `&PolicyAction` and a token — there is no manifest parameter, no
        // `From<EnvironmentManifest>` into any policy type, and no manifest
        // field on the action. This test pins the behavior: collecting (and
        // holding) a manifest cannot change any verdict.
        use concerto_core::ids::Ulid;
        use concerto_core::types::{PolicyAction, SessionContext};

        let allow_all = vec![PolicyRule::AutoApprove(Condition::Always)];
        let engine = SimplePolicyEngine::new(allow_all, Arc::new(TestAudit));
        let session = SessionContext::new(Ulid::new(), PathBuf::from("/proj"));
        let input = serde_json::json!({ "operation": "write", "path": "a.md" });
        let tool = concerto_tools::filesystem::FilesystemTool::new("/proj".into());
        let action = PolicyAction {
            tool_name: "filesystem",
            input: &input,
            session_id: session.session_id,
            correlation_id: Ulid::new(),
            capability_requirements: tool.capability_requirements(),
            sandbox_profile: tool.sandbox_profile(),
            estimated_cost_usd: None,
            command_facts: None,
            path_facts: None,
            orchestrator_authority: false,
        };
        let before = engine.evaluate(&action, CancellationToken::new()).await.expect("evaluates");

        // A manifest is collected and held — with nowhere to feed it.
        let root = tempfile::tempdir().expect("tempdir created");
        let manifest = collect_environment_manifest(
            test_inputs(root.path(), inventory_from_executor(&allow_all_executor())),
            &CountingProber::default(),
            &CancellationToken::new(),
        )
        .await;
        let _held = manifest.summary();

        let after = engine.evaluate(&action, CancellationToken::new()).await.expect("evaluates");
        assert_eq!(before, after, "the verdict is identical whether or not a manifest exists");
    }

    /// File-backed pool with the same PRAGMAs as production and all sessions
    /// migrations applied (the workspace-snapshot barrier's test pattern).
    async fn test_pool() -> (tempfile::TempDir, sqlx::SqlitePool) {
        let dir = tempfile::tempdir().expect("tempdir created");
        let path = dir.path().join("manifest_test.db");
        let options = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .busy_timeout(std::time::Duration::from_secs(5))
            .foreign_keys(true)
            .synchronous(sqlx::sqlite::SqliteSynchronous::Normal);
        let pool = sqlx::pool::PoolOptions::new()
            .max_connections(4)
            .connect_with(options)
            .await
            .expect("test pool connects");
        sqlx::migrate!("../sessions/migrations")
            .run(&pool)
            .await
            .expect("sessions migrations apply");
        (dir, pool)
    }

    fn collected_manifest(root: &Path, revision: Option<u64>) -> EnvironmentManifest {
        // Synchronous scaffold: real collection is async (probes); tests that
        // need probing use `collect_environment_manifest` directly. This
        // builds the deterministic non-probe half for persistence tests.
        EnvironmentManifest {
            schema_version: ENVIRONMENT_MANIFEST_SCHEMA_VERSION,
            run_id: "run-9".to_string(),
            session_id: "sess-9".to_string(),
            os: std::env::consts::OS.to_string(),
            arch: std::env::consts::ARCH.to_string(),
            backend: Some(ManifestExecutionBackend::InProcess),
            sandbox_profile: None,
            container_routing: CommandRouting::Direct,
            project_root: root.to_string_lossy().replace('\\', "/"),
            project_root_hash: crate::tool_facts::project_root_hash(root),
            tools: vec![ManifestToolEntry {
                registered_name: "write".to_string(),
                canonical_policy_name: "filesystem".to_string(),
            }],
            executables: Vec::new(),
            policy_scope: Some(PolicyScopeDescription::for_run(None, 9, None)),
            persistence: vec![PersistenceClass::WorkspaceFiles],
            restore: Some(RestoreCapabilities::current()),
            collected_at_ms: 1_700_000_000_000,
            revision: revision.unwrap_or(0),
            freshness: Some(FreshnessStamp {
                collected_at_ms: 1_700_000_000_000,
                revision: revision.unwrap_or(0),
            }),
        }
    }

    #[tokio::test]
    async fn whiteboard_round_trip_preserves_the_manifest() {
        let (_dir, pool) = test_pool().await;
        let root = tempfile::tempdir().expect("tempdir created");
        let manifest = collected_manifest(root.path(), None);

        let stored = persist_manifest(&pool, "sess-9", &manifest).await.expect("persists");
        assert_eq!(stored.kind, WhiteboardKind::EnvironmentObserved);
        assert_eq!(
            serde_json::to_value(WhiteboardKind::EnvironmentObserved).expect("kind"),
            serde_json::json!("environment-observed"),
            "additive kebab-case kind"
        );

        let restored = restore_environment_manifest(&pool, "sess-9").await.expect("restores");
        assert_eq!(restored, manifest, "resume restores the exact manifest, revision included");
    }

    #[tokio::test]
    async fn resume_restores_the_newest_manifest_with_zero_probes() {
        let (_dir, pool) = test_pool().await;
        let root = tempfile::tempdir().expect("tempdir created");
        persist_manifest(&pool, "sess-9", &collected_manifest(root.path(), Some(0)))
            .await
            .expect("persists rev 0");
        persist_manifest(&pool, "sess-9", &collected_manifest(root.path(), Some(1)))
            .await
            .expect("persists rev 1");

        // Restore takes no prober — structurally incapable of probing.
        let restored = restore_environment_manifest(&pool, "sess-9").await.expect("restores");
        assert_eq!(restored.revision, 1, "resume picks the newest collection");
        assert_eq!(restored.run_id, "run-9", "the run identity survives resume");
    }

    #[tokio::test]
    async fn restore_is_fail_soft_without_rows_or_with_future_versions() {
        let (_dir, pool) = test_pool().await;
        assert!(
            restore_environment_manifest(&pool, "sess-empty").await.is_none(),
            "no rows ⇒ no manifest, never an error"
        );

        // A future-version payload is rejected, never silently adopted.
        let root = tempfile::tempdir().expect("tempdir created");
        let mut future = collected_manifest(root.path(), None);
        future.schema_version = ENVIRONMENT_MANIFEST_SCHEMA_VERSION + 1;
        let event = NewWhiteboardEvent {
            event_id: concerto_core::ids::Ulid::new().to_string(),
            agent_id: MANIFEST_AGENT_ID.to_string(),
            kind: WhiteboardKind::EnvironmentObserved,
            scope: String::new(),
            session_id: Some("sess-future".to_string()),
            plan_id: None,
            causation: None,
            payload: serde_json::to_value(&future).expect("to value"),
            pre_image_hash: None,
            created_at: future.collected_at_ms,
        };
        append_whiteboard_event(&pool, &event).await.expect("appends future row");
        assert!(
            restore_environment_manifest(&pool, "sess-future").await.is_none(),
            "unknown future versions fail soft to no-manifest"
        );
    }

    #[tokio::test]
    async fn barrier_collects_persists_and_restores_on_resume() {
        let (_dir, pool) = test_pool().await;
        let root = tempfile::tempdir().expect("tempdir created");
        let prober = CountingProber::default();

        let first = run_manifest_barrier(
            Some(&pool),
            ManifestInputs {
                run_id: "run-barrier".to_string(),
                session_id: "sess-barrier".to_string(),
                ..test_inputs(root.path(), Vec::new())
            },
            false,
            &prober,
            &CancellationToken::new(),
        )
        .await
        .expect("barrier collects");
        assert_eq!(first.revision, 0);

        // Resume restores from the event: no new probes, same revision.
        let resumed = run_manifest_barrier(
            Some(&pool),
            ManifestInputs {
                run_id: "run-barrier-new-identity".to_string(),
                session_id: "sess-barrier".to_string(),
                ..test_inputs(root.path(), Vec::new())
            },
            true,
            &prober,
            &CancellationToken::new(),
        )
        .await
        .expect("barrier restores");
        assert_eq!(resumed.revision, 0);
        assert_eq!(resumed.run_id, "run-barrier", "resume keeps the stored run identity");

        // No pool: still collects for context, persists nothing.
        let pool_less = run_manifest_barrier(
            None,
            ManifestInputs {
                run_id: "run-x".to_string(),
                session_id: "sess-x".to_string(),
                ..test_inputs(root.path(), Vec::new())
            },
            false,
            &prober,
            &CancellationToken::new(),
        )
        .await
        .expect("pool-less barrier still collects");
        assert_eq!(pool_less.run_id, "run-x");
    }
}
