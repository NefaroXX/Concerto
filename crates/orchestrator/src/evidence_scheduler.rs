//! ADR-65 §6 — deterministic, model-free evidence-driven scheduling.
//!
//! This module replaces the fixed `design → research → implement` fallback
//! pipeline (ADR-65 Context, fourth failure: "deterministic fake
//! coordination"). When the planner's JSON output fails to parse, the
//! coordinator no longer walks a hardcoded sequence; instead it derives the
//! unmet needs from EVIDENCE and schedules only what the evidence justifies,
//! among the currently registered agents (ADR-58: missing stages are simply
//! not candidates — a stage without a registered agent is never called).
//!
//! The scheduler is a PURE function: no model, no I/O, no cancellation token.
//! Every input is gathered by the caller (snapshot record, verifier verdict,
//! observation facts with real event ids, registry candidates) and every
//! output is a deterministic function of that state — the same
//! [`EvidenceState`] always yields the same [`DispatchPlan`]. The caller
//! threads the `CancellationToken` around the calls (evidence gathering and
//! dispatch), never inside.
//!
//! # Decision rules (deterministic priority, first match wins)
//!
//! The rules below implement ADR-65 §6 verbatim. Letters reference the
//! Phase-6 decision rules; §6's four bullets are the same needs viewed as a
//! menu ("no agent is called because its stage exists").
//!
//! 1. **Doc Quarantined (rule d).** The doc is advisory; the machine-checkable
//!    quarantine reasons decide:
//!    - grounding-fixable reasons (`UNGROUNDED_PATH` / `NO_OBSERVATIONS`) with
//!      an exploration dispatch still available → Exploration step first
//!      (grounding), then Implement **without** the doc contract. The
//!      quarantine persists as advisory context; the doc is never silently
//!      re-activated.
//!    - `TREE_CONFLICT` → Implement without the doc contract ("reality wins
//!      in claim validation", §5): the disk is the contract.
//!    - anything else (exploration exhausted / no explorer / unfixable
//!      reasons) → Implement without the doc contract, fail-soft.
//! 2. **Evidence sufficient (rule a).** Snapshot present and observations
//!    recorded, no pending quarantine → a single Implement step. No architect,
//!    no researcher. The contract rides the implement step only when the doc
//!    is Active (rules e/f below are the doc-shaped cases of the same
//!    outcome).
//! 3. **Evidence missing (rule b).** No snapshot record, or zero in-scope
//!    observations → the NEXT STEP ONLY: an Exploration step whose
//!    `required_output` is a grounded fact inventory (tool reads only). The
//!    caller re-consults the scheduler after the exploration's facts land
//!    (bounded, deterministic loop — `exploration_attempted` is explicit
//!    input, so the scheduler itself can never loop).
//! 4. **Design undecided (rule c).** No doc exists and the planner produced
//!    no decidable plan → a Design step (any design-capable agent); the same
//!    deterministic Phase-5 verifier resolves its output afterwards and the
//!    doc binds only when Active (§5). With no design-capable candidate the
//!    stage is skipped (acceptance 6), not called.
//! 5. **Doc Active (rule e)** → Implement with the doc contract.
//! 6. **Doc Skipped (rule f)** → Implement directly ("design is the repo").
//!
//! # Candidate selection
//!
//! Among the registered agents matching the required capability tag, the
//! deterministic tie-break is the caller-assigned `order` (registration
//! order; the coordinator ranks candidates lexicographically by agent id to
//! match `first_agent_for_stage`). A step whose capability class has no
//! registered candidate is DROPPED from the plan — never scheduled against a
//! missing stage (acceptance 6).
//!
//! # Evidence ids
//!
//! `supporting_evidence_ids` are the real event ids the scheduler consumed
//! from the evidence pool (observation facts, the snapshot event, the
//! design-doc claim and its verdict decision). The scheduler never fabricates
//! an id: evidence that carries no id is cited by omission. The append side
//! rejects Decision events citing unknown ids (ADR-65 §1, acceptance 8).

use std::collections::BTreeSet;

/// The capability classes the scheduler dispatches against.
///
/// The coordinator maps these onto the registry's real capability-tag
/// representation — the ADR-58 lifecycle stage tags (`AgentStage`): `Explore`
/// ↔ the `research` stage tag, `Design` ↔ the `design` stage tag, `Implement`
/// ↔ the resolved primary `Execution` stage tag (`implement` on the default
/// blueprint). Stage tags are config data; a tag with no registered agent is
/// not a candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Capability {
    /// Ground workspace evidence with tool reads (research-stage agents).
    Explore,
    /// Produce a DesignDoc (design-stage agents).
    Design,
    /// Carry implementation work (primary Execution-stage agents).
    Implement,
}

/// One registered scheduling candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// The registered agent id (never the reserved coordinator id unless the
    /// coordinator itself stands in as the implement-capable candidate).
    pub agent_id: String,
    /// Capability classes this agent matches.
    pub capabilities: BTreeSet<Capability>,
    /// Stable tie-break rank assigned by the caller (registration order). The
    /// scheduler picks the lowest-ranked candidate per capability class.
    pub order: usize,
}

/// One observed workspace fact, cited by its REAL whiteboard event id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observation {
    /// The `event_id` of the observation (from the evidence store; never
    /// fabricated — rows without an id are omitted by the caller).
    pub event_id: String,
    /// Canonical project-relative path the fact observed.
    pub path: String,
}

/// Machine-checkable quarantine reason codes (mirrors the Phase-5 verifier's
/// reason taxonomy, collapsed to the classes the scheduler decides on).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuarantineCode {
    /// A proposed path resolves to nothing observed — grounding may fix it.
    UngroundedPath,
    /// The proposal conflicts with an observed tree shape — reality wins.
    TreeConflict,
    /// A non-empty doc authored with zero attributed reads — grounding may
    /// fix it.
    NoObservations,
    /// Any other reason (weight-zero informational codes, degraded
    /// evidence-unavailable verdicts) — never fixable by exploration.
    Other,
}

/// Where the DesignDoc claim stands when the scheduler runs (ADR-65 §5
/// lifecycle, as resolved by the deterministic verifier).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DocResolution {
    /// No doc exists (design genuinely undecided).
    Undecided,
    /// The doc binds (Verified, or a human-approved seeded doc): its contract
    /// is consumed.
    Active {
        /// The grounded contract paths (verifier `contract_paths`).
        contract_paths: Vec<String>,
        /// Real event ids establishing the resolution (doc claim + verdict
        /// decision). Omitted ids stay omitted — never fabricated.
        evidence_ids: Vec<String>,
    },
    /// The doc was quarantined; the codes decide revise/skip/proceed.
    Quarantined {
        /// Every quarantine reason code, deterministic order.
        codes: Vec<QuarantineCode>,
        /// Real event ids establishing the quarantine (doc claim + verdict
        /// decision).
        evidence_ids: Vec<String>,
    },
    /// The doc was skipped (empty claim): the design is the repo.
    Skipped {
        /// Real event ids establishing the skip.
        evidence_ids: Vec<String>,
    },
}

/// Everything the scheduler needs — gathered by the caller from the runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvidenceState {
    /// The run objective text (free-form; never parsed — no language
    /// detection).
    pub objective: String,
    /// Whether the planner produced no decidable plan (always `true` on the
    /// fallback path this module serves; rule (c) requires it).
    pub planner_failed: bool,
    /// Whether a workspace snapshot record exists (the bootstrap inventory).
    pub snapshot_present: bool,
    /// The REAL event id of the `WorkspaceSnapshot` log row, when known.
    pub snapshot_event_id: Option<String>,
    /// In-scope observed facts (newest first, caller-ordered), each cited by
    /// its real event id.
    pub observations: Vec<Observation>,
    /// Where the DesignDoc claim stands.
    pub doc: DocResolution,
    /// The registered candidates (caller-assigned stable order).
    pub candidates: Vec<Candidate>,
    /// Whether an exploration dispatch was already attempted for this
    /// decision chain. Explicit loop state: the caller sets it after running
    /// an exploration step, which bounds the re-evaluation loop by
    /// construction.
    pub exploration_attempted: bool,
}

/// Why a step was scheduled — machine reason codes recorded on the `Decision`
/// whiteboard event (ADR-65 §6: `selected_agent, reason, required_output,
/// supporting_evidence_ids`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecisionReason {
    /// Rule (a): evidence sufficient — implement directly, no architect, no
    /// researcher.
    EvidenceSufficientImplement,
    /// Rule (b): workspace evidence missing — exploration is the next step
    /// only; the caller re-consults after the facts land.
    EvidenceGapExplore,
    /// Rule (c): design genuinely undecided — design-capable agent dispatches.
    DesignUndecidedDesign,
    /// Rule (c) skip: no design-capable candidate — implementation proceeds
    /// without a design stage (acceptance 6: never call a missing stage).
    DesignUndecidedNoDesigner,
    /// Rule (c): implementation following the deferred design step; the doc
    /// binds only when the verifier marks it Active.
    ImplementAfterDesign,
    /// Rule (d): quarantine grounding-fixable — exploration grounds first.
    QuarantinedGroundingExplore,
    /// Rule (d): the doc stays advisory — proceed without the doc contract
    /// (grounding exhausted, unfixable, or no explorer).
    QuarantinedProceedWithoutDoc,
    /// Rule (d): `TREE_CONFLICT` — reality wins; implement without the doc
    /// contract.
    QuarantinedTreeConflictProceedWithoutDoc,
    /// Rule (e): the doc is Active — implement with the contract.
    DocActiveImplementContract,
    /// Rule (f): the doc was skipped — implement directly ("design is the
    /// repo").
    DocSkippedImplement,
}

impl DecisionReason {
    /// The machine-stable kebab-case reason code recorded on the Decision
    /// event payload.
    pub fn code(self) -> &'static str {
        match self {
            Self::EvidenceSufficientImplement => "evidence-sufficient-implement",
            Self::EvidenceGapExplore => "evidence-gap-explore",
            Self::DesignUndecidedDesign => "design-undecided-design",
            Self::DesignUndecidedNoDesigner => "design-undecided-no-designer-implement",
            Self::ImplementAfterDesign => "implement-after-design",
            Self::QuarantinedGroundingExplore => "quarantined-grounding-explore",
            Self::QuarantinedProceedWithoutDoc => "quarantined-proceed-without-doc",
            Self::QuarantinedTreeConflictProceedWithoutDoc => {
                "quarantined-tree-conflict-implement-without-contract"
            }
            Self::DocActiveImplementContract => "doc-active-implement-with-contract",
            Self::DocSkippedImplement => "doc-skipped-implement",
        }
    }

    /// Whether the reason is doc-driven (the DesignDoc claim's events are the
    /// natural `causation` for the Decision event).
    pub fn is_doc_driven(self) -> bool {
        matches!(
            self,
            Self::QuarantinedGroundingExplore
                | Self::QuarantinedProceedWithoutDoc
                | Self::QuarantinedTreeConflictProceedWithoutDoc
                | Self::DocActiveImplementContract
                | Self::DocSkippedImplement
        )
    }
}

/// One dispatch the scheduler decided on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DispatchStep {
    /// The selected agent (a registered candidate; never a missing stage).
    pub candidate_agent_id: String,
    /// The capability class the step fulfills.
    pub capability: Capability,
    /// What the dispatch must produce (machine + human readable).
    pub required_output: String,
    /// The machine reason code for the dispatch.
    pub reason: DecisionReason,
    /// The real event ids the decision consumed from the evidence pool.
    /// Omitted ids stay omitted — never fabricated.
    pub supporting_evidence_ids: Vec<String>,
    /// Whether an Active doc's contract binds this implement step (Phase-5
    /// enforcement consumes it as today's expected artifacts).
    pub bind_contract: bool,
}

/// The ordered dispatch plan for the current evidence state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DispatchPlan {
    /// Steps in dispatch order. Every terminal plan ends in an implement
    /// step when an implement-capable candidate exists; a plan that is a
    /// single Exploration step is the rule-(b) deferred case — the caller
    /// re-consults the scheduler after the facts land.
    pub steps: Vec<DispatchStep>,
}

/// Upper bound on the evidence ids cited per Decision event, so payloads stay
/// bounded on heavily-observed workspaces.
const MAX_SUPPORTING_EVIDENCE_IDS: usize = 8;

/// The deterministic candidate for a capability class: lowest caller-assigned
/// `order` wins (registration order; the coordinator ranks candidates
/// lexicographically by id, matching `first_agent_for_stage`).
fn first_candidate(state: &EvidenceState, capability: Capability) -> Option<&Candidate> {
    state
        .candidates
        .iter()
        .filter(|candidate| candidate.capabilities.contains(&capability))
        .min_by_key(|candidate| candidate.order)
}

/// Whether the workspace evidence is missing for the objective (rule b):
/// no snapshot record, or a snapshot with zero in-scope recorded facts.
/// The objective's scope is the project root itself — the only honest,
/// language-free scoping — so recorded facts for the root ARE the objective's
/// observations.
fn evidence_missing(state: &EvidenceState) -> bool {
    !state.snapshot_present || state.observations.is_empty()
}

/// The evidence ids establishing sufficiency: the snapshot event (when known)
/// plus the recorded observations, capped for payload size. Real ids only.
fn sufficient_evidence_ids(state: &EvidenceState) -> Vec<String> {
    let mut ids = Vec::new();
    if let Some(snapshot) = state.snapshot_event_id.clone() {
        ids.push(snapshot);
    }
    ids.extend(state.observations.iter().map(|observation| observation.event_id.clone()));
    ids.truncate(MAX_SUPPORTING_EVIDENCE_IDS);
    ids
}

fn explore_step(
    candidate: &Candidate,
    reason: DecisionReason,
    evidence_ids: Vec<String>,
    objective: &str,
) -> DispatchStep {
    DispatchStep {
        candidate_agent_id: candidate.agent_id.clone(),
        capability: Capability::Explore,
        required_output: format!("Grounded fact inventory (tool reads only) for: {objective}"),
        reason,
        supporting_evidence_ids: evidence_ids,
        bind_contract: false,
    }
}

fn design_step(candidate: &Candidate, objective: &str) -> DispatchStep {
    DispatchStep {
        candidate_agent_id: candidate.agent_id.clone(),
        capability: Capability::Design,
        required_output: format!(
            "DesignDoc JSON for: {objective} (the deterministic verifier resolves it; \
             it binds only when Active)"
        ),
        reason: DecisionReason::DesignUndecidedDesign,
        supporting_evidence_ids: Vec::new(),
        bind_contract: false,
    }
}

#[allow(clippy::too_many_arguments)]
fn implement_step(
    candidate: &Candidate,
    reason: DecisionReason,
    evidence_ids: Vec<String>,
    bind_contract: bool,
    objective: &str,
) -> DispatchStep {
    DispatchStep {
        candidate_agent_id: candidate.agent_id.clone(),
        capability: Capability::Implement,
        required_output: format!("Implement: {objective}"),
        reason,
        supporting_evidence_ids: evidence_ids,
        bind_contract,
    }
}

/// Schedule the dispatch plan for the current evidence state.
///
/// Pure and deterministic: the same state yields the same plan. See the
/// module docs for the rule priority.
pub fn schedule(state: &EvidenceState) -> DispatchPlan {
    let objective = state.objective.as_str();
    let explorer = first_candidate(state, Capability::Explore);
    let designer = first_candidate(state, Capability::Design);
    let implementer = first_candidate(state, Capability::Implement);

    // ── Rule (d): doc quarantined — the machine reason codes decide. ────
    if let DocResolution::Quarantined { codes, evidence_ids } = &state.doc {
        let grounding_fixable = codes.iter().any(|code| {
            matches!(code, QuarantineCode::UngroundedPath | QuarantineCode::NoObservations)
        });
        // Grounding-fixable quarantines explore FIRST (grounding), then
        // proceed without the doc — the quarantine persists as advisory
        // context and is never silently re-activated.
        if grounding_fixable && !state.exploration_attempted {
            if let Some(explorer) = explorer {
                let mut steps = vec![explore_step(
                    explorer,
                    DecisionReason::QuarantinedGroundingExplore,
                    evidence_ids.clone(),
                    objective,
                )];
                if let Some(implementer) = implementer {
                    steps.push(implement_step(
                        implementer,
                        DecisionReason::QuarantinedProceedWithoutDoc,
                        evidence_ids.clone(),
                        false,
                        objective,
                    ));
                }
                return DispatchPlan { steps };
            }
        }
        // Tree conflict → reality wins (§5): implement WITHOUT the doc
        // contract. Everything else (grounding exhausted, no explorer,
        // unfixable codes) → proceed without the doc contract as well.
        let reason = if codes.contains(&QuarantineCode::TreeConflict) {
            DecisionReason::QuarantinedTreeConflictProceedWithoutDoc
        } else {
            DecisionReason::QuarantinedProceedWithoutDoc
        };
        let steps = implementer
            .map(|implementer| {
                vec![implement_step(implementer, reason, evidence_ids.clone(), false, objective)]
            })
            .unwrap_or_default();
        return DispatchPlan { steps };
    }

    // ── Rule (b): evidence missing — exploration is the NEXT STEP ONLY. ──
    // The caller re-consults the scheduler after the exploration's facts
    // land (`exploration_attempted` bounds the loop by construction).
    if evidence_missing(state) && !state.exploration_attempted {
        if let Some(explorer) = explorer {
            // The gap is an absence: cite the snapshot event when it exists
            // (snapshot present but zero observations), otherwise nothing —
            // never fabricate an id.
            let evidence_ids: Vec<String> = state.snapshot_event_id.clone().into_iter().collect();
            return DispatchPlan {
                steps: vec![explore_step(
                    explorer,
                    DecisionReason::EvidenceGapExplore,
                    evidence_ids,
                    objective,
                )],
            };
        }
    }

    // ── Terminal rules: every remaining plan ends in Implement when an
    //    implement-capable candidate exists. ─────────────────────────────
    let Some(implementer) = implementer else {
        // No implement candidate: the plan is empty; the caller surfaces its
        // "no implementation-stage agent" failure (preserved from the
        // heuristic fallback).
        return DispatchPlan { steps: Vec::new() };
    };

    match &state.doc {
        // Rule (e): the doc is Active — its contract is consumed (§6: an
        // architecture doc is only consumed when Active).
        DocResolution::Active { evidence_ids, .. } => DispatchPlan {
            steps: vec![implement_step(
                implementer,
                DecisionReason::DocActiveImplementContract,
                evidence_ids.clone(),
                true,
                objective,
            )],
        },
        // Rule (f): the doc was skipped — implement directly ("design is the
        // repo").
        DocResolution::Skipped { evidence_ids } => DispatchPlan {
            steps: vec![implement_step(
                implementer,
                DecisionReason::DocSkippedImplement,
                evidence_ids.clone(),
                false,
                objective,
            )],
        },
        // No doc (design undecided):
        DocResolution::Undecided => {
            if evidence_missing(state) {
                // Rule (c): design genuinely undecided (no doc AND the
                // planner produced no decidable plan) — a design-capable
                // agent runs, then the same Phase-5 verifier resolves its
                // output. Without a design candidate the stage is skipped,
                // never called (acceptance 6).
                if state.planner_failed {
                    if let Some(designer) = designer {
                        let mut steps = vec![design_step(designer, objective)];
                        steps.push(implement_step(
                            implementer,
                            DecisionReason::ImplementAfterDesign,
                            Vec::new(),
                            false,
                            objective,
                        ));
                        return DispatchPlan { steps };
                    }
                }
                // Exploration unavailable or already attempted and the design
                // stage cannot run: proceed on current knowledge.
                return DispatchPlan {
                    steps: vec![implement_step(
                        implementer,
                        DecisionReason::DesignUndecidedNoDesigner,
                        sufficient_evidence_ids(state),
                        false,
                        objective,
                    )],
                };
            }
            // Rule (a): evidence sufficient — implementation directly. No
            // architect, no researcher (§6 bullet 3).
            DispatchPlan {
                steps: vec![implement_step(
                    implementer,
                    DecisionReason::EvidenceSufficientImplement,
                    sufficient_evidence_ids(state),
                    false,
                    objective,
                )],
            }
        }
        // Quarantined docs never reach this arm (handled first above).
        DocResolution::Quarantined { .. } => DispatchPlan { steps: Vec::new() },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(id: &str, order: usize, capabilities: &[Capability]) -> Candidate {
        Candidate {
            agent_id: id.to_owned(),
            capabilities: capabilities.iter().copied().collect(),
            order,
        }
    }

    /// The default roster: explorer, designer, implementer (registration
    /// order = lexicographic id rank).
    fn full_roster() -> Vec<Candidate> {
        vec![
            candidate("architect", 0, &[Capability::Design]),
            candidate("coder", 1, &[Capability::Implement]),
            candidate("researcher", 2, &[Capability::Explore]),
        ]
    }

    fn state(doc: DocResolution, candidates: Vec<Candidate>) -> EvidenceState {
        EvidenceState {
            objective: "build the thing".to_owned(),
            planner_failed: true,
            snapshot_present: false,
            snapshot_event_id: None,
            observations: Vec::new(),
            doc,
            candidates,
            exploration_attempted: false,
        }
    }

    fn observed(
        doc: DocResolution,
        candidates: Vec<Candidate>,
        snapshot_event: Option<&str>,
    ) -> EvidenceState {
        let mut base = state(doc, candidates);
        base.snapshot_present = true;
        base.snapshot_event_id = snapshot_event.map(str::to_owned);
        base.observations = vec![
            Observation { event_id: "ev-obs-1".to_owned(), path: "src/main.rs".to_owned() },
            Observation { event_id: "ev-obs-2".to_owned(), path: "src/lib.rs".to_owned() },
        ];
        base
    }

    fn single(steps: &[DispatchStep], capability: Capability) -> bool {
        steps.len() == 1 && steps[0].capability == capability
    }

    // ── Matrix: evidence-sufficient ⇒ Implement direct ──────────────────
    #[test]
    fn evidence_sufficient_implements_directly_without_architect_or_researcher() {
        let state = observed(DocResolution::Undecided, full_roster(), Some("ev-snap"));
        let plan = schedule(&state);
        assert!(single(&plan.steps, Capability::Implement), "got: {plan:?}");
        assert_eq!(plan.steps[0].candidate_agent_id, "coder");
        assert_eq!(plan.steps[0].reason, DecisionReason::EvidenceSufficientImplement);
        assert!(!plan.steps[0].bind_contract, "no doc: no contract");
        // The decision cites the REAL evidence it consumed: the snapshot and
        // the observations.
        assert_eq!(
            plan.steps[0].supporting_evidence_ids,
            vec!["ev-snap".to_owned(), "ev-obs-1".to_owned(), "ev-obs-2".to_owned()]
        );
    }

    // ── Matrix: evidence-gap ⇒ Exploration (next step only) ─────────────
    #[test]
    fn evidence_gap_schedules_exploration_as_the_next_step_only() {
        let state = state(DocResolution::Undecided, full_roster());
        let plan = schedule(&state);
        assert!(single(&plan.steps, Capability::Explore), "got: {plan:?}");
        assert_eq!(plan.steps[0].candidate_agent_id, "researcher");
        assert_eq!(plan.steps[0].reason, DecisionReason::EvidenceGapExplore);
        assert!(
            plan.steps[0].required_output.contains("fact inventory")
                && plan.steps[0].required_output.contains("tool reads only"),
            "required_output names the grounded fact inventory: {plan:?}"
        );
        // No snapshot → nothing to cite; the gap is an absence.
        assert!(plan.steps[0].supporting_evidence_ids.is_empty());
    }

    /// Snapshot present but zero recorded facts is still an evidence gap —
    /// the gap cites the snapshot event (real id) without inventing more.
    #[test]
    fn snapshot_without_observations_is_an_evidence_gap_citing_the_snapshot() {
        let mut state = state(DocResolution::Undecided, full_roster());
        state.snapshot_present = true;
        state.snapshot_event_id = Some("ev-snap".to_owned());
        let plan = schedule(&state);
        assert!(single(&plan.steps, Capability::Explore), "got: {plan:?}");
        assert_eq!(plan.steps[0].supporting_evidence_ids, vec!["ev-snap".to_owned()]);
    }

    // ── Matrix: design-undecided ⇒ Design (verifier drives binding) ─────
    #[test]
    fn design_undecided_with_no_explorer_schedules_design_then_implement() {
        // Evidence gap + exploration unavailable (no explorer candidate) and
        // the design is genuinely undecided: the design-capable agent runs.
        let candidates = vec![
            candidate("architect", 0, &[Capability::Design]),
            candidate("coder", 1, &[Capability::Implement]),
        ];
        let plan = schedule(&state(DocResolution::Undecided, candidates));
        assert_eq!(plan.steps.len(), 2, "got: {plan:?}");
        assert_eq!(plan.steps[0].capability, Capability::Design);
        assert_eq!(plan.steps[0].candidate_agent_id, "architect");
        assert_eq!(plan.steps[0].reason, DecisionReason::DesignUndecidedDesign);
        assert_eq!(plan.steps[1].capability, Capability::Implement);
        assert_eq!(plan.steps[1].reason, DecisionReason::ImplementAfterDesign);
        assert!(!plan.steps[1].bind_contract, "no verified doc: no contract");
    }

    /// After an exploration was attempted (loop state), an unresolved design
    /// still schedules the design step rather than exploring forever.
    #[test]
    fn exploration_attempted_undecided_design_still_schedules_design() {
        let mut state = state(DocResolution::Undecided, full_roster());
        state.exploration_attempted = true;
        let plan = schedule(&state);
        assert_eq!(plan.steps.len(), 2, "got: {plan:?}");
        assert_eq!(plan.steps[0].capability, Capability::Design);
    }

    // ── Matrix: Quarantined(UNGROUNDED_PATH) ⇒ Exploration then
    //    proceed-without-doc ─────────────────────────────────────────────
    #[test]
    fn quarantined_ungrounded_path_explores_then_proceeds_without_doc() {
        let doc = DocResolution::Quarantined {
            codes: vec![QuarantineCode::UngroundedPath],
            evidence_ids: vec!["ev-claim".to_owned(), "ev-verdict".to_owned()],
        };
        let plan = schedule(&observed(doc, full_roster(), None));
        assert_eq!(plan.steps.len(), 2, "got: {plan:?}");
        assert_eq!(plan.steps[0].capability, Capability::Explore);
        assert_eq!(plan.steps[0].reason, DecisionReason::QuarantinedGroundingExplore);
        assert_eq!(plan.steps[1].capability, Capability::Implement);
        assert_eq!(plan.steps[1].reason, DecisionReason::QuarantinedProceedWithoutDoc);
        assert!(!plan.steps[1].bind_contract, "quarantined doc never binds");
        // The grounding decision cites the doc claim + verdict events — the
        // REAL ids the scheduler consumed.
        assert_eq!(
            plan.steps[0].supporting_evidence_ids,
            vec!["ev-claim".to_owned(), "ev-verdict".to_owned()]
        );
        assert_eq!(plan.steps[1].supporting_evidence_ids, plan.steps[0].supporting_evidence_ids);
    }

    /// NO_OBSERVATIONS quarantines are grounding-fixable too.
    #[test]
    fn quarantined_no_observations_explores_first() {
        let doc = DocResolution::Quarantined {
            codes: vec![QuarantineCode::NoObservations],
            evidence_ids: vec![],
        };
        let plan = schedule(&observed(doc, full_roster(), None));
        assert_eq!(plan.steps[0].reason, DecisionReason::QuarantinedGroundingExplore);
    }

    // ── Matrix: Quarantined(TREE_CONFLICT) ⇒ Implement without contract ──
    #[test]
    fn quarantined_tree_conflict_implements_without_contract_reality_wins() {
        let doc = DocResolution::Quarantined {
            codes: vec![QuarantineCode::TreeConflict],
            evidence_ids: vec!["ev-claim".to_owned()],
        };
        let plan = schedule(&observed(doc, full_roster(), None));
        assert!(single(&plan.steps, Capability::Implement), "got: {plan:?}");
        assert_eq!(plan.steps[0].reason, DecisionReason::QuarantinedTreeConflictProceedWithoutDoc);
        assert!(!plan.steps[0].bind_contract, "reality wins: no doc contract");
    }

    /// Grounding exhausted (attempted) or no explorer: proceed without doc.
    #[test]
    fn quarantined_without_available_exploration_proceeds_without_doc() {
        let doc = DocResolution::Quarantined {
            codes: vec![QuarantineCode::UngroundedPath],
            evidence_ids: vec![],
        };
        let mut state = observed(doc.clone(), full_roster(), None);
        state.exploration_attempted = true;
        let plan = schedule(&state);
        assert!(single(&plan.steps, Capability::Implement), "got: {plan:?}");
        assert_eq!(plan.steps[0].reason, DecisionReason::QuarantinedProceedWithoutDoc);

        // Same for a roster without any explorer (acceptance 6).
        let candidates = vec![candidate("coder", 0, &[Capability::Implement])];
        let plan = schedule(&observed(doc, candidates, None));
        assert!(single(&plan.steps, Capability::Implement), "got: {plan:?}");
    }

    // ── Matrix: Skipped ⇒ Implement; Active ⇒ Implement with contract ───
    #[test]
    fn skipped_doc_implements_directly() {
        let doc = DocResolution::Skipped { evidence_ids: vec!["ev-claim".to_owned()] };
        let plan = schedule(&observed(doc, full_roster(), None));
        assert!(single(&plan.steps, Capability::Implement), "got: {plan:?}");
        assert_eq!(plan.steps[0].reason, DecisionReason::DocSkippedImplement);
        assert!(!plan.steps[0].bind_contract);
        assert_eq!(plan.steps[0].supporting_evidence_ids, vec!["ev-claim".to_owned()]);
    }

    #[test]
    fn active_doc_implements_with_contract() {
        let doc = DocResolution::Active {
            contract_paths: vec!["src/main.rs".to_owned()],
            evidence_ids: vec!["ev-claim".to_owned(), "ev-verdict".to_owned()],
        };
        let plan = schedule(&observed(doc, full_roster(), None));
        assert!(single(&plan.steps, Capability::Implement), "got: {plan:?}");
        assert_eq!(plan.steps[0].reason, DecisionReason::DocActiveImplementContract);
        assert!(plan.steps[0].bind_contract, "the Active doc's contract binds");
    }

    /// A Skipped/Active doc plus an evidence gap still grounds FIRST (rule b
    /// outranks the terminal implement), then the loop reaches the doc rule.
    #[test]
    fn evidence_gap_outranks_terminal_doc_rules() {
        let doc = DocResolution::Skipped { evidence_ids: vec![] };
        let plan = schedule(&state(doc, full_roster()));
        assert!(single(&plan.steps, Capability::Explore), "got: {plan:?}");

        let doc = DocResolution::Active { contract_paths: vec![], evidence_ids: vec![] };
        let plan = schedule(&state(doc, full_roster()));
        assert!(single(&plan.steps, Capability::Explore), "got: {plan:?}");
    }

    // ── Acceptance 6: missing stages are never called ───────────────────
    #[test]
    fn roster_without_design_and_research_still_plans_validly() {
        // Same inputs as the matrix, but the roster has ONLY an
        // implement-capable agent: every plan stays valid, references only
        // registered agents, and never schedules the missing stages.
        let implementer_only = vec![candidate("coder", 0, &[Capability::Implement])];

        // Evidence sufficient: implement direct.
        let plan = schedule(&observed(DocResolution::Undecided, implementer_only.clone(), None));
        assert!(single(&plan.steps, Capability::Implement), "got: {plan:?}");
        assert_eq!(plan.steps[0].candidate_agent_id, "coder");

        // Evidence gap: no explorer exists → the stage is skipped; the plan
        // degrades to implement rather than calling a missing stage.
        let plan = schedule(&state(DocResolution::Undecided, implementer_only.clone()));
        assert!(single(&plan.steps, Capability::Implement), "got: {plan:?}");
        assert_eq!(plan.steps[0].reason, DecisionReason::DesignUndecidedNoDesigner);

        // Design undecided with no designer and no explorer: implement.
        let plan = schedule(&state(DocResolution::Undecided, implementer_only));
        assert!(plan.steps.iter().all(|step| step.capability != Capability::Design));
        assert!(plan.steps.iter().all(|step| step.capability != Capability::Explore));
    }

    /// No implement-capable candidate at all: an empty plan — the caller
    /// surfaces the preserved "no implementation-stage agent" failure.
    #[test]
    fn no_implement_candidate_yields_an_empty_plan() {
        let candidates = vec![
            candidate("architect", 0, &[Capability::Design]),
            candidate("researcher", 1, &[Capability::Explore]),
        ];
        let plan = schedule(&observed(DocResolution::Undecided, candidates, None));
        assert!(plan.steps.is_empty(), "got: {plan:?}");
    }

    // ── Determinism and id discipline ───────────────────────────────────
    #[test]
    fn same_input_yields_the_same_plan() {
        let make = || {
            observed(
                DocResolution::Quarantined {
                    codes: vec![QuarantineCode::UngroundedPath],
                    evidence_ids: vec!["ev-claim".to_owned()],
                },
                full_roster(),
                None,
            )
        };
        assert_eq!(schedule(&make()), schedule(&make()), "the scheduler is a pure function");
    }

    /// Tie-break: equal capability classes resolve by the caller-assigned
    /// registration order, stably.
    #[test]
    fn candidate_tie_break_follows_registration_order() {
        let candidates = vec![
            candidate("zeta-implementer", 7, &[Capability::Implement]),
            candidate("alpha-implementer", 3, &[Capability::Implement]),
        ];
        let plan = schedule(&observed(DocResolution::Undecided, candidates, None));
        assert_eq!(plan.steps[0].candidate_agent_id, "alpha-implementer", "lower order wins");
    }

    /// Evidence ids are cited only from the state the scheduler consumed —
    /// the scheduler fabricates nothing, and the citation cap stays bounded.
    #[test]
    fn evidence_ids_come_only_from_the_state_and_stay_bounded() {
        let mut state = observed(DocResolution::Undecided, full_roster(), Some("ev-snap"));
        state.observations = (0..20)
            .map(|i| Observation { event_id: format!("ev-{i:02}"), path: format!("p{i}.rs") })
            .collect();
        let plan = schedule(&state);
        let ids = &plan.steps[0].supporting_evidence_ids;
        assert_eq!(ids.len(), 8, "cited ids stay capped: {ids:?}");
        assert_eq!(ids[0], "ev-snap", "the snapshot id is cited first");
        assert!(ids.iter().all(|id| id == "ev-snap" || id.starts_with("ev-")));
    }
}
