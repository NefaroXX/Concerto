//! ADR-60 D7 approved-plan continuity coverage for `runtime_runner_impl`.
//!
//! Mechanical extraction (NORM S24D): the D7 fixtures — `d7_pool` (file-backed
//! pool + session migrations), `supervised_config`, `d7_design_doc`,
//! `d7_binding`, `d7_binding_named` — and the four approved-plan continuity
//! tests move verbatim out of `runtime_runner::runtime_runner_tests`, so test
//! names and assertions are unchanged. `supervised_config` and
//! `d7_binding_named` move as private (every user is inside this span).
//! `d7_pool` / `d7_design_doc` / `d7_binding` are SHARED with the continuity
//! tests in `runtime_runner/continuity.rs`: they were `pub(super)` inside
//! `runtime_runner_tests` (= visible across the whole `runtime_runner_impl`
//! subtree), and the new module's parent is `tests`, so they keep that exact
//! scope as `pub(in crate::runtime_runner_impl)` and are re-exported at the old
//! `runtime_runner_tests` path — the continuity tests' back-import is
//! unchanged. `use super::super::*;` keeps the parent (`runtime_runner_impl`)
//! items in scope; this cluster touches none of the old test-mod imports.

use super::super::*;

// ------------------------------------------------------------------
// ADR-60 D7 (#152): Plan → Execute two-turn continuity over the
// whiteboard (oracle review 2026-08-22, comments 1–5).
// ------------------------------------------------------------------

/// File-backed pool with production PRAGMAs and every session migration
/// applied — the same substrate `create_audit_pool` gives production runs.
pub(in crate::runtime_runner_impl) async fn d7_pool() -> (tempfile::TempDir, sqlx::SqlitePool) {
    use sqlx::pool::PoolOptions;
    use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqliteSynchronous};
    let dir = tempfile::tempdir().expect("tempdir created");
    let path = dir.path().join("d7_runtime_test.db");
    let options = SqliteConnectOptions::new()
        .filename(&path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .busy_timeout(std::time::Duration::from_secs(5))
        .foreign_keys(true)
        .synchronous(SqliteSynchronous::Normal);
    let pool = PoolOptions::new()
        .max_connections(2)
        .connect_with(options)
        .await
        .expect("test pool connects");
    sqlx::migrate!("../sessions/migrations").run(&pool).await.expect("migrations apply");
    (dir, pool)
}

fn supervised_config() -> concerto_config::MultiAgentConfig {
    concerto_config::MultiAgentConfig { supervisor_enabled: true, ..Default::default() }
}

pub(in crate::runtime_runner_impl) fn d7_design_doc() -> DesignDoc {
    DesignDoc {
        goals: vec!["add plan continuity".to_owned()],
        constraints: vec!["no new dependencies".to_owned()],
        proposed_files: vec![camino::Utf8PathBuf::from("src/continuity.rs")],
        interface_sketch: "load_approved_plan(pool, binding)".to_owned(),
        risks: vec![],
    }
}

pub(in crate::runtime_runner_impl) fn d7_binding() -> PlanBinding {
    d7_binding_named("plan-d7")
}

fn d7_binding_named(plan_id: &str) -> PlanBinding {
    PlanBinding::new(
        plan_id.into(),
        "0123456789abcdef0123456789abcdef".into(),
        Some("abc1234".into()),
        "# Plan\nstep 1: build continuity\nstep 2: verify it".to_owned(),
    )
}

/// Two-turn simulation, happy path:
/// (a) a completed Plan run approves and its structured artifact lands in
///     the whiteboard keyed by `plan_id` (the sole source of truth);
/// (b) the Execute turn rehydrates the STRUCTURED doc + ledger from the
///     log — the task description carries the design doc and carry-forward
///     facts, NOT the rendered plan markdown.
#[tokio::test]
async fn approved_plan_execute_rehydrates_structured_state_not_prose() {
    let (_dir, pool) = d7_pool().await;
    let multi_agent = supervised_config();
    let binding = d7_binding();
    let doc = d7_design_doc();

    // Turn 1 (Plan): approval persists the content-addressed event BEFORE
    // any Execute dispatch exists.
    append_plan_binding_event(Some(&pool), Ulid::new(), &binding, Some(&doc), Some(&multi_agent))
        .await;

    // Turn 2 (Execute): the verified rehydration replaces prose.
    let ctx = load_approved_plan(&pool, &binding).await.expect("verified load").expect("approval");
    assert!(
        ctx.design_doc.is_some() && ctx.ledger == crate::plan_approval::PlanLedger::default(),
        "a first Execute carries the structured doc and a truthful empty ledger"
    );

    let task = build_run_task(
        Ulid::new(),
        TaskExecutionMode::ACTION_REQUIRED,
        true,
        Some(&binding),
        Some(&ctx),
        "i approve the plan",
    );
    assert!(
        task.description.contains("<approved-design-doc>"),
        "the structured artifact governs the task: {}",
        task.description
    );
    assert!(
        task.description.contains("Goal: add plan continuity"),
        "design doc goals ride the structured block: {}",
        task.description
    );
    assert!(
        !task.description.contains("step 1: build continuity"),
        "the rendered plan markdown must NOT be injected as prose: {}",
        task.description
    );
}

/// Carry-forward completeness (oracle comment 4): an Execute AFTER prior
/// work under the same plan knows the completed subtask, the files
/// touched, and the failed command — from the whiteboard ledger.
#[tokio::test]
async fn approved_plan_execute_knows_prior_ledger() {
    use concerto_sessions::whiteboard::{append_whiteboard_event, NewWhiteboardEvent};
    let (_dir, pool) = d7_pool().await;
    let multi_agent = supervised_config();
    let binding = d7_binding();

    append_plan_binding_event(
        Some(&pool),
        Ulid::new(),
        &binding,
        Some(&d7_design_doc()),
        Some(&multi_agent),
    )
    .await;
    // Prior partial execution under this plan id.
    for (event_id, kind, payload) in [
        (
            "done-1",
            concerto_sessions::whiteboard::WhiteboardKind::SubtaskCompleted,
            serde_json::json!({ "task_id": "01HQ", "status": "completed" }),
        ),
        (
            "write-1",
            concerto_sessions::whiteboard::WhiteboardKind::WriteApplied,
            serde_json::json!({ "pre_images": { "src/lib.rs": "h" } }),
        ),
        (
            "fail-1",
            concerto_sessions::whiteboard::WhiteboardKind::Failure,
            serde_json::json!({ "tool": "shell", "error": "cargo test exited 101" }),
        ),
    ] {
        append_whiteboard_event(
            &pool,
            &NewWhiteboardEvent {
                event_id: event_id.to_owned(),
                agent_id: "agent-a".into(),
                kind,
                scope: String::new(),
                session_id: None,
                plan_id: Some(binding.plan_id().to_owned()),
                causation: None,
                payload,
                pre_image_hash: None,
                created_at: 2,
            },
        )
        .await
        .expect("ledger event");
    }

    let ctx = load_approved_plan(&pool, &binding).await.expect("verified load").expect("approval");
    let task = build_run_task(
        Ulid::new(),
        TaskExecutionMode::ACTION_REQUIRED,
        true,
        Some(&binding),
        Some(&ctx),
        "continue",
    );
    assert!(
        task.description.contains("- 01HQ"),
        "completed subtasks are carried forward: {}",
        task.description
    );
    assert!(
        task.description.contains("src/lib.rs"),
        "files touched are carried forward: {}",
        task.description
    );
    assert!(
        task.description.contains("cargo test exited 101"),
        "failed commands carry their failure reasons: {}",
        task.description
    );
}

/// Oracle comment 3: a plan change injected into the whiteboard AFTER
/// `plan-approved` but BEFORE Execute loud-fails — it never silently
/// re-decomposes.
#[tokio::test]
async fn injected_plan_change_loud_fails_without_reapproval() {
    let (_dir, pool) = d7_pool().await;
    let multi_agent = supervised_config();
    let binding = d7_binding();

    append_plan_binding_event(
        Some(&pool),
        Ulid::new(),
        &binding,
        Some(&d7_design_doc()),
        Some(&multi_agent),
    )
    .await;
    // The injected change: a second approval of DIFFERENT content under
    // the same plan id.
    let injected = PlanBinding::new(
        binding.plan_id().to_owned(),
        binding.objective_hash().to_owned(),
        None,
        "# Plan\nstep 1: DELETE EVERYTHING".to_owned(),
    );
    append_plan_binding_event(Some(&pool), Ulid::new(), &injected, None, Some(&multi_agent)).await;

    // The run maps a divergence to a loud, unrecoverable failure.
    match load_approved_plan(&pool, &binding).await {
        Ok(_) => panic!("an injected plan change must never rehydrate cleanly"),
        Err(divergence) => {
            let error = OrchestratorError::Unrecoverable { message: divergence };
            let rendered = error.to_string();
            assert!(
                rendered.contains("re-approval"),
                "the loud failure names the required re-approval: {rendered}"
            );
        }
    }
}

/// Issue #19 decoupling: the whiteboard write gate follows
/// `plan_binding_source` alone, not the supervisor opt-in. A default
/// multi-agent config (supervisor off, `whiteboard` default) writes the
/// structured artifact; `legacy` keeps the pre-D7 prose path regardless of
/// the supervisor flag; a missing multi-agent config (`None`) stays off —
/// continuity is never auto-enabled when the config is absent. Each
/// negative case uses its own plan id so the positive case's committed
/// rows cannot mask a regression.
#[tokio::test]
async fn d7_write_gated_by_plan_binding_source_not_supervisor() {
    let (_dir, pool) = d7_pool().await;

    // supervisor_enabled = false + default source = whiteboard: rows ARE
    // written — Plan→Execute continuity is on by default.
    let off = concerto_config::MultiAgentConfig::default();
    assert!(!off.supervisor_enabled);
    assert_eq!(off.plan_binding_source, concerto_config::PlanBindingSource::Whiteboard);
    let default_binding = d7_binding();
    append_plan_binding_event(
        Some(&pool),
        Ulid::new(),
        &default_binding,
        Some(&d7_design_doc()),
        Some(&off),
    )
    .await;
    let rehydrated = load_approved_plan(&pool, &default_binding).await.expect("load");
    assert!(rehydrated.is_some(), "whiteboard continuity is active without the supervisor opt-in");
    assert!(
        rehydrated.expect("rehydrated").design_doc.is_some(),
        "the structured DesignDoc persists alongside the binding"
    );

    // supervisor_enabled = true but source = legacy: still nothing.
    let mut legacy_supervised = supervised_config();
    legacy_supervised.plan_binding_source = concerto_config::PlanBindingSource::Legacy;
    let legacy_binding = d7_binding_named("plan-d7-legacy-supervised");
    append_plan_binding_event(
        Some(&pool),
        Ulid::new(),
        &legacy_binding,
        None,
        Some(&legacy_supervised),
    )
    .await;
    assert!(
        load_approved_plan(&pool, &legacy_binding).await.expect("load").is_none(),
        "legacy keeps the exact pre-D7 behavior even with supervision on"
    );

    // supervisor_enabled = false + legacy: still nothing.
    let legacy_unsupervised = concerto_config::MultiAgentConfig {
        plan_binding_source: concerto_config::PlanBindingSource::Legacy,
        ..Default::default()
    };
    let legacy_off_binding = d7_binding_named("plan-d7-legacy-unsupervised");
    append_plan_binding_event(
        Some(&pool),
        Ulid::new(),
        &legacy_off_binding,
        None,
        Some(&legacy_unsupervised),
    )
    .await;
    assert!(
        load_approved_plan(&pool, &legacy_off_binding).await.expect("load").is_none(),
        "legacy keeps the exact pre-D7 behavior with supervision off"
    );

    // No multi-agent config at all: defaults to Whiteboard — fresh installs
    // get continuity (DesignDoc persists), legacy must be opted into.
    let orphan_binding = d7_binding_named("plan-d7-no-config");
    append_plan_binding_event(
        Some(&pool),
        Ulid::new(),
        &orphan_binding,
        Some(&d7_design_doc()),
        None,
    )
    .await;
    let orphan_rehydrated = load_approved_plan(&pool, &orphan_binding).await.expect("load");
    assert!(
        orphan_rehydrated.is_some(),
        "fresh installs (no multi_agent section) default to whiteboard continuity"
    );
    assert!(
        orphan_rehydrated.expect("rehydrated").design_doc.is_some(),
        "the structured DesignDoc persists even without a config section"
    );
}
