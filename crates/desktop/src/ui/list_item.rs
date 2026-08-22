//! Uniform list-item button with a leading accent bar for active/selected rows.

use iced::widget::{button, container, row};
use iced::{Alignment, Background, Border, Element, Length};

use crate::theme::AppTheme;

/// Uniform list-item button: consistent padding, 8px radius, and a purple
/// left accent bar + slightly elevated surface when `active` (accent is used
/// ONLY for active/selected states). Inactive: transparent, hover = surface_variant + border.
/// Callers color the text content themselves (e.g. `crate::theme::sidebar_item_style(palette, active)`).
pub fn list_item<'a, Message: 'a + Clone>(
    theme: &'a AppTheme,
    active: bool,
    on_press: Message,
    content: impl Into<Element<'a, Message>>,
) -> Element<'a, Message> {
    let palette = &theme.palette;
    let content: Element<'a, Message> = content.into();

    // 3px-wide accent bar, kept at constant height so text never shifts.
    let accent_bar = container(iced::widget::Space::new()).width(3).height(14).style(
        move |_theme: &iced::Theme| container::Style {
            background: Some(Background::Color(if active {
                palette.accent
            } else {
                iced::Color { a: 0.0, ..palette.accent }
            })),
            border: Border { radius: 2.0.into(), ..Default::default() },
            ..container::Style::default()
        },
    );

    let body = row![accent_bar, content].spacing(10).align_y(Alignment::Center);

    let btn_style = move |_theme: &iced::Theme, status: button::Status| {
        if active {
            button::Style {
                background: Some(Background::Color(iced::Color { a: 0.12, ..palette.accent })),
                border: Border { radius: 8.0.into(), ..Default::default() },
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
            button::Style { background: bg, border, ..button::Style::default() }
        }
    };

    button(body).style(btn_style).on_press(on_press).width(Length::Fill).padding([8, 12]).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum TestMsg {
        Clicked,
    }

    #[test]
    fn list_item_renders_active() {
        let theme = AppTheme::by_name("Midnight");
        let _el: iced::Element<'_, TestMsg> =
            list_item(&theme, true, TestMsg::Clicked, iced::widget::text("Chat"));
    }

    #[test]
    fn list_item_renders_inactive() {
        let theme = AppTheme::by_name("Midnight");
        let _el: iced::Element<'_, TestMsg> =
            list_item(&theme, false, TestMsg::Clicked, iced::widget::text("Settings"));
    }

    #[test]
    fn list_item_renders_all_themes() {
        for name in ["Midnight", "Slate", "Chalk", "Nebula"] {
            let theme = AppTheme::by_name(name);
            let _el: iced::Element<'_, TestMsg> =
                list_item(&theme, true, TestMsg::Clicked, iced::widget::text("Item"));
            let _el: iced::Element<'_, TestMsg> =
                list_item(&theme, false, TestMsg::Clicked, iced::widget::text("Item"));
        }
    }
}
