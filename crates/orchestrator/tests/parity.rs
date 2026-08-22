//! Frontend parity contract — CLI and Desktop.
//!
//! Both frontends are thin UI layers over the same core: they share
//! `run_shared_agent` / `ServicesBuilder` / `RequestBuilder` from this crate.
//! This file pins the behavioural contracts that must not drift:
//!
//! 1. **History loading** — both frontends load session history through
//!    `ProjectSessionManager::load_recent_messages`; no frontend-specific
//!    slicing or capping is allowed.
//! 2. **Approval semantics** — approval is per-call prompting unless the user
//!    explicitly opts into session-wide auto-approval ("approve all for
//!    session" / "Always allow"). A plain single grant never auto-approves
//!    later calls, even for the same tool. The sink-level behavior tests live
//!    in each frontend's crate: `crates/cli/src/approval.rs` and the
//!    `DesktopApprovalSink` tests in `crates/desktop/src/app.rs`.
//! 3. **Memory retrieval** — memory is retrieved for a run exactly when
//!    `memory_enabled(fast, configured)` is true: CLI `-f/--fast` and the
//!    desktop "Fast mode" toggle both disable retrieval while leaving the
//!    configured `[memory] enabled` flag untouched. Both frontends must call
//!    the shared helper (never inline `!fast && configured`).

use std::collections::HashMap;
use std::sync::Arc;

use concerto_core::ids::Ulid;
use concerto_core::types::{Message, Role};
use concerto_orchestrator::runtime_runner::memory_enabled;
use concerto_orchestrator::session_manager::ProjectSessionManager;
use concerto_sessions::{SessionStore, SqliteSessionStore};
use tokio_util::sync::CancellationToken;

/// Pin the CLI/desktop-shared memory-enabled contract. `memory_enabled` is the
/// single source of truth for both frontends' `with_memory_enabled` wiring, so
/// fast mode must disable retrieval regardless of the configured flag.
#[test]
fn memory_enabled_contract() {
    // Fast mode always disables retrieval, even when memory is configured on.
    assert!(!memory_enabled(true, true), "fast must disable configured memory");
    assert!(!memory_enabled(true, false), "fast with memory off stays off");
    // Without fast mode the configured flag decides.
    assert!(memory_enabled(false, true), "normal mode honours configured memory");
    assert!(!memory_enabled(false, false), "memory off stays off in normal mode");
}

/// Create a session with `count` messages of alternating user/assistant roles.
async fn create_session_with_messages(
    store: &SqliteSessionStore,
    count: usize,
) -> (Ulid, Vec<Message>) {
    let project_dir = camino::Utf8PathBuf::from("/test/project");
    let session = store
        .create_session(&project_dir, "test-provider", "test-model", CancellationToken::new())
        .await
        .expect("create_session should succeed");

    let mut messages: Vec<Message> = Vec::with_capacity(count);
    for i in 0..count {
        let role = if i % 2 == 0 { Role::User } else { Role::Assistant };
        messages.push(Message {
            role,
            content: format!(
                "Message number {i} with some padding to make it realistic: {}",
                "x".repeat(50)
            ),
            tool_calls: None,
            tool_results: None,
            reasoning_content: None,
            tokens_in: None,
            tokens_out: None,
        });
    }

    store
        .append_messages(session.id, &messages, CancellationToken::new())
        .await
        .expect("append_messages should succeed");

    (session.id, messages)
}

#[tokio::test]
async fn both_frontends_load_identical_history() {
    let store = SqliteSessionStore::connect_in_memory().await.unwrap();
    let (session_id, original_messages) = create_session_with_messages(&store, 50).await;

    let manager = ProjectSessionManager::from_store(Arc::new(store));

    let loaded = manager
        .load_recent_messages(session_id, CancellationToken::new())
        .await
        .expect("load_recent_messages should succeed");

    // Both frontends load through the same path, so the result is identical
    // to what was stored (no cap, no frontend-specific slicing).
    assert_eq!(
        loaded.len(),
        original_messages.len(),
        "load_recent_messages should return all messages (no silent cap)"
    );

    // Verify content integrity — every original message is present and in order.
    for (original, loaded) in original_messages.iter().zip(loaded.iter()) {
        assert_eq!(original.role, loaded.role);
        assert_eq!(original.content, loaded.content);
    }
}

#[tokio::test]
async fn empty_session_returns_empty_history() {
    let store = SqliteSessionStore::connect_in_memory().await.unwrap();
    let project_dir = camino::Utf8PathBuf::from("/test/project");
    let session = store
        .create_session(&project_dir, "test-provider", "test-model", CancellationToken::new())
        .await
        .expect("create_session should succeed");

    let manager = ProjectSessionManager::from_store(Arc::new(store));

    let loaded = manager
        .load_recent_messages(session.id, CancellationToken::new())
        .await
        .expect("load_recent_messages should succeed");

    assert!(loaded.is_empty(), "new session should have no messages");
}

#[tokio::test]
async fn large_session_does_not_crash() {
    let store = SqliteSessionStore::connect_in_memory().await.unwrap();
    let (session_id, original_messages) = create_session_with_messages(&store, 5000).await;

    let manager = ProjectSessionManager::from_store(Arc::new(store));

    let loaded = manager
        .load_recent_messages(session_id, CancellationToken::new())
        .await
        .expect("load_recent_messages should succeed");

    assert_eq!(loaded.len(), original_messages.len());
}

/// ADR-58 P1 parity: the default (`standard`) blueprint, resolved through the
/// public `[orchestration]` seam, must equal the runtime's hardcoded pipeline
/// tables — stage write masks, per-agent resolved capabilities, collaboration
/// rows, feed bindings, and gate cycle caps. Every literal below is anchored
/// to its runtime source line so a refactor that changes a table fails this
/// test instead of silently drifting the blueprint from the engine.
#[test]
fn default_blueprint_resolves_to_runtime_tables() {
    use concerto_config::blueprint::{
        CapabilityMask, OrchestrationConfig, ResolvedStage, StageKind,
    };
    use concerto_config::{builtin_agent_seeds, AppConfig, CustomAgentConfig};
    use concerto_core::RunStage;
    use concerto_orchestrator::relationship::{default_collaboration_rules, AgentRelationship};

    // The default AppConfig carries no `[orchestration]` section (legacy
    // equivalence, ADR-58): the equivalent default state is the `standard`
    // named blueprint, which staffs exactly the five built-in seeds
    // (schema.rs `builtin_agent_seeds`, ADR-35 phase 4).
    let app_config = AppConfig::default();
    assert!(app_config.orchestration.is_none(), "no [orchestration] section -> legacy default");
    let seeds = builtin_agent_seeds();
    let seed_ids: Vec<&str> = seeds.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(seed_ids, vec!["architect", "researcher", "coder", "reviewer", "validator"]);

    let resolved = OrchestrationConfig::default()
        .resolve(&[], None)
        .expect("the standard blueprint must validate and resolve");
    assert_eq!(resolved.blueprint.name, "standard");

    let stage = |tag: &str| -> &ResolvedStage {
        resolved
            .stages
            .iter()
            .find(|s| s.def.tag == tag)
            .unwrap_or_else(|| panic!("no stage {tag:?} in the standard blueprint"))
    };

    // -- 1. Stage write masks ------------------------------------------------
    // The Execution-kind stage grants fs_write + shell (blueprint.rs
    // `StageKind::default_capability_mask`, ADR-58 D1); every other kind
    // grants neither.
    assert_eq!(
        stage("implement").effective_capabilities,
        CapabilityMask { fs_write: true, shell: true }
    );
    for tag in ["design", "research", "review", "validate"] {
        assert_eq!(
            stage(tag).effective_capabilities,
            CapabilityMask::default(),
            "stage {tag:?} must carry no write mask"
        );
    }

    // -- 2. Per-agent resolved capabilities ----------------------------------
    // An agent's resolved shape is its seed capabilities (`effective()`) with
    // the fs_write/shell flags overlaid by the staffing stage's write mask.
    // The coder therefore resolves to {fs_read:false, fs_write:true, shell:
    // true, git:false, lsp:false, eval:true}; the four non-write specialists
    // keep the seed defaults ({f,f,f,f,f,t}).
    let seed = |id: &str| -> &CustomAgentConfig {
        seeds.iter().find(|seed| seed.id == id).unwrap_or_else(|| panic!("no builtin seed {id:?}"))
    };
    let resolved_caps = |agent_id: &str| -> (bool, bool, bool, bool, bool, bool) {
        let caps = seed(agent_id).capabilities.effective();
        let mask = resolved
            .stages
            .iter()
            .find(|s| s.def.agents.iter().any(|a| a == agent_id))
            .map(|s| s.effective_capabilities)
            .unwrap_or_else(|| {
                panic!("agent {agent_id:?} is not staffed in the standard pipeline")
            });
        (caps.fs_read, mask.fs_write, mask.shell, caps.git, caps.lsp, caps.eval)
    };
    // coder: seed {false,false,false,false,false,true} overlaid with the
    // Execution mask {fs_write:true, shell:true}.
    assert_eq!(resolved_caps("coder"), (false, true, true, false, false, true));
    for agent in ["architect", "researcher", "reviewer", "validator"] {
        assert_eq!(resolved_caps(agent), (false, false, false, false, false, true), "{agent}");
    }

    // -- 3. Collaboration rows -----------------------------------------------
    // The blueprint's open relationship registry (from/to + kind string) is
    // the runtime default rule table as data rows, in the same order. Kinds
    // use the string vocabulary the runtime parses (`configured_relationship`,
    // runtime_runner.rs:86-94): "supervises" → Supervises,
    // "provides_context_to" → ProvidesContextTo, "owns_design" → OwnsDesign.
    let expected: Vec<(&str, &str, AgentRelationship, Option<u32>)> = vec![
        // relationship.rs:142-145 — reviewer→coder Supervises, cap 3.
        ("reviewer", "coder", AgentRelationship::Supervises, Some(3)),
        // relationship.rs:148-152 — researcher→coder ProvidesContextTo, no cap.
        ("researcher", "coder", AgentRelationship::ProvidesContextTo, None),
        // relationship.rs:153-158 — architect→coder OwnsDesign, no cap.
        ("architect", "coder", AgentRelationship::OwnsDesign, None),
        // relationship.rs:159-164 — architect→researcher OwnsDesign, no cap.
        ("architect", "researcher", AgentRelationship::OwnsDesign, None),
        // relationship.rs:165-170 — validator→coder Supervises, cap 2.
        ("validator", "coder", AgentRelationship::Supervises, Some(2)),
    ];
    let rules = default_collaboration_rules();
    assert_eq!(rules.len(), expected.len(), "runtime default rules changed shape");
    assert_eq!(resolved.relationship_defaults.len(), expected.len());

    let kind_to_relationship = |kind: &str| -> AgentRelationship {
        match kind {
            "supervises" => AgentRelationship::Supervises,
            "provides_context_to" => AgentRelationship::ProvidesContextTo,
            "owns_design" => AgentRelationship::OwnsDesign,
            other => panic!("unexpected relationship kind string {other:?}"),
        }
    };
    for (i, (expect_from, expect_to, expect_relationship, expect_cycles)) in
        expected.iter().enumerate()
    {
        let def = &resolved.relationship_defaults[i];
        assert_eq!(def.from, *expect_from, "row {i} from");
        assert_eq!(def.to, *expect_to, "row {i} to");
        assert_eq!(
            kind_to_relationship(&def.kind),
            *expect_relationship,
            "row {i} kind: {:?}",
            def.kind
        );
        // The blueprint row carries no cycle cap; the cap lives on the runtime
        // CollaborationRule and must be the documented value.
        let rule = &rules[i];
        assert_eq!(rule.from.as_str(), *expect_from, "row {i} runtime from");
        assert_eq!(rule.to.as_str(), *expect_to, "row {i} runtime to");
        assert_eq!(rule.relationship, *expect_relationship, "row {i} runtime relationship");
        assert_eq!(rule.max_cycles, *expect_cycles, "row {i} runtime max_cycles");
    }

    // -- 4. Feed bindings ----------------------------------------------------
    // Per-kind default feed table (blueprint §5.6): Research→Understand,
    // Planning→Plan, Execution→Execute, Review→Verify, Acceptance→Verify,
    // RunOnce→None. Emission anchors in the pre-blueprint runtime: run start
    // reports Understand (runtime_runner.rs:2618), plan-only outcomes report
    // Plan (runtime_runner.rs:1716), granted-execute reports Execute
    // (runtime_runner.rs:1714), and the validation cycle reports Verify
    // (runtime_runner.rs:3278-3280).
    assert_eq!(stage("research").effective_feed, Some(RunStage::Understand));
    assert_eq!(stage("design").effective_feed, Some(RunStage::Plan));
    assert_eq!(stage("implement").effective_feed, Some(RunStage::Execute));
    // Review → Verify is the explicit P1 binding (blueprint §5.6): the
    // pre-blueprint runtime emits Verify only for validation cycles; the P2
    // coordinator rewrite realizes the review feed from this binding.
    assert_eq!(stage("review").effective_feed, Some(RunStage::Verify));
    assert_eq!(stage("validate").effective_feed, Some(RunStage::Verify));
    assert_eq!(resolved.feed_map.len(), 5);

    // RunOnce → None: the sixth kind binds no feed entry by default (the
    // closed FeedLabel catalog has no RunOnce label).
    let run_once = concerto_config::blueprint::StageDef {
        tag: "run-once".into(),
        label: "Run Once".into(),
        kind: StageKind::RunOnce.as_str().to_string(),
        version: 1,
        flags: Default::default(),
        condition: Default::default(),
        max_cycles: None,
        feed: None,
        primary: false,
        agents: Vec::new(),
        fallback: None,
        files: None,
    };
    assert_eq!(run_once.effective_feed(), None);

    // -- 5. Cycle caps -------------------------------------------------------
    // review is Review-kind with no explicit cap → engine default 3
    // (relationship.rs:142-145 reviewer→coder Some(3); the coordinator uses
    // the same fallback when running the review gate, coordinator.rs:3513).
    let review = stage("review");
    assert_eq!(review.def.max_cycles, None, "no explicit cap on the review stage");
    assert_eq!(review.def.default_max_cycles(), 3);
    // validate is Acceptance-kind with no explicit cap → engine default 2
    // (relationship.rs:165-170 validator→coder Some(2); coordinator.rs:3915).
    let validate = stage("validate");
    assert_eq!(validate.def.max_cycles, None, "no explicit cap on the validate stage");
    assert_eq!(validate.def.default_max_cycles(), 2);
    // The gate caps sit beneath the single-agent iteration ceiling
    // `DEFAULT_MAX_ITERATIONS = 25` (runtime_runner.rs:453-454) and the
    // per-subtask dispatch ceiling `DEFAULT_MAX_SUBTASK_ATTEMPTS = 3`
    // (coordinator.rs:52) that bound engine loops today.
    assert!(review.def.default_max_cycles() <= 25);
    assert!(validate.def.default_max_cycles() <= 25);
}

/// ADR-58 P2+P3 (Batch 1) sequencing cross-check: on `standard`, the
/// runtime's registry staffing must equal the resolved blueprint's
/// `def.agents` per stage tag, in both directions.
///
/// The registry is the coordinator's dispatch table (`stage_of` /
/// `first_agent_for_stage`); the facade built from `AppConfig.resolved_blueprint`
/// is the post-ADR-58 authority. The runtime_runner construction seam now
/// derives unset stages from the facade (runtime_runner.rs:2871-2891), so a
/// drift here would mean the two consumers of the pipeline disagree — exactly
/// the drift the Batch 1 `debug_assert!` sequencing guards exist to surface.
#[test]
fn registry_staffing_matches_resolved_blueprint_on_standard() {
    use concerto_config::blueprint::OrchestrationConfig;
    use concerto_core::event::EventBus;
    use concerto_core::executor::ToolExecutor;
    use concerto_core::policy::SimplePolicyEngine;
    use concerto_core::traits::policy::AuditLog;
    use concerto_core::types::{AgentStage, ToolRegistry};
    use concerto_core::CancellationToken;
    use concerto_orchestrator::registry::AgentRegistry;
    use concerto_providers::mock::MockProvider;
    use concerto_providers::retry::RetryPolicy;

    struct NoopAudit;

    #[async_trait::async_trait]
    impl AuditLog for NoopAudit {
        async fn record(
            &self,
            _entry: concerto_core::traits::policy::AuditEntry,
            _cancel: CancellationToken,
        ) -> Result<(), concerto_core::error::PolicyError> {
            Ok(())
        }
    }

    let resolved = OrchestrationConfig::default()
        .resolve(&[], None)
        .expect("the standard blueprint must validate and resolve");

    let executor = Arc::new(ToolExecutor::new(
        Arc::new(ToolRegistry::default()),
        Arc::new(SimplePolicyEngine::new(Vec::new(), Arc::new(NoopAudit))),
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
    );

    // Both directions: every blueprint stage's `def.agents` must be exactly
    // the registry's declared participants for that tag, and every canonical
    // agent stage must appear in the resolved blueprint.
    for stage_tag in ["design", "research", "implement", "review", "validate"] {
        let stage = resolved
            .stages
            .iter()
            .find(|s| s.def.tag == stage_tag)
            .unwrap_or_else(|| panic!("standard blueprint must define stage {stage_tag:?}"));
        let mut expected: Vec<&str> = stage.def.agents.iter().map(String::as_str).collect();
        expected.sort_unstable();
        let mut actual: Vec<String> = registry
            .ids_for_stage(&AgentStage::new(stage_tag))
            .iter()
            .map(|id| id.as_str().to_string())
            .collect();
        actual.sort_unstable();
        let actual: Vec<&str> = actual.iter().map(String::as_str).collect();
        assert_eq!(
            actual, expected,
            "registry staffing for {stage_tag:?} must equal blueprint def.agents"
        );
    }
}
