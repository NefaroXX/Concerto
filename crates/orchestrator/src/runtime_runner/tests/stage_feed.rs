//! Per-stage feed-binding and transcript gate-label coverage for
//! `runtime_runner_impl`.
//!
//! Mechanical extraction (NORM S24I): the ADR-58 P2+P3 (Batch 3b) feed banner,
//! the `stage_feed_bindings_resolve_standard_and_legacy_advances` test, the
//! ADR-58 P2+P3 (Batch 4b) gate-label banner and the three `gate_labels_*`
//! tests move verbatim out of `runtime_runner::runtime_runner_tests`, so test
//! names and assertions are unchanged. `use super::super::*;` keeps the parent
//! (`runtime_runner_impl`) items in scope; `TestAudit` is imported from
//! `stage_harness.rs` (slice G) at that path — the harness is shared, never
//! duplicated.

use super::super::*;
use super::stage_harness::TestAudit;
use concerto_config::ResolvedBlueprint;
use concerto_core::event::EventKind;
use concerto_core::transcript::GateLabels;

// ------------------------------------------------------------------
// ADR-58 P2+P3 (Batch 3b): per-stage feed bindings through the resolved
// blueprint (R6/F3). The Q4 pin — a review-gate cycle now advances the
// Verify chip — is deliberate (design doc §7 Q4; P1 binds `review →
// Verify`, blueprint.rs:668) and is feed-only: replay binds the same
// table `stage_feed_advance` resolves.
// ------------------------------------------------------------------

#[test]
fn stage_feed_bindings_resolve_standard_and_legacy_advances() {
    use concerto_config::blueprint::OrchestrationConfig;
    use concerto_core::policy::SimplePolicyEngine;
    use concerto_core::types::ToolRegistry;
    use concerto_providers::mock::MockProvider;
    use concerto_providers::retry::RetryPolicy;

    // The resolved standard blueprint binds research → Understand, design
    // → Plan, implement → Execute, review → Verify, validate → Verify
    // (blueprint §5.6); feed-only emission and replay derive from the same
    // table.
    let resolved = OrchestrationConfig::default()
        .resolve(&[], None)
        .expect("the standard blueprint must validate and resolve");
    let facade = BlueprintFacade::new(&resolved);
    assert_eq!(
        facade.feed_for("review"),
        Some(RunStage::Verify),
        "the review-gate feed binding is the source of the Verify advance (Q4)"
    );

    let executor = Arc::new(ToolExecutor::new(
        Arc::new(ToolRegistry::default()),
        Arc::new(SimplePolicyEngine::new(Vec::new(), Arc::new(TestAudit))),
    ));
    let registry = AgentRegistry::build_with_roles_for_project(
        HashMap::new(),
        Arc::new(MockProvider::default()),
        executor,
        EventBus::new(128),
        RetryPolicy::default(),
        std::path::Path::new("."),
        &HashMap::new(),
        "",
        true,
        None, // no fact-writer pool in this test
        None, // no shell profile in this test
    );

    let task_id = TaskId::new();
    // The review-gate cycle advances the chip to Verify via the review
    // stage's feed binding — today only the validation cycle did (Q4).
    assert_eq!(
        stage_feed_advance(
            &EventKind::ReviewCycleStarted { task_id, cycle_num: 1 },
            &registry,
            Some(&facade),
            false,
            false,
        ),
        Some(RunStage::Verify),
        "a review cycle must advance the Verify chip (Q4)"
    );
    // The validation cycle keeps advancing Verify through the same table.
    assert_eq!(
        stage_feed_advance(
            &EventKind::ValidationCycleStarted { task_id, cycle_num: 1 },
            &registry,
            Some(&facade),
            false,
            false,
        ),
        Some(RunStage::Verify),
        "a validation cycle keeps advancing the Verify chip"
    );
    // A staffed implement-stage subtask advances to Execute from its feed
    // binding.
    assert_eq!(
        stage_feed_advance(
            &EventKind::SubTaskCreated {
                task_id,
                role: AgentId::new("coder"),
                description: "implement the change".into(),
            },
            &registry,
            Some(&facade),
            false,
            false,
        ),
        Some(RunStage::Execute),
        "a coder subtask advances to Execute from the implement feed"
    );
    // A research-stage subtask advances to Understand — the R6 per-stage
    // generalization of the old implement-only classification.
    assert_eq!(
        stage_feed_advance(
            &EventKind::SubTaskCreated {
                task_id,
                role: AgentId::new("researcher"),
                description: "research".into(),
            },
            &registry,
            Some(&facade),
            false,
            false,
        ),
        Some(RunStage::Understand),
        "a researcher subtask advances to Understand from the research feed"
    );
    // The coordinator self-implement sentinel keeps advancing to Execute
    // when the Execution stage is unstaffed (review F4).
    assert_eq!(
        stage_feed_advance(
            &EventKind::SubTaskCreated {
                task_id,
                role: AgentId::new("coordinator"),
                description: "self-execute".into(),
            },
            &registry,
            Some(&facade),
            false,
            true,
        ),
        Some(RunStage::Execute),
        "the coordinator self-implement sentinel advances to Execute"
    );
    // Planning-only runs (M1) never report an implement transition.
    assert_eq!(
        stage_feed_advance(
            &EventKind::ReviewCycleStarted { task_id, cycle_num: 1 },
            &registry,
            Some(&facade),
            true,
            false,
        ),
        None,
        "planning-only runs never advance past Planning"
    );
    // Without a facade the legacy feed classification keeps today's
    // behavior: a review cycle emits nothing, the validation cycle still
    // advances Verify, and only implement-stage subtasks advance Execute.
    assert_eq!(
        stage_feed_advance(
            &EventKind::ReviewCycleStarted { task_id, cycle_num: 1 },
            &registry,
            None,
            false,
            false,
        ),
        None,
        "without a facade a review cycle keeps the legacy no-advance behavior"
    );
    assert_eq!(
        stage_feed_advance(
            &EventKind::ValidationCycleStarted { task_id, cycle_num: 1 },
            &registry,
            None,
            false,
            false,
        ),
        Some(RunStage::Verify),
        "without a facade the validation cycle keeps advancing Verify"
    );
}

// ------------------------------------------------------------------
// ADR-58 P2+P3 (Batch 4b): F8 — gate labels for transcript activity
// entries resolve from the resolved blueprint's stage definitions.
// ------------------------------------------------------------------

#[test]
fn gate_labels_resolve_from_blueprint_and_default_on_standard() {
    use concerto_config::blueprint::OrchestrationConfig;

    // No resolved blueprint (tests, `[orchestration]`-less configs):
    // the generic gate labels are used (never role ids).
    assert_eq!(
        gate_labels_for_resolved(None),
        GateLabels { review: "Review".into(), validate: "Validate".into() },
    );

    // The default `standard` blueprint carries the generic stage labels
    // ("Review"/"Validate"), so the resolved path and the facade-less
    // fallback agree.
    let resolved = OrchestrationConfig::default()
        .resolve(&[], None)
        .expect("the standard blueprint must validate and resolve");
    assert_eq!(
        gate_labels_for_resolved(Some(&resolved)),
        GateLabels { review: "Review".into(), validate: "Validate".into() },
        "standard blueprint uses the generic gate labels"
    );
}

#[test]
fn gate_labels_route_custom_stage_labels() {
    use concerto_config::blueprint::{
        Blueprint, CapabilityMask, PipelineDef, StageCondition, StageDef, StageFlags, StageKind,
    };
    use concerto_config::ResolvedStage;
    use std::collections::HashMap;

    // A custom blueprint that renames the gate stages surfaces its
    // configured labels in transcript activity entries (F8).
    let review_def = StageDef {
        tag: "review".into(),
        label: "QA Reviewer".into(),
        kind: StageKind::Review.as_str().to_string(),
        version: 1,
        flags: StageFlags::default(),
        condition: StageCondition::Always,
        max_cycles: None,
        feed: None,
        primary: false,
        agents: vec!["reviewer".into()],
        fallback: None,
        files: None,
    };
    let validate_def = StageDef {
        tag: "validate".into(),
        label: "QA Verifier".into(),
        kind: StageKind::Acceptance.as_str().to_string(),
        version: 1,
        flags: StageFlags::default(),
        condition: StageCondition::Always,
        max_cycles: None,
        feed: None,
        primary: false,
        agents: vec!["validator".into()],
        fallback: None,
        files: None,
    };
    let resolved = ResolvedBlueprint {
        blueprint: Blueprint {
            schema_version: 1,
            name: "custom-gates".into(),
            description: None,
            pipeline: PipelineDef { stages: vec![review_def.clone(), validate_def.clone()] },
            relationships: Vec::new(),
        },
        stages: vec![
            ResolvedStage {
                def: review_def,
                effective_capabilities: CapabilityMask::default(),
                effective_feed: None,
            },
            ResolvedStage {
                def: validate_def,
                effective_capabilities: CapabilityMask::default(),
                effective_feed: None,
            },
        ],
        feed_map: HashMap::new(),
        relationship_defaults: Vec::new(),
    };
    assert_eq!(
        gate_labels_for_resolved(Some(&resolved)),
        GateLabels { review: "QA Reviewer".into(), validate: "QA Verifier".into() },
    );
}

/// Issue #150: gate labels are resolved by KIND, so renamed review/
/// validate TAGS still surface their configured labels. This test's
/// blueprint renames both tags (`quality`/`ship`, kinds preserved) — the
/// canonical-tag lookup the old code used would miss them entirely and
/// fall back to the default labels.
#[test]
fn gate_labels_follow_renamed_gate_tags() {
    use concerto_config::blueprint::{
        Blueprint, CapabilityMask, PipelineDef, StageCondition, StageDef, StageFlags,
    };
    use concerto_config::ResolvedStage;
    use std::collections::HashMap;

    let stage = |tag: &str, label: &str, kind: StageKind| StageDef {
        tag: tag.into(),
        label: label.into(),
        kind: kind.as_str().to_string(),
        version: 1,
        flags: StageFlags::default(),
        condition: StageCondition::Always,
        max_cycles: None,
        feed: None,
        primary: false,
        agents: Vec::new(),
        fallback: None,
        files: None,
    };
    let defs = vec![
        stage("quality", "QA Reviewer", StageKind::Review),
        stage("ship", "QA Verifier", StageKind::Acceptance),
    ];
    let resolved = ResolvedBlueprint {
        blueprint: Blueprint {
            schema_version: 1,
            name: "renamed-gate-labels".into(),
            description: None,
            pipeline: PipelineDef { stages: defs.clone() },
            relationships: Vec::new(),
        },
        stages: defs
            .iter()
            .map(|def| ResolvedStage {
                def: def.clone(),
                effective_capabilities: CapabilityMask::default(),
                effective_feed: None,
            })
            .collect(),
        feed_map: HashMap::new(),
        relationship_defaults: Vec::new(),
    };
    assert_eq!(
        gate_labels_for_resolved(Some(&resolved)),
        GateLabels { review: "QA Reviewer".into(), validate: "QA Verifier".into() },
        "renamed gate tags keep their labels via kind-based resolution"
    );
}
