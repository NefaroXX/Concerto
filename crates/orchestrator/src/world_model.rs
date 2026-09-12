//! The Coordinator's structured world model (issue #56, parent #51) — a
//! compact, bounded projection of what the run KNOWS about the workspace,
//! its work, and its open problems, built deterministically by a pure
//! builder from existing state only.
//!
//! NOT a new store and NOT prose summaries: the model is a projection with
//! its own small struct, rebuilt from inputs the coordinator already holds
//! (the whiteboard event window from the ADR-65 evidence spine, the
//! resource-fact rows from ADR-65 §4, the decision journal, the structured
//! failure diagnoses, the roster, the ledger's artifact paths, and the
//! checkpoint workspace-generation signals). Every fact is referenced BY ID
//! (event/blob id + a short bounded label) — full prose is never copied in.
//!
//! Freshness rules (explicit, deterministic — never per-field guesses):
//! - **F-VERIFY**: a fact derived from a successful `WriteApplied` event or
//!   a successful file-affecting `ToolExecuted` fact is `Verified` — it
//!   quotes an executed observation.
//! - **F-ASSUME**: a fact derived from a `Finding` or `DesignDoc` event (an
//!   assertion with no executed observation behind it) is `Assumed`.
//! - **F-SUPERSEDE**: a fact that names an artifact path is `Stale` when the
//!   event window holds a NEWER effective write to that path (higher
//!   `gate_seq`) than the fact's derivation event — the artifact moved on
//!   after the fact was derived.
//! - **F-GENERATION**: when the resume workspace-change verdict fired
//!   (checkpoint generation ≠ current snapshot generation), every log-
//!   derived fact drops to `Stale` until a fresh observation re-verifies it
//!   — the conservative reset (ADR-65 §7 F3).
//! - **A-DIRTY**: a resource-fact row with `dirty = true` marks its
//!   artifact `Dirty` (uncertain, never verified-clean).
//! - **A-GENERATION-ROW**: a resource-fact row whose recorded generation
//!   differs from the current snapshot generation marks its artifact
//!   `Stale`.
//! - **A-CLEAN**: a clean row whose generation matches (or with no current
//!   generation known) marks its artifact `Clean`, owned by the row writer.
//! - **A-WRITTEN**: a path known only from effective write events or the
//!   ledger's file list (no observation row) is `Written`, owned by the
//!   newest write's agent.
//! - **V-CHANGE**: while the resume workspace-change verdict stands, NO
//!   artifact is verified-clean (the deterministic
//!   `artifact_verified_clean` query demands a workspace that has not
//!   changed materially since the checkpoint).
//!
//! Unresolved-question rules (fed ONLY from existing signals):
//! - **Q-OPEN-PROBLEM**: a failure diagnosis requiring replanning or a
//!   non-retryable, unviable-retry one opens an `OpenProblem` question.
//! - **Q-OPEN-MISSING**: a journal decision stood `Rejected` opens a
//!   `MissingEvidence` question (the work needs a corrected decision).
//! - **Q-OPEN-AMBIGUOUS**: a stale pending dispatch decision opens an
//!   `AmbiguousRecovery` question.
//! - **Q-OPEN-BLOCKED**: an artifact with status `Dirty` opens a
//!   `BlockedPath` question.
//! - **Q-DEDUPE**: a rebuilt question matching a standing question by
//!   stable key does NOT open again — the standing entry survives (one id,
//!   growing age) until resolved. This is the repeated-question reduction.
//! - **Q-RESOLVE-PROBLEM/MISSING**: a journal decision with status `Settled`
//!   recorded at or after the question's opening journal length resolves
//!   the question (subsequent settled work is the recovery evidence).
//! - **Q-RESOLVE-AMBIGUOUS**: the pending dispatch decision cleared.
//! - **Q-RESOLVE-BLOCKED**: a newer CLEAN observation of the path (an
//!   observation event id different from the one the question was opened
//!   against) resolves it.
//! - **Q-RESOLVE-STAY**: a resolved question is never re-opened.
//! - **Q-PERSIST**: open questions persist across rebuild cycles until one
//!   of the Q-RESOLVE rules fires, independent of whether the opening
//!   signal still shows.
//!
//! Bounds: every list is capped and every label is length-bounded; the
//! facts reference ids, never prose. The rendered block is hard-bounded at
//! [`MAX_RENDER_CHARS`] characters with an explicit truncation mark, so the
//! prompt cost is pinned, never proportional to a long log.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use concerto_sessions::whiteboard::WhiteboardEvent;
use concerto_sessions::WhiteboardKind;

use crate::checkpoint::CheckpointPendingDecision;
use crate::decisions::{CoordinatorDecision, DecisionStatus};
use crate::failure_diagnosis::FailureDiagnosis;

/// Upper bound on referenced facts (event id + label), never prose-proportional.
pub const MAX_WORLD_FACTS: usize = 32;
/// Upper bound on tracked tasks (one journal dispatch entry each).
pub const MAX_WORLD_TASKS: usize = 24;
/// Upper bound on tracked artifact entries.
pub const MAX_WORLD_ARTIFACTS: usize = 32;
/// Upper bound on simultaneously open questions.
pub const MAX_OPEN_QUESTIONS: usize = 12;
/// Upper bound on remembered resolved questions (the evidence trail).
pub const MAX_RESOLVED_QUESTIONS: usize = 8;
/// Upper bound on assumptions and risks each.
pub const MAX_WORLD_ASSUMPTIONS: usize = 8;
pub const MAX_WORLD_RISKS: usize = 8;
/// Upper bound on roster names and seen models each.
pub const MAX_WORLD_AGENTS: usize = 16;
pub const MAX_WORLD_MODELS: usize = 8;
pub const MAX_WORLD_CRITERIA: usize = 8;
/// Every stored label is at most this many characters.
pub const MAX_LABEL_CHARS: usize = 120;
/// The objective line is bounded independently of the task text.
pub const MAX_OBJECTIVE_CHARS: usize = 200;
/// Hard character bound on the rendered prompt block (the truncation mark
/// included).
pub const MAX_RENDER_CHARS: usize = 4_000;
/// The truncation marker appended when the render is cut.
pub const RENDER_TRUNCATION_MARK: &str = "…[truncated]";
/// Effective writes per event contribute at most this many path facts.
const MAX_PATHS_PER_EVENT: usize = 4;

/// How fresh a world-model fact is (issue #56: facts vs assumptions vs
/// stale MUST be distinguishable).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FactStatus {
    /// Backed by an executed observation (F-VERIFY).
    Verified,
    /// Backed only by an assertion (F-ASSUME).
    Assumed,
    /// Invalidated by a newer write or a workspace-generation change
    /// (F-SUPERSEDE / F-GENERATION).
    Stale,
}

/// What kind of unresolved signal a question captures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum QuestionKind {
    /// A failed/non-retryable diagnosis awaiting recovery (Q-OPEN-PROBLEM).
    OpenProblem,
    /// A rejected journal decision awaiting a corrected approach
    /// (Q-OPEN-MISSING).
    MissingEvidence,
    /// A stale pending dispatch (Q-OPEN-AMBIGUOUS).
    AmbiguousRecovery,
    /// A dirty artifact awaiting a fresh observation (Q-OPEN-BLOCKED).
    BlockedPath,
}

/// Lifecycle state of a question: open until evidence resolves it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum QuestionState {
    Open,
    Resolved,
}

/// One explicit, actionable unresolved question: what is unknown, what it
/// blocks, what evidence would answer it, and how long it has stood.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UnresolvedQuestion {
    /// Stable id derived from the question key — the same underlying
    /// question keeps one id across cycles (Q-DEDUPE).
    pub id: String,
    pub kind: QuestionKind,
    /// The question text, actionable: names the subject and the evidence
    /// that answers it.
    pub question: String,
    /// The artifact path or work item the question blocks on, when one is
    /// named.
    #[serde(default)]
    pub blocks: Option<String>,
    /// The event/observation ids or evidence names that answer the question.
    #[serde(default)]
    pub needed: Vec<String>,
    /// Journal length when the question was opened — the resolution anchor
    /// for Q-RESOLVE-PROBLEM/MISSING.
    #[serde(default)]
    pub opened_journal_len: usize,
    /// The observation event id the question was opened against
    /// (BlockedPath).
    #[serde(default)]
    pub opened_ref: Option<String>,
    /// Unix ms when the question was first opened (the age anchor).
    #[serde(default)]
    pub opened_at_ms: i64,
    /// How many full rebuild cycles the question has stood open.
    #[serde(default)]
    pub cycles_open: u32,
    pub state: QuestionState,
    /// The evidence id that resolved the question.
    #[serde(default)]
    pub resolved_by: Option<String>,
}

/// An artifact's freshness class (issue #56: basic ownership — current
/// writer per artifact; the full ownership model is issue #61, out of
/// scope here).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ArtifactStatus {
    /// Known only from a write event, no contradicting observation.
    Written,
    /// Observation row clean with a matching generation (A-CLEAN).
    Clean,
    /// Observation row dirty — the workspace state is uncertain (A-DIRTY).
    Dirty,
    /// Stale by superseding write or generation change (A-GENERATION-*).
    Stale,
}

/// One tracked artifact: path + freshness + current writer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorldArtifact {
    pub path: String,
    pub status: ArtifactStatus,
    /// The current writer of the artifact (basic ownership), when known.
    #[serde(default)]
    pub owner: Option<String>,
    /// The event id the artifact's state is attributable to.
    #[serde(default)]
    pub last_ref: Option<String>,
}

/// A run's fact referenced BY ID (event id + short label — never prose).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorldFact {
    /// The derivation reference (whiteboard event id).
    pub ref_id: String,
    /// A very short label describing the fact.
    pub label: String,
    pub status: FactStatus,
    /// The artifact path the fact is about, when one is named.
    #[serde(default)]
    pub artifact: Option<String>,
    /// Whiteboard `gate_seq` of the derivation event (0 when not log-derived)
    /// — drives the F-SUPERSEDE ordering.
    #[serde(default)]
    pub seq: u64,
}

/// One tracked work item (a journal dispatch decision): id, short label,
/// status label.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorldTask {
    pub ref_id: String,
    pub label: String,
    pub status: String,
}

/// One assumption the stance rests on (derived from Assumed-fact labels).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorldAssumption {
    pub ref_id: String,
    pub label: String,
}

/// One risk / blocker carved out of existing failure + freshness signals.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorldRisk {
    pub ref_id: String,
    pub label: String,
}

/// The compact coordinator world model: a PROJECTION rebuilt from the run's
/// existing state at every refresh point. The only carried progress is the
/// question ledger (lifecycle persistence, Q-PERSIST), checkpointed
/// additively so a resume restores (and, for old checkpoints, rebuilds)
/// the same model.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct WorldModel {
    #[serde(default)]
    pub built_at_ms: i64,
    /// The workspace snapshot generation the model was built against.
    #[serde(default)]
    pub generation: Option<String>,
    /// Whether the workspace changed materially since the checkpoint
    /// (the resume F3 verdict). While set, nothing is verified-clean (the
    /// V-CHANGE rule).
    #[serde(default)]
    pub workspace_changed: bool,
    /// The run's objective, length-bounded.
    #[serde(default)]
    pub objective: Option<String>,
    /// Success criteria (DesignDoc goals when one binds), length-bounded.
    #[serde(default)]
    pub criteria: Vec<String>,
    /// The callable roster + the coordinator.
    #[serde(default)]
    pub agents: Vec<String>,
    /// Models seen in the run's assignments.
    #[serde(default)]
    pub models: Vec<String>,
    #[serde(default)]
    pub facts: Vec<WorldFact>,
    #[serde(default)]
    pub tasks: Vec<WorldTask>,
    #[serde(default)]
    pub artifacts: Vec<WorldArtifact>,
    /// Unresolved questions, open ones first then the resolved memory.
    #[serde(default)]
    pub questions: Vec<UnresolvedQuestion>,
    #[serde(default)]
    pub assumptions: Vec<WorldAssumption>,
    #[serde(default)]
    pub risks: Vec<WorldRisk>,
    /// The pending dispatch decision awaiting completion (bounded label).
    #[serde(default)]
    pub pending: Option<String>,
}

/// One per-path freshness/ownership observation handed to the builder. Two
/// shapes, distinguished by `observed`:
/// - an ADR-65 §4 resource-fact ROW (`observed = true`): it rules the
///   artifact Clean/Dirty/Stale per A-*, and is the only source that can
///   clear `Dirty`;
/// - the ledger/event's WRITE attribution for a path the run produced
///   (`observed = false`): `"dirty"` is ignored; the artifact class is
///   `Written` (issue #56: ledger/all-files paths adapt here with the last
///   writer attribution the ledger holds).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactObservation {
    pub path: String,
    /// Whether this is a real observation row (true) or write attribution
    /// only (false).
    pub observed: bool,
    pub dirty: bool,
    pub generation: Option<String>,
    pub last_agent_id: Option<String>,
    pub last_event_id: Option<String>,
}

/// The pure builder's inputs — the coordinator's existing state, nothing
/// new: the projection reads, it never invents sources.
pub struct WorldModelInput<'a> {
    pub objective: &'a str,
    /// Success criteria (DesignDoc goals when one binds).
    pub criteria: Vec<String>,
    /// The CURRENT workspace snapshot generation (may be `None`).
    pub current_generation: Option<String>,
    /// Whether the resume workspace-change verdict fired (generation
    /// mismatch between checkpoint and now).
    pub workspace_changed: bool,
    /// The bounded newest-first whiteboard event window the caller loaded.
    pub events: &'a [WhiteboardEvent],
    /// The resource-fact rows (bounded), adapted by the caller.
    pub artifacts: Vec<ArtifactObservation>,
    /// The run's decision journal so far.
    pub decisions: &'a [CoordinatorDecision],
    /// The run's structured failure diagnoses.
    pub diagnoses: &'a [FailureDiagnosis],
    /// The registry roster (callable agents).
    pub roster: Vec<String>,
    /// Models seen in the run's assignments.
    pub models: Vec<String>,
    /// The pending dispatch decision awaiting completion, if any.
    pub pending: Option<&'a CheckpointPendingDecision>,
    /// Whether that pending decision is stale (roster/evidence mismatch).
    pub pending_stale: bool,
    /// The question ledger carried from the previous model (lifecycle,
    /// Q-PERSIST).
    pub previous_questions: Vec<UnresolvedQuestion>,
    /// The build clock (injected so builds stay deterministic in tests).
    pub now_ms: i64,
}

/// An effective write extracted from the event window: the path it moved,
/// when (gate_seq), and who moved it.
struct RecordedWrite {
    path: String,
    seq: u64,
    event_id: String,
    agent: String,
}

/// A fact candidate extracted from the event window.
struct FactCandidate {
    ref_id: String,
    label: String,
    status: FactStatus,
    artifact: Option<String>,
    seq: u64,
}

/// Extract effective writes and fact candidates from the event window.
/// Unknown event kinds are opaque and skipped (already the project's
/// log-compat convention).
fn extract_writes_and_facts(
    events: &[WhiteboardEvent],
) -> (Vec<RecordedWrite>, Vec<FactCandidate>) {
    let mut writes = Vec::new();
    let mut facts = Vec::new();
    for event in events {
        match event.kind {
            WhiteboardKind::WriteApplied => {
                // The write gate's applied-write record: the written path
                // lives at payload["input"]["path"] (see the own-write
                // reconciliation in coordinator.rs for the canonical shape).
                let Some(path) = event
                    .payload
                    .get("input")
                    .and_then(|input| input.get("path"))
                    .and_then(serde_json::Value::as_str)
                else {
                    continue;
                };
                facts.push(FactCandidate {
                    ref_id: event.event_id.clone(),
                    label: bounded(format!("wrote {} by {}", path, event.agent_id)),
                    status: FactStatus::Verified,
                    artifact: Some(path.to_owned()),
                    seq: event.gate_seq,
                });
                writes.push(RecordedWrite {
                    path: path.to_owned(),
                    seq: event.gate_seq,
                    event_id: event.event_id.clone(),
                    agent: event.agent_id.clone(),
                });
            }
            WhiteboardKind::ToolExecuted => {
                let tool = event.payload.get("tool").and_then(serde_json::Value::as_str);
                let args = event.payload.get("args").cloned().unwrap_or(serde_json::Value::Null);
                let success =
                    event.payload.get("success").and_then(serde_json::Value::as_bool) == Some(true);
                if !success {
                    continue;
                }
                let paths: Vec<String> = event
                    .payload
                    .get("paths")
                    .and_then(serde_json::Value::as_array)
                    .map(|rows| {
                        rows.iter()
                            .filter_map(|row| row.get("path").and_then(serde_json::Value::as_str))
                            .map(str::to_owned)
                            .collect()
                    })
                    .unwrap_or_default();
                let file_affecting =
                    tool.is_some_and(|tool| crate::tool_facts::is_file_affecting_tool(tool, &args));
                let tool_label = tool.unwrap_or("tool");
                if file_affecting {
                    for path in paths.iter().take(MAX_PATHS_PER_EVENT) {
                        facts.push(FactCandidate {
                            ref_id: event.event_id.clone(),
                            label: bounded(format!("{tool_label} applied {path}")),
                            status: FactStatus::Verified,
                            artifact: Some(path.clone()),
                            seq: event.gate_seq,
                        });
                        writes.push(RecordedWrite {
                            path: path.clone(),
                            seq: event.gate_seq,
                            event_id: event.event_id.clone(),
                            agent: event.agent_id.clone(),
                        });
                    }
                    continue;
                }
                // A non-write execution: a verified observation WITHOUT a
                // written artifact (reads, builds, checks).
                let subject = paths.first().cloned().unwrap_or_else(|| tool_label.to_owned());
                facts.push(FactCandidate {
                    ref_id: event.event_id.clone(),
                    label: bounded(format!("ran {tool_label} ({subject})")),
                    status: FactStatus::Verified,
                    artifact: None,
                    seq: event.gate_seq,
                });
            }
            WhiteboardKind::Finding => {
                // An assertion without an executed observation: Assumed
                // (F-ASSUME), never Verified.
                let summary = event
                    .payload
                    .get("summary")
                    .or_else(|| event.payload.get("content"))
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("finding recorded");
                facts.push(FactCandidate {
                    ref_id: event.event_id.clone(),
                    label: bounded(summary.to_owned()),
                    status: FactStatus::Assumed,
                    artifact: None,
                    seq: event.gate_seq,
                });
            }
            WhiteboardKind::DesignDoc => {
                // The doc is an assertion about the intended contract
                // (Assumed at the world-model level; the ADR-65 verifier
                // chain owns the binding verdict separately).
                let paths_len = event
                    .payload
                    .get("proposed_files")
                    .and_then(serde_json::Value::as_array)
                    .map_or(0, |rows| rows.len());
                facts.push(FactCandidate {
                    ref_id: event.event_id.clone(),
                    label: bounded(format!("design doc binds {paths_len} proposed paths")),
                    status: FactStatus::Assumed,
                    artifact: None,
                    seq: event.gate_seq,
                });
            }
            _ => {}
        }
    }
    (writes, facts)
}

/// Whether the window holds an effective write to `path` newer than `seq`.
fn has_newer_write(writes: &[RecordedWrite], path: &str, seq: u64) -> bool {
    writes.iter().any(|write| write.path == path && write.seq > seq)
}

/// Stable question identity from kind + subject: the same underlying
/// question keeps the same id forever (Q-DEDUPE) and a resolved standing
/// entry blocks re-opening (Q-RESOLVE-STAY).
fn question_key(kind: QuestionKind, subject: &str) -> String {
    let raw = format!("{kind:?}:{subject}");
    let digest = blake3::hash(raw.as_bytes()).to_hex();
    format!("q-{}", &digest[..12])
}

/// The open-question overflow order under the cap: ambiguous recovery
/// first, then missing evidence, then open problems, then blocked paths
/// (dispatch recovery outranks blocking detail).
fn question_cap_rank(kind: QuestionKind) -> u8 {
    match kind {
        QuestionKind::AmbiguousRecovery => 0,
        QuestionKind::MissingEvidence => 1,
        QuestionKind::OpenProblem => 2,
        QuestionKind::BlockedPath => 3,
    }
}

/// Bound a label to [`MAX_LABEL_CHARS`] characters with a deterministic
/// truncation ellipsis.
fn bounded(text: impl Into<String>) -> String {
    let text = text.into();
    if text.chars().count() <= MAX_LABEL_CHARS {
        text
    } else {
        let mut out: String = text.chars().take(MAX_LABEL_CHARS).collect();
        out.push('…');
        out
    }
}

/// The build clock fallback for decision entries whose timestamp conversion
/// fails (`None` is rare; the caller pins the build clock instead).
fn decision_time_ms(created_at: &time::OffsetDateTime) -> Option<i64> {
    i64::try_from(created_at.unix_timestamp_nanos() / 1_000_000).ok()
}

/// Truncate a rendered string at a character boundary with the mark.
fn truncate_with_mark(text: String, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text;
    }
    let keep = max_chars.saturating_sub(RENDER_TRUNCATION_MARK.chars().count());
    let mut out: String = text.chars().take(keep).collect();
    out.push_str(RENDER_TRUNCATION_MARK);
    out
}

// The ledger's write attribution reaches the builder through the
// `ArtifactObservation` channel (`observed = false`), so the builder keeps
// ONE artifact shape; the coordinator refresh adapts its ledger paths.

impl WorldModel {
    /// The pure deterministic builder: same input → same model (and the
    /// same render). No model calls, no I/O, no randomness.
    #[must_use]
    pub fn build(input: &WorldModelInput<'_>) -> Self {
        let (writes, candidates) = extract_writes_and_facts(input.events);

        // ── Facts (capped from the NEWEST side; staleness per F-*) ──────
        let mut facts: Vec<WorldFact> = candidates
            .into_iter()
            .map(|candidate| {
                let stale = input.workspace_changed
                    // F-GENERATION: the workspace generation changed since
                    // the checkpoint — every log-derived fact is
                    // conservative-stale until a fresh observation lands.
                    ||
                    // F-SUPERSEDE: a newer effective write to the same
                    // artifact invalidates the older fact.
                    candidate.artifact.as_ref().is_some_and(|path| {
                        has_newer_write(&writes, path, candidate.seq)
                    });
                WorldFact {
                    ref_id: candidate.ref_id,
                    label: candidate.label,
                    status: if stale { FactStatus::Stale } else { candidate.status },
                    artifact: candidate.artifact,
                    seq: candidate.seq,
                }
            })
            .collect();
        facts.sort_by_key(|fact| core::cmp::Reverse(fact.seq));
        facts.truncate(MAX_WORLD_FACTS);

        // ── Artifacts (union of observation rows + write attribution) ────
        // Pass 1: real observation rows are authoritative — the only source
        // that can rule an artifact Dirty, and the only one that wins when
        // the same path also carries ledger/event write attribution.
        let mut row_paths: HashSet<String> = HashSet::new();
        let mut artifact_rows: Vec<(String, ArtifactStatus, Option<String>, Option<String>)> =
            Vec::new();
        for row in input.artifacts.iter().filter(|row| row.observed) {
            row_paths.insert(row.path.clone());
            let status = if row.dirty {
                ArtifactStatus::Dirty // A-DIRTY
            } else if let (Some(current), Some(row_generation)) =
                (input.current_generation.as_deref(), row.generation.as_deref())
            {
                if current == row_generation {
                    ArtifactStatus::Clean // A-CLEAN
                } else {
                    ArtifactStatus::Stale // A-GENERATION-ROW
                }
            } else {
                ArtifactStatus::Clean // no generation to contradict yet
            };
            artifact_rows.push((
                row.path.clone(),
                status,
                row.last_agent_id.clone(),
                row.last_event_id.clone(),
            ));
        }
        // Pass 2: write attribution only (ledger paths, `observed = false`)
        // — A-WRITTEN, owner as the ledger holds it. Paths an observation
        // row already covers are skipped (the row wins).
        let mut attribution_paths: HashSet<String> = HashSet::new();
        for row in input.artifacts.iter().filter(|row| !row.observed) {
            if row_paths.contains(&row.path) || !attribution_paths.insert(row.path.clone()) {
                continue;
            }
            artifact_rows.push((
                row.path.clone(),
                ArtifactStatus::Written,
                row.last_agent_id.clone(),
                row.last_event_id.clone(),
            ));
        }
        // Pass 3: event-window write paths not covered by a row or an
        // earlier attribution — owned by the newest write's agent (walk
        // newest-first, first hit wins).
        for (path, write) in write_latest_per_path(&writes) {
            if row_paths.contains(&path) || attribution_paths.contains(&path) {
                continue; // the observation row / ledger attribution wins
            }
            artifact_rows.push((
                path.clone(),
                ArtifactStatus::Written,
                Some(write.agent.clone()),
                Some(write.event_id.clone()),
            ));
        }
        artifact_rows.sort();
        artifact_rows.dedup_by(|a, b| a.0 == b.0);
        let artifacts: Vec<WorldArtifact> = artifact_rows
            .into_iter()
            .take(MAX_WORLD_ARTIFACTS)
            .map(|(path, status, owner, last_ref)| WorldArtifact { path, status, owner, last_ref })
            .collect();

        // ── Objective / criteria / roster / models ───────────────────────
        let objective = bounded_objective(input.objective);
        let criteria: Vec<String> =
            input.criteria.iter().map(|c| bounded(c.clone())).take(MAX_WORLD_CRITERIA).collect();
        let mut agents: Vec<String> = std::iter::once("coordinator".to_owned())
            .chain(input.roster.iter().cloned())
            .take(MAX_WORLD_AGENTS)
            .collect();
        agents.sort();
        let mut models = input.models.clone();
        models.sort();
        models.dedup();
        models.truncate(MAX_WORLD_MODELS);

        // ── Tasks (journal dispatch decisions, capped) ───────────────────
        let tasks: Vec<WorldTask> = input
            .decisions
            .iter()
            .filter(|decision| decision.kind.requires_target())
            .map(|decision| WorldTask {
                ref_id: decision.id.clone(),
                label: bounded(decision.task_description.clone()),
                status: crate::progress::decision_status_label(decision.status).to_owned(),
            })
            .take(MAX_WORLD_TASKS)
            .collect();

        // ── Assumptions (Assumed-fact labels, deduped) ─────────────────────
        let mut assumptions: Vec<WorldAssumption> = Vec::new();
        for fact in &facts {
            if fact.status != FactStatus::Assumed
                || assumptions.iter().any(|existing| existing.label == fact.label)
            {
                continue;
            }
            assumptions
                .push(WorldAssumption { ref_id: fact.ref_id.clone(), label: fact.label.clone() });
            if assumptions.len() >= MAX_WORLD_ASSUMPTIONS {
                break;
            }
        }

        // ── Risks (existing signals only, deterministic order, deduped) ──
        let mut risks: Vec<WorldRisk> = Vec::new();
        let push_risk = |ref_id: String, label: String, risks: &mut Vec<WorldRisk>| {
            if risks.iter().any(|existing| existing.label == label) {
                return;
            }
            risks.push(WorldRisk { ref_id, label });
        };
        if input.pending_stale {
            if let Some(pending) = input.pending {
                push_risk(
                    pending.selected_agent.clone(),
                    bounded(format!(
                        "pending dispatch to {} is stale — the resume must not stand behind it",
                        pending.selected_agent
                    )),
                    &mut risks,
                );
            }
        }
        for diagnosis in input.diagnoses {
            if diagnosis.replan_required {
                push_risk(
                    diagnosis.code.clone(),
                    bounded(format!("work may need replanning: {}", diagnosis.code)),
                    &mut risks,
                );
            }
            if !diagnosis.retryable
                && !diagnosis.same_agent_viable
                && !diagnosis.alternate_agent_viable
            {
                push_risk(
                    diagnosis.code.clone(),
                    bounded(format!("unrecoverable failure: {}", diagnosis.code)),
                    &mut risks,
                );
            }
        }
        // Decision-named expected artifacts that are Dirty/Stale are
        // concrete blockers for the work the model just decided.
        for decision in input.decisions {
            for path in &decision.expected_artifacts {
                let Some(entry) = artifacts.iter().find(|entry| entry.path == *path) else {
                    continue;
                };
                if !matches!(entry.status, ArtifactStatus::Dirty | ArtifactStatus::Stale) {
                    continue;
                }
                push_risk(
                    entry.last_ref.clone().unwrap_or_else(|| entry.path.clone()),
                    bounded(format!(
                        "expected artifact {} is tracked {}",
                        entry.path,
                        entry.status.status_label()
                    )),
                    &mut risks,
                );
            }
        }
        risks.truncate(MAX_WORLD_RISKS);

        // ── Pending dispatch label ────────────────────────────────────────
        let pending = input.pending.map(|pending| {
            bounded(format!(
                "dispatch to {}: {}",
                pending.selected_agent,
                bounded(pending.required_output.clone())
            ))
        });

        // ── Questions (resolve → feed → cap) ─────────────────────────────
        let questions = resolve_and_feed_questions(input);

        Self {
            built_at_ms: input.now_ms,
            generation: input.current_generation.clone(),
            workspace_changed: input.workspace_changed,
            objective,
            criteria,
            agents,
            models,
            facts,
            tasks,
            artifacts,
            questions,
            assumptions,
            risks,
            pending,
        }
    }

    /// Whether the model carries anything beyond the trivial header — the
    /// prompt block is injected only when there is content to consume.
    #[must_use]
    pub fn is_empty_beyond_objective(&self) -> bool {
        self.criteria.is_empty()
            && self.facts.is_empty()
            && self.tasks.is_empty()
            && self.artifacts.is_empty()
            && self.assumptions.is_empty()
            && self.risks.is_empty()
            && self.pending.is_none()
            && self.questions.iter().all(|question| question.state != QuestionState::Open)
    }

    /// The number of open questions.
    #[must_use]
    pub fn open_question_count(&self) -> usize {
        self.questions.iter().filter(|q| q.state == QuestionState::Open).count()
    }

    /// The tracked status of an artifact path, when tracked.
    #[must_use]
    pub fn artifact_status(&self, path: &str) -> Option<ArtifactStatus> {
        self.artifacts.iter().find(|a| a.path == path).map(|a| a.status)
    }

    /// Deterministic query: is artifact `path` verified-clean? `true` only
    /// when the workspace has not changed materially since the checkpoint
    /// (V-CHANGE), the artifact carries a Clean/Written status (never
    /// Dirty, never Stale), and no open question blocks on it. The
    /// coordinator's decision machinery consumes this (skip-recommending
    /// completed-verified work).
    #[must_use]
    pub fn artifact_verified_clean(&self, path: &str) -> bool {
        !self.workspace_changed
            && self.artifact_status(path).is_some_and(|status| status.is_transparent())
            && !self.questions.iter().any(|question| {
                question.state == QuestionState::Open
                    && question.kind == QuestionKind::BlockedPath
                    && question.blocks.as_deref() == Some(path)
            })
    }

    /// Which of `paths` are verified-clean, in input order.
    #[must_use]
    pub fn verified_clean_artifacts(&self, paths: &[String]) -> Vec<String> {
        paths.iter().filter(|path| self.artifact_verified_clean(path)).cloned().collect()
    }

    /// Render the compact prompt block. Character-bounded by construction:
    /// even maximally hostile state cannot exceed [`MAX_RENDER_CHARS`]
    /// (including the truncation mark).
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str("<world_model>\n");
        if let Some(generation) = &self.generation {
            out.push_str(&format!("workspace generation: {generation}\n"));
        }
        if let Some(objective) = &self.objective {
            out.push_str(&format!("objective: {objective}\n"));
        }
        if !self.criteria.is_empty() {
            out.push_str(&format!("success criteria: {}\n", self.criteria.join("; ")));
        }
        if !self.agents.is_empty() {
            out.push_str(&format!("agents: {}\n", self.agents.join(", ")));
        }
        if !self.models.is_empty() {
            out.push_str(&format!("models: {}\n", self.models.join(", ")));
        }
        if !self.tasks.is_empty() {
            out.push_str(&format!("work ({} entries):\n", self.tasks.len()));
            for task in self.tasks.iter().take(8) {
                out.push_str(&format!("- [{}] {} ({})\n", task.status, task.label, task.ref_id));
            }
        }
        if !self.facts.is_empty() {
            let verified = self.facts.iter().filter(|f| f.status == FactStatus::Verified).count();
            let stale = self.facts.iter().filter(|f| f.status == FactStatus::Stale).count();
            out.push_str(&format!("facts ({verified} verified, {stale} stale):\n"));
            for fact in self.facts.iter().take(8) {
                out.push_str(&format!(
                    "- (status: {:?}) {} [{}]\n",
                    fact.status, fact.label, fact.ref_id
                ));
            }
        }
        if !self.artifacts.is_empty() {
            out.push_str("artifacts (owner — status):\n");
            for artifact in self.artifacts.iter().take(8) {
                let owner = artifact.owner.as_deref().unwrap_or("unknown");
                let reference =
                    artifact.last_ref.as_deref().map(|r| format!(" ({r})")).unwrap_or_default();
                out.push_str(&format!(
                    "- {} — {owner} — {}{}\n",
                    artifact.path,
                    artifact.status.status_label(),
                    reference
                ));
            }
        }
        for question in self.questions.iter().filter(|q| q.state == QuestionState::Open).take(6) {
            out.push_str(&format!(
                "UNRESOLVED QUESTION {} ({:?}, open for {} cycle(s)): {}\n  blocks: {} — needed evidence: {}\n",
                question.id,
                question.kind,
                question.cycles_open,
                question.question,
                question.blocks.as_deref().unwrap_or("-"),
                if question.needed.is_empty() { "-".to_owned() } else { question.needed.join(", ") },
            ));
        }
        if !self.assumptions.is_empty() {
            out.push_str(&format!(
                "assumptions (no executed observation behind them): {}\n",
                self.assumptions.iter().map(|a| a.label.as_str()).collect::<Vec<_>>().join("; ")
            ));
        }
        if !self.risks.is_empty() {
            out.push_str(&format!(
                "risks: {}\n",
                self.risks.iter().map(|r| r.label.as_str()).collect::<Vec<_>>().join("; ")
            ));
        }
        if let Some(pending) = &self.pending {
            out.push_str(&format!("pending dispatch: {pending}\n"));
        }
        out.push_str("</world_model>\n");
        truncate_with_mark(out, MAX_RENDER_CHARS)
    }
}

impl ArtifactStatus {
    /// The deterministic text label for renders/logs (never `Debug::fmt`
    /// down a user path).
    #[must_use]
    pub fn status_label(self) -> &'static str {
        match self {
            Self::Written => "written (no contradicting observation)",
            Self::Clean => "clean",
            Self::Dirty => "dirty (uncertain)",
            Self::Stale => "stale",
        }
    }

    /// `Clean`/`Written` count as verified-clean; `Dirty`/`Stale` do not.
    #[must_use]
    pub fn is_transparent(self) -> bool {
        matches!(self, Self::Clean | Self::Written)
    }
}

impl UnresolvedQuestion {
    /// Whether the question is still open.
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.state == QuestionState::Open
    }
}

/// Bound the objective independently of the (untrusted, user-typed) task
/// text.
fn bounded_objective(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.chars().count() <= MAX_OBJECTIVE_CHARS {
        Some(trimmed.to_owned())
    } else {
        Some(bounded(trimmed.to_owned()))
    }
}

/// One entry per path: the newest effective write's attribution.
fn write_latest_per_path(writes: &[RecordedWrite]) -> Vec<(String, &RecordedWrite)> {
    // Writes arrive in log order; walk newest-first and keep the first hit
    // per path.
    let mut seen: HashSet<String> = HashSet::new();
    let mut out = Vec::new();
    for write in writes.iter().rev() {
        if seen.insert(write.path.clone()) {
            out.push((write.path.clone(), write));
        }
    }
    out
}

/// The question lifecycle pass: resolve carried standing questions with
/// fresh evidence, feed the current signals (deduping against open AND
/// resolved entries), then cap both lists deterministically.
fn resolve_and_feed_questions(input: &WorldModelInput<'_>) -> Vec<UnresolvedQuestion> {
    let mut questions: Vec<UnresolvedQuestion> = input.previous_questions.clone();

    // ── Resolution pass (Q-RESOLVE-*) ────────────────────────────────────
    for question in questions.iter_mut() {
        if question.state != QuestionState::Open {
            continue;
        }
        let resolved_now = match question.kind {
            QuestionKind::OpenProblem | QuestionKind::MissingEvidence => input
                .decisions
                .iter()
                .skip(question.opened_journal_len)
                .find(|decision| decision.status == DecisionStatus::Settled)
                .map(|decision| decision.id.clone()),
            QuestionKind::AmbiguousRecovery => {
                (!input.pending_stale).then(|| "pending-cleared".to_owned())
            }
            QuestionKind::BlockedPath => {
                // A newer CLEAN observation supersedes the standing dirt
                // (an observation event id different from the opening ref).
                let subject = question.blocks.clone().unwrap_or_default();
                input.artifacts.iter().find_map(|row| {
                    let fresh = row.path == subject
                        && !row.dirty
                        && row.last_event_id.as_deref()
                            != Some(question.opened_ref.as_deref().unwrap_or(""));
                    fresh.then(|| {
                        row.last_event_id
                            .clone()
                            .unwrap_or_else(|| "fresh-clean-observation".to_owned())
                    })
                })
            }
        };
        if let Some(resolved_by) = resolved_now {
            question.state = QuestionState::Resolved;
            question.resolved_by = Some(resolved_by);
            question.cycles_open = 0;
        }
    }

    // ── Feeding pass (Q-OPEN-*), each deduping against standing entries ──
    // 1) failure diagnoses → OpenProblem
    for diagnosis in input.diagnoses {
        if !(diagnosis.replan_required || (!diagnosis.retryable && !diagnosis.same_agent_viable)) {
            continue;
        }
        let subject = format!("{}:{}", diagnosis.code, bounded(diagnosis.evidence.clone()));
        feed_or_advance(
            QuestionKind::OpenProblem,
            subject,
            &mut questions,
            || {
                bounded(format!(
                    "how does the run recover from its failure ({}) with evidence {}",
                    diagnosis.code, diagnosis.evidence
                ))
            },
            None,
            Vec::new(),
            input.decisions.len(),
            None,
            input.now_ms,
        );
    }

    // 2) rejected journal decisions → MissingEvidence
    for (index, decision) in input.decisions.iter().enumerate() {
        if decision.status != DecisionStatus::Rejected {
            continue;
        }
        let subject = decision.id.clone();
        let needed = vec![decision.id.clone()];
        let blocks = decision.expected_artifacts.first().cloned();
        feed_or_advance(
            QuestionKind::MissingEvidence,
            subject,
            &mut questions,
            || {
                bounded(format!(
                    "the decision for '{}' was rejected — what is the corrected approach?",
                    bounded(decision.task_description.clone())
                ))
            },
            blocks,
            needed,
            index,
            None,
            decision_time_ms(&decision.created_at).unwrap_or(input.now_ms),
        );
    }

    // 3) stale pending decision → AmbiguousRecovery
    if input.pending_stale {
        if let Some(pending) = input.pending {
            let subject =
                format!("{}:{}", pending.selected_agent, bounded(pending.required_output.clone()));
            let needed: Vec<String> =
                pending.supporting_evidence_ids.iter().take(4).cloned().collect();
            feed_or_advance(
                QuestionKind::AmbiguousRecovery,
                subject,
                &mut questions,
                || {
                    bounded(format!(
                        "the recorded dispatch to {} may or may not have completed — \
                     verify before standing behind it",
                        pending.selected_agent
                    ))
                },
                None,
                needed,
                input.decisions.len(),
                None,
                input.now_ms,
            );
        }
    }

    // 4) dirty artifacts → BlockedPath (observation rows only; stale facts
    //    do not open questions — their reason is already explained by the
    //    superseding-write/generation rules)
    for row in input.artifacts.iter().filter(|row| row.dirty) {
        feed_or_advance(
            QuestionKind::BlockedPath,
            row.path.clone(),
            &mut questions,
            || {
                bounded(format!(
                    "is '{}' as recorded, or did the workspace move on? re-verify",
                    row.path
                ))
            },
            Some(row.path.clone()),
            vec![row.last_event_id.clone().unwrap_or_else(|| "unknown".to_owned())],
            input.decisions.len(),
            row.last_event_id.clone(),
            input.now_ms,
        );
    }

    // ── Caps (deterministic order: priority, then longest-standing first)
    let mut open: Vec<UnresolvedQuestion> = questions
        .iter()
        .filter(|question| question.state == QuestionState::Open)
        .cloned()
        .collect();
    let mut resolved: Vec<UnresolvedQuestion> = questions
        .iter()
        .filter(|question| question.state == QuestionState::Resolved)
        .cloned()
        .collect();
    open.sort_by(|a, b| {
        question_cap_rank(a.kind)
            .cmp(&question_cap_rank(b.kind))
            .then(b.cycles_open.cmp(&a.cycles_open))
    });
    open.truncate(MAX_OPEN_QUESTIONS);
    // Resolved memory keeps the NEWEST entries past the cap.
    resolved.sort_by_key(|question| question.opened_at_ms);
    if resolved.len() > MAX_RESOLVED_QUESTIONS {
        let excess = resolved.len() - MAX_RESOLVED_QUESTIONS;
        resolved.drain(0..excess);
    }
    open.extend(resolved);
    open
}

/// Feed or age one question attempt: an existing OPEN entry with this key
/// keeps standing (cycles_open grows); an existing RESOLVED entry blocks
/// re-opening (Q-RESOLVE-STAY); otherwise the closure builds a fresh
/// question. `opened_journal_len`/`opened_ref`/`opened_at_ms` differ per
/// signal, so they arrive as plain parameters.
#[allow(clippy::too_many_arguments)]
fn feed_or_advance(
    kind: QuestionKind,
    subject: String,
    questions: &mut Vec<UnresolvedQuestion>,
    question_text: impl FnOnce() -> String,
    blocks: Option<String>,
    needed: Vec<String>,
    opened_journal_len: usize,
    opened_ref: Option<String>,
    opened_at_ms: i64,
) {
    let id = question_key(kind, &subject);
    if let Some(question) = questions.iter_mut().find(|question| question.id == id) {
        if question.state == QuestionState::Open {
            question.cycles_open = question.cycles_open.saturating_add(1);
        }
        return; // resolved entries stay resolved (Q-RESOLVE-STAY)
    }
    questions.push(UnresolvedQuestion {
        id,
        kind,
        question: question_text(),
        blocks,
        needed,
        opened_journal_len,
        opened_ref,
        opened_at_ms,
        cycles_open: 1,
        state: QuestionState::Open,
        resolved_by: None,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::failure_diagnosis::{FailureDiagnosis, FailureKind};

    const NOW_MS: i64 = 1_000_000;
    const GENERATION: &str = "gen-current";

    /// One synthetic whiteboard event with the columns the builder reads.
    fn event(
        kind: WhiteboardKind,
        event_id: &str,
        seq: u64,
        payload: serde_json::Value,
    ) -> WhiteboardEvent {
        WhiteboardEvent {
            event_id: event_id.to_owned(),
            gate_seq: seq,
            agent_id: "coder".to_owned(),
            agent_seq: seq,
            kind,
            scope: String::new(),
            session_id: Some("session".to_owned()),
            plan_id: None,
            causation: None,
            payload,
            content_hash: "hash".to_owned(),
            pre_image_hash: None,
            created_at: 1_000,
        }
    }

    fn write_applied(path: &str) -> serde_json::Value {
        serde_json::json!({ "input": { "path": path } })
    }

    fn decision(id: &str, status: DecisionStatus, artifacts: &[&str]) -> CoordinatorDecision {
        CoordinatorDecision {
            id: id.to_owned(),
            kind: crate::decisions::DecisionKind::DispatchSpecialist,
            target_agent: None,
            task_description: format!("work for {id}"),
            notes: None,
            supporting_evidence_ids: Vec::new(),
            expected_artifacts: artifacts.iter().map(|p| (*p).to_owned()).collect(),
            transform: None,
            max_tool_calls: None,
            created_at: time::OffsetDateTime::from_unix_timestamp(1_240)
                .unwrap_or(time::OffsetDateTime::UNIX_EPOCH),
            status,
        }
    }

    fn diagnosis(code: &str, replan_required: bool, retryable: bool) -> FailureDiagnosis {
        FailureDiagnosis {
            kind: FailureKind::Provider,
            code: code.to_owned(),
            transient: false,
            retryable,
            same_agent_viable: retryable,
            alternate_agent_viable: false,
            replan_required,
            evidence: format!("evidence for {code}"),
        }
    }

    fn observation(
        path: &str,
        observed: bool,
        dirty: bool,
        generation: Option<&str>,
        last_event: Option<&str>,
    ) -> ArtifactObservation {
        ArtifactObservation {
            path: path.to_owned(),
            observed,
            dirty,
            generation: generation.map(str::to_owned),
            last_agent_id: Some("coder".to_owned()),
            last_event_id: last_event.map(str::to_owned),
        }
    }

    fn input<'a>(
        events: &'a [WhiteboardEvent],
        artifacts: Vec<ArtifactObservation>,
        decisions: &'a [CoordinatorDecision],
        diagnoses: &'a [FailureDiagnosis],
        previous: Vec<UnresolvedQuestion>,
    ) -> WorldModelInput<'a> {
        WorldModelInput {
            objective: "fix the parser",
            criteria: vec!["tests pass".to_owned()],
            current_generation: Some(GENERATION.to_owned()),
            workspace_changed: false,
            events,
            artifacts,
            decisions,
            diagnoses,
            roster: vec!["coder".to_owned()],
            models: vec!["cheap".to_owned()],
            pending: None,
            pending_stale: false,
            previous_questions: previous,
            now_ms: NOW_MS,
        }
    }

    #[test]
    fn build_is_deterministic_for_identical_inputs() {
        let events = vec![
            event(WhiteboardKind::DesignDoc, "ev1", 10, write_applied("src/a.rs")),
            event(WhiteboardKind::WriteApplied, "ev2", 20, write_applied("src/b.rs")),
            event(
                WhiteboardKind::ToolExecuted,
                "ev3",
                30,
                serde_json::json!({
                    "tool": "filesystem", "args": {}, "success": true, "paths": [
                        {"path": "src/a.rs"}
                    ]
                }),
            ),
        ];
        let decisions = vec![decision("d1", DecisionStatus::Settled, &[])];
        let one = WorldModel::build(&input(&events, Vec::new(), &decisions, &[], Vec::new()));
        let two = WorldModel::build(&input(&events, Vec::new(), &decisions, &[], Vec::new()));
        assert_eq!(one, two, "the pure builder is a function of its inputs");
        assert_eq!(one.render(), two.render(), "identical inputs render identically");
    }

    /// F-SUPERSEDE: a newer effective write to the same path stales the
    /// earlier fact about it.
    #[test]
    fn superseding_write_stales_earlier_fact() {
        let events = vec![
            event(
                WhiteboardKind::ToolExecuted,
                "ev-old",
                10,
                serde_json::json!({
                    "tool": "edit_file", "args": {"path": "src/a.rs"},
                    "success": true, "paths": [{"path": "src/a.rs"}]
                }),
            ),
            event(WhiteboardKind::WriteApplied, "ev-new", 30, write_applied("src/a.rs")),
        ];
        let model = WorldModel::build(&input(&events, Vec::new(), &[], &[], Vec::new()));
        let old = model.facts.iter().find(|f| f.ref_id == "ev-old").expect("old fact tracked");
        assert_eq!(old.status, FactStatus::Stale, "superseded fact is stale");
        let fresh = model.facts.iter().find(|f| f.ref_id == "ev-new").expect("new fact tracked");
        assert_eq!(fresh.status, FactStatus::Verified, "the newest write stays verified");
        // The artifact is written-by the newest writer (no observation row).
        let artifact = model.artifacts.iter().find(|a| a.path == "src/a.rs").expect("artifact");
        assert_eq!(artifact.status, ArtifactStatus::Written);
        assert_eq!(artifact.owner.as_deref(), Some("coder"));
        assert_eq!(artifact.last_ref.as_deref(), Some("ev-new"));
    }

    /// F-GENERATION + V-CHANGE: a workspace-generation change stales every
    /// log-derived fact and pulls the model out of verified-clean stance.
    #[test]
    fn generation_change_stales_facts_and_blocks_verified_clean() {
        let events =
            vec![event(WhiteboardKind::WriteApplied, "ev1", 10, write_applied("src/a.rs"))];
        let rows =
            vec![observation("src/observed.rs", true, false, Some(GENERATION), Some("ev-obs"))];
        let mut base = input(&events, rows, &[], &[], Vec::new());
        base.workspace_changed = true;
        let model = WorldModel::build(&base);
        let fact = model.facts.iter().find(|f| f.ref_id == "ev1").expect("fact");
        assert_eq!(fact.status, FactStatus::Stale, "generation change stales the fact");
        let observed =
            model.artifacts.iter().find(|a| a.path == "src/observed.rs").expect("row artifact");
        assert_eq!(
            observed.status,
            ArtifactStatus::Clean,
            "a row of the CURRENT generation stays clean"
        );
        assert!(!model.artifact_verified_clean("src/observed.rs"), "V-CHANGE: not verified-clean");
        assert!(
            !model.artifact_verified_clean("src/a.rs"),
            "changed workspace verities nothing clean"
        );
    }

    /// The verified-clean rules: written-only and matching-clean artifacts
    /// answer true; dirty rows and open blocking questions do not.
    #[test]
    fn artifact_verified_clean_rules() {
        let events =
            vec![event(WhiteboardKind::WriteApplied, "ev-write", 10, write_applied("written.rs"))];
        let rows = vec![
            observation("dirty.rs", true, true, Some(GENERATION), Some("obs-dirty")),
            observation("clean.rs", true, false, Some(GENERATION), Some("obs-clean")),
        ];
        let model = WorldModel::build(&input(&events, rows, &[], &[], Vec::new()));
        assert!(model.artifact_verified_clean("written.rs"), "A-WRITTEN is verified-clean");
        assert!(model.artifact_verified_clean("clean.rs"), "A-CLEAN is verified-clean");
        assert!(!model.artifact_verified_clean("dirty.rs"), "dirty is never verified-clean");
        assert!(!model.artifact_verified_clean("absent.rs"), "untracked is not verified-clean");

        // An open BlockedPath question on the path pulls the verdict to false.
        let dirty_rows =
            vec![observation("clean.rs", true, true, Some(GENERATION), Some("obs-dirty-2"))];
        let blocked = WorldModel::build(&input(&events, dirty_rows, &[], &[], Vec::new()));
        assert!(
            blocked.questions.iter().any(|q| q.kind == QuestionKind::BlockedPath && q.is_open()),
            "a dirty artifact opens one blocked-path question"
        );
        assert_eq!(blocked.open_question_count(), 1);
    }

    /// Q lifecycle (issue acceptance): a diagnosis opens a question (cycle
    /// 1), the same signal keeps THE SAME question standing with a growing
    // age (cycle 2, Q-DEDUPE), and a settled decision recorded after the
    /// opening resolves it (cycle 3, Q-RESOLVE-PROBLEM).
    #[test]
    fn question_opens_persists_then_resolves_by_settled_work() {
        let diagnoses = vec![diagnosis("repl-required", true, false)];
        let first = WorldModel::build(&input(&[], Vec::new(), &[], &diagnoses, Vec::new()));
        let opened = first
            .questions
            .iter()
            .find(|q| q.kind == QuestionKind::OpenProblem && q.is_open())
            .expect("the failure opened one question");
        assert_eq!(opened.cycles_open, 1, "a fresh question starts at age 1");

        // Cycle 2: the same signal again — one standing entry, older.
        let second =
            WorldModel::build(&input(&[], Vec::new(), &[], &diagnoses, first.questions.clone()));
        let standing = second
            .questions
            .iter()
            .find(|q| q.kind == QuestionKind::OpenProblem && q.is_open())
            .expect("the open question persists while unresolved");
        assert_eq!(opened.id, standing.id, "Q-DEDUPE keeps one id per question");
        assert_eq!(standing.cycles_open, 2, "the standing question ages, not duplicates");
        assert_eq!(second.open_question_count(), 1, "no duplicated question");

        // Cycle 3: settled work recorded after the question's opening.
        let decisions = vec![decision("d-resolved", DecisionStatus::Settled, &[])];
        let third = WorldModel::build(&input(
            &[],
            Vec::new(),
            &decisions,
            &diagnoses,
            second.questions.clone(),
        ));
        let resolved = third
            .questions
            .iter()
            .find(|q| q.kind == QuestionKind::OpenProblem)
            .expect("the resolved question is remembered");
        assert_eq!(resolved.state, QuestionState::Resolved, "resolved by the settled decision");
        assert_eq!(resolved.resolved_by.as_deref(), Some("d-resolved"));
        assert_eq!(third.open_question_count(), 0);

        // The question is not re-opened while the resolution stands.
        let fourth = WorldModel::build(&input(
            &[],
            Vec::new(),
            &decisions,
            &diagnoses,
            third.questions.clone(),
        ));
        assert!(
            fourth.questions.iter().all(|q| q.state == QuestionState::Resolved),
            "Q-RESOLVE-STAY: resolved questions never re-open"
        );
        assert_eq!(fourth.open_question_count(), 0);
    }

    /// Q-OPEN-BLOCKED + Q-RESOLVE-BLOCKED: a dirty row opens the question;
    /// a newer clean observation resolves it.
    #[test]
    fn dirty_artifact_question_resolves_by_clean_observation() {
        let dirty =
            vec![observation("src/lib.rs", true, true, Some(GENERATION), Some("obs-dirty"))];
        let first = WorldModel::build(&input(&[], dirty.clone(), &[], &[], Vec::new()));
        let open =
            first.questions.iter().find(|q| q.kind == QuestionKind::BlockedPath).expect("question");
        assert_eq!(open.blocks.as_deref(), Some("src/lib.rs"), "the question names what it blocks");
        assert_eq!(open.opened_ref.as_deref(), Some("obs-dirty"));

        // Still-dirty state persists THE SAME question with growing age.
        let second = WorldModel::build(&input(&[], dirty, &[], &[], first.questions.clone()));
        let standing = second
            .questions
            .iter()
            .find(|q| q.kind == QuestionKind::BlockedPath)
            .expect("standing");
        assert_eq!(open.id, standing.id, "one id per underlying question");
        assert_eq!(standing.cycles_open, 2);

        // A clean observation at a NEWER event id resolves it.
        let clean =
            vec![observation("src/lib.rs", true, false, Some(GENERATION), Some("obs-clean"))];
        let third = WorldModel::build(&input(&[], clean, &[], &[], second.questions.clone()));
        let resolved =
            third.questions.iter().find(|q| q.kind == QuestionKind::BlockedPath).expect("kept");
        assert_eq!(resolved.state, QuestionState::Resolved);
        assert_eq!(resolved.resolved_by.as_deref(), Some("obs-clean"));
        assert!(third.artifact_verified_clean("src/lib.rs"), "clean observation re-verifies it");
    }

    /// Caps are enforced with oversized inputs: an arbitrary pile of
    /// diagnoses, decisions and dirty rows lands within the declared bounds,
    /// and the render stays within its character bound.
    #[test]
    fn caps_enforced_with_oversized_inputs() {
        let mut decisions = Vec::new();
        for index in 0..40 {
            decisions.push(decision(
                &format!("d-{index}"),
                DecisionStatus::Rejected,
                &["src/a.rs"],
            ));
        }
        let mut facts_events = Vec::new();
        for index in 0..80 {
            facts_events.push(event(
                WhiteboardKind::WriteApplied,
                &format!("ev-{index}"),
                index,
                write_applied(&format!("src/{index}.rs")),
            ));
        }
        let diagnoses: Vec<FailureDiagnosis> =
            (0..20).map(|i| diagnosis(&format!("diag-{i}"), true, false)).collect();
        let rows: Vec<ArtifactObservation> = (0..50)
            .map(|i| observation(&format!("r/{i}.rs"), true, true, Some(GENERATION), None))
            .collect();
        let model =
            WorldModel::build(&input(&facts_events, rows, &decisions, &diagnoses, Vec::new()));
        assert!(model.facts.len() <= MAX_WORLD_FACTS, "facts are capped");
        assert_eq!(model.open_question_count(), MAX_OPEN_QUESTIONS, "questions are capped");
        assert!(model.artifacts.len() <= MAX_WORLD_ARTIFACTS, "artifacts are capped");
        assert!(model.tasks.len() <= MAX_WORLD_TASKS, "tasks are capped");
        assert!(model.risks.len() <= MAX_WORLD_RISKS, "risks are capped");
        assert!(model.render().chars().count() <= MAX_RENDER_CHARS, "the render is hard-bounded");
    }

    /// Every stored label is characters-bounded and every fact is a
    /// reference: the event id is stored, the prose never is.
    #[test]
    fn labels_are_bounded_and_facts_reference_ids() {
        let huge = "x".repeat(MAX_LABEL_CHARS * 4);
        let events = vec![
            event(WhiteboardKind::WriteApplied, "ev1", 10, write_applied("src/a.rs")),
            event(WhiteboardKind::Finding, "ev2", 20, serde_json::json!({"summary": huge})),
            event(
                WhiteboardKind::DesignDoc,
                "ev3",
                30,
                serde_json::json!({"proposed_files": ["a.rs", "b.rs"]}),
            ),
        ];
        let model = WorldModel::build(&input(&events, Vec::new(), &[], &[], Vec::new()));
        assert_eq!(model.facts.len(), 3, "write + finding + design fact derived");
        for fact in &model.facts {
            assert!(
                fact.label.chars().count() <= MAX_LABEL_CHARS + 1,
                "every label is bounded, got {}",
                fact.label.chars().count()
            );
            assert!(fact.ref_id.starts_with("ev"), "the fact references its event id");
        }
        let write = model.facts.iter().find(|f| f.ref_id == "ev1").expect("write fact");
        assert_eq!(write.status, FactStatus::Verified, "the write fact verifies");
        assert_eq!(write.artifact.as_deref(), Some("src/a.rs"));
        let finding = model.facts.iter().find(|f| f.ref_id == "ev2").expect("finding fact");
        assert_eq!(finding.status, FactStatus::Assumed, "findings are only assumed");
        let design = model.facts.iter().find(|f| f.ref_id == "ev3").expect("design fact");
        assert_eq!(design.status, FactStatus::Assumed, "design claims are only assumed");
        // The model never carries the full prose in.
        let rendered = model.render();
        assert!(rendered.contains(&finding.label), "short labels render");
        assert!(!rendered.contains(&huge), "prose is never copied into the model");
    }

    /// The rendered block is bounded even for a huge single entry, and the
    /// objective is bounded independently of the task text.
    #[test]
    fn render_is_bounded_and_objective_is_bounded() {
        let mut decisions = Vec::new();
        for index in 0..MAX_WORLD_TASKS * 2 {
            decisions.push(decision(&format!("d-{index}"), DecisionStatus::Settled, &[]));
        }
        let model = WorldModel::build(&input(&[], Vec::new(), &decisions, &[], Vec::new()));
        let rendered = model.render();
        assert!(
            rendered.chars().count() <= MAX_RENDER_CHARS,
            "pinned bound: {} > {MAX_RENDER_CHARS}",
            rendered.chars().count()
        );
        let mut long_objective = input(&[], Vec::new(), &[], &[], Vec::new());
        let long_text = "y".repeat(MAX_OBJECTIVE_CHARS * 5);
        long_objective.objective = &long_text;
        let with_long = WorldModel::build(&long_objective);
        let objective = with_long.objective.expect("objective tracked");
        assert!(
            objective.chars().count() <= MAX_OBJECTIVE_CHARS + 2,
            "the objective is independently bounded"
        );
        assert!(objective.ends_with('…'), "the objective truncates with a mark");
    }
}
