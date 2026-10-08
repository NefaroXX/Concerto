//! ADR-60 D7 run-continuity cluster for `runtime_runner_impl`.
//!
//! This module owns the read-side continuity a `continue`/resume run
//! rehydrates from the session whiteboard when no approved-plan binding
//! governs it: the bounded window consts (`RUN_CONTINUITY_WINDOW` /
//! `RUN_CONTINUITY_PLAN_WINDOW`), the `RunContinuity` snapshot, the trigger
//! predicate (`run_continuity_applies`), the windowed loaders
//! (`load_run_continuity` / `load_newest_plan_approval`), the rendered task
//! section (`run_continuity_description`), and the fail-soft seeder
//! (`seed_run_continuity`) that also yields the ADR-60 D7 headless dispatch
//! seed. The cluster moves verbatim from `runtime_runner.rs` (NORM S21d): no
//! behavior, signature, or call-site change.
//!
//! Stability contract: every item is `pub(crate)` and re-exported by the
//! parent's `pub(crate) use continuity::*;`, so the run-loop dispatch site
//! (`run_continuity_applies` + `seed_run_continuity`) stays byte-identical
//! through this glob. The seven run-continuity tests plus their
//! `append_session_event` helper moved here from
//! `runtime_runner::runtime_runner_tests` and exercise the cluster in scope;
//! the shared D7 fixtures they call (`d7_pool`, `d7_binding`, `d7_design_doc`)
//! stay in `runtime_runner_tests` (still shared with the approved-plan
//! rehydration suite) and are imported `pub(super)`. This file is loaded via
//! the parent's explicit `#[path = "runtime_runner/continuity.rs"]`, mirroring
//! `runtime_runner/recorders.rs`, `runtime_runner/memory_bootstrap.rs`, and
//! `runtime_runner/plugin_setup.rs` (the parent itself is loaded via
//! `#[path]` as `runtime_runner_impl`).

use crate::coordinator::HeadlessResumeSeed;
use crate::plan_approval::{fold_ledger, plan_artifact_hash, PlanApprovedPayload, PlanLedger};

use concerto_core::ids::Ulid;
use concerto_core::types::AgentTask;

use concerto_sessions::whiteboard::{
    latest_gate_seq, load_whiteboard_events, WhiteboardKind, WhiteboardLoadOpts,
};

use super::{d7_whiteboard_enabled, is_resume_request, render_plan_ledger_section};

// ===========================================================================
// ADR-60 D7 run-continuity: `continue` without an approved-plan binding.
//
// The approved-plan rehydration above only fires when a Plan was approved and
// applied. A `continue` after a failed Execute — or in a reopened project,
// where the active session already carries earlier runs — used to start
// blank, wastefully re-deriving what the ledger already knows. The fix is
// read-side only: the gate already persists the durable substrate (keyed by
// the run's session id — `write-applied` rows carry the files touched,
// `failure` rows carry failed commands, and any `plan-approved` row carries
// the session's last approved artifact), so continuity rehydrates from the
// log instead of duplicating it into a new summary event. No new write path,
// no new whiteboard kind, no double bookkeeping to drift.
//
// Fail-soft by the same contract as every D7 step: no pool, an empty log, or
// a read error leaves the task untouched (with an observable warn), and a
// plan payload that fails its own artifact hash is skipped, never trusted.
// ===========================================================================

/// How many of a session's most recent whiteboard events the run-continuity
/// read folds. Bounds the query; the gate's write/failure rows and the last
/// plan approval sit at the tail of the session's log, so the newest window
/// is the relevant one.
pub(crate) const RUN_CONTINUITY_WINDOW: usize = 200;
/// ADR-60 D7 (interrupt-safe resume): the wider bounded read the headless
/// plan-anchor lookup uses. A long build can scroll the plan approval past
/// the continuity window's newest 200 events, but the approval stays the
/// dispatch anchor a headless resume re-derives from; this window bounds the
/// extra read (same posture as the coordinator's 2000-event §7 resume
/// window).
pub(crate) const RUN_CONTINUITY_PLAN_WINDOW: usize = 2000;

/// One session's run-continuity snapshot: the newest hash-verified
/// `plan-approved` payload (when the session ever approved a plan) plus the
/// carry-forward ledger folded from the gate's write/failure events.
#[derive(Debug, Default)]
pub(crate) struct RunContinuity {
    /// The session's last approved plan, artifact-hash verified at read.
    plan: Option<PlanApprovedPayload>,
    /// The REAL whiteboard event id of the `plan-approved` row `plan` came
    /// from — the evidence citation the headless-resume dispatch decisions
    /// record (ADR-65 §7: never fabricated). `Some` iff `plan` is `Some`.
    plan_approved_event_id: Option<String>,
    /// Carry-forward state from the session's gate-written ledger events.
    ledger: PlanLedger,
}

impl RunContinuity {
    /// An empty snapshot seeds nothing — the truthful state for a fresh
    /// session or a pre-whiteboard log.
    fn is_empty(&self) -> bool {
        // `plan_approved_event_id` is `Some` iff `plan` is `Some`, so the
        // emptiness test needs neither field spelled out.
        self.plan.is_none() && self.ledger == PlanLedger::default()
    }
}

/// Whether a run seeds run-continuity into its task: a `continue`/resume
/// request that is NOT governed by an approved-plan binding (that path has
/// its own verified rehydration), gated by the same `plan_binding_source`
/// switch as every D7 whiteboard read.
pub(crate) fn run_continuity_applies(
    apply_plan: bool,
    input: &str,
    multi_agent: Option<&concerto_config::MultiAgentConfig>,
) -> bool {
    !apply_plan && is_resume_request(input) && d7_whiteboard_enabled(multi_agent)
}

/// Load a session's run-continuity snapshot from the whiteboard: the newest
/// `plan-approved` payload (verified against its own artifact hash — an
/// unattested artifact is skipped with a warn, never trusted) plus the ledger
/// folded from the session's gate-written events.
///
/// ADR-65 §7: `cursor_gate_seq` anchors the read at the checkpoint's
/// whiteboard cursor when one governs the run — only facts appended AFTER the
/// cursor are folded into the resumed run's evidence view; pre-cursor events
/// are state the checkpoint itself carries, never replayed prose.
///
/// ADR-60 D7 (interrupt-safe resume): on a HEADLESS resume (`headless_plan_anchor`,
/// no checkpoint row governs the run) the plan approval is re-read
/// independently of the continuity window — a long build can scroll the
/// approval past the newest [`RUN_CONTINUITY_WINDOW`] events, but it stays the
/// dispatch anchor the resume re-derives from.
pub(crate) async fn load_run_continuity(
    pool: &sqlx::SqlitePool,
    session_id: Ulid,
    cursor_gate_seq: Option<u64>,
    headless_plan_anchor: bool,
) -> Result<RunContinuity, concerto_sessions::SessionError> {
    // Read the newest tail of the session's log. `gate_seq` is global: with
    // a §7 cursor the evidence view starts exactly there; otherwise anchor
    // the cursor `RUN_CONTINUITY_WINDOW` events back from the head and rely
    // on the session filter to keep only this session's rows.
    let after = match cursor_gate_seq {
        Some(cursor) => cursor,
        None => {
            let head = latest_gate_seq(pool).await?;
            head.saturating_sub(RUN_CONTINUITY_WINDOW as u64)
        }
    };
    let events = load_whiteboard_events(
        pool,
        &WhiteboardLoadOpts {
            after_gate_seq: after,
            session_id: Some(session_id.to_string()),
            scope: None,
            limit: RUN_CONTINUITY_WINDOW,
        },
    )
    .await?;

    let plan: Option<(PlanApprovedPayload, String)> = if headless_plan_anchor {
        load_newest_plan_approval(pool, session_id).await?
    } else {
        // The newest approval only: an older plan could be stale, and digging
        // backwards past an unverifiable one would trust a log the caller cannot
        // attest to.
        events.iter().rev().find(|event| event.kind == WhiteboardKind::PlanApproved).and_then(
            |event| match serde_json::from_value::<PlanApprovedPayload>(event.payload.clone()) {
                Ok(payload) if plan_artifact_hash(&payload.plan_text) == payload.artifact_hash => {
                    Some((payload, event.event_id.clone()))
                }
                Ok(payload) => {
                    tracing::warn!(
                        event_id = %event.event_id,
                        plan_id = %payload.plan_id,
                        "session's last plan-approved payload fails its artifact hash — \
                         run continuity skips it"
                    );
                    None
                }
                Err(error) => {
                    tracing::warn!(
                        %error,
                        event_id = %event.event_id,
                        "session's last plan-approved payload is unreadable — run continuity \
                         skips it"
                    );
                    None
                }
            },
        )
    };
    let ledger = fold_ledger(&events);
    Ok(RunContinuity {
        plan: plan.as_ref().map(|(payload, _)| payload.clone()),
        plan_approved_event_id: plan.map(|(_, event_id)| event_id),
        ledger,
    })
}

/// The session's newest hash-verified `plan-approved` payload plus its REAL
/// whiteboard event id, read independently of the run-continuity window
/// (ADR-60 D7 interrupt-safe resume): the approval can be older than the
/// newest [`RUN_CONTINUITY_WINDOW`] events on a long build, yet remains the
/// dispatch anchor a headless resume re-derives from. Bounded by
/// [`RUN_CONTINUITY_PLAN_WINDOW`] events back from the log head — an
/// unreadable or unattested newest approval is skipped with a warn, never
/// trusted.
pub(crate) async fn load_newest_plan_approval(
    pool: &sqlx::SqlitePool,
    session_id: Ulid,
) -> Result<Option<(PlanApprovedPayload, String)>, concerto_sessions::SessionError> {
    let head = latest_gate_seq(pool).await?;
    let after = head.saturating_sub(RUN_CONTINUITY_PLAN_WINDOW as u64);
    let events = load_whiteboard_events(
        pool,
        &WhiteboardLoadOpts {
            after_gate_seq: after,
            session_id: Some(session_id.to_string()),
            scope: None,
            limit: RUN_CONTINUITY_PLAN_WINDOW,
        },
    )
    .await?;
    Ok(events.iter().rev().find(|event| event.kind == WhiteboardKind::PlanApproved).and_then(
        |event| match serde_json::from_value::<PlanApprovedPayload>(event.payload.clone()) {
            Ok(payload) if plan_artifact_hash(&payload.plan_text) == payload.artifact_hash => {
                Some((payload, event.event_id.clone()))
            }
            Ok(payload) => {
                tracing::warn!(
                    event_id = %event.event_id,
                    plan_id = %payload.plan_id,
                    "session's newest plan-approved payload fails its artifact hash — \
                     the headless resume skips it"
                );
                None
            }
            Err(error) => {
                tracing::warn!(
                    %error,
                    event_id = %event.event_id,
                    "session's newest plan-approved payload is unreadable — the headless \
                     resume skips it"
                );
                None
            }
        },
    ))
}

/// Render the run-continuity section appended to a `continue` run's task
/// description: the re-anchor context (last approved artifact) plus the same
/// carry-forward ledger grammar the approved-plan Execute uses.
pub(crate) fn run_continuity_description(continuity: &RunContinuity) -> String {
    let mut out = String::new();
    out.push_str("<run-continuity>\n");
    out.push_str(
        "This run resumes prior work in this session. The state below is rehydrated from the \
         session's whiteboard ledger — do not re-plan from scratch and do not redo completed \
         work.\n",
    );
    if let Some(plan) = &continuity.plan {
        out.push_str(&format!(
            "\n<last-approved-plan id=\"{}\">\n{}\n</last-approved-plan>\n",
            plan.plan_id, plan.plan_text
        ));
    }
    out.push_str(&render_plan_ledger_section(&continuity.ledger));
    out
}

/// Load the session's run-continuity snapshot and append it to the task
/// description when the log carries anything. Fail-soft: an empty log seeds
/// nothing; a read error warns and leaves the task untouched — continuity
/// bookkeeping never fails the run. `cursor_gate_seq` anchors the read at the
/// checkpoint's whiteboard cursor (ADR-65 §7) when one governs the run.
///
/// ADR-60 D7 (interrupt-safe resume, 2026-09-05): when the run is HEADLESS
/// (`headless_plan_anchor` — no checkpoint row governs this resume) and the
/// log carries a hash-verified `plan-approved` payload, this ALSO returns the
/// dispatch seed the coordinator consumes to schedule from the evidence
/// chain (verified design + research done ⇒ the coder, never the architect).
pub(crate) async fn seed_run_continuity(
    task: &mut AgentTask,
    pool: &sqlx::SqlitePool,
    session_id: Ulid,
    cursor_gate_seq: Option<u64>,
    headless_plan_anchor: bool,
) -> Option<HeadlessResumeSeed> {
    match load_run_continuity(pool, session_id, cursor_gate_seq, headless_plan_anchor).await {
        Ok(continuity) if !continuity.is_empty() => {
            tracing::info!(
                session_id = %task.session_id,
                last_plan = continuity.plan.as_ref().map(|plan| plan.plan_id.as_str()),
                files_touched = continuity.ledger.files_touched.len(),
                failed_commands = continuity.ledger.failed_commands.len(),
                "rehydrated run continuity from the whiteboard for a resume request \
                 (ADR-60 D7)"
            );
            task.description.push_str("\n\n");
            task.description.push_str(&run_continuity_description(&continuity));
            // ADR-60 D7 (interrupt-safe resume): the dispatch cursor rides the
            // same verified payload — headless only, and only when the
            // approval is attested with its real event id (the loader sets
            // both from one row; a degraded read yields neither).
            if headless_plan_anchor {
                match (continuity.plan, continuity.plan_approved_event_id) {
                    (Some(plan), Some(plan_approved_event_id)) => {
                        return Some(HeadlessResumeSeed {
                            objective_hash: plan.objective_hash,
                            plan_text: plan.plan_text,
                            design_doc: plan.design_doc,
                            plan_approved_event_id,
                        });
                    }
                    (Some(_), None) => {
                        tracing::warn!(
                            session_id = %task.session_id,
                            "plan-approved payload loaded without its event id — the \
                             headless resume proceeds without a dispatch seed (never \
                             fabricates evidence)"
                        );
                    }
                    (None, _) => {}
                }
            }
            None
        }
        Ok(_) => None,
        Err(error) => {
            tracing::warn!(
                %error,
                session_id = %task.session_id,
                "failed to load the session's run-continuity ledger; the resume run \
                 proceeds without it"
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::runtime_runner_tests::{d7_binding, d7_design_doc, d7_pool};
    use super::super::{append_plan_binding_event, build_run_task};
    use super::*;
    use concerto_core::types::TaskExecutionMode;

    // ------------------------------------------------------------------
    // ADR-60 D7 run-continuity: `continue` without an approved-plan
    // binding rehydrates the session's ledger (files touched, failed
    // commands) and the session's last approved artifact from the
    // whiteboard, so a resume after a failed Execute — or in a reopened
    // project — never starts blank.
    // ------------------------------------------------------------------

    /// Helper: append one gate-shaped whiteboard event keyed to `session_id`
    /// (mirroring `in_process_gate`: session-keyed, plan-less).
    async fn append_session_event(
        pool: &sqlx::SqlitePool,
        session_id: Ulid,
        event_id: &str,
        kind: concerto_sessions::whiteboard::WhiteboardKind,
        payload: serde_json::Value,
    ) {
        use concerto_sessions::whiteboard::{append_whiteboard_event, NewWhiteboardEvent};
        append_whiteboard_event(
            pool,
            &NewWhiteboardEvent {
                event_id: event_id.to_owned(),
                agent_id: "single-agent".into(),
                kind,
                scope: String::new(),
                session_id: Some(session_id.to_string()),
                plan_id: None,
                causation: None,
                payload,
                pre_image_hash: None,
                created_at: 2,
            },
        )
        .await
        .expect("session-keyed whiteboard event");
    }

    /// The trigger follows the resume request and the D7 gate alone: a
    /// `continue` on the whiteboard source seeds (including fresh installs
    /// with no multi-agent section); an Apply run keeps its own verified
    /// rehydration; a non-resume input and the `legacy` source seed nothing.
    #[test]
    fn run_continuity_applies_follows_resume_request_and_whiteboard_gate() {
        let default = concerto_config::MultiAgentConfig::default();
        assert!(run_continuity_applies(false, "continue", Some(&default)));
        assert!(run_continuity_applies(false, "continue", None));
        assert!(run_continuity_applies(false, "resume the task", Some(&default)));
        // The approved-plan path governs its own rehydration.
        assert!(!run_continuity_applies(true, "continue", Some(&default)));
        // Only resume-shaped inputs seed.
        assert!(!run_continuity_applies(false, "fix the login bug", Some(&default)));
        // `legacy` keeps the exact pre-D7 behavior.
        let legacy = concerto_config::MultiAgentConfig {
            plan_binding_source: concerto_config::PlanBindingSource::Legacy,
            ..Default::default()
        };
        assert!(!run_continuity_applies(false, "continue", Some(&legacy)));
    }

    /// The live scenario: a `continue` after a failed Execute (session-keyed
    /// gate rows from the prior run, no binding in play) seeds the files
    /// touched, the failed command with its reason, and the session's last
    /// approved artifact into the task description.
    #[tokio::test]
    async fn continue_rehydrates_session_ledger_and_last_plan_from_whiteboard() {
        let (_dir, pool) = d7_pool().await;
        let session_id = Ulid::new();
        let multi_agent = concerto_config::MultiAgentConfig::default();

        // The session's last approved plan (single-agent shape: no DesignDoc,
        // the capped text is the artifact), written session-keyed like the
        // post-Plan binding insert does.
        let binding = d7_binding();
        append_plan_binding_event(Some(&pool), session_id, &binding, None, Some(&multi_agent))
            .await;
        // Gate-shaped ledger rows from the failed prior run.
        append_session_event(
            &pool,
            session_id,
            "write-1",
            concerto_sessions::whiteboard::WhiteboardKind::WriteApplied,
            serde_json::json!({ "pre_images": { "src/lib.rs": "h" } }),
        )
        .await;
        append_session_event(
            &pool,
            session_id,
            "fail-1",
            concerto_sessions::whiteboard::WhiteboardKind::Failure,
            serde_json::json!({ "tool": "shell", "error": "cargo test exited 101" }),
        )
        .await;

        let continuity = load_run_continuity(&pool, session_id, None, false).await.expect("load");
        assert!(
            continuity
                .plan
                .as_ref()
                .is_some_and(|plan| plan.plan_text.contains("build continuity")),
            "the session's last approved artifact rehydrates: {:?}",
            continuity.plan
        );
        assert!(
            continuity.ledger.files_touched.contains(&"src/lib.rs".to_owned()),
            "files touched fold from the gate rows: {:?}",
            continuity.ledger
        );
        assert!(
            continuity
                .ledger
                .failed_commands
                .iter()
                .any(|entry| entry.contains("cargo test exited 101")),
            "failed commands carry their failure reasons: {:?}",
            continuity.ledger
        );

        // The seed mirrors the hook: a resume-shaped task grows the section.
        let mut task = build_run_task(
            session_id,
            TaskExecutionMode::ACTION_REQUIRED,
            false,
            None,
            None,
            "continue",
        );
        assert_eq!(task.description, "continue");
        seed_run_continuity(&mut task, &pool, session_id, None, false).await;
        assert!(
            task.description.contains("<run-continuity>"),
            "the continuity section is appended: {}",
            task.description
        );
        assert!(
            task.description.contains("<last-approved-plan"),
            "the last approved artifact re-anchors the run: {}",
            task.description
        );
        assert!(
            task.description.contains("src/lib.rs")
                && task.description.contains("cargo test exited 101"),
            "the ledger rides the section: {}",
            task.description
        );
    }

    /// ADR-60 D7 (interrupt-safe resume): on a HEADLESS resume (no
    /// checkpoint row governs the run) the verified `plan-approved` payload
    /// also yields the dispatch seed — the original objective hash, the plan
    /// text, and the REAL plan-approved event id. The checkpoint-governed
    /// shape (`headless_plan_anchor = false`) never yields one.
    #[tokio::test]
    async fn headless_resume_seeds_the_dispatch_cursor_from_the_plan_approval() {
        let (_dir, pool) = d7_pool().await;
        let session_id = Ulid::new();
        let binding = d7_binding();
        append_plan_binding_event(
            Some(&pool),
            session_id,
            &binding,
            Some(&d7_design_doc()),
            Some(&concerto_config::MultiAgentConfig::default()),
        )
        .await;

        // Headless: the seed rides the verified payload.
        let mut task = build_run_task(
            session_id,
            TaskExecutionMode::ACTION_REQUIRED,
            false,
            None,
            None,
            "continue",
        );
        let seed = seed_run_continuity(&mut task, &pool, session_id, None, true)
            .await
            .expect("the headless resume seeds from the verified approval");
        assert_eq!(seed.objective_hash, binding.objective_hash(), "the ORIGINAL objective hash");
        assert_eq!(seed.plan_text, binding.plan_text(), "the approved plan text");
        assert!(
            seed.design_doc.is_some(),
            "the structured doc rides the seed so the architect is never re-derived"
        );
        // The event id must be a REAL plan-approved row in the log.
        let logged =
            load_whiteboard_events(&pool, &WhiteboardLoadOpts::default()).await.expect("load");
        assert!(
            logged.iter().any(|event| {
                event.kind == WhiteboardKind::PlanApproved
                    && event.event_id == seed.plan_approved_event_id
            }),
            "the seed cites the REAL plan-approved event id"
        );

        // Checkpoint-governed: no dispatch seed (the §7 evaluator governs).
        let mut task = build_run_task(
            session_id,
            TaskExecutionMode::ACTION_REQUIRED,
            false,
            None,
            None,
            "continue",
        );
        assert!(
            seed_run_continuity(&mut task, &pool, session_id, None, false).await.is_none(),
            "a checkpoint-governed resume never takes the headless dispatch cursor"
        );
    }

    /// A long build scrolls the plan approval past the continuity window's
    /// newest 200 events, but the headless dispatch anchor is read
    /// independently (bounded by the wider plan window) — the approval is
    /// still found, still hash-verified.
    #[tokio::test]
    async fn headless_plan_anchor_survives_window_scrolling() {
        let (_dir, pool) = d7_pool().await;
        let session_id = Ulid::new();
        let binding = d7_binding();
        append_plan_binding_event(
            Some(&pool),
            session_id,
            &binding,
            None,
            Some(&concerto_config::MultiAgentConfig::default()),
        )
        .await;
        // Scroll the approval out of the 200-event continuity window.
        for index in 0..(RUN_CONTINUITY_WINDOW + 20) {
            append_session_event(
                &pool,
                session_id,
                &format!("filler-{index}"),
                concerto_sessions::whiteboard::WhiteboardKind::Failure,
                serde_json::json!({ "tool": "shell", "error": format!("e{index}") }),
            )
            .await;
        }

        // The in-window continuity read no longer sees the approval...
        let windowed = load_run_continuity(&pool, session_id, None, false).await.expect("load");
        assert!(
            windowed.plan.is_none(),
            "the approval scrolled past the continuity window (the prose seed degrades)"
        );
        // ...but the headless anchor read still finds it, hash-verified.
        let anchored = load_newest_plan_approval(&pool, session_id).await.expect("anchor load");
        let (payload, event_id) = anchored.expect("the approval is still reachable");
        assert_eq!(payload.plan_id, binding.plan_id());
        assert_eq!(payload.artifact_hash, binding.artifact_hash().unwrap_or_default());
        assert!(
            load_whiteboard_events(&pool, &WhiteboardLoadOpts::default())
                .await
                .expect("load")
                .iter()
                .any(|event| event.event_id == event_id
                    && event.kind == WhiteboardKind::PlanApproved),
            "the anchor cites a REAL plan-approved row"
        );
    }

    /// ADR-65 §7: a cursor-anchored continuity read folds ONLY events
    /// appended after the cursor — pre-cursor rows are checkpoint state,
    /// never replayed prose — while the no-cursor read keeps the legacy
    /// whole-window behavior.
    #[tokio::test]
    async fn run_continuity_respects_the_whiteboard_cursor() {
        const WRITE_APPLIED: concerto_sessions::whiteboard::WhiteboardKind =
            concerto_sessions::whiteboard::WhiteboardKind::WriteApplied;
        let (_dir, pool) = d7_pool().await;
        let session_id = Ulid::new();

        // Pre-cursor rows: the state the checkpoint itself carries.
        append_session_event(
            &pool,
            session_id,
            "pre-write",
            WRITE_APPLIED,
            serde_json::json!({ "pre_images": { "src/pre.rs": "h" } }),
        )
        .await;
        let stored =
            load_whiteboard_events(&pool, &WhiteboardLoadOpts::default()).await.expect("load");
        let cursor = stored
            .iter()
            .filter(|event| event.kind == WRITE_APPLIED)
            .map(|event| event.gate_seq)
            .max()
            .expect("a pre-cursor write row exists");

        // The post-cursor row: appended after the checkpoint.
        append_session_event(
            &pool,
            session_id,
            "post-write",
            WRITE_APPLIED,
            serde_json::json!({ "pre_images": { "src/post.rs": "h" } }),
        )
        .await;

        let cursor_view =
            load_run_continuity(&pool, session_id, Some(cursor), false).await.expect("load");
        assert!(
            cursor_view.ledger.files_touched.contains(&"src/post.rs".to_owned()),
            "post-cursor facts are the evidence view: {:?}",
            cursor_view.ledger
        );
        assert!(
            !cursor_view.ledger.files_touched.contains(&"src/pre.rs".to_owned()),
            "pre-cursor events are NOT replayed into the evidence view: {:?}",
            cursor_view.ledger
        );

        // Without a cursor the legacy whole-window read still folds both.
        let legacy = load_run_continuity(&pool, session_id, None, false).await.expect("load");
        assert!(
            legacy.ledger.files_touched.contains(&"src/pre.rs".to_owned())
                && legacy.ledger.files_touched.contains(&"src/post.rs".to_owned()),
            "no cursor → the legacy window reads the whole tail: {:?}",
            legacy.ledger
        );
    }

    /// A session's last plan-approved payload that fails its own artifact
    /// hash is never trusted: the plan section is skipped (with a warn), but
    /// the ledger still folds — continuity degrades, it does not break.
    #[tokio::test]
    async fn run_continuity_skips_a_plan_payload_that_fails_its_artifact_hash() {
        use concerto_sessions::whiteboard::{append_whiteboard_event, NewWhiteboardEvent};
        let (_dir, pool) = d7_pool().await;
        let session_id = Ulid::new();

        append_whiteboard_event(
            &pool,
            &NewWhiteboardEvent {
                event_id: "tampered-plan".into(),
                agent_id: "coordinator".into(),
                kind: concerto_sessions::whiteboard::WhiteboardKind::PlanApproved,
                scope: String::new(),
                session_id: Some(session_id.to_string()),
                plan_id: Some("plan-tampered".into()),
                causation: None,
                payload: serde_json::json!({
                    "plan_id": "plan-tampered",
                    "objective_hash": "0123456789abcdef0123456789abcdef",
                    "artifact_hash": "deadbeef",
                    "design_doc": null,
                    "plan_text": "# Plan\nstep 1: build continuity",
                    "created_at_ms": 2,
                }),
                pre_image_hash: None,
                created_at: 2,
            },
        )
        .await
        .expect("tampered plan-approved event");
        append_session_event(
            &pool,
            session_id,
            "write-1",
            concerto_sessions::whiteboard::WhiteboardKind::WriteApplied,
            serde_json::json!({ "path": "src/main.rs" }),
        )
        .await;

        let continuity = load_run_continuity(&pool, session_id, None, false).await.expect("load");
        assert!(continuity.plan.is_none(), "an unattested artifact is skipped, never trusted");
        assert!(
            continuity.ledger.files_touched.contains(&"src/main.rs".to_owned()),
            "the ledger still folds past the skipped plan: {:?}",
            continuity.ledger
        );
    }

    /// A fresh session (or a pre-whiteboard log) carries nothing: the
    /// snapshot is truthfully empty and the task is untouched.
    #[tokio::test]
    async fn run_continuity_empty_log_is_a_noop() {
        let (_dir, pool) = d7_pool().await;
        let session_id = Ulid::new();
        let continuity = load_run_continuity(&pool, session_id, None, false).await.expect("load");
        assert!(continuity.is_empty(), "an empty log is the truthful empty state");

        let mut task = build_run_task(
            session_id,
            TaskExecutionMode::ACTION_REQUIRED,
            false,
            None,
            None,
            "continue",
        );
        seed_run_continuity(&mut task, &pool, session_id, None, false).await;
        assert_eq!(
            task.description, "continue",
            "an empty ledger seeds nothing — the resume text is untouched"
        );
    }
}
