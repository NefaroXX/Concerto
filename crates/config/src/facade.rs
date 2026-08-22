//! ADR-58 P2+P3 (Batch 1): the read-only, typed lookup surface over a
//! resolved blueprint.
//!
//! The runtime and coordinator query the blueprint through [`BlueprintFacade`]
//! instead of importing orchestration internals. Each method is a lookup or
//! derivation — no new orchestration semantics. In Batch 1 the facade is
//! wired into the runtime multi-agent construction (runtime_runner.rs) and
//! consumed by the sequencing guards at `Coordinator::stage_of` /
//! `Coordinator::first_agent_for_stage`; dispatch sites switch over in
//! Batch 2+ (design doc §2.2 replacement table).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use concerto_core::types::AgentId;
use concerto_core::RunStage;

use crate::blueprint::{
    BlueprintError, RelationshipSemantics, ResolvedBlueprint, ResolvedStage, StageKind,
};
use crate::schema::{builtin_agent_seeds, CustomAgentConfig, ResolvedCapabilities};

/// Read-only query wrapper over a validated [`ResolvedBlueprint`].
///
/// The blueprint is shared via `Arc`, so cloning the facade clones the
/// pointer — cheap enough to store alongside the coordinator's other shared
/// subsystems.
#[derive(Debug, Clone)]
pub struct BlueprintFacade {
    resolved: Arc<ResolvedBlueprint>,
}

impl BlueprintFacade {
    /// Wrap a validated, resolved blueprint.
    pub fn new(resolved: &ResolvedBlueprint) -> Self {
        Self { resolved: Arc::new(resolved.clone()) }
    }

    /// The resolved stage registered under `tag`, in pipeline order.
    pub fn stage_by_tag(&self, tag: &str) -> Option<&ResolvedStage> {
        self.resolved.stages.iter().find(|stage| stage.def.tag == tag)
    }

    /// The closed engine kind of the stage registered under `tag`, if its
    /// kind string parses into the known-kind vocabulary. Unknown tags (and
    /// unknown user kinds) resolve to `None`.
    pub fn stage_kind(&self, tag: &str) -> Option<StageKind> {
        self.stage_by_tag(tag).and_then(|stage| stage.def.known_kind())
    }

    /// The pipeline's (declarative) primary `Execution` stage, if any.
    /// Planner partitions and fails-fast checks key off this stage's
    /// staffing and plan-artifact contract instead of tag string identity.
    /// `primary` is a plain flag — the rulebook no longer enforces
    /// exactly-one.
    ///
    /// Tolerant resolution chain (slice 1b): the first stage flagged
    /// `primary` **and** carrying a known `Execution` kind wins; otherwise the
    /// first stage anywhere in the pipeline with a known `Execution` kind;
    /// otherwise `None`. A blueprint that marks an unknown-kind or a
    /// non-`Execution` stage `primary` (legal since rule (a) was removed)
    /// therefore still resolves to an actual `Execution` stage when one
    /// exists, and callers handle `None` (an `Execution`-free blueprint) with
    /// their legacy fallback.
    pub fn primary_execution_stage(&self) -> Option<&ResolvedStage> {
        let is_execution =
            |stage: &ResolvedStage| stage.def.known_kind() == Some(StageKind::Execution);
        // Preferred: a stage flagged `primary` that is also a known Execution
        // kind. `stage` here is `&&ResolvedStage`; deref coercion feeds it to
        // `is_execution`.
        self.resolved
            .stages
            .iter()
            .find(|stage| stage.def.primary && is_execution(stage))
            // Fallback: no primary Execution stage (or the primary-flagged
            // stage is a non-Execution/unknown kind) — any known Execution
            // stage keeps planner partitions and fails-fast working.
            .or_else(|| self.resolved.stages.iter().find(|stage| is_execution(stage)))
    }

    /// The first pipeline stage whose kind string parses to the given known
    /// kind, in pipeline order (issue #150).
    ///
    /// Gate, feed, label, and fallback lookups must key off **semantics**
    /// (kind), never canonical tags: a blueprint that renames the review
    /// stage to `quality` or the validation stage to `ship` (kinds
    /// preserved) keeps its gate cycles, feeds, and labels exactly where
    /// the renamed stage is. Unknown-kind stages never match.
    pub fn first_stage_of_kind(&self, kind: StageKind) -> Option<&ResolvedStage> {
        self.resolved.stages.iter().find(|stage| stage.def.known_kind() == Some(kind))
    }

    /// The resolved stage in which `id` is staffed (search over each stage's
    /// `def.agents`), if any.
    pub fn stage_for_agent(&self, id: &AgentId) -> Option<&ResolvedStage> {
        self.resolved
            .stages
            .iter()
            .find(|stage| stage.def.agents.iter().any(|agent| agent.as_str() == id.as_str()))
    }

    /// The stage's observability feed binding in the engine's `RunStage`
    /// vocabulary. `None` = no feed entry for the stage (or unknown tag).
    pub fn feed_for(&self, tag: &str) -> Option<RunStage> {
        self.stage_by_tag(tag).and_then(|stage| stage.effective_feed)
    }

    /// Whether the stage with the given tag is a gate (Review/Acceptance
    /// kind). Unknown tags are not gates.
    pub fn is_gate(&self, tag: &str) -> bool {
        self.stage_by_tag(tag).is_some_and(|stage| stage.def.is_gate())
    }

    /// The maximum dispatch cycles for a relationship, resolved to the gate
    /// kind's engine default.
    ///
    /// The blueprint's open relationship registry governs **topology**
    /// (`from`→`to` pairs and their closed kind), never cycle caps — a
    /// relationship row carries no cap (parity.rs §5). The cap is a property
    /// of the gate's `StageKind` ([`StageKind::default_max_cycles`]:
    /// Review → 3, Acceptance → 2), which callers pass as `kind_default`. An
    /// unmatched pair resolves to the same fallback the pre-blueprint
    /// `RelationshipManager::max_cycles(from, to, fallback)` returns with no
    /// rule (relationship.rs:88-90), so standard behavior is preserved.
    pub fn max_cycles(&self, _from: &AgentId, _to: &AgentId, kind_default: u32) -> u32 {
        kind_default
    }

    /// Roles that require a tool-calling model, preserving the full legacy
    /// disjunction (design doc §4 Q5 pin; `runtime_runner::tool_calling_roles_for`):
    ///
    /// - Builtin seeds require tool calling when staffed in a stage whose tag
    ///   keeps the pre-ADR-35 requirement (research/implement/validate) **or**
    ///   their effective capabilities include `fs_write`/`shell`.
    /// - Custom agents hold the shared executor, so any capability implies
    ///   tool calling; explicitly capability-free agents need none.
    /// - The coordinator is constructed in code and never requires tool
    ///   calling.
    ///
    /// On the default `standard` blueprint this reproduces exactly the legacy
    /// set `["researcher", "coder", "validator"]`.
    pub fn tool_calling_roles(
        &self,
        topology: &[AgentId],
        agent_configs: &HashMap<AgentId, CustomAgentConfig>,
    ) -> HashSet<AgentId> {
        let seed_ids: HashSet<String> =
            builtin_agent_seeds().iter().map(|seed| seed.id.clone()).collect();
        // Blueprint-driven legacy classification: agents staffed in a stage
        // whose tag keeps the pre-ADR-35 tool-calling requirement. On
        // `standard` this equals the seed-stage classification
        // (runtime_runner.rs:212-220).
        let legacy_ids: HashSet<String> = self
            .resolved
            .stages
            .iter()
            .filter(|stage| matches!(stage.def.tag.as_str(), "research" | "implement" | "validate"))
            .flat_map(|stage| stage.def.agents.iter().cloned())
            .collect();
        let legacy = |role: &str| legacy_ids.contains(role);
        let is_builtin = |role: &str| seed_ids.contains(role);
        let mut tool_calling = HashSet::new();
        for role in topology {
            if role.as_str() == "coordinator" {
                continue;
            }
            let requires = match agent_configs.get(role) {
                Some(cfg) => {
                    let effective = cfg.capabilities.effective();
                    let any_cap = effective.fs_read
                        || effective.fs_write
                        || effective.shell
                        || effective.git
                        || effective.lsp;
                    if any_cap {
                        if is_builtin(role.as_str()) {
                            legacy(role.as_str()) || effective.fs_write || effective.shell
                        } else {
                            // Custom agents hold the shared executor.
                            true
                        }
                    } else {
                        // Explicitly capability-free: the agent needs no tools.
                        false
                    }
                }
                None => legacy(role.as_str()),
            };
            if requires {
                tool_calling.insert(role.clone());
            }
        }
        tool_calling
    }

    /// The resolved per-agent capabilities for `id` as registered from
    /// `seed` (the merged seed/user config): the seed's `effective()` shape
    /// with the `fs_write`/`shell` flags overlaid by the staffing stage's
    /// write mask (ADR-58 D1).
    ///
    /// This generalizes exactly the overlay the P1 parity test computes
    /// (tests/parity.rs:204-221): on `standard` the coder resolves to
    /// `{fs_read, fs_write, shell, git, lsp, eval} = {f,t,t,f,f,t}` and the
    /// four non-Execution specialists keep their seed shape `{f,f,f,f,f,t}`.
    /// Roles the blueprint does not staff (Freeform / `run_once` custom
    /// agents) pass their seed `effective()` through unchanged — no mask
    /// overlay (their write gate stays the engine's no-mask default).
    pub fn effective_capabilities_for(
        &self,
        seed: &CustomAgentConfig,
        id: &AgentId,
    ) -> ResolvedCapabilities {
        let effective = seed.capabilities.effective();
        let mask = self.stage_for_agent(id).map(|stage| stage.effective_capabilities);
        ResolvedCapabilities {
            fs_read: effective.fs_read,
            fs_write: mask.map_or(effective.fs_write, |mask| mask.fs_write),
            shell: mask.map_or(effective.shell, |mask| mask.shell),
            git: effective.git,
            lsp: effective.lsp,
            eval: effective.eval,
        }
    }

    /// Resolve a relationship *kind string* to its closed engine semantics
    /// through the blueprint's open relationship registry.
    ///
    /// The registry lookup is a **hard error** on an unmatched kind (design
    /// doc §4 F7 review): under `[orchestration]` a typo'd legacy relationship
    /// must not be silently dropped — mirroring the load-time guarantee
    /// `validate_multi_agent_relationships` provides today (lib.rs).
    ///
    /// The closed legacy kind `reports_to` is folded into the catalog as
    /// `Delegation` (design doc §7 Q3 review): it sits in the same family the
    /// default `supervises` rows already use, preserves the closed-list
    /// semantics, and stays zero-delta on `standard` — which registers no
    /// `reports_to` row. Genuinely unknown kinds still hard-error (F7).
    pub fn relationship_semantics(
        &self,
        kind: &str,
    ) -> Result<RelationshipSemantics, BlueprintError> {
        if kind == "reports_to" {
            // Q3 (review): `ReportsTo` semantics accepted as `Delegation`.
            return Ok(RelationshipSemantics::Delegation);
        }
        self.resolved
            .relationship_defaults
            .iter()
            .find(|rule| rule.kind == kind)
            .map(|rule| rule.semantics)
            .ok_or_else(|| {
                BlueprintError::Validation(format!(
                    "blueprint relationship registry has no kind '{kind}' — an unmatched \
                     legacy relationship must not be silently dropped (ADR-58 F7 review)"
                ))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blueprint::{OrchestrationConfig, StageKind};
    use crate::schema::{AgentCapabilities, PromptSections};
    use concerto_core::types::{AgentStage, OutputMode};

    fn standard_facade() -> BlueprintFacade {
        let resolved = OrchestrationConfig::default()
            .resolve(&[], None)
            .expect("the standard blueprint must validate and resolve");
        BlueprintFacade::new(&resolved)
    }

    fn agent_config(id: &str, capabilities: AgentCapabilities) -> CustomAgentConfig {
        CustomAgentConfig {
            id: id.into(),
            name: id.into(),
            role: id.into(),
            stage: None,
            prompt_sections: PromptSections::default(),
            capabilities,
            is_custom: true,
            output_mode: OutputMode::default(),
            ..Default::default()
        }
    }

    fn five_seed_topology() -> Vec<AgentId> {
        ["architect", "researcher", "coder", "reviewer", "validator", "coordinator"]
            .iter()
            .map(|id| AgentId::new(*id))
            .collect()
    }

    #[test]
    fn stage_lookups_resolve_on_standard() {
        let facade = standard_facade();
        assert_eq!(
            facade.stage_by_tag("implement").expect("stage").def.known_kind(),
            Some(StageKind::Execution)
        );
        assert_eq!(
            facade.stage_by_tag("review").expect("stage").def.known_kind(),
            Some(StageKind::Review)
        );
        assert!(facade.stage_by_tag("nope").is_none());

        let coder = AgentId::new("coder");
        assert_eq!(facade.stage_for_agent(&coder).expect("staffed").def.tag, "implement");
        assert_eq!(
            facade.stage_for_agent(&AgentId::new("architect")).expect("staffed").def.tag,
            "design"
        );
        assert!(facade.stage_for_agent(&AgentId::new("unknown")).is_none());

        assert_eq!(facade.feed_for("research"), Some(RunStage::Understand));
        assert_eq!(facade.feed_for("implement"), Some(RunStage::Execute));
        assert_eq!(facade.feed_for("validate"), Some(RunStage::Verify));
        assert_eq!(facade.feed_for("nope"), None);

        assert!(facade.is_gate("review"), "Review kind is a gate");
        assert!(facade.is_gate("validate"), "Acceptance kind is a gate");
        assert!(!facade.is_gate("research"));
        assert!(!facade.is_gate("nope"));
    }

    /// Issue #150: kind-based lookups must follow RENAMED stage tags. The
    /// coordinator's gate/feed/label/fallback resolution keys off
    /// `first_stage_of_kind` — a blueprint that renames the review stage to
    /// `quality` or the acceptance stage to `ship` keeps its gate cycles
    /// exactly where the renamed stage is.
    #[test]
    fn first_stage_of_kind_follows_renamed_tags() {
        use crate::blueprint::{
            Blueprint, CapabilityMask, PipelineDef, StageCondition, StageDef, StageFlags,
        };
        use std::collections::HashMap;

        let stage = |tag: &str, label: &str, kind: &str, agents: &[&str]| StageDef {
            tag: tag.into(),
            label: label.into(),
            kind: kind.to_string(),
            version: 1,
            flags: StageFlags::default(),
            condition: StageCondition::Always,
            max_cycles: None,
            feed: None,
            primary: false,
            agents: agents.iter().map(|a| (*a).to_string()).collect(),
            fallback: None,
            files: None,
        };
        let defs = vec![
            stage("research", "Research", StageKind::Research.as_str(), &["researcher"]),
            stage("build", "Build", StageKind::Execution.as_str(), &["coder"]),
            stage("quality", "Quality Gate", StageKind::Review.as_str(), &["reviewer"]),
            stage("ship", "Ship", StageKind::Acceptance.as_str(), &["validator"]),
            stage("blogger", "Blogger", "custom-draft", &["writer"]),
        ];
        let resolved = ResolvedBlueprint {
            blueprint: Blueprint {
                schema_version: 1,
                name: "renamed-tags".into(),
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
        let facade = BlueprintFacade::new(&resolved);

        // Renamed known kinds resolve to the renamed tags.
        assert_eq!(facade.first_stage_of_kind(StageKind::Execution).unwrap().def.tag, "build");
        assert_eq!(facade.first_stage_of_kind(StageKind::Review).unwrap().def.tag, "quality");
        assert_eq!(facade.first_stage_of_kind(StageKind::Acceptance).unwrap().def.tag, "ship");
        assert_eq!(facade.first_stage_of_kind(StageKind::Research).unwrap().def.tag, "research");
        // A kind absent from the pipeline resolves to None (no canonical
        // fallback at the facade level).
        assert!(facade.first_stage_of_kind(StageKind::Planning).is_none());
        // Unknown user kinds never match a known-kind lookup.
        assert_eq!(facade.first_stage_of_kind(StageKind::Planning), None);
        assert!(facade.stage_by_tag("blogger").is_some_and(|s| s.def.known_kind().is_none()));
        // Canonical lookups by tag still work for the stages that carry them.
        assert_eq!(facade.stage_by_tag("quality").unwrap().def.kind, StageKind::Review.as_str());
    }

    #[test]
    fn tool_calling_roles_preserve_legacy_disjunction() {
        let facade = standard_facade();
        let topology = five_seed_topology();
        let empty = HashMap::new();

        // Unconfigured: exactly the legacy set on standard.
        let default_roles = facade.tool_calling_roles(&topology, &empty);
        let mut expected: HashSet<AgentId> =
            ["researcher", "coder", "validator"].iter().map(|id| AgentId::new(*id)).collect();
        assert_eq!(default_roles, expected, "legacy default on standard");

        // A read-only builtin (architect: fs_read) must NOT be tool-calling —
        // the builtin branch evaluates the legacy disjunction, not any-cap.
        let mut configs = HashMap::new();
        configs.insert(
            AgentId::new("architect"),
            agent_config(
                "architect",
                AgentCapabilities { fs_read: Some(true), ..Default::default() },
            ),
        );
        assert_eq!(
            facade.tool_calling_roles(&topology, &configs),
            expected,
            "read-only builtins never force tool calling"
        );

        // A custom agent with any capability holds the shared executor → true.
        let mut extended_topology = topology.clone();
        extended_topology.push(AgentId::new("qa"));
        configs.insert(
            AgentId::new("qa"),
            agent_config("qa", AgentCapabilities { fs_read: Some(true), ..Default::default() }),
        );
        expected.insert(AgentId::new("qa"));
        assert_eq!(facade.tool_calling_roles(&extended_topology, &configs), expected);

        // An explicitly capability-free custom agent needs no tools.
        let mut free_configs = HashMap::new();
        free_configs.insert(
            AgentId::new("qa"),
            agent_config("qa", AgentCapabilities { fs_read: Some(false), ..Default::default() }),
        );
        let mut without_qa: HashSet<AgentId> =
            ["researcher", "coder", "validator"].iter().map(|id| AgentId::new(*id)).collect();
        assert_eq!(facade.tool_calling_roles(&extended_topology, &free_configs), without_qa);

        // A write-capable builtin stays tool-calling even when its legacy
        // stage is overridden by config (write capabilities force it).
        configs.remove(&AgentId::new("qa"));
        configs.insert(
            AgentId::new("coder"),
            agent_config("coder", AgentCapabilities { fs_write: Some(true), ..Default::default() }),
        );
        without_qa.insert(AgentId::new("coder"));
        assert_eq!(facade.tool_calling_roles(&extended_topology, &configs), without_qa);

        // The coordinator is never tool-calling even with a config entry.
        configs.insert(
            AgentId::new("coordinator"),
            agent_config(
                "coordinator",
                AgentCapabilities { fs_write: Some(true), ..Default::default() },
            ),
        );
        assert_eq!(facade.tool_calling_roles(&extended_topology, &configs), without_qa);
    }

    #[test]
    fn effective_capabilities_for_matches_parity_overlay() {
        // Mirrors tests/parity.rs:204-221: seed `effective()` overlaid with
        // the staffing stage's write mask. On `standard` only the coder
        // (Execution-kind implement stage) gains fs_write/shell.
        let facade = standard_facade();
        for seed in builtin_agent_seeds() {
            let id = AgentId::new(&seed.id);
            let resolved = facade.effective_capabilities_for(&seed, &id);
            let (fs_write, shell) = match seed.id.as_str() {
                "coder" => (true, true),
                _ => (false, false),
            };
            assert_eq!(
                resolved,
                ResolvedCapabilities {
                    fs_read: false,
                    fs_write,
                    shell,
                    git: false,
                    lsp: false,
                    eval: true,
                },
                "resolved shape for {}",
                seed.id
            );
        }
    }

    #[test]
    fn effective_capabilities_for_unstaffed_role_passes_seed_through() {
        // Freeform/run_once custom agents are not staffed: no mask overlay,
        // so an explicit write capability survives untouched.
        let facade = standard_facade();
        let writer = agent_config(
            "docs-writer",
            AgentCapabilities { fs_write: Some(true), shell: Some(true), ..Default::default() },
        );
        let resolved = facade.effective_capabilities_for(&writer, &AgentId::new("docs-writer"));
        assert!(resolved.fs_write && resolved.shell, "unstaffed role keeps seed effective()");
    }

    #[test]
    fn stage_kind_and_primary_execution_stage_resolve_on_standard() {
        let facade = standard_facade();
        assert_eq!(facade.stage_kind("implement"), Some(StageKind::Execution));
        assert_eq!(facade.stage_kind("design"), Some(StageKind::Planning));
        assert_eq!(facade.stage_kind("nope"), None);

        let primary = facade.primary_execution_stage().expect("standard has a primary stage");
        assert_eq!(primary.def.tag, "implement");
        assert_eq!(primary.def.known_kind(), Some(StageKind::Execution));
        assert_eq!(primary.def.agents, vec!["coder".to_string()]);
    }

    #[test]
    fn relationship_semantics_resolves_open_kinds_and_reports_to() {
        let facade = standard_facade();
        // Registered open kinds on `standard` (standard_relationships()).
        assert_eq!(
            facade.relationship_semantics("supervises").expect("registered kind"),
            RelationshipSemantics::Delegation
        );
        assert_eq!(
            facade.relationship_semantics("provides_context_to").expect("registered kind"),
            RelationshipSemantics::ContextFlow
        );
        assert_eq!(
            facade.relationship_semantics("owns_design").expect("registered kind"),
            RelationshipSemantics::Delegation
        );
        // The closed legacy `reports_to` kind folds into the open catalog as
        // `Delegation` (design doc §7 Q3 review); `standard` registers no
        // `reports_to` row, so this is a zero-delta catalog fold, not a
        // shipped row.
        assert_eq!(
            facade.relationship_semantics("reports_to").expect("legacy kind"),
            RelationshipSemantics::Delegation
        );
        // Genuinely unknown kinds remain hard errors, never silent None (F7).
        assert!(facade.relationship_semantics("banana").is_err(), "typo'd kind");
    }

    #[test]
    fn max_cycles_resolves_to_kind_default() {
        let facade = standard_facade();
        // The cap is a StageKind property (Review → 3, Acceptance → 2); the
        // relationship registry governs topology only (parity.rs §5).
        assert_eq!(facade.max_cycles(&AgentId::new("reviewer"), &AgentId::new("coder"), 3), 3);
        assert_eq!(facade.max_cycles(&AgentId::new("validator"), &AgentId::new("coder"), 2), 2);
        assert_eq!(facade.max_cycles(&AgentId::new("nope"), &AgentId::new("nope"), 3), 3);
    }

    #[test]
    fn agent_stage_consts_align_with_standard_tags() {
        // The coordinator guards map AgentStage ↔ blueprint stage tag by
        // string; pin the five canonical tags against the standard blueprint
        // staffing so the mapping can never drift.
        let facade = standard_facade();
        for stage in [
            AgentStage::DESIGN,
            AgentStage::RESEARCH,
            AgentStage::IMPLEMENT,
            AgentStage::REVIEW,
            AgentStage::VALIDATE,
        ] {
            assert!(facade.stage_by_tag(stage).is_some(), "stage {stage} must exist on standard");
        }
    }

    /// Slice 1b: the tolerant `primary_execution_stage` resolution chain. A
    /// primary-flagged non-`Execution`/unknown-kind stage (legal since rule
    /// (a) was removed) does not hijack the resolution — the first known
    /// `Execution` stage wins — and an `Execution`-free blueprint resolves to
    /// `None` with no panic for callers.
    #[test]
    fn primary_execution_stage_falls_back_through_tolerant_chain() {
        use crate::blueprint::{Blueprint, PipelineDef, StageCondition, StageDef, StageFlags};

        let stage = |tag: &str, kind: &str, primary: bool| StageDef {
            tag: tag.into(),
            label: tag.into(),
            kind: kind.into(),
            version: 1,
            flags: StageFlags::default(),
            condition: StageCondition::Always,
            max_cycles: None,
            feed: None,
            primary,
            agents: Vec::new(),
            fallback: None,
            files: None,
        };
        let resolve = |name: &str, stages: Vec<StageDef>| {
            BlueprintFacade::new(
                &crate::blueprint::resolve_blueprint(&Blueprint {
                    schema_version: crate::ORCHESTRATION_SCHEMA_VERSION,
                    name: name.into(),
                    description: None,
                    pipeline: PipelineDef { stages },
                    relationships: Vec::new(),
                })
                .expect("the test blueprint must validate and resolve"),
            )
        };

        // A Planning-kind `design` stage flagged `primary` (removed rule
        // (a)): the resolution must skip it and pick the known Execution
        // stage.
        let facade = resolve(
            "non-execution-primary",
            vec![stage("design", "planning", true), stage("build", "execution", false)],
        );
        let primary = facade.primary_execution_stage().expect("falls back to the Execution stage");
        assert_eq!(
            primary.def.tag, "build",
            "a primary-flagged non-Execution stage must not hijack the resolution"
        );
        assert_eq!(primary.def.known_kind(), Some(StageKind::Execution));

        // An unknown user kind flagged `primary` behaves the same: unknown
        // kinds carry no engine defaults, so resolution falls through to the
        // known Execution stage.
        let facade = resolve(
            "unknown-primary",
            vec![stage("blogger", "blogger", true), stage("build", "execution", false)],
        );
        assert_eq!(
            facade.primary_execution_stage().expect("falls back to the Execution stage").def.tag,
            "build",
            "an unknown-kind primary must not hijack the resolution"
        );

        // An Execution-free blueprint resolves to `None` — callers degrade
        // with their legacy fallback, never with a panic.
        let facade = resolve(
            "no-execution",
            vec![stage("design", "planning", true), stage("research", "research", false)],
        );
        assert_eq!(facade.primary_execution_stage(), None, "no known Execution kind");
    }
}
