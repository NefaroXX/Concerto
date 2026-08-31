use concerto_api_types::extension::{McpToolDescriptor, SkillDescriptor};

use super::{PolicyActionChoice, PolicyConditionChoice, WorkingDirBehaviorChoice};

#[derive(Debug, Clone)]
pub enum Message {
    ThemeSelected(&'static str),
    FontSizeChanged(f32),

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

    // ADR-43 — Skills and MCP extension configuration
    /// Toggle the master skills enable flag (`skills.enabled`).
    SkillsEnabledToggled(bool),
    /// Toggle one skill in the enabled allow-list (`skills.enabled_ids`).
    SkillTogglePressed(String, bool),
    /// Expand/collapse one discovered skill's instruction preview.
    SkillExpandToggled(String),
    /// Run a skill discovery pass (Refresh button, or lazily on page open).
    SkillsDiscoveryRequested,
    /// Result of a discovery run: the found packs, or a human-readable error.
    SkillsDiscoveryResult(Result<Vec<SkillDescriptor>, String>),
    /// Toggle the master MCP enable flag (`mcp.enabled`).
    McpEnabledToggled(bool),
    /// Toggle one MCP server (`mcp.servers[].enabled`).
    McpServerEnabledToggled(String, bool),
    /// Probe one MCP server (spawn + initialize + list tools + stop).
    McpProbePressed(String),
    /// Result of an MCP probe, keyed by server id.
    McpProbeResult(String, Result<Vec<McpToolDescriptor>, String>),

    SaveSettings,
    /// Toggle a collapsible section open/closed.
    #[allow(private_interfaces)]
    ToggleSection(SectionId),
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
    Plugins,
    Skills,
    Mcp,
}
