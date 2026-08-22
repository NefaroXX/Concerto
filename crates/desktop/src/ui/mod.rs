//! Shared UI helpers for consistent, polished views.
//!
//! This module provides reusable components that enforce visual consistency
//! across all views: empty states, themed widget extensions, section cards,
//! form fields, feedback toasts, navigation items, and layout constants.
//!
//! Every public helper accepts `&AppTheme` and returns `Element<'_, Message>`.

pub mod button;
pub mod container;
pub mod empty_state;
pub mod feedback;
pub mod form;
pub mod layout;
pub mod list_item;
pub mod nav_item;
pub mod section_card;
pub mod segmented;
pub mod theme_ext;

pub use empty_state::{empty_state, empty_state_compact};
pub use form::{form_field, labeled_slider};
pub use layout::{padded, section as layout_section, SPACING_MD, SPACING_SM, SPACING_XS};
pub use list_item::list_item;
pub use nav_item::nav_item;
pub use section_card::{section_card, section_card_with_subtitle};
pub use segmented::{segmented, Segment};
pub use theme_ext::ThemeExt;
