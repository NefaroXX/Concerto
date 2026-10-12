//! ADR-82 H05 — `verify.run`: verification as a callable service.
//!
//! This module owns the reusable verification service: the freshness/stamp
//! logic (H04 run-scoped freshness clock, formerly private to
//! [`crate::agent_loop::AgentLoop`]), the per-file sweep and harness record
//! builders the loop has always used, four labeled adapters, a coalescing
//! layer over workspace-wide spawns, and the `verify.run` tool surface.
//!
//! # One implementation, two consumers
//!
//! Both the single-agent loop and the `verify.run` tool call the free
//! functions here ([`verify_file_changes`], [`run_project_tests`], the
//! freshness predicates). Each consumer stamps records with its OWN run
//! clock — the loop keeps its fields and delegates evaluation here, the
//! service keeps a [`FreshnessClock`] per run — so a record always cites the
//! generation it actually ran against. There is deliberately no shared
//! mutable clock: two writers on one clock would stamp one run's checks with
//! another run's epoch.
//!
//! # Evidence, not a dispatch rule (ADR-65, ADR-71)
//!
//! Everything here evaluates facts. Nothing here selects work, ends work, or
//! authorizes anything: the loop's gate remains the sole authority, and
//! `verify.run` carries empty capabilities through the normal
//! [`concerto_core::executor::ToolExecutor`] policy/audit path like any
//! other read.
//!
//! # Honesty contract
//!
//! Command success proves only the command's declared assertion. Each
//! adapter names what it actually proves (see [`ADAPTER_PROVES`]):
//! reference existence does not establish page rendering, `cargo check`
//! does not establish behavioral correctness, and a quiet language server
//! does not establish correctness either.
//!
//! # Materialization honesty
//!
//! Verification reads on-disk bytes and records the blake3 fingerprint of
//! the bytes it actually checked. Callers holding uncommitted overlay state
//! must materialize (commit/flush to disk) before invoking: when the caller
//! reports [`VerifyRequest::overlay_pending`], the result envelope says so
//! explicitly instead of claiming clean-disk state.
//!
//! # Coalescing
//!
//! Workspace-wide spawns (`cargo check`, `npm test`, the eval harness) are
//! cached on the run generation the freshness clock already computes
//! (`{base}#{epoch}`). Repeated project-wide requests for the same
//! generation reuse the cached spawn; per-file requests each build their own
//! record (own path/scope/fingerprint) from the cached spawn. A changed file
//! re-scopes to that file's adapters instead of re-running project-wide work.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use camino::Utf8PathBuf;
use concerto_core::types::{
    CapabilitySet, EvalResult, PathPolicyFacts, SessionContext, ToolOutput, ToolRegistry,
    VerificationOutcome, VerificationSummary,
};
use concerto_core::{CancellationToken, ToolError};
use concerto_eval::{EvalEngine, EvalError};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Bounds and identities
// ---------------------------------------------------------------------------

/// Registry name of the H05 operation. Dotted like `environment.describe`.
pub const VERIFY_RUN_TOOL_NAME: &str = "verify.run";

/// Default per-run wall-clock budget when the caller names none.
pub const VERIFY_DEFAULT_TIMEOUT_MS: u64 = 120_000;

/// Hard ceiling for a caller-supplied timeout (the eval engine's own default
/// is 300s; a verify run never outlives this ceiling).
pub const VERIFY_MAX_TIMEOUT_MS: u64 = 600_000;

/// Default budget for a single spawned check on the loop path (matches the
/// eval engine's own default so the shared implementation stays
/// behavior-preserving where the loop previously waited unboundedly).
pub const VERIFY_SPAWN_TIMEOUT: Duration = Duration::from_secs(300);

/// Per-record output cap: records, not dumps (the eval harness's own 2000-char
/// tail precedent).
pub const VERIFY_OUTPUT_TAIL_LIMIT: usize = 2000;

/// Envelope record cap (the filesystem-`list` bounded shape: capped with
/// `truncated`/`total_records` flags, never an unbounded dump).
pub const VERIFY_MAX_RECORDS: usize = 32;

/// Verifier identity stamped on every syntax/build record (ADR-82 §3 R2
/// verifier axis). Shared with the loop's sweep: same checks, same identity.
pub const VERIFY_SWEEP_VERIFIER: &str = "concerto-verify-sweep";

/// Verifier identity stamped on every eval-harness record.
pub const EVAL_HARNESS_VERIFIER: &str = "concerto-eval-harness";

/// Verifier identity stamped on every language-server record.
pub const VERIFY_LSP_VERIFIER: &str = "concerto-verify-lsp";

/// Verifier identity stamped on every existence record.
pub const VERIFY_EXISTENCE_VERIFIER: &str = "concerto-verify-existence";

/// What the build/syntax adapter actually proves (ADR honesty: `cargo check`
/// does not establish behavioral correctness).
pub const BUILD_PROVES: &str =
    "proves: the checked code parses and builds (syntax/build only); does not establish behavioral correctness";

/// What the project-tests adapter actually proves.
pub const EVAL_PROVES: &str =
    "proves: the project's test suite passed at this scope (Harness|Fallback provenance in detail); does not establish correctness beyond the suite";

/// What the LSP adapter actually proves.
pub const LSP_PROVES: &str =
    "proves: the language server reported this diagnostic state for the checked file(s) (editor diagnostics only); does not establish correctness";

/// What the existence adapter actually proves (ADR honesty: reference
/// existence does not establish page rendering).
pub const EXISTENCE_PROVES: &str =
    "existence-only check: proves referenced paths exist; does not establish page rendering or runtime behavior";

/// Envelope honesty note: command success proves only the command's declared
/// assertion (one line per adapter in [`ADAPTER_PROVES_TABLE`]).
pub const VERIFY_HONESTY_NOTE: &str =
    "command success proves only the command's declared assertion (see adapters[].proves); \
     a passing record is evidence for exactly that assertion, nothing more";

/// Notice carried on the envelope when the run cannot claim clean-disk
/// state: the caller reported uncommitted overlay changes, live overlay state
/// was detected, or the overlay state is simply unknown (an omitted flag with
/// no reachable overlay source). The records describe on-disk bytes, never
/// the claimed overlay state.
pub const OVERLAY_PENDING_NOTICE: &str =
    "overlay-pending: uncommitted overlay state was reported, detected, or could not be ruled out; \
     records below describe on-disk bytes and may not reflect the claimed state";

// ---------------------------------------------------------------------------
// Freshness (H04 logic, owned here; the loop delegates)
// ---------------------------------------------------------------------------

/// ADR-82 H04: why one typed-`Passed` record does not count as fresh passing
/// evidence. Each cause cites the rule that fires it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FreshnessFailure {
    /// The record is a typed `Passed` but its R2 freshness set is incomplete.
    MissingAxes,
    /// The record's generation base differs from the run's current base.
    Generation,
    /// Same generation base, but a relevant audited mutation landed after the
    /// check (positional per record scope).
    Superseded,
    /// The checked bytes on disk no longer hash to the recorded fingerprint.
    Fingerprint,
}

impl FreshnessFailure {
    /// Machine-checkable cause label for `Blocked` reasons.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MissingAxes => "missing-axes",
            Self::Generation => "generation",
            Self::Superseded => "superseded",
            Self::Fingerprint => "fingerprint",
        }
    }
}

/// ADR-82 H04 freshness mode of a gated ledger.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateFreshness {
    Absent,
    NotPassed,
    Stale,
}

impl GateFreshness {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Absent => "absent",
            Self::NotPassed => "not-passed",
            Self::Stale => "stale",
        }
    }
}

/// One stale typed-`Passed` record as reported in a `Blocked` reason.
#[derive(Debug, Clone)]
pub struct StaleRecord {
    pub path: String,
    pub command: String,
    pub cause: FreshnessFailure,
}

/// ADR-82 H04: the gated arm's verdict over a verification ledger.
#[derive(Debug, Clone, Default)]
pub struct GateLedger {
    /// Records that are fresh-passing evidence right now.
    pub fresh: usize,
    /// Records with typed `Some(Passed)` (fresh or stale).
    pub typed_passes: usize,
    /// Per-record staleness causes for the `Blocked` reason.
    pub stale: Vec<StaleRecord>,
}

impl GateLedger {
    /// The reason's `freshness=` mode.
    pub fn freshness(&self, record_count: usize) -> GateFreshness {
        if self.typed_passes > 0 {
            GateFreshness::Stale
        } else if record_count > 0 {
            GateFreshness::NotPassed
        } else {
            GateFreshness::Absent
        }
    }
}

/// Split an H04 run generation (`{base}#{epoch}`) into its base and epoch.
/// `None` for generations no H04 clock stamped — those can never equal a
/// current generation and report as `Generation`.
pub fn split_run_generation(generation: &str) -> Option<(&str, u64)> {
    let (base, epoch) = generation.rsplit_once('#')?;
    Some((base, epoch.parse::<u64>().ok()?))
}

/// Resolve a verification record's path exactly the way the sweep hashes it:
/// absolute spellings stand alone, relative spellings resolve under the
/// project root. Stamp-time hashing and gate-time re-stat must read the same
/// bytes.
pub fn resolve_verification_path(project_root: &Path, path: &Utf8PathBuf) -> PathBuf {
    if path.is_absolute() {
        PathBuf::from(path.as_str())
    } else {
        project_root.join(path.as_std_path())
    }
}

/// Machine-checkable ledger label for one verification record.
pub fn verification_ledger_label(record: &VerificationSummary) -> &'static str {
    match record.outcome {
        Some(outcome) => outcome.as_str(),
        None if record.passed => "legacy-passed",
        None => "legacy-failed",
    }
}

/// Maximum ledger records itemized in a strict-gate `Blocked` reason.
pub const STRICT_GATE_REASON_DETAIL_LIMIT: usize = 8;

/// Build the machine-checkable `Blocked` reason for the strict verification
/// gate (ADR-82 slice 4, R5; freshness modes by H04).
pub fn strict_gate_blocked_reason(
    output: &concerto_core::types::AgentOutput,
    task_finished: bool,
    freshness: GateFreshness,
    stale: &[StaleRecord],
) -> String {
    let mut counts: std::collections::BTreeMap<&'static str, usize> =
        std::collections::BTreeMap::new();
    for record in &output.verification {
        *counts.entry(verification_ledger_label(record)).or_default() += 1;
    }
    let outcomes = if counts.is_empty() {
        "absent".to_owned()
    } else {
        counts.iter().map(|(label, count)| format!("{label}:{count}")).collect::<Vec<_>>().join(",")
    };
    let mut reason = format!(
        "strict_verification_gate: action-required task ended without required verification \
         (task_finished={task_finished} verification_passed=false require_verification=true \
         records={} outcomes=[{outcomes}] freshness={})",
        output.verification.len(),
        freshness.as_str(),
    );
    let detail: Vec<String> = output
        .verification
        .iter()
        .take(STRICT_GATE_REASON_DETAIL_LIMIT)
        .map(|record| {
            format!("{}:{}:{}", record.path, record.command, verification_ledger_label(record))
        })
        .collect();
    if !detail.is_empty() {
        reason.push_str(&format!(" detail=[{}]", detail.join("; ")));
        let rest = output.verification.len().saturating_sub(detail.len());
        if rest > 0 {
            reason.push_str(&format!(" +{rest} more"));
        }
    }
    if !stale.is_empty() {
        let causes: Vec<String> = stale
            .iter()
            .take(STRICT_GATE_REASON_DETAIL_LIMIT)
            .map(|stale| format!("{}:{}:{}", stale.path, stale.command, stale.cause.as_str()))
            .collect();
        reason.push_str(&format!(" stale_detail=[{}]", causes.join("; ")));
        let rest = stale.len().saturating_sub(causes.len());
        if rest > 0 {
            reason.push_str(&format!(" +{rest} more"));
        }
    }
    reason
}

/// ADR-82 H04 run-scoped freshness context stamped onto verification records
/// at check time: the current run generation plus the run's audited write
/// evidence. A default (empty) stamp leaves records display-grade.
#[derive(Debug, Clone, Default)]
pub struct VerificationStamp {
    pub generation: String,
    /// `(tool-reported path spelling, whiteboard event id)` of every executed
    /// file-affecting write this run, in order.
    pub writes: Vec<(String, String)>,
}

impl VerificationStamp {
    /// Backing evidence for one checked path: the whiteboard event ids of
    /// this run's audited writes whose canonical project-relative key matches
    /// the checked path (ADR-65 F5d key space).
    pub fn evidence_for(&self, project_root: &Path, record_path: &Utf8PathBuf) -> Vec<String> {
        let Some(canonical) =
            crate::tool_facts::canonical_project_path(project_root, record_path.as_str())
        else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for (raw, event_id) in &self.writes {
            if crate::tool_facts::canonical_project_path(project_root, raw).as_deref()
                == Some(canonical.as_str())
                && !out.iter().any(|seen| seen == event_id)
            {
                out.push(event_id.clone());
            }
        }
        out
    }

    /// Backing evidence for a workspace-scope check: every audited write is
    /// relevant to the workspace as a whole (F-WORKSPACE-SUPERSEDE).
    pub fn workspace_evidence(&self) -> Vec<String> {
        let mut out = Vec::new();
        for (_, event_id) in &self.writes {
            if !out.iter().any(|seen| seen == event_id) {
                out.push(event_id.clone());
            }
        }
        out
    }
}

/// The run's current verification generation (`{base}#{epoch}`, `""` when
/// the clock was never armed).
pub fn current_generation_for(base: &str, epoch: u64) -> String {
    if base.is_empty() {
        String::new()
    } else {
        format!("{base}#{epoch}")
    }
}

/// Advance the run freshness clock for one executed file-affecting write and
/// return the new epoch. Pure bookkeeping over the caller's state: a failed
/// or interrupted write still advances (it ran and may have mutated
/// unpredictably); a `Denied` refusal never reaches here.
pub fn advance_for_write(
    project_root: &Path,
    paths: &[String],
    event_id: Option<&str>,
    epoch: u64,
    path_write_epoch: &mut HashMap<String, u64>,
    writes: &mut Vec<(String, String)>,
) -> u64 {
    let epoch = epoch.saturating_add(1);
    for path in paths {
        if let Some(canonical) = crate::tool_facts::canonical_project_path(project_root, path) {
            path_write_epoch.insert(canonical, epoch);
        }
    }
    if let Some(event_id) = event_id {
        for path in paths {
            writes.push((path.clone(), event_id.to_string()));
        }
    }
    epoch
}

/// Capture the content-addressed workspace base generation from the existing
/// ADR-65 §2 source. Fails soft to `""`: gated checks then report stale
/// `generation` instead of passing on an unknown workspace (fail-closed).
pub fn capture_base_generation(project_root: &Path, cancel: &CancellationToken) -> String {
    let project_dir = project_root.to_path_buf();
    let cancel = cancel.clone();
    // Synchronous walk; callers already run off the async executor, and the
    // loop's arming path uses `spawn_blocking` at its own call site.
    let entries = crate::workspace_snapshot::capture_workspace_snapshot(&project_dir, &cancel);
    crate::workspace_snapshot::generation_for(&entries)
}

/// Is this record *fresh-passing evidence* right now? The single
/// implementation behind the loop's gate and the service's
/// [`FreshnessClock::record_freshness`].
pub fn record_freshness(
    project_root: &Path,
    base: &str,
    epoch: u64,
    path_write_epoch: &HashMap<String, u64>,
    record: &VerificationSummary,
) -> Result<(), FreshnessFailure> {
    if !record.is_passing_evidence() {
        return Err(FreshnessFailure::MissingAxes);
    }
    let current = current_generation_for(base, epoch);
    if record.generation != current {
        match (split_run_generation(&record.generation), split_run_generation(&current)) {
            (Some((record_base, record_epoch)), Some((current_base, _)))
                if record_base == current_base =>
            {
                if superseded_at_epoch(project_root, path_write_epoch, record, record_epoch) {
                    return Err(FreshnessFailure::Superseded);
                }
            }
            _ => return Err(FreshnessFailure::Generation),
        }
    }
    if !fingerprint_matches(project_root, record) {
        return Err(FreshnessFailure::Fingerprint);
    }
    Ok(())
}

/// ADR-65 scope split over the run clock: has an audited write superseded
/// this record, given the epoch (`record_epoch`) the record was stamped at?
pub fn superseded_at_epoch(
    project_root: &Path,
    path_write_epoch: &HashMap<String, u64>,
    record: &VerificationSummary,
    record_epoch: u64,
) -> bool {
    if !is_file_scope_record(project_root, record) {
        return true;
    }
    let Some(canonical) =
        crate::tool_facts::canonical_project_path(project_root, record.path.as_str())
    else {
        return true;
    };
    path_write_epoch.get(&canonical).is_some_and(|last_write| *last_write > record_epoch)
}

/// Whether `record` is file-scope for supersession: its `scope` names the
/// single checked file. Anything else is workspace-scope.
pub fn is_file_scope_record(project_root: &Path, record: &VerificationSummary) -> bool {
    let scope = Utf8PathBuf::from(record.scope.as_str());
    resolve_verification_path(project_root, &scope).is_file()
}

/// Re-stat the checked file and compare its blake3 against the recorded
/// fingerprint. Any doubt is stale — never fresh.
pub fn fingerprint_matches(project_root: &Path, record: &VerificationSummary) -> bool {
    let resolved = resolve_verification_path(project_root, &record.path);
    match std::fs::read(&resolved) {
        Ok(bytes) => blake3::hash(&bytes).to_hex().to_string() == record.content_fingerprint,
        Err(_) => false,
    }
}

/// Evaluate a verification ledger for the gated arm: counts fresh-passing
/// records, typed passes, and collects per-record stale causes.
pub fn evaluate_gate_ledger(
    project_root: &Path,
    base: &str,
    epoch: u64,
    path_write_epoch: &HashMap<String, u64>,
    verification: &[VerificationSummary],
) -> GateLedger {
    let mut ledger = GateLedger::default();
    for record in verification {
        if !matches!(record.outcome, Some(VerificationOutcome::Passed)) {
            continue;
        }
        ledger.typed_passes += 1;
        match record_freshness(project_root, base, epoch, path_write_epoch, record) {
            Ok(()) => ledger.fresh += 1,
            Err(cause) => ledger.stale.push(StaleRecord {
                path: record.path.to_string(),
                command: record.command.clone(),
                cause,
            }),
        }
    }
    ledger
}

/// Owned run-scoped freshness clock for the verification service: one per
/// run (the service instance is per-run, built in `setup_shared_tooling`),
/// never shared across runs. The loop keeps its own fields and delegates to
/// the free functions above; both consult the same predicates.
#[derive(Debug, Clone, Default)]
pub struct FreshnessClock {
    base: String,
    epoch: u64,
    path_write_epoch: HashMap<String, u64>,
    writes: Vec<(String, String)>,
}

impl FreshnessClock {
    /// Empty clock: every stamped record stays display-grade until armed.
    pub fn new() -> Self {
        Self::default()
    }

    /// Forget all run state (run boundary).
    pub fn reset(&mut self) {
        self.base.clear();
        self.epoch = 0;
        self.path_write_epoch.clear();
        self.writes.clear();
    }

    /// Set the content-addressed workspace base (run start / refresh).
    pub fn set_base(&mut self, base: String) {
        self.base = base;
    }

    /// The run's current generation (`{base}#{epoch}`).
    pub fn current_generation(&self) -> String {
        current_generation_for(&self.base, self.epoch)
    }

    /// Record one executed file-affecting write (the audited-write path).
    pub fn note_write(&mut self, project_root: &Path, paths: &[String], event_id: Option<&str>) {
        self.epoch = advance_for_write(
            project_root,
            paths,
            event_id,
            self.epoch,
            &mut self.path_write_epoch,
            &mut self.writes,
        );
    }

    /// The freshness context the next check stamps.
    pub fn stamp(&self) -> VerificationStamp {
        VerificationStamp { generation: self.current_generation(), writes: self.writes.clone() }
    }

    /// Freshness verdict for one record against this clock.
    pub fn record_freshness(
        &self,
        project_root: &Path,
        record: &VerificationSummary,
    ) -> Result<(), FreshnessFailure> {
        record_freshness(project_root, &self.base, self.epoch, &self.path_write_epoch, record)
    }

    /// Ledger verdict for the gated arm.
    pub fn evaluate_ledger(
        &self,
        project_root: &Path,
        verification: &[VerificationSummary],
    ) -> GateLedger {
        evaluate_gate_ledger(
            project_root,
            &self.base,
            self.epoch,
            &self.path_write_epoch,
            verification,
        )
    }
}

// ---------------------------------------------------------------------------
// Verification tree (H08): the host workspace or one experiment copy
// ---------------------------------------------------------------------------

/// H08 — what tree a verification run executes against: the host workspace
/// or one managed experiment copy.
///
/// An experiment workspace is a first-class scope, not a bypass: a scoped
/// run arms its own [`FreshnessClock`] from the scope's own root (via
/// [`capture_scoped_generation`]), so its generation, fingerprints, and
/// coalescing keys bind to the experiment copy's bytes. Host evidence never
/// counts for experiment code and vice versa — a generation stamped from the
/// wrong root reports `Generation` staleness instead of passing on foreign
/// bytes. Coalescing, generation binding, and fingerprint invalidation are
/// unchanged; only the root they bind to is scoped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyTree {
    /// The host workspace.
    Host,
    /// One managed experiment copy (`experiment_id` names the
    /// [`crate::experiment::ExperimentRecord`]).
    Experiment {
        /// The experiment under verification.
        experiment_id: String,
    },
}

impl VerifyTree {
    /// Machine-checkable scope label (record attribution, coalescing keys).
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::Host => "host".to_string(),
            Self::Experiment { experiment_id } => format!("experiment:{experiment_id}"),
        }
    }

    /// Whether this scope is an experiment copy.
    #[must_use]
    pub fn is_experiment(&self) -> bool {
        matches!(self, Self::Experiment { .. })
    }
}

/// Errors resolving a verification tree to a filesystem root.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum VerifyTreeError {
    /// An experiment scope without its workspace root: fail-closed (a scoped
    /// run must never silently verify the host tree while claiming the
    /// experiment scope).
    #[error("experiment scope {experiment_id} has no workspace root: refusing to fall back to the host tree")]
    ExperimentRootMissing {
        /// The experiment asking for a root.
        experiment_id: String,
    },
}

/// Resolve the filesystem root a scoped verification run executes against:
/// the host root for [`VerifyTree::Host`], the experiment workspace root
/// for [`VerifyTree::Experiment`]. `None` experiment roots are
/// [`VerifyTreeError::ExperimentRootMissing`] — never a silent host
/// fallback.
pub fn verify_root_for(
    host_root: &Path,
    scope: &VerifyTree,
    experiment_root: Option<&Path>,
) -> Result<PathBuf, VerifyTreeError> {
    match scope {
        VerifyTree::Host => Ok(host_root.to_path_buf()),
        VerifyTree::Experiment { experiment_id } => experiment_root
            .map(Path::to_path_buf)
            .ok_or(VerifyTreeError::ExperimentRootMissing { experiment_id: experiment_id.clone() }),
    }
}

/// Capture the freshness base generation for a scope: the content-addressed
/// workspace generation of the scope's own root — a workspace generation
/// scoped to the experiment copy for experiment scopes, the host generation
/// for host scopes. Returns the resolved root with its generation so the
/// caller arms exactly the clock it verifies.
pub fn capture_scoped_generation(
    host_root: &Path,
    scope: &VerifyTree,
    experiment_root: Option<&Path>,
    cancel: &CancellationToken,
) -> Result<(PathBuf, String), VerifyTreeError> {
    let root = verify_root_for(host_root, scope, experiment_root)?;
    Ok((root.clone(), capture_base_generation(&root, cancel)))
}

// ---------------------------------------------------------------------------
// Record builders (shared by the loop sweep and the service adapters)
// ---------------------------------------------------------------------------

/// Elapsed milliseconds since `start`, saturating instead of wrapping.
pub fn elapsed_ms(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Truncate a record output to the bounded tail (char-boundary safe).
pub fn truncate_output(output: &str) -> String {
    if output.chars().count() <= VERIFY_OUTPUT_TAIL_LIMIT {
        return output.to_string();
    }
    let mut truncated: String = output.chars().take(VERIFY_OUTPUT_TAIL_LIMIT).collect();
    truncated.push_str("…(truncated)");
    truncated
}

/// Build one sweep record with the freshness basis available in this context.
/// `generation` and `evidence_ids` are wired by the caller over every record.
pub async fn sweep_record(
    project_root: &Path,
    path: &Utf8PathBuf,
    command: &str,
    outcome: VerificationOutcome,
    output: String,
    duration_ms: Option<u64>,
    exit_code: Option<i32>,
) -> VerificationSummary {
    sweep_record_with_verifier(
        project_root,
        path,
        command,
        outcome,
        output,
        duration_ms,
        exit_code,
        VERIFY_SWEEP_VERIFIER,
    )
    .await
}

/// [`sweep_record`] with an explicit verifier identity (the existence adapter
/// stamps [`VERIFY_EXISTENCE_VERIFIER`] so its records are attributable).
#[allow(clippy::too_many_arguments)]
pub async fn sweep_record_with_verifier(
    project_root: &Path,
    path: &Utf8PathBuf,
    command: &str,
    outcome: VerificationOutcome,
    output: String,
    duration_ms: Option<u64>,
    exit_code: Option<i32>,
    verifier: &str,
) -> VerificationSummary {
    let full = resolve_verification_path(project_root, path);
    let content_fingerprint = match tokio::fs::read(&full).await {
        Ok(bytes) => blake3::hash(&bytes).to_hex().to_string(),
        Err(_) => String::new(),
    };
    let mut record = VerificationSummary::new(path.clone(), command, outcome, output)
        .with_verifier(verifier)
        .with_scope(path.as_str())
        .with_content_fingerprint(content_fingerprint);
    if let Some(duration_ms) = duration_ms {
        record = record.with_duration_ms(duration_ms);
    }
    if let Some(exit_code) = exit_code {
        record = record.with_exit_code(exit_code);
    }
    record
}

/// One denial record for a build-check spawn the policy refused: a typed
/// `Error` naming the denial (never a pass, never silent, never retried
/// ungoverned). The caller never chose the command — `command` is the
/// adapter's own label — so the denial matches the equivalent `shell` tool
/// call for that command.
pub async fn denied_spawn_record(
    project_root: &Path,
    path: &Utf8PathBuf,
    command: &str,
    detail: &str,
    duration_ms: Option<u64>,
) -> VerificationSummary {
    sweep_record(
        project_root,
        path,
        command,
        VerificationOutcome::Error,
        format!(
            "policy denied: {detail} ({command} not run; \
             the policy decision matches the equivalent shell tool call)"
        ),
        duration_ms,
        None,
    )
    .await
}

/// Map a finished harness run (pass or fail) into its typed verification
/// record. The `detail` carries the full [`EvalResult`] including
/// Harness|Fallback provenance.
pub fn eval_result_record(eval_path: &Utf8PathBuf, result: &EvalResult) -> VerificationSummary {
    let outcome =
        if result.passed { VerificationOutcome::Passed } else { VerificationOutcome::Failed };
    let summary = format!(
        "eval {} {} in {}ms (exit {}). {EVAL_PROVES}",
        result.runner,
        if result.passed { "passed" } else { "failed" },
        result.duration_ms,
        result.exit_code,
    );
    VerificationSummary::new(eval_path.clone(), "eval", outcome, summary)
        .with_verifier(EVAL_HARNESS_VERIFIER)
        .with_scope(eval_path.as_str())
        .with_duration_ms(result.duration_ms)
        .with_exit_code(result.exit_code)
        .with_detail(result.clone())
}

/// Map a harness error into its typed verification record: `Cancelled` maps
/// to `Cancelled`, a policy denial maps to `Error` naming the denial (the
/// check never ran — never a pass, never silent), every other variant to
/// `Error` carrying the error text.
pub fn eval_error_record(eval_path: &Utf8PathBuf, error: &EvalError) -> VerificationSummary {
    match error {
        EvalError::Cancelled => {
            VerificationSummary::cancelled(eval_path.clone(), "eval", "eval cancelled")
        }
        EvalError::Denied(detail) => VerificationSummary::error(
            eval_path.clone(),
            "eval",
            format!(
                "policy denied: {detail} (eval harness not run; \
                 the policy decision matches the equivalent shell tool call)"
            ),
        ),
        other => {
            VerificationSummary::error(eval_path.clone(), "eval", format!("eval error: {other}"))
        }
    }
    .with_verifier(EVAL_HARNESS_VERIFIER)
    .with_scope(eval_path.as_str())
}

/// Map a cached eval outcome back into its record (the coalesce path stores
/// the outcome, then rebuilds the per-request record with its own path,
//  scope, and stamp — same generation, so the rebuild is exact).
#[derive(Debug, Clone)]
pub enum CachedEvalOutcome {
    Ran(EvalResult),
    Cancelled,
    Failed(String),
}

impl CachedEvalOutcome {
    fn into_record(self, eval_path: &Utf8PathBuf) -> VerificationSummary {
        match self {
            Self::Ran(result) => eval_result_record(eval_path, &result),
            Self::Cancelled => eval_error_record(eval_path, &EvalError::Cancelled),
            Self::Failed(message) => VerificationSummary::error(eval_path.clone(), "eval", message)
                .with_verifier(EVAL_HARNESS_VERIFIER)
                .with_scope(eval_path.as_str()),
        }
    }
}

/// Run the project test suite for `files_modified` (the loop's `run_evaluation`
/// and the service's project-tests adapter share this): resolves the
/// manifest-bearing directory nearest the evidence, runs the scoped suite,
/// and stamps the H04 axes (workspace-scope evidence — every audited write is
/// relevant to the workspace as a whole).
pub async fn run_project_tests(
    eval: &EvalEngine,
    project_root: &Path,
    files_modified: &[Utf8PathBuf],
    stamp: &VerificationStamp,
    cancel: CancellationToken,
) -> (Option<EvalResult>, Vec<VerificationSummary>) {
    let eval_dir = EvalEngine::resolve_manifest_root(project_root, files_modified);
    let eval_path =
        Utf8PathBuf::from_path_buf(eval_dir.clone()).unwrap_or_else(|_| Utf8PathBuf::from("eval"));
    let stamp_harness = |mut record: VerificationSummary| {
        record.generation = stamp.generation.clone();
        record.evidence_ids = stamp.workspace_evidence();
        record
    };
    match eval.run_scoped_in_dir(&eval_dir, files_modified, cancel).await {
        Ok(result) => {
            let record = stamp_harness(eval_result_record(&eval_path, &result));
            (Some(result), vec![record])
        }
        Err(EvalError::Cancelled) => {
            (None, vec![stamp_harness(eval_error_record(&eval_path, &EvalError::Cancelled))])
        }
        Err(error) => (None, vec![stamp_harness(eval_error_record(&eval_path, &error))]),
    }
}

/// One language-server record over `path`: empty diagnostics pass, reported
/// diagnostics fail, both labeled editor-diagnostics-only.
///
/// `analyzed` names whether the server published diagnostics for this file at
/// least once this session. A never-analyzed file yields empty diagnostics
/// that must not read as "clean", so it records `Unavailable` instead of a
/// pass — an unanalyzed file proves nothing either way.
pub async fn lsp_record(
    project_root: &Path,
    path: &Utf8PathBuf,
    diagnostics: &[serde_json::Value],
    analyzed: bool,
) -> VerificationSummary {
    if !analyzed {
        return sweep_record_with_verifier(
            project_root,
            path,
            "lsp diagnostics",
            VerificationOutcome::Unavailable,
            format!(
                "lsp diagnostics unavailable: {path} was never analyzed in this session \
                 (the language server published no diagnostics for it); \
                 empty diagnostics prove nothing. {LSP_PROVES}"
            ),
            None,
            None,
            VERIFY_LSP_VERIFIER,
        )
        .await;
    }
    let rendered: Vec<String> = diagnostics
        .iter()
        .take(8)
        .map(|diagnostic| {
            let text = diagnostic.to_string();
            if text.chars().count() > 300 {
                truncate_output(&text)
            } else {
                text
            }
        })
        .collect();
    let (outcome, output) = if diagnostics.is_empty() {
        (
            VerificationOutcome::Passed,
            format!(
                "lsp diagnostics: no diagnostics reported for {path} \
                 (analyzed in this session). {LSP_PROVES}"
            ),
        )
    } else {
        (
            VerificationOutcome::Failed,
            format!(
                "lsp diagnostics: {} diagnostic(s) for {path}: [{}]{}. {LSP_PROVES}",
                diagnostics.len(),
                rendered.join("; "),
                if diagnostics.len() > rendered.len() {
                    format!(" +{} more", diagnostics.len() - rendered.len())
                } else {
                    String::new()
                },
            ),
        )
    };
    sweep_record_with_verifier(
        project_root,
        path,
        "lsp diagnostics",
        outcome,
        output,
        None,
        None,
        VERIFY_LSP_VERIFIER,
    )
    .await
}

/// Collect local `href`/`src` references missing under `project_root` from
/// already-read file `content`. Pure and bounded by the caller's content.
pub fn missing_local_references(project_root: &Path, content: &str) -> Vec<String> {
    let mut missing = Vec::new();
    for line in content.lines() {
        for (marker, offset) in [("href=\"", 6_usize), ("src=\"", 5_usize)] {
            let mut rest = line;
            while let Some(start) = rest.find(marker) {
                let after = &rest[start + offset..];
                let Some(end) = after.find('"') else { break };
                let target = &after[..end];
                let external = target.starts_with("http") || target.starts_with('#');
                if !external && !target.is_empty() {
                    let referenced = project_root.join(target);
                    if !referenced.exists() && !missing.iter().any(|seen| seen == target) {
                        missing.push(target.to_string());
                    }
                }
                rest = &after[end + 1..];
            }
        }
    }
    missing
}

/// One existence record over an HTML/CSS `path`: explicitly labeled
/// existence-only in the record (ADR honesty: reference existence does not
/// establish page rendering). `verifier` lets the loop keep its sweep
/// identity while the service stamps [`VERIFY_EXISTENCE_VERIFIER`].
pub async fn reference_check_record(
    project_root: &Path,
    path: &Utf8PathBuf,
    verifier: &str,
) -> VerificationSummary {
    let full = resolve_verification_path(project_root, path);
    let content = tokio::fs::read_to_string(&full).await.unwrap_or_default();
    let missing = missing_local_references(project_root, &content);
    let (outcome, output) = if missing.is_empty() {
        (
            VerificationOutcome::Passed,
            format!("all referenced local files exist for {path}. {EXISTENCE_PROVES}"),
        )
    } else {
        (
            VerificationOutcome::Failed,
            format!("missing referenced files for {path}: {missing:?}. {EXISTENCE_PROVES}"),
        )
    };
    sweep_record_with_verifier(
        project_root,
        path,
        "reference check",
        outcome,
        output,
        None,
        None,
        verifier,
    )
    .await
}

// ---------------------------------------------------------------------------
// Spawner seam (process execution behind the adapters; counting in tests)
// ---------------------------------------------------------------------------

/// One finished child process (stdout/stderr decoded lossily at the seam so
/// adapters never handle raw bytes).
#[derive(Debug, Clone)]
pub struct SpawnOutput {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
    pub code: Option<i32>,
}

/// Why a spawn did not produce output. `Cancelled` stays distinct so
/// cancellation maps to `Cancelled` records instead of collapsing into
/// `Error` (the total mapping the harness path already uses). `Denied` stays
/// distinct so a policy refusal maps to an honestly-labeled denial record —
/// a denied check never ran and must never fall back to an ungoverned spawn.
#[derive(Debug, Clone)]
pub enum SpawnError {
    Cancelled,
    Timeout(String),
    Failed(String),
    Denied(String),
}

/// Language-server diagnostics plus whether the file was actually analyzed in
/// this session (the server published diagnostics for it at least once — see
/// [`concerto_lsp::LspClient::has_seen_file`]).
#[derive(Debug, Clone)]
pub struct LspDiagnostics {
    pub diagnostics: Vec<serde_json::Value>,
    pub analyzed: bool,
}

/// Language-server query failure. `Cancelled` stays distinct for the same
/// reason as [`SpawnError::Cancelled`].
#[derive(Debug, Clone)]
pub enum LspQueryError {
    Cancelled,
    Unavailable(String),
}

/// Process execution behind the verification adapters. The production
/// implementation spawns real check commands; tests inject a stub and count
/// invocations through [`CountingSpawner`].
///
/// ADR-62 single-chokepoint rule: once a run installs its governed engine
/// (via [`VerifySpawner::set_governed_eval`], fed by
/// [`VerifierService::set_eval`]), spawns travel the same policy-gated
/// `shell` path the eval harness uses — a policy decision applies exactly as
/// it would for the equivalent `shell` tool call, the caller never chooses
/// the command (adapters pick it), and a denial surfaces as
/// [`SpawnError::Denied`] (reported honestly, never retried ungoverned).
#[async_trait]
pub trait VerifySpawner: Send + Sync {
    /// Run `program args...` in `cwd`, bounded by `timeout` and `cancel`.
    /// `program`/`args` are adapter-chosen (never caller input).
    async fn spawn(
        &self,
        program: &str,
        args: &[&str],
        cwd: &Path,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<SpawnOutput, SpawnError>;

    /// Run the eval harness (scoped entry point, shared with the loop).
    async fn run_eval(
        &self,
        eval: &EvalEngine,
        dir: &Path,
        files: &[Utf8PathBuf],
        cancel: &CancellationToken,
    ) -> Result<EvalResult, EvalError>;

    /// Read cached language-server diagnostics for `file`, plus whether the
    /// file was actually analyzed in this session.
    async fn lsp_diagnostics(
        &self,
        project_root: &Path,
        file: &Path,
        cancel: &CancellationToken,
    ) -> Result<LspDiagnostics, LspQueryError>;

    /// Install the run's governed engine for [`Self::spawn`]. The default
    /// keeps direct spawning (test/legacy contexts with no executor); the
    /// service calls this from [`VerifierService::set_eval`] so production
    /// runs route through the policy path.
    fn set_governed_eval(&self, _eval: EvalEngine) {}
}

/// Production spawner: real check commands, the real harness, the real
/// language-server cache. Every spawn is bounded by the caller's timeout and
/// racing cancellation (`kill_on_drop` reaps on either).
///
/// Without a governed engine (no [`VerifySpawner::set_governed_eval`] call)
/// spawns run directly, preserving the legacy sweep behavior for contexts
/// with no executor. With one installed, [`Self::spawn`] delegates to the
/// engine's governed command path — the exact path the harness uses when its
/// `process_executor` is set — and a denial is reported, never bypassed.
pub struct SystemSpawner {
    governed_eval: Mutex<Option<EvalEngine>>,
}

impl SystemSpawner {
    pub fn new() -> Self {
        Self { governed_eval: Mutex::new(None) }
    }
}

impl Default for SystemSpawner {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl VerifySpawner for SystemSpawner {
    async fn spawn(
        &self,
        program: &str,
        args: &[&str],
        cwd: &Path,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<SpawnOutput, SpawnError> {
        if cancel.is_cancelled() {
            return Err(SpawnError::Cancelled);
        }
        // Governed path (ADR-62): the run's own engine — the same object the
        // harness routes through — so build checks and test runs share one
        // policy chokepoint. Denials and failures report honestly; there is
        // no fallback to a direct spawn once this path is installed.
        // (The snapshot clones out from under the lock before any `.await`.)
        let governed = self.governed_eval.lock().unwrap_or_else(|error| error.into_inner()).clone();
        if let Some(eval) = governed {
            return match eval.run_command(program, args, cwd, timeout, cancel.clone()).await {
                Ok(out) => Ok(SpawnOutput {
                    success: out.success(),
                    stdout: out.stdout,
                    stderr: out.stderr,
                    code: Some(out.exit_code),
                }),
                Err(EvalError::Cancelled) => Err(SpawnError::Cancelled),
                Err(EvalError::Denied(detail)) => Err(SpawnError::Denied(detail)),
                Err(EvalError::Timeout(_)) => Err(SpawnError::Timeout(format!(
                    "{program} timed out after {}ms",
                    timeout.as_millis()
                ))),
                Err(error) => {
                    Err(SpawnError::Failed(format!("{program} governed spawn failed: {error}")))
                }
            };
        }
        let mut command = tokio::process::Command::new(program);
        command
            .args(args)
            .current_dir(cwd)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let child = command
            .spawn()
            .map_err(|error| SpawnError::Failed(format!("{program} spawn failed: {error}")))?;
        tokio::select! {
            output = child.wait_with_output() => {
                let output = output
                    .map_err(|error| SpawnError::Failed(format!("{program} wait failed: {error}")))?;
                Ok(SpawnOutput {
                    success: output.status.success(),
                    stdout: String::from_utf8_lossy(&output.stdout).to_string(),
                    stderr: String::from_utf8_lossy(&output.stderr).to_string(),
                    code: output.status.code(),
                })
            }
            () = cancel.cancelled() => Err(SpawnError::Cancelled),
            () = tokio::time::sleep(timeout) => {
                Err(SpawnError::Timeout(format!(
                    "{program} timed out after {}ms",
                    timeout.as_millis()
                )))
            }
        }
    }

    async fn run_eval(
        &self,
        eval: &EvalEngine,
        dir: &Path,
        files: &[Utf8PathBuf],
        cancel: &CancellationToken,
    ) -> Result<EvalResult, EvalError> {
        eval.run_scoped_in_dir(dir, files, cancel.clone()).await
    }

    async fn lsp_diagnostics(
        &self,
        project_root: &Path,
        file: &Path,
        cancel: &CancellationToken,
    ) -> Result<LspDiagnostics, LspQueryError> {
        if cancel.is_cancelled() {
            return Err(LspQueryError::Cancelled);
        }
        if !file.is_file() {
            return Err(LspQueryError::Unavailable(format!(
                "lsp diagnostics unavailable: no such file {}",
                file.display()
            )));
        }
        // Fail-soft like every other LSP consumer: a missing server simply
        // yields the (empty) published-diagnostics cache, labeled accordingly.
        let project_id = concerto_core::types::ProjectId::resolve(project_root);
        let client = concerto_lsp::LspManager::get_or_start(
            project_id,
            project_root.to_path_buf(),
            cancel.clone(),
        )
        .await;
        if cancel.is_cancelled() {
            return Err(LspQueryError::Cancelled);
        }
        let key = file.to_string_lossy().to_string();
        let locked = client.lock().await;
        Ok(LspDiagnostics {
            diagnostics: locked.get_diagnostics(&key).await,
            analyzed: locked.has_seen_file(&key).await,
        })
    }

    fn set_governed_eval(&self, eval: EvalEngine) {
        *self.governed_eval.lock().unwrap_or_else(|error| error.into_inner()) = Some(eval);
    }
}

/// Counting wrapper over any spawner: the coalescing seam. Production wraps
/// [`SystemSpawner`]; tests wrap a stub and assert spawn counts.
pub struct CountingSpawner {
    inner: Arc<dyn VerifySpawner>,
    spawns: AtomicUsize,
    evals: AtomicUsize,
    lsp_queries: AtomicUsize,
}

impl CountingSpawner {
    /// Wrap `inner` (usually [`SystemSpawner` in production, a stub in
    /// tests) and count invocations per seam.
    pub fn new(inner: Arc<dyn VerifySpawner>) -> Self {
        Self {
            inner,
            spawns: AtomicUsize::new(0),
            evals: AtomicUsize::new(0),
            lsp_queries: AtomicUsize::new(0),
        }
    }

    /// Process spawns so far (build/syntax checks).
    pub fn spawn_count(&self) -> usize {
        self.spawns.load(Ordering::SeqCst)
    }

    /// Harness runs so far.
    pub fn eval_count(&self) -> usize {
        self.evals.load(Ordering::SeqCst)
    }

    /// Language-server queries so far.
    pub fn lsp_count(&self) -> usize {
        self.lsp_queries.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl VerifySpawner for CountingSpawner {
    async fn spawn(
        &self,
        program: &str,
        args: &[&str],
        cwd: &Path,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<SpawnOutput, SpawnError> {
        self.spawns.fetch_add(1, Ordering::SeqCst);
        self.inner.spawn(program, args, cwd, timeout, cancel).await
    }

    async fn run_eval(
        &self,
        eval: &EvalEngine,
        dir: &Path,
        files: &[Utf8PathBuf],
        cancel: &CancellationToken,
    ) -> Result<EvalResult, EvalError> {
        self.evals.fetch_add(1, Ordering::SeqCst);
        self.inner.run_eval(eval, dir, files, cancel).await
    }

    async fn lsp_diagnostics(
        &self,
        project_root: &Path,
        file: &Path,
        cancel: &CancellationToken,
    ) -> Result<LspDiagnostics, LspQueryError> {
        self.lsp_queries.fetch_add(1, Ordering::SeqCst);
        self.inner.lsp_diagnostics(project_root, file, cancel).await
    }

    fn set_governed_eval(&self, eval: EvalEngine) {
        self.inner.set_governed_eval(eval);
    }
}

// ---------------------------------------------------------------------------
// Sweep (per-file checks; the loop calls this directly)
// ---------------------------------------------------------------------------

/// Check one file by extension, reusing the exact commands the loop has
/// always spawned: `py_compile` for Python, `cargo check` for Rust (once per
/// workspace state — see the coalescing note), `npm test` for JS/TS, and
/// reference checks for HTML/CSS. Returns `None` for extensions with no
/// applicable check (the caller, not the ledger, decides what that means).
async fn sweep_one_file(
    project_root: &Path,
    path: &Utf8PathBuf,
    spawner: &dyn VerifySpawner,
    spawn_timeout: Duration,
    coalesce: &mut HashMap<String, SpawnOutput>,
    generation: &str,
    cancel: &CancellationToken,
) -> Option<VerificationSummary> {
    if cancel.is_cancelled() {
        return None;
    }
    let ext = path.extension().unwrap_or_default();
    match ext {
        "py" => {
            let start = Instant::now();
            match spawner
                .spawn(
                    "python",
                    &["-m", "py_compile", path.as_str()],
                    project_root,
                    spawn_timeout,
                    cancel,
                )
                .await
            {
                Ok(out) if out.success => Some(
                    sweep_record(
                        project_root,
                        path,
                        "py_compile",
                        VerificationOutcome::Passed,
                        format!("py_compile passed. {BUILD_PROVES}"),
                        Some(elapsed_ms(start)),
                        out.code,
                    )
                    .await,
                ),
                Ok(out) => {
                    let detail = if out.stderr.is_empty() { out.stdout } else { out.stderr };
                    Some(
                        sweep_record(
                            project_root,
                            path,
                            "py_compile",
                            VerificationOutcome::Failed,
                            format!("{detail} {BUILD_PROVES}"),
                            Some(elapsed_ms(start)),
                            out.code,
                        )
                        .await,
                    )
                }
                Err(SpawnError::Cancelled) => Some(
                    sweep_record(
                        project_root,
                        path,
                        "py_compile",
                        VerificationOutcome::Cancelled,
                        "py_compile cancelled".to_string(),
                        Some(elapsed_ms(start)),
                        None,
                    )
                    .await,
                ),
                Err(SpawnError::Denied(detail)) => Some(
                    denied_spawn_record(
                        project_root,
                        path,
                        "py_compile",
                        &detail,
                        Some(elapsed_ms(start)),
                    )
                    .await,
                ),
                Err(SpawnError::Timeout(message) | SpawnError::Failed(message)) => Some(
                    sweep_record(
                        project_root,
                        path,
                        "py_compile",
                        VerificationOutcome::Error,
                        message,
                        Some(elapsed_ms(start)),
                        None,
                    )
                    .await,
                ),
            }
        }
        "rs" => {
            if !project_root.join("Cargo.toml").exists() {
                return Some(
                    sweep_record(
                        project_root,
                        path,
                        "cargo check",
                        VerificationOutcome::Unavailable,
                        "unavailable: no Cargo.toml under project root; cargo check not run"
                            .to_string(),
                        None,
                        None,
                    )
                    .await,
                );
            }
            // Coalescing: every changed Rust file in one workspace state
            // shares a single `cargo check` spawn.
            let key = format!("build:cargo-check:{generation}");
            let start = Instant::now();
            let out = match coalesce.get(&key) {
                Some(cached) => cached.clone(),
                None => match spawner
                    .spawn("cargo", &["check"], project_root, spawn_timeout, cancel)
                    .await
                {
                    Ok(out) => {
                        coalesce.insert(key, out.clone());
                        out
                    }
                    Err(SpawnError::Cancelled) => {
                        return Some(
                            sweep_record(
                                project_root,
                                path,
                                "cargo check",
                                VerificationOutcome::Cancelled,
                                "cargo check cancelled".to_string(),
                                Some(elapsed_ms(start)),
                                None,
                            )
                            .await,
                        );
                    }
                    Err(SpawnError::Denied(detail)) => {
                        return Some(
                            denied_spawn_record(
                                project_root,
                                path,
                                "cargo check",
                                &detail,
                                Some(elapsed_ms(start)),
                            )
                            .await,
                        );
                    }
                    Err(SpawnError::Timeout(message) | SpawnError::Failed(message)) => {
                        return Some(
                            sweep_record(
                                project_root,
                                path,
                                "cargo check",
                                VerificationOutcome::Error,
                                message,
                                Some(elapsed_ms(start)),
                                None,
                            )
                            .await,
                        );
                    }
                },
            };
            if out.success {
                Some(
                    sweep_record(
                        project_root,
                        path,
                        "cargo check",
                        VerificationOutcome::Passed,
                        format!("cargo check passed. {BUILD_PROVES}"),
                        Some(elapsed_ms(start)),
                        out.code,
                    )
                    .await,
                )
            } else {
                let detail = if out.stderr.is_empty() { out.stdout } else { out.stderr };
                Some(
                    sweep_record(
                        project_root,
                        path,
                        "cargo check",
                        VerificationOutcome::Failed,
                        format!("{detail} {BUILD_PROVES}"),
                        Some(elapsed_ms(start)),
                        out.code,
                    )
                    .await,
                )
            }
        }
        "js" | "ts" | "jsx" | "tsx" => {
            let package_json = project_root.join("package.json");
            if !package_json.exists() {
                return Some(
                    sweep_record(
                        project_root,
                        path,
                        "npm test",
                        VerificationOutcome::Unavailable,
                        "unavailable: no package.json under project root; npm test not run"
                            .to_string(),
                        None,
                        None,
                    )
                    .await,
                );
            }
            let content = tokio::fs::read_to_string(&package_json).await.unwrap_or_default();
            if !content.contains("\"test\"") {
                return Some(
                    sweep_record(
                        project_root,
                        path,
                        "npm test",
                        VerificationOutcome::Skipped,
                        "skipped (no test script in package.json)".to_string(),
                        None,
                        None,
                    )
                    .await,
                );
            }
            // `npm test` runs the whole suite: coalesce like `cargo check`.
            let key = format!("build:npm-test:{generation}");
            let start = Instant::now();
            let out = match coalesce.get(&key) {
                Some(cached) => cached.clone(),
                None => {
                    match spawner.spawn("npm", &["test"], project_root, spawn_timeout, cancel).await
                    {
                        Ok(out) => {
                            coalesce.insert(key, out.clone());
                            out
                        }
                        Err(SpawnError::Cancelled) => {
                            return Some(
                                sweep_record(
                                    project_root,
                                    path,
                                    "npm test",
                                    VerificationOutcome::Cancelled,
                                    "npm test cancelled".to_string(),
                                    Some(elapsed_ms(start)),
                                    None,
                                )
                                .await,
                            );
                        }
                        Err(SpawnError::Denied(detail)) => {
                            return Some(
                                denied_spawn_record(
                                    project_root,
                                    path,
                                    "npm test",
                                    &detail,
                                    Some(elapsed_ms(start)),
                                )
                                .await,
                            );
                        }
                        Err(SpawnError::Timeout(message) | SpawnError::Failed(message)) => {
                            return Some(
                                sweep_record(
                                    project_root,
                                    path,
                                    "npm test",
                                    VerificationOutcome::Error,
                                    message,
                                    Some(elapsed_ms(start)),
                                    None,
                                )
                                .await,
                            );
                        }
                    }
                }
            };
            if out.success {
                Some(
                    sweep_record(
                        project_root,
                        path,
                        "npm test",
                        VerificationOutcome::Passed,
                        format!("npm test passed. {BUILD_PROVES}"),
                        Some(elapsed_ms(start)),
                        out.code,
                    )
                    .await,
                )
            } else {
                let detail = if out.stderr.is_empty() { out.stdout } else { out.stderr };
                Some(
                    sweep_record(
                        project_root,
                        path,
                        "npm test",
                        VerificationOutcome::Failed,
                        format!("{detail} {BUILD_PROVES}"),
                        Some(elapsed_ms(start)),
                        out.code,
                    )
                    .await,
                )
            }
        }
        "html" | "css" => {
            Some(reference_check_record(project_root, path, VERIFY_SWEEP_VERIFIER).await)
        }
        _ => None,
    }
}

/// Run after each tool call iteration: extension-based verification on each
/// modified file, returning structured results. The loop calls this
/// directly; the service's file-scope path calls
/// [`verify_file_changes_with`] with its own spawner so both share one
/// implementation.
///
/// Direct-spawn legacy: this builds its own [`SystemSpawner`] with no
/// governed engine, so build checks spawn directly. Contexts with an executor
/// (the loop's run path, the service) install it via
/// [`VerifySpawner::set_governed_eval`] and call [`verify_file_changes_with`]
/// instead.
///
/// `stamp` wires the deferred H04 axes at the source — every record gets the
/// run's current generation plus the run's audited write evidence for its
/// own canonical path. A default (empty) stamp reproduces display-grade
/// records.
pub async fn verify_file_changes(
    files: &[Utf8PathBuf],
    project_root: &Path,
    stamp: &VerificationStamp,
) -> Vec<VerificationSummary> {
    let spawner = SystemSpawner::new();
    let mut coalesce = HashMap::new();
    verify_file_changes_with(
        files,
        project_root,
        stamp,
        &spawner,
        VERIFY_SPAWN_TIMEOUT,
        &mut coalesce,
        &CancellationToken::new(),
    )
    .await
}

/// [`verify_file_changes`] with an explicit spawner, spawn timeout,
/// coalescing cache, and cancellation token (the service path).
#[allow(clippy::too_many_arguments)]
pub async fn verify_file_changes_with(
    files: &[Utf8PathBuf],
    project_root: &Path,
    stamp: &VerificationStamp,
    spawner: &dyn VerifySpawner,
    spawn_timeout: Duration,
    coalesce: &mut HashMap<String, SpawnOutput>,
    cancel: &CancellationToken,
) -> Vec<VerificationSummary> {
    let mut results = Vec::new();
    for path in files {
        if cancel.is_cancelled() {
            results.push(
                sweep_record(
                    project_root,
                    path,
                    "verify",
                    VerificationOutcome::Cancelled,
                    "verification cancelled".to_string(),
                    None,
                    None,
                )
                .await,
            );
            break;
        }
        if let Some(record) = sweep_one_file(
            project_root,
            path,
            spawner,
            spawn_timeout,
            coalesce,
            &stamp.generation,
            cancel,
        )
        .await
        {
            results.push(record);
        }
    }
    for record in &mut results {
        record.generation = stamp.generation.clone();
        // A cancelled record carries no meaningful evidence; the sweep
        // records cite their own path's writes.
        if !matches!(record.outcome, Some(VerificationOutcome::Cancelled)) {
            record.evidence_ids = stamp.evidence_for(project_root, &record.path);
        }
    }
    results
}

// ---------------------------------------------------------------------------
// Service request surface
// ---------------------------------------------------------------------------

/// What the run checks: one file or the whole project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyScope {
    /// One workspace-relative (or absolute) file.
    File(Utf8PathBuf),
    /// The project as a whole (harness + manifest-detected build checks +
    /// bounded existence/LSP scans).
    Project,
}

impl VerifyScope {
    /// Scope key for coalescing and stamping (the file's spelling, or the
    /// project root marker).
    fn key(&self) -> String {
        match self {
            Self::File(path) => format!("file:{}", path.as_str()),
            Self::Project => "project".to_string(),
        }
    }
}

/// Which adapter(s) the run executes. `All` runs every applicable adapter
/// for the scope; a single variant re-scopes to that adapter only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VerifySelector {
    #[default]
    All,
    ProjectTests,
    BuildCheck,
    LspDiagnostics,
    Existence,
}

impl VerifySelector {
    /// Stable coalesce/run-cache label.
    fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::ProjectTests => "project-tests",
            Self::BuildCheck => "build-check",
            Self::LspDiagnostics => "lsp-diagnostics",
            Self::Existence => "existence",
        }
    }

    /// Parse a `verifier` override (`eval` is accepted as the harness alias).
    /// Unknown names are rejected with the valid set (bounded message).
    pub fn parse_override(raw: &str) -> Result<Self, String> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "all" => Ok(Self::All),
            "project-tests" | "eval" | "project_tests" => Ok(Self::ProjectTests),
            "build" | "build-check" | "build_check" | "syntax" => Ok(Self::BuildCheck),
            "lsp" | "lsp-diagnostics" | "lsp_diagnostics" | "diagnostics" => Ok(Self::LspDiagnostics),
            "existence" | "reference" | "reference-check" | "references" => Ok(Self::Existence),
            other => Err(format!(
                "unknown verifier '{other}': expected one of all|project-tests|build|lsp-diagnostics|existence"
            )),
        }
    }

    fn includes_tests(self) -> bool {
        matches!(self, Self::All | Self::ProjectTests)
    }

    fn includes_build(self) -> bool {
        matches!(self, Self::All | Self::BuildCheck)
    }

    fn includes_lsp(self) -> bool {
        matches!(self, Self::All | Self::LspDiagnostics)
    }

    fn includes_existence(self) -> bool {
        matches!(self, Self::All | Self::Existence)
    }
}

/// One verification run: scope, adapter selection, wall-clock budget, and the
/// materialization flag. Timeouts and cancellation are honored per run;
/// output is bounded per record and per envelope.
#[derive(Debug, Clone)]
pub struct VerifyRequest {
    pub scope: VerifyScope,
    pub selector: VerifySelector,
    pub timeout: Duration,
    /// The caller holds uncommitted overlay state that was NOT materialized
    /// before this run. The envelope then says so explicitly instead of
    /// claiming clean-disk state.
    pub overlay_pending: bool,
}

impl VerifyRequest {
    /// Project-wide run with the default budget. The `overlay_pending: false`
    /// is an explicit clean-disk attestation by the constructing caller
    /// (service-direct callers, tests) — the `verify.run` tool surface never
    /// defaults here; it resolves through
    /// [`VerifierService::effective_overlay_pending`].
    pub fn project() -> Self {
        Self {
            scope: VerifyScope::Project,
            selector: VerifySelector::All,
            timeout: Duration::from_millis(VERIFY_DEFAULT_TIMEOUT_MS),
            overlay_pending: false,
        }
    }

    /// Single-file run with the default budget (same explicit clean-disk
    /// attestation as [`Self::project`]).
    pub fn file(path: Utf8PathBuf) -> Self {
        Self {
            scope: VerifyScope::File(path),
            selector: VerifySelector::All,
            timeout: Duration::from_millis(VERIFY_DEFAULT_TIMEOUT_MS),
            overlay_pending: false,
        }
    }
}

/// Why [`VerifierService::verify`] did not return records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyError {
    Cancelled,
    Timeout(String),
    InvalidInput(String),
    /// A build-check spawn the policy refused. Handled per-adapter as a
    /// denial record (never a silent pass, never an ungoverned retry); it
    /// only reaches the tool surface as a safety net.
    Denied(String),
}

/// Live overlay-pending source: returns `true` while uncommitted overlay
/// state exists. Installed from the run's overlay handle at registration
/// (desktop `VirtualFs.changed_paths`); `None` (CLI, tests) means the overlay
/// state is unreachable, and an omitted input flag then reports pending
/// (fail-safe: unknown is never clean-disk).
pub type OverlayPendingSource = Arc<dyn Fn() -> bool + Send + Sync>;

/// The reusable verification service. Owns a per-run [`FreshnessClock`], the
/// harness engine (upgradable to the governed executor-backed engine once
/// the run builds one), the spawner seam, and the coalescing caches.
///
/// `std::sync::Mutex` with short critical sections — never held across
/// `.await` (snapshots are cloned out first), mirroring
/// `environment.describe`.
pub struct VerifierService {
    project_root: PathBuf,
    eval: Mutex<EvalEngine>,
    spawner: Arc<dyn VerifySpawner>,
    clock: Mutex<FreshnessClock>,
    /// Live overlay-pending source (`None` when unreachable — CLI/tests).
    overlay_source: Mutex<Option<OverlayPendingSource>>,
    /// Coalesced workspace-wide spawns keyed `{adapter}:{generation}`.
    /// `tokio::sync::Mutex` (not `std`) because the file-scope path holds
    /// the map across `.await` while per-file records build; the guard is
    /// `Send`, so the service future stays `Send`.
    build_cache: tokio::sync::Mutex<HashMap<String, SpawnOutput>>,
    eval_cache: Mutex<HashMap<String, CachedEvalOutcome>>,
}

impl VerifierService {
    /// Build a service over `project_root` with a direct-spawn harness
    /// engine. The run upgrades the engine via [`Self::set_eval`] once the
    /// governed (executor-backed) engine exists, so harness commands travel
    /// the same policy path the loop uses.
    pub fn new(project_root: PathBuf, eval: EvalEngine) -> Self {
        Self::with_spawner(project_root, eval, Arc::new(SystemSpawner::new()))
    }

    /// [`Self::new`] with an explicit spawner (counting or stub in tests).
    pub fn with_spawner(
        project_root: PathBuf,
        eval: EvalEngine,
        spawner: Arc<dyn VerifySpawner>,
    ) -> Self {
        // The harness engine and the build-check spawns share one policy
        // chokepoint: installing the governed engine here (not just for the
        // harness) is what closes the second spawn path (ADR-62).
        spawner.set_governed_eval(eval.clone());
        Self {
            project_root,
            eval: Mutex::new(eval),
            spawner,
            clock: Mutex::new(FreshnessClock::new()),
            overlay_source: Mutex::new(None),
            build_cache: tokio::sync::Mutex::new(HashMap::new()),
            eval_cache: Mutex::new(HashMap::new()),
        }
    }

    /// Install the live overlay-pending source (the run's overlay handle).
    /// Until this is set, an omitted input flag reports pending (fail-safe).
    pub fn set_overlay_source(&self, source: OverlayPendingSource) {
        *self.overlay_source.lock().unwrap_or_else(|error| error.into_inner()) = Some(source);
    }

    /// Whether live overlay state is currently pending, `None` when the
    /// overlay is unreachable (a poisoned lock reads as unreachable — the
    /// caller then falls back to the fail-safe default, never to clean).
    fn overlay_pending_now(&self) -> Option<bool> {
        let guard = self.overlay_source.lock().ok()?;
        guard.as_ref().map(|source| source())
    }

    /// Resolve the envelope's overlay-pending flag from the caller's
    /// attestation plus live state, honest by construction:
    /// - an explicit `true` always wins (the caller attests pending);
    /// - live pending state overrides an explicit `false` (real state beats a
    ///   clean claim — the overlay may hold unflushed writes the caller
    ///   forgot);
    /// - an omitted flag derives from live state when reachable;
    /// - an omitted flag with no reachable state reports pending (fail-safe:
    ///   an unknown overlay state is never reported as clean-disk).
    pub fn effective_overlay_pending(&self, input: Option<bool>) -> bool {
        let live = self.overlay_pending_now();
        match (input, live) {
            (Some(true), _) | (_, Some(true)) => true,
            (Some(false), _) => false,
            (None, Some(false)) => false,
            (None, None) => true,
        }
    }

    /// Upgrade the harness engine (e.g. to the run's governed,
    /// executor-backed engine). The tool remains usable before the upgrade;
    /// it then spawns directly like the loop's sweep does.
    ///
    /// The upgrade also governs build-check spawns: the same engine object
    /// feeds the spawner, so `cargo check` / `py_compile` / `npm test` travel
    /// the policy path the harness uses (ADR-62 single chokepoint).
    pub fn set_eval(&self, eval: EvalEngine) {
        self.spawner.set_governed_eval(eval.clone());
        let mut guarded = self.eval.lock().unwrap_or_else(|error| error.into_inner());
        *guarded = eval;
    }

    /// Record one executed file-affecting write on the service clock (the
    /// audited-write path feeding supersession). Tool-path mutations that
    /// bypass this still invalidate through the live base re-capture: any
    /// disk change moves the content-addressed base.
    pub fn note_write(&self, paths: &[String], event_id: Option<&str>) {
        let mut clock = self.clock.lock().unwrap_or_else(|error| error.into_inner());
        clock.note_write(&self.project_root, paths, event_id);
    }

    /// Forget all run state (run boundary).
    pub async fn reset(&self) {
        {
            let mut clock = self.clock.lock().unwrap_or_else(|error| error.into_inner());
            clock.reset();
        }
        self.build_cache.lock().await.clear();
        self.eval_cache.lock().unwrap_or_else(|error| error.into_inner()).clear();
    }

    /// Re-capture the workspace base (fail-soft: a failed capture keeps the
    /// previous base and every gated check then reports stale).
    ///
    /// When the base moved, the coalescing caches are dropped with it: they
    /// are keyed by generation, so a moved base makes every entry
    /// stale-but-unreachable — clearing bounds memory in a long-lived
    /// process instead of accumulating one dead entry per workspace state.
    async fn refresh_base(&self, cancel: &CancellationToken) {
        if cancel.is_cancelled() {
            return;
        }
        let project_dir = self.project_root.clone();
        let token = cancel.clone();
        let captured = tokio::task::spawn_blocking(move || {
            let entries =
                crate::workspace_snapshot::capture_workspace_snapshot(&project_dir, &token);
            crate::workspace_snapshot::generation_for(&entries)
        })
        .await
        .unwrap_or_default();
        if captured.is_empty() {
            tracing::warn!(
                "verify.run: workspace generation capture failed; freshness falls back to the previous base"
            );
            return;
        }
        let base_changed = {
            let mut clock = self.clock.lock().unwrap_or_else(|error| error.into_inner());
            if clock.base == captured {
                false
            } else {
                clock.set_base(captured);
                true
            }
        };
        if base_changed {
            self.build_cache.lock().await.clear();
            self.eval_cache.lock().unwrap_or_else(|error| error.into_inner()).clear();
        }
    }

    /// Snapshot the clock fields one run needs (cloned out from under the
    /// lock before any `.await`).
    fn snapshot(&self) -> (String, VerificationStamp) {
        let clock = self.clock.lock().unwrap_or_else(|error| error.into_inner());
        (clock.current_generation(), clock.stamp())
    }

    /// Freshness verdict for one record against the service clock (live base
    /// re-captured first, so out-of-band mutations invalidate).
    pub async fn check_freshness(
        &self,
        record: &VerificationSummary,
        cancel: &CancellationToken,
    ) -> Result<(), FreshnessFailure> {
        self.refresh_base(cancel).await;
        let clock = self.clock.lock().unwrap_or_else(|error| error.into_inner());
        clock.record_freshness(&self.project_root, record)
    }

    /// Envelope state label: what bytes this run claims to have checked.
    fn checked_state(&self, generation: &str, overlay_pending: bool) -> String {
        if overlay_pending {
            OVERLAY_PENDING_NOTICE.to_string()
        } else {
            format!(
                "disk generation={generation} (materialized on-disk bytes; any overlay must be committed before invoking)"
            )
        }
    }

    /// The single public entry point: run `request` to bounded records. Both
    /// the `verify.run` tool and internal evaluation callers use this — the
    /// loop's own sweep/harness path shares the builders, stamped with the
    /// loop's clock instead.
    pub async fn verify(
        &self,
        request: &VerifyRequest,
        cancel: &CancellationToken,
    ) -> Result<Vec<VerificationSummary>, VerifyError> {
        if cancel.is_cancelled() {
            return Err(VerifyError::Cancelled);
        }
        self.refresh_base(cancel).await;
        if cancel.is_cancelled() {
            return Err(VerifyError::Cancelled);
        }
        let (generation, stamp) = self.snapshot();
        let run = self.run_adapters(request, &generation, &stamp, cancel.clone());
        match tokio::time::timeout(request.timeout, run).await {
            Err(_) => Ok(vec![timeout_record(request, &generation)]),
            Ok(Err(error)) => Err(error),
            Ok(Ok(records)) => Ok(bound_records(records)),
        }
    }

    /// Checked-state label for `request` at `generation` (envelope honesty).
    pub fn checked_state_for(&self, request: &VerifyRequest, generation: &str) -> String {
        self.checked_state(generation, request.overlay_pending)
    }

    /// Current generation without a base refresh (envelope/debug use).
    pub fn current_generation(&self) -> String {
        self.clock.lock().unwrap_or_else(|error| error.into_inner()).current_generation()
    }

    /// Run the selected adapters for `request`, stamped with `stamp`.
    async fn run_adapters(
        &self,
        request: &VerifyRequest,
        generation: &str,
        stamp: &VerificationStamp,
        cancel: CancellationToken,
    ) -> Result<Vec<VerificationSummary>, VerifyError> {
        let mut records = Vec::new();
        match &request.scope {
            VerifyScope::File(path) => {
                if cancel.is_cancelled() {
                    return Err(VerifyError::Cancelled);
                }
                // Changing one file re-scopes to that file's adapters instead
                // of re-running project-wide work.
                if request.selector.includes_tests() {
                    records.extend(
                        self.project_tests_records(
                            std::slice::from_ref(path),
                            path,
                            generation,
                            stamp,
                            &cancel,
                        )
                        .await?,
                    );
                }
                if cancel.is_cancelled() {
                    return Err(VerifyError::Cancelled);
                }
                if request.selector.includes_build() || request.selector.includes_existence() {
                    let mut build_cache = self.build_cache.lock().await;
                    let mut swept = verify_file_changes_with(
                        std::slice::from_ref(path),
                        &self.project_root,
                        stamp,
                        self.spawner.as_ref(),
                        request.timeout,
                        &mut build_cache,
                        &cancel,
                    )
                    .await;
                    // The file sweep only produces build/existence records;
                    // drop any adapter the selector did not ask for.
                    swept.retain(|record| {
                        (request.selector.includes_build() && record.command != "reference check")
                            || (request.selector.includes_existence()
                                && record.command == "reference check")
                    });
                    // Existence records from the loop-shaped sweep carry the
                    // sweep identity; the service stamps its own so the
                    // adapter is attributable end to end.
                    for record in &mut swept {
                        if record.command == "reference check" {
                            record.verifier = VERIFY_EXISTENCE_VERIFIER.to_string();
                        }
                    }
                    records.extend(swept);
                }
                if cancel.is_cancelled() {
                    return Err(VerifyError::Cancelled);
                }
                if request.selector.includes_lsp() && is_source_path(path) {
                    records.extend(self.lsp_records(path, stamp, &cancel).await?);
                }
            }
            VerifyScope::Project => {
                if cancel.is_cancelled() {
                    return Err(VerifyError::Cancelled);
                }
                if request.selector.includes_tests() {
                    let eval_path = Utf8PathBuf::from(self.project_root.to_string_lossy().as_ref());
                    records.extend(
                        self.project_tests_records(&[], &eval_path, generation, stamp, &cancel)
                            .await?,
                    );
                }
                if cancel.is_cancelled() {
                    return Err(VerifyError::Cancelled);
                }
                if request.selector.includes_build() {
                    records.extend(
                        self.project_build_records(generation, stamp, request.timeout, &cancel)
                            .await?,
                    );
                }
                if cancel.is_cancelled() {
                    return Err(VerifyError::Cancelled);
                }
                if request.selector.includes_existence() {
                    for path in project_text_files(&self.project_root, &["html", "css"], 16) {
                        if cancel.is_cancelled() {
                            return Err(VerifyError::Cancelled);
                        }
                        let mut record = reference_check_record(
                            &self.project_root,
                            &path,
                            VERIFY_EXISTENCE_VERIFIER,
                        )
                        .await;
                        record.generation = generation.to_string();
                        record.evidence_ids = stamp.evidence_for(&self.project_root, &record.path);
                        records.push(record);
                    }
                }
                if cancel.is_cancelled() {
                    return Err(VerifyError::Cancelled);
                }
                if request.selector.includes_lsp() {
                    for path in project_text_files(
                        &self.project_root,
                        &["rs", "py", "js", "ts", "jsx", "tsx"],
                        8,
                    ) {
                        if cancel.is_cancelled() {
                            return Err(VerifyError::Cancelled);
                        }
                        records.extend(self.lsp_records(&path, stamp, &cancel).await?);
                    }
                }
            }
        }
        Ok(records)
    }

    /// Project-tests adapter: the scoped harness entry point shared with the
    /// loop, coalesced on `(scope, generation)` so repeated project-wide runs
    /// never re-invoke the suite. `Harness|Fallback` provenance travels in
    /// `detail`.
    async fn project_tests_records(
        &self,
        files: &[Utf8PathBuf],
        record_path: &Utf8PathBuf,
        generation: &str,
        stamp: &VerificationStamp,
        cancel: &CancellationToken,
    ) -> Result<Vec<VerificationSummary>, VerifyError> {
        let scope_key = if files.is_empty() { "project".to_string() } else { files[0].to_string() };
        let key = format!("eval:{scope_key}:{generation}");
        let cached =
            self.eval_cache.lock().unwrap_or_else(|error| error.into_inner()).get(&key).cloned();
        let outcome = match cached {
            Some(outcome) => outcome,
            None => {
                if cancel.is_cancelled() {
                    return Err(VerifyError::Cancelled);
                }
                let eval_dir = EvalEngine::resolve_manifest_root(&self.project_root, files);
                let eval = self.eval.lock().unwrap_or_else(|error| error.into_inner()).clone();
                match self.spawner.run_eval(&eval, &eval_dir, files, cancel).await {
                    Ok(result) => CachedEvalOutcome::Ran(result),
                    Err(EvalError::Cancelled) => return Err(VerifyError::Cancelled),
                    Err(error) => CachedEvalOutcome::Failed(format!("eval error: {error}")),
                }
            }
        };
        let mut record = outcome.clone().into_record(record_path);
        record.generation = generation.to_string();
        // Workspace-scope evidence for harness records (F-WORKSPACE-SUPERSEDE);
        // the content-fingerprint axis stays empty — a workspace check names
        // no single file, so harness records remain display-grade by design.
        record.evidence_ids = stamp.workspace_evidence();
        if self.eval_cache.lock().unwrap_or_else(|error| error.into_inner()).get(&key).is_none() {
            self.eval_cache.lock().unwrap_or_else(|error| error.into_inner()).insert(key, outcome);
        }
        Ok(vec![record])
    }

    /// Project-scope build adapter: one coalesced spawn per manifest kind
    /// present (never one `cargo check` per changed file). A policy denial
    /// becomes a denial record for that adapter (the run continues with the
    /// other adapters) — never a silent pass, never an ungoverned retry.
    async fn project_build_records(
        &self,
        generation: &str,
        stamp: &VerificationStamp,
        spawn_timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<Vec<VerificationSummary>, VerifyError> {
        let mut records = Vec::new();
        let root_label = Utf8PathBuf::from(self.project_root.to_string_lossy().as_ref());
        if self.project_root.join("Cargo.toml").exists() {
            if cancel.is_cancelled() {
                return Err(VerifyError::Cancelled);
            }
            let key = format!("build:cargo-check:{generation}");
            match self.cached_spawn(&key, "cargo", &["check"], spawn_timeout, cancel).await {
                Ok(outcome) => {
                    let (result, output) = if outcome.success {
                        (VerificationOutcome::Passed, format!("cargo check passed. {BUILD_PROVES}"))
                    } else {
                        let detail = if outcome.stderr.is_empty() {
                            outcome.stdout.clone()
                        } else {
                            outcome.stderr.clone()
                        };
                        (VerificationOutcome::Failed, format!("{detail} {BUILD_PROVES}"))
                    };
                    let mut record = sweep_record(
                        &self.project_root,
                        &root_label,
                        "cargo check",
                        result,
                        output,
                        Some(outcome.duration_ms),
                        outcome.code,
                    )
                    .await;
                    record.generation = generation.to_string();
                    record.evidence_ids = stamp.workspace_evidence();
                    records.push(record);
                }
                Err(VerifyError::Denied(detail)) => {
                    records.push(
                        self.denied_build_record("cargo check", &detail, generation, stamp).await,
                    );
                }
                Err(other) => return Err(other),
            }
        }
        if self.project_root.join("package.json").exists() {
            if cancel.is_cancelled() {
                return Err(VerifyError::Cancelled);
            }
            let content = tokio::fs::read_to_string(self.project_root.join("package.json"))
                .await
                .unwrap_or_default();
            if content.contains("\"test\"") {
                let key = format!("build:npm-test:{generation}");
                match self.cached_spawn(&key, "npm", &["test"], spawn_timeout, cancel).await {
                    Ok(outcome) => {
                        let (result, output) = if outcome.success {
                            (
                                VerificationOutcome::Passed,
                                format!("npm test passed. {BUILD_PROVES}"),
                            )
                        } else {
                            let detail = if outcome.stderr.is_empty() {
                                outcome.stdout.clone()
                            } else {
                                outcome.stderr.clone()
                            };
                            (VerificationOutcome::Failed, format!("{detail} {BUILD_PROVES}"))
                        };
                        let mut record = sweep_record(
                            &self.project_root,
                            &root_label,
                            "npm test",
                            result,
                            output,
                            Some(outcome.duration_ms),
                            outcome.code,
                        )
                        .await;
                        record.generation = generation.to_string();
                        record.evidence_ids = stamp.workspace_evidence();
                        records.push(record);
                    }
                    Err(VerifyError::Denied(detail)) => {
                        records.push(
                            self.denied_build_record("npm test", &detail, generation, stamp).await,
                        );
                    }
                    Err(other) => return Err(other),
                }
            }
        }
        Ok(records)
    }

    /// One project-scope denial record: the file-scope denial shape
    /// re-stamped workspace-scope (same generation, workspace evidence).
    async fn denied_build_record(
        &self,
        command: &str,
        detail: &str,
        generation: &str,
        stamp: &VerificationStamp,
    ) -> VerificationSummary {
        let root_label = Utf8PathBuf::from(self.project_root.to_string_lossy().as_ref());
        let mut record =
            denied_spawn_record(&self.project_root, &root_label, command, detail, None).await;
        record.generation = generation.to_string();
        record.evidence_ids = stamp.workspace_evidence();
        record
    }

    /// One coalesced workspace-wide spawn: the cache key already carries the
    /// run generation, so a repeat for the same workspace state never
    /// re-invokes the command.
    async fn cached_spawn(
        &self,
        key: &str,
        program: &str,
        args: &[&str],
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<TimedSpawn, VerifyError> {
        if let Some(cached) = self.build_cache.lock().await.get(key) {
            return Ok(TimedSpawn {
                success: cached.success,
                stdout: cached.stdout.clone(),
                stderr: cached.stderr.clone(),
                code: cached.code,
                duration_ms: 0,
            });
        }
        if cancel.is_cancelled() {
            return Err(VerifyError::Cancelled);
        }
        let start = Instant::now();
        match self.spawner.spawn(program, args, &self.project_root, timeout, cancel).await {
            Ok(out) => {
                let timed = TimedSpawn {
                    success: out.success,
                    stdout: out.stdout.clone(),
                    stderr: out.stderr.clone(),
                    code: out.code,
                    duration_ms: elapsed_ms(start),
                };
                self.build_cache.lock().await.insert(
                    key.to_string(),
                    SpawnOutput {
                        success: out.success,
                        stdout: out.stdout,
                        stderr: out.stderr,
                        code: out.code,
                    },
                );
                Ok(timed)
            }
            Err(SpawnError::Cancelled) => Err(VerifyError::Cancelled),
            Err(SpawnError::Denied(detail)) => Err(VerifyError::Denied(detail)),
            Err(SpawnError::Timeout(message) | SpawnError::Failed(message)) => {
                Err(VerifyError::InvalidInput(message))
            }
        }
    }

    /// LSP adapter over one file: cached diagnostics in, typed record out.
    async fn lsp_records(
        &self,
        path: &Utf8PathBuf,
        stamp: &VerificationStamp,
        cancel: &CancellationToken,
    ) -> Result<Vec<VerificationSummary>, VerifyError> {
        if cancel.is_cancelled() {
            return Err(VerifyError::Cancelled);
        }
        let full = resolve_verification_path(&self.project_root, path);
        match self.spawner.lsp_diagnostics(&self.project_root, &full, cancel).await {
            Ok(found) => {
                let mut record =
                    lsp_record(&self.project_root, path, &found.diagnostics, found.analyzed).await;
                record.generation = stamp.generation.clone();
                record.evidence_ids = stamp.evidence_for(&self.project_root, &record.path);
                Ok(vec![record])
            }
            Err(LspQueryError::Cancelled) => Err(VerifyError::Cancelled),
            Err(LspQueryError::Unavailable(message)) => {
                let mut record = sweep_record_with_verifier(
                    &self.project_root,
                    path,
                    "lsp diagnostics",
                    VerificationOutcome::Unavailable,
                    message,
                    None,
                    None,
                    VERIFY_LSP_VERIFIER,
                )
                .await;
                record.generation = stamp.generation.clone();
                Ok(vec![record])
            }
        }
    }
}

/// One coalesced spawn result plus the measured duration of the spawn that
/// produced it (cache hits report `duration_ms: 0`).
struct TimedSpawn {
    success: bool,
    stdout: String,
    stderr: String,
    code: Option<i32>,
    duration_ms: u64,
}

/// Whether `path` names a source file the LSP adapter covers.
fn is_source_path(path: &Utf8PathBuf) -> bool {
    matches!(path.extension().unwrap_or_default(), "rs" | "py" | "js" | "ts" | "jsx" | "tsx")
}

/// Bounded recursive scan for `extensions` under `root` (byte-sorted for
/// determinism). Skips hidden directories and `target`/`node_modules`.
fn project_text_files(root: &Path, extensions: &[&str], limit: usize) -> Vec<Utf8PathBuf> {
    fn walk(
        dir: &Path,
        root: &Path,
        extensions: &[&str],
        out: &mut Vec<Utf8PathBuf>,
        limit: usize,
    ) {
        if out.len() >= limit {
            return;
        }
        let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
            .map(|read| read.filter_map(|entry| entry.ok().map(|entry| entry.path())).collect())
            .unwrap_or_default();
        entries.sort();
        for entry in entries {
            if out.len() >= limit {
                return;
            }
            let name = entry
                .file_name()
                .map(|base| base.to_string_lossy().to_string())
                .unwrap_or_default();
            if name.starts_with('.') || name == "target" || name == "node_modules" {
                continue;
            }
            if entry.is_dir() {
                walk(&entry, root, extensions, out, limit);
            } else if entry.is_file() {
                let matches = entry
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .is_some_and(|ext| extensions.contains(&ext));
                if matches {
                    let rel = entry.strip_prefix(root).unwrap_or(&entry);
                    if let Some(text) = rel.to_str() {
                        out.push(Utf8PathBuf::from(text));
                    }
                }
            }
        }
    }

    let mut out = Vec::new();
    walk(root, root, extensions, &mut out, limit);
    out
}

/// Timeout record: the run exceeded its own budget (an `Error`, never a
/// pass — a timed-out check proves nothing).
fn timeout_record(request: &VerifyRequest, generation: &str) -> VerificationSummary {
    let scope = request.scope.key();
    let mut record = VerificationSummary::error(
        scope.clone(),
        VERIFY_RUN_TOOL_NAME,
        format!(
            "verify.run timed out after {}ms (scope={scope}, verifier={})",
            request.timeout.as_millis(),
            request.selector.as_str(),
        ),
    )
    .with_verifier(VERIFY_RUN_TOOL_NAME)
    .with_scope(scope);
    record.generation = generation.to_string();
    record
}

/// Bound one run's records: per-record tails truncated, envelope capped.
fn bound_records(mut records: Vec<VerificationSummary>) -> Vec<VerificationSummary> {
    for record in &mut records {
        record.output = truncate_output(&record.output);
    }
    records.truncate(VERIFY_MAX_RECORDS);
    records
}

// ---------------------------------------------------------------------------
// `verify.run` tool (shared registry; normal policy/audit path)
// ---------------------------------------------------------------------------

/// Static adapter contract table: adapter | spawns | proves | labeled how.
/// Served on every envelope so an agent can read the honesty contract
/// without running anything.
fn adapter_proves_table() -> serde_json::Value {
    serde_json::json!([
        {
            "adapter": "project-tests",
            "verifier": EVAL_HARNESS_VERIFIER,
            "spawns": "project test command (cargo test | npm test | pytest | make test) via the eval harness",
            "proves": EVAL_PROVES,
        },
        {
            "adapter": "build-check",
            "verifier": VERIFY_SWEEP_VERIFIER,
            "spawns": "cargo check | python -m py_compile | npm test (whatever the sweep runs for the extension)",
            "proves": BUILD_PROVES,
        },
        {
            "adapter": "lsp-diagnostics",
            "verifier": VERIFY_LSP_VERIFIER,
            "spawns": "none (reads the language-server diagnostics cache)",
            "proves": LSP_PROVES,
        },
        {
            "adapter": "existence",
            "verifier": VERIFY_EXISTENCE_VERIFIER,
            "spawns": "none (filesystem reads only)",
            "proves": EXISTENCE_PROVES,
        },
    ])
}

/// Input for the `verify.run` operation. Every field is
/// `#[serde(default)]`-tolerant: `{}` means "project scope, all adapters,
/// default budget, unknown overlay" — and unknown overlay reports
/// overlay-pending (fail-safe: only an explicit `overlay_pending: false`
/// attests clean-disk state, and live pending state still overrides it).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct VerifyRunInput {
    /// Scope: `"project"` (default) or one workspace-relative file path.
    #[serde(default)]
    pub scope: Option<String>,
    /// Adapter override: `all` (default) | `project-tests` | `build` |
    /// `lsp-diagnostics` | `existence`. Descriptive selection only — never
    /// an authorization input.
    #[serde(default)]
    pub verifier: Option<String>,
    /// Wall-clock budget in milliseconds, clamped to
    /// [`VERIFY_MAX_TIMEOUT_MS`] (default [`VERIFY_DEFAULT_TIMEOUT_MS`]).
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    /// Set when the caller holds uncommitted overlay state that was NOT
    /// materialized. Omit it only when the overlay state is unknown (the
    /// envelope then reports overlay-pending instead of claiming clean-disk
    /// state); pass explicit `false` only to attest that no uncommitted
    /// overlay state exists.
    #[serde(default)]
    pub overlay_pending: Option<bool>,
}

/// The `verify.run` operation: callable verification returning bounded
/// evidence records.
///
/// Empty capability requirements (the `environment.describe` precedent):
/// every agent may discover it; whether a call *runs* is the policy engine's
/// job through the normal executor gate. Descriptive of verification, never
/// an authorization input — no policy path reads verification results.
pub struct VerifyRunTool {
    service: Arc<VerifierService>,
}

impl VerifyRunTool {
    /// Attach to an existing service (registration + tests).
    pub fn with_service(service: Arc<VerifierService>) -> Self {
        Self { service }
    }

    /// Parse a raw tool input into a [`VerifyRequest`]. The project root
    /// comes from the session, never from the input. The overlay flag
    /// resolves through the service (live state plus the fail-safe default),
    /// so an omitted flag never claims clean-disk state.
    fn parse_request(
        &self,
        input: &VerifyRunInput,
        session: &SessionContext,
    ) -> Result<VerifyRequest, ToolError> {
        let scope = match input.scope.as_deref().map(str::trim) {
            None | Some("") | Some("project") => VerifyScope::Project,
            Some(raw) => {
                // Lexical `starts_with` is blind to `..`: reject parent
                // components up front so a relative scope can never name a
                // path outside the project root.
                if raw.split(['/', '\\']).any(|segment| segment == "..") {
                    return Err(ToolError::ExecutionFailed {
                        message: format!(
                            "invalid verify.run scope '{raw}': path escapes the project root"
                        ),
                    });
                }
                let candidate = Utf8PathBuf::from(raw);
                let resolved = if candidate.is_absolute() {
                    PathBuf::from(candidate.as_str())
                } else {
                    session.project_dir.join(candidate.as_std_path())
                };
                if !resolved.starts_with(&session.project_dir) {
                    return Err(ToolError::ExecutionFailed {
                        message: format!(
                            "invalid verify.run scope '{raw}': path escapes the project root"
                        ),
                    });
                }
                VerifyScope::File(candidate)
            }
        };
        let selector = match input.verifier.as_deref().map(str::trim) {
            None | Some("") => VerifySelector::All,
            Some(raw) => VerifySelector::parse_override(raw).map_err(|message| {
                ToolError::ExecutionFailed {
                    message: format!("invalid verify.run input: {message}"),
                }
            })?,
        };
        let timeout_ms =
            input.timeout_ms.unwrap_or(VERIFY_DEFAULT_TIMEOUT_MS).clamp(1, VERIFY_MAX_TIMEOUT_MS);
        Ok(VerifyRequest {
            scope,
            selector,
            timeout: Duration::from_millis(timeout_ms),
            overlay_pending: self.service.effective_overlay_pending(input.overlay_pending),
        })
    }
}

#[async_trait]
impl concerto_core::traits::tool::Tool for VerifyRunTool {
    fn name(&self) -> &str {
        VERIFY_RUN_TOOL_NAME
    }

    fn description(&self) -> &str {
        "Run verification checks (project tests, build/syntax, LSP diagnostics, existence checks) \
         against the current workspace state and return bounded evidence records. Descriptive only: \
         a passing record proves only its adapter's declared assertion (see adapters[].proves) and \
         never authorizes further work. Materialize any overlay state before invoking: pass \
         overlay_pending:false only when no uncommitted overlay state exists (an omitted flag is \
         treated as pending, never as clean-disk)."
    }

    fn input_schema(&self) -> serde_json::Value {
        let root = schemars::schema_for!(VerifyRunInput);
        serde_json::to_value(&root).unwrap_or_else(|error| {
            tracing::error!(%error, "failed to serialize VerifyRunInput schema");
            serde_json::json!({
                "type": "object",
                "properties": {
                    "scope": { "type": ["string", "null"] },
                    "verifier": { "type": ["string", "null"] },
                    "timeout_ms": { "type": ["integer", "null"] },
                    "overlay_pending": { "type": ["boolean", "null"] }
                }
            })
        })
    }

    fn capability_requirements(&self) -> CapabilitySet {
        CapabilitySet::default()
    }

    fn path_facts(
        &self,
        input: &serde_json::Value,
        _session: &SessionContext,
    ) -> Option<PathPolicyFacts> {
        // File-scope runs name their target for the audit trail (the
        // `environment.describe` precedent names nothing; verification names
        // its scope). Project-scope runs name no path.
        let scope = input.get("scope").and_then(serde_json::Value::as_str)?;
        if scope.trim().is_empty() || scope.trim() == "project" {
            return None;
        }
        Some(PathPolicyFacts {
            operation: VERIFY_RUN_TOOL_NAME.to_string(),
            attempted_path: Some(scope.to_string()),
            ..PathPolicyFacts::default()
        })
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        _policy: &dyn concerto_core::traits::PolicyEngine,
        session: &SessionContext,
        cancel: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        if cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        let parsed: VerifyRunInput = serde_json::from_value(input).map_err(|error| {
            ToolError::ExecutionFailed { message: format!("invalid verify.run input: {error}") }
        })?;
        let request = self.parse_request(&parsed, session)?;
        let records = match self.service.verify(&request, &cancel).await {
            Ok(records) => records,
            Err(VerifyError::Cancelled) => return Err(ToolError::Cancelled),
            Err(VerifyError::Denied(detail)) => {
                return Err(ToolError::ExecutionFailed {
                    message: format!(
                        "policy denied: {detail} (verify.run sub-spawn denied; no ungoverned fallback)"
                    ),
                });
            }
            Err(VerifyError::Timeout(message) | VerifyError::InvalidInput(message)) => {
                return Err(ToolError::ExecutionFailed { message });
            }
        };
        // The service re-captures the workspace base during the run; read the
        // generation and state for the envelope AFTER the run so they describe
        // the checked bytes, not the pre-run estimate.
        let generation = self.service.current_generation();
        let checked_state = self.service.checked_state_for(&request, &generation);
        let passed = records
            .iter()
            .filter(|record| matches!(record.outcome, Some(VerificationOutcome::Passed)))
            .count();
        let failed = records
            .iter()
            .filter(|record| {
                matches!(
                    record.outcome,
                    Some(VerificationOutcome::Failed) | Some(VerificationOutcome::Error)
                )
            })
            .count();
        let summary = format!(
            "verify.run scope={} verifier={} records={} passed={passed} failed={failed} checked_state=[{checked_state}]",
            request.scope.key(),
            request.selector.as_str(),
            records.len(),
        );
        let data = serde_json::json!({
            "scope": request.scope.key(),
            "verifier": request.selector.as_str(),
            "generation": self.service.current_generation(),
            "checked_state": checked_state,
            "overlay_materialized": !request.overlay_pending,
            "records": records,
            "total_records": records.len(),
            "truncated_records": false,
            "adapters": adapter_proves_table(),
            "honesty": VERIFY_HONESTY_NOTE,
        });
        Ok(ToolOutput { summary, data })
    }
}

/// Build the per-run service and register the operation into `registry`.
///
/// Call after plugin/MCP registration but BEFORE `environment.describe`
/// registration so the describe tool-names snapshot covers `verify.run`.
/// Returns the shared handle for the run wiring (the run upgrades the
/// harness engine to the governed one once the executor exists).
///
/// `overlay_source` feeds the envelope's overlay-pending flag (live overlay
/// state when reachable); `None` means unknown, and an omitted input flag
/// then reports pending instead of claiming clean-disk state.
pub fn register_verify_tool(
    registry: &mut ToolRegistry,
    project_root: &Path,
    eval: EvalEngine,
    overlay_source: Option<OverlayPendingSource>,
) -> Arc<VerifierService> {
    let service = Arc::new(VerifierService::new(project_root.to_path_buf(), eval));
    if let Some(source) = overlay_source {
        service.set_overlay_source(source);
    }
    registry.register(Box::new(VerifyRunTool::with_service(service.clone())));
    service
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use concerto_core::executor::ToolExecutor;
    use concerto_core::ids::Ulid;
    use concerto_core::policy::SimplePolicyEngine;
    use concerto_core::traits::policy::{AuditEntry, AuditLog};
    use concerto_core::traits::tool::Tool;
    use concerto_core::types::{Condition, EvalProvenance, PolicyRule, TestRunner};
    use std::sync::atomic::AtomicU64;
    use std::sync::Mutex as StdMutex;

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

    #[derive(Default)]
    struct RecordingAudit {
        entries: StdMutex<Vec<AuditEntry>>,
    }

    #[async_trait]
    impl AuditLog for RecordingAudit {
        async fn record(
            &self,
            entry: AuditEntry,
            _cancel: CancellationToken,
        ) -> Result<(), concerto_core::error::PolicyError> {
            self.entries.lock().unwrap_or_else(|error| error.into_inner()).push(entry);
            Ok(())
        }
    }

    fn test_session(project_dir: PathBuf) -> SessionContext {
        SessionContext::new(Ulid::new(), project_dir)
    }

    /// Stub spawner: canned build outputs, a canned harness result, canned
    /// diagnostics — no real processes, with invocation counters behind the
    /// public [`CountingSpawner`] seam.
    struct StubSpawner {
        build_output: StdMutex<SpawnOutput>,
        build_error: StdMutex<Option<SpawnError>>,
        eval_result: StdMutex<Option<EvalResult>>,
        eval_error: StdMutex<Option<String>>,
        diagnostics: StdMutex<Vec<serde_json::Value>>,
        /// Whether the stub reports the file as analyzed in this session
        /// (`true` preserves the passing-LSP shape; `false` exercises the
        /// never-analyzed `Unavailable` record).
        analyzed: bool,
        hang_ms: AtomicU64,
    }

    impl StubSpawner {
        fn passing() -> Self {
            Self {
                build_output: StdMutex::new(SpawnOutput {
                    success: true,
                    stdout: String::new(),
                    stderr: String::new(),
                    code: Some(0),
                }),
                build_error: StdMutex::new(None),
                eval_result: StdMutex::new(Some(EvalResult {
                    runner: TestRunner::Cargo,
                    exit_code: 0,
                    passed: true,
                    duration_ms: 3,
                    output_tail: "test result: ok".to_string(),
                    coverage: None,
                    provenance: EvalProvenance::Harness,
                })),
                eval_error: StdMutex::new(None),
                diagnostics: StdMutex::new(Vec::new()),
                analyzed: true,
                hang_ms: AtomicU64::new(0),
            }
        }

        fn hang(ms: u64) -> Self {
            let stub = Self::passing();
            stub.hang_ms.store(ms, Ordering::SeqCst);
            stub
        }
    }

    #[async_trait]
    impl VerifySpawner for StubSpawner {
        async fn spawn(
            &self,
            _program: &str,
            _args: &[&str],
            _cwd: &Path,
            _timeout: Duration,
            cancel: &CancellationToken,
        ) -> Result<SpawnOutput, SpawnError> {
            let hang = self.hang_ms.load(Ordering::SeqCst);
            if hang > 0 {
                tokio::select! {
                    () = cancel.cancelled() => return Err(SpawnError::Cancelled),
                    () = tokio::time::sleep(Duration::from_millis(hang)) => {}
                }
            }
            if cancel.is_cancelled() {
                return Err(SpawnError::Cancelled);
            }
            if let Some(error) = self.build_error.lock().unwrap_or_else(|e| e.into_inner()).clone()
            {
                return Err(error);
            }
            Ok(self.build_output.lock().unwrap_or_else(|e| e.into_inner()).clone())
        }

        async fn run_eval(
            &self,
            _eval: &EvalEngine,
            _dir: &Path,
            _files: &[Utf8PathBuf],
            cancel: &CancellationToken,
        ) -> Result<EvalResult, EvalError> {
            let hang = self.hang_ms.load(Ordering::SeqCst);
            if hang > 0 {
                tokio::select! {
                    () = cancel.cancelled() => return Err(EvalError::Cancelled),
                    () = tokio::time::sleep(Duration::from_millis(hang)) => {}
                }
            }
            if cancel.is_cancelled() {
                return Err(EvalError::Cancelled);
            }
            if let Some(message) = self.eval_error.lock().unwrap_or_else(|e| e.into_inner()).clone()
            {
                return Err(EvalError::TestRunnerFailed(message));
            }
            self.eval_result
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
                .ok_or_else(|| EvalError::HarnessSetupFailed("stub has no result".to_string()))
        }

        async fn lsp_diagnostics(
            &self,
            _project_root: &Path,
            _file: &Path,
            cancel: &CancellationToken,
        ) -> Result<LspDiagnostics, LspQueryError> {
            if cancel.is_cancelled() {
                return Err(LspQueryError::Cancelled);
            }
            Ok(LspDiagnostics {
                diagnostics: self.diagnostics.lock().unwrap_or_else(|e| e.into_inner()).clone(),
                analyzed: self.analyzed,
            })
        }
    }

    fn stub_service(dir: &Path, stub: StubSpawner) -> (Arc<VerifierService>, Arc<CountingSpawner>) {
        let stub = Arc::new(stub);
        let counting = Arc::new(CountingSpawner::new(stub));
        let service = Arc::new(VerifierService::with_spawner(
            dir.to_path_buf(),
            EvalEngine::new(dir),
            counting.clone(),
        ));
        (service, counting)
    }

    fn write_fixture(dir: &Path, rel: &str, content: &str) {
        let full = dir.join(rel);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).expect("fixture parent created");
        }
        std::fs::write(&full, content).expect("fixture written");
    }

    /// H05 acceptance walkthrough: edit a fixture → verify → failure evidence
    /// → correct the fixture → fresh passing result → mutate again →
    /// invalidated.
    #[tokio::test]
    async fn acceptance_edit_verify_fix_pass_then_invalidated() {
        let dir = tempfile::tempdir().expect("tempdir created");
        // Broken fixture: references a stylesheet that does not exist.
        write_fixture(
            dir.path(),
            "index.html",
            "<html><head><link href=\"missing.css\"></head><body>hi</body></html>\n",
        );
        let (service, _counting) = stub_service(dir.path(), StubSpawner::passing());
        let cancel = CancellationToken::new();

        // 1. Edit happened (note the audited write), then verify: failure
        //    evidence, explicitly existence-labeled.
        service.note_write(&["index.html".to_string()], Some("evt-broken"));
        let request = VerifyRequest {
            scope: VerifyScope::File(Utf8PathBuf::from("index.html")),
            selector: VerifySelector::Existence,
            timeout: Duration::from_secs(30),
            overlay_pending: false,
        };
        let records = service.verify(&request, &cancel).await.expect("verify runs");
        assert_eq!(records.len(), 1);
        let failure = &records[0];
        assert_eq!(failure.outcome, Some(VerificationOutcome::Failed));
        assert_eq!(failure.verifier, VERIFY_EXISTENCE_VERIFIER);
        assert!(
            failure.output.contains("existence-only"),
            "existence failures say what they prove: {}",
            failure.output
        );
        assert!(!failure.is_passing_evidence(), "a failure is never passing evidence");

        // 2. Correct the fixture through the audited path, then verify: a
        //    fresh passing result bound to the current bytes.
        write_fixture(dir.path(), "missing.css", "body { color: black; }\n");
        service.note_write(&["index.html".to_string()], Some("evt-fixed"));
        let records = service.verify(&request, &cancel).await.expect("verify runs");
        assert_eq!(records.len(), 1);
        let passing = &records[0];
        assert_eq!(passing.outcome, Some(VerificationOutcome::Passed));
        assert_eq!(passing.verifier, VERIFY_EXISTENCE_VERIFIER);
        service
            .check_freshness(passing, &cancel)
            .await
            .expect("the corrected check is fresh passing evidence");

        // 3. A subsequent relevant mutation invalidates that evidence.
        write_fixture(
            dir.path(),
            "index.html",
            "<html><head><link href=\"other-missing.css\"></head><body>hi</body></html>\n",
        );
        let cause = service
            .check_freshness(passing, &cancel)
            .await
            .expect_err("the mutation must invalidate the prior evidence");
        assert!(
            matches!(cause, FreshnessFailure::Generation | FreshnessFailure::Fingerprint),
            "invalidation names its cause, got: {}",
            cause.as_str(),
        );
    }

    /// Every adapter is labeled with what it actually proves: verifier
    /// identity on the record, the proves text on the record or envelope.
    #[tokio::test]
    async fn adapters_carry_honest_labels() {
        let dir = tempfile::tempdir().expect("tempdir created");
        write_fixture(dir.path(), "Cargo.toml", "[package]\nname = \"demo\"\n");
        write_fixture(dir.path(), "a.py", "x = 1\n");
        write_fixture(dir.path(), "page.html", "<html><body>plain</body></html>\n");
        let (service, _counting) = stub_service(dir.path(), StubSpawner::passing());
        let cancel = CancellationToken::new();

        // Project tests: harness verifier + Harness|Fallback provenance.
        let eval_records = service
            .verify(
                &VerifyRequest {
                    scope: VerifyScope::Project,
                    selector: VerifySelector::ProjectTests,
                    timeout: Duration::from_secs(30),
                    overlay_pending: false,
                },
                &cancel,
            )
            .await
            .expect("eval runs");
        assert_eq!(eval_records.len(), 1);
        assert_eq!(eval_records[0].verifier, EVAL_HARNESS_VERIFIER);
        assert_eq!(eval_records[0].command, "eval");
        let provenance = eval_records[0]
            .detail
            .as_ref()
            .map(|detail| detail.provenance)
            .expect("harness detail travels");
        assert_eq!(provenance, EvalProvenance::Harness);

        // Build check: sweep verifier + proves text on the record.
        let build_records = service
            .verify(
                &VerifyRequest {
                    scope: VerifyScope::File(Utf8PathBuf::from("a.py")),
                    selector: VerifySelector::BuildCheck,
                    timeout: Duration::from_secs(30),
                    overlay_pending: false,
                },
                &cancel,
            )
            .await
            .expect("build runs");
        assert_eq!(build_records.len(), 1);
        assert_eq!(build_records[0].verifier, VERIFY_SWEEP_VERIFIER);
        assert!(
            build_records[0].output.contains("does not establish behavioral correctness"),
            "build records name their limit: {}",
            build_records[0].output
        );

        // LSP: own verifier, editor-diagnostics-only label.
        let lsp_records = service
            .verify(
                &VerifyRequest {
                    scope: VerifyScope::File(Utf8PathBuf::from("a.py")),
                    selector: VerifySelector::LspDiagnostics,
                    timeout: Duration::from_secs(30),
                    overlay_pending: false,
                },
                &cancel,
            )
            .await
            .expect("lsp runs");
        assert_eq!(lsp_records.len(), 1);
        assert_eq!(lsp_records[0].verifier, VERIFY_LSP_VERIFIER);
        assert_eq!(lsp_records[0].command, "lsp diagnostics");
        assert!(lsp_records[0].output.contains("editor diagnostics only"));

        // Existence: own verifier, existence-only label.
        let existence_records = service
            .verify(
                &VerifyRequest {
                    scope: VerifyScope::File(Utf8PathBuf::from("page.html")),
                    selector: VerifySelector::Existence,
                    timeout: Duration::from_secs(30),
                    overlay_pending: false,
                },
                &cancel,
            )
            .await
            .expect("existence runs");
        assert_eq!(existence_records.len(), 1);
        assert_eq!(existence_records[0].verifier, VERIFY_EXISTENCE_VERIFIER);
        assert!(existence_records[0].output.contains("existence-only"));
        assert!(existence_records[0].output.contains("does not establish page rendering"));
    }

    /// Coalescing: two identical project requests share one `cargo check`
    /// spawn and one harness run; per-file requests share the build spawn
    /// while keeping per-file records.
    #[tokio::test]
    async fn project_wide_spawns_coalesce_on_generation() {
        let dir = tempfile::tempdir().expect("tempdir created");
        write_fixture(dir.path(), "Cargo.toml", "[package]\nname = \"demo\"\n");
        write_fixture(dir.path(), "src/a.rs", "pub fn a() {}\n");
        write_fixture(dir.path(), "src/b.rs", "pub fn b() {}\n");
        let (service, counting) = stub_service(dir.path(), StubSpawner::passing());
        let cancel = CancellationToken::new();
        let project = VerifyRequest::project();

        let first = service.verify(&project, &cancel).await.expect("first runs");
        assert!(!first.is_empty());
        let second = service.verify(&project, &cancel).await.expect("second runs");
        assert_eq!(second.len(), first.len(), "coalesced rerun returns the same shape");
        assert_eq!(
            counting.spawn_count(),
            1,
            "two identical project requests share one build spawn"
        );
        assert_eq!(
            counting.eval_count(),
            1,
            "two identical project requests share one harness run"
        );

        // Per-file requests re-scope to the file (own record) but still share
        // the one workspace-wide `cargo check` spawn for this generation.
        for rel in ["src/a.rs", "src/b.rs"] {
            let records = service
                .verify(
                    &VerifyRequest {
                        scope: VerifyScope::File(Utf8PathBuf::from(rel)),
                        selector: VerifySelector::BuildCheck,
                        timeout: Duration::from_secs(30),
                        overlay_pending: false,
                    },
                    &cancel,
                )
                .await
                .expect("file check runs");
            assert_eq!(records.len(), 1);
            assert_eq!(records[0].path.as_str(), rel, "the record names its own file");
        }
        assert_eq!(
            counting.spawn_count(),
            1,
            "both file checks reused the generation's single cargo check spawn"
        );
    }

    /// Timeout and cancellation are honored: a hung adapter yields a bounded
    /// timeout record, a cancelled token yields `Cancelled`.
    #[tokio::test]
    async fn timeout_and_cancellation_are_honored() {
        let dir = tempfile::tempdir().expect("tempdir created");
        write_fixture(dir.path(), "Cargo.toml", "[package]\nname = \"demo\"\n");
        write_fixture(dir.path(), "src/a.rs", "pub fn a() {}\n");

        // Hung spawner + tiny budget: exactly one bounded timeout record.
        let (service, _counting) = stub_service(dir.path(), StubSpawner::hang(30_000));
        let timeout_request = VerifyRequest {
            scope: VerifyScope::Project,
            selector: VerifySelector::BuildCheck,
            timeout: Duration::from_millis(50),
            overlay_pending: false,
        };
        let records = service
            .verify(&timeout_request, &CancellationToken::new())
            .await
            .expect("timeout is a record");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].outcome, Some(VerificationOutcome::Error));
        assert!(records[0].output.contains("timed out"), "{}", records[0].output);

        // Pre-cancelled token: the run refuses before any adapter.
        let (live, counting) = stub_service(dir.path(), StubSpawner::passing());
        let cancelled = CancellationToken::new();
        cancelled.cancel();
        let error = live
            .verify(&VerifyRequest::project(), &cancelled)
            .await
            .expect_err("cancelled verify returns Cancelled");
        assert_eq!(error, VerifyError::Cancelled);
        assert_eq!(counting.spawn_count(), 0, "a cancelled run spawns nothing");
        assert_eq!(counting.eval_count(), 0, "a cancelled run runs no harness");
    }

    /// Output is bounded: long adapter output is tailed, record counts are
    /// capped, and the timeout record fits the same bounds.
    #[tokio::test]
    async fn output_is_bounded() {
        let dir = tempfile::tempdir().expect("tempdir created");
        for index in 0..(VERIFY_MAX_RECORDS + 10) {
            write_fixture(
                dir.path(),
                &format!("page-{index:03}.html"),
                "<html><body>plain</body></html>\n",
            );
        }
        let stub = StubSpawner::passing();
        *stub.build_output.lock().unwrap_or_else(|e| e.into_inner()) = SpawnOutput {
            success: false,
            stdout: "x".repeat(VERIFY_OUTPUT_TAIL_LIMIT + 500),
            stderr: String::new(),
            code: Some(1),
        };
        let (service, _counting) = stub_service(dir.path(), stub);
        let cancel = CancellationToken::new();

        // File-scope failure carries a tailed (bounded) output.
        write_fixture(dir.path(), "Cargo.toml", "[package]\nname = \"demo\"\n");
        write_fixture(dir.path(), "src/a.rs", "pub fn a() {}\n");
        let records = service
            .verify(
                &VerifyRequest {
                    scope: VerifyScope::File(Utf8PathBuf::from("src/a.rs")),
                    selector: VerifySelector::BuildCheck,
                    timeout: Duration::from_secs(30),
                    overlay_pending: false,
                },
                &cancel,
            )
            .await
            .expect("build runs");
        assert_eq!(records.len(), 1);
        assert!(
            records[0].output.chars().count() <= VERIFY_OUTPUT_TAIL_LIMIT + 32,
            "failure output is tailed, got {} chars",
            records[0].output.chars().count()
        );

        // Project-scope existence over many files is capped.
        let records = service
            .verify(
                &VerifyRequest {
                    scope: VerifyScope::Project,
                    selector: VerifySelector::Existence,
                    timeout: Duration::from_secs(30),
                    overlay_pending: false,
                },
                &cancel,
            )
            .await
            .expect("existence runs");
        assert!(records.len() <= VERIFY_MAX_RECORDS, "envelope is capped, got {}", records.len());
    }

    /// Materialization honesty: an overlay-pending run says so on the
    /// envelope instead of claiming clean-disk state.
    #[tokio::test]
    async fn materialization_honesty_labels_overlay_pending() {
        let dir = tempfile::tempdir().expect("tempdir created");
        write_fixture(dir.path(), "page.html", "<html><body>plain</body></html>\n");
        let (service, _counting) = stub_service(dir.path(), StubSpawner::passing());

        let pending = VerifyRequest {
            scope: VerifyScope::File(Utf8PathBuf::from("page.html")),
            selector: VerifySelector::Existence,
            timeout: Duration::from_secs(30),
            overlay_pending: true,
        };
        let generation = service.current_generation();
        let state = service.checked_state_for(&pending, &generation);
        assert!(state.contains("overlay-pending"), "pending overlay is labeled: {state}");
        assert!(!state.starts_with("disk"), "a pending run never claims clean-disk state: {state}");

        let clean_request = VerifyRequest {
            scope: VerifyScope::File(Utf8PathBuf::from("page.html")),
            selector: VerifySelector::Existence,
            timeout: Duration::from_secs(30),
            overlay_pending: false,
        };
        let state = service.checked_state_for(&clean_request, &generation);
        assert!(state.starts_with("disk"), "a clean run claims disk bytes: {state}");
    }

    /// `verify.run` travels the normal executor path: policy verdict row +
    /// post-execution completion row, with the bounded envelope.
    #[tokio::test]
    async fn tool_travels_the_executor_path_with_audit_rows() {
        let dir = tempfile::tempdir().expect("tempdir created");
        write_fixture(dir.path(), "page.html", "<html><body>plain</body></html>\n");
        let (service, _counting) = stub_service(dir.path(), StubSpawner::passing());
        let mut registry = ToolRegistry::default();
        registry.register(Box::new(VerifyRunTool::with_service(service)));
        let audit = Arc::new(RecordingAudit::default());
        let policy = Arc::new(SimplePolicyEngine::new(
            vec![PolicyRule::AutoApprove(Condition::Always)],
            audit.clone(),
        ));
        let executor = ToolExecutor::new(Arc::new(registry), policy);

        let output = executor
            .execute(
                VERIFY_RUN_TOOL_NAME,
                serde_json::json!({"scope": "page.html", "verifier": "existence"}),
                &test_session(dir.path().to_path_buf()),
                CancellationToken::new(),
            )
            .await
            .expect("verify.run executes through the normal path");

        assert_eq!(output.data["scope"], "file:page.html");
        assert_eq!(output.data["verifier"], "existence");
        assert!(output.data["records"].as_array().is_some_and(|records| !records.is_empty()));
        assert!(
            output.data["adapters"].as_array().is_some_and(|adapters| adapters.len() == 4),
            "the envelope carries the full adapter honesty table"
        );
        assert!(output.data["honesty"].as_str().is_some_and(|note| !note.is_empty()));
        assert!(output.summary.contains("verify.run"));

        let entries = audit.entries.lock().unwrap_or_else(|error| error.into_inner());
        assert_eq!(entries.len(), 2, "policy row + completion row: {entries:?}");
        assert_eq!(entries[0].verdict, "Allow");
        assert_eq!(entries[0].tool_name, VERIFY_RUN_TOOL_NAME);
        assert_eq!(entries[1].verdict, "ExecutionSucceeded");
    }

    /// Unknown verifier overrides and escaping scopes are bounded input
    /// errors, never spawns.
    #[tokio::test]
    async fn tool_rejects_unknown_verifier_and_escaping_scope() {
        let dir = tempfile::tempdir().expect("tempdir created");
        let (service, counting) = stub_service(dir.path(), StubSpawner::passing());
        let tool = VerifyRunTool::with_service(service);
        let policy = SimplePolicyEngine::new(vec![], Arc::new(TestAudit));
        let session = test_session(dir.path().to_path_buf());

        let error = tool
            .execute(
                serde_json::json!({"verifier": "no-such-adapter"}),
                &policy,
                &session,
                CancellationToken::new(),
            )
            .await
            .expect_err("unknown verifier is rejected");
        assert!(matches!(error, ToolError::ExecutionFailed { .. }));

        let error = tool
            .execute(
                serde_json::json!({"scope": "../outside.txt"}),
                &policy,
                &session,
                CancellationToken::new(),
            )
            .await
            .expect_err("escaping scope is rejected");
        assert!(matches!(error, ToolError::ExecutionFailed { .. }));
        assert_eq!(counting.spawn_count(), 0, "rejected inputs spawn nothing");
        assert_eq!(counting.eval_count(), 0, "rejected inputs run no harness");
    }

    /// Empty input defaults to a project-wide, all-adapter run and
    /// round-trips (R6 wire discipline).
    #[test]
    fn empty_input_defaults_and_round_trips() {
        let parsed: VerifyRunInput =
            serde_json::from_value(serde_json::json!({})).expect("empty input parses");
        assert_eq!(parsed.scope, None);
        assert_eq!(parsed.verifier, None);
        assert_eq!(parsed.timeout_ms, None);
        assert_eq!(parsed.overlay_pending, None);
        let future = serde_json::json!({"scope": "project", "whatever_next": 1});
        let _tolerant: VerifyRunInput =
            serde_json::from_value(future).expect("unknown fields ignored");
        assert_eq!(
            VerifySelector::parse_override("eval").expect("eval alias"),
            VerifySelector::ProjectTests
        );
        assert!(VerifySelector::parse_override("nope").is_err());
    }

    /// System spawner honors cancellation before spawning (no process is
    /// launched for a dead run).
    #[tokio::test]
    async fn system_spawner_refuses_cancelled_spawn() {
        let spawner = SystemSpawner::new();
        let cancelled = CancellationToken::new();
        cancelled.cancel();
        let error = spawner
            .spawn("cargo", &["check"], Path::new("/tmp"), Duration::from_secs(5), &cancelled)
            .await
            .expect_err("cancelled spawn fails without launching");
        assert!(matches!(error, SpawnError::Cancelled));
    }

    /// ADR-62 (Blocker 1): a denying governed executor refuses the spawn and
    /// the adapters report a denial record — never a silent pass, never an
    /// ungoverned retry. The executor observes the adapter-chosen argv (the
    /// caller never chooses the command).
    struct DenyingExecutor {
        calls: AtomicUsize,
    }

    #[async_trait]
    impl concerto_eval::EvalProcessExecutor for DenyingExecutor {
        async fn execute(
            &self,
            program: &str,
            args: &[&str],
            _cwd: &Path,
            _cancel: CancellationToken,
        ) -> Result<ToolOutput, EvalError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            // The caller never chooses the command: adapters pick it, and the
            // governed path sees exactly what the sweep would have spawned.
            assert_eq!(program, "cargo");
            assert_eq!(args, &["check"]);
            Err(EvalError::Denied("cargo denied by policy".to_string()))
        }
    }

    #[tokio::test]
    async fn governed_spawn_denial_reports_denied_record() {
        let dir = tempfile::tempdir().expect("tempdir created");
        write_fixture(dir.path(), "Cargo.toml", "[package]\nname = \"demo\"\n");
        write_fixture(dir.path(), "src/a.rs", "pub fn a() {}\n");
        let denying = Arc::new(DenyingExecutor { calls: AtomicUsize::new(0) });
        let engine = EvalEngine::new(dir.path())
            .with_process_executor(denying.clone() as Arc<dyn concerto_eval::EvalProcessExecutor>);
        let service = Arc::new(VerifierService::with_spawner(
            dir.path().to_path_buf(),
            engine,
            Arc::new(SystemSpawner::new()),
        ));
        let cancel = CancellationToken::new();

        // File-scope build check: one denial record, typed Error.
        let records = service
            .verify(
                &VerifyRequest {
                    scope: VerifyScope::File(Utf8PathBuf::from("src/a.rs")),
                    selector: VerifySelector::BuildCheck,
                    timeout: Duration::from_secs(30),
                    overlay_pending: false,
                },
                &cancel,
            )
            .await
            .expect("denial is a record");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].outcome, Some(VerificationOutcome::Error));
        assert!(
            records[0].output.contains("policy denied"),
            "denial is reported honestly: {}",
            records[0].output
        );
        assert!(!records[0].is_passing_evidence(), "a denied check is never passing evidence");
        assert_eq!(
            denying.calls.load(Ordering::SeqCst),
            1,
            "the governed path ran exactly once (no silent ungoverned retry)"
        );

        // Project-scope build check: the same denial through the coalesced
        // path (a denial record for the adapter, not an aborted run).
        let records = service
            .verify(
                &VerifyRequest {
                    scope: VerifyScope::Project,
                    selector: VerifySelector::BuildCheck,
                    timeout: Duration::from_secs(30),
                    overlay_pending: false,
                },
                &cancel,
            )
            .await
            .expect("project denial is a record");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].outcome, Some(VerificationOutcome::Error));
        assert!(records[0].output.contains("policy denied"), "{}", records[0].output);
        assert_eq!(
            denying.calls.load(Ordering::SeqCst),
            2,
            "file and project scopes each consult the governed path once"
        );
    }

    /// Blocker 2: an omitted overlay flag never claims clean-disk state, and
    /// live pending state overrides an explicit clean claim.
    #[tokio::test]
    async fn overlay_pending_omitted_is_fail_safe_and_live_state_wins() {
        let dir = tempfile::tempdir().expect("tempdir created");
        write_fixture(dir.path(), "page.html", "<html><body>plain</body></html>\n");
        let policy = SimplePolicyEngine::new(vec![], Arc::new(TestAudit));
        let session = test_session(dir.path().to_path_buf());

        // No reachable overlay source + omitted flag: pending (fail-safe).
        let (service, _counting) = stub_service(dir.path(), StubSpawner::passing());
        let tool = VerifyRunTool::with_service(service);
        let output = tool
            .execute(
                serde_json::json!({"scope": "page.html", "verifier": "existence"}),
                &policy,
                &session,
                CancellationToken::new(),
            )
            .await
            .expect("verify.run executes");
        assert!(
            output.data["checked_state"]
                .as_str()
                .is_some_and(|state| state.contains("overlay-pending")),
            "omitted flag with unreachable overlay reports pending: {}",
            output.data["checked_state"]
        );
        assert_eq!(output.data["overlay_materialized"].as_bool(), Some(false));

        // Live pending state overrides an explicit clean claim.
        let (service, _counting) = stub_service(dir.path(), StubSpawner::passing());
        service.set_overlay_source(Arc::new(|| true));
        let tool = VerifyRunTool::with_service(service);
        let output = tool
            .execute(
                serde_json::json!({"scope": "page.html", "verifier": "existence", "overlay_pending": false}),
                &policy,
                &session,
                CancellationToken::new(),
            )
            .await
            .expect("verify.run executes");
        assert!(
            output.data["checked_state"]
                .as_str()
                .is_some_and(|state| state.contains("overlay-pending")),
            "live pending state beats a clean claim: {}",
            output.data["checked_state"]
        );

        // Live clean state + explicit clean claim: disk bytes claimed.
        let (service, _counting) = stub_service(dir.path(), StubSpawner::passing());
        service.set_overlay_source(Arc::new(|| false));
        let tool = VerifyRunTool::with_service(service);
        let output = tool
            .execute(
                serde_json::json!({"scope": "page.html", "verifier": "existence", "overlay_pending": false}),
                &policy,
                &session,
                CancellationToken::new(),
            )
            .await
            .expect("verify.run executes");
        assert!(
            output.data["checked_state"].as_str().is_some_and(|state| state.starts_with("disk")),
            "attested clean with no live pending claims disk bytes: {}",
            output.data["checked_state"]
        );
        assert_eq!(output.data["overlay_materialized"].as_bool(), Some(true));
    }

    /// Blocker 3: `note_write` advances the service epoch, so a write after a
    /// check supersedes it positionally within the run (same base).
    #[tokio::test]
    async fn note_write_advances_service_epoch_within_run() {
        let dir = tempfile::tempdir().expect("tempdir created");
        write_fixture(dir.path(), "page.html", "<html><body>plain</body></html>\n");
        let (service, _counting) = stub_service(dir.path(), StubSpawner::passing());
        let cancel = CancellationToken::new();
        let request = VerifyRequest {
            scope: VerifyScope::File(Utf8PathBuf::from("page.html")),
            selector: VerifySelector::Existence,
            timeout: Duration::from_secs(30),
            overlay_pending: false,
        };

        service.note_write(&["page.html".to_string()], Some("evt-1"));
        let records = service.verify(&request, &cancel).await.expect("verify runs");
        assert_eq!(records.len(), 1);
        let checked = &records[0];
        assert_eq!(checked.outcome, Some(VerificationOutcome::Passed));
        let (_, epoch) =
            split_run_generation(&checked.generation).expect("the run stamps a generation");
        assert_eq!(epoch, 1, "one noted write advances the epoch: {}", checked.generation);
        service.check_freshness(checked, &cancel).await.expect("the just-run check is fresh");

        // An intervening write (no disk change, so the base is identical)
        // supersedes the earlier check positionally.
        service.note_write(&["page.html".to_string()], Some("evt-2"));
        let cause = service
            .check_freshness(checked, &cancel)
            .await
            .expect_err("the intervening write must supersede the prior check");
        assert_eq!(cause, FreshnessFailure::Superseded, "got: {}", cause.as_str());

        let records = service.verify(&request, &cancel).await.expect("verify runs");
        let (_, epoch) =
            split_run_generation(&records[0].generation).expect("the run stamps a generation");
        assert_eq!(epoch, 2, "the second noted write advances the epoch again");
    }

    /// LSP honesty: a never-analyzed file records `Unavailable` (its empty
    /// diagnostics prove nothing), never a pass.
    #[tokio::test]
    async fn lsp_never_analyzed_file_is_not_clean() {
        let dir = tempfile::tempdir().expect("tempdir created");
        write_fixture(dir.path(), "a.py", "x = 1\n");
        let mut stub = StubSpawner::passing();
        stub.analyzed = false;
        let (service, _counting) = stub_service(dir.path(), stub);
        let records = service
            .verify(
                &VerifyRequest {
                    scope: VerifyScope::File(Utf8PathBuf::from("a.py")),
                    selector: VerifySelector::LspDiagnostics,
                    timeout: Duration::from_secs(30),
                    overlay_pending: false,
                },
                &CancellationToken::new(),
            )
            .await
            .expect("lsp runs");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].outcome, Some(VerificationOutcome::Unavailable));
        assert!(
            records[0].output.contains("never analyzed"),
            "the record says the file was not analyzed: {}",
            records[0].output
        );
        assert!(!records[0].is_passing_evidence(), "an unanalyzed file is never evidence");
    }

    /// `refresh_base` drops the coalescing caches when the base moves: they
    /// are keyed by generation, so a moved base leaves stale-but-unreachable
    /// entries (bounded memory in a long-lived process).
    #[tokio::test]
    async fn base_change_drops_coalesced_caches() {
        let dir = tempfile::tempdir().expect("tempdir created");
        write_fixture(dir.path(), "Cargo.toml", "[package]\nname = \"demo\"\n");
        write_fixture(dir.path(), "src/a.rs", "pub fn a() {}\n");
        let (service, counting) = stub_service(dir.path(), StubSpawner::passing());
        let cancel = CancellationToken::new();
        let project = VerifyRequest::project();

        service.verify(&project, &cancel).await.expect("first runs");
        service.verify(&project, &cancel).await.expect("second runs");
        assert_eq!(counting.spawn_count(), 1, "same base reuses the coalesced spawn");
        assert_eq!(counting.eval_count(), 1, "same base reuses the coalesced harness run");

        // A disk change moves the content-addressed base: the next run must
        // re-invoke instead of serving the dead generation's cache.
        write_fixture(dir.path(), "src/b.rs", "pub fn b() {}\n");
        service.verify(&project, &cancel).await.expect("third runs");
        assert_eq!(counting.spawn_count(), 2, "a moved base drops the build cache");
        assert_eq!(counting.eval_count(), 2, "a moved base drops the eval cache");
    }
}
