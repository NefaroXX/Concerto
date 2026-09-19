//! CLI theme bridge: the four desktop palettes (`Midnight` / `Slate` /
//! `Chalk` / `Nebula`, same names as `concerto_desktop::theme::AppTheme`)
//! mapped to ANSI role colors for the ratatui frontend.
//!
//! The map is data-only: each [`CliTheme`] carries one [`Color`] per chat
//! role (`user` / `assistant` / `policy`) plus the status colors
//! (`success` / `warning` / `danger` / `muted` / `border`). `NO_COLOR` and
//! off-TTY plain output stay caller-gated via the existing `styling_enabled`
//! bool — a theme never emits ANSI on its own, so pipes stay clean.

use ratatui::style::Color;

use concerto_config::AppConfig;

/// The four theme names shared with the desktop (`AppTheme::by_name`).
pub const CLI_THEME_NAMES: [&str; 4] = ["Midnight", "Slate", "Chalk", "Nebula"];
/// Environment override for the CLI theme (precedence: flag > env > config).
pub const CLI_THEME_ENV_VAR: &str = "CONCERTO_THEME";
/// Default theme when no flag, env, or config key selects one.
pub const DEFAULT_CLI_THEME: &str = "Midnight";

/// ANSI role colors for one CLI theme: chat gutters plus status chrome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CliTheme {
    /// Canonical theme name (one of [`CLI_THEME_NAMES`]).
    pub name: &'static str,
    /// User gutter (`›`).
    pub user: Color,
    /// Assistant gutter (`♪`) and reveal line.
    pub assistant: Color,
    /// Policy gutter (`‖`) and thinking digest.
    pub policy: Color,
    /// Tool success / input-active border / completed transcript lines.
    pub success: Color,
    /// Selection highlight / tool timeout.
    pub warning: Color,
    /// Tool failure / error lines.
    pub danger: Color,
    /// Dimmed chrome: status bar, footers, fence gutters, idle input.
    pub muted: Color,
    /// Modal and focus borders.
    pub border: Color,
}

impl CliTheme {
    /// Look up a theme by name (case-insensitive, trimmed). Unknown or empty
    /// names fall back to `Midnight` — the same contract as the desktop
    /// `AppTheme::by_name`.
    pub fn by_name(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "slate" => slate(),
            "chalk" => chalk(),
            "nebula" => nebula(),
            _ => midnight(),
        }
    }

    /// Stable ANSI color per specialist role (V2 score-accordion contract).
    /// `Midnight` keeps the historical hues; light/neon themes adjust only
    /// the low-contrast buckets (coordinator / fallback) so headers stay
    /// matched to the desktop agent buckets on every theme.
    pub fn agent_color(&self, role: &str) -> Color {
        match (self.name, role.to_lowercase().as_str()) {
            (_, "architect") => {
                if self.name == "Nebula" {
                    Color::LightMagenta
                } else {
                    Color::Magenta
                }
            }
            (_, "researcher") => match self.name {
                "Slate" => Color::LightBlue,
                "Nebula" => Color::LightBlue,
                _ => Color::Blue,
            },
            (_, "coder") => {
                if self.name == "Nebula" {
                    Color::LightGreen
                } else {
                    Color::Green
                }
            }
            (_, "reviewer") => match self.name {
                "Chalk" => Color::Yellow,
                "Nebula" => Color::LightYellow,
                _ => Color::Yellow,
            },
            (_, "validator") => match self.name {
                "Nebula" => Color::LightCyan,
                _ => Color::Cyan,
            },
            (_, "coordinator") => {
                if self.name == "Chalk" {
                    Color::Black
                } else {
                    Color::White
                }
            }
            _ => self.muted,
        }
    }
}

/// `Midnight` — deep blue-gray dark theme; the historical CLI hues, so the
/// default output is byte-identical to the pre-theme CLI.
fn midnight() -> CliTheme {
    CliTheme {
        name: "Midnight",
        user: Color::Cyan,
        assistant: Color::Green,
        policy: Color::Yellow,
        success: Color::Green,
        warning: Color::Yellow,
        danger: Color::Red,
        muted: Color::DarkGray,
        border: Color::Cyan,
    }
}

/// `Slate` — medium-contrast warm-gray theme; user/border shift blue-ward,
/// dimmed chrome lifts to `Gray` for the lighter surface.
fn slate() -> CliTheme {
    CliTheme {
        name: "Slate",
        user: Color::Blue,
        assistant: Color::Green,
        policy: Color::Yellow,
        success: Color::Green,
        warning: Color::Yellow,
        danger: Color::Red,
        muted: Color::Gray,
        border: Color::Blue,
    }
}

/// `Chalk` — warm light theme; warning moves to `Magenta` (plain yellow is
/// unreadable on light surfaces) and dimmed chrome lifts to `Gray`.
fn chalk() -> CliTheme {
    CliTheme {
        name: "Chalk",
        user: Color::Blue,
        assistant: Color::Green,
        policy: Color::Yellow,
        success: Color::Green,
        warning: Color::Magenta,
        danger: Color::Red,
        muted: Color::Gray,
        border: Color::Blue,
    }
}

/// `Nebula` — deep-space neon theme; every role uses its light variant.
fn nebula() -> CliTheme {
    CliTheme {
        name: "Nebula",
        user: Color::LightCyan,
        assistant: Color::LightGreen,
        policy: Color::LightYellow,
        success: Color::LightGreen,
        warning: Color::LightYellow,
        danger: Color::LightRed,
        muted: Color::Gray,
        border: Color::LightCyan,
    }
}

/// Resolve the effective CLI theme.
///
/// Precedence: explicit `--theme` flag > `CONCERTO_THEME` env > config file
/// (`[display] theme`) > default (`Midnight`). Empty values at any level are
/// skipped; unknown names fall back to `Midnight` via [`CliTheme::by_name`].
pub fn resolve_cli_theme(explicit: Option<&str>, config: &AppConfig) -> CliTheme {
    if let Some(name) = explicit.map(str::trim).filter(|name| !name.is_empty()) {
        return CliTheme::by_name(name);
    }
    if let Ok(raw) = std::env::var(CLI_THEME_ENV_VAR) {
        if !raw.trim().is_empty() {
            return CliTheme::by_name(&raw);
        }
    }
    if let Some(name) =
        config.display.theme.as_deref().map(str::trim).filter(|name| !name.is_empty())
    {
        return CliTheme::by_name(name);
    }
    CliTheme::by_name(DEFAULT_CLI_THEME)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static THEME_ENV_LOCK: Mutex<()> = Mutex::new(());

    fn config_with_theme(theme: Option<&str>) -> AppConfig {
        let mut config = AppConfig::default();
        config.display.theme = theme.map(str::to_string);
        config
    }

    #[test]
    fn all_four_themes_resolve_with_complete_role_sets() {
        for name in CLI_THEME_NAMES {
            let theme = CliTheme::by_name(name);
            assert_eq!(theme.name, name, "canonical name must round-trip");
            // Every role carries a concrete ANSI color — never Reset.
            for color in [
                theme.user,
                theme.assistant,
                theme.policy,
                theme.success,
                theme.warning,
                theme.danger,
                theme.muted,
                theme.border,
            ] {
                assert_ne!(color, Color::Reset, "{name} must map every role");
            }
            // Every specialist bucket resolves (V2 accordion contract).
            for role in ["architect", "researcher", "coder", "reviewer", "validator"] {
                assert_ne!(theme.agent_color(role), Color::Reset, "{name} must color agent {role}");
            }
        }
        // Midnight preserves the historical hues (pre-theme CLI output).
        let midnight = CliTheme::by_name("Midnight");
        assert_eq!(midnight.user, Color::Cyan);
        assert_eq!(midnight.assistant, Color::Green);
        assert_eq!(midnight.policy, Color::Yellow);
        assert_eq!(midnight.danger, Color::Red);
        assert_eq!(midnight.muted, Color::DarkGray);
    }

    #[test]
    fn unknown_theme_falls_back_to_midnight() {
        assert_eq!(CliTheme::by_name("nope").name, "Midnight");
        assert_eq!(CliTheme::by_name("").name, "Midnight");
        assert_eq!(CliTheme::by_name(" midnight ").name, "Midnight");
        assert_eq!(CliTheme::by_name("NEBULA").name, "Nebula");
    }

    #[test]
    fn resolve_precedence_is_flag_over_env_over_config_over_default() {
        let _guard = THEME_ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        std::env::remove_var(CLI_THEME_ENV_VAR);
        // Default with no other source.
        assert_eq!(resolve_cli_theme(None, &config_with_theme(None)).name, "Midnight");
        // Config applies when flag and env are absent.
        assert_eq!(resolve_cli_theme(None, &config_with_theme(Some("Slate"))).name, "Slate");
        // Env beats config.
        std::env::set_var(CLI_THEME_ENV_VAR, "Chalk");
        assert_eq!(resolve_cli_theme(None, &config_with_theme(Some("Slate"))).name, "Chalk");
        // Flag beats env and config alike.
        assert_eq!(
            resolve_cli_theme(Some("Nebula"), &config_with_theme(Some("Slate"))).name,
            "Nebula"
        );
        std::env::remove_var(CLI_THEME_ENV_VAR);
    }
}
