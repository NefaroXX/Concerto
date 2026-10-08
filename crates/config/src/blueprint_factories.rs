//! Shipped named-blueprint catalog and its factory helpers (ADR-58 D3).
//!
//! This module owns `named_blueprint` / `default_blueprint`, the four shipped
//! variants (`standard`, `tdd`, `docs-only`, `research-only`), the `stage`
//! builder, the coordinator fallback personas, and the
//! `standard_relationships()` defaults. The bodies move verbatim from
//! `blueprint.rs`; the parent module re-exports them so every existing
//! `crate::blueprint::{...}` path — including the crate-root re-exports in
//! `lib.rs` — keeps resolving unchanged. No tests moved: the parent's tests
//! are scattered around this span and stay put, resolving these items through
//! that re-export. `stage` and `standard_relationships` are `pub(super)` (they
//! were module-private) so the parent's resolution code and tests keep
//! reaching them; their effective visibility is unchanged.

use super::{
    Blueprint, ExecutionFilesDef, FallbackPersonaDef, FeedLabel, PipelineDef, RelationshipDef,
    RelationshipSemantics, StageCondition, StageDef, StageFlags, StageKind,
    ORCHESTRATION_SCHEMA_VERSION,
};
/// Look up a shipped named blueprint variant (ADR-58 D3).
pub fn named_blueprint(name: &str) -> Option<Blueprint> {
    match name {
        "standard" => Some(standard_blueprint()),
        "tdd" => Some(tdd_blueprint()),
        "docs-only" => Some(docs_only_blueprint()),
        "research-only" => Some(research_only_blueprint()),
        _ => None,
    }
}

/// The default blueprint — the pre-blueprint five-stage pipeline reproduced
/// as data (byte-identical legacy equivalence, ADR-58 Consequences).
pub fn default_blueprint() -> Blueprint {
    standard_blueprint()
}

/// `standard`: design → research → implement (primary) → review → validate,
/// staffed by the five built-in seeds with today's feed bindings and
/// relationship defaults.
pub fn standard_blueprint() -> Blueprint {
    Blueprint {
        schema_version: ORCHESTRATION_SCHEMA_VERSION,
        name: "standard".to_string(),
        description: Some(
            "The default pipeline (pre-blueprint default): design, research, implement, \
             review, validate, staffed by the five built-in specialists."
                .to_string(),
        ),
        pipeline: PipelineDef {
            stages: vec![
                StageDef {
                    kind: StageKind::Planning.as_str().to_string(),
                    feed: Some(FeedLabel::Plan),
                    agents: vec!["architect".into()],
                    ..stage("design", "Design", StageKind::Planning.as_str())
                },
                StageDef {
                    kind: StageKind::Research.as_str().to_string(),
                    feed: Some(FeedLabel::Understand),
                    agents: vec!["researcher".into()],
                    ..stage("research", "Research", StageKind::Research.as_str())
                },
                StageDef {
                    kind: StageKind::Execution.as_str().to_string(),
                    feed: Some(FeedLabel::Execute),
                    primary: true,
                    agents: vec!["coder".into()],
                    ..stage("implement", "Implement", StageKind::Execution.as_str())
                },
                StageDef {
                    kind: StageKind::Review.as_str().to_string(),
                    feed: Some(FeedLabel::Verify),
                    agents: vec!["reviewer".into()],
                    fallback: Some(coordinator_fallback()),
                    ..stage("review", "Review", StageKind::Review.as_str())
                },
                StageDef {
                    kind: StageKind::Acceptance.as_str().to_string(),
                    feed: Some(FeedLabel::Verify),
                    agents: vec!["validator".into()],
                    fallback: Some(coordinator_fallback()),
                    ..stage("validate", "Validate", StageKind::Acceptance.as_str())
                },
            ],
        },
        relationships: standard_relationships(),
    }
}

/// `tdd`: research → design → implement (primary) → test gate → validate.
///
/// The `test` gate carries an `OnGateCycle` condition and an explicit cycle
/// cap so the red-green-refactor loop is bounded by the rulebook.
pub fn tdd_blueprint() -> Blueprint {
    Blueprint {
        schema_version: ORCHESTRATION_SCHEMA_VERSION,
        name: "tdd".to_string(),
        description: Some(
            "Test-driven pipeline: research, design, implement, a bounded test gate \
             (Review kind), then acceptance."
                .to_string(),
        ),
        pipeline: PipelineDef {
            stages: vec![
                StageDef {
                    kind: StageKind::Research.as_str().to_string(),
                    feed: Some(FeedLabel::Understand),
                    agents: vec!["researcher".into()],
                    ..stage("research", "Research", StageKind::Research.as_str())
                },
                StageDef {
                    kind: StageKind::Planning.as_str().to_string(),
                    feed: Some(FeedLabel::Plan),
                    agents: vec!["architect".into()],
                    ..stage("design", "Design", StageKind::Planning.as_str())
                },
                StageDef {
                    kind: StageKind::Execution.as_str().to_string(),
                    feed: Some(FeedLabel::Execute),
                    primary: true,
                    agents: vec!["coder".into()],
                    ..stage("implement", "Implement", StageKind::Execution.as_str())
                },
                StageDef {
                    kind: StageKind::Review.as_str().to_string(),
                    feed: Some(FeedLabel::Verify),
                    condition: StageCondition::OnGateCycle,
                    max_cycles: Some(3),
                    agents: vec!["reviewer".into()],
                    fallback: Some(coordinator_fallback()),
                    ..stage("test", "Test Gate", StageKind::Review.as_str())
                },
                StageDef {
                    kind: StageKind::Acceptance.as_str().to_string(),
                    feed: Some(FeedLabel::Verify),
                    agents: vec!["validator".into()],
                    fallback: Some(coordinator_fallback()),
                    ..stage("validate", "Validate", StageKind::Acceptance.as_str())
                },
            ],
        },
        relationships: standard_relationships(),
    }
}

/// `docs-only`: research → design → documentation (primary `Execution`,
/// owning `docs/`) → validate.
///
/// The primary `Execution` stage owns the docs artifact set — the plan's
/// single writer, writing `plan.files`.
pub fn docs_only_blueprint() -> Blueprint {
    Blueprint {
        schema_version: ORCHESTRATION_SCHEMA_VERSION,
        name: "docs-only".to_string(),
        description: Some(
            "Documentation-only pipeline: research, design, a documentation Execution \
             stage that owns docs/, then acceptance."
                .to_string(),
        ),
        pipeline: PipelineDef {
            stages: vec![
                StageDef {
                    kind: StageKind::Research.as_str().to_string(),
                    feed: Some(FeedLabel::Understand),
                    agents: vec!["researcher".into()],
                    ..stage("research", "Research", StageKind::Research.as_str())
                },
                StageDef {
                    kind: StageKind::Planning.as_str().to_string(),
                    feed: Some(FeedLabel::Plan),
                    agents: vec!["architect".into()],
                    ..stage("design", "Design", StageKind::Planning.as_str())
                },
                StageDef {
                    kind: StageKind::Execution.as_str().to_string(),
                    feed: Some(FeedLabel::Execute),
                    primary: true,
                    agents: vec!["coder".into()],
                    files: Some(ExecutionFilesDef {
                        ownership: "docs/".into(),
                        expected_artifacts: vec!["docs/*.md".into()],
                    }),
                    ..stage("documentation", "Documentation", StageKind::Execution.as_str())
                },
                StageDef {
                    kind: StageKind::Acceptance.as_str().to_string(),
                    feed: Some(FeedLabel::Verify),
                    agents: vec!["validator".into()],
                    fallback: Some(coordinator_fallback()),
                    ..stage("validate", "Validate", StageKind::Acceptance.as_str())
                },
            ],
        },
        relationships: standard_relationships(),
    }
}

/// `research-only`: research → analysis (primary `Execution`, owning
/// `research/`) → validate.
pub fn research_only_blueprint() -> Blueprint {
    Blueprint {
        schema_version: ORCHESTRATION_SCHEMA_VERSION,
        name: "research-only".to_string(),
        description: Some(
            "Research-only pipeline: research, an analysis Execution stage that owns \
             research/, then acceptance."
                .to_string(),
        ),
        pipeline: PipelineDef {
            stages: vec![
                StageDef {
                    kind: StageKind::Research.as_str().to_string(),
                    feed: Some(FeedLabel::Understand),
                    agents: vec!["researcher".into()],
                    ..stage("research", "Research", StageKind::Research.as_str())
                },
                StageDef {
                    kind: StageKind::Execution.as_str().to_string(),
                    feed: Some(FeedLabel::Execute),
                    primary: true,
                    agents: vec!["coder".into()],
                    files: Some(ExecutionFilesDef {
                        ownership: "research/".into(),
                        expected_artifacts: vec!["research/*.md".into()],
                    }),
                    ..stage("analysis", "Analysis", StageKind::Execution.as_str())
                },
                StageDef {
                    kind: StageKind::Acceptance.as_str().to_string(),
                    feed: Some(FeedLabel::Verify),
                    agents: vec!["validator".into()],
                    fallback: Some(coordinator_fallback()),
                    ..stage("validate", "Validate", StageKind::Acceptance.as_str())
                },
            ],
        },
        relationships: standard_relationships(),
    }
}

/// Minimal `StageDef` builder: everything defaulted; the caller overrides the
/// fields that carry pipeline data. `kind` is the open kind string — pass
/// [`StageKind::as_str`] for a known kind or a plain `&str` for a user kind.
// `pub(super)`, not private: the parent module's tests (`blueprint::tests`)
// live outside this submodule and call this directly — `pub(super)` keeps
// exactly the pre-extraction visibility scope (the `blueprint` subtree).
pub(super) fn stage(tag: &str, label: &str, kind: impl Into<String>) -> StageDef {
    StageDef {
        tag: tag.to_string(),
        label: label.to_string(),
        kind: kind.into(),
        version: 1,
        flags: StageFlags::default(),
        condition: StageCondition::Always,
        max_cycles: None,
        feed: None,
        primary: false,
        agents: Vec::new(),
        fallback: None,
        files: None,
    }
}

/// The engine's unstaffed-gate persona: the coordinator identity. Rendered
/// only when a gate is actually unstaffed; its mask defaults to the stage-kind
/// mask (no writes for gates).
///
/// Faithful mirror (B5): the runtime's `self_verify_agent`
/// (`coordinator.rs` `self_verify_agent`, eval-mode semantics) builds the
/// persona from `PromptSections::default()` — empty system instructions — and
/// has no tool executor. `system_instructions: None` reproduces that exactly;
/// the persona def adds nothing on top of the engine-rendered coordinator
/// prompt.
///
/// The coordinator's gate-fallback renders use this as the **engine default**
/// when a gate stage ships `fallback: None` (an unstaffed non-default
/// blueprint that leaves a gate without a configured persona): the reserved
/// `coordinator` identity and empty persona keep the trigger-1 semantics of
/// the pre-blueprint hardcoded `self_verify_agent` construction.
pub fn coordinator_fallback() -> FallbackPersonaDef {
    FallbackPersonaDef {
        id: "coordinator".to_string(),
        label: "Coordinator".to_string(),
        system_instructions: None,
        capabilities: StageFlags::default(),
    }
}

/// The engine's unstaffed-`Execution`-stage persona (ADR-35 §8): the
/// coordinator self-implements the subtask. No shipped named blueprint uses
/// this today (the `implement` stage is staffed by the coder seed); the
/// `coordinator-self-execute` sentinel id is documented on
/// [`FallbackPersonaDef::id`] for custom include blueprints that leave an
/// `Execution` stage unstaffed.
///
/// Faithful mirror (B5): the runtime renders `COORDINATOR_SELF_IMPLEMENT_PROMPT`
/// (`coordinator.rs:60-73`) inside the sentinel mechanism, not from the
/// persona def. The persona def therefore carries no instructions here.
///
/// ADR-58 P2+P3 (review F5): promoted from a `#[cfg(test)]` placeholder to
/// production. The orchestrator's decompose roster and `self_implement_agent`
/// render use this as the **engine default** when the primary `Execution`
/// stage ships `fallback: None` (the `standard` blueprint) and the stage is
/// unstaffed — so an unstaffed-`Execution` blueprint gets a working sentinel
/// without shipping a persona in the default blueprint. The runtime keeps
/// emitting the reserved `coordinator` id (design doc §3 review F2), never the
/// persona's `coordinator-self-execute` id, which is validation-only.
pub fn coordinator_self_implement_fallback() -> FallbackPersonaDef {
    FallbackPersonaDef {
        id: "coordinator-self-execute".to_string(),
        label: "Coordinator (self-execute)".to_string(),
        system_instructions: None,
        capabilities: StageFlags::default(),
    }
}

/// `standard` relationship defaults as data rows over closed semantics
/// (ADR-58 §4), mirroring `default_stage_relationships()`
/// (`relationship.rs`): review→implement & validate→implement `Supervises`
/// (Delegation), research→implement `ProvidesContextTo` (ContextFlow),
/// design→implement & design→research `OwnsDesign` (Delegation).
///
/// `from`/`to` are **stage tags** (the Studio's stage pickers restrict
/// endpoints to the pipeline's stage tags), never agent role ids.
// `pub(super)`, not private: the parent's `resolve_blueprint_validated` and
// the `blueprint::tests` case for the N5 empty-relationship fallback call this
// directly — `pub(super)` keeps exactly the pre-extraction visibility scope
// (the `blueprint` subtree).
pub(super) fn standard_relationships() -> Vec<RelationshipDef> {
    vec![
        RelationshipDef {
            kind: "supervises".into(),
            semantics: RelationshipSemantics::Delegation,
            from: "review".into(),
            to: "implement".into(),
        },
        RelationshipDef {
            kind: "provides_context_to".into(),
            semantics: RelationshipSemantics::ContextFlow,
            from: "research".into(),
            to: "implement".into(),
        },
        RelationshipDef {
            kind: "owns_design".into(),
            semantics: RelationshipSemantics::Delegation,
            from: "design".into(),
            to: "implement".into(),
        },
        RelationshipDef {
            kind: "owns_design".into(),
            semantics: RelationshipSemantics::Delegation,
            from: "design".into(),
            to: "research".into(),
        },
        RelationshipDef {
            kind: "supervises".into(),
            semantics: RelationshipSemantics::Delegation,
            from: "validate".into(),
            to: "implement".into(),
        },
    ]
}
