//! ADR-60 Testing §5 — #152 acceptance (Phase 4): Plan→Execute as two turns
//! must NOT re-invoke the architect, and the continuity state MUST come from
//! the whiteboard log.
//!
//! The architect-skip itself is pinned in `coordinator.rs`'s approved-plan
//! tests (a seeded `ApprovedPlanSeed` never dispatches the architect). What
//! this suite adds is the WHITEBOARD-SOURCE half of the acceptance chain:
//!
//! 1. Turn 1 (Plan) approves a plan and records it ONLY in the whiteboard +
//!    the process registry;
//! 2. the coordinator process dies (registry cleared);
//! 3. Turn 2 (Execute, fresh state) rehydrates the verified structured doc
//!    and the carry-forward ledger EXCLUSIVELY via
//!    `load_approved_plan` — the exact call whose output feeds
//!    `CoordinatorAgent::with_approved_plan_seed`;
//! 4. a divergent re-approval of the same plan id is a LOUD failure, and an
//!    unknown plan degrades to `Ok(None)` (legacy prose), never invented
//!    state.

use camino::Utf8PathBuf;
use concerto_core::ids::Ulid;
use concerto_core::types::DesignDoc;
use concerto_orchestrator::coordinator::ApprovedPlanSeed;
use concerto_orchestrator::plan_approval::{
    append_plan_approved_event, load_approved_plan, plan_registry, PlanBinding,
};
use concerto_sessions::whiteboard::{
    append_whiteboard_event, load_whiteboard_events_by_plan, NewWhiteboardEvent, WhiteboardKind,
};
use serde_json::json;

async fn pool() -> (tempfile::TempDir, sqlx::SqlitePool) {
    let dir = tempfile::tempdir().expect("tempdir created");
    let options = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(dir.path().join("continuity.db"))
        .create_if_missing(true)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
        .busy_timeout(std::time::Duration::from_secs(5))
        .foreign_keys(true)
        .synchronous(sqlx::sqlite::SqliteSynchronous::Normal);
    let pool = sqlx::pool::PoolOptions::new()
        .max_connections(4)
        .connect_with(options)
        .await
        .expect("test pool connects");
    sqlx::migrate!("../sessions/migrations").run(&pool).await.expect("migrations apply");
    (dir, pool)
}

fn design_doc() -> DesignDoc {
    DesignDoc {
        goals: vec!["ship the gate".to_owned()],
        constraints: vec!["wal-before-execute".to_owned()],
        proposed_files: vec![Utf8PathBuf::from("src/gate.rs")],
        interface_sketch: "submit(req) -> outcome".to_owned(),
        risks: vec![],
    }
}

fn canonical_doc(doc: &DesignDoc) -> String {
    serde_json::to_string(&doc).expect("doc serializes")
}

fn ledger_event(
    plan_id: &str,
    kind: WhiteboardKind,
    payload: serde_json::Value,
) -> NewWhiteboardEvent {
    NewWhiteboardEvent {
        event_id: Ulid::new().to_string(),
        agent_id: "coder".to_owned(),
        kind,
        scope: String::new(),
        session_id: None,
        plan_id: Some(plan_id.to_owned()),
        causation: None,
        payload,
        pre_image_hash: None,
        created_at: 1_700_000_000_000,
    }
}

#[tokio::test]
async fn plan_to_execute_continuity_survives_a_process_restart_via_the_whiteboard() {
    let (_dir, pool) = pool().await;
    let session = Ulid::new();
    let objective_hash = blake3::hash(b"build the write gate").to_hex().to_string();
    let plan_text = "# Plan\n\n1. build the write gate".to_owned();
    let doc = design_doc();

    // ── Turn 1 (Plan): approve + record. The registry is the only OTHER
    // holder of the doc — and it dies with the process below.
    let binding =
        PlanBinding::new("plan-152".to_owned(), objective_hash.clone(), None, plan_text.clone());
    plan_registry().insert(session, binding.clone());
    append_plan_approved_event(&pool, session, &binding, Some(&doc))
        .await
        .expect("plan-approved recorded");

    // ── The coordinator process dies: every in-process holder is gone.
    plan_registry().clear_session(session);

    // ── Turn 2 (Execute, fresh state): the durable mirror path rehydrates
    // the binding, and the WHITEBOARD is the sole source of the structured
    // state. Execute-phase ledger events land between the two turns.
    let restored = PlanBinding::restored(
        "plan-152".to_owned(),
        objective_hash.clone(),
        None,
        plan_text,
        binding.artifact_hash().map(ToOwned::to_owned),
        binding.created_at(),
    );
    append_whiteboard_event(
        &pool,
        &ledger_event(
            "plan-152",
            WhiteboardKind::SubtaskCompleted,
            json!({ "description": "gate skeleton" }),
        ),
    )
    .await
    .expect("subtask-completed recorded");
    append_whiteboard_event(
        &pool,
        &ledger_event(
            "plan-152",
            WhiteboardKind::WriteApplied,
            json!({ "pre_images": { "src/gate.rs": "abc" } }),
        ),
    )
    .await
    .expect("write-applied recorded");
    append_whiteboard_event(
        &pool,
        &ledger_event(
            "plan-152",
            WhiteboardKind::Failure,
            json!({ "tool": "shell", "error": "cargo test failed once" }),
        ),
    )
    .await
    .expect("failure recorded");

    let context = load_approved_plan(&pool, &restored)
        .await
        .expect("verified load")
        .expect("the approved plan survives past the planning process");

    // The structured doc came back byte-identical FROM THE LOG — this is the
    // object that seeds decompose, so the architect is never re-invoked.
    assert_eq!(
        canonical_doc(context.design_doc.as_ref().expect("doc rehydrated")),
        canonical_doc(&doc),
        "the whiteboard restores the exact approved DesignDoc"
    );
    assert!(context.binding.artifact_verifies(), "the rehydrated binding verifies");

    // Carry-forward ledger folded from the log (fix #3: earlier writes are
    // known; failed commands are not re-run unchanged).
    assert_eq!(context.ledger.completed_subtasks, vec!["gate skeleton"]);
    assert_eq!(context.ledger.files_touched, vec!["src/gate.rs"]);
    assert_eq!(context.ledger.failed_commands, vec!["shell: cargo test failed once"]);

    // Seed fidelity: the coordinator consumes exactly this shape.
    let seed = ApprovedPlanSeed {
        plan_id: context.binding.plan_id().to_owned(),
        design_doc: context.design_doc,
    };
    assert_eq!(seed.plan_id, "plan-152");
    assert_eq!(
        serde_json::to_string(&seed.design_doc.expect("seed carries the doc")).expect("json"),
        canonical_doc(&design_doc()),
        "whiteboard -> seed fidelity"
    );

    // Negative control: a plan with no whiteboard events degrades to None
    // (the legacy prose path) instead of inventing continuity.
    let unknown = PlanBinding::new(
        "plan-unknown".to_owned(),
        blake3::hash(b"another objective").to_hex().to_string(),
        None,
        "text".to_owned(),
    );
    assert!(
        load_approved_plan(&pool, &unknown).await.expect("verified load").is_none(),
        "missing whiteboard state is never invented"
    );

    // And a DIVERGENT re-approval (same plan id, different content) is a loud
    // failure — silent re-decompose is forbidden.
    let mut tampered = ledger_event(
        "plan-152",
        WhiteboardKind::PlanApproved,
        json!({
            "plan_id": "plan-152",
            "objective_hash": objective_hash,
            "artifact_hash": "deadbeef",
            "plan_text": "injected divergence",
            "created_at_ms": 1
        }),
    );
    tampered.agent_id = "attacker".to_owned();
    append_whiteboard_event(&pool, &tampered).await.expect("second approval appended");
    let events = load_whiteboard_events_by_plan(&pool, "plan-152").await.expect("plan events");
    assert!(events.len() >= 2, "both approvals are on the log");
    assert!(
        load_approved_plan(&pool, &restored).await.is_err(),
        "divergence between two approvals must loud-fail, never silently pick one"
    );
}
