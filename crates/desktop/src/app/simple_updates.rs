//! Trivial `App::update()` arm groups for [`App`] — one cluster extracted
//! from `app.rs` (NORM slice S30).
//!
//! This module owns four trivial arm groups of the root update match: the
//! run-cancel + shared animation ticks (cancel, circuit, scanline, overlay
//! fade / terminal slide), the `AgentGraph`/`Terminal` child-view
//! passthroughs, the prefs-theme reload + help-overlay flip, and the
//! screenshot / save-feedback / toast / quick-panel / memory-modal /
//! terminal-panel / git-summary tail that closes the match. The bodies
//! moved verbatim from `app.rs` — at the same 12-space arm indent, so each
//! copy is line-for-line — and the only structural edit is each group
//! becoming one `pub(super)` method that takes the full [`Message`] and
//! re-matches it (the same pattern as `views::settings::update_mcp`), so
//! every arm keeps its exact early-return `Task` semantics. The fallback
//! arm in each method is unreachable through the parent dispatcher and is a
//! documented no-op. The parent `update` keeps one thin delegating arm per
//! group; the help / screenshot / save-feedback / terminal / anim / toast
//! tests in `app.rs`'s `mod tests` drive `update()` unchanged.

use super::*;

impl App {
    /// Run cancel + the three shared animation ticks (NORM S30 group 1).
    ///
    /// The parent routes `CancelAgentRun`, `CircuitTick`, `ScanlineTick`,
    /// and `AnimTick` here. Cancel fires the run's `CancellationToken` and
    /// flips `run_status` to `Cancelling` (only while a run is `Running`);
    /// the ticks advance the circuit/scanline background progress and ease
    /// the overlay fade + terminal-panel slide toward their targets.
    /// Returns the arm's `Task` unchanged; a message the parent never
    /// routes here is a documented no-op.
    pub(super) fn update_cancel_and_ticks(&mut self, message: Message) -> iced::Task<Message> {
        match message {
            Message::CancelAgentRun => {
                if self.run_status == RunStatus::Running {
                    self.cancel_token.cancel();
                    self.run_status = RunStatus::Cancelling;
                }
                iced::Task::none()
            }
            Message::CircuitTick => {
                self.circuit_progress =
                    (self.circuit_progress + circuit_background::PROGRESS_STEP) % 1.0;
                iced::Task::none()
            }
            Message::ScanlineTick => {
                self.scanline_progress =
                    (self.scanline_progress + scanline_overlay::PROGRESS_STEP) % 1.0;
                iced::Task::none()
            }
            Message::AnimTick => {
                // Overlay backdrop fade: advance toward the target in fixed
                // 0.08 steps, snapping on the final step (~15 ticks ≈ 240 ms
                // for a full fade). Clears the flag when settled, which also
                // stops the shared animation subscription.
                if self.overlay_fading {
                    let target = self.overlay_fade_target;
                    if (target - self.overlay_fade).abs() <= 0.08 {
                        self.overlay_fade = target;
                        self.overlay_fading = false;
                    } else {
                        self.overlay_fade += (target - self.overlay_fade).signum() * 0.08;
                    }
                }
                // Terminal panel slide: animate toward open (1.0) / closed
                // (0.0) from the `terminal_panel_open` flag.
                if self.terminal_panel_animating {
                    let target = if self.terminal_panel_open { 1.0 } else { 0.0 };
                    if (target - self.terminal_panel_anim).abs() <= 0.08 {
                        self.terminal_panel_anim = target;
                        self.terminal_panel_animating = false;
                    } else {
                        self.terminal_panel_anim +=
                            (target - self.terminal_panel_anim).signum() * 0.08;
                    }
                }
                iced::Task::none()
            }
            // Never routed by the parent dispatcher; documented no-op.
            _ => iced::Task::none(),
        }
    }

    /// `AgentGraph`/`Terminal` child-view passthroughs (NORM S30 group 2).
    ///
    /// Pure forwards: each child state's own `update` runs and its `Task`
    /// is mapped back into the parent `Message` space. Terminal also
    /// re-passes the current theme so the shell styles the event.
    /// Returns the arm's `Task` unchanged; a message the parent never
    /// routes here is a documented no-op.
    pub(super) fn update_passthrough_views(&mut self, message: Message) -> iced::Task<Message> {
        match message {
            Message::AgentGraph(msg) => self.agent_graph.update(msg).map(Message::AgentGraph),
            Message::Terminal(msg) => {
                self.terminal.update(msg, &self.current_theme).map(Message::Terminal)
            }
            // Never routed by the parent dispatcher; documented no-op.
            _ => iced::Task::none(),
        }
    }

    /// External-prefs theme reload + the shortcuts-help flip (NORM S30
    /// group 3).
    ///
    /// `ThemeChanged` re-reads the single source (prefs) and re-applies it
    /// to every surface via `apply_prefs_theme`; `HelpToggled` flips the
    /// help overlay. Returns the arm's `Task` unchanged; a message the
    /// parent never routes here is a documented no-op.
    pub(super) fn update_theme_and_help(&mut self, message: Message) -> iced::Task<Message> {
        match message {
            Message::ThemeChanged => {
                // Re-read the single source (prefs) and re-apply it to every
                // surface, including the Settings picker — a prefs reload that
                // only replaced `current_theme` would leave the picker showing
                // a theme the shell is not rendering.
                let theme = load_prefs_theme();
                self.apply_prefs_theme(theme);
                iced::Task::none()
            }
            Message::HelpToggled => {
                self.show_help = !self.show_help;
                iced::Task::none()
            }
            // Never routed by the parent dispatcher; documented no-op.
            _ => iced::Task::none(),
        }
    }

    /// The trivial tail cluster that closes the parent match (NORM S30
    /// group 4): screenshot capture/status, save-feedback clearing, toast
    /// dismiss/expiry, the quick panel, the memory modal/graph, the
    /// terminal panel (toggle + drag resize), and the loaded git summary.
    ///
    /// Each arm returns its `Task` unchanged — `Task::none()`, an existing
    /// loader call (`load_memory_entries`, `load_memory_graph`,
    /// `load_git_summary`, `terminal.ensure_started`), or the arm's own
    /// small `Task` (screenshot capture, 5-second status clear). Returns
    /// the arm's `Task` unchanged; a message the parent never routes here
    /// is a documented no-op.
    pub(super) fn update_trivial_ui_state(&mut self, message: Message) -> iced::Task<Message> {
        match message {
            Message::TakeScreenshot => {
                self.screenshot_status = Some("Capturing...".to_string());
                iced::window::latest().and_then(|id| {
                    iced::window::screenshot(id).map(|screenshot| {
                        let rgba: &[u8] = screenshot.as_ref();
                        let w = screenshot.size.width;
                        let h = screenshot.size.height;
                        match crate::services::screenshot::save_png(rgba, w, h, true) {
                            Ok(res) => Message::ScreenshotCompleted(Ok(res)),
                            Err(e) => Message::ScreenshotCompleted(Err(e.to_string())),
                        }
                    })
                })
            }
            Message::ScreenshotCompleted(result) => {
                match result {
                    Ok(res) => {
                        let path_str = res.file_path.display().to_string();
                        tracing::info!(path = %path_str, "Screenshot saved");
                        self.screenshot_status = Some(format!("Saved: {}", path_str));
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "Screenshot failed");
                        self.screenshot_status = Some(format!("Failed: {}", e));
                    }
                }
                // Clear status after 5 seconds
                let status = self.screenshot_status.clone();
                iced::Task::perform(
                    async move {
                        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                        status
                    },
                    |_| Message::ClearScreenshotStatus,
                )
            }
            Message::ClearScreenshotStatus => {
                self.screenshot_status = None;
                iced::Task::none()
            }
            Message::ClearSaveFeedback(generation) => {
                // Ignore stale timers from a previous save: only the current
                // generation may clear the notice.
                if generation == self.save_feedback_generation {
                    self.save_feedback = None;
                }
                iced::Task::none()
            }
            Message::ToastDismissed(id) => {
                self.toasts.dismiss(id);
                iced::Task::none()
            }
            Message::ToastExpiryTick => {
                let cutoff =
                    std::time::Instant::now() - std::time::Duration::from_secs(TOAST_LIFETIME_SECS);
                self.toasts.prune_older_than(cutoff);
                iced::Task::none()
            }
            Message::ToggleQuickPanel => {
                self.quick_panel_open = !self.quick_panel_open;
                if self.quick_panel_open {
                    // Opening the panel also (re)loads memory entries so the
                    // Memory section is fresh; the git summary is loaded
                    // alongside it.
                    if self.config.as_ref().is_some_and(|c| c.memory.enabled) {
                        iced::Task::batch(vec![self.load_memory_entries(), self.load_git_summary()])
                    } else {
                        self.load_git_summary()
                    }
                } else {
                    iced::Task::none()
                }
            }
            Message::OpenMemoryModal => {
                self.memory_view_open = true;
                self.load_memory_entries()
            }
            Message::CloseMemoryModal => {
                self.memory_view_open = false;
                iced::Task::none()
            }
            Message::OpenMemoryGraph => {
                self.memory_graph_open = true;
                self.memory_graph = views::memory_graph::State::Loading;
                self.load_memory_graph()
            }
            Message::CloseMemoryGraph => {
                self.memory_graph_open = false;
                iced::Task::none()
            }
            Message::MemoryGraph(msg) => match msg {
                views::memory_graph::Message::Refresh => {
                    self.memory_graph = views::memory_graph::State::Loading;
                    self.load_memory_graph()
                }
            },
            Message::MemoryGraphLoaded(result) => {
                match result {
                    Ok(graph) => self.memory_graph = views::memory_graph::State::Loaded(graph),
                    Err(error) => self.memory_graph = views::memory_graph::State::Error(error),
                }
                iced::Task::none()
            }
            Message::ToggleTerminalPanel => {
                self.terminal_panel_open = !self.terminal_panel_open;
                // Kick off the slide animation; `AnimTick` eases the panel
                // height toward the new `terminal_panel_open` target.
                self.terminal_panel_animating = true;
                if self.terminal_panel_open {
                    // Opening the panel lazily starts the shell (and re-focuses
                    // it when the panel is already running).
                    self.terminal.ensure_started(&self.current_theme).map(Message::Terminal)
                } else {
                    iced::Task::none()
                }
            }
            Message::TerminalPanelResizeStart => {
                if self.terminal_resizing {
                    return iced::Task::none();
                }
                self.terminal_resizing = true;
                self.terminal_start_height = self.terminal_panel_height;
                self.terminal_drag_origin = None;
                iced::Task::none()
            }
            Message::TerminalPanelResizeMoved(y) => {
                if !self.terminal_resizing {
                    return iced::Task::none();
                }
                if self.terminal_drag_origin.is_none() {
                    self.terminal_drag_origin = Some(y);
                    return iced::Task::none();
                }
                let origin = self.terminal_drag_origin.unwrap_or(y);
                self.terminal_panel_height =
                    (self.terminal_start_height + (origin - y)).clamp(120.0, 600.0);
                iced::Task::none()
            }
            Message::TerminalPanelResizeEnd => {
                self.terminal_resizing = false;
                self.terminal_drag_origin = None;
                iced::Task::none()
            }
            Message::GitSummaryLoaded(summary) => {
                self.git_summary = summary;
                iced::Task::none()
            }
            // Never routed by the parent dispatcher; documented no-op.
            _ => iced::Task::none(),
        }
    }
}
