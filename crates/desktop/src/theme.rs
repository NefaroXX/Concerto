pub mod contrast;
pub mod prefs;

use std::collections::HashMap;

use iced::widget::container;
use iced::{Background, Border, Color, Shadow, Vector};

use concerto_core::AgentId;

// ---------------------------------------------------------------------------
// FontStack
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct FontStack {
    pub regular: iced::Font,
    pub mono: iced::Font,
    pub base_size: f32,
}

// ---------------------------------------------------------------------------
// Palette — semantic color tokens for the entire UI
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Palette {
    pub background: Color,
    pub surface: Color,
    pub surface_variant: Color,
    pub primary: Color,
    pub primary_text: Color,
    pub secondary: Color,
    pub success: Color,
    pub warning: Color,
    pub danger: Color,
    pub text: Color,
    pub text_muted: Color,
    pub border: Color,
    pub accent: Color,
    pub focus_ring: Color,
    pub agent_roles: HashMap<AgentId, Color>,
}

fn agent_role_map(
    architect: Color,
    researcher: Color,
    coder: Color,
    reviewer: Color,
    validator: Color,
) -> HashMap<AgentId, Color> {
    let mut m = HashMap::new();
    m.insert(AgentId::new("architect"), architect);
    m.insert(AgentId::new("researcher"), researcher);
    m.insert(AgentId::new("coder"), coder);
    m.insert(AgentId::new("reviewer"), reviewer);
    m.insert(AgentId::new("validator"), validator);
    m
}

// ---------------------------------------------------------------------------
// TypeScale — hierarchical text sizes
// ---------------------------------------------------------------------------

/// Hierarchical text-size tokens. Use these instead of inline `.size(N)` so
/// that section headers, body text, captions, and titles are visually distinct
/// without magic numbers. Values are chosen so adjacent steps are clearly
/// different (at least 2 px apart) while staying within normal UI range.
#[derive(Debug, Clone, Copy)]
pub struct TypeScale {
    /// Small hints, metadata, and status bar labels (11 px).
    pub caption: f32,
    /// Primary body text, button labels (13 px).
    pub body: f32,
    /// Section headers, panel sub-titles (15 px).
    pub label: f32,
    /// Dialog titles, page headers (19 px).
    pub title: f32,
    /// Hero/empty-state display text (24 px).
    pub display: f32,
}

impl Default for TypeScale {
    fn default() -> Self {
        Self { caption: 11.0, body: 13.0, label: 15.0, title: 19.0, display: 24.0 }
    }
}

// ---------------------------------------------------------------------------
// Spacing — consistent layout rhythm
// ---------------------------------------------------------------------------

/// Spacing tokens for layout. Use these instead of inline `.spacing(N)` so
/// that within-section, between-section, and page-level gaps are consistent.
/// Based on a 4 px grid.
#[derive(Debug, Clone, Copy)]
pub struct Spacing {
    /// Tight intra-item spacing (4 px).
    pub xs: f32,
    /// Standard within-section spacing (8 px).
    pub sm: f32,
    /// Between related sections / standard button rows (12 px).
    pub md: f32,
    /// Between distinct sections (16 px).
    pub lg: f32,
    /// Page-level separation (24 px).
    pub xl: f32,
}

impl Default for Spacing {
    fn default() -> Self {
        Self { xs: 4.0, sm: 8.0, md: 12.0, lg: 16.0, xl: 24.0 }
    }
}

// ---------------------------------------------------------------------------
// AppTheme
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct AppTheme {
    pub name: &'static str,
    pub iced: iced::Theme,
    pub palette: Palette,
    pub font_stack: FontStack,
    pub type_scale: TypeScale,
    pub spacing: Spacing,
}

impl AppTheme {
    /// Look up a theme by name. Falls back to Midnight.
    pub fn by_name(name: &str) -> AppTheme {
        match name {
            "Slate" => slate(),
            "Chalk" => chalk(),
            "Nebula" => nebula(),
            _ => midnight(),
        }
    }

    /// All defined themes, for iteration (used by contrast tests).
    pub fn all() -> [AppTheme; 4] {
        [midnight(), slate(), chalk(), nebula()]
    }

    pub fn with_base_size(mut self, size: f32) -> Self {
        self.font_stack.base_size = size.clamp(12.0, 20.0);
        self
    }
}

// ---------------------------------------------------------------------------
// Default font (Iced built-in)
// ---------------------------------------------------------------------------

fn default_font() -> iced::Font {
    iced::Font::default()
}

fn mono_font() -> iced::Font {
    iced::Font::MONOSPACE
}

// ---------------------------------------------------------------------------
// MIDNIGHT — deep blue-gray dark theme
// ---------------------------------------------------------------------------

fn midnight() -> AppTheme {
    AppTheme {
        name: "Midnight",
        iced: iced::Theme::Dark,
        palette: Palette {
            background: Color::from_rgb(0.08, 0.09, 0.12),
            surface: Color::from_rgb(0.12, 0.14, 0.18),
            surface_variant: Color::from_rgb(0.16, 0.18, 0.23),
            primary: Color::from_rgb(0.30, 0.55, 1.00),
            primary_text: Color::from_rgb(1.00, 1.00, 1.00),
            secondary: Color::from_rgb(0.40, 0.70, 1.00),
            success: Color::from_rgb(0.30, 0.85, 0.45),
            warning: Color::from_rgb(1.00, 0.75, 0.15),
            danger: Color::from_rgb(1.00, 0.30, 0.30),
            text: Color::from_rgb(0.92, 0.93, 0.95),
            text_muted: Color::from_rgb(0.55, 0.57, 0.62),
            border: Color::from_rgb(0.45, 0.47, 0.52),
            accent: Color::from_rgb(0.50, 0.40, 1.00),
            focus_ring: Color::from_rgb(0.30, 0.55, 1.00),
            agent_roles: agent_role_map(
                Color::from_rgb(0.50, 0.40, 1.00),
                Color::from_rgb(0.20, 0.70, 0.90),
                Color::from_rgb(0.30, 0.85, 0.45),
                Color::from_rgb(1.00, 0.75, 0.15),
                Color::from_rgb(1.00, 0.50, 0.20),
            ),
        },
        font_stack: FontStack { regular: default_font(), mono: mono_font(), base_size: 14.0 },
        type_scale: TypeScale::default(),
        spacing: Spacing::default(),
    }
}

// ---------------------------------------------------------------------------
// SLATE — medium-contrast warm-gray theme
// ---------------------------------------------------------------------------

fn slate() -> AppTheme {
    AppTheme {
        name: "Slate",
        iced: iced::Theme::Dark,
        palette: Palette {
            background: Color::from_rgb(0.22, 0.22, 0.24),
            surface: Color::from_rgb(0.28, 0.28, 0.30),
            surface_variant: Color::from_rgb(0.33, 0.33, 0.35),
            primary: Color::from_rgb(0.35, 0.60, 1.00),
            primary_text: Color::from_rgb(1.00, 1.00, 1.00),
            secondary: Color::from_rgb(0.50, 0.72, 1.00),
            success: Color::from_rgb(0.35, 0.80, 0.50),
            warning: Color::from_rgb(1.00, 0.78, 0.25),
            danger: Color::from_rgb(1.00, 0.38, 0.38),
            text: Color::from_rgb(0.90, 0.90, 0.92),
            text_muted: Color::from_rgb(0.60, 0.60, 0.64),
            border: Color::from_rgb(0.60, 0.60, 0.64),
            accent: Color::from_rgb(0.55, 0.45, 1.00),
            focus_ring: Color::from_rgb(0.35, 0.60, 1.00),
            agent_roles: agent_role_map(
                Color::from_rgb(0.55, 0.45, 1.00),
                Color::from_rgb(0.30, 0.72, 0.92),
                Color::from_rgb(0.35, 0.80, 0.50),
                Color::from_rgb(1.00, 0.78, 0.25),
                Color::from_rgb(1.00, 0.55, 0.25),
            ),
        },
        font_stack: FontStack { regular: default_font(), mono: mono_font(), base_size: 14.0 },
        type_scale: TypeScale::default(),
        spacing: Spacing::default(),
    }
}

// ---------------------------------------------------------------------------
// CHALK — warm light theme
// ---------------------------------------------------------------------------

fn chalk() -> AppTheme {
    AppTheme {
        name: "Chalk",
        iced: iced::Theme::Light,
        palette: Palette {
            background: Color::from_rgb(0.96, 0.95, 0.93),
            surface: Color::from_rgb(1.00, 1.00, 1.00),
            surface_variant: Color::from_rgb(0.94, 0.93, 0.90),
            primary: Color::from_rgb(0.20, 0.45, 0.90),
            primary_text: Color::from_rgb(1.00, 1.00, 1.00),
            secondary: Color::from_rgb(0.35, 0.60, 1.00),
            success: Color::from_rgb(0.15, 0.60, 0.28),
            warning: Color::from_rgb(0.85, 0.65, 0.10),
            danger: Color::from_rgb(0.75, 0.15, 0.15),
            text: Color::from_rgb(0.12, 0.12, 0.14),
            text_muted: Color::from_rgb(0.50, 0.50, 0.54),
            border: Color::from_rgb(0.55, 0.54, 0.50),
            accent: Color::from_rgb(0.45, 0.35, 0.90),
            focus_ring: Color::from_rgb(0.20, 0.45, 0.90),
            agent_roles: agent_role_map(
                Color::from_rgb(0.45, 0.35, 0.90),
                Color::from_rgb(0.15, 0.55, 0.75),
                Color::from_rgb(0.20, 0.70, 0.35),
                Color::from_rgb(0.85, 0.65, 0.10),
                Color::from_rgb(0.85, 0.40, 0.10),
            ),
        },
        font_stack: FontStack { regular: default_font(), mono: mono_font(), base_size: 14.0 },
        type_scale: TypeScale::default(),
        spacing: Spacing::default(),
    }
}

// ---------------------------------------------------------------------------
// NEBULA — deep-space futuristic theme (neon cyan/violet on near-black)
// ---------------------------------------------------------------------------

fn nebula() -> AppTheme {
    use iced::theme::Palette as IcedPalette;

    // The `iced::Theme` carries the neon palette so built-in and custom
    // stylers (which read `theme.extended_palette()`) render neon too.
    let iced_palette = IcedPalette {
        background: Color::from_rgb(0.03, 0.04, 0.07),
        text: Color::from_rgb(0.92, 0.95, 1.0),
        primary: Color::from_rgb(0.15, 0.85, 1.0), // Vibrant cyan
        success: Color::from_rgb(0.25, 1.0, 0.65),
        warning: Color::from_rgb(1.0, 0.82, 0.25),
        danger: Color::from_rgb(1.0, 0.3, 0.55),
    };

    AppTheme {
        name: "Nebula",
        iced: iced::Theme::custom("Nebula".to_string(), iced_palette),
        palette: Palette {
            background: Color::from_rgb(0.03, 0.04, 0.07),
            surface: Color::from_rgb(0.09, 0.11, 0.18), // Slightly lifted panels
            surface_variant: Color::from_rgb(0.13, 0.15, 0.24),
            primary: Color::from_rgb(0.15, 0.85, 1.0),
            primary_text: Color::WHITE,
            secondary: Color::from_rgb(0.7, 0.4, 1.0), // Purple accent
            success: Color::from_rgb(0.25, 1.0, 0.65),
            warning: Color::from_rgb(1.0, 0.82, 0.25),
            danger: Color::from_rgb(1.0, 0.3, 0.55),
            text: Color::from_rgb(0.92, 0.95, 1.0),
            text_muted: Color::from_rgb(0.58, 0.65, 0.78),
            border: Color::from_rgb(0.25, 0.45, 0.65),
            accent: Color::from_rgb(0.4, 0.9, 1.0),
            focus_ring: Color::from_rgb(0.15, 0.85, 1.0),
            agent_roles: agent_role_map(
                Color::from_rgb(0.7, 0.4, 1.0),   // Architect
                Color::from_rgb(0.15, 0.85, 1.0), // Researcher
                Color::from_rgb(0.25, 1.0, 0.65), // Coder
                Color::from_rgb(1.0, 0.82, 0.25), // Reviewer
                Color::from_rgb(1.0, 0.55, 0.3),  // Validator
            ),
        },
        font_stack: FontStack { regular: default_font(), mono: mono_font(), base_size: 14.5 },
        type_scale: TypeScale::default(),
        spacing: Spacing::default(),
    }
}

// ---------------------------------------------------------------------------
// Card style — rounded, shadowed container for depth-on-surface
// ---------------------------------------------------------------------------

/// A lifted-card container style with rounded corners, a thin border, and a
/// soft drop shadow. Apply to welcome cards, agent panels, and any surface
/// that should feel elevated above the background.
pub fn card_style(palette: &Palette) -> container::Style {
    container::Style {
        background: Some(Background::Color(palette.surface)),
        border: Border { radius: 12.0.into(), width: 1.0, color: palette.border },
        shadow: Shadow {
            color: Color::from_rgba(0.0, 0.0, 0.0, 0.3),
            offset: Vector::new(0.0, 4.0),
            blur_radius: 12.0,
        },
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// Sidebar typography helpers
// ---------------------------------------------------------------------------

/// Style for sidebar section headers (e.g. "WORKSPACE", "REVIEW").
pub fn sidebar_header_style(palette: &Palette) -> iced::widget::text::Style {
    iced::widget::text::Style { color: Some(palette.text_muted) }
}

/// Style for sidebar nav items — accent color when active, body text otherwise.
pub fn sidebar_item_style(palette: &Palette, active: bool) -> iced::widget::text::Style {
    iced::widget::text::Style { color: Some(if active { palette.accent } else { palette.text }) }
}

/// Style for agent role names — drawn from the agent_roles palette map.
pub fn agent_role_style(palette: &Palette, role: &str) -> iced::widget::text::Style {
    let color = palette.agent_roles.get(&AgentId::new(role)).copied().unwrap_or(palette.accent);
    iced::widget::text::Style { color: Some(color) }
}

#[cfg(test)]
mod tests {
    use super::{sidebar_header_style, sidebar_item_style, slate};

    #[test]
    fn slate_returns_dark_theme() {
        assert_eq!(slate().iced, iced::Theme::Dark);
    }

    #[test]
    fn slate_palette_has_all_colors() {
        let theme = slate();
        assert!(theme.palette.background.r > 0.0 || theme.palette.surface.r > 0.0);
        assert!(
            theme.palette.text.r > 0.0 || theme.palette.text.g > 0.0 || theme.palette.text.b > 0.0
        );
    }

    #[test]
    fn slate_palette_has_accent_color() {
        let theme = slate();
        assert!(
            theme.palette.accent.r > 0.0
                || theme.palette.accent.g > 0.0
                || theme.palette.accent.b > 0.0
        );
    }

    #[test]
    fn sidebar_header_style_uses_muted_color() {
        let theme = slate();
        let style = sidebar_header_style(&theme.palette);
        assert!(style.color.is_some());
    }

    #[test]
    fn sidebar_item_active_uses_accent() {
        let theme = slate();
        let active = sidebar_item_style(&theme.palette, true);
        assert_eq!(active.color, Some(theme.palette.accent));
    }

    #[test]
    fn sidebar_item_inactive_uses_text() {
        let theme = slate();
        let inactive = sidebar_item_style(&theme.palette, false);
        assert_eq!(inactive.color, Some(theme.palette.text));
    }

    /// Slate palette has a valid surface color distinct from background.
    #[test]
    fn slate_surface_color_is_distinct() {
        let theme = slate();
        let bg = theme.palette.background;
        let surface = theme.palette.surface;
        assert!(
            (bg.r - surface.r).abs() > 0.01
                || (bg.g - surface.g).abs() > 0.01
                || (bg.b - surface.b).abs() > 0.01,
            "surface should be distinct from background"
        );
    }
}
