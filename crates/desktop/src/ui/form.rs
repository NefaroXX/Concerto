//! Form field helpers for consistent, polished input rendering.
//!
//! Provides reusable components that wrap input widgets with labels,
//! optional help text, error messages, and required markers.

use iced::widget::{column, row, slider, text};
use iced::Element;

use crate::theme::AppTheme;

/// A labeled form field with optional help text and error display.
///
/// * `label` — The field label text (e.g. "Model Name")
/// * `required` — Whether to show a `*` required marker
/// * `help` — Optional help text shown below the input in muted style
/// * `error` — Optional error message shown in danger color
/// * `input` — The actual input widget (text_input, pick_list, etc.)
pub fn form_field<'a, Message: 'a + Clone>(
    theme: &'a AppTheme,
    label: impl Into<String>,
    required: bool,
    help: Option<impl Into<String>>,
    error: Option<impl Into<String>>,
    input: impl Into<Element<'a, Message>>,
) -> Element<'a, Message> {
    let palette = &theme.palette;

    let label_text = if required { format!("{} *", label.into()) } else { label.into() };

    let mut col = column![text(label_text).size(13).color(palette.text), input.into(),].spacing(4);

    if let Some(help_text) = help {
        col = col.push(text(help_text.into()).size(11).color(palette.text_muted));
    }

    if let Some(err) = error {
        col = col.push(text(err.into()).size(11).color(palette.danger));
    }

    col.spacing(4).into()
}

/// A labeled slider with a live value display.
///
/// * `label` — Label text
/// * `value` — Current slider value
/// * `range` — Slider range (e.g. `1.0..=365.0`)
/// * `on_change` — Message to emit on change
/// * `formatter` — Display formatting for the value (e.g. `|v| format!("{:.0} days", v)`)
pub fn labeled_slider<'a, Message: 'a + Clone>(
    theme: &'a AppTheme,
    label: impl Into<String>,
    value: f32,
    range: std::ops::RangeInclusive<f32>,
    on_change: impl Fn(f32) -> Message + 'a,
    formatter: impl Fn(f32) -> String + 'a,
) -> Element<'a, Message> {
    let palette = &theme.palette;

    let label_row = row![
        text(label.into()).size(13).color(palette.text),
        text(formatter(value)).size(12).color(palette.text_muted),
    ]
    .spacing(8)
    .align_y(iced::Alignment::Center);

    column![label_row, slider(range, value, on_change),].spacing(2).into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::AppTheme;
    use iced::widget::text_input;

    #[derive(Debug, Clone)]
    enum TestMsg {
        #[allow(dead_code)]
        ValueChanged(f32),
    }

    /// `form_field` renders without panicking (basic smoke test).
    #[test]
    fn form_field_renders() {
        let theme = AppTheme::by_name("Midnight");
        let input = text_input("Enter value", "test");
        let _element = form_field::<TestMsg>(
            &theme,
            "Model Name",
            true,
            Some("Helpful description"),
            None::<&str>,
            input,
        );
    }

    /// `labeled_slider` renders without panicking.
    #[test]
    fn labeled_slider_renders() {
        let theme = AppTheme::by_name("Midnight");
        let _element = labeled_slider::<TestMsg>(
            &theme,
            "Test Slider",
            50.0,
            0.0..=100.0,
            TestMsg::ValueChanged,
            |v| format!("{:.0}%", v),
        );
    }
}
