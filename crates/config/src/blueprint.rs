//! Orchestration Blueprint data model (ADR-58, P1).
//!
//! The blueprint is a config-as-data pipeline: an open stage registry over
//! **string kind tags**, an ordered pipeline, per-stage lifecycle data
//! (`condition`, `max_cycles`, `feed`), fallback personas, and an open
//! relationship registry over closed semantics.
//!
//! Stage kinds are OPEN: the six engine kinds
//! (`research`/`planning`/`execution`/`review`/`acceptance`/`run_once`) are
//! the known-kind vocabulary that keeps engine semantics; an arbitrary user
//! kind string is a valid stage that carries no engine defaults (no writes,
//! not a gate, one cycle). See [`StageKind::parse`] / [`StageDef::known_kind`].
//!
//! Load-time validation applies the relaxed rulebook (ADR-58 D2/D5, blueprint
//! §5.3): it keeps only the integrity and safety gates — unstaffed non-
//! `Acceptance` gates need a fallback (c), no self-fallback (d), no zero
//! cycle caps (e), a bounded sum of caps (f), unique non-empty tags (g), and
//! no reserved-name collisions (j). The structural gates (a) exactly-one
//! primary, (b) terminal reachability, (d) fallback narrowing/widening, and
//! (i) `OnGateCycle`-requires-gate are removed; resolution derives each
//! stage's effective write mask (`fs_write`/`shell`, from its known-kind
//! default overlaid with any explicit stage flags — unknown kinds grant no
//! writes unless explicit flags grant them) and its observability feed
//! binding (blueprint §5.6).
//!
//! P1 scope: data model + validation + resolution + the named blueprint
//! catalog (standard / tdd / docs-only / research-only) + include-file
//! loading. The runtime is **not** rewritten in P1 (that is the P2+P3
//! table-driven coordinator rewrite); nothing in this module changes how the
//! engine executes today. `[orchestration]` is additive and defaults to
//! `None`, so every pre-existing config loads unchanged (legacy equivalence).

use concerto_core::RunStage;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Schema version of the orchestration section / blueprint include file
/// (ADR-58 §6, blueprint §5.10). The orchestration section and every
/// blueprint block deserialize with `deny_unknown_fields`, so a future key
/// removal inside the orchestration section becomes a hard breaking change
/// gated only by this version.
pub const ORCHESTRATION_SCHEMA_VERSION: u32 = 1;

/// Name of the blueprint include file (ADR-58 D4): Studio writes a clean,
/// mergeable section here so the user's `config.toml` is never rewritten
/// wholesale — TOML round-trips lose comments, which is exactly why the
/// blueprint lives in its own file.
pub const BLUEPRINT_INCLUDE_FILE: &str = "orchestration.blueprint.toml";

/// Reserved identities (ADR-58 rulebook (j)): the coordinator's provider
/// sentinel and the coordinator identity itself. A **stage tag** colliding
/// with one of these (e.g. a custom stage tagged `coordinator`) is a hard
/// load error. Fallback persona *ids* are not restricted here — the sentinel
/// is the sanctioned fallback identity for an unstaffed `Execution` stage.
pub const RESERVED_BLUEPRINT_NAMES: &[&str] = &["coordinator", "coordinator-self-execute"];

/// The four named blueprint variants shipped with P1 (ADR-58 D3).
pub const NAMED_BLUEPRINTS: &[&str] = &["standard", "tdd", "docs-only", "research-only"];

/// The two write-capability flags governed by the stage-kind catalog
/// (ADR-58 D1). Everything else (`fs_read`/`git`/`lsp`/`eval`) stays
/// per-agent config and is not part of a stage mask.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityMask {
    #[serde(default)]
    pub fs_write: bool,
    #[serde(default)]
    pub shell: bool,
}

/// The six **known engine kinds** (ADR-58 D2, blueprint §5.1).
///
/// `StageDef.kind` is an open string; these six are the known-kind
/// vocabulary. A stage whose kind string parses to one of these ([`StageKind::parse`])
/// keeps the closed semantics behind the pre-blueprint stage vocabulary — the
/// engine behaviors with safety consequences (writes, gates, cycle caps) are
/// reachable only through the catalog. An arbitrary user kind string is a
/// valid stage that carries none of those defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StageKind {
    /// Context gathering; no writes. (Today: `research`.)
    Research,
    /// Plan/design-doc production only. (Today: `design`.)
    Planning,
    /// **Owns `plan.files`**; write-granted. (Today: `implement`.)
    Execution,
    /// Iterative gate with a cycle cap. (Today: `review`.)
    Review,
    /// Final gate; if unstaffed, falls back to a persona. (Today: `validate`.)
    Acceptance,
    /// Run-once with full context and no lifecycle — today's unknown-tag
    /// semantics as a first-class kind (ADR-58 D2).
    RunOnce,
}

impl StageKind {
    /// Parse a snake_case kind string into the known-kind vocabulary:
    /// `"research"`, `"planning"`, `"execution"`, `"review"`,
    /// `"acceptance"`, `"run_once"`. Anything else is `None` — a valid
    /// unknown user kind with no engine defaults.
    pub fn parse(s: &str) -> Option<StageKind> {
        match s {
            "research" => Some(Self::Research),
            "planning" => Some(Self::Planning),
            "execution" => Some(Self::Execution),
            "review" => Some(Self::Review),
            "acceptance" => Some(Self::Acceptance),
            "run_once" => Some(Self::RunOnce),
            _ => None,
        }
    }

    /// The snake_case config string for a known kind — the exact spelling the
    /// engine previously read through `serde`'s `rename_all = "snake_case"`
    /// ([`StageKind::parse`] is its inverse).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Research => "research",
            Self::Planning => "planning",
            Self::Execution => "execution",
            Self::Review => "review",
            Self::Acceptance => "acceptance",
            Self::RunOnce => "run_once",
        }
    }

    /// Display label for the known kind.
    pub fn label(self) -> &'static str {
        match self {
            Self::Research => "Research",
            Self::Planning => "Planning",
            Self::Execution => "Execution",
            Self::Review => "Review",
            Self::Acceptance => "Acceptance",
            Self::RunOnce => "RunOnce",
        }
    }

    /// Whether the kind can terminate a pipeline (rulebook (b)).
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Acceptance | Self::RunOnce)
    }

    /// Whether the kind is a gate (rulebook (c)). A gate is a stage that may
    /// loop against a cycle cap and gate downstream progress.
    pub fn is_gate(self) -> bool {
        matches!(self, Self::Review | Self::Acceptance)
    }

    /// Stage-kind → default write mask (ADR-58 D1): `Execution` grants
    /// `fs_write` + `shell`; every other kind grants neither.
    pub fn default_capability_mask(self) -> CapabilityMask {
        match self {
            Self::Execution => CapabilityMask { fs_write: true, shell: true },
            _ => CapabilityMask::default(),
        }
    }

    /// Engine-default cycle cap for the kind (used by rulebook (f) when a
    /// stage has no explicit `max_cycles`). Mirrors today's coordinator:
    /// review/validate gates loop up to 3/2 (the `CollaborationRule`
    /// fallbacks at `relationship.rs`), everything else runs once.
    pub fn default_max_cycles(self) -> u32 {
        match self {
            Self::Review => 3,
            Self::Acceptance => 2,
            _ => 1,
        }
    }
}

/// Stage-block flags drawn from the fixed engine-capability catalog
/// (ADR-58 D2). `None` inherits the stage kind's default mask; an explicit
/// value overrides it (custom stages are flag composites of the catalog).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StageFlags {
    #[serde(default)]
    pub fs_write: Option<bool>,
    #[serde(default)]
    pub shell: Option<bool>,
}

/// Predicate name from the closed **condition catalog** (ADR-58 §5.3,
/// rulebook (i)) — no conditionals over arbitrary expressions, no scripts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StageCondition {
    /// The stage always runs when the pipeline reaches it (the default).
    #[default]
    Always,
    /// The stage runs only after a gate cycle has completed. The rulebook no
    /// longer restricts the predicate to gate kinds — any stage may declare
    /// it (on non-gate stages the condition is inert; the engine resolves
    /// semantics from the stage).
    OnGateCycle,
}

/// Closed observability feed catalog (ADR-58 §5.6, rulebook (h)).
///
/// The label maps to the engine's `RunStage` vocabulary; `None` on a stage
/// means no feed entry (e.g. the `RunOnce` kind). `EventKind` stays closed —
/// custom stages bind to one of these or emit nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeedLabel {
    Understand,
    Plan,
    Execute,
    Verify,
}

impl FeedLabel {
    /// Map the label to the engine's `RunStage` feed vocabulary.
    pub fn to_run_stage(self) -> RunStage {
        match self {
            Self::Understand => RunStage::Understand,
            Self::Plan => RunStage::Plan,
            Self::Execute => RunStage::Execute,
            Self::Verify => RunStage::Verify,
        }
    }
}

/// Per-stage default persona used when the stage is unstaffed (ADR-58 §5.5,
/// replacing `self_implement_agent`/`self_verify`).
///
/// Fallback records carry explicit flags drawn from the fixed engine catalog
/// that are **plain flags** overlaying the stage-kind default: an absent flag
/// (`None`) inherits the staging stage-kind mask, an explicit `Some(value)`
/// overrides it outright (no narrowing/widening gate — the removed rulebook
/// (d) checks). Fallback prompts render only when the stage is actually
/// unstaffed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FallbackPersonaDef {
    /// Identity id. Must differ from any agent staffed in the same stage
    /// (rulebook (d)). The `coordinator-self-execute` sentinel is the
    /// sanctioned fallback id for an unstaffed `Execution` stage.
    pub id: String,
    /// Human-readable persona label (rendered through the fixed role
    /// template — no free inline prompts).
    pub label: String,
    /// Supplementary system instructions rendered only when unstaffed.
    #[serde(default)]
    pub system_instructions: Option<String>,
    /// Explicit capability flags defaulting to the stage-kind mask
    /// ([`FallbackPersonaDef::effective_capabilities`]). Plain flags — an
    /// explicit value may narrow or widen freely.
    #[serde(default)]
    pub capabilities: StageFlags,
}

impl FallbackPersonaDef {
    /// Effective fallback write mask: explicit flags override the staging
    /// stage-kind's default mask; absent flags inherit it (ADR-58 D1 §5.5).
    /// Mirrors [`StageDef::effective_capabilities`] for stage flags.
    pub fn effective_capabilities(&self, stage_kind: StageKind) -> CapabilityMask {
        let base = stage_kind.default_capability_mask();
        CapabilityMask {
            fs_write: self.capabilities.fs_write.unwrap_or(base.fs_write),
            shell: self.capabilities.shell.unwrap_or(base.shell),
        }
    }
}

/// Plan-artifact contract for an `Execution`-kind stage (blueprint §5.7).
///
/// `Execution`-kind *custom* stages carry this field so plan semantics do not
/// fork: ownership and `expected_artifacts` key off this block (never off the
/// tag string) for any custom `Execution` stage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionFilesDef {
    /// Path/pattern the stage owns within `plan.files`.
    pub ownership: String,
    /// Artifacts the stage is expected to produce.
    #[serde(default)]
    pub expected_artifacts: Vec<String>,
}

fn default_schema_version() -> u32 {
    ORCHESTRATION_SCHEMA_VERSION
}

fn default_stage_version() -> u32 {
    1
}

/// A registered stage block: `{tag, label, kind, version, flags}` plus
/// lifecycle data (ADR-58 D2, blueprint §5.1/§5.3).
///
/// The stage tag is the registry key; every block deserializes with
/// `deny_unknown_fields` (the deliberate asymmetry with the crate's lenient
/// `AppConfig` — see ADR-58 §6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StageDef {
    /// Registry key; must be non-empty, unique, and not reserved (rulebook
    /// (g)/(j)).
    pub tag: String,
    /// Human-readable label.
    pub label: String,
    /// The stage's kind tag — an **open** string. The known-kind vocabulary
    /// (`"research"`/`"planning"`/`"execution"`/`"review"`/`"acceptance"`/
    /// `"run_once"`, see [`StageKind::parse`]) keeps the engine semantics;
    /// arbitrary strings are valid unknown user kinds carrying no engine
    /// defaults.
    pub kind: String,
    /// Semantics version this stage was written against (blueprint §5.10).
    #[serde(default = "default_stage_version")]
    pub version: u32,
    /// Flags drawn from the fixed engine-capability catalog.
    #[serde(default)]
    pub flags: StageFlags,
    /// Predicate name from the closed condition catalog; any stage may
    /// declare `OnGateCycle` (the removed rulebook (i) gate).
    #[serde(default)]
    pub condition: StageCondition,
    /// Per-stage loop cap; `None` = the kind's engine default. A value of 0
    /// is rejected (rulebook (e)).
    #[serde(default)]
    pub max_cycles: Option<u32>,
    /// Observability feed binding (rulebook (h)); `None` = no feed entry.
    #[serde(default)]
    pub feed: Option<FeedLabel>,
    /// Whether this is a primary `Execution` stage. Purely declarative — the
    /// rulebook no longer enforces exactly-one ([`validate_blueprint`]).
    #[serde(default)]
    pub primary: bool,
    /// Agent ids staffed in this stage (rulebook (c)/(d)).
    #[serde(default)]
    pub agents: Vec<String>,
    /// Default persona used when the stage is unstaffed (rulebook (c)/(d)).
    #[serde(default)]
    pub fallback: Option<FallbackPersonaDef>,
    /// Plan-artifact contract for custom `Execution` stages (blueprint §5.7).
    #[serde(default)]
    pub files: Option<ExecutionFilesDef>,
}

impl StageDef {
    /// Parse the kind string into the known-kind vocabulary. `None` = a valid
    /// unknown user kind.
    pub fn known_kind(&self) -> Option<StageKind> {
        StageKind::parse(&self.kind)
    }

    /// Whether the stage is a gate: a known `Review`/`Acceptance` kind.
    /// Unknown kinds are never gates.
    pub fn is_gate(&self) -> bool {
        self.known_kind().is_some_and(StageKind::is_gate)
    }

    /// The engine-default cycle cap for the stage: the known kind's default
    /// (Review → 3, Acceptance → 2), or 1 for unknown kinds and every other
    /// kind.
    pub fn default_max_cycles(&self) -> u32 {
        self.known_kind().map_or(1, StageKind::default_max_cycles)
    }

    /// Effective write mask: the known-kind default overlaid with any explicit
    /// catalog flags (ADR-58 D1 resolution semantics). Unknown kinds grant no
    /// writes unless explicit flags grant them.
    pub fn effective_capabilities(&self) -> CapabilityMask {
        let base = self
            .known_kind()
            .map_or_else(CapabilityMask::default, StageKind::default_capability_mask);
        CapabilityMask {
            fs_write: self.flags.fs_write.unwrap_or(base.fs_write),
            shell: self.flags.shell.unwrap_or(base.shell),
        }
    }

    /// The observability feed binding resolved to the engine's `RunStage`
    /// vocabulary.
    pub fn effective_feed(&self) -> Option<RunStage> {
        self.feed.map(FeedLabel::to_run_stage)
    }
}

/// Ordered pipeline definition (blueprint §5.3): the pipeline control flow is
/// the ordered stage list itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PipelineDef {
    pub stages: Vec<StageDef>,
}

/// Closed engine semantics behind an open relationship kind (ADR-58 §4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelationshipSemantics {
    /// The `from` agent gates/reviews the `to` agent's work (today: `ReportsTo`).
    ApprovalGate,
    /// Context flows from `from` to `to` (today: `ProvidesContextTo`).
    ContextFlow,
    /// `from` delegates/oversees work performed by `to` (today: `Supervises` /
    /// `OwnsDesign`).
    Delegation,
}

/// A registered relationship kind: open name over closed semantics
/// (ADR-58 §4). The registry of kind names is open; each registered kind
/// references closed engine semantics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RelationshipDef {
    /// Kind name in the open registry (e.g. `supervises`). Unmatched config
    /// strings are no longer a silent `None` — they fail deserialization.
    pub kind: String,
    /// The closed engine semantics the kind references.
    pub semantics: RelationshipSemantics,
    pub from: String,
    pub to: String,
}

/// A complete Orchestration Blueprint (ADR-58 D4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Blueprint {
    /// Blueprint schema version (`ORCHESTRATION_SCHEMA_VERSION`).
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    /// Pipeline name.
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    /// The ordered pipeline.
    pub pipeline: PipelineDef,
    /// Open relationship registry over closed semantics.
    #[serde(default)]
    pub relationships: Vec<RelationshipDef>,
}

/// A resolved stage: the validated definition plus everything the engine needs
/// to answer "may this actor write here" and "which feed does this run show".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedStage {
    pub def: StageDef,
    /// Effective write mask (kind default overlaid with explicit flags).
    pub effective_capabilities: CapabilityMask,
    /// Feed binding mapped to the engine's `RunStage` vocabulary.
    pub effective_feed: Option<RunStage>,
}

/// The fully resolved, validated blueprint (ADR-58 D1 §5.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedBlueprint {
    pub blueprint: Blueprint,
    /// Stages in pipeline order with resolved masks/feeds.
    pub stages: Vec<ResolvedStage>,
    /// Feed binding by stage tag (mirror of `stages` for lookup).
    pub feed_map: HashMap<String, Option<RunStage>>,
    /// Relationship registry carrying the pipeline's defaults.
    pub relationship_defaults: Vec<RelationshipDef>,
}

/// Errors produced while loading, validating, or resolving a blueprint.
#[derive(Debug, Clone, thiserror::Error)]
pub enum BlueprintError {
    /// A relaxed-rulebook (c)/(d)/(e)/(f)/(g)/(j) or schema-version violation.
    #[error("blueprint validation failed: {0}")]
    Validation(String),
    /// A structured rulebook violation carrying a field path, a
    /// machine-readable code (`"rule_c"`…`"rule_j"` — the letters of the
    /// remaining rules), and a user-facing message for per-field Studio
    /// surfacing (ADR-59 Decision 5). The `Display` output keeps the exact
    /// `rule (x): ...` prefix of the legacy messages so existing
    /// `.contains("rule (x)")` assertions stay valid.
    #[error("rule ({letter}): {message}", letter = code.trim_start_matches("rule_"))]
    Rule { field: String, code: &'static str, message: String },
    /// `name` referenced a named blueprint that is not in the shipped catalog.
    #[error("unknown named blueprint '{0}' (known: {1})")]
    UnknownNamedBlueprint(String, String),
    /// The include-file path did not exist.
    #[error("blueprint include file not found: {0}")]
    MissingIncludeFile(String),
    /// The include file could not be read or parsed as a [`Blueprint`].
    #[error("failed to load blueprint include file '{path}': {detail}")]
    ParseIncludeFile { path: String, detail: String },
    /// The `[orchestration]` blueprint selection was invalid (not exactly one
    /// of `name`/`include`/`inline`).
    #[error("invalid blueprint selection: {0}")]
    InvalidSelection(String),
}

impl BlueprintError {
    /// Construct a structured rulebook violation for the Studio's per-field
    /// surfacing (ADR-59 Decision 5): a dotted field path (e.g.
    /// `"stage.max_cycles"`), the rule's machine code (`"rule_c"`…
    /// `"rule_j"`), and the user-facing message — without the `rule (x): `
    /// prefix, which the `Display` impl adds back.
    pub fn rule(field: &str, code: &'static str, message: &str) -> Self {
        Self::Rule { field: field.to_string(), code, message: message.to_string() }
    }
}

/// Validate a blueprint against the load-time rulebook (ADR-58 D2/D5,
/// blueprint §5.3).
///
/// The relaxed rulebook keeps only the integrity and safety gates:
/// - (c) an unstaffed non-`Acceptance` gate (known `Review`/`Acceptance`
///   kind, via [`StageDef::is_gate`]) without a fallback persona is rejected;
///   `Acceptance` is exempt (unstaffed, it falls back to a persona by engine
///   design);
/// - (d) a stage's fallback persona id must differ from any staffed agent
///   (no self-fallback; fallback capability flags are plain flags — the
///   narrowing/widening check is removed);
/// - (e) `max_cycles = 0` is rejected;
/// - (f) the sum of stage caps (using [`StageDef::default_max_cycles`]) must
///   not exceed the ADR-52 engine global maximum, when set;
/// - (g) stage tags are unique and non-empty;
/// - (j) stage tags must not collide with the reserved engine names.
///
/// Removed rules: (a) exactly-one primary `Execution` (primary is a plain
/// declarative flag), (b) terminal reachability (a pipeline may end on any
/// stage), (d) fallback capability narrowing/widening (flags are plain
/// flags), and (i) `OnGateCycle` requires a gate kind (any stage may declare
/// it; the engine resolves semantics). (h) remains structural: `feed` is a
/// closed `FeedLabel` enum, so an unknown label is a hard parse error.
///
/// `global_max_dispatch_cycles` is the ADR-52 run cap (`max_total_iterations`);
/// rule (f) is vacuous when the cap is unset (`None`).
pub fn validate_blueprint(
    blueprint: &Blueprint,
    global_max_dispatch_cycles: Option<usize>,
) -> Result<(), BlueprintError> {
    if blueprint.schema_version != ORCHESTRATION_SCHEMA_VERSION {
        return Err(BlueprintError::Validation(format!(
            "unsupported orchestration schema_version {} (supported: {ORCHESTRATION_SCHEMA_VERSION})",
            blueprint.schema_version
        )));
    }
    if blueprint.name.trim().is_empty() {
        return Err(BlueprintError::Validation("blueprint name must be non-empty".into()));
    }

    let stages = &blueprint.pipeline.stages;
    if stages.is_empty() {
        return Err(BlueprintError::Validation("pipeline must contain at least one stage".into()));
    }

    // (g) registry integrity: unique, non-empty, unreserved tags. The registry
    // IS the stage list; any stage the pipeline references lives here.
    let mut seen_tags = std::collections::HashSet::new();
    for stage in stages {
        if stage.tag.trim().is_empty() {
            return Err(BlueprintError::rule("stage.tag", "rule_g", "stage tag must be non-empty"));
        }
        if !seen_tags.insert(stage.tag.as_str()) {
            return Err(BlueprintError::rule(
                "stage.tag",
                "rule_g",
                &format!("duplicate stage tag '{}' in the registry", stage.tag),
            ));
        }
        // (j) reserved-name collisions (e.g. a custom stage tagged
        // `coordinator` colliding with the sentinel provider identity).
        if RESERVED_BLUEPRINT_NAMES.contains(&stage.tag.as_str()) {
            return Err(BlueprintError::rule(
                "stage.tag",
                "rule_j",
                &format!("stage tag '{}' collides with a reserved engine name", stage.tag),
            ));
        }
    }

    for stage in stages {
        // (e) max_cycles = 0 → reject.
        if stage.max_cycles == Some(0) {
            return Err(BlueprintError::rule(
                "stage.max_cycles",
                "rule_e",
                &format!("stage '{}' max_cycles must be at least 1", stage.tag),
            ));
        }

        // (c) an unstaffed non-`Acceptance` gate without a fallback persona
        // → reject. `Acceptance` is exempt: unstaffed, it falls back to a
        // persona by engine design. Unknown kinds are never gates.
        //
        // N6: staffing here is checked against the literal `agents` list only.
        // Enforcement against the EFFECTIVE registry (agents `disabled: true`,
        // builtin seeds replaced at load, etc.) is a P2+P3 concern — in P1 the
        // named blueprints and the rulebook are byte-identical to the
        // pre-blueprint pipeline by design.
        if stage.is_gate()
            && stage.known_kind() != Some(StageKind::Acceptance)
            && stage.agents.is_empty()
            && stage.fallback.is_none()
        {
            return Err(BlueprintError::rule(
                "stage.fallback",
                "rule_c",
                &format!("stage '{}' is an unstaffed gate without a fallback persona", stage.tag),
            ));
        }

        // (d) a stage's fallback id must differ from any agent staffed in the
        // same stage (no self-fallback). Fallback capability flags are plain
        // flags — the removed widening/narrowing check no longer applies.
        if let Some(fallback) = &stage.fallback {
            if stage.agents.iter().any(|a| a == &fallback.id) {
                return Err(BlueprintError::rule(
                    "stage.fallback",
                    "rule_d",
                    &format!(
                        "stage '{}' fallback persona '{}' collides with a staffed agent",
                        stage.tag, fallback.id
                    ),
                ));
            }
        }
    }

    // (f) the sum of stage caps is bounded under the engine global maximum
    // (ADR-52 run cap). Vacuous when the cap is unset. Rule (f) sums ALL
    // stage caps including gates — deliberately a stricter static bound than
    // ADR-52's dispatch counter (ADR-58 "Relationship to prior ADRs").
    if let Some(global_max) = global_max_dispatch_cycles {
        let sum: u64 = stages
            .iter()
            .map(|s| u64::from(s.max_cycles.unwrap_or_else(|| s.default_max_cycles())))
            .sum();
        if sum > global_max as u64 {
            return Err(BlueprintError::rule(
                "orchestration.max_total_iterations",
                "rule_f",
                &format!(
                    "sum of stage cycle caps ({sum}) exceeds the engine global \
                     maximum ({global_max})"
                ),
            ));
        }
    }

    // (h) `feed` must be a member of the closed feed catalog. Enforced
    // structurally: `feed` is a closed `FeedLabel` enum and every `StageDef`
    // block deserializes with `deny_unknown_fields`, so an unknown label
    // string is a hard parse error.

    Ok(())
}

/// Validate the blueprint against the rulebook and resolve it.
pub fn validate_and_resolve(
    blueprint: &Blueprint,
    global_max_dispatch_cycles: Option<usize>,
) -> Result<ResolvedBlueprint, BlueprintError> {
    validate_blueprint(blueprint, global_max_dispatch_cycles)?;
    resolve_blueprint_validated(blueprint)
}

/// Resolve a blueprint (validated with a vacuous rule (f)).
pub fn resolve_blueprint(blueprint: &Blueprint) -> Result<ResolvedBlueprint, BlueprintError> {
    validate_and_resolve(blueprint, None)
}

/// Pure resolution — callers must have validated the blueprint first.
fn resolve_blueprint_validated(blueprint: &Blueprint) -> Result<ResolvedBlueprint, BlueprintError> {
    let stages = blueprint
        .pipeline
        .stages
        .iter()
        .map(|def| ResolvedStage {
            def: def.clone(),
            effective_capabilities: def.effective_capabilities(),
            effective_feed: def.effective_feed(),
        })
        .collect::<Vec<_>>();
    let feed_map =
        stages.iter().map(|s| (s.def.tag.clone(), s.effective_feed)).collect::<HashMap<_, _>>();
    // N5: an empty relationship registry falls back to the engine's default
    // five rows (the pre-blueprint `default_collaboration_rules`), so an
    // include/inline blueprint that omits `relationships` still pins the
    // standard topology instead of silently resolving to an empty one.
    let relationship_defaults = if blueprint.relationships.is_empty() {
        standard_relationships()
    } else {
        blueprint.relationships.clone()
    };
    Ok(ResolvedBlueprint { blueprint: blueprint.clone(), stages, feed_map, relationship_defaults })
}

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
fn stage(tag: &str, label: &str, kind: impl Into<String>) -> StageDef {
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
/// (ADR-58 §4), mirroring `default_collaboration_rules()`
/// (`relationship.rs`): reviewer→coder & validator→coder `Supervises`
/// (Delegation), researcher→coder `ProvidesContextTo` (ContextFlow),
/// architect→coder & architect→researcher `OwnsDesign` (Delegation).
fn standard_relationships() -> Vec<RelationshipDef> {
    vec![
        RelationshipDef {
            kind: "supervises".into(),
            semantics: RelationshipSemantics::Delegation,
            from: "reviewer".into(),
            to: "coder".into(),
        },
        RelationshipDef {
            kind: "provides_context_to".into(),
            semantics: RelationshipSemantics::ContextFlow,
            from: "researcher".into(),
            to: "coder".into(),
        },
        RelationshipDef {
            kind: "owns_design".into(),
            semantics: RelationshipSemantics::Delegation,
            from: "architect".into(),
            to: "coder".into(),
        },
        RelationshipDef {
            kind: "owns_design".into(),
            semantics: RelationshipSemantics::Delegation,
            from: "architect".into(),
            to: "researcher".into(),
        },
        RelationshipDef {
            kind: "supervises".into(),
            semantics: RelationshipSemantics::Delegation,
            from: "validator".into(),
            to: "coder".into(),
        },
    ]
}

impl BlueprintSelection {
    /// Load the selected blueprint: named catalog lookup or include-file
    /// parse (inline defs are returned as-is).
    pub fn load(&self, config_dirs: &[PathBuf]) -> Result<Blueprint, BlueprintError> {
        let selected = [self.name.is_some(), self.include.is_some(), self.inline.is_some()]
            .iter()
            .filter(|set| **set)
            .count();
        if selected != 1 {
            return Err(BlueprintError::InvalidSelection(format!(
                "expected exactly one of name/include/inline, found {selected}"
            )));
        }

        if let Some(name) = &self.name {
            return named_blueprint(name).ok_or_else(|| {
                BlueprintError::UnknownNamedBlueprint(name.clone(), NAMED_BLUEPRINTS.join(", "))
            });
        }

        if let Some(include) = &self.include {
            return load_blueprint_file(config_dirs, include);
        }

        match &self.inline {
            Some(blueprint) => Ok(blueprint.clone()),
            None => Err(BlueprintError::InvalidSelection("blueprint selection is empty".into())),
        }
    }
}

/// Read and parse a blueprint include file.
///
/// The path is resolved relative to each candidate config directory in order
/// (the project root first — project overrides global, consistent with the
/// crate's layered merge — then the global config file's directory), finally
/// falling back to the current working directory. P1 keeps a single
/// include-merge point so Studio apply and the ADR-57 watcher can share it
/// (ADR-58 "Relationship to prior ADRs").
fn load_blueprint_file(
    config_dirs: &[PathBuf],
    include: &str,
) -> Result<Blueprint, BlueprintError> {
    // N1: first-wins over the candidate dirs in the order provided. Callers
    // pass the project config directory first and the global config directory
    // second (project-overrides-global, matching `load_config_layers`), then
    // the bare include name is resolved against the current working directory
    // as a last resort.
    let mut candidates: Vec<PathBuf> = config_dirs.iter().map(|dir| dir.join(include)).collect();
    candidates.push(PathBuf::from(include));

    let mut last_error: Option<BlueprintError> = None;
    for path in &candidates {
        let raw = match std::fs::read_to_string(path) {
            Ok(raw) => raw,
            Err(error) => {
                // N2: error paths are absolute so messages stay unambiguous
                // regardless of the caller's working directory.
                let absolute = absolute_candidate(path);
                last_error = Some(match error.kind() {
                    std::io::ErrorKind::NotFound => {
                        BlueprintError::MissingIncludeFile(absolute.display().to_string())
                    }
                    _ => BlueprintError::ParseIncludeFile {
                        path: absolute.display().to_string(),
                        detail: error.to_string(),
                    },
                });
                continue;
            }
        };
        let blueprint = toml::from_str(&raw).map_err(|error| BlueprintError::ParseIncludeFile {
            path: absolute_candidate(path).display().to_string(),
            detail: error.to_string(),
        })?;
        return Ok(blueprint);
    }

    let last = last_error.unwrap_or_else(|| {
        BlueprintError::MissingIncludeFile(
            absolute_candidate(&PathBuf::from(include)).display().to_string(),
        )
    });
    Err(last)
}

/// Absolute rendering of a candidate include path for error messages. Unlike
/// `std::fs::canonicalize`, this works even when the file does not exist
/// (the missing-include case).
fn absolute_candidate(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().map(|cwd| cwd.join(path)).unwrap_or_else(|_| path.to_path_buf())
    }
}

/// Parse an existing blueprint include file at an exact `path` (no candidate
/// search) as a [`Blueprint`].
///
/// This is the save-time parse check behind the ADR-59 Decision 3
/// unparseable-include block. `save_blueprint` serializes from the in-memory
/// model, so a round-trip through it would **silently delete unknown keys**
/// the on-disk file carries (`deny_unknown_fields` on every blueprint block);
/// the file must therefore parse as a valid [`Blueprint`] before any write.
/// Callers pass the resolved write target — computed via
/// [`include_write_target`] after the target-shadow guard — never a
/// speculative candidate list. Unlike [`load_blueprint_file`] (which
/// distinguishes a missing candidate), every failure — read error or TOML
/// parse error — maps to [`BlueprintError::ParseIncludeFile`] with an
/// absolute path, because a save target that cannot be read or parsed must
/// always block the write.
pub fn parse_blueprint_file(path: &Path) -> Result<Blueprint, BlueprintError> {
    let raw = std::fs::read_to_string(path).map_err(|error| BlueprintError::ParseIncludeFile {
        path: absolute_candidate(path).display().to_string(),
        detail: error.to_string(),
    })?;
    toml::from_str::<Blueprint>(&raw).map_err(|error| BlueprintError::ParseIncludeFile {
        path: absolute_candidate(path).display().to_string(),
        detail: error.to_string(),
    })
}

/// Compute the first-wins write target for a blueprint include file,
/// mirroring [`load_blueprint_file`]'s resolution order: each candidate config
/// directory in order (project first, global second), then the bare include
/// name resolved against the current working directory as the last resort.
///
/// This is the ADR-59 Decision 3 target-shadowing guard's input: if the file
/// actually loaded from a different location than `include_write_target`
/// returns, the Studio must warn or refuse rather than silently save to a path
/// a later load would not read (a save that would be shadowed).
pub fn include_write_target(config_dirs: &[PathBuf], include: &str) -> PathBuf {
    config_dirs
        .iter()
        .map(|dir| dir.join(include))
        .find(|path| path.exists())
        .unwrap_or_else(|| PathBuf::from(include))
}

/// How a pipeline is selected (ADR-58 D4): exactly one of `name`, `include`,
/// or `inline` must be set; an empty (or multi-set) selection is a load error.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct BlueprintSelection {
    /// Named blueprint variant from the shipped catalog (see
    /// [`named_blueprint`]).
    #[serde(default)]
    pub name: Option<String>,
    /// Path of the blueprint include file (`orchestration.blueprint.toml`),
    /// resolved relative to the config directory.
    #[serde(default)]
    pub include: Option<String>,
    /// Inline blueprint definition.
    #[serde(default)]
    pub inline: Option<Blueprint>,
}

impl Default for BlueprintSelection {
    /// Absent `name`/`include`/`inline` selects the shipped `standard`
    /// pipeline — the pre-blueprint default reproduced as data.
    fn default() -> Self {
        Self { name: Some("standard".to_string()), include: None, inline: None }
    }
}

/// The `[orchestration]` section (ADR-58).
///
/// `None` on `AppConfig` (no `[orchestration]` table) keeps the engine's
/// embedded five-stage pipeline — nothing here runs and every pre-existing
/// config loads unchanged (legacy equivalence). When present, the section
/// opts into the blueprint data model: the selected blueprint is validated at
/// load by the relaxed rulebook and resolved for consumers.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct OrchestrationConfig {
    /// Blueprint schema version this section was written against.
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    /// Blueprint selection (exactly one of `name`/`include`/`inline`).
    #[serde(default)]
    pub blueprint: BlueprintSelection,
}

impl Default for OrchestrationConfig {
    fn default() -> Self {
        Self {
            schema_version: ORCHESTRATION_SCHEMA_VERSION,
            blueprint: BlueprintSelection::default(),
        }
    }
}

impl OrchestrationConfig {
    /// Load the selected blueprint (named catalog or include file), validate
    /// it against the relaxed rulebook, and resolve it.
    ///
    /// `config_dirs` are the candidate base directories for include-file
    /// resolution (global config dir, then project root) in precedence order.
    pub fn resolve(
        &self,
        config_dirs: &[PathBuf],
        global_max_dispatch_cycles: Option<usize>,
    ) -> Result<ResolvedBlueprint, BlueprintError> {
        if self.schema_version != ORCHESTRATION_SCHEMA_VERSION {
            return Err(BlueprintError::Validation(format!(
                "unsupported orchestration schema_version {} (supported: {ORCHESTRATION_SCHEMA_VERSION})",
                self.schema_version
            )));
        }
        let blueprint = self.blueprint.load(config_dirs)?;
        validate_and_resolve(&blueprint, global_max_dispatch_cycles)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shipped catalog: every named blueprint validates and resolves, and
    /// `default_blueprint()` is `standard`.
    #[test]
    fn named_blueprints_all_validate_and_resolve() {
        for name in NAMED_BLUEPRINTS {
            let blueprint = named_blueprint(name).unwrap_or_else(|| panic!("{name} must exist"));
            validate_blueprint(&blueprint, None)
                .unwrap_or_else(|e| panic!("{name} must validate: {e}"));
            let resolved = resolve_blueprint(&blueprint)
                .unwrap_or_else(|e| panic!("{name} must resolve: {e}"));
            assert_eq!(resolved.stages.len(), blueprint.pipeline.stages.len());
            assert_eq!(resolved.feed_map.len(), blueprint.pipeline.stages.len());
        }
        assert_eq!(default_blueprint().name, "standard");
    }

    /// Legacy parity (ADR-58 Testing): the standard blueprint's resolved
    /// masks and feeds equal today's hardcoded tables — `implement` →
    /// `Execute` with writes, `design`/`research`/`review`/`validate` with
    /// their today feed bindings and no write mask.
    #[test]
    fn standard_blueprint_parity_with_engine_tables() {
        let resolved = resolve_blueprint(&standard_blueprint()).expect("standard must resolve");

        let by_tag: HashMap<&str, &ResolvedStage> =
            resolved.stages.iter().map(|s| (s.def.tag.as_str(), s)).collect();

        // Stage-kind masks: only the primary Execution stage carries writes.
        for (tag, fs_write, shell) in [
            ("design", false, false),
            ("research", false, false),
            ("implement", true, true),
            ("review", false, false),
            ("validate", false, false),
        ] {
            let stage = by_tag[tag];
            assert_eq!(stage.effective_capabilities.fs_write, fs_write, "{tag} fs_write");
            assert_eq!(stage.effective_capabilities.shell, shell, "{tag} shell");
        }

        // Feed bindings (blueprint §5.6, mirroring runtime_runner.rs):
        // research→Understand, design→Plan, implement→Execute,
        // review/validate→Verify.
        let expected: HashMap<&str, Option<RunStage>> = [
            ("research", Some(RunStage::Understand)),
            ("design", Some(RunStage::Plan)),
            ("implement", Some(RunStage::Execute)),
            ("review", Some(RunStage::Verify)),
            ("validate", Some(RunStage::Verify)),
        ]
        .into_iter()
        .collect();
        for (tag, feed) in expected {
            assert_eq!(resolved.feed_map.get(tag).copied().flatten(), feed, "{tag} feed");
        }

        // Relationship defaults mirror default_collaboration_rules() data
        // rows (ADR-58 §4).
        assert_eq!(resolved.relationship_defaults.len(), 5);
        assert_eq!(resolved.relationship_defaults[0].from, "reviewer");
        assert_eq!(resolved.relationship_defaults[0].to, "coder");
    }

    // ---- rulebook (relaxed): (c)/(d)/(e)/(f)/(g)/(j) kept; (a)/(b)/(i) and
    // the fallback widening check removed ----

    /// Modify a clone of the standard blueprint for failure-injection tests.
    fn mutate<F: FnOnce(&mut Blueprint)>(f: F) -> Blueprint {
        let mut blueprint = standard_blueprint();
        f(&mut blueprint);
        blueprint
    }

    /// Removed rule (a): `primary` is a plain declarative flag — a blueprint
    /// with zero primary `Execution` stages validates, and so does one with
    /// two, including a non-`Execution` stage marked primary.
    #[test]
    fn rule_a_removed_primaries_are_declarative() {
        let no_primary = mutate(|b| {
            for s in &mut b.pipeline.stages {
                s.primary = false;
            }
        });
        validate_blueprint(&no_primary, None).expect("zero primary stages must validate");

        // A second Execution-kind stage marked primary.
        let two_primary = mutate(|b| {
            b.pipeline.stages[4].kind = StageKind::Execution.as_str().to_string();
            b.pipeline.stages[4].primary = true;
        });
        validate_blueprint(&two_primary, None).expect("two primary stages must validate");

        // A non-Execution stage (design, Planning) marked primary.
        let non_execution_primary = mutate(|b| {
            b.pipeline.stages[0].primary = true;
        });
        validate_blueprint(&non_execution_primary, None)
            .expect("a non-Execution primary must validate");
    }

    /// Removed rule (b): a pipeline with no terminal kind validates (it may
    /// end on any stage).
    #[test]
    fn rule_b_removed_no_terminal_kind_validates() {
        let bad = mutate(|b| {
            b.pipeline.stages.retain(|s| s.known_kind().is_none_or(|k| !k.is_terminal()));
        });
        // Retaining only design/research/implement/review — no terminal kind.
        validate_blueprint(&bad, None).expect("a no-terminal pipeline must validate");
    }

    #[test]
    fn rule_c_rejects_unstaffed_gate_without_fallback() {
        let bad = mutate(|b| {
            let review = b.pipeline.stages.iter_mut().find(|s| s.tag == "review").unwrap();
            review.agents.clear();
            review.fallback = None;
        });
        let err = validate_blueprint(&bad, None).unwrap_err();
        assert!(format!("{err}").contains("rule (c)"), "{err}");
    }

    #[test]
    fn rule_c_accepts_unstaffed_acceptance_gate() {
        let ok = mutate(|b| {
            let validate = b.pipeline.stages.iter_mut().find(|s| s.tag == "validate").unwrap();
            validate.agents.clear();
            validate.fallback = None; // Acceptance is exempt by design
        });
        validate_blueprint(&ok, None).expect("unstaffed Acceptance without fallback is allowed");
    }

    #[test]
    fn rule_d_rejects_self_fallback() {
        let bad = mutate(|b| {
            let review = b.pipeline.stages.iter_mut().find(|s| s.tag == "review").unwrap();
            let mut fallback = coordinator_fallback();
            fallback.id = "reviewer".into(); // reviewer is staffed in this stage
            review.fallback = Some(fallback);
        });
        let err = validate_blueprint(&bad, None).unwrap_err();
        assert!(format!("{err}").contains("rule (d)"), "{err}");
    }

    #[test]
    fn rule_e_rejects_zero_max_cycles() {
        let bad = mutate(|b| {
            b.pipeline.stages[0].max_cycles = Some(0);
        });
        let err = validate_blueprint(&bad, None).unwrap_err();
        assert!(format!("{err}").contains("rule (e)"), "{err}");
    }

    #[test]
    fn rule_f_vacuous_when_cap_unset_and_bounded_when_set() {
        // Standard blueprint caps: design 1 + research 1 + implement 1 +
        // review 3 + validate 2 = 8.
        let blueprint = standard_blueprint();
        validate_blueprint(&blueprint, None).expect("vacuous when the cap is unset");
        validate_blueprint(&blueprint, Some(8)).expect("exactly at the bound is accepted");
        let err = validate_blueprint(&blueprint, Some(7)).unwrap_err();
        assert!(format!("{err}").contains("rule (f)"), "{err}");
    }

    #[test]
    fn rule_g_rejects_duplicate_tags_and_empty_tags() {
        let dup = mutate(|b| {
            b.pipeline.stages[4].tag = "implement".into();
        });
        let err = validate_blueprint(&dup, None).unwrap_err();
        assert!(format!("{err}").contains("duplicate stage tag"), "{err}");

        let empty = mutate(|b| {
            b.pipeline.stages[0].tag = "".into();
        });
        let err = validate_blueprint(&empty, None).unwrap_err();
        assert!(format!("{err}").contains("non-empty"), "{err}");
    }

    #[test]
    fn rule_h_rejects_unknown_feed_label_at_parse() {
        // The closed FeedLabel enum + deny_unknown_fields: an unknown label
        // string is a hard parse error before validation runs.
        let toml_str = r#"
            schema_version = 1
            name = "bad-feeds"
            [pipeline]
            [[pipeline.stages]]
            tag = "s"
            label = "S"
            kind = "execution"
            feed = "telemetry"
        "#;
        let err = toml::from_str::<Blueprint>(toml_str).unwrap_err();
        assert!(format!("{err}").contains("feed"), "{err}");
    }

    /// Removed rule (i): `OnGateCycle` is allowed on any stage — a non-gate
    /// kind carrying the condition validates (the engine resolves semantics).
    #[test]
    fn rule_i_removed_on_gate_cycle_on_non_gate_validates() {
        let ok = mutate(|b| {
            b.pipeline.stages[2].condition = StageCondition::OnGateCycle; // implement is not a gate
        });
        validate_blueprint(&ok, None).expect("OnGateCycle on a non-gate stage must validate");
    }

    #[test]
    fn rule_j_rejects_reserved_stage_tag() {
        for reserved in RESERVED_BLUEPRINT_NAMES {
            let bad = mutate(|b| {
                b.pipeline.stages[0].tag = (*reserved).into();
            });
            let err = validate_blueprint(&bad, None).unwrap_err();
            assert!(format!("{err}").contains("rule (j)"), "{err}");
        }
    }

    /// Removed fallback widening/narrowing check (rule (d)): fallback
    /// capability flags are plain flags — `fs_write = true` on a
    /// non-`Execution` stage (Acceptance, mask `{false,false}`) validates.
    #[test]
    fn fallback_plain_flags_can_widen() {
        let ok = mutate(|b| {
            let validate = b.pipeline.stages.iter_mut().find(|s| s.tag == "validate").unwrap();
            let mut fallback = coordinator_fallback();
            fallback.capabilities.fs_write = Some(true); // Acceptance mask has no writes
            validate.fallback = Some(fallback);
        });
        validate_blueprint(&ok, None).expect("a widening fallback flag must validate");
    }

    /// Open kinds: an unknown kind string (e.g. `"blogger"`) is a valid stage
    /// with no engine defaults — no write mask, not a gate, one default
    /// cycle — until explicit flags grant writes.
    #[test]
    fn unknown_kind_string_is_valid_with_no_engine_defaults() {
        let blueprint = mutate(|b| {
            b.pipeline.stages.push(StageDef {
                kind: "blogger".to_string(),
                feed: None,
                primary: false,
                ..stage("blog", "Blogger", "blogger")
            });
        });
        validate_blueprint(&blueprint, None)
            .expect("a pipeline with an unknown user kind must validate");
        let resolved = resolve_blueprint(&blueprint).expect("must resolve");
        let blog =
            resolved.stages.iter().find(|s| s.def.tag == "blog").expect("blog stage present");

        assert_eq!(blog.def.known_kind(), None, "unknown kind parses to no known kind");
        assert!(!blog.def.is_gate(), "unknown kinds are never gates");
        assert_eq!(blog.def.default_max_cycles(), 1, "unknown kinds default to one cycle");
        assert_eq!(
            blog.effective_capabilities,
            CapabilityMask::default(),
            "unknown kinds grant no writes by default"
        );

        // An explicit flag grants a write on the unknown kind.
        let flagged = mutate(|b| {
            b.pipeline.stages.push(StageDef {
                kind: "blogger".to_string(),
                flags: StageFlags { fs_write: Some(true), shell: None },
                feed: None,
                primary: false,
                ..stage("blog", "Blogger", "blogger")
            });
        });
        let resolved = resolve_blueprint(&flagged).expect("must resolve");
        let blog =
            resolved.stages.iter().find(|s| s.def.tag == "blog").expect("blog stage present");
        assert_eq!(
            blog.effective_capabilities,
            CapabilityMask { fs_write: true, shell: false },
            "explicit flags grant writes on unknown kinds"
        );
    }

    #[test]
    fn unknown_named_blueprint_is_rejected() {
        assert!(named_blueprint("not-a-real-blueprint").is_none());
    }

    #[test]
    fn blueprint_denies_unknown_fields() {
        // The deliberate asymmetry with AppConfig (ADR-58 §6): unknown keys
        // inside blueprint blocks are hard errors.
        let toml_str = r#"
            schema_version = 1
            name = "custom"
            unexpected = true
            [pipeline]
            [[pipeline.stages]]
            tag = "s"
            label = "S"
            kind = "execution"
        "#;
        assert!(toml::from_str::<Blueprint>(toml_str).is_err());
    }

    #[test]
    fn selection_requires_exactly_one_of_name_include_inline() {
        let empty = BlueprintSelection { name: None, include: None, inline: None };
        let err = empty.load(&[]).unwrap_err();
        assert!(format!("{err}").contains("exactly one"), "{err}");

        let both = BlueprintSelection {
            name: Some("standard".into()),
            include: Some(BLUEPRINT_INCLUDE_FILE.into()),
            inline: None,
        };
        let err = both.load(&[]).unwrap_err();
        assert!(format!("{err}").contains("exactly one"), "{err}");
    }

    #[test]
    fn named_selection_loads_standard_by_default() {
        let selection = BlueprintSelection::default();
        let blueprint = selection.load(&[]).expect("default selects standard");
        assert_eq!(blueprint.name, "standard");
    }

    #[test]
    fn inline_selection_loads_and_validates() {
        let inline = standard_blueprint().clone();
        let selection =
            BlueprintSelection { name: None, include: None, inline: Some(inline.clone()) };
        let loaded = selection.load(&[]).expect("inline must load");
        assert_eq!(loaded, inline);
        let section = OrchestrationConfig {
            schema_version: ORCHESTRATION_SCHEMA_VERSION,
            blueprint: selection,
        };
        let _resolved = section.resolve(&[], None).expect("inline must resolve");
    }

    #[test]
    fn include_file_loads_from_config_dir() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(BLUEPRINT_INCLUDE_FILE);
        let blueprint = standard_blueprint();
        let rendered = toml::to_string_pretty(&blueprint).expect("blueprint serializes");
        std::fs::write(&path, rendered).expect("write include file");

        let selection = BlueprintSelection {
            name: None,
            include: Some(BLUEPRINT_INCLUDE_FILE.into()),
            inline: None,
        };
        let loaded = selection.load(&[dir.path().to_path_buf()]).expect("include file must load");
        assert_eq!(loaded.name, "standard");
    }

    #[test]
    fn include_file_missing_is_rejected() {
        let selection = BlueprintSelection {
            name: None,
            include: Some("does-not-exist.toml".into()),
            inline: None,
        };
        let err = selection.load(&[]).unwrap_err();
        assert!(matches!(err, BlueprintError::MissingIncludeFile(_)), "{err}");
    }

    #[test]
    fn run_once_kind_is_terminal_without_write_mask() {
        let blueprint = mutate(|b| {
            b.pipeline.stages.retain(|s| s.tag != "validate" && s.tag != "review");
            b.pipeline.stages.push(StageDef {
                kind: StageKind::RunOnce.as_str().to_string(),
                feed: None, // RunOnce has no feed binding
                primary: false,
                ..stage("scratch", "Scratch run-once", StageKind::RunOnce.as_str())
            });
        });
        validate_blueprint(&blueprint, None).expect("RunOnce stage must validate");
        let resolved = resolve_blueprint(&blueprint).expect("must resolve");
        let scratch =
            resolved.stages.iter().find(|s| s.def.tag == "scratch").expect("scratch stage present");
        assert_eq!(scratch.effective_capabilities, CapabilityMask::default());
        assert_eq!(scratch.effective_feed, None);
    }

    /// B1: editor typos in stage/capability flag keys must be caught at the
    /// TOML boundary (`deny_unknown_fields`), not silently defaulted.
    #[test]
    fn b1_deny_unknown_fields_on_stage_flags_and_capability_mask() {
        let flag_typo = toml::from_str::<StageFlags>("fs_writ = true");
        assert!(flag_typo.is_err(), "typoed fs_write must be a parse error");

        let capability_typo = toml::from_str::<CapabilityMask>("fs_writ = true");
        assert!(capability_typo.is_err(), "typoed mask key must be a parse error");
    }

    /// B2: a fallback persona with absent capability flags inherits the
    /// stage-kind's write mask. The `Execution`-kind fallback therefore gains
    /// `{fs_write, shell}` even though its own flags are all `None`.
    #[test]
    fn b2_fallback_absent_flags_inherit_stage_kind_mask() {
        let execution_fallback = coordinator_self_implement_fallback();
        assert_eq!(
            execution_fallback.effective_capabilities(StageKind::Execution),
            CapabilityMask { fs_write: true, shell: true },
            "unstaffed Execution fallback inherits the Execution write mask"
        );

        // An explicit narrow on a masked kind is honored (narrowing-only).
        let narrowed = FallbackPersonaDef {
            capabilities: StageFlags { fs_write: Some(false), shell: None },
            ..execution_fallback
        };
        assert_eq!(
            narrowed.effective_capabilities(StageKind::Execution),
            CapabilityMask { fs_write: false, shell: true }
        );

        // Gate fallbacks inherit the write-less gate mask.
        let gate_fallback = coordinator_fallback();
        assert_eq!(
            gate_fallback.effective_capabilities(StageKind::Review),
            CapabilityMask::default(),
            "gate fallbacks never gain writes"
        );
    }

    /// B5: the shipped gate fallbacks are a faithful mirror of the runtime's
    /// `self_verify_agent` eval-mode persona — empty instruction sections —
    /// and the Execution-kind fallback carries no instructions either (the
    /// sentinel mechanism renders `COORDINATOR_SELF_IMPLEMENT_PROMPT`).
    #[test]
    fn b5_shipped_fallbacks_have_empty_instruction_sections() {
        for blueprint in [standard_blueprint(), research_only_blueprint()] {
            for stage in &blueprint.pipeline.stages {
                let Some(fallback) = &stage.fallback else {
                    // Only stages that may render a fallback carry one
                    // (gates; staffed Execution stages have none).
                    continue;
                };
                assert_eq!(
                    fallback.system_instructions, None,
                    "{}:{} fallback must render no instructions (empty PromptSections mirror)",
                    blueprint.name, stage.tag
                );
            }
        }
        assert_eq!(
            coordinator_self_implement_fallback().system_instructions,
            None,
            "execution fallback def is instruction-free; the sentinel renders the prompt"
        );
    }

    /// N5: an empty `relationships` registry falls back to the default five
    /// collaboration rows (ADR-58 §4), keeping legacy configs and the
    /// standard named blueprint byte-identical to the runtime tables.
    #[test]
    fn n5_empty_relationships_fall_back_to_default_rows() {
        let resolved = resolve_blueprint(&mutate(|b| b.relationships.clear()))
            .expect("empty relationships must resolve to defaults");
        assert_eq!(resolved.relationship_defaults.len(), 5);
        assert_eq!(resolved.relationship_defaults[0].from, "reviewer");
        assert_eq!(resolved.relationship_defaults[0].to, "coder");
        assert_eq!(resolved.relationship_defaults, standard_relationships());
    }

    /// ADR-59 Decision 5: rulebook failures surface as the structured `Rule`
    /// variant (field path + machine code + message), with the `Display`
    /// output keeping the exact `rule (x): ...` prefix of the legacy
    /// messages.
    #[test]
    fn rule_error_codes_map_to_rulebook_letters() {
        // (e): max_cycles = 0 → stage.max_cycles + rule_e.
        let bad_cycles = mutate(|b| b.pipeline.stages[0].max_cycles = Some(0));
        let err = validate_blueprint(&bad_cycles, None).unwrap_err();
        match err {
            BlueprintError::Rule { field, code, message } => {
                assert_eq!(field, "stage.max_cycles");
                assert_eq!(code, "rule_e");
                assert!(message.contains("max_cycles must be at least 1"), "{message}");
                assert!(
                    format!("{}", BlueprintError::Rule { field, code, message })
                        .contains("rule (e)"),
                    "Display must keep the exact rule (x) prefix"
                );
            }
            other => panic!("expected Rule variant, got {other:?}"),
        }

        // (c): an unstaffed gate without a fallback persona → stage.fallback +
        // rule_c.
        let unstaffed = mutate(|b| {
            let review = b.pipeline.stages.iter_mut().find(|s| s.tag == "review").unwrap();
            review.agents.clear();
            review.fallback = None;
        });
        let err = validate_blueprint(&unstaffed, None).unwrap_err();
        match err {
            BlueprintError::Rule { field, code, message } => {
                assert_eq!(field, "stage.fallback");
                assert_eq!(code, "rule_c");
                assert!(message.contains("unstaffed gate"), "{message}");
            }
            other => panic!("expected Rule variant, got {other:?}"),
        }
    }

    /// ADR-59 Decision 3 target-shadowing guard: `include_write_target`
    /// mirrors `load_blueprint_file`'s first-wins order — project config dir
    /// beats the global config dir, and the bare include name (resolved
    /// against the current working directory) is the last resort.
    #[test]
    fn include_write_target_first_wins() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        let global = dir.path().join("global");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&global).unwrap();

        let include = BLUEPRINT_INCLUDE_FILE;

        // With no file anywhere, the write target is the bare name (cwd) —
        // the last candidate in the load order.
        assert_eq!(
            include_write_target(&[project.clone(), global.clone()], include),
            PathBuf::from(include)
        );
        assert_eq!(include_write_target(&[], include), PathBuf::from(include));

        // A copy in both dirs: the project dir wins (project overrides global).
        std::fs::write(project.join(include), "x").unwrap();
        std::fs::write(global.join(include), "x").unwrap();
        assert_eq!(
            include_write_target(&[project.clone(), global.clone()], include),
            project.join(include)
        );

        // Without the project copy, the global dir becomes the target.
        std::fs::remove_file(project.join(include)).unwrap();
        assert_eq!(
            include_write_target(&[project.clone(), global.clone()], include),
            global.join(include)
        );
    }
}

/// ADR-59 P4 Batch 3, Slice 4b: the save-time parse seam behind the
/// unparseable-include guard. A separate module so the PINNED `mod tests`
/// block above stays untouched; these tests pin the four failure/success
/// shapes the Studio's Save arm depends on (data-loss guard rationale).
#[cfg(test)]
mod parse_blueprint_file_tests {
    use super::*;
    use crate::saving::save_blueprint;

    /// The standard blueprint serializes to a file and parses back Ok —
    /// the happy path the Studio's Save arm writes through.
    #[test]
    fn valid_blueprint_file_parses_ok() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(BLUEPRINT_INCLUDE_FILE);
        let standard = named_blueprint("standard").unwrap();
        save_blueprint(&standard, &path).unwrap();

        let parsed = parse_blueprint_file(&path).unwrap();
        assert_eq!(parsed, standard, "the file round-trips to an equal blueprint");
    }

    /// Syntactically invalid TOML is a `ParseIncludeFile` whose message
    /// carries both the (absolute) path and the toml detail — the caller's
    /// "draft kept, nothing written" surface needs both.
    #[test]
    fn syntactically_invalid_toml_returns_parse_include_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(BLUEPRINT_INCLUDE_FILE);
        std::fs::write(&path, "this is { not toml ==").unwrap();

        let error = parse_blueprint_file(&path).unwrap_err();
        match error {
            BlueprintError::ParseIncludeFile { path: error_path, detail } => {
                assert!(
                    error_path.contains("orchestration.blueprint.toml"),
                    "message path must identify the include file: {error_path}"
                );
                assert!(
                    !detail.is_empty() && detail != error_path,
                    "the toml parse detail must be present: {detail}"
                );
            }
            other => panic!("expected ParseIncludeFile, got {other:?}"),
        }
    }

    /// An unknown key is a hard error (`deny_unknown_fields`) — the rationale
    /// for the guard: a naive round-trip through `save_blueprint` would
    /// silently drop the unknown key from the file, so the file must parse
    /// before any write.
    #[test]
    fn unknown_key_returns_parse_include_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(BLUEPRINT_INCLUDE_FILE);
        let mut standard = named_blueprint("standard").unwrap();
        standard.description = None;
        let mut raw = toml::to_string_pretty(&standard).unwrap();
        raw.push_str("unknown_future_key = true\n");
        std::fs::write(&path, raw).unwrap();

        let error = parse_blueprint_file(&path).unwrap_err();
        assert!(
            matches!(error, BlueprintError::ParseIncludeFile { .. }),
            "deny_unknown_fields must surface as ParseIncludeFile, got {error}"
        );
        let message = error.to_string();
        assert!(
            message.contains("unknown_future_key"),
            "the toml detail names the offending key: {message}"
        );
    }

    /// A missing file is a `ParseIncludeFile` (read error), never a silent
    /// pass — a save target that vanished must block the write, not proceed
    /// from the in-memory model alone.
    #[test]
    fn missing_file_returns_parse_include_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(BLUEPRINT_INCLUDE_FILE);
        assert!(!path.exists(), "precondition: no file seeded");

        let error = parse_blueprint_file(&path).unwrap_err();
        match error {
            BlueprintError::ParseIncludeFile { path: error_path, detail } => {
                assert!(
                    error_path.contains("orchestration.blueprint.toml"),
                    "message path must identify the include file: {error_path}"
                );
                assert!(
                    detail.contains("o such file") || detail.contains("system cannot find"),
                    "read-error detail must be surfaced: {detail}"
                );
            }
            other => panic!("expected ParseIncludeFile, got {other:?}"),
        }
    }
}
