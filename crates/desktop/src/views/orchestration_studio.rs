// crates/desktop/src/views/orchestration_studio.rs
// Orchestration Studio: compact agent library + focused workspace.
// - Searchable agent library (built-in specialists + custom agents)
// - Pipeline workspace: compact agent map plus a real relationship editor
// - Inspector: structured prompt editor, model assignment, permissions
// - Persistence of custom agents / prompts / relationships / presets to config

use iced::widget::{
    button, checkbox, column, container, pick_list, row, scrollable, text, text_input, toggler,
    tooltip, Space,
};
use iced::{Alignment, Color, Element, Length};

use crate::app::Message;
use crate::theme::AppTheme;
use crate::ui::section_card::{section_card, section_card_with_subtitle};
use crate::widgets::agent_graph::{
    self, AgentGraphModel, AgentState, EdgeKind, Message as GraphMessage,
};
use concerto_config::{
    validate_blueprint, AgentCapabilities, AgentModelAssignment, AgentRelationshipConfig,
    AppConfig, Blueprint, BlueprintError, CustomAgentConfig, FallbackPersonaDef, FeedLabel,
    FewShotExample, OrchestrationConfig, PipelinePreset, PromptSections, RelationshipDef,
    RelationshipSemantics, ResolvedBlueprint, StageCondition, StageDef, StageKind,
};
use concerto_core::types::OutputMode;
use concerto_core::{AgentId, AgentStage};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// Id of the Hand-offs list scrollable in the pipeline view. Clicking a graph
/// node snaps this list to the related row(s) in `self.relationships`.
const STUDIO_HANDOFFS_LIST_ID: &str = "studio-handoffs-list";

/// The engine-owned fallback sentinel id (`FallbackPersonaDef::id`,
/// blueprint.rs:193): the sanctioned fallback for an unstaffed `Execution`
/// stage. The sentinel provider mechanism is a non-overridable (ADR-58 §5.9),
/// so the Studio renders this id read-only in the fallback persona card.
const FALLBACK_SENTINEL_ID: &str = "coordinator-self-execute";

/// Display option for agent pick-lists: carries the stable `id` (used as the
/// relationship/assignment key) while showing the human `name`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct AgentOption {
    id: String,
    label: String,
}

impl std::fmt::Display for AgentOption {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.label)
    }
}

/// Display option for the unified model pick-list: carries a compound `key`
/// (`"provider_id|model_name"` — empty for "Use global default") while showing
/// a human `label` (`"model — ProviderName"`).
#[derive(Debug, Clone, PartialEq, Eq)]
struct ModelOption {
    key: String,
    label: String,
}

impl std::fmt::Display for ModelOption {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.label)
    }
}

/// Display option for the typed "Submission contract" (output mode) picker.
/// Wraps the value because `OutputMode` is a foreign type and its `Display`
/// impl cannot be provided from this crate (orphan rule).
#[derive(Debug, Clone, PartialEq, Eq)]
struct ModeOption {
    value: OutputMode,
    label: &'static str,
}

impl std::fmt::Display for ModeOption {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label)
    }
}

/// Display option for the "Lifecycle" (pipeline stage) picker. `value: None`
/// renders as "Freeform (no lifecycle)"; the wrapper keeps `AgentStage` (a
/// foreign type) out of the `Display` impl.
#[derive(Debug, Clone, PartialEq, Eq)]
struct StageOption {
    value: Option<AgentStage>,
    label: &'static str,
}

impl std::fmt::Display for StageOption {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label)
    }
}

/// Display option for the stage-kind suggestions (the six known kinds of the
/// open vocabulary, ADR-58 §2). `StageKind` is a config type without
/// `Display`, so the wrapper renders
/// `StageKind::label` and carries the kind back to the `StageKindChanged`
/// message. Suggestions are one-click writers only — the kind input itself is
/// free text (Slice 3), so unknown user kinds remain valid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct KindOption {
    kind: StageKind,
}

impl std::fmt::Display for KindOption {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.kind.label())
    }
}

/// Display option for the feed picker. `value: None` renders "No feed"; the
/// wrapper keeps `FeedLabel` (a config type without `Display`) out of the
/// `Display` impl (mirrors `StageOption`/`ModeOption`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FeedOption {
    value: Option<FeedLabel>,
}

impl std::fmt::Display for FeedOption {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.value {
            Some(feed) => f.write_str(feed_name(feed)),
            None => f.write_str("No feed"),
        }
    }
}

/// Display option for the condition picker. `StageCondition` is a config
/// type without `Display`; the wrapper renders `condition_name`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ConditionOption {
    value: StageCondition,
}

impl std::fmt::Display for ConditionOption {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(condition_name(self.value))
    }
}

/// Display option for the relationship-kind picker (spec §3): one registered
/// kind name from the blueprint's OPEN relationship registry paired with the
/// CLOSED `RelationshipSemantics` it references (ADR-58 §4). The label
/// renders the semantics glyph so every option carries its semantic
/// affordance (shield / flow arrow / hierarchy chain).
#[derive(Debug, Clone, PartialEq, Eq)]
struct RelationshipKindOption {
    kind: String,
    semantics: RelationshipSemantics,
}

impl std::fmt::Display for RelationshipKindOption {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}", semantics_glyph(self.semantics), self.kind)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capability {
    FsRead,
    FsWrite,
    Shell,
    Git,
    Lsp,
    /// Whether the agent may use the built-in eval/test engine (validator stage).
    Eval,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresetName {
    ReadOnlyResearcher,
    FullCoder,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InspectorSection {
    Prompt,
    Model,
    Permissions,
}

/// Which stage-mask flag a `StageMaskToggled` targets. The blueprint model
/// stores each as `StageFlags.{fs_write,shell}: Option<bool>` (ADR-58 D1); a
/// toggle writes the explicit flag, which overlays the stage-kind default
/// mask in `StageDef::effective_capabilities`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageMaskFlag {
    FsWrite,
    Shell,
}

// ============================================================================
// Data model
// ============================================================================

#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub id: String,
    pub name: String,
    pub role: String,
    pub stage: Option<AgentStage>,
    /// Structured submission contract (Freeform = plain text output; the others
    /// tie the agent into the typed `submit_design_doc` / `submit_research_report`
    /// / `submit_review_report` contracts). Preserved across saves.
    pub output_mode: OutputMode,
    pub prompt_sections: PromptSections,
    pub model_override: Option<String>,
    pub provider_id: Option<String>,
    pub capabilities: AgentCapabilities,
    pub is_custom: bool,
    /// When true the agent is not registered at runtime (topology control).
    pub disabled: bool,
}

#[derive(Debug, Clone)]
pub struct State {
    pub agents: Vec<AgentConfig>,
    pub relationships: Vec<AgentRelationshipConfig>,
    pub model_assignments: Vec<AgentModelAssignment>,
    pub presets: Vec<PipelinePreset>,
    /// Name of the currently active pipeline. Surfaced in the toolbar; the
    /// "New" button switches the studio to a fresh untitled pipeline.
    pub active_pipeline_name: String,
    pub available_providers: Vec<String>,
    /// The global default model from Settings (bare model string). Rendered in
    /// `model_pane` when an agent has no per-agent override. `None` means no
    /// global default has been set.
    pub global_default_model: Option<String>,
    /// Per-provider model names, sourced from settings' model cache. Used by
    /// the unified model picker in `model_pane`. Updated via `sync_models()`
    /// whenever providers change or models are refreshed.
    pub cached_models_by_provider: HashMap<String, Vec<String>>,
    pub selected_agent_id: Option<String>,
    pub selected_relationship: Option<usize>,
    /// Whether the "Add hand-off" form is expanded in the pipeline view
    /// (independent of editing an existing relationship).
    pub show_relationship_editor: bool,
    /// Whether the collapsed "+ Add agent" form is expanded in the library.
    pub show_add_agent_form: bool,
    pub inspector_section: InspectorSection,
    pub search_query: String,
    pub new_agent_name: String,
    pub new_agent_role: String,
    pub new_rel_from: String,
    pub new_rel_to: String,
    pub new_rel_type: String,
    pub new_rel_max_cycles: String,
    pub unsaved: bool,
    pub save_error: Option<String>,
    pub saved_notice: bool,
    /// Maximum specialist runs executing concurrently across all providers.
    pub max_concurrent_agents: usize,
    /// Maximum specialist runs executing concurrently against one provider.
    pub max_concurrent_per_provider: usize,
    /// Spend cap multiplier relative to single-agent mode.
    pub spend_cap_multiplier: f64,
    /// Draft text for max concurrent agents (unparsed input).
    pub run_agents_draft: String,
    /// Draft text for max concurrent per provider (unparsed input).
    pub run_provider_draft: String,
    /// Draft text for the spend cap multiplier (unparsed input).
    pub run_spend_draft: String,
    /// Whether the toolbar issue badge shows its inline issue summary bar
    /// (expanded under the toolbar, next to the separate Validation card).
    pub show_validation_detail: bool,
    /// Memoized `pipeline_graph_model` output (model + edge→relationship map).
    /// Kept in a `RefCell` so the cache can be invalidated from `&mut self`
    /// mutation paths while `pipeline_graph_model(&self)` stays borrow-clean.
    /// This keeps `graph_height` and node positions stable across renders
    /// unless agents/relationships actually change (issue #112).
    pub graph_cache: std::cell::RefCell<Option<(AgentGraphModel, Vec<usize>)>>,

    // ------------------------------------------------------------------
    // ADR-59 blueprint read-path mirror (P4 Batch 3, Slice 1).
    //
    // Dual-path decision: `orchestration` is `Some` exactly when the config
    // opts into the blueprint data model (`[orchestration]` in config.toml).
    // The Studio still renders the legacy tables above until a later slice
    // migrates the view; these fields exist so the blueprint model is
    // available to bind as the unified surface (ADR-59 Decision 1) and so
    // the read side of `resolved_blueprint` / `orchestration` is populated
    // on every `load_from_config`.
    //
    // Why mirror `orchestration` rather than infer from
    // `resolved_blueprint`: the config load seam resolves a blueprint on
    // EVERY load path — legacy configs without `[orchestration]` get the
    // default `standard` blueprint attached (`crates/config/src/lib.rs`
    // ~199-224). `resolved_blueprint.is_some()` is therefore true on both
    // paths and cannot distinguish them; only the presence of the
    // `[orchestration]` section can.
    pub orchestration: Option<OrchestrationConfig>,
    /// The editable blueprint model (the include-file surface the Studio
    /// will bind once the view migrates). Cloned out of the resolved
    /// blueprint so the Studio can mutate it without aliasing the
    /// `Arc<ResolvedBlueprint>` the runtime consumes.
    pub blueprint: Option<Arc<Blueprint>>,
    /// The validated, resolved blueprint attached at config load
    /// (`schema.rs:436`), mirrored only on the blueprint path (see
    /// `load_from_config`). `None` when the config has no `[orchestration]`
    /// section, or when it was never loaded through the load seam (e.g.
    /// `AppConfig::default()` or direct struct literals in tests).
    pub resolved_blueprint: Option<Arc<ResolvedBlueprint>>,
    /// ADR-59 D5 structured validation result for the editable blueprint
    /// (`self.blueprint`), recomputed by `refresh_blueprint_validation` on
    /// load today (later slices call it after every Studio mutation).
    /// `validate_blueprint` fails fast, so a non-empty collection normally
    /// holds a single `Rule` violation; `errors_for(field)` addresses entries
    /// by dotted field path for Slice 3's per-field outlines. Empty on the
    /// legacy path (no `[orchestration]`).
    pub blueprint_errors: Vec<BlueprintError>,
    /// Stage-card "Advanced" sections currently expanded, keyed by stage
    /// index into `blueprint.pipeline.stages` (P4 Batch 3, Slice 3a-1).
    /// View-only presentation state — the collapsible panel is a surface
    /// toggle, not blueprint data — so toggling never marks the studio dirty.
    pub stage_advanced_open: HashSet<usize>,
    /// Per-stage `max_cycles` draft text, keyed by stage index into
    /// `blueprint.pipeline.stages` (P4 Batch 3, Slice 3a-2). The numeric
    /// input keeps its raw string so an unparsable value stays visible
    /// (mirrors the run-limit drafts); the model only receives values that
    /// parse. Seeded from the loaded blueprint so the input always has a
    /// self-owned value to borrow.
    pub stage_max_cycles_drafts: HashMap<usize, String>,
    /// The ADR-52 run cap (`multi_agent.max_total_iterations`) mirrored from
    /// config so `validation()`'s rule (f) is bounded exactly like the config
    /// load seam (`crates/config/src/lib.rs` ~220). `None` = unbounded.
    pub global_max_dispatch_cycles: Option<usize>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            agents: Vec::new(),
            relationships: Vec::new(),
            model_assignments: Vec::new(),
            presets: Vec::new(),
            active_pipeline_name: "Standard Pipeline".into(),
            available_providers: vec!["openai".into(), "anthropic".into(), "ollama".into()],
            global_default_model: None,
            cached_models_by_provider: HashMap::new(),
            selected_agent_id: None,
            selected_relationship: None,
            show_relationship_editor: false,
            show_add_agent_form: false,
            inspector_section: InspectorSection::Prompt,
            search_query: String::new(),
            new_agent_name: String::new(),
            new_agent_role: String::new(),
            new_rel_from: String::new(),
            new_rel_to: String::new(),
            new_rel_type: String::new(),
            new_rel_max_cycles: String::new(),
            unsaved: false,
            save_error: None,
            saved_notice: false,
            max_concurrent_agents: 3,
            max_concurrent_per_provider: 2,
            spend_cap_multiplier: 3.0,
            run_agents_draft: "3".into(),
            run_provider_draft: "2".into(),
            run_spend_draft: "3.0".into(),
            show_validation_detail: false,
            graph_cache: std::cell::RefCell::new(None),
            orchestration: None,
            blueprint: None,
            resolved_blueprint: None,
            blueprint_errors: Vec::new(),
            stage_advanced_open: HashSet::new(),
            stage_max_cycles_drafts: HashMap::new(),
            global_max_dispatch_cycles: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ValidationReport {
    pub ok: bool,
    pub messages: Vec<String>,
}

/// Structured, UI-ready projection of a single [`BlueprintError`] (ADR-59
/// Decision 5): the per-field addressing key, the machine-readable rule code,
/// and the user-facing message. Every enum variant maps here so the Studio's
/// error surfacing never panics and never falls back to a raw `Display` string
/// with a lossy prefix.
#[derive(Debug, Clone, PartialEq, Eq)]
struct BlueprintErrorView {
    /// The dotted field path the error addresses (e.g. `"stage.tag"`); `None`
    /// for message-only variants that carry no field.
    field: Option<String>,
    /// Machine-readable code (`"rule_a"`…`"rule_j"`, or a synthesized code for
    /// the load-time variants).
    code: String,
    /// User-facing message.
    message: String,
}

/// Map every [`BlueprintError`] variant to its UI-facing projection. Rulebook
/// violations pass through verbatim; the remaining variants cannot be produced
/// by `validate_blueprint` on an in-memory `Blueprint` (they come from
/// load-time paths — missing include files, parse failures, selection errors)
/// but are mapped anyway so the detail surface stays total over the enum.
fn blueprint_error_view(error: &BlueprintError) -> BlueprintErrorView {
    match error {
        BlueprintError::Rule { field, code, message } => BlueprintErrorView {
            field: Some(field.clone()),
            code: (*code).to_string(),
            message: message.clone(),
        },
        BlueprintError::Validation(message) => BlueprintErrorView {
            field: None,
            code: "validation".to_string(),
            message: message.clone(),
        },
        BlueprintError::UnknownNamedBlueprint(name, known) => BlueprintErrorView {
            field: None,
            code: "unknown_named_blueprint".to_string(),
            message: format!("unknown named blueprint '{name}' (known: {known})"),
        },
        BlueprintError::MissingIncludeFile(path) => BlueprintErrorView {
            field: None,
            code: "missing_include_file".to_string(),
            message: format!("blueprint include file not found: {path}"),
        },
        BlueprintError::ParseIncludeFile { path, detail } => BlueprintErrorView {
            field: None,
            code: "parse_include_file".to_string(),
            message: format!("failed to load blueprint include file '{path}': {detail}"),
        },
        BlueprintError::InvalidSelection(message) => BlueprintErrorView {
            field: None,
            code: "invalid_selection".to_string(),
            message: message.clone(),
        },
    }
}

/// Run-limit / budget tuning knobs surfaced from `MultiAgentConfig` in the
/// Studio and persisted back through `persist_orchestration_studio`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RunTuning {
    pub max_concurrent_agents: usize,
    pub max_concurrent_per_provider: usize,
    pub spend_cap_multiplier: f64,
}

// ============================================================================
// Studio messages
// ============================================================================

#[derive(Debug, Clone)]
pub enum StudioMessage {
    NewAgentName(String),
    NewAgentRole(String),
    AddAgent,
    RemoveAgent(String),
    SelectAgent(Option<String>),
    SelectRelationship(Option<usize>),
    /// Expand/collapse the "Add hand-off" form in the pipeline view.
    ToggleRelationshipEditor(bool),
    /// Expand/collapse the "+ Add agent" form in the library.
    ToggleAddAgentForm,
    /// A graph node was clicked; the payload is the index into `agents` (which
    /// equals the index into the graph model's nodes). Scrolls the Hand-offs
    /// list to the clicked agent's related row(s).
    GraphNodeClicked(usize),
    /// Expand/collapse the inline issue summary under the toolbar badge.
    ToggleValidationDetail,
    /// Expand/collapse one stage card's collapsible "Advanced" section. The
    /// payload is the stage's index into `blueprint.pipeline.stages`. This is
    /// a view-only toggle: it mutates the presentation set, never blueprint
    /// data, so it never marks the studio dirty.
    StageAdvancedToggle(usize),
    /// A stage's registry tag was edited (rulebook (g)/(j) field
    /// `"stage.tag"`). The payload is the stage index and the raw input
    /// text; `update` writes it through `Arc::make_mut` and revalidates.
    StageTagEdited(usize, String),
    /// A stage's human-readable label was edited. No rulebook rule targets
    /// `"stage.label"`, so this field never carries a per-field outline.
    StageLabelEdited(usize, String),
    /// A stage's closed engine kind changed (rulebook (b) field
    /// `"stage.kind"`). The picker is restricted to the six closed
    /// [`StageKind`] entries.
    StageKindChanged(usize, StageKind),
    /// Free-text stage-kind input (Slice 3, spec §2): `stage.kind` is an open
    /// `String`, so a user kind outside the six known `StageKind` entries must
    /// be typable — unknown kinds are valid (never gates, rulebook (c) only
    /// fires for `is_gate()`). The six known kinds are offered as suggestion
    /// buttons that emit [`StudioMessage::StageKindChanged`] instead.
    StageKindEdited(usize, String),
    /// Toggle one agent id's membership in a stage's staffing list
    /// (`stage.agents`): present → removed, absent → appended. Both the
    /// chip × button and the add pick-list emit this message.
    StageStaffingToggle(usize, AgentId),
    /// One stage-mask flag (`fs_write` / `shell`) was toggled; the explicit
    /// flag overlays the stage-kind default mask (ADR-58 D1). No rulebook
    /// rule targets `stage.flags`, so mask toggles never carry a field
    /// outline.
    StageMaskToggled(usize, StageMaskFlag, bool),
    /// A stage's observability feed binding changed (`None` = no feed entry).
    /// Rule (h) is enforced structurally by the closed `FeedLabel` enum, so
    /// this field has no rulebook path and never carries a field outline.
    StageFeedChanged(usize, Option<FeedLabel>),
    /// A stage's closed condition predicate changed (rulebook (i) field
    /// `"stage.condition"`).
    StageConditionChanged(usize, StageCondition),
    /// A stage's `max_cycles` input changed (rulebook (e) field
    /// `"stage.max_cycles"`). The payload is the raw draft text: an
    /// unparsable value is kept visible in the draft map and never reaches
    /// the model (mirrors the run-limit drafts).
    StageMaxCyclesEdited(usize, String),
    /// Reorder a stage one position earlier in the pipeline (spec §2): swaps
    /// with the previous stage; a no-op at the head. The index-keyed drafts
    /// are re-seeded for the swapped positions.
    StageMoveUp(usize),
    /// Reorder a stage one position later in the pipeline (spec §2): swaps
    /// with the next stage; a no-op at the tail.
    StageMoveDown(usize),
    /// Delete a stage card (spec §2). Relationships whose `from`/`to` tag the
    /// removed stage owned are dropped too — their row endpoints would
    /// otherwise dangle past the stage-tag picker's catalog.
    StageDeleted(usize),
    /// Append a default stage to the end of the pipeline (spec §2): a
    /// Freeform `run_once` kind with a unique `stage-N` tag, unflagged,
    /// unstaffed, and no feed / condition / cycle-cap override.
    StageAdded,
    /// One relationship row's `from` stage tag changed (row index into
    /// `blueprint.relationships`, spec §3). Rulebook note: `validate_blueprint`
    /// emits no relationship field paths — the rulebook (a)–(j) covers
    /// stages, fallbacks, and the global cap only — so relationship rows
    /// never carry a per-field outline.
    RelationshipFromChanged(usize, String),
    /// One relationship row's `to` stage tag changed (row index into
    /// `blueprint.relationships`, spec §3).
    RelationshipToChanged(usize, String),
    /// One relationship row's kind changed (row index into
    /// `blueprint.relationships`). The picker is restricted to the kinds
    /// registered in the open registry; `update` re-resolves the closed
    /// `RelationshipSemantics` from the registry so the kind/semantics pair
    /// stays consistent (ADR-58 §4).
    RelationshipKindChanged(usize, String),
    /// Delete one relationship row by its index into `blueprint.relationships`
    /// (row-level trash/× affordance, spec §3).
    RelationshipDeleted(usize),
    /// Append a default relationship row: first stage → first registered kind
    /// → first stage (`supervises`/Delegation when the registry is empty).
    RelationshipAdded,
    /// A stage fallback persona's `id` was edited (rulebook (d) field
    /// `"stage.fallback"`). The engine-owned sentinel id
    /// (`coordinator-self-execute`) is rendered read-only; every other id is
    /// editable.
    FallbackIdEdited(usize, String),
    /// A stage fallback persona's `label` was edited. No rulebook rule
    /// targets the label, so it never carries a field outline.
    FallbackLabelEdited(usize, String),
    /// A stage fallback persona's `system_instructions` were edited. The
    /// field is `Option<String>`; every edit writes `Some` so the text area
    /// always has a self-owned value to borrow.
    FallbackInstructionsEdited(usize, String),
    /// One fallback capability flag (`fs_write` / `shell`) was toggled
    /// (spec §4, stage-mask pattern). Rulebook (d) addresses widening
    /// beyond the stage-kind mask on `"stage.fallback.capabilities"`.
    FallbackCapabilityToggled(usize, StageMaskFlag, bool),
    /// Add a default fallback persona to the stage (oracle carry-over,
    /// spec §4): emitted by the "Add fallback persona" affordance on a
    /// stage without one. The payload is the stage index into
    /// `blueprint.pipeline.stages`.
    FallbackAdded(usize),
    InspectorSection(InspectorSection),
    SysPromptChanged(String),
    ConstraintsChanged(String),
    OutputFormatChanged(String),
    /// Selected agent's structured submission contract (output mode) changed.
    OutputModeChanged(OutputMode),
    /// Selected agent's pipeline lifecycle stage changed (`None` = Freeform).
    StageChanged(Option<AgentStage>),
    AddFewShot,
    FewShotInputChanged {
        idx: usize,
        value: String,
    },
    FewShotOutputChanged {
        idx: usize,
        value: String,
    },
    RemoveFewShot(usize),
    CapabilityToggled {
        agent: String,
        cap: Capability,
        value: bool,
    },
    DisabledToggled {
        agent: String,
        value: bool,
    },
    RunAgentsChanged(String),
    RunProviderChanged(String),
    RunSpendChanged(String),
    CapabilityPreset {
        agent: String,
        preset: PresetName,
    },
    AssignModel {
        agent_id: String,
        provider_id: String,
        model: String,
    },
    NewRelFrom(String),
    NewRelTo(String),
    NewRelType(String),
    NewRelMaxCycles(String),
    CreateRelationship,
    DeleteRelationship(usize),
    /// Create a new empty untitled pipeline and make it active.
    NewPipeline,
    LoadPreset(String),
    SaveOrchestration,
    SearchChanged(String),
    /// Return from an agent/relationship editor to the pipeline overview.
    ShowPipeline,
}

// ============================================================================
// Helpers
// ============================================================================

fn agent_to_custom(a: &AgentConfig) -> CustomAgentConfig {
    CustomAgentConfig {
        id: a.id.clone(),
        name: a.name.clone(),
        role: a.role.clone(),
        stage: a.stage.clone(),
        output_mode: a.output_mode,
        prompt_sections: a.prompt_sections.clone(),
        model_override: a.model_override.clone(),
        provider_id: a.provider_id.clone(),
        capabilities: a.capabilities.clone(),
        is_custom: a.is_custom,
        disabled: a.disabled,
    }
}

fn custom_to_agent(c: &CustomAgentConfig) -> AgentConfig {
    AgentConfig {
        id: c.id.clone(),
        name: c.name.clone(),
        role: c.role.clone(),
        stage: c.stage.clone(),
        output_mode: c.output_mode,
        prompt_sections: c.prompt_sections.clone(),
        model_override: c.model_override.clone(),
        provider_id: c.provider_id.clone(),
        capabilities: c.capabilities.clone(),
        is_custom: c.is_custom,
        disabled: c.disabled,
    }
}

fn caps_summary(c: &AgentCapabilities) -> String {
    let c = c.effective();
    let mut parts: Vec<&str> = Vec::new();
    if c.fs_read {
        parts.push("FS-R");
    }
    if c.fs_write {
        parts.push("FS-W");
    }
    if c.shell {
        parts.push("Shell");
    }
    if c.git {
        parts.push("Git");
    }
    if c.lsp {
        parts.push("LSP");
    }
    if c.eval {
        parts.push("Eval");
    }
    if parts.is_empty() {
        "none".into()
    } else {
        parts.join("·")
    }
}

/// Small rounded pill badge (palette-derived — no hardcoded colors). Used for
/// status chips like "Disabled" or "protected".
fn badge<'a>(theme: &'a AppTheme, label: &'a str) -> Element<'a, Message> {
    let bg = theme.palette.surface_variant;
    let border = theme.palette.border;
    container(text(label).size(theme.type_scale.caption).color(theme.palette.text_muted))
        .padding([1.0, 6.0])
        .style(move |_t: &iced::Theme| iced::widget::container::Style {
            background: Some(iced::Background::Color(bg)),
            border: iced::Border { color: border, width: 1.0, radius: 8.0.into() },
            ..iced::widget::container::Style::default()
        })
        .into()
}

/// Toolbar-badge label for blueprint validation errors (ADR-59 Decision 5):
/// the alert icon + error count — so the badge is never color alone — plus
/// the existing expand/collapse arrow shared with the legacy badge. The count
/// mirrors the detail bar, which renders one entry per stored error.
fn validation_badge_label(error_count: usize, expanded: bool) -> String {
    let arrow = if expanded { " ▴" } else { " ▾" };
    let plural = if error_count == 1 { "" } else { "s" };
    format!("⚠ {error_count} error{plural}{arrow}")
}

// ---------------------------------------------------------------------------
// Stage-card text helpers (P4 Batch 3, Slice 3a-1).
//
// Static display projections of the closed catalogs (`FeedLabel`,
// `StageCondition`, per-stage cycle caps). The editable pickers/numeric inputs
// are wired in Slice 3a-2; these helpers only render read-only values from
// `self.blueprint`.
// ---------------------------------------------------------------------------

/// Display name for a feed binding from the closed observability catalog
/// (blueprint §5.6).
fn feed_name(feed: FeedLabel) -> &'static str {
    match feed {
        FeedLabel::Understand => "Understand",
        FeedLabel::Plan => "Plan",
        FeedLabel::Execute => "Execute",
        FeedLabel::Verify => "Verify",
    }
}

/// Display name for the closed condition catalog (rulebook (i)).
fn condition_name(condition: StageCondition) -> &'static str {
    match condition {
        StageCondition::Always => "Always",
        StageCondition::OnGateCycle => "On gate cycle",
    }
}

/// The closed stage-kind picker catalog (spec §2): exactly the six closed
/// [`StageKind`] entries. Keep this the single source the kind picker
/// renders so the surface can never offer an open/foreign kind.
fn kind_options() -> [KindOption; 6] {
    [
        KindOption { kind: StageKind::Research },
        KindOption { kind: StageKind::Planning },
        KindOption { kind: StageKind::Execution },
        KindOption { kind: StageKind::Review },
        KindOption { kind: StageKind::Acceptance },
        KindOption { kind: StageKind::RunOnce },
    ]
}

/// The feed picker catalog (blueprint §5.6): `None` ("No feed") plus the
/// four closed `FeedLabel` entries. `None` is a first-class option —
/// removing a binding — which the underlying `Option<FeedLabel>` model
/// expresses as `None`.
fn feed_options() -> [FeedOption; 5] {
    [
        FeedOption { value: None },
        FeedOption { value: Some(FeedLabel::Understand) },
        FeedOption { value: Some(FeedLabel::Plan) },
        FeedOption { value: Some(FeedLabel::Execute) },
        FeedOption { value: Some(FeedLabel::Verify) },
    ]
}

/// The condition picker catalog: the two closed predicates from
/// [`StageCondition`] (rulebook (i)).
fn condition_options() -> [ConditionOption; 2] {
    [
        ConditionOption { value: StageCondition::Always },
        ConditionOption { value: StageCondition::OnGateCycle },
    ]
}

/// Glyph for a relationship's closed semantics (spec §3): shield =
/// ApprovalGate, flow arrow = ContextFlow, hierarchy/chain = Delegation.
/// Text glyphs only — the semantic affordance must stay visible under any
/// palette, so it is never color alone.
fn semantics_glyph(semantics: RelationshipSemantics) -> &'static str {
    match semantics {
        RelationshipSemantics::ApprovalGate => "🛡",
        RelationshipSemantics::ContextFlow => "➜",
        RelationshipSemantics::Delegation => "⛓",
    }
}

/// Short display label for a relationship's closed semantics, paired with
/// [`semantics_glyph`] in the kind picker options and the row's semantics
/// tag (spec §3).
fn semantics_label(semantics: RelationshipSemantics) -> &'static str {
    match semantics {
        RelationshipSemantics::ApprovalGate => "approval gate",
        RelationshipSemantics::ContextFlow => "context flow",
        RelationshipSemantics::Delegation => "delegation",
    }
}

/// Per-field validation surface (ADR-59 D5, spec §5): wrap one editable
/// widget so a rulebook violation on its field path renders as a 1px
/// `theme.palette.danger` border around the field, an alert icon to the
/// right (a text glyph — never color alone), and a hover tooltip carrying
/// the rule message. `None` (no violation) returns the field untouched so
/// clean fields render exactly as before.
fn outlined_field<'a>(
    theme: &'a AppTheme,
    error: Option<String>,
    field: Element<'a, Message>,
) -> Element<'a, Message> {
    let Some(message) = error else {
        return field;
    };
    let ts = &theme.type_scale;
    let sp = &theme.spacing;
    let icon = tooltip(
        text("⚠").size(ts.caption).color(theme.palette.danger),
        container(text(message).size(ts.caption)).padding([sp.xs, sp.sm]).style(
            move |_t: &iced::Theme| iced::widget::container::Style {
                background: Some(iced::Background::Color(theme.palette.surface_variant)),
                border: iced::Border {
                    color: theme.palette.border,
                    width: 1.0,
                    radius: 6.0.into(),
                },
                ..iced::widget::container::Style::default()
            },
        ),
        iced::widget::tooltip::Position::Top,
    )
    .gap(4.0_f32);
    container(row![field, icon].spacing(sp.xs).align_y(Alignment::Center))
        .padding([sp.xs, sp.sm])
        .style(move |_t: &iced::Theme| iced::widget::container::Style {
            border: iced::Border { color: theme.palette.danger, width: 1.0, radius: 6.0.into() },
            ..iced::widget::container::Style::default()
        })
        .into()
}

/// Tab-strip button for the Inspector. Shared so future tabs (e.g. a dedicated
/// Lifecycle pane) render identically without copy-pasting the active/inactive
/// style ternary.
fn inspector_tab<'a>(
    active: bool,
    label: &'static str,
    section: InspectorSection,
) -> iced::widget::Button<'a, Message> {
    iced::widget::button(label)
        .style(if active { crate::ui::button::primary } else { crate::ui::button::secondary })
        .on_press(Message::OrchestrationStudio(StudioMessage::InspectorSection(section)))
}

fn cap_toggle(agent: String, cap: Capability) -> impl Fn(bool) -> Message {
    move |v| {
        Message::OrchestrationStudio(StudioMessage::CapabilityToggled {
            agent: agent.clone(),
            cap,
            value: v,
        })
    }
}

fn dfs_cycle<'a>(
    node: &'a str,
    adj: &HashMap<&'a str, Vec<&'a str>>,
    visited: &mut HashSet<&'a str>,
    stack: &mut HashSet<&'a str>,
) -> bool {
    if stack.contains(node) {
        return true;
    }
    if visited.contains(node) {
        return false;
    }
    visited.insert(node);
    stack.insert(node);
    if let Some(neighbors) = adj.get(node) {
        for n in neighbors {
            if dfs_cycle(n, adj, visited, stack) {
                return true;
            }
        }
    }
    stack.remove(node);
    false
}

fn relationships_have_cycle(relationships: &[AgentRelationshipConfig]) -> bool {
    let mut adj: HashMap<&str, Vec<&str>> = HashMap::new();
    for relationship in relationships {
        adj.entry(relationship.from.as_str()).or_default().push(relationship.to.as_str());
    }
    let mut visited: HashSet<&str> = HashSet::new();
    let mut stack: HashSet<&str> = HashSet::new();
    for start in adj.keys() {
        if !visited.contains(start) && dfs_cycle(start, &adj, &mut visited, &mut stack) {
            return true;
        }
    }
    false
}

fn default_builtin_agents() -> Vec<AgentConfig> {
    // Matches config::supported.builtin_agent_seeds() typed submission contracts.
    vec![
        AgentConfig {
            id: "coordinator".into(),
            name: "Coordinator".into(),
            role: "coordinator".into(),
            stage: None,
            output_mode: OutputMode::Freeform,
            prompt_sections: PromptSections {
                system_instructions: "You are the Coordinator. Break the incoming task into a short plan, delegate each step to the right specialist (Architect, Researcher, Coder, Reviewer, Validator), and synthesize their outputs into a final answer. You do not write code or run commands yourself.".into(),
                constraints: "Never bypass a specialist to do their job directly. If a step is ambiguous, get clarification from the Architect or Researcher before assigning it to the Coder. Don't exceed configured max_cycles between any two agents. If a specialist reports a blocking risk (security, data loss, destructive command), stop and surface it to the user instead of proceeding.".into(),
                output_format: "1) Plan (numbered steps), 2) Specialist assignments, 3) Final synthesized result once specialists report back. No internal chain-of-thought.".into(),
                ..Default::default()
            },
            model_override: None,
            provider_id: None,
            capabilities: AgentCapabilities { fs_read: Some(true), ..Default::default() },
            is_custom: false,
            disabled: false,
        },
        AgentConfig {
            id: "architect".into(),
            name: "Architect".into(),
            role: "architect".into(),
            stage: Some(AgentStage::new(AgentStage::DESIGN)),
            output_mode: OutputMode::DesignDoc,
            prompt_sections: PromptSections {
                system_instructions: "You are the Software Architect. Produce a high-level design: goals, constraints, proposed files, interface sketch, risks.".into(),
                constraints: "Base the design on the actual codebase — read files before proposing changes; don't invent APIs/files without saying so. Flag anything touching auth, credentials, payments, or user data as high-risk. Keep the design proportional — don't over-architect a one-line fix.".into(),
                output_format: "ONLY a valid JSON DesignDoc: goals, constraints, proposed_files, interface_sketch, risks. No Markdown, no prose outside the JSON.".into(),
                ..Default::default()
            },
            model_override: None,
            provider_id: None,
            capabilities: AgentCapabilities { fs_read: Some(true), lsp: Some(true), ..Default::default() },
            is_custom: false,
            disabled: false,
        },
        AgentConfig {
            id: "researcher".into(),
            name: "Researcher".into(),
            role: "researcher".into(),
            stage: Some(AgentStage::new(AgentStage::RESEARCH)),
            output_mode: OutputMode::ResearchReport,
            prompt_sections: PromptSections {
                system_instructions: "You are the Researcher. Investigate the codebase and produce factual findings with citations.".into(),
                constraints: "Every claim needs a path:line citation. If you can't find evidence, say so rather than guessing. Don't speculate about original-author intent beyond what code/comments show.".into(),
                output_format: "Findings list — one-sentence claim + citation. End with an 'Open questions' section.".into(),
                ..Default::default()
            },
            model_override: None,
            provider_id: None,
            capabilities: AgentCapabilities { fs_read: Some(true), lsp: Some(true), ..Default::default() },
            is_custom: false,
            disabled: false,
        },
        AgentConfig {
            id: "coder".into(),
            name: "Coder".into(),
            role: "coder".into(),
            stage: Some(AgentStage::new(AgentStage::IMPLEMENT)),
            output_mode: OutputMode::Freeform,
            prompt_sections: PromptSections {
                system_instructions: "You are the Coder. Implement the Architect's changes precisely and safely.".into(),
                constraints: "Only touch files in the Architect's proposed_files unless strictly required to keep the build compiling — call that out explicitly. Never run destructive shell commands (rm -rf, force-push, history rewrite) without explicit approval. Never commit or expose secrets/API keys. Add tests where the existing suite has a pattern for it. If a needed capability is denied, stop and report rather than failing silently.".into(),
                output_format: "Files changed + one-line rationale each, then the diff/patch. Flag anything skipped and why.".into(),
                ..Default::default()
            },
            model_override: None,
            provider_id: None,
            capabilities: AgentCapabilities {
                fs_read: Some(true),
                fs_write: Some(true),
                shell: Some(true),
                git: Some(true),
                lsp: Some(true),
                eval: Some(false),
            },
            is_custom: false,
            disabled: false,
        },
        AgentConfig {
            id: "reviewer".into(),
            name: "Reviewer".into(),
            role: "reviewer".into(),
            stage: Some(AgentStage::new(AgentStage::REVIEW)),
            output_mode: OutputMode::ReviewReport,
            prompt_sections: PromptSections {
                system_instructions: "You are the Reviewer. Check the Coder's output against the Architect's requirements and the Researcher's findings.".into(),
                constraints: "Review against explicit requirements, not personal style, unless style violates conventions found in git history. Call out missing tests, unhandled errors, security issues, DesignDoc deviations. Approve only with zero blocking issues.".into(),
                output_format: "Verdict: Approved / Changes Requested. If the latter, numbered required fixes with file:line refs, plus a separate 'Optional' section for nice-to-haves.".into(),
                ..Default::default()
            },
            model_override: None,
            provider_id: None,
            capabilities: AgentCapabilities { fs_read: Some(true), git: Some(true), lsp: Some(true), ..Default::default() },
            is_custom: false,
            disabled: false,
        },
        AgentConfig {
            id: "validator".into(),
            name: "Validator".into(),
            role: "validator".into(),
            stage: Some(AgentStage::new(AgentStage::VALIDATE)),
            output_mode: OutputMode::Freeform,
            prompt_sections: PromptSections {
                system_instructions: "You are the Validator. Run the eval engine (build/tests/lint) and report whether acceptance criteria are met — you don't reason about correctness yourself, you report what actually ran.".into(),
                constraints: "Never mark a task passing if the build fails or tests are skipped/ignored. Separate flaky-looking failures from deterministic ones. Never modify files — report back to the Coordinator if a fix is needed.".into(),
                output_format: "Pass/Fail, then raw eval output (or truncated tail), then a one-line summary of what changed since the last run.".into(),
                ..Default::default()
            },
            model_override: None,
            provider_id: None,
            capabilities: AgentCapabilities { fs_read: Some(true), shell: Some(true), eval: Some(true), ..Default::default() },
            is_custom: false,
            disabled: false,
        },
    ]
}

fn standard_pipeline_preset() -> PipelinePreset {
    PipelinePreset {
        name: "Standard Pipeline".into(),
        description: "Coordinator → Architect → Researcher → Coder → Reviewer → Validator".into(),
        agents: Vec::new(),
        relationships: vec![
            AgentRelationshipConfig {
                from: "coordinator".into(),
                to: "architect".into(),
                relationship: "supervises".into(),
                max_cycles: Some(3),
            },
            AgentRelationshipConfig {
                from: "architect".into(),
                to: "researcher".into(),
                relationship: "provides_context_to".into(),
                max_cycles: Some(3),
            },
            AgentRelationshipConfig {
                from: "researcher".into(),
                to: "coder".into(),
                relationship: "provides_context_to".into(),
                max_cycles: Some(3),
            },
            AgentRelationshipConfig {
                from: "coder".into(),
                to: "reviewer".into(),
                relationship: "supervises".into(),
                max_cycles: Some(3),
            },
            AgentRelationshipConfig {
                from: "reviewer".into(),
                to: "validator".into(),
                relationship: "supervises".into(),
                max_cycles: Some(3),
            },
        ],
        is_builtin: true,
    }
}

// ============================================================================
// State implementation
// ============================================================================

impl State {
    pub fn new() -> Self {
        Self {
            agents: default_builtin_agents(),
            presets: vec![standard_pipeline_preset()],
            ..Default::default()
        }
    }

    /// Merge persisted config (custom agents, relationships, presets, model
    /// assignments, available providers) over the seeded built-ins.
    ///
    /// Roster ownership (maintainer revision of ADR-58/59): when the config
    /// owns the roster (`custom_agents` non-empty or `[orchestration]`
    /// present) the config's agent list IS the roster — deleted seeds stay
    /// deleted and never reappear after a restart. The embedded five seeds
    /// stand in only when the config declares neither (legacy embedded
    /// default).
    pub fn load_from_config(&mut self, config: &AppConfig) {
        let multi = config.multi_agent.clone().unwrap_or_default();
        let config_agents: Vec<AgentConfig> =
            multi.custom_agents.iter().map(custom_to_agent).collect();
        self.agents = if config.owns_agent_roster() {
            // The config IS the roster: whatever the config lists (possibly
            // nothing after the user deleted every agent) is exactly what the
            // Studio shows. No seed resurrection.
            config_agents
        } else {
            let mut agents = default_builtin_agents();
            for ac in config_agents {
                if let Some(existing) = agents.iter_mut().find(|a| a.id == ac.id) {
                    *existing = ac;
                } else {
                    agents.push(ac);
                }
            }
            agents
        };
        self.relationships = multi.relationships.clone();
        // The relationship list was replaced wholesale, so the index-based
        // selection (and any in-flight hand-off edit) no longer refers to the
        // same row. Agent selection is id-based and survives; relationship
        // selection is index-based and must not.
        self.selected_relationship = None;
        self.show_relationship_editor = false;
        self.clear_relationship_draft();

        let mut presets = vec![standard_pipeline_preset()];
        // Defensively dedupe by name: pre-#116 configs may already contain the
        // built-in "Standard Pipeline" as a persisted entry, so the list is
        // seeded built-in-first and each config preset is appended only once.
        let mut seen: HashSet<String> = HashSet::from([presets[0].name.clone()]);
        for preset in &multi.presets {
            if seen.insert(preset.name.clone()) {
                presets.push(preset.clone());
            }
        }
        self.presets = presets;

        self.max_concurrent_agents = multi.max_concurrent_agents;
        self.max_concurrent_per_provider = multi.max_concurrent_per_provider;
        self.spend_cap_multiplier = multi.spend_cap_multiplier;
        self.run_agents_draft = multi.max_concurrent_agents.to_string();
        self.run_provider_draft = multi.max_concurrent_per_provider.to_string();
        self.run_spend_draft = multi.spend_cap_multiplier.to_string();

        let ms = config.model_settings.clone().unwrap_or_default();
        self.global_default_model = ms.global_default_model.clone();
        self.model_assignments = ms.agent_assignments.clone();
        for assignment in &self.model_assignments {
            if let Some(agent) = self.agents.iter_mut().find(|agent| {
                agent.id == assignment.agent_role || agent.role == assignment.agent_role
            }) {
                agent.provider_id = Some(assignment.provider_config_id.clone());
                agent.model_override = assignment.model_override.clone();
            }
        }
        // Backfill assignments from per-agent model pins already present in
        // `custom_agents` (a config may pin models per agent without persisted
        // `agent_assignments`). Only entries with no existing assignment are
        // added, so pre-existing assignments are never duplicated. This
        // mirrors config state faithfully, so the studio stays clean (no
        // mark_dirty) and Save persists the pins into `agent_assignments`.
        for agent in &self.agents {
            let Some(provider_id) = &agent.provider_id else {
                continue;
            };
            if self.model_assignments.iter().any(|assignment| {
                assignment.agent_role == agent.role || assignment.agent_role == agent.id
            }) {
                continue;
            }
            self.model_assignments.push(AgentModelAssignment {
                agent_role: agent.role.clone(),
                provider_config_id: provider_id.clone(),
                model_override: agent.model_override.clone(),
            });
        }
        self.available_providers = if ms.providers.is_empty() {
            vec!["openai".into(), "anthropic".into(), "ollama".into()]
        } else {
            ms.providers.iter().map(|p| p.id.clone()).collect()
        };
        self.unsaved = false;
        self.save_error = None;
        // load_from_config does not go through mark_dirty(); invalidate the
        // memoized graph cache explicitly since agents/relationships changed.
        *self.graph_cache.borrow_mut() = None;

        // ADR-59 blueprint read-path mirror (P4 Batch 3, Slice 1). Populate
        // the blueprint-typed fields from the config. Each stays `None`
        // gracefully when the source is absent (handled below). The legacy
        // tables above remain authoritative for configs without
        // `[orchestration]` — this block is purely additive, so the
        // resulting legacy surface is byte-equivalent to pre-Slice-1
        // behavior.
        //
        // `orchestration` is cloned rather than derived so the view can
        // distinguish "legacy `multi_agent` authoritative" from "blueprint
        // model active" (see the state-field docs for why
        // `resolved_blueprint.is_some()` cannot serve as that signal).
        self.orchestration = config.orchestration.clone();
        // The blueprint-typed fields are populated only on the blueprint
        // path. The load seam resolves a blueprint on EVERY load (legacy
        // configs get the default `standard` attached), so unconditionally
        // mirroring `config.resolved_blueprint` would leak a
        // `Some(standard)` into legacy-mode Studio state; gating on
        // `orchestration` keeps the legacy path's blueprint fields `None`
        // and the dual-path decision exact.
        if config.orchestration.is_some() {
            // `resolved_blueprint` is a cheap `Arc` clone (shared allocation).
            self.resolved_blueprint = config.resolved_blueprint.clone();
            // The editable `Blueprint` is the raw model inside the resolved
            // blueprint — cloned so the Studio can later mutate it without
            // aliasing the runtime's `Arc<ResolvedBlueprint>`.
            self.blueprint = config
                .resolved_blueprint
                .as_ref()
                .map(|resolved| Arc::new(resolved.blueprint.clone()));
            // Mirror the config-load seam's ADR-52 run cap (`lib.rs` ~220) so
            // rule (f) is bounded by the same `max_total_iterations` the load
            // path enforced, then recompute the structured per-field errors
            // for the freshly loaded editable blueprint (ADR-59 D5).
            self.global_max_dispatch_cycles = multi.max_total_iterations;
            // The editable stage list was replaced wholesale: reset the
            // per-stage max-cycles drafts and re-seed them from the fresh
            // model so the inputs never show stale text from a previous
            // pipeline, and always have a self-owned value to borrow.
            self.stage_max_cycles_drafts.clear();
            if let Some(blueprint) = &self.blueprint {
                for (index, stage) in blueprint.pipeline.stages.iter().enumerate() {
                    self.stage_max_cycles_drafts.insert(
                        index,
                        stage.max_cycles.map(|value| value.to_string()).unwrap_or_default(),
                    );
                }
            }
            self.refresh_blueprint_validation();
        } else {
            self.resolved_blueprint = None;
            self.blueprint = None;
            // No blueprint path: no editable model, no rule-(f) bound, and no
            // structured errors to surface on the legacy surface.
            self.global_max_dispatch_cycles = None;
            self.blueprint_errors.clear();
        }
    }

    /// Parts of the studio that should be persisted back to config.
    pub fn persisted_parts(
        &self,
    ) -> (Vec<CustomAgentConfig>, Vec<AgentRelationshipConfig>, Vec<PipelinePreset>) {
        let custom = self.agents.iter().map(agent_to_custom).collect();
        let rels = self.relationships.clone();
        // Built-in presets are seeded in code and must never round-trip
        // through config — persisting them is what made the list grow.
        let presets = self.presets.iter().filter(|p| !p.is_builtin).cloned().collect();
        (custom, rels, presets)
    }

    /// Canonical run-limit / budget knobs to persist back to `MultiAgentConfig`.
    pub fn run_tuning(&self) -> RunTuning {
        RunTuning {
            max_concurrent_agents: self.max_concurrent_agents,
            max_concurrent_per_provider: self.max_concurrent_per_provider,
            spend_cap_multiplier: self.spend_cap_multiplier,
        }
    }

    pub fn validation(&self) -> ValidationReport {
        // ADR-59 D5: when `[orchestration]` is present the Studio validates
        // the editable `Blueprint` model with the config-load rulebook instead
        // of the legacy `multi_agent` checks. The legacy path below is
        // untouched — no `[orchestration]` section → byte-identical checks
        // and report.
        if self.orchestration.is_some() {
            return self.blueprint_validation();
        }

        let mut messages: Vec<String> = Vec::new();

        if relationships_have_cycle(&self.relationships) {
            messages.push("Cycle detected in relationship graph".into());
        }

        // Run-limit / budget drafts gate the Save button: a non-numeric or
        // below-minimum draft keeps the canonical untouched but blocks saving.
        if self.run_agents_draft.parse::<usize>().map_or(true, |v| v < 1) {
            messages.push("Run limits: max concurrent agents must be a whole number ≥ 1".into());
        }
        if self.run_provider_draft.parse::<usize>().map_or(true, |v| v < 1) {
            messages
                .push("Run limits: max concurrent per provider must be a whole number ≥ 1".into());
        }
        if self.run_spend_draft.parse::<f64>().map_or(true, |v| v <= 0.0) {
            messages.push("Run limits: spend cap multiplier must be a positive number".into());
        }

        if !self.agents.iter().any(|a| a.id == "coordinator" || a.role == "coordinator") {
            messages.push("No coordinator agent present".into());
        }

        // ADR-35 phase 4: the eval engine is gated on the validator's `eval`
        // capability. Warn in the studio so an eval-off validator doesn't
        // fail every Build at the validation stage unexpectedly.
        if let Some(validator) =
            self.agents.iter().find(|a| a.id == "validator" || a.role == "validator")
        {
            if !validator.capabilities.effective().eval {
                messages.push(
                    "Validator has the Eval Engine capability off: multi-agent Builds will \
                     fail with 'validation disabled' until it is re-enabled or the agent is \
                     disabled entirely"
                        .into(),
                );
            }
        }

        for i in 0..self.relationships.len() {
            let relationship = &self.relationships[i];
            if !self.agents.iter().any(|agent| agent.id == relationship.from) {
                messages.push(format!("Unknown source agent: {}", relationship.from));
            }
            if !self.agents.iter().any(|agent| agent.id == relationship.to) {
                messages.push(format!("Unknown target agent: {}", relationship.to));
            }
            if relationship.max_cycles == Some(0) {
                messages.push(format!(
                    "{} → {} must allow at least one cycle",
                    self.agent_label(&relationship.from),
                    self.agent_label(&relationship.to)
                ));
            }
            for j in (i + 1)..self.relationships.len() {
                let a = relationship;
                let b = &self.relationships[j];
                if a.from == b.from && a.to == b.to && a.relationship == b.relationship {
                    messages.push(format!("Duplicate relationship: {} -> {}", a.from, a.to));
                }
            }
        }

        ValidationReport { ok: messages.is_empty(), messages }
    }

    /// Blueprint-path validation (ADR-59 D5): run `validate_blueprint` on the
    /// editable model and render the same report shape the legacy checks use.
    /// `validate_blueprint` fails fast, so the report carries at most one
    /// message; the full structured error (field path + code) lives in
    /// `self.blueprint_errors` via `errors_for`. Every `BlueprintError` variant
    /// maps through `blueprint_error_view` — the Studio never unwraps or
    /// panics on a validation failure.
    fn blueprint_validation(&self) -> ValidationReport {
        let Some(blueprint) = &self.blueprint else {
            // `[orchestration]` present but the editable model was never
            // populated (e.g. a config built by `AppConfig::default()` in
            // tests): surface a clear issue instead of a silent pass.
            return ValidationReport {
                ok: false,
                messages: vec!["Blueprint model is not loaded".into()],
            };
        };
        match validate_blueprint(blueprint, self.global_max_dispatch_cycles) {
            Ok(()) => ValidationReport { ok: true, messages: Vec::new() },
            Err(error) => {
                let view = blueprint_error_view(&error);
                ValidationReport { ok: false, messages: vec![view.message] }
            }
        }
    }

    /// Recompute the stored per-field validation errors for the editable
    /// blueprint (ADR-59 D5). Called on every `load_from_config`; later slices
    /// call it after each Studio mutation of `self.blueprint` so the toolbar
    /// badge and detail bar stay current. Never panics: a missing `blueprint`
    /// simply clears the collection.
    pub fn refresh_blueprint_validation(&mut self) {
        self.blueprint_errors = match self.blueprint.as_ref().and_then(|blueprint| {
            validate_blueprint(blueprint, self.global_max_dispatch_cycles).err()
        }) {
            Some(error) => vec![error],
            None => Vec::new(),
        };
    }

    /// Number of structured validation errors on the editable blueprint model.
    /// Backs the persistent toolbar badge on the blueprint path.
    pub fn blueprint_error_count(&self) -> usize {
        self.blueprint_errors.len()
    }

    /// The editable blueprint model (`None` on the legacy path, or before the
    /// config load seam populated it). Clone of the field: the App's
    /// blueprint-path save consumes the model through `Arc::make_mut`-friendly
    /// ownership without aliasing the runtime's resolved blueprint.
    pub fn blueprint(&self) -> Option<Arc<Blueprint>> {
        self.blueprint.clone()
    }

    /// Rulebook violations addressing the dotted field path `field` (e.g.
    /// `"stage.tag"`, `"stage.max_cycles"`, `"orchestration.max_total_iterations"`)
    /// — the per-field addressing key for Slice 3's field outlines. Message-only
    /// variants (`Validation`, load-time failures) carry no field and never match.
    pub fn errors_for(&self, field: &str) -> Vec<&BlueprintError> {
        self.blueprint_errors
            .iter()
            .filter(|error| match error {
                BlueprintError::Rule { field: error_field, .. } => error_field.as_str() == field,
                _ => false,
            })
            .collect()
    }

    /// The rule message for an editable widget's rulebook field path (ADR-59
    /// D5, spec §5), or `None` when the field is clean. Reads the stored
    /// `blueprint_errors` — the authoritative collection recomputed by
    /// `refresh_blueprint_validation` on every mutation, never by `view()`.
    /// `validate_blueprint` fails fast so at most one stored error exists;
    /// `errors_for` matches it by exact dotted path, so no aggregation or
    /// per-stage disambiguation is needed.
    fn field_error(&self, field: &str) -> Option<String> {
        match self.errors_for(field).first() {
            Some(BlueprintError::Rule { message, .. }) => Some(message.clone()),
            _ => None,
        }
    }

    /// The single mutation path for one stage of the editable blueprint
    /// (ADR-59 D5): writes through `Arc::make_mut` (clones the blueprint
    /// only while it is shared — `Blueprint: Clone`) and immediately
    /// recomputes the stored per-field validation so `blueprint_errors`
    /// stays authoritative. `view()` never validates;
    /// `refresh_blueprint_validation()` is the only refresh site. A missing
    /// blueprint or an out-of-range stage index is a silent no-op — the
    /// Studio never panics on a stale message. Blueprint edits do not mark
    /// the studio dirty (persistence is out of scope for the stage-card
    /// surface; `SaveOrchestration` stays a no-op).
    fn mutate_stage(&mut self, index: usize, edit: impl FnOnce(&mut StageDef)) {
        {
            let Some(arc) = self.blueprint.as_mut() else {
                return;
            };
            let blueprint = Arc::make_mut(arc);
            if let Some(stage) = blueprint.pipeline.stages.get_mut(index) {
                edit(stage);
            }
        }
        self.refresh_blueprint_validation();
    }

    /// The single whole-blueprint mutation path (ADR-59 D5): writes through
    /// `Arc::make_mut` (clones the blueprint only while it is shared —
    /// `Blueprint: Clone`) and immediately recomputes the stored per-field
    /// validation. This is the primitive `mutate_stage`,
    /// `mutate_relationship`, and `mutate_fallback` delegate to, so every
    /// blueprint-path edit shares one refresh site. A missing blueprint is a
    /// silent no-op — the Studio never panics on a stale message — and
    /// blueprint edits never mark the studio dirty (persistence is out of
    /// scope; `SaveOrchestration` stays a no-op).
    fn mutate_blueprint(&mut self, edit: impl FnOnce(&mut Blueprint)) {
        {
            let Some(arc) = self.blueprint.as_mut() else {
                return;
            };
            edit(Arc::make_mut(arc));
        }
        self.refresh_blueprint_validation();
    }

    /// The single mutation path for one relationship row of the editable
    /// blueprint (spec §3): an out-of-range row index is a silent no-op.
    fn mutate_relationship(&mut self, index: usize, edit: impl FnOnce(&mut RelationshipDef)) {
        self.mutate_blueprint(|blueprint| {
            if let Some(relationship) = blueprint.relationships.get_mut(index) {
                edit(relationship);
            }
        });
    }

    /// The single mutation path for one stage's fallback persona (spec §4):
    /// a missing blueprint, an out-of-range stage index, or a stage without a
    /// fallback persona is a silent no-op — the fallback card only renders
    /// for stages that own one, so stale messages stay harmless.
    fn mutate_fallback(&mut self, stage_index: usize, edit: impl FnOnce(&mut FallbackPersonaDef)) {
        self.mutate_blueprint(|blueprint| {
            if let Some(stage) = blueprint.pipeline.stages.get_mut(stage_index) {
                if let Some(fallback) = stage.fallback.as_mut() {
                    edit(fallback);
                }
            }
        });
    }

    /// Swap two pipeline stages (Slice 3 reorder). Both bounds are guarded — a
    /// stale message is a silent no-op — and the write goes through
    /// [`Self::mutate_blueprint`] so validation refreshes. The index-keyed
    /// `max_cycles` drafts shift with the swap, so they are re-seeded.
    fn move_stage(&mut self, from: usize, to: usize) {
        let len = self.blueprint.as_ref().map(|b| b.pipeline.stages.len()).unwrap_or(0);
        if from >= len || to >= len {
            return;
        }
        self.mutate_blueprint(|blueprint| blueprint.pipeline.stages.swap(from, to));
        self.reseed_stage_drafts();
    }

    /// Remove one stage and every relationship whose `from`/`to` tag it owned
    /// (Slice 3 delete): a dangling row would otherwise render unselected in
    /// the relationship view, which is the stage-tag picker's only catalog.
    /// The index-keyed drafts are re-seeded for the shifted tail.
    fn delete_stage(&mut self, index: usize) {
        let removed_tag = {
            let mut tag = None;
            self.mutate_blueprint(|blueprint| {
                if index < blueprint.pipeline.stages.len() {
                    tag = Some(blueprint.pipeline.stages.remove(index).tag);
                }
            });
            tag
        };
        if let Some(tag) = removed_tag {
            self.mutate_blueprint(|blueprint| {
                blueprint
                    .relationships
                    .retain(|relationship| relationship.from != tag && relationship.to != tag);
            });
        }
        self.reseed_stage_drafts();
    }

    /// Append a default stage to the end of the pipeline (Slice 3 add): a
    /// Freeform `run_once` kind with a unique `stage-N` tag (collision-suffixed
    /// against the existing tags), unflagged, unstaffed, and no feed /
    /// condition / cycle-cap override. Mutation goes through
    /// [`Self::mutate_blueprint`] so validation refreshes; the drafts gain the
    /// new entry.
    fn add_stage(&mut self) {
        self.mutate_blueprint(|blueprint| {
            let mut suffix = blueprint.pipeline.stages.len() + 1;
            let tag = loop {
                let candidate = format!("stage-{suffix}");
                if !blueprint.pipeline.stages.iter().any(|stage| stage.tag == candidate) {
                    break candidate;
                }
                suffix += 1;
            };
            let index = blueprint.pipeline.stages.len();
            blueprint.pipeline.stages.push(StageDef {
                tag,
                label: format!("Stage {}", index + 1),
                kind: StageKind::RunOnce.as_str().to_string(),
                version: 1,
                flags: concerto_config::StageFlags::default(),
                condition: StageCondition::Always,
                max_cycles: None,
                feed: None,
                primary: false,
                agents: Vec::new(),
                fallback: None,
                files: None,
            });
        });
        self.reseed_stage_drafts();
    }

    /// Rebuild the per-stage `max_cycles` drafts from the current blueprint
    /// after a structural edit (add / remove / reorder) shifts the index space
    /// — mirrors the `load_from_config` seeding so every input keeps a
    /// self-owned value.
    fn reseed_stage_drafts(&mut self) {
        self.stage_max_cycles_drafts.clear();
        if let Some(blueprint) = &self.blueprint {
            for (index, stage) in blueprint.pipeline.stages.iter().enumerate() {
                self.stage_max_cycles_drafts.insert(
                    index,
                    stage.max_cycles.map(|value| value.to_string()).unwrap_or_default(),
                );
            }
        }
    }

    fn relationship_draft(&self) -> Result<AgentRelationshipConfig, String> {
        if self.new_rel_from.is_empty() {
            return Err("Choose a source agent".into());
        }
        if self.new_rel_to.is_empty() {
            return Err("Choose a target agent".into());
        }
        if self.new_rel_from == self.new_rel_to {
            return Err("Source and target must be different agents".into());
        }
        let relationship = if self.new_rel_type.is_empty() {
            "supervises".to_string()
        } else {
            self.new_rel_type.clone()
        };
        let max_cycles = if self.new_rel_max_cycles.trim().is_empty() {
            None
        } else {
            let parsed = self
                .new_rel_max_cycles
                .trim()
                .parse::<u32>()
                .map_err(|_| "Max cycles must be a positive whole number".to_string())?;
            if parsed == 0 {
                return Err("Max cycles must be at least 1".into());
            }
            Some(parsed)
        };
        let candidate = AgentRelationshipConfig {
            from: self.new_rel_from.clone(),
            to: self.new_rel_to.clone(),
            relationship,
            max_cycles,
        };
        if self.relationships.iter().enumerate().any(|(index, existing)| {
            Some(index) != self.selected_relationship
                && existing.from == candidate.from
                && existing.to == candidate.to
                && existing.relationship == candidate.relationship
        }) {
            return Err("That relationship already exists".into());
        }
        let mut proposed = self.relationships.clone();
        if let Some(index) = self.selected_relationship {
            if let Some(existing) = proposed.get_mut(index) {
                *existing = candidate.clone();
            }
        } else {
            proposed.push(candidate.clone());
        }
        if relationships_have_cycle(&proposed) {
            return Err("This relationship would create a dependency cycle".into());
        }
        Ok(candidate)
    }

    fn clear_relationship_draft(&mut self) {
        self.selected_relationship = None;
        self.new_rel_from.clear();
        self.new_rel_to.clear();
        self.new_rel_type.clear();
        self.new_rel_max_cycles.clear();
    }

    fn mark_dirty(&mut self) {
        self.unsaved = true;
        self.saved_notice = false;
        self.save_error = None;
        // The memoized pipeline graph derives from `agents` + `relationships`;
        // every dirty mutation must drop the cache so the next render rebuilds
        // it (issue #112 bounds-churn guard).
        *self.graph_cache.borrow_mut() = None;
    }

    pub fn mark_saved(&mut self) {
        self.unsaved = false;
        self.saved_notice = true;
        self.save_error = None;
    }

    pub fn mark_save_failed(&mut self, error: String) {
        self.saved_notice = false;
        self.save_error = Some(error);
    }

    /// Update the per-provider model cache so the unified model picker shows
    /// the latest models (e.g. after a provider is added in Settings or models
    /// are refreshed from the API).
    pub fn sync_models(&mut self, models_by_provider: HashMap<String, Vec<String>>) {
        self.cached_models_by_provider = models_by_provider;
    }

    fn agent_label(&self, id: &str) -> String {
        self.agents
            .iter()
            .find(|a| a.id == id)
            .map(|a| a.name.clone())
            .unwrap_or_else(|| id.to_string())
    }

    /// Build the pipeline canvas model from the current agents and
    /// relationships. Node ids are indices into `self.agents`. Returns the
    /// model plus an `edge_to_relationship` map (model edge id → index into
    /// `self.relationships`): relationships with unknown endpoints are skipped
    /// (validation() surfaces them), so without the map a click on edge `i`
    /// could not be resolved back to the correct relationship row.
    ///
    /// The result is memoized in `graph_cache` and rebuilt only when the cache
    /// is invalidated (any agents/relationships mutation), keeping
    /// `graph_height` and node positions stable across renders (#112).
    fn pipeline_graph_model(&self) -> (AgentGraphModel, Vec<usize>) {
        if let Some(cached) = self.graph_cache.borrow().as_ref() {
            return cached.clone();
        }
        let mut model = AgentGraphModel::new();
        for agent in &self.agents {
            model.add_node(agent.name.clone(), AgentState::Idle, AgentId::new(agent.id.clone()));
        }
        let mut edge_to_relationship = Vec::new();
        for (rel_index, rel) in self.relationships.iter().enumerate() {
            // Unknown endpoints are surfaced by validation(); skip the edge
            // here rather than drawing a dangling arrow.
            let Some(from) = self.agents.iter().position(|a| a.id == rel.from) else {
                continue;
            };
            let Some(to) = self.agents.iter().position(|a| a.id == rel.to) else {
                continue;
            };
            let kind = if rel.relationship == "supervises" {
                EdgeKind::Delegation
            } else {
                EdgeKind::Dependency
            };
            let cycles =
                rel.max_cycles.map(|value| value.to_string()).unwrap_or_else(|| "∞".into());
            model.add_labeled_edge(from, to, kind, format!("{} · {}", rel.relationship, cycles));
            edge_to_relationship.push(rel_index);
        }
        let result = (model, edge_to_relationship);
        // Cache the freshly-built model; the caller keeps the original so the
        // model is only cloned once (into the cache).
        *self.graph_cache.borrow_mut() = Some(result.clone());
        result
    }

    /// Resolve a graph node click to the scroll operation for the Hand-offs
    /// list. The Hand-offs list renders `self.relationships.iter()` in order,
    /// so the first row whose `from`/`to` matches the clicked agent has a list
    /// index equal to its relationship index. `None` when the index is out of
    /// range or the agent has no hand-offs yet.
    fn graph_node_click_task(&self, idx: usize) -> Option<iced::Task<Message>> {
        let agent = self.agents.get(idx)?;
        let row =
            self.relationships.iter().position(|rel| rel.from == agent.id || rel.to == agent.id)?;
        let n = self.relationships.len().max(1) as f32;
        let offset = iced_core::widget::operation::scrollable::RelativeOffset {
            x: Some(0.0),
            y: Some(row as f32 / n),
        };
        Some(iced::advanced::widget::operate(iced_core::widget::operation::scrollable::snap_to(
            iced::widget::Id::new(STUDIO_HANDOFFS_LIST_ID),
            offset,
        )))
    }

    /// Node colors for the pipeline canvas: built-in specialists keep their
    /// theme-assigned role colors; everything else is colored by lifecycle
    /// stage (disabled agents are dimmed to the surface variant, regardless
    /// of their role color). The coordinator — the hub — gets the theme's
    /// primary hue instead of the unassigned fallback.
    fn graph_colors(&self, theme: &AppTheme) -> HashMap<AgentId, Color> {
        let mut colors = theme.palette.agent_roles.clone();
        for agent in &self.agents {
            let id = AgentId::new(agent.id.clone());
            if agent.disabled {
                colors.insert(id, theme.palette.surface_variant);
                continue;
            }
            if agent.id == "coordinator" {
                colors.insert(id, theme.palette.primary);
                continue;
            }
            if colors.contains_key(&id) {
                continue;
            }
            let color = match agent.stage.as_ref().map(|s| s.as_str()) {
                Some(AgentStage::DESIGN) => theme.palette.primary,
                Some(AgentStage::RESEARCH) => theme.palette.secondary,
                Some(AgentStage::IMPLEMENT) => theme.palette.success,
                Some(AgentStage::REVIEW) => theme.palette.warning,
                Some(AgentStage::VALIDATE) => theme.palette.danger,
                _ => theme.palette.text_muted,
            };
            colors.insert(id, color);
        }
        colors
    }

    /// Cheap display estimate. Loading a model tokenizer during every Iced
    /// render made merely opening Studio consume a full CPU core.
    fn token_estimate_for(&self, agent: &AgentConfig) -> u64 {
        let p = &agent.prompt_sections;
        let mut characters = p.system_instructions.chars().count()
            + p.constraints.chars().count()
            + p.output_format.chars().count();
        for ex in &p.few_shot {
            characters += ex.input.chars().count() + ex.output.chars().count();
        }
        (characters as u64).div_ceil(4)
    }

    pub fn update(&mut self, message: StudioMessage) -> iced::Task<Message> {
        match message {
            StudioMessage::NewAgentName(s) => self.new_agent_name = s,
            StudioMessage::NewAgentRole(s) => self.new_agent_role = s,
            StudioMessage::AddAgent => {
                if !self.new_agent_name.trim().is_empty() {
                    let mut suffix = self.agents.len();
                    let new_id = loop {
                        let candidate = format!("custom_{suffix}");
                        if self.agents.iter().all(|agent| agent.id != candidate) {
                            break candidate;
                        }
                        suffix += 1;
                    };
                    self.agents.push(AgentConfig {
                        id: new_id.clone(),
                        name: self.new_agent_name.trim().to_string(),
                        role: if self.new_agent_role.trim().is_empty() {
                            "Custom".into()
                        } else {
                            self.new_agent_role.trim().to_string()
                        },
                        stage: None,
                        output_mode: OutputMode::default(),
                        prompt_sections: PromptSections::default(),
                        model_override: None,
                        provider_id: None,
                        capabilities: AgentCapabilities::default(),
                        is_custom: true,
                        disabled: false,
                    });
                    self.new_agent_name.clear();
                    self.new_agent_role.clear();
                    self.selected_agent_id = Some(new_id);
                    self.inspector_section = InspectorSection::Prompt;
                    self.mark_dirty();
                }
            }
            StudioMessage::RemoveAgent(id) => {
                // The coordinator is code-constructed and always active (see
                // ADR-35 §5). Guard the invariant at the message level, not
                // only in the view's "Remove" affordance, so no future code
                // path (renamed role, loaded preset) can delete it.
                if id == "coordinator" {
                    return iced::Task::none();
                }
                self.agents.retain(|a| a.id != id);
                if self.selected_agent_id.as_ref() == Some(&id) {
                    self.selected_agent_id = None;
                }
                self.relationships.retain(|r| r.from != id && r.to != id);
                // Pruning shifts the relationship index space; a stale
                // selection could silently target a different row (or an
                // out-of-range one). Drop any in-flight edit.
                if self.selected_relationship.is_some() {
                    self.show_relationship_editor = false;
                    self.clear_relationship_draft();
                }
                self.mark_dirty();
            }
            StudioMessage::SelectAgent(opt) => {
                self.selected_agent_id = opt;
                self.selected_relationship = None;
                self.show_relationship_editor = false;
                self.clear_relationship_draft();
                if self.selected_agent_id.is_some() {
                    self.inspector_section = InspectorSection::Prompt;
                }
            }
            StudioMessage::SelectRelationship(opt) => {
                // Guard against stale indices (an edge click resolved after a
                // same-frame relationship removal): an out-of-range selection
                // is treated as "nothing selected" rather than opening an
                // empty editor aimed at a row that no longer exists.
                let opt = opt.filter(|index| *index < self.relationships.len());
                self.selected_relationship = opt;
                self.show_relationship_editor = opt.is_some();
                self.selected_agent_id = None;
                if let Some(index) = opt {
                    if let Some(relationship) = self.relationships.get(index) {
                        self.new_rel_from = relationship.from.clone();
                        self.new_rel_to = relationship.to.clone();
                        self.new_rel_type = relationship.relationship.clone();
                        self.new_rel_max_cycles = relationship
                            .max_cycles
                            .map(|value| value.to_string())
                            .unwrap_or_default();
                    }
                } else {
                    self.clear_relationship_draft();
                }
            }
            StudioMessage::ToggleRelationshipEditor(open) => {
                self.show_relationship_editor = open;
                if open {
                    // Start a blank hand-off; editing an existing one goes
                    // through SelectRelationship (e.g. an edge click).
                    self.selected_relationship = None;
                    self.clear_relationship_draft();
                }
            }
            StudioMessage::ToggleAddAgentForm => {
                self.show_add_agent_form = !self.show_add_agent_form;
            }
            StudioMessage::GraphNodeClicked(idx) => {
                // Clicking a node snaps the Hand-offs list to the related
                // row(s); nothing else to mutate.
                if let Some(task) = self.graph_node_click_task(idx) {
                    return task;
                }
            }
            StudioMessage::ToggleValidationDetail => {
                self.show_validation_detail = !self.show_validation_detail;
            }
            StudioMessage::StageAdvancedToggle(index) => {
                // View-only presentation toggle: no `mark_dirty()`, no
                // blueprint mutation. `remove` returns whether the index was
                // present; a miss means "was closed, open it now".
                if !self.stage_advanced_open.remove(&index) {
                    self.stage_advanced_open.insert(index);
                }
            }
            StudioMessage::StageTagEdited(index, value) => {
                self.mutate_stage(index, |stage| stage.tag = value);
            }
            StudioMessage::StageLabelEdited(index, value) => {
                self.mutate_stage(index, |stage| stage.label = value);
            }
            StudioMessage::StageKindChanged(index, kind) => {
                self.mutate_stage(index, |stage| stage.kind = kind.as_str().to_string());
            }
            StudioMessage::StageKindEdited(index, kind) => {
                // Free-text kind (Slice 3): the model carries the open string
                // as typed, so an unknown user kind is valid — rulebook (c)
                // ignores non-gate kinds and `is_gate()` only matches the
                // known `Review`/`Acceptance` kinds.
                self.mutate_stage(index, |stage| stage.kind = kind);
            }
            StudioMessage::StageStaffingToggle(index, agent_id) => {
                // Toggle membership: present → removed, absent → appended.
                // `AgentId` normalizes to lowercase, which keeps the chip ×
                // and the add pick-list on the same id space as the legacy
                // agent library (`self.agents`).
                //
                // The coordinator is engine-owned (ADR-35 §5, Slice 3): it is
                // never selectable as stage staff. Guard at the message level,
                // not only in the picker's candidate filter, so no future code
                // path (a stale chip, a hand-edited blueprint) can staff it.
                if agent_id.as_str() == "coordinator" {
                    return iced::Task::none();
                }
                self.mutate_stage(index, |stage| {
                    let id = agent_id.as_str();
                    if stage.agents.iter().any(|existing| existing == id) {
                        stage.agents.retain(|existing| existing != id);
                    } else {
                        stage.agents.push(id.to_string());
                    }
                });
            }
            StudioMessage::StageMaskToggled(index, flag, value) => {
                self.mutate_stage(index, |stage| match flag {
                    StageMaskFlag::FsWrite => stage.flags.fs_write = Some(value),
                    StageMaskFlag::Shell => stage.flags.shell = Some(value),
                });
            }
            StudioMessage::StageFeedChanged(index, feed) => {
                self.mutate_stage(index, |stage| stage.feed = feed);
            }
            StudioMessage::StageConditionChanged(index, condition) => {
                self.mutate_stage(index, |stage| stage.condition = condition);
            }
            StudioMessage::StageMaxCyclesEdited(index, draft) => {
                // Keep the raw draft so an unparsable value stays visible in
                // the input (mirrors the run-limit drafts): the field never
                // fights the user's typing. Only a value that parses (or an
                // empty string, meaning "use the kind default") reaches the
                // model; a parse failure leaves the stored value untouched.
                self.stage_max_cycles_drafts.insert(index, draft.clone());
                let trimmed = draft.trim();
                if trimmed.is_empty() {
                    self.mutate_stage(index, |stage| stage.max_cycles = None);
                } else if let Ok(value) = trimmed.parse::<u32>() {
                    // `Some(0)` is written deliberately: rule (e) then
                    // surfaces the violation inline on `stage.max_cycles`.
                    self.mutate_stage(index, |stage| stage.max_cycles = Some(value));
                }
            }
            StudioMessage::StageMoveUp(index) => {
                if index > 0 {
                    self.move_stage(index, index - 1);
                }
            }
            StudioMessage::StageMoveDown(index) => {
                if let Some(len) = self.blueprint.as_ref().map(|b| b.pipeline.stages.len()) {
                    if index + 1 < len {
                        self.move_stage(index, index + 1);
                    }
                }
            }
            StudioMessage::StageDeleted(index) => self.delete_stage(index),
            StudioMessage::StageAdded => self.add_stage(),
            StudioMessage::RelationshipFromChanged(index, value) => {
                self.mutate_relationship(index, |relationship| relationship.from = value);
            }
            StudioMessage::RelationshipToChanged(index, value) => {
                self.mutate_relationship(index, |relationship| relationship.to = value);
            }
            StudioMessage::RelationshipKindChanged(index, kind) => {
                // The kind picker only offers kinds registered in the open
                // registry, so the new kind's closed semantics always exists
                // there; resolving before the mutation keeps the closure free
                // of the blueprint borrow. A miss (defensive — a stale
                // message) keeps the row's current semantics unchanged.
                let semantics = self.blueprint.as_ref().and_then(|arc| {
                    arc.relationships
                        .iter()
                        .find(|relationship| relationship.kind == kind)
                        .map(|relationship| relationship.semantics)
                });
                self.mutate_relationship(index, |relationship| {
                    relationship.kind = kind;
                    if let Some(semantics) = semantics {
                        relationship.semantics = semantics;
                    }
                });
            }
            StudioMessage::RelationshipDeleted(index) => {
                self.mutate_blueprint(|blueprint| {
                    if index < blueprint.relationships.len() {
                        blueprint.relationships.remove(index);
                    }
                });
            }
            StudioMessage::RelationshipAdded => {
                // Model-appropriate default row (spec §3): first stage →
                // first registered kind → first stage. On an empty registry
                // the default kind is `supervises` (Delegation), the standard
                // blueprint's first row — so the first add always registers a
                // kind the picker can then offer.
                let first_stage = self
                    .blueprint
                    .as_ref()
                    .and_then(|arc| arc.pipeline.stages.first())
                    .map(|stage| stage.tag.clone())
                    .unwrap_or_default();
                let (kind, semantics) = self
                    .blueprint
                    .as_ref()
                    .and_then(|arc| arc.relationships.first())
                    .map(|relationship| (relationship.kind.clone(), relationship.semantics))
                    .unwrap_or_else(|| {
                        ("supervises".to_string(), RelationshipSemantics::Delegation)
                    });
                self.mutate_blueprint(|blueprint| {
                    blueprint.relationships.push(RelationshipDef {
                        kind,
                        semantics,
                        from: first_stage.clone(),
                        to: first_stage,
                    });
                });
            }
            StudioMessage::FallbackIdEdited(index, value) => {
                self.mutate_fallback(index, |fallback| fallback.id = value);
            }
            StudioMessage::FallbackLabelEdited(index, value) => {
                self.mutate_fallback(index, |fallback| fallback.label = value);
            }
            StudioMessage::FallbackInstructionsEdited(index, value) => {
                // `system_instructions` is `Option<String>`; editing always
                // writes `Some` so the input has a self-owned value to
                // borrow (an empty string renders nothing).
                self.mutate_fallback(index, |fallback| fallback.system_instructions = Some(value));
            }
            StudioMessage::FallbackCapabilityToggled(index, flag, value) => {
                // Writes the explicit catalog flag (stage-mask pattern,
                // spec §4); rulebook (d) surfaces widening on
                // `"stage.fallback.capabilities"` immediately.
                self.mutate_fallback(index, |fallback| match flag {
                    StageMaskFlag::FsWrite => fallback.capabilities.fs_write = Some(value),
                    StageMaskFlag::Shell => fallback.capabilities.shell = Some(value),
                });
            }
            StudioMessage::FallbackAdded(index) => {
                // Oracle carry-over (spec §4): a stage without a fallback
                // persona gets a default one. The default id must satisfy
                // rulebook (d) (differ from every agent staffed in the
                // stage), so a stage-derived id with a collision suffix is
                // synthesized — mirroring the `AddAgent` id-suffix loop. The
                // resulting persona is user-added: its id is editable (the
                // engine-owned sentinel id is never synthesized here).
                self.mutate_stage(index, |stage| {
                    if stage.fallback.is_some() {
                        return;
                    }
                    let base = format!("{}_fallback", stage.tag);
                    let mut suffix = 0usize;
                    let id = loop {
                        let candidate =
                            if suffix == 0 { base.clone() } else { format!("{base}_{suffix}") };
                        if !stage.agents.iter().any(|agent| agent == &candidate) {
                            break candidate;
                        }
                        suffix += 1;
                    };
                    stage.fallback = Some(FallbackPersonaDef {
                        id,
                        label: format!("{} fallback", stage.label),
                        system_instructions: None,
                        capabilities: concerto_config::StageFlags::default(),
                    });
                });
            }
            StudioMessage::ShowPipeline => {
                self.selected_agent_id = None;
                self.show_relationship_editor = false;
                self.clear_relationship_draft();
            }
            StudioMessage::InspectorSection(s) => self.inspector_section = s,
            StudioMessage::SysPromptChanged(s) => {
                if let Some(id) = &self.selected_agent_id {
                    if let Some(a) = self.agents.iter_mut().find(|a| &a.id == id) {
                        a.prompt_sections.system_instructions = s;
                        self.mark_dirty();
                    }
                }
            }
            StudioMessage::ConstraintsChanged(s) => {
                if let Some(id) = &self.selected_agent_id {
                    if let Some(a) = self.agents.iter_mut().find(|a| &a.id == id) {
                        a.prompt_sections.constraints = s;
                        self.mark_dirty();
                    }
                }
            }
            StudioMessage::OutputFormatChanged(s) => {
                if let Some(id) = &self.selected_agent_id {
                    if let Some(a) = self.agents.iter_mut().find(|a| &a.id == id) {
                        a.prompt_sections.output_format = s;
                        self.mark_dirty();
                    }
                }
            }
            StudioMessage::OutputModeChanged(mode) => {
                if let Some(id) = &self.selected_agent_id {
                    if let Some(a) = self.agents.iter_mut().find(|a| &a.id == id) {
                        a.output_mode = mode;
                        self.mark_dirty();
                    }
                }
            }
            StudioMessage::StageChanged(stage) => {
                if let Some(id) = &self.selected_agent_id {
                    if let Some(a) = self.agents.iter_mut().find(|a| &a.id == id) {
                        a.stage = stage;
                        self.mark_dirty();
                    }
                }
            }
            StudioMessage::AddFewShot => {
                if let Some(id) = &self.selected_agent_id {
                    if let Some(a) = self.agents.iter_mut().find(|a| &a.id == id) {
                        a.prompt_sections.few_shot.push(FewShotExample::default());
                        self.mark_dirty();
                    }
                }
            }
            StudioMessage::FewShotInputChanged { idx, value } => {
                if let Some(id) = &self.selected_agent_id {
                    if let Some(a) = self.agents.iter_mut().find(|a| &a.id == id) {
                        if let Some(ex) = a.prompt_sections.few_shot.get_mut(idx) {
                            ex.input = value;
                            self.mark_dirty();
                        }
                    }
                }
            }
            StudioMessage::FewShotOutputChanged { idx, value } => {
                if let Some(id) = &self.selected_agent_id {
                    if let Some(a) = self.agents.iter_mut().find(|a| &a.id == id) {
                        if let Some(ex) = a.prompt_sections.few_shot.get_mut(idx) {
                            ex.output = value;
                            self.mark_dirty();
                        }
                    }
                }
            }
            StudioMessage::RemoveFewShot(idx) => {
                if let Some(id) = &self.selected_agent_id {
                    if let Some(a) = self.agents.iter_mut().find(|a| &a.id == id) {
                        if idx < a.prompt_sections.few_shot.len() {
                            a.prompt_sections.few_shot.remove(idx);
                            self.mark_dirty();
                        }
                    }
                }
            }
            StudioMessage::CapabilityToggled { agent, cap, value } => {
                if let Some(a) = self.agents.iter_mut().find(|a| a.id == agent) {
                    match cap {
                        Capability::FsRead => a.capabilities.fs_read = Some(value),
                        Capability::FsWrite => a.capabilities.fs_write = Some(value),
                        Capability::Shell => a.capabilities.shell = Some(value),
                        Capability::Git => a.capabilities.git = Some(value),
                        Capability::Lsp => a.capabilities.lsp = Some(value),
                        Capability::Eval => a.capabilities.eval = Some(value),
                    }
                    self.mark_dirty();
                }
            }
            StudioMessage::DisabledToggled { agent, value } => {
                if let Some(a) = self.agents.iter_mut().find(|a| a.id == agent) {
                    a.disabled = value;
                    self.mark_dirty();
                }
            }
            StudioMessage::CapabilityPreset { agent, preset } => {
                if let Some(a) = self.agents.iter_mut().find(|a| a.id == agent) {
                    a.capabilities = match preset {
                        PresetName::ReadOnlyResearcher => AgentCapabilities {
                            fs_read: Some(true),
                            fs_write: Some(false),
                            shell: Some(false),
                            git: Some(false),
                            lsp: Some(false),
                            eval: Some(false),
                        },
                        PresetName::FullCoder => AgentCapabilities {
                            fs_read: Some(true),
                            fs_write: Some(true),
                            shell: Some(true),
                            git: Some(true),
                            lsp: Some(true),
                            eval: Some(false),
                        },
                    };
                    self.mark_dirty();
                }
            }
            StudioMessage::AssignModel { agent_id, provider_id, model } => {
                let use_global_default = provider_id.is_empty();
                let model_override =
                    if use_global_default || model == "default" { None } else { Some(model) };
                if let Some(a) = self.agents.iter_mut().find(|a| a.id == agent_id) {
                    a.provider_id =
                        if use_global_default { None } else { Some(provider_id.clone()) };
                    a.model_override = model_override.clone();
                }
                if use_global_default {
                    self.model_assignments.retain(|assignment| assignment.agent_role != agent_id);
                } else if let Some(assign) =
                    self.model_assignments.iter_mut().find(|m| m.agent_role == agent_id)
                {
                    assign.provider_config_id = provider_id;
                    assign.model_override = model_override;
                } else {
                    self.model_assignments.push(AgentModelAssignment {
                        agent_role: agent_id,
                        provider_config_id: provider_id,
                        model_override,
                    });
                }
                self.mark_dirty();
            }
            StudioMessage::NewRelFrom(s) => self.new_rel_from = s,
            StudioMessage::NewRelTo(s) => self.new_rel_to = s,
            StudioMessage::NewRelType(s) => self.new_rel_type = s,
            StudioMessage::NewRelMaxCycles(s) => self.new_rel_max_cycles = s,
            StudioMessage::CreateRelationship => {
                if let Ok(candidate) = self.relationship_draft() {
                    if let Some(index) = self.selected_relationship {
                        if let Some(existing) = self.relationships.get_mut(index) {
                            *existing = candidate;
                        }
                    } else {
                        self.relationships.push(candidate);
                    }
                    self.clear_relationship_draft();
                    self.mark_dirty();
                }
            }
            StudioMessage::DeleteRelationship(idx) => {
                if idx < self.relationships.len() {
                    self.relationships.remove(idx);
                    self.clear_relationship_draft();
                    self.mark_dirty();
                }
            }
            StudioMessage::NewPipeline => {
                // Pick the first free "Untitled Pipeline N" name.
                let base = "Untitled Pipeline";
                let name = if self.presets.iter().all(|p| p.name != base) {
                    base.to_string()
                } else {
                    let mut suffix = 2;
                    loop {
                        let candidate = format!("{base} {suffix}");
                        if self.presets.iter().all(|p| p.name != candidate) {
                            break candidate;
                        }
                        suffix += 1;
                    }
                };
                self.presets.push(PipelinePreset {
                    name: name.clone(),
                    description: String::new(),
                    agents: vec![],
                    relationships: vec![],
                    is_builtin: false,
                });
                self.active_pipeline_name = name;
                self.relationships = vec![];
                self.selected_agent_id = None;
                self.selected_relationship = None;
                self.show_relationship_editor = false;
                self.clear_relationship_draft();
                self.mark_dirty();
            }
            StudioMessage::LoadPreset(name) => {
                self.active_pipeline_name = name.clone();
                if name == "Standard Pipeline" {
                    self.relationships = standard_pipeline_preset().relationships.clone();
                } else if let Some(p) = self.presets.iter().find(|p| p.name == name) {
                    for ca in &p.agents {
                        let ac = custom_to_agent(ca);
                        if let Some(existing) = self.agents.iter_mut().find(|a| a.id == ac.id) {
                            *existing = ac;
                        } else {
                            self.agents.push(ac);
                        }
                    }
                    self.relationships = p.relationships.clone();
                }
                self.selected_agent_id = None;
                self.show_relationship_editor = false;
                self.clear_relationship_draft();
                self.mark_dirty();
            }
            StudioMessage::SaveOrchestration => {}
            StudioMessage::SearchChanged(s) => self.search_query = s,
            StudioMessage::RunAgentsChanged(s) => {
                self.run_agents_draft = s;
                if let Ok(parsed) = self.run_agents_draft.parse::<usize>() {
                    if parsed >= 1 && parsed != self.max_concurrent_agents {
                        self.max_concurrent_agents = parsed;
                        self.mark_dirty();
                    }
                }
            }
            StudioMessage::RunProviderChanged(s) => {
                self.run_provider_draft = s;
                if let Ok(parsed) = self.run_provider_draft.parse::<usize>() {
                    if parsed >= 1 && parsed != self.max_concurrent_per_provider {
                        self.max_concurrent_per_provider = parsed;
                        self.mark_dirty();
                    }
                }
            }
            StudioMessage::RunSpendChanged(s) => {
                self.run_spend_draft = s;
                if let Ok(parsed) = self.run_spend_draft.parse::<f64>() {
                    if parsed > 0.0 && parsed != self.spend_cap_multiplier {
                        self.spend_cap_multiplier = parsed;
                        self.mark_dirty();
                    }
                }
            }
        }
        iced::Task::none()
    }

    /// ADR-59 P4 Batch 3, Slice 4b (UX spec §8 defect 3): the toolbar's
    /// "Modified" caption — rendered only while the studio holds unsaved
    /// changes, `None` (renders nothing) once saved. A bare caption keeps the
    /// toolbar single-line next to the Save button. Factored as a helper so
    /// the rendered-state logic is directly testable (iced 0.14 elements are
    /// opaque in headless tests — there is no text-extraction API).
    fn modified_caption<'a>(unsaved: bool, theme: &'a AppTheme) -> Option<Element<'a, Message>> {
        if unsaved {
            Some(
                text("Modified")
                    .size(theme.type_scale.caption)
                    .color(theme.palette.text_muted)
                    .into(),
            )
        } else {
            None
        }
    }

    /// ADR-59 P4 Batch 3, Slice 4b (oracle B2): whether the Studio is on the
    /// unified blueprint path — `[orchestration]` active AND the editable
    /// `Blueprint` loaded. On this path the legacy surfaces are hidden (the
    /// agent library, the per-agent inspector, and the "+ New"/preset
    /// actions): the stage cards ARE the editor (ADR-59 D1), and the Studio
    /// never calls `save_config` after P4 (ADR-59 D2), so legacy actions
    /// would edit a pipeline nothing loads. Factored as a helper so the exact
    /// branch that drives the toolbar/library/workspace hides is directly
    /// testable — iced 0.14 elements are opaque in headless tests, so the
    /// rendered-state logic lives here (see `modified_caption`).
    fn on_blueprint_path(&self) -> bool {
        self.orchestration.is_some() && self.blueprint.is_some()
    }

    pub fn view<'a>(&'a self, theme: &'a AppTheme) -> Element<'a, Message> {
        let ts = &theme.type_scale;
        let sp = &theme.spacing;
        // B2: computed once and reused by the toolbar, library, and workspace
        // branches below so the legacy hides cannot drift apart.
        let blueprint_path = self.on_blueprint_path();
        let preset_options: Vec<String> = self.presets.iter().map(|p| p.name.clone()).collect();
        let report = self.validation();
        let validation_status: Element<'_, Message> = if report.ok {
            text("Pipeline valid").color(theme.palette.success).size(ts.body).into()
        } else {
            let arrow = if self.show_validation_detail { " ▴" } else { " ▾" };
            if self.orchestration.is_some() {
                // ADR-59 D5: persistent blueprint-error badge — alert icon +
                // error count (never color alone), danger text on the
                // secondary style, mirroring the status-bar config badge.
                button(
                    text(validation_badge_label(
                        self.blueprint_error_count(),
                        self.show_validation_detail,
                    ))
                    .size(ts.body)
                    .color(theme.palette.danger),
                )
                .style(crate::ui::button::secondary)
                .on_press(Message::OrchestrationStudio(StudioMessage::ToggleValidationDetail))
                .into()
            } else {
                button(text(format!("{} issue(s){arrow}", report.messages.len())).size(ts.body))
                    .style(crate::ui::button::danger)
                    .on_press(Message::OrchestrationStudio(StudioMessage::ToggleValidationDetail))
                    .into()
            }
        };
        let save_button = if self.unsaved && report.ok {
            button("Save")
                .style(crate::ui::button::primary)
                .on_press(Message::OrchestrationStudio(StudioMessage::SaveOrchestration))
        } else {
            button("Save").style(crate::ui::button::primary)
        };
        // Save success/failure is surfaced as a toast at the App level (the
        // same channel Settings uses); the toolbar keeps only the persistent
        // validation status, which is a status indicator, not a one-off
        // notification.
        // Keep the toolbar single-line so every action (Save, + New, preset
        // picker) stays visible even with the app sidebar expanded — the
        // subtitle was the widest element and pushed the Save button out of
        // the window. The subtitle now lives full-width above the panes.
        //
        // "+ New" and the preset picker are LEGACY pipeline actions (they
        // mutate the legacy `multi_agent` tables and emit `LoadPreset`); on
        // the blueprint path they are hidden, not disabled (B2): the stage
        // cards are the only editor, and legacy edits would target a pipeline
        // nothing loads after P4 (ADR-59 D2 — the Studio never calls
        // `save_config`). `None` renders as a zero-size widget, so the
        // toolbar layout is unchanged on the legacy path.
        let legacy_actions: Option<Element<'_, Message>> = if blueprint_path {
            None
        } else {
            Some(
                row![
                    button("+ New")
                        .style(button::secondary)
                        .on_press(Message::OrchestrationStudio(StudioMessage::NewPipeline)),
                    pick_list(preset_options, Option::<String>::None, |name| {
                        Message::OrchestrationStudio(StudioMessage::LoadPreset(name))
                    })
                    .placeholder("Load preset…"),
                ]
                .spacing(sp.sm)
                .align_y(Alignment::Center)
                .into(),
            )
        };
        let toolbar = row![
            text("Orchestration Studio").size(ts.display),
            Space::new().width(Length::Fill),
            validation_status,
            text(format!("Pipeline: {}", self.active_pipeline_name))
                .size(ts.label)
                .color(theme.palette.text_muted),
            legacy_actions,
            // UX spec §8 defect 3: a quiet "Modified" caption while the
            // studio holds unsaved changes; nothing renders once saved.
            // `Option<Element>` renders nothing (zero-sized) when `None`.
            Self::modified_caption(self.unsaved, theme),
            save_button,
        ]
        .spacing(sp.md)
        .align_y(Alignment::Center);

        // ADR-58/59 (rewritten) Slice 3: the roster is a one-surface editor on BOTH
        // paths — the left 280px pane lists every agent (engine-owned
        // coordinator locked, the five seeds, and user agents) with
        // Edit/Delete affordances and the "+ New Agent" form. On the legacy
        // path this is exactly today's library; on the blueprint path the same
        // list doubles as the roster editor (its edits persist through the
        // roster Save arm, so they are no longer silently dropped).
        let library: Element<'_, Message> = self.library_view(theme);
        let workspace = if blueprint_path {
            // Slice 3 drill-down: selecting a roster agent routes the workspace
            // to the per-agent inspector (whose edits now persist through the
            // roster Save arm); otherwise the stage-card + relationship
            // surface renders as before.
            if self.selected_agent_id.is_some() {
                self.inspector_view(theme)
            } else {
                self.stage_cards_view(theme)
            }
        } else if self.selected_agent_id.is_some() {
            self.inspector_view(theme)
        } else {
            // ADR-58/59 (rewritten) Slice 2: no splash and no manual-init button — the roster
            // is auto-seeded on Studio open, so this inactive fallback renders
            // only as a defensive placeholder (a broken/torn-down config).
            self.blueprint_inactive_view(theme)
        };
        let panes = row![
            container(library).width(Length::Fixed(280.0)).height(Length::Fill),
            container(scrollable(workspace).height(Length::Fill))
                .padding([0.0, sp.md])
                .width(Length::Fill)
                .height(Length::Fill),
        ]
        .spacing(sp.md)
        .height(Length::Fill);

        // Inline issue summary expanded from the toolbar badge. Rendered as a
        // zero-height space when hidden so the layout height is unchanged. On
        // the blueprint path each item carries the field path + rule code +
        // message (ADR-59 D5); on the legacy path the pre-existing message
        // bullets are kept byte-identical.
        let validation_detail_bar: Element<'_, Message> = if self.show_validation_detail
            && !report.ok
        {
            let mut issues = column![].spacing(sp.xs);
            if self.orchestration.is_some() {
                issues = issues.push(
                    text(format!("{} blueprint validation error(s)", self.blueprint_error_count()))
                        .size(ts.caption)
                        .color(theme.palette.danger),
                );
                for error in &self.blueprint_errors {
                    let BlueprintErrorView { field, code, message } = blueprint_error_view(error);
                    let field = field.unwrap_or_else(|| "(blueprint)".to_string());
                    issues = issues.push(
                        row![
                            text(field).size(ts.caption).color(theme.palette.danger),
                            text(format!("[{code}]"))
                                .size(ts.caption)
                                .color(theme.palette.text_muted),
                            text(message).size(ts.caption).color(theme.palette.danger),
                        ]
                        .spacing(sp.xs)
                        .align_y(Alignment::Center),
                    );
                }
            } else {
                issues = issues.push(
                    text(format!("{} validation issue(s)", report.messages.len()))
                        .size(ts.caption)
                        .color(theme.palette.danger),
                );
                for message in &report.messages {
                    issues = issues.push(
                        text(format!("• {message}")).size(ts.caption).color(theme.palette.danger),
                    );
                }
            }
            container(issues)
                .width(Length::Fill)
                .padding([sp.xs, sp.sm])
                .style(move |_t: &iced::Theme| iced::widget::container::Style {
                    background: Some(iced::Background::Color(theme.palette.surface_variant)),
                    border: iced::Border {
                        color: theme.palette.border,
                        width: 1.0,
                        radius: 8.0.into(),
                    },
                    ..iced::widget::container::Style::default()
                })
                .into()
        } else {
            Space::new().height(0.0).into()
        };

        column![
            toolbar,
            text("Configure the agents and hand-offs used by multi-agent mode.")
                .size(ts.body)
                .color(theme.palette.text_muted),
            validation_detail_bar,
            iced::widget::rule::horizontal(1),
            panes,
        ]
        .spacing(sp.md)
        .height(Length::Fill)
        .into()
    }

    // ------------------------------------------------------------------
    // Blueprint surface inactive (ADR-58/59 (rewritten) Slice 2).
    //
    // Rendered only when the blueprint surface is NOT active — a defensive
    // fallback, because Slice 2 auto-seeds the orchestration roster on Studio
    // open (`App::ensure_orchestration_seeded`), so the surface is active
    // from the very first open. There is no splash and no manual-init button
    // anymore: this placeholder carries nothing actionable. Palette colors
    // only.
    // ------------------------------------------------------------------

    fn blueprint_inactive_view<'a>(&'a self, theme: &'a AppTheme) -> Element<'a, Message> {
        let ts = &theme.type_scale;
        let sp = &theme.spacing;
        let palette = &theme.palette;
        container(
            column![
                text("Orchestration blueprint inactive").size(ts.title).color(palette.text),
                text(
                    "The orchestration roster is not active. Check the project config to \
                     re-enable it.",
                )
                .size(ts.body)
                .color(palette.text_muted),
            ]
            .spacing(sp.md)
            .align_x(Alignment::Center),
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .center_x(Length::Fill)
        .center_y(Length::Fill)
        .into()
    }

    // ------------------------------------------------------------------
    // Stage-card surface (P4 Batch 3, Slice 3a-1).
    //
    // The blueprint-path main editor area: one card per `StageDef` in
    // `blueprint.pipeline.stages` (pipeline order), per spec §2. Everything
    // is read from `self.blueprint` (an `Arc` read — no mutations this
    // slice; edits land in 3a-2). Palette colors only; where a status
    // matters an icon/text pair is used, never color alone.
    // ------------------------------------------------------------------

    /// Vertical stack of stage cards plus the blueprint's name/description
    /// and the relationship surface below the cards (spec §3). Called only
    /// from the dual-path branch in `view()` (both `orchestration` and
    /// `blueprint` are `Some`).
    fn stage_cards_view<'a>(&'a self, theme: &'a AppTheme) -> Element<'a, Message> {
        let ts = &theme.type_scale;
        let sp = &theme.spacing;
        let Some(blueprint) = self.blueprint.as_ref() else {
            // Defensive: `view()` routes here only when `blueprint` is `Some`.
            return Space::new().into();
        };
        let mut cards = column![].spacing(sp.md);
        for (index, stage) in blueprint.pipeline.stages.iter().enumerate() {
            cards = cards.push(self.stage_card(theme, stage, index));
        }
        let description: Element<'_, Message> = blueprint
            .description
            .as_deref()
            .map(|description| {
                text(description).size(ts.body).color(theme.palette.text_muted).into()
            })
            .unwrap_or_else(|| Space::new().into());
        column![
            text(&blueprint.name).size(ts.title),
            description,
            iced::widget::rule::horizontal(1),
            cards,
            iced::widget::rule::horizontal(1),
            row![button("+ Add Stage")
                .style(crate::ui::button::secondary)
                .on_press(Message::OrchestrationStudio(StudioMessage::StageAdded)),]
            .padding([0.0, sp.xs]),
            self.relationships_view(theme),
        ]
        .width(Length::Fill)
        .spacing(sp.md)
        .padding([sp.xs, 0.0])
        .into()
    }

    /// One stage card. `index` is the stage's position in
    /// `blueprint.pipeline.stages`, used to key the collapsible "Advanced"
    /// section's expanded state in `self.stage_advanced_open`.
    fn stage_card<'a>(
        &'a self,
        theme: &'a AppTheme,
        stage: &'a StageDef,
        index: usize,
    ) -> Element<'a, Message> {
        let ts = &theme.type_scale;
        let sp = &theme.spacing;
        let open = self.stage_advanced_open.contains(&index);

        // Header: tag (editable, bold weight to keep the Slice-3a-1
        // emphasis) and label (editable). The rulebook addresses tag edits
        // via `"stage.tag"` (rule_g/rule_j); label has no rule of its own,
        // so its outline never fires but the helper stays uniform.
        let tag_input = text_input("tag", &stage.tag)
            .font(iced::Font { weight: iced::font::Weight::Bold, ..Default::default() })
            .width(Length::Fill)
            .on_input(move |value| {
                Message::OrchestrationStudio(StudioMessage::StageTagEdited(index, value))
            });
        let label_input =
            text_input("label", &stage.label).width(Length::Fill).on_input(move |value| {
                Message::OrchestrationStudio(StudioMessage::StageLabelEdited(index, value))
            });
        let header = row![
            outlined_field(theme, self.field_error("stage.tag"), tag_input.into()),
            outlined_field(theme, self.field_error("stage.label"), label_input.into()),
            // Slice 3 stage CRUD (§2): per-card reorder (swap with the
            // previous/next stage) and delete. The arrows are a no-op at the
            // pipeline head/tail via the bounds check in the update arms.
            button("↑")
                .style(button::text)
                .on_press(Message::OrchestrationStudio(StudioMessage::StageMoveUp(index))),
            button("↓")
                .style(button::text)
                .on_press(Message::OrchestrationStudio(StudioMessage::StageMoveDown(index))),
            button("×")
                .style(button::text)
                .on_press(Message::OrchestrationStudio(StudioMessage::StageDeleted(index))),
        ]
        .width(Length::Fill)
        .spacing(sp.sm)
        .align_y(Alignment::Center);

        // Kind: a free-text input (Slice 3, spec §2) — the model kind is an open
        // `String`, and unknown user kinds are valid (never gates; rulebook
        // (c) only fires for `is_gate()`, which matches the known
        // `Review`/`Acceptance` kinds). The six known `StageKind` entries are
        // offered as suggestion buttons that write the closed kind directly;
        // the enum value is normalized to its canonical snake_case string via
        // `StageKindChanged`, while typed text flows verbatim. Violations
        // addressed to `"stage.kind"` outline the input.
        let kind_input =
            text_input("kind", &stage.kind).width(Length::Fill).on_input(move |value| {
                Message::OrchestrationStudio(StudioMessage::StageKindEdited(index, value))
            });
        let known_kinds = kind_options();
        let mut suggestions: Vec<Element<'a, Message>> = Vec::new();
        for option in known_kinds {
            let kind = option.kind;
            suggestions.push(
                button(text(kind.label()).size(ts.caption))
                    .style(button::text)
                    .on_press(Message::OrchestrationStudio(StudioMessage::StageKindChanged(
                        index, kind,
                    )))
                    .into(),
            );
        }
        let kind = column![
            row![
                text("Kind").size(ts.caption).color(theme.palette.text_muted),
                outlined_field(theme, self.field_error("stage.kind"), kind_input.into()),
            ]
            .spacing(sp.sm)
            .align_y(Alignment::Center),
            row![
                text("suggestions:").size(ts.caption).color(theme.palette.text_muted),
                row(suggestions),
            ]
            .spacing(sp.xs)
            .align_y(Alignment::Center),
        ]
        .spacing(sp.xs);

        // Staffing: one chip per agent id with an × remove affordance, plus
        // an add pick-list of the unstaffed agents from the legacy library
        // (`self.agents`, the same source the hand-off editor uses). Both
        // the chip × and the picker emit `StageStaffingToggle`; the update
        // arm toggles membership so add and remove share one message.
        let chips: Vec<Element<'a, Message>> = stage
            .agents
            .iter()
            .map(|agent| {
                row![
                    badge(theme, agent.as_str()),
                    button("×").style(button::text).on_press(Message::OrchestrationStudio(
                        StudioMessage::StageStaffingToggle(index, AgentId::new(agent.clone())),
                    )),
                ]
                .spacing(sp.xs)
                .align_y(Alignment::Center)
                .into()
            })
            .collect();
        let candidates: Vec<AgentOption> = self
            .agents
            .iter()
            .filter(|agent| !stage.agents.contains(&agent.id))
            .map(|agent| AgentOption { id: agent.id.clone(), label: agent.name.clone() })
            .collect();
        let add_agent_pick = pick_list(candidates, Option::<AgentOption>::None, move |option| {
            Message::OrchestrationStudio(StudioMessage::StageStaffingToggle(
                index,
                AgentId::new(option.id),
            ))
        })
        .placeholder("+ Add agent");
        let staffing_control: Element<'a, Message> = if chips.is_empty() {
            text("Unstaffed").size(ts.body).color(theme.palette.text_muted).into()
        } else {
            row(chips).spacing(sp.xs).into()
        };
        let staffing: Element<'a, Message> =
            row![staffing_control, add_agent_pick].spacing(sp.sm).align_y(Alignment::Center).into();

        // Mask flags (fs_write / shell): toggle switches writing explicit
        // catalog flags. The toggle state is the EFFECTIVE mask (kind
        // default overlaid with explicit flags — D1 resolution), so turning
        // a flag off on a write-granted kind narrows the mask and turning it
        // on elsewhere widens it; the rulebook imposes no restriction on
        // explicit stage flags (unlike fallback capabilities), so all kinds
        // can edit. `stage.flags` has no rulebook field path, so mask
        // toggles never carry a field outline.
        let mask = stage.effective_capabilities();
        let mask_flags = row![
            toggler(mask.fs_write).label("FS Write").on_toggle(move |value| {
                Message::OrchestrationStudio(StudioMessage::StageMaskToggled(
                    index,
                    StageMaskFlag::FsWrite,
                    value,
                ))
            }),
            toggler(mask.shell).label("Shell").on_toggle(move |value| {
                Message::OrchestrationStudio(StudioMessage::StageMaskToggled(
                    index,
                    StageMaskFlag::Shell,
                    value,
                ))
            }),
        ]
        .spacing(sp.sm);

        // Advanced (collapsible): feed, condition, max cycles, and the
        // fallback persona become editable controls (spec §2/§4). The toggle
        // mutates only the view-only adjacency set, never blueprint data.
        let advanced_toggle = button(text(format!("Advanced {}", if open { "▴" } else { "▾" })))
            .style(crate::ui::button::secondary)
            .on_press(Message::OrchestrationStudio(StudioMessage::StageAdvancedToggle(index)));
        // Fallback persona: an editable card embedded in the stage card
        // (spec §4 placement) when the stage owns one; "—" when it does not.
        // `Blueprint` has no fallback field — `FallbackPersonaDef` lives only
        // as `StageDef.fallback` (blueprint.rs:286) — so the card renders
        // per-stage here rather than as a standalone blueprint-level card.
        let fallback: Element<'a, Message> = match &stage.fallback {
            Some(fallback) => self.fallback_persona_card(
                theme,
                fallback,
                // The fallback card renders the persona's write mask against
                // the stage-kind default. An unknown open kind carries no
                // engine mask (no writes); `RunOnce`'s mask is the same
                // no-write default, so it is the safe compile-time stand-in.
                StageKind::parse(&stage.kind).unwrap_or(StageKind::RunOnce),
                index,
            ),
            None => row![
                text("—").size(ts.body).color(theme.palette.text_muted),
                button("Add fallback persona")
                    .style(crate::ui::button::secondary)
                    .on_press(Message::OrchestrationStudio(StudioMessage::FallbackAdded(index))),
            ]
            .spacing(sp.sm)
            .align_y(Alignment::Center)
            .into(),
        };
        let advanced_body: Element<'a, Message> = if open {
            // `max_cycles` input: the draft map is seeded on load, so the
            // input always borrows a self-owned string; an unset cap shows
            // the placeholder. Rule (e) (`"stage.max_cycles"`) outlines it.
            let max_cycles_value: &str =
                self.stage_max_cycles_drafts.get(&index).map(String::as_str).unwrap_or("");
            let feed_pick =
                pick_list(feed_options(), Some(FeedOption { value: stage.feed }), move |option| {
                    Message::OrchestrationStudio(StudioMessage::StageFeedChanged(
                        index,
                        option.value,
                    ))
                });
            let condition_pick = pick_list(
                condition_options(),
                Some(ConditionOption { value: stage.condition }),
                move |option| {
                    Message::OrchestrationStudio(StudioMessage::StageConditionChanged(
                        index,
                        option.value,
                    ))
                },
            );
            let max_cycles_input =
                text_input("Max cycles", max_cycles_value).on_input(move |value| {
                    Message::OrchestrationStudio(StudioMessage::StageMaxCyclesEdited(index, value))
                });
            column![
                row![
                    text("Feed").size(ts.caption).color(theme.palette.text_muted),
                    outlined_field(theme, self.field_error("stage.feed"), feed_pick.into()),
                ]
                .spacing(sp.sm)
                .align_y(Alignment::Center),
                row![
                    text("Condition").size(ts.caption).color(theme.palette.text_muted),
                    outlined_field(
                        theme,
                        self.field_error("stage.condition"),
                        condition_pick.into(),
                    ),
                ]
                .spacing(sp.sm)
                .align_y(Alignment::Center),
                row![
                    text(format!("Max cycles (default: {})", stage.default_max_cycles()))
                        .size(ts.caption)
                        .color(theme.palette.text_muted),
                    outlined_field(
                        theme,
                        self.field_error("stage.max_cycles"),
                        max_cycles_input.into(),
                    ),
                ]
                .spacing(sp.sm)
                .align_y(Alignment::Center),
                fallback,
            ]
            .spacing(sp.sm)
            .into()
        } else {
            Space::new().into()
        };

        container(
            column![
                header,
                kind,
                row![
                    column![
                        text("Staffing").size(ts.caption).color(theme.palette.text_muted),
                        staffing,
                    ]
                    .spacing(sp.xs),
                    Space::new().width(Length::Fill),
                    column![
                        text("Mask flags").size(ts.caption).color(theme.palette.text_muted),
                        mask_flags,
                    ]
                    .spacing(sp.xs),
                ]
                .spacing(sp.md)
                .align_y(Alignment::Center),
                advanced_toggle,
                advanced_body,
            ]
            .spacing(sp.sm),
        )
        .width(Length::Fill)
        .padding([sp.sm, sp.md])
        .style(move |_t: &iced::Theme| iced::widget::container::Style {
            background: Some(iced::Background::Color(theme.palette.surface_variant)),
            border: iced::Border { color: theme.palette.border, width: 1.0, radius: 10.0.into() },
            ..iced::widget::container::Style::default()
        })
        .into()
    }

    // ------------------------------------------------------------------
    // Fallback persona card (P4 Batch 3, Slice 3b).
    //
    // `Blueprint` has no fallback field — `FallbackPersonaDef` exists only
    // as `StageDef.fallback` (blueprint.rs:286) — so the editable card embeds
    // in the owning stage card (spec §4 placement) instead of a standalone
    // blueprint-level card. Rulebook paths: `"stage.fallback"` (rule_c:
    // unstaffed gate without a fallback persona; rule_d: id colliding with a
    // staffed agent) and `"stage.fallback.capabilities"` (rule_d: widening
    // beyond the stage-kind mask) — quoted from `validate_blueprint`
    // (blueprint.rs:545-549, 562-569, 578-587).
    // ------------------------------------------------------------------

    /// Editable fallback-persona card for one stage (spec §4), rendered in
    /// the stage card's collapsible Advanced section. The engine-owned
    /// sentinel id (`coordinator-self-execute`) is read-only; label, system
    /// instructions, and the two capability flags are editable through
    /// `mutate_fallback`. All edits revalidate immediately via the single
    /// mutation path.
    fn fallback_persona_card<'a>(
        &'a self,
        theme: &'a AppTheme,
        fallback: &'a FallbackPersonaDef,
        stage_kind: StageKind,
        stage_index: usize,
    ) -> Element<'a, Message> {
        let ts = &theme.type_scale;
        let sp = &theme.spacing;
        // Sentinel ids are engine-owned (ADR-58 §5.9 non-overridables): the
        // input is read-only (no `on_input`) so the reserved identity cannot
        // be edited away.
        let sentinel = fallback.id == FALLBACK_SENTINEL_ID;
        let id_input = if sentinel {
            text_input("id", &fallback.id)
        } else {
            text_input("id", &fallback.id).on_input(move |value| {
                Message::OrchestrationStudio(StudioMessage::FallbackIdEdited(stage_index, value))
            })
        };
        let label_input = text_input("label", &fallback.label).on_input(move |value| {
            Message::OrchestrationStudio(StudioMessage::FallbackLabelEdited(stage_index, value))
        });
        // The repo's long-text pattern is a plain `text_input` (the Inspector
        // prompt pane uses it for System Instructions); mirror that here.
        let instructions = text_input(
            "System instructions",
            fallback.system_instructions.as_deref().unwrap_or(""),
        )
        .on_input(move |value| {
            Message::OrchestrationStudio(StudioMessage::FallbackInstructionsEdited(
                stage_index,
                value,
            ))
        });
        // Capability toggles mirror the stage-mask pattern (spec §4): the
        // toggle state is the EFFECTIVE mask (stage-kind default overlaid
        // with explicit flags — `FallbackPersonaDef::effective_capabilities`).
        // Widening beyond the stage-kind mask trips rule (d) inline on
        // `"stage.fallback.capabilities"`.
        let mask = fallback.effective_capabilities(stage_kind);
        let flags = row![
            toggler(mask.fs_write).label("FS Write").on_toggle(move |value| {
                Message::OrchestrationStudio(StudioMessage::FallbackCapabilityToggled(
                    stage_index,
                    StageMaskFlag::FsWrite,
                    value,
                ))
            }),
            toggler(mask.shell).label("Shell").on_toggle(move |value| {
                Message::OrchestrationStudio(StudioMessage::FallbackCapabilityToggled(
                    stage_index,
                    StageMaskFlag::Shell,
                    value,
                ))
            }),
        ]
        .spacing(sp.sm);
        container(
            column![
                row![
                    text("Fallback persona").size(ts.caption).color(theme.palette.text_muted),
                    if sentinel { badge(theme, "sentinel") } else { Space::new().into() },
                ]
                .spacing(sp.sm)
                .align_y(Alignment::Center),
                row![
                    outlined_field(theme, self.field_error("stage.fallback"), id_input.into()),
                    outlined_field(theme, self.field_error("stage.fallback"), label_input.into()),
                ]
                .spacing(sp.sm),
                outlined_field(theme, self.field_error("stage.fallback"), instructions.into(),),
                row![
                    text("Capabilities").size(ts.caption).color(theme.palette.text_muted),
                    outlined_field(
                        theme,
                        self.field_error("stage.fallback.capabilities"),
                        flags.into(),
                    ),
                ]
                .spacing(sp.sm)
                .align_y(Alignment::Center),
            ]
            .spacing(sp.sm),
        )
        .width(Length::Fill)
        .padding([sp.xs, sp.sm])
        .style(move |_t: &iced::Theme| iced::widget::container::Style {
            background: Some(iced::Background::Color(theme.palette.surface)),
            border: iced::Border { color: theme.palette.border, width: 1.0, radius: 8.0.into() },
            ..iced::widget::container::Style::default()
        })
        .into()
    }

    // ------------------------------------------------------------------
    // Relationship rows (P4 Batch 3, Slice 3b).
    //
    // A table-like surface below the stage-card stack (spec §3): one row per
    // `RelationshipDef` — [From stage pick] -- [kind pick] --> [To stage
    // pick] — with a row-level delete button and an "+ Add relationship"
    // button. The kind picker is the OPEN registry with CLOSED semantics
    // (ADR-58 §4): options are the distinct kind names currently registered
    // in `blueprint.relationships`, each carrying its closed semantics.
    // ------------------------------------------------------------------

    /// The blueprint-path relationship surface: header + one row per
    /// `RelationshipDef` + the add button. Defensive on a missing blueprint
    /// (the caller only renders it on the blueprint path).
    fn relationships_view<'a>(&'a self, theme: &'a AppTheme) -> Element<'a, Message> {
        let ts = &theme.type_scale;
        let sp = &theme.spacing;
        let Some(blueprint) = self.blueprint.as_ref() else {
            return Space::new().into();
        };
        // From/To pickers are restricted to the pipeline's stage tags — the
        // only valid endpoints for a relationship row.
        let stage_tags: Vec<String> =
            blueprint.pipeline.stages.iter().map(|stage| stage.tag.clone()).collect();
        // Deduplicate the open registry by kind name, preserving first-seen
        // order, so the picker offers each registered kind exactly once with
        // the closed semantics it references.
        let mut seen = HashSet::new();
        let kind_options: Vec<RelationshipKindOption> = blueprint
            .relationships
            .iter()
            .filter(|relationship| seen.insert(relationship.kind.clone()))
            .map(|relationship| RelationshipKindOption {
                kind: relationship.kind.clone(),
                semantics: relationship.semantics,
            })
            .collect();
        let mut rows = column![].spacing(sp.xs);
        for (index, relationship) in blueprint.relationships.iter().enumerate() {
            rows = rows.push(self.relationship_row(
                theme,
                relationship,
                index,
                &stage_tags,
                &kind_options,
            ));
        }
        if blueprint.relationships.is_empty() {
            // The empty-registry caption mirrors the resolution seam
            // (blueprint.rs:649-657): an empty registry falls back to the
            // engine's five standard rows at resolve time, so the caption
            // names those kinds rather than implying no hand-offs exist.
            rows = rows.push(
                text(
                    "Registry is empty — the engine falls back to the standard hand-offs \
                     (supervises, provides_context_to, owns_design). Add a row to override.",
                )
                .size(ts.body)
                .color(theme.palette.text_muted),
            );
        }
        column![
            row![
                text("Relationships").size(ts.title),
                Space::new().width(Length::Fill),
                button("+ Add relationship")
                    .style(crate::ui::button::secondary)
                    .on_press(Message::OrchestrationStudio(StudioMessage::RelationshipAdded)),
            ]
            .align_y(Alignment::Center),
            rows,
        ]
        .spacing(sp.sm)
        .into()
    }

    /// One relationship row: [From stage pick] -- [kind pick + closed-
    /// semantics tag] --> [To stage pick], plus the row-level × delete
    /// button (spec §3). Rulebook note: `validate_blueprint` emits no
    /// relationship field paths (the rulebook (a)–(j) covers stages,
    /// fallbacks, and the global cap only — blueprint.rs:436-618), so the
    /// `field_error`/`outlined_field` helpers are intentionally not applied
    /// here: there is no rulebook path for these fields to violate.
    fn relationship_row<'a>(
        &'a self,
        theme: &'a AppTheme,
        relationship: &'a RelationshipDef,
        index: usize,
        stage_tags: &[String],
        kind_options: &[RelationshipKindOption],
    ) -> Element<'a, Message> {
        let ts = &theme.type_scale;
        let sp = &theme.spacing;
        let selected_from = stage_tags.iter().find(|tag| *tag == &relationship.from).cloned();
        let selected_to = stage_tags.iter().find(|tag| *tag == &relationship.to).cloned();
        let selected_kind =
            kind_options.iter().find(|option| option.kind == relationship.kind).cloned();
        let from_pick = pick_list(stage_tags.to_vec(), selected_from, move |tag| {
            Message::OrchestrationStudio(StudioMessage::RelationshipFromChanged(index, tag))
        })
        .placeholder("from stage");
        let to_pick = pick_list(stage_tags.to_vec(), selected_to, move |tag| {
            Message::OrchestrationStudio(StudioMessage::RelationshipToChanged(index, tag))
        })
        .placeholder("to stage");
        let kind_pick = pick_list(kind_options.to_vec(), selected_kind, move |option| {
            Message::OrchestrationStudio(StudioMessage::RelationshipKindChanged(index, option.kind))
        })
        .placeholder("kind");
        // Distinct semantic affordance per kind (spec §3): glyph + text tag
        // in palette colors, never color alone.
        let semantics_tag: Element<'_, Message> = row![
            text(semantics_glyph(relationship.semantics)).size(ts.caption),
            text(semantics_label(relationship.semantics))
                .size(ts.caption)
                .color(theme.palette.text_muted),
        ]
        .spacing(sp.xs)
        .align_y(Alignment::Center)
        .into();
        row![
            column![text("From").size(ts.caption).color(theme.palette.text_muted), from_pick,]
                .spacing(sp.xs),
            text("--").color(theme.palette.text_muted),
            column![
                row![text("Kind").size(ts.caption).color(theme.palette.text_muted), semantics_tag,]
                    .spacing(sp.sm)
                    .align_y(Alignment::Center),
                kind_pick,
            ]
            .spacing(sp.xs),
            text("-->").color(theme.palette.text_muted),
            column![text("To").size(ts.caption).color(theme.palette.text_muted), to_pick,]
                .spacing(sp.xs),
            button("×")
                .style(button::text)
                .on_press(Message::OrchestrationStudio(StudioMessage::RelationshipDeleted(index),)),
        ]
        .spacing(sp.sm)
        .align_y(Alignment::Center)
        .into()
    }

    fn library_view<'a>(&'a self, theme: &'a AppTheme) -> Element<'a, Message> {
        let ts = &theme.type_scale;
        let sp = &theme.spacing;
        // Slice 3: on the blueprint path the library doubles as the one-surface
        // roster editor with Edit/Delete CRUD; on the legacy path it is the
        // byte-identical historical agent library.
        let blueprint_path = self.on_blueprint_path();
        let search = text_input("Search agents...", &self.search_query)
            .on_input(|s| Message::OrchestrationStudio(StudioMessage::SearchChanged(s)));

        let query = self.search_query.to_lowercase();
        let mut list = column![].spacing(sp.xs);
        for agent in &self.agents {
            if query.is_empty()
                || agent.name.to_lowercase().contains(&query)
                || agent.role.to_lowercase().contains(&query)
            {
                let model_hint =
                    agent.model_override.clone().unwrap_or_else(|| "default model".into());
                let selected = self.selected_agent_id.as_ref() == Some(&agent.id);
                let disabled_badge: Element<'_, Message> =
                    if agent.disabled { badge(theme, "Disabled") } else { Space::new().into() };
                let role_badge: Element<'_, Message> = if agent.role.is_empty() {
                    Space::new().into()
                } else {
                    badge(theme, &agent.role)
                };
                let select = button(
                    column![
                        row![text(&agent.name).size(ts.label), disabled_badge]
                            .spacing(sp.xs)
                            .align_y(Alignment::Center),
                        row![
                            role_badge,
                            text(model_hint).size(ts.caption).color(theme.palette.text_muted)
                        ]
                        .spacing(sp.xs)
                        .align_y(Alignment::Center),
                    ]
                    .spacing(sp.xs)
                    .width(Length::Fill),
                )
                .style(if selected { button::primary } else { button::text })
                .width(Length::Fill);
                // The coordinator is engine-owned (ADR-35 §5): on the roster
                // surface it is a locked row — visibly rendered with an
                // "Engine-owned" marker, but never selectable into the
                // inspector and with no Edit/Delete affordance. Guarding the
                // affordance here (not the row) keeps the sibling
                // `RemoveAgent`/`SelectAgent` message guards as the final
                // backstop for non-UI paths.
                let select = if agent.id == "coordinator" && blueprint_path {
                    select
                } else {
                    select.on_press(Message::OrchestrationStudio(StudioMessage::SelectAgent(Some(
                        agent.id.clone(),
                    ))))
                };
                // Slice 3 roster CRUD: on the blueprint path every agent is a
                // fully editable template except the engine-owned coordinator
                // (locked, see above). Edit routes to the per-agent inspector
                // (persisted through the roster Save arm); Delete removes the
                // agent from config on the next Save. The legacy path keeps
                // its historical "Remove custom agent only" affordance.
                let agent_row = if blueprint_path {
                    if agent.id == "coordinator" {
                        row![select, badge(theme, "Engine-owned")]
                            .spacing(sp.xs)
                            .align_y(Alignment::Center)
                    } else {
                        row![
                            select,
                            button("Edit").style(button::text).on_press(
                                Message::OrchestrationStudio(StudioMessage::SelectAgent(Some(
                                    agent.id.clone()
                                )),)
                            ),
                            button("Delete").style(button::text).on_press(
                                Message::OrchestrationStudio(StudioMessage::RemoveAgent(
                                    agent.id.clone()
                                ),)
                            ),
                        ]
                        .spacing(sp.xs)
                        .align_y(Alignment::Center)
                    }
                } else if agent.is_custom {
                    row![
                        select,
                        button("Remove").style(button::text).on_press(
                            Message::OrchestrationStudio(StudioMessage::RemoveAgent(
                                agent.id.clone(),
                            ))
                        ),
                    ]
                    .spacing(sp.xs)
                    .align_y(Alignment::Center)
                } else {
                    row![select].align_y(Alignment::Center)
                };
                list = list.push(agent_row).push(iced::widget::rule::horizontal(1));
            }
        }

        let add_button = if self.new_agent_name.trim().is_empty() {
            button("Add agent").style(button::secondary)
        } else {
            button("Add agent")
                .style(button::secondary)
                .on_press(Message::OrchestrationStudio(StudioMessage::AddAgent))
        };
        let new_agent: Element<'_, Message> = if self.show_add_agent_form {
            column![
                text("Add custom agent").size(ts.body).color(theme.palette.text_muted),
                text_input("Name", &self.new_agent_name)
                    .on_input(|s| Message::OrchestrationStudio(StudioMessage::NewAgentName(s))),
                text_input("Role", &self.new_agent_role)
                    .on_input(|s| Message::OrchestrationStudio(StudioMessage::NewAgentRole(s))),
                add_button,
            ]
            .spacing(sp.sm)
            .into()
        } else {
            Space::new().into()
        };
        let add_toggle_label =
            if self.show_add_agent_form { "− Hide form" } else { "+ Add agent" };

        column![
            text(format!("{} agents", self.agents.len()))
                .size(ts.caption)
                .color(theme.palette.text_muted),
            search,
            scrollable(list).height(Length::Fill),
            iced::widget::rule::horizontal(1),
            button(add_toggle_label)
                .style(button::secondary)
                .on_press(Message::OrchestrationStudio(StudioMessage::ToggleAddAgentForm)),
            new_agent,
        ]
        .spacing(sp.sm)
        .height(Length::Fill)
        .into()
    }

    fn pipeline_view<'a>(
        &'a self,
        theme: &'a AppTheme,
        report: &ValidationReport,
    ) -> Element<'a, Message> {
        let ts = &theme.type_scale;
        let sp = &theme.spacing;
        // The pipeline canvas: nodes are agents (click to inspect/scroll to
        // their hand-offs), edges are drawn but no longer interactive.
        let (graph_model, _edge_to_relationship) = self.pipeline_graph_model();
        let graph_colors = self.graph_colors(theme);
        let graph = agent_graph::view(graph_model.clone(), graph_colors, theme.palette.text).map(
            move |msg: GraphMessage| {
                let studio = match msg {
                    GraphMessage::NodeClicked(idx) => StudioMessage::GraphNodeClicked(idx),
                };
                Message::OrchestrationStudio(studio)
            },
        );
        // Size the canvas to the model's vertical extent so the whole chain is
        // visible on open (a fixed 340px clipped the standard 6-rank layout).
        let graph_height =
            graph_model.nodes.iter().map(|node| node.position.y + 60.0).fold(260.0, f32::max);
        // Clip the canvas to the card so panned/zoomed geometry never bleeds
        // over the Hand-offs / Run Limits panels below it (issue #136).
        let graph_canvas =
            container(graph).clip(true).width(Length::Fill).height(Length::Fixed(graph_height));

        let mut relationship_list = column![].spacing(sp.xs);
        for (i, rel) in self.relationships.iter().enumerate() {
            let cycles = rel
                .max_cycles
                .map(|value| format!("{value} cycle{}", if value == 1 { "" } else { "s" }))
                .unwrap_or_else(|| "unlimited cycles".into());
            relationship_list = relationship_list.push(
                row![
                    column![
                        text(format!(
                            "{} → {}",
                            self.agent_label(&rel.from),
                            self.agent_label(&rel.to)
                        ))
                        .size(ts.body),
                        text(format!("{} · {cycles}", rel.relationship))
                            .size(ts.caption)
                            .color(theme.palette.text_muted),
                    ]
                    .spacing(sp.xs)
                    .width(Length::Fill),
                    button("Edit").style(button::secondary).on_press(Message::OrchestrationStudio(
                        StudioMessage::SelectRelationship(Some(i))
                    )),
                    button("Delete").style(button::danger).on_press(Message::OrchestrationStudio(
                        StudioMessage::DeleteRelationship(i)
                    )),
                ]
                .spacing(sp.sm)
                .align_y(Alignment::Center),
            );
        }
        if self.relationships.is_empty() {
            relationship_list = relationship_list.push(
                text("No hand-offs configured yet.").size(ts.body).color(theme.palette.text_muted),
            );
        }

        // The rows scroll inside a bounded region (240px max) so long lists
        // don't push the add/edit form off-screen; a short list (or the empty
        // state) shrinks to its natural height and wheel over a non-overflowing
        // list passes through to the page scrollable.
        let relationship_list_scrollable = container(
            scrollable(relationship_list)
                .id(iced::widget::Id::new(STUDIO_HANDOFFS_LIST_ID))
                .height(Length::Shrink),
        )
        .max_height(240.0);

        let agent_options: Vec<AgentOption> = self
            .agents
            .iter()
            .map(|a| AgentOption { id: a.id.clone(), label: a.name.clone() })
            .collect();
        let selected_from = agent_options.iter().find(|o| o.id == self.new_rel_from).cloned();
        let selected_to = agent_options.iter().find(|o| o.id == self.new_rel_to).cloned();
        let rel_types: Vec<String> = vec![
            "supervises".into(),
            "provides_context_to".into(),
            "reports_to".into(),
            "owns_design".into(),
        ];
        // The add/edit form only renders while it is relevant: when an edge is
        // clicked (or "Add hand-off" is pressed). Browsing the pipeline and
        // editing a hand-off are now visually distinct states.
        let editor_visible = self.selected_relationship.is_some() || self.show_relationship_editor;
        // relationship_draft() clones the relationship list and runs a cycle
        // DFS — only pay for it while the editor is on screen.
        let draft_error = if editor_visible { self.relationship_draft().err() } else { None };
        let submit_label = if self.selected_relationship.is_some() {
            "Save relationship"
        } else {
            "Add relationship"
        };
        let submit = if draft_error.is_none() {
            button(submit_label)
                .style(button::primary)
                .on_press(Message::OrchestrationStudio(StudioMessage::CreateRelationship))
        } else {
            button(submit_label).style(button::primary)
        };
        let draft_note: Element<'_, Message> = match draft_error {
            Some(error) => text(error).size(ts.caption).color(theme.palette.text_muted).into(),
            None => text("This hand-off keeps the pipeline acyclic.")
                .size(ts.caption)
                .color(theme.palette.success)
                .into(),
        };
        let relationship_editor = column![
            text(if self.selected_relationship.is_some() {
                "Edit relationship"
            } else {
                "Add relationship"
            })
            .size(ts.label),
            row![
                pick_list(agent_options.clone(), selected_from, |option| {
                    Message::OrchestrationStudio(StudioMessage::NewRelFrom(option.id))
                })
                .placeholder("From agent"),
                pick_list(agent_options, selected_to, |option| {
                    Message::OrchestrationStudio(StudioMessage::NewRelTo(option.id))
                })
                .placeholder("To agent"),
                pick_list(
                    rel_types,
                    if self.new_rel_type.is_empty() {
                        None
                    } else {
                        Some(self.new_rel_type.clone())
                    },
                    |relationship| Message::OrchestrationStudio(StudioMessage::NewRelType(
                        relationship
                    )),
                )
                .placeholder("Relationship type"),
                text_input("Max cycles (optional)", &self.new_rel_max_cycles).on_input(|value| {
                    Message::OrchestrationStudio(StudioMessage::NewRelMaxCycles(value))
                }),
            ]
            .spacing(sp.sm),
            row![
                submit,
                if self.selected_relationship.is_some() {
                    button("Cancel").style(button::secondary).on_press(
                        Message::OrchestrationStudio(StudioMessage::SelectRelationship(None)),
                    )
                } else {
                    button("Clear").style(button::secondary).on_press(Message::OrchestrationStudio(
                        StudioMessage::SelectRelationship(None),
                    ))
                },
                draft_note,
            ]
            .spacing(sp.sm)
            .align_y(Alignment::Center),
        ]
        .spacing(sp.sm);

        let validation_details: Element<'_, Message> = if report.ok {
            text("Pipeline validation passed.").size(ts.body).color(theme.palette.success).into()
        } else {
            let mut issues = column![text("Fix these issues before saving:")
                .size(ts.body)
                .color(theme.palette.danger)]
            .spacing(sp.xs);
            for message in &report.messages {
                issues = issues.push(text(format!("• {message}")).size(ts.body));
            }
            issues.into()
        };
        let total_tokens: u64 =
            self.agents.iter().map(|agent| self.token_estimate_for(agent)).sum();
        // "Reset to standard" is intentionally absent: the toolbar's
        // "Load preset…" pick-list is the single entry point and already lists
        // "Standard Pipeline" first, so resetting stays one click away.
        let pipeline_card = section_card_with_subtitle(
            theme,
            "Pipeline",
            format!(
                "{} agents · {} relationships · ~{} prompt tokens",
                self.agents.len(),
                self.relationships.len(),
                total_tokens
            ),
            graph_canvas,
        );
        // The editor block renders only while relevant (see editor_visible
        // above); otherwise the space is reserved so the layout does not jump.
        let relationship_editor_block: Element<'_, Message> =
            if editor_visible { relationship_editor.into() } else { Space::new().into() };
        let handoffs_card = section_card(
            theme,
            "Hand-offs",
            column![
                row![
                    text(format!("{} configured", self.relationships.len()))
                        .size(ts.caption)
                        .color(theme.palette.text_muted),
                    Space::new().width(Length::Fill),
                    button("+ Add hand-off").style(button::secondary).on_press(
                        Message::OrchestrationStudio(StudioMessage::ToggleRelationshipEditor(true))
                    ),
                ]
                .align_y(Alignment::Center),
                relationship_list_scrollable,
                relationship_editor_block,
            ]
            .spacing(sp.sm),
        );
        // Blueprint users never see the legacy run-limit drafts (oracle
        // finding, Slice 2): `[orchestration]` configs are governed by the
        // blueprint model (per-stage `max_cycles`, rule (f) bound) instead of
        // the legacy `multi_agent` tuning, and `validation()` skips the
        // drafts' checks on the blueprint path — rendering editable drafts
        // whose checks are skipped would be a dead/lying surface. The card
        // is the drafts' only renderer, so gating it here makes the drafts
        // unreachable on the blueprint path; the legacy path renders
        // unchanged.
        let run_limits_block: Element<'_, Message> =
            self.run_limits_card(theme).unwrap_or_else(|| Space::new().into());
        let validation_card = section_card(theme, "Validation", validation_details);
        column![
            pipeline_card,
            handoffs_card,
            row![run_limits_block, validation_card].spacing(sp.md),
        ]
        .spacing(sp.md)
        .padding([sp.xs, 0.0])
        .into()
    }

    /// The legacy Run Limits card (concurrency + spend-cap drafts), or `None`
    /// on the blueprint path — see the oracle-finding note at the call site.
    /// `None` keeps the legacy surface byte-identical: the card only exists
    /// for `multi_agent` configs without `[orchestration]`.
    fn run_limits_card<'a>(&'a self, theme: &'a AppTheme) -> Option<Element<'a, Message>> {
        if self.orchestration.is_some() {
            return None;
        }
        let ts = &theme.type_scale;
        let sp = &theme.spacing;
        // Inline captions under each run-limit input mirror validation()'s
        // checks. Always present (a zero-height space when valid) so the row
        // heights stay stable whether or not the caption shows.
        let agents_caption: Element<'_, Message> = if self
            .run_agents_draft
            .parse::<usize>()
            .map_or(true, |v| v < 1)
        {
            text("must be a whole number ≥ 1").size(ts.caption).color(theme.palette.danger).into()
        } else {
            Space::new().into()
        };
        let provider_caption: Element<'_, Message> = if self
            .run_provider_draft
            .parse::<usize>()
            .map_or(true, |v| v < 1)
        {
            text("must be a whole number ≥ 1").size(ts.caption).color(theme.palette.danger).into()
        } else {
            Space::new().into()
        };
        let spend_caption: Element<'_, Message> = if self
            .run_spend_draft
            .parse::<f64>()
            .map_or(true, |v| v <= 0.0)
        {
            text("must be a positive number").size(ts.caption).color(theme.palette.danger).into()
        } else {
            Space::new().into()
        };
        let card = section_card_with_subtitle(
            theme,
            "Run Limits",
            "Concurrency and shared spend budget",
            column![
                row![
                    column![
                        text("Max concurrent agents").size(ts.caption),
                        text_input("3", &self.run_agents_draft).on_input(|value| {
                            Message::OrchestrationStudio(StudioMessage::RunAgentsChanged(value))
                        }),
                        agents_caption,
                    ]
                    .spacing(sp.xs),
                    column![
                        text("Max concurrent per provider").size(ts.caption),
                        text_input("2", &self.run_provider_draft).on_input(|value| {
                            Message::OrchestrationStudio(StudioMessage::RunProviderChanged(value))
                        }),
                        provider_caption,
                    ]
                    .spacing(sp.xs),
                    column![
                        text("Spend cap multiplier").size(ts.caption),
                        text_input("3.0", &self.run_spend_draft).on_input(|value| {
                            Message::OrchestrationStudio(StudioMessage::RunSpendChanged(value))
                        }),
                        spend_caption,
                    ]
                    .spacing(sp.xs),
                ]
                .spacing(sp.sm),
                text(
                    "Controls how many agents run at once and how tall the shared spend budget \
                     is. Multi-agent defaults: 3 agents, 2 per provider, budget ×3.",
                )
                .size(ts.caption)
                .color(theme.palette.text_muted),
            ]
            .spacing(sp.sm),
        );
        Some(card)
    }

    fn inspector_view<'a>(&'a self, theme: &'a AppTheme) -> Element<'a, Message> {
        let ts = &theme.type_scale;
        let sp = &theme.spacing;
        if let Some(id) = &self.selected_agent_id {
            if let Some(agent) = self.agents.iter().find(|a| &a.id == id) {
                let tabs = row![
                    inspector_tab(
                        self.inspector_section == InspectorSection::Prompt,
                        "Prompt",
                        InspectorSection::Prompt,
                    ),
                    inspector_tab(
                        self.inspector_section == InspectorSection::Model,
                        "Model",
                        InspectorSection::Model,
                    ),
                    inspector_tab(
                        self.inspector_section == InspectorSection::Permissions,
                        "Permissions",
                        InspectorSection::Permissions,
                    ),
                ]
                .spacing(sp.sm);

                let body = match self.inspector_section {
                    InspectorSection::Prompt => self.prompt_pane(agent, theme),
                    InspectorSection::Model => self.model_pane(agent, theme),
                    InspectorSection::Permissions => self.permissions_pane(agent, theme),
                };
                return column![
                    row![
                        button("← Pipeline")
                            .style(button::secondary)
                            .on_press(Message::OrchestrationStudio(StudioMessage::ShowPipeline)),
                        column![
                            row![
                                text(&agent.name).size(ts.display),
                                if agent.id == "coordinator" {
                                    badge(theme, "protected")
                                } else {
                                    Space::new().into()
                                },
                            ]
                            .spacing(sp.xs)
                            .align_y(Alignment::Center),
                            text(format!("{} · {}", agent.role, caps_summary(&agent.capabilities)))
                                .size(ts.body)
                                .color(theme.palette.text_muted),
                        ]
                        .spacing(sp.xs),
                    ]
                    .spacing(sp.md)
                    .align_y(Alignment::Center),
                    tabs,
                    iced::widget::rule::horizontal(1),
                    body,
                ]
                .spacing(sp.md)
                .padding([sp.xs, 0.0])
                .into();
            }
        }
        self.pipeline_view(theme, &self.validation())
    }

    fn prompt_pane(&self, agent: &AgentConfig, theme: &AppTheme) -> Element<'_, Message> {
        let ts = &theme.type_scale;
        let sp = &theme.spacing;
        let p = &agent.prompt_sections;

        // "Submission contract" (typed output mode) picker.
        let mode_options: Vec<ModeOption> = vec![
            ModeOption { value: OutputMode::Freeform, label: "Freeform" },
            ModeOption { value: OutputMode::DesignDoc, label: "Design Doc" },
            ModeOption { value: OutputMode::ResearchReport, label: "Research Report" },
            ModeOption { value: OutputMode::ReviewReport, label: "Review Report" },
        ];
        let mode_selected = mode_options.iter().find(|o| o.value == agent.output_mode).cloned();
        let mode_pick = pick_list(mode_options, mode_selected, |m| {
            Message::OrchestrationStudio(StudioMessage::OutputModeChanged(m.value))
        })
        .width(Length::Fill);

        // "Lifecycle" (pipeline stage) picker. Only known stages are mapped;
        // anything else (e.g. a stage the studio doesn't know) renders as
        // Freeform WITHOUT side effects — selection only mutates on a user pick.
        let stage_options: Vec<StageOption> = vec![
            StageOption { value: None, label: "Freeform (no lifecycle)" },
            StageOption { value: Some(AgentStage::new(AgentStage::DESIGN)), label: "Design" },
            StageOption { value: Some(AgentStage::new(AgentStage::RESEARCH)), label: "Research" },
            StageOption { value: Some(AgentStage::new(AgentStage::IMPLEMENT)), label: "Implement" },
            StageOption { value: Some(AgentStage::new(AgentStage::REVIEW)), label: "Review" },
            StageOption { value: Some(AgentStage::new(AgentStage::VALIDATE)), label: "Validate" },
        ];
        let stage_current = agent.stage.as_ref().filter(|s| s.is_known()).cloned();
        let stage_selected = stage_options.iter().find(|o| o.value == stage_current).cloned();
        let stage_pick = pick_list(stage_options, stage_selected, |s| {
            Message::OrchestrationStudio(StudioMessage::StageChanged(s.value))
        })
        .width(Length::Fill);

        let sys = text_input("System Instructions", &p.system_instructions)
            .on_input(|s| Message::OrchestrationStudio(StudioMessage::SysPromptChanged(s)))
            .width(Length::Fill);
        let cons = text_input("Constraints & Safety", &p.constraints)
            .on_input(|s| Message::OrchestrationStudio(StudioMessage::ConstraintsChanged(s)))
            .width(Length::Fill);
        let out = text_input("Output Format", &p.output_format)
            .on_input(|s| Message::OrchestrationStudio(StudioMessage::OutputFormatChanged(s)))
            .width(Length::Fill);

        let mut few = column![text("Few-Shot Examples").size(ts.body)];
        for (i, ex) in p.few_shot.iter().enumerate() {
            few = few.push(
                row![
                    text_input("input", &ex.input).on_input(move |s| {
                        Message::OrchestrationStudio(StudioMessage::FewShotInputChanged {
                            idx: i,
                            value: s,
                        })
                    }),
                    text_input("output", &ex.output).on_input(move |s| {
                        Message::OrchestrationStudio(StudioMessage::FewShotOutputChanged {
                            idx: i,
                            value: s,
                        })
                    }),
                    button("Remove")
                        .style(crate::ui::button::danger)
                        .on_press(Message::OrchestrationStudio(StudioMessage::RemoveFewShot(i))),
                ]
                .spacing(sp.xs),
            );
        }
        let add_ex =
            button("Add Example").on_press(Message::OrchestrationStudio(StudioMessage::AddFewShot));

        let tokens = self.token_estimate_for(agent);
        let contract_row = row![
            column![text("Submission contract").size(ts.label), mode_pick,]
                .spacing(sp.xs)
                .width(Length::Fill),
            column![text("Lifecycle").size(ts.label), stage_pick,]
                .spacing(sp.xs)
                .width(Length::Fill),
        ]
        .spacing(sp.sm);
        column![
            contract_row,
            text(
                "Typed contracts travel via submit_design_doc / submit_research_report / \
                 submit_review_report and are validated field-by-field."
            )
            .size(ts.caption)
            .color(theme.palette.text_muted),
            text(
                "Stage ties an agent into the Coordinator's lifecycle (Review / Validate \
                 cycles) and defaults to Freeform for custom agents."
            )
            .size(ts.caption)
            .color(theme.palette.text_muted),
            sys,
            cons,
            out,
            few,
            add_ex,
            text(format!("~{} tokens (est.)", tokens)).size(ts.body),
        ]
        .spacing(sp.sm)
        .into()
    }

    fn model_pane(&self, agent: &AgentConfig, theme: &AppTheme) -> Element<'_, Message> {
        let ts = &theme.type_scale;
        let sp = &theme.spacing;
        let agent_id = agent.id.clone();

        // Build unified model options: "model — provider" with a "global default"
        // entry at the top.
        let mut options: Vec<ModelOption> = Vec::new();
        options.push(ModelOption { key: String::new(), label: "Use global default".into() });

        let mut pairs: Vec<(String, String)> = Vec::new();
        for (provider_id, models) in &self.cached_models_by_provider {
            for model in models {
                pairs.push((provider_id.clone(), model.clone()));
            }
        }
        pairs.sort_by(|a, b| a.1.cmp(&b.1).then(a.0.cmp(&b.0)));

        for (provider_id, model) in &pairs {
            let label = format!("{} — {}", model, provider_id);
            let key = format!("{}|{}", provider_id, model);
            options.push(ModelOption { key, label });
        }

        // Determine currently-selected option.
        let current_key = agent
            .provider_id
            .as_ref()
            .and_then(|pid| agent.model_override.as_ref().map(|m| format!("{}|{}", pid, m)))
            .unwrap_or_default();
        let selected = options.iter().find(|o| o.key == current_key).cloned();

        let model_pick = pick_list(options, selected, {
            let agent_id = agent_id.clone();
            move |option: ModelOption| {
                let (provider_id, model) = if option.key.is_empty() {
                    (String::new(), "default".into())
                } else {
                    let parts: Vec<&str> = option.key.splitn(2, '|').collect();
                    (parts[0].to_string(), parts[1].to_string())
                };
                Message::OrchestrationStudio(StudioMessage::AssignModel {
                    agent_id: agent_id.clone(),
                    provider_id,
                    model,
                })
            }
        });

        let name =
            self.global_default_model.as_deref().map(|m| format!(": {m}")).unwrap_or_default();
        let help_text =
            format!("Using the global default model{name} — set it in Settings › Model.");
        let help = if agent.provider_id.is_some() {
            text("A specific model is selected for this agent. Choose 'Use global default' to use the default model.")
        } else {
            text(help_text)
        };
        let open_settings = button("Open Model Settings")
            .style(crate::ui::button::secondary)
            .on_press(Message::Navigate(crate::app::Page::Settings));

        column![
            text("Model Selection").size(ts.label),
            model_pick,
            row![help.size(ts.body), open_settings].spacing(sp.sm).align_y(Alignment::Center),
        ]
        .spacing(sp.sm)
        .into()
    }

    fn permissions_pane(&self, agent: &AgentConfig, theme: &AppTheme) -> Element<'_, Message> {
        let ts = &theme.type_scale;
        let sp = &theme.spacing;
        let id = agent.id.clone();
        let c = agent.capabilities.effective();
        let fs_read = checkbox(c.fs_read)
            .label("FS Read")
            .on_toggle(cap_toggle(id.clone(), Capability::FsRead));
        let fs_write = checkbox(c.fs_write)
            .label("FS Write")
            .on_toggle(cap_toggle(id.clone(), Capability::FsWrite));
        let shell =
            checkbox(c.shell).label("Shell").on_toggle(cap_toggle(id.clone(), Capability::Shell));
        let git = checkbox(c.git).label("Git").on_toggle(cap_toggle(id.clone(), Capability::Git));
        let lsp = checkbox(c.lsp).label("LSP").on_toggle(cap_toggle(id.clone(), Capability::Lsp));
        let eval = checkbox(c.eval)
            .label("Eval Engine")
            .on_toggle(cap_toggle(id.clone(), Capability::Eval));
        let disabled_id = id.clone();
        let disabled: Element<'_, Message> = if agent.id == "coordinator" {
            // The coordinator is code-constructed and always active; its
            // "disabled" affordance would be dishonest, so show a caption.
            text("Coordinator is always active (code-constructed) — it cannot be disabled here.")
                .size(ts.caption)
                .color(theme.palette.text_muted)
                .into()
        } else {
            checkbox(agent.disabled)
                .label("Disabled (not registered)")
                .on_toggle(move |value| {
                    Message::OrchestrationStudio(StudioMessage::DisabledToggled {
                        agent: disabled_id.clone(),
                        value,
                    })
                })
                .into()
        };
        let preset_ro = button("Read-Only Researcher")
            .style(crate::ui::button::secondary)
            .on_press(Message::OrchestrationStudio(StudioMessage::CapabilityPreset {
                agent: id.clone(),
                preset: PresetName::ReadOnlyResearcher,
            }));
        let preset_fc = button("Full Coder").style(crate::ui::button::secondary).on_press(
            Message::OrchestrationStudio(StudioMessage::CapabilityPreset {
                agent: id.clone(),
                preset: PresetName::FullCoder,
            }),
        );
        // Reflect the current capability combination so the preset buttons read
        // as a shortcut into the checkboxes, not as an opaque separate control.
        let current_preset = if c.fs_read && !c.fs_write && !c.shell && !c.git && !c.lsp && !c.eval
        {
            "Read-Only Researcher"
        } else if c.fs_read && c.fs_write && c.shell && c.git && c.lsp && !c.eval {
            "Full Coder"
        } else {
            "Custom"
        };

        column![
            text("Permissions / Capabilities").size(ts.label),
            fs_read,
            fs_write,
            shell,
            git,
            lsp,
            eval,
            text(
                "Enables the acceptance gate (C-06): a multi-agent Build fails with \
                 'validation disabled' when the Validator has this off.",
            )
            .size(ts.caption)
            .color(theme.palette.text_muted),
            disabled,
            row![preset_ro, preset_fc].spacing(sp.sm),
            text(format!("Current preset: {current_preset}"))
                .size(ts.caption)
                .color(theme.palette.text_muted),
        ]
        .spacing(sp.sm)
        .into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load_standard(state: &mut State) {
        let _ = state.update(StudioMessage::LoadPreset("Standard Pipeline".into()));
    }

    #[test]
    fn standard_pipeline_is_acyclic_and_valid() {
        let mut state = State::new();
        load_standard(&mut state);

        assert!(state.validation().ok);
        assert_eq!(state.relationships.len(), 5);
    }

    #[test]
    fn coordinator_cannot_be_removed_even_directly() {
        let mut state = State::new();
        let _ = state.update(StudioMessage::RemoveAgent("coordinator".into()));

        assert!(state.agents.iter().any(|a| a.id == "coordinator"));
        assert!(!state.unsaved, "blocked removal must not mark the studio dirty");
    }

    #[test]
    fn relationship_editor_opens_and_closes_via_toggle() {
        let mut state = State::new();
        assert!(!state.show_relationship_editor);

        let _ = state.update(StudioMessage::ToggleRelationshipEditor(true));
        assert!(state.show_relationship_editor);

        let _ = state.update(StudioMessage::SelectRelationship(None));
        assert!(!state.show_relationship_editor);

        // Selecting an agent closes the hand-off editor.
        let _ = state.update(StudioMessage::ToggleRelationshipEditor(true));
        let _ = state.update(StudioMessage::SelectAgent(Some("coder".into())));
        assert!(!state.show_relationship_editor);
        assert!(state.selected_agent_id.is_some());
    }

    #[test]
    fn pipeline_graph_model_mirrors_agents_and_relationships() {
        let mut state = State::new();
        load_standard(&mut state);

        let (model, edge_to_relationship) = state.pipeline_graph_model();
        assert_eq!(model.nodes.len(), state.agents.len());
        assert_eq!(model.edges.len(), state.relationships.len());
        // With no dangling relationships the map is the identity.
        assert_eq!(edge_to_relationship, (0..state.relationships.len()).collect::<Vec<_>>());
        // Node ids follow agent order: the coordinator seed is index 0.
        assert_eq!(model.nodes[0].label, "Coordinator");
        // Edge ids follow relationship order, with type + cycle labels.
        assert!(model.edges.iter().any(|e| e.label.as_deref() == Some("supervises · 3")));
        assert!(model.edges.iter().any(|e| e.label.as_deref() == Some("provides_context_to · 3")));
    }

    #[test]
    fn graph_model_is_cached_and_invalidated_on_mutation() {
        let mut state = State::new();
        load_standard(&mut state);

        // Repeated calls return the same (cached) result: same node/edge counts
        // and the same edge-to-relationship map.
        let first = state.pipeline_graph_model();
        let second = state.pipeline_graph_model();
        assert_eq!(first.0.nodes.len(), second.0.nodes.len());
        assert_eq!(first.0.edges.len(), second.0.edges.len());
        assert_eq!(first.1, second.1);
        assert_eq!(first.0.edges.len(), state.relationships.len());

        // Mutate relationships through a real handler (NewPipeline clears them
        // and marks the studio dirty, which invalidates the graph cache).
        let _ = state.update(StudioMessage::NewPipeline);
        assert!(state.relationships.is_empty());

        // The rebuilt model reflects the mutation, and a follow-up call returns
        // the same cached model as a fresh build.
        let third = state.pipeline_graph_model();
        assert!(third.0.edges.is_empty(), "new model must reflect cleared relationships");
        let fresh = state.pipeline_graph_model();
        assert_eq!(third.0.nodes.len(), fresh.0.nodes.len());
        assert_eq!(third.0.edges.len(), fresh.0.edges.len());
        assert_eq!(third.1, fresh.1);
    }

    #[test]
    fn graph_skips_dangling_relationships_but_maps_edges_to_real_rows() {
        let mut state = State::new();
        load_standard(&mut state);
        // A relationship referencing an unknown agent: validation() flags it,
        // and the canvas must skip the edge while still letting every drawn
        // edge resolve back to the correct relationship row.
        state.relationships.insert(
            2,
            AgentRelationshipConfig {
                from: "ghost_agent".into(),
                to: "coder".into(),
                relationship: "reports_to".into(),
                max_cycles: None,
            },
        );

        let (model, edge_to_relationship) = state.pipeline_graph_model();
        assert_eq!(model.edges.len(), state.relationships.len() - 1);
        // The map is not the identity: the dangling row (2) is skipped, so
        // the edge at model index 2 points at relationship row 3.
        assert_eq!(edge_to_relationship, vec![0, 1, 3, 4, 5]);
        // Every model edge id resolves to an in-range relationship row.
        for &relationship_index in &edge_to_relationship {
            assert!(relationship_index < state.relationships.len());
        }
    }

    #[test]
    fn removing_an_agent_clears_a_stale_relationship_selection() {
        let mut state = State::new();
        load_standard(&mut state);
        // Select the last relationship (reviewer → validator)...
        let _ = state.update(StudioMessage::SelectRelationship(Some(4)));
        assert_eq!(state.selected_relationship, Some(4));
        // ...then remove "validator", which prunes that very relationship and
        // shifts the index space below it.
        let _ = state.update(StudioMessage::RemoveAgent("validator".into()));

        assert_eq!(state.selected_relationship, None);
        assert!(!state.show_relationship_editor);
        assert!(state.new_rel_from.is_empty());
        // The row that shifted into index 4 would have been "coder → reviewer";
        // a stale selection would silently edit the wrong row (or no-op).
        assert!(!state.relationships.iter().any(|r| r.to == "validator"));
    }

    #[test]
    fn load_from_config_clears_stale_relationship_selection() {
        let mut state = State::new();
        load_standard(&mut state);
        let _ = state.update(StudioMessage::SelectRelationship(Some(1)));
        assert_eq!(state.selected_relationship, Some(1));
        assert!(state.show_relationship_editor);

        // Reload with a config whose relationship list differs; the
        // index-based selection must not survive into the new list.
        let config = AppConfig {
            multi_agent: Some(concerto_config::MultiAgentConfig {
                relationships: vec![AgentRelationshipConfig {
                    from: "coordinator".into(),
                    to: "architect".into(),
                    relationship: "supervises".into(),
                    max_cycles: Some(2),
                }],
                ..Default::default()
            }),
            ..Default::default()
        };
        state.load_from_config(&config);

        assert_eq!(state.selected_relationship, None);
        assert!(!state.show_relationship_editor);
        assert!(state.new_rel_from.is_empty());
        assert_eq!(state.relationships.len(), 1);
    }

    #[test]
    fn load_from_config_keeps_deleted_seed_agents_deleted() {
        // Maintainer revision of ADR-58/59: once the config owns the roster
        // ([orchestration] present), the config's agent list IS the roster.
        // A seed deleted in the Studio (here: reviewer) must NOT reappear in
        // the surface after a reload.
        let mut state = State::new();
        let config = AppConfig {
            orchestration: Some(concerto_config::OrchestrationConfig::default()),
            multi_agent: Some(concerto_config::MultiAgentConfig {
                custom_agents: vec![
                    concerto_config::CustomAgentConfig {
                        id: "architect".into(),
                        ..Default::default()
                    },
                    concerto_config::CustomAgentConfig {
                        id: "researcher".into(),
                        ..Default::default()
                    },
                    concerto_config::CustomAgentConfig { id: "coder".into(), ..Default::default() },
                    concerto_config::CustomAgentConfig {
                        id: "validator".into(),
                        ..Default::default()
                    },
                ],
                ..Default::default()
            }),
            ..Default::default()
        };
        state.load_from_config(&config);

        let ids: Vec<String> = state.agents.iter().map(|a| a.id.clone()).collect();
        assert_eq!(ids, vec!["architect", "researcher", "coder", "validator"]);
        assert!(
            !ids.iter().any(|id| id == "reviewer"),
            "deleted seed 'reviewer' must not be resurrected by load_from_config"
        );
        assert_eq!(state.agents.len(), 4);
    }

    #[test]
    fn select_relationship_out_of_range_is_treated_as_none() {
        let mut state = State::new();
        load_standard(&mut state);

        // A stale-frame edge click can carry an index past the (shrunken)
        // list; the handler must not open an editor aimed at a phantom row.
        let _ = state.update(StudioMessage::SelectRelationship(Some(99)));

        assert_eq!(state.selected_relationship, None);
        assert!(!state.show_relationship_editor);
    }

    #[test]
    fn coordinator_gets_the_primary_hue_in_graph_colors() {
        let mut state = State::new();
        load_standard(&mut state);
        let theme = AppTheme::by_name("Midnight");
        let colors = state.graph_colors(&theme);

        let coordinator = colors.get(&AgentId::new("coordinator")).copied().unwrap();
        assert_eq!(coordinator, theme.palette.primary);
    }

    #[test]
    fn disabled_agent_is_dimmed_in_graph_colors() {
        let mut state = State::new();
        let _ =
            state.update(StudioMessage::DisabledToggled { agent: "validator".into(), value: true });
        let theme = AppTheme::by_name("Midnight");
        let colors = state.graph_colors(&theme);
        let validator = colors.get(&AgentId::new("validator")).copied().unwrap();
        assert_eq!(validator, theme.palette.surface_variant);
    }

    #[test]
    fn disabled_and_eval_flags_round_trip_through_persisted_parts() {
        let mut state = State::new();
        load_standard(&mut state);
        let target = "validator".to_string();

        let _ = state.update(StudioMessage::DisabledToggled { agent: target.clone(), value: true });
        let _ = state.update(StudioMessage::CapabilityToggled {
            agent: target.clone(),
            cap: Capability::Eval,
            value: false,
        });

        let (custom, _, _) = state.persisted_parts();
        let validator = custom
            .iter()
            .find(|a| a.id == target || a.role == target)
            .expect("validator should be persisted");
        assert!(validator.disabled);
        assert_eq!(validator.capabilities.eval, Some(false));
        assert!(state.unsaved);
    }

    #[test]
    fn relationship_editor_rejects_a_cycle_without_mutating_the_pipeline() {
        let mut state = State::new();
        load_standard(&mut state);
        let original_len = state.relationships.len();
        let _ = state.update(StudioMessage::NewRelFrom("validator".into()));
        let _ = state.update(StudioMessage::NewRelTo("coordinator".into()));
        let _ = state.update(StudioMessage::NewRelType("reports_to".into()));

        assert!(matches!(
            state.relationship_draft(),
            Err(error) if error == "This relationship would create a dependency cycle"
        ));
        let _ = state.update(StudioMessage::CreateRelationship);
        assert_eq!(state.relationships.len(), original_len);
    }

    #[test]
    fn relationship_edit_updates_in_place_and_formats_cycles_as_data() {
        let mut state = State::new();
        load_standard(&mut state);
        let original_len = state.relationships.len();
        let _ = state.update(StudioMessage::SelectRelationship(Some(0)));
        let _ = state.update(StudioMessage::NewRelMaxCycles("1".into()));
        let _ = state.update(StudioMessage::CreateRelationship);

        assert_eq!(state.relationships.len(), original_len);
        assert_eq!(state.relationships[0].max_cycles, Some(1));
        assert_eq!(state.selected_relationship, None);
    }

    #[test]
    fn prompt_edits_stay_dirty_until_the_explicit_save_succeeds() {
        let mut state = State::new();
        let _ = state.update(StudioMessage::SelectAgent(Some("coordinator".into())));
        let _ = state.update(StudioMessage::SysPromptChanged("Updated prompt".into()));

        assert!(state.unsaved);
        assert!(!state.saved_notice);
        state.mark_saved();
        assert!(!state.unsaved);
        assert!(state.saved_notice);
    }

    #[test]
    fn selecting_global_provider_removes_the_explicit_assignment() {
        let mut state = State::new();
        let _ = state.update(StudioMessage::AssignModel {
            agent_id: "coder".into(),
            provider_id: "nim".into(),
            model: "coder-model".into(),
        });
        let _ = state.update(StudioMessage::AssignModel {
            agent_id: "coder".into(),
            provider_id: String::new(),
            model: "default".into(),
        });

        let Some(coder) = state.agents.iter().find(|agent| agent.id == "coder") else {
            panic!("built-in coder is missing");
        };
        assert_eq!(coder.provider_id, None);
        assert_eq!(coder.model_override, None);
        assert!(state.model_assignments.iter().all(|assignment| assignment.agent_role != "coder"));
    }

    #[test]
    fn persisted_model_assignment_is_reflected_by_the_agent_inspector() {
        let config = AppConfig {
            model_settings: Some(concerto_config::ModelSettings {
                agent_assignments: vec![AgentModelAssignment {
                    agent_role: "coder".into(),
                    provider_config_id: "nim".into(),
                    model_override: Some("coder-model".into()),
                }],
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut state = State::new();
        state.load_from_config(&config);

        let Some(coder) = state.agents.iter().find(|agent| agent.id == "coder") else {
            panic!("built-in coder is missing");
        };
        assert_eq!(coder.provider_id.as_deref(), Some("nim"));
        assert_eq!(coder.model_override.as_deref(), Some("coder-model"));
    }

    #[test]
    fn load_from_config_backfills_assignments_from_custom_agent_pins() {
        // Config pins models per agent inside `multi_agent.custom_agents` but
        // has NO `model_settings.agent_assignments`: `load_from_config` must
        // backfill assignments so the Model tab reflects the pins and Save
        // persists them into `agent_assignments`.
        let config = AppConfig {
            multi_agent: Some(concerto_config::MultiAgentConfig {
                custom_agents: vec![
                    CustomAgentConfig {
                        id: "coder".into(),
                        name: "Coder".into(),
                        role: "coder".into(),
                        provider_id: Some("nim".into()),
                        model_override: Some("coder-model".into()),
                        ..Default::default()
                    },
                    // Model pinned but no provider: nothing to backfill.
                    CustomAgentConfig {
                        id: "docs-writer".into(),
                        name: "Docs Writer".into(),
                        role: "docs-writer".into(),
                        model_override: Some("docs-model".into()),
                        ..Default::default()
                    },
                ],
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut state = State::new();
        state.load_from_config(&config);

        assert!(!state.unsaved, "backfill mirrors config state; the studio stays clean");

        let coder = state
            .model_assignments
            .iter()
            .find(|assignment| assignment.agent_role == "coder")
            .expect("coder assignment backfilled from custom_agents pin");
        assert_eq!(coder.provider_config_id, "nim");
        assert_eq!(coder.model_override.as_deref(), Some("coder-model"));

        assert!(
            state.model_assignments.iter().all(|assignment| assignment.agent_role != "docs-writer"),
            "agents without a provider pin are not backfilled"
        );
    }

    #[test]
    fn load_from_config_does_not_duplicate_existing_assignments() {
        // A persisted `agent_assignments` entry for an agent that also pins a
        // model in `custom_agents` must not be duplicated by the backfill.
        let config = AppConfig {
            multi_agent: Some(concerto_config::MultiAgentConfig {
                custom_agents: vec![CustomAgentConfig {
                    id: "coder".into(),
                    name: "Coder".into(),
                    role: "coder".into(),
                    provider_id: Some("nim".into()),
                    model_override: Some("coder-model".into()),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            model_settings: Some(concerto_config::ModelSettings {
                agent_assignments: vec![AgentModelAssignment {
                    agent_role: "coder".into(),
                    provider_config_id: "openai".into(),
                    model_override: Some("gpt-4".into()),
                }],
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut state = State::new();
        state.load_from_config(&config);

        let matches = state.model_assignments.iter().filter(|a| a.agent_role == "coder").count();
        assert_eq!(matches, 1, "pre-existing assignment is not duplicated by the backfill");
    }

    #[test]
    fn compact_studio_view_renders_without_a_selected_agent() {
        let state = State::new();
        let theme = AppTheme::by_name("Midnight");
        let _ = state.view(&theme);
    }

    fn set_custom_agent(state: &mut State) -> String {
        state.new_agent_name = "Custom Agent".into();
        state.new_agent_role = "custom".into();
        let _ = state.update(StudioMessage::AddAgent);
        state.selected_agent_id.clone().expect("AddAgent should select the new agent")
    }

    #[test]
    fn builtin_output_contracts_survive_persisted_parts() {
        let state = State::new();
        let (custom, _, _) = state.persisted_parts();
        let by_id = |id: &str| {
            custom
                .iter()
                .find(|a| a.id == id)
                .unwrap_or_else(|| panic!("built-in {} missing", id))
                .output_mode
        };
        assert_eq!(by_id("architect"), OutputMode::DesignDoc);
        assert_eq!(by_id("researcher"), OutputMode::ResearchReport);
        assert_eq!(by_id("reviewer"), OutputMode::ReviewReport);
        assert_eq!(by_id("coder"), OutputMode::Freeform);
        assert_eq!(by_id("validator"), OutputMode::Freeform);
    }

    #[test]
    fn output_mode_edit_marks_dirty_and_round_trips() {
        let mut state = State::new();
        state.new_agent_name = "Coder Clone".into();
        state.new_agent_role = "coder".into();
        let _ = state.update(StudioMessage::AddAgent);
        let id = state.selected_agent_id.clone().expect("selected after add");
        let _ = state.update(StudioMessage::OutputModeChanged(OutputMode::ReviewReport));

        let agent = state.agents.iter().find(|a| a.id == id).expect("agent should exist");
        assert_eq!(agent.output_mode, OutputMode::ReviewReport);
        assert!(state.unsaved);

        let (custom, _, _) = state.persisted_parts();
        let saved = custom.iter().find(|a| a.id == id).expect("saved agent should exist");
        assert_eq!(saved.output_mode, OutputMode::ReviewReport);
    }

    #[test]
    fn stage_edit_marks_dirty_and_persists() {
        let mut state = State::new();
        let id = set_custom_agent(&mut state);
        let _ =
            state.update(StudioMessage::StageChanged(Some(AgentStage::new(AgentStage::REVIEW))));

        let agent = state.agents.iter().find(|a| a.id == id).expect("agent should exist");
        assert_eq!(agent.stage.as_ref().map(|s| s.as_str()), Some(AgentStage::REVIEW));
        assert!(state.unsaved);

        let (custom, _, _) = state.persisted_parts();
        let saved = custom.iter().find(|a| a.id == id).expect("saved agent should exist");
        assert_eq!(saved.stage.as_ref().map(|s| s.as_str()), Some(AgentStage::REVIEW));
    }

    #[test]
    fn persisted_output_mode_contract_is_reflected_by_the_agent_inspector() {
        let config = AppConfig {
            multi_agent: Some(concerto_config::MultiAgentConfig {
                custom_agents: vec![CustomAgentConfig {
                    id: "architect".into(),
                    name: "Architect".into(),
                    role: "architect".into(),
                    output_mode: OutputMode::DesignDoc,
                    ..Default::default()
                }],
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut state = State::new();
        state.load_from_config(&config);

        let Some(architect) = state.agents.iter().find(|a| a.id == "architect") else {
            panic!("architect agent is missing");
        };
        assert_eq!(architect.output_mode, OutputMode::DesignDoc);
    }

    #[test]
    fn inspector_renders_for_coordinator() {
        let mut state = State::new();
        let _ = state.update(StudioMessage::SelectAgent(Some("coordinator".into())));
        let _ = state.update(StudioMessage::InspectorSection(InspectorSection::Permissions));
        let theme = AppTheme::by_name("Midnight");
        let _ = state.view(&theme);
    }

    #[test]
    fn run_limit_defaults() {
        let state = State::new();
        assert_eq!(state.max_concurrent_agents, 3);
        assert_eq!(state.max_concurrent_per_provider, 2);
        assert_eq!(state.spend_cap_multiplier, 3.0);
        assert_eq!(state.run_agents_draft, "3");
        assert_eq!(state.run_provider_draft, "2");
        assert_eq!(state.run_spend_draft, "3.0");
    }

    #[test]
    fn invalid_run_limits_shown_and_block_save() {
        let mut state = State::new();
        let _ = state.update(StudioMessage::RunSpendChanged("abc".into()));
        assert!(!state.validation().ok);
        assert_eq!(state.spend_cap_multiplier, 3.0);
        let _ = state.update(StudioMessage::RunSpendChanged("5.5".into()));
        assert!(state.validation().ok);
        assert_eq!(state.spend_cap_multiplier, 5.5);
    }

    #[test]
    fn validation_badge_toggle_expands_and_collapses_issue_summary() {
        let mut state = State::default();
        assert!(!state.show_validation_detail);
        // Make validation() report the spend-cap issue.
        state.run_spend_draft = "0".into();
        assert!(!state.validation().ok);

        let _ = state.update(StudioMessage::ToggleValidationDetail);
        assert!(state.show_validation_detail);

        let _ = state.update(StudioMessage::ToggleValidationDetail);
        assert!(!state.show_validation_detail);
    }

    #[test]
    fn spend_cap_issue_reported_alone_when_only_spend_draft_invalid() {
        let mut state = State::new();
        state.run_spend_draft = "0".into();

        let report = state.validation();
        assert!(!report.ok);
        assert_eq!(
            report.messages,
            vec!["Run limits: spend cap multiplier must be a positive number".to_string()]
        );
    }

    #[test]
    fn run_limits_round_trip_via_persist_path() {
        let mut state = State::new();
        let _ = state.update(StudioMessage::RunAgentsChanged("5".into()));
        let _ = state.update(StudioMessage::RunProviderChanged("3".into()));
        let _ = state.update(StudioMessage::RunSpendChanged("7.5".into()));

        let tuning = state.run_tuning();
        assert_eq!(tuning.max_concurrent_agents, 5);
        assert_eq!(tuning.max_concurrent_per_provider, 3);
        assert_eq!(tuning.spend_cap_multiplier, 7.5);

        let config = AppConfig {
            multi_agent: Some(concerto_config::MultiAgentConfig {
                max_concurrent_agents: 5,
                max_concurrent_per_provider: 3,
                spend_cap_multiplier: 7.5,
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut reloaded = State::new();
        reloaded.load_from_config(&config);
        assert_eq!(reloaded.max_concurrent_agents, 5);
        assert_eq!(reloaded.max_concurrent_per_provider, 3);
        assert_eq!(reloaded.spend_cap_multiplier, 7.5);
        assert_eq!(reloaded.run_agents_draft, "5");
        assert_eq!(reloaded.run_provider_draft, "3");
        assert_eq!(reloaded.run_spend_draft, "7.5");
    }

    #[test]
    fn preset_round_trip_never_accumulates_builtin_duplicates() {
        let mut config = AppConfig {
            multi_agent: Some(concerto_config::MultiAgentConfig::default()),
            ..Default::default()
        };
        for _ in 0..3 {
            let mut state = State::new();
            state.load_from_config(&config);
            assert_eq!(state.presets.len(), 1, "preset list must never grow");
            assert_eq!(state.presets[0].name, "Standard Pipeline");
            assert!(state.presets[0].is_builtin);

            // Simulate the app persist path: write the persisted presets back
            // into the config, exactly as `persist_orchestration_studio` does.
            let (_, _, persisted) = state.persisted_parts();
            assert!(persisted.is_empty(), "the built-in preset must never round-trip");
            let multi = config.multi_agent.as_mut().expect("multi_agent is set");
            multi.presets = persisted;
        }
    }

    #[test]
    fn load_from_config_dedupes_corrupted_duplicate_presets() {
        // A pre-#116 config persisted the built-in "Standard Pipeline" several
        // times (is_builtin is false, as it deserializes from an old file).
        let config = AppConfig {
            multi_agent: Some(concerto_config::MultiAgentConfig {
                presets: vec![
                    PipelinePreset { name: "Standard Pipeline".into(), ..Default::default() },
                    PipelinePreset { name: "Standard Pipeline".into(), ..Default::default() },
                    PipelinePreset { name: "custom".into(), ..Default::default() },
                ],
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut state = State::new();
        state.load_from_config(&config);

        assert_eq!(state.presets.len(), 2, "built-in + one unique custom");
        let mut names: Vec<&str> = state.presets.iter().map(|p| p.name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, ["Standard Pipeline", "custom"]);
        assert_eq!(state.presets.iter().filter(|p| p.is_builtin).count(), 1);
    }

    #[test]
    fn new_pipeline_creates_untitled_preset_and_clears_relationships() {
        let mut state = State::new();
        load_standard(&mut state);
        assert_eq!(state.active_pipeline_name, "Standard Pipeline");
        assert_eq!(state.relationships.len(), 5);
        let original_presets = state.presets.len();

        let _ = state.update(StudioMessage::NewPipeline);

        // A new preset is appended and made active under the base name.
        assert_eq!(state.presets.len(), original_presets + 1);
        let created = state.presets.last().expect("new preset appended");
        assert_eq!(created.name, "Untitled Pipeline");
        assert!(created.relationships.is_empty());
        assert!(!created.is_builtin);
        assert_eq!(state.active_pipeline_name, "Untitled Pipeline");
        // The workspace is reset to the empty pipeline.
        assert!(state.relationships.is_empty());
        assert_eq!(state.selected_agent_id, None);
        assert_eq!(state.selected_relationship, None);
        assert!(!state.show_relationship_editor);
        assert!(state.new_rel_from.is_empty());
        assert!(state.unsaved);

        // A second call must pick the next free name and append another preset.
        let _ = state.update(StudioMessage::NewPipeline);
        assert_eq!(state.active_pipeline_name, "Untitled Pipeline 2");
        assert_eq!(state.presets.len(), original_presets + 2);
    }

    #[test]
    fn load_preset_sets_active_pipeline_name() {
        let mut state = State::new();
        let _ = state.update(StudioMessage::NewPipeline);
        assert_eq!(state.active_pipeline_name, "Untitled Pipeline");

        let _ = state.update(StudioMessage::LoadPreset("Standard Pipeline".into()));

        assert_eq!(state.active_pipeline_name, "Standard Pipeline");
        // The loaded built-in topology is applied as before.
        assert_eq!(state.relationships.len(), 5);
    }

    // ────────────────────────────────────────────────────────────────────────
    // ADR-59 P4 Batch 3, Slice 2: structured blueprint validation surfacing.
    // These tests exercise the blueprint path (`[orchestration]` present) and
    // assert the legacy path stays byte-identical. They are hermetic — they
    // only mutate `State` fields directly and never touch environment
    // variables.
    // ────────────────────────────────────────────────────────────────────────

    /// A broken blueprint: the standard pipeline with one stage cloned, which
    /// trips rule (g) with a deterministic `stage.tag` violation.
    fn broken_blueprint() -> Blueprint {
        let mut blueprint =
            concerto_config::named_blueprint("standard").expect("standard blueprint exists");
        blueprint.pipeline.stages.push(blueprint.pipeline.stages[0].clone());
        blueprint
    }

    /// A blueprint-path `State` whose editable model fails validation.
    fn broken_blueprint_state() -> State {
        let mut state = State {
            orchestration: Some(OrchestrationConfig::default()),
            blueprint: Some(Arc::new(broken_blueprint())),
            ..Default::default()
        };
        // `load_from_config` normally runs this; the unit path calls it
        // directly so `validation()` and `errors_for` agree.
        state.refresh_blueprint_validation();
        state
    }

    #[test]
    fn blueprint_rule_error_maps_to_per_field_structured_errors() {
        let state = broken_blueprint_state();

        // `validation()` switches to `validate_blueprint` on the blueprint
        // path: the report reflects the rulebook violation.
        let report = state.validation();
        assert!(!report.ok);
        assert_eq!(report.messages.len(), 1);
        assert!(report.messages[0].contains("duplicate stage tag"));

        // The stored per-field collection mirrors the report and is
        // addressable by dotted field path for Slice 3's field outlines.
        assert_eq!(state.blueprint_error_count(), 1);
        let stage_tag = state.errors_for("stage.tag");
        assert_eq!(stage_tag.len(), 1);
        match stage_tag[0] {
            BlueprintError::Rule { field, code, message } => {
                assert_eq!(field, "stage.tag");
                assert_eq!(*code, "rule_g");
                assert!(message.contains("duplicate stage tag"));
            }
            _ => panic!("expected a structured rule error"),
        }
        // Unrelated fields carry no errors.
        assert!(state.errors_for("stage.max_cycles").is_empty());
        assert!(state.errors_for("unknown.field").is_empty());
    }

    #[test]
    fn blueprint_badge_renders_error_count_with_alert_icon() {
        // ADR-59 D5: icon + count on the badge string surface (never color
        // alone); the label is unit-tested and the full view renders both
        // collapsed and expanded with a broken blueprint.
        assert_eq!(validation_badge_label(1, false), "⚠ 1 error ▾");
        assert_eq!(validation_badge_label(3, true), "⚠ 3 errors ▴");

        let mut state = broken_blueprint_state();
        let theme = AppTheme::by_name("Midnight");
        let _ = state.view(&theme);

        state.show_validation_detail = true;
        let _ = state.view(&theme);
        assert_eq!(state.blueprint_error_count(), 1);
    }

    #[test]
    fn legacy_path_validation_is_unchanged_without_orchestration() {
        // No `[orchestration]` section → the Studio keeps the legacy
        // `multi_agent` checks byte-identical and the blueprint error surfaces
        // stay empty.
        let mut state = State::new();
        state.run_spend_draft = "0".into();
        let report = state.validation();
        assert!(!report.ok);
        assert_eq!(
            report.messages,
            vec!["Run limits: spend cap multiplier must be a positive number".to_string()]
        );
        assert!(state.orchestration.is_none());
        assert_eq!(state.blueprint_error_count(), 0);
        assert!(state.errors_for("stage.tag").is_empty());
    }

    #[test]
    fn load_from_config_recomputes_blueprint_validation_and_rule_f_bound() {
        // A resolved `[orchestration]` config loads the editable blueprint and
        // recomputes the (empty) error collection; rule (f)'s bound mirrors
        // the load seam's `max_total_iterations`.
        let blueprint =
            concerto_config::named_blueprint("standard").expect("standard blueprint exists");
        let resolved =
            concerto_config::resolve_blueprint(&blueprint).expect("standard resolves cleanly");
        let config = AppConfig {
            orchestration: Some(OrchestrationConfig::default()),
            resolved_blueprint: Some(Arc::new(resolved)),
            multi_agent: Some(concerto_config::MultiAgentConfig {
                max_total_iterations: Some(50),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut state = State::new();
        state.load_from_config(&config);

        assert!(state.orchestration.is_some());
        assert!(state.blueprint.is_some());
        assert!(state.resolved_blueprint.is_some());
        assert_eq!(state.global_max_dispatch_cycles, Some(50));
        assert!(state.validation().ok);
        assert_eq!(state.blueprint_error_count(), 0);
    }

    #[test]
    fn every_blueprint_error_variant_maps_to_a_ui_friendly_projection() {
        // The Studio's error surface must be total over `BlueprintError` —
        // the detail renderer and report builder never panic on any variant.
        // The load-time variants cannot be produced by `validate_blueprint` on
        // an in-memory model, but they are mapped anyway so the union is safe.
        let variants = [
            BlueprintError::Validation("blueprint name must be non-empty".into()),
            BlueprintError::UnknownNamedBlueprint(
                "nope".into(),
                "standard, tdd, docs-only, research-only".into(),
            ),
            BlueprintError::MissingIncludeFile("/tmp/nope.toml".into()),
            BlueprintError::ParseIncludeFile {
                path: "/tmp/a.toml".into(),
                detail: "syntax error".into(),
            },
            BlueprintError::InvalidSelection("blueprint selection is empty".into()),
            BlueprintError::rule("stage.tag", "rule_g", "duplicate stage tag 'x'"),
        ];
        for variant in &variants {
            let view = blueprint_error_view(variant);
            assert!(!view.code.is_empty());
            assert!(!view.message.is_empty());
        }
        let rule = blueprint_error_view(&variants[5]);
        assert_eq!(rule.field.as_deref(), Some("stage.tag"));
        assert_eq!(rule.code, "rule_g");
        let validation = blueprint_error_view(&variants[0]);
        assert_eq!(validation.field, None);
        assert_eq!(validation.code, "validation");
    }

    // ────────────────────────────────────────────────────────────────────────
    // ADR-59 P4 Batch 3, Slice 3a-1: dual-path view branch + stage cards.
    // These tests are hermetic — they only mutate `State` fields directly and
    // never touch environment variables. The blueprint-path state mirrors the
    // Slice-2 `broken_blueprint_state` shape (orchestration + blueprint
    // populated via struct literal over `Default`).
    // ────────────────────────────────────────────────────────────────────────

    /// A blueprint-path `State` carrying the valid standard blueprint.
    fn standard_blueprint_state() -> State {
        State {
            orchestration: Some(OrchestrationConfig::default()),
            blueprint: Some(Arc::new(
                concerto_config::named_blueprint("standard").expect("standard blueprint exists"),
            )),
            ..Default::default()
        }
    }

    #[test]
    fn stage_cards_view_renders_for_a_standard_blueprint_state() {
        // Dual-path branch: `orchestration` AND `blueprint` are `Some`, so
        // `view()` routes the main editor area to `stage_cards_view`. Renders
        // without panicking, both collapsed and with an advanced section open.
        let mut state = standard_blueprint_state();
        let theme = AppTheme::by_name("Midnight");
        let _ = state.view(&theme);

        let _ = state.update(StudioMessage::StageAdvancedToggle(0));
        let _ = state.view(&theme);
        assert!(state.stage_advanced_open.contains(&0));
    }

    #[test]
    fn legacy_state_without_orchestration_renders_without_panicking() {
        // ADR-58/59 (rewritten) Slice 2: with no `[orchestration]` section — and hence no
        // loaded blueprint — the main editor area routes to the inactive
        // placeholder (the splash is gone), so `view()` must render without
        // panicking and must never fabricate blueprint state (no stage-card
        // surface is reachable).
        let state = State::new();
        let theme = AppTheme::by_name("Midnight");
        let _ = state.view(&theme);

        assert!(state.orchestration.is_none());
        assert!(state.blueprint.is_none());
        assert!(state.stage_advanced_open.is_empty());
    }

    #[test]
    fn stage_advanced_toggle_changes_the_open_set_without_marking_dirty() {
        let mut state = standard_blueprint_state();
        assert!(state.stage_advanced_open.is_empty());

        // Opening an index inserts it; toggling it again removes it.
        let _ = state.update(StudioMessage::StageAdvancedToggle(2));
        assert!(state.stage_advanced_open.contains(&2));
        assert_eq!(state.stage_advanced_open.len(), 1);
        assert!(!state.unsaved, "view-only toggles never mark the studio dirty");

        let _ = state.update(StudioMessage::StageAdvancedToggle(3));
        assert_eq!(state.stage_advanced_open.len(), 2);

        let _ = state.update(StudioMessage::StageAdvancedToggle(2));
        assert!(!state.stage_advanced_open.contains(&2));
        assert!(state.stage_advanced_open.contains(&3));
    }

    #[test]
    fn stage_cards_surface_covers_all_standard_stages_in_order() {
        // The stage-card surface reads `blueprint.pipeline.stages` in order,
        // so the standard blueprint (design, research, implement, review,
        // validate) must all be renderable through the card path — the
        // surface must never panic on any of the closed kinds' data shapes
        // (staffed, gated-with-fallback, Execution-kind mask, etc.).
        let state = standard_blueprint_state();
        let stages = state
            .blueprint
            .as_ref()
            .expect("blueprint loaded")
            .pipeline
            .stages
            .iter()
            .map(|stage| stage.tag.clone())
            .collect::<Vec<_>>();
        assert_eq!(stages, vec!["design", "research", "implement", "review", "validate"]);

        let theme = AppTheme::by_name("Midnight");
        let _ = state.view(&theme);
    }

    // ────────────────────────────────────────────────────────────────────────
    // ADR-59 P4 Batch 3, Slice 3a-2: stage-card mutations + per-field
    // outlines. These tests are hermetic — they only mutate `State` fields
    // directly and never touch environment variables. The outline-state
    // helper (`field_error`) is asserted against the same broken-blueprint
    // shape the Slice-2 tests use, so `errors_for` and the widget outline
    // always agree on the rulebook field path.
    // ────────────────────────────────────────────────────────────────────────

    fn stage_of(state: &State, index: usize) -> &StageDef {
        state
            .blueprint
            .as_ref()
            .expect("blueprint loaded")
            .pipeline
            .stages
            .get(index)
            .expect("stage index in range")
    }

    #[test]
    fn stage_tag_edit_mutates_the_blueprint_and_revalidates_immediately() {
        let mut state = standard_blueprint_state();
        assert_eq!(stage_of(&state, 0).tag, "design");

        // Plain edit: the model is updated through `Arc::make_mut` (the
        // shared `Arc` still aliases the original) and validation stays
        // clean. Blueprint edits never mark the studio dirty.
        let _ = state.update(StudioMessage::StageTagEdited(0, "renamed".into()));
        assert_eq!(stage_of(&state, 0).tag, "renamed");
        assert!(state.validation().ok);
        assert!(state.errors_for("stage.tag").is_empty());
        assert!(!state.unsaved, "blueprint edits do not mark the studio dirty");

        // Violating edit: a duplicate tag trips rule_g on `"stage.tag"` and
        // `refresh_blueprint_validation` recomputes immediately, so the
        // outline-state helper sees the rule message right away.
        let _ = state.update(StudioMessage::StageTagEdited(1, "renamed".into()));
        assert_eq!(state.errors_for("stage.tag").len(), 1);
        assert!(state.field_error("stage.tag").is_some());
        assert!(!state.validation().ok);
    }

    #[test]
    fn stage_kind_input_accepts_unknown_kinds_and_offers_the_six_known_suggestions() {
        // Slice 3 (spec §2): the kind input is free text — the model kind is
        // an open String, and unknown user kinds are valid (never gates) — so
        // the six closed `StageKind` entries are catalog'd only as one-click
        // suggestions, not a restricted picker.
        let kinds = kind_options().map(|option| option.kind);
        assert_eq!(
            kinds,
            [
                StageKind::Research,
                StageKind::Planning,
                StageKind::Execution,
                StageKind::Review,
                StageKind::Acceptance,
                StageKind::RunOnce,
            ]
        );

        // A suggestion writes the canonical snake_case string into the model.
        let mut state = standard_blueprint_state();
        assert_eq!(stage_of(&state, 0).kind, StageKind::Planning.as_str().to_string());
        let _ = state.update(StudioMessage::StageKindChanged(0, StageKind::RunOnce));
        assert_eq!(stage_of(&state, 0).kind, StageKind::RunOnce.as_str().to_string());

        // Free text keeps an unknown kind verbatim and stays valid: unknown
        // kinds are never gates, so rulebook (c) does not fire, and the
        // remaining stage data (staffing, tag) is unchanged.
        let _ = state.update(StudioMessage::StageKindEdited(0, "my_custom_kind".into()));
        assert_eq!(stage_of(&state, 0).kind, "my_custom_kind");
        assert!(
            state.errors_for("stage.kind").is_empty(),
            "unknown kinds are valid — no rulebook violation"
        );
        assert!(state.validation().ok);
    }

    #[test]
    fn stage_staffing_toggle_adds_and_removes_membership() {
        // The chip × and the add pick-list share one toggle message: present
        // → removed, absent → appended. `AgentId` normalizes to lowercase,
        // matching the legacy library ids.
        let mut state = standard_blueprint_state();
        assert_eq!(stage_of(&state, 0).agents, vec!["architect"]);

        let _ = state.update(StudioMessage::StageStaffingToggle(0, AgentId::new("architect")));
        assert!(stage_of(&state, 0).agents.is_empty());

        let _ = state.update(StudioMessage::StageStaffingToggle(0, AgentId::new("coder")));
        assert_eq!(stage_of(&state, 0).agents, vec!["coder"]);

        let _ = state.update(StudioMessage::StageStaffingToggle(0, AgentId::new("Coder")));
        assert!(stage_of(&state, 0).agents.is_empty(), "case-insensitive id space");
        assert!(!state.unsaved);
    }

    #[test]
    fn stage_mask_toggle_writes_an_explicit_catalog_flag() {
        // Toggling a mask flag writes the explicit value (overriding the kind
        // default) and the effective mask follows immediately. `stage.flags`
        // has no rulebook field path, so no error surface is involved.
        let mut state = standard_blueprint_state();
        let initial = stage_of(&state, 0).effective_capabilities();

        let _ = state.update(StudioMessage::StageMaskToggled(
            0,
            StageMaskFlag::FsWrite,
            !initial.fs_write,
        ));
        assert_eq!(stage_of(&state, 0).flags.fs_write, Some(!initial.fs_write));
        assert_eq!(stage_of(&state, 0).effective_capabilities().fs_write, !initial.fs_write);

        let _ = state.update(StudioMessage::StageMaskToggled(0, StageMaskFlag::Shell, true));
        assert_eq!(stage_of(&state, 0).flags.shell, Some(true));
        assert!(stage_of(&state, 0).effective_capabilities().shell);
        assert!(!state.unsaved);
    }

    #[test]
    fn stage_feed_and_condition_changes_update_the_model() {
        // The feed picker can bind a closed feed or remove it (`None`); the
        // condition picker selects one of the two closed predicates. Uses
        // relative edits so the test never depends on a blueprint's defaults.
        let mut state = standard_blueprint_state();
        let feed_before = stage_of(&state, 0).feed;
        let condition_before = stage_of(&state, 0).condition;

        // A different closed feed replaces the current binding.
        let other = match feed_before {
            Some(FeedLabel::Verify) => FeedLabel::Understand,
            _ => FeedLabel::Verify,
        };
        let _ = state.update(StudioMessage::StageFeedChanged(0, Some(other)));
        assert_eq!(stage_of(&state, 0).feed, Some(other));

        // Removing the binding writes `None` into the model.
        let _ = state.update(StudioMessage::StageFeedChanged(0, None));
        assert_eq!(stage_of(&state, 0).feed, None);

        // The condition picker selects a closed predicate (here, always the
        // non-default one when the stage starts on `Always`).
        let other_condition = if condition_before == StageCondition::Always {
            StageCondition::OnGateCycle
        } else {
            StageCondition::Always
        };
        let _ = state.update(StudioMessage::StageConditionChanged(0, other_condition));
        assert_eq!(stage_of(&state, 0).condition, other_condition);
    }

    #[test]
    fn stage_max_cycles_edit_keeps_an_invalid_draft_and_only_writes_parsed_values() {
        // Mirrors the run-limit drafts: the raw input is always stored so an
        // unparsable value stays visible and never fights the user's typing.
        // Only a parsed `u32` (or empty → kind default) reaches the model.
        let mut state = standard_blueprint_state();
        let before = stage_of(&state, 0).max_cycles;

        let _ = state.update(StudioMessage::StageMaxCyclesEdited(0, "abc".into()));
        assert_eq!(state.stage_max_cycles_drafts.get(&0).map(String::as_str), Some("abc"));
        assert_eq!(stage_of(&state, 0).max_cycles, before, "invalid draft never reaches the model");
        assert!(
            state.errors_for("stage.max_cycles").is_empty(),
            "no model edit ⇒ no revalidation noise"
        );

        let _ = state.update(StudioMessage::StageMaxCyclesEdited(0, "12".into()));
        assert_eq!(stage_of(&state, 0).max_cycles, Some(12));
        assert_eq!(state.stage_max_cycles_drafts.get(&0).map(String::as_str), Some("12"));

        // Whitespace-only input means "use the kind default".
        let _ = state.update(StudioMessage::StageMaxCyclesEdited(0, "   ".into()));
        assert_eq!(stage_of(&state, 0).max_cycles, None);
    }

    #[test]
    fn stage_move_up_swaps_with_the_previous_stage_and_reseeds_drafts() {
        // Slice 3 (spec §2) reorder: `StageMoveUp(i)` swaps stage `i` with
        // `i-1`. The index-keyed max-cycles drafts shift with the swap, so the
        // re-seeded drafts follow the moved stage, not its new position.
        let mut state = standard_blueprint_state();
        // Set a max-cycles cap on stage 1 through the real edit path (writes
        // both the model and the draft), so the reseed has model truth to
        // follow after the move.
        let _ = state.update(StudioMessage::StageMaxCyclesEdited(1, "7".into()));
        let tags_before: Vec<String> = state
            .blueprint
            .as_ref()
            .unwrap()
            .pipeline
            .stages
            .iter()
            .map(|s| s.tag.clone())
            .collect();
        let len = tags_before.len();

        let _ = state.update(StudioMessage::StageMoveUp(1));
        let stages = &state.blueprint.as_ref().unwrap().pipeline.stages;
        assert_eq!(stages[0].tag, tags_before[1], "index 1 moved to position 0");
        assert_eq!(stages[1].tag, tags_before[0]);
        assert_eq!(stages.len(), len, "reorder never changes the stage count");
        assert_eq!(
            state.stage_max_cycles_drafts.get(&0).map(String::as_str),
            Some("7"),
            "max-cycles draft follows the moved stage into its new index"
        );
        assert!(state.validation().ok, "a valid reorder stays valid");
    }

    #[test]
    fn stage_move_down_swaps_with_the_next_stage() {
        // Slice 3 (spec §2): `StageMoveDown(i)` swaps stage `i` with `i+1`.
        let mut state = standard_blueprint_state();
        let tags_before: Vec<String> = state
            .blueprint
            .as_ref()
            .unwrap()
            .pipeline
            .stages
            .iter()
            .map(|s| s.tag.clone())
            .collect();

        let _ = state.update(StudioMessage::StageMoveDown(0));
        let stages = &state.blueprint.as_ref().unwrap().pipeline.stages;
        assert_eq!(stages[0].tag, tags_before[1], "index 0 moved to position 1");
        assert_eq!(stages[1].tag, tags_before[0]);
    }

    #[test]
    fn stage_move_at_pipeline_bounds_is_a_noop() {
        // Reordering past the head or tail must be a silent no-op — no panic,
        // no mutation, drafts untouched.
        let mut state = standard_blueprint_state();
        let tags_before: Vec<String> = state
            .blueprint
            .as_ref()
            .unwrap()
            .pipeline
            .stages
            .iter()
            .map(|s| s.tag.clone())
            .collect();
        let last = tags_before.len() - 1;

        let _ = state.update(StudioMessage::StageMoveUp(0));
        let _ = state.update(StudioMessage::StageMoveDown(last));
        let stages = &state.blueprint.as_ref().unwrap().pipeline.stages;
        let tags_after: Vec<String> = stages.iter().map(|s| s.tag.clone()).collect();
        assert_eq!(tags_after, tags_before, "out-of-bounds moves never mutate the pipeline");
        assert!(state.stage_max_cycles_drafts.is_empty(), "drafts are untouched by no-ops");
    }

    #[test]
    fn stage_deleted_removes_the_stage_and_any_relationship_touching_it() {
        // Slice 3 (spec §2): deleting a stage also drops the relationships
        // whose `from`/`to` the stage owned — their row endpoints would
        // otherwise dangle past the stage-tag picker's catalog. The standard
        // blueprint ties its five relationships to AGENT ids, not stage tags,
        // so the test adds a stage-tag relationship first ("design" → "design")
        // and asserts exactly that row is pruned while the others survive.
        let mut state = standard_blueprint_state();
        let relationships_before_len = state.blueprint.as_ref().unwrap().relationships.len();
        // `RelationshipAdded` seeds a default row from/to the FIRST stage tag.
        let _ = state.update(StudioMessage::RelationshipAdded);
        let blueprint = state.blueprint.as_ref().unwrap();
        assert_eq!(blueprint.relationships.len(), relationships_before_len + 1);
        assert_eq!(
            blueprint.relationships.last().unwrap().to,
            "design",
            "added row targets the first stage tag"
        );

        // Deleting the "design" stage prunes the design-targeting row only.
        let _ = state.update(StudioMessage::StageDeleted(0));
        let blueprint = state.blueprint.as_ref().unwrap();
        assert!(
            blueprint.pipeline.stages.iter().all(|s| s.tag != "design"),
            "the stage itself is gone"
        );
        assert_eq!(
            blueprint.relationships.len(),
            relationships_before_len,
            "only the dangling design row is pruned"
        );
        assert!(
            blueprint.relationships.iter().all(|r| r.from != "design" && r.to != "design"),
            "no relationship references the deleted stage"
        );
        assert!(
            !state.stage_max_cycles_drafts.is_empty(),
            "drafts are re-seeded for the shifted tail"
        );
        assert!(state.stage_max_cycles_drafts.len() <= blueprint.pipeline.stages.len());
    }

    #[test]
    fn stage_deleted_out_of_range_is_a_noop() {
        // A stale delete index must not remove the wrong stage (or panic).
        let mut state = standard_blueprint_state();
        let len = state.blueprint.as_ref().unwrap().pipeline.stages.len();

        let _ = state.update(StudioMessage::StageDeleted(len + 5));
        assert_eq!(state.blueprint.as_ref().unwrap().pipeline.stages.len(), len);
    }

    #[test]
    fn stage_added_appends_a_default_freeform_stage() {
        // Slice 3 (spec §2): "+ Add Stage" appends a default stage — freeform
        // `RunOnce` kind, unique `stage-N` tag, unflagged, unstaffed, no
        // feed/condition/max-cycles override. The drafts gain the new entry.
        let mut state = standard_blueprint_state();
        let len = state.blueprint.as_ref().unwrap().pipeline.stages.len();

        let _ = state.update(StudioMessage::StageAdded);
        let blueprint = state.blueprint.as_ref().unwrap();
        assert_eq!(blueprint.pipeline.stages.len(), len + 1);
        let added = blueprint.pipeline.stages.last().unwrap();
        assert_eq!(
            added.tag, "stage-6",
            "the suffix continues from the pipeline length ({len} stages → stage-6)"
        );
        assert_eq!(added.kind, "run_once");
        assert_eq!(added.label, format!("Stage {}", len + 1));
        assert!(added.agents.is_empty());
        assert!(added.fallback.is_none());
        assert_eq!(added.max_cycles, None);
        assert_eq!(added.condition, StageCondition::Always);
        assert!(blueprint.pipeline.stages.iter().filter(|s| s.tag == "stage-6").count() == 1);
        let second_tag = {
            // A second add must collision-suffix forward.
            let _ = state.update(StudioMessage::StageAdded);
            state.blueprint.as_ref().unwrap().pipeline.stages.last().unwrap().tag.clone()
        };
        assert_eq!(second_tag, "stage-7");
        assert!(
            state.stage_max_cycles_drafts.contains_key(&(len + 1)),
            "the new stage gets a draft entry"
        );
        assert!(state.validation().ok, "a default added stage satisfies the rulebook");
    }

    #[test]
    fn coordinator_is_never_staffable_on_any_stage() {
        // Slice 3 engine-owned invariant: the coordinator is not selectable as
        // stage staff. It must be rejected even when a message names it
        // directly (the picker already filters the candidate list).
        let mut state = standard_blueprint_state();
        let before: Vec<String> = stage_of(&state, 0).agents.clone();

        let _ = state.update(StudioMessage::StageStaffingToggle(0, AgentId::new("coordinator")));
        assert_eq!(
            stage_of(&state, 0).agents,
            before,
            "a direct coordinator staffing message is refused"
        );

        // Removing a coordinator chip (defensive) is also a no-op.
        let _ = state.update(StudioMessage::StageStaffingToggle(1, AgentId::new("coordinator")));
        assert!(stage_of(&state, 1).agents.iter().all(|a| a != "coordinator"));
    }

    #[test]
    fn broken_blueprint_outline_helper_reads_the_rule_message_by_field_path() {
        // The outline-state helper reads the same authoritative collection
        // the widget outlines use: a rule addressed to `"stage.tag"` surfaces
        // on the tag field's outline and nowhere else.
        let state = broken_blueprint_state();
        assert_eq!(state.errors_for("stage.tag").len(), 1);
        let message = state.field_error("stage.tag").expect("outline message for the tag field");
        assert!(message.contains("duplicate stage tag"));

        // Clean fields carry no outline.
        assert!(state.field_error("stage.max_cycles").is_none());
        assert!(state.field_error("stage.kind").is_none());
        assert!(state.field_error("stage.condition").is_none());
        assert!(state.field_error("stage.feed").is_none());
    }

    #[test]
    fn stage_mutations_on_missing_blueprint_or_bad_index_are_noops() {
        // The legacy path has no blueprint: the stage-card messages must be
        // accepted without panicking and must not invent a model.
        let mut state = State::new();
        let _ = state.update(StudioMessage::StageTagEdited(0, "x".into()));
        let _ = state.update(StudioMessage::StageStaffingToggle(0, AgentId::new("coder")));
        let _ = state.update(StudioMessage::StageMaxCyclesEdited(0, "9".into()));
        assert!(state.blueprint.is_none());

        // A stale-frame index past the (shrunken) stage list is a no-op.
        let mut state = standard_blueprint_state();
        let _ = state.update(StudioMessage::StageTagEdited(99, "x".into()));
        assert_eq!(stage_of(&state, 0).tag, "design");
        assert_eq!(state.errors_for("stage.tag").len(), 0);
    }

    // ────────────────────────────────────────────────────────────────────────
    // ADR-59 P4 Batch 3, Slice 3b: relationship rows + fallback persona card
    // + legacy-draft hide. These tests are hermetic — they only mutate
    // `State` fields directly and never touch environment variables. Rulebook
    // note: `validate_blueprint` emits no relationship field paths, so the
    // relationship-row mutations never surface errors; the fallback card's
    // editable fields DO have paths (`"stage.fallback"`, rulebook (c)/(d);
    // `"stage.fallback.capabilities"`, rulebook (d) widening), and those are
    // asserted through the same `errors_for` surface the widget outlines use.
    // ────────────────────────────────────────────────────────────────────────

    /// The fallback persona of one stage of the editable blueprint.
    fn fallback_of(state: &State, stage_index: usize) -> &FallbackPersonaDef {
        state
            .blueprint
            .as_ref()
            .expect("blueprint loaded")
            .pipeline
            .stages
            .get(stage_index)
            .expect("stage index in range")
            .fallback
            .as_ref()
            .expect("stage ships a fallback persona")
    }

    #[test]
    fn relationship_add_and_delete_mutate_the_blueprint_and_revalidate() {
        let mut state = standard_blueprint_state();
        let before = state.blueprint.as_ref().expect("blueprint loaded").relationships.len();
        assert_eq!(before, 5, "the standard blueprint ships five relationship rows");

        let _ = state.update(StudioMessage::RelationshipAdded);
        let blueprint = state.blueprint.as_ref().expect("blueprint loaded");
        assert_eq!(blueprint.relationships.len(), before + 1);
        let added = &blueprint.relationships[before];
        // Default row (spec §3): first stage → first registered kind → first
        // stage, carrying that kind's closed semantics from the registry.
        assert_eq!(added.from, "design");
        assert_eq!(added.to, "design");
        assert_eq!(added.kind, "supervises");
        assert_eq!(added.semantics, RelationshipSemantics::Delegation);
        // Relationship rows have no rulebook field path — the added row never
        // trips validation, and blueprint edits never mark the studio dirty.
        assert!(state.validation().ok);
        assert!(!state.unsaved);

        let _ = state.update(StudioMessage::RelationshipDeleted(before));
        assert_eq!(
            state.blueprint.as_ref().expect("blueprint loaded").relationships.len(),
            before,
            "row-level delete restores the row count"
        );
    }

    #[test]
    fn relationship_field_edits_mutate_and_revalidate_immediately() {
        let mut state = standard_blueprint_state();
        assert_eq!(
            state.blueprint.as_ref().expect("blueprint loaded").relationships[0].from,
            "reviewer"
        );

        let _ = state.update(StudioMessage::RelationshipFromChanged(0, "design".into()));
        assert_eq!(
            state.blueprint.as_ref().expect("blueprint loaded").relationships[0].from,
            "design"
        );

        let _ = state.update(StudioMessage::RelationshipToChanged(0, "validate".into()));
        assert_eq!(
            state.blueprint.as_ref().expect("blueprint loaded").relationships[0].to,
            "validate"
        );

        // The kind picker only offers kinds registered in the open registry;
        // selecting one resolves the row's closed semantics from the registry
        // (row 1 is `provides_context_to` → ContextFlow).
        let _ =
            state.update(StudioMessage::RelationshipKindChanged(0, "provides_context_to".into()));
        let relationship = &state.blueprint.as_ref().expect("blueprint loaded").relationships[0];
        assert_eq!(relationship.kind, "provides_context_to");
        assert_eq!(relationship.semantics, RelationshipSemantics::ContextFlow);

        assert!(state.validation().ok, "relationship edits have no rulebook paths");
        assert!(!state.unsaved);
    }

    #[test]
    fn relationship_edits_without_a_blueprint_or_with_a_stale_index_are_noops() {
        let mut state = State::new();
        let _ = state.update(StudioMessage::RelationshipDeleted(0));
        let _ = state.update(StudioMessage::RelationshipFromChanged(0, "design".into()));
        let _ = state.update(StudioMessage::RelationshipAdded);
        assert!(state.blueprint.is_none(), "no blueprint ⇒ no model is invented");

        let mut state = standard_blueprint_state();
        let len = state.blueprint.as_ref().expect("blueprint loaded").relationships.len();
        let _ = state.update(StudioMessage::RelationshipDeleted(99));
        assert_eq!(state.blueprint.as_ref().expect("blueprint loaded").relationships.len(), len);
        assert!(state.validation().ok);
    }

    #[test]
    fn relationship_semantics_helpers_cover_the_closed_semantics_set() {
        // ADR-58 §4 closed semantics: the glyph + label affordances are total
        // over the three variants — never a panic, never an empty label.
        let closed = [
            RelationshipSemantics::ApprovalGate,
            RelationshipSemantics::ContextFlow,
            RelationshipSemantics::Delegation,
        ];
        for semantics in closed {
            assert!(!semantics_glyph(semantics).is_empty());
            assert!(!semantics_label(semantics).is_empty());
        }
    }

    #[test]
    fn fallback_persona_edits_mutate_and_revalidate_immediately() {
        // The standard blueprint's review stage (index 3) ships a fallback
        // persona (`coordinator_fallback`).
        let mut state = standard_blueprint_state();
        assert_eq!(fallback_of(&state, 3).id, "coordinator");

        let _ = state.update(StudioMessage::FallbackLabelEdited(3, "Quality reviewer".into()));
        assert_eq!(fallback_of(&state, 3).label, "Quality reviewer");

        let _ =
            state.update(StudioMessage::FallbackInstructionsEdited(3, "check the tests".into()));
        assert_eq!(fallback_of(&state, 3).system_instructions.as_deref(), Some("check the tests"));

        let _ = state.update(StudioMessage::FallbackIdEdited(3, "coordinator".into()));
        assert_eq!(fallback_of(&state, 3).id, "coordinator");
        assert!(state.validation().ok, "edits that keep the model valid stay clean");
        assert!(!state.unsaved);
    }

    #[test]
    fn fallback_id_colliding_with_a_staffed_agent_trips_rule_d_inline() {
        // Rulebook (d): the fallback id must differ from any agent staffed in
        // the same stage. Editing the review stage's fallback to its own
        // agent id surfaces the violation on `"stage.fallback"` immediately.
        let mut state = standard_blueprint_state();
        assert!(state.validation().ok);
        let _ = state.update(StudioMessage::FallbackIdEdited(3, "reviewer".into()));
        let errors = state.errors_for("stage.fallback");
        assert_eq!(errors.len(), 1);
        match &errors[0] {
            BlueprintError::Rule { field, code, .. } => {
                assert_eq!(field, "stage.fallback");
                assert_eq!(*code, "rule_d");
            }
            other => panic!("expected rule_d on stage.fallback, got {other:?}"),
        }
        assert!(!state.validation().ok);
    }

    #[test]
    fn fallback_capability_toggle_writes_a_plain_flag_without_rule_d() {
        // Slice 1b: the removed rulebook (d) widening/narrowing check — a
        // fallback flag is a plain flag, exactly like the stage mask. Toggling
        // an explicit `true`/`false` writes the value, the effective mask
        // follows, and `"stage.fallback.capabilities"` never carries an
        // outline (no rule_d fires on width).
        let mut state = standard_blueprint_state();
        assert_eq!(fallback_of(&state, 3).capabilities, concerto_config::StageFlags::default());
        assert!(state.validation().ok);

        // An explicit false on the default-false review-gate mask.
        let _ = state.update(StudioMessage::FallbackCapabilityToggled(
            3,
            StageMaskFlag::FsWrite,
            false,
        ));
        assert_eq!(fallback_of(&state, 3).capabilities.fs_write, Some(false));
        assert!(
            !fallback_of(&state, 3)
                .effective_capabilities(concerto_config::StageKind::Review)
                .fs_write
        );
        assert!(state.validation().ok);

        // An explicit true — the flag is plain, so this writes through cleanly
        // (formerly "widening" tripped rule_d; rule (d) is removed).
        let _ =
            state.update(StudioMessage::FallbackCapabilityToggled(3, StageMaskFlag::Shell, true));
        assert_eq!(fallback_of(&state, 3).capabilities.shell, Some(true));
        assert!(
            fallback_of(&state, 3).effective_capabilities(concerto_config::StageKind::Review).shell
        );
        assert!(state.validation().ok, "fallback flags never trip a rulebook gate");
        assert!(state.errors_for("stage.fallback.capabilities").is_empty());
        assert!(!state.unsaved);

        // Restoring the default keeps everything clean.
        let _ =
            state.update(StudioMessage::FallbackCapabilityToggled(3, StageMaskFlag::Shell, false));
        assert!(state.validation().ok);
        assert!(state.errors_for("stage.fallback.capabilities").is_empty());
    }

    #[test]
    fn fallback_edits_on_a_stage_without_a_fallback_are_noops() {
        // The standard blueprint's implement stage (index 2) ships no
        // fallback: the edits are accepted without panicking and never invent
        // a persona.
        let mut state = standard_blueprint_state();
        assert!(state.blueprint.as_ref().expect("blueprint loaded").pipeline.stages[2]
            .fallback
            .is_none());
        let _ = state.update(StudioMessage::FallbackLabelEdited(2, "x".into()));
        let _ = state.update(StudioMessage::FallbackInstructionsEdited(2, "y".into()));
        let _ = state.update(StudioMessage::FallbackIdEdited(2, "z".into()));
        let _ =
            state.update(StudioMessage::FallbackCapabilityToggled(2, StageMaskFlag::Shell, true));
        assert!(state.blueprint.as_ref().expect("blueprint loaded").pipeline.stages[2]
            .fallback
            .is_none());
        assert!(state.validation().ok);
    }

    #[test]
    fn blueprint_path_renders_relationship_rows_and_fallback_cards_without_panicking() {
        // Full-surface smoke: the standard blueprint renders the relationship
        // rows (5) and the fallback persona cards on the review + validate
        // stages, collapsed and with an advanced section open.
        let mut state = standard_blueprint_state();
        let theme = AppTheme::by_name("Midnight");
        let _ = state.view(&theme);

        let _ = state.update(StudioMessage::StageAdvancedToggle(3));
        let _ = state.view(&theme);
        assert!(state.stage_advanced_open.contains(&3));
    }

    #[test]
    fn legacy_drafts_are_unreachable_on_the_blueprint_path() {
        // Blueprint users never see the legacy run-limit drafts (oracle
        // finding, Slice 2): the Run Limits card — the drafts' only renderer —
        // returns `None` on the blueprint path and `Some` on the legacy path,
        // so the drafts are unreachable when `[orchestration]` governs.
        let theme = AppTheme::by_name("Midnight");
        let state = standard_blueprint_state();
        assert!(state.run_limits_card(&theme).is_none(), "blueprint path hides the legacy drafts");

        let legacy = State::new();
        assert!(legacy.run_limits_card(&theme).is_some(), "legacy path keeps the run limits card");
    }

    // ────────────────────────────────────────────────────────────────────────
    // ADR-58/59 (rewritten) Slice 2: inactive-blueprint-surface placeholder + oracle
    // carry-overs. These tests are hermetic — they only mutate `State` fields
    // and never touch environment variables.
    // ────────────────────────────────────────────────────────────────────────

    #[test]
    fn inactive_surface_renders_for_every_inactive_blueprint_surface_state() {
        // ADR-58/59 (rewritten) Slice 2: the blueprint surface is active from the first open
        // (the roster auto-seeds in `App`), so the inactive placeholder only
        // ever renders defensively — (a) the legacy `State::new()` shape and
        // (b) an `[orchestration]` selection whose editable `Blueprint` never
        // loaded. Both must render `view()` without panicking, and neither
        // exposes any Initialize action (the manual-init path is gone).
        for state in [
            State::new(),
            State { orchestration: Some(OrchestrationConfig::default()), ..Default::default() },
        ] {
            assert!(
                !(state.orchestration.is_some() && state.blueprint.is_some()),
                "precondition: no active stage-card surface"
            );
            let theme = AppTheme::by_name("Midnight");
            let _ = state.view(&theme);
        }
    }

    #[test]
    fn fallback_added_mutates_the_stage_and_revalidates_immediately() {
        // Oracle carry-over (Slice 4a, spec §4): the "Add fallback persona"
        // affordance on a stage without one synthesizes a default persona via
        // the `mutate_stage` seam (which refreshes blueprint validation). The
        // id derives from the stage tag and is collision-suffixed against the
        // stage's staffed agents (rulebook (d)); the engine-owned sentinel id
        // is never synthesized; an existing fallback is never replaced.
        let mut state = standard_blueprint_state();
        let index = 2; // `implement` — the standard blueprint's no-fallback stage.
        assert!(stage_of(&state, index).fallback.is_none(), "precondition: no fallback persona");

        let _ = state.update(StudioMessage::FallbackAdded(index));
        let fallback = fallback_of(&state, index);
        assert_eq!(fallback.id, "implement_fallback");
        assert_eq!(fallback.label, "Implement fallback");
        assert_eq!(fallback.system_instructions, None);
        assert_eq!(fallback.capabilities, concerto_config::StageFlags::default());
        assert_ne!(
            fallback.id, FALLBACK_SENTINEL_ID,
            "user-added fallbacks never carry the engine-owned sentinel id"
        );
        assert!(state.validation().ok, "mutation refreshes stored blueprint validation");

        // Adding again is a no-op: an existing fallback persona is kept as-is.
        let _ = state.update(StudioMessage::FallbackAdded(index));
        assert_eq!(fallback_of(&state, index).id, "implement_fallback");
        assert!(!state.unsaved, "blueprint edits never mark the studio dirty");
    }

    #[test]
    fn fallback_added_collision_suffix_keeps_the_default_id_unique_per_stage() {
        // When the plain tag-derived id collides with a staffed agent
        // (rulebook (d)), the synthesized id is suffixed until unique — mirror
        // of the `AddAgent` collision loop — so the added persona is always
        // valid against the stage's staffing.
        let mut state = standard_blueprint_state();
        let index = 2;
        let mut blueprint = state.blueprint.as_ref().expect("blueprint loaded").as_ref().clone();
        // Staff the stage with the would-be `implement_fallback` id.
        blueprint.pipeline.stages[index].agents.push("implement_fallback".into());
        state.blueprint = Some(Arc::new(blueprint));

        let _ = state.update(StudioMessage::FallbackAdded(index));
        let fallback = fallback_of(&state, index);
        assert_eq!(fallback.id, "implement_fallback_1");
        assert_ne!(
            fallback.id, FALLBACK_SENTINEL_ID,
            "even a suffixed id never synthesizes the engine-owned sentinel"
        );
        assert!(state.validation().ok);
    }

    #[test]
    fn fallback_added_never_touches_the_engine_sentinel_persona() {
        // The sentinel (`coordinator-self-execute`) is engine-owned and
        // rendered read-only (Batch 3 fallback card); the Add affordance is
        // only offered on stages WITHOUT a fallback, but a stale/duplicate
        // message must still never overwrite an existing sentinel persona.
        // No named blueprint ships the sentinel (it is an engine default,
        // `coordinator_self_implement_fallback`, blueprint.rs ~944-957), so
        // it is installed on a stage explicitly to pin the invariant.
        let sentinel = concerto_config::coordinator_self_implement_fallback();
        assert_eq!(sentinel.id, FALLBACK_SENTINEL_ID, "precondition");

        let mut blueprint =
            concerto_config::named_blueprint("standard").expect("standard blueprint exists");
        let stage = &mut blueprint.pipeline.stages[3]; // `review`
        stage.fallback = Some(sentinel);

        let state = State {
            orchestration: Some(OrchestrationConfig::default()),
            blueprint: Some(Arc::new(blueprint)),
            ..Default::default()
        };
        let mut state = state;
        let _ = state.update(StudioMessage::FallbackAdded(3));
        let fallback = fallback_of(&state, 3);
        assert_eq!(fallback.id, FALLBACK_SENTINEL_ID, "sentinel id stays untouched");
        assert!(state.validation().ok);
    }

    #[test]
    fn empty_registry_caption_branch_renders_without_panicking() {
        // Slice 4a (spec §3): an EMPTY relationship registry exercises the
        // caption branch of `relationships_view`, whose wording mirrors the
        // resolution seam (blueprint.rs:649-657 — an empty registry falls back
        // to the engine's five standard rows at resolve time). The standard
        // blueprint ships non-empty relationships, so this shape is otherwise
        // unreachable in the smoke suite.
        let mut blueprint =
            concerto_config::named_blueprint("standard").expect("standard blueprint exists");
        blueprint.relationships.clear();
        assert!(blueprint.relationships.is_empty(), "precondition");

        let empty = State {
            orchestration: Some(OrchestrationConfig::default()),
            blueprint: Some(Arc::new(blueprint)),
            ..Default::default()
        };
        let theme = AppTheme::by_name("Midnight");
        let _ = empty.view(&theme);
    }

    #[test]
    fn modified_indicator_renders_only_while_unsaved() {
        // Slice 4b (UX spec §8 defect 3): the toolbar "Modified" caption must
        // track the dirty flag exactly — rendered while `unsaved`, gone after
        // `mark_saved()`. Blueprint edits deliberately never mark the studio
        // dirty, so the dirty flag is set explicitly (the caption is a
        // function of `unsaved`, not of the edit message; the edit message is
        // still fired to prove a real edit + the flag combine cleanly).
        let mut state = standard_blueprint_state();
        let _ = state.update(StudioMessage::StageLabelEdited(0, "planning".into()));
        state.unsaved = true;
        let theme = AppTheme::by_name("Midnight");
        assert!(
            State::modified_caption(state.unsaved, &theme).is_some(),
            "the caption must render while the studio is dirty"
        );
        let _ = state.view(&theme);

        state.mark_saved();
        assert!(
            State::modified_caption(state.unsaved, &theme).is_none(),
            "the caption must not render after saving"
        );
        let _ = state.view(&theme);
    }

    #[test]
    fn blueprint_path_renders_the_roster_library_and_drills_into_the_inspector() {
        // ADR-59 (rewritten) Slice 3: the library pane IS the roster editor on
        // the blueprint path — one surface for agent CRUD that persists
        // through the roster Save arm — so the old "library hidden" behavior
        // is gone. The workspace still defaults to the stage cards, but
        // selecting a roster agent drills into the per-agent inspector (whose
        // edits persist the same way). iced 0.14 elements are opaque in
        // headless tests — there is no text-extraction API (see
        // `modified_caption`) — so this test pins the branch condition that
        // drives both the library and the workspace routing
        // (`on_blueprint_path`, `selected_agent_id`) and renders each
        // combination without panicking.
        let blueprint = standard_blueprint_state();
        assert!(
            blueprint.on_blueprint_path(),
            "orchestration + blueprint present ⇒ blueprint path (roster library + stage cards)"
        );
        let theme = AppTheme::by_name("Midnight");
        let _ = blueprint.view(&theme);

        // Roster drill-down: the selected agent routes the workspace to the
        // per-agent inspector (the Slice 3 edit affordance).
        let mut selection = standard_blueprint_state();
        selection.selected_agent_id = Some("coder".to_string());
        assert!(selection.on_blueprint_path());
        let _ = selection.view(&theme);

        // The legacy path (no `[orchestration]`) keeps the library and the
        // legacy actions exactly as today.
        let legacy = State::new();
        assert!(
            !legacy.on_blueprint_path(),
            "no orchestration ⇒ legacy path (library and actions rendered)"
        );
        let _ = legacy.view(&theme);
    }
}
