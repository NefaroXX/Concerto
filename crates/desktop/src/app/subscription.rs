//! Subscription wiring for [`App`] — one cluster extracted from `app.rs`
//! (NORM slice 13).
//!
//! This module owns the root `Subscription` builder: every passive event
//! stream the desktop app listens to (keyboard shortcuts, event-bus relay,
//! terminal panel, drag tracking, animation ticks, toast expiry, config
//! watch). The body is byte-identical to the original inline implementation
//! in `app.rs`; only its location moved. No `State`/`Message` shape, update
//! routing, or view tree change.

use iced::keyboard;
use iced::Subscription;

use super::*;

/// Cadence of the blinking cursor on the live streaming assistant entry.
/// The subscription that drives it exists only while a run is streaming, so
/// this costs nothing at idle.
const STREAMING_CURSOR_PERIOD_MS: u64 = 500;

impl App {
    /// Assemble every passive event stream the desktop app subscribes to.
    ///
    /// Returned to iced; each sub-stream maps into a [`Message`] so `update`
    /// stays the single routing point. Streams with nothing to do (idle
    /// toasts, no run in flight, overlay disabled) collapse to
    /// `Subscription::none()` and cost nothing.
    pub fn subscription(&self) -> Subscription<Message> {
        let terminal_active = self.terminal_panel_open;
        let keyboard_sub =
            keyboard::listen().with(terminal_active).filter_map(|(terminal_active, event)| {
                // Extract key and modifiers from the keyboard event
                match event {
                    iced::keyboard::Event::KeyPressed { key, modifiers, .. } => {
                        let shortcut = if terminal_active {
                            shortcuts::resolve_terminal(&key, modifiers)
                        } else {
                            shortcuts::resolve(
                                &key,
                                modifiers,
                                TEXT_FOCUSED.load(Ordering::Relaxed),
                            )
                        };
                        shortcut.map(Message::Shortcut)
                    }
                    _ => None,
                }
            });
        let bus = self.bus.clone();
        let bus_sub = Subscription::run_with(bus, |bus| {
            let bus = bus.clone();
            // Keep a single receiver across unfold iterations so events
            // published between yields are not dropped. A fresh `subscribe()`
            // per iteration would miss everything published in that gap,
            // causing the UI to show a tool as "running" forever or to drop
            // tool-completion events under bursty agent output.
            futures::stream::unfold((bus, None), |(bus, rx_opt)| async move {
                let mut rx = rx_opt.unwrap_or_else(|| bus.subscribe());
                loop {
                    match rx.recv().await {
                        Ok(evt) => {
                            let desktop_evt = crate::runtime::translate_event(&evt);
                            if let Some(evt) = desktop_evt {
                                return Some((evt, (bus, Some(rx))));
                            }
                        }
                        Err(_) => return None,
                    }
                }
            })
        })
        .map(Message::DesktopEvent);
        let terminal_sub = self.terminal.subscription().map(Message::Terminal);
        // While a terminal-panel drag is in progress, stream cursor moves to
        // update the panel height and end the drag on any release (or any
        // other event — a safety net so the drag can never get stuck). Uses
        // `iced::event::listen` because `iced::mouse::listen` does not exist in
        // iced 0.14; `iced::Event::Mouse(mouse::Event::CursorMoved { position })`
        // carries the cursor position in logical points.
        let drag_sub = if self.terminal_resizing {
            iced::event::listen().map(|event| match event {
                iced::Event::Mouse(iced::mouse::Event::CursorMoved { position }) => {
                    Message::TerminalPanelResizeMoved(position.y)
                }
                iced::Event::Mouse(iced::mouse::Event::ButtonReleased(_)) => {
                    Message::TerminalPanelResizeEnd
                }
                _ => Message::TerminalPanelResizeEnd,
            })
        } else {
            Subscription::none()
        };
        // Only ticks while an agent run is actually in flight, so the
        // ambient background costs nothing the rest of the time.
        let circuit_sub = if self.run_status == RunStatus::Running {
            iced::time::every(std::time::Duration::from_millis(circuit_background::TICK_MS))
                .map(|_| Message::CircuitTick)
        } else {
            Subscription::none()
        };
        // Scanline overlay tick: active when the default-off flag is enabled
        // and the Chat page is showing. Advances the breathing phase so
        // the overlay pulses; streaming doubles the effective rate inside
        // the widget. Costs nothing when disabled.
        let scanline_sub = if self.scanline_overlay_enabled && self.page == Page::Chat {
            iced::time::every(std::time::Duration::from_millis(scanline_overlay::TICK_MS))
                .map(|_| Message::ScanlineTick)
        } else {
            Subscription::none()
        };
        // One shared tick drives both the overlay backdrop fade and the
        // terminal panel slide. Active only while at least one animation is
        // in flight, so it costs nothing the rest of the time.
        let anim_sub = if self.overlay_fading || self.terminal_panel_animating {
            iced::time::every(std::time::Duration::from_millis(circuit_background::TICK_MS))
                .map(|_| Message::AnimTick)
        } else {
            Subscription::none()
        };
        // Blinking cursor on the streaming assistant entry — active only while
        // the run is actually streaming text and reduced-motion is off, so it
        // costs nothing at idle (and reduced-motion renders no blink).
        let blink_sub = if self.chat.is_streaming() && !self.reduced_motion {
            iced::time::every(std::time::Duration::from_millis(STREAMING_CURSOR_PERIOD_MS))
                .map(|_| Message::Chat(views::chat::Message::StreamingTick))
        } else {
            Subscription::none()
        };
        // One shared 16 ms tick drives every chat animation — the assistant
        // typewriter reveal, entrance fades, the open-thinking shimmer and the
        // "Composing…" row. Active while a reveal is in flight OR any run is in
        // progress (`run_status == Running`), so the composing indicator keeps
        // animating even when the busy work has no reveal/entrance state of its
        // own (e.g. a coordinator thinking phase before any subagent dispatch).
        let typing_sub = if self.chat.is_revealing() || self.run_status == RunStatus::Running {
            iced::time::every(std::time::Duration::from_millis(circuit_background::TICK_MS))
                .map(|_| Message::Chat(views::chat::Message::TypingTick))
        } else {
            Subscription::none()
        };
        // Ticks every second while any toast is showing so stale toasts
        // auto-dismiss after `TOAST_LIFETIME_SECS`. Inactive when idle.
        let toast_sub = if self.toasts.has_toasts() {
            iced::time::every(std::time::Duration::from_secs(1)).map(|_| Message::ToastExpiryTick)
        } else {
            Subscription::none()
        };
        // Config file watcher (ADR-57 §7): emits once per debounced batch of
        // edits to the global config or the active project's config, so
        // external edits reach the next run without a restart. The identity is
        // the project dir — switching projects re-arms the watched path set.
        let config_watch_sub = {
            let project_dir = self.project_dir.clone();
            Subscription::run_with(project_dir, |project_dir| {
                futures::stream::unfold(
                    crate::config_watch::ConfigWatch::start(project_dir.clone()),
                    |mut watch| async move {
                        watch.recv().await.map(|_| (Message::ConfigReloaded, watch))
                    },
                )
            })
        };
        iced::Subscription::batch(vec![
            keyboard_sub,
            bus_sub,
            terminal_sub,
            drag_sub,
            circuit_sub,
            scanline_sub,
            anim_sub,
            blink_sub,
            typing_sub,
            toast_sub,
            config_watch_sub,
        ])
    }
}
