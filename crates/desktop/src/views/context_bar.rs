//! Top context bar: the Chat | Diff | Editor | Terminal view switcher.
//!
//! Project/session context lives in the left sidebar (project + session
//! tree), so this bar deliberately carries no breadcrumb — the shared
//! `ui::segmented` control is the only thing here, and it is the single place
//! primary view switching lives since the sidebar nav items were removed.

use iced::widget::container;
use iced::{Element, Length};

use crate::app::{App, Message, Page};
use crate::ui::{segmented, Segment};

/// Top context bar: centered view switcher.
pub fn context_bar_view(app: &App) -> Element<'_, Message> {
    let theme = &app.current_theme;
    let sp = &theme.spacing;

    let switcher = segmented(
        theme,
        &[
            Segment {
                label: "Chat",
                active: app.page == Page::Chat,
                on_press: Message::Navigate(Page::Chat),
            },
            Segment {
                label: "Diff",
                active: app.page == Page::DiffViewer,
                on_press: Message::Navigate(Page::DiffViewer),
            },
            Segment {
                label: "Editor",
                active: app.page == Page::Editor,
                on_press: Message::Navigate(Page::Editor),
            },
            Segment {
                label: "Terminal",
                active: app.terminal_panel_open,
                on_press: Message::ToggleTerminalPanel,
            },
        ],
    );

    container(switcher).width(Length::Fill).center_x(Length::Fill).padding([sp.sm, sp.lg]).into()
}
