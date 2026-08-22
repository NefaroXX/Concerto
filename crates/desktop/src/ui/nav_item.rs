//! Enhanced navigation item with icon, hover, active glow, and consistent depth.

use iced::widget::{button, row, text};
use iced::{Background, Border, Element, Length, Shadow, Vector};

use crate::theme::AppTheme;

/// Render a sidebar navigation item with optional icon, active glow, and hover lift.
///
/// * `theme` — Current app theme for palette colors
/// * `icon` — Optional geometric icon string
/// * `label` — Item label text
/// * `is_active` — Whether this is the currently selected page
/// * `on_press` — Message to emit on click
pub fn nav_item<'a, Message: 'a + Clone>(
    theme: &'a AppTheme,
    icon: Option<&'a str>,
    label: &'a str,
    is_active: bool,
    on_press: Message,
) -> Element<'a, Message> {
    let palette = &theme.palette;

    let content: Element<'a, Message> = if let Some(icon_str) = icon {
        row![
            text(icon_str).size(15).style(move |_| {
                iced::widget::text::Style {
                    color: Some(if is_active { palette.accent } else { palette.text_muted }),
                }
            }),
            text(label)
                .size(15)
                .style(move |_| { crate::theme::sidebar_item_style(palette, is_active) }),
        ]
        .spacing(10)
        .align_y(iced::Alignment::Center)
        .into()
    } else {
        text(label)
            .size(15)
            .style(move |_| crate::theme::sidebar_item_style(palette, is_active))
            .into()
    };

    let btn_style = move |_theme: &iced::Theme, status: button::Status| {
        if is_active {
            button::Style {
                background: Some(Background::Color(iced::Color { a: 0.12, ..palette.accent })),
                text_color: palette.accent,
                border: Border { radius: 8.0.into(), ..Default::default() },
                shadow: Shadow {
                    color: iced::Color { a: 0.15, ..palette.accent },
                    offset: Vector::new(0.0, 0.0),
                    blur_radius: 8.0,
                },
                ..button::Style::default()
            }
        } else {
            let bg = match status {
                button::Status::Hovered => Some(Background::Color(palette.surface_variant)),
                _ => None,
            };
            let border = match status {
                button::Status::Hovered => {
                    Border { radius: 8.0.into(), color: palette.border, width: 1.0 }
                }
                _ => Border { radius: 8.0.into(), ..Default::default() },
            };
            button::Style {
                background: bg,
                text_color: palette.text_muted,
                border,
                ..button::Style::default()
            }
        }
    };

    button(content).style(btn_style).on_press(on_press).width(Length::Fill).padding([7, 14]).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum TestMsg {
        Clicked,
    }

    #[test]
    fn nav_item_renders_active() {
        let theme = AppTheme::by_name("Midnight");
        let _el: iced::Element<'_, TestMsg> =
            nav_item(&theme, Some(">"), "Chat", true, TestMsg::Clicked);
    }

    #[test]
    fn nav_item_renders_inactive() {
        let theme = AppTheme::by_name("Midnight");
        let _el: iced::Element<'_, TestMsg> =
            nav_item(&theme, None, "Settings", false, TestMsg::Clicked);
    }
}
