//! Navigation and session-select `App::update()` arms for [`App`] — NORM
//! slice S41.
//!
//! This module owns two groups of the root update match, extracted from
//! `app.rs`:
//!
//! * [`App::update_navigation`] — the `Navigate` arm (page switch +
//!   `chat.finalize_streaming` + `TEXT_FOCUSED` clear off-Chat, the Settings
//!   relationship/provider config syncs + the one-time skills discovery
//!   `Task`, the Studio first-run seed + draft-preserving load + model sync +
//!   runtime-snapshot load), the `SetSubView` arm (chat sub-view + page +
//!   overlay-fade target + the Spend Log / Runtime modal reloads), and the
//!   `OpenSpendLog` one-liner re-dispatch.
//! * [`App::update_session_selected`] — the `SessionSelected` arm (active-
//!   graph flush, active-session id + checkpoint/spend reset, transcript /
//!   agent-graph / tool-log restore with the durable-event replay fallback,
//!   display-pref re-seed, and the final page switch).
//!
//! Bodies moved verbatim at the same 12-space arm indent, so each arm is
//! line-for-line with its origin; the only structural edit is each group
//! becoming one `pub(super)` method taking the full [`Message`] and
//! re-matching it (the same pattern as `views::settings::update_mcp` and
//! the sibling `simple_updates` / `project_switch` / `delegated_updates` /
//! `event_model_updates` / `view_dispatch` / `settings_updates` groups), so
//! every arm keeps its exact early-return `Task` semantics. The parent
//! `update` keeps one thin delegating arm per group; the fallback arm in
//! each method is unreachable through the parent dispatcher and is a
//! documented no-op. No `Message` / `App` shape change and no behavior
//! change: the tests in `app.rs`'s `mod tests` stay put and drive
//! `update()` unchanged.
//!
//! `SessionSelected` stayed in scope after the coupling check: it reaches
//! chat / tool-log / agent-graph / memory only through the shared `App`
//! fields and helpers (`State::from_entries`, the `route_event` replay,
//! `seed_muted_agents`) — the same seam `event_model_updates` already owns
//! for the `DesktopEvent` fan-out — so it is no more entangled than the
//! arms already extracted.

use super::*;

impl App {
    /// The navigation group (NORM S41): `Navigate`, `SetSubView`, and
    /// `OpenSpendLog`.
    ///
    /// The parent routes all three here. Page switches finalize streaming
    /// and clear the keyboard focus flag off-Chat, refresh the Settings
    /// config-derived lists (plus the one-time skills discovery `Task`) and
    /// the Studio roster/model/runtime state; sub-view switches set the
    /// chat sub-view, page, and overlay-fade target, reloading the Spend
    /// Log or Runtime modal contents on open. Returns the arm's `Task`
    /// unchanged; a message the parent never routes here is a documented
    /// no-op.
    pub(super) fn update_navigation(&mut self, message: Message) -> iced::Task<Message> {
        match message {
            Message::Navigate(page) => {
                self.page = page;
                if page != Page::Chat {
                    self.chat.finalize_streaming();
                    TEXT_FOCUSED.store(false, Ordering::Relaxed);
                }
                if page == Page::Settings {
                    // The relationship manager and provider list here are
                    // seeded from config at startup; the studio saves
                    // `multi_agent.relationships` and external config edits
                    // may change `model_settings.providers` independently, so
                    // refresh both on every entry. In-flight edits made here
                    // are preserved by the states themselves (ADR-57 §3d).
                    if let Some(config) = &self.config {
                        self.settings.sync_relationships_from_config(config);
                        self.settings.sync_providers_from_config(config);
                    }
                    // ADR-43 — one lazy skill discovery pass the first time the
                    // page opens. Discovery is blocking filesystem work, so it
                    // runs inside `Task::perform` (the same pattern as the
                    // shell profile test); the Refresh button re-runs it.
                    if self.settings.skills_never_discovered() {
                        return self.settings.start_skill_discovery().map(Message::Settings);
                    }
                }
                if page == Page::OrchestrationStudio {
                    // ADR-58/59 (rewritten) Slice 2 (first-run bootstrap), AMENDED
                    // (global-only orchestration): auto-seed the orchestration
                    // roster into the GLOBAL config before the Studio first
                    // renders, so the blueprint surface is active from the very
                    // first open — no splash, no manual init. Idempotent: a
                    // config that already owns its roster is never touched, and
                    // no project `.concerto.toml` is ever created here.
                    self.ensure_orchestration_seeded();
                    // Do not replace an in-progress Studio draft when the user
                    // briefly visits another page. Saved state may be reloaded
                    // so changes made elsewhere (for example in Settings) are
                    // reflected when Studio is opened again.
                    if !self.orchestration_studio.unsaved {
                        if let Some(config) = &self.config {
                            self.orchestration_studio.load_from_config(config);
                        }
                    }
                    // Model cache is always refreshed — it is non-destructive
                    // (only updates dropdown options, not agent assignments).
                    self.orchestration_studio
                        .sync_models(self.settings.cached_models_by_provider());
                    // Read-only Coordinator 2.0 observability: refresh the
                    // runtime snapshot from the active session's persisted
                    // checkpoint. No polling — Studio open and the Runtime
                    // modal (Ctrl+R) are the only triggers.
                    return self.load_studio_runtime();
                }
                iced::Task::none()
            }
            Message::SetSubView(sub_view) => {
                self.chat.sub_view = sub_view;
                self.page = Page::Chat;
                // Start (or continue) the overlay fade. Opening targets 1.0
                // without resetting the current alpha, so re-opening mid-fade
                // resumes from where the backdrop is; closing targets 0.0 and
                // the tick ramps the dim layer out over the base.
                self.overlay_fading = true;
                self.overlay_fade_target =
                    if sub_view == views::chat::SubView::Main { 0.0 } else { 1.0 };
                // Opening the Spend Log loads the active session's records so
                // the modal body is fresh (idempotent re-open).
                if sub_view == views::chat::SubView::SpendLog {
                    return self.load_spend_log();
                }
                // Opening the Runtime modal refreshes the active session's
                // read-only observability snapshot through the existing
                // checkpoint reader (idempotent re-open).
                if sub_view == views::chat::SubView::Runtime {
                    return self.load_studio_runtime();
                }
                iced::Task::none()
            }
            Message::OpenSpendLog => {
                self.update(Message::SetSubView(views::chat::SubView::SpendLog))
            }
            // Never routed by the parent dispatcher; documented no-op.
            _ => iced::Task::none(),
        }
    }

    /// The `SessionSelected` arm (NORM S41): session switch / restore.
    ///
    /// The parent routes `SessionSelected` here. Flushes the previously
    /// active agent graph, swaps the active session id, resets checkpoint /
    /// spend state, restores the chat / agent-graph / tool-log view states
    /// from their sidecars (replaying the durable event sequence when a
    /// sidecar is missing), re-applies the display prefs, and lands on the
    /// Chat page. Returns the arm's `Task` unchanged; a message the parent
    /// never routes here is a documented no-op.
    pub(super) fn update_session_selected(&mut self, message: Message) -> iced::Task<Message> {
        match message {
            Message::SessionSelected { session_id, history, events, transcript } => {
                // Flush the previously active session before switching.
                self.persist_active_agent_graph();
                self.active_session_id = Ulid::from_string(&session_id).ok();
                self.resume_checkpoint_json = None;
                // A resumed session starts with a fresh spend chip + log; its
                // records (if any) are re-loaded when the Spend Log opens.
                self.reset_spend_state();
                let rich_transcript = transcript_path(&self.project_dir, &session_id);
                let persisted_entries = views::chat::State::load_entries(&rich_transcript);
                // Restore priority (ADR-36 §5): the durable DB transcript is
                // canonical; fall back to the local transcript.json cache, and
                // only then to the degraded messages-only view (legacy
                // sessions predate the typed transcript).
                let transcript_entries = transcript_to_entries(transcript);
                let transcript_missing = transcript_entries.is_empty();
                self.chat = views::chat::State::from_entries(if transcript_missing {
                    persisted_entries.clone().unwrap_or_else(|| messages_to_entries(history))
                } else {
                    transcript_entries
                });
                let persisted_graph = self.active_session_id.and_then(|id| {
                    views::agent_graph::State::load_from(agent_graph_path(
                        &self.project_dir,
                        &id.to_string(),
                    ))
                });
                let graph_missing = persisted_graph.is_none();
                self.agent_graph = persisted_graph.unwrap_or_default();
                self.tool_log = views::tool_log::State::new();
                if (transcript_missing && persisted_entries.is_none()) || graph_missing {
                    // A crash can occur before the UI sidecars are written.
                    // Rebuild visible activity and graph state from the
                    // authoritative durable event sequence in that case.
                    let mut replay_chat = views::chat::State::new();
                    let mut replay_tool_log = views::tool_log::State::new();
                    let mut replay_graph = views::agent_graph::State::new();
                    for stored in &events {
                        let Ok(event) = stored.to_event() else { continue };
                        let Some(event) = crate::runtime::translate_event(&event) else {
                            continue;
                        };
                        crate::runtime::route_event(
                            &event,
                            &mut replay_chat,
                            &mut replay_tool_log,
                            &mut replay_graph,
                            &mut self.memory,
                        );
                    }
                    if transcript_missing && persisted_entries.is_none() {
                        self.chat = replay_chat;
                    }
                    if graph_missing {
                        self.agent_graph = replay_graph;
                    }
                    self.tool_log = replay_tool_log;
                } else {
                    self.tool_log.load_stored_events(&events);
                }
                // Restored and replayed entries normalize on read; re-apply the
                // display prefs on top — the mute filter from config plus the
                // reduced-motion override (a replaced chat otherwise reverts
                // to the factory default and silently re-enables animations).
                self.seed_muted_agents();
                self.chat.set_reduced_motion(self.reduced_motion);
                self.page = Page::Chat;
                iced::Task::none()
            }
            // Never routed by the parent dispatcher; documented no-op.
            _ => iced::Task::none(),
        }
    }
}
