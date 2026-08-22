use iced::widget::{container, row, text};
use iced::{Element, Length};

use crate::theme::AppTheme;

/// Severity level for a toast notification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastLevel {
    Success,
    Error,
    Info,
}

/// A single toast notification.
#[derive(Debug, Clone)]
pub struct Toast {
    pub message: String,
    pub level: ToastLevel,
    pub id: u64,
    /// Instant the toast was created, used to auto-dismiss stale toasts.
    pub created_at: std::time::Instant,
}

/// Manages a queue of toasts. Add toasts via `push`, render via `view`.
///
/// Example:
/// ```ignore
/// let mut manager = ToastManager::new();
/// manager.push(ToastLevel::Success, "Settings saved".into());
/// let element = manager.view(&theme, &Message::DismissToast);
/// ```
#[derive(Debug, Clone)]
pub struct ToastManager {
    toasts: Vec<Toast>,
    next_id: u64,
}

impl ToastManager {
    /// Create a new empty toast manager.
    pub fn new() -> Self {
        Self { toasts: Vec::new(), next_id: 1 }
    }

    /// Push a new toast. Keeps only the 3 most recent toasts.
    pub fn push(&mut self, level: ToastLevel, message: String) {
        self.toasts.push(Toast {
            message,
            level,
            id: self.next_id,
            created_at: std::time::Instant::now(),
        });
        self.next_id += 1;
        // Keep only the 3 most recent
        if self.toasts.len() > 3 {
            self.toasts.remove(0);
        }
    }

    /// Remove toasts created before `cutoff` (for time-based auto-dismiss).
    pub fn prune_older_than(&mut self, cutoff: std::time::Instant) {
        self.toasts.retain(|t| t.created_at >= cutoff);
    }

    /// Remove a toast by ID (for dismissal).
    pub fn dismiss(&mut self, id: u64) {
        self.toasts.retain(|t| t.id != id);
    }

    /// Returns true if there are active toasts.
    pub fn has_toasts(&self) -> bool {
        !self.toasts.is_empty()
    }

    /// Render the toast notification bar at the top of a container.
    ///
    /// Returns None if there are no toasts.
    pub fn view<'a, Message: 'a + Clone>(
        &'a self,
        theme: &'a AppTheme,
    ) -> Option<Element<'a, Message>> {
        if self.toasts.is_empty() {
            return None;
        }

        // Show only the most recent toast
        let toast = self.toasts.last()?;
        let palette = &theme.palette;

        let (bg, icon) = match toast.level {
            ToastLevel::Success => (palette.success, "✓"),
            ToastLevel::Error => (palette.danger, "✗"),
            ToastLevel::Info => (palette.secondary, "ℹ"),
        };

        Some(
            container(
                row![
                    text(icon).size(14).color(palette.background),
                    text(&toast.message).size(13).color(palette.background),
                ]
                .spacing(8)
                .padding(8),
            )
            .width(Length::Fill)
            .style(move |_theme: &iced::Theme| container::Style {
                background: Some(iced::Background::Color(bg)),
                ..container::Style::default()
            })
            .into(),
        )
    }
}

impl Default for ToastManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::AppTheme;

    #[derive(Debug, Clone)]
    enum TestMsg {}

    #[test]
    fn new_has_no_toasts() {
        let mgr = ToastManager::new();
        assert!(!mgr.has_toasts());
    }

    #[test]
    fn push_adds_toast() {
        let mut mgr = ToastManager::new();
        mgr.push(ToastLevel::Success, "Saved".into());
        assert!(mgr.has_toasts());
    }

    #[test]
    fn max_three_toasts() {
        let mut mgr = ToastManager::new();
        mgr.push(ToastLevel::Info, "1".into());
        mgr.push(ToastLevel::Info, "2".into());
        mgr.push(ToastLevel::Info, "3".into());
        mgr.push(ToastLevel::Info, "4".into());
        assert_eq!(mgr.toasts.len(), 3);
        assert_eq!(mgr.toasts[0].message, "2");
    }

    #[test]
    fn dismiss_removes_toast() {
        let mut mgr = ToastManager::new();
        mgr.push(ToastLevel::Success, "Test".into());
        let id = mgr.toasts[0].id;
        mgr.dismiss(id);
        assert!(!mgr.has_toasts());
    }

    #[test]
    fn push_sets_created_at() {
        let mut mgr = ToastManager::new();
        mgr.push(ToastLevel::Success, "Saved".into());
        // A toast pushed now must survive a prune of anything older than 10s.
        let cutoff = std::time::Instant::now() - std::time::Duration::from_secs(10);
        mgr.prune_older_than(cutoff);
        assert!(mgr.has_toasts());
        assert!(mgr.toasts[0].created_at >= cutoff);
    }

    #[test]
    fn prune_removes_expired() {
        let mut mgr = ToastManager::new();
        mgr.push(ToastLevel::Success, "Saved".into());
        // Prune with a cutoff in the future: the toast is older than it, so it
        // is removed.
        mgr.prune_older_than(std::time::Instant::now() + std::time::Duration::from_secs(2));
        assert!(!mgr.has_toasts());
    }

    #[test]
    fn view_returns_none_when_empty() {
        let theme = AppTheme::by_name("Midnight");
        let mgr = ToastManager::new();
        let result: Option<Element<'_, TestMsg>> = mgr.view(&theme);
        assert!(result.is_none());
    }

    #[test]
    fn view_returns_some_with_toasts() {
        let theme = AppTheme::by_name("Midnight");
        let mut mgr = ToastManager::new();
        mgr.push(ToastLevel::Success, "Saved".into());
        let result: Option<Element<'_, TestMsg>> = mgr.view(&theme);
        assert!(result.is_some());
    }
}
