//! Execution obligations — the concurrent-execution correction to the
//! mode-only dispatch model (review of `feat/coordinator-conversational-145-148`).
//!
//! Root cause the review identified: [`requires_mandatory_dispatch`] arms every
//! prose/vacuous/zero-work/completion guard on
//! [`TaskExecutionMode::ActionRequired`] alone, so a fresh implementation
//! obligation entering in [`TaskExecutionMode::CoordinatorDecides`] is
//! prose-exempt by construction. Flipping the predicate to force a dispatch
//! per conversational turn would be equally wrong.
//!
//! Revised model: communication and execution are concurrent capabilities, not
//! mutually exclusive modes. A turn may be answered in prose *while* execution
//! proceeds; the prose half discharges nothing. What the run owes is recorded
//! as structured work:
//!
//! - **outstanding obligations** ([`ExecutionObligation`]): structured work the
//!   run owes — multiple per message, chained
//!   investigate → implement → verify → explain, each in
//!   [`ObligationState`] (`Outstanding`/`Completed`/`Blocked`/`Failed`/
//!   `Superseded`). Only validated [`ObligationEvent`] transitions move them;
//!   a conversational message moves nothing — the graph stays authoritative
//!   and the ledger is simply re-derived from it
//!   ([`ObligationLedger::sync_from_graph`]).
//! - **completion evidence** (the `evidence` field of [`ExecutionObligation`]):
//!   the whiteboard event ids / artifact paths backing a completion claim.
//!   Prose is never evidence, and [`ObligationLedger::apply`] rejects an
//!   evidenceless [`ObligationEvent::Complete`] on execution kinds.
//! - **the dispatch guard** ([`dispatch_guard_arms`]): whether the run owes a
//!   dispatch, derived from the mode *or* the outstanding obligations — never
//!   the mode alone.
//!
//! Obligations are a validated *view* over the existing [`TaskGraph`]
//! (extended, not paralleled): [`ObligationLedger::sync_from_graph`]
//! materializes them from subtask statuses, so checkpoint persistence,
//! interrupt/cancel/retry/resume continuity, and declared follow-up work all
//! ride the graph rows the checkpoint already stores. No keyword router, no
//! greeting list: the coordinator interprets intent and creates graph work;
//! this module (and the coordinator guards built on it) only validates the
//! resulting state transitions.
//!
//! [`requires_mandatory_dispatch`]: crate::coordinator::requires_mandatory_dispatch
//! [`TaskGraph`]: crate::graph::TaskGraph
//! [`TaskExecutionMode::ActionRequired`]: concerto_core::types::TaskExecutionMode::ActionRequired
//! [`TaskExecutionMode::CoordinatorDecides`]: concerto_core::types::TaskExecutionMode::CoordinatorDecides

use std::collections::HashMap;

use concerto_core::types::AgentId;
use serde::{Deserialize, Serialize};

use crate::graph::TaskGraph;

mod transitions;
pub use self::transitions::{dispatch_guard_arms, obligation_state_for_status, try_transition};

/// What kind of work an obligation tracks. Chained in the canonical
/// investigate → implement → verify → explain order via
/// [`ExecutionObligation::depends_on`]; conversational prose is not an
/// obligation — a message that needs no work derives zero of these.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ObligationKind {
    /// Read-only fact-finding that must happen before implementation.
    Investigate,
    /// Workspace-changing work that must be delegated to a specialist.
    Implement,
    /// Proof the implementation holds (tests, checks, review acceptance).
    Verify,
    /// The human-facing explanation owed alongside or after the work.
    Explain,
}

impl ObligationKind {
    /// Whether this kind is execution work that prose alone can never
    /// discharge. Explanations are satisfiable in prose; everything else
    /// requires tool/dispatch evidence.
    pub fn requires_execution_evidence(self) -> bool {
        !matches!(self, Self::Explain)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Investigate => "investigate",
            Self::Implement => "implement",
            Self::Verify => "verify",
            Self::Explain => "explain",
        }
    }

    /// Whether prose alone may satisfy an obligation of this kind. Only
    /// explanations are prose-satisfiable, and only once the obligations they
    /// explain are themselves settled — enforced by
    /// [`ObligationLedger::apply`] via the `depends_on` check, not by this
    /// predicate alone.
    pub fn is_prose_satisfiable(self) -> bool {
        !self.requires_execution_evidence()
    }
}

/// Lifecycle state of one obligation. Terminal states ([`Self::Completed`],
/// [`Self::Superseded`]) reject every further transition; `Blocked` and
/// `Failed` require an explicit [`ObligationEvent::Retry`] before work can
/// resume, so a stall is never silently re-armed by the next turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ObligationState {
    Outstanding,
    Completed,
    Blocked,
    Failed,
    Superseded,
}

impl ObligationState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Outstanding => "outstanding",
            Self::Completed => "completed",
            Self::Blocked => "blocked",
            Self::Failed => "failed",
            Self::Superseded => "superseded",
        }
    }

    /// Whether the obligation still constrains the run: anything but the two
    /// terminal states keeps the execution policy armed.
    pub fn is_open(self) -> bool {
        matches!(self, Self::Outstanding | Self::Blocked | Self::Failed)
    }
}

/// One unit of work the run owes. `depends_on` holds the obligation ids that
/// must complete first (investigation → implementation → verification →
/// explanation); `evidence` holds the whiteboard event ids / artifact paths
/// that back a [`ObligationState::Completed`] claim — prose is never stored
/// here, and [`ObligationLedger::sync_from_graph`] never populates it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionObligation {
    /// Stable id — the backing subtask id when derived from the graph, or a
    /// synthetic `urn`-style id for ledger-only obligations.
    pub id: String,
    pub kind: ObligationKind,
    pub description: String,
    pub state: ObligationState,
    pub depends_on: Vec<String>,
    pub evidence: Vec<String>,
}

impl ExecutionObligation {
    pub fn new(
        id: impl Into<String>,
        kind: ObligationKind,
        description: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            kind,
            description: description.into(),
            state: ObligationState::Outstanding,
            depends_on: Vec::new(),
            evidence: Vec::new(),
        }
    }
}

/// Validated state-transition events. Every mutation of an obligation goes
/// through [`ObligationLedger::apply`], which rejects illegal moves instead of
/// silently re-cutting state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObligationEvent {
    /// Settle the obligation with execution evidence (tool-backed files,
    /// verification output, whiteboard event ids). Empty evidence is
    /// rejected: prose never marks execution complete.
    Complete {
        evidence: Vec<String>,
    },
    Block {
        reason: String,
    },
    Fail {
        reason: String,
    },
    /// Re-arm a `Blocked`/`Failed` obligation after interruption, cancel, or
    /// retry. The only event those states accept back toward `Outstanding`.
    Retry,
    /// Retire the obligation because a reconsider/split/merge replaced it.
    /// Terminal like `Completed`.
    Supersede {
        by: String,
    },
}

/// Why a transition was rejected. Returned, never panicked: the coordinator
/// surfaces these as structured tool errors the model can read and fix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObligationTransitionError {
    pub obligation_id: String,
    pub from: ObligationState,
    pub reason: String,
}

impl std::fmt::Display for ObligationTransitionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "obligation {} ({}): {}", self.obligation_id, self.from.as_str(), self.reason)
    }
}

impl std::error::Error for ObligationTransitionError {}

/// Owned, transition-validated collection of a run's obligations.
///
/// The ledger is a validated *view*, not a second store: [`Self::sync_from_graph`]
/// rebuilds it from the [`TaskGraph`] rows the checkpoint already persists, so
/// interrupt/cancel/retry/resume continuity rides the existing checkpoint
/// rows. Ledger-only mutations ([`Self::supersede`]) are run-scoped and
/// re-derived on every resume — the graph stays authoritative.
#[derive(Debug, Clone, Default)]
pub struct ObligationLedger {
    obligations: HashMap<String, ExecutionObligation>,
}

impl ObligationLedger {
    pub fn new() -> Self {
        Self::default()
    }

    /// Rebuild the ledger from the live graph. `classify` maps a subtask role
    /// to its obligation kind; roles returning `None` (conversational,
    /// advisory, freeform work) derive no execution obligation. Dependencies
    /// mirror the graph edges, giving the
    /// investigate → implement → verify → explain chain its order.
    ///
    /// Evidence is never derived from the subtask row: `SubTask::deliverable`
    /// is agent prose, and the graph carries no structured file/evidence
    /// record on the row. A synced obligation therefore starts with an
    /// **empty** `evidence` set — including a subtask already `Completed`,
    /// whose state comes from the graph (the authoritative store) rather than
    /// from a ledger transition. Only [`Self::apply`] with a validated
    /// [`ObligationEvent::Complete`] records evidence.
    pub fn sync_from_graph(
        &mut self,
        graph: &TaskGraph,
        classify: impl Fn(&AgentId) -> Option<ObligationKind>,
    ) {
        self.obligations.clear();
        for subtask in graph.all_tasks() {
            let Some(kind) = classify(&subtask.role) else {
                continue;
            };
            let depends_on = graph
                .dependencies_of(&subtask.id)
                .into_iter()
                .map(|id| id.to_string())
                .collect::<Vec<_>>();
            let mut obligation =
                ExecutionObligation::new(subtask.id.to_string(), kind, &subtask.description);
            obligation.state = obligation_state_for_status(subtask.status);
            obligation.depends_on = depends_on;
            self.obligations.insert(obligation.id.clone(), obligation);
        }
    }

    pub fn get(&self, id: &str) -> Option<&ExecutionObligation> {
        self.obligations.get(id)
    }

    pub fn all(&self) -> Vec<&ExecutionObligation> {
        self.obligations.values().collect()
    }

    pub fn len(&self) -> usize {
        self.obligations.len()
    }

    pub fn is_empty(&self) -> bool {
        self.obligations.is_empty()
    }

    /// Apply one validated transition. Dependency-gated: completing an
    /// obligation whose `depends_on` obligations are still open is rejected,
    /// so verification cannot settle before the implementation it verifies.
    pub fn apply(
        &mut self,
        id: &str,
        event: ObligationEvent,
    ) -> Result<ObligationState, ObligationTransitionError> {
        let obligation = self.obligations.get(id).ok_or_else(|| ObligationTransitionError {
            obligation_id: id.to_owned(),
            from: ObligationState::Outstanding,
            reason: "unknown obligation id".to_owned(),
        })?;
        if matches!(event, ObligationEvent::Complete { .. }) {
            let open_deps = obligation
                .depends_on
                .iter()
                .filter(|dep_id| {
                    self.obligations.get(dep_id.as_str()).is_some_and(|dep| dep.state.is_open())
                })
                .cloned()
                .collect::<Vec<_>>();
            if !open_deps.is_empty() {
                return Err(ObligationTransitionError {
                    obligation_id: id.to_owned(),
                    from: obligation.state,
                    reason: format!("dependency obligations still open: {}", open_deps.join(", ")),
                });
            }
        }
        let next = try_transition(obligation.state, &event, obligation.kind, id)?;
        let stored = self.obligations.get_mut(id).ok_or_else(|| ObligationTransitionError {
            obligation_id: id.to_owned(),
            from: ObligationState::Outstanding,
            reason: "unknown obligation id".to_owned(),
        })?;
        stored.state = next;
        if let ObligationEvent::Complete { evidence } = &event {
            stored.evidence.extend(evidence.iter().cloned());
        }
        Ok(next)
    }

    /// Retire an obligation a reconsider/split/merge replaced. Shorthand for
    /// `apply(id, ObligationEvent::Supersede { .. })`.
    pub fn supersede(
        &mut self,
        id: &str,
        by: impl Into<String>,
    ) -> Result<ObligationState, ObligationTransitionError> {
        self.apply(id, ObligationEvent::Supersede { by: by.into() })
    }

    /// Whether any execution-kind obligation (investigate/implement/verify)
    /// is still open. Explanations never arm execution policy on their own.
    pub fn has_open_execution_work(&self) -> bool {
        self.obligations
            .values()
            .any(|ob| ob.kind.requires_execution_evidence() && ob.state.is_open())
    }

    /// Whether any *implementation* obligation is still open — the narrow
    /// predicate behind the dispatch guards.
    pub fn has_open_implementation(&self) -> bool {
        self.obligations
            .values()
            .any(|ob| matches!(ob.kind, ObligationKind::Implement) && ob.state.is_open())
    }

    /// Whether any *verification* obligation is still open — the narrow
    /// predicate behind the verification requirement.
    pub fn has_open_verification(&self) -> bool {
        self.obligations
            .values()
            .any(|ob| matches!(ob.kind, ObligationKind::Verify) && ob.state.is_open())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use concerto_core::ids::Ulid;
    use concerto_core::types::{SubTask, SubTaskStatus, TaskId};

    use crate::graph::Dependency;

    fn subtask_with_status(role: &str, status: SubTaskStatus) -> SubTask {
        let mut task = SubTask::new(Ulid::new(), AgentId::new(role), format!("{role} work"));
        task.status = status;
        task
    }

    fn implement_graph() -> (TaskGraph, TaskId, TaskId, TaskId) {
        let mut graph = TaskGraph::new();
        let investigate_id = TaskId::new();
        let implement_id = TaskId::new();
        let verify_id = TaskId::new();
        let mut investigate = subtask_with_status("researcher", SubTaskStatus::Completed);
        investigate.id = investigate_id;
        let mut implement = subtask_with_status("coder", SubTaskStatus::Pending);
        implement.id = implement_id;
        let mut verify = subtask_with_status("validator", SubTaskStatus::Pending);
        verify.id = verify_id;
        graph.add_root(investigate);
        graph.add_child(implement, investigate_id, Dependency::MustFinishBefore);
        graph.add_child(verify, implement_id, Dependency::MustFinishBefore);
        (graph, investigate_id, implement_id, verify_id)
    }

    fn classify(role: &AgentId) -> Option<ObligationKind> {
        match role.as_str() {
            "researcher" => Some(ObligationKind::Investigate),
            "coder" => Some(ObligationKind::Implement),
            "validator" => Some(ObligationKind::Verify),
            _ => None,
        }
    }

    fn synced_ledger(graph: &TaskGraph) -> ObligationLedger {
        let mut ledger = ObligationLedger::new();
        ledger.sync_from_graph(graph, classify);
        ledger
    }

    /// Ledger built straight from obligations — the test-side stand-in for
    /// ledger-only rows (production only ever derives the view from the graph).
    fn ledger_with(obligations: Vec<ExecutionObligation>) -> ObligationLedger {
        let mut ledger = ObligationLedger::new();
        for obligation in obligations {
            ledger.obligations.insert(obligation.id.clone(), obligation);
        }
        ledger
    }

    /// Count of obligations still constraining the run — the number a
    /// conversational turn must leave untouched.
    fn open_count(ledger: &ObligationLedger) -> usize {
        ledger.all().iter().filter(|ob| ob.state.is_open()).count()
    }

    // ── A. Conversational (1–5) ──────────────────────────────────────────

    #[test]
    fn a1_empty_graph_derives_zero_execution_obligations() {
        let graph = TaskGraph::new();
        let ledger = synced_ledger(&graph);
        assert!(ledger.is_empty(), "a conversational turn creates no obligations");
        assert!(!ledger.has_open_execution_work());
        assert!(!ledger.has_open_implementation());
        assert!(!ledger.has_open_verification());
    }

    #[test]
    fn a2_conversational_turn_leaves_outstanding_obligations_untouched() {
        let (graph, _, implement_id, _) = implement_graph();
        let mut ledger = synced_ledger(&graph);
        let open_before = open_count(&ledger);
        assert_eq!(open_before, 2, "implement + verify stay open, investigate is done");
        // A conversational turn mutates nothing: the graph is unchanged, so
        // the derived view rebuilt the way production rebuilds it is identical.
        ledger.sync_from_graph(&graph, classify);
        assert_eq!(
            open_count(&ledger),
            open_before,
            "prose must not reset, satisfy, or replace execution work"
        );
        assert_eq!(
            ledger.get(&implement_id.to_string()).map(|ob| ob.state),
            Some(ObligationState::Outstanding)
        );
    }

    #[test]
    fn a5_conversational_follow_up_adds_nothing_and_preserves_states() {
        let (graph, _, implement_id, _) = implement_graph();
        let mut ledger = synced_ledger(&graph);
        // A conversational follow-up declares no graph work, so the view
        // re-derived from the unchanged graph gains no obligation and loses
        // no state.
        ledger.sync_from_graph(&graph, classify);
        assert_eq!(ledger.len(), 3);
        assert_eq!(
            ledger.get(&implement_id.to_string()).map(|ob| ob.state),
            Some(ObligationState::Outstanding),
            "a follow-up with no new execution must not disturb existing states"
        );
        assert!(ledger.has_open_implementation());
    }

    // ── B. Enforcement (6–10) ────────────────────────────────────────────

    #[test]
    fn b6_open_implement_arms_dispatch_without_action_required_mode() {
        let (graph, _, _, _) = implement_graph();
        let ledger = synced_ledger(&graph);
        assert!(ledger.has_open_execution_work());
        assert!(
            dispatch_guard_arms(false, &ledger),
            "fresh implement work requires dispatch even in a prose-capable mode"
        );
        assert!(
            ledger.has_open_implementation(),
            "the open implement obligation is what arms the guard without the mode"
        );
    }

    #[test]
    fn b8_settled_implement_without_verification_keeps_verification_required() {
        let (mut graph, _, implement_id, _) = implement_graph();
        if let Some(task) = graph.get_mut(&implement_id) {
            task.status = SubTaskStatus::Completed;
        }
        let ledger = synced_ledger(&graph);
        assert!(!ledger.has_open_implementation(), "implement settled");
        assert!(
            ledger.has_open_verification(),
            "verification is still enforced after implement settles"
        );
    }

    #[test]
    fn b9_blocked_implement_stays_open_and_must_retry_first() {
        let mut ledger =
            ledger_with(vec![ExecutionObligation::new("impl-1", ObligationKind::Implement, "fix")]);
        assert_eq!(
            ledger.apply("impl-1", ObligationEvent::Block { reason: "waiting on dep".into() }),
            Ok(ObligationState::Blocked)
        );
        assert!(ledger.has_open_implementation(), "blocked work is unresolved, never droppable");
        let completed =
            ledger.apply("impl-1", ObligationEvent::Complete { evidence: vec!["event-1".into()] });
        assert!(completed.is_err(), "blocked work cannot complete without an explicit retry");
        assert_eq!(
            ledger.apply("impl-1", ObligationEvent::Retry),
            Ok(ObligationState::Outstanding)
        );
    }

    #[test]
    fn b10_failed_implement_rejects_completion_and_accepts_retry() {
        let mut ledger =
            ledger_with(vec![ExecutionObligation::new("impl-1", ObligationKind::Implement, "fix")]);
        assert_eq!(
            ledger.apply("impl-1", ObligationEvent::Fail { reason: "tests red".into() }),
            Ok(ObligationState::Failed)
        );
        assert!(ledger.has_open_implementation());
        assert!(
            ledger
                .apply("impl-1", ObligationEvent::Complete { evidence: vec!["e".into()] })
                .is_err(),
            "failed work must be retried before it can complete"
        );
        assert_eq!(
            ledger.apply("impl-1", ObligationEvent::Retry),
            Ok(ObligationState::Outstanding)
        );
    }

    // ── C. Mixed "fix X and explain" (11–15) ─────────────────────────────

    #[test]
    fn c12_explain_completes_in_prose_while_implement_stays_open() {
        let mut explain =
            ExecutionObligation::new("explain-1", ObligationKind::Explain, "explain fix");
        explain.depends_on = vec!["impl-1".into()];
        let mut ledger = ledger_with(vec![
            ExecutionObligation::new("impl-1", ObligationKind::Implement, "fix"),
            explain,
        ]);
        assert!(
            ledger.apply("explain-1", ObligationEvent::Complete { evidence: Vec::new() }).is_err(),
            "the explanation cannot settle before the work it explains"
        );
        assert_eq!(
            ledger.apply("impl-1", ObligationEvent::Complete { evidence: vec!["event-9".into()] }),
            Ok(ObligationState::Completed)
        );
        assert_eq!(
            ledger.apply("explain-1", ObligationEvent::Complete { evidence: Vec::new() }),
            Ok(ObligationState::Completed),
            "explanations settle in prose once the work they explain is evidenced"
        );
    }

    #[test]
    fn c13_chain_order_investigate_implement_verify_explain() {
        let (graph, investigate_id, implement_id, verify_id) = implement_graph();
        let ledger = synced_ledger(&graph);
        let verify = ledger.get(&verify_id.to_string()).expect("verify derived");
        assert_eq!(verify.kind, ObligationKind::Verify);
        assert_eq!(
            verify.depends_on,
            vec![implement_id.to_string()],
            "graph edges become the obligation chain order"
        );
        let implement = ledger.get(&implement_id.to_string()).expect("implement derived");
        assert_eq!(implement.depends_on, vec![investigate_id.to_string()]);
        assert!(ledger
            .get(&verify_id.to_string())
            .map(|ob| ob.state)
            .is_some_and(|state| state == ObligationState::Outstanding));
    }

    #[test]
    fn c14_incremental_completion_leaves_siblings_open() {
        let mut graph = TaskGraph::new();
        let first_id = TaskId::new();
        let second_id = TaskId::new();
        let mut first = subtask_with_status("coder", SubTaskStatus::Pending);
        first.id = first_id;
        let mut second = subtask_with_status("coder", SubTaskStatus::Pending);
        second.id = second_id;
        graph.add_root(first);
        graph.add_root(second);
        let mut ledger = synced_ledger(&graph);
        assert_eq!(
            ledger.apply(
                &first_id.to_string(),
                ObligationEvent::Complete { evidence: vec!["event-1".into()] }
            ),
            Ok(ObligationState::Completed)
        );
        assert!(ledger.has_open_implementation(), "one settled implement leaves its sibling armed");
        assert!(dispatch_guard_arms(false, &ledger));
    }

    #[test]
    fn c15_verification_evidence_settles_verify_empty_prose_does_not() {
        let (graph, _, implement_id, verify_id) = implement_graph();
        let mut ledger = synced_ledger(&graph);
        assert!(
            ledger
                .apply(&verify_id.to_string(), ObligationEvent::Complete { evidence: Vec::new() })
                .is_err(),
            "verification demands evidence, never bare prose"
        );
        assert_eq!(
            ledger.apply(
                &implement_id.to_string(),
                ObligationEvent::Complete { evidence: vec!["event-impl".into()] }
            ),
            Ok(ObligationState::Completed)
        );
        assert_eq!(
            ledger.apply(
                &verify_id.to_string(),
                ObligationEvent::Complete { evidence: vec!["cargo-test-pass".into()] }
            ),
            Ok(ObligationState::Completed)
        );
        assert!(!ledger.has_open_execution_work());
        assert!(!dispatch_guard_arms(false, &ledger));
    }

    // ── D. Lifecycle (16–20) ─────────────────────────────────────────────

    #[test]
    fn d16_checkpoint_round_trip_preserves_derived_obligations() {
        use crate::checkpoint::{
            build_checkpoint, restore_graph, CheckpointContext, CheckpointScope, CheckpointStage,
        };
        let (graph, _, implement_id, _) = implement_graph();
        let before = synced_ledger(&graph);
        let scope = CheckpointScope {
            run_id: Ulid::new(),
            session_id: Ulid::new(),
            root_task_id: TaskId::new(),
            project_id: "test".into(),
            objective: "fix".into(),
            objective_hash: "hash".into(),
            source_revision: None,
            sequence_num: 0,
        };
        let working_memory = concerto_core::memory::WorkingMemorySnapshot {
            id: Ulid::new(),
            session_id: Ulid::new(),
            decisions: Vec::new(),
            task_tree: Vec::new(),
            created_at: time::OffsetDateTime::now_utc(),
        };
        let checkpoint = build_checkpoint(
            &scope,
            CheckpointStage::Executing,
            None,
            &working_memory,
            &graph,
            &std::collections::HashMap::new(),
            0.0,
            0,
            &[],
            &[],
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
            &std::collections::HashMap::new(),
            &CheckpointContext::default(),
        );
        let json = serde_json::to_string(&checkpoint).expect("checkpoint serializes");
        let restored_checkpoint =
            crate::checkpoint::GraphCheckpoint::from_json(&json).expect("checkpoint loads");
        let restored = restore_graph(&restored_checkpoint).expect("graph restores");
        let after = synced_ledger(&restored);
        assert_eq!(
            after.get(&implement_id.to_string()).map(|ob| ob.state),
            before.get(&implement_id.to_string()).map(|ob| ob.state),
            "interrupt/resume must not lose obligation state"
        );
        assert!(after.has_open_implementation());
    }

    #[test]
    fn d17_interrupted_running_work_resumes_as_outstanding() {
        let mut graph = TaskGraph::new();
        let id = TaskId::new();
        let mut task = subtask_with_status("coder", SubTaskStatus::Running);
        task.id = id;
        graph.add_root(task);
        // The checkpoint restore maps in-flight Running back to Pending so a
        // crashed process can safely reschedule it; either way the derived
        // obligation stays open.
        let ledger = synced_ledger(&graph);
        assert_eq!(
            ledger.get(&id.to_string()).map(|ob| ob.state),
            Some(ObligationState::Outstanding)
        );
        assert!(ledger.has_open_implementation());
    }

    #[test]
    fn d18_cancelled_failed_work_retries_without_losing_the_obligation() {
        let mut ledger =
            ledger_with(vec![ExecutionObligation::new("impl-1", ObligationKind::Implement, "fix")]);
        assert_eq!(
            ledger.apply("impl-1", ObligationEvent::Fail { reason: "cancelled".into() }),
            Ok(ObligationState::Failed)
        );
        assert_eq!(ledger.len(), 1, "cancel preserves the obligation row for retry");
        assert_eq!(
            ledger.apply("impl-1", ObligationEvent::Retry),
            Ok(ObligationState::Outstanding)
        );
        assert!(ledger.has_open_implementation());
    }

    #[test]
    fn d20_terminal_states_reject_every_transition() {
        let mut ledger = ledger_with(vec![
            ExecutionObligation::new("done-1", ObligationKind::Implement, "done"),
            ExecutionObligation::new("old-1", ObligationKind::Investigate, "old"),
        ]);
        assert_eq!(
            ledger.apply("done-1", ObligationEvent::Complete { evidence: vec!["e".into()] }),
            Ok(ObligationState::Completed)
        );
        for event in [
            ObligationEvent::Retry,
            ObligationEvent::Block { reason: "x".into() },
            ObligationEvent::Complete { evidence: vec!["y".into()] },
        ] {
            assert!(ledger.apply("done-1", event).is_err(), "completed work is terminal");
        }
        assert_eq!(ledger.supersede("old-1", "reconsidered"), Ok(ObligationState::Superseded));
        assert!(
            ledger.apply("old-1", ObligationEvent::Retry).is_err(),
            "superseded work is terminal"
        );
        assert!(!ledger.has_open_execution_work());
    }

    // ── Declared obligations (missing transition) ──────────────────────

    #[test]
    fn declared_status_derives_outstanding_and_arms_execution() {
        use concerto_core::ids::Ulid;
        use concerto_core::types::{AgentId, SubTask, TaskId};

        assert_eq!(
            obligation_state_for_status(SubTaskStatus::Declared),
            ObligationState::Outstanding,
            "declared (not-yet-dispatched) work is Outstanding — enforceable before any dispatch"
        );
        // A graph holding a Declared implement node derives an open
        // Implement obligation even with an empty evidence set and pure
        // prose around it.
        let mut graph = TaskGraph::new();
        let id = TaskId::new();
        let mut declared = SubTask::new(Ulid::new(), AgentId::new("coder"), "fix the bug");
        declared.id = id;
        declared.status = SubTaskStatus::Declared;
        graph.add_root(declared);
        let mut ledger = ObligationLedger::new();
        ledger.sync_from_graph(&graph, |role| {
            if role.as_str() == "coder" {
                Some(ObligationKind::Implement)
            } else {
                None
            }
        });
        assert!(ledger.has_open_implementation());
        assert!(ledger.has_open_execution_work());
        assert!(dispatch_guard_arms(false, &ledger));
    }

    /// H. A conversational follow-up adds no obligation and resets none: a
    /// direct answer with a Declared obligation standing keeps every state,
    /// keeps the dispatch armed, and records no dispatch of its own.
    #[test]
    fn h_follow_up_direct_answer_preserves_declared_obligations() {
        use concerto_core::ids::Ulid;
        use concerto_core::types::{AgentId, SubTask, TaskId};

        let mut graph = TaskGraph::new();
        let id = TaskId::new();
        let mut declared = SubTask::new(Ulid::new(), AgentId::new("coder"), "fix the bug");
        declared.id = id;
        declared.status = SubTaskStatus::Declared;
        graph.add_root(declared);
        let mut ledger = synced_ledger(&graph);
        // The follow-up is a direct answer: no new execution, no dispatch —
        // the graph is untouched, so the view re-derived from it the way the
        // coordinator's guard derives it is identical.
        let open_before = open_count(&ledger);
        ledger.sync_from_graph(&graph, classify);
        assert_eq!(
            open_count(&ledger),
            open_before,
            "prose must not reset, satisfy, or replace declared work"
        );
        assert_eq!(
            ledger.get(&id.to_string()).map(|ob| ob.state),
            Some(ObligationState::Outstanding),
            "the declared obligation is intact after the follow-up"
        );
        assert!(ledger.has_open_implementation());
        assert!(dispatch_guard_arms(false, &ledger));
    }

    // ── E. Evidence provenance ─────────────────────────────────────────────

    /// Sync carries no prose: a subtask completed with only an agent
    /// `deliverable` derives an obligation with an **empty** evidence set.
    /// Prose is never evidence, the graph row holds no structured
    /// file/evidence record to cite, and the graph status alone settles the
    /// state — only a validated `Complete` event fills `evidence`.
    #[test]
    fn e21_synced_completed_prose_only_subtask_has_empty_evidence() {
        let mut graph = TaskGraph::new();
        let id = TaskId::new();
        let mut completed = subtask_with_status("coder", SubTaskStatus::Completed);
        completed.id = id;
        completed.deliverable = Some("fixed it; summary of the fix in prose".to_owned());
        graph.add_root(completed);

        let ledger = synced_ledger(&graph);
        let obligation =
            ledger.get(&id.to_string()).expect("the completed subtask derives an obligation");
        assert_eq!(
            obligation.state,
            ObligationState::Completed,
            "the graph status is authoritative for a synced obligation"
        );
        assert!(
            obligation.evidence.is_empty(),
            "agent prose is never evidence: {:?}",
            obligation.evidence
        );
        assert_eq!(
            graph.get(&id).and_then(|task| task.deliverable.as_deref()),
            Some("fixed it; summary of the fix in prose"),
            "the prose stays on the graph row; the ledger cites structured evidence only"
        );
    }
}
