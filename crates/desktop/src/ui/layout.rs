//! Named spacing and layout constants for visual consistency.
//!
//! Use these constants instead of raw literals throughout views.
//!
//! # Spacing scale
//!
//! The spacing scale is based on a **4 px grid**:
//!
//! | Name | Value | Use case |
//! |------|-------|----------|
//! | `SPACING_XS` | 4 px  | Tight gaps, icon-to-text |
//! | `SPACING_SM` | 8 px  | Related items, button groups |
//! | `SPACING_MD` | 16 px | Sections, cards, form rows |
//! | `SPACING_LG` | 24 px | Major sections, page blocks |
//! | `SPACING_XL` | 32 px | Page-level separation |
//!
//! Prefer the constants over raw numbers so the entire UI stays on the
//! same grid and adjustments are trivial.

use iced::widget::{column, container, text};
use iced::Element;

// ---------------------------------------------------------------------------
// Spacing scale (px)
// ---------------------------------------------------------------------------

pub const SPACING_XS: f32 = 4.0;
pub const SPACING_SM: f32 = 8.0;
pub const SPACING_MD: f32 = 16.0;
pub const SPACING_LG: f32 = 24.0;
pub const SPACING_XL: f32 = 32.0;

// ---------------------------------------------------------------------------
// Named padding presets
// ---------------------------------------------------------------------------

/// Standard padding for view content areas.
pub const PADDING_VIEW: f32 = 20.0;

/// Standard padding for card/section interiors.
pub const PADDING_CARD: f32 = 16.0;

/// Standard padding for tight/compact sections.
pub const PADDING_COMPACT: f32 = 8.0;

/// Standard padding for form fields.
pub const PADDING_FORM: f32 = 12.0;

// ---------------------------------------------------------------------------
// Layout helpers
// ---------------------------------------------------------------------------

/// Wrap content in a labeled section with consistent padding.
///
/// Produces a column with a section title header followed by the content,
/// using standard section spacing.
pub fn section<'a, Message: 'a + Clone>(
    title: impl Into<String>,
    content: impl Into<Element<'a, Message>>,
) -> Element<'a, Message> {
    column![text(title.into()).size(16), content.into(),].spacing(SPACING_SM).into()
}

/// Wrap content in a padded container.
///
/// Shorthand for `container(content).padding(padding)`.
pub fn padded<'a, Message: 'a>(
    padding: f32,
    content: impl Into<Element<'a, Message>>,
) -> Element<'a, Message> {
    container(content.into()).padding(padding).into()
}
