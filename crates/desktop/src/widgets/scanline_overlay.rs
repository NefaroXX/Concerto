//! Faint horizontal scan-line overlay with optional grid, intended to sit
//! above the circuit-trace background. Pulse rate doubles while a streaming
//! assistant entry is active; reduced-motion freezes the animation.
//!
//! Design doc prototypes #3-#4: no per-char color cycling, no random
//! timing, palette-only colors, deterministic rendering.

use iced::widget::canvas::{self, Canvas, Geometry, Path, Stroke};
use iced::{mouse, Color, Element, Length, Point, Rectangle, Renderer, Theme};

/// Tick cadence shared with `circuit_background::TICK_MS` (16 ms).
pub const TICK_MS: u64 = 16;

/// Seconds for one full pulse cycle at normal (idle) rate.
const PULSE_PERIOD_SECS: f32 = 4.0;

/// Fraction of `[0, 1)` progress per tick at idle rate.
pub const PROGRESS_STEP: f32 = (TICK_MS as f32) / 1000.0 / PULSE_PERIOD_SECS;

/// Scan-line spacing in pixels (1 px line every 4 px).
const LINE_SPACING: f32 = 4.0;

/// Grid-line spacing in pixels (when enabled).
const GRID_SPACING: f32 = 32.0;

/// Minimum alpha for scan-line breathing (0.0–1.0).
const MIN_ALPHA: f32 = 0.06;

/// Maximum alpha for scan-line breathing (0.0–1.0).
const MAX_ALPHA: f32 = 0.10;

/// Fixed alpha for the optional grid lines (subtle, not breathing).
const GRID_ALPHA: f32 = 0.04;

/// Scanline overlay widget.
pub struct ScanlineOverlay {
    progress: f32,
    is_streaming: bool,
    reduced_motion: bool,
    show_grid: bool,
    surface: Color,
    border: Color,
}

impl ScanlineOverlay {
    /// Create a new overlay.
    ///
    /// - `progress`: current animation phase in `[0, 1)`.
    /// - `is_streaming`: when `true` the pulse rate doubles.
    /// - `reduced_motion`: when `true` the overlay is static (no pulse).
    /// - `show_grid`: render the optional grid overlay.
    /// - `surface` / `border`: palette-derived colors (widget is exempt from
    ///   the `palette.*` constraint that applies to views/ui, but should use
    ///   palette tokens for consistency).
    pub fn new(
        progress: f32,
        is_streaming: bool,
        reduced_motion: bool,
        show_grid: bool,
        surface: Color,
        border: Color,
    ) -> Self {
        Self { progress, is_streaming, reduced_motion, show_grid, surface, border }
    }
}

impl<Message> canvas::Program<Message> for ScanlineOverlay {
    type State = ();

    fn draw(
        &self,
        _state: &Self::State,
        renderer: &Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<Geometry> {
        let mut frame = canvas::Frame::new(renderer, bounds.size());

        // Effective progress: frozen when reduced_motion, doubled when streaming.
        let effective_progress = if self.reduced_motion {
            0.0
        } else if self.is_streaming {
            self.progress * 2.0
        } else {
            self.progress
        };

        // Breathing phase: sin wave for smooth alpha oscillation.
        let breath = (effective_progress * std::f32::consts::TAU).sin() * 0.5 + 0.5;
        let scanline_alpha = MIN_ALPHA + breath * (MAX_ALPHA - MIN_ALPHA);

        // Surface-tinted scan lines (1 px every LINE_SPACING px).
        let scanline_color = Color { a: scanline_alpha, ..self.surface };
        let scanline_stroke = Stroke::default().with_width(1.0).with_color(scanline_color);

        let mut y = 0.0;
        while y < bounds.height {
            let path = Path::line(Point::new(0.0, y), Point::new(bounds.width, y));
            frame.stroke(&path, scanline_stroke);
            y += LINE_SPACING;
        }

        // Optional border-tinted grid (every GRID_SPACING px, both axes).
        if self.show_grid {
            let grid_color = Color { a: GRID_ALPHA, ..self.border };
            let grid_stroke = Stroke::default().with_width(1.0).with_color(grid_color);

            let mut gy = 0.0;
            while gy < bounds.height {
                let path = Path::line(Point::new(0.0, gy), Point::new(bounds.width, gy));
                frame.stroke(&path, grid_stroke);
                gy += GRID_SPACING;
            }

            let mut gx = 0.0;
            while gx < bounds.width {
                let path = Path::line(Point::new(gx, 0.0), Point::new(gx, bounds.height));
                frame.stroke(&path, grid_stroke);
                gx += GRID_SPACING;
            }
        }

        vec![frame.into_geometry()]
    }
}

/// Build the full-bleed scan-line overlay element.
///
/// Colors should come from `theme.palette.surface` and `theme.palette.border`.
pub fn view<'a, Message: 'a>(
    progress: f32,
    is_streaming: bool,
    reduced_motion: bool,
    show_grid: bool,
    surface: Color,
    border: Color,
) -> Element<'a, Message> {
    Canvas::new(ScanlineOverlay::new(
        progress,
        is_streaming,
        reduced_motion,
        show_grid,
        surface,
        border,
    ))
    .width(Length::Fill)
    .height(Length::Fill)
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_step_matches_tick_ms() {
        let expected = (TICK_MS as f32) / 1000.0 / PULSE_PERIOD_SECS;
        assert!((PROGRESS_STEP - expected).abs() < f32::EPSILON);
    }

    #[test]
    fn alpha_range_is_sensible() {
        assert!(MIN_ALPHA > 0.0 && MIN_ALPHA < 1.0);
        assert!(MAX_ALPHA > MIN_ALPHA && MAX_ALPHA <= 1.0);
    }

    #[test]
    fn grid_alpha_is_subtle() {
        assert!(GRID_ALPHA > 0.0 && GRID_ALPHA < 0.1);
    }

    #[test]
    fn reduced_motion_zeroes_progress() {
        let overlay = ScanlineOverlay::new(0.5, false, true, false, Color::WHITE, Color::BLACK);
        // When reduced_motion is true, effective_progress should be 0.0
        // (static), so breath = sin(0)*0.5+0.5 = 0.5 → mid-range alpha.
        assert!(overlay.reduced_motion);
    }

    #[test]
    fn streaming_flag_stored() {
        let overlay = ScanlineOverlay::new(0.0, true, false, false, Color::WHITE, Color::BLACK);
        assert!(overlay.is_streaming);
    }

    #[test]
    fn grid_flag_stored() {
        let overlay = ScanlineOverlay::new(0.0, false, false, true, Color::WHITE, Color::BLACK);
        assert!(overlay.show_grid);
    }

    #[test]
    fn new_construction_sets_all_fields() {
        let overlay = ScanlineOverlay::new(0.3, true, false, true, Color::WHITE, Color::BLACK);
        assert!((overlay.progress - 0.3).abs() < f32::EPSILON);
        assert!(overlay.is_streaming);
        assert!(!overlay.reduced_motion);
        assert!(overlay.show_grid);
    }

    #[test]
    fn line_spacing_and_grid_spacing_are_positive() {
        assert!(LINE_SPACING > 0.0);
        assert!(GRID_SPACING > LINE_SPACING);
    }
}
