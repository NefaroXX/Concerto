use iced::widget::{button, column, container, row, text};
use iced::{Element, Length, Subscription, Task};
use iced_term::actions::Action;
use iced_term::settings::{BackendSettings, FontSettings, Settings, ThemeSettings};
use iced_term::{ColorPalette, Terminal, TerminalView};
use std::collections::HashMap;
use std::path::PathBuf;

use concerto_config::shell::{ProfileAvailability, ShellProfileConfig};

use crate::theme::{AppTheme, Palette};

#[derive(Debug, Clone)]
pub enum Message {
    Event(iced_term::Event),
    Restart,
}

/// Project-scoped state for the integrated terminal.
///
/// The terminal is created lazily so launching Concerto does not also launch a
/// shell unless the user opens this page.
pub struct State {
    terminal: Option<Terminal>,
    project_dir: PathBuf,
    /// Resolved canonical shell profile, shared with agent execution. `None`
    /// falls back to the legacy
    /// `$SHELL`/`COMSPEC` behaviour with no injected env.
    profile: Option<ShellProfileConfig>,
    error: Option<String>,
    title: Option<String>,
    next_id: u64,
}

impl State {
    /// `profiles` is the full configured list; `active_id` selects the one
    /// used for agent execution (or `None` for legacy
    /// `$SHELL`/`COMSPEC`).
    pub fn new(
        project_dir: PathBuf,
        profiles: Vec<ShellProfileConfig>,
        active_id: Option<String>,
    ) -> Self {
        let profile =
            active_id.as_ref().and_then(|id| profiles.iter().find(|p| p.id == *id).cloned());
        Self { terminal: None, project_dir, profile, error: None, title: None, next_id: 0 }
    }

    /// Replace the available profiles and active selection (e.g. after the
    /// user saves Settings). If a terminal is already running it is restarted
    /// so the change takes effect; otherwise it applies on next start.
    pub fn set_profiles(
        &mut self,
        profiles: Vec<ShellProfileConfig>,
        active_id: Option<String>,
        theme: &AppTheme,
    ) -> Task<Message> {
        self.profile = active_id.and_then(|id| profiles.iter().find(|p| p.id == id).cloned());
        if self.terminal.is_some() || self.error.is_some() {
            self.restart(theme)
        } else {
            Task::none()
        }
    }

    /// Resolve the backend program/args/env/working-dir to launch, honouring
    /// the configured `terminal` profile (ADR-28) when present.
    fn resolved(&self) -> ResolvedTerminalShell {
        match &self.profile {
            None => ResolvedTerminalShell {
                program: selected_shell(),
                args: Vec::new(),
                env: HashMap::new(),
                cwd: self.project_dir.clone(),
            },
            Some(profile) => {
                let cwd = profile
                    .resolve_working_dir(&self.project_dir, dirs::home_dir().as_deref())
                    .unwrap_or_else(|| self.project_dir.clone());
                let base: HashMap<String, String> = std::env::vars().collect();
                ResolvedTerminalShell {
                    program: profile.resolve_executable(),
                    args: profile.interactive_launch_args(),
                    env: profile.effective_env(&base),
                    cwd,
                }
            }
        }
    }

    pub fn ensure_started(&mut self, theme: &AppTheme) -> Task<Message> {
        if self.terminal.is_some() {
            return self.focus();
        }
        self.start(theme)
    }

    pub fn update(&mut self, message: Message, theme: &AppTheme) -> Task<Message> {
        match message {
            Message::Restart => self.restart(theme),
            Message::Event(iced_term::Event::BackendCall(id, command)) => {
                let action = match self.terminal.as_mut() {
                    Some(terminal) if terminal.id == id => {
                        terminal.handle(iced_term::Command::ProxyToBackend(command))
                    }
                    _ => return Task::none(),
                };

                match action {
                    Action::Shutdown => {
                        self.terminal = None;
                        self.title = None;
                        self.error = Some(
                            "The terminal process exited. Restart it to open a new shell."
                                .to_string(),
                        );
                    }
                    Action::ChangeTitle(title) => self.title = Some(title),
                    Action::Ignore => {}
                }
                Task::none()
            }
        }
    }

    pub fn set_project_dir(&mut self, project_dir: PathBuf, theme: &AppTheme) -> Task<Message> {
        if self.project_dir == project_dir {
            return Task::none();
        }

        let was_started = self.terminal.is_some() || self.error.is_some();
        self.project_dir = project_dir;
        self.error = None;
        self.title = None;

        if was_started {
            self.restart(theme)
        } else {
            Task::none()
        }
    }

    pub fn set_theme(&mut self, theme: &AppTheme) {
        if let Some(terminal) = self.terminal.as_mut() {
            let _ = terminal.handle(iced_term::Command::ChangeTheme(Box::new(terminal_palette(
                &theme.palette,
            ))));
        }
    }

    pub fn subscription(&self) -> Subscription<Message> {
        self.terminal
            .as_ref()
            .map(|terminal| terminal.subscription().map(Message::Event))
            .unwrap_or_else(Subscription::none)
    }

    pub fn view<'a>(&'a self, theme: &'a AppTheme) -> Element<'a, Message> {
        let palette = &theme.palette;
        let shell_name: String = match &self.profile {
            Some(profile) => profile.name.clone(),
            None => {
                let shell = selected_shell();
                shell.file_name().and_then(|n| n.to_str()).unwrap_or("shell").to_string()
            }
        };
        let title = self.title.clone().unwrap_or_else(|| shell_name.clone());
        let path = self.project_dir.to_string_lossy();

        let header = container(
            row![
                column![
                    text(title).size(14),
                    text(format!("{path} · Selected in Settings for agent execution"))
                        .size(11)
                        .color(palette.text_muted),
                ]
                .spacing(2)
                .width(Length::Fill),
                button(text("Restart").size(12))
                    .style(button::secondary)
                    .on_press(Message::Restart),
            ]
            .spacing(12)
            .align_y(iced::Alignment::Center),
        )
        .padding([8, 12])
        .width(Length::Fill)
        .style(move |_theme: &iced::Theme| container::Style {
            background: Some(iced::Background::Color(palette.surface_variant)),
            ..container::Style::default()
        });

        let body: Element<'a, Message> = if let Some(terminal) = &self.terminal {
            TerminalView::show(terminal).map(Message::Event)
        } else {
            let message = self.error.as_deref().unwrap_or(
                "The terminal has not started. Open a shell for the active project folder.",
            );
            container(
                column![
                    text(message).size(14).color(if self.error.is_some() {
                        palette.danger
                    } else {
                        palette.text_muted
                    }),
                    button(text(if self.error.is_some() { "Retry" } else { "Start Terminal" }))
                        .style(crate::ui::button::primary)
                        .on_press(Message::Restart),
                ]
                .spacing(12),
            )
            .width(Length::Fill)
            .height(Length::Fill)
            .center_x(Length::Fill)
            .center_y(Length::Fill)
            .into()
        };

        column![header, body].height(Length::Fill).into()
    }

    fn restart(&mut self, theme: &AppTheme) -> Task<Message> {
        self.terminal = None;
        self.error = None;
        self.title = None;
        self.start(theme)
    }

    fn start(&mut self, theme: &AppTheme) -> Task<Message> {
        self.next_id = self.next_id.wrapping_add(1).max(1);

        // Recoverable diagnostic (ADR-28 Slice 1): if a profile is selected but
        // its executable is unavailable, surface a clear error instead of
        // letting the OS spawn fail with an opaque message. The Retry button
        // re-runs this check.
        if let Some(profile) = &self.profile {
            if let ProfileAvailability::Unavailable(reason) = profile.availability() {
                self.terminal = None;
                self.title = None;
                self.error =
                    Some(format!("Shell profile '{}' is unavailable: {reason}", profile.name));
                return Task::none();
            }
        }

        let resolved = self.resolved();
        let settings = Settings {
            backend: BackendSettings {
                program: resolved.program.to_string_lossy().into_owned(),
                args: resolved.args,
                env: resolved.env,
                working_directory: Some(resolved.cwd.clone()),
            },
            font: FontSettings {
                size: theme.font_stack.base_size,
                font_type: theme.font_stack.mono,
                ..FontSettings::default()
            },
            theme: ThemeSettings::new(Box::new(terminal_palette(&theme.palette))),
        };

        match Terminal::new(self.next_id, settings) {
            Ok(terminal) => {
                self.terminal = Some(terminal);
                self.error = None;
                self.focus()
            }
            Err(error) => {
                tracing::error!(%error, project_dir = %self.project_dir.display(), "terminal spawn failed");
                self.terminal = None;
                self.error = Some(format!(
                    "Could not start the terminal in {}: {error}",
                    self.project_dir.display()
                ));
                Task::none()
            }
        }
    }

    fn focus(&self) -> Task<Message> {
        self.terminal
            .as_ref()
            .map(|terminal| TerminalView::focus(terminal.widget_id().clone()))
            .unwrap_or_else(Task::none)
    }
}

/// Fully-resolved backend launch parameters for the terminal (ADR-28).
struct ResolvedTerminalShell {
    program: PathBuf,
    args: Vec<String>,
    env: HashMap<String, String>,
    cwd: PathBuf,
}

#[cfg(windows)]
fn selected_shell() -> PathBuf {
    std::env::var_os("COMSPEC")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("cmd.exe"))
}

#[cfg(not(windows))]
fn selected_shell() -> PathBuf {
    std::env::var_os("SHELL")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/bin/sh"))
}

fn terminal_palette(palette: &Palette) -> ColorPalette {
    ColorPalette {
        foreground: color_hex(palette.text),
        background: color_hex(palette.background),
        black: color_hex(palette.background),
        red: color_hex(palette.danger),
        green: color_hex(palette.success),
        yellow: color_hex(palette.warning),
        blue: color_hex(palette.primary),
        magenta: color_hex(palette.accent),
        cyan: color_hex(palette.secondary),
        white: color_hex(palette.text),
        bright_black: color_hex(palette.text_muted),
        bright_red: color_hex(palette.danger),
        bright_green: color_hex(palette.success),
        bright_yellow: color_hex(palette.warning),
        bright_blue: color_hex(palette.primary),
        bright_magenta: color_hex(palette.accent),
        bright_cyan: color_hex(palette.secondary),
        bright_white: color_hex(palette.primary_text),
        bright_foreground: Some(color_hex(palette.primary_text)),
        dim_foreground: color_hex(palette.text_muted),
        dim_black: color_hex(palette.surface),
        dim_red: color_hex(palette.danger),
        dim_green: color_hex(palette.success),
        dim_yellow: color_hex(palette.warning),
        dim_blue: color_hex(palette.primary),
        dim_magenta: color_hex(palette.accent),
        dim_cyan: color_hex(palette.secondary),
        dim_white: color_hex(palette.text_muted),
    }
}

fn color_hex(color: iced::Color) -> String {
    let channel = |value: f32| (value.clamp(0.0, 1.0) * 255.0).round() as u8;
    format!("#{:02x}{:02x}{:02x}", channel(color.r), channel(color.g), channel(color.b))
}

#[cfg(test)]
mod tests {
    use super::*;
    use concerto_config::shell::ShellBackendType;
    use std::path::Path;

    #[test]
    fn terminal_creation_is_lazy() {
        let state = State::new(PathBuf::from("project"), Vec::new(), None);
        assert!(state.terminal.is_none());
        assert!(state.error.is_none());
    }

    #[test]
    fn project_change_before_start_remains_lazy() {
        let mut state = State::new(PathBuf::from("first"), Vec::new(), None);
        let _ = state.set_project_dir(PathBuf::from("second"), &AppTheme::by_name("Midnight"));
        assert_eq!(state.project_dir, Path::new("second"));
        assert!(state.terminal.is_none());
    }

    #[test]
    fn configured_profile_change_applies_without_spawning() {
        let profile = ShellProfileConfig {
            id: "bash".into(),
            name: "Bash".into(),
            backend: ShellBackendType::System,
            executable: "bash".into(),
            ..Default::default()
        };
        let mut state =
            State::new(PathBuf::from("project"), vec![profile.clone()], Some("bash".into()));
        assert_eq!(state.profile.as_ref().map(|p| p.id.as_str()), Some("bash"));

        // A saved unknown id resolves to the legacy fallback (no profile).
        let _ = state.set_profiles(
            vec![profile.clone()],
            Some("nope".into()),
            &AppTheme::by_name("Midnight"),
        );
        assert!(state.profile.is_none());
        // Saving a known id restores the resolved profile.
        let _ =
            state.set_profiles(vec![profile], Some("bash".into()), &AppTheme::by_name("Midnight"));
        assert_eq!(state.profile.as_ref().map(|p| p.id.as_str()), Some("bash"));
    }

    #[test]
    fn theme_colors_are_emitted_as_valid_hex() {
        let palette = terminal_palette(&AppTheme::by_name("Chalk").palette);
        assert_eq!(palette.foreground.len(), 7);
        assert!(palette.foreground.starts_with('#'));
        assert!(palette.foreground[1..].chars().all(|c| c.is_ascii_hexdigit()));
    }
}
