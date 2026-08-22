use crate::widgets::highlight;
use iced::widget::{button, column, container, row, text};
use iced::{Background, Color, Element};

/// A simple code‑block widget with a language header and a copy button.
///
/// `on_copy` is the message emitted when the copy button is pressed.
/// `surface_variant` is the background color from the theme palette.
pub fn view<M: Clone + 'static>(
    code: &str,
    lang: Option<&str>,
    on_copy: M,
    surface_variant: Color,
) -> Element<'static, M> {
    // Header bar – language label and copy button.
    let header = row![
        text(lang.unwrap_or("plain").to_string()).size(14),
        button(text("Copy")).on_press(on_copy.clone())
    ]
    .spacing(8)
    .padding(4);

    // Highlight the code lines with style information.
    let highlighted = highlight::highlight_lines(code, lang);
    let code_lines: Vec<Element<'static, M>> = highlighted
        .into_iter()
        .map(|line| {
            let line_text: String = line.iter().map(|(_, s)| *s).collect();
            text(line_text).into()
        })
        .collect();

    let code_column = column(code_lines).spacing(0).padding(4);

    container(column![header, code_column])
        .padding(4)
        .style(move |_theme| container::Style {
            background: Some(Background::Color(surface_variant)),
            ..container::Style::default()
        })
        .into()
}
