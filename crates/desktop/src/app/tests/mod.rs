//! Desktop `App` unit-test suite for `super` (the `app` module).
//!
//! Mechanical extraction (NORM S45): the entire `mod tests` block moved
//! verbatim out of `crates/desktop/src/app.rs` so test names, assertions,
//! and helpers are unchanged; the module path stays `app::tests::…`.

use super::{
    configured_default_route, orchestration_hides_relationships, AgentOutput, App,
    DesktopApprovalSink, EventBus, Message, Page, PolicyAction, RunStatus, ThinkingKind, Ulid,
};
use crate::views::chat::Message as ChatMessage;
use crate::views::settings::Message as SettingsMessage;
use concerto_config::{AppConfig, ProviderConfig};
use concerto_core::event::EventKind;
use concerto_core::intent::{PlanDecision, RequestedOutcome, RunStage};
use concerto_core::traits::approval::{ApprovalDecision, ApprovalSink};
use concerto_core::types::{AgentCompletionStatus, TaskId};
use concerto_core::CancellationToken;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

// ── ADR-60 D7 (interrupt-safe resume): the graceful window-close path ──

/// `is_run_active` reflects the run status, and every settle point bumps
/// the settlement epoch the window-close handler polls.
#[test]
fn run_settle_epoch_tracks_run_settlement() {
    let (mut app, _) = App::new();
    assert!(!app.is_run_active(), "idle at construction");
    let before = app.run_settle_epoch().load(Ordering::Acquire);

    app.run_status = RunStatus::Running;
    assert!(app.is_run_active(), "a running run is active");

    // The completion settle point: status back to Idle + epoch bump.
    app.run_status = RunStatus::Idle;
    app.note_run_settled();
    assert!(!app.is_run_active());
    let after = app.run_settle_epoch().load(Ordering::Acquire);
    assert_eq!(after, before + 1, "a settled run bumps the epoch");
}

/// Serializes tests that redirect `XDG_CONFIG_HOME` (which `dirs` reads
/// for the config directory on Linux) against each other — env vars are
/// process-global and cargo runs tests in parallel threads. Mirrors
/// `PROJECT_ROOTS_ENV_LOCK` (concerto-config) and `ENV_LOCK` (concerto-cli).
static CONFIG_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn new_creates_initial_state() {
    let (app, _) = App::new();
    assert_eq!(app.page, Page::Chat);
}

#[test]
fn navigate_changes_page() {
    let (mut app, _) = App::new();
    let _ = app.update(Message::Navigate(Page::Settings));
    assert_eq!(app.page, Page::Settings);
}

/// The read-only Studio runtime snapshot is applied only for the active
/// session; a result for a session the user has left is discarded.
#[test]
fn studio_runtime_loaded_applies_only_for_the_active_session() {
    use crate::views::studio_runtime::StudioRuntimeSnapshot;

    let (mut app, _) = App::new();
    let active = Ulid::new();
    app.active_session_id = Some(active);
    assert!(app.orchestration_studio.runtime_snapshot.load_error.is_none());

    // A result for a different session is dropped.
    let _ = app.update(Message::StudioRuntimeLoaded(
        Some(Ulid::new()),
        Box::new(StudioRuntimeSnapshot::unavailable("stale")),
    ));
    assert!(
        app.orchestration_studio.runtime_snapshot.load_error.is_none(),
        "a stale-session result must be discarded"
    );

    // A result for the active session is applied.
    let _ = app.update(Message::StudioRuntimeLoaded(
        Some(active),
        Box::new(StudioRuntimeSnapshot::unavailable("fresh")),
    ));
    assert_eq!(app.orchestration_studio.runtime_snapshot.load_error.as_deref(), Some("fresh"));
}

#[test]
fn help_toggle_works() {
    let (mut app, _) = App::new();
    assert!(!app.show_help);
    let _ = app.update(Message::HelpToggled);
    assert!(app.show_help);
    let _ = app.update(Message::HelpToggled);
    assert!(!app.show_help);
}

#[test]
fn new_session_reapplies_reduced_motion_to_the_fresh_chat() {
    let (mut app, _) = App::new();
    // Simulate a user who enabled reduced-motion (the default config
    // disables it): an interrupted NewSession reset must not revert the
    // fresh chat to the factory default and silently re-enable the
    // animations the setting turned off.
    app.reduced_motion = true;
    let _ = app.update(Message::Chat(ChatMessage::NewSession));
    assert!(app.chat.reduced_motion(), "a fresh chat must honor the reduced-motion override");
}

#[test]
fn diff_viewer_shortcut_sets_subview_diff() {
    let (mut app, _) = App::new();
    let _ = app.update(Message::Shortcut(crate::shortcuts::Shortcut::DiffViewer));
    assert_eq!(app.page, Page::Chat);
    assert_eq!(app.chat.sub_view, crate::views::chat::SubView::Diff);
}

/// Ctrl+R opens the per-session Runtime modal in the chat canvas, and
/// pressing it again closes it (toggle, mirroring Ctrl+D / Ctrl+L).
#[test]
fn runtime_shortcut_toggles_the_runtime_subview() {
    let (mut app, _) = App::new();
    let _ = app.update(Message::Shortcut(crate::shortcuts::Shortcut::RuntimePanels));
    assert_eq!(app.page, Page::Chat);
    assert_eq!(app.chat.sub_view, crate::views::chat::SubView::Runtime);

    let _ = app.update(Message::Shortcut(crate::shortcuts::Shortcut::RuntimePanels));
    assert_eq!(app.chat.sub_view, crate::views::chat::SubView::Main);
}

/// Esc closes the Runtime modal in the chat canvas (mirrors the Memory
/// modal's Esc dismissal).
#[test]
fn escape_closes_the_runtime_modal() {
    let (mut app, _) = App::new();
    let _ = app.update(Message::SetSubView(crate::views::chat::SubView::Runtime));
    assert_eq!(app.chat.sub_view, crate::views::chat::SubView::Runtime);

    let _ = app.update(Message::Shortcut(crate::shortcuts::Shortcut::CancelDialog));
    assert_eq!(app.chat.sub_view, crate::views::chat::SubView::Main);
}

// ── ADR-57 — config reload reconciliation ──────────────────────────────
//
// `apply_reloaded_config` is the harness-free half of
// `reconcile_config_from_reload` (which loads from disk); feeding it
// already-parsed configs exercises the equality short-circuit and the
// full re-derivation deterministically.

#[test]
fn apply_reloaded_config_is_noop_when_content_is_equal() {
    let (mut app, _) = App::new();
    // Seed a broken flag: a successful (even no-op) reload must clear it.
    app.config_broken = true;
    let snapshot = (
        app.multi_agent,
        app.active_provider_id.clone(),
        app.active_model.clone(),
        app.chat_model_options.clone(),
        app.session_cap,
        app.global_config.clone(),
    );

    app.apply_reloaded_config(snapshot.5.clone(), app.config.clone().expect("App config"));

    assert!(!app.config_broken, "a successful no-op reload clears the broken flag");
    assert_eq!(app.multi_agent, snapshot.0, "run-mode must be untouched on a no-op");
    assert_eq!(app.active_provider_id, snapshot.1);
    assert_eq!(app.active_model, snapshot.2);
    assert_eq!(app.chat_model_options, snapshot.3);
    assert_eq!(app.session_cap, snapshot.4);
    assert_eq!(app.global_config, snapshot.5);
}

#[test]
fn apply_reloaded_config_rederives_on_differing_config() {
    let (mut app, _) = App::new();
    let mut expected = app.config.clone().expect("App config");
    // Force a deterministic difference without touching the route inputs,
    // so re-derivation is observable on the cap, memory flag, and the
    // run-mode toggle parity (which must reflect the file, not reset).
    let multi_before = app.multi_agent;
    expected.session_spend_cap_usd = Some(2.0);
    expected.memory.enabled = false;

    app.apply_reloaded_config(expected.clone(), expected.clone());

    assert_eq!(app.config.as_ref(), Some(&expected), "config must be replaced");
    assert_eq!(app.multi_agent, multi_before, "run-mode re-derives from the file");
    assert_eq!(app.session_cap, Some(2.0), "session cap re-derives from the file");
    let expected_route = configured_default_route(&expected);
    assert_eq!(app.active_provider_id, expected_route.0);
    assert_eq!(app.active_model, expected_route.1);
    assert!(
        matches!(app.memory.status, crate::views::memory::MemoryStatus::Disabled),
        "memory view flag must track the reloaded config"
    );
    assert!(!app.config_broken);
}

#[test]
fn apply_reloaded_config_refreshes_settings_provider_rows() {
    let (mut app, _) = App::new();
    let mut reloaded = app.config.clone().expect("App config");
    // The external edit adds a provider row to `model_settings`.
    reloaded.model_settings.get_or_insert_with(Default::default).providers =
        vec![concerto_config::ProviderConfig {
            id: "external".into(),
            provider: "openai".into(),
            model: "gpt-4o".into(),
            ..Default::default()
        }];

    app.apply_reloaded_config(reloaded.clone(), reloaded);

    assert!(
        app.settings.providers.iter().any(|p| p.id == "external"),
        "the Settings provider rows must reflect the reloaded config"
    );
    assert!(
        app.settings.cached_provider_ids.contains(&"external".to_string()),
        "the provider caches must reflect the reloaded config"
    );
}

#[test]
fn memory_teardown_is_deferred_while_run_is_active() {
    let (mut app, _) = App::new();
    let mut reloaded = app.config.clone().expect("App config");
    reloaded.memory.enabled = false;

    // Mid-run: the disabled flag reaches the view immediately, but the
    // memory-service slot must not be torn down under an active run.
    // (The slot is empty here, so this verifies the reconcile reaches the
    // memory section during a run without touching the slot; completion
    // of the deferred teardown happens in `Message::AgentRunCompleted`.)
    app.run_status = RunStatus::Running;
    app.apply_reloaded_config(reloaded.clone(), reloaded.clone());
    assert!(
        matches!(app.memory.status, crate::views::memory::MemoryStatus::Disabled),
        "memory view flag updates even mid-run"
    );
    assert!(
        app.memory_services.lock().unwrap_or_else(|e| e.into_inner()).is_none(),
        "no teardown may run while a run is active"
    );

    // At idle the same reload must also settle cleanly (the deferred
    // teardown path is a no-op when the slot is empty).
    app.run_status = RunStatus::Idle;
    app.apply_reloaded_config(reloaded.clone(), reloaded);
    assert!(matches!(app.memory.status, crate::views::memory::MemoryStatus::Disabled));
}

// ── ADR-59 — blueprint apply path + first-run init ─────────────────────

/// ADR-59 D4 (apply-path test, content-aware): an include-file content
/// change leaves the persisted surface equal (`AppConfig`'s `PartialEq`
/// deliberately excludes `resolved_blueprint`) but moves the resolved
/// model — the reload must NOT short-circuit, and the live config must
/// consume the freshly resolved blueprint. The pre-batch mirror
/// (`apply_reloaded_config_is_noop_when_content_is_equal`) cannot catch
/// this because its equality holds on the persisted surface.
#[test]
fn apply_reloaded_config_applies_blueprint_content_change() {
    let (mut app, _) = App::new();
    let mut reloaded = app.config.clone().expect("App config");
    let live_blueprint = app
        .config
        .as_ref()
        .and_then(|c| c.resolved_blueprint.clone())
        .expect("resolved blueprint attached by the load seam");

    // Re-resolve a content-edited blueprint (a watched include-file
    // change: new pipeline name, renamed stage, added stage cap). The
    // persisted `[orchestration]` selection is untouched.
    let mut edited = live_blueprint.blueprint.clone();
    edited.name = "edited-include".to_string();
    edited.pipeline.stages[0].label = "Renamed by include edit".to_string();
    edited.pipeline.stages[0].max_cycles = Some(2);
    let rebased =
        concerto_config::resolve_blueprint(&edited).expect("edited blueprint must resolve");
    reloaded.resolved_blueprint = Some(Arc::new(rebased));

    // This is exactly the ADR-59 D4 no-op trap: persisted-surface equality
    // holds while the resolved model moves.
    assert_eq!(
        &reloaded,
        app.config.as_ref().expect("live config"),
        "persisted surface equality holds after the content edit"
    );
    assert_ne!(
        reloaded.resolved_blueprint.as_ref(),
        Some(&live_blueprint),
        "the resolved model must differ after the include-content edit"
    );

    app.apply_reloaded_config(reloaded.clone(), reloaded.clone());

    assert_eq!(
        app.config.as_ref().and_then(|c| c.resolved_blueprint.clone()),
        reloaded.resolved_blueprint,
        "the live config must consume the freshly resolved blueprint"
    );
    assert_eq!(
        app.config
            .as_ref()
            .and_then(|c| c.resolved_blueprint.as_ref().map(|r| r.blueprint.name.as_str())),
        Some("edited-include"),
        "the live blueprint content must change after apply"
    );
    assert!(!app.config_broken, "a successful apply clears the broken flag");
}

/// ADR-58/59 (rewritten) Slice 2 (first-run bootstrap): opening the Studio for the first
/// time auto-seeds the orchestration roster into the PROJECT config
/// (`.concerto.toml`) — `[orchestration]` with the standard blueprint
/// inlined + the five `[multi_agent.custom_agents]` seeds. No splash, no
/// manual init: the config owns its roster afterwards, the blueprint
/// resolves from the written file, the global `config.toml` is untouched,
/// and no include file is created.
#[test]
fn first_studio_open_auto_seeds_the_orchestration_roster() {
    let _guard = CONFIG_ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let dir = tempfile::tempdir().expect("tempdir");
    let previous = std::env::var_os("XDG_CONFIG_HOME");
    std::env::set_var("XDG_CONFIG_HOME", dir.path());

    // Global config: schema only — the seed lands here. Project dir is
    // empty: no config file, no include file (a brand-new project).
    let global_dir = dir.path().join("concerto");
    std::fs::create_dir_all(&global_dir).expect("create global config dir");
    let global_config_path = global_dir.join("config.toml");
    std::fs::write(&global_config_path, "schema_version = 7\n").expect("seed global config");
    let project_dir = dir.path().join("project");
    std::fs::create_dir_all(&project_dir).expect("create project dir");

    let (mut app, _) = App::new();
    app.project_dir = project_dir.clone();
    // Force the no-roster shape (global has no materialized roster) so
    // `ensure_orchestration_seeded` must really write.
    app.config = None;
    // The first Studio open is exactly what triggers the auto-seed.
    let _ = app.update(Message::Navigate(Page::OrchestrationStudio));

    // Env restored before assertions so a panic cannot leak the redirect.
    match previous {
        Some(value) => std::env::set_var("XDG_CONFIG_HOME", value),
        None => std::env::remove_var("XDG_CONFIG_HOME"),
    }

    // The seed landed in the GLOBAL config layer (global-only
    // orchestration — never a project file).
    let raw = std::fs::read_to_string(&global_config_path).expect("global config read back");
    assert!(raw.contains("[orchestration]"), "roster section written\n{raw}");
    assert!(
        raw.contains("blueprint = { inline = {") && raw.contains("name = \"standard\""),
        "the standard blueprint must be seeded inline\n{raw}"
    );
    assert_eq!(
        raw.matches("[[multi_agent.custom_agents]]").count(),
        5,
        "five seeded agents expected\n{raw}"
    );
    // No project config file may be created by the seed — creating a
    // file is an explicit user save only.
    assert!(
        !project_dir.join(concerto_config::legacy::NEW_PROJECT_CONFIG_FILE).exists(),
        "the seed must never create a project .concerto.toml"
    );
    // No include file is created — the seed is inline, not include-based.
    assert!(
        !project_dir.join(concerto_config::BLUEPRINT_INCLUDE_FILE).exists(),
        "the seed must not write a blueprint include file"
    );

    // App state owns the roster and the Studio is already on the blueprint
    // path from the very first open (no splash to clear).
    let config = app.config.as_ref().expect("config loaded after the seed");
    assert!(config.owns_agent_roster(), "a seeded config owns its roster");
    assert_eq!(
        config.orchestration.as_ref().and_then(|o| o
            .blueprint
            .inline
            .as_ref()
            .map(|b| b.name.as_str())),
        Some("standard"),
        "the seeded selection is inline-standard"
    );
    assert_eq!(
        config.resolved_blueprint.as_ref().map(|r| r.blueprint.pipeline.stages.len()),
        Some(5),
        "the seeded standard blueprint resolves with a five-stage pipeline"
    );
    assert!(
        app.orchestration_studio.blueprint().is_some(),
        "the Studio must hold the editable blueprint from the first open"
    );
}

/// Global-only blueprint fill on Studio open: a global config that owns
/// its roster (`custom_agents` present) but declares `[orchestration]`
/// with NO selector used to hit the old early return and stay
/// blueprint-selection-less forever. The seed flow now fills the default
/// standard selection into the global file (never a project file), so the
/// Studio activates the blueprint surface from the first open.
#[test]
fn ensure_orchestration_seeded_fills_a_missing_blueprint_selection() {
    let _guard = CONFIG_ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let dir = tempfile::tempdir().expect("tempdir");
    let previous = std::env::var_os("XDG_CONFIG_HOME");
    std::env::set_var("XDG_CONFIG_HOME", dir.path());

    let global_dir = dir.path().join("concerto");
    std::fs::create_dir_all(&global_dir).expect("create global config dir");
    let global_config_path = global_dir.join("config.toml");
    // Roster owned (custom_agents key present, no agents), orchestration
    // declared, but no name/include/inline selection.
    std::fs::write(
            &global_config_path,
            "schema_version = 7\n\n[orchestration]\nschema_version = 1\n\n[multi_agent]\ncustom_agents = []\n",
        )
        .expect("seed global config");
    let project_dir = dir.path().join("project");
    std::fs::create_dir_all(&project_dir).expect("create project dir");

    let (mut app, _) = App::new();
    app.project_dir = project_dir.clone();
    let _ = app.update(Message::Navigate(Page::OrchestrationStudio));

    // Env restored before assertions so a panic cannot leak the redirect.
    match previous {
        Some(value) => std::env::set_var("XDG_CONFIG_HOME", value),
        None => std::env::remove_var("XDG_CONFIG_HOME"),
    }

    let raw = std::fs::read_to_string(&global_config_path).expect("global config read back");
    assert!(
        raw.contains("name = \"standard\""),
        "the missing selection must be filled with the standard default\n{raw}"
    );
    // The roster was already owned — the fill must not have re-seeded it.
    assert!(raw.contains("custom_agents = []"), "the owned roster must be preserved\n{raw}");
    assert!(
        !project_dir.join(concerto_config::legacy::NEW_PROJECT_CONFIG_FILE).exists(),
        "the fill must never create a project .concerto.toml"
    );

    let config = app.config.as_ref().expect("config loaded after the fill");
    assert_eq!(
        config.resolved_blueprint.as_ref().map(|r| r.blueprint.name.as_str()),
        Some("standard"),
        "the filled selection must resolve to the standard blueprint"
    );
    assert!(
        app.orchestration_studio.blueprint().is_some(),
        "the Studio must hold the editable blueprint from the first open"
    );
}

/// ADR-59 D5 (startup-fallback test): when config loading falls back to
/// defaults at startup (unparsable `config.toml`), `App::new` must surface
/// it via `config_broken` instead of failing silently.
#[test]
fn startup_config_load_failure_marks_config_broken() {
    let _guard = CONFIG_ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let dir = tempfile::tempdir().expect("tempdir");
    // `dirs` resolves the config dir from `XDG_CONFIG_HOME` on Linux, so
    // the redirect makes both load seams read the broken file.
    let config_dir = dir.path().join("concerto");
    std::fs::create_dir_all(&config_dir).expect("create config dir");
    std::fs::write(config_dir.join("config.toml"), "[unterminated\n").expect("seed broken config");

    let previous = std::env::var_os("XDG_CONFIG_HOME");
    std::env::set_var("XDG_CONFIG_HOME", dir.path());
    let (app, _) = App::new();
    match previous {
        Some(value) => std::env::set_var("XDG_CONFIG_HOME", value),
        None => std::env::remove_var("XDG_CONFIG_HOME"),
    }

    assert!(app.config_broken, "startup config fallback must surface via config_broken");
}

/// ADR-58/59 (rewritten) Slice 2 (orphan contract), AMENDED (global-only):
/// when the GLOBAL config owns its roster — the raw file carries the
/// `custom_agents` key, even as `[]` meaning every agent was deleted —
/// the auto-seed is a strict no-op: nothing is written and the global
/// file stays byte-identical ("key present" = owned; deletions stick).
#[test]
fn ensure_orchestration_seeded_is_a_noop_when_key_present_even_empty() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config_path = dir.path().join(concerto_config::legacy::NEW_PROJECT_CONFIG_FILE);
    let content = r#"# sentinel comment
schema_version = 7

[orchestration]
schema_version = 1

[multi_agent]
custom_agents = []
"#;
    std::fs::write(&config_path, content).expect("seed owned-roster config");
    let before = std::fs::read_to_string(&config_path).expect("read before");

    let (mut app, _) = App::new();
    app.project_dir = dir.path().to_path_buf();
    app.config = Some(AppConfig {
        orchestration: Some(concerto_config::OrchestrationConfig::default()),
        ..AppConfig::default()
    });
    let owned = app.config.as_ref().and_then(|config| config.orchestration.as_ref()).cloned();

    app.ensure_orchestration_seeded();

    let after = std::fs::read_to_string(&config_path).expect("read back");
    assert_eq!(after, before, "an owned roster (even empty) must never trigger a write");
    assert_eq!(
        app.config.as_ref().and_then(|config| config.orchestration.as_ref()),
        owned.as_ref(),
        "the owned orchestration selection is left untouched"
    );
}

/// ADR-58/59 (rewritten) Slice 2 (orphan self-heal), AMENDED (global-only
/// orchestration): a GLOBAL config carrying `[orchestration]` (a custom
/// blueprint) but NO materialized `custom_agents` key is the orphan
/// shape — the auto-seed writes ONLY the five seed agents under
/// `[multi_agent.custom_agents]` of the global config and preserves the
/// existing orchestration blueprint text unchanged, so the Studio's
/// searchable library matches the blueprint's staffing. The project
/// directory stays untouched (no project file is created).
#[test]
fn ensure_orchestration_seeded_self_heals_the_orphan_shape() {
    let _guard = CONFIG_ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let dir = tempfile::tempdir().expect("tempdir");
    let previous = std::env::var_os("XDG_CONFIG_HOME");
    std::env::set_var("XDG_CONFIG_HOME", dir.path());

    let global_dir = dir.path().join("concerto");
    std::fs::create_dir_all(&global_dir).expect("create global config dir");
    let config_path = global_dir.join("config.toml");
    let content = r#"schema_version = 7

[orchestration]
schema_version = 1

[orchestration.blueprint]
name = "custom-blueprint"
description = "keep me"
"#;
    std::fs::write(&config_path, content).expect("seed orphan-shape config");
    let project_dir = dir.path().join("project");
    std::fs::create_dir_all(&project_dir).expect("create project dir");

    let (mut app, _) = App::new();
    app.project_dir = project_dir.clone();
    app.reconcile_config_from_reload();

    app.ensure_orchestration_seeded();

    // Env restored before assertions so a panic cannot leak the redirect.
    match previous {
        Some(value) => std::env::set_var("XDG_CONFIG_HOME", value),
        None => std::env::remove_var("XDG_CONFIG_HOME"),
    }

    let after = std::fs::read_to_string(&config_path).expect("read back");
    // The agents-only seed preserves the existing orchestration blueprint.
    assert!(
        after.contains("\"custom-blueprint\"") && after.contains("\"keep me\""),
        "the orchestration blueprint must be preserved\n{after}"
    );
    // Exactly the five seed agents are now materialized.
    assert_eq!(
        after.matches("[[multi_agent.custom_agents]]").count(),
        5,
        "five seeded agents expected\n{after}"
    );
    for id in ["architect", "researcher", "coder", "reviewer", "validator"] {
        assert!(after.contains(&format!("id = \"{id}\"")), "seed agent {id} missing\n{after}");
    }
    // The seed never creates a project config file.
    assert!(
        !project_dir.join(concerto_config::legacy::NEW_PROJECT_CONFIG_FILE).exists(),
        "the seed must never create a project .concerto.toml\n"
    );
}

/// Back-compat seed no-op: when a PROJECT config owns its roster (the
/// raw `custom_agents` key is present there), the auto-seed is skipped
/// entirely — the project keeps being authoritative for itself (its
/// orchestration keeps loading as today) and neither the project file
/// nor the global config is written.
#[test]
fn seed_is_skipped_and_global_untouched_when_the_project_owns_the_roster() {
    let dir = tempfile::tempdir().expect("tempdir");
    let project_config = dir.path().join(concerto_config::legacy::NEW_PROJECT_CONFIG_FILE);
    let content = r#"# sentinel comment
schema_version = 7

[orchestration]
schema_version = 1

[multi_agent]
custom_agents = []
"#;
    std::fs::write(&project_config, content).expect("seed owned-roster config");
    let before = std::fs::read_to_string(&project_config).expect("read before");

    let (mut app, _) = App::new();
    app.project_dir = dir.path().to_path_buf();
    app.config = Some(AppConfig {
        orchestration: Some(concerto_config::OrchestrationConfig::default()),
        ..AppConfig::default()
    });
    let owned = app.config.as_ref().and_then(|config| config.orchestration.as_ref()).cloned();

    app.ensure_orchestration_seeded();

    let after = std::fs::read_to_string(&project_config).expect("read back");
    assert_eq!(after, before, "an owned roster (even empty) must never trigger a write");
    assert_eq!(
        app.config.as_ref().and_then(|config| config.orchestration.as_ref()),
        owned.as_ref(),
        "the owned orchestration selection is left untouched"
    );
}

/// ADR-58/59 (rewritten) Slice 2 (single-arm Save, name source), AMENDED
/// (global-only orchestration): a selection that is a bare catalog
/// `name` (the code-catalog is seed-only) is materialized — Save writes
/// the edited blueprint inline into the GLOBAL config, the dangling
/// `name` selector is removed (exactly-one selection), the project
/// directory stays file-free, and a full reload consumes the EDITS
/// (the B1 property: the runtime reads what Save wrote, not the catalog).
#[test]
fn save_materializes_a_name_selection_inline_into_the_global_config() {
    let _guard = CONFIG_ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let dir = tempfile::tempdir().expect("tempdir");
    let previous = std::env::var_os("XDG_CONFIG_HOME");
    std::env::set_var("XDG_CONFIG_HOME", dir.path());

    let global_dir = dir.path().join("concerto");
    std::fs::create_dir_all(&global_dir).expect("create global config dir");
    let global_config_path = global_dir.join("config.toml");
    std::fs::write(&global_config_path, "schema_version = 7\n").expect("seed global config");
    // A bare name-based selection in the global layer (catalog shape).
    concerto_config::save_blueprint_selection(
        &global_config_path,
        &concerto_config::BlueprintSelection {
            name: Some("standard".to_string()),
            include: None,
            inline: None,
        },
    )
    .expect("seed name selection");
    let project_dir = dir.path().join("project");
    std::fs::create_dir_all(&project_dir).expect("create project dir");

    let (mut app, _) = App::new();
    app.project_dir = project_dir.clone();
    app.reconcile_config_from_reload();
    let config = app.config.clone().expect("config loaded after reconcile");
    assert_eq!(
        config.orchestration.as_ref().and_then(|o| o.blueprint.name.as_deref()),
        Some("standard"),
        "precondition: the selection is catalog-name based"
    );
    app.orchestration_studio.load_from_config(&config);

    // Seed the specialist roster (production does this on Studio open via
    // `ensure_orchestration_seeded`) so the standard blueprint's staffing
    // satisfies the roster-membership rule and Save is not pre-empted.
    let _ = app.update(Message::OrchestrationStudio(
        crate::views::orchestration_studio::StudioMessage::RestoreDefaultAgents,
    ));

    // Edit the first stage's label, then Save.
    let _ = app.orchestration_studio.update(
        crate::views::orchestration_studio::StudioMessage::StageLabelEdited(0, "planning".into()),
    );
    let _ = app.update(Message::OrchestrationStudio(
        crate::views::orchestration_studio::StudioMessage::SaveOrchestration,
    ));

    // All app operations stay under the XDG redirect — whole-config
    // persistence paths serialize the app's in-memory global config, and
    // running them against the real machine config would leak test state
    // there. The env is restored only after every app call below.
    app.reconcile_config_from_reload();
    let reloaded = app.config.clone().expect("config after reload");

    // Env restored before assertions so a panic cannot leak the redirect.
    match previous {
        Some(value) => std::env::set_var("XDG_CONFIG_HOME", value),
        None => std::env::remove_var("XDG_CONFIG_HOME"),
    }

    assert!(!app.orchestration_studio.unsaved, "a successful save marks the studio clean");
    let after = std::fs::read_to_string(&global_config_path).expect("global config read back");
    assert!(after.contains("inline = {"), "save must write the blueprint inline\n{after}");
    // Save is global-only: no project config file may appear.
    assert!(
        !project_dir.join(concerto_config::legacy::NEW_PROJECT_CONFIG_FILE).exists(),
        "creating a project file is an explicit user save, not a Save side effect"
    );

    // The materialized selection is exactly-one (inline) — the load seam
    // rejects any dangling sibling selector, so a successful reload is
    // itself the proof the `name` selector was removed.
    let selection = reloaded.orchestration.as_ref().expect("[orchestration] present");
    assert!(selection.blueprint.name.is_none(), "the name selector must be removed");
    assert!(selection.blueprint.include.is_none(), "no include selector may appear");
    assert!(selection.blueprint.inline.is_some(), "the selection must be inline");
    let reloaded_label =
        reloaded.resolved_blueprint.as_ref().map(|r| r.blueprint.pipeline.stages[0].label.as_str());
    assert_eq!(
        reloaded_label,
        Some("planning"),
        "the runtime must load the edited blueprint — not the catalog standard"
    );
}

/// Global-only Save conflict guard: while a project `.concerto.toml`
/// still declares `[orchestration]`, the inline save is refused (draft
/// kept, NOTHING written) — writing the fresher selection to the global
/// file would leave the stale project selection merged on top and the
/// exactly-one load seam would reject the mixed selection. Project
/// orchestration data is never silently deleted by a save.
#[test]
fn save_is_refused_when_the_project_config_still_declares_orchestration() {
    let _guard = CONFIG_ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let dir = tempfile::tempdir().expect("tempdir");
    let previous = std::env::var_os("XDG_CONFIG_HOME");
    std::env::set_var("XDG_CONFIG_HOME", dir.path());

    let global_dir = dir.path().join("concerto");
    std::fs::create_dir_all(&global_dir).expect("create global config dir");
    let global_config_path = global_dir.join("config.toml");
    std::fs::write(&global_config_path, "schema_version = 7\n").expect("seed global config");
    let global_before = std::fs::read_to_string(&global_config_path).expect("read global");

    let project_dir = dir.path().join("project");
    std::fs::create_dir_all(&project_dir).expect("create project dir");
    let project_config = project_dir.join(concerto_config::legacy::NEW_PROJECT_CONFIG_FILE);
    std::fs::write(&project_config, "schema_version = 7\n").expect("seed project config");
    concerto_config::save_blueprint_selection(
        &project_config,
        &concerto_config::BlueprintSelection {
            name: Some("standard".to_string()),
            include: None,
            inline: None,
        },
    )
    .expect("seed project name selection");
    let project_before = std::fs::read_to_string(&project_config).expect("read project");

    let (mut app, _) = App::new();
    app.project_dir = project_dir.clone();
    app.reconcile_config_from_reload();
    let config = app.config.clone().expect("config loaded after reconcile");
    app.orchestration_studio.load_from_config(&config);
    let _ = app.orchestration_studio.update(
        crate::views::orchestration_studio::StudioMessage::StageLabelEdited(0, "planning".into()),
    );
    let _ = app.update(Message::OrchestrationStudio(
        crate::views::orchestration_studio::StudioMessage::SaveOrchestration,
    ));

    // Env restored before assertions so a panic cannot leak the redirect.
    match previous {
        Some(value) => std::env::set_var("XDG_CONFIG_HOME", value),
        None => std::env::remove_var("XDG_CONFIG_HOME"),
    }

    // The save is refused with the failure surfaced, nothing written.
    assert!(
        app.orchestration_studio.save_error.is_some(),
        "the save conflict must surface on the studio"
    );
    assert!(app.toasts.has_toasts(), "the save conflict must surface as a toast");
    let global_after = std::fs::read_to_string(&global_config_path).expect("read global");
    assert_eq!(global_before, global_after, "the global config must stay untouched");
    let project_after = std::fs::read_to_string(&project_config).expect("read project");
    assert_eq!(project_before, project_after, "the project config must stay untouched");
}

/// ADR-58/59 (rewritten) Slice 2 (single-arm Save, guard), AMENDED
/// (global-only): a validation-invalid draft is rejected on the inline
/// path too — nothing is written, the draft is kept, and the failure is
/// surfaced (studio error + error toast). The auto-seed that ran on the
/// Studio open wrote the GLOBAL config; the project directory must stay
/// file-free after the failed save.
#[test]
fn save_rejects_an_invalid_draft_on_the_inline_path() {
    let _guard = CONFIG_ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let dir = tempfile::tempdir().expect("tempdir");
    let previous = std::env::var_os("XDG_CONFIG_HOME");
    std::env::set_var("XDG_CONFIG_HOME", dir.path());

    std::fs::create_dir_all(dir.path().join("concerto")).expect("create global config dir");
    std::fs::write(dir.path().join("concerto").join("config.toml"), "schema_version = 7\n")
        .expect("seed global config");
    let project_dir = dir.path().join("project");
    std::fs::create_dir_all(&project_dir).expect("create project dir");

    let (mut app, _) = App::new();
    app.project_dir = project_dir.clone();
    // Same shape as the seed matrix (see the first-open test): the
    // machine's persisted registry must not pre-own a roster, or the
    // seed short-circuits.
    app.config = None;
    // First Studio open auto-seeds the inline roster — globally now.
    let _ = app.update(Message::Navigate(Page::OrchestrationStudio));
    let global_config_path = dir.path().join("concerto").join("config.toml");
    let before = std::fs::read_to_string(&global_config_path).expect("read global config before");

    // Force a rulebook violation the UI would flag: an empty stage tag
    // (rule (g), "stage tag must be non-empty").
    let _ = app
        .orchestration_studio
        .update(crate::views::orchestration_studio::StudioMessage::StageTagEdited(0, "".into()));
    assert!(
        !app.orchestration_studio.validation().ok,
        "the edited draft must be invalid (precondition)"
    );
    let _ = app.update(Message::OrchestrationStudio(
        crate::views::orchestration_studio::StudioMessage::SaveOrchestration,
    ));

    // Env restored before assertions so a panic cannot leak the redirect.
    match previous {
        Some(value) => std::env::set_var("XDG_CONFIG_HOME", value),
        None => std::env::remove_var("XDG_CONFIG_HOME"),
    }

    let after = std::fs::read_to_string(&global_config_path).expect("read global config after");
    assert_eq!(before, after, "an invalid draft must never reach the config");
    assert!(
        !project_dir.join(concerto_config::legacy::NEW_PROJECT_CONFIG_FILE).exists(),
        "the seed and a failed save must never create a project config"
    );
    assert!(
        app.orchestration_studio.save_error.is_some(),
        "the save failure must surface on the studio"
    );
    assert!(app.toasts.has_toasts(), "the save failure must surface as a toast");
}

/// ADR-58/59 (rewritten) Slice 3, AMENDED (global-only): a valid Save
/// writes the agent roster — the Studio's authoritative agent list
/// (mirrors of the seeds plus user agents) — into
/// `[multi_agent.custom_agents]` of the GLOBAL config, atomically and
/// merge-aware, while the project directory stays file-free. The roster
/// has no rulebook of its own, so it is gated on the same blueprint
/// validation that gates the blueprint write (an invalid draft never
/// reaches the config at all — covered by
/// `save_rejects_an_invalid_draft_on_the_inline_path`).
#[test]
fn save_writes_the_agent_roster_alongside_the_blueprint() {
    let _guard = CONFIG_ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let dir = tempfile::tempdir().expect("tempdir");
    let previous = std::env::var_os("XDG_CONFIG_HOME");
    std::env::set_var("XDG_CONFIG_HOME", dir.path());

    std::fs::create_dir_all(dir.path().join("concerto")).expect("create global config dir");
    std::fs::write(dir.path().join("concerto").join("config.toml"), "schema_version = 7\n")
        .expect("seed global config");
    let project_dir = dir.path().join("project");
    std::fs::create_dir_all(&project_dir).expect("create project dir");

    let (mut app, _) = App::new();
    app.project_dir = project_dir.clone();
    // Same shape as the seed matrix: the machine's persisted registry
    // must not pre-own a roster, or the seed short-circuits.
    app.config = None;
    let _ = app.update(Message::Navigate(Page::OrchestrationStudio));

    // Add a user agent to the roster (mirrors Add/Rename in the library).
    let _ = app
        .orchestration_studio
        .update(crate::views::orchestration_studio::StudioMessage::NewAgentName("Planner".into()));
    let _ = app
        .orchestration_studio
        .update(crate::views::orchestration_studio::StudioMessage::AddAgent);
    assert!(app.orchestration_studio.unsaved, "a roster edit marks the studio dirty");

    let _ = app.update(Message::OrchestrationStudio(
        crate::views::orchestration_studio::StudioMessage::SaveOrchestration,
    ));

    assert!(
        !app.orchestration_studio.unsaved,
        "a successful save marks the studio clean (blueprint + roster)"
    );
    // The roster is written to per-agent files, never inline into the
    // config (single source of truth).
    let agents_dir = dir.path().join("concerto").join(concerto_config::AGENTS_DIR_NAME);
    let after_agents: Vec<String> = std::fs::read_dir(&agents_dir)
        .expect("read agents dir")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|e| e.to_str()) == Some("toml"))
        .filter_map(|path| std::fs::read_to_string(path).ok())
        .collect();
    assert!(
        after_agents.iter().any(|raw| raw.contains("Planner")),
        "the added roster agent must exist as a per-agent file"
    );
    assert!(
        !project_dir.join(concerto_config::legacy::NEW_PROJECT_CONFIG_FILE).exists(),
        "the save must never create a project .concerto.toml"
    );
    // The roster owns the config (owns_agent_roster): a reload keeps it —
    // performed under the redirect (see the round-trip test note) so the
    // reconcile consumes the redirected global file.
    app.reconcile_config_from_reload();
    let reloaded = app.config.clone().expect("config after reload");

    // Env restored after every app operation, before the final assertions.
    match previous {
        Some(value) => std::env::set_var("XDG_CONFIG_HOME", value),
        None => std::env::remove_var("XDG_CONFIG_HOME"),
    }
    assert!(reloaded.owns_agent_roster(), "the global config must own the agent roster");
}

/// Slice 4a (spec §7): the Settings Relationships-hide flag is a pure
/// projection of `[orchestration]` presence — the plumbing the Settings
/// view consumes to gate the legacy "Agent Relationship Manager" section.
#[test]
fn settings_hide_relationships_flag_tracks_orchestration_presence() {
    let mut config = AppConfig::default();
    assert!(
        !orchestration_hides_relationships(&config),
        "without [orchestration] the legacy relationship manager stays visible"
    );
    config.orchestration = Some(concerto_config::OrchestrationConfig::default());
    assert!(
        orchestration_hides_relationships(&config),
        "with [orchestration] the legacy relationship manager is hidden"
    );
}

#[test]
fn new_task_shortcut_clears_the_visible_conversation() {
    let (mut app, _) = App::new();
    let _ = app.chat.update(crate::views::chat::Message::AddUser("old task".into()));
    let _ = app.update(Message::Shortcut(crate::shortcuts::Shortcut::NewTask));
    assert!(app.chat.entries().is_empty());
}

#[test]
fn startup_opens_a_blank_session() {
    let (app, _) = App::new();
    assert!(app.chat.entries().is_empty());
    assert!(app.active_session_id.is_none());
    assert!(app.agent_graph.model.nodes.is_empty());
}

#[test]
fn text_focus_tracks_chat_input() {
    let (mut app, _) = App::new();
    assert!(!app.text_focused);
    let _ = app.update(Message::Chat(crate::views::chat::Message::InputChanged("hello".into())));
    assert!(app.text_focused);
    let _ = app.update(Message::Chat(crate::views::chat::Message::SubmitInput));
    assert!(!app.text_focused);
}

#[test]
fn project_dir_picker_opens_and_cancels() {
    let (mut app, _) = App::new();
    assert!(!app.show_dir_picker);
    let _ = app.update(Message::OpenProjectDirPicker);
    assert!(app.show_dir_picker);
    // The input is pre-filled with the current folder.
    assert_eq!(app.project_dir_input, app.project_dir.to_string_lossy());
    let _ = app.update(Message::ProjectDirCancel);
    assert!(!app.show_dir_picker);
}

#[test]
fn project_dir_apply_switches_active_folder() {
    let (mut app, _) = App::new();
    let _ = app.chat.update(crate::views::chat::Message::AddUser("old task".into()));
    let target = std::env::temp_dir();
    let _ = app.update(Message::ProjectDirInputChanged(target.to_string_lossy().to_string()));
    let _ = app.update(Message::ProjectDirApply);
    let expected = target.canonicalize().unwrap_or_else(|_| target.to_path_buf());
    assert_eq!(app.project_dir, expected);
    assert!(!app.show_dir_picker);
    // Switching rebinds the session handler so the next run uses the new folder.
    assert!(app.session_manager.lock().unwrap_or_else(|e| e.into_inner()).is_none());
    assert!(app.chat.entries().is_empty());
    assert!(app.active_session_id.is_none());
}

// -----------------------------------------------------------------------
// ADR-44 §4 — out-of-root consent gate
// -----------------------------------------------------------------------

/// An out-of-root target defers the switch to the consent gate; Deny
/// aborts it cleanly without changing the project or showing an error.
#[test]
fn out_of_root_project_apply_requires_consent_and_deny_aborts() {
    let _guard = crate::root_consent::REGISTRY_SAVE_LOCK.lock().unwrap();
    let (mut app, _) = App::new();
    app.effective_roots = vec![std::path::PathBuf::from("/srv/configured-root")];
    let target = tempfile::tempdir().unwrap();
    let canonical = target.path().canonicalize().unwrap_or_else(|_| target.path().to_path_buf());
    let before = app.project_dir.clone();

    // The real flow: the user opens the dir-picker modal, types the path
    // and confirms.
    app.show_dir_picker = true;
    let _ =
        app.update(Message::ProjectDirInputChanged(target.path().to_string_lossy().to_string()));
    let _ = app.update(Message::ProjectDirApply);

    // The switch is deferred, not applied: the gate is pending and the
    // picker stays open behind it.
    assert_eq!(app.pending_root_consent.as_deref(), Some(canonical.as_path()));
    assert_eq!(app.project_dir, before);
    assert!(app.show_dir_picker);

    let _ = app.update(Message::RootConsentDeny);
    assert!(app.pending_root_consent.is_none());
    assert_eq!(app.project_dir, before, "deny must not change the project");
}

/// Allow records the canonical dir in the effective allowlist and applies
/// the switch; a subsequent apply for the same dir passes without a gate.
#[test]
fn root_consent_allow_switches_and_records_allowlist() {
    let _guard = crate::root_consent::REGISTRY_SAVE_LOCK.lock().unwrap();
    let (mut app, _) = App::new();
    app.effective_roots = vec![std::path::PathBuf::from("/srv/configured-root")];
    let target = tempfile::tempdir().unwrap();
    let canonical = target.path().canonicalize().unwrap_or_else(|_| target.path().to_path_buf());

    let _ =
        app.update(Message::ProjectDirInputChanged(target.path().to_string_lossy().to_string()));
    let _ = app.update(Message::ProjectDirApply);
    assert!(app.pending_root_consent.is_some());

    let _ = app.update(Message::RootConsentAllow);
    assert!(app.pending_root_consent.is_none());
    assert_eq!(app.project_dir, canonical);
    assert!(app.effective_roots.contains(&canonical), "allowed dir joins the allowlist");
    assert!(!app.show_dir_picker);

    // Re-applying the same directory no longer gates (effective allowlist).
    let _ =
        app.update(Message::ProjectDirInputChanged(target.path().to_string_lossy().to_string()));
    let _ = app.update(Message::ProjectDirApply);
    assert!(app.pending_root_consent.is_none());
}

/// In-root targets never gate.
#[test]
fn in_root_project_apply_skips_consent() {
    let _guard = crate::root_consent::REGISTRY_SAVE_LOCK.lock().unwrap();
    let (mut app, _) = App::new();
    let target = tempfile::tempdir().unwrap();
    let canonical = target.path().canonicalize().unwrap_or_else(|_| target.path().to_path_buf());
    app.effective_roots = vec![canonical.clone()];

    let _ =
        app.update(Message::ProjectDirInputChanged(target.path().to_string_lossy().to_string()));
    let _ = app.update(Message::ProjectDirApply);

    assert!(app.pending_root_consent.is_none(), "in-root apply must not gate");
    assert_eq!(app.project_dir, canonical);
}

/// Re-selecting the current project is a no-op and never gates, even when
/// it lies outside the roots (nothing new is exposed).
#[test]
fn reselecting_current_project_never_gates() {
    let (mut app, _) = App::new();
    app.effective_roots = vec![std::path::PathBuf::from("/srv/configured-root")];
    let before = app.project_dir.clone();

    let _ = app.update(Message::ProjectDirInputChanged(before.to_string_lossy().to_string()));
    let _ = app.update(Message::ProjectDirApply);

    assert!(app.pending_root_consent.is_none());
    assert_eq!(app.project_dir, before);
}

// -----------------------------------------------------------------------
// Memory explorer modal (issue #110)
// -----------------------------------------------------------------------

fn sample_memory_row() -> crate::views::memory::MemoryRow {
    crate::views::memory::MemoryRow {
        id: "chunk-1".into(),
        content_preview: "a preview".into(),
        source: "src/main.rs".into(),
        age: String::new(),
        score: 0.9,
        entry_type: crate::views::memory::MemoryEntryType::Fact,
        stale: false,
    }
}

/// OpenMemoryModal opens the memory modal; CloseMemoryModal closes it.
#[test]
fn memory_modal_opens_and_closes() {
    let (mut app, _) = App::new();
    assert!(!app.memory_view_open);
    let _ = app.update(Message::OpenMemoryModal);
    assert!(app.memory_view_open);
    let _ = app.update(Message::CloseMemoryModal);
    assert!(!app.memory_view_open);
}

/// Ctrl+M (Shortcut::Memory) opens the memory modal instead of expanding
/// the retired quick-panel section.
#[test]
fn memory_shortcut_opens_the_memory_modal() {
    let (mut app, _) = App::new();
    assert!(!app.memory_view_open);
    let _ = app.update(Message::Shortcut(crate::shortcuts::Shortcut::Memory));
    assert!(app.memory_view_open);
}

/// OpenMemoryGraph opens the read-only graph modal; CloseMemoryGraph
/// closes it (ADR-69 slice 3).
#[test]
fn memory_graph_modal_opens_and_closes() {
    let (mut app, _) = App::new();
    assert!(!app.memory_graph_open);
    let _ = app.update(Message::OpenMemoryGraph);
    assert!(app.memory_graph_open);
    let _ = app.update(Message::CloseMemoryGraph);
    assert!(!app.memory_graph_open);
}

/// Esc (Shortcut::CancelDialog) dismisses the memory graph modal.
#[test]
fn escape_closes_the_memory_graph_modal() {
    let (mut app, _) = App::new();
    let _ = app.update(Message::OpenMemoryGraph);
    assert!(app.memory_graph_open);
    let _ = app.update(Message::Shortcut(crate::shortcuts::Shortcut::CancelDialog));
    assert!(!app.memory_graph_open);
}

/// MemoryGraphLoaded routes a load result into the graph view state.
#[test]
fn memory_graph_loaded_routes_into_state() {
    let (mut app, _) = App::new();
    let _ = app
        .update(Message::MemoryGraphLoaded(Ok(concerto_memory::mermaid::MemoryGraph::default())));
    assert!(matches!(app.memory_graph, crate::views::memory_graph::State::Loaded(_)));
    let _ = app.update(Message::MemoryGraphLoaded(Err("boom".into())));
    assert!(matches!(app.memory_graph, crate::views::memory_graph::State::Error(_)));
}

/// Esc (Shortcut::CancelDialog) dismisses the memory modal.
#[test]
fn escape_closes_the_memory_modal() {
    let (mut app, _) = App::new();
    let _ = app.update(Message::OpenMemoryModal);
    assert!(app.memory_view_open);
    let _ = app.update(Message::Shortcut(crate::shortcuts::Shortcut::CancelDialog));
    assert!(!app.memory_view_open);
}

/// DeleteRequested routes through the Memory state and arms the
/// ConfirmModal gate with the target recorded; DeleteCancelled clears it
/// without removing anything.
#[test]
fn memory_delete_request_arms_confirm_and_cancel_clears() {
    let (mut app, _) = App::new();
    app.memory.set_entries(vec![sample_memory_row()]);
    let _ = app.update(Message::Memory(crate::views::memory::Message::DeleteRequested(0)));
    assert!(app.memory.pending_delete.is_some());
    assert_eq!(app.memory.delete_target_id().as_deref(), Some("chunk-1"));
    let _ = app.update(Message::Memory(crate::views::memory::Message::DeleteCancelled));
    assert!(app.memory.pending_delete.is_none());
    assert_eq!(app.memory.delete_target_id(), None);
}

/// DeleteConfirmed clears the gate; the async backend invalidate resolves
/// as MemoryEntryDeleted and removes the entry from the visible list.
#[test]
fn memory_delete_confirm_removes_entry_on_success() {
    let (mut app, _) = App::new();
    app.memory.set_entries(vec![sample_memory_row()]);
    let _ = app.update(Message::Memory(crate::views::memory::Message::DeleteRequested(0)));
    let id = app.memory.delete_target_id().unwrap();
    let _ = app.update(Message::Memory(crate::views::memory::Message::DeleteConfirmed));
    assert!(app.memory.pending_delete.is_none());
    let _ = app.update(Message::MemoryEntryDeleted { id, result: Ok(()) });
    // The target is gone: the gate was cleared and the entry removed.
    assert_eq!(app.memory.delete_target_id(), None);
}

// -----------------------------------------------------------------------
// AgentMode tests (ADR-55 §7: the mode picker is gone; the intent
// gate derives the effective outcome instead of a persisted mode).
// -----------------------------------------------------------------------

#[test]
fn default_route_is_chat() {
    let (app, _) = App::new();
    assert_eq!(app.page, Page::Chat);
}

#[test]
fn help_toggle_cycles() {
    let (mut app, _) = App::new();
    assert!(!app.show_help);
    let _ = app.update(Message::HelpToggled);
    assert!(app.show_help);
    let _ = app.update(Message::HelpToggled);
    assert!(!app.show_help);
}

// ── Terminal bottom panel (toggle + drag resize) ────────────────────────

/// Opening the terminal panel flips `terminal_panel_open` and closing it
/// flips it back. The returned `ensure_started` task is dropped without
/// being run (iced only executes tasks handed to the runtime). Native
/// console initialization and execution are asynchronous, so this test
/// starts no process and stays hermetic.
#[test]
fn toggle_terminal_panel_opens_and_closes() {
    let (mut app, _) = App::new();
    assert!(!app.terminal_panel_open);
    let _ = app.update(Message::ToggleTerminalPanel);
    assert!(app.terminal_panel_open);
    let _ = app.update(Message::ToggleTerminalPanel);
    assert!(!app.terminal_panel_open);
}

/// The resize drag captures the origin at the first move and clamps the
/// resulting height to [120.0, 600.0] even for huge deltas.
#[test]
fn terminal_resize_clamps_height() {
    let (mut app, _) = App::new();
    app.terminal_panel_height = 300.0;
    let _ = app.update(Message::TerminalPanelResizeStart);
    assert!(app.terminal_resizing);
    assert_eq!(app.terminal_start_height, 300.0);

    // First move only captures the drag origin.
    let _ = app.update(Message::TerminalPanelResizeMoved(500.0));
    assert_eq!(app.terminal_drag_origin, Some(500.0));
    assert_eq!(app.terminal_panel_height, 300.0);

    // A small upward drag grows the panel (origin above cursor = taller).
    let _ = app.update(Message::TerminalPanelResizeMoved(400.0));
    assert_eq!(app.terminal_panel_height, 400.0);

    // A huge downward delta is clamped to the minimum.
    let _ = app.update(Message::TerminalPanelResizeMoved(10_000.0));
    assert_eq!(app.terminal_panel_height, 120.0);

    // A huge upward delta is clamped to the maximum.
    let _ = app.update(Message::TerminalPanelResizeMoved(-10_000.0));
    assert_eq!(app.terminal_panel_height, 600.0);

    // Release ends the drag and clears the origin.
    let _ = app.update(Message::TerminalPanelResizeEnd);
    assert!(!app.terminal_resizing);
    assert_eq!(app.terminal_drag_origin, None);
}

/// Moves and end events are no-ops while no resize is in progress.
#[test]
fn terminal_resize_ignored_when_not_resizing() {
    let (mut app, _) = App::new();
    app.terminal_panel_height = 200.0;
    let _ = app.update(Message::TerminalPanelResizeMoved(123.0));
    assert_eq!(app.terminal_panel_height, 200.0);
    let _ = app.update(Message::TerminalPanelResizeEnd);
    assert!(!app.terminal_resizing);
}

// ── Overlay fade + terminal panel slide animations ─────────────────────

/// Opening a sub-view overlay starts the fade-in (target 1.0); closing
/// back to Main starts the fade-out (target 0.0). The current alpha is
/// never reset, so re-opening mid-fade resumes from where it was.
#[test]
fn subview_open_starts_fade() {
    let (mut app, _) = App::new();
    let _ = app.update(Message::SetSubView(crate::views::chat::SubView::Diff));
    assert!(app.overlay_fading);
    assert_eq!(app.overlay_fade_target, 1.0);
    let _ = app.update(Message::SetSubView(crate::views::chat::SubView::Main));
    assert!(app.overlay_fading);
    assert_eq!(app.overlay_fade_target, 0.0);
}

/// The shared `AnimTick` advances the overlay fade and the terminal panel
/// slide toward their targets in 0.08 steps and settles (~15 ticks = a
/// full 240 ms fade/slide), clearing the in-flight flags.
#[test]
fn anim_tick_advances_and_settles() {
    let (mut app, _) = App::new();

    // Overlay fade: ramp the backdrop from transparent to fully dimmed.
    app.overlay_fading = true;
    app.overlay_fade_target = 1.0;
    app.overlay_fade = 0.0;
    for _ in 0..15 {
        let _ = app.update(Message::AnimTick);
    }
    assert!(!app.overlay_fading);
    assert!(app.overlay_fade >= 0.99);

    // Terminal panel: slide open from a fully closed position.
    app.terminal_panel_open = true;
    app.terminal_panel_animating = true;
    app.terminal_panel_anim = 0.0;
    for _ in 0..15 {
        let _ = app.update(Message::AnimTick);
    }
    assert!(!app.terminal_panel_animating);
    assert!(app.terminal_panel_anim >= 0.99);
}

#[test]
fn configured_default_route_falls_back() {
    let config = concerto_config::AppConfig::default();
    let (provider, model) = configured_default_route(&config);
    assert!(provider.is_empty() || !provider.is_empty());
    assert!(model.is_empty() || !model.is_empty());
}

#[test]
fn run_summary_includes_changed_files() {
    use camino::Utf8PathBuf;

    let output = AgentOutput {
        task_id: TaskId::new(),
        session_id: Ulid::new(),
        final_message: String::new(),
        files_modified: vec![Utf8PathBuf::from("src/main.rs")],
        tool_call_count: 1,
        eval_result: None,
        tool_events: Vec::new(),
        verification: Vec::new(),
        project_root: None,
        completion_status: concerto_core::types::AgentCompletionStatus::Completed,
        provider_metrics: Vec::new(),
        checkpoint_json: None,
    };
    let summary = super::format_run_summary(&output);
    assert!(summary.contains("src/main.rs"), "summary must list the changed file: {summary}");
    assert!(summary.contains("Files changed"), "summary must include a changed-files section");

    let mut partial = output.clone();
    partial.completion_status = concerto_core::types::AgentCompletionStatus::Partial;
    assert!(super::format_run_summary(&partial).starts_with("Partial progress preserved."));

    // When no files changed, the raw final message is used verbatim.
    let plain = AgentOutput {
        task_id: TaskId::new(),
        session_id: Ulid::new(),
        final_message: "All done".to_string(),
        files_modified: vec![],
        tool_call_count: 0,
        eval_result: None,
        tool_events: Vec::new(),
        verification: Vec::new(),
        project_root: None,
        completion_status: concerto_core::types::AgentCompletionStatus::Completed,
        provider_metrics: Vec::new(),
        checkpoint_json: None,
    };
    assert_eq!(super::format_run_summary(&plain), "All done");
}

// ── Run-stage chip (ADR-55 §9) ───────────────────────────────────

/// A `RunStageChanged` event while a run is in flight arms the chip.
#[test]
fn run_stage_event_updates_chip_while_running() {
    let (mut app, _) = App::new();
    app.run_status = RunStatus::Running;
    let _ = app.update(Message::DesktopEvent(crate::runtime::DesktopEvent::RunStageChanged {
        stage: RunStage::Execute,
    }));
    assert_eq!(app.run_stage, Some(RunStage::Execute));
}

/// A stage event outside a run (stale bus replay, cancelled run) must not
/// re-arm the chip: the App update is guarded on the run status.
#[test]
fn run_stage_event_ignored_when_not_running() {
    let (mut app, _) = App::new();
    assert_eq!(app.run_status, RunStatus::Idle);
    // A stale stage left over (e.g. mid-cancel) stays untouched: the chip
    // only re-appears after a fresh run boundary re-arms it.
    app.run_stage = Some(RunStage::Inspect);
    let _ = app.update(Message::DesktopEvent(crate::runtime::DesktopEvent::RunStageChanged {
        stage: RunStage::Execute,
    }));
    assert!(app.run_stage.is_none() || app.run_stage == Some(RunStage::Inspect));
}

/// The chip clears at the run boundary: the completion handler drops the
/// stage alongside the Idle transition (before the result match, so both
/// Ok and Err completions pass through the same clearing line).
#[test]
fn agent_run_completed_clears_run_stage() {
    let (mut app, _) = App::new();
    app.run_status = RunStatus::Running;
    app.run_stage = Some(RunStage::Execute);
    // True run boundary: status flips to Idle and the stage is dropped in
    // one line before any result handling.
    let _ = app.update(Message::AgentRunCompleted(
        None,
        Box::new(Ok(AgentOutput {
            task_id: TaskId::new(),
            session_id: Ulid::new(),
            final_message: "done".to_string(),
            files_modified: vec![],
            tool_call_count: 0,
            eval_result: None,
            tool_events: Vec::new(),
            verification: Vec::new(),
            project_root: None,
            completion_status: AgentCompletionStatus::Completed,
            provider_metrics: Vec::new(),
            checkpoint_json: None,
        })),
    ));
    assert_eq!(app.run_status, RunStatus::Idle);
    assert_eq!(app.run_stage, None);
}

// ── Dispatch-boundary validation (plan §13 Runtime selection) ──────────

fn push_provider(app: &mut App, id: &str, provider: &str, model: &str) {
    app.settings.providers.push(ProviderConfig {
        id: id.into(),
        name: provider.into(),
        provider: provider.into(),
        model: model.into(),
        cached_models: Vec::new(),
        cached_models_fetched_at: 0,
        ..ProviderConfig::default()
    });
}

/// Mirror the Settings rows into `config.model_settings.providers` so
/// `runtime_providers` resolves them. Tests push rows into
/// `settings.providers`, but on a host with a real user config
/// `App::new()` loads it and `runtime_providers` prefers the config list —
/// without this mirror the refresh handlers treat every pushed row as
/// deleted and silently drop its results.
fn sync_config_providers(app: &mut App) {
    let ms = app
        .config
        .get_or_insert_with(AppConfig::default)
        .model_settings
        .get_or_insert_with(concerto_config::ModelSettings::default);
    ms.providers = app.settings.providers.clone();
}

#[test]
fn dispatch_blocked_when_active_provider_missing_credential() {
    let (mut app, _) = App::new();
    app.settings.providers.clear();
    // OpenAI requires a credential; with none stored it is not ready and must
    // not be dispatched. (Models are now assigned per role, so an empty model
    // on the provider itself no longer blocks dispatch.)
    push_provider(&mut app, "prov1", "openai", "");
    app.active_provider_id = "prov1".into();
    assert!(
        app.dispatch_validation_error().is_some(),
        "active provider missing required credential must block dispatch"
    );
}

#[test]
fn dispatch_allowed_when_active_provider_ready() {
    // CONFIG_ENV_LOCK: `dispatch_validation_error` reads `runtime_providers`,
    // which prefers a config loaded by `App::new` — a concurrent XDG
    // redirect seeding a foreign provider list must not interleave.
    let _guard = CONFIG_ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let (mut app, _) = App::new();
    app.settings.providers.clear();
    // Ollama needs no credential, so it is ready regardless of a stored model
    // (models are chosen per agent role now).
    push_provider(&mut app, "prov1", "ollama", "");
    app.active_provider_id = "prov1".into();
    assert!(app.dispatch_validation_error().is_none(), "ready active provider must allow dispatch");
}

#[test]
fn dispatch_blocked_when_assignment_missing_model() {
    let (mut app, _) = App::new();
    app.settings.providers.clear();
    push_provider(&mut app, "prov1", "ollama", "");
    app.active_provider_id = "prov1".into();
    app.multi_agent = true;
    // Ensure assignments in config (runtime_assignments reads from config)
    let ms = app
        .config
        .get_or_insert_with(AppConfig::default)
        .model_settings
        .get_or_insert_with(concerto_config::ModelSettings::default);
    ms.agent_assignments = vec![concerto_config::AgentModelAssignment {
        agent_role: "coordinator".into(),
        provider_config_id: "prov1".into(),
        model_override: None,
    }];
    assert!(
        app.dispatch_validation_error().is_some(),
        "role assignment without a model must block dispatch"
    );
}

#[test]
fn multi_agent_validates_all_assignments() {
    let (mut app, _) = App::new();
    app.settings.providers.clear();
    push_provider(&mut app, "prov1", "ollama", "");
    app.active_provider_id = "prov1".into();
    app.multi_agent = true;
    let ms = app
        .config
        .get_or_insert_with(AppConfig::default)
        .model_settings
        .get_or_insert_with(concerto_config::ModelSettings::default);
    // Keep providers and assignments in config in sync — both
    // runtime_providers and runtime_assignments read from config.
    ms.providers = app.settings.providers.clone();
    ms.agent_assignments = vec![
        concerto_config::AgentModelAssignment {
            agent_role: "coordinator".into(),
            provider_config_id: "prov1".into(),
            model_override: Some("llama3".into()),
        },
        concerto_config::AgentModelAssignment {
            agent_role: "coder".into(),
            provider_config_id: "prov1".into(),
            model_override: None,
        },
    ];

    // The intent gate is always on (ADR-55 §7): every run is a
    // potential Execute, so an incomplete specialist assignment blocks
    // dispatch — there is no mode picker to narrow the check.
    assert!(
        app.dispatch_validation_error().is_some(),
        "multi-agent runs must validate every assignment"
    );
}

#[test]
fn save_discovery_queues_only_unfetched_ready_providers() {
    let (mut app, _) = App::new();
    app.settings.providers.clear();
    app.pending_refresh.clear();
    // `ollama` needs no credential and supports discovery, so it is ready
    // without touching the keychain (keeps this test hermetic).
    push_provider(&mut app, "fresh", "ollama", "");
    push_provider(&mut app, "populated", "ollama", "");
    app.settings.providers[1].cached_models = vec!["llama3".into()];
    app.settings.providers[1].cached_models_fetched_at = 1;
    sync_config_providers(&mut app);

    let before = app.refresh_seq;
    let _ = app.discover_unfetched_models();

    assert!(
        app.pending_refresh.contains_key("fresh"),
        "an unfetched discoverable provider must be queued on save"
    );
    assert!(
        !app.pending_refresh.contains_key("populated"),
        "a provider with a cached catalog must not be re-fetched on save"
    );
    assert!(app.refresh_seq > before, "queuing a fetch must issue a request id");
}

// ── Refresh concurrency (plan §13 Refresh concurrency) ─────────────────

#[test]
fn stale_refresh_result_is_ignored() {
    let (mut app, _) = App::new();
    app.settings.providers.clear();
    push_provider(&mut app, "prov1", "openai", "gpt-4");
    app.settings.providers[0].cached_models = vec!["gpt-4".into()];
    app.settings.providers[0].cached_models_fetched_at = 1;

    // Simulate an in-flight refresh (request id 1) without spawning a task.
    app.refresh_seq = 1;
    app.pending_refresh.insert("prov1".into(), 1);

    // A result carrying a stale request id (0) must be dropped.
    let _ = app.update(Message::Settings(SettingsMessage::ProviderModelsRefreshed {
        provider_id: "prov1".into(),
        request_id: 0,
        result: Ok(vec!["hacked-model".into()]),
    }));
    let p = app.settings.providers.iter().find(|p| p.id == "prov1").unwrap();
    assert_eq!(
        p.cached_models,
        vec!["gpt-4".to_string()],
        "stale refresh result must not mutate the cache"
    );
}

#[test]
fn current_refresh_updates_cache_and_failure_preserves_it() {
    let (mut app, _) = App::new();
    app.settings.providers.clear();
    push_provider(&mut app, "prov1", "openai", "gpt-4");
    sync_config_providers(&mut app);
    app.settings.providers[0].cached_models = vec!["gpt-4".into()];
    app.settings.providers[0].cached_models_fetched_at = 1;

    // First refresh (request id 1) succeeds.
    app.refresh_seq = 1;
    app.pending_refresh.insert("prov1".into(), 1);
    let _ = app.update(Message::Settings(SettingsMessage::ProviderModelsRefreshed {
        provider_id: "prov1".into(),
        request_id: 1,
        result: Ok(vec!["gpt-4".into(), "gpt-4o".into()]),
    }));
    let p = app.settings.providers.iter().find(|p| p.id == "prov1").unwrap();
    assert!(
        p.cached_models.contains(&"gpt-4o".to_string()),
        "successful refresh must update the cache"
    );

    // Second refresh (request id 2) fails.
    app.refresh_seq = 2;
    app.pending_refresh.insert("prov1".into(), 2);
    let _ = app.update(Message::Settings(SettingsMessage::ProviderModelsRefreshed {
        provider_id: "prov1".into(),
        request_id: 2,
        result: Err("network down".into()),
    }));
    let p = app.settings.providers.iter().find(|p| p.id == "prov1").unwrap();
    assert!(
        p.cached_models.contains(&"gpt-4o".to_string()),
        "failed refresh must preserve the previous cache"
    );
}

#[test]
fn refresh_result_for_deleted_provider_is_ignored() {
    let (mut app, _) = App::new();
    app.settings.providers.clear();
    push_provider(&mut app, "provA", "openai", "gpt-4");
    push_provider(&mut app, "provB", "openai", "gpt-4");

    // Delete provA while the refresh is "in flight".
    app.settings.providers.retain(|p| p.id != "provA");
    // Mirror AFTER the deletion so runtime_providers agrees provA is gone
    // (a host's real user config would otherwise keep it resolvable).
    sync_config_providers(&mut app);
    app.refresh_seq = 1;
    app.pending_refresh.insert("provA".into(), 1);

    // The result for the deleted provider must be ignored, and provB must
    // be untouched.
    let _ = app.update(Message::Settings(SettingsMessage::ProviderModelsRefreshed {
        provider_id: "provA".into(),
        request_id: 1,
        result: Ok(vec!["hacked".into()]),
    }));
    assert!(!app.settings.providers.iter().any(|p| p.id == "provA"), "provA must remain deleted");
    let b = app.settings.providers.iter().find(|p| p.id == "provB").unwrap();
    assert_eq!(
        b.cached_models,
        Vec::<String>::new(),
        "other providers must not be affected by a deleted provider's result"
    );
}

#[test]
fn discovered_models_populate_picker_and_provider_options() {
    let (mut app, _) = App::new();
    app.settings.providers.clear();
    push_provider(&mut app, "prov1", "ollama", "llama3");
    sync_config_providers(&mut app);
    app.active_provider_id = "prov1".into();
    app.refresh_seq = 1;
    app.pending_refresh.insert("prov1".into(), 1);

    // Simulate a discovery result flowing through the full update path.
    let _ = app.update(Message::Settings(SettingsMessage::ProviderModelsRefreshed {
        provider_id: "prov1".into(),
        request_id: 1,
        result: Ok(vec!["discovered-model".into(), "llama3".into()]),
    }));
    let cache = app.settings.cached_models_by_provider();
    assert!(
        cache
            .get("prov1")
            .map(|models| models.iter().any(|m| m == "discovered-model"))
            .unwrap_or(false),
        "discovered model must appear in the per-provider model cache"
    );
}

#[test]
fn manual_refresh_request_registers_tracked_in_flight_fetch() {
    let (mut app, _) = App::new();
    app.settings.providers.clear();
    push_provider(&mut app, "prov1", "openai", "");
    sync_config_providers(&mut app);

    // Startup auto-discovery may already have consumed request ids.
    let seq_before = app.refresh_seq;
    let _ = app
        .update(Message::Settings(SettingsMessage::ProviderModelsRefreshRequested("prov1".into())));

    assert_eq!(
        app.pending_refresh.get("prov1"),
        Some(&seq_before.wrapping_add(1)),
        "manual refresh must register a tracked request id"
    );
    assert!(
        app.settings.refreshing_providers.contains("prov1"),
        "the provider row must report its refresh as in flight"
    );
}

#[test]
fn manual_refresh_request_for_unknown_or_nondiscovering_provider_is_ignored() {
    let (mut app, _) = App::new();
    // Startup auto-discovery may legitimately hold tracked requests; only
    // the unknown id must stay untouched.
    let pending_before = app.pending_refresh.len();

    let _ = app
        .update(Message::Settings(SettingsMessage::ProviderModelsRefreshRequested("ghost".into())));

    assert_eq!(
        app.pending_refresh.len(),
        pending_before,
        "unknown providers must not spawn fetches"
    );
    assert!(!app.pending_refresh.contains_key("ghost"));
    assert!(app.settings.refreshing_providers.is_empty());
}

#[test]
fn completed_manual_refresh_updates_cache_and_clears_in_flight_state() {
    let (mut app, _) = App::new();
    app.settings.providers.clear();
    push_provider(&mut app, "prov1", "openai", "");
    sync_config_providers(&mut app);

    let _ = app
        .update(Message::Settings(SettingsMessage::ProviderModelsRefreshRequested("prov1".into())));
    let request_id = *app.pending_refresh.get("prov1").expect("tracked request");

    // The spawned task is dropped in tests; simulate its completion with
    // the request id the handler assigned.
    let _ = app.update(Message::Settings(SettingsMessage::ProviderModelsRefreshed {
        provider_id: "prov1".into(),
        request_id,
        result: Ok(vec!["ox-alpha".into(), "gpt-4o".into()]),
    }));

    assert!(
        !app.settings.refreshing_providers.contains("prov1"),
        "the in-flight marker must clear when the result arrives"
    );
    assert!(!app.pending_refresh.contains_key("prov1"));
    assert!(!app.settings.provider_refresh_errors.contains_key("prov1"));
    let cache = app.settings.cached_models_by_provider();
    assert!(
        cache.get("prov1").map(|m| m.iter().any(|n| n == "ox-alpha")).unwrap_or(false),
        "newly released models must appear in the picker cache immediately"
    );
}

#[test]
fn failed_manual_refresh_preserves_cache_and_reports_inline_error() {
    let (mut app, _) = App::new();
    app.settings.providers.clear();
    push_provider(&mut app, "prov1", "openai", "");
    sync_config_providers(&mut app);
    app.settings.providers[0].cached_models = vec!["gpt-4".into()];
    app.settings.providers[0].cached_models_fetched_at = 1;

    // An explicit Err (network outage) must preserve the cache, clear the
    // in-flight marker, and surface the error inline.
    app.refresh_seq = 1;
    app.pending_refresh.insert("prov1".into(), 1);
    app.settings.begin_provider_refresh("prov1");
    let _ = app.update(Message::Settings(SettingsMessage::ProviderModelsRefreshed {
        provider_id: "prov1".into(),
        request_id: 1,
        result: Err("connection refused".into()),
    }));
    let p = app.settings.providers.iter().find(|p| p.id == "prov1").unwrap();
    assert!(
        p.cached_models.contains(&"gpt-4".to_string()),
        "failed refresh must preserve the previous cache"
    );
    assert!(!app.settings.refreshing_providers.contains("prov1"));
    assert_eq!(
        app.settings.provider_refresh_errors.get("prov1").map(String::as_str),
        Some("connection refused")
    );

    // A later empty discovery result — the shape every providers-crate
    // failure collapses to before this handler — must likewise never wipe
    // the cached list.
    app.refresh_seq = 2;
    app.pending_refresh.insert("prov1".into(), 2);
    app.settings.begin_provider_refresh("prov1");
    let _ = app.update(Message::Settings(SettingsMessage::ProviderModelsRefreshed {
        provider_id: "prov1".into(),
        request_id: 2,
        result: Ok(Vec::new()),
    }));
    let p = app.settings.providers.iter().find(|p| p.id == "prov1").unwrap();
    assert!(
        p.cached_models.contains(&"gpt-4".to_string()),
        "an empty discovery result must not wipe the cached model list"
    );
    assert!(
        app.settings.provider_refresh_errors.contains_key("prov1"),
        "an empty discovery result must be surfaced as a failure"
    );
}

/// The App layer records discovery into the *config-side* cache as well as
/// the Settings rows. An empty refresh must not clobber that good catalog
/// or advance its fetch time, and the user must be told inline that the
/// previous list is being kept rather than seeing a fresh, empty discovery.
#[test]
fn empty_refresh_keeps_config_side_cache_and_reports_it() {
    let (mut app, _) = App::new();
    app.settings.providers.clear();
    push_provider(&mut app, "prov1", "openai", "");
    sync_config_providers(&mut app);

    const SEED_FETCHED_AT: i64 = 1_700_000_000;
    let seeded = vec!["gpt-4o".to_string(), "gpt-4o-mini".to_string()];
    {
        let ms = app
            .config
            .as_mut()
            .expect("config present")
            .model_settings
            .as_mut()
            .expect("model settings present");
        let p = ms.providers.iter_mut().find(|p| p.id == "prov1").expect("config provider");
        p.cached_models = seeded.clone();
        p.cached_models_fetched_at = SEED_FETCHED_AT;
    }
    app.settings.providers[0].cached_models = seeded.clone();
    app.settings.providers[0].cached_models_fetched_at = SEED_FETCHED_AT;

    app.refresh_seq = 1;
    app.pending_refresh.insert("prov1".into(), 1);
    app.settings.begin_provider_refresh("prov1");
    let _ = app.update(Message::Settings(SettingsMessage::ProviderModelsRefreshed {
        provider_id: "prov1".into(),
        request_id: 1,
        result: Ok(Vec::new()),
    }));

    let p = app
        .config
        .as_ref()
        .expect("config present")
        .model_settings
        .as_ref()
        .expect("model settings present")
        .providers
        .iter()
        .find(|p| p.id == "prov1")
        .expect("config provider");
    assert_eq!(
        p.cached_models, seeded,
        "an empty refresh must not clobber the config-side cached model list"
    );
    assert_eq!(
        p.cached_models_fetched_at, SEED_FETCHED_AT,
        "an ignored empty refresh must not advance the config-side fetch time"
    );
    assert_eq!(
        app.settings.provider_refresh_errors.get("prov1").map(String::as_str),
        Some(crate::views::settings::EMPTY_DISCOVERY_KEPT),
        "the user must be told the refresh produced nothing and the previous list was kept"
    );
}

// ── Shared selector consistency (plan §13 Runtime selection) ────────────

#[test]
fn chat_model_options_match_shared_resolver() {
    // Hold CONFIG_ENV_LOCK and redirect XDG like the other config-sensitive
    // tests: `App::new` loads the user config, and a concurrent
    // XDG_CONFIG_HOME redirect (e.g. the agent-override test) would make
    // `runtime_providers` resolve a foreign provider list instead of the
    // `settings.providers` rows pushed below.
    let _guard = CONFIG_ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let dir = tempfile::tempdir().expect("tempdir");
    let previous = std::env::var_os("XDG_CONFIG_HOME");
    std::env::set_var("XDG_CONFIG_HOME", dir.path());
    let (mut app, _) = App::new();
    app.settings.providers.clear();
    push_provider(&mut app, "prov1", "openai", "gpt-4");
    app.settings.providers[0].cached_models = vec!["gpt-4o".into()];
    app.active_provider_id = "prov1".into();
    app.active_model = "gpt-4".into();
    app.sync_chat_model_options();
    match previous {
        Some(value) => std::env::set_var("XDG_CONFIG_HOME", value),
        None => std::env::remove_var("XDG_CONFIG_HOME"),
    }
    assert!(
        app.chat_model_options.contains(&"gpt-4".to_string()),
        "the active/selected model must be present in the chat picker"
    );
    assert!(
        app.chat_model_options.contains(&"gpt-4o".to_string()),
        "discovered models must be present in the chat picker"
    );
}

#[test]
fn configured_route_is_resolved_from_model_not_deprecated_provider_id() {
    let config = AppConfig {
        model_settings: Some(concerto_config::ModelSettings {
            providers: vec![
                concerto_config::ProviderConfig {
                    id: "first".into(),
                    provider: "openai".into(),
                    model: "gpt-4o".into(),
                    ..Default::default()
                },
                concerto_config::ProviderConfig {
                    id: "second".into(),
                    provider: "anthropic".into(),
                    model: "claude-sonnet-4".into(),
                    ..Default::default()
                },
            ],
            global_default_model: Some("claude-sonnet-4".into()),
            global_default_id: Some("first".into()),
            ..Default::default()
        }),
        ..Default::default()
    };
    assert_eq!(
        configured_default_route(&config),
        ("second".to_string(), "claude-sonnet-4".to_string())
    );
}

// ── Generation-safe save feedback (plan §9.2 / §13 UI feedback) ────────

#[test]
fn stale_save_feedback_timer_cannot_clear_newer_save() {
    let (mut app, _) = App::new();
    // Simulate the first save scheduling a clear for generation 1.
    app.save_feedback = Some("Saved".into());
    app.save_feedback_generation = 1;

    // A newer save increments the generation and shows fresh feedback.
    app.save_feedback_generation = 2;
    app.save_feedback = Some("Saved again".into());

    // The stale timer from the first save (generation 1) arrives.
    let _ = app.update(Message::ClearSaveFeedback(1));
    assert_eq!(
        app.save_feedback.as_deref(),
        Some("Saved again"),
        "stale timer must not erase the newer save's feedback"
    );

    // The current timer (generation 2) clears it.
    let _ = app.update(Message::ClearSaveFeedback(2));
    assert!(app.save_feedback.is_none(), "current timer must clear feedback");
}

/// Navigation to Studio page works.
#[test]
fn navigate_to_studio_changes_page() {
    // Redirect XDG before App::new: navigating to the Studio auto-seeds
    // the GLOBAL orchestration config now (global-only seeding), so this
    // test must never touch the machine's real config file.
    let _guard = CONFIG_ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let dir = tempfile::tempdir().expect("tempdir");
    let previous = std::env::var_os("XDG_CONFIG_HOME");
    std::env::set_var("XDG_CONFIG_HOME", dir.path());
    let (mut app, _) = App::new();
    let _ = app.update(Message::Navigate(Page::OrchestrationStudio));
    match previous {
        Some(value) => std::env::set_var("XDG_CONFIG_HOME", value),
        None => std::env::remove_var("XDG_CONFIG_HOME"),
    }
    assert_eq!(app.page, Page::OrchestrationStudio);
}

/// Ctrl+Shift+S (the always-screenshot chord) reaches the capture path on
/// the Editor page — the one page where Ctrl+S means Save, so it is the
/// chord that keeps screenshots reachable there.
#[test]
fn screenshot_always_reaches_capture_on_the_editor_page() {
    let (mut app, _) = App::new();
    app.page = Page::Editor;
    assert!(app.screenshot_status.is_none());
    let _ = app.update(Message::Shortcut(crate::shortcuts::Shortcut::ScreenshotAlways));
    assert_eq!(
        app.screenshot_status.as_deref(),
        Some("Capturing..."),
        "Ctrl+Shift+S must capture even on the Editor page"
    );
}

/// Screenshot status can be stored and cleared.
#[test]
fn screenshot_status_stored_and_cleared() {
    let (mut app, _) = App::new();
    assert!(app.screenshot_status.is_none());
    app.screenshot_status = Some("captured".into());
    assert_eq!(app.screenshot_status.as_deref(), Some("captured"));
    app.screenshot_status = None;
    assert!(app.screenshot_status.is_none());
}

/// Save feedback can be stored and cleared.
#[test]
fn save_feedback_stored_and_cleared() {
    let (mut app, _) = App::new();
    assert!(app.save_feedback.is_none());
    app.save_feedback = Some("saved".into());
    assert_eq!(app.save_feedback.as_deref(), Some("saved"));
    app.save_feedback = None;
    assert!(app.save_feedback.is_none());
}

// -----------------------------------------------------------------------
// Durable typed transcript restore (ADR-36, stage 3)
// -----------------------------------------------------------------------

/// Every `TranscriptEntry` variant maps to the expected `ChatEntry` with
/// sequential ids, correct status mapping, thinking label formats and
/// collapse flags.
#[test]
fn transcript_to_entries_maps_all_variants() {
    use crate::views::chat::{ChatEntry, RunCompletionSummary, ToolCallStatus};
    use concerto_core::transcript::{TranscriptEntry, TranscriptToolStatus};

    let transcript = vec![
        TranscriptEntry::User { content: "build the widget".into() },
        TranscriptEntry::Assistant { content: "on it".into() },
        TranscriptEntry::Thinking {
            agent: "coder".into(),
            content: "step one".into(),
            kind: ThinkingKind::Headline,
        },
        TranscriptEntry::Thinking {
            agent: String::new(),
            content: "bare thought".into(),
            kind: ThinkingKind::Detail,
        },
        TranscriptEntry::ToolCall {
            tool_name: "fs_write".into(),
            detail: "write main.rs".into(),
            status: TranscriptToolStatus::Completed,
        },
        TranscriptEntry::ToolCall {
            tool_name: "shell".into(),
            detail: String::new(),
            status: TranscriptToolStatus::Allowed,
        },
        TranscriptEntry::ToolCall {
            tool_name: "git".into(),
            detail: String::new(),
            status: TranscriptToolStatus::Denied,
        },
        TranscriptEntry::ToolCall {
            tool_name: "net".into(),
            detail: String::new(),
            status: TranscriptToolStatus::Cancelled,
        },
        TranscriptEntry::ToolCall {
            tool_name: "probe".into(),
            detail: String::new(),
            status: TranscriptToolStatus::Failed,
        },
        TranscriptEntry::ToolCall {
            tool_name: "live".into(),
            detail: String::new(),
            status: TranscriptToolStatus::Running,
        },
        TranscriptEntry::Activity {
            agent: "Coordinator".into(),
            content: "Delegated subtask T1 to coder".into(),
        },
        TranscriptEntry::Error { content: "boom".into() },
        TranscriptEntry::Summary { content: "context compacted".into() },
        TranscriptEntry::Completion {
            multi_agent: true,
            completed: true,
            files: vec!["main.rs".into()],
            project_root: Some("/proj".into()),
        },
    ];

    let entries = super::transcript_to_entries(transcript);
    let expected = vec![
        ChatEntry::User { id: 1, content: "build the widget".into(), created_at: None },
        ChatEntry::Assistant { id: 2, content: "on it".into(), streaming: false, created_at: None },
        ChatEntry::Thinking {
            id: 3,
            agent: "coder".into(),
            content: "step one".into(),
            kind: ThinkingKind::Headline,
            collapsed: false,
            created_at: None,
            finished_at: None,
        },
        ChatEntry::Thinking {
            id: 4,
            agent: String::new(),
            content: "bare thought".into(),
            kind: ThinkingKind::Detail,
            collapsed: false,
            created_at: None,
            finished_at: None,
        },
        ChatEntry::ToolCall {
            id: 5,
            tool_name: "fs_write".into(),
            detail: "write main.rs".into(),
            status: ToolCallStatus::Completed,
            created_at: None,
        },
        ChatEntry::ToolCall {
            id: 6,
            tool_name: "shell".into(),
            detail: String::new(),
            status: ToolCallStatus::Allowed,
            created_at: None,
        },
        ChatEntry::ToolCall {
            id: 7,
            tool_name: "git".into(),
            detail: String::new(),
            status: ToolCallStatus::Denied,
            created_at: None,
        },
        ChatEntry::ToolCall {
            id: 8,
            tool_name: "net".into(),
            detail: String::new(),
            status: ToolCallStatus::Cancelled,
            created_at: None,
        },
        ChatEntry::ToolCall {
            id: 9,
            tool_name: "probe".into(),
            detail: String::new(),
            status: ToolCallStatus::Failed,
            created_at: None,
        },
        ChatEntry::ToolCall {
            id: 10,
            tool_name: "live".into(),
            detail: String::new(),
            status: ToolCallStatus::Running,
            created_at: None,
        },
        ChatEntry::Thinking {
            id: 11,
            agent: "coordinator".into(),
            content: "Delegated subtask T1 to coder".into(),
            kind: ThinkingKind::Detail,
            collapsed: false,
            created_at: None,
            finished_at: None,
        },
        ChatEntry::Error { id: 12, content: "boom".into(), created_at: None },
        ChatEntry::Thinking {
            id: 13,
            agent: "context".into(),
            content: "context compacted".into(),
            kind: ThinkingKind::Detail,
            collapsed: true,
            created_at: None,
            finished_at: None,
        },
        ChatEntry::Completion {
            id: 14,
            summary: RunCompletionSummary {
                multi_agent: true,
                completed: true,
                files: vec!["main.rs".into()],
                project_root: Some("/proj".into()),
            },
            created_at: None,
        },
    ];

    assert_eq!(serde_json::to_value(&entries).unwrap(), serde_json::to_value(&expected).unwrap(),);
}

/// The ADR-36 proof: restored transcript entries equal the live-built
/// entries for the same scripted event sequence (user, thinking, correlated
/// tool lifecycle, assistant, error). The tool-call entry mirrors the
/// recorder's merge (start detail + terminal detail).
#[test]
fn restored_transcript_matches_live_rendering() {
    use crate::views::chat::State;
    use concerto_core::transcript::{TranscriptEntry, TranscriptToolStatus};

    let transcript = vec![
        TranscriptEntry::User { content: "build the widget".into() },
        TranscriptEntry::Thinking {
            agent: "coder".into(),
            content: "step one".into(),
            kind: ThinkingKind::Detail,
        },
        TranscriptEntry::ToolCall {
            tool_name: "fs_write".into(),
            detail: "write main.rs\nWrote 42 bytes".into(),
            status: TranscriptToolStatus::Completed,
        },
        TranscriptEntry::Assistant { content: "the fix is in".into() },
        TranscriptEntry::Error { content: "boom".into() },
    ];
    let restored = super::transcript_to_entries(transcript);

    // Live rendering for the same scripted events (mirrors runtime.rs
    // route_event + the run-end finalize_run).
    let mut live = State::new();
    let _ = live.update(crate::views::chat::Message::AddUser("build the widget".into()));
    live.add_thinking("coder", "step one".into(), ThinkingKind::Detail);
    live.add_tool_call("fs_write".into(), "write main.rs".into());
    live.update_tool_call("fs_write", "Wrote 42 bytes".into(), true);
    live.update_last_assistant("the fix is in".into());
    live.finalize_run();
    live.add_error("boom".into());

    assert_eq!(
        without_timestamps(serde_json::to_value(&restored).unwrap()),
        without_timestamps(serde_json::to_value(live.entries()).unwrap()),
        "restored transcript entries must equal live-built entries \
             for the same scripted sequence"
    );

    // Restoring through State::from_entries must preserve the same entries
    // (no Running tool calls remain to settle).
    let restored_state = State::from_entries(restored);
    assert_eq!(
        without_timestamps(serde_json::to_value(restored_state.entries()).unwrap()),
        without_timestamps(serde_json::to_value(live.entries()).unwrap()),
    );
}

/// Drop the `created_at`/`finished_at` keys from serialized entries so
/// restored (timestamp-less) transcripts can be compared with live-built
/// entries that carry real timestamps.
fn without_timestamps(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => serde_json::Value::Object(
            map.into_iter()
                .filter(|(key, _)| key != "created_at" && key != "finished_at")
                .map(|(key, value)| (key, without_timestamps(value)))
                .collect(),
        ),
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.into_iter().map(without_timestamps).collect())
        }
        other => other,
    }
}

/// Documented divergences between restored and live rendering: approval
/// representation, SubTaskCreated wording, summary collapse state and the
/// structured completion card. The restored (canonical) forms are asserted
/// here alongside the live forms they intentionally differ from.
#[test]
fn restored_approval_activity_and_summary_renderings_are_documented() {
    use crate::views::chat::{ChatEntry, State, ToolCallStatus};
    use concerto_core::transcript::{TranscriptEntry, TranscriptToolStatus};

    let transcript = vec![
        TranscriptEntry::User { content: "deploy".into() },
        // Approval outcome is persisted as the canonical status on the tool call.
        TranscriptEntry::ToolCall {
            tool_name: "shell".into(),
            detail: String::new(),
            status: TranscriptToolStatus::Allowed,
        },
        TranscriptEntry::Activity {
            agent: "Coordinator".into(),
            content: "Decomposed task T1 into specialist subtask: apply the fix".into(),
        },
        TranscriptEntry::Summary { content: "context compacted".into() },
        TranscriptEntry::Completion {
            multi_agent: true,
            completed: false,
            files: vec!["main.rs".into()],
            project_root: Some("/proj".into()),
        },
    ];
    let restored = super::transcript_to_entries(transcript);

    // Restored approval: canonical tool name + Allowed status (1:1 mapping).
    assert!(matches!(
        &restored[1],
        ChatEntry::ToolCall { tool_name, status: ToolCallStatus::Allowed, .. }
            if tool_name == "shell"
    ));
    // Live approval rendering diverges: it pushes a separate Running entry
    // with the outcome appended to the name ("shell (allowed)").
    let mut live = State::new();
    live.add_tool_call("shell (allowed)".into(), String::new());
    assert!(matches!(
        live.entries().last(),
        Some(ChatEntry::ToolCall { tool_name, status: ToolCallStatus::Running, .. })
            if tool_name == "shell (allowed)"
    ));

    // Restored activity: typed thinking line in the agent's bucket. The
    // live SubTaskCreated line uses different wording ("→ coder: apply
    // the fix") — a documented string divergence; both render as
    // thinking lines.
    assert!(matches!(
        &restored[2],
        ChatEntry::Thinking { agent, content, collapsed: false, .. }
            if agent == "coordinator"
                && content == "Decomposed task T1 into specialist subtask: apply the fix"
    ));

    // Restored summary: collapsed thinking line in the Context bucket.
    assert!(matches!(
        &restored[3],
        ChatEntry::Thinking { agent, content, collapsed: true, .. }
            if agent == "context" && content == "context compacted"
    ));

    // Restored completion: structured RunCompletionSummary.
    match &restored[4] {
        ChatEntry::Completion { summary, .. } => {
            assert!(summary.multi_agent);
            assert!(!summary.completed);
            assert_eq!(summary.files, vec!["main.rs".to_string()]);
            assert_eq!(summary.project_root.as_deref(), Some("/proj"));
        }
        other => panic!("expected Completion entry, got {other:?}"),
    }
}

// -----------------------------------------------------------------------
// DesktopApprovalSink — frontend parity with the CLI approval sink
// -----------------------------------------------------------------------
//
// The desktop sink must behave exactly like `CliApprovalSink`: per-call
// prompting unless the user explicitly opts into session-wide
// auto-approval. In particular a plain "Grant for this session" decision
// must NOT auto-approve later calls of the same tool (the old name-based
// `session_grants` cache is gone).

/// Minimal policy action for approval-sink tests.
fn make_action<'a>(tool_name: &'a str, input: &'a serde_json::Value) -> PolicyAction<'a> {
    PolicyAction {
        tool_name,
        input,
        session_id: Ulid::new(),
        correlation_id: Ulid::new(),
        capability_requirements: concerto_core::types::CapabilitySet::default(),
        sandbox_profile: None,
        estimated_cost_usd: None,
        command_facts: None,
        orchestrator_authority: false,
        path_facts: None,
    }
}

/// Wait until the sink's request future has queued a dialog on
/// `cap_pending`, so the test resolves it without racing the spawn.
/// Bounded so a broken sink fails the test instead of hanging forever.
async fn wait_for_pending_dialog(shared: &crate::widgets::capability_dialog::SharedPending) {
    for _ in 0..500 {
        if !shared.lock().unwrap_or_else(|e| e.into_inner()).is_empty() {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("approval request never queued a dialog");
}

#[tokio::test]
async fn approval_sink_auto_approve_returns_approve_without_dialog() {
    let cap_pending = crate::widgets::capability_dialog::shared_pending();
    let pending_ack = crate::widgets::capability_dialog::shared_pending_ack();
    let sink = DesktopApprovalSink {
        cap_pending: cap_pending.clone(),
        pending_ack: pending_ack.clone(),
        pending_intent: crate::widgets::capability_dialog::shared_pending_intent(),
        pending_plan: crate::widgets::capability_dialog::shared_pending_plan(),
        auto_approve: Arc::new(AtomicBool::new(true)),
        bus: EventBus::default(),
    };
    let input = serde_json::json!({ "path": "/tmp/example.rs" });
    let action = make_action("write_file", &input);
    let cancel = CancellationToken::new();
    let decision = sink.request_approval(&action, cancel).await;
    assert_eq!(decision, ApprovalDecision::Approve);
    // Fast path short-circuits before pushing a dialog.
    assert!(
        cap_pending.lock().unwrap_or_else(|e| e.into_inner()).is_empty(),
        "auto-approve must not queue a dialog"
    );
}

#[tokio::test]
async fn approval_sink_approve_all_for_session_enables_auto_approve() {
    let cap_pending = crate::widgets::capability_dialog::shared_pending();
    let pending_ack = crate::widgets::capability_dialog::shared_pending_ack();
    let sink = DesktopApprovalSink {
        cap_pending: cap_pending.clone(),
        pending_ack: pending_ack.clone(),
        pending_intent: crate::widgets::capability_dialog::shared_pending_intent(),
        pending_plan: crate::widgets::capability_dialog::shared_pending_plan(),
        auto_approve: Arc::new(AtomicBool::new(false)),
        bus: EventBus::default(),
    };
    let cancel = CancellationToken::new();
    assert!(!sink.auto_approve.load(Ordering::Relaxed));
    sink.approve_all_for_session(Ulid::new(), cancel.clone()).await;
    assert!(sink.auto_approve.load(Ordering::Relaxed));

    // Subsequent request is approved without a dialog.
    let input = serde_json::json!({ "path": "/tmp/example.rs" });
    let action = make_action("write_file", &input);
    let decision = sink.request_approval(&action, cancel).await;
    assert_eq!(decision, ApprovalDecision::Approve);
    assert!(
        cap_pending.lock().unwrap_or_else(|e| e.into_inner()).is_empty(),
        "auto-approve must not queue a dialog"
    );
}

#[tokio::test]
async fn approval_sink_request_ack_auto_approves_when_flag_set() {
    let cap_pending = crate::widgets::capability_dialog::shared_pending();
    let pending_ack = crate::widgets::capability_dialog::shared_pending_ack();
    let sink = DesktopApprovalSink {
        cap_pending: cap_pending.clone(),
        pending_ack: pending_ack.clone(),
        pending_intent: crate::widgets::capability_dialog::shared_pending_intent(),
        pending_plan: crate::widgets::capability_dialog::shared_pending_plan(),
        auto_approve: Arc::new(AtomicBool::new(true)),
        bus: EventBus::default(),
    };
    let cancel = CancellationToken::new();
    let ack = sink.request_ack(Ulid::new(), "some warning", cancel).await;
    assert!(ack, "request_ack must return true when auto-approve is on");
}

/// Identical pending approval requests (same tool + input) coalesce onto
/// ONE dialog instead of stacking duplicates; both waiters receive the
/// same decision. The production failure mode (identical retries after a
/// timeout) no longer queues a duplicate dialog.
#[tokio::test]
async fn approval_sink_identical_requests_coalesce_onto_one_dialog() {
    let cap_pending = crate::widgets::capability_dialog::shared_pending();
    let pending_ack = crate::widgets::capability_dialog::shared_pending_ack();
    let sink = DesktopApprovalSink {
        cap_pending: cap_pending.clone(),
        pending_ack: pending_ack.clone(),
        pending_intent: crate::widgets::capability_dialog::shared_pending_intent(),
        pending_plan: crate::widgets::capability_dialog::shared_pending_plan(),
        auto_approve: Arc::new(AtomicBool::new(false)),
        bus: EventBus::default(),
    };
    let cancel = CancellationToken::new();
    let input = serde_json::json!({ "path": "/tmp/example.rs" });

    let first = tokio::spawn({
        let sink = sink.clone();
        let cancel = cancel.clone();
        let input = input.clone();
        async move {
            let action = make_action("write_file", &input);
            sink.request_approval(&action, cancel).await
        }
    });
    wait_for_pending_dialog(&cap_pending).await;

    // A second identical request coalesces: it must NOT queue a second
    // dialog and must await the SAME decision.
    let second = tokio::spawn({
        let sink = sink.clone();
        let cancel = cancel.clone();
        let input = input.clone();
        async move {
            let action = make_action("write_file", &input);
            sink.request_approval(&action, cancel).await
        }
    });
    // Give the second task a chance to run and (wrongly) push a duplicate.
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
    assert_eq!(
        cap_pending.lock().unwrap_or_else(|e| e.into_inner()).len(),
        1,
        "identical requests must coalesce onto ONE pending dialog"
    );

    // Resolving the single dialog delivers the same decision to both.
    assert!(crate::widgets::capability_dialog::resolve(
        &cap_pending,
        &crate::widgets::capability_dialog::Message::GrantSession
    ));
    assert_eq!(first.await.expect("first task panicked"), ApprovalDecision::Approve);
    assert_eq!(second.await.expect("second task panicked"), ApprovalDecision::Approve);
}

/// Wait until the spawn sink future has installed the pending ack so the
/// test resolves it without racing the spawn. Bounded so a broken sink
/// fails the test instead of hanging forever.
async fn wait_for_pending_ack(shared: &crate::widgets::capability_dialog::SharedPendingAck) {
    for _ in 0..500 {
        if !shared.lock().unwrap_or_else(|e| e.into_inner()).is_empty() {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("ack request never queued a dialog");
}

#[tokio::test]
async fn approval_sink_request_ack_forwards_session_id() {
    let cap_pending = crate::widgets::capability_dialog::shared_pending();
    let pending_ack = crate::widgets::capability_dialog::shared_pending_ack();
    let sink = DesktopApprovalSink {
        cap_pending: cap_pending.clone(),
        pending_ack: pending_ack.clone(),
        pending_intent: crate::widgets::capability_dialog::shared_pending_intent(),
        pending_plan: crate::widgets::capability_dialog::shared_pending_plan(),
        auto_approve: Arc::new(AtomicBool::new(false)),
        bus: EventBus::default(),
    };
    let session_id = Ulid::new();
    let cancel = CancellationToken::new();

    let sink2 = sink.clone();
    let handle =
        tokio::spawn(async move { sink2.request_ack(session_id, "some warning", cancel).await });
    wait_for_pending_ack(&pending_ack).await;

    // The pending ack carries the requesting session id so resolution can
    // confirm membership (ADR-68, audit H-04).
    let pending_session = {
        let guard = pending_ack.lock().unwrap_or_else(|e| e.into_inner());
        guard.front().expect("ack must be pending").session_id
    };
    assert_eq!(pending_session, session_id, "pending ack must carry the session id");

    assert!(
        crate::widgets::capability_dialog::resolve_ack(&pending_ack, session_id, true),
        "matching-session resolve must succeed"
    );
    assert!(handle.await.expect("ack task panicked"), "an approved ack continues");
    assert!(
        pending_ack.lock().unwrap_or_else(|e| e.into_inner()).is_empty(),
        "after resolve no ack should be pending"
    );
}

/// Shown / queued / refused, end to end through the sink (ADR-68 §6,
/// DEFERRED row 4): the first ack is shown, a second is queued behind it,
/// and a third — beyond the bounded depth — is explicitly refused
/// (fail-closed, `false`) instead of being dropped or displacing an entry.
#[tokio::test]
async fn approval_sink_request_ack_queues_then_refuses_overflow() {
    let cap_pending = crate::widgets::capability_dialog::shared_pending();
    let pending_ack = crate::widgets::capability_dialog::shared_pending_ack();
    let bus = EventBus::default();
    let mut events = bus.subscribe();
    let sink = DesktopApprovalSink {
        cap_pending: cap_pending.clone(),
        pending_ack: pending_ack.clone(),
        pending_intent: crate::widgets::capability_dialog::shared_pending_intent(),
        pending_plan: crate::widgets::capability_dialog::shared_pending_plan(),
        auto_approve: Arc::new(AtomicBool::new(false)),
        bus,
    };
    let cancel = CancellationToken::new();

    // Shown: the first request occupies the active slot.
    let first_session = Ulid::new();
    let sink_first = sink.clone();
    let cancel_first = cancel.clone();
    let first = tokio::spawn(async move {
        sink_first.request_ack(first_session, "first warning", cancel_first).await
    });
    wait_for_pending_ack(&pending_ack).await;

    // Queued: a second request (different session) waits behind the first
    // rather than overwriting it.
    let second_session = Ulid::new();
    let sink_second = sink.clone();
    let cancel_second = cancel.clone();
    let second = tokio::spawn(async move {
        sink_second.request_ack(second_session, "second warning", cancel_second).await
    });
    for _ in 0..500 {
        if pending_ack.lock().unwrap_or_else(|e| e.into_inner()).len() == 2 {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(
        pending_ack.lock().unwrap_or_else(|e| e.into_inner()).len(),
        crate::widgets::capability_dialog::MAX_PENDING_ACKS,
        "the second ack must be queued, not refused or overwritten"
    );

    // Refused: a third request exceeds the bound and fails closed.
    let third = sink.request_ack(Ulid::new(), "third warning", cancel.clone()).await;
    assert!(!third, "an ack beyond the bounded depth must be refused (abort)");
    assert_eq!(
        pending_ack.lock().unwrap_or_else(|e| e.into_inner()).len(),
        crate::widgets::capability_dialog::MAX_PENDING_ACKS,
        "a refused ack must not grow the queue"
    );

    // The refusal is visible: an ErrorOccurred event carries the named error.
    let mut refusal_seen = false;
    while let Ok(event) = events.try_recv() {
        if let EventKind::ErrorOccurred { message } = &event.kind {
            if message.contains("queue is full") {
                refusal_seen = true;
            }
        }
    }
    assert!(refusal_seen, "a refused ack must publish an ErrorOccurred event");

    // FIFO: the first, then the second, resolve in order.
    assert!(
        crate::widgets::capability_dialog::resolve_ack(&pending_ack, first_session, true),
        "the active first ack resolves first"
    );
    assert!(first.await.expect("first ack task panicked"), "the first ack continues");
    assert!(
        crate::widgets::capability_dialog::resolve_ack(&pending_ack, second_session, false),
        "the queued second ack resolves next"
    );
    assert!(!second.await.expect("second ack task panicked"), "the second ack aborts");
    assert!(
        pending_ack.lock().unwrap_or_else(|e| e.into_inner()).is_empty(),
        "the queue drains in order"
    );
}

#[tokio::test]
async fn approval_sink_granted_single_decision_does_not_enable_auto_approve() {
    let cap_pending = crate::widgets::capability_dialog::shared_pending();
    let pending_ack = crate::widgets::capability_dialog::shared_pending_ack();
    let sink = DesktopApprovalSink {
        cap_pending: cap_pending.clone(),
        pending_ack: pending_ack.clone(),
        pending_intent: crate::widgets::capability_dialog::shared_pending_intent(),
        pending_plan: crate::widgets::capability_dialog::shared_pending_plan(),
        auto_approve: Arc::new(AtomicBool::new(false)),
        bus: EventBus::default(),
    };
    let cancel = CancellationToken::new();

    // First request: the user picks "Grant for this session" (Granted).
    let sink2 = sink.clone();
    let cancel2 = cancel.clone();
    let handle = tokio::spawn(async move {
        let input = serde_json::json!({ "path": "/tmp/example.rs" });
        let action = make_action("write_file", &input);
        sink2.request_approval(&action, cancel2).await
    });
    wait_for_pending_dialog(&cap_pending).await;
    assert!(
        crate::widgets::capability_dialog::resolve(
            &cap_pending,
            &crate::widgets::capability_dialog::Message::GrantSession
        ),
        "expected a pending approval to resolve"
    );
    assert_eq!(handle.await.expect("request task panicked"), ApprovalDecision::Approve);
    assert!(
        !sink.auto_approve.load(Ordering::Relaxed),
        "a single Granted decision must not enable auto-approve"
    );

    // Second request: a different path for the same tool. It must prompt
    // again (no name-based cache) — denying it yields Deny, whereas the
    // old behavior auto-approved every `write_file` after the first grant.
    let sink2 = sink.clone();
    let cancel2 = cancel.clone();
    let handle = tokio::spawn(async move {
        let input = serde_json::json!({ "path": "/tmp/other.rs" });
        let action = make_action("write_file", &input);
        sink2.request_approval(&action, cancel2).await
    });
    wait_for_pending_dialog(&cap_pending).await;
    assert!(
        crate::widgets::capability_dialog::resolve(
            &cap_pending,
            &crate::widgets::capability_dialog::Message::Deny
        ),
        "expected the second approval to prompt again"
    );
    assert_eq!(handle.await.expect("request task panicked"), ApprovalDecision::Deny);
    assert!(
        !sink.auto_approve.load(Ordering::Relaxed),
        "auto-approve must stay off after a per-call grant"
    );
}

#[tokio::test]
async fn approval_sink_grant_always_enables_auto_approve_and_records_session_decision() {
    let cap_pending = crate::widgets::capability_dialog::shared_pending();
    let pending_ack = crate::widgets::capability_dialog::shared_pending_ack();
    let sink = DesktopApprovalSink {
        cap_pending: cap_pending.clone(),
        pending_ack: pending_ack.clone(),
        pending_intent: crate::widgets::capability_dialog::shared_pending_intent(),
        pending_plan: crate::widgets::capability_dialog::shared_pending_plan(),
        auto_approve: Arc::new(AtomicBool::new(false)),
        bus: EventBus::default(),
    };
    let cancel = CancellationToken::new();

    // User picks "Always allow" (GrantedPersistent): the sink must flip its
    // auto-approve flag AND return ApproveAllForSession so the audit log
    // records "ApprovedAllForSession" (mirrors the CLI sink).
    let sink2 = sink.clone();
    let cancel2 = cancel.clone();
    let handle = tokio::spawn(async move {
        let input = serde_json::json!({ "path": "/tmp/example.rs" });
        let action = make_action("write_file", &input);
        sink2.request_approval(&action, cancel2).await
    });
    wait_for_pending_dialog(&cap_pending).await;
    assert!(
        crate::widgets::capability_dialog::resolve(
            &cap_pending,
            &crate::widgets::capability_dialog::Message::GrantAlways
        ),
        "expected a pending approval to resolve"
    );
    assert_eq!(
        handle.await.expect("request task panicked"),
        ApprovalDecision::ApproveAllForSession
    );
    assert!(sink.auto_approve.load(Ordering::Relaxed));

    // Subsequent request is approved without a dialog.
    let input2 = serde_json::json!({ "path": "/tmp/other.rs" });
    let action2 = make_action("write_file", &input2);
    let decision = sink.request_approval(&action2, cancel).await;
    assert_eq!(decision, ApprovalDecision::Approve);
    assert!(
        cap_pending.lock().unwrap_or_else(|e| e.into_inner()).is_empty(),
        "auto-approve must not queue a dialog"
    );
}

// -----------------------------------------------------------------------
// request_intent_confirmation (ADR-55 §2)
// -----------------------------------------------------------------------

/// Wait until the sink's request future has queued a dialog on
/// `pending_intent`, so the test resolves it without racing the spawn.
async fn wait_for_pending_intent(shared: &crate::widgets::capability_dialog::SharedPendingIntent) {
    for _ in 0..500 {
        if !shared.lock().unwrap_or_else(|e| e.into_inner()).is_empty() {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("intent confirmation never queued a dialog");
}

#[tokio::test]
async fn intent_sink_returned_outcome_when_user_picks_an_option() {
    let cap_pending = crate::widgets::capability_dialog::shared_pending();
    let pending_ack = crate::widgets::capability_dialog::shared_pending_ack();
    let pending_intent = crate::widgets::capability_dialog::shared_pending_intent();
    let sink = DesktopApprovalSink {
        cap_pending: cap_pending.clone(),
        pending_ack: pending_ack.clone(),
        pending_intent: pending_intent.clone(),
        pending_plan: crate::widgets::capability_dialog::shared_pending_plan(),
        auto_approve: Arc::new(AtomicBool::new(false)),
        bus: EventBus::default(),
    };
    let cancel = CancellationToken::new();
    let options =
        vec![RequestedOutcome::Answer, RequestedOutcome::Diagnose, RequestedOutcome::Execute];

    // Request runs in background while the UI would show the dialog.
    let sink2 = sink.clone();
    let cancel2 = cancel.clone();
    let handle = tokio::spawn(async move {
        sink2.request_intent_confirmation("What should I work on?".into(), &options, cancel2).await
    });
    wait_for_pending_intent(&pending_intent).await;

    // The user picks an outcome via the dialog's select button.
    assert!(
        crate::widgets::capability_dialog::resolve_intent(
            &pending_intent,
            crate::widgets::capability_dialog::IntentDialogMessage::Select(
                RequestedOutcome::Execute
            )
        ),
        "expected a pending intent to resolve"
    );
    assert_eq!(handle.await.expect("intent task panicked"), Some(RequestedOutcome::Execute));
    assert!(
        pending_intent.lock().unwrap_or_else(|e| e.into_inner()).is_empty(),
        "a resolved intent must not stay queued"
    );
}

#[tokio::test]
async fn intent_sink_cancel_returns_none() {
    let cap_pending = crate::widgets::capability_dialog::shared_pending();
    let pending_ack = crate::widgets::capability_dialog::shared_pending_ack();
    let pending_intent = crate::widgets::capability_dialog::shared_pending_intent();
    let sink = DesktopApprovalSink {
        cap_pending: cap_pending.clone(),
        pending_ack: pending_ack.clone(),
        pending_intent: pending_intent.clone(),
        pending_plan: crate::widgets::capability_dialog::shared_pending_plan(),
        auto_approve: Arc::new(AtomicBool::new(false)),
        bus: EventBus::default(),
    };
    let cancel = CancellationToken::new();
    let options = vec![RequestedOutcome::Execute];

    let sink2 = sink.clone();
    let cancel2 = cancel.clone();
    let handle = tokio::spawn(async move {
        sink2.request_intent_confirmation("Proceed?".into(), &options, cancel2).await
    });
    wait_for_pending_intent(&pending_intent).await;

    // Cancel (reject) resolves the dialog with `None` → read-only run.
    assert!(
        crate::widgets::capability_dialog::resolve_intent(
            &pending_intent,
            crate::widgets::capability_dialog::IntentDialogMessage::Cancel
        ),
        "expected a pending intent to resolve"
    );
    assert_eq!(handle.await.expect("intent task panicked"), None);
}

#[tokio::test]
async fn intent_sink_returns_none_when_dialog_dropped_without_selection() {
    let cap_pending = crate::widgets::capability_dialog::shared_pending();
    let pending_ack = crate::widgets::capability_dialog::shared_pending_ack();
    let pending_intent = crate::widgets::capability_dialog::shared_pending_intent();
    let sink = DesktopApprovalSink {
        cap_pending: cap_pending.clone(),
        pending_ack: pending_ack.clone(),
        pending_intent: pending_intent.clone(),
        pending_plan: crate::widgets::capability_dialog::shared_pending_plan(),
        auto_approve: Arc::new(AtomicBool::new(false)),
        bus: EventBus::default(),
    };
    let cancel = CancellationToken::new();
    let options = vec![RequestedOutcome::Plan, RequestedOutcome::Execute];

    let sink2 = sink.clone();
    let cancel2 = cancel.clone();
    let handle = tokio::spawn(async move {
        sink2.request_intent_confirmation("Plan or do?".into(), &options, cancel2).await
    });
    wait_for_pending_intent(&pending_intent).await;

    // The dialog is dropped without a selection (e.g. app teardown): the
    // oneshot sender is cancelled and the sink falls back to the
    // conservative read-only `None`, as if never invoked.
    {
        let _ = pending_intent.lock().unwrap_or_else(|e| e.into_inner()).pop_front();
    }
    assert_eq!(handle.await.expect("intent task panicked"), None);
}

#[tokio::test]
async fn intent_sink_empty_options_returns_none_without_dialog() {
    let cap_pending = crate::widgets::capability_dialog::shared_pending();
    let pending_ack = crate::widgets::capability_dialog::shared_pending_ack();
    let pending_intent = crate::widgets::capability_dialog::shared_pending_intent();
    let sink = DesktopApprovalSink {
        cap_pending: cap_pending.clone(),
        pending_ack: pending_ack.clone(),
        pending_intent: pending_intent.clone(),
        pending_plan: crate::widgets::capability_dialog::shared_pending_plan(),
        auto_approve: Arc::new(AtomicBool::new(false)),
        bus: EventBus::default(),
    };
    let cancel = CancellationToken::new();
    let options: Vec<RequestedOutcome> = Vec::new();

    // Nothing to confirm: the sink must not queue a dialog.
    let result =
        sink.request_intent_confirmation("nothing to confirm".into(), &options, cancel).await;
    assert_eq!(result, None);
    assert!(
        pending_intent.lock().unwrap_or_else(|e| e.into_inner()).is_empty(),
        "empty options must not queue a dialog"
    );
}

// -----------------------------------------------------------------------
// request_plan_approval (ADR-55 §4)
// -----------------------------------------------------------------------

/// Wait until the sink's request future has queued a dialog on
/// `pending_plan`, so the test resolves it without racing the spawn.
async fn wait_for_pending_plan(shared: &crate::widgets::capability_dialog::SharedPendingPlan) {
    for _ in 0..500 {
        if !shared.lock().unwrap_or_else(|e| e.into_inner()).is_empty() {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("plan approval never queued a dialog");
}

/// Build a sink wired to its own fresh queues plus the given plan queue and
/// event bus, so tests observe the event and the chosen decision in
/// isolation.
fn plan_sink(
    pending_plan: crate::widgets::capability_dialog::SharedPendingPlan,
    bus: EventBus,
) -> DesktopApprovalSink {
    DesktopApprovalSink {
        cap_pending: crate::widgets::capability_dialog::shared_pending(),
        pending_ack: crate::widgets::capability_dialog::shared_pending_ack(),
        pending_intent: crate::widgets::capability_dialog::shared_pending_intent(),
        pending_plan,
        auto_approve: Arc::new(AtomicBool::new(false)),
        bus,
    }
}

#[tokio::test]
async fn plan_sink_apply_sends_apply_and_publishes_event() {
    let pending_plan = crate::widgets::capability_dialog::shared_pending_plan();
    let bus = EventBus::default();
    let mut events = bus.subscribe();
    let sink = plan_sink(pending_plan.clone(), bus);
    let session_id = Ulid::new();
    const PLAN_ID: &str = "01JTESTPLAN0000000000001A";

    let sink2 = sink.clone();
    let handle = tokio::spawn(async move {
        sink2
            .request_plan_approval(
                session_id,
                PLAN_ID,
                "Apply the stored plan?".into(),
                "step 1: rework module\nstep 2: verify",
                time::OffsetDateTime::now_utc(),
                CancellationToken::new(),
            )
            .await
    });
    wait_for_pending_plan(&pending_plan).await;

    // The redraw signal is published with the `intent:plan` tool identity.
    let mut seen = false;
    while let Ok(event) = events.try_recv() {
        if let EventKind::ApprovalRequested { tool_name, timeout_secs } = &event.kind {
            if tool_name == "intent:plan" && *timeout_secs == 0 {
                seen = true;
            }
        }
    }
    assert!(seen, "request_plan_approval must publish an ApprovalRequested event");

    // Apply resolves the dialog with `Some(Apply)`.
    assert!(
        crate::widgets::capability_dialog::resolve_plan(
            &pending_plan,
            session_id,
            PLAN_ID,
            crate::widgets::capability_dialog::PlanDialogMessage::Apply,
        ),
        "expected a pending plan to resolve"
    );
    assert_eq!(handle.await.expect("plan task panicked"), Some(PlanDecision::Apply));
    assert!(
        pending_plan.lock().unwrap_or_else(|e| e.into_inner()).is_empty(),
        "a resolved plan must not stay queued"
    );
}

#[tokio::test]
async fn plan_sink_replan_sends_replan() {
    let pending_plan = crate::widgets::capability_dialog::shared_pending_plan();
    let sink = plan_sink(pending_plan.clone(), EventBus::default());
    let session_id = Ulid::new();
    const PLAN_ID: &str = "01JTESTPLAN0000000000001B";

    let sink2 = sink.clone();
    let handle = tokio::spawn(async move {
        sink2
            .request_plan_approval(
                session_id,
                PLAN_ID,
                "Apply it or replan?".into(),
                "step 1: draft",
                time::OffsetDateTime::now_utc(),
                CancellationToken::new(),
            )
            .await
    });
    wait_for_pending_plan(&pending_plan).await;

    assert!(
        crate::widgets::capability_dialog::resolve_plan(
            &pending_plan,
            session_id,
            PLAN_ID,
            crate::widgets::capability_dialog::PlanDialogMessage::Replan,
        ),
        "expected a pending plan to resolve"
    );
    assert_eq!(handle.await.expect("plan task panicked"), Some(PlanDecision::Replan));
}

#[tokio::test]
async fn plan_sink_cancel_returns_none() {
    let pending_plan = crate::widgets::capability_dialog::shared_pending_plan();
    let sink = plan_sink(pending_plan.clone(), EventBus::default());
    let session_id = Ulid::new();
    const PLAN_ID: &str = "01JTESTPLAN0000000000001C";

    let sink2 = sink.clone();
    let handle = tokio::spawn(async move {
        sink2
            .request_plan_approval(
                session_id,
                PLAN_ID,
                "Apply the stored plan?".into(),
                "step 1: change",
                time::OffsetDateTime::now_utc(),
                CancellationToken::new(),
            )
            .await
    });
    wait_for_pending_plan(&pending_plan).await;

    // Dismiss (Cancel) resolves the dialog with `None` → read-only run.
    assert!(
        crate::widgets::capability_dialog::resolve_plan(
            &pending_plan,
            session_id,
            PLAN_ID,
            crate::widgets::capability_dialog::PlanDialogMessage::Cancel,
        ),
        "expected a pending plan to resolve"
    );
    assert_eq!(handle.await.expect("plan task panicked"), None);
    assert!(
        pending_plan.lock().unwrap_or_else(|e| e.into_inner()).is_empty(),
        "a dismissed plan must not stay queued"
    );
}

#[tokio::test]
async fn plan_sink_returns_none_when_dialog_dropped_without_selection() {
    let pending_plan = crate::widgets::capability_dialog::shared_pending_plan();
    let sink = plan_sink(pending_plan.clone(), EventBus::default());
    let session_id = Ulid::new();
    const PLAN_ID: &str = "01JTESTPLAN0000000000001D";

    let sink2 = sink.clone();
    let handle = tokio::spawn(async move {
        sink2
            .request_plan_approval(
                session_id,
                PLAN_ID,
                "Apply the stored plan?".into(),
                "step 1: change",
                time::OffsetDateTime::now_utc(),
                CancellationToken::new(),
            )
            .await
    });
    wait_for_pending_plan(&pending_plan).await;

    // The dialog is dropped without a selection (e.g. window close): the
    // oneshot sender is cancelled and the sink falls back to the
    // conservative read-only `None`, as if never invoked.
    {
        let _ = pending_plan.lock().unwrap_or_else(|e| e.into_inner()).pop_front();
    }
    assert_eq!(handle.await.expect("plan task panicked"), None);
}

#[tokio::test]
async fn plan_sink_cross_session_does_not_resolve() {
    let pending_plan = crate::widgets::capability_dialog::shared_pending_plan();
    let sink = plan_sink(pending_plan.clone(), EventBus::default());
    let session_id = Ulid::new();
    let other_session = Ulid::new();
    const PLAN_ID: &str = "01JTESTPLAN0000000000001E";
    const OTHER_PLAN_ID: &str = "01JTESTPLAN0000000000FFFF";

    let sink2 = sink.clone();
    let handle = tokio::spawn(async move {
        sink2
            .request_plan_approval(
                session_id,
                PLAN_ID,
                "Apply the stored plan?".into(),
                "step 1: change",
                time::OffsetDateTime::now_utc(),
                CancellationToken::new(),
            )
            .await
    });
    wait_for_pending_plan(&pending_plan).await;

    // A cross-session resolve must not answer this prompt: the entry stays
    // queued and no decision is delivered.
    assert!(
        !crate::widgets::capability_dialog::resolve_plan(
            &pending_plan,
            other_session,
            PLAN_ID,
            crate::widgets::capability_dialog::PlanDialogMessage::Apply,
        ),
        "a cross-session resolve must be rejected"
    );
    assert!(
        !pending_plan.lock().unwrap_or_else(|e| e.into_inner()).is_empty(),
        "a rejected resolve must leave the entry queued"
    );
    assert!(!handle.is_finished(), "a rejected resolve must not answer the task");

    // A wrong plan_id for the same session is also rejected.
    assert!(
        !crate::widgets::capability_dialog::resolve_plan(
            &pending_plan,
            session_id,
            OTHER_PLAN_ID,
            crate::widgets::capability_dialog::PlanDialogMessage::Apply,
        ),
        "a wrong plan_id resolve must be rejected"
    );

    // The matching resolve then completes as expected.
    assert!(crate::widgets::capability_dialog::resolve_plan(
        &pending_plan,
        session_id,
        PLAN_ID,
        crate::widgets::capability_dialog::PlanDialogMessage::Replan,
    ));
    assert_eq!(handle.await.expect("plan task panicked"), Some(PlanDecision::Replan));
    assert!(
        pending_plan.lock().unwrap_or_else(|e| e.into_inner()).is_empty(),
        "a resolved plan must not stay queued"
    );
}

// -----------------------------------------------------------------------
// ADR-58/59 (rewritten) Slice 2 — single-arm Save (include source)
// -----------------------------------------------------------------------
//
// Each test follows the CONFIG_ENV_LOCK + XDG_CONFIG_HOME redirect pattern
// of the auto-seed matrix: lock the env lock, redirect the config dir to a
// tempdir, seed the files through the config crate's own seams, construct
// `App::new`, point `project_dir` at the seeded project, and restore the
// env BEFORE any assertion so a panic cannot leak the redirect into later
// tests.
//
// `reconcile_config_from_reload` activates `[orchestration]` but — by
// design (ADR-57 §3b) — never rebuilds Studio drafts, so the tests then
// populate the Studio from the reloaded config exactly like `App::new`
// does at startup (the app constructs with the real initial project dir,
// which these tests cannot predict). The Save dispatch itself runs while
// the env is still redirected: `persist_include_blueprint` resolves the
// global config dir for its target-shadow guard, and the save re-loads
// config on success.

/// Save on the include source writes the edited blueprint to the PROJECT
/// include file the selection points at — and only there — plus (global-
/// only roster) the agent roster into the global config: the inline
/// edited blueprint NEVER reaches the global `[orchestration]` (global-
/// only enforcement 2026-09: the include selection itself is seeded into
/// the GLOBAL config — project-layer orchestration keys are ignored at
/// load; the include file the selection points at is not a config key,
/// so keeping the file in the project directory stays valid).
#[test]
fn save_on_blueprint_path_writes_the_edited_blueprint_to_the_project_include() {
    let _guard = CONFIG_ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let dir = tempfile::tempdir().expect("tempdir");
    let previous = std::env::var_os("XDG_CONFIG_HOME");
    std::env::set_var("XDG_CONFIG_HOME", dir.path());

    // Global config file: the include selection is seeded here (global-
    // only orchestration) and the roster save lands here, but the edited
    // inline blueprint must never.
    let global_dir = dir.path().join("concerto");
    std::fs::create_dir_all(&global_dir).expect("create global config dir");
    let global_config_path = global_dir.join("config.toml");
    std::fs::write(&global_config_path, "schema_version = 7\n").expect("seed global config");
    concerto_config::save_blueprint_selection(
        &global_config_path,
        &concerto_config::BlueprintSelection {
            name: None,
            include: Some(concerto_config::BLUEPRINT_INCLUDE_FILE.to_string()),
            inline: None,
        },
    )
    .expect("seed global include selection");

    // The include file the selection points at lives in the GLOBAL config
    // dir: `load_global_config` (settings editor + reconcile) resolves
    // include paths without a project root, so a global-layer selection
    // with a project-dir include file would leave the global load broken.
    let project_dir = dir.path().join("project");
    std::fs::create_dir_all(&project_dir).expect("create project dir");
    let include_target = global_dir.join(concerto_config::BLUEPRINT_INCLUDE_FILE);
    let standard = concerto_config::named_blueprint("standard").expect("standard named blueprint");
    concerto_config::save_blueprint(&standard, &include_target).expect("seed include file");

    let (mut app, _) = App::new();
    app.project_dir = project_dir.clone();
    app.reconcile_config_from_reload();
    let config = app.config.clone().expect("config loaded after reconcile");
    app.orchestration_studio.load_from_config(&config);

    // Seed the specialist roster (production does this on Studio open via
    // `ensure_orchestration_seeded`) so the standard blueprint's staffing
    // satisfies the roster-membership rule and Save is not pre-empted.
    let _ = app.update(Message::OrchestrationStudio(
        crate::views::orchestration_studio::StudioMessage::RestoreDefaultAgents,
    ));

    // The Studio draft: edit the first stage's label, add a roster agent
    // (a roster edit alongside the blueprint — exactly one agent list is
    // persisted), then Save.
    let _ = app.orchestration_studio.update(
        crate::views::orchestration_studio::StudioMessage::StageLabelEdited(0, "planning".into()),
    );
    let _ = app
        .orchestration_studio
        .update(crate::views::orchestration_studio::StudioMessage::NewAgentName("Planner".into()));
    let _ = app
        .orchestration_studio
        .update(crate::views::orchestration_studio::StudioMessage::AddAgent);
    let _ = app.update(Message::OrchestrationStudio(
        crate::views::orchestration_studio::StudioMessage::SaveOrchestration,
    ));

    // Env restored before assertions so a panic cannot leak the redirect.
    match previous {
        Some(value) => std::env::set_var("XDG_CONFIG_HOME", value),
        None => std::env::remove_var("XDG_CONFIG_HOME"),
    }

    assert!(
        app.config.as_ref().and_then(|c| c.orchestration.as_ref()).is_some(),
        "the blueprint path must be active before Save"
    );
    let raw = std::fs::read_to_string(&include_target).expect("include read back");
    let saved = concerto_config::parse_blueprint_file(&include_target).expect("include parses");
    assert_eq!(
        saved.pipeline.stages[0].label, "planning",
        "the edited label must reach the include file\n{raw}"
    );
    assert!(!app.orchestration_studio.unsaved, "a successful save marks the studio clean");
    let global_after =
        std::fs::read_to_string(&global_config_path).expect("global config read back");
    assert!(
        !global_after.contains("inline = {"),
        "the edited inline blueprint must never reach the global file (global-only \
             roster aside; the include selection itself is there)\n{global_after}"
    );
    // The roster now lives in per-agent files (single source of truth):
    // the added agent reaches `<global-config-dir>/agents/`, not the config
    // file.
    let agents_dir = global_dir.join(concerto_config::AGENTS_DIR_NAME);
    assert!(
        agents_dir.is_dir(),
        "the per-agent roster directory must exist next to the global config"
    );
    let planner = std::fs::read_dir(&agents_dir)
        .expect("read agents dir")
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().extension().and_then(|e| e.to_str()) == Some("toml"))
        .any(|entry| {
            std::fs::read_to_string(entry.path())
                .map(|raw| raw.contains("Planner"))
                .unwrap_or(false)
        });
    assert!(planner, "the added roster agent must exist as a per-agent file");
    assert!(
        !global_after.contains("[[multi_agent.custom_agents]]"),
        "the roster must no longer be written inline into the config\n{global_after}"
    );
}

/// Save is blocked (draft kept, include untouched) when the draft has
/// validation errors — the belt-and-braces guard behind the disabled
/// Save button.
#[test]
fn save_is_blocked_when_the_draft_has_validation_errors() {
    let _guard = CONFIG_ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let dir = tempfile::tempdir().expect("tempdir");
    let previous = std::env::var_os("XDG_CONFIG_HOME");
    std::env::set_var("XDG_CONFIG_HOME", dir.path());

    let global_dir = dir.path().join("concerto");
    std::fs::create_dir_all(&global_dir).expect("create global config dir");
    let global_config_path = global_dir.join("config.toml");
    std::fs::write(&global_config_path, "schema_version = 7\n").expect("seed global config");
    concerto_config::save_blueprint_selection(
        &global_config_path,
        &concerto_config::BlueprintSelection {
            name: None,
            include: Some(concerto_config::BLUEPRINT_INCLUDE_FILE.to_string()),
            inline: None,
        },
    )
    .expect("seed global include selection");
    let project_dir = dir.path().join("project");
    std::fs::create_dir_all(&project_dir).expect("create project dir");
    // The include file lives in the GLOBAL config dir so the global-layer
    // include selection loads with and without a project root
    // (`load_global_config` has no project dir to resolve through).
    let include_target = global_dir.join(concerto_config::BLUEPRINT_INCLUDE_FILE);
    let standard = concerto_config::named_blueprint("standard").expect("standard named blueprint");
    concerto_config::save_blueprint(&standard, &include_target).expect("seed include file");
    let before = std::fs::read_to_string(&include_target).expect("read include before");

    let (mut app, _) = App::new();
    app.project_dir = project_dir.clone();
    app.reconcile_config_from_reload();
    let config = app.config.clone().expect("config loaded after reconcile");
    app.orchestration_studio.load_from_config(&config);

    // Force a rulebook violation the UI would flag: an empty stage tag
    // (rule (g), "stage tag must be non-empty").
    let _ = app
        .orchestration_studio
        .update(crate::views::orchestration_studio::StudioMessage::StageTagEdited(0, "".into()));
    assert!(
        !app.orchestration_studio.validation().ok,
        "the edited draft must be invalid (precondition)"
    );
    let _ = app.update(Message::OrchestrationStudio(
        crate::views::orchestration_studio::StudioMessage::SaveOrchestration,
    ));

    match previous {
        Some(value) => std::env::set_var("XDG_CONFIG_HOME", value),
        None => std::env::remove_var("XDG_CONFIG_HOME"),
    }

    let after = std::fs::read_to_string(&include_target).expect("read include after");
    assert_eq!(before, after, "an invalid draft must never reach the include file");
    assert!(
        app.orchestration_studio.save_error.is_some(),
        "the save failure must surface on the studio"
    );
}

/// Save is blocked when the include file on disk no longer parses — the
/// data-loss guard: a write from the in-memory model alone would silently
/// drop the unknown keys the file carries (`deny_unknown_fields`).
#[test]
fn save_is_blocked_when_the_include_file_does_not_parse() {
    let _guard = CONFIG_ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let dir = tempfile::tempdir().expect("tempdir");
    let previous = std::env::var_os("XDG_CONFIG_HOME");
    std::env::set_var("XDG_CONFIG_HOME", dir.path());

    let global_dir = dir.path().join("concerto");
    std::fs::create_dir_all(&global_dir).expect("create global config dir");
    let global_config_path = global_dir.join("config.toml");
    std::fs::write(&global_config_path, "schema_version = 7\n").expect("seed global config");
    concerto_config::save_blueprint_selection(
        &global_config_path,
        &concerto_config::BlueprintSelection {
            name: None,
            include: Some(concerto_config::BLUEPRINT_INCLUDE_FILE.to_string()),
            inline: None,
        },
    )
    .expect("seed global include selection");
    let project_dir = dir.path().join("project");
    std::fs::create_dir_all(&project_dir).expect("create project dir");
    // The include file lives in the GLOBAL config dir so the global-layer
    // include selection loads with and without a project root
    // (`load_global_config` has no project dir to resolve through).
    let include_target = global_dir.join(concerto_config::BLUEPRINT_INCLUDE_FILE);
    let standard = concerto_config::named_blueprint("standard").expect("standard named blueprint");
    concerto_config::save_blueprint(&standard, &include_target).expect("seed include file");

    let (mut app, _) = App::new();
    app.project_dir = project_dir.clone();
    app.reconcile_config_from_reload();
    let config = app.config.clone().expect("config loaded after reconcile");
    app.orchestration_studio.load_from_config(&config);

    // Seed the specialist roster (production does this on Studio open via
    // `ensure_orchestration_seeded`) so the standard blueprint's staffing
    // satisfies the roster-membership rule and Save reaches the include
    // guard under test.
    let _ = app.update(Message::OrchestrationStudio(
        crate::views::orchestration_studio::StudioMessage::RestoreDefaultAgents,
    ));

    // The watcher (or a hand edit) replaced the include with garbage
    // AFTER the load: the on-disk file no longer parses.
    let garbage = b"this is { not toml ==";
    std::fs::write(&include_target, garbage).expect("corrupt the include file");
    let _ = app.update(Message::OrchestrationStudio(
        crate::views::orchestration_studio::StudioMessage::SaveOrchestration,
    ));

    match previous {
        Some(value) => std::env::set_var("XDG_CONFIG_HOME", value),
        None => std::env::remove_var("XDG_CONFIG_HOME"),
    }

    let after = std::fs::read(&include_target).expect("read include after");
    assert_eq!(after, garbage, "the unparseable file must be left untouched");
    let error = app.orchestration_studio.save_error.as_deref().expect("save_error set");
    assert!(
        error.contains("orchestration.blueprint.toml") || error.contains("failed to load"),
        "the save error must carry the path or a parse detail: {error}"
    );
}

/// Save writes the global-dir include the selection points at — the
/// same file the config would load. (Global-only enforcement 2026-09
/// AMENDED the former shadow-refusal: with the include living in the
/// global config dir and resolved by the same project-first/global-second
/// order, saving over it is exactly right; the bare fallback and the
/// project file are the other two allowed targets. The guard remains for
/// any other target shape.)
#[test]
fn save_writes_the_global_dir_include_the_selection_points_at() {
    let _guard = CONFIG_ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let dir = tempfile::tempdir().expect("tempdir");
    let previous = std::env::var_os("XDG_CONFIG_HOME");
    std::env::set_var("XDG_CONFIG_HOME", dir.path());

    // Global-scope include + a GLOBAL selection pointing at it; NO project
    // include, so load resolves the include from the global config dir.
    let global_dir = dir.path().join("concerto");
    std::fs::create_dir_all(&global_dir).expect("create global config dir");
    let global_include = global_dir.join(concerto_config::BLUEPRINT_INCLUDE_FILE);
    let standard = concerto_config::named_blueprint("standard").expect("standard named blueprint");
    concerto_config::save_blueprint(&standard, &global_include).expect("seed global include");
    let global_config_path = global_dir.join("config.toml");
    std::fs::write(&global_config_path, "schema_version = 7\n").expect("seed global config");
    concerto_config::save_blueprint_selection(
        &global_config_path,
        &concerto_config::BlueprintSelection {
            name: None,
            include: Some(concerto_config::BLUEPRINT_INCLUDE_FILE.to_string()),
            inline: None,
        },
    )
    .expect("seed global selection");
    let project_dir = dir.path().join("project");
    std::fs::create_dir_all(&project_dir).expect("create project dir");
    let project_include = project_dir.join(concerto_config::BLUEPRINT_INCLUDE_FILE);

    let (mut app, _) = App::new();
    app.project_dir = project_dir.clone();
    app.reconcile_config_from_reload();
    let config = app.config.clone().expect("config loaded after reconcile");
    assert!(
        config.orchestration.as_ref().is_some(),
        "the global-scope selection must activate the blueprint path"
    );
    app.orchestration_studio.load_from_config(&config);

    let _ = app.update(Message::OrchestrationStudio(
        crate::views::orchestration_studio::StudioMessage::SaveOrchestration,
    ));

    match previous {
        Some(value) => std::env::set_var("XDG_CONFIG_HOME", value),
        None => std::env::remove_var("XDG_CONFIG_HOME"),
    }

    assert!(
        !project_include.exists(),
        "no file may be created in the project dir — the include lives in the global dir"
    );
    assert!(!app.orchestration_studio.unsaved, "a successful save marks the studio clean");
    let saved = concerto_config::parse_blueprint_file(&global_include)
        .expect("the saved include must parse");
    assert_eq!(
        saved.schema_version,
        concerto_config::ORCHESTRATION_SCHEMA_VERSION,
        "the global-dir include the selection points at must be rewritten by Save"
    );
}

/// B1 regression guard (oracle finding), ADR-58/59 (rewritten) Slice 2 shape: the app's
/// default first-run flow must round-trip — auto-seed (inline) → edit →
/// save → reload must load the EDITS. The Slice-2 default selection is
/// INLINE, so the runtime consumes exactly what the seed (and every
/// subsequent Save) writes — no include file, no catalog indirection.
#[test]
fn default_auto_seed_then_save_then_reload_round_trips_the_edited_blueprint() {
    let _guard = CONFIG_ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let dir = tempfile::tempdir().expect("tempdir");
    let mut previous = std::env::var_os("XDG_CONFIG_HOME");
    std::env::set_var("XDG_CONFIG_HOME", dir.path());

    let global_dir = dir.path().join("concerto");
    std::fs::create_dir_all(&global_dir).expect("create global config dir");
    std::fs::write(global_dir.join("config.toml"), "schema_version = 7\n")
        .expect("seed global config");
    let project_dir = dir.path().join("project");
    std::fs::create_dir_all(&project_dir).expect("create project dir");

    let (mut app, _) = App::new();
    app.project_dir = project_dir.clone();
    // Force the fresh-project shape (see the first-open test): the seed
    // must really write for this round-trip to prove anything.
    app.config = None;
    // First-run bootstrap: opening the Studio seeds the roster inline.
    // All app operations stay under the XDG redirect — whole-config
    // persistence paths serialize the app's in-memory global config, so
    // any app call against the real machine config would leak test state
    // there. The env is restored only before the final assertions.
    let _ = app.update(Message::Navigate(Page::OrchestrationStudio));

    // The auto-seed activates the blueprint path through an INLINE
    // selection (the Slice-2 default shape).
    let config = app.config.clone().expect("config loaded after the auto-seed");
    assert!(
        config.orchestration.as_ref().is_some(),
        "the app's own default auto-seed must activate [orchestration]"
    );
    assert!(
        config.orchestration.as_ref().and_then(|o| o.blueprint.include.as_deref()).is_none(),
        "the default selection is inline — no include file is involved"
    );
    assert!(
        config.orchestration.as_ref().and_then(|o| o.blueprint.inline.as_ref()).is_some(),
        "the default selection must carry the inline blueprint"
    );

    // Populate the Studio draft from the activated config, edit stage 0's
    // label, then Save through the single arm.
    app.orchestration_studio.load_from_config(&config);
    let _ = app.orchestration_studio.update(
        crate::views::orchestration_studio::StudioMessage::StageLabelEdited(0, "planning".into()),
    );
    let _ = app.update(Message::OrchestrationStudio(
        crate::views::orchestration_studio::StudioMessage::SaveOrchestration,
    ));
    assert!(!app.orchestration_studio.unsaved, "a successful save marks the studio clean");

    // Save rewrote the inline in the GLOBAL config (global-only
    // orchestration); the project directory stays file-free — not an
    // include file either. Reload the fresh file while the redirect is
    // still active — whole-config persistence paths serialize the app's
    // in-memory global config, and running them against the real machine
    // config would leak test state there.
    assert!(
        !project_dir.join(concerto_config::legacy::NEW_PROJECT_CONFIG_FILE).exists(),
        "the save must never create a project .concerto.toml"
    );
    assert!(
        !project_dir.join(concerto_config::BLUEPRINT_INCLUDE_FILE).exists(),
        "no include file is created on the default inline path"
    );
    let global_config_path = dir.path().join("concerto").join("config.toml");
    let raw_global = std::fs::read_to_string(&global_config_path).expect("global config read");
    assert!(
        raw_global.contains("inline = {"),
        "save must write the edited blueprint inline into the global file\n{raw_global}"
    );

    // A full reload from disk must now load the EDITS — the B1 property:
    // the runtime consumes the inline Save wrote, not an unedited default.
    app.reconcile_config_from_reload();

    // Env restored after every app operation, before the final assertions.
    let previous = previous.take();
    match &previous {
        Some(value) => std::env::set_var("XDG_CONFIG_HOME", value),
        None => std::env::remove_var("XDG_CONFIG_HOME"),
    }
    let reloaded = app.config.clone().expect("config after reload");
    assert!(reloaded.orchestration.as_ref().is_some(), "reload must keep [orchestration] active");
    let reloaded_label =
        reloaded.resolved_blueprint.as_ref().map(|r| r.blueprint.pipeline.stages[0].label.as_str());
    assert_eq!(
        reloaded_label,
        Some("planning"),
        "the runtime must load the edited inline — not the unedited standard blueprint"
    );
}

// ---- global-only orchestration enforcement (2026-09): banner + import ----

/// The banner condition tests (keys present ⇒ keys non-empty; absent ⇒
/// suppressed; dismissed ⇒ suppressed for the session). The rendered
/// banner is an opaque iced element in headless tests, so the
/// rendered-state predicate lives here, exactly like
/// `views::orchestration_studio::modified_caption`.
#[test]
fn banner_condition_ignores_and_dismissal() {
    let dir = tempfile::tempdir().expect("tempdir");
    let project_dir = dir.path().join("project");
    std::fs::create_dir_all(&project_dir).expect("create project dir");
    let (mut app, _) = App::new();
    app.project_dir = project_dir.clone();
    app.orchestration_banner_dismissed = false;
    app.project_orchestration_keys = Vec::new();

    // No orchestration keys in the project file → banner suppressed.
    app.refresh_project_orchestration_keys();
    assert!(app.project_orchestration_keys.is_empty());

    std::fs::write(
        project_dir.join(concerto_config::legacy::NEW_PROJECT_CONFIG_FILE),
        "schema_version = 7\n[orchestration]\nschema_version = 1\n",
    )
    .expect("seed project orchestration");
    app.refresh_project_orchestration_keys();
    assert_eq!(app.project_orchestration_keys, vec!["orchestration"]);
    let banner_visible =
        !app.project_orchestration_keys.is_empty() && !app.orchestration_banner_dismissed;
    assert!(banner_visible, "declared keys ⇒ banner shows");

    // Dismiss hides it for this session only (new sessions re-show via a
    // fresh `App` whose `orchestration_banner_dismissed` is `false`).
    let _ = app.update(Message::DismissOrchestrationBanner);
    assert!(app.orchestration_banner_dismissed);
    let banner_visible =
        !app.project_orchestration_keys.is_empty() && !app.orchestration_banner_dismissed;
    assert!(!banner_visible, "dismissed ⇒ banner hidden in-session");
}

/// A project file with keys but no reconciliation must not accidentally
/// clear stale keys: `refresh_project_orchestration_keys` reflects the
/// CURRENT file state, including clearing when the keys disappear.
#[test]
fn banner_keys_clear_when_the_project_file_stops_declaring() {
    let dir = tempfile::tempdir().expect("tempdir");
    let project_dir = dir.path().join("project");
    std::fs::create_dir_all(&project_dir).expect("create project dir");
    let (mut app, _) = App::new();
    app.project_dir = project_dir.clone();
    app.project_orchestration_keys = vec!["orchestration".into()];
    std::fs::write(
        project_dir.join(concerto_config::legacy::NEW_PROJECT_CONFIG_FILE),
        "schema_version = 7\nsession_spend_cap_usd = 2.0\n",
    )
    .expect("seed non-orchestration project config");
    app.refresh_project_orchestration_keys();
    assert!(app.project_orchestration_keys.is_empty(), "stale keys must clear");
}

/// Import round-trip (one explicit user action): the declared keys land
/// in the GLOBAL file, the project file loses them, the reload resolves
/// orchestration from the global layer while the project's unrelated
/// keys keep overriding — and the banner condition clears.
#[test]
fn import_round_trips_the_keys_to_global_and_off_the_project() {
    let _guard = CONFIG_ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let dir = tempfile::tempdir().expect("tempdir");
    let previous = std::env::var_os("XDG_CONFIG_HOME");
    std::env::set_var("XDG_CONFIG_HOME", dir.path());

    let global_dir = dir.path().join("concerto");
    std::fs::create_dir_all(&global_dir).expect("create global config dir");
    let global_config_path = global_dir.join("config.toml");
    std::fs::write(&global_config_path, "schema_version = 7\n").expect("seed global config");

    let project_dir = dir.path().join("project");
    std::fs::create_dir_all(&project_dir).expect("create project dir");
    std::fs::write(
        project_dir.join(concerto_config::legacy::NEW_PROJECT_CONFIG_FILE),
        r#"schema_version = 7
session_spend_cap_usd = 2.5
[orchestration]
schema_version = 1
[orchestration.blueprint]
name = "tdd"
[multi_agent]
max_concurrent_agents = 3
model_pins = { coder = "local-model" }
"#,
    )
    .expect("seed project orchestration");

    let (mut app, _) = App::new();
    app.project_dir = project_dir.clone();
    app.reconcile_config_from_reload();
    assert_eq!(
        app.project_orchestration_keys,
        vec!["orchestration", "multi_agent.model_pins"],
        "precondition: the banner is armed with the ignored keys"
    );
    let banner_visible =
        !app.project_orchestration_keys.is_empty() && !app.orchestration_banner_dismissed;
    assert!(banner_visible);

    // The one explicit import action.
    let _ = app.update(Message::ImportProjectOrchestration);

    // Env restored before assertions so a panic cannot leak the redirect.
    match previous {
        Some(value) => std::env::set_var("XDG_CONFIG_HOME", value),
        None => std::env::remove_var("XDG_CONFIG_HOME"),
    }

    // The global file gained the keys; the project file lost them.
    let gdoc = std::fs::read_to_string(&global_config_path).expect("global read back");
    assert!(gdoc.contains("[orchestration]") && gdoc.contains("name = \"tdd\""), "{gdoc}");
    assert!(gdoc.contains("local-model"), "the pins moved\n{gdoc}");
    let pdoc =
        std::fs::read_to_string(project_dir.join(".concerto.toml")).expect("project read back");
    assert!(!pdoc.contains("[orchestration]"), "orchestration removed\n{pdoc}");
    assert!(!pdoc.contains("local-model"), "pins removed\n{pdoc}");
    assert!(pdoc.contains("session_spend_cap_usd = 2.5"), "unrelated project key kept\n{pdoc}");

    // The reload consumed the import: orchestration resolves from the
    // global layer, the project override still applies, and the banner
    // condition cleared.
    assert!(app.toasts.has_toasts(), "the import must be toasted");
    let cfg = app.config.as_ref().expect("config reloaded");
    assert_eq!(
        cfg.orchestration.as_ref().expect("orchestration present").blueprint.name.as_deref(),
        Some("tdd"),
        "the imported selection resolves from the global layer"
    );
    assert_eq!(
        cfg.session_spend_cap_usd,
        Some(2.5),
        "the unrelated project key still applies after import"
    );
    assert!(app.project_orchestration_keys.is_empty(), "the import clears the banner condition");
}

/// Conflict rule (pinned at the desktop layer): when the global config
/// ALREADY declares the keys, the import refuses — both files stay
/// byte-identical, the refusal is toasted naming the collision (user
/// global data is never silently overwritten).
#[test]
fn import_refuses_when_the_global_config_already_declares_the_keys() {
    let _guard = CONFIG_ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let dir = tempfile::tempdir().expect("tempdir");
    let previous = std::env::var_os("XDG_CONFIG_HOME");
    std::env::set_var("XDG_CONFIG_HOME", dir.path());

    let global_dir = dir.path().join("concerto");
    std::fs::create_dir_all(&global_dir).expect("create global config dir");
    let global_config_path = global_dir.join("config.toml");
    let global_before = "schema_version = 7\n[orchestration]\nschema_version = 1\n";
    std::fs::write(&global_config_path, global_before).expect("seed global config");

    let project_dir = dir.path().join("project");
    std::fs::create_dir_all(&project_dir).expect("create project dir");
    let project_config = project_dir.join(".concerto.toml");
    let project_before = "schema_version = 7\n[orchestration]\nschema_version = 1\n";
    std::fs::write(&project_config, project_before).expect("seed project config");

    let (mut app, _) = App::new();
    app.project_dir = project_dir.clone();
    app.reconcile_config_from_reload();
    let _ = app.update(Message::ImportProjectOrchestration);

    match previous {
        Some(value) => std::env::set_var("XDG_CONFIG_HOME", value),
        None => std::env::remove_var("XDG_CONFIG_HOME"),
    }

    assert!(app.toasts.has_toasts(), "the refusal must surface as a toast");
    assert_eq!(
        std::fs::read_to_string(&global_config_path).unwrap(),
        global_before,
        "the global config must stay untouched"
    );
    assert_eq!(
        std::fs::read_to_string(&project_config).unwrap(),
        project_before,
        "the project config must stay untouched"
    );
    assert!(
        !app.project_orchestration_keys.is_empty(),
        "the keys stay declared (still ignored at load) until the user resolves it"
    );
    let _ = app;
}

/// The Settings theme picker seeds from the applied (persisted) theme at
/// startup, so it never reverts to the hardcoded Midnight default.
#[test]
fn settings_theme_seeds_from_applied_theme() {
    let (app, _) = App::new();
    assert_eq!(
        app.settings.selected_theme, app.current_theme.name,
        "settings picker must show the applied theme after restart"
    );
}

/// Selecting a theme applies and persists it immediately (no SaveSettings
/// round-trip), keeping the picker, the live theme, the prefs store, and
/// the CLI-facing `display.theme` bridge in lockstep. Runs under the
/// config/env lock with isolated data + config dirs so it never touches
/// the developer's real preferences.
#[test]
fn theme_selected_applies_immediately() {
    let _guard = CONFIG_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let config_dir = tempfile::tempdir().expect("tempdir");
    let data_dir = tempfile::tempdir().expect("tempdir");
    let previous_config = std::env::var_os("XDG_CONFIG_HOME");
    let previous_data = std::env::var_os("XDG_DATA_HOME");
    std::env::set_var("XDG_CONFIG_HOME", config_dir.path());
    std::env::set_var("XDG_DATA_HOME", data_dir.path());

    let (mut app, _) = App::new();
    let target = if app.current_theme.name == "Slate" { "Chalk" } else { "Slate" };
    let _ = app.update(Message::Settings(SettingsMessage::ThemeSelected(target)));
    assert_eq!(app.current_theme.name, target);
    assert_eq!(app.settings.selected_theme, target);
    // Applied (prefs) theme and the CLI bridge are written together, so
    // neither store can report a theme the UI is not rendering.
    assert_eq!(
        app.global_config.display.theme.as_deref(),
        Some(target),
        "the in-memory config must carry the applied theme"
    );
    let merged = app.config.as_ref().and_then(|c| c.display.theme.as_deref());
    assert_eq!(merged, Some(target), "the merged config copy must match the global one");
    let on_disk = concerto_config::load_global_config(None).expect("reload config");
    assert_eq!(
        on_disk.display.theme.as_deref(),
        Some(target),
        "the config file must carry the applied theme"
    );
    let prefs_dir = data_dir.path().join("concerto").join("prefs");
    let store = concerto_memory::prefs::UserPrefsStore::open(&prefs_dir).expect("prefs store");
    let saved = crate::theme::prefs::load_theme(&store);
    assert_eq!(saved.name, target, "the prefs store must carry the applied theme");

    // A font-size tweak has no config key: it must apply and persist to
    // prefs without rewriting (and therefore without clobbering) the theme.
    let _ = app.update(Message::Settings(SettingsMessage::FontSizeChanged(18.0)));
    assert_eq!(app.current_theme.font_stack.base_size, 18.0);
    assert_eq!(app.settings.font_size, 18.0);
    let store = concerto_memory::prefs::UserPrefsStore::open(&prefs_dir).expect("prefs store");
    assert_eq!(crate::theme::prefs::load_theme(&store).font_stack.base_size, 18.0);
    let on_disk = concerto_config::load_global_config(None).expect("reload config");
    assert_eq!(on_disk.display.theme.as_deref(), Some(target), "a size tweak keeps the theme");

    // A later save round-trip must never write back a stale theme.
    let _ = app.update(Message::Settings(SettingsMessage::SaveSettings));
    assert_eq!(app.current_theme.name, target, "save must not revert the applied theme");
    assert_eq!(app.settings.selected_theme, target);
    let on_disk = concerto_config::load_global_config(None).expect("reload config");
    assert_eq!(
        on_disk.display.theme.as_deref(),
        Some(target),
        "SaveSettings must not persist a theme the UI is not rendering"
    );

    restore_xdg_env(previous_config, previous_data);
}

/// Startup seeds every theme surface from the prefs store, even when the
/// config file's `display.theme` says something else: prefs are the
/// source of truth, config only bridges the same palettes to the CLI.
#[test]
fn startup_seeds_theme_surfaces_from_prefs_not_config() {
    let _guard = CONFIG_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let config_dir = tempfile::tempdir().expect("tempdir");
    let data_dir = tempfile::tempdir().expect("tempdir");
    let previous_config = std::env::var_os("XDG_CONFIG_HOME");
    let previous_data = std::env::var_os("XDG_DATA_HOME");
    std::env::set_var("XDG_CONFIG_HOME", config_dir.path());
    std::env::set_var("XDG_DATA_HOME", data_dir.path());

    // Config and prefs deliberately disagree; prefs must win for the UI.
    if let Some(path) = concerto_config::default_config_path() {
        let mut config = AppConfig::default();
        config.display.theme = Some("Slate".to_string());
        concerto_config::save_config(&config, &path).expect("seed config");
    }
    let prefs_dir = data_dir.path().join("concerto").join("prefs");
    if let Ok(store) = concerto_memory::prefs::UserPrefsStore::open(&prefs_dir) {
        let theme = crate::theme::AppTheme::by_name("Chalk").with_base_size(18.0);
        crate::theme::prefs::save_theme(&store, &theme);
    }

    let (app, _) = App::new();
    assert_eq!(app.current_theme.name, "Chalk", "startup must render the prefs theme");
    assert_eq!(
        app.settings.selected_theme, "Chalk",
        "the picker must be seeded from the same prefs value as the live theme"
    );
    assert_eq!(app.current_theme.font_stack.base_size, 18.0);
    assert_eq!(app.settings.font_size, 18.0);

    restore_xdg_env(previous_config, previous_data);
}

/// An external prefs change (another window, or the CLI writing the store)
/// is re-applied to every surface that renders it, including the Settings
/// picker — not just the live theme.
#[test]
fn theme_changed_reseeds_the_picker_from_prefs() {
    let _guard = CONFIG_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let config_dir = tempfile::tempdir().expect("tempdir");
    let data_dir = tempfile::tempdir().expect("tempdir");
    let previous_config = std::env::var_os("XDG_CONFIG_HOME");
    let previous_data = std::env::var_os("XDG_DATA_HOME");
    std::env::set_var("XDG_CONFIG_HOME", config_dir.path());
    std::env::set_var("XDG_DATA_HOME", data_dir.path());

    let (mut app, _) = App::new();
    let target = if app.current_theme.name == "Nebula" { "Slate" } else { "Nebula" };
    let prefs_dir = data_dir.path().join("concerto").join("prefs");
    if let Ok(store) = concerto_memory::prefs::UserPrefsStore::open(&prefs_dir) {
        let theme = crate::theme::AppTheme::by_name(target).with_base_size(16.0);
        crate::theme::prefs::save_theme(&store, &theme);
    }

    let _ = app.update(Message::ThemeChanged);
    assert_eq!(app.current_theme.name, target, "the live theme must follow prefs");
    assert_eq!(app.current_theme.font_stack.base_size, 16.0);
    assert_eq!(
        app.settings.selected_theme, target,
        "the picker must be re-seeded from prefs on an external theme change"
    );
    assert_eq!(app.settings.font_size, 16.0);

    restore_xdg_env(previous_config, previous_data);
}

/// A per-agent model override from the right toolbar persists an
/// assignment and keeps the Studio card in sync; clearing it removes the
/// assignment (fall back to the global default). Isolated like the theme
/// test so it never writes the developer's config.
#[test]
fn set_agent_model_persists_and_clears_override() {
    let _guard = CONFIG_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let config_dir = tempfile::tempdir().expect("tempdir");
    let data_dir = tempfile::tempdir().expect("tempdir");
    let previous_config = std::env::var_os("XDG_CONFIG_HOME");
    let previous_data = std::env::var_os("XDG_DATA_HOME");
    std::env::set_var("XDG_CONFIG_HOME", config_dir.path());
    std::env::set_var("XDG_DATA_HOME", data_dir.path());

    // Seed a provider so an agent model override has a valid assignment
    // target; App::new loads it as the active route.
    if let Some(path) = concerto_config::default_config_path() {
        let config = AppConfig {
            model_settings: Some(concerto_config::ModelSettings {
                providers: vec![ProviderConfig {
                    id: "test-provider".into(),
                    provider: "openai".into(),
                    model: "gpt-4o-mini".into(),
                    ..Default::default()
                }],
                global_default_model: Some("gpt-4o-mini".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        concerto_config::save_config(&config, &path).expect("seed config");
    }

    let (mut app, _) = App::new();
    let agent_id = app.orchestration_studio.roster_agents().next().map(|agent| agent.id.clone());
    let Some(agent_id) = agent_id else {
        restore_xdg_env(previous_config, previous_data);
        return;
    };
    let _ = app.update(Message::SetAgentModel {
        agent_id: agent_id.clone(),
        model: "test-model-x".into(),
    });
    assert!(
        app.runtime_assignments().iter().any(
            |a| a.agent_role == agent_id && a.model_override.as_deref() == Some("test-model-x")
        ),
        "the override must be persisted to the model assignment"
    );
    // Write path stays override-only: the quick panel picker displays the
    // *effective* model (override, else Settings global default) but must
    // never repoint single-agent chat when an override is picked.
    assert_eq!(
        app.settings.global_default_model.as_deref(),
        Some("gpt-4o-mini"),
        "picking an agent model must leave the Settings global default alone"
    );
    let _ =
        app.update(Message::SetAgentModel { agent_id: agent_id.clone(), model: "default".into() });
    assert!(
        !app.runtime_assignments().iter().any(|a| a.agent_role == agent_id),
        "clearing must drop the assignment"
    );
    assert_eq!(
        app.settings.global_default_model.as_deref(),
        Some("gpt-4o-mini"),
        "clearing an override must also leave the Settings global default alone"
    );

    restore_xdg_env(previous_config, previous_data);
}

/// Restore the two XDG dirs captured by an isolated env test.
fn restore_xdg_env(
    previous_config: Option<std::ffi::OsString>,
    previous_data: Option<std::ffi::OsString>,
) {
    match previous_config {
        Some(value) => std::env::set_var("XDG_CONFIG_HOME", value),
        None => std::env::remove_var("XDG_CONFIG_HOME"),
    }
    match previous_data {
        Some(value) => std::env::set_var("XDG_DATA_HOME", value),
        None => std::env::remove_var("XDG_DATA_HOME"),
    }
}
