//! Reusable `EmptyState` widget for placeholder / empty views.
//!
//! Provides a consistent, visually clear empty state with an icon, title,
//! description, and optional action button. Every view that shows a
//! "no data yet" message should use this instead of ad‑hoc inline text.

use iced::widget::{button, column, container, text};
use iced::{Element, Length};

use crate::theme::AppTheme;

/// Render a centered empty-state placeholder.
///
/// * `icon` — Any `Element` rendered large (e.g. `text("◇")`, `text("☷")`).
/// * `title` — Brief heading in 18px text (e.g. "No messages yet").
/// * `description` — Explanatory body in 13px muted text.
/// * `action` — Optional primary action button (label + message).
///
/// # Example
///
/// ```ignore
/// use crate::ui::empty_state;
/// use iced::widget::text;
///
/// empty_state(
///     theme,
///     text("◇"),
///     "No messages yet",
///     "Type a message below to start a conversation.",
///     None::<(String, Message)>,  // or Some(("Get started".into(), Message::Start))
/// )
/// ```
pub fn empty_state<'a, Message>(
    theme: &'a AppTheme,
    icon: impl Into<Element<'a, Message>>,
    title: impl Into<String>,
    description: impl Into<String>,
    action: Option<(String, Message)>,
) -> Element<'a, Message>
where
    Message: Clone + 'a,
{
    let palette = &theme.palette;

    let mut col = column![
        container(icon).width(Length::Shrink),
        text(title.into()).size(18).color(palette.text),
        text(description.into()).size(13).color(palette.text_muted),
    ]
    .spacing(8)
    .align_x(iced::Alignment::Center);

    if let Some((label, msg)) = action {
        col = col.push(
            button(text(label).size(14))
                .style(crate::ui::button::primary)
                .on_press(msg)
                .padding([8, 16]),
        );
    }

    container(col)
        .width(Length::Fill)
        .height(Length::Fill)
        .center_x(Length::Fill)
        .center_y(Length::Fill)
        .into()
}

/// Compact variant — smaller icon and tighter spacing for panels / sidebars.
///
/// Same parameters as [`empty_state`] but uses 24px icon and reduced spacing.
pub fn empty_state_compact<'a, Message>(
    theme: &'a AppTheme,
    icon: impl Into<Element<'a, Message>>,
    title: impl Into<String>,
    description: impl Into<String>,
) -> Element<'a, Message>
where
    Message: Clone + 'a,
{
    let palette = &theme.palette;

    let col = column![
        container(icon).width(Length::Shrink),
        text(title.into()).size(14).color(palette.text),
        text(description.into()).size(11).color(palette.text_muted),
    ]
    .spacing(4)
    .align_x(iced::Alignment::Center);

    container(col)
        .width(Length::Fill)
        .height(Length::Fill)
        .center_x(Length::Fill)
        .center_y(Length::Fill)
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::AppTheme;

    #[derive(Debug, Clone)]
    enum TestMsg {
        Action,
    }

    /// `empty_state` renders without panicking (basic smoke).
    #[test]
    fn empty_state_renders() {
        let theme = AppTheme::by_name("Midnight");
        let _element = empty_state::<TestMsg>(
            &theme,
            text("☰"),
            "No data",
            "Nothing to see here yet.",
            None::<(String, TestMsg)>,
        );
    }

    /// `empty_state` with an action button renders without panicking.
    #[test]
    fn empty_state_with_action_renders() {
        let theme = AppTheme::by_name("Midnight");
        let _element = empty_state::<TestMsg>(
            &theme,
            text("☰"),
            "No data",
            "Nothing to see here yet.",
            Some(("Go".into(), TestMsg::Action)),
        );
    }

    /// `empty_state_compact` renders withoutsegfaulting.
    #[test]
    fn empty_state_compact_renders() {
        let theme = AppTheme::by_name("Midnight");
        let _element = empty_state_compact::<TestMsg>(&theme, text("☰"), "Empty", "No content.");
    }

    /// Works with all three themes.
    #[test]
    fn empty_state_all_themes() {
        for theme in AppTheme::all() {
            let _element = empty_state::<TestMsg>(
                &theme,
                text("☰"),
                "No data",
                "Nothing to see here yet.",
                None::<(String, TestMsg)>,
            );
        }
    }
}
