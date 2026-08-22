//! Agent state machine — the lifecycle states an agent passes through
//! during a single task execution.

use concerto_config::BlueprintFacade;
use concerto_core::event::{EventBus, EventKind};
use concerto_core::ids::Ulid;
use concerto_core::types::{AgentId, AgentStage, TaskId};
use concerto_core::OrchestratorError;
use serde::{Deserialize, Serialize};

use crate::hash::SubTaskHasher;

/// The current state of an agent within a single `AgentLoop::run` cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum AgentState {
    Idle,
    Planning,
    ToolSelection,
    AwaitingApproval,
    Executing,
    Observing,
    Evaluating,
    Done,
    Completed,
    Failed,
    Cancelled,
}

impl AgentState {
    /// Human-readable label for event emission.
    pub fn as_str(&self) -> &'static str {
        match self {
            AgentState::Idle => "idle",
            AgentState::Planning => "planning",
            AgentState::ToolSelection => "tool_selection",
            AgentState::AwaitingApproval => "awaiting_approval",
            AgentState::Executing => "executing",
            AgentState::Observing => "observing",
            AgentState::Evaluating => "evaluating",
            AgentState::Done => "done",
            AgentState::Completed => "completed",
            AgentState::Failed => "failed",
            AgentState::Cancelled => "cancelled",
        }
    }
}

// ---------------------------------------------------------------------------
// Phase 5: Multi-agent orchestration cycle detection state
// ---------------------------------------------------------------------------

/// Tracks multi-agent orchestration state for cycle detection.
///
/// Maintains a sequence of `(AgentId, task_hash)` entries and detects
/// two patterns:
/// - **Rule A**: same `(role, hash)` 3× without progress.
/// - **Rule B**: a gate-stage agent repeats the same issue 2× without
///   progress. Keyed on the stage tag of the **gate being executed** via the
///   resolved blueprint's gate classification (ADR-58 P2+P3 R11/F3/F5), not a
///   `reviewer` role id and not the agent's registered stage — the fallback
///   sentinel is never registered, so `stage_of` alone would silently disable
///   the rule when a gate renders the coordinator persona. Without an attached
///   facade the legacy `AgentStage::is_review` classification applies
///   unchanged (ADR-35), keeping the unit-test surface byte-identical.
#[derive(Clone)]
pub struct OrchestratorState {
    sequence: Vec<(AgentId, String)>,
    bus: Option<EventBus>,
    facade: Option<BlueprintFacade>,
}

impl OrchestratorState {
    /// Create a fresh state without an event bus.
    pub fn new() -> Self {
        Self { sequence: Vec::new(), bus: None, facade: None }
    }

    /// Create a state that publishes events on cycle detection.
    pub fn with_bus(bus: EventBus) -> Self {
        Self { sequence: Vec::new(), bus: Some(bus), facade: None }
    }

    /// Attach the resolved-blueprint facade so Rule B keys on the gate kind
    /// of the stage being executed (ADR-58 P2+P3 R11/F3). `None` keeps the
    /// legacy `AgentStage::is_review` classification.
    pub fn with_blueprint_facade(mut self, facade: Option<BlueprintFacade>) -> Self {
        self.facade = facade;
        self
    }

    /// Record a `(role, description)` for cycle detection.
    ///
    /// `stage` is the recording agent's declared stage tag resolved from the
    /// registry (ADR-35); Rule B fires only for review-stage agents,
    /// independent of the role id. `task_id` is the task being checked — it
    /// is emitted in the
    /// `OrchestratorCycleDetected` event so downstream consumers (UI,
    /// audit, event log) can correlate the cycle with the correct task.
    /// `has_progress` should be `true` if the agent produced any net
    /// file change (computed by the caller, e.g. via
    /// [`FileDeltaTracker`](crate::delta::FileDeltaTracker)).
    #[allow(clippy::too_many_arguments)]
    pub fn record(
        &mut self,
        session_id: Ulid,
        task_id: TaskId,
        role: AgentId,
        stage: Option<AgentStage>,
        description: &str,
        dependencies: &[TaskId],
        has_progress: bool,
    ) -> Result<(), OrchestratorError> {
        let hash = SubTaskHasher::compute(description, dependencies);

        // Progress resets the cycle counter.
        if has_progress {
            self.sequence.clear();
            return Ok(());
        }

        self.sequence.push((role.clone(), hash.clone()));
        let len = self.sequence.len();

        // Helper: emit cycle-detected event on the bus if available.
        let emit = |bus: &Option<EventBus>, task_id: TaskId| {
            if let Some(b) = bus {
                let seq: Vec<String> = self.sequence.iter().map(|(_, h)| h.clone()).collect();
                let _ = b.publish_for_session(
                    session_id,
                    task_id.0,
                    EventKind::OrchestratorCycleDetected { task_id, sequence: seq },
                );
            }
        };

        // Rule B: a gate-stage agent repeats the same issue twice without
        // progress. Keyed on the stage tag of the gate being executed
        // (ADR-58 P2+P3 R11/F3): the resolved blueprint's gate classification
        // covers custom Review/Acceptance-kind tags, so the rule no longer
        // depends on the `reviewer` role id or the `review` tag literal. The
        // previous sequence entry must be the same agent reporting the same
        // issue; comparing `prev.0 == role` is behavior-identical to the old
        // `== "reviewer"` comparison for the default config, where the only
        // review-stage agent is the `reviewer` seed.
        let gate_stage = stage.as_ref().is_some_and(|tag| match &self.facade {
            Some(facade) => facade.is_gate(tag.as_str()),
            None => tag.is_review(),
        });
        if gate_stage && len >= 2 {
            let prev = &self.sequence[len - 2];
            if prev.0 == role && prev.1 == hash {
                emit(&self.bus, task_id);
                return Err(OrchestratorError::CycleDetected {
                    tool_name: "reviewer".into(),
                    count: 2,
                });
            }
        }

        // Rule A: same (role, hash) three times in a row.
        if len >= 3 {
            let last = &self.sequence[len - 1];
            if self.sequence[len - 2] == *last && self.sequence[len - 3] == *last {
                emit(&self.bus, task_id);
                return Err(OrchestratorError::CycleDetected {
                    tool_name: "orchestrator".into(),
                    count: 3,
                });
            }
        }

        Ok(())
    }

    /// Reset the sequence for a new task.
    pub fn reset(&mut self) {
        self.sequence.clear();
    }

    /// Force a cycle reset (for user "Continue" action).
    pub fn clear_cycle(&mut self) {
        self.reset();
    }
}

impl Default for OrchestratorState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_resets_sequence() {
        let mut state = OrchestratorState::new();
        let session_id = Ulid::new();
        let tid = TaskId::new();
        let deps = [TaskId::new()];
        assert!(state
            .record(session_id, tid, AgentId::new("coder"), None, "do thing", &deps, false)
            .is_ok());
        assert!(state
            .record(session_id, tid, AgentId::new("coder"), None, "do thing", &deps, true)
            .is_ok());
        // After progress, sequence is empty — no cycle should fire.
        assert!(state
            .record(session_id, tid, AgentId::new("coder"), None, "do thing", &deps, false)
            .is_ok());
    }

    #[test]
    fn rule_a_three_repeats_detected() {
        let mut state = OrchestratorState::new();
        let session_id = Ulid::new();
        let tid = TaskId::new();
        let deps = [TaskId::new()];
        assert!(state
            .record(session_id, tid, AgentId::new("coder"), None, "fix bug", &deps, false)
            .is_ok());
        assert!(state
            .record(session_id, tid, AgentId::new("coder"), None, "fix bug", &deps, false)
            .is_ok());
        let err =
            state.record(session_id, tid, AgentId::new("coder"), None, "fix bug", &deps, false);
        assert!(err.is_err());
        assert!(matches!(err.unwrap_err(), OrchestratorError::CycleDetected { .. }));
    }

    #[test]
    fn rule_b_reviewer_repeat_detected() {
        let mut state = OrchestratorState::new();
        let session_id = Ulid::new();
        let tid = TaskId::new();
        let deps = [TaskId::new()];
        let review_stage = Some(AgentStage::new(AgentStage::REVIEW));
        assert!(state
            .record(
                session_id,
                tid,
                AgentId::new("reviewer"),
                review_stage.clone(),
                "missing error handling",
                &deps,
                false,
            )
            .is_ok());
        let err = state.record(
            session_id,
            tid,
            AgentId::new("reviewer"),
            review_stage,
            "missing error handling",
            &deps,
            false,
        );
        assert!(err.is_err());
    }

    #[test]
    fn rule_b_keys_on_gate_kind_tag_via_facade() {
        // ADR-58 P2+P3 (R11/F3): with the resolved-blueprint facade attached,
        // Rule B fires for a role staffed at a custom Review-kind gate tag —
        // not just the `review` tag literal / `reviewer` id (ADR-35).
        use concerto_config::blueprint::{
            Blueprint, CapabilityMask, PipelineDef, StageCondition, StageDef, StageFlags, StageKind,
        };
        use concerto_config::{BlueprintFacade, ResolvedBlueprint, ResolvedStage};
        use std::collections::HashMap;

        let gate_def = StageDef {
            tag: "approve".into(),
            label: "Approve".into(),
            kind: StageKind::Review.as_str().to_string(),
            version: 1,
            flags: StageFlags::default(),
            condition: StageCondition::Always,
            max_cycles: None,
            feed: None,
            primary: false,
            agents: vec!["approver".into()],
            fallback: None,
            files: None,
        };
        let resolved = ResolvedBlueprint {
            blueprint: Blueprint {
                schema_version: 1,
                name: "test".into(),
                description: None,
                pipeline: PipelineDef { stages: vec![gate_def.clone()] },
                relationships: Vec::new(),
            },
            stages: vec![ResolvedStage {
                def: gate_def.clone(),
                effective_capabilities: CapabilityMask::default(),
                effective_feed: None,
            }],
            feed_map: HashMap::new(),
            relationship_defaults: Vec::new(),
        };
        let facade = BlueprintFacade::new(&resolved);

        let session_id = Ulid::new();
        let tid = TaskId::new();
        let deps = [TaskId::new()];
        let gate_stage = Some(AgentStage::new("approve"));

        // Without a facade the rule keeps the legacy `AgentStage::is_review`
        // classification, which does not recognize the custom gate tag.
        let mut legacy = OrchestratorState::new();
        assert!(legacy
            .record(
                session_id,
                tid,
                AgentId::new("approver"),
                gate_stage.clone(),
                "same issue",
                &deps,
                false,
            )
            .is_ok());
        assert!(legacy
            .record(
                session_id,
                tid,
                AgentId::new("approver"),
                gate_stage.clone(),
                "same issue",
                &deps,
                false,
            )
            .is_ok());

        // With the facade, the Review-kind gate classification trips Rule B
        // exactly like the reviewer on the default pipeline.
        let mut gated = OrchestratorState::new().with_blueprint_facade(Some(facade));
        assert!(gated
            .record(
                session_id,
                tid,
                AgentId::new("approver"),
                gate_stage.clone(),
                "same issue",
                &deps,
                false,
            )
            .is_ok());
        let err = gated.record(
            session_id,
            tid,
            AgentId::new("approver"),
            gate_stage,
            "same issue",
            &deps,
            false,
        );
        assert!(err.is_err());
        assert!(matches!(err.unwrap_err(), OrchestratorError::CycleDetected { .. }));
    }

    /// ADR-35: Rule B is keyed on the review stage tag, so a *custom*
    /// review-stage role whose id is not "reviewer" triggers it too.
    #[test]
    fn rule_b_custom_review_stage_role_detected() {
        let mut state = OrchestratorState::new();
        let session_id = Ulid::new();
        let tid = TaskId::new();
        let deps = [TaskId::new()];
        let review_stage = Some(AgentStage::new(AgentStage::REVIEW));
        assert!(state
            .record(
                session_id,
                tid,
                AgentId::new("code-reviewer"),
                review_stage.clone(),
                "missing error handling",
                &deps,
                false,
            )
            .is_ok());
        let err = state.record(
            session_id,
            tid,
            AgentId::new("code-reviewer"),
            review_stage,
            "missing error handling",
            &deps,
            false,
        );
        let err =
            err.expect_err("a custom review-stage role repeating the same issue must trip Rule B");
        assert!(
            matches!(err, OrchestratorError::CycleDetected { tool_name, count: 2 } if tool_name == "reviewer")
        );
    }

    #[test]
    fn different_tasks_independent_state() {
        let mut state = OrchestratorState::new();
        let tid1 = TaskId::new();
        let tid2 = TaskId::new();
        let deps = [TaskId::new()];
        // Task 1: three identical consecutive calls → cycle detected.
        assert!(state
            .record(Ulid::new(), tid1, AgentId::new("coder"), None, "fix", &deps, false)
            .is_ok());
        assert!(state
            .record(Ulid::new(), tid1, AgentId::new("coder"), None, "fix", &deps, false)
            .is_ok());
        let err = state.record(Ulid::new(), tid1, AgentId::new("coder"), None, "fix", &deps, false);
        assert!(err.is_err(), "task1 should hit cycle limit (3x same role/description)");

        // After reset, task 2 can make fresh calls without interference.
        state.reset();
        assert!(
            state
                .record(Ulid::new(), tid2, AgentId::new("coder"), None, "fix", &deps, false)
                .is_ok(),
            "task2 should be fine after reset"
        );
    }

    #[test]
    fn reset_clears_sequence() {
        let mut state = OrchestratorState::new();
        let tid = TaskId::new();
        let deps = [TaskId::new()];
        assert!(state
            .record(Ulid::new(), tid, AgentId::new("coder"), None, "x", &deps, false)
            .is_ok());
        assert!(state
            .record(Ulid::new(), tid, AgentId::new("coder"), None, "x", &deps, false)
            .is_ok());
        state.reset();
        assert!(state
            .record(Ulid::new(), tid, AgentId::new("coder"), None, "x", &deps, false)
            .is_ok());
    }

    #[test]
    fn clear_cycle_works() {
        let mut state = OrchestratorState::new();
        let tid = TaskId::new();
        let deps = [TaskId::new()];
        assert!(state
            .record(Ulid::new(), tid, AgentId::new("coder"), None, "x", &deps, false)
            .is_ok());
        assert!(state
            .record(Ulid::new(), tid, AgentId::new("coder"), None, "x", &deps, false)
            .is_ok());
        state.clear_cycle();
        assert!(state
            .record(Ulid::new(), tid, AgentId::new("coder"), None, "x", &deps, false)
            .is_ok());
    }

    #[test]
    fn different_content_does_not_trigger_cycle() {
        let mut state = OrchestratorState::new();
        let tid = TaskId::new();
        let deps = [TaskId::new()];
        assert!(state
            .record(Ulid::new(), tid, AgentId::new("coder"), None, "fix a", &deps, false)
            .is_ok());
        assert!(state
            .record(Ulid::new(), tid, AgentId::new("coder"), None, "fix b", &deps, false)
            .is_ok());
        assert!(state
            .record(Ulid::new(), tid, AgentId::new("coder"), None, "fix c", &deps, false)
            .is_ok());
    }

    #[test]
    fn different_role_does_not_trigger_cycle() {
        let mut state = OrchestratorState::new();
        let tid = TaskId::new();
        let deps = [TaskId::new()];
        assert!(state
            .record(Ulid::new(), tid, AgentId::new("coder"), None, "fix", &deps, false)
            .is_ok());
        assert!(state
            .record(Ulid::new(), tid, AgentId::new("architect"), None, "fix", &deps, false)
            .is_ok());
        assert!(state
            .record(Ulid::new(), tid, AgentId::new("reviewer"), None, "fix", &deps, false)
            .is_ok());
    }

    #[test]
    fn agent_state_as_str_returns_labels() {
        assert_eq!(AgentState::Idle.as_str(), "idle");
        assert_eq!(AgentState::Planning.as_str(), "planning");
        assert_eq!(AgentState::Executing.as_str(), "executing");
        assert_eq!(AgentState::Done.as_str(), "done");
        assert_eq!(AgentState::Failed.as_str(), "failed");
        assert_eq!(AgentState::Cancelled.as_str(), "cancelled");
    }

    #[test]
    fn with_bus_creates_state_with_bus() {
        let bus = EventBus::default();
        let state = OrchestratorState::with_bus(bus);
        assert!(state.bus.is_some());
    }

    #[test]
    fn default_is_empty() {
        let state = OrchestratorState::default();
        assert!(state.sequence.is_empty());
        assert!(state.bus.is_none());
    }

    /// Rule A should NOT fire when different roles interleave with the
    /// same description — the sequence must match the exact (role, hash) pair.
    #[test]
    fn rule_a_ignores_interleaved_roles() {
        let mut state = OrchestratorState::new();
        let tid = TaskId::new();
        let deps = [TaskId::new()];
        // Interleave: Coder, Architect, Coder, Architect, Coder — the Coder
        // entries are not three consecutive identical (role, hash) pairs.
        assert!(state
            .record(Ulid::new(), tid, AgentId::new("coder"), None, "fix", &deps, false)
            .is_ok());
        assert!(state
            .record(Ulid::new(), tid, AgentId::new("architect"), None, "fix", &deps, false)
            .is_ok());
        assert!(state
            .record(Ulid::new(), tid, AgentId::new("coder"), None, "fix", &deps, false)
            .is_ok());
        assert!(state
            .record(Ulid::new(), tid, AgentId::new("architect"), None, "fix", &deps, false)
            .is_ok());
        // This third Coder entry makes three total but not consecutive — OK.
        assert!(state
            .record(Ulid::new(), tid, AgentId::new("coder"), None, "fix", &deps, false)
            .is_ok());
    }

    /// Progress should reset the sequence even after multiple records.
    #[test]
    fn progress_resets_after_multiple_records() {
        let mut state = OrchestratorState::new();
        let tid = TaskId::new();
        let deps = [TaskId::new()];
        // Build up 3 records without progress.
        assert!(state
            .record(Ulid::new(), tid, AgentId::new("coder"), None, "fix", &deps, false)
            .is_ok());
        assert!(state
            .record(Ulid::new(), tid, AgentId::new("coder"), None, "fix", &deps, false)
            .is_ok());
        assert!(state
            .record(Ulid::new(), tid, AgentId::new("coder"), None, "fix", &deps, false)
            .is_err());
        // Reset by recording progress.
        state.reset();
        assert!(state
            .record(Ulid::new(), tid, AgentId::new("coder"), None, "fix", &deps, false)
            .is_ok());
    }

    /// `new()` creates an empty state without a bus.
    #[test]
    fn state_new_is_empty() {
        let state = OrchestratorState::new();
        assert!(state.sequence.is_empty());
        assert!(state.bus.is_none());
    }
}
