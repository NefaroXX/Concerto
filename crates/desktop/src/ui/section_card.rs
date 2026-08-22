use iced::widget::{column, container, text};
use iced::{Background, Border, Element, Length};

use crate::theme::AppTheme;

/// A consistent card container with title and content.
///
/// Renders a rounded container with `surface_variant` background,
/// a title at 16px, and the content below with standard card padding.
pub fn section_card<'a, Message: 'a + Clone>(
    theme: &'a AppTheme,
    title: impl Into<String>,
    content: impl Into<Element<'a, Message>>,
) -> Element<'a, Message> {
    let palette = &theme.palette;
    let title_text = text(title.into()).size(16).color(palette.text);

    let body = column![title_text, content.into()].spacing(12);

    container(body)
        .width(Length::Fill)
        .padding(16)
        .style(move |_theme: &iced::Theme| container::Style {
            background: Some(Background::Color(palette.surface_variant)),
            border: Border { color: palette.border, width: 1.0, radius: 12.0.into() },
            ..container::Style::default()
        })
        .into()
}

/// A card with an additional subtitle in muted text below the title.
pub fn section_card_with_subtitle<'a, Message: 'a + Clone>(
    theme: &'a AppTheme,
    title: impl Into<String>,
    subtitle: impl Into<String>,
    content: impl Into<Element<'a, Message>>,
) -> Element<'a, Message> {
    let palette = &theme.palette;
    let title_text = text(title.into()).size(16).color(palette.text);
    let subtitle_text = text(subtitle.into()).size(12).color(palette.text_muted);

    let header = column![title_text, subtitle_text].spacing(2);
    let body = column![header, content.into()].spacing(12);

    container(body)
        .width(Length::Fill)
        .padding(16)
        .style(move |_theme: &iced::Theme| container::Style {
            background: Some(Background::Color(palette.surface_variant)),
            border: Border { color: palette.border, width: 1.0, radius: 12.0.into() },
            ..container::Style::default()
        })
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use iced::widget::text;

    /// Smoke test: section_card renders without panicking with the default theme.
    #[test]
    fn section_card_renders() {
        let theme = AppTheme::by_name("Midnight");
        let _card: iced::Element<'_, ()> = section_card(&theme, "Test Title", text("Test content"));
    }

    /// Smoke test: section_card_with_subtitle renders without panicking.
    #[test]
    fn section_card_with_subtitle_renders() {
        let theme = AppTheme::by_name("Midnight");
        let _card: iced::Element<'_, ()> =
            section_card_with_subtitle(&theme, "Test Title", "Test subtitle", text("Test content"));
    }
}
