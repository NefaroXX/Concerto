//! Agency envelope, resume-scope, multi-agent history, and topology/roster
//! coverage for `runtime_runner_impl`.
//!
//! Mechanical extraction (NORM S24E): the ADR-55 envelope + acceptance banner,
//! the "Full local agency" banner, and the eleven tests from
//! `full_agency_envelope_is_the_only_mode` through
//! `topology_roles_excludes_disabled` move verbatim out of
//! `runtime_runner::runtime_runner_tests`, so test names and assertions are
//! unchanged. The span carries no fixtures — every helper it calls
//! (`dispatches_to_coordinator`, `tool_support_override`,
//! `resume_scope_project_id`, `multi_agent_task_with_history`,
//! `topology_roles`, `build_agent_config_map`) is production code in
//! `runtime_runner_impl` — so nothing is shared with, or duplicated into, the
//! donor module. `use super::super::*;` keeps the parent
//! (`runtime_runner_impl`) items in scope.

use super::super::*;

// ===========================================================================
// ADR-55 §2: envelope + acceptance tests (archived acceptance ids).
// ===========================================================================

// ===========================================================================
// Full local agency: routing is no longer control flow.
// ===========================================================================

/// Every run enters the unified loop with full local agency — the
/// `Acting` envelope — and the coordinator is engaged for every non-forced
/// run. No keyword selects a read-only or tool-less path.
#[test]
fn full_agency_envelope_is_the_only_mode() {
    assert_eq!(RunEnvelope::Acting, RunEnvelope::Acting);
    assert!(dispatches_to_coordinator(false), "multi mode engages the coordinator");
    assert!(!dispatches_to_coordinator(true), "force_single_agent stays on the single-agent loop");
}

/// Negation arrives as ordinary language the model understands: there is no
/// veto list that flips the run's grants. The run remains fully agentic
/// (`Acting`) regardless of how the utterance reads; deny-class policy
/// rules and the approval sink remain the boundaries.
#[test]
fn negation_is_language_not_a_grant_flip() {
    // The deprecated router may still classify the phrase, but nothing in
    // production consults it: the envelope is Acting for every input.
    assert_eq!(RunEnvelope::Acting, RunEnvelope::Acting);
}

/// ADR-66 §2(a) + §4: the selection gate refuses a tool-requiring run
/// only when no tool path exists at all — plugin-backed providers
/// (decision (a)). The Zen Responses-dialect models now carry native
/// tools (the converter was completed, ADR-75), so they pass on native
/// support rather than via the fallback driver. Precedence level 1 (the
/// explicit-config override) is unchanged.
#[test]
fn selection_gate_refuses_only_uncoverable_tool_gaps() {
    // Responses-dialect models on Zen now have native tool support.
    assert!(concerto_providers::capability::require_tool_support_with_fallback(
        "opencode",
        "muse-spark-1.3-contributor-free",
        None,
        None,
    )
    .is_ok());
    assert!(concerto_providers::capability::require_tool_support_with_fallback(
        "opencode", "muse-v2", None, None
    )
    .is_ok());
    // Advertised absence still proceeds via the coverable fallback driver
    // (not refused, not forced native).
    assert!(concerto_providers::capability::require_tool_support_with_fallback(
        "openai",
        "gpt-4o",
        None,
        Some(false),
    )
    .is_ok());
    // Plugin-backed gap: refused, naming everything (decision (a)).
    let error = concerto_providers::capability::require_tool_support_with_fallback(
        "plugin:my-llm",
        "any",
        None,
        None,
    )
    .expect_err("plugin providers stay hard-gated for tool tasks");
    match &error {
        ProviderError::CapabilityRefused { provider, model, capability } => {
            assert_eq!(provider, "plugin:my-llm");
            assert_eq!(model, "any");
            assert_eq!(capability, "tool_calling");
        }
        other => panic!("expected CapabilityRefused, got: {other:?}"),
    }
    // The refusal is permanent: retrying cannot add the capability.
    assert!(!error.is_transient());
    // Inverted from the old assertion: the pure §2(a) primitive no longer
    // refuses the Responses-dialect models — no name decides capability.
    assert!(concerto_providers::capability::require_tool_support(
        "opencode",
        "muse-spark-1.3-contributor-free",
        None,
        None,
    )
    .is_ok());
}

/// ADR-66 §3 precedence level 1: `tool_support_override` reads the
/// `[model_profiles.<id>]` explicit override for the resolved provider
/// config only.
#[test]
fn tool_support_override_reads_the_resolved_config_override() {
    let mut config = AppConfig::default();
    assert_eq!(tool_support_override(&config, None), None, "no config id → no override");

    let settings = concerto_config::ModelSettings {
        providers: vec![concerto_config::ProviderConfig {
            id: "zen-muse".into(),
            provider: "opencode".into(),
            model: "muse-v2".into(),
            ..Default::default()
        }],
        model_profile_overrides: std::iter::once((
            "zen-muse".to_string(),
            concerto_config::ModelProfileOverride {
                supports_tool_calling: Some(true),
                ..Default::default()
            },
        ))
        .collect(),
        ..Default::default()
    };
    config.model_settings = Some(settings);

    assert_eq!(
        tool_support_override(&config, Some("zen-muse")),
        Some(true),
        "explicit override must be surfaced for the resolved config"
    );
    assert_eq!(
        tool_support_override(&config, Some("unrelated-id")),
        None,
        "unrelated configs have no override"
    );
}

/// ADR-60 D7 (interrupt-safe resume): the resume scope check must use the
/// checkpoint's own project-id definition. A coordinator-written row
/// (project_id = `ProjectId::resolve`) validated against the unrelated
/// path-hash definition never matched, so every real checkpoint was
/// discarded with "belongs to a different project" and cleared on
/// resume — the only resumable state a graceful interrupt left never
/// survived.
#[test]
fn resume_scope_uses_the_checkpoint_project_id_definition() {
    let dir = tempfile::tempdir().expect("tempdir");
    let written = concerto_core::types::ProjectId::resolve(dir.path()).0;
    assert_eq!(
        resume_scope_project_id(dir.path()),
        written,
        "the resume scope must equal the project id the coordinator writes"
    );
    // The regression pin: the old definition is a DIFFERENT hash and
    // could never validate a production row.
    assert_ne!(
        concerto_core::helpers::project_id_hash(dir.path()),
        written,
        "the path-hash definition must stay distinct from the checkpoint's"
    );
    // A different project dir resolves to a different scope (the check
    // still rejects genuinely foreign checkpoints).
    let other = tempfile::tempdir().expect("tempdir");
    assert_ne!(
        resume_scope_project_id(dir.path()),
        resume_scope_project_id(other.path()),
        "distinct project dirs must remain distinct scopes"
    );
}

#[test]
fn multi_agent_task_includes_persisted_conversation_history() {
    let session_id = Ulid::new();
    let task = AgentTask::new_action_required(session_id, "apply the fix");
    let history = vec![
        Message {
            role: Role::User,
            content: "do not change the public API".into(),
            tool_calls: None,
            tool_results: None,
            reasoning_content: None,
            tokens_in: None,
            tokens_out: None,
        },
        Message {
            role: Role::Assistant,
            content: "understood".into(),
            tool_calls: None,
            tool_results: None,
            reasoning_content: None,
            tokens_in: None,
            tokens_out: None,
        },
    ];

    let enriched = multi_agent_task_with_history(task, &history);

    assert!(enriched.description.starts_with("apply the fix"));
    assert!(enriched.description.contains("[user]\ndo not change the public API"));
    assert!(enriched.description.contains("[assistant]\nunderstood"));
}

#[test]
fn multi_agent_task_is_unchanged_without_history() {
    let task = AgentTask::new_action_required(Ulid::new(), "apply the fix");
    let enriched = multi_agent_task_with_history(task.clone(), &[]);
    assert_eq!(enriched.description, task.description);
}

// ------------------------------------------------------------------
// topology_roles (ADR-35 phase 4)
// ------------------------------------------------------------------

#[test]
fn topology_roles_includes_coordinator_builtins_and_customs() {
    let multi_agent = concerto_config::MultiAgentConfig {
        custom_agents: vec![
            concerto_config::CustomAgentConfig {
                id: "docs-writer".into(),
                name: "Docs Writer".into(),
                role: "docs-writer".into(),
                ..Default::default()
            },
            concerto_config::CustomAgentConfig {
                id: "copilot".into(),
                name: "Copilot".into(),
                role: "copilot".into(),
                ..Default::default()
            },
        ],
        ..Default::default()
    };

    let roles = topology_roles(&Some(multi_agent));

    assert_eq!(
        roles,
        vec![
            AgentId::new("coordinator"),
            AgentId::new("architect"),
            AgentId::new("researcher"),
            AgentId::new("coder"),
            AgentId::new("reviewer"),
            AgentId::new("validator"),
            AgentId::new("docs-writer"),
            AgentId::new("copilot"),
        ],
        "coordinator first, builtins in fixed order, custom agents last"
    );
}

/// The per-agent files are the single source of truth at the runtime
/// boundary too: a file-backed config (`agent_files_authoritative`) owns
/// the roster, so `merge_seeds` is false and `build_agent_config_map`
/// yields exactly the file roster — a deleted builtin is not resurrected.
#[test]
fn file_backed_roster_is_authoritative_for_runtime_role_resolution() {
    use concerto_config::{AppConfig, CustomAgentConfig, MultiAgentConfig};

    let config = AppConfig {
        agent_files_authoritative: true,
        multi_agent: Some(MultiAgentConfig {
            custom_agents: vec![CustomAgentConfig {
                id: "coder".into(),
                name: "Coder".into(),
                role: "coder".into(),
                ..Default::default()
            }],
            ..Default::default()
        }),
        ..Default::default()
    };

    assert!(config.owns_agent_roster(), "the file-backed roster is authoritative");
    // Mirrors the call-site expression: `merge_seeds = !owns_agent_roster()`.
    let merge_seeds = !config.owns_agent_roster();
    assert!(!merge_seeds, "no builtin seed is merged back over a file roster");

    let map = build_agent_config_map(&config.multi_agent);
    assert_eq!(map.len(), 1, "the file roster is exactly the runtime config map");
    assert!(map.contains_key(&AgentId::new("coder")));
    assert!(
        !map.contains_key(&AgentId::new("architect")),
        "a builtin missing from the files stays deleted at runtime"
    );
}

/// An initialized-but-empty file roster (every agent deleted) still owns
/// the runtime roster: no seed resurrection.
#[test]
fn empty_file_roster_owns_the_runtime_roster() {
    use concerto_config::AppConfig;

    let config = AppConfig {
        agent_files_authoritative: true,
        multi_agent: Some(concerto_config::MultiAgentConfig::default()),
        ..Default::default()
    };

    assert!(config.owns_agent_roster());
    let map = build_agent_config_map(&config.multi_agent);
    assert!(map.is_empty(), "an empty file roster registers no specialists from config");
}

#[test]
fn topology_roles_excludes_disabled() {
    let multi_agent = concerto_config::MultiAgentConfig {
        custom_agents: vec![
            // Disabled built-in (reviewer): omitted from the builtin pass
            // and the custom pass alike.
            concerto_config::CustomAgentConfig {
                id: "reviewer".into(),
                name: "Reviewer".into(),
                role: "reviewer".into(),
                disabled: true,
                ..Default::default()
            },
            // Disabled custom agent: omitted.
            concerto_config::CustomAgentConfig {
                id: "docs-writer".into(),
                name: "Docs Writer".into(),
                role: "docs-writer".into(),
                disabled: true,
                ..Default::default()
            },
            // Enabled custom agent: still present.
            concerto_config::CustomAgentConfig {
                id: "copilot".into(),
                name: "Copilot".into(),
                role: "copilot".into(),
                ..Default::default()
            },
        ],
        ..Default::default()
    };

    let roles = topology_roles(&Some(multi_agent));

    assert!(
        !roles.contains(&AgentId::new("reviewer")),
        "disabled built-in must be omitted from the topology"
    );
    assert!(
        !roles.contains(&AgentId::new("docs-writer")),
        "disabled custom agent must be omitted from the topology"
    );
    assert!(roles.contains(&AgentId::new("copilot")));
    assert_eq!(roles.first(), Some(&AgentId::new("coordinator")));
    // Deterministic order: coordinator, then builtins, then customs.
    let builtins = ["architect", "researcher", "coder", "validator"];
    let builtin_positions: Vec<usize> = builtins
        .iter()
        .map(|name| roles.iter().position(|r| r.as_str() == *name).unwrap())
        .collect();
    assert_eq!(
        builtin_positions,
        vec![1, 2, 3, 4],
        "builtins keep their fixed order after the coordinator"
    );
    assert_eq!(
        roles.last(),
        Some(&AgentId::new("copilot")),
        "custom agents come last, in config order"
    );
}
