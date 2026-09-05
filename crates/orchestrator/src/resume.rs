//! ADR-65 §7 — continuation restores state at the whiteboard cursor; it does
//! not replay prose.
//!
//! When a stalled multi-agent run is continued, the checkpoint's §7 fields
//! (whiteboard cursor, doc resolution, snapshot generation, pending decision)
//! place the resume INSIDE the log: facts appended after the cursor are the
//! evidence view, and the resume chooses among exactly five outcomes —
//! continue the blocked task / replace the agent / skip the step / refresh
//! evidence / replan because the workspace objectively changed.
//!
//! Everything in this module is **deterministic runtime code, not a model**:
//! the same checkpoint + log slice + workspace reality always yield the same
//! outcome. The caller (the coordinator's restore path) gathers the inputs —
//! thread the `CancellationToken` around the I/O, never through here — and
//! records every outcome as a whiteboard `Decision` event with reason +
//! supporting evidence ids (real ids only; the append side validates them,
//! ADR-65 §1 acceptance 8).
//!
//! # Resume decision policy (deterministic, first match wins)
//!
//! Inputs: the FIRST blocked/failed step in the restored graph (graph order),
//! its post-cursor task facts, the replacement candidates for its capability
//! class (registered agents with the same stage tag, already filtered of
//! agents already tried for the step), the checkpoint's pending decision, and
//! the workspace-change verdict.
//!
//! 1. **Workspace objectively changed** — reconciliation (ADR-65 F3) shows
//!    changed/vanished observed paths that the run's own recorded writes do
//!    NOT explain:
//!    - material to the pending step (a pending decision or an open blocked
//!      step exists) → **Replan**: the recorded decision delegates new
//!      planning to the Phase-6 scheduler; the resume path itself dispatches
//!      nothing.
//!    - otherwise → **RefreshEvidence**: the evidence barrier is re-run
//!      (already done at run start) and the run continues.
//! 2. **Generation mismatch without external change** (the run's own writes
//!    moved the workspace, or the mismatch is unexplained) →
//!    **RefreshEvidence** — never a replan on the run's own progress.
//! 3. **Blocked design/explore-class step** (architect/researcher work):
//!    - the recorded pending decision explicitly selects this step's agent
//!      (with evidence ids) AND post-cursor progress facts exist →
//!      **ContinueBlocked** with the same agent (dispatch allowed behind the
//!      recorded, evidence-backed decision — acceptance 7);
//!    - otherwise → **SkipStep**: the resume path NEVER re-dispatches
//!      architect/researcher work on its own initiative; a later replan may
//!      re-select the stage through the scheduler's own recorded decision.
//! 4. **Blocked implement-class step**:
//!    - post-cursor progress facts for the step (successful tool executions,
//!      a completion) → **ContinueBlocked** with the same agent;
//!    - an untried replacement candidate exists → **ReplaceAgent** (kills the
//!      documented live failure of identical "blocked coder → continue"
//!      repeats re-dispatching the same coder with no evidence check);
//!    - no candidate and ≥ [`RESUME_REPLACE_FAILURE_THRESHOLD`] consecutive
//!      failed outcomes → **SkipStep**;
//!    - no candidate and fewer failures → **ContinueBlocked** with the same
//!      agent — the one bounded same-agent retry; the next stalled resume
//!      with no progress crosses the threshold and skips.
//! 5. **No blocked step** → **RestoreAndContinue** (the ordinary resume).
//!
//! # Cursor semantics
//!
//! [`split_at_cursor`] partitions a session log slice at the checkpoint's
//! `gate_seq` cursor: everything at or before the cursor is pre-cursor state
//! derivation input, everything after is the decision's evidence view. Events
//! before the cursor are never replayed into a decision or into agent prose
//! (the run-continuity read is cursor-anchored by the caller).

use concerto_core::types::{SubTaskStatus, TaskId};
use concerto_sessions::whiteboard::WhiteboardEvent;

use crate::checkpoint::{CheckpointAction, CheckpointPendingDecision};
use crate::graph::TaskGraph;

/// How many consecutive failed outcomes for the same step (with no
/// intervening progress) force a replace-then-skip instead of another
/// same-agent retry. This is the bound that kills the live failure of five
/// identical "blocked coder → continue" repeats: with the threshold at 2, a
/// stalled step gets at most ONE bounded same-agent continue before the
/// policy replaces (when a candidate exists) or skips.
pub const RESUME_REPLACE_FAILURE_THRESHOLD: u32 = 2;

/// The capability class of a blocked step, mapped by the caller from the
/// registry's lifecycle stage tags (ADR-58): `research` → Explore, `design` →
/// Design, the primary Execution tag → Implement. Architect/researcher work
/// is the Explore/Design classes — the classes a resume never re-dispatches
/// without a recorded, evidence-backed decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepClass {
    Explore,
    Design,
    Implement,
}

impl StepClass {
    /// Whether this class is architect/researcher work (acceptance 7
    /// protects it from un-decided resume re-dispatch).
    pub fn is_planning_class(self) -> bool {
        matches!(self, Self::Explore | Self::Design)
    }
}

/// One blocked/failed step in the restored graph, prepared by the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockedStep {
    pub task_id: TaskId,
    pub agent: String,
    pub class: StepClass,
    /// Failed outcomes recorded for this step: the checkpoint ledger's
    /// `failed` entries (pre-cursor) plus post-cursor failure facts — summed
    /// when the cursor is known, taken as the log-wide count when it is not
    /// (so pre-cursor failures are never double-counted).
    pub failure_count: u32,
    /// Agents already recorded as dispatched/selected for this step (ledger
    /// `dispatched` entries, the pending decision, logged decision events).
    /// A replacement never re-selects a tried agent.
    pub tried_agents: Vec<String>,
    /// Whether a recorded, evidence-backed Decision event (the checkpoint's
    /// pending decision or a logged scheduler decision) explicitly selected
    /// this step's agent — the only gate under which a resume may dispatch
    /// architect/researcher work (acceptance 7).
    pub recorded_selection: bool,
}

/// Post-cursor facts for one task, extracted from the log slice.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TaskFacts {
    /// Real event ids of post-cursor progress facts for the task
    /// (successful `ToolExecuted`, `SubtaskCompleted`).
    pub progress_event_ids: Vec<String>,
    /// Real event ids of post-cursor failure facts for the task.
    pub failure_event_ids: Vec<String>,
}

impl TaskFacts {
    /// Whether the agent made observable progress after the checkpoint.
    pub fn has_progress(&self) -> bool {
        !self.progress_event_ids.is_empty()
    }
}

/// The workspace-change verdict for a resume (ADR-65 §7 "objectively
/// changed"). Computed by the caller from the fresh snapshot generation and
/// the F3 reconciliation of the observed rows against the live filesystem.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkspaceChange {
    /// Whether the fresh snapshot generation differs from the checkpoint's.
    pub generation_mismatch: bool,
    /// Changed/vanished OBSERVED paths the run's own recorded writes do NOT
    /// explain, each cited by the REAL `resource_facts` event id of the last
    /// observation when one exists (never fabricated).
    pub externally_changed: Vec<(String, Option<String>)>,
}

impl WorkspaceChange {
    /// Whether the workspace objectively changed for reasons outside the
    /// run's own recorded writes.
    pub fn externally_changed(&self) -> bool {
        !self.externally_changed.is_empty()
    }
}

/// The outcome of a resume evaluation (ADR-65 §7's five choices, plus the
/// plain restore-and-continue when there is nothing to decide).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResumeOutcome {
    /// No blocked step: restore the graph and continue normally.
    RestoreAndContinue,
    /// Continue the blocked step with the named (same) agent, behind the
    /// recorded decision.
    ContinueBlocked { agent: String },
    /// Replace the blocked step's agent with the named alternative.
    ReplaceAgent { previous: String, replacement: String },
    /// Skip the blocked step (recorded; the step fails honestly and the run
    /// stalls Partial rather than silently proceeding past unfinished work).
    SkipStep { agent: String },
    /// Re-run the evidence barrier and continue — no replan.
    RefreshEvidence,
    /// The workspace objectively changed materially to the pending step:
    /// delegate new planning to the Phase-6 scheduler. The resume path
    /// itself dispatches nothing.
    Replan,
}

impl ResumeOutcome {
    /// The kebab-case reason code recorded on the whiteboard `Decision`
    /// event (ADR-65 §6 payload shape).
    pub fn reason_code(&self) -> &'static str {
        match self {
            Self::RestoreAndContinue => "resume-restore-and-continue",
            Self::ContinueBlocked { .. } => "resume-continue-blocked",
            Self::ReplaceAgent { .. } => "resume-replace-agent",
            Self::SkipStep { .. } => "resume-skip-step",
            Self::RefreshEvidence => "resume-refresh-evidence",
            Self::Replan => "resume-replan-workspace-changed",
        }
    }

    /// The agent the decision names (Continue/Replace select a dispatch
    /// target; Skip names the affected step's agent for audit).
    pub fn selected_agent(&self) -> Option<&str> {
        match self {
            Self::ContinueBlocked { agent } => Some(agent),
            Self::ReplaceAgent { replacement, .. } => Some(replacement),
            Self::SkipStep { agent } => Some(agent),
            Self::RestoreAndContinue | Self::RefreshEvidence | Self::Replan => None,
        }
    }

    /// Whether the outcome re-dispatches (or keeps) a blocked step's agent —
    /// the acceptance-7 gate: architect/researcher dispatches are allowed
    /// only when the outcome itself is recorded AND justified.
    pub fn dispatches(&self) -> bool {
        matches!(self, Self::ContinueBlocked { .. } | Self::ReplaceAgent { .. })
    }
}

/// Everything the pure resume evaluation consumes. Built by the caller from
/// the restored checkpoint, the post-cursor log slice, and workspace reality.
#[derive(Debug, Clone)]
pub struct ResumeInput<'a> {
    /// The first blocked/failed step in the restored graph (graph order), if
    /// any.
    pub blocked_step: Option<&'a BlockedStep>,
    /// Ordered replacement candidates for the blocked step's capability
    /// class (registered agents with the same stage tag, filtered of
    /// `tried_agents`). Empty when the step is `None`.
    pub replacement_candidates: &'a [String],
    /// The checkpoint's pending decision (the last scheduler dispatch
    /// awaiting completion), when recorded.
    pub pending_decision: Option<&'a CheckpointPendingDecision>,
    /// Post-cursor facts for the blocked step's task.
    pub step_facts: &'a TaskFacts,
    /// The workspace-change verdict.
    pub change: &'a WorkspaceChange,
}

/// Evaluate the resume outcome (deterministic; see the module docs for the
/// policy and its rationale).
pub fn evaluate(input: &ResumeInput<'_>) -> ResumeOutcome {
    // ── 1. Workspace objectively changed (outside the run's own writes). ──
    if input.change.externally_changed() {
        // Material to the pending step: any open work on the objective (a
        // pending dispatch or a blocked step) rests on evidence the changed
        // paths invalidate — replan via the scheduler. With no open work the
        // change only invalidates the evidence view: refresh it.
        let material = input.pending_decision.is_some() || input.blocked_step.is_some();
        return if material { ResumeOutcome::Replan } else { ResumeOutcome::RefreshEvidence };
    }

    // ── 2. Generation mismatch without external change: the run's own
    //       writes (or an unexplained mismatch) — refresh evidence, never
    //       replan on the run's own progress. ─────────────────────────────
    if input.change.generation_mismatch {
        return ResumeOutcome::RefreshEvidence;
    }

    // ── 3./4. The blocked step policy. ──────────────────────────────────
    let Some(step) = input.blocked_step else {
        return ResumeOutcome::RestoreAndContinue;
    };

    if step.class.is_planning_class() {
        // Acceptance 7: architect/researcher work is re-dispatched on resume
        // ONLY behind a recorded, evidence-backed decision that explicitly
        // selects the agent (the checkpoint's pending decision, or a logged
        // scheduler decision for this step) AND only when the facts show it
        // made progress. Otherwise the step is skipped: a later replan may
        // re-select the stage through the scheduler's own recorded decision.
        let pending_selection = input.pending_decision.is_some_and(|pending| {
            pending.selected_agent == step.agent && !pending.supporting_evidence_ids.is_empty()
        });
        if (pending_selection || step.recorded_selection) && input.step_facts.has_progress() {
            return ResumeOutcome::ContinueBlocked { agent: step.agent.clone() };
        }
        return ResumeOutcome::SkipStep { agent: step.agent.clone() };
    }

    // Implement-class step.
    if input.step_facts.has_progress() {
        // Prior facts indicate progress: continue with the same agent.
        return ResumeOutcome::ContinueBlocked { agent: step.agent.clone() };
    }
    let untried = input
        .replacement_candidates
        .iter()
        .find(|candidate| !step.tried_agents.contains(*candidate));
    if let Some(replacement) = untried {
        // No progress: replace the agent (never blind same-agent retry).
        return ResumeOutcome::ReplaceAgent {
            previous: step.agent.clone(),
            replacement: replacement.clone(),
        };
    }
    if step.failure_count >= RESUME_REPLACE_FAILURE_THRESHOLD {
        // The roster is exhausted for this step and failures keep repeating:
        // skip the step instead of an infinite same-agent retry.
        return ResumeOutcome::SkipStep { agent: step.agent.clone() };
    }
    // The one bounded same-agent retry (documented policy): with fewer than
    // RESUME_REPLACE_FAILURE_THRESHOLD recorded failures and no alternative
    // candidate, continue once — the next stalled resume crosses the
    // threshold and skips.
    ResumeOutcome::ContinueBlocked { agent: step.agent.clone() }
}

// ---------------------------------------------------------------------------
// Post-cursor fact extraction
// ---------------------------------------------------------------------------

/// Split an ordered log slice at the cursor. Returns `(pre_cursor,
/// post_cursor)`: events with `gate_seq <= cursor` are pre-cursor state
/// input; events after it are the resume decision's evidence view. The slice
/// must be ordered by `gate_seq` ascending (the order every whiteboard reader
/// returns); the split is a binary search on that order.
pub fn split_at_cursor(
    events: &[WhiteboardEvent],
    cursor: u64,
) -> (&[WhiteboardEvent], &[WhiteboardEvent]) {
    let split = events.partition_point(|event| event.gate_seq <= cursor);
    events.split_at(split)
}

/// The `task_id` a fact payload attributes, when present.
fn payload_task_id(payload: &serde_json::Value) -> Option<&str> {
    payload.get("task_id").and_then(serde_json::Value::as_str)
}

/// Extract the post-cursor facts for one task from the evidence view:
/// successful `ToolExecuted` rows and `SubtaskCompleted` rows are progress;
/// failed `ToolExecuted` rows and `SubtaskFailed` rows are failures. Every
/// id is the REAL log event id (never fabricated). Unknown payload shapes
/// contribute nothing.
pub fn task_facts_after_cursor(events: &[WhiteboardEvent], task_id: &str) -> TaskFacts {
    let mut facts = TaskFacts::default();
    for event in events {
        let Some(attributed) = payload_task_id(&event.payload) else { continue };
        if attributed != task_id {
            continue;
        }
        match event.kind {
            concerto_sessions::whiteboard::WhiteboardKind::ToolExecuted => {
                let success = event.payload.get("success").and_then(serde_json::Value::as_bool);
                match success {
                    Some(true) => facts.progress_event_ids.push(event.event_id.clone()),
                    Some(false) => facts.failure_event_ids.push(event.event_id.clone()),
                    // An unreadable success flag is not evidence either way.
                    None => {}
                }
            }
            concerto_sessions::whiteboard::WhiteboardKind::SubtaskCompleted => {
                facts.progress_event_ids.push(event.event_id.clone());
            }
            concerto_sessions::whiteboard::WhiteboardKind::SubtaskFailed => {
                facts.failure_event_ids.push(event.event_id.clone());
            }
            _ => {}
        }
    }
    facts
}

/// Collect the agent ids the post-cursor decision events selected. Used to
/// extend the tried-agents set for a step (a replacement never re-selects an
/// agent a recorded decision already chose after the checkpoint).
pub fn selected_agents_after_cursor(events: &[WhiteboardEvent]) -> Vec<String> {
    events
        .iter()
        .filter(|event| event.kind == concerto_sessions::whiteboard::WhiteboardKind::Decision)
        .filter_map(|event| event.payload.get("selected_agent").and_then(serde_json::Value::as_str))
        .filter(|agent| !agent.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Whether a logged event is a `Decision` row that explicitly selects
/// `agent` with at least one supporting evidence id — the "recorded,
/// evidence-backed decision" gate of acceptance 7.
pub fn decision_selects(event: &WhiteboardEvent, agent: &str) -> bool {
    event.kind == concerto_sessions::whiteboard::WhiteboardKind::Decision
        && event.payload.get("selected_agent").and_then(serde_json::Value::as_str) == Some(agent)
        && event
            .payload
            .get("supporting_evidence_ids")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|ids| !ids.is_empty())
}

// ---------------------------------------------------------------------------
// §7 field backfill (v3 → v4, additive, fail-soft)
// ---------------------------------------------------------------------------

/// ADR-65 §7 additive backfill for checkpoints persisted before the §7
/// fields existed (or by paths that did not capture them). Only fills fields
/// that are `None`; a present value is authoritative and never overwritten.
///
/// - `whiteboard_cursor_gate_seq`: the `gate_seq` of the last log event
///   appended at or before the checkpoint row's own `updated_at` (the "last
///   event before the checkpoint's own append"). Without a usable hint the
///   cursor stays `None` and the resume treats the whole log as pre-cursor —
///   the conservative reading (pre-cursor events are never replayed).
/// - `doc_resolution`: the newest `design-doc` claim and its verdict
///   decision (matched by `causation`), rebuilt into the typed resolution.
/// - `snapshot_generation`: the newest `workspace-snapshot` event's
///   generation.
///
/// Fail-soft by contract: unreadable payloads degrade to "no backfill for
/// that field", never an error — a resume must not fail because history is
/// imperfectly recorded.
pub fn backfill_v4_fields(
    checkpoint: &mut crate::checkpoint::GraphCheckpoint,
    events: &[WhiteboardEvent],
    checkpoint_updated_at_ms: Option<i64>,
) {
    if checkpoint.whiteboard_cursor_gate_seq.is_none() {
        checkpoint.whiteboard_cursor_gate_seq = checkpoint_updated_at_ms.and_then(|hint_ms| {
            events
                .iter()
                .filter(|event| event.created_at <= hint_ms)
                .map(|event| event.gate_seq)
                .max()
        });
    }

    if checkpoint.doc_resolution.is_none() {
        checkpoint.doc_resolution = backfill_doc_resolution(events);
    }

    if checkpoint.snapshot_generation.is_none() {
        checkpoint.snapshot_generation = events
            .iter()
            .rev()
            .find(|event| {
                event.kind == concerto_sessions::whiteboard::WhiteboardKind::WorkspaceSnapshot
            })
            .and_then(|event| event.payload.get("generation").and_then(serde_json::Value::as_str))
            .map(str::to_owned);
    }
}

/// Rebuild the doc resolution from the newest design-doc claim + its verdict
/// decision. The verdict decision is the coordinator `Decision` row whose
/// `causation` is the claim's event id and whose payload parses as the
/// Phase-5 `DesignDocVerdict`. `None` when no claim/verdict pair is readable.
fn backfill_doc_resolution(
    events: &[WhiteboardEvent],
) -> Option<crate::checkpoint::CheckpointDocResolution> {
    use concerto_sessions::whiteboard::WhiteboardKind;

    let claim = events.iter().rev().find(|event| event.kind == WhiteboardKind::DesignDoc)?;
    let verdict_event = events.iter().rev().find(|event| {
        event.kind == WhiteboardKind::Decision
            && event.causation.as_deref() == Some(&claim.event_id)
    })?;
    let verdict: crate::design_doc_verifier::DesignDocVerdict =
        serde_json::from_value(verdict_event.payload.clone()).ok()?;
    let claim_event_id = Some(claim.event_id.clone());
    let verdict_event_id = Some(verdict_event.event_id.clone());
    use crate::design_doc_verifier::{DesignDocReasonCode, DesignDocState};
    Some(match verdict.state {
        DesignDocState::Verified => crate::checkpoint::CheckpointDocResolution::Active {
            contract_paths: verdict.contract_paths.clone(),
            claim_event_id,
            verdict_event_id,
        },
        DesignDocState::Skipped => {
            crate::checkpoint::CheckpointDocResolution::Skipped { claim_event_id, verdict_event_id }
        }
        DesignDocState::Quarantined => {
            let mut codes = Vec::new();
            for reason in &verdict.reasons {
                let code = match reason.code {
                    DesignDocReasonCode::UngroundedPath => "ungrounded-path",
                    DesignDocReasonCode::TreeConflict => "tree-conflict",
                    DesignDocReasonCode::NoObservations => "no-observations",
                    DesignDocReasonCode::NoDesignNeeded => "no-design-needed",
                    DesignDocReasonCode::NoObservationsNoDesign => "no-observations-no-design",
                    DesignDocReasonCode::UnknownEvidenceRef => "unknown-evidence-ref",
                    DesignDocReasonCode::EvidenceUnavailable => "evidence-unavailable",
                };
                if !codes.contains(&code.to_owned()) {
                    codes.push(code.to_owned());
                }
            }
            crate::checkpoint::CheckpointDocResolution::Quarantined {
                reason_codes: codes,
                claim_event_id,
                verdict_event_id,
            }
        }
    })
}

// ---------------------------------------------------------------------------
// Outcome application (deterministic graph + ledger mutations)
// ---------------------------------------------------------------------------

/// Apply a resume outcome to the restored graph, returning the checkpoint
/// ledger entries that record it. Only Continue/Replace/Skip mutate the
/// graph; RefreshEvidence/Replan/RestoreAndContinue change nothing here.
///
/// - Continue: the blocked step is re-armed (`Blocked → Pending`) with its
///   original role; the next ready-batch dispatch picks it up with the same
///   agent.
/// - Replace: the step's role is rewritten to the replacement agent and the
///   step re-armed.
/// - Skip: the step is marked `Failed` — an honest terminal state. The run
///   stalls Partial (existing blocked/failed handling) instead of silently
///   proceeding past unfinished work; the recorded decision carries the why.
pub fn apply_outcome(
    graph: &mut TaskGraph,
    outcome: &ResumeOutcome,
    step: Option<&BlockedStep>,
) -> Vec<CheckpointAction> {
    let Some(step) = step else { return Vec::new() };
    let timestamp = time::OffsetDateTime::now_utc();
    match outcome {
        ResumeOutcome::ContinueBlocked { .. } => {
            graph.mark_pending(&step.task_id);
            vec![CheckpointAction {
                kind: "resume-continued".into(),
                task_id: Some(step.task_id),
                timestamp,
                evidence: None,
            }]
        }
        ResumeOutcome::ReplaceAgent { replacement, .. } => {
            if let Some(subtask) = graph.get_mut(&step.task_id) {
                subtask.role = concerto_core::types::AgentId::new(replacement);
                subtask.status = SubTaskStatus::Pending;
            }
            vec![CheckpointAction {
                kind: "resume-replaced".into(),
                task_id: Some(step.task_id),
                timestamp,
                evidence: None,
            }]
        }
        ResumeOutcome::SkipStep { .. } => {
            if let Some(subtask) = graph.get_mut(&step.task_id) {
                subtask.status = SubTaskStatus::Failed;
            }
            vec![CheckpointAction {
                kind: "resume-skipped".into(),
                task_id: Some(step.task_id),
                timestamp,
                evidence: None,
            }]
        }
        ResumeOutcome::RestoreAndContinue
        | ResumeOutcome::RefreshEvidence
        | ResumeOutcome::Replan => Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checkpoint::CheckpointPendingDecision;
    use concerto_core::ids::Ulid;
    use concerto_sessions::whiteboard::WhiteboardKind;
    use serde_json::json;

    /// A `ToolExecuted`-shaped fact row.
    fn tool_fact(event_id: &str, gate_seq: u64, task_id: &str, success: bool) -> WhiteboardEvent {
        WhiteboardEvent {
            event_id: event_id.to_owned(),
            gate_seq,
            agent_id: "coder".to_owned(),
            agent_seq: 1,
            kind: WhiteboardKind::ToolExecuted,
            scope: String::new(),
            session_id: Some("sess".to_owned()),
            plan_id: None,
            causation: None,
            payload: json!({ "task_id": task_id, "success": success, "tool": "filesystem" }),
            content_hash: String::new(),
            pre_image_hash: None,
            created_at: 1_700_000_000_000,
        }
    }

    fn blocked_step(agent: &str, class: StepClass, failure_count: u32) -> BlockedStep {
        BlockedStep {
            task_id: TaskId(Ulid::new()),
            agent: agent.to_owned(),
            class,
            failure_count,
            tried_agents: vec![agent.to_owned()],
            recorded_selection: false,
        }
    }

    fn pending(agent: &str, task_id: TaskId) -> CheckpointPendingDecision {
        CheckpointPendingDecision {
            selected_agent: agent.to_owned(),
            reason: "evidence-sufficient-implement".to_owned(),
            required_output: "Implement: obj".to_owned(),
            supporting_evidence_ids: vec!["ev-1".to_owned()],
            task_id: Some(task_id),
        }
    }

    fn input<'a>(
        step: Option<&'a BlockedStep>,
        candidates: &'a [String],
        pending_decision: Option<&'a CheckpointPendingDecision>,
        facts: &'a TaskFacts,
        change: &'a WorkspaceChange,
    ) -> ResumeInput<'a> {
        ResumeInput {
            blocked_step: step,
            replacement_candidates: candidates,
            pending_decision,
            step_facts: facts,
            change,
        }
    }

    // ── Acceptance 7: architect/researcher are never re-dispatched on
    //    resume without a recorded, evidence-backed decision. ─────────────
    #[test]
    fn planning_class_step_without_recorded_decision_is_skipped_never_dispatched() {
        // A blocked architect step with NO recorded decision: the resume path
        // must not dispatch the architect — the outcome is a recorded skip.
        let step = blocked_step("architect", StepClass::Design, 0);
        let change = WorkspaceChange::default();
        let outcome = evaluate(&input(Some(&step), &[], None, &TaskFacts::default(), &change));
        assert_eq!(outcome, ResumeOutcome::SkipStep { agent: "architect".to_owned() });
        assert!(!outcome.dispatches(), "no dispatch without a recorded decision");

        // Same for a researcher step.
        let step = blocked_step("researcher", StepClass::Explore, 0);
        let outcome = evaluate(&input(Some(&step), &[], None, &TaskFacts::default(), &change));
        assert!(matches!(outcome, ResumeOutcome::SkipStep { .. }));
        assert!(!outcome.dispatches());
    }

    #[test]
    fn planning_class_step_with_recorded_decision_and_progress_continues() {
        // The Phase-6 scheduler recorded a decision selecting the researcher
        // (the checkpoint's pending decision, evidence ids present) and the
        // post-cursor facts show progress: the dispatch is allowed.
        let step = blocked_step("researcher", StepClass::Explore, 0);
        let pending = pending("researcher", step.task_id);
        let facts = TaskFacts {
            progress_event_ids: vec!["ev-progress".to_owned()],
            failure_event_ids: vec![],
        };
        let outcome =
            evaluate(&input(Some(&step), &[], Some(&pending), &facts, &WorkspaceChange::default()));
        assert_eq!(
            outcome,
            ResumeOutcome::ContinueBlocked { agent: "researcher".to_owned() },
            "a recorded, evidence-backed decision allows the dispatch (acceptance 7)"
        );
        assert!(outcome.dispatches());
    }

    #[test]
    fn planning_class_step_with_recorded_decision_but_no_progress_is_skipped() {
        // A recorded decision selecting the architect but NO post-cursor
        // progress: the resume path does not re-run the architect on faith.
        let step = blocked_step("architect", StepClass::Design, 0);
        let pending = pending("architect", step.task_id);
        let outcome = evaluate(&input(
            Some(&step),
            &[],
            Some(&pending),
            &TaskFacts::default(),
            &WorkspaceChange::default(),
        ));
        assert!(matches!(outcome, ResumeOutcome::SkipStep { .. }));
    }

    #[test]
    fn pending_decision_for_a_different_agent_does_not_authorize_planning_dispatch() {
        // The pending decision names the coder; it does NOT authorize a
        // researcher re-dispatch.
        let step = blocked_step("researcher", StepClass::Explore, 0);
        let pending = pending("coder", TaskId(Ulid::new()));
        let outcome = evaluate(&input(
            Some(&step),
            &[],
            Some(&pending),
            &TaskFacts::default(),
            &WorkspaceChange::default(),
        ));
        assert!(matches!(outcome, ResumeOutcome::SkipStep { .. }));
    }

    #[test]
    fn logged_scheduler_decision_selecting_the_agent_authorizes_planning_continue() {
        // The recorded decision lives in the LOG (the scheduler's dispatch
        // decision for this step), not in the checkpoint's pending field:
        // with progress facts the dispatch is still allowed (acceptance 7).
        let mut step = blocked_step("researcher", StepClass::Explore, 0);
        step.recorded_selection = true;
        let facts =
            TaskFacts { progress_event_ids: vec!["ev-read".to_owned()], failure_event_ids: vec![] };
        let outcome = evaluate(&input(Some(&step), &[], None, &facts, &WorkspaceChange::default()));
        assert_eq!(outcome, ResumeOutcome::ContinueBlocked { agent: "researcher".to_owned() });
        assert!(outcome.dispatches());
    }

    // ── Implement-class policy: progress → continue; else replace → skip. ─
    #[test]
    fn implement_step_with_progress_continues_with_the_same_agent() {
        let step = blocked_step("coder", StepClass::Implement, 3);
        let facts = TaskFacts {
            progress_event_ids: vec!["ev-write".to_owned()],
            failure_event_ids: vec!["ev-fail".to_owned()],
        };
        let candidates = vec!["coder2".to_owned()];
        let outcome =
            evaluate(&input(Some(&step), &candidates, None, &facts, &WorkspaceChange::default()));
        assert_eq!(outcome, ResumeOutcome::ContinueBlocked { agent: "coder".to_owned() });
    }

    #[test]
    fn implement_step_without_progress_is_replaced_not_blindly_redispatched() {
        // The live failure this kills: "blocked coder → continue" repeats
        // re-dispatching the SAME coder with no evidence check. No progress
        // facts + an untried candidate ⇒ replace the agent.
        let step = blocked_step("coder", StepClass::Implement, 1);
        let candidates = vec!["coder2".to_owned()];
        let outcome = evaluate(&input(
            Some(&step),
            &candidates,
            None,
            &TaskFacts::default(),
            &WorkspaceChange::default(),
        ));
        assert_eq!(
            outcome,
            ResumeOutcome::ReplaceAgent {
                previous: "coder".to_owned(),
                replacement: "coder2".to_owned()
            }
        );
    }

    #[test]
    fn implement_step_replacement_never_reselects_a_tried_agent() {
        // coder2 was already tried for this step: the replacement must move
        // to coder3, never back to a tried agent.
        let mut step = blocked_step("coder", StepClass::Implement, 1);
        step.tried_agents = vec!["coder".to_owned(), "coder2".to_owned()];
        let candidates = vec!["coder2".to_owned(), "coder3".to_owned()];
        let outcome = evaluate(&input(
            Some(&step),
            &candidates,
            None,
            &TaskFacts::default(),
            &WorkspaceChange::default(),
        ));
        assert_eq!(
            outcome,
            ResumeOutcome::ReplaceAgent {
                previous: "coder".to_owned(),
                replacement: "coder3".to_owned()
            }
        );
    }

    #[test]
    fn implement_step_without_candidates_gets_one_bounded_same_agent_retry() {
        // Single-coder roster: one bounded continue below the threshold.
        let step = blocked_step("coder", StepClass::Implement, 1);
        let outcome = evaluate(&input(
            Some(&step),
            &[],
            None,
            &TaskFacts::default(),
            &WorkspaceChange::default(),
        ));
        assert_eq!(outcome, ResumeOutcome::ContinueBlocked { agent: "coder".to_owned() });

        // The threshold crossed: skip — the 5-repeat failure mode is dead.
        let step = blocked_step("coder", StepClass::Implement, RESUME_REPLACE_FAILURE_THRESHOLD);
        let outcome = evaluate(&input(
            Some(&step),
            &[],
            None,
            &TaskFacts::default(),
            &WorkspaceChange::default(),
        ));
        assert!(matches!(outcome, ResumeOutcome::SkipStep { .. }));
    }

    // ── Workspace change: refresh / replan ──────────────────────────────
    #[test]
    fn externally_changed_workspace_with_pending_step_replans() {
        let step = blocked_step("coder", StepClass::Implement, 0);
        let change = WorkspaceChange {
            generation_mismatch: true,
            externally_changed: vec![("src/main.rs".to_owned(), Some("ev-obs".to_owned()))],
        };
        let outcome = evaluate(&input(Some(&step), &[], None, &TaskFacts::default(), &change));
        assert_eq!(outcome, ResumeOutcome::Replan);
    }

    #[test]
    fn externally_changed_workspace_without_open_work_refreshes_evidence() {
        let change = WorkspaceChange {
            generation_mismatch: true,
            externally_changed: vec![("docs/notes.md".to_owned(), Some("ev-obs".to_owned()))],
        };
        let outcome = evaluate(&input(None, &[], None, &TaskFacts::default(), &change));
        assert_eq!(outcome, ResumeOutcome::RefreshEvidence);
    }

    #[test]
    fn generation_mismatch_without_external_change_never_replans() {
        // The run's own writes moved the generation: refresh evidence, keep
        // the restored graph — the run's own progress must not trigger a
        // replan loop.
        let step = blocked_step("coder", StepClass::Implement, 0);
        let change = WorkspaceChange { generation_mismatch: true, externally_changed: Vec::new() };
        let outcome = evaluate(&input(Some(&step), &[], None, &TaskFacts::default(), &change));
        assert_eq!(outcome, ResumeOutcome::RefreshEvidence);
    }

    #[test]
    fn unchanged_workspace_with_no_blocked_step_restores_and_continues() {
        let outcome =
            evaluate(&input(None, &[], None, &TaskFacts::default(), &WorkspaceChange::default()));
        assert_eq!(outcome, ResumeOutcome::RestoreAndContinue);
    }

    // ── Cursor semantics ────────────────────────────────────────────────
    #[test]
    fn split_at_cursor_keeps_pre_cursor_events_out_of_the_evidence_view() {
        let events = vec![
            tool_fact("pre-1", 1, "t", true),
            tool_fact("pre-2", 2, "t", false),
            tool_fact("post-1", 3, "t", false),
            tool_fact("post-2", 4, "t", true),
        ];
        let (pre, post) = split_at_cursor(&events, 2);
        assert_eq!(
            pre.iter().map(|e| e.event_id.as_str()).collect::<Vec<_>>(),
            vec!["pre-1", "pre-2"]
        );
        assert_eq!(
            post.iter().map(|e| e.event_id.as_str()).collect::<Vec<_>>(),
            vec!["post-1", "post-2"]
        );

        // Facts after the cursor only: the pre-cursor progress fact is NOT
        // replayed into the evidence view.
        let facts = task_facts_after_cursor(post, "t");
        assert_eq!(facts.progress_event_ids, vec!["post-2".to_owned()]);
        assert_eq!(facts.failure_event_ids, vec!["post-1".to_owned()]);
    }

    #[test]
    fn task_facts_ignore_unknown_payload_shapes_and_other_tasks() {
        let mut alien = tool_fact("alien", 1, "other", false);
        alien.payload = json!({ "task_id": "t" }); // a SubtaskFailed for t
        alien.kind = WhiteboardKind::SubtaskFailed;
        let mut unreadable = tool_fact("unreadable", 2, "t", false);
        unreadable.payload = json!({ "task_id": "t" }); // success flag missing
        let events = vec![alien, unreadable, tool_fact("ok", 3, "t", true)];
        let facts = task_facts_after_cursor(&events, "t");
        assert_eq!(facts.progress_event_ids, vec!["ok".to_owned()]);
        assert_eq!(facts.failure_event_ids, vec!["alien".to_owned()]);
    }

    #[test]
    fn selected_agents_after_cursor_reads_only_decision_rows() {
        let mut decision = tool_fact("dec", 1, "t", true);
        decision.kind = WhiteboardKind::Decision;
        decision.payload = json!({ "selected_agent": "coder2", "reason": "resume-replace-agent" });
        let mut empty_agent = tool_fact("empty", 2, "t", true);
        empty_agent.kind = WhiteboardKind::Decision;
        empty_agent.payload = json!({ "selected_agent": "" });
        let events = vec![decision, empty_agent, tool_fact("fact", 3, "t", true)];
        assert_eq!(selected_agents_after_cursor(&events), vec!["coder2".to_owned()]);
    }

    #[test]
    fn decision_selects_requires_the_kind_the_agent_and_evidence_ids() {
        let mut decision = tool_fact("dec", 1, "t", true);
        decision.kind = WhiteboardKind::Decision;
        // No evidence ids: not an evidence-backed selection.
        decision.payload = json!({ "selected_agent": "researcher" });
        assert!(!decision_selects(&decision, "researcher"));
        // Evidence ids present: the gate opens for the selected agent only.
        decision.payload =
            json!({ "selected_agent": "researcher", "supporting_evidence_ids": ["ev-1"] });
        assert!(decision_selects(&decision, "researcher"));
        assert!(!decision_selects(&decision, "architect"));
        // A non-Decision row never selects.
        let mut fact = tool_fact("fact", 2, "t", true);
        fact.payload =
            json!({ "selected_agent": "researcher", "supporting_evidence_ids": ["ev-1"] });
        assert!(!decision_selects(&fact, "researcher"));
    }

    // ── Outcome application ─────────────────────────────────────────────
    fn graph_with_blocked(agent: &str) -> (TaskGraph, TaskId) {
        let mut graph = TaskGraph::new();
        let subtask = concerto_core::types::SubTask {
            id: TaskId(Ulid::new()),
            parent_id: None,
            session_id: Ulid::new(),
            role: concerto_core::types::AgentId::new(agent),
            description: "blocked work".into(),
            status: SubTaskStatus::Blocked,
            dependencies: vec![],
            deliverable: None,
            created_at: time::OffsetDateTime::now_utc(),
            completed_at: None,
        };
        let id = subtask.id;
        graph.add_root(subtask);
        (graph, id)
    }

    #[test]
    fn apply_continue_rearms_the_blocked_step_with_the_same_role() {
        let (mut graph, id) = graph_with_blocked("coder");
        let step = blocked_step("coder", StepClass::Implement, 0);
        let step = BlockedStep { task_id: id, ..step };
        let ledger = apply_outcome(
            &mut graph,
            &ResumeOutcome::ContinueBlocked { agent: "coder".to_owned() },
            Some(&step),
        );
        assert_eq!(graph.get(&id).map(|s| s.status), Some(SubTaskStatus::Pending));
        assert_eq!(graph.get(&id).map(|s| s.role.as_str()), Some("coder"));
        assert_eq!(ledger.len(), 1);
        assert_eq!(ledger[0].kind, "resume-continued");
    }

    #[test]
    fn apply_replace_rewrites_the_role_and_rearms() {
        let (mut graph, id) = graph_with_blocked("coder");
        let step = blocked_step("coder", StepClass::Implement, 0);
        let step = BlockedStep { task_id: id, ..step };
        let ledger = apply_outcome(
            &mut graph,
            &ResumeOutcome::ReplaceAgent {
                previous: "coder".to_owned(),
                replacement: "coder2".to_owned(),
            },
            Some(&step),
        );
        assert_eq!(graph.get(&id).map(|s| s.role.as_str()), Some("coder2"));
        assert_eq!(graph.get(&id).map(|s| s.status), Some(SubTaskStatus::Pending));
        assert_eq!(ledger[0].kind, "resume-replaced");
    }

    #[test]
    fn apply_skip_marks_the_step_failed_and_other_outcomes_change_nothing() {
        let (mut graph, id) = graph_with_blocked("coder");
        let step = blocked_step("coder", StepClass::Implement, 0);
        let step = BlockedStep { task_id: id, ..step };
        let ledger = apply_outcome(
            &mut graph,
            &ResumeOutcome::SkipStep { agent: "coder".to_owned() },
            Some(&step),
        );
        assert_eq!(graph.get(&id).map(|s| s.status), Some(SubTaskStatus::Failed));
        assert_eq!(ledger[0].kind, "resume-skipped");

        // Refresh/Replan/Restore are read-only outcomes.
        for outcome in [
            ResumeOutcome::RefreshEvidence,
            ResumeOutcome::Replan,
            ResumeOutcome::RestoreAndContinue,
        ] {
            let (mut graph, id) = graph_with_blocked("coder");
            assert!(apply_outcome(&mut graph, &outcome, Some(&step)).is_empty());
            assert_eq!(graph.get(&id).map(|s| s.status), Some(SubTaskStatus::Blocked));
        }
    }

    // ── §7 backfill (v3 → v4, additive, fail-soft) ──────────────────────
    fn snapshot_event(
        event_id: &str,
        gate_seq: u64,
        generation: &str,
        created_at: i64,
    ) -> WhiteboardEvent {
        let mut event = tool_fact(event_id, gate_seq, "t", true);
        event.kind = WhiteboardKind::WorkspaceSnapshot;
        event.payload = json!({ "generation": generation });
        event.created_at = created_at;
        event
    }

    fn doc_claim(event_id: &str, gate_seq: u64, created_at: i64) -> WhiteboardEvent {
        let mut event = tool_fact(event_id, gate_seq, "t", true);
        event.kind = WhiteboardKind::DesignDoc;
        event.payload = json!({ "goals": [] });
        event.created_at = created_at;
        event
    }

    fn verdict_decision(
        event_id: &str,
        gate_seq: u64,
        claim: &str,
        created_at: i64,
    ) -> WhiteboardEvent {
        let mut event = tool_fact(event_id, gate_seq, "t", true);
        event.kind = WhiteboardKind::Decision;
        event.payload = json!({
            "state": "verified",
            "reasons": [],
            "author_read_count": 2,
            "reject_count": 0,
            "contract_paths": ["src/main.rs"],
        });
        event.causation = Some(claim.to_owned());
        event.created_at = created_at;
        event
    }

    #[test]
    fn backfill_derives_cursor_doc_and_snapshot_from_the_log() {
        use crate::checkpoint::GraphCheckpoint;
        let mut cp: GraphCheckpoint = crate::checkpoint::GraphCheckpoint::from_json(
            r#"{
                "schema_version": 3,
                "run_id": "01HZ0X0X0X0X0X0X0X0X0X0X0X",
                "session_id": "01HZ0X0X0X0X0X0X0X0X0X0X0X",
                "root_task_id": "01HZ0X0X0X0X0X0X0X0X0X0X0X",
                "subtasks": [],
                "edges": [],
                "completed_results": {},
                "total_cost": 0.0,
                "total_tool_calls": 0,
                "provider_metrics": [],
                "all_files": [],
                "expected_artifacts": {},
                "subtask_attempts": {},
                "retry_feedback": {}
            }"#,
        )
        .unwrap();
        assert_eq!(
            cp.schema_version,
            crate::checkpoint::GRAPH_CHECKPOINT_SCHEMA_VERSION,
            "v3 migrated to v4 on load"
        );

        // Log timeline: snapshot (t=10) → doc claim (t=20) → verdict (t=21)
        // → the checkpoint's own append (hint t=30) → later fact (t=40).
        let events = vec![
            snapshot_event("snap", 1, "gen-1", 10),
            doc_claim("claim", 2, 20),
            verdict_decision("verdict", 3, "claim", 21),
            tool_fact("after-checkpoint", 4, "t", true),
        ];
        // `after-checkpoint` was appended AFTER the checkpoint row
        // (created_at 40 > hint 30): it must NOT pull the cursor forward.
        backfill_v4_fields(&mut cp, &events, Some(30));

        assert_eq!(
            cp.whiteboard_cursor_gate_seq,
            Some(3),
            "cursor = last event before the checkpoint's own append"
        );
        assert_eq!(cp.snapshot_generation.as_deref(), Some("gen-1"));
        match &cp.doc_resolution {
            Some(crate::checkpoint::CheckpointDocResolution::Active {
                contract_paths,
                claim_event_id,
                verdict_event_id,
            }) => {
                assert_eq!(contract_paths, &["src/main.rs".to_owned()]);
                assert_eq!(claim_event_id.as_deref(), Some("claim"));
                assert_eq!(verdict_event_id.as_deref(), Some("verdict"));
            }
            other => panic!("expected an Active doc resolution, got {other:?}"),
        }
    }

    #[test]
    fn backfill_never_overwrites_present_fields_and_degrades_without_hint() {
        use crate::checkpoint::{GraphCheckpoint, GRAPH_CHECKPOINT_SCHEMA_VERSION};
        let cp_json = format!(
            r#"{{
                "schema_version": {GRAPH_CHECKPOINT_SCHEMA_VERSION},
                "run_id": "01HZ0X0X0X0X0X0X0X0X0X0X0X",
                "session_id": "01HZ0X0X0X0X0X0X0X0X0X0X0X",
                "root_task_id": "01HZ0X0X0X0X0X0X0X0X0X0X0X",
                "subtasks": [],
                "edges": [],
                "completed_results": {{}},
                "total_cost": 0.0,
                "total_tool_calls": 0,
                "provider_metrics": [],
                "all_files": [],
                "expected_artifacts": {{}},
                "subtask_attempts": {{}},
                "retry_feedback": {{}},
                "whiteboard_cursor_gate_seq": 7,
                "snapshot_generation": "gen-recorded"
            }}"#
        );
        let mut cp: GraphCheckpoint = serde_json::from_str(&cp_json).unwrap();
        let events = vec![snapshot_event("snap", 1, "gen-log", 10)];
        // No hint: the cursor stays as recorded (None is impossible here — it
        // IS recorded), and without a hint nothing else may invent one.
        backfill_v4_fields(&mut cp, &events, None);
        assert_eq!(cp.whiteboard_cursor_gate_seq, Some(7), "recorded cursor is authoritative");
        assert_eq!(
            cp.snapshot_generation.as_deref(),
            Some("gen-recorded"),
            "recorded generation is authoritative"
        );
        assert!(cp.doc_resolution.is_none(), "no doc pair in the log → stays None (fail-soft)");
    }

    #[test]
    fn backfill_tolerates_unreadable_verdict_payloads() {
        use crate::checkpoint::GraphCheckpoint;
        let mut cp: GraphCheckpoint = crate::checkpoint::GraphCheckpoint::from_json(
            r#"{
                "schema_version": 3,
                "run_id": "01HZ0X0X0X0X0X0X0X0X0X0X0X",
                "session_id": "01HZ0X0X0X0X0X0X0X0X0X0X0X",
                "root_task_id": "01HZ0X0X0X0X0X0X0X0X0X0X0X",
                "subtasks": [],
                "edges": [],
                "completed_results": {},
                "total_cost": 0.0,
                "total_tool_calls": 0,
                "provider_metrics": [],
                "all_files": [],
                "expected_artifacts": {},
                "subtask_attempts": {},
                "retry_feedback": {}
            }"#,
        )
        .unwrap();
        let mut bad_verdict = verdict_decision("verdict", 3, "claim", 21);
        bad_verdict.payload = json!({ "state": "not-a-real-state" });
        let events = vec![doc_claim("claim", 2, 20), bad_verdict];
        backfill_v4_fields(&mut cp, &events, Some(30));
        assert!(cp.doc_resolution.is_none(), "an unreadable verdict degrades to no backfill");
        assert_eq!(cp.whiteboard_cursor_gate_seq, Some(3));
    }
}
