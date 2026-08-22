//! Futuristic container stylers — rounded surfaces with a subtle neon edge.

use iced::widget::container;
use iced::{Background, Border, Shadow, Theme, Vector};

/// Corner radius (px) for modal / floating card surfaces.
const RADIUS: f32 = 14.0;

/// A floating modal card: lifted surface, neon-edged border, soft glow.
///
/// Colors come from the active theme's extended palette, so the card reads
/// neon on Nebula and stays on-brand for the other themes.
///
/// The card surface is intentionally *slightly translucent* (alpha 0.92) so
/// the dimmed backdrop reads through it — a subtle glass effect over the
/// overlay. iced 0.14 has no per-element opacity, so this is color-alpha only.
pub fn modal(theme: &Theme) -> container::Style {
    let p = theme.extended_palette();
    let mut c = p.background.strong.color;
    c.a = 0.92;
    container::Style {
        background: Some(Background::Color(c)),
        border: Border { radius: RADIUS.into(), width: 1.0, color: p.primary.base.color },
        shadow: Shadow {
            color: p.primary.strong.color,
            offset: Vector::new(0.0, 0.0),
            blur_radius: 18.0,
        },
        text_color: Some(p.background.base.text),
        snap: false,
    }
}
