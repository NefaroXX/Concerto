//! Project-first desktop shell.

use std::path::{Path, PathBuf};

use concerto_config::projects::ProjectRegistry;
use concerto_core::helpers::canonical_project_path;
use iced::widget::{button, column, container, row, scrollable, stack, text, Column};
use iced::{Element, Length, Subscription, Task};

use crate::app;
use crate::root_consent;
use crate::theme::AppTheme;

/// Messages handled by the project shell or forwarded to the project-scoped app.
#[derive(Debug, Clone)]
pub enum Message {
    App(app::Message),
    OpenProject(PathBuf),
    Browse,
    FolderPicked(Option<PathBuf>),
    ToggleReopenLast,
    CloseChooser,
    CloseRequested,
    /// ADR-60 D7 (interrupt-safe resume): the graceful-close wait finished
    /// (the run settled, or the bounded window lapsed) — the process may
    /// exit now.
    ExitAfterGracefulStop,
    /// ADR-44 §4: user allowed opening the pending out-of-root first project
    /// (for the process lifetime). Proceeds to create the app.
    RootConsentAllow,
    /// ADR-44 §4: user denied opening the pending out-of-root first project.
    /// Aborts the open cleanly.
    RootConsentDeny,
}

/// ADR-60 D7 (interrupt-safe resume): how long a window close with a run in
/// flight waits for the cancelled run to unwind and persist its interrupted
/// checkpoint before the process exits. Bounded so the exit is never a hang;
/// a miss is the documented hard-kill loss (the later `continue` resumes
/// headless from the evidence chain). The desktop executor's own shutdown
/// grace (750 ms) only reaps already-finished tasks after this wait.
const GRACEFUL_SHUTDOWN_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

/// Top-level desktop state. The full application is only created after a
/// project has been chosen, unless reopening the last project is explicitly
/// enabled.
pub struct DesktopApp {
    app: Option<app::App>,
    /// Persisted user theme driving the chooser UI while no app is open.
    /// Initialized exactly like the app-side ThemeChanged arm, so the chooser
    /// matches whatever the in-app shell renders.
    current_theme: AppTheme,
    registry: ProjectRegistry,
    chooser_open: bool,
    error: Option<String>,
    /// ADR-44 §4: canonical path awaiting the out-of-root consent gate before
    /// the first project is opened (no App exists yet to own the gate).
    pending_root_consent: Option<PathBuf>,
    /// ADR-44 §4: effective allowlist — canonicalized configured roots seeded
    /// at startup plus every canonical path allowed for this process. Empty =
    /// roots unset = no gating.
    effective_roots: Vec<PathBuf>,
}

impl DesktopApp {
    pub fn new() -> (Self, Task<Message>) {
        // Mirror the app-side theme bootstrap (app.rs ThemeChanged): load the
        // persisted user theme, falling back to Midnight on any error.
        let data_dir = dirs::data_dir()
            .map(|d| d.join("concerto"))
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        let prefs_dir = data_dir.join("prefs");
        let _ = std::fs::create_dir_all(&prefs_dir);
        let current_theme = match concerto_memory::prefs::UserPrefsStore::open(&prefs_dir) {
            Ok(store) => crate::theme::prefs::load_theme(&store),
            Err(_) => AppTheme::by_name("Midnight"),
        };

        let (registry, error) = match ProjectRegistry::load() {
            Ok(registry) => (registry, None),
            Err(error) => (
                ProjectRegistry::default(),
                Some(format!("Could not load the project registry: {error}")),
            ),
        };

        // ADR-44 §4: seed the effective allowlist from the env-inclusive
        // config (config files + CONCERTO_PROJECT_ROOTS).
        let effective_roots = concerto_config::load_config(None, None)
            .ok()
            .map(|config| root_consent::canonical_roots(&config.project_roots))
            .unwrap_or_default();

        if startup_project(&registry).is_some() {
            let (app, task) = app::App::new();
            return (
                Self {
                    app: Some(app),
                    current_theme,
                    registry,
                    chooser_open: false,
                    error,
                    pending_root_consent: None,
                    effective_roots,
                },
                task.map(Message::App),
            );
        }

        (
            Self {
                app: None,
                current_theme,
                registry,
                chooser_open: true,
                error,
                pending_root_consent: None,
                effective_roots,
            },
            Task::none(),
        )
    }

    pub fn title(&self) -> String {
        if self.chooser_open {
            "Concerto — Projects".to_string()
        } else {
            self.app.as_ref().map_or_else(|| "Concerto".to_string(), app::App::title)
        }
    }

    pub fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::App(app::Message::OpenProjectDirPicker) => {
                self.refresh_registry();
                self.chooser_open = true;
                self.error = None;
                Task::none()
            }
            Message::App(message) => self
                .app
                .as_mut()
                .map(|app| app.update(message).map(Message::App))
                .unwrap_or_else(Task::none),
            Message::OpenProject(path) => self.open_project(path),
            Message::Browse => {
                let start_directory = self.chooser_start_directory();
                Task::perform(
                    async move {
                        let mut dialog =
                            rfd::AsyncFileDialog::new().set_title("Open Concerto project");
                        if let Some(directory) = start_directory {
                            dialog = dialog.set_directory(directory);
                        }
                        dialog.pick_folder().await.map(|folder| folder.path().to_path_buf())
                    },
                    Message::FolderPicked,
                )
            }
            Message::FolderPicked(Some(path)) => self.open_project(path),
            Message::FolderPicked(None) => Task::none(),
            Message::ToggleReopenLast => {
                let reopen = !self.registry.reopen_last_project();
                self.registry.set_reopen_last_project(reopen);
                match self.registry.save() {
                    Ok(()) => self.error = None,
                    Err(error) => {
                        self.registry.set_reopen_last_project(!reopen);
                        self.error = Some(format!("Could not save the startup setting: {error}"));
                    }
                }
                Task::none()
            }
            Message::CloseChooser => {
                if self.app.is_some() {
                    self.chooser_open = false;
                    self.error = None;
                }
                Task::none()
            }
            Message::CloseRequested => {
                // ADR-60 D7 (interrupt-safe resume): a close with a run in
                // flight cancels it and waits, bounded, for the run to
                // settle — the coordinator's cancel path persists the
                // interrupted (completed=0) checkpoint BEFORE
                // `run_shared_agent` returns, so a settled epoch bump implies
                // durable resumable state. Exiting over the run (the previous
                // behavior) was the desktop's hard-kill: the executor drops
                // its runtime 750 ms after exit and nothing lands.
                let running = self.app.as_ref().is_some_and(app::App::is_run_active);
                let settle_epoch =
                    self.app.as_ref().map(|app| (app.cancel_token.clone(), app.run_settle_epoch()));
                if let Some(app) = &self.app {
                    app.cancel_token.cancel();
                }
                if !running {
                    iced::exit()
                } else {
                    let (cancel_token, settle) =
                        settle_epoch.expect("a running app carries the settle signal");
                    // Defensive: the close request also cancelled the token
                    // above; cancelling again is a no-op.
                    cancel_token.cancel();
                    Task::perform(
                        async move {
                            let start = settle.load(std::sync::atomic::Ordering::Acquire);
                            let deadline = tokio::time::Instant::now() + GRACEFUL_SHUTDOWN_WAIT;
                            loop {
                                if settle.load(std::sync::atomic::Ordering::Acquire) != start {
                                    return;
                                }
                                if tokio::time::Instant::now() >= deadline {
                                    tracing::warn!(
                                        "window closed while a run was active; the \
                                         graceful-stop window lapsed before the run settled"
                                    );
                                    return;
                                }
                                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                            }
                        },
                        |()| Message::ExitAfterGracefulStop,
                    )
                }
            }
            Message::ExitAfterGracefulStop => iced::exit(),
            Message::RootConsentAllow => {
                let Some(canonical) = self.pending_root_consent.take() else {
                    return Task::none();
                };
                // The user allowed the canonical dir for this process: record
                // it in the effective allowlist, then proceed with the open.
                if !self.effective_roots.contains(&canonical) {
                    self.effective_roots.push(canonical.clone());
                }
                self.open_project(canonical)
            }
            Message::RootConsentDeny => {
                // Abort the open cleanly: no app is created and no error is
                // shown; the chooser stays open.
                self.pending_root_consent = None;
                Task::none()
            }
        }
    }

    pub fn view(&self) -> Element<'_, Message> {
        let base = if self.chooser_open {
            self.project_chooser()
        } else {
            self.app
                .as_ref()
                .map(|app| app.view().map(Message::App))
                .unwrap_or_else(|| self.project_chooser())
        };

        // ADR-44 §4: overlay the consent gate over the chooser for the first
        // out-of-root open. Composed like the app-side system dialogs: a
        // centered modal card over a semi-transparent palette backdrop.
        let Some(pending) = &self.pending_root_consent else {
            return base;
        };
        let modal = container(root_consent::consent_card(
            pending,
            &self.theme(),
            Message::RootConsentAllow,
            Message::RootConsentDeny,
        ))
        .width(Length::FillPortion(2))
        .height(Length::FillPortion(2))
        .style(crate::ui::container::modal);
        let backdrop = container(modal)
            .width(Length::Fill)
            .height(Length::Fill)
            .center_x(Length::Fill)
            .center_y(Length::Fill)
            .style(|theme: &iced::Theme| container::Style {
                background: Some(iced::Background::Color(iced::Color {
                    a: 0.55,
                    ..theme.palette().background
                })),
                ..container::Style::default()
            });
        stack![base, backdrop].into()
    }

    pub fn subscription(&self) -> Subscription<Message> {
        let app_subscription = self
            .app
            .as_ref()
            .map(|app| app.subscription().map(Message::App))
            .unwrap_or_else(Subscription::none);
        let close_requests = iced::window::close_requests().map(|_| Message::CloseRequested);

        Subscription::batch([app_subscription, close_requests])
    }

    pub fn theme(&self) -> iced::Theme {
        match self.app.as_ref() {
            Some(app) => app::App::theme(app),
            None => self.current_theme.iced.clone(),
        }
    }

    fn open_project(&mut self, path: PathBuf) -> Task<Message> {
        if !path.is_dir() {
            self.error = Some(format!("Project directory does not exist: {}", path.display()));
            return Task::none();
        }

        if let Some(app) = self.app.as_mut() {
            let desired = canonical_project_path(&path);
            let before = app.project_dir.clone();
            drop(
                app.update(app::Message::ProjectDirInputChanged(
                    path.to_string_lossy().into_owned(),
                )),
            );
            let task = app.update(app::Message::ProjectDirApply).map(Message::App);
            // ADR-44 §4: an out-of-root switch is deferred to the app's
            // consent-gate modal (the apply returned early without switching).
            // Recognise that pending state instead of treating it as a failed
            // switch — the old "Finish or cancel the active session" branch
            // would be a false error here. Close the chooser so the app's
            // gate modal is visible above the running app.
            if app.pending_root_consent.is_some() {
                self.chooser_open = false;
                self.error = None;
                return task;
            }
            let after = app.project_dir.clone();

            if after == desired || before == desired {
                self.chooser_open = false;
                self.error = None;
                self.refresh_registry();
            } else {
                self.error = Some(
                    "Finish or cancel the active session before switching projects.".to_string(),
                );
            }
            return task;
        }

        // First open: no App exists yet to own the apply-path gate, so run the
        // ADR-44 §4 check here before creating one. The pending open waits for
        // the consent modal; Allow re-enters this function.
        let canonical = canonical_project_path(&path);
        if root_consent::needs_consent(&canonical, &self.effective_roots) {
            self.pending_root_consent = Some(canonical);
            return Task::none();
        }

        match self.registry.select(&path).and_then(|_| self.registry.save()) {
            Ok(()) => {
                let (app, task) = app::App::new();
                self.app = Some(app);
                self.chooser_open = false;
                self.error = None;
                task.map(Message::App)
            }
            Err(error) => {
                self.error = Some(format!("Could not open the project: {error}"));
                Task::none()
            }
        }
    }

    fn refresh_registry(&mut self) {
        match ProjectRegistry::load() {
            Ok(registry) => self.registry = registry,
            Err(error) => {
                self.error = Some(format!("Could not refresh the project registry: {error}"));
            }
        }
    }

    fn chooser_start_directory(&self) -> Option<PathBuf> {
        self.app
            .as_ref()
            .map(|app| app.project_dir.clone())
            .or_else(|| self.registry.recent().next().map(Path::to_path_buf))
            .or_else(dirs::home_dir)
    }

    fn project_chooser(&self) -> Element<'_, Message> {
        let palette = &self.current_theme.palette;

        // One uniform list-item card per recent project: purple accent bar +
        // elevated surface for the active project (mirrors the sidebar rows).
        let recent_projects =
            self.registry.recent().fold(Column::new().spacing(8), |projects, path| {
                let active = self.app.as_ref().is_some_and(|app| {
                    canonical_project_path(&app.project_dir) == canonical_project_path(path)
                });
                projects.push(crate::ui::list_item(
                    &self.current_theme,
                    active,
                    Message::OpenProject(path.to_path_buf()),
                    column![
                        text(project_name(path)).size(14).color(palette.text),
                        text(path.display().to_string()).size(11).color(palette.text_muted),
                    ]
                    .spacing(2),
                ))
            });

        let recent_section: Element<'_, Message> = if self.registry.recent().next().is_some() {
            scrollable(recent_projects).height(Length::Fill).into()
        } else {
            container(text("No recent projects yet.").size(13).color(palette.text_muted))
                .height(Length::Fill)
                .center_y(Length::Fill)
                .into()
        };

        // Section header sits 8px above the list; the header→list gap is
        // intentionally tighter than the 16px between the other blocks.
        let recent_block = column![
            text("RECENT PROJECTS")
                .size(11)
                .shaping(iced::widget::text::Shaping::Advanced)
                .style(move |_| crate::theme::sidebar_header_style(palette)),
            recent_section,
        ]
        .spacing(8)
        .height(Length::Fill);

        let reopen_label = if self.registry.reopen_last_project() {
            "✓ Reopen last project on startup"
        } else {
            "○ Reopen last project on startup"
        };

        // Uniform secondary action row — every button shares the same style,
        // padding and text size (no default solid-blue primary buttons).
        let mut actions = row![
            button(text("Browse for project…").size(13))
                .style(crate::ui::button::secondary)
                .padding([8, 16])
                .on_press(Message::Browse),
            button(text(reopen_label).size(13))
                .style(crate::ui::button::secondary)
                .padding([8, 16])
                .on_press(Message::ToggleReopenLast),
        ]
        .spacing(12);

        if self.app.is_some() {
            actions = actions.push(
                button(text("Cancel").size(13))
                    .style(crate::ui::button::secondary)
                    .padding([8, 16])
                    .on_press(Message::CloseChooser),
            );
        }

        let mut content = column![
            text("Open a project").size(28),
            text("Choose a recent project or browse to an existing project directory.")
                .size(14)
                .color(palette.text_muted),
            recent_block,
        ]
        .spacing(16)
        .height(Length::Fill);

        if let Some(error) = &self.error {
            content = content.push(text(error).size(13));
        }
        content = content.push(actions);

        container(
            // Constrain the column so the chooser stays readable on wide windows.
            container(content)
                .width(Length::Fill)
                .max_width(560.0)
                .height(Length::Fill)
                .padding(32),
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .center_x(Length::Fill)
        .center_y(Length::Fill)
        .into()
    }
}

impl Drop for DesktopApp {
    fn drop(&mut self) {
        if let Some(app) = &self.app {
            app.cancel_token.cancel();
        }
    }
}

fn startup_project(registry: &ProjectRegistry) -> Option<PathBuf> {
    if registry.reopen_last_project() {
        registry.active().map(Path::to_path_buf)
    } else {
        None
    }
}

fn project_name(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(str::to_owned)
        .unwrap_or_else(|| path.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_project_requires_explicit_opt_in() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();

        let mut registry = ProjectRegistry::default();
        registry.select(&project).unwrap();
        assert!(startup_project(&registry).is_none());

        registry.set_reopen_last_project(true);
        assert_eq!(startup_project(&registry), Some(canonical_project_path(&project)));
    }

    #[test]
    fn project_name_uses_directory_name() {
        assert_eq!(project_name(Path::new("/tmp/concerto")), "concerto");
    }

    #[test]
    fn project_launcher_has_default_title() {
        let (app, _) = super::DesktopApp::new();
        let title = app.title();
        assert!(!title.is_empty(), "project launcher title should not be empty");
    }

    /// The chooser renders via `view()` (app: None → chooser) without
    /// panicking. With a recent project recorded, the list-item branch
    /// (project name + path rows) is exercised end to end.
    #[test]
    fn project_chooser_view_renders_recent_projects() {
        let target = tempfile::tempdir().unwrap();
        let mut registry = ProjectRegistry::default();
        registry.select(target.path()).unwrap();
        assert!(registry.recent().next().is_some(), "selected project must be recent");

        let launcher = DesktopApp {
            app: None,
            current_theme: AppTheme::by_name("Midnight"),
            registry,
            chooser_open: true,
            error: None,
            pending_root_consent: None,
            effective_roots: Vec::new(),
        };

        let _element: iced::Element<'_, Message> = launcher.view();
    }

    // -----------------------------------------------------------------------
    // ADR-44 §4 — out-of-root consent gate (first open / app switch)
    // -----------------------------------------------------------------------

    /// A DesktopApp with no active app, ready for controlled gate tests.
    fn launcher_with(roots: Vec<PathBuf>) -> DesktopApp {
        DesktopApp {
            app: None,
            current_theme: AppTheme::by_name("Midnight"),
            registry: ProjectRegistry::default(),
            chooser_open: true,
            error: None,
            pending_root_consent: None,
            effective_roots: roots,
        }
    }

    /// Opening an out-of-root first project defers to the consent gate; Deny
    /// aborts the open cleanly (no app created, no error).
    #[test]
    fn first_open_out_of_root_waits_for_consent_and_deny_aborts() {
        let target = tempfile::tempdir().unwrap();
        let mut launcher = launcher_with(vec![PathBuf::from("/srv/configured-root")]);

        let _ = launcher.update(Message::OpenProject(target.path().to_path_buf()));
        assert!(launcher.pending_root_consent.is_some(), "gate must be pending");
        assert!(launcher.app.is_none(), "no app may be created while gated");
        assert_eq!(launcher.error, None);

        let _ = launcher.update(Message::RootConsentDeny);
        assert!(launcher.pending_root_consent.is_none(), "deny must clear the gate");
        assert!(launcher.app.is_none(), "deny must not open a project");
        assert_eq!(launcher.error, None, "deny must not produce error spam");
    }

    /// Allow records the canonical dir in the effective allowlist and opens
    /// the project.
    #[test]
    fn first_open_allow_opens_project_and_records_allowlist() {
        let _guard = crate::root_consent::REGISTRY_SAVE_LOCK.lock().unwrap();
        let target = tempfile::tempdir().unwrap();
        let canonical =
            target.path().canonicalize().unwrap_or_else(|_| target.path().to_path_buf());
        let mut launcher = launcher_with(vec![PathBuf::from("/srv/configured-root")]);

        let _ = launcher.update(Message::OpenProject(target.path().to_path_buf()));
        assert!(launcher.pending_root_consent.is_some());

        let _ = launcher.update(Message::RootConsentAllow);
        assert!(launcher.pending_root_consent.is_none());
        assert!(launcher.app.is_some(), "allow must proceed with the open");
        assert!(launcher.effective_roots.contains(&canonical));
    }

    /// An in-root first open never gates.
    #[test]
    fn first_open_inside_root_does_not_gate() {
        let _guard = crate::root_consent::REGISTRY_SAVE_LOCK.lock().unwrap();
        let target = tempfile::tempdir().unwrap();
        let canonical =
            target.path().canonicalize().unwrap_or_else(|_| target.path().to_path_buf());
        let mut launcher = launcher_with(vec![canonical.clone()]);

        let _ = launcher.update(Message::OpenProject(target.path().to_path_buf()));
        assert!(launcher.pending_root_consent.is_none(), "in-root open must not gate");
        assert!(launcher.app.is_some());
    }

    /// When an app is already open and a switch is deferred to the app's
    /// consent gate, the launcher must NOT show the false "Finish or cancel the
    /// active session" error — it closes the chooser so the app's gate modal is
    /// visible.
    #[test]
    fn app_switch_out_of_root_defers_to_app_gate_without_false_error() {
        let _guard = crate::root_consent::REGISTRY_SAVE_LOCK.lock().unwrap();
        let (mut started, _) = app::App::new();
        started.effective_roots = vec![PathBuf::from("/srv/configured-root")];
        let target = tempfile::tempdir().unwrap();
        let mut launcher = DesktopApp {
            app: Some(started),
            current_theme: AppTheme::by_name("Midnight"),
            registry: ProjectRegistry::default(),
            chooser_open: true,
            error: None,
            pending_root_consent: None,
            effective_roots: Vec::new(),
        };

        let _ = launcher.update(Message::OpenProject(target.path().to_path_buf()));
        let gate_app = launcher.app.as_ref().expect("app exists");
        assert!(gate_app.pending_root_consent.is_some(), "switch must be gated");
        assert!(!launcher.chooser_open, "chooser closes so the app gate modal shows");
        assert_eq!(launcher.error, None, "no false 'finish or cancel' error");
    }
}
