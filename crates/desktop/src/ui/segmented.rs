//! Uniform segmented control for view switching (e.g. Chat | Diff | Terminal).

use iced::widget::{button, container, row, text};
use iced::{Background, Border, Element};

use crate::theme::AppTheme;

/// One segment of a segmented control.
///
/// `label` is `&'static str` so callers can build segment arrays as view-local
/// values: the slice only needs to live for the duration of the call, while
/// the returned element borrows just the theme.
#[derive(Debug, Clone)]
pub struct Segment<Message> {
    pub label: &'static str,
    /// Whether this segment is the currently selected one (filled accent).
    pub active: bool,
    /// Message emitted on press.
    pub on_press: Message,
}

/// Uniform segmented control (e.g. view switching Chat | Diff | Terminal).
/// Track: palette.background container, radius 8, padding 3. Each segment is a
/// button with radius 8, padding [6, 14], text 13px (theme.type_scale.body).
/// Active segment: background = palette.accent (filled purple), text = palette.primary_text.
/// Inactive segment: transparent background, text = palette.text, hover = palette.surface_variant.
/// The accent color is used ONLY for the active segment.
pub fn segmented<'a, Message: 'a + Clone>(
    theme: &'a AppTheme,
    segments: &[Segment<Message>],
) -> Element<'a, Message> {
    let palette = &theme.palette;

    let segment_row = row(segments.iter().map(|seg| {
        // Copy the per-segment values first so the style closures below do not
        // capture a reference into `segments`: the returned element must only
        // borrow the theme, not the caller's segment array.
        let seg_active = seg.active;
        let text_color = if seg_active { palette.primary_text } else { palette.text };
        let label = text(seg.label)
            .size(theme.type_scale.body)
            .style(move |_| iced::widget::text::Style { color: Some(text_color) });

        let btn_style = move |_theme: &iced::Theme, status: button::Status| {
            if seg_active {
                button::Style {
                    background: Some(Background::Color(palette.accent)),
                    border: Border { radius: 8.0.into(), ..Default::default() },
                    ..button::Style::default()
                }
            } else {
                let bg = match status {
                    button::Status::Hovered => Some(Background::Color(palette.surface_variant)),
                    _ => None,
                };
                button::Style {
                    background: bg,
                    border: Border { radius: 8.0.into(), ..Default::default() },
                    ..button::Style::default()
                }
            }
        };

        button(label).style(btn_style).on_press(seg.on_press.clone()).padding([6, 14]).into()
    }))
    .spacing(2);

    container(segment_row)
        .padding(3)
        .style(move |_theme: &iced::Theme| container::Style {
            background: Some(Background::Color(palette.background)),
            border: Border { radius: 8.0.into(), ..Default::default() },
            ..container::Style::default()
        })
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum TestMsg {
        Selected,
    }

    #[test]
    fn segmented_renders_with_active_segment() {
        let theme = AppTheme::by_name("Midnight");
        let segments = [
            Segment { label: "Chat", active: true, on_press: TestMsg::Selected },
            Segment { label: "Diff", active: false, on_press: TestMsg::Selected },
            Segment { label: "Terminal", active: false, on_press: TestMsg::Selected },
        ];
        let _el: iced::Element<'_, TestMsg> = segmented(&theme, &segments);
    }

    #[test]
    fn segmented_renders_empty_slice() {
        let theme = AppTheme::by_name("Midnight");
        let segments: [Segment<TestMsg>; 0] = [];
        let _el: iced::Element<'_, TestMsg> = segmented(&theme, &segments);
    }
}
