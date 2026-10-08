//! Spend-log persistence, dispatch-mode, and approved-plan task-phrase
//! coverage for `runtime_runner_impl`.
//!
//! Mechanical extraction (NORM S24F): the spend-log, dispatch-switch, and
//! ADR-55 §4 banners and the four tests from
//! `persist_spend_records_writes_one_record_per_metrics_entry` through
//! `apply_run_uses_approved_plan_not_approval_phrase` move verbatim out of
//! `runtime_runner::runtime_runner_tests`, so test names and assertions are
//! unchanged. The span carries no fixtures — every helper it calls
//! (`persist_spend_records`, `dispatches_to_coordinator`,
//! `approved_plan_task_description`, `build_run_task`) is production code in
//! `runtime_runner_impl` — so nothing is shared with, or duplicated into, the
//! donor module. `use super::super::*;` keeps the parent
//! (`runtime_runner_impl`) items in scope.

use super::super::*;

// ------------------------------------------------------------------
// Spend-log persistence (Phase 3, issue #93)
// ------------------------------------------------------------------
/// One spend record is persisted per completed provider call with the
/// settled actual cost, exactly the data `persist_spend_records` receives
/// from the run output. Exercises the best-effort path against a real
/// in-memory store so the row is actually written.
#[tokio::test]
async fn persist_spend_records_writes_one_record_per_metrics_entry() {
    let store = concerto_sessions::SqliteSessionStore::connect_in_memory().await.unwrap();
    let store: Arc<dyn SessionStore> = Arc::new(store);
    // `spend_records.session_id` REFERENCES sessions(id), so the session
    // must exist for the row to be written.
    let session_id = store
        .create_session(
            camino::Utf8Path::new("/tmp/spend-persist"),
            "openai",
            "gpt-4",
            CancellationToken::new(),
        )
        .await
        .unwrap()
        .id;
    let task_id = Ulid::new();

    // Two entries = two settled provider calls (e.g. two subtasks in a
    // multi-agent run). One carries an empty provider and must be skipped,
    // mirroring the `persist_provider_metrics` guard.
    let metrics = vec![
        ProviderMetrics {
            provider: "openai".into(),
            model: "gpt-4".into(),
            tokens_in: 120,
            tokens_out: 60,
            cost_usd: 0.02,
            latency_ms: 42,
        },
        ProviderMetrics {
            provider: "".into(),
            model: "gpt-4".into(),
            tokens_in: 999,
            tokens_out: 999,
            cost_usd: 1.0,
            latency_ms: 0,
        },
    ];

    persist_spend_records(
        Some(&store),
        session_id,
        Some(task_id),
        &metrics,
        CancellationToken::new(),
    )
    .await;

    let records = store.list_spend_records(session_id, CancellationToken::new()).await.unwrap();
    assert_eq!(records.len(), 1, "exactly one record per settled call, empty providers skipped");
    assert_eq!(records[0].session_id, session_id);
    assert_eq!(records[0].task_id, Some(task_id));
    assert_eq!(records[0].provider, "openai");
    assert_eq!(records[0].model, "gpt-4");
    assert_eq!(records[0].tokens_in, 120);
    assert_eq!(records[0].tokens_out, 60);
    assert!((records[0].cost_usd - 0.02).abs() < f64::EPSILON);
}

// ------------------------------------------------------------------
// Full local agency + dispatch switch.
// ------------------------------------------------------------------

/// `force_single_agent` is the only remaining mode switch; routing no
/// longer selects the loop. Every non-forced run is coordinator-owned so
/// the coordinator can escalate to specialists on need.
#[test]
fn dispatch_switch_is_explicit_mode_only() {
    assert!(dispatches_to_coordinator(false), "multi mode engages the coordinator");
    assert!(!dispatches_to_coordinator(true), "forced single-agent stays on the loop");
}

// ------------------------------------------------------------------
// ADR-55 §4 (M3, live-fix): an Apply run's task describes the
// approved plan, never the approval phrase that armed the dialog.
// ------------------------------------------------------------------

#[test]
fn approved_plan_task_description_embeds_plan_text_and_id() {
    let plan_text = "step 1: read the code\nstep 2: implement the change";
    let binding = PlanBinding::new(
        "plan-123".into(),
        "0123456789abcdef0123456789abcdef".into(),
        None,
        plan_text.into(),
    );

    let description = approved_plan_task_description(&binding);

    assert!(
        description.contains(binding.plan_id()),
        "the description names the plan id, got: {description}"
    );
    assert!(description.contains(plan_text), "the description carries the full plan text");
}

#[test]
fn apply_run_uses_approved_plan_not_approval_phrase() {
    let session_id = Ulid::new();
    let plan_text = "step 1: read the code";
    let binding = PlanBinding::new(
        "plan-123".into(),
        "0123456789abcdef0123456789abcdef".into(),
        None,
        plan_text.into(),
    );
    let input = "i approve";

    // An Apply run with a captured binding executes the approved plan.
    let task = build_run_task(
        session_id,
        TaskExecutionMode::ACTION_REQUIRED,
        true,
        Some(&binding),
        None,
        input,
    );
    assert!(
        task.description.contains(plan_text),
        "Apply task describes the approved plan, got: {}",
        task.description,
    );
    assert!(
        !task.description.contains(input),
        "Apply task must not carry the approval phrase, got: {}",
        task.description,
    );
    assert!(
        matches!(
            task.execution_mode,
            concerto_core::types::TaskExecutionMode::ActionRequired { .. }
        ),
        "the Apply task must stay action-required"
    );

    // Defensive fallback: apply without a captured binding (should be
    // impossible) reuses the input rather than panicking.
    let fallback =
        build_run_task(session_id, TaskExecutionMode::ACTION_REQUIRED, true, None, None, input);
    assert_eq!(fallback.description, input);

    // Non-apply routing is unchanged: action-required and answer-only
    // tasks both carry the user's input verbatim.
    let action =
        build_run_task(session_id, TaskExecutionMode::ACTION_REQUIRED, false, None, None, input);
    assert_eq!(action.description, input);
    assert!(
        matches!(
            action.execution_mode,
            concerto_core::types::TaskExecutionMode::ActionRequired { .. }
        ),
        "a confirmed Execute must stay action-required"
    );
    let answer =
        build_run_task(session_id, TaskExecutionMode::AnswerOnly, false, None, None, "explain X");
    assert_eq!(answer.description, "explain X");
    assert_eq!(answer.execution_mode, TaskExecutionMode::AnswerOnly);

    // Issue #145: the coordinator entry's default mode is carried verbatim
    // through a non-apply build.
    let conversational =
        build_run_task(session_id, TaskExecutionMode::CoordinatorDecides, false, None, None, "hi");
    assert_eq!(conversational.execution_mode, TaskExecutionMode::CoordinatorDecides);
}
