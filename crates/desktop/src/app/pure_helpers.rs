//! Free pure functions and small theme/run helpers for [`App`] — NORM slice S48.
//!
//! This module owns two clusters moved verbatim from `app.rs`:
//!
//! * the zero-`self` free-function batch — `ease_out_cubic`, `project_name`,
//!   `transcript_path`, `agent_graph_path`, the chat-entry delegates
//!   `messages_to_entries` / `transcript_to_entries`,
//!   `configured_default_route`, `orchestration_hides_relationships`,
//!   `provider_discovery_ready`, and the prefs loader `load_prefs_theme`.
//!   Every body is line-for-line with its origin at the top-level `fn`
//!   indent; the only structural edit is each (previously module-private)
//!   fn becoming `pub(super)`, which keeps effective visibility unchanged:
//!   every caller lives inside the `app` module tree — the sibling
//!   submodule groups, the root `impl App` bodies left in `app.rs`, and the
//!   tests in `app.rs`'s `mod tests`. `app.rs` re-binds the batch through
//!   `use pure_helpers::{…};` so the existing bare-name call sites and the
//!   `super::{…}` imports in `mod tests` resolve unchanged.
//! * the small `impl App` helpers around the theme and a run: the apply pair
//!   `apply_and_save_theme` / `apply_prefs_theme` (both module-private to
//!   `app.rs` before the move, so they become `pub(super)`), the
//!   `note_run_settled` epoch bump (same treatment), and the three getters
//!   `theme` / `is_run_active` / `run_settle_epoch`, which stay `pub` —
//!   `crate::project_launcher` calls them across the module boundary (the
//!   window-close handler). Method-call syntax means no re-export is needed
//!   for any of them.
//!
//! No `Message` / `App` shape change and no behavior change.

use super::*;

// ---------------------------------------------------------------------------
// Free pure functions (zero `self`)
// ---------------------------------------------------------------------------

/// Ease-out cubic curve: fast start, gentle landing. Used to map the
/// terminal panel's linear animation fraction to a visually decelerating
/// height so the slide feels natural instead of mechanical.
pub(super) fn ease_out_cubic(t: f32) -> f32 {
    let u = 1.0 - t;
    1.0 - u * u * u
}

/// Display name for a project directory in the sidebar tree: the last path
/// segment, falling back to the full path when it has no usable name.
pub(super) fn project_name(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(str::to_owned)
        .unwrap_or_else(|| path.display().to_string())
}

/// Path of the persisted chat transcript for one explicit project session.
///
/// Scoped by a stable hash of the project directory so switching projects
/// never mixes one project's on-screen conversation into another's.
pub(super) fn transcript_path(project_dir: &std::path::Path, session_id: &str) -> PathBuf {
    let proj_id = project_id_hash(project_dir);
    let filename = format!("{proj_id}-{session_id}.json");
    dirs::data_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("concerto")
        .join("sessions")
        .join(filename)
}

/// Path of the per-project persisted agent-graph state for `project_dir`,
/// scoped by session id (mirrors [`transcript_path`]).
pub(super) fn agent_graph_path(project_dir: &std::path::Path, session_id: &str) -> PathBuf {
    let proj_id = project_id_hash(project_dir);
    let filename = format!("{proj_id}-{session_id}-agent-graph.json");
    dirs::data_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("concerto")
        .join("sessions")
        .join(filename)
}

/// Convert a persisted session history (`Vec<core::Message>`) into chat
/// entries for display.
///
/// Thin delegate: the mapping (and its skip rules) live in
/// [`views::chat::entries_from_messages`], next to `ChatEntry`.
pub(super) fn messages_to_entries(
    history: Vec<concerto_core::types::Message>,
) -> Vec<views::chat::ChatEntry> {
    views::chat::entries_from(views::chat::ChatSource::Messages(history))
}

/// Map the durable typed transcript (ADR-36) onto chat entries for restore.
///
/// Thin delegate: the mapping lives in
/// [`views::chat::entries_from_transcript`], next to `ChatEntry`.
pub(super) fn transcript_to_entries(entries: Vec<TranscriptEntry>) -> Vec<views::chat::ChatEntry> {
    views::chat::entries_from(views::chat::ChatSource::Transcript(entries))
}

pub(super) fn configured_default_route(config: &AppConfig) -> (String, String) {
    if let Some(settings) = &config.model_settings {
        let assignment_default = settings.agent_assignments.iter().find_map(|assignment| {
            assignment
                .model_override
                .as_deref()
                .filter(|model| !model.trim().is_empty())
                .map(|model| (assignment.provider_config_id.as_str(), model))
        });
        let model = settings
            .global_default_model
            .as_deref()
            .filter(|model| !model.trim().is_empty())
            .or_else(|| {
                settings
                    .providers
                    .iter()
                    .map(|provider| provider.model.as_str())
                    .find(|model| !model.trim().is_empty())
            })
            .or_else(|| assignment_default.map(|(_, model)| model))
            .unwrap_or_default()
            .to_string();
        let provider = ProviderFactory::config_for_model(settings, &model, None)
            .or_else(|| {
                assignment_default.and_then(|(provider_id, _)| {
                    settings.providers.iter().find(|provider| provider.id == provider_id)
                })
            })
            .or_else(|| settings.providers.first());
        return (provider.map(ProviderFactory::config_id).unwrap_or_default(), model);
    }
    config
        .primary_provider_config
        .as_ref()
        .map(|provider| (ProviderFactory::config_id(provider), provider.model.clone()))
        .unwrap_or_default()
}

/// Slice 4a (spec §7): with `[orchestration]` present the blueprint's open
/// relationship registry (Studio → Relationships) governs hand-offs, so the
/// legacy Settings → Agent Relationship Manager is hidden in favor of the
/// Studio's relationship rows. Pure projection over the persisted surface —
/// deliberately a free function so the flag's plumbing is unit-testable with
/// plain `AppConfig` values, mirroring `configured_default_route`.
pub(super) fn orchestration_hides_relationships(config: &AppConfig) -> bool {
    config.orchestration.is_some()
}

/// Whether a provider is eligible for live model discovery: its type supports
/// it and any required credential is present.
///
/// Shared by startup auto-discovery and the save-triggered pass so the two
/// readiness gates can never drift. A provider type whose discovery is
/// unsupported, or which needs a credential that is not stored, is skipped.
pub(super) fn provider_discovery_ready(
    provider: &concerto_config::ProviderConfig,
    credentials: &CredentialStore,
) -> bool {
    let definition = provider_definition(&provider.provider);
    definition.supports_discovery()
        && (!definition.requires_credential()
            || provider.api_key(credentials).map(|key| !key.is_empty()).unwrap_or(false))
}

/// Load the persisted desktop theme from the `UserPrefsStore`, falling back
/// to Midnight when the store cannot be opened.
///
/// Prefs are the single source of truth for every desktop surface that
/// renders the theme. `config.display.theme` only bridges the same palettes
/// to the CLI and is never read back into the UI — it is written alongside
/// prefs by [`App::apply_and_save_theme`] and asserted on every Settings save.
pub(super) fn load_prefs_theme() -> AppTheme {
    let data_dir = dirs::data_dir()
        .map(|d| d.join("concerto"))
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let prefs_dir = data_dir.join("prefs");
    match concerto_memory::prefs::UserPrefsStore::open(&prefs_dir) {
        Ok(store) => crate::theme::prefs::load_theme(&store),
        Err(_) => AppTheme::by_name("Midnight"),
    }
}

// ---------------------------------------------------------------------------
// Theme apply pair + small theme/run getters
// ---------------------------------------------------------------------------

impl App {
    /// Apply the theme currently selected in Settings and persist it to both
    /// stores that hold it — the `UserPrefsStore` (theme name + font size)
    /// and `config.display.theme` (the CLI bridge) — in one step, without
    /// waiting for a `SaveSettings` round-trip. Keeps `current_theme`, the
    /// terminal palette, the Settings picker, and both stores in lockstep so
    /// a restart restores the chosen theme (the theme-persistence defect:
    /// `ThemeSelected` only mutated the editor's local selection and was lost
    /// unless Settings was saved).
    pub(super) fn apply_and_save_theme(&mut self) {
        let new_theme =
            AppTheme::by_name(self.settings.selected_theme).with_base_size(self.settings.font_size);
        let name_changed = new_theme.name != self.current_theme.name;
        self.current_theme = new_theme.clone();
        self.terminal.set_theme(&self.current_theme);
        // The picker is part of the applied state: re-seed it from what was
        // just applied so selection and live theme cannot drift apart.
        self.settings.seed_theme(new_theme.name, new_theme.font_stack.base_size);
        // Prefs are the single source the UI renders from; `display.theme` is
        // a read-through bridge for the CLI only, so it is asserted (memory +
        // disk, in one step) exactly when the applied theme's NAME changes —
        // a font-size tweak has no config key and must not rewrite the file.
        if name_changed {
            // Both in-memory copies are asserted together with the write, so
            // the merged config shown elsewhere in the app cannot keep the old
            // value. A project-layer `display.theme` override still wins on
            // the next reload (the file is truth, ADR-57 §6).
            let value = Some(new_theme.name.to_string());
            self.global_config.display.theme = value.clone();
            if let Some(config) = self.config.as_mut() {
                config.display.theme = value;
            }
            if let Some(path) = concerto_config::default_config_path() {
                if let Err(error) = concerto_config::save_config(&self.global_config, &path) {
                    tracing::error!(%error, "failed to persist the theme to the config");
                }
            }
        }
        let data_dir = dirs::data_dir()
            .map(|d| d.join("concerto"))
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        let prefs_dir = data_dir.join("prefs");
        if let Ok(store) = concerto_memory::prefs::UserPrefsStore::open(&prefs_dir) {
            crate::theme::prefs::save_theme(&store, &new_theme);
        }
    }

    /// Apply a theme loaded from the `UserPrefsStore` to every surface that
    /// renders it: the live theme, the terminal palette, and the Settings
    /// picker. Used by the external-prefs reload path (`Message::ThemeChanged`),
    /// where prefs — not config — are the source. Deliberately does not touch
    /// `config.display.theme`: an out-of-band prefs change must not rewrite
    /// the config file, and mutating only the in-memory copy would let the
    /// next config write persist a value that was never on disk.
    pub(super) fn apply_prefs_theme(&mut self, theme: AppTheme) {
        self.current_theme = theme.clone();
        self.terminal.set_theme(&self.current_theme);
        self.settings.seed_theme(theme.name, theme.font_stack.base_size);
    }

    pub fn theme(&self) -> iced::Theme {
        self.current_theme.iced.clone()
    }

    /// ADR-60 D7 (interrupt-safe resume): whether a run is in flight — the
    /// window-close handler cancels it and waits for settlement instead of
    /// exiting over it.
    pub fn is_run_active(&self) -> bool {
        self.run_status != RunStatus::Idle
    }

    /// ADR-60 D7 (interrupt-safe resume): the run-settlement epoch — bumped
    /// every time an in-flight run settles. The window-close handler polls
    /// this (bounded) so the coordinator's cancel path can persist the
    /// interrupted checkpoint before the process exits.
    pub fn run_settle_epoch(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.run_settle_epoch)
    }

    pub(super) fn note_run_settled(&mut self) {
        self.run_settle_epoch.fetch_add(1, Ordering::Release);
    }
}
