//! ADR-82 §2 — `EnvironmentManifest`: a descriptive, cached per-run environment
//! declaration answering "what can this run do".
//!
//! **Load-bearing invariant: the manifest is descriptive and is never an
//! authorization token.** No code path branches authorization on it; the
//! policy engine never reads it; and there is no conversion from the manifest
//! into a policy type. Structurally:
//!
//! - [`EnvironmentManifest`] lives in this vocabulary module, which imports
//!   only core-and-below types (`crate::checkpoint` for the byte budget,
//!   `crate::types` for [`SandboxProfile`][crate::types::SandboxProfile] and
//!   [`CommandRouting`][crate::types::CommandRouting], plus `serde`/`std`).
//!   It never names `crate::policy`, `crate::authorization`,
//!   `crate::executor`, or `crate::traits::policy`.
//! - No `From`/`Into` impl connects this module to any policy type, in either
//!   direction. `PolicyAction` construction sites take no manifest parameter.
//! - The manifest carries no credentials: effective root/scope are declared
//!   as plain paths, labels, hashes, and counts — no keys, tokens, or secret
//!   material. The JSON field surface is pinned by test
//!   (`manifest_json_has_exactly_the_documented_fields`).
//!
//! Wire form: additive fields, unknown fields ignored on read; unknown
//! `schema_version` values are rejected by [`EnvironmentManifest::from_json`]
//! (the [`ToolFailure::from_json`][crate::tool_failure::ToolFailure::from_json]
//! precedent), never silently accepted. Every field except `schema_version`
//! is `#[serde(default)]`-tolerant so a payload missing newly added fields
//! still decodes; `schema_version` itself defaults to the current version when
//! absent (the `VerificationSummary` legacy precedent), and any *present but
//! unsupported* version is rejected.

use serde::{Deserialize, Serialize};

use crate::checkpoint::CHECKPOINT_TOTAL_LIMIT_BYTES;
use crate::types::{CommandRouting, SandboxProfile};

/// Current [`EnvironmentManifest`] envelope schema version (ADR-82 §2).
pub const ENVIRONMENT_MANIFEST_SCHEMA_VERSION: u16 = 1;

fn default_manifest_schema_version() -> u16 {
    ENVIRONMENT_MANIFEST_SCHEMA_VERSION
}

/// How many tool names the [`EnvironmentManifest::summary`] digest renders
/// before folding the remainder into a `+N more` count, so prompt injection
/// stays bounded no matter how large the registry grows.
pub const ENVIRONMENT_MANIFEST_DIGEST_MAX_TOOLS: usize = 12;

/// Descriptive execution-backend label for a run (ADR-82 §2).
///
/// This is a *vocabulary* label recorded after the fact — it does not select,
/// configure, or authorize any backend. The two variants name the two
/// executor call-site realities: the loop runs in-process against the local
/// executor, or supervised behind the gate-proxy boundary (ADR-60 S5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ManifestExecutionBackend {
    /// The run executes tools in-process against the local [`ToolExecutor`]
    /// (directly or through the in-process gate backend).
    ///
    /// [`ToolExecutor`]: crate::executor::ToolExecutor
    InProcess,
    /// The run executes tools supervised: agent-side calls cross the
    /// gate-proxy boundary and the supervisor resolves authority.
    Supervised,
}

impl ManifestExecutionBackend {
    /// The stable wire/display label for this backend.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InProcess => "in-process",
            Self::Supervised => "supervised",
        }
    }
}

impl std::fmt::Display for ManifestExecutionBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Which effect classes survive the run (ADR-82 §2 "persistence classes").
///
/// Descriptive only: naming a class records that the harness *may* leave such
/// effects behind; it grants nothing and is never consulted by any gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum PersistenceClass {
    /// Agent filesystem writes land on disk at execute time.
    WorkspaceFiles,
    /// Whiteboard/audit/checkpoint rows land in the session database.
    SessionDatabase,
    /// Memory facts land in the memory index (recorded only when the
    /// collector was told memory is attached for the run).
    MemoryIndex,
}

impl PersistenceClass {
    /// The stable wire/display label for this class.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::WorkspaceFiles => "workspace-files",
            Self::SessionDatabase => "session-db",
            Self::MemoryIndex => "memory-index",
        }
    }
}

/// One tool the run may invoke, under both identities (ADR-82 slice 1).
///
/// `registered_name` is the name the caller invokes and the audit row
/// attributes; `canonical_policy_name` is the policy-view identity
/// ([`Tool::policy_view`][crate::traits::tool::Tool::policy_view]'s first
/// element) accounting keys on. The two diverge for alias tools (a `write`
/// alias records `filesystem` here); consumers must never re-derive the
/// canonical name from the registered one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestToolEntry {
    /// The name the caller invokes (registry key).
    #[serde(default)]
    pub registered_name: String,
    /// The canonical policy-view tool name policy evaluates.
    #[serde(default)]
    pub canonical_policy_name: String,
}

/// One lazily probed executable observation (ADR-82 §2).
///
/// Probed once per run by the collector — never per prompt — and stamped with
/// `observed_at` so a consumer can tell point-in-time fact from live truth
/// (accepted limitation 6: a tool installed mid-run is invisible until the
/// next explicit re-probe).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutableObservation {
    /// The probed program name (e.g. `"cargo"`).
    #[serde(default)]
    pub name: String,
    /// The `PATH`-resolved location, when the program was found.
    #[serde(default)]
    pub resolved_path: Option<String>,
    /// The first line of `<exe> --version`, when the probe answered in time.
    #[serde(default)]
    pub version: Option<String>,
    /// Unix epoch milliseconds (UTC) when this observation was taken.
    #[serde(default)]
    pub observed_at_ms: i64,
}

/// Descriptive policy scope: ceiling, write preset, and sandbox (ADR-82 §2).
///
/// Every field is a human-readable *label* describing the run's configured
/// posture. These strings are never parsed, never matched by any rule, and
/// never fed back into evaluation — they exist so an operator reading the
/// manifest can see what the run was configured with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyScopeDescription {
    /// Which rule set ceilings the run (custom-configured vs concerto
    /// defaults), with its rule count.
    #[serde(default)]
    pub ceiling: String,
    /// The write-gating posture label.
    #[serde(default)]
    pub write_preset: String,
    /// The sandbox label (mirrors the run's sandbox profile, if any).
    #[serde(default)]
    pub sandbox: String,
}

impl PolicyScopeDescription {
    /// Honest fallback when the collector knows nothing about the run's
    /// policy posture: every facet says so explicitly rather than inventing
    /// a label.
    pub fn unspecified() -> Self {
        Self {
            ceiling: "unspecified".to_string(),
            write_preset: "unspecified".to_string(),
            sandbox: "unspecified".to_string(),
        }
    }

    /// Describe one run's posture from caller-supplied facts.
    ///
    /// `configured_rules` is `Some(n)` when the operator configured `n`
    /// policy rules, `None` when the run fell back to the concerto defaults
    /// (`default_rule_count` long). `sandbox` is the run-level sandbox
    /// profile, if any was configured.
    pub fn for_run(
        configured_rules: Option<usize>,
        default_rule_count: usize,
        sandbox: Option<SandboxProfile>,
    ) -> Self {
        let ceiling = match configured_rules {
            Some(count) => format!("custom: {count} configured rule(s)"),
            None => format!("default: {default_rule_count} concerto rule(s)"),
        };
        let sandbox = match sandbox {
            Some(profile) => profile.as_str().to_string(),
            None => "none".to_string(),
        };
        Self {
            ceiling,
            write_preset: "policy-gated (no blanket write pre-approval)".to_string(),
            sandbox,
        }
    }
}

/// Descriptive restore capabilities, derived from checkpoint-record coverage
/// (ADR-82 §§2, 4).
///
/// Every field states a harness ceiling honestly: what restore *can* do, and
/// — just as importantly — what it degrades (`Manual`) on. In particular
/// `untracked_file_restore` stays `false` until the git-stash `-u` gap
/// closes; the byte bound below is the sidecar's own total budget, never
/// reconstructed from a hash. Binary restore is claimed since the chunked
/// pre-image store (migration 039) banks arbitrary bytes within budget.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestoreCapabilities {
    /// Whether within-budget pre-images (binary included) restore from
    /// sidecar bytes.
    #[serde(default)]
    pub file_pre_image_bytes: bool,
    /// The sidecar byte budget per checkpoint
    /// ([`CHECKPOINT_TOTAL_LIMIT_BYTES`], not the evidence-path cache bound;
    /// banked as `CHECKPOINT_BLOB_LIMIT_BYTES`-capped chunks).
    #[serde(default)]
    pub max_pre_image_bytes: u64,
    /// Whether git-stash undo is available behind `RestoreMethod::GitStash`.
    #[serde(default)]
    pub git_stash_undo: bool,
    /// Whether untracked-file restore works (false: the stash omits `-u`).
    #[serde(default)]
    pub untracked_file_restore: bool,
    /// Whether binary restore works (true: the chunked store replays
    /// within-budget bytes byte-exact; over-budget targets stay hash-only —
    /// drift-detectable, never reconstructible).
    #[serde(default)]
    pub binary_restore: bool,
}

impl RestoreCapabilities {
    /// The current harness's restore ceilings, derived from the checkpoint
    /// vocabulary's own budget constant so the two can never disagree.
    pub fn current() -> Self {
        Self {
            file_pre_image_bytes: true,
            max_pre_image_bytes: CHECKPOINT_TOTAL_LIMIT_BYTES as u64,
            git_stash_undo: true,
            untracked_file_restore: false,
            binary_restore: true,
        }
    }
}

/// Collection-freshness stamp (ADR-82 §2).
///
/// The manifest is probed lazily and cached per run; this stamp tells a
/// consumer *when* the cached description was taken and which collection
/// produced it (`revision` bumps on every explicit re-collection).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FreshnessStamp {
    /// Unix epoch milliseconds (UTC) of the collection that produced this
    /// manifest revision.
    #[serde(default)]
    pub collected_at_ms: i64,
    /// Collection generation: `0` for the run's first collection, bumped by
    /// one on every explicit re-collection.
    #[serde(default)]
    pub revision: u64,
}

/// A descriptive, cached per-run environment declaration (ADR-82 §2).
///
/// Answers the operator's "what can this run do" question: run identity,
/// host, backend, sandbox profile + routing, project root + root hash, the
/// tool set under both identities, lazily probed executables, descriptive
/// policy scope, persistence classes, and restore capabilities — plus
/// collection metadata (`collected_at_ms`, `revision`, [`FreshnessStamp`]).
///
/// Wire form: additive fields, unknown fields ignored; unknown
/// `schema_version` values are rejected by [`Self::from_json`]. See the
/// module docs for the never-an-authorization-token invariant.
///
/// `Eq` is deliberately *not* derived: [`SandboxProfile`] is `PartialEq`
/// only, and the manifest must name the real profile type rather than a
/// lossy string copy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnvironmentManifest {
    /// Envelope schema version ([`ENVIRONMENT_MANIFEST_SCHEMA_VERSION`]).
    #[serde(default = "default_manifest_schema_version")]
    pub schema_version: u16,
    /// Run identity (stable across resume: a resumed run restores the stored
    /// manifest rather than minting a new identity).
    #[serde(default)]
    pub run_id: String,
    /// Owning session identity.
    #[serde(default)]
    pub session_id: String,
    /// Host OS (`std::env::consts::OS` at collection).
    #[serde(default)]
    pub os: String,
    /// Host architecture (`std::env::consts::ARCH` at collection).
    #[serde(default)]
    pub arch: String,
    /// Which execution backend the run dispatches through (descriptive
    /// label, not a selector).
    #[serde(default)]
    pub backend: Option<ManifestExecutionBackend>,
    /// The run-level sandbox profile, when one was configured.
    #[serde(default)]
    pub sandbox_profile: Option<SandboxProfile>,
    /// The run's command-routing assertion (defaults to `Direct`).
    #[serde(default)]
    pub container_routing: CommandRouting,
    /// The project root path, as supplied (forward slashes).
    #[serde(default)]
    pub project_root: String,
    /// The blake3 project-root identity scoping every `resource_facts` row
    /// (the `tool_facts::project_root_hash` grammar, computed by the
    /// collector — this type only carries the resulting string).
    #[serde(default)]
    pub project_root_hash: String,
    /// The run's tool set under both identities, sorted by registered name.
    #[serde(default)]
    pub tools: Vec<ManifestToolEntry>,
    /// Lazily probed executable observations, each with `observed_at_ms`.
    #[serde(default)]
    pub executables: Vec<ExecutableObservation>,
    /// Descriptive policy scope (labels only — never evaluated).
    #[serde(default)]
    pub policy_scope: Option<PolicyScopeDescription>,
    /// Which effect classes survive the run.
    #[serde(default)]
    pub persistence: Vec<PersistenceClass>,
    /// Descriptive restore capabilities (checkpoint coverage).
    #[serde(default)]
    pub restore: Option<RestoreCapabilities>,
    /// Unix epoch milliseconds (UTC) of this collection.
    #[serde(default)]
    pub collected_at_ms: i64,
    /// Collection generation (`0` first, bumped on explicit re-collection).
    #[serde(default)]
    pub revision: u64,
    /// Freshness stamp mirroring this collection.
    #[serde(default)]
    pub freshness: Option<FreshnessStamp>,
}

impl EnvironmentManifest {
    /// Deserialize with schema-version enforcement: the current version loads
    /// as-is (unknown *fields* ignored — additive wire form); unknown
    /// (future) versions are rejected, never silently accepted. A missing
    /// version defaults to current (legacy precedent).
    pub fn from_json(json: &str) -> Result<Self, String> {
        let manifest: Self = serde_json::from_str(json)
            .map_err(|error| format!("failed to deserialize environment manifest: {error}"))?;
        if manifest.schema_version != ENVIRONMENT_MANIFEST_SCHEMA_VERSION {
            return Err(format!(
                "unsupported environment-manifest schema version {}: this runtime supports v{}",
                manifest.schema_version, ENVIRONMENT_MANIFEST_SCHEMA_VERSION
            ));
        }
        Ok(manifest)
    }

    /// Serialize the manifest envelope.
    pub fn to_json(&self) -> Result<String, String> {
        serde_json::to_string(self)
            .map_err(|error| format!("failed to serialize environment manifest: {error}"))
    }

    /// A compact, bounded, agent-readable summary of the manifest for context
    /// injection beside the snapshot digest.
    ///
    /// Bounded: the tool list renders at most
    /// [`ENVIRONMENT_MANIFEST_DIGEST_MAX_TOOLS`] names before folding the
    /// remainder into a count, so injection cost never grows with the
    /// registry. Contains no credentials by construction — only names,
    /// labels, paths, hashes, counts, and version strings.
    pub fn summary(&self) -> String {
        let mut lines = vec![format!(
            "environment-manifest schema={} revision={} backend={} os={} arch={}",
            self.schema_version,
            self.revision,
            self.backend.map_or("unknown", |backend| backend.as_str()),
            self.os,
            self.arch,
        )];
        lines.push(format!(
            "sandbox={} routing={:?} root={} root_hash={}",
            self.sandbox_profile.map_or("none", |profile| profile.as_str()),
            self.container_routing,
            self.project_root,
            short_hash(&self.project_root_hash),
        ));
        let shown = self.tools.len().min(ENVIRONMENT_MANIFEST_DIGEST_MAX_TOOLS);
        let mut tools_line = format!("tools={}", self.tools.len());
        if shown > 0 {
            let names: Vec<String> = self.tools[..shown]
                .iter()
                .map(|entry| {
                    if entry.registered_name == entry.canonical_policy_name {
                        entry.registered_name.clone()
                    } else {
                        format!("{}({})", entry.registered_name, entry.canonical_policy_name)
                    }
                })
                .collect();
            tools_line.push_str(&format!(" {}", names.join(", ")));
        }
        if self.tools.len() > shown {
            tools_line.push_str(&format!(" +{} more", self.tools.len() - shown));
        }
        lines.push(tools_line);
        if let Some(scope) = &self.policy_scope {
            lines.push(format!(
                "policy ceiling=[{}] writes=[{}] sandbox=[{}]",
                scope.ceiling, scope.write_preset, scope.sandbox
            ));
        }
        if !self.persistence.is_empty() {
            let classes: Vec<&str> = self.persistence.iter().map(|class| class.as_str()).collect();
            lines.push(format!("persists: {}", classes.join(", ")));
        }
        if let Some(restore) = &self.restore {
            lines.push(format!(
                "restore: pre-image-bytes={} (<= {} bytes) git-stash={} untracked={} binary={}",
                restore.file_pre_image_bytes,
                restore.max_pre_image_bytes,
                restore.git_stash_undo,
                restore.untracked_file_restore,
                restore.binary_restore,
            ));
        }
        if !self.executables.is_empty() {
            let observed: Vec<String> = self
                .executables
                .iter()
                .map(|exe| match (&exe.resolved_path, &exe.version) {
                    (Some(path), Some(version)) => {
                        format!("{}={} ({})", exe.name, path, version)
                    }
                    (Some(path), None) => format!("{}={}", exe.name, path),
                    (None, _) => format!("{}=not-found", exe.name),
                })
                .collect();
            lines.push(format!("executables: {}", observed.join(", ")));
        }
        lines.push(format!("collected_at_ms={} revision={}", self.collected_at_ms, self.revision));
        lines.join("\n")
    }
}

/// Shorten a hex hash for the summary line: first 12 characters, enough to
/// scope rows without pasting 64 hex digits into every prompt.
fn short_hash(hash: &str) -> &str {
    hash.get(..12).unwrap_or(hash)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_manifest() -> EnvironmentManifest {
        EnvironmentManifest {
            schema_version: ENVIRONMENT_MANIFEST_SCHEMA_VERSION,
            run_id: "run-1".to_string(),
            session_id: "sess-1".to_string(),
            os: "linux".to_string(),
            arch: "x86_64".to_string(),
            backend: Some(ManifestExecutionBackend::InProcess),
            sandbox_profile: None,
            container_routing: CommandRouting::Direct,
            project_root: "/proj".to_string(),
            project_root_hash: "abc123".to_string(),
            tools: vec![
                ManifestToolEntry {
                    registered_name: "write".to_string(),
                    canonical_policy_name: "filesystem".to_string(),
                },
                ManifestToolEntry {
                    registered_name: "shell".to_string(),
                    canonical_policy_name: "shell".to_string(),
                },
            ],
            executables: vec![ExecutableObservation {
                name: "cargo".to_string(),
                resolved_path: Some("/usr/bin/cargo".to_string()),
                version: Some("cargo 1.96.0".to_string()),
                observed_at_ms: 7,
            }],
            policy_scope: Some(PolicyScopeDescription::for_run(None, 9, None)),
            persistence: vec![PersistenceClass::WorkspaceFiles, PersistenceClass::SessionDatabase],
            restore: Some(RestoreCapabilities::current()),
            collected_at_ms: 42,
            revision: 0,
            freshness: Some(FreshnessStamp { collected_at_ms: 42, revision: 0 }),
        }
    }

    #[test]
    fn manifest_round_trip_preserves_every_field() {
        let manifest = sample_manifest();
        let decoded =
            EnvironmentManifest::from_json(&manifest.to_json().expect("manifest serializes"))
                .expect("current version loads");
        assert_eq!(decoded, manifest);
    }

    #[test]
    fn unknown_future_schema_versions_are_rejected() {
        for bad in [0, ENVIRONMENT_MANIFEST_SCHEMA_VERSION + 1, 999] {
            let mut value = serde_json::to_value(sample_manifest()).expect("to value");
            value["schema_version"] = json!(bad);
            let err = EnvironmentManifest::from_json(&value.to_string())
                .expect_err("future version must be rejected");
            assert!(
                err.contains("unsupported environment-manifest schema version"),
                "unexpected error: {err}"
            );
        }
    }

    #[test]
    fn missing_schema_version_defaults_to_current() {
        let mut value = serde_json::to_value(sample_manifest()).expect("to value");
        value.as_object_mut().expect("object").remove("schema_version");
        let decoded =
            EnvironmentManifest::from_json(&value.to_string()).expect("missing version loads");
        assert_eq!(decoded.schema_version, ENVIRONMENT_MANIFEST_SCHEMA_VERSION);
    }

    /// Older-shape readers (a subset struct with all-default fields, as a
    /// pre-slice-6 binary would carry) still decode a current payload:
    /// additive fields are ignored, never fatal.
    #[test]
    fn older_shape_subset_reader_decodes_current_payload() {
        #[derive(Debug, PartialEq, Deserialize)]
        struct LegacyReader {
            #[serde(default)]
            run_id: String,
            #[serde(default)]
            session_id: String,
        }
        let json = sample_manifest().to_json().expect("serializes");
        let legacy: LegacyReader = serde_json::from_str(&json).expect("subset decodes");
        assert_eq!(
            legacy,
            LegacyReader { run_id: "run-1".to_string(), session_id: "sess-1".to_string() }
        );
        // And the full current shape tolerates a sparse legacy payload.
        let sparse = json!({ "run_id": "run-9", "session_id": "sess-9" });
        let decoded = EnvironmentManifest::from_json(&sparse.to_string())
            .expect("sparse payload loads with defaults");
        assert_eq!(decoded.run_id, "run-9");
        assert!(decoded.tools.is_empty());
        assert_eq!(decoded.schema_version, ENVIRONMENT_MANIFEST_SCHEMA_VERSION);
    }

    /// The JSON field surface is exactly the documented set: no credential,
    /// token, key, or secret field can ride the manifest without this test
    /// failing first.
    #[test]
    fn manifest_json_has_exactly_the_documented_fields() {
        let value = serde_json::to_value(sample_manifest()).expect("to value");
        let mut fields: Vec<&str> =
            value.as_object().expect("object").keys().map(String::as_str).collect();
        fields.sort_unstable();
        assert_eq!(
            fields,
            vec![
                "arch",
                "backend",
                "collected_at_ms",
                "container_routing",
                "executables",
                "freshness",
                "os",
                "persistence",
                "policy_scope",
                "project_root",
                "project_root_hash",
                "restore",
                "revision",
                "run_id",
                "sandbox_profile",
                "schema_version",
                "session_id",
                "tools",
            ]
        );
        let serialized = value.to_string().to_lowercase();
        for forbidden in ["token", "secret", "credential", "password", "api_key", "apikey"] {
            assert!(
                !serialized.contains(forbidden),
                "manifest wire form must never carry {forbidden}: {serialized}"
            );
        }
    }

    #[test]
    fn restore_capabilities_match_the_chunked_store_ceilings() {
        let restore = RestoreCapabilities::current();
        assert!(restore.file_pre_image_bytes);
        assert_eq!(restore.max_pre_image_bytes, CHECKPOINT_TOTAL_LIMIT_BYTES as u64);
        assert!(restore.git_stash_undo);
        assert!(
            !restore.untracked_file_restore,
            "git-stash omits -u: untracked restore is not claimed"
        );
        assert!(restore.binary_restore, "the chunked store banks within-budget binary byte-exact");
    }

    #[test]
    fn summary_is_bounded_and_names_backend_and_alias_pairs() {
        let mut manifest = sample_manifest();
        manifest.tools = (0..50)
            .map(|i| ManifestToolEntry {
                registered_name: format!("tool-{i:02}"),
                canonical_policy_name: format!("tool-{i:02}"),
            })
            .collect();
        let summary = manifest.summary();
        assert!(summary.contains("backend=in-process"), "{summary}");
        assert!(summary.lines().count() <= 8, "bounded digest:\n{summary}");
        assert!(summary.contains("+38 more"), "overflow folds into a count:\n{summary}");
        assert!(!summary.contains("tool-49"), "folded names stay out:\n{summary}");

        // The alias pair renders under both identities.
        let alias_summary = sample_manifest().summary();
        assert!(alias_summary.contains("write(filesystem)"), "{alias_summary}");
    }

    #[test]
    fn backend_and_persistence_labels_are_stable() {
        assert_eq!(ManifestExecutionBackend::InProcess.as_str(), "in-process");
        assert_eq!(ManifestExecutionBackend::Supervised.as_str(), "supervised");
        assert_eq!(PersistenceClass::WorkspaceFiles.as_str(), "workspace-files");
        assert_eq!(PersistenceClass::SessionDatabase.as_str(), "session-db");
        assert_eq!(PersistenceClass::MemoryIndex.as_str(), "memory-index");
        assert_eq!(
            serde_json::to_value(ManifestExecutionBackend::Supervised).expect("serializes"),
            json!("supervised")
        );
    }

    #[test]
    fn command_routing_serde_round_trips_with_new_derives() {
        for routing in [CommandRouting::Direct, CommandRouting::Containerized] {
            let encoded = serde_json::to_value(routing).expect("serializes");
            let decoded: CommandRouting = serde_json::from_value(encoded).expect("deserializes");
            assert_eq!(decoded, routing);
        }
    }
}
