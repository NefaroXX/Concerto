use concerto_api_types::extension::McpToolDescriptor;

use super::{
    CreateParentOption, PolicyActionChoice, PolicyConditionChoice, WorkingDirBehaviorChoice,
};

#[derive(Debug, Clone)]
pub enum Message {
    ThemeSelected(&'static str),
    FontSizeChanged(f32),
    /// Toggle reduced-motion (`display.reduced_motion`): skips scan-line
    /// pulse, first-token emphasis, handoff hold, and line wipe.
    ReducedMotionToggled(bool),
    /// Toggle the scan-line overlay (`display.scanline_overlay_enabled`).
    ScanlineOverlayToggled(bool),

    // Legacy single-provider messages (kept for backward compat)
    ProviderSelected(&'static str),
    ModelChanged(String),
    ApiBaseChanged(String),
    ProviderApiKeyChanged(String),
    SaveProviderKey,
    ClearProviderKey,

    // Multi-provider management
    ProviderAddPressed,
    ProviderDeletePressed(usize),
    /// Second step of provider deletion: the user confirmed the prompt armed by
    /// `ProviderDeletePressed`. Performs the actual keyring + provider removal.
    ProviderDeleteConfirmed(usize),
    /// Cancel an armed provider-deletion prompt without deleting anything.
    ProviderDeleteCancelled(usize),
    FormProviderTypeChanged(String),
    FormNameChanged(String),
    FormApiBaseChanged(String),
    FormApiKeyChanged(String),
    FormEditKeyPressed(usize),
    FormKeyEditTextChanged(String),
    FormSaveKey(usize),
    FormClearKey(usize),
    FormClearKeyConfirmed(usize),
    FormKeyEditCancel(usize),
    FormConfirmAdd,
    FormCancel,

    // Phase 3 — model discovery
    /// User pressed a provider row's "Refresh" model-list control. The App
    /// layer intercepts this: it owns the request bookkeeping
    /// (`pending_refresh`) and spawns the async discovery fetch.
    ProviderModelsRefreshRequested(String),
    /// Discovery result for a saved provider, produced by the app's async task.
    ProviderModelsRefreshed {
        provider_id: String,
        request_id: u64,
        result: Result<Vec<String>, String>,
    },

    // Global default model — single unified picker for single-agent mode + fallback
    GlobalDefaultModelChanged(Option<String>),

    RelationshipFromChanged(&'static str),
    RelationshipToChanged(&'static str),
    RelationshipTypeChanged(&'static str),
    RelationshipCyclesChanged(String),
    RelationshipAdded,
    RelationshipRemoved(usize),

    // Policy (kept)
    NewPolicyActionSelected(PolicyActionChoice),
    NewPolicyConditionKindSelected(PolicyConditionChoice),
    NewPolicyToolSelected(&'static str),
    NewPolicyOperationSelected(&'static str),
    NewPolicyConditionValueChanged(String),
    PolicyRuleAdded,
    PolicyRuleRemoved(usize),
    PolicyRuleMovedUp(usize),
    PolicyRuleMovedDown(usize),

    // Memory (kept)
    MemoryEnabledToggled(bool),
    MemoryTtlChanged(f32),

    // Provider retry and recovery
    RetryEnabledToggled(bool),
    RetryInitialDelayChanged(f32),
    RetryMaxDelayChanged(f32),
    RetryMultiplierChanged(f32),
    RetryFixedDelayChanged(String),
    RetryRespectAfterToggled(bool),
    RetryJitterToggled(bool),
    RetryMaxElapsedChanged(String),

    // ADR-28 — Shell profiles and integrated toolchain
    /// Change the shell used by agents, validation, and the terminal.
    ShellActiveProfileChanged(String),
    /// Select a profile in the editor list.
    ShellProfileSelected(usize),
    ShellProfileExecutableChanged(String),
    ShellProfileArgsChanged(String),
    ShellProfileEnvKeyChanged(usize, String),
    ShellProfileEnvValueChanged(usize, String),
    ShellProfileAddEnv,
    ShellProfileRemoveEnv(usize),
    ShellNewEnvKeyChanged(String),
    ShellNewEnvValueChanged(String),
    ShellProfilePathAddChanged(String),
    ShellProfileWorkingDirChanged(WorkingDirBehaviorChoice),
    ShellProfileLoginToggled(bool),
    ShellProfileInteractiveToggled(bool),
    ShellProfileStartupChanged(String),
    ShellProfileAdd,
    ShellProfileRemove(usize),
    /// Run an availability check for the profile at `index`.
    ShellProfileTest(usize),
    ShellProfileTestResult {
        index: usize,
        available: bool,
        detail: String,
    },

    // ADR-28 Slice 2 — Managed Bash runtime management
    /// Source bash path for the (adopt) install action.
    ShellManagedSourceChanged(String),
    /// Export destination path for the runtime manifest.
    ShellManagedExportPathChanged(String),
    /// Import source path for a runtime manifest.
    ShellManagedImportPathChanged(String),
    /// Install a managed Bash by adopting the configured source path.
    ShellManagedInstall,
    /// Remove the installed managed runtime.
    ShellManagedRemove,
    /// Verify integrity of the installed runtime + tools.
    ShellManagedVerify,
    /// Export the current runtime manifest to a file.
    ShellManagedExport,
    /// Import a runtime manifest from a file.
    ShellManagedImport,
    /// Transient result of a managed-runtime action (install/remove/verify/…).
    ShellManagedResult(String),

    // ADR-37 — Plugin grant lifecycle management
    /// Request to revoke a plugin's capability grants.
    PluginRevokePressed(String),
    /// Result of a revoke action: a human-readable outcome line on success, or
    /// an error message on failure.
    PluginRevokeResult(Result<String, String>),

    // ADR-37 — Plugin install / remove (Settings → Plugins)
    /// Change the typed source path in the plugin install card.
    PluginInstallPathChanged(String),
    /// Open the native file picker for a `.wasm` plugin.
    PluginInstallBrowsePressed,
    /// Result of the install file picker (`None` = user cancelled).
    PluginBrowsePicked(Option<String>),
    /// Install (or replace) the plugin at the current source path.
    PluginInstallPressed,
    /// Result of an install: a human-readable outcome line on success, or an
    /// error message on failure.
    PluginInstallResult(Result<String, String>),
    /// Replace the selected plugin: opens the file picker for a new `.wasm`.
    PluginReplacePressed,
    /// Result of the replace file picker (`None` = user cancelled; a picked
    /// path immediately starts a replace install).
    PluginReplacePicked(Option<String>),
    /// Delete the plugin with this id: arms the inline confirm prompt.
    PluginDeletePressed(String),
    /// Cancel an armed delete prompt without deleting anything.
    PluginDeleteCancelled(String),
    /// Confirm deletion: revoke grants, unload live state, and remove the
    /// plugin's files.
    PluginDeleteConfirmed(String),
    /// Result of a delete: a human-readable outcome line on success, or an
    /// error message on failure.
    PluginDeleteResult(Result<String, String>),
    /// Re-scan the canonical plugins directory and refresh the installed list.
    PluginListRefreshRequested,
    /// Result of the directory re-scan: the installed plugins (with their
    /// capability-grant summaries), or a human-readable error.
    PluginListRefreshResult(Result<Vec<super::InstalledPluginInfo>, String>),

    // ADR-43 — Skills and MCP extension configuration
    /// Toggle the master skills enable flag (`skills.enabled`).
    SkillsEnabledToggled(bool),
    /// Toggle one skill in the enabled allow-list (`skills.enabled_ids`).
    SkillTogglePressed(String, bool),
    /// Expand/collapse one discovered skill's instruction preview.
    SkillExpandToggled(String),
    /// Run a skill discovery pass (Refresh button, or lazily on page open).
    SkillsDiscoveryRequested,
    /// Result of a discovery run: the report (found packs plus per-path
    /// diagnostics and warnings), or a human-readable error.
    SkillsDiscoveryResult(Result<concerto_skills::DiscoveryReport, String>),
    /// Open the skill-pack create wizard (ADR-43). Transient view state: it
    /// never arms the dirty flag and the new pack only lands on disk once
    /// `SkillCreateConfirmed` validates it.
    SkillCreatePressed,
    /// Change the wizard's new-pack id field.
    SkillCreateIdChanged(String),
    /// Change the wizard's new-pack display-name field.
    SkillCreateNameChanged(String),
    /// Change the wizard's new-pack version field.
    SkillCreateVersionChanged(String),
    /// Change the wizard's new-pack description field.
    SkillCreateDescriptionChanged(String),
    /// Edit the wizard's inline instructions body.
    SkillCreateInstructionsChanged(iced::widget::text_editor::Action),
    /// Pick the wizard's parent directory (one of the configured search
    /// paths, shown with an existence badge).
    SkillCreateParentChanged(CreateParentOption),
    /// Validate the wizard and create the pack on disk; a success triggers a
    /// discovery refresh so the new pack appears immediately.
    SkillCreateConfirmed,
    /// Discard the create wizard without writing anything.
    SkillCreateCancelled,
    /// Result of the create operation: a human-readable outcome line, or an
    /// error (the wizard stays open for correction).
    SkillCreateResult(Result<String, String>),
    /// Enter edit mode for one discovered skill, seeding a draft from its
    /// descriptor. Transient until saved; never arms the dirty flag.
    SkillEditPressed(String),
    /// Change the edit draft's display-name field.
    SkillEditNameChanged(String),
    /// Change the edit draft's description field.
    SkillEditDescriptionChanged(String),
    /// Edit the draft's inline instructions body.
    SkillEditInstructionsChanged(iced::widget::text_editor::Action),
    /// Write the edit draft back to the pack's `skill.toml` (name,
    /// description, and inline instructions; version/tools/resources are
    /// preserved). A success triggers a discovery refresh.
    SkillEditSaved,
    /// Discard the edit draft and leave edit mode.
    SkillEditCancelled,
    /// Result of the edit operation: a human-readable outcome line, or an
    /// error (edit mode stays open for correction).
    SkillEditResult(Result<String, String>),
    /// Press Delete on a skill: arms the inline confirm prompt (deletion is
    /// destructive, so the first press only confirms intent).
    SkillDeletePressed(String),
    /// Cancel an armed skill delete prompt.
    SkillDeleteCancelled(String),
    /// Confirm deletion of the armed skill pack. The manifest files are moved
    /// to hidden `.deleted-*` backups in place (reversible); the list refreshes
    /// afterwards.
    SkillDeleteConfirmed(String),
    /// Result of the delete operation: a human-readable outcome line, or an
    /// error.
    SkillDeleteResult(Result<String, String>),
    /// Toggle the master MCP enable flag (`mcp.enabled`).
    McpEnabledToggled(bool),
    /// Toggle one MCP server (`mcp.servers[].enabled`).
    McpServerEnabledToggled(String, bool),
    /// Probe one MCP server (spawn + initialize + list tools + stop).
    McpProbePressed(String),
    /// Result of an MCP probe, keyed by server id.
    McpProbeResult(String, Result<Vec<McpToolDescriptor>, String>),
    /// Enter edit mode for one MCP server, seeding a draft from its config.
    /// Transient until saved; never arms the dirty flag by itself.
    McpEditPressed(String),
    /// Discard the in-progress MCP server edit draft and leave edit mode.
    McpEditCancelled,
    /// Change the command field of the in-progress MCP server edit draft.
    McpEditCommandChanged(String),
    /// Change the (space-joined) arguments field of the edit draft.
    McpEditArgsChanged(String),
    /// Change one environment-variable key row of the edit draft.
    McpEditEnvKeyChanged(usize, String),
    /// Change one environment-variable value row of the edit draft.
    McpEditEnvValueChanged(usize, String),
    /// Add an empty environment-variable row to the edit draft.
    McpEditEnvAdd,
    /// Remove an environment-variable row from the edit draft.
    McpEditEnvRemove(usize),
    /// Change the per-call timeout field of the edit draft.
    McpEditTimeoutChanged(String),
    /// Validate and apply the edit draft to its server. Leaves edit mode on
    /// success and arms the dirty flag; stays with inline field errors when
    /// the draft is invalid.
    McpEditSaved,
    /// Press Delete on a server: arms the inline confirm prompt (deletion is
    /// destructive, so the first press only confirms intent).
    McpDeletePressed(String),
    /// Cancel an armed MCP server delete prompt.
    McpDeleteCancelled(String),
    /// Confirm deletion of the armed MCP server (removes it from the pending
    /// config; persisted on Save Settings).
    McpDeleteConfirmed(String),
    /// Open the add-server form in the detail pane, seeding a fresh blank
    /// draft. Transient view state: nothing here arms the dirty flag.
    McpAddPressed,
    /// Discard the in-progress MCP server add draft and leave the add form.
    McpAddCancelled,
    /// Change the id field of the in-progress MCP server add draft.
    McpAddIdChanged(String),
    /// Change the command field of the in-progress MCP server add draft.
    McpAddCommandChanged(String),
    /// Change the (space-joined) arguments field of the add draft.
    McpAddArgsChanged(String),
    /// Change one environment-variable key row of the add draft.
    McpAddEnvKeyChanged(usize, String),
    /// Change one environment-variable value row of the add draft.
    McpAddEnvValueChanged(usize, String),
    /// Add an empty environment-variable row to the add draft.
    McpAddEnvAdd,
    /// Remove an environment-variable row from the add draft.
    McpAddEnvRemove(usize),
    /// Change the per-call timeout field of the add draft.
    McpAddTimeoutChanged(String),
    /// Validate and append the add draft as a new `mcp.servers[]` entry.
    /// Arms the dirty flag (persisted on Save Settings, next-run semantics)
    /// and selects the new server; stays in the add form with inline field
    /// errors when the draft is invalid.
    McpAddSaved,

    // Unified Extensions manager (ADR-37/43/70)
    /// Switch the active sub-tab of the Extensions section.
    ExtensionTabSelected(ExtensionTab),
    /// Select an item (skill / MCP server / plugin) in the Extensions master
    /// list so its metadata renders in the detail pane. Transient view state:
    /// selection never arms the dirty flag and is never persisted.
    ExtensionItemSelected(ExtensionTab, String),
    /// Toggle project AGENTS.md context injection (`project_context.enabled`).
    ProjectContextEnabledToggled(bool),
    /// Toggle the coordinator's advisory AGENTS.md-refresh nudge
    /// (`project_context.auto_update_agents_md`).
    ProjectContextNudgeToggled(bool),

    SaveSettings,
    /// Toggle a collapsible section open/closed.
    #[allow(private_interfaces)]
    ToggleSection(SectionId),
    /// Navigate to a section: expand it and scroll the main column to its
    /// header. Sent by the sidebar index (which navigates); the section headers
    /// keep sending [`Message::ToggleSection`], which only folds.
    #[allow(private_interfaces)]
    JumpToSection(SectionId),
}

/// Sub-tab of the unified Extensions manager (Skills / MCP servers / Plugins /
/// project context). Selection and the active tab are transient view state;
/// none of it is persisted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtensionTab {
    Skills,
    Mcp,
    Plugins,
    ProjectContext,
}

/// Identifies a collapsible section in the Settings view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum SectionId {
    Theme,
    Providers,
    Assignments,
    Relationships,
    Policy,
    Retry,
    Memory,
    Shell,
    /// Unified Extensions hub (ADR-37/43/70): Skills, MCP servers, Plugins,
    /// and project context share one collapsible section with sub-tabs.
    Extensions,
}

impl SectionId {
    /// Every section in the order it renders in the main column (and in the
    /// sidebar index). This canonical order is what a sidebar jump uses to
    /// derive a scroll position; it is intentionally independent of whether
    /// the Relationships section is currently hidden (`[orchestration]`-gated),
    /// because the resulting fractional offset stays within one section height
    /// either way.
    pub(crate) const ALL: [SectionId; 9] = [
        SectionId::Theme,
        SectionId::Providers,
        SectionId::Assignments,
        SectionId::Policy,
        SectionId::Relationships,
        SectionId::Retry,
        SectionId::Memory,
        SectionId::Shell,
        SectionId::Extensions,
    ];
}
