//! Ambient circuit-trace background animation shown while agents are
//! actively running. Purely decorative — implements `canvas::Program`
//! generically over the host `Message` type since it never produces one
//! (no `update()` override, so events pass through untouched).
//!
//! The trace layout is a small, fixed set of PCB-style polylines (axis-
//! aligned segments, via dots at every joint) expressed as fractions of the
//! canvas bounds so they scale with window size. Only the pulse phase
//! changes frame to frame; the geometry itself is static, which is why this
//! skips `canvas::Cache` entirely rather than repeating the
//! clear()-then-draw() pattern already in `agent_graph.rs` — with a value
//! that changes every tick, that cache buys nothing but an extra upload.

use iced::widget::canvas::{self, Canvas, Geometry, Path, Stroke};
use iced::{mouse, Color, Element, Length, Point, Rectangle, Renderer, Theme};

/// Tick cadence for the host `Subscription`. Exposed so `app.rs` builds its
/// `iced::time::every` subscription from the same value used to derive
/// [`PROGRESS_STEP`] below, so the two can't drift apart.
pub const TICK_MS: u64 = 16;

/// Seconds for one full breathing cycle. Tune to taste.
const PULSE_PERIOD_SECS: f32 = 3.2;

/// Fraction of a full `[0.0, 1.0)` cycle advanced per tick.
pub const PROGRESS_STEP: f32 = (TICK_MS as f32) / 1000.0 / PULSE_PERIOD_SECS;

const MIN_ALPHA: f32 = 0.04;
const MAX_ALPHA: f32 = 0.16;
const VIA_ALPHA_BOOST: f32 = 0.06;
const TRACE_WIDTH: f32 = 1.2;
const VIA_RADIUS: f32 = 2.0;

/// One PCB-style trace as waypoints in `[0.0, 1.0]` fractions of the canvas
/// size. Consecutive points are joined with straight segments — keep turns
/// axis-aligned (or 45°) for a "PCB", not "oscilloscope", read.
type Trace = &'static [(f32, f32)];

/// Hand-placed starting layout: short traces hugging the corners/edges so
/// the center — where real content sits — stays clear. Treat these
/// coordinates as a first draft to eyeball and adjust once it's on screen,
/// not a spec.
const TRACES: &[Trace] = &[
    &[(0.03, 0.10), (0.03, 0.04), (0.16, 0.04)],
    &[(0.20, 0.04), (0.34, 0.04), (0.34, 0.09), (0.40, 0.09)],
    &[(0.97, 0.12), (0.97, 0.05), (0.86, 0.05)],
    &[(0.82, 0.05), (0.70, 0.05), (0.70, 0.10), (0.63, 0.10)],
    &[(0.04, 0.90), (0.04, 0.96), (0.18, 0.96)],
    &[(0.22, 0.96), (0.38, 0.96), (0.38, 0.90)],
    &[(0.96, 0.88), (0.96, 0.95), (0.80, 0.95)],
    &[(0.76, 0.95), (0.60, 0.95), (0.60, 0.89), (0.55, 0.89)],
];

/// Indices into `TRACES` that additionally carry a traveling "signal" pulse
/// on top of the ambient breathing glow — keep this short, one or two is
/// plenty for "subtle".
const SIGNAL_TRACES: &[usize] = &[1, 6];

pub struct CircuitBackground {
    progress: f32,
    accent: Color,
}

impl CircuitBackground {
    pub fn new(progress: f32, accent: Color) -> Self {
        Self { progress, accent }
    }

    fn scaled(point: (f32, f32), bounds: Rectangle) -> Point {
        Point::new(point.0 * bounds.width, point.1 * bounds.height)
    }

    /// Point at fractional arc-length `t` (`0.0..=1.0`) along a polyline,
    /// interpolated by segment length so the traveling pulse moves at a
    /// visually constant speed rather than snapping across long segments.
    fn point_along(trace: Trace, bounds: Rectangle, t: f32) -> Point {
        let pts: Vec<Point> = trace.iter().map(|&p| Self::scaled(p, bounds)).collect();
        let lengths: Vec<f32> =
            pts.windows(2).map(|w| (w[1].x - w[0].x).hypot(w[1].y - w[0].y)).collect();
        let total: f32 = lengths.iter().sum();
        if total <= f32::EPSILON || pts.is_empty() {
            return pts.first().copied().unwrap_or(Point::new(0.0, 0.0));
        }
        let target = t.clamp(0.0, 1.0) * total;
        let mut walked = 0.0;
        for (i, seg_len) in lengths.iter().enumerate() {
            if walked + seg_len >= target || i == lengths.len() - 1 {
                let local_t =
                    if *seg_len > f32::EPSILON { (target - walked) / seg_len } else { 0.0 };
                let a = pts[i];
                let b = pts[i + 1];
                return Point::new(a.x + (b.x - a.x) * local_t, a.y + (b.y - a.y) * local_t);
            }
            walked += seg_len;
        }
        pts.last().copied().unwrap_or(Point::new(0.0, 0.0))
    }
}

impl<Message> canvas::Program<Message> for CircuitBackground {
    type State = ();

    fn draw(
        &self,
        _state: &Self::State,
        renderer: &Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<Geometry> {
        // Redrawn every tick regardless of what changed, so a `Cache` here
        // would only add an upload indirection with nothing to reuse.
        let mut frame = canvas::Frame::new(renderer, bounds.size());

        // Phase as a full `[0, 2π)` angle keeps brightness continuous across
        // the `1.0 -> 0.0` wrap (`sin(2π) == sin(0)`), so there's no visual
        // pop when `progress` resets.
        let breath = (self.progress * std::f32::consts::TAU).sin() * 0.5 + 0.5;
        let alpha = MIN_ALPHA + breath * (MAX_ALPHA - MIN_ALPHA);
        let trace_color = Color { a: alpha, ..self.accent };
        let via_color = Color { a: (alpha + VIA_ALPHA_BOOST).min(1.0), ..self.accent };

        for trace in TRACES {
            let points: Vec<Point> = trace.iter().map(|&p| Self::scaled(p, bounds)).collect();
            if points.len() < 2 {
                continue;
            }
            let path = Path::new(|b| {
                b.move_to(points[0]);
                for p in &points[1..] {
                    b.line_to(*p);
                }
            });
            frame.stroke(&path, Stroke::default().with_width(TRACE_WIDTH).with_color(trace_color));

            for point in &points {
                frame.fill(&Path::circle(*point, VIA_RADIUS), via_color);
            }
        }

        // A couple of traveling "signal" pulses for a sense of live current,
        // on their own faster phase so they don't look locked to the
        // breathing cycle.
        let signal_t = (self.progress * 2.0) % 1.0;
        for &idx in SIGNAL_TRACES {
            if let Some(trace) = TRACES.get(idx) {
                let p = Self::point_along(trace, bounds, signal_t);
                // Blend the accent toward white for a "hot" highlight
                // instead of a flat white dot riding on the trace color.
                let hot = Color {
                    r: self.accent.r + (1.0 - self.accent.r) * 0.6,
                    g: self.accent.g + (1.0 - self.accent.g) * 0.6,
                    b: self.accent.b + (1.0 - self.accent.b) * 0.6,
                    a: 0.6,
                };
                frame.fill(&Path::circle(p, VIA_RADIUS * 1.4), hot);
            }
        }

        vec![frame.into_geometry()]
    }
}

/// Build the full-bleed canvas element. `accent` should come from the active
/// theme (`theme.palette.accent`) so the pulse matches whichever of the four
/// themes is active instead of a fixed hardcoded hue.
pub fn view<'a, Message: 'a>(progress: f32, accent: Color) -> Element<'a, Message> {
    Canvas::new(CircuitBackground::new(progress, accent))
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}
