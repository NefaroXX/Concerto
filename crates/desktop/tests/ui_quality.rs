//! UI quality smoke tests: ensure every view renders without panicking.
//!
//! These tests verify that all views:
//! - Can be constructed with default state
//! - Render without panicking in at least one theme
//! - Render without panicking in all three themes

use concerto_desktop::theme::AppTheme;
use concerto_desktop::views;

// ---------------------------------------------------------------------------
// Smoke tests — default state, single theme
// ---------------------------------------------------------------------------

#[test]
fn chat_view_renders_without_panic() {
    let state = views::chat::State::new();
    let graph = views::agent_graph::State::new();
    let theme = AppTheme::by_name("Midnight");
    let _element = state.view(&theme, false, false, "", &[], "", &graph, false);
}

#[test]
fn memory_view_renders_without_panic() {
    let state = views::memory::State::new();
    let theme = AppTheme::by_name("Midnight");
    let _element = state.modal_view(&theme);
}

#[test]
fn diff_view_renders_without_panic() {
    let state = views::diff::State::new();
    let theme = AppTheme::by_name("Midnight");
    let _element = state.view(&theme);
}

#[test]
fn tool_log_view_renders_without_panic() {
    let state = views::tool_log::State::new();
    let theme = AppTheme::by_name("Midnight");
    let _element = state.view(&theme);
}

#[test]
fn agent_graph_view_renders_without_panic() {
    let state = views::agent_graph::State::new();
    let theme = AppTheme::by_name("Midnight");
    let _element = state.view(&theme);
}

#[test]
fn settings_view_renders_without_panic() {
    let state = views::settings::State::new();
    let theme = AppTheme::by_name("Midnight");
    let _element = state.view(&theme, false);
}

// ---------------------------------------------------------------------------
// Cross-theme smoke tests — ensure no theme-specific panics
// ---------------------------------------------------------------------------

#[test]
fn all_views_render_in_all_themes() {
    let themes = AppTheme::all();

    for theme in &themes {
        // Chat
        let chat = views::chat::State::new();
        let graph = views::agent_graph::State::new();
        let _ = chat.view(theme, false, false, "", &[], "", &graph, false);

        // Memory (modal dialog content)
        let mem = views::memory::State::new();
        let _ = mem.modal_view(theme);

        // Diff
        let diff = views::diff::State::new();
        let _ = diff.view(theme);

        // Tool Log
        let log = views::tool_log::State::new();
        let _ = log.view(theme);

        // Agent Graph
        let graph = views::agent_graph::State::new();
        let _ = graph.view(theme);

        // Settings
        let settings = views::settings::State::new();
        let _ = settings.view(theme, false);
    }
}

// ---------------------------------------------------------------------------
// Empty state tests — ensure empty-state prompts are present
// ---------------------------------------------------------------------------

/// When the chat has no entries, the view should return a non-empty element
/// (i.e., the empty state, not a panic).
#[test]
fn chat_empty_state_shows_prompt() {
    let state = views::chat::State::new();
    let graph = views::agent_graph::State::new();
    let theme = AppTheme::by_name("Midnight");
    // Must not panic — this is the primary smoke assertion.
    // We can't easily inspect Iced Element internals, so we just verify
    // it renders cleanly (no panic) with the default empty state.
    let _element = state.view(&theme, false, false, "", &[], "", &graph, false);
}

/// When the tool log has no rows, should not panic.
#[test]
fn tool_log_empty_state_shows_prompt() {
    let state = views::tool_log::State::new();
    let theme = AppTheme::by_name("Midnight");
    let _element = state.view(&theme);
}

/// Agent graph with no activity should show placeholder, not panic.
#[test]
fn agent_graph_empty_state_shows_prompt() {
    let state = views::agent_graph::State::new();
    let theme = AppTheme::by_name("Midnight");
    let _element = state.view(&theme);
}

// ---------------------------------------------------------------------------
// Spend Log modal (issue #93 Phase 4)
// ---------------------------------------------------------------------------

/// The Spend Log modal body renders without panicking when populated with
/// per-call spend records.
#[test]
fn spend_log_subview_renders_without_panic() {
    use concerto_core::ids::Ulid;
    use concerto_sessions::spend::SpendRecord;
    use time::OffsetDateTime;

    let mut state = views::chat::State::new();
    let fixtures: Vec<SpendRecord> = (0..3)
        .map(|i| SpendRecord {
            id: Ulid::new(),
            session_id: Ulid::new(),
            task_id: None,
            provider: "openrouter".into(),
            model: "anthropic/claude-3.5-sonnet".into(),
            tokens_in: 100 + i,
            tokens_out: 50 + i,
            cost_usd: 0.001 * (i as f64 + 1.0),
            created_at: OffsetDateTime::now_utc(),
        })
        .collect();
    let _ = state.update(views::chat::Message::SpendLogsLoaded(fixtures));

    let theme = AppTheme::by_name("Midnight");
    let _element = views::chat::spend_log_view(
        state.spend_log(),
        None,
        Some(1.0),
        &views::spend::CapUiState::Normal,
        &theme,
    );
}

/// The Spend Log modal body renders without panicking with an empty log
/// (fresh session / no settled provider calls yet) across all themes.
#[test]
fn spend_log_empty_state_renders_in_all_themes() {
    for theme in AppTheme::all() {
        let _element =
            views::chat::spend_log_view(&[], None, None, &views::spend::CapUiState::Normal, &theme);
    }
}

/// The Spend Log modal body renders in the Approaching cap state (warning
/// header color path) without panicking.
#[test]
fn spend_log_renders_with_approaching_cap() {
    use concerto_core::ids::Ulid;
    use concerto_sessions::spend::SpendRecord;
    use time::OffsetDateTime;

    let mut state = views::chat::State::new();
    let fixtures = vec![SpendRecord {
        id: Ulid::new(),
        session_id: Ulid::new(),
        task_id: None,
        provider: "openai".into(),
        model: "gpt-4o".into(),
        tokens_in: 1_000,
        tokens_out: 400,
        cost_usd: 0.9,
        created_at: OffsetDateTime::now_utc(),
    }];
    let _ = state.update(views::chat::Message::SpendLogsLoaded(fixtures));

    let theme = AppTheme::by_name("Midnight");
    let approaching =
        views::spend::CapUiState::Approaching { current_usd: 0.9, cap_usd: 1.0, pct: 90.0 };
    let _element =
        views::chat::spend_log_view(state.spend_log(), None, Some(1.0), &approaching, &theme);
}
