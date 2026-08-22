use concerto_memory::prefs::{PrefKey, UserPrefsStore};

use crate::theme::AppTheme;

pub fn load_theme(prefs: &UserPrefsStore) -> AppTheme {
    let name = prefs.get(&PrefKey::UiTheme).unwrap_or_else(|| "Midnight".into());
    let size = prefs
        .get(&PrefKey::UiFontSize)
        .and_then(|s| s.parse::<f32>().ok())
        .unwrap_or(14.0)
        .clamp(12.0, 20.0);
    AppTheme::by_name(&name).clone().with_base_size(size)
}

pub fn save_theme(prefs: &UserPrefsStore, theme: &AppTheme) {
    let _ = prefs.set(&PrefKey::UiTheme, theme.name.to_string());
    let _ = prefs.set(&PrefKey::UiFontSize, theme.font_stack.base_size.to_string());
}
