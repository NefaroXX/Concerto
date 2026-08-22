//! Futuristic button stylers — rounded corners, neon glow, and gradient fills.
//!
//! All colors are derived from the active `iced::Theme`'s extended palette
//! (never hardcoded), so the look follows whichever theme is selected and the
//! `check-hardcoded-colors.sh` CI gate stays clean.

use iced::gradient::Linear;
use iced::widget::button;
use iced::{Background, Border, Color, Radians, Shadow, Theme, Vector};

/// Corner radius (px) shared by all futuristic buttons.
const RADIUS: f32 = 8.0;

/// Build a horizontal gradient between two colors.
fn gradient(a: Color, b: Color) -> Background {
    Background::Gradient(Linear::new(Radians(0.0)).add_stop(0.0, a).add_stop(1.0, b).into())
}

/// A soft neon glow in `color`, or an invisible shadow when `blur` is zero.
fn glow(color: Color, blur: f32) -> Shadow {
    if blur > 0.0 {
        Shadow { color, offset: Vector::new(0.0, 0.0), blur_radius: blur }
    } else {
        Shadow::default()
    }
}

/// Primary button: cyan→violet gradient with a glowing edge.
pub fn primary(theme: &Theme, status: button::Status) -> button::Style {
    let p = theme.extended_palette();
    let base = button::primary(theme, status);
    let (bg, border_color, glow_color, blur) = match status {
        button::Status::Hovered => (
            gradient(p.primary.strong.color, p.secondary.strong.color),
            p.primary.strong.color,
            p.primary.strong.color,
            14.0,
        ),
        button::Status::Disabled => (
            gradient(p.primary.weak.color, p.secondary.weak.color),
            p.background.strong.color,
            p.background.strong.color,
            0.0,
        ),
        _ => (
            gradient(p.primary.strong.color, p.secondary.strong.color),
            p.primary.base.color,
            p.primary.strong.color,
            8.0,
        ),
    };
    button::Style {
        background: Some(bg),
        border: Border { radius: RADIUS.into(), width: 1.0, color: border_color },
        shadow: glow(glow_color, blur),
        ..base
    }
}

/// Secondary button: violet gradient with a softer glow.
pub fn secondary(theme: &Theme, status: button::Status) -> button::Style {
    let p = theme.extended_palette();
    let base = button::secondary(theme, status);
    let (bg, border_color, glow_color, blur) = match status {
        button::Status::Hovered => (
            gradient(p.secondary.strong.color, p.primary.weak.color),
            p.secondary.strong.color,
            p.secondary.strong.color,
            14.0,
        ),
        button::Status::Disabled => (
            gradient(p.secondary.weak.color, p.background.weak.color),
            p.background.strong.color,
            p.background.strong.color,
            0.0,
        ),
        _ => (
            gradient(p.secondary.strong.color, p.secondary.base.color),
            p.secondary.base.color,
            p.secondary.strong.color,
            8.0,
        ),
    };
    button::Style {
        background: Some(bg),
        border: Border { radius: RADIUS.into(), width: 1.0, color: border_color },
        shadow: glow(glow_color, blur),
        ..base
    }
}

/// Danger button: magenta/red gradient with a warning glow.
pub fn danger(theme: &Theme, status: button::Status) -> button::Style {
    let p = theme.extended_palette();
    let base = button::danger(theme, status);
    let (bg, border_color, glow_color, blur) = match status {
        button::Status::Hovered => (
            gradient(p.danger.strong.color, p.danger.base.color),
            p.danger.strong.color,
            p.danger.strong.color,
            14.0,
        ),
        button::Status::Disabled => (
            gradient(p.danger.weak.color, p.background.weak.color),
            p.background.strong.color,
            p.background.strong.color,
            0.0,
        ),
        _ => (
            gradient(p.danger.strong.color, p.danger.base.color),
            p.danger.base.color,
            p.danger.strong.color,
            8.0,
        ),
    };
    button::Style {
        background: Some(bg),
        border: Border { radius: RADIUS.into(), width: 1.0, color: border_color },
        shadow: glow(glow_color, blur),
        ..base
    }
}

/// Danger outline button: same geometry as [`secondary`], but rendered as a
/// subtle red outline with red label text — no solid fill. Used for
/// destructive list-row actions (e.g. profile removal in Settings) where the
/// loud, filled [`danger`] style would overpower the page.
pub fn danger_outline(theme: &Theme, status: button::Status) -> button::Style {
    let p = theme.extended_palette();
    let danger = p.danger.base.color;
    let (background, text_color, border_color) = match status {
        button::Status::Hovered => (
            Some(Background::Color(iced::Color { a: 0.10, ..danger })),
            p.danger.strong.color,
            p.danger.strong.color,
        ),
        button::Status::Disabled => (None, p.danger.weak.color, p.danger.weak.color),
        _ => (None, danger, danger),
    };
    button::Style {
        background,
        text_color,
        border: Border { radius: RADIUS.into(), width: 1.0, color: border_color },
        ..button::Style::default()
    }
}
