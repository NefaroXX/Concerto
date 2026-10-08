//! Shell-body view sections for [`App`] — one cluster extracted from
//! `app.rs` (NORM slice S29).
//!
//! This module owns the status/context/toast column (V2: the context bar
//! plus the optional toast bar stacked over the page content) and the
//! toggleable terminal bottom panel with its drag-resize handle (V3). The
//! bodies moved verbatim from [`App::view`]; the only edits are the
//! `pub(super)` annotations, the parameter plumbing (each fn takes the
//! Element it stacks instead of closing over a `view()` local), and the
//! status bar moving in from V2 — it is consumed only by the terminal
//! panel's two column arms. Both fns are pure `&self` builders: read-only
//! state + the theme only, no `Task`, no locks, no mutation. `App::view()`
//! keeps composition only; the modal/terminal tests in `app.rs`'s
//! `mod tests` stay put untouched.

use super::*;

// Explicit import outranks both the `use super::*;` glob and the std
// prelude's `column!` (Rust 1.96), which otherwise collide as ambiguous.
use iced::widget::column;

impl App {
    /// V2 — status/context/toast column: the context bar plus, while a
    /// toast is showing, the toast bar, over the page `content`, sized to
    /// fill the shell.
    pub(super) fn view_status_column<'a>(
        &'a self,
        content: Element<'a, Message>,
    ) -> Element<'a, Message> {
        let content_column =
            if let Some(toast_bar) = self.toasts.view::<Message>(&self.current_theme) {
                column![
                    views::context_bar::context_bar_view(self),
                    toast_bar,
                    container(content).width(Length::Fill).height(Length::Fill),
                ]
            } else {
                column![
                    views::context_bar::context_bar_view(self),
                    container(content).width(Length::Fill).height(Length::Fill),
                ]
            };
        content_column.into()
    }

    /// V3 — terminal bottom panel: wraps the status column (and its status
    /// bar) with the terminal, shown when the slide animation is mid-open
    /// or fully open.
    ///
    /// The terminal is a toggleable bottom panel with a drag resize handle,
    /// shown on top of the current page's content. The handle is a
    /// `mouse_area` (not a `button`): in iced 0.14 a button's `on_press`
    /// fires on *release*, while the mouse area fires on press-down, which
    /// is what a press-drag-release resize handle needs. The subscription
    /// tracks CursorMoved during the drag and ends it on any release.
    ///
    /// The panel's rendered height is the configured full height eased by
    /// `terminal_panel_anim`, so it slides up/down. It is rendered while
    /// `anim > 0.01` — including mid-close (open == false, anim still > 0)
    /// — so the closing panel slides down instead of vanishing.
    pub(super) fn view_terminal_panel<'a>(
        &'a self,
        content_column: Element<'a, Message>,
    ) -> Element<'a, Message> {
        let status_bar = views::status_bar::status_bar_view(self);

        let panel_height = self.terminal_panel_height * ease_out_cubic(self.terminal_panel_anim);
        let main_area = if self.terminal_panel_anim > 0.01 {
            let palette = &self.current_theme.palette;
            let resize_handle = mouse_area(
                container(text("⠿").size(10).color(palette.text_muted))
                    .width(Length::Fill)
                    .height(Length::Fixed(8.0))
                    .center_x(Length::Fill)
                    .center_y(Length::Fill),
            )
            .on_press(Message::TerminalPanelResizeStart)
            .on_release(Message::TerminalPanelResizeEnd);
            let terminal_area =
                container(self.terminal.view(&self.current_theme).map(Message::Terminal))
                    .width(Length::Fill)
                    .height(Length::Fixed(panel_height));
            // The main area must be an explicit `Fill` child of the shell row
            // so it takes exactly the leftover space between the fixed side
            // rails. Without it the row-sized column reports its natural
            // width and overflows behind the sidebar/quick panel on narrow
            // windows.
            column![content_column, resize_handle, terminal_area, status_bar].width(Length::Fill)
        } else {
            column![content_column, status_bar].width(Length::Fill)
        };
        main_area.into()
    }
}
