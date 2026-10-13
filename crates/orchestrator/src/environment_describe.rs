//! ADR-82 H03 — `environment.describe`: agent-callable, read-only access to
//! the slice-6 [`EnvironmentManifest`].
//!
//! An agent calls this through the normal [`ToolExecutor`][concerto_core::executor::ToolExecutor]
//! path and gets back the run's cached manifest (or a bounded digest of it).
//! It is descriptive and read-only: it never grants, denies, or widens
//! anything, and no policy path reads the manifest — the operation goes
//! through normal policy evaluation as an ordinary read (an unmatched read
//! hits the read-only ceiling / default posture like any other read; there is
//! deliberately no bypass, not even via [`ToolExecutor::execute_read_only`][concerto_core::executor::ToolExecutor::execute_read_only]).
//!
//! # Reachability: both paths, one registry
//!
//! The tool is registered once in the run's shared registry
//! ([`register_describe_tool`], called from `setup_shared_tooling` after all
//! plugin/MCP tools), so it is reachable from **both** the single-agent loop
//! and the coordinator: both consume the same registry/executor pair, and a
//! per-path registry fork would split the tool surface for no benefit.
//! Prompt injection of the manifest digest runs on **both** paths through the
//! shared [`crate::environment_manifest::format_manifest_block`] shape (the
//! slice-6 coordinator block plus the single-agent `PromptBuilder` injection);
//! `environment.describe` remains the on-demand read of the same cached
//! manifest.
//!
//! # Probe discipline: one set per run, never per call
//!
//! The tool reuses the slice-6 collector/cache and never probes per call:
//!
//! - **Both paths:** the shared manifest barrier collects one probe set at run
//!   start (or restores the stored row on resume) and hands the result to
//!   [`populate_from_barrier`]; every describe call after that is a pure cache
//!   read with zero probes.
//! - **Fallback (no barrier manifest):** the first describe call lazily
//!   collects exactly one probe set and caches it; every later call is a
//!   cache read. This covers runs whose barrier produced no manifest —
//!   including every pre-barrier single-agent run — never the steady state.
//!
//! # Revision invalidation on config change (approach (b): lazy detect +
//! explicit re-collect)
//!
//! Config is frozen within a run (ADR-57 next-run semantics: a reloaded config
//! flows into the *next* run via a fresh `AppConfig`, never into an in-flight
//! one), so hooking the ADR-57 watcher into the run would violate the config
//! architecture. Instead the operation detects a stale revision and
//! re-collects lazily under the same probe discipline:
//!
//! - Every cached manifest carries a blake3 **configuration fingerprint**
//!   ([`fingerprint_of_manifest`]) over the manifest-relevant config surface:
//!   project root, backend/sandbox/routing labels, policy-scope labels,
//!   persistence labels, and the sorted registered tool names (canonical alias
//!   identities are deliberately excluded — membership is the config-change
//!   signal).
//! - Each call recomputes the live fingerprint from the run facts
//!   ([`fingerprint_of_facts`]) captured at registration. On mismatch the
//!   cached revision is stale — the tool performs an **explicit re-collection**
//!   (the collector's documented refresh path: `previous_revision` set, so the
//!   revision bumps and the run identity is kept), persists the new
//!   `EnvironmentObserved` row best-effort, and serves the fresh manifest. A
//!   stale manifest is therefore never returned as current.
//!
//! The genuine production trigger is **resume across a config change**: the
//! barrier restores the previous run's row (collected under the old config),
//! the first describe call observes the fingerprint mismatch, and the revision
//! bumps exactly once. Fresh-runs and same-config resumes match on the first
//! comparison and never re-probe.
//!
//! # Sandbox capability reporting (H10, ADR-83)
//!
//! Every describe rendering carries the run's [`SandboxCapabilityDescriptor`]
//! (H10 portable capability interface): the run profile evaluated against the
//! registration-time runtime probe. It is descriptive only — never an
//! admission gate — and offers no restore coverage (no container/runtime
//! checkpoint-restore backend ships in this milestone). The probe result is
//! captured once at registration (the cached system probe: one `PATH` scan
//! per process, zero subprocess spawns), so reporting performs zero per-call
//! probes; it is point-in-time observation like the executable set and is
//! deliberately NOT fingerprinted as config.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use concerto_core::environment_manifest::{
    EnvironmentManifest, ManifestExecutionBackend, ManifestToolEntry, PersistenceClass,
    PolicyScopeDescription,
};
use concerto_core::sandbox::{
    detect_container_runtime, sandbox_capability_for, RuntimeAvailability,
    SandboxCapabilityDescriptor,
};
use concerto_core::traits::tool::Tool;
use concerto_core::traits::PolicyEngine;
use concerto_core::types::{
    CapabilitySet, CommandRouting, SandboxProfile, SessionContext, ToolOutput, ToolRegistry,
};
use concerto_core::{CancellationToken, ToolError};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::environment_manifest::{
    collect_environment_manifest, persist_manifest, ManifestInputs, SystemVersionProber,
    VersionProber,
};

/// Registry name of the H03 operation. Dotted to read as an operation under
/// the `environment` namespace (the `mcp:<server>:<tool>` precedent shows
/// non-alphanumeric names are ordinary registry keys, matched by equality).
pub const ENVIRONMENT_DESCRIBE_TOOL_NAME: &str = "environment.describe";

/// Default full-detail tool-entry bound: a bounded default payload no matter
/// how large the registry (plugin/MCP tools included) grows.
pub const ENVIRONMENT_DESCRIBE_DEFAULT_MAX_TOOLS: usize = 32;

/// Hard ceiling for `max_tools`, mirroring the filesystem `list` 500-entry
/// cap (`crates/tools/src/filesystem.rs`): a caller can ask for detail, never
/// for an unbounded dump.
pub const ENVIRONMENT_DESCRIBE_MAX_TOOLS_CEILING: usize = 500;

/// Default executable-observation bound (the collection candidate set is
/// already tiny; this caps custom-candidate runs).
pub const ENVIRONMENT_DESCRIBE_DEFAULT_MAX_EXECUTABLES: usize = 32;

/// Hard ceiling for `max_executables`, same precedent as the tool cap.
pub const ENVIRONMENT_DESCRIBE_MAX_EXECUTABLES_CEILING: usize = 500;

/// Detail level for one describe call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DescribeDetail {
    /// Bounded digest: the manifest [`summary`][EnvironmentManifest::summary]
    /// plus scalar revision/freshness/count fields. Always complete, never
    /// truncated — the digest itself folds the tool list at
    /// `ENVIRONMENT_MANIFEST_DIGEST_MAX_TOOLS`.
    #[default]
    Summary,
    /// The manifest body with tool/executable arrays truncated to the
    /// effective caps (with `truncated_*` flags and `total_*` counts).
    Full,
}

/// Input for the `environment.describe` operation. Every field is
/// `#[serde(default)]`-tolerant (R6 wire discipline): `{}` means
/// "summary with default caps", and newly added fields will never break old
/// callers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct EnvironmentDescribeInput {
    /// Detail level (default: summary).
    #[serde(default)]
    pub detail: DescribeDetail,
    /// Tool-entry bound override, clamped to
    /// [`ENVIRONMENT_DESCRIBE_MAX_TOOLS_CEILING`] (default:
    /// [`ENVIRONMENT_DESCRIBE_DEFAULT_MAX_TOOLS`]).
    #[serde(default)]
    pub max_tools: Option<u32>,
    /// Executable-observation bound override, clamped to
    /// [`ENVIRONMENT_DESCRIBE_MAX_EXECUTABLES_CEILING`] (default:
    /// [`ENVIRONMENT_DESCRIBE_DEFAULT_MAX_EXECUTABLES`]).
    #[serde(default)]
    pub max_executables: Option<u32>,
}

/// Immutable run facts the operation renders and re-collects from.
///
/// Captured once at registration from the run's `AppConfig`, project dir, and
/// registry contents; config is frozen for the run (ADR-57 next-run
/// semantics), so these never change mid-run. Not serialized anywhere —
// (internal run state, no wire-compat burden).
#[derive(Debug, Clone)]
pub struct DescribeFacts {
    /// Workspace root the manifest describes.
    pub project_root: PathBuf,
    /// Descriptive backend label (mirrors the barrier: in-process).
    pub backend: ManifestExecutionBackend,
    /// Run-level sandbox profile, when configured (mirrors the barrier).
    pub sandbox_profile: Option<SandboxProfile>,
    /// Command-routing assertion (mirrors the barrier).
    pub container_routing: CommandRouting,
    /// Descriptive policy-scope labels derived from config.
    pub policy_scope: PolicyScopeDescription,
    /// Persistence base classes (workspace files + session DB when a pool
    /// exists; `MemoryIndex` is added by [`Self::normalized_persistence`]
    /// when memory is enabled — the same rule the collector applies).
    pub persistence: Vec<PersistenceClass>,
    /// Whether the memory index persists for this run.
    pub memory_enabled: bool,
    /// Sorted registered tool names at registration (plus this tool's own
    /// name), the membership half of the fingerprint.
    pub tool_names: Vec<String>,
    /// Container-runtime probe result captured at registration (the cached
    /// system probe — one `PATH` scan per process, zero subprocess spawns),
    /// so sandbox capability reporting performs zero per-call probes.
    /// Point-in-time observation like the executable set: deliberately NOT
    /// fingerprinted as config (a runtime installed mid-session changes what
    /// is observed, not what the run was configured with).
    pub sandbox_runtime: RuntimeAvailability,
    /// Probe-candidate override (`None` selects the collector default).
    pub probe_candidates: Option<Vec<String>>,
    /// `PATH`-directory override for hermetic tests (`None` reads `PATH`).
    pub probe_path_dirs: Option<Vec<PathBuf>>,
    /// H13 `[harness].declared_capabilities`: descriptive labels the operator
    /// declares for their environment. Descriptive only — never an admission
    /// gate and never read by any authorization path.
    pub declared_capabilities: Vec<String>,
}

impl DescribeFacts {
    /// Persistence classes for collection: the base plus `MemoryIndex` when
    /// memory is enabled — the exact rule [`collect_environment_manifest`]
    /// applies, so facts-derived and collector-derived manifests agree.
    pub fn normalized_persistence(&self) -> Vec<PersistenceClass> {
        let mut persistence = self.persistence.clone();
        if self.memory_enabled && !persistence.contains(&PersistenceClass::MemoryIndex) {
            persistence.push(PersistenceClass::MemoryIndex);
        }
        persistence
    }

    /// Project root in manifest form (forward slashes), matching the
    /// collector's grammar.
    pub fn project_root_string(&self) -> String {
        self.project_root.to_string_lossy().replace('\\', "/")
    }
}

/// Derive the descriptive policy scope from config, shared by the manifest
/// barrier and this operation so both label the run identically.
///
/// `configured_rules` is `Some(n)` when the operator configured `n` policy
/// rules, `None` on concerto defaults (`default_rule_count` long).
pub fn policy_scope_from_config(config: &concerto_config::AppConfig) -> PolicyScopeDescription {
    let configured_rules: Option<usize> = config
        .policy
        .as_ref()
        .map(|policy| policy.to_rules())
        .filter(|rules| !rules.is_empty())
        .map(|rules| rules.len());
    let default_rule_count = concerto_core::policy_presets::PolicyPresets::default_rules().len();
    PolicyScopeDescription::for_run(configured_rules, default_rule_count, None)
}

/// Normalized fingerprint inputs: the scalar config labels plus the two
/// label sets (sorted + deduped at hash time, so ordering never matters).
struct FingerprintInput {
    project_root: String,
    backend: String,
    sandbox: String,
    routing: String,
    ceiling: String,
    write_preset: String,
    scope_sandbox: String,
    persistence_labels: Vec<String>,
    tool_names: Vec<String>,
}

/// Core fingerprint: blake3 over the manifest-relevant config surface. Both
/// [`fingerprint_of_facts`] (live) and [`fingerprint_of_manifest`] (cached)
/// normalize through here, so equal configurations hash equal regardless of
/// which side produced them.
fn describe_fingerprint(mut input: FingerprintInput) -> String {
    input.persistence_labels.sort();
    input.persistence_labels.dedup();
    input.tool_names.sort();
    input.tool_names.dedup();
    let mut hasher = blake3::Hasher::new();
    for part in [
        &input.project_root,
        &input.backend,
        &input.sandbox,
        &input.routing,
        &input.ceiling,
        &input.write_preset,
        &input.scope_sandbox,
    ] {
        hasher.update(part.as_bytes());
        hasher.update(b"\0");
    }
    for label in input.persistence_labels.iter().chain(input.tool_names.iter()) {
        hasher.update(label.as_bytes());
        hasher.update(b"\0");
    }
    hasher.finalize().to_hex().to_string()
}

/// Live fingerprint from the run facts captured at registration.
pub fn fingerprint_of_facts(facts: &DescribeFacts) -> String {
    describe_fingerprint(FingerprintInput {
        project_root: facts.project_root_string(),
        backend: facts.backend.as_str().to_string(),
        sandbox: facts.sandbox_profile.map_or("none", |profile| profile.as_str()).to_string(),
        routing: format!("{:?}", facts.container_routing),
        ceiling: facts.policy_scope.ceiling.clone(),
        write_preset: facts.policy_scope.write_preset.clone(),
        scope_sandbox: facts.policy_scope.sandbox.clone(),
        persistence_labels: facts
            .normalized_persistence()
            .iter()
            .map(|class| class.as_str().to_string())
            .collect(),
        tool_names: facts.tool_names.clone(),
    })
}

/// Fingerprint of a cached manifest over the same normalized surface. Unknown
/// (`None`) facets hash as their honest labels (`"unknown"` backend,
/// `"unspecified"` scope) so a sparse legacy manifest simply fingerprints
/// differently — never equal to a fully-described live run by accident.
pub fn fingerprint_of_manifest(manifest: &EnvironmentManifest) -> String {
    let (ceiling, write_preset, scope_sandbox) = manifest.policy_scope.as_ref().map_or(
        ("unspecified".to_string(), "unspecified".to_string(), "unspecified".to_string()),
        |scope| (scope.ceiling.clone(), scope.write_preset.clone(), scope.sandbox.clone()),
    );
    describe_fingerprint(FingerprintInput {
        project_root: manifest.project_root.clone(),
        backend: manifest.backend.map_or("unknown", |backend| backend.as_str()).to_string(),
        sandbox: manifest.sandbox_profile.map_or("none", |profile| profile.as_str()).to_string(),
        routing: format!("{:?}", manifest.container_routing),
        ceiling,
        write_preset,
        scope_sandbox,
        persistence_labels: manifest
            .persistence
            .iter()
            .map(|class| class.as_str().to_string())
            .collect(),
        tool_names: manifest.tools.iter().map(|entry| entry.registered_name.clone()).collect(),
    })
}

/// The run's sandbox capability descriptor (H10): the run profile (or `None`
/// when unconfigured) evaluated against the registration-time runtime probe.
///
/// Pure — zero probes per call. The descriptor is descriptive only (never an
/// admission gate) and offers no restore coverage: no container/runtime
/// checkpoint-restore backend ships in this milestone (ADR-83).
pub fn describe_sandbox_capability(facts: &DescribeFacts) -> SandboxCapabilityDescriptor {
    sandbox_capability_for(
        facts.sandbox_profile.unwrap_or(SandboxProfile::None),
        &facts.sandbox_runtime,
    )
}

/// Mutable per-run describe cache behind [`SharedDescribeState`].
pub struct DescribeState {
    /// The cached manifest: barrier-populated on the coordinator path, lazily
    /// collected on the first single-agent call, refreshed on fingerprint
    /// mismatch. `None` until one of those fills it.
    pub manifest: Option<EnvironmentManifest>,
    /// [`fingerprint_of_manifest`] of the cached manifest, captured when it
    /// was stored so staleness is one string comparison.
    pub manifest_fingerprint: Option<String>,
    /// Immutable run facts (live side of the comparison + re-collection
    /// inputs).
    pub facts: DescribeFacts,
    /// Who answers `<exe> --version` on (re-)collection. Production uses
    /// [`SystemVersionProber`]; tests inject the counting stub.
    pub prober: Arc<dyn VersionProber>,
    /// Session-DB pool for best-effort `EnvironmentObserved` persistence of
    /// lazily (re-)collected manifests (`None` disables the seam).
    pub pool: Option<sqlx::SqlitePool>,
}

/// Shared per-run describe cache: cloned into the registered tool and into
/// the run wiring (barrier populate). `std::sync::Mutex` with short critical
/// sections — never held across `.await` (snapshots are cloned out first).
pub type SharedDescribeState = Arc<Mutex<DescribeState>>;

/// Create the shared per-run describe cache.
pub fn new_shared_state(
    facts: DescribeFacts,
    prober: Arc<dyn VersionProber>,
    pool: Option<sqlx::SqlitePool>,
) -> SharedDescribeState {
    Arc::new(Mutex::new(DescribeState {
        manifest: None,
        manifest_fingerprint: None,
        facts,
        prober,
        pool,
    }))
}

/// Populate the cache from the manifest barrier (coordinator path): stores the
/// barrier's manifest — freshly collected or resume-restored — with its
/// fingerprint. Run facts are deliberately NOT touched: the live side stays
/// the current run's facts, so a resume-restored manifest collected under an
/// older config fingerprints stale on the first describe call and refreshes
/// (the H03 invalidation acceptance).
pub fn populate_from_barrier(state: &SharedDescribeState, manifest: &EnvironmentManifest) {
    let fingerprint = fingerprint_of_manifest(manifest);
    let mut guarded = state.lock().unwrap_or_else(|error| error.into_inner());
    guarded.manifest = Some(manifest.clone());
    guarded.manifest_fingerprint = Some(fingerprint);
}

/// Build run facts + shared cache and register the operation into `registry`.
///
/// Must be called LAST (after plugin/MCP registration) so the tool-names
/// snapshot covers every other tool; the operation's own name is appended
/// explicitly since it is not yet registered when the snapshot is taken.
/// Returns the shared handle for the run wiring (`SharedTooling` →
/// barrier populate).
pub fn register_describe_tool(
    registry: &mut ToolRegistry,
    project_root: &Path,
    config: &concerto_config::AppConfig,
    session_db_present: bool,
    memory_enabled: bool,
    pool: Option<sqlx::SqlitePool>,
) -> SharedDescribeState {
    let mut tool_names: Vec<String> =
        registry.all_tool_definitions().iter().map(|definition| definition.name.clone()).collect();
    tool_names.push(ENVIRONMENT_DESCRIBE_TOOL_NAME.to_string());
    tool_names.sort();
    let mut persistence = vec![PersistenceClass::WorkspaceFiles];
    if session_db_present {
        persistence.push(PersistenceClass::SessionDatabase);
    }
    let facts = DescribeFacts {
        project_root: project_root.to_path_buf(),
        backend: ManifestExecutionBackend::InProcess,
        sandbox_profile: None,
        container_routing: CommandRouting::Direct,
        policy_scope: policy_scope_from_config(config),
        persistence,
        memory_enabled,
        tool_names,
        probe_candidates: None,
        probe_path_dirs: None,
        declared_capabilities: config
            .harness
            .as_ref()
            .map(|harness| harness.declared_capabilities.clone())
            .unwrap_or_default(),
        // One cached probe at registration: zero per-call probes after this.
        sandbox_runtime: detect_container_runtime(),
    };
    let state = new_shared_state(facts, Arc::new(SystemVersionProber::default()), pool);
    registry.register(Box::new(EnvironmentDescribeTool { state: state.clone() }));
    state
}

/// The `environment.describe` operation: read-only, descriptive access to the
/// run's cached [`EnvironmentManifest`].
///
/// Empty capability requirements (the LSP/MCP-bridge precedent): every agent
/// may discover it; whether a call *runs* is the policy engine's job through
/// the normal [`ToolExecutor`][concerto_core::executor::ToolExecutor] gate.
/// Default `policy_view` (own name), no command/path facts — the operation
/// names no executable and no path, so there is nothing attributable to
/// enrich, and nothing here may feed an authorization decision.
pub struct EnvironmentDescribeTool {
    state: SharedDescribeState,
}

impl EnvironmentDescribeTool {
    /// Attach to an existing shared cache (registration + tests).
    pub fn with_state(state: SharedDescribeState) -> Self {
        Self { state }
    }
}

/// Clamp an optional caller bound to (`default`, `ceiling`).
fn effective_cap(requested: Option<u32>, default: usize, ceiling: usize) -> usize {
    requested.map_or(default, |value| (value as usize).min(ceiling))
}

/// Bounded summary rendering: the manifest digest plus scalar
/// revision/freshness/count fields. Complete by construction — the digest
/// itself folds the tool list, so no truncation flags are needed.
fn render_summary(
    manifest: &EnvironmentManifest,
    sandbox_capability: &SandboxCapabilityDescriptor,
    declared_capabilities: &[String],
) -> ToolOutput {
    let backend = manifest.backend.map_or("unknown", |backend| backend.as_str());
    ToolOutput {
        summary: manifest.summary(),
        data: serde_json::json!({
            "schema_version": manifest.schema_version,
            "run_id": manifest.run_id,
            "session_id": manifest.session_id,
            "backend": backend,
            "os": manifest.os,
            "arch": manifest.arch,
            "sandbox": manifest.sandbox_profile.map_or("none", |profile| profile.as_str()),
            "routing": format!("{:?}", manifest.container_routing),
            "project_root": manifest.project_root,
            "revision": manifest.revision,
            "collected_at_ms": manifest.collected_at_ms,
            "tool_count": manifest.tools.len(),
            "executable_count": manifest.executables.len(),
            "policy_scope": manifest.policy_scope,
            "persistence": manifest.persistence.iter().map(|class| class.as_str()).collect::<Vec<_>>(),
            "restore": manifest.restore,
            "sandbox_capability": sandbox_capability,
            "declared_capabilities": declared_capabilities,
            "summary": manifest.summary(),
        }),
    }
}

/// Bounded full rendering: the manifest body with tool/executable arrays cut
/// to the effective caps, with `truncated_*` flags and `total_*` counts so a
/// caller can tell partial from complete (the filesystem-`list` shape).
fn render_full(
    manifest: &EnvironmentManifest,
    max_tools: Option<u32>,
    max_executables: Option<u32>,
    sandbox_capability: &SandboxCapabilityDescriptor,
    declared_capabilities: &[String],
) -> ToolOutput {
    let tools_cap = effective_cap(
        max_tools,
        ENVIRONMENT_DESCRIBE_DEFAULT_MAX_TOOLS,
        ENVIRONMENT_DESCRIBE_MAX_TOOLS_CEILING,
    );
    let executables_cap = effective_cap(
        max_executables,
        ENVIRONMENT_DESCRIBE_DEFAULT_MAX_EXECUTABLES,
        ENVIRONMENT_DESCRIBE_MAX_EXECUTABLES_CEILING,
    );
    let mut value = serde_json::to_value(manifest).unwrap_or(serde_json::Value::Null);
    let total_tools = manifest.tools.len();
    let total_executables = manifest.executables.len();
    let truncated_tools = total_tools > tools_cap;
    let truncated_executables = total_executables > executables_cap;
    if let Some(object) = value.as_object_mut() {
        if let Some(tools) = object.get("tools").and_then(|tools| tools.as_array()).cloned() {
            object.insert(
                "tools".to_string(),
                tools.into_iter().take(tools_cap).collect::<Vec<_>>().into(),
            );
        }
        if let Some(executables) =
            object.get("executables").and_then(|executables| executables.as_array()).cloned()
        {
            object.insert(
                "executables".to_string(),
                executables.into_iter().take(executables_cap).collect::<Vec<_>>().into(),
            );
        }
        object.insert("truncated_tools".to_string(), truncated_tools.into());
        object.insert("total_tools".to_string(), total_tools.into());
        object.insert("truncated_executables".to_string(), truncated_executables.into());
        object.insert("total_executables".to_string(), total_executables.into());
        object.insert(
            "sandbox_capability".to_string(),
            serde_json::to_value(sandbox_capability).unwrap_or(serde_json::Value::Null),
        );
        object.insert(
            "declared_capabilities".to_string(),
            serde_json::to_value(declared_capabilities).unwrap_or(serde_json::Value::Null),
        );
    }
    ToolOutput {
        summary: format!(
            "environment revision={} tools={}{} executables={}{}",
            manifest.revision,
            total_tools.min(tools_cap),
            if truncated_tools { format!(" of {total_tools} (truncated)") } else { String::new() },
            total_executables.min(executables_cap),
            if truncated_executables {
                format!(" of {total_executables} (truncated)")
            } else {
                String::new()
            },
        ),
        data: value,
    }
}

/// Snapshot of the cache fields one call needs (cloned out from under the
/// lock before any `.await`).
struct DescribeSnapshot {
    manifest: Option<EnvironmentManifest>,
    manifest_fingerprint: Option<String>,
    facts: DescribeFacts,
    prober: Arc<dyn VersionProber>,
    pool: Option<sqlx::SqlitePool>,
}

fn snapshot_state(state: &SharedDescribeState) -> DescribeSnapshot {
    let guarded = state.lock().unwrap_or_else(|error| error.into_inner());
    DescribeSnapshot {
        manifest: guarded.manifest.clone(),
        manifest_fingerprint: guarded.manifest_fingerprint.clone(),
        facts: guarded.facts.clone(),
        prober: guarded.prober.clone(),
        pool: guarded.pool.clone(),
    }
}

fn store_manifest(
    state: &SharedDescribeState,
    manifest: &EnvironmentManifest,
    fingerprint: String,
) {
    let mut guarded = state.lock().unwrap_or_else(|error| error.into_inner());
    guarded.manifest = Some(manifest.clone());
    guarded.manifest_fingerprint = Some(fingerprint);
}

/// (Re-)collect one manifest revision from run facts: the lazy single-agent
/// path (`previous_revision: None`, fresh run id) and the stale-revision
/// refresh (`previous_revision: Some`, run identity kept — the collector's
/// explicit-refresh path, which bumps the revision).
async fn collect_from_facts(
    facts: &DescribeFacts,
    run_id: String,
    session_id: String,
    previous_revision: Option<u64>,
    prober: &dyn VersionProber,
    cancel: &CancellationToken,
) -> EnvironmentManifest {
    let candidates_owned = facts.probe_candidates.clone();
    let candidates_ref: Option<Vec<&str>> =
        candidates_owned.as_ref().map(|names| names.iter().map(String::as_str).collect());
    let inventory: Vec<ManifestToolEntry> = facts
        .tool_names
        .iter()
        .map(|name| ManifestToolEntry {
            // Registration-time snapshot: canonical identities are unknown
            // without an executor, so both identities carry the registered
            // name — the collector's own unknown-tool fallback convention
            // (`inventory_from_executor`), truthful either way.
            registered_name: name.clone(),
            canonical_policy_name: name.clone(),
        })
        .collect();
    let inputs = ManifestInputs {
        run_id,
        session_id,
        project_root: &facts.project_root,
        backend: facts.backend,
        sandbox_profile: facts.sandbox_profile,
        container_routing: facts.container_routing,
        inventory,
        policy_scope: facts.policy_scope.clone(),
        persistence: facts.persistence.clone(),
        memory_enabled: facts.memory_enabled,
        previous_revision,
        candidates: candidates_ref.as_deref(),
        path_dirs: facts.probe_path_dirs.clone(),
    };
    collect_environment_manifest(inputs, prober, cancel).await
}

#[async_trait]
impl Tool for EnvironmentDescribeTool {
    fn name(&self) -> &str {
        ENVIRONMENT_DESCRIBE_TOOL_NAME
    }

    fn description(&self) -> &str {
        "Describe the current run's environment: workspace scope, execution backend, available tools, probed executables, persistence classes, and restore limits. Read-only; the manifest is descriptive and never authorizes anything."
    }

    fn input_schema(&self) -> serde_json::Value {
        // Derive the schema from the Rust type so the advertised contract can
        // never drift from the deserialization target. Every field has a
        // default, so none is required.
        let root = schemars::schema_for!(EnvironmentDescribeInput);
        serde_json::to_value(&root).unwrap_or_else(|error| {
            tracing::error!(%error, "failed to serialize EnvironmentDescribeInput schema");
            serde_json::json!({
                "type": "object",
                "properties": {
                    "detail": { "type": "string", "enum": ["summary", "full"] },
                    "max_tools": { "type": ["integer", "null"] },
                    "max_executables": { "type": ["integer", "null"] }
                }
            })
        })
    }

    fn capability_requirements(&self) -> CapabilitySet {
        // Empty: every agent may discover the operation (the LSP/MCP-bridge
        // precedent). Read vs deny enforcement is the policy engine's job.
        CapabilitySet::default()
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        _policy: &dyn PolicyEngine,
        session: &SessionContext,
        cancel: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        if cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        let parsed: EnvironmentDescribeInput =
            serde_json::from_value(input).map_err(|error| ToolError::ExecutionFailed {
                message: format!("invalid environment.describe input: {error}"),
            })?;

        let snapshot = snapshot_state(&self.state);
        let live_fingerprint = fingerprint_of_facts(&snapshot.facts);
        let stale = snapshot.manifest.is_none()
            || snapshot.manifest_fingerprint.as_deref() != Some(live_fingerprint.as_str());

        let manifest = if stale {
            let (run_id, session_id, previous_revision) = snapshot.manifest.as_ref().map_or(
                (concerto_core::ids::Ulid::new().to_string(), session.session_id.to_string(), None),
                |cached| (cached.run_id.clone(), cached.session_id.clone(), Some(cached.revision)),
            );
            if cancel.is_cancelled() {
                return Err(ToolError::Cancelled);
            }
            let collected = collect_from_facts(
                &snapshot.facts,
                run_id,
                session_id,
                previous_revision,
                snapshot.prober.as_ref(),
                &cancel,
            )
            .await;
            // Best-effort whiteboard persistence (the barrier's fail-soft
            // contract): a store failure degrades to a warning, never a
            // failed describe — the in-memory manifest is still served.
            if let Some(pool) = snapshot.pool.as_ref() {
                let session_id = collected.session_id.clone();
                if let Err(error) = persist_manifest(pool, &session_id, &collected).await {
                    tracing::warn!(
                        %error,
                        "failed to persist lazily collected EnvironmentObserved event; serving the in-memory manifest"
                    );
                }
            }
            let fingerprint = fingerprint_of_manifest(&collected);
            store_manifest(&self.state, &collected, fingerprint);
            collected
        } else if let Some(cached) = snapshot.manifest {
            cached
        } else {
            // Unreachable: `stale` is true whenever the cache is empty, so
            // this arm only runs on a lock/representation race. Fail closed
            // rather than serve a synthesized manifest.
            return Err(ToolError::ExecutionFailed {
                message: "environment.describe cache unavailable".to_string(),
            });
        };

        // H10: the run's sandbox capability rides every describe rendering —
        // computed pure from the registration-time facts (zero per-call
        // probes), descriptive only, offering no restore coverage.
        let sandbox_capability = describe_sandbox_capability(&snapshot.facts);
        Ok(match parsed.detail {
            DescribeDetail::Summary => render_summary(
                &manifest,
                &sandbox_capability,
                &snapshot.facts.declared_capabilities,
            ),
            DescribeDetail::Full => render_full(
                &manifest,
                parsed.max_tools,
                parsed.max_executables,
                &sandbox_capability,
                &snapshot.facts.declared_capabilities,
            ),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::environment_manifest::CountingProber;
    use async_trait::async_trait;
    use concerto_core::environment_manifest::{
        ManifestExecutionBackend, ENVIRONMENT_MANIFEST_SCHEMA_VERSION,
    };
    use concerto_core::executor::ToolExecutor;
    use concerto_core::ids::Ulid;
    use concerto_core::policy::SimplePolicyEngine;
    use concerto_core::traits::policy::{AuditEntry, PolicyEngine};
    use concerto_core::types::{Condition, PolicyRule};
    use std::sync::Mutex as StdMutex;

    /// No-op audit log for tests that do not inspect rows.
    struct TestAudit;

    #[async_trait]
    impl concerto_core::traits::policy::AuditLog for TestAudit {
        async fn record(
            &self,
            _entry: AuditEntry,
            _cancel: CancellationToken,
        ) -> Result<(), concerto_core::error::PolicyError> {
            Ok(())
        }
    }

    /// Recording audit log: proves describe travels the normal executor path
    /// (policy row + post-execution completion row, like every other tool).
    #[derive(Default)]
    struct RecordingAudit {
        entries: StdMutex<Vec<AuditEntry>>,
    }

    #[async_trait]
    impl concerto_core::traits::policy::AuditLog for RecordingAudit {
        async fn record(
            &self,
            entry: AuditEntry,
            _cancel: CancellationToken,
        ) -> Result<(), concerto_core::error::PolicyError> {
            self.entries.lock().unwrap_or_else(|error| error.into_inner()).push(entry);
            Ok(())
        }
    }

    /// Minimal stub tool to fill registries/inventories in tests.
    struct StubTool {
        tool_name: String,
    }

    #[async_trait]
    impl Tool for StubTool {
        fn name(&self) -> &str {
            &self.tool_name
        }
        fn description(&self) -> &str {
            "test stub"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        fn capability_requirements(&self) -> CapabilitySet {
            CapabilitySet::default()
        }
        async fn execute(
            &self,
            _input: serde_json::Value,
            _policy: &dyn PolicyEngine,
            _session: &SessionContext,
            _cancel: CancellationToken,
        ) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput { summary: "stub".to_string(), data: serde_json::json!({}) })
        }
    }

    fn test_session(project_dir: PathBuf) -> SessionContext {
        SessionContext::new(Ulid::new(), project_dir)
    }

    fn hermetic_facts(project_root: &Path, tool_names: Vec<String>, dir: &Path) -> DescribeFacts {
        DescribeFacts {
            project_root: project_root.to_path_buf(),
            backend: ManifestExecutionBackend::InProcess,
            sandbox_profile: None,
            container_routing: CommandRouting::Direct,
            policy_scope: PolicyScopeDescription::for_run(None, 9, None),
            persistence: vec![PersistenceClass::WorkspaceFiles, PersistenceClass::SessionDatabase],
            memory_enabled: false,
            tool_names,
            // Hermetic probes: two resolvable fakes + one missing name, so
            // the probe count is deterministic regardless of host PATH.
            probe_candidates: Some(vec![
                "probe-aaa".to_string(),
                "probe-bbb".to_string(),
                "probe-missing-xyz".to_string(),
            ]),
            probe_path_dirs: Some(vec![dir.to_path_buf()]),
            // Hermetic runtime: tests control the probe result, so sandbox
            // reporting never depends on the host's docker/podman.
            sandbox_runtime: RuntimeAvailability::Unavailable {
                reason: "test: no container runtime".to_string(),
            },
            declared_capabilities: vec!["test-declared".to_string()],
        }
    }

    /// A hermetic fake-executable dir: two runnable scripts answering
    /// `--version` via the real PATH lookup would be host-dependent, so the
    /// counting prober below answers for every resolved path instead — the
    /// dir only needs to exist with two executable files for resolution.
    fn fake_exe_dir() -> tempfile::TempDir {
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
        dir
    }

    fn test_state(
        facts: DescribeFacts,
        prober: CountingProber,
    ) -> (SharedDescribeState, Arc<CountingProber>) {
        let prober = Arc::new(prober);
        let state = new_shared_state(facts, prober.clone(), None);
        (state, prober)
    }

    fn allow_all_executor(registry: ToolRegistry, audit: Arc<RecordingAudit>) -> ToolExecutor {
        let policy = Arc::new(SimplePolicyEngine::new(
            vec![PolicyRule::AutoApprove(Condition::Always)],
            audit,
        ));
        ToolExecutor::new(Arc::new(registry), policy)
    }

    /// File-backed pool with all sessions migrations applied (the
    /// slice-6 barrier test pattern) for persistence assertions.
    async fn test_pool() -> (tempfile::TempDir, sqlx::SqlitePool) {
        let dir = tempfile::tempdir().expect("tempdir created");
        let path = dir.path().join("describe_test.db");
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
        sqlx::migrate!("../sessions/migrations").run(&pool).await.expect("migrations apply");
        (dir, pool)
    }

    #[tokio::test]
    async fn describe_returns_the_current_manifest_through_the_executor_with_audit_rows() {
        let exe_dir = fake_exe_dir();
        let root = tempfile::tempdir().expect("tempdir created");
        let facts = hermetic_facts(
            root.path(),
            vec!["alpha".to_string(), ENVIRONMENT_DESCRIBE_TOOL_NAME.to_string()],
            exe_dir.path(),
        );
        let (state, _prober) = test_state(facts, CountingProber::default());
        let mut registry = ToolRegistry::default();
        registry.register(Box::new(EnvironmentDescribeTool::with_state(state)));
        let audit = Arc::new(RecordingAudit::default());
        let executor = allow_all_executor(registry, audit.clone());

        let output = executor
            .execute(
                ENVIRONMENT_DESCRIBE_TOOL_NAME,
                serde_json::json!({}),
                &test_session(root.path().to_path_buf()),
                CancellationToken::new(),
            )
            .await
            .expect("describe executes through the normal path");

        assert_eq!(output.data["revision"], 0, "first collection starts at revision 0");
        assert_eq!(output.data["schema_version"], ENVIRONMENT_MANIFEST_SCHEMA_VERSION);
        assert_eq!(output.data["tool_count"], 2);
        assert!(
            output.summary.contains("revision=0"),
            "summary digest names the revision: {}",
            output.summary
        );

        // The normal executor path writes both rows: the policy verdict row
        // and the post-execution completion row — identical to every other
        // tool, with the registered name for attribution.
        let entries = audit.entries.lock().unwrap_or_else(|error| error.into_inner());
        assert_eq!(entries.len(), 2, "policy row + completion row: {entries:?}");
        assert_eq!(entries[0].verdict, "Allow");
        assert_eq!(entries[0].tool_name, ENVIRONMENT_DESCRIBE_TOOL_NAME);
        assert_eq!(entries[1].verdict, "ExecutionSucceeded");
        assert_eq!(entries[1].tool_name, ENVIRONMENT_DESCRIBE_TOOL_NAME);
        assert_eq!(entries[0].correlation_id, entries[1].correlation_id);
    }

    #[tokio::test]
    async fn describe_reports_the_sandbox_capability() {
        // H10: every describe rendering carries the run's sandbox capability
        // descriptor — hermetic here (the facts' canned probe), so no host
        // runtime can change the verdicts.
        let exe_dir = fake_exe_dir();
        let root = tempfile::tempdir().expect("tempdir created");
        let facts = hermetic_facts(
            root.path(),
            vec![ENVIRONMENT_DESCRIBE_TOOL_NAME.to_string()],
            exe_dir.path(),
        );
        let (state, _prober) = test_state(facts, CountingProber::default());
        let tool = EnvironmentDescribeTool::with_state(state);
        let session = test_session(root.path().to_path_buf());
        let policy = SimplePolicyEngine::new(vec![], Arc::new(TestAudit));

        let summary = tool
            .execute(serde_json::json!({}), &policy, &session, CancellationToken::new())
            .await
            .expect("summary executes");
        let capability = &summary.data["sandbox_capability"];
        assert_eq!(capability["schema_version"], 1);
        assert_eq!(capability["profile"], "None");
        assert_eq!(capability["isolation"], "unconfined");
        // (Externally tagged: {"unavailable": {"reason": …}}.)
        assert!(
            !capability["linux"]["unavailable"]["reason"].as_str().expect("reason").is_empty(),
            "the Unavailable reason is the report: {capability}"
        );
        assert!(
            capability["linux"]["unavailable"]["reason"]
                .as_str()
                .expect("reason")
                .contains("no sandbox isolation"),
            "unconfined reports Unavailable with a reason: {capability}"
        );
        assert_eq!(capability["windows"]["unsupported_by_decision"]["decision"], "ADR-72 §5");
        assert!(
            !capability["windows"]["unsupported_by_decision"]["reason"]
                .as_str()
                .expect("reason")
                .is_empty(),
            "the decision carries its reason: {capability}"
        );
        let unrestorable: Vec<&str> = capability["cannot_restore"]
            .as_array()
            .expect("cannot_restore array")
            .iter()
            .map(|entry| entry["kind"].as_str().expect("kind string"))
            .collect();
        for expected in ["mounted_host_data", "remote_services", "outside_container_writable_layer"]
        {
            assert!(unrestorable.contains(&expected), "named unrestorable: {unrestorable:?}");
        }
        // The reported coverage refuses shell/network through H09's
        // preflight (adaptation, not a preflight fork).
        let coverage: Vec<concerto_core::checkpoint::EffectCoverage> =
            serde_json::from_value(capability["coverage"].clone()).expect("coverage decodes");
        let report = concerto_core::checkpoint::preflight_rollback_guarantee(
            &[
                concerto_core::checkpoint::EffectKind::File,
                concerto_core::checkpoint::EffectKind::Shell,
                concerto_core::checkpoint::EffectKind::Network,
            ],
            &coverage,
        );
        assert_eq!(report.verdict, concerto_core::checkpoint::PreflightVerdict::Refused);

        // Full detail carries the same descriptor.
        let full = tool
            .execute(
                serde_json::json!({"detail": "full"}),
                &policy,
                &session,
                CancellationToken::new(),
            )
            .await
            .expect("full executes");
        assert_eq!(full.data["sandbox_capability"]["schema_version"], 1);
        assert_eq!(
            full.data["sandbox_capability"]["windows"]["unsupported_by_decision"]["decision"],
            "ADR-72 §5"
        );
    }

    #[tokio::test]
    async fn describe_honors_a_found_runtime_without_probing_windows() {
        // Failure injection the other way: a FOUND runtime is Available on
        // Linux/macOS yet Windows stays decided — the decision ignores the
        // probe (a probe would be a lie).
        use concerto_core::sandbox::ContainerRuntime;
        let exe_dir = fake_exe_dir();
        let root = tempfile::tempdir().expect("tempdir created");
        let mut facts = hermetic_facts(
            root.path(),
            vec![ENVIRONMENT_DESCRIBE_TOOL_NAME.to_string()],
            exe_dir.path(),
        );
        facts.sandbox_profile = Some(SandboxProfile::Containerized);
        facts.sandbox_runtime = RuntimeAvailability::Available(ContainerRuntime::Docker);
        let (state, _prober) = test_state(facts, CountingProber::default());
        let tool = EnvironmentDescribeTool::with_state(state);
        let session = test_session(root.path().to_path_buf());
        let policy = SimplePolicyEngine::new(vec![], Arc::new(TestAudit));
        let output = tool
            .execute(serde_json::json!({}), &policy, &session, CancellationToken::new())
            .await
            .expect("describe executes");
        let capability = &output.data["sandbox_capability"];
        assert_eq!(capability["profile"], "Containerized");
        assert!(
            capability["linux"]["available"]["detail"].as_str().expect("detail").contains("docker"),
            "found runtime is honored where probes are meaningful: {capability}"
        );
        assert_eq!(capability["windows"]["unsupported_by_decision"]["decision"], "ADR-72 §5");
        assert!(
            capability["windows"]["unsupported_by_decision"]["reason"]
                .as_str()
                .expect("reason")
                .contains("not consulted"),
            "Windows ignores the found runtime: {capability}"
        );
    }

    #[tokio::test]
    async fn probe_discipline_holds_across_many_describe_calls() {
        let exe_dir = fake_exe_dir();
        let root = tempfile::tempdir().expect("tempdir created");
        let facts = hermetic_facts(
            root.path(),
            vec![ENVIRONMENT_DESCRIBE_TOOL_NAME.to_string()],
            exe_dir.path(),
        );
        let (state, prober) = test_state(facts, CountingProber::default());
        let tool = EnvironmentDescribeTool::with_state(state);
        let session = test_session(root.path().to_path_buf());

        // Mixed summary/full calls: only the first collects.
        for input in [
            serde_json::json!({}),
            serde_json::json!({"detail": "full"}),
            serde_json::json!({"detail": "summary"}),
            serde_json::json!({"detail": "full", "max_tools": 5}),
            serde_json::json!({}),
        ] {
            let output = tool
                .execute(
                    input,
                    &SimplePolicyEngine::new(vec![], Arc::new(TestAudit)),
                    &session,
                    CancellationToken::new(),
                )
                .await
                .expect("describe executes");
            assert_eq!(output.data["revision"], 0);
        }
        assert_eq!(
            prober.calls(),
            2,
            "exactly one probe set (two resolved fakes) across five describe calls"
        );
    }

    #[tokio::test]
    async fn bounded_output_cap_is_enforced_with_truncation_flags() {
        let exe_dir = fake_exe_dir();
        let root = tempfile::tempdir().expect("tempdir created");
        let tool_names: Vec<String> = (0..600).map(|index| format!("tool-{index:03}")).collect();
        let facts = hermetic_facts(root.path(), tool_names, exe_dir.path());
        let (state, _prober) = test_state(facts, CountingProber::default());
        let tool = EnvironmentDescribeTool::with_state(state);
        let session = test_session(root.path().to_path_buf());
        let policy = SimplePolicyEngine::new(vec![], Arc::new(TestAudit));

        // Bounded default: 32 of 600, flagged.
        let bounded = tool
            .execute(
                serde_json::json!({"detail": "full"}),
                &policy,
                &session,
                CancellationToken::new(),
            )
            .await
            .expect("full detail executes");
        let tools = bounded.data["tools"].as_array().expect("tools array");
        assert_eq!(tools.len(), ENVIRONMENT_DESCRIBE_DEFAULT_MAX_TOOLS);
        assert_eq!(bounded.data["truncated_tools"], true);
        assert_eq!(bounded.data["total_tools"], 600);

        // The summary digest stays bounded too: folded names stay out.
        let summary = tool
            .execute(serde_json::json!({}), &policy, &session, CancellationToken::new())
            .await
            .expect("summary executes");
        let digest = summary.data["summary"].as_str().expect("digest string");
        assert!(digest.contains("+588 more"), "overflow folds into a count: {digest}");
        assert!(!digest.contains("tool-599"), "folded names stay out of the digest");

        // An over-ceiling request clamps to the 500-entry ceiling (the
        // filesystem-list precedent), never an unbounded dump.
        let clamped = tool
            .execute(
                serde_json::json!({"detail": "full", "max_tools": 10_000}),
                &policy,
                &session,
                CancellationToken::new(),
            )
            .await
            .expect("clamped full executes");
        assert_eq!(
            clamped.data["tools"].as_array().expect("tools array").len(),
            ENVIRONMENT_DESCRIBE_MAX_TOOLS_CEILING
        );
        assert_eq!(clamped.data["total_tools"], 600);
    }

    #[tokio::test]
    async fn config_change_invalidates_the_prior_revision_with_one_refresh_probe_set() {
        let exe_dir = fake_exe_dir();
        let root = tempfile::tempdir().expect("tempdir created");
        // Cached manifest collected under the OLD config: different tool
        // membership and a different policy-ceiling label (as a resume across
        // a config change would restore).
        let mut stale_facts = hermetic_facts(
            root.path(),
            vec!["old-tool".to_string(), ENVIRONMENT_DESCRIBE_TOOL_NAME.to_string()],
            exe_dir.path(),
        );
        stale_facts.policy_scope = PolicyScopeDescription::for_run(Some(3), 9, None);
        let (state, prober) = test_state(
            hermetic_facts(
                root.path(),
                vec!["new-tool".to_string(), ENVIRONMENT_DESCRIBE_TOOL_NAME.to_string()],
                exe_dir.path(),
            ),
            CountingProber::default(),
        );
        let stale = collect_from_facts(
            &stale_facts,
            "run-keep".to_string(),
            "sess-keep".to_string(),
            Some(4),
            &CountingProber::default(),
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(stale.revision, 5);
        populate_from_barrier(&state, &stale);
        assert_eq!(prober.calls(), 0, "populate performs zero probes");

        // Same-config describe would serve the cache; the changed config
        // fingerprints stale, so this call refreshes exactly once.
        let tool = EnvironmentDescribeTool::with_state(state.clone());
        let session = test_session(root.path().to_path_buf());
        let policy = SimplePolicyEngine::new(vec![], Arc::new(TestAudit));
        let refreshed = tool
            .execute(serde_json::json!({}), &policy, &session, CancellationToken::new())
            .await
            .expect("stale describe refreshes");
        assert_eq!(
            refreshed.data["revision"], 6,
            "explicit re-collection bumps the prior revision"
        );
        assert_eq!(prober.calls(), 2, "exactly one refresh probe set");
        assert_eq!(refreshed.data["run_id"], "run-keep", "refresh keeps the run identity");

        // The refreshed revision is current now: further calls probe nothing.
        for _ in 0..3 {
            let again = tool
                .execute(
                    serde_json::json!({"detail": "full"}),
                    &policy,
                    &session,
                    CancellationToken::new(),
                )
                .await
                .expect("current describe serves the cache");
            assert_eq!(again.data["revision"], 6);
        }
        assert_eq!(prober.calls(), 2, "no per-call probes after the refresh");
    }

    #[tokio::test]
    async fn same_config_resume_restore_serves_without_reprobing() {
        let exe_dir = fake_exe_dir();
        let root = tempfile::tempdir().expect("tempdir created");
        let facts = hermetic_facts(
            root.path(),
            vec![ENVIRONMENT_DESCRIBE_TOOL_NAME.to_string()],
            exe_dir.path(),
        );
        let (state, prober) = test_state(facts.clone(), CountingProber::default());
        // A resume restore under an UNCHANGED config: the restored manifest
        // fingerprints identical to the live facts.
        let restored = collect_from_facts(
            &facts,
            "run-resume".to_string(),
            "sess-resume".to_string(),
            None,
            &CountingProber::default(),
            &CancellationToken::new(),
        )
        .await;
        populate_from_barrier(&state, &restored);

        let tool = EnvironmentDescribeTool::with_state(state);
        let session = test_session(root.path().to_path_buf());
        let policy = SimplePolicyEngine::new(vec![], Arc::new(TestAudit));
        let output = tool
            .execute(serde_json::json!({}), &policy, &session, CancellationToken::new())
            .await
            .expect("describe serves the restored manifest");
        assert_eq!(output.data["revision"], 0);
        assert_eq!(output.data["run_id"], "run-resume");
        assert_eq!(prober.calls(), 0, "no probes: the restored revision is current");
    }

    #[tokio::test]
    async fn default_posture_applies_with_no_bypass_and_no_probes_on_deny() {
        let exe_dir = fake_exe_dir();
        let root = tempfile::tempdir().expect("tempdir created");
        let facts = hermetic_facts(
            root.path(),
            vec![ENVIRONMENT_DESCRIBE_TOOL_NAME.to_string()],
            exe_dir.path(),
        );
        let (state, prober) = test_state(facts, CountingProber::default());
        let mut registry = ToolRegistry::default();
        registry.register(Box::new(EnvironmentDescribeTool::with_state(state)));
        // Default presets: an unmatched tool name falls to the catch-all
        // `RequireApproval(Always)` — with no approval sink the call is
        // denied WITHOUT executing, exactly like any other new tool.
        let audit = Arc::new(RecordingAudit::default());
        let policy = Arc::new(SimplePolicyEngine::new(
            concerto_core::policy_presets::PolicyPresets::default_rules(),
            audit.clone(),
        ));
        let executor = ToolExecutor::new(Arc::new(registry), policy);

        let error = executor
            .execute(
                ENVIRONMENT_DESCRIBE_TOOL_NAME,
                serde_json::json!({}),
                &test_session(root.path().to_path_buf()),
                CancellationToken::new(),
            )
            .await
            .expect_err("default posture denies without a sink");
        assert!(
            matches!(error, ToolError::PolicyDenied { .. }),
            "normal gate verdict, never a bypass: {error:?}"
        );
        assert_eq!(prober.calls(), 0, "a denied call never reaches collection");
        // The denial is still audited through the normal channel.
        let entries = audit.entries.lock().unwrap_or_else(|error| error.into_inner());
        assert!(!entries.is_empty(), "the gate verdict leaves an audit row");
    }

    #[tokio::test]
    async fn lazily_collected_manifest_persists_an_environment_observed_row() {
        let (_pool_dir, pool) = test_pool().await;
        let exe_dir = fake_exe_dir();
        let root = tempfile::tempdir().expect("tempdir created");
        let facts = hermetic_facts(
            root.path(),
            vec![ENVIRONMENT_DESCRIBE_TOOL_NAME.to_string()],
            exe_dir.path(),
        );
        let prober = Arc::new(CountingProber::default());
        let state = new_shared_state(facts, prober, Some(pool.clone()));
        let tool = EnvironmentDescribeTool::with_state(state);
        let session = test_session(root.path().to_path_buf());
        let policy = SimplePolicyEngine::new(vec![], Arc::new(TestAudit));
        let output = tool
            .execute(serde_json::json!({}), &policy, &session, CancellationToken::new())
            .await
            .expect("lazy describe collects");
        let session_id = output.data["session_id"].as_str().expect("session id").to_string();

        let restored =
            crate::environment_manifest::restore_environment_manifest(&pool, &session_id)
                .await
                .expect("lazy collection persists the whiteboard row");
        assert_eq!(restored.revision, 0);
        assert_eq!(restored.session_id, session_id);
    }

    #[test]
    fn empty_input_defaults_to_summary_and_round_trips() {
        // R6: every serialized field additive + defaulted — `{}` is summary.
        let parsed: EnvironmentDescribeInput =
            serde_json::from_value(serde_json::json!({})).expect("empty input parses");
        assert_eq!(parsed.detail, DescribeDetail::Summary);
        assert_eq!(parsed.max_tools, None);
        // Sparse legacy payloads still decode (forward-compat probe).
        let sparse = serde_json::json!({"detail": "full"});
        let full: EnvironmentDescribeInput =
            serde_json::from_value(sparse).expect("sparse input parses");
        assert_eq!(full.detail, DescribeDetail::Full);
        // Unknown fields are ignored, never fatal (additive wire form).
        let future = serde_json::json!({"detail": "summary", "whatever_next": 1});
        let _tolerant: EnvironmentDescribeInput =
            serde_json::from_value(future).expect("unknown fields ignored");
    }

    #[test]
    fn fingerprints_agree_within_a_config_and_diverge_across_changes() {
        let exe_dir = fake_exe_dir();
        let root = tempfile::tempdir().expect("tempdir created");
        let facts = hermetic_facts(
            root.path(),
            vec!["a".to_string(), ENVIRONMENT_DESCRIBE_TOOL_NAME.to_string()],
            exe_dir.path(),
        );
        let same = facts.clone();
        assert_eq!(fingerprint_of_facts(&facts), fingerprint_of_facts(&same));

        // Tool membership change diverges.
        let mut added_tool = facts.clone();
        added_tool.tool_names.push("mcp:server:thing".to_string());
        assert_ne!(fingerprint_of_facts(&facts), fingerprint_of_facts(&added_tool));

        // Policy-ceiling label change diverges.
        let mut changed_policy = facts.clone();
        changed_policy.policy_scope = PolicyScopeDescription::for_run(Some(2), 9, None);
        assert_ne!(fingerprint_of_facts(&facts), fingerprint_of_facts(&changed_policy));

        // Memory toggle changes persistence labels, so it diverges.
        let mut memory_on = facts.clone();
        memory_on.memory_enabled = true;
        assert_ne!(fingerprint_of_facts(&facts), fingerprint_of_facts(&memory_on));
    }

    #[test]
    fn registration_helper_registers_a_discoverable_tool_covering_every_tool() {
        let root = tempfile::tempdir().expect("tempdir created");
        let mut registry = ToolRegistry::default();
        registry.register(Box::new(StubTool { tool_name: "alpha".to_string() }));
        registry.register(Box::new(StubTool { tool_name: "beta".to_string() }));
        let config = concerto_config::AppConfig::default();
        let state = register_describe_tool(&mut registry, root.path(), &config, true, false, None);

        // Discoverable through the standard definition surface both paths
        // consume (single registry → single-agent loop and coordinator).
        let names: Vec<String> = registry
            .all_tool_definitions()
            .iter()
            .map(|definition| definition.name.clone())
            .collect();
        assert!(names.contains(&ENVIRONMENT_DESCRIBE_TOOL_NAME.to_string()), "{names:?}");
        assert!(
            registry.get(ENVIRONMENT_DESCRIBE_TOOL_NAME).is_some(),
            "the operation resolves through the normal registry lookup"
        );
        // Empty capability requirements: offered to every agent; the policy
        // gate (not the capability filter) decides each call.
        let tool = registry.get(ENVIRONMENT_DESCRIBE_TOOL_NAME).expect("registered");
        assert!(tool.capability_requirements().is_empty());

        let guarded = state.lock().unwrap_or_else(|error| error.into_inner());
        assert!(guarded.manifest.is_none(), "registration never probes");
        assert_eq!(
            guarded.facts.tool_names,
            vec![
                "alpha".to_string(),
                "beta".to_string(),
                ENVIRONMENT_DESCRIBE_TOOL_NAME.to_string()
            ]
        );
        assert!(
            guarded.facts.persistence.contains(&PersistenceClass::SessionDatabase),
            "session-DB presence is recorded when a pool exists"
        );
    }

    #[tokio::test]
    async fn invalid_detail_is_rejected_without_panicking() {
        let exe_dir = fake_exe_dir();
        let root = tempfile::tempdir().expect("tempdir created");
        let facts = hermetic_facts(
            root.path(),
            vec![ENVIRONMENT_DESCRIBE_TOOL_NAME.to_string()],
            exe_dir.path(),
        );
        let (state, prober) = test_state(facts, CountingProber::default());
        let tool = EnvironmentDescribeTool::with_state(state);
        let session = test_session(root.path().to_path_buf());
        let policy = SimplePolicyEngine::new(vec![], Arc::new(TestAudit));
        let error = tool
            .execute(
                serde_json::json!({"detail": "everything"}),
                &policy,
                &session,
                CancellationToken::new(),
            )
            .await
            .expect_err("unknown detail is rejected");
        assert!(
            matches!(error, ToolError::ExecutionFailed { .. }),
            "a corrective refusal, not a panic: {error:?}"
        );
        assert_eq!(prober.calls(), 0, "rejected input never collects");
    }

    /// The operation's policy identity is its own registered name (no alias
    /// view): the catch-all posture that governs unknown tools governs this
    /// one — structurally incapable of inheriting another tool's allow rule.
    #[test]
    fn policy_view_is_the_registered_name_with_caller_input() {
        let exe_dir = fake_exe_dir();
        let root = tempfile::tempdir().expect("tempdir created");
        let facts = hermetic_facts(
            root.path(),
            vec![ENVIRONMENT_DESCRIBE_TOOL_NAME.to_string()],
            exe_dir.path(),
        );
        let (state, _prober) = test_state(facts, CountingProber::default());
        let tool = EnvironmentDescribeTool::with_state(state);
        let input = serde_json::json!({"detail": "summary"});
        let (name, viewed) = tool.policy_view(&input);
        assert_eq!(name, ENVIRONMENT_DESCRIBE_TOOL_NAME);
        assert_eq!(viewed, input);
    }

    /// Audit-shape probe: with an allow-all policy the completion row for a
    /// describe call carries no command/path facts (the operation names no
    /// executable and no path) — the row stays minimal and truthful.
    #[tokio::test]
    async fn completion_row_carries_no_command_or_path_facts() {
        let exe_dir = fake_exe_dir();
        let root = tempfile::tempdir().expect("tempdir created");
        let facts = hermetic_facts(
            root.path(),
            vec![ENVIRONMENT_DESCRIBE_TOOL_NAME.to_string()],
            exe_dir.path(),
        );
        let (state, _prober) = test_state(facts, CountingProber::default());
        let mut registry = ToolRegistry::default();
        registry.register(Box::new(EnvironmentDescribeTool::with_state(state)));
        let audit = Arc::new(RecordingAudit::default());
        let executor = allow_all_executor(registry, audit.clone());
        executor
            .execute(
                ENVIRONMENT_DESCRIBE_TOOL_NAME,
                serde_json::json!({}),
                &test_session(root.path().to_path_buf()),
                CancellationToken::new(),
            )
            .await
            .expect("describe executes");

        let entries = audit.entries.lock().unwrap_or_else(|error| error.into_inner());
        let completion = entries
            .iter()
            .find(|entry| entry.verdict == "ExecutionSucceeded")
            .expect("completion row exists");
        assert_eq!(completion.resolved_executable, None);
        assert_eq!(completion.path_facts, None);
        assert_eq!(completion.canonical_tool.as_deref(), Some(ENVIRONMENT_DESCRIBE_TOOL_NAME));
    }

    /// Cancelled calls fail closed before any collection work.
    #[tokio::test]
    async fn cancelled_describe_never_collects() {
        let exe_dir = fake_exe_dir();
        let root = tempfile::tempdir().expect("tempdir created");
        let facts = hermetic_facts(
            root.path(),
            vec![ENVIRONMENT_DESCRIBE_TOOL_NAME.to_string()],
            exe_dir.path(),
        );
        let (state, prober) = test_state(facts, CountingProber::default());
        let tool = EnvironmentDescribeTool::with_state(state);
        let cancel = CancellationToken::new();
        cancel.cancel();
        let error = tool
            .execute(
                serde_json::json!({}),
                &SimplePolicyEngine::new(vec![], Arc::new(TestAudit)),
                &test_session(root.path().to_path_buf()),
                cancel,
            )
            .await
            .expect_err("cancelled describe fails closed");
        assert!(matches!(error, ToolError::Cancelled));
        assert_eq!(prober.calls(), 0);
    }
}
