//! Extension methods on [`AppTheme`] for consistent widget styling.
//!
//! Instead of inlining raw `Color::from_rgb(...)` or ad‑hoc container
//! styles across views, use these helpers which pull from the central
//! [`Palette`](crate::theme::Palette).

use iced::widget::{button, container, text_input};
use iced::{Background, Border, Color};

use crate::theme::AppTheme;

/// Convenience methods on [`AppTheme`] for common widget styles.
///
/// # Usage
///
/// ```ignore
/// use crate::ui::ThemeExt;
/// let style = theme.button_primary();
/// ```
pub trait ThemeExt {
    /// Primary button style — uses `palette.primary` background with
    /// `palette.primary_text`.
    fn button_primary(&self) -> button::Style;

    /// Secondary/text button style — transparent background, `palette.text` color.
    fn button_secondary(&self) -> button::Style;

    /// Standard container surface — `palette.surface` background with
    /// `palette.border` border.
    fn container_surface(&self) -> container::Style;

    /// Standard text input style using palette colors.
    fn text_input(&self) -> text_input::Style;

    /// A bordered card (subtle surface variant background + border).
    fn card(&self) -> container::Style;

    /// Focus ring color.
    fn focus_ring_color(&self) -> Color;
}

impl ThemeExt for AppTheme {
    fn button_primary(&self) -> button::Style {
        button::Style {
            background: Some(Background::Color(self.palette.primary)),
            text_color: self.palette.primary_text,
            border: Border::default(),
            shadow: iced::Shadow::default(),
            snap: false,
        }
    }

    fn button_secondary(&self) -> button::Style {
        button::Style {
            background: None,
            text_color: self.palette.text,
            border: Border::default(),
            shadow: iced::Shadow::default(),
            snap: false,
        }
    }

    fn container_surface(&self) -> container::Style {
        container::Style {
            background: Some(Background::Color(self.palette.surface)),
            border: Border { color: self.palette.border, width: 1.0, ..Default::default() },
            ..container::Style::default()
        }
    }

    fn text_input(&self) -> text_input::Style {
        text_input::Style {
            background: self.palette.surface_variant.into(),
            border: Border { color: self.palette.border, width: 1.0, radius: 4.0.into() },
            icon: self.palette.text_muted,
            placeholder: self.palette.text_muted,
            value: self.palette.text,
            selection: self.palette.primary,
        }
    }

    fn card(&self) -> container::Style {
        container::Style {
            background: Some(Background::Color(self.palette.surface_variant)),
            border: Border { color: self.palette.border, width: 1.0, radius: 12.0.into() },
            ..container::Style::default()
        }
    }

    fn focus_ring_color(&self) -> Color {
        self.palette.focus_ring
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::AppTheme;

    /// Ensure button styles return valid colors (no panic, no degenerate values).
    #[test]
    fn button_styles_have_valid_colors() {
        for theme in AppTheme::all() {
            let primary = theme.button_primary();
            assert!(primary.text_color.a > 0.0, "{}: primary button text alpha", theme.name);

            let secondary = theme.button_secondary();
            assert!(secondary.text_color.a > 0.0, "{}: secondary button text alpha", theme.name);
        }
    }

    /// Container styles must have a background set (not transparent).
    #[test]
    fn container_styles_have_backgrounds() {
        for theme in AppTheme::all() {
            let surface = theme.container_surface();
            assert!(
                surface.background.is_some(),
                "{}: container_surface has no background",
                theme.name
            );

            let card = theme.card();
            assert!(card.background.is_some(), "{}: card has no background", theme.name);
        }
    }
}
